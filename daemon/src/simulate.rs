//! PRECOG — a COUNTERFACTUAL command simulator ("what would you do if I said X").
//!
//! PRECOG answers ONE question without ever acting: *if the user actually said
//! `X`, what would DARWIN do?* It runs the SAME pipeline the live turn would —
//! classify -> [`crate::selector::classify_mode`] -> [`crate::agents::AgentRegistry`]
//! delegation -> [`crate::model_tier::resolve_tier`] -> [`crate::confirm::is_consequential_tool`]
//! -> [`crate::journal::derive_inverse`] reversibility — and returns a
//! [`PlannedOutcome`] describing the plan. It runs UP TO but NEVER THROUGH the
//! confirmation gate: PRECOG can report that a real run WOULD park, but it never
//! satisfies a gate itself and never fires an action, not even a benign one.
//!
//! ## The sacred invariant: PRECOG is READ-ONLY by CONSTRUCTION, not by promise
//!
//! This mirrors [`crate::focus`]'s type-level guarantee. The whole simulate path
//! operates over [`SimContext`], whose fields are ONLY read views:
//!
//!   * `&AgentRegistry` — a shared reference; only `&self` read methods are
//!     reachable (delegation lookup). It can name an agent, never invoke a tool.
//!   * `&Config`        — read-only configuration data.
//!   * `&S: AgentScorer`— a PURE scorer (`score(&self, ..) -> Vec<f64>`); no I/O,
//!     no network, no clock.
//!   * `cloud_reachable: bool` and `override_tier: Option<Tier>` — plain Copy
//!     values, not handles.
//!
//! There is NO field for an actuator, NO `&mut Memory` (memory-write), NO
//! `InferenceClient`/Brain handle, NO confirm slot, NO integrations gate. The type
//! *literally cannot express* "run the model" or "fire a tool" — so [`simulate`],
//! which takes only `&SimContext`, provably cannot. The classification the plan is
//! built on is passed IN as a [`PredictedIntent`] read view (the router does the
//! one read-only `classify` call and hands the label here); `simulate` never
//! reaches for the inference server itself. The
//! `sim_context_has_no_actuator_or_write_handle` test encodes this: it exhaustively
//! destructures a [`SimContext`] into exactly its read-only fields, so a future edit
//! that added a write/actuator handle would FAIL TO COMPILE, forcing a re-review.
//!
//! ## What PRECOG projects (honestly)
//!
//! The one thing a pure simulation cannot know is which exact tool the CLOUD model
//! would choose (that is a model decision made mid-turn). PRECOG does NOT pretend
//! to. It projects the tool CLASS from the deterministic, side-effecting vocabulary
//! the system already gates on ([`crate::confirm::CONSEQUENTIAL_TOOLS`]): a clear
//! "send an email" projects `gmail_send`, "turn on the lights" projects
//! `dume_control`, a recurring cadence (the selector's `Standing` mode) projects
//! `standing_create`. When the utterance names no side-effecting action the planned
//! tool is `None` — an honest "no consequential action; nothing to confirm." The
//! reversibility verdict reuses [`crate::journal::derive_inverse`] over that
//! projected tool CLASS, so PRECOG and a real undo agree on what can be undone.

use serde_json::{json, Value};

use crate::agents::AgentScorer;
use crate::config::Config;
use crate::confirm::is_consequential_tool;
use crate::journal::{derive_inverse, Inverse};
use crate::model_tier::{resolve_tier, Tier};
use crate::selector::{classify_mode, Mode};

// ---------------------------------------------------------------------------
// PredictedIntent — the classifier's verdict as a READ VIEW
// ---------------------------------------------------------------------------

/// The intent classification for the HYPOTHETICAL utterance, passed into
/// [`simulate`] as a read-only view. The real classifier ([`crate::inference::InferenceClient::classify`])
/// is an async read — the router calls it ONCE on the hypothetical and hands the
/// resulting label here, so [`simulate`] stays a pure function (no inference
/// handle, unit-testable with a synthetic classification). A `classify` call is
/// itself read-only: it labels text, it fires nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct PredictedIntent {
    /// The classifier intent id (e.g. "app.launch", "conversation").
    pub intent: String,
    /// The classifier confidence in [0,1].
    pub confidence: f64,
    /// The classifier complexity ("heavy" => a complex turn; else light/trivial).
    pub complexity: String,
}

impl PredictedIntent {
    /// The safe default when the hypothetical could not be classified (the
    /// inference backend was unreachable). Treated as a low-confidence plain
    /// conversation — exactly the honest degrade the live pipeline takes.
    pub fn unknown() -> Self {
        PredictedIntent { intent: "conversation".to_string(), confidence: 0.0, complexity: "light".to_string() }
    }
}

