//! LinkedIn client for agents "veronica" (Content/Comms) and "stark" (Business
//! Intel).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]) and the provider-parameterized OAuth2 core
//! ([`crate::integrations::oauth2`]). It is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests) and gets its access token from a
//! shared LinkedIn [`ProviderAuth`] handle via [`ProviderAuth::bearer`]. The
//! LinkedIn client NEVER touches the refresh token and never resolves a Keychain
//! secret itself — that is the OAuth core's job; this client only ever asks for a
//! bearer at the moment of each send and attaches it as the `Authorization`
//! header. The token VALUE is never logged, never stored on the transport, never
//! in an error/Debug field — only presence (a bool) is ever recorded.
//!
//! Two tiers of methods, mirroring the foundation's safety model:
//!   * READ (safe, never gated): [`LinkedinClient::me`] — `GET /v2/userinfo`
//!     (the OpenID Connect member-identity endpoint the `openid`/`profile` scopes
//!     grant). It returns the member's display name plus their `sub`, which is
//!     also the author identifier: the post author URN is `urn:li:person:<sub>`.
//!   * CONSEQUENTIAL (gated by [`ActionMode`]): [`LinkedinClient::create_post`] —
//!     publishes a PUBLIC LinkedIn post AS THE MEMBER via the current Posts API
//!     (`POST /rest/posts`). In [`ActionMode::DryRun`] it issues NO request and
//!     returns a clear PREVIEW of the post text; only in [`ActionMode::Execute`]
//!     does it issue exactly one create-post request, carrying the author URN +
//!     commentary + PUBLIC visibility and the `LinkedIn-Version` /
//!     `X-Restli-Protocol-Version` headers the Posts API requires. Call sites get
//!     `mode` from the foundation's `gate(confirm)`, so with
//!     `[integrations].allow_consequential` false (the shipped default) a post
//!     always previews.
//!
//! Non-2xx responses map to friendly, secret-free errors via [`map_status`]
//! (401 -> reconnect; 403 -> the LinkedIn "posting not permitted" app/scope hint;
//! 422 -> a validation hint), never echoing the provider body (which can carry
//! member content or token-bearing fields).

use serde::Deserialize;
use tracing::info;

use super::oauth2::ProviderAuth;
use super::{
    status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// The OpenID Connect member-identity endpoint. With the `openid`/`profile`
/// scopes LinkedIn returns `{sub, name, ...}`; `sub` is the member id used to
/// build the author URN.
const USERINFO_URL: &str = "https://api.linkedin.com/v2/userinfo";

/// The current LinkedIn Posts API endpoint. `POST` here creates a post on the
/// versioned REST surface (hence the required `LinkedIn-Version` header).
const POSTS_URL: &str = "https://api.linkedin.com/rest/posts";

/// The `LinkedIn-Version` the versioned REST surface (`/rest/*`) requires, in
/// `YYYYMM` form. Sent on the create-post request; the Posts API rejects a
/// versioned call without it.
const LINKEDIN_VERSION: &str = "202401";

/// The protocol version the versioned REST surface requires on writes.
const RESTLI_PROTOCOL_VERSION: &str = "2.0.0";

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields the agents actually surface are
// decoded. `#[serde(default)]` keeps parsing resilient to LinkedIn's many extra
// keys and to objects that omit an optional field.
// ---------------------------------------------------------------------------

/// The `/v2/userinfo` (OpenID Connect) response. We decode the member id (`sub`)
/// and display `name`; LinkedIn also returns given/family name, locale, etc.,
/// which we deliberately ignore.
#[derive(Debug, Clone, Deserialize)]
struct UserInfo {
    #[serde(default)]
    sub: String,
    #[serde(default)]
    name: String,
}

/// The authenticated member's identity, as surfaced to a caller of
/// [`LinkedinClient::me`]. `id` is the raw `sub`; `author_urn` is the
/// `urn:li:person:<sub>` form the Posts API requires as the post author.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The member's display name (e.g. "Ada Lovelace").
    pub name: String,
    /// The member id (`sub` from `/v2/userinfo`).
    pub id: String,
    /// The author URN derived from the id: `urn:li:person:<id>`.
    pub author_urn: String,
}

