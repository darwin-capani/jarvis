//! JARVIS HUD shell — Tauri 2 commands for the multi-credential settings panel.
//!
//! Keychain access is IN-PROCESS via the Security.framework bindings
//! (`security-framework` crate) — secret material never appears on any process
//! command line. The service string is `com.jarvis.daemon`; each credential
//! has a fixed account string drawn from the registry in `credentials.rs`.
//! Every account argument is validated against that registry allowlist BEFORE
//! any Keychain operation, so the frontend cannot write arbitrary items.
//!
//! Secret values are NEVER logged, never appear in telemetry or error strings,
//! and never leave this process except in the verify request headers
//! (`x-api-key` / `Authorization: Bearer`) the user explicitly triggers.

mod actuator;
mod command;
mod config_settings;
mod credentials;
mod heal;
mod mic_stream;
mod permissions;
mod setup;
mod tcc;
mod takeover;
mod updates;
mod uninstall;

use std::time::Duration;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{Emitter, WebviewWindow};

use takeover::{reset_presentation_to_default, Mutation, Takeover};

use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use serde::Serialize;

use credentials::{account_for_id, credential_by_id, is_known_account, Kind};

const SERVICE: &str = "com.jarvis.daemon";
const API_TIMEOUT: Duration = Duration::from_secs(10);
/// Security.framework OSStatus for errSecItemNotFound.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// Run a blocking Security.framework call off the async runtime. Errors are
/// status descriptions only — never secret material.
async fn run_keychain<T, F>(op: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, security_framework::base::Error> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(op)
        .await
        .map_err(|e| format!("keychain task failed: {e}"))?
        .map_err(|e| format!("keychain error (OSStatus {})", e.code()))
}

/// Reject any account not in the registry allowlist. The error never echoes a
/// secret (the account string is a fixed public identifier, not a secret).
fn guard_account(account: &str) -> Result<(), String> {
    if is_known_account(account) {
        Ok(())
    } else {
        Err(format!("unknown credential account: {account}"))
    }
}

/// Presence check for one credential account. Returns only a bool.
#[tauri::command]
async fn keychain_status(account: String) -> Result<bool, String> {
    guard_account(&account)?;
    run_keychain(move || match get_generic_password(SERVICE, &account) {
        Ok(_) => Ok(true),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
        Err(e) => Err(e),
    })
    .await
}

/// Save (create-or-update) a secret as a generic password item.
#[tauri::command]
async fn keychain_set(account: String, secret: String) -> Result<(), String> {
    guard_account(&account)?;
    let secret = secret.trim().to_string();
    if secret.is_empty() {
        return Err("secret is empty".to_string());
    }
    run_keychain(move || set_generic_password(SERVICE, &account, secret.as_bytes())).await
}

/// Remove an item. A missing item (errSecItemNotFound) is treated as success —
/// the desired end state (no item) holds either way.
#[tauri::command]
async fn keychain_delete(account: String) -> Result<(), String> {
    guard_account(&account)?;
    run_keychain(move || match delete_generic_password(SERVICE, &account) {
        Ok(()) => Ok(()),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(()),
        Err(e) => Err(e),
    })
    .await
}

/// Internal Keychain write for an already-validated account.
async fn keychain_set_internal(account: &'static str, secret: String) -> Result<(), String> {
    run_keychain(move || set_generic_password(SERVICE, account, secret.as_bytes())).await
}

/* ----------------------------------------------------------- verification */

/// Typed verification outcome returned to the UI. `status` is the contract
/// vocabulary; `detail` is a short human string (never contains the secret).
#[derive(Debug, Serialize, PartialEq, Eq)]
struct VerifyResult {
    status: String,
    detail: String,
}

impl VerifyResult {
    fn valid(detail: impl Into<String>) -> Self {
        Self { status: "valid".into(), detail: detail.into() }
    }
    fn unauthorized(detail: impl Into<String>) -> Self {
        Self { status: "unauthorized".into(), detail: detail.into() }
    }
    fn network(detail: impl Into<String>) -> Self {
        Self { status: "network_error".into(), detail: detail.into() }
    }
}

/// Verify-and-store outcome (the Enter-key action). `stored` is true only when
/// the secret verified AND was written to the Keychain.
#[derive(Debug, Serialize)]
struct StoreResult {
    stored: bool,
    status: String,
    detail: String,
}

/// Map an HTTP status + the github login (when 200) to a typed result. PURE —
/// unit-tested without the network.
fn classify_github(status: u16, login: Option<&str>) -> VerifyResult {
    match status {
        200 => VerifyResult::valid(login.unwrap_or("authenticated").to_string()),
        401 | 403 => VerifyResult::unauthorized("token rejected"),
        other => VerifyResult::network(format!("unexpected http {other}")),
    }
}

/// Map an HTTP status (anthropic /v1/models) to a typed result. PURE.
fn classify_anthropic(status: u16) -> VerifyResult {
    match status {
        200 => VerifyResult::valid("models reachable"),
        401 | 403 => VerifyResult::unauthorized("key rejected"),
        other => VerifyResult::network(format!("unexpected http {other}")),
    }
}

/// Map a parsed Slack auth.test body to a typed result. PURE.
fn classify_slack(ok: bool, team: Option<&str>, error: Option<&str>) -> VerifyResult {
    if ok {
        VerifyResult::valid(team.unwrap_or("authenticated").to_string())
    } else {
        VerifyResult::unauthorized(error.unwrap_or("auth_failed").to_string())
    }
}

/// A connection-STATUS (OAuth) row is not paste-verifiable: its secret (the
/// refresh token) is minted by the daemon's browser consent flow, never typed
/// here. Typing into it is a no-op that names the daemon's connect flow. PURE.
/// `provider` is a public platform name (never secret material).
fn oauth_connect_via_daemon(provider: &str) -> VerifyResult {
    VerifyResult::unauthorized(format!(
        "connect {provider} in the daemon — there is nothing to paste here"
    ))
}

/// The provider name for an OAuth connection-STATUS id, used only for guidance
/// copy (the platform's public name — never secret material).
fn oauth_provider_for(id: &str) -> &'static str {
    match id {
        "x_social" => "X",
        "linkedin_social" => "LinkedIn",
        "google_ads" => "Google Ads",
        "meta_ads" => "Meta Ads",
        "whoop" => "WHOOP",
        // google_workspace and any future OAuth row default to Google's prior copy.
        _ => "Google",
    }
}

/// Format/sanity check for a pasted Google OAuth client id or secret. These
/// CANNOT be verified by a bare HTTP call — verification only happens through
/// the full browser consent flow — so on Enter we accept a well-SHAPED value and
/// store it, returning a "stored, connect to finish" result. A client id must
/// look like a Google installed-app client (`*.apps.googleusercontent.com`); a
/// secret only needs to be non-trivially non-empty. PURE — unit-tested. The
/// secret is NEVER echoed in the detail.
fn classify_google_client(id: &str, secret: &str) -> VerifyResult {
    let secret = secret.trim();
    match id {
        "google_client_id" => {
            if secret.ends_with(".apps.googleusercontent.com") && secret.len() > ".apps.googleusercontent.com".len() {
                VerifyResult::valid("client id saved — Connect Google to finish")
            } else {
                VerifyResult::unauthorized(
                    "expected a Desktop-app client id ending in .apps.googleusercontent.com",
                )
            }
        }
        "google_client_secret" => {
            // Google client secrets are short opaque strings; we only assert a
            // plausible minimum length (never the value).
            if secret.len() >= 8 {
                VerifyResult::valid("client secret saved — Connect Google to finish")
            } else {
                VerifyResult::unauthorized("that does not look like a client secret")
            }
        }
        _ => VerifyResult::network("no verifier for this credential"),
    }
}

