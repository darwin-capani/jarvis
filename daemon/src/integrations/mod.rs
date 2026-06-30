//! Shared integration foundation: the base every Chart-2 integration (GitHub,
//! Slack, Google Drive/Calendar) plugs into. Three primitives live here so the
//! per-service clients (B/C/D) stay thin and uniformly safe:
//!
//!   1. `resolve_secret` — one generic, allowlisted macOS Keychain reader,
//!      generalizing anthropic.rs's `keychain_lookup`. Same security(1) argv,
//!      same 5s/kill_on_drop discipline, same never-log-the-secret rule, plus
//!      an internal account allowlist so a bug can never pull an arbitrary
//!      Keychain item.
//!   2. `HttpTransport` — an injectable async HTTP seam. `ReqwestTransport` is
//!      the real client (bounded timeout, like the cloud leg); `MockTransport`
//!      (test-only) returns canned responses and RECORDS requests, so client
//!      tests are fully hermetic and never hit the network.
//!   3. The consequential-action SAFETY GATE — `ActionMode` / `gate` /
//!      `consequential_allowed`. Side-effecting actions (post a message, create
//!      an event) are DRY-RUN by default; they only execute when the operator
//!      has flipped `[integrations].allow_consequential` true AND the call site
//!      passed an explicit confirm. Ships OFF, exactly like self-heal.
//!
//! The Authorization / x-api-key header is ALWAYS set per-request by the
//! calling client at the moment of the send — never stored in the transport,
//! never logged, never on argv. Presence is logged as a bool at most.
//!
//! This module is the FOUNDATION the per-service clients (B/C/D) build on: most
//! of its public surface (the transport trait, request/response builders, the
//! gate, the status mapper) is consumed by those sibling modules, which land
//! next. Until they do, the unused-public-item lint would flag the whole API,
//! so dead_code is allowed module-wide — the same "shared contract that another
//! component reads" rationale config.rs uses per-item.
#![allow(dead_code)]

// Per-service integration clients (B/C/D) that build on this foundation.
pub mod github;
// Google Calendar service client (friday/pepper/herald) — lists/reads events and,
// under the gate, creates them. Shares the `GoogleAuth` handle for its bearer.
pub mod google_calendar;
// Google OAuth2 core — one client + one consent grants Calendar/Gmail/Drive
// together; the three Google service clients share its `GoogleAuth` handle.
pub mod google_oauth;
// Provider-parameterized OAuth2 core (round 3a) — generalizes the round-2
// desktop loopback+PKCE machinery so the social platforms (X / Twitter API v2
// and LinkedIn) reuse it. Defines `ProviderConfig`/`ProviderAuth` and the shipped
// `X`/`LINKEDIN` configs; the social platform clients share its `ProviderAuth`.
pub mod oauth2;
// Meta (Facebook) Ads auth (round 3b) — the second ads provider. Meta has no
// refresh token: the installed-app dance yields a SHORT-lived token, exchanged
// once for a LONG-lived (~60-day) token stored in the Keychain. Defines `MetaAuth`
// (long-lived-token, no-refresh) + the short->long consent flow; reuses oauth2.rs's
// loopback/CSRF/randomness machinery.
pub mod meta_ads;
// X (Twitter API v2) social client (veronica) — reads the user's profile/timeline
// /mentions and, under the gate, posts a public tweet as the user. Shares the
// generic `ProviderAuth` handle (X provider) for its bearer.
pub mod x_social;
// Google Ads client (stark/gecko) — reads campaign spend via GoogleAdsService.search
// (GAQL) and, under the gate, mutates campaigns (pause/enable) and budgets — money-
// touching actions. Shares the generic `ProviderAuth` handle (GOOGLE_ADS provider)
// for its bearer and the non-OAuth `GoogleAdsCall` (developer token + customer id).
pub mod google_ads;
// LinkedIn social client (veronica/stark) — reads the member's identity and,
// under the gate, publishes a PUBLIC post as the member. Shares the generic
// `ProviderAuth` handle (LinkedIn provider) for its bearer.
pub mod linkedin;
// Gmail service client (friday/pepper) — reads metadata/snippets and, under the
// gate, sends mail as the user. Shares the `GoogleAuth` handle for its bearer.
pub mod google_gmail;
// Drive service client (friday/pepper/veronica) — lists/searches/reads file
// metadata and, under the gate, uploads small text files. Shares the
// `GoogleAuth` handle for its bearer.
pub mod google_drive;
pub mod slack;
// WHOOP biometrics client (vitalis, Health & Biometrics) — READ-ONLY: reads the
// latest recovery (score/HRV/RHR), sleep (performance/duration), and cycle strain
// over the WHOOP API. Shares the generic `ProviderAuth` handle (WHOOP provider)
// for its bearer; it never writes, so it holds no consequential surface.
pub mod whoop;
// Home Assistant smart-home bridge client (dume, Home & Environment) — READS the
// hub's entities/states (GET /api/states) ungated, and under the gate CONTROLS a
// device by calling a service (POST /api/services/<domain>/<service>). Token-based
// (base URL + long-lived token from the Keychain), not OAuth. Control goes through
// the user's OWN Home Assistant hub — JARVIS does not speak HomeKit directly.
pub mod smarthome;
// Plaid personal-finance client (midas, Personal Treasury) — READ-ONLY: reads the
// user's account balances, recent transactions, and a by-category spending summary
// over the Plaid API. Token-based (client_id + secret + per-institution access
// token from the Keychain, all in the JSON body — Plaid uses no Authorization
// header), not OAuth. HARD RULE: it holds NO consequential surface — there is no
// transfer/payment/trade method, not even a gated one — so it never imports
// `ActionMode` and never touches the foundation gate. Midas watches the books; it
// never moves the money.
pub mod plaid;
// Maps client (voyager, Travel & Logistics) — READ-ONLY: reads routes
// (directions), places (text search), and travel times (distance matrix) over a
// maps provider (Google Maps Platform). Key-based (a single Maps Platform API key
// from the Keychain), not OAuth. The key rides ONLY the `X-Goog-Api-Key` request
// HEADER — never the URL — so it can never land in a logged/recorded request line.
// HARD SCOPE: it holds NO consequential surface — there is no reservation/payment
// method, not even a gated one — so it never imports `ActionMode` and never touches
// the foundation gate. Voyager finds the way; it never books the trip.
pub mod maps;
// Have I Been Pwned breach-check client (aegis, Defense & Privacy) — READ-ONLY,
// DEFENSIVE: checks whether the USER'S OWN email appears in any known breach over
// the HIBP API. Key-based (a single HIBP API key from the Keychain), not OAuth. The
// key rides ONLY the `hibp-api-key` request HEADER — never the URL — so it can never
// land in a logged/recorded request line. NO offensive surface: it reads a breach
// catalog keyed by one address; it does not scan hosts, crack credentials, or fetch
// leaked passwords. It holds NO consequential surface (no remediation that changes
// anything), so it never imports `ActionMode` and never touches the foundation gate.
pub mod hibp;

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde_json::Value;
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::Config;

// ---------------------------------------------------------------------------
// Error type / Result alias
// ---------------------------------------------------------------------------

/// Shared error for the integration layer. A thin newtype over `anyhow::Error`
/// so clients get one `IntegrationResult` to return while still using `?` over
/// reqwest/serde errors. Display never includes secret material — callers must
/// keep tokens out of any context they attach.
pub type IntegrationError = anyhow::Error;

/// Result alias every integration client returns.
pub type IntegrationResult<T> = std::result::Result<T, IntegrationError>;

// ---------------------------------------------------------------------------
// (1) Generic Keychain secret resolver
// ---------------------------------------------------------------------------

