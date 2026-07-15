//! STANDING MISSIONS — durable, scheduled, autonomous goals that run on the
//! standing-missions scheduler tick (a dedicated runtime loop in `main.rs`,
//! distinct from EDITH's anticipation tick; ON by default via [standing].enabled)
//! and reason over the shared World Model.
//!
//! A STANDING MISSION is a saved goal plus a TRIGGER. A trigger is one of two axes:
//!   - a SCHEDULE (time): "every morning, review my deadlines and flag anything
//!     slipping", "every 6 hours, check the world model for blocked tasks"; OR
//!   - a TRIPWIRE (condition, [`Schedule::Condition`]): a PURE threshold predicate
//!     ([`Condition`]) over the verified signal snapshot the scheduler tick
//!     assembles — "when free disk drops below 10%", "when unread > 5", "when a
//!     calendar event is within 15 minutes". Reactive autonomy, still gated per step.
//!
//! On each tick the scheduler decides which missions are DUE: the TIME triggers
//! purely from the clock + each mission's schedule + its last-run stamp
//! ([`due_missions`]); the TRIPWIRES from the signal snapshot via a debounced,
//! hysteresis-guarded predicate ([`due_condition_missions`]) so a flapping signal
//! can't spam. A due mission RUNS through FURY's bounded mission engine
//! ([`crate::mission`]) — decompose -> dispatch each sub-task to its owning
//! specialist -> synthesize — grounding itself on the shared World Model, and a
//! tripwire RE-REASONS its response each time it fires (never a frozen macro). The
//! result surfaces to the HUD as a `standing.*` telemetry card and is only SPOKEN
//! when proactive speech is on. A tripwire may only READ + REASON; it holds no
//! actuator, and every consequential step a run proposes routes through the SAME
//! confirmation gate + per-action policy a DIRECT request would — parking for a
//! fresh spoken "yes" UNLESS you have granted that specific tool a standing
//! "always-allow" policy. Arming a tripwire is itself the CONFIRMED `standing_create`.
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
//! 2. **NO AUTONOMY BEYOND WHAT YOU ALREADY AUTHORIZED when a mission RUNS.** A run
//!    reuses [`crate::mission::run_mission`], so every sub-task executes as its
//!    OWNING specialist under that specialist's tool allowlist, and every
//!    CONSEQUENTIAL step (post/send/spend/control) routes through the SAME
//!    confirmation gate + per-action policy + master switch a DIRECT request would —
//!    a standing mission never acts more freely than you acting yourself. Such a step
//!    PARKS for a fresh spoken "yes" UNLESS you have granted that specific tool a
//!    standing "always-allow" policy, in which case it acts under exactly the
//!    authorization you configured (and only that tool — the master switch and
//!    lockdown still bind). A run does autonomous READING/REASONING; any outward
//!    action it takes is one you have already authorized, per-time or via policy.
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
use crate::anticipate::Signals;
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

/// The rate-limit FLOOR for a condition ("tripwire") trigger, in seconds: even a
/// condition that keeps clearing and re-crossing its threshold can never re-fire
/// faster than this. The configured `[standing].condition_debounce_secs` is clamped
/// UP to this (defense against a hand-edited tiny debounce that would let a flapping
/// signal spam fires). Five minutes — tighter than [`MIN_INTERVAL_SECS`] because a
/// reactive tripwire (disk crossed low, calendar event imminent) is worth catching
/// sooner than a recurring time mission, but still far too slow to hammer.
pub const MIN_CONDITION_DEBOUNCE_SECS: u64 = 300;

/// Schmitt-trigger hysteresis dead-band for the disk/memory PERCENTAGE conditions:
/// once a tripwire fires, its latch clears (re-arming it for the next rising edge)
/// only after the watched percentage has receded past the trigger threshold by this
/// many points. So a reading hovering right at the threshold cannot flap the latch
/// on and off — and thus cannot spam fires.
const DISK_MEM_HYSTERESIS_PCT: f64 = 2.0;

/// Hysteresis dead-band (minutes) for the calendar-lead condition: the latch clears
/// only once NO event is within `threshold + this` minutes, so an event sitting near
/// the boundary can't flap the latch.
const CALENDAR_HYSTERESIS_MIN: i64 = 5;

// ---------------------------------------------------------------------------
// Condition (TRIPWIRE) — a PURE threshold predicate over the signals snapshot
// ---------------------------------------------------------------------------

/// A TRIPWIRE predicate: a PURE, threshold-based condition over the verified
/// [`Signals`] snapshot the standing scheduler tick assembles (the SAME snapshot
/// EDITH's anticipation tick reasons over — disk-free %, memory-used %, the
/// important-unread count, upcoming-event lead time, presence). A condition standing
/// mission ([`Schedule::Condition`]) fires when its predicate crosses the threshold,
/// launching the SAME bounded FURY engine a time mission uses — RE-REASONED each
/// fire, never a frozen macro, and every consequential step still parks.
///
/// Evaluation is a pure function of the snapshot ([`Self::holds`]) so it is directly
/// unit-testable, with a companion re-arm predicate ([`Self::cleared`]) that carries
/// the hysteresis dead-band. A tripwire holds NO actuator: it may only READ + REASON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "metric", rename_all = "snake_case")]
pub enum Condition {
    /// Free disk drops strictly BELOW `pct` percent (a low-disk tripwire).
    DiskFreePctBelow { pct: f64 },
    /// Memory use reaches AT/ABOVE `pct` percent (a memory-pressure tripwire).
    MemUsedPctAtLeast { pct: f64 },
    /// Important-unread mail reaches AT/ABOVE `count`.
    UnreadAtLeast { count: u32 },
    /// A (non-past) calendar event starts WITHIN `minutes` from now.
    CalendarWithinMinutes { minutes: i64 },
}