/// Format/sanity check for a pasted X / LinkedIn OAuth client id or secret.
/// Unlike Google's installed-app client id, these are opaque strings with no
/// stable suffix, so we only assert a plausible non-trivial length (never the
/// value) — the only real proof is the daemon's browser consent. On a
/// well-SHAPED value we accept + store, returning a "stored, connect to finish"
/// result that names the daemon's connect flow. PURE — unit-tested. The secret
/// is NEVER echoed in the detail. `provider` is the public platform name.
fn classify_oauth_client(provider: &str, field: &str, value: &str) -> VerifyResult {
    let value = value.trim();
    if value.len() >= 8 {
        VerifyResult::valid(format!("{field} saved — connect {provider} to finish"))
    } else {
        VerifyResult::unauthorized(format!("that does not look like a {provider} {field}"))
    }
}

/// Format/sanity check for a Google Ads customer id (operating OR login). Google
/// Ads customer ids are 10-digit numbers (often shown dash-grouped like
/// 123-456-7890); we accept a value that is all digits once dashes/spaces are
/// stripped and is 10 digits long. PURE — unit-tested. The value is never echoed.
fn classify_customer_id(provider: &str, value: &str) -> VerifyResult {
    let digits: String = value.chars().filter(|c| !matches!(c, '-' | ' ')).collect();
    if digits.len() == 10 && digits.chars().all(|c| c.is_ascii_digit()) {
        VerifyResult::valid(format!("customer id saved — connect {provider} to finish"))
    } else {
        VerifyResult::unauthorized("expected a 10-digit Google Ads customer id (digits only)")
    }
}

/// Format/sanity check for a Meta ad account id. Meta ad account ids are the
/// `act_` prefix followed by the numeric account id (e.g. `act_1234567890`). PURE
/// — unit-tested. The value is never echoed.
fn classify_meta_ad_account(value: &str) -> VerifyResult {
    let value = value.trim();
    if let Some(digits) = value.strip_prefix("act_") {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return VerifyResult::valid("ad account id saved — connect Meta Ads to finish");
        }
    }
    VerifyResult::unauthorized("expected a Meta ad account id like act_1234567890")
}

/// Format/sanity check for a Home Assistant base URL. We do NOT verify it over
/// HTTP — that would reach into the user's own LAN — so we only assert a plausible
/// `http(s)://host` shape and store on success. PURE — unit-tested. The value is
/// never echoed; the success line names DUM-E's local-bridge model honestly.
fn classify_home_assistant_url(value: &str) -> VerifyResult {
    let value = value.trim();
    let has_scheme = value.starts_with("http://") || value.starts_with("https://");
    // Require something after the scheme (a host), not a bare "https://".
    let has_host = value.len() > "https://".len();
    if has_scheme && has_host {
        VerifyResult::valid("Home Assistant URL saved — add your long-lived token to finish")
    } else {
        VerifyResult::unauthorized("expected a Home Assistant URL like http://homeassistant.local:8123")
    }
}

/// Format/sanity check for a pasted Plaid access_token. We do NOT verify it over
/// HTTP — that would call Plaid with the user's credentials — so we only assert a
/// plausible shape. Plaid Link access tokens look like `access-<env>-<uuid>` (e.g.
/// `access-sandbox-…`); we accept a value with the `access-` prefix and some body
/// after it, and store on success. PURE — unit-tested. The value is never echoed;
/// the success line names that MIDAS reads only. (The client_id/secret are opaque
/// strings handled by `classify_oauth_client`.)
fn classify_plaid_access_token(value: &str) -> VerifyResult {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("access-") {
        if rest.len() >= 4 {
            return VerifyResult::valid("Plaid access token saved — MIDAS reads only, it never moves money");
        }
    }
    VerifyResult::unauthorized("expected a Plaid Link access token like access-sandbox-…")
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(API_TIMEOUT)
        .build()
        .map_err(|e| format!("http client init failed: {e}"))
}

