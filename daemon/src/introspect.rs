//! Micro-app introspection — DEFENSIVE, READ-ONLY self-observation.
//!
//! darwind already OWNS the processes it wants to watch: `apps.rs` spawns each
//! micro-app as a same-UID child under `sandbox-exec` and holds its `Child`. So
//! this subsystem needs NONE of the heavy macOS observation machinery — no
//! Endpoint Security (which needs root, the restricted
//! `com.apple.developer.endpoint-security.client` entitlement, Full Disk Access,
//! and a notarized host), no `task_for_pid`/Mach ports (which need
//! `com.apple.security.cs.debugger` and, on hardened/Apple-signed targets, would
//! not even yield a port), and no `ptrace` (an adversarial, exclusive, target-
//! stopping facility on macOS). It observes its own children the cheap way:
//!
//!   1. **SBPL profile-drift detection.** At profile-write time we fingerprint
//!      exactly what was written (SHA-256, `sha2` is already a dep); the sentinel
//!      re-reads the on-disk `state/apps/<name>/<name>.sb` and flags any
//!      post-launch tampering. Pure, CI-tested.
//!   2. **Resource sampling.** Per-app RSS / CPU via `sysinfo` (already a dep) —
//!      same-UID, no entitlement — classified against a rolling per-app baseline.
//!      The classifier is pure and CI-tested; the live sample is device-gated.
//!
//! Everything relays through the EXISTING `telemetry::emit("system", …)` bus
//! (byte-identical envelope shape to `app.data`/`app.log`), so the HUD renders it
//! with no protocol change, and `posture.rs` can fold the anomaly counts into its
//! read-only report. This module has NO actuator: it never signals, kills,
//! ptraces, injects, or writes config — it reads and reports. Reacting to a
//! finding (e.g. tightening a profile) would be CONSEQUENTIAL and must ride the
//! existing confirm + voice-id + policy + lockdown gates, exactly like `heal.rs`
//! is PROPOSE-ONLY. See docs/INTROSPECT.md.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tracing::debug;

// The sentinel cadence (startup delay + interval) and anomaly thresholds are
// configurable via `[introspect]` (config.rs); the defaults there match this
// subsystem's original constants, so an absent section behaves as before.

// ===========================================================================
// Pure cores (no I/O — unit-tested directly)
// ===========================================================================

/// SHA-256 hex fingerprint of a seatbelt profile's bytes. Stable and
/// deterministic — the drift detector compares fingerprints, never raw strings,
/// so the emitted telemetry carries a short digest rather than the whole profile.
pub fn sbpl_fingerprint(profile: &str) -> String {
    let mut h = Sha256::new();
    h.update(profile.as_bytes());
    hex::encode(h.finalize())
}

/// A detected mismatch between the profile darwind wrote and what is on disk now.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileDrift {
    pub app: String,
    pub expected_fp: String,
    pub actual_fp: String,
}

/// Compare the recorded expected fingerprint against the current on-disk profile
/// contents. `Some` iff they differ — i.e. the profile was edited after darwind
/// wrote it (a same-UID tamper of `state/apps/<name>/<name>.sb`). Pure.
pub fn detect_profile_drift(app: &str, expected_fp: &str, on_disk: &str) -> Option<ProfileDrift> {
    let actual_fp = sbpl_fingerprint(on_disk);
    if actual_fp == expected_fp {
        None
    } else {
        Some(ProfileDrift {
            app: app.to_string(),
            expected_fp: expected_fp.to_string(),
            actual_fp,
        })
    }
}

/// One resource reading of a micro-app process (same-UID, via sysinfo).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResourceSample {
    /// Resident set size in bytes.
    pub rss_bytes: u64,
    /// CPU usage percent since the previous refresh.
    pub cpu_percent: f32,
}

/// Thresholds for the anomaly classifier. Conservative by default: only clearly
/// abnormal drift trips a signal, so the HUD is not spammed on normal variation.
#[derive(Debug, Clone, Copy)]
pub struct AnomalyThresholds {
    /// RSS growth multiple over baseline that counts as a leak/runaway.
    pub rss_growth_ratio: f64,
    /// Ignore RSS growth below this absolute floor (avoids noise on tiny procs).
    pub rss_floor_bytes: u64,
    /// Sustained CPU percent above this counts as a spin/runaway.
    pub cpu_percent: f32,
}

impl Default for AnomalyThresholds {
    fn default() -> Self {
        Self {
            rss_growth_ratio: 3.0,
            rss_floor_bytes: 64 * 1024 * 1024, // 64 MiB
            cpu_percent: 95.0,
        }
    }
}

impl AnomalyThresholds {
    /// Build thresholds from the tunable `[introspect]` config values, keeping the
    /// RSS noise floor at its default (not user-exposed — it only suppresses noise
    /// on tiny processes and has no security-sensitivity meaning).
    pub fn from_config(rss_growth_ratio: f64, cpu_percent: f32) -> Self {
        Self {
            rss_growth_ratio,
            cpu_percent,
            ..Self::default()
        }
    }
}

/// A classified anomaly — informational only (surfaced to the HUD/posture; never
/// acted on here).
#[derive(Debug, Clone, PartialEq)]
pub struct Anomaly {
    pub app: String,
    pub kind: &'static str,
    pub detail: String,
}

/// Classify a current sample against a per-app baseline. Pure — the sentinel
/// seeds a baseline on first observation (no classify) and only calls this on
/// subsequent ticks. Returns every anomaly the sample trips (possibly empty).
pub fn classify_anomalies(
    app: &str,
    baseline: &ResourceSample,
    current: &ResourceSample,
    th: &AnomalyThresholds,
) -> Vec<Anomaly> {
    let mut out = Vec::new();
    // RSS growth: only when we have a real baseline, the current reading is above
    // the noise floor, and it exceeds the baseline by the configured multiple.
    if baseline.rss_bytes > 0
        && current.rss_bytes > th.rss_floor_bytes
        && (current.rss_bytes as f64) > (baseline.rss_bytes as f64) * th.rss_growth_ratio
    {
        out.push(Anomaly {
            app: app.to_string(),
            kind: "rss_growth",
            detail: format!(
                "rss {} -> {} bytes (> {:.1}x baseline)",
                baseline.rss_bytes, current.rss_bytes, th.rss_growth_ratio
            ),
        });
    }
    if current.cpu_percent > th.cpu_percent {
        out.push(Anomaly {
            app: app.to_string(),
            kind: "cpu_spike",
            detail: format!(
                "cpu {:.0}% > {:.0}% threshold",
                current.cpu_percent, th.cpu_percent
            ),
        });
    }
    out
}

/// What the sentinel should do with an app's resource baseline this tick. Pure —
/// extracted from `sentinel_tick` (which is runtime-only and untested) so the
/// pid-change reseed + classify + baseline-advance logic — where a real bug hid
/// (a relaunched process judged against the old process's baseline) — IS tested.
#[derive(Debug, PartialEq)]
enum BaselineOutcome {
    /// No comparable baseline (cold start OR the pid changed = a relaunch): seed
    /// silently, never alert. The caller stores `(pid, sample)`.
    Seed,
    /// Same process, within baseline: advance the baseline (store `(pid, sample)`).
    UpdateHealthy,
    /// Same process, anomalous: report these anomalies but HOLD the baseline (do
    /// not advance), so a genuine runaway keeps tripping instead of the baseline
    /// creeping up to absorb it.
    HoldAnomalous(Vec<Anomaly>),
}