impl Condition {
    /// PURE fire predicate: does this condition HOLD for the snapshot right now?
    /// Absent sources (no health reading, no events) never hold — a tripwire never
    /// fires on data it cannot measure (the no-fabrication ethos).
    pub fn holds(&self, s: &Signals) -> bool {
        match self {
            Condition::DiskFreePctBelow { pct } => {
                s.health.is_some_and(|h| h.disk_free_pct < *pct)
            }
            Condition::MemUsedPctAtLeast { pct } => {
                s.health.is_some_and(|h| h.mem_used_pct >= *pct)
            }
            Condition::UnreadAtLeast { count } => s.important_unread >= *count,
            Condition::CalendarWithinMinutes { minutes } => s
                .events
                .iter()
                .any(|e| e.minutes_until >= 0 && e.minutes_until <= *minutes),
        }
    }

    /// PURE re-arm predicate: has the metric receded past the trigger threshold by
    /// the hysteresis dead-band, so the latch may clear and re-arm for the next
    /// rising edge? An ABSENT source counts as cleared (it cannot be "low"/"high"/
    /// "imminent" if we cannot read it), so a source dropping out re-arms the
    /// tripwire rather than pinning it latched forever. Disjoint from [`Self::holds`]:
    /// between the two thresholds is the dead-band where neither fires nor re-arms.
    pub fn cleared(&self, s: &Signals) -> bool {
        match self {
            Condition::DiskFreePctBelow { pct } => s
                .health
                .is_none_or(|h| h.disk_free_pct >= *pct + DISK_MEM_HYSTERESIS_PCT),
            Condition::MemUsedPctAtLeast { pct } => s
                .health
                .is_none_or(|h| h.mem_used_pct <= *pct - DISK_MEM_HYSTERESIS_PCT),
            Condition::UnreadAtLeast { count } => {
                s.important_unread < *count // a count metric: cleared once back under the floor
            }
            Condition::CalendarWithinMinutes { minutes } => !s
                .events
                .iter()
                .any(|e| e.minutes_until >= 0 && e.minutes_until <= *minutes + CALENDAR_HYSTERESIS_MIN),
        }
    }

    /// A compact human-readable rendering used in the id, the establish preview, and
    /// the telemetry frame ("free disk drops below 10%"). Pure.
    pub fn describe(&self) -> String {
        match self {
            Condition::DiskFreePctBelow { pct } => {
                format!("free disk drops below {}%", trim_num(*pct))
            }
            Condition::MemUsedPctAtLeast { pct } => {
                format!("memory use reaches {}%", trim_num(*pct))
            }
            Condition::UnreadAtLeast { count } => format!("unread mail reaches {count}"),
            Condition::CalendarWithinMinutes { minutes } => {
                format!("a calendar event is within {minutes} minutes")
            }
        }
    }

    /// Parse a free-form tripwire phrase into a [`Condition`], or `None` when the
    /// phrase is not a recognized condition. Conservative: a condition is only
    /// recognized when a KNOWN METRIC KEYWORD is present ALONGSIDE a number, so a
    /// bare on-signal phrase ("when calendar changes") or a time phrase ("daily at
    /// 7", "every 6 hours") never mis-parses as a tripwire. Pure and unit-testable.
    ///
    /// Accepts (examples): "free disk below 10%", "disk under 15" (DiskFreePctBelow);
    /// "memory above 90%", "mem >= 85" (MemUsedPctAtLeast); "unread above 5",
    /// "unread > 3" (UnreadAtLeast); "calendar event within 15m", "meeting within 30
    /// minutes" (CalendarWithinMinutes).
    pub fn parse(phrase: &str) -> Option<Condition> {
        let p = phrase.trim().to_lowercase();
        let num = first_number(&p);
        if p.contains("disk") {
            return Some(Condition::DiskFreePctBelow { pct: num? });
        }
        if p.contains("mem") {
            return Some(Condition::MemUsedPctAtLeast { pct: num? });
        }
        if p.contains("unread") || p.contains("inbox") {
            return Some(Condition::UnreadAtLeast {
                count: num?.max(1.0) as u32,
            });
        }
        if p.contains("calendar") || p.contains("event") || p.contains("meeting") {
            return Some(Condition::CalendarWithinMinutes {
                minutes: num?.max(0.0) as i64,
            });
        }
        None
    }
}

