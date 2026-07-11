//! WHOOP biometrics client for agent "vitalis" (Health & Biometrics).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]) and the generic OAuth2 core
//! ([`crate::integrations::oauth2`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests) and borrows a shared
//! [`ProviderAuth`] handle (configured with the [`WHOOP`] provider) for its
//! bearer. The client holds NO secret of its own — every request's access token
//! comes from `auth.bearer()` at the moment of the send, so the token is never
//! logged, never stored on the transport, never put in an error or a `Debug`
//! field; only that an auth handle is attached is ever recorded.
//!
//! READ-ONLY by construction — Vitalis reads the body's signals, it never
//! changes WHOOP data, so there is NO consequential surface and nothing routes
//! through the foundation gate. Three reads, all plain GETs:
//!   * [`WhoopClient::latest_recovery`] — most recent recovery: score %, HRV
//!     (RMSSD, ms), resting heart rate (bpm).
//!   * [`WhoopClient::latest_sleep`] — most recent sleep: performance %, total
//!     time asleep.
//!   * [`WhoopClient::latest_strain`] — most recent cycle's day strain (0–21).
//!
//! Every method returns a concise human-facing `String` — what Vitalis would say
//! — while parsing only the typed fields it needs. An empty `records` array is a
//! friendly "no recent data" message, never a fabricated number. Non-2xx
//! responses map to friendly, secret-free errors via [`map_status`]: 401 ->
//! reconnect, 403 -> scope (the WHOOP app is missing the read scope), 429 -> rate
//! limited. The provider body (which echoes the user's biometrics) is never
//! included in an error.
//!
//! HONESTY: this is the ONLY health data source wired here. There is no Apple
//! Health / HealthKit path — that data is iOS/watchOS only and is not reachable
//! from macOS — so the client neither reads nor claims it.

use serde::Deserialize;
use tracing::info;

use super::oauth2::{ProviderAuth, WHOOP};
use super::{status_outcome, HttpMethod, HttpRequest, HttpTransport, IntegrationResult, StatusOutcome};

/// WHOOP API base (the developer REST API, v1). All paths are appended to this.
const API_BASE: &str = "https://api.prod.whoop.com/developer/v1";

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields Vitalis actually needs are decoded.
// `#[serde(default)]` on the soft fields keeps parsing resilient to the many
// extra keys WHOOP returns and to objects that omit an optional field (e.g. a
// record still scoring has no `score` yet).
// ---------------------------------------------------------------------------

/// The recovery score block of a recovery record (`score`).
#[derive(Debug, Clone, Deserialize, Default)]
struct RecoveryScore {
    /// Recovery as a percentage 0–100.
    #[serde(default)]
    recovery_score: f64,
    /// Heart-rate variability (RMSSD) in milliseconds.
    #[serde(default)]
    hrv_rmssd_milli: f64,
    /// Resting heart rate in beats per minute.
    #[serde(default)]
    resting_heart_rate: f64,
}

/// One recovery record (only the score block is read).
#[derive(Debug, Clone, Deserialize, Default)]
struct RecoveryRecord {
    #[serde(default)]
    score: Option<RecoveryScore>,
}

/// The `{"records": [ ... ]}` envelope WHOOP returns for a collection read. The
/// array is absent/empty when there are no records, so it defaults to empty.
#[derive(Debug, Clone, Deserialize, Default)]
struct RecoveryPage {
    #[serde(default)]
    records: Vec<RecoveryRecord>,
}

/// The sleep score block of a sleep activity record (`score`).
#[derive(Debug, Clone, Deserialize, Default)]
struct SleepScore {
    /// Sleep performance as a percentage 0–100.
    #[serde(default)]
    sleep_performance_percentage: f64,
    #[serde(default)]
    stage_summary: SleepStageSummary,
}

/// The portion of WHOOP's sleep `stage_summary` Vitalis reads: total in-bed and
/// awake time, in milliseconds, from which time-asleep is derived.
#[derive(Debug, Clone, Deserialize, Default)]
struct SleepStageSummary {
    #[serde(default)]
    total_in_bed_time_milli: f64,
    #[serde(default)]
    total_awake_time_milli: f64,
}

