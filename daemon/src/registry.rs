//! Attested Registry + SBOM — verify plugin bytes ON YOUR OWN MACHINE.
//!
//! "Trust the publisher" becomes "verify the bytes on my own machine". A plugin
//! is ADMITTED to DARWIN's signed LOCAL index ONLY when BOTH gates clear:
//!
//!   (a) a re-derived build in the STAGING sandbox produces an artifact hash +
//!       a dependency-closure (SBOM) hash that MATCH the plugin's attestation
//!       (the reproducible-build check — the publisher's declared bytes are the
//!       bytes WE built), AND
//!   (b) an allowlisted ed25519 signature over the attestation VERIFIES (the
//!       attestation was signed by a key the OWNER trusts, unmodified).
//!
//! This is a VERIFICATION GATE, never a new install authority. Exactly like
//! forge.rs's propose-only contract, admission to this local index does NOT
//! install anything: the actual install stays the HUMAN-GATED step
//! (scripts/apply_forge.sh / the owner's explicit deploy). Verification is
//! NECESSARY but NEVER SUFFICIENT — see [`install_decision`], which installs
//! ONLY when the registry admitted the plugin AND the human approved.
//!
//! FAIL-CLOSED by construction. Every check refuses on any doubt:
//!   - a non-allowlisted signer key id            -> Refused;
//!   - an invalid/tampered signature              -> Refused;
//!   - a wrong key (allowlisted id, wrong bytes)  -> Refused;
//!   - a rebuild artifact/closure hash mismatch   -> Refused;
//!   - a missing rebuild while one is required    -> Refused.
//!
//! Ships ARMED for VERIFY ([registry].verify default true) but INERT UNTIL THE
//! OWNER ADDS A TRUSTED SIGNER: the allowlist ships EMPTY, so with no allowlisted
//! key NO attestation can verify and admission refuses everything (a safe,
//! inert default, not a bypass).
//!
//! SEAM SHAPE (mirrors forge.rs / introspect.rs): the SBOM build, the attestation
//! compare (rebuilt hash vs attested), and the ed25519 verify are a PURE,
//! unit-tested seam ([`build_sbom`] / [`Sbom::closure_hash`] / [`hash_artifact`]
//! / [`attestation_matches`] / [`verify_signature`] / [`decide_admission`]). The
//! STAGING REBUILD is the device-gated runner ([`CargoRebuilder`], behind the
//! [`StagingRebuilder`] trait so tests drive the flow with a mock — no real
//! build under `cargo test`). ed25519 verification reuses `ring` — the vetted
//! crate already in the tree (ring::aead is DARWIN's AES-256-GCM core). STANDALONE:
//! this module depends on NO envlock/substrate-lock closure (that coupling is a
//! follow-on).

#![allow(dead_code)] // Complete + unit-tested VERIFICATION SEAM. Its LIVE call
// site is the human-gated install (apps.rs / forge's apply_forge.sh), wired at
// integration — mirrors introspect.rs's ES seam (tested now; front-end deferred).
// `announce_status` IS live (a startup registry.status frame). No item here adds
// install authority, so nothing is silently reachable before integration review.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ring::signature::{UnparsedPublicKey, ED25519};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::telemetry;

/// A raw ed25519 public key is exactly 32 bytes; a signature is exactly 64.
const ED25519_PUBKEY_LEN: usize = 32;

/// Field separator for canonical (signed/hashed) byte strings — a control byte
/// that cannot appear in a hostname, hex digest, or plugin id, so the canonical
/// encoding is unambiguous (no field can be smuggled across a boundary).
const SEP: char = '\u{1f}';

/// Staging rebuild deadline (mirrors forge's VALIDATE_TIMEOUT).
const REBUILD_TIMEOUT: Duration = Duration::from_secs(600);

// ===========================================================================
// (1) SBOM build — PURE, unit-tested. A stable dependency-closure hash.
// ===========================================================================

/// One declared dependency in a plugin's closure: its name, version, and the
/// hash (checksum) of that dependency's own bytes. The triple is what makes the
/// closure hash tamper-evident — a swapped version or checksum changes the hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclaredDep {
    pub name: String,
    pub version: String,
    pub hash: String,
}

/// A plugin's Software Bill of Materials: its canonicalized dependency closure.
/// Canonical = trimmed, empty-name entries dropped, sorted, de-duplicated — so
/// [`closure_hash`](Sbom::closure_hash) is STABLE regardless of declaration
/// order or duplicate listings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sbom {
    pub entries: Vec<DeclaredDep>,
}

/// Build a canonical SBOM from a declared dependency set. PURE and STABLE: the
/// same set in ANY order (with duplicates or surrounding whitespace) yields the
/// same canonical `entries` and therefore the same closure hash.
pub fn build_sbom(deps: &[DeclaredDep]) -> Sbom {
    let mut entries: Vec<DeclaredDep> = deps
        .iter()
        .map(|d| DeclaredDep {
            name: d.name.trim().to_string(),
            version: d.version.trim().to_string(),
            hash: d.hash.trim().to_string(),
        })
        .filter(|d| !d.name.is_empty())
        .collect();
    entries.sort_by(|a, b| {
        (a.name.as_str(), a.version.as_str(), a.hash.as_str()).cmp(&(
            b.name.as_str(),
            b.version.as_str(),
            b.hash.as_str(),
        ))
    });
    entries.dedup();
    Sbom { entries }
}

