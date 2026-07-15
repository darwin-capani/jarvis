//! EDITH's anticipation engine: the FIRST time DARWIN surfaces something
//! UNPROMPTED. Because nothing else in the system speaks on its own, this
//! module ships conservative and echo-safe on purpose.
//!
//! The heart of the module is a PURE, deterministic evaluator —
//! [`evaluate`] — that takes a [`Signals`] snapshot (upcoming calendar events
//! with their lead time, the count of important-unread mail, system-health
//! readings, an optional market delta, presence), an injected clock
//! ([`now_secs`] is the only ambient time and lives at the live-loop edge), and
//! the [`FiredState`] of what was last surfaced, and returns a [`Decision`]:
//!   - `Nothing`            — no trigger crossed its threshold, or a guard
//!     suppressed it (debounce / cooldown / quiet hours);
//!   - `Surface(Brief)`     — emit a HUD proactive card only (the SHIPPED
//!     default: `[proactive].speak = false`);
//!   - `Speak(Brief)`       — additionally voice it via the EXISTING speech
//!     path, ONLY when the operator has opted in.
//!
//! The [`Brief`] it composes is GROUNDED: every line traces to a field of the
//! `Signals` snapshot. The evaluator never invents a fact, a number, a time, or
//! an event — exactly the no-fabrication ethos friday/ultron carry. There is no
//! model in this path: a brief is assembled from measured/stored state alone.
//!
//! GUARDS (all unit-tested, all pure):
//!   - relevance threshold — a trigger only fires when its signal crosses a
//!     configured floor (a meeting within `lead_minutes`, unread mail at/above
//!     `unread_floor`, disk/mem past their percentage thresholds, a market move
//!     past `market_delta_floor`). Trivia never surfaces.
//!   - per-trigger cooldown — the same trigger key is not repeated until
//!     `cooldown_secs` have passed since it last fired (don't nag).
//!   - rate limit / debounce — at most `max_per_window` briefs in a rolling
//!     `window_secs`, and never two briefs closer than `min_gap_secs`.
//!   - quiet hours — within the configured `[quiet_start, quiet_end)` local
//!     hour band, EDITH stays silent entirely (it surfaces NOTHING, not even a
//!     card — the user asked not to be interrupted).
//!
//! SAFETY: the evaluator decides `Speak` ONLY when `cfg.speak` is true; with it
//! false (the shipped default) the strongest outcome is `Surface`. EDITH never
//! *acts* — a consequential follow-up the user approves still routes through
//! `integrations::gate()` at the call site, never from here. And when the live
//! loop does speak, it goes through speech.rs `speak()` like every other
//! utterance, so `is_speaking()` / `SPEAKING` / `MUTE_TAIL` / barge all cover
//! it (the loop also refuses to fire while `is_speaking()` — see main.rs).

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// Configuration (the pure knobs; the live loop reads these from Config)
// ---------------------------------------------------------------------------

/// The anticipation policy: thresholds, guards, and the master speak switch.
/// Pure data so the evaluator is a function of (signals, clock, state, policy)
/// alone — no globals, fully unit-testable.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Policy {
    /// Master switch for SPOKEN proactivity. `false` (the shipped default):
    /// EDITH only ever returns `Surface` (a HUD card) — it NEVER speaks
    /// unprompted. `true`: a relevant, un-suppressed trigger may return
    /// `Speak`, voiced through the existing speech path.
    pub speak: bool,
    /// A calendar event this many minutes away (or nearer) is worth surfacing.
    pub lead_minutes: i64,
    /// Important-unread mail at or above this count is worth surfacing.
    pub unread_floor: u32,
    /// Disk-free percentage at or below this is worth surfacing (health).
    pub disk_low_pct: f64,
    /// Memory-used percentage at or above this is worth surfacing (health).
    pub mem_high_pct: f64,
    /// Absolute market move (percent) at or above this is worth surfacing.
    pub market_delta_floor: f64,
    /// Don't repeat the SAME trigger until this many seconds have passed.
    pub cooldown_secs: u64,
    /// Never two briefs closer together than this (debounce).
    pub min_gap_secs: u64,
    /// At most this many briefs within `window_secs` (rate limit).
    pub max_per_window: u32,
    /// The rolling window the rate limit counts over.
    pub window_secs: u64,
    /// Quiet-hours band start (local hour, 0-23 inclusive).
    pub quiet_start: u8,
    /// Quiet-hours band end (local hour, 0-23 exclusive). The band is
    /// `[quiet_start, quiet_end)` and WRAPS midnight when `start > end`
    /// (e.g. 22..7 covers 22,23,0,1,...,6). `start == end` means "no quiet
    /// hours" (an empty band), so the operator can disable it cleanly.
    pub quiet_end: u8,
}