// ---------------------------------------------------------------------------
// SimContext — the READ-ONLY context. Structurally "cannot act" (see module docs)
// ---------------------------------------------------------------------------

/// The read-only context [`simulate`] runs over. EVERY field is a read view (see
/// the module-level invariant): a shared `&AgentRegistry` (delegation lookup only),
/// read-only `&Config`, a pure `&S: AgentScorer`, and two Copy values. There is no
/// actuator, no memory-write handle, no inference/Brain handle, no confirm slot —
/// the type cannot express an action, so `simulate` cannot perform one.
pub struct SimContext<'a, S: AgentScorer> {
    /// The live agent roster — used ONLY for delegation lookup (`&self` reads).
    pub agents: &'a crate::agents::AgentRegistry,
    /// Read-only configuration (the router route default + cloud thresholds/models).
    pub cfg: &'a Config,
    /// A PURE semantic scorer (the same [`crate::agents::LexicalAgentScorer`] the
    /// live routing uses). No I/O, no network, no clock.
    pub scorer: &'a S,
    /// The manual model-tier override in force (usually [`crate::model_tier::current_override`]).
    /// A Copy value, not a handle.
    pub override_tier: Option<Tier>,
    /// Whether a cloud call could be made this turn (key + reachability). A plain
    /// bool — PRECOG never makes the call; it only reports which tier a real run
    /// would resolve to.
    pub cloud_reachable: bool,
}

// ---------------------------------------------------------------------------
// PlannedOutcome — the projected plan (never executed)
// ---------------------------------------------------------------------------

/// The projected plan for a hypothetical utterance: what a real turn WOULD do,
/// derived by running the live pipeline up to (never through) the gate. Carries no
/// capability — it is a description, produced by a function with no way to act.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedOutcome {
    /// The classifier intent (from [`PredictedIntent`]).
    pub intent: String,
    /// The agent Darwin-Prime would delegate to (the live `select_with_fallback`).
    pub agent: String,
    /// The capability MODE the selector would route to ("one_shot" / "world_query"
    /// / "world_update" / "mission" / "standing"), or "clarify" when a real run
    /// would ASK a one-line clarifying question instead of acting.
    pub mode: String,
    /// The model TIER a real run would resolve to ("local" / "fast" / "heavy").
    pub tier: String,
    /// The CONSEQUENTIAL tool CLASS a real run would engage, if the utterance names
    /// a side-effecting action (a real tool name from [`crate::confirm::CONSEQUENTIAL_TOOLS`]).
    /// `None` when the utterance names no gated action (pure conversation, a benign
    /// local action, or a clarify) — an honest "nothing to confirm".
    pub tool: Option<String>,
    /// Whether a real run would PARK at the confirmation gate (true iff `tool` is a
    /// consequential tool). PRECOG only ever REPORTS this — it never satisfies a
    /// gate itself.
    pub would_park: bool,
    /// Whether the planned action has a safe mechanical inverse (via
    /// [`crate::journal::derive_inverse`] over the projected tool class). Meaningful
    /// only when `would_park`; `true` (nothing to reverse) when no gated action.
    pub reversible: bool,
    /// The classifier confidence for the hypothetical.
    pub confidence: f64,
    /// A grounded one-line explanation of the plan (the spoken/HUD rationale).
    pub why: String,
}

impl PlannedOutcome {
    /// The `precog.plan` telemetry frame for the HUD. SECRET-FREE by construction:
    /// only the pipeline decisions + the (already user-spoken) hypothetical ride
    /// the wire — no fact value, no memory, no tool output (nothing ran). The
    /// `executed` / `satisfied_a_gate` booleans are PINNED false: a simulation
    /// never executes and never satisfies a gate, and the HUD states that from the
    /// payload rather than a hardcode.
    pub fn telemetry(&self, utterance: &str) -> Value {
        json!({
            "utterance": utterance,
            "intent": self.intent,
            "agent": self.agent,
            "mode": self.mode,
            "tier": self.tier,
            "tool": self.tool,
            "would_park": self.would_park,
            "reversible": self.reversible,
            "confidence": round4(self.confidence),
            "why": self.why,
            // The PRECOG contract, on the wire so the HUD copy is grounded:
            // a simulation NEVER runs and NEVER satisfies a gate.
            "executed": false,
            "satisfied_a_gate": false,
        })
    }

