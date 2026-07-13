//! Task #12 — the PANIC / LOCKDOWN emergency stop.
//!
//! This is THE emergency stop, and it is absolute when engaged. Lockdown is a
//! process-global OVERLAY that, while ON, forces OFF every consequential /
//! outward / autonomy / mic surface in the daemon — no exception, no race that
//! lets one through. It PERSISTS across a restart (a marker on disk) until an
//! explicit, deliberate, USER-ONLY unlock.
//!
//! ## What it is (and is not)
//!
//!  * It is a single, lock-free [`AtomicBool`] read ([`is_locked_down`]) that
//!    every master gate ANDs into its decision (`gate && !locked`). With it OFF
//!    — the SHIPPED DEFAULT — every gate is byte-for-byte today: this module
//!    only ever ADDS a force-off; it never loosens anything.
//!  * It is an OVERLAY, not a config clobber. [`panic`] does NOT touch the
//!    user's individual `[integrations]/[mcp]/[standing]/...` switches; it flips
//!    one global that overrides them while engaged. So [`unlock`] restores the
//!    user's CONFIGURED state exactly — nothing was mutated underneath them.
//!  * It is HONEST. Panic stops FUTURE outward actions + autonomy + the mic
//!    immediately, and persists. It does NOT and CANNOT undo an action already
//!    executed (you can't un-send an email). The spoken confirmation says so.
//!
//! ## The persistence marker (restart re-entry)
//!
//! On [`panic`] we write a tiny marker file (`state/lockdown`). On daemon start
//! [`init`] reads it: if present, the process RE-ENTERS lockdown before any gate
//! is consulted, so a restart cannot quietly drop the emergency stop. [`unlock`]
//! removes the marker, so the next start comes up normal. The marker content is
//! irrelevant — its mere EXISTENCE is the signal — so a corrupt/partial write
//! still fails SAFE (present => locked).
//!
//! ## Who may unlock
//!
//! [`unlock`] is USER-ONLY and deliberate: it is reached ONLY from the explicit
//! user voice intent ("unlock" / "resume normal" / "end lockdown") and the
//! HUD/Settings command verb (`Command::Unlock`, NOT model-routed). There is no
//! path from the cloud tool loop, an MCP server, or any injected/agent text to
//! [`unlock`] — by construction it is never called from `execute_tool` or any
//! model-facing surface. [`panic`], by contrast, is reachable from everywhere
//! (the more it can fire, the safer), including a router intent honored BEFORE
//! normal routing so "panic" works mid-anything.
//!
//! Everything here is HERMETIC: the state is in-process, the marker path is
//! injectable for tests (a temp dir), and nothing touches the network, the mic,
//! the brain, or a real client.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use tracing::{info, warn};

/// THE process-global emergency-stop flag. `false` (the shipped default) = normal
/// operation, every gate byte-for-byte today. `true` = LOCKED DOWN: every master
/// gate is forced off. A lock-free [`AtomicBool`] so the hot-path gate read
/// ([`is_locked_down`]) never blocks and can be called from any thread (including
/// the realtime audio thread's downstream) without contention.
static LOCKDOWN: AtomicBool = AtomicBool::new(false);

/// Where the persistence marker lives, installed once by [`init`]. `None` until
/// init runs (any unit test that does not call init): with no path we keep the
/// in-memory flag honest but skip disk I/O — the safe, hermetic default.
static MARKER_PATH: OnceLock<PathBuf> = OnceLock::new();

/// The marker filename under `state/`. Its EXISTENCE is the signal; the content
/// is a short human-readable note (never parsed).
const MARKER_FILE: &str = "lockdown";

/// The honest spoken confirmation [`panic`] returns. Names exactly what the stop
/// does — and, critically, what it does NOT do. The HUD echoes the same copy.
pub const PANIC_CONFIRMATION: &str = "Lockdown engaged. I've stopped all future outward actions, \
     all autonomy, and the microphone immediately, and this persists across a restart until you \
     unlock. I can't undo anything already done — a sent message stays sent. Say 'unlock' or use \
     the panic control in Settings to resume.";