/// One sleep activity record (only the score block is read).
#[derive(Debug, Clone, Deserialize, Default)]
struct SleepRecord {
    #[serde(default)]
    score: Option<SleepScore>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct SleepPage {
    #[serde(default)]
    records: Vec<SleepRecord>,
}

/// The score block of a cycle record (`score`) — only day strain is read.
#[derive(Debug, Clone, Deserialize, Default)]
struct CycleScore {
    /// Day strain on WHOOP's 0–21 logarithmic scale.
    #[serde(default)]
    strain: f64,
}

/// One physiological-cycle record (only the score block is read).
#[derive(Debug, Clone, Deserialize, Default)]
struct CycleRecord {
    #[serde(default)]
    score: Option<CycleScore>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct CyclePage {
    #[serde(default)]
    records: Vec<CycleRecord>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// WHOOP client bound to a transport and a shared [`ProviderAuth`] handle
/// configured for the [`WHOOP`] provider.
///
/// Construct with [`WhoopClient::new`] (production: a `ReqwestTransport` API
/// client paired with a connected `ProviderAuth`) or, in tests,
/// [`WhoopClient::with_auth`] (a `MockTransport` API client + a `ProviderAuth`
/// wired over its own `MockTransport`). The client holds NO secret of its own —
/// every request's bearer comes from `auth.bearer()` at the moment of the send,
/// so there is nothing to redact in `Debug` beyond noting the handle is present.
pub struct WhoopClient<T: HttpTransport, A: HttpTransport> {
    transport: T,
    auth: ProviderAuth<A>,
}

/// `Debug` notes only that an auth handle is attached — it never prints any token
/// (the `ProviderAuth` `Debug` itself redacts all secrets, but we keep this
/// minimal so a `{:?}` of the WHOOP client can't widen the surface).
impl<T: HttpTransport, A: HttpTransport> std::fmt::Debug for WhoopClient<T, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WhoopClient")
            .field("auth_attached", &true)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport, A: HttpTransport> WhoopClient<T, A> {
    /// Build a client over `transport`, taking ownership of a shared
    /// [`ProviderAuth`] handle. Used by tests (mock transports) and by
    /// [`WhoopClient::new`] internally. No secret is resolved here — the bearer
    /// is fetched per request from `auth`.
    pub fn with_auth(transport: T, auth: ProviderAuth<A>) -> Self {
        Self { transport, auth }
    }

    /// Compose a request with the WHOOP-standard Authorization header, attaching
    /// the Bearer token HERE — fetched fresh from `auth` at the moment of the
    /// call — and nowhere else. The token never lands on the transport or in a
    /// log.
    async fn request(&self, method: HttpMethod, path: &str) -> IntegrationResult<HttpRequest> {
        let token = self.auth.bearer().await?;
        Ok(HttpRequest::new(method, format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {token}")))
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// Most recent recovery (`GET /recovery?limit=1`). Read-only. Returns
    /// recovery score %, HRV (RMSSD, ms) and resting heart rate; an empty page is
    /// a friendly "no recent recovery" message rather than a zeroed-out number.
    pub async fn latest_recovery(&self) -> IntegrationResult<String> {
        let req = self.request(HttpMethod::Get, "/recovery?limit=1").await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading your recovery")?;
        let page: RecoveryPage = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("reading your recovery returned an unexpected response"))?;
        let Some(score) = page.records.into_iter().find_map(|r| r.score) else {
            info!(has_data = false, "whoop: latest recovery (no data)");
            return Ok("No recent recovery data from WHOOP yet.".to_string());
        };
        info!(has_data = true, "whoop: latest recovery");
        Ok(format!(
            "Recovery {}%, HRV {} ms, resting heart rate {} bpm.",
            round0(score.recovery_score),
            round0(score.hrv_rmssd_milli),
            round0(score.resting_heart_rate),
        ))
    }

    /// Most recent sleep (`GET /activity/sleep?limit=1`). Read-only. Returns sleep
    /// performance % and total time asleep (in-bed minus awake); an empty page is
    /// a friendly "no recent sleep" message rather than a fabricated number.
    pub async fn latest_sleep(&self) -> IntegrationResult<String> {
        let req = self.request(HttpMethod::Get, "/activity/sleep?limit=1").await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading your sleep")?;
        let page: SleepPage = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("reading your sleep returned an unexpected response"))?;
        let Some(score) = page.records.into_iter().find_map(|r| r.score) else {
            info!(has_data = false, "whoop: latest sleep (no data)");
            return Ok("No recent sleep data from WHOOP yet.".to_string());
        };
        let asleep_milli =
            (score.stage_summary.total_in_bed_time_milli - score.stage_summary.total_awake_time_milli).max(0.0);
        info!(has_data = true, "whoop: latest sleep");
        Ok(format!(
            "Sleep performance {}%, {} asleep.",
            round0(score.sleep_performance_percentage),
            format_duration(asleep_milli),
        ))
    }

    /// Most recent cycle's day strain (`GET /cycle?limit=1`). Read-only. Returns
    /// strain on WHOOP's 0–21 scale; an empty page is a friendly "no recent
    /// strain" message rather than a zeroed-out number.
    pub async fn latest_strain(&self) -> IntegrationResult<String> {
        let req = self.request(HttpMethod::Get, "/cycle?limit=1").await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading your strain")?;
        let page: CyclePage = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("reading your strain returned an unexpected response"))?;
        let Some(score) = page.records.into_iter().find_map(|r| r.score) else {
            info!(has_data = false, "whoop: latest strain (no data)");
            return Ok("No recent strain data from WHOOP yet.".to_string());
        };
        info!(has_data = true, "whoop: latest strain");
        Ok(format!(
            "Day strain {} (on WHOOP's 0 to 21 scale).",
            round1(score.strain),
        ))
    }
}

impl WhoopClient<super::ReqwestTransport, super::ReqwestTransport> {
    /// Production constructor: pair the real reqwest transport for WHOOP's API
    /// with a connected [`ProviderAuth`] handle for the [`WHOOP`] provider (which
    /// itself resolves the OAuth credentials + refresh token from the Keychain
    /// and wires its own reqwest transport). Returns the OAuth core's friendly
    /// "not connected" error when WHOOP has not been connected in Settings.
    pub async fn new() -> IntegrationResult<Self> {
        let auth = ProviderAuth::<super::ReqwestTransport>::connect(WHOOP).await?;
        Ok(Self::with_auth(super::ReqwestTransport::new(), auth))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// Round a non-negative figure to a whole number for a spoken summary. Pure.
fn round0(v: f64) -> i64 {
    v.round() as i64
}

/// Round to one decimal place (strain is reported with one decimal). Pure.
fn round1(v: f64) -> String {
    format!("{:.1}", v)
}

/// Render a duration in milliseconds as "Hh Mm" (or "Mm" under an hour). Pure.
fn format_duration(milli: f64) -> String {
    let total_min = (milli / 60_000.0).round() as i64;
    let h = total_min / 60;
    let m = total_min % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

/// Map a WHOOP API status to a friendly, secret-free error. 2xx is `Ok`. The
/// task's mappings: 401 -> reconnect; 403 -> the WHOOP app is missing the read
/// scope; 429 -> rate limited. The provider body (which echoes the user's
/// biometrics) is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    match status {
        401 => Err(anyhow::anyhow!("WHOOP access expired — reconnect in Settings")),
        403 => Err(anyhow::anyhow!(
            "WHOOP read not permitted (check your WHOOP app's read scopes)"
        )),
        _ => match status_outcome(status) {
            StatusOutcome::Success => Ok(()),
            StatusOutcome::NotFound => {
                Err(anyhow::anyhow!("{what} failed — WHOOP had no such record"))
            }
            StatusOutcome::RateLimited => {
                Err(anyhow::anyhow!("{what} was rate limited by WHOOP; try again shortly"))
            }
            StatusOutcome::ServerError => {
                Err(anyhow::anyhow!("{what} failed on WHOOP's side; this is usually transient"))
            }
            other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned WHOOP JSON (realistic API SHAPE, never fetched). The shared
// `ProviderAuth` handle is wired over its OWN MockTransport with a canned refresh
// response so `bearer()` works without a network or real token. No network, no
// real WHOOP round-trip, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::oauth2::{ProviderAuth, RefreshTokenStore, WHOOP};
    use crate::integrations::testing::MockTransport;

    /// Fake credential values that, if leaked, would be unmistakable in an
    /// assertion. None of these is ever asserted to APPEAR — they are scanned for
    /// ABSENCE in produced output.
    const FAKE_CLIENT_ID: &str = "FAKE-WHOOP-CLIENT-ID-1234";
    const FAKE_CLIENT_SECRET: &str = "FAKE-WHOOP-CLIENT-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "FAKE-WHOOP-REFRESH-TOKEN-NEVER-LEAK";
    /// The access token the canned refresh response mints. The WHOOP client puts
    /// THIS in its Authorization header; tests assert it never lands in output.
    const FAKE_ACCESS: &str = "ACCESS-FAKE-WHOOP-NEVER-LEAK-IN-OUTPUT";

    /// A no-op Keychain store so building a `ProviderAuth` never touches the real
    /// Keychain.
    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    /// Canned WHOOP refresh response so `auth.bearer()` mints `FAKE_ACCESS`
    /// without a network call.
    fn refresh_ok_json() -> String {
        format!(r#"{{"access_token":"{FAKE_ACCESS}","expires_in":3600,"token_type":"bearer"}}"#)
    }

    /// Build a `ProviderAuth` handle (WHOOP provider) over its own MockTransport
    /// that answers the token endpoint with a canned access token — the shared
    /// handle the WHOOP client borrows for `bearer()`.
    fn test_auth() -> ProviderAuth<MockTransport> {
        let token_mock =
            MockTransport::new().on(HttpMethod::Post, WHOOP.token_endpoint, 200, refresh_ok_json());
        ProviderAuth::new(
            WHOOP,
            token_mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        )
    }

    /// A WHOOP client whose API transport is `api_mock` and whose auth handle is a
    /// canned-refresh `ProviderAuth`.
    fn client(api_mock: MockTransport) -> WhoopClient<MockTransport, MockTransport> {
        WhoopClient::with_auth(api_mock, test_auth())
    }

    // -- realistic canned payloads (hand-written from the WHOOP API shape) ----

    fn recovery_json() -> &'static str {
        r#"{"records":[
            {"cycle_id":93845,"sleep_id":10235,"score_state":"SCORED",
             "score":{"user_calibrating":false,"recovery_score":66,
                      "resting_heart_rate":52,"hrv_rmssd_milli":78.4}}
        ],"next_token":null}"#
    }

    fn sleep_json() -> &'static str {
        r#"{"records":[
            {"id":10235,"score_state":"SCORED",
             "score":{"stage_summary":{"total_in_bed_time_milli":29700000,
                                       "total_awake_time_milli":1800000},
                      "sleep_performance_percentage":89}}
        ],"next_token":null}"#
    }

