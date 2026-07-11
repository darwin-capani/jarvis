//! STANDING MISSIONS — durable, scheduled, autonomous goals that run on the
//! standing-missions scheduler tick (a dedicated runtime loop in `main.rs`,
//! distinct from EDITH's anticipation tick; ON by default via [standing].enabled)
//! and reason over the shared World Model.
//!
//! A STANDING MISSION is a saved goal plus a SCHEDULE: "every morning, review my
//! deadlines and flag anything slipping", "every 6 hours, check the world model
//! for blocked tasks". On each tick the scheduler decides which missions are DUE
//! (purely, from the clock + each mission's schedule + its last-run stamp), and a
//! due mission RUNS through FURY's bounded mission engine ([`crate::mission`]) —
//! decompose -> dispatch each sub-task to its owning specialist -> synthesize —
//! grounding itself on the shared World Model. The result surfaces to the HUD as a
//! `standing.*` telemetry card and is only SPOKEN when proactive speech is on.
//!
//! ## The two safety rails (non-negotiable)
//!
//! 1. **ESTABLISHING a standing mission is a CONFIRMED action.** Creating one is
//!    not done silently: the `standing_create` tool is in
//!    [`crate::confirm::CONSEQUENTIAL_TOOLS`], so when the master switch is on a
//!    create PARKS for a spoken human "yes" on a later turn (the cross-turn
//!    confirmation gate) instead of spawning recurring autonomy on a guess. The
//!    DRY-RUN preview names the goal + schedule precisely ("I'll set up a standing
//!    mission to <goal>, <schedule> — confirm?"). The master switch ships ON
//!    (armed), so a create PARKS for a spoken yes; with the master switch OFF
//!    (lockdown, or an operator who disarmed it) it previews and creates nothing.
//!
//! 2. **NO SILENT AUTONOMY when a mission RUNS.** A run reuses
//!    [`crate::mission::run_mission`], so every sub-task executes as its OWNING
//!    specialist under that specialist's tool allowlist, and every CONSEQUENTIAL
//!    step (post/send/spend/control) STILL routes through the SAME confirmation
//!    gate + the armed-by-default master switch (ON, but a confirmed action still
//!    needs a fresh confirm) — a standing mission can never auto-send, auto-post, or
//!    auto-spend; those steps PARK exactly as a direct request would. A run does
//!    autonomous READING/REASONING; any outward action waits for a human yes.
//!
//! The standing-missions subsystem ships ON (`[standing] enabled = true`, full-power
//! default), but establishing a mission is still confirmation-gated and every
//! consequential step a run proposes still parks. Bounded: at most [`MAX_ACTIVE`]
//! active missions, and each RUN is bounded by FURY's per-mission caps.
//!
//! ## Where it lives (persistence + isolation)
//!
//! Each mission is persisted as ONE fact row under the internal-bookkeeping
//! `meta.standing.<id>` prefix (a compact JSON record). `meta.*` is excluded from
//! every prompt feed and from `agent_scoped_facts`, so standing-mission records
//! never leak into an agent's context or the World Model — they are daemon state,
//! not user knowledge. Writes use the trusted internal `upsert_fact`/`delete_fact`
//! path (the store is a trusted subsystem, not a model-driven write).
//!
//! Everything testable here is HERMETIC: the SCHEDULER is a pure function over an
//! injected clock, the store round-trips against a temp DB, and a RUN is driven by
//! the mission engine's injected [`Planner`]/[`Dispatcher`] mocks — no live tick,
//! no real cloud, ever. The live tick that calls [`due_missions`] + runs them is
//! RUNTIME-only (wired in `main.rs`), never exercised by a test.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agents::AgentRegistry;
use crate::memory::Memory;
use crate::mission::{run_mission, Dispatcher, Planner};

/// Internal-bookkeeping key prefix for persisted standing missions. `meta.*` is
/// filtered from every prompt feed and from `agent_scoped_facts`, so a stored
/// mission never reaches an agent's context or the World Model.
pub const STANDING_PREFIX: &str = "meta.standing.";

/// Hard cap on the number of standing missions that may exist at once. A create
/// beyond this is refused (never silently dropped) so a runaway can't fill the
/// schedule. Small on purpose: standing autonomy is a deliberate, scarce thing.
pub const MAX_ACTIVE: usize = 8;

/// Max chars in a standing mission's goal text (bounded before persistence).
pub const MAX_GOAL_LEN: usize = 280;

/// The smallest interval a recurring mission may run on, in seconds. An interval
/// schedule is clamped UP to this so a mission can never be set to hammer the
/// tick (defense against a 1-second recurring autonomous run). One hour.
pub const MIN_INTERVAL_SECS: u64 = 3_600;

// ---------------------------------------------------------------------------
// Schedule (pure value type)
// ---------------------------------------------------------------------------

/// WHEN a standing mission runs. Three shapes, all evaluated purely against an
/// injected clock by [`Schedule::is_due`]:
///   - `Daily`    — once per local day, at or after `hour:minute`.
///   - `Interval` — every `secs` seconds (clamped to [`MIN_INTERVAL_SECS`]).
///   - `OnSignal` — when a named signal fires this tick (e.g. "mail", "calendar").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Once per local day, fired on the first tick at/after `hour:minute` local.
    Daily { hour: u8, minute: u8 },
    /// Every `secs` seconds since the last run (clamped to [`MIN_INTERVAL_SECS`]).
    Interval { secs: u64 },
    /// When the named signal is present this tick. `signal` is a lowercase token
    /// (e.g. "mail", "calendar", "market") the live tick maps to a real signal.
    OnSignal { signal: String },
}