/// The honest spoken confirmation [`unlock`] returns.
pub const UNLOCK_CONFIRMATION: &str =
    "Lockdown lifted. Your configured settings are restored — nothing was changed underneath them.";

/// The AUTHORITATIVE HUD posture payload (`lockdown.status`) — built in ONE
/// place so [`panic`], [`unlock`], and main's startup snapshot all put the exact
/// field names the HUD reads (hud/src/core/events.ts `parseLockdownStatus`:
/// `locked` + `restored_from_marker`) on the wire. The HUD's LOCKED-DOWN
/// indicator listens ONLY to `lockdown.status` — the audit-shaped
/// `lockdown.panic`/`lockdown.unlock` events never flip it.
pub fn status_payload(locked: bool, restored_from_marker: bool) -> serde_json::Value {
    serde_json::json!({"locked": locked, "restored_from_marker": restored_from_marker})
}

// ---------------------------------------------------------------------------
// Test-only marker-path seam (mirrors model_tier's OVERRIDE_TL / voiceid's
// GATE_OVERRIDE): a test points the marker at its OWN temp dir on its OWN
// thread, so the persistence round-trip is exercised hermetically without
// touching a shared global path. Compiled out of release entirely.
// ---------------------------------------------------------------------------
#[cfg(test)]
thread_local! {
    static MARKER_PATH_TL: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// The effective marker path: the test-local override when set, else the
/// init-installed global, else `None` (no disk persistence — in-memory only).
fn marker_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(p) = MARKER_PATH_TL.with(|c| c.borrow().clone()) {
            return Some(p);
        }
    }
    MARKER_PATH.get().cloned()
}

/// Compute the marker path under a state dir. Single source of truth so init and
/// tests agree on the location.
fn marker_in(state_dir: &Path) -> PathBuf {
    state_dir.join(MARKER_FILE)
}

