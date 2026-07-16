//! DARWIN LANGUAGE SERVER (DLS) — an LSP-style endpoint grounded in the LIVE
//! capability graph, for a human editing DARWIN's config / manifests.
//!
//! There is no static schema file to drift against: every answer is computed from
//! the RUNNING registries the daemon itself validates against —
//!
//!   - COMPLETION of config keys comes from [`crate::config::known_keys`] (the SAME
//!     `KNOWN_KEYS` the parser flags typos against);
//!   - HOVERS on a config section are synthesized from the capability atlas
//!     (`capability.rs` for the section's live armed/inert status, `atlas.rs` for
//!     capability-name hovers);
//!   - DIAGNOSTICS run the daemon's REAL rules — an over-privileged / malformed
//!     manifest via [`crate::plugin_sdk::validate_manifest`], an unknown agent tool
//!     via the `agents.rs` allowlist, a `net` scope without `net_hosts` (a specific
//!     over-privilege the manifest validator rejects), a `mode = "auto"` autonomy
//!     lint, and an unknown config key/section via `KNOWN_KEYS`.
//!
//! STRICTLY READ-ONLY + LOCAL. The endpoint binds 127.0.0.1 ONLY (a LOOPBACK
//! socket the editor shim connects to) and NEVER writes config or takes any action
//! — the intentional absence of a config-write primitive in the daemon is
//! preserved. It only ASSISTS the human who edits: it has no `applyEdit` / write /
//! mutate method, by construction (an unsupported method is refused, never
//! silently performed). SHIPS OFF (`[dls].enabled`, opt-in) — read-only + loopback
//! is low-risk, but a listening socket is still a surface.
//!
//! The completion / hover / diagnostic COMPUTATION is a PURE, unit-testable seam
//! (a config / manifest / agents doc in → completions / hovers / diagnostics out).
//! The socket serve is the thin runner around those pure functions.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tracing::{debug, error, info, warn};

use crate::agents::AgentRegistry;
use crate::apps::AppRegistry;
use crate::atlas::{CapEntry, CapKind};
use crate::capability::CapDeps;
use crate::config::{Config, DlsConfig};

/// The wildcard tools entry — the orchestrator (darwin) alone holds it and may
/// invoke ANY tool, so it is excluded from the "unknown agent tool" allowlist.
const WILDCARD: &str = "*";

/// A single connection is bounded to this many bytes total (a client that wants
/// more just reconnects). The loopback endpoint is trusted + default-off, but a
/// hard cap keeps a stuck client from growing the read buffer without bound.
const MAX_CONN_BYTES: u64 = 8 * 1024 * 1024;

/// Which document the editor is editing — each maps to a different set of the
/// daemon's own validation rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocKind {
    /// `config/darwin.toml` — KNOWN_KEYS completion/diagnostics + the mode=auto lint.
    Config,
    /// An `apps/<dir>/manifest.toml` — the plugin_sdk manifest validator.
    Manifest { dir_name: String },
    /// `config/agents.toml` — the per-agent tool allowlist.
    Agents,
}

/// A config-key/section completion, from the LIVE `KNOWN_KEYS` registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Completion {
    pub label: String,
    pub kind: CompletionKind,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompletionKind {
    Section,
    Key,
}

/// A hover doc — the section/capability title and a synthesized body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hover {
    pub title: String,
    pub body: String,
}

/// Diagnostic severity, LSP-flavored (`error` blocks parsing/loading; `warning`
/// advises — a typo, an unknown tool, an autonomy lint the daemon still honors).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// Which daemon rule produced a diagnostic — a stable machine key the editor can
/// group/filter on. Every variant is grounded in a real daemon rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Rule {
    /// A top-level section not in `KNOWN_KEYS`.
    UnknownSection,
    /// A key not in its section's `KNOWN_KEYS` entry.
    UnknownKey,
    /// `mode = "auto"` under an autonomy subsystem (self_heal / forge / optimize).
    ModeAutoDanger,
    /// A manifest tool scope not backed by the `[permissions]` block.
    OverPrivilegedManifest,
    /// The specific over-privilege of a `net` scope with empty `net_hosts`.
    NetScopeWithoutHosts,
    /// A manifest that fails the base contract (name != dir, bad names, bad TOML).
    MalformedManifest,
    /// An agent tool literal not in the daemon's tool allowlist.
    UnknownAgentTool,
    /// The document is not valid TOML.
    SyntaxError,
}

/// One diagnostic. `line` is a best-effort 0-based line (LSP convention); `None`
/// when the rule cannot cheaply localize it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub rule: Rule,
    pub message: String,
    pub line: Option<usize>,
}

// ---------------------------------------------------------------------------
// PURE SEAM — completion
// ---------------------------------------------------------------------------