impl Schedule {
    /// Parse a free-form schedule phrase into a [`Schedule`]. Conservative and
    /// bounded: anything it cannot confidently parse falls back to a SAFE default
    /// (`Daily` at 09:00) rather than guessing an aggressive cadence — a standing
    /// mission set up from an ambiguous phrase should run at most daily, never
    /// minute-by-minute. Pure and unit-testable.
    ///
    /// Accepts: "daily" / "every day" / "each morning" (Daily 09:00);
    /// "daily at 7" / "at 18:30" / "every day at 7am" (Daily H:M);
    /// "every N hours" / "hourly" / "every N minutes" (Interval, clamped);
    /// "on <signal>" / "when <signal> arrives" (OnSignal).
    pub fn parse(phrase: &str) -> Schedule {
        let p = phrase.trim().to_lowercase();
        if p.is_empty() {
            return Schedule::Daily { hour: 9, minute: 0 };
        }
        // on-signal: "on mail", "when calendar changes", "on signal market"
        for cue in ["on signal ", "when ", "on "] {
            if let Some(rest) = p.strip_prefix(cue) {
                if let Some(sig) = first_signal_token(rest) {
                    return Schedule::OnSignal { signal: sig };
                }
            }
        }
        // interval: "every N hours", "every N minutes", "hourly"
        if p.contains("hourly") {
            return Schedule::Interval { secs: clamp_interval(3_600) };
        }
        if let Some(secs) = parse_interval(&p) {
            return Schedule::Interval { secs: clamp_interval(secs) };
        }
        // daily (with optional time): "daily", "every day", "each morning",
        // "daily at 7", "at 18:30", "every morning at 7am"
        if p.contains("dail") || p.contains("every day") || p.contains("each day") || p.contains("morning") || p.starts_with("at ") || p.contains(" at ") {
            let (hour, minute) = parse_time_of_day(&p).unwrap_or((9, 0));
            return Schedule::Daily { hour, minute };
        }
        // Safe default: at most daily.
        Schedule::Daily { hour: 9, minute: 0 }
    }

    /// A compact human-readable rendering of the schedule, used in the
    /// confirmation preview and the HUD card ("daily at 09:00", "every 6h",
    /// "when mail arrives"). Pure.
    pub fn describe(&self) -> String {
        match self {
            Schedule::Daily { hour, minute } => format!("daily at {hour:02}:{minute:02}"),
            Schedule::Interval { secs } => {
                if secs % 3_600 == 0 {
                    format!("every {}h", secs / 3_600)
                } else {
                    format!("every {}m", secs / 60)
                }
            }
            Schedule::OnSignal { signal } => format!("when {signal} fires"),
        }
    }

    /// PURE due check: given `now` (unix secs), the user's `local_hour`/
    /// `local_minute` for the daily case, the mission's `last_run` (unix secs, 0 =
    /// never), and the set of `signals_present` this tick, decide whether this
    /// schedule is DUE to fire NOW. This is the heart of the scheduler — entirely
    /// a function of its inputs, so the tests drive it with an injected clock and
    /// never a live loop.
    ///
    ///   - `Daily`    — due if the local time is at/after hour:minute AND it has
    ///                  not already run today (last_run was before today's
    ///                  fire-time boundary). Never twice in one day.
    ///   - `Interval` — due if `now - last_run >= secs` (always due the first time,
    ///                  last_run == 0).
    ///   - `OnSignal` — due if the named signal is present this tick (debounced by
    ///                  the caller's last-run cooldown, see [`MIN_INTERVAL_SECS`]).
    pub fn is_due(
        &self,
        now: u64,
        local_hour: u8,
        local_minute: u8,
        last_run: u64,
        signals_present: &[String],
    ) -> bool {
        match self {
            Schedule::Daily { hour, minute } => {
                // Past the daily fire time?
                let past_fire = (local_hour as u32) * 60 + local_minute as u32
                    >= (*hour as u32) * 60 + *minute as u32;
                if !past_fire {
                    return false;
                }
                // Already ran today? First run (last_run == 0) always fires. Else it
                // fires again only once last_run PREDATES today's local midnight:
                // more than the seconds-since-local-midnight must have elapsed since
                // the last run. A 60s margin absorbs the minute-granular
                // local_minute (seconds-since-midnight can read up to ~59s short), so
                // a Daily mission fires at most ONCE per local calendar day at any
                // fire hour. (The old fixed 23h window let a 00:xx-hour mission
                // re-fire 23h later — the SAME calendar day.)
                if last_run == 0 {
                    return true;
                }
                let secs_since_local_midnight =
                    (local_hour as u64) * 3_600 + (local_minute as u64) * 60;
                now.saturating_sub(last_run) > secs_since_local_midnight + 60
            }
            Schedule::Interval { secs } => {
                let secs = clamp_interval(*secs);
                if last_run == 0 {
                    return true;
                }
                now.saturating_sub(last_run) >= secs
            }
            Schedule::OnSignal { signal } => {
                // The signal must be present this tick AND a cooldown must have
                // elapsed since the last run, so a signal that stays present for
                // many consecutive ticks does not re-fire the mission every tick.
                let present = signals_present.iter().any(|s| s == signal);
                if !present {
                    return false;
                }
                if last_run == 0 {
                    return true;
                }
                now.saturating_sub(last_run) >= MIN_INTERVAL_SECS
            }
        }
    }
}

