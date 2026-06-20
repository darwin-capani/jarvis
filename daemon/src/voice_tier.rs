//! VOICE TIER — the honest ElevenLabs cloud-TTS layer (ships ON, INERT WITHOUT A KEY) that sits
//! ON TOP of the on-device Kokoro engine. This module is the pure tier-decision
//! brain: given the config, the active model-swap tier, and whether an ElevenLabs
//! key is present, it answers ONE question — which TTS backend speaks this
//! sentence — and it answers it conservatively.
//!
//! ## What this tier is (and is NOT)
//!
//!   * It is an ADDED VOICE LAYER (text-to-speech only). The premium ElevenLabs
//!     voices are an OPTION; on-device Kokoro is the DEFAULT and the FALLBACK.
//!   * It is NOT a replacement for Kokoro, and NOT a hosted "Conversational
//!     Agents" platform — JARVIS owns its own brain/router/turn-taking. ElevenLabs
//!     is used purely to synthesize speech from text JARVIS already produced.
//!   * It SHIPS ON ([voice].cloud_tier=true, full-power default) but is INERT
//!     WITHOUT AN ELEVENLABS KEY: reached only when the key is present AND the tier
//!     is non-Local; otherwise on-device Kokoro (the private default + fallback).
//!     When active the TTS TEXT leaves the device.
//!
//! ## The precedence (resolve_voice_backend) — ElevenLabs only when ALL hold
//!
//! Choose [`Backend::ElevenLabs`] for an agent ONLY IF:
//!   1. `[voice].cloud_tier` is true (the master switch), AND
//!   2. an `elevenlabs_api_key` is present (`key_present`), AND
//!   3. the active model-swap [`Tier`] is NOT `Local` (i.e. the operator has not
//!      said "work offline / go offline / stay on device" — offline intent keeps
//!      VOICE on-device too), AND
//!   4. the agent has a non-empty mapped ElevenLabs voice id in `[voice.voices]`.
//!
//! In EVERY other case — tier off, no key, offline/Local, or an unmapped agent —
//! fall back to [`Backend::Kokoro`] with that agent's on-device Kokoro voice. So
//! the cloud path can never be reached by accident, and an agent with no EL voice
//! mapped simply keeps its Kokoro voice even while the tier is on for others.
//!
//! ## Honesty / privacy
//!
//! When ElevenLabs is chosen, the sentence text LEAVES the device (a cloud round
//! trip). Kokoro is the private/offline default and the fallback the inference
//! server uses on ANY ElevenLabs error/timeout, so a turn is never failed by the
//! cloud leg. The API key NEVER appears in this module's output, telemetry, or
//! Debug: it is resolved separately (Keychain, allowlisted) and threaded straight
//! to the request — `resolve_voice_backend` is told only WHETHER a key exists, not
//! its value, and the [`Backend`] it returns carries no key.
//!
//! Everything here is HERMETIC: [`resolve_voice_backend`] is a pure function of
//! its inputs (no I/O, no globals, no network), and the [`Backend`] it returns is
//! the single verified contract the speak path threads to the inference server.

use crate::config::Config;
use crate::model_tier::Tier;

/// The Keychain account holding the ElevenLabs TTS key. Single source of truth,
/// mirrored on `integrations::ALLOWED_ACCOUNTS` (a mirror test keeps them in
/// lockstep) so the allowlist + this module can never drift on the account name.
pub const ELEVENLABS_ACCOUNT: &str = "elevenlabs_api_key";

/// Which TTS backend synthesizes a sentence. Kokoro is the on-device default +
/// fallback; ElevenLabs is the opt-in cloud voice tier.
///
/// SECURITY: this enum deliberately carries NO API key — only the voice id and
/// model, both non-secret. The resolved key is threaded separately (request-only,
/// never logged/Debug), so a `Backend` value can be logged or telemetried freely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// On-device Kokoro TTS (the default + the fallback). `voice` is the agent's
    /// Kokoro voice id — exactly what today's pipeline passes.
    Kokoro { voice: String },
    /// The ElevenLabs cloud voice tier. `voice_id` is the agent's mapped EL voice
    /// id and `model` is the EL model id ([voice].model). Reaching this variant
    /// means the text will leave the device for synthesis.
    ElevenLabs { voice_id: String, model: String },
}

