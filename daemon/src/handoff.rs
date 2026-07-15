//! CONTINUITY HANDOFF (handoff.rs) — resume a LIVE cognitive session on ANOTHER
//! of the owner's Macs. A running session's CONTEXT — its transcript window, the
//! active agent, the mission it's on, the pending world-model deltas, the draft
//! ids in flight, and the focus profile — is sealed into a [`SessionCapsule`] and
//! moved to the owner's OTHER device over the EXISTING sync.rs sealed path
//! (AES-256-GCM under a Keychain-paired `handoff_shared_key`). The receiving
//! daemon RESTORES that context and PARKS: it repopulates what the assistant
//! knows and does NOTHING with it.
//!
//! THE HARD RULES (each pinned by a test):
//!   1. SHIPS OFF. `[handoff].enabled` defaults false, and it rides sync (also
//!      OFF). Off, every entry point is a no-op — nothing is sealed, nothing is
//!      staged, and the status honestly reports "off".
//!   2. AUTHORITY NEVER TRANSFERS. The capsule carries CONTEXT, never PERMISSION.
//!      It has NO resolved credential, NO bearer/OAuth token, NO confirm bit, and
//!      NO authority field — the free-text it does carry (the transcript window,
//!      the world-model deltas) is REDACTED at build time with the SAME stripper
//!      the macro recorder uses ([`crate::optimize::redact`]), so a secret can
//!      never ride across. Restoring context is NOT restoring permission: on the
//!      receiving device EVERY consequential step re-hits the FRESH confirm
//!      ([`crate::confirm`]) + voice-id ([`crate::voiceid`]) + master switch +
//!      lockdown, exactly as if the session had started there.
//!   3. RESTORE PARKS. [`restore`] repopulates the context and returns a session
//!      that is PARKED (`parked == true`); it never replays an action, never
//!      resumes a pending confirm, never acts.
//!
//! The capsule build + seal/open + the redaction are a PURE, hermetically tested
//! seam (an injected key; no live peer, no Keychain, no session loop in the
//! tests). The network leg between devices is the SAME armed-but-inert transport
//! sync.rs rides — a sealed capsule never leaves the box in the clear.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// The capsule wire-format version — bumped if the sealed shape changes so an old
/// peer's capsule is rejected honestly rather than mis-parsed.
const CAPSULE_VERSION: u32 = 1;
/// Bounds — a capsule is a RESUME HINT, not an archive. The window is the tail of
/// the conversation, the deltas/ids the small in-flight set.
const MAX_TRANSCRIPT_LINES: usize = 40;
const MAX_WORLD_DELTAS: usize = 64;
const MAX_DRAFT_IDS: usize = 64;
/// Per-field character cap (a redacted line, a delta, a ref) — keeps one capsule
/// bounded even before the seal.
const MAX_FIELD_LEN: usize = 2000;

/// Truncate `s` to at most `max` chars (char-boundary safe), so a capsule field
/// is always bounded.
fn bound(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

// ---------------------------------------------------------------------------
// The capsule — the sealed, secret-free unit of context
// ---------------------------------------------------------------------------

/// The RAW live-session refs the daemon hands to [`build_capsule`]. The free-text
/// (`transcript_window`, `world_deltas`) may contain anything the user said or the
/// model extracted, so it is REDACTED at build time; the refs (`active_agent`,
/// `mission_ref`, `draft_ids`, `focus_profile`) are secret-free identifiers by
/// construction (agent name / mission id / content-hash draft ids / profile name),
/// carried verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionRefs {
    pub transcript_window: Vec<String>,
    pub active_agent: String,
    pub mission_ref: Option<String>,
    pub world_deltas: Vec<String>,
    pub draft_ids: Vec<String>,
    pub focus_profile: String,
}

/// A sealed-able snapshot of a live session's CONTEXT. DELIBERATELY carries NO
/// authority/permission/credential/bearer/token/confirm field — restoring it
/// repopulates what the assistant knows, never what it is allowed to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCapsule {
    pub version: u32,
    /// The exporting device's stable id (meta.device_id) — so the receiving Mac
    /// can name where the session came from ("Resume on <device>").
    pub origin_device: String,
    pub created: String,
    /// The tail of the conversation, each line REDACTED (secret-free).
    pub transcript_window: Vec<String>,
    /// The active agent's name (a roster identifier, not free-text).
    pub active_agent: String,
    /// The current mission reference (an id), if a mission is in flight.
    pub mission_ref: Option<String>,
    /// Pending world-model deltas, each REDACTED (secret-free).
    pub world_deltas: Vec<String>,
    /// The in-flight draft ids (content hashes; secret-free by construction).
    pub draft_ids: Vec<String>,
    /// The active focus profile's name.
    pub focus_profile: String,
}

