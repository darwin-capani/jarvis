//! EDITH's live SIGNAL COLLECTOR — the bridge between the real machine/account
//! state and the PURE evaluator in [`crate::anticipate`].
//!
//! The evaluator (`anticipate::evaluate`) is a function of a verified
//! [`anticipate::Signals`] snapshot. Until now the live tick built that snapshot
//! from system MEMORY pressure alone (disk was hardcoded "plenty", calendar/mail
//! were never read), so only the MemHigh trigger could ever fire. This module
//! assembles the snapshot from the REAL signals EDITH is built to watch:
//!
//!   - DISK: a real free-percentage from the cached telemetry snapshot (now that
//!     the snapshot carries the volume total alongside free bytes). Always
//!     available; no network.
//!   - CALENDAR: upcoming events via [`google_calendar`], mapped to
//!     [`anticipate::UpcomingEvent`] with a lead time computed against an
//!     INJECTED clock. When Google is not connected the read fails and we degrade
//!     to NO events — never a fabricated meeting.
//!   - IMPORTANT-UNREAD MAIL: a bounded `is:unread` count via [`google_gmail`].
//!     Not connected / read failure -> 0, never a fabricated count.
//!   - PRESENCE + MEMORY HEALTH: kept from the existing sources (the caller wires
//!     these — they need no client).
//!   - MARKET: left `None`. There is NO live price/quote source in the codebase
//!     today (Cassandra is a Monte-Carlo MODEL over assumptions, not a quote;
//!     Plaid is balances/transactions, not market moves). Fabricating a delta
//!     would violate the no-fabrication ethos, so market stays honestly UNWIRED —
//!     a future source slots in here.
//!
//! THROTTLE: the calendar/mail reads are NETWORK calls; the tick runs every 60s,
//! far faster than those signals move. So each EXTERNAL signal is cached behind a
//! refresh interval (a [`SignalCache`] keyed off an injected clock): a tick reuses
//! the last value until the interval elapses, then refreshes. A read that fails
//! or isn't connected does NOT poison the cache with a wrong value — it degrades
//! to the absent default (empty events / 0 unread) for that cycle and is retried
//! next interval.
//!
//! HERMETIC TESTABILITY: the pure helpers ([`disk_free_pct`],
//! [`lead_minutes_from_rfc3339`]) and the cache logic are unit-tested directly;
//! the per-source async collectors are GENERIC over the integration
//! [`HttpTransport`] seam, so tests drive them with a `MockTransport` (canned
//! Calendar/Gmail JSON, a mocked token endpoint) and assert the assembled
//! `Signals` — NO network, NO Keychain, NO wall clock (the clock is injected).
//! The top-level [`collect_signals`] wires the REAL reqwest clients and is
//! runtime-only (never run in tests): its network/IO is what's runtime-gated; the
//! LOGIC it composes is what the tests cover.

use crate::anticipate::{HealthReading, Signals, UpcomingEvent};
use crate::integrations::google_calendar::GoogleCalendarClient;
use crate::integrations::google_gmail::GmailClient;
use crate::integrations::HttpTransport;

/// How many upcoming events to pull per refresh. The evaluator only ever
/// surfaces the single nearest within the lead window, but a small look-ahead
/// lets it pick the nearest correctly even when the soonest item is an all-day
/// event we cannot lead-time. Bounded so a refresh stays cheap.
pub const CALENDAR_LOOKAHEAD: u32 = 10;

/// How many unread ids to ask Gmail for per refresh. The evaluator only needs to
/// know whether the count is at/above the unread floor (default 3); a small cap
/// keeps the `messages.list` cheap while still distinguishing "a few" from
/// "none". The returned count saturates at this cap — that is fine: anything at
/// or above the floor crosses the threshold identically.
pub const UNREAD_FETCH_CAP: u32 = 10;

/// Default refresh interval for the EXTERNAL (network) signals: how long a cached
/// calendar/mail value is reused before the next tick refreshes it. The tick is
/// 60s; refreshing these every few minutes keeps the tick cheap without letting
/// the signals go stale enough to matter (a meeting 15 min out is still caught;
/// an unread count a few minutes old is fine for a quiet card).
pub const DEFAULT_REFRESH_SECS: u64 = 5 * 60;

// ---------------------------------------------------------------------------
// Pure helpers (directly unit-tested; no clock, no network)
// ---------------------------------------------------------------------------

/// Disk-free percentage (0-100) from real free/total bytes. `total == 0` (no
/// disk visible, or a bogus reading) yields `None` rather than a divide-by-zero
/// or a fabricated figure — the caller then reports no disk reading instead of a
/// made-up one. The result is clamped to 0..=100 so a momentary free>total race
/// can never produce an absurd percentage.
pub fn disk_free_pct(free_bytes: u64, total_bytes: u64) -> Option<f64> {
    if total_bytes == 0 {
        return None;
    }
    let pct = (free_bytes as f64 / total_bytes as f64) * 100.0;
    Some(pct.clamp(0.0, 100.0))
}