/// Decide the baseline action for one app given its stored `(pid, baseline)` (if
/// any) and the current `(pid, sample)`. Only a baseline seeded for the SAME pid
/// is comparable — a relaunch (same name, new pid) re-seeds. PURE.
fn resource_decision(
    name: &str,
    stored: Option<(u32, ResourceSample)>,
    pid: u32,
    sample: ResourceSample,
    thresholds: &AnomalyThresholds,
) -> BaselineOutcome {
    match stored {
        Some((base_pid, base)) if base_pid == pid => {
            let anomalies = classify_anomalies(name, &base, &sample, thresholds);
            if anomalies.is_empty() {
                BaselineOutcome::UpdateHealthy
            } else {
                BaselineOutcome::HoldAnomalous(anomalies)
            }
        }
        _ => BaselineOutcome::Seed, // cold start OR a pid change (relaunch)
    }
}

// ===========================================================================
// dyld module attestation (pure core + trust-on-first-use baseline)
// ===========================================================================
//
// COOPERATIVE ATTESTATION — honest scope. A micro-app's in-proc SDK stub reports
// its loaded-module set (dyld `_dyld_get_image_name` + LC_UUID) over the EXISTING
// HMAC-tokened per-app socket; the daemon attests it against a per-app baseline.
// Because the socket is authenticated, a DIFFERENT process cannot forge a report,
// so this reliably catches injection into an otherwise-honest app (a rogue
// DYLD_INSERT, an unexpected dlopen) and gives an auditable inventory. It is NOT a
// defense against a FULLY-compromised app that lies about its own modules — that
// deeper compromise is bounded by the sandbox + token model, and the tamper-
// resistant out-of-process path (task_for_pid → dyld_all_image_infos) is deferred
// because it needs com.apple.security.cs.debugger, which does not even yield ports
// for darwind's own hardened processes. See docs/INTROSPECT.md.

/// Cap on modules parsed from one report (bounds a hostile/oversized payload).
const MAX_MODULES: usize = 8192;

/// One loaded module an app reported: its (logical) dyld image path and, when the
/// stub parsed LC_UUID, the build UUID. On Apple Silicon most system dylibs live
/// in the shared cache with no standalone file, so the path is a logical name —
/// the UUID is the tamper-resistant identity when present (a spoofed path alone
/// would slip past a path-only baseline).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub path: String,
    pub uuid: Option<String>,
}

impl Module {
    /// Canonical identity key `path|uuid`. Same image collapses to one key; a path
    /// whose UUID changed is a DIFFERENT module (the swap an attacker would try).
    fn key(&self) -> String {
        format!("{}|{}", self.path, self.uuid.as_deref().unwrap_or(""))
    }
}

/// Parse an app's `modules` report (`data.modules = [{path, uuid?}, …]`) into a
/// bounded, de-duplicated module list. Pure; drops malformed/empty-path entries.
pub fn parse_module_report(data: &Value) -> Vec<Module> {
    let Some(arr) = data.get("modules").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for item in arr.iter().take(MAX_MODULES) {
        let Some(path) = item.get("path").and_then(Value::as_str) else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        let uuid = item
            .get("uuid")
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string);
        let m = Module {
            path: path.to_string(),
            uuid,
        };
        if seen.insert(m.key()) {
            out.push(m);
        }
    }
    out
}

/// The result of attesting an observed module set against a baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleAttestation {
    pub total: usize,
    /// Observed modules NOT in the baseline — the injection / unexpected-dlopen
    /// signal (the load-bearing finding).
    pub unexpected: Vec<Module>,
    /// Count of baseline modules not observed now — informational (a dlclose'd
    /// dylib); never treated as a violation.
    pub missing_count: usize,
}

/// Attest an observed module set against a baseline key-set. Pure. `unexpected`
/// is every observed module the baseline never had.
pub fn attest_modules(baseline: &BTreeSet<String>, observed: &[Module]) -> ModuleAttestation {
    let observed_keys: BTreeSet<String> = observed.iter().map(Module::key).collect();
    let unexpected: Vec<Module> = observed
        .iter()
        .filter(|m| !baseline.contains(&m.key()))
        .cloned()
        .collect();
    let missing_count = baseline.iter().filter(|k| !observed_keys.contains(*k)).count();
    ModuleAttestation {
        total: observed.len(),
        unexpected,
        missing_count,
    }
}

/// name -> baseline module key-set (trust-on-first-use anchor).
fn module_baselines() -> &'static Mutex<HashMap<String, BTreeSet<String>>> {
    static M: OnceLock<Mutex<HashMap<String, BTreeSet<String>>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Session total of module violations, folded into `posture_line`.
static MODULE_VIOLATIONS: AtomicUsize = AtomicUsize::new(0);

/// Trust-on-first-use: the FIRST module report for an app SEEDS its baseline and
/// returns `None` (a first sighting never alerts). Every LATER report is attested
/// against that seeded anchor — the baseline is deliberately NOT widened by later
/// reports, so a module injected after the first report keeps tripping (the same
/// "dedup vs the seeded set, not vs confirmed" discipline used elsewhere). The
/// session violation counter is advanced by the unexpected count.
pub fn attest_or_seed(name: &str, observed: &[Module]) -> Option<ModuleAttestation> {
    // An empty report carries no information: never SEED a baseline from it (an
    // empty first baseline would flag every real module as "unexpected" on the
    // next report) and never attest against it. Wait for a non-empty report.
    if observed.is_empty() {
        return None;
    }
    let mut map = module_baselines().lock().ok()?;
    match map.get(name) {
        None => {
            let seed: BTreeSet<String> = observed.iter().map(Module::key).collect();
            map.insert(name.to_string(), seed);
            None
        }
        Some(baseline) => {
            let att = attest_modules(baseline, observed);
            if !att.unexpected.is_empty() {
                MODULE_VIOLATIONS.fetch_add(att.unexpected.len(), Ordering::Relaxed);
            }
            Some(att)
        }
    }
}

/// Drop an app's seeded module baseline so its NEXT report re-seeds (trust-on-
/// first-use again). Called by `apps.rs` on every (re)launch: a legitimately
/// updated app loads a different module set, so persisting the old baseline
/// across a relaunch would false-flag every changed module as an injection. Each
/// launch is a fresh trust anchor; injection is caught WITHIN a launch, not
/// across a restart the daemon itself performed.
pub fn reset_module_baseline(name: &str) {
    if let Ok(mut map) = module_baselines().lock() {
        map.remove(name);
    }
}

// ===========================================================================
// Kernel security-event classification (the ES seam)
// ===========================================================================
//
// This is the PURE, CI-tested brain a future Endpoint Security NOTIFY client
// would drive. The live ES front-end is DEVICE-GATED and DEFERRED (it needs root
// + the restricted `com.apple.developer.endpoint-security.client` entitlement +
// Full Disk Access + a notarized host, and must subscribe NOTIFY-only), so it is
// NOT built here — but the classification it feeds, and the ingestion seam it
// plugs into, are real and tested now. See docs/INTROSPECT.md. This ties the
// three focus areas together: the W^X / `jit` manifest key (an app that makes
// memory executable but declared `jit=false` is a violation), and the arm64
// "someone acquired my task port" attach/inject signal.