const SECURITY_BIN: &str = "/usr/bin/security";
/// Same service namespace the HUD settings panel and anthropic.rs use.
const KEYCHAIN_SERVICE: &str = "com.jarvis.daemon";
/// security(1) is bounded exactly like actions.rs / anthropic.rs: 5s +
/// kill_on_drop so a hung subprocess can never wedge the caller.
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// The ONLY Keychain accounts this resolver will read. Defense in depth: even
/// if a caller (or a future bug) passes an attacker-influenced account string,
/// `resolve_secret` refuses anything off this set, so it can never be coerced
/// into exfiltrating an arbitrary generic-password item from the user's login
/// keychain. Anthropic's own `anthropic_api_key` is included so this resolver
/// is a strict superset of the existing lookup.
const ALLOWED_ACCOUNTS: &[&str] = &[
    "anthropic_api_key",
    "github_pat",
    "slack_bot_token",
    "google_drive_oauth",
    "google_calendar_oauth",
    // Round-2 Google OAuth2 core (google_oauth.rs): one OAuth client + one
    // consent for Calendar/Gmail/Drive. The first two are pasted by the user in
    // Settings; the refresh token is WRITTEN by JARVIS after consent. Access
    // tokens are NEVER persisted (memory only), so there is deliberately no
    // access-token account here. These constants live in google_oauth.rs
    // (`ACCOUNT_*`); the literals are repeated here only because the allowlist is
    // a `&[&str]` const — a mirror test keeps them in lockstep.
    "google_oauth_client_id",
    "google_oauth_client_secret",
    "google_oauth_refresh_token",
    // Round-3a social OAuth2 providers (oauth2.rs): X (Twitter API v2) and
    // LinkedIn, each a confidential OAuth2 client. The client id/secret are
    // pasted by the user in Settings; the refresh token is WRITTEN by JARVIS
    // after consent (access tokens are NEVER persisted — memory only, so there is
    // deliberately no access-token account). These constants live in oauth2.rs
    // (`X_ACCOUNT_*` / `LINKEDIN_ACCOUNT_*`); the literals are repeated here only
    // because the allowlist is a `&[&str]` const — a mirror test keeps them in
    // lockstep.
    "x_oauth_client_id",
    "x_oauth_client_secret",
    "x_oauth_refresh_token",
    "linkedin_oauth_client_id",
    "linkedin_oauth_client_secret",
    "linkedin_oauth_refresh_token",
    // Round-3b GOOGLE ADS (oauth2.rs `GOOGLE_ADS` ProviderConfig). A SEPARATE
    // connection from Workspace (different scope `adwords`, different refresh
    // token), so it uses its own OAuth trio. The Google Ads REST API ALSO needs
    // two non-OAuth values on every call — a developer token (a SECRET, header
    // `developer-token`) and a customer id (in the resource path) — plus an
    // OPTIONAL login-customer-id (manager account) header; all pasted by the user
    // in Settings. The refresh token is WRITTEN by JARVIS after consent. These
    // constants live in oauth2.rs (`GOOGLE_ADS_ACCOUNT_*`); a mirror test keeps
    // them in lockstep.
    "google_ads_client_id",
    "google_ads_client_secret",
    "google_ads_refresh_token",
    "google_ads_developer_token",
    "google_ads_customer_id",
    "google_ads_login_customer_id",
    // Round-3b META ADS (meta_ads.rs). Meta has NO refresh token: the app
    // credentials (`meta_app_id`/`meta_app_secret`) are pasted in Settings, the
    // ~60-day LONG-lived token (`meta_long_lived_token`) is WRITTEN by JARVIS after
    // the short->long exchange (the ONLY token persisted — no refresh token, so no
    // access-token-vs-refresh distinction here), and `meta_ad_account_id` is the
    // targeted ad account. These constants live in meta_ads.rs (`META_ACCOUNT_*`);
    // a mirror test keeps them in lockstep.
    "meta_app_id",
    "meta_app_secret",
    "meta_long_lived_token",
    "meta_ad_account_id",
    // WHOOP (oauth2.rs `WHOOP` ProviderConfig — agent "vitalis", Health &
    // Biometrics). A confidential OAuth2 client: the client id/secret are pasted
    // by the user in Settings (from their own WHOOP developer app); the refresh
    // token is WRITTEN by JARVIS after consent (access tokens are NEVER persisted —
    // memory only, so there is deliberately no access-token account). These
    // constants live in oauth2.rs (`WHOOP_ACCOUNT_*`); the literals are repeated
    // here only because the allowlist is a `&[&str]` const — a mirror test keeps
    // them in lockstep.
    "whoop_oauth_client_id",
    "whoop_oauth_client_secret",
    "whoop_oauth_refresh_token",
    // Home Assistant (smarthome.rs — agent "dume", Home & Environment). A
    // TOKEN-based local bridge, NOT OAuth: the user pastes their hub's base URL
    // (`homeassistant_url`) and a long-lived access token (`homeassistant_token`)
    // in Settings; both ride only the request at call time (the token in the
    // Authorization header), neither is logged. There is no refresh/consent flow
    // — Home Assistant long-lived tokens do not expire on a schedule. These
    // constants live in smarthome.rs (`ACCOUNT_*`); a mirror test keeps them in
    // lockstep. HONESTY: control goes through the user's OWN Home Assistant hub;
    // JARVIS does not talk HomeKit directly (raw HomeKit is not cleanly reachable
    // from a macOS daemon).
    "homeassistant_url",
    "homeassistant_token",
    // Plaid (plaid.rs — agent "midas", Personal Treasury). A TOKEN-based finance
    // READER, NOT OAuth: the user pastes their own Plaid app `plaid_client_id` +
    // `plaid_secret` in Settings, plus a per-institution `plaid_access_token` minted
    // by Plaid LINK (a frontend/sandbox token-exchange step JARVIS does not
    // perform). All three ride only the request BODY at call time (Plaid uses no
    // Authorization header); none is logged. There is no refresh/consent flow here.
    // These constants live in plaid.rs (`ACCOUNT_*`); a mirror test keeps them in
    // lockstep. HARD RULE: Midas READS only — it never moves money, so there is no
    // consequential surface and nothing to gate.
    "plaid_client_id",
    "plaid_secret",
    "plaid_access_token",
    // Maps (maps.rs — agent "voyager", Travel & Logistics). A single KEY-based maps
    // provider API key, NOT OAuth: the user pastes their own Maps Platform
    // `maps_api_key` in Settings. It rides ONLY the `X-Goog-Api-Key` request HEADER
    // at call time — never the URL — so it can never land in a logged/recorded
    // request line; it is never logged. There is no refresh/consent flow here. This
    // constant lives in maps.rs (`ACCOUNT_API_KEY`); a mirror test keeps it in
    // lockstep. READ-ONLY: Voyager reads routes/places/times and never books or pays,
    // so there is no consequential surface and nothing to gate.
    "maps_api_key",
    // Have I Been Pwned (hibp.rs — agent "aegis", Defense & Privacy). A single
    // KEY-based breach-check API key, NOT OAuth: the user pastes their own HIBP
    // `hibp_api_key` in Settings. It rides ONLY the `hibp-api-key` request HEADER at
    // call time — never the URL — so it can never land in a logged/recorded request
    // line; it is never logged. There is no refresh/consent flow here. This constant
    // lives in hibp.rs (`ACCOUNT_API_KEY`); a mirror test keeps it in lockstep.
    // DEFENSIVE + READ-ONLY: Aegis checks the user's OWN email's breach exposure and
    // never scans anyone else, so there is no consequential surface and nothing to gate.
    "hibp_api_key",
    // The user's OWN email address (aegis, Defense & Privacy). NOT a secret in the
    // credential sense — it is the address the breach check defaults to when the user
    // does not pass one. Stored as a Keychain item only so the daemon has a single
    // allowlisted place to read it; it is the user's OWN address (authorized-use:
    // Aegis checks the user's own exposure, never a third party's). Resolved via the
    // same allowlisted reader; never logged. When unset, the breach check asks the
    // user for their address rather than guessing.
    "user_email",
    // ElevenLabs cloud VOICE TIER (voice_tier.rs / speech.rs — an OPTIONAL TTS
    // layer, NOT an agent). A single KEY-based TTS provider key, NOT OAuth: the
    // user pastes their own ElevenLabs `elevenlabs_api_key` in Settings. It rides
    // ONLY the `xi-api-key` request HEADER at the inference server's TTS call —
    // never the URL, never argv, never a log/Debug/telemetry line. The cloud tier
    // SHIPS ON ([voice].cloud_tier=true) but is INERT WITHOUT A KEY, the ADDED tier
    // on top of the
    // on-device Kokoro default; with it off (or no key, or the model-swap is
    // "work offline/local") TTS behaves exactly as today (on-device Kokoro). This
    // constant lives in `crate::voice_tier::ELEVENLABS_ACCOUNT`; a mirror test keeps
    // the allowlist + the module in lockstep. VOICE-ONLY: it synthesizes speech;
    // JARVIS owns its own brain/router/turn-taking (no hosted Conversational Agents).
    "elevenlabs_api_key",
    // AT-REST ENCRYPTION master key (#11; crypto.rs). The 256-bit master key for
    // transparent whole-file SQLCipher encryption of the sensitive local stores.
    // GENERATED on enable (OS CSPRNG) and WRITTEN to the Keychain by JARVIS via the
    // same `security add-generic-password -U` pattern as the OAuth refresh tokens;
    // READ at startup by the existing `resolve_secret` to open the encrypted stores.
    // It is held in a zeroizing `crypto::SecretKey` and is NEVER logged / Debug /
    // argv / telemetry — only handed to SQLCipher's `PRAGMA key`. SHIPS OFF
    // ([security].encrypt_memory=false): no key is generated or read until the
    // operator opts in. The constant lives in `crate::crypto::MASTER_KEY_ACCOUNT`; a
    // mirror test keeps the allowlist + the module in lockstep. HONESTY: the key
    // lives in the OS Keychain — lose that item and the encrypted DBs are
    // unrecoverable.
    "memory_encryption_key",
];

