//! CAUSA — the CAUSAL DECISION-TRACE EXPLAINER ("why did you do that").
//!
//! A PURE assembler that folds the per-turn branch signals the daemon ALREADY
//! produces (intent classification, selector mode, the agent it routed to, the
//! local-vs-cloud route, the per-turn owner gate, the capability it used, and the
//! final outcome) into an ordered [`DecisionTrace`], held in a SMALL, BOUNDED,
//! redacted ring buffer (the last [`RING_CAP_MAX`]-clamped N turns). It answers
//! "why did you do that" / "why <Agent>" by narrating that trace in persona and
//! emitting a secret-free `causa.trace` telemetry frame.
//!
//! HONESTY CONTRACT (the lines this module must hold):
//!   * It reconstructs a trace ONLY from records the turn loop already computed
//!     and handed to [`record`] — it NEVER fabricates a rationale. A step is
//!     emitted only for a signal that was actually captured (e.g. the identity
//!     step appears ONLY when voice-id was enforcing that turn; the capability
//!     step ONLY when a tool/skill fired). We do not invent alternatives we did
//!     not record, so `alternatives` is usually empty — honestly so.
//!   * When a turn wasn't recorded (the ring is empty, or nothing matches the
//!     named agent) it returns an HONEST EMPTY ("I don't have a trace for that"),
//!     never a plausible-sounding guess.
//!   * SECRET-FREE: the utterance and the outcome are the only free-text fields,
//!     and both are run through [`crate::optimize::redact`] at assembly time (the
//!     SAME discipline the episodic/journal/audit stores apply at write) and
//!     bounded — so no email, phone, token, or long id ever rides a trace or its
//!     `causa.trace` frame. The remaining fields are decision TOKENS
//!     (intent/mode/agent/route/tool), safe by construction.
//!
//! Bounded + session-scoped: a process-global slot (the confirm-pending / journal
//! pattern) holds at most the configured ring size; a daemon restart starts an
//! empty ring, which the narration reports honestly.

use std::sync::Mutex;

use serde_json::{json, Value};

/// Hard floor/ceiling on the ring size the config may request — a decision trace
/// is a glance at the last few turns, never an archive.
pub const RING_CAP_MIN: usize = 1;
pub const RING_CAP_MAX: usize = 128;
/// The default ring size when the config leaves `[explain].ring_size` unset.
pub const RING_CAP_DEFAULT: usize = 16;
/// Per-string bound on the redacted free-text fields carried on the wire /
/// narrated (utterance + outcome). Sources are already redacted; bound anyway.
const STR_CAP: usize = 200;

/// Clamp a requested ring size into the sane band. A 0 (or absurd) config value
/// never disables recording silently nor lets the ring grow unbounded.
pub fn clamp_ring(requested: usize) -> usize {
    requested.clamp(RING_CAP_MIN, RING_CAP_MAX)
}

// ---------------------------------------------------------------------------
// The trace + its steps (pure data)
// ---------------------------------------------------------------------------

/// One decision point in a turn: WHAT stage of the pipeline, WHAT was chosen,
/// WHY (derived purely from the recorded signal), and any alternatives that were
/// actually recorded (usually none — the daemon does not persist the rejected
/// candidates, and CAUSA never invents them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub stage: &'static str,
    pub chosen: String,
    pub why: String,
    pub alternatives: Vec<String>,
}

/// The reconstructed causal trace of ONE turn. `turn_ref` is a stable
/// per-session handle (monotonic, assigned at record time); `utterance` and the
/// outcome inside the steps are REDACTED.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionTrace {
    /// Monotonic per-session turn reference (1-based). Stable handle for a trace.
    pub turn_ref: u64,
    /// RFC3339 record time.
    pub ts: String,
    /// The agent that handled the turn ("darwin") — the key "why <Agent>" filters on.
    pub agent: String,
    /// The user's utterance, REDACTED + bounded (never raw).
    pub utterance: String,
    /// The ordered decision steps, pipeline order.
    pub steps: Vec<Step>,
}

