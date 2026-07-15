//! X (Twitter API v2) client for agent "veronica" (Content + Comms).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]) and the round-3a generic OAuth2 core
//! ([`crate::integrations::oauth2`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests) and borrows a shared
//! [`ProviderAuth`] handle (configured with the [`X`] provider) for its bearer.
//! The client holds NO secret of its own — every request's access token comes
//! from `auth.bearer()` at the moment of the send, so the token is never logged,
//! never stored on the transport, never put in an error or a `Debug` field; only
//! that an auth handle is attached is ever recorded.
//!
//! Two tiers of methods, mirroring the foundation's safety model:
//!   * READ (safe — no gate): [`XClient::whoami`] (the connectivity probe that
//!     also resolves the user id), [`XClient::recent_tweets`], and
//!     [`XClient::mentions`] — plain GETs, no side effects.
//!   * CONSEQUENTIAL (gated): [`XClient::post_tweet`] posts a PUBLIC tweet AS THE
//!     USER. It takes an [`ActionMode`]: in [`ActionMode::DryRun`] it builds and
//!     returns a clear PREVIEW of the exact tweet text and issues NO request; only
//!     in [`ActionMode::Execute`] does it POST exactly one tweet. Call sites get
//!     the mode from the foundation's `gate(confirm)`, so with
//!     `[integrations].allow_consequential` false (the shipped default) it always
//!     previews. The 280-char limit is enforced with a friendly error BEFORE any
//!     request is built — an over-long tweet never reaches the network.
//!
//! Every method returns a concise human-facing `String` — what veronica would
//! say — while parsing the typed fields it needs (user id/handle/name, tweet
//! id/text). Non-2xx responses map to friendly, secret-free errors via
//! [`map_status`]: 401 -> reconnect, 403 -> write not permitted, 429 -> rate
//! limited. The provider body (which can echo PII) is never included.

use serde::Deserialize;
use tracing::info;