/// A [`HealthReading`] from the cached telemetry snapshot: memory grounded from
/// used/total, disk grounded from free/total when BOTH are present. When the
/// volume total is missing (no disk visible), disk is reported as 100% ("plenty")
/// rather than inventing a low-disk figure — EDITH never surfaces a disk alert it
/// cannot measure. Returns `None` only when there is no snapshot at all.
pub fn health_from_snapshot(snap: &crate::telemetry::SystemSnapshot) -> HealthReading {
    let total = snap.mem_total_bytes.max(1) as f64;
    let mem_used_pct = (snap.mem_used_bytes as f64 / total) * 100.0;
    // Disk: only ground a percentage when free AND total are both known.
    let disk_free_pct = match (snap.disk_free_bytes, snap.disk_total_bytes) {
        (Some(free), Some(total)) => disk_free_pct(free, total).unwrap_or(100.0),
        // No measured total -> cannot ground a low-disk figure; report plenty.
        _ => 100.0,
    };
    HealthReading {
        disk_free_pct,
        mem_used_pct,
    }
}

/// Lead time in minutes from an RFC 3339 `start` to `now` (unix seconds). Returns
/// `None` for:
///   - an all-day event (a bare `YYYY-MM-DD` `date`, no time) — we cannot give it
///     a precise lead time, so the evaluator does not surface it as "in N min";
///   - any string that does not parse as an RFC 3339 instant.
/// A negative result (already started) is RETURNED as-is — the evaluator itself
/// drops past events; this helper only converts time, it does not editorialize.
pub fn lead_minutes_from_rfc3339(start: &str, now: u64) -> Option<i64> {
    let start = start.trim();
    if start.is_empty() {
        return None;
    }
    // chrono's RFC 3339 parser requires a time + offset; an all-day `date`
    // ("2026-06-15") fails to parse here, which is exactly the behavior we want
    // (no precise lead time for an all-day event).
    let dt = chrono::DateTime::parse_from_rfc3339(start).ok()?;
    let start_secs = dt.timestamp();
    let delta_secs = start_secs - now as i64;
    // Integer-division toward zero; 89s -> 1 min, -89s -> -1 min. Minute
    // granularity is all the lead window needs.
    Some(delta_secs / 60)
}