/// Parse a Cargo.lock into the declared dependency set (one entry per
/// `[[package]]` — name, version, checksum). PURE; a malformed lockfile yields
/// an empty set (fail-closed: an empty closure never matches a real attestation).
/// This is the realistic SBOM source for a Rust plugin's reproducible closure.
pub fn parse_cargo_lock(lock: &str) -> Vec<DeclaredDep> {
    let Ok(table) = lock.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(pkgs) = table.get("package").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for p in pkgs {
        let Some(name) = p.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let version = p.get("version").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let hash = p.get("checksum").and_then(|v| v.as_str()).unwrap_or("").to_string();
        out.push(DeclaredDep {
            name: name.to_string(),
            version,
            hash,
        });
    }
    out
}

/// Convenience: canonical SBOM straight from a Cargo.lock body.
pub fn sbom_from_cargo_lock(lock: &str) -> Sbom {
    build_sbom(&parse_cargo_lock(lock))
}

impl Sbom {
    /// The stable SHA-256 hex of the canonical closure. Domain-separated so an
    /// SBOM hash can never collide with an artifact/attestation hash.
    pub fn closure_hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"darwin-sbom-v1\n");
        for e in &self.entries {
            h.update(e.name.as_bytes());
            h.update([SEP as u8]);
            h.update(e.version.as_bytes());
            h.update([SEP as u8]);
            h.update(e.hash.as_bytes());
            h.update(b"\n");
        }
        hex::encode(h.finalize())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// SHA-256 hex of a built artifact's raw bytes. PURE over the bytes — the
/// device-gated runner reads the built file and hands the bytes here.
pub fn hash_artifact(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b"darwin-artifact-v1\n");
    h.update(bytes);
    hex::encode(h.finalize())
}

// ===========================================================================
// (2) Attestation + the rebuilt-vs-attested compare — PURE, unit-tested.
// ===========================================================================

/// What a publisher attests about a plugin: its identity, the closure (SBOM)
/// hash of its declared dependency set, and the hash of the built artifact. The
/// ed25519 signature is over EXACTLY [`signing_bytes`](Attestation::signing_bytes),
/// so tampering ANY field invalidates the signature.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attestation {
    pub plugin_id: String,
    pub version: String,
    pub closure_hash: String,
    pub artifact_hash: String,
    /// Which allowlisted signer key id the attestation claims to be signed by.
    pub signer_key_id: String,
}

impl Attestation {
    /// The canonical bytes the signature covers. Deterministic + field-ordered +
    /// domain-separated: the verify side re-derives these EXACT bytes, so a
    /// single changed field breaks verification (fail-closed).
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut s = String::from("darwin-attestation-v1\n");
        for (label, value) in [
            ("plugin_id", &self.plugin_id),
            ("version", &self.version),
            ("closure_hash", &self.closure_hash),
            ("artifact_hash", &self.artifact_hash),
            ("signer_key_id", &self.signer_key_id),
        ] {
            s.push_str(label);
            s.push(SEP);
            s.push_str(value);
            s.push('\n');
        }
        s.into_bytes()
    }
}

/// The hashes a local staging rebuild produced. Compared against the attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildResult {
    pub closure_hash: String,
    pub artifact_hash: String,
}

/// Outcome of comparing a local rebuild against the attestation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchOutcome {
    /// Both the artifact hash and the closure hash matched — the bytes we built
    /// ARE the bytes the attestation claims.
    Match,
    /// A hash differed — REFUSE. `detail` names which (never a secret).
    Mismatch { detail: String },
}

/// Compare a local rebuild against the attestation. BOTH the artifact hash and
/// the closure hash must match; any difference is a `Mismatch` (fail-closed).
/// PURE.
pub fn attestation_matches(attested: &Attestation, rebuilt: &RebuildResult) -> MatchOutcome {
    if attested.artifact_hash != rebuilt.artifact_hash {
        return MatchOutcome::Mismatch {
            detail: "artifact hash differs from attestation".to_string(),
        };
    }
    if attested.closure_hash != rebuilt.closure_hash {
        return MatchOutcome::Mismatch {
            detail: "closure (SBOM) hash differs from attestation".to_string(),
        };
    }
    MatchOutcome::Match
}

// ===========================================================================
// (3) The signer allowlist + ed25519 verify — PURE, unit-tested. Fail-closed.
// ===========================================================================

/// The owner's set of trusted ed25519 signers: key id -> raw 32-byte public key.
/// Built from `[registry].signers` (hex-encoded). A malformed hex/length entry
/// is DROPPED (never trusted) so a typo can only make the gate refuse MORE,
/// never admit on a bad key.
#[derive(Debug, Clone, Default)]
pub struct SignerAllowlist {
    keys: BTreeMap<String, [u8; ED25519_PUBKEY_LEN]>,
}

