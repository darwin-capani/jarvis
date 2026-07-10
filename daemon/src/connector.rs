//! Connector Add — the CONSEQUENTIAL "add an MCP connector" action.
//!
//! Adding an MCP connector is a real, persistent mutation of the user's machine
//! posture, so it runs ONLY through the strongest gate JARVIS has: it is in
//! `confirm::CONSEQUENTIAL_TOOLS`, so `execute_tool` PARKS it and replays it only
//! after a fresh, cross-turn SPOKEN "yes" on the exact spec (name, transport,
//! url/command). Nothing is written until that confirmation fires.
//!
//! SAFETY CONTRACT (non-negotiable):
//!   * NO SECRET EVER TRANSITS THE MODEL. This tool takes NO token argument
//!     (`deny_unknown_fields` makes a stray one a hard parse error, not a leak).
//!     A connector's auth token is placed in the Keychain OUT-OF-BAND — by the
//!     user, via Settings or the exact `security add-generic-password` line in
//!     the success report — exactly like the OAuth client secrets. The daemon
//!     only ever READS `mcp_<name>_token` at connect; it never writes it here.
//!   * ADDED INERT. The new `[[mcp.servers]]` block ships `agents = []` (no agent
//!     may use it) and `default_class = "consequential"` (every tool on it still
//!     parks). Adding a connector therefore NEVER grants any capability — the
//!     user separately grants agents and restarts to actually connect. "Armed by
//!     default, gated per action" is preserved: an added-but-ungranted connector
//!     is ON-but-inert.
//!   * SURGICAL, ATOMIC CONFIG EDIT. jarvis.toml is a hand-authored file the
//!     daemon otherwise never rewrites; this APPENDS one validated block to the
//!     end (temp file + rename) and touches nothing else — no reparse-and-
//!     reserialize that could drop the user's comments, formatting, or
//!     server-side-only keys.
//!   * VETTED SHAPE. The server name must pass the same strict
//!     `integrations::mcp_token_account` guard that mints its Keychain account;
//!     an http endpoint must be `https://` (no plaintext bearer); a stdio command
//!     must be an absolute path that exists on disk (no PATH search, no shell). A
//!     duplicate name is refused (no silent overwrite).
//!
//! The live tools/list handshake on add (and a Settings UI for the dynamic
//! per-connector token) are deliberate follow-ons; a freshly added connector
//! connects on the next start via the existing `McpManager::connect_all` boot
//! path, which is how every MCP server comes up.

use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use tracing::info;

use crate::config::Config;
use crate::integrations::{gate, mcp_token_account, ActionMode};
use crate::mcp::is_https_url;

/// Process-global path to `config/jarvis.toml`, installed once at startup (the
/// same OnceLock pattern as the optimizer trace store) so the dispatch layer can
/// reach the config file without threading a path through every tool signature.
static CONFIG_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Install the config path (called once from `main`). A second call is ignored.
pub fn set_config_path(path: PathBuf) {
    let _ = CONFIG_PATH.set(path);
}

fn config_path() -> Option<&'static PathBuf> {
    CONFIG_PATH.get()
}

/// The two MCP transports JARVIS understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Http,
    Stdio,
}

impl Transport {
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "http" => Some(Self::Http),
            "stdio" => Some(Self::Stdio),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Stdio => "stdio",
        }
    }
}

/// The tool's argument contract. `deny_unknown_fields` is load-bearing security:
/// it turns any field we did not name — most importantly a sneaked `token` — into
/// a hard parse error, so a secret can never ride in via the model and land in
/// the parked/audited input. There is intentionally NO secret field.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorRequest {
    /// Server id; must be the strict `[a-z0-9_-]+` shape (validated below).
    pub name: String,
    /// "http" or "stdio".
    pub transport: String,
    /// http: the `https://` endpoint URL. Ignored for stdio.
    #[serde(default)]
    pub url: String,
    /// stdio: the ABSOLUTE interpreter/binary to spawn. Ignored for http.
    #[serde(default)]
    pub command: String,
    /// stdio: argv after `command`. Ignored for http.
    #[serde(default)]
    pub args: Vec<String>,
    /// Whether the server authenticates with a token at `mcp_<name>_token`. The
    /// token itself is supplied out-of-band — never here.
    #[serde(default)]
    pub uses_token: bool,
    /// The spoken-confirmation flag the park/replay machinery sets. The model's
    /// own value never executes anything — only a real human "yes" replays the
    /// parked action with this true.
    #[serde(default)]
    pub confirm: bool,
}