/// Map structured calendar `(summary, start_rfc3339)` pairs into the evaluator's
/// [`UpcomingEvent`] shape, computing each lead time against `now`. Events whose
/// start cannot be lead-timed (all-day / unparseable) are DROPPED — never
/// surfaced with a fabricated time. Pure: no clock read (now is injected), no
/// network. Public so tests can assert the mapping independently of any client.
pub fn events_from_pairs(pairs: &[(String, String)], now: u64) -> Vec<UpcomingEvent> {
    pairs
        .iter()
        .filter_map(|(summary, start)| {
            lead_minutes_from_rfc3339(start, now).map(|minutes_until| UpcomingEvent {
                summary: summary.clone(),
                minutes_until,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// SMARTER BRIEF (#23) — map the verified snapshot into cited brief signals
// ---------------------------------------------------------------------------

/// Project the verified [`Signals`] snapshot into the ranked, cited
/// [`crate::brief::Signal`] list the SMARTER BRIEF (#23) builder consumes — using
/// the SAME relevance thresholds as the EDITH evaluator (the injected `policy`),
/// so the smart brief surfaces exactly what crosses a real floor.
///
/// PURE + GROUNDED + HONEST: every emitted signal cites a REAL origin that is
/// PRESENT in the snapshot — the verbatim calendar event summary, the Gmail
/// `is:unread` query (the real source of the count), the telemetry health
/// subsystem, the market instrument label. It NEVER invents a citation, and an
/// ABSENT source (no events / 0 unread / no health / no market — e.g. an
/// unconnected calendar) contributes NO signal (honestly absent, never padded).
/// The builder then ranks/caps/honest-empties over this list.
///
/// Priority + category map the relevance to the brief axes: an imminent calendar
/// event (within `lead_minutes`) is Urgent/Critical; a low-disk / high-mem
/// reading is Urgent/Critical; mail at/above the floor is Important; a notable
/// market move is Routine/Market. The list is unordered here — the builder sorts.
pub fn brief_signals_from_snapshot(
    signals: &Signals,
    policy: &crate::anticipate::Policy,
) -> Vec<crate::brief::Signal> {
    use crate::brief::{Priority, Signal as BriefSignal};
    use crate::focus::SignalCategory;

    let mut out: Vec<BriefSignal> = Vec::new();

    // CALENDAR — every upcoming event within the lead window (not past). Cited to
    // the event SUMMARY (verbatim from the calendar, the real origin present in
    // the snapshot). The nearest is the most urgent; all within the window are
    // surfaced (the builder ranks + caps to a glance).
    for ev in &signals.events {
        if ev.minutes_until < 0 || ev.minutes_until > policy.lead_minutes {
            continue; // outside the lead window / already started — never surfaced
        }
        let when = match ev.minutes_until {
            0 => "now".to_string(),
            1 => "in 1 minute".to_string(),
            n => format!("in {n} minutes"),
        };
        // An imminent event (<= 5 min) is Critical (it survives even DeepFocus);
        // otherwise an Urgent calendar item.
        let (priority, category) = if ev.minutes_until <= 5 {
            (Priority::Urgent, SignalCategory::Critical)
        } else {
            (Priority::Urgent, SignalCategory::Calendar)
        };
        out.push(BriefSignal::new(
            category,
            priority,
            format!("\"{}\" starts {when}.", ev.summary),
            "calendar",
            // The event summary is the real, verifiable reference present in the
            // verified snapshot (the snapshot does not carry the opaque event id).
            ev.summary.trim().to_string(),
        ));
    }

    // MAIL — important-unread at/above the floor. Cited to the Gmail `is:unread`
    // query, the REAL source of the count (the snapshot carries the count, not
    // per-message ids — citing the query is honest, not a fabricated id).
    if signals.important_unread >= policy.unread_floor && policy.unread_floor > 0 {
        let n = signals.important_unread;
        let noun = if n == 1 { "message" } else { "messages" };
        out.push(BriefSignal::new(
            SignalCategory::Mail,
            Priority::Important,
            format!("{n} important unread {noun} waiting."),
            "gmail",
            "is:unread important",
        ));
    }

    // HEALTH — disk-low / mem-high past their thresholds. A genuinely low disk is
    // Critical (it survives even DeepFocus — a full disk can break the machine);
    // high memory is Urgent/Health. Cited to the telemetry subsystem.
    if let Some(h) = signals.health {
        if h.disk_free_pct <= policy.disk_low_pct {
            out.push(BriefSignal::new(
                SignalCategory::Critical,
                Priority::Urgent,
                format!("Disk space is low: {:.0} percent free.", h.disk_free_pct),
                "memory_health",
                "disk_free_pct",
            ));
        }
        if h.mem_used_pct >= policy.mem_high_pct {
            out.push(BriefSignal::new(
                SignalCategory::Health,
                Priority::Urgent,
                format!("Memory is running high: {:.0} percent used.", h.mem_used_pct),
                "memory_health",
                "mem_used_pct",
            ));
        }
    }

    // MARKET — a notable move past the floor. Routine priority, Market category
    // (a profile quiets this first). Cited to the instrument label, the real
    // origin present in the snapshot.
    if let Some(m) = &signals.market {
        if m.change_pct.abs() >= policy.market_delta_floor && policy.market_delta_floor > 0.0 {
            let dir = if m.change_pct >= 0.0 { "up" } else { "down" };
            out.push(BriefSignal::new(
                SignalCategory::Market,
                Priority::Routine,
                format!("{} is {dir} {:.1} percent.", m.label, m.change_pct.abs()),
                "market",
                m.label.trim().to_string(),
            ));
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Throttle cache (pure logic; injected clock)
// ---------------------------------------------------------------------------

/// A single throttled external signal: the last successfully-fetched value plus
/// the unix-second instant it was fetched. The live loop holds one of these per
/// network signal (calendar, mail) so a tick reuses the cached value until
/// `refresh_secs` have elapsed, then refreshes. Pure bookkeeping — the clock is
/// passed in, never read here.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SignalCache<T> {
    /// (value, fetched_at_unix_secs). `None` before the first successful fetch.
    last: Option<(T, u64)>,
}

impl<T: Clone> SignalCache<T> {
    /// Whether a refresh is due at `now`: true when nothing has been fetched yet,
    /// or when `refresh_secs` have elapsed since the last fetch. `refresh_secs ==
    /// 0` means "always refresh" (no throttling).
    pub fn is_due(&self, now: u64, refresh_secs: u64) -> bool {
        match self.last {
            None => true,
            Some((_, fetched_at)) => now.saturating_sub(fetched_at) >= refresh_secs,
        }
    }

    /// Record a fresh value at `now`, resetting the refresh timer.
    pub fn store(&mut self, value: T, now: u64) {
        self.last = Some((value, now));
    }

    /// The cached value, if any (ignoring staleness — the caller decides whether
    /// to refresh via [`Self::is_due`]).
    pub fn value(&self) -> Option<&T> {
        self.last.as_ref().map(|(v, _)| v)
    }

    /// The throttled value for this tick, given an async `fetch` that produces a
    /// FRESH value. When a refresh is due, `fetch` runs: on success the result is
    /// cached and returned; on failure the LAST cached value is returned (the
    /// cache is not poisoned with a wrong value), or `None` if nothing was ever
    /// cached. When no refresh is due, the cached value is returned without
    /// calling `fetch`. This is the one place the throttle + degrade-silently
    /// policy lives, so the live loop just calls it.
    pub async fn throttled<F, Fut, E>(
        &mut self,
        now: u64,
        refresh_secs: u64,
        fetch: F,
    ) -> Option<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<T, E>>,
    {
        if self.is_due(now, refresh_secs) {
            match fetch().await {
                Ok(v) => {
                    self.store(v.clone(), now);
                    return Some(v);
                }
                // Degrade silently: keep (and return) the last good value if any;
                // otherwise absent. A failed read never overwrites a good cache.
                Err(_) => return self.value().cloned(),
            }
        }
        self.value().cloned()
    }
}

// ---------------------------------------------------------------------------
// Per-source async collectors (GENERIC over the transport seam — hermetic)
// ---------------------------------------------------------------------------

/// Fetch + map upcoming calendar events into [`UpcomingEvent`]s, computing lead
/// times against the injected `now` (unix secs) and `now_rfc3339` (the
/// `timeMin` the client passes through — it reads no wall clock itself). Generic
/// over the transport so a test drives it with a `MockTransport`; the live loop
/// wires the real reqwest client. On ANY read error (including "not connected")
/// the error propagates so the caller's cache can degrade silently — it is NEVER
/// turned into a fabricated event here.
pub async fn fetch_calendar_events<T: HttpTransport>(
    client: &GoogleCalendarClient<T>,
    now_rfc3339: &str,
    now: u64,
) -> crate::integrations::IntegrationResult<Vec<UpcomingEvent>> {
    let pairs = client
        .upcoming_events_structured("primary", now_rfc3339, CALENDAR_LOOKAHEAD)
        .await?;
    Ok(events_from_pairs(&pairs, now))
}

/// Fetch the important-unread mail COUNT (a bounded `is:unread` `messages.list`,
/// no body fan-out). Generic over the transport so a test drives it with a
/// `MockTransport`. On ANY read error (including "not connected") the error
/// propagates so the caller degrades to 0 — never a fabricated count.
pub async fn fetch_unread_count<T: HttpTransport, A: HttpTransport>(
    client: &GmailClient<T, A>,
) -> crate::integrations::IntegrationResult<u32> {
    client.count_messages(UNREAD_FETCH_CAP, Some("is:unread")).await
}

// ---------------------------------------------------------------------------
// The live collector (runtime-only — NOT exercised in tests)
// ---------------------------------------------------------------------------

/// Carries the throttle caches across ticks so the network signals aren't
/// re-fetched every 60s. Owned by the live `anticipation_task`. The pure
/// evaluator stays a function of (signals, clock, fired-state, policy); this is
/// just the IO-throttle bookkeeping that lives at the loop edge.
#[derive(Debug, Default)]
pub struct CollectorState {
    /// Throttled upcoming-events cache (degrades to the last good list, then to
    /// empty, on a failed/not-connected read).
    pub calendar: SignalCache<Vec<UpcomingEvent>>,
    /// Throttled important-unread count cache (degrades to the last good count,
    /// then to 0).
    pub unread: SignalCache<u32>,
}

impl CollectorState {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Assemble the LIVE [`Signals`] snapshot for one anticipation tick.
///
/// RUNTIME-ONLY: this builds REAL reqwest-backed Google clients via `connect()`
/// and issues network reads (throttled). It is wired into `main.rs`
/// `anticipation_task` and is NEVER run in tests — the LOGIC it composes (disk
/// math, lead-time mapping, throttle/degrade) is what the hermetic tests cover
/// through the pure helpers and the generic per-source collectors.
///
/// Inputs the caller already has cheaply: the cached telemetry `snapshot`
/// (memory + disk), `present` (from the recent-interaction stamp), the injected
/// `now`/`now_rfc3339` clock, and the `refresh_secs` throttle interval. Both
/// external reads degrade SILENTLY: not connected / failed -> absent (empty
/// events, 0 unread), never fabricated. Market stays `None` (no live source).
pub async fn collect_signals(
    state: &mut CollectorState,
    snapshot: Option<crate::telemetry::SystemSnapshot>,
    present: bool,
    now: u64,
    now_rfc3339: &str,
    refresh_secs: u64,
) -> Signals {
    let health = snapshot.as_ref().map(health_from_snapshot);

    // Calendar: throttled real read; degrade to empty on any failure.
    let events = state
        .calendar
        .throttled(now, refresh_secs, || async {
            let client = GoogleCalendarClient::connect().await?;
            fetch_calendar_events(&client, now_rfc3339, now).await
        })
        .await
        .unwrap_or_default();

    // Important-unread mail: throttled real count; degrade to 0 on any failure.
    let important_unread = state
        .unread
        .throttled(now, refresh_secs, || async {
            let client = GmailClient::new().await?;
            fetch_unread_count(&client).await
        })
        .await
        .unwrap_or(0);

    Signals {
        events,
        important_unread,
        health,
        // No live market/quote source exists in the codebase — leave it unwired
        // rather than fabricate a delta. A future source slots in here.
        market: None,
        present,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::google_oauth::{GoogleAuth, RefreshTokenStore, TOKEN_ENDPOINT};
    use crate::integrations::testing::MockTransport;
    use crate::integrations::HttpMethod;
    use std::cell::Cell;
    use std::sync::Arc;

    // Fake credential values that, if leaked, would be unmistakable.
    const FAKE_CLIENT_ID: &str = "111-FAKE.apps.googleusercontent.com";
    const FAKE_CLIENT_SECRET: &str = "GOCSPX-FAKE-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "1//FAKE-REFRESH-TOKEN-NEVER-LEAK";
    const FAKE_ACCESS: &str = "ya29.FAKE-ACCESS-TOKEN-NEVER-LEAK";
    /// A fixed RFC 3339 "now" the tests inject — no client reads a wall clock.
    const NOW_RFC3339: &str = "2026-06-14T09:00:00Z";
    /// The matching unix-second "now" for lead-time math (2026-06-14T09:00:00Z).
    const NOW_UNIX: u64 = 1_781_427_600;

    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    fn refresh_ok_json() -> String {
        format!(r#"{{"access_token":"{FAKE_ACCESS}","expires_in":3599,"token_type":"Bearer"}}"#)
    }

    /// A `GoogleAuth<MockTransport>` (by value) whose token endpoint mints
    /// `FAKE_ACCESS` on refresh — `bearer()` works with NO network. Used to build
    /// the Gmail client (which takes the auth handle by value).
    fn connected_auth() -> GoogleAuth<MockTransport> {
        let mock =
            MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_ok_json());
        GoogleAuth::new(
            mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        )
    }

    /// The same connected handle wrapped in `Arc` — the Calendar client borrows
    /// its auth via `Arc`.
    fn connected_auth_arc() -> Arc<GoogleAuth<MockTransport>> {
        Arc::new(connected_auth())
    }

    /// A `GoogleAuth<MockTransport>` with NO refresh token seeded: `bearer()`
    /// returns the friendly "not connected" error, so a client over it behaves
    /// exactly as it would when Google is not connected (no network).
    fn not_connected_auth_arc() -> Arc<GoogleAuth<MockTransport>> {
        Arc::new(GoogleAuth::new(
            MockTransport::new(),
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            "", // empty refresh -> bearer() => not_connected_error
            noop_store(),
        ))
    }

    fn not_connected_auth() -> GoogleAuth<MockTransport> {
        GoogleAuth::new(
            MockTransport::new(),
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            "",
            noop_store(),
        )
    }

    /// Canned `events.list` JSON: one timed event 10 minutes out, one timed event
    /// well outside the lead window, and one all-day event (date only).
    fn events_json() -> String {
        // NOW is 2026-06-14T09:00:00Z. +10 min = 09:10:00Z.
        r#"{"items":[
            {"id":"e1","summary":"Standup","start":{"dateTime":"2026-06-14T09:10:00Z"},"end":{"dateTime":"2026-06-14T09:25:00Z"}},
            {"id":"e2","summary":"Quarterly review","start":{"dateTime":"2026-06-14T15:00:00Z"},"end":{"dateTime":"2026-06-14T16:00:00Z"}},
            {"id":"e3","summary":"Company holiday","start":{"date":"2026-06-15"},"end":{"date":"2026-06-16"}}
        ]}"#
        .to_string()
    }

    fn empty_events_json() -> &'static str {
        r#"{"items":[]}"#
    }

    /// Canned `messages.list` JSON with two unread ids (count only — the
    /// collector does no metadata fan-out).
    fn unread_two_json() -> &'static str {
        r#"{"messages":[{"id":"m1","threadId":"t1"},{"id":"m2","threadId":"t2"}],"resultSizeEstimate":2}"#
    }

    fn unread_zero_json() -> &'static str {
        r#"{"resultSizeEstimate":0}"#
    }

    fn calendar_client(mock: MockTransport) -> GoogleCalendarClient<MockTransport> {
        GoogleCalendarClient::new(mock, connected_auth_arc())
    }

    fn calendar_client_with(
        mock: MockTransport,
        auth: Arc<GoogleAuth<MockTransport>>,
    ) -> GoogleCalendarClient<MockTransport> {
        GoogleCalendarClient::new(mock, auth)
    }

    fn gmail_client(mock: MockTransport) -> GmailClient<MockTransport, MockTransport> {
        GmailClient::with_auth(mock, connected_auth())
    }

    fn gmail_client_with(
        mock: MockTransport,
        auth: GoogleAuth<MockTransport>,
    ) -> GmailClient<MockTransport, MockTransport> {
        GmailClient::with_auth(mock, auth)
    }

    // ---- pure disk math ----------------------------------------------------

    #[test]
    fn disk_free_pct_grounds_a_real_ratio_and_guards_zero() {
        // 100 GiB free of 500 GiB total -> 20%.
        let gib = 1024u64 * 1024 * 1024;
        assert_eq!(disk_free_pct(100 * gib, 500 * gib), Some(20.0));
        // Full disk -> 0%.
        assert_eq!(disk_free_pct(0, 500 * gib), Some(0.0));
        // Empty disk -> 100%.
        assert_eq!(disk_free_pct(500 * gib, 500 * gib), Some(100.0));
        // total == 0 -> None (no divide-by-zero, no fabricated figure).
        assert_eq!(disk_free_pct(0, 0), None);
        // free > total race -> clamped to 100, never absurd.
        assert_eq!(disk_free_pct(600 * gib, 500 * gib), Some(100.0));
    }

    #[test]
    fn health_from_snapshot_grounds_disk_when_total_known_and_falls_back_otherwise() {
        let gib = 1024u64 * 1024 * 1024;
        // Disk total present -> a REAL free percentage flows (not the old 100.0).
        let snap = crate::telemetry::SystemSnapshot {
            cpu_percent: 5.0,
            mem_used_bytes: 8 * gib,
            mem_total_bytes: 16 * gib,
            disk_free_bytes: Some(50 * gib),
            disk_total_bytes: Some(500 * gib),
            uptime_secs: 0,
        };
        let h = health_from_snapshot(&snap);
        assert_eq!(h.disk_free_pct, 10.0, "real free pct flows from free/total");
        assert_eq!(h.mem_used_pct, 50.0);
        // No disk total -> report plenty (100%), never a made-up low figure.
        let no_total = crate::telemetry::SystemSnapshot {
            disk_free_bytes: Some(50 * gib),
            disk_total_bytes: None,
            ..snap
        };
        assert_eq!(health_from_snapshot(&no_total).disk_free_pct, 100.0);
    }

    // ---- pure lead-time mapping -------------------------------------------

    #[test]
    fn lead_minutes_parses_timed_events_and_rejects_all_day_and_junk() {
        // +10 minutes.
        assert_eq!(
            lead_minutes_from_rfc3339("2026-06-14T09:10:00Z", NOW_UNIX),
            Some(10)
        );
        // Already started (negative) is returned as-is; the evaluator drops it.
        assert_eq!(
            lead_minutes_from_rfc3339("2026-06-14T08:55:00Z", NOW_UNIX),
            Some(-5)
        );
        // All-day (date only) -> None (no precise lead time).
        assert_eq!(lead_minutes_from_rfc3339("2026-06-15", NOW_UNIX), None);
        // Junk / empty -> None.
        assert_eq!(lead_minutes_from_rfc3339("not-a-time", NOW_UNIX), None);
        assert_eq!(lead_minutes_from_rfc3339("", NOW_UNIX), None);
    }

    #[test]
    fn events_from_pairs_maps_timed_and_drops_unleadtimeable() {
        let pairs = vec![
            ("Standup".to_string(), "2026-06-14T09:10:00Z".to_string()),
            ("Holiday".to_string(), "2026-06-15".to_string()), // all-day -> dropped
            ("Past".to_string(), "2026-06-14T08:00:00Z".to_string()),
        ];
        let events = events_from_pairs(&pairs, NOW_UNIX);
        assert_eq!(events.len(), 2, "all-day event dropped: {events:?}");
        assert_eq!(events[0].summary, "Standup");
        assert_eq!(events[0].minutes_until, 10);
        assert_eq!(events[1].summary, "Past");
        assert_eq!(events[1].minutes_until, -60);
    }

    // ---- throttle cache (injected clock) ----------------------------------

    #[test]
    fn signal_cache_due_logic_respects_refresh_interval() {
        let mut c: SignalCache<u32> = SignalCache::default();
        // Nothing fetched -> always due.
        assert!(c.is_due(1000, 300));
        c.store(7, 1000);
        // Within the interval -> not due, value reused.
        assert!(!c.is_due(1000 + 299, 300));
        assert_eq!(c.value(), Some(&7));
        // At/after the interval -> due again.
        assert!(c.is_due(1000 + 300, 300));
        // refresh_secs == 0 -> always due (no throttle).
        assert!(c.is_due(1000, 0));
    }

    #[tokio::test]
    async fn throttled_fetches_only_when_due_and_caches_between() {
        let mut c: SignalCache<u32> = SignalCache::default();
        let calls = Cell::new(0u32);
        let fetch = || async {
            calls.set(calls.get() + 1);
            Ok::<u32, anyhow::Error>(42)
        };
        // First tick: due (nothing cached) -> fetches.
        let v = c.throttled(1000, 300, &fetch).await;
        assert_eq!(v, Some(42));
        assert_eq!(calls.get(), 1);
        // Next tick within the interval: reuses cache, NO fetch.
        let v = c.throttled(1000 + 100, 300, &fetch).await;
        assert_eq!(v, Some(42));
        assert_eq!(calls.get(), 1, "throttled within interval");
        // Past the interval: refreshes (one more fetch).
        let v = c.throttled(1000 + 300, 300, &fetch).await;
        assert_eq!(v, Some(42));
        assert_eq!(calls.get(), 2, "refreshed after interval");
    }

    #[tokio::test]
    async fn throttled_failure_degrades_to_last_good_value_then_absent() {
        // With NO prior value, a failed fetch degrades to absent (None).
        let mut c: SignalCache<u32> = SignalCache::default();
        let v = c
            .throttled(1000, 0, || async { Err::<u32, anyhow::Error>(anyhow::anyhow!("down")) })
            .await;
        assert_eq!(v, None, "failed read with no cache -> absent, never fabricated");
        // Seed a good value, then a later failed refresh keeps the last good one
        // (the cache is NOT poisoned by the failure).
        c.store(5, 1000);
        let v = c
            .throttled(2000, 0, || async { Err::<u32, anyhow::Error>(anyhow::anyhow!("down")) })
            .await;
        assert_eq!(v, Some(5), "failed refresh keeps last good value");
    }

    // ---- per-source async collectors (MockTransport — hermetic) -----------

    #[tokio::test]
    async fn fetch_calendar_events_maps_timed_events_with_lead_time() {
        let mock =
            MockTransport::new().on(HttpMethod::Get, "/events?", 200, events_json());
        let client = calendar_client(mock);
        let events = fetch_calendar_events(&client, NOW_RFC3339, NOW_UNIX)
            .await
            .expect("connected read succeeds");
        // Standup (+10) and Quarterly review (+360) map; the all-day Company
        // holiday is dropped (no precise lead time).
        assert_eq!(events.len(), 2, "all-day dropped: {events:?}");
        let standup = events.iter().find(|e| e.summary == "Standup").unwrap();
        assert_eq!(standup.minutes_until, 10, "lead time grounded against injected now");
        assert!(
            events.iter().any(|e| e.summary == "Quarterly review"),
            "timed events both mapped: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.summary == "Company holiday"),
            "all-day event must not get a fabricated lead time: {events:?}"
        );
    }

    #[tokio::test]
    async fn fetch_calendar_events_not_connected_errors_so_caller_degrades() {
        // No refresh token -> bearer() => not connected; no Calendar request is
        // even attempted. The error propagates so the cache degrades to empty.
        let mock = MockTransport::new(); // no /events canned: must not be reached
        let client = calendar_client_with(mock, not_connected_auth_arc());
        let res = fetch_calendar_events(&client, NOW_RFC3339, NOW_UNIX).await;
        assert!(res.is_err(), "not-connected calendar read must error (no fabrication)");
    }

    #[tokio::test]
    async fn fetch_calendar_events_empty_calendar_yields_no_events() {
        let mock =
            MockTransport::new().on(HttpMethod::Get, "/events?", 200, empty_events_json());
        let client = calendar_client(mock);
        let events = fetch_calendar_events(&client, NOW_RFC3339, NOW_UNIX).await.unwrap();
        assert!(events.is_empty(), "empty calendar -> no events");
    }

    #[tokio::test]
    async fn fetch_unread_count_counts_ids_and_filters_to_is_unread_without_fanout() {
        // The canned response is keyed on the FULL "/messages?...q=is%3Aunread"
        // substring: the call ONLY matches (and thus only succeeds) if the
        // request carried the is:unread filter AND hit the bare messages.list
        // path. A per-message metadata GET ("/messages/<id>?format=metadata")
        // would NOT contain "q=is%3Aunread", so any fan-out would miss the canned
        // response and error — making "no fan-out" an end-to-end assertion, not a
        // stubbed one. The exact url-encoding is also covered in google_gmail.rs.
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/messages?maxResults=10&q=is%3Aunread",
            200,
            unread_two_json(),
        );
        let client = gmail_client(mock);
        let count = fetch_unread_count(&client).await.expect("connected read");
        assert_eq!(count, 2, "counts the unread ids (no metadata fan-out)");
    }

    #[tokio::test]
    async fn fetch_unread_count_not_connected_errors_so_caller_degrades_to_zero() {
        let mock = MockTransport::new(); // no /messages canned: must not be reached
        let client = gmail_client_with(mock, not_connected_auth());
        let res = fetch_unread_count(&client).await;
        assert!(res.is_err(), "not-connected mail read must error (caller -> 0)");
    }

    #[tokio::test]
    async fn fetch_unread_count_empty_inbox_is_zero() {
        let mock =
            MockTransport::new().on(HttpMethod::Get, "/messages?", 200, unread_zero_json());
        let client = gmail_client(mock);
        let count = fetch_unread_count(&client).await.unwrap();
        assert_eq!(count, 0, "no unread -> 0, never fabricated");
    }

    // ---- the integrated collector mapping (pure assembly) -----------------

    #[test]
    fn collector_state_defaults_to_empty_caches() {
        let s = CollectorState::new();
        assert!(s.calendar.value().is_none());
        assert!(s.unread.value().is_none());
    }

    // ---- SMARTER BRIEF (#23): snapshot -> cited brief signals --------------

    #[test]
    fn brief_signals_cite_real_origins_present_in_the_snapshot() {
        use crate::anticipate::{MarketDelta, Policy};
        use crate::brief::Priority;
        use crate::focus::SignalCategory;
        let policy = Policy::default();
        let snap = Signals {
            events: vec![
                UpcomingEvent { summary: "1:1 with Pepper".into(), minutes_until: 3 },
                UpcomingEvent { summary: "Far meeting".into(), minutes_until: 999 }, // outside lead -> dropped
            ],
            important_unread: 5, // >= floor 3
            health: Some(HealthReading { disk_free_pct: 4.0, mem_used_pct: 40.0 }), // disk low
            market: Some(MarketDelta { label: "BTC".into(), change_pct: -6.0 }), // past floor
            present: true,
        };
        let sigs = brief_signals_from_snapshot(&snap, &policy);
        // Calendar (the near event only), mail, disk-low, market => 4 signals.
        assert_eq!(sigs.len(), 4, "near event + mail + disk + market: {sigs:?}");
        // The near calendar event is Critical (<=5 min) and cited to its summary.
        let cal = sigs.iter().find(|s| s.category == SignalCategory::Critical && s.text.contains("1:1")).unwrap();
        assert_eq!(cal.citation.source, "calendar");
        assert_eq!(cal.citation.ref_id, "1:1 with Pepper", "cited to the verbatim event summary");
        assert_eq!(cal.priority, Priority::Urgent);
        // Mail cited to the real is:unread query (the source of the count).
        let mail = sigs.iter().find(|s| s.category == SignalCategory::Mail).unwrap();
        assert_eq!(mail.citation.source, "gmail");
        assert!(mail.citation.ref_id.contains("unread"));
        // Disk-low is Critical, cited to the telemetry subsystem.
        let disk = sigs.iter().find(|s| s.text.contains("Disk")).unwrap();
        assert_eq!(disk.category, SignalCategory::Critical);
        assert_eq!(disk.citation.source, "memory_health");
        // Market cited to the instrument label.
        let mkt = sigs.iter().find(|s| s.category == SignalCategory::Market).unwrap();
        assert_eq!(mkt.citation.ref_id, "BTC");
        // The far event was dropped (outside the lead window) — never surfaced.
        assert!(!sigs.iter().any(|s| s.text.contains("Far meeting")));
    }

    #[test]
    fn an_unconnected_or_quiet_snapshot_yields_no_brief_signals_honestly_absent() {
        use crate::anticipate::Policy;
        let policy = Policy::default();
        // Nothing connected / nothing over a floor: no events, 0 unread, healthy,
        // no market. Every source is honestly ABSENT -> no signal is invented.
        let snap = Signals {
            events: vec![],
            important_unread: 0,
            health: Some(HealthReading { disk_free_pct: 80.0, mem_used_pct: 30.0 }),
            market: None,
            present: true,
        };
        let sigs = brief_signals_from_snapshot(&snap, &policy);
        assert!(sigs.is_empty(), "absent/quiet sources contribute nothing: {sigs:?}");
        // And the builder turns that into an honest-empty brief.
        let tuned = crate::focus::apply_profile(
            &crate::focus::FocusProfile::Default,
            &crate::focus::BaseBehavior::default(),
        );
        assert!(crate::brief::build_brief(&sigs, &tuned).empty);
    }

    #[test]
    fn below_floor_mail_and_below_threshold_disk_contribute_no_brief_signal() {
        use crate::anticipate::Policy;
        let policy = Policy::default(); // unread_floor 3, disk_low 10%
        let snap = Signals {
            events: vec![],
            important_unread: 2,                // below floor -> no mail signal
            health: Some(HealthReading { disk_free_pct: 50.0, mem_used_pct: 50.0 }),
            market: None,
            present: true,
        };
        let sigs = brief_signals_from_snapshot(&snap, &policy);
        assert!(sigs.is_empty(), "nothing crosses a floor -> no signal, never fabricated: {sigs:?}");
    }
}
