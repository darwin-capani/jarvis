//! Substrate Lock — reproducible env pins with lockfile-verified dependency
//! closures for micro-apps / forged apps (the host side complement to
//! docs/SANDBOX.md's `generate_sbpl`).
//!
//! THE CAVEATS THIS CLOSES (docs/SANDBOX.md → *Honest limitations*): a
//! `runtime = "python"` micro-app is launched under the PROJECT-SHARED
//! `.venv/bin/python3` and the generated seatbelt profile grants it read of the
//! entire shared `.venv` tree. Two documented consequences:
//!   1. **shared-.venv reach** — every python app reads the SAME site-packages
//!      tree, so one app's dependency is visible to all; the grant is wider than
//!      any single app needs.
//!   2. **venv drift** — the interpreter + site-packages the app ran against are
//!      whatever the shared `.venv` happens to hold at launch, with nothing
//!      pinning them; an in-place `pip install`/upgrade silently changes what a
//!      given app executes.
//!
//! Substrate Lock NARROWS (never widens) both: an app may be PINNED to a
//! CONTENT-ADDRESSED, lockfile-verified dependency closure materialized at
//! `state/envstore/<closure_hash>/`, described by an `apps/<name>/env.lock` (the
//! per-file hashes the closure is the content address of). At SPAWN the host:
//!   - recomputes the closure's content address from the ON-DISK materialized
//!     files and compares it to `env.lock`. **FAIL-CLOSED**: any mismatch (a
//!     changed/missing/extra file, a tampered lock) REFUSES to spawn — it never
//!     silently falls back to the shared `.venv`.
//!   - on a match, grants the sandbox exec/read of ONLY that pinned closure path
//!     (`state/envstore/<hash>/`) INSTEAD of the shared `.venv` — a strictly
//!     narrower, app-specific, read-only reach.
//!
//! NARROWS, BENIGN-ONLY: the envstore is read-only to the app; the sandboxed app
//! still gets ZERO network and its exec is confined to its own pinned closure.
//! The one network step — MATERIALIZING a closure ([`env_build`]) — is a
//! USER-ORIGINATED, config-gated fetch run by the daemon behind the same
//! prompt-injection egress gate as `open_url`/`web_search` (an injected or
//! autonomous build is REFUSED); installing/authoring a lock stays human-gated
//! exactly like forge. The closure-hash compute, the `env.lock` verify, and the
//! SBPL-narrowing are all PURE, unit-tested seams; only the fetch is impure and
//! DEVICE-GATED (wired, never exercised in tests — the webhook-bind / mic-loop
//! precedent).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::apps::Runtime;

/// Largest single artifact [`env_build`] will download + hash (bytes). A larger
/// body is refused rather than buffered — a lock entry can never make the daemon
/// pull an unbounded blob. Generous for a wheel/tarball, far below OOM.
const MAX_ARTIFACT_BYTES: usize = 512 * 1024 * 1024;

/// Largest env.lock we will read (bytes) — a lock is a small manifest of hashes,
/// never megabytes; cap it so a hostile file cannot balloon the parse.
const MAX_LOCK_BYTES: u64 = 4 * 1024 * 1024;

// ===========================================================================
// Spawn-time VERIFY master switch ([envlock].enabled) — armed-by-default
// ===========================================================================

/// Whether the spawn-time closure VERIFY + SBPL-narrow is armed. Installed once
/// at startup from `[envlock].enabled` (mirrors the `lockdown` global). ARMED BY
/// DEFAULT (`true`) so a launcher path — or a test — that never calls
/// [`set_verify_enabled`] still gets the strict-only verify; unpinned apps are
/// unaffected either way.
static VERIFY_ENABLED: AtomicBool = AtomicBool::new(true);

/// Install the `[envlock].enabled` flag at startup. Off => the launcher skips the
/// closure verify + narrowing entirely (every app keeps the legacy shared-.venv).
pub fn set_verify_enabled(on: bool) {
    VERIFY_ENABLED.store(on, Ordering::Relaxed);
}

/// Whether the spawn-time verify + narrow is armed (see [`set_verify_enabled`]).
pub fn verify_enabled() -> bool {
    VERIFY_ENABLED.load(Ordering::Relaxed)
}

// ===========================================================================
// Lock + closure model
// ===========================================================================

/// One file in a pinned dependency closure: a closure-relative path and the
/// SHA-256 of its bytes (lowercase hex). `url` is OPTIONAL provenance used ONLY
/// by the materialization fetch — it is deliberately NOT part of the content
/// address (two closures with identical bytes are the same closure regardless of
/// where they were fetched from). `deny_unknown_fields` so a typo'd key is a
/// parse error, never a silently-dropped hash.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClosureEntry {
    /// Path of the file WITHIN the closure dir (e.g. `bin/python3`,
    /// `lib/python3.11/site-packages/foo/__init__.py`). Confined: validated (no
    /// traversal / absolute) before it is ever joined onto a real path.
    pub path: String,
    /// SHA-256 of the file's bytes, lowercase hex. The content address of THIS
    /// file; the closure's content address is derived from the whole set.
    pub sha256: String,
    /// OPTIONAL source URL for the materialization fetch. Not part of the content
    /// address. Empty for entries produced by scanning an already-materialized
    /// closure ([`scan_closure`]).
    #[serde(default)]
    pub url: String,
}

/// A parsed `apps/<name>/env.lock`: the recorded content address of the whole
/// closure plus the per-file hashes it is derived from. `deny_unknown_fields`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvLock {
    /// The closure's content address: `sha256` over the canonical, sorted list of
    /// `(path, sha256)`. Recomputed from the on-disk closure at spawn and compared
    /// FAIL-CLOSED. A lock whose recorded hash disagrees with its own entries (a
    /// hand-edited hash) is rejected as inconsistent.
    pub closure_hash: String,
    /// The per-file hashes the `closure_hash` is the content address of.
    pub entries: Vec<ClosureEntry>,
}

// ===========================================================================
// PURE seam 1 — content-addressed closure hash
// ===========================================================================

