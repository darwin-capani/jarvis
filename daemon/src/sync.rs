//! FEDERATED MEMORY SYNC (F18) — end-to-end-encrypted sync of the user's OWN
//! facts across their OWN devices. The crypto (sealed bundles) and the merge
//! (conflict-aware, never-clobber) are REAL and hermetically tested; the network
//! transport that moves a sealed bundle between devices is device-gated
//! (armed-but-inert), like shell.rs::run_sandboxed.
//!
//! THE HARD RULES (each pinned by a test):
//!   1. SHIPS OFF. `[sync].enabled` defaults false — sync moves the user's data
//!      off one device, a consequential act, so it is a deliberate opt-in like
//!      `[security]`/`[distill]`. Off, every entry point is a no-op.
//!   2. END-TO-END ENCRYPTED. A bundle NEVER leaves the box in the clear: facts
//!      are stored RAW (unredacted) and encryption is their ONLY off-device
//!      protection, so every bundle is sealed with AES-256-GCM under a shared
//!      key that lives ONLY in the Keychain (account `sync_shared_key`, paired
//!      between the user's own devices), never in config, never on the wire.
//!   3. NEVER SILENTLY CLOBBERS. The merge is additive + newest-wins with a
//!      DETERMINISTIC tie-break, and EVERY genuine divergence (same key, two
//!      different values) is recorded as a Conflict the user sees — a peer's
//!      value never overwrites yours without that being surfaced.
//!
//! SCOPE (stated, not hidden): syncs the live facts store (the natural
//! last-writer-wins-by-(key,ts) CRDT), EXCLUDING `meta.*` (internal bookkeeping
//! — syncing a peer's reflection clock or live missions would corrupt it). Fact
//! DELETIONS do NOT propagate in this version (the store keeps no tombstones);
//! the status says so rather than pretending a delete synced.

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The bundle wire-format version — bumped if the sealed shape changes so an
/// old peer's bundle is rejected honestly rather than mis-parsed.
const BUNDLE_VERSION: u32 = 1;
/// Cap on facts per bundle — a personal knowledge set, not a firehose.
const MAX_BUNDLE_FACTS: usize = 5000;

// ---------------------------------------------------------------------------
// AEAD sealed bundles (ring::aead AES-256-GCM) — the real crypto core
// ---------------------------------------------------------------------------

/// Seal `plaintext` under `key` (32 bytes) with AES-256-GCM: a fresh random
/// 96-bit nonce is generated and PREPENDED to the ciphertext+tag. The tag
/// authenticates the whole payload, so any tamper (or a wrong key) makes
/// [`open`] fail rather than return garbage. PURE-ish (only the CSPRNG); the
/// round-trip is hermetically tested.
pub fn seal(key: &[u8; 32], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let unbound = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| anyhow::anyhow!("bad AEAD key"))?;
    let sealing = LessSafeKey::new(unbound);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| anyhow::anyhow!("CSPRNG failure generating a sync nonce"))?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.to_vec();
    sealing
        .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| anyhow::anyhow!("sealing the sync bundle failed"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + in_out.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&in_out);
    Ok(out)
}

/// Open a sealed bundle: split off the prepended nonce, verify + decrypt. FAILS
/// (never returns garbage) on a wrong key, a truncated payload, or ANY tamper —
/// the GCM tag is checked before a byte is trusted.
pub fn open(key: &[u8; 32], sealed: &[u8]) -> anyhow::Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN {
        anyhow::bail!("sealed bundle is too short to contain a nonce");
    }
    let (nonce_bytes, ct) = sealed.split_at(NONCE_LEN);
    let nonce_arr: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("bad nonce length"))?;
    let nonce = Nonce::assume_unique_for_key(nonce_arr);
    let unbound = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| anyhow::anyhow!("bad AEAD key"))?;
    let opening = LessSafeKey::new(unbound);
    let mut in_out = ct.to_vec();
    let plaintext = opening
        .open_in_place(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| anyhow::anyhow!("sync bundle failed authentication (wrong key or tampered)"))?;
    Ok(plaintext.to_vec())
}

// ---------------------------------------------------------------------------
// The bundle + its facts
// ---------------------------------------------------------------------------

/// One syncable fact: the store's (key, value) plus its last-touch RFC3339 ts
/// (the merge's newest-wins ordering key). Never a `meta.*` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncFact {
    pub key: String,
    pub value: String,
    pub ts: String,
}

/// The plaintext (pre-seal) bundle a device exports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncBundle {
    pub version: u32,
    /// The exporting device's stable id (meta.device_id) — for conflict
    /// attribution and so a device can skip re-merging its own bundle.
    pub device_id: String,
    pub created: String,
    pub facts: Vec<SyncFact>,
}

/// Serialize a bundle to JSON bytes (the plaintext fed to [`seal`]).
pub fn serialize_bundle(bundle: &SyncBundle) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(bundle)?)
}

