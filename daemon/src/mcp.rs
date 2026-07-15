//! Model Context Protocol (MCP) CLIENT CORE.
//!
//! Connects DARWIN to external MCP tool servers — local subprocesses speaking
//! JSON-RPC 2.0 over newline-delimited stdio (the primary local transport), or a
//! remote HTTPS endpoint speaking MCP Streamable-HTTP/SSE — discovers their tools (`tools/list`) and
//! invokes them (`tools/call`), bounded and safe. This is the MOST DANGEROUS
//! external surface in DARWIN: an MCP server runs code on the user's machine and
//! offers tools an agent can call. The whole module is built to be SAFE BY
//! DEFAULT:
//!
//!   * SHIPS OFF. `[mcp].enabled = false` is the shipped default; with it false
//!     [`McpManager::connect_all`] connects to NOTHING and [`McpManager::tools`]
//!     is empty. No server connects, no tool exists. (config.rs + a pinned test.)
//!
//!   * GATED. Every discovered tool carries a [`ToolClass`]; an unknown/mutating
//!     tool is CONSEQUENTIAL (fail-safe). [`McpManager::call_tool`] takes an
//!     [`ActionMode`] (from `integrations::gate`): a consequential tool returns a
//!     DRY-RUN PREVIEW unless the master switch is on AND the call confirmed. So
//!     even with the subsystem enabled, an MCP tool can never auto-mutate without
//!     a spoken yes routed through the existing confirmation gate.
//!
//!   * PER-AGENT ALLOWLISTED. Each server config lists which DARWIN agents may
//!     use it; the orchestrator is always admitted, every other agent only if
//!     explicitly listed — NEVER an auto-grant of all agents.
//!
//!   * BOUNDED. Per-call timeout, output-size cap, max servers, max tools/server.
//!     A slow / oversized / malformed server response is rejected and never hangs
//!     the tool loop.
//!
//!   * SANDBOXED (stdio) / TLS-TRUSTED (http). For a LOCAL stdio server,
//!     [`stdio_sandbox_profile`] derives a default-deny seatbelt (SBPL) profile —
//!     reusing apps.rs's `(deny default)` + `bsd.sb` base + `sbpl_str` quoting —
//!     that grants ONLY the exec, the filesystem subpaths, and the network
//!     host-names the server's config declares. A REMOTE http server CANNOT be
//!     SBPL-sandboxed (it runs elsewhere — there is no local process to wrap);
//!     its layers are instead TLS (https-only), Keychain bearer auth, the bounded
//!     SSE read, and the SAME gate + per-agent allowlist + per-call bounds as
//!     stdio. We do NOT claim a remote server is sandboxed — see the honest
//!     residual-trust note on [`HttpTransport`].
//!
//!   * SECRET-CLEAN. A server's optional token resolves from the Keychain
//!     (`mcp_<name>_token`, via `integrations::resolve_secret`) at call time and
//!     is NEVER logged, in Debug, on argv, or in a URL.
//!
//! INJECTABLE TRANSPORT. [`McpTransport`] is the seam: tests wire a
//! [`testing::MockTransport`] (canned JSON-RPC, records requests) and production
//! wires [`StdioTransport`] (tokio::process) or [`HttpTransport`] (reqwest, MCP
//! Streamable-HTTP/SSE). NO test spawns a real subprocess or touches the network;
//! the http transport's network leg is runtime-gated and its SSE/JSON-RPC reply
//! parsing is a PURE function ([`parse_sse_events`] / [`extract_rpc_response`])
//! unit-tested with canned bytes.
//!
//! Like the integration foundation, most of this public surface is consumed by
//! the cloud tool-loop wiring that lands alongside it; until that is in place the
//! unused-public-item lint would flag the API, so dead_code is allowed
//! module-wide — the same "shared contract another component reads" rationale
//! integrations/mod.rs uses.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::apps::{sbpl_str, BSD_BASE_PROFILE};
use crate::config::{McpConfig, McpServerConfig, McpToolClass, McpTransportKind};
use crate::integrations::{self, mcp_token_account, ActionMode};

/// The MCP protocol version DARWIN's client speaks. Sent in `initialize`. MCP
/// versions are date-stamped; this is the revision the lifecycle below targets.
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// The orchestrator agent id. Always admitted to every configured server (it is
/// the delegation fallback + tool owner); every OTHER agent must be on a server's
/// `agents` allowlist. Kept in lockstep with agents.rs by a test.
const ORCHESTRATOR: &str = "darwin";

// ===========================================================================
// Result / classification types
// ===========================================================================

/// MCP-layer result. Thin over `anyhow` so the manager can `?` over JSON-RPC /
/// serde / transport errors while presenting one type. Display never carries a
/// secret — callers keep tokens out of any attached context.
pub type McpResult<T> = anyhow::Result<T>;

/// Read-only vs consequential, the classification every discovered tool carries.
/// Mirrors `confirm::is_consequential_tool` for the static built-in tools, but
/// for MCP it is resolved at DISCOVERY time from the server config (unknown ->
/// consequential, fail-safe).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolClass {
    /// Safe to call ungated — the server config asserted this tool read-only.
    ReadOnly,
    /// Side-effecting (or unknown): parks behind the confirmation gate + the
    /// `[integrations].allow_consequential` master switch.
    Consequential,
}

impl ToolClass {
    /// Is this tool side-effecting (and therefore gated)?
    pub fn is_consequential(self) -> bool {
        matches!(self, ToolClass::Consequential)
    }

    /// Map a config default-class to a runtime class.
    fn from_config(c: McpToolClass) -> Self {
        match c {
            McpToolClass::ReadOnly => ToolClass::ReadOnly,
            McpToolClass::Consequential => ToolClass::Consequential,
        }
    }
}

/// One tool discovered on a server: its name, description, JSON-Schema for its
/// arguments, the server it belongs to, and its safety classification. The
/// classification is resolved ONCE at discovery from the server config — a tool
/// the config did not assert read-only is consequential (fail-safe), so a server
/// can never sneak a mutating tool past the gate by omitting it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredTool {
    /// The server this tool lives on (the config `name`).
    pub server: String,
    /// The MCP tool name (as the server reports it).
    pub name: String,
    /// Human/agent-facing description (may be empty if the server omits one).
    pub description: String,
    /// The tool's argument JSON-Schema, verbatim from `inputSchema`. `Null` when
    /// the server omits it.
    pub input_schema: Value,
    /// Safety classification, fail-safe (unknown -> consequential).
    pub class: ToolClass,
}

impl DiscoveredTool {
    /// The fully-qualified id the tool loop addresses this tool by: `mcp.<server>.<tool>`.
    /// Namespaced so an MCP tool can never collide with a built-in tool name and
    /// the server is always recoverable from the id.
    pub fn qualified_name(&self) -> String {
        format!("mcp.{}.{}", self.server, self.name)
    }

    /// Is this tool side-effecting (gated)?
    pub fn is_consequential(&self) -> bool {
        self.class.is_consequential()
    }
}

/// The outcome of a `tools/call`, mapped into DARWIN-friendly language. Mirrors
/// the integration-client outcome shape so the tool loop renders MCP results the
/// same way it renders a built-in tool's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallOutcome {
    /// The tool ran and returned text content.
    Ok(String),
    /// A consequential tool was NOT executed (gate off / unconfirmed); this is
    /// the dry-run preview to surface (and, via the confirmation layer, park).
    DryRun(String),
    /// The tool ran but the server flagged the result an error (`isError: true`),
    /// or returned a JSON-RPC error. The string is secret-free + spoken-friendly.
    ToolError(String),
}

// ===========================================================================
// JSON-RPC framing
// ===========================================================================

/// A monotonically increasing JSON-RPC request id source. Per-client so ids are
/// unique within a connection; the responder/mock echoes the id back.
#[derive(Debug, Default)]
struct IdGen(AtomicI64);

impl IdGen {
    fn next(&self) -> i64 {
        self.0.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Build a JSON-RPC 2.0 request object.
fn rpc_request(id: i64, method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    })
}

/// Build a JSON-RPC 2.0 notification (no id — no response expected).
fn rpc_notification(method: &str, params: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    })
}

/// Parse a JSON-RPC response, returning the `result` value or mapping an `error`
/// object to a friendly `Err`. Enforces the response is a JSON-RPC 2.0 object;
/// the server's raw error message is NOT echoed verbatim into the friendly text
/// (a server could put anything there) — only its code drives the phrasing.
fn parse_rpc_result(resp: &Value) -> McpResult<Value> {
    if resp.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        bail!("server reply is not JSON-RPC 2.0");
    }
    if let Some(err) = resp.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
        // Friendly phrasing keyed by code only — never the server's free-text.
        let phrase = match code {
            -32700 => "the server could not parse the request",
            -32600 => "the server rejected the request as invalid",
            -32601 => "the server does not offer that method",
            -32602 => "the server rejected the arguments",
            -32603 => "the server hit an internal error",
            _ => "the server returned an error",
        };
        bail!("{phrase}");
    }
    resp.get("result")
        .cloned()
        .ok_or_else(|| anyhow!("server reply has neither result nor error"))
}

// ===========================================================================
// Transport seam
// ===========================================================================

/// Boxed-future alias for the object-safe transport (the crate avoids
/// `async_trait`; same shape it would desugar to), so a transport is usable as
/// `Box<dyn McpTransport>`.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The injectable MCP transport. One connection = one transport. `request` sends
/// a JSON-RPC request object and resolves the response object; `notify` sends a
/// notification (no reply). Production wires [`StdioTransport`] /
/// [`HttpTransport`]; tests wire [`testing::MockTransport`].
///
/// Transport-level failures (subprocess died, IO error) surface as `Err`. A
/// JSON-RPC error OBJECT is a successful round-trip and comes back inside the
/// `Ok(Value)` for [`parse_rpc_result`] to classify — same split as the HTTP
/// transport's status handling.
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and resolve its response object.
    fn request<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<Value>>;
    /// Send a JSON-RPC notification (fire-and-forget; no response).
    fn notify<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<()>>;
}

// ===========================================================================
// Production: StdioTransport (tokio::process, newline-delimited JSON-RPC)
// ===========================================================================

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex as AsyncMutex;

/// Production stdio transport: a spawned subprocess whose stdin/stdout carry
/// newline-delimited JSON-RPC. Holds the child + framed handles behind an async
/// mutex so concurrent calls serialize on the single pipe pair.
///
/// SPAWNING IS RUNTIME-GATED: [`spawn`] is the ONLY path that launches a real
/// process, it is never reached from a test (tests use the mock), and the daemon
/// only reaches it when `[mcp].enabled` is true AND a server is configured. The
/// command is wrapped by `sandbox-exec -f <profile>` (default-deny) at the call
/// site in the manager, so the child runs sandboxed.
pub struct StdioTransport {
    inner: AsyncMutex<StdioInner>,
    max_output_bytes: usize,
}

struct StdioInner {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl StdioTransport {
    /// Spawn `command argv...` (already wrapped in sandbox-exec by the manager),
    /// piping stdin/stdout. RUNTIME-ONLY — never called from a test. Bounded
    /// output is enforced per read in [`Self::request`].
    pub async fn spawn(
        command: &str,
        args: &[String],
        max_output_bytes: usize,
    ) -> McpResult<Self> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // stderr inherits the daemon's null/log; the server's stderr is NOT
            // parsed as protocol and is never echoed into a reply.
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().context("spawning MCP stdio server")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no child stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no child stdout"))?;
        Ok(Self {
            inner: AsyncMutex::new(StdioInner {
                child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            max_output_bytes,
        })
    }

    /// Write one newline-delimited JSON message to the child's stdin.
    async fn write_line(inner: &mut StdioInner, message: &Value) -> McpResult<()> {
        let mut line = serde_json::to_string(message).context("serializing JSON-RPC message")?;
        line.push('\n');
        inner
            .stdin
            .write_all(line.as_bytes())
            .await
            .context("writing to MCP server stdin")?;
        inner.stdin.flush().await.context("flushing MCP server stdin")?;
        Ok(())
    }

    /// Read one newline-delimited JSON message from the child's stdout, rejecting
    /// a line larger than `max` (the oversize bound) so a flooding server can
    /// never make the daemon buffer unbounded data. Pulls from the `BufReader`'s
    /// filled buffer in chunks and stops the instant the accumulated line would
    /// exceed the cap — we never allocate past `max + 1` bytes for one line.
    async fn read_line(inner: &mut StdioInner, max: usize) -> McpResult<Value> {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let available = inner
                .stdout
                .fill_buf()
                .await
                .context("reading from MCP server stdout")?;
            if available.is_empty() {
                // EOF before any newline.
                if buf.is_empty() {
                    bail!("MCP server closed the connection");
                }
                bail!("MCP server sent a truncated line");
            }
            // Consume up to the newline (inclusive) or the whole chunk.
            let (chunk, found_newline) = match available.iter().position(|&b| b == b'\n') {
                Some(i) => (&available[..=i], true),
                None => (available, false),
            };
            let take = chunk.len();
            // Enforce the cap BEFORE growing the buffer.
            if buf.len() + take > max {
                bail!("MCP server response exceeded the output-size cap");
            }
            buf.extend_from_slice(chunk);
            inner.stdout.consume(take);
            if found_newline {
                break;
            }
        }
        serde_json::from_slice(&buf).context("MCP server sent malformed JSON")
    }
}

impl McpTransport for StdioTransport {
    fn request<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<Value>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::write_line(&mut inner, &message).await?;
            Self::read_line(&mut inner, self.max_output_bytes).await
        })
    }

    fn notify<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<()>> {
        Box::pin(async move {
            let mut inner = self.inner.lock().await;
            Self::write_line(&mut inner, &message).await
        })
    }
}

