//! VOICE CLONING (build 2/2) — the CONSENT-GATED, AUTHORIZATION-BOUND capability
//! that registers an owner-authorized audio sample with ElevenLabs and stores the
//! returned voice id so it is usable like any other ElevenLabs voice.
//!
//! ## The honesty that governs every word of copy
//! This is the ONE path in the voice stack where AUDIO (not just text) leaves the
//! device: cloning uploads a voice SAMPLE to ElevenLabs (POST /v1/voices/add). It is
//! therefore the most sensitive voice operation, and it is gated harder than the
//! rest:
//!   * CONSENT-GATED — NEVER automatic. An explicit "clone my voice" intent (or a
//!     Settings trigger) PROPOSES a clone and PARKS; nothing is uploaded until the
//!     user explicitly CONFIRMS on a later turn ([`CloneState`] is a two-step
//!     pending->confirmed machine, mirroring the cross-turn confirmation gate).
//!   * AUTHORIZATION-BOUND — you may only clone a voice you are authorized to use
//!     (your own). The sample must be an owner-authorized file CONFINED under the
//!     JARVIS root ([`confine_sample`]); a path that escapes the root is rejected, so
//!     this can never be pointed at an arbitrary recording of someone else.
//!   * REVERSIBLE + OFF-respecting — the cloned id is stored LOCALLY
//!     (`state/voice/cloned.json`); deleting it ("forget the clone") drops the slot.
//!     With the cloud voice tier OFF the stored id is simply unused (Kokoro speaks),
//!     exactly like an unmapped agent.
//!
//! ## What leaves the device — say it plainly
//! On a CONFIRMED clone the audio sample is uploaded to ElevenLabs. The resolved
//! ElevenLabs key rides ONLY the request body for the server's `xi-api-key` header
//! (never logged/argv/telemetry). If the clone fails (no key / network / quota) the
//! user keeps Kokoro / their existing voice — nothing is silently changed. The
//! returned `voice_id` is NON-SECRET and is what gets stored.
//!
//! ## Hermetic
//! Everything here — intent detection, the consent state machine, path confinement,
//! and the cloned-id store — is PURE / filesystem-local and unit-tested with no
//! network and no real audio. The actual upload is the inference clone seam
//! ([`crate::inference::InferenceClient::clone_voice`]), credential+runtime-gated and
//! exercised only via a stub; this module never makes a network call.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// An explicit voice-clone management intent parsed from a spoken utterance. Only
/// these EXPLICIT phrasings ever PROPOSE a clone or drop a stored one — a clone is
/// NEVER started from an ordinary utterance, and even a proposed clone uploads
/// NOTHING until separately confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneIntent {
    /// "clone my voice" / "register my voice with ElevenLabs" / "make a voice clone".
    Clone,
    /// "forget my voice clone" / "delete the cloned voice" / "remove my clone".
    Forget,
}

/// Detect an explicit clone/forget-clone intent. CONSERVATIVE and phrase-anchored:
/// the utterance must mention CLONING / a voice CLONE (or registering a voice with
/// the cloud) together with the speaker's voice, so an ordinary sentence never trips
/// it. Distinct from [`crate::voiceid::classify_intent`] (on-device enrollment): a
/// clone uploads a SAMPLE to the cloud, so it carries the word "clone"/"register"
/// rather than "enroll"/"learn". Pure — unit-tested without audio.
pub fn classify_intent(utterance: &str) -> Option<CloneIntent> {
    let lower = utterance.to_lowercase();

    // Must be about the speaker's own voice and a CLONE/REGISTER-with-the-cloud act.
    let mentions_voice = lower.contains("my voice") || lower.contains("voice clone");
    let clone_word = lower.contains("clone")
        || lower.contains("register my voice")
        || lower.contains("voice clone");
    if !(mentions_voice && clone_word) {
        return None;
    }

    // FORGET takes priority (an unambiguous "drop the clone").
    const FORGET: &[&str] = &["forget", "delete", "remove", "clear", "unclone", "erase"];
    if FORGET.iter().any(|v| lower.contains(v)) {
        return Some(CloneIntent::Forget);
    }
    // Otherwise an explicit clone proposal (still consent-gated downstream).
    Some(CloneIntent::Clone)
}