/// A semantic security event about one of darwind's tracked apps. Produced by the
/// (deferred, device-gated) ES NOTIFY client — or, today, by tests.
// ES SEAM: intentionally has no LIVE caller yet — the ES NOTIFY front-end that
// produces these is deferred (needs the restricted entitlement + FDA + notarized
// root host). The classifier + ingestion below are exercised by tests and ready
// for that front-end. allow(dead_code) so the binary build stays warning-clean.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityEvent {
    /// `mprotect(..., PROT_EXEC)` — a page was made executable (a W^X flip toward X).
    MprotectExec,
    /// `mmap(..., MAP_JIT)` — a JIT-eligible executable mapping was created.
    MapJit,
    /// Another process acquired this app's task port (`GET_TASK`/`GET_TASK_READ`):
    /// the arm64 "a debugger/injector is attaching" signal.
    GetTask { by_pid: i32, by_path: String },
    /// A signal was delivered to this app by `by_pid`.
    Signal { signal: i32, by_pid: i32 },
}

/// A classified security finding — informational only (surfaced to the HUD/posture
/// and the findings ring; never acted on here).
#[allow(dead_code)] // ES seam (see SecurityEvent) — tested; awaits the live ES front-end.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityFinding {
    pub app: String,
    pub kind: &'static str,
    /// True for a strong compromise signal (W^X violation, task-port acquisition).
    pub high: bool,
    pub detail: String,
}

/// Classify a security event about `app`. `jit_declared` is whether the app's
/// manifest declared `jit=true` (so an executable/JIT mapping is EXPECTED, not a
/// violation). Returns a finding when noteworthy, else `None` (benign/expected).
/// PURE.
#[allow(dead_code)] // ES seam (see SecurityEvent) — tested; awaits the live ES front-end.
pub fn classify_security_event(
    app: &str,
    jit_declared: bool,
    ev: &SecurityEvent,
) -> Option<SecurityFinding> {
    match ev {
        // A jit=false app creating executable / JIT memory is a W^X violation: it
        // declared no JIT, and the sandbox + arm64 W^X + the missing allow-jit
        // entitlement should have blocked it — so observing it succeed is a strong
        // compromise signal. A jit=true app doing this is expected → no finding.
        SecurityEvent::MprotectExec | SecurityEvent::MapJit => {
            if jit_declared {
                None
            } else {
                let op = if matches!(ev, SecurityEvent::MapJit) {
                    "mapped MAP_JIT executable memory"
                } else {
                    "made a page executable (mprotect PROT_EXEC)"
                };
                Some(SecurityFinding {
                    app: app.to_string(),
                    kind: "wx_violation",
                    high: true,
                    detail: format!("{app} {op} but its manifest declares jit=false"),
                })
            }
        }
        // Someone acquired this app's task port — the arm64 attach/inject signal.
        SecurityEvent::GetTask { by_pid, by_path } => Some(SecurityFinding {
            app: app.to_string(),
            kind: "task_port_acquired",
            high: true,
            detail: format!(
                "pid {by_pid} ({by_path}) acquired {app}'s task port (possible debugger/injector)"
            ),
        }),
        // A signal to a supervised app — a notice (the daemon itself signals on
        // stop/restart; a signal from an UNEXPECTED sender is what matters, but the
        // sender policy lives in the live front-end; here we surface it as low).
        SecurityEvent::Signal { signal, by_pid } => Some(SecurityFinding {
            app: app.to_string(),
            kind: "signal",
            high: false,
            detail: format!("{app} received signal {signal} from pid {by_pid}"),
        }),
    }
}

/// Feed a security event through the classifier and, if it produces a finding,
/// record it (findings ring) and emit `introspect.security_event`. This is the
/// SEAM the deferred ES NOTIFY client plugs into; it is exercised by tests today.
/// READ-ONLY — it reports; it never blocks/kills/responds (an ES observer must be
/// NOTIFY-only, never AUTH).
#[allow(dead_code)] // ES seam (see SecurityEvent) — tested; awaits the live ES front-end.
pub fn ingest_security_event(app: &str, jit_declared: bool, ev: &SecurityEvent) {
    if let Some(f) = classify_security_event(app, jit_declared, ev) {
        // Finding ring is user/cloud-facing -> redact the home prefix (username);
        // the telemetry envelope keeps the full detail for local forensics.
        record_finding(redact_home(&format!("{}: {}", f.kind, f.detail)));
        let (event, payload) = ev_security(&f.app, f.kind, f.high, &f.detail);
        crate::telemetry::emit("system", event, payload);
    }
}

// ===========================================================================
// introspect.* telemetry contract — the SINGLE SOURCE OF TRUTH for the event
// names and field names the HUD parsers (hud/src/core/events.ts) read.
// ===========================================================================
//
// Every introspect envelope is built here, and the tests at the bottom of this
// file assert every builder's exact shape. This anchors the cross-language wire
// contract: a field rename now breaks a daemon test instead of silently making
// the HUD drop the event (the `module_violation` "name"/"app" bug that two unit
// suites each missed in isolation). INVARIANT: every SINGLE-APP event keys the
// app on "app" — NOT the "name" convention of the app.data/app.log relay.

pub const EV_SNAPSHOT: &str = "introspect.snapshot";
pub const EV_CAPABILITIES: &str = "introspect.capabilities";
pub const EV_PROFILE_DRIFT: &str = "introspect.profile_drift";
pub const EV_ANOMALY: &str = "introspect.anomaly";
pub const EV_MODATTEST: &str = "introspect.modattest";
pub const EV_MODULE_VIOLATION: &str = "introspect.module_violation";
#[allow(dead_code)] // ES seam — only emitted by the deferred live ES front-end (tested).
pub const EV_SECURITY: &str = "introspect.security_event";

pub fn ev_snapshot(apps: usize, drift: usize, anomalies: usize) -> (&'static str, serde_json::Value) {
    (EV_SNAPSHOT, json!({"apps": apps, "drift": drift, "anomalies": anomalies}))
}

pub fn ev_capabilities(caps: &[(String, String)]) -> (&'static str, serde_json::Value) {
    let apps: Vec<Value> = caps.iter().map(|(n, c)| json!({"name": n, "caps": c})).collect();
    (EV_CAPABILITIES, json!({"apps": apps}))
}

pub fn ev_profile_drift(app: &str, expected_fp: &str, actual_fp: &str) -> (&'static str, serde_json::Value) {
    (EV_PROFILE_DRIFT, json!({"app": app, "expected_fp": expected_fp, "actual_fp": actual_fp}))
}

pub fn ev_profile_missing(app: &str) -> (&'static str, serde_json::Value) {
    (EV_PROFILE_DRIFT, json!({"app": app, "missing": true}))
}

pub fn ev_anomaly(app: &str, kind: &str, detail: &str) -> (&'static str, serde_json::Value) {
    (EV_ANOMALY, json!({"app": app, "kind": kind, "detail": detail}))
}

