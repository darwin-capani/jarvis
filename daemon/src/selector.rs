//! CAPABILITY SELECTOR — the smart, higher-level dispatch that decides WHICH
//! capability a natural request engages, so the user never manages modes.
//!
//! This is the layer that sits in [`crate::router::route`] BEFORE agent
//! selection. The agent scorer ([`crate::agents`]) decides WHICH AGENT handles a
//! turn; the selector decides WHICH MODE the turn is — a plain one-shot answer, a
//! read of the shared World Model, a fold of a stated fact INTO the World Model, a
//! complex multi-step mission NOW (FURY), or the SETUP of a recurring/durable
//! standing mission. The user can always be explicit ("just answer", "set up a
//! standing mission to ...") and override the selector; the selector only steps in
//! to route a phrasing that didn't name its own mode.
//!
//! ## How it decides (deterministic cues FIRST, semantic fallback SECOND)
//!
//! 1. **Deterministic cues** — fast, authoritative, the same discipline the rest
//!    of the router uses. Recurring-time phrases ("every morning", "from now on",
//!    "each day", "keep watching", "whenever X happens") -> `Standing`. World-read
//!    phrases ("what's the state of", "who's on", "what's due") -> `WorldQuery`.
//!    World-write phrases ("remember that X is now", "the deadline moved to",
//!    "X joined the project") -> `WorldUpdate`. Multi-step-now phrases ("plan and
//!    kick off", "research and then ...") -> `Mission`. Anything else falls
//!    through.
//! 2. **Semantic fallback** — reuse the smart-routing [`AgentScorer`] machinery
//!    (injectable for tests) to score the utterance against a short role text for
//!    each non-default mode. It only PROMOTES a mode out of `OneShot` when the best
//!    score is BOTH above an absolute floor AND clearly ahead of the runner-up; a
//!    weak/tied/absent signal stays `OneShot`. Pure and deterministic — no clock,
//!    no I/O, no network (the shipped scorer is honest keyword-semantic BM25).
//!
//! ## The two safety rails (NON-NEGOTIABLE)
//!
//! **Rail 1 — clarify or safe-default when uncertain, NEVER guess into autonomy
//! or a consequential action.** The selector must never silently pick a mode that
//! establishes standing autonomy from a low-confidence guess. When the only signal
//! pointing at `Standing` is a weak semantic one (no hard recurring cue), the
//! selector does NOT propose a standing mission — it asks a single one-line
//! clarifying question ("did you mean run this every morning, or just once now?")
//! or, when there's nothing to clarify, falls back to a plain one-shot answer. The
//! default-safe outcome is always a one-shot answer or one clarifying question.
//!
//! **Rail 2 — no silent autonomy.** The `Standing` mode only ever PROPOSES the
//! standing mission: it routes to `standing_create`, which PARKS behind the
//! cross-turn confirmation gate (and the armed-by-default master switch, which still
//! requires a fresh per-action confirm) before
//! anything is established. The selector NEVER creates a standing mission itself.
//! `WorldUpdate` writes ONLY the shared `user.world.*` tier — never a consequential
//! external action, never a private agent namespace.
//!
//! Everything here is HERMETIC: [`classify_mode`] is a pure function of the
//! utterance + an injected [`AgentScorer`]; no live tick, no real cloud, no clock.

use crate::agents::AgentScorer;

/// Absolute floor a semantic score must clear before it can PROMOTE a turn out of
/// the safe [`Mode::OneShot`] default. Below this the signal is treated as noise.
/// The shipped scorer ([`crate::agents::LexicalAgentScorer`]) returns UNBOUNDED
/// BM25 scores, so this floor only screens out near-zero noise; the MARGIN below
/// (a relative test, scale-independent) is the real discriminator.
const SEMANTIC_FLOOR: f64 = 0.30;

/// Multiplicative margin the best semantic mode must beat the runner-up by to be
/// trusted (best >= MARGIN * runner_up). Deliberately strict: a single shared
/// word (e.g. "set" overlapping "set up") produces a weak, non-dominant lean that
/// must NOT promote. A near-tie is "ambiguous" -> one-shot (or, for an
/// autonomy-shaped tie, clarify). Scale-independent, so it works on raw BM25.
const SEMANTIC_MARGIN: f64 = 2.0;