    /// The spoken summary DARWIN says back for a "what would you do if I said X"
    /// query. Honest and specific: it names the plan and, crucially, states that a
    /// real run would PARK for a spoken yes (PRECOG never satisfies that gate).
    pub fn spoken_summary(&self, utterance: &str) -> String {
        if self.mode == "clarify" {
            return format!(
                "If you said \"{utterance}\", I wouldn't act, sir — I'd ask you one clarifying \
                 question first (whether you meant just once or every day). Nothing would run."
            );
        }
        if self.would_park {
            let rev = if self.reversible {
                "and I'd have a safe way to undo it afterward"
            } else {
                "and there'd be no mechanical undo, so I'd be extra clear before firing"
            };
            let tool = self.tool.as_deref().unwrap_or("that action");
            return format!(
                "If you said \"{utterance}\", I'd route it to {} as a {} turn and prepare '{tool}' — \
                 but I would NOT run it, sir. It would PARK for your spoken yes ({rev}). \
                 Simulating never satisfies that gate.",
                self.agent, self.mode
            );
        }
        format!(
            "If you said \"{utterance}\", I'd handle it as a {} turn on the {} tier via {} — \
             no consequential action, nothing to confirm, sir.",
            self.mode, self.tier, self.agent
        )
    }
}

/// Round a confidence to 4dp for the wire (house telemetry style; avoids a long
/// float tail in the HUD readout).
fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

// ---------------------------------------------------------------------------
// simulate — the pure pipeline projection (UP TO but NEVER THROUGH the gate)
// ---------------------------------------------------------------------------

/// Run the live pipeline over a HYPOTHETICAL utterance and return the
/// [`PlannedOutcome`] — WITHOUT ever executing. PURE and DETERMINISTIC given the
/// read-only [`SimContext`] + the [`PredictedIntent`] view: it reuses the exact
/// pure functions the live turn does (selector, delegation, tier resolution, the
/// consequential predicate, the inverse deriver), so the projection agrees with
/// what a real run would decide up to the gate. It runs UP TO the gate and STOPS:
/// it reports `would_park` but never satisfies it, and — because [`SimContext`]
/// holds no actuator/write/Brain handle — it structurally cannot fire anything.
pub fn simulate<S: AgentScorer>(
    utterance: &str,
    predicted: &PredictedIntent,
    ctx: &SimContext<'_, S>,
) -> PlannedOutcome {
    // 1. MODE — the capability selector (pure, injected scorer). A `Clarify` means
    //    a real run would ASK a one-line question, not act.
    let selection = classify_mode(utterance, ctx.scorer);
    let mode = selection.mode();

    // 2. AGENT — Darwin-Prime delegation, exactly the live `select_with_fallback`
    //    (deterministic intent map + keyword cues + the same pure semantic
    //    fallback). A `&self` read of the roster; it names an agent, invokes nothing.
    let agent = ctx
        .agents
        .select_with_fallback(&predicted.intent, utterance, ctx.cloud_reachable, ctx.scorer)
        .name
        .clone();

    // 3. TIER — the precedence Override > Auto > Fallback, from the SAME resolver.
    let (tier, _reason) = resolve_tier(
        ctx.cfg,
        ctx.override_tier,
        &predicted.complexity,
        predicted.confidence,
        ctx.cfg.router.cloud_confidence_threshold,
        ctx.cloud_reachable,
    );

    // 4. PLANNED TOOL CLASS — the deterministic side-effecting-vocabulary
    //    projection. `None` for a benign/conversational turn.
    let planned = plan_tool(utterance, mode);

    // 5. GATE PROJECTION + reversibility — UP TO the gate, never through it.
    //    `would_park` = the tool is consequential (parks for a spoken yes).
    //    `reversible` reuses derive_inverse over the projected tool class.
    let (tool, would_park, reversible) = match planned {
        Some(pt) => {
            let park = is_consequential_tool(&pt.name);
            let rev = if park {
                matches!(derive_inverse(&pt.name, &pt.input, &pt.outcome), Inverse::Ready { .. })
            } else {
                // A non-gated tool: there is nothing to reverse (no gate parks it).
                true
            };
            (Some(pt.name), park, rev)
        }
        // No side-effecting action projected: nothing parks, nothing to reverse.
        None => (None, false, true),
    };

    let mode_str = match &selection {
        crate::selector::Selection::Route(m) => m.as_str().to_string(),
        crate::selector::Selection::Clarify(_) => "clarify".to_string(),
    };

    let why = build_why(&mode_str, &tool, would_park, reversible, &agent, tier);

    PlannedOutcome {
        intent: predicted.intent.clone(),
        agent,
        mode: mode_str,
        tier: tier.as_str().to_string(),
        tool,
        would_park,
        reversible,
        confidence: predicted.confidence,
        why,
    }
}

