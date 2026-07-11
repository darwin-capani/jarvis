//! Gmail client for agents "friday" (Daily Intel) and "pepper" (Personal EA).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]) and the Google OAuth2 core
//! ([`crate::integrations::google_oauth`]). It is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests) and gets its access token from a
//! shared [`GoogleAuth`] handle via [`GoogleAuth::bearer`]. The Gmail client
//! NEVER touches the refresh token and never resolves a Keychain secret itself —
//! that is the OAuth core's job; this client only ever asks for a bearer at the
//! moment of each send and attaches it as the `Authorization` header. The token
//! VALUE is never logged, never stored on the transport, never in an
//! error/Debug field — only presence (a bool) is ever recorded.
//!
//! Two tiers of methods, mirroring the foundation's safety model:
//!   * READ (safe, never gated): `list_recent_messages`, `get_message` — these
//!     fetch only message METADATA (From/Subject) plus Gmail's own short
//!     `snippet`, NEVER the full body. `list_recent_messages` does a
//!     `messages.list` then a bounded fan-out of `messages.get?format=metadata`
//!     to surface a concise newest-first summary.
//!   * CONSEQUENTIAL (gated by [`ActionMode`]): `send_message` — the MOST
//!     sensitive action in this round, sending email AS THE USER. In
//!     [`ActionMode::DryRun`] it issues NO request and returns a clear PREVIEW
//!     (to / subject / first line); only in [`ActionMode::Execute`] does it
//!     issue exactly one `messages.send` carrying a base64url-encoded RFC 2822
//!     message. Call sites get `mode` from the foundation's `gate(confirm)`, so
//!     with `[integrations].allow_consequential` false (the shipped default) a
//!     send always previews.
//!
//! Non-2xx responses map to friendly, secret-free errors via [`map_status`]
//! (401 -> reconnect; 403 -> scope), never echoing the provider body (which can
//! carry message content or token-bearing fields).

use serde::Deserialize;
use tracing::info;