impl SignerAllowlist {
    /// Parse the config allowlist (`key_id -> hex(32-byte pubkey)`), dropping any
    /// entry whose value is not exactly 32 hex-decoded bytes.
    pub fn from_config(signers: &BTreeMap<String, String>) -> Self {
        let mut keys = BTreeMap::new();
        for (id, hex_key) in signers {
            match hex::decode(hex_key.trim()) {
                Ok(bytes) if bytes.len() == ED25519_PUBKEY_LEN => {
                    let mut arr = [0u8; ED25519_PUBKEY_LEN];
                    arr.copy_from_slice(&bytes);
                    keys.insert(id.clone(), arr);
                }
                _ => {
                    tracing::warn!(signer = %id, "registry: ignoring malformed ed25519 signer key (must be 32-byte hex)");
                }
            }
        }
        Self { keys }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    fn get(&self, id: &str) -> Option<&[u8; ED25519_PUBKEY_LEN]> {
        self.keys.get(id)
    }
}

/// The verdict of an ed25519 verification. Fail-closed: only `Verified` admits;
/// every other arm refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SigVerdict {
    /// The signer id is allowlisted AND ring verified the signature — trusted.
    Verified,
    /// The signer id is NOT in the owner's allowlist — untrusted publisher.
    NotAllowlisted,
    /// The signer id is allowlisted but the signature did not verify (wrong key,
    /// tampered attestation, or malformed signature) — refuse.
    Invalid,
}

/// Verify an ed25519 signature over `message` by `signer_key_id`, against the
/// owner's allowlist. FAIL-CLOSED:
///   - signer id not allowlisted        -> `NotAllowlisted`;
///   - allowlisted but ring rejects it  -> `Invalid`;
///   - allowlisted AND ring accepts it  -> `Verified`.
///
/// PURE (ring's verify is a pure function of key+message+signature). Reuses
/// `ring` — no new signature crate.
pub fn verify_signature(
    allowlist: &SignerAllowlist,
    signer_key_id: &str,
    message: &[u8],
    signature: &[u8],
) -> SigVerdict {
    let Some(pubkey) = allowlist.get(signer_key_id) else {
        return SigVerdict::NotAllowlisted;
    };
    let key = UnparsedPublicKey::new(&ED25519, pubkey.as_ref());
    match key.verify(message, signature) {
        Ok(()) => SigVerdict::Verified,
        Err(_) => SigVerdict::Invalid,
    }
}

// ===========================================================================
// (4) The admission decision — PURE, unit-tested. NO install authority.
// ===========================================================================

/// A record admitted into the signed LOCAL index. It carries the attestation
/// fields plus the allowlisted signer id and the signature (hex), so the index
/// is SELF-ATTESTING: [`Registry::reverify`] re-checks every record's signature
/// against the current allowlist. SECRET-FREE (a public key id + a signature are
/// not secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRecord {
    pub plugin_id: String,
    pub version: String,
    pub closure_hash: String,
    pub artifact_hash: String,
    pub signer_key_id: String,
    /// The verified ed25519 signature over the attestation (hex).
    pub signature_hex: String,
}

impl RegistryRecord {
    fn attestation(&self) -> Attestation {
        Attestation {
            plugin_id: self.plugin_id.clone(),
            version: self.version.clone(),
            closure_hash: self.closure_hash.clone(),
            artifact_hash: self.artifact_hash.clone(),
            signer_key_id: self.signer_key_id.clone(),
        }
    }
}

/// The admission verdict. `Admitted` is ELIGIBILITY for the local index — it is
/// NOT an install (see [`install_decision`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Both gates cleared: eligible to record in the signed index.
    Admitted { record: RegistryRecord },
    /// Refused, fail-closed. `reason` never contains a secret.
    Refused { reason: String },
}

impl Admission {
    pub fn is_admitted(&self) -> bool {
        matches!(self, Admission::Admitted { .. })
    }
}

/// THE pure admission decision. Fail-closed — EVERY check must pass:
///   1. the ed25519 signature over the attestation must VERIFY against an
///      ALLOWLISTED signer (else Refused: not-allowlisted / invalid);
///   2. when `require_rebuild_match` is set, a local rebuild MUST be present and
///      its artifact + closure hashes MUST match the attestation (else Refused:
///      no-rebuild / mismatch).
///
/// On success returns `Admitted { record }` — a record for the index, NOT an
/// install. This function has no I/O and no actuator; it cannot install anything.
pub fn decide_admission(
    allowlist: &SignerAllowlist,
    attestation: &Attestation,
    signature: &[u8],
    rebuild: Option<&RebuildResult>,
    require_rebuild_match: bool,
) -> Admission {
    // (1) signature gate.
    match verify_signature(
        allowlist,
        &attestation.signer_key_id,
        &attestation.signing_bytes(),
        signature,
    ) {
        SigVerdict::Verified => {}
        SigVerdict::NotAllowlisted => {
            return Admission::Refused {
                reason: format!(
                    "signer {:?} is not in the owner's allowlist",
                    attestation.signer_key_id
                ),
            };
        }
        SigVerdict::Invalid => {
            return Admission::Refused {
                reason: "attestation signature did not verify (wrong key or tampered)".to_string(),
            };
        }
    }

    // (2) reproducible-build gate.
    if require_rebuild_match {
        let Some(rebuild) = rebuild else {
            return Admission::Refused {
                reason: "no local staging rebuild to compare against the attestation".to_string(),
            };
        };
        if let MatchOutcome::Mismatch { detail } = attestation_matches(attestation, rebuild) {
            return Admission::Refused {
                reason: format!("rebuild does not match attestation: {detail}"),
            };
        }
    }

    Admission::Admitted {
        record: RegistryRecord {
            plugin_id: attestation.plugin_id.clone(),
            version: attestation.version.clone(),
            closure_hash: attestation.closure_hash.clone(),
            artifact_hash: attestation.artifact_hash.clone(),
            signer_key_id: attestation.signer_key_id.clone(),
            signature_hex: hex::encode(signature),
        },
    }
}