/// Compose the grounded one-line rationale for the plan.
fn build_why(
    mode: &str,
    tool: &Option<String>,
    would_park: bool,
    reversible: bool,
    agent: &str,
    tier: Tier,
) -> String {
    if mode == "clarify" {
        return "A real run would ask one clarifying question first (recurring vs. one-off) and act on nothing.".to_string();
    }
    if would_park {
        let t = tool.as_deref().unwrap_or("a consequential action");
        let rev = if reversible { "reversible" } else { "irreversible" };
        return format!(
            "A real run would delegate to {agent} and PARK '{t}' at the confirmation gate for a spoken yes ({rev}); PRECOG never satisfies that gate."
        );
    }
    format!(
        "A real run would handle this as a {mode} turn on the {} tier via {agent}, with no consequential action to confirm.",
        tier.as_str()
    )
}

// ---------------------------------------------------------------------------
// Planned-tool projection — the deterministic side-effecting vocabulary
// ---------------------------------------------------------------------------

/// A projected tool CLASS: the (real) consequential tool name a real run would
/// engage, plus a REPRESENTATIVE synthetic `input`/`outcome` so
/// [`crate::journal::derive_inverse`] returns the CLASS-accurate reversibility
/// verdict. No specific action has run — these stand in for "an action of this
/// class", never a real invocation.
struct PlannedTool {
    name: String,
    input: Value,
    outcome: String,
}

impl PlannedTool {
    /// A tool whose reversibility is INPUT-INDEPENDENT (the inverse verdict is the
    /// same for every input, e.g. `gmail_send` is always irreversible).
    fn bare(name: &str) -> Self {
        PlannedTool { name: name.to_string(), input: json!({}), outcome: String::new() }
    }
    /// A tool whose inverse depends on input fields — supply a representative input.
    fn with_input(name: &str, input: Value) -> Self {
        PlannedTool { name: name.to_string(), input, outcome: String::new() }
    }
}

