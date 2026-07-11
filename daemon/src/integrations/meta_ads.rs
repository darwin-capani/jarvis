//! Meta (Facebook) Ads OAuth2 auth — the SECOND ads provider this round.
//!
//! Meta's token model differs from the generic OAuth2 providers in `oauth2.rs`:
//! there is NO refresh token. The installed-app dance yields a SHORT-lived user
//! access token, which you then EXCHANGE (once, server-side) for a LONG-lived
//! (~60-day) token. JARVIS stores THAT long-lived token in the Keychain and uses
//! it directly as the bearer — there is no silent refresh. When it expires the
//! user must reconnect, so [`MetaAuth::access_token`] returns a friendly
//! "reconnect in Settings" error rather than attempting a refresh.
//!
//! Because of that single difference, Meta gets its own small handle here
//! ([`MetaAuth`]) rather than bending the refresh-token-shaped [`oauth2::ProviderAuth`]
//! out of shape. It REUSES every provider-agnostic, already-audited piece of
//! `oauth2.rs` it can:
//!
//!   * the loopback redirect handler ([`oauth2::receive_redirect`]) that
//!     CSRF-checks `state` BEFORE trusting a code;
//!   * the injectable [`oauth2::RandomSource`] (`/dev/urandom` in production,
//!     fixed bytes in tests) + [`oauth2::generate_state`] for the CSRF state;
//!   * the percent-encoder + the `127.0.0.1:<port>` redirect URI;
//!   * the injected browser [`oauth2::UrlOpener`] and [`oauth2::ConsentOutcome`].
//!
//! SECURITY POSTURE (identical to the foundation / `oauth2.rs`):
//!   * app_secret, the authorization code, and BOTH the short-lived and
//!     long-lived tokens are NEVER logged, never in an error/Debug/tracing field,
//!     never on a URL or argv. The long-lived token lives ONLY in the Keychain;
//!     the in-memory copy is held behind a Mutex and redacted in `Debug`.
//!   * The CSRF `state` is validated on the redirect; the listener is loopback-
//!     only and one-shot. Randomness is injectable for deterministic tests.
//!   * Every token call in tests goes through the foundation's `MockTransport`
//!     with canned JSON — zero network. The Keychain WRITE is injected so tests
//!     never touch the real Keychain.

use std::sync::Mutex;

use serde::Deserialize;
use tracing::info;

use super::oauth2::{
    self, generate_state, percent_encode, receive_redirect, redirect_uri, ConsentOutcome,
    OsEntropy, RandomSource, RedirectOutcome, RefreshTokenStore, UrlOpener,
};
use super::{
    resolve_secret, status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport,
    IntegrationResult, StatusOutcome,
};

// ===========================================================================
// (0) Provider constants
// ===========================================================================

/// The Graph API version Meta's OAuth + Ads endpoints are pinned to. Bumping this
/// in one place moves both the dialog and the token endpoint together.
pub const META_GRAPH_VERSION: &str = "v21.0";

/// Meta's OAuth consent ("login dialog") endpoint — where the browser is sent.
pub const META_AUTH_ENDPOINT: &str = "https://www.facebook.com/v21.0/dialog/oauth";

/// Meta's Graph token endpoint — used for BOTH the code->short-lived exchange and
/// the short->long-lived exchange (the call differs only by query params).
pub const META_TOKEN_ENDPOINT: &str = "https://graph.facebook.com/v21.0/oauth/access_token";

/// Meta Ads OAuth scopes. `ads_read` covers reporting/read; `ads_management`
/// covers the consequential mutations (which still route through the foundation's
/// gate). Least-privilege for an ads agent.
pub const META_SCOPES: &[&str] = &["ads_read", "ads_management"];

/// Keychain account names for Meta Ads. `meta_app_id` / `meta_app_secret` are the
/// app credentials the user pastes in Settings; `meta_long_lived_token` is the
/// ~60-day token JARVIS WRITES after the short->long exchange (the ONLY token
/// persisted — there is no refresh token); `meta_ad_account_id` is the ad account
/// the calls target (e.g. `act_1234567890`). Mirrored on the foundation allowlist
/// in `mod.rs`.
pub const META_ACCOUNT_APP_ID: &str = "meta_app_id";
pub const META_ACCOUNT_APP_SECRET: &str = "meta_app_secret";
pub const META_ACCOUNT_LONG_LIVED_TOKEN: &str = "meta_long_lived_token";
pub const META_ACCOUNT_AD_ACCOUNT_ID: &str = "meta_ad_account_id";

/// The provider name, for friendly messages/logs.
const META_NAME: &str = "Meta Ads";

// ===========================================================================
// (1) Auth URL assembly — pure
// ===========================================================================

/// Build Meta's consent ("dialog/oauth") URL against `app_id`, the loopback
/// `port`, and the CSRF `state`. PURE — no I/O, no secret. The app SECRET is never
/// part of an auth URL (asserted in tests); only the app id, redirect, scopes and
/// state ride here. Meta's dialog uses `client_id` for the app id and a
/// space-joined `scope`.
pub fn build_auth_url(app_id: &str, port: u16, state: &str) -> String {
    let scope = META_SCOPES.join(" ");
    let redirect = redirect_uri(port);
    format!(
        "{META_AUTH_ENDPOINT}?\
         response_type=code&\
         client_id={}&\
         redirect_uri={}&\
         scope={}&\
         state={}",
        percent_encode(app_id),
        percent_encode(&redirect),
        percent_encode(&scope),
        percent_encode(state),
    )
}

// ===========================================================================
// (2) Token-endpoint response shapes
// ===========================================================================

/// Meta's token-endpoint success JSON (`{"access_token":"…","token_type":"bearer",
/// "expires_in":5184000}`). We decode only the token + lifetime; `expires_in` may
/// be absent on some responses, so it defaults to 0 (logged as a bool/number, the
/// token value never logged).
#[derive(Debug, Deserialize)]
struct MetaTokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    expires_in: i64,
}

/// Meta's token-endpoint error JSON (`{"error":{"message":"…","type":"…",
/// "code":190}}`). We extract only the fixed `type`/`code` (not the message, which
/// can echo input); these are not secret.
#[derive(Debug, Default, Deserialize)]
struct MetaErrorEnvelope {
    #[serde(default)]
    error: MetaError,
}

#[derive(Debug, Default, Deserialize)]
struct MetaError {
    #[serde(rename = "type", default)]
    error_type: String,
    #[serde(default)]
    code: i64,
}

// ===========================================================================
// (3) The MetaAuth handle
// ===========================================================================

/// The in-memory long-lived token. Held behind a Mutex inside [`MetaAuth`]; never
/// logged (only presence as a bool).
#[derive(Default)]
struct CachedToken {
    long_lived_token: String,
}

/// Meta Ads auth handle. Owns the app id/secret and (when connected) the stored
/// long-lived token, and exposes [`Self::access_token`] which returns that stored
/// token — NO silent refresh (Meta has no refresh token). The short->long exchange
/// happens once, during consent, in [`Self::exchange_for_long_lived`].
///
/// Generic over the foundation's [`HttpTransport`] so production wires
/// `ReqwestTransport` and tests wire `MockTransport`. `Debug` redacts every secret.
pub struct MetaAuth<T: HttpTransport> {
    /// The injected HTTP seam for the Graph token endpoint. `pub(crate)` so sibling
    /// test modules can introspect recorded token-endpoint requests (by presence,
    /// not value).
    pub(crate) transport: T,
    app_id: String,
    app_secret: String,
    /// The stored long-lived token (memory copy). Empty BEFORE consent / when not
    /// connected.
    cached: Mutex<CachedToken>,
    /// Injected Keychain writer for the long-lived token.
    store: RefreshTokenStore,
}