/// The exact security(1) argv for a Keychain read of `account`. Factored out
/// (not inlined) so the contract-mandated invocation is asserted in tests
/// without ever executing security(1) there. Mirrors anthropic.rs's
/// `keychain_query_args`, generalized over the account.
fn keychain_query_args(account: &str) -> [&str; 6] {
    [
        "find-generic-password",
        "-s",
        KEYCHAIN_SERVICE,
        "-a",
        account,
        "-w",
    ]
}

/// Prefix/suffix bracketing a per-MCP-server Keychain account. An MCP server's
/// optional auth token lives at `mcp_<server>_token` (one account per configured
/// server), so — unlike the fixed integration accounts — the full set is not
/// statically enumerable: it is keyed by the operator's server names. Rather than
/// punch a wildcard into the allowlist, [`mcp_token_account`] mints the account
/// name from a STRICTLY-VALIDATED server name and [`account_allowed`] admits only
/// names of exactly this shape with a safe middle. A hostile server name can
/// never reach this path: the config layer validates the name on load, and the
/// pure [`is_safe_mcp_server_name`] guard here re-checks it (defense in depth) so
/// the resolver itself refuses anything but `[a-z0-9_-]+` between the brackets.
const MCP_ACCOUNT_PREFIX: &str = "mcp_";
const MCP_ACCOUNT_SUFFIX: &str = "_token";

/// Strict server-name charset for an MCP Keychain account: lowercase alnum plus
/// single `_`/`-` separators, non-empty. Pure + tiny so it is unit-testable and so
/// the keychain resolver can never be coerced with a name carrying a NUL, a slash,
/// a space, a quote, or an extra `-a`/`-w` security(1) flag.
///
/// Additionally rejects a leading/trailing `_`/`-` and any *consecutive* `_`/`-`.
/// The `__` ban is load-bearing for namespacing: the cloud tool loop addresses an
/// MCP tool by the flat id `mcp__<server>__<tool>` and recovers the boundary by
/// splitting on the first `__`, which is only unambiguous when the server name
/// itself contains no `__`. (See `mcp::flat_tool_name` / `parse_flat_tool_name`.)
/// The HUD mirror `MCP_SERVER_NAME` in `hud/src/core/credentials.ts` is kept in
/// lockstep with this rule.
fn is_safe_mcp_server_name(name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let bytes = name.as_bytes();
    let is_sep = |b: u8| b == b'_' || b == b'-';
    // No leading/trailing separator.
    if is_sep(bytes[0]) || is_sep(bytes[bytes.len() - 1]) {
        return false;
    }
    // Charset, and no two separators in a row (bans `__`, `--`, `_-`, `-_`).
    bytes.iter().enumerate().all(|(i, &b)| {
        let ok = b.is_ascii_lowercase() || b.is_ascii_digit() || is_sep(b);
        let no_double_sep = !(is_sep(b) && i > 0 && is_sep(bytes[i - 1]));
        ok && no_double_sep
    })
}

/// The Keychain account holding `server`'s auth token, or `None` when the server
/// name is not of the safe shape (so a bad name yields no account at all rather
/// than a malformed one). Single source of truth for the `mcp_<server>_token`
/// account string, shared by the resolver and the MCP layer.
pub fn mcp_token_account(server: &str) -> Option<String> {
    is_safe_mcp_server_name(server).then(|| format!("{MCP_ACCOUNT_PREFIX}{server}{MCP_ACCOUNT_SUFFIX}"))
}

/// Is `account` one this resolver is permitted to read? Pure, so the allowlist
/// is unit-testable without spawning security(1).
///
/// Two admittance paths, both fail-safe: the fixed [`ALLOWED_ACCOUNTS`] set, OR
/// an `mcp_<server>_token` account whose `<server>` middle passes the strict
/// [`is_safe_mcp_server_name`] guard. Anything else — including a malformed
/// `mcp_..._token` with an unsafe middle — is refused before any subprocess runs.
fn account_allowed(account: &str) -> bool {
    if ALLOWED_ACCOUNTS.contains(&account) {
        return true;
    }
    // mcp_<server>_token with a strictly-validated middle.
    account
        .strip_prefix(MCP_ACCOUNT_PREFIX)
        .and_then(|rest| rest.strip_suffix(MCP_ACCOUNT_SUFFIX))
        .is_some_and(is_safe_mcp_server_name)
}

/// Public, side-effect-free view of [`account_allowed`] — answers "would
/// `resolve_secret` even attempt this account?" without touching the Keychain.
/// Used by the Capability Atlas to fail the build if it ever probes an account
/// that has drifted off the allowlist (which would make that integration
/// silently, permanently inert).
pub fn account_is_allowlisted(account: &str) -> bool {
    account_allowed(account)
}

/// Read an integration secret from the macOS Keychain by account name.
///
/// Runs `security find-generic-password -s com.jarvis.daemon -a <account> -w`
/// as an args-only [`Command`] (never a shell string), with a 5s timeout and
/// kill_on_drop — the same discipline as `actions::run_command` and
/// anthropic.rs's key lookup. Returns the trimmed, non-blank secret, or `None`
/// on ANY failure: an account not on [`ALLOWED_ACCOUNTS`], a missing item
/// (errSecItemNotFound / exit 44), a locked or access-denied keychain, a
/// non-zero security(1) exit, a spawn error, or a timeout.
///
/// SECURITY: `account` is checked against the internal allowlist BEFORE any
/// subprocess runs, so an unknown or hostile account ("\0evil", "../../foo",
/// "anthropic_api_key -w extra") never reaches security(1). The secret VALUE
/// is never logged — stdout IS the secret; only presence (a bool) and, on the
/// not-found path, the exit code are recorded.
///
/// Blocking-vs-async: kept `async` to match the existing `keychain_lookup`
/// (the integration clients are already async around their HTTP calls), so the
/// subprocess wait never blocks the runtime's worker thread.
pub async fn resolve_secret(account: &str) -> Option<String> {
    if !account_allowed(account) {
        // No subprocess at all for an off-allowlist account. The account name
        // is a fixed-set identifier (not the secret), so logging it is safe
        // and the rejection is the signal an operator needs.
        warn!(account, "integrations: refusing keychain read for non-allowlisted account");
        return None;
    }

    let mut cmd = Command::new(SECURITY_BIN);
    cmd.args(keychain_query_args(account)).kill_on_drop(true);
    match tokio::time::timeout(KEYCHAIN_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let secret = String::from_utf8_lossy(&out.stdout).trim().to_string();
            // Log presence only — never the value (stdout IS the secret).
            let present = !secret.is_empty();
            info!(account, present, "integrations: keychain secret resolved");
            present.then_some(secret)
        }
        Ok(Ok(out)) => {
            // Exit 44 (errSecItemNotFound) is the normal "not configured yet"
            // case; anything else (locked/denied) is equally a None. stderr is
            // never logged — it can echo the query and we keep the surface tiny.
            info!(account, code = out.status.code(), "integrations: no keychain secret for account");
            None
        }
        Ok(Err(e)) => {
            warn!(account, error = %e, "integrations: security(1) could not run for keychain lookup");
            None
        }
        Err(_) => {
            warn!(
                account,
                secs = KEYCHAIN_TIMEOUT.as_secs(),
                "integrations: security(1) keychain lookup timed out"
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// (2) Injectable async HTTP transport
// ---------------------------------------------------------------------------

/// HTTP verbs the integration clients need. Kept tiny and explicit so the mock
/// matcher and request recorder have a closed set to reason about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl HttpMethod {
    /// Uppercase wire name, used by the mock key and the reqwest mapping.
    pub fn as_str(&self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Put => "PUT",
            HttpMethod::Patch => "PATCH",
            HttpMethod::Delete => "DELETE",
        }
    }
}

/// One outbound request. Headers carry the per-request Authorization /
/// x-api-key the CLIENT sets — the transport never owns or persists them. A body
/// is sent as exactly ONE of three shapes, at most one of which is ever set:
///   * a JSON payload ([`Self::json_body`], the common case for the REST APIs) —
///     `application/json`;
///   * an `application/x-www-form-urlencoded` parameter list ([`Self::form_body`])
///     — the RFC 6749 §4.1.3/§6 wire format OAuth2 TOKEN ENDPOINTS require (code
///     exchange + refresh); the transport percent-encodes the pairs and sets the
///     matching `Content-Type`. Used ONLY by the OAuth token POST;
///   * a verbatim text payload ([`Self::raw_body`], the rare non-JSON case like a
///     Drive multipart/related upload, where the client sets its own
///     `Content-Type`).
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    /// (name, value) header pairs. The client puts the auth header here at the
    /// moment of the call; nothing in this struct is logged by the transport.
    pub headers: Vec<(String, String)>,
    /// Optional JSON request body (POST/PUT/PATCH). `None` for bodyless GETs.
    pub body: Option<Value>,
    /// Optional `application/x-www-form-urlencoded` body as ordered (name, value)
    /// pairs (values UNENCODED here; the transport percent-encodes on the wire).
    /// Set ONLY by the OAuth2 token POST via [`Self::form_body`]. `None`
    /// otherwise. The recorder mirrors this so a test can assert the encoded
    /// params + content type WITHOUT asserting any secret VALUE.
    pub form: Option<Vec<(String, String)>>,
    /// Optional verbatim (non-JSON) text body, sent as-is with the client's own
    /// `Content-Type`. Used for multipart/related uploads. `None` unless
    /// [`Self::raw_body`] set it.
    pub raw_body: Option<String>,
}

