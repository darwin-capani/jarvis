//! MACRO RECORD/REPLAY (#27): record a NAMED sequence of commands and replay it,
//! re-running EACH command through the NORMAL router path + the gate FRESH.
//!
//! A macro is `(name, steps[])` where each step is the recorded UTTERANCE plus the
//! classifier INTENT name it routed to. Two invariants are enforced HERE:
//!
//! - **SECRET-FREE.** A recorded step stores ONLY the (redacted) utterance text and
//!   the intent name. The recorder runs every utterance through
//!   [`crate::optimize::redact`] before persisting (the SAME stripper the audit log
//!   uses), so an email/token/key/long-digit run can never land in the store. No
//!   resolved credential, OAuth bearer, or tool input is ever recorded — a macro is
//!   a list of *what to say*, not *what was authorized*.
//! - **NO GATE BYPASS.** [`replay`] re-runs each recorded utterance through the
//!   injected [`MacroRouter`] — the SAME router path a live utterance takes — ONE
//!   AT A TIME. A consequential step therefore hits the cross-turn confirmation
//!   gate + the `[integrations].allow_consequential` master switch FRESH on replay,
//!   exactly as if spoken live: no pre-approval, no batching, no "the user already
//!   ran this macro once so skip the gate". The macro carries no authority; it only
//!   re-issues the words.
//!
//! OFF by default ([macros].enabled = false): with it off nothing is recorded or
//! replayed. Persisted to the SAME bounded `meta.*` fact store (in-RAM/temp SQLite
//! in tests). HONESTY: a macro stores no secret and never bypasses the gate; replay
//! never fabricates a step result — it returns exactly what the router gave back.

use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::memory::Memory;

/// Default max commands one macro may hold (a bounded sequence).
pub const DEFAULT_MAX_STEPS: usize = 12;
/// Default evict-oldest cap on stored macros (the bounded store).
pub const DEFAULT_RETENTION: usize = 100;

/// Reserved `meta.*` key prefix the macro records live under (internal bookkeeping,
/// filtered from every agent prompt feed + the world model).
const MACRO_PREFIX: &str = "meta.macro.";

/// Bound on a recorded utterance so one step can never grow unbounded.
const MAX_UTTERANCE_LEN: usize = 400;
/// Bound on a macro name.
const MAX_NAME_LEN: usize = 64;

/// One recorded step: the (already-redacted) utterance + the intent it routed to.
/// Carries NO tool input, NO resolved credential, NO confirm bit — only the words +
/// the classifier intent name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroStep {
    /// The redacted command text (what the user said). Secret-free by construction.
    pub utterance: String,
    /// The classifier intent name this routed to (e.g. "web.open"), for the listing
    /// + so replay can note what each step is. Descriptive only.
    pub intent: String,
}

/// A named recorded macro, persisted as one JSON fact row under `meta.macro.<name>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Macro {
    /// The macro's lowercase name (the addressing label for replay/forget).
    pub name: String,
    /// The recorded steps, in order.
    pub steps: Vec<MacroStep>,
    /// RFC3339 creation timestamp.
    pub created: String,
}

/// Normalize a macro name: trimmed, lowercased, bounded. Pure.
pub fn normalize_name(name: &str) -> String {
    let n = name.trim().to_lowercase();
    bound(&n, MAX_NAME_LEN)
}