use super::google_oauth::GoogleAuth;
use super::{
    status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// Gmail REST API base, scoped to the authenticated user (`me`). All paths are
/// appended to this.
const API_BASE: &str = "https://gmail.googleapis.com/gmail/v1/users/me";

/// Hard ceiling on how many messages `list_recent_messages` will fan out a
/// `messages.get` for. `messages.list` can return many ids; each id costs one
/// metadata GET, so we cap the fan-out to keep a read cheap and the summary
/// concise — the user gets the newest few, not a mailbox dump.
const MAX_FANOUT: u32 = 10;

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields the agents actually surface are
// decoded. `#[serde(default)]` keeps parsing resilient to Gmail's many extra
// keys and to objects that omit an optional field.
// ---------------------------------------------------------------------------

/// One id stub from `messages.list`. Gmail returns `{id, threadId}` per entry;
/// we only need the id to fan out the metadata GET.
#[derive(Debug, Clone, Deserialize)]
struct MessageRef {
    #[serde(default)]
    id: String,
}

/// The `messages.list` envelope: a (possibly empty) array of id stubs.
#[derive(Debug, Clone, Deserialize)]
struct MessagesList {
    #[serde(default)]
    messages: Vec<MessageRef>,
}

/// A single message as returned by `messages.get?format=metadata`. We decode the
/// id, Gmail's short `snippet`, and the metadata `headers` (From/Subject live
/// here); the full body parts are deliberately NOT requested or decoded.
#[derive(Debug, Clone, Deserialize)]
struct Message {
    #[serde(default)]
    id: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    payload: Payload,
}

/// The metadata-format payload: just the header list (no body parts when
/// `format=metadata` is used).
#[derive(Debug, Clone, Default, Deserialize)]
struct Payload {
    #[serde(default)]
    headers: Vec<Header>,
}

/// One RFC 2822 header name/value pair from the metadata payload.
#[derive(Debug, Clone, Deserialize)]
struct Header {
    #[serde(default)]
    name: String,
    #[serde(default)]
    value: String,
}

impl Message {
    /// Case-insensitive header lookup (Gmail returns canonical casing, but RFC
    /// header names are case-insensitive, so we don't rely on it).
    fn header(&self, name: &str) -> Option<&str> {
        self.payload
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(name))
            .map(|h| h.value.as_str())
    }

    /// A concise one-line summary of this message: "From … — Subject (snippet)".
    /// Uses only metadata + Gmail's own snippet, never the full body.
    fn summarize(&self) -> String {
        let from = self.header("From").unwrap_or("(unknown sender)");
        let subject = self.header("Subject").unwrap_or("(no subject)");
        let snippet = self.snippet.trim();
        if snippet.is_empty() {
            format!("From {from} — {subject}")
        } else {
            format!("From {from} — {subject} ({})", snippet_preview(snippet))
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Gmail client bound to a transport and a shared [`GoogleAuth`] handle.
///
/// Construct with [`GmailClient::new`] (production: a `ReqwestTransport` Gmail
/// client paired with a connected `GoogleAuth`) or, in tests,
/// [`GmailClient::with_auth`] (a `MockTransport` Gmail client + a `GoogleAuth`
/// wired over its own `MockTransport`). The client holds NO secret of its own —
/// every request's bearer comes from `auth.bearer()` at the moment of the send,
/// so there is nothing to redact in `Debug` beyond noting the handle is present.
pub struct GmailClient<T: HttpTransport, A: HttpTransport> {
    transport: T,
    auth: GoogleAuth<A>,
}

/// `Debug` notes only that an auth handle is attached — it never prints any
/// token (the `GoogleAuth` `Debug` itself redacts all secrets, but we keep this
/// minimal so a `{:?}` of the Gmail client can't widen the surface).
impl<T: HttpTransport, A: HttpTransport> std::fmt::Debug for GmailClient<T, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GmailClient")
            .field("auth_attached", &true)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport, A: HttpTransport> GmailClient<T, A> {
    /// Build a client over `transport`, taking ownership of a shared
    /// [`GoogleAuth`] handle. Used by tests (mock transports) and by
    /// [`GmailClient::new`] internally. No secret is resolved here — the bearer
    /// is fetched per request from `auth`.
    pub fn with_auth(transport: T, auth: GoogleAuth<A>) -> Self {
        Self { transport, auth }
    }

    /// Compose a request with the Gmail-standard headers, attaching the Bearer
    /// token HERE — fetched fresh from `auth` at the moment of the call — and
    /// nowhere else. The token never lands on the transport or in a log.
    async fn request(&self, method: HttpMethod, path: &str) -> IntegrationResult<HttpRequest> {
        let token = self.auth.bearer().await?;
        Ok(HttpRequest::new(method, format!("{API_BASE}{path}"))
            .header("Authorization", format!("Bearer {token}")))
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// Summarize the most recent messages (up to `max`, clamped to a small
    /// bound), optionally filtered by a Gmail search `query` (e.g. "is:unread",
    /// "from:boss@acme.com"). Read-only: does a `messages.list` then a bounded
    /// fan-out of `messages.get?format=metadata` to surface From/Subject/snippet,
    /// newest first (Gmail returns ids newest-first and we preserve that order).
    /// Returns a concise human-facing summary — never full bodies.
    pub async fn list_recent_messages(
        &self,
        max: u32,
        query: Option<&str>,
    ) -> IntegrationResult<String> {
        let want = max.clamp(1, MAX_FANOUT);
        // `messages.list` only returns ids; ask for at most `want` of them.
        let mut path = format!("/messages?maxResults={want}");
        if let Some(q) = query {
            let q = q.trim();
            if !q.is_empty() {
                path.push_str(&format!("&q={}", encode_query(q)));
            }
        }
        let req = self.request(HttpMethod::Get, &path).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing recent email")?;

        let list: MessagesList = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("listing recent email returned an unexpected response"))?;
        if list.messages.is_empty() {
            return Ok(match query {
                Some(q) if !q.trim().is_empty() => {
                    format!("No recent email matching \"{}\".", q.trim())
                }
                _ => "No recent email found.".to_string(),
            });
        }

        // Fan out one metadata GET per id (already capped by `want`), newest
        // first. A single message that fails to fetch is skipped, not fatal —
        // the summary degrades gracefully rather than failing the whole read.
        let mut lines: Vec<String> = Vec::new();
        for r in list.messages.iter().take(want as usize) {
            if r.id.is_empty() {
                continue;
            }
            match self.fetch_metadata(&r.id).await {
                Ok(msg) => lines.push(msg.summarize()),
                Err(_) => continue,
            }
        }
        if lines.is_empty() {
            return Ok("No recent email could be read.".to_string());
        }
        info!(count = lines.len(), "gmail: listed recent messages");
        let header = match query {
            Some(q) if !q.trim().is_empty() => format!(
                "{} recent message(s) matching \"{}\" (newest first):",
                lines.len(),
                q.trim()
            ),
            _ => format!("{} recent message(s) (newest first):", lines.len()),
        };
        Ok(format!("{header} {}", lines.join(" | ")))
    }

    /// COUNT matching messages for EDITH's anticipation collector: one
    /// `messages.list` (NO metadata fan-out — the collector needs a count, not
    /// content), optionally filtered by a Gmail search `query` (e.g.
    /// "is:unread"). Returns how many ids the list returned, capped at `max`
    /// (clamped to the same small fan-out bound so a single read stays cheap and
    /// bounded). Read-only; fetches no bodies, no metadata, no snippets — just the
    /// id count, so it can never surface mail content from the autonomous tick.
    pub async fn count_messages(&self, max: u32, query: Option<&str>) -> IntegrationResult<u32> {
        let want = max.clamp(1, MAX_FANOUT);
        let mut path = format!("/messages?maxResults={want}");
        if let Some(q) = query {
            let q = q.trim();
            if !q.is_empty() {
                path.push_str(&format!("&q={}", encode_query(q)));
            }
        }
        let req = self.request(HttpMethod::Get, &path).await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "counting recent email")?;

        let list: MessagesList = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("counting recent email returned an unexpected response"))?;
        let count = list.messages.iter().filter(|m| !m.id.is_empty()).count() as u32;
        info!(count, "gmail: counted recent messages (no fan-out)");
        Ok(count)
    }

    /// Fetch a single message by `id` (metadata + Gmail's snippet, NOT the full
    /// body). Read-only. Returns a one-line From/Subject/snippet summary.
    pub async fn get_message(&self, id: &str) -> IntegrationResult<String> {
        let msg = self.fetch_metadata(id).await?;
        info!(id_present = !msg.id.is_empty(), "gmail: fetched message metadata");
        Ok(msg.summarize())
    }

    /// Shared metadata GET used by both reads: `messages.get?format=metadata`
    /// restricted to the From/Subject headers, so Gmail returns headers +
    /// `snippet` only — never body parts. Maps non-2xx to a friendly error.
    async fn fetch_metadata(&self, id: &str) -> IntegrationResult<Message> {
        // format=metadata with explicit metadataHeaders keeps the response to
        // the two headers we surface plus the snippet — no body content.
        let req = self
            .request(
                HttpMethod::Get,
                &format!("/messages/{id}?format=metadata&metadataHeaders=From&metadataHeaders=Subject"),
            )
            .await?;
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading the email")?;
        serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("the email response was not in the expected shape"))
    }

    // -- CONSEQUENTIAL (gated by ActionMode) ---------------------------------

    /// Send an email AS THE USER to `to` with `subject` and `body`. The MOST
    /// sensitive action in this round.
    ///
    /// In [`ActionMode::DryRun`] this issues NO request — it returns a clear
    /// PREVIEW of exactly what would be sent (recipient, subject, and the first
    /// line of the body). In [`ActionMode::Execute`] it issues exactly one
    /// `messages.send` carrying a base64url-encoded RFC 2822 message and returns
    /// a short confirmation. Callers obtain `mode` from the foundation's
    /// `gate(confirm)`, so the shipped default (gate OFF) always previews.
    pub async fn send_message(
        &self,
        to: &str,
        subject: &str,
        body: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        if mode == ActionMode::DryRun {
            // No request is built or sent — pure preview. We never include the
            // full body, only its first line, so a long mail doesn't blow up a
            // spoken reply.
            info!(dry_run = true, "gmail: send preview (no request issued)");
            return Ok(format!(
                "[dry run] Would send an email to {to} with subject \"{subject}\" \
                 (begins: \"{}\"). Enable consequential actions and confirm to send.",
                first_line(body)
            ));
        }

        // Build the RFC 2822 message and base64url-encode it (Gmail's raw form).
        let raw = encode_raw_message(to, subject, body);
        let req = self
            .request(HttpMethod::Post, "/messages/send")
            .await?
            .json_body(serde_json::json!({ "raw": raw }));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "sending the email")?;
        info!("gmail: message sent");
        Ok(format!("Email sent to {to}."))
    }
}