/// The STABLE, content-addressed hash of a dependency closure: `sha256` over the
/// canonical form of its entries. ORDER-INDEPENDENT — entries are sorted by path
/// first — so the same set of files hashes identically regardless of listing
/// order (mirrors `apps::canonical_permissions`). Depends ONLY on each entry's
/// `(path, sha256)`; the optional `url` provenance is excluded, so re-pointing a
/// mirror never changes a closure's identity. Pure.
pub fn compute_closure_hash(entries: &[ClosureEntry]) -> String {
    let mut sorted: Vec<(&str, &str)> = entries
        .iter()
        .map(|e| (e.path.as_str(), e.sha256.as_str()))
        .collect();
    sorted.sort_unstable();
    let mut h = Sha256::new();
    for (path, sha) in sorted {
        // NUL-delimit every field so no path can bleed into the next hash (a path
        // ending in the next entry's prefix can never collide) — same discipline
        // as apps::token_message.
        h.update(path.as_bytes());
        h.update([0u8]);
        h.update(sha.as_bytes());
        h.update([0u8]);
    }
    hex::encode(h.finalize())
}

// ===========================================================================
// PURE seam 2 — env.lock verify (FAIL-CLOSED)
// ===========================================================================

/// Why a pinned closure was refused. Each is a fail-closed refusal to spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefuseReason {
    /// `env.lock`'s recorded `closure_hash` disagrees with the hash re-derived
    /// from its OWN entries — a lock whose hash was hand-edited without
    /// re-deriving. Untrusted; refuse.
    LockInconsistent,
    /// The materialized closure on disk hashes to a DIFFERENT content address
    /// than `env.lock` records (a changed/missing/extra file, or a missing
    /// closure). The dependency the app would run against is not the pinned one.
    ClosureMismatch,
}

impl RefuseReason {
    /// A short, secret-free label for telemetry / logs.
    pub fn as_str(self) -> &'static str {
        match self {
            RefuseReason::LockInconsistent => "lock_inconsistent",
            RefuseReason::ClosureMismatch => "closure_mismatch",
        }
    }
}

/// The verify verdict for a materialized closure against its `env.lock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnVerdict {
    /// The materialized closure matches `env.lock` exactly — spawn may proceed
    /// and the sandbox is narrowed to the pinned path. Carries the verified
    /// content address (the shared telemetry field).
    Verified { closure_hash: String },
    /// FAIL-CLOSED: the closure does not verify — the launcher REFUSES to spawn.
    Refused {
        reason: RefuseReason,
        /// The hash actually observed (the materialized closure's, or the lock's
        /// re-derived one for a `LockInconsistent`) — for the audit frame.
        closure_hash: String,
    },
}

impl SpawnVerdict {
    /// Whether this verdict permits the app to spawn.
    pub fn is_allowed(&self) -> bool {
        matches!(self, SpawnVerdict::Verified { .. })
    }
}

/// Verify a materialized closure against its `env.lock`, **FAIL-CLOSED**. Two
/// checks, BOTH must hold:
///   1. `env.lock` is self-consistent — its recorded `closure_hash` equals the
///      hash re-derived from its own `entries` (a lock whose hash was hand-edited
///      is untrusted).
///   2. the ACTUAL materialized entries hash to that SAME content address.
///
/// Any mismatch ⇒ `Refused` (the launcher refuses to spawn); it never falls back
/// to the shared `.venv`. Pure — the tests drive it directly.
pub fn verify_closure(lock: &EnvLock, actual: &[ClosureEntry]) -> SpawnVerdict {
    // (1) The lock must be self-consistent: a recorded hash that does not match
    // its own entries means the lock was tampered with (hash edited, entries not,
    // or vice-versa) — trust nothing derived from it.
    let lock_recomputed = compute_closure_hash(&lock.entries);
    if lock_recomputed != lock.closure_hash {
        return SpawnVerdict::Refused {
            reason: RefuseReason::LockInconsistent,
            closure_hash: lock_recomputed,
        };
    }
    // (2) The materialized closure must reproduce the pinned content address.
    let actual_hash = compute_closure_hash(actual);
    if actual_hash != lock.closure_hash {
        return SpawnVerdict::Refused {
            reason: RefuseReason::ClosureMismatch,
            closure_hash: actual_hash,
        };
    }
    SpawnVerdict::Verified {
        closure_hash: actual_hash,
    }
}

// ===========================================================================
// PURE seam 3 — SBPL narrowing (pinned path instead of the shared .venv)
// ===========================================================================

/// The env store root: `<project_root>/state/envstore`. Every pinned closure
/// lives under a content-address subdir here.
pub fn envstore_root(project_root: &Path) -> PathBuf {
    project_root.join("state").join("envstore")
}

/// A pinned closure's directory: `state/envstore/<closure_hash>/`. `closure_hash`
/// is expected to be a validated hex content address (see [`is_hex_hash`]); it is
/// the ONLY path component derived from lock data.
pub fn closure_dir(project_root: &Path, closure_hash: &str) -> PathBuf {
    envstore_root(project_root).join(closure_hash)
}