/// The cross-turn CONSENT state for a proposed clone. A "clone my voice" intent
/// installs [`CloneState::Pending`] and JARVIS asks the user to confirm; uploading
/// only happens on the NEXT turn when the user explicitly says yes. This mirrors the
/// confirmation gate so a clone (audio leaving the device) can never fire from a
/// single utterance, automatically, or from a misheard fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloneState {
    /// No clone proposed.
    Idle,
    /// A clone was proposed and is AWAITING an explicit spoken confirmation. Carries
    /// the resolved owner sample path (already confined) and the agent slot the
    /// cloned id will fill. Nothing has left the device.
    Pending { sample: PathBuf, agent: String },
}

impl Default for CloneState {
    fn default() -> Self {
        CloneState::Idle
    }
}

/// Whether an utterance is an explicit affirmative confirming a pending clone.
/// CONSERVATIVE and NEGATION-FAIL-SAFE: only clear yes-phrasings confirm (so an
/// ambiguous reply never uploads audio), and ANY clear negation rejects FIRST — so a
/// REFUSAL like "no, don't do it" / "actually, don't clone it" cancels rather than
/// uploading, even though it contains the YES substring "do it" / "clone it". A
/// non-affirmative cancels the pending proposal (fail-safe — the audio stays
/// on-device unless the user clearly says yes).
pub fn is_confirmation(utterance: &str) -> bool {
    let lower = utterance.trim().to_lowercase();

    // (1) NEGATION GUARD FIRST. If the utterance carries any clear negation token we
    //     treat it as a refusal and never confirm — this is what stops "don't do it"
    //     / "no, don't clone it" from matching a multi-word YES substring. The check
    //     is on whitespace/punctuation-delimited tokens (so "nope" is caught, but a
    //     word that merely contains "no" is not), plus the "n't" contraction anywhere.
    const NEGATIONS: &[&str] = &[
        "no", "nope", "nah", "not", "don't", "do", "cancel", "stop", "never",
        "abort", "decline", "negative",
    ];
    if lower.contains("n't") {
        return false;
    }
    let token_negation = lower
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .any(|tok| {
            // "do" only negates as part of "do not" — guard it specially so a bare
            // "do it" still confirms.
            if tok == "do" {
                return false;
            }
            NEGATIONS.iter().any(|n| *n != "do" && tok == *n)
        });
    // "do not" (without the contraction) is an explicit refusal.
    let do_not = lower.contains("do not");
    if token_negation || do_not {
        return false;
    }

    // (2) YES scan — EXACT match or a LEADING "{yes}" phrase (followed by a space or
    //     punctuation). No raw .contains(): a YES token only confirms when it IS the
    //     utterance or opens it, so a stray substring (e.g. inside a negated clause)
    //     can never confirm. The negation guard above already rejected anything with a
    //     refusal token, so a leading-YES match here is genuinely affirmative.
    const YES: &[&str] = &[
        "yes", "yeah", "yep", "confirm", "confirmed", "do it", "go ahead",
        "clone it", "proceed", "affirmative", "please do",
    ];
    YES.iter().any(|y| {
        lower == *y
            || lower.starts_with(&format!("{y} "))
            || lower.starts_with(&format!("{y},"))
            || lower.starts_with(&format!("{y}."))
            || lower.starts_with(&format!("{y}!"))
    })
}

/// Confine a chosen sample path to the JARVIS root (AUTHORIZATION-BOUND): the sample
/// must resolve to a real file UNDER `root` (e.g. `state/voiceid/…`,
/// `state/voice-samples/…`, or an owner file the operator placed in the tree). A
/// path that escapes the root (absolute elsewhere, `..` traversal) is REJECTED, so a
/// clone can never be pointed at an arbitrary recording of someone else. Returns the
/// canonical, confined path or `None`.
///
/// `candidate` may be absolute or relative to `root`. The check is done on the
/// canonicalized paths so symlink/`..` escapes are caught. Pure filesystem
/// validation — no network.
pub fn confine_sample(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let canon = joined.canonicalize().ok()?;
    let root_canon = root.canonicalize().ok()?;
    if !canon.starts_with(&root_canon) {
        return None;
    }
    if !canon.is_file() {
        return None;
    }
    Some(canon)
}