/// The CAPABILITY the selector routes a request to. `OneShot` is the safe default
/// (the normal pipeline). The other four engage a specific subsystem. Only
/// `Standing` is consequential (it establishes recurring autonomy) — and even then
/// it only PROPOSES (parks behind the confirmation gate); the selector never
/// creates one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The normal pipeline — a plain one-shot answer/action. The default-safe
    /// outcome whenever the request doesn't clearly call for another capability.
    OneShot,
    /// Answer from the shared World Model ("what's the state of project X / who's
    /// on it / what's due"). Read-only.
    WorldQuery,
    /// Fold a stated fact INTO the shared World Model ("the deadline moved to
    /// Friday", "Sam joined the project"). Writes ONLY `user.world.*`.
    WorldUpdate,
    /// A complex multi-step goal to run NOW (FURY, bounded).
    Mission,
    /// A RECURRING / durable goal -> PROPOSE a standing mission (which is then
    /// CONFIRMED via the gate before it's established). Never silently created.
    Standing,
}

impl Mode {
    /// A stable identifier for telemetry / the HUD selector card.
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::OneShot => "one_shot",
            Mode::WorldQuery => "world_query",
            Mode::WorldUpdate => "world_update",
            Mode::Mission => "mission",
            Mode::Standing => "standing",
        }
    }

    /// Whether engaging this mode is CONSEQUENTIAL (establishes standing
    /// autonomy). Only `Standing` is — and even it only proposes behind the gate.
    /// The selector uses this to enforce Rail 1: a consequential mode is NEVER
    /// reached from a low-confidence guess.
    pub fn is_consequential(&self) -> bool {
        matches!(self, Mode::Standing)
    }
}

/// The selector's decision for a turn. Either a chosen [`Mode`] to route to, or a
/// single one-line CLARIFYING question to speak (Rail 1) when the request is
/// genuinely ambiguous between a safe one-shot and establishing autonomy. A
/// clarify is itself default-safe: it commits to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Route this turn to the given capability.
    Route(Mode),
    /// Ask the user this one-line question instead of acting — used only to avoid
    /// guessing into autonomy/consequence. The router speaks it and stops.
    Clarify(String),
}

impl Selection {
    /// The mode this selection routes to, if it routes (None when it clarifies).
    /// Public predicate used by the optimizer-trace bookkeeping in `main` (to
    /// label a turn's mode) and exercised by the selector tests (the router
    /// matches on the `Selection` variants directly).
    pub fn mode(&self) -> Option<Mode> {
        match self {
            Selection::Route(m) => Some(*m),
            Selection::Clarify(_) => None,
        }
    }
}

/// The single one-line clarifying question for the recurring-vs-once ambiguity
/// (Rail 1). Kept as a function so the router and tests share the exact copy.
pub fn recurring_clarify() -> String {
    "Did you mean for me to do this every day from now on, or just once right now?".to_string()
}

/// CLASSIFY the request into a [`Selection`]. PURE and DETERMINISTIC — a function
/// of the utterance and the injected [`AgentScorer`] only (no clock, no I/O, no
/// network). Deterministic cues are authoritative and run FIRST; the semantic
/// scorer is a fallback that can only PROMOTE out of the safe one-shot default
/// when its signal is strong and unambiguous.
///
/// The two rails are enforced HERE:
///   * Rail 1: a `Standing` promotion is allowed ONLY from a hard recurring cue.
///     A merely-semantic lean toward autonomy NEVER silently establishes a mode —
///     it clarifies (recurring-shaped) or stays one-shot.
///   * Rail 2: the returned `Standing` mode is a PROPOSAL only; the router maps it
///     to `standing_create` (parks behind the gate). The selector creates nothing.
pub fn classify_mode<S: AgentScorer>(utterance: &str, scorer: &S) -> Selection {
    let raw = utterance.trim();
    if raw.is_empty() {
        // Nothing to act on — the safe default.
        return Selection::Route(Mode::OneShot);
    }
    let text = raw.to_lowercase();

    // ----- DETERMINISTIC CUES (authoritative, run first) ---------------------

    // STANDING: recurring/durable cadence. These are the hard cues the user can
    // rely on; only a hard cue may reach the consequential standing mode.
    if has_recurring_cue(&text) {
        return Selection::Route(Mode::Standing);
    }

    // WORLD UPDATE: an explicit "fold this stated fact into what you know" — a
    // remember/record directive, or a state-change statement about a tracked
    // entity. Checked before WorldQuery so "remember that X is now Y" (which also
    // contains "is now") is a write, not a read.
    if has_world_update_cue(&text) {
        return Selection::Route(Mode::WorldUpdate);
    }

    // WORLD QUERY: a read of the tracked world ("what's the state/status of",
    // "who's on", "what's due").
    if has_world_query_cue(&text) {
        return Selection::Route(Mode::WorldQuery);
    }

    // MISSION: a complex multi-step job to run NOW (plan + execute), with no
    // recurring cadence (a recurring multi-step goal already routed to Standing).
    if has_mission_cue(&text) {
        return Selection::Route(Mode::Mission);
    }

    // ----- SEMANTIC FALLBACK (injectable; only PROMOTES on a strong signal) ---

    semantic_mode(&text, scorer)
}