/// The raw per-turn branch signals captured at the end-of-turn chokepoint in
/// `main.rs::run_pipeline`. Every field is a decision the daemon ALREADY made
/// this turn — CAUSA computes none of them. The free-text fields (`utterance`,
/// `outcome`) are redacted inside [`assemble`], so a caller may pass raw text.
#[derive(Debug, Clone)]
pub struct TurnSignals {
    /// The raw utterance (redacted at assembly).
    pub utterance: String,
    /// The intent the classifier inferred (optimize/episodic's `class.intent`).
    pub intent: String,
    /// The classifier's confidence in [0,1]; 0 when not meaningfully recorded.
    pub confidence: f64,
    /// The selector mode (selector::classify_mode output, e.g. "act"/"ask").
    pub mode: String,
    /// The agent the turn routed to ("darwin").
    pub agent: String,
    /// The agent namespace ("agent.darwin").
    pub namespace: String,
    /// Where the turn was answered: "local" or "cloud" (router Outcome.routed_to).
    pub routed_to: String,
    /// Was voice-id ENFORCING this turn? Only then is the identity step a real
    /// branch signal worth narrating.
    pub gate_enforcing: bool,
    /// Did this turn's speaker verify as the owner? Meaningful only when enforcing.
    pub gate_verified: bool,
    /// The capability the turn invoked ("" when none — no tool/skill fired).
    pub tool_or_skill: String,
    /// The final spoken response (redacted at assembly).
    pub outcome: String,
    /// RFC3339 record time (injected so assembly stays testable).
    pub ts: String,
}

fn bound(s: &str) -> String {
    if s.chars().count() <= STR_CAP {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(STR_CAP).collect();
        out.push('…');
        out
    }
}

/// A confidence in [0,1] as a whole-percent string, or None when not recorded.
fn pct(confidence: f64) -> Option<u32> {
    if confidence.is_finite() && confidence > 0.0 {
        Some((confidence.clamp(0.0, 1.0) * 100.0).round() as u32)
    } else {
        None
    }
}

/// Fold one turn's signals into an ordered [`DecisionTrace`]. PURE (a function of
/// its inputs + the injected `turn_ref`) and HONEST: a step is added ONLY for a
/// signal the turn actually produced. The utterance and outcome are redacted here
/// (defense in depth — the SAME discipline the episodic/journal stores apply), so
/// no secret rides the trace even if a caller passed raw text.
pub fn assemble(signals: &TurnSignals, turn_ref: u64) -> DecisionTrace {
    let mut steps: Vec<Step> = Vec::with_capacity(6);

    // 1. INTENT — how the utterance was classified (with the honest confidence).
    let intent_why = match pct(signals.confidence) {
        Some(p) => format!("I classified the request as \"{}\" ({p}% confident)", signals.intent),
        None => format!("I classified the request as \"{}\"", signals.intent),
    };
    steps.push(Step {
        stage: "intent",
        chosen: signals.intent.clone(),
        why: intent_why,
        alternatives: Vec::new(),
    });

    // 2. SELECTOR — the pure mode classification of the phrasing.
    if !signals.mode.trim().is_empty() {
        steps.push(Step {
            stage: "selector",
            chosen: signals.mode.clone(),
            why: format!("the selector read the phrasing as \"{}\"", signals.mode),
            alternatives: Vec::new(),
        });
    }

    // 3. AGENT — which agent the intent+mode routed to.
    steps.push(Step {
        stage: "agent",
        chosen: signals.agent.clone(),
        why: format!(
            "a \"{}\" request routed to the {} agent ({})",
            signals.intent, signals.agent, signals.namespace
        ),
        alternatives: Vec::new(),
    });

    // 4. ROUTE — local (on-device) vs cloud model.
    let route_why = if signals.routed_to == "cloud" {
        "the request needed the cloud model, so I answered via the cloud".to_string()
    } else {
        "I answered on-device (the local model), no cloud call".to_string()
    };
    steps.push(Step {
        stage: "route",
        chosen: signals.routed_to.clone(),
        why: route_why,
        alternatives: Vec::new(),
    });

    // 5. IDENTITY — ONLY when voice-id was enforcing (otherwise it wasn't a real
    // decision this turn, and inventing one would over-claim).
    if signals.gate_enforcing {
        let (chosen, why) = if signals.gate_verified {
            ("owner-verified".to_string(), "your voice verified as the owner, so the turn was allowed".to_string())
        } else {
            ("unverified".to_string(), "the speaker didn't verify as the owner — consequential actions were withheld".to_string())
        };
        steps.push(Step { stage: "identity", chosen, why, alternatives: Vec::new() });
    }

    // 6. CAPABILITY — ONLY when a tool/skill actually fired.
    if !signals.tool_or_skill.trim().is_empty() {
        steps.push(Step {
            stage: "capability",
            chosen: signals.tool_or_skill.clone(),
            why: format!("I used the \"{}\" capability to carry it out", signals.tool_or_skill),
            alternatives: Vec::new(),
        });
    }

    // 7. OUTCOME — the redacted result line.
    let outcome = bound(&crate::optimize::redact(signals.outcome.trim()));
    if !outcome.is_empty() {
        steps.push(Step {
            stage: "outcome",
            chosen: "answered".to_string(),
            why: outcome,
            alternatives: Vec::new(),
        });
    }

    DecisionTrace {
        turn_ref,
        ts: signals.ts.clone(),
        agent: signals.agent.clone(),
        utterance: bound(&crate::optimize::redact(signals.utterance.trim())),
        steps,
    }
}