/// Clamp an interval to at least [`MIN_INTERVAL_SECS`] (so a recurring mission can
/// never be set to fire faster than once an hour). Pure.
pub fn clamp_interval(secs: u64) -> u64 {
    secs.max(MIN_INTERVAL_SECS)
}

/// Pull the first plausible signal token from a phrase tail ("mail arrives" ->
/// "mail"). Keeps only a short alphanumeric word; `None` if there is none.
fn first_signal_token(rest: &str) -> Option<String> {
    rest.split(|c: char| !c.is_alphanumeric())
        .find(|w| w.len() >= 2 && w.len() <= 24)
        .map(|w| w.to_lowercase())
}

/// Parse "every N hours" / "every N minutes" into seconds. `None` if not that
/// shape. Pure.
fn parse_interval(p: &str) -> Option<u64> {
    let rest = p.strip_prefix("every ")?;
    let mut parts = rest.split_whitespace();
    let n: u64 = parts.next()?.parse().ok()?;
    let unit = parts.next()?;
    if unit.starts_with("hour") {
        Some(n.saturating_mul(3_600))
    } else if unit.starts_with("min") {
        Some(n.saturating_mul(60))
    } else if unit.starts_with("day") {
        Some(n.saturating_mul(86_400))
    } else {
        None
    }
}

/// Parse a time-of-day out of a phrase: "at 7", "at 7am", "at 18:30", "at 7:05pm".
/// Returns (hour, minute) on a 24h clock, or `None` if no time is present. Pure.
fn parse_time_of_day(p: &str) -> Option<(u8, u8)> {
    let idx = p.find("at ")?;
    let tail = &p[idx + 3..];
    let token = tail.split_whitespace().next()?;
    // Detect am/pm either glued to the token or as the next word.
    let next_word = tail.split_whitespace().nth(1).unwrap_or("");
    let mut t = token.to_string();
    let mut pm = t.contains("pm") || next_word.starts_with("pm");
    let am = t.contains("am") || next_word.starts_with("am");
    t = t.replace("am", "").replace("pm", "");
    let (h_str, m_str) = match t.split_once(':') {
        Some((h, m)) => (h, m),
        None => (t.as_str(), "0"),
    };
    let mut hour: u8 = h_str.parse().ok()?;
    let minute: u8 = m_str.parse().ok()?;
    if hour > 23 || minute > 59 {
        return None;
    }
    // 12h -> 24h conversion when am/pm present.
    if pm && hour < 12 {
        hour += 12;
    }
    if am && hour == 12 {
        hour = 0;
    }
    let _ = (&mut pm,); // silence unused-assign warnings on some paths
    Some((hour, minute))
}

// ---------------------------------------------------------------------------
// The standing mission record (persisted)
// ---------------------------------------------------------------------------

/// One durable standing mission. Persisted as a single JSON fact row under
/// `meta.standing.<id>`. The id is content-derived (stable, collision-resistant)
/// so the same goal+schedule round-trips and a cancel addresses it precisely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StandingMission {
    /// Stable content id (short hex), the addressing label for cancel.
    pub id: String,
    /// The autonomous objective FURY runs each time it fires.
    pub goal: String,
    /// When it runs.
    pub schedule: Schedule,
    /// Unix-secs of the last run (0 = never run yet).
    pub last_run: u64,
    /// Whether this individual mission is active. (The MASTER `[standing].enabled`
    /// switch gates the whole subsystem on top of this per-mission flag.)
    pub enabled: bool,
}

impl StandingMission {
    /// Build a new mission record from a goal + a parsed schedule, minting the
    /// stable content id and bounding the goal. `enabled` defaults true (the
    /// per-mission flag); the subsystem master switch still gates whether ANY
    /// mission runs. Pure.
    pub fn new(goal: &str, schedule: Schedule) -> StandingMission {
        let goal = bound_goal(goal);
        let id = derive_id(&goal, &schedule);
        StandingMission {
            id,
            goal,
            schedule,
            last_run: 0,
            enabled: true,
        }
    }
}

/// Bound + trim a goal string for persistence. Pure.
fn bound_goal(goal: &str) -> String {
    let g = goal.trim();
    if g.len() > MAX_GOAL_LEN {
        let mut end = MAX_GOAL_LEN;
        while end > 0 && !g.is_char_boundary(end) {
            end -= 1;
        }
        g[..end].to_string()
    } else {
        g.to_string()
    }
}

/// Derive a stable content id from goal + schedule: a short hex prefix of SHA-256
/// over `goal || NUL || describe(schedule)`. Two identical missions hash the same
/// (so a re-create is idempotent on id), and a cancel can name a mission by an id
/// that is reproducible from its content. Pure.
pub fn derive_id(goal: &str, schedule: &Schedule) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(goal.trim().to_lowercase().as_bytes());
    h.update([0u8]);
    h.update(schedule.describe().as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..6]) // 12 hex chars (48 bits) — ample to address a few
}