/// Build the author URN the Posts API wants from a member `sub`/id.
fn author_urn(member_id: &str) -> String {
    format!("urn:li:person:{member_id}")
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// LinkedIn client bound to a transport and a shared LinkedIn [`ProviderAuth`]
/// handle.
///
/// Construct with [`LinkedinClient::with_auth`] (tests: a `MockTransport`
/// LinkedIn client + a `ProviderAuth` wired over its own `MockTransport`) or, in
/// production, [`LinkedinClient::connect`] (a `ReqwestTransport` LinkedIn client
/// paired with a connected `ProviderAuth`). The client holds NO secret of its own
/// — every request's bearer comes from `auth.bearer()` at the moment of the send,
/// so there is nothing to redact in `Debug` beyond noting the handle is present.
pub struct LinkedinClient<T: HttpTransport, A: HttpTransport> {
    transport: T,
    auth: ProviderAuth<A>,
}

/// `Debug` notes only that an auth handle is attached — it never prints any token
/// (the `ProviderAuth` `Debug` itself redacts all secrets, but we keep this
/// minimal so a `{:?}` of the LinkedIn client can't widen the surface).
impl<T: HttpTransport, A: HttpTransport> std::fmt::Debug for LinkedinClient<T, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkedinClient")
            .field("auth_attached", &true)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport, A: HttpTransport> LinkedinClient<T, A> {
    /// Build a client over `transport`, taking ownership of a shared LinkedIn
    /// [`ProviderAuth`] handle. Used by tests (mock transports) and by
    /// [`LinkedinClient::connect`] internally. No secret is resolved here — the
    /// bearer is fetched per request from `auth`.
    pub fn with_auth(transport: T, auth: ProviderAuth<A>) -> Self {
        Self { transport, auth }
    }

    /// Compose a request to `url` with the Bearer token attached HERE — fetched
    /// fresh from `auth` at the moment of the call — and nowhere else. The token
    /// never lands on the transport or in a log.
    async fn request(&self, method: HttpMethod, url: &str) -> IntegrationResult<HttpRequest> {
        let token = self.auth.bearer().await?;
        Ok(HttpRequest::new(method, url).header("Authorization", format!("Bearer {token}")))
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// Fetch the authenticated member's identity from `/v2/userinfo`. Read-only.
    /// Returns the member's display name, id (`sub`) and the `urn:li:person:<sub>`
    /// author URN [`create_post`](Self::create_post) needs. Maps non-2xx to a
    /// friendly, secret-free error.
    pub async fn me(&self) -> IntegrationResult<Member> {
        let req = self.request(HttpMethod::Get, USERINFO_URL).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading your LinkedIn profile")?;
        let info: UserInfo = serde_json::from_str(&resp.body).map_err(|_| {
            anyhow::anyhow!("reading your LinkedIn profile returned an unexpected response")
        })?;
        if info.sub.is_empty() {
            return Err(anyhow::anyhow!(
                "LinkedIn did not return a member id; reconnect LinkedIn in Settings"
            ));
        }
        info!(
            has_name = !info.name.is_empty(),
            "linkedin: fetched member identity"
        );
        let urn = author_urn(&info.sub);
        Ok(Member {
            name: info.name,
            id: info.sub,
            author_urn: urn,
        })
    }

    // -- CONSEQUENTIAL (gated by ActionMode) ---------------------------------

    /// Publish a PUBLIC LinkedIn post AS THE MEMBER, with `text` as its
    /// commentary. The MOST sensitive action this client exposes — it posts under
    /// the user's own identity for the whole network to see.
    ///
    /// In [`ActionMode::DryRun`] this issues NO request — it returns a clear
    /// PREVIEW of exactly what would be posted (the post text, PUBLIC). In
    /// [`ActionMode::Execute`] it first resolves the author URN via
    /// [`me`](Self::me) (one read), then issues exactly one `POST /rest/posts`
    /// carrying the author URN + commentary + PUBLIC visibility and the
    /// `LinkedIn-Version` / `X-Restli-Protocol-Version` headers the Posts API
    /// requires, and returns a short confirmation. Callers obtain `mode` from the
    /// foundation's `gate(confirm)`, so the shipped default (gate OFF) always
    /// previews.
    pub async fn create_post(&self, text: &str, mode: ActionMode) -> IntegrationResult<String> {
        if mode == ActionMode::DryRun {
            // No request is built or sent — pure preview. We surface the text via
            // a length-capped preview so a long post doesn't blow up a spoken
            // reply, but the whole post is the user's own words, so we show it in
            // full up to the cap.
            info!(dry_run = true, "linkedin: post preview (no request issued)");
            return Ok(format!(
                "[dry run] Would publish a PUBLIC LinkedIn post (begins: \"{}\"). \
                 Enable consequential actions and confirm to post.",
                preview(text)
            ));
        }

        // Execute: resolve the author URN, then issue exactly one create-post.
        let member = self.me().await?;
        let body = post_body(&member.author_urn, text);
        let req = self
            .request(HttpMethod::Post, POSTS_URL)
            .await?
            .header("LinkedIn-Version", LINKEDIN_VERSION)
            .header("X-Restli-Protocol-Version", RESTLI_PROTOCOL_VERSION)
            .header("Content-Type", "application/json")
            .json_body(body);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "publishing your LinkedIn post")?;
        info!("linkedin: post published");
        Ok("Your LinkedIn post is published.".to_string())
    }
}

impl LinkedinClient<super::ReqwestTransport, super::ReqwestTransport> {
    /// Production constructor: pair the real reqwest transport for LinkedIn's API
    /// with a connected LinkedIn [`ProviderAuth`] handle (which itself resolves the
    /// OAuth credentials from the Keychain and wires its own reqwest transport).
    /// Returns the OAuth core's friendly "LinkedIn isn't connected" error when
    /// LinkedIn has not been connected in Settings.
    pub async fn connect() -> IntegrationResult<Self> {
        let auth = ProviderAuth::<super::ReqwestTransport>::connect(super::oauth2::LINKEDIN).await?;
        Ok(Self::with_auth(super::ReqwestTransport::new(), auth))
    }
}

// ---------------------------------------------------------------------------
// Post body assembly (pure — unit-testable, no transport)
// ---------------------------------------------------------------------------

/// Build the JSON body the LinkedIn Posts API (`POST /rest/posts`) expects for a
/// plain-text, PUBLIC post authored by `author_urn` with `text` as its
/// commentary. PURE — no I/O, no secret. The shape mirrors the current Posts API:
/// `author` (the member URN), `commentary` (the post text), `visibility: PUBLIC`,
/// a main-feed `distribution`, `lifecycleState: PUBLISHED`, and reshare allowed.
fn post_body(author_urn: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "author": author_urn,
        "commentary": text,
        "visibility": "PUBLIC",
        "distribution": {
            "feedDistribution": "MAIN_FEED",
            "targetEntities": [],
            "thirdPartyDistributionChannels": []
        },
        "lifecycleState": "PUBLISHED",
        "isReshareDisabledByAuthor": false
    })
}