/// Pick a DEFAULT owner sample to clone from, preferring the on-device voice-id
/// enrollment audio location, then the TTS audition samples — both already inside
/// the root. Returns the first existing, confined file, or `None` (the caller then
/// asks the user to point at an authorized sample). Conservative: only known,
/// in-tree owner locations are searched — never the wider filesystem.
pub fn default_owner_sample(root: &Path) -> Option<PathBuf> {
    // 1. A voice-id enrollment recording, if the operator saved one alongside the
    //    profile (the owner's authorized audio).
    let voiceid_dir = root.join("state").join("voiceid");
    if let Some(s) = first_wav_in(&voiceid_dir) {
        if let Some(c) = confine_sample(root, &s) {
            return Some(c);
        }
    }
    // 2. A TTS audition sample under state/voice-samples/.
    let samples_dir = root.join("state").join("voice-samples");
    if let Some(s) = first_wav_in(&samples_dir) {
        if let Some(c) = confine_sample(root, &s) {
            return Some(c);
        }
    }
    None
}

/// The first `*.wav` (sorted) directly inside `dir`, or `None` if the dir is
/// missing/empty. Deterministic ordering for stable tests.
fn first_wav_in(dir: &Path) -> Option<PathBuf> {
    let mut wavs: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("wav")))
        .collect();
    wavs.sort();
    wavs.into_iter().next()
}

/// The LOCALLY-stored cloned voice ids (agent name -> ElevenLabs voice id), the
/// output of a CONFIRMED clone. NON-SECRET (a voice id, never a key). Persisted at
/// `state/voice/cloned.json`. At runtime these are merged into the effective
/// `[voice.voices]` map ([`merge_into`]) so a cloned voice is usable exactly like a
/// config-mapped one — but ONLY when the cloud voice tier is on (with it off the id
/// is simply unused and Kokoro speaks).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClonedVoices {
    /// agent name -> cloned ElevenLabs voice id.
    pub voices: BTreeMap<String, String>,
}

impl ClonedVoices {
    /// Record a confirmed clone for `agent`. Overwrites any prior cloned id for that
    /// agent (a re-clone replaces the old voice).
    pub fn set(&mut self, agent: &str, voice_id: &str) {
        self.voices.insert(agent.to_string(), voice_id.to_string());
    }

    /// Drop the cloned id for `agent` ("forget the clone"). Returns whether one was
    /// present. After this the agent falls back to its config/Kokoro voice.
    pub fn forget(&mut self, agent: &str) -> bool {
        self.voices.remove(agent).is_some()
    }

    /// Merge the stored cloned ids into a `[voice.voices]`-shaped map WITHOUT
    /// clobbering an explicit config mapping: a config-mapped agent wins (the
    /// operator's explicit choice), a cloned id fills only an UNMAPPED agent. This is
    /// how a confirmed clone becomes usable like any EL voice. Pure.
    pub fn merge_into(&self, voices: &mut BTreeMap<String, String>) {
        for (agent, id) in &self.voices {
            voices.entry(agent.clone()).or_insert_with(|| id.clone());
        }
    }
}

/// The on-disk path of the cloned-voice store: `<root>/state/voice/cloned.json`.
/// NON-SECRET ids only — never audio, never a key.
pub fn store_path(root: &Path) -> PathBuf {
    root.join("state").join("voice").join("cloned.json")
}

/// The LOCALLY-stored ACTIVE pronunciation-dictionary locator (Phase-2): the NON-secret
/// (dictionary_id, version_id) pair a CONFIRMED `create_pronunciation` op returned.
/// Mirrors [`ClonedVoices`] exactly — persisted at `state/voice/pronunciation.json`,
/// loaded at startup and folded into `[voice].pronunciation_dictionary_id`/`_version`
/// so a minted dictionary becomes a speak locator. With the keys empty (no dictionary
/// minted) speech is byte-for-byte today's. NON-secret (ids only, NEVER a key).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivePronunciation {
    /// The active pronunciation-dictionary id (empty = none active).
    pub dictionary_id: String,
    /// The version paired with `dictionary_id` (empty = latest).
    pub version_id: String,
}

impl ActivePronunciation {
    /// Record the active locator from a confirmed `create_pronunciation` (replaces any
    /// prior one — a re-create supersedes the old dictionary).
    #[allow(dead_code)] // written by trigger_create_pronunciation + exercised in tests
    pub fn set(&mut self, dictionary_id: &str, version_id: &str) {
        self.dictionary_id = dictionary_id.to_string();
        self.version_id = version_id.to_string();
    }

    /// Fold the active locator into the runtime config keys WITHOUT clobbering an
    /// explicit config value (the operator's explicit choice wins; a minted locator
    /// fills only an EMPTY config). Mirrors [`ClonedVoices::merge_into`]. Pure.
    pub fn merge_into(&self, dict_id: &mut String, version: &mut String) {
        if self.dictionary_id.is_empty() {
            return;
        }
        if dict_id.trim().is_empty() {
            *dict_id = self.dictionary_id.clone();
            *version = self.version_id.clone();
        }
    }
}