pub fn ev_module_violation(app: &str, path: &str, uuid: &Option<String>) -> (&'static str, serde_json::Value) {
    (EV_MODULE_VIOLATION, json!({"app": app, "path": path, "uuid": uuid}))
}

pub fn ev_modattest(
    app: &str,
    modules: usize,
    unexpected: usize,
    missing: usize,
    seeded: bool,
) -> (&'static str, serde_json::Value) {
    (
        EV_MODATTEST,
        json!({"app": app, "modules": modules, "unexpected": unexpected, "missing": missing, "seeded": seeded}),
    )
}

#[allow(dead_code)] // ES seam — only called by the deferred live ES front-end (tested).
pub fn ev_security(app: &str, kind: &str, high: bool, detail: &str) -> (&'static str, serde_json::Value) {
    (EV_SECURITY, json!({"app": app, "kind": kind, "high": high, "detail": detail}))
}

// ===========================================================================
// Process-global registries (populated by apps.rs; read by the sentinel)
// ===========================================================================

/// name -> SHA-256 fingerprint of the profile darwind last WROTE for the app.
fn expected_profiles() -> &'static Mutex<HashMap<String, String>> {
    static M: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// name -> live child pid (only while the app is running; cleared by `PidGuard`).
fn child_pids() -> &'static Mutex<HashMap<String, u32>> {
    static M: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record the fingerprint of the profile just written for `name`. Called from
/// `apps.rs::write_profile` after a successful write.
pub fn record_profile(name: &str, profile: &str) {
    if let Ok(mut m) = expected_profiles().lock() {
        m.insert(name.to_string(), sbpl_fingerprint(profile));
    }
}

/// Record the running child's pid and return a guard that CLEARS it on drop.
/// `apps.rs::run_once` holds the guard for the child's lifetime, so every return
/// path (stop / exit / error) removes the pid — a dead/OS-reused pid is never
/// sampled. A `None` pid records nothing but still yields a (no-op) guard.
#[must_use = "hold the PidGuard for the child's lifetime so the pid is cleared on exit"]
pub fn record_child(name: &str, pid: Option<u32>) -> PidGuard {
    if let Some(pid) = pid {
        if let Ok(mut m) = child_pids().lock() {
            m.insert(name.to_string(), pid);
        }
    }
    PidGuard {
        name: name.to_string(),
        pid,
    }
}

/// Clears an app's recorded pid when dropped (RAII, mirrors `kill_on_drop`).
pub struct PidGuard {
    name: String,
    /// The pid THIS guard recorded (None if `record_child` got no pid).
    pid: Option<u32>,
}

impl Drop for PidGuard {
    fn drop(&mut self) {
        // Remove the entry only if it still holds the exact pid this guard
        // recorded — so a guard that recorded nothing (None), or a stale guard,
        // can never clear a newer launch's pid out from under the sentinel.
        let Some(pid) = self.pid else { return };
        if let Ok(mut m) = child_pids().lock() {
            if m.get(&self.name) == Some(&pid) {
                m.remove(&self.name);
            }
        }
    }
}

/// Snapshot of the pid map (owned clone — never hold the lock across `.await`).
fn snapshot_pids() -> HashMap<String, u32> {
    child_pids()
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default()
}

/// Snapshot of the expected-profile fingerprints (owned clone).
fn snapshot_expected() -> HashMap<String, String> {
    expected_profiles()
        .lock()
        .map(|m| m.clone())
        .unwrap_or_default()
}

/// name -> whether the app declared `jit=true` (from its manifest), so the ES
/// front-end can tell an EXPECTED executable mapping from a W^X violation.
fn jit_declared() -> &'static Mutex<HashMap<String, bool>> {
    static M: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record an app's declared `jit` bit at (re)launch. Called from `apps.rs` next
/// to `record_child`; overwritten each launch (declared state is stable).
pub fn record_app_jit(name: &str, jit: bool) {
    if let Ok(mut m) = jit_declared().lock() {
        m.insert(name.to_string(), jit);
    }
}

/// Reverse-lookup: the `(app name, jit_declared)` for a tracked child pid, or
/// `None` if the pid is not one of darwind's own micro-apps. The ES front-end
/// uses this to key a kernel event to one of OUR apps (and ignore everything
/// else). Only the feature-gated ES client + tests call it.
#[allow(dead_code)]
pub fn app_for_pid(pid: u32) -> Option<(String, bool)> {
    let name = child_pids()
        .lock()
        .ok()?
        .iter()
        .find(|(_, p)| **p == pid)
        .map(|(n, _)| n.clone())?;
    let jit = jit_declared()
        .lock()
        .ok()?
        .get(&name)
        .copied()
        .unwrap_or(false);
    Some((name, jit))
}

// ===========================================================================
// Posture summary (read by posture.rs; updated each tick)
// ===========================================================================

/// The latest per-tick tally, so `posture.rs` can fold a one-liner into its
/// read-only report. Counts only — never secret.
#[derive(Debug, Clone, Copy, Default)]
struct LastSnapshot {
    apps: usize,
    drift: usize,
    anomalies: usize,
}

static LAST_SNAPSHOT: Mutex<Option<LastSnapshot>> = Mutex::new(None);

fn set_last_snapshot(apps: usize, drift: usize, anomalies: usize) {
    if let Ok(mut g) = LAST_SNAPSHOT.lock() {
        *g = Some(LastSnapshot {
            apps,
            drift,
            anomalies,
        });
    }
}

/// The last declared-capability inventory the sentinel computed, so the on-demand
/// `status_summary` query can report "what can each app do" without re-reading the
/// registry. SECRET-FREE (name + capability summary; counts, never paths/hosts).
static LAST_CAPS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn set_last_caps(caps: Vec<(String, String)>) {
    if let Ok(mut g) = LAST_CAPS.lock() {
        *g = caps;
    }
}

/// Pure: a concise " Declared capabilities: name [caps]; …." suffix, or "" if the
/// inventory is empty. Tested directly.
fn format_capabilities(caps: &[(String, String)]) -> String {
    if caps.is_empty() {
        return String::new();
    }
    let list: Vec<String> = caps.iter().map(|(n, c)| format!("{n} [{c}]")).collect();
    format!(" Declared capabilities: {}.", list.join("; "))
}

/// A one-line introspection summary for `posture.rs`'s read-only report, or
/// `None` if the sentinel has not ticked yet (so posture shows nothing stale).
/// SECRET-FREE — counts only.
pub fn posture_line() -> Option<String> {
    let snap = (*LAST_SNAPSHOT.lock().ok()?).as_ref().copied()?;
    let violations = MODULE_VIOLATIONS.load(Ordering::Relaxed);
    Some(format!(
        "Micro-app introspection: {} running · {} profile-drift · {} resource-anomalies · {} module-violations (session) — read-only",
        snap.apps, snap.drift, snap.anomalies, violations
    ))
}

/// Bounded, newest-first ring of human-readable finding lines, so the user-facing
/// status query can list recent drift/anomaly/module findings (the HUD gets them
/// live over telemetry; this retains a short tail for a spoken/typed query).
const MAX_FINDINGS: usize = 20;

fn findings() -> &'static Mutex<VecDeque<String>> {
    static M: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Redact the user's home-directory prefix (which embeds the username) to `~` in