/// Project the CONSEQUENTIAL tool a real run would engage from the utterance's
/// side-effecting vocabulary, or `None` for a benign/conversational turn. PURE and
/// deterministic. Precedence:
///   1. The selector's `Standing` mode (a recurring cadence) always projects
///      `standing_create` — establishing recurring autonomy is the gated action.
///   2. Otherwise, the FIRST matching deterministic action cue wins (most specific
///      first), mirroring the vocabulary [`crate::confirm::CONSEQUENTIAL_TOOLS`]
///      gates on. A tool with an input-dependent inverse (home control) carries a
///      representative input so the reversibility verdict is class-accurate.
///
/// This deliberately does NOT try to guess the exact tool a cloud model might pick
/// mid-turn (a model decision PRECOG cannot know); it projects the tool CLASS from
/// the deterministic action vocabulary, and returns `None` when no such action is
/// named — an honest "no consequential action".
fn plan_tool(utterance: &str, mode: Option<Mode>) -> Option<PlannedTool> {
    // 1. A recurring cadence -> establishing a standing mission (gated). The
    //    synthetic outcome carries a placeholder id in the exact "It's saved (id
    //    <hex>)" shape journal::extract_standing_id parses, so derive_inverse
    //    reports the honest class verdict: reversible via the wired standing_cancel.
    if mode == Some(Mode::Standing) {
        return Some(PlannedTool {
            name: "standing_create".to_string(),
            input: json!({}),
            outcome: "It's saved (id abc123).".to_string(),
        });
    }

    let t = utterance.to_lowercase();
    let has = |cues: &[&str]| cues.iter().any(|c| t.contains(c));

    // 2. Deterministic side-effecting cues, most specific first. --------------

    // Home / smart-home control (dume_control): the inverse is INPUT-DEPENDENT
    // (turn_on/unlock are reversible; lock/set are not), so carry a representative
    // {entity_id, action} the deriver reads.
    if let Some(action) = home_action(&t) {
        return Some(PlannedTool::with_input(
            "dume_control",
            json!({ "entity_id": "home_device", "action": action }),
        ));
    }

    // Calendar event (before mail so "schedule a meeting and email them" prefers the
    // more specific calendar cue).
    if has(&["create a calendar event", "add it to my calendar", "add to my calendar", "put it on my calendar", "put on my calendar", "schedule a meeting", "book a meeting", "set up a meeting", "add a calendar event", "add an event to"]) {
        return Some(PlannedTool::bare("gcal_create_event"));
    }
    // Email.
    if has(&["send an email", "send them an email", "send him an email", "send her an email", "send a mail", "email my", "email the team", "shoot an email", "fire off an email", "send out an email"]) {
        return Some(PlannedTool::bare("gmail_send"));
    }
    // Slack.
    if has(&["post to slack", "post in slack", "post a slack", "send a slack", "message on slack", "slack the team", "post it to slack"]) {
        return Some(PlannedTool::bare("slack_post_message"));
    }
    // X / Twitter.
    if has(&["post to x", "post on x", "post a tweet", "tweet that", "send a tweet", "tweet it"]) {
        return Some(PlannedTool::bare("x_post"));
    }
    // LinkedIn.
    if has(&["post to linkedin", "post on linkedin", "share on linkedin", "linkedin post"]) {
        return Some(PlannedTool::bare("linkedin_post"));
    }
    // GitHub PR / issue comment.
    if has(&["open a pr", "open a pull request", "open the pr", "raise a pr", "create a pull request"]) {
        return Some(PlannedTool::bare("github_open_pr"));
    }
    if has(&["comment on the issue", "comment on issue", "leave a comment on the issue", "post a comment on the issue"]) {
        return Some(PlannedTool::bare("github_comment_issue"));
    }
    // Google Drive upload.
    if has(&["upload to drive", "upload it to drive", "save to google drive", "upload to google drive", "put it in my drive"]) {
        return Some(PlannedTool::bare("gdrive_upload_text"));
    }
    // Ad campaigns — Meta first (its cues carry the brand), then Google Ads.
    if has(&["pause the meta campaign", "pause the facebook campaign", "pause the instagram campaign"]) {
        return Some(PlannedTool::with_input("meta_pause_campaign", json!({ "campaign_id": "camp_1" })));
    }
    if has(&["resume the meta campaign", "resume the facebook campaign", "enable the meta campaign"]) {
        return Some(PlannedTool::with_input("meta_resume_campaign", json!({ "campaign_id": "camp_1" })));
    }
    if has(&["set the meta budget", "set the facebook budget", "change the meta budget"]) {
        return Some(PlannedTool::with_input("meta_set_budget", json!({ "campaign_id": "camp_1" })));
    }
    if has(&["pause the campaign", "pause the ad campaign", "pause the google ads campaign", "pause my campaign"]) {
        return Some(PlannedTool::with_input("gads_pause_campaign", json!({ "campaign_id": "camp_1" })));
    }
    if has(&["enable the campaign", "resume the campaign", "turn the campaign back on"]) {
        return Some(PlannedTool::with_input("gads_enable_campaign", json!({ "campaign_id": "camp_1" })));
    }
    if has(&["set the ad budget", "set the campaign budget", "change the ad budget", "set the daily budget"]) {
        return Some(PlannedTool::with_input("gads_set_budget", json!({ "campaign_id": "camp_1" })));
    }
    // Sandboxed shell — the most consequential tool.
    if has(&["run the command", "run a shell command", "run this in the terminal", "run it in the terminal", "execute the command", "run the shell command"]) {
        return Some(PlannedTool::bare("shell_run"));
    }
    // Adding an MCP connector — a persistent mutation of the tool surface.
    if has(&["add a connector", "add an mcp connector", "install a connector", "add the mcp server"]) {
        return Some(PlannedTool::bare("connector_add"));
    }

    // No side-effecting action named -> a benign/conversational turn.
    None
}

/// Detect a home-control action verb SCOPED to a home-device noun, returning the
/// `dume_control` `action` string the inverse deriver reads. Mirrors the
/// device-context scoping in `agents.rs::is_home_query`: the broad verbs
/// ("turn on/off", "lock/unlock", "set") only count alongside a home-device noun,
/// so "turn on do-not-disturb" is NOT read as a device action.
fn home_action(t: &str) -> Option<&'static str> {
    const DEVICE_NOUNS: &[&str] = &[
        "light", "lights", "lamp", "thermostat", "heater", "heating", "ac", "fan",
        "door", "lock", "blinds", "shades", "outlet", "plug", "bedroom", "living room",
        "kitchen", "garage", "scene", "hvac",
    ];
    let has_device = DEVICE_NOUNS.iter().any(|n| t.contains(n));
    if !has_device {
        return None;
    }
    if t.contains("unlock") {
        return Some("unlock");
    }
    if t.contains("lock") {
        return Some("lock");
    }
    if t.contains("turn on") {
        return Some("turn_on");
    }
    if t.contains("turn off") {
        return Some("turn_off");
    }
    if t.contains("set the ") || t.contains("dim ") {
        return Some("set");
    }
    None
}

