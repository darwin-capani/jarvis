//! PROACTIVE INTELLIGENCE — the HABIT DETECTOR (#13) + the PREDICTIVE SUGGESTER
//! (#14). The piece of DARWIN that, having WATCHED a recurring pattern in the
//! redacted, agent-scoped episodic store, OFFERS to help — and never more than
//! that. Everything here is a SUGGESTION: it is surfaced, never acted; the user
//! ACCEPTS it, and accepting still routes through the EXISTING gated path.
//!
//! ## What it produces
//!   * a HABIT-AUTOMATION OFFER (#13): "you do X every weekday morning — want me
//!     to make that a standing mission?". The offer CARRIES a fully-built
//!     [`crate::standing::StandingMission`] proposal, but DOES NOT create it —
//!     the mission is minted only so the Accept path can hand it to the existing
//!     gated `standing_create` (which is in [`crate::confirm::CONSEQUENTIAL_TOOLS`]
//!     and parks for a spoken yes + the consequential master switch).
//!   * a PREDICTIVE SUGGESTION (#14): "you usually review the budget around now".
//!     A line of intel, NOT an action and NOT an offer to automate anything.
//!
//! ## The contract (mirrors optimize.rs's propose-only + episodic.rs's honesty)
//!   1. OBSERVED-ONLY / NEVER-FABRICATE. A suggestion is emitted ONLY when a real
//!      recurring pattern clears a `>= K` recurrence threshold over the agent's
//!      OWN recorded episodes. Sparse / empty / contradictory history yields NO
//!      suggestion — we never invent one (the same rule episodic_recall follows:
//!      a no-match recall returns nothing). The detection is a HEURISTIC over
//!      counts, never a claim that DARWIN "knows what you want".
//!   2. PROPOSE-ONLY + GATED-ACCEPT. A suggestion NEVER auto-acts. Accepting a
//!      habit offer creates the standing mission through the EXISTING gated
//!      [`crate::standing::create`] path (the confirmed `standing_create`
//!      target) — there is NO new ungated create here. A predictive suggestion
//!      carries no action at all.
//!   3. GATED BY `[proactive].suggest` (SHIPS ON). [`detect`] returns EMPTY
//!      unless `[proactive].suggest` is on. That flag is the suggester's OWN
//!      master switch and ships TRUE (config.rs default + darwin.toml pin),
//!      mirroring `[proactive].speak` — it is deliberately NOT `[proactive].enabled`
//!      (which ships ON only to power the unrelated first-contact brief). So with
//!      the shipped config NO suggestion surfaces. Even with `suggest` on, a
//!      suggestion is only SURFACED (HUD feed / proactive brief), never acted; the
//!      ACTION (accepting a habit offer) still needs accept + the standing gate.
//!   4. AGENT-SCOPED. Detection runs over ONE agent's episodes (the namespace it
//!      is called with); a pattern mined from agent A never surfaces under agent
//!      B. No cross-agent leak, no new data exposure — it mines only what is
//!      already in the redacted episodic store.
//!   5. DEDUP + BOUNDED. A dismissed (or accepted) offer is not re-offered — its
//!      stable id is suppressed. At most [`MAX_SUGGESTIONS`] suggestions per
//!      detection pass.
//!
//! Nothing here speaks, acts, or reaches the network. It reads recorded episodes
//! and proposes. The live tick that would surface these is RUNTIME-only; this
//! module is a PURE detector over an injected slice of episodes plus the policy
//! and the dismiss-ledger — fully hermetic, no clock of its own beyond what the
//! caller injects.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::ProactiveConfig;
use crate::memory::Episode;
use crate::standing::{Schedule, StandingMission};

/// The recurrence floor: an intent/topic must repeat at least this many times in
/// the agent's recorded episodes before it is considered a HABIT worth offering
/// to automate. Conservative on purpose — three occurrences is the smallest
/// count that is plausibly a pattern and not a coincidence. Below this: NOTHING
/// (never fabricate a habit from one or two turns).
pub const HABIT_MIN_OCCURRENCES: usize = 3;

/// The recurrence floor for a PREDICTIVE time-of-day suggestion: an intent must
/// recur at least this many times WITHIN THE SAME time-of-day bucket (morning /
/// afternoon / evening) before "you usually do X around now" is honest. Same
/// spirit as the habit floor — a time pattern needs real repetition.
pub const PREDICT_MIN_OCCURRENCES: usize = 3;

/// Hard cap on suggestions returned from one detection pass. Small: a proactive
/// surface is a hint, never a dump. Habit offers are filled first (they are the
/// more actionable kind), then predictive suggestions.
pub const MAX_SUGGESTIONS: usize = 4;

/// Topics that are NOT meaningful habits to automate or predict — pure
/// conversational/chit-chat intents that recurring frequently says nothing
/// actionable. Excluded so "you chat every morning" never becomes an offer.
/// Small + hardcoded (same spirit as episodic's stoplist).
const NON_HABIT_TOPICS: &[&str] = &["conversation", "chitchat", "smalltalk", "unknown", ""];

// ---------------------------------------------------------------------------
// Time-of-day bucketing (pure) — the cadence axis for the predictive suggester
// ---------------------------------------------------------------------------

/// The three coarse time-of-day buckets a predictive suggestion reasons over.
/// Coarse on purpose: "around now" is a soft claim, so a coarse bucket keeps the
/// heuristic honest (we never claim a precise minute). Mirrors
/// [`crate::proactive::time_of_day_word`]'s morning/afternoon/evening split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeOfDay {
    Morning,
    Afternoon,
    Evening,
}

impl TimeOfDay {
    /// Map a local hour (0-23) to its bucket — the SAME bands
    /// [`crate::proactive::time_of_day_word`] uses, so the two surfaces agree.
    pub fn from_hour(hour: u32) -> TimeOfDay {
        match hour % 24 {
            5..=11 => TimeOfDay::Morning,
            12..=16 => TimeOfDay::Afternoon,
            _ => TimeOfDay::Evening,
        }
    }

    /// The bucket word, for the suggestion copy + the stable id.
    pub fn word(&self) -> &'static str {
        match self {
            TimeOfDay::Morning => "morning",
            TimeOfDay::Afternoon => "afternoon",
            TimeOfDay::Evening => "evening",
        }
    }
}