/// a string. The findings ring feeds the user/cloud-facing `status_summary`
/// query, so paths that reach it are kept PII-free — while the LOCAL telemetry
/// envelopes keep the full path for on-device forensics.
pub(crate) fn redact_home(s: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => s.replace(&home, "~"),
        _ => s.to_string(),
    }
}

/// Retain one already-safe (SECRET-FREE) finding line for the status query.
pub fn record_finding(line: String) {
    if let Ok(mut q) = findings().lock() {
        q.push_front(line);
        while q.len() > MAX_FINDINGS {
            q.pop_back();
        }
    }
}

/// Pure formatter for the status query — unit-tested without the live globals.
fn format_status(
    snapshot: Option<(usize, usize, usize)>,
    violations: usize,
    recent: &[String],
) -> String {
    let Some((apps, drift, anomalies)) = snapshot else {
        return "Micro-app introspection: no observations yet — the sentinel starts ~30s after boot and reports once a sandboxed app is running. READ-ONLY (I watch my own apps and report; I change nothing).".to_string();
    };
    let mut s = format!(
        "Micro-app introspection (READ-ONLY — I watch my own sandboxed apps and report; I never kill, unload, or change a profile): {apps} app(s) observed, {drift} profile-drift, {anomalies} resource-anomalies, {violations} module-violations this session."
    );
    if recent.is_empty() {
        s.push_str(" No findings — every observed app is within its baseline.");
    } else {
        s.push_str(" Recent findings: ");
        s.push_str(&recent.join("; "));
        s.push('.');
    }
    s
}

/// A human summary of the introspection sentinel for the user-facing query
/// (`aegis_introspect`). READ-ONLY; SECRET-FREE.
pub fn status_summary() -> String {
    let snapshot = LAST_SNAPSHOT
        .lock()
        .ok()
        .and_then(|g| (*g).map(|s| (s.apps, s.drift, s.anomalies)));
    let violations = MODULE_VIOLATIONS.load(Ordering::Relaxed);
    let recent: Vec<String> = findings()
        .lock()
        .map(|q| q.iter().take(8).cloned().collect())
        .unwrap_or_default();
    let caps: Vec<(String, String)> = LAST_CAPS.lock().map(|g| g.clone()).unwrap_or_default();
    let mut s = format_status(snapshot, violations, &recent);
    s.push_str(&format_capabilities(&caps));
    s
}

// ===========================================================================
// Runtime sentinel (device-gated; never run in tests)
// ===========================================================================

/// Sample one live process's RSS/CPU via sysinfo. `sys` must already have been
/// refreshed for `pid` this tick. `None` if the process is gone.
fn sample_process(sys: &sysinfo::System, pid: u32) -> Option<ResourceSample> {
    let proc = sys.process(sysinfo::Pid::from_u32(pid))?;
    Some(ResourceSample {
        rss_bytes: proc.memory(),
        cpu_percent: proc.cpu_usage(),
    })
}

/// One sentinel tick: for each running app, (a) re-read its on-disk profile and
/// flag drift vs. the fingerprint we wrote, and (b) sample its process and
/// classify against a rolling baseline. Emits an ambient `introspect.snapshot`
/// plus per-finding `introspect.profile_drift` / `introspect.anomaly`. READ-ONLY.
async fn sentinel_tick(
    registry: &std::sync::Arc<crate::apps::AppRegistry>,
    sys: &mut sysinfo::System,
    // name -> (pid the baseline was seeded under, the baseline sample). The pid is
    // tracked so a relaunched app (new pid, same name) re-seeds instead of being
    // judged against the previous process's baseline.
    baselines: &mut HashMap<String, (u32, ResourceSample)>,
    thresholds: &AnomalyThresholds,
) {
    let apps = registry.observed_apps().await;
    let expected = snapshot_expected();
    let pids = snapshot_pids();

    // Refresh only the pids we track (same-UID children), then drop dead ones.
    let track: Vec<sysinfo::Pid> = pids.values().map(|p| sysinfo::Pid::from_u32(*p)).collect();
    if !track.is_empty() {
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&track), true);
    }

    let mut drift_count = 0usize;
    let mut anomaly_count = 0usize;
    let mut running = 0usize;

    for (name, profile_path, is_running) in &apps {
        if !*is_running {
            continue;
        }
        running += 1;

        // (a) profile drift.
        if let Some(expected_fp) = expected.get(name) {
            match std::fs::read_to_string(profile_path) {
                Ok(on_disk) => {
                    if let Some(drift) = detect_profile_drift(name, expected_fp, &on_disk) {
                        drift_count += 1;
                        record_finding(format!("profile-drift: {name}"));
                        let (event, payload) =
                            ev_profile_drift(name, &drift.expected_fp, &drift.actual_fp);
                        crate::telemetry::emit("system", event, payload);
                    }
                }
                Err(_) => {
                    // The profile file vanished while the app runs — also drift.
                    drift_count += 1;
                    record_finding(format!("profile-missing: {name}"));
                    let (event, payload) = ev_profile_missing(name);
                    crate::telemetry::emit("system", event, payload);
                }
            }
        }

        // (b) resource sampling + classification (decision is the pure, tested
        // resource_decision; a relaunch with a new pid re-seeds rather than being
        // judged against the previous process's baseline).
        if let Some(pid) = pids.get(name).copied() {
            if let Some(sample) = sample_process(sys, pid) {
                match resource_decision(name, baselines.get(name).copied(), pid, sample, thresholds)
                {
                    BaselineOutcome::Seed | BaselineOutcome::UpdateHealthy => {
                        baselines.insert(name.clone(), (pid, sample));
                    }
                    BaselineOutcome::HoldAnomalous(anomalies) => {
                        for a in &anomalies {
                            anomaly_count += 1;
                            record_finding(format!("{}: {} — {}", a.kind, a.app, a.detail));
                            let (event, payload) = ev_anomaly(&a.app, a.kind, &a.detail);
                            crate::telemetry::emit("system", event, payload);
                        }
                        // Hold the baseline (do NOT advance) so a runaway keeps tripping.
                    }
                }
            }
        }
    }

    // Forget baselines for apps that are no longer running/tracked.
    baselines.retain(|name, _| pids.contains_key(name));

    set_last_snapshot(running, drift_count, anomaly_count);
    let (event, payload) = ev_snapshot(running, drift_count, anomaly_count);
    crate::telemetry::emit("system", event, payload);

    // Declared-capability inventory (static, from manifests): the "what can each
    // app DO" audit alongside the runtime "what is it doing". Secret-free (counts,
    // never paths/hosts). Re-emitted each tick so a late-connecting HUD still gets
    // it (fire-and-forget, small — a handful of apps).
    let inventory = registry.capability_inventory().await;
    set_last_caps(inventory.clone()); // so the on-demand status query can report it
    let (event, payload) = ev_capabilities(&inventory);
    crate::telemetry::emit("system", event, payload);

    debug!(running, drift_count, anomaly_count, "introspect tick");
}

