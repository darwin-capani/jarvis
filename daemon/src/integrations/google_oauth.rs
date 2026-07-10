//! Google OAuth2 core for a DESKTOP (installed) app — the security crux of
//! Chart-2 round 2. ONE Google OAuth client + ONE user consent grants Calendar,
//! Gmail and Drive together (combined scopes, a single long-lived refresh
//! token), and the three service clients (Calendar/Gmail/Drive) share one
//! [`GoogleAuth`] handle that hands them a fresh bearer token on demand.
//!
//! The flow is the standard installed-app **loopback redirect + PKCE** (RFC
//! 7636) Google documents for desktop clients:
//!
//!   1. [`begin_auth`] builds the consent URL (PKCE `code_challenge`, CSRF
//!      `state`, `access_type=offline`, `prompt=consent`) pointing the redirect
//!      at `http://127.0.0.1:<port>`. URL building is PURE and unit-tested
//!      (every param asserted; the client_secret is asserted ABSENT).
//!   2. The user approves in a real browser (device-gated — never automated
//!      here). Google redirects to the loopback with `?code=…&state=…`.
//!   3. A tiny one-shot localhost responder ([`receive_redirect`]) accepts that
//!      single request, validates `state` (mismatch = CSRF, rejected), extracts
//!      the `code`, and replies with a "you can close this tab" page. The
//!      request-line parsing + state check + code extraction are factored into a
//!      PURE function ([`parse_redirect`]) tested with crafted request strings.
//!   4. [`GoogleAuth::exchange_code`] POSTs the code + verifier to Google's token
//!      endpoint and stores the returned refresh token in the Keychain.
//!      [`GoogleAuth::refresh_access_token`] mints a fresh access token from the
//!      stored refresh token; [`GoogleAuth::bearer`] returns a cached access
//!      token, transparently refreshing ~60s before expiry.
//!
//! SECURITY POSTURE (mirrors the foundation and the round-1 clients):
//!   * The client_secret, authorization code, access token and refresh token are
//!     NEVER logged, never put in an error/Debug/tracing field, never on argv.
//!     The refresh token lives ONLY in the Keychain; the access token lives ONLY
//!     in memory. Presence/expiry are logged as bools/times at most.
//!   * Randomness (PKCE verifier + CSRF state) is injectable so tests are
//!     deterministic; production pulls OS entropy from `/dev/urandom` exactly
//!     like `apps::session_key`, adding no RNG dependency. The Keychain WRITE is
//!     also injectable so tests never touch the real Keychain.
//!   * Every token exchange / refresh / API call in tests goes through the
//!     foundation's `MockTransport` with canned Google JSON — zero network.

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::info;

use super::{
    resolve_secret, status_outcome, BoxFuture, HttpMethod, HttpRequest, HttpTransport,
    IntegrationResult, StatusOutcome,
};

// ---------------------------------------------------------------------------
// Keychain accounts
// ---------------------------------------------------------------------------

/// The user's OAuth *client id* (pasted in Settings). Not secret per se, but
/// resolved through the same allowlisted reader for uniformity.
pub const ACCOUNT_CLIENT_ID: &str = "google_oauth_client_id";
/// The user's OAuth *client secret* (pasted in Settings). "Desktop app" OAuth
/// clients still issue a secret; it is used only in the token POST body, never
/// in the auth URL, never logged.
pub const ACCOUNT_CLIENT_SECRET: &str = "google_oauth_client_secret";
/// The long-lived *refresh token* — WRITTEN by JARVIS after consent, read back
/// on every connect. Lives ONLY in the Keychain.
pub const ACCOUNT_REFRESH_TOKEN: &str = "google_oauth_refresh_token";

// ---------------------------------------------------------------------------
// Endpoints
// ---------------------------------------------------------------------------

/// Google's OAuth2 authorization (consent) endpoint.
pub const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
/// Google's OAuth2 token endpoint (code exchange + refresh).
pub const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

// ---------------------------------------------------------------------------
// Combined scope set
// ---------------------------------------------------------------------------
//
// One consent must cover read+write for Calendar, Gmail and Drive. The user's
// consent screen MUST include exactly these scopes:
//
//   Calendar: https://www.googleapis.com/auth/calendar.events
//     — read AND write calendar events (the granular events scope, not the
//       broader read/write `calendar` scope: least privilege that still lets us
//       list/read events and create/update them under the gate).
//   Gmail:    https://www.googleapis.com/auth/gmail.readonly
//             https://www.googleapis.com/auth/gmail.send
//     — readonly for reading/searching mail; send for the gated "send a draft".
//       (We deliberately avoid gmail.modify / full mail scope.)
//   Drive:    https://www.googleapis.com/auth/drive.file
//             https://www.googleapis.com/auth/drive.metadata.readonly
//     — drive.file grants access ONLY to files the app creates or the user opens
//       with it (supports the gated upload + reading our own files);
//       drive.metadata.readonly lets us LIST/SEARCH the user's Drive by metadata
//       (name/type/modified) WITHOUT being able to read arbitrary file CONTENT.
//       This is strictly less privilege than `drive.readonly` (which would grant
//       read of EVERY file's content). The trade-off: we can search/list all
//       files but can only read the content of files the app itself touched —
//       the least-privilege choice that still supports list/search/read-our-own
//       + the gated upload.

/// Calendar: read + write events (granular).
pub const SCOPE_CALENDAR_EVENTS: &str = "https://www.googleapis.com/auth/calendar.events";
/// Gmail: read/search mail.
pub const SCOPE_GMAIL_READONLY: &str = "https://www.googleapis.com/auth/gmail.readonly";
/// Gmail: send mail (gated).
pub const SCOPE_GMAIL_SEND: &str = "https://www.googleapis.com/auth/gmail.send";
/// Drive: per-file access to files the app creates/opens (supports gated upload).
pub const SCOPE_DRIVE_FILE: &str = "https://www.googleapis.com/auth/drive.file";
/// Drive: list/search by metadata only — NO arbitrary content read.
pub const SCOPE_DRIVE_METADATA_READONLY: &str =
    "https://www.googleapis.com/auth/drive.metadata.readonly";

/// The full combined scope set, in a stable order. This is exactly what the
/// user's consent screen must grant. Exposed so the begin-auth caller and the
/// service clients reference one source of truth.
pub const COMBINED_SCOPES: &[&str] = &[
    SCOPE_CALENDAR_EVENTS,
    SCOPE_GMAIL_READONLY,
    SCOPE_GMAIL_SEND,
    SCOPE_DRIVE_FILE,
    SCOPE_DRIVE_METADATA_READONLY,
];

/// Refresh this many seconds BEFORE the access token's stated expiry, so a token
/// can't expire mid-flight between the freshness check and the API call.
const EXPIRY_SKEW_SECS: i64 = 60;

// ===========================================================================
// (2) PKCE + state — pure math, injectable randomness
// ===========================================================================

/// Base64url (RFC 4648 §5) WITHOUT padding — the encoding RFC 7636 mandates for
/// both the verifier alphabet and the S256 challenge. Implemented locally (≈15
/// lines, fully unit-tested against the RFC vector) so we add no `base64`
/// dependency. Pure.
fn base64url_nopad(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 0x3f) as usize] as char);
        }
    }
    out
}

/// Compute the S256 PKCE challenge from a verifier: base64url(SHA-256(verifier)),
/// no padding (RFC 7636 §4.2). PURE — unit-tested against the RFC 7636 Appendix B
/// test vector.
pub fn code_challenge_s256(verifier: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    base64url_nopad(&hasher.finalize())
}

/// A source of cryptographic randomness, injected so tests are deterministic.
/// Production uses [`OsEntropy`] (`/dev/urandom`); tests use a fixed-bytes
/// source. The method fills `buf` with random bytes (or, in the test source,
/// canned bytes).
pub trait RandomSource {
    /// Fill `buf` with random bytes.
    fn fill(&self, buf: &mut [u8]);
}