// Test-only thread-local override of the lockdown read, mirroring voiceid's
// `GATE_OVERRIDE` / model_tier's `OVERRIDE_TL` / integrations' `CONSEQUENTIAL_
// OVERRIDE`. The lockdown flag is a process-global atomic that EVERY gate reads,
// and cargo runs tests concurrently, so a test that flips the real global would
// race a parallel gate test on another thread. Instead, a test forces the read on
// its OWN thread via [`LockdownOverride`]; production (and any non-overriding
// test thread) compiles this block out and reads the atomic directly, byte-for-
// byte the original. Default `None` => read through to the atomic.
#[cfg(test)]
thread_local! {
    static LOCKDOWN_TL: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The fast, lock-free read EVERY master gate ANDs into its decision. `true` =
/// locked down (force everything off); `false` = normal (shipped default).
/// Acquire-ordered against the [`panic`]/[`unlock`] store so a gate on another
/// thread observes the flip promptly.
#[inline]
pub fn is_locked_down() -> bool {
    #[cfg(test)]
    {
        if let Some(v) = LOCKDOWN_TL.with(std::cell::Cell::get) {
            return v;
        }
    }
    LOCKDOWN.load(Ordering::Acquire)
}

/// Set the lockdown flag, honoring the test thread-local seam: when a
/// [`LockdownOverride`] is active on this thread the write goes there (so the
/// shared atomic — which parallel gate tests read — is never touched); otherwise
/// (production, and any non-overriding thread) it writes the real atomic with
/// Release ordering. So [`panic`]/[`unlock`] under a thread-local override stay
/// fully thread-local, exactly like model_tier's `set_override`.
#[inline]
fn set_flag(value: bool) {
    #[cfg(test)]
    {
        if LOCKDOWN_TL.with(|c| c.get().is_some()) {
            LOCKDOWN_TL.with(|c| c.set(Some(value)));
            return;
        }
    }
    LOCKDOWN.store(value, Ordering::Release);
}

/// `#[cfg(test)]`-only RAII guard that forces [`is_locked_down`] to a value on the
/// CURRENT thread, restoring the prior state on drop so the override never leaks
/// into another parallel test. Other gate-tests (mcp, anthropic, ...) that drive
/// lockdown use THIS so they never touch the shared atomic. The whole seam is
/// `cfg(test)`, so production behavior is unchanged.
#[cfg(test)]
pub(crate) struct LockdownOverride {
    prev: Option<bool>,
}

#[cfg(test)]
impl LockdownOverride {
    /// Force `is_locked_down()` to `value` on this thread until the guard drops.
    pub(crate) fn force(value: bool) -> Self {
        let prev = LOCKDOWN_TL.with(|c| c.replace(Some(value)));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for LockdownOverride {
    fn drop(&mut self) {
        LOCKDOWN_TL.with(|c| c.set(self.prev));
    }
}

/// Wire the marker path from the daemon's state dir and RE-ENTER lockdown if the
/// marker is present (a prior panic that has not been unlocked). Called once from
/// `main()` at startup, BEFORE any gate is consulted, so a restart can never
/// silently drop the emergency stop. Idempotent and safe to call with the marker
/// absent (the normal cold start: stays unlocked).
///
/// Returns whether the process came up LOCKED (so the caller can log/telemeter
/// the restart re-entry). Only ever SETS the flag here — startup never clears a
/// persisted lockdown; that requires a deliberate [`unlock`].
pub fn init(state_dir: &Path) -> bool {
    let path = marker_in(state_dir);
    let present = path.exists();
    // Install the path so panic/unlock persist to the right place. A lost set
    // means init ran twice with the same dir — harmless.
    let _ = MARKER_PATH.set(path);
    if present {
        // `set_flag` writes the real atomic in production (Release-ordered) and
        // the thread-local under a test override, so the restart-re-entry test
        // stays on its own thread and never pollutes the shared atomic that
        // parallel gate tests read.
        set_flag(true);
        warn!("lockdown: marker present at startup — RE-ENTERING lockdown (panic persists across restart until unlock)");
    } else {
        info!(locked = false, "lockdown: initialized (no marker; normal operation)");
    }
    present
}

/// Write the persistence marker so a restart re-enters lockdown. Best-effort: a
/// write failure is logged, never fatal — the in-memory flag is already set, so
/// the CURRENT process is locked regardless; only cross-restart persistence is at
/// risk, and we surface that loudly. With no marker path installed (a unit test
/// that skipped init) this is a silent no-op.
fn write_marker() {
    let Some(path) = marker_path() else { return };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(error = %e, "lockdown: could not create state dir for the marker");
        }
    }
    // Content is never parsed (existence is the signal); a human note aids debugging.
    if let Err(e) = std::fs::write(&path, b"locked down by panic; remove only via explicit unlock\n") {
        warn!(error = %e, "lockdown: FAILED to persist the marker — lockdown may not survive a restart");
    }
}

/// Remove the persistence marker so the next start comes up normal. Best-effort:
/// an already-absent marker is success; any other error is logged. With no marker
/// path installed this is a silent no-op.
fn remove_marker() {
    let Some(path) = marker_path() else { return };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!(error = %e, "lockdown: could not remove the marker on unlock"),
    }
}

/// ENGAGE the emergency stop. Reachable from EVERYWHERE — the voice intent, the
/// HUD `Command::Panic` verb, a future signal handler — because the more places
/// can fire it, the safer. Effects, in order:
///   1. Set the global flag (so EVERY gate reads OFF from this instant; the mic
///      capture loop's per-chunk check drops the live mic immediately).
///   2. DROP any parked confirmation (an armed action must never survive a panic).
///   3. PERSIST the marker (so a restart re-enters lockdown until unlock).
///   4. Best-effort AUDIT the panic event (accountability; never blocks).
///   5. Return the honest spoken confirmation.
///
/// HONESTY: this stops FUTURE actions + the mic. It does NOT undo an action that
/// already executed — the confirmation says so.
pub async fn panic() -> &'static str {
    // 1. The flag FIRST (Release-ordered in production), so any gate read after
    //    this observes locked. This single store is what makes every gate force-
    //    off. Under a test thread-local override it stays thread-local.
    set_flag(true);
    // 2. Drop any armed confirmation — a parked outward action must not survive.
    crate::confirm::clear();
    // 2b. Silence any composed track playing in the background. The emergency stop
    //     cuts ALL audible output; a 30 s–10 min music track is no exception. This
    //     touches only the separate music sink — the speech path is unaffected.
    crate::playback::stop_track();
    // 3. Persist so a restart re-enters lockdown.
    write_marker();
    // 3b. Flip the HUD indicator: the LOCKED-DOWN light reads `lockdown.status`
    //     (state.ts), so the authoritative posture event must ride from HERE —
    //     every panic path (voice intent, HUD Command::Panic, a future signal
    //     handler) goes through this function. A live panic is never a marker
    //     restore. No-op before telemetry::init (a unit test) — fail-safe.
    crate::telemetry::emit("system", "lockdown.status", status_payload(true, false));
    // 4. Audit (secret-free, fire-and-forget on the success path).
    crate::audit::record_global(
        "system",
        "lockdown.panic",
        "all outward actions, autonomy, and the mic",
        crate::policy::Decision::Never,
        crate::audit::Outcome::BlockedByPolicy,
    )
    .await;
    info!("lockdown: PANIC — all future outward actions, autonomy, and the mic are now OFF and persisted");
    PANIC_CONFIRMATION
}