/// Hard recurring/durable cadence cues -> Standing. Conservative on purpose: only
/// phrasings that unambiguously mean "do this repeatedly / from now on". A single
/// "once" / "right now" / "just this once" anywhere VETOES a recurring read (the
/// user is explicitly scoping it to one run), so "do this every day — well, just
/// once for now" does not arm autonomy.
fn has_recurring_cue(text: &str) -> bool {
    // Explicit one-shot scoping vetoes recurrence — the user overrode it.
    const ONE_SHOT_OVERRIDES: &[&str] =
        &["just once", "only once", "just this once", "one time", "right now,", "for now"];
    for ov in ONE_SHOT_OVERRIDES {
        if text.contains(ov) {
            return false;
        }
    }
    const RECURRING: &[&str] = &[
        "every morning",
        "every day",
        "every evening",
        "every night",
        "every week",
        "every hour",
        "every monday",
        "every tuesday",
        "every wednesday",
        "every thursday",
        "every friday",
        "every saturday",
        "every sunday",
        "each morning",
        "each day",
        "each evening",
        "each week",
        "each night",
        "from now on",
        "going forward",
        "keep watching",
        "keep an eye on",
        "keep monitoring",
        "keep tracking",
        "keep me posted",
        "keep me updated",
        "every single day",
        "on a schedule",
        "on a recurring",
        "recurring",
        "daily",
        "nightly",
        "hourly",
        "weekly",
        "whenever ",
        "any time ",
        "anytime ",
        "always let me know",
        "always tell me",
        "always alert me",
        "always remind me",
        "every few hours",
        "every couple hours",
        "twice a day",
        "once a day",
        "once a week",
    ];
    if RECURRING.iter().any(|c| text.contains(c)) {
        return true;
    }
    // Numeric interval cadence: "every 6 hours", "every 30 minutes", "every 2
    // days" — the same shape standing::Schedule::parse reads as an Interval. A
    // hard recurring cue (not a guess), so it routes straight to the gated setup.
    has_numeric_interval(text)
}

/// Detect an "every N <hours|minutes|days>" cadence (a hard recurring cue).
/// Scans for "every " followed by a number and a time-unit word — mirroring the
/// interval phrasing `standing::Schedule::parse` accepts, so the selector and the
/// schedule parser agree on what counts as recurring.
fn has_numeric_interval(text: &str) -> bool {
    // The unit word must BE a time unit (optionally pluralized), not a longer word
    // that merely CONTAINS a unit substring — else "every 4 items on monday" trips
    // on "day" inside "monday" (and "every 5 reminders" on "min" inside "reminder"),
    // wrongly routing a one-shot to a recurring-autonomy proposal.
    fn is_unit(word: &str) -> bool {
        const UNITS: &[&str] = &["hour", "minute", "min", "day", "week"];
        let w = word.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
        UNITS
            .iter()
            .any(|u| w.strip_prefix(u).is_some_and(|r| r.is_empty() || r == "s"))
    }
    let mut rest = text;
    while let Some(pos) = rest.find("every ") {
        let after = &rest[pos + "every ".len()..];
        let mut words = after.split_whitespace();
        // The first token after "every " must START with a digit (e.g. "6 hours").
        if let Some(first) = words.next() {
            if first.starts_with(|c: char| c.is_ascii_digit()) {
                // The unit is either fused onto the number ("6hours") or the very
                // NEXT word ("6 hours") — check ONLY that word, as a whole unit.
                let fused = first.trim_start_matches(|c: char| c.is_ascii_digit());
                let unit_word = if fused.is_empty() { words.next().unwrap_or("") } else { fused };
                if is_unit(unit_word) {
                    return true;
                }
            }
        }
        rest = after;
    }
    false
}