/// The on-disk path of the active-pronunciation store:
/// `<root>/state/voice/pronunciation.json`. NON-SECRET ids only — never a key.
pub fn pronunciation_store_path(root: &Path) -> PathBuf {
    root.join("state").join("voice").join("pronunciation.json")
}

/// Load the active-pronunciation store, or an EMPTY one when none exists / it is
/// malformed (treated as "no active dictionary", never an error that wedges the daemon).
pub fn load_pronunciation(root: &Path) -> ActivePronunciation {
    let path = pronunciation_store_path(root);
    std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<ActivePronunciation>(&b).ok())
        .unwrap_or_default()
}

/// Persist the active-pronunciation store (non-secret ids) with restrictive perms,
/// mirroring [`save_clones`]. Best-effort 0700 dir / 0600 file.
#[allow(dead_code)] // written by trigger_create_pronunciation + exercised in tests
pub fn save_pronunciation(root: &Path, active: &ActivePronunciation) -> std::io::Result<()> {
    let path = pronunciation_store_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let bytes = serde_json::to_vec_pretty(active)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, bytes)?;
    set_mode(&path, 0o600);
    Ok(())
}

/// Load the cloned-voice store, or an EMPTY store when none exists / it is malformed
/// (treated as "no clones", never an error that wedges the daemon).
pub fn load_clones(root: &Path) -> ClonedVoices {
    let path = store_path(root);
    std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice::<ClonedVoices>(&b).ok())
        .unwrap_or_default()
}

/// Persist the cloned-voice store (non-secret ids) with restrictive perms, mirroring
/// `voiceid::save_profile`. Best-effort 0700 dir / 0600 file.
pub fn save_clones(root: &Path, clones: &ClonedVoices) -> std::io::Result<()> {
    let path = store_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let bytes = serde_json::to_vec_pretty(clones)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&path, bytes)?;
    set_mode(&path, 0o600);
    Ok(())
}

/// chmod best-effort (Unix); mirrors `voiceid::set_mode` / `command::set_mode`.
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) {}

/// The spoken proposal asking the user to confirm a clone (the CONSENT prompt). It
/// is HONEST: it names that the audio sample will leave the device for ElevenLabs and
/// that a clone requires authorization (no impersonating others).
pub fn consent_prompt(sample_display: &str) -> String {
    format!(
        "To clone your voice I'll upload an audio sample ({sample_display}) to ElevenLabs — \
         that sample LEAVES this device. Only clone a voice you're authorized to use, which is \
         your own. Say \"yes\" to go ahead, or anything else to cancel."
    )
}