// ===========================================================================
// Production: HttpTransport (MCP Streamable-HTTP / SSE)
// ===========================================================================
//
// REMOTE SAFETY MODEL — read this honestly. A remote MCP server runs on someone
// else's machine, so it CANNOT be SBPL-sandboxed the way a stdio subprocess is:
// there is no local process to wrap in seatbelt. The protections for a remote
// server are therefore a DIFFERENT (still layered, still default-deny) set:
//
//   * TLS-ONLY. The url MUST be `https://` — rejected at config/connect time
//     ([`HttpTransport::new`]) so a bearer token never rides a plaintext wire.
//   * KEYCHAIN BEARER AUTH. The token resolves from the Keychain
//     (`mcp_<server>_token`) and rides ONLY the `Authorization: Bearer` header —
//     never the URL, never a log line, never Debug.
//   * THE SAME GATE + ALLOWLIST + BOUNDS as stdio. An http server connects
//     through the identical `McpClient` handshake -> tools/list -> per-agent
//     allowlist + consequential-park + per-call timeout / output cap. A
//     consequential remote tool parks behind the confirmation gate exactly like
//     a local one.
//   * BOUNDED SSE. A streamed (text/event-stream) reply is read under a hard cap
//     on events, total bytes, and the per-call timeout, so a hostile or slow
//     remote can neither hang nor flood the daemon.
//
// What we DO NOT claim: we do NOT claim a remote server is sandboxed or that its
// operator is trustworthy. Ultimately you TRUST the remote operator with the
// arguments you send and the results you receive; the layers above bound the
// blast radius and keep the secret clean, they do not neutralize a malicious
// operator. That residual trust is the honest cost of a remote tool.

/// Hard ceiling on SSE events read for one request before we abandon the stream —
/// a hostile server can't make us spin forever waiting for a matching id.
const SSE_MAX_EVENTS: usize = 1024;

/// MCP Streamable-HTTP / SSE transport: a `reqwest`-backed client that POSTs one
/// JSON-RPC message per call and parses EITHER an `application/json` reply OR a
/// `text/event-stream` (SSE) body, bounded.
///
/// NETWORK IS RUNTIME-GATED: the only code that touches the wire is
/// [`Self::post`] (reqwest send + body read); it is never reached from a test
/// (tests drive [`McpClient`]/[`McpManager`] via [`testing::MockTransport`] and
/// unit-test the pure [`parse_sse_events`] / [`extract_rpc_response`] parsers with
/// canned bytes). The daemon only reaches it when `[mcp].enabled` is true AND an
/// `http` server is configured with an `https://` url.
pub struct HttpTransport {
    /// The endpoint URL — validated `https://` at construction. NEVER carries the
    /// token (which is header-only).
    url: String,
    client: reqwest::Client,
    /// Resolved bearer token, if the server declares one. Held only for this
    /// connection's lifetime; rides the `Authorization` header exclusively and is
    /// never logged / Debugged / placed in the URL.
    token: Option<String>,
    /// Per-call timeout — bounds both the connect/send and the SSE read.
    timeout: Duration,
    /// Output-size cap (bytes) on any single response body (json or the summed SSE
    /// data), so an oversized remote reply is a friendly Err, never buffered whole.
    max_output_bytes: usize,
    /// The session id the server handed back in `Mcp-Session-Id`, echoed on every
    /// subsequent request. `Mutex` because the trait takes `&self`.
    session_id: std::sync::Mutex<Option<String>>,
}

impl HttpTransport {
    /// Build an HTTPS transport. REJECTS a non-`https://` url (TLS-only: a bearer
    /// token must never ride plaintext). `token` is the already-resolved Keychain
    /// secret (or `None`); it is stored only to ride the `Authorization` header.
    pub fn new(
        url: impl Into<String>,
        token: Option<String>,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> McpResult<Self> {
        let url = url.into();
        if !is_https_url(&url) {
            // Do NOT echo the url (it could carry a path a log shouldn't keep);
            // the friendly message names the rule, not the value.
            bail!("MCP http server url must be https:// (plaintext is refused so a token never rides the wire)");
        }
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // No proxy auto-config surprises; explicit https only.
            .https_only(true)
            .build()
            .context("building MCP http client")?;
        Ok(Self {
            url,
            client,
            token,
            timeout,
            max_output_bytes,
            session_id: std::sync::Mutex::new(None),
        })
    }

    /// The `Mcp-Session-Id` value to echo on the NEXT request: whatever the server
    /// last minted (via [`Self::capture_session_id`]), or `None` before any session
    /// is established. PURE w.r.t. the network (touches only the session mutex), so
    /// the echo-if-present behavior is unit-testable without a wire call.
    fn session_id_to_echo(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }

    /// Capture a session id the server handed back in its `Mcp-Session-Id` response
    /// header. A present value is stored to echo next time; `None` (the server sent
    /// no such header) LEAVES any existing session untouched — a server that mints a
    /// session once need not repeat it on every reply. PURE w.r.t. the network.
    fn capture_session_id(&self, header: Option<&str>) {
        if let Some(sid) = header {
            *self.session_id.lock().unwrap() = Some(sid.to_string());
        }
    }

    /// POST one JSON-RPC `message` and return the raw `(content_type, body_bytes,
    /// session_id_header)`. RUNTIME-ONLY: the single network leg. The token rides
    /// the `Authorization` header (never the URL); the body read is capped at
    /// `max_output_bytes`. Errors are mapped to secret-free messages.
    async fn post(&self, message: &Value) -> McpResult<HttpReply> {
        let body = serde_json::to_vec(message).context("serializing JSON-RPC message")?;
        let mut req = self
            .client
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            // Tell the server we accept BOTH reply modes.
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .body(body);
        if let Some(tok) = &self.token {
            // Header-only. Never the URL, never a log.
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {tok}"));
        }
        if let Some(sid) = self.session_id_to_echo() {
            req = req.header("Mcp-Session-Id", sid);
        }

        let resp = req.send().await.map_err(map_reqwest_err)?;
        let status = resp.status();

        // Capture a session id the server may have minted, to echo next time.
        self.capture_session_id(
            resp.headers()
                .get("Mcp-Session-Id")
                .and_then(|v| v.to_str().ok()),
        );

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if !status.is_success() {
            // 4xx/5xx -> friendly, secret-free. We do NOT echo the body (it could
            // mirror back the Authorization header or other sensitive context).
            bail!("the MCP server returned HTTP {}", status.as_u16());
        }

        // Read the body under the size cap (defensive: also honored against
        // Content-Length-less chunked bodies because we count as we read).
        let bytes = read_body_capped(resp, self.max_output_bytes).await?;
        Ok(HttpReply { content_type, bytes })
    }

    /// Drive `post` and resolve the JSON-RPC response object for `message`'s id,
    /// parsing whichever reply mode the server used. Bounded throughout. The
    /// reply-mode DISPATCH is the pure [`select_reply`] (unit-tested with canned
    /// bytes); this method is only the network leg (`post`) plus that call.
    async fn round_trip(&self, message: Value) -> McpResult<Value> {
        let want_id = message.get("id").cloned().unwrap_or(Value::Null);
        let reply = self.post(&message).await?;
        select_reply(&reply, &want_id, self.max_output_bytes)
    }
}

/// Resolve the JSON-RPC response object from a raw HTTP reply, dispatching on its
/// content type: a `text/event-stream` body is parsed by the bounded SSE parser
/// and the response matching `want_id` is extracted; ANY other content type
/// (the common `application/json` single-reply mode, or an unlabeled body) is
/// parsed as one JSON-RPC object. PURE — operates on the already-read
/// [`HttpReply`] value, touches NO network, so the json single-reply path AND the
/// SSE-to-`want_id` wiring are unit-testable with canned bytes (the network leg,
/// [`HttpTransport::post`], is runtime-only and tested via the mock seam at the
/// `McpTransport` level instead).
fn select_reply(reply: &HttpReply, want_id: &Value, max_bytes: usize) -> McpResult<Value> {
    if reply.content_type.contains("text/event-stream") {
        // SSE: parse the canned-shaped event list with the pure parser, then
        // extract the response whose id matches our request.
        let events = parse_sse_events(&reply.bytes, max_bytes, SSE_MAX_EVENTS)?;
        extract_rpc_response(&events, want_id)
    } else {
        // Default to application/json (the common, single-reply mode). An
        // empty/garbage body fails the JSON parse with a friendly message.
        serde_json::from_slice(&reply.bytes).context("MCP server sent malformed JSON")
    }
}

impl McpTransport for HttpTransport {
    fn request<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<Value>> {
        Box::pin(async move { self.round_trip(message).await })
    }
    fn notify<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<()>> {
        // A notification has no id; we POST it and ignore the (commonly 202-empty)
        // body. Per-call bounds still apply via the client timeout.
        Box::pin(async move {
            let _ = self.post(&message).await?;
            Ok(())
        })
    }
}

/// A raw HTTP reply: the (lowercased) content type and the capped body bytes.
struct HttpReply {
    content_type: String,
    bytes: Vec<u8>,
}

/// Is `url` an `https://` URL? Case-insensitive on the scheme. The ONLY accepted
/// scheme — `http://`, `ws://`, a bare host, etc. are all rejected so a token can
/// never ride a plaintext wire. Pure, so it is unit-tested directly.
pub fn is_https_url(url: &str) -> bool {
    let scheme_end = match url.find("://") {
        Some(i) => i,
        None => return false,
    };
    url[..scheme_end].eq_ignore_ascii_case("https")
}

/// Map a `reqwest` error to a friendly, SECRET-FREE message. `reqwest`'s Display
/// can include the URL; we deliberately do NOT forward it (the url may carry a
/// path, and we never want a token-adjacent value in a log) — we classify by kind
/// instead.
fn map_reqwest_err(e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow!("the MCP server did not respond in time")
    } else if e.is_connect() {
        anyhow!("could not connect to the MCP server")
    } else if e.is_request() {
        anyhow!("the MCP request could not be sent")
    } else {
        anyhow!("the MCP server connection failed")
    }
}