/// Live verification dispatched per credential id (reqwest, 10s timeout). The
/// secret is sent ONLY in the appropriate auth header and never logged.
async fn verify_dispatch(id: &str, secret: &str) -> Result<VerifyResult, String> {
    let cred = credential_by_id(id).ok_or_else(|| format!("unknown credential id: {id}"))?;

    if cred.kind == Kind::OAuth {
        return Ok(oauth_connect_via_daemon(oauth_provider_for(id)));
    }

    // The Google client id/secret are format-checked locally and stored on
    // success; they are NOT verifiable over HTTP (only the consent flow proves
    // them). No network is touched for these ids.
    if matches!(id, "google_client_id" | "google_client_secret") {
        return Ok(classify_google_client(id, secret));
    }

    // X / LinkedIn client id/secret are likewise format-checked locally and
    // stored on success — only the daemon's consent flow can prove them, so no
    // network is touched. They are opaque strings (no stable suffix), so the
    // check is a plausible-length one (never the value).
    match id {
        "x_client_id" => return Ok(classify_oauth_client("X", "client id", secret)),
        "x_client_secret" => return Ok(classify_oauth_client("X", "client secret", secret)),
        "linkedin_client_id" => {
            return Ok(classify_oauth_client("LinkedIn", "client id", secret))
        }
        "linkedin_client_secret" => {
            return Ok(classify_oauth_client("LinkedIn", "client secret", secret))
        }
        _ => {}
    }

    // Google Ads + Meta Ads pasted values are likewise format-checked locally and
    // stored on success — none are verifiable by a bare HTTP call (only the
    // daemon's consent flow + a real Ads call prove them), so no network is
    // touched. The Google Ads client id reuses Google's installed-app shape; the
    // rest are plausible-length / shape checks that never echo the value.
    match id {
        "google_ads_client_id" => {
            // Reuse Google's installed-app client-id shape, then word the success
            // line for the Ads connect flow.
            let value = secret.trim();
            return Ok(
                if value.ends_with(".apps.googleusercontent.com")
                    && value.len() > ".apps.googleusercontent.com".len()
                {
                    VerifyResult::valid("client id saved — connect Google Ads to finish")
                } else {
                    VerifyResult::unauthorized(
                        "expected a Desktop-app client id ending in .apps.googleusercontent.com",
                    )
                },
            );
        }
        "google_ads_client_secret" => {
            return Ok(classify_oauth_client("Google Ads", "client secret", secret))
        }
        "google_ads_developer_token" => {
            return Ok(classify_oauth_client("Google Ads", "developer token", secret))
        }
        "google_ads_customer_id" => return Ok(classify_customer_id("Google Ads", secret)),
        "google_ads_login_customer_id" => {
            return Ok(classify_customer_id("Google Ads", secret))
        }
        "meta_app_id" => return Ok(classify_oauth_client("Meta Ads", "app id", secret)),
        "meta_app_secret" => return Ok(classify_oauth_client("Meta Ads", "app secret", secret)),
        "meta_ad_account_id" => return Ok(classify_meta_ad_account(secret)),
        // WHOOP client id/secret (from the user's own WHOOP developer app) are
        // likewise format-checked locally and stored on success — only the
        // daemon's consent flow proves them, so no network is touched. They are
        // opaque strings, so the check is a plausible-length one (never the value).
        "whoop_client_id" => return Ok(classify_oauth_client("WHOOP", "client id", secret)),
        "whoop_client_secret" => {
            return Ok(classify_oauth_client("WHOOP", "client secret", secret))
        }
        // Home Assistant (dume, Home & Environment) is a TOKEN-based LOCAL bridge —
        // both values are format-checked locally and stored on success, with NO
        // network touched: the URL is the user's own hub on their LAN, and a bare
        // HTTP probe of it from here would reach into that network. The URL is shape-
        // checked (http(s)://host); the long-lived token is an opaque string, so the
        // check is a plausible-length one (never the value). The real proof is dume's
        // first hub read at runtime.
        "homeassistant_url" => return Ok(classify_home_assistant_url(secret)),
        "homeassistant_token" => {
            return Ok(classify_oauth_client("Home Assistant", "token", secret))
        }
        // Plaid (midas, Personal Treasury) is a TOKEN-based finance READER — all
        // three values are format-checked locally and stored on success, with NO
        // network touched: a bare probe would call Plaid with the user's own
        // credentials. The client_id/secret are opaque strings (plausible-length
        // check via classify_oauth_client); the access_token is shape-checked for the
        // Plaid Link `access-…` prefix (Plaid Link runs in the user's own frontend,
        // not JARVIS). The real proof is midas's first balance read at runtime.
        // MIDAS reads only — none of these enables any money movement.
        "plaid_client_id" => return Ok(classify_oauth_client("Plaid", "client id", secret)),
        "plaid_secret" => return Ok(classify_oauth_client("Plaid", "secret", secret)),
        "plaid_access_token" => return Ok(classify_plaid_access_token(secret)),
        // Maps (voyager, Travel & Logistics) is a KEY-based maps provider — the
        // single API key is format-checked locally and stored on success, with NO
        // network touched: a bare probe would call the maps provider with the user's
        // own key (and the key must NOT ride a logged URL). The key is an opaque
        // string, so the check is a plausible-length one (never the value), reusing
        // classify_oauth_client. The real proof is voyager's first route read at
        // runtime. READ-ONLY — the key enables routes/places/times, never booking.
        "maps_api_key" => return Ok(classify_oauth_client("Google Maps", "API key", secret)),
        // Have I Been Pwned (aegis, Defense & Privacy) is a KEY-based breach checker —
        // the single API key is format-checked locally and stored on success, with NO
        // network touched: a bare probe would call HIBP with the user's own key (and
        // the key must NOT ride a logged URL). The key is an opaque string, so the
        // check is a plausible-length one (never the value), reusing
        // classify_oauth_client. The real proof is aegis's first breach read at
        // runtime. DEFENSIVE + READ-ONLY — the key enables a breach check on the
        // user's OWN email, never offensive scanning.
        "hibp_api_key" => return Ok(classify_oauth_client("Have I Been Pwned", "API key", secret)),
        // ElevenLabs cloud VOICE TIER — the single API key is format-checked locally
        // and stored on success, with NO network touched: an HTTP probe would make a
        // cloud TTS call with the user's own key (and the key must NOT ride a logged
        // URL — it belongs in the xi-api-key header). The key is an opaque string, so
        // the check is a plausible-length one (never the value), reusing
        // classify_oauth_client. HONESTY: storing the key does NOT turn the tier on —
        // [voice].cloud_tier ships OFF; the operator flips it on separately, and
        // on-device Kokoro stays the private/offline default + fallback. The real
        // proof is the daemon's first ElevenLabs synthesis at runtime (device + key
        // gated; not verified here).
        "elevenlabs_api_key" => {
            return Ok(classify_oauth_client("ElevenLabs", "API key", secret))
        }
        _ => {}
    }

    let client = http_client()?;

    match id {
        "anthropic" => {
            let resp = client
                .get("https://api.anthropic.com/v1/models")
                .header("x-api-key", secret)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await;
            Ok(match resp {
                Ok(r) => classify_anthropic(r.status().as_u16()),
                Err(e) => VerifyResult::network(net_detail(&e)),
            })
        }
        "github" => {
            let resp = client
                .get("https://api.github.com/user")
                .header("Authorization", format!("Bearer {secret}"))
                .header("User-Agent", "jarvis-hud")
                .header("Accept", "application/vnd.github+json")
                .send()
                .await;
            Ok(match resp {
                Ok(r) => {
                    let status = r.status().as_u16();
                    if status == 200 {
                        let login = r
                            .json::<serde_json::Value>()
                            .await
                            .ok()
                            .and_then(|v| v.get("login").and_then(|l| l.as_str()).map(String::from));
                        classify_github(status, login.as_deref())
                    } else {
                        classify_github(status, None)
                    }
                }
                Err(e) => VerifyResult::network(net_detail(&e)),
            })
        }
        "slack" => {
            let resp = client
                .post("https://slack.com/api/auth.test")
                .header("Authorization", format!("Bearer {secret}"))
                .send()
                .await;
            Ok(match resp {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(v) => {
                        let ok = v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
                        let team = v.get("team").and_then(|t| t.as_str());
                        let error = v.get("error").and_then(|e| e.as_str());
                        classify_slack(ok, team, error)
                    }
                    Err(_) => VerifyResult::network("malformed slack response"),
                },
                Err(e) => VerifyResult::network(net_detail(&e)),
            })
        }
        // Any other bearer id without a handler is treated as not reachable.
        _ => Ok(VerifyResult::network("no verifier for this credential")),
    }
}

/// Short network failure label (never includes secret material).
fn net_detail(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".to_string()
    } else if e.is_connect() {
        "unreachable".to_string()
    } else {
        "request failed".to_string()
    }
}

/// Verify a candidate secret for `id` without storing it.
#[tauri::command]
async fn verify_credential(id: String, secret: String) -> Result<VerifyResult, String> {
    let secret = secret.trim().to_string();
    verify_dispatch(&id, &secret).await
}

/// Verify a candidate secret and, only if valid, persist it to the Keychain
/// under the registered account. The Enter-key action. An unverified secret is
/// NEVER stored.
#[tauri::command]
async fn verify_and_store(id: String, secret: String) -> Result<StoreResult, String> {
    let secret = secret.trim().to_string();
    let account = account_for_id(&id).ok_or_else(|| format!("unknown credential id: {id}"))?;

    let result = verify_dispatch(&id, &secret).await?;
    let stored = if result.status == "valid" {
        keychain_set_internal(account, secret).await?;
        true
    } else {
        false
    };

    Ok(StoreResult { stored, status: result.status, detail: result.detail })
}

/* ------------------------------------------------------- Google connect (OAuth) */

/// Outcome of asking the HUD to connect Google. The browser consent + loopback
/// run inside the DAEMON at runtime (it opens the URL, binds the loopback, mints
/// + stores the refresh token); the HUD has no daemon handle in a pure build, so
/// this command is an honest stub that tells the user where the flow lives. It
/// reports whether both client halves are on file, and whether a refresh token
/// already exists (the only real signal of "connected").
#[derive(Debug, Serialize)]
struct ConnectResult {
    /// True only when client id AND secret are stored — Connect is meaningful.
    ready: bool,
    /// True only when a refresh token is actually on file (truly connected).
    connected: bool,
    /// Human guidance — never contains secret material.
    detail: String,
}