impl GmailClient<super::ReqwestTransport, super::ReqwestTransport> {
    /// Production constructor: pair the real reqwest transport for Gmail's API
    /// with a connected [`GoogleAuth`] handle (which itself resolves the OAuth
    /// credentials from the Keychain and wires its own reqwest transport).
    /// Returns the OAuth core's friendly "not connected" error when Google has
    /// not been connected in Settings.
    pub async fn new() -> IntegrationResult<Self> {
        let auth = GoogleAuth::<super::ReqwestTransport>::connect().await?;
        Ok(Self::with_auth(super::ReqwestTransport::new(), auth))
    }
}

// ---------------------------------------------------------------------------
// RFC 2822 message + base64url encoding (pure — unit-testable, no transport)
// ---------------------------------------------------------------------------

/// Build the raw, base64url-encoded (web-safe, padded) RFC 2822 message Gmail's
/// `messages.send` expects in its `raw` field. We set the minimal headers the
/// task surfaces (To, Subject) plus MIME framing for a plain-text UTF-8 body,
/// then base64url-encode the whole thing. Gmail accepts standard base64url for
/// `raw`; we keep padding so the wire form matches the RFC 4648 §5 default and
/// decodes cleanly in the round-trip test.
fn encode_raw_message(to: &str, subject: &str, body: &str) -> String {
    // Headers are sanitized against CRLF injection so a hostile `to`/`subject`
    // can't smuggle extra headers into the message.
    let to = sanitize_header(to);
    let subject = sanitize_header(subject);
    let message = format!(
        "To: {to}\r\n\
         Subject: {subject}\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=\"UTF-8\"\r\n\
         Content-Transfer-Encoding: 7bit\r\n\
         \r\n\
         {body}"
    );
    base64url(message.as_bytes())
}