// ---------------------------------------------------------------------------
// Hypothetical extraction — the "what would you do if I said X" router cue
// ---------------------------------------------------------------------------

/// The high-precision cue phrases that introduce a PRECOG hypothetical. Each is
/// already specific (a plain substring match is enough) and carries the tail as the
/// utterance to simulate. Deliberately does NOT include a bare "simulate" /
/// "precog" (those would poach CASSANDRA's forecast turns) — only the unambiguous
/// "what would you do if I said/asked/told you to X" framing routes here.
/// ORDER MATTERS: a longer, more-specific cue must precede any cue it contains as
/// a prefix, so the first `.find` match is the most specific (e.g. "... asked you
/// to X" wins over "... asked X", which would otherwise leave "you to X" as the
/// tail). Overlapping pairs are kept longest-first below.
const PRECOG_CUES: &[&str] = &[
    "what would you do if i asked you to ",
    "what would you do if i asked ",
    "what would you do if i told you to ",
    "what would you do if i said ",
    "what would you do if i say ",
    "what would you do if i wanted to ",
    "what would you do if you were told to ",
    "what would happen if i asked you to ",
    "what would happen if i said ",
    "what would happen if you ",
];

/// Extract the HYPOTHETICAL utterance from a "what would you do if I said X"
/// query, or `None` when the text is not a PRECOG query. PURE. Conservatively
/// anchored on the high-precision [`PRECOG_CUES`] so an ordinary sentence never
/// trips it. The tail is cleaned of wrapping quotes and a trailing '?'; an empty
/// tail yields `None` (nothing to simulate).
///
/// ASCII-safe extraction: the cue is matched on a lowercased copy; the original
/// tail is sliced only when lowercasing changed no byte lengths (all-ASCII, the
/// common case) so casing/offsets are preserved, else the lowercased tail is used.
pub fn extract_hypothetical(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let lower = trimmed.to_lowercase();
    let ascii_stable = lower.len() == trimmed.len();
    for cue in PRECOG_CUES {
        if let Some(pos) = lower.find(cue) {
            let start = pos + cue.len();
            let tail = if ascii_stable && trimmed.is_char_boundary(start) {
                &trimmed[start..]
            } else {
                &lower[start..]
            };
            let cleaned = clean_hypothetical(tail);
            if !cleaned.is_empty() {
                return Some(cleaned);
            }
        }
    }
    None
}

/// Trim a hypothetical tail: drop surrounding quotes (straight + curly) and a
/// trailing sentence terminator, then trim whitespace.
fn clean_hypothetical(tail: &str) -> String {
    let mut s = tail.trim();
    // Strip a trailing terminator ("...?" / "...!" / "...." ).
    s = s.trim_end_matches(['?', '!', '.']).trim();
    // Strip a single wrapping quote pair (straight or curly).
    if let Some(inner) = unquote(s) {
        s = inner.trim();
    }
    s.to_string()
}