/// Parse a decrypted bundle, rejecting a wrong/old wire version honestly.
pub fn deserialize_bundle(bytes: &[u8]) -> anyhow::Result<SyncBundle> {
    let bundle: SyncBundle = serde_json::from_slice(bytes)?;
    if bundle.version != BUNDLE_VERSION {
        anyhow::bail!(
            "sync bundle version {} is not supported (this device speaks {BUNDLE_VERSION})",
            bundle.version
        );
    }
    Ok(bundle)
}

// ---------------------------------------------------------------------------
// The conflict-aware merge — PURE, never silently clobbers
// ---------------------------------------------------------------------------

/// A genuine divergence: the SAME key held DIFFERENT values on the two devices.
/// Recorded for every divergence regardless of who won, so a peer's value never
/// overwrites yours invisibly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub key: String,
    pub local_value: String,
    pub remote_value: String,
    pub local_ts: String,
    pub remote_ts: String,
    /// "local" (we kept ours) or "remote" (we take theirs). Deterministic.
    /// Owned so the record survives a seal -> disk -> open round-trip.
    pub winner: String,
}

/// The result of planning a merge. `apply` are the remote facts to upsert;
/// counts + `conflicts` are the honest report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    /// Remote facts to write locally (new keys + conflicts the remote won).
    pub apply: Vec<SyncFact>,
    /// Every genuine divergence, winner attributed.
    pub conflicts: Vec<Conflict>,
    /// New keys the local device didn't have.
    pub added: usize,
    /// Keys present on both with the SAME value (no-op).
    pub unchanged: usize,
    /// `meta.*` facts in the remote bundle, refused defensively.
    pub skipped_meta: usize,
}

/// Plan the merge of a remote bundle into the local facts. PURE and
/// exhaustively tested. Rules:
///   * a `meta.*` remote key is REFUSED (bookkeeping never syncs) — even though
///     export excludes it, import enforces it too (defense in depth);
///   * a key the local device lacks is ADDED;
///   * the same key with an EQUAL value is unchanged (equal values are never a
///     conflict, so a redundant re-touch on one device can't win on ts alone);
///   * the same key with a DIFFERENT value is a CONFLICT: newest RFC3339 ts
///     wins; an EXACT ts tie breaks deterministically by (device_id, then the
///     value bytes) so BOTH devices converge to the same winner — and it is
///     LOGGED regardless of winner. The remote value is applied only when the
///     remote won.
pub fn plan_merge(
    local: &[SyncFact],
    remote_bundle: &SyncBundle,
    local_device_id: &str,
) -> MergePlan {
    use std::collections::HashMap;
    let local_by_key: HashMap<&str, &SyncFact> =
        local.iter().map(|f| (f.key.as_str(), f)).collect();

    let mut plan = MergePlan {
        apply: Vec::new(),
        conflicts: Vec::new(),
        added: 0,
        unchanged: 0,
        skipped_meta: 0,
    };
    let remote_device = remote_bundle.device_id.as_str();

    for rf in &remote_bundle.facts {
        if crate::memory::is_reserved_key(&rf.key) {
            plan.skipped_meta += 1;
            continue;
        }
        match local_by_key.get(rf.key.as_str()) {
            None => {
                plan.added += 1;
                plan.apply.push(rf.clone());
            }
            Some(lf) if lf.value == rf.value => {
                plan.unchanged += 1;
            }
            Some(lf) => {
                let remote_wins = remote_beats_local(
                    &rf.ts,
                    remote_device,
                    &rf.value,
                    &lf.ts,
                    local_device_id,
                    &lf.value,
                );
                plan.conflicts.push(Conflict {
                    key: rf.key.clone(),
                    local_value: lf.value.clone(),
                    remote_value: rf.value.clone(),
                    local_ts: lf.ts.clone(),
                    remote_ts: rf.ts.clone(),
                    winner: if remote_wins { "remote".into() } else { "local".into() },
                });
                if remote_wins {
                    plan.apply.push(rf.clone());
                }
            }
        }
    }
    plan
}

/// Deterministic winner for a divergence: newer ts wins; on an exact ts tie,
/// the larger (device_id, value) tuple wins — a total order both devices
/// compute identically, so they converge to the same bytes.
fn remote_beats_local(
    remote_ts: &str,
    remote_device: &str,
    remote_value: &str,
    local_ts: &str,
    local_device: &str,
    local_value: &str,
) -> bool {
    use std::cmp::Ordering;
    match remote_ts.cmp(local_ts) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => {
            (remote_device, remote_value) > (local_device, local_value)
        }
    }
}

// ---------------------------------------------------------------------------
// Thin async wrappers over the store (all policy is in the pure fns)
// ---------------------------------------------------------------------------

/// Read-or-mint this device's stable id (meta.device_id). Minted via
/// `upsert_fact` (the trusted path — a model can't forge a reserved key), so it
/// persists in darwin.db and is AUTOMATICALLY excluded from every sync bundle
/// (meta.* is never synced). 128 bits of CSPRNG hex.
pub async fn device_id(memory: &crate::memory::Memory) -> String {
    if let Ok(Some(id)) = memory.get_fact("meta.device_id").await {
        if !id.trim().is_empty() {
            return id;
        }
    }
    let id = match crate::crypto::SecretKey::generate() {
        Ok(k) => k.keychain_value()[..32].to_string(), // 128-bit hex slice
        Err(_) => "unknown-device".to_string(),
    };
    let _ = memory.upsert_fact("meta.device_id", &id).await;
    id
}

