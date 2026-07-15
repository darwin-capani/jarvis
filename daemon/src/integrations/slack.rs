//! Slack client for agent "veronica" (Content+Comms). Thin layer over the
//! shared integration foundation (`super`): generic over [`HttpTransport`] so
//! production wires [`ReqwestTransport`] and tests wire `MockTransport`, with
//! the bot token resolved from the Keychain via `resolve_secret`.
//!
//! Slack's Web API is the awkward one: it answers 200 OK even for failures and
//! puts the real verdict in a JSON `{"ok": bool, "error": "<code>"}` envelope.
//! So a green HTTP status is necessary but NOT sufficient — every method checks
//! `ok` and maps a false verdict to a secret-free, human-facing message (the
//! task pins invalid_auth/not_authed -> "Slack token rejected — check it in
//! Settings"; channel_not_found; etc.). HTTP-level failures (network, 5xx,
//! 429) still route through the foundation's `status_outcome` first.
//!
//! READ methods (list_channels, channel_history, auth_whoami) are safe and
//! never gated. The one CONSEQUENTIAL method (post_message) takes an
//! [`ActionMode`]: in `DryRun` it builds a preview and posts NOTHING; only in
//! `Execute` does it issue exactly one chat.postMessage. The bot token is set
//! in the Authorization header at the moment of each send and is NEVER logged —
//! presence is reported as a bool at most.

use serde::Deserialize;
use serde_json::json;
use tracing::info;

