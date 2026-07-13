//! REVERSIBLE-ACTION JOURNAL + ONE-WORD UNDO (F2) — a bounded, in-process,
//! session-scoped ledger of every consequential action that ACTUALLY EXECUTED,
//! each labelled with an HONEST inverse verdict, plus the "undo that" voice
//! intent that arms the inverse — through the SAME gates as a live command.
//!
//! THE HARD SAFETY RULE: undo NEVER executes anything itself. Arming an undo
//! calls `anthropic::execute_tool` with the inverse — byte-for-byte the entry
//! point a live utterance's tool call uses — so the inverse gets the identical
//! treatment: voice-id refusal, faithful dry-run preview, the policy layer
//! (NEVER/ALWAYS/ASK), the master switch ceiling, and the single-slot spoken
//! confirmation park. A gated inverse parks for its own fresh "confirm"; a
//! reversible-by-design inverse (standing_cancel) runs directly, exactly as if
//! the user had spoken it — undo grants NOTHING the spoken command would not.
//!
//! HONESTY CONTRACT:
//!   * Only actions that genuinely executed are recorded — never dry-run
//!     previews, never refusals (the caller passes `executed`, derived from the
//!     master-switch gate + the dispatch error flag at the chokepoint).
//!   * The inverse verdict never over-claims: most consequential actions are
//!     IRREVERSIBLE (sent mail, public posts, shell commands) and the entry
//!     says so with the specific reason, rather than pretending.
//!   * Risk-asymmetric inverses are refused BY POLICY: undoing a door `lock`
//!     would mean arming an `unlock` — never offered as an undo.
//!   * The emitted `journal.snapshot` is SECRET-FREE: it carries the audit-grade
//!     preview text, never the raw tool input (which may hold message bodies).
//!
//! Bounded + session-scoped: at most [`JOURNAL_CAP`] entries in a process-global
//! slot (the confirm-pending store pattern); a daemon restart starts an empty
//! journal, which the snapshot reports honestly ("this session").

use std::sync::Mutex;

use serde_json::{json, Value};

/// Hard cap on retained entries — evict-oldest beyond this. 64 consequential
/// executions is far more than a session plausibly accumulates.
const JOURNAL_CAP: usize = 64;
/// How many (newest-first) entries the HUD snapshot carries per tick.
const SNAPSHOT_ENTRIES: usize = 12;

/// The honest inverse verdict for one executed action, decided at RECORD time
/// (pure over the tool name + input, so it is exhaustively unit-testable).
#[derive(Debug, Clone, PartialEq)]
pub enum Inverse {
    /// A safe mechanical inverse exists using an ALREADY-WIRED tool. `note`
    /// carries the honest caveat spoken alongside the confirmation prompt (e.g.
    /// re-enabling a campaign resumes live spend), empty when there is none.
    Ready { tool: String, input: Value, note: String },
    /// No safe inverse — with the specific, honest reason why not.
    None { why: String },
}

/// One executed consequential action. The raw tool input is NOT retained: the
/// inverse is derived from it at record time and only that inverse (plus the
/// secret-free preview) is kept — the ledger never holds message bodies.
#[derive(Debug, Clone)]
pub struct JournalEntry {
    /// Monotonic per-session sequence (1-based). Stable handle for undo marking.
    pub seq: u64,
    /// RFC3339 wall-clock record time (house telemetry style).
    pub ts: String,
    /// The agent namespace that executed the action ("agent.<name>").
    pub agent: String,
    /// The executed tool name.
    pub tool: String,
    /// The secret-free dry-run preview (the same audit-grade text the
    /// confirmation gate showed). This IS what the snapshot emits.
    pub preview: String,
    /// The agent's allowlist snapshot at execution time. An undo re-parks under
    /// the SAME agent + allowlist, so the replay's `agent_may_use` re-check
    /// applies to the inverse exactly as it did to the forward action.
    pub allowed: Vec<String>,
    /// The honest inverse verdict, decided at record time.
    pub inverse: Inverse,
    /// Set when this action's inverse has since executed.
    pub undone: bool,
    /// How the execution was approved: "confirm" (spoken/HUD yes) or "policy"
    /// (an owner-installed Always rule auto-approved it).
    pub via: &'static str,
}

/// An armed undo: when the content-derived pending id of a later EXECUTION
/// matches `pending_id`, entry `seq` is marked undone. Content-derived matching
/// means even an independently spoken inverse (the user says "enable campaign
/// 123" themselves after a pause) honestly flips the flag.
#[derive(Debug, Clone)]
struct ExpectedUndo {
    seq: u64,
    pending_id: String,
}

#[derive(Debug)]
struct JournalState {
    next_seq: u64,
    entries: Vec<JournalEntry>,
    expected: Option<ExpectedUndo>,
}

/// Process-global journal — the daemon's other small shared state (the confirm
/// pending slot, the macro recording slot) uses the same const `Mutex` pattern.
static JOURNAL: Mutex<JournalState> =
    Mutex::new(JournalState { next_seq: 1, entries: Vec::new(), expected: None });