/// World-WRITE cues -> WorldUpdate. Two families: an explicit remember/record
/// directive ("remember that ...", "note that ...", "make a note that ..."), and a
/// state-change STATEMENT about a tracked entity ("the deadline moved to ...",
/// "X slipped to ...", "Sam joined the project", "the launch is now ..."). These
/// fold a known fact into the SHARED world; they never fire an external action.
fn has_world_update_cue(text: &str) -> bool {
    // Explicit record/remember directives.
    const RECORD: &[&str] = &[
        "remember that ",
        "make a note that ",
        "note that ",
        "for the record,",
        "jot down that ",
        "record that ",
        "update the world model",
        "update your notes",
        "save this:",
    ];
    if RECORD.iter().any(|c| text.contains(c)) {
        return true;
    }
    // State-change statements: a tracked-entity word PLUS a change/assignment verb.
    // Requiring both keeps a generic "the weather moved on" from looking like a
    // world write while catching "the launch slipped to next Tuesday" / "Sam
    // joined the project" / "the deadline moved to Friday".
    let mentions_entity = ENTITY_WORDS.iter().any(|w| text.contains(w));
    const CHANGE_VERBS: &[&str] = &[
        " moved to ",
        " slipped to ",
        " slipped ",
        " pushed to ",
        " is now ",
        " are now ",
        " was moved ",
        " got moved ",
        " changed to ",
        " is rescheduled ",
        " rescheduled to ",
        " joined ",
        " left ",
        " is assigned to ",
        " was assigned ",
        " is blocked ",
        " is done ",
        " is complete",
        " is at risk",
        " is delayed",
        " bumped to ",
        " postponed to ",
        " is owned by ",
        " owns ",
        " is responsible for ",
    ];
    mentions_entity && CHANGE_VERBS.iter().any(|v| text.contains(v))
}

/// World-READ cues -> WorldQuery. A question about the tracked world's STATE: the
/// "what's the state/status of <X>", "who's on/working on <X>", "what's due / on
/// my plate" shapes. Requires a question/read framing so a plain statement isn't
/// read as a query.
fn has_world_query_cue(text: &str) -> bool {
    const STATE_PHRASES: &[&str] = &[
        "what's the state of",
        "what is the state of",
        "whats the state of",
        "what's the status of",
        "what is the status of",
        "whats the status of",
        "status of the",
        "state of the",
        "where do things stand",
        "where do we stand",
        "where does ",
        "how is the ",
        "how's the ",
        "what's happening with",
        "what is happening with",
        "what's going on with",
        "who's on ",
        "whos on ",
        "who is on ",
        "who's working on",
        "who is working on",
        "who owns ",
        "who is responsible for",
        "who's responsible for",
        "what's due",
        "what is due",
        "whats due",
        "what's on my plate",
        "what's coming up",
        "what deadlines",
        "which deadlines",
        "when is the ",
        "when's the ",
    ];
    // For the broad "how is the / when is the / where does" shapes, only treat as a
    // world query when an entity word is present (otherwise "how is the weather" is a
    // plain one-shot). But the entity gate applies ONLY when the match is
    // EXCLUSIVELY broad: a SPECIFIC phrase like "what's due" classifies
    // unconditionally, even if a broad phrase co-occurs ("what's due, and how's the
    // office coffee?") — otherwise a co-occurring broad phrase would suppress a real
    // world query.
    const BROAD: &[&str] = &["how is the ", "how's the ", "when is the ", "when's the ", "where does "];
    if STATE_PHRASES.iter().any(|c| text.contains(c) && !BROAD.contains(c)) {
        return true; // a specific state phrase matched
    }
    if BROAD.iter().any(|b| text.contains(b)) {
        return ENTITY_WORDS.iter().any(|w| text.contains(w));
    }
    false
}