use super::{
    resolve_secret, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// Keychain account holding the Slack bot token (xoxb-…). Must be on the
/// foundation's `ALLOWED_ACCOUNTS`, which it is.
const TOKEN_ACCOUNT: &str = "slack_bot_token";
/// Slack Web API base. Each method appends its endpoint (e.g. `auth.test`).
const API_BASE: &str = "https://slack.com/api";

/// A Slack client bound to a bot token and an HTTP transport. Generic over
/// `T: HttpTransport` so the same code runs against `ReqwestTransport` in
/// production and `MockTransport` in tests. The token lives only in this struct
/// and only ever leaves it inside the per-request Authorization header.
pub struct SlackClient<T: HttpTransport> {
    transport: T,
    token: String,
}

impl<T: HttpTransport> SlackClient<T> {
    /// Build a client from an explicit token + transport. Used by tests (mock
    /// transport, dummy token) and by [`Self::connect`] internally. The token is
    /// not validated here — a bad token surfaces as an `ok=false` invalid_auth
    /// from Slack on the first call, mapped to a friendly message.
    pub fn new(transport: T, token: impl Into<String>) -> Self {
        Self {
            transport,
            token: token.into(),
        }
    }

    // -- request plumbing -----------------------------------------------------

    /// Full URL for a Slack Web API method name.
    fn endpoint(method: &str) -> String {
        format!("{API_BASE}/{method}")
    }

    /// Attach the bearer auth header. The token rides here at the moment of the
    /// send and nowhere else — never persisted on the transport, never logged.
    fn authed(&self, req: HttpRequest) -> HttpRequest {
        req.header("Authorization", format!("Bearer {}", self.token))
    }

    /// Send a request, map any HTTP-level failure (network already surfaced as
    /// `Err`; 4xx/5xx via `status_outcome`) to an error, then parse the body as
    /// JSON. The returned `Value`'s `ok`/`error` envelope is the caller's to
    /// interpret. `what` names the action for any error message — keep it
    /// secret-free.
    async fn send_json(
        &self,
        req: HttpRequest,
        what: &str,
    ) -> IntegrationResult<serde_json::Value> {
        let resp = self.transport.send(self.authed(req)).await?;
        // Slack normally answers 200 even for logical failures, but a real
        // HTTP error (5xx, 429, an auth-layer 401) still must not be parsed as
        // a Slack envelope — translate it to a friendly outcome first.
        status_outcome_for(resp.status).into_result(what)?;
        resp.json()
    }

    // -- READ methods (safe, never gated) -------------------------------------

    /// `auth.test`: confirm the token works and report which team + user it is.
    /// A cheap connectivity probe. Returns a one-line human-facing summary.
    pub async fn auth_whoami(&self) -> IntegrationResult<String> {
        let req = HttpRequest::new(HttpMethod::Get, Self::endpoint("auth.test"));
        let body = self.send_json(req, "Slack auth check").await?;
        let env: AuthTest = parse_envelope(body, "Slack auth check")?;
        info!(ok = true, "slack: auth.test succeeded");
        Ok(format!(
            "Connected to Slack workspace \"{}\" as {}.",
            env.team, env.user
        ))
    }

    /// `conversations.list` (public channels): the first `limit` public channels
    /// as a "#name (id)" list. Read-only.
    pub async fn list_channels(&self, limit: u32) -> IntegrationResult<String> {
        let req = HttpRequest::new(
            HttpMethod::Get,
            format!(
                "{}?types=public_channel&limit={}",
                Self::endpoint("conversations.list"),
                limit
            ),
        );
        let body = self.send_json(req, "Slack channel list").await?;
        let env: ConversationsList = parse_envelope(body, "Slack channel list")?;
        if env.channels.is_empty() {
            return Ok("No public Slack channels found.".to_string());
        }
        let lines: Vec<String> = env
            .channels
            .iter()
            .map(|c| format!("#{} ({})", c.name, c.id))
            .collect();
        info!(count = env.channels.len(), "slack: conversations.list succeeded");
        Ok(format!(
            "{} public channel(s): {}",
            env.channels.len(),
            lines.join(", ")
        ))
    }

    /// `conversations.history`: the most recent `limit` messages in `channel`,
    /// newest first, as "user: text" lines. Read-only.
    pub async fn channel_history(&self, channel: &str, limit: u32) -> IntegrationResult<String> {
        let req = HttpRequest::new(
            HttpMethod::Get,
            format!(
                "{}?channel={}&limit={}",
                Self::endpoint("conversations.history"),
                channel,
                limit
            ),
        );
        let body = self.send_json(req, "Slack channel history").await?;
        let env: ConversationsHistory = parse_envelope(body, "Slack channel history")?;
        if env.messages.is_empty() {
            return Ok(format!("No recent messages in {channel}."));
        }
        // Slack returns history newest-first already; keep that order.
        let lines: Vec<String> = env
            .messages
            .iter()
            .map(|m| {
                let who = m.user.as_deref().unwrap_or("unknown");
                let text = m.text.as_deref().unwrap_or("").trim();
                format!("{who}: {text}")
            })
            .collect();
        info!(count = env.messages.len(), "slack: conversations.history succeeded");
        Ok(format!(
            "{} recent message(s) in {channel} (newest first): {}",
            env.messages.len(),
            lines.join(" | ")
        ))
    }

    // -- CONSEQUENTIAL method (gated) -----------------------------------------

    /// `chat.postMessage`: post `text` to `channel`. CONSEQUENTIAL — routed
    /// through [`ActionMode`]. In [`ActionMode::DryRun`] it posts NOTHING and
    /// returns a preview of what would be sent; only in [`ActionMode::Execute`]
    /// does it issue exactly one chat.postMessage. On success it confirms; on an
    /// `ok=false` verdict it returns the friendly mapping.
    pub async fn post_message(
        &self,
        channel: &str,
        text: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        if mode == ActionMode::DryRun {
            // No request is built or sent — pure preview.
            info!(dry_run = true, "slack: chat.postMessage preview (no post)");
            return Ok(format!(
                "[dry run] Would post to Slack {channel}: \"{text}\" (not sent)"
            ));
        }
        let req = HttpRequest::new(HttpMethod::Post, Self::endpoint("chat.postMessage"))
            .json_body(json!({ "channel": channel, "text": text }));
        let body = self.send_json(req, "Slack message post").await?;
        let env: PostMessage = parse_envelope(body, "Slack message post")?;
        info!(ok = true, "slack: chat.postMessage succeeded");
        Ok(format!(
            "Posted to Slack {}.",
            env.channel.as_deref().unwrap_or(channel)
        ))
    }
}

impl SlackClient<super::ReqwestTransport> {
    /// Production constructor: resolve the bot token from the Keychain and wire
    /// the real reqwest transport. Returns `None` when no token is configured
    /// (the not-yet-connected case) so the caller can render a "connect Slack in
    /// Settings" message rather than erroring.
    pub async fn connect() -> Option<Self> {
        let token = resolve_secret(TOKEN_ACCOUNT).await?;
        Some(Self::new(super::ReqwestTransport::new(), token))
    }
}

// ---------------------------------------------------------------------------
// Envelope parsing + ok=false -> friendly error
// ---------------------------------------------------------------------------

/// Minimal view of the Slack `ok`/`error` envelope every Web API response
/// carries, parsed BEFORE the typed payload so a logical failure (ok=false) is
/// mapped to a friendly message regardless of which method made the call.
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    error: Option<String>,
}

/// Deserialize a Slack response into a typed payload `P`, but first enforce the
/// `ok` verdict: an `ok=false` body becomes a friendly, secret-free error
/// (never the raw provider body). `what` names the action for the message.
fn parse_envelope<P: serde::de::DeserializeOwned>(
    body: serde_json::Value,
    what: &str,
) -> IntegrationResult<P> {
    let env: Envelope =
        serde_json::from_value(body.clone()).map_err(|_| anyhow_secret_free(what, None))?;
    if !env.ok {
        return Err(anyhow_secret_free(what, env.error.as_deref()));
    }
    serde_json::from_value(body).map_err(|_| anyhow_secret_free(what, None))
}