/// Lock the journal, recovering from poisoning rather than panicking — a
/// bookkeeping ledger must never wedge the daemon.
fn lock() -> std::sync::MutexGuard<'static, JournalState> {
    JOURNAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Test-only reset. Tests that touch this process-global store serialize on
/// `confirm::PENDING_TEST_LOCK` (the crate-wide global-state lock): the
/// anthropic replay tests that hold it are exactly the tests that also write
/// here, so sharing that one lock prevents cross-module races.
#[cfg(test)]
pub(crate) fn clear_for_test() {
    let mut g = lock();
    g.next_seq = 1;
    g.entries.clear();
    g.expected = None;
}

// ---------------------------------------------------------------------------
// Recording (called from the execution chokepoints in anthropic.rs)
// ---------------------------------------------------------------------------

/// Is this tool's EXECUTION gated by the consequential master switch? Used by
/// the chokepoints to decide whether `consequential_allowed()` participates in
/// the "did it actually execute" predicate (a master-off replay returns a
/// dry-run preview with is_error=false — that must never be journaled).
pub fn master_gated(tool: &str) -> bool {
    crate::confirm::is_consequential_tool(tool)
        || tool == "skill_invoke"
        || crate::mcp::is_mcp_flat_name(tool)
}

/// Record one consequential execution. No-op unless `executed` is true — the
/// journal holds ONLY actions that genuinely fired (the caller derives
/// `executed` from the gate + dispatch error at the chokepoint). `outcome` is
/// the dispatch's returned text: some arms report failure as friendly Ok prose
/// (standing_create's cap refusal), so [`execution_confirmed`] refines the
/// verdict per tool before anything is recorded. Derives the honest inverse,
/// marks a matching armed undo, evicts past the cap, and emits the secret-free
/// `journal.recorded` event.
#[allow(clippy::too_many_arguments)]
pub fn record_executed(
    agent: &str,
    tool: &str,
    input: &Value,
    preview: &str,
    outcome: &str,
    allowed: &[String],
    executed: bool,
    via: &'static str,
) {
    if !executed || !execution_confirmed(tool, outcome) {
        return;
    }
    record_at(agent, tool, input, preview, outcome, allowed, via, chrono::Utc::now().to_rfc3339());
}

/// Per-tool refinement of "did the Ok outcome really mean the action fired":
/// some dispatch arms return failure as friendly Ok prose. standing_create's
/// success text is the ONLY one that carries the saved-mission id marker
/// (anthropic.rs::standing_create_tool), so its absence means the create was
/// refused (e.g. the active-mission cap) — never journal it. Everything else
/// keeps the caller's verdict.
fn execution_confirmed(tool: &str, outcome: &str) -> bool {
    match tool {
        "standing_create" => extract_standing_id(outcome).is_some(),
        _ => true,
    }
}

/// Pull the saved mission id out of standing_create_tool's SUCCESS outcome:
/// `... It's saved (id {12-hex}). ...` (anthropic.rs::standing_create_tool —
/// the format is pinned by tests there and here). Extracting the id the
/// mission was ACTUALLY saved under (rather than re-deriving it from the goal)
/// keeps the inverse exact even where create normalizes/truncates the goal
/// before hashing (standing.rs::bound_goal).
fn extract_standing_id(outcome: &str) -> Option<String> {
    let rest = outcome.split("It's saved (id ").nth(1)?;
    let id: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    let terminated = rest[id.len()..].starts_with(')');
    (terminated && !id.is_empty()).then_some(id)
}

/// Per-tool refinement of "did the inverse's Ok outcome really undo it": the
/// ungated standing_cancel returns its no-op miss ("I have no standing mission
/// with id ...") and its failure ("I couldn't cancel ...") as friendly Ok
/// prose, and only its success starts with "Cancelled standing mission"
/// (anthropic.rs::standing_cancel_tool). The router consults this before
/// claiming "Undone." or flipping the ledger flag. Everything else keeps the
/// caller's verdict.
pub fn inverse_confirmed(tool: &str, outcome: &str) -> bool {
    match tool {
        "standing_cancel" => outcome.trim_start().starts_with("Cancelled standing mission"),
        _ => true,
    }
}