/// The syncable facts: every non-`meta.*` fact as (key, value, ts), bounded.
/// A failed read degrades to empty (an honest "nothing to sync"), never errors.
pub async fn syncable_facts(memory: &crate::memory::Memory) -> Vec<SyncFact> {
    memory
        .syncable_facts(MAX_BUNDLE_FACTS)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|(key, value, ts)| SyncFact { key, value, ts })
        .collect()
}

/// Apply a planned merge: upsert each winning remote fact (defensively skipping
/// any meta.* that slipped through). Returns how many were written.
///
/// The remote fact's ORIGINAL ts is preserved (upsert_fact_at) — ts is the
/// newest-wins ordering key, so re-stamping "now" here would let a re-imported
/// STALE bundle masquerade as fresh and beat a genuinely newer edit on the peer
/// at the next exchange. Only a real local edit may touch a fact's ts forward.
pub async fn apply_plan(memory: &crate::memory::Memory, plan: &MergePlan) -> usize {
    let mut applied = 0;
    for f in &plan.apply {
        if crate::memory::is_reserved_key(&f.key) {
            continue;
        }
        if memory.upsert_fact_at(&f.key, &f.value, &f.ts).await.is_ok() {
            applied += 1;
        }
    }
    applied
}

// ---------------------------------------------------------------------------
// The device-gated transport — BUILT here, sent only on-device behind the gate
// ---------------------------------------------------------------------------

/// The sync state tree under the daemon-owned, gitignored `state/`.
pub fn sync_root(root: &std::path::Path) -> std::path::PathBuf {
    root.join("state").join("sync")
}

/// POST a SEALED bundle to the user's OWN paired device. BUILT-BUT-INERT: it is
/// reached ONLY from [`sync_now`] when `[sync].enabled` AND a non-empty
/// `peer_endpoint` are set, and never in any test (no live peer in CI) — the
/// shell.rs::run_sandboxed discipline. It moves ONLY ciphertext (the bundle is
/// already AES-256-GCM sealed), to a fixed configured endpoint, with bounded
/// timeouts. It never fabricates success.
async fn transport_push(endpoint: &str, sealed: &[u8]) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()?;
    let resp = client
        .post(endpoint)
        .header("content-type", "application/octet-stream")
        .body(sealed.to_vec())
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("peer returned {}", resp.status());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The operator-triggered orchestrator
// ---------------------------------------------------------------------------

/// Run one operator-triggered sync (authenticated command channel; never a
/// background cadence). EXPORT: seal the local facts to `state/sync/outbox`.
/// IMPORT: open + merge + apply every sealed bundle a paired device left in
/// `state/sync/inbox`, logging conflicts. The network delivery between devices'
/// outbox/inbox is the armed-but-inert transport (fired only with a configured
/// peer). `key` is the shared Keychain key, injected so this is hermetically
/// tested; a missing key reports "not paired", never a plaintext export.
pub async fn sync_now(
    cfg: &crate::config::Config,
    memory: &crate::memory::Memory,
    root: &std::path::Path,
    now_rfc3339: String,
    key: Option<crate::crypto::SecretKey>,
) -> String {
    if !cfg.sync.enabled {
        return "Federated sync is off, sir — turn on [sync].enabled to sync your facts across your own devices.".to_string();
    }
    let Some(key) = key else {
        return "No shared sync key, sir — pair a device (its key lives in the Keychain as sync_shared_key). Nothing is ever exported in the clear.".to_string();
    };
    let key_bytes = *key.raw_bytes();
    let device = device_id(memory).await;

    // EXPORT: seal the local facts to the outbox.
    let facts = syncable_facts(memory).await;
    let bundle = SyncBundle { version: BUNDLE_VERSION, device_id: device.clone(), created: now_rfc3339, facts };
    let outbox = sync_root(root).join("outbox");
    let mut export_note = format!("Sealed {} facts", bundle.facts.len());
    match serialize_bundle(&bundle).and_then(|pt| seal(&key_bytes, &pt)) {
        Ok(sealed) => {
            let _ = std::fs::create_dir_all(&outbox);
            if std::fs::write(outbox.join(format!("{device}.bundle")), &sealed).is_err() {
                export_note = "Couldn't stage the sealed bundle".to_string();
            } else if !cfg.sync.peer_endpoint.trim().is_empty() {
                // Armed-but-inert transport: only reached with a configured peer.
                match transport_push(&cfg.sync.peer_endpoint, &sealed).await {
                    Ok(()) => export_note.push_str(" and delivered to the paired device"),
                    Err(e) => export_note.push_str(&format!(" (staged; delivery to the peer failed: {e})")),
                }
            } else {
                export_note.push_str(" to the outbox (transport to a paired device is armed but inert)");
            }
        }
        Err(e) => export_note = format!("Couldn't seal the bundle: {e}"),
    }

    // IMPORT: open + merge every bundle a paired device left in the inbox.
    let (merged, conflicts) = import_inbox(memory, root, &key_bytes, &device).await;
    let import_note = if merged == 0 && conflicts == 0 {
        "no incoming bundle to merge".to_string()
    } else {
        format!(
            "merged {merged} fact{} from a paired device{}",
            if merged == 1 { "" } else { "s" },
            if conflicts > 0 {
                format!(", {conflicts} conflict{} logged for you to resolve", if conflicts == 1 { "" } else { "s" })
            } else {
                String::new()
            }
        )
    };
    format!("{export_note}; {import_note}, sir.")
}