/// A validated connector spec (NO secret).
#[derive(Debug, Clone)]
struct Spec {
    name: String,
    transport: Transport,
    url: String,
    command: String,
    args: Vec<String>,
    uses_token: bool,
}

/// Add an MCP connector. CONSEQUENTIAL: in DryRun (the park preview, and the OFF
/// master switch) it returns the faithful preview and writes NOTHING; in Execute
/// (a replayed spoken "yes") it appends the validated block to jarvis.toml. A
/// validation failure returns `Err` so the action errors out rather than parking.
pub async fn add_connector(req: ConnectorRequest) -> Result<String> {
    let transport = Transport::parse(&req.transport)
        .ok_or_else(|| anyhow!("transport must be 'http' or 'stdio', got '{}'", req.transport))?;
    let spec = Spec {
        name: req.name.trim().to_string(),
        transport,
        url: req.url.trim().to_string(),
        command: req.command.trim().to_string(),
        args: req.args,
        uses_token: req.uses_token,
    };

    let path = config_path().ok_or_else(|| anyhow!("connector_add: the config path is not available"))?;

    // Authoritative duplicate check via the real config parser (not a hand
    // scanner): a name already present is refused, never silently overwritten.
    let (existing, _issues) = Config::load(path);
    let existing_names: Vec<String> = existing.mcp.servers.iter().map(|s| s.name.clone()).collect();

    validate(&spec, &existing_names)?;
    // IO-bound check kept out of the pure validator: a stdio command must exist
    // on disk as a real file (we never spawn through a PATH search or a shell).
    if spec.transport == Transport::Stdio {
        let cmd = std::path::Path::new(&spec.command);
        if !cmd.is_file() {
            return Err(anyhow!(
                "stdio command '{}' is not a file on disk; give the absolute path to the interpreter/binary",
                spec.command
            ));
        }
    }

    match gate(req.confirm) {
        ActionMode::DryRun => Ok(preview(&spec)),
        ActionMode::Execute => {
            let block = render_block(&spec);
            let raw = std::fs::read_to_string(path)
                .map_err(|e| anyhow!("connector_add: cannot read {}: {e}", path.display()))?;
            let updated = appended_text(&raw, &block);
            atomic_write(path, &updated)?;
            // INERT-by-construction; the name is safe (validated); no secret here.
            info!(connector = %spec.name, transport = spec.transport.as_str(), "connector: added (inert) to config");
            Ok(success_report(&spec))
        }
    }
}

// ---------------------------------------------------------------------------
// Validation + rendering (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Structural validation (pure). Name must mint a Keychain account (the strict
/// `[a-z0-9_-]+` shape); the name must be unused; http needs an `https://` url
/// and stdio needs an absolute command. Returns a friendly `Err` otherwise.
fn validate(spec: &Spec, existing_names: &[String]) -> Result<()> {
    if mcp_token_account(&spec.name).is_none() {
        return Err(anyhow!(
            "'{}' is not a valid connector name — use lowercase letters, digits, and single _ or - separators (no leading/trailing/double separator)",
            spec.name
        ));
    }
    if existing_names.iter().any(|n| n == &spec.name) {
        return Err(anyhow!(
            "a connector named '{}' already exists in the config — pick a different name",
            spec.name
        ));
    }
    match spec.transport {
        Transport::Http => {
            if spec.url.is_empty() {
                return Err(anyhow!("an http connector needs a 'url'"));
            }
            if !is_https_url(&spec.url) {
                return Err(anyhow!(
                    "the connector url must be https:// (a bearer token must never ride plaintext), got '{}'",
                    spec.url
                ));
            }
        }
        Transport::Stdio => {
            if spec.command.is_empty() {
                return Err(anyhow!("a stdio connector needs a 'command' (the absolute interpreter/binary path)"));
            }
            if !std::path::Path::new(&spec.command).is_absolute() {
                return Err(anyhow!(
                    "the stdio command must be an ABSOLUTE path (no PATH search, no shell), got '{}'",
                    spec.command
                ));
            }
        }
    }
    Ok(())
}