/// The human-gated install decision. Verification is NECESSARY but NEVER
/// SUFFICIENT — a plugin installs ONLY when BOTH (a) the registry ADMITTED it and
/// (b) the human approved (the same out-of-band gate forge's apply_forge.sh is).
/// So the registry verify gate adds NO NEW INSTALL AUTHORITY: an `Admitted`
/// verdict on its own installs NOTHING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallDecision {
    /// Both gates cleared: the human-gated install may proceed.
    Install,
    /// The registry refused admission (fail-closed) — never install.
    RefusedByRegistry,
    /// Admitted, but the human has not approved — the install stays gated.
    AwaitingHumanGate,
}

/// Combine the registry verdict with the human's out-of-band approval. A refused
/// plugin NEVER installs, even with human approval; an admitted plugin installs
/// ONLY with human approval. This is the whole "no new install authority" proof.
pub fn install_decision(verdict: &Admission, human_approved: bool) -> InstallDecision {
    match verdict {
        Admission::Refused { .. } => InstallDecision::RefusedByRegistry,
        Admission::Admitted { .. } if human_approved => InstallDecision::Install,
        Admission::Admitted { .. } => InstallDecision::AwaitingHumanGate,
    }
}

// ===========================================================================
// The signed LOCAL index — self-attesting over the allowlist.
// ===========================================================================

/// The local index of admitted plugins. "Signed" = every record carries an
/// allowlisted ed25519 signature over its attestation, so the index re-attests
/// itself on load via [`reverify`](Registry::reverify) — tamper any record field
/// or drop a signer from the allowlist and reverify fails (fail-closed).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    pub records: Vec<RegistryRecord>,
}

impl Registry {
    /// Add an admitted record (the ONLY way a record enters the index is through
    /// [`decide_admission`], which already verified it). Adding to the index is
    /// NOT installing.
    pub fn admit(&mut self, record: RegistryRecord) {
        self.records.push(record);
    }

    /// Re-verify EVERY record against the current allowlist. `true` iff all
    /// records still carry an allowlisted, valid ed25519 signature over their
    /// attestation. An empty index re-verifies vacuously (`true`); a single bad
    /// record fails the whole index (fail-closed).
    pub fn reverify(&self, allowlist: &SignerAllowlist) -> bool {
        self.records.iter().all(|r| {
            let Ok(sig) = hex::decode(&r.signature_hex) else {
                return false;
            };
            let att = r.attestation();
            verify_signature(allowlist, &r.signer_key_id, &att.signing_bytes(), &sig)
                == SigVerdict::Verified
        })
    }
}

// ===========================================================================
// Telemetry contract — SECRET-FREE. The single source of truth for the
// registry.* wire shapes (mirrors introspect.rs's ev_* builders).
// ===========================================================================

pub const EV_VERDICT: &str = "registry.verdict";
pub const EV_STATUS: &str = "registry.status";

/// `(verdict_label, reason?)` for an admission — the SECRET-FREE fields the
/// telemetry frame carries.
fn verdict_label(a: &Admission) -> (&'static str, Option<&str>) {
    match a {
        Admission::Admitted { .. } => ("admitted", None),
        Admission::Refused { reason } => ("refused", Some(reason.as_str())),
    }
}

/// The per-plugin admission frame. SECRET-FREE: plugin id + verdict + signer KEY
/// ID (a label, never a key) + a refusal reason. NEVER a public/secret key, never
/// the signature bytes.
pub fn ev_verdict(plugin_id: &str, admission: &Admission, signer_key_id: &str) -> (&'static str, serde_json::Value) {
    let (verdict, reason) = verdict_label(admission);
    (
        EV_VERDICT,
        json!({
            "plugin": plugin_id,
            "verdict": verdict,
            "reason": reason,
            "signer": signer_key_id,
        }),
    )
}

/// The startup status frame: whether VERIFY is armed and how many trusted
/// signers are configured (0 => inert). SECRET-FREE (a count, never a key).
pub fn ev_status(verify: bool, require_rebuild_match: bool, signer_count: usize) -> (&'static str, serde_json::Value) {
    (
        EV_STATUS,
        json!({
            "verify": verify,
            "require_rebuild_match": require_rebuild_match,
            "signers": signer_count,
            "inert": signer_count == 0,
        }),
    )
}

/// Emit the startup `registry.status` frame ONCE so a HUD that connects after
/// boot learns the armed/inert posture. LIVE caller (main.rs). SECRET-FREE.
pub fn announce_status(verify: bool, require_rebuild_match: bool, signers: &BTreeMap<String, String>) {
    let allowlist = SignerAllowlist::from_config(signers);
    let (event, payload) = ev_status(verify, require_rebuild_match, allowlist.len());
    telemetry::emit("system", event, payload);
    tracing::info!(
        verify,
        require_rebuild_match,
        signers = allowlist.len(),
        "registry: attested-registry verify gate armed (inert until a trusted signer is added)"
    );
}

// ===========================================================================
// (5) The staging rebuild — the DEVICE-GATED runner. Behind a trait so tests
// drive the flow with a mock (no real build under `cargo test`).
// ===========================================================================

/// What a staging rebuild needs: a confined dir already populated with the
/// plugin's sources, and the relative path of the artifact to hash after build.
#[derive(Debug, Clone)]
pub struct RebuildRequest {
    pub staging_dir: PathBuf,
    pub artifact_rel: String,
}

