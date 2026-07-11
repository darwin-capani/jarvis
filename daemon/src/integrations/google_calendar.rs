//! Google Calendar client for agents friday (Daily Intel), pepper (Personal EA)
//! and herald (Meetings).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]) and the Google OAuth2 core
//! ([`crate::integrations::google_oauth`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests) and gets its access token from the
//! SHARED [`GoogleAuth`] handle via [`GoogleAuth::bearer`] at the moment of each
//! send. It NEVER touches the refresh token and never stores or logs the access
//! token — `bearer()` hands back a fresh one for exactly one request, attached as
//! `Authorization: Bearer <token>` and nowhere else.
//!
//! Two tiers of methods, mirroring the foundation's safety model and the round-1
//! clients (github.rs / slack.rs):
//!   * READ (safe — no gate): `list_upcoming_events`, `get_event`. Plain GETs.
//!     `list_upcoming_events` takes the `now` lower bound as an INJECTED string
//!     param (RFC 3339) rather than reading a wall clock inside the client, so the
//!     tests stay hermetic and deterministic.
//!   * CONSEQUENTIAL (gated): `create_event` takes an [`ActionMode`]. In
//!     [`ActionMode::DryRun`] it builds and returns a human-readable preview and
//!     issues NO request; only in [`ActionMode::Execute`] does it POST exactly one
//!     `events.insert`. Call sites obtain `mode` from the foundation's
//!     `gate(confirm)`, so with `[integrations].allow_consequential` false (the
//!     shipped default) `create_event` always previews.
//!
//! Every method returns a concise human-facing `String` while parsing the typed
//! fields it needs from the Calendar JSON (event id/summary/start/end). Non-2xx
//! responses map to friendly, secret-free errors via [`map_status`] (401 →
//! reconnect; 403 → scope/permission; 404 → not found).

use std::sync::Arc;

use serde::Deserialize;
use tracing::info;