impl Backend {
    /// Stable identifier for `voice.tier` telemetry / the HUD indicator. Never the
    /// key (this enum holds no key); "elevenlabs" reads as the CLOUD voice path,
    /// "kokoro" as the ON-DEVICE default.
    pub fn as_str(&self) -> &'static str {
        match self {
            Backend::Kokoro { .. } => "kokoro",
            Backend::ElevenLabs { .. } => "elevenlabs",
        }
    }

    /// Whether this backend makes a CLOUD call (text leaves the device). Kokoro
    /// does NOT (the private/offline path); ElevenLabs does. Part of the tier
    /// contract (mirrors `Tier::is_cloud`); exercised by the selection-matrix test.
    #[allow(dead_code)] // contract predicate; consumed by tests + the honest copy
    pub fn is_cloud(&self) -> bool {
        matches!(self, Backend::ElevenLabs { .. })
    }
}

/// Resolve the TTS backend for `agent_name`, applying the conservative precedence
/// documented at the module level: ElevenLabs ONLY when the cloud tier is on AND a
/// key is present AND the active tier is non-Local AND the agent is mapped to a
/// non-empty EL voice id; otherwise on-device Kokoro with `kokoro_voice` (the
/// agent's Kokoro voice — the fallback for an unmapped agent too).
///
/// * `cfg`          — for `[voice].cloud_tier`, `[voice].model`, `[voice.voices]`.
/// * `agent_name`   — the speaking agent's name (the key into the per-agent map).
/// * `kokoro_voice` — the agent's on-device Kokoro voice id (today's `agent.voice`
///   / `[speech].voice`); used for the Kokoro variant in every fall-through.
/// * `tier`         — the active runtime model-swap [`Tier`]. `Local` means the
///   operator is offline/on-device, so VOICE stays on-device too (NO cloud TTS).
/// * `key_present`  — whether an `elevenlabs_api_key` exists in the Keychain. This
///   is a BOOL by design: the key value never enters this pure decision.
///
/// PURE: no I/O, no globals, no network. The returned [`Backend`] is the single
/// verified contract the speak path threads to the inference server (the resolved
/// key, when ElevenLabs, is fetched + threaded separately — never via this enum).
pub fn resolve_voice_backend(
    cfg: &Config,
    agent_name: &str,
    kokoro_voice: &str,
    tier: Tier,
    key_present: bool,
) -> Backend {
    let kokoro = || Backend::Kokoro {
        voice: kokoro_voice.to_string(),
    };

    // (1) Master switch OFF -> on-device Kokoro, exactly like today.
    if !cfg.voice.cloud_tier {
        return kokoro();
    }
    // (2) No key -> on-device Kokoro. The cloud leg cannot run without the key.
    if !key_present {
        return kokoro();
    }
    // (3) Offline / on-device tier -> VOICE stays on-device too (offline intent
    //     ties to the model-swap "work offline/local"). NO cloud TTS.
    if tier == Tier::Local {
        return kokoro();
    }
    // (4) Per-agent EL voice id: an unmapped agent (or an empty id) keeps its
    //     Kokoro voice even while the tier is on for others.
    match cfg.voice.voices.get(agent_name) {
        Some(voice_id) if !voice_id.trim().is_empty() => Backend::ElevenLabs {
            voice_id: voice_id.clone(),
            model: cfg.voice.model.clone(),
        },
        _ => kokoro(),
    }
}