/// Build a secret-free error for a Slack failure. Maps the known `error` codes
/// to friendly language (the task pins the auth cases) and falls back to the
/// code itself (which is a fixed Slack identifier, not secret material) for the
/// rest. A `None` code means the body was unparsable.
fn anyhow_secret_free(what: &str, code: Option<&str>) -> super::IntegrationError {
    let detail = match code {
        Some("invalid_auth") | Some("not_authed") | Some("token_revoked") | Some("account_inactive") => {
            "Slack token rejected — check it in Settings".to_string()
        }
        Some("missing_scope") | Some("not_allowed_token_type") => {
            "the Slack token is missing a required scope".to_string()
        }
        Some("channel_not_found") => "that Slack channel was not found".to_string(),
        Some("not_in_channel") => "the bot is not a member of that Slack channel".to_string(),
        Some("is_archived") => "that Slack channel is archived".to_string(),
        Some("msg_too_long") => "the Slack message is too long".to_string(),
        Some("rate_limited") | Some("ratelimited") => {
            "Slack rate limited the request; try again shortly".to_string()
        }
        Some(other) => format!("Slack returned error \"{other}\""),
        None => "Slack returned an unreadable response".to_string(),
    };
    anyhow::anyhow!("{what} failed: {detail}")
}

/// Thin wrapper over the foundation's status mapper, kept local so the call
/// sites read clearly. Pure.
fn status_outcome_for(status: u16) -> StatusOutcome {
    super::status_outcome(status)
}

// ---------------------------------------------------------------------------
// Typed payloads (only the fields we surface)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct AuthTest {
    #[serde(default)]
    team: String,
    #[serde(default)]
    user: String,
}

#[derive(Deserialize)]
struct ConversationsList {
    #[serde(default)]
    channels: Vec<Channel>,
}

#[derive(Deserialize)]
struct Channel {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct ConversationsHistory {
    #[serde(default)]
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct Message {
    text: Option<String>,
    user: Option<String>,
}

#[derive(Deserialize)]
struct PostMessage {
    channel: Option<String>,
}

// ---------------------------------------------------------------------------
// Tests — hermetic, via the foundation's MockTransport. No network, ever.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;

    /// A token value that, if it ever appeared in output, would be unmistakable
    /// in an assertion. Tests assert it NEVER surfaces in a human-facing string.
    const FAKE_TOKEN: &str = "xoxb-FAKE-SECRET-DO-NOT-LEAK-123456";