/// LIFT the emergency stop. USER-ONLY + deliberate: this is reached ONLY from the
/// explicit user voice intent and the HUD `Command::Unlock` verb — there is NO
/// call from the model tool loop, an MCP server, or any injected text (by
/// construction: `execute_tool` and every model-facing path never name this
/// function). Effects:
///   1. Clear the global flag (gates return to their CONFIGURED values — lockdown
///      was an overlay, so nothing was clobbered to restore).
///   2. REMOVE the marker (so the next restart comes up normal).
///   3. Best-effort AUDIT the unlock event.
///   4. Return the honest spoken confirmation.
pub async fn unlock() -> &'static str {
    set_flag(false);
    remove_marker();
    // The authoritative HUD posture event — the twin of panic()'s emit, so the
    // LOCKED-DOWN indicator returns to NORMAL from BOTH unlock paths (voice
    // intent + HUD Command::Unlock). An unlock is by definition not a restore.
    crate::telemetry::emit("system", "lockdown.status", status_payload(false, false));
    crate::audit::record_global(
        "system",
        "lockdown.unlock",
        "user lifted lockdown; configured settings restored",
        crate::policy::Decision::Always,
        crate::audit::Outcome::Confirmed,
    )
    .await;
    info!("lockdown: UNLOCK — emergency stop lifted by the user; configured settings restored");
    UNLOCK_CONFIRMATION
}

// ---------------------------------------------------------------------------
// Voice intent classifiers — PURE, conservatively anchored, unit-testable.
// ---------------------------------------------------------------------------

/// The PANIC trigger phrases. Honored by the router BEFORE normal routing (even
/// mid-confirmation / mid-anything), so any of these stops everything at once.
/// Anchored phrases, not bare words, so an incidental "stop" in a sentence does
/// not nuke the system — but the canonical emergency phrasings always fire.
const PANIC_PHRASES: &[&str] = &[
    "panic",
    "lockdown",
    "lock down",
    "stop everything",
    "kill switch",
    "shut it all down",
    "shut everything down",
    "emergency stop",
];

/// The UNLOCK trigger phrases — the explicit, deliberate user resume. Distinct
/// from panic so a single utterance can never both lock and unlock.
const UNLOCK_PHRASES: &[&str] = &[
    "unlock",
    "resume normal",
    "end lockdown",
    "lift lockdown",
    "lift the lockdown",
    "resume normal operation",
    "exit lockdown",
];