// ---------------------------------------------------------------------------
// The bounded ring (process-global, session-scoped)
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RingState {
    next_ref: u64,
    traces: Vec<DecisionTrace>,
}

/// Process-global trace ring — the same const-`Mutex` pattern as the journal /
/// confirm-pending slots.
static RING: Mutex<RingState> = Mutex::new(RingState { next_ref: 1, traces: Vec::new() });

/// Lock the ring, recovering from poisoning rather than panicking — a
/// bookkeeping surface must never wedge the daemon.
fn lock() -> std::sync::MutexGuard<'static, RingState> {
    RING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record one turn's signals: assemble the redacted trace and push it to the
/// ring, evicting the oldest beyond the (clamped) `ring_size`. Called ONCE per
/// turn from the turn loop. No-op-safe: a clamp guarantees at least one slot.
pub fn record(signals: &TurnSignals, ring_size: usize) {
    // THRESHOLD write-integrity chokepoint: a GUEST turn leaves NO durable trace.
    // The decision-trace ring is owner-influencing state the OWNER reads back via
    // "why did you do that" — a bystander's turn must never enter it (nor bump the
    // turn_ref counter). Refuse HERE, at the ring primitive. Background tasks read
    // false and are unaffected.
    if crate::threshold::guest_write_blocked() {
        return;
    }
    let cap = clamp_ring(ring_size);
    let mut g = lock();
    let turn_ref = g.next_ref;
    g.next_ref += 1;
    let trace = assemble(signals, turn_ref);
    g.traces.push(trace);
    if g.traces.len() > cap {
        let excess = g.traces.len() - cap;
        g.traces.drain(..excess);
    }
}

/// The most recent recorded trace, or None (the honest empty).
pub fn explain_last() -> Option<DecisionTrace> {
    lock().traces.last().cloned()
}

/// The most recent trace whose handling agent matches `agent` (case-insensitive,
/// tolerant of an "agent." namespace prefix or the bare name), or None.
pub fn explain_for_agent(agent: &str) -> Option<DecisionTrace> {
    let needle = agent.trim().trim_start_matches("agent.").to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    lock()
        .traces
        .iter()
        .rev()
        .find(|t| t.agent.to_ascii_lowercase() == needle)
        .cloned()
}

/// Resolve a classified query to a trace (or the honest None).
pub fn lookup(query: &ExplainQuery) -> Option<DecisionTrace> {
    match query {
        ExplainQuery::Last => explain_last(),
        ExplainQuery::Agent(name) => explain_for_agent(name),
    }
}

/// Test-only reset of the process-global ring.
#[cfg(test)]
pub(crate) fn clear_for_test() {
    let mut g = lock();
    g.next_ref = 1;
    g.traces.clear();
}