/// Escape a string for a double-quoted TOML basic string (backslash + quote).
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Render the `[[mcp.servers]]` block for `spec`. Always ships `agents = []`
/// (inert) and `default_class = "consequential"` (every tool gated). Only the
/// fields relevant to the transport are emitted.
fn render_block(spec: &Spec) -> String {
    let mut b = String::new();
    b.push_str("[[mcp.servers]]\n");
    b.push_str(&format!("name = \"{}\"\n", toml_escape(&spec.name)));
    b.push_str(&format!("transport = \"{}\"\n", spec.transport.as_str()));
    match spec.transport {
        Transport::Http => {
            b.push_str(&format!("url = \"{}\"\n", toml_escape(&spec.url)));
        }
        Transport::Stdio => {
            b.push_str(&format!("command = \"{}\"\n", toml_escape(&spec.command)));
            let args = spec
                .args
                .iter()
                .map(|a| format!("\"{}\"", toml_escape(a)))
                .collect::<Vec<_>>()
                .join(", ");
            b.push_str(&format!("args = [{args}]\n"));
        }
    }
    b.push_str(&format!("uses_token = {}\n", spec.uses_token));
    // INERT by construction: no agent may use it, every tool stays gated.
    b.push_str("agents = []\n");
    b.push_str("default_class = \"consequential\"\n");
    b.push_str("read_only_tools = []\n");
    b
}