/// Production randomness: 32-byte reads from `/dev/urandom`, the same OS-entropy
/// path `apps::session_key` uses, so we add no RNG dependency and keep the
/// entropy off any logged path. A read failure panics rather than minting a
/// predictable verifier/state (predictable PKCE/state would defeat the security
/// of the flow).
pub struct OsEntropy;

impl RandomSource for OsEntropy {
    fn fill(&self, buf: &mut [u8]) {
        use std::io::Read;
        match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(buf)) {
            Ok(()) => {}
            Err(e) => panic!("cannot read /dev/urandom for OAuth PKCE/state: {e}"),
        }
    }
}

/// Number of random bytes behind the verifier and the state. 32 bytes →
/// 43 base64url chars for the verifier (within RFC 7636's 43..=128 range) and a
/// 256-bit, unguessable CSRF state.
const PKCE_RANDOM_BYTES: usize = 32;

/// Generate a fresh, high-entropy PKCE `code_verifier`: base64url-nopad over 32
/// random bytes (43 chars, in RFC 7636's legal 43..=128 verifier range, all
/// unreserved). Randomness is injected for deterministic tests.
pub fn generate_verifier(rng: &dyn RandomSource) -> String {
    let mut bytes = [0u8; PKCE_RANDOM_BYTES];
    rng.fill(&mut bytes);
    base64url_nopad(&bytes)
}

/// Generate a fresh, high-entropy CSRF `state`: base64url-nopad over 32 random
/// bytes. Injected randomness for deterministic tests.
pub fn generate_state(rng: &dyn RandomSource) -> String {
    let mut bytes = [0u8; PKCE_RANDOM_BYTES];
    rng.fill(&mut bytes);
    base64url_nopad(&bytes)
}

// ===========================================================================
// (3) begin_auth — pure URL assembly
// ===========================================================================

/// Everything needed to correlate the browser redirect back to this auth
/// attempt: the PKCE `verifier` (to send at code exchange), the CSRF `state` (to
/// match against the redirect), and the loopback `port` the redirect must hit.
///
/// `Debug` is hand-written to REDACT the verifier (it is the PKCE secret) — only
/// the state and port print.
#[derive(Clone)]
pub struct PendingAuth {
    /// PKCE code_verifier — secret; sent only in the token-exchange body.
    pub verifier: String,
    /// CSRF state — matched against the `state` returned on the redirect.
    pub state: String,
    /// The loopback port the redirect URI points at (`http://127.0.0.1:<port>`).
    pub port: u16,
}

impl std::fmt::Debug for PendingAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the verifier (PKCE secret); state/port are safe.
        f.debug_struct("PendingAuth")
            .field("verifier_present", &!self.verifier.is_empty())
            .field("state", &self.state)
            .field("port", &self.port)
            .finish()
    }
}

/// The loopback redirect URI for a given port. Google's installed-app flow
/// accepts a bare `http://127.0.0.1:<port>` (no path) for the loopback
/// redirect.
pub fn redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}")
}

/// Percent-encode a string for use in a URL query value, per RFC 3986: keep the
/// unreserved set (ALPHA / DIGIT / `-` `_` `.` `~`) literal, percent-encode
/// everything else (notably `:` `/` and space, which appear in scopes and the
/// redirect URI). Local + pure so URL assembly stays dependency-free and the
/// exact output is unit-tested.
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

/// Build the Google consent URL for `scopes` against the given `client_id`,
/// `state`, PKCE `challenge` and loopback `port`. PURE — no I/O, no secret. The
/// client_secret is NEVER part of an auth URL (asserted in tests). Scopes are
/// space-joined then percent-encoded as one value.
pub fn build_auth_url(
    client_id: &str,
    scopes: &[&str],
    state: &str,
    challenge: &str,
    port: u16,
) -> String {
    let scope = scopes.join(" ");
    let redirect = redirect_uri(port);
    format!(
        "{AUTH_ENDPOINT}?\
         client_id={}&\
         redirect_uri={}&\
         response_type=code&\
         scope={}&\
         code_challenge={}&\
         code_challenge_method=S256&\
         state={}&\
         access_type=offline&\
         prompt=consent",
        percent_encode(client_id),
        percent_encode(&redirect),
        percent_encode(&scope),
        percent_encode(challenge),
        percent_encode(state),
    )
}

/// Begin an auth attempt against `client_id` for `scopes`, binding the redirect
/// to `port`. Generates a fresh PKCE verifier+challenge and CSRF state (from the
/// injected `rng`), returns the consent URL plus the [`PendingAuth`] the caller
/// holds to correlate the redirect. PURE except for the RNG read — no network,
/// no I/O. The verifier itself never appears in the URL (only its S256
/// challenge does).
pub fn begin_auth(
    client_id: &str,
    scopes: &[&str],
    port: u16,
    rng: &dyn RandomSource,
) -> (String, PendingAuth) {
    let verifier = generate_verifier(rng);
    let challenge = code_challenge_s256(&verifier);
    let state = generate_state(rng);
    let url = build_auth_url(client_id, scopes, &state, &challenge, port);
    let pending = PendingAuth {
        verifier,
        state,
        port,
    };
    info!(port, scopes = scopes.len(), "google_oauth: built consent URL");
    (url, pending)
}

// ===========================================================================
// (4) Loopback redirect handler — pure parsing + one-shot responder
// ===========================================================================

/// Outcome of parsing the browser's redirect request. Either we got the code
/// (state validated) or Google sent an `?error=…`, or the request was malformed
/// / state-mismatched (a probable CSRF attempt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectOutcome {
    /// Consent succeeded: the authorization `code`, with a validated `state`.
    Code(String),
    /// Google returned `?error=<code>` (e.g. `access_denied`).
    Denied(String),
}

/// Parse the first line of an HTTP request (e.g.
/// `GET /?code=abc&state=xyz HTTP/1.1`), validate `state` against
/// `expected_state`, and extract the `code` (or the `error`). PURE — the crux of
/// the loopback handler, tested with crafted request strings.
///
/// Returns `Err` (secret-free) for: a non-GET / malformed request line, a
/// MISSING or MISMATCHED state (treated as a possible CSRF — rejected BEFORE the
/// code is trusted), or a response carrying neither `code` nor `error`. On a
/// valid `?error=` it returns `Ok(Denied(..))`; on a valid `?code=` with a
/// matching state it returns `Ok(Code(..))`.
pub fn parse_redirect(request_line: &str, expected_state: &str) -> IntegrationResult<RedirectOutcome> {
    // Request line: METHOD SP target SP HTTP/x.y
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if method != "GET" {
        return Err(anyhow::anyhow!("unexpected redirect method"));
    }
    // Target is `/` or `/?<query>` (occasionally an absolute-form URI); take
    // whatever follows the first '?' as the query.
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);

    let got_state = params.iter().find(|(k, _)| k == "state").map(|(_, v)| v.as_str());
    let code = params.iter().find(|(k, _)| k == "code").map(|(_, v)| v.as_str());
    let error = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v.as_str());

    // CSRF: the state MUST be present and equal. Constant-time-ish compare is
    // unnecessary (state is single-use and high-entropy), but we reject any
    // mismatch BEFORE trusting a code. A missing state is also a mismatch.
    match got_state {
        Some(s) if s == expected_state => {}
        _ => return Err(anyhow::anyhow!("redirect state did not match — possible CSRF")),
    }

    if let Some(err) = error {
        // `error` is a fixed OAuth identifier (e.g. access_denied), not secret.
        return Ok(RedirectOutcome::Denied(err.to_string()));
    }
    match code {
        Some(c) if !c.is_empty() => Ok(RedirectOutcome::Code(c.to_string())),
        _ => Err(anyhow::anyhow!("redirect carried neither code nor error")),
    }
}