/// A `Send` future for the trait method, spelled out so the trait stays
/// object-safe (`&dyn StagingRebuilder`) WITHOUT the async-trait crate (mirrors
/// forge's `BrainFuture`).
type RebuildFuture<'a> = Pin<Box<dyn Future<Output = Result<RebuildResult>> + Send + 'a>>;

/// The device-gated rebuild seam — the ONLY route to a real toolchain build.
/// Unit tests inject a mock so no build runs under `cargo test`.
pub trait StagingRebuilder: Send + Sync {
    fn rebuild<'a>(&'a self, req: &'a RebuildRequest) -> RebuildFuture<'a>;
}

/// Production rebuilder: `cargo build --release --locked` in the confined staging
/// dir (timeout-capped, kill_on_drop, stdin null — mirrors forge's run_capture),
/// then derives the closure hash from the staging Cargo.lock and the artifact
/// hash from the built file. DEVICE-GATED: it spawns a real toolchain build, so
/// it is NEVER run under `cargo test` (the mock covers the flow).
pub struct CargoRebuilder;

impl StagingRebuilder for CargoRebuilder {
    fn rebuild<'a>(&'a self, req: &'a RebuildRequest) -> RebuildFuture<'a> {
        Box::pin(async move {
            let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
            let child = tokio::process::Command::new(&cargo)
                .args(["build", "--release", "--locked"])
                .current_dir(&req.staging_dir)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .context("spawning the staging rebuild")?;
            let out = match tokio::time::timeout(REBUILD_TIMEOUT, child.wait_with_output()).await {
                Ok(result) => result?,
                Err(_) => bail!("staging rebuild timed out after {}s", REBUILD_TIMEOUT.as_secs()),
            };
            if !out.status.success() {
                bail!(
                    "staging rebuild failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            // Closure hash from the reproduced lockfile.
            let lock = std::fs::read_to_string(req.staging_dir.join("Cargo.lock"))
                .context("reading Cargo.lock for the SBOM closure hash")?;
            let closure_hash = sbom_from_cargo_lock(&lock).closure_hash();
            // Artifact hash from the built binary.
            let artifact_path = req.staging_dir.join(&req.artifact_rel);
            let bytes = std::fs::read(&artifact_path)
                .with_context(|| format!("reading built artifact {}", artifact_path.display()))?;
            Ok(RebuildResult {
                closure_hash,
                artifact_hash: hash_artifact(&bytes),
            })
        })
    }
}

/// The device-gated verification flow: run the staging rebuild, feed the result
/// into the PURE [`decide_admission`], emit the SECRET-FREE `registry.verdict`
/// frame, and return the admission. FAIL-CLOSED: a rebuild that fails to run
/// yields `None`, which [`decide_admission`] refuses whenever a rebuild is
/// required. Adds NO install authority — the return is an eligibility verdict.
pub async fn run_verification(
    allowlist: &SignerAllowlist,
    attestation: &Attestation,
    signature: &[u8],
    require_rebuild_match: bool,
    rebuilder: &dyn StagingRebuilder,
    req: &RebuildRequest,
) -> Admission {
    let rebuild = match rebuilder.rebuild(req).await {
        Ok(r) => Some(r),
        Err(e) => {
            tracing::warn!(error = %e, plugin = %attestation.plugin_id, "registry: staging rebuild failed");
            None
        }
    };
    let admission = decide_admission(
        allowlist,
        attestation,
        signature,
        rebuild.as_ref(),
        require_rebuild_match,
    );
    let (event, payload) = ev_verdict(&attestation.plugin_id, &admission, &attestation.signer_key_id);
    telemetry::emit("system", event, payload);
    admission
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    // -- test signing helpers (ring signs in the TEST only; the daemon runtime
    //    is verify-only). A fixed seed -> a deterministic keypair. ------------

    fn keypair(seed: &[u8; 32]) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(seed).expect("valid ed25519 seed")
    }

    fn pubkey_hex(kp: &Ed25519KeyPair) -> String {
        hex::encode(kp.public_key().as_ref())
    }

    fn allowlist_of(pairs: &[(&str, &Ed25519KeyPair)]) -> SignerAllowlist {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(id, kp)| (id.to_string(), pubkey_hex(kp)))
            .collect();
        SignerAllowlist::from_config(&map)
    }

    fn sample_attestation() -> Attestation {
        let sbom = sbom_from_cargo_lock(SAMPLE_LOCK);
        Attestation {
            plugin_id: "acme-linter".to_string(),
            version: "1.2.0".to_string(),
            closure_hash: sbom.closure_hash(),
            artifact_hash: hash_artifact(b"the built artifact bytes"),
            signer_key_id: "owner-key-1".to_string(),
        }
    }

    const SAMPLE_LOCK: &str = "\
version = 3

[[package]]
name = \"serde\"
version = \"1.0.0\"
checksum = \"aaaa\"

[[package]]
name = \"anyhow\"
version = \"1.0.0\"
checksum = \"bbbb\"
";

    // -- (1) SBOM build: STABLE ----------------------------------------------