/// Cap on the retained sealed conflict log — a bounded review queue, not an
/// archive. Newest divergences are kept.
const CONFLICT_LOG_CAP: usize = 200;

/// Open + merge + apply every sealed bundle in `state/sync/inbox` (excluding
/// this device's own). Returns (facts_applied, conflicts_this_run). A bundle
/// that fails to open (wrong key / tampered / bad version) is SKIPPED honestly,
/// never applied.
///
/// Multi-bundle correctness: the local view is refreshed AS EACH BUNDLE APPLIES
/// (a working map folded forward), and bundles are processed in a stable sorted
/// order — so two bundles touching the same key are merged newest-wins against
/// each other (never both blindly "added"), and the outcome is deterministic
/// regardless of directory-read order.
async fn import_inbox(
    memory: &crate::memory::Memory,
    root: &std::path::Path,
    key_bytes: &[u8; 32],
    self_device: &str,
) -> (usize, usize) {
    use std::collections::HashMap;
    let inbox = sync_root(root).join("inbox");
    let Ok(read) = std::fs::read_dir(&inbox) else {
        return (0, 0);
    };
    // Stable order so the merge is deterministic across devices.
    let mut paths: Vec<std::path::PathBuf> = read.flatten().map(|e| e.path()).collect();
    paths.sort();

    // A working view of the local facts, folded forward as each bundle applies —
    // so the NEXT bundle plans against what the PREVIOUS one already merged.
    let mut view: HashMap<String, SyncFact> =
        syncable_facts(memory).await.into_iter().map(|f| (f.key.clone(), f)).collect();

    let mut applied = 0;
    let mut conflicts: Vec<Conflict> = Vec::new();
    for path in paths {
        let Ok(sealed) = std::fs::read(&path) else { continue };
        let bundle = match open(key_bytes, &sealed).and_then(|pt| deserialize_bundle(&pt)) {
            Ok(b) => b,
            Err(_) => continue, // wrong key / tampered / bad version -> skip, never apply
        };
        if bundle.device_id == self_device {
            continue; // never re-merge our own export
        }
        let local: Vec<SyncFact> = view.values().cloned().collect();
        let plan = plan_merge(&local, &bundle, self_device);
        applied += apply_plan(memory, &plan).await;
        // Fold the applied winners into the working view for the next bundle.
        for f in &plan.apply {
            view.insert(f.key.clone(), f.clone());
        }
        conflicts.extend(plan.conflicts);
    }

    // Persist the divergences to a SEALED, APPENDED, bounded conflict log — the
    // loser value is the user's own fact, so it never sits in plaintext (matches
    // the never-plaintext-off-device posture) and an earlier unreviewed conflict
    // is never overwritten away.
    let run_conflicts = conflicts.len();
    if !conflicts.is_empty() {
        append_sealed_conflicts(root, key_bytes, conflicts);
    }
    (applied, run_conflicts)
}

/// The sealed conflict-log path.
fn conflict_log_path(root: &std::path::Path) -> std::path::PathBuf {
    sync_root(root).join("conflicts.sealed")
}

/// Append `new` divergences to the sealed conflict log: open the existing log
/// (if any), extend, dedup by (key, local_value, remote_value) keeping the
/// latest, cap newest-first, and re-seal. The values never touch disk in the
/// clear. Best-effort; a failed read starts a fresh log rather than losing the
/// new entries.
fn append_sealed_conflicts(root: &std::path::Path, key_bytes: &[u8; 32], new: Vec<Conflict>) {
    let path = conflict_log_path(root);
    let mut log: Vec<Conflict> = std::fs::read(&path)
        .ok()
        .and_then(|sealed| open(key_bytes, &sealed).ok())
        .and_then(|pt| serde_json::from_slice::<Vec<Conflict>>(&pt).ok())
        .unwrap_or_default();
    // New entries first (newest), then prior; dedup by identity keeps the new.
    let mut merged = new;
    merged.append(&mut log);
    let mut seen = std::collections::HashSet::new();
    merged.retain(|c| seen.insert((c.key.clone(), c.local_value.clone(), c.remote_value.clone())));
    merged.truncate(CONFLICT_LOG_CAP);
    if let Ok(pt) = serde_json::to_vec(&merged) {
        if let Ok(sealed) = seal(key_bytes, &pt) {
            let _ = std::fs::create_dir_all(sync_root(root));
            let _ = std::fs::write(&path, sealed);
        }
    }
}