/// Parse a URL query string into (key, value) pairs, percent-DECODING each. Pure
/// + local so the handler needs no `url` dependency. Tolerates empty values and
/// missing '='.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Percent-decode a query component (`%XX` → byte, `+` → space). Lossy on
/// invalid UTF-8, which is fine for the codes/states we handle (all ASCII).
/// Pure.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The tiny HTML page sent back to the browser after the redirect, so the user
/// sees confirmation instead of a raw response. No secrets, no dynamic content.
const CLOSE_TAB_PAGE: &str =
    "<!doctype html><html><body style=\"font-family:system-ui;text-align:center;padding:3rem\">\
     <h2>JARVIS is connected to Google.</h2><p>You can close this tab.</p></body></html>";

/// Build the full HTTP/1.1 response (status line + headers + body) for the
/// close-tab page. Pure, so the wire shape is testable.
fn close_tab_response() -> String {
    format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        CLOSE_TAB_PAGE.len(),
        CLOSE_TAB_PAGE
    )
}

/// How long the one-shot loopback responder waits for the browser redirect
/// before giving up — the user has to approve in the browser, so this is
/// generous.
const REDIRECT_TIMEOUT: Duration = Duration::from_secs(300);

/// Accept exactly ONE request on `listener` (the loopback the redirect URI
/// points at), parse + validate it against `expected_state`, reply with the
/// close-tab page, and return the [`RedirectOutcome`]. The listener is consumed
/// (one-shot): no persistent server, no second accept.
///
/// SECURITY/SCOPE: production code binds the listener on `127.0.0.1:<port>`
/// (device-gated — a real localhost bind during a real consent flow). This
/// function does the accept/parse/respond given an already-bound listener, so a
/// test can hand it an ephemeral `127.0.0.1:0` listener, send itself one
/// request, and assert the code comes back — without a fixed port and without a
/// long-lived server.
pub async fn receive_redirect(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> IntegrationResult<RedirectOutcome> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let accept = tokio::time::timeout(REDIRECT_TIMEOUT, listener.accept()).await;
    let (mut stream, _peer) = match accept {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => return Err(anyhow::anyhow!("loopback accept failed: {e}")),
        Err(_) => return Err(anyhow::anyhow!("timed out waiting for the Google redirect")),
    };

    // Read just enough to get the request line. The redirect is a small GET; we
    // read one bounded chunk and parse the first line. We never read or log a
    // body.
    let mut buf = [0u8; 2048];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| anyhow::anyhow!("reading loopback request failed: {e}"))?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let request_line = head.lines().next().unwrap_or("");

    let outcome = parse_redirect(request_line, expected_state);

    // Always reply with the close-tab page (even on a rejected/denied parse, so
    // the browser shows something rather than hanging), then close.
    let _ = stream.write_all(close_tab_response().as_bytes()).await;
    let _ = stream.flush().await;
    let _ = stream.shutdown().await;

    outcome
}

// ===========================================================================
// (5)/(6) Token exchange + refresh + the shared GoogleAuth handle
// ===========================================================================

/// Google's token-endpoint success JSON. We decode only what we need; the
/// refresh token is OPTIONAL on a refresh response (Google omits it there).
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// Lifetime in seconds (Google sends ~3599).
    #[serde(default)]
    expires_in: i64,
}

/// Google's token-endpoint error JSON (`{"error":"invalid_grant", ...}`). The
/// `error` field is a fixed OAuth identifier, not secret.
#[derive(Debug, Deserialize)]
struct TokenError {
    #[serde(default)]
    error: String,
}

/// A function that PERSISTS the refresh token to the Keychain. Injected so tests
/// substitute an in-memory recorder and never touch the real Keychain. The
/// production impl ([`keychain_store`]) shells out to `security add-generic-password`.
pub type RefreshTokenStore = Box<dyn Fn(&str) -> IntegrationResult<()> + Send + Sync>;

/// The in-memory access token + its expiry. Held behind a Mutex inside
/// [`GoogleAuth`]; never logged (only presence/expiry as bool/time).
#[derive(Default)]
struct CachedToken {
    access_token: String,
    /// Unix epoch seconds at which the token expires; 0 = none cached yet.
    expires_at: i64,
}

/// The shared Google auth handle the three service clients hold. Owns the
/// client_id/secret and the refresh token (resolved from the Keychain), caches
/// the in-memory access token, and hands out a fresh bearer on demand via
/// [`Self::bearer`].
///
/// Generic over the foundation's [`HttpTransport`] so production wires
/// `ReqwestTransport` and tests wire `MockTransport`. `Debug` is hand-written to
/// redact every secret.
pub struct GoogleAuth<T: HttpTransport> {
    /// The injected HTTP seam for the token endpoint. `pub(crate)` so sibling
    /// service-client test modules (Gmail/Calendar/Drive) can introspect the
    /// recorded token-endpoint requests via the test `MockTransport` — e.g. to
    /// assert a DryRun never even minted an access token. Read-only seam: no
    /// secret is reachable through it (tokens ride in request headers/bodies the
    /// tests check by presence, not value).
    pub(crate) transport: T,
    client_id: String,
    client_secret: String,
    /// The long-lived grant. Empty BEFORE the first consent (exchange writes it);
    /// resolved from the Keychain by `connect`.
    refresh_token: Mutex<String>,
    /// Cached access token (memory only).
    cached: Mutex<CachedToken>,
    /// Injected Keychain writer for the refresh token.
    store: RefreshTokenStore,
}