use super::oauth2::{ProviderAuth, X};
use super::{
    status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// Twitter API v2 base. All paths are appended to this.
const API_BASE: &str = "https://api.twitter.com/2";

/// X's hard limit on a standard tweet. Enforced BEFORE any request so an
/// over-long tweet is rejected locally rather than round-tripped to a 4xx.
const TWEET_MAX_CHARS: usize = 280;

/// How many recent tweets / mentions one read may ask X for. X's
/// `tweets`/`mentions` timelines accept `max_results` in 5..=100; we clamp the
/// caller's request into that band so a bad `max` can never 4xx the call.
const X_MIN_RESULTS: u32 = 5;
const X_MAX_RESULTS: u32 = 100;

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields veronica actually needs are decoded.
// `#[serde(default)]` on the soft fields keeps parsing resilient to the many
// extra keys the v2 API returns and to objects that omit an optional field.
// ---------------------------------------------------------------------------

/// The authenticated user, as returned by `GET /2/users/me` (the data object).
#[derive(Debug, Clone, Deserialize, Default)]
struct XUser {
    /// Numeric user id (a string in the v2 API), used to address the user's
    /// timeline (`/2/users/:id/tweets`) and mentions.
    #[serde(default)]
    id: String,
    /// The @handle, WITHOUT the leading "@".
    #[serde(default)]
    username: String,
    /// The display name.
    #[serde(default)]
    name: String,
}

/// The `{"data": ...}` envelope around a single user object.
#[derive(Debug, Clone, Deserialize, Default)]
struct UserEnvelope {
    #[serde(default)]
    data: XUser,
}

/// One tweet, as returned in the `data` array of a timeline / mentions read and
/// in the `data` object of a `POST /2/tweets` 201.
#[derive(Debug, Clone, Deserialize, Default)]
struct Tweet {
    #[serde(default)]
    id: String,
    #[serde(default)]
    text: String,
}

/// The `{"data": [ ... ]}` envelope around a timeline / mentions list. The
/// array is absent when there are no results, so it defaults to empty.
#[derive(Debug, Clone, Deserialize, Default)]
struct TweetList {
    #[serde(default)]
    data: Vec<Tweet>,
}

/// The `{"data": { ... }}` envelope around a just-created tweet.
#[derive(Debug, Clone, Deserialize, Default)]
struct CreatedTweet {
    #[serde(default)]
    data: Tweet,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// X (Twitter API v2) client bound to a transport and a shared [`ProviderAuth`]
/// handle configured for the [`X`] provider.
///
/// Construct with [`XClient::new`] (production: a `ReqwestTransport` API client
/// paired with a connected `ProviderAuth`) or, in tests,
/// [`XClient::with_auth`] (a `MockTransport` API client + a `ProviderAuth`
/// wired over its own `MockTransport`). The client holds NO secret of its own —
/// every request's bearer comes from `auth.bearer()` at the moment of the send,
/// so there is nothing to redact in `Debug` beyond noting the handle is present.
pub struct XClient<T: HttpTransport, A: HttpTransport> {
    transport: T,
    auth: ProviderAuth<A>,
}

/// `Debug` notes only that an auth handle is attached — it never prints any
/// token (the `ProviderAuth` `Debug` itself redacts all secrets, but we keep
/// this minimal so a `{:?}` of the X client can't widen the surface).
impl<T: HttpTransport, A: HttpTransport> std::fmt::Debug for XClient<T, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XClient")
            .field("auth_attached", &true)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport, A: HttpTransport> XClient<T, A> {
    /// Build a client over `transport`, taking ownership of a shared
    /// [`ProviderAuth`] handle. Used by tests (mock transports) and by
    /// [`XClient::new`] internally. No secret is resolved here — the bearer is
    /// fetched per request from `auth`.
    pub fn with_auth(transport: T, auth: ProviderAuth<A>) -> Self {
        Self { transport, auth }
    }

    /// Compose a request with the X-standard Authorization header, attaching the
    /// Bearer token HERE — fetched fresh from `auth` at the moment of the call —
    /// and nowhere else. The token never lands on the transport or in a log.
    async fn request(&self, method: HttpMethod, path: &str) -> IntegrationResult<HttpRequest> {
        let token = self.auth.bearer().await?;
        Ok(HttpRequest::new(method, format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {token}")))
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// Connectivity probe: `GET /2/users/me`. Read-only. Returns the
    /// authenticated user's @handle and display name — a quick "who am I /
    /// is X connected" check. Internally shared with the timeline reads, which
    /// need the numeric id this endpoint also returns (see [`Self::me`]).
    pub async fn whoami(&self) -> IntegrationResult<String> {
        let user = self.me().await?;
        info!(has_handle = !user.username.is_empty(), "x: whoami");
        if user.username.is_empty() {
            return Ok("Connected to X.".to_string());
        }
        Ok(format!(
            "Connected to X as @{} ({}).",
            user.username,
            if user.name.is_empty() { "—" } else { &user.name }
        ))
    }

    /// Resolve the authenticated user (`GET /2/users/me`) into the typed object,
    /// used by [`Self::whoami`] for the spoken summary and by the timeline reads
    /// for the numeric id they must address. Maps non-2xx to a friendly error.
    async fn me(&self) -> IntegrationResult<XUser> {
        let req = self.request(HttpMethod::Get, "/users/me").await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "checking your X account")?;
        let env: UserEnvelope = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("checking your X account returned an unexpected response"))?;
        if env.data.id.is_empty() {
            return Err(anyhow::anyhow!("X did not return your account id"));
        }
        Ok(env.data)
    }

    /// List the authenticated user's recent tweets (up to `max`, clamped to X's
    /// 5..=100 band). Read-only: resolves the user id via `GET /2/users/me`, then
    /// reads `GET /2/users/:id/tweets`. Returns a count plus the first few tweet
    /// texts.
    pub async fn recent_tweets(&self, max: u32) -> IntegrationResult<String> {
        let want = max.clamp(X_MIN_RESULTS, X_MAX_RESULTS);
        let id = self.me().await?.id;
        let req = self
            .request(
                HttpMethod::Get,
                &format!("/users/{id}/tweets?max_results={want}"),
            )
            .await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading your recent tweets")?;

        let list: TweetList = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("reading your recent tweets returned an unexpected response"))?;
        info!(count = list.data.len(), "x: listed recent tweets");
        Ok(summarize_tweets(&list.data, "recent tweet", "You have no recent tweets."))
    }

    /// List recent mentions of the authenticated user (up to `max`, clamped to
    /// X's 5..=100 band). Read-only: resolves the user id via `GET /2/users/me`,
    /// then reads `GET /2/users/:id/mentions`. Returns a count plus the first few
    /// mention texts.
    pub async fn mentions(&self, max: u32) -> IntegrationResult<String> {
        let want = max.clamp(X_MIN_RESULTS, X_MAX_RESULTS);
        let id = self.me().await?.id;
        let req = self
            .request(
                HttpMethod::Get,
                &format!("/users/{id}/mentions?max_results={want}"),
            )
            .await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading your mentions")?;

        let list: TweetList = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("reading your mentions returned an unexpected response"))?;
        info!(count = list.data.len(), "x: listed mentions");
        Ok(summarize_tweets(&list.data, "mention", "You have no recent mentions."))
    }

    // -- CONSEQUENTIAL (gated by ActionMode) ---------------------------------

    /// Post a PUBLIC tweet AS THE USER with `text` (`POST /2/tweets`). The most
    /// sensitive action this client performs.
    ///
    /// The 280-character limit is enforced FIRST, before any request is built —
    /// an over-long tweet returns a friendly error and never touches the network
    /// (or the transport). Then:
    ///   * In [`ActionMode::DryRun`] this issues NO request — it returns a clear
    ///     PREVIEW of the exact tweet text that would be posted.
    ///   * In [`ActionMode::Execute`] it issues exactly one `POST /2/tweets`
    ///     carrying `{"text": ...}` and returns a short confirmation with the new
    ///     tweet id.
    ///     Callers obtain `mode` from the foundation's `gate(confirm)`, so the shipped
    ///     default (gate OFF) always previews.
    pub async fn post_tweet(&self, text: &str, mode: ActionMode) -> IntegrationResult<String> {
        // Enforce the length limit up front — counted in Unicode scalar values,
        // matching how X counts a standard tweet for this purpose — so a too-long
        // tweet is rejected BEFORE we build or send anything.
        let len = text.chars().count();
        if len == 0 {
            return Err(anyhow::anyhow!("a tweet can't be empty"));
        }
        if len > TWEET_MAX_CHARS {
            return Err(anyhow::anyhow!(
                "that tweet is {len} characters — X allows at most {TWEET_MAX_CHARS}; shorten it by {} and try again",
                len - TWEET_MAX_CHARS
            ));
        }

        if mode == ActionMode::DryRun {
            // No request is built or sent — pure preview of the EXACT text.
            info!(dry_run = true, chars = len, "x: post preview (no request issued)");
            return Ok(format!(
                "[dry run] Would post this tweet ({len}/{TWEET_MAX_CHARS} chars): \"{text}\". \
                 Enable consequential actions and confirm to post."
            ));
        }

        let req = self
            .request(HttpMethod::Post, "/tweets")
            .await?
            .json_body(serde_json::json!({ "text": text }));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "posting the tweet")?;

        let created: CreatedTweet = serde_json::from_str(&resp.body).unwrap_or_default();
        info!(id_present = !created.data.id.is_empty(), "x: tweet posted");
        if created.data.id.is_empty() {
            Ok("Tweet posted.".to_string())
        } else {
            Ok(format!("Tweet posted (id {}).", created.data.id))
        }
    }
}