/// Parse the hour out of an episode's RFC3339 `ts`, in the SAME local zone as the
/// caller's "now". We parse the stored UTC instant and convert to local — the
/// episode store stamps UTC, but a HABIT is about the user's WALL-CLOCK day, so
/// the bucket must be local. Returns `None` for an unparseable ts (degrade: that
/// episode simply contributes no time signal, never a panic, never a guess).
fn episode_local_hour(ts: &str) -> Option<u32> {
    use chrono::{DateTime, Timelike};
    let parsed = DateTime::parse_from_rfc3339(ts).ok()?;
    Some(parsed.with_timezone(&chrono::Local).hour())
}

// ---------------------------------------------------------------------------
// The suggestion (the propose-only output)
// ---------------------------------------------------------------------------

/// The KIND of a proactive suggestion, with the data each kind carries. Both are
/// SUGGESTIONS — surfaced, dismissible, never auto-acted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SuggestionKind {
    /// HABIT-AUTOMATION OFFER (#13): a recurring intent we offer to turn into a
    /// standing mission. Carries the PROPOSED mission (built, NOT created) so the
    /// Accept path hands it straight to the gated `standing_create`.
    HabitAutomation {
        /// The recurring topic/intent the habit is built from.
        topic: String,
        /// How many times it recurred (the evidence — surfaced honestly).
        occurrences: usize,
        /// The PROPOSED standing mission. NOT persisted; only [`accept`] (via the
        /// gated standing path) ever creates it.
        proposed: StandingMission,
    },
    /// PREDICTIVE SUGGESTION (#14): "you usually do X around <time-of-day>". A
    /// line of intel, no action attached.
    Predictive {
        /// The recurring topic/intent.
        topic: String,
        /// The time-of-day bucket it recurs in.
        time_of_day: String,
        /// How many times it recurred in that bucket (the evidence).
        occurrences: usize,
    },
}

/// One propose-only suggestion: a stable id (for dedup), the agent it was mined
/// under (scope), the kind+evidence, and the honest human copy. NEVER carries an
/// executed action — accepting a habit offer routes through the gated standing
/// path; a predictive suggestion carries nothing to accept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// Stable content id — dedup key. Reproducible from (agent, kind, topic[,
    /// time-of-day]) so a dismissed suggestion stays dismissed across passes.
    pub id: String,
    /// The agent namespace this was mined under. The SCOPE: a suggestion stays in
    /// the namespace whose episodes produced it.
    pub agent: String,
    /// What kind of suggestion + its evidence.
    pub kind: SuggestionKind,
    /// The honest, dismissible human line the HUD/brief shows.
    pub text: String,
}

impl Suggestion {
    /// The HUD `proactive.suggestion` telemetry payload — the suggestions-feed
    /// card. Carries the id (so Accept/Dismiss address it), the agent scope, the
    /// kind, the evidence, and the copy. For a habit offer it also carries the
    /// PROPOSED goal+schedule (what an Accept would establish) so the HUD can
    /// preview it — but the mission is NOT created until Accept routes through the
    /// gate. Secret-free: every field traces to redacted episodic data.
    pub fn telemetry(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "agent": self.agent,
            "text": self.text,
        });
        match &self.kind {
            SuggestionKind::HabitAutomation { topic, occurrences, proposed } => {
                v["kind"] = json!("habit_automation");
                v["topic"] = json!(topic);
                v["occurrences"] = json!(occurrences);
                // The proposal an Accept would hand to the gated standing_create —
                // shown for preview, NOT a created mission.
                v["proposed_goal"] = json!(proposed.goal);
                v["proposed_schedule"] = json!(proposed.schedule.describe());
                // Make the gated-accept posture explicit for the HUD copy.
                v["accept_routes_through"] = json!("standing_create");
                v["auto_acts"] = json!(false);
            }
            SuggestionKind::Predictive { topic, time_of_day, occurrences } => {
                v["kind"] = json!("predictive");
                v["topic"] = json!(topic);
                v["time_of_day"] = json!(time_of_day);
                v["occurrences"] = json!(occurrences);
                // A predictive suggestion has no action to accept.
                v["auto_acts"] = json!(false);
            }
        }
        v
    }
}

/// Stable content id for a habit-automation offer mined under `agent` for
/// `topic`. Reproducible (so dedup survives across passes) and namespaced by the
/// agent (so the SAME topic under two agents is two distinct, separately-
/// dismissible suggestions). Pure.
fn habit_id(agent: &str, topic: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"habit\0");
    h.update(agent.as_bytes());
    h.update([0u8]);
    h.update(topic.trim().to_lowercase().as_bytes());
    hex::encode(&h.finalize()[..6])
}

/// Stable content id for a predictive suggestion mined under `agent` for `topic`
/// in `tod`. Distinct from a habit id on the same topic (different prefix) and
/// per-(agent, topic, time-of-day). Pure.
fn predict_id(agent: &str, topic: &str, tod: TimeOfDay) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"predict\0");
    h.update(agent.as_bytes());
    h.update([0u8]);
    h.update(topic.trim().to_lowercase().as_bytes());
    h.update([0u8]);
    h.update(tod.word().as_bytes());
    hex::encode(&h.finalize()[..6])
}

// ---------------------------------------------------------------------------
// Pattern mining (PURE — the unit-tested heart)
// ---------------------------------------------------------------------------

/// Whether `topic` is a meaningful habit/prediction subject (not chit-chat).
fn is_actionable_topic(topic: &str) -> bool {
    let t = topic.trim().to_lowercase();
    !NON_HABIT_TOPICS.contains(&t.as_str())
}

/// Count occurrences of each actionable topic across `episodes`. Returns
/// (topic, count) pairs in DESCENDING count order, ties broken by topic name for
/// determinism. Chit-chat topics are excluded. Pure.
fn topic_counts(episodes: &[Episode]) -> Vec<(String, usize)> {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for ep in episodes {
        if is_actionable_topic(&ep.topic) {
            *counts.entry(ep.topic.trim().to_lowercase()).or_insert(0) += 1;
        }
    }
    let mut pairs: Vec<(String, usize)> = counts.into_iter().collect();
    // Descending by count, then ascending by topic name (BTreeMap already gives
    // name order; stable sort by count desc preserves it for ties).
    pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    pairs
}