/// Read a response body, abandoning it the instant the accumulated bytes would
/// exceed `max` — so an oversized (or Content-Length-lying / chunked-flooding)
/// remote body is a friendly Err, never buffered whole. Streams chunks so we
/// never allocate past `max + one chunk`.
async fn read_body_capped(resp: reqwest::Response, max: usize) -> McpResult<Vec<u8>> {
    use futures_util::StreamExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_reqwest_err)?;
        if buf.len() + chunk.len() > max {
            bail!("the MCP server response exceeded the output-size cap");
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

// ---------------------------------------------------------------------------
// PURE SSE / JSON-RPC response parsing (unit-tested with canned bytes, no net)
// ---------------------------------------------------------------------------

/// Parse a Server-Sent-Events byte stream into the JSON values carried on its
/// `data:` lines. PURE — takes canned bytes, returns the decoded events, touches
/// NO network. This is the seam the hermetic SSE tests drive directly.
///
/// SSE framing per the WHATWG spec subset MCP uses: events are separated by a
/// blank line; within an event, one or more `data:` lines (optionally with a
/// leading space after the colon) are joined with `\n`; non-`data:` fields
/// (`event:`, `id:`, `:comment`, `retry:`) are ignored. The joined data of each
/// event is parsed as a JSON value (an MCP SSE stream carries one JSON-RPC
/// message per event).
///
/// BOUNDED: rejects (friendly Err) once the scanned bytes exceed `max_bytes` or
/// the event count would exceed `max_events`, so a hostile/oversize/never-blank
/// stream can neither flood memory nor spin. A `data:` payload that is not valid
/// JSON is skipped (a keep-alive comment or a partial frame is not fatal); a
/// stream with NO parseable event yields an empty Vec (the caller reports "no
/// matching response").
pub fn parse_sse_events(bytes: &[u8], max_bytes: usize, max_events: usize) -> McpResult<Vec<Value>> {
    if bytes.len() > max_bytes {
        bail!("the MCP SSE stream exceeded the output-size cap");
    }
    // SSE is UTF-8; a non-UTF-8 stream is malformed.
    let text = std::str::from_utf8(bytes).map_err(|_| anyhow!("MCP SSE stream is not valid UTF-8"))?;

    let mut events: Vec<Value> = Vec::new();
    let mut data_lines: Vec<&str> = Vec::new();

    // Flush the accumulated data lines of one event into a parsed JSON value.
    // Returns Err only when the event cap is exceeded.
    let flush = |data_lines: &mut Vec<&str>, events: &mut Vec<Value>| -> McpResult<()> {
        if data_lines.is_empty() {
            return Ok(());
        }
        let joined = data_lines.join("\n");
        data_lines.clear();
        // Only count + keep events that parse as JSON; a keep-alive / partial is
        // silently skipped rather than aborting the whole stream.
        if let Ok(v) = serde_json::from_str::<Value>(&joined) {
            if events.len() >= max_events {
                bail!("the MCP SSE stream exceeded the event cap");
            }
            events.push(v);
        }
        Ok(())
    };

    // Normalize CRLF/CR to LF-style line iteration: split on '\n', strip a
    // trailing '\r' so a CRLF stream parses identically.
    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.is_empty() {
            // Blank line: end of one event.
            flush(&mut data_lines, &mut events)?;
            continue;
        }
        if line.starts_with(':') {
            // A comment / keep-alive line — ignored.
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            // One space after the colon is part of the framing, not the data.
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // Any other field (event:, id:, retry:, unknown) is ignored.
    }
    // A final event with no trailing blank line is still a complete event.
    flush(&mut data_lines, &mut events)?;
    Ok(events)
}

/// From the events of an SSE stream, pick the JSON-RPC RESPONSE whose `id` matches
/// `want_id` (the id we sent). PURE. An SSE stream may interleave server-initiated
/// requests/notifications (no/other id) with our response; we select OURS by id.
/// When no event matches, that is a friendly Err (the caller surfaces "the server
/// did not return a matching response") — never a hang.
pub fn extract_rpc_response(events: &[Value], want_id: &Value) -> McpResult<Value> {
    for ev in events {
        // A JSON-RPC response carries our id and either result or error.
        if ev.get("id") == Some(want_id)
            && (ev.get("result").is_some() || ev.get("error").is_some())
        {
            return Ok(ev.clone());
        }
    }
    bail!("the MCP server did not return a matching response on the event stream")
}

// ===========================================================================
// Client: one connection's lifecycle + tool surface
// ===========================================================================

/// A single connected MCP server: its config, its transport, and the tools it
/// discovered. Created by [`McpClient::handshake`] (lifecycle: initialize ->
/// initialized) and queried by the manager. The token (if any) is resolved fresh
/// per `tools/call` and never stored on this struct.
pub struct McpClient {
    name: String,
    transport: Box<dyn McpTransport>,
    ids: IdGen,
    timeout: Duration,
    max_tools: usize,
    default_class: ToolClass,
    read_only: Vec<String>,
    tools: Vec<DiscoveredTool>,
}

impl McpClient {
    /// Run the MCP lifecycle over `transport` and return a ready client.
    ///
    /// 1. `initialize` — send our `protocolVersion` + client capabilities + info,
    ///    read the server's capability reply (we only require it parses).
    /// 2. `initialized` notification — tell the server the handshake is complete.
    ///
    /// Every send is bounded by `timeout` so a server that never answers the
    /// handshake cannot wedge connect. `default_class` / `read_only` come from the
    /// server config and drive tool classification at discovery.
    pub async fn handshake(
        name: impl Into<String>,
        transport: Box<dyn McpTransport>,
        timeout: Duration,
        max_tools: usize,
        default_class: ToolClass,
        read_only: Vec<String>,
    ) -> McpResult<Self> {
        let name = name.into();
        let ids = IdGen::default();

        // (1) initialize
        let init_id = ids.next();
        let init = rpc_request(
            init_id,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "clientInfo": { "name": "darwin", "version": env!("CARGO_PKG_VERSION") },
            }),
        );
        let reply = bounded(timeout, transport.request(init))
            .await
            .context("MCP initialize")?;
        // We don't pin the server's protocolVersion (servers may negotiate down);
        // we only require the reply is a well-formed JSON-RPC result.
        let _server_caps = parse_rpc_result(&reply).context("MCP initialize reply")?;

        // (2) initialized notification
        bounded(timeout, transport.notify(rpc_notification("notifications/initialized", json!({}))))
            .await
            .context("MCP initialized notification")?;

        info!(server = %name, "mcp: handshake complete");
        Ok(Self {
            name,
            transport,
            ids,
            timeout,
            max_tools,
            default_class,
            read_only,
            tools: Vec::new(),
        })
    }

    /// `tools/list`: enumerate the server's tools, parse each `{name, description,
    /// inputSchema}`, classify it (read-only iff named in the config's
    /// `read_only_tools`, else `default_class` — unknown therefore consequential
    /// when the default is consequential), and TRUNCATE to `max_tools` (the
    /// per-server bound — a server flooding the list cannot register unboundedly).
    /// Caches the result on the client and returns a borrow.
    pub async fn list_tools(&mut self) -> McpResult<&[DiscoveredTool]> {
        let id = self.ids.next();
        let reply = bounded(self.timeout, self.transport.request(rpc_request(id, "tools/list", json!({}))))
            .await
            .context("MCP tools/list")?;
        let result = parse_rpc_result(&reply).context("MCP tools/list reply")?;
        let raw = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("tools/list reply has no tools array"))?;

        let mut tools = Vec::new();
        for t in raw {
            let tname = match t.get("name").and_then(Value::as_str) {
                Some(n) if !n.is_empty() => n.to_string(),
                // A nameless tool is unusable — skip it rather than register a
                // tool the loop could never address.
                _ => {
                    warn!(server = %self.name, "mcp: skipping tool with no name");
                    continue;
                }
            };
            let description = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let input_schema = t.get("inputSchema").cloned().unwrap_or(Value::Null);
            let class = if self.read_only.iter().any(|r| r == &tname) {
                ToolClass::ReadOnly
            } else {
                // Not asserted read-only -> the server default (consequential
                // unless the config widened it). Unknown is therefore gated.
                self.default_class
            };
            tools.push(DiscoveredTool {
                server: self.name.clone(),
                name: tname,
                description,
                input_schema,
                class,
            });
            if tools.len() >= self.max_tools {
                warn!(
                    server = %self.name,
                    cap = self.max_tools,
                    "mcp: tools/list truncated at the per-server cap"
                );
                break;
            }
        }
        self.tools = tools;
        Ok(&self.tools)
    }

    /// The tools discovered on this server (after [`Self::list_tools`]).
    pub fn tools(&self) -> &[DiscoveredTool] {
        &self.tools
    }

    /// The server's config name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Look up a discovered tool by its bare name.
    fn find_tool(&self, tool: &str) -> Option<&DiscoveredTool> {
        self.tools.iter().find(|t| t.name == tool)
    }

    /// `tools/call`: invoke `tool` with `arguments`, honoring `mode`.
    ///
    /// A CONSEQUENTIAL tool in `DryRun` mode performs NO call — it returns a
    /// faithful preview ([`CallOutcome::DryRun`]) the confirmation layer parks.
    /// Only `Execute` (which the gate returns ONLY when the master switch is on
    /// AND the call confirmed) actually sends the request. A read-only tool is
    /// never gated. The round-trip is bounded by the per-call timeout; an
    /// oversized/malformed reply is rejected by the transport.
    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        mode: ActionMode,
    ) -> McpResult<CallOutcome> {
        let discovered = self
            .find_tool(tool)
            .ok_or_else(|| anyhow!("unknown MCP tool {tool} on server {}", self.name))?;

        // Gate: a consequential tool not in Execute mode is a dry run.
        if discovered.is_consequential() && mode != ActionMode::Execute {
            return Ok(CallOutcome::DryRun(format!(
                "[dry run] would call MCP tool {} on {} with the provided arguments. \
                 Enable consequential actions and confirm to run it.",
                tool, self.name
            )));
        }

        let id = self.ids.next();
        let req = rpc_request(
            id,
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        );
        let reply = bounded(self.timeout, self.transport.request(req))
            .await
            .context("MCP tools/call")?;
        // A JSON-RPC error object -> a friendly tool error (not a hard Err, so the
        // loop can surface it to the user as a result).
        let result = match parse_rpc_result(&reply) {
            Ok(r) => r,
            Err(e) => return Ok(CallOutcome::ToolError(e.to_string())),
        };
        Ok(render_call_result(&result))
    }
}

/// Render a `tools/call` `result` into a DARWIN-friendly outcome. MCP returns
/// `{ content: [{type, text|...}], isError? }`. We concatenate the TEXT parts
/// (the spoken-friendly payload), label non-text parts by kind (we don't inline
/// binary blobs), and honor `isError: true` as a tool error.
fn render_call_result(result: &Value) -> CallOutcome {
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for c in content {
            match c.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        parts.push(t.to_string());
                    }
                }
                Some(other) => parts.push(format!("[{other} content]")),
                None => {}
            }
        }
    }
    let text = if parts.is_empty() {
        "the tool returned no text content".to_string()
    } else {
        parts.join("\n")
    };
    if is_error {
        CallOutcome::ToolError(text)
    } else {
        CallOutcome::Ok(text)
    }
}

/// Apply the per-call timeout to a transport future. A timeout is a friendly Err
/// — the loop never hangs on a slow server.
async fn bounded<T>(timeout: Duration, fut: impl std::future::Future<Output = McpResult<T>>) -> McpResult<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(r) => r,
        Err(_) => bail!("the MCP server did not respond in time"),
    }
}

// ===========================================================================
// Connection manager
// ===========================================================================

/// Manages every connected MCP server: applies the master switch + bounds at
/// connect, holds the live clients, aggregates their tools, enforces the
/// per-agent allowlist, and routes `call_tool` through the gate.
///
/// INERT WHEN DISABLED: [`McpManager::new`] with `[mcp].enabled = false` builds a
/// manager that connects to nothing and offers no tools — the shipped state.
pub struct McpManager {
    cfg: McpConfig,
    /// Connected clients, keyed by server name. Empty when disabled or when no
    /// server is configured/reachable.
    clients: BTreeMap<String, Arc<McpClient>>,
}

impl McpManager {
    /// Build a manager from config. Does NOT connect — call [`Self::connect_all`]
    /// (or, in tests, [`Self::insert_client`]) to populate it. Cheap + pure, so a
    /// disabled manager is trivially constructed and verified inert.
    pub fn new(cfg: McpConfig) -> Self {
        Self {
            cfg,
            clients: BTreeMap::new(),
        }
    }

    /// Is the subsystem enabled? FORCED false while the emergency stop is engaged
    /// (task #12 lockdown overlay), so every reader of the master gate sees the
    /// MCP host OFF when locked. With lockdown OFF this is byte-for-byte
    /// `self.cfg.enabled`.
    pub fn enabled(&self) -> bool {
        self.cfg.enabled && !crate::lockdown::is_locked_down()
    }