    #[test]
    fn sbom_closure_hash_is_order_and_dup_and_whitespace_stable() {
        let a = vec![
            DeclaredDep { name: "serde".into(), version: "1.0".into(), hash: "h1".into() },
            DeclaredDep { name: "anyhow".into(), version: "1.0".into(), hash: "h2".into() },
        ];
        // Same set, reversed order + a duplicate + surrounding whitespace.
        let b = vec![
            DeclaredDep { name: " anyhow ".into(), version: "1.0".into(), hash: "h2".into() },
            DeclaredDep { name: "serde".into(), version: " 1.0 ".into(), hash: " h1 ".into() },
            DeclaredDep { name: "serde".into(), version: "1.0".into(), hash: "h1".into() },
        ];
        let ha = build_sbom(&a).closure_hash();
        let hb = build_sbom(&b).closure_hash();
        assert_eq!(ha, hb, "closure hash must be canonical/stable");
        assert_eq!(ha.len(), 64, "sha-256 hex");
        // A changed dependency hash changes the closure hash.
        let c = vec![
            DeclaredDep { name: "serde".into(), version: "1.0".into(), hash: "TAMPERED".into() },
            DeclaredDep { name: "anyhow".into(), version: "1.0".into(), hash: "h2".into() },
        ];
        assert_ne!(ha, build_sbom(&c).closure_hash(), "a swapped dep hash must change the closure");
    }

    #[test]
    fn sbom_build_drops_empty_names_and_dedups() {
        let deps = vec![
            DeclaredDep { name: "  ".into(), version: "1".into(), hash: "x".into() }, // dropped
            DeclaredDep { name: "a".into(), version: "1".into(), hash: "x".into() },
            DeclaredDep { name: "a".into(), version: "1".into(), hash: "x".into() }, // dup
        ];
        let sbom = build_sbom(&deps);
        assert_eq!(sbom.entries.len(), 1);
        assert_eq!(sbom.entries[0].name, "a");
    }

    #[test]
    fn parse_cargo_lock_extracts_packages_and_is_stable() {
        let deps = parse_cargo_lock(SAMPLE_LOCK);
        assert_eq!(deps.len(), 2);
        // The SBOM is order-independent, so the two package orders hash equally.
        let reordered = "\
[[package]]
name = \"anyhow\"
version = \"1.0.0\"
checksum = \"bbbb\"

[[package]]
name = \"serde\"
version = \"1.0.0\"
checksum = \"aaaa\"
";
        assert_eq!(
            sbom_from_cargo_lock(SAMPLE_LOCK).closure_hash(),
            sbom_from_cargo_lock(reordered).closure_hash()
        );
        // A malformed lockfile yields an empty (fail-closed) closure.
        assert!(sbom_from_cargo_lock("not : valid ! toml [[[").is_empty());
    }

    #[test]
    fn artifact_hash_is_content_sensitive() {
        assert_eq!(hash_artifact(b"same"), hash_artifact(b"same"));
        assert_ne!(hash_artifact(b"a"), hash_artifact(b"b"));
        // Domain separation: an SBOM hash and an artifact hash over the same
        // logical bytes never collide.
        assert_ne!(hash_artifact(b""), Sbom { entries: vec![] }.closure_hash());
    }

    // -- (2) attestation compare: match vs mismatch --------------------------

    #[test]
    fn attestation_matches_only_when_both_hashes_agree() {
        let att = sample_attestation();
        let good = RebuildResult {
            closure_hash: att.closure_hash.clone(),
            artifact_hash: att.artifact_hash.clone(),
        };
        assert_eq!(attestation_matches(&att, &good), MatchOutcome::Match);

        // Artifact hash differs -> mismatch (refuse).
        let bad_artifact = RebuildResult {
            closure_hash: att.closure_hash.clone(),
            artifact_hash: "deadbeef".into(),
        };
        assert!(matches!(
            attestation_matches(&att, &bad_artifact),
            MatchOutcome::Mismatch { .. }
        ));

        // Closure hash differs -> mismatch (refuse).
        let bad_closure = RebuildResult {
            closure_hash: "deadbeef".into(),
            artifact_hash: att.artifact_hash.clone(),
        };
        assert!(matches!(
            attestation_matches(&att, &bad_closure),
            MatchOutcome::Mismatch { .. }
        ));
    }

    // -- (3) ed25519 verify: valid vs wrong-key vs tampered vs non-allowlisted