impl XClient<super::ReqwestTransport, super::ReqwestTransport> {
    /// Production constructor: pair the real reqwest transport for X's API with a
    /// connected [`ProviderAuth`] handle for the [`X`] provider (which itself
    /// resolves the OAuth credentials + refresh token from the Keychain and wires
    /// its own reqwest transport). Returns the OAuth core's friendly "not
    /// connected" error when X has not been connected in Settings.
    pub async fn new() -> IntegrationResult<Self> {
        let auth = ProviderAuth::<super::ReqwestTransport>::connect(X).await?;
        Ok(Self::with_auth(super::ReqwestTransport::new(), auth))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// Render a list of tweets into a concise, spoken-friendly summary: a count of
/// `noun`s plus the first few texts (collapsed to one line each), with an
/// `empty` message when there are none. Pure.
fn summarize_tweets(tweets: &[Tweet], noun: &str, empty: &str) -> String {
    if tweets.is_empty() {
        return empty.to_string();
    }
    let lines: Vec<String> = tweets
        .iter()
        .take(5)
        .map(|t| format!("\"{}\"", one_line(&t.text)))
        .collect();
    let more = tweets.len().saturating_sub(lines.len());
    let mut out = format!(
        "{} {noun}{}: {}",
        tweets.len(),
        if tweets.len() == 1 { "" } else { "s" },
        lines.join("; ")
    );
    if more > 0 {
        out.push_str(&format!("; and {more} more"));
    }
    out.push('.');
    out
}

/// Collapse a tweet body to a single trimmed line for a summary, so a multi-line
/// tweet doesn't blow up the spoken reply. Pure.
fn one_line(text: &str) -> String {
    text.replace('\n', " ").trim().to_string()
}

/// Map an X (Twitter API v2) status to a friendly, secret-free error. 2xx is
/// `Ok`. The task's mappings: 401 -> reconnect; 403 -> write not permitted
/// (API tier / scopes); 429 -> rate limited. The provider body (which can echo
/// PII) is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    // 401 and 403 both classify as Unauthorized in the foundation, but X gives
    // them distinct meanings: 401 is an expired/invalid token (reconnect), 403 is
    // a permitted-but-not-authorized write (wrong API tier / missing scope). We
    // branch on the raw code first so each gets its own hint.
    match status {
        401 => Err(anyhow::anyhow!("X access expired — reconnect in Settings")),
        403 => Err(anyhow::anyhow!(
            "X write not permitted (check API tier / scopes)"
        )),
        _ => match status_outcome(status) {
            StatusOutcome::Success => Ok(()),
            StatusOutcome::NotFound => {
                Err(anyhow::anyhow!("{what} failed — the requested item was not found on X"))
            }
            StatusOutcome::RateLimited => {
                Err(anyhow::anyhow!("{what} was rate limited by X; try again shortly"))
            }
            StatusOutcome::ServerError => {
                Err(anyhow::anyhow!("{what} failed on X's side; this is usually transient"))
            }
            // Any other 4xx / unexpected code: lean on the foundation's phrasing.
            other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned X JSON (realistic API SHAPE, never fetched). The shared
// `ProviderAuth` handle is wired over its OWN MockTransport with a canned refresh
// response so `bearer()` works without a network or real token. No network, no
// real X round-trip, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::oauth2::{ProviderAuth, RefreshTokenStore, X};
    use crate::integrations::testing::MockTransport;

    /// Fake credential values that, if leaked, would be unmistakable in an
    /// assertion. None of these is ever asserted to APPEAR — they are scanned for
    /// ABSENCE in produced output.
    const FAKE_CLIENT_ID: &str = "FAKE-X-CLIENT-ID-1234";
    const FAKE_CLIENT_SECRET: &str = "FAKE-X-CLIENT-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "FAKE-X-REFRESH-TOKEN-NEVER-LEAK";
    /// The access token the canned refresh response mints. The X client puts THIS
    /// in its Authorization header; tests assert it never lands in output.
    const FAKE_ACCESS: &str = "ACCESS-FAKE-X-NEVER-LEAK-IN-OUTPUT";

    /// A no-op Keychain store so building a `ProviderAuth` never touches the real
    /// Keychain.
    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    /// Canned X refresh response so `auth.bearer()` mints `FAKE_ACCESS` without a
    /// network call. Refresh responses may omit `refresh_token`.
    fn refresh_ok_json() -> String {
        format!(r#"{{"access_token":"{FAKE_ACCESS}","expires_in":7200,"token_type":"bearer"}}"#)
    }

    /// Build a `ProviderAuth` handle (X provider) over its own MockTransport that
    /// answers the token endpoint with a canned access token — the shared handle
    /// the X client borrows for `bearer()`.
    fn test_auth() -> ProviderAuth<MockTransport> {
        let token_mock =
            MockTransport::new().on(HttpMethod::Post, X.token_endpoint, 200, refresh_ok_json());
        ProviderAuth::new(
            X,
            token_mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        )
    }

    /// An X client whose API transport is `api_mock` and whose auth handle is a
    /// canned-refresh `ProviderAuth`.
    fn client(api_mock: MockTransport) -> XClient<MockTransport, MockTransport> {
        XClient::with_auth(api_mock, test_auth())
    }

    // -- realistic canned payloads (hand-written from the v2 API shape) -------

    fn me_json() -> &'static str {
        r#"{"data":{"id":"1234567890","username":"veronica","name":"Veronica J"}}"#
    }

    fn tweets_json() -> &'static str {
        r#"{"data":[
            {"id":"111","text":"Shipping the new build today."},
            {"id":"112","text":"Two\nline tweet."}
        ],"meta":{"result_count":2}}"#
    }