// ---------------------------------------------------------------------------
// The store (persisted via Memory; round-trips against a temp DB in tests)
// ---------------------------------------------------------------------------

/// Persist a standing mission as a single JSON fact row under
/// `meta.standing.<id>`. Trusted internal write (`upsert_fact`, not the
/// model-driven `upsert_user_fact`, which would reject the reserved `meta.*`
/// prefix). Idempotent on id: re-saving the same mission overwrites in place.
pub async fn save(memory: &Memory, mission: &StandingMission) -> Result<()> {
    let key = format!("{STANDING_PREFIX}{}", mission.id);
    let json = serde_json::to_string(mission)?;
    memory.upsert_fact(&key, &json).await
}

/// Create a NEW standing mission (the establish path), enforcing the active cap.
/// Returns the created record. Refuses (Err) when [`MAX_ACTIVE`] missions already
/// exist UNLESS this id already exists (a re-create/update of an existing mission
/// always succeeds, so the store never wedges). This is the function the
/// confirmed `standing_create` replay calls.
pub async fn create(memory: &Memory, goal: &str, schedule: Schedule) -> Result<StandingMission> {
    let mission = StandingMission::new(goal, schedule);
    let existing = list(memory).await?;
    let already = existing.iter().any(|m| m.id == mission.id);
    if !already && existing.len() >= MAX_ACTIVE {
        anyhow::bail!(
            "you already have the maximum of {MAX_ACTIVE} standing missions; cancel one first"
        );
    }
    save(memory, &mission).await?;
    Ok(mission)
}

/// Load every persisted standing mission, newest first. Malformed rows (a
/// hand-edited or partial record) are skipped, never panic. Bounded by the
/// store's prefix scan; the active cap keeps the real count small.
pub async fn list(memory: &Memory) -> Result<Vec<StandingMission>> {
    let rows = memory
        .recall_facts_limited(STANDING_PREFIX, 256)
        .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str::<StandingMission>(&v).ok())
        .collect())
}

/// Cancel (delete) the standing mission with `id`. Returns whether a row existed
/// (so the caller can say "no such mission" honestly). The id is matched against
/// the stored content id.
pub async fn cancel(memory: &Memory, id: &str) -> Result<bool> {
    let key = format!("{STANDING_PREFIX}{}", id.trim());
    memory.delete_fact(&key).await
}

/// Record that a mission RAN at `now` (stamps `last_run` and persists). Called by
/// the live tick AFTER a run so the scheduler's next due-check sees the new
/// last-run. A mission that vanished (cancelled mid-run) is a no-op, not an error.
pub async fn mark_ran(memory: &Memory, mission: &StandingMission, now: u64) -> Result<()> {
    // Re-load to avoid stamping a stale copy over a concurrent edit; if it's gone,
    // do nothing.
    let live = list(memory).await?;
    let Some(mut current) = live.into_iter().find(|m| m.id == mission.id) else {
        return Ok(());
    };
    current.last_run = now;
    save(memory, &current).await
}

// ---------------------------------------------------------------------------
// The scheduler (pure: decide which missions are DUE this tick)
// ---------------------------------------------------------------------------

/// PURE scheduler: given the current clock + the set of missions + the signals
/// present this tick + the subsystem `master_enabled` flag, return the missions
/// that are DUE to run NOW, in stable order. This is the testable heart of the
/// scheduler — it touches no store and no clock of its own; the live tick passes
/// in `now`/`local_hour`/`local_minute` from the injected/system clock.
///
/// SAFETY: with `master_enabled == false` (the shipped default) NOTHING is ever
/// due — the master switch gates the whole subsystem on top of each mission's own
/// `enabled` flag. A disabled INDIVIDUAL mission is also never due.
pub fn due_missions<'a>(
    missions: &'a [StandingMission],
    now: u64,
    local_hour: u8,
    local_minute: u8,
    signals_present: &[String],
    master_enabled: bool,
) -> Vec<&'a StandingMission> {
    if !master_enabled {
        return Vec::new();
    }
    missions
        .iter()
        .filter(|m| m.enabled)
        .filter(|m| {
            m.schedule
                .is_due(now, local_hour, local_minute, m.last_run, signals_present)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Establishing preview (the confirmation-gated establish text)
// ---------------------------------------------------------------------------

/// The faithful dry-run PREVIEW for ESTABLISHING a standing mission — what the
/// confirmation gate shows the user before a spoken yes spawns the recurring
/// autonomy. Names the goal + schedule precisely. Pure, so the establish copy is
/// unit-testable. Mirrors the integration dry-run previews' "[dry run] … Enable …"
/// shape so `confirm::confirmation_prompt` strips the boilerplate uniformly when
/// the action parks.
pub fn establish_preview(goal: &str, schedule: &Schedule) -> String {
    format!(
        "[dry run] I'll set up a STANDING MISSION to {} — {}. It will run on that \
         schedule and reason over the world model; any consequential step it \
         proposes still waits for your confirmation. Enable consequential actions \
         and confirm to establish it.",
        bound_goal(goal),
        schedule.describe(),
    )
}

// ---------------------------------------------------------------------------
// Running a due mission (reuses FURY's bounded engine; injected seams in tests)
// ---------------------------------------------------------------------------

/// The result of running one standing mission: the goal, the synthesized report
/// FURY produced, and the schedule description — assembled into the HUD telemetry
/// card and (when proactive speech is on) the spoken line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunReport {
    pub id: String,
    pub goal: String,
    pub schedule: String,
    pub report: String,
}

impl RunReport {
    /// The HUD `standing.run` telemetry payload.
    pub fn telemetry(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "goal": self.goal,
            "schedule": self.schedule,
            "report": self.report,
        })
    }
}