/// The display name for the cloned voice on ElevenLabs. Stable + non-secret.
pub fn clone_display_name(agent: &str) -> String {
    format!("JARVIS cloned voice ({agent})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_and_forget_intents_are_phrase_anchored() {
        use CloneIntent::*;
        // Clone phrasings.
        for u in [
            "clone my voice",
            "JARVIS, clone my voice with ElevenLabs",
            "make a voice clone of me",
            "register my voice with the cloud, clone it",
        ] {
            assert_eq!(classify_intent(u), Some(Clone), "{u:?} should propose a clone");
        }
        // Forget phrasings (forget verb wins).
        for u in [
            "forget my voice clone",
            "delete the cloned voice, it's my voice",
            "remove my voice clone",
        ] {
            assert_eq!(classify_intent(u), Some(Forget), "{u:?} should forget the clone");
        }
        // Ordinary sentences — and even an ENROLL phrasing (on-device, no cloud
        // upload) — must NOT trip the clone intent.
        for u in [
            "what's the weather",
            "enroll my voice",            // on-device enrollment, not a cloud clone
            "learn my voice",            // on-device enrollment
            "i love the sound of my voice",
            "clone the git repo",        // clone, but not about a voice
        ] {
            assert_eq!(classify_intent(u), None, "{u:?} must not be a clone intent");
        }
    }

    #[test]
    fn confirmation_requires_a_clear_yes() {
        // Every currently-passing affirmative must still confirm.
        assert!(is_confirmation("yes"));
        assert!(is_confirmation("yes, go ahead"));
        assert!(is_confirmation("go ahead"));
        assert!(is_confirmation("clone it"));
        assert!(is_confirmation("confirm"));
        assert!(is_confirmation("yeah"));
        assert!(is_confirmation("yep"));
        assert!(is_confirmation("please do"));
        assert!(is_confirmation("proceed"));
        // A non-affirmative is NOT a confirmation -> the pending clone is cancelled,
        // audio never leaves the device.
        assert!(!is_confirmation("no"));
        assert!(!is_confirmation("cancel"));
        assert!(!is_confirmation("actually never mind"));
        assert!(!is_confirmation(""));
        // NEGATION FAIL-SAFE (build-2 consent contract): a stated REFUSAL must NEVER
        // confirm, even though each of these CONTAINS a multi-word YES substring
        // ("do it" / "clone it"). Before the negation guard these returned true and
        // would have UPLOADED the owner's voice audio against an explicit "no" — the
        // one path where audio leaves the device. Pin them so the regression can't
        // come back.
        assert!(!is_confirmation("don't do it"));
        assert!(!is_confirmation("do not do it"));
        assert!(!is_confirmation("please don't do it"));
        assert!(!is_confirmation("no, don't do it"));
        assert!(!is_confirmation("actually, don't clone it"));
        assert!(!is_confirmation("nope"));
        assert!(!is_confirmation("stop"));
        assert!(!is_confirmation("never mind, don't clone it"));
    }

    #[test]
    fn clone_state_defaults_idle_and_holds_a_pending_proposal() {
        // The default is Idle — no clone proposed, nothing pending.
        assert_eq!(CloneState::default(), CloneState::Idle);
        // A proposal parks a Pending carrying the confined sample + slot — still
        // nothing has left the device.
        let pending = CloneState::Pending {
            sample: PathBuf::from("/root/state/voiceid/owner.wav"),
            agent: "jarvis".to_string(),
        };
        assert_ne!(pending, CloneState::Idle);
    }

    #[test]
    fn confine_sample_rejects_paths_that_escape_the_root() {
        let root = tmp_dir("confine");
        // A real file inside the root is accepted (confined).
        let inside = root.join("state").join("voice-samples").join("owner.wav");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, b"RIFFstub").unwrap();
        let confined = confine_sample(&root, &inside).expect("in-tree file is confined");
        assert!(confined.starts_with(root.canonicalize().unwrap()));

        // A relative path resolving inside the root is also accepted.
        let rel = confine_sample(&root, Path::new("state/voice-samples/owner.wav"));
        assert!(rel.is_some(), "a relative in-tree path must confine");

        // An absolute path OUTSIDE the root is REJECTED (authorization-bound: never
        // an arbitrary recording elsewhere on the disk).
        let outside = std::env::temp_dir().join("someone-elses-voice.wav");
        std::fs::write(&outside, b"RIFFstub").unwrap();
        assert!(
            confine_sample(&root, &outside).is_none(),
            "a path outside the root must be rejected"
        );

        // A `..` traversal escape is rejected too.
        assert!(
            confine_sample(&root, Path::new("state/../../etc/passwd")).is_none(),
            "a .. escape must be rejected"
        );

        // A non-existent in-tree path is rejected (must be a real file).
        assert!(confine_sample(&root, Path::new("state/nope.wav")).is_none());

        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_file(&outside).ok();
    }

    #[test]
    fn default_owner_sample_prefers_voiceid_then_voice_samples() {
        let root = tmp_dir("default-sample");
        // Nothing yet -> None (the caller asks the user to point at a sample).
        assert!(default_owner_sample(&root).is_none());

        // A voice-samples wav is found.
        let samples = root.join("state").join("voice-samples");
        std::fs::create_dir_all(&samples).unwrap();
        std::fs::write(samples.join("kokoro-bm_george.wav"), b"RIFFstub").unwrap();
        let picked = default_owner_sample(&root).expect("voice-samples wav found");
        assert!(picked.to_string_lossy().contains("voice-samples"));

        // A voice-id enrollment wav takes PRIORITY (the owner's authorized audio).
        let vid = root.join("state").join("voiceid");
        std::fs::create_dir_all(&vid).unwrap();
        std::fs::write(vid.join("owner.wav"), b"RIFFstub").unwrap();
        let preferred = default_owner_sample(&root).expect("voiceid wav found");
        assert!(
            preferred.to_string_lossy().contains("voiceid"),
            "voice-id enrollment audio must be preferred: {preferred:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cloned_voices_store_round_trips_and_merges_without_clobbering_config() {
        let root = tmp_dir("clone-store");
        // No store yet -> empty.
        assert!(load_clones(&root).voices.is_empty());

        // Record a confirmed clone, persist, reload.
        let mut clones = ClonedVoices::default();
        clones.set("jarvis", "EL_CLONED_JARVIS");
        save_clones(&root, &clones).expect("persist the non-secret cloned id");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store_path(&root)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "the cloned-id store must be 0600");
        }
        let loaded = load_clones(&root);
        assert_eq!(loaded.voices.get("jarvis").map(String::as_str), Some("EL_CLONED_JARVIS"));

        // merge_into fills an UNMAPPED agent but NEVER clobbers an explicit config map.
        let mut effective: BTreeMap<String, String> = BTreeMap::new();
        effective.insert("jarvis".to_string(), "CONFIG_JARVIS".to_string()); // operator's explicit choice
        let mut more = loaded.clone();
        more.set("friday", "EL_CLONED_FRIDAY"); // unmapped in config
        more.merge_into(&mut effective);
        assert_eq!(
            effective.get("jarvis").map(String::as_str),
            Some("CONFIG_JARVIS"),
            "an explicit config mapping must win over a cloned id"
        );
        assert_eq!(
            effective.get("friday").map(String::as_str),
            Some("EL_CLONED_FRIDAY"),
            "a cloned id fills an unmapped agent"
        );

        // forget drops the slot; after forget the agent has no cloned id.
        let mut f = loaded.clone();
        assert!(f.forget("jarvis"), "forget reports a present clone");
        assert!(!f.forget("jarvis"), "second forget reports none");
        assert!(f.voices.is_empty());

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn active_pronunciation_store_round_trips_and_merges_without_clobbering_config() {
        let root = tmp_dir("pron-store");
        // No store yet -> empty (no active dictionary => today's speech).
        assert!(load_pronunciation(&root).dictionary_id.is_empty());

        // Record a confirmed create_pronunciation, persist, reload.
        let mut active = ActivePronunciation::default();
        active.set("EL_PD_ID", "EL_PD_VER");
        save_pronunciation(&root, &active).expect("persist the non-secret locator");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(pronunciation_store_path(&root))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the pronunciation store must be 0600");
        }
        let loaded = load_pronunciation(&root);
        assert_eq!(loaded.dictionary_id, "EL_PD_ID");
        assert_eq!(loaded.version_id, "EL_PD_VER");

        // merge_into fills an EMPTY config but NEVER clobbers an explicit config value.
        let mut cfg_id = String::new();
        let mut cfg_ver = String::new();
        loaded.merge_into(&mut cfg_id, &mut cfg_ver);
        assert_eq!(cfg_id, "EL_PD_ID", "a minted locator fills an empty config key");
        assert_eq!(cfg_ver, "EL_PD_VER");

        let mut explicit_id = "OPERATOR_PD".to_string();
        let mut explicit_ver = "OPERATOR_VER".to_string();
        loaded.merge_into(&mut explicit_id, &mut explicit_ver);
        assert_eq!(explicit_id, "OPERATOR_PD", "an explicit config value must win");
        assert_eq!(explicit_ver, "OPERATOR_VER");

        // An empty store merges NOTHING (no active dictionary).
        let empty = ActivePronunciation::default();
        let mut id = String::new();
        let mut ver = String::new();
        empty.merge_into(&mut id, &mut ver);
        assert!(id.is_empty() && ver.is_empty(), "an empty store threads no locator");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn consent_prompt_is_honest_about_audio_leaving_and_authorization() {
        let p = consent_prompt("state/voiceid/owner.wav");
        let lower = p.to_lowercase();
        assert!(lower.contains("leaves this device"), "must say the audio leaves the device: {p}");
        assert!(lower.contains("elevenlabs"), "must name the cloud destination: {p}");
        assert!(lower.contains("authorized"), "must name the authorization requirement: {p}");
        // The key is NEVER in any user-facing copy.
        assert!(!lower.contains("api_key"));
        assert!(!lower.contains("xi-api"));
    }

    #[test]
    fn clone_display_name_is_stable_and_secret_free() {
        let n = clone_display_name("jarvis");
        assert!(n.contains("jarvis"));
        assert!(!n.to_lowercase().contains("api_key"));
    }

    /// A unique, throwaway temp dir under the OS temp — no daemon, no network.
    fn tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "jarvis-voiceclone-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