/// Begin the Google connect flow. In a pure HUD build (no daemon socket) this
/// does NOT fake a connection: it checks prerequisites in the Keychain and
/// returns guidance to run the daemon's own consent flow. We never claim
/// "connected" unless a refresh token is genuinely present.
#[tauri::command]
async fn begin_google_auth() -> Result<ConnectResult, String> {
    // The runtime consent flow lives in the daemon's `connect_google` tool:
    // saying "connect Google" to jarvisd opens the browser consent page, runs
    // the loopback, and stores the refresh token.
    begin_oauth_connect(
        "Google",
        "connect Google",
        "google_oauth_client_id",
        "google_oauth_client_secret",
        "google_oauth_refresh_token",
    )
    .await
}

/// Shared connect-prerequisite check for a daemon-run OAuth provider. Like
/// `begin_google_auth`, this NEVER fakes a connection: it reports whether both
/// client halves are on file and whether a refresh token is genuinely present,
/// and returns guidance pointing at the daemon's own consent flow. `provider`
/// is the public platform name; `connect_phrase` is the exact thing the user
/// says to jarvisd (e.g. "connect X"). No secret material is ever read or
/// logged — only presence bools.
async fn begin_oauth_connect(
    provider: &str,
    connect_phrase: &str,
    id_account: &'static str,
    secret_account: &'static str,
    refresh_account: &'static str,
) -> Result<ConnectResult, String> {
    let has_id = keychain_present(id_account).await?;
    let has_secret = keychain_present(secret_account).await?;
    let connected = keychain_present(refresh_account).await?;

    let detail = if connected {
        format!("{provider} is connected (refresh token on file).")
    } else if has_id && has_secret {
        format!(
            "Client id and secret are on file. Start jarvisd and say '{connect_phrase}' — it opens the browser to finish."
        )
    } else {
        "Paste the OAuth client id and secret first, then Connect.".to_string()
    };

    Ok(ConnectResult { ready: has_id && has_secret, connected, detail })
}

/// Begin the X (Twitter) connect flow. Honest stub: the browser consent runs in
/// the daemon's `connect_x` tool — this only checks prerequisites + presence.
#[tauri::command]
async fn begin_x_auth() -> Result<ConnectResult, String> {
    begin_oauth_connect(
        "X",
        "connect X",
        "x_oauth_client_id",
        "x_oauth_client_secret",
        "x_oauth_refresh_token",
    )
    .await
}

/// Begin the LinkedIn connect flow. Honest stub: the browser consent runs in the
/// daemon's `connect_linkedin` tool — this only checks prerequisites + presence.
#[tauri::command]
async fn begin_linkedin_auth() -> Result<ConnectResult, String> {
    begin_oauth_connect(
        "LinkedIn",
        "connect LinkedIn",
        "linkedin_oauth_client_id",
        "linkedin_oauth_client_secret",
        "linkedin_oauth_refresh_token",
    )
    .await
}

/// Begin the Google Ads connect flow. Honest stub: the browser consent runs in
/// the daemon's `connect_google_ads` tool — this only checks the OAuth client
/// prerequisites + the (separate-from-Workspace) refresh-token presence. The
/// developer token + customer id are needed for actual Ads calls, not the connect
/// step, so they are not part of this readiness check.
#[tauri::command]
async fn begin_google_ads_auth() -> Result<ConnectResult, String> {
    begin_oauth_connect(
        "Google Ads",
        "connect Google Ads",
        "google_ads_client_id",
        "google_ads_client_secret",
        "google_ads_refresh_token",
    )
    .await
}

/// Begin the Meta Ads connect flow. Honest stub: the browser consent (and the
/// short->long token exchange) runs in the daemon's `connect_meta_ads` tool — this
/// only checks the app-credential prerequisites + the long-lived-token presence
/// (Meta has no refresh token, so the long-lived token IS the "connected" signal).
#[tauri::command]
async fn begin_meta_ads_auth() -> Result<ConnectResult, String> {
    begin_oauth_connect(
        "Meta Ads",
        "connect Meta",
        "meta_app_id",
        "meta_app_secret",
        "meta_long_lived_token",
    )
    .await
}

/// Begin the WHOOP connect flow. Honest stub: the browser consent + loopback run
/// in the daemon's `connect_whoop` tool — this only checks the OAuth client
/// prerequisites + the refresh-token presence. Never fakes a connection.
#[tauri::command]
async fn begin_whoop_auth() -> Result<ConnectResult, String> {
    begin_oauth_connect(
        "WHOOP",
        "connect WHOOP",
        "whoop_oauth_client_id",
        "whoop_oauth_client_secret",
        "whoop_oauth_refresh_token",
    )
    .await
}

/// Presence check for an already-validated account (internal; returns a bool).
async fn keychain_present(account: &'static str) -> Result<bool, String> {
    run_keychain(move || match get_generic_password(SERVICE, account) {
        Ok(_) => Ok(true),
        Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
        Err(e) => Err(e),
    })
    .await
}

/* ------------------------------------------------- fullscreen kiosk takeover */

/// Drive ONE planned takeover step against the calling window. `on` means "apply
/// the takeover form" of the mutation; the per-mutation boolean each Tauri setter
/// wants is derived here. The macOS presentation-options step is DEVICE-GATED and
/// only does real work on macOS (a no-op elsewhere). Returns the setter error
/// string on failure so the caller can abort + roll back.
fn drive_step(window: &WebviewWindow, mutation: Mutation, on: bool) -> Result<(), String> {
    let label = |r: tauri::Result<()>| r.map_err(|e| e.to_string());
    match mutation {
        // Fullscreen on == true.
        Mutation::Fullscreen => label(window.set_fullscreen(on)),
        // Decorations are HIDDEN under takeover, so "on" => set_decorations(false).
        Mutation::Decorations => label(window.set_decorations(!on)),
        // Always-on-top on == true.
        Mutation::AlwaysOnTop => label(window.set_always_on_top(on)),
        // macOS hide Dock + menu bar on == true; default (visible) on exit.
        Mutation::PresentationOptions => takeover::macos_set_kiosk_presentation(on),
    }
}

/// ENTER the fullscreen kiosk takeover for the calling window. EXPLICIT user
/// action only — nothing calls this automatically and the app ships windowed.
/// Idempotent: a second enter while already active is a clean no-op. On ANY step
/// failure it rolls back the steps already applied (best effort) and stays OUT of
/// takeover, so a partial enter never strands the desktop.
#[tauri::command]
async fn enter_takeover(
    window: WebviewWindow,
    takeover: tauri::State<'_, Takeover>,
) -> Result<bool, String> {
    let plan = {
        let guard = takeover.state.lock().map_err(|_| "takeover state poisoned".to_string())?;
        guard.plan_enter()
    };
    if plan.is_empty() {
        return Ok(true); // already active — idempotent.
    }

    let mut applied: Vec<Mutation> = Vec::new();
    for step in &plan {
        if let Err(e) = drive_step(&window, step.mutation, step.on) {
            // Roll back what we applied, in reverse, then report the failure. The
            // reset-on-exit net + macOS auto-restore still backstop the worst case.
            for m in applied.iter().rev() {
                let _ = drive_step(&window, *m, false);
            }
            let _ = reset_presentation_to_default();
            return Err(format!("enter_takeover failed at {:?}: {e}", step.mutation));
        }
        applied.push(step.mutation);
    }

    let mut guard = takeover.state.lock().map_err(|_| "takeover state poisoned".to_string())?;
    guard.commit_enter();
    Ok(true)
}