/// Build a [`SessionCapsule`] from the live session refs, REDACTING every
/// free-text span so NO resolved credential / bearer / secret can ride across —
/// the SAME [`crate::optimize::redact`] the macro recorder uses. PURE: the
/// redaction IS the no-secret guarantee, unit-testable without a session. The
/// window / lists are bounded (a capsule is a resume hint, not an archive).
///
/// Only the genuinely free-text fields are redacted (`transcript_window`,
/// `world_deltas`); the refs are secret-free identifiers carried verbatim (a
/// content-hash draft id must survive intact — redaction would corrupt it).
pub fn build_capsule(origin_device: &str, created: &str, refs: SessionRefs) -> SessionCapsule {
    let transcript_window = refs
        .transcript_window
        .into_iter()
        .take(MAX_TRANSCRIPT_LINES)
        .map(|line| bound(&crate::optimize::redact(&line), MAX_FIELD_LEN))
        .collect();
    let world_deltas = refs
        .world_deltas
        .into_iter()
        .take(MAX_WORLD_DELTAS)
        .map(|d| bound(&crate::optimize::redact(&d), MAX_FIELD_LEN))
        .collect();
    let draft_ids = refs
        .draft_ids
        .into_iter()
        .take(MAX_DRAFT_IDS)
        .map(|id| bound(&id, 64))
        .collect();
    SessionCapsule {
        version: CAPSULE_VERSION,
        origin_device: bound(origin_device, 128),
        created: bound(created, 64),
        transcript_window,
        active_agent: bound(&refs.active_agent, 64),
        mission_ref: refs.mission_ref.map(|m| bound(&m, 128)),
        world_deltas,
        draft_ids,
        focus_profile: bound(&refs.focus_profile, 64),
    }
}

/// Serialize a capsule to JSON bytes (the plaintext fed to the seal).
pub fn serialize_capsule(c: &SessionCapsule) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(c)?)
}

/// Parse a decrypted capsule, rejecting a wrong/old wire version honestly.
pub fn deserialize_capsule(bytes: &[u8]) -> anyhow::Result<SessionCapsule> {
    let c: SessionCapsule = serde_json::from_slice(bytes)?;
    if c.version != CAPSULE_VERSION {
        anyhow::bail!(
            "handoff capsule version {} is not supported (this device speaks {CAPSULE_VERSION})",
            c.version
        );
    }
    Ok(c)
}

/// Seal a capsule under the paired key, riding the EXISTING sync.rs AES-256-GCM
/// sealed path. The plaintext is the serialized capsule; a fresh nonce is
/// prepended and the GCM tag authenticates the whole payload, so ANY tamper (or a
/// wrong key) makes [`open_capsule`] fail rather than return garbage. Nothing
/// leaves the box unsealed.
pub fn seal_capsule(key: &[u8; 32], c: &SessionCapsule) -> anyhow::Result<Vec<u8>> {
    crate::sync::seal(key, &serialize_capsule(c)?)
}

/// Open + parse a sealed capsule. FAILS (never returns garbage) on a wrong key, a
/// truncated payload, ANY tamper, or a bad wire version.
pub fn open_capsule(key: &[u8; 32], sealed: &[u8]) -> anyhow::Result<SessionCapsule> {
    deserialize_capsule(&crate::sync::open(key, sealed)?)
}

// ---------------------------------------------------------------------------
// Restore — repopulates context and PARKS (never acts)
// ---------------------------------------------------------------------------

/// The receiving daemon's view after a restore: the context is repopulated but
/// the session is PARKED. There is DELIBERATELY no authority/permission/confirm
/// field here or in [`SessionCapsule`] — restoring context is not restoring
/// permission, so there is nothing to "resume" into an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredSession {
    pub capsule: SessionCapsule,
    /// ALWAYS true. The receiving daemon repopulates context and stops; every
    /// consequential step re-hits the fresh confirm + voice-id + master switch +
    /// lockdown, exactly as a fresh session on this device would.
    pub parked: bool,
    /// A human-readable note the HUD/voice surfaces — states plainly that
    /// authority did not transfer.
    pub note: String,
}