/// Which STT backend transcribes the user's captured audio. On-device whisper
/// (mlx_whisper, [models].stt) is the DEFAULT, the private/offline path, AND the
/// fallback on ANY cloud error; ElevenLabs Scribe is the opt-in cloud-STT tier.
///
/// HONESTY: STT is MORE sensitive than TTS — choosing `ElevenLabsScribe` means the
/// user's VOICE AUDIO (not just text) leaves the device for the cloud. Whisper is
/// the private default, gated behind its OWN switch ([voice].cloud_stt), and the
/// server falls back to whisper on any Scribe failure so a turn is never lost.
///
/// SECURITY: like [`Backend`], this enum carries NO API key — only the non-secret
/// model id for the Scribe variant. The resolved key is threaded separately
/// (request-only, never logged/Debug), so an `SttBackend` value is safe to
/// telemetry/Debug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SttBackend {
    /// On-device whisper (mlx_whisper) — the default + the fallback. No model id is
    /// carried: the server already knows its resident STT model ([models].stt), so
    /// the wire stays byte-for-byte today's transcribe request.
    Whisper,
    /// The ElevenLabs Scribe cloud-STT tier. `model` is the Scribe model id
    /// ("scribe_v1"). Reaching this variant means the captured audio will leave the
    /// device for transcription.
    ElevenLabsScribe { model: String },
}

/// The ElevenLabs Scribe model id used when the cloud-STT tier is active. A
/// constant (not a config knob): Scribe has a single shipped model and the daemon
/// threads it to the server, which sets it on the speech-to-text request.
pub const SCRIBE_MODEL: &str = "scribe_v1";

impl SttBackend {
    /// Stable identifier for `stt.tier` telemetry / the HUD indicator. Never the
    /// key (this enum holds none); "elevenlabs_scribe" reads as the CLOUD STT path,
    /// "whisper" as the ON-DEVICE default.
    pub fn as_str(&self) -> &'static str {
        match self {
            SttBackend::Whisper => "whisper",
            SttBackend::ElevenLabsScribe { .. } => "elevenlabs_scribe",
        }
    }

    /// Whether this backend makes a CLOUD call (the user's AUDIO leaves the device).
    /// Whisper does NOT (the private/offline default); Scribe does. Mirrors
    /// [`Backend::is_cloud`]; exercised by the selection-matrix test.
    #[allow(dead_code)] // contract predicate; consumed by tests + the honest copy
    pub fn is_cloud(&self) -> bool {
        matches!(self, SttBackend::ElevenLabsScribe { .. })
    }
}

/// Resolve the STT backend, applying the SAME conservative precedence as
/// [`resolve_voice_backend`] but on the SEPARATE `[voice].cloud_stt` switch:
/// ElevenLabs Scribe ONLY when cloud-STT is on AND a key is present AND the active
/// model-swap tier is non-Local; otherwise on-device whisper (the default + the
/// fallback). There is no per-agent map for STT — transcription happens before any
/// agent is selected — so the only gate beyond the switch/key/tier is none.
///
/// * `cfg`         — for `[voice].cloud_stt`.
/// * `tier`        — the active runtime model-swap [`Tier`]. `Local` means the
///   operator is offline/on-device, so STT stays on-device too (NO cloud STT).
/// * `key_present` — whether an `elevenlabs_api_key` exists in the Keychain. A
///   BOOL by design: the key value never enters this pure decision.
///
/// PURE: no I/O, no globals, no network. HONESTY: STT audio is MORE sensitive than
/// TTS text — whisper is the private/offline default and the fallback on ANY Scribe
/// error, and this never picks Scribe by accident (off / no key / offline all stay
/// on whisper).
pub fn resolve_stt_backend(cfg: &Config, tier: Tier, key_present: bool) -> SttBackend {
    // (1) Master switch OFF -> on-device whisper, exactly like today.
    if !cfg.voice.cloud_stt {
        return SttBackend::Whisper;
    }
    // (2) No key -> on-device whisper. The cloud leg cannot run without the key.
    if !key_present {
        return SttBackend::Whisper;
    }
    // (3) Offline / on-device tier -> STT stays on-device too (offline intent ties
    //     to the model-swap "work offline/local"). NO cloud STT.
    if tier == Tier::Local {
        return SttBackend::Whisper;
    }
    // (4) The ONLY cloud-STT path: switch on + key + non-Local.
    SttBackend::ElevenLabsScribe {
        model: SCRIBE_MODEL.to_string(),
    }
}