/// If `s` opens with a quote (straight or curly), return the content after it
/// (and before a matching closing quote when present) — a single wrapping quote
/// pair is dropped. `None` when `s` does not open with a quote.
fn unquote(s: &str) -> Option<&str> {
    for (open, close) in [('"', '"'), ('\'', '\''), ('\u{201c}', '\u{201d}'), ('\u{2018}', '\u{2019}')] {
        if let Some(inner) = s.strip_prefix(open) {
            return Some(inner.strip_suffix(close).unwrap_or(inner));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentRegistry, LexicalAgentScorer};
    use crate::config::Config;

    /// Build a read-only SimContext over the canonical roster + default config with
    /// the shipped lexical scorer — exactly the wiring the router uses.
    fn ctx<'a>(
        agents: &'a AgentRegistry,
        cfg: &'a Config,
        scorer: &'a LexicalAgentScorer,
        cloud_reachable: bool,
    ) -> SimContext<'a, LexicalAgentScorer> {
        SimContext { agents, cfg, scorer, override_tier: None, cloud_reachable }
    }

    fn predicted(intent: &str, confidence: f64, complexity: &str) -> PredictedIntent {
        PredictedIntent { intent: intent.to_string(), confidence, complexity: complexity.to_string() }
    }

    // ---- STRUCTURAL READ-ONLY INVARIANT (mirrors focus.rs) -----------------

    #[test]
    fn sim_context_has_no_actuator_or_write_handle() {
        // STANDING ASSERTION (read with the SimContext def): the ONLY things a
        // SimContext exposes are read views — a shared &AgentRegistry, a read-only
        // &Config, a pure &AgentScorer, and two Copy values. There is no
        // `&mut Memory`, no InferenceClient/Brain handle, no actuator, no confirm
        // slot. The exhaustive destructure below is the proof: if a future edit
        // added a write/actuator handle to SimContext, this test would FAIL TO
        // COMPILE, forcing a re-review of the PRECOG read-only invariant.
        let (agents, _issues) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let SimContext {
            agents: _,
            cfg: _,
            scorer: _,
            override_tier: _,
            cloud_reachable: _,
        } = c;
        // (No assertions needed — the exhaustive pattern IS the proof: the type has
        // only read-only fields, so simulate() provably cannot act.)
    }

    #[test]
    fn simulate_takes_only_read_views_and_returns_a_description() {
        // simulate's signature is (utterance, &PredictedIntent, &SimContext) ->
        // PlannedOutcome. It borrows the context immutably and returns a plain data
        // description — there is no `&mut`, no owned actuator, no side channel. This
        // test simply exercises that signature end-to-end on a benign utterance and
        // asserts the result is a pure description (no gate touched).
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let out = simulate("open safari", &predicted("app.launch", 0.95, "light"), &c);
        assert!(!out.would_park, "a benign app launch never parks");
        assert_eq!(out.tool, None, "a benign app launch names no consequential tool");
        // The telemetry pins the never-executes contract.
        let tel = out.telemetry("open safari");
        assert_eq!(tel["executed"], false);
        assert_eq!(tel["satisfied_a_gate"], false);
    }

    // ---- PLANNED-OUTCOME DERIVATION on representative utterances ------------

    #[test]
    fn benign_open_app_plans_no_gated_action() {
        // A benign local action ("open safari"): no consequential tool, no park,
        // trivially reversible (nothing gated), and the plan reads as a plain turn.
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let out = simulate("open safari", &predicted("app.launch", 0.95, "light"), &c);
        assert_eq!(out.tool, None);
        assert!(!out.would_park, "no gate for a benign action");
        assert!(out.reversible, "nothing gated -> nothing to reverse");
        assert_eq!(out.mode, "one_shot");
        assert!(out.why.to_lowercase().contains("no consequential action"), "why: {}", out.why);
    }

    #[test]
    fn consequential_email_plans_a_park_and_is_irreversible() {
        // A consequential, IRREVERSIBLE action ("send an email ..."): projects
        // gmail_send, would_park, and derive_inverse says sent mail can't be unsent.
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let out = simulate(
            "send an email to the team about the launch",
            &predicted("conversation", 0.8, "light"),
            &c,
        );
        assert_eq!(out.tool.as_deref(), Some("gmail_send"));
        assert!(out.would_park, "a consequential tool parks at the gate");
        assert!(!out.reversible, "sent mail can't be unsent");
        assert!(out.why.contains("PARK"), "why states the park: {}", out.why);
        // The spoken summary is honest about never satisfying the gate.
        let spoken = out.spoken_summary("send an email to the team about the launch");
        assert!(spoken.to_lowercase().contains("would not run") || spoken.contains("PARK"), "spoken: {spoken}");
        assert!(spoken.to_lowercase().contains("never satisfies"), "spoken names the gate contract: {spoken}");
    }

    #[test]
    fn consequential_but_reversible_home_control_plans_a_reversible_park() {
        // A consequential action WITH a safe inverse ("turn on the living room
        // lights"): projects dume_control turn_on, would_park, and derive_inverse
        // reports Ready (turn_off is the wired inverse).
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let out = simulate("turn on the living room lights", &predicted("conversation", 0.7, "light"), &c);
        assert_eq!(out.tool.as_deref(), Some("dume_control"));
        assert!(out.would_park);
        assert!(out.reversible, "turning a light on is mechanically reversible (turn off)");
    }

    #[test]
    fn locking_a_door_plans_a_park_but_is_honestly_irreversible() {
        // Undoing a `lock` would mean arming an unlock — derive_inverse refuses it,
        // so PRECOG honestly reports this consequential action as irreversible.
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let out = simulate("lock the front door", &predicted("conversation", 0.7, "light"), &c);
        assert_eq!(out.tool.as_deref(), Some("dume_control"));
        assert!(out.would_park);
        assert!(!out.reversible, "arming an unlock is not offered as an undo");
    }

    #[test]
    fn recurring_cadence_plans_a_standing_create_park_reversible_by_design() {
        // A hard recurring cue ("every morning ...") routes the selector to Standing
        // -> the projected tool is standing_create (establishing recurring autonomy,
        // gated) and it is reversible by design (the wired, ungated standing_cancel).
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let out = simulate("every morning brief me on my deadlines", &predicted("conversation", 0.8, "light"), &c);
        assert_eq!(out.mode, "standing");
        assert_eq!(out.tool.as_deref(), Some("standing_create"));
        assert!(out.would_park, "establishing a standing mission parks at the gate");
        assert!(out.reversible, "a standing mission is reversible via standing_cancel");
    }

    #[test]
    fn ambiguous_recurring_lean_plans_a_clarify_and_acts_on_nothing() {
        // RAIL 1: a semantic lean toward recurring autonomy with NO hard cue makes
        // the selector CLARIFY. PRECOG reports mode=clarify, no tool, no park — a
        // real run would ask a question, not act.
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        // "look after my deadlines for me" is the shipped-scorer ambiguous case the
        // selector's own tests use for the clarify rail.
        let out = simulate("look after my deadlines for me", &predicted("conversation", 0.5, "light"), &c);
        // With the lexical scorer this is either a clarify or a safe one_shot; in
        // BOTH cases PRECOG must never plan a consequential park from a mere lean.
        assert!(!out.would_park, "a mere lean toward autonomy never plans a park");
        assert_eq!(out.tool, None, "no consequential tool from an ambiguous lean");
    }

    #[test]
    fn plain_question_plans_a_one_shot_no_park() {
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let out = simulate("what's the weather like today", &predicted("conversation", 0.9, "light"), &c);
        assert_eq!(out.mode, "one_shot");
        assert!(!out.would_park);
        assert_eq!(out.tool, None);
    }

    #[test]
    fn tier_projection_matches_the_resolver_precedence() {
        // A HEAVY complexity turn on the shipped cloud_heavy default resolves to
        // heavy when the cloud is reachable, and DEGRADES to local (fallback) when
        // it is not — exactly resolve_tier's precedence, surfaced by PRECOG.
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let online = ctx(&agents, &cfg, &scorer, true);
        let out = simulate("explain quantum entanglement in depth", &predicted("conversation", 0.9, "heavy"), &online);
        assert_eq!(out.tier, "heavy", "a hard turn resolves to heavy online");
        let offline = ctx(&agents, &cfg, &scorer, false);
        let out = simulate("explain quantum entanglement in depth", &predicted("conversation", 0.9, "heavy"), &offline);
        assert_eq!(out.tier, "local", "a cloud tier degrades to local when unreachable");
    }

    // ---- HYPOTHETICAL EXTRACTION -------------------------------------------

    #[test]
    fn extracts_the_hypothetical_from_the_precog_cue() {
        assert_eq!(
            extract_hypothetical("what would you do if I said send an email to Bob?").as_deref(),
            Some("send an email to Bob"),
        );
        assert_eq!(
            extract_hypothetical("What would you do if I asked you to lock the front door").as_deref(),
            Some("lock the front door"),
        );
        // Wrapping quotes are stripped.
        assert_eq!(
            extract_hypothetical("what would you do if I said \"open safari\"").as_deref(),
            Some("open safari"),
        );
    }

    #[test]
    fn non_precog_text_and_bare_simulate_are_not_hijacked() {
        // An ordinary sentence is not a PRECOG query.
        assert_eq!(extract_hypothetical("send an email to Bob"), None);
        // A bare "simulate" (CASSANDRA's forecast vocabulary) is NOT a PRECOG cue —
        // PRECOG only claims the explicit "what would you do if I said X" framing.
        assert_eq!(extract_hypothetical("simulate the stock over a year"), None);
        // An empty tail yields nothing to simulate.
        assert_eq!(extract_hypothetical("what would you do if I said"), None);
    }

    #[test]
    fn end_to_end_extract_then_simulate_is_honest_about_the_gate() {
        // The exact router flow: extract the hypothetical, then simulate it. A
        // consequential hypothetical is reported as a park PRECOG never satisfies.
        let (agents, _) = AgentRegistry::load(std::path::Path::new("/nonexistent/agents.toml"));
        let cfg = Config::default();
        let scorer = LexicalAgentScorer;
        let c = ctx(&agents, &cfg, &scorer, true);
        let hyp = extract_hypothetical("what would you do if I said post to slack that we shipped").unwrap();
        assert_eq!(hyp, "post to slack that we shipped");
        let out = simulate(&hyp, &predicted("conversation", 0.8, "light"), &c);
        assert_eq!(out.tool.as_deref(), Some("slack_post_message"));
        assert!(out.would_park);
    }
}