/// Whether `s` is a safe content-address hex string usable as a directory name:
/// non-empty, all lowercase ASCII hex digits, bounded length. All-hex GUARANTEES
/// no `/`, `.`, or `..`, so a lock's `closure_hash` can never traverse out of the
/// env store when it is joined onto a path.
pub fn is_hex_hash(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// If `interp_abs` resolves INSIDE the env store
/// (`state/envstore/<hash>/…`), return that closure's dir
/// (`state/envstore/<hash>`); else `None`. This is the narrowing PREDICATE: an
/// interpreter under the env store means the app is running against a pinned
/// closure, so the sandbox read root should be that closure — not the shared
/// `.venv`. Pure. `<hash>` is validated hex so a crafted interpreter path cannot
/// widen the grant.
pub fn pinned_closure_of(project_root: &Path, interp_abs: &Path) -> Option<PathBuf> {
    let store = envstore_root(project_root);
    let rel = interp_abs.strip_prefix(&store).ok()?;
    let first = rel.components().next()?;
    let hash = first.as_os_str().to_str()?;
    if !is_hex_hash(hash) {
        return None;
    }
    Some(store.join(hash))
}

/// The single directory tree a `runtime = "python"` app may READ its interpreter
/// + site-packages from, given the interpreter the launcher resolved.
///   - PINNED (interpreter under `state/envstore/<hash>/`): that CLOSURE dir —
///     app-specific, read-only, exactly the pinned files. This NARROWS the grant
///     from the project-shared, drift-prone `.venv` to only the pinned closure,
///     closing the shared-.venv reach + venv-drift caveats.
///   - UNPINNED (interpreter under `.venv`, the legacy path): the project
///     `.venv`, byte-for-byte the prior behavior.
///
/// This is the seam `apps::generate_sbpl` calls in place of the hard-coded
/// `.venv` read prefix. Pure. Strictly narrower or identical — it can only ever
/// REPLACE the shared `.venv` with a subset-scoped pinned dir, never add reach.
pub fn python_runtime_read_root(project_root: &Path, interp_abs: &Path) -> PathBuf {
    match pinned_closure_of(project_root, interp_abs) {
        Some(dir) => dir,
        None => project_root.join(".venv"),
    }
}

// ===========================================================================
// Lock + closure I/O (local files only — no network)
// ===========================================================================

/// A confined closure-relative path: non-empty, not absolute, no `..` / root
/// component — so it can never escape the closure dir it is joined onto.
fn is_confined_relpath(p: &str) -> bool {
    let p = p.trim();
    if p.is_empty() || p.starts_with('/') {
        return false;
    }
    !Path::new(p).components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    })
}

/// The env.lock path for an app dir.
pub fn envlock_path(app_dir: &Path) -> PathBuf {
    app_dir.join("env.lock")
}

/// Load + parse `apps/<name>/env.lock`. Returns:
///   - `None` when no env.lock exists — the app is UNPINNED and keeps the legacy
///     shared-`.venv` behavior (every app that ships today).
///   - `Some(Ok(lock))` on a well-formed lock.
///   - `Some(Err(_))` on an unreadable / malformed / oversized lock — the caller
///     treats a pinned-but-broken lock as fail-closed, never a silent fallback.
pub fn load_lock(app_dir: &Path) -> Option<Result<EnvLock>> {
    let path = envlock_path(app_dir);
    let meta = std::fs::metadata(&path).ok()?; // absent => None => Unpinned
    if meta.len() > MAX_LOCK_BYTES {
        return Some(Err(anyhow::anyhow!(
            "env.lock {} is larger than the {}-byte cap",
            path.display(),
            MAX_LOCK_BYTES
        )));
    }
    let parsed = (|| -> Result<EnvLock> {
        let raw =
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let lock: EnvLock = toml::from_str(&raw)
            .with_context(|| format!("env.lock {} is not valid TOML for the schema", path.display()))?;
        Ok(lock)
    })();
    Some(parsed)
}

/// Recursively hash every file under `closure_dir` into a `ClosureEntry` list,
/// with `path` relative to `closure_dir` (forward-slash, closure-internal). Local
/// I/O only — NO network; this is the spawn-time recompute of the on-disk
/// closure's content, fed to [`verify_closure`]. Symlinks are NOT followed (a
/// closure is expected to be self-contained real files; a symlink out would let a
/// closure's content address depend on something outside it). Returns an error if
/// the dir does not exist (a pinned app whose closure was never materialized).
pub fn scan_closure(closure_dir: &Path) -> Result<Vec<ClosureEntry>> {
    let mut out = Vec::new();
    scan_into(closure_dir, closure_dir, &mut out)
        .with_context(|| format!("scanning closure {}", closure_dir.display()))?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn scan_into(root: &Path, dir: &Path, out: &mut Vec<ClosureEntry>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        // symlink_metadata: do NOT follow symlinks (see scan_closure).
        let ft = entry.file_type()?;
        let path = entry.path();
        if ft.is_symlink() {
            // A symlink's target is outside the closure's own bytes; refuse to
            // let it participate in the content address at all.
            anyhow::bail!("closure contains a symlink {} (closures must be self-contained)", path.display());
        }
        if ft.is_dir() {
            scan_into(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = std::fs::read(&path)
                .with_context(|| format!("reading closure file {}", path.display()))?;
            out.push(ClosureEntry {
                path: rel,
                sha256: sha256_hex(&bytes),
                url: String::new(),
            });
        }
    }
    Ok(())
}

/// SHA-256 of bytes, lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

// ===========================================================================
// LIVE spawn gate — pin state + refuse-to-spawn (called from apps.rs launch)
// ===========================================================================

/// An app's pin state at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinState {
    /// No `env.lock` — legacy shared-`.venv` behavior (every app that ships
    /// today). The launcher proceeds exactly as before.
    Unpinned,
    /// An `env.lock` is present. `closure_dir` is the pinned closure's location;
    /// `verdict` decides whether the app may spawn (FAIL-CLOSED on a mismatch).
    Pinned {
        closure_dir: PathBuf,
        verdict: SpawnVerdict,
    },
}