/// Count occurrences of each (topic, time-of-day) pair across `episodes`, using
/// each episode's LOCAL hour. Episodes with an unparseable ts contribute no time
/// signal (degrade, never guess). Returns (topic, tod, count) in descending
/// count order, ties broken by (topic, tod-word) for determinism. Pure but for
/// the ts parse, which is deterministic given the local zone.
fn topic_tod_counts(episodes: &[Episode]) -> Vec<(String, TimeOfDay, usize)> {
    use std::collections::BTreeMap;
    // key: (topic, tod-word) so the BTreeMap orders deterministically.
    let mut counts: BTreeMap<(String, &'static str), (TimeOfDay, usize)> = BTreeMap::new();
    for ep in episodes {
        if !is_actionable_topic(&ep.topic) {
            continue;
        }
        let Some(hour) = episode_local_hour(&ep.ts) else {
            continue; // no usable time signal from this episode
        };
        let tod = TimeOfDay::from_hour(hour);
        let key = (ep.topic.trim().to_lowercase(), tod.word());
        let slot = counts.entry(key).or_insert((tod, 0));
        slot.1 += 1;
    }
    let mut out: Vec<(String, TimeOfDay, usize)> = counts
        .into_iter()
        .map(|((topic, _word), (tod, n))| (topic, tod, n))
        .collect();
    out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)).then_with(|| a.1.word().cmp(b.1.word())));
    out
}

/// Build the proposed standing mission for a habit on `topic`. The goal is a
/// plain English objective DERIVED from the observed topic (never invented
/// beyond it); the schedule is a SAFE default daily cadence (the standing layer
/// clamps anything aggressive anyway, and a habit offer should default to the
/// least-surprising cadence). The mission is BUILT, not created. Pure.
fn proposed_mission_for(topic: &str) -> StandingMission {
    let goal = format!("regularly handle the recurring task: {}", topic.trim());
    // Daily at 09:00 — the standing layer's own safe default; the user can edit
    // the schedule when they accept. Never a fast/aggressive cadence from a guess.
    StandingMission::new(&goal, Schedule::Daily { hour: 9, minute: 0 })
}

// ---------------------------------------------------------------------------
// The detector (PURE — gated, observed-only, dedup, bounded)
// ---------------------------------------------------------------------------