/// The ambient introspect sentinel loop (runtime-only; never run in tests).
/// Mirrors `tcc::sentinel_task`: a startup delay, then a slow periodic tick. The
/// delay, interval, and anomaly thresholds come from `[introspect]` config (with
/// defaults matching the original constants). Spawned from `main.rs` only when
/// `[introspect].enabled` is true.
pub async fn sentinel_task(
    registry: std::sync::Arc<crate::apps::AppRegistry>,
    startup_delay_secs: u64,
    interval_secs: u64,
    thresholds: AnomalyThresholds,
) {
    tokio::time::sleep(Duration::from_secs(startup_delay_secs)).await;
    let interval = Duration::from_secs(interval_secs.max(1));
    let mut sys = sysinfo::System::new();
    let mut baselines: HashMap<String, (u32, ResourceSample)> = HashMap::new();
    loop {
        sentinel_tick(&registry, &mut sys, &mut baselines, &thresholds).await;
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_content_sensitive() {
        let a = sbpl_fingerprint("(version 1)\n(deny default)\n");
        let b = sbpl_fingerprint("(version 1)\n(deny default)\n");
        let c = sbpl_fingerprint("(version 1)\n(allow default)\n");
        assert_eq!(a, b, "same bytes -> same fingerprint");
        assert_ne!(a, c, "different bytes -> different fingerprint");
        assert_eq!(a.len(), 64, "sha-256 hex is 64 chars");
    }

    #[test]
    fn no_drift_when_on_disk_matches() {
        let profile = "(version 1)\n(deny default)\n(deny dynamic-code-generation)\n";
        let fp = sbpl_fingerprint(profile);
        assert_eq!(detect_profile_drift("global-scan", &fp, profile), None);
    }

    #[test]
    fn drift_detected_when_on_disk_tampered() {
        let original = "(version 1)\n(deny default)\n(deny dynamic-code-generation)\n";
        let tampered = "(version 1)\n(deny default)\n(allow dynamic-code-generation)\n";
        let fp = sbpl_fingerprint(original);
        let drift = detect_profile_drift("global-scan", &fp, tampered)
            .expect("a tampered profile must be flagged as drift");
        assert_eq!(drift.app, "global-scan");
        assert_eq!(drift.expected_fp, fp);
        assert_eq!(drift.actual_fp, sbpl_fingerprint(tampered));
        assert_ne!(drift.expected_fp, drift.actual_fp);
    }

    #[test]
    fn stable_process_trips_no_anomaly() {
        let th = AnomalyThresholds::default();
        let base = ResourceSample { rss_bytes: 200 * 1024 * 1024, cpu_percent: 5.0 };
        let now = ResourceSample { rss_bytes: 210 * 1024 * 1024, cpu_percent: 7.0 };
        assert!(classify_anomalies("app", &base, &now, &th).is_empty());
    }

    #[test]
    fn rss_growth_beyond_ratio_and_floor_trips() {
        let th = AnomalyThresholds::default(); // 3x, 64MiB floor, 95% cpu
        let base = ResourceSample { rss_bytes: 100 * 1024 * 1024, cpu_percent: 1.0 };
        let now = ResourceSample { rss_bytes: 400 * 1024 * 1024, cpu_percent: 1.0 };
        let a = classify_anomalies("app", &base, &now, &th);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "rss_growth");
    }

    #[test]
    fn rss_growth_below_floor_is_ignored() {
        // Tripled, but the current reading is tiny (under the 64 MiB floor) —
        // must NOT trip, so a small process ramping proportionally is not noise.
        let th = AnomalyThresholds::default();
        let base = ResourceSample { rss_bytes: 4 * 1024 * 1024, cpu_percent: 1.0 };
        let now = ResourceSample { rss_bytes: 20 * 1024 * 1024, cpu_percent: 1.0 };
        assert!(classify_anomalies("app", &base, &now, &th).is_empty());
    }

    #[test]
    fn cpu_spike_trips_independently() {
        let th = AnomalyThresholds::default();
        let base = ResourceSample { rss_bytes: 100 * 1024 * 1024, cpu_percent: 10.0 };
        let now = ResourceSample { rss_bytes: 100 * 1024 * 1024, cpu_percent: 99.0 };
        let a = classify_anomalies("app", &base, &now, &th);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].kind, "cpu_spike");
    }

    #[test]
    fn resource_decision_seeds_on_cold_start() {
        let th = AnomalyThresholds::default();
        let now = ResourceSample { rss_bytes: 200 * 1024 * 1024, cpu_percent: 50.0 };
        assert_eq!(resource_decision("app", None, 100, now, &th), BaselineOutcome::Seed);
    }

    #[test]
    fn resource_decision_reseeds_on_pid_change_not_flags() {
        // THE BUG GUARD: a relaunch (same name, NEW pid) must re-seed, never judge
        // the new process against the old process's baseline — even if the new
        // sample would look like a runaway vs the old one.
        let th = AnomalyThresholds::default();
        let old = (100u32, ResourceSample { rss_bytes: 50 * 1024 * 1024, cpu_percent: 1.0 });
        let heavy_new = ResourceSample { rss_bytes: 500 * 1024 * 1024, cpu_percent: 99.0 };
        assert_eq!(
            resource_decision("app", Some(old), 200 /* new pid */, heavy_new, &th),
            BaselineOutcome::Seed,
            "a new pid must re-seed, not flag the relaunch as a runaway"
        );
    }

    #[test]
    fn resource_decision_updates_when_same_pid_and_healthy() {
        let th = AnomalyThresholds::default();
        let base = (100u32, ResourceSample { rss_bytes: 200 * 1024 * 1024, cpu_percent: 5.0 });
        let now = ResourceSample { rss_bytes: 210 * 1024 * 1024, cpu_percent: 6.0 };
        assert_eq!(
            resource_decision("app", Some(base), 100, now, &th),
            BaselineOutcome::UpdateHealthy
        );
    }

    #[test]
    fn resource_decision_holds_and_reports_when_same_pid_and_anomalous() {
        let th = AnomalyThresholds::default();
        let base = (100u32, ResourceSample { rss_bytes: 100 * 1024 * 1024, cpu_percent: 1.0 });
        let runaway = ResourceSample { rss_bytes: 400 * 1024 * 1024, cpu_percent: 1.0 };
        match resource_decision("app", Some(base), 100, runaway, &th) {
            BaselineOutcome::HoldAnomalous(anomalies) => {
                assert_eq!(anomalies.len(), 1);
                assert_eq!(anomalies[0].kind, "rss_growth");
            }
            other => panic!("expected HoldAnomalous, got {other:?}"),
        }
    }

    #[test]
    fn zero_baseline_never_trips_rss_growth() {
        // Defensive: a not-yet-baselined (0) reading can't produce a ratio.
        let th = AnomalyThresholds::default();
        let base = ResourceSample { rss_bytes: 0, cpu_percent: 0.0 };
        let now = ResourceSample { rss_bytes: 500 * 1024 * 1024, cpu_percent: 1.0 };
        assert!(classify_anomalies("app", &base, &now, &th).is_empty());
    }

    #[test]
    fn record_child_guard_clears_pid_on_drop() {
        {
            let _g = record_child("guard-test-app", Some(4242));
            assert_eq!(snapshot_pids().get("guard-test-app"), Some(&4242));
        }
        assert_eq!(snapshot_pids().get("guard-test-app"), None, "pid cleared on drop");
    }

    #[test]
    fn pid_guard_only_clears_the_exact_pid_it_recorded() {
        // A guard that recorded NOTHING (None) must not clear an existing entry.
        child_pids().lock().unwrap().insert("pg-app".to_string(), 111);
        {
            let _none_guard = record_child("pg-app", None);
        }
        assert_eq!(snapshot_pids().get("pg-app"), Some(&111), "None guard must not clear");
        // A stale guard (recorded 111) must not clear a NEWER pid (222) that a
        // later launch inserted — it only removes its own pid.
        let stale = record_child("pg-app", Some(111));
        child_pids().lock().unwrap().insert("pg-app".to_string(), 222);
        drop(stale);
        assert_eq!(snapshot_pids().get("pg-app"), Some(&222), "stale guard must not clear newer pid");
        child_pids().lock().unwrap().remove("pg-app");
    }

    #[test]
    fn redact_home_replaces_the_home_prefix_only() {
        if let Ok(home) = std::env::var("HOME") {
            let r = redact_home(&format!("module: x loaded unexpected {home}/Library/evil.dylib"));
            assert!(!r.contains(&home), "home prefix (username) must be redacted: {r}");
            assert!(r.contains("~/Library/evil.dylib"));
        }
        // A path with no home prefix is passed through unchanged (forensics intact).
        assert_eq!(redact_home("/usr/lib/libSystem.B.dylib"), "/usr/lib/libSystem.B.dylib");
    }

    #[test]
    fn record_profile_stores_fingerprint() {
        let profile = "(version 1)\n(deny default)\n";
        record_profile("fp-test-app", profile);
        assert_eq!(
            snapshot_expected().get("fp-test-app"),
            Some(&sbpl_fingerprint(profile))
        );
    }

    #[test]
    fn app_for_pid_reverse_looks_up_name_and_jit() {
        // The ES front-end keys a kernel event to one of OUR apps via its pid.
        let _g = record_child("es-lookup-app", Some(59123));
        record_app_jit("es-lookup-app", true);
        assert_eq!(app_for_pid(59123), Some(("es-lookup-app".to_string(), true)));
        // An app with no recorded jit bit defaults to false (not JIT-declared).
        let _g2 = record_child("es-lookup-nojit", Some(59124));
        assert_eq!(app_for_pid(59124), Some(("es-lookup-nojit".to_string(), false)));
        // A pid that is not one of our children is not attributed.
        assert_eq!(app_for_pid(1), None);
    }

    // -- module attestation --------------------------------------------------

    fn m(path: &str, uuid: Option<&str>) -> Module {
        Module {
            path: path.to_string(),
            uuid: uuid.map(str::to_string),
        }
    }

    #[test]
    fn parse_module_report_drops_junk_and_dedupes_and_bounds() {
        let data = json!({"modules": [
            {"path": "/usr/lib/libSystem.B.dylib", "uuid": "AAAA"},
            {"path": "/usr/lib/libSystem.B.dylib", "uuid": "AAAA"}, // dup -> collapsed
            {"path": ""},                                            // empty path -> dropped
            {"uuid": "BBBB"},                                        // no path -> dropped
            {"path": "/opt/x.dylib"},                                // uuid-less is fine
        ]});
        let mods = parse_module_report(&data);
        assert_eq!(mods.len(), 2);
        assert_eq!(mods[0], m("/usr/lib/libSystem.B.dylib", Some("AAAA")));
        assert_eq!(mods[1], m("/opt/x.dylib", None));
    }

    #[test]
    fn parse_module_report_absent_or_wrong_type_is_empty() {
        assert!(parse_module_report(&json!({})).is_empty());
        assert!(parse_module_report(&json!({"modules": "nope"})).is_empty());
    }

    #[test]
    fn attest_flags_unexpected_and_counts_missing() {
        let baseline: BTreeSet<String> = [m("/a", Some("1")), m("/b", Some("2"))]
            .iter()
            .map(Module::key)
            .collect();
        // /a present, /b gone (missing), /evil.dylib new (unexpected).
        let observed = vec![m("/a", Some("1")), m("/evil.dylib", Some("9"))];
        let att = attest_modules(&baseline, &observed);
        assert_eq!(att.total, 2);
        assert_eq!(att.unexpected, vec![m("/evil.dylib", Some("9"))]);
        assert_eq!(att.missing_count, 1);
    }

    #[test]
    fn attest_treats_a_uuid_swap_as_unexpected() {
        // Same path, different UUID = a swapped module, not the baseline one.
        let baseline: BTreeSet<String> = [m("/a", Some("1"))].iter().map(Module::key).collect();
        let att = attest_modules(&baseline, &[m("/a", Some("HACKED"))]);
        assert_eq!(att.unexpected, vec![m("/a", Some("HACKED"))]);
    }

    #[test]
    fn attest_or_seed_ignores_an_empty_report_and_never_poisons_the_baseline() {
        let app = "attest-empty-app";
        // An empty report never seeds and never attests -> always None. This is
        // the guard: an empty FIRST report used to seed an empty baseline, which
        // then flagged every real module on the next report as an injection.
        assert!(attest_or_seed(app, &[]).is_none());
        // The first NON-empty report seeds (returns None), not flags-everything.
        let real = vec![m("/usr/lib/libSystem.B.dylib", Some("AAAA"))];
        assert!(attest_or_seed(app, &real).is_none());
        // A later empty report is still ignored — never a false attestation.
        assert!(attest_or_seed(app, &[]).is_none());
        // The real baseline stands: re-reporting it flags nothing.
        let att = attest_or_seed(app, &real).expect("attests against the seeded baseline");
        assert!(att.unexpected.is_empty());
    }

    #[test]
    fn attest_or_seed_seeds_first_then_detects_injection() {
        let app = "attest-seed-app";
        let first = vec![m("/usr/lib/libSystem.B.dylib", Some("AAAA")), m("/app/main", None)];
        // First report SEEDS — no attestation, no alert.
        assert!(attest_or_seed(app, &first).is_none());
        // A later report with an extra module trips exactly that one as unexpected.
        let mut later = first.clone();
        later.push(m("/tmp/inject.dylib", Some("EVIL")));
        let att = attest_or_seed(app, &later).expect("second report attests");
        assert_eq!(att.unexpected, vec![m("/tmp/inject.dylib", Some("EVIL"))]);
        // The clean subset (baseline members) never trips even if some dropped.
        let att2 = attest_or_seed(app, &first).expect("third report attests");
        assert!(att2.unexpected.is_empty());
    }

    #[test]
    fn reset_module_baseline_re_seeds_on_relaunch() {
        let app = "attest-reset-app";
        let v1 = vec![m("/app/v1", Some("OLD"))];
        // Seed with the old build.
        assert!(attest_or_seed(app, &v1).is_none());
        // A relaunch of a legitimately-updated app: reset, then its new module set
        // must SEED (return None), not be flagged as unexpected.
        reset_module_baseline(app);
        let v2 = vec![m("/app/v2", Some("NEW"))];
        assert!(
            attest_or_seed(app, &v2).is_none(),
            "post-reset first report must re-seed, not flag the new build"
        );
        // And after re-seeding, injection is still caught within the new launch.
        let mut injected = v2.clone();
        injected.push(m("/tmp/x.dylib", Some("EVIL")));
        let att = attest_or_seed(app, &injected).expect("attests after re-seed");
        assert_eq!(att.unexpected, vec![m("/tmp/x.dylib", Some("EVIL"))]);
    }

    // -- status summary ------------------------------------------------------

    #[test]
    fn format_status_before_any_tick_is_honest_and_read_only() {
        let s = format_status(None, 0, &[]);
        assert!(s.contains("no observations yet"));
        assert!(s.contains("READ-ONLY"));
    }

    #[test]
    fn format_status_clean_says_within_baseline() {
        let s = format_status(Some((3, 0, 0)), 0, &[]);
        assert!(s.contains("3 app(s) observed"));
        assert!(s.contains("0 module-violations"));
        assert!(s.contains("within its baseline"));
        // Never implies it acts.
        assert!(s.contains("I never kill, unload, or change a profile"));
    }

    #[test]
    fn format_capabilities_is_concise_and_empty_safe() {
        assert_eq!(format_capabilities(&[]), "");
        let caps = vec![
            ("global-scan".to_string(), "net(2), fs_read(1)".to_string()),
            ("vision".to_string(), "camera, screen".to_string()),
        ];
        let s = format_capabilities(&caps);
        assert!(s.contains("Declared capabilities:"));
        assert!(s.contains("global-scan [net(2), fs_read(1)]"));
        assert!(s.contains("vision [camera, screen]"));
    }

    #[test]
    fn format_status_lists_recent_findings() {
        let recent = vec![
            "module: vision loaded unexpected /tmp/x.dylib".to_string(),
            "cpu_spike: algo-core — cpu 99% > 95% threshold".to_string(),
        ];
        let s = format_status(Some((2, 0, 1)), 1, &recent);
        assert!(s.contains("Recent findings:"));
        assert!(s.contains("/tmp/x.dylib"));
        assert!(s.contains("cpu_spike: algo-core"));
    }

    #[test]
    fn record_finding_is_bounded_and_newest_first() {
        // Isolated key space so other tests' findings don't interfere with counts.
        for i in 0..(MAX_FINDINGS + 5) {
            record_finding(format!("rf-test-{i}"));
        }
        let q = findings().lock().unwrap();
        assert_eq!(q.len(), MAX_FINDINGS, "the ring is capped");
        assert_eq!(q.front().unwrap(), &format!("rf-test-{}", MAX_FINDINGS + 4));
    }

    // -- security-event classification (the ES seam) -------------------------

    #[test]
    fn exec_mapping_by_a_non_jit_app_is_a_wx_violation() {
        for ev in [SecurityEvent::MprotectExec, SecurityEvent::MapJit] {
            let f = classify_security_event("global-scan", false, &ev)
                .expect("a jit=false app making memory executable must be flagged");
            assert_eq!(f.kind, "wx_violation");
            assert!(f.high);
            assert!(f.detail.contains("jit=false"));
        }
    }

    #[test]
    fn exec_mapping_by_a_jit_declared_app_is_expected_no_finding() {
        // An app that declared jit=true is EXPECTED to make executable memory.
        assert!(classify_security_event("algo-core", true, &SecurityEvent::MprotectExec).is_none());
        assert!(classify_security_event("algo-core", true, &SecurityEvent::MapJit).is_none());
    }

    #[test]
    fn task_port_acquisition_is_a_high_attach_signal() {
        let ev = SecurityEvent::GetTask { by_pid: 99, by_path: "/usr/bin/lldb".into() };
        let f = classify_security_event("vision", false, &ev).expect("get_task must be flagged");
        assert_eq!(f.kind, "task_port_acquired");
        assert!(f.high);
        assert!(f.detail.contains("/usr/bin/lldb"));
        assert!(f.detail.contains("vision"));
    }

    #[test]
    fn signal_is_a_low_notice() {
        let ev = SecurityEvent::Signal { signal: 9, by_pid: 42 };
        let f = classify_security_event("global-scan", false, &ev).expect("signal is surfaced");
        assert_eq!(f.kind, "signal");
        assert!(!f.high, "a signal is a notice, not a high compromise signal");
    }

    #[test]
    fn ingest_security_event_records_a_finding_for_a_violation() {
        // A jit=false app mapping MAP_JIT -> a finding is retained in the ring.
        ingest_security_event("ingest-sec-app", false, &SecurityEvent::MapJit);
        let q = findings().lock().unwrap();
        assert!(
            q.iter().any(|l| l.starts_with("wx_violation:") && l.contains("ingest-sec-app")),
            "the violation must be recorded as a finding"
        );
    }

    // -- telemetry wire contract (anchors the field names the HUD reads) -----
    //
    // This is the guard that was MISSING when introspect.module_violation shipped
    // with "name" while the HUD read "app". Every SINGLE-APP event MUST key the
    // app on "app"; these asserts fail if a builder renames a field the HUD reads.

    #[test]
    fn every_single_app_event_keys_the_app_on_app_not_name() {
        let uuid = Some("U".to_string());
        let cases: Vec<(&str, serde_json::Value)> = vec![
            ev_profile_drift("gs", "e", "a"),
            ev_profile_missing("gs"),
            ev_anomaly("gs", "cpu_spike", "hot"),
            ev_module_violation("gs", "/x.dylib", &uuid),
            ev_modattest("gs", 3, 1, 0, false),
            ev_security("gs", "wx_violation", true, "d"),
        ];
        for (event, payload) in cases {
            assert!(event.starts_with("introspect."), "{event} must be an introspect.* event");
            assert_eq!(
                payload.get("app").and_then(|v| v.as_str()),
                Some("gs"),
                "event {event} must key the app on \"app\" (HUD reads \"app\"): {payload}"
            );
            assert!(
                payload.get("name").is_none(),
                "event {event} must NOT use \"name\" for the app: {payload}"
            );
        }
    }

    #[test]
    fn module_violation_envelope_matches_the_hud_parser_shape() {
        // HUD introspectModuleViolationLine reads app + path; parse_module_report /
        // the HUD read uuid (nullable). Assert the exact shape.
        let (event, p) = ev_module_violation("vision", "/tmp/inject.dylib", &Some("ABC".into()));
        assert_eq!(event, "introspect.module_violation");
        assert_eq!(p["app"], "vision");
        assert_eq!(p["path"], "/tmp/inject.dylib");
        assert_eq!(p["uuid"], "ABC");
        // A nil uuid serializes to JSON null (the HUD's str() coerces it away).
        let (_e, p2) = ev_module_violation("vision", "/tmp/x", &None);
        assert!(p2["uuid"].is_null());
    }

    #[test]
    fn snapshot_and_capabilities_envelopes_match_the_hud_parser_shape() {
        let (e1, p1) = ev_snapshot(4, 1, 2);
        assert_eq!(e1, "introspect.snapshot");
        assert_eq!(p1["apps"], 4);
        assert_eq!(p1["drift"], 1);
        assert_eq!(p1["anomalies"], 2);

        let (e2, p2) = ev_capabilities(&[("gs".into(), "net(2)".into())]);
        assert_eq!(e2, "introspect.capabilities");
        // capabilities ITEMS key on "name"+"caps" (the HUD reads item.name/item.caps);
        // this is a list-item shape, distinct from the single-app "app" events.
        assert_eq!(p2["apps"][0]["name"], "gs");
        assert_eq!(p2["apps"][0]["caps"], "net(2)");
    }
}