impl HttpRequest {
    /// A bodyless request (typical GET/DELETE). Add headers with [`Self::header`].
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
            form: None,
            raw_body: None,
        }
    }

    /// Builder: append one header. Used for the per-request auth header.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Builder: attach a JSON body.
    pub fn json_body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Builder: attach an `application/x-www-form-urlencoded` body from ordered
    /// (name, value) pairs. The transport percent-encodes each value and sets the
    /// matching `Content-Type` — the RFC 6749 wire format OAuth2 token endpoints
    /// require. The pairs are stored UNENCODED (so the recorder can assert them by
    /// name without re-decoding); encoding happens only at send time.
    pub fn form_body(mut self, params: &[(&str, &str)]) -> Self {
        self.form = Some(
            params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        self
    }

    /// Builder: attach a verbatim text body sent as-is (no JSON encoding). The
    /// client is responsible for setting an appropriate `Content-Type` header.
    /// Used for the Drive multipart/related upload.
    pub fn raw_body(mut self, body: impl Into<String>) -> Self {
        self.raw_body = Some(body.into());
        self
    }
}

/// One inbound response. `status` is the raw HTTP code; `body` is the response
/// payload as text (clients parse it as JSON when they expect JSON). Bytes are
/// available via `body.as_bytes()` for the rare non-text case.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    /// Did the server return a 2xx?
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parse the body as JSON, attaching context (never the body's secrets) on
    /// failure.
    pub fn json(&self) -> IntegrationResult<Value> {
        serde_json::from_str(&self.body).context("integration response is not JSON")
    }
}

/// Encode (name, value) pairs as an `application/x-www-form-urlencoded` body
/// (RFC 3986 / WHATWG form-urlencoded): `k1=v1&k2=v2`, with EACH key and value
/// percent-encoded over the unreserved set (ALPHA / DIGIT / `-` `_` `.` `~`).
/// Space becomes `%20` (we never emit `+`, which is also legal but ambiguous).
/// Pure — the single source of truth the [`ReqwestTransport`] sends and the
/// tests assert against, so the wire bytes for the OAuth2 token POST are exact
/// and verifiable. Values (the client_secret / code / tokens) are encoded here
/// but never logged by this function.
pub fn encode_form_body(pairs: &[(String, String)]) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for &b in s.as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Boxed future alias for the object-safe transport method. The crate does not
/// depend on `async_trait`, so the trait returns a pinned boxed future — the
/// same shape async_trait would desugar to — keeping `HttpTransport` usable as
/// `dyn HttpTransport` (what clients hold).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The injectable HTTP seam. Clients hold a `&dyn HttpTransport` (or an
/// `Arc<dyn HttpTransport>`) so production wires [`ReqwestTransport`] and tests
/// wire [`MockTransport`] — identical client code, zero network in tests.
pub trait HttpTransport: Send + Sync {
    /// Send one request and resolve its response. Network/transport failures
    /// surface as `Err`; an HTTP error STATUS (4xx/5xx) is a successful send
    /// and comes back as `Ok` with that status, so clients decide how to map
    /// it (see [`status_outcome`]).
    fn send<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, IntegrationResult<HttpResponse>>;
}

/// Production transport over the shared reqwest client. Bounded request +
/// connect timeouts mirror anthropic.rs's `client()` so an integration call can
/// never wedge a caller indefinitely.
pub struct ReqwestTransport {
    client: reqwest::Client,
}

/// Per-request ceiling for an integration call — same 60s the cloud leg uses
/// for a full non-streaming completion; integration calls are far smaller, so
/// this is generous headroom, not a tight bound.
const INTEGRATION_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