/// Collapse + truncate text to a single short line for a dry-run preview so a
/// long or multi-line post doesn't blow up the spoken reply.
fn preview(text: &str) -> String {
    let one_line = text.replace(['\r', '\n'], " ");
    let trimmed = one_line.trim();
    const MAX: usize = 120;
    if trimmed.chars().count() <= MAX {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(MAX).collect();
        format!("{head}…")
    }
}

// ---------------------------------------------------------------------------
// Status mapping (pure)
// ---------------------------------------------------------------------------

/// Map a LinkedIn HTTP status to a friendly, secret-free error. 2xx is `Ok`. The
/// task's mappings: 401 -> reconnect (the OAuth token was rejected/expired);
/// 403 -> the LinkedIn-specific "posting not permitted — check app
/// products/scopes" hint; 422 -> a validation hint; plus the foundation's
/// 404/429/5xx phrasing. The provider body (which can echo member content) is
/// never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    // 401 and 403 both classify as `Unauthorized` in the foundation, but LinkedIn
    // means different things by them, and 422 is its validation code, so we branch
    // on the raw code first.
    match status {
        401 => {
            return Err(anyhow::anyhow!(
                "{what} failed — LinkedIn rejected the access token; reconnect LinkedIn in Settings"
            ))
        }
        403 => {
            return Err(anyhow::anyhow!(
                "{what} failed — LinkedIn posting not permitted; check your LinkedIn app products and scopes, then reconnect LinkedIn in Settings"
            ))
        }
        422 => {
            return Err(anyhow::anyhow!(
                "{what} failed — LinkedIn rejected the post as invalid (it may be too long or empty)"
            ))
        }
        _ => {}
    }
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::NotFound => Err(anyhow::anyhow!("{what} failed — that item was not found")),
        StatusOutcome::RateLimited => {
            Err(anyhow::anyhow!("{what} was rate limited by LinkedIn; try again shortly"))
        }
        StatusOutcome::ServerError => {
            Err(anyhow::anyhow!("{what} failed on LinkedIn's side; this is usually transient"))
        }
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned LinkedIn JSON (realistic API SHAPE, never fetched), and the
// shared ProviderAuth handle is wired over its OWN MockTransport with a canned
// refresh response so `bearer()` works without a network or real token. No
// network, no real LinkedIn round-trip, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::oauth2::{RefreshTokenStore, LINKEDIN};
    use crate::integrations::testing::MockTransport;

    /// Fake credential values that, if leaked, would be unmistakable in an
    /// assertion. None of these is ever asserted to APPEAR — they are scanned for
    /// ABSENCE in produced output.
    const FAKE_CLIENT_ID: &str = "FAKE-LI-CLIENT-ID-1234";
    const FAKE_CLIENT_SECRET: &str = "FAKE-LI-CLIENT-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "FAKE-LI-REFRESH-TOKEN-NEVER-LEAK";
    /// The access token the canned refresh response mints. The LinkedIn client
    /// puts THIS in its Authorization header; tests assert it never lands in
    /// output, a URL, or a body.
    const FAKE_ACCESS: &str = "LI-ACCESS-FAKE-NEVER-LEAK-IN-OUTPUT";

    /// A no-op Keychain store so building a `ProviderAuth` never touches the real
    /// Keychain.
    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    /// Canned LinkedIn refresh response so `auth.bearer()` mints `FAKE_ACCESS`
    /// without a network call. LinkedIn's token endpoint returns an access token +
    /// expiry; we omit a rotated refresh token here.
    fn refresh_ok_json() -> String {
        format!(r#"{{"access_token":"{FAKE_ACCESS}","expires_in":3599,"token_type":"Bearer"}}"#)
    }

    /// Build a LinkedIn `ProviderAuth` handle over its own MockTransport that
    /// answers the token endpoint with a canned access token — the shared handle
    /// the LinkedIn client borrows for `bearer()`.
    fn test_auth() -> ProviderAuth<MockTransport> {
        let token_mock = MockTransport::new().on(
            HttpMethod::Post,
            LINKEDIN.token_endpoint,
            200,
            refresh_ok_json(),
        );
        ProviderAuth::new(
            LINKEDIN,
            token_mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        )
    }

    /// A LinkedIn client whose API transport is `api_mock` and whose auth handle
    /// is a canned-refresh `ProviderAuth`.
    fn client(api_mock: MockTransport) -> LinkedinClient<MockTransport, MockTransport> {
        LinkedinClient::with_auth(api_mock, test_auth())
    }

    // -- realistic canned payloads (hand-written from the LinkedIn API shape) ---

    fn userinfo_json() -> &'static str {
        // /v2/userinfo (OpenID Connect) returns the member id as `sub` plus name.
        r#"{
            "sub":"ABC123xyz",
            "name":"Ada Lovelace",
            "given_name":"Ada",
            "family_name":"Lovelace",
            "locale":{"country":"US","language":"en"}
        }"#
    }

    fn create_post_ok_json() -> &'static str {
        // /rest/posts 201 returns the created post id in the body (LinkedIn also
        // echoes it in the x-restli-id header; we parse neither — a 2xx is enough).
        r#"{"id":"urn:li:share:7012345678901234567"}"#
    }

    // -- READ: me() parses userinfo and derives the author URN ----------------

    #[tokio::test]
    async fn me_parses_userinfo_and_derives_author_urn() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json());
        let member = client(mock).me().await.unwrap();
        assert_eq!(member.name, "Ada Lovelace");
        assert_eq!(member.id, "ABC123xyz");
        // The author URN the Posts API needs is derived from `sub`.
        assert_eq!(member.author_urn, "urn:li:person:ABC123xyz");
    }

    #[tokio::test]
    async fn me_hits_userinfo_with_auth_header() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json());
        let c = client(mock);
        c.me().await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        assert!(req.url.contains("/v2/userinfo"), "url: {}", req.url);
        assert!(req.has_header("authorization"), "auth attached (value not asserted)");
    }

    #[tokio::test]
    async fn me_errors_when_sub_missing() {
        // A userinfo body without `sub` cannot yield an author URN -> friendly err.
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/v2/userinfo",
            200,
            r#"{"name":"No Id Person"}"#,
        );
        let err = client(mock).me().await.unwrap_err().to_string();
        assert!(err.contains("member id"), "got: {err}");
        assert!(err.contains("reconnect LinkedIn"), "got: {err}");
    }

    // -- CONSEQUENTIAL: DryRun issues NO request, previews --------------------

    #[tokio::test]
    async fn create_post_dry_run_posts_nothing_and_previews() {
        // Register both endpoints so that, if DryRun mistakenly sent, it would be
        // recorded — proving by absence that nothing went out.
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json())
            .on(HttpMethod::Post, "/rest/posts", 201, create_post_ok_json());
        let c = client(mock);
        let out = c
            .create_post("Shipping a new feature today!", ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("PUBLIC"), "preview notes PUBLIC visibility: {out}");
        assert!(
            out.contains("Shipping a new feature today!"),
            "preview shows the post text: {out}"
        );
        // The CRUX: NO request was ever issued in DryRun (not even the me() read
        // nor a bearer fetch on the LinkedIn transport).
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not issue any LinkedIn request"
        );
    }

    // -- CONSEQUENTIAL: Execute issues exactly one create-post ----------------

    #[tokio::test]
    async fn create_post_execute_issues_one_post_with_required_shape_and_headers() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json())
            .on(HttpMethod::Post, "/rest/posts", 201, create_post_ok_json());
        let c = client(mock);
        let out = c
            .create_post("Hello, network!", ActionMode::Execute)
            .await
            .unwrap();
        assert!(out.contains("published"), "got: {out}");

        // Two LinkedIn requests in order: the me() read, then exactly one
        // create-post POST.
        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 2, "exactly one read + one post");
        assert_eq!(reqs[0].method, HttpMethod::Get);
        assert!(reqs[0].url.contains("/v2/userinfo"));

        let post = &reqs[1];
        assert_eq!(post.method, HttpMethod::Post);
        assert!(post.url.ends_with("/rest/posts"), "url: {}", post.url);
        assert!(post.has_header("authorization"), "auth attached");
        // The two headers the versioned Posts API requires.
        assert!(post.has_header("LinkedIn-Version"), "LinkedIn-Version header required");
        assert!(
            post.has_header("X-Restli-Protocol-Version"),
            "X-Restli-Protocol-Version header required"
        );

        // The body carries the author URN (derived from me()), the text, and
        // PUBLIC visibility.
        let body = post.body.as_ref().expect("post has a JSON body");
        assert_eq!(body["author"], "urn:li:person:ABC123xyz");
        assert_eq!(body["commentary"], "Hello, network!");
        assert_eq!(body["visibility"], "PUBLIC");
        assert_eq!(body["lifecycleState"], "PUBLISHED");
    }

    /// The exact header VALUES are the documented protocol constants (asserted via
    /// the recorded request, not by reading any secret).
    #[tokio::test]
    async fn create_post_sends_the_documented_header_values() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json())
            .on(HttpMethod::Post, "/rest/posts", 201, create_post_ok_json());
        let c = client(mock);
        c.create_post("x", ActionMode::Execute).await.unwrap();
        let post = &c.transport.requests()[1];
        let version = post
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("LinkedIn-Version"))
            .map(|(_, v)| v.as_str());
        let restli = post
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("X-Restli-Protocol-Version"))
            .map(|(_, v)| v.as_str());
        assert_eq!(version, Some(LINKEDIN_VERSION));
        assert_eq!(restli, Some(RESTLI_PROTOCOL_VERSION));
    }

    // -- error mapping --------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_401_maps_to_reconnect() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/v2/userinfo", 401, "{}");
        let err = client(mock).me().await.unwrap_err().to_string();
        assert!(err.contains("reconnect LinkedIn"), "got: {err}");
        assert!(err.contains("access token"), "got: {err}");
    }

    #[tokio::test]
    async fn forbidden_403_maps_to_posting_not_permitted() {
        // 403 on the create-post path -> the LinkedIn app/products/scopes hint.
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json())
            .on(HttpMethod::Post, "/rest/posts", 403, "{}");
        let err = client(mock)
            .create_post("hi", ActionMode::Execute)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("posting not permitted"), "got: {err}");
        assert!(err.contains("products"), "got: {err}");
    }

    #[tokio::test]
    async fn validation_422_maps_to_validation_hint() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json())
            .on(HttpMethod::Post, "/rest/posts", 422, "{}");
        let err = client(mock)
            .create_post("hi", ActionMode::Execute)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/v2/userinfo", 503, "down");
        let err = client(mock).me().await.unwrap_err().to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- NOTHING secret is ever logged / leaked into output -------------------

    /// Neither the access token nor the refresh token appears in a produced
    /// outcome string, in a mapped error, or in the client's Debug. We drive a
    /// representative slice of the surface and scan every produced string. The
    /// access token must live ONLY in the Authorization header on the wire — never
    /// in a URL or a body.
    #[tokio::test]
    async fn no_token_ever_leaks() {
        // Debug of the client notes nothing secret.
        let dbg = format!("{:?}", client(MockTransport::new()));
        assert!(!dbg.contains(FAKE_ACCESS), "Debug leaked the access token: {dbg}");
        assert!(!dbg.contains(FAKE_REFRESH), "Debug leaked the refresh token: {dbg}");

        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/v2/userinfo", 200, userinfo_json())
            .on(HttpMethod::Post, "/rest/posts", 201, create_post_ok_json());
        let c = client(mock);

        let me = format!("{:?}", c.me().await.unwrap());
        let posted = c.create_post("Public words.", ActionMode::Execute).await.unwrap();
        let preview = c.create_post("Public words.", ActionMode::DryRun).await.unwrap();

        for s in [&me, &posted, &preview] {
            assert!(!s.contains(FAKE_ACCESS), "output leaked the access token: {s}");
            assert!(!s.contains(FAKE_REFRESH), "output leaked the refresh token: {s}");
            assert!(!s.contains(FAKE_CLIENT_SECRET), "output leaked the client secret: {s}");
        }

        // On the wire, the access token rides ONLY in the Authorization header —
        // never in a URL or a request body.
        for req in c.transport.requests() {
            assert!(!req.url.contains(FAKE_ACCESS), "token must not be in a URL: {}", req.url);
            if let Some(body) = &req.body {
                assert!(
                    !body.to_string().contains(FAKE_ACCESS),
                    "token must not be in a body"
                );
            }
        }

        // An error path never leaks a token either.
        let err_mock = MockTransport::new().on(HttpMethod::Get, "/v2/userinfo", 401, "{}");
        let err = client(err_mock).me().await.unwrap_err().to_string();
        assert!(!err.contains(FAKE_ACCESS), "error leaked the access token: {err}");
        assert!(!err.contains(FAKE_REFRESH), "error leaked the refresh token: {err}");
    }

    // -- pure helpers ---------------------------------------------------------

    #[test]
    fn author_urn_is_person_form() {
        assert_eq!(author_urn("ABC123"), "urn:li:person:ABC123");
    }

    #[test]
    fn post_body_has_required_fields() {
        let body = post_body("urn:li:person:ABC123", "the post text");
        assert_eq!(body["author"], "urn:li:person:ABC123");
        assert_eq!(body["commentary"], "the post text");
        assert_eq!(body["visibility"], "PUBLIC");
        assert_eq!(body["lifecycleState"], "PUBLISHED");
        assert_eq!(body["isReshareDisabledByAuthor"], false);
        assert_eq!(body["distribution"]["feedDistribution"], "MAIN_FEED");
    }

    #[test]
    fn preview_collapses_and_caps() {
        assert_eq!(preview("one line"), "one line");
        assert_eq!(preview("a\nb\r\nc"), "a b  c");
        let long = "x".repeat(200);
        assert!(preview(&long).ends_with('…'));
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(201, "x").is_ok());
        assert!(map_status(401, "x").unwrap_err().to_string().contains("reconnect LinkedIn"));
        assert!(map_status(403, "x")
            .unwrap_err()
            .to_string()
            .contains("posting not permitted"));
        assert!(map_status(422, "x").unwrap_err().to_string().contains("invalid"));
        assert!(map_status(404, "x").unwrap_err().to_string().contains("not found"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(500, "x").unwrap_err().to_string().contains("transient"));
    }
}