impl<T: HttpTransport> std::fmt::Debug for MetaAuth<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let has_token = self
            .cached
            .lock()
            .map(|c| !c.long_lived_token.is_empty())
            .unwrap_or(false);
        f.debug_struct("MetaAuth")
            .field("app_id_present", &!self.app_id.is_empty())
            .field("app_secret_present", &!self.app_secret.is_empty())
            .field("long_lived_token_present", &has_token)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> MetaAuth<T> {
    /// Build a handle from explicit credentials + transport + Keychain store. Used
    /// by tests (mock transport, fake creds, recording store) and by the
    /// production constructors. `long_lived_token` may be empty for the pre-consent
    /// flow.
    pub fn new(
        transport: T,
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        long_lived_token: impl Into<String>,
        store: RefreshTokenStore,
    ) -> Self {
        Self {
            transport,
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            cached: Mutex::new(CachedToken {
                long_lived_token: long_lived_token.into(),
            }),
            store,
        }
    }

    /// Return the stored long-lived access token for the caller to put in its
    /// `Authorization: Bearer` header (or the `access_token` query param Meta
    /// accepts). NO refresh: if there is no stored token, the friendly
    /// "Meta token expired — reconnect in Settings" error is returned (the user
    /// must re-run consent). The token VALUE is returned to the caller for one
    /// request but never logged here.
    pub fn access_token(&self) -> IntegrationResult<String> {
        let token = self
            .cached
            .lock()
            .map(|c| c.long_lived_token.clone())
            .unwrap_or_default();
        if token.is_empty() {
            return Err(meta_expired_error());
        }
        Ok(token)
    }

    /// Exchange a freshly-consented authorization `code` for a SHORT-lived token,
    /// then immediately exchange that for a LONG-lived (~60-day) token, store the
    /// long-lived token via the injected store, and cache it in memory. `port` is
    /// the loopback port used at consent — Meta requires the exchange's
    /// `redirect_uri` to byte-match the dialog's. The app_secret, code and both
    /// tokens ride only in query params to Graph and are NEVER logged.
    pub async fn exchange_for_long_lived(&self, code: &str, port: u16) -> IntegrationResult<()> {
        // Leg 1: code -> short-lived user access token.
        let short = self.exchange_code_for_short(code, port).await?;
        // Leg 2: short-lived -> long-lived (~60 day) token.
        let long = self.exchange_short_for_long(&short).await?;
        if long.is_empty() {
            return Err(anyhow::anyhow!(
                "Meta did not return a long-lived token — reconnect in Settings"
            ));
        }
        // Persist off the async worker: the store drives a synchronous security(1)
        // child (see `super::keychain_write`), so run it on the blocking pool rather
        // than pinning this runtime thread for the write's duration.
        {
            let store = self.store.clone();
            let token = long.clone();
            tokio::task::spawn_blocking(move || store(&token))
                .await
                .map_err(|e| anyhow::anyhow!("keychain write task failed: {e}"))??;
        }
        if let Ok(mut c) = self.cached.lock() {
            c.long_lived_token = long;
        }
        info!(provider = META_NAME, "meta: short token exchanged for long-lived; stored");
        Ok(())
    }

    // -- internals -----------------------------------------------------------

    /// Leg 1: `GET /oauth/access_token?client_id&client_secret&redirect_uri&code`
    /// -> short-lived user access token. (Meta's token endpoint is a GET with query
    /// params; the secret + code ride in the query to Graph over TLS, never logged.)
    async fn exchange_code_for_short(&self, code: &str, port: u16) -> IntegrationResult<String> {
        let url = format!(
            "{META_TOKEN_ENDPOINT}?\
             client_id={}&\
             client_secret={}&\
             redirect_uri={}&\
             code={}",
            percent_encode(&self.app_id),
            percent_encode(&self.app_secret),
            percent_encode(&redirect_uri(port)),
            percent_encode(code),
        );
        let tokens = self
            .get_token(&url, "exchanging the authorization code")
            .await?;
        if tokens.access_token.is_empty() {
            return Err(anyhow::anyhow!(
                "Meta returned no short-lived token for the authorization code"
            ));
        }
        Ok(tokens.access_token)
    }

    /// Leg 2: `GET /oauth/access_token?grant_type=fb_exchange_token&client_id&
    /// client_secret&fb_exchange_token=<short>` -> long-lived (~60 day) token.
    async fn exchange_short_for_long(&self, short_token: &str) -> IntegrationResult<String> {
        let url = format!(
            "{META_TOKEN_ENDPOINT}?\
             grant_type=fb_exchange_token&\
             client_id={}&\
             client_secret={}&\
             fb_exchange_token={}",
            percent_encode(&self.app_id),
            percent_encode(&self.app_secret),
            percent_encode(short_token),
        );
        let tokens = self
            .get_token(&url, "exchanging for a long-lived Meta token")
            .await?;
        Ok(tokens.access_token)
    }

    /// GET a Graph token URL and decode the result, mapping non-2xx + Meta's
    /// `OAuthException` (code 190 = expired/invalid token) to friendly, secret-free
    /// errors. The URL (which carries the secret + tokens in its query) is NEVER
    /// logged — only the action name + status.
    async fn get_token(&self, url: &str, what: &str) -> IntegrationResult<MetaTokenResponse> {
        let req = HttpRequest::new(HttpMethod::Get, url.to_string());
        // DEFENSE IN DEPTH over the shared transport's own URL-stripping: `url`
        // here carries the app_secret + code/short-token in its QUERY. Never let a
        // transport-level error propagate the request URL out of this function —
        // map ANY transport error to a fixed, secret-free message (the shared
        // ReqwestTransport already strips reqwest's attached URL, but a future or
        // alternate transport must not be able to reopen the leak through this
        // secret-bearing call). This error is what reaches the daemon log AND the
        // cloud-bound tool outcome on a flaky/intercepted network, so it must hold
        // no credential bytes.
        let resp = self
            .transport
            .send(req)
            .await
            .map_err(|_| anyhow::anyhow!("the network request to Meta failed while {what}"))?;
        if !resp.is_success() {
            let env = serde_json::from_str::<MetaErrorEnvelope>(&resp.body).unwrap_or_default();
            return Err(map_meta_error(resp.status, &env.error, what));
        }
        serde_json::from_str::<MetaTokenResponse>(&resp.body)
            .map_err(|_| anyhow::anyhow!("{what} returned an unexpected response"))
    }
}

impl MetaAuth<super::ReqwestTransport> {
    /// Production constructor: resolve app id + app secret + the stored long-lived
    /// token from the Keychain and wire the real reqwest transport + the real
    /// Keychain writer. Returns the friendly "Meta Ads isn't connected" error when
    /// the app credentials are missing, or the "token expired — reconnect" error
    /// when only the long-lived token is missing.
    pub async fn connect() -> IntegrationResult<Self> {
        let app_id = resolve_secret(META_ACCOUNT_APP_ID)
            .await
            .ok_or_else(meta_not_connected_error)?;
        let app_secret = resolve_secret(META_ACCOUNT_APP_SECRET)
            .await
            .ok_or_else(meta_not_connected_error)?;
        let long_lived = resolve_secret(META_ACCOUNT_LONG_LIVED_TOKEN)
            .await
            .ok_or_else(meta_expired_error)?;
        Ok(Self::new(
            super::ReqwestTransport::new(),
            app_id,
            app_secret,
            long_lived,
            oauth2::keychain_store(META_ACCOUNT_LONG_LIVED_TOKEN),
        ))
    }

    /// Pre-consent constructor for the CONNECT flow: resolves only app id + app
    /// secret (the long-lived token does not exist yet — consent will mint it).
    /// Returns the friendly "Meta Ads isn't connected" error if the app
    /// credentials have not been pasted in Settings.
    pub async fn connect_for_consent() -> IntegrationResult<Self> {
        let app_id = resolve_secret(META_ACCOUNT_APP_ID)
            .await
            .ok_or_else(meta_not_connected_error)?;
        let app_secret = resolve_secret(META_ACCOUNT_APP_SECRET)
            .await
            .ok_or_else(meta_not_connected_error)?;
        Ok(Self::new(
            super::ReqwestTransport::new(),
            app_id,
            app_secret,
            String::new(),
            oauth2::keychain_store(META_ACCOUNT_LONG_LIVED_TOKEN),
        ))
    }
}

/// Everything a Meta Ads call needs: the access token (the stored long-lived
/// token) and the ad account id (e.g. `act_1234567890`). Resolved by
/// [`meta_ads_call`]. `Debug` redacts the token (a secret); the ad account id is
/// an identifier, not a secret.
#[derive(Clone)]
pub struct MetaAdsCall {
    /// The long-lived access token — a SECRET. Sent as the bearer; never logged.
    pub access_token: String,
    /// The ad account id the call targets (e.g. `act_1234567890`). Not a secret.
    pub ad_account_id: String,
}

impl std::fmt::Debug for MetaAdsCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaAdsCall")
            .field("access_token_present", &!self.access_token.is_empty())
            .field("ad_account_id", &self.ad_account_id)
            .finish()
    }
}

/// Resolve everything a Meta Ads call needs from a connected [`MetaAuth`] + the
/// Keychain: the long-lived access token (via [`MetaAuth::access_token`], which
/// surfaces the friendly "expired — reconnect" error when absent) and the ad
/// account id. Returns the "not fully configured" error when the ad account id is
/// missing. No secret is logged.
pub async fn meta_ads_call<T: HttpTransport>(auth: &MetaAuth<T>) -> IntegrationResult<MetaAdsCall> {
    let access_token = auth.access_token()?;
    let ad_account_id = resolve_secret(META_ACCOUNT_AD_ACCOUNT_ID)
        .await
        .ok_or_else(meta_not_configured_error)?;
    Ok(MetaAdsCall {
        access_token,
        ad_account_id,
    })
}

// ===========================================================================
// (4) Runtime consent orchestrator — the production entry point
// ===========================================================================

/// Binding `127.0.0.1:0` lets the OS pick any free ephemeral port, read back with
/// `local_addr()` so the redirect URI and the token exchange use the SAME port
/// (Meta requires byte-equality). No fixed/known port is ever bound.
const LOOPBACK_BIND_ADDR: &str = "127.0.0.1:0";