/// The SOUND-EFFECT CUE gate (Phase-2): whether the `sound_effect` op may be reached.
/// SFX has NO on-device fallback (there is no local SFX generator), so unlike the TTS
/// backend selection this is a SIMPLE enabled gate rather than a backend choice: it is
/// reachable ONLY when `[voice].cloud_sfx` is on AND an `elevenlabs_api_key` is present
/// (`key_present`). With it off, or with no key, the cue is honestly UNAVAILABLE (a
/// silent no-op) — never a fabricated/placeholder cue.
///
/// Like [`resolve_voice_backend`], this is told only WHETHER a key exists (a bool by
/// design — the key value never enters this pure decision). PURE: no I/O, no globals,
/// no network. The daemon still reads the runtime tier at the call site (an offline /
/// `Local` tier is handled there, mirroring the SFX gate's "key + non-Local" contract);
/// this predicate is the switch+key half the unit tests pin.
#[allow(dead_code)] // Phase-2 gate; consumed by trigger_sound_effect + the unit test
pub fn sfx_enabled(cfg: &Config, key_present: bool) -> bool {
    cfg.voice.cloud_sfx && key_present
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model_tier::Tier;

    /// A config with the cloud tier ON and `jarvis`/`friday` mapped to EL voices.
    /// `vision` is deliberately LEFT UNMAPPED to exercise the per-agent fallback.
    fn cfg_tier_on() -> Config {
        let mut c = Config::default();
        c.voice.cloud_tier = true;
        c.voice.model = "eleven_flash_v2_5".to_string();
        c.voice
            .voices
            .insert("jarvis".to_string(), "EL_JARVIS".to_string());
        c.voice
            .voices
            .insert("friday".to_string(), "EL_FRIDAY".to_string());
        // An agent mapped to an EMPTY/whitespace id must still fall back to Kokoro.
        c.voice
            .voices
            .insert("ghost".to_string(), "   ".to_string());
        c
    }

    // -- (a) ships OFF: Kokoro regardless of key/tier ------------------------

    #[test]
    fn tier_off_always_picks_kokoro() {
        // With cloud_tier explicitly OFF, even with a key + heavy tier, the backend is
        // on-device Kokoro with the agent's Kokoro voice. (The shipped DEFAULT is now
        // ON, full-power, but INERT WITHOUT A KEY; this proves the explicit off path.)
        let mut cfg = Config::default();
        cfg.voice.cloud_tier = false;
        assert!(!cfg.voice.cloud_tier, "explicitly disabled cloud tier");
        let b = resolve_voice_backend(&cfg, "jarvis", "bm_george", Tier::Heavy, true);
        assert_eq!(b, Backend::Kokoro { voice: "bm_george".to_string() });
        assert!(!b.is_cloud(), "Kokoro never leaves the device");
        assert_eq!(b.as_str(), "kokoro");
    }

    // -- (b) no key: Kokoro even when the tier is on -------------------------

    #[test]
    fn no_key_picks_kokoro_even_when_enabled() {
        let cfg = cfg_tier_on();
        // jarvis IS mapped + tier is non-Local, but there is NO key -> Kokoro.
        let b = resolve_voice_backend(&cfg, "jarvis", "bm_george", Tier::Heavy, false);
        assert_eq!(b, Backend::Kokoro { voice: "bm_george".to_string() });
    }

    // -- (c) offline/Local tier: Kokoro (offline intent keeps voice on-device) -

    #[test]
    fn local_tier_picks_kokoro_even_with_key_and_mapping() {
        let cfg = cfg_tier_on();
        // Everything else satisfied, but the active tier is Local ("work offline")
        // -> VOICE stays on-device. NO cloud TTS.
        let b = resolve_voice_backend(&cfg, "jarvis", "bm_george", Tier::Local, true);
        assert_eq!(b, Backend::Kokoro { voice: "bm_george".to_string() });
        assert!(!b.is_cloud());
    }

    // -- (d) the ONLY ElevenLabs path: enabled + key + non-Local + mapped ----

    #[test]
    fn elevenlabs_only_when_enabled_key_nonlocal_and_mapped() {
        let cfg = cfg_tier_on();
        for tier in [Tier::Fast, Tier::Heavy] {
            let b = resolve_voice_backend(&cfg, "jarvis", "bm_george", tier, true);
            assert_eq!(
                b,
                Backend::ElevenLabs {
                    voice_id: "EL_JARVIS".to_string(),
                    model: "eleven_flash_v2_5".to_string(),
                },
                "ElevenLabs expected at tier {tier:?}"
            );
            assert!(b.is_cloud(), "ElevenLabs is the cloud path");
            assert_eq!(b.as_str(), "elevenlabs");
        }
        // friday is also mapped (its own voice id).
        let b = resolve_voice_backend(&cfg, "friday", "bf_emma", Tier::Heavy, true);
        assert_eq!(
            b,
            Backend::ElevenLabs {
                voice_id: "EL_FRIDAY".to_string(),
                model: "eleven_flash_v2_5".to_string(),
            }
        );
    }

    // -- (e) per-agent fallback: unmapped / empty-mapped -> Kokoro voice -----

    #[test]
    fn unmapped_agent_falls_back_to_its_kokoro_voice() {
        let cfg = cfg_tier_on();
        // vision is NOT in the map -> Kokoro with its own voice, even with the
        // tier on + key + a cloud tier.
        let b = resolve_voice_backend(&cfg, "vision", "bf_isabella", Tier::Heavy, true);
        assert_eq!(b, Backend::Kokoro { voice: "bf_isabella".to_string() });
        // ghost is mapped to whitespace -> treated as unmapped -> Kokoro.
        let b = resolve_voice_backend(&cfg, "ghost", "am_onyx", Tier::Heavy, true);
        assert_eq!(b, Backend::Kokoro { voice: "am_onyx".to_string() });
    }

    // -- (f) the resolved Backend NEVER carries the key (Debug-safe) ---------

    #[test]
    fn backend_debug_and_value_never_contain_a_key() {
        // resolve_voice_backend is told only key_present (a bool); the value it
        // returns is constructed from config voice ids + model — never a key. Prove
        // the Debug rendering of BOTH variants carries nothing key-shaped.
        let cfg = cfg_tier_on();
        let el = resolve_voice_backend(&cfg, "jarvis", "bm_george", Tier::Heavy, true);
        let kk = resolve_voice_backend(&cfg, "vision", "bf_isabella", Tier::Heavy, true);
        for b in [&el, &kk] {
            let dbg = format!("{b:?}");
            assert!(!dbg.contains("xi-api"), "no header name in Debug: {dbg}");
            assert!(!dbg.to_lowercase().contains("api_key"), "no key field in Debug: {dbg}");
            // The Backend type has no key field at all — assert structurally too.
        }
        // ElevenLabs variant exposes only voice_id + model (both non-secret).
        if let Backend::ElevenLabs { voice_id, model } = &el {
            assert_eq!(voice_id, "EL_JARVIS");
            assert_eq!(model, "eleven_flash_v2_5");
        } else {
            panic!("expected ElevenLabs");
        }
    }

    // -- (g) the EL account name is the allowlisted Keychain account ---------

    #[test]
    fn elevenlabs_account_constant_is_the_keychain_account() {
        assert_eq!(ELEVENLABS_ACCOUNT, "elevenlabs_api_key");
    }

    // === SFX cue gate (Phase-2): sfx_enabled ===============================

    /// The SFX cue gate is a SIMPLE switch+key AND (no on-device fallback): reachable
    /// ONLY when `[voice].cloud_sfx` is on AND a key is present. The shipped default is
    /// cloud_sfx=ON, but INERT WITHOUT A KEY — so the default with no key is still
    /// disabled (an honest silent no-op), and any one of {switch off, no key} disables.
    #[test]
    fn sfx_gate_needs_both_the_switch_and_a_key() {
        // Shipped default: cloud_sfx ON. With NO key it is still disabled (INERT
        // WITHOUT A KEY — there is no on-device SFX generator to fall back to).
        let default_cfg = Config::default();
        assert!(default_cfg.voice.cloud_sfx, "SFX cue tier SHIPS ON (full-power default)");
        assert!(
            !sfx_enabled(&default_cfg, false),
            "ON but NO KEY -> disabled (honest silent no-op, never a fabricated cue)"
        );
        // ON + key -> the ONLY enabled cell.
        assert!(sfx_enabled(&default_cfg, true), "ON + key -> SFX is reachable");

        // Switch explicitly OFF -> disabled even WITH a key.
        let mut off = Config::default();
        off.voice.cloud_sfx = false;
        assert!(!sfx_enabled(&off, true), "switch OFF -> disabled even with a key");
        assert!(!sfx_enabled(&off, false), "switch OFF + no key -> disabled");
    }

    // === STT tier (build 2/2): resolve_stt_backend ==========================

    /// A config with the SEPARATE cloud-STT switch ON (TTS tier irrelevant here).
    fn cfg_stt_on() -> Config {
        let mut c = Config::default();
        c.voice.cloud_stt = true;
        c
    }

    // -- STT (a) ships OFF (pinned): whisper regardless of key/tier ----------

    #[test]
    fn stt_tier_when_disabled_picks_whisper() {
        // With cloud_stt explicitly OFF, even with a key + heavy tier, transcription is
        // on-device whisper. (The shipped DEFAULT is now ON, full-power, but INERT
        // WITHOUT A KEY; STT is MORE sensitive than TTS — this proves the off path.)
        let mut cfg = Config::default();
        cfg.voice.cloud_stt = false;
        assert!(!cfg.voice.cloud_stt, "explicitly disabled Scribe cloud-STT tier");
        let b = resolve_stt_backend(&cfg, Tier::Heavy, true);
        assert_eq!(b, SttBackend::Whisper);
        assert!(!b.is_cloud(), "whisper never sends the user's audio off-device");
        assert_eq!(b.as_str(), "whisper");
    }

    // -- STT (b) no key: whisper even when enabled --------------------------

    #[test]
    fn stt_no_key_picks_whisper_even_when_enabled() {
        let cfg = cfg_stt_on();
        let b = resolve_stt_backend(&cfg, Tier::Heavy, false);
        assert_eq!(b, SttBackend::Whisper, "no key -> on-device whisper");
    }

    // -- STT (c) offline/Local tier: whisper (offline keeps STT on-device) --

    #[test]
    fn stt_local_tier_picks_whisper_even_with_key() {
        let cfg = cfg_stt_on();
        // Switch on + key present, but the active tier is Local ("work offline")
        // -> the user's audio stays on-device. NO cloud STT.
        let b = resolve_stt_backend(&cfg, Tier::Local, true);
        assert_eq!(b, SttBackend::Whisper);
        assert!(!b.is_cloud());
    }

    // -- STT (d) the ONLY Scribe path: enabled + key + non-Local ------------

    #[test]
    fn stt_scribe_only_when_enabled_key_and_nonlocal() {
        let cfg = cfg_stt_on();
        for tier in [Tier::Fast, Tier::Heavy] {
            let b = resolve_stt_backend(&cfg, tier, true);
            assert_eq!(
                b,
                SttBackend::ElevenLabsScribe { model: SCRIBE_MODEL.to_string() },
                "Scribe expected at tier {tier:?}"
            );
            assert!(b.is_cloud(), "Scribe is the cloud STT path (audio leaves the device)");
            assert_eq!(b.as_str(), "elevenlabs_scribe");
        }
    }

    // -- STT (e) the TTS and STT switches are INDEPENDENT --------------------

    #[test]
    fn stt_switch_is_independent_of_the_tts_switch() {
        // cloud_tier (TTS) on but cloud_stt (STT) off -> STT stays whisper. The
        // more-sensitive audio leg has its OWN gate and is not turned on by the
        // text-only TTS switch.
        let mut cfg = Config::default();
        cfg.voice.cloud_tier = true;
        cfg.voice.cloud_stt = false;
        assert_eq!(
            resolve_stt_backend(&cfg, Tier::Heavy, true),
            SttBackend::Whisper,
            "the TTS switch must NOT enable the STT cloud leg"
        );

        // And vice versa: cloud_stt on, cloud_tier off -> STT is Scribe.
        cfg.voice.cloud_tier = false;
        cfg.voice.cloud_stt = true;
        assert!(resolve_stt_backend(&cfg, Tier::Heavy, true).is_cloud());
    }

    // -- STT (f) the resolved SttBackend NEVER carries the key (Debug-safe) --

    #[test]
    fn stt_backend_debug_never_contains_a_key() {
        // resolve_stt_backend is told only key_present (a bool); the value it
        // returns carries only the model id. Prove the Debug rendering of BOTH
        // variants carries nothing key-shaped.
        let cfg = cfg_stt_on();
        let scribe = resolve_stt_backend(&cfg, Tier::Heavy, true);
        let whisper = resolve_stt_backend(&cfg, Tier::Local, true);
        for b in [&scribe, &whisper] {
            let dbg = format!("{b:?}");
            assert!(!dbg.contains("xi-api"), "no header name in Debug: {dbg}");
            assert!(!dbg.to_lowercase().contains("api_key"), "no key field in Debug: {dbg}");
        }
        if let SttBackend::ElevenLabsScribe { model } = &scribe {
            assert_eq!(model, "scribe_v1", "Scribe carries only the non-secret model id");
        } else {
            panic!("expected Scribe");
        }
    }

    // -- STT (g) full matrix sweep: only the on+key+non-Local cells are cloud -

    #[test]
    fn stt_selection_matrix_is_conservative() {
        let cfg = cfg_stt_on();
        // (key_present, tier) -> expected cloud?  Only key && non-Local is cloud.
        let cases = [
            (true, Tier::Heavy, true),
            (true, Tier::Fast, true),
            (true, Tier::Local, false),
            (false, Tier::Heavy, false),
            (false, Tier::Fast, false),
            (false, Tier::Local, false),
        ];
        for (key, tier, want_cloud) in cases {
            let b = resolve_stt_backend(&cfg, tier, key);
            assert_eq!(
                b.is_cloud(),
                want_cloud,
                "key={key} tier={tier:?} should be cloud={want_cloud}, got {b:?}"
            );
        }
        // And with the switch explicitly OFF, NO cell is ever cloud (the off guard).
        let mut off = Config::default();
        off.voice.cloud_stt = false;
        for tier in [Tier::Heavy, Tier::Fast, Tier::Local] {
            assert!(
                !resolve_stt_backend(&off, tier, true).is_cloud(),
                "cloud_stt OFF must keep every cell on-device at tier {tier:?}"
            );
        }
    }

    // -- (h) full matrix sweep: only ONE cell is ElevenLabs ------------------

    #[test]
    fn backend_selection_matrix_is_conservative() {
        let cfg = cfg_tier_on();
        // (key_present, tier) -> expected cloud?  Only key && non-Local is cloud
        // (jarvis is mapped). Local and no-key are always on-device.
        let cases = [
            (true, Tier::Heavy, true),
            (true, Tier::Fast, true),
            (true, Tier::Local, false),
            (false, Tier::Heavy, false),
            (false, Tier::Fast, false),
            (false, Tier::Local, false),
        ];
        for (key, tier, want_cloud) in cases {
            let b = resolve_voice_backend(&cfg, "jarvis", "bm_george", tier, key);
            assert_eq!(
                b.is_cloud(),
                want_cloud,
                "key={key} tier={tier:?} should be cloud={want_cloud}, got {b:?}"
            );
        }
    }
}