/// Multi-step-NOW cues -> Mission. "plan and kick off", "research X and then
/// draft Y", "set up the whole ...". A recurring multi-step goal already left for
/// Standing above, so anything matching here is a one-time bounded job.
fn has_mission_cue(text: &str) -> bool {
    const MISSION_PHRASES: &[&str] = &[
        "plan and ",
        "plan out and ",
        "plan, then ",
        "kick off the ",
        "kick off a ",
        "spin up the ",
        "coordinate the ",
        "orchestrate the ",
        "run a full ",
        "do everything to ",
        "take care of everything for ",
        "handle the whole ",
        "handle everything for ",
        "manage the whole ",
        "end to end",
        "end-to-end",
        "research and then ",
        "investigate and then ",
        "and then draft ",
        "and then send ",
        "multi-step",
        "several steps",
        "break this down and ",
        "break it down and ",
    ];
    MISSION_PHRASES.iter().any(|c| text.contains(c))
}

/// Entity words that anchor a world read/write to a TRACKED thing (project,
/// person-role, deadline, task, topic, thread). Shared by the update + query cues.
const ENTITY_WORDS: &[&str] = &[
    "project",
    "launch",
    "deadline",
    "milestone",
    "task",
    "ticket",
    "deliverable",
    "release",
    "sprint",
    "migration",
    "rollout",
    "the team",
    "thread",
    "topic",
    "initiative",
    "campaign",
    "feature",
];

/// Short role text for each non-default mode, scored by the [`AgentScorer`] in the
/// semantic fallback. The scorer ranks the utterance against these; the order here
/// is the order of [`semantic_modes`].
const MODE_ROLES: &[&str] = &[
    // WorldQuery
    "questions about the current state status who is on what is due of a tracked project deadline person task topic",
    // WorldUpdate
    "record remember note that a fact changed a deadline moved a person joined a project a task is now done",
    // Mission
    "plan coordinate and execute a complex multi step job now research draft set up an entire effort end to end",
    // Standing
    "a recurring durable repeating standing goal to run every day on a schedule from now on keep watching always",
];

/// The non-default modes the semantic fallback can promote to, parallel to
/// [`MODE_ROLES`].
fn semantic_modes() -> [Mode; 4] {
    [Mode::WorldQuery, Mode::WorldUpdate, Mode::Mission, Mode::Standing]
}