// ---------------------------------------------------------------------------
// The "why did you do that" / "why <Agent>" intent
// ---------------------------------------------------------------------------

/// A classified explain request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExplainQuery {
    /// "why did you do that" — explain the LAST turn.
    Last,
    /// "why did <name>" / "why <name> agent" — explain the last turn the NAMED
    /// agent handled.
    Agent(String),
}

/// Classify an utterance as an explain request, or None. PURE and CONSERVATIVE,
/// exactly like `rewind::classify_rewind_intent` and `journal::classify_undo_command`:
/// only clear, anchored phrasings fire, so an ordinary "why is the sky blue"
/// question is never hijacked into the explainer. Agent-directed forms are tried
/// FIRST so "why did darwin do that" names the agent rather than reading as the
/// generic Last.
pub fn classify_explain_intent(text: &str) -> Option<ExplainQuery> {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches(['.', '!', '?']).trim().trim_end_matches(',').trim();

    if let Some(name) = agent_query(t) {
        return Some(ExplainQuery::Agent(name));
    }

    const LAST_PHRASES: &[&str] = &[
        "why did you do that",
        "why did you do this",
        "why did you do it",
        "why'd you do that",
        "why did you do",
        "why did you",
        "why did you decide that",
        "why did you choose that",
        "why did you say that",
        "why did you respond that way",
        "why that decision",
        "explain your last decision",
        "explain that decision",
        "explain your decision",
        "explain your reasoning",
        "explain your last answer",
        "explain yourself",
        "what made you do that",
        "what was your reasoning",
        "how did you decide that",
    ];
    if LAST_PHRASES.contains(&t) {
        return Some(ExplainQuery::Last);
    }
    None
}

/// Extract a NAMED-agent explain target from a trimmed, lowercased utterance, or
/// None. Deliberately narrow so an ordinary "why did the server crash" or "why is
/// X" question never resolves to an agent:
///   * "why <name> agent" / "why the <name> agent" / "why did the <name> agent
///     …" — an explicit "agent" keyword names the target UNAMBIGUOUSLY.
///   * "why did <name> <decision tail>" — no "agent" keyword, so a decision/
///     action reference in the tail ("do that", "decide", …) is REQUIRED; without
///     it the phrase is an ordinary question, not an explain request.
///
/// `<name>` is always a single alphabetic token that is NOT a pronoun/filler.
fn agent_query(t: &str) -> Option<String> {
    let words: Vec<&str> = t.split_whitespace().collect();
    if words.first() != Some(&"why") {
        return None;
    }
    // 1. An explicit "agent" keyword: the token immediately before it is the name.
    if let Some(pos) = words.iter().position(|w| *w == "agent") {
        if pos >= 2 {
            let cand = words[pos - 1];
            if is_name_token(cand) {
                return Some(cand.to_string());
            }
        }
    }
    // 2. "why did <name> <decision tail>" (skip an optional "the"). The tail MUST
    //    reference a decision/action — otherwise "why did the build fail" would
    //    wrongly read as a request to explain a "build" agent.
    if words.len() >= 4 && words[1] == "did" {
        let idx = if words[2] == "the" { 3 } else { 2 };
        if let Some(cand) = words.get(idx) {
            if is_name_token(cand) && has_decision_tail(&words[idx + 1..].join(" ")) {
                return Some((*cand).to_string());
            }
        }
    }
    None
}

/// Does the tail of a "why did <name> …" phrase reference a decision/action (so
/// it reads as "why did <agent> ACT that way", not an ordinary "why did <noun>
/// <happen>" question)?
fn has_decision_tail(tail: &str) -> bool {
    const CUES: &[&str] = &[
        "do that", "do this", "do it", "decide", "chose", "choose", "handle",
        "pick", "answer", "respond", "say that", "said that", "act on", "run that",
    ];
    CUES.iter().any(|c| tail.contains(c))
}