/// Compute an app's [`PinState`] at spawn: read `apps/<name>/env.lock`; if
/// present, locate + scan the materialized closure at `state/envstore/<hash>/`
/// and [`verify_closure`] it FAIL-CLOSED. NO network — only local reads + hashes.
/// A malformed lock, an out-of-range hash, or an unscannable closure all yield a
/// `Refused` verdict (never a silent fallback to the shared `.venv`).
pub fn pin_state(project_root: &Path, app_dir: &Path) -> PinState {
    let lock = match load_lock(app_dir) {
        None => return PinState::Unpinned,
        Some(Ok(lock)) => lock,
        Some(Err(e)) => {
            warn!(app_dir = %app_dir.display(), error = %e, "envlock: unreadable env.lock — refusing to spawn (fail-closed)");
            return PinState::Pinned {
                closure_dir: PathBuf::new(),
                verdict: SpawnVerdict::Refused {
                    reason: RefuseReason::LockInconsistent,
                    closure_hash: String::new(),
                },
            };
        }
    };
    // The recorded hash keys the closure DIR, so it must be a safe hex name
    // before it is ever joined onto a path.
    if !is_hex_hash(&lock.closure_hash) {
        warn!(app_dir = %app_dir.display(), "envlock: env.lock closure_hash is not a valid content address — refusing (fail-closed)");
        return PinState::Pinned {
            closure_dir: PathBuf::new(),
            verdict: SpawnVerdict::Refused {
                reason: RefuseReason::LockInconsistent,
                closure_hash: lock.closure_hash.clone(),
            },
        };
    }
    let cdir = closure_dir(project_root, &lock.closure_hash);
    let actual = match scan_closure(&cdir) {
        Ok(a) => a,
        Err(e) => {
            warn!(closure = %cdir.display(), error = %e, "envlock: cannot scan pinned closure — refusing (fail-closed)");
            return PinState::Pinned {
                closure_dir: cdir,
                verdict: SpawnVerdict::Refused {
                    reason: RefuseReason::ClosureMismatch,
                    closure_hash: String::new(),
                },
            };
        }
    };
    let verdict = verify_closure(&lock, &actual);
    PinState::Pinned {
        closure_dir: cdir,
        verdict,
    }
}

/// The effective interpreter path the launcher should exec, given the legacy
/// interpreter it resolved and the app's [`PinState`]. For a PINNED + VERIFIED
/// python/node app the interpreter is the one INSIDE the pinned closure
/// (`state/envstore/<hash>/bin/python3` | `…/bin/node`) — so both the exec grant
/// and the read grant target the pinned path, never the shared `.venv`. For an
/// unpinned app, a binary app, or an unverified pin (which the launcher refuses
/// separately) this returns the legacy interpreter unchanged. Pure.
pub fn effective_interpreter(pin: &PinState, legacy: &Path, runtime: Runtime) -> PathBuf {
    let closure = match pin {
        PinState::Pinned {
            closure_dir,
            verdict,
        } if verdict.is_allowed() => closure_dir,
        _ => return legacy.to_path_buf(),
    };
    match runtime {
        Runtime::Python => closure.join("bin").join("python3"),
        Runtime::Node => closure.join("bin").join("node"),
        // A binary app is its own interpreter; the closure would contain it, but
        // the entry path already points inside the app dir — leave it to the
        // caller's legacy resolution (no shared-.venv caveat applies to binaries).
        Runtime::Binary => legacy.to_path_buf(),
    }
}

// ===========================================================================
// Telemetry frame (secret-free: closure hash + verdict, never paths/urls)
// ===========================================================================

/// Build the secret-free `envlock.verify` telemetry frame for a pin state:
/// `{app, verdict, closure_hash?, reason?}`. It carries ONLY the app name, the
/// verdict word, and the content-address hash (a public digest) — NEVER a
/// filesystem path, a source URL, or the entry list. Returns `None` for an
/// unpinned app (nothing to report). Pure — the tests assert the field set.
pub fn verdict_frame(app: &str, pin: &PinState) -> Option<(&'static str, Value)> {
    let PinState::Pinned { verdict, .. } = pin else {
        return None;
    };
    let data = match verdict {
        SpawnVerdict::Verified { closure_hash } => json!({
            "app": app,
            "verdict": "verified",
            "closure_hash": closure_hash,
        }),
        SpawnVerdict::Refused {
            reason,
            closure_hash,
        } => json!({
            "app": app,
            "verdict": "refused",
            "reason": reason.as_str(),
            "closure_hash": closure_hash,
        }),
    };
    Some(("envlock.verify", data))
}

/// Emit the [`verdict_frame`] onto telemetry (no-op for an unpinned app). Live
/// wiring called from the app launch path.
pub fn emit_verdict(app: &str, pin: &PinState) {
    if let Some((event, data)) = verdict_frame(app, pin) {
        crate::telemetry::emit("system", event, data);
    }
}

// ===========================================================================
// PURE seam 4 — the fetch/build GATE (user-originated + config, not run on verify)
// ===========================================================================

/// The gate decision for [`env_build`]. Kept pure + separate from the async
/// runner so the GATE is unit-testable WITHOUT a network, and so a test can prove
/// the spawn-time VERIFY path never reaches it (verify does no fetch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildGate {
    /// Not user-originated (a continuation acting on injected content, or an
    /// autonomous tick) — egress-REFUSED regardless of config. Fail-closed on
    /// origin FIRST, exactly like the `open_url`/`web_search` egress guard.
    Refused,
    /// `[envlock].fetch_enabled = false` — the network materialization is off.
    Disabled,
    /// No `env.lock` to materialize — nothing to fetch.
    NoLock,
    /// Both gates pass and a lock is present — the device-gated fetch may run.
    Allowed,
}

/// Decide whether the closure-materialization fetch may run. The user-originated
/// egress gate is checked FIRST (an injected/autonomous build is refused even
/// when the feature is enabled), then the config gate, then lock presence. Pure.
pub fn env_build_gate(fetch_enabled: bool, user_originated: bool, has_lock: bool) -> BuildGate {
    if !user_originated {
        return BuildGate::Refused;
    }
    if !fetch_enabled {
        return BuildGate::Disabled;
    }
    if !has_lock {
        return BuildGate::NoLock;
    }
    BuildGate::Allowed
}

/// The spoken egress refusal for a NON user-originated `env_build`, or `None`
/// when the call is user-originated (allowed). Materializing a closure FETCHES
/// its artifacts over the network, so — exactly like `open_url`/`web_search`/
/// `sage_research` — it must only run for a request the USER made directly (the
/// human running `darwind --env-build <app>`), never as a side effect of injected
/// content or an autonomous tick. Pure, mirrors
/// `anthropic::outward_get_egress_refusal`.
pub fn env_build_egress_refusal(user_originated: bool) -> Option<String> {
    if user_originated {
        None
    } else {
        Some(
            "I won't materialize a dependency closure that wasn't requested directly — \
             building one fetches artifacts over the network, and a hidden instruction \
             could use it to reach out. Ask me to build it directly."
                .to_string(),
        )
    }
}