/// Trim + length-bound a string. Pure.
fn bound(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Build a secret-free [`MacroStep`] from a raw utterance + its intent. The
/// utterance is REDACTED here ([`crate::optimize::redact`]) before it ever reaches
/// the struct, so a secret can never be persisted. Pure — the redaction is the
/// secret-free guarantee, unit-testable without a store.
pub fn make_step(raw_utterance: &str, intent: &str) -> MacroStep {
    let redacted = crate::optimize::redact(raw_utterance.trim());
    MacroStep {
        utterance: bound(&redacted, MAX_UTTERANCE_LEN),
        intent: bound(intent.trim(), 64),
    }
}

// ---------------------------------------------------------------------------
// Spoken macro-command classifier (PURE — the router's live detector)
// ---------------------------------------------------------------------------

/// A recognized spoken macro control command. CONSERVATIVELY anchored (only the
/// explicit phrase shapes classify — a sentence that merely mentions "macro" does
/// NOT trigger), mirroring how the router classifies model-swap / whisper / policy
/// voice commands. This is the heart of the LIVE wiring; it touches no store and is
/// fully unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MacroCommand {
    /// "record a macro called X" / "start recording macro X" — begin capturing.
    StartRecording { name: String },
    /// "stop recording" / "save the macro" — finish capturing + persist.
    StopRecording,
    /// "replay macro X" / "run the macro X" / "play X" — re-run the named macro.
    Replay { name: String },
    /// "list macros" / "what macros do I have" — read-only listing.
    List,
    /// "forget macro X" / "delete macro X" — remove the named macro.
    Forget { name: String },
}