impl Default for Policy {
    /// Sensible, conservative defaults. SPEAK is OFF; the thresholds are tuned
    /// to surface only things that genuinely matter; quiet hours default to a
    /// nighttime band (22:00-07:00 local) so EDITH never wakes the user.
    fn default() -> Self {
        Self {
            speak: false,
            lead_minutes: 15,
            unread_floor: 3,
            disk_low_pct: 10.0,
            mem_high_pct: 90.0,
            market_delta_floor: 3.0,
            cooldown_secs: 30 * 60, // 30 min
            min_gap_secs: 10 * 60,  // 10 min
            max_per_window: 4,
            window_secs: 60 * 60, // 1 hour
            quiet_start: 22,
            quiet_end: 7,
        }
    }
}

impl Policy {
    /// Build the evaluation policy from the loaded `[proactive]` config: the
    /// operator-tunable knobs (speak switch, lead time, unread floor, quiet
    /// band) override the defaults; the remaining guard knobs (cooldown, rate
    /// limit, health thresholds, market floor) keep their conservative code
    /// defaults. Pure, so the live loop and tests build the same Policy the same
    /// way. Public so later rounds (Fury) reuse one conversion.
    pub fn from_config(cfg: &crate::config::ProactiveConfig) -> Self {
        Policy {
            speak: cfg.speak,
            lead_minutes: cfg.lead_minutes,
            unread_floor: cfg.unread_floor,
            quiet_start: cfg.quiet_start,
            quiet_end: cfg.quiet_end,
            ..Policy::default()
        }
    }
}

// ---------------------------------------------------------------------------
// Signals (the verified-only snapshot the evaluator reasons over)
// ---------------------------------------------------------------------------

/// One upcoming calendar event, reduced to what a brief needs. `summary` is the
/// event title (verbatim from the calendar); `minutes_until` is how far away it
/// is (negative = already started/past — never surfaced as "upcoming").
#[derive(Debug, Clone, PartialEq)]
pub struct UpcomingEvent {
    pub summary: String,
    pub minutes_until: i64,
}

/// System-health reading reduced to the two percentages EDITH watches. Built
/// from the cached telemetry [`crate::telemetry::SystemSnapshot`] at the live
/// edge; `None` here when no snapshot exists yet (no health trigger then).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HealthReading {
    /// Free disk as a percentage of the volume (0-100).
    pub disk_free_pct: f64,
    /// Used memory as a percentage of total (0-100).
    pub mem_used_pct: f64,
}

/// One optional market reading: a named instrument and its percent move since
/// some baseline. Verified data only (a connected market source) — EDITH never
/// invents a ticker or a number.
#[derive(Debug, Clone, PartialEq)]
pub struct MarketDelta {
    pub label: String,
    pub change_pct: f64,
}

/// The verified snapshot EDITH evaluates. Every field is measured/stored state:
/// the evaluator composes its brief EXCLUSIVELY from what is present here, so it
/// can never fabricate. Absent data (empty vec / `None`) simply yields no
/// trigger for that source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signals {
    /// Upcoming events, soonest first is not required — the evaluator picks the
    /// nearest within `lead_minutes` itself.
    pub events: Vec<UpcomingEvent>,
    /// Count of important-unread mail (an integration supplies this; 0 = none).
    pub important_unread: u32,
    /// Latest health reading, when a snapshot exists.
    pub health: Option<HealthReading>,
    /// Optional market delta from a connected source.
    pub market: Option<MarketDelta>,
    /// Whether the user is present/at the machine. EDITH does not surface to an
    /// empty room: when `present` is false, [`evaluate`] returns `Nothing`
    /// regardless of triggers (the card would go unseen and a spoken line would
    /// talk to nobody). Defaults to `false` so an uninitialized snapshot is
    /// treated as "nobody here" — fail safe (silent), never fail loud.
    pub present: bool,
}

// ---------------------------------------------------------------------------
// Fired state (what was last surfaced — for cooldown + rate limiting)
// ---------------------------------------------------------------------------

/// Bookkeeping of recently-fired briefs. Held by the live loop across ticks
/// (the evaluator is pure: it READS this and RETURNS the next state via
/// [`Decision::record_onto`], it never mutates a global). Times are unix
/// seconds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FiredState {
    /// (trigger key, unix secs it last fired) — the cooldown ledger.
    pub last_fired: Vec<(TriggerKind, u64)>,
    /// Unix-second timestamps of recent briefs, oldest-to-newest, for the
    /// rolling-window rate limit. Pruned to the window by the live loop.
    pub recent_briefs: Vec<u64>,
}