/// Restore a session's CONTEXT from an opened capsule and PARK. Repopulating what
/// the assistant knows NEVER grants it permission to act — `parked` is hard-coded
/// true and the returned session carries no authority. PURE.
pub fn restore(capsule: SessionCapsule) -> RestoredSession {
    let origin = if capsule.origin_device.trim().is_empty() {
        "your other Mac".to_string()
    } else {
        capsule.origin_device.clone()
    };
    let note = format!(
        "Restored the session context from {origin} — parked, sir. Authority did not transfer: \
         every consequential step still needs a fresh confirm, voice-id, and the master switch on this device."
    );
    RestoredSession { capsule, parked: true, note }
}

// ---------------------------------------------------------------------------
// The device-gated staging — sealed to disk here, sent only on-device behind the
// gate (rides the sync.rs armed-but-inert transport)
// ---------------------------------------------------------------------------

/// The handoff state tree under the daemon-owned, gitignored `state/`.
pub fn handoff_root(root: &Path) -> PathBuf {
    root.join("state").join("handoff")
}

/// The sealed inbound capsules a paired device left for this Mac, in stable
/// sorted order.
fn inbound_files(root: &Path) -> Vec<PathBuf> {
    let inbox = handoff_root(root).join("inbox");
    let Ok(read) = std::fs::read_dir(&inbox) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
    paths.sort();
    paths
}

/// Seal `capsule` to the outbox for the paired device. The plaintext is never
/// written; only the AES-256-GCM sealed bytes touch disk.
pub fn stage_capsule(root: &Path, key: &[u8; 32], capsule: &SessionCapsule) -> anyhow::Result<()> {
    let outbox = handoff_root(root).join("outbox");
    std::fs::create_dir_all(&outbox)?;
    let sealed = seal_capsule(key, capsule)?;
    let name = if capsule.origin_device.trim().is_empty() {
        "session".to_string()
    } else {
        capsule.origin_device.clone()
    };
    std::fs::write(outbox.join(format!("{name}.capsule")), sealed)?;
    Ok(())
}