/// The testable body of [`record_executed`] with an injectable timestamp.
#[allow(clippy::too_many_arguments)]
fn record_at(
    agent: &str,
    tool: &str,
    input: &Value,
    preview: &str,
    outcome: &str,
    allowed: &[String],
    via: &'static str,
    ts: String,
) {
    let inverse = derive_inverse(tool, input, outcome);
    // Redact the preview at record time — the SAME discipline the audit log
    // applies to its target summary (audit.rs::record redacts again via
    // optimize::redact). Dry-run previews faithfully name recipients/commands,
    // so the raw text is NOT secret-free; only the redacted form may be stored
    // or broadcast.
    let preview = crate::optimize::redact(preview);
    let mut g = lock();

    // A matching armed undo: this very execution IS the inverse of an earlier
    // entry — flip that entry's flag. Matching is by the content-derived pending
    // id (agent || tool || input), so only the exact armed inverse matches.
    if let Some(exp) = g.expected.clone() {
        let id = crate::confirm::derive_pending_id(agent, tool, input);
        if id == exp.pending_id {
            if let Some(target) = g.entries.iter_mut().find(|e| e.seq == exp.seq) {
                target.undone = true;
            }
            g.expected = None;
        }
    }

    let seq = g.next_seq;
    g.next_seq += 1;
    let undoable = matches!(inverse, Inverse::Ready { .. });
    g.entries.push(JournalEntry {
        seq,
        ts,
        agent: agent.to_string(),
        tool: tool.to_string(),
        preview,
        allowed: allowed.to_vec(),
        inverse,
        undone: false,
        via,
    });
    if g.entries.len() > JOURNAL_CAP {
        let excess = g.entries.len() - JOURNAL_CAP;
        g.entries.drain(..excess);
    }
    drop(g);

    crate::telemetry::emit(
        "system",
        "journal.recorded",
        json!({"tool": tool, "agent": agent, "undoable": undoable, "via": via}),
    );
}

/// Mark entry `seq` undone directly — used when an UNGATED reversible inverse
/// (e.g. standing_cancel) executed immediately instead of parking.
pub fn mark_undone(seq: u64) {
    let mut g = lock();
    if let Some(e) = g.entries.iter_mut().find(|e| e.seq == seq) {
        e.undone = true;
    }
    g.expected = None;
}

// ---------------------------------------------------------------------------
// The honest inverse derivation (pure)
// ---------------------------------------------------------------------------

/// Decide the honest inverse for an executed tool call. PURE over
/// (tool, input, outcome) so every verdict is unit-tested. The verdicts never
/// over-claim: an inverse is `Ready` ONLY when an already-wired tool can
/// mechanically reverse the action, and risk-asymmetric reversals (arming an
/// `unlock`) are refused by policy. `outcome` supplies created-object ids that
/// only exist in the dispatch's returned text (the standing mission id).
pub fn derive_inverse(tool: &str, input: &Value, outcome: &str) -> Inverse {
    let field = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    match tool {
        // -- Ads campaign state: mutual inverse pairs (both tools are wired). --
        // Re-enabling is the risk-carrying direction: it resumes live spend, and
        // is only a true inverse if the campaign was running before the pause.
        // The note is spoken with the confirmation prompt so the user decides.
        "gads_pause_campaign" => Inverse::Ready {
            tool: "gads_enable_campaign".into(),
            input: json!({"campaign_id": field("campaign_id")}),
            note: "re-enabling resumes live spend, and is only right if the campaign was running before the pause".into(),
        },
        "gads_enable_campaign" => Inverse::Ready {
            tool: "gads_pause_campaign".into(),
            input: json!({"campaign_id": field("campaign_id")}),
            note: String::new(),
        },
        "meta_pause_campaign" => Inverse::Ready {
            tool: "meta_resume_campaign".into(),
            input: json!({"campaign_id": field("campaign_id")}),
            note: "resuming restarts live spend, and is only right if the campaign was running before the pause".into(),
        },
        "meta_resume_campaign" => Inverse::Ready {
            tool: "meta_pause_campaign".into(),
            input: json!({"campaign_id": field("campaign_id")}),
            note: String::new(),
        },
        // -- Budgets: the PRIOR value is not readable by any wired tool, so an
        // honest inverse is impossible (re-setting needs the old number). -----
        "gads_set_budget" | "meta_set_budget" => Inverse::None {
            why: "the previous budget isn't recorded anywhere I can read, so I can't restore it".into(),
        },
        // -- Smart home: symmetric services invert; risk-asymmetric ones don't.
        "dume_control" => {
            let entity = field("entity_id");
            let inverse_service = match field("action").as_str() {
                "turn_on" => Some(("turn_off", String::new())),
                "turn_off" => Some((
                    "turn_on",
                    "this restores power only — any prior brightness or settings aren't recorded".to_string(),
                )),
                // Undoing an `unlock` RE-SECURES — the safe direction.
                "unlock" => Some(("lock", String::new())),
                // Undoing a `lock` would mean arming an UNLOCK. Never offered:
                // unlocking a physical door is not risk-symmetric with locking it.
                "lock" => {
                    return Inverse::None {
                        why: "undoing a lock would mean unlocking — I won't arm that as an undo; say 'unlock' yourself if you mean it".into(),
                    }
                }
                "set" => {
                    return Inverse::None {
                        why: "I don't record the device's prior settings, so I can't restore them".into(),
                    }
                }
                _ => None,
            };
            match inverse_service {
                Some((service, note)) if !entity.is_empty() => Inverse::Ready {
                    tool: "dume_control".into(),
                    input: json!({"entity_id": entity, "action": service}),
                    note,
                },
                _ => Inverse::None {
                    why: "I only know safe inverses for turning devices on/off and re-locking".into(),
                },
            }
        }
        // -- Standing missions: the cleanest pair. standing_cancel is wired and
        // deliberately ungated (reversible). The id comes from the SUCCESS
        // outcome — the id the mission was ACTUALLY saved under — never
        // re-derived from the goal (create truncates/normalizes the goal via
        // standing.rs::bound_goal before hashing, so a re-derivation over the
        // raw input can name a mission that doesn't exist). ------------------
        "standing_create" => match extract_standing_id(outcome) {
            Some(id) => Inverse::Ready {
                tool: "standing_cancel".into(),
                input: json!({"id": id}),
                note: String::new(),
            },
            None => Inverse::None {
                why: "the saved mission's id wasn't in the result, so I can't address it".into(),
            },
        },
        // -- Honest irreversibles, each with its specific reason. -------------
        "gmail_send" => Inverse::None { why: "sent mail can't be unsent".into() },
        "slack_post_message" => Inverse::None {
            why: "the posted message's id isn't captured and no delete call is wired".into(),
        },
        "gcal_create_event" => Inverse::None {
            why: "no calendar-delete call is wired, and any attendee invites already went out".into(),
        },
        "github_comment_issue" => Inverse::None {
            why: "no comment-delete call is wired, and watchers were already notified".into(),
        },
        "github_open_pr" => Inverse::None {
            why: "no close-PR call is wired — the PR number is in the result if you want to close it yourself".into(),
        },
        "gdrive_upload_text" => Inverse::None {
            why: "no Drive-delete call is wired — the file id is in the result if you want to remove it yourself".into(),
        },
        "x_post" => Inverse::None {
            why: "no delete call is wired, and the post was already public".into(),
        },
        "linkedin_post" => Inverse::None {
            why: "the post's id isn't captured and no delete call is wired".into(),
        },
        "shell_run" => Inverse::None {
            why: "a shell command's effects can't be mechanically reversed".into(),
        },
        "ui_actuate" => Inverse::None {
            why: "a UI action's downstream effect is app-defined — there's no safe mechanical inverse".into(),
        },
        "connector_add" => Inverse::None {
            why: "no connector-remove primitive exists; the connector was added inert — delete its block from jarvis.toml to undo".into(),
        },
        "skill_invoke" => Inverse::None {
            why: "a consequential skill's effects aren't mechanically reversible".into(),
        },
        // MCP + anything unknown: never guess an inverse.
        _ => Inverse::None { why: "I don't know a safe inverse for this tool".into() },
    }
}