/// Run ONE standing mission through FURY's bounded mission engine and return its
/// [`RunReport`]. The run is identical to a `fury_mission` call — decompose ->
/// dispatch each sub-task as its OWNING specialist (under that specialist's
/// allowlist) -> synthesize — so EVERY consequential step inside the run STILL
/// parks behind the confirmation gate + the armed-by-default master switch (a
/// confirmed action still needs a fresh per-action confirm). A
/// standing mission therefore does autonomous reasoning but can never auto-fire an
/// outward action. Generic over the [`Planner`]/[`Dispatcher`] seams so tests
/// drive it with the mission engine's mocks (no real cloud); the live tick wires
/// the cloud-backed pair, exactly like `run_fury_mission`.
pub async fn run_one(
    mission: &StandingMission,
    registry: &AgentRegistry,
    planner: &dyn Planner,
    dispatcher: &dyn Dispatcher,
    cloud_reachable: bool,
) -> RunReport {
    let report = run_mission(&mission.goal, registry, planner, dispatcher, cloud_reachable).await;
    RunReport {
        id: mission.id.clone(),
        goal: mission.goal.clone(),
        schedule: mission.schedule.describe(),
        report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::PlannedTask;
    use std::future::Future;
    use std::path::PathBuf;
    use std::pin::Pin;
    use std::sync::Mutex;

    // ---- temp DB ----------------------------------------------------------

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jarvis-standing-test-{}-{}.db",
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

    // ---- schedule parsing (pure) ------------------------------------------

    #[test]
    fn parse_daily_variants() {
        assert_eq!(Schedule::parse("daily"), Schedule::Daily { hour: 9, minute: 0 });
        assert_eq!(Schedule::parse("every day"), Schedule::Daily { hour: 9, minute: 0 });
        assert_eq!(Schedule::parse("each morning"), Schedule::Daily { hour: 9, minute: 0 });
        assert_eq!(Schedule::parse("daily at 7"), Schedule::Daily { hour: 7, minute: 0 });
        assert_eq!(Schedule::parse("at 18:30"), Schedule::Daily { hour: 18, minute: 30 });
        assert_eq!(Schedule::parse("every day at 7am"), Schedule::Daily { hour: 7, minute: 0 });
        assert_eq!(Schedule::parse("daily at 7pm"), Schedule::Daily { hour: 19, minute: 0 });
        assert_eq!(Schedule::parse("at 12am"), Schedule::Daily { hour: 0, minute: 0 });
        assert_eq!(Schedule::parse("at 12pm"), Schedule::Daily { hour: 12, minute: 0 });
    }

    #[test]
    fn parse_interval_variants_and_clamp() {
        assert_eq!(Schedule::parse("every 6 hours"), Schedule::Interval { secs: 6 * 3_600 });
        assert_eq!(Schedule::parse("hourly"), Schedule::Interval { secs: 3_600 });
        // sub-hour intervals clamp UP to the floor (no minute-by-minute autonomy).
        assert_eq!(Schedule::parse("every 5 minutes"), Schedule::Interval { secs: MIN_INTERVAL_SECS });
        assert_eq!(Schedule::parse("every 2 days"), Schedule::Interval { secs: 2 * 86_400 });
    }

    #[test]
    fn parse_on_signal() {
        assert_eq!(
            Schedule::parse("on mail"),
            Schedule::OnSignal { signal: "mail".to_string() }
        );
        assert_eq!(
            Schedule::parse("when calendar changes"),
            Schedule::OnSignal { signal: "calendar".to_string() }
        );
    }

    #[test]
    fn parse_ambiguous_falls_back_to_safe_daily_never_aggressive() {
        // An un-parseable phrase must NOT become a fast interval — it defaults to
        // at-most-daily, the safe cadence for an ambiguous establish.
        for p in ["whenever", "sometime", "asap", ""] {
            assert_eq!(
                Schedule::parse(p),
                Schedule::Daily { hour: 9, minute: 0 },
                "{p:?} must fall back to safe daily, not a fast interval"
            );
        }
    }

    #[test]
    fn describe_is_human_readable() {
        assert_eq!(Schedule::Daily { hour: 9, minute: 0 }.describe(), "daily at 09:00");
        assert_eq!(Schedule::Interval { secs: 6 * 3_600 }.describe(), "every 6h");
        assert_eq!(
            Schedule::OnSignal { signal: "mail".into() }.describe(),
            "when mail fires"
        );
    }

    // ---- scheduler due logic (pure, injected clock) -----------------------

    #[test]
    fn interval_is_due_first_time_then_after_the_interval() {
        let s = Schedule::Interval { secs: 6 * 3_600 };
        let now = 1_000_000u64;
        // never run -> due.
        assert!(s.is_due(now, 0, 0, 0, &[]));
        // ran just now -> not due.
        assert!(!s.is_due(now, 0, 0, now, &[]));
        // ran 5h ago -> not due (interval is 6h).
        assert!(!s.is_due(now, 0, 0, now - 5 * 3_600, &[]));
        // ran 6h ago -> due.
        assert!(s.is_due(now, 0, 0, now - 6 * 3_600, &[]));
    }

    #[test]
    fn interval_is_clamped_so_a_fast_schedule_cannot_hammer() {
        // A persisted 60s interval (e.g. a hand-edited record) is clamped to the
        // floor at due-check time: running 10 minutes ago is NOT due.
        let s = Schedule::Interval { secs: 60 };
        let now = 1_000_000u64;
        assert!(!s.is_due(now, 0, 0, now - 600, &[]), "sub-hour interval must clamp up");
        assert!(s.is_due(now, 0, 0, now - MIN_INTERVAL_SECS, &[]));
    }

    #[test]
    fn daily_is_due_after_fire_time_and_not_twice_in_a_day() {
        let s = Schedule::Daily { hour: 9, minute: 0 };
        let now = 1_000_000u64;
        // Before 09:00 -> not due.
        assert!(!s.is_due(now, 8, 59, 0, &[]), "before fire time is not due");
        // At/after 09:00, never run -> due.
        assert!(s.is_due(now, 9, 0, 0, &[]));
        assert!(s.is_due(now, 10, 30, 0, &[]));
        // Already ran an hour ago today -> not due again today.
        assert!(!s.is_due(now, 10, 0, now - 3_600, &[]), "must not fire twice in a day");
        // Ran 24h ago -> due again.
        assert!(s.is_due(now, 9, 0, now - 24 * 3_600, &[]));
    }

    /// REGRESSION: a MIDNIGHT-hour Daily mission must not fire twice in one local
    /// day. The old fixed 23h window (< 24h) let a 00:00 mission become due again
    /// exactly 23h later — still the same calendar day.
    #[test]
    fn daily_midnight_mission_does_not_fire_twice_in_one_local_day() {
        let s = Schedule::Daily { hour: 0, minute: 0 };
        let t0 = 1_000_000u64;
        // 00:00, never run -> fires (last_run becomes t0).
        assert!(s.is_due(t0, 0, 0, 0, &[]));
        // 23:00 the SAME day (23h later): ran at 00:00 -> must NOT fire again today.
        assert!(
            !s.is_due(t0 + 23 * 3_600, 23, 0, t0, &[]),
            "a midnight Daily mission must not re-fire 23h later the same day"
        );
        // Next local day at 00:00 (~24h later) -> fires again.
        assert!(
            s.is_due(t0 + 24 * 3_600, 0, 0, t0, &[]),
            "fires again the next local day"
        );
    }

    #[test]
    fn on_signal_is_due_only_when_present_and_debounced() {
        let s = Schedule::OnSignal { signal: "mail".into() };
        let now = 1_000_000u64;
        // Not present -> not due.
        assert!(!s.is_due(now, 0, 0, 0, &["calendar".to_string()]));
        // Present, never run -> due.
        assert!(s.is_due(now, 0, 0, 0, &["mail".to_string()]));
        // Present but ran 10 minutes ago -> debounced, not due.
        assert!(!s.is_due(now, 0, 0, now - 600, &["mail".to_string()]));
        // Present and cooldown elapsed -> due.
        assert!(s.is_due(now, 0, 0, now - MIN_INTERVAL_SECS, &["mail".to_string()]));
    }

    #[test]
    fn due_missions_master_switch_off_fires_nothing() {
        let m = StandingMission::new("review deadlines", Schedule::Interval { secs: 3_600 });
        let missions = vec![m];
        // Even though the interval mission has never run (would be due), the
        // master switch OFF means NOTHING is due — the core safety property.
        let due = due_missions(&missions, 1_000_000, 9, 0, &[], false);
        assert!(due.is_empty(), "master switch off must fire nothing");
        // With it on, it IS due.
        let due = due_missions(&missions, 1_000_000, 9, 0, &[], true);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn due_missions_skips_individually_disabled_missions() {
        let mut m = StandingMission::new("x", Schedule::Interval { secs: 3_600 });
        m.enabled = false;
        let missions = vec![m];
        let due = due_missions(&missions, 1_000_000, 9, 0, &[], true);
        assert!(due.is_empty(), "a disabled mission is never due even with master on");
    }

    #[test]
    fn due_missions_selects_only_the_due_ones() {
        let now = 1_000_000u64;
        let mut a = StandingMission::new("a — due now", Schedule::Interval { secs: 3_600 });
        a.last_run = now - 7_200; // 2h ago, interval 1h -> due
        let mut b = StandingMission::new("b — not yet", Schedule::Interval { secs: 6 * 3_600 });
        b.last_run = now - 3_600; // 1h ago, interval 6h -> not due
        let missions = vec![a.clone(), b];
        let due = due_missions(&missions, now, 9, 0, &[], true);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, a.id);
    }

    // ---- store round-trips (temp DB) --------------------------------------

    #[tokio::test]
    async fn create_list_cancel_roundtrip() {
        let db = TempDb::new("roundtrip");
        let mem = Memory::open(&db.0).unwrap();

        let m = create(&mem, "review my deadlines", Schedule::parse("daily at 8"))
            .await
            .unwrap();
        assert_eq!(m.schedule, Schedule::Daily { hour: 8, minute: 0 });
        assert!(m.enabled);
        assert_eq!(m.last_run, 0);

        let listed = list(&mem).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].goal, "review my deadlines");
        assert_eq!(listed[0].id, m.id);

        // Cancel by id removes it; a second cancel is a no-op.
        assert!(cancel(&mem, &m.id).await.unwrap());
        assert!(!cancel(&mem, &m.id).await.unwrap());
        assert!(list(&mem).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn standing_records_never_leak_into_agent_recall_or_world() {
        // A standing mission is persisted under meta.standing.* — internal
        // bookkeeping that must NEVER reach an agent's scoped recall or the
        // world model (it's daemon state, not user knowledge).
        let db = TempDb::new("isolation");
        let mem = Memory::open(&db.0).unwrap();
        let _ = create(&mem, "secret standing goal about jarvis", Schedule::parse("daily"))
            .await
            .unwrap();
        // Agent-scoped recall (what an agent's prompt is fed) must not see it.
        let scoped = mem.agent_scoped_facts("agent.friday", 50).await.unwrap();
        assert!(
            !scoped.iter().any(|(k, _)| k.starts_with("meta.standing.")),
            "standing record leaked into agent recall: {scoped:?}"
        );
        // The world model snapshot (shared tier) must not see it either.
        let world = crate::world_model::snapshot(&mem).await.unwrap();
        assert!(world.is_empty(), "standing record leaked into the world model: {world:?}");
    }

    #[tokio::test]
    async fn create_enforces_the_active_cap() {
        let db = TempDb::new("cap");
        let mem = Memory::open(&db.0).unwrap();
        for i in 0..MAX_ACTIVE {
            create(&mem, &format!("mission number {i}"), Schedule::parse("daily"))
                .await
                .unwrap();
        }
        // One more NEW mission is refused.
        let err = create(&mem, "one too many missions here", Schedule::parse("daily"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("maximum"), "wrong error: {err}");
        // But re-creating (updating) an EXISTING one still succeeds.
        create(&mem, "mission number 0", Schedule::parse("daily"))
            .await
            .unwrap();
        assert_eq!(list(&mem).await.unwrap().len(), MAX_ACTIVE);
    }

    #[tokio::test]
    async fn mark_ran_stamps_last_run_and_changes_due() {
        let db = TempDb::new("markran");
        let mem = Memory::open(&db.0).unwrap();
        let m = create(&mem, "interval mission", Schedule::Interval { secs: 6 * 3_600 })
            .await
            .unwrap();
        let now = 2_000_000u64;
        // Before any run, it is due (last_run == 0).
        let missions = list(&mem).await.unwrap();
        assert_eq!(due_missions(&missions, now, 9, 0, &[], true).len(), 1);
        // Mark it ran now.
        mark_ran(&mem, &m, now).await.unwrap();
        let missions = list(&mem).await.unwrap();
        assert_eq!(missions[0].last_run, now);
        // Now it is NOT due (just ran).
        assert!(due_missions(&missions, now + 60, 9, 0, &[], true).is_empty());
        // After the interval, it is due again.
        assert_eq!(
            due_missions(&missions, now + 6 * 3_600, 9, 0, &[], true).len(),
            1
        );
    }

    // ---- establishing preview (the confirmation copy) ----------------------

    #[test]
    fn establish_preview_names_goal_and_schedule() {
        let p = establish_preview("review deadlines", &Schedule::Daily { hour: 9, minute: 0 });
        assert!(p.contains("review deadlines"), "names the goal: {p}");
        assert!(p.contains("daily at 09:00"), "names the schedule: {p}");
        assert!(p.contains("STANDING MISSION"), "frames it as a standing mission: {p}");
        // The faithful preview carries the boilerplate the confirm prompt strips.
        assert!(p.starts_with("[dry run]"), "carries the dry-run lead-in: {p}");
    }

    /// ESTABLISHING routes through the cross-turn confirmation gate: parking the
    /// standing_create preview yields a clean spoken confirm prompt naming the
    /// goal+schedule, and NOTHING is created until a later "yes" replays it. This
    /// proves a create PARKS rather than silently spawning recurring autonomy.
    #[test]
    fn establishing_parks_for_confirmation_and_creates_nothing_yet() {
        use crate::confirm::{self, PendingConfirmation};
        // Serialize against the process-global confirm slot.
        let _g = confirm::PENDING_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        confirm::clear();

        // standing_create MUST be a consequential tool so the gate parks it.
        assert!(
            confirm::is_consequential_tool("standing_create"),
            "standing_create must be gated so establishing requires a spoken yes"
        );

        let preview = establish_preview("review deadlines", &Schedule::Daily { hour: 9, minute: 0 });
        let prompt = confirm::park(PendingConfirmation {
            agent: "agent.fury".into(),
            tool: "standing_create".into(),
            input: serde_json::json!({"goal": "review deadlines", "schedule": "daily"}),
            allowed: vec!["standing_create".into()],
            preview,
            created_at: std::time::Instant::now(),
            id: String::new(),
        });
        // The spoken prompt invites a yes/no and names the action — and the
        // off-mode boilerplate is stripped.
        assert!(prompt.contains("confirm"), "prompt invites a yes: {prompt}");
        assert!(prompt.contains("review deadlines"), "prompt names the goal: {prompt}");
        assert!(!prompt.contains("[dry run]"), "lead-in stripped: {prompt}");
        assert!(
            !prompt.contains("Enable consequential actions"),
            "enablement hint stripped: {prompt}"
        );

        // A live pending is parked (nothing has been created in any store) — only
        // a later spoken Affirm would replay the create.
        let taken = confirm::take_live(std::time::Instant::now()).expect("create parked");
        assert_eq!(taken.tool, "standing_create");
        confirm::clear();
    }

    // ---- a RUN reasons over the world model + a consequential step PARKS ----

    /// A mock planner returning a fixed plan (no cloud). Mirrors the mock pattern
    /// in `mission.rs` (`Box::pin` over the trait's future type) so no test makes
    /// a real cloud call.
    struct MockPlanner {
        plan: Vec<PlannedTask>,
    }
    impl Planner for MockPlanner {
        fn plan<'a>(
            &'a self,
            _goal: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<PlannedTask>>> + Send + 'a>> {
            let plan = self.plan.clone();
            Box::pin(async move { Ok(plan) })
        }
    }

    /// A dispatcher that records calls and models the consequential GATE: a
    /// "post"/"send" sub-task comes back as a DRY-RUN PREVIEW (never executed),
    /// exactly as `integrations::gate` forces with the master switch off — so a
    /// standing-mission run can never auto-fire an outward action.
    struct MockDispatcher {
        calls: Mutex<Vec<String>>,
    }
    impl Dispatcher for MockDispatcher {
        fn dispatch<'a>(
            &'a self,
            _agent: &'a str,
            _tools: &'a [String],
            instruction: &'a str,
            _depth: usize,
        ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(instruction.to_string());
                let lower = instruction.to_lowercase();
                if lower.contains("post ") || lower.contains("send ") {
                    Ok(format!("[dry-run preview] would have actioned: {instruction}"))
                } else {
                    Ok(format!("done: {instruction}"))
                }
            })
        }
    }

    #[tokio::test]
    async fn a_run_reasons_over_the_world_and_a_consequential_step_still_parks() {
        let registry = AgentRegistry::canonical();
        // The plan: one READ/REASON step (world query) and one CONSEQUENTIAL step
        // (post). The consequential one must come back as a preview, NOT executed.
        let planner = MockPlanner {
            plan: vec![
                PlannedTask::say("check the world model for deadlines slipping this week"),
                PlannedTask::say("post a summary to the team channel"),
            ],
        };
        let dispatcher = MockDispatcher { calls: Mutex::new(Vec::new()) };
        let mission = StandingMission::new(
            "review deadlines and flag slippage",
            Schedule::parse("daily"),
        );

        let run = run_one(&mission, &registry, &planner, &dispatcher, true).await;

        // The run reasoned (both sub-tasks dispatched) and produced a report.
        let calls = dispatcher.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2, "both sub-tasks ran: {calls:?}");
        assert!(run.report.contains("review deadlines"), "report names the goal: {}", run.report);
        // The CONSEQUENTIAL step PREVIEWED — it did NOT auto-fire. No silent
        // autonomy: a standing mission cannot post on its own.
        assert!(
            run.report.to_lowercase().contains("dry-run")
                || run.report.to_lowercase().contains("preview"),
            "the consequential step must preview, not execute: {}",
            run.report
        );
        assert!(
            !run.report.to_lowercase().contains("posted to"),
            "the action must NOT have actually executed: {}",
            run.report
        );
    }

    #[tokio::test]
    async fn a_run_offline_degrades_without_pretending() {
        let registry = AgentRegistry::canonical();
        let planner = MockPlanner { plan: vec![PlannedTask::say("x")] };
        let dispatcher = MockDispatcher { calls: Mutex::new(Vec::new()) };
        let mission = StandingMission::new("do the thing", Schedule::parse("daily"));
        // cloud_reachable = false -> the mission engine degrades, no dispatch.
        let run = run_one(&mission, &registry, &planner, &dispatcher, false).await;
        assert!(dispatcher.calls.lock().unwrap().is_empty(), "offline must not dispatch");
        assert!(
            run.report.to_lowercase().contains("cloud") || run.report.to_lowercase().contains("offline"),
            "offline degrades honestly: {}",
            run.report
        );
    }

    #[test]
    fn run_report_telemetry_carries_the_card_fields() {
        let r = RunReport {
            id: "abc123".into(),
            goal: "review deadlines".into(),
            schedule: "daily at 09:00".into(),
            report: "Mission on review deadlines, sir.".into(),
        };
        let t = r.telemetry();
        assert_eq!(t["id"], "abc123");
        assert_eq!(t["goal"], "review deadlines");
        assert_eq!(t["schedule"], "daily at 09:00");
        assert!(t["report"].as_str().unwrap().contains("review deadlines"));
    }
}