impl FiredState {
    /// When trigger `kind` last fired, if ever.
    fn last_fired_at(&self, kind: TriggerKind) -> Option<u64> {
        self.last_fired
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, t)| *t)
    }

    /// How many briefs fired within `window_secs` ending at `now`.
    fn briefs_in_window(&self, now: u64, window_secs: u64) -> u32 {
        let cutoff = now.saturating_sub(window_secs);
        self.recent_briefs.iter().filter(|&&t| t >= cutoff).count() as u32
    }

    /// The most recent brief time, if any (for the min-gap debounce).
    fn most_recent_brief(&self) -> Option<u64> {
        self.recent_briefs.iter().copied().max()
    }

    /// Apply a fired brief at `now` for `kind`: stamp the cooldown ledger and
    /// append to the rolling window, pruning entries older than `window_secs`.
    /// This is how the live loop advances state after acting on a `Decision`;
    /// the evaluator itself never calls it.
    pub fn record(&mut self, kind: TriggerKind, now: u64, window_secs: u64) {
        match self.last_fired.iter_mut().find(|(k, _)| *k == kind) {
            Some(slot) => slot.1 = now,
            None => self.last_fired.push((kind, now)),
        }
        self.recent_briefs.push(now);
        let cutoff = now.saturating_sub(window_secs);
        self.recent_briefs.retain(|&t| t >= cutoff);
    }
}

// ---------------------------------------------------------------------------
// Triggers + decision
// ---------------------------------------------------------------------------

/// The kinds of thing EDITH watches. Ordered by priority: when several cross
/// their thresholds in one tick, the EARLIER (more time-sensitive / more
/// important) one wins the single brief that tick. Used as the cooldown key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    /// A calendar event coming up within the lead window.
    Calendar,
    /// Important-unread mail at/above the floor.
    Mail,
    /// Disk free at/below the low threshold.
    DiskLow,
    /// Memory used at/above the high threshold.
    MemHigh,
    /// A market move at/above the delta floor.
    Market,
}

impl TriggerKind {
    /// A short, stable string for telemetry/cooldown logging.
    pub fn as_str(&self) -> &'static str {
        match self {
            TriggerKind::Calendar => "calendar",
            TriggerKind::Mail => "mail",
            TriggerKind::DiskLow => "disk_low",
            TriggerKind::MemHigh => "mem_high",
            TriggerKind::Market => "market",
        }
    }
}

/// A composed, grounded proactive brief — the trigger that produced it plus the
/// one-line text (assembled from signal fields only). The live loop emits this
/// as a HUD card and, when speaking is enabled, voices `text`.
#[derive(Debug, Clone, PartialEq)]
pub struct Brief {
    pub kind: TriggerKind,
    pub text: String,
}

impl Brief {
    /// The HUD proactive-card telemetry payload (no secrets, just the grounded
    /// brief and its trigger key). The live loop emits this under the
    /// `proactive.surface` event so the HUD can render an EDITH card.
    pub fn telemetry(&self) -> Value {
        json!({ "trigger": self.kind.as_str(), "text": self.text })
    }
}

/// What the evaluator decided for one tick.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Nothing to surface (no trigger, or a guard suppressed it).
    Nothing,
    /// Surface a HUD card only — the shipped default (`speak = false`).
    Surface(Brief),
    /// Surface AND speak — only when the operator enabled `[proactive].speak`.
    Speak(Brief),
}

impl Decision {
    /// The brief this decision carries, if any (both `Surface` and `Speak`
    /// carry one; `Nothing` does not).
    pub fn brief(&self) -> Option<&Brief> {
        match self {
            Decision::Nothing => None,
            Decision::Surface(b) | Decision::Speak(b) => Some(b),
        }
    }

    /// Whether this decision should be voiced (only `Speak`).
    pub fn should_speak(&self) -> bool {
        matches!(self, Decision::Speak(_))
    }

    /// Whether this decision should be voiced RIGHT NOW, applying the lockdown
    /// overlay (task #12): EDITH's proactive speech is FORCED off while the
    /// emergency stop is engaged ([`crate::lockdown::is_locked_down`]) — no
    /// unprompted voice when locked, even if `[proactive].speak` is on. With
    /// lockdown OFF (the shipped default) this equals [`Self::should_speak`]
    /// byte-for-byte. This is the chokepoint the live anticipation tick consults
    /// before voicing; the pure [`evaluate`] stays untouched so its decision
    /// tables are unaffected.
    pub fn should_speak_now(&self) -> bool {
        self.should_speak() && !crate::lockdown::is_locked_down()
    }
}

// ---------------------------------------------------------------------------
// Quiet hours (pure)
// ---------------------------------------------------------------------------

/// Is `hour` (local, 0-23) inside the quiet band `[start, end)`? The band wraps
/// midnight when `start > end`. `start == end` is an EMPTY band (quiet hours
/// disabled) so the operator can turn the feature off. Out-of-range inputs are
/// clamped to 0-23 by the modulo so a misconfig can never panic.
pub fn in_quiet_hours(hour: u8, start: u8, end: u8) -> bool {
    let h = hour % 24;
    let s = start % 24;
    let e = end % 24;
    if s == e {
        false // empty band: quiet hours disabled
    } else if s < e {
        h >= s && h < e // same-day band
    } else {
        h >= s || h < e // wraps midnight
    }
}