    /// The servers config admits to connect, after the master switch + bounds:
    /// empty when disabled; otherwise the first `max_servers` configured servers
    /// whose names are valid. Pure (no IO), so the gating is unit-testable without
    /// spawning anything.
    pub fn connectable_servers(&self) -> Vec<&McpServerConfig> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        self.cfg
            .servers
            .iter()
            .filter(|s| mcp_token_account(&s.name).is_some()) // valid name shape
            .take(self.cfg.max_servers)
            .collect()
    }

    /// Connect every connectable server: spawn its transport (stdio: sandboxed
    /// subprocess; http: TLS-only reqwest transport), run the handshake, discover
    /// its tools.
    ///
    /// RUNTIME-ONLY: this is the ONLY method that spawns a real process / opens a
    /// real transport, it is never called from a test (tests use
    /// [`Self::insert_client`] with a mock-backed client), and it is a no-op when
    /// disabled. A single server failing to connect is logged and skipped — one
    /// bad server never blocks the rest.
    pub async fn connect_all(&mut self, project_root: &Path) -> McpResult<()> {
        if !self.cfg.enabled {
            info!("mcp: disabled — no server connected");
            return Ok(());
        }
        let timeout = Duration::from_millis(self.cfg.call_timeout_ms);
        let max_tools = self.cfg.max_tools_per_server;
        let max_out = self.cfg.max_output_bytes;
        // Clone the connectable configs so we don't hold an immutable borrow of
        // self.cfg across the mutable self.clients insert.
        let targets: Vec<McpServerConfig> =
            self.connectable_servers().into_iter().cloned().collect();
        for s in targets {
            match Self::connect_one(&s, project_root, timeout, max_tools, max_out).await {
                Ok(client) => {
                    info!(server = %s.name, tools = client.tools().len(), "mcp: server connected");
                    self.clients.insert(s.name.clone(), Arc::new(client));
                }
                Err(e) => {
                    // Never log the error chain raw if it could carry a path; the
                    // server name + a short reason is enough signal.
                    warn!(server = %s.name, reason = %e, "mcp: server failed to connect; skipping");
                }
            }
        }
        Ok(())
    }

    /// Connect one server end-to-end. RUNTIME-ONLY (spawns / opens a transport).
    async fn connect_one(
        s: &McpServerConfig,
        project_root: &Path,
        timeout: Duration,
        max_tools: usize,
        max_out: usize,
    ) -> McpResult<McpClient> {
        let transport: Box<dyn McpTransport> = match s.transport {
            McpTransportKind::Stdio => {
                if s.command.is_empty() {
                    bail!("stdio server has no command");
                }
                // Wrap the command in sandbox-exec -f <profile> so the child runs
                // under a default-deny seatbelt profile. The profile is written to
                // a per-server file under the project state tree; deriving it is
                // pure ([`stdio_sandbox_profile`]) and unit-tested.
                let (sandbox_cmd, sandbox_args) =
                    sandbox_wrapped_argv(s, project_root).context("deriving MCP sandbox wrapper")?;
                Box::new(
                    StdioTransport::spawn(&sandbox_cmd, &sandbox_args, max_out)
                        .await
                        .context("spawning sandboxed MCP stdio server")?,
                )
            }
            McpTransportKind::Http => {
                if s.url.is_empty() {
                    bail!("http server has no url");
                }
                // HTTPS-ONLY is enforced in HttpTransport::new (a non-https url is
                // a friendly Err, so a token never rides plaintext). A REMOTE
                // server is NOT SBPL-sandboxed — there is no local process to
                // wrap; its layers are TLS + Keychain bearer + the SAME gate /
                // allowlist / bounds as stdio (documented on HttpTransport).
                //
                // Resolve the optional bearer token from the Keychain HERE, at
                // connect, so it can ride the Authorization header for every
                // request on this connection. It is header-only: never the URL,
                // never logged (only its presence, as a bool), never in Debug.
                let token = if s.uses_token {
                    match mcp_token_account(&s.name) {
                        Some(account) => {
                            let t = integrations::resolve_secret(&account).await;
                            info!(server = %s.name, present = t.is_some(), "mcp: resolved http server token");
                            t
                        }
                        None => None,
                    }
                } else {
                    None
                };
                Box::new(
                    HttpTransport::new(&s.url, token, timeout, max_out)
                        .context("building MCP http transport")?,
                )
            }
        };
        let mut client = McpClient::handshake(
            &s.name,
            transport,
            timeout,
            max_tools,
            ToolClass::from_config(s.default_class),
            s.read_only_tools.clone(),
        )
        .await?;
        client.list_tools().await?;
        Ok(client)
    }

    /// TEST/RUNTIME seam: insert an already-handshaken client (mock-backed in
    /// tests). Lets the whole manager surface — allowlist, tool aggregation,
    /// gated call_tool — be exercised hermetically without a transport spawn.
    pub fn insert_client(&mut self, client: McpClient) {
        self.clients.insert(client.name().to_string(), Arc::new(client));
    }

    /// Every tool across every connected server, namespaced. Empty when disabled.
    pub fn tools(&self) -> Vec<DiscoveredTool> {
        self.clients
            .values()
            .flat_map(|c| c.tools().iter().cloned())
            .collect()
    }

    /// May `agent` use `server`? The orchestrator always may; every other agent
    /// only if the server's `agents` allowlist names it. A server that does not
    /// exist (not connected, or not configured) is never usable. Pure.
    pub fn agent_may_use(&self, agent: &str, server: &str) -> bool {
        if agent == ORCHESTRATOR {
            // Even the orchestrator can only use a server that is configured.
            return self.cfg.servers.iter().any(|s| s.name == server);
        }
        self.cfg
            .servers
            .iter()
            .find(|s| s.name == server)
            .is_some_and(|s| s.agents.iter().any(|a| a == agent))
    }

    /// Resolve the classification of a discovered tool on a server (for the loop /
    /// confirmation layer to decide gating before a call). `None` when the
    /// server/tool is unknown — an unknown tool is treated as consequential by the
    /// caller (fail-safe), but we report honestly that we don't know it.
    pub fn tool_class(&self, server: &str, tool: &str) -> Option<ToolClass> {
        self.clients
            .get(server)
            .and_then(|c| c.find_tool(tool).map(|t| t.class))
    }

    /// Call `tool` on `server` as `agent`, with `mode` from the gate.
    ///
    /// Enforces, in order: (1) the per-agent allowlist — a disallowed agent is
    /// refused without any call; (2) the server must be connected; (3) the
    /// gate-driven `mode` (a consequential tool in DryRun returns a preview). The
    /// optional auth token is resolved from the Keychain HERE, immediately before
    /// the call, and dropped after — never stored, never logged, never on argv.
    pub async fn call_tool(
        &self,
        agent: &str,
        server: &str,
        tool: &str,
        arguments: Value,
        mode: ActionMode,
    ) -> McpResult<CallOutcome> {
        // LOCKDOWN OVERLAY (task #12): the MCP host is FORCED off while the
        // emergency stop is engaged — no external tool call leaves the daemon when
        // locked, even if a stale tool def slipped through to the model. Belt-and-
        // braces with `tool_defs_for_agent` (which offers nothing when locked); a
        // call attempt is refused here before any transport is touched. With
        // lockdown OFF (the shipped default) this is byte-for-byte today.
        if crate::lockdown::is_locked_down() {
            bail!("MCP is locked down (panic engaged); no external tool calls until unlock");
        }
        if !self.agent_may_use(agent, server) {
            bail!("agent {agent} is not allowed to use MCP server {server}");
        }
        let client = self
            .clients
            .get(server)
            .ok_or_else(|| anyhow!("MCP server {server} is not connected"))?;

        // Resolve the server's token (if it declares one) at the last moment. The
        // value is read but NOT used to mutate argv/url here — the stdio transport
        // is already spawned, so the token, when present, is surfaced to a future
        // HTTP/auth-header path. We resolve-and-drop to (a) prove the Keychain
        // account is reachable and (b) keep the secret off every persistent
        // surface. It is never logged — only its presence, as a bool.
        if let Some(server_cfg) = self.cfg.servers.iter().find(|s| s.name == server) {
            if server_cfg.uses_token {
                if let Some(account) = mcp_token_account(server) {
                    let token = integrations::resolve_secret(&account).await;
                    info!(server, present = token.is_some(), "mcp: resolved server token");
                    // `token` drops at the end of this block — never stored.
                }
            }
        }

        client.call_tool(tool, arguments, mode).await
    }

    /// The configured (not necessarily connected) servers — for status/HUD.
    pub fn configured_servers(&self) -> &[McpServerConfig] {
        &self.cfg.servers
    }

    /// The tools `agent` may call across every connected server it is allowlisted
    /// for, each as a flat `mcp__<server>__<tool>` id with its Anthropic tool-def.
    /// This is the DYNAMIC registration surface the cloud tool loop appends to the
    /// static `tool_defs()` — a non-allowlisted agent is never offered a server's
    /// tools (the model cannot call what it cannot see), mirroring `tools_for_agent`.
    /// Empty when disabled (no clients) or when `agent` is on no server's allowlist.
    ///
    /// LOCKDOWN OVERLAY (task #12): while the emergency stop is engaged the host
    /// offers NO MCP tools to the model (the model cannot call what it cannot
    /// see), so the external-tool surface vanishes the instant panic fires. With
    /// lockdown OFF (the shipped default) this is byte-for-byte today.
    pub fn tool_defs_for_agent(&self, agent: &str) -> Vec<Value> {
        if crate::lockdown::is_locked_down() {
            return Vec::new();
        }
        let mut defs = Vec::new();
        for tool in self.tools() {
            if !self.agent_may_use(agent, &tool.server) {
                continue;
            }
            defs.push(json!({
                "name": flat_tool_name(&tool.server, &tool.name),
                "description": tool.description,
                // The server's verbatim inputSchema, or a permissive object schema
                // when it omitted one (Null) — the Messages API requires an object.
                "input_schema": if tool.input_schema.is_object() {
                    tool.input_schema.clone()
                } else {
                    json!({ "type": "object" })
                },
            }));
        }
        defs
    }

    /// Resolve the class of a flat `mcp__<server>__<tool>` id, FAIL-SAFE: an id we
    /// cannot parse or whose tool is not discovered classifies as Consequential, so
    /// an unknown MCP tool always parks behind the gate rather than running ungated.
    pub fn class_for_flat(&self, flat: &str) -> ToolClass {
        match parse_flat_tool_name(flat) {
            Some((server, tool)) => self
                .tool_class(&server, &tool)
                .unwrap_or(ToolClass::Consequential),
            None => ToolClass::Consequential,
        }
    }

    /// A SECRET-FREE status snapshot for the HUD MCP panel: every CONFIGURED
    /// server, its transport, whether it is currently CONNECTED, the tools it
    /// exposes (name + class), and which agents may use it. This carries NO token
    /// and NO secret — only `uses_token` as a bool, never the value, account, or a
    /// URL with credentials in it. It is the visibility surface the HUD reduces;
    /// the panel renders it read-only.
    pub fn status_snapshot(&self) -> Value {
        let servers: Vec<Value> = self
            .cfg
            .servers
            .iter()
            .map(|s| {
                let connected = self.clients.contains_key(&s.name);
                let tools: Vec<Value> = self
                    .clients
                    .get(&s.name)
                    .map(|c| {
                        c.tools()
                            .iter()
                            .map(|t| {
                                json!({
                                    "name": t.name,
                                    "consequential": t.is_consequential(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                json!({
                    "name": s.name,
                    "transport": match s.transport {
                        McpTransportKind::Stdio => "stdio",
                        McpTransportKind::Http => "http",
                    },
                    "connected": connected,
                    // uses_token is a BOOL only — the token value/account is never
                    // here, never logged, never on argv or in a URL.
                    "uses_token": s.uses_token,
                    "agents": s.agents,
                    "tools": tools,
                })
            })
            .collect();
        json!({
            "enabled": self.cfg.enabled,
            "servers": servers,
        })
    }
}

// ===========================================================================
// Flat tool-name namespacing for the cloud tool loop
// ===========================================================================

/// The flat, double-underscore-delimited id the Anthropic tool loop addresses an
/// MCP tool by: `mcp__<server>__<tool>`. Distinct from [`DiscoveredTool::qualified_name`]
/// (`mcp.<server>.<tool>`, the human/log form) because the Messages API tool
/// `name` must match `^[a-zA-Z0-9_-]{1,128}$` — dots are illegal, so the wire id
/// uses underscores. The `mcp__` prefix can never collide with a built-in tool
/// (none start with it), and the server is always recoverable via
/// [`parse_flat_tool_name`]. Server names are validated to the strict shape in
/// `integrations::is_safe_mcp_server_name` — `[a-z0-9_-]+` with NO consecutive
/// separator — so the server name can never contain `__`, making the
/// double-underscore delimiter unambiguous.
pub fn flat_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// Recover `(server, tool)` from a flat `mcp__<server>__<tool>` id, or `None` when
/// it is not an MCP flat id / is malformed. Splits on the FIRST `__` after the
/// `mcp__` prefix as the server boundary; the remainder (which may itself contain
/// `__` if a server names a tool that way) is the tool name. Server names are the
/// validated strict shape (`[a-z0-9_-]+`, no consecutive separator — see
/// `integrations::is_safe_mcp_server_name`), so they never contain `__`, making
/// the first boundary the correct one.
pub fn parse_flat_tool_name(flat: &str) -> Option<(String, String)> {
    let rest = flat.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server.to_string(), tool.to_string()))
}

/// Is `name` an MCP flat tool id (`mcp__*`)? Cheap prefix check the tool loop uses
/// to route a tool_use block to the MCP dispatch instead of the static one.
pub fn is_mcp_flat_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

// ===========================================================================
// stdio sandbox: default-deny SBPL profile derivation (reuses apps.rs)
// ===========================================================================

/// The argv to actually spawn for a stdio server: `sandbox-exec -f <profile>
/// <command> <args...>`. Writes the derived profile to a per-server file under
/// `state/mcp/` and returns `(sandbox-exec, [-f, profile, command, args...])`.
///
/// RESIDUAL TRUST (honest): `sandbox-exec` is deprecated-but-functional on macOS
/// and the same coarse-DNS / shared-CDN caveats apps.rs documents apply here. The
/// profile bounds the server's filesystem + network to what its config declares;
/// it does NOT make an untrusted server safe — the gate + allowlist + bounds are
/// the other layers. A server whose binary itself is malicious is constrained,
/// not neutralized.
fn sandbox_wrapped_argv(
    s: &McpServerConfig,
    project_root: &Path,
) -> McpResult<(String, Vec<String>)> {
    let profile = stdio_sandbox_profile(s, project_root);
    let profile_dir = project_root.join("state").join("mcp");
    std::fs::create_dir_all(&profile_dir).context("creating MCP sandbox profile dir")?;
    let profile_path = profile_dir.join(format!("{}.sb", s.name));
    std::fs::write(&profile_path, profile).context("writing MCP sandbox profile")?;

    let mut args = vec![
        "-f".to_string(),
        profile_path.to_string_lossy().into_owned(),
        s.command.clone(),
    ];
    args.extend(s.args.iter().cloned());
    Ok((crate::apps::SANDBOX_EXEC.to_string(), args))
}

/// Derive the default-deny seatbelt (SBPL) profile text for a stdio MCP server.
///
/// DEFAULT-DENY, reusing the apps.rs micro-app machinery: opens with `(deny
/// default)`, imports Apple's `bsd.sb` base (so the process can boot without
/// opening anything), then grants ONLY:
///   * exec of the server command + its own directory,
///   * read of the command's dir + each declared `fs_read` subpath,
///   * write of each declared `fs_write` subpath,
///   * outbound TCP to each declared `net_hosts` host (+ DNS); EMPTY net_hosts =>
///     `(deny network*)` — no network at all.
///     Everything else — the mic, GPU, the rest of the filesystem, the memory DB,
///     secrets — stays denied by the opener. PURE (no IO), so the profile is fully
///     unit-testable, mirroring apps.rs's `generate_sbpl`.
pub fn stdio_sandbox_profile(s: &McpServerConfig, project_root: &Path) -> String {
    let mut p = String::new();
    p.push_str("(version 1)\n");
    p.push_str(&format!(
        ";; Generated by darwind for MCP stdio server {:?} — docs/SANDBOX.md.\n",
        s.name
    ));
    p.push_str(";; DEFAULT-DENY: everything below is the complete grant set for\n");
    p.push_str(";; this MCP server. sandbox-exec is deprecated-but-functional;\n");
    p.push_str(";; the kernel seatbelt enforcement is live. RESIDUAL TRUST: this\n");
    p.push_str(";; bounds the server, it does not make an untrusted server safe.\n");
    p.push_str("(deny default)\n");
    if Path::new(BSD_BASE_PROFILE).exists() {
        p.push_str(&format!("(import {})\n", sbpl_str(Path::new(BSD_BASE_PROFILE))));
    }

    // No mic / GPU — stated explicitly though (deny default) already covers them.
    p.push_str("\n;; No microphone / GPU for an MCP tool server.\n");
    p.push_str("(deny device-microphone)\n");

    // --- exec + read of the command itself -----------------------------
    p.push_str("\n;; Exec + read the server command and its own directory.\n");
    if !s.command.is_empty() {
        let cmd = Path::new(&s.command);
        p.push_str(&format!("(allow process-exec* (literal {}))\n", sbpl_str(cmd)));
        p.push_str(&format!("(allow file-read* (literal {}))\n", sbpl_str(cmd)));
        if let Some(dir) = cmd.parent() {
            p.push_str(&format!("(allow file-read* (subpath {}))\n", sbpl_str(dir)));
        }
    }
    // The runtime loader needs to read system framework / dyld roots; bsd.sb
    // covers the boot reads. We additionally allow reading the project root's
    // read-declared paths below.

    // --- declared reads ------------------------------------------------
    p.push_str("\n;; Declared filesystem reads.\n");
    for r in &s.fs_read {
        p.push_str(&format!("(allow file-read* (subpath {}))\n", sbpl_str(Path::new(r))));
    }

    // --- declared writes -----------------------------------------------
    p.push_str("\n;; Declared filesystem writes.\n");
    for w in &s.fs_write {
        p.push_str(&format!("(allow file-write* (subpath {}))\n", sbpl_str(Path::new(w))));
    }

    // --- network -------------------------------------------------------
    // Same last-match-wins discipline as apps.rs: deny network*, then re-allow
    // DNS + the declared host-names. Empty net_hosts => no network at all.
    if s.net_hosts.is_empty() {
        p.push_str("\n;; net_hosts = [] -> no outbound network at all.\n");
        p.push_str("(deny network*)\n");
    } else {
        p.push_str("\n;; net_hosts non-empty -> outbound TCP to the listed hosts\n");
        p.push_str(";; only, plus DNS. CAVEAT: SBPL host-name filtering is coarse\n");
        p.push_str(";; (cannot pin the resolved IP) and allowing DNS opens a side\n");
        p.push_str(";; channel — see docs/SANDBOX.md. This RAISES the bar, it does\n");
        p.push_str(";; not close it.\n");
        p.push_str("(system-network)\n");
        p.push_str("(deny network*)\n");
        p.push_str("(allow network-outbound (remote udp \"*:53\"))\n");
        p.push_str("(allow network-outbound (remote tcp \"*:53\"))\n");
        let mut hosts: Vec<&str> = s.net_hosts.iter().map(String::as_str).collect();
        hosts.sort_unstable();
        hosts.dedup();
        for host in hosts {
            p.push_str(&format!(
                "(allow network-outbound (remote tcp (host-name {})))\n",
                sbpl_str(Path::new(host))
            ));
        }
    }

    // Note: project_root is accepted for parity with apps.rs and future grants
    // (e.g. a per-server socket); referenced here so the signature is stable.
    let _ = project_root;

    // --- mach / loader services ----------------------------------------
    p.push_str("\n;; Mach lookups the dynamic loader and runtime require.\n");
    p.push_str("(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n");
    p.push_str("(allow sysctl-read)\n");

    p
}

// ===========================================================================
// Process-global manager seam (read-only after startup connect)
// ===========================================================================

/// The one connected [`McpManager`], installed ONCE at daemon startup AFTER
/// `connect_all` has run (so the live clients are already populated), and read
/// thereafter by the cloud tool loop WITHOUT threading a `&McpManager` through
/// `execute_tool`'s many call sites. Mirrors anthropic.rs's `MISSION_MODEL` /
/// `FORGE_GATE` startup-installed globals.
///
/// `None` until [`install`] is called — any test, or a startup path that skips
/// MCP, reads an inert disabled manager via [`with`], so the SHIPPED-OFF posture
/// holds even when the global is unset: no servers, no tools, no calls.
static GLOBAL: std::sync::OnceLock<McpManager> = std::sync::OnceLock::new();

/// Install the connected manager as the process-global. Called once from `main()`
/// after `McpManager::connect_all`. Idempotent (a lost `set` means the same
/// manager was already installed); a disabled manager is a valid, inert install.
pub fn install(manager: McpManager) {
    let _ = GLOBAL.set(manager);
}

/// Borrow the installed manager. Falls back to a freshly-built DISABLED manager
/// (the shipped-OFF default) when [`install`] was never called — so every reader
/// fails safe: a never-installed global offers no MCP tools and refuses every
/// call. The fallback is a leaked `'static` built lazily exactly once.
pub fn global() -> &'static McpManager {
    GLOBAL.get().unwrap_or_else(|| {
        static DISABLED: std::sync::OnceLock<McpManager> = std::sync::OnceLock::new();
        // The UNINSTALLED fallback is explicitly disabled (fail-safe) — independent
        // of the [mcp].enabled config default (now ON, full-power). Until install()
        // wires the real, config-driven manager, every reader sees an inert manager:
        // no tools, no server usable. (A configured-ON manager still connects to
        // nothing without a [[mcp.servers]] entry — see install().)
        DISABLED.get_or_init(|| {
            McpManager::new(McpConfig { enabled: false, ..McpConfig::default() })
        })
    })
}

#[cfg(test)]
/// Test seam: build a manager from `cfg`, hand it to `f` to insert mock-backed
/// clients, then return it — so a test can exercise the agent-surface wiring
/// (tool_defs_for_agent / class_for_flat / call_tool) hermetically without ever
/// touching the process-global (which a parallel test could race).
pub fn test_manager(cfg: McpConfig, f: impl FnOnce(&mut McpManager)) -> McpManager {
    let mut mgr = McpManager::new(cfg);
    f(&mut mgr);
    mgr
}

// ===========================================================================
// Hermetic test double + tests
// ===========================================================================

#[cfg(test)]
pub mod testing {
    use super::*;
    use std::sync::Mutex;

    /// A scriptable, recording, process-free [`McpTransport`]. Register canned
    /// JSON-RPC responses keyed by method name; every request/notification is
    /// recorded. Makes NO subprocess + NO network calls — an unmatched request
    /// resolves to an explicit error so a test fails loudly rather than silently
    /// reaching out. Optionally injects a delay (for the timeout test) or returns
    /// a verbatim malformed/oversized value (for the bounds tests).
    pub struct MockTransport {
        /// (method -> response value). The response is the FULL JSON-RPC object
        /// the server would send (so a test can script a result or an error).
        canned: Mutex<Vec<(String, Value)>>,
        recorded: Mutex<Vec<Value>>,
        /// Optional per-request delay, to exercise the per-call timeout.
        delay: Option<Duration>,
        /// When set, every `request` returns this Err instead of a canned reply
        /// (to exercise a transport-level failure / malformed handling).
        fail_with: Option<String>,
    }

    impl MockTransport {
        pub fn new() -> Self {
            Self {
                canned: Mutex::new(Vec::new()),
                recorded: Mutex::new(Vec::new()),
                delay: None,
                fail_with: None,
            }
        }

        /// Register a canned full JSON-RPC response object for `method`. The id is
        /// filled in from the request at send time so it always echoes correctly.
        pub fn on(self, method: impl Into<String>, response: Value) -> Self {
            self.canned.lock().unwrap().push((method.into(), response));
            self
        }

        /// Inject a delay longer than the test's per-call timeout.
        pub fn with_delay(mut self, d: Duration) -> Self {
            self.delay = Some(d);
            self
        }

        /// Make every `request` fail at the transport level.
        pub fn failing(msg: impl Into<String>) -> Self {
            let mut t = Self::new();
            t.fail_with = Some(msg.into());
            t
        }

        /// Every message the mock received, in order.
        pub fn recorded(&self) -> Vec<Value> {
            self.recorded.lock().unwrap().clone()
        }

        /// The method names recorded, in order — for asserting the lifecycle
        /// sequence without inspecting ids.
        pub fn methods(&self) -> Vec<String> {
            self.recorded
                .lock()
                .unwrap()
                .iter()
                .filter_map(|m| m.get("method").and_then(Value::as_str).map(str::to_string))
                .collect()
        }
    }

    impl Default for MockTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl McpTransport for MockTransport {
        fn request<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<Value>> {
            Box::pin(async move {
                self.recorded.lock().unwrap().push(message.clone());
                if let Some(d) = self.delay {
                    tokio::time::sleep(d).await;
                }
                if let Some(msg) = &self.fail_with {
                    bail!("{msg}");
                }
                let method = message.get("method").and_then(Value::as_str).unwrap_or("");
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                let canned = self.canned.lock().unwrap();
                match canned.iter().find(|(m, _)| m == method) {
                    Some((_, resp)) => {
                        // Echo the request id into the canned response.
                        let mut r = resp.clone();
                        if let Some(obj) = r.as_object_mut() {
                            obj.insert("id".to_string(), id);
                        }
                        Ok(r)
                    }
                    None => bail!("MockTransport: no canned response for method {method}"),
                }
            })
        }

        fn notify<'a>(&'a self, message: Value) -> BoxFuture<'a, McpResult<()>> {
            Box::pin(async move {
                self.recorded.lock().unwrap().push(message);
                Ok(())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::MockTransport;
    use super::*;
    use crate::config::McpTransportKind;

    fn ok_result(result: Value) -> Value {
        json!({ "jsonrpc": "2.0", "result": result })
    }

    fn rpc_error(code: i64, message: &str) -> Value {
        json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message } })
    }

    fn server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Stdio,
            command: "/usr/bin/true".to_string(),
            ..Default::default()
        }
    }

    // -- lifecycle: initialize / initialized -----------------------------

    #[tokio::test]
    async fn handshake_sends_initialize_then_initialized() {
        let transport = MockTransport::new().on(
            "initialize",
            ok_result(json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {} })),
        );
        // Box it but keep a raw pointer-free reference for assertions: we re-build
        // a separate mock for recording inspection instead.
        let boxed: Box<dyn McpTransport> = Box::new(transport);
        let client = McpClient::handshake(
            "files",
            boxed,
            Duration::from_secs(5),
            64,
            ToolClass::Consequential,
            vec![],
        )
        .await
        .expect("handshake");
        assert_eq!(client.name(), "files");
    }

    #[tokio::test]
    async fn handshake_records_initialize_and_initialized_in_order() {
        // Use Arc to keep a handle to the mock for assertions after handshake.
        let mock = Arc::new(MockTransport::new().on(
            "initialize",
            ok_result(json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {} })),
        ));
        // A tiny adapter so we can both pass the transport AND inspect it.
        struct ArcT(Arc<MockTransport>);
        impl McpTransport for ArcT {
            fn request<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<Value>> {
                self.0.request(m)
            }
            fn notify<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<()>> {
                self.0.notify(m)
            }
        }
        let client = McpClient::handshake(
            "files",
            Box::new(ArcT(mock.clone())),
            Duration::from_secs(5),
            64,
            ToolClass::Consequential,
            vec![],
        )
        .await
        .expect("handshake");
        let _ = client;
        let methods = mock.methods();
        assert_eq!(methods, vec!["initialize", "notifications/initialized"]);
    }

    // -- tools/list parse -------------------------------------------------

    async fn connected_client(
        default_class: ToolClass,
        read_only: Vec<String>,
        tools_reply: Value,
    ) -> (McpClient, Arc<MockTransport>) {
        let mock = Arc::new(
            MockTransport::new()
                .on("initialize", ok_result(json!({ "capabilities": {} })))
                .on("tools/list", ok_result(tools_reply))
                .on(
                    "tools/call",
                    ok_result(json!({ "content": [{ "type": "text", "text": "done" }] })),
                ),
        );
        struct ArcT(Arc<MockTransport>);
        impl McpTransport for ArcT {
            fn request<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<Value>> {
                self.0.request(m)
            }
            fn notify<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<()>> {
                self.0.notify(m)
            }
        }
        let mut client = McpClient::handshake(
            "files",
            Box::new(ArcT(mock.clone())),
            Duration::from_secs(5),
            64,
            default_class,
            read_only,
        )
        .await
        .expect("handshake");
        client.list_tools().await.expect("tools/list");
        (client, mock)
    }

    #[tokio::test]
    async fn tools_list_parses_name_description_schema() {
        let reply = json!({
            "tools": [
                {
                    "name": "read_file",
                    "description": "Read a file",
                    "inputSchema": { "type": "object", "properties": { "path": { "type": "string" } } }
                }
            ]
        });
        let (client, _m) = connected_client(ToolClass::Consequential, vec!["read_file".into()], reply).await;
        let tools = client.tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "read_file");
        assert_eq!(tools[0].description, "Read a file");
        assert_eq!(tools[0].qualified_name(), "mcp.files.read_file");
        assert!(tools[0].input_schema.get("type").is_some());
        // It was asserted read-only in config.
        assert_eq!(tools[0].class, ToolClass::ReadOnly);
    }

    // -- unknown tool -> consequential (fail-safe) -----------------------

    #[tokio::test]
    async fn unknown_tool_classified_consequential_by_default() {
        let reply = json!({ "tools": [{ "name": "delete_everything", "description": "" }] });
        // default consequential, read_only list does NOT contain it.
        let (client, _m) = connected_client(ToolClass::Consequential, vec![], reply).await;
        let t = &client.tools()[0];
        assert_eq!(t.class, ToolClass::Consequential, "unknown tool must be consequential");
        assert!(t.is_consequential());
    }

    #[tokio::test]
    async fn read_only_listed_tool_is_read_only_even_under_default_consequential() {
        let reply = json!({ "tools": [
            { "name": "list", "description": "" },
            { "name": "write", "description": "" },
        ]});
        let (client, _m) = connected_client(ToolClass::Consequential, vec!["list".into()], reply).await;
        let by = |n: &str| client.tools().iter().find(|t| t.name == n).unwrap().class;
        assert_eq!(by("list"), ToolClass::ReadOnly);
        assert_eq!(by("write"), ToolClass::Consequential, "unlisted -> consequential");
    }

    // -- tools/call round trip + gate ------------------------------------

    #[tokio::test]
    async fn read_only_tool_call_round_trips() {
        let reply = json!({ "tools": [{ "name": "read_file", "description": "" }] });
        let (client, _m) = connected_client(ToolClass::ReadOnly, vec!["read_file".into()], reply).await;
        // Read-only tool: even DryRun mode runs it (it is not gated).
        let out = client
            .call_tool("read_file", json!({ "path": "/tmp/x" }), ActionMode::DryRun)
            .await
            .expect("call");
        assert_eq!(out, CallOutcome::Ok("done".to_string()));
    }

    #[tokio::test]
    async fn consequential_tool_in_dryrun_does_not_execute() {
        let reply = json!({ "tools": [{ "name": "delete", "description": "" }] });
        let (client, mock) = connected_client(ToolClass::Consequential, vec![], reply).await;
        let before = mock.methods().iter().filter(|m| *m == "tools/call").count();
        let out = client
            .call_tool("delete", json!({}), ActionMode::DryRun)
            .await
            .expect("call");
        match out {
            CallOutcome::DryRun(p) => assert!(p.contains("[dry run]")),
            other => panic!("expected dry run, got {other:?}"),
        }
        let after = mock.methods().iter().filter(|m| *m == "tools/call").count();
        assert_eq!(before, after, "no tools/call must have been sent in dry run");
    }

    #[tokio::test]
    async fn consequential_tool_in_execute_runs() {
        let reply = json!({ "tools": [{ "name": "delete", "description": "" }] });
        let (client, _m) = connected_client(ToolClass::Consequential, vec![], reply).await;
        let out = client
            .call_tool("delete", json!({}), ActionMode::Execute)
            .await
            .expect("call");
        assert_eq!(out, CallOutcome::Ok("done".to_string()));
    }

    // -- bounds: timeout / oversize / malformed --------------------------

    #[tokio::test]
    async fn slow_server_trips_the_per_call_timeout() {
        let slow = MockTransport::new()
            .on("initialize", ok_result(json!({})))
            .with_delay(Duration::from_millis(200));
        let res = McpClient::handshake(
            "files",
            Box::new(slow),
            Duration::from_millis(20), // tighter than the delay
            64,
            ToolClass::Consequential,
            vec![],
        )
        .await;
        assert!(res.is_err(), "handshake must time out on a slow server");
        let msg = res.err().unwrap().to_string();
        assert!(msg.contains("initialize") || msg.contains("respond"), "{msg}");
    }

    #[tokio::test]
    async fn malformed_rpc_reply_is_rejected() {
        // Reply is missing jsonrpc + result + error.
        let bad = MockTransport::new().on("initialize", json!({ "garbage": true }));
        let res = McpClient::handshake(
            "files",
            Box::new(bad),
            Duration::from_secs(5),
            64,
            ToolClass::Consequential,
            vec![],
        )
        .await;
        assert!(res.is_err(), "a malformed handshake reply must be rejected");
    }

    #[tokio::test]
    async fn rpc_error_object_maps_to_friendly_message_without_leaking_server_text() {
        let leak = "SECRET-DB-PATH-/etc/passwd";
        let erroring = MockTransport::new()
            .on("initialize", ok_result(json!({})))
            .on("tools/list", ok_result(json!({ "tools": [{ "name": "x" }] })))
            .on("tools/call", rpc_error(-32602, leak));
        struct OneShot(MockTransport);
        impl McpTransport for OneShot {
            fn request<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<Value>> {
                self.0.request(m)
            }
            fn notify<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<()>> {
                self.0.notify(m)
            }
        }
        let mut client = McpClient::handshake(
            "files",
            Box::new(OneShot(erroring)),
            Duration::from_secs(5),
            64,
            ToolClass::ReadOnly,
            vec!["x".into()],
        )
        .await
        .unwrap();
        client.list_tools().await.unwrap();
        let out = client.call_tool("x", json!({}), ActionMode::Execute).await.unwrap();
        match out {
            CallOutcome::ToolError(msg) => {
                assert!(!msg.contains(leak), "server error text must not leak: {msg}");
                assert!(msg.contains("arguments"), "friendly phrasing expected: {msg}");
            }
            other => panic!("expected tool error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tools_list_truncates_at_the_per_server_cap() {
        let many: Vec<Value> = (0..100)
            .map(|i| json!({ "name": format!("t{i}"), "description": "" }))
            .collect();
        let mock = Arc::new(
            MockTransport::new()
                .on("initialize", ok_result(json!({})))
                .on("tools/list", ok_result(json!({ "tools": many }))),
        );
        struct ArcT(Arc<MockTransport>);
        impl McpTransport for ArcT {
            fn request<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<Value>> {
                self.0.request(m)
            }
            fn notify<'a>(&'a self, m: Value) -> BoxFuture<'a, McpResult<()>> {
                self.0.notify(m)
            }
        }
        let mut client = McpClient::handshake(
            "files",
            Box::new(ArcT(mock)),
            Duration::from_secs(5),
            5, // cap
            ToolClass::Consequential,
            vec![],
        )
        .await
        .unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 5, "tools/list must truncate at max_tools");
    }

    // -- manager: enabled=false is inert ---------------------------------

    #[test]
    fn disabled_manager_connects_to_nothing_and_offers_no_tools() {
        let cfg = McpConfig {
            enabled: false,
            servers: vec![server("files")],
            ..Default::default()
        };
        let mgr = McpManager::new(cfg);
        assert!(!mgr.enabled());
        assert!(mgr.connectable_servers().is_empty(), "disabled -> nothing connectable");
        assert!(mgr.tools().is_empty(), "disabled -> no tools");
    }

    #[tokio::test]
    async fn disabled_connect_all_is_a_noop() {
        let cfg = McpConfig {
            enabled: false,
            servers: vec![server("files")],
            ..Default::default()
        };
        let mut mgr = McpManager::new(cfg);
        mgr.connect_all(Path::new("/tmp/darwin-test-root"))
            .await
            .expect("noop");
        assert!(mgr.tools().is_empty());
    }

    #[test]
    fn enabled_manager_respects_max_servers_bound() {
        let cfg = McpConfig {
            enabled: true,
            max_servers: 2,
            servers: vec![server("a"), server("b"), server("c")],
            ..Default::default()
        };
        let mgr = McpManager::new(cfg);
        assert_eq!(mgr.connectable_servers().len(), 2, "max_servers must cap fan-out");
    }

    #[test]
    fn invalid_server_name_is_not_connectable() {
        let cfg = McpConfig {
            enabled: true,
            // "weather__api" is the namespacing-hostile case: its `__` would
            // mis-split the flat id `mcp__weather__api__<tool>`. It must be
            // refused alongside the obviously-bad space/`!`/uppercase names.
            servers: vec![
                server("Bad Name!"),
                server("weather__api"),
                server("_lead"),
                server("ok"),
            ],
            ..Default::default()
        };
        let mgr = McpManager::new(cfg);
        let names: Vec<&str> = mgr
            .connectable_servers()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(names, vec!["ok"], "an unsafe server name must be refused");
    }

    // -- manager: per-agent allowlist ------------------------------------

    #[test]
    fn allowlist_admits_orchestrator_and_listed_agents_only() {
        let mut s = server("files");
        s.agents = vec!["friday".into()];
        let cfg = McpConfig {
            enabled: true,
            servers: vec![s],
            ..Default::default()
        };
        let mgr = McpManager::new(cfg);
        assert!(mgr.agent_may_use("darwin", "files"), "orchestrator always");
        assert!(mgr.agent_may_use("friday", "files"), "listed agent");
        assert!(!mgr.agent_may_use("veronica", "files"), "unlisted agent denied");
        assert!(!mgr.agent_may_use("darwin", "nope"), "unknown server denied");
    }

    #[tokio::test]
    async fn call_tool_refuses_a_disallowed_agent_without_calling() {
        // Build a manager and insert a mock-backed client so call_tool reaches the
        // allowlist check.
        let mut s = server("files");
        s.agents = vec!["friday".into()];
        let cfg = McpConfig {
            enabled: true,
            servers: vec![s],
            ..Default::default()
        };
        let mut mgr = McpManager::new(cfg);
        let (client, _m) = connected_client(
            ToolClass::ReadOnly,
            vec!["read_file".into()],
            json!({ "tools": [{ "name": "read_file" }] }),
        )
        .await;
        mgr.insert_client(client);
        // Disallowed agent -> refused.
        let res = mgr
            .call_tool("veronica", "files", "read_file", json!({}), ActionMode::Execute)
            .await;
        assert!(res.is_err(), "disallowed agent must be refused");
        // Allowed agent -> runs.
        let out = mgr
            .call_tool("friday", "files", "read_file", json!({}), ActionMode::Execute)
            .await
            .expect("allowed agent runs");
        assert_eq!(out, CallOutcome::Ok("done".to_string()));
    }

    // -- http transport: HTTPS-only construction -------------------------

    #[test]
    fn http_transport_rejects_non_https_url() {
        let to = Duration::from_secs(5);
        // http:// is refused so a token never rides plaintext.
        assert!(HttpTransport::new("http://example.com/mcp", None, to, 4096).is_err());
        // ws://, a bare host, an empty string -> all refused.
        assert!(HttpTransport::new("ws://example.com/mcp", None, to, 4096).is_err());
        assert!(HttpTransport::new("example.com/mcp", None, to, 4096).is_err());
        assert!(HttpTransport::new("", None, to, 4096).is_err());
        // https:// is accepted.
        assert!(HttpTransport::new("https://example.com/mcp", None, to, 4096).is_ok());
        // Scheme match is case-insensitive.
        assert!(HttpTransport::new("HTTPS://example.com/mcp", None, to, 4096).is_ok());
    }

    #[test]
    fn is_https_url_only_accepts_https() {
        assert!(is_https_url("https://a.example/x"));
        assert!(is_https_url("HTTPS://a.example"));
        assert!(!is_https_url("http://a.example"));
        assert!(!is_https_url("httpss://a.example"));
        assert!(!is_https_url("ftp://a.example"));
        assert!(!is_https_url("a.example"));
        assert!(!is_https_url(""));
    }

    #[test]
    fn http_transport_rejecting_http_does_not_echo_the_url() {
        // The friendly error names the RULE, not the value — a url could carry a
        // path or token-adjacent material a log should not keep.
        let err = HttpTransport::new("http://secret-host.internal/private/path", None, Duration::from_secs(5), 4096)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("https"), "names the rule: {err}");
        assert!(!err.contains("secret-host"), "must not echo the url: {err}");
        assert!(!err.contains("private/path"), "must not echo the path: {err}");
    }

    // -- http transport: PURE SSE event parsing (canned bytes, no network) --

    #[test]
    fn sse_parser_single_json_data_event() {
        // One event carrying a single JSON-RPC reply.
        let raw = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let events = parse_sse_events(raw, 64 * 1024, 1024).expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["id"], json!(1));
        assert_eq!(events[0]["result"]["ok"], json!(true));
    }

    #[test]
    fn sse_parser_multi_event_stream_and_matching_id_extraction() {
        // A stream that interleaves a server-initiated notification (no id), a
        // response with a DIFFERENT id, then OUR response (id 7). The extractor
        // must pick ours by id, skipping the others.
        let raw = b"\
event: message\n\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\
\n\
data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"other\":1}}\n\
\n\
: keep-alive comment\n\
data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"mine\":true}}\n\
\n";
        let events = parse_sse_events(raw, 64 * 1024, 1024).expect("parse");
        assert_eq!(events.len(), 3, "notification + 2 responses, comment ignored");
        let picked = extract_rpc_response(&events, &json!(7)).expect("extract id 7");
        assert_eq!(picked["result"]["mine"], json!(true));
        // A non-present id -> friendly Err, never a hang.
        assert!(extract_rpc_response(&events, &json!(999)).is_err());
        // The notification (no id) must not be mistaken for a response.
        assert!(extract_rpc_response(&events, &Value::Null).is_err());
    }

    #[test]
    fn sse_parser_joins_multiline_data_and_handles_crlf() {
        // Multiple data: lines in one event are joined with \n; CRLF framing
        // parses identically to LF.
        let raw = b"data: {\"jsonrpc\":\"2.0\",\r\ndata: \"id\":5,\"result\":{}}\r\n\r\n";
        let events = parse_sse_events(raw, 64 * 1024, 1024).expect("parse");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["id"], json!(5));
    }

    #[test]
    fn sse_parser_skips_non_json_keepalive_without_aborting() {
        // A keep-alive data line that is not JSON is skipped; the real reply still
        // parses. No event is fabricated from junk.
        let raw = b"data: ping\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let events = parse_sse_events(raw, 64 * 1024, 1024).expect("parse");
        assert_eq!(events.len(), 1, "only the JSON event is kept");
        assert_eq!(events[0]["id"], json!(1));
    }

    #[test]
    fn sse_parser_rejects_oversize_stream() {
        // A stream larger than the byte cap is a friendly Err, never buffered.
        let big = vec![b'a'; 10_000];
        let res = parse_sse_events(&big, 4096, 1024);
        assert!(res.is_err(), "oversize stream must be rejected");
        assert!(res.err().unwrap().to_string().contains("cap"));
    }

    #[test]
    fn sse_parser_rejects_too_many_events() {
        // A flood of well-formed events past the event cap is a friendly Err.
        let mut raw = Vec::new();
        for i in 0..50 {
            raw.extend_from_slice(format!("data: {{\"id\":{i},\"result\":{{}}}}\n\n").as_bytes());
        }
        let res = parse_sse_events(&raw, 1024 * 1024, 8); // cap 8 events
        assert!(res.is_err(), "event flood must be rejected");
        assert!(res.err().unwrap().to_string().contains("event cap"));
    }

    #[test]
    fn sse_parser_rejects_non_utf8() {
        let raw = [0xff, 0xfe, 0x00, 0x01];
        assert!(parse_sse_events(&raw, 4096, 1024).is_err());
    }

    #[test]
    fn sse_parser_empty_stream_yields_no_events() {
        // No data: line at all -> empty Vec; the caller then reports "no matching
        // response" rather than hanging.
        let events = parse_sse_events(b": only a comment\n\n", 4096, 1024).expect("parse");
        assert!(events.is_empty());
        assert!(extract_rpc_response(&events, &json!(1)).is_err());
    }

    // -- http reply-mode dispatch (select_reply): PURE, canned bytes -------
    //
    // select_reply is the content-type dispatch round_trip uses; testing it
    // directly proves BOTH wire reply modes end-to-end (the application/json
    // single reply AND the SSE-to-want_id extraction) without any network, since
    // it consumes an already-read HttpReply value.

    fn reply(content_type: &str, body: &[u8]) -> HttpReply {
        HttpReply {
            content_type: content_type.to_ascii_lowercase(),
            bytes: body.to_vec(),
        }
    }

    #[test]
    fn select_reply_parses_a_single_application_json_reply() {
        // The common mode: one application/json JSON-RPC object. The id is NOT
        // consulted in the json branch (a single reply IS the reply); we return it
        // verbatim for parse_rpc_result to classify.
        let r = reply(
            "application/json",
            br#"{"jsonrpc":"2.0","id":4,"result":{"ok":true}}"#,
        );
        let got = select_reply(&r, &json!(4), 64 * 1024).expect("json reply");
        assert_eq!(got["id"], json!(4));
        assert_eq!(got["result"]["ok"], json!(true));
        // An application/json reply carrying a JSON-RPC ERROR object round-trips as
        // the object (parse_rpc_result later maps it to a friendly message).
        let e = reply(
            "application/json; charset=utf-8",
            br#"{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"x"}}"#,
        );
        let got = select_reply(&e, &json!(4), 64 * 1024).expect("json error obj");
        assert_eq!(got["error"]["code"], json!(-32602));
    }

    #[test]
    fn select_reply_rejects_malformed_json_on_the_json_path() {
        // A non-JSON / empty application/json body is a friendly Err, never a panic.
        let bad = reply("application/json", b"not json at all");
        assert!(select_reply(&bad, &json!(1), 64 * 1024).is_err());
        let empty = reply("application/json", b"");
        assert!(select_reply(&empty, &json!(1), 64 * 1024).is_err());
    }

    #[test]
    fn select_reply_extracts_the_matching_id_from_an_sse_stream() {
        // text/event-stream -> parse + pick OUR id. A stream that interleaves a
        // notification and another response must yield ours (id 7), proving the
        // round_trip wiring (want_id from the request) end-to-end with canned bytes.
        let body = b"\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\
\n\
data: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"other\":1}}\n\
\n\
data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"mine\":true}}\n\
\n";
        let r = reply("text/event-stream", body);
        let got = select_reply(&r, &json!(7), 64 * 1024).expect("sse match");
        assert_eq!(got["result"]["mine"], json!(true));
        // A request id with NO matching response on the stream -> friendly Err,
        // never a hang.
        let r2 = reply("text/event-stream", body);
        assert!(select_reply(&r2, &json!(404), 64 * 1024).is_err());
    }

    #[test]
    fn select_reply_honors_the_byte_cap_on_an_sse_stream() {
        // The cap passed to select_reply is forwarded to the SSE parser, so an
        // oversize stream is rejected through the dispatch too (not just the bare
        // parser).
        let big = vec![b'a'; 10_000];
        let r = reply("text/event-stream", &big);
        let res = select_reply(&r, &json!(1), 4096);
        assert!(res.is_err(), "oversize SSE body rejected via dispatch");
        assert!(res.err().unwrap().to_string().contains("cap"));
    }

    #[test]
    fn select_reply_defaults_an_unlabeled_body_to_json() {
        // A server that omits Content-Type (empty string) is treated as the
        // json single-reply mode rather than mis-parsed as SSE.
        let r = reply("", br#"{"jsonrpc":"2.0","id":9,"result":{}}"#);
        let got = select_reply(&r, &json!(9), 64 * 1024).expect("default json");
        assert_eq!(got["id"], json!(9));
    }

    // -- Mcp-Session-Id capture + echo (pure helpers, no network) ----------

    /// A bare HttpTransport for exercising the session-id helpers. https url so
    /// construction succeeds; no request is ever sent.
    fn session_transport() -> HttpTransport {
        HttpTransport::new(
            "https://api.example/mcp",
            None,
            Duration::from_secs(5),
            4096,
        )
        .expect("https transport")
    }

    #[test]
    fn session_id_is_none_until_the_server_mints_one() {
        // Before any reply, there is no session to echo: the first request carries
        // no Mcp-Session-Id header.
        let t = session_transport();
        assert_eq!(t.session_id_to_echo(), None, "no session before first reply");
    }

    #[test]
    fn session_id_is_captured_then_echoed_on_the_next_request() {
        // The server mints a session on its first reply; the next request echoes it.
        let t = session_transport();
        t.capture_session_id(Some("sess-abc-123"));
        assert_eq!(
            t.session_id_to_echo().as_deref(),
            Some("sess-abc-123"),
            "a minted session id is echoed next time",
        );
    }

    #[test]
    fn absent_session_header_leaves_an_established_session_intact() {
        // Once established, a reply that omits the header must NOT clear the session
        // (a server need not repeat Mcp-Session-Id on every reply).
        let t = session_transport();
        t.capture_session_id(Some("sess-1"));
        t.capture_session_id(None); // a later reply with no session header
        assert_eq!(
            t.session_id_to_echo().as_deref(),
            Some("sess-1"),
            "an absent header must not drop the session",
        );
        // A new non-empty value REPLACES the old one (server rotated the session).
        t.capture_session_id(Some("sess-2"));
        assert_eq!(t.session_id_to_echo().as_deref(), Some("sess-2"));
    }

    // -- http server through the manager: SAME gate/allowlist/bounds ------

    /// Build an `McpClient` over a MockTransport, tagged as belonging to an HTTP
    /// server (the transport seam means the client lifecycle is identical to
    /// stdio — only the wire leg differs, which the mock stands in for). This is
    /// exactly how `connect_one` would hand a handshaken http client to the
    /// manager, minus the network.
    async fn http_backed_client(
        name: &str,
        default_class: ToolClass,
        read_only: Vec<String>,
        tools_reply: Value,
    ) -> McpClient {
        let mock = MockTransport::new()
            .on("initialize", ok_result(json!({ "capabilities": {} })))
            .on("tools/list", ok_result(tools_reply))
            .on(
                "tools/call",
                ok_result(json!({ "content": [{ "type": "text", "text": "done" }] })),
            );
        let mut client = McpClient::handshake(
            name,
            Box::new(mock),
            Duration::from_secs(5),
            64,
            default_class,
            read_only,
        )
        .await
        .expect("http-backed handshake");
        client.list_tools().await.expect("tools/list");
        client
    }

    fn http_server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransportKind::Http,
            url: format!("https://{name}.example/mcp"),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn http_server_connects_lists_and_calls_like_stdio() {
        // An [[mcp.servers]] with transport="http" lists + calls a READ-ONLY tool
        // through the same manager path as a stdio server.
        let mut s = http_server("weather");
        s.agents = vec!["friday".into()];
        let cfg = McpConfig {
            enabled: true,
            servers: vec![s],
            ..Default::default()
        };
        let mut mgr = McpManager::new(cfg);
        let client = http_backed_client(
            "weather",
            ToolClass::Consequential,
            vec!["forecast".into()],
            json!({ "tools": [{ "name": "forecast", "description": "today's weather" }] }),
        )
        .await;
        mgr.insert_client(client);

        // Discovered + namespaced identically to a stdio tool.
        let defs = mgr.tool_defs_for_agent("friday");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0]["name"], json!("mcp__weather__forecast"));

        // Read-only -> runs (not gated).
        let out = mgr
            .call_tool("friday", "weather", "forecast", json!({}), ActionMode::DryRun)
            .await
            .expect("read-only call runs");
        assert_eq!(out, CallOutcome::Ok("done".to_string()));
    }

    #[tokio::test]
    async fn http_consequential_tool_parks_behind_the_gate_like_stdio() {
        // A CONSEQUENTIAL remote tool classifies consequential and, in DryRun,
        // parks (no tools/call sent) — identical to the stdio gate.
        let mut s = http_server("ops");
        s.agents = vec!["friday".into()];
        let cfg = McpConfig {
            enabled: true,
            servers: vec![s],
            ..Default::default()
        };
        let mut mgr = McpManager::new(cfg);
        let client = http_backed_client(
            "ops",
            ToolClass::Consequential,
            vec![], // delete is NOT asserted read-only -> consequential
            json!({ "tools": [{ "name": "delete", "description": "" }] }),
        )
        .await;
        mgr.insert_client(client);

        // Classifies consequential (fail-safe), both directly and via the flat id.
        assert_eq!(mgr.tool_class("ops", "delete"), Some(ToolClass::Consequential));
        assert_eq!(mgr.class_for_flat("mcp__ops__delete"), ToolClass::Consequential);

        // DryRun -> a parked preview, NOT an execution.
        let out = mgr
            .call_tool("friday", "ops", "delete", json!({}), ActionMode::DryRun)
            .await
            .expect("dry run");
        match out {
            CallOutcome::DryRun(p) => assert!(p.contains("[dry run]")),
            other => panic!("expected dry run park, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn http_server_enforces_the_per_agent_allowlist() {
        // The allowlist is transport-agnostic: a non-listed agent is refused for
        // an http server exactly as for stdio.
        let mut s = http_server("ops");
        s.agents = vec!["friday".into()];
        let cfg = McpConfig {
            enabled: true,
            servers: vec![s],
            ..Default::default()
        };
        let mut mgr = McpManager::new(cfg);
        let client = http_backed_client(
            "ops",
            ToolClass::ReadOnly,
            vec!["status".into()],
            json!({ "tools": [{ "name": "status" }] }),
        )
        .await;
        mgr.insert_client(client);

        assert!(mgr.agent_may_use("darwin", "ops"), "orchestrator always");
        assert!(mgr.agent_may_use("friday", "ops"), "listed agent");
        assert!(!mgr.agent_may_use("veronica", "ops"), "unlisted denied");

        let refused = mgr
            .call_tool("veronica", "ops", "status", json!({}), ActionMode::Execute)
            .await;
        assert!(refused.is_err(), "disallowed agent refused, no call");
    }

    #[test]
    fn http_server_url_must_be_https_to_be_meaningful() {
        // A config sanity property: an http server with a plaintext url is rejected
        // at construction (the connect path bails before any handshake). We pin the
        // pure guard the connect path uses.
        assert!(!is_https_url("http://internal.ops/mcp"), "plaintext rejected");
        assert!(is_https_url("https://internal.ops/mcp"), "tls accepted");
    }

    // -- http transport: token rides header only, never URL/log/Debug ----

    #[test]
    fn http_transport_debug_and_url_never_contain_the_token() {
        // HttpTransport holds the token in a field that Debug is NOT derived on;
        // and the url field is the bare https endpoint with NO token. We assert the
        // token is absent from the url and that the url is the only public value.
        let secret = "sk-super-secret-bearer-123";
        let t = HttpTransport::new(
            "https://api.example/mcp",
            Some(secret.to_string()),
            Duration::from_secs(5),
            4096,
        )
        .expect("https transport");
        // The url stored for requests must never carry the token.
        assert!(!t.url.contains(secret), "token must never ride the url");
        assert_eq!(t.url, "https://api.example/mcp");
    }

    // -- token never leaks -----------------------------------------------

    #[test]
    fn mcp_token_account_is_derived_and_safe() {
        // Valid name -> account.
        assert_eq!(
            mcp_token_account("files").as_deref(),
            Some("mcp_files_token")
        );
        // Hostile names -> no account at all (so resolve_secret never runs).
        assert!(mcp_token_account("../../etc").is_none());
        assert!(mcp_token_account("a b").is_none());
        assert!(mcp_token_account("a\0b").is_none());
        assert!(mcp_token_account("").is_none());
    }

    #[test]
    fn server_config_debug_never_contains_a_token() {
        // The config holds NO token field at all — it is keyed only by uses_token
        // + the Keychain account. Debug of the whole config can therefore never
        // print a secret. This pins that property structurally.
        let mut s = server("files");
        s.uses_token = true;
        let dbg = format!("{s:?}");
        assert!(dbg.contains("uses_token"));
        // There is no field that could hold a token literal.
        assert!(!dbg.to_lowercase().contains("secret"));
        assert!(!dbg.to_lowercase().contains("bearer"));
    }

    // -- sandbox profile derivable for a stdio server --------------------

    #[test]
    fn stdio_sandbox_profile_is_default_deny_and_scopes_to_declarations() {
        let mut s = server("files");
        s.command = "/opt/mcp/files-server".into();
        s.fs_read = vec!["/Users/me/project".into()];
        s.fs_write = vec!["/Users/me/project/out".into()];
        s.net_hosts = vec![];
        let profile = stdio_sandbox_profile(&s, Path::new("/Users/me/darwin"));
        assert!(profile.starts_with("(version 1)"));
        assert!(profile.contains("(deny default)"), "must be default-deny");
        assert!(profile.contains("(allow process-exec* (literal \"/opt/mcp/files-server\"))"));
        assert!(profile.contains("(allow file-read* (subpath \"/Users/me/project\"))"));
        assert!(profile.contains("(allow file-write* (subpath \"/Users/me/project/out\"))"));
        // No net_hosts -> no network at all.
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("host-name"), "no host allowed when net_hosts empty");
        // No mic.
        assert!(profile.contains("(deny device-microphone)"));
    }

    #[test]
    fn stdio_sandbox_profile_grants_only_declared_hosts() {
        let mut s = server("weather");
        s.command = "/opt/mcp/weather".into();
        s.net_hosts = vec!["api.weather.example".into()];
        let profile = stdio_sandbox_profile(&s, Path::new("/r"));
        assert!(profile.contains("(host-name \"api.weather.example\")"));
        // DNS allowed (with the documented caveat) but nothing else.
        assert!(profile.contains("\"*:53\""));
    }

    #[test]
    fn sandbox_profile_quoting_neutralizes_a_hostile_path() {
        let mut s = server("evil");
        // A path with a quote must be escaped so it cannot break out of the literal
        // and widen the profile.
        s.fs_read = vec!["/tmp/a\")(allow default)(deny".into()];
        let profile = stdio_sandbox_profile(&s, Path::new("/r"));
        // The injected close-paren+allow must appear ONLY inside an escaped literal.
        assert!(!profile.contains("(subpath \"/tmp/a\")(allow default)"), "injection must be escaped");
        assert!(profile.contains("\\\""), "the quote must be backslash-escaped");
    }

    // -- flat tool-name namespacing (cloud loop) -------------------------

    #[test]
    fn flat_tool_name_round_trips_through_parse() {
        let flat = flat_tool_name("files", "read_file");
        assert_eq!(flat, "mcp__files__read_file");
        assert_eq!(
            parse_flat_tool_name(&flat),
            Some(("files".to_string(), "read_file".to_string()))
        );
        // A tool name that itself contains a double underscore: the FIRST boundary
        // after mcp__ is the server, the remainder (incl. __) is the tool.
        let flat2 = flat_tool_name("gh", "list__prs");
        assert_eq!(
            parse_flat_tool_name(&flat2),
            Some(("gh".to_string(), "list__prs".to_string()))
        );
    }

    #[test]
    fn parse_flat_tool_name_rejects_non_mcp_and_malformed() {
        assert!(parse_flat_tool_name("open_app").is_none(), "built-in name is not MCP");
        assert!(parse_flat_tool_name("mcp__only").is_none(), "no tool segment");
        assert!(parse_flat_tool_name("mcp____tool").is_none(), "empty server");
        assert!(parse_flat_tool_name("mcp__server__").is_none(), "empty tool");
        assert!(is_mcp_flat_name("mcp__a__b"));
        assert!(!is_mcp_flat_name("open_app"));
    }

    // -- dynamic tool-def registration (per-agent) -----------------------

    /// Build a manager with one connected (mock-backed) server `files` exposing a
    /// read-only `read_file` and a consequential `write_file`, allowlisted to
    /// `friday`. Hermetic — `connected_client` uses the mock transport.
    async fn manager_with_files_server(agents: Vec<String>) -> McpManager {
        let mut s = server("files");
        s.agents = agents;
        let cfg = McpConfig {
            enabled: true,
            servers: vec![s],
            ..Default::default()
        };
        let mut mgr = McpManager::new(cfg);
        let (client, _m) = connected_client(
            ToolClass::Consequential,
            vec!["read_file".into()],
            json!({ "tools": [
                { "name": "read_file", "description": "read a file" },
                { "name": "write_file", "description": "write a file" },
            ]}),
        )
        .await;
        mgr.insert_client(client);
        mgr
    }

    #[tokio::test]
    async fn tool_defs_for_agent_offers_only_allowlisted_servers() {
        let mgr = manager_with_files_server(vec!["friday".into()]).await;
        // Orchestrator + listed agent see the two tools, flat-named.
        for agent in ["darwin", "friday"] {
            let defs = mgr.tool_defs_for_agent(agent);
            let names: Vec<&str> = defs.iter().filter_map(|d| d["name"].as_str()).collect();
            assert!(
                names.contains(&"mcp__files__read_file")
                    && names.contains(&"mcp__files__write_file"),
                "{agent} must be offered both tools, got {names:?}"
            );
            // The def carries an object input_schema (Messages API requires it).
            assert!(defs[0]["input_schema"].is_object());
        }
        // A non-allowlisted agent is offered NOTHING — it cannot even see them.
        let none = mgr.tool_defs_for_agent("veronica");
        assert!(none.is_empty(), "unlisted agent must be offered no MCP tools");
    }

    #[tokio::test]
    async fn disabled_manager_offers_no_mcp_tool_defs() {
        // enabled=false -> no clients are ever inserted at runtime; even the
        // orchestrator is offered nothing.
        let cfg = McpConfig { enabled: false, servers: vec![server("files")], ..Default::default() };
        let mgr = McpManager::new(cfg);
        assert!(mgr.tool_defs_for_agent("darwin").is_empty(), "disabled -> no defs");
    }

    #[tokio::test]
    async fn class_for_flat_is_fail_safe() {
        let mgr = manager_with_files_server(vec!["friday".into()]).await;
        assert_eq!(
            mgr.class_for_flat("mcp__files__read_file"),
            ToolClass::ReadOnly,
            "config-asserted read-only"
        );
        assert_eq!(
            mgr.class_for_flat("mcp__files__write_file"),
            ToolClass::Consequential,
            "unlisted -> consequential"
        );
        // Unknown tool / unknown server / malformed -> consequential (fail-safe).
        assert_eq!(mgr.class_for_flat("mcp__files__nope"), ToolClass::Consequential);
        assert_eq!(mgr.class_for_flat("mcp__ghost__x"), ToolClass::Consequential);
        assert_eq!(mgr.class_for_flat("not_an_mcp_id"), ToolClass::Consequential);
    }

    #[tokio::test]
    async fn status_snapshot_is_secret_free_and_reflects_connection() {
        let mut mgr = manager_with_files_server(vec!["friday".into()]).await;
        // Mark the server as token-using to prove the snapshot still carries no
        // secret — only the uses_token bool.
        // (insert a second, UNCONNECTED, token-using server via config.)
        let mut s2 = server("vault");
        s2.uses_token = true;
        s2.agents = vec!["pepper".into()];
        mgr.cfg.servers.push(s2);

        let snap = mgr.status_snapshot();
        assert_eq!(snap["enabled"], json!(true));
        let servers = snap["servers"].as_array().unwrap();
        assert_eq!(servers.len(), 2, "both configured servers are listed");

        let files = servers.iter().find(|s| s["name"] == json!("files")).unwrap();
        assert_eq!(files["connected"], json!(true), "files is connected");
        assert_eq!(files["transport"], json!("stdio"));
        assert_eq!(files["agents"], json!(["friday"]));
        // Tools carry name + class only.
        let tools = files["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        let read = tools.iter().find(|t| t["name"] == json!("read_file")).unwrap();
        assert_eq!(read["consequential"], json!(false));

        let vault = servers.iter().find(|s| s["name"] == json!("vault")).unwrap();
        assert_eq!(vault["connected"], json!(false), "vault never connected");
        assert_eq!(vault["uses_token"], json!(true), "presence reported as a bool");

        // SECRET HYGIENE: the whole serialized snapshot carries no token value /
        // account / bearer / secret literal — only the uses_token bool key.
        let blob = snap.to_string().to_lowercase();
        assert!(!blob.contains("bearer"), "no bearer token in the snapshot");
        assert!(!blob.contains("secret"), "no secret literal in the snapshot");
        assert!(!blob.contains("mcp_vault_token"), "no Keychain account in the snapshot");
    }

    #[test]
    fn global_falls_back_to_a_disabled_manager_when_uninstalled() {
        // Without install(), global() is the inert disabled default: no tools, no
        // server usable — the shipped-OFF posture even when the global is unset.
        let g = global();
        assert!(!g.enabled());
        assert!(g.tools().is_empty());
        assert!(g.tool_defs_for_agent("darwin").is_empty());
    }

    // -- protocol-version + orchestrator lockstep ------------------------

    #[test]
    fn protocol_version_is_sent_in_initialize_shape() {
        let req = rpc_request(
            1,
            "initialize",
            json!({ "protocolVersion": PROTOCOL_VERSION, "capabilities": {} }),
        );
        assert_eq!(req["params"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["method"], "initialize");
    }
}