/// EXIT the takeover for the calling window — the BULLETPROOF reverse. Reverses
/// EVERY recorded mutation in inverse order, then restores the default macOS
/// presentation options unconditionally as a belt-and-suspenders net. Idempotent
/// and TOTAL: calling it when not in takeover is a clean no-op, and it always
/// leaves the desktop fully restored. This is what the Esc handler and the in-HUD
/// EXIT control both ultimately call (and what an OS-level global shortcut would
/// call if the global-shortcut plugin is later added — it is NOT a dependency
/// today, so that exit does not yet exist).
#[tauri::command]
async fn exit_takeover(
    window: WebviewWindow,
    takeover: tauri::State<'_, Takeover>,
) -> Result<bool, String> {
    let plan = {
        let guard = takeover.state.lock().map_err(|_| "takeover state poisoned".to_string())?;
        guard.plan_exit()
    };

    // Reverse every recorded mutation. We do NOT abort on a single setter error —
    // exit must try EVERY reversal so the user is never left locked in.
    let mut first_err: Option<String> = None;
    for step in &plan {
        if let Err(e) = drive_step(&window, step.mutation, step.on) {
            first_err.get_or_insert(format!("exit_takeover: {:?}: {e}", step.mutation));
        }
    }
    // Belt-and-suspenders: always restore the default presentation options even if
    // the plan was empty or a step failed — the Dock/menu bar MUST come back.
    reset_presentation_to_default();

    let mut guard = takeover.state.lock().map_err(|_| "takeover state poisoned".to_string())?;
    guard.commit_exit();

    match first_err {
        Some(e) => Err(e),
        None => Ok(true),
    }
}

/// The id of the custom "About J.A.R.V.I.S." menu item, shared by the menu
/// construction and the menu-event handler so they can never drift.
const ABOUT_MENU_ID: &str = "about_jarvis";
/// The event the About menu item emits to the webview (payload = app version).
/// Must match `ABOUT_MENU_EVENT` in `hud/src/tauri/bridge.ts`.
const ABOUT_MENU_EVENT: &str = "menu://about";