impl ReqwestTransport {
    /// Build a transport with bounded timeouts. Falls back to a default client
    /// if the builder somehow fails (mirrors the daemon's existing
    /// `.expect("building HTTP client")` posture — a client build failure here
    /// is a process-startup bug, not a runtime condition).
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(INTEGRATION_REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("building integration HTTP client");
        Self { client }
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpTransport for ReqwestTransport {
    fn send<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, IntegrationResult<HttpResponse>> {
        Box::pin(async move {
            let mut builder = match req.method {
                HttpMethod::Get => self.client.get(&req.url),
                HttpMethod::Post => self.client.post(&req.url),
                HttpMethod::Put => self.client.put(&req.url),
                HttpMethod::Patch => self.client.patch(&req.url),
                HttpMethod::Delete => self.client.delete(&req.url),
            };
            // The auth header rides in here, set by the client per request —
            // never persisted on the transport, never logged.
            for (name, value) in &req.headers {
                builder = builder.header(name, value);
            }
            if let Some(body) = &req.body {
                builder = builder.json(body);
            } else if let Some(form) = &req.form {
                // application/x-www-form-urlencoded — the RFC 6749 §4.1.3/§6 wire
                // format the OAuth2 token endpoints require (code exchange +
                // refresh). We percent-encode the pairs ourselves (one source of
                // truth in `encode_form_body`, also exercised by tests) and set the
                // matching Content-Type explicitly, rather than relying on
                // reqwest's `.form()`. The pair values (which carry the
                // client_secret / code / tokens) ride in the body only — never
                // logged.
                builder = builder
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(encode_form_body(form));
            } else if let Some(raw) = &req.raw_body {
                // Verbatim body: sent as-is, with the client's own Content-Type
                // (already added above). Used for multipart/related uploads.
                builder = builder.body(raw.clone());
            }
            // SECRET-SAFE ERROR PATH: on a transport-level failure (connect
            // timeout, DNS, TLS, reset mid-body) reqwest attaches the FULL request
            // URL to its Error, and Error::Display appends " for url (<url>)" with
            // the query string UNREDACTED. Some integration clients legitimately
            // carry secrets in the query (Meta's Graph token endpoint is a
            // GET-with-query holding client_secret + the code/short-token), so a
            // leaked URL would put a live credential into the daemon log AND the
            // cloud-bound tool outcome. `reqwest::Error::without_url()` drops the
            // attached URL before we wrap it, so no request URL can ever enter an
            // error string. This is the single shared chokepoint, so the guarantee
            // holds for every current and future integration client uniformly.
            let resp = builder
                .send()
                .await
                .map_err(|e| e.without_url())
                .context("integration request failed")?;
            let status = resp.status().as_u16();
            let body = resp
                .text()
                .await
                .map_err(|e| e.without_url())
                .context("reading integration response body")?;
            Ok(HttpResponse { status, body })
        })
    }
}

// ---------------------------------------------------------------------------
// (3) Consequential-action safety gate
// ---------------------------------------------------------------------------

/// Whether a side-effecting integration action should actually run, or only
/// produce a preview. The single decision every consequential client routes
/// through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionMode {
    /// Build and return a human-readable preview; perform NO side effect.
    DryRun,
    /// Perform the real side effect (post the message, create the event).
    Execute,
}

/// Process-global master switch for consequential actions, set ONCE from
/// `[integrations].allow_consequential` at daemon startup. `None`/unset reads
/// as `false`: if init is never called (any test, or a code path that skips
/// startup), the gate is OFF — the safe default, never accidentally on.
static ALLOW_CONSEQUENTIAL: OnceLock<bool> = OnceLock::new();

/// Wire the runtime gate from the loaded config. Called once from `main()`
/// after `Config::load`. Idempotent and race-safe: a lost `set` means another
/// caller already installed the same value. Never logs more than the bool.
pub fn init(cfg: &Config) {
    let allow = cfg.integrations.allow_consequential;
    let _ = ALLOW_CONSEQUENTIAL.set(allow);
    info!(allow_consequential = allow, "integrations: consequential-action gate initialized");
}

// `#[cfg(test)]` override seam: lets a single test flip the master switch ON on
// its own thread WITHOUT mutating the set-once `ALLOW_CONSEQUENTIAL` global
// (which cannot be reset and which other tests assert is OFF). Default `None`
// means every test reads through to the `OnceLock` exactly as production does;
// only a test holding a [`ConsequentialOverride`] guard changes the answer, and
// only on its own thread. Compiled out entirely in non-test builds.
#[cfg(test)]
thread_local! {
    static CONSEQUENTIAL_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Is the operator currently permitting consequential (side-effecting)
/// actions at all? Reads `false` until [`init`] installs the configured value —
/// a code-level FAIL-SAFE: if startup is ever skipped (any test, a code path that
/// never calls `init`), the gate is OFF, never accidentally on. The SHIPPED config
/// default is `true` (full-power), which `init` propagates; the runtime gate still
/// requires a fresh per-action confirm (see `gate`) + voice-id + !lockdown.
///
/// LOCKDOWN OVERLAY (task #12): while the emergency stop is engaged
/// ([`crate::lockdown::is_locked_down`]) this is FORCED false — the master
/// outward-action switch reads OFF no matter the configured value or a per-thread
/// test override, so EVERY consequential call site (every `gate(confirm)`, the
/// confirmation replay's master-on re-check, the establish path) goes dry-run.
/// With lockdown OFF (the shipped default) this is byte-for-byte the original
/// read — lockdown only ever ADDS a force-off, never loosens.
pub fn consequential_allowed() -> bool {
    // The emergency stop wins over everything, including the test override below:
    // when locked, no consequential action is ever permitted.
    if crate::lockdown::is_locked_down() {
        return false;
    }
    // Test-only override: present only under `cfg(test)`, default `None` (read
    // through to the OnceLock). Production compiles this block out and is byte-for-
    // byte the original `*ALLOW_CONSEQUENTIAL.get().unwrap_or(&false)`.
    #[cfg(test)]
    {
        if let Some(v) = CONSEQUENTIAL_OVERRIDE.with(std::cell::Cell::get) {
            return v;
        }
    }
    *ALLOW_CONSEQUENTIAL.get().unwrap_or(&false)
}

/// `#[cfg(test)]`-only RAII guard that forces `consequential_allowed()` to a value
/// on the current thread, restoring the prior state (normally `None` -> read the
/// `OnceLock`) on drop so the override never leaks into another test. The whole
/// seam is `cfg(test)`, so production behavior is unchanged.
#[cfg(test)]
pub(crate) struct ConsequentialOverride {
    prev: Option<bool>,
}

#[cfg(test)]
impl ConsequentialOverride {
    /// Force the switch to `value` on this thread until the guard drops.
    pub(crate) fn force(value: bool) -> Self {
        let prev = CONSEQUENTIAL_OVERRIDE.with(|c| c.replace(Some(value)));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for ConsequentialOverride {
    fn drop(&mut self) {
        CONSEQUENTIAL_OVERRIDE.with(|c| c.set(self.prev));
    }
}

/// The gate every consequential action calls. Returns [`ActionMode::Execute`]
/// ONLY when BOTH the global switch is on AND the call site passed an explicit
/// `confirm` — any other combination is [`ActionMode::DryRun`]. So even with the
/// switch ON (the shipped full-power default) a consequential action without a
/// fresh `confirm` ALWAYS returns a dry-run preview and performs no side effect:
/// the master switch alone never executes anything. With the switch off (lockdown,
/// or an operator who disarmed it) it is DryRun regardless of `confirm`.
pub fn gate(confirm: bool) -> ActionMode {
    if consequential_allowed() && confirm {
        ActionMode::Execute
    } else {
        ActionMode::DryRun
    }
}

// ---------------------------------------------------------------------------
// (4) HTTP status -> friendly outcome
// ---------------------------------------------------------------------------

/// A coarse, client-agnostic classification of an HTTP status, so each
/// integration maps failures to spoken-friendly language the same way instead
/// of leaking raw codes or provider bodies into a reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusOutcome {
    /// 2xx — the request succeeded.
    Success,
    /// 401/403 — the token is missing, wrong, or lacks the needed scope.
    Unauthorized,
    /// 404 — the target (repo, channel, file) was not found.
    NotFound,
    /// 429 — rate limited; the caller should back off.
    RateLimited,
    /// Other 4xx — a client-side problem with the request.
    ClientError,
    /// 5xx — the upstream service failed.
    ServerError,
    /// Anything outside 200-599 (e.g. a synthetic/unknown code).
    Unexpected,
}

/// Map a raw status code to a [`StatusOutcome`]. Pure, so the whole table is
/// unit-tested without a network.
pub fn status_outcome(status: u16) -> StatusOutcome {
    match status {
        200..=299 => StatusOutcome::Success,
        401 | 403 => StatusOutcome::Unauthorized,
        404 => StatusOutcome::NotFound,
        429 => StatusOutcome::RateLimited,
        400..=499 => StatusOutcome::ClientError,
        500..=599 => StatusOutcome::ServerError,
        _ => StatusOutcome::Unexpected,
    }
}

impl StatusOutcome {
    /// A short, secret-free phrase suitable for a spoken reply or log line.
    /// Never includes the provider's response body (which can echo tokens or
    /// PII).
    pub fn friendly(&self) -> &'static str {
        match self {
            StatusOutcome::Success => "succeeded",
            StatusOutcome::Unauthorized => "was refused — the credential is missing or lacks access",
            StatusOutcome::NotFound => "could not find the requested item",
            StatusOutcome::RateLimited => "was rate limited; try again shortly",
            StatusOutcome::ClientError => "was rejected as a bad request",
            StatusOutcome::ServerError => "failed on the service's side",
            StatusOutcome::Unexpected => "returned an unexpected response",
        }
    }

    /// Turn a non-success outcome into an `Err` for the `?` path; `Success`
    /// returns `Ok(())`. `what` names the attempted action ("post message",
    /// "create event") for the message — keep it secret-free.
    pub fn into_result(self, what: &str) -> IntegrationResult<()> {
        if self == StatusOutcome::Success {
            Ok(())
        } else {
            Err(anyhow!("{what} {}", self.friendly()))
        }
    }
}

// ---------------------------------------------------------------------------
// MockTransport — hermetic test double (also reachable from sibling client
// test modules via `crate::integrations::testing`).
// ---------------------------------------------------------------------------

/// Hermetic transport double and helpers. Compiled only for tests.
/// `MockTransport` lives in here; every consumer (this crate's unit tests and the
/// sibling integration client tests B/C/D) reaches it via the full
/// `crate::integrations::testing::MockTransport` path, so no top-level re-export
/// is needed.
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A request as the mock saw it — recorded so a test can assert the
    /// method/URL/header-NAMES/body SHAPE without ever asserting a secret
    /// VALUE. (Tests check that an `Authorization` header is present, not what
    /// it contains.)
    #[derive(Debug, Clone)]
    pub struct RecordedRequest {
        pub method: HttpMethod,
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub body: Option<Value>,
        /// The `application/x-www-form-urlencoded` parameter list, when the client
        /// used [`HttpRequest::form_body`] (the OAuth2 token POST). Pairs are the
        /// UNENCODED (name, value) the client passed, so a test can assert the
        /// params by name + content type WITHOUT asserting a secret VALUE.
        pub form: Option<Vec<(String, String)>>,
        /// The verbatim text body, when the client used [`HttpRequest::raw_body`]
        /// (e.g. a Drive multipart upload) instead of a JSON body.
        pub raw_body: Option<String>,
    }

    impl RecordedRequest {
        /// Does a header with this (case-insensitive) name exist? Lets a test
        /// assert auth was attached without reading its value.
        pub fn has_header(&self, name: &str) -> bool {
            self.headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(name))
        }

        /// The value of the named form parameter, if a form body was set and the
        /// key is present. Lets a test assert the encoded params by name without
        /// re-decoding the wire body — and assert a secret param's PRESENCE
        /// (`.is_some()`) without asserting its VALUE.
        pub fn form_param(&self, name: &str) -> Option<&str> {
            self.form
                .as_ref()
                .and_then(|f| f.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str()))
        }
    }

    /// One canned answer, matched by HTTP method + a URL substring.
    struct Canned {
        method: HttpMethod,
        url_substring: String,
        response: HttpResponse,
    }

    /// A scriptable, recording, network-free [`HttpTransport`]. Register
    /// canned responses keyed by `(method, url-substring)`; every `send` is
    /// recorded for later assertions. Makes NO network calls — an unmatched
    /// request resolves to an explicit error so a test fails loudly rather
    /// than silently reaching out.
    pub struct MockTransport {
        canned: Vec<Canned>,
        recorded: Mutex<Vec<RecordedRequest>>,
    }

    impl MockTransport {
        /// An empty mock — register responses with [`Self::on`].
        pub fn new() -> Self {
            Self {
                canned: Vec::new(),
                recorded: Mutex::new(Vec::new()),
            }
        }

        /// Register a canned response for the first request whose method
        /// matches AND whose URL contains `url_substring`. Builder-style so a
        /// test can chain several. Later-registered entries match after
        /// earlier ones (registration order = match priority).
        pub fn on(
            mut self,
            method: HttpMethod,
            url_substring: impl Into<String>,
            status: u16,
            body: impl Into<String>,
        ) -> Self {
            self.canned.push(Canned {
                method,
                url_substring: url_substring.into(),
                response: HttpResponse {
                    status,
                    body: body.into(),
                },
            });
            self
        }

        /// Every request the mock received, in order. Tests assert URL/header
        /// names/body shape here — NEVER secret values.
        pub fn requests(&self) -> Vec<RecordedRequest> {
            self.recorded.lock().unwrap().clone()
        }

        /// Convenience: the single recorded request, panicking if there was not
        /// exactly one.
        pub fn last_request(&self) -> RecordedRequest {
            let recorded = self.recorded.lock().unwrap();
            assert_eq!(recorded.len(), 1, "expected exactly one recorded request");
            recorded[0].clone()
        }
    }

    impl Default for MockTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl HttpTransport for MockTransport {
        fn send<'a>(
            &'a self,
            req: HttpRequest,
        ) -> BoxFuture<'a, IntegrationResult<HttpResponse>> {
            Box::pin(async move {
                // Record FIRST, so even an unmatched request is observable.
                self.recorded.lock().unwrap().push(RecordedRequest {
                    method: req.method,
                    url: req.url.clone(),
                    headers: req.headers.clone(),
                    body: req.body.clone(),
                    form: req.form.clone(),
                    raw_body: req.raw_body.clone(),
                });
                match self
                    .canned
                    .iter()
                    .find(|c| c.method == req.method && req.url.contains(&c.url_substring))
                {
                    Some(c) => Ok(c.response.clone()),
                    None => Err(anyhow!(
                        "MockTransport: no canned response for {} {}",
                        req.method.as_str(),
                        req.url
                    )),
                }
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::testing::MockTransport;
    use super::*;

    // -- (1) resolver allowlist ------------------------------------------------

    /// Every shipping integration account is on the allowlist (so a real
    /// resolve can succeed) and nothing else is. This is the set the task pins:
    /// the round-1 accounts, the round-2 Google OAuth2 trio, and the round-3a
    /// social OAuth2 trios (X + LinkedIn).
    #[test]
    fn allowlist_is_exactly_the_known_integration_accounts() {
        for account in [
            "anthropic_api_key",
            "github_pat",
            "slack_bot_token",
            "google_drive_oauth",
            "google_calendar_oauth",
            "google_oauth_client_id",
            "google_oauth_client_secret",
            "google_oauth_refresh_token",
            "x_oauth_client_id",
            "x_oauth_client_secret",
            "x_oauth_refresh_token",
            "linkedin_oauth_client_id",
            "linkedin_oauth_client_secret",
            "linkedin_oauth_refresh_token",
            "google_ads_client_id",
            "google_ads_client_secret",
            "google_ads_refresh_token",
            "google_ads_developer_token",
            "google_ads_customer_id",
            "google_ads_login_customer_id",
            "meta_app_id",
            "meta_app_secret",
            "meta_long_lived_token",
            "meta_ad_account_id",
            "whoop_oauth_client_id",
            "whoop_oauth_client_secret",
            "whoop_oauth_refresh_token",
            "homeassistant_url",
            "homeassistant_token",
            "plaid_client_id",
            "plaid_secret",
            "plaid_access_token",
            "maps_api_key",
            "hibp_api_key",
            "user_email",
            "elevenlabs_api_key",
            "memory_encryption_key",
        ] {
            assert!(account_allowed(account), "{account} must be allowed");
        }
        assert_eq!(ALLOWED_ACCOUNTS.len(), 37, "the allowlist must not grow silently");
    }

    /// Lockstep: the at-rest encryption master-key account literal on the allowlist
    /// matches the `MASTER_KEY_ACCOUNT` constant `crypto.rs` owns, so the allowlist
    /// and the crypto module can never drift on the account name. (There is no
    /// separate "decrypted" or per-store account — one master key, one account.)
    #[test]
    fn allowlist_includes_the_memory_encryption_key_account_constant() {
        assert!(
            account_allowed(crate::crypto::MASTER_KEY_ACCOUNT),
            "{} (crypto const) must be allowed",
            crate::crypto::MASTER_KEY_ACCOUNT
        );
        assert_eq!(crate::crypto::MASTER_KEY_ACCOUNT, "memory_encryption_key");
    }

    /// Lockstep: the Google OAuth account literals on the allowlist match the
    /// `ACCOUNT_*` constants the `google_oauth` module owns, so the allowlist and
    /// the client can never drift on the account names.
    #[test]
    fn allowlist_includes_the_google_oauth_account_constants() {
        use crate::integrations::google_oauth::{
            ACCOUNT_CLIENT_ID, ACCOUNT_CLIENT_SECRET, ACCOUNT_REFRESH_TOKEN,
        };
        for account in [ACCOUNT_CLIENT_ID, ACCOUNT_CLIENT_SECRET, ACCOUNT_REFRESH_TOKEN] {
            assert!(account_allowed(account), "{account} (google_oauth const) must be allowed");
        }
        // And there is NO access-token account — access tokens stay in memory.
        assert!(!account_allowed("google_oauth_access_token"));
    }

    /// Lockstep: the X + LinkedIn OAuth account literals on the allowlist match
    /// the `*_ACCOUNT_*` constants the `oauth2` module owns (and the constants the
    /// shipped `ProviderConfig`s reference), so the allowlist, the configs and the
    /// client can never drift on the account names.
    #[test]
    fn allowlist_includes_the_social_oauth_account_constants() {
        use crate::integrations::oauth2::{
            LINKEDIN, LINKEDIN_ACCOUNT_CLIENT_ID, LINKEDIN_ACCOUNT_CLIENT_SECRET,
            LINKEDIN_ACCOUNT_REFRESH_TOKEN, X, X_ACCOUNT_CLIENT_ID, X_ACCOUNT_CLIENT_SECRET,
            X_ACCOUNT_REFRESH_TOKEN,
        };
        for account in [
            X_ACCOUNT_CLIENT_ID,
            X_ACCOUNT_CLIENT_SECRET,
            X_ACCOUNT_REFRESH_TOKEN,
            LINKEDIN_ACCOUNT_CLIENT_ID,
            LINKEDIN_ACCOUNT_CLIENT_SECRET,
            LINKEDIN_ACCOUNT_REFRESH_TOKEN,
        ] {
            assert!(account_allowed(account), "{account} (oauth2 const) must be allowed");
        }
        // The ProviderConfigs reference exactly these allowlisted accounts.
        for cfg in [X, LINKEDIN] {
            assert!(account_allowed(cfg.account_client_id), "{} client_id", cfg.name);
            assert!(account_allowed(cfg.account_client_secret), "{} client_secret", cfg.name);
            assert!(account_allowed(cfg.account_refresh_token), "{} refresh", cfg.name);
        }
        // No social access-token account — access tokens stay in memory.
        assert!(!account_allowed("x_oauth_access_token"));
        assert!(!account_allowed("linkedin_oauth_access_token"));
    }

    /// Lockstep: the Google Ads account literals on the allowlist match the
    /// `GOOGLE_ADS_ACCOUNT_*` constants the `oauth2` module owns (and the OAuth
    /// trio the `GOOGLE_ADS` ProviderConfig references), so the allowlist, the
    /// config and the client can never drift on the account names.
    #[test]
    fn allowlist_includes_the_google_ads_account_constants() {
        use crate::integrations::oauth2::{
            GOOGLE_ADS, GOOGLE_ADS_ACCOUNT_CLIENT_ID, GOOGLE_ADS_ACCOUNT_CLIENT_SECRET,
            GOOGLE_ADS_ACCOUNT_CUSTOMER_ID, GOOGLE_ADS_ACCOUNT_DEVELOPER_TOKEN,
            GOOGLE_ADS_ACCOUNT_LOGIN_CUSTOMER_ID, GOOGLE_ADS_ACCOUNT_REFRESH_TOKEN,
        };
        for account in [
            GOOGLE_ADS_ACCOUNT_CLIENT_ID,
            GOOGLE_ADS_ACCOUNT_CLIENT_SECRET,
            GOOGLE_ADS_ACCOUNT_REFRESH_TOKEN,
            GOOGLE_ADS_ACCOUNT_DEVELOPER_TOKEN,
            GOOGLE_ADS_ACCOUNT_CUSTOMER_ID,
            GOOGLE_ADS_ACCOUNT_LOGIN_CUSTOMER_ID,
        ] {
            assert!(account_allowed(account), "{account} (oauth2 const) must be allowed");
        }
        // The ProviderConfig's OAuth trio is the allowlisted Google-Ads trio —
        // distinct from Workspace's `google_oauth_*` accounts.
        assert!(account_allowed(GOOGLE_ADS.account_client_id));
        assert!(account_allowed(GOOGLE_ADS.account_client_secret));
        assert!(account_allowed(GOOGLE_ADS.account_refresh_token));
        assert_ne!(
            GOOGLE_ADS.account_refresh_token, "google_oauth_refresh_token",
            "Google Ads is a SEPARATE connection from Workspace"
        );
        // No Google-Ads access-token account — access tokens stay in memory.
        assert!(!account_allowed("google_ads_access_token"));
    }

    /// Lockstep: the Meta Ads account literals on the allowlist match the
    /// `META_ACCOUNT_*` constants the `meta_ads` module owns.
    #[test]
    fn allowlist_includes_the_meta_ads_account_constants() {
        use crate::integrations::meta_ads::{
            META_ACCOUNT_AD_ACCOUNT_ID, META_ACCOUNT_APP_ID, META_ACCOUNT_APP_SECRET,
            META_ACCOUNT_LONG_LIVED_TOKEN,
        };
        for account in [
            META_ACCOUNT_APP_ID,
            META_ACCOUNT_APP_SECRET,
            META_ACCOUNT_LONG_LIVED_TOKEN,
            META_ACCOUNT_AD_ACCOUNT_ID,
        ] {
            assert!(account_allowed(account), "{account} (meta_ads const) must be allowed");
        }
        // Meta has no refresh token: the long-lived token IS the persisted grant,
        // so there is deliberately no separate refresh/access account.
        assert!(!account_allowed("meta_refresh_token"));
        assert!(!account_allowed("meta_access_token"));
    }

    /// Lockstep: the ElevenLabs cloud-voice-tier account literal on the allowlist
    /// matches the `ELEVENLABS_ACCOUNT` constant the `voice_tier` module owns, so
    /// the allowlist + the module can never drift on the account name. This is the
    /// xi-api-key the inference server reads only from the daemon-passed request —
    /// it is allowlisted exactly like every other Keychain credential.
    #[test]
    fn allowlist_includes_the_elevenlabs_voice_account_constant() {
        assert!(
            account_allowed(crate::voice_tier::ELEVENLABS_ACCOUNT),
            "{} (voice_tier const) must be allowed",
            crate::voice_tier::ELEVENLABS_ACCOUNT
        );
        assert_eq!(crate::voice_tier::ELEVENLABS_ACCOUNT, "elevenlabs_api_key");
        // VOICE-ONLY tier: there is no separate refresh/oauth account — ElevenLabs
        // TTS is a single header key, not an OAuth flow.
        assert!(!account_allowed("elevenlabs_oauth_refresh_token"));
        assert!(!account_allowed("elevenlabs_access_token"));
    }

    /// Lockstep: the WHOOP OAuth account literals on the allowlist match the
    /// `WHOOP_ACCOUNT_*` constants the `oauth2` module owns (and the OAuth trio the
    /// `WHOOP` ProviderConfig references), so the allowlist, the config and the
    /// client can never drift on the account names.
    #[test]
    fn allowlist_includes_the_whoop_oauth_account_constants() {
        use crate::integrations::oauth2::{
            WHOOP, WHOOP_ACCOUNT_CLIENT_ID, WHOOP_ACCOUNT_CLIENT_SECRET,
            WHOOP_ACCOUNT_REFRESH_TOKEN,
        };
        for account in [
            WHOOP_ACCOUNT_CLIENT_ID,
            WHOOP_ACCOUNT_CLIENT_SECRET,
            WHOOP_ACCOUNT_REFRESH_TOKEN,
        ] {
            assert!(account_allowed(account), "{account} (oauth2 const) must be allowed");
        }
        // The ProviderConfig references exactly these allowlisted accounts.
        assert!(account_allowed(WHOOP.account_client_id));
        assert!(account_allowed(WHOOP.account_client_secret));
        assert!(account_allowed(WHOOP.account_refresh_token));
        // No WHOOP access-token account — access tokens stay in memory.
        assert!(!account_allowed("whoop_oauth_access_token"));
    }

    /// Lockstep: the Home Assistant account literals on the allowlist match the
    /// `ACCOUNT_*` constants the `smarthome` module owns, so the allowlist and the
    /// client can never drift on the account names.
    #[test]
    fn allowlist_includes_the_smarthome_account_constants() {
        use crate::integrations::smarthome::{ACCOUNT_TOKEN, ACCOUNT_URL};
        for account in [ACCOUNT_URL, ACCOUNT_TOKEN] {
            assert!(account_allowed(account), "{account} (smarthome const) must be allowed");
        }
        // Home Assistant long-lived tokens are not OAuth — there is no client
        // id/secret or refresh-token account here.
        assert!(!account_allowed("homeassistant_client_id"));
        assert!(!account_allowed("homeassistant_refresh_token"));
    }

    /// Lockstep: the Plaid account literals on the allowlist match the `ACCOUNT_*`
    /// constants the `plaid` module owns, so the allowlist and the client can never
    /// drift on the account names. Plaid is token-based (client_id + secret +
    /// access_token, all in the body), not OAuth — so there is no client-id/secret
    /// OAuth distinction and no refresh-token account here.
    #[test]
    fn allowlist_includes_the_plaid_account_constants() {
        use crate::integrations::plaid::{ACCOUNT_ACCESS_TOKEN, ACCOUNT_CLIENT_ID, ACCOUNT_SECRET};
        for account in [ACCOUNT_CLIENT_ID, ACCOUNT_SECRET, ACCOUNT_ACCESS_TOKEN] {
            assert!(account_allowed(account), "{account} (plaid const) must be allowed");
        }
        // Plaid is not OAuth: no refresh-token account, no separate access-token-vs-
        // refresh distinction.
        assert!(!account_allowed("plaid_refresh_token"));
        assert!(!account_allowed("plaid_oauth_client_id"));
    }

    /// Lockstep: the Maps account literal on the allowlist matches the `ACCOUNT_*`
    /// constant the `maps` module owns, so the allowlist and the client can never
    /// drift on the account name. Maps is a single KEY-based provider key (not
    /// OAuth) — so there is no client-id/secret OAuth distinction and no
    /// refresh-token account here.
    #[test]
    fn allowlist_includes_the_maps_account_constant() {
        use crate::integrations::maps::ACCOUNT_API_KEY;
        assert!(account_allowed(ACCOUNT_API_KEY), "{ACCOUNT_API_KEY} (maps const) must be allowed");
        assert_eq!(ACCOUNT_API_KEY, "maps_api_key");
        // Maps is not OAuth: no client-id/secret or refresh-token account here.
        assert!(!account_allowed("maps_oauth_client_id"));
        assert!(!account_allowed("maps_refresh_token"));
    }

    /// Lockstep: the HIBP account literal on the allowlist matches the `ACCOUNT_*`
    /// constant the `hibp` module owns, so the allowlist and the client can never
    /// drift on the account name. HIBP is a single KEY-based provider key (not
    /// OAuth) — so there is no client-id/secret OAuth distinction and no
    /// refresh-token account here. The user's own email address is allowlisted too
    /// (the breach check's default address), but it is NOT a secret/OAuth account.
    #[test]
    fn allowlist_includes_the_hibp_account_constant() {
        use crate::integrations::hibp::ACCOUNT_API_KEY;
        assert!(account_allowed(ACCOUNT_API_KEY), "{ACCOUNT_API_KEY} (hibp const) must be allowed");
        assert_eq!(ACCOUNT_API_KEY, "hibp_api_key");
        // The user's own email (the breach-check default) is allowlisted.
        assert!(account_allowed("user_email"));
        // HIBP is not OAuth: no client-id/secret or refresh-token account here.
        assert!(!account_allowed("hibp_oauth_client_id"));
        assert!(!account_allowed("hibp_refresh_token"));
    }

    /// Unknown and HOSTILE account strings are rejected by the pure allowlist
    /// check — so `resolve_secret` never even spawns security(1) for them.
    /// Covers the null-byte and path-traversal style inputs the task calls out,
    /// plus argv-injection-shaped strings.
    #[test]
    fn hostile_and_unknown_accounts_are_rejected() {
        for bad in [
            "",
            "unknown",
            "ANTHROPIC_API_KEY", // case games must not pass
            "anthropic_api_key ", // trailing space is a different item
            "\0evil",
            "github_pat\0",
            "../../secret",
            "../login.keychain",
            "anthropic_api_key -w extra", // argv-shaped (harmless: args are not a shell)
            "com.apple.something",
        ] {
            assert!(!account_allowed(bad), "{bad:?} must be rejected");
        }
    }

    /// MCP per-server token accounts (`mcp_<server>_token`) are admitted ONLY for
    /// a strictly-validated server name; the fixed allowlist is untouched (still
    /// exactly 35). A hostile name (traversal, NUL, space, argv-shaped, empty
    /// middle) is refused, and `mcp_token_account` mints NO account for it — so
    /// such a name can never reach security(1).
    #[test]
    fn mcp_token_accounts_admitted_only_for_safe_server_names() {
        // Valid names -> admitted + a derivable account.
        for name in ["files", "weather-api", "srv_1", "a"] {
            let account = format!("mcp_{name}_token");
            assert!(account_allowed(&account), "{account} must be allowed");
            assert_eq!(mcp_token_account(name).as_deref(), Some(account.as_str()));
        }
        // Hostile / malformed middles -> refused, and no account is minted.
        for bad in [
            "mcp__token",                   // empty middle
            "mcp_../../etc_token",          // traversal
            "mcp_a b_token",                // space
            "mcp_a\0b_token",               // NUL
            "mcp_Files_token",              // uppercase (strict lowercase)
            "mcp_a_token -w x",             // argv-shaped
            "mcp_files",                    // missing suffix
            "files_token",                  // missing prefix
        ] {
            assert!(!account_allowed(bad), "{bad:?} must be rejected");
        }
        for bad_name in ["../../etc", "a b", "a\0b", "", "Files"] {
            assert!(mcp_token_account(bad_name).is_none(), "{bad_name:?} mints no account");
        }
        // `__` (and any leading/trailing/consecutive separator) mints no account,
        // because the flat namespacing id `mcp__<server>__<tool>` splits on the
        // first `__` and is only unambiguous when the server name carries no `__`.
        for bad_name in [
            "weather__api", // double underscore — would mis-split the flat id
            "a__b",
            "a--b",  // consecutive `-`
            "a_-b",  // mixed consecutive separators
            "_files", // leading separator
            "files_", // trailing separator
            "-files",
            "files-",
        ] {
            assert!(
                mcp_token_account(bad_name).is_none(),
                "{bad_name:?} must mint no Keychain account"
            );
            assert!(
                !account_allowed(&format!("mcp_{bad_name}_token")),
                "mcp_{bad_name}_token must be refused"
            );
        }
        // The fixed allowlist did not grow.
        assert_eq!(ALLOWED_ACCOUNTS.len(), 37, "the fixed allowlist must not change");
    }

    /// `resolve_secret` returns `None` for a non-allowlisted account WITHOUT
    /// touching security(1) — the allowlist is checked first, so this is safe
    /// to run in the build/test sandbox (no subprocess for the rejected path).
    #[tokio::test]
    async fn resolve_secret_rejects_unknown_account_without_subprocess() {
        assert!(resolve_secret("definitely-not-allowed").await.is_none());
        assert!(resolve_secret("\0evil").await.is_none());
        assert!(resolve_secret("../../etc/passwd").await.is_none());
    }

    /// The security(1) argv is exactly the contract invocation, generalized
    /// over the account: find-generic-password against com.jarvis.daemon with
    /// -w. Asserted without running security(1).
    #[test]
    fn keychain_argv_is_the_contract_invocation() {
        assert_eq!(
            keychain_query_args("github_pat"),
            ["find-generic-password", "-s", "com.jarvis.daemon", "-a", "github_pat", "-w"]
        );
    }

    // -- (3) gate truth table --------------------------------------------------

    /// `gate` returns Execute ONLY when (allowed && confirm). The other three
    /// rows are DryRun. We drive `consequential_allowed` via the pure helper
    /// `gate_mode` so the truth table does not depend on process-global init
    /// order (which a parallel test could otherwise race).
    #[test]
    fn gate_truth_table() {
        // Pure form of the gate, independent of the OnceLock global.
        fn gate_mode(allowed: bool, confirm: bool) -> ActionMode {
            if allowed && confirm {
                ActionMode::Execute
            } else {
                ActionMode::DryRun
            }
        }
        assert_eq!(gate_mode(false, false), ActionMode::DryRun);
        assert_eq!(gate_mode(false, true), ActionMode::DryRun, "flag off: confirm cannot execute");
        assert_eq!(gate_mode(true, false), ActionMode::DryRun, "no confirm: never executes");
        assert_eq!(gate_mode(true, true), ActionMode::Execute);
    }

    /// Code-level FAIL-SAFE (NOT the shipped config default, which is ON): with the
    /// global gate UNINITIALIZED (as in any test that does not call `init`, or any
    /// code path that skips startup), `consequential_allowed()` is false and
    /// `gate(true)` is therefore a DryRun — no side effect can happen until `init`
    /// propagates the (full-power) config AND a fresh confirm clears.
    #[test]
    fn consequential_ships_off_and_gate_is_dryrun_by_default() {
        // `init` is never called in this test, so the OnceLock is unset ->
        // false. (Other tests in this binary also never enable it.)
        assert!(!consequential_allowed(), "uninitialized gate must FAIL SAFE to off");
        assert_eq!(gate(true), ActionMode::DryRun, "off + confirm must still be a preview");
        assert_eq!(gate(false), ActionMode::DryRun);
    }

    /// Config lockstep: `[integrations].allow_consequential` defaults to TRUE
    /// (full-power default — the master gate ships ARMED), and `init` would
    /// propagate that. This is INERT-SAFE: the RUNTIME gate
    /// (`consequential_allowed()` / `gate()`) still enforces voice-id + confirm +
    /// !lockdown + policy at the chokepoints regardless of this default — see
    /// `consequential_ships_off_and_gate_is_dryrun_by_default`, which proves the
    /// runtime gate is DryRun until `init` propagates the (now ON) config AND a
    /// fresh confirm clears.
    #[test]
    fn config_default_for_allow_consequential_is_true() {
        let cfg = Config::default();
        assert!(
            cfg.integrations.allow_consequential,
            "allow_consequential defaults to true (full-power default; runtime gate still enforces per-action)"
        );
    }

    // -- (4) status mapping ----------------------------------------------------

    #[test]
    fn status_outcome_maps_the_classes() {
        assert_eq!(status_outcome(200), StatusOutcome::Success);
        assert_eq!(status_outcome(204), StatusOutcome::Success);
        assert_eq!(status_outcome(401), StatusOutcome::Unauthorized);
        assert_eq!(status_outcome(403), StatusOutcome::Unauthorized);
        assert_eq!(status_outcome(404), StatusOutcome::NotFound);
        assert_eq!(status_outcome(429), StatusOutcome::RateLimited);
        assert_eq!(status_outcome(422), StatusOutcome::ClientError);
        assert_eq!(status_outcome(500), StatusOutcome::ServerError);
        assert_eq!(status_outcome(503), StatusOutcome::ServerError);
        assert_eq!(status_outcome(100), StatusOutcome::Unexpected);
        assert_eq!(status_outcome(700), StatusOutcome::Unexpected);
    }

    #[test]
    fn status_outcome_into_result_only_oks_success() {
        assert!(status_outcome(200).into_result("post message").is_ok());
        let err = status_outcome(403).into_result("post message").unwrap_err();
        // The action name surfaces; no status code or body leaks into it.
        assert!(err.to_string().contains("post message"));
        assert!(!err.to_string().contains("403"));
    }

    // -- (2) MockTransport recording + matching --------------------------------

    /// MockTransport returns the canned response for a matching (method, URL
    /// substring) and RECORDS the request — including that an auth header was
    /// present — without exposing the secret value to the assertion and without
    /// any network call.
    #[tokio::test]
    async fn mock_transport_records_requests_and_serves_canned_responses() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/user", 200, r#"{"login":"octocat"}"#)
            .on(HttpMethod::Post, "/repos/o/r/issues", 201, r#"{"number":7}"#);

        // A GET with a (fake) bearer token the test never asserts the value of.
        let get = HttpRequest::new(HttpMethod::Get, "https://api.github.com/user")
            .header("Authorization", "Bearer SECRET-NEVER-ASSERTED");
        let resp = mock.send(get).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.json().unwrap()["login"], "octocat");

        // A POST with a JSON body.
        let post = HttpRequest::new(HttpMethod::Post, "https://api.github.com/repos/o/r/issues")
            .header("Authorization", "Bearer SECRET-NEVER-ASSERTED")
            .json_body(serde_json::json!({"title": "hi"}));
        let resp = mock.send(post).await.unwrap();
        assert_eq!(resp.status, 201);

        let recorded = mock.requests();
        assert_eq!(recorded.len(), 2, "both requests recorded");

        // Assert SHAPE, never the secret VALUE.
        assert_eq!(recorded[0].method, HttpMethod::Get);
        assert!(recorded[0].url.ends_with("/user"));
        assert!(recorded[0].has_header("authorization"), "auth header present (value not asserted)");
        assert!(recorded[0].body.is_none());

        assert_eq!(recorded[1].method, HttpMethod::Post);
        assert!(recorded[1].url.contains("/issues"));
        assert_eq!(recorded[1].body.as_ref().unwrap()["title"], "hi");
    }

    /// An unmatched request errors loudly (proving the mock never silently
    /// reaches the network) and is STILL recorded.
    #[tokio::test]
    async fn mock_transport_errors_and_records_on_unmatched_request() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/user", 200, "{}");
        let req = HttpRequest::new(HttpMethod::Delete, "https://api.github.com/nope");
        let err = mock.send(req).await.unwrap_err();
        assert!(err.to_string().contains("no canned response"));
        assert_eq!(mock.requests().len(), 1, "unmatched request still recorded");
    }

    /// `ReqwestTransport` constructs cleanly (bounded timeouts) without making
    /// any network call — construction must be side-effect-free.
    #[test]
    fn reqwest_transport_constructs() {
        let _ = ReqwestTransport::new();
        let _ = ReqwestTransport::default();
    }

    /// `HttpResponse::is_success` matches the 2xx band exactly.
    #[test]
    fn http_response_success_band() {
        assert!(HttpResponse { status: 200, body: String::new() }.is_success());
        assert!(HttpResponse { status: 299, body: String::new() }.is_success());
        assert!(!HttpResponse { status: 300, body: String::new() }.is_success());
        assert!(!HttpResponse { status: 404, body: String::new() }.is_success());
    }
}