impl<T: HttpTransport> std::fmt::Debug for GoogleAuth<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret, the refresh token, or the access token — only
        // booleans for presence.
        let has_refresh = self
            .refresh_token
            .lock()
            .map(|t| !t.is_empty())
            .unwrap_or(false);
        let has_access = self
            .cached
            .lock()
            .map(|c| !c.access_token.is_empty())
            .unwrap_or(false);
        f.debug_struct("GoogleAuth")
            .field("client_id_present", &!self.client_id.is_empty())
            .field("client_secret_present", &!self.client_secret.is_empty())
            .field("refresh_token_present", &has_refresh)
            .field("access_token_present", &has_access)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> GoogleAuth<T> {
    /// Build a handle from explicit credentials + transport + Keychain store.
    /// Used by tests (mock transport, fake creds, recording store) and by
    /// [`GoogleAuth::connect`] internally. `refresh_token` may be empty for the
    /// pre-consent exchange flow.
    pub fn new(
        transport: T,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        refresh_token: impl Into<String>,
        store: RefreshTokenStore,
    ) -> Self {
        Self {
            transport,
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            refresh_token: Mutex::new(refresh_token.into()),
            cached: Mutex::new(CachedToken::default()),
            store,
        }
    }

    /// Exchange an authorization `code` (+ its PKCE `verifier`) for tokens at
    /// Google's token endpoint, store the returned refresh token via the injected
    /// store, and cache the access token. `port` is the loopback port used at
    /// consent — Google requires the exchange's `redirect_uri` to byte-match the
    /// auth request's, so the caller passes the same port it bound the loopback
    /// on (carried in [`PendingAuth::port`]). The client_secret, code, verifier
    /// and both tokens are sent only in the POST body and NEVER logged.
    ///
    /// On `invalid_grant` (expired/replayed code) or any non-2xx, returns a
    /// friendly, secret-free error.
    pub async fn exchange_code(
        &self,
        code: &str,
        verifier: &str,
        port: u16,
    ) -> IntegrationResult<()> {
        let params: Vec<(String, String)> = vec![
            ("grant_type".to_string(), "authorization_code".to_string()),
            ("code".to_string(), code.to_string()),
            ("code_verifier".to_string(), verifier.to_string()),
            ("client_id".to_string(), self.client_id.clone()),
            ("client_secret".to_string(), self.client_secret.clone()),
            ("redirect_uri".to_string(), redirect_uri(port)),
        ];
        let tokens = self.post_token(params, "exchanging the authorization code").await?;
        let refresh = tokens.refresh_token.unwrap_or_default();
        if refresh.is_empty() {
            return Err(anyhow::anyhow!(
                "Google did not return a refresh token — reconnect and grant offline access"
            ));
        }
        (self.store)(&refresh)?;
        if let Ok(mut rt) = self.refresh_token.lock() {
            *rt = refresh;
        }
        self.cache_access(&tokens.access_token, tokens.expires_in);
        info!("google_oauth: code exchanged; refresh token stored");
        Ok(())
    }

    /// Mint a fresh access token from the stored refresh token + client_id/secret
    /// and cache it. Returns the new access token's value to the caller of
    /// [`Self::bearer`]. Errors (secret-free) if there is no refresh token, or on
    /// `invalid_grant` (the user revoked access — they must reconnect).
    pub async fn refresh_access_token(&self) -> IntegrationResult<String> {
        let refresh = self
            .refresh_token
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default();
        if refresh.is_empty() {
            return Err(not_connected_error());
        }
        let params: Vec<(String, String)> = vec![
            ("grant_type".to_string(), "refresh_token".to_string()),
            ("refresh_token".to_string(), refresh),
            ("client_id".to_string(), self.client_id.clone()),
            ("client_secret".to_string(), self.client_secret.clone()),
        ];
        let tokens = self.post_token(params, "refreshing the Google access token").await?;
        if tokens.access_token.is_empty() {
            return Err(anyhow::anyhow!("Google returned no access token on refresh"));
        }
        self.cache_access(&tokens.access_token, tokens.expires_in);
        info!(expires_in = tokens.expires_in, "google_oauth: access token refreshed");
        Ok(tokens.access_token)
    }

    /// Return a FRESH access token for the service clients to put in their
    /// `Authorization: Bearer` header, refreshing transparently when the cached
    /// token is absent or within [`EXPIRY_SKEW_SECS`] of expiry. The token VALUE
    /// is returned to the caller (which uses it immediately for one request) but
    /// never logged here.
    pub async fn bearer(&self) -> IntegrationResult<String> {
        // Fast path: a cached token still comfortably valid.
        if let Ok(c) = self.cached.lock() {
            if !c.access_token.is_empty() && c.expires_at - EXPIRY_SKEW_SECS > now_unix() {
                return Ok(c.access_token.clone());
            }
        }
        // Otherwise refresh (which re-caches) and return the new token.
        self.refresh_access_token().await
    }

    // -- internals -----------------------------------------------------------

    /// POST the params to Google's token endpoint as
    /// `application/x-www-form-urlencoded` and decode the result, mapping non-2xx
    /// and `invalid_grant` to friendly, secret-free errors. The form body (which
    /// carries the secret + tokens) is never logged.
    async fn post_token(
        &self,
        params: Vec<(String, String)>,
        what: &str,
    ) -> IntegrationResult<TokenResponse> {
        // RFC 6749 §4.1.3/§6: Google's token endpoint
        // (oauth2.googleapis.com/token) requires the request parameters as
        // `application/x-www-form-urlencoded` in the body — NOT JSON, which it
        // rejects (invalid_request / unsupported media type). The transport
        // percent-encodes the pairs and sets the matching Content-Type; the
        // client_secret, code/verifier and refresh token all ride in this form
        // body, none of it logged.
        let pairs: Vec<(&str, &str)> =
            params.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let req = HttpRequest::new(HttpMethod::Post, TOKEN_ENDPOINT).form_body(&pairs);
        let resp = self.transport.send(req).await?;

        if !resp.is_success() {
            // Try to extract Google's `error` code (a fixed identifier) without
            // ever surfacing the raw body (which can echo tokens).
            let code = serde_json::from_str::<TokenError>(&resp.body)
                .ok()
                .map(|e| e.error)
                .unwrap_or_default();
            return Err(map_token_error(resp.status, &code, what));
        }
        serde_json::from_str::<TokenResponse>(&resp.body)
            .map_err(|_| anyhow::anyhow!("{what} returned an unexpected response"))
    }

    /// Cache an access token with an absolute expiry computed from `expires_in`.
    fn cache_access(&self, access_token: &str, expires_in: i64) {
        if let Ok(mut c) = self.cached.lock() {
            c.access_token = access_token.to_string();
            c.expires_at = now_unix() + expires_in.max(0);
        }
    }
}

impl GoogleAuth<super::ReqwestTransport> {
    /// Production constructor: resolve client_id + client_secret + refresh token
    /// from the Keychain and wire the real reqwest transport and the real
    /// Keychain writer. Returns a typed, friendly "not connected" error when ANY
    /// of the three is missing (the user has not finished Settings + Connect).
    pub async fn connect() -> IntegrationResult<Self> {
        let client_id = resolve_secret(ACCOUNT_CLIENT_ID)
            .await
            .ok_or_else(not_connected_error)?;
        let client_secret = resolve_secret(ACCOUNT_CLIENT_SECRET)
            .await
            .ok_or_else(not_connected_error)?;
        let refresh_token = resolve_secret(ACCOUNT_REFRESH_TOKEN)
            .await
            .ok_or_else(not_connected_error)?;
        Ok(Self::new(
            super::ReqwestTransport::new(),
            client_id,
            client_secret,
            refresh_token,
            keychain_store(),
        ))
    }

    /// Pre-consent constructor for the CONNECT flow: resolves only client_id +
    /// client_secret (the refresh token does not exist yet — consent will mint
    /// it). The caller runs begin_auth + the loopback + `exchange_code`
    /// to obtain and store it. Returns the friendly "not connected" error if the
    /// client credentials have not been pasted in Settings.
    pub async fn connect_for_consent() -> IntegrationResult<Self> {
        let client_id = resolve_secret(ACCOUNT_CLIENT_ID)
            .await
            .ok_or_else(not_connected_error)?;
        let client_secret = resolve_secret(ACCOUNT_CLIENT_SECRET)
            .await
            .ok_or_else(not_connected_error)?;
        Ok(Self::new(
            super::ReqwestTransport::new(),
            client_id,
            client_secret,
            String::new(),
            keychain_store(),
        ))
    }
}

// ===========================================================================
// (7) Runtime consent orchestrator — the production entry point that ties the
//     pure pieces (begin_auth + the loopback + exchange_code) into ONE call the
//     daemon's tool surface invokes when the user says "connect Google".
// ===========================================================================

/// Opens the consent URL in the user's real browser. Injected so the orchestrator
/// stays free of a dependency on `crate::actions` (which would be a cycle) and so
/// a test can substitute a recorder that captures the URL WITHOUT launching a
/// browser. The production caller passes a closure over `actions::open_url`.
pub type UrlOpener<'a> = Box<dyn Fn(&str) -> BoxFuture<'a, IntegrationResult<()>> + Send + Sync + 'a>;

/// What the consent flow produced, for a friendly, secret-free spoken reply. The
/// refresh token itself is NEVER carried here — it went straight to the Keychain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentOutcome {
    /// Consent succeeded: the refresh token was minted and stored in the Keychain.
    Connected,
    /// The user (or Google) refused consent (`?error=access_denied`, etc.). Carries
    /// the fixed OAuth error identifier (never secret).
    Declined(String),
}

/// The loopback port range. Binding `127.0.0.1:0` lets the OS pick any free
/// ephemeral port, which we then read back with `local_addr()` so the redirect
/// URI and the token exchange use the SAME port (Google requires byte-equality).
/// No fixed/known port is ever bound — this is a transient, one-shot loopback for
/// exactly one redirect, closed the instant consent completes.
const LOOPBACK_BIND_ADDR: &str = "127.0.0.1:0";