/// Format an f64 without a trailing `.0` for whole values ("10" not "10.0"),
/// keeping the decimals otherwise ("12.5"). Pure.
fn trim_num(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Pull the first contiguous number (integer or decimal) out of a phrase. `None`
/// when there is no digit. Pure.
fn first_number(p: &str) -> Option<f64> {
    let bytes = p.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            return p[start..i].parse::<f64>().ok();
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Schedule (pure value type)
// ---------------------------------------------------------------------------

/// WHEN a standing mission runs. Four shapes:
///   - `Daily`     — once per local day, at or after `hour:minute` (a TIME trigger).
///   - `Interval`  — every `secs` seconds (a TIME trigger, clamped to
///     [`MIN_INTERVAL_SECS`]).
///   - `OnSignal`  — when a named signal TOKEN is present this tick (e.g. "mail").
///   - `Condition` — a TRIPWIRE: fires when a [`Condition`] predicate crosses its
///     threshold against the verified signal snapshot (reactive autonomy).
///
/// The three non-condition shapes are evaluated purely against an injected clock by
/// [`Schedule::is_due`]. `Condition` is DELIBERATELY not clock-due — [`Schedule::is_due`]
/// returns `false` for it — because a tripwire is evaluated on a DIFFERENT axis (the
/// signal snapshot + a hysteresis/debounce ledger) by [`due_condition_missions`],
/// NOT the clock/signal-token scheduler. This keeps the two paths from ever
/// double-firing the same mission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Schedule {
    /// Once per local day, fired on the first tick at/after `hour:minute` local.
    Daily { hour: u8, minute: u8 },
    /// Every `secs` seconds since the last run (clamped to [`MIN_INTERVAL_SECS`]).
    Interval { secs: u64 },
    /// When the named signal is present this tick. `signal` is a lowercase token
    /// (e.g. "mail", "calendar", "market") the live tick maps to a real signal.
    OnSignal { signal: String },
    /// TRIPWIRE: fire when the [`Condition`] predicate crosses its threshold against
    /// the verified signal snapshot. Evaluated (debounced + hysteresis-guarded) by
    /// [`due_condition_missions`] on the scheduler tick, never by [`Schedule::is_due`].
    Condition { cond: Condition },
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
        // TRIPWIRE: a condition-trigger phrase ("free disk below 10%", "unread above
        // 5", "calendar event within 15m") parses to a Condition. Recognized ONLY
        // when a metric keyword AND a number are both present, so a bare signal
        // phrase ("when calendar changes") stays OnSignal and a time phrase stays
        // Daily/Interval — this check never steals those.
        if let Some(cond) = Condition::parse(&p) {
            return Schedule::Condition { cond };
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
            Schedule::Condition { cond } => format!("when {}", cond.describe()),
        }
    }

    /// Whether this schedule is a TRIPWIRE ([`Schedule::Condition`]) — evaluated by
    /// [`due_condition_missions`] against the signal snapshot rather than the clock.
    pub fn is_condition(&self) -> bool {
        matches!(self, Schedule::Condition { .. })
    }

    /// PURE due check: given `now` (unix secs), the user's `local_hour`/
    /// `local_minute` for the daily case, the mission's `last_run` (unix secs, 0 =
    /// never), and the set of `signals_present` this tick, decide whether this
    /// schedule is DUE to fire NOW. This is the heart of the scheduler — entirely
    /// a function of its inputs, so the tests drive it with an injected clock and
    /// never a live loop.
    ///
    ///   - `Daily`    — due if the local time is at/after hour:minute AND it has
    ///     not already run today (last_run was before today's
    ///     fire-time boundary). Never twice in one day.
    ///   - `Interval` — due if `now - last_run >= secs` (always due the first time,
    ///     last_run == 0).
    ///   - `OnSignal` — due if the named signal is present this tick (debounced by
    ///     the caller's last-run cooldown, see [`MIN_INTERVAL_SECS`]).
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
            Schedule::Condition { .. } => {
                // A TRIPWIRE is NEVER clock/signal-token due: it is evaluated on the
                // signal snapshot (with hysteresis + debounce) by
                // [`due_condition_missions`], not here. Returning false keeps the
                // time scheduler ([`due_missions`]) from ever selecting it, so a
                // condition mission fires on exactly ONE path.
                false
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
/// (Not `Eq`: a [`Schedule::Condition`] carries an `f64` threshold, so the whole
/// record is `PartialEq` only — sufficient for the round-trip/assert tests.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    // TRIPWIRE ARM telemetry: arming a condition trigger is itself a confirmed
    // action (this `create` is only reached from the confirmed `standing_create`
    // path), so an arm is worth its own HUD frame. Time-based missions keep only the
    // plain `standing.created` frame (this returns None for them). Fire-and-forget;
    // a no-op when no HUD hub is initialized (so tests are unaffected).
    if let Some(frame) = tripwire_armed_telemetry(&mission) {
        crate::telemetry::emit("system", "standing.tripwire_armed", frame);
    }
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
// The TRIPWIRE scheduler (pure: condition triggers + hysteresis/debounce)
// ---------------------------------------------------------------------------

/// Clamp a condition-tripwire debounce interval UP to [`MIN_CONDITION_DEBOUNCE_SECS`]
/// (so even a hand-edited tiny value can never let a flapping signal spam fires).
/// Pure.
pub fn clamp_debounce(secs: u64) -> u64 {
    secs.max(MIN_CONDITION_DEBOUNCE_SECS)
}

/// Per-tripwire debounce/hysteresis bookkeeping, carried across ticks by the live
/// standing loop (like [`crate::anticipate::FiredState`] /
/// [`crate::signals::CollectorState`] — it lives at the loop edge, NOT in the
/// persisted record). Maps a mission id to whether its condition is currently
/// LATCHED (it has crossed the fire threshold and has not yet receded past the
/// hysteresis dead-band). Ephemeral on purpose: a restart re-derives the latch from
/// the first fresh reading, which is fail-SAFE (a still-true condition simply
/// re-arms and re-fires once, respecting the debounce cooldown — never a storm).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TripwireLedger {
    latched: Vec<(String, bool)>,
}

impl TripwireLedger {
    /// Whether `id`'s tripwire is currently latched (default `false` — unseen).
    pub fn latched(&self, id: &str) -> bool {
        self.latched
            .iter()
            .find(|(k, _)| k == id)
            .map(|(_, v)| *v)
            .unwrap_or(false)
    }

    /// Set `id`'s latch state. The ledger is kept bounded by the ACTIVE
    /// condition-mission set via [`retain_ids`](Self::retain_ids), called each eval —
    /// so it never accumulates entries for cancelled/edited tripwires.
    pub fn set(&mut self, id: &str, v: bool) {
        match self.latched.iter_mut().find(|(k, _)| k == id) {
            Some(slot) => slot.1 = v,
            None => self.latched.push((id.to_string(), v)),
        }
    }

    /// Drop latch entries whose id no longer belongs to a live tripwire. Called each
    /// eval against the CURRENT condition-mission ids, so the ledger stays bounded by
    /// the active set (<= [`MAX_ACTIVE`]) instead of growing with every distinct id
    /// ever seen over the daemon's uptime (a cancelled/edited tripwire mints a new
    /// content-derived id, leaving the old entry stranded without this).
    pub fn retain_ids<F: Fn(&str) -> bool>(&mut self, keep: F) {
        self.latched.retain(|(k, _)| keep(k));
    }

    /// The number of latch entries currently held (test-only accessor for the bound).
    #[cfg(test)]
    pub fn latched_count(&self) -> usize {
        self.latched.len()
    }
}

/// PURE tripwire step for ONE condition mission: a Schmitt-trigger + cooldown that
/// decides whether it FIRES this tick and returns its next latch state.
///
///   - `holds`   — the condition crosses its FIRE threshold on this snapshot.
///   - `cleared` — the metric has receded past the hysteresis dead-band (safe to
///     re-arm). `holds` and `cleared` are disjoint; between them is the dead-band
///     where the latch simply holds.
///   - Latch: SET on a fire-level reading, CLEAR once cleared, else HOLD. This is
///     what gives "no re-fire while still true" — a rising edge is `holds &&
///     !was_latched`, and a condition that STAYS true stays latched (no edge, no
///     re-fire).
///   - Rate limit: even a genuine rising edge fires only once the `debounce_secs`
///     cooldown (clamped up to the floor) has elapsed since `last_run` — so a signal
///     that flaps across the dead-band can never spam.
///
/// Returns `(fire, next_latched)`. Entirely a function of its inputs — the live tick
/// passes `now`/`last_run` from the clock/store and threads `was_latched` through the
/// [`TripwireLedger`], so this is exercised with no live loop.
pub fn tripwire_step(
    holds: bool,
    cleared: bool,
    was_latched: bool,
    now: u64,
    last_run: u64,
    debounce_secs: u64,
) -> (bool, bool) {
    // Advance the Schmitt latch first (independent of whether we fire).
    let next_latched = if cleared {
        false
    } else if holds {
        true
    } else {
        was_latched
    };
    // FIRE only on a rising edge — never while the condition stays continuously true.
    if !holds || was_latched {
        return (false, next_latched);
    }
    // Rate limit: never re-fire within the debounce cooldown of the last fire.
    let debounce = clamp_debounce(debounce_secs);
    let cooled = last_run == 0 || now.saturating_sub(last_run) >= debounce;
    (cooled, next_latched)
}

/// PURE tripwire scheduler: given the verified `signals` snapshot, the clock, the
/// debounce/hysteresis `ledger` (mutated in place across ticks), the subsystem
/// `master_enabled` flag, and the configured `debounce_secs`, return the CONDITION
/// missions that FIRE this tick — advancing each tripwire's latch. The companion to
/// [`due_missions`] (which handles the TIME triggers); only [`Schedule::Condition`]
/// missions are considered here, so the two paths never double-fire a mission.
///
/// SAFETY (mirrors [`due_missions`]): with `master_enabled == false` NOTHING fires
/// and the ledger is left untouched — a tripwire is INERT when `[standing]` is off
/// (or lockdown forces it off). A disabled INDIVIDUAL mission is also skipped. A
/// tripwire holds NO actuator: firing only launches the bounded mission engine,
/// whose every consequential step still parks behind the confirm gate.
pub fn due_condition_missions<'a>(
    missions: &'a [StandingMission],
    signals: &Signals,
    now: u64,
    ledger: &mut TripwireLedger,
    master_enabled: bool,
    debounce_secs: u64,
) -> Vec<&'a StandingMission> {
    let mut fired = Vec::new();
    if !master_enabled {
        return fired; // inert when the subsystem is off / locked down
    }
    // Keep the latch ledger bounded to the CURRENT tripwire set: drop entries for
    // cancelled/edited condition missions so it can't grow unbounded over uptime.
    let live: std::collections::HashSet<&str> = missions
        .iter()
        .filter(|m| matches!(m.schedule, Schedule::Condition { .. }))
        .map(|m| m.id.as_str())
        .collect();
    ledger.retain_ids(|id| live.contains(id));
    for m in missions {
        if !m.enabled {
            continue;
        }
        let Schedule::Condition { cond } = &m.schedule else {
            continue; // TIME triggers go through due_missions
        };
        let (fire, next) = tripwire_step(
            cond.holds(signals),
            cond.cleared(signals),
            ledger.latched(&m.id),
            now,
            m.last_run,
            debounce_secs,
        );
        ledger.set(&m.id, next);
        if fire {
            fired.push(m);
        }
    }
    fired
}