// ---------------------------------------------------------------------------
// The "undo that" intent
// ---------------------------------------------------------------------------

/// The recognized undo voice commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoCommand {
    /// "undo that" — arm the inverse of the last executed action.
    UndoLast,
    /// "what can you undo" — answer honestly, arm nothing.
    Status,
}

/// Classify a spoken utterance as an undo command, or None. PURE and
/// conservative, exactly like `macros::classify_macro_command`: only clear,
/// anchored phrasings fire — a sentence that merely mentions "undo" (a question
/// about git, an aside) never triggers. A QUESTION about undo ("can you undo
/// that") is Status, never UndoLast: an ambiguous ask must not arm an action.
pub fn classify_undo_command(text: &str) -> Option<UndoCommand> {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches(['.', '!', '?']).trim().trim_end_matches(',').trim();

    const UNDO_PHRASES: &[&str] = &[
        "undo",
        "undo that",
        "undo it",
        "undo this",
        "undo last action",
        "undo the last action",
        "undo that action",
        "undo my last action",
        "undo your last action",
        "revert that",
        "revert it",
        "revert the last action",
        "take that back",
    ];
    if UNDO_PHRASES.contains(&t) {
        return Some(UndoCommand::UndoLast);
    }

    const STATUS_PHRASES: &[&str] = &[
        "what can you undo",
        "can you undo that",
        "can you undo it",
        "can you undo",
        "is that undoable",
        "can that be undone",
    ];
    if STATUS_PHRASES.contains(&t) {
        return Some(UndoCommand::Status);
    }
    None
}

/// What arming an undo would do, decided over the journal. The `Ready` variant
/// carries everything the router needs to hand the inverse to
/// `anthropic::execute_tool` — which then applies the FULL live-command
/// treatment (voice-id, preview, policy, master ceiling, spoken-confirm park).
#[derive(Debug, Clone)]
pub enum UndoPrep {
    /// Nothing consequential has executed this session.
    Nothing,
    /// The last action was already undone.
    AlreadyUndone,
    /// The last action has no safe inverse — with the honest reason.
    Irreversible { why: String },
    /// The inverse is ready to arm.
    Ready {
        seq: u64,
        agent: String,
        tool: String,
        input: Value,
        allowed: Vec<String>,
        note: String,
        /// The content-derived id the inverse's park WILL get — armed as the
        /// expected-undo so the eventual execution flips the forward entry.
        pending_id: String,
    },
}