// ---------------------------------------------------------------------------
// The evaluator (PURE — the unit-tested heart of the module)
// ---------------------------------------------------------------------------

/// Pick the single highest-priority trigger whose signal crosses its relevance
/// threshold, and compose its grounded brief. Returns `None` when no signal is
/// relevant. This is the RELEVANCE gate; cooldown / rate-limit / quiet-hours
/// are applied by [`evaluate`] around it.
fn strongest_trigger(signals: &Signals, policy: &Policy) -> Option<Brief> {
    // 1. Calendar — the nearest event within the lead window (and not past).
    //    Nearest-first so the brief names the most imminent commitment.
    let nearest = signals
        .events
        .iter()
        .filter(|e| e.minutes_until >= 0 && e.minutes_until <= policy.lead_minutes)
        .min_by_key(|e| e.minutes_until);
    if let Some(ev) = nearest {
        let when = match ev.minutes_until {
            0 => "now".to_string(),
            1 => "in 1 minute".to_string(),
            n => format!("in {n} minutes"),
        };
        return Some(Brief {
            kind: TriggerKind::Calendar,
            text: format!("Heads up: \"{}\" starts {when}.", ev.summary),
        });
    }

    // 2. Mail — important-unread at/above the floor.
    if signals.important_unread >= policy.unread_floor && policy.unread_floor > 0 {
        let n = signals.important_unread;
        let noun = if n == 1 { "message" } else { "messages" };
        return Some(Brief {
            kind: TriggerKind::Mail,
            text: format!("You have {n} important unread {noun} waiting."),
        });
    }

    // 3/4. System health — disk first (harder to recover from), then memory.
    if let Some(h) = signals.health {
        if h.disk_free_pct <= policy.disk_low_pct {
            return Some(Brief {
                kind: TriggerKind::DiskLow,
                text: format!("Disk space is low: {:.0} percent free.", h.disk_free_pct),
            });
        }
        if h.mem_used_pct >= policy.mem_high_pct {
            return Some(Brief {
                kind: TriggerKind::MemHigh,
                text: format!("Memory is running high: {:.0} percent used.", h.mem_used_pct),
            });
        }
    }

    // 5. Market — an absolute move at/above the floor.
    if let Some(m) = &signals.market {
        if m.change_pct.abs() >= policy.market_delta_floor && policy.market_delta_floor > 0.0 {
            let dir = if m.change_pct >= 0.0 { "up" } else { "down" };
            return Some(Brief {
                kind: TriggerKind::Market,
                text: format!("{} is {dir} {:.1} percent.", m.label, m.change_pct.abs()),
            });
        }
    }

    None
}

/// THE evaluator. Pure and deterministic: given the verified `signals`, the
/// current `local_hour` and `now` (unix secs) from an injected clock, the
/// `fired` state, and the `policy`, decide what (if anything) to surface this
/// tick. The order of the guards is deliberate and each short-circuits to
/// `Nothing`:
///
///   1. presence — never surface to an empty room.
///   2. quiet hours — within the band, stay fully silent (no card either).
///   3. relevance — no trigger crosses its threshold -> nothing.
///   4. per-trigger cooldown — same trigger fired too recently -> nothing.
///   5. debounce — a brief fired within `min_gap_secs` -> nothing.
///   6. rate limit — already `max_per_window` briefs in the window -> nothing.
///
/// Surviving all guards, the result is `Speak(brief)` iff `policy.speak`, else
/// `Surface(brief)`. The caller advances [`FiredState`] via
/// [`FiredState::record`] only when it ACTS on a non-`Nothing` decision.
pub fn evaluate(
    signals: &Signals,
    local_hour: u8,
    now: u64,
    fired: &FiredState,
    policy: &Policy,
) -> Decision {
    // 1. Presence: an unseen card / a line to nobody is noise.
    if !signals.present {
        return Decision::Nothing;
    }
    // 2. Quiet hours: the user asked not to be interrupted — surface nothing.
    if in_quiet_hours(local_hour, policy.quiet_start, policy.quiet_end) {
        return Decision::Nothing;
    }
    // 3. Relevance: only a threshold-crossing signal is worth a brief.
    let Some(brief) = strongest_trigger(signals, policy) else {
        return Decision::Nothing;
    };
    // 4. Per-trigger cooldown: don't repeat the same alert.
    if let Some(last) = fired.last_fired_at(brief.kind) {
        if now.saturating_sub(last) < policy.cooldown_secs {
            return Decision::Nothing;
        }
    }
    // 5. Debounce: never two briefs closer than the min gap.
    if let Some(last) = fired.most_recent_brief() {
        if now.saturating_sub(last) < policy.min_gap_secs {
            return Decision::Nothing;
        }
    }
    // 6. Rate limit: at most max_per_window briefs in the rolling window.
    if fired.briefs_in_window(now, policy.window_secs) >= policy.max_per_window {
        return Decision::Nothing;
    }

    // Survived every guard. Speak only when the operator opted in; otherwise a
    // HUD card is the strongest outcome (the shipped default).
    if policy.speak {
        Decision::Speak(brief)
    } else {
        Decision::Surface(brief)
    }
}