    fn mentions_json() -> &'static str {
        r#"{"data":[
            {"id":"211","text":"@veronica great work!"}
        ],"meta":{"result_count":1}}"#
    }

    fn empty_list_json() -> &'static str {
        r#"{"meta":{"result_count":0}}"#
    }

    fn created_tweet_json() -> &'static str {
        // Shape of X's 201 response to POST /2/tweets.
        r#"{"data":{"id":"999000111","text":"Hello from DARWIN"}}"#
    }

    // -- READ: parsing -------------------------------------------------------

    #[tokio::test]
    async fn whoami_parses_handle_and_name() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/users/me", 200, me_json());
        let out = client(mock).whoami().await.unwrap();
        assert!(out.contains("@veronica"), "got: {out}");
        assert!(out.contains("Veronica J"), "got: {out}");
    }

    #[tokio::test]
    async fn recent_tweets_resolves_id_then_lists() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/users/me", 200, me_json())
            .on(HttpMethod::Get, "/users/1234567890/tweets", 200, tweets_json());
        let out = client(mock).recent_tweets(10).await.unwrap();
        assert!(out.contains("2 recent tweets"), "got: {out}");
        assert!(out.contains("Shipping the new build today."), "got: {out}");
        // Multi-line tweet collapses to one line.
        assert!(out.contains("Two line tweet."), "got: {out}");
    }

    #[tokio::test]
    async fn recent_tweets_addresses_the_users_timeline() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/users/me", 200, me_json())
            .on(HttpMethod::Get, "/users/1234567890/tweets", 200, tweets_json());
        let c = client(mock);
        c.recent_tweets(10).await.unwrap();
        let reqs = c.transport.requests();
        // me -> timeline: two GETs, the second addressing the resolved id.
        assert_eq!(reqs.len(), 2, "one /users/me then one timeline GET");
        assert!(reqs[0].url.contains("/users/me"));
        assert_eq!(reqs[1].method, HttpMethod::Get);
        assert!(reqs[1].url.contains("/users/1234567890/tweets"));
        // max clamped into X's band and present on the query.
        assert!(reqs[1].url.contains("max_results=10"));
        assert!(reqs[1].has_header("authorization"), "auth header attached");
    }

    #[tokio::test]
    async fn recent_tweets_clamps_max_into_x_band() {
        // Below the floor and above the ceiling both clamp.
        for (asked, want) in [(1u32, "max_results=5"), (1000u32, "max_results=100")] {
            let mock = MockTransport::new()
                .on(HttpMethod::Get, "/users/me", 200, me_json())
                .on(HttpMethod::Get, "/users/1234567890/tweets", 200, tweets_json());
            let c = client(mock);
            c.recent_tweets(asked).await.unwrap();
            let reqs = c.transport.requests();
            assert!(reqs[1].url.contains(want), "asked {asked}, url: {}", reqs[1].url);
        }
    }

    #[tokio::test]
    async fn mentions_parses() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/users/me", 200, me_json())
            .on(HttpMethod::Get, "/users/1234567890/mentions", 200, mentions_json());
        let out = client(mock).mentions(10).await.unwrap();
        assert!(out.contains("1 mention"), "got: {out}");
        assert!(out.contains("great work"), "got: {out}");
    }

    #[tokio::test]
    async fn empty_timeline_is_friendly() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/users/me", 200, me_json())
            .on(HttpMethod::Get, "/users/1234567890/tweets", 200, empty_list_json());
        let out = client(mock).recent_tweets(10).await.unwrap();
        assert!(out.contains("no recent tweets"), "got: {out}");
    }

    // -- READ: header SHAPE on the recorded request (never the token) --------

    #[tokio::test]
    async fn read_request_carries_auth_header() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/users/me", 200, me_json());
        let c = client(mock);
        c.whoami().await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req.url.contains("/users/me"));
        // Presence of auth — NOT its value.
        assert!(req.has_header("authorization"), "auth header attached");
        // No body on a read.
        assert!(req.body.is_none());
    }

    // -- CONSEQUENTIAL: DryRun issues NO request -----------------------------

    #[tokio::test]
    async fn post_tweet_dry_run_issues_no_request_and_previews_exact_text() {
        let mock = MockTransport::new(); // no canned responses on purpose
        let c = client(mock);
        let out = c
            .post_tweet("Hello from DARWIN", ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        // The PREVIEW carries the EXACT tweet text.
        assert!(out.contains("Hello from DARWIN"), "got: {out}");
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not touch the API transport"
        );
    }

    // -- CONSEQUENTIAL: Execute issues EXACTLY one correct request -----------

    #[tokio::test]
    async fn post_tweet_execute_posts_exactly_one_request_with_text_body() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/tweets", 201, created_tweet_json());
        let c = client(mock);
        let out = c
            .post_tweet("Hello from DARWIN", ActionMode::Execute)
            .await
            .unwrap();
        assert!(out.contains("Tweet posted"), "got: {out}");
        assert!(out.contains("999000111"), "echoes the new tweet id: {out}");

        // EXACTLY one request, and it is the POST with the right body.
        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 1, "Execute issues exactly one POST");
        let req = &reqs[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.ends_with("/tweets"), "got url: {}", req.url);
        // Body SHAPE — the tweet text in `text`.
        assert_eq!(req.body.as_ref().unwrap()["text"], "Hello from DARWIN");
        // Auth attached, value never asserted.
        assert!(req.has_header("authorization"));
    }

    // -- CONSEQUENTIAL: the 280-char limit is enforced PRE-request ------------

    #[tokio::test]
    async fn post_tweet_over_280_is_rejected_before_any_request_execute() {
        let too_long = "x".repeat(281);
        let mock = MockTransport::new().on(HttpMethod::Post, "/tweets", 201, created_tweet_json());
        let c = client(mock);
        let err = c.post_tweet(&too_long, ActionMode::Execute).await.unwrap_err();
        assert!(err.to_string().contains("281"), "names the length: {err}");
        assert!(err.to_string().contains("280"), "names the limit: {err}");
        // No request was issued — the check happens before the network.
        assert_eq!(c.transport.requests().len(), 0, "over-long tweet never hits the transport");
    }

    #[tokio::test]
    async fn post_tweet_over_280_is_rejected_even_in_dry_run() {
        let too_long = "x".repeat(300);
        let c = client(MockTransport::new());
        let err = c.post_tweet(&too_long, ActionMode::DryRun).await.unwrap_err();
        assert!(err.to_string().contains("280"), "got: {err}");
        assert_eq!(c.transport.requests().len(), 0);
    }

    #[tokio::test]
    async fn post_tweet_exactly_280_is_allowed() {
        // Boundary: 280 chars is fine; 281 is not (proven above).
        let exactly = "x".repeat(280);
        let mock = MockTransport::new().on(HttpMethod::Post, "/tweets", 201, created_tweet_json());
        let c = client(mock);
        let out = c.post_tweet(&exactly, ActionMode::Execute).await.unwrap();
        assert!(out.contains("Tweet posted"), "got: {out}");
        assert_eq!(c.transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn post_tweet_empty_is_rejected() {
        let c = client(MockTransport::new());
        let err = c.post_tweet("", ActionMode::DryRun).await.unwrap_err();
        assert!(err.to_string().contains("empty"), "got: {err}");
        assert_eq!(c.transport.requests().len(), 0);
    }

    // -- error mapping -------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_401_maps_to_reconnect_hint() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/users/me", 401, "{}");
        let err = client(mock).whoami().await.unwrap_err();
        assert!(
            err.to_string().contains("X access expired — reconnect in Settings"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn forbidden_403_maps_to_write_not_permitted() {
        // 403 on the write path is the "wrong API tier / scope" case.
        let mock = MockTransport::new().on(HttpMethod::Post, "/tweets", 403, "{}");
        let err = client(mock)
            .post_tweet("hi", ActionMode::Execute)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("X write not permitted (check API tier / scopes)"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn rate_limited_429_maps_friendly() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/users/me", 429, "{}");
        let err = client(mock).whoami().await.unwrap_err();
        assert!(err.to_string().contains("rate limited"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/users/me", 503, "upstream down");
        let err = client(mock).whoami().await.unwrap_err();
        assert!(err.to_string().contains("transient"), "got: {err}");
    }

    // -- the TOKEN never leaks ----------------------------------------------

    /// Neither the access token the auth handle mints nor any of the OAuth
    /// credentials may appear in the client's `Debug` output, in any returned
    /// outcome string, or in any mapped error. We drive a representative slice of
    /// the surface and scan every produced string for each secret.
    #[tokio::test]
    async fn secrets_never_appear_in_any_produced_output() {
        // Debug of the client.
        let dbg = format!("{:?}", client(MockTransport::new()));
        assert!(!dbg.contains(FAKE_ACCESS), "Debug leaked the access token: {dbg}");
        assert!(!dbg.contains(FAKE_REFRESH), "Debug leaked the refresh token: {dbg}");
        assert!(dbg.contains("auth_attached"), "Debug should note the handle is attached");

        // Success + dry-run + error outcome strings, across read and write.
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/users/me", 200, me_json())
            .on(HttpMethod::Get, "/users/1234567890/tweets", 200, tweets_json())
            .on(HttpMethod::Post, "/tweets", 201, created_tweet_json());
        let c = client(mock);
        let ok1 = c.whoami().await.unwrap();
        let ok2 = c.recent_tweets(10).await.unwrap();
        let ok3 = c.post_tweet("posting now", ActionMode::Execute).await.unwrap();
        let dry = c.post_tweet("preview me", ActionMode::DryRun).await.unwrap();

        let err_mock = MockTransport::new().on(HttpMethod::Get, "/users/me", 401, "{}");
        let err = client(err_mock).whoami().await.unwrap_err().to_string();

        for s in [&ok1, &ok2, &ok3, &dry, &err] {
            for secret in [FAKE_ACCESS, FAKE_REFRESH, FAKE_CLIENT_ID, FAKE_CLIENT_SECRET] {
                assert!(!s.contains(secret), "output leaked a secret: {s}");
            }
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn summarize_tweets_counts_and_collapses() {
        let one = vec![Tweet {
            id: "1".into(),
            text: "hi\nthere".into(),
        }];
        let s = summarize_tweets(&one, "recent tweet", "none");
        assert!(s.contains("1 recent tweet:"), "got: {s}");
        assert!(s.contains("\"hi there\""), "collapses newlines: {s}");

        let many: Vec<Tweet> = (0..7)
            .map(|i| Tweet {
                id: i.to_string(),
                text: format!("t{i}"),
            })
            .collect();
        let s = summarize_tweets(&many, "recent tweet", "none");
        assert!(s.contains("7 recent tweets:"), "got: {s}");
        assert!(s.contains("and 2 more"), "shows the overflow count: {s}");

        assert_eq!(summarize_tweets(&[], "mention", "no mentions"), "no mentions");
    }

    #[test]
    fn one_line_collapses_and_trims() {
        assert_eq!(one_line("  hi there  "), "hi there");
        assert_eq!(one_line("a\nb\nc"), "a b c");
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(201, "x").is_ok());
        assert!(map_status(401, "x")
            .unwrap_err()
            .to_string()
            .contains("reconnect in Settings"));
        assert!(map_status(403, "x")
            .unwrap_err()
            .to_string()
            .contains("write not permitted"));
        assert!(map_status(404, "x").unwrap_err().to_string().contains("not found"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(500, "x").unwrap_err().to_string().contains("transient"));
    }
}