/// Open + restore + PARK every sealed capsule a paired device left in the inbox.
/// A capsule that fails to open (wrong key / tampered / bad version) is SKIPPED
/// honestly, never restored. Every returned session is PARKED.
pub fn resume_from_inbox(root: &Path, key: &[u8; 32]) -> Vec<RestoredSession> {
    let mut out = Vec::new();
    for path in inbound_files(root) {
        let Ok(sealed) = std::fs::read(&path) else { continue };
        match open_capsule(key, &sealed) {
            Ok(c) => out.push(restore(c)),
            Err(_) => continue, // wrong key / tampered / bad version -> skip, never restore
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The operator-triggered orchestrator
// ---------------------------------------------------------------------------

/// Hand the current session off to the paired Mac: seal its `capsule` to the
/// outbox. OFF (the shipped default) is a no-op; without the shared key it
/// refuses, NEVER staging an unsealed capsule. `key` is injected so this is
/// hermetically tested. The delivery leg rides the sync.rs armed-but-inert
/// transport (a configured `[sync].peer_endpoint`), never a background cadence.
pub fn hand_off(
    cfg: &crate::config::Config,
    root: &Path,
    capsule: &SessionCapsule,
    key: Option<crate::crypto::SecretKey>,
) -> String {
    if !cfg.handoff.enabled {
        return "Continuity handoff is off, sir — turn on [handoff].enabled to resume this session on your other Mac.".to_string();
    }
    let Some(key) = key else {
        return "No shared handoff key, sir — pair your Macs (the key lives in the Keychain as handoff_shared_key). A session capsule is never staged in the clear.".to_string();
    };
    match stage_capsule(root, key.raw_bytes(), capsule) {
        Ok(()) => {
            let n = capsule.transcript_window.len();
            let peer = !cfg.sync.peer_endpoint.trim().is_empty();
            format!(
                "Sealed your session ({n} transcript line{}) to hand off{}. Authority did not transfer — every consequential step re-asks on the other Mac, sir.",
                if n == 1 { "" } else { "s" },
                if peer {
                    " and armed for delivery (transport rides sync, inert without a live peer)"
                } else {
                    " to the outbox (transport to your paired Mac is armed but inert)"
                }
            )
        }
        Err(e) => format!("Couldn't seal the handoff capsule: {e}"),
    }
}

// ---------------------------------------------------------------------------
// The honest status surface
// ---------------------------------------------------------------------------

/// The `handoff.status` wire payload. PURE + total. SECRET-FREE: booleans, a
/// pending-capsule flag, and the paired device's non-secret label — never a
/// transcript line, never a fact, never the key.
pub fn status_payload(
    enabled: bool,
    key_present: bool,
    peer_configured: bool,
    pending_capsule: bool,
    device: &str,
) -> Value {
    json!({
        "enabled": enabled,
        "key_present": key_present,
        "peer_configured": peer_configured,
        "transport_inert": true,
        // A capsule carries CONTEXT, never PERMISSION — its free-text is redacted
        // like a macro, so no resolved credential/bearer rides across.
        "carries_credentials": false,
        // Restoring context on the receiving device NEVER restores permission:
        // every consequential step re-hits the fresh confirm + voice-id + master
        // switch + lockdown. Pinned honest — a payload can't claim otherwise.
        "restore_parks": true,
        "pending_capsule": pending_capsule,
        "device": device,
    })
}

/// The newest inbound capsule that opens under `key`, if any (needs the key to
/// read the origin device for the panel's "Resume on <device>").
fn staged_inbound(root: &Path, key: Option<&crate::crypto::SecretKey>) -> Option<SessionCapsule> {
    let key = key?;
    for path in inbound_files(root).into_iter().rev() {
        if let Ok(sealed) = std::fs::read(&path) {
            if let Ok(c) = open_capsule(key.raw_bytes(), &sealed) {
                return Some(c);
            }
        }
    }
    None
}

/// Emit `handoff.status` for the HUD on the audit-snapshot cadence. READ-ONLY:
/// probes the key/peer/inbox; runs no handoff. Fail-open.
///
/// OFF (the shipped default) emits the honest off payload WITHOUT touching the
/// Keychain (resolve_secret spawns a real security(1) subprocess up to a 5s
/// timeout; this fn rides the shared snapshot cadence, so only an armed
/// `[handoff]` pays that bounded probe).
pub async fn emit_status(cfg: &crate::config::Config, root: &Path) {
    if !cfg.handoff.enabled {
        crate::telemetry::emit(
            "system",
            "handoff.status",
            status_payload(false, false, false, false, ""),
        );
        return;
    }
    let key = crate::integrations::resolve_secret("handoff_shared_key")
        .await
        .and_then(|hex| crate::crypto::SecretKey::from_hex(hex.trim()).ok());
    let peer_configured = !cfg.sync.peer_endpoint.trim().is_empty();
    let pending = !inbound_files(root).is_empty();
    let device = staged_inbound(root, key.as_ref())
        .map(|c| c.origin_device)
        .unwrap_or_default();
    crate::telemetry::emit(
        "system",
        "handoff.status",
        status_payload(true, key.is_some(), peer_configured, pending, &device),
    );
}

// ---------------------------------------------------------------------------
// Tests — the capsule build + redaction + seal/open + restore-parks are PURE and
// hermetic (an injected key + a tempdir; no live peer, no Keychain, no session).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> crate::crypto::SecretKey {
        crate::crypto::SecretKey::from_bytes([11u8; 32])
    }

    /// A session with a token-shaped secret AND a bearer AND an email planted in
    /// both free-text fields — the redaction must strip all of them.
    fn refs_with_a_secret() -> SessionRefs {
        SessionRefs {
            transcript_window: vec![
                "remind me to email darcapalb@gmail.com".to_string(),
                "the api key is sk-ABCD1234SECRETTOKEN use it".to_string(),
            ],
            active_agent: "aegis".to_string(),
            mission_ref: Some("mission-42".to_string()),
            world_deltas: vec!["auth header Bearer sk-LIVEDEADBEEF1234567890 seen".to_string()],
            draft_ids: vec!["a1b2c3d4e5f6".to_string()],
            focus_profile: "deep-work".to_string(),
        }
    }

    // -- build carries the refs -------------------------------------------------

    #[test]
    fn build_carries_the_session_refs_and_bounds_them() {
        let c = build_capsule("dev-a", "2026-07-15T10:00:00Z", refs_with_a_secret());
        assert_eq!(c.version, CAPSULE_VERSION);
        assert_eq!(c.origin_device, "dev-a");
        assert_eq!(c.active_agent, "aegis");
        assert_eq!(c.mission_ref.as_deref(), Some("mission-42"));
        assert_eq!(c.draft_ids, vec!["a1b2c3d4e5f6"], "content-hash draft ids survive verbatim");
        assert_eq!(c.focus_profile, "deep-work");
        assert_eq!(c.transcript_window.len(), 2, "the window is carried");
        assert_eq!(c.world_deltas.len(), 1, "the pending deltas are carried");

        // The window is bounded to the tail.
        let big = SessionRefs {
            transcript_window: (0..500).map(|i| format!("line {i}")).collect(),
            ..Default::default()
        };
        let c = build_capsule("d", "t", big);
        assert_eq!(c.transcript_window.len(), MAX_TRANSCRIPT_LINES, "window bounded");
    }

    // -- redaction: NO credential/bearer/secret survives the seal ---------------

    #[test]
    fn build_redacts_every_free_text_field_so_no_secret_survives() {
        let c = build_capsule("dev-a", "t", refs_with_a_secret());
        let joined = format!("{:?}{:?}", c.transcript_window, c.world_deltas);
        assert!(!joined.contains("sk-ABCD1234SECRETTOKEN"), "token stripped: {joined}");
        assert!(!joined.contains("sk-LIVEDEADBEEF1234567890"), "bearer payload stripped: {joined}");
        assert!(!joined.contains("darcapalb@gmail.com"), "email stripped: {joined}");
        assert!(joined.contains("[redacted]"), "redaction markers present: {joined}");
    }

    #[test]
    fn no_secret_is_present_in_the_sealed_bytes_or_the_opened_capsule() {
        let key = *test_key().raw_bytes();
        let c = build_capsule("dev-a", "t", refs_with_a_secret());
        let sealed = seal_capsule(&key, &c).unwrap();
        // Not in the ciphertext (belt-and-braces: the free-text was redacted
        // BEFORE the seal, and the seal itself is opaque).
        assert!(
            !sealed.windows(6).any(|w| w == b"SECRET"),
            "no plaintext secret on the wire"
        );
        // And not in the opened capsule (the redaction is durable through the seal).
        let opened = open_capsule(&key, &sealed).unwrap();
        let joined = format!("{:?}{:?}", opened.transcript_window, opened.world_deltas);
        assert!(!joined.contains("SECRETTOKEN"), "no secret survives the round-trip: {joined}");
        assert!(!joined.contains("Bearer sk-"), "no bearer survives the round-trip: {joined}");
    }

    // -- seal / open round-trip (injected key) ----------------------------------

    #[test]
    fn seal_open_round_trips_and_rejects_tamper_wrong_key_and_bad_version() {
        let key = *test_key().raw_bytes();
        let c = build_capsule("dev-a", "2026-07-15T10:00:00Z", refs_with_a_secret());
        let sealed = seal_capsule(&key, &c).unwrap();
        assert_eq!(open_capsule(&key, &sealed).unwrap(), c, "round-trips exactly");

        // A single flipped byte fails authentication.
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(open_capsule(&key, &tampered).is_err(), "tamper is caught");

        // The wrong key fails.
        assert!(open_capsule(&[9u8; 32], &sealed).is_err(), "wrong key fails");

        // A future/old wire version is refused, not mis-parsed.
        let mut bad = c.clone();
        bad.version = 999;
        let sealed_bad = seal_capsule(&key, &bad).unwrap();
        assert!(open_capsule(&key, &sealed_bad).is_err(), "bad version refused");
    }

    // -- restore PARKS; the capsule has NO authority/permission field -----------

    #[test]
    fn restore_parks_and_never_grants_authority() {
        let c = build_capsule("dev-a", "t", refs_with_a_secret());
        let restored = restore(c.clone());
        assert!(restored.parked, "restore ALWAYS parks");
        assert_eq!(restored.capsule, c, "context is repopulated verbatim");
        assert!(restored.note.to_lowercase().contains("authority did not transfer"));
    }

    #[test]
    fn the_capsule_shape_carries_no_authority_or_permission_field() {
        // Assert on the FIELD NAMES (keys), not the values — a redacted transcript
        // line may legitimately still read "...bearer [redacted]..." as descriptive
        // text once the secret itself is stripped. What must never exist is a
        // capsule FIELD that could carry authority/permission across the handoff.
        let c = build_capsule("dev-a", "t", refs_with_a_secret());
        let value = serde_json::to_value(&c).unwrap();
        let keys: Vec<String> = value
            .as_object()
            .expect("the capsule serializes to a JSON object")
            .keys()
            .map(|k| k.to_lowercase())
            .collect();
        for forbidden in [
            "authority",
            "permission",
            "credential",
            "bearer",
            "token",
            "confirm",
            "password",
            "secret",
            "master_switch",
            "voiceid",
        ] {
            assert!(
                !keys.iter().any(|k| k.contains(forbidden)),
                "the capsule must carry no `{forbidden}` field; keys were {keys:?}"
            );
        }
        // Concretely: the capsule's fields are exactly the context refs — nothing
        // that grants permission.
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "active_agent",
                "created",
                "draft_ids",
                "focus_profile",
                "mission_ref",
                "origin_device",
                "transcript_window",
                "version",
                "world_deltas",
            ]
        );
    }

    // -- status payload is honest ----------------------------------------------

    #[test]
    fn status_is_honest_about_off_transport_credentials_and_parking() {
        let off = status_payload(false, false, false, false, "");
        assert_eq!(off["enabled"], false);
        assert_eq!(off["transport_inert"], true);
        assert_eq!(off["carries_credentials"], false, "honest: no credentials ride across");
        assert_eq!(off["restore_parks"], true, "honest: restoring never restores permission");

        let armed = status_payload(true, true, true, true, "dev-b");
        assert_eq!(armed["key_present"], true);
        assert_eq!(armed["peer_configured"], true);
        assert_eq!(armed["pending_capsule"], true);
        assert_eq!(armed["device"], "dev-b");
        // Pinned honest even when armed.
        assert_eq!(armed["carries_credentials"], false);
        assert_eq!(armed["restore_parks"], true);
    }

    // -- orchestrator (hermetic: tempdir + injected key) ------------------------

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("darwin-handoff-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    #[test]
    fn hand_off_is_off_and_keyless_safe_and_never_stages_in_the_clear() {
        let dir = tempdir("off");
        let capsule = build_capsule("dev-a", "t", refs_with_a_secret());

        // Off: no-op, nothing on disk.
        let cfg = crate::config::Config::default();
        let r = hand_off(&cfg, &dir.0, &capsule, Some(test_key()));
        assert!(r.contains("off"), "{r}");
        assert!(!handoff_root(&dir.0).exists(), "off never touches disk");

        // On but no key: refuses, and NOTHING is written in the clear.
        let mut cfg2 = crate::config::Config::default();
        cfg2.handoff.enabled = true;
        let r = hand_off(&cfg2, &dir.0, &capsule, None);
        assert!(r.contains("No shared handoff key"), "{r}");
        assert!(!handoff_root(&dir.0).exists(), "no plaintext capsule without a key");
    }

    #[test]
    fn seal_to_outbox_then_restore_from_inbox_parks_and_carries_no_secret() {
        let dir = tempdir("e2e");
        let mut cfg = crate::config::Config::default();
        cfg.handoff.enabled = true;
        let key = test_key();
        let capsule = build_capsule("dev-a", "2026-07-15T10:00:00Z", refs_with_a_secret());

        // Hand off: sealed to the outbox, transport reported inert.
        let r = hand_off(&cfg, &dir.0, &capsule, Some(test_key()));
        assert!(r.contains("armed but inert"), "transport honesty: {r}");
        let out = std::fs::read(handoff_root(&dir.0).join("outbox").join("dev-a.capsule")).unwrap();
        assert!(!out.windows(6).any(|w| w == b"SECRET"), "the staged capsule is sealed, no plaintext on disk");

        // The receiving device: drop the sealed capsule in the inbox, restore.
        let inbox = handoff_root(&dir.0).join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("peer.capsule"), &out).unwrap();
        let restored = resume_from_inbox(&dir.0, key.raw_bytes());
        assert_eq!(restored.len(), 1, "the capsule restored");
        assert!(restored[0].parked, "restore PARKS — context, never permission");
        assert_eq!(restored[0].capsule.active_agent, "aegis", "context repopulated");
        let joined = format!("{:?}", restored[0].capsule.transcript_window);
        assert!(!joined.contains("SECRETTOKEN"), "no secret survived the handoff: {joined}");
    }

    #[test]
    fn a_tampered_or_wrong_key_inbox_capsule_is_skipped_never_restored() {
        let dir = tempdir("bad");
        // A capsule sealed under a DIFFERENT key lands in the inbox.
        let capsule = build_capsule("evil", "t", refs_with_a_secret());
        let sealed = seal_capsule(&[1u8; 32], &capsule).unwrap();
        let inbox = handoff_root(&dir.0).join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("evil.capsule"), &sealed).unwrap();

        let restored = resume_from_inbox(&dir.0, test_key().raw_bytes());
        assert!(restored.is_empty(), "a capsule we can't authenticate is NEVER restored");
    }
}