/// Compose the on-demand brief for the `edith_brief` tool: the SAME grounded
/// composition the evaluator uses, but WITHOUT the suppression guards (the user
/// asked for it explicitly, so cooldown / quiet-hours / rate-limit do not
/// apply) and WITHOUT presence (the user is plainly here — they asked). Returns
/// a plain spoken-friendly sentence; when nothing is relevant it says so
/// honestly rather than inventing intel. Read-only: no side effects, no state.
pub fn on_demand_brief(signals: &Signals, policy: &Policy) -> String {
    match strongest_trigger(signals, policy) {
        Some(brief) => brief.text,
        None => "Nothing on the radar right now, sir. All quiet.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn present_signals() -> Signals {
        Signals {
            present: true,
            ..Default::default()
        }
    }

    /// Helper: a policy with the suppression guards effectively disabled so a
    /// test exercising ONE guard isn't shadowed by the others (each guard is
    /// then pinned individually below).
    fn open_policy() -> Policy {
        Policy {
            cooldown_secs: 0,
            min_gap_secs: 0,
            max_per_window: u32::MAX,
            quiet_start: 0,
            quiet_end: 0, // empty band: quiet hours off
            ..Default::default()
        }
    }

    // ---- relevance truth table: each trigger fires/doesn't at its threshold --

    #[test]
    fn calendar_fires_within_lead_and_not_outside_or_past() {
        let policy = open_policy();
        // Within lead -> Calendar brief.
        let s = Signals {
            events: vec![UpcomingEvent {
                summary: "Standup".into(),
                minutes_until: 10,
            }],
            ..present_signals()
        };
        let d = evaluate(&s, 12, 1000, &FiredState::default(), &policy);
        let b = d.brief().expect("a brief within lead");
        assert_eq!(b.kind, TriggerKind::Calendar);
        assert!(b.text.contains("Standup"), "grounded in the event title: {}", b.text);
        assert!(b.text.contains("10 minutes"), "{}", b.text);

        // Outside the lead window -> nothing.
        let s = Signals {
            events: vec![UpcomingEvent {
                summary: "Standup".into(),
                minutes_until: policy.lead_minutes + 1,
            }],
            ..present_signals()
        };
        assert_eq!(evaluate(&s, 12, 1000, &FiredState::default(), &policy), Decision::Nothing);

        // Already started (negative) -> never "upcoming".
        let s = Signals {
            events: vec![UpcomingEvent {
                summary: "Standup".into(),
                minutes_until: -5,
            }],
            ..present_signals()
        };
        assert_eq!(evaluate(&s, 12, 1000, &FiredState::default(), &policy), Decision::Nothing);
    }

    #[test]
    fn calendar_picks_the_nearest_event() {
        let policy = open_policy();
        let s = Signals {
            events: vec![
                UpcomingEvent { summary: "Later".into(), minutes_until: 14 },
                UpcomingEvent { summary: "Soon".into(), minutes_until: 3 },
            ],
            ..present_signals()
        };
        let b = evaluate(&s, 12, 1000, &FiredState::default(), &policy)
            .brief()
            .cloned()
            .expect("nearest event surfaces");
        assert!(b.text.contains("Soon"), "the nearest event must win: {}", b.text);
        assert!(b.text.contains("3 minutes"), "{}", b.text);
    }

    #[test]
    fn mail_fires_at_or_above_floor_only() {
        let policy = open_policy(); // unread_floor default 3
        // Below floor -> nothing.
        let s = Signals { important_unread: 2, ..present_signals() };
        assert_eq!(evaluate(&s, 12, 1000, &FiredState::default(), &policy), Decision::Nothing);
        // At floor -> Mail brief grounded in the count.
        let s = Signals { important_unread: 3, ..present_signals() };
        let b = evaluate(&s, 12, 1000, &FiredState::default(), &policy)
            .brief()
            .cloned()
            .expect("at floor surfaces");
        assert_eq!(b.kind, TriggerKind::Mail);
        assert!(b.text.contains('3'), "count is grounded: {}", b.text);
        assert!(b.text.contains("messages"), "plural for 3: {}", b.text);
    }

    #[test]
    fn health_fires_on_low_disk_and_high_mem_thresholds() {
        let policy = open_policy();
        // Disk low (<=10%) -> DiskLow.
        let s = Signals {
            health: Some(HealthReading { disk_free_pct: 8.0, mem_used_pct: 40.0 }),
            ..present_signals()
        };
        let b = evaluate(&s, 12, 1000, &FiredState::default(), &policy).brief().cloned().unwrap();
        assert_eq!(b.kind, TriggerKind::DiskLow);
        assert!(b.text.contains("8 percent"), "grounded reading: {}", b.text);

        // Disk fine, mem high (>=90%) -> MemHigh.
        let s = Signals {
            health: Some(HealthReading { disk_free_pct: 50.0, mem_used_pct: 93.0 }),
            ..present_signals()
        };
        let b = evaluate(&s, 12, 1000, &FiredState::default(), &policy).brief().cloned().unwrap();
        assert_eq!(b.kind, TriggerKind::MemHigh);
        assert!(b.text.contains("93 percent"), "{}", b.text);

        // Both fine -> nothing.
        let s = Signals {
            health: Some(HealthReading { disk_free_pct: 50.0, mem_used_pct: 40.0 }),
            ..present_signals()
        };
        assert_eq!(evaluate(&s, 12, 1000, &FiredState::default(), &policy), Decision::Nothing);
    }

    #[test]
    fn market_fires_on_absolute_move_at_or_above_floor() {
        let policy = open_policy(); // market_delta_floor default 3.0
        // Below floor -> nothing.
        let s = Signals {
            market: Some(MarketDelta { label: "BTC".into(), change_pct: 1.5 }),
            ..present_signals()
        };
        assert_eq!(evaluate(&s, 12, 1000, &FiredState::default(), &policy), Decision::Nothing);
        // A down move past the floor -> Market, direction + magnitude grounded.
        let s = Signals {
            market: Some(MarketDelta { label: "BTC".into(), change_pct: -4.2 }),
            ..present_signals()
        };
        let b = evaluate(&s, 12, 1000, &FiredState::default(), &policy).brief().cloned().unwrap();
        assert_eq!(b.kind, TriggerKind::Market);
        assert!(b.text.contains("BTC"), "label grounded: {}", b.text);
        assert!(b.text.contains("down"), "direction grounded: {}", b.text);
        assert!(b.text.contains("4.2 percent"), "magnitude grounded: {}", b.text);
    }

    #[test]
    fn trigger_priority_calendar_beats_mail_beats_health_beats_market() {
        let policy = open_policy();
        // All four cross their thresholds at once; calendar (most time-sensitive)
        // must win the single brief.
        let s = Signals {
            events: vec![UpcomingEvent { summary: "1:1".into(), minutes_until: 5 }],
            important_unread: 9,
            health: Some(HealthReading { disk_free_pct: 1.0, mem_used_pct: 99.0 }),
            market: Some(MarketDelta { label: "SPY".into(), change_pct: 10.0 }),
            present: true,
        };
        let b = evaluate(&s, 12, 1000, &FiredState::default(), &policy).brief().cloned().unwrap();
        assert_eq!(b.kind, TriggerKind::Calendar, "calendar outranks the rest");
    }

    // ---- guards ------------------------------------------------------------

    #[test]
    fn presence_gate_surfaces_nothing_to_an_empty_room() {
        let policy = open_policy();
        let mut s = Signals {
            important_unread: 5,
            ..Default::default()
        };
        s.present = false;
        assert_eq!(
            evaluate(&s, 12, 1000, &FiredState::default(), &policy),
            Decision::Nothing,
            "absent user -> nothing, even with a strong signal"
        );
        // Same signal, now present -> it surfaces.
        s.present = true;
        assert!(matches!(
            evaluate(&s, 12, 1000, &FiredState::default(), &policy),
            Decision::Surface(_)
        ));
    }

    #[test]
    fn quiet_hours_suppress_everything_inside_the_band() {
        // Default band 22..7 (wraps midnight).
        let policy = Policy { cooldown_secs: 0, min_gap_secs: 0, max_per_window: u32::MAX, ..Default::default() };
        let s = Signals { important_unread: 5, ..present_signals() };
        // 23:00 is inside -> nothing.
        assert_eq!(evaluate(&s, 23, 1000, &FiredState::default(), &policy), Decision::Nothing);
        // 03:00 is inside (wrap) -> nothing.
        assert_eq!(evaluate(&s, 3, 1000, &FiredState::default(), &policy), Decision::Nothing);
        // 12:00 is outside -> it surfaces.
        assert!(matches!(
            evaluate(&s, 12, 1000, &FiredState::default(), &policy),
            Decision::Surface(_)
        ));
    }

    #[test]
    fn in_quiet_hours_band_math() {
        // Same-day band 9..17.
        assert!(!in_quiet_hours(8, 9, 17));
        assert!(in_quiet_hours(9, 9, 17));
        assert!(in_quiet_hours(16, 9, 17));
        assert!(!in_quiet_hours(17, 9, 17), "end is exclusive");
        // Wrapping band 22..7.
        assert!(in_quiet_hours(22, 22, 7));
        assert!(in_quiet_hours(23, 22, 7));
        assert!(in_quiet_hours(0, 22, 7));
        assert!(in_quiet_hours(6, 22, 7));
        assert!(!in_quiet_hours(7, 22, 7), "end exclusive on the wrap too");
        assert!(!in_quiet_hours(12, 22, 7));
        // Empty band (start == end) disables quiet hours entirely.
        assert!(!in_quiet_hours(3, 0, 0));
        assert!(!in_quiet_hours(15, 10, 10));
    }

    #[test]
    fn per_trigger_cooldown_suppresses_a_repeat() {
        let policy = Policy { min_gap_secs: 0, max_per_window: u32::MAX, quiet_start: 0, quiet_end: 0, ..Default::default() };
        let s = Signals { important_unread: 5, ..present_signals() };
        let now = 100_000u64;
        // Mail fired 10 minutes ago; cooldown is 30 min -> still suppressed.
        let fired = FiredState {
            last_fired: vec![(TriggerKind::Mail, now - 10 * 60)],
            recent_briefs: vec![],
        };
        assert_eq!(evaluate(&s, 12, now, &fired, &policy), Decision::Nothing, "within cooldown");
        // Past the cooldown -> it fires again.
        let fired = FiredState {
            last_fired: vec![(TriggerKind::Mail, now - policy.cooldown_secs - 1)],
            recent_briefs: vec![],
        };
        assert!(matches!(evaluate(&s, 12, now, &fired, &policy), Decision::Surface(_)));
    }

    #[test]
    fn cooldown_is_per_trigger_not_global() {
        let policy = Policy { min_gap_secs: 0, max_per_window: u32::MAX, quiet_start: 0, quiet_end: 0, ..Default::default() };
        let now = 100_000u64;
        // A calendar event is up; the COOLDOWN ledger only has a recent MAIL —
        // a different trigger must not be suppressed by mail's cooldown.
        let s = Signals {
            events: vec![UpcomingEvent { summary: "Sync".into(), minutes_until: 5 }],
            ..present_signals()
        };
        let fired = FiredState {
            last_fired: vec![(TriggerKind::Mail, now - 60)],
            recent_briefs: vec![],
        };
        assert!(
            matches!(evaluate(&s, 12, now, &fired, &policy), Decision::Surface(_)),
            "mail cooldown must not gag the calendar trigger"
        );
    }

    #[test]
    fn debounce_blocks_two_briefs_within_min_gap() {
        let policy = Policy { cooldown_secs: 0, max_per_window: u32::MAX, quiet_start: 0, quiet_end: 0, ..Default::default() };
        let s = Signals { important_unread: 5, ..present_signals() };
        let now = 100_000u64;
        // A brief fired 2 minutes ago; min_gap is 10 min -> debounced.
        let fired = FiredState { last_fired: vec![], recent_briefs: vec![now - 2 * 60] };
        assert_eq!(evaluate(&s, 12, now, &fired, &policy), Decision::Nothing, "within min gap");
        // Beyond the gap -> fires.
        let fired = FiredState { last_fired: vec![], recent_briefs: vec![now - policy.min_gap_secs - 1] };
        assert!(matches!(evaluate(&s, 12, now, &fired, &policy), Decision::Surface(_)));
    }

    #[test]
    fn rate_limit_caps_briefs_per_window() {
        let policy = Policy {
            cooldown_secs: 0,
            min_gap_secs: 0,
            max_per_window: 2,
            window_secs: 3600,
            quiet_start: 0,
            quiet_end: 0,
            ..Default::default()
        };
        let s = Signals { important_unread: 5, ..present_signals() };
        let now = 100_000u64;
        // Two briefs already in the window -> capped.
        let fired = FiredState { last_fired: vec![], recent_briefs: vec![now - 100, now - 200] };
        assert_eq!(evaluate(&s, 12, now, &fired, &policy), Decision::Nothing, "at the cap");
        // One of them is older than the window -> only one counts -> fires.
        let fired = FiredState { last_fired: vec![], recent_briefs: vec![now - 100, now - 4000] };
        assert!(matches!(evaluate(&s, 12, now, &fired, &policy), Decision::Surface(_)));
    }

    // ---- the OFF flag: HUD-only, never speak -------------------------------

    #[test]
    fn off_flag_surfaces_a_card_and_never_speaks() {
        let policy = open_policy(); // speak defaults to false
        assert!(!policy.speak, "speak must default OFF");
        let s = Signals { important_unread: 5, ..present_signals() };
        let d = evaluate(&s, 12, 1000, &FiredState::default(), &policy);
        match d {
            Decision::Surface(_) => {}
            other => panic!("OFF flag must SURFACE only, got {other:?}"),
        }
        assert!(!d.should_speak(), "OFF flag must never speak");
    }

    #[test]
    fn on_flag_promotes_surface_to_speak() {
        let policy = Policy { speak: true, ..open_policy() };
        let s = Signals { important_unread: 5, ..present_signals() };
        let d = evaluate(&s, 12, 1000, &FiredState::default(), &policy);
        assert!(matches!(d, Decision::Speak(_)), "speak=true promotes to Speak");
        assert!(d.should_speak());
        // ...but suppression still wins: a guard returns Nothing even with speak on.
        let s_quiet = Signals { important_unread: 5, ..present_signals() };
        let speak_quiet = Policy { speak: true, ..Default::default() }; // default quiet band
        assert_eq!(
            evaluate(&s_quiet, 23, 1000, &FiredState::default(), &speak_quiet),
            Decision::Nothing,
            "quiet hours suppress even when speak is on"
        );
    }

    // ---- grounding: a brief contains ONLY signal-derived facts -------------

    #[test]
    fn brief_is_grounded_only_in_signal_fields() {
        let policy = open_policy();
        let s = Signals {
            events: vec![UpcomingEvent { summary: "Board review".into(), minutes_until: 7 }],
            ..present_signals()
        };
        let b = evaluate(&s, 12, 1000, &FiredState::default(), &policy).brief().cloned().unwrap();
        // The only proper-noun content is the event title the signal carried.
        assert!(b.text.contains("Board review"));
        assert!(b.text.contains("7 minutes"));
        // No fabricated specifics: an empty-signal evaluation invents nothing.
        assert_eq!(
            evaluate(&present_signals(), 12, 1000, &FiredState::default(), &policy),
            Decision::Nothing,
            "no signal -> no brief, never a manufactured one"
        );
    }

    // ---- FiredState bookkeeping (what the live loop advances) --------------

    #[test]
    fn record_stamps_cooldown_and_prunes_the_window() {
        let mut fired = FiredState::default();
        let window = 3600u64;
        fired.record(TriggerKind::Mail, 1000, window);
        assert_eq!(fired.last_fired_at(TriggerKind::Mail), Some(1000));
        assert_eq!(fired.briefs_in_window(1000, window), 1);
        // A second, later fire updates mail's stamp and adds to the window.
        fired.record(TriggerKind::Mail, 2000, window);
        assert_eq!(fired.last_fired_at(TriggerKind::Mail), Some(2000), "stamp advances, no dup row");
        assert_eq!(fired.last_fired.len(), 1, "same kind reuses its slot");
        assert_eq!(fired.briefs_in_window(2000, window), 2);
        // A far-future fire prunes both earlier entries out of the window.
        fired.record(TriggerKind::DiskLow, 2000 + window + 10, window);
        assert_eq!(
            fired.briefs_in_window(2000 + window + 10, window),
            1,
            "only the newest brief remains in the window"
        );
    }

    // ---- on-demand brief (edith_brief tool) -------------------------------

    #[test]
    fn policy_from_config_overrides_knobs_and_keeps_guard_defaults() {
        let cfg = crate::config::ProactiveConfig {
            speak: true,
            lead_minutes: 30,
            unread_floor: 5,
            quiet_start: 23,
            quiet_end: 6,
            ..Default::default()
        };
        let p = Policy::from_config(&cfg);
        // Operator-tunable knobs come from config.
        assert!(p.speak);
        assert_eq!(p.lead_minutes, 30);
        assert_eq!(p.unread_floor, 5);
        assert_eq!(p.quiet_start, 23);
        assert_eq!(p.quiet_end, 6);
        // The conservative guard knobs keep the code defaults.
        let d = Policy::default();
        assert_eq!(p.cooldown_secs, d.cooldown_secs);
        assert_eq!(p.min_gap_secs, d.min_gap_secs);
        assert_eq!(p.max_per_window, d.max_per_window);
        assert_eq!(p.window_secs, d.window_secs);
        // Default config -> speak ON (the full-power shipped posture; EDITH voices
        // its brief through the echo-safe speech path, never while already speaking).
        assert!(Policy::from_config(&crate::config::ProactiveConfig::default()).speak);
    }

    #[test]
    fn on_demand_brief_ignores_guards_but_stays_grounded() {
        let policy = Policy::default(); // guards/quiet at defaults — irrelevant on demand
        // With a signal it composes the same grounded line, regardless of quiet
        // hours / cooldown (the user asked).
        let s = Signals {
            events: vec![UpcomingEvent { summary: "Dentist".into(), minutes_until: 12 }],
            ..present_signals()
        };
        let line = on_demand_brief(&s, &policy);
        assert!(line.contains("Dentist"), "grounded on demand: {line}");
        // With nothing relevant it says so honestly, never fabricates.
        let line = on_demand_brief(&Signals::default(), &policy);
        assert!(line.to_lowercase().contains("nothing"), "honest empty brief: {line}");
    }
}