/// Count the divergences pending review in the sealed conflict log. Needs the
/// shared key to open it; without the key (unpaired) it reports 0.
fn count_conflicts(root: &std::path::Path, key: Option<&crate::crypto::SecretKey>) -> usize {
    let Some(key) = key else { return 0 };
    std::fs::read(conflict_log_path(root))
        .ok()
        .and_then(|sealed| open(key.raw_bytes(), &sealed).ok())
        .and_then(|pt| serde_json::from_slice::<Vec<Conflict>>(&pt).ok())
        .map(|v| v.len())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The honest status surface
// ---------------------------------------------------------------------------

/// The `sync.status` wire payload. PURE + total. SECRET-FREE: booleans, a
/// syncable-fact COUNT, and a pending-conflict count — never a fact value, never
/// the key, never the peer's data.
pub fn status_payload(
    enabled: bool,
    key_present: bool,
    peer_configured: bool,
    syncable_facts: usize,
    pending_conflicts: usize,
) -> Value {
    json!({
        "enabled": enabled,
        // The daemon CAN confirm the key + peer config, but NOT that the peer is
        // a genuine paired device reachable now — so "ready" means locally armed,
        // and the transport remains the on-device-verified leg.
        "key_present": key_present,
        "peer_configured": peer_configured,
        "transport_inert": true,
        "syncable_facts": syncable_facts,
        "pending_conflicts": pending_conflicts,
        // Honest scope limit: deletions don't propagate (no tombstones).
        "deletes_propagate": false,
    })
}

/// Emit `sync.status` for the HUD on the audit-snapshot cadence. READ-ONLY:
/// counts facts + probes the key/peer/conflicts; runs no sync. Fail-open.
///
/// OFF (the shipped default) emits the honest off payload WITHOUT touching the
/// Keychain: resolve_secret spawns a real security(1) subprocess (up to a 5s
/// timeout), and this fn rides the shared 15s snapshot cadence — an ungated
/// probe would spawn ~5760 subprocesses/day on every install and a hung login
/// keychain would stall every downstream emitter on the tick. Only an armed
/// [sync] pays that (bounded) probe; opening the sealed conflict log needs the
/// key anyway, so it is resolved once per emit and reused.
pub async fn emit_status(cfg: &crate::config::Config, memory: &crate::memory::Memory, root: &std::path::Path) {
    if !cfg.sync.enabled {
        crate::telemetry::emit("system", "sync.status", status_payload(false, false, false, 0, 0));
        return;
    }
    let syncable = syncable_facts(memory).await.len();
    let key = crate::integrations::resolve_secret("sync_shared_key")
        .await
        .and_then(|hex| crate::crypto::SecretKey::from_hex(hex.trim()).ok());
    crate::telemetry::emit(
        "system",
        "sync.status",
        status_payload(
            true,
            key.is_some(),
            !cfg.sync.peer_endpoint.trim().is_empty(),
            syncable,
            count_conflicts(root, key.as_ref()),
        ),
    );
}

// ---------------------------------------------------------------------------
// Tests — crypto round-trip + merge exhaustively; orchestrator hermetically
// (TempDb + tempdir + an injected test key; the network transport never runs).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> crate::crypto::SecretKey {
        crate::crypto::SecretKey::from_bytes([7u8; 32])
    }

    fn fact(key: &str, value: &str, ts: &str) -> SyncFact {
        SyncFact { key: key.into(), value: value.into(), ts: ts.into() }
    }

    fn bundle(device: &str, facts: Vec<SyncFact>) -> SyncBundle {
        SyncBundle { version: BUNDLE_VERSION, device_id: device.into(), created: "2026-07-13T10:00:00Z".into(), facts }
    }

    // -- AEAD ------------------------------------------------------------------

    #[test]
    fn seal_open_round_trips_and_rejects_tamper_and_wrong_key() {
        let key = *test_key().raw_bytes();
        let msg = b"user.name = Darwin Capani; a secret fact value";
        let sealed = seal(&key, msg).unwrap();
        assert_ne!(&sealed[NONCE_LEN..], msg, "ciphertext is not the plaintext");
        assert!(!sealed.windows(6).any(|w| w == b"Darwin"), "plaintext never on the wire");
        assert_eq!(open(&key, &sealed).unwrap(), msg, "round-trips");

        // A single flipped byte fails authentication.
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert!(open(&key, &tampered).is_err(), "tamper is caught");

        // The wrong key fails.
        let wrong = [9u8; 32];
        assert!(open(&wrong, &sealed).is_err(), "wrong key fails");

        // A truncated payload fails.
        assert!(open(&key, &sealed[..3]).is_err());
    }

    #[test]
    fn bundle_serialize_round_trips_and_rejects_a_wrong_version() {
        let b = bundle("dev-a", vec![fact("user.name", "Darwin", "2026-07-13T10:00:00Z")]);
        let bytes = serialize_bundle(&b).unwrap();
        assert_eq!(deserialize_bundle(&bytes).unwrap(), b);

        // A future/old version is refused, not mis-parsed.
        let mut bad = b.clone();
        bad.version = 999;
        let bytes = serialize_bundle(&bad).unwrap();
        assert!(deserialize_bundle(&bytes).is_err());
    }

    // -- merge -----------------------------------------------------------------

    #[test]
    fn merge_adds_new_keys_leaves_equal_and_excludes_meta() {
        let local = vec![fact("user.name", "Darwin", "2026-07-13T09:00:00Z")];
        let remote = bundle(
            "dev-b",
            vec![
                fact("user.name", "Darwin", "2026-07-13T10:00:00Z"), // equal value -> unchanged
                fact("user.city", "London", "2026-07-13T10:00:00Z"), // new -> added
                fact("meta.device_id", "dev-b", "2026-07-13T10:00:00Z"), // reserved -> skipped
            ],
        );
        let plan = plan_merge(&local, &remote, "dev-a");
        assert_eq!(plan.added, 1);
        assert_eq!(plan.unchanged, 1, "equal value never a conflict even with newer ts");
        assert_eq!(plan.skipped_meta, 1);
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.apply, vec![fact("user.city", "London", "2026-07-13T10:00:00Z")]);
    }

    #[test]
    fn a_divergence_is_always_logged_newest_wins_and_a_tie_is_deterministic() {
        // Remote is newer -> remote wins, but the conflict is STILL logged.
        let local = vec![fact("user.mood", "tired", "2026-07-13T09:00:00Z")];
        let remote = bundle("dev-b", vec![fact("user.mood", "great", "2026-07-13T11:00:00Z")]);
        let plan = plan_merge(&local, &remote, "dev-a");
        assert_eq!(plan.conflicts.len(), 1, "divergence logged");
        assert_eq!(plan.conflicts[0].winner, "remote");
        assert_eq!(plan.conflicts[0].local_value, "tired");
        assert_eq!(plan.conflicts[0].remote_value, "great");
        assert_eq!(plan.apply, vec![fact("user.mood", "great", "2026-07-13T11:00:00Z")]);

        // Local is newer -> local wins, remote NOT applied, but STILL logged
        // (a peer value never overwrites yours silently).
        let local = vec![fact("user.mood", "tired", "2026-07-13T12:00:00Z")];
        let plan = plan_merge(&local, &remote, "dev-a");
        assert_eq!(plan.conflicts[0].winner, "local");
        assert!(plan.apply.is_empty(), "we keep ours; nothing applied");

        // Exact ts tie -> deterministic by (device_id, value); both devices agree.
        let local = vec![fact("k", "aaa", "2026-07-13T10:00:00Z")];
        let remote = bundle("dev-z", vec![fact("k", "bbb", "2026-07-13T10:00:00Z")]);
        let plan = plan_merge(&local, &remote, "dev-a");
        // ("dev-z","bbb") > ("dev-a","aaa") -> remote wins deterministically.
        assert_eq!(plan.conflicts[0].winner, "remote");
        // Same inputs from the OTHER device's view converge to the same winner.
        let plan_other = plan_merge(
            &[fact("k", "bbb", "2026-07-13T10:00:00Z")],
            &bundle("dev-a", vec![fact("k", "aaa", "2026-07-13T10:00:00Z")]),
            "dev-z",
        );
        assert_eq!(plan_other.conflicts[0].winner, "local", "the other device keeps its bbb — both converge");
    }

    #[test]
    fn bundle_is_bounded() {
        let facts: Vec<SyncFact> =
            (0..(MAX_BUNDLE_FACTS + 10)).map(|i| fact(&format!("k{i}"), "v", "2026-07-13T10:00:00Z")).collect();
        let b = bundle("dev", facts);
        // plan_merge itself doesn't cap (the reader does); assert the reader cap
        // constant is what bounds the export.
        assert_eq!(MAX_BUNDLE_FACTS, 5000);
        let plan = plan_merge(&[], &b, "self");
        assert_eq!(plan.added, MAX_BUNDLE_FACTS + 10, "merge trusts its bounded input");
    }

    // -- status ----------------------------------------------------------------

    #[test]
    fn status_is_honest_about_off_key_transport_and_deletes() {
        let off = status_payload(false, false, false, 0, 0);
        assert_eq!(off["enabled"], false);
        assert_eq!(off["transport_inert"], true);
        assert_eq!(off["deletes_propagate"], false, "honest: deletions don't sync");

        let armed = status_payload(true, true, true, 120, 3);
        assert_eq!(armed["key_present"], true);
        assert_eq!(armed["peer_configured"], true);
        assert_eq!(armed["syncable_facts"], 120);
        assert_eq!(armed["pending_conflicts"], 3);
        // No secret ever on the wire.
        assert!(!armed.to_string().contains("Darwin"));
    }

    // -- orchestrator (hermetic: TempDb + tempdir + injected key) --------------

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("darwin-sync-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    async fn mem(dir: &std::path::Path) -> crate::memory::Memory {
        crate::memory::Memory::open(&dir.join("m.db")).unwrap()
    }

    #[tokio::test]
    async fn sync_now_is_off_and_keyless_safe_and_never_exports_in_the_clear() {
        let dir = tempdir("off");
        let m = mem(&dir.0).await;
        m.upsert_fact("user.name", "Darwin").await.unwrap();

        // Off: no-op.
        let cfg = crate::config::Config::default();
        let r = sync_now(&cfg, &m, &dir.0, "2026-07-13T10:00:00Z".into(), Some(test_key())).await;
        assert!(r.contains("off"), "{r}");
        assert!(!sync_root(&dir.0).exists(), "off never touches disk");

        // On but no key: refuses, and NOTHING is written in the clear.
        let mut cfg2 = crate::config::Config::default();
        cfg2.sync.enabled = true;
        let r = sync_now(&cfg2, &m, &dir.0, "2026-07-13T10:00:00Z".into(), None).await;
        assert!(r.contains("No shared sync key"), "{r}");
        assert!(!sync_root(&dir.0).exists(), "no plaintext export without a key");
    }

    #[tokio::test]
    async fn end_to_end_seal_export_then_merge_from_a_peer_inbox() {
        let dir = tempdir("e2e");
        let m = mem(&dir.0).await;
        m.upsert_fact("user.name", "Darwin").await.unwrap();
        m.upsert_fact("meta.last_reflection", "clock").await.unwrap(); // never syncs
        let mut cfg = crate::config::Config::default();
        cfg.sync.enabled = true;
        let key = test_key();

        // A PEER device seals a bundle and drops it in our inbox.
        let peer = bundle(
            "peer-device",
            vec![
                fact("user.city", "London", "2026-07-13T11:00:00Z"), // new
                // Divergence, peer deterministically newer (far future beats the
                // local upsert's real-wall-clock ts).
                fact("user.name", "Darwin C.", "2099-01-01T00:00:00Z"),
                fact("meta.forge_pending", "x", "2099-01-01T00:00:00Z"), // reserved -> refused
            ],
        );
        let sealed = seal(test_key().raw_bytes(), &serialize_bundle(&peer).unwrap()).unwrap();
        let inbox = sync_root(&dir.0).join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("peer.bundle"), &sealed).unwrap();

        let r = sync_now(&cfg, &m, &dir.0, "2026-07-13T12:00:00Z".into(), Some(key)).await;

        // Export: our facts sealed to the outbox, transport reported inert.
        assert!(r.contains("armed but inert"), "transport honesty: {r}");
        let out = std::fs::read(sync_root(&dir.0).join("outbox").join(format!("{}.bundle", device_id(&m).await))).unwrap();
        assert!(!out.windows(6).any(|w| w == b"Darwin"), "export is sealed, no plaintext on disk");

        // Import: the new fact merged; the divergence applied (peer newer) + logged;
        // the peer's meta.* refused; our meta.last_reflection untouched.
        assert_eq!(m.get_fact("user.city").await.unwrap().as_deref(), Some("London"));
        assert_eq!(m.get_fact("user.name").await.unwrap().as_deref(), Some("Darwin C."));
        assert!(m.get_fact("meta.forge_pending").await.unwrap().is_none(), "peer meta.* refused");
        assert_eq!(m.get_fact("meta.last_reflection").await.unwrap().as_deref(), Some("clock"));
        assert!(r.contains("conflict"), "the divergence is surfaced: {r}");
        // The conflict was persisted for review — SEALED, never plaintext on disk.
        let log = conflict_log_path(&dir.0);
        assert!(log.exists(), "conflict log persisted");
        let raw = std::fs::read(&log).unwrap();
        assert!(!raw.windows(6).any(|w| w == b"Darwin"), "conflict values are sealed, not plaintext");
        // Opened with the key, the divergence is there for review; the count reads it.
        assert_eq!(count_conflicts(&dir.0, Some(&test_key())), 1);
        let opened: Vec<Conflict> =
            serde_json::from_slice(&open(test_key().raw_bytes(), &raw).unwrap()).unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].key, "user.name");
        assert_eq!(opened[0].local_value, "Darwin");
        assert_eq!(opened[0].remote_value, "Darwin C.");
    }

    #[tokio::test]
    async fn multiple_inbox_bundles_touching_one_key_merge_newest_wins_no_silent_clobber() {
        // Two peers each carry the SAME key the local device lacks. Before the
        // fix, both planned "added" against a stale snapshot and the last one
        // written by read_dir order silently won. Now the second bundle plans
        // against the first's applied value: newest-ts wins deterministically and
        // the divergence is logged.
        let dir = tempdir("multi");
        let m = mem(&dir.0).await;
        let mut cfg = crate::config::Config::default();
        cfg.sync.enabled = true;
        let key = test_key();
        let inbox = sync_root(&dir.0).join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();

        // Older value in the alphabetically-LATER filename, newer in the earlier —
        // so filesystem order alone would pick the older; the merge must not.
        let older = bundle("peer-b", vec![fact("user.city", "Paris", "2026-07-13T09:00:00Z")]);
        let newer = bundle("peer-a", vec![fact("user.city", "London", "2026-07-13T11:00:00Z")]);
        std::fs::write(inbox.join("z-older.bundle"), seal(key.raw_bytes(), &serialize_bundle(&older).unwrap()).unwrap()).unwrap();
        std::fs::write(inbox.join("a-newer.bundle"), seal(key.raw_bytes(), &serialize_bundle(&newer).unwrap()).unwrap()).unwrap();

        sync_now(&cfg, &m, &dir.0, "2026-07-13T12:00:00Z".into(), Some(test_key())).await;

        // Newest-ts value wins regardless of directory-read order; the older peer's
        // divergence is logged, never silently dropped.
        assert_eq!(m.get_fact("user.city").await.unwrap().as_deref(), Some("London"));
        assert_eq!(count_conflicts(&dir.0, Some(&test_key())), 1);
    }

    #[tokio::test]
    async fn the_sealed_conflict_log_appends_and_dedups_across_runs() {
        // An earlier unreviewed conflict is never overwritten away by a later run
        // (the plaintext-per-device file used to truncate it), and re-importing the
        // identical divergence doesn't double-count.
        let dir = tempdir("append");
        let key = test_key();
        let c1 = Conflict {
            key: "user.name".into(),
            local_value: "A".into(),
            remote_value: "B".into(),
            local_ts: "2026-07-13T09:00:00Z".into(),
            remote_ts: "2026-07-13T10:00:00Z".into(),
            winner: "remote".into(),
        };
        let c2 = Conflict { key: "user.city".into(), local_value: "X".into(), remote_value: "Y".into(), ..c1.clone() };

        append_sealed_conflicts(&dir.0, key.raw_bytes(), vec![c1.clone()]);
        append_sealed_conflicts(&dir.0, key.raw_bytes(), vec![c2.clone()]);
        assert_eq!(count_conflicts(&dir.0, Some(&test_key())), 2, "second run keeps the first's conflict");
        append_sealed_conflicts(&dir.0, key.raw_bytes(), vec![c1.clone()]);
        assert_eq!(count_conflicts(&dir.0, Some(&test_key())), 2, "identical divergence deduped");
        // No key -> can't read the sealed log -> honest 0, never a plaintext peek.
        assert_eq!(count_conflicts(&dir.0, None), 0);
    }

    #[tokio::test]
    async fn apply_preserves_remote_ts_so_a_stale_reimport_never_beats_a_newer_local_edit() {
        // Regression for the post-merge audit finding: apply_plan used to write
        // winners via upsert_fact, which re-stamped ts = now — so a re-imported
        // OLD bundle masqueraded as fresh and could durably overwrite a newer
        // edit on the peer. The applied fact must keep the REMOTE ts verbatim.
        let dir = tempdir("stale");
        let m = mem(&dir.0).await;

        // Run 1: an old peer bundle introduces the fact.
        let old = bundle("peer", vec![fact("user.city", "London", "2020-01-01T00:00:00Z")]);
        let plan = plan_merge(&syncable_facts(&m).await, &old, "self");
        apply_plan(&m, &plan).await;
        let stored = syncable_facts(&m).await;
        let city = stored.iter().find(|f| f.key == "user.city").expect("applied");
        assert_eq!(city.ts, "2020-01-01T00:00:00Z", "the remote ts is preserved verbatim, never re-stamped now");

        // The user then edits locally — a genuine touch-now, newer than the bundle.
        m.upsert_fact("user.city", "Paris").await.unwrap();

        // Run 2: the SAME old bundle is re-imported. The newer local edit must
        // win, the stale value must not be re-applied, and the divergence is
        // logged with the honest winner.
        let plan2 = plan_merge(&syncable_facts(&m).await, &old, "self");
        apply_plan(&m, &plan2).await;
        assert_eq!(m.get_fact("user.city").await.unwrap().as_deref(), Some("Paris"), "stale re-import never clobbers the newer edit");
        assert_eq!(plan2.conflicts.len(), 1);
        assert_eq!(plan2.conflicts[0].winner, "local");
    }

    #[tokio::test]
    async fn a_tampered_or_wrong_key_inbox_bundle_is_skipped_never_applied() {
        let dir = tempdir("bad");
        let m = mem(&dir.0).await;
        let mut cfg = crate::config::Config::default();
        cfg.sync.enabled = true;

        // A bundle sealed under a DIFFERENT key lands in the inbox.
        let peer = bundle("evil", vec![fact("user.name", "Injected", "2026-07-13T11:00:00Z")]);
        let sealed = seal(&[1u8; 32], &serialize_bundle(&peer).unwrap()).unwrap();
        let inbox = sync_root(&dir.0).join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("evil.bundle"), &sealed).unwrap();

        let _ = sync_now(&cfg, &m, &dir.0, "2026-07-13T12:00:00Z".into(), Some(test_key())).await;
        assert!(m.get_fact("user.name").await.unwrap().is_none(), "a bundle we can't authenticate is NEVER applied");
    }
}