use super::google_oauth::GoogleAuth;
use super::{
    status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// Google Calendar API v3 base. All paths are appended to this.
const API_BASE: &str = "https://www.googleapis.com/calendar/v3";
/// The default calendar id when the caller does not name one — the user's own
/// primary calendar, which Google aliases as "primary".
const DEFAULT_CALENDAR: &str = "primary";

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields the agents actually surface are
// decoded. `#[serde(default)]` on the soft fields keeps parsing resilient to the
// many extra keys Calendar returns and to events that omit an optional field.
// ---------------------------------------------------------------------------

/// One event, as returned by `events.list`, `events.get` and `events.insert`.
#[derive(Debug, Clone, Deserialize)]
#[derive(Default)]
struct Event {
    #[serde(default)]
    id: String,
    /// The event title. Google omits this for events with no summary set.
    #[serde(default)]
    summary: String,
    #[serde(default)]
    start: EventTime,
    #[serde(default)]
    end: EventTime,
    /// Browser URL for the event (Google's `htmlLink`).
    #[serde(default, rename = "htmlLink")]
    html_link: String,
}

/// The start/end of an event. Timed events carry `dateTime` (RFC 3339); all-day
/// events carry `date` (YYYY-MM-DD). We surface whichever is present.
#[derive(Debug, Clone, Default, Deserialize)]
struct EventTime {
    #[serde(default, rename = "dateTime")]
    date_time: String,
    #[serde(default)]
    date: String,
}

impl EventTime {
    /// The best human-facing instant for this end of the event: the timed
    /// `dateTime` if present, else the all-day `date`, else empty.
    fn display(&self) -> &str {
        if !self.date_time.is_empty() {
            &self.date_time
        } else {
            &self.date
        }
    }
}

/// The `events.list` response envelope. We only need the `items` array.
#[derive(Debug, Clone, Deserialize)]
struct EventsList {
    #[serde(default)]
    items: Vec<Event>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Google Calendar client bound to a transport and the SHARED [`GoogleAuth`]
/// handle the three Google service clients use.
///
/// Construct with [`GoogleCalendarClient::connect`] (production: resolves the
/// OAuth credentials from the Keychain and wires the real reqwest transport) or,
/// in tests, [`GoogleCalendarClient::new`] (an explicit `GoogleAuth` over a mock
/// + a `MockTransport` for the Calendar calls). The access token is never held on
/// this struct — it is fetched per request from `auth.bearer()` and attached to
/// the outbound `Authorization` header only.
pub struct GoogleCalendarClient<T: HttpTransport> {
    /// The Calendar API transport (production: reqwest; tests: a MockTransport).
    transport: T,
    /// The shared Google auth handle that mints/refreshes access tokens. Held by
    /// `Arc` so Calendar/Gmail/Drive can share ONE handle (one cached token, one
    /// refresh path) rather than each re-resolving the refresh token.
    auth: Arc<GoogleAuth<T>>,
}

/// Custom `Debug` that prints NOTHING about the auth handle's secrets — only that
/// the client is wired. `GoogleAuth`'s own `Debug` already redacts; this keeps a
/// `{:?}` of the Calendar client equally safe.
impl<T: HttpTransport> std::fmt::Debug for GoogleCalendarClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleCalendarClient")
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> GoogleCalendarClient<T> {
    /// Build a client over `transport`, sharing the supplied [`GoogleAuth`]
    /// handle. Used by tests (a `GoogleAuth<MockTransport>` paired with a
    /// `MockTransport` for the Calendar calls) and by any caller that already
    /// holds the shared handle. No secret is read or stored here.
    pub fn new(transport: T, auth: Arc<GoogleAuth<T>>) -> Self {
        Self { transport, auth }
    }

    /// Compose a Calendar request with a FRESH bearer token from the shared auth
    /// handle, attaching it HERE — at the moment of the call — and nowhere else.
    /// The token value is never stored on the client and never logged.
    async fn request(&self, method: HttpMethod, path: &str) -> IntegrationResult<HttpRequest> {
        let token = self.auth.bearer().await?;
        Ok(HttpRequest::new(method, format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {token}")))
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// List upcoming events on `calendar_id` (defaulting to "primary"), starting
    /// from `now` (an RFC 3339 timestamp the CALLER supplies, so the client reads
    /// no wall clock), up to `max` events. Uses `singleEvents=true` +
    /// `orderBy=startTime` so recurring events are expanded and the list is in
    /// chronological order. Read-only. Returns a count plus the first few
    /// "<summary> @ <start>".
    pub async fn list_upcoming_events(
        &self,
        calendar_id: &str,
        now: &str,
        max: u32,
    ) -> IntegrationResult<String> {
        let cal = calendar_or_default(calendar_id);
        let max = max.clamp(1, 50);
        let path = format!(
            "/calendars/{}/events?singleEvents=true&orderBy=startTime&maxResults={}&timeMin={}",
            encode_path_segment(cal),
            max,
            encode_query_value(now),
        );
        let req = self.request(HttpMethod::Get, &path).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing calendar events")?;

        let list: EventsList = serde_json::from_str(&resp.body).map_err(|_| {
            anyhow::anyhow!("listing calendar events returned an unexpected response")
        })?;
        info!(count = list.items.len(), "google_calendar: listed upcoming events");

        if list.items.is_empty() {
            return Ok(format!("No upcoming events on the {cal} calendar."));
        }
        let lines: Vec<String> = list
            .items
            .iter()
            .take(5)
            .map(|e| format!("{} @ {}", event_title(e), e.start.display()))
            .collect();
        let more = list.items.len().saturating_sub(lines.len());
        let mut out = format!(
            "{} upcoming event{} on {cal}: {}",
            list.items.len(),
            if list.items.len() == 1 { "" } else { "s" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    /// STRUCTURED upcoming events for EDITH's anticipation collector: the SAME
    /// `events.list` read as [`Self::list_upcoming_events`], but returning typed
    /// `(summary, start)` pairs instead of a human summary, so the caller can
    /// compute each event's lead time against its OWN injected clock (this client
    /// still reads no wall clock — `now` is supplied). All-day events (a bare
    /// `date`, no `dateTime`) are returned with their `date` as the start string;
    /// the caller decides whether a date-only event has a usable lead time. The
    /// summary falls back to "(no title)" exactly like the human read. Read-only.
    pub async fn upcoming_events_structured(
        &self,
        calendar_id: &str,
        now: &str,
        max: u32,
    ) -> IntegrationResult<Vec<(String, String)>> {
        let cal = calendar_or_default(calendar_id);
        let max = max.clamp(1, 50);
        let path = format!(
            "/calendars/{}/events?singleEvents=true&orderBy=startTime&maxResults={}&timeMin={}",
            encode_path_segment(cal),
            max,
            encode_query_value(now),
        );
        let req = self.request(HttpMethod::Get, &path).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing calendar events")?;

        let list: EventsList = serde_json::from_str(&resp.body).map_err(|_| {
            anyhow::anyhow!("listing calendar events returned an unexpected response")
        })?;
        info!(
            count = list.items.len(),
            "google_calendar: listed upcoming events (structured)"
        );
        Ok(list
            .items
            .iter()
            .map(|e| (event_title(e).to_string(), e.start.display().to_string()))
            .collect())
    }

    /// Fetch one event by id on `calendar_id` (defaulting to "primary").
    /// Read-only. Returns its summary, start and end.
    pub async fn get_event(&self, calendar_id: &str, event_id: &str) -> IntegrationResult<String> {
        let cal = calendar_or_default(calendar_id);
        let path = format!(
            "/calendars/{}/events/{}",
            encode_path_segment(cal),
            encode_path_segment(event_id),
        );
        let req = self.request(HttpMethod::Get, &path).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "fetching the calendar event")?;

        let event: Event = serde_json::from_str(&resp.body).map_err(|_| {
            anyhow::anyhow!("the calendar event response was not in the expected shape")
        })?;
        info!(has_id = !event.id.is_empty(), "google_calendar: fetched event");

        Ok(format!(
            "\"{}\" on {cal}: {} to {}.",
            event_title(&event),
            event.start.display(),
            event.end.display()
        ))
    }

    // -- CONSEQUENTIAL (gated by ActionMode) ---------------------------------

    /// Create an event on `calendar_id` (defaulting to "primary") titled
    /// `summary`, running from `start` to `end` (RFC 3339 timestamps), optionally
    /// inviting `attendees` (email addresses).
    ///
    /// In [`ActionMode::DryRun`] this issues NO request — it returns a preview of
    /// exactly what would be created. In [`ActionMode::Execute`] it POSTs one
    /// `events.insert` and returns the created event's link. Callers obtain `mode`
    /// from the foundation's `gate(confirm)`, so the shipped default (gate OFF)
    /// always previews.
    pub async fn create_event(
        &self,
        calendar_id: &str,
        summary: &str,
        start: &str,
        end: &str,
        attendees: &[String],
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        let cal = calendar_or_default(calendar_id);
        if mode == ActionMode::DryRun {
            info!("google_calendar: dry-run create event (no request issued)");
            let who = if attendees.is_empty() {
                String::new()
            } else {
                format!(" inviting {}", attendees.join(", "))
            };
            return Ok(format!(
                "[dry run] Would create \"{summary}\" on {cal} from {start} to {end}{who}. \
                 Enable consequential actions and confirm to create it."
            ));
        }

        let req = self
            .request(
                HttpMethod::Post,
                &format!("/calendars/{}/events", encode_path_segment(cal)),
            )
            .await?
            .json_body(event_body(summary, start, end, attendees));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "creating the calendar event")?;

        let created: Event = serde_json::from_str(&resp.body).unwrap_or_default();
        info!(has_id = !created.id.is_empty(), "google_calendar: created event");
        let tail = if created.html_link.is_empty() {
            String::new()
        } else {
            format!(" — {}", created.html_link)
        };
        Ok(format!("Created \"{summary}\" on {cal} from {start} to {end}{tail}."))
    }
}

impl GoogleCalendarClient<super::ReqwestTransport> {
    /// Production constructor: build the shared [`GoogleAuth`] handle from the
    /// Keychain (via `GoogleAuth::connect`) and wire a real reqwest transport for
    /// the Calendar calls. Returns the friendly "not connected" error when Google
    /// has not been connected in Settings.
    ///
    /// NOTE: this builds a Calendar-OWNED auth handle. When friday/pepper/herald
    /// share one process-wide handle across Calendar/Gmail/Drive, prefer
    /// [`GoogleCalendarClient::new`] with the shared `Arc<GoogleAuth>` instead, so
    /// all three reuse one cached access token and one refresh path.
    pub async fn connect() -> IntegrationResult<Self> {
        let auth = GoogleAuth::<super::ReqwestTransport>::connect().await?;
        Ok(Self {
            transport: super::ReqwestTransport::new(),
            auth: Arc::new(auth),
        })
    }
}

// ---------------------------------------------------------------------------
// `Default` for `Event` so a malformed-but-2xx insert body degrades gracefully
// instead of failing the whole call after the event was already created.
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// Map a Calendar status to a friendly, secret-free error. 2xx is `Ok`. The
/// task's mappings: 401 → access expired (reconnect); 403 → scope/permission;
/// 404 → not found; 429/5xx → transient. The provider body (which can echo PII)
/// is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    // 401 and 403 both classify as Unauthorized in the foundation, but the task
    // pins DIFFERENT messages for them, so we branch on the raw code first.
    match status {
        401 => return Err(anyhow::anyhow!("Google access expired — reconnect in Settings")),
        403 => {
            return Err(anyhow::anyhow!(
                "{what} was refused — the Google account lacks the calendar scope or permission"
            ))
        }
        _ => {}
    }
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::NotFound => Err(anyhow::anyhow!(
            "{what} failed — the calendar or event was not found"
        )),
        StatusOutcome::RateLimited => Err(anyhow::anyhow!(
            "{what} was rate limited by Google; try again shortly"
        )),
        StatusOutcome::ServerError => Err(anyhow::anyhow!(
            "{what} failed on Google's side; this is usually transient"
        )),
        // Any remaining 4xx / unexpected code leans on the foundation's phrasing.
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

/// The calendar id to use, defaulting a blank one to "primary". A pure helper so
/// every method shares one default rule.
fn calendar_or_default(calendar_id: &str) -> &str {
    let trimmed = calendar_id.trim();
    if trimmed.is_empty() {
        DEFAULT_CALENDAR
    } else {
        trimmed
    }
}

/// An event's display title, falling back to "(no title)" for events Google
/// returned with no `summary` set.
fn event_title(e: &Event) -> &str {
    if e.summary.is_empty() {
        "(no title)"
    } else {
        &e.summary
    }
}

/// The `events.insert` request body. `attendees` (when non-empty) become the
/// Calendar attendees array of `{ "email": ... }` objects; start/end are timed
/// `dateTime` values. Pure, so the exact body shape is unit-testable.
fn event_body(summary: &str, start: &str, end: &str, attendees: &[String]) -> serde_json::Value {
    let mut body = serde_json::json!({
        "summary": summary,
        "start": { "dateTime": start },
        "end": { "dateTime": end },
    });
    if !attendees.is_empty() {
        let list: Vec<serde_json::Value> = attendees
            .iter()
            .map(|email| serde_json::json!({ "email": email }))
            .collect();
        body["attendees"] = serde_json::Value::Array(list);
    }
    body
}

/// Percent-encode a single PATH segment (a calendar id or event id) per RFC 3986:
/// keep the unreserved set literal, percent-encode everything else — notably the
/// `@` in a calendar id like `user@group.calendar.google.com` and any `/` so a
/// crafted id can never escape its segment. Local + pure so URL assembly stays
/// dependency-free (matching google_oauth.rs's own `percent_encode`).
fn encode_path_segment(value: &str) -> String {
    percent_encode(value)
}

/// Percent-encode a query VALUE (e.g. the `timeMin` timestamp, whose `:` and `+`
/// must be encoded). Same rule as a path segment for our inputs.
fn encode_query_value(value: &str) -> String {
    percent_encode(value)
}

/// RFC 3986 unreserved-set percent-encoder. Mirrors google_oauth.rs's encoder so
/// the two agree; kept local to avoid widening that module's surface.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned Calendar JSON (realistic API SHAPE, never fetched). The
// access token comes from a `GoogleAuth<MockTransport>` whose token endpoint is
// also mocked — so a single refresh mints a fake bearer with NO network. No real
// Google round-trip, no real token, no wall clock.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::google_oauth::{GoogleAuth, RefreshTokenStore, TOKEN_ENDPOINT};
    use crate::integrations::testing::MockTransport;

    // Fake credential values that, if leaked, would be unmistakable.
    const FAKE_CLIENT_ID: &str = "111-FAKE.apps.googleusercontent.com";
    const FAKE_CLIENT_SECRET: &str = "GOCSPX-FAKE-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "1//FAKE-REFRESH-TOKEN-NEVER-LEAK";
    /// The access token the mocked token endpoint mints. If it ever surfaced in a
    /// human-facing string it would be unmistakable.
    const FAKE_ACCESS: &str = "ya29.FAKE-ACCESS-TOKEN-NEVER-LEAK";
    /// A fixed RFC 3339 "now" the tests inject — the client reads no wall clock.
    const NOW: &str = "2026-06-14T09:00:00Z";

    /// A no-op refresh-token store: the Calendar tests never trigger a Keychain
    /// write (no `exchange_code`), but `GoogleAuth::new` requires a store.
    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    /// Build a `GoogleAuth<MockTransport>` whose token endpoint returns
    /// `FAKE_ACCESS` on refresh, so `bearer()` mints a token with NO network. The
    /// refresh token is seeded so `bearer()` takes the refresh path.
    fn auth() -> Arc<GoogleAuth<MockTransport>> {
        let refresh_json = format!(
            r#"{{"access_token":"{FAKE_ACCESS}","expires_in":3599,"token_type":"Bearer"}}"#
        );
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_json);
        Arc::new(GoogleAuth::new(
            mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        ))
    }

    /// Build a Calendar client over a Calendar `MockTransport` and a fresh mocked
    /// auth handle.
    fn client(cal_mock: MockTransport) -> GoogleCalendarClient<MockTransport> {
        GoogleCalendarClient::new(cal_mock, auth())
    }

    // -- realistic canned payloads (hand-written from the Calendar API shape) --

    fn events_list_json() -> &'static str {
        // Shape of GET .../events with singleEvents=true&orderBy=startTime: an
        // `items` array, a timed event and an all-day event.
        r#"{
          "kind": "calendar#events",
          "summary": "primary",
          "items": [
            {
              "id": "evt_1",
              "summary": "Standup",
              "status": "confirmed",
              "htmlLink": "https://www.google.com/calendar/event?eid=evt_1",
              "start": {"dateTime": "2026-06-14T10:00:00Z"},
              "end": {"dateTime": "2026-06-14T10:15:00Z"}
            },
            {
              "id": "evt_2",
              "summary": "Company holiday",
              "status": "confirmed",
              "start": {"date": "2026-06-15"},
              "end": {"date": "2026-06-16"}
            }
          ]
        }"#
    }

    fn single_event_json() -> &'static str {
        r#"{
          "kind": "calendar#event",
          "id": "evt_1",
          "summary": "Standup",
          "status": "confirmed",
          "htmlLink": "https://www.google.com/calendar/event?eid=evt_1",
          "start": {"dateTime": "2026-06-14T10:00:00Z"},
          "end": {"dateTime": "2026-06-14T10:15:00Z"}
        }"#
    }

    fn created_event_json() -> &'static str {
        // Shape of the 200 response to POST .../events (events.insert).
        r#"{
          "kind": "calendar#event",
          "id": "evt_new",
          "summary": "Design review",
          "status": "confirmed",
          "htmlLink": "https://www.google.com/calendar/event?eid=evt_new",
          "start": {"dateTime": "2026-06-20T15:00:00Z"},
          "end": {"dateTime": "2026-06-20T16:00:00Z"}
        }"#
    }

    // -- READ: parsing --------------------------------------------------------

    #[tokio::test]
    async fn list_upcoming_events_parses_and_summarizes() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events",
            200,
            events_list_json(),
        );
        let out = client(mock)
            .list_upcoming_events("primary", NOW, 10)
            .await
            .unwrap();
        assert!(out.contains("2 upcoming events on primary"), "got: {out}");
        // Timed event surfaces its dateTime; all-day event surfaces its date.
        assert!(out.contains("Standup @ 2026-06-14T10:00:00Z"), "got: {out}");
        assert!(out.contains("Company holiday @ 2026-06-15"), "got: {out}");
    }

    #[tokio::test]
    async fn list_upcoming_events_defaults_blank_calendar_to_primary() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events",
            200,
            events_list_json(),
        );
        let c = client(mock);
        let out = c.list_upcoming_events("", NOW, 10).await.unwrap();
        assert!(out.contains("on primary"), "got: {out}");
        // The request URL used the "primary" default and the injected timeMin.
        let req = c.transport.last_request();
        assert!(req.url.contains("/calendars/primary/events"), "url: {}", req.url);
        assert!(req.url.contains("singleEvents=true"), "url: {}", req.url);
        assert!(req.url.contains("orderBy=startTime"), "url: {}", req.url);
        // The injected `now` rides as timeMin, percent-encoded.
        assert!(req.url.contains("timeMin=2026-06-14T09%3A00%3A00Z"), "url: {}", req.url);
    }

    #[tokio::test]
    async fn list_upcoming_events_empty_is_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events",
            200,
            r#"{"kind":"calendar#events","items":[]}"#,
        );
        let out = client(mock)
            .list_upcoming_events("primary", NOW, 10)
            .await
            .unwrap();
        assert!(out.contains("No upcoming events"), "got: {out}");
    }

    #[tokio::test]
    async fn get_event_parses_summary_start_end() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events/evt_1",
            200,
            single_event_json(),
        );
        let out = client(mock).get_event("primary", "evt_1").await.unwrap();
        assert!(out.contains("Standup"), "got: {out}");
        assert!(out.contains("2026-06-14T10:00:00Z"), "got: {out}");
        assert!(out.contains("2026-06-14T10:15:00Z"), "got: {out}");
    }

    /// A calendar id with an `@` (a shared/secondary calendar) is percent-encoded
    /// into its path segment so it can't break the URL.
    #[tokio::test]
    async fn get_event_encodes_calendar_id_with_at_sign() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            // The matcher sees the ENCODED url; %40 is the encoded '@'.
            "/calendars/team%40group.calendar.google.com/events/evt_1",
            200,
            single_event_json(),
        );
        let c = client(mock);
        c.get_event("team@group.calendar.google.com", "evt_1")
            .await
            .unwrap();
        let req = c.transport.last_request();
        assert!(req.url.contains("%40"), "calendar id must be encoded: {}", req.url);
        assert!(!req.url.contains("@"), "raw @ must not leak into the URL: {}", req.url);
    }

    // -- READ: header SHAPE on the recorded request (never the token) ---------

    #[tokio::test]
    async fn read_request_carries_a_bearer_auth_header() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events",
            200,
            events_list_json(),
        );
        let c = client(mock);
        c.list_upcoming_events("primary", NOW, 5).await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        // Presence of auth — NOT its value.
        assert!(req.has_header("authorization"), "bearer auth header attached");
        // No body on a read.
        assert!(req.body.is_none());
    }

    // -- CONSEQUENTIAL: DryRun issues NO request ------------------------------

    #[tokio::test]
    async fn create_event_dry_run_issues_no_request() {
        // Give the auth handle a mock with NO token endpoint registered: if DryRun
        // tried to mint a bearer it would error, so a clean Ok here proves DryRun
        // never even reached for an access token.
        let no_token_auth = Arc::new(GoogleAuth::new(
            MockTransport::new(),
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        ));
        let cal_mock = MockTransport::new(); // no canned Calendar responses on purpose
        let c = GoogleCalendarClient::new(cal_mock, no_token_auth);
        let out = c
            .create_event(
                "primary",
                "Design review",
                "2026-06-20T15:00:00Z",
                "2026-06-20T16:00:00Z",
                &["alice@example.com".to_string()],
                ActionMode::DryRun,
            )
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("Design review"), "got: {out}");
        assert!(out.contains("alice@example.com"), "got: {out}");
        // The crux: NO Calendar request was issued in DryRun. (And because the
        // auth handle's token endpoint is unmocked, the clean Ok above also proves
        // DryRun never minted an access token — a refresh would have errored.)
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not touch the Calendar transport"
        );
    }

    // -- CONSEQUENTIAL: Execute issues EXACTLY one events.insert --------------

    #[tokio::test]
    async fn create_event_execute_posts_one_insert_with_right_body() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/calendars/primary/events",
            200,
            created_event_json(),
        );
        let c = client(mock);
        let out = c
            .create_event(
                "primary",
                "Design review",
                "2026-06-20T15:00:00Z",
                "2026-06-20T16:00:00Z",
                &["alice@example.com".to_string(), "bob@example.com".to_string()],
                ActionMode::Execute,
            )
            .await
            .unwrap();
        assert!(out.contains("Created \"Design review\""), "got: {out}");
        assert!(
            out.contains("https://www.google.com/calendar/event?eid=evt_new"),
            "got: {out}"
        );

        // Exactly one Calendar request, and it is the events.insert with our body.
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.ends_with("/calendars/primary/events"), "url: {}", req.url);
        let body = req.body.as_ref().expect("insert must carry a JSON body");
        assert_eq!(body["summary"], "Design review");
        assert_eq!(body["start"]["dateTime"], "2026-06-20T15:00:00Z");
        assert_eq!(body["end"]["dateTime"], "2026-06-20T16:00:00Z");
        assert_eq!(body["attendees"][0]["email"], "alice@example.com");
        assert_eq!(body["attendees"][1]["email"], "bob@example.com");
        // Auth attached, value never asserted.
        assert!(req.has_header("authorization"));
    }

    #[tokio::test]
    async fn create_event_without_attendees_omits_the_array() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/calendars/primary/events",
            200,
            created_event_json(),
        );
        let c = client(mock);
        c.create_event(
            "primary",
            "Solo block",
            "2026-06-20T15:00:00Z",
            "2026-06-20T16:00:00Z",
            &[],
            ActionMode::Execute,
        )
        .await
        .unwrap();
        let req = c.transport.last_request();
        let body = req.body.as_ref().unwrap();
        assert!(body.get("attendees").is_none(), "no attendees key when none given");
    }

    // -- error mapping --------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_401_maps_to_reconnect() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events",
            401,
            r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#,
        );
        let err = client(mock)
            .list_upcoming_events("primary", NOW, 5)
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Google access expired — reconnect in Settings", "got: {err}");
    }

    #[tokio::test]
    async fn forbidden_403_maps_to_scope_permission() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events",
            403,
            r#"{"error":{"code":403,"message":"Insufficient Permission"}}"#,
        );
        let err = client(mock)
            .list_upcoming_events("primary", NOW, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("scope or permission"), "got: {err}");
    }

    #[tokio::test]
    async fn not_found_404_maps_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events/missing",
            404,
            r#"{"error":{"code":404,"message":"Not Found"}}"#,
        );
        let err = client(mock)
            .get_event("primary", "missing")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/calendars/primary/events",
            503,
            "upstream down",
        );
        let err = client(mock)
            .list_upcoming_events("primary", NOW, 5)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- the access TOKEN never leaks ----------------------------------------

    /// The access token (minted by the mocked refresh) must never appear in the
    /// client's `Debug` output, in any returned outcome string, or in any mapped
    /// error. We drive a representative slice of the surface and scan every
    /// produced string for the token.
    #[tokio::test]
    async fn access_token_never_appears_in_any_produced_output() {
        // Debug of the client (and, transitively, the auth handle).
        let dbg = format!("{:?}", client(MockTransport::new()));
        assert!(!dbg.contains(FAKE_ACCESS), "Debug leaked the access token: {dbg}");
        assert!(!dbg.contains(FAKE_REFRESH), "Debug leaked the refresh token: {dbg}");
        assert!(!dbg.contains(FAKE_CLIENT_SECRET), "Debug leaked the client secret: {dbg}");

        // Success outcome strings (list + create), plus an error path.
        let ok_mock = MockTransport::new()
            .on(HttpMethod::Get, "/calendars/primary/events", 200, events_list_json())
            .on(HttpMethod::Post, "/calendars/primary/events", 200, created_event_json());
        let c = client(ok_mock);
        let listed = c.list_upcoming_events("primary", NOW, 5).await.unwrap();
        let created = c
            .create_event(
                "primary",
                "x",
                "2026-06-20T15:00:00Z",
                "2026-06-20T16:00:00Z",
                &[],
                ActionMode::Execute,
            )
            .await
            .unwrap();

        let err_mock =
            MockTransport::new().on(HttpMethod::Get, "/calendars/primary/events", 401, "{}");
        let err = client(err_mock)
            .list_upcoming_events("primary", NOW, 5)
            .await
            .unwrap_err()
            .to_string();

        for s in [&listed, &created, &err] {
            assert!(!s.contains(FAKE_ACCESS), "output leaked the access token: {s}");
        }
        // And the token is never echoed into a URL — only into the auth header.
        for req in c.transport.requests() {
            assert!(!req.url.contains(FAKE_ACCESS), "token must not be in a URL");
            if let Some(body) = &req.body {
                assert!(
                    !body.to_string().contains(FAKE_ACCESS),
                    "token must not be in a body"
                );
            }
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn calendar_or_default_defaults_blank() {
        assert_eq!(calendar_or_default(""), "primary");
        assert_eq!(calendar_or_default("   "), "primary");
        assert_eq!(calendar_or_default("work@x.com"), "work@x.com");
    }

    #[test]
    fn event_body_shape_with_and_without_attendees() {
        let with = event_body("Sync", "S", "E", &["a@x.com".to_string()]);
        assert_eq!(with["summary"], "Sync");
        assert_eq!(with["start"]["dateTime"], "S");
        assert_eq!(with["end"]["dateTime"], "E");
        assert_eq!(with["attendees"][0]["email"], "a@x.com");

        let without = event_body("Sync", "S", "E", &[]);
        assert!(without.get("attendees").is_none());
    }

    #[test]
    fn percent_encode_keeps_unreserved_encodes_the_rest() {
        assert_eq!(percent_encode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(percent_encode("user@host"), "user%40host");
        assert_eq!(percent_encode("2026-06-14T09:00:00Z"), "2026-06-14T09%3A00%3A00Z");
        assert_eq!(percent_encode("a/b"), "a%2Fb");
    }

    #[test]
    fn event_title_falls_back_when_blank() {
        let mut e = Event::default();
        assert_eq!(event_title(&e), "(no title)");
        e.summary = "Lunch".to_string();
        assert_eq!(event_title(&e), "Lunch");
    }
}