/// Prepare an undo of the LAST journal entry — deliberately the last entry, not
/// the last *invertible* one: "undo that" must never silently reach past what
/// the user means by "that" and revert something older. Arms the expected-undo
/// match when Ready.
pub fn prepare_undo() -> UndoPrep {
    let mut g = lock();
    let Some(last) = g.entries.last().cloned() else {
        return UndoPrep::Nothing;
    };
    if last.undone {
        return UndoPrep::AlreadyUndone;
    }
    match last.inverse.clone() {
        Inverse::None { why } => UndoPrep::Irreversible { why },
        Inverse::Ready { tool, input, note } => {
            let pending_id = crate::confirm::derive_pending_id(&last.agent, &tool, &input);
            g.expected = Some(ExpectedUndo { seq: last.seq, pending_id: pending_id.clone() });
            UndoPrep::Ready {
                seq: last.seq,
                agent: last.agent,
                tool,
                input,
                allowed: last.allowed,
                note,
                pending_id,
            }
        }
    }
}

/// The honest spoken answer for "what can you undo". Read-only.
pub fn status_line() -> String {
    let g = lock();
    match g.entries.last() {
        None => "Nothing consequential has executed this session, so there's nothing to undo.".to_string(),
        Some(e) if e.undone => format!(
            "The last consequential action ({}) was already undone.",
            short_desc(&e.preview, &e.tool)
        ),
        Some(e) => match &e.inverse {
            Inverse::Ready { .. } => format!(
                "The last consequential action was {} — say 'undo that' and I'll arm the inverse for your confirmation.",
                short_desc(&e.preview, &e.tool)
            ),
            Inverse::None { why } => format!(
                "The last consequential action was {} — I can't undo it: {}.",
                short_desc(&e.preview, &e.tool),
                why
            ),
        },
    }
}

/// Compose the spoken reply after the router handed the inverse to
/// `execute_tool`. PURE so the phrasing is unit-tested. `parked` = the inverse
/// is now the pending confirmation (its prompt already says how to confirm);
/// `executed_directly` = an ungated reversible inverse (or a policy-Always
/// approval) already ran.
pub fn compose_undo_response(
    outcome: &str,
    is_error: bool,
    parked: bool,
    executed_directly: bool,
    note: &str,
) -> String {
    if is_error {
        // Voice-id refusal, policy Never, provider error — relay honestly.
        return outcome.to_string();
    }
    if parked {
        // The park prompt already carries the faithful preview + "say 'confirm'".
        return if note.is_empty() {
            outcome.to_string()
        } else {
            format!("Heads up: {note}. {outcome}")
        };
    }
    if executed_directly {
        return format!("Undone. {outcome}");
    }
    // Master switch off: the outcome is the honest dry-run preview.
    outcome.to_string()
}

/// A short, spoken-length description of an entry (preview first sentence,
/// falling back to the tool name). Sentence boundary is ". " (dot + space) —
/// never a bare '.', which would cut email addresses, domains, and ids
/// mid-token ("send an email to x@y" from "x@y.z").
fn short_desc(preview: &str, tool: &str) -> String {
    let p = preview.trim().trim_start_matches("[dry run]").trim();
    let line = p.lines().next().unwrap_or("");
    let first = line.split(". ").next().unwrap_or("").trim().trim_end_matches('.');
    if first.is_empty() {
        format!("a '{tool}' action")
    } else {
        let mut s: String = first.chars().take(120).collect();
        if first.chars().count() > 120 {
            s.push('…');
        }
        s
    }
}

// ---------------------------------------------------------------------------
// The HUD snapshot (READ-ONLY, SECRET-FREE)
// ---------------------------------------------------------------------------

/// Build the secret-free `journal.snapshot` payload: the newest entries
/// (preview text only — NEVER the raw input), each with its honest undo verdict.
/// PURE over the store contents.
pub fn snapshot_payload() -> Value {
    let g = lock();
    let entries: Vec<Value> = g
        .entries
        .iter()
        .rev()
        .take(SNAPSHOT_ENTRIES)
        .map(|e| {
            let (undoable, note) = match &e.inverse {
                Inverse::Ready { note, .. } => (true, note.clone()),
                Inverse::None { why } => (false, why.clone()),
            };
            json!({
                "ts": e.ts,
                "agent": e.agent,
                "tool": e.tool,
                "preview": e.preview,
                "undoable": undoable,
                "note": note,
                "undone": e.undone,
                "via": e.via,
            })
        })
        .collect();
    json!({ "count": g.entries.len(), "entries": entries })
}

/// Emit the `journal.snapshot` telemetry for the HUD panel. READ-ONLY — called
/// on the audit-snapshot cadence.
pub fn emit_snapshot() {
    crate::telemetry::emit("system", "journal.snapshot", snapshot_payload());
}