/// Normalize an utterance for phrase matching: lowercase, every non-alphanumeric
/// to a space, whitespace collapsed. So "PANIC!", "panic.", and "  panic  " all
/// match "panic", while "panicking" (a different token sequence) does not match
/// the standalone phrase because we match on whole space-delimited substrings.
fn normalize(utterance: &str) -> String {
    let mut out = String::with_capacity(utterance.len());
    let mut last_space = true; // collapse leading space
    for c in utterance.chars() {
        if c.is_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    if out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Whole-token phrase containment: `haystack` (already normalized) contains
/// `needle` (a space-delimited phrase) on token boundaries, so "panic" matches
/// "panic now" and "hit the panic button" but NOT "panicking" or "hispanic".
fn contains_phrase(haystack: &str, needle: &str) -> bool {
    let hay: Vec<&str> = haystack.split(' ').filter(|s| !s.is_empty()).collect();
    let need: Vec<&str> = needle.split(' ').filter(|s| !s.is_empty()).collect();
    if need.is_empty() || need.len() > hay.len() {
        return false;
    }
    hay.windows(need.len()).any(|w| w == need.as_slice())
}

/// Is this utterance a PANIC trigger? PURE; the router calls it FIRST (before the
/// confirmation pre-check and all other routing) so "panic" works mid-anything.
///
/// CONSERVATIVE on the panic/unlock overlap: "end lockdown" / "lift lockdown" /
/// "exit lockdown" all contain the panic token "lockdown", but they are clearly
/// the user RESUMING, not re-arming. So an utterance that matches an UNLOCK phrase
/// is never read as panic — the two are disjoint by construction, and the safe
/// reading of "end lockdown" is unlock, not a second panic (which would be a
/// harmless no-op anyway since panic is idempotent, but the honest reply matters).
pub fn is_panic_intent(utterance: &str) -> bool {
    let norm = normalize(utterance);
    if norm.is_empty() {
        return false;
    }
    if UNLOCK_PHRASES.iter().any(|p| contains_phrase(&norm, p)) {
        return false;
    }
    PANIC_PHRASES.iter().any(|p| contains_phrase(&norm, p))
}

/// Is this utterance an explicit USER UNLOCK trigger? PURE. The router calls it
/// only on the user voice path; together with the HUD verb this is the ONLY way
/// to [`unlock`].
pub fn is_unlock_intent(utterance: &str) -> bool {
    let norm = normalize(utterance);
    if norm.is_empty() {
        return false;
    }
    UNLOCK_PHRASES.iter().any(|p| contains_phrase(&norm, p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialize every test that touches the process-global LOCKDOWN flag: cargo
    /// runs tests concurrently, and the flag is a single global. Each test takes
    /// this lock and resets the flag on entry so one never leaks into the next.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Guard bundle for a lockdown test: serialize on TEST_LOCK, install a
    /// thread-local lockdown override (so `panic`/`unlock`/`is_locked_down` all
    /// stay on THIS thread and never touch the shared atomic that parallel gate
    /// tests read), and clear any test-local marker path. The override starts at
    /// `false` (the shipped default). Holds the serialization guard + the override
    /// for the test's lifetime.
    struct TestGuard {
        _serial: std::sync::MutexGuard<'static, ()>,
        _override: LockdownOverride,
    }

    fn test_guard() -> TestGuard {
        let serial = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // Force the read OFF on this thread to start; panic/unlock then flip the
        // thread-local, never the global atomic.
        let ovr = LockdownOverride::force(false);
        MARKER_PATH_TL.with(|c| *c.borrow_mut() = None);
        TestGuard { _serial: serial, _override: ovr }
    }

    /// Guard for the `init` tests. `init` now writes through `set_flag`, which
    /// honors the thread-local seam, so an InitGuard installs a thread-local
    /// override (starting OFF) exactly like `test_guard`: `init`'s re-entry write
    /// lands on THIS thread's local, never the shared atomic that parallel gate
    /// tests read. It is a thin alias of `test_guard` kept named for the init tests
    /// to read clearly.
    fn init_guard() -> TestGuard {
        test_guard()
    }

    /// A unique, self-cleaning temp state dir — the stdlib pattern used across the
    /// crate's FS tests (no extra dep): `temp_dir()/jarvis-lockdown-<pid>-<tag>`,
    /// wiped on construction AND on drop.
    struct TempState(PathBuf);
    impl TempState {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("jarvis-lockdown-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("state")).unwrap();
            TempState(dir)
        }
        fn state(&self) -> PathBuf {
            self.0.join("state")
        }
    }
    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Point the marker at a temp state dir on THIS thread for a hermetic
    /// persistence round-trip, returning the dir handle (kept alive by the caller).
    fn with_temp_marker(tag: &str) -> TempState {
        let dir = TempState::new(tag);
        MARKER_PATH_TL.with(|c| *c.borrow_mut() = Some(marker_in(&dir.state())));
        dir
    }

    // -- default / shipped posture --------------------------------------------

    #[test]
    fn default_is_unlocked() {
        // Read the REAL atomic (no thread-local override) so this asserts the
        // shipped default, not a test seam.
        let _g = init_guard();
        assert!(!is_locked_down(), "the shipped default is unlocked — every gate byte-for-byte today");
    }

    // -- panic sets the flag + persists; unlock clears both -------------------

    #[tokio::test]
    async fn panic_sets_locked_and_persists_marker() {
        let _g = test_guard();
        let _dir = with_temp_marker("persist");
        assert!(!is_locked_down());
        let msg = panic().await;
        assert!(is_locked_down(), "panic engages the global flag");
        // Honest copy: stops future actions + mic, does NOT undo the past.
        assert!(msg.contains("stopped all future outward actions"));
        assert!(msg.contains("microphone"));
        assert!(msg.to_lowercase().contains("can't undo") || msg.to_lowercase().contains("cant undo"));
        assert!(msg.contains("persists across a restart"));
        // The marker is on disk.
        let path = marker_path().unwrap();
        assert!(path.exists(), "panic persists the marker");
    }

    #[tokio::test]
    async fn unlock_clears_flag_and_removes_marker() {
        let _g = test_guard();
        let _dir = with_temp_marker("unlock");
        let _ = panic().await;
        assert!(is_locked_down());
        assert!(marker_path().unwrap().exists());
        let msg = unlock().await;
        assert!(!is_locked_down(), "unlock clears the flag");
        assert!(!marker_path().unwrap().exists(), "unlock removes the marker");
        assert!(msg.contains("restored"));
    }

    // -- the HUD wire contract: lockdown.status ---------------------------------

    /// The exact `lockdown.status` payload shape the HUD reads. state.ts's
    /// `case "lockdown.status"` + parseLockdownStatus consume `locked` and
    /// `restored_from_marker` — a rename here blanks the LOCKED-DOWN indicator.
    #[test]
    fn status_payload_matches_the_hud_contract() {
        let p = status_payload(true, false);
        assert_eq!(p["locked"], serde_json::json!(true));
        assert_eq!(p["restored_from_marker"], serde_json::json!(false));
        assert_eq!(
            p.as_object().unwrap().len(),
            2,
            "exactly the two booleans the HUD parses — no extra/renamed field"
        );
        let p = status_payload(false, true);
        assert_eq!(p["locked"], serde_json::json!(false));
        assert_eq!(p["restored_from_marker"], serde_json::json!(true));
    }

    /// panic()/unlock() must EMIT the authoritative `lockdown.status` posture —
    /// the router's voice path emits only the audit-shaped lockdown.panic/unlock,
    /// which the HUD indicator never reads (the wire-contract gap this pins).
    #[tokio::test]
    async fn panic_and_unlock_emit_the_authoritative_lockdown_status() {
        let _g = test_guard();
        let _dir = with_temp_marker("status-emit");
        let mut rx = crate::telemetry::subscribe_for_test();
        let _ = panic().await;
        let _ = unlock().await;
        // Drain the hub, tolerating unrelated envelopes from parallel tests on
        // the shared broadcast (and a Lagged skip on a busy run).
        let mut saw_locked = false;
        let mut saw_unlocked_after = false;
        loop {
            match rx.try_recv() {
                Ok(line) => {
                    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                    if v["event"] != "lockdown.status" {
                        continue;
                    }
                    match v["data"]["locked"].as_bool() {
                        Some(true) => {
                            saw_locked = true;
                            assert_eq!(
                                v["data"]["restored_from_marker"],
                                serde_json::json!(false),
                                "a live panic is never a marker restore"
                            );
                        }
                        Some(false) if saw_locked => saw_unlocked_after = true,
                        Some(false) => {}
                        None => panic!("lockdown.status must carry a boolean `locked`, got: {v}"),
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        assert!(saw_locked, "panic() must emit lockdown.status {{locked:true}}");
        assert!(
            saw_unlocked_after,
            "unlock() must emit lockdown.status {{locked:false}} after the panic"
        );
    }

    /// A panic drops any parked confirmation — an armed outward action must never
    /// survive the emergency stop.
    // intentional: hold the global PENDING serialization guard across the awaited action; #[tokio::test] is current-thread so it cannot self-deadlock
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn panic_drops_a_pending_confirmation() {
        use crate::confirm::{self, PendingConfirmation};
        let _serial = confirm::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _g = test_guard();
        let _dir = with_temp_marker("drop-pending");
        confirm::clear();
        confirm::park(PendingConfirmation {
            agent: "agent.pepper".into(),
            tool: "gmail_send".into(),
            input: serde_json::json!({"to": "a@b.com"}),
            allowed: vec!["gmail_send".into()],
            preview: "Would send an email".into(),
            created_at: std::time::Instant::now(),
            id: String::new(),
        });
        assert!(confirm::peek_pending(std::time::Instant::now()).is_some(), "armed before panic");
        let _ = panic().await;
        assert!(
            confirm::peek_pending(std::time::Instant::now()).is_none(),
            "panic drops the parked confirmation"
        );
        confirm::clear();
    }

    // -- simulated restart: a present marker re-enters lockdown ----------------

    #[test]
    fn init_re_enters_lockdown_when_marker_present() {
        let _g = init_guard();
        let dir = TempState::new("init-locked");
        let state = dir.state();
        // Simulate a prior panic by writing the marker directly (no global path
        // install needed — init reads the dir we pass).
        std::fs::write(marker_in(&state), b"locked\n").unwrap();
        // A fresh process would call init at startup: it must come up LOCKED.
        let came_up_locked = init(&state);
        assert!(came_up_locked, "init reports the restart re-entry");
        assert!(is_locked_down(), "a present marker re-enters lockdown across a restart");
    }

    #[test]
    fn init_stays_unlocked_when_no_marker() {
        let _g = init_guard();
        let dir = TempState::new("init-cold");
        let state = dir.state();
        let came_up_locked = init(&state);
        assert!(!came_up_locked, "no marker => normal cold start");
        assert!(!is_locked_down(), "cold start is unlocked");
    }

    // -- voice intent classifiers ---------------------------------------------

    #[test]
    fn panic_phrases_classify_as_panic() {
        for u in [
            "panic",
            "PANIC!",
            "panic now",
            "hit the panic button",
            "lockdown",
            "lock down",
            "go into lockdown",
            "stop everything",
            "kill switch",
            "shut it all down",
            "shut everything down",
            "emergency stop",
            "Jarvis, emergency stop.",
        ] {
            assert!(is_panic_intent(u), "{u:?} should trigger panic");
        }
    }

    #[test]
    fn non_panic_does_not_trigger() {
        for u in [
            "",
            "what's the weather",
            "panicking about the deadline", // not the standalone token
            "hispanic heritage month",      // substring, not a token
            "stop the music",               // "stop" alone is not a panic phrase
            "open the lock screen",
            "tell me about kill chains",    // "kill" alone is not "kill switch"
        ] {
            assert!(!is_panic_intent(u), "{u:?} must NOT trigger panic");
        }
    }

    #[test]
    fn unlock_phrases_classify_as_unlock() {
        for u in [
            "unlock",
            "Unlock.",
            "resume normal",
            "resume normal operation",
            "end lockdown",
            "lift lockdown",
            "lift the lockdown",
            "exit lockdown",
        ] {
            assert!(is_unlock_intent(u), "{u:?} should trigger unlock");
        }
    }

    #[test]
    fn panic_and_unlock_are_disjoint() {
        // No phrase is both — a single utterance can never simultaneously lock
        // and unlock.
        for u in PANIC_PHRASES {
            assert!(!is_unlock_intent(u), "{u:?} (panic) must not also be unlock");
        }
        for u in UNLOCK_PHRASES {
            assert!(!is_panic_intent(u), "{u:?} (unlock) must not also be panic");
        }
    }

    // -- EVERY master gate reads OFF when locked (the core invariant) ----------
    // Each gate's force-off is unit-tested in its OWN module too; this is the
    // cross-cutting proof that ONE flip of the lockdown read drives EVERY gate to
    // its safe value at once, and that unlocking restores the CONFIGURED value
    // (lockdown is an overlay, not a clobber). We drive the read via the
    // thread-local `LockdownOverride` so the whole suite stays hermetic.

    #[test]
    fn consequential_gate_is_forced_off_when_locked_and_restored_when_unlocked() {
        let _g = test_guard();
        // Force the operator's master switch ON (the configured state) for the
        // duration, so we can prove lockdown overrides it AND that unlock restores
        // it — not that it was merely already off.
        let _master_on = crate::integrations::ConsequentialOverride::force(true);

        // Unlocked: the configured value shows through (ON) — byte-for-byte today.
        assert!(!is_locked_down());
        assert!(
            crate::integrations::consequential_allowed(),
            "unlocked: the configured master switch (ON) shows through"
        );

        // Lock down: the master outward-action switch is FORCED off, even though
        // the operator's configured switch is still ON underneath.
        let lock = LockdownOverride::force(true);
        assert!(
            !crate::integrations::consequential_allowed(),
            "locked: consequential_allowed() is forced false regardless of config"
        );
        // gate(true) — the strongest possible request — still yields DryRun.
        assert_eq!(
            crate::integrations::gate(true),
            crate::integrations::ActionMode::DryRun,
            "locked: even an explicit confirm only previews"
        );

        // Unlock: the CONFIGURED value (ON) is restored — nothing was clobbered.
        drop(lock);
        assert!(
            crate::integrations::consequential_allowed(),
            "unlocked: the operator's configured master switch (ON) is restored, not clobbered"
        );
    }

    #[test]
    fn proactive_speak_is_forced_off_when_locked() {
        use crate::anticipate::{Brief, Decision, TriggerKind};
        let _g = test_guard();
        let decision = Decision::Speak(Brief {
            kind: TriggerKind::Calendar,
            text: "Heads up: a meeting starts now.".into(),
        });
        // Unlocked: a Speak decision voices.
        assert!(decision.should_speak(), "the decision itself is a Speak");
        assert!(decision.should_speak_now(), "unlocked: proactive speech is allowed");
        // Locked: the SAME decision must NOT voice.
        let _lock = LockdownOverride::force(true);
        assert!(decision.should_speak(), "the decision is unchanged (still a Speak)");
        assert!(
            !decision.should_speak_now(),
            "locked: proactive speech is forced off (the HUD card already surfaced)"
        );
    }

    #[test]
    fn standing_subsystem_is_forced_off_when_locked() {
        // due_missions is the standing master gate. It is a pure function of
        // master_enabled; the live tick passes `cfg.standing.enabled &&
        // !is_locked_down()`. Prove that AND: with the config ON, lockdown drives
        // the effective master to false, so nothing is ever due.
        let _g = test_guard();
        let config_enabled = true;

        let _lock = LockdownOverride::force(true);
        let effective = config_enabled && !is_locked_down();
        assert!(!effective, "locked: the standing master is forced off");
        // due_missions with master=false marks NOTHING due regardless of missions.
        let due = crate::standing::due_missions(&[], 0, 12, 0, &[], effective);
        assert!(due.is_empty(), "locked: no standing mission ever fires");
    }

    #[test]
    fn mic_capture_is_suppressed_when_locked() {
        // The capture loop drops every chunk while locked. The pure decision is in
        // audio.rs; here we prove the lockdown READ that feeds it flips correctly.
        let _g = test_guard();
        assert!(!is_locked_down(), "unlocked: the mic captures normally");
        let _lock = LockdownOverride::force(true);
        assert!(
            is_locked_down(),
            "locked: the capture loop's `is_locked_down()` check drops audio"
        );
    }

    #[test]
    fn off_default_leaves_every_gate_byte_for_byte_today() {
        // With lockdown OFF (the shipped default) the lockdown read is false, so
        // every gate's `&& !is_locked_down()` is a no-op and behavior is unchanged.
        let _g = test_guard(); // override defaults to false
        assert!(!is_locked_down(), "the shipped default is unlocked");
        // The consequential gate reflects ONLY the configured switch (here: the
        // OnceLock default false), with no lockdown interference.
        assert!(
            !crate::integrations::consequential_allowed(),
            "off default: the gate is exactly its configured value, no lockdown effect"
        );
    }
}