/// A plausible agent-name token: 2..=20 ASCII letters, and not a pronoun/filler
/// that would read as the generic "you"/"it" case.
fn is_name_token(w: &str) -> bool {
    const STOP: &[&str] = &[
        "you", "it", "that", "this", "we", "they", "he", "she", "the", "there",
        "not", "so", "though", "now", "then", "me", "us", "do", "did", "does",
    ];
    let len = w.chars().count();
    (2..=20).contains(&len) && w.chars().all(|c| c.is_ascii_alphabetic()) && !STOP.contains(&w)
}

// ---------------------------------------------------------------------------
// Narration (persona) + the wire frame
// ---------------------------------------------------------------------------

/// The spoken narration for an explain query. First-person persona register
/// ("sir"), HONEST-EMPTY when there is no trace — never a fabricated rationale.
pub fn render_spoken(query: &ExplainQuery, trace: Option<&DecisionTrace>) -> String {
    let Some(t) = trace else {
        return match query {
            ExplainQuery::Last => "I don't have a decision trace for that, sir — I keep only the last few turns, and there's nothing recorded to explain.".to_string(),
            ExplainQuery::Agent(name) => format!(
                "I don't have a recent trace of the {name} agent acting, sir — nothing in the last few turns matches.",
            ),
        };
    };

    // Fold the recorded steps into one honest sentence, in pipeline order. We
    // narrate ONLY the steps that exist (identity/capability appear only when
    // they were real branch signals).
    let mut clauses: Vec<String> = Vec::new();
    for step in &t.steps {
        match step.stage {
            "intent" => clauses.push(step.why.clone()),
            "agent" => clauses.push(format!("routed it to the {} agent", step.chosen)),
            "route" => clauses.push(if step.chosen == "cloud" {
                "answered via the cloud model".to_string()
            } else {
                "answered on-device".to_string()
            }),
            "identity" => clauses.push(step.why.clone()),
            "capability" => clauses.push(format!("used the \"{}\" capability", step.chosen)),
            // selector + outcome are shown on the HUD; keep the spoken line tight.
            _ => {}
        }
    }
    let body = if clauses.is_empty() {
        "I have the turn recorded but no decision steps to narrate".to_string()
    } else {
        clauses.join("; ")
    };
    format!("Here's why, sir: {body}. The full decision trace is on the HUD.")
}