// ---------------------------------------------------------------------------
// Tests — store tests serialize on confirm::PENDING_TEST_LOCK (the crate-wide
// global-state lock) because the anthropic replay tests that hold it also
// write this journal; pure fns (classifier, derive_inverse, compose) need none.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Take the crate-wide global-state lock and reset the journal.
    fn store_guard() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::confirm::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_for_test();
        g
    }

    fn record(agent: &str, tool: &str, input: Value, preview: &str) {
        record_at(agent, tool, &input, preview, "", &["a".into(), "b".into()], "confirm", "2026-07-12T00:00:00Z".into());
    }

    // -- classifier ----------------------------------------------------------

    #[test]
    fn classify_undo_is_anchored_and_conservative() {
        assert_eq!(classify_undo_command("undo"), Some(UndoCommand::UndoLast));
        assert_eq!(classify_undo_command("Undo that."), Some(UndoCommand::UndoLast));
        assert_eq!(classify_undo_command("undo it!"), Some(UndoCommand::UndoLast));
        assert_eq!(classify_undo_command("take that back"), Some(UndoCommand::UndoLast));
        assert_eq!(classify_undo_command("revert the last action"), Some(UndoCommand::UndoLast));
        // Questions are Status — an ambiguous ask never arms an action.
        assert_eq!(classify_undo_command("what can you undo?"), Some(UndoCommand::Status));
        assert_eq!(classify_undo_command("can you undo that"), Some(UndoCommand::Status));
        // Ordinary sentences mentioning undo never trigger.
        assert_eq!(classify_undo_command("how do I undo a git commit"), None);
        assert_eq!(classify_undo_command("undo the volume change and then send it"), None);
        assert_eq!(classify_undo_command("what does undo mean"), None);
        assert_eq!(classify_undo_command(""), None);
    }

    // -- derive_inverse ------------------------------------------------------

    #[test]
    fn campaign_pause_and_enable_are_mutual_inverses_with_honest_spend_note() {
        let inv = derive_inverse("gads_pause_campaign", &json!({"campaign_id": "123"}), "");
        match inv {
            Inverse::Ready { tool, input, note } => {
                assert_eq!(tool, "gads_enable_campaign");
                assert_eq!(input, json!({"campaign_id": "123"}));
                assert!(note.contains("live spend"), "risk named: {note}");
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        // The safe direction (pausing) carries no caveat.
        match derive_inverse("meta_resume_campaign", &json!({"campaign_id": "9"}), "") {
            Inverse::Ready { tool, note, .. } => {
                assert_eq!(tool, "meta_pause_campaign");
                assert!(note.is_empty());
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn budgets_are_honestly_irreversible_because_prior_value_is_unreadable() {
        for tool in ["gads_set_budget", "meta_set_budget"] {
            match derive_inverse(tool, &json!({"budget_id": "b", "amount": 5}), "") {
                Inverse::None { why } => assert!(why.contains("previous budget"), "{why}"),
                other => panic!("expected None, got {other:?}"),
            }
        }
    }

    #[test]
    fn dume_symmetric_services_invert_but_lock_is_never_undone_by_unlocking() {
        match derive_inverse("dume_control", &json!({"entity_id": "light.x", "action": "turn_on"}), "") {
            Inverse::Ready { tool, input, .. } => {
                assert_eq!(tool, "dume_control");
                assert_eq!(input, json!({"entity_id": "light.x", "action": "turn_off"}));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        // unlock -> lock (re-securing is the safe direction).
        match derive_inverse("dume_control", &json!({"entity_id": "lock.door", "action": "unlock"}), "") {
            Inverse::Ready { input, .. } => {
                assert_eq!(input, json!({"entity_id": "lock.door", "action": "lock"}));
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        // lock -> REFUSED: an undo must never arm an unlock.
        match derive_inverse("dume_control", &json!({"entity_id": "lock.door", "action": "lock"}), "") {
            Inverse::None { why } => assert!(why.contains("won't arm"), "{why}"),
            other => panic!("expected None, got {other:?}"),
        }
        // set -> prior settings unreadable.
        assert!(matches!(
            derive_inverse("dume_control", &json!({"entity_id": "light.x", "action": "set"}), ""),
            Inverse::None { .. }
        ));
    }

    /// The exact success format of anthropic.rs::standing_create_tool — the id
    /// marker this module extracts is pinned HERE against wording drift there.
    const STANDING_OK: &str = "Standing mission established: \"Check the mail\" — daily. \
        It's saved (id abc123def456). The standing-missions subsystem is on by default, \
        so it will run on schedule; any consequential step it proposes will still wait \
        for your confirmation (it can never auto-send, post, or spend).";

    #[test]
    fn standing_create_inverse_is_cancel_addressed_by_the_saved_id_from_the_outcome() {
        let input = json!({"goal": "Check the mail", "schedule": "daily"});
        match derive_inverse("standing_create", &input, STANDING_OK) {
            Inverse::Ready { tool, input: inv, note } => {
                assert_eq!(tool, "standing_cancel");
                // The id the mission was ACTUALLY saved under — never re-derived
                // from the goal (create truncates >280-byte goals before hashing,
                // so a re-derivation could name a mission that doesn't exist).
                assert_eq!(inv, json!({"id": "abc123def456"}));
                assert!(note.is_empty());
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        // No saved-id marker (refusal / drifted wording) -> honest None.
        assert!(matches!(
            derive_inverse("standing_create", &input, "I couldn't establish that standing mission: cap"),
            Inverse::None { .. }
        ));
    }

    #[test]
    fn extract_standing_id_requires_the_exact_terminated_marker() {
        assert_eq!(extract_standing_id(STANDING_OK).as_deref(), Some("abc123def456"));
        assert_eq!(extract_standing_id("It's saved (id abc123def456)."), Some("abc123def456".into()));
        assert_eq!(extract_standing_id("It's saved (id )"), None);
        assert_eq!(extract_standing_id("It's saved (id zzz)"), None); // non-hex
        assert_eq!(extract_standing_id("It's saved (id abc123def456"), None); // unterminated
        assert_eq!(extract_standing_id("no marker at all"), None);
    }

    #[test]
    fn a_refused_standing_create_is_never_journaled_as_executed() {
        let _g = store_guard();
        // The cap refusal comes back as friendly Ok prose (is_error=false), so
        // the chokepoint's `executed` is true — the outcome refinement must
        // still keep it out of the ledger.
        record_executed(
            "agent.a",
            "standing_create",
            &json!({"goal": "g", "schedule": "daily"}),
            "Would establish a standing mission",
            "I couldn't establish that standing mission: cap reached",
            &[],
            true,
            "confirm",
        );
        assert_eq!(snapshot_payload()["count"], 0);
        // The genuine success IS journaled, undoable via the saved id.
        record_executed(
            "agent.a",
            "standing_create",
            &json!({"goal": "Check the mail", "schedule": "daily"}),
            "Would establish a standing mission",
            STANDING_OK,
            &[],
            true,
            "confirm",
        );
        assert_eq!(snapshot_payload()["count"], 1);
        assert_eq!(snapshot_payload()["entries"][0]["undoable"], true);
    }

    #[test]
    fn inverse_confirmed_never_claims_undone_over_a_cancel_miss() {
        // Pinned against anthropic.rs::standing_cancel_tool's three outcomes.
        assert!(inverse_confirmed("standing_cancel", "Cancelled standing mission abc123."));
        assert!(!inverse_confirmed("standing_cancel", "I have no standing mission with id abc123 to cancel."));
        assert!(!inverse_confirmed("standing_cancel", "I couldn't cancel that standing mission: db error"));
        // Gated inverses are judged by the gate + chokepoint, not prose.
        assert!(inverse_confirmed("gads_enable_campaign", "anything"));
    }

    #[test]
    fn irreversibles_carry_specific_honest_reasons_and_unknowns_never_guess() {
        match derive_inverse("gmail_send", &json!({"to": "a@b.c"}), "") {
            Inverse::None { why } => assert!(why.contains("unsent")),
            other => panic!("{other:?}"),
        }
        match derive_inverse("slack_post_message", &json!({"channel": "#g", "text": "hi"}), "") {
            Inverse::None { why } => assert!(why.contains("id isn't captured")),
            other => panic!("{other:?}"),
        }
        // Unknown / MCP tools: never guess.
        match derive_inverse("mcp__srv__mystery_tool", &json!({}), "") {
            Inverse::None { why } => assert!(why.contains("don't know a safe inverse")),
            other => panic!("{other:?}"),
        }
    }

    // -- store: record / cap / undone marking --------------------------------

    #[test]
    fn record_only_keeps_genuinely_executed_actions_and_evicts_past_the_cap() {
        let _g = store_guard();
        // executed=false is never recorded.
        record_executed("agent.a", "gmail_send", &json!({}), "p", "Email sent.", &[], false, "confirm");
        assert_eq!(snapshot_payload()["count"], 0);
        // Overfill past the cap: oldest evicted, seq stays monotonic.
        for i in 0..(JOURNAL_CAP + 6) {
            record("agent.a", "gmail_send", json!({"i": i}), &format!("preview {i}"));
        }
        let g = lock();
        assert_eq!(g.entries.len(), JOURNAL_CAP);
        assert_eq!(g.entries.last().unwrap().seq, (JOURNAL_CAP + 6) as u64);
        assert_eq!(g.entries.first().unwrap().seq, 7);
    }

    #[test]
    fn prepare_undo_arms_the_inverse_and_a_matching_execution_marks_undone() {
        let _g = store_guard();
        record("agent.ads", "gads_pause_campaign", json!({"campaign_id": "42"}), "Would pause campaign 42");
        let prep = prepare_undo();
        let UndoPrep::Ready { seq, agent, tool, input, allowed, pending_id, note } = prep else {
            panic!("expected Ready");
        };
        assert_eq!(seq, 1);
        assert_eq!(agent, "agent.ads");
        assert_eq!(tool, "gads_enable_campaign");
        assert_eq!(input, json!({"campaign_id": "42"}));
        assert_eq!(allowed, vec!["a".to_string(), "b".to_string()]);
        assert!(note.contains("live spend"));
        assert_eq!(pending_id, crate::confirm::derive_pending_id("agent.ads", "gads_enable_campaign", &input));
        // The confirmed inverse executes -> the forward entry flips to undone,
        // and the inverse itself is journaled (a gated redo chain).
        record("agent.ads", "gads_enable_campaign", json!({"campaign_id": "42"}), "Would enable campaign 42");
        let g = lock();
        assert!(g.entries[0].undone, "forward entry marked undone");
        assert!(!g.entries[1].undone);
        assert!(g.expected.is_none(), "armed match consumed");
    }

    #[test]
    fn undo_of_the_last_action_never_reaches_past_an_irreversible_one() {
        let _g = store_guard();
        record("agent.ads", "gads_pause_campaign", json!({"campaign_id": "42"}), "Would pause 42");
        record("agent.pepper", "gmail_send", json!({"to": "x@y.z"}), "Would email x@y.z");
        // The LAST action is the irreversible mail — undo must refuse honestly,
        // not silently revert the older campaign pause.
        match prepare_undo() {
            UndoPrep::Irreversible { why } => assert!(why.contains("unsent")),
            other => panic!("expected Irreversible, got {other:?}"),
        }
        let g = lock();
        assert!(g.expected.is_none(), "nothing armed on refusal");
    }

    #[test]
    fn prepare_undo_reports_nothing_and_already_undone_honestly() {
        let _g = store_guard();
        assert!(matches!(prepare_undo(), UndoPrep::Nothing));
        record("agent.a", "gads_pause_campaign", json!({"campaign_id": "1"}), "Would pause 1");
        mark_undone(1);
        assert!(matches!(prepare_undo(), UndoPrep::AlreadyUndone));
    }

    // -- snapshot honesty ----------------------------------------------------

    #[test]
    fn snapshot_is_secret_free_redacted_and_bounded() {
        let _g = store_guard();
        record(
            "agent.pepper",
            "gmail_send",
            json!({"to": "alice@example.com", "body": "SECRET-BODY-XYZ"}),
            "Would send an email to alice@example.com",
        );
        for i in 0..20 {
            record("agent.a", "gads_pause_campaign", json!({"campaign_id": format!("{i}")}), &format!("Would pause {i}"));
        }
        let payload = snapshot_payload();
        let text = payload.to_string();
        assert!(!text.contains("SECRET-BODY-XYZ"), "raw input must never be emitted");
        // The preview is REDACTED at record time (same discipline as the audit
        // log): the raw dry-run preview names recipients precisely, so the
        // stored/broadcast form must mask them.
        assert!(!text.contains("alice@example.com"), "preview must be redacted: {text}");
        let gmail = lock().entries.first().cloned().unwrap();
        assert!(gmail.preview.contains("[redacted]"), "stored preview redacted: {}", gmail.preview);
        assert_eq!(payload["count"], 21);
        let entries = payload["entries"].as_array().unwrap();
        assert_eq!(entries.len(), SNAPSHOT_ENTRIES);
        // Newest first; benign words/short numbers pass through redaction.
        assert_eq!(entries[0]["preview"], "Would pause 19");
        assert_eq!(entries[0]["undoable"], true);
        assert_eq!(entries[0]["undone"], false);
        assert_eq!(entries[0]["via"], "confirm");
    }

    // -- spoken composition ---------------------------------------------------

    #[test]
    fn compose_undo_response_is_honest_per_outcome() {
        // Parked: the prompt speaks for itself; a caveat note leads.
        let parked = compose_undo_response("I'd enable campaign 42 — say 'confirm' to proceed or 'cancel' to drop it.", false, true, false, "re-enabling resumes live spend");
        assert!(parked.starts_with("Heads up: re-enabling resumes live spend."));
        assert!(parked.contains("say 'confirm'"));
        // Direct reversible execution announces the undo.
        assert_eq!(
            compose_undo_response("Standing mission cancelled.", false, false, true, ""),
            "Undone. Standing mission cancelled."
        );
        // Errors and master-off previews are relayed verbatim.
        assert_eq!(compose_undo_response("refused", true, false, false, ""), "refused");
        assert_eq!(compose_undo_response("[dry run] Would…", false, false, false, ""), "[dry run] Would…");
    }

    #[test]
    fn short_desc_cuts_at_sentence_boundaries_never_inside_dotted_tokens() {
        // ". " is the boundary — an email/domain/id is never cut at its dots.
        assert_eq!(
            short_desc("Would send an email to bob@example.com. Subject: hi.", "gmail_send"),
            "Would send an email to bob@example.com"
        );
        assert_eq!(short_desc("[dry run] Would pause campaign 42.", "gads_pause_campaign"), "Would pause campaign 42");
        assert_eq!(short_desc("", "x_post"), "a 'x_post' action");
    }

    #[test]
    fn status_line_reads_the_journal_honestly() {
        let _g = store_guard();
        assert!(status_line().contains("Nothing consequential"));
        record("agent.a", "gmail_send", json!({"to": "x@y.z"}), "Would send an email to x@y.z");
        let s = status_line();
        assert!(s.contains("can't undo"), "{s}");
        assert!(s.contains("unsent"), "{s}");
        record("agent.a", "gads_pause_campaign", json!({"campaign_id": "7"}), "Would pause campaign 7");
        let s = status_line();
        assert!(s.contains("undo that"), "{s}");
    }
}