    #[test]
    fn verify_signature_accepts_valid_allowlisted_and_refuses_everything_else() {
        let kp = keypair(&[7u8; 32]);
        let other = keypair(&[9u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let msg = att.signing_bytes();
        let sig = kp.sign(&msg);

        // Valid + allowlisted -> Verified.
        assert_eq!(
            verify_signature(&allow, "owner-key-1", &msg, sig.as_ref()),
            SigVerdict::Verified
        );

        // Tampered message (attestation edited after signing) -> Invalid.
        let mut tampered = att.clone();
        tampered.artifact_hash = "0000".into();
        assert_eq!(
            verify_signature(&allow, "owner-key-1", &tampered.signing_bytes(), sig.as_ref()),
            SigVerdict::Invalid,
            "a signature over the original bytes must not verify against edited bytes"
        );

        // Wrong key: allowlisted id 'owner-key-1' holds kp's pubkey, but the
        // signature was made by `other` -> Invalid.
        let wrong_sig = other.sign(&msg);
        assert_eq!(
            verify_signature(&allow, "owner-key-1", &msg, wrong_sig.as_ref()),
            SigVerdict::Invalid
        );

        // Non-allowlisted signer id -> NotAllowlisted (never even reaches ring).
        assert_eq!(
            verify_signature(&allow, "unknown-key", &msg, sig.as_ref()),
            SigVerdict::NotAllowlisted
        );

        // Empty allowlist (shipped default) -> nothing verifies (inert/fail-closed).
        let empty = SignerAllowlist::default();
        assert_eq!(
            verify_signature(&empty, "owner-key-1", &msg, sig.as_ref()),
            SigVerdict::NotAllowlisted
        );
    }

    #[test]
    fn allowlist_from_config_drops_malformed_keys() {
        let kp = keypair(&[3u8; 32]);
        let mut cfg = BTreeMap::new();
        cfg.insert("good".to_string(), pubkey_hex(&kp));
        cfg.insert("short".to_string(), "abcd".to_string()); // not 32 bytes
        cfg.insert("nonhex".to_string(), "zzzz".to_string()); // not hex
        let allow = SignerAllowlist::from_config(&cfg);
        assert_eq!(allow.len(), 1, "only the well-formed 32-byte key is trusted");
        assert!(allow.get("good").is_some());
        assert!(allow.get("short").is_none());
        assert!(allow.get("nonhex").is_none());
    }

    // -- (4) decide_admission: fail-closed on ANY failing gate ---------------

    #[test]
    fn decide_admission_admits_only_when_signature_and_rebuild_both_pass() {
        let kp = keypair(&[7u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let sig = kp.sign(&att.signing_bytes());
        let good = RebuildResult {
            closure_hash: att.closure_hash.clone(),
            artifact_hash: att.artifact_hash.clone(),
        };

        // Both gates pass -> Admitted, and the record round-trips the attestation.
        let admission = decide_admission(&allow, &att, sig.as_ref(), Some(&good), true);
        match &admission {
            Admission::Admitted { record } => {
                assert_eq!(record.plugin_id, att.plugin_id);
                assert_eq!(record.artifact_hash, att.artifact_hash);
                assert_eq!(record.signer_key_id, "owner-key-1");
                assert_eq!(record.signature_hex, hex::encode(sig.as_ref()));
            }
            other => panic!("expected Admitted, got {other:?}"),
        }

        // Rebuild MISMATCH -> Refused, even with a valid signature.
        let bad = RebuildResult { artifact_hash: "0000".into(), ..good.clone() };
        assert!(matches!(
            decide_admission(&allow, &att, sig.as_ref(), Some(&bad), true),
            Admission::Refused { .. }
        ));

        // Rebuild REQUIRED but ABSENT -> Refused (fail-closed).
        assert!(matches!(
            decide_admission(&allow, &att, sig.as_ref(), None, true),
            Admission::Refused { .. }
        ));

        // Non-allowlisted signer -> Refused (even with a matching rebuild).
        let stranger = keypair(&[1u8; 32]);
        let mut att2 = att.clone();
        att2.signer_key_id = "stranger".to_string();
        let sig2 = stranger.sign(&att2.signing_bytes());
        assert!(matches!(
            decide_admission(&allow, &att2, sig2.as_ref(), Some(&good), true),
            Admission::Refused { .. }
        ));

        // Tampered attestation (re-hashed after signing) -> Refused.
        let mut tampered = att.clone();
        tampered.artifact_hash = "beef".into();
        assert!(
            matches!(
                decide_admission(&allow, &tampered, sig.as_ref(), Some(&good), true),
                Admission::Refused { .. }
            ),
            "an attestation edited after signing must be refused"
        );
    }

    #[test]
    fn decide_admission_signature_only_mode_skips_rebuild_but_still_needs_signature() {
        let kp = keypair(&[7u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let sig = kp.sign(&att.signing_bytes());
        // require_rebuild_match = false -> a valid signature admits without a rebuild.
        assert!(decide_admission(&allow, &att, sig.as_ref(), None, false).is_admitted());
        // ...but a bad signature is STILL refused (the signature gate never relaxes).
        assert!(matches!(
            decide_admission(&allow, &att, b"not-a-signature", None, false),
            Admission::Refused { .. }
        ));
    }

    // -- the "NO new install authority" proof --------------------------------

    #[test]
    fn verification_adds_no_new_install_authority() {
        let kp = keypair(&[7u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let sig = kp.sign(&att.signing_bytes());
        let good = RebuildResult {
            closure_hash: att.closure_hash.clone(),
            artifact_hash: att.artifact_hash.clone(),
        };
        let admitted = decide_admission(&allow, &att, sig.as_ref(), Some(&good), true);
        assert!(admitted.is_admitted());

        // ADMITTED alone installs NOTHING — the human gate is still required.
        assert_eq!(
            install_decision(&admitted, false),
            InstallDecision::AwaitingHumanGate,
            "an admitted verdict on its own must NOT install (no new install authority)"
        );
        // Only ADMITTED + human approval installs.
        assert_eq!(install_decision(&admitted, true), InstallDecision::Install);

        // A REFUSED plugin never installs — even if the human approves.
        let refused = Admission::Refused { reason: "x".into() };
        assert_eq!(install_decision(&refused, true), InstallDecision::RefusedByRegistry);
        assert_eq!(install_decision(&refused, false), InstallDecision::RefusedByRegistry);
    }

    // -- the signed LOCAL index self-attests ---------------------------------

    #[test]
    fn registry_reverify_is_self_attesting_and_fail_closed() {
        let kp = keypair(&[7u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let sig = kp.sign(&att.signing_bytes());
        let good = RebuildResult {
            closure_hash: att.closure_hash.clone(),
            artifact_hash: att.artifact_hash.clone(),
        };
        let Admission::Admitted { record } = decide_admission(&allow, &att, sig.as_ref(), Some(&good), true)
        else {
            panic!("expected admission");
        };

        let mut reg = Registry::default();
        assert!(reg.reverify(&allow), "an empty index re-verifies vacuously");
        reg.admit(record);
        assert!(reg.reverify(&allow), "an admitted record re-verifies");

        // Tamper a stored field -> the signature no longer covers it -> reverify fails.
        let mut tampered = reg.clone();
        tampered.records[0].artifact_hash = "0000".into();
        assert!(!tampered.reverify(&allow), "a tampered index record must fail reverify");

        // Drop the signer from the allowlist -> the record is no longer trusted.
        let empty = SignerAllowlist::default();
        assert!(!reg.reverify(&empty), "without the trusted signer the index fails reverify");
    }

    // -- telemetry: SECRET-FREE ----------------------------------------------

    #[test]
    fn verdict_frame_is_secret_free() {
        let kp = keypair(&[7u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let sig = kp.sign(&att.signing_bytes());
        let good = RebuildResult {
            closure_hash: att.closure_hash.clone(),
            artifact_hash: att.artifact_hash.clone(),
        };
        let admission = decide_admission(&allow, &att, sig.as_ref(), Some(&good), true);
        let (event, payload) = ev_verdict(&att.plugin_id, &admission, &att.signer_key_id);
        assert_eq!(event, "registry.verdict");
        assert_eq!(payload["plugin"], "acme-linter");
        assert_eq!(payload["verdict"], "admitted");
        assert_eq!(payload["signer"], "owner-key-1");
        // NEVER a key or a signature on the wire.
        let wire = payload.to_string();
        assert!(!wire.contains(&pubkey_hex(&kp)), "no public key bytes on the wire");
        assert!(!wire.contains(&hex::encode(sig.as_ref())), "no signature bytes on the wire");

        // A refusal carries the reason (a label), still secret-free.
        let refused = Admission::Refused { reason: "signer not allowlisted".into() };
        let (_e, p) = ev_verdict("acme-linter", &refused, "owner-key-1");
        assert_eq!(p["verdict"], "refused");
        assert_eq!(p["reason"], "signer not allowlisted");
    }

    #[test]
    fn status_frame_reports_armed_and_inert() {
        // Armed + no signer -> inert.
        let (event, p) = ev_status(true, true, 0);
        assert_eq!(event, "registry.status");
        assert_eq!(p["verify"], true);
        assert_eq!(p["signers"], 0);
        assert_eq!(p["inert"], true, "no trusted signer => inert");
        // Armed + a signer -> not inert.
        let (_e, p2) = ev_status(true, true, 2);
        assert_eq!(p2["signers"], 2);
        assert_eq!(p2["inert"], false);
    }

    // -- run_verification drives the whole flow via a MOCK rebuilder (no real
    //    build under cargo test) ----------------------------------------------

    struct MockRebuilder {
        result: RebuildResult,
    }
    impl StagingRebuilder for MockRebuilder {
        fn rebuild<'a>(&'a self, _req: &'a RebuildRequest) -> RebuildFuture<'a> {
            let r = self.result.clone();
            Box::pin(async move { Ok(r) })
        }
    }

    struct FailRebuilder;
    impl StagingRebuilder for FailRebuilder {
        fn rebuild<'a>(&'a self, _req: &'a RebuildRequest) -> RebuildFuture<'a> {
            Box::pin(async move { bail!("rebuild toolchain unavailable") })
        }
    }

    fn dummy_req() -> RebuildRequest {
        RebuildRequest { staging_dir: PathBuf::from("/nonexistent"), artifact_rel: "bin".into() }
    }

    #[tokio::test]
    async fn run_verification_admits_on_a_matching_rebuild_and_refuses_on_mismatch() {
        let kp = keypair(&[7u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let sig = kp.sign(&att.signing_bytes());

        // Matching rebuild -> Admitted.
        let matching = MockRebuilder {
            result: RebuildResult {
                closure_hash: att.closure_hash.clone(),
                artifact_hash: att.artifact_hash.clone(),
            },
        };
        let admission =
            run_verification(&allow, &att, sig.as_ref(), true, &matching, &dummy_req()).await;
        assert!(admission.is_admitted(), "a matching rebuild + valid signature must admit");

        // Mismatching rebuild -> Refused.
        let mismatch = MockRebuilder {
            result: RebuildResult {
                closure_hash: att.closure_hash.clone(),
                artifact_hash: "0000".into(),
            },
        };
        let refused =
            run_verification(&allow, &att, sig.as_ref(), true, &mismatch, &dummy_req()).await;
        assert!(!refused.is_admitted(), "a mismatched rebuild must refuse");
    }

    #[tokio::test]
    async fn run_verification_is_fail_closed_when_the_rebuild_cannot_run() {
        let kp = keypair(&[7u8; 32]);
        let allow = allowlist_of(&[("owner-key-1", &kp)]);
        let att = sample_attestation();
        let sig = kp.sign(&att.signing_bytes());
        // The rebuild errored: with require_rebuild_match, admission must refuse.
        let admission =
            run_verification(&allow, &att, sig.as_ref(), true, &FailRebuilder, &dummy_req()).await;
        assert!(
            !admission.is_admitted(),
            "a rebuild that cannot run must fail-closed (no admission)"
        );
    }
}