/// Classify a spoken utterance as a macro control command, or None. PURE and
/// conservative: it only fires on clear, anchored phrasings so an ordinary sentence
/// never accidentally toggles recording. Returns the (already-lowercased) macro name
/// for the name-bearing variants.
pub fn classify_macro_command(text: &str) -> Option<MacroCommand> {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches(['.', '!', '?']).trim();

    // STOP first (so "stop recording the macro" is never read as a start).
    if t == "stop recording"
        || t == "stop the recording"
        || t == "save the macro"
        || t == "save macro"
        || t == "finish recording"
        || t == "end recording"
    {
        return Some(MacroCommand::StopRecording);
    }

    // LIST (read-only).
    if t == "list macros"
        || t == "list my macros"
        || t == "what macros do i have"
        || t == "show my macros"
        || t == "show macros"
    {
        return Some(MacroCommand::List);
    }

    // START recording: "record a macro called X" / "start recording a macro called X"
    // / "record macro X".
    for lead in [
        "start recording a macro called ",
        "start recording a macro named ",
        "record a macro called ",
        "record a macro named ",
        "start recording macro ",
        "record macro ",
    ] {
        if let Some(rest) = t.strip_prefix(lead) {
            let name = normalize_name(rest);
            if !name.is_empty() {
                return Some(MacroCommand::StartRecording { name });
            }
        }
    }

    // REPLAY: "replay macro X" / "run the macro X" / "run macro X" / "play macro X".
    for lead in [
        "replay the macro ",
        "replay macro ",
        "run the macro ",
        "run macro ",
        "play the macro ",
        "play macro ",
    ] {
        if let Some(rest) = t.strip_prefix(lead) {
            let name = normalize_name(rest);
            if !name.is_empty() {
                return Some(MacroCommand::Replay { name });
            }
        }
    }

    // FORGET: "forget macro X" / "delete macro X" / "remove macro X".
    for lead in [
        "forget the macro ",
        "forget macro ",
        "delete the macro ",
        "delete macro ",
        "remove the macro ",
        "remove macro ",
    ] {
        if let Some(rest) = t.strip_prefix(lead) {
            let name = normalize_name(rest);
            if !name.is_empty() {
                return Some(MacroCommand::Forget { name });
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Recording buffer (process-global single slot — the live capture state)
// ---------------------------------------------------------------------------

/// The in-progress recording: a name + the captured `(utterance, intent)` steps so
/// far. Single-slot like the confirmation pending — at most one recording is active.
#[derive(Debug, Clone, Default)]
struct Recording {
    name: String,
    steps: Vec<(String, String)>,
}

/// Process-global recording slot. `None` = not recording. A plain
/// `Mutex<Option<_>>` mirrors the daemon's other small shared state (the confirm
/// pending slot). The recorded steps are RAW here; they are redacted at PERSIST time
/// by [`make_step`], so a secret never reaches the store.
static RECORDING: Mutex<Option<Recording>> = Mutex::new(None);

/// Lock the recording slot, recovering from poisoning rather than panicking.
fn rec_lock() -> std::sync::MutexGuard<'static, Option<Recording>> {
    RECORDING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Begin recording a macro under `name`, replacing any in-progress recording.
pub fn start_recording(name: &str) {
    *rec_lock() = Some(Recording {
        name: normalize_name(name),
        steps: Vec::new(),
    });
}

/// Are we currently recording a macro? (The router checks this each turn so it can
/// CAPTURE the command in addition to running it.)
pub fn is_recording() -> bool {
    rec_lock().is_some()
}

/// CAPTURE a routed command into the in-progress recording, if any. The router calls
/// this AFTER a normal command routes (so the command still ran), appending its
/// utterance + intent. No-op when not recording. Capped at a sane bound so a runaway
/// session cannot grow the buffer unbounded; excess is dropped (the macro is
/// clamped to max_steps at persist time anyway).
pub fn capture(utterance: &str, intent: &str) {
    // THRESHOLD write-integrity chokepoint: a GUEST turn leaves NO durable trace.
    // The recording buffer is owner-influencing state — its captured steps become
    // durable (persisted as a macro fact) when the OWNER later says "stop recording",
    // so a bystander's utterance must never be appended to the owner's in-progress
    // macro. Refuse HERE, at the buffer primitive, no matter which caller reaches it.
    // Background tasks read false and are unaffected.
    if crate::threshold::guest_write_blocked() {
        return;
    }
    let mut guard = rec_lock();
    if let Some(rec) = guard.as_mut() {
        if rec.steps.len() < 256 {
            rec.steps.push((utterance.to_string(), intent.to_string()));
        }
    }
}

/// STOP the in-progress recording and return its `(name, steps)`, clearing the slot.
/// None when nothing was recording.
pub fn stop_recording() -> Option<(String, Vec<(String, String)>)> {
    rec_lock().take().map(|r| (r.name, r.steps))
}

/// Unconditionally clear any in-progress recording (e.g. on lockdown/barge).
pub fn clear_recording() {
    *rec_lock() = None;
}

// ---------------------------------------------------------------------------
// The store (persisted via Memory; round-trips against a temp/in-RAM DB in tests)
// ---------------------------------------------------------------------------

/// Record a NAMED macro from a list of `(raw_utterance, intent)` commands. Each
/// utterance is REDACTED before storage (secret-free), the sequence is clamped to
/// `max_steps`, and the record is persisted. Enforces the store retention cap.
/// Returns the stored [`Macro`]. This only writes; it runs nothing.
pub async fn record(
    memory: &Memory,
    retention: usize,
    max_steps: usize,
    name: &str,
    commands: &[(String, String)],
) -> Result<Macro> {
    let name = normalize_name(name);
    let steps: Vec<MacroStep> = commands
        .iter()
        .take(max_steps.max(1))
        .map(|(u, i)| make_step(u, i))
        .collect();
    let m = Macro {
        name,
        steps,
        created: chrono::Utc::now().to_rfc3339(),
    };
    save(memory, &m).await?;
    enforce_retention(memory, retention.max(1)).await?;
    Ok(m)
}

/// Persist a macro as one JSON fact row under `meta.macro.<name>`. Trusted internal
/// write. Idempotent on name (re-recording overwrites in place).
async fn save(memory: &Memory, m: &Macro) -> Result<()> {
    let key = format!("{MACRO_PREFIX}{}", m.name);
    let json = serde_json::to_string(m)?;
    memory.upsert_fact(&key, &json).await
}

/// Load one macro by name. Returns None when absent or malformed.
pub async fn load(memory: &Memory, name: &str) -> Result<Option<Macro>> {
    let key = format!("{MACRO_PREFIX}{}", normalize_name(name));
    let Some(json) = memory.get_fact(&key).await? else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<Macro>(&json).ok())
}

/// Every stored macro, newest first. Malformed rows are skipped.
pub async fn list(memory: &Memory) -> Result<Vec<Macro>> {
    let rows = memory.recall_facts_limited(MACRO_PREFIX, 512).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str::<Macro>(&v).ok())
        .collect())
}

/// Forget (delete) a macro by name. Returns whether a row existed.
pub async fn forget(memory: &Memory, name: &str) -> Result<bool> {
    let key = format!("{MACRO_PREFIX}{}", normalize_name(name));
    memory.delete_fact(&key).await
}

/// Evict the oldest macros past `keep` so the store stays bounded.
async fn enforce_retention(memory: &Memory, keep: usize) -> Result<()> {
    let rows = memory.recall_facts_limited(MACRO_PREFIX, 1024).await?;
    if rows.len() <= keep {
        return Ok(());
    }
    for (key, _) in rows.into_iter().skip(keep) {
        let _ = memory.delete_fact(&key).await;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Replay (the gate-honoring core, generic over an injected router)
// ---------------------------------------------------------------------------

/// The router seam replay re-issues each command through. The PRODUCTION
/// implementation wraps the real `router::route` path (so a replayed utterance goes
/// through the SAME classify -> handler -> gate pipeline a live utterance does);
/// tests inject a mock that records each routed command and models the gate. Each
/// call routes EXACTLY ONE command — replay never batches, so a consequential step
/// hits the confirmation gate + master switch FRESH every time.
pub trait MacroRouter: Send + Sync {
    /// Route one recorded utterance as if spoken live, returning a human-facing
    /// outcome string. A consequential utterance must (by riding the live router)
    /// hit the gate FRESH — replay grants no pre-approval.
    fn route_once<'a>(
        &'a self,
        utterance: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>>;
}

/// One replayed step's outcome (echoed for the spoken summary).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedStep {
    pub utterance: String,
    pub outcome: String,
}

/// REPLAY a macro by name: re-run EACH recorded utterance through the injected
/// [`MacroRouter`], one at a time, returning the per-step outcomes. SAFETY: a
/// consequential step re-enters the live router path here, so it hits the
/// confirmation gate + master switch FRESH (no pre-approval, no batching past the
/// gate). Replay NEVER fabricates a result — each outcome is exactly what the router
/// returned. A missing macro returns an empty list (the caller says so honestly).
pub async fn replay(memory: &Memory, name: &str, router: &dyn MacroRouter) -> Result<Vec<ReplayedStep>> {
    let Some(m) = load(memory, name).await? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(m.steps.len());
    for step in &m.steps {
        // Route THIS utterance fresh through the live router path. Whatever the
        // router does — answer it, or PARK it for a spoken yes because it is
        // consequential — is what we record. We never short-circuit the gate.
        let outcome = router.route_once(&step.utterance).await;
        out.push(ReplayedStep {
            utterance: step.utterance.clone(),
            outcome,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-macros-test-{}-{}.db",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_file(&path);
            TempDb(path)
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(p));
            }
        }
    }

    fn mem(tag: &str) -> (Memory, TempDb) {
        let db = TempDb::new(tag);
        let m = Memory::open(&db.0).unwrap();
        (m, db)
    }

    /// A mock router that records every routed utterance and models the gate: a
    /// "post"/"send" style utterance comes back PARKED for a spoken yes (the
    /// confirmation prompt), exactly as the live router would when the action is
    /// consequential and the master switch is on — proving each consequential step
    /// is RE-GATED on replay, never fired from a stored approval.
    struct MockRouter {
        routed: Mutex<Vec<String>>,
    }
    impl MockRouter {
        fn new() -> Self {
            MockRouter { routed: Mutex::new(Vec::new()) }
        }
        fn routed(&self) -> Vec<String> {
            self.routed.lock().unwrap().clone()
        }
    }
    impl MacroRouter for MockRouter {
        fn route_once<'a>(
            &'a self,
            utterance: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
            Box::pin(async move {
                self.routed.lock().unwrap().push(utterance.to_string());
                let lower = utterance.to_lowercase();
                if lower.contains("post ") || lower.contains("send ") {
                    // A consequential utterance PARKS for a spoken confirmation —
                    // the gate fired fresh; nothing executed.
                    format!("say 'confirm' to proceed: {utterance}")
                } else {
                    format!("done: {utterance}")
                }
            })
        }
    }

    // ---- record stores ONLY redacted utterance + intent (no secret) --------

    #[tokio::test]
    async fn record_strips_secrets_and_stores_only_intent_and_utterance() {
        let (m, _db) = mem("nosecret");
        // A command carrying a token-shaped secret + an email. Both MUST be redacted
        // out before persistence.
        let commands = vec![
            (
                "email the report to alice@example.com using key sk-ABCD1234SECRETTOKENvalue99"
                    .to_string(),
                "comms.send".to_string(),
            ),
            ("open safari".to_string(), "web.open".to_string()),
        ];
        let recorded = record(&m, DEFAULT_RETENTION, DEFAULT_MAX_STEPS, "morning", &commands)
            .await
            .unwrap();

        // The stored step text carries NO secret and NO email.
        let stored_text = recorded
            .steps
            .iter()
            .map(|s| s.utterance.clone())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(
            !stored_text.contains("sk-ABCD1234SECRETTOKEN"),
            "a token-shaped secret must never be stored: {stored_text}"
        );
        assert!(
            !stored_text.contains("alice@example.com"),
            "an email must be redacted before storage: {stored_text}"
        );
        assert!(stored_text.contains("[redacted]"), "redaction markers present: {stored_text}");

        // Re-load from the store and re-check (the persisted bytes, not just the
        // in-memory return).
        let loaded = load(&m, "morning").await.unwrap().unwrap();
        let json = serde_json::to_string(&loaded).unwrap();
        assert!(!json.contains("sk-ABCD1234SECRETTOKEN"), "no secret in the stored JSON: {json}");
        assert!(!json.contains("alice@example.com"), "no email in the stored JSON: {json}");
        // The intent names ARE stored (descriptive, not secret).
        assert_eq!(loaded.steps[1].intent, "web.open");
    }

    // ---- record -> list -> forget round-trip -------------------------------

    #[tokio::test]
    async fn record_list_forget_roundtrip_and_max_steps() {
        let (m, _db) = mem("roundtrip");
        // More commands than max_steps -> clamped.
        let commands: Vec<(String, String)> = (0..20)
            .map(|i| (format!("do step {i}"), "conversation".to_string()))
            .collect();
        let recorded = record(&m, DEFAULT_RETENTION, 3, "big", &commands).await.unwrap();
        assert_eq!(recorded.steps.len(), 3, "a macro is clamped to max_steps");

        let listed = list(&m).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "big");

        assert!(forget(&m, "BIG").await.unwrap(), "forget is name-insensitive");
        assert!(!forget(&m, "big").await.unwrap());
        assert!(list(&m).await.unwrap().is_empty());
    }

    // ---- replay re-routes each command through the gate --------------------

    #[tokio::test]
    async fn replay_reroutes_each_command_and_regates_consequential_steps() {
        let (m, _db) = mem("replay");
        let commands = vec![
            ("open safari".to_string(), "web.open".to_string()),
            (
                "post the launch update to the team channel".to_string(),
                "comms.post".to_string(),
            ),
        ];
        record(&m, DEFAULT_RETENTION, DEFAULT_MAX_STEPS, "launch", &commands)
            .await
            .unwrap();

        let router = MockRouter::new();
        let results = replay(&m, "launch", &router).await.unwrap();

        // EACH recorded command was re-routed through the router — one at a time.
        assert_eq!(router.routed().len(), 2, "every command is re-routed (no batching)");
        assert_eq!(results.len(), 2);

        // The CONSEQUENTIAL step ("post ...") came back PARKED for a fresh spoken
        // confirmation — the gate fired again on replay; it was NOT auto-executed.
        let consequential = results
            .iter()
            .find(|r| r.utterance.contains("post the launch update"))
            .unwrap();
        assert!(
            consequential.outcome.to_lowercase().contains("confirm"),
            "a replayed consequential step must re-hit the gate (park for confirm): {}",
            consequential.outcome
        );
        assert!(
            !consequential.outcome.to_lowercase().contains("posted to"),
            "the replayed step must NOT have executed: {}",
            consequential.outcome
        );
    }

    #[tokio::test]
    async fn replay_of_unknown_macro_is_empty_not_fabricated() {
        let (m, _db) = mem("unknown");
        let router = MockRouter::new();
        let results = replay(&m, "nope", &router).await.unwrap();
        assert!(results.is_empty(), "an unknown macro replays nothing, never a fabricated step");
        assert!(router.routed().is_empty(), "nothing is routed for a missing macro");
    }

    #[test]
    fn make_step_redacts_and_normalize_name_lowercases() {
        let step = make_step("  call +1 (415) 555-0123 now  ", "phone.call");
        assert!(step.utterance.contains("[redacted]"), "phone number redacted: {}", step.utterance);
        assert_eq!(step.intent, "phone.call");
        assert_eq!(normalize_name("  Morning Brief "), "morning brief");
    }

    // ---- the spoken classifier is conservative + anchored ------------------

    #[test]
    fn classify_macro_command_is_anchored_and_conservative() {
        // Start / stop / list / forget / replay are recognized on anchored phrasings.
        assert_eq!(
            classify_macro_command("record a macro called morning"),
            Some(MacroCommand::StartRecording { name: "morning".to_string() })
        );
        assert_eq!(
            classify_macro_command("Start recording macro Launch."),
            Some(MacroCommand::StartRecording { name: "launch".to_string() })
        );
        assert_eq!(classify_macro_command("stop recording"), Some(MacroCommand::StopRecording));
        assert_eq!(classify_macro_command("list macros"), Some(MacroCommand::List));
        assert_eq!(
            classify_macro_command("replay macro morning"),
            Some(MacroCommand::Replay { name: "morning".to_string() })
        );
        assert_eq!(
            classify_macro_command("forget macro morning"),
            Some(MacroCommand::Forget { name: "morning".to_string() })
        );

        // A sentence that merely MENTIONS a macro never triggers (conservative).
        assert_eq!(classify_macro_command("what does macro mean"), None);
        assert_eq!(classify_macro_command("the recording studio downtown"), None);
        assert_eq!(classify_macro_command("open safari"), None);
        // A start with no name is not a valid command.
        assert_eq!(classify_macro_command("record a macro called "), None);
    }

    // ---- the recording buffer captures only, never changes a gate ----------

    #[test]
    fn recording_buffer_captures_and_stops() {
        // Serialize every test that touches the PROCESS-GLOBAL recording slot on the
        // crate-wide test lock so the guest-guard test below cannot interleave.
        let _g = crate::confirm::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_recording();
        assert!(!is_recording());

        start_recording("MyMacro");
        assert!(is_recording());
        capture("open safari", "web.open");
        capture("post to slack", "comms.post");

        let (name, steps) = stop_recording().expect("a recording was in progress");
        assert_eq!(name, "mymacro");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0], ("open safari".to_string(), "web.open".to_string()));
        assert!(!is_recording(), "stop clears the slot");

        // capture is a no-op when not recording.
        capture("stray command", "conversation");
        assert!(stop_recording().is_none());
        clear_recording();
    }

    /// THRESHOLD write-integrity chokepoint (macros::capture). The recording buffer's
    /// captured steps become DURABLE at "stop recording", so a GUEST turn must append
    /// NOTHING even while a recording is in progress — a bystander's utterance can
    /// never end up in the owner's saved macro. The owner's own steps still buffer.
    #[test]
    fn guest_turn_captures_nothing_into_the_recording_buffer() {
        let _g = crate::confirm::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_recording();
        start_recording("gt");
        // GUEST: capture appends nothing, even mid-recording.
        {
            let guest = crate::threshold::guest_from(
                &crate::threshold::Scope::owner(vec!["*".to_string()], crate::focus::FocusProfile::Default),
                &crate::focus::FocusProfile::DeepFocus,
            );
            let _o = crate::threshold::ScopeOverride::guest(guest);
            assert!(crate::threshold::is_guest_turn());
            capture("guest secret command", "conversation");
        }
        // OWNER: capture appends as usual.
        capture("owner command", "web.open");
        let (name, steps) = stop_recording().expect("a recording was in progress");
        assert_eq!(name, "gt");
        assert_eq!(
            steps,
            vec![("owner command".to_string(), "web.open".to_string())],
            "the guest capture must be absent — only the owner's step is buffered"
        );
        clear_recording();
    }
}