/// The SECRET-FREE `causa.trace` wire payload. The utterance + outcome are
/// already redacted in the trace; the step tokens are safe. WIRE CONTRACT
/// (mirrored by hud/src/core/events.ts::parseCausaTrace; pinned by tests both
/// sides):
///
///   { "query", "agent_query", "empty", "turn_ref", "ts", "agent", "utterance",
///     "steps": [ { "stage", "chosen", "why", "alternatives": [..] } ] }
pub fn payload(query: &ExplainQuery, trace: Option<&DecisionTrace>) -> Value {
    let (query_kind, agent_query) = match query {
        ExplainQuery::Last => ("last", String::new()),
        ExplainQuery::Agent(name) => ("agent", name.clone()),
    };
    match trace {
        None => json!({
            "query": query_kind,
            "agent_query": agent_query,
            "empty": true,
            "turn_ref": 0,
            "ts": "",
            "agent": "",
            "utterance": "",
            "steps": [],
        }),
        Some(t) => json!({
            "query": query_kind,
            "agent_query": agent_query,
            "empty": false,
            "turn_ref": t.turn_ref,
            "ts": t.ts,
            "agent": t.agent,
            "utterance": t.utterance,
            "steps": t.steps.iter().map(|s| json!({
                "stage": s.stage,
                "chosen": s.chosen,
                "why": s.why,
                "alternatives": s.alternatives,
            })).collect::<Vec<_>>(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests — pure (classifier, assembly, narration, redaction) + the store (ring).
// The ring is process-global; store tests serialize on the crate-wide lock the
// journal uses (confirm::PENDING_TEST_LOCK) so they never race pure fns.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn signals() -> TurnSignals {
        TurnSignals {
            utterance: "pause the ads campaign".to_string(),
            intent: "action".to_string(),
            confidence: 0.72,
            mode: "act".to_string(),
            agent: "darwin".to_string(),
            namespace: "agent.darwin".to_string(),
            routed_to: "local".to_string(),
            gate_enforcing: false,
            gate_verified: false,
            tool_or_skill: String::new(),
            outcome: "Paused the campaign, sir.".to_string(),
            ts: "2026-07-15T12:00:00Z".to_string(),
        }
    }

    // -- classifier ----------------------------------------------------------

    #[test]
    fn classify_last_is_anchored_and_conservative() {
        assert_eq!(classify_explain_intent("why did you do that"), Some(ExplainQuery::Last));
        assert_eq!(classify_explain_intent("Why did you do that?"), Some(ExplainQuery::Last));
        assert_eq!(classify_explain_intent("explain your last decision"), Some(ExplainQuery::Last));
        assert_eq!(classify_explain_intent("what made you do that"), Some(ExplainQuery::Last));
        assert_eq!(classify_explain_intent("why that decision."), Some(ExplainQuery::Last));
        // A near-miss / ordinary question is NEVER hijacked.
        assert_eq!(classify_explain_intent("why is the sky blue"), None);
        assert_eq!(classify_explain_intent(""), None);
        assert_eq!(classify_explain_intent("do that again"), None);
    }

    #[test]
    fn classify_agent_forms_name_the_agent_but_you_stays_last() {
        assert_eq!(classify_explain_intent("why did darwin do that"), Some(ExplainQuery::Agent("darwin".into())));
        assert_eq!(classify_explain_intent("why the pepper agent"), Some(ExplainQuery::Agent("pepper".into())));
        assert_eq!(classify_explain_intent("why darwin agent"), Some(ExplainQuery::Agent("darwin".into())));
        assert_eq!(classify_explain_intent("why did the friday agent do that"), Some(ExplainQuery::Agent("friday".into())));
        // "you" is never an agent — it stays the generic Last.
        assert_eq!(classify_explain_intent("why did you do that"), Some(ExplainQuery::Last));
        // A pronoun/filler after "did" never resolves to an agent.
        assert_eq!(classify_explain_intent("why did it rain"), None);
        assert_eq!(classify_explain_intent("why did they leave"), None);
        // "why did <noun> <happen>" is an ordinary question, NOT an agent explain —
        // the decision-tail requirement keeps it out.
        assert_eq!(classify_explain_intent("why did the server crash"), None);
        assert_eq!(classify_explain_intent("why did the build fail"), None);
        assert_eq!(classify_explain_intent("why did the meeting run late"), None);
        // A bare "why did darwin" (no decision tail, no "agent" keyword) is too
        // ambiguous to fire.
        assert_eq!(classify_explain_intent("why did darwin"), None);
    }

    #[test]
    fn never_collides_with_rewind_or_undo_phrasings() {
        // rewind gate phrases + undo phrases must not resolve to an explain query.
        for p in ["what happened in the last hour", "rewind the last hour", "walk me through this morning", "replay the macro standup"] {
            assert_eq!(classify_explain_intent(p), None, "{p}");
        }
        for p in ["undo that", "revert that", "what can you undo"] {
            assert_eq!(classify_explain_intent(p), None, "{p}");
        }
    }

    // -- assembly (pure fold) ------------------------------------------------

    #[test]
    fn assemble_builds_ordered_steps_from_signals() {
        let t = assemble(&signals(), 1);
        assert_eq!(t.turn_ref, 1);
        assert_eq!(t.agent, "darwin");
        let stages: Vec<&str> = t.steps.iter().map(|s| s.stage).collect();
        // No identity (gate off), no capability (no tool) — honestly omitted.
        assert_eq!(stages, ["intent", "selector", "agent", "route", "outcome"]);
        assert!(t.steps[0].why.contains("72% confident"), "{:?}", t.steps[0].why);
        assert_eq!(t.steps[2].chosen, "darwin");
        assert_eq!(t.steps[3].chosen, "local");
    }

    #[test]
    fn identity_step_only_when_enforcing_capability_only_when_a_tool_fired() {
        let mut s = signals();
        s.gate_enforcing = true;
        s.gate_verified = false;
        s.tool_or_skill = "gads_pause_campaign".to_string();
        s.routed_to = "cloud".to_string();
        let t = assemble(&s, 2);
        let stages: Vec<&str> = t.steps.iter().map(|s| s.stage).collect();
        assert_eq!(stages, ["intent", "selector", "agent", "route", "identity", "capability", "outcome"]);
        let identity = t.steps.iter().find(|s| s.stage == "identity").unwrap();
        assert_eq!(identity.chosen, "unverified");
        let cap = t.steps.iter().find(|s| s.stage == "capability").unwrap();
        assert_eq!(cap.chosen, "gads_pause_campaign");
        let route = t.steps.iter().find(|s| s.stage == "route").unwrap();
        assert!(route.why.contains("cloud"));
    }

    #[test]
    fn zero_confidence_and_blank_mode_are_omitted_honestly() {
        let mut s = signals();
        s.confidence = 0.0;
        s.mode = "  ".to_string();
        let t = assemble(&s, 1);
        assert!(!t.steps[0].why.contains('%'), "no fabricated confidence: {:?}", t.steps[0].why);
        assert!(t.steps.iter().all(|s| s.stage != "selector"), "blank mode omitted");
    }

    // -- redaction (no secret leaks into a trace) ----------------------------

    #[test]
    fn utterance_and_outcome_are_redacted_no_secret_rides_the_trace() {
        let mut s = signals();
        s.utterance = "email alice@example.com the key sk-LIVEsecret1234567890".to_string();
        s.outcome = "Sent to bob@corp.com with token ghp_ABCDEFGHIJKLMNOP1234567890.".to_string();
        let t = assemble(&s, 1);
        let blob = payload(&ExplainQuery::Last, Some(&t)).to_string();
        assert!(!blob.contains("alice@example.com"), "utterance email leaked: {blob}");
        assert!(!blob.contains("sk-LIVEsecret1234567890"), "utterance secret leaked: {blob}");
        assert!(!blob.contains("bob@corp.com"), "outcome email leaked: {blob}");
        assert!(!blob.contains("ghp_ABCDEFGHIJKLMNOP1234567890"), "outcome token leaked: {blob}");
        assert!(blob.contains("[redacted]"), "redaction ran: {blob}");
    }

    #[test]
    fn long_fields_are_bounded() {
        let mut s = signals();
        s.utterance = "x".repeat(500);
        let t = assemble(&s, 1);
        assert!(t.utterance.chars().count() <= STR_CAP + 1);
        assert!(t.utterance.ends_with('…'));
    }

    // -- narration -----------------------------------------------------------

    #[test]
    fn render_is_honest_empty_and_persona_for_a_trace() {
        assert!(render_spoken(&ExplainQuery::Last, None).contains("don't have a decision trace"));
        assert!(render_spoken(&ExplainQuery::Agent("darwin".into()), None).contains("darwin agent"));
        let t = assemble(&signals(), 1);
        let spoken = render_spoken(&ExplainQuery::Last, Some(&t));
        assert!(spoken.starts_with("Here's why, sir:"), "{spoken}");
        assert!(spoken.contains("routed it to the darwin agent"), "{spoken}");
        assert!(spoken.contains("on-device"), "{spoken}");
        assert!(spoken.contains("on the HUD"), "{spoken}");
    }

    // -- wire shape ----------------------------------------------------------

    #[test]
    fn payload_pins_the_wire_shape_and_the_empty_frame() {
        let t = assemble(&signals(), 3);
        let p = payload(&ExplainQuery::Last, Some(&t));
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            ["agent", "agent_query", "empty", "query", "steps", "ts", "turn_ref", "utterance"]
        );
        assert_eq!(p["empty"], false);
        assert_eq!(p["turn_ref"], 3);
        assert_eq!(p["query"], "last");
        let step_keys: Vec<&String> = p["steps"][0].as_object().unwrap().keys().collect();
        assert_eq!(step_keys, ["alternatives", "chosen", "stage", "why"]);
        // The honest-empty frame still carries the query so the HUD can show it.
        let empty = payload(&ExplainQuery::Agent("nobody".into()), None);
        assert_eq!(empty["empty"], true);
        assert_eq!(empty["query"], "agent");
        assert_eq!(empty["agent_query"], "nobody");
        assert_eq!(empty["steps"].as_array().unwrap().len(), 0);
    }

    // -- the ring: record / eviction / honest empty / lookup -----------------

    fn store_guard() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::confirm::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        clear_for_test();
        g
    }

    /// THRESHOLD write-integrity chokepoint (explain::record). The decision-trace
    /// ring is owner-influencing state the OWNER reads back via "why did you do
    /// that" — a GUEST turn must push NOTHING and must not even bump the turn_ref
    /// counter. The owner path records as usual.
    #[test]
    fn guest_turn_records_no_decision_trace() {
        let _g = store_guard(); // holds PENDING_TEST_LOCK + clears the ring
        let before_ref = lock().next_ref;
        {
            let guest = crate::threshold::guest_from(
                &crate::threshold::Scope::owner(vec!["*".to_string()], crate::focus::FocusProfile::Default),
                &crate::focus::FocusProfile::DeepFocus,
            );
            let _o = crate::threshold::ScopeOverride::guest(guest);
            assert!(crate::threshold::is_guest_turn());
            record(&signals(), 8);
        }
        assert!(explain_last().is_none(), "a guest turn seeded a decision trace");
        assert_eq!(lock().next_ref, before_ref, "a guest turn bumped the ring's turn counter");
        // OWNER path records normally.
        record(&signals(), 8);
        assert!(explain_last().is_some(), "the owner turn's trace was not recorded");
    }

    #[test]
    fn record_pushes_and_the_ring_evicts_oldest_past_the_cap() {
        let _g = store_guard();
        assert!(explain_last().is_none(), "honest empty before any turn");
        for i in 0..10u64 {
            let mut s = signals();
            s.utterance = format!("turn {i}");
            record(&s, 3); // ring size 3
        }
        let last = explain_last().unwrap();
        assert_eq!(last.turn_ref, 10, "turn_ref is monotonic across eviction");
        assert!(last.utterance.contains("turn 9"));
        // Only the last 3 survive; refs stay monotonic (oldest evicted).
        let g = lock();
        assert_eq!(g.traces.len(), 3);
        assert_eq!(g.traces.first().unwrap().turn_ref, 8);
        assert_eq!(g.traces.last().unwrap().turn_ref, 10);
    }

    #[test]
    fn a_zero_or_absurd_ring_size_is_clamped_never_disables_recording() {
        let _g = store_guard();
        record(&signals(), 0); // clamps to RING_CAP_MIN (1), still records
        assert!(explain_last().is_some(), "a 0 ring size must not silently drop the trace");
        assert_eq!(lock().traces.len(), 1);
        record(&signals(), usize::MAX); // clamps to RING_CAP_MAX
        assert_eq!(lock().traces.len(), 2);
    }

    #[test]
    fn explain_for_agent_matches_the_newest_and_is_honest_empty_on_a_miss() {
        let _g = store_guard();
        let mut a = signals();
        a.agent = "darwin".to_string();
        record(&a, 8);
        let mut b = signals();
        b.agent = "pepper".to_string();
        b.utterance = "book a table".to_string();
        record(&b, 8);
        // Newest darwin turn (the first one) is found by name / namespace / case.
        assert_eq!(explain_for_agent("darwin").unwrap().agent, "darwin");
        assert_eq!(explain_for_agent("agent.darwin").unwrap().agent, "darwin");
        assert_eq!(explain_for_agent("DARWIN").unwrap().agent, "darwin");
        assert_eq!(explain_for_agent("pepper").unwrap().utterance, "book a table");
        // No such agent -> honest empty, never a wrong trace.
        assert!(explain_for_agent("nobody").is_none());
        assert!(explain_for_agent("").is_none());
        // lookup dispatches the query the same way.
        assert!(lookup(&ExplainQuery::Agent("nobody".into())).is_none());
        assert_eq!(lookup(&ExplainQuery::Last).unwrap().agent, "pepper");
    }
}