/// Build the macOS app menu. We REPLACE Tauri's default menu so the
/// "About J.A.R.V.I.S." item opens our CUSTOM About panel (which carries a
/// working "Check for Updates" button + the credit) instead of the system about
/// panel. Everything else is reconstructed from PREDEFINED items so the standard
/// behavior is preserved — in particular the Edit menu keeps Cut/Copy/Paste/
/// Select-All working (the credential paste-boxes depend on it), and Hide/
/// Services/Quit/Window keep their conventional shortcuts.
fn build_app_menu<R: tauri::Runtime>(handle: &tauri::AppHandle<R>) -> tauri::Result<Menu<R>> {
    let about = MenuItem::with_id(handle, ABOUT_MENU_ID, "About J.A.R.V.I.S.", true, None::<&str>)?;

    let app_menu = Submenu::with_items(
        handle,
        "J.A.R.V.I.S.",
        true,
        &[
            &about,
            &PredefinedMenuItem::separator(handle)?,
            &PredefinedMenuItem::services(handle, None)?,
            &PredefinedMenuItem::separator(handle)?,
            &PredefinedMenuItem::hide(handle, None)?,
            &PredefinedMenuItem::hide_others(handle, None)?,
            &PredefinedMenuItem::show_all(handle, None)?,
            &PredefinedMenuItem::separator(handle)?,
            &PredefinedMenuItem::quit(handle, None)?,
        ],
    )?;

    // Edit — REQUIRED for Cut/Copy/Paste in the credential paste-boxes.
    let edit_menu = Submenu::with_items(
        handle,
        "Edit",
        true,
        &[
            &PredefinedMenuItem::undo(handle, None)?,
            &PredefinedMenuItem::redo(handle, None)?,
            &PredefinedMenuItem::separator(handle)?,
            &PredefinedMenuItem::cut(handle, None)?,
            &PredefinedMenuItem::copy(handle, None)?,
            &PredefinedMenuItem::paste(handle, None)?,
            &PredefinedMenuItem::select_all(handle, None)?,
        ],
    )?;

    let window_menu = Submenu::with_items(
        handle,
        "Window",
        true,
        &[
            &PredefinedMenuItem::minimize(handle, None)?,
            &PredefinedMenuItem::maximize(handle, None)?,
            &PredefinedMenuItem::separator(handle)?,
            &PredefinedMenuItem::close_window(handle, None)?,
        ],
    )?;

    Menu::with_items(handle, &[&app_menu, &edit_menu, &window_menu])
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        // Custom macOS menu: the "About J.A.R.V.I.S." item opens our custom About
        // panel (emits ABOUT_MENU_EVENT to the webview) instead of the system
        // about panel; everything else is the standard predefined menu.
        .menu(|handle| build_app_menu(handle))
        // When About is picked, emit the app version to the webview, which mounts
        // the custom About panel. No other menu id is consequential here.
        .on_menu_event(|handle, event| {
            if event.id() == ABOUT_MENU_ID {
                let version = handle.package_info().version.to_string();
                let _ = handle.emit(ABOUT_MENU_EVENT, version);
            }
        })
        // WS4a auto-updater. The plugin reads its endpoint + the OWNER's PUBLIC
        // updater key from tauri.conf.json (plugins.updater). It performs NO work
        // on its own — it only exposes the update API the `check_for_updates`
        // command drives, and that command no-ops cleanly while the pubkey is the
        // committed PLACEHOLDER / no real release is published (see updates.rs).
        // The matching PRIVATE key lives ONLY in CI secrets, never in this repo.
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Explicit, idempotent takeover state shared by the enter/exit commands
        // and the reset-on-exit safety net. Ships INACTIVE (Default).
        .manage(Takeover::default())
        // In-process microphone capture state (mic_stream.rs). Ships idle; the
        // start_mic_stream command (and the setup auto-start below) only opens
        // the mic AFTER a successful connect to the daemon's app-audio socket,
        // so this no-ops cleanly when the daemon isn't in app-mode.
        .manage(mic_stream::MicState::default())
        // AUTO-START the mic→daemon stream once on launch. It is a clean no-op
        // when the daemon isn't listening on its app-audio socket (the connect
        // fails before the mic is ever opened, so no prompt fires), so this is
        // safe to fire unconditionally. Run it off the main thread so a slow
        // socket connect / device query never blocks app startup.
        .setup(|app| {
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                use tauri::Manager;
                let state = handle.state::<mic_stream::MicState>();
                let _ = mic_stream::start_mic_stream(state);
            });
            // START the UI ACTUATOR listener (actuator.rs) on its own background
            // thread. It binds <root>/state/ipc/actuate.sock and serves ONE
            // token-authenticated actuation request per connection, posting the
            // CGEvent IN THIS app process so macOS shows a clean "JARVIS would like
            // to control this computer" Accessibility prompt. NON-BLOCKING and a
            // clean no-op when the daemon hasn't created state/ipc/ yet; it NEVER
            // actuates on its own — only on a token-verified daemon request. Kept
            // entirely off the async runtime / managed state (no !Send type).
            actuator::start_actuator_listener();
            Ok(())
        })
        // RESET-ON-EXIT safety net (window side): when the main window is
        // destroyed, restore the default macOS presentation options so a window
        // close can never leave the Dock/menu bar hidden.
        .on_window_event(|_window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                reset_presentation_to_default();
            }
        })
        .invoke_handler(tauri::generate_handler![
            keychain_status,
            keychain_set,
            keychain_delete,
            verify_credential,
            verify_and_store,
            begin_google_auth,
            begin_x_auth,
            begin_linkedin_auth,
            begin_google_ads_auth,
            begin_meta_ads_auth,
            begin_whoop_auth,
            heal::heal_proposal_detail,
            heal::heal_apply,
            command::send_command,
            command::play_sfx_cue,
            command::design_voice,
            command::create_pronunciation,
            command::compose_music,
            mic_stream::start_mic_stream,
            mic_stream::stop_mic_stream,
            config_settings::config_get,
            config_settings::config_set,
            config_settings::daemon_restart,
            config_settings::pick_folder,
            updates::check_for_updates,
            updates::relaunch_app,
            uninstall::uninstall_open,
            setup::backend_installed,
            setup::open_setup_install,
            permissions::open_privacy_pane,
            permissions::request_all_permissions,
            permissions::request_access,
            enter_takeover,
            exit_takeover
        ])
        .build(tauri::generate_context!())
        .expect("error while building the JARVIS HUD");

    // RESET-ON-EXIT safety net (app side): on the event loop exiting (Cmd+Q /
    // quit), restore the default macOS presentation options. macOS ALSO
    // auto-restores presentation options when the process dies, so even a hard
    // crash/force-quit can never permanently hide the Dock or menu bar — this
    // makes the clean-quit path explicit too.
    app.run(|_app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            reset_presentation_to_default();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_menu_contract_matches_the_frontend() {
        // These two strings are the contract with hud/src/tauri/bridge.ts: the
        // menu item id the handler matches on, and the event the webview listens
        // for. If either drifts, the About menu click stops opening the panel.
        assert_eq!(ABOUT_MENU_ID, "about_jarvis");
        assert_eq!(ABOUT_MENU_EVENT, "menu://about");
    }

    #[test]
    fn unknown_account_is_rejected_before_any_keychain_op() {
        assert!(guard_account("anthropic_api_key").is_ok());
        assert!(guard_account("github_pat").is_ok());
        assert!(guard_account("login_keychain").is_err());
        assert!(guard_account("").is_err());
        assert!(guard_account("anthropic_api_key\0x").is_err());
    }

    #[test]
    fn id_to_account_resolves_via_registry() {
        assert_eq!(account_for_id("anthropic"), Some("anthropic_api_key"));
        assert_eq!(account_for_id("slack"), Some("slack_bot_token"));
        assert_eq!(account_for_id("bogus"), None);
    }

    #[test]
    fn anthropic_dispatch_shape() {
        assert_eq!(classify_anthropic(200), VerifyResult::valid("models reachable"));
        assert_eq!(classify_anthropic(401), VerifyResult::unauthorized("key rejected"));
        assert_eq!(classify_anthropic(403), VerifyResult::unauthorized("key rejected"));
        assert_eq!(classify_anthropic(500).status, "network_error");
    }

    #[test]
    fn github_dispatch_shape() {
        // 200 -> detail is the login field.
        assert_eq!(classify_github(200, Some("octocat")), VerifyResult::valid("octocat"));
        assert_eq!(classify_github(200, None), VerifyResult::valid("authenticated"));
        assert_eq!(classify_github(401, None), VerifyResult::unauthorized("token rejected"));
        assert_eq!(classify_github(403, None), VerifyResult::unauthorized("token rejected"));
        assert_eq!(classify_github(502, None).status, "network_error");
    }

    #[test]
    fn slack_dispatch_shape() {
        // ok==true -> valid, detail = team.
        assert_eq!(classify_slack(true, Some("Acme"), None), VerifyResult::valid("Acme"));
        // ok==false -> unauthorized, detail = error.
        assert_eq!(
            classify_slack(false, None, Some("invalid_auth")),
            VerifyResult::unauthorized("invalid_auth")
        );
        assert_eq!(
            classify_slack(false, None, None),
            VerifyResult::unauthorized("auth_failed")
        );
    }

    #[test]
    fn workspace_status_row_is_not_paste_verifiable() {
        let r = oauth_connect_via_daemon("Google");
        assert_eq!(r.status, "unauthorized");
        assert!(r.detail.contains("connect Google"));
        // The detail must never leak any token-shaped material — it is guidance.
        assert!(!r.detail.contains("token"));
    }

    #[test]
    fn oauth_provider_names_map_for_status_rows() {
        assert_eq!(oauth_provider_for("google_workspace"), "Google");
        assert_eq!(oauth_provider_for("x_social"), "X");
        assert_eq!(oauth_provider_for("linkedin_social"), "LinkedIn");
        // The connect copy names the right platform.
        assert!(oauth_connect_via_daemon(oauth_provider_for("x_social"))
            .detail
            .contains("connect X"));
        assert!(oauth_connect_via_daemon(oauth_provider_for("linkedin_social"))
            .detail
            .contains("connect LinkedIn"));
    }

    #[test]
    fn oauth_client_classifier_valid_and_empty() {
        // A plausible-length opaque value is accepted (stored, connect to finish).
        let ok = classify_oauth_client("X", "client id", "abc12345xyz");
        assert_eq!(ok.status, "valid");
        assert!(ok.detail.contains("connect X"));
        let ok2 = classify_oauth_client("LinkedIn", "client secret", "WPL_AP1.abcdef");
        assert_eq!(ok2.status, "valid");
        assert!(ok2.detail.contains("connect LinkedIn"));
        // Empty / whitespace-only / too-short are rejected (won't be stored).
        assert_eq!(
            classify_oauth_client("X", "client secret", "").status,
            "unauthorized"
        );
        assert_eq!(
            classify_oauth_client("X", "client secret", "      ").status,
            "unauthorized"
        );
        assert_eq!(
            classify_oauth_client("LinkedIn", "client id", "short").status,
            "unauthorized"
        );
    }

    #[test]
    fn oauth_client_classifier_never_echoes_the_secret() {
        let secret = "super-secret-x-client-value-do-not-leak";
        let r = classify_oauth_client("X", "client secret", secret);
        assert!(!r.detail.contains(secret));
        assert!(!r.detail.contains("super-secret"));
    }

    #[tokio::test]
    async fn social_status_rows_short_circuit_without_network() {
        // The X / LinkedIn OAuth status ids must NOT hit the network.
        let x = verify_dispatch("x_social", "anything").await.unwrap();
        assert_eq!(x.status, "unauthorized");
        assert!(x.detail.contains("connect X"));
        let li = verify_dispatch("linkedin_social", "anything").await.unwrap();
        assert_eq!(li.status, "unauthorized");
        assert!(li.detail.contains("connect LinkedIn"));
    }

    #[tokio::test]
    async fn social_client_dispatch_is_local_only() {
        // The X / LinkedIn client id/secret ids are format-checked locally, never
        // over HTTP.
        assert_eq!(
            verify_dispatch("x_client_id", "abc12345xyz")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("x_client_secret", "no")
                .await
                .unwrap()
                .status,
            "unauthorized"
        );
        assert_eq!(
            verify_dispatch("linkedin_client_id", "78xa9bcd2ef")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("linkedin_client_secret", "")
                .await
                .unwrap()
                .status,
            "unauthorized"
        );
    }

    #[test]
    fn google_client_classifier_valid_shape() {
        // A Desktop-app client id has the canonical googleusercontent suffix.
        let r = classify_google_client(
            "google_client_id",
            "123-abc.apps.googleusercontent.com",
        );
        assert_eq!(r.status, "valid");
        assert!(r.detail.contains("Connect Google"));
        // A plausible-length secret is accepted (stored, connect to finish).
        let s = classify_google_client("google_client_secret", "GOCSPX-abcdefgh");
        assert_eq!(s.status, "valid");
        assert!(s.detail.contains("Connect Google"));
    }

    #[test]
    fn google_client_classifier_bad_shape() {
        // Bare/garbage client id is rejected (won't be stored).
        let bad_id = classify_google_client("google_client_id", "not-a-client");
        assert_eq!(bad_id.status, "unauthorized");
        assert!(bad_id.detail.contains("apps.googleusercontent.com"));
        // The suffix alone (no prefix) is not a real id.
        let suffix_only =
            classify_google_client("google_client_id", ".apps.googleusercontent.com");
        assert_eq!(suffix_only.status, "unauthorized");
        // Too-short secret is rejected.
        let bad_secret = classify_google_client("google_client_secret", "short");
        assert_eq!(bad_secret.status, "unauthorized");
    }

    #[test]
    fn google_client_classifier_never_echoes_the_secret() {
        // The detail is fixed guidance — it must not contain the pasted value.
        let secret = "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ak";
        let r = classify_google_client("google_client_secret", secret);
        assert!(!r.detail.contains(secret));
        assert!(!r.detail.contains("super-secret"));
    }

    #[test]
    fn ads_oauth_provider_names_map_for_status_rows() {
        assert_eq!(oauth_provider_for("google_ads"), "Google Ads");
        assert_eq!(oauth_provider_for("meta_ads"), "Meta Ads");
        // Google Ads names its OWN connect phrase (distinct from Workspace).
        assert!(oauth_connect_via_daemon(oauth_provider_for("google_ads"))
            .detail
            .contains("connect Google Ads"));
        assert!(oauth_connect_via_daemon(oauth_provider_for("meta_ads"))
            .detail
            .contains("connect Meta Ads"));
    }

    #[test]
    fn customer_id_classifier_accepts_ten_digits_with_or_without_dashes() {
        assert_eq!(classify_customer_id("Google Ads", "1234567890").status, "valid");
        assert_eq!(classify_customer_id("Google Ads", "123-456-7890").status, "valid");
        // Wrong length / non-digits are rejected.
        assert_eq!(classify_customer_id("Google Ads", "12345").status, "unauthorized");
        assert_eq!(classify_customer_id("Google Ads", "12345678901").status, "unauthorized");
        assert_eq!(classify_customer_id("Google Ads", "12345abcde").status, "unauthorized");
        assert_eq!(classify_customer_id("Google Ads", "").status, "unauthorized");
    }

    #[test]
    fn meta_ad_account_classifier_requires_act_prefix_and_digits() {
        assert_eq!(classify_meta_ad_account("act_1234567890").status, "valid");
        // Missing prefix, empty digits, or non-digits are rejected.
        assert_eq!(classify_meta_ad_account("1234567890").status, "unauthorized");
        assert_eq!(classify_meta_ad_account("act_").status, "unauthorized");
        assert_eq!(classify_meta_ad_account("act_12ab34").status, "unauthorized");
        assert_eq!(classify_meta_ad_account("").status, "unauthorized");
    }

    #[test]
    fn ads_classifiers_never_echo_the_value() {
        // The customer-id / ad-account classifiers' detail is fixed guidance.
        let cust = classify_customer_id("Google Ads", "1234567890");
        assert!(!cust.detail.contains("1234567890"));
        let acct = classify_meta_ad_account("act_9998887776");
        assert!(!acct.detail.contains("9998887776"));
    }

    #[tokio::test]
    async fn ads_status_rows_short_circuit_without_network() {
        // The Google Ads / Meta Ads OAuth status ids must NOT hit the network.
        let g = verify_dispatch("google_ads", "anything").await.unwrap();
        assert_eq!(g.status, "unauthorized");
        assert!(g.detail.contains("connect Google Ads"));
        let m = verify_dispatch("meta_ads", "anything").await.unwrap();
        assert_eq!(m.status, "unauthorized");
        assert!(m.detail.contains("connect Meta Ads"));
    }

    #[tokio::test]
    async fn ads_client_dispatch_is_local_only() {
        // Every Google Ads / Meta Ads pasted value is format-checked locally, never
        // over HTTP.
        assert_eq!(
            verify_dispatch("google_ads_client_id", "9-a.apps.googleusercontent.com")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("google_ads_client_secret", "GOCSPX-abcdefgh")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("google_ads_developer_token", "DEVTOKEN1234")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("google_ads_customer_id", "123-456-7890")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("google_ads_login_customer_id", "1234567890")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("meta_app_id", "1234567890123456")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("meta_app_secret", "abcdef1234567890")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("meta_ad_account_id", "act_1234567890")
                .await
                .unwrap()
                .status,
            "valid"
        );
        // A malformed ad account id is rejected (won't be stored).
        assert_eq!(
            verify_dispatch("meta_ad_account_id", "1234567890")
                .await
                .unwrap()
                .status,
            "unauthorized"
        );
    }

    #[tokio::test]
    async fn whoop_status_row_short_circuits_without_network() {
        // The WHOOP OAuth status id must NOT hit the network — the consent runs in
        // the daemon's `connect_whoop` tool.
        let w = verify_dispatch("whoop", "anything").await.unwrap();
        assert_eq!(w.status, "unauthorized");
        assert!(w.detail.contains("connect WHOOP"), "got: {}", w.detail);
    }

    #[tokio::test]
    async fn whoop_client_dispatch_is_local_only() {
        // The WHOOP client id/secret are format-checked locally, never over HTTP.
        assert_eq!(
            verify_dispatch("whoop_client_id", "abc12345xyz")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("whoop_client_secret", "whoop-secret-9988")
                .await
                .unwrap()
                .status,
            "valid"
        );
        // Too-short values are rejected (won't be stored), and the detail never
        // echoes the value.
        let bad = verify_dispatch("whoop_client_secret", "no").await.unwrap();
        assert_eq!(bad.status, "unauthorized");
        assert!(!bad.detail.contains("no") || !bad.detail.contains("\"no\""));
    }

    #[tokio::test]
    async fn home_assistant_dispatch_is_local_only() {
        // Both Home Assistant rows are format-checked locally, never over HTTP — a
        // bare probe would reach into the user's own LAN. The URL is shape-checked.
        let ok_url = verify_dispatch("homeassistant_url", "http://homeassistant.local:8123")
            .await
            .unwrap();
        assert_eq!(ok_url.status, "valid");
        assert!(ok_url.detail.contains("token"), "names the next step: {}", ok_url.detail);
        // A non-URL is rejected and won't be stored.
        let bad_url = verify_dispatch("homeassistant_url", "homeassistant.local")
            .await
            .unwrap();
        assert_eq!(bad_url.status, "unauthorized");
        // The long-lived token is an opaque string -> plausible-length check.
        assert_eq!(
            verify_dispatch("homeassistant_token", "eyJhbG.fake.long-lived-token")
                .await
                .unwrap()
                .status,
            "valid"
        );
        let bad_token = verify_dispatch("homeassistant_token", "no").await.unwrap();
        assert_eq!(bad_token.status, "unauthorized");
    }

    #[test]
    fn classify_home_assistant_url_shape_check() {
        assert_eq!(
            classify_home_assistant_url("http://homeassistant.local:8123").status,
            "valid"
        );
        assert_eq!(
            classify_home_assistant_url("https://hub.example.com").status,
            "valid"
        );
        assert_eq!(classify_home_assistant_url("hub.example.com").status, "unauthorized");
        assert_eq!(classify_home_assistant_url("https://").status, "unauthorized");
        assert_eq!(classify_home_assistant_url("").status, "unauthorized");
    }

    #[tokio::test]
    async fn plaid_dispatch_is_local_only() {
        // All three Plaid rows are format-checked locally, never over HTTP — a bare
        // probe would call Plaid with the user's own credentials. The client id /
        // secret are opaque strings -> plausible-length check; the access token is
        // shape-checked for the Plaid Link `access-…` prefix.
        assert_eq!(
            verify_dispatch("plaid_client_id", "5f9a8b7c6d5e4f3a2b1c")
                .await
                .unwrap()
                .status,
            "valid"
        );
        assert_eq!(
            verify_dispatch("plaid_secret", "plaid-secret-abcdef123456")
                .await
                .unwrap()
                .status,
            "valid"
        );
        // A well-shaped Plaid Link access token (access-<env>-…) is accepted, and the
        // success line states MIDAS reads only.
        let ok_tok = verify_dispatch("plaid_access_token", "access-sandbox-1234abcd-ef56")
            .await
            .unwrap();
        assert_eq!(ok_tok.status, "valid");
        assert!(
            ok_tok.detail.to_lowercase().contains("reads only")
                || ok_tok.detail.to_lowercase().contains("never moves money"),
            "the access-token success line must state MIDAS reads only: {}",
            ok_tok.detail
        );
        // Too-short id/secret and a non-`access-` token are rejected (won't be stored).
        assert_eq!(
            verify_dispatch("plaid_client_id", "no").await.unwrap().status,
            "unauthorized"
        );
        let bad_tok = verify_dispatch("plaid_access_token", "not-a-plaid-token")
            .await
            .unwrap();
        assert_eq!(bad_tok.status, "unauthorized");
    }

    #[test]
    fn classify_plaid_access_token_shape_check() {
        assert_eq!(
            classify_plaid_access_token("access-sandbox-deadbeef-0000").status,
            "valid"
        );
        assert_eq!(
            classify_plaid_access_token("access-production-abcd1234").status,
            "valid"
        );
        // Missing prefix, or nothing after it, is rejected.
        assert_eq!(classify_plaid_access_token("sandbox-1234").status, "unauthorized");
        assert_eq!(classify_plaid_access_token("access-").status, "unauthorized");
        assert_eq!(classify_plaid_access_token("").status, "unauthorized");
        // The detail never echoes the pasted token value.
        let secret = "access-sandbox-super-secret-do-not-leak";
        let r = classify_plaid_access_token(secret);
        assert!(!r.detail.contains(secret), "must not echo the token");
        assert!(!r.detail.contains("super-secret"));
    }

    #[tokio::test]
    async fn maps_dispatch_is_local_only() {
        // The Maps Platform API key is format-checked locally, never over HTTP — a
        // bare probe would call the maps provider with the user's own key (and the
        // key must never ride a logged URL). The key is an opaque string, so the
        // check is a plausible-length one via classify_oauth_client; the value is
        // never echoed.
        let ok = verify_dispatch("maps_api_key", "AIzaSyA-FAKE-maps-key-1234567890")
            .await
            .unwrap();
        assert_eq!(ok.status, "valid");
        assert!(
            !ok.detail.contains("AIzaSyA-FAKE-maps-key-1234567890"),
            "the success line must not echo the key: {}",
            ok.detail
        );
        // A too-short value is rejected (won't be stored).
        assert_eq!(
            verify_dispatch("maps_api_key", "no").await.unwrap().status,
            "unauthorized"
        );
    }

    #[tokio::test]
    async fn hibp_dispatch_is_local_only() {
        // The Have I Been Pwned API key is format-checked locally, never over HTTP —
        // a bare probe would call HIBP with the user's own key (and the key must never
        // ride a logged URL). The key is an opaque string, so the check is a
        // plausible-length one via classify_oauth_client; the value is never echoed.
        let ok = verify_dispatch("hibp_api_key", "HIBP-FAKE-api-key-1234567890")
            .await
            .unwrap();
        assert_eq!(ok.status, "valid");
        assert!(
            !ok.detail.contains("HIBP-FAKE-api-key-1234567890"),
            "the success line must not echo the key: {}",
            ok.detail
        );
        // A too-short value is rejected (won't be stored).
        assert_eq!(
            verify_dispatch("hibp_api_key", "no").await.unwrap().status,
            "unauthorized"
        );
    }

    #[tokio::test]
    async fn elevenlabs_dispatch_is_local_only() {
        // The ElevenLabs API key is format-checked locally, never over HTTP — a bare
        // probe would make a cloud TTS call with the user's own key (and the key must
        // never ride a logged URL; it belongs in the xi-api-key header). The key is an
        // opaque string, so the check is a plausible-length one via
        // classify_oauth_client; the value is never echoed. Storing it does NOT turn
        // the cloud voice tier on — that ships OFF and is flipped separately.
        let ok = verify_dispatch("elevenlabs_api_key", "EL-FAKE-api-key-1234567890")
            .await
            .unwrap();
        assert_eq!(ok.status, "valid");
        assert!(
            !ok.detail.contains("EL-FAKE-api-key-1234567890"),
            "the success line must not echo the key: {}",
            ok.detail
        );
        // A too-short value is rejected (won't be stored).
        assert_eq!(
            verify_dispatch("elevenlabs_api_key", "no").await.unwrap().status,
            "unauthorized"
        );
    }

    #[tokio::test]
    async fn workspace_dispatch_short_circuits_without_network() {
        // verify_dispatch must NOT hit the network for the OAuth status id.
        let r = verify_dispatch("google_workspace", "anything").await.unwrap();
        assert_eq!(r.status, "unauthorized");
    }

    #[tokio::test]
    async fn google_client_dispatch_is_local_only() {
        // The client id/secret ids are format-checked locally, never over HTTP.
        let r = verify_dispatch(
            "google_client_id",
            "999-xyz.apps.googleusercontent.com",
        )
        .await
        .unwrap();
        assert_eq!(r.status, "valid");
        let bad = verify_dispatch("google_client_id", "nope").await.unwrap();
        assert_eq!(bad.status, "unauthorized");
        let sec = verify_dispatch("google_client_secret", "GOCSPX-abcdefgh")
            .await
            .unwrap();
        assert_eq!(sec.status, "valid");
    }

    #[tokio::test]
    async fn unknown_id_is_rejected() {
        assert!(verify_dispatch("loki", "x").await.is_err());
    }
}