/// Concatenate `block` onto `existing`, guaranteeing a blank line of separation
/// and a trailing newline. Pure (the IO half is `atomic_write`).
fn appended_text(existing: &str, block: &str) -> String {
    let mut out = String::from(existing);
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    // One blank line before the new block for readability.
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// The faithful DryRun preview the user confirms (and the secret-free summary the
/// audit log keeps). Names the exact spec and states the inert posture.
fn preview(spec: &Spec) -> String {
    let detail = match spec.transport {
        Transport::Http => format!("https endpoint {}", spec.url),
        Transport::Stdio => {
            if spec.args.is_empty() {
                format!("command {}", spec.command)
            } else {
                format!("command {} {}", spec.command, spec.args.join(" "))
            }
        }
    };
    format!(
        "Add the MCP connector '{name}' ({transport}: {detail}){token}? It will be written to config/jarvis.toml \
         INERT — no agent may use it and every tool stays gated until you grant agents and restart. \
         I never handle the secret. Say yes to confirm.",
        name = spec.name,
        transport = spec.transport.as_str(),
        token = if spec.uses_token { ", needs a token" } else { "" },
    )
}

/// The success report after the block is written. Tells the user the exact,
/// out-of-band steps to make the connector live — the secret never touches JARVIS.
fn success_report(spec: &Spec) -> String {
    let account = mcp_token_account(&spec.name).unwrap_or_else(|| format!("mcp_{}_token", spec.name));
    let token_step = if spec.uses_token {
        format!(
            "\n  1. Store its token in the Keychain yourself (I never see it). Run this \
             and paste the token at the prompt (so it never lands in your shell history \
             or the process list): \
             security add-generic-password -U -s com.jarvis.daemon -a {account} -w \
             (or paste it in Settings)."
        )
    } else {
        String::new()
    };
    format!(
        "Added MCP connector '{name}' ({transport}) to config/jarvis.toml. It is INERT — no agent may use it \
         and every tool on it stays gated. To make it live:{token_step}\n  {grant}. Grant the agents you want in \
         that block's `agents = []`.\n  {restart}. Restart JARVIS so it connects.",
        name = spec.name,
        transport = spec.transport.as_str(),
        grant = if spec.uses_token { "2" } else { "1" },
        restart = if spec.uses_token { "3" } else { "2" },
    )
}

// ---------------------------------------------------------------------------
// Atomic write (IO)
// ---------------------------------------------------------------------------

/// Write `content` to `path` atomically: write a sibling temp file, flush+sync,
/// then rename over the target. A crash mid-write leaves the original intact.
fn atomic_write(path: &std::path::Path, content: &str) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("toml.connector-tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| anyhow!("connector_add: cannot create temp file {}: {e}", tmp.display()))?;
        f.write_all(content.as_bytes())
            .map_err(|e| anyhow!("connector_add: cannot write temp file: {e}"))?;
        f.sync_all()
            .map_err(|e| anyhow!("connector_add: cannot fsync temp file: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow!("connector_add: cannot replace {}: {e}", path.display())
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_spec() -> Spec {
        Spec {
            name: "files".into(),
            transport: Transport::Http,
            url: "https://mcp.example.com/sse".into(),
            command: String::new(),
            args: vec![],
            uses_token: true,
        }
    }

    fn stdio_spec() -> Spec {
        Spec {
            name: "local-fs".into(),
            transport: Transport::Stdio,
            url: String::new(),
            command: "/usr/local/bin/mcp-fs".into(),
            args: vec!["--root".into(), "/data".into()],
            uses_token: false,
        }
    }

    #[test]
    fn transport_parses_known_only() {
        assert_eq!(Transport::parse("http"), Some(Transport::Http));
        assert_eq!(Transport::parse(" stdio "), Some(Transport::Stdio));
        assert_eq!(Transport::parse("ws"), None);
        assert_eq!(Transport::parse(""), None);
    }

    #[test]
    fn validate_accepts_good_specs() {
        assert!(validate(&http_spec(), &[]).is_ok());
        assert!(validate(&stdio_spec(), &[]).is_ok());
    }

    #[test]
    fn validate_rejects_bad_names() {
        let mut s = http_spec();
        s.name = "Bad Name".into();
        assert!(validate(&s, &[]).is_err());
        s.name = "_lead".into();
        assert!(validate(&s, &[]).is_err());
        s.name = "double__under".into();
        assert!(validate(&s, &[]).is_err());
        s.name = "../etc".into();
        assert!(validate(&s, &[]).is_err());
    }

    #[test]
    fn validate_rejects_duplicate_name() {
        let s = http_spec();
        let err = validate(&s, &["files".to_string()]).unwrap_err().to_string();
        assert!(err.contains("already exists"), "got: {err}");
    }

    #[test]
    fn validate_enforces_https_for_http() {
        let mut s = http_spec();
        s.url = "http://insecure.example.com".into();
        assert!(validate(&s, &[]).is_err());
        s.url = String::new();
        assert!(validate(&s, &[]).is_err());
    }

    #[test]
    fn validate_requires_absolute_stdio_command() {
        let mut s = stdio_spec();
        s.command = "mcp-fs".into(); // relative -> rejected (no PATH search)
        assert!(validate(&s, &[]).is_err());
        s.command = String::new();
        assert!(validate(&s, &[]).is_err());
    }

    #[test]
    fn render_http_block_is_inert_and_https() {
        let b = render_block(&http_spec());
        assert!(b.contains("[[mcp.servers]]"));
        assert!(b.contains("name = \"files\""));
        assert!(b.contains("transport = \"http\""));
        assert!(b.contains("url = \"https://mcp.example.com/sse\""));
        assert!(b.contains("uses_token = true"));
        assert!(b.contains("agents = []"), "must be added INERT");
        assert!(b.contains("default_class = \"consequential\""));
        assert!(!b.contains("command"));
    }

    #[test]
    fn render_stdio_block_has_command_and_args() {
        let b = render_block(&stdio_spec());
        assert!(b.contains("transport = \"stdio\""));
        assert!(b.contains("command = \"/usr/local/bin/mcp-fs\""));
        assert!(b.contains("args = [\"--root\", \"/data\"]"));
        assert!(b.contains("uses_token = false"));
        assert!(b.contains("agents = []"));
    }

    #[test]
    fn toml_escape_handles_quotes_and_backslashes() {
        assert_eq!(toml_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn appended_text_separates_and_preserves() {
        let existing = "[mcp]\nenabled = true\n";
        let block = render_block(&http_spec());
        let out = appended_text(existing, &block);
        // Original kept verbatim at the head.
        assert!(out.starts_with("[mcp]\nenabled = true\n"));
        // Blank-line separated, block appended, trailing newline.
        assert!(out.contains("\n\n[[mcp.servers]]"));
        assert!(out.ends_with('\n'));
        // Idempotent shape: appending to empty still yields the block.
        let from_empty = appended_text("", &block);
        assert!(from_empty.starts_with("[[mcp.servers]]"));
    }

    #[test]
    fn preview_states_inert_and_names_target() {
        let p = preview(&http_spec());
        assert!(p.contains("files"));
        assert!(p.contains("INERT"));
        assert!(p.contains("https://mcp.example.com/sse"));
        assert!(p.contains("needs a token"));
        assert!(p.contains("Say yes"));
    }

    #[test]
    fn success_report_gives_out_of_band_token_step() {
        let r = success_report(&http_spec());
        assert!(r.contains("INERT"));
        assert!(r.contains("security add-generic-password"));
        assert!(r.contains("mcp_files_token"));
        assert!(r.contains("Restart JARVIS"));
        // A no-token connector omits the keychain step.
        let r2 = success_report(&stdio_spec());
        assert!(!r2.contains("security add-generic-password"));
    }
}