// ===========================================================================
// DEVICE-GATED runner — the ONE network step (wired; not exercised in tests)
// ===========================================================================

/// How one `env_build` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
    /// Egress-refused (not user-originated). Carries the spoken refusal.
    Refused { message: String },
    /// `[envlock].fetch_enabled = false`.
    Disabled,
    /// The app has no `env.lock` to materialize.
    NoLock,
    /// The closure was materialized + verified at `state/envstore/<hash>/`.
    Built { closure_hash: String },
    /// A materialization failure (bad lock, download/hash mismatch, I/O). The
    /// partial closure is NOT left where a spawn could pick it up.
    Failed { reason: String },
}

/// USER-ORIGINATED, gated closure materialization — the ONE network step in this
/// module. Gated by BOTH [`env_build_gate`] (user-originated egress + the
/// `[envlock].fetch_enabled` config flag) AND per-artifact hash verification. It
/// reads `apps/<name>/env.lock`, self-consistency-checks it, then downloads each
/// entry from its `url`, verifies the bytes against the entry's `sha256`
/// (FAIL-CLOSED — a mismatch aborts), and writes the closure into
/// `state/envstore/<closure_hash>/`. Finally it re-scans + [`verify_closure`]s
/// the result so a Built outcome is one the spawn gate will also accept.
///
/// DEVICE-GATED: the network fetch is wired but never exercised under `cargo test`
/// (the webhook-bind / mic-loop precedent) — the GATE, the hash, the verify, and
/// the narrowing are the pure, tested seams. `install` (authoring the lock) stays
/// a human step exactly like forge's `scripts/apply_forge.sh`.
pub async fn env_build(
    project_root: &Path,
    app_name: &str,
    fetch_enabled: bool,
    user_originated: bool,
) -> BuildOutcome {
    let app_dir = project_root.join("apps").join(app_name);
    let has_lock = envlock_path(&app_dir).exists();
    match env_build_gate(fetch_enabled, user_originated, has_lock) {
        BuildGate::Refused => {
            let message = env_build_egress_refusal(user_originated)
                .unwrap_or_else(|| "env_build refused".to_string());
            warn!(app = app_name, "envlock: refusing a non-user-originated env_build (egress gate)");
            crate::telemetry::emit(
                "system",
                "envlock.build_refused",
                json!({ "app": app_name }),
            );
            return BuildOutcome::Refused { message };
        }
        BuildGate::Disabled => return BuildOutcome::Disabled,
        BuildGate::NoLock => return BuildOutcome::NoLock,
        BuildGate::Allowed => {}
    }

    let lock = match load_lock(&app_dir) {
        Some(Ok(lock)) => lock,
        Some(Err(e)) => return BuildOutcome::Failed { reason: e.to_string() },
        None => return BuildOutcome::NoLock,
    };
    // The lock must be self-consistent + safely-named BEFORE any fetch.
    if !is_hex_hash(&lock.closure_hash) {
        return BuildOutcome::Failed {
            reason: "env.lock closure_hash is not a valid content address".to_string(),
        };
    }
    if compute_closure_hash(&lock.entries) != lock.closure_hash {
        return BuildOutcome::Failed {
            reason: "env.lock is inconsistent (recorded hash != entries)".to_string(),
        };
    }

    match materialize_closure(project_root, &lock).await {
        Ok(()) => {
            info!(app = app_name, hash = %lock.closure_hash, "envlock: closure materialized + verified");
            crate::telemetry::emit(
                "system",
                "envlock.built",
                json!({ "app": app_name, "closure_hash": lock.closure_hash }),
            );
            BuildOutcome::Built {
                closure_hash: lock.closure_hash,
            }
        }
        Err(e) => BuildOutcome::Failed { reason: e.to_string() },
    }
}

/// Download + hash-verify each entry into `state/envstore/<hash>/`, then re-scan
/// and [`verify_closure`] the result. DEVICE-GATED network leg. Writes into a
/// `.partial` staging dir and only promotes it to the final content-addressed
/// dir once the whole closure verifies — a failed/partial fetch never leaves a
/// dir the spawn gate could accept.
async fn materialize_closure(project_root: &Path, lock: &EnvLock) -> Result<()> {
    let final_dir = closure_dir(project_root, &lock.closure_hash);
    if final_dir.exists() {
        // Already materialized: re-verify rather than refetch.
        let actual = scan_closure(&final_dir)?;
        return match verify_closure(lock, &actual) {
            SpawnVerdict::Verified { .. } => Ok(()),
            SpawnVerdict::Refused { reason, .. } => {
                anyhow::bail!("existing closure does not verify: {}", reason.as_str())
            }
        };
    }
    let staging = final_dir.with_extension("partial");
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    std::fs::create_dir_all(&staging)?;

    for entry in &lock.entries {
        if !is_confined_relpath(&entry.path) {
            std::fs::remove_dir_all(&staging).ok();
            anyhow::bail!("lock entry path {:?} is not confined", entry.path);
        }
        if entry.url.trim().is_empty() {
            std::fs::remove_dir_all(&staging).ok();
            anyhow::bail!("lock entry {:?} has no url to fetch from", entry.path);
        }
        let bytes = fetch_bounded(&entry.url).await?;
        let got = sha256_hex(&bytes);
        if got != entry.sha256 {
            std::fs::remove_dir_all(&staging).ok();
            anyhow::bail!(
                "artifact hash mismatch for {:?}: expected {}, got {}",
                entry.path,
                entry.sha256,
                got
            );
        }
        let dest = staging.join(&entry.path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &bytes)?;
    }

    // Re-scan the staged tree and prove it reproduces the pinned content address
    // before promoting it — the same check the spawn gate runs.
    let actual = scan_closure(&staging)?;
    match verify_closure(lock, &actual) {
        SpawnVerdict::Verified { .. } => {}
        SpawnVerdict::Refused { reason, .. } => {
            std::fs::remove_dir_all(&staging).ok();
            anyhow::bail!("materialized closure does not verify: {}", reason.as_str());
        }
    }
    // Atomic-ish promote: rename staging -> final content-addressed dir.
    std::fs::rename(&staging, &final_dir)
        .with_context(|| format!("promoting closure to {}", final_dir.display()))?;
    Ok(())
}