/// Run the FULL installed-app consent flow end to end for Meta and store the
/// resulting long-lived token. This is the production runtime entry point the
/// daemon's `connect_meta_ads` tool calls; it ties together the pure, unit-tested
/// pieces and the `oauth2.rs` loopback machinery:
///
///   1. Bind the loopback on `127.0.0.1:0` (OS-picked free port) and read it back.
///   2. [`build_auth_url`] builds the dialog URL against that port with a fresh
///      CSRF `state` drawn from [`OsEntropy`] (Meta's dialog uses no PKCE).
///   3. `open` launches the URL in the user's browser (injected).
///   4. [`oauth2::receive_redirect`] accepts exactly one loopback request,
///      validates the state (CSRF) and extracts the code (or a `?error=`).
///   5. On a code, [`MetaAuth::exchange_for_long_lived`] does the short->long
///      exchange (same port's redirect URI) and (via the injected Keychain store)
///      persists the long-lived token.
///
/// No secret is ever logged: presence/port/outcome only. A `Declined` redirect is
/// `Ok(ConsentOutcome::Declined(..))`; every other failure is a friendly,
/// secret-free `Err`.
pub async fn run_meta_consent_flow<T: HttpTransport>(
    auth: &MetaAuth<T>,
    open: UrlOpener<'_>,
) -> IntegrationResult<ConsentOutcome> {
    run_meta_consent_flow_with_rng(auth, open, &OsEntropy).await
}