    fn list_ok_body() -> &'static str {
        r#"{"ok":true,"channels":[
            {"id":"C111","name":"general"},
            {"id":"C222","name":"random"}
        ]}"#
    }

    fn history_ok_body() -> &'static str {
        r#"{"ok":true,"messages":[
            {"type":"message","user":"U999","text":"latest message"},
            {"type":"message","user":"U888","text":"older message"}
        ]}"#
    }

    fn post_ok_body() -> &'static str {
        r#"{"ok":true,"channel":"C111","ts":"1700000000.000100"}"#
    }

    fn invalid_auth_body() -> &'static str {
        r#"{"ok":false,"error":"invalid_auth"}"#
    }

    // -- READ: parsing --------------------------------------------------------

    #[tokio::test]
    async fn list_channels_parses_names_and_ids() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "conversations.list",
            200,
            list_ok_body(),
        );
        let client = SlackClient::new(mock, FAKE_TOKEN);
        let out = client.list_channels(50).await.unwrap();
        assert!(out.contains("#general (C111)"), "got: {out}");
        assert!(out.contains("#random (C222)"), "got: {out}");
    }

    #[tokio::test]
    async fn channel_history_parses_text_and_author_newest_first() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "conversations.history",
            200,
            history_ok_body(),
        );
        let client = SlackClient::new(mock, FAKE_TOKEN);
        let out = client.channel_history("C111", 10).await.unwrap();
        assert!(out.contains("U999: latest message"), "got: {out}");
        assert!(out.contains("U888: older message"), "got: {out}");
        // Newest-first: the latest message appears before the older one.
        let latest = out.find("latest message").unwrap();
        let older = out.find("older message").unwrap();
        assert!(latest < older, "history must be newest-first: {out}");
    }

    #[tokio::test]
    async fn auth_whoami_reports_team_and_user() {
        let body = r#"{"ok":true,"team":"Acme","user":"darwinbot","team_id":"T1","user_id":"U1"}"#;
        let mock = MockTransport::new().on(HttpMethod::Get, "auth.test", 200, body);
        let client = SlackClient::new(mock, FAKE_TOKEN);
        let out = client.auth_whoami().await.unwrap();
        assert!(out.contains("Acme"), "got: {out}");
        assert!(out.contains("darwinbot"), "got: {out}");
    }

    // -- ok=false -> friendly error -------------------------------------------

    #[tokio::test]
    async fn invalid_auth_maps_to_friendly_message() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "auth.test",
            200,
            invalid_auth_body(),
        );
        let client = SlackClient::new(mock, FAKE_TOKEN);
        let err = client.auth_whoami().await.unwrap_err().to_string();
        assert!(
            err.contains("Slack token rejected — check it in Settings"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn channel_not_found_maps_to_friendly_message() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "conversations.history",
            200,
            r#"{"ok":false,"error":"channel_not_found"}"#,
        );
        let client = SlackClient::new(mock, FAKE_TOKEN);
        let err = client.channel_history("CDOESNOTEXIST", 5).await.unwrap_err().to_string();
        assert!(err.contains("channel was not found"), "got: {err}");
    }

    // -- CONSEQUENTIAL: DryRun posts nothing ----------------------------------

    #[tokio::test]
    async fn post_message_dry_run_posts_nothing_and_previews() {
        // Register a post endpoint so that, if DryRun mistakenly sent, it would
        // be recorded — proving by absence that nothing went out.
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "chat.postMessage",
            200,
            post_ok_body(),
        );
        let client = SlackClient::new(mock, FAKE_TOKEN);
        let out = client
            .post_message("#general", "hello team", ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("hello team"), "got: {out}");
        assert!(out.contains("not sent"), "got: {out}");
        // The crux: NO request was ever issued in DryRun.
        assert!(
            client.transport.requests().is_empty(),
            "DryRun must not issue any HTTP request"
        );
    }

    // -- CONSEQUENTIAL: Execute posts exactly once with right channel+text ----

    #[tokio::test]
    async fn post_message_execute_issues_one_post_with_channel_and_text() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "chat.postMessage",
            200,
            post_ok_body(),
        );
        let client = SlackClient::new(mock, FAKE_TOKEN);
        let out = client
            .post_message("C111", "deploy is green", ActionMode::Execute)
            .await
            .unwrap();
        assert!(out.contains("Posted to Slack"), "got: {out}");

        // Exactly one request, and it is the chat.postMessage with our payload.
        let req = client.transport.last_request();
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.contains("chat.postMessage"), "url: {}", req.url);
        let sent = req.body.as_ref().expect("post must carry a JSON body");
        assert_eq!(sent["channel"], "C111");
        assert_eq!(sent["text"], "deploy is green");
        // Auth header is PRESENT — its value is never asserted.
        assert!(req.has_header("authorization"), "auth header must be set");
    }

    // -- token never leaks ----------------------------------------------------

    /// The bot token must NEVER appear in any human-facing outcome string —
    /// across success and failure paths, read and write — and must only ever
    /// live in the Authorization header on the wire (asserted by header
    /// PRESENCE, never value).
    #[tokio::test]
    async fn token_never_leaks_into_any_output() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "conversations.list", 200, list_ok_body())
            .on(HttpMethod::Get, "conversations.history", 200, history_ok_body())
            .on(HttpMethod::Get, "auth.test", 200, invalid_auth_body())
            .on(HttpMethod::Post, "chat.postMessage", 200, post_ok_body());
        let client = SlackClient::new(mock, FAKE_TOKEN);

        let list = client.list_channels(10).await.unwrap();
        let hist = client.channel_history("C111", 5).await.unwrap();
        let auth_err = client.auth_whoami().await.unwrap_err().to_string();
        let preview = client
            .post_message("C111", "hi", ActionMode::DryRun)
            .await
            .unwrap();
        let posted = client
            .post_message("C111", "hi", ActionMode::Execute)
            .await
            .unwrap();

        for s in [&list, &hist, &auth_err, &preview, &posted] {
            assert!(
                !s.contains(FAKE_TOKEN),
                "token must never appear in output: {s}"
            );
        }
        // And it never appears in any RECORDED header value either, except as a
        // bearer in the Authorization header (we assert the token is not echoed
        // into URL or body — it lives only in the auth header value).
        for req in client.transport.requests() {
            assert!(!req.url.contains(FAKE_TOKEN), "token must not be in a URL");
            if let Some(body) = &req.body {
                assert!(
                    !body.to_string().contains(FAKE_TOKEN),
                    "token must not be in a body"
                );
            }
        }
    }
}