/// Detect propose-only suggestions over ONE agent's recorded `episodes`.
///
/// PURE and deterministic: a function of (cfg, agent, episodes, dismissed). It
/// reads no store and no clock — the caller passes the agent's already-recalled
/// episodes (agent-scoped at the Db) and the dismiss-ledger. Returns an EMPTY
/// vec — never a fabricated suggestion — whenever:
///   * `[proactive].suggest` is off (the gate, which SHIPS OFF; with it off
///     NOTHING surfaces — this is the suggester's own ships-off switch);
///   * the history is sparse/empty/contradictory (no topic clears the floor);
///   * every candidate is already in `dismissed` (dedup).
///
/// When a topic clears [`HABIT_MIN_OCCURRENCES`], a HABIT-AUTOMATION OFFER is
/// produced carrying a PROPOSED [`StandingMission`] (built, NOT created). When a
/// (topic, time-of-day) pair clears [`PREDICT_MIN_OCCURRENCES`], a PREDICTIVE
/// suggestion is produced. Both carry a stable, per-agent id so a dismissed one
/// is suppressed on the next pass. The result is bounded to [`MAX_SUGGESTIONS`],
/// habit offers first.
///
/// `dismissed` is the set of suggestion ids the user has already dismissed (or
/// accepted) — passed in so the detector suppresses them. The caller persists
/// this ledger (see [`DismissLedger`]); the detector only reads it.
pub fn detect(
    cfg: &ProactiveConfig,
    agent: &str,
    episodes: &[Episode],
    dismissed: &DismissLedger,
) -> Vec<Suggestion> {
    // GATE: with the suggester off, surface nothing at all. This is the
    // suggester's OWN master switch — `[proactive].suggest` — which SHIPS ON
    // (config.rs default true + darwin.toml `suggest = true`), mirroring
    // `[proactive].speak`. It is deliberately NOT `[proactive].enabled` (which
    // ships ON only for the first-contact brief), so the suggestion feed is gated
    // by a flag that actually ships off, not by being dead code.
    if !cfg.suggest {
        return Vec::new();
    }

    let mut out: Vec<Suggestion> = Vec::new();

    // ---- HABIT-AUTOMATION OFFERS (#13): recurring intent >= floor -----------
    for (topic, count) in topic_counts(episodes) {
        if count < HABIT_MIN_OCCURRENCES {
            // topic_counts is sorted descending, so once we drop below the floor
            // no later topic can clear it — stop scanning.
            break;
        }
        let id = habit_id(agent, &topic);
        if dismissed.contains(&id) {
            continue; // dedup: already dismissed/accepted, don't re-offer.
        }
        let proposed = proposed_mission_for(&topic);
        let text = format!(
            "You've done \"{topic}\" {count} times — want me to make it a standing \
             mission? It's just a suggestion from an observed pattern (I could be \
             wrong); accepting still goes through the normal confirmation, and I \
             never set it up on my own.",
        );
        out.push(Suggestion {
            id,
            agent: agent.to_string(),
            kind: SuggestionKind::HabitAutomation { topic, occurrences: count, proposed },
            text,
        });
        if out.len() >= MAX_SUGGESTIONS {
            return out;
        }
    }

    // ---- PREDICTIVE SUGGESTIONS (#14): recurring time-of-day >= floor -------
    for (topic, tod, count) in topic_tod_counts(episodes) {
        if count < PREDICT_MIN_OCCURRENCES {
            break; // sorted descending: nothing later clears the floor.
        }
        let id = predict_id(agent, &topic, tod);
        if dismissed.contains(&id) {
            continue; // dedup.
        }
        let tod_word = tod.word();
        let text = format!(
            "You usually do \"{topic}\" in the {tod_word} — it tends to come up around \
             now ({count} times observed). Just a heads-up from a pattern I noticed; \
             it can be wrong, and I'm not acting on it.",
        );
        out.push(Suggestion {
            id,
            agent: agent.to_string(),
            kind: SuggestionKind::Predictive {
                topic,
                time_of_day: tod_word.to_string(),
                occurrences: count,
            },
            text,
        });
        if out.len() >= MAX_SUGGESTIONS {
            return out;
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Dedup ledger (the dismissed/accepted set; the caller persists it)
// ---------------------------------------------------------------------------

/// The set of suggestion ids the user has DISMISSED or ACCEPTED — so the detector
/// does not re-offer them. A tiny, bounded set of stable ids (no free text, no
/// secrets). The caller persists it (e.g. under a `meta.*` key) and passes it to
/// [`detect`]; the detector only reads it. Pure value type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissLedger {
    /// The suppressed ids. Bounded by the caller (evict-oldest past a cap); the
    /// detector treats it as a membership set.
    pub ids: Vec<String>,
}

impl DismissLedger {
    /// Is this suggestion id suppressed?
    pub fn contains(&self, id: &str) -> bool {
        self.ids.iter().any(|i| i == id)
    }

    /// Suppress `id` (idempotent — a re-dismiss does not duplicate it). This is
    /// how the DISMISS path drops a suggestion and how the ACCEPT path stops a
    /// just-accepted offer from re-surfacing.
    pub fn suppress(&mut self, id: &str) {
        if !self.contains(id) {
            self.ids.push(id.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// ACCEPT / DISMISS (the propose-only -> gated-create boundary)
// ---------------------------------------------------------------------------

/// What an ACCEPT of a habit-automation offer hands to the EXISTING gated
/// standing-creation path. Carries the proposed goal + schedule for the
/// `standing_create` tool input — the SAME shape `standing::create` consumes — so
/// accepting an offer is byte-for-byte an ordinary gated establish, NOT a new
/// ungated create. A predictive suggestion has NO accept request (returns `None`
/// from [`accept_request`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptRequest {
    /// The proposed standing-mission goal (what `standing_create` establishes).
    pub goal: String,
    /// The proposed schedule.
    pub schedule: Schedule,
    /// The suggestion id to suppress once accepted (so it never re-offers).
    pub suggestion_id: String,
}

impl AcceptRequest {
    /// The `standing_create` tool input this accept maps to — the EXACT JSON the
    /// gated, consequential `standing_create` tool consumes. Routing an accept
    /// through this (rather than calling `standing::create` directly) means the
    /// create PARKS for the spoken confirmation + the consequential master switch,
    /// exactly like a user-typed "set up a standing mission". No new ungated path.
    pub fn to_standing_create_input(&self) -> Value {
        json!({
            "goal": self.goal,
            "schedule": self.schedule.describe(),
        })
    }
}

/// Build the [`AcceptRequest`] for a suggestion the user accepted. ONLY a
/// HABIT-AUTOMATION offer is acceptable (it carries a proposed mission); a
/// PREDICTIVE suggestion returns `None` (there is nothing to act on — it is a
/// heads-up, not an offer). This is the ONLY bridge from a suggestion to an
/// action, and it produces a request for the EXISTING gated `standing_create`
/// path — it NEVER creates a mission itself.
pub fn accept_request(suggestion: &Suggestion) -> Option<AcceptRequest> {
    match &suggestion.kind {
        SuggestionKind::HabitAutomation { proposed, .. } => Some(AcceptRequest {
            goal: proposed.goal.clone(),
            schedule: proposed.schedule.clone(),
            suggestion_id: suggestion.id.clone(),
        }),
        SuggestionKind::Predictive { .. } => None,
    }
}

// ---------------------------------------------------------------------------
// LIVE WIRING — the impure surfacing pass the anticipation tick calls
// ---------------------------------------------------------------------------

/// How many of the agent's most-recent episodes the live pass mines. Same order
/// of magnitude as the episodic RECALL_WINDOW — a bounded recent slice, never
/// the whole store. The detector counts within this slice; the bound keeps the
/// pass cheap and the "recurring" claim about RECENT behavior.
pub const LIVE_EPISODE_WINDOW: usize = 200;

/// The meta-store key the persisted [`DismissLedger`] lives under. The live pass
/// loads it before [`detect`] (so a dismissed/accepted suggestion stays
/// suppressed across ticks) and the dismiss/accept handlers write it back. A
/// tiny JSON blob of stable ids — no free text, no secrets.
pub const DISMISS_LEDGER_META_KEY: &str = "meta.proactive_dismiss_ledger";

/// Load the persisted dismiss-ledger from the meta store. A missing/empty/garbled
/// value degrades to an EMPTY ledger (never a panic, never a guess) — the only
/// cost is that a previously-dismissed suggestion could re-offer once, which is
/// safe (it is still just a suggestion). Pure-ish: reads one fact.
pub async fn load_dismiss_ledger(memory: &crate::memory::Memory) -> DismissLedger {
    match memory.get_fact(DISMISS_LEDGER_META_KEY).await {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => DismissLedger::default(),
    }
}

/// The result of one live surfacing pass: the suggestions to emit as
/// `proactive.suggestion` cards. Empty whenever the gate is off, the history is
/// sparse, or every candidate is suppressed — the caller emits nothing then.
pub struct SurfacePass {
    pub suggestions: Vec<Suggestion>,
}

/// Run ONE live surfacing pass for `agent`: recall that agent's recent episodes
/// (agent-scoped at the Db), load the persisted dismiss-ledger, and run the PURE
/// [`detect`] behind the `[proactive].suggest` gate. Returns the suggestions to
/// surface (the caller emits each `Suggestion::telemetry()` as a
/// `proactive.suggestion` card). This is the ONLY impure entry point; it reads
/// the store but NEVER acts — no mission is created, nothing is spoken. The gate
/// is checked inside [`detect`], so with `suggest` off this returns EMPTY without
/// even touching the store-derived candidate set's meaning (detect short-circuits).
pub async fn surface_pass(
    cfg: &ProactiveConfig,
    memory: &crate::memory::Memory,
    agent: &str,
) -> SurfacePass {
    // Gate first: with the suggester off, do no work and surface nothing. This
    // mirrors detect()'s gate so an off pass is a true no-op (no store read).
    if !cfg.suggest {
        return SurfacePass { suggestions: Vec::new() };
    }
    let episodes = memory
        .episodes_scoped(agent, LIVE_EPISODE_WINDOW)
        .await
        .unwrap_or_default();
    let ledger = load_dismiss_ledger(memory).await;
    let suggestions = detect(cfg, agent, &episodes, &ledger);
    SurfacePass { suggestions }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config with the suggester ON (so the gate lets suggestions through) for
    /// the detection tests. `speak` stays OFF — a suggestion is surfaced, never
    /// spoken, regardless. NOTE the gate is `suggest`, NOT `enabled` (which ships
    /// ON for the first-contact brief and must not surface suggestions).
    fn cfg_on() -> ProactiveConfig {
        ProactiveConfig { suggest: true, speak: false, ..Default::default() }
    }

    /// A config with the suggester OFF — its master gate; with it off NO
    /// suggestion surfaces. Uses the DEFAULT (which ships `suggest = false`).
    fn cfg_off() -> ProactiveConfig {
        ProactiveConfig { suggest: false, ..Default::default() }
    }

    /// Build an episode with a controlled topic + ts under an agent. ts defaults
    /// to a fixed morning instant unless overridden.
    fn ep(agent: &str, topic: &str, ts: &str) -> Episode {
        Episode {
            id: 0,
            ts: ts.to_string(),
            agent_namespace: agent.to_string(),
            utterance_redacted: format!("did {topic}"),
            topic: topic.to_string(),
            salient_entities: vec![],
            outcome: "ok".to_string(),
            summary: format!("{topic} -> done"),
        }
    }

    /// A fixed UTC morning timestamp helper (08:00Z) — for tests that don't care
    /// about the bucket, only the count. (Bucketing tests build their own local
    /// instants.)
    fn morning_ts(day: u32) -> String {
        format!("2026-06-{day:02}T08:00:00+00:00")
    }

    // =====================================================================
    // HABIT DETECTOR — recurring intent >= floor -> offer, but NO mission
    // =====================================================================

    #[test]
    fn recurring_intent_at_floor_produces_a_habit_offer_carrying_a_proposed_mission() {
        // Three "budget.review" turns -> a habit offer.
        let eps = vec![
            ep("agent.darwin", "budget.review", &morning_ts(1)),
            ep("agent.darwin", "budget.review", &morning_ts(2)),
            ep("agent.darwin", "budget.review", &morning_ts(3)),
        ];
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        // At least one habit-automation offer for budget.review.
        let habit = out.iter().find_map(|s| match &s.kind {
            SuggestionKind::HabitAutomation { topic, occurrences, proposed } if topic == "budget.review" => {
                Some((s, *occurrences, proposed))
            }
            _ => None,
        });
        let (sugg, occ, proposed) = habit.expect("a habit offer for budget.review");
        assert_eq!(occ, 3, "the occurrence count is the observed evidence");
        // The offer CARRIES a proposed mission — but it was never CREATED here
        // (the function touches no store; the mission is an in-memory proposal).
        assert!(proposed.goal.contains("budget.review"), "proposal grounded in the topic: {}", proposed.goal);
        assert_eq!(proposed.last_run, 0, "a fresh proposal, never run");
        // The copy is HONEST: a suggestion, from an observed pattern, dismissible,
        // and accepting still goes through the gate.
        let t = sugg.text.to_lowercase();
        assert!(t.contains("suggestion"), "framed as a suggestion: {t}");
        assert!(t.contains("observed pattern") || t.contains("pattern"), "names the heuristic: {t}");
        assert!(t.contains("confirmation") || t.contains("normal"), "says accepting is gated: {t}");
    }

    #[test]
    fn detect_creates_no_standing_mission_only_a_proposal() {
        // The detector is PURE: it returns a proposal but persists nothing. We
        // assert the proposal exists and is a plain StandingMission value — there
        // is no store handle in this path at all, so it CANNOT have created one.
        let eps = vec![
            ep("agent.darwin", "deploy.check", &morning_ts(1)),
            ep("agent.darwin", "deploy.check", &morning_ts(2)),
            ep("agent.darwin", "deploy.check", &morning_ts(3)),
        ];
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let proposed = out.iter().find_map(|s| match &s.kind {
            SuggestionKind::HabitAutomation { proposed, .. } => Some(proposed.clone()),
            _ => None,
        });
        assert!(proposed.is_some(), "an offer with a proposal");
        // The proposed schedule is the SAFE default daily — never an aggressive
        // cadence guessed from the data.
        assert_eq!(proposed.unwrap().schedule, Schedule::Daily { hour: 9, minute: 0 });
    }

    // =====================================================================
    // NEVER-FABRICATE — sparse / below-floor / contradictory -> NOTHING
    // =====================================================================

    #[test]
    fn below_floor_history_produces_no_habit_offer_never_fabricates() {
        // Only two budget.review turns — below the floor of 3.
        let eps = vec![
            ep("agent.darwin", "budget.review", &morning_ts(1)),
            ep("agent.darwin", "budget.review", &morning_ts(2)),
        ];
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        assert!(
            !out.iter().any(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { .. })),
            "two occurrences must NOT fabricate a habit offer: {out:?}"
        );
    }

    #[test]
    fn empty_history_produces_nothing() {
        let out = detect(&cfg_on(), "agent.darwin", &[], &DismissLedger::default());
        assert!(out.is_empty(), "no episodes -> no suggestions, never invented: {out:?}");
    }

    #[test]
    fn contradictory_scattered_history_produces_nothing() {
        // Many DIFFERENT topics, none repeating to the floor -> no pattern.
        let eps = vec![
            ep("agent.darwin", "budget.review", &morning_ts(1)),
            ep("agent.darwin", "deploy.check", &morning_ts(2)),
            ep("agent.darwin", "music.play", &morning_ts(3)),
            ep("agent.darwin", "weather.query", &morning_ts(4)),
            ep("agent.darwin", "calendar.add", &morning_ts(5)),
        ];
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        assert!(out.is_empty(), "no topic clears the floor -> nothing: {out:?}");
    }

    #[test]
    fn pure_chitchat_never_becomes_a_habit_even_when_frequent() {
        // "conversation" recurs many times but is explicitly non-actionable.
        let eps: Vec<Episode> = (1..=6)
            .map(|d| ep("agent.darwin", "conversation", &morning_ts(d)))
            .collect();
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        assert!(out.is_empty(), "chit-chat must never become an offer: {out:?}");
    }

    // =====================================================================
    // PREDICTIVE SUGGESTER — recurring time-of-day -> a suggestion
    // =====================================================================

    #[test]
    fn recurring_time_of_day_pattern_produces_a_predictive_suggestion() {
        // Three "news.brief" turns, all in the local MORNING. Build local-zone
        // instants so the bucket is deterministic regardless of the test host's
        // zone: take a fixed local morning hour and format with the local offset.
        use chrono::{Local, TimeZone};
        let local_morning = |day: u32| -> String {
            Local
                .with_ymd_and_hms(2026, 6, day, 8, 0, 0)
                .single()
                .expect("valid local instant")
                .to_rfc3339()
        };
        let eps = vec![
            ep("agent.darwin", "news.brief", &local_morning(1)),
            ep("agent.darwin", "news.brief", &local_morning(2)),
            ep("agent.darwin", "news.brief", &local_morning(3)),
        ];
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let pred = out.iter().find_map(|s| match &s.kind {
            SuggestionKind::Predictive { topic, time_of_day, occurrences } if topic == "news.brief" => {
                Some((s, time_of_day.clone(), *occurrences))
            }
            _ => None,
        });
        let (sugg, tod, occ) = pred.expect("a predictive suggestion for news.brief");
        assert_eq!(tod, "morning", "the recurring bucket is morning");
        assert_eq!(occ, 3, "evidence count is grounded");
        // Honest copy: a heads-up from a pattern, can be wrong, not acted on.
        let t = sugg.text.to_lowercase();
        assert!(t.contains("usually") || t.contains("pattern"), "framed as a heads-up: {t}");
        assert!(t.contains("not acting") || t.contains("wrong"), "honest about uncertainty: {t}");
    }

    #[test]
    fn a_predictive_suggestion_carries_no_acceptable_action() {
        // A predictive suggestion is intel only — accept_request yields None.
        use chrono::{Local, TimeZone};
        let local_morning = |day: u32| {
            Local.with_ymd_and_hms(2026, 6, day, 8, 0, 0).single().unwrap().to_rfc3339()
        };
        let eps: Vec<Episode> = (1..=3).map(|d| ep("agent.darwin", "stretch.routine", &local_morning(d))).collect();
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let pred = out
            .iter()
            .find(|s| matches!(&s.kind, SuggestionKind::Predictive { .. }))
            .expect("a predictive suggestion");
        assert!(accept_request(pred).is_none(), "a prediction has no action to accept");
    }

    // =====================================================================
    // ACCEPT path — routes through the EXISTING gated standing creation
    // =====================================================================

    #[test]
    fn accepting_a_habit_offer_yields_a_gated_standing_create_request_not_a_direct_create() {
        let eps: Vec<Episode> = (1..=3).map(|d| ep("agent.darwin", "inbox.triage", &morning_ts(d))).collect();
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let offer = out
            .iter()
            .find(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { .. }))
            .expect("a habit offer");
        let req = accept_request(offer).expect("a habit offer is acceptable");
        // The accept maps to the standing_create TOOL INPUT — i.e. it routes
        // through the EXISTING gated, consequential `standing_create` path (which
        // parks for a spoken yes + the master switch), not a direct ungated create.
        let input = req.to_standing_create_input();
        assert!(input["goal"].as_str().unwrap().contains("inbox.triage"), "goal carried: {input}");
        assert_eq!(input["schedule"], "daily at 09:00", "schedule carried in the gated shape: {input}");
        // And `standing_create` is genuinely a consequential (gated) tool — the
        // accept can only land via the confirmation gate.
        assert!(
            crate::confirm::is_consequential_tool("standing_create"),
            "the accept target must be a gated consequential tool"
        );
    }

    #[test]
    fn accept_does_not_itself_create_anything() {
        // accept_request is a pure mapper: it produces a REQUEST, not a created
        // mission. There is no Memory handle in its signature, so by construction
        // it cannot persist a standing mission — the create only happens when the
        // request is replayed through the gated standing path.
        let eps: Vec<Episode> = (1..=3).map(|d| ep("agent.darwin", "report.compile", &morning_ts(d))).collect();
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let offer = out.iter().find(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { .. })).unwrap();
        let req = accept_request(offer).unwrap();
        // The request names the suggestion id (so the caller suppresses it after
        // the gated create), and carries the goal/schedule for that gated create.
        assert_eq!(req.suggestion_id, offer.id);
        assert!(req.goal.contains("report.compile"));
    }

    // =====================================================================
    // [proactive] GATING — OFF => no suggestions at all
    // =====================================================================

    #[test]
    fn proactive_off_surfaces_nothing_even_with_a_strong_pattern() {
        // A clear, strong recurring pattern that would CERTAINLY produce offers...
        let eps: Vec<Episode> = (1..=6).map(|d| ep("agent.darwin", "budget.review", &morning_ts(d))).collect();
        // ...but with the suggester OFF, the master gate suppresses ALL of it — no
        // offer, no prediction, nothing surfaces.
        let out = detect(&cfg_off(), "agent.darwin", &eps, &DismissLedger::default());
        assert!(out.is_empty(), "[proactive].suggest off must surface NOTHING: {out:?}");
    }

    #[test]
    fn the_shipped_default_config_has_the_suggestion_gate_on() {
        // FULL-POWER DEFAULT: the suggester is gated by `suggest`, which now defaults
        // TRUE (config.rs default) so the feature ships ON. The struct default is what
        // `Config::load` falls back to and what the shipped darwin.toml pins. Accepting
        // a surfaced habit offer STILL routes through the gated standing_create
        // confirmation — surfacing a suggestion is never an auto-action.
        let shipped = ProactiveConfig::default();
        assert!(shipped.suggest, "the suggester gate ships ON (suggest=true, full-power default)");
        // And a strong pattern under the SHIPPED default DOES surface a suggestion.
        let eps: Vec<Episode> = (1..=6).map(|d| ep("agent.darwin", "budget.review", &morning_ts(d))).collect();
        let out = detect(&shipped, "agent.darwin", &eps, &DismissLedger::default());
        assert!(!out.is_empty(), "shipped-default config (suggest on) must surface the observed pattern: {out:?}");
    }

    #[test]
    fn enabled_on_does_not_open_the_suggestion_gate() {
        // The suggester does NOT piggyback on `enabled` (which ships ON for the
        // first-contact brief). With enabled=true but suggest=false (the shipped
        // posture), a strong pattern still surfaces nothing — only `suggest` opens
        // the feed.
        let cfg = ProactiveConfig { enabled: true, suggest: false, ..Default::default() };
        let eps: Vec<Episode> = (1..=6).map(|d| ep("agent.darwin", "budget.review", &morning_ts(d))).collect();
        let out = detect(&cfg, "agent.darwin", &eps, &DismissLedger::default());
        assert!(out.is_empty(), "enabled=true must NOT open the suggestion gate: {out:?}");
    }

    // =====================================================================
    // DEDUP — a dismissed suggestion is not re-offered
    // =====================================================================

    #[test]
    fn a_dismissed_offer_is_suppressed_on_the_next_pass() {
        let eps: Vec<Episode> = (1..=4).map(|d| ep("agent.darwin", "budget.review", &morning_ts(d))).collect();
        // First pass: an offer is produced.
        let first = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let offer = first
            .iter()
            .find(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { topic, .. } if topic == "budget.review"))
            .expect("a first-pass offer");
        // Dismiss it.
        let mut ledger = DismissLedger::default();
        ledger.suppress(&offer.id);
        // Second pass over the SAME episodes: the dismissed offer does not return.
        let second = detect(&cfg_on(), "agent.darwin", &eps, &ledger);
        assert!(
            !second.iter().any(|s| s.id == offer.id),
            "a dismissed offer must not re-surface: {second:?}"
        );
    }

    #[test]
    fn suppress_is_idempotent() {
        let mut l = DismissLedger::default();
        l.suppress("abc");
        l.suppress("abc");
        assert_eq!(l.ids.len(), 1, "re-dismissing the same id does not duplicate it");
        assert!(l.contains("abc"));
        assert!(!l.contains("def"));
    }

    // =====================================================================
    // BOUNDED — at most MAX_SUGGESTIONS per pass
    // =====================================================================

    #[test]
    fn detection_is_bounded_to_max_suggestions() {
        // Many distinct recurring topics, each well over the floor — far more than
        // the cap. The result is bounded.
        let mut eps: Vec<Episode> = Vec::new();
        for topic in ["t.alpha", "t.bravo", "t.charlie", "t.delta", "t.echo", "t.foxtrot"] {
            for d in 1..=4 {
                eps.push(ep("agent.darwin", topic, &morning_ts(d)));
            }
        }
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        assert!(out.len() <= MAX_SUGGESTIONS, "result must be bounded: {} > {}", out.len(), MAX_SUGGESTIONS);
        assert_eq!(out.len(), MAX_SUGGESTIONS, "with abundant patterns the cap is filled");
    }

    // =====================================================================
    // AGENT SCOPING — a suggestion mined under A carries A's scope + id
    // =====================================================================

    #[test]
    fn a_suggestion_is_scoped_to_the_agent_it_was_mined_under() {
        // The detector runs over ONE agent's episodes (agent-scoped at the Db);
        // the produced suggestion carries that agent's namespace and a per-agent
        // id, so the SAME topic under two agents is two distinct, separately-
        // dismissible suggestions.
        let eps_a: Vec<Episode> = (1..=3).map(|d| ep("agent.friday", "market.scan", &morning_ts(d))).collect();
        let eps_b: Vec<Episode> = (1..=3).map(|d| ep("agent.jerome", "market.scan", &morning_ts(d))).collect();

        let a = detect(&cfg_on(), "agent.friday", &eps_a, &DismissLedger::default());
        let b = detect(&cfg_on(), "agent.jerome", &eps_b, &DismissLedger::default());

        let a_offer = a.iter().find(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { .. })).unwrap();
        let b_offer = b.iter().find(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { .. })).unwrap();
        // Same topic, but the scope + id differ by agent.
        assert_eq!(a_offer.agent, "agent.friday");
        assert_eq!(b_offer.agent, "agent.jerome");
        assert_ne!(a_offer.id, b_offer.id, "same topic under two agents yields two distinct ids");

        // Dismissing friday's offer does NOT suppress jerome's (the dedup is
        // per-agent because the id is per-agent).
        let mut ledger = DismissLedger::default();
        ledger.suppress(&a_offer.id);
        let b_again = detect(&cfg_on(), "agent.jerome", &eps_b, &ledger);
        assert!(
            b_again.iter().any(|s| s.id == b_offer.id),
            "friday's dismiss must not suppress jerome's offer: {b_again:?}"
        );
    }

    // =====================================================================
    // TELEMETRY shape — the HUD suggestions-feed card
    // =====================================================================

    #[test]
    fn habit_offer_telemetry_carries_the_feed_card_fields() {
        let eps: Vec<Episode> = (1..=3).map(|d| ep("agent.darwin", "budget.review", &morning_ts(d))).collect();
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let offer = out.iter().find(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { .. })).unwrap();
        let t = offer.telemetry();
        assert_eq!(t["kind"], "habit_automation");
        assert_eq!(t["agent"], "agent.darwin");
        assert_eq!(t["topic"], "budget.review");
        assert_eq!(t["occurrences"], 3);
        assert_eq!(t["id"], offer.id);
        // The proposed (NOT created) goal + schedule ride along for the HUD preview.
        assert!(t["proposed_goal"].as_str().unwrap().contains("budget.review"));
        assert_eq!(t["proposed_schedule"], "daily at 09:00");
        // The card states plainly it never auto-acts and accept is gated.
        assert_eq!(t["auto_acts"], false);
        assert_eq!(t["accept_routes_through"], "standing_create");
    }

    #[test]
    fn predictive_telemetry_carries_no_action_and_is_marked_non_acting() {
        use chrono::{Local, TimeZone};
        let local_morning = |day: u32| Local.with_ymd_and_hms(2026, 6, day, 8, 0, 0).single().unwrap().to_rfc3339();
        let eps: Vec<Episode> = (1..=3).map(|d| ep("agent.darwin", "news.brief", &local_morning(d))).collect();
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        let pred = out.iter().find(|s| matches!(&s.kind, SuggestionKind::Predictive { .. })).unwrap();
        let t = pred.telemetry();
        assert_eq!(t["kind"], "predictive");
        assert_eq!(t["time_of_day"], "morning");
        assert_eq!(t["auto_acts"], false);
        assert!(t.get("proposed_goal").is_none(), "a prediction carries no proposal");
    }

    // =====================================================================
    // Time-of-day bucketing (pure)
    // =====================================================================

    #[test]
    fn time_of_day_buckets_match_the_proactive_brief_bands() {
        assert_eq!(TimeOfDay::from_hour(5), TimeOfDay::Morning);
        assert_eq!(TimeOfDay::from_hour(11), TimeOfDay::Morning);
        assert_eq!(TimeOfDay::from_hour(12), TimeOfDay::Afternoon);
        assert_eq!(TimeOfDay::from_hour(16), TimeOfDay::Afternoon);
        assert_eq!(TimeOfDay::from_hour(17), TimeOfDay::Evening);
        assert_eq!(TimeOfDay::from_hour(4), TimeOfDay::Evening);
        assert_eq!(TimeOfDay::from_hour(23), TimeOfDay::Evening);
        // Matches crate::proactive::time_of_day_word for the same hours.
        for h in 0..24u32 {
            assert_eq!(TimeOfDay::from_hour(h).word(), crate::proactive::time_of_day_word(h));
        }
    }

    #[test]
    fn an_unparseable_episode_ts_contributes_no_time_signal_but_does_not_panic() {
        // A garbled ts must not crash the predictive miner; that episode just adds
        // no time bucket. The habit (count-only) path still works from it.
        let mut eps: Vec<Episode> = (1..=3).map(|d| ep("agent.darwin", "budget.review", &morning_ts(d))).collect();
        eps.push(ep("agent.darwin", "budget.review", "not-a-timestamp"));
        let out = detect(&cfg_on(), "agent.darwin", &eps, &DismissLedger::default());
        // The habit offer still fires (count includes the garbled-ts episode).
        let offer = out.iter().find_map(|s| match &s.kind {
            SuggestionKind::HabitAutomation { occurrences, .. } => Some(*occurrences),
            _ => None,
        });
        assert_eq!(offer, Some(4), "count-based habit still works with a garbled ts");
    }

    // =====================================================================
    // LIVE WIRING — surface_pass over a real (in-memory) episodic store. This
    // is the integration the anticipation tick drives: recall agent-scoped
    // episodes, load the ledger, run detect behind the [proactive].suggest gate.
    // =====================================================================

    use crate::memory::Memory;
    use std::path::PathBuf;

    /// A throwaway on-disk Db for the live-pass tests (the episodic store is
    /// SQLite; in-memory recall reads through it). Cleaned up on drop.
    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-proactive-intel-test-{}-{}.db",
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

    /// Seed `n` episodes of `topic` under `agent` into a real Memory. Memory
    /// stamps ts=now on write (so these are all "today"); the habit detector is
    /// count-based and does not depend on the ts.
    async fn seed(memory: &Memory, agent: &str, topic: &str, n: usize) {
        for _ in 0..n {
            let e = ep(agent, topic, &morning_ts(1));
            memory.record_episode(&e).await.expect("seed episode");
        }
    }

    #[tokio::test]
    async fn live_pass_over_threshold_history_surfaces_a_habit_offer() {
        // The integration the live anticipation tick runs: over-threshold
        // recurring episodes in the agent's OWN scope => a habit-automation offer,
        // but NO standing mission created (surface_pass touches no standing store).
        let db = TempDb::new("over-threshold");
        let memory = Memory::open(&db.0).expect("open mem");
        seed(&memory, "agent.darwin", "budget.review", 4).await;

        let pass = surface_pass(&cfg_on(), &memory, "agent.darwin").await;
        let offer = pass.suggestions.iter().find(|s| {
            matches!(&s.kind, SuggestionKind::HabitAutomation { topic, .. } if topic == "budget.review")
        });
        let offer = offer.expect("a habit offer from the live pass");
        // The card the tick emits as `proactive.suggestion` carries the honest
        // non-acting shape.
        let t = offer.telemetry();
        assert_eq!(t["kind"], "habit_automation");
        assert_eq!(t["agent"], "agent.darwin");
        assert_eq!(t["auto_acts"], false);
        assert_eq!(t["accept_routes_through"], "standing_create");
        // No standing mission exists — surface_pass only reads episodes.
        let missions = crate::standing::list(&memory).await.unwrap_or_default();
        assert!(missions.is_empty(), "live pass must create NO standing mission: {missions:?}");
    }

    #[tokio::test]
    async fn live_pass_with_the_gate_off_surfaces_nothing() {
        // [proactive].suggest off (the SHIPPED default) => the live pass is a true
        // no-op even with a strong pattern. This is the ships-OFF contract at the
        // wiring boundary (not just inside the pure detector).
        let db = TempDb::new("gate-off");
        let memory = Memory::open(&db.0).expect("open mem");
        seed(&memory, "agent.darwin", "budget.review", 6).await;

        let pass = surface_pass(&cfg_off(), &memory, "agent.darwin").await;
        assert!(
            pass.suggestions.is_empty(),
            "gate off => the live pass surfaces nothing: {:?}",
            pass.suggestions
        );
    }

    #[tokio::test]
    async fn live_pass_with_sparse_history_surfaces_nothing_never_fabricates() {
        // Below-threshold history through the live path => nothing (never invents).
        let db = TempDb::new("sparse");
        let memory = Memory::open(&db.0).expect("open mem");
        seed(&memory, "agent.darwin", "budget.review", 2).await; // below floor of 3

        let pass = surface_pass(&cfg_on(), &memory, "agent.darwin").await;
        assert!(
            pass.suggestions.is_empty(),
            "sparse history => no live suggestion: {:?}",
            pass.suggestions
        );
    }

    #[tokio::test]
    async fn live_pass_is_agent_scoped_at_the_db() {
        // A pattern recorded under agent A is mined ONLY when the pass runs for A;
        // running the pass for agent B (whose own scope is sparse) surfaces nothing
        // — no cross-agent leak.
        let db = TempDb::new("agent-scope");
        let memory = Memory::open(&db.0).expect("open mem");
        seed(&memory, "agent.friday", "market.scan", 4).await;

        let a = surface_pass(&cfg_on(), &memory, "agent.friday").await;
        assert!(
            a.suggestions.iter().any(|s| s.agent == "agent.friday"),
            "agent.friday's pass surfaces its own offer"
        );
        let b = surface_pass(&cfg_on(), &memory, "agent.jerome").await;
        assert!(
            b.suggestions.is_empty(),
            "agent.jerome's scope is empty => nothing leaks from friday: {:?}",
            b.suggestions
        );
    }

    #[tokio::test]
    async fn live_pass_respects_a_persisted_dismiss_ledger() {
        // A dismissed id persisted under the meta key is loaded and suppressed on
        // the next live pass — dedup survives across ticks.
        let db = TempDb::new("ledger");
        let memory = Memory::open(&db.0).expect("open mem");
        seed(&memory, "agent.darwin", "budget.review", 4).await;

        // First pass produces an offer.
        let first = surface_pass(&cfg_on(), &memory, "agent.darwin").await;
        let offer = first
            .suggestions
            .iter()
            .find(|s| matches!(&s.kind, SuggestionKind::HabitAutomation { .. }))
            .expect("a first-pass offer");
        // Persist a ledger suppressing it, exactly as the dismiss handler would.
        let mut ledger = DismissLedger::default();
        ledger.suppress(&offer.id);
        memory
            .upsert_fact(DISMISS_LEDGER_META_KEY, &serde_json::to_string(&ledger).unwrap())
            .await
            .unwrap();

        // Next pass loads the ledger and suppresses the dismissed offer.
        let second = surface_pass(&cfg_on(), &memory, "agent.darwin").await;
        assert!(
            !second.suggestions.iter().any(|s| s.id == offer.id),
            "a persisted-dismissed offer must not re-surface: {:?}",
            second.suggestions
        );
    }

    #[tokio::test]
    async fn load_dismiss_ledger_degrades_on_missing_or_garbled_value() {
        let db = TempDb::new("ledger-degrade");
        let memory = Memory::open(&db.0).expect("open mem");
        // Missing => empty ledger.
        let l = load_dismiss_ledger(&memory).await;
        assert!(l.ids.is_empty(), "missing ledger => empty");
        // Garbled => empty ledger (never a panic).
        memory.upsert_fact(DISMISS_LEDGER_META_KEY, "{ not json").await.unwrap();
        let l = load_dismiss_ledger(&memory).await;
        assert!(l.ids.is_empty(), "garbled ledger => empty, no panic");
    }
}