/// Telemetry payload for ARMING a tripwire — emitted (as `standing.tripwire_armed`)
/// when a CONDITION standing mission is established through the confirmed
/// `standing_create` path. `None` for a time-based (Daily/Interval/OnSignal)
/// mission, which keeps only the plain `standing.created` frame. Pure builder (the
/// live create path emits it), so the arm-frame shape is unit-testable.
pub fn tripwire_armed_telemetry(m: &StandingMission) -> Option<serde_json::Value> {
    match &m.schedule {
        Schedule::Condition { cond } => Some(serde_json::json!({
            "id": m.id,
            "goal": m.goal,
            "condition": cond.describe(),
            "kind": "condition",
        })),
        _ => None,
    }
}

/// Telemetry payload for a tripwire FIRING — emitted (as `standing.tripwire`) by the
/// live tick right before it launches the fired mission's bounded run. Names the
/// condition that tripped so the HUD can show WHY the mission fired. Pure builder.
pub fn tripwire_fired_telemetry(m: &StandingMission) -> serde_json::Value {
    let condition = m.schedule.describe();
    serde_json::json!({
        "id": m.id,
        "goal": m.goal,
        "condition": condition,
    })
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
                "darwin-standing-test-{}-{}.db",
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
        let _ = create(&mem, "secret standing goal about darwin", Schedule::parse("daily"))
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

    // =======================================================================
    // TRIPWIRE — condition triggers (pure predicate + hysteresis/debounce)
    // =======================================================================

    use crate::anticipate::{HealthReading, UpcomingEvent};
    // `Signals` is already in scope via the module-level `use` (through `super::*`).

    /// Build a signal snapshot with a chosen disk-free %, unread count, and an
    /// optional nearest-event lead time (minutes). `present` is irrelevant to
    /// conditions (none read it) but kept true for honesty.
    fn snap(disk_free_pct: Option<f64>, unread: u32, event_min: Option<i64>) -> Signals {
        Signals {
            events: event_min
                .map(|m| {
                    vec![UpcomingEvent {
                        summary: "Sync".into(),
                        minutes_until: m,
                    }]
                })
                .unwrap_or_default(),
            important_unread: unread,
            health: disk_free_pct.map(|d| HealthReading {
                disk_free_pct: d,
                mem_used_pct: 40.0,
            }),
            market: None,
            present: true,
        }
    }

    fn snap_mem(mem_used_pct: f64) -> Signals {
        Signals {
            health: Some(HealthReading {
                disk_free_pct: 80.0,
                mem_used_pct,
            }),
            present: true,
            ..Default::default()
        }
    }

    // ---- the pure predicate: fires / doesn't across representative snapshots ----

    #[test]
    fn condition_predicate_fires_and_doesnt_across_snapshots() {
        // Free disk BELOW 10% (strict).
        let disk = Condition::DiskFreePctBelow { pct: 10.0 };
        assert!(disk.holds(&snap(Some(8.0), 0, None)), "8% < 10% fires");
        assert!(!disk.holds(&snap(Some(10.0), 0, None)), "exactly 10% does NOT fire (strict below)");
        assert!(!disk.holds(&snap(Some(25.0), 0, None)), "25% is fine");
        // No health reading -> never fires (never fabricate a low-disk figure).
        assert!(!disk.holds(&snap(None, 0, None)), "absent health cannot fire");

        // Memory AT/ABOVE 90%.
        let mem = Condition::MemUsedPctAtLeast { pct: 90.0 };
        assert!(mem.holds(&snap_mem(92.0)), "92% >= 90% fires");
        assert!(mem.holds(&snap_mem(90.0)), "exactly 90% fires (at-least)");
        assert!(!mem.holds(&snap_mem(80.0)), "80% is fine");

        // Unread AT/ABOVE 5.
        let unread = Condition::UnreadAtLeast { count: 5 };
        assert!(unread.holds(&snap(None, 5, None)), "5 >= 5 fires");
        assert!(unread.holds(&snap(None, 9, None)), "9 >= 5 fires");
        assert!(!unread.holds(&snap(None, 4, None)), "4 < 5 does not fire");

        // Calendar event WITHIN 15 minutes (and not already past).
        let cal = Condition::CalendarWithinMinutes { minutes: 15 };
        assert!(cal.holds(&snap(None, 0, Some(10))), "10 min out fires");
        assert!(cal.holds(&snap(None, 0, Some(15))), "exactly 15 min out fires");
        assert!(!cal.holds(&snap(None, 0, Some(20))), "20 min out is outside the window");
        assert!(!cal.holds(&snap(None, 0, Some(-5))), "already started -> never fires");
        assert!(!cal.holds(&snap(None, 0, None)), "no event -> never fires");
    }

    // ---- parsing representative tripwire phrases (and NOT stealing others) ----

    #[test]
    fn condition_parse_reads_representative_phrases() {
        assert_eq!(
            Condition::parse("free disk below 10%"),
            Some(Condition::DiskFreePctBelow { pct: 10.0 })
        );
        assert_eq!(
            Condition::parse("disk under 15"),
            Some(Condition::DiskFreePctBelow { pct: 15.0 })
        );
        assert_eq!(
            Condition::parse("memory above 90%"),
            Some(Condition::MemUsedPctAtLeast { pct: 90.0 })
        );
        assert_eq!(
            Condition::parse("unread above 5"),
            Some(Condition::UnreadAtLeast { count: 5 })
        );
        assert_eq!(
            Condition::parse("calendar event within 15m"),
            Some(Condition::CalendarWithinMinutes { minutes: 15 })
        );
        assert_eq!(
            Condition::parse("meeting within 30 minutes"),
            Some(Condition::CalendarWithinMinutes { minutes: 30 })
        );
        // A metric keyword with NO number is not a condition (falls through).
        assert_eq!(Condition::parse("when calendar changes"), None);
        assert_eq!(Condition::parse("on disk"), None);
        // Non-metric phrases are never conditions.
        assert_eq!(Condition::parse("daily at 7"), None);
        assert_eq!(Condition::parse("every 6 hours"), None);
        assert_eq!(Condition::parse("on mail"), None);
    }

    #[test]
    fn schedule_parse_routes_conditions_without_stealing_time_or_signal_phrases() {
        // Condition phrases become Schedule::Condition.
        assert_eq!(
            Schedule::parse("free disk below 10%"),
            Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 10.0 } }
        );
        assert_eq!(
            Schedule::parse("calendar event within 15m"),
            Schedule::Condition { cond: Condition::CalendarWithinMinutes { minutes: 15 } }
        );
        // The EXISTING contract phrases are untouched by the new tripwire branch.
        assert_eq!(Schedule::parse("daily at 7"), Schedule::Daily { hour: 7, minute: 0 });
        assert_eq!(Schedule::parse("every 6 hours"), Schedule::Interval { secs: 6 * 3_600 });
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
    fn condition_describe_and_is_condition() {
        let s = Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 10.0 } };
        assert_eq!(s.describe(), "when free disk drops below 10%");
        assert!(s.is_condition());
        assert!(!Schedule::Daily { hour: 9, minute: 0 }.is_condition());
        assert_eq!(
            Schedule::Condition { cond: Condition::UnreadAtLeast { count: 5 } }.describe(),
            "when unread mail reaches 5"
        );
    }

    // ---- a condition schedule is NEVER clock-due (fires on ONE path only) ----

    #[test]
    fn condition_schedule_is_never_time_due() {
        let s = Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 10.0 } };
        // No clock/last_run/signal-token combination makes a tripwire time-due.
        assert!(!s.is_due(1_000_000, 9, 0, 0, &[]));
        assert!(!s.is_due(1_000_000, 23, 59, 0, &["disk".to_string()]));
        // And the TIME scheduler never selects a condition mission, even master-on.
        let missions = vec![StandingMission::new("watch disk", s)];
        let due = due_missions(&missions, 1_000_000, 9, 0, &[], true);
        assert!(due.is_empty(), "a tripwire must not be selected by the time scheduler");
    }

    // ---- debounce/hysteresis: no re-fire while true / rate-limited ----

    #[test]
    fn tripwire_step_fires_on_rising_edge_only_and_rate_limits() {
        let debounce = 3_600u64;
        let now = 1_000_000u64;
        // Rising edge, never fired (last_run 0) -> FIRES, latches.
        let (fire, latched) = tripwire_step(true, false, false, now, 0, debounce);
        assert!(fire, "rising edge fires");
        assert!(latched, "latch sets on a fire reading");
        // Still true next tick (was_latched) -> NO re-fire (the core anti-spam rule).
        let (fire, latched) = tripwire_step(true, false, true, now + 60, now, debounce);
        assert!(!fire, "no re-fire while still true");
        assert!(latched, "stays latched while still true");
        // Condition CLEARS (receded past the dead-band) -> latch clears, no fire.
        let (fire, latched) = tripwire_step(false, true, true, now + 120, now, debounce);
        assert!(!fire);
        assert!(!latched, "clears once cleared");
        // Re-crosses within the debounce cooldown -> RATE-LIMITED, no fire (but relatches).
        let (fire, latched) = tripwire_step(true, false, false, now + 200, now, debounce);
        assert!(!fire, "a re-cross within the debounce cooldown is rate-limited");
        assert!(latched);
        // Re-crosses AFTER the cooldown -> fires again.
        let (fire, _l) = tripwire_step(true, false, false, now + debounce, now, debounce);
        assert!(fire, "after the cooldown a fresh rising edge fires again");
    }

    #[test]
    fn tripwire_debounce_is_clamped_to_the_floor() {
        // A hand-edited tiny debounce is clamped UP so a flap cannot spam.
        assert_eq!(clamp_debounce(1), MIN_CONDITION_DEBOUNCE_SECS);
        assert_eq!(clamp_debounce(0), MIN_CONDITION_DEBOUNCE_SECS);
        assert_eq!(clamp_debounce(7_200), 7_200);
        // With a 1s configured debounce, a re-cross 60s after the last fire is STILL
        // suppressed (clamped to the 300s floor), not fired.
        let now = 1_000_000u64;
        let (fire, _l) = tripwire_step(true, false, false, now + 60, now, 1);
        assert!(!fire, "the debounce floor still rate-limits a 60s re-cross");
        let (fire, _l) = tripwire_step(true, false, false, now + MIN_CONDITION_DEBOUNCE_SECS, now, 1);
        assert!(fire, "past the clamped floor it may fire");
    }

    #[test]
    fn tripwire_hysteresis_deadband_prevents_flapping_relatch() {
        // Disk tripwire at 10%: fire threshold 10, re-arm threshold 12 (margin 2).
        let cond = Condition::DiskFreePctBelow { pct: 10.0 };
        let ledger_step = |disk: f64, was_latched: bool| {
            let s = snap(Some(disk), 0, None);
            tripwire_step(cond.holds(&s), cond.cleared(&s), was_latched, 2_000_000, 0, 3_600)
        };
        // 9% -> fires, latches.
        let (fire, latched) = ledger_step(9.0, false);
        assert!(fire && latched);
        // 11% is INSIDE the dead-band (>=10 so not firing, <12 so not cleared): the
        // latch HOLDS — no re-arm, hence no possibility of a re-fire from a flap.
        assert!(!cond.holds(&snap(Some(11.0), 0, None)), "11% does not re-fire");
        assert!(!cond.cleared(&snap(Some(11.0), 0, None)), "11% does not re-arm (dead-band)");
        let (fire, latched) = ledger_step(11.0, true);
        assert!(!fire, "dead-band never re-fires");
        assert!(latched, "dead-band holds the latch (no flap)");
        // 13% is past the re-arm margin -> latch clears.
        let (fire, latched) = ledger_step(13.0, true);
        assert!(!fire);
        assert!(!latched, "past the margin the latch clears / re-arms");
    }

    // ---- the tripwire scheduler: guards + selection + ledger advance ----

    #[test]
    fn due_condition_missions_master_off_is_inert() {
        let m = StandingMission::new(
            "free up space",
            Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 10.0 } },
        );
        let missions = vec![m];
        let mut ledger = TripwireLedger::default();
        // Disk is well below the threshold, but the master switch is OFF -> nothing
        // fires and the ledger is untouched (inert when [standing] is off).
        let fired = due_condition_missions(&missions, &snap(Some(3.0), 0, None), 1_000, &mut ledger, false, 3_600);
        assert!(fired.is_empty(), "master off fires no tripwire");
        assert!(!ledger.latched(&missions[0].id), "master off leaves the ledger untouched");
        // With it on it DOES fire.
        let fired = due_condition_missions(&missions, &snap(Some(3.0), 0, None), 1_000, &mut ledger, true, 3_600);
        assert_eq!(fired.len(), 1, "master on: the crossed tripwire fires");
    }

    #[test]
    fn due_condition_missions_skips_disabled_and_advances_the_ledger_no_refire() {
        let mut disabled = StandingMission::new(
            "disabled watcher",
            Schedule::Condition { cond: Condition::UnreadAtLeast { count: 3 } },
        );
        disabled.enabled = false;
        let active = StandingMission::new(
            "flag inbox pileup",
            Schedule::Condition { cond: Condition::UnreadAtLeast { count: 3 } },
        );
        let missions = vec![disabled, active.clone()];
        let mut ledger = TripwireLedger::default();
        let s = snap(None, 7, None); // 7 >= 3 -> the active one holds
        // First eval: only the ENABLED tripwire fires; it latches.
        let fired = due_condition_missions(&missions, &s, 1_000, &mut ledger, true, 3_600);
        assert_eq!(fired.len(), 1, "the disabled tripwire never fires");
        assert_eq!(fired[0].id, active.id);
        assert!(ledger.latched(&active.id), "the fired tripwire is now latched");
        // Second eval, condition STILL true -> no re-fire (edge already passed).
        let fired = due_condition_missions(&missions, &s, 1_060, &mut ledger, true, 3_600);
        assert!(fired.is_empty(), "no re-fire while the condition stays true");
    }

    // ---- ARMING a tripwire routes through the consequential standing_create path --

    /// ESTABLISHING a tripwire is a CONFIRMED action, exactly like any standing
    /// mission: `Schedule::parse` maps the condition phrase to a `Schedule::Condition`,
    /// the establish preview names the condition, and parking `standing_create` yields
    /// a spoken confirm prompt while creating NOTHING until a later yes replays it.
    #[test]
    fn arming_a_tripwire_parks_for_confirmation_through_standing_create() {
        use crate::confirm::{self, PendingConfirmation};
        let _g = confirm::PENDING_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        confirm::clear();

        // Arming a tripwire uses the SAME consequential tool as any standing create.
        assert!(
            confirm::is_consequential_tool("standing_create"),
            "arming a tripwire must be gated (standing_create is consequential)"
        );

        let sched = Schedule::parse("free disk below 10%");
        assert!(sched.is_condition(), "the phrase armed a condition trigger: {sched:?}");
        let preview = establish_preview("free up space when disk is low", &sched);
        assert!(preview.contains("free disk drops below 10%"), "preview names the condition: {preview}");
        assert!(preview.starts_with("[dry run]"));

        let prompt = confirm::park(PendingConfirmation {
            agent: "agent.fury".into(),
            tool: "standing_create".into(),
            input: serde_json::json!({"goal": "free up space when disk is low", "schedule": "free disk below 10%"}),
            allowed: vec!["standing_create".into()],
            preview,
            created_at: std::time::Instant::now(),
            id: String::new(),
        });
        assert!(prompt.contains("confirm"), "prompt invites a yes: {prompt}");
        assert!(!prompt.contains("[dry run]"), "lead-in stripped: {prompt}");

        // A live pending is parked (nothing created) — only a spoken Affirm replays it.
        let taken = confirm::take_live(std::time::Instant::now()).expect("arm parked");
        assert_eq!(taken.tool, "standing_create");
        confirm::clear();
    }

    #[tokio::test]
    async fn arming_a_tripwire_persists_a_condition_schedule_that_roundtrips() {
        let db = TempDb::new("tripwire-roundtrip");
        let mem = Memory::open(&db.0).unwrap();
        let m = create(&mem, "flag inbox pileup", Schedule::parse("unread above 5"))
            .await
            .unwrap();
        assert_eq!(m.schedule, Schedule::Condition { cond: Condition::UnreadAtLeast { count: 5 } });
        let listed = list(&mem).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].schedule, m.schedule, "the condition schedule round-trips");
        assert!(listed[0].schedule.is_condition());
    }

    // ---- a FIRED tripwire mission's consequential step STILL parks ----

    #[tokio::test]
    async fn a_fired_tripwire_run_reasons_and_its_consequential_step_still_parks() {
        let registry = AgentRegistry::canonical();
        // A tripwire mission that, when fired, plans a READ step and a CONSEQUENTIAL
        // (post) step. The condition FIRES it (disk below threshold) via the pure
        // scheduler; the run then re-reasons and the consequential step must PREVIEW.
        let mission = StandingMission::new(
            "when disk is low, review large files and post a cleanup summary",
            Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 10.0 } },
        );
        let missions = vec![mission.clone()];
        let mut ledger = TripwireLedger::default();
        let fired = due_condition_missions(&missions, &snap(Some(4.0), 0, None), 1_000, &mut ledger, true, 3_600);
        assert_eq!(fired.len(), 1, "the disk tripwire fires the mission");

        let planner = MockPlanner {
            plan: vec![
                PlannedTask::say("check the world model for the largest recent files"),
                PlannedTask::say("post a cleanup summary to the team channel"),
            ],
        };
        let dispatcher = MockDispatcher { calls: Mutex::new(Vec::new()) };
        let run = run_one(fired[0], &registry, &planner, &dispatcher, true).await;

        // The run reasoned (both sub-tasks dispatched)...
        assert_eq!(dispatcher.calls.lock().unwrap().len(), 2, "the fired mission re-reasoned");
        // ...but the CONSEQUENTIAL step PREVIEWED — a tripwire cannot auto-post.
        assert!(
            run.report.to_lowercase().contains("dry-run") || run.report.to_lowercase().contains("preview"),
            "the fired tripwire's consequential step must park, not execute: {}",
            run.report
        );
        assert!(
            !run.report.to_lowercase().contains("posted to"),
            "the action must NOT have actually executed: {}",
            run.report
        );
    }

    // ---- telemetry frames on arm + fire ----

    #[test]
    fn tripwire_arm_and_fire_telemetry_carry_the_condition() {
        let m = StandingMission::new(
            "free up space",
            Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 10.0 } },
        );
        let armed = tripwire_armed_telemetry(&m).expect("a condition mission emits an arm frame");
        assert_eq!(armed["kind"], "condition");
        assert_eq!(armed["goal"], "free up space");
        assert_eq!(armed["condition"], "free disk drops below 10%");
        assert_eq!(armed["id"], m.id);

        let fired = tripwire_fired_telemetry(&m);
        assert_eq!(fired["id"], m.id);
        assert_eq!(fired["condition"], "when free disk drops below 10%");

        // A TIME mission carries NO arm frame (it keeps the plain standing.created).
        let time = StandingMission::new("morning review", Schedule::parse("daily at 8"));
        assert!(tripwire_armed_telemetry(&time).is_none(), "a time mission emits no tripwire-arm frame");
    }

    #[test]
    fn the_tripwire_ledger_stays_bounded_to_the_live_mission_set() {
        // REGRESSION: the latch ledger must not accumulate stale entries for
        // cancelled/edited tripwires. due_condition_missions reconciles it against the
        // CURRENT condition missions each eval, so it stays bounded by the active set.
        let mut ledger = TripwireLedger::default();
        let sig = Signals::default();
        // Eval round 1: two live tripwires.
        let a = StandingMission::new("free space", Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 10.0 } });
        let b = StandingMission::new("triage unread", Schedule::Condition { cond: Condition::UnreadAtLeast { count: 5 } });
        let _ = due_condition_missions(&[a.clone(), b.clone()], &sig, 1_000, &mut ledger, true, 3600);
        // Eval round 2: `a` was cancelled and replaced by a NEW tripwire `c` (a
        // distinct content-derived id) — the stale entry for `a` must be dropped.
        let c = StandingMission::new("check backups", Schedule::Condition { cond: Condition::DiskFreePctBelow { pct: 5.0 } });
        let _ = due_condition_missions(&[b.clone(), c.clone()], &sig, 2_000, &mut ledger, true, 3600);
        assert!(!ledger.latched(&a.id) && ledger.latched_count() <= 2, "stale id pruned; bounded to the live set");
    }
}