/// SEMANTIC FALLBACK: when no hard cue fired, score the utterance against each
/// mode's role text and PROMOTE out of one-shot only on a strong, unambiguous
/// signal. Enforces the rails:
///   * A weak/tied/absent signal -> stays [`Mode::OneShot`] (default-safe).
///   * The winner is `Standing` (consequential) -> NEVER silently routed from a
///     mere semantic lean: clarify instead (Rail 1). The hard recurring cue is the
///     only path that routes straight to Standing.
fn semantic_mode<S: AgentScorer>(text: &str, scorer: &S) -> Selection {
    let modes = semantic_modes();
    let scores = scorer.score(text, MODE_ROLES);
    // A scorer that can't produce a parallel ranking (backend down) returns a
    // wrong-length / empty vector -> no signal -> safe one-shot.
    if scores.len() != MODE_ROLES.len() {
        return Selection::Route(Mode::OneShot);
    }

    // Best + runner-up.
    let mut best_idx = 0usize;
    let mut best = f64::MIN;
    let mut second = f64::MIN;
    for (i, &s) in scores.iter().enumerate() {
        if s > best {
            second = best;
            best = s;
            best_idx = i;
        } else if s > second {
            second = s;
        }
    }

    // Below the absolute floor: noise -> one-shot.
    if best < SEMANTIC_FLOOR {
        return Selection::Route(Mode::OneShot);
    }
    // Not clearly ahead of the runner-up: ambiguous -> one-shot (a near-tie must
    // never be read as a confident mode pick).
    let runner_up = if second.is_finite() && second > 0.0 { second } else { 0.0 };
    if runner_up > 0.0 && best < SEMANTIC_MARGIN * runner_up {
        return Selection::Route(Mode::OneShot);
    }

    let winner = modes[best_idx];
    // RAIL 1 (defense in depth): a CONSEQUENTIAL mode (today only Standing — it
    // arms recurring autonomy) is NEVER established from a semantic guess. The
    // signal leaned recurring but no hard cue confirmed it — ask one clarifying
    // question instead of arming autonomy. Keyed on the `is_consequential`
    // predicate so any future consequential mode inherits the rail automatically.
    if winner.is_consequential() {
        return Selection::Clarify(recurring_clarify());
    }
    // Mission spins up FURY's multi-step engine — too heavy to fire on a mere
    // keyword-overlap lean (a single shared verb like "set"/"run" can tip BM25
    // here). It requires a deterministic MISSION cue; absent one, stay safe. The
    // user can always be explicit ("plan and kick off ...").
    if winner == Mode::Mission {
        return Selection::Route(Mode::OneShot);
    }
    // The cheap, bounded read/shared-write modes may promote on a strong,
    // dominant signal — this is the "smart" part the rails still permit.
    Selection::Route(winner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentScorer;

    /// A scorer the tests drive by hand: returns a fixed score vector. Models a
    /// "backend up" semantic ranker without any inference call. `None` models a
    /// backend that can't rank (wrong-length vector) -> the caller reads no signal.
    struct MockScorer {
        scores: Option<Vec<f64>>,
    }
    impl AgentScorer for MockScorer {
        fn score(&self, _utterance: &str, roles: &[&str]) -> Vec<f64> {
            match &self.scores {
                Some(s) => s.clone(),
                None => vec![0.0; roles.len().saturating_sub(1)], // wrong length -> no signal
            }
        }
    }
    /// A scorer that always returns all-zero (the honest "no signal" a down
    /// embedder produces). Every turn must stay safe (one-shot) under it.
    struct NoSignalScorer;
    impl AgentScorer for NoSignalScorer {
        fn score(&self, _utterance: &str, roles: &[&str]) -> Vec<f64> {
            vec![0.0; roles.len()]
        }
    }

    fn no_signal() -> MockScorer {
        MockScorer { scores: None }
    }

    // ----- Deterministic cue routing (the headline cases) --------------------

    #[test]
    fn recurring_phrase_routes_to_standing_via_hard_cue() {
        // "every morning brief me" -> standing (a hard recurring cue, not a guess).
        for q in [
            "every morning brief me on my deadlines",
            "from now on, keep watching the launch project",
            "each day check the world model for blocked tasks",
            "every 6 hours review my calendar",
            "keep an eye on the migration and tell me if it slips",
            "daily, summarize what's due",
            "whenever new mail arrives, flag anything urgent",
        ] {
            assert_eq!(
                classify_mode(q, &no_signal()),
                Selection::Route(Mode::Standing),
                "should be standing: {q}"
            );
        }
    }

    #[test]
    fn world_status_question_routes_to_world_query() {
        // "what's the status of the launch project" -> world_query.
        for q in [
            "what's the status of the launch project",
            "what is the state of the migration",
            "who's on the launch project",
            "who is working on the migration",
            "what's due this week on the project",
            "where do things stand with the rollout",
        ] {
            assert_eq!(
                classify_mode(q, &no_signal()),
                Selection::Route(Mode::WorldQuery),
                "should be world_query: {q}"
            );
        }
    }

    /// REGRESSION: "every N <non-time-noun> ... <word-containing-a-unit-substring>"
    /// must NOT be read as a recurring interval. The old code scanned the whole
    /// remainder for a unit SUBSTRING, so "day" inside "monday" (and "min" inside
    /// "reminders") wrongly routed a one-shot to Standing (a recurring-autonomy
    /// proposal). A genuine cadence still routes to Standing.
    #[test]
    fn every_n_with_a_non_time_unit_is_not_a_recurring_interval() {
        for q in ["buy every 4 items on monday", "batch every 5 reminders now"] {
            assert_ne!(
                classify_mode(q, &no_signal()),
                Selection::Route(Mode::Standing),
                "no genuine cadence, must not route to Standing: {q}"
            );
        }
        // A REAL numeric cadence (unit or plural) still routes to Standing.
        for q in ["ping the server every 6 hours", "back up every 2 days"] {
            assert_eq!(
                classify_mode(q, &no_signal()),
                Selection::Route(Mode::Standing),
                "genuine cadence: {q}"
            );
        }
    }

    /// REGRESSION: a SPECIFIC world-query cue ("what's due") classifies as
    /// WorldQuery even when a BROAD shape ("how's the ...", which needs an entity
    /// word) co-occurs. The old code applied the entity gate whenever ANY broad
    /// phrase was present, suppressing a real world query.
    #[test]
    fn a_specific_world_cue_survives_a_co_occurring_broad_phrase() {
        assert_eq!(
            classify_mode("what's due, and how's the office coffee?", &no_signal()),
            Selection::Route(Mode::WorldQuery),
        );
    }

    #[test]
    fn stated_fact_change_routes_to_world_update() {
        // "the launch slipped to next Tuesday" -> world_update (shared tier only).
        for q in [
            "the launch slipped to next Tuesday",
            "the deadline moved to Friday",
            "Sam joined the project",
            "the migration is now blocked",
            "remember that the release is now in July",
            "make a note that the project is at risk",
        ] {
            assert_eq!(
                classify_mode(q, &no_signal()),
                Selection::Route(Mode::WorldUpdate),
                "should be world_update: {q}"
            );
        }
    }

    #[test]
    fn world_update_cue_beats_world_query_for_remember_that_is_now() {
        // "remember that X is now Y" contains "is now" (a query-ish shape) but the
        // record directive must win -> a WRITE, never a read.
        assert_eq!(
            classify_mode("remember that the launch is now next month", &no_signal()),
            Selection::Route(Mode::WorldUpdate)
        );
    }

    #[test]
    fn multi_step_now_routes_to_mission() {
        // "plan and kick off the migration" -> mission (FURY).
        for q in [
            "plan and kick off the migration",
            "spin up the whole launch effort",
            "coordinate the rollout end to end",
            "research the vendors and then draft a recommendation",
            "handle everything for the conference booth",
        ] {
            assert_eq!(
                classify_mode(q, &no_signal()),
                Selection::Route(Mode::Mission),
                "should be mission: {q}"
            );
        }
    }

    #[test]
    fn plain_request_and_normal_question_stay_one_shot() {
        // A plain action / normal question -> one_shot (the existing pipeline is
        // unchanged). No hard cue, no signal.
        for q in [
            "open safari",
            "what time is it",
            "what's the weather",
            "tell me a joke",
            "how is the weather today",
            "set a 5 minute timer",
            "who won the world cup in 2018",
            "hi darwin",
        ] {
            assert_eq!(
                classify_mode(q, &no_signal()),
                Selection::Route(Mode::OneShot),
                "should stay one_shot: {q}"
            );
        }
    }

    #[test]
    fn empty_or_blank_is_safe_one_shot() {
        assert_eq!(classify_mode("", &no_signal()), Selection::Route(Mode::OneShot));
        assert_eq!(classify_mode("   ", &no_signal()), Selection::Route(Mode::OneShot));
    }

    // ----- RAIL 1: never guess into autonomy / consequence -------------------

    #[test]
    fn semantic_lean_toward_standing_clarifies_never_silently_establishes() {
        // No hard recurring cue, but the scorer leans hard toward Standing (index
        // 3). RAIL 1: this must NOT route to Standing — it CLARIFIES instead.
        let leans_standing = MockScorer { scores: Some(vec![0.05, 0.05, 0.05, 0.95]) };
        let sel = classify_mode("look after my deadlines for me", &leans_standing);
        assert_eq!(
            sel,
            Selection::Clarify(recurring_clarify()),
            "a mere semantic lean toward autonomy must clarify, never silently establish"
        );
        assert!(sel.mode().is_none(), "a clarify routes to no mode");
        // And the clarify question offers the once-vs-recurring choice.
        if let Selection::Clarify(q) = sel {
            assert!(q.to_lowercase().contains("once"), "clarify must offer the once option: {q}");
            assert!(
                q.to_lowercase().contains("every day") || q.to_lowercase().contains("from now on"),
                "clarify must offer the recurring option: {q}"
            );
        }
    }

    #[test]
    fn one_shot_override_vetoes_a_recurring_cue() {
        // An explicit one-shot scope must override a recurring word — the user
        // scoped it to a single run, so no autonomy is armed.
        assert_eq!(
            classify_mode("every day — well, just once for now — brief me", &no_signal()),
            Selection::Route(Mode::OneShot),
            "an explicit 'just once' must veto the recurring cue"
        );
        assert_eq!(
            classify_mode("brief me on deadlines, only once", &no_signal()),
            Selection::Route(Mode::OneShot)
        );
    }

    #[test]
    fn ambiguous_near_tie_stays_one_shot_never_a_mode_or_consequence() {
        // A near-tie between two modes is ambiguous -> safe one-shot, never a
        // confident mode pick and certainly never a consequential one.
        let near_tie = MockScorer { scores: Some(vec![0.50, 0.49, 0.10, 0.10]) };
        assert_eq!(
            classify_mode("do the thing with the stuff", &near_tie),
            Selection::Route(Mode::OneShot),
            "a near-tie must stay one_shot"
        );
        // A near-tie that INCLUDES standing still never establishes autonomy.
        let near_tie_standing = MockScorer { scores: Some(vec![0.10, 0.10, 0.50, 0.49]) };
        let sel = classify_mode("handle this somehow", &near_tie_standing);
        assert_eq!(sel, Selection::Route(Mode::OneShot));
        assert_ne!(sel.mode(), Some(Mode::Standing));
    }

    #[test]
    fn below_floor_signal_stays_one_shot() {
        // Best score under the floor is noise -> one-shot.
        let weak = MockScorer { scores: Some(vec![0.20, 0.05, 0.05, 0.05]) };
        assert_eq!(
            classify_mode("something vague", &weak),
            Selection::Route(Mode::OneShot)
        );
    }

    #[test]
    fn no_signal_scorer_always_stays_safe() {
        // The honest "embedder down" all-zero ranking: every non-cued turn stays
        // one-shot. Hard-cued turns still route (cues run before the scorer).
        for q in ["look after things", "deal with my stuff", "what about the thing"] {
            assert_eq!(
                classify_mode(q, &NoSignalScorer),
                Selection::Route(Mode::OneShot),
                "no signal must stay safe: {q}"
            );
        }
        // A hard recurring cue still routes to standing even under no semantic signal.
        assert_eq!(
            classify_mode("every morning brief me", &NoSignalScorer),
            Selection::Route(Mode::Standing)
        );
    }

    #[test]
    fn wrong_length_score_vector_is_no_signal() {
        // A scorer returning a wrong-length vector (backend can't rank) -> safe.
        assert_eq!(
            classify_mode("look after my deadlines", &no_signal()),
            Selection::Route(Mode::OneShot)
        );
    }

    #[test]
    fn semantic_can_promote_a_nonconsequential_mode_on_a_strong_clear_signal() {
        // A strong, clear, NON-consequential winner (WorldQuery, idx 0) is allowed
        // to promote — the selector is smart, not just safe.
        let leans_query = MockScorer { scores: Some(vec![0.95, 0.10, 0.10, 0.10]) };
        assert_eq!(
            classify_mode("how are things looking for me", &leans_query),
            Selection::Route(Mode::WorldQuery)
        );
    }

    #[test]
    fn semantic_lean_toward_mission_never_spins_up_fury_stays_one_shot() {
        // Mission (idx 2) winning the semantic ranking must NOT fire FURY — a heavy
        // multi-step engine requires a deterministic cue, never a keyword-overlap
        // guess. Without a MISSION cue, a Mission-leaning score stays one_shot.
        let leans_mission = MockScorer { scores: Some(vec![0.10, 0.10, 0.95, 0.10]) };
        assert_eq!(
            classify_mode("set a timer for ten minutes", &leans_mission),
            Selection::Route(Mode::OneShot),
            "a semantic Mission lean must never spin up FURY"
        );
        // But an explicit MISSION cue still routes to mission (cues run first).
        assert_eq!(
            classify_mode("plan and kick off the migration", &leans_mission),
            Selection::Route(Mode::Mission)
        );
    }

    #[test]
    fn mode_consequence_flag_only_standing() {
        assert!(Mode::Standing.is_consequential());
        for m in [Mode::OneShot, Mode::WorldQuery, Mode::WorldUpdate, Mode::Mission] {
            assert!(!m.is_consequential(), "{:?} must not be consequential", m);
        }
    }

    #[test]
    fn explicit_user_phrasing_is_honored_by_cues() {
        // The user being explicit overrides any semantic ambiguity: an explicit
        // recurring phrase routes to standing; an explicit world read routes to
        // world_query — regardless of a (here absent) scorer.
        assert_eq!(
            classify_mode("from now on keep me posted on the launch", &no_signal()),
            Selection::Route(Mode::Standing)
        );
        assert_eq!(
            classify_mode("what's the status of the launch", &no_signal()),
            Selection::Route(Mode::WorldQuery)
        );
    }

    #[test]
    fn mode_as_str_is_stable() {
        assert_eq!(Mode::OneShot.as_str(), "one_shot");
        assert_eq!(Mode::WorldQuery.as_str(), "world_query");
        assert_eq!(Mode::WorldUpdate.as_str(), "world_update");
        assert_eq!(Mode::Mission.as_str(), "mission");
        assert_eq!(Mode::Standing.as_str(), "standing");
    }
}