/// Strip CR/LF from a header value so a crafted recipient or subject cannot
/// inject additional RFC 2822 headers (header-injection defense). Newlines in a
/// single-line header are never legitimate here.
fn sanitize_header(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

/// Base64url (RFC 4648 §5) WITH padding — the URL/filename-safe alphabet Gmail
/// accepts for the `raw` field. Implemented locally (no `base64` dependency, the
/// same rationale `google_oauth::base64url_nopad` uses); padding is kept so the
/// output round-trips through a standard base64url decoder in tests. Pure.
fn base64url(input: &[u8]) -> String {
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
        // Pad the final partial chunk so the output is a multiple of 4.
        match chunk.len() {
            1 => {
                out.push('=');
                out.push('=');
            }
            2 => {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                out.push('=');
            }
            _ => {
                out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
                out.push(ALPHABET[(n & 0x3f) as usize] as char);
            }
        }
    }
    out
}

/// Percent-encode a Gmail search query for use as the `q` URL parameter. Keeps
/// the unreserved set literal and encodes everything else (notably space, `:`,
/// `@` which appear in Gmail search operators). Local + pure so we add no `url`
/// dependency and the encoding is testable.
fn encode_query(value: &str) -> String {
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

/// The first non-empty line of `body`, trimmed and length-capped, for the
/// dry-run preview so a long or multi-line message doesn't blow up the reply.
fn first_line(body: &str) -> String {
    let line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    snippet_preview(line.trim())
}

/// Collapse + truncate a snippet to a single short line for a summary/preview.
fn snippet_preview(text: &str) -> String {
    let one_line = text.replace(['\r', '\n'], " ");
    let trimmed = one_line.trim();
    const MAX: usize = 80;
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

/// Map a Gmail HTTP status to a friendly, secret-free error. 2xx is `Ok`. The
/// task's mappings: 401 -> reconnect (the OAuth token was rejected/expired);
/// 403 -> a missing-scope hint; plus the foundation's 404/429/5xx phrasing. The
/// provider body (which can echo message content) is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    // 401 and 403 both classify as `Unauthorized` in the foundation, but Gmail
    // means different things by them, so we branch on the raw code first.
    match status {
        401 => {
            return Err(anyhow::anyhow!(
                "{what} failed — Google rejected the access token; reconnect Google in Settings"
            ))
        }
        403 => {
            return Err(anyhow::anyhow!(
                "{what} failed — the Google grant is missing a required Gmail scope; reconnect Google in Settings"
            ))
        }
        _ => {}
    }
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::NotFound => {
            Err(anyhow::anyhow!("{what} failed — that message was not found"))
        }
        StatusOutcome::RateLimited => {
            Err(anyhow::anyhow!("{what} was rate limited by Gmail; try again shortly"))
        }
        StatusOutcome::ServerError => {
            Err(anyhow::anyhow!("{what} failed on Google's side; this is usually transient"))
        }
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned Gmail JSON (realistic API SHAPE, never fetched), and the
// shared GoogleAuth handle is wired over its OWN MockTransport with a canned
// refresh response so `bearer()` works without a network or real token. No
// network, no real Google round-trip, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;
    use crate::integrations::google_oauth::{GoogleAuth, RefreshTokenStore, TOKEN_ENDPOINT};

    /// Fake credential values that, if leaked, would be unmistakable in an
    /// assertion. None of these is ever asserted to APPEAR — they are scanned
    /// for ABSENCE in produced output.
    const FAKE_CLIENT_ID: &str = "111-FAKE.apps.googleusercontent.com";
    const FAKE_CLIENT_SECRET: &str = "GOCSPX-FAKE-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "1//FAKE-REFRESH-TOKEN-NEVER-LEAK";
    /// The access token the canned refresh response mints. The Gmail client puts
    /// THIS in its Authorization header; tests assert it never lands in output.
    const FAKE_ACCESS: &str = "ACCESS-FAKE-NEVER-LEAK-IN-OUTPUT";

    /// A no-op Keychain store so building a `GoogleAuth` never touches the real
    /// Keychain.
    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    /// Canned Google refresh response so `auth.bearer()` mints `FAKE_ACCESS`
    /// without a network call. Refresh responses omit `refresh_token`.
    fn refresh_ok_json() -> String {
        format!(
            r#"{{"access_token":"{FAKE_ACCESS}","expires_in":3599,"token_type":"Bearer"}}"#
        )
    }

    /// Build a `GoogleAuth` handle over its own MockTransport that answers the
    /// token endpoint with a canned access token — the shared handle the Gmail
    /// client borrows for `bearer()`.
    fn test_auth() -> GoogleAuth<MockTransport> {
        let token_mock =
            MockTransport::new().on(HttpMethod::Post, TOKEN_ENDPOINT, 200, refresh_ok_json());
        GoogleAuth::new(
            token_mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        )
    }

    /// A Gmail client whose API transport is `gmail_mock` and whose auth handle
    /// is a canned-refresh `GoogleAuth`.
    fn client(gmail_mock: MockTransport) -> GmailClient<MockTransport, MockTransport> {
        GmailClient::with_auth(gmail_mock, test_auth())
    }

    // -- realistic canned payloads (hand-written from the Gmail API shape) ----

    fn messages_list_json() -> &'static str {
        // messages.list returns id stubs, newest first.
        r#"{"messages":[
            {"id":"msg-1","threadId":"t-1"},
            {"id":"msg-2","threadId":"t-2"}
        ],"resultSizeEstimate":2}"#
    }

    fn message_1_json() -> &'static str {
        r#"{"id":"msg-1","threadId":"t-1","snippet":"Quarterly numbers are in",
            "payload":{"headers":[
              {"name":"From","value":"Boss <boss@acme.com>"},
              {"name":"Subject","value":"Q2 results"}
            ]}}"#
    }

    fn message_2_json() -> &'static str {
        r#"{"id":"msg-2","threadId":"t-2","snippet":"Lunch tomorrow?",
            "payload":{"headers":[
              {"name":"From","value":"Pat <pat@example.com>"},
              {"name":"Subject","value":"Catch up"}
            ]}}"#
    }

    fn send_ok_json() -> &'static str {
        // messages.send 200 returns the created message id/threadId.
        r#"{"id":"sent-1","threadId":"t-9","labelIds":["SENT"]}"#
    }

    // -- a standalone base64url decoder used ONLY to verify the encoding -------

    /// Decode standard base64url (with optional padding) back to bytes, so the
    /// test can prove `send_message` encoded the RFC 2822 message correctly.
    /// Test-only; the production path only ever ENCODES.
    fn base64url_decode(s: &str) -> Vec<u8> {
        fn val(c: u8) -> Option<u8> {
            match c {
                b'A'..=b'Z' => Some(c - b'A'),
                b'a'..=b'z' => Some(c - b'a' + 26),
                b'0'..=b'9' => Some(c - b'0' + 52),
                b'-' => Some(62),
                b'_' => Some(63),
                _ => None,
            }
        }
        let mut six: Vec<u8> = Vec::new();
        for &c in s.as_bytes() {
            if c == b'=' {
                continue;
            }
            six.push(val(c).expect("invalid base64url char"));
        }
        let mut out = Vec::new();
        for chunk in six.chunks(4) {
            let n = chunk
                .iter()
                .enumerate()
                .fold(0u32, |acc, (i, &v)| acc | ((v as u32) << (18 - 6 * i)));
            out.push((n >> 16) as u8);
            if chunk.len() > 2 {
                out.push((n >> 8) as u8);
            }
            if chunk.len() > 3 {
                out.push(n as u8);
            }
        }
        out
    }

    // -- READ: parsing + bounded fan-out -------------------------------------

    #[tokio::test]
    async fn list_recent_messages_parses_and_summarizes_newest_first() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/messages?maxResults=", 200, messages_list_json())
            .on(HttpMethod::Get, "/messages/msg-1", 200, message_1_json())
            .on(HttpMethod::Get, "/messages/msg-2", 200, message_2_json());
        let out = client(mock).list_recent_messages(5, None).await.unwrap();
        assert!(out.contains("2 recent message(s)"), "got: {out}");
        assert!(out.contains("boss@acme.com"), "got: {out}");
        assert!(out.contains("Q2 results"), "got: {out}");
        assert!(out.contains("Quarterly numbers are in"), "snippet surfaced: {out}");
        // Newest-first: msg-1 (first in the list) appears before msg-2.
        let p1 = out.find("Q2 results").unwrap();
        let p2 = out.find("Catch up").unwrap();
        assert!(p1 < p2, "must preserve newest-first order: {out}");
    }

    #[tokio::test]
    async fn list_recent_messages_passes_query_and_clamps_max() {
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/messages?maxResults=", 200, messages_list_json())
            .on(HttpMethod::Get, "/messages/msg-1", 200, message_1_json())
            .on(HttpMethod::Get, "/messages/msg-2", 200, message_2_json());
        let c = client(mock);
        // max 999 must clamp to MAX_FANOUT in the list URL.
        let _ = c.list_recent_messages(999, Some("is:unread")).await.unwrap();
        let first = &c.transport.requests()[0];
        assert!(
            first.url.contains(&format!("maxResults={MAX_FANOUT}")),
            "max must clamp to {MAX_FANOUT}: {}",
            first.url
        );
        // The query is URL-encoded into `q` (the ':' becomes %3A).
        assert!(first.url.contains("q=is%3Aunread"), "query not encoded: {}", first.url);
    }

    #[tokio::test]
    async fn list_recent_messages_empty_is_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/messages?maxResults=",
            200,
            r#"{"resultSizeEstimate":0}"#,
        );
        let out = client(mock).list_recent_messages(5, None).await.unwrap();
        assert!(out.contains("No recent email"), "got: {out}");
    }

    #[tokio::test]
    async fn get_message_parses_metadata_and_snippet() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/messages/msg-1", 200, message_1_json());
        let out = client(mock).get_message("msg-1").await.unwrap();
        assert!(out.contains("boss@acme.com"), "got: {out}");
        assert!(out.contains("Q2 results"), "got: {out}");
        assert!(out.contains("Quarterly numbers are in"), "got: {out}");
    }

    #[tokio::test]
    async fn get_message_requests_metadata_format_not_full_body() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/messages/msg-1", 200, message_1_json());
        let c = client(mock);
        c.get_message("msg-1").await.unwrap();
        // The read must ask for metadata, never the full body.
        let req = c.transport.last_request();
        assert!(req.url.contains("format=metadata"), "must use metadata format: {}", req.url);
        assert!(!req.url.contains("format=full"), "must NOT fetch full body: {}", req.url);
        assert!(req.has_header("authorization"), "auth attached");
    }

    // -- CONSEQUENTIAL: DryRun issues NO request, previews --------------------

    #[tokio::test]
    async fn send_message_dry_run_sends_nothing_and_previews() {
        // Register the send endpoint so that, if DryRun mistakenly sent, it
        // would be recorded — proving by absence that nothing went out.
        let mock = MockTransport::new().on(HttpMethod::Post, "/messages/send", 200, send_ok_json());
        let c = client(mock);
        let out = c
            .send_message(
                "alice@example.com",
                "Hello there",
                "First line of the body.\nSecond line.",
                ActionMode::DryRun,
            )
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("alice@example.com"), "preview shows recipient: {out}");
        assert!(out.contains("Hello there"), "preview shows subject: {out}");
        assert!(out.contains("First line of the body."), "preview shows first line: {out}");
        // The CRUX: NO request was ever issued in DryRun (not even a bearer
        // fetch on the Gmail transport).
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not issue any Gmail request"
        );
    }

    // -- CONSEQUENTIAL: Execute issues EXACTLY one send with right encoding ----

    #[tokio::test]
    async fn send_message_execute_issues_one_send_with_correct_raw_encoding() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/messages/send", 200, send_ok_json());
        let c = client(mock);
        let out = c
            .send_message(
                "alice@example.com",
                "Project update",
                "Hi Alice,\nShipping today.\n",
                ActionMode::Execute,
            )
            .await
            .unwrap();
        assert!(out.contains("Email sent to alice@example.com"), "got: {out}");

        // Exactly one Gmail request, and it is the messages.send POST.
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Post);
        assert!(req.url.ends_with("/messages/send"), "url: {}", req.url);
        assert!(req.has_header("authorization"), "auth attached");

        // Decode the `raw` field back to the RFC 2822 message and verify the
        // headers + body survived the base64url round-trip.
        let raw = req.body.as_ref().unwrap()["raw"]
            .as_str()
            .expect("raw must be a string");
        let decoded = String::from_utf8(base64url_decode(raw)).unwrap();
        assert!(decoded.contains("To: alice@example.com\r\n"), "decoded: {decoded}");
        assert!(decoded.contains("Subject: Project update\r\n"), "decoded: {decoded}");
        assert!(decoded.contains("MIME-Version: 1.0\r\n"), "decoded: {decoded}");
        assert!(
            decoded.contains("Content-Type: text/plain; charset=\"UTF-8\"\r\n"),
            "decoded: {decoded}"
        );
        // Header/body separator then the exact body.
        assert!(decoded.contains("\r\n\r\nHi Alice,\nShipping today.\n"), "decoded: {decoded}");
    }

    #[tokio::test]
    async fn send_message_raw_is_web_safe_base64() {
        // Use a body whose bytes force the high base64 indices (62/63), proving
        // the URL-safe alphabet '-'/'_' is used and never '+'/'/'.
        let mock = MockTransport::new().on(HttpMethod::Post, "/messages/send", 200, send_ok_json());
        let c = client(mock);
        c.send_message("a@b.co", "S", "\u{00ff}\u{00ff}\u{00fe}", ActionMode::Execute)
            .await
            .unwrap();
        let req = c.transport.last_request();
        let raw = req.body.as_ref().unwrap()["raw"].as_str().unwrap();
        assert!(!raw.contains('+'), "raw must be URL-safe (no '+'): {raw}");
        assert!(!raw.contains('/'), "raw must be URL-safe (no '/'): {raw}");
    }

    #[tokio::test]
    async fn send_message_sanitizes_header_injection() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/messages/send", 200, send_ok_json());
        let c = client(mock);
        // A recipient trying to smuggle a Bcc header via CRLF.
        c.send_message(
            "victim@example.com\r\nBcc: attacker@evil.com",
            "subj",
            "body",
            ActionMode::Execute,
        )
        .await
        .unwrap();
        let req = c.transport.last_request();
        let raw = req.body.as_ref().unwrap()["raw"].as_str().unwrap();
        let decoded = String::from_utf8(base64url_decode(raw)).unwrap();
        // The injected CRLF was neutralized: the smuggled "Bcc:" is folded into
        // the To value rather than becoming its own header line. Split on the
        // RFC line terminator and assert no line is a standalone Bcc header.
        assert!(
            !decoded.split("\r\n").any(|line| line.starts_with("Bcc:")),
            "header injection produced a rogue Bcc line: {decoded}"
        );
    }

    // -- error mapping --------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_401_maps_to_reconnect() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/messages/msg-x", 401, "{}");
        let err = client(mock).get_message("msg-x").await.unwrap_err().to_string();
        assert!(err.contains("reconnect Google"), "got: {err}");
        assert!(err.contains("access token"), "got: {err}");
    }

    #[tokio::test]
    async fn forbidden_403_maps_to_scope_hint() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/messages/msg-x", 403, "{}");
        let err = client(mock).get_message("msg-x").await.unwrap_err().to_string();
        assert!(err.contains("scope"), "got: {err}");
    }

    #[tokio::test]
    async fn not_found_404_maps_friendly() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/messages/ghost", 404, "{}");
        let err = client(mock).get_message("ghost").await.unwrap_err().to_string();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/messages/msg-1", 503, "down");
        let err = client(mock).get_message("msg-1").await.unwrap_err().to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- NOTHING secret is ever logged / leaked into output -------------------

    /// Neither the access token, the refresh token, nor any email BODY appears
    /// in a produced outcome string, in a mapped error, or in the client's
    /// Debug. We drive a representative slice of the surface and scan every
    /// produced string. The access token must live ONLY in the Authorization
    /// header on the wire (asserted by presence elsewhere), never in a URL or
    /// body.
    #[tokio::test]
    async fn no_token_or_body_ever_leaks() {
        // Debug of the client notes nothing secret.
        let dbg = format!("{:?}", client(MockTransport::new()));
        assert!(!dbg.contains(FAKE_ACCESS), "Debug leaked the access token: {dbg}");
        assert!(!dbg.contains(FAKE_REFRESH), "Debug leaked the refresh token: {dbg}");

        let secret_body = "TOP-SECRET-BODY-CONTENT-DO-NOT-LOG";
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/messages?maxResults=", 200, messages_list_json())
            .on(HttpMethod::Get, "/messages/msg-1", 200, message_1_json())
            .on(HttpMethod::Get, "/messages/msg-2", 200, message_2_json())
            .on(HttpMethod::Post, "/messages/send", 200, send_ok_json());
        let c = client(mock);

        let list = c.list_recent_messages(5, None).await.unwrap();
        let one = c.get_message("msg-1").await.unwrap();
        let sent = c
            .send_message("alice@example.com", "Subj", secret_body, ActionMode::Execute)
            .await
            .unwrap();

        // The send CONFIRMATION must not echo the body, and no produced string
        // may carry either token value.
        assert!(!sent.contains(secret_body), "send confirmation leaked the body: {sent}");
        for s in [&list, &one, &sent] {
            assert!(!s.contains(FAKE_ACCESS), "output leaked the access token: {s}");
            assert!(!s.contains(FAKE_REFRESH), "output leaked the refresh token: {s}");
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
        let err_mock = MockTransport::new().on(HttpMethod::Get, "/messages/x", 401, "{}");
        let err = client(err_mock).get_message("x").await.unwrap_err().to_string();
        assert!(!err.contains(FAKE_ACCESS), "error leaked the access token: {err}");
        assert!(!err.contains(FAKE_REFRESH), "error leaked the refresh token: {err}");
    }

    // -- pure helpers ---------------------------------------------------------

    #[test]
    fn base64url_round_trips_and_is_web_safe() {
        for sample in [
            &b""[..],
            &b"f"[..],
            &b"fo"[..],
            &b"foo"[..],
            &b"foob"[..],
            &b"fooba"[..],
            &b"foobar"[..],
            &[0xff, 0xff, 0xfe][..],
        ] {
            let enc = base64url(sample);
            assert!(!enc.contains('+'), "must be web-safe: {enc}");
            assert!(!enc.contains('/'), "must be web-safe: {enc}");
            // Padded to a multiple of 4.
            assert_eq!(enc.len() % 4, 0, "must be padded to 4: {enc}");
            assert_eq!(base64url_decode(&enc), sample, "round-trip failed for {sample:?}");
        }
    }

    #[test]
    fn encode_raw_message_has_the_required_headers() {
        let raw = encode_raw_message("a@b.co", "Hi", "body text");
        let decoded = String::from_utf8(base64url_decode(&raw)).unwrap();
        assert!(decoded.starts_with("To: a@b.co\r\n"));
        assert!(decoded.contains("Subject: Hi\r\n"));
        assert!(decoded.ends_with("\r\n\r\nbody text"));
    }

    #[test]
    fn sanitize_header_strips_crlf() {
        assert_eq!(sanitize_header("plain"), "plain");
        // Each CR and each LF becomes a space, so "\r\n" collapses to two
        // spaces — the point is only that no CR/LF survives to start a new
        // header line, not that whitespace is minimized.
        assert_eq!(sanitize_header("a\r\nBcc: x"), "a  Bcc: x");
        assert!(!sanitize_header("a\r\nBcc: x").contains('\n'));
        assert!(!sanitize_header("a\r\nBcc: x").contains('\r'));
        assert_eq!(sanitize_header("a\nb"), "a b");
    }

    #[test]
    fn encode_query_keeps_unreserved_encodes_the_rest() {
        assert_eq!(encode_query("is-unread"), "is-unread");
        assert_eq!(encode_query("is:unread from:a@b"), "is%3Aunread%20from%3Aa%40b");
    }

    #[test]
    fn first_line_takes_first_nonempty_and_caps() {
        assert_eq!(first_line("hello\nworld"), "hello");
        assert_eq!(first_line("\n\n  second\nthird"), "second");
        let long = "x".repeat(200);
        assert!(first_line(&long).ends_with('…'));
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(401, "x").unwrap_err().to_string().contains("reconnect Google"));
        assert!(map_status(403, "x").unwrap_err().to_string().contains("scope"));
        assert!(map_status(404, "x").unwrap_err().to_string().contains("not found"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(500, "x").unwrap_err().to_string().contains("transient"));
    }
}