/// GET a single artifact, bounded to [`MAX_ARTIFACT_BYTES`]. Device-gated: only
/// reached from [`materialize_closure`] under a user-originated, config-enabled
/// build; never touched by `cargo test`.
async fn fetch_bounded(url: &str) -> Result<Vec<u8>> {
    use futures_util::StreamExt;
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching {url}"))?
        .error_for_status()
        .with_context(|| format!("non-success status fetching {url}"))?;
    let mut stream = resp.bytes_stream();
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("reading body of {url}"))?;
        if out.len() + chunk.len() > MAX_ARTIFACT_BYTES {
            anyhow::bail!("artifact {url} exceeds the {}-byte cap", MAX_ARTIFACT_BYTES);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

// ===========================================================================
// Tests — the pure seams (hash, verify, narrowing, gate) proven hermetically.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, sha: &str) -> ClosureEntry {
        ClosureEntry {
            path: path.to_string(),
            sha256: sha.to_string(),
            url: String::new(),
        }
    }

    // -- seam 1: content-addressed closure hash -------------------------

    #[test]
    fn closure_hash_is_content_addressed_and_order_independent() {
        let a = vec![entry("bin/python3", "aa"), entry("lib/foo.py", "bb")];
        let b = vec![entry("lib/foo.py", "bb"), entry("bin/python3", "aa")]; // reordered
        assert_eq!(
            compute_closure_hash(&a),
            compute_closure_hash(&b),
            "hash must be independent of entry order"
        );
        // Stable: recomputing the same set yields the same digest.
        assert_eq!(compute_closure_hash(&a), compute_closure_hash(&a));
        // It is a 64-hex-char sha256.
        let h = compute_closure_hash(&a);
        assert_eq!(h.len(), 64);
        assert!(is_hex_hash(&h));
    }

    #[test]
    fn closure_hash_changes_when_any_content_changes() {
        let base = vec![entry("bin/python3", "aa"), entry("lib/foo.py", "bb")];
        let changed_hash = vec![entry("bin/python3", "aa"), entry("lib/foo.py", "cc")];
        let changed_path = vec![entry("bin/python3", "aa"), entry("lib/bar.py", "bb")];
        let extra = vec![
            entry("bin/python3", "aa"),
            entry("lib/foo.py", "bb"),
            entry("lib/baz.py", "dd"),
        ];
        let missing = vec![entry("bin/python3", "aa")];
        let h = compute_closure_hash(&base);
        assert_ne!(h, compute_closure_hash(&changed_hash), "a changed file hash must change the closure hash");
        assert_ne!(h, compute_closure_hash(&changed_path), "a renamed file must change the closure hash");
        assert_ne!(h, compute_closure_hash(&extra), "an extra file must change the closure hash");
        assert_ne!(h, compute_closure_hash(&missing), "a missing file must change the closure hash");
    }

    #[test]
    fn closure_hash_ignores_url_provenance() {
        let mut with_url = vec![entry("bin/python3", "aa")];
        with_url[0].url = "https://mirror.example/py".to_string();
        let without = vec![entry("bin/python3", "aa")];
        assert_eq!(
            compute_closure_hash(&with_url),
            compute_closure_hash(&without),
            "the fetch url is provenance, not part of the content address"
        );
    }

    #[test]
    fn no_field_bleed_between_path_and_hash() {
        // NUL-delimiting means ("ab","c") and ("a","bc") cannot collide.
        let x = vec![entry("ab", "c")];
        let y = vec![entry("a", "bc")];
        assert_ne!(compute_closure_hash(&x), compute_closure_hash(&y));
    }

    // -- seam 2: env.lock verify (FAIL-CLOSED) --------------------------

    fn lock_for(entries: Vec<ClosureEntry>) -> EnvLock {
        let closure_hash = compute_closure_hash(&entries);
        EnvLock { closure_hash, entries }
    }

    #[test]
    fn verify_matches_allows_spawn() {
        let entries = vec![entry("bin/python3", "aa"), entry("lib/foo.py", "bb")];
        let lock = lock_for(entries.clone());
        // The materialized closure is presented in a DIFFERENT order — verify is
        // order-independent, so it still matches.
        let actual = vec![entry("lib/foo.py", "bb"), entry("bin/python3", "aa")];
        let verdict = verify_closure(&lock, &actual);
        assert!(verdict.is_allowed(), "an exact (order-independent) match must allow spawn");
        match verdict {
            SpawnVerdict::Verified { closure_hash } => assert_eq!(closure_hash, lock.closure_hash),
            other => panic!("expected Verified, got {other:?}"),
        }
    }

    #[test]
    fn verify_refuses_on_any_closure_mismatch_failclosed() {
        let lock = lock_for(vec![entry("bin/python3", "aa"), entry("lib/foo.py", "bb")]);
        // Every kind of drift must fail-closed to Refused/ClosureMismatch.
        for actual in [
            vec![entry("bin/python3", "aa"), entry("lib/foo.py", "CHANGED")], // changed hash
            vec![entry("bin/python3", "aa")],                                 // missing file
            vec![
                entry("bin/python3", "aa"),
                entry("lib/foo.py", "bb"),
                entry("lib/extra.py", "zz"),
            ], // extra file
            vec![],                                                           // empty closure
        ] {
            let verdict = verify_closure(&lock, &actual);
            assert!(!verdict.is_allowed(), "a mismatch must refuse to spawn: {actual:?}");
            match verdict {
                SpawnVerdict::Refused { reason, .. } => {
                    assert_eq!(reason, RefuseReason::ClosureMismatch)
                }
                other => panic!("expected Refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn verify_refuses_a_tampered_lock_failclosed() {
        // A lock whose recorded hash was hand-edited to a wrong value must be
        // rejected as inconsistent BEFORE the materialized closure is even trusted
        // — even if the actual closure happens to match the (tampered) hash.
        let entries = vec![entry("bin/python3", "aa")];
        let mut lock = lock_for(entries.clone());
        lock.closure_hash = "deadbeef".to_string(); // != compute_closure_hash(entries)
        let verdict = verify_closure(&lock, &entries);
        assert!(!verdict.is_allowed());
        match verdict {
            SpawnVerdict::Refused { reason, .. } => assert_eq!(reason, RefuseReason::LockInconsistent),
            other => panic!("expected LockInconsistent, got {other:?}"),
        }
    }

    // -- seam 3: SBPL narrowing (pinned path, NOT the shared .venv) ------

    #[test]
    fn pinned_closure_of_detects_envstore_interpreter() {
        let root = Path::new("/Users/test/darwin");
        let hash = "a".repeat(64);
        let pinned_interp = root.join(format!("state/envstore/{hash}/bin/python3"));
        assert_eq!(
            pinned_closure_of(root, &pinned_interp),
            Some(root.join(format!("state/envstore/{hash}"))),
            "an interpreter under the env store resolves to its closure dir"
        );
        // The shared venv interpreter is NOT pinned.
        assert_eq!(pinned_closure_of(root, &root.join(".venv/bin/python3")), None);
        // A non-hex subdir under envstore is rejected (no traversal via a crafted name).
        assert_eq!(
            pinned_closure_of(root, &root.join("state/envstore/../evil/bin/python3")),
            None
        );
    }

    #[test]
    fn narrowing_grants_pinned_path_not_shared_venv() {
        let root = Path::new("/Users/test/darwin");
        let hash = "b".repeat(64);
        let pinned_interp = root.join(format!("state/envstore/{hash}/bin/python3"));
        let venv_interp = root.join(".venv/bin/python3");

        let pinned_root = python_runtime_read_root(root, &pinned_interp);
        let legacy_root = python_runtime_read_root(root, &venv_interp);

        // PINNED: the read root is the closure dir — and it is NOT the shared .venv.
        assert_eq!(pinned_root, root.join(format!("state/envstore/{hash}")));
        assert_ne!(pinned_root, root.join(".venv"));
        assert!(!pinned_root.starts_with(root.join(".venv")), "pinned reach excludes the shared .venv");
        // UNPINNED: byte-for-byte the legacy .venv grant.
        assert_eq!(legacy_root, root.join(".venv"));

        // STRICTLY NARROWER: the pinned root is confined to this ONE app's closure
        // under the env store; the shared .venv is not reachable from it and vice
        // versa (disjoint), so no python app's grant can reach another's closure.
        assert!(pinned_root.starts_with(envstore_root(root)));
        assert!(!legacy_root.starts_with(envstore_root(root)));
    }

    #[test]
    fn generated_profile_grants_pinned_path_not_venv() {
        // Prove the WIRED generator (apps::generate_sbpl) narrows: with a pinned
        // interpreter the emitted profile grants exec/read of the closure path and
        // does NOT grant read of the shared project .venv.
        use crate::apps::{generate_sbpl, AppManifest};
        let root = Path::new("/Users/test/darwin");
        let hash = "c".repeat(64);
        let closure = root.join(format!("state/envstore/{hash}"));
        let interp = closure.join("bin/python3");
        let app_dir = root.join("apps/pinned-app");
        let sock = root.join("state/ipc/apps/pinned-app.sock");
        let manifest: AppManifest = toml::from_str(
            "[app]\nname=\"pinned-app\"\nversion=\"0.1.0\"\ndescription=\"d\"\n\
             entry=\"apps/pinned-app/main.py\"\nruntime=\"python\"\n",
        )
        .unwrap();
        let profile = generate_sbpl(&manifest, root, &interp, &app_dir, &sock);
        let closure_s = closure.to_string_lossy();
        // Exec + read of the pinned closure path.
        assert!(
            profile.contains(&format!("(allow process-exec* (literal \"{closure_s}/bin/python3\"))")),
            "must exec the pinned interpreter: {profile}"
        );
        assert!(
            profile.contains(&format!("(allow file-read* (subpath \"{closure_s}\"))")),
            "must read the pinned closure: {profile}"
        );
        // And NOT the shared .venv (the caveat this closes).
        assert!(
            !profile.contains("(allow file-read* (subpath \"/Users/test/darwin/.venv\"))"),
            "a pinned app must NOT be granted the shared .venv: {profile}"
        );
    }

    #[test]
    fn effective_interpreter_pins_only_when_verified() {
        let root = Path::new("/Users/test/darwin");
        let hash = "d".repeat(64);
        let legacy = root.join(".venv/bin/python3");
        let verified = PinState::Pinned {
            closure_dir: root.join(format!("state/envstore/{hash}")),
            verdict: SpawnVerdict::Verified { closure_hash: hash.clone() },
        };
        let refused = PinState::Pinned {
            closure_dir: root.join(format!("state/envstore/{hash}")),
            verdict: SpawnVerdict::Refused {
                reason: RefuseReason::ClosureMismatch,
                closure_hash: hash.clone(),
            },
        };
        // Verified python pin -> the pinned interpreter.
        assert_eq!(
            effective_interpreter(&verified, &legacy, Runtime::Python),
            root.join(format!("state/envstore/{hash}/bin/python3"))
        );
        // Unverified pin -> legacy (the launcher refuses the spawn separately).
        assert_eq!(effective_interpreter(&refused, &legacy, Runtime::Python), legacy);
        // Unpinned -> legacy.
        assert_eq!(effective_interpreter(&PinState::Unpinned, &legacy, Runtime::Python), legacy);
    }

    // -- seam 4: the fetch is GATED (and not run on verify) --------------

    #[test]
    fn fetch_is_refused_unless_user_originated() {
        // Non-user-originated => Refused, regardless of config / lock presence.
        assert_eq!(env_build_gate(true, false, true), BuildGate::Refused);
        assert_eq!(env_build_gate(false, false, true), BuildGate::Refused);
        assert!(env_build_egress_refusal(false).is_some(), "a non-user-originated build has a refusal");
        // User-originated but disabled / no lock => not Allowed (no fetch).
        assert_eq!(env_build_gate(false, true, true), BuildGate::Disabled);
        assert_eq!(env_build_gate(true, true, false), BuildGate::NoLock);
        // Only user-originated + enabled + a lock reaches Allowed.
        assert_eq!(env_build_gate(true, true, true), BuildGate::Allowed);
        assert!(env_build_egress_refusal(true).is_none(), "a user-originated build is not refused");
    }

    #[test]
    fn verify_path_never_triggers_a_fetch() {
        // The spawn-time verify (seam 2) is a PURE function over already-known
        // entries: it takes no url, opens no socket, and cannot reach env_build.
        // Proven structurally here — verifying a matching closure returns a
        // verdict with no network, and the gate that guards the fetch is a
        // SEPARATE function the verify never calls.
        let lock = lock_for(vec![entry("bin/python3", "aa")]);
        let verdict = verify_closure(&lock, &lock.entries);
        assert!(verdict.is_allowed());
        // The fetch gate is independent: verify producing Verified does not imply
        // Allowed — a fetch still needs its own user-originated + enabled gate.
        assert_eq!(env_build_gate(true, false, true), BuildGate::Refused);
    }

    // -- telemetry frame is secret-free ---------------------------------

    #[test]
    fn verdict_frame_is_secret_free() {
        let hash = "e".repeat(64);
        let verified = PinState::Pinned {
            closure_dir: PathBuf::from("/Users/test/darwin/state/envstore").join(&hash),
            verdict: SpawnVerdict::Verified { closure_hash: hash.clone() },
        };
        let (event, data) = verdict_frame("pinned-app", &verified).expect("pinned app emits a frame");
        assert_eq!(event, "envlock.verify");
        // ONLY the allowed keys — never a path, url, or the entry list.
        let obj = data.as_object().unwrap();
        let keys: std::collections::BTreeSet<&str> = obj.keys().map(String::as_str).collect();
        let allowed: std::collections::BTreeSet<&str> =
            ["app", "verdict", "closure_hash", "reason"].into_iter().collect();
        assert!(keys.is_subset(&allowed), "frame keys {keys:?} must be within the allowed set");
        assert_eq!(obj["verdict"], "verified");
        assert_eq!(obj["closure_hash"], hash);
        // The serialized frame must not leak the closure DIRECTORY PATH.
        assert!(!data.to_string().contains("/state/envstore/"), "no filesystem path in the frame");

        // A refused frame carries the reason word, still secret-free.
        let refused = PinState::Pinned {
            closure_dir: PathBuf::new(),
            verdict: SpawnVerdict::Refused {
                reason: RefuseReason::ClosureMismatch,
                closure_hash: hash.clone(),
            },
        };
        let (_e, rdata) = verdict_frame("pinned-app", &refused).unwrap();
        assert_eq!(rdata["verdict"], "refused");
        assert_eq!(rdata["reason"], "closure_mismatch");
        // Unpinned apps emit nothing.
        assert!(verdict_frame("legacy-app", &PinState::Unpinned).is_none());
    }

    // -- pin_state end-to-end over a real on-disk closure ----------------

    #[test]
    fn pin_state_verifies_then_refuses_a_tampered_closure() {
        let tmp = std::env::temp_dir().join(format!("envlock_pin_{}", std::process::id()));
        let root = tmp.join("root");
        let app_dir = root.join("apps").join("demo");
        std::fs::create_dir_all(&app_dir).unwrap();

        // Materialize a two-file closure on disk, hash it, and write the env.lock.
        let entries: Vec<(&str, Vec<u8>)> = vec![
            ("bin/python3", b"#!fake-interp\n".to_vec()),
            ("lib/mod.py", b"x = 1\n".to_vec()),
        ];
        // Compute the content address from the real bytes, then lay the closure
        // down under state/envstore/<hash>/.
        let scanned: Vec<ClosureEntry> = entries
            .iter()
            .map(|(p, b)| ClosureEntry { path: (*p).to_string(), sha256: sha256_hex(b), url: String::new() })
            .collect();
        let chash = compute_closure_hash(&scanned);
        let cdir = closure_dir(&root, &chash);
        for (p, b) in &entries {
            let dest = cdir.join(p);
            std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
            std::fs::write(&dest, b).unwrap();
        }
        // env.lock (TOML) with the recorded closure_hash + entries.
        let mut lock_toml = format!("closure_hash = \"{chash}\"\n");
        for e in &scanned {
            lock_toml.push_str(&format!(
                "\n[[entries]]\npath = \"{}\"\nsha256 = \"{}\"\n",
                e.path, e.sha256
            ));
        }
        std::fs::write(envlock_path(&app_dir), &lock_toml).unwrap();

        // (1) A faithful closure verifies.
        match pin_state(&root, &app_dir) {
            PinState::Pinned { verdict, closure_dir: cd } => {
                assert!(verdict.is_allowed(), "faithful closure must verify");
                assert_eq!(cd, cdir);
            }
            other => panic!("expected Pinned, got {other:?}"),
        }

        // (2) Tamper one byte on disk -> fail-closed refuse.
        std::fs::write(cdir.join("lib/mod.py"), b"x = 999\n").unwrap();
        match pin_state(&root, &app_dir) {
            PinState::Pinned { verdict, .. } => assert!(!verdict.is_allowed(), "a tampered closure must refuse"),
            other => panic!("expected Pinned, got {other:?}"),
        }

        // (3) No env.lock at all -> Unpinned (legacy).
        std::fs::remove_file(envlock_path(&app_dir)).unwrap();
        assert_eq!(pin_state(&root, &app_dir), PinState::Unpinned);

        std::fs::remove_dir_all(&tmp).ok();
    }
}