    fn cycle_json() -> &'static str {
        r#"{"records":[
            {"id":93845,"score_state":"SCORED",
             "score":{"strain":12.7,"kilojoule":8288.3,"average_heart_rate":68}}
        ],"next_token":null}"#
    }

    fn empty_page_json() -> &'static str {
        r#"{"records":[],"next_token":null}"#
    }

    /// A record still being scored has no `score` block — must read as "no data",
    /// never a fabricated zero.
    fn unscored_recovery_json() -> &'static str {
        r#"{"records":[{"cycle_id":1,"score_state":"PENDING_SCORE"}],"next_token":null}"#
    }

    // -- READ: parsing -------------------------------------------------------

    #[tokio::test]
    async fn latest_recovery_parses_score_hrv_and_rhr() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/recovery", 200, recovery_json());
        let out = client(mock).latest_recovery().await.unwrap();
        assert!(out.contains("Recovery 66%"), "got: {out}");
        assert!(out.contains("HRV 78 ms"), "got: {out}");
        assert!(out.contains("resting heart rate 52 bpm"), "got: {out}");
    }

    #[tokio::test]
    async fn latest_sleep_parses_performance_and_time_asleep() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/activity/sleep", 200, sleep_json());
        let out = client(mock).latest_sleep().await.unwrap();
        assert!(out.contains("Sleep performance 89%"), "got: {out}");
        // 29.7M - 1.8M = 27.9M ms = 465 min = 7h 45m.
        assert!(out.contains("7h 45m"), "got: {out}");
    }

    #[tokio::test]
    async fn latest_strain_parses_day_strain() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/cycle", 200, cycle_json());
        let out = client(mock).latest_strain().await.unwrap();
        assert!(out.contains("Day strain 12.7"), "got: {out}");
        assert!(out.contains("0 to 21"), "got: {out}");
    }

    // -- READ: empty / unscored -> friendly no-data, never a fabricated number -

    #[tokio::test]
    async fn empty_recovery_page_is_friendly_no_data() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/recovery", 200, empty_page_json());
        let out = client(mock).latest_recovery().await.unwrap();
        assert!(out.contains("No recent recovery"), "got: {out}");
        // No fabricated zeroed-out number.
        assert!(!out.contains('%'), "must not invent a percent: {out}");
    }

    #[tokio::test]
    async fn unscored_recovery_record_is_friendly_no_data() {
        let mock =
            MockTransport::new().on(HttpMethod::Get, "/recovery", 200, unscored_recovery_json());
        let out = client(mock).latest_recovery().await.unwrap();
        assert!(out.contains("No recent recovery"), "got: {out}");
    }

    #[tokio::test]
    async fn empty_sleep_and_strain_pages_are_friendly_no_data() {
        let s = MockTransport::new().on(HttpMethod::Get, "/activity/sleep", 200, empty_page_json());
        assert!(client(s).latest_sleep().await.unwrap().contains("No recent sleep"));
        let c = MockTransport::new().on(HttpMethod::Get, "/cycle", 200, empty_page_json());
        assert!(client(c).latest_strain().await.unwrap().contains("No recent strain"));
    }

    // -- error mapping (401 reconnect, 403 scope) ----------------------------

    #[tokio::test]
    async fn unauthorized_maps_to_reconnect() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/recovery", 401, "{}");
        let err = client(mock).latest_recovery().await.unwrap_err().to_string();
        assert!(err.contains("reconnect"), "401 -> reconnect: {err}");
    }

    #[tokio::test]
    async fn forbidden_maps_to_scope_hint() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/activity/sleep", 403, "{}");
        let err = client(mock).latest_sleep().await.unwrap_err().to_string();
        assert!(err.contains("scope"), "403 -> scope hint: {err}");
    }

    #[tokio::test]
    async fn rate_limited_is_friendly() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/cycle", 429, "{}");
        let err = client(mock).latest_strain().await.unwrap_err().to_string();
        assert!(err.contains("rate limited"), "429 -> rate limited: {err}");
    }

    // -- no secret EVER appears in any produced output -----------------------

    #[tokio::test]
    async fn no_token_or_secret_leaks_into_output_or_debug() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/recovery", 200, recovery_json())
            .on(HttpMethod::Get, "/activity/sleep", 200, sleep_json())
            .on(HttpMethod::Get, "/cycle", 200, cycle_json());
        let c = client(mock);
        let outs = [
            c.latest_recovery().await.unwrap(),
            c.latest_sleep().await.unwrap(),
            c.latest_strain().await.unwrap(),
        ];
        for out in &outs {
            for secret in [FAKE_ACCESS, FAKE_CLIENT_SECRET, FAKE_REFRESH, FAKE_CLIENT_ID] {
                assert!(!out.contains(secret), "output leaked a secret: {out}");
            }
        }
        // And the client's Debug never widens the surface.
        let dbg = format!("{c:?}");
        for secret in [FAKE_ACCESS, FAKE_CLIENT_SECRET, FAKE_REFRESH] {
            assert!(!dbg.contains(secret), "Debug leaked a secret: {dbg}");
        }
        assert!(dbg.contains("auth_attached"));
    }

    // -- pure-helper checks --------------------------------------------------

    #[test]
    fn format_duration_renders_hours_and_minutes() {
        assert_eq!(format_duration(27_900_000.0), "7h 45m");
        assert_eq!(format_duration(1_800_000.0), "30m");
        assert_eq!(format_duration(0.0), "0m");
    }
}