/// Run the FULL installed-app consent flow end to end and store the resulting
/// refresh token. This is the production runtime entry point the daemon's
/// `connect_google` tool calls; it ties together the pure, unit-tested pieces:
///
///   1. [`GoogleAuth::connect_for_consent`] resolves the client id+secret from
///      the Keychain (errors friendly if they were never pasted in Settings).
///   2. Bind the loopback on `127.0.0.1:0` (OS-picked free port) and read the
///      port back — a REAL localhost bind for a REAL consent flow (device-gated:
///      this only runs when the user explicitly asks to connect).
///   3. [`begin_auth`] builds the consent URL (PKCE challenge + CSRF state +
///      `access_type=offline`) against that port, drawing entropy from
///      [`OsEntropy`].
///   4. `open` launches the URL in the user's browser (injected so no `actions`
///      cycle and so tests don't spawn a browser).
///   5. [`receive_redirect`] accepts exactly one loopback request, validates the
///      state (CSRF) and extracts the code (or a `?error=`).
///   6. On a code, [`GoogleAuth::exchange_code`] POSTs it (with the SAME port's
///      redirect URI and the PKCE verifier) to Google, which mints + (via the
///      injected Keychain store) persists the refresh token.
///
/// No secret is ever logged: presence/port/outcome only. On a `Declined`
/// redirect we return `Ok(ConsentOutcome::Declined(..))` (a normal "user said
/// no", not an error); every other failure is a friendly, secret-free `Err`.
///
/// Generic over [`HttpTransport`] so production wires `ReqwestTransport` and the
/// test wires `MockTransport`; the listener bind + the injected opener are the
/// only impure seams, and the test exercises both on an ephemeral port.
pub async fn run_consent_flow<T: HttpTransport>(
    auth: &GoogleAuth<T>,
    open: UrlOpener<'_>,
) -> IntegrationResult<ConsentOutcome> {
    // (2) Bind the loopback the redirect URI will point at. Port 0 -> the OS
    // picks a free ephemeral port; we read it back so the auth URL and the
    // token-exchange redirect_uri agree on the exact port Google must match.
    let listener = tokio::net::TcpListener::bind(LOOPBACK_BIND_ADDR)
        .await
        .map_err(|e| anyhow::anyhow!("could not open the local consent listener: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| anyhow::anyhow!("could not read the local consent port: {e}"))?
        .port();

    // (3) Build the consent URL (PKCE + CSRF state) against that port.
    let (url, pending) = begin_auth(&auth.client_id, COMBINED_SCOPES, port, &OsEntropy);

    // (4) Open it in the user's browser. The user approves there (never automated).
    open(&url).await?;
    info!(port, "google_oauth: opened consent URL; awaiting redirect");

    // (5) Wait for the single loopback redirect and validate it.
    match receive_redirect(listener, &pending.state).await? {
        RedirectOutcome::Denied(err) => {
            // A user/Google "no" is a normal outcome, not a failure.
            info!("google_oauth: consent declined");
            Ok(ConsentOutcome::Declined(err))
        }
        RedirectOutcome::Code(code) => {
            // (6) Exchange the code (same port) -> refresh token to the Keychain.
            auth.exchange_code(&code, &pending.verifier, port).await?;
            Ok(ConsentOutcome::Connected)
        }
    }
}

// ---------------------------------------------------------------------------
// Keychain writer for the refresh token (production)
// ---------------------------------------------------------------------------


/// Build the production [`RefreshTokenStore`] that writes the refresh token to
/// the macOS Keychain under [`ACCOUNT_REFRESH_TOKEN`] via
/// `security add-generic-password -U` (update-or-add). The token is passed as an
/// argv value to security(1) only (never a shell string, never logged). This
/// runs ONLY in the real connect flow (device-gated); tests inject a recorder.
fn keychain_store() -> RefreshTokenStore {
    Box::new(|token: &str| -> IntegrationResult<()> {
        // ARGV-FREE write: the secret rides security(1)'s stdin, never argv. See
        // `super::keychain_write`.
        super::keychain_write(ACCOUNT_REFRESH_TOKEN, token)?;
        info!("google_oauth: refresh token written to keychain");
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Helpers (pure)
// ---------------------------------------------------------------------------

/// The typed, friendly "Google isn't connected" error. Used by `connect` and the
/// refresh path when a credential is missing.
fn not_connected_error() -> super::IntegrationError {
    anyhow::anyhow!("Google isn't connected — add your OAuth client in Settings and click Connect")
}

/// Map a token-endpoint failure to a friendly, secret-free error. `invalid_grant`
/// is the common "code expired/replayed" or "refresh token revoked" case and
/// gets a reconnect hint; other failures lean on the status mapper. The provider
/// body is never included.
fn map_token_error(status: u16, code: &str, what: &str) -> super::IntegrationError {
    if code == "invalid_grant" {
        return anyhow::anyhow!(
            "{what} failed — Google rejected the grant (it may have expired or been revoked); reconnect Google in Settings"
        );
    }
    match status_outcome(status) {
        StatusOutcome::Unauthorized => {
            anyhow::anyhow!("{what} failed — the OAuth client id or secret was rejected")
        }
        other => anyhow::anyhow!("{what} {}", other.friendly()),
    }
}

/// Current Unix time in seconds. Kept tiny so the token-freshness math is one
/// call. (chrono is already a dependency.)
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

// ===========================================================================
// Tests — fully hermetic. Token exchange/refresh go through MockTransport with
// canned Google JSON; the Keychain WRITE goes through an injected recorder; the
// ONE loopback test binds an EPHEMERAL 127.0.0.1:0 socket, sends itself a single
// request, and closes it immediately. No real Google round-trip, no fixed port,
// no persistent listener.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;
    use std::sync::Arc;

    // -- deterministic randomness source for tests ---------------------------

    /// A `RandomSource` that returns reproducible-but-DISTINCT bytes per call.
    /// Each `fill` mixes a monotonic call counter into the seed, so successive
    /// fills (verifier, then state) differ — mirroring production's two separate
    /// `/dev/urandom` reads — while staying deterministic for a given seed.
    struct FixedRng {
        seed: Vec<u8>,
        calls: Mutex<u8>,
    }
    impl FixedRng {
        fn new(seed: Vec<u8>) -> Self {
            Self {
                seed,
                calls: Mutex::new(0),
            }
        }
    }
    impl RandomSource for FixedRng {
        fn fill(&self, buf: &mut [u8]) {
            let n = {
                let mut c = self.calls.lock().unwrap();
                let v = *c;
                *c = c.wrapping_add(1);
                v
            };
            for (i, b) in buf.iter_mut().enumerate() {
                *b = self.seed[i % self.seed.len()].wrapping_add(n);
            }
        }
    }

    /// A recording Keychain store: captures whatever refresh token would be
    /// written, so tests assert persistence WITHOUT touching the real Keychain.
    fn recording_store() -> (RefreshTokenStore, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let log2 = log.clone();
        let store: RefreshTokenStore = Box::new(move |t: &str| {
            log2.lock().unwrap().push(t.to_string());
            Ok(())
        });
        (store, log)
    }

    // Fake credential values that, if leaked, would be unmistakable.
    const FAKE_CLIENT_ID: &str = "111-FAKE.apps.googleusercontent.com";
    const FAKE_CLIENT_SECRET: &str = "GOCSPX-FAKE-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "1//FAKE-REFRESH-TOKEN-NEVER-LEAK";

    fn auth(mock: MockTransport, refresh: &str) -> (GoogleAuth<MockTransport>, Arc<Mutex<Vec<String>>>) {
        let (store, log) = recording_store();
        let g = GoogleAuth::new(mock, FAKE_CLIENT_ID, FAKE_CLIENT_SECRET, refresh, store);
        (g, log)
    }

    /// Assert the recorded token request is an `application/x-www-form-urlencoded`
    /// POST (RFC 6749 §4.1.3/§6 — Google's token endpoint requires this, not
    /// JSON): a form body is present and it is NOT a JSON or raw body. The form
    /// body is what makes the `ReqwestTransport` emit the
    /// `application/x-www-form-urlencoded` Content-Type on the wire (the exact
    /// percent-encoding is asserted in
    /// `token_form_body_is_percent_encoded_per_rfc6749`).
    fn assert_form_content_type(req: &crate::integrations::testing::RecordedRequest) {
        assert!(req.form.is_some(), "token POST must carry a form-urlencoded body");
        assert!(req.body.is_none(), "token POST must NOT be a JSON body");
        assert!(req.raw_body.is_none(), "token POST must NOT be a raw body");
    }

    // -- (2) PKCE: RFC 7636 Appendix B test vector ---------------------------

    /// The canonical RFC 7636 §4.6 (Appendix B) S256 vector. If our base64url +
    /// SHA-256 math is correct, this verifier maps to exactly this challenge.
    #[test]
    fn pkce_s256_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = code_challenge_s256(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    /// base64url-nopad against the RFC 4648 §10 test vectors (the "foobar"
    /// progression), with `-`/`_` for 62/63 implicitly exercised by the PKCE
    /// vector above. No padding is ever emitted.
    #[test]
    fn base64url_nopad_known_vectors() {
        assert_eq!(base64url_nopad(b""), "");
        assert_eq!(base64url_nopad(b"f"), "Zg");
        assert_eq!(base64url_nopad(b"fo"), "Zm8");
        assert_eq!(base64url_nopad(b"foo"), "Zm9v");
        assert_eq!(base64url_nopad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_nopad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_nopad(b"foobar"), "Zm9vYmFy");
        // No '=' padding and no '+'/'/' ever.
        let enc = base64url_nopad(&[0xff, 0xff, 0xfe]);
        assert!(!enc.contains('='));
        assert!(!enc.contains('+'));
        assert!(!enc.contains('/'));
    }

    /// A generated verifier is in RFC 7636's legal 43..=128 length range and
    /// contains only unreserved base64url characters.
    #[test]
    fn generated_verifier_is_valid_length_and_charset() {
        let v = generate_verifier(&FixedRng::new(vec![0x42, 0x13, 0x37, 0xab, 0xcd]));
        assert!((43..=128).contains(&v.len()), "len {} out of range", v.len());
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        // Deterministic given the seed (a fresh RNG with the same seed + same
        // first-call state reproduces the same verifier).
        assert_eq!(
            generate_verifier(&FixedRng::new(vec![0x42, 0x13, 0x37, 0xab, 0xcd])),
            v
        );
    }

    // -- (3) begin_auth / URL assembly ---------------------------------------

    #[test]
    fn build_auth_url_has_every_required_param_and_no_secret() {
        let url = build_auth_url(
            FAKE_CLIENT_ID,
            COMBINED_SCOPES,
            "STATE123",
            "CHALLENGE456",
            49152,
        );
        // Endpoint.
        assert!(url.starts_with(AUTH_ENDPOINT));
        // Required params (client_id is percent-encoded — its dots survive).
        assert!(url.contains("client_id=111-FAKE.apps.googleusercontent.com"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge=CHALLENGE456"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=STATE123"));
        assert!(url.contains("access_type=offline"));
        assert!(url.contains("prompt=consent"));
        // Redirect URI is the loopback for the given port, percent-encoded.
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A49152"));
        // Scopes are space-joined then percent-encoded (space -> %20).
        assert!(url.contains("calendar.events"));
        assert!(url.contains("gmail.readonly"));
        assert!(url.contains("gmail.send"));
        assert!(url.contains("drive.file"));
        assert!(url.contains("drive.metadata.readonly"));
        assert!(url.contains("%20"), "scopes must be space-joined+encoded: {url}");
        // CRUX: the client SECRET is never part of an auth URL.
        assert!(
            !url.contains(FAKE_CLIENT_SECRET) && !url.contains("client_secret"),
            "auth URL must never carry the client secret: {url}"
        );
    }

    #[test]
    fn begin_auth_returns_url_and_correlatable_pending() {
        let rng = FixedRng::new(vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        let (url, pending) = begin_auth(FAKE_CLIENT_ID, COMBINED_SCOPES, 49152, &rng);
        // The URL carries the challenge derived from pending.verifier...
        let expected_challenge = code_challenge_s256(&pending.verifier);
        assert!(url.contains(&format!("code_challenge={expected_challenge}")));
        // ...and the state.
        assert!(url.contains(&format!("state={}", pending.state)));
        assert_eq!(pending.port, 49152);
        // The raw verifier is NOT in the URL (only its S256 challenge is).
        assert!(!url.contains(&pending.verifier), "verifier must not appear in the URL");
        // Debug redacts the verifier.
        let dbg = format!("{pending:?}");
        assert!(!dbg.contains(&pending.verifier), "Debug leaked the verifier");
        assert!(dbg.contains("verifier_present"));
    }

    // -- (4) redirect parsing truth table ------------------------------------

    #[test]
    fn parse_redirect_valid_code_with_matching_state() {
        let out = parse_redirect("GET /?code=AUTHCODE&state=S1 HTTP/1.1", "S1").unwrap();
        assert_eq!(out, RedirectOutcome::Code("AUTHCODE".to_string()));
    }

    #[test]
    fn parse_redirect_state_order_independent_and_decoded() {
        // state before code, and a percent-encoded code.
        let out = parse_redirect("GET /?state=S1&code=4%2Fabc%2Ddef HTTP/1.1", "S1").unwrap();
        assert_eq!(out, RedirectOutcome::Code("4/abc-def".to_string()));
    }

    #[test]
    fn parse_redirect_state_mismatch_is_rejected_as_csrf() {
        let err = parse_redirect("GET /?code=AUTHCODE&state=WRONG HTTP/1.1", "S1").unwrap_err();
        assert!(err.to_string().contains("CSRF"), "got: {err}");
    }

    #[test]
    fn parse_redirect_missing_state_is_rejected() {
        let err = parse_redirect("GET /?code=AUTHCODE HTTP/1.1", "S1").unwrap_err();
        assert!(err.to_string().contains("CSRF"), "got: {err}");
    }

    #[test]
    fn parse_redirect_error_access_denied_with_matching_state() {
        let out = parse_redirect("GET /?error=access_denied&state=S1 HTTP/1.1", "S1").unwrap();
        assert_eq!(out, RedirectOutcome::Denied("access_denied".to_string()));
    }

    #[test]
    fn parse_redirect_error_still_requires_matching_state() {
        // An error with a mismatched state is still a CSRF reject (we never trust
        // a redirect whose state we can't verify).
        let err = parse_redirect("GET /?error=access_denied&state=WRONG HTTP/1.1", "S1").unwrap_err();
        assert!(err.to_string().contains("CSRF"), "got: {err}");
    }

    #[test]
    fn parse_redirect_neither_code_nor_error_is_rejected() {
        let err = parse_redirect("GET /?state=S1 HTTP/1.1", "S1").unwrap_err();
        assert!(err.to_string().contains("neither code nor error"), "got: {err}");
    }

    #[test]
    fn parse_redirect_non_get_is_rejected() {
        let err = parse_redirect("POST /?code=x&state=S1 HTTP/1.1", "S1").unwrap_err();
        assert!(err.to_string().contains("method"), "got: {err}");
    }

    #[test]
    fn close_tab_response_is_well_formed_http() {
        let r = close_tab_response();
        assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(r.contains("Content-Type: text/html"));
        assert!(r.contains("close this tab"));
    }

    // -- (4) ONE scoped loopback test: ephemeral 127.0.0.1:0, one request -----

    /// Bind an EPHEMERAL loopback socket (port 0 -> OS picks a free port),
    /// connect to it, send one crafted redirect line, and assert the handler
    /// returns the code. The listener is consumed (one accept) and both sockets
    /// drop at end of scope — no fixed port, no persistent server. This is normal
    /// hermetic testing of the loopback handler, NOT running the daemon.
    #[tokio::test]
    async fn receive_redirect_happy_path_on_ephemeral_port() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // The "browser": connect and send exactly one redirect request, then
        // read the close-tab page back.
        let client = tokio::spawn(async move {
            let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
            sock.write_all(b"GET /?code=LOOPCODE&state=STATEOK HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            let mut resp = Vec::new();
            // Read until the server closes (one-shot handler shuts down).
            let _ = sock.read_to_end(&mut resp).await;
            String::from_utf8_lossy(&resp).into_owned()
        });

        let outcome = receive_redirect(listener, "STATEOK").await.unwrap();
        assert_eq!(outcome, RedirectOutcome::Code("LOOPCODE".to_string()));

        let page = client.await.unwrap();
        assert!(page.contains("close this tab"), "browser got: {page}");
    }

    // -- (5) token exchange + refresh via MockTransport ----------------------

    fn exchange_ok_json() -> &'static str {
        // Google's response to an authorization_code exchange.
        r#"{"access_token":"ACCESS-1","expires_in":3599,
            "refresh_token":"REFRESH-FROM-GOOGLE","scope":"...","token_type":"Bearer"}"#
    }

    fn refresh_ok_json() -> &'static str {
        // Refresh responses omit refresh_token.
        r#"{"access_token":"ACCESS-2","expires_in":3599,"scope":"...","token_type":"Bearer"}"#
    }

    fn invalid_grant_json() -> &'static str {
        r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#
    }

    #[tokio::test]
    async fn exchange_code_stores_refresh_and_caches_access() {
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, exchange_ok_json());
        let (g, store_log) = auth(mock, ""); // no refresh yet (pre-consent)
        g.exchange_code("AUTHCODE", "VERIFIER", 49152)
            .await
            .unwrap();

        // The refresh token was persisted via the injected store.
        let stored = store_log.lock().unwrap().clone();
        assert_eq!(stored, vec!["REFRESH-FROM-GOOGLE".to_string()]);

        // The access token is cached and bearer() returns it WITHOUT another call.
        let before = g.transport.requests().len();
        let token = g.bearer().await.unwrap();
        assert_eq!(token, "ACCESS-1");
        assert_eq!(g.transport.requests().len(), before, "cached token: no refresh call");

        // The exchange POST carried the right grant_type + code + verifier in the
        // body and went to the token endpoint.
        let req = &g.transport.requests()[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.starts_with(TOKEN_ENDPOINT));
        // RFC 6749: the token POST is application/x-www-form-urlencoded.
        assert_form_content_type(req);
        assert_eq!(req.form_param("grant_type"), Some("authorization_code"));
        assert_eq!(req.form_param("code"), Some("AUTHCODE"));
        assert_eq!(req.form_param("code_verifier"), Some("VERIFIER"));
        assert_eq!(req.form_param("redirect_uri"), Some("http://127.0.0.1:49152"));
        // Google authenticates via body params: client_secret rides in the form.
        assert!(
            req.form_param("client_secret").is_some(),
            "Google sends client_secret in the form body"
        );
    }

    #[tokio::test]
    async fn refresh_access_token_mints_new_token() {
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_ok_json());
        let (g, _log) = auth(mock, FAKE_REFRESH);
        let token = g.refresh_access_token().await.unwrap();
        assert_eq!(token, "ACCESS-2");

        // The refresh POST used grant_type=refresh_token + the stored token.
        let req = g.transport.last_request();
        // RFC 6749: the token POST is application/x-www-form-urlencoded.
        assert_form_content_type(&req);
        assert_eq!(req.form_param("grant_type"), Some("refresh_token"));
        assert_eq!(req.form_param("refresh_token"), Some(FAKE_REFRESH));
    }

    #[tokio::test]
    async fn bearer_refreshes_when_no_cached_token() {
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_ok_json());
        let (g, _log) = auth(mock, FAKE_REFRESH);
        // No cached token yet -> bearer() must refresh.
        let token = g.bearer().await.unwrap();
        assert_eq!(token, "ACCESS-2");
        assert_eq!(g.transport.requests().len(), 1, "one refresh call");
    }

    #[tokio::test]
    async fn invalid_grant_maps_to_reconnect_hint() {
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 400, invalid_grant_json());
        let (g, _log) = auth(mock, FAKE_REFRESH);
        let err = g.refresh_access_token().await.unwrap_err().to_string();
        assert!(err.contains("reconnect Google"), "got: {err}");
        assert!(!err.contains(FAKE_REFRESH), "error leaked the refresh token");
    }

    #[tokio::test]
    async fn token_401_maps_to_client_credential_rejected() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            TOKEN_ENDPOINT,
            401,
            r#"{"error":"invalid_client"}"#,
        );
        let (g, _log) = auth(mock, FAKE_REFRESH);
        let err = g.refresh_access_token().await.unwrap_err().to_string();
        assert!(err.contains("client id or secret was rejected"), "got: {err}");
    }

    #[tokio::test]
    async fn refresh_without_token_is_not_connected() {
        let mock = MockTransport::new(); // no canned response needed
        let (g, _log) = auth(mock, ""); // empty refresh token
        let err = g.refresh_access_token().await.unwrap_err().to_string();
        assert!(err.contains("Google isn't connected"), "got: {err}");
        // No request was issued (we never reached the transport).
        assert_eq!(g.transport.requests().len(), 0);
    }

    // -- (6) "not connected" friendly error ----------------------------------

    #[test]
    fn not_connected_error_is_friendly() {
        let e = not_connected_error().to_string();
        assert!(e.contains("Google isn't connected"));
        assert!(e.contains("Settings"));
        assert!(e.contains("Connect"));
    }

    // -- secrets never leak via Debug / output -------------------------------

    /// No secret (client_secret, refresh token, access token) appears in the
    /// handle's Debug output or in any error string we produce.
    #[tokio::test]
    async fn no_secret_leaks_via_debug_or_errors() {
        // Debug: presence bools only.
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, exchange_ok_json());
        let (g, _log) = auth(mock, FAKE_REFRESH);
        let dbg = format!("{g:?}");
        for secret in [FAKE_CLIENT_SECRET, FAKE_REFRESH] {
            assert!(!dbg.contains(secret), "Debug leaked a secret: {dbg}");
        }
        assert!(dbg.contains("client_secret_present"));
        assert!(dbg.contains("refresh_token_present"));

        // After a successful exchange, Debug still leaks nothing — and the access
        // token (now cached) is not printed either.
        g.exchange_code("CODE", "VER", 1).await.unwrap();
        let dbg2 = format!("{g:?}");
        assert!(!dbg2.contains("ACCESS-1"), "Debug leaked the access token");
        assert!(!dbg2.contains("REFRESH-FROM-GOOGLE"), "Debug leaked the refresh token");
        assert!(dbg2.contains("access_token_present"));

        // Error paths: an invalid_grant error never echoes the refresh token.
        let bad = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 400, invalid_grant_json());
        let (g2, _l) = auth(bad, FAKE_REFRESH);
        let err = g2.refresh_access_token().await.unwrap_err().to_string();
        assert!(!err.contains(FAKE_REFRESH), "error leaked the refresh token: {err}");
    }

    /// The token-exchange/refresh request form bodies carry the secret + tokens,
    /// but those values must never appear in a URL (only in the form body, which
    /// is TLS to Google in production and never logged).
    #[tokio::test]
    async fn secrets_ride_only_in_the_body_never_the_url() {
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_ok_json());
        let (g, _log) = auth(mock, FAKE_REFRESH);
        g.refresh_access_token().await.unwrap();
        let req = g.transport.last_request();
        assert!(!req.url.contains(FAKE_CLIENT_SECRET), "secret in URL");
        assert!(!req.url.contains(FAKE_REFRESH), "refresh token in URL");
        // They ARE present in the form body (that's where they belong).
        assert_eq!(req.form_param("client_secret"), Some(FAKE_CLIENT_SECRET));
        assert_eq!(req.form_param("refresh_token"), Some(FAKE_REFRESH));
    }

    /// The token POST is `application/x-www-form-urlencoded` (RFC 6749 §4.1.3/§6),
    /// not JSON — Google's token endpoint rejects JSON. Prove the recorded form
    /// pairs encode to a properly percent-encoded wire body via the shared
    /// `encode_form_body` (the single source of truth the transport sends), using
    /// a refresh token with special characters to pin the escaping.
    #[tokio::test]
    async fn token_form_body_is_percent_encoded_per_rfc6749() {
        let mock = MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_ok_json());
        // A refresh token whose special chars MUST be percent-encoded on the wire.
        let special_refresh = "1//a b/c+d=e&f";
        let (g, _log) = auth(mock, special_refresh);
        g.refresh_access_token().await.unwrap();
        let req = g.transport.last_request();
        assert_form_content_type(&req);
        // The recorder holds the UNENCODED pairs.
        assert_eq!(req.form_param("refresh_token"), Some(special_refresh));
        // Encode them exactly as the transport does and assert the wire bytes.
        let form = req.form.as_ref().unwrap();
        let wire = crate::integrations::encode_form_body(form);
        assert!(wire.contains("grant_type=refresh_token"), "wire: {wire}");
        // space->%20, /->%2F, +->%2B, =->%3D, &->%26 (all escaped).
        assert!(
            wire.contains("refresh_token=1%2F%2Fa%20b%2Fc%2Bd%3De%26f"),
            "special chars must be percent-encoded on the wire: {wire}"
        );
        // The raw special-char value never appears unescaped in the wire body.
        assert!(!wire.contains("1//a b/c+d=e&f"), "unescaped value leaked: {wire}");
    }

    // -- scope set is the documented combined set ----------------------------

    #[test]
    fn combined_scopes_are_the_documented_least_privilege_set() {
        assert_eq!(
            COMBINED_SCOPES,
            &[
                "https://www.googleapis.com/auth/calendar.events",
                "https://www.googleapis.com/auth/gmail.readonly",
                "https://www.googleapis.com/auth/gmail.send",
                "https://www.googleapis.com/auth/drive.file",
                "https://www.googleapis.com/auth/drive.metadata.readonly",
            ]
        );
        // Explicitly NOT the broad content-read scope.
        assert!(!COMBINED_SCOPES.contains(&"https://www.googleapis.com/auth/drive.readonly"));
        assert!(!COMBINED_SCOPES.contains(&"https://mail.google.com/"));
    }

    // -- percent-encoding/decoding round trip --------------------------------

    #[test]
    fn percent_encode_keeps_unreserved_encodes_the_rest() {
        assert_eq!(percent_encode("aZ09-_.~"), "aZ09-_.~");
        assert_eq!(percent_encode("a b/c:d"), "a%20b%2Fc%3Ad");
    }

    #[test]
    fn percent_decode_round_trips() {
        assert_eq!(percent_decode("a%20b%2Fc%3Ad"), "a b/c:d");
        assert_eq!(percent_decode("4%2Fabc"), "4/abc");
        assert_eq!(percent_decode("plain"), "plain");
    }

    // -- (7) runtime consent orchestrator (run_consent_flow) -----------------
    //
    // These exercise the production entry point end to end WITHOUT a browser or
    // network: `run_consent_flow` binds the loopback itself (127.0.0.1:0,
    // ephemeral), so the injected opener plays the role of "browser + Google" —
    // it parses the port + the CSRF `state` straight out of the consent URL the
    // flow built, connects to the loopback, and replays a redirect carrying that
    // exact state plus a canned code. The token exchange rides MockTransport. No
    // real Google round-trip, no fixed port, no persistent listener.

    /// Pull a query-parameter value out of a built consent URL (the flow
    /// percent-encodes values; for our ASCII state/port that's an identity, but
    /// we decode to be exact). Test-only helper.
    fn url_param(url: &str, key: &str) -> Option<String> {
        let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
        parse_query(query)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// Build an injected `UrlOpener` that, given the consent URL, acts as the
    /// browser+Google: it extracts the loopback port + the CSRF state from the
    /// URL and connects to the loopback, sending one redirect line built from
    /// `make_redirect(state)` (so a test can send a code, a mismatched state, or
    /// an `?error=`). Returns the opener.
    fn browser_opener<F>(make_redirect: F) -> UrlOpener<'static>
    where
        F: Fn(&str) -> String + Send + Sync + 'static,
    {
        Box::new(move |url: &str| {
            // The redirect URI carries the port the flow bound on.
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
    async fn run_consent_flow_happy_path_stores_refresh_and_returns_connected() {
        // Canned token-exchange success; the flow will POST the code to it.
        let mock =
            MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, exchange_ok_json());
        let (g, store_log) = auth(mock, ""); // pre-consent: no refresh yet

        // The "browser" echoes the real state back with a canned code.
        let opener = browser_opener(|state| {
            format!("GET /?code=LIVECODE&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        });

        let outcome = run_consent_flow(&g, opener).await.unwrap();
        assert_eq!(outcome, ConsentOutcome::Connected);

        // The refresh token Google returned was persisted via the injected store.
        let stored = store_log.lock().unwrap().clone();
        assert_eq!(stored, vec!["REFRESH-FROM-GOOGLE".to_string()]);

        // The exchange POST went to the token endpoint with the code the browser
        // sent and a redirect_uri whose port matches the loopback the flow bound
        // (begin_auth + exchange_code agreed on the OS-picked port).
        let req = &g.transport.requests()[0];
        assert!(req.url.starts_with(TOKEN_ENDPOINT));
        // RFC 6749: the token POST is application/x-www-form-urlencoded.
        assert_form_content_type(req);
        assert_eq!(req.form_param("grant_type"), Some("authorization_code"));
        assert_eq!(req.form_param("code"), Some("LIVECODE"));
        let redirect = req.form_param("redirect_uri").unwrap();
        assert!(
            redirect.starts_with("http://127.0.0.1:") && redirect != "http://127.0.0.1:0",
            "exchange must reuse the OS-picked loopback port, got {redirect}"
        );
    }

    #[tokio::test]
    async fn run_consent_flow_access_denied_returns_declined_and_stores_nothing() {
        // No token call should happen on a decline; an empty mock proves it.
        let mock = MockTransport::new();
        let (g, store_log) = auth(mock, "");

        let opener = browser_opener(|state| {
            format!("GET /?error=access_denied&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        });

        let outcome = run_consent_flow(&g, opener).await.unwrap();
        assert_eq!(outcome, ConsentOutcome::Declined("access_denied".to_string()));

        // Nothing was stored and no token request was issued.
        assert!(store_log.lock().unwrap().is_empty());
        assert_eq!(g.transport.requests().len(), 0);
    }

    #[tokio::test]
    async fn run_consent_flow_csrf_mismatch_is_rejected_before_any_exchange() {
        let mock = MockTransport::new();
        let (g, store_log) = auth(mock, "");

        // The "browser" returns a code but with a WRONG state (CSRF attempt).
        let opener = browser_opener(|_state| {
            "GET /?code=EVIL&state=WRONGSTATE HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n".to_string()
        });

        let err = run_consent_flow(&g, opener).await.unwrap_err().to_string();
        assert!(err.contains("CSRF"), "got: {err}");
        // The code was never trusted: nothing stored, no exchange POST.
        assert!(store_log.lock().unwrap().is_empty());
        assert_eq!(g.transport.requests().len(), 0);
    }
}