/// Inner consent flow with an injectable [`RandomSource`], so a test can drive the
/// CSRF state deterministically while production passes [`OsEntropy`].
async fn run_meta_consent_flow_with_rng<T: HttpTransport>(
    auth: &MetaAuth<T>,
    open: UrlOpener<'_>,
    rng: &dyn RandomSource,
) -> IntegrationResult<ConsentOutcome> {
    let listener = tokio::net::TcpListener::bind(LOOPBACK_BIND_ADDR)
        .await
        .map_err(|e| anyhow::anyhow!("could not open the local consent listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("could not read the local consent port: {e}"))?
        .port();

    let state = generate_state(rng);
    let url = build_auth_url(&auth.app_id, port, &state);

    open(&url).await?;
    info!(provider = META_NAME, port, "meta: opened consent URL; awaiting redirect");

    match receive_redirect(listener, &state, META_NAME).await? {
        RedirectOutcome::Denied(err) => {
            info!(provider = META_NAME, "meta: consent declined");
            Ok(ConsentOutcome::Declined(err))
        }
        RedirectOutcome::Code(code) => {
            auth.exchange_for_long_lived(&code, port).await?;
            Ok(ConsentOutcome::Connected)
        }
    }
}

// ===========================================================================
// Friendly, secret-free errors
// ===========================================================================

/// The "Meta Ads isn't connected" error — app credentials missing. Names the two
/// steps the user takes. Carries no secret.
pub fn meta_not_connected_error() -> super::IntegrationError {
    anyhow::anyhow!(
        "Meta Ads isn't connected — add your Meta app in Settings and say 'connect Meta'"
    )
}

/// The "Meta token expired — reconnect" error — there is NO silent refresh, so an
/// absent/expired long-lived token means the user must re-run consent.
pub fn meta_expired_error() -> super::IntegrationError {
    anyhow::anyhow!("Meta token expired — reconnect in Settings")
}

/// The "Meta Ads isn't fully configured" error — the ad account id is missing.
pub fn meta_not_configured_error() -> super::IntegrationError {
    anyhow::anyhow!("Meta Ads isn't fully configured — add the ad account id in Settings")
}

/// Map a Graph token failure to a friendly, secret-free error. Meta's
/// `OAuthException` (error code 190) is the "token expired or revoked" case and
/// gets the reconnect hint; other failures lean on the status mapper. The Graph
/// error message is never included (it can echo input).
fn map_meta_error(status: u16, err: &MetaError, what: &str) -> super::IntegrationError {
    if err.code == 190 || err.error_type == "OAuthException" {
        return anyhow::anyhow!(
            "{what} failed — Meta rejected the token (it may have expired or been revoked); reconnect Meta in Settings"
        );
    }
    match status_outcome(status) {
        StatusOutcome::Unauthorized => {
            anyhow::anyhow!("{what} failed — the Meta app id or secret was rejected")
        }
        other => anyhow::anyhow!("{what} {}", other.friendly()),
    }
}

// ===========================================================================
// (5) The Meta Ads CLIENT — Marketing API on the Graph API, for stark/gecko
// ===========================================================================
//
// Thin, typed wrapper over the foundation's [`HttpTransport`] (so production
// wires [`ReqwestTransport`] and tests wire `MockTransport` — zero network in
// tests). The client holds a resolved [`MetaAdsCall`] (the long-lived access
// token + the `act_…` ad account id) and attaches the bearer HERE, per request,
// at the moment of the send — never on the transport, never logged, never in an
// error or a `Debug` field. Meta accepts the token either as `Authorization:
// Bearer …` or as an `access_token` query param; we use the header so the token
// is NEVER on a URL (a logged URL must never carry it). Only that an auth handle
// is attached is ever recorded.
//
// Two tiers, mirroring the foundation's safety model:
//   * READ (safe — no gate): [`MetaAdsClient::report_campaigns`] (a concise
//     spend report joining campaign names with insights) and
//     [`MetaAdsClient::list_campaigns`] (names + status + daily budget) — plain
//     GETs, no side effects.
//   * CONSEQUENTIAL (gated — touches MONEY): [`MetaAdsClient::pause_campaign`],
//     [`MetaAdsClient::resume_campaign`] and [`MetaAdsClient::set_campaign_budget`]
//     each take an [`ActionMode`]: in [`ActionMode::DryRun`] they build and return
//     a clear PREVIEW of the EXACT change and issue NO request; only in
//     [`ActionMode::Execute`] do they issue exactly one `POST /{campaign_id}`.
//     Call sites get the mode from the foundation's `gate(confirm)`, so with
//     `[integrations].allow_consequential` false (the shipped default) every
//     mutation previews and changes nothing.

/// Meta Graph API base. The Marketing API lives on the Graph API; all paths are
/// appended to this. Pinned to the same version as the OAuth endpoints via
/// [`META_GRAPH_VERSION`] so a single bump moves the whole surface together.
const GRAPH_API_BASE: &str = "https://graph.facebook.com/v21.0";

/// How many campaigns / insight rows one read may ask Graph for. The Graph API
/// paginates with a `limit`; we clamp the caller's request into a sane band so a
/// bad `max` can never 4xx the call or pull an unbounded page.
const META_MIN_RESULTS: u32 = 1;
const META_MAX_RESULTS: u32 = 100;

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields stark/gecko actually need are decoded.
// `#[serde(default)]` on the soft fields keeps parsing resilient to the many
// extra keys the Graph API returns and to objects that omit an optional field.
// ---------------------------------------------------------------------------

/// One campaign, as returned in the `data` array of
/// `GET /act_{id}/campaigns?fields=name,status,objective,daily_budget`. `daily_budget`
/// is a string of MINOR currency units (cents) on the wire, absent for campaigns
/// budgeted at the ad-set level.
#[derive(Debug, Clone, Deserialize, Default)]
struct MetaCampaign {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    objective: String,
    #[serde(default)]
    daily_budget: String,
}

/// One insights row, as returned in the `data` array of
/// `GET /act_{id}/insights?fields=campaign_name,spend,impressions,clicks`. Every
/// numeric is a string on the wire; we keep them as strings (we only echo them in
/// a spoken summary, never do arithmetic that would need parsing).
#[derive(Debug, Clone, Deserialize, Default)]
struct MetaInsight {
    #[serde(default)]
    campaign_name: String,
    #[serde(default)]
    spend: String,
    #[serde(default)]
    impressions: String,
    #[serde(default)]
    clicks: String,
}

/// The `{"data": [ ... ]}` envelope Graph wraps a list endpoint in. The array is
/// absent when there are no results, so it defaults to empty.
#[derive(Debug, Clone, Deserialize, Default)]
struct DataList<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
}

/// Graph's error envelope (`{"error":{"message":"…","type":"OAuthException",
/// "code":190}}`). We extract only the fixed `type`/`code` (not the message,
/// which can echo input); these are not secret. Reuses the same shape the auth
/// layer maps, but decoded locally so the client's reads/mutations get the same
/// friendly mapping.
#[derive(Debug, Default, Deserialize)]
struct GraphErrorEnvelope {
    #[serde(default)]
    error: GraphError,
}

#[derive(Debug, Default, Deserialize)]
struct GraphError {
    #[serde(rename = "type", default)]
    error_type: String,
    #[serde(default)]
    code: i64,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Meta Ads (Marketing API) client bound to a transport and a resolved
/// [`MetaAdsCall`] (the long-lived access token + the `act_…` ad account id).
///
/// Construct with [`MetaAdsClient::with_call`] (tests: a `MockTransport` + a
/// hand-built `MetaAdsCall`) or [`MetaAdsClient::connect`] (production: a real
/// `ReqwestTransport` paired with the [`MetaAdsCall`] resolved from a connected
/// [`MetaAuth`] + the Keychain). The token lives only in the held `MetaAdsCall`,
/// is attached per request, and is never logged — `Debug` notes only its
/// presence and the (non-secret) ad account id.
pub struct MetaAdsClient<T: HttpTransport> {
    transport: T,
    call: MetaAdsCall,
}

/// `Debug` reports only that a token is present plus the (non-secret) ad account
/// id — the access token itself is never printed.
impl<T: HttpTransport> std::fmt::Debug for MetaAdsClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetaAdsClient")
            .field("access_token_present", &!self.call.access_token.is_empty())
            .field("ad_account_id", &self.call.ad_account_id)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> MetaAdsClient<T> {
    /// Build a client over `transport` from an already-resolved [`MetaAdsCall`].
    /// Used by tests (mock transport, hand-built call) and by
    /// [`MetaAdsClient::connect`] internally. No secret is resolved here — the
    /// bearer rides in the call and is attached per request.
    pub fn with_call(transport: T, call: MetaAdsCall) -> Self {
        Self { transport, call }
    }

    /// Compose a request to a Graph path with the Meta bearer attached HERE — the
    /// only place the token touches a request, and never on the URL. The path is
    /// appended to the versioned Graph base. The token is never logged.
    fn request(&self, method: HttpMethod, path: &str) -> HttpRequest {
        HttpRequest::new(method, format!("{GRAPH_API_BASE}{path}")).header(
            "Authorization",
            format!("Bearer {}", self.call.access_token),
        )
    }

    /// The `act_…`-prefixed ad account node these calls hang off, e.g.
    /// `act_1234567890`. Graph requires the `act_` prefix; we add it only if the
    /// stored id is missing it, so either form in Settings works.
    fn account_node(&self) -> String {
        let id = &self.call.ad_account_id;
        if id.starts_with("act_") {
            id.clone()
        } else {
            format!("act_{id}")
        }
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// List the ad account's campaigns (up to `max`, clamped to 1..=100): name,
    /// status, and daily budget. Read-only: one
    /// `GET /act_{id}/campaigns?fields=name,status,daily_budget`. Returns a count
    /// plus the first few campaigns, each with its status and (when set) daily
    /// budget rendered from the minor units Graph reports.
    pub async fn list_campaigns(&self, max: u32) -> IntegrationResult<String> {
        let want = max.clamp(META_MIN_RESULTS, META_MAX_RESULTS);
        let req = self.request(
            HttpMethod::Get,
            &format!(
                "/{}/campaigns?fields=name,status,daily_budget&limit={want}",
                self.account_node()
            ),
        );
        let resp = self.transport.send(req).await?;
        map_meta_status(resp.status, &resp.body, "listing your Meta campaigns")?;

        let list: DataList<MetaCampaign> = serde_json::from_str(&resp.body).map_err(|_| {
            anyhow::anyhow!("listing your Meta campaigns returned an unexpected response")
        })?;
        info!(count = list.data.len(), "meta: listed campaigns");
        Ok(summarize_campaigns(&list.data))
    }

    /// A concise SPEND report for the ad account (up to `max` campaigns, clamped
    /// to 1..=100). Read-only: it reads campaign names/status via
    /// `GET /act_{id}/campaigns` and the spend/impressions/clicks via
    /// `GET /act_{id}/insights`, then joins them into one spoken summary. If the
    /// insights endpoint returns no rows (e.g. no spend in the window) it falls
    /// back to just the campaign roster so the caller still gets something useful.
    pub async fn report_campaigns(&self, max: u32) -> IntegrationResult<String> {
        let want = max.clamp(META_MIN_RESULTS, META_MAX_RESULTS);

        // Leg 1: the campaign roster (name + status), so a zero-spend campaign
        // still shows up in the report.
        let camp_req = self.request(
            HttpMethod::Get,
            &format!(
                "/{}/campaigns?fields=name,status,objective&limit={want}",
                self.account_node()
            ),
        );
        let camp_resp = self.transport.send(camp_req).await?;
        map_meta_status(camp_resp.status, &camp_resp.body, "reading your Meta campaigns")?;
        let campaigns: DataList<MetaCampaign> =
            serde_json::from_str(&camp_resp.body).map_err(|_| {
                anyhow::anyhow!("reading your Meta campaigns returned an unexpected response")
            })?;

        // Leg 2: the insights (spend/impressions/clicks per campaign).
        let ins_req = self.request(
            HttpMethod::Get,
            &format!(
                "/{}/insights?fields=campaign_name,spend,impressions,clicks&limit={want}",
                self.account_node()
            ),
        );
        let ins_resp = self.transport.send(ins_req).await?;
        map_meta_status(ins_resp.status, &ins_resp.body, "reading your Meta ad spend")?;
        let insights: DataList<MetaInsight> = serde_json::from_str(&ins_resp.body)
            .map_err(|_| anyhow::anyhow!("reading your Meta ad spend returned an unexpected response"))?;

        info!(
            campaigns = campaigns.data.len(),
            insight_rows = insights.data.len(),
            "meta: built spend report"
        );
        Ok(build_spend_report(&campaigns.data, &insights.data))
    }

    // -- CONSEQUENTIAL (gated by ActionMode — touches MONEY) -----------------

    /// PAUSE a campaign (`POST /{campaign_id}` with `status=PAUSED`). Consequential
    /// — it stops a live campaign from spending. In [`ActionMode::DryRun`] it
    /// issues NO request and returns a PREVIEW of the exact change; in
    /// [`ActionMode::Execute`] it issues exactly one POST. Callers get `mode` from
    /// the foundation's `gate(confirm)`, so the shipped default (gate OFF) always
    /// previews.
    pub async fn pause_campaign(
        &self,
        campaign_id: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        self.set_status(campaign_id, "PAUSED", "pause", mode).await
    }

    /// RESUME a campaign (`POST /{campaign_id}` with `status=ACTIVE`). Consequential
    /// — it lets a paused campaign spend again. DryRun previews and issues no
    /// request; Execute issues exactly one POST.
    pub async fn resume_campaign(
        &self,
        campaign_id: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        self.set_status(campaign_id, "ACTIVE", "resume", mode).await
    }

    /// Shared status-mutation core for [`Self::pause_campaign`] /
    /// [`Self::resume_campaign`]. Validates the id, then either previews (DryRun)
    /// or issues exactly one `POST /{campaign_id}` carrying `{"status": …}`.
    async fn set_status(
        &self,
        campaign_id: &str,
        status: &str,
        verb: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        if campaign_id.trim().is_empty() {
            return Err(anyhow::anyhow!("a campaign id is required to {verb} a campaign"));
        }

        if mode == ActionMode::DryRun {
            // No request is built or sent — pure preview of the EXACT change.
            info!(dry_run = true, verb, "meta: campaign status preview (no request issued)");
            return Ok(format!(
                "[dry run] Would {verb} Meta campaign {campaign_id} (set status={status}). \
                 Enable consequential actions and confirm to apply."
            ));
        }

        let req = self
            .request(HttpMethod::Post, &format!("/{campaign_id}"))
            .json_body(serde_json::json!({ "status": status }));
        let resp = self.transport.send(req).await?;
        map_meta_status(resp.status, &resp.body, &format!("the request to {verb} the campaign"))?;
        info!(verb, "meta: campaign status updated");
        Ok(format!("Meta campaign {campaign_id} is now {status}."))
    }

    /// Change a campaign's DAILY BUDGET (`POST /{campaign_id}` with `daily_budget`,
    /// in MINOR currency units — cents). Consequential — it changes how much money
    /// the campaign can spend per day. In [`ActionMode::DryRun`] it issues NO
    /// request and returns a PREVIEW of the exact new budget; in
    /// [`ActionMode::Execute`] it issues exactly one POST. A zero/empty budget is
    /// rejected up front (before any request).
    pub async fn set_campaign_budget(
        &self,
        campaign_id: &str,
        daily_budget_minor_units: u64,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        if campaign_id.trim().is_empty() {
            return Err(anyhow::anyhow!("a campaign id is required to set a budget"));
        }
        if daily_budget_minor_units == 0 {
            return Err(anyhow::anyhow!(
                "a daily budget of 0 isn't allowed — give a positive amount in minor units (cents)"
            ));
        }

        if mode == ActionMode::DryRun {
            info!(dry_run = true, "meta: campaign budget preview (no request issued)");
            return Ok(format!(
                "[dry run] Would set Meta campaign {campaign_id} daily budget to {} minor units \
                 ({}). Enable consequential actions and confirm to apply.",
                daily_budget_minor_units,
                render_minor_units(daily_budget_minor_units)
            ));
        }

        // Graph expects daily_budget as a string of minor units.
        let req = self
            .request(HttpMethod::Post, &format!("/{campaign_id}"))
            .json_body(serde_json::json!({
                "daily_budget": daily_budget_minor_units.to_string()
            }));
        let resp = self.transport.send(req).await?;
        map_meta_status(resp.status, &resp.body, "the request to set the campaign budget")?;
        info!("meta: campaign budget updated");
        Ok(format!(
            "Meta campaign {campaign_id} daily budget is now {} ({} minor units).",
            render_minor_units(daily_budget_minor_units),
            daily_budget_minor_units
        ))
    }
}

impl MetaAdsClient<super::ReqwestTransport> {
    /// Production constructor: connect a [`MetaAuth`] handle (which resolves the
    /// app credentials + the stored long-lived token from the Keychain), resolve
    /// the full [`MetaAdsCall`] (token + ad account id) from it, and pair that with
    /// the real reqwest transport. Surfaces the friendly secret-free errors when
    /// Meta is not connected ("isn't connected"), the token is absent/expired
    /// ("token expired — reconnect"), or the ad account id is missing ("isn't fully
    /// configured").
    pub async fn connect() -> IntegrationResult<Self> {
        let auth = MetaAuth::<super::ReqwestTransport>::connect().await?;
        let call = meta_ads_call(&auth).await?;
        Ok(Self::with_call(super::ReqwestTransport::new(), call))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// Render `count` campaigns into a concise, spoken-friendly roster: a count plus
/// the first few names with their status and (when set) daily budget. Pure.
fn summarize_campaigns(campaigns: &[MetaCampaign]) -> String {
    if campaigns.is_empty() {
        return "You have no Meta campaigns in this ad account.".to_string();
    }
    let lines: Vec<String> = campaigns
        .iter()
        .take(5)
        .map(|c| {
            let name = if c.name.is_empty() { "(unnamed)" } else { &c.name };
            let status = if c.status.is_empty() { "—" } else { &c.status };
            if c.daily_budget.is_empty() {
                format!("\"{name}\" [{status}]")
            } else {
                format!(
                    "\"{name}\" [{status}, {}/day]",
                    render_minor_units_str(&c.daily_budget)
                )
            }
        })
        .collect();
    let more = campaigns.len().saturating_sub(lines.len());
    let mut out = format!(
        "{} campaign{}: {}",
        campaigns.len(),
        if campaigns.len() == 1 { "" } else { "s" },
        lines.join("; ")
    );
    if more > 0 {
        out.push_str(&format!("; and {more} more"));
    }
    out.push('.');
    out
}

/// Join the campaign roster with the insights rows into one concise spend report.
/// When there are insight rows, lead with spend per campaign; otherwise fall back
/// to the roster so a zero-spend account still gets a useful answer. Pure.
fn build_spend_report(campaigns: &[MetaCampaign], insights: &[MetaInsight]) -> String {
    if insights.is_empty() {
        // No spend in the window — still report what campaigns exist.
        if campaigns.is_empty() {
            return "No Meta campaigns and no spend in this ad account.".to_string();
        }
        return format!("No Meta ad spend in this window. {}", summarize_campaigns(campaigns));
    }
    let lines: Vec<String> = insights
        .iter()
        .take(5)
        .map(|i| {
            let name = if i.campaign_name.is_empty() {
                "(unnamed)"
            } else {
                &i.campaign_name
            };
            let spend = if i.spend.is_empty() { "0" } else { &i.spend };
            format!(
                "\"{name}\" — spend {spend}, {} impressions, {} clicks",
                if i.impressions.is_empty() { "0" } else { &i.impressions },
                if i.clicks.is_empty() { "0" } else { &i.clicks }
            )
        })
        .collect();
    let more = insights.len().saturating_sub(lines.len());
    let mut out = format!(
        "Meta spend across {} campaign{}: {}",
        insights.len(),
        if insights.len() == 1 { "" } else { "s" },
        lines.join("; ")
    );
    if more > 0 {
        out.push_str(&format!("; and {more} more"));
    }
    out.push('.');
    out
}

/// Render an amount in MINOR currency units (cents) as a major-unit string with
/// two decimals (e.g. 1500 -> "15.00"). Currency-symbol-free on purpose — the ad
/// account's currency isn't fetched here and a wrong symbol would mislead. Pure.
fn render_minor_units(minor: u64) -> String {
    format!("{}.{:02}", minor / 100, minor % 100)
}

/// Render a Graph `daily_budget` string (minor units) for display, falling back to
/// the raw string when it isn't a clean integer. Pure.
fn render_minor_units_str(minor: &str) -> String {
    match minor.parse::<u64>() {
        Ok(n) => render_minor_units(n),
        Err(_) => minor.to_string(),
    }
}

/// Map a Graph API status + body to a friendly, secret-free error. 2xx is `Ok`.
/// Meta's `OAuthException` (error code 190) is the expired/invalid-token case ->
/// "reconnect"; a permission error (code 200 / 10, or a 403) -> the
/// `ads_management`/app-review hint; everything else leans on the foundation's
/// status phrasing. The Graph error MESSAGE (which can echo input/PII) is never
/// included.
fn map_meta_status(status: u16, body: &str, what: &str) -> IntegrationResult<()> {
    if status_outcome(status) == StatusOutcome::Success {
        return Ok(());
    }
    let env = serde_json::from_str::<GraphErrorEnvelope>(body).unwrap_or_default();
    let err = &env.error;
    // Permission errors are checked FIRST: Graph uses code 200 (and sometimes 10)
    // for "permission denied", and crucially it often delivers them WITH
    // `type: OAuthException`, so the specific permission codes must win over the
    // generic OAuthException check below. A bare 403 is the same class. Point at
    // the missing capability.
    if err.code == 200 || err.code == 10 || status == 403 {
        return Err(anyhow::anyhow!(
            "{what} failed — ads_management not granted (the Meta app needs app review for this permission)"
        ));
    }
    // Expired/invalid token — code 190, or a bare OAuthException with no more
    // specific code — gets the reconnect hint.
    if err.code == 190 || err.error_type == "OAuthException" {
        return Err(anyhow::anyhow!(
            "Meta token expired/invalid — reconnect in Settings"
        ));
    }
    Err(anyhow::anyhow!("{what} {}", status_outcome(status).friendly()))
}

// ===========================================================================
// Tests — fully hermetic. Token exchanges go through MockTransport with canned
// Graph JSON; the Keychain WRITE goes through an injected recorder; the consent
// flow binds an EPHEMERAL 127.0.0.1:0 socket and replays one redirect. No real
// Meta round-trip, no fixed port, no persistent listener.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;
    use crate::integrations::BoxFuture;
    use std::sync::Arc;

    // -- deterministic randomness source -------------------------------------

    /// A `RandomSource` returning reproducible bytes, so the CSRF state is
    /// deterministic in tests.
    struct FixedRng(Vec<u8>);
    impl RandomSource for FixedRng {
        fn fill(&self, buf: &mut [u8]) {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = self.0[i % self.0.len()];
            }
        }
    }

    /// A recording Keychain store: captures whatever long-lived token would be
    /// written, so tests assert persistence WITHOUT touching the real Keychain.
    fn recording_store() -> (RefreshTokenStore, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let log2 = log.clone();
        let store: RefreshTokenStore = Arc::new(move |t: &str| {
            log2.lock().unwrap().push(t.to_string());
            Ok(())
        });
        (store, log)
    }

    // Fake credential values that, if leaked, would be unmistakable.
    const FAKE_APP_ID: &str = "FAKE-APP-ID-1234";
    const FAKE_APP_SECRET: &str = "FAKE-APP-SECRET-NEVER-LEAK";
    const FAKE_SHORT: &str = "FAKE-SHORT-TOKEN-NEVER-LEAK";
    const FAKE_LONG: &str = "FAKE-LONG-TOKEN-NEVER-LEAK";

    fn meta(
        mock: MockTransport,
        long_lived: &str,
    ) -> (MetaAuth<MockTransport>, Arc<Mutex<Vec<String>>>) {
        let (store, log) = recording_store();
        let a = MetaAuth::new(mock, FAKE_APP_ID, FAKE_APP_SECRET, long_lived, store);
        (a, log)
    }

    // Canned Graph responses. Leg 1 returns a short token, leg 2 a long token. The
    // mock matches by (method, url-substring); both legs hit the same endpoint, so
    // we register two GET entries — registration order = match priority, so the
    // FIRST registered matches leg 1 (no grant_type) and we distinguish leg 2 by
    // its `grant_type=fb_exchange_token` substring.
    fn short_token_json() -> String {
        format!(r#"{{"access_token":"{FAKE_SHORT}","token_type":"bearer","expires_in":3600}}"#)
    }
    fn long_token_json() -> String {
        format!(r#"{{"access_token":"{FAKE_LONG}","token_type":"bearer","expires_in":5184000}}"#)
    }
    fn oauth_exception_json() -> &'static str {
        r#"{"error":{"message":"expired","type":"OAuthException","code":190}}"#
    }

    /// A mock wired for BOTH exchange legs: leg 2 is keyed by the more specific
    /// `grant_type=fb_exchange_token` substring (registered first => higher
    /// priority), leg 1 by the bare endpoint.
    fn two_leg_mock() -> MockTransport {
        MockTransport::new()
            .on(HttpMethod::Get, "grant_type=fb_exchange_token", 200, long_token_json())
            .on(HttpMethod::Get, "oauth/access_token", 200, short_token_json())
    }

    // -- (0) provider constants ----------------------------------------------

    #[test]
    fn meta_constants_are_the_documented_shape() {
        assert_eq!(META_AUTH_ENDPOINT, "https://www.facebook.com/v21.0/dialog/oauth");
        assert_eq!(META_TOKEN_ENDPOINT, "https://graph.facebook.com/v21.0/oauth/access_token");
        assert_eq!(META_SCOPES, &["ads_read", "ads_management"]);
        assert_eq!(META_ACCOUNT_APP_ID, "meta_app_id");
        assert_eq!(META_ACCOUNT_APP_SECRET, "meta_app_secret");
        assert_eq!(META_ACCOUNT_LONG_LIVED_TOKEN, "meta_long_lived_token");
        assert_eq!(META_ACCOUNT_AD_ACCOUNT_ID, "meta_ad_account_id");
    }

    // -- (1) auth URL --------------------------------------------------------

    #[test]
    fn meta_auth_url_has_scopes_loopback_state_and_no_secret() {
        let url = build_auth_url(FAKE_APP_ID, 49152, "STATE123");
        assert!(url.starts_with("https://www.facebook.com/v21.0/dialog/oauth"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(&format!("client_id={FAKE_APP_ID}")));
        assert!(url.contains("state=STATE123"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49152"));
        // Scopes space-joined+encoded.
        assert!(url.contains("ads_read"));
        assert!(url.contains("ads_management"));
        assert!(url.contains("%20"), "scopes must be space-joined+encoded: {url}");
        // CRUX: the app SECRET is never part of the auth URL.
        assert!(
            !url.contains(FAKE_APP_SECRET) && !url.contains("client_secret"),
            "Meta auth URL must never carry the app secret: {url}"
        );
        // Meta's dialog uses no PKCE.
        assert!(!url.contains("code_challenge"), "Meta dialog uses no PKCE: {url}");
    }

    // -- (2) short -> long token exchange ------------------------------------

    #[tokio::test]
    async fn short_to_long_exchange_stores_long_token_and_no_secret_in_response() {
        let (a, store_log) = meta(two_leg_mock(), "");
        a.exchange_for_long_lived("AUTHCODE", 49152).await.unwrap();

        // The LONG-lived token (not the short one) is persisted via the store.
        assert_eq!(store_log.lock().unwrap().clone(), vec![FAKE_LONG.to_string()]);

        // access_token() now hands back the stored long-lived token.
        assert_eq!(a.access_token().unwrap(), FAKE_LONG);

        // Two GETs hit the Graph token endpoint: leg 1 (code) then leg 2 (exchange).
        let reqs = a.transport.requests();
        assert_eq!(reqs.len(), 2, "two exchange legs");
        assert!(reqs[0].url.contains("oauth/access_token"));
        assert!(reqs[0].url.contains("code=AUTHCODE"), "leg 1 carries the code");
        assert!(reqs[0].url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49152"));
        assert!(reqs[1].url.contains("grant_type=fb_exchange_token"), "leg 2 is the exchange");
        // The short token from leg 1 is fed into leg 2's fb_exchange_token param.
        assert!(reqs[1].url.contains(&percent_encode(FAKE_SHORT)));
    }

    #[tokio::test]
    async fn exchange_propagates_oauth_exception_as_reconnect_hint() {
        // Leg 1 fails with code 190 — surfaces the reconnect hint, no secret.
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "oauth/access_token",
            400,
            oauth_exception_json(),
        );
        let (a, store_log) = meta(mock, "");
        let err = a
            .exchange_for_long_lived("AUTHCODE", 1)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("reconnect Meta"), "got: {err}");
        assert!(!err.contains(FAKE_APP_SECRET), "error leaked the app secret");
        assert!(store_log.lock().unwrap().is_empty(), "nothing stored on failure");
    }

    /// REGRESSION (transport-FAILURE path, distinct from the HTTP-error-STATUS
    /// path above): a TRANSPORT-LEVEL failure during the token exchange — connect
    /// timeout / DNS / TLS / reset — must NOT propagate the request URL, which
    /// carries the app_secret + the code (leg 1) or the short token (leg 2) in its
    /// query. reqwest attaches the full URL to such an error and Displays it
    /// "for url (<url>)" unredacted; the propagated error string is written to the
    /// daemon log AND returned as the cloud-bound tool outcome, so a leak here puts
    /// a live credential off-device. We model the transport failure with the
    /// MockTransport's own "no canned response" error, which (faithfully to
    /// reqwest) embeds the full request URL in the error string — if get_token /
    /// the shared transport did not scrub it, the secret would ride out.
    #[tokio::test]
    async fn transport_failure_during_exchange_never_leaks_secret_in_error() {
        // -- Leg 1 (code -> short): empty mock => the FIRST send errors with the
        //    leg-1 URL, which carries client_secret=<APP_SECRET> and code=AUTHCODE.
        let (a, store_log) = meta(MockTransport::new(), "");
        let err1 = a
            .exchange_for_long_lived("AUTHCODE", 49152)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !err1.contains(FAKE_APP_SECRET),
            "leg-1 transport error leaked the app secret: {err1}"
        );
        assert!(
            !err1.contains("AUTHCODE") && !err1.contains("client_secret"),
            "leg-1 transport error leaked the auth code / secret param: {err1}"
        );
        // The whole secret-bearing token endpoint URL must be gone, not just the
        // literal secret value.
        assert!(
            !err1.contains("oauth/access_token"),
            "leg-1 transport error leaked the secret-bearing token URL: {err1}"
        );
        assert!(store_log.lock().unwrap().is_empty(), "nothing stored on a failed leg 1");

        // -- Leg 2 (short -> long): leg 1 succeeds (canned), leg 2 has NO canned
        //    match, so its send errors with the leg-2 URL carrying the short token.
        //    Key the canned response on `code=AUTHCODE` — present ONLY in leg 1's
        //    URL (leg 2 carries `grant_type`/`fb_exchange_token`, no `code`) — so
        //    leg 2 goes unmatched and the transport errors with the leg-2 URL.
        let leg1_only =
            MockTransport::new().on(HttpMethod::Get, "code=AUTHCODE", 200, short_token_json());
        let (b, store_log2) = meta(leg1_only, "");
        let err2 = b
            .exchange_for_long_lived("AUTHCODE", 49152)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            !err2.contains(FAKE_APP_SECRET) && !err2.contains(FAKE_SHORT),
            "leg-2 transport error leaked the app secret or short token: {err2}"
        );
        assert!(
            !err2.contains("fb_exchange_token") && !err2.contains("oauth/access_token"),
            "leg-2 transport error leaked the secret-bearing exchange URL: {err2}"
        );
        assert!(store_log2.lock().unwrap().is_empty(), "nothing stored on a failed leg 2");
    }

    // -- (3) access_token: stored vs expired ---------------------------------

    #[test]
    fn access_token_returns_stored_long_lived_token() {
        let (a, _log) = meta(MockTransport::new(), FAKE_LONG);
        assert_eq!(a.access_token().unwrap(), FAKE_LONG);
    }

    #[test]
    fn access_token_without_stored_token_is_expired_reconnect() {
        let (a, _log) = meta(MockTransport::new(), "");
        let err = a.access_token().unwrap_err().to_string();
        assert!(err.contains("Meta token expired"), "got: {err}");
        assert!(err.contains("reconnect"), "got: {err}");
    }

    // -- (4) meta_ads_call ----------------------------------------------------
    //
    // meta_ads_call resolves the ad account id from the Keychain, which is not
    // available in the test sandbox, so we exercise the token-absent branch (which
    // short-circuits BEFORE any Keychain read) here; the Keychain-backed branch is
    // covered by the production wiring + the mod.rs allowlist test.

    #[tokio::test]
    async fn meta_ads_call_surfaces_expired_when_no_token() {
        let (a, _log) = meta(MockTransport::new(), "");
        let err = meta_ads_call(&a).await.unwrap_err().to_string();
        assert!(err.contains("Meta token expired"), "got: {err}");
    }

    // -- (5) friendly errors --------------------------------------------------

    #[test]
    fn friendly_errors_name_provider_and_steps() {
        assert!(meta_not_connected_error().to_string().contains("Meta Ads isn't connected"));
        assert!(meta_not_connected_error().to_string().contains("connect Meta"));
        assert!(meta_expired_error().to_string().contains("Meta token expired"));
        assert!(meta_not_configured_error()
            .to_string()
            .contains("ad account id"));
    }

    // -- secrets never leak ---------------------------------------------------

    #[tokio::test]
    async fn no_secret_leaks_via_debug_or_request_urls() {
        let (a, _log) = meta(two_leg_mock(), FAKE_LONG);
        let dbg = format!("{a:?}");
        for secret in [FAKE_APP_SECRET, FAKE_LONG] {
            assert!(!dbg.contains(secret), "Debug leaked a secret: {dbg}");
        }
        assert!(dbg.contains("app_secret_present"));
        assert!(dbg.contains("long_lived_token_present"));

        // The MetaAdsCall Debug redacts the token too.
        let call = MetaAdsCall {
            access_token: FAKE_LONG.to_string(),
            ad_account_id: "act_123".to_string(),
        };
        let cdbg = format!("{call:?}");
        assert!(!cdbg.contains(FAKE_LONG), "MetaAdsCall Debug leaked the token");
        assert!(cdbg.contains("access_token_present"));
        assert!(cdbg.contains("act_123"), "ad account id is not secret");
    }

    // -- (6) run_meta_consent_flow: ephemeral loopback + injected opener ------
    //
    // Exercises the production entry point end to end WITHOUT a browser or network:
    // run_meta_consent_flow binds the loopback itself (127.0.0.1:0), so the injected
    // opener plays "browser + Meta" — it parses the port + CSRF state out of the
    // consent URL, connects to the loopback, and replays a redirect carrying that
    // exact state + a canned code. Both token legs ride MockTransport.

    fn url_param(url: &str, key: &str) -> Option<String> {
        let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
        query
            .split('&')
            .filter_map(|p| p.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| {
                // The redirect_uri is percent-encoded in the URL; decode just the
                // colon/slash escapes the test needs (the port is plain digits).
                v.replace("%3A", ":").replace("%2F", "/")
            })
    }

    fn browser_opener<F>(make_redirect: F) -> UrlOpener<'static>
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        Box::new(move |url: &str| {
            let redirect = url_param(url, "redirect_uri").unwrap_or_default();
            let port: u16 = redirect
                .rsplit(':')
                .next()
                .and_then(|p| p.parse().ok())
                .expect("consent URL must carry a loopback port");
            let state = url_param(url, "state").expect("consent URL must carry a state");
            let line = make_redirect(&state);
            Box::pin(async move {
                use tokio::io::AsyncWriteExt;
                let addr = format!("127.0.0.1:{port}");
                let mut sock = tokio::net::TcpStream::connect(&addr)
                    .await
                    .map_err(|e| anyhow::anyhow!("test opener could not reach loopback: {e}"))?;
                sock.write_all(line.as_bytes())
                    .await
                    .map_err(|e| anyhow::anyhow!("test opener write failed: {e}"))?;
                let _ = sock.flush().await;
                Ok(())
            }) as BoxFuture<'static, IntegrationResult<()>>
        })
    }

    #[tokio::test]
    async fn run_consent_flow_happy_path_stores_long_token_and_returns_connected() {
        let (a, store_log) = meta(two_leg_mock(), "");
        let rng = FixedRng(vec![0x11, 0x22, 0x33, 0x44, 0x55]);
        let opener = browser_opener(|state| {
            format!("GET /?code=LIVECODE&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        });

        let outcome = run_meta_consent_flow_with_rng(&a, opener, &rng).await.unwrap();
        assert_eq!(outcome, ConsentOutcome::Connected);
        assert_eq!(store_log.lock().unwrap().clone(), vec![FAKE_LONG.to_string()]);

        let reqs = a.transport.requests();
        assert_eq!(reqs.len(), 2, "two exchange legs after one consent");
        assert!(reqs[0].url.contains("code=LIVECODE"));
        // The exchange reuses the OS-picked loopback port (not :0).
        assert!(
            reqs[0].url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A")
                && !reqs[0].url.contains("127.0.0.1%3A0&"),
            "exchange must reuse the OS-picked loopback port: {}",
            reqs[0].url
        );
    }

    #[tokio::test]
    async fn run_consent_flow_declined_stores_nothing() {
        let (a, store_log) = meta(MockTransport::new(), "");
        let rng = FixedRng(vec![0x01, 0x02, 0x03]);
        let opener = browser_opener(|state| {
            format!("GET /?error=access_denied&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        });
        let outcome = run_meta_consent_flow_with_rng(&a, opener, &rng).await.unwrap();
        assert_eq!(outcome, ConsentOutcome::Declined("access_denied".to_string()));
        assert!(store_log.lock().unwrap().is_empty());
        assert_eq!(a.transport.requests().len(), 0, "no exchange on a declined consent");
    }

    #[tokio::test]
    async fn run_consent_flow_csrf_mismatch_rejected_before_any_exchange() {
        let (a, store_log) = meta(MockTransport::new(), "");
        let rng = FixedRng(vec![0x09, 0x08, 0x07]);
        let opener = browser_opener(|_state| {
            "GET /?code=EVIL&state=WRONGSTATE HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_string()
        });
        let err = run_meta_consent_flow_with_rng(&a, opener, &rng)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("CSRF"), "got: {err}");
        assert!(store_log.lock().unwrap().is_empty());
        assert_eq!(a.transport.requests().len(), 0, "no exchange on a CSRF mismatch");
    }

    // =======================================================================
    // (7) The Meta Ads CLIENT — reads, gated mutations, error mapping, secrets
    //
    // Fully hermetic: every case drives the foundation's MockTransport with
    // hand-written canned Graph JSON (realistic API SHAPE, never fetched). The
    // client holds a hand-built MetaAdsCall, so no Keychain, no network, no real
    // Meta round-trip.
    // =======================================================================

    /// The access token the client puts in its Authorization header. Tests assert
    /// it NEVER lands in any produced output (Debug / outcome / error) and never on
    /// a request URL.
    const FAKE_ACCESS: &str = "ACCESS-FAKE-META-NEVER-LEAK-IN-OUTPUT";
    const FAKE_ACCOUNT: &str = "act_9876543210";

    /// A Meta Ads client over `api_mock` carrying a hand-built call (fake token +
    /// ad account id).
    fn ads_client(api_mock: MockTransport) -> MetaAdsClient<MockTransport> {
        MetaAdsClient::with_call(
            api_mock,
            MetaAdsCall {
                access_token: FAKE_ACCESS.to_string(),
                ad_account_id: FAKE_ACCOUNT.to_string(),
            },
        )
    }

    // -- realistic canned Graph payloads (hand-written from the API shape) ----

    fn campaigns_json() -> &'static str {
        r#"{"data":[
            {"id":"23851","name":"Summer Sale","status":"ACTIVE","objective":"OUTCOME_SALES","daily_budget":"1500"},
            {"id":"23852","name":"Retargeting","status":"PAUSED"}
        ]}"#
    }

    fn insights_json() -> &'static str {
        r#"{"data":[
            {"campaign_name":"Summer Sale","spend":"42.17","impressions":"10240","clicks":"318"},
            {"campaign_name":"Retargeting","spend":"5.00","impressions":"900","clicks":"22"}
        ]}"#
    }

    fn empty_data_json() -> &'static str {
        r#"{"data":[]}"#
    }

    fn mutate_ok_json() -> &'static str {
        r#"{"success":true}"#
    }

    fn oauth_exception_400() -> &'static str {
        r#"{"error":{"message":"Session expired","type":"OAuthException","code":190}}"#
    }

    fn permission_error_json() -> &'static str {
        r#"{"error":{"message":"requires ads_management","type":"OAuthException","code":200}}"#
    }

    // -- READ: list_campaigns parsing ---------------------------------------

    #[tokio::test]
    async fn list_campaigns_parses_names_status_and_budget() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/campaigns", 200, campaigns_json());
        let out = ads_client(mock).list_campaigns(10).await.unwrap();
        assert!(out.contains("2 campaigns"), "got: {out}");
        assert!(out.contains("Summer Sale"), "got: {out}");
        assert!(out.contains("ACTIVE"), "got: {out}");
        // Daily budget rendered from minor units (1500 cents -> 15.00).
        assert!(out.contains("15.00"), "renders the daily budget: {out}");
        // The budget-less campaign shows status without a budget.
        assert!(out.contains("Retargeting"), "got: {out}");
        assert!(out.contains("PAUSED"), "got: {out}");
    }

    #[tokio::test]
    async fn list_campaigns_addresses_the_act_node_with_fields_and_limit() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/campaigns", 200, campaigns_json());
        let c = ads_client(mock);
        c.list_campaigns(7).await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        // The path hangs off the act_-prefixed ad account node.
        assert!(req.url.contains("/act_9876543210/campaigns"), "got url: {}", req.url);
        assert!(req.url.contains("fields=name,status,daily_budget"), "got url: {}", req.url);
        assert!(req.url.contains("limit=7"), "got url: {}", req.url);
        // Auth attached (value never asserted), and the token is NOT on the URL.
        assert!(req.has_header("authorization"), "auth header attached");
        assert!(!req.url.contains(FAKE_ACCESS), "token must never ride on the URL");
    }

    #[tokio::test]
    async fn list_campaigns_clamps_max_into_band() {
        for (asked, want) in [(0u32, "limit=1"), (1000u32, "limit=100")] {
            let mock =
                MockTransport::new().on(HttpMethod::Get, "/campaigns", 200, campaigns_json());
            let c = ads_client(mock);
            c.list_campaigns(asked).await.unwrap();
            assert!(c.transport.last_request().url.contains(want), "asked {asked}");
        }
    }

    #[tokio::test]
    async fn list_campaigns_empty_is_friendly() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/campaigns", 200, empty_data_json());
        let out = ads_client(mock).list_campaigns(10).await.unwrap();
        assert!(out.contains("no Meta campaigns"), "got: {out}");
    }

    /// Bare-id ad accounts (no `act_` prefix in Settings) still address the act_
    /// node correctly.
    #[tokio::test]
    async fn account_node_adds_act_prefix_when_missing() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/campaigns", 200, campaigns_json());
        let c = MetaAdsClient::with_call(
            mock,
            MetaAdsCall {
                access_token: FAKE_ACCESS.to_string(),
                ad_account_id: "9876543210".to_string(), // no act_ prefix
            },
        );
        c.list_campaigns(10).await.unwrap();
        assert!(
            c.transport.last_request().url.contains("/act_9876543210/campaigns"),
            "got url: {}",
            c.transport.last_request().url
        );
    }

    // -- READ: report_campaigns joins roster + insights ----------------------

    #[tokio::test]
    async fn report_campaigns_joins_roster_and_insights() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/campaigns", 200, campaigns_json())
            .on(HttpMethod::Get, "/insights", 200, insights_json());
        let out = ads_client(mock).report_campaigns(10).await.unwrap();
        assert!(out.contains("Meta spend across 2 campaigns"), "got: {out}");
        assert!(out.contains("Summer Sale"), "got: {out}");
        assert!(out.contains("spend 42.17"), "echoes spend: {out}");
        assert!(out.contains("10240 impressions"), "echoes impressions: {out}");
        assert!(out.contains("318 clicks"), "echoes clicks: {out}");
    }

    #[tokio::test]
    async fn report_campaigns_issues_two_reads_to_the_act_node() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/campaigns", 200, campaigns_json())
            .on(HttpMethod::Get, "/insights", 200, insights_json());
        let c = ads_client(mock);
        c.report_campaigns(10).await.unwrap();
        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 2, "campaigns + insights reads");
        assert!(reqs[0].url.contains("/act_9876543210/campaigns"));
        assert!(reqs[1].url.contains("/act_9876543210/insights"));
        assert!(reqs[1].url.contains("fields=campaign_name,spend,impressions,clicks"));
        for r in &reqs {
            assert!(r.has_header("authorization"), "auth header on every read");
            assert!(!r.url.contains(FAKE_ACCESS), "token never on a read URL");
        }
    }

    /// No insight rows (no spend in the window) falls back to the roster so the
    /// caller still gets a useful answer.
    #[tokio::test]
    async fn report_campaigns_falls_back_to_roster_when_no_spend() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/campaigns", 200, campaigns_json())
            .on(HttpMethod::Get, "/insights", 200, empty_data_json());
        let out = ads_client(mock).report_campaigns(10).await.unwrap();
        assert!(out.contains("No Meta ad spend"), "got: {out}");
        assert!(out.contains("Summer Sale"), "still names the campaigns: {out}");
    }

    // -- CONSEQUENTIAL: DryRun issues NO request, previews the EXACT change ---

    #[tokio::test]
    async fn pause_dry_run_issues_no_request_and_previews_exact_change() {
        let mock = MockTransport::new(); // no canned responses on purpose
        let c = ads_client(mock);
        let out = c.pause_campaign("23851", ActionMode::DryRun).await.unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("23851"), "names the campaign: {out}");
        assert!(out.contains("status=PAUSED"), "previews the exact change: {out}");
        assert_eq!(c.transport.requests().len(), 0, "DryRun must touch nothing");
    }

    #[tokio::test]
    async fn resume_dry_run_previews_active_and_issues_no_request() {
        let c = ads_client(MockTransport::new());
        let out = c.resume_campaign("23852", ActionMode::DryRun).await.unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("status=ACTIVE"), "previews the exact change: {out}");
        assert_eq!(c.transport.requests().len(), 0);
    }

    #[tokio::test]
    async fn set_budget_dry_run_previews_amount_and_issues_no_request() {
        let c = ads_client(MockTransport::new());
        let out = c
            .set_campaign_budget("23851", 2500, ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("2500 minor units"), "names the raw amount: {out}");
        assert!(out.contains("25.00"), "renders the major-unit amount: {out}");
        assert_eq!(c.transport.requests().len(), 0, "DryRun must touch nothing");
    }

    // -- CONSEQUENTIAL: Execute issues EXACTLY one POST with the right body ---

    #[tokio::test]
    async fn pause_execute_posts_exactly_one_status_paused() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/23851", 200, mutate_ok_json());
        let c = ads_client(mock);
        let out = c.pause_campaign("23851", ActionMode::Execute).await.unwrap();
        assert!(out.contains("now PAUSED"), "got: {out}");

        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 1, "Execute issues exactly one POST");
        let req = &reqs[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.ends_with("/23851"), "posts to the campaign node: {}", req.url);
        assert_eq!(req.body.as_ref().unwrap()["status"], "PAUSED");
        assert!(req.has_header("authorization"));
        assert!(!req.url.contains(FAKE_ACCESS), "token never on the mutate URL");
    }

    #[tokio::test]
    async fn resume_execute_posts_exactly_one_status_active() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/23852", 200, mutate_ok_json());
        let c = ads_client(mock);
        c.resume_campaign("23852", ActionMode::Execute).await.unwrap();
        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].body.as_ref().unwrap()["status"], "ACTIVE");
    }

    #[tokio::test]
    async fn set_budget_execute_posts_exactly_one_daily_budget_as_minor_units_string() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/23851", 200, mutate_ok_json());
        let c = ads_client(mock);
        let out = c
            .set_campaign_budget("23851", 5000, ActionMode::Execute)
            .await
            .unwrap();
        assert!(out.contains("50.00"), "confirms the new budget: {out}");

        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 1, "Execute issues exactly one POST");
        let req = &reqs[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.ends_with("/23851"));
        // Graph wants daily_budget as a STRING of minor units.
        assert_eq!(req.body.as_ref().unwrap()["daily_budget"], "5000");
    }

    // -- CONSEQUENTIAL: pre-request validation -------------------------------

    #[tokio::test]
    async fn empty_campaign_id_rejected_before_any_request() {
        let c = ads_client(MockTransport::new());
        let e1 = c.pause_campaign("  ", ActionMode::Execute).await.unwrap_err();
        assert!(e1.to_string().contains("campaign id is required"), "got: {e1}");
        let e2 = c
            .set_campaign_budget("", 100, ActionMode::Execute)
            .await
            .unwrap_err();
        assert!(e2.to_string().contains("campaign id is required"), "got: {e2}");
        assert_eq!(c.transport.requests().len(), 0, "no request on a bad id");
    }

    #[tokio::test]
    async fn zero_budget_rejected_before_any_request_even_in_dry_run() {
        let c = ads_client(MockTransport::new());
        let err = c
            .set_campaign_budget("23851", 0, ActionMode::DryRun)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("0 isn't allowed"), "got: {err}");
        assert_eq!(c.transport.requests().len(), 0);
    }

    // -- error mapping -------------------------------------------------------

    #[tokio::test]
    async fn oauth_exception_190_maps_to_reconnect_on_read() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/campaigns", 400, oauth_exception_400());
        let err = ads_client(mock).list_campaigns(10).await.unwrap_err();
        assert!(
            err.to_string().contains("Meta token expired/invalid — reconnect"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn permission_error_maps_to_ads_management_hint_on_mutate() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/23851", 400, permission_error_json());
        let err = ads_client(mock)
            .pause_campaign("23851", ActionMode::Execute)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ads_management not granted"), "got: {err}");
        assert!(err.to_string().contains("app review"), "got: {err}");
    }

    #[tokio::test]
    async fn forbidden_403_maps_to_ads_management_hint() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/23851", 403, "{}");
        let err = ads_client(mock)
            .resume_campaign("23851", ActionMode::Execute)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("ads_management not granted"), "got: {err}");
    }

    #[tokio::test]
    async fn rate_limited_and_server_errors_map_friendly() {
        let mock429 = MockTransport::new().on(HttpMethod::Get, "/campaigns", 429, "{}");
        let e429 = ads_client(mock429).list_campaigns(10).await.unwrap_err();
        assert!(e429.to_string().contains("rate limited"), "got: {e429}");

        let mock503 = MockTransport::new().on(HttpMethod::Get, "/campaigns", 503, "{}");
        let e503 = ads_client(mock503).list_campaigns(10).await.unwrap_err();
        assert!(e503.to_string().contains("service's side"), "got: {e503}");
    }

    // -- the TOKEN never leaks ----------------------------------------------

    /// The access token may never appear in the client's Debug, in any returned
    /// outcome string (read / dry-run / execute), or in any mapped error. Drive a
    /// representative slice and scan every produced string for the secret.
    #[tokio::test]
    async fn token_never_appears_in_any_produced_output() {
        // Debug of the client.
        let dbg = format!("{:?}", ads_client(MockTransport::new()));
        assert!(!dbg.contains(FAKE_ACCESS), "Debug leaked the token: {dbg}");
        assert!(dbg.contains("access_token_present"), "Debug notes presence");
        assert!(dbg.contains(FAKE_ACCOUNT), "ad account id is not secret");

        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/campaigns", 200, campaigns_json())
            .on(HttpMethod::Get, "/insights", 200, insights_json())
            .on(HttpMethod::Post, "/23851", 200, mutate_ok_json());
        let c = ads_client(mock);
        let ok1 = c.list_campaigns(10).await.unwrap();
        let ok2 = c.report_campaigns(10).await.unwrap();
        let ok3 = c.pause_campaign("23851", ActionMode::Execute).await.unwrap();
        let dry = c.set_campaign_budget("23851", 100, ActionMode::DryRun).await.unwrap();

        let err_mock = MockTransport::new().on(HttpMethod::Get, "/campaigns", 400, oauth_exception_400());
        let err = ads_client(err_mock).list_campaigns(10).await.unwrap_err().to_string();

        for s in [&ok1, &ok2, &ok3, &dry, &err] {
            assert!(!s.contains(FAKE_ACCESS), "output leaked the token: {s}");
        }
        // And the token never landed on any recorded request URL.
        for r in c.transport.requests() {
            assert!(!r.url.contains(FAKE_ACCESS), "token on a URL: {}", r.url);
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn render_minor_units_formats_cents() {
        assert_eq!(render_minor_units(0), "0.00");
        assert_eq!(render_minor_units(5), "0.05");
        assert_eq!(render_minor_units(1500), "15.00");
        assert_eq!(render_minor_units(123456), "1234.56");
        assert_eq!(render_minor_units_str("2500"), "25.00");
        // Non-integer strings pass through unchanged.
        assert_eq!(render_minor_units_str("n/a"), "n/a");
    }

    #[test]
    fn map_meta_status_table() {
        assert!(map_meta_status(200, "{}", "x").is_ok());
        assert!(map_meta_status(400, oauth_exception_400(), "x")
            .unwrap_err()
            .to_string()
            .contains("reconnect"));
        assert!(map_meta_status(400, permission_error_json(), "x")
            .unwrap_err()
            .to_string()
            .contains("ads_management"));
        assert!(map_meta_status(403, "{}", "x")
            .unwrap_err()
            .to_string()
            .contains("ads_management"));
        assert!(map_meta_status(429, "{}", "x")
            .unwrap_err()
            .to_string()
            .contains("rate limited"));
    }
}