/// Complete config identifiers from the LIVE `KNOWN_KEYS` registry.
///
///   - `section = None`  → complete SECTION names starting with `partial`.
///   - `section = Some(s)` → complete `s`'s KEY names starting with `partial`.
///
/// Deterministic (alphabetical). Grounded in `crate::config::known_keys` — never a
/// second static list that could drift from the parser's own unknown-key rule.
pub fn complete_config(section: Option<&str>, partial: &str) -> Vec<Completion> {
    let registry = crate::config::known_keys();
    let mut out: Vec<Completion> = Vec::new();
    match section {
        None => {
            for (name, keys) in registry {
                if name.starts_with(partial) {
                    out.push(Completion {
                        label: (*name).to_string(),
                        kind: CompletionKind::Section,
                        detail: format!("section — {} keys", keys.len()),
                    });
                }
            }
        }
        Some(sec) => {
            for (name, keys) in registry {
                if *name == sec {
                    for key in *keys {
                        if key.starts_with(partial) {
                            out.push(Completion {
                                label: (*key).to_string(),
                                kind: CompletionKind::Key,
                                detail: format!("[{sec}] key"),
                            });
                        }
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    out
}

/// The `[section]` header the cursor is under, given the doc text UP TO the cursor
/// (so the runner can turn a key-completion request into the right section). Reads
/// the last TOML table header before the cursor; array-of-tables (`[[mcp.servers]]`)
/// and dotted headers (`[voice.voices]`) resolve to their top segment.
pub fn current_section(text_before_cursor: &str) -> Option<String> {
    for line in text_before_cursor.lines().rev() {
        let t = line.trim();
        let inner = t
            .strip_prefix("[[")
            .and_then(|s| s.strip_suffix("]]"))
            .or_else(|| t.strip_prefix('[').and_then(|s| s.strip_suffix(']')));
        if let Some(inner) = inner {
            let head = inner.trim().split('.').next().unwrap_or("").trim();
            if !head.is_empty() {
                return Some(head.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// PURE SEAM — hover
// ---------------------------------------------------------------------------

/// The config sections that map onto a live capability-map row (`capability.rs`),
/// so a section hover can report the subsystem's current armed/ready/inert status
/// grounded in the running config + dependency probes.
const SECTION_CAPABILITY: &[(&str, &str)] = &[
    ("integrations", "consequential_actions"),
    ("shell", "shell_run"),
    ("ui_automation", "ui_actuate"),
    ("docsearch", "file_search"),
    ("distill", "self_distill"),
    ("sync", "federated_sync"),
    ("scene", "acoustic_scene"),
    ("overnight", "overnight_agents"),
    ("self_heal", "self_heal"),
    ("forge", "forge"),
    ("optimize", "optimize"),
    ("proactive", "proactive"),
    ("plugin_sdk", "plugin_sdk"),
    ("mcp", "mcp"),
    ("voice_id", "voice_id"),
];

/// Hover for a config SECTION: its `KNOWN_KEYS` key list, plus — when the section
/// maps to a capability — the live capability label/status/dependency from
/// [`crate::capability::capability_map`]. PURE over the injected config + probes.
pub fn hover_config_section(section: &str, cfg: &Config, deps: &CapDeps) -> Option<Hover> {
    let registry = crate::config::known_keys();
    let mut keys: Option<&[&str]> = None;
    for (name, k) in registry {
        if *name == section {
            keys = Some(*k);
            break;
        }
    }
    let keys = keys?;
    let mut body = format!("Config section [{section}] — keys: {}.", keys.join(", "));

    let mut cap_key: Option<&str> = None;
    for (sec, cap) in SECTION_CAPABILITY {
        if *sec == section {
            cap_key = Some(*cap);
            break;
        }
    }
    if let Some(ck) = cap_key {
        let map = crate::capability::capability_map(cfg, deps);
        if let Some(caps) = map["capabilities"].as_array() {
            if let Some(cap) = caps.iter().find(|c| c["key"] == ck) {
                let label = cap["label"].as_str().unwrap_or("");
                let status = cap["status"].as_str().unwrap_or("");
                let dependency = cap["dependency"].as_str().unwrap_or("");
                body.push_str(&format!("\nLive capability: {label} — status: {status}"));
                if !dependency.is_empty() {
                    body.push_str(&format!(" (needs {dependency})"));
                }
            }
        }
    }
    Some(Hover { title: format!("[{section}]"), body })
}

/// Hover for a capability NAME (a skill / agent / micro-app / integration), read
/// from the assembled capability atlas (`atlas.rs`). PURE over the injected
/// entries.
pub fn hover_capability(name: &str, entries: &[CapEntry]) -> Option<Hover> {
    let e = entries.iter().find(|e| e.name == name)?;
    let state = if e.armed { "armed" } else { "inert" };
    let kind = match e.kind {
        CapKind::Skill => "skill",
        CapKind::Agent => "agent",
        CapKind::App => "app",
        CapKind::Integration => "integration",
    };
    Some(Hover {
        title: e.name.clone(),
        body: format!("{kind} capability — {state}. {}", e.detail),
    })
}

// ---------------------------------------------------------------------------
// PURE SEAM — diagnostics
// ---------------------------------------------------------------------------

/// Lint a document against the daemon's REAL rules. `known_agent_tools` is the
/// daemon's tool allowlist (see [`known_agent_tools`]); it is injected so the
/// function stays pure + unit-testable.
pub fn diagnose(kind: &DocKind, text: &str, known_agent_tools: &BTreeSet<String>) -> Vec<Diagnostic> {
    match kind {
        DocKind::Config => diagnose_config(text),
        DocKind::Manifest { dir_name } => diagnose_manifest(text, dir_name),
        DocKind::Agents => diagnose_agents(text, known_agent_tools),
    }
}

fn diagnose_config(text: &str) -> Vec<Diagnostic> {
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            return vec![Diagnostic {
                severity: Severity::Error,
                rule: Rule::SyntaxError,
                message: format!("config is not valid TOML: {e}"),
                line: None,
            }];
        }
    };
    let registry = crate::config::known_keys();
    let mut out = Vec::new();
    for (section, value) in &table {
        let mut known: Option<&[&str]> = None;
        for (name, keys) in registry {
            if *name == section.as_str() {
                known = Some(*keys);
                break;
            }
        }
        match known {
            None => out.push(Diagnostic {
                severity: Severity::Warning,
                rule: Rule::UnknownSection,
                message: format!("unknown config section [{section}] — the daemon ignores it"),
                line: header_line(text, section),
            }),
            Some(keys) => {
                if let Some(entries) = value.as_table() {
                    for key in entries.keys() {
                        if !keys.contains(&key.as_str()) {
                            out.push(Diagnostic {
                                severity: Severity::Warning,
                                rule: Rule::UnknownKey,
                                message: format!(
                                    "unknown config key {section}.{key} — the daemon ignores it (a typo means a tuning change you believe is active is not)"
                                ),
                                line: key_line(text, key),
                            });
                        }
                    }
                    // mode=auto autonomy lint — grounded: self_heal / forge / optimize
                    // are the sections whose string `mode` accepts "auto".
                    if entries.get("mode").and_then(toml::Value::as_str) == Some("auto") {
                        out.push(Diagnostic {
                            severity: Severity::Warning,
                            rule: Rule::ModeAutoDanger,
                            message: format!(
                                "[{section}].mode = \"auto\" raises autonomy — even so, deploying/applying a proposed artifact stays a separate human step; keep \"propose\" unless you intend the higher-autonomy mode"
                            ),
                            line: key_line(text, "mode"),
                        });
                    }
                }
            }
        }
    }
    out
}

fn diagnose_manifest(text: &str, dir_name: &str) -> Vec<Diagnostic> {
    match crate::plugin_sdk::validate_manifest(text, dir_name) {
        Ok(_) => Vec::new(),
        Err(msg) => {
            // The rule is classified from the validator's OWN message (no re-derived
            // logic): an over-privileged error names the offending scope in quotes,
            // so a `"net"` scope surfaces as the specific net-without-hosts rule.
            let rule = if msg.contains("over-privileged") {
                if msg.contains("\"net\"") {
                    Rule::NetScopeWithoutHosts
                } else {
                    Rule::OverPrivilegedManifest
                }
            } else {
                Rule::MalformedManifest
            };
            vec![Diagnostic { severity: Severity::Error, rule, message: msg, line: None }]
        }
    }
}

fn diagnose_agents(text: &str, known: &BTreeSet<String>) -> Vec<Diagnostic> {
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            return vec![Diagnostic {
                severity: Severity::Error,
                rule: Rule::SyntaxError,
                message: format!("agents file is not valid TOML: {e}"),
                line: None,
            }];
        }
    };
    let mut out = Vec::new();
    let Some(agents) = table.get("agent").and_then(toml::Value::as_array) else {
        return out;
    };
    for agent in agents {
        let Some(tools) = agent.get("tools").and_then(toml::Value::as_array) else {
            continue;
        };
        // The orchestrator holds the wildcard — it may invoke ANY tool — so it is
        // never flagged for a tool "outside its allowlist".
        if tools.iter().any(|t| t.as_str() == Some(WILDCARD)) {
            continue;
        }
        let name = agent.get("name").and_then(toml::Value::as_str).unwrap_or("?");
        for tool in tools {
            if let Some(t) = tool.as_str() {
                if t != WILDCARD && !known.contains(t) {
                    out.push(Diagnostic {
                        severity: Severity::Warning,
                        rule: Rule::UnknownAgentTool,
                        message: format!(
                            "agent {name:?} lists unknown tool {t:?} — not in the daemon's tool allowlist (agents.rs); it will be refused at runtime"
                        ),
                        line: key_line(text, "tools"),
                    });
                }
            }
        }
    }
    out
}

/// Best-effort 0-based line of a section header (`[section]`, `[section.x]`,
/// `[[section]]`, `[[section.x]]`). `None` when not found.
fn header_line(text: &str, section: &str) -> Option<usize> {
    let exact = format!("[{section}]");
    let dotted = format!("[{section}.");
    let arr = format!("[[{section}]]");
    let arr_dotted = format!("[[{section}.");
    text.lines().position(|l| {
        let t = l.trim();
        t == exact || t == arr || t.starts_with(&dotted) || t.starts_with(&arr_dotted)
    })
}

/// Best-effort 0-based line of the FIRST `key = ...` assignment. `None` when not
/// found. Coarse (does not scope to a section), but a diagnostic hint only.
fn key_line(text: &str, key: &str) -> Option<usize> {
    text.lines().position(|l| {
        l.trim_start()
            .split('=')
            .next()
            .map(|lhs| lhs.trim() == key)
            .unwrap_or(false)
    })
}

/// The daemon's tool allowlist: the union of every tool literal any CANONICAL
/// agent holds, excluding the orchestrator wildcard — the authoritative set of
/// tool names the router can dispatch. Grounded in `agents.rs`.
pub fn known_agent_tools() -> BTreeSet<String> {
    AgentRegistry::canonical()
        .all()
        .iter()
        .flat_map(|a| a.tools.iter())
        .filter(|t| t.as_str() != WILDCARD)
        .cloned()
        .collect()
}

/// The secret-free `dls.status` telemetry payload: the master switch, the loopback
/// port, and the registry sizes the server serves from. CONFIG-DERIVED — no socket,
/// no probe.
pub fn status_frame(cfg: &DlsConfig) -> Value {
    let registry = crate::config::known_keys();
    let sections = registry.len();
    let keys: usize = registry.iter().map(|(_, k)| k.len()).sum();
    json!({
        "enabled": cfg.enabled,
        "port": cfg.port,
        "loopback": true,
        "read_only": true,
        "sections": sections,
        "keys": keys,
        "rules": [
            "unknown_section", "unknown_key", "mode_auto_danger",
            "over_privileged_manifest", "net_scope_without_hosts", "unknown_agent_tool",
        ],
        "note": "READ-ONLY loopback LSP: never writes config, takes no action; it only assists a human editor.",
    })
}

// ---------------------------------------------------------------------------
// THIN RUNNER — the LSP-style wire + the loopback socket
// ---------------------------------------------------------------------------

/// An LSP-style request line. The editor shim sends one JSON object per line:
/// `{"id":N,"method":"completion|hover|diagnostics|initialize|shutdown","params":{…}}`.
#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// Everything the pure seams need to answer a request, gathered ONCE when the
/// server starts (the config snapshot + dependency probes + the atlas + the tool
/// allowlist). Holds no writer / actuator — the server is read-only by
/// construction.
pub struct DlsContext {
    pub port: u16,
    pub cfg: Config,
    pub deps: CapDeps,
    pub agent_tools: BTreeSet<String>,
    pub atlas: Vec<CapEntry>,
}

/// Gather the read-only context the server answers from: the dependency probes
/// (cloud key / pdfjail / sandbox-exec, exactly as `capability::emit_map`), the
/// live capability atlas, and the tool allowlist. Async only because the probes
/// are — run it once at startup.
pub async fn build_context(cfg: &Config, agents: &AgentRegistry, apps: &AppRegistry) -> DlsContext {
    let deps = CapDeps {
        cloud_key: crate::anthropic::resolve_api_key().await.is_some(),
        pdfjail: crate::docsearch::pdfjail_available(),
        sandbox_exec: std::path::Path::new(crate::apps::SANDBOX_EXEC).exists(),
    };
    let atlas = crate::atlas::build_entries(cfg, agents, apps).await;
    DlsContext {
        port: cfg.dls.port,
        cfg: cfg.clone(),
        deps,
        agent_tools: known_agent_tools(),
        atlas,
    }
}

/// Dispatch one request to the pure seams and build the JSON response. PURE +
/// SYNC — it computes completions/hovers/diagnostics and NOTHING else. There is no
/// write / edit / applyEdit method: an unsupported method is REFUSED, never
/// silently performed.
pub fn handle(req: &Request, ctx: &DlsContext) -> Value {
    match req.method.as_str() {
        "initialize" => ok(
            &req.id,
            json!({
                "server": "darwin-language-server",
                "read_only": true,
                "capabilities": ["completion", "hover", "diagnostics"],
                "note": "READ-ONLY: this server never writes config and takes no action; it only assists a human editor. It exposes no write/edit/applyEdit method by design.",
            }),
        ),
        "completion" => {
            // The shim may name the section explicitly, or send the doc text UP TO
            // the cursor and let the server derive the current [section] header.
            let explicit = req.params.get("section").and_then(Value::as_str).map(str::to_string);
            let derived = req
                .params
                .get("text_before_cursor")
                .and_then(Value::as_str)
                .and_then(current_section);
            let section = explicit.or(derived);
            let partial = req.params.get("partial").and_then(Value::as_str).unwrap_or("");
            let items = match parse_kind(&req.params) {
                Some(DocKind::Config) => complete_config(section.as_deref(), partial),
                _ => Vec::new(),
            };
            ok(&req.id, json!({ "items": items }))
        }
        "hover" => {
            let symbol = req.params.get("symbol").and_then(Value::as_str).unwrap_or("");
            let hover = match parse_kind(&req.params) {
                Some(DocKind::Config) => hover_config_section(symbol, &ctx.cfg, &ctx.deps)
                    .or_else(|| hover_capability(symbol, &ctx.atlas)),
                _ => hover_capability(symbol, &ctx.atlas),
            };
            ok(&req.id, json!({ "hover": hover }))
        }
        "diagnostics" => match parse_kind(&req.params) {
            Some(kind) => {
                let text = req.params.get("text").and_then(Value::as_str).unwrap_or("");
                let diagnostics = diagnose(&kind, text, &ctx.agent_tools);
                ok(&req.id, json!({ "diagnostics": diagnostics }))
            }
            None => err(&req.id, "diagnostics requires params.kind of \"config\" | \"manifest\" | \"agents\""),
        },
        "shutdown" => ok(&req.id, json!({ "ok": true })),
        other => err(
            &req.id,
            &format!(
                "unsupported method {other:?}: the DARWIN Language Server is strictly READ-ONLY (completion / hover / diagnostics only) — it has no write / edit / applyEdit capability and never mutates config"
            ),
        ),
    }
}

fn ok(id: &Value, result: Value) -> Value {
    json!({ "id": id, "result": result })
}

fn err(id: &Value, message: &str) -> Value {
    json!({ "id": id, "error": { "message": message } })
}

fn parse_kind(params: &Value) -> Option<DocKind> {
    match params.get("kind").and_then(Value::as_str)? {
        "config" => Some(DocKind::Config),
        "agents" => Some(DocKind::Agents),
        "manifest" => {
            let dir = params
                .get("dir_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some(DocKind::Manifest { dir_name: dir })
        }
        _ => None,
    }
}

fn process_line(line: &str, ctx: &DlsContext) -> String {
    match serde_json::from_str::<Request>(line) {
        Ok(req) => handle(&req, ctx).to_string(),
        Err(e) => json!({ "error": { "message": format!("invalid JSON request: {e}") } }).to_string(),
    }
}

/// Serve the read-only LSP endpoint on 127.0.0.1:`ctx.port` (LOOPBACK ONLY — a
/// non-loopback bind is never attempted). One newline-delimited JSON request →
/// one newline-delimited JSON response. The runner NEVER writes any file; it only
/// calls the pure seams and writes their answers back.
pub async fn serve(ctx: Arc<DlsContext>) {
    let addr = format!("127.0.0.1:{}", ctx.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr, error = %e, "DLS server failed to bind");
            return;
        }
    };
    info!(addr, "DARWIN Language Server (read-only, loopback) listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(error = %e, "DLS accept failed");
                continue;
            }
        };
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let (rd, mut wr) = stream.into_split();
            // Bound the whole connection so a stuck client can't grow the buffer
            // without limit (a client wanting more simply reconnects).
            let reader = BufReader::new(rd.take(MAX_CONN_BYTES));
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let response = process_line(&line, &ctx);
                if wr.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
                if wr.write_all(b"\n").await.is_err() {
                    break;
                }
            }
            debug!(%peer, "DLS client disconnected");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn ctx() -> DlsContext {
        DlsContext {
            port: 0,
            cfg: Config::default(),
            deps: CapDeps { cloud_key: false, pdfjail: false, sandbox_exec: true },
            agent_tools: known_agent_tools(),
            // A minimal atlas: no skills/agents/apps, but the integration rows are
            // always present (all inert with no credentials) — enough for a
            // capability-name hover test.
            atlas: crate::atlas::assemble(&[], true, &[], &[], &HashSet::new()),
        }
    }

    // -- completion ----------------------------------------------------------

    #[test]
    fn completes_section_names_from_the_live_registry() {
        let items = complete_config(None, "voi");
        let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
        // Both "voice" and "voice_id" are real KNOWN_KEYS sections starting "voi".
        assert!(labels.contains(&"voice"), "voice section completed: {labels:?}");
        assert!(labels.contains(&"voice_id"), "voice_id section completed: {labels:?}");
        assert!(items.iter().all(|c| c.kind == CompletionKind::Section));
    }

    #[test]
    fn completes_keys_within_a_section() {
        let items = complete_config(Some("audio"), "rms");
        let labels: Vec<&str> = items.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(labels, vec!["rms_threshold"], "the one [audio] key starting rms");
        assert!(items.iter().all(|c| c.kind == CompletionKind::Key));
        // A partial that matches an early prefix returns several keys, sorted.
        let more = complete_config(Some("audio"), "barge");
        assert!(more.len() >= 3, "barge_in / barge_in_rms / barge_in_ms: {more:?}");
        let mut sorted = more.clone();
        sorted.sort_by(|a, b| a.label.cmp(&b.label));
        assert_eq!(more, sorted, "completions come back sorted");
    }

    #[test]
    fn completion_of_an_unknown_section_or_partial_is_empty() {
        assert!(complete_config(None, "zzz_no_such").is_empty());
        assert!(complete_config(Some("no_such_section"), "").is_empty());
        assert!(complete_config(Some("audio"), "zzz").is_empty());
    }

    #[test]
    fn current_section_reads_the_last_header() {
        let doc = "[audio]\nrms_threshold = 0.02\n\n[speech]\nvoice = \"bf_emma\"\nsp";
        assert_eq!(current_section(doc).as_deref(), Some("speech"));
        // Array-of-tables + dotted headers resolve to the top segment.
        assert_eq!(current_section("[[mcp.servers]]\ncmd").as_deref(), Some("mcp"));
        assert_eq!(current_section("[voice.voices]\nx").as_deref(), Some("voice"));
        // A comment is not a header.
        assert_eq!(current_section("# [audio]\nfoo").as_deref(), None);
    }

    // -- hover ---------------------------------------------------------------

    #[test]
    fn hover_on_a_section_is_synthesized_from_the_capability_atlas() {
        let cfg = Config::default();
        let deps = CapDeps { cloud_key: false, pdfjail: false, sandbox_exec: true };
        // [shell] maps to the shell_run capability; the body carries its live status.
        let h = hover_config_section("shell", &cfg, &deps).expect("shell hover");
        assert_eq!(h.title, "[shell]");
        assert!(h.body.contains("keys: enabled"), "lists the section's keys: {}", h.body);
        assert!(h.body.contains("Live capability"), "carries the capability line: {}", h.body);
        assert!(
            h.body.to_lowercase().contains("shell"),
            "names the shell capability: {}",
            h.body
        );
    }

    #[test]
    fn hover_on_a_plain_section_lists_keys_without_a_capability_line() {
        let cfg = Config::default();
        let deps = CapDeps { cloud_key: false, pdfjail: false, sandbox_exec: true };
        // [audio] has no capability-map row — just the key list.
        let h = hover_config_section("audio", &cfg, &deps).expect("audio hover");
        assert!(h.body.contains("rms_threshold"), "lists keys: {}", h.body);
        assert!(!h.body.contains("Live capability"), "no capability line: {}", h.body);
    }

    #[test]
    fn hover_on_an_unknown_section_is_none() {
        let cfg = Config::default();
        let deps = CapDeps { cloud_key: false, pdfjail: false, sandbox_exec: true };
        assert!(hover_config_section("no_such_section", &cfg, &deps).is_none());
    }

    #[test]
    fn hover_on_a_capability_name_reads_the_atlas() {
        // GitHub is an atlas Integration row; with no credentials it is inert.
        let entries = crate::atlas::assemble(&[], true, &[], &[], &HashSet::new());
        let h = hover_capability("GitHub", &entries).expect("GitHub capability hover");
        assert_eq!(h.title, "GitHub");
        assert!(h.body.starts_with("integration capability"), "kind+state: {}", h.body);
        assert!(h.body.contains("inert"), "no credentials -> inert: {}", h.body);
        assert!(hover_capability("no-such-capability", &entries).is_none());
    }

    // -- diagnostics: config -------------------------------------------------

    #[test]
    fn config_unknown_section_and_key_are_flagged_against_known_keys() {
        let diags = diagnose(&DocKind::Config, "[made_up]\nfoo = 1\n", &BTreeSet::new());
        assert!(
            diags.iter().any(|d| d.rule == Rule::UnknownSection && d.message.contains("made_up")),
            "unknown section flagged: {diags:?}"
        );

        let diags = diagnose(&DocKind::Config, "[audio]\nnot_a_key = 1\n", &BTreeSet::new());
        assert!(
            diags.iter().any(|d| d.rule == Rule::UnknownKey && d.message.contains("audio.not_a_key")),
            "unknown key flagged: {diags:?}"
        );
    }

    #[test]
    fn config_mode_auto_is_flagged_as_a_danger_lint() {
        let diags = diagnose(&DocKind::Config, "[self_heal]\nenabled = true\nmode = \"auto\"\n", &BTreeSet::new());
        let lint = diags.iter().find(|d| d.rule == Rule::ModeAutoDanger).expect("mode=auto lint");
        assert_eq!(lint.severity, Severity::Warning);
        assert!(lint.message.contains("auto"), "explains the autonomy: {}", lint.message);
        // mode = "propose" is NOT flagged.
        let ok = diagnose(&DocKind::Config, "[self_heal]\nmode = \"propose\"\n", &BTreeSet::new());
        assert!(ok.iter().all(|d| d.rule != Rule::ModeAutoDanger), "propose is fine: {ok:?}");
    }

    #[test]
    fn a_clean_config_produces_no_diagnostics() {
        let clean = "[audio]\nrms_threshold = 0.02\n\n[self_heal]\nenabled = true\nmode = \"propose\"\n\n[telemetry]\nport = 7177\n";
        assert!(diagnose(&DocKind::Config, clean, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_config_syntax_error_is_reported_not_panicked() {
        let diags = diagnose(&DocKind::Config, "this is = = not toml", &BTreeSet::new());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, Rule::SyntaxError);
        assert_eq!(diags[0].severity, Severity::Error);
    }

    // -- diagnostics: manifest -----------------------------------------------

    const OVERPRIV_FS_WRITE: &str = r#"
        [app]
        name = "q"
        version = "0.1.0"
        description = "over-privileged fs_write declaration"
        entry = "q"
        runtime = "binary"

        [permissions]
        fs_write = []

        [[tools.exposes]]
        name = "q.write"
        scopes = ["fs_write"]
    "#;

    const NET_WITHOUT_HOSTS: &str = r#"
        [app]
        name = "netty"
        version = "0.1.0"
        description = "a net scope with no net_hosts"
        entry = "netty"
        runtime = "binary"

        [permissions]
        net_hosts = []

        [[tools.exposes]]
        name = "netty.fetch"
        scopes = ["net"]
    "#;

    const GOOD_MANIFEST: &str = r#"
        [app]
        name = "netty"
        version = "0.1.0"
        description = "a plugin with a backed net scope"
        entry = "netty"
        runtime = "binary"

        [permissions]
        net_hosts = ["example.com"]

        [[tools.exposes]]
        name = "netty.fetch"
        scopes = ["net"]
    "#;

    #[test]
    fn an_over_privileged_manifest_is_flagged_via_validate_manifest() {
        let kind = DocKind::Manifest { dir_name: "q".to_string() };
        let diags = diagnose(&kind, OVERPRIV_FS_WRITE, &BTreeSet::new());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, Rule::OverPrivilegedManifest);
        assert_eq!(diags[0].severity, Severity::Error);
        assert!(diags[0].message.contains("over-privileged") && diags[0].message.contains("fs_write"));
    }

    #[test]
    fn a_net_scope_without_hosts_is_flagged_as_its_own_rule() {
        let kind = DocKind::Manifest { dir_name: "netty".to_string() };
        let diags = diagnose(&kind, NET_WITHOUT_HOSTS, &BTreeSet::new());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, Rule::NetScopeWithoutHosts);
        assert!(diags[0].message.contains("net"), "names the net scope: {}", diags[0].message);
    }

    #[test]
    fn a_clean_manifest_produces_no_diagnostics() {
        let kind = DocKind::Manifest { dir_name: "netty".to_string() };
        assert!(diagnose(&kind, GOOD_MANIFEST, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn a_malformed_manifest_is_flagged() {
        // name != directory — the base contract violation.
        let kind = DocKind::Manifest { dir_name: "wrong-dir".to_string() };
        let diags = diagnose(&kind, GOOD_MANIFEST, &BTreeSet::new());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].rule, Rule::MalformedManifest);
    }

    // -- diagnostics: agents -------------------------------------------------

    #[test]
    fn an_unknown_agent_tool_is_flagged_against_the_allowlist() {
        let known = known_agent_tools();
        let doc = r#"
            [[agent]]
            name = "rogue"
            tools = ["conversation", "launch_missiles"]
            namespace = "agent.rogue"
        "#;
        let diags = diagnose(&DocKind::Agents, doc, &known);
        assert_eq!(diags.len(), 1, "only the fake tool is unknown: {diags:?}");
        assert_eq!(diags[0].rule, Rule::UnknownAgentTool);
        assert!(diags[0].message.contains("launch_missiles"), "names it: {}", diags[0].message);
        assert!(diags[0].message.contains("rogue"), "names the agent: {}", diags[0].message);
    }

    #[test]
    fn a_clean_agents_file_produces_no_diagnostics_and_skips_the_orchestrator() {
        let known = known_agent_tools();
        // darwin holds the wildcard (any tool) and must not be flagged; friday's tools
        // are all real allowlist entries.
        let doc = r#"
            [[agent]]
            name = "darwin"
            tools = ["*"]
            namespace = "agent.darwin"

            [[agent]]
            name = "friday"
            tools = ["conversation", "recall_facts", "world_query"]
            namespace = "agent.friday"
        "#;
        assert!(diagnose(&DocKind::Agents, doc, &known).is_empty());
    }

    #[test]
    fn known_agent_tools_unions_the_roster_and_drops_the_wildcard() {
        let known = known_agent_tools();
        assert!(known.contains("conversation"), "a common tool is known");
        assert!(known.contains("web_search"), "a specialist tool is known");
        assert!(!known.contains("*"), "the orchestrator wildcard is not a tool");
        assert!(known.len() > 20, "the roster union is broad: {}", known.len());
    }

    // -- the runner is READ-ONLY --------------------------------------------

    #[test]
    fn the_server_refuses_every_write_method_and_never_returns_an_edit() {
        let ctx = ctx();
        // A caller trying to make it write / edit is REFUSED (error, no result).
        for method in ["workspace/applyEdit", "config/write", "apply", "mutate", "setConfig"] {
            let req = Request { id: json!(1), method: method.to_string(), params: json!({}) };
            let resp = handle(&req, &ctx);
            assert!(resp.get("error").is_some(), "{method} must be refused: {resp}");
            assert!(resp.get("result").is_none(), "{method} must not produce a result: {resp}");
            let text = resp.to_string();
            assert!(text.contains("READ-ONLY"), "the refusal states read-only: {text}");
        }
        // Every SUPPORTED method returns only read-only results — the result object
        // never carries an edit/write instruction (a key, not the honest prose note
        // in `initialize` that says it has NO such capability).
        for method in ["initialize", "completion", "hover", "diagnostics", "shutdown"] {
            let req = Request {
                id: json!(2),
                method: method.to_string(),
                params: json!({ "kind": "config", "text": "[audio]\nrms_threshold = 0.02\n" }),
            };
            let resp = handle(&req, &ctx);
            let result = &resp["result"];
            for forbidden in ["edit", "changes", "workspaceEdit", "applyEdit", "write"] {
                assert!(
                    result.get(forbidden).is_none(),
                    "{method} result must carry no {forbidden:?} field: {resp}"
                );
            }
        }
    }

    #[test]
    fn wire_completion_dispatch_returns_registry_items() {
        let ctx = ctx();
        let req = Request {
            id: json!(7),
            method: "completion".to_string(),
            params: json!({ "kind": "config", "section": "audio", "partial": "rms" }),
        };
        let resp = handle(&req, &ctx);
        let items = resp["result"]["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["label"], "rms_threshold");
        assert_eq!(resp["id"], json!(7));
    }

    #[test]
    fn wire_completion_derives_the_section_from_doc_text() {
        let ctx = ctx();
        // No explicit "section" — the server reads the last header from the text.
        let req = Request {
            id: json!(9),
            method: "completion".to_string(),
            params: json!({
                "kind": "config",
                "text_before_cursor": "[audio]\nrms_threshold = 0.02\nbarge",
                "partial": "barge",
            }),
        };
        let resp = handle(&req, &ctx);
        let items = resp["result"]["items"].as_array().expect("items array");
        assert!(!items.is_empty(), "derived [audio] section keys: {resp}");
        assert!(items.iter().all(|i| i["label"].as_str().unwrap().starts_with("barge")));
    }

    #[test]
    fn wire_diagnostics_dispatch_flags_an_unknown_key() {
        let ctx = ctx();
        let req = Request {
            id: json!(8),
            method: "diagnostics".to_string(),
            params: json!({ "kind": "config", "text": "[audio]\nbogus = 1\n" }),
        };
        let resp = handle(&req, &ctx);
        let diags = resp["result"]["diagnostics"].as_array().expect("diagnostics array");
        assert!(diags.iter().any(|d| d["rule"] == "unknown_key"), "flagged: {resp}");
    }

    #[test]
    fn status_frame_is_secret_free_and_config_derived() {
        let frame = status_frame(&DlsConfig::default());
        assert_eq!(frame["enabled"], json!(false), "ships off");
        assert_eq!(frame["read_only"], json!(true));
        assert_eq!(frame["loopback"], json!(true));
        assert!(frame["sections"].as_u64().unwrap() > 60, "reports the registry size");
        assert!(frame["keys"].as_u64().unwrap() > 60);
    }
}
