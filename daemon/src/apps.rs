//! Micro-app runtime substrate — the host side of docs/SANDBOX.md.
//!
//! Each micro-app is a SEPARATE process launched by darwind, never run in the
//! daemon's address space. At launch the host:
//!   1. parses `apps/<name>/manifest.toml` into a typed [`AppManifest`],
//!   2. generates a macOS `sandbox-exec` (seatbelt / SBPL) profile to
//!      `state/apps/<name>/<name>.sb` — DEFAULT-DENY, granting only what the
//!      manifest declares (see [`generate_sbpl`]),
//!   3. mints a per-launch HMAC-SHA256 capability token bound to the app's
//!      name + permission set + a session nonce ([`AppRegistry::mint_token`]),
//!   4. spawns `/usr/bin/sandbox-exec -f <profile> <interp> <entry...>` with
//!      the token + socket path handed to the app via the launch env, and
//!   5. accepts the app's connection on a per-app Unix socket
//!      (`state/ipc/apps/<name>.sock`, JSONL), VERIFIES the token on every
//!      inbound line, and relays accepted data onto the 7177 telemetry WS so
//!      the HUD panel renders without its own socket.
//!
//! sandbox-exec is DEPRECATED-BUT-FUNCTIONAL on macOS (the CLI prints a
//! deprecation notice yet the seatbelt kernel enforcement is fully live).
//! Phase-4 may move to a sandboxd profile or App Sandbox entitlements; the
//! manifest -> profile derivation here is the stable part.
//!
//! Reuses the actions.rs discipline: args-only `Command` (never a shell
//! string), `kill_on_drop(true)`, bounded waits. The session HMAC key lives in
//! a process-lifetime `OnceLock` and is NEVER logged, NEVER put on telemetry,
//! and NEVER handed to an app — only the derived per-app token reaches the
//! app's environment.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use hmac::{Hmac, KeyInit, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::telemetry;

type HmacSha256 = Hmac<Sha256>;

/// macOS seatbelt wrapper — deprecated CLI, live kernel enforcement.
pub(crate) const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
/// Apple's baseline BSD seatbelt profile. Imported by every generated profile
/// so the sandboxed process can actually boot (dyld, frameworks, the syscalls
/// every macOS binary needs) WITHOUT opening the filesystem, network, mic, or
/// GPU — those stay default-deny and are granted only per the manifest. This
/// is the same base Apple's own daemon profiles import.
pub(crate) const BSD_BASE_PROFILE: &str = "/System/Library/Sandbox/Profiles/bsd.sb";
/// The project venv interpreter a `runtime = "python"` app launches under.
/// Relative to the project root; resolved per-launch.
const PYTHON_INTERP_REL: &str = ".venv/bin/python3";

const MAX_APP_LINE_BYTES: usize = 1024 * 1024; // 1 MiB: app items/status/log relay lines; bounds a malicious/compromised app from OOMing the daemon (mirrors command.rs MAX_LINE_BYTES).

/// Restart governor: at most this many restarts within the window before the
/// host gives up on an app and emits app.crashed (see [`RestartGovernor`]).
const MAX_RESTARTS: u32 = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(5 * 60);

// ===========================================================================
// Manifest
// ===========================================================================

/// A parsed `apps/<name>/manifest.toml` (docs/SANDBOX.md schema). Unknown keys
/// are rejected (`deny_unknown_fields`) so a typo'd permission can never
/// silently widen or narrow the sandbox.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppManifest {
    pub app: AppSection,
    #[serde(default)]
    pub permissions: PermissionsSection,
    #[serde(default)]
    pub ui: UiSection,
    /// #36 PLUGIN SDK — the OPTIONAL capability-module contract block: the
    /// intents this plugin answers and the tools it exposes. `#[serde(default)]`
    /// (=> empty) so EVERY existing manifest (global-scan, vision, …) that omits
    /// it still parses unchanged. The block is VALIDATED by
    /// `crate::plugin_sdk::validate_manifest` (required fields, well-formed
    /// intent/tool names, requested capability scopes within the allowed set);
    /// the daemon's launcher continues to derive the SBPL profile + token from
    /// `[permissions]` exactly as before — declaring an intent grants nothing.
    #[serde(default)]
    pub intents: IntentsSection,
    #[serde(default)]
    pub tools: ToolsSection,
}

/// #36 — the `[intents]` block: the intent names this plugin claims to answer.
/// EMPTY by default (a plugin need not declare any). Validated by the plugin SDK.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IntentsSection {
    /// The intent names the plugin answers (e.g. "fab.status"). Each must be a
    /// well-formed dotted lowercase identifier (validated in plugin_sdk.rs).
    pub provides: Vec<String>,
}

/// #36 — the `[tools]` block: the tools this plugin exposes, each with the
/// capability scopes it requests. EMPTY by default. Validated by the plugin SDK:
/// a requested scope outside the allowed set, or a scope the sandbox forbids,
/// is rejected; an exposed tool the SDK marks consequential still rides the gate.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolsSection {
    /// The tools the plugin exposes (array-of-tables: `[[tools.exposes]]`).
    pub exposes: Vec<ToolDecl>,
}

/// #36 — one exposed tool's declaration. `deny_unknown_fields` so a typo'd tool
/// key is a parse error, never a silently-dropped scope.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolDecl {
    /// Tool name (e.g. "fab.read_status"). Well-formed dotted lowercase id.
    pub name: String,
    /// The capability scopes this tool requests (e.g. "net", "fs_read"). Each
    /// must be within the allowed scope set AND consistent with what the
    /// `[permissions]` block / sandbox actually grants (validated in plugin_sdk).
    pub scopes: Vec<String>,
    /// Whether the tool is side-effecting. A consequential tool still PARKS
    /// behind the cross-turn confirmation gate when invoked — declaring it here
    /// only makes the contract auditable, it never bypasses the gate.
    pub consequential: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSection {
    pub name: String,
    pub version: String,
    pub description: String,
    /// Command the app runs. For python/node this is the entry script
    /// (relative to the project root); for a binary it is the executable.
    pub entry: String,
    pub runtime: Runtime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    Python,
    Binary,
    Node,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PermissionsSection {
    pub audio: bool,
    pub net_hosts: Vec<String>,
    pub fs_read: Vec<String>,
    pub fs_write: Vec<String>,
    pub gpu: bool,
    /// AVFoundation capture from the user's OWN camera (Vision micro-app).
    ///
    /// IMPORTANT — TCC IS THE REAL GATE: this key only *declares* that the app
    /// needs camera access so the daemon can surface it in the launch UI /
    /// status and so the manifest's intent is auditable. It grants NOTHING by
    /// itself. macOS TCC (the Camera privacy permission) requires runtime USER
    /// CONSENT and is NOT grantable by an SBPL/seatbelt profile — consent
    /// happens on-device at first capture. `#[serde(default)]` (=> false) so
    /// EVERY existing manifest (global-scan, silicon-canvas, …) that omits the
    /// key still parses unchanged and stays camera-denied.
    pub camera: bool,
    /// ScreenCaptureKit capture of the user's OWN screen (Vision micro-app).
    /// Same TCC caveat as `camera` (the Screen Recording privacy permission):
    /// a DECLARATION only, never a grant; TCC consent is the on-device gate and
    /// is not SBPL-grantable. `#[serde(default)]` (=> false) keeps all existing
    /// manifests parsing and screen-denied.
    pub screen: bool,
    /// Dynamic code generation (JIT / writable-then-executable memory).
    ///
    /// DEFENSE-IN-DEPTH + AUDITABLE INTENT — NOT the primary gate. On Apple
    /// Silicon a DARWIN micro-app already cannot obtain RWX / `MAP_JIT` memory:
    /// the profile is `(deny default)` and the app runs under an unsigned/ad-hoc
    /// interpreter (python3/node) with NO `com.apple.security.cs.allow-jit`
    /// code-signing entitlement, and arm64e hardware W^X never maps a page
    /// writable-and-executable at once (`pthread_jit_write_protect_np` toggles a
    /// MAP_JIT region between `rw-` and `r-x`, never both). So `jit` here does
    /// three things the platform deny does not: it makes the intent DECLARED and
    /// auditable, it lets `generate_sbpl` emit an EXPLICIT `dynamic-code-generation`
    /// deny/allow (reorder-safe, like `gpu`), and it BINDS the bit into the
    /// per-launch HMAC token (see `canonical_permissions`) so a manifest that
    /// flips `jit` after a token was minted fails verification. `#[serde(default)]`
    /// (=> false) so every existing manifest parses unchanged and stays JIT-denied.
    ///
    /// HONESTY: `jit = true` is NECESSARY-BUT-NOT-SUFFICIENT — the seatbelt
    /// `(allow dynamic-code-generation)` does not grant the hardened-runtime
    /// entitlement, so under the current unsigned-interpreter launch it still does
    /// not enable RWX. Treating `jit = true` as a CONSEQUENTIAL capability
    /// declaration (an authored manifest edit, never a runtime auto-grant) is the
    /// project rule; auto-promotion must ride confirm + voice-id + policy + lockdown.
    pub jit: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UiSection {
    pub surface: String,
    pub telemetry_topics: Vec<String>,
}

impl Default for UiSection {
    fn default() -> Self {
        Self {
            surface: "panel".to_string(),
            telemetry_topics: Vec::new(),
        }
    }
}

impl AppManifest {
    /// Parse a manifest from its TOML text and validate the invariants the
    /// launcher relies on (non-empty name/version/entry, name = directory).
    /// `dir_name` is the on-disk app directory the manifest was read from;
    /// SANDBOX.md requires `[app].name` to match it (it keys the socket and
    /// the token, so a mismatch would mint a token for the wrong identity).
    pub fn parse(raw: &str, dir_name: &str) -> Result<Self> {
        let manifest: AppManifest =
            toml::from_str(raw).context("manifest is not valid TOML for the SANDBOX.md schema")?;
        manifest.validate(dir_name)?;
        Ok(manifest)
    }

    /// Read and parse `<app_dir>/manifest.toml`.
    pub fn load(app_dir: &Path) -> Result<Self> {
        let dir_name = app_dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("app dir has no readable name: {}", app_dir.display()))?;
        let path = app_dir.join("manifest.toml");
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&raw, dir_name)
    }

    fn validate(&self, dir_name: &str) -> Result<()> {
        if self.app.name.trim().is_empty() {
            bail!("manifest [app].name is empty");
        }
        if self.app.name != dir_name {
            bail!(
                "manifest [app].name ({:?}) must match its directory name ({:?})",
                self.app.name,
                dir_name
            );
        }
        if self.app.version.trim().is_empty() {
            bail!("manifest [app].version is empty");
        }
        if self.app.entry.trim().is_empty() {
            bail!("manifest [app].entry is empty");
        }
        self.validate_capability_ceiling()?;
        Ok(())
    }

    /// CAPABILITY CEILING (Wave A): bound the STRUCTURAL shape of `[permissions]`
    /// at discover time, so an over-broad or malformed manifest is rejected
    /// (fail-closed, surfaced as an install error) BEFORE the app is ever
    /// registered or launched — the runtime discover/launch path previously had
    /// no permission bound at all.
    ///
    /// Deliberately NOT the forge author-time ban on audio/gpu/camera/screen:
    /// those are legitimate for first-party apps (vision needs camera/screen,
    /// nexus needs audio). This bounds the invariants EVERY app must honor:
    ///   - fs_write / fs_read are CONFINED in-project relative paths (no
    ///     absolute path, no `..`/root escape) — a manifest can never declare
    ///     write access to `/` or read access to `../../etc`;
    ///   - net_hosts are BARE hostnames (never a URL / path / port / space /
    ///     `..`) and bounded in count.
    ///
    /// Every shipped manifest already satisfies this; the ceiling exists to stop
    /// a NEW/edited manifest from widening the sandbox beyond these invariants.
    fn validate_capability_ceiling(&self) -> Result<()> {
        const MAX_APP_NET_HOSTS: usize = 16;
        let p = &self.permissions;
        for w in &p.fs_write {
            if !crate::forge::is_confined_relpath(w) {
                bail!("over-broad permission: fs_write {w:?} is not a confined in-project relative path");
            }
        }
        for r in &p.fs_read {
            if !crate::forge::is_confined_relpath(r) {
                bail!("over-broad permission: fs_read {r:?} is not a confined in-project relative path");
            }
        }
        if p.net_hosts.len() > MAX_APP_NET_HOSTS {
            bail!(
                "over-broad permission: net_hosts declares {} hosts (max {MAX_APP_NET_HOSTS})",
                p.net_hosts.len()
            );
        }
        for h in &p.net_hosts {
            let h = h.trim();
            if h.is_empty() || h.contains('/') || h.contains(':') || h.contains(' ') || h.contains("..") {
                bail!("over-broad permission: net_hosts entry {h:?} is not a bare hostname");
            }
        }
        Ok(())
    }

    pub fn name(&self) -> &str {
        &self.app.name
    }
}

// ===========================================================================
// Capability token (HMAC-SHA256 over name || perms || nonce)
// ===========================================================================

/// Canonical, stable string of the manifest's permission set. The token binds
/// to THIS exact set, so a manifest that widens its permissions after a token
/// was minted (or a token lifted from another app with a different set) fails
/// verification. Sorting every list makes the canonical form independent of
/// declaration order — two manifests that grant the same thing in a different
/// order produce the same token, a reordered-but-identical manifest is not a
/// new identity.
pub fn canonical_permissions(p: &PermissionsSection) -> String {
    fn joined(label: &str, items: &[String]) -> String {
        let mut v: Vec<&str> = items.iter().map(String::as_str).collect();
        v.sort_unstable();
        format!("{label}=[{}]", v.join(","))
    }
    // camera/screen/jit are part of the bound permission set: a manifest that
    // flips any of them after a token was minted must fail verification (same
    // discipline as audio/gpu — see the token_is_bound_to_* tests). Appended
    // AFTER the original fields so the canonical form stays a stable, readable
    // suffix. The session HMAC key is regenerated every daemon boot and tokens
    // are minted per launch from THIS function, so widening the canonical string
    // does not strand any persisted token — there are none across a restart.
    format!(
        "audio={};gpu={};{};{};{};camera={};screen={};jit={}",
        p.audio,
        p.gpu,
        joined("net_hosts", &p.net_hosts),
        joined("fs_read", &p.fs_read),
        joined("fs_write", &p.fs_write),
        p.camera,
        p.screen,
        p.jit,
    )
}

/// A compact, SECRET-FREE, human-readable summary of what a micro-app is DECLARED
/// to be able to do (its granted capabilities from `[permissions]`) — the static
/// "what can this app do" audit that complements the runtime introspection's "what
/// is it doing". Lists ONLY granted capabilities (counts for the list-valued ones,
/// never the paths/hosts themselves), so a locked-down app reads short. Pure.
pub fn capability_summary(p: &PermissionsSection) -> String {
    let mut parts: Vec<String> = Vec::new();
    if p.audio {
        parts.push("audio".to_string());
    }
    if p.gpu {
        parts.push("gpu".to_string());
    }
    if p.camera {
        parts.push("camera".to_string());
    }
    if p.screen {
        parts.push("screen".to_string());
    }
    if p.jit {
        parts.push("jit".to_string());
    }
    if !p.net_hosts.is_empty() {
        parts.push(format!("net({})", p.net_hosts.len()));
    }
    if !p.fs_read.is_empty() {
        parts.push(format!("fs_read({})", p.fs_read.len()));
    }
    if !p.fs_write.is_empty() {
        parts.push(format!("fs_write({})", p.fs_write.len()));
    }
    if parts.is_empty() {
        "sandboxed (no extra capabilities)".to_string()
    } else {
        parts.join(", ")
    }
}

/// The message the HMAC is computed over: `name || canonical(perms) || nonce`,
/// joined with NUL so no field can bleed into the next (a name ending in the
/// next field's prefix can never collide).
fn token_message(name: &str, perms: &PermissionsSection, nonce: &str) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.extend_from_slice(name.as_bytes());
    msg.push(0);
    msg.extend_from_slice(canonical_permissions(perms).as_bytes());
    msg.push(0);
    msg.extend_from_slice(nonce.as_bytes());
    msg
}

/// Compute the hex-encoded HMAC-SHA256 token. Pure given the key — the unit
/// tests drive it directly with a fixed key to prove forgery/tamper/cross-app
/// rejection without a live daemon.
pub fn compute_token(key: &[u8], name: &str, perms: &PermissionsSection, nonce: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&token_message(name, perms, nonce));
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verification: recompute and compare with the MAC's own
/// constant-time `verify_slice` (never a `==` on the hex string).
pub fn verify_token_with_key(
    key: &[u8],
    name: &str,
    perms: &PermissionsSection,
    nonce: &str,
    presented: &str,
) -> bool {
    let Ok(presented_bytes) = hex::decode(presented) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&token_message(name, perms, nonce));
    mac.verify_slice(&presented_bytes).is_ok()
}

/// The daemon-local session HMAC key, generated once at startup and never
/// after. NEVER logged, NEVER on telemetry, NEVER in an app's env — only the
/// derived per-app token leaves this module. A fresh key every boot means a
/// token leaked from a previous run is dead after a restart.
static SESSION_KEY: OnceLock<[u8; 32]> = OnceLock::new();

fn session_key() -> &'static [u8; 32] {
    SESSION_KEY.get_or_init(|| {
        // 32 bytes of OS entropy. getrandom via a fresh, unseeded source: we
        // pull from /dev/urandom directly to avoid adding an RNG dependency
        // and to keep the key off any logged code path.
        let mut key = [0u8; 32];
        match std::fs::File::open("/dev/urandom")
            .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut key))
        {
            Ok(()) => key,
            Err(e) => {
                // /dev/urandom is effectively always present on macOS; if it
                // is not, fail loud rather than minting predictable tokens.
                panic!("cannot read /dev/urandom to seed the app session key: {e}");
            }
        }
    })
}

// ===========================================================================
// Command-channel capability token (HUD -> daemon command socket)
// ===========================================================================
//
// The HUD command channel (command.rs) is JUST ANOTHER authenticated local
// caller, so it reuses the SAME HMAC-SHA256 machinery as the per-app relay and
// the generate proxy — no parallel token scheme. The principal is a RESERVED
// pseudo-app name (never a real micro-app, so it can never collide with a
// manifest) with an EMPTY permission set, bound to a per-BOOT nonce minted
// once at startup. A fresh session key + fresh nonce every boot means a token
// captured from a previous run is dead after a restart, exactly like an app
// token. The token is the daemon's authority to ACCEPT commands; it is handed
// to the Tauri backend out-of-band (the same keychain/handshake path the HUD
// already uses for verify_dispatch) and NEVER logged or put on telemetry.

/// The reserved capability principal for the HUD command channel. Prefixed with
/// a character no manifest name can use (manifest names are the on-disk app
/// directory name) so it can never collide with a real micro-app identity.
pub const COMMAND_PRINCIPAL: &str = "@hud-command";

/// The command principal's bound permission set: EMPTY. The command token grants
/// no filesystem/network/device capability of its own — it only authenticates
/// the caller to the command socket, whose allowed actions are a fixed structural
/// allowlist (command.rs), each routing into the EXISTING gated pipeline. Binding
/// to a constant empty set means the token is over exactly `name || "" || nonce`,
/// matching the per-app token shape without granting any app permission.
fn command_perms() -> PermissionsSection {
    PermissionsSection::default()
}

/// The per-boot nonce for the command principal, minted once at startup from OS
/// entropy. A leaked command token dies when the daemon restarts (new nonce).
static COMMAND_NONCE: OnceLock<String> = OnceLock::new();

fn command_nonce() -> &'static str {
    COMMAND_NONCE.get_or_init(fresh_nonce)
}

/// Mint the HUD command-channel capability token from the CURRENT session key,
/// the reserved principal, its empty permission set, and the per-boot nonce.
/// Called ONCE at daemon startup; the value is handed to the Tauri backend
/// out-of-band and presented on every command line. Reuses [`compute_token`] —
/// the SAME HMAC machinery as the per-app/genproxy tokens.
pub fn mint_command_token() -> String {
    compute_token(
        session_key(),
        COMMAND_PRINCIPAL,
        &command_perms(),
        command_nonce(),
    )
}

/// Constant-time verify of a token presented on the command socket against the
/// CURRENT session key + per-boot nonce. A forged/tampered/stale (pre-restart)
/// token fails closed. Reuses [`verify_token_with_key`] — no new crypto.
pub fn verify_command_token(presented: &str) -> bool {
    if presented.is_empty() {
        return false;
    }
    verify_token_with_key(
        session_key(),
        COMMAND_PRINCIPAL,
        &command_perms(),
        command_nonce(),
        presented,
    )
}

// ===========================================================================
// Process-global registry handle
// ===========================================================================
//
// The router threads the `Arc<AppRegistry>` explicitly into every app handler
// (handle_silicon_canvas/…), which is the primary path. But a MODEL-callable tool
// (`share_guard_scrub`) runs under `anthropic::execute_tool`, which is NOT given
// the registry, so it reaches the app runtime through this process-global handle
// — the SAME pattern `mcp::global()` uses for its manager. Set ONCE at startup
// (next to `AppRegistry::discover`); `None` until then (a pre-startup / unit-test
// caller gets an honest "runtime not up" rather than a panic).

/// The one live app registry, set once at daemon startup. Read via
/// [`global_registry`]; the router's explicit `Arc<AppRegistry>` threading is
/// unchanged and remains the primary path.
static GLOBAL_REGISTRY: OnceLock<Arc<AppRegistry>> = OnceLock::new();

/// Publish the process-global app registry (called ONCE at startup, right after
/// `AppRegistry::discover`). Idempotent: a second call is ignored so a stray
/// re-init can never swap the live registry out from under running apps.
pub fn set_global_registry(registry: Arc<AppRegistry>) {
    let _ = GLOBAL_REGISTRY.set(registry);
}

/// The process-global app registry, or `None` before startup published it (a
/// pre-startup or unit-test caller). Callers answer honestly on `None` — the app
/// runtime simply isn't up yet.
pub fn global_registry() -> Option<Arc<AppRegistry>> {
    GLOBAL_REGISTRY.get().cloned()
}

// ===========================================================================
// SBPL (seatbelt) profile generation
// ===========================================================================

/// Generate the macOS `sandbox-exec` (seatbelt / SBPL) profile text for an
/// app. DEFAULT-DENY: the profile opens with `(deny default)` and then grants
/// ONLY what the manifest declares.
///
/// `project_root` is the absolute project root; `interp` is the absolute
/// interpreter/runtime path the app launches under (the venv python for a
/// python app, the binary itself for a binary app); `app_dir` is the app's own
/// absolute directory. All allow-paths are emitted absolute — SBPL path
/// filters are not relative to a cwd.
///
/// Grants:
///   - process-exec* of the interpreter + the entry/app dir (start the child),
///   - file-read* of: the app's own dir, the interpreter & its runtime libs
///     (for python: the project .venv tree + the system framework prefixes the
///     stdlib loads from), each manifest `fs_read` path, plus dyld/dylib search
///     roots so the runtime can actually start,
///   - file-write* of each manifest `fs_write` path + the app's own per-app
///     socket dir (state/ipc/apps) so it can connect,
///   - network: when `net_hosts` is non-empty, `(system-network)` + outbound
///     with `remote tcp` host-name filters for the listed hosts (plus DNS);
///     empty list => no network at all,
///   - mach lookups the loader needs (dyld, the system framework registry).
///     Everything else — other filesystem, other network, the microphone, GPU,
///     the window server, the memory DB, secrets — stays denied by the opener.
pub fn generate_sbpl(
    manifest: &AppManifest,
    project_root: &Path,
    interp: &Path,
    app_dir: &Path,
    socket_path: &Path,
) -> String {
    let p = &manifest.permissions;
    let root = project_root;
    let mut s = String::new();

    // --- header --------------------------------------------------------
    s.push_str("(version 1)\n");
    s.push_str(&format!(
        ";; Generated by darwind for micro-app {:?} — docs/SANDBOX.md.\n",
        manifest.name()
    ));
    s.push_str(";; sandbox-exec is deprecated-but-functional on macOS; the\n");
    s.push_str(";; kernel seatbelt enforcement is live. Phase-4 may migrate to\n");
    s.push_str(";; a sandboxd profile or App Sandbox entitlements.\n");
    s.push_str(";; DEFAULT-DENY: everything below is the complete grant set.\n");
    s.push_str("(deny default)\n");
    // Import Apple's baseline BSD profile: it grants ONLY the syscalls, dyld /
    // framework boot reads, and timezone/encoding files that EVERY macOS
    // process needs to start — it does NOT open the filesystem, the network,
    // the mic, or the GPU (reading ~/.ssh or the memory DB is still denied).
    // Without this base, even /bin/sleep aborts on launch under (deny default);
    // with it, file/network/device access remains exactly the manifest grants
    // added below. system.sb is pulled in transitively by bsd.sb.
    if Path::new(BSD_BASE_PROFILE).exists() {
        s.push_str(&format!("(import {})\n", sbpl_str(Path::new(BSD_BASE_PROFILE))));
    }

    // --- explicit denies the manifest's booleans map to -----------------
    // These are already covered by (deny default); stated explicitly so the
    // profile reads as the SANDBOX.md derivation table and so a future
    // allow-rule reordering can't accidentally open them.
    if !p.audio {
        s.push_str("\n;; audio = false -> no microphone / audio device access.\n");
        s.push_str("(deny device-microphone)\n");
    }
    if !p.gpu {
        s.push_str("\n;; gpu = false -> no Metal / IOKit GPU client.\n");
        s.push_str("(deny iokit-open (iokit-user-client-class \"IOAccelerator\"))\n");
        s.push_str("(deny iokit-open (iokit-user-client-class \"AGXDeviceUserClient\"))\n");
    }

    // --- camera / screen (TCC-gated; SBPL is best-effort only) -------------
    // CRITICAL HONESTY: on macOS, CAMERA (AVFoundation) and SCREEN RECORDING
    // (ScreenCaptureKit) are gated by TCC — the privacy-consent subsystem.
    // TCC requires a RUNTIME USER-CONSENT prompt and is NOT grantable by an
    // SBPL/seatbelt profile: there is no `(allow camera)` / `(allow screen)`
    // operation, and even with everything below allowed the kernel+TCC still
    // block capture until the user consents on-device at first use. So the
    // manifest's `camera`/`screen = true` only DECLARES the need (surfaced in
    // the launch UI / status); the profile cannot and does not pretend to
    // enable capture. We keep DEFAULT-DENY and, at most, grant the mach-lookup
    // /device plumbing the capture frameworks need to even REACH the consent
    // prompt (best effort) — never the capture grant itself.
    if p.camera {
        s.push_str("\n;; camera = true -> DECLARED need for AVFoundation capture of\n");
        s.push_str(";; the user's OWN camera. macOS TCC (Camera) is the REAL gate:\n");
        s.push_str(";; it needs runtime user consent and is NOT SBPL-grantable, so\n");
        s.push_str(";; the lines below are BEST EFFORT plumbing only (reach the\n");
        s.push_str(";; capture stack + its consent prompt) — they do NOT enable\n");
        s.push_str(";; capture. No consent -> no frames, profile notwithstanding.\n");
        s.push_str("(allow iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.cmio.AppleCameraAssistant\"))\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.tccd\"))\n");
    } else {
        s.push_str("\n;; camera = false -> no camera. (deny default) already blocks\n");
        s.push_str(";; it; stated explicitly so a future allow-reorder can't open it.\n");
        s.push_str("(deny iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))\n");
    }
    if p.screen {
        s.push_str("\n;; screen = true -> DECLARED need for ScreenCaptureKit capture\n");
        s.push_str(";; of the user's OWN screen. macOS TCC (Screen Recording) is the\n");
        s.push_str(";; REAL gate: runtime user consent, NOT SBPL-grantable. The lines\n");
        s.push_str(";; below are BEST EFFORT plumbing (reach the window/capture\n");
        s.push_str(";; server + its consent prompt) — they do NOT enable capture.\n");
        s.push_str(";; No consent -> no frames, profile notwithstanding.\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.windowserver.active\"))\n");
        s.push_str("(allow mach-lookup (global-name \"com.apple.tccd\"))\n");
    } else {
        s.push_str("\n;; screen = false -> no screen capture. (deny default) already\n");
        s.push_str(";; blocks the window server; stated explicitly for clarity.\n");
        s.push_str("(deny mach-lookup (global-name \"com.apple.windowserver.active\"))\n");
    }

    // --- jit / dynamic code generation (defense-in-depth; NOT the sole gate) ---
    // Only `dynamic-code-generation` is a current seatbelt operation — the
    // legacy `dynamic-signature` op is NOT emitted (it is not a live operation on
    // current macOS and would risk a profile-compile error, the class of failure
    // deny_unknown_fields guards elsewhere). On Apple Silicon the RWX/MAP_JIT deny
    // is PRIMARILY enforced by the platform (no com.apple.security.cs.allow-jit
    // entitlement on the unsigned/ad-hoc interpreter + arm64e hardware W^X), so
    // this line is defense-in-depth and auditable intent, not the primary barrier.
    if !p.jit {
        s.push_str("\n;; jit = false -> no dynamic code generation (JIT / RWX).\n");
        s.push_str(";; Already denied by (deny default) AND, on Apple Silicon, by the\n");
        s.push_str(";; platform (no allow-jit entitlement + arm64e W^X). Stated\n");
        s.push_str(";; explicitly so a future allow-reorder can't open it.\n");
        s.push_str("(deny dynamic-code-generation)\n");
    } else {
        s.push_str("\n;; jit = true -> DECLARED need for dynamic code generation (JIT).\n");
        s.push_str(";; HONESTY: NECESSARY-BUT-NOT-SUFFICIENT. On a hardened/notarized\n");
        s.push_str(";; build the PROCESS also needs the com.apple.security.cs.allow-jit\n");
        s.push_str(";; code-signing entitlement (SBPL cannot grant it) and must use\n");
        s.push_str(";; MAP_JIT + pthread_jit_write_protect_np to keep W^X. Under the\n");
        s.push_str(";; current unsigned-interpreter launch this grant alone does NOT\n");
        s.push_str(";; enable RWX — same best-effort caveat as camera/screen.\n");
        s.push_str("(allow dynamic-code-generation)\n");
    }

    // Resolve the interpreter's REAL path once. The venv python3 is a SYMLINK
    // (.venv/bin/python3 -> the Homebrew Cellar python) and seatbelt checks
    // exec against the RESOLVED target, so we must grant exec on the canonical
    // path too — but as a LITERAL on the resolved file, NOT a subpath over the
    // whole Homebrew/usr-local tree (a broad prefix would let the app exec any
    // bash/curl/git/compiler planted there, and those prefixes are user-
    // writable on Homebrew installs). canonicalize() is best-effort: if it
    // fails (path not yet materialized in a test root) we fall back to the
    // configured path, which the literal below already covers.
    let interp_abs = abs(root, interp);
    let interp_real = std::fs::canonicalize(&interp_abs).unwrap_or_else(|_| interp_abs.clone());

    // Read prefixes: the directory trees the interpreter + its standard
    // libraries live under. The app still needs to READ its stdlib/site-
    // packages to import anything — for a venv those live under .venv and under
    // the resolved interpreter's own INSTALL PREFIX (the Cellar version dir
    // that holds lib/pythonX.Y), which we derive tightly from the resolved
    // interpreter path rather than opening all of /opt/homebrew. Read is a far
    // weaker grant than exec, but we still scope it to just what boots.
    let runtime_read_prefixes: Vec<PathBuf> = match manifest.app.runtime {
        Runtime::Python => {
            let mut v = vec![
                // The interpreter + site-packages read root. SUBSTRATE LOCK
                // (envlock.rs) NARROWING SEAM: when the interpreter lives inside a
                // pinned content-addressed closure (state/envstore/<hash>/…) this is
                // that CLOSURE dir — app-specific, read-only, exactly the pinned
                // files — replacing the shared project .venv and closing the
                // shared-.venv reach + venv-drift caveats. For an UNPINNED app
                // (interpreter under .venv, the legacy path) it returns the project
                // .venv, byte-for-byte the prior behavior.
                crate::envlock::python_runtime_read_root(root, &interp_abs),
                // The system Python framework, when used directly.
                PathBuf::from("/Library/Frameworks/Python.framework"),
            ];
            // The resolved interpreter's install prefix: <prefix>/bin/python3
            // -> <prefix> holds lib/pythonX.Y (the stdlib). Grant read on that
            // prefix only, not the whole Cellar/Homebrew root.
            if let Some(prefix) = interpreter_install_prefix(&interp_real) {
                v.push(prefix);
            }
            v
        }
        Runtime::Node => {
            let mut v = Vec::new();
            if let Some(prefix) = interpreter_install_prefix(&interp_real) {
                v.push(prefix);
            }
            v
        }
        // A prebuilt binary IS its own interpreter; nothing extra to read.
        Runtime::Binary => Vec::new(),
    };

    // --- process exec ---------------------------------------------------
    s.push_str("\n;; Start the child: exec the runtime interpreter (or, for a\n");
    s.push_str(";; binary app, the entry itself) and the app dir's own scripts.\n");
    s.push_str(";; Exec is granted ONLY on the configured interpreter path and\n");
    s.push_str(";; its canonicalized target — never a broad Homebrew/usr-local\n");
    s.push_str(";; subpath — so the app cannot exec other binaries planted there.\n");
    s.push_str("(allow process-fork)\n");
    match manifest.app.runtime {
        Runtime::Python | Runtime::Node => {
            // The configured interpreter path (the venv symlink) AND its
            // canonicalized target (what seatbelt actually checks exec against).
            s.push_str(&format!(
                "(allow process-exec* (literal {}))\n",
                sbpl_str(&interp_abs)
            ));
            if interp_real != interp_abs {
                s.push_str(&format!(
                    "(allow process-exec* (literal {}))\n",
                    sbpl_str(&interp_real)
                ));
            }
        }
        Runtime::Binary => {
            // The entry binary itself (the interp == entry for a binary app).
            s.push_str(&format!(
                "(allow process-exec* (literal {}))\n",
                sbpl_str(&interp_abs)
            ));
        }
    }
    // Scripts/helpers inside the app's own dir.
    s.push_str(&format!(
        "(allow process-exec* (subpath {}))\n",
        sbpl_str(&abs(root, app_dir))
    ));

    // --- file reads -----------------------------------------------------
    s.push_str("\n;; Reads: the app's own dir, the runtime libs needed to start,\n");
    s.push_str(";; and each manifest fs_read path. Nothing else is readable.\n");
    let mut read_subpaths: Vec<PathBuf> = Vec::new();
    // The app's own directory is implicitly readable (SANDBOX.md).
    read_subpaths.push(abs(root, app_dir));
    // The runtime read prefixes (interpreter install prefix + venv + libs).
    read_subpaths.extend(runtime_read_prefixes.iter().cloned());
    // System dyld/dylib search roots every macOS process loads from.
    read_subpaths.push(PathBuf::from("/usr/lib"));
    read_subpaths.push(PathBuf::from("/System/Library"));
    read_subpaths.push(PathBuf::from("/Library/Apple"));
    // The configured interpreter path AND its canonical target.
    read_subpaths.push(interp_abs.clone());
    if interp_real != interp_abs {
        read_subpaths.push(interp_real.clone());
    }
    // Manifest fs_read grants (resolved relative to the project root).
    for r in &p.fs_read {
        read_subpaths.push(abs(root, Path::new(r)));
    }
    for path in &read_subpaths {
        s.push_str(&format!("(allow file-read* (subpath {}))\n", sbpl_str(path)));
    }
    // file-read-metadata is SCOPED to the same granted roots — never a blanket
    // grant. A bare `(allow file-read-metadata)` (no path filter) would let the
    // app stat/test-existence on the ENTIRE filesystem — probing whether
    // ~/.ssh/id_rsa or another app's state exists and its size/mtime — an
    // info-leak side channel even though the contents stay denied. file-read*
    // already implies metadata for these subpaths; emitting the scoped
    // metadata rule explicitly documents the boundary and survives a future
    // rule reorder. dyld's startup stats of "/" and the firmlink ancestors are
    // already covered by the bsd.sb/system.sb import, so no blanket grant is
    // needed to boot.
    for path in &read_subpaths {
        s.push_str(&format!(
            "(allow file-read-metadata (subpath {}))\n",
            sbpl_str(path)
        ));
    }

    // --- file writes ----------------------------------------------------
    s.push_str("\n;; Writes: each manifest fs_write path + the app's own socket.\n");
    for w in &p.fs_write {
        s.push_str(&format!(
            "(allow file-write* (subpath {}))\n",
            sbpl_str(&abs(root, Path::new(w)))
        ));
    }
    // The per-app socket the daemon owns: the app connects (read+write) to its
    // own socket path only. The socket dir is under state/ipc/apps.
    let sock_abs = abs(root, socket_path);
    s.push_str(&format!(
        "(allow file-read* file-write* (literal {}))\n",
        sbpl_str(&sock_abs)
    ));

    // --- network --------------------------------------------------------
    // SBPL is last-match-wins, so the IP-network deny/allow rules go FIRST and
    // the Unix-socket connect grant goes LAST — otherwise a (deny network*)
    // would clobber the socket grant and the app could never reach its host.
    if p.net_hosts.is_empty() {
        s.push_str("\n;; net_hosts = [] -> no outbound IP network at all.\n");
        s.push_str("(deny network*)\n");
    } else {
        s.push_str("\n;; net_hosts non-empty -> outbound TCP to the listed hosts\n");
        s.push_str(";; only, plus DNS. CAVEAT 1 (coarse host filtering): SBPL\n");
        s.push_str(";; `remote tcp host-name` matches the connect-time name but\n");
        s.push_str(";; cannot pin the resolved IP, and a host that resolves to a\n");
        s.push_str(";; shared CDN may share an allow with unrelated names on that\n");
        s.push_str(";; CDN. CAVEAT 2 (DNS exfil): allowing DNS at all opens a\n");
        s.push_str(";; side channel — a malicious app could encode data in query\n");
        s.push_str(";; labels to an attacker-controlled nameserver, bypassing the\n");
        s.push_str(";; net_hosts allow-list entirely. We restrict DNS to the\n");
        s.push_str(";; system resolver address (below) to RAISE the bar, but this\n");
        s.push_str(";; does NOT close the channel. Both caveats are the headline\n");
        s.push_str(";; justification for the Phase-4 daemon-mediated fetch proxy\n");
        s.push_str(";; (app declares URLs, daemon fetches; app gets NO direct\n");
        s.push_str(";; network or DNS at all). See docs/SANDBOX.md.\n");
        s.push_str("(system-network)\n");
        // Deny IP network first, then re-allow only DNS + the declared hosts,
        // so nothing outside the allow-list survives (last-match-wins).
        s.push_str("(deny network*)\n");
        // DNS resolution. Pin to the SYSTEM RESOLVER address(es) from
        // /etc/resolv.conf when we can read them, so the app cannot send DNS
        // queries directly to an attacker-controlled nameserver — this raises
        // the bar on the exfil channel (it does not close it; the resolver
        // still forwards). If no resolver is readable, fall back to *:53 so
        // the app can still boot (a host with no resolv.conf is unusual).
        let resolvers = system_resolvers();
        if resolvers.is_empty() {
            s.push_str(";; no /etc/resolv.conf nameserver found -> DNS to any *:53.\n");
            s.push_str("(allow network-outbound (remote udp \"*:53\"))\n");
            s.push_str("(allow network-outbound (remote tcp \"*:53\"))\n");
        } else {
            s.push_str(";; DNS pinned to the system resolver address(es).\n");
            for r in &resolvers {
                s.push_str(&format!(
                    "(allow network-outbound (remote udp \"{r}:53\"))\n"
                ));
                s.push_str(&format!(
                    "(allow network-outbound (remote tcp \"{r}:53\"))\n"
                ));
            }
        }
        // Each declared host (the feeds are all HTTPS).
        let mut hosts: Vec<&str> = p.net_hosts.iter().map(String::as_str).collect();
        hosts.sort_unstable();
        hosts.dedup();
        for host in hosts {
            s.push_str(&format!(
                "(allow network-outbound (remote tcp (host-name {})))\n",
                sbpl_str(Path::new(host))
            ));
        }
    }
    // The app's OWN Unix socket — granted LAST so neither network branch above
    // can clobber it. Connecting to a Unix-domain socket is network-outbound to
    // the socket path.
    s.push_str(";; The app's own per-app Unix socket (never clobbered above).\n");
    s.push_str(&format!(
        "(allow network-outbound (literal {}))\n",
        sbpl_str(&sock_abs)
    ));
    // A declared fs_read entry that IS a Unix socket (path ends in .sock) needs
    // an AF_UNIX `network-outbound` literal grant IN ADDITION to its file-read*
    // subpath above: on this macOS, file-read alone does NOT permit connect() to
    // a Unix-domain socket (connect is a network operation, not a file read).
    // Emitted here, AFTER the (deny network*) branch, so last-match-wins keeps
    // the connect grant alive. This is how a micro-app reaches the daemon-
    // mediated generate proxy at state/ipc/apps/generate.sock — and ONLY that
    // proxy, since the manifest no longer lists the raw inference.sock at all.
    for r in &p.fs_read {
        if Path::new(r).extension().and_then(|e| e.to_str()) == Some("sock") {
            let r_abs = abs(root, Path::new(r));
            // The app's own socket already has this grant; don't double-emit.
            if r_abs != sock_abs {
                s.push_str(";; fs_read Unix socket -> AF_UNIX connect() grant.\n");
                s.push_str(&format!(
                    "(allow network-outbound (literal {}))\n",
                    sbpl_str(&r_abs)
                ));
            }
        }
    }

    // --- mach / loader services the runtime needs -----------------------
    s.push_str("\n;; Mach lookups the dynamic loader and runtime require.\n");
    s.push_str("(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))\n");
    s.push_str("(allow mach-lookup (global-name \"com.apple.system.notification_center\"))\n");
    s.push_str("(allow mach-lookup (global-name \"com.apple.coreservices.launchservicesd\"))\n");
    s.push_str("(allow sysctl-read)\n");

    s
}

/// Quote a path/string as an SBPL string literal. SBPL strings are
/// double-quoted with backslash escaping; app paths never contain quotes in
/// practice, but escape defensively so a path with a quote or backslash can
/// never break out of the literal and widen the profile.
pub(crate) fn sbpl_str(p: &Path) -> String {
    let raw = p.to_string_lossy();
    let mut out = String::with_capacity(raw.len() + 2);
    out.push('"');
    for c in raw.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Resolve a possibly-relative manifest path against the project root; absolute
/// paths pass through unchanged.
fn abs(root: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    }
}

/// The install prefix an interpreter's standard library lives under, derived
/// tightly from the RESOLVED interpreter path so the read grant covers the
/// stdlib without opening the whole Homebrew/usr-local tree. A CPython install
/// is laid out as `<prefix>/bin/python3.11` with the stdlib under
/// `<prefix>/lib/pythonX.Y`, so the prefix is the interpreter's grandparent
/// (`bin/`'s parent). Returns None when the path has no such structure (e.g. a
/// bare `/usr/bin/python3`), in which case the per-file interpreter read grant
/// and the system dyld roots already cover the boot.
fn interpreter_install_prefix(interp_real: &Path) -> Option<PathBuf> {
    let bin_dir = interp_real.parent()?; // <prefix>/bin
    // Only treat it as an install prefix when the interpreter sits in a `bin`
    // directory — otherwise we would grant read on an arbitrary ancestor.
    if bin_dir.file_name().and_then(|n| n.to_str()) != Some("bin") {
        return None;
    }
    let prefix = bin_dir.parent()?; // <prefix>
    // Guard against pathological prefixes ("/", "/usr") that would re-open a
    // broad tree — require at least two path components beyond the root.
    if prefix.components().count() < 3 {
        return None;
    }
    Some(prefix.to_path_buf())
}

/// The system DNS resolver address(es) from `/etc/resolv.conf`, used to PIN the
/// app's DNS grant instead of opening `*:53` (raises the bar on the DNS-exfil
/// side channel). Each entry is validated as a literal IPv4/IPv6 address —
/// never echoed verbatim into the SBPL — so a tampered resolv.conf cannot
/// inject profile syntax. Returns an empty Vec when none can be read (the
/// caller then falls back to `*:53` so the app still boots).
fn system_resolvers() -> Vec<String> {
    let Ok(raw) = std::fs::read_to_string("/etc/resolv.conf") else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some(rest) = line.strip_prefix("nameserver") else {
            continue;
        };
        let addr = rest.trim();
        // Only accept a literal IP — reject anything that is not parseable as
        // one (defensive against a hostile/garbled resolv.conf).
        if addr.parse::<std::net::IpAddr>().is_ok() && !out.iter().any(|a| a == addr) {
            out.push(addr.to_string());
        }
    }
    out
}

// ===========================================================================
// Restart governor (pure rate math)
// ===========================================================================

/// Bounded-restart bookkeeping for one app: at most [`MAX_RESTARTS`] restarts
/// within [`RESTART_WINDOW`], after which the host gives up. Pure and tested:
/// the lifecycle loop only calls `should_restart` / `record_restart`.
#[derive(Debug)]
pub struct RestartGovernor {
    window: Duration,
    max: u32,
    /// Restart instants within the rolling window (oldest first).
    restarts: Vec<Instant>,
}

impl RestartGovernor {
    pub fn new() -> Self {
        Self {
            window: RESTART_WINDOW,
            max: MAX_RESTARTS,
            restarts: Vec::new(),
        }
    }

    #[cfg(test)]
    fn with_limits(window: Duration, max: u32) -> Self {
        Self {
            window,
            max,
            restarts: Vec::new(),
        }
    }

    /// Drop restart marks older than the window relative to `now`.
    fn evict(&mut self, now: Instant) {
        let window = self.window;
        self.restarts
            .retain(|t| now.duration_since(*t) <= window);
    }

    /// Would a restart right now stay within the budget? Counts the restarts
    /// still inside the window; true iff fewer than `max` remain.
    pub fn should_restart(&mut self, now: Instant) -> bool {
        self.evict(now);
        (self.restarts.len() as u32) < self.max
    }

    /// Record that a restart happened at `now` (call after `should_restart`).
    pub fn record_restart(&mut self, now: Instant) {
        self.evict(now);
        self.restarts.push(now);
    }

    /// Restarts counted within the window as of `now` — for telemetry.
    pub fn count(&mut self, now: Instant) -> u32 {
        self.evict(now);
        self.restarts.len() as u32
    }
}

impl Default for RestartGovernor {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// App registry + lifecycle
// ===========================================================================

/// A micro-app known to the host: its manifest, its session nonce (rotated per
/// launch), its minted token, and the paths the launcher needs.
struct AppEntry {
    manifest: AppManifest,
    app_dir: PathBuf,
    socket_path: PathBuf,
    profile_path: PathBuf,
    /// Rotated on every (re)launch — a leaked token dies when the nonce moves.
    nonce: String,
    token: String,
    /// Set while the app is supposed to be running; the lifecycle task owns it.
    running: bool,
    /// Fired by stop()/restart give-up to WAKE the lifecycle task out of its
    /// blocking select! on read_line/child.wait — otherwise a quiet, well-
    /// behaved app (one that sends a line then idles) would not be torn down
    /// until it happened to exit. Cloned into the lifecycle task at launch.
    stop_notify: Arc<tokio::sync::Notify>,
    /// HOST -> APP op queue. The router pushes a structured op line here via
    /// [`send_op`]; the live connection handler drains it and writes the line
    /// to the app's socket (alongside the start/refresh/stop control verbs).
    /// Unbounded because op lines are tiny and rare (one per spoken command)
    /// and the drain is always live while the app is connected. The receiver
    /// is `take()`n into the connection handler at accept; a line queued while
    /// the app is between connections is held until the next accept drains it.
    /// `Mutex<Option<...>>` so the lifecycle task can move the receiver out for
    /// the duration of a connection and return it on reconnect.
    op_tx: mpsc::UnboundedSender<String>,
    op_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,
}

/// The host's registry of installed micro-apps, keyed by name. One per daemon.
pub struct AppRegistry {
    project_root: PathBuf,
    /// name -> entry. Mutex (async) because the router and the lifecycle task
    /// both touch it; held only briefly.
    apps: Mutex<HashMap<String, AppEntry>>,
    /// Test-only: override the resolved interpreter for python/node apps so the
    /// hermetic integration test can point at a real interpreter without a
    /// project .venv in its tempdir. Never set in production.
    #[cfg(test)]
    interpreter_override: Option<PathBuf>,
}

/// Public, read-only view of a registered app for routing/intent matching.
#[derive(Debug, Clone)]
pub struct AppInfo {
    pub name: String,
    pub description: String,
    pub running: bool,
    /// Whether the manifest's entry file actually EXISTS right now. Spec-only
    /// apps (manifest + SPEC.md, no code yet) and unbuilt compiled apps
    /// register (deliberate: visible in the deck, build-state independent) but
    /// are honestly labeled not-runnable instead of failing at spawn time.
    pub entry_present: bool,
}

impl AppRegistry {
    /// Scan `apps/` under the project root, parse every `manifest.toml`, and
    /// build the registry. Apps with a malformed/mismatched manifest are
    /// skipped with a WARN (a bad manifest must not stop the daemon) and
    /// surfaced on telemetry so the HUD can show the install error.
    pub fn discover(project_root: &Path) -> Arc<Self> {
        let apps_dir = project_root.join("apps");
        let mut apps = HashMap::new();
        if let Ok(entries) = std::fs::read_dir(&apps_dir) {
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                if !dir.join("manifest.toml").exists() {
                    continue;
                }
                match AppManifest::load(&dir) {
                    Ok(manifest) => {
                        let name = manifest.name().to_string();
                        // Entry-resolution guard: child_argv resolves [app].entry as a
                        // SINGLE project-root-relative path (never a shell command), so an
                        // entry that resolves OUTSIDE the app's own directory — the legacy
                        // "python3 main.py" command form (-> <root>/python3 main.py) or a
                        // bare binary name like "vision" (-> <root>/vision) — would fail
                        // SILENTLY at spawn. Report it as an invalid manifest and skip
                        // registration instead. STRUCTURAL (build-state independent): a
                        // not-yet-built binary artifact still resolves inside its app dir,
                        // so it registers and launches once built.
                        let entry_abs = abs(project_root, Path::new(&manifest.app.entry));
                        if !entry_abs.starts_with(&dir) {
                            warn!(
                                dir = %dir.display(),
                                entry = %manifest.app.entry,
                                "skipping micro-app: [app].entry resolves outside the app directory"
                            );
                            telemetry::emit(
                                "system",
                                "app.manifest_invalid",
                                json!({
                                    "name": name,
                                    "error": format!(
                                        "[app].entry {:?} must be a project-root-relative path \
                                         inside the app directory (resolved to {})",
                                        manifest.app.entry,
                                        entry_abs.display()
                                    ),
                                }),
                            );
                            continue;
                        }
                        let socket_path = project_root
                            .join("state/ipc/apps")
                            .join(format!("{name}.sock"));
                        let profile_path = project_root
                            .join("state/apps")
                            .join(&name)
                            .join(format!("{name}.sb"));
                        let (op_tx, op_rx) = mpsc::unbounded_channel::<String>();
                        apps.insert(
                            name.clone(),
                            AppEntry {
                                manifest,
                                app_dir: dir,
                                socket_path,
                                profile_path,
                                nonce: String::new(),
                                token: String::new(),
                                running: false,
                                stop_notify: Arc::new(tokio::sync::Notify::new()),
                                op_tx,
                                op_rx: Arc::new(Mutex::new(Some(op_rx))),
                            },
                        );
                        info!(app = name, "micro-app manifest registered");
                    }
                    Err(e) => {
                        warn!(dir = %dir.display(), error = %e, "skipping invalid micro-app manifest");
                        if let Some(dn) = dir.file_name().and_then(|n| n.to_str()) {
                            telemetry::emit(
                                "system",
                                "app.manifest_invalid",
                                json!({"name": dn, "error": e.to_string()}),
                            );
                        }
                    }
                }
            }
        }
        Arc::new(Self {
            project_root: project_root.to_path_buf(),
            apps: Mutex::new(apps),
            #[cfg(test)]
            interpreter_override: None,
        })
    }

    /// The absolute project root this registry resolves app paths against. Used by
    /// callers that must resolve a manifest-relative sandbox dir (e.g. the Share
    /// Guard bridge staging an image under `state/tmp/share-guard/input`).
    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// Read-only listing for the router's intent matcher (sorted by name).
    pub async fn list(&self) -> Vec<AppInfo> {
        let apps = self.apps.lock().await;
        let mut out: Vec<AppInfo> = apps
            .values()
            .map(|e| AppInfo {
                name: e.manifest.name().to_string(),
                description: e.manifest.app.description.clone(),
                running: e.running,
                // Live probe (one stat per app per list): a spec-only or
                // unbuilt entry reads not-runnable; building it flips this
                // honestly without a restart.
                entry_present: abs(&self.project_root, Path::new(&e.manifest.app.entry))
                    .is_file(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Resolve a spoken app reference (e.g. "global scan", "globalscan",
    /// "global-scan") to a registered app name. Compares against each app
    /// name with hyphens/whitespace normalized away, so the classifier's
    /// loosely-spaced transcription still matches the manifest name.
    pub async fn resolve_name(&self, spoken: &str) -> Option<String> {
        let want = normalize_app_ref(spoken);
        if want.is_empty() {
            return None;
        }
        let apps = self.apps.lock().await;
        apps.keys()
            .find(|name| normalize_app_ref(name) == want)
            .cloned()
    }

    /// #36 PLUGIN SDK — the register-on-launch HANDSHAKE for a started plugin.
    /// Re-reads the app's on-disk `manifest.toml`, then drives
    /// [`crate::plugin_sdk::register_plugin`] with the app's CURRENT launch token
    /// + nonce against the live session key — proving the manifest's contract
    ///   block ([intents]/[tools]) validates AND the presented token verifies under
    ///   the SAME HMAC machinery the per-app relay uses. Returns the handshake
    ///   outcome; the caller (main.rs autostart, gated by `[plugin_sdk].enabled`)
    ///   emits secret-free telemetry from it. A not-running / unknown app, or a
    ///   manifest that no longer reads, yields `Unauthorized`/`InvalidManifest` —
    ///   fail-closed. This is the LIVE wiring of the #36 handshake; the pure
    ///   `register_plugin` is what the hermetic tests prove.
    pub async fn register_on_launch(&self, name: &str) -> crate::plugin_sdk::HandshakeOutcome {
        let (manifest_path, token, nonce) = {
            let apps = self.apps.lock().await;
            let Some(entry) = apps.get(name) else {
                return crate::plugin_sdk::HandshakeOutcome::Unauthorized;
            };
            (
                entry.app_dir.join("manifest.toml"),
                entry.token.clone(),
                entry.nonce.clone(),
            )
        };
        let Ok(raw) = std::fs::read_to_string(&manifest_path) else {
            return crate::plugin_sdk::HandshakeOutcome::InvalidManifest(format!(
                "could not read {}",
                manifest_path.display()
            ));
        };
        // The plugin presents its manifest + the launch token; the daemon
        // re-validates + verifies against the live session key + this launch nonce.
        crate::plugin_sdk::register_plugin(&raw, name, &token, session_key(), &nonce)
    }

    /// Mint the capability token for an app from the CURRENT session key, the
    /// app's name + permission set, and its current launch nonce. Pure over
    /// the static session key; the unit tests cover the math via
    /// [`compute_token`] directly.
    fn mint_token(&self, entry: &AppEntry) -> String {
        compute_token(
            session_key(),
            entry.manifest.name(),
            &entry.manifest.permissions,
            &entry.nonce,
        )
    }

    /// Verify a token an app presented on its socket, against that app's
    /// CURRENT nonce + permission set. A bad/forged/stale/cross-app token is
    /// rejected. `name` is the app the connection was accepted for.
    ///
    /// `pub(crate)` so the daemon-mediated generate proxy (genproxy.rs) can
    /// reuse the SAME token machinery as the per-app relay — no duplicate
    /// HMAC/nonce logic lives in the proxy.
    pub(crate) async fn verify_token(&self, name: &str, presented: &str) -> bool {
        let apps = self.apps.lock().await;
        let Some(entry) = apps.get(name) else {
            return false;
        };
        // A token presented before launch (empty nonce) is never valid.
        if entry.nonce.is_empty() || entry.token.is_empty() {
            return false;
        }
        verify_token_with_key(
            session_key(),
            entry.manifest.name(),
            &entry.manifest.permissions,
            &entry.nonce,
            presented,
        )
    }

    /// Test-only: rotate a registered app's nonce and mint+store a VALID token
    /// for it WITHOUT spawning a sandboxed child. Lets the genproxy unit tests
    /// drive the real `verify_token` path (same HMAC/nonce machinery as a live
    /// launch) without `sandbox-exec`. Returns the minted token, or None if the
    /// app is not registered.
    #[cfg(test)]
    pub(crate) async fn mint_for_test(&self, name: &str) -> Option<String> {
        let mut apps = self.apps.lock().await;
        let entry = apps.get_mut(name)?;
        entry.nonce = fresh_nonce();
        let token = compute_token(
            session_key(),
            entry.manifest.name(),
            &entry.manifest.permissions,
            &entry.nonce,
        );
        entry.token = token.clone();
        Some(token)
    }

    /// Resolve the runtime interpreter path for an app's runtime.
    fn interpreter(&self, manifest: &AppManifest) -> PathBuf {
        #[cfg(test)]
        if let Some(over) = &self.interpreter_override {
            if matches!(manifest.app.runtime, Runtime::Python | Runtime::Node) {
                return over.clone();
            }
        }
        match manifest.app.runtime {
            Runtime::Python => self.project_root.join(PYTHON_INTERP_REL),
            Runtime::Node => PathBuf::from("/usr/local/bin/node"),
            // A binary IS its own interpreter — exec the entry directly.
            Runtime::Binary => abs(&self.project_root, Path::new(&manifest.app.entry)),
        }
    }

    /// The argv the sandboxed child runs (after `sandbox-exec -f <profile>`).
    /// For python/node it is `<interp> <entry>`; for a binary it is the binary
    /// alone (the entry IS the interpreter).
    fn child_argv(&self, manifest: &AppManifest, interp: &Path) -> Vec<String> {
        // Test seam: with an interpreter override the entry is irrelevant (the
        // overridden interpreter is a stand-in idle process — /bin/sleep — not
        // a real app); give it a long sleep so the child stays alive while the
        // in-process test plays the app role over the socket, then is reaped by
        // kill_on_drop at stop().
        #[cfg(test)]
        if self.interpreter_override.is_some() {
            return vec![interp.to_string_lossy().into_owned(), "120".to_string()];
        }
        match manifest.app.runtime {
            Runtime::Python | Runtime::Node => vec![
                interp.to_string_lossy().into_owned(),
                abs(&self.project_root, Path::new(&manifest.app.entry))
                    .to_string_lossy()
                    .into_owned(),
            ],
            Runtime::Binary => vec![interp.to_string_lossy().into_owned()],
        }
    }

    /// Read-only snapshot for the introspect sentinel (introspect.rs): one
    /// `(name, profile_path, running)` per registered app. Holds the apps lock
    /// only long enough to clone the tuples — it reads, it changes nothing, and
    /// it exposes no new authority (the paths are already derived at discover).
    pub async fn observed_apps(&self) -> Vec<(String, PathBuf, bool)> {
        let apps = self.apps.lock().await;
        apps.iter()
            .map(|(name, e)| (name.clone(), e.profile_path.clone(), e.running))
            .collect()
    }

    /// Read-only DECLARED-capability inventory: one `(name, capability_summary)`
    /// per registered app, derived purely from each manifest's `[permissions]`.
    /// SECRET-FREE (counts, never paths/hosts). Sorted by name for a stable readout.
    pub async fn capability_inventory(&self) -> Vec<(String, String)> {
        let apps = self.apps.lock().await;
        let mut inv: Vec<(String, String)> = apps
            .iter()
            .map(|(name, e)| (name.clone(), capability_summary(&e.manifest.permissions)))
            .collect();
        inv.sort_by(|a, b| a.0.cmp(&b.0));
        inv
    }
}

/// Normalize an app reference for matching: lowercase, strip everything but
/// alphanumerics (so "global scan", "global-scan", "GlobalScan" all collapse
/// to "globalscan").
fn normalize_app_ref(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

// ===========================================================================
// Launch / lifecycle / socket relay
// ===========================================================================

/// Start a micro-app by name: (re)mint its token, regenerate its seatbelt
/// profile, ensure its dirs/socket, and spawn the supervised lifecycle task.
/// Idempotent — starting an already-running app is a no-op that returns Ok.
pub async fn start(registry: &Arc<AppRegistry>, name: &str) -> Result<()> {
    {
        let mut apps = registry.apps.lock().await;
        let entry = apps
            .get_mut(name)
            .ok_or_else(|| anyhow!("no micro-app named {name:?}"))?;
        if entry.running {
            info!(app = name, "micro-app already running");
            return Ok(());
        }
        // HONEST-LABELING GUARD: a spec-only app (manifest + SPEC.md, no code)
        // or an unbuilt compiled app registers deliberately (visible in the
        // deck) but must refuse to START with a clear reason — not flip
        // `running`, spawn, and die in the lifecycle with a confusing exec
        // error. Skipped under the test interpreter override, where the entry
        // is a stand-in played in-process and need not exist on disk.
        #[cfg(test)]
        let probe_entry = registry.interpreter_override.is_none();
        #[cfg(not(test))]
        let probe_entry = true;
        if probe_entry {
            let entry_abs = abs(&registry.project_root, Path::new(&entry.manifest.app.entry));
            if !entry_abs.is_file() {
                return Err(anyhow!(
                    "micro-app {name:?} isn't runnable yet — its entry {:?} does not exist (spec-only, or not built)",
                    entry.manifest.app.entry
                ));
            }
        }
        // Rotate the nonce + mint a fresh token for this launch.
        entry.nonce = fresh_nonce();
        entry.running = true;
    }
    // Mint after dropping the borrow conflict (mint_token borrows &entry).
    {
        let token = {
            let apps = registry.apps.lock().await;
            let entry = apps.get(name).expect("entry exists; just inserted");
            registry.mint_token(entry)
        };
        let mut apps = registry.apps.lock().await;
        if let Some(entry) = apps.get_mut(name) {
            entry.token = token;
        }
    }

    let reg = registry.clone();
    let name = name.to_string();
    tokio::spawn(async move {
        lifecycle(reg, name).await;
    });
    Ok(())
}

/// Stop a running micro-app: flip its running flag and WAKE the lifecycle task
/// (the notify) so it tears down immediately — kills the child via
/// kill_on_drop, removes the socket — instead of waiting for the child to exit
/// on its own.
pub async fn stop(registry: &Arc<AppRegistry>, name: &str) -> Result<()> {
    let notify = {
        let mut apps = registry.apps.lock().await;
        let entry = apps
            .get_mut(name)
            .ok_or_else(|| anyhow!("no micro-app named {name:?}"))?;
        if !entry.running {
            return Ok(());
        }
        entry.running = false;
        // Invalidate the token immediately so any in-flight line is dropped.
        entry.token.clear();
        entry.nonce.clear();
        entry.stop_notify.clone()
    };
    // Wake the lifecycle task out of its blocking select!.
    notify.notify_waiters();
    Ok(())
}

/// HOST -> APP: forward one already-structured op line to a RUNNING micro-app.
///
/// This is the op-forwarding seam the voice router uses to drive an app after
/// it is launched (e.g. `{"op":"select.net","name":"3V3"}` for Silicon Canvas).
/// `op_line` is the COMPLETE JSON op object as a single line (no trailing
/// newline needed — this adds it); the daemon forwards it VERBATIM and never
/// interprets it, so the contract for what the op means lives entirely in the
/// target app (Silicon Canvas's `src/ops.rs`). The router is responsible for
/// classifying the spoken utterance into the structured op string; the app
/// never parses natural language (SPEC §6).
///
/// Errors when the app is unknown or not running; the line is dropped (never
/// queued for a future launch) so a stale op cannot fire on the next start.
/// Delivery is best-effort once queued: the live connection handler drains the
/// queue and writes the line; a line queued between connections rides the next
/// accepted connection. The op is NOT token-stamped — host->app lines are
/// authenticated by the socket itself (the daemon owns and bound it, 0600), the
/// same trust model the start/refresh/stop control verbs already rely on; the
/// per-app capability token authenticates the REVERSE direction (app->host).
pub async fn send_op(registry: &Arc<AppRegistry>, name: &str, op_line: &str) -> Result<()> {
    let apps = registry.apps.lock().await;
    let entry = apps
        .get(name)
        .ok_or_else(|| anyhow!("no micro-app named {name:?}"))?;
    if !entry.running {
        bail!("micro-app {name:?} is not running; cannot forward op");
    }
    entry
        .op_tx
        .send(op_line.to_string())
        .map_err(|_| anyhow!("micro-app {name:?} op queue is closed"))?;
    Ok(())
}

/// One app's supervised lifecycle: bind its socket, spawn the sandboxed child,
/// relay its JSONL onto telemetry, and restart on exit within the governor's
/// budget. Returns when the app is stopped or has exhausted its restarts.
async fn lifecycle(registry: Arc<AppRegistry>, name: String) {
    let mut governor = RestartGovernor::new();

    // The stop notifier for this app, cloned once: stop() fires it to wake the
    // blocking select! below.
    let stop_notify = {
        let apps = registry.apps.lock().await;
        match apps.get(&name) {
            Some(entry) => entry.stop_notify.clone(),
            None => return,
        }
    };

    // Prepare paths + the seatbelt profile once (regenerated on each loop pass
    // so a manifest edit between restarts is picked up).
    loop {
        // Read the snapshot needed to launch under a short lock.
        let (manifest, app_dir, socket_path, profile_path, token) = {
            let apps = registry.apps.lock().await;
            let Some(entry) = apps.get(&name) else {
                return;
            };
            if !entry.running {
                cleanup_socket(&entry.socket_path);
                telemetry::emit("system", "app.stopped", json!({"name": name}));
                return;
            }
            (
                entry.manifest.clone(),
                entry.app_dir.clone(),
                entry.socket_path.clone(),
                entry.profile_path.clone(),
                entry.token.clone(),
            )
        };

        match run_once(
            &registry,
            &name,
            &manifest,
            &app_dir,
            &socket_path,
            &profile_path,
            &token,
            &stop_notify,
        )
        .await
        {
            RunResult::StoppedByHost => {
                cleanup_socket(&socket_path);
                telemetry::emit("system", "app.stopped", json!({"name": name}));
                return;
            }
            RunResult::ChildExited => {
                let now = Instant::now();
                if governor.should_restart(now) {
                    governor.record_restart(now);
                    warn!(app = %name, restart = governor.count(now), "micro-app exited; restarting");
                    // Rotate the nonce + re-mint the token for the new launch.
                    let mut apps = registry.apps.lock().await;
                    if let Some(entry) = apps.get_mut(&name) {
                        if !entry.running {
                            // Stopped while we were deciding to restart.
                            drop(apps);
                            cleanup_socket(&socket_path);
                            telemetry::emit("system", "app.stopped", json!({"name": name}));
                            return;
                        }
                        entry.nonce = fresh_nonce();
                        entry.token = registry.mint_token(entry);
                    }
                    continue;
                } else {
                    let restarts = governor.count(now);
                    error!(app = %name, restarts, "micro-app crashed too often; giving up");
                    {
                        let mut apps = registry.apps.lock().await;
                        if let Some(entry) = apps.get_mut(&name) {
                            entry.running = false;
                            entry.token.clear();
                            entry.nonce.clear();
                        }
                    }
                    cleanup_socket(&socket_path);
                    telemetry::emit(
                        "system",
                        "app.crashed",
                        json!({"name": name, "restarts": restarts}),
                    );
                    return;
                }
            }
            RunResult::LaunchFailed(e) => {
                error!(app = %name, error = %e, "micro-app launch failed");
                {
                    let mut apps = registry.apps.lock().await;
                    if let Some(entry) = apps.get_mut(&name) {
                        entry.running = false;
                        entry.token.clear();
                        entry.nonce.clear();
                    }
                }
                cleanup_socket(&socket_path);
                telemetry::emit(
                    "system",
                    "app.crashed",
                    json!({"name": name, "restarts": 0, "error": e.to_string()}),
                );
                return;
            }
        }
    }
}

enum RunResult {
    /// The host flipped running=false; tear down cleanly.
    StoppedByHost,
    /// The child process exited on its own; the governor decides on restart.
    ChildExited,
    /// Could not even launch (profile write / bind / spawn failed).
    LaunchFailed(anyhow::Error),
}

/// One launch: write the profile, bind the socket, spawn the sandboxed child,
/// accept its connection, verify+relay its JSONL until it exits or the host
/// stops it. The child is held with kill_on_drop so every early return reaps
/// it (actions.rs discipline).
#[allow(clippy::too_many_arguments)]
async fn run_once(
    registry: &Arc<AppRegistry>,
    name: &str,
    manifest: &AppManifest,
    app_dir: &Path,
    socket_path: &Path,
    profile_path: &Path,
    token: &str,
    stop_notify: &Arc<tokio::sync::Notify>,
) -> RunResult {
    // SUBSTRATE LOCK (envlock.rs) spawn gate. Armed-by-default ([envlock].enabled).
    // If this app is PINNED (has apps/<name>/env.lock), re-hash its materialized
    // closure under state/envstore/<hash>/ and verify it against the lock
    // FAIL-CLOSED: a mismatch REFUSES to spawn (never a silent fall-back to the
    // shared .venv). On a verified pin the interpreter is the one INSIDE the pinned
    // closure, so generate_sbpl narrows exec/read to that closure instead of the
    // shared .venv. An UNPINNED app (no env.lock — every app that ships today)
    // resolves to the legacy interpreter unchanged.
    let legacy_interp = registry.interpreter(manifest);
    let interp = if crate::envlock::verify_enabled() {
        let pin = crate::envlock::pin_state(&registry.project_root, app_dir);
        crate::envlock::emit_verdict(name, &pin);
        if let crate::envlock::PinState::Pinned {
            verdict: crate::envlock::SpawnVerdict::Refused { reason, .. },
            ..
        } = &pin
        {
            return RunResult::LaunchFailed(anyhow!(
                "envlock: refusing to spawn {name}: pinned dependency closure failed verification ({})",
                reason.as_str()
            ));
        }
        crate::envlock::effective_interpreter(&pin, &legacy_interp, manifest.app.runtime)
    } else {
        legacy_interp
    };

    // The HOST -> APP op queue handle for this app (shared across reconnects):
    // handle_conn moves the receiver out for the life of a connection and puts
    // it back on exit, so a line queued between connections is not lost.
    let op_rx = {
        let apps = registry.apps.lock().await;
        match apps.get(name) {
            Some(entry) => entry.op_rx.clone(),
            None => return RunResult::StoppedByHost,
        }
    };

    // Generate the seatbelt profile (also writes the on-disk AUDIT copy). The
    // returned string is the EXEC source, passed inline to `sandbox-exec -p`
    // below so no on-disk file is re-read at exec time (closes the write->exec
    // TOCTOU — a same-UID swap of the audit copy can't alter the running sandbox).
    let profile = match write_profile(manifest, &registry.project_root, &interp, app_dir, socket_path, profile_path) {
        Ok(p) => p,
        Err(e) => return RunResult::LaunchFailed(e),
    };
    // Ensure the fs_write dirs exist (the app's own state dir) so first write
    // does not fail inside the sandbox.
    ensure_write_dirs(&registry.project_root, manifest);

    // Bind the per-app socket (host owns it). Remove any stale one first.
    cleanup_socket(socket_path);
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return RunResult::LaunchFailed(anyhow!("creating socket dir: {e}"));
        }
        // Tighten the socket DIR to 0700: only the daemon's UID may even
        // traverse into it. Same-UID is the trust boundary either way, but this
        // removes the casual cross-process connect a 0755 dir would permit and
        // matches SANDBOX.md's "the daemon creates and owns the socket" claim.
        restrict_dir_perms(parent);
    }
    let listener = match UnixListener::bind(socket_path) {
        Ok(l) => l,
        Err(e) => return RunResult::LaunchFailed(anyhow!("binding {}: {e}", socket_path.display())),
    };
    // Tighten the socket itself to 0600: defense-in-depth so an unrelated
    // same-UID process cannot connect() and read the host's start/refresh/stop
    // command stream or wedge the accept/reconnect path (a local DoS). Token
    // verification already blocks INJECTION (a connector can't forge the
    // per-launch HMAC), but 0600 closes the casual-connect leak. This does not
    // stop a same-UID attacker who can chmod — that is outside the trust model.
    restrict_socket_perms(socket_path);

    // Spawn the sandboxed child: sandbox-exec -p <profile-string> <interp> <entry...>.
    // The profile is passed INLINE (not `-f <file>`) so the compiled policy is the
    // daemon's in-memory string — a same-UID edit of the on-disk audit copy cannot
    // widen the running sandbox (no file is re-read at exec time). The SBPL names
    // paths only (no secret), so it is safe in argv.
    let argv = registry.child_argv(manifest, &interp);
    let mut cmd = Command::new(SANDBOX_EXEC);
    cmd.arg("-p").arg(&profile);
    for a in &argv {
        cmd.arg(a);
    }
    // SECURITY: clear the INHERITED environment so no daemon secret crosses into a
    // sandboxed micro-app. The SBPL profile filters files/mach/network — NOT env
    // vars — so an inherited ANTHROPIC_API_KEY / ELEVENLABS_API_KEY / HF_TOKEN would
    // sail past the default-deny sandbox and be readable by a malicious app via
    // getenv(). We re-add ONLY a minimal, non-secret allowlist (mirrors shell.rs's
    // sandboxed-shell spawn, which already env_clear()s). The app learns its socket +
    // token from the env ONLY — never argv (argv is world-readable via ps). The
    // session key never appears here, only the derived token.
    cmd.env_clear();
    cmd.env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
    cmd.env("HOME", &registry.project_root);
    // Forward the vision app's non-secret capability DECLARATIONS if the operator set
    // them in the daemon env — these grant nothing (macOS TCC is the real gate).
    for var in ["DARWIN_VISION_CAMERA", "DARWIN_VISION_SCREEN"] {
        if let Ok(v) = std::env::var(var) {
            cmd.env(var, v);
        }
    }
    cmd.env("DARWIN_APP_TOKEN", token);
    cmd.env("DARWIN_APP_SOCKET", abs(&registry.project_root, socket_path));
    cmd.env("DARWIN_APP_NAME", name);
    cmd.current_dir(&registry.project_root);
    cmd.kill_on_drop(true);
    // Capture stdout/stderr so app logs become telemetry instead of polluting
    // the daemon's own stdio.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return RunResult::LaunchFailed(anyhow!("spawning sandbox-exec: {e}")),
    };
    info!(app = name, "micro-app launched under sandbox-exec");
    telemetry::emit("system", "app.started", json!({"name": name}));
    // Record the child pid for the introspect sentinel to sample (read-only).
    // The guard clears it on EVERY return path from here (StoppedByHost,
    // ChildExited, or any early error), so a dead/reused pid is never sampled —
    // same kill_on_drop discipline that reaps `child` itself.
    let _pid_guard = crate::introspect::record_child(name, child.id());
    // Fresh trust anchor per launch: drop any prior dyld module baseline so this
    // launch's first `modules` report re-seeds (trust-on-first-use). A legitimately
    // updated app loads a different module set; persisting the old baseline across
    // the relaunch would false-flag every changed module as an injection.
    crate::introspect::reset_module_baseline(name);
    // Record the app's declared jit bit so the (feature-gated) ES front-end can
    // tell an EXPECTED executable mapping (jit=true) from a W^X violation.
    crate::introspect::record_app_jit(name, manifest.permissions.jit);

    // Relay the child's stderr/stdout as app.log lines.
    if let Some(out) = child.stdout.take() {
        spawn_log_relay(name.to_string(), out);
    }
    if let Some(err) = child.stderr.take() {
        spawn_log_relay(name.to_string(), err);
    }

    // Accept the app's connection (bounded — a sandboxed app that never
    // connects must not hang the supervisor forever; we still watch the child
    // and the stop flag concurrently).
    let topic = default_topic(manifest);

    loop {
        tokio::select! {
            // The host asked us to stop — tear down now (child reaped by
            // kill_on_drop when this fn returns and `child` drops).
            _ = stop_notify.notified() => {
                info!(app = name, "stop requested; tearing down micro-app");
                return RunResult::StoppedByHost;
            }
            // The child exited on its own.
            status = child.wait() => {
                match status {
                    Ok(s) => info!(app = name, code = s.code(), "micro-app process exited"),
                    Err(e) => warn!(app = name, error = %e, "waiting on micro-app failed"),
                }
                return RunResult::ChildExited;
            }
            // A new connection from the app.
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _peer)) => {
                        // Serve this connection until it closes, the child
                        // exits, or the host stops the app. handle_conn returns
                        // the reason so the outer loop reacts correctly.
                        match handle_conn(registry, name, &topic, manifest, stream, &mut child, stop_notify, &op_rx).await {
                            ConnEnd::HostStopped => return RunResult::StoppedByHost,
                            ConnEnd::ChildExited => return RunResult::ChildExited,
                            // The connection dropped but the child is alive and
                            // the host still wants it: loop to accept a
                            // reconnect (the app may reconnect after a hiccup).
                            ConnEnd::ConnClosed => continue,
                        }
                    }
                    Err(e) => {
                        warn!(app = name, error = %e, "accept on app socket failed");
                        // Fall through to re-check the child / stop flag.
                        if !host_wants_running(registry, name).await {
                            return RunResult::StoppedByHost;
                        }
                    }
                }
            }
        }
    }
}

enum ConnEnd {
    HostStopped,
    ChildExited,
    ConnClosed,
}

/// Serve one accepted app connection: send the initial `start` command, then
/// read JSONL lines. Every inbound line's token is VERIFIED against the app's
/// current nonce+perms; a bad/missing token drops the line and emits
/// app.auth_failed. Accepted items/status lines relay onto telemetry as
/// app.data; log lines as app.log.
#[allow(clippy::too_many_arguments)]
async fn handle_conn(
    registry: &Arc<AppRegistry>,
    name: &str,
    topic: &str,
    manifest: &AppManifest,
    stream: UnixStream,
    child: &mut Child,
    stop_notify: &Arc<tokio::sync::Notify>,
    op_rx: &Arc<Mutex<Option<mpsc::UnboundedReceiver<String>>>>,
) -> ConnEnd {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Host -> app: kick it off.
    let _ = send_command(&mut write_half, "start").await;

    // Take the op receiver for the life of THIS connection. None should never
    // happen (run_once serves one connection at a time per app), but if it
    // does we still serve the connection without op forwarding rather than
    // panicking. The receiver is put back below on every exit path so a
    // reconnect resumes draining the same queue.
    let mut op_rx_guard = op_rx.lock().await.take();

    let end = serve_conn(
        registry,
        name,
        topic,
        manifest,
        &mut reader,
        &mut write_half,
        child,
        stop_notify,
        op_rx_guard.as_mut(),
    )
    .await;

    // Return the receiver so the next connection (or send_op between
    // connections, via the still-live op_tx) keeps the same queue.
    if let Some(rx) = op_rx_guard {
        *op_rx.lock().await = Some(rx);
    }
    end
}

/// Read one newline-terminated line into `line`, buffering AT MOST `max` bytes.
/// The stdlib/tokio `read_line` grows the target String without limit until it
/// sees a newline, so a malicious/compromised micro-app that sends a huge line
/// with no `\n` would OOM the daemon BEFORE any post-hoc `line.len()` check could
/// run. This caps the buffer as it fills: `Ok(0)` on EOF, the byte count on a
/// complete line, or an `InvalidData` error once `max` bytes arrive without a
/// newline (the caller then drops the connection — a well-behaved app never sends
/// a line anywhere near `MAX_APP_LINE_BYTES`).
///
/// CANCELLATION SAFETY: this future is used in a `tokio::select!` arm, so it can
/// be DROPPED mid-read whenever another arm (a queued host->app op, a stop, a
/// child exit) wins. The accumulator therefore lives in the CALLER's `pending`
/// buffer, NOT a local one: bytes already pulled off the reader survive the drop
/// and the next call resumes exactly where it left off. The single `.await` is
/// `fill_buf`, which is itself cancellation-safe (it consumes nothing until we
/// call `consume`), so no byte is ever read-and-lost. `pending` is cleared only
/// when a complete line (or EOF/oversize) is returned.
///
/// `pub(crate)` so the generate proxy (`genproxy.rs`), which reads the SAME
/// untrusted-micro-app socket line protocol, shares this one audited bounded
/// reader instead of an unbounded `read_line` that a hostile app could OOM.
pub(crate) async fn read_line_bounded(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    pending: &mut Vec<u8>,
    line: &mut String,
    max: usize,
) -> std::io::Result<usize> {
    use tokio::io::AsyncBufReadExt;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if pending.is_empty() {
                return Ok(0); // clean EOF
            }
            *line = String::from_utf8_lossy(pending).into_owned();
            let n = pending.len();
            pending.clear();
            return Ok(n); // trailing line without newline at EOF
        }
        if let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            // The newline branch is subject to the SAME cap: a line whose bytes up to
            // the newline (already-buffered `pending` + `i` in this chunk) exceed
            // `max` is rejected EXACTLY, not returned. Without this, a line that ends
            // in a newline within one fill_buf chunk could overshoot the cap by up to
            // a buffer's worth — tightened because this reader is shared with the
            // pre-auth generate proxy.
            if pending.len().saturating_add(i) > max {
                reader.consume(i + 1);
                pending.clear();
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "app line exceeds MAX_APP_LINE_BYTES",
                ));
            }
            let take = i + 1;
            pending.extend_from_slice(&chunk[..take]);
            reader.consume(take);
            *line = String::from_utf8_lossy(pending).into_owned();
            let n = pending.len();
            pending.clear();
            return Ok(n);
        }
        let take = chunk.len();
        pending.extend_from_slice(chunk);
        reader.consume(take);
        if pending.len() > max {
            pending.clear(); // drop the oversized accumulation; caller closes the conn
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "app line exceeds MAX_APP_LINE_BYTES with no newline",
            ));
        }
    }
}

/// The connection service loop, factored out so [`handle_conn`] can put the op
/// receiver back on every exit path without repeating it at each `return`.
#[allow(clippy::too_many_arguments)]
async fn serve_conn(
    registry: &Arc<AppRegistry>,
    name: &str,
    topic: &str,
    manifest: &AppManifest,
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    child: &mut Child,
    stop_notify: &Arc<tokio::sync::Notify>,
    mut op_rx: Option<&mut mpsc::UnboundedReceiver<String>>,
) -> ConnEnd {
    let mut line = String::new();
    // Persists ACROSS loop iterations so a partial line survives a select!
    // cancellation (a queued op firing mid-read) — see read_line_bounded's
    // cancellation-safety contract.
    let mut pending: Vec<u8> = Vec::new();
    loop {
        line.clear();
        // A future that resolves to the next queued op line, or never resolves
        // when there is no receiver — so the select! arm is simply inert in
        // that case rather than spinning.
        let next_op = async {
            match op_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            // Host stop: wake out of the blocking read so a quiet, idling app
            // is torn down immediately rather than at its next line / exit.
            _ = stop_notify.notified() => {
                info!(app = name, "stop requested mid-connection; tearing down");
                return ConnEnd::HostStopped;
            }
            status = child.wait() => {
                match status {
                    Ok(s) => info!(app = name, code = s.code(), "micro-app process exited"),
                    Err(e) => warn!(app = name, error = %e, "waiting on micro-app failed"),
                }
                return ConnEnd::ChildExited;
            }
            // HOST -> APP: a structured op line the router queued via send_op.
            // Forward it VERBATIM (the daemon never interprets the op body) on
            // the same socket as the control verbs. A write failure means the
            // connection is gone; loop will pick up the close/exit next.
            op = next_op => {
                // The sender is dropped only when the registry is torn down;
                // treat as nothing more to forward (do not exit the conn).
                if let Some(op_line) = op {
                    if let Err(e) = send_op_line(write_half, &op_line).await {
                        warn!(app = name, error = %e, "forwarding op to app failed");
                    }
                }
            }
            read = read_line_bounded(reader, &mut pending, &mut line, MAX_APP_LINE_BYTES) => {
                match read {
                    Ok(0) => return ConnEnd::ConnClosed, // app closed the socket
                    Ok(_) => {
                        if line.len() > MAX_APP_LINE_BYTES {
                            warn!(app = name, len = line.len(), "oversized line from app; dropping");
                            continue;
                        }
                        if !host_wants_running(registry, name).await {
                            return ConnEnd::HostStopped;
                        }
                        relay_line(registry, name, topic, manifest, line.trim()).await;
                    }
                    Err(e) => {
                        warn!(app = name, error = %e, "reading app socket failed");
                        return ConnEnd::ConnClosed;
                    }
                }
            }
        }
    }
}

/// What an authenticated App->host line resolves to, decided purely so it can
/// be unit-tested without telemetry side effects.
#[derive(Debug, PartialEq)]
enum RelayDecision {
    /// items/status: relay as app.data on this topic with this payload.
    Data { topic: String, payload: Value },
    /// log: relay as app.log with this line.
    Log { line: String },
    /// modules: an app's in-proc dyld loaded-module report — attested against a
    /// trust-on-first-use baseline in introspect.rs (defensive, observability-only).
    Modules { modules: Vec<crate::introspect::Module> },
    /// Malformed JSON, an unknown message type, or an empty line — drop it.
    Drop,
}

/// PURE classification of an already-token-verified line. The token check lives
/// in [`relay_line`] (it needs the async registry); everything after it —
/// JSON parse, type dispatch, topic resolution — is decided here so the unit
/// tests can prove an app cannot publish to an undeclared topic and that junk
/// is dropped, with no socket and no telemetry.
fn classify_inbound_line(manifest: &AppManifest, default_topic: &str, raw: &str) -> RelayDecision {
    if raw.trim().is_empty() {
        return RelayDecision::Drop;
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return RelayDecision::Drop;
    };
    let msg_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    match msg_type {
        "items" | "status" => RelayDecision::Data {
            topic: resolve_topic(manifest, default_topic, &data),
            payload: data,
        },
        "log" => {
            // Apps ship logs as data={"line":str} per the app contract; accept
            // that first, then a bare string, then any other JSON as-is.
            let line = data
                .get("line")
                .and_then(Value::as_str)
                .or_else(|| data.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| data.to_string());
            RelayDecision::Log { line }
        }
        "modules" => RelayDecision::Modules {
            modules: crate::introspect::parse_module_report(&data),
        },
        _ => RelayDecision::Drop,
    }
}

/// Parse, authenticate, and relay one App->host JSONL line.
///   {"token":str,"type":"items"|"status"|"log","data":obj}
async fn relay_line(
    registry: &Arc<AppRegistry>,
    name: &str,
    topic: &str,
    manifest: &AppManifest,
    raw: &str,
) {
    if raw.is_empty() {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        warn!(app = name, "dropping non-JSON line from app");
        return;
    };
    // Token check FIRST — a line without a valid token never reaches relay.
    let presented = value.get("token").and_then(Value::as_str).unwrap_or("");
    if !registry.verify_token(name, presented).await {
        warn!(app = name, "app line failed token verification; dropping");
        telemetry::emit("system", "app.auth_failed", json!({"name": name}));
        return;
    }
    match classify_inbound_line(manifest, topic, raw) {
        RelayDecision::Data { topic, payload } => {
            // CONTINUOUS SCREEN CONTEXT (#42): a vision.screen readout tagged
            // `read_kind=context` is a snapshot from the Vision app's DEVICE-gated
            // continuous capture loop — route its recognized text into the daemon's
            // bounded/redacted/transient context ring (the redaction + bounding
            // happen inside `ingest_continuous_snapshot`, which is itself GATED on
            // [screen_context].enabled — ships ON but INERT WITHOUT Screen-Recording
            // TCC consent (and a no-op when disabled, the ring never grows). The raw
            // text is NOT echoed to telemetry; only the honest
            // WATCHING indicator (the loop is active) rides, so the HUD can show the
            // prominent watching state without the sensitive glyphs. A one-shot
            // read (read_kind=screen/handwriting/document) is left UNTOUCHED — it is
            // the transient on-request read, never the continuous ring.
            if topic == "vision.screen"
                && payload.get("read_kind").and_then(Value::as_str) == Some("context")
            {
                let text = payload.get("text").and_then(Value::as_str).unwrap_or("");
                let ts = payload
                    .get("ts")
                    .and_then(Value::as_f64)
                    .map(|t| t as u64)
                    .unwrap_or(0);
                let src = payload
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or("screen");
                let ingested =
                    crate::screen_context::ingest_continuous_snapshot(ts, text, src);
                telemetry::emit(
                    "system",
                    "screen_context.watching",
                    // SECRET-FREE: never the recognized text — only that the loop is
                    // active (watching) and whether THIS snapshot was ingested
                    // (false when the loop is OFF, so this honestly reflects the
                    // OFF-default gate). The HUD reads this for the WATCHING badge.
                    json!({
                        "name": name,
                        "watching": crate::screen_context::is_enabled(),
                        "ingested": ingested,
                        // A bounded, secret-free count of how much recent context is
                        // held (never the glyphs) plus the hard cap — for the HUD
                        // WATCHING badge ("held N / cap M").
                        "held": crate::screen_context::global_len(),
                        "cap": crate::screen_context::global_cap(),
                    }),
                );
                // Do NOT relay the sensitive glyphs onward as app.data; the
                // continuous context lives only in the transient ring.
                return;
            }
            // LUMEN voice-navigation: a one-shot on-request screen read
            // (read_kind=screen) is the readout Lumen consults to resolve a
            // voice-named UI action — cache its controls so "read me the buttons,
            // then click the third" can select a target. READ-ONLY: the cache is
            // only ever consulted by the per-action-gated `ui_actuate` path (which
            // still PARKS for a spoken confirm), never an autonomous click; parse is
            // bounded. The readout still relays to the HUD below.
            if topic == "vision.screen"
                && payload.get("read_kind").and_then(Value::as_str) == Some("screen")
            {
                crate::lumen::remember_readout(&payload);
            }
            telemetry::emit(
                "system",
                "app.data",
                json!({"name": name, "topic": topic, "payload": payload}),
            );
        }
        RelayDecision::Log { line } => {
            telemetry::emit("system", "app.log", json!({"name": name, "line": line}));
        }
        RelayDecision::Modules { modules } => {
            // Cooperative dyld attestation: seed on first report, then flag any
            // module the baseline never had (injection / unexpected dlopen). The
            // token was already verified above, so a different process can't forge
            // this. READ-ONLY: it reports, it never unloads/blocks anything.
            let total = modules.len();
            // Envelopes come from introspect.rs's telemetry-contract builders (the
            // single source of truth for the field names the HUD reads), which key
            // the app on "app" — NOT the "name" of the app.data/app.log relay.
            match crate::introspect::attest_or_seed(name, &modules) {
                None => {
                    // First report — baseline seeded silently.
                    let (event, payload) =
                        crate::introspect::ev_modattest(name, total, 0, 0, true);
                    telemetry::emit("system", event, payload);
                }
                Some(att) => {
                    let (event, payload) = crate::introspect::ev_modattest(
                        name,
                        att.total,
                        att.unexpected.len(),
                        att.missing_count,
                        false,
                    );
                    telemetry::emit("system", event, payload);
                    // Bound the per-report fan-out: a malicious app could report up
                    // to MAX_MODULES unexpected entries, which unthrottled would be
                    // MAX_MODULES telemetry emits + findings-ring evictions per line
                    // (a telemetry-flood DoS + a way to evict real findings). The
                    // aggregate `unexpected` count already rides the single
                    // ev_modattest envelope above, so emit only the first K here and
                    // summarize the rest.
                    const MAX_VIOLATION_EMITS: usize = 16;
                    for module in att.unexpected.iter().take(MAX_VIOLATION_EMITS) {
                        // Finding ring is user/cloud-facing -> redact the home
                        // prefix; the telemetry envelope below keeps the full path.
                        crate::introspect::record_finding(crate::introspect::redact_home(&format!(
                            "module: {name} loaded unexpected {}",
                            module.path
                        )));
                        let (event, payload) =
                            crate::introspect::ev_module_violation(name, &module.path, &module.uuid);
                        telemetry::emit("system", event, payload);
                    }
                    if att.unexpected.len() > MAX_VIOLATION_EMITS {
                        crate::introspect::record_finding(format!(
                            "module: {name} +{} more unexpected modules (per-report cap)",
                            att.unexpected.len() - MAX_VIOLATION_EMITS
                        ));
                    }
                }
            }
        }
        RelayDecision::Drop => {
            warn!(app = name, "app sent an unhandled/empty line; dropping");
        }
    }
}

/// Topic for an app.data relay: a topic the app names in its data IF it is one
/// the manifest declared, else the manifest's first declared topic, else
/// "feed". Apps can never publish to a topic they did not declare.
fn resolve_topic(manifest: &AppManifest, default: &str, data: &Value) -> String {
    if let Some(requested) = data.get("topic").and_then(Value::as_str) {
        if manifest
            .ui
            .telemetry_topics
            .iter()
            .any(|t| t == requested)
        {
            return requested.to_string();
        }
    }
    default.to_string()
}

/// The default telemetry topic for an app's data: its first declared topic, or
/// "feed" when it declared none (the contract default).
fn default_topic(manifest: &AppManifest) -> String {
    manifest
        .ui
        .telemetry_topics
        .first()
        .cloned()
        .unwrap_or_else(|| "feed".to_string())
}

/// Host -> app command line: {"type":"start"|"refresh"|"stop"}.
async fn send_command(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    command: &str,
) -> std::io::Result<()> {
    let mut line = json!({"type": command}).to_string();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

/// Host -> app: write one already-structured op line VERBATIM, JSONL-framed.
/// The daemon never interprets the body — the op contract lives in the target
/// app — so this writes exactly what the router queued, trimming any trailing
/// newline and re-appending a single one so the framing is well-formed.
async fn send_op_line(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    op_line: &str,
) -> std::io::Result<()> {
    let mut line = op_line.trim_end_matches('\n').to_string();
    line.push('\n');
    write_half.write_all(line.as_bytes()).await?;
    write_half.flush().await
}

/// Is the app still supposed to be running?
async fn host_wants_running(registry: &Arc<AppRegistry>, name: &str) -> bool {
    let apps = registry.apps.lock().await;
    apps.get(name).map(|e| e.running).unwrap_or(false)
}

/// Read ONE line from a buffered stream, capping the retained bytes at `max`. If a
/// line exceeds `max` before a newline, the first `max` bytes are kept and the rest
/// (up to the next newline or EOF) is DRAINED WITHOUT BUFFERING — so a hostile
/// micro-app streaming a newline-free flood on its stdout cannot grow the daemon's
/// memory without bound, while logging RESYNCS on the next line. Returns `Ok(None)`
/// at clean EOF, `Ok(Some(()))` when `out` holds a (possibly truncated) line. The
/// sole `.await` is `fill_buf` (cancellation-safe), and peak memory is `max` + one
/// fill_buf chunk. Generic over any buffered reader, so it bounds the stdout/stderr
/// relay just as `read_line_bounded` bounds the socket relay.
async fn read_capped_log_line<R>(
    reader: &mut R,
    out: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Option<()>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    out.clear();
    let mut saw_any = false;
    let mut overflowed = false; // cap reached: keep draining to the newline, stop buffering
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return if saw_any { Ok(Some(())) } else { Ok(None) };
        }
        saw_any = true;
        if let Some(i) = chunk.iter().position(|&b| b == b'\n') {
            if !overflowed {
                let room = max.saturating_sub(out.len());
                out.extend_from_slice(&chunk[..room.min(i)]); // exclude the '\n'
            }
            reader.consume(i + 1);
            return Ok(Some(()));
        }
        if !overflowed {
            let room = max.saturating_sub(out.len());
            if chunk.len() <= room {
                out.extend_from_slice(chunk);
            } else {
                out.extend_from_slice(&chunk[..room]);
                overflowed = true;
            }
        }
        let n = chunk.len();
        reader.consume(n);
    }
}

/// Relay one of the child's stdio streams as app.log telemetry, line by line.
/// BOUNDED per line (see [`read_capped_log_line`]): a micro-app fully controls its
/// own stdout, and this relay is attached to EVERY launched app, so an unbounded
/// `next_line()` would let a hostile app OOM the daemon with a newline-free flood.
fn spawn_log_relay<R>(name: String, stream: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(stream);
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(Some(())) = read_capped_log_line(&mut reader, &mut buf, MAX_APP_LINE_BYTES).await {
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            telemetry::emit("system", "app.log", json!({"name": name, "line": line}));
        }
    });
}

/// Write the seatbelt profile to disk (creating its dir).
/// Sequence counter for unique temp-profile names (so a same-UID pre-plant can
/// never sit at the exact temp path we `create_new`).
static PROFILE_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Generate the seatbelt profile, RETURN it (the exec source — see below), and
/// write an on-disk AUDIT COPY that the introspect sentinel monitors for
/// integrity. Returns the profile string so the launcher can pass it to
/// `sandbox-exec -p` INLINE.
///
/// TOCTOU: the EXECUTED policy is the returned in-memory string, handed to
/// `sandbox-exec -p <profile>` on the command line — so a same-UID edit of the
/// on-disk copy between this write and the exec CANNOT widen (or alter) the
/// running sandbox (there is no file for the launcher to re-read at exec time).
/// The on-disk copy at `profile_path` is therefore an AUDIT ARTIFACT, not the
/// exec source: it is written atomically to an owner-only (0600) unique temp via
/// `create_new` (so a pre-planted symlink or looser-mode file at the temp path
/// cannot hijack the write) and renamed into place, and its fingerprint is
/// recorded so the introspect drift sentinel can flag any later tampering of the
/// record. (The SBPL is not secret — it names paths, no token/key — so passing
/// it in argv is fine; argv carries no secret, per the launch's env-only rule.)
fn write_profile(
    manifest: &AppManifest,
    project_root: &Path,
    interp: &Path,
    app_dir: &Path,
    socket_path: &Path,
    profile_path: &Path,
) -> Result<String> {
    let profile = generate_sbpl(manifest, project_root, interp, app_dir, socket_path);
    let parent = profile_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("profile path has no parent dir"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating profile dir {}", parent.display()))?;
    // Owner-only atomic write of the audit copy via a UNIQUE temp + create_new
    // (O_EXCL: never follows a symlink, fails on any pre-existing path) so no
    // same-UID pre-plant can redirect or loosen it; then rename into place.
    let seq = PROFILE_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{}.{}.{}.sb.tmp", manifest.name(), std::process::id(), seq));
    {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp_path)
            .with_context(|| format!("creating temp profile {}", tmp_path.display()))?;
        f.write_all(profile.as_bytes())
            .with_context(|| format!("writing temp profile {}", tmp_path.display()))?;
        f.flush().ok();
    }
    std::fs::rename(&tmp_path, profile_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path); // don't leak the temp on failure
        anyhow::anyhow!("installing audit profile {}: {e}", profile_path.display())
    })?;
    // Fingerprint the audit copy so the introspect sentinel can flag later
    // tampering of the record (the executed policy is the returned string, so
    // this is an integrity signal on the audit artifact, not the exec source).
    crate::introspect::record_profile(manifest.name(), &profile);
    Ok(profile)
}

/// Create the app's declared fs_write directories so the first write inside
/// the sandbox does not fail on a missing parent.
fn ensure_write_dirs(project_root: &Path, manifest: &AppManifest) {
    for w in &manifest.permissions.fs_write {
        let dir = abs(project_root, Path::new(w));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), error = %e, "could not pre-create app write dir");
        }
    }
}

/// Remove an app's socket file (missing is fine).
fn cleanup_socket(socket_path: &Path) {
    if let Err(e) = std::fs::remove_file(socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %socket_path.display(), error = %e, "failed to remove app socket");
        }
    }
}

/// Set a path's permission bits, warning (not failing) on error — these are
/// defense-in-depth tightenings, not load-bearing for correctness.
fn set_mode(path: &Path, mode: u32, what: &str) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        warn!(path = %path.display(), error = %e, "could not tighten {what} permissions");
    }
}

/// Restrict the bound per-app socket to 0600 (owner read/write only).
fn restrict_socket_perms(socket_path: &Path) {
    set_mode(socket_path, 0o600, "app socket");
}

/// Restrict the per-app socket directory to 0700 (owner-only traversal).
fn restrict_dir_perms(dir: &Path) {
    set_mode(dir, 0o700, "app socket dir");
}

/// A fresh per-launch nonce: hex of 16 bytes of OS entropy. Distinct from the
/// session key (which is the HMAC secret); the nonce is non-secret and rotates
/// per launch so a leaked token dies on restart.
fn fresh_nonce() -> String {
    let mut buf = [0u8; 16];
    match std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut buf))
    {
        Ok(()) => hex::encode(buf),
        Err(_) => {
            // Extremely unlikely; fall back to a time+pid mix so a launch still
            // gets a unique-per-launch nonce rather than a fixed string.
            let t = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{t:x}{:x}", std::process::id())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> AppManifest {
        let raw = r#"
            [app]
            name        = "global-scan"
            version     = "0.1.0"
            description = "Intel feed aggregator."
            entry       = "apps/global-scan/main.py"
            runtime     = "python"

            [permissions]
            audio     = false
            gpu       = false
            net_hosts = ["feeds.npr.org", "hnrss.org"]
            fs_read   = ["state/ipc/inference.sock"]
            fs_write  = ["state/apps/global-scan"]

            [ui]
            surface          = "panel"
            telemetry_topics = ["feed"]
        "#;
        AppManifest::parse(raw, "global-scan").expect("sample manifest parses")
    }

    /// A manifest with the given `[permissions]` body, else the sample shape.
    fn manifest_with_perms(perms: &str) -> Result<AppManifest> {
        let raw = format!(
            r#"
            [app]
            name        = "probe"
            version     = "0.1.0"
            description = "ceiling probe."
            entry       = "apps/probe/main.py"
            runtime     = "python"

            [permissions]
            {perms}

            [ui]
            surface          = "panel"
            telemetry_topics = ["feed"]
        "#
        );
        AppManifest::parse(&raw, "probe")
    }

    // -- capability ceiling (Wave A) ------------------------------------
    #[test]
    fn ceiling_rejects_an_escaping_or_absolute_fs_path() {
        // Absolute fs_write is refused.
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[]\nfs_read=[]\nfs_write=[\"/etc\"]"
        )
        .is_err());
        // A `..` escape in fs_read is refused.
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[]\nfs_read=[\"../../etc/passwd\"]\nfs_write=[]"
        )
        .is_err());
        // A confined in-project path is allowed (state/tmp + apps/<x>/data shapes
        // the first-party apps actually use).
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[]\nfs_read=[\"state/ipc/inference.sock\"]\nfs_write=[\"state/tmp/probe\"]"
        )
        .is_ok());
    }

    #[test]
    fn ceiling_rejects_a_non_bare_or_overlong_net_hosts() {
        // A URL / path / port in net_hosts is refused (must be a bare hostname).
        for bad in ["https://evil.com", "evil.com/path", "host:8080", "a b"] {
            assert!(
                manifest_with_perms(&format!(
                    "audio=false\ngpu=false\nnet_hosts=[\"{bad}\"]\nfs_read=[]\nfs_write=[]"
                ))
                .is_err(),
                "net_host {bad:?} must be rejected"
            );
        }
        // A bare hostname (incl. the .local printer shape fab-link uses) is fine.
        assert!(manifest_with_perms(
            "audio=false\ngpu=false\nnet_hosts=[\"octoprint.local\"]\nfs_read=[]\nfs_write=[]"
        )
        .is_ok());
        // Over the count ceiling (>16) is refused.
        let many = (0..17).map(|i| format!("\"h{i}.example\"")).collect::<Vec<_>>().join(",");
        assert!(manifest_with_perms(&format!(
            "audio=false\ngpu=false\nnet_hosts=[{many}]\nfs_read=[]\nfs_write=[]"
        ))
        .is_err());
    }

    #[test]
    fn ceiling_does_not_ban_first_party_elevated_permissions() {
        // audio/gpu/camera are LEGITIMATE for first-party apps (nexus/vision) —
        // the runtime ceiling bounds path/host SHAPE, not these declarations.
        assert!(manifest_with_perms(
            "audio=true\ngpu=true\nnet_hosts=[]\nfs_read=[]\nfs_write=[\"state/tmp/probe\"]"
        )
        .is_ok());
    }

    // -- manifest parse -------------------------------------------------

    #[test]
    fn manifest_parses_full_schema() {
        let m = sample_manifest();
        assert_eq!(m.app.name, "global-scan");
        assert_eq!(m.app.version, "0.1.0");
        assert_eq!(m.app.runtime, Runtime::Python);
        assert_eq!(m.app.entry, "apps/global-scan/main.py");
        assert!(!m.permissions.audio);
        assert!(!m.permissions.gpu);
        assert_eq!(m.permissions.net_hosts, vec!["feeds.npr.org", "hnrss.org"]);
        assert_eq!(m.permissions.fs_read, vec!["state/ipc/inference.sock"]);
        assert_eq!(m.permissions.fs_write, vec!["state/apps/global-scan"]);
        assert_eq!(m.ui.surface, "panel");
        assert_eq!(m.ui.telemetry_topics, vec!["feed"]);
    }

    #[test]
    fn manifest_name_must_match_directory() {
        let raw = r#"
            [app]
            name = "global-scan"
            version = "0.1.0"
            description = "x"
            entry = "main.py"
            runtime = "python"
        "#;
        assert!(AppManifest::parse(raw, "global-scan").is_ok());
        let err = AppManifest::parse(raw, "wrong-dir").unwrap_err().to_string();
        assert!(err.contains("must match its directory"), "{err}");
    }

    #[test]
    fn manifest_rejects_unknown_keys_and_unknown_runtime() {
        // Unknown permission key — must not silently widen/narrow the sandbox.
        let raw = r#"
            [app]
            name = "x"
            version = "0.1.0"
            description = "d"
            entry = "main.py"
            runtime = "python"
            [permissions]
            net_hots = ["a.com"]
        "#;
        assert!(AppManifest::parse(raw, "x").is_err(), "typo'd key must be rejected");

        let bad_runtime = r#"
            [app]
            name = "x"
            version = "0.1.0"
            description = "d"
            entry = "main.py"
            runtime = "ruby"
        "#;
        assert!(AppManifest::parse(bad_runtime, "x").is_err(), "unknown runtime rejected");
    }

    #[test]
    fn manifest_defaults_empty_permissions_and_ui() {
        let raw = r#"
            [app]
            name = "bare"
            version = "0.1.0"
            description = "d"
            entry = "bare"
            runtime = "binary"
        "#;
        let m = AppManifest::parse(raw, "bare").unwrap();
        assert!(!m.permissions.audio && !m.permissions.gpu);
        assert!(m.permissions.net_hosts.is_empty());
        assert_eq!(m.ui.surface, "panel"); // default surface
        assert!(m.ui.telemetry_topics.is_empty());
    }

    #[test]
    fn camera_and_screen_default_false_and_omitting_them_still_parses() {
        // The NEW camera/screen keys are #[serde(default)] => false. EVERY
        // existing manifest omits them, so omission must parse and leave both
        // false (camera/screen-denied). This is the invariant that keeps all
        // shipped manifests (global-scan, silicon-canvas) green.
        let m = sample_manifest();
        assert!(!m.permissions.camera, "camera defaults false when omitted");
        assert!(!m.permissions.screen, "screen defaults false when omitted");

        // When a manifest DOES declare them, they parse through.
        let raw = r#"
            [app]
            name = "vision"
            version = "0.1.0"
            description = "d"
            entry = "vision"
            runtime = "binary"
            [permissions]
            gpu = true
            camera = true
            screen = true
        "#;
        let v = AppManifest::parse(raw, "vision").unwrap();
        assert!(v.permissions.camera);
        assert!(v.permissions.screen);
    }

    #[test]
    fn shipped_vision_manifest_parses_with_tcc_keys() {
        // The shipped Vision manifest must parse under the extended schema: it
        // is offline (net_hosts empty), GPU-on (ANE/Core ML), and declares the
        // camera/screen TCC needs. (It currently keeps camera/screen as TOML
        // comments pending this schema land; we assert the parse + the offline /
        // gpu invariants regardless of whether the keys are uncommented yet.)
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("vision");
        let m = AppManifest::load(&path).expect("shipped vision manifest must parse");
        assert_eq!(m.name(), "vision");
        assert_eq!(m.app.runtime, Runtime::Binary);
        // Defensive-only + on-device: fully offline.
        assert!(
            m.permissions.net_hosts.is_empty(),
            "Vision must be fully offline (net_hosts = [])"
        );
        assert!(m.permissions.gpu, "Vision uses the ANE/GPU for built-in Vision requests");
        assert!(!m.permissions.audio, "Vision never touches the microphone");
        // Declared topics include the detection + status streams.
        assert!(m.ui.telemetry_topics.iter().any(|t| t == "vision.detections"));
    }

    #[test]
    fn shipped_global_scan_manifest_parses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("global-scan");
        let m = AppManifest::load(&path).expect("shipped global-scan manifest must parse");
        assert_eq!(m.name(), "global-scan");
        assert_eq!(m.app.runtime, Runtime::Python);
        // The manifest's net_hosts MUST be exactly the feed hostnames (the
        // contract requires lockstep with feeds.toml).
        assert!(m.permissions.net_hosts.contains(&"feeds.npr.org".to_string()));
        assert!(m.permissions.net_hosts.contains(&"hnrss.org".to_string()));
        assert_eq!(m.permissions.fs_write, vec!["state/apps/global-scan"]);
        assert_eq!(m.ui.telemetry_topics, vec!["feed"]);
    }

    /// Lockstep: every hostname in the manifest's net_hosts must appear as a
    /// URL host in feeds.toml, and vice versa — the seatbelt allow-list and the
    /// feed list cannot drift.
    #[test]
    fn manifest_net_hosts_match_feeds_toml_hosts() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("global-scan");
        let m = AppManifest::load(&base).unwrap();
        let mut manifest_hosts: Vec<String> = m.permissions.net_hosts.clone();
        manifest_hosts.sort();

        let feeds_raw = std::fs::read_to_string(base.join("feeds.toml")).unwrap();
        // Extract every https://HOST/ from the feeds file.
        let mut feed_hosts: Vec<String> = feeds_raw
            .lines()
            .filter_map(|l| {
                let l = l.trim();
                let start = l.find("https://")? + "https://".len();
                let rest = &l[start..];
                let end = rest.find('/').unwrap_or(rest.len());
                Some(rest[..end].to_string())
            })
            .collect();
        feed_hosts.sort();
        feed_hosts.dedup();

        assert_eq!(
            manifest_hosts, feed_hosts,
            "manifest net_hosts and feeds.toml hosts must be identical"
        );
    }

    // -- SBPL generation ------------------------------------------------

    fn gen_profile(m: &AppManifest) -> String {
        let root = Path::new("/Users/test/darwin");
        let interp = root.join(".venv/bin/python3");
        let app_dir = root.join("apps/global-scan");
        let sock = root.join("state/ipc/apps/global-scan.sock");
        generate_sbpl(m, root, &interp, &app_dir, &sock)
    }

    #[test]
    fn sbpl_is_default_deny() {
        let p = gen_profile(&sample_manifest());
        assert!(p.starts_with("(version 1)\n"), "must start with version");
        assert!(p.contains("(deny default)"), "must be default-deny");
    }

    #[test]
    fn sbpl_grants_exec_read_write_for_declared_paths() {
        let p = gen_profile(&sample_manifest());
        // Exec the interpreter + the app dir.
        assert!(p.contains("(allow process-exec* (literal \"/Users/test/darwin/.venv/bin/python3\"))"));
        assert!(p.contains("(allow process-exec* (subpath \"/Users/test/darwin/apps/global-scan\"))"));
        // Read the app dir + the venv + the declared fs_read.
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/apps/global-scan\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/.venv\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/state/ipc/inference.sock\"))"));
        // Write the declared fs_write only.
        assert!(p.contains("(allow file-write* (subpath \"/Users/test/darwin/state/apps/global-scan\"))"));
        // Connect to its own socket.
        assert!(p.contains("(allow network-outbound (literal \"/Users/test/darwin/state/ipc/apps/global-scan.sock\"))"));
    }

    #[test]
    fn sbpl_fs_read_unix_socket_gets_af_unix_connect_grant() {
        // Finding #4 fix (SBPL side): a declared fs_read entry that IS a Unix
        // socket needs an AF_UNIX network-outbound literal grant IN ADDITION to
        // its file-read* subpath — file-read alone does not permit connect() on
        // this macOS. A NORMAL (non-.sock) fs_read entry must NOT get one.
        let mut m = sample_manifest();
        m.permissions.fs_read = vec![
            "state/ipc/apps/generate.sock".to_string(), // a socket
            "state/shared/config.json".to_string(),     // a normal file
        ];
        let p = gen_profile(&m);
        // Both get the file-read* subpath grant (unchanged behavior).
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/state/ipc/apps/generate.sock\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/test/darwin/state/shared/config.json\"))"));
        // Only the .sock entry gets the AF_UNIX connect() literal.
        assert!(
            p.contains("(allow network-outbound (literal \"/Users/test/darwin/state/ipc/apps/generate.sock\"))"),
            "a .sock fs_read entry must get an AF_UNIX connect grant"
        );
        assert!(
            !p.contains("(allow network-outbound (literal \"/Users/test/darwin/state/shared/config.json\"))"),
            "a normal file fs_read entry must NOT get a network-outbound grant"
        );
        // And the grant lands AFTER the (deny network*) for the no-network
        // branch would; here net_hosts is non-empty so (deny network*) is
        // present and last-match-wins must keep the connect alive.
        let deny_idx = p.find("(deny network*)").expect("deny network present");
        let grant_idx = p
            .find("(allow network-outbound (literal \"/Users/test/darwin/state/ipc/apps/generate.sock\"))")
            .expect("socket grant present");
        assert!(grant_idx > deny_idx, "the connect grant must come after the network deny");
    }

    #[test]
    fn sbpl_network_is_host_filtered_when_listed() {
        let p = gen_profile(&sample_manifest());
        assert!(p.contains("(system-network)"));
        assert!(p.contains("(allow network-outbound (remote tcp (host-name \"feeds.npr.org\")))"));
        assert!(p.contains("(allow network-outbound (remote tcp (host-name \"hnrss.org\")))"));
        // DNS is granted on port 53 — pinned to the system resolver address(es)
        // when /etc/resolv.conf is readable, else *:53. Either way a :53 grant
        // must be present so the app can resolve the feed hosts.
        assert!(
            p.contains("(remote udp \"") && p.contains(":53\""),
            "a DNS (:53) grant must be present"
        );
        // No grant for a host that was NOT declared.
        assert!(!p.contains("host-name \"evil.com\""));
    }

    #[test]
    fn sbpl_dns_is_pinned_to_system_resolvers_when_available() {
        // When /etc/resolv.conf yields resolver IPs, DNS must be pinned to
        // those addresses (not *:53) — the DNS-exfil-channel hardening. We
        // assert against the ACTUAL system resolvers so the test reflects the
        // host it runs on; if none are configured the generator falls back to
        // *:53 (and this assertion is vacuously satisfied by the fallback).
        let resolvers = system_resolvers();
        let p = gen_profile(&sample_manifest());
        if resolvers.is_empty() {
            assert!(p.contains("(allow network-outbound (remote udp \"*:53\"))"));
        } else {
            // No wildcard DNS grant survived.
            assert!(
                !p.contains("\"*:53\""),
                "wildcard DNS must not be granted when a resolver is known"
            );
            for r in &resolvers {
                assert!(
                    p.contains(&format!("(remote udp \"{r}:53\")")),
                    "DNS must be pinned to resolver {r}"
                );
            }
        }
    }

    #[test]
    fn sbpl_exec_is_literal_only_never_a_broad_prefix() {
        // Finding #2 fix: exec must be granted ONLY on literal interpreter
        // paths + the app's own dir subpath — NEVER a broad /opt/homebrew or
        // /usr/local subpath that would let the app exec arbitrary binaries.
        let p = gen_profile(&sample_manifest());
        assert!(!p.contains("(allow process-exec* (subpath \"/opt/homebrew\"))"));
        assert!(!p.contains("(allow process-exec* (subpath \"/usr/local\"))"));
        // The only process-exec* subpath is the app's own directory.
        let exec_subpaths: Vec<&str> = p
            .lines()
            .filter(|l| l.contains("process-exec* (subpath"))
            .collect();
        assert_eq!(exec_subpaths.len(), 1, "only the app dir may be an exec subpath: {exec_subpaths:?}");
        assert!(exec_subpaths[0].contains("apps/global-scan"));
    }

    #[test]
    fn sbpl_file_read_metadata_is_scoped_never_blanket() {
        // Finding #1 fix: a bare `(allow file-read-metadata)` (no path filter)
        // is an arbitrary-path stat side channel and must NEVER be emitted.
        let p = gen_profile(&sample_manifest());
        assert!(
            !p.lines().any(|l| l.trim() == "(allow file-read-metadata)"),
            "blanket file-read-metadata must never be emitted"
        );
        // Every metadata grant is subpath-scoped, and to a root we also granted
        // file-read* on (e.g. the app dir).
        assert!(p.contains("(allow file-read-metadata (subpath \"/Users/test/darwin/apps/global-scan\"))"));
    }

    #[test]
    fn interpreter_install_prefix_derivation() {
        // <prefix>/bin/python3.11 -> <prefix>
        assert_eq!(
            interpreter_install_prefix(Path::new(
                "/opt/homebrew/Cellar/python@3.11/3.11.9/bin/python3.11"
            )),
            Some(PathBuf::from("/opt/homebrew/Cellar/python@3.11/3.11.9"))
        );
        // Not in a bin/ dir -> None (no broad-ancestor grant).
        assert_eq!(
            interpreter_install_prefix(Path::new("/opt/homebrew/python3")),
            None
        );
        // Pathologically shallow prefix -> None (would re-open a broad tree).
        assert_eq!(interpreter_install_prefix(Path::new("/usr/bin/python3")), None);
    }

    #[test]
    fn system_resolvers_only_accepts_literal_ips() {
        // The parser must reject anything that is not a literal IP so a hostile
        // resolv.conf can never inject SBPL syntax. We exercise the real reader
        // (it reads the host's /etc/resolv.conf) and assert every returned
        // entry parses as an IP.
        for r in system_resolvers() {
            assert!(
                r.parse::<std::net::IpAddr>().is_ok(),
                "system_resolvers returned a non-IP: {r:?}"
            );
        }
    }

    #[test]
    fn sbpl_no_network_when_net_hosts_empty() {
        let mut m = sample_manifest();
        m.permissions.net_hosts.clear();
        let p = gen_profile(&m);
        assert!(p.contains("(deny network*)"), "empty net_hosts -> no network");
        assert!(!p.contains("(system-network)"));
        assert!(!p.contains("host-name"));
    }

    #[test]
    fn sbpl_denies_mic_and_gpu_by_default_and_grants_nothing_stray() {
        let p = gen_profile(&sample_manifest());
        assert!(p.contains("(deny device-microphone)"), "audio=false denies mic");
        assert!(p.contains("AGXDeviceUserClient"), "gpu=false denies the GPU client");
        // No stray write grant outside the declared path: the only file-write*
        // subpath is the declared one (state/apps/global-scan); the socket is
        // a literal, not a subpath.
        let write_subpaths: Vec<&str> = p
            .lines()
            .filter(|l| l.contains("file-write* (subpath"))
            .collect();
        assert_eq!(write_subpaths.len(), 1, "exactly one write subpath: {write_subpaths:?}");
        assert!(write_subpaths[0].contains("state/apps/global-scan"));
    }

    #[test]
    fn sbpl_gpu_true_omits_the_gpu_deny() {
        let mut m = sample_manifest();
        m.permissions.gpu = true;
        let p = gen_profile(&m);
        assert!(!p.contains("AGXDeviceUserClient"), "gpu=true must not deny the GPU client");
    }

    #[test]
    fn sbpl_jit_defaults_denied_and_never_emits_legacy_dynamic_signature() {
        // Every existing manifest omits `jit` -> jit=false -> explicit deny of the
        // ONE current operation (dynamic-code-generation). The legacy
        // `dynamic-signature` op must NEVER be emitted (not a live operation).
        let p = gen_profile(&sample_manifest());
        assert!(
            p.contains("(deny dynamic-code-generation)"),
            "jit=false must explicitly deny dynamic-code-generation"
        );
        assert!(
            !p.contains("dynamic-signature"),
            "the non-current dynamic-signature op must never be emitted"
        );
        assert!(
            !p.contains("(allow dynamic-code-generation)"),
            "jit=false must not allow dynamic-code-generation"
        );
    }

    #[test]
    fn sbpl_jit_true_allows_dynamic_code_generation_and_documents_the_entitlement_caveat() {
        let mut m = sample_manifest();
        m.permissions.jit = true;
        let p = gen_profile(&m);
        assert!(
            p.contains("(allow dynamic-code-generation)"),
            "jit=true must allow dynamic-code-generation"
        );
        assert!(
            !p.contains("(deny dynamic-code-generation)"),
            "jit=true must not also deny it"
        );
        // The best-effort honesty note (the process still needs the allow-jit
        // entitlement) must be present so the profile never pretends SBPL alone
        // enables JIT — same discipline as the camera/screen TCC caveat.
        assert!(
            p.contains("allow-jit"),
            "jit=true must document that the process also needs cs.allow-jit"
        );
        // Still never the legacy op.
        assert!(!p.contains("dynamic-signature"));
    }

    #[test]
    fn sbpl_camera_and_screen_default_deny_when_unset() {
        // An app that does NOT declare camera/screen (every existing one) must
        // get the explicit camera/screen denies and NONE of the best-effort
        // plumbing allows.
        let p = gen_profile(&sample_manifest()); // camera=false, screen=false
        assert!(
            p.contains("(deny iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"),
            "camera=false must explicitly deny the camera device client"
        );
        assert!(
            p.contains("(deny mach-lookup (global-name \"com.apple.windowserver.active\"))"),
            "screen=false must explicitly deny the window-server lookup"
        );
        // No best-effort capture plumbing leaks in when both are false.
        assert!(!p.contains("(allow iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"));
        assert!(!p.contains("AppleCameraAssistant"));
    }

    #[test]
    fn sbpl_camera_screen_grant_is_best_effort_and_documents_tcc_is_the_real_gate() {
        // With camera/screen declared, the profile grants ONLY best-effort
        // plumbing AND must DOCUMENT that TCC — not SBPL — is the real gate, so
        // the profile never pretends to enable capture on its own.
        let mut m = sample_manifest();
        m.permissions.camera = true;
        m.permissions.screen = true;
        let p = gen_profile(&m);

        // Best-effort plumbing present (reaches the capture stack + consent
        // prompt) but NOT a capture grant — there is no such SBPL op.
        assert!(p.contains("(allow iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"));
        assert!(p.contains("(allow mach-lookup (global-name \"com.apple.windowserver.active\"))"));
        assert!(p.contains("com.apple.tccd"), "must allow reaching tccd for the consent prompt");
        // The explicit denies are gone now that the keys are true.
        assert!(!p.contains("(deny iokit-open (iokit-user-client-class \"IOVideoDeviceUserClient\"))"));
        assert!(!p.contains("(deny mach-lookup (global-name \"com.apple.windowserver.active\"))"));

        // Honesty requirement: the profile DOCUMENTS that TCC is the real gate
        // and is NOT SBPL-grantable — for BOTH camera and screen.
        assert!(
            p.contains("macOS TCC (Camera) is the REAL gate"),
            "camera block must document TCC as the real gate"
        );
        assert!(
            p.contains("macOS TCC (Screen Recording) is the\n;; REAL gate"),
            "screen block must document TCC as the real gate"
        );
        assert!(
            p.contains("NOT SBPL-grantable") || p.contains("NOT\n;; SBPL-grantable"),
            "must state TCC is not SBPL-grantable"
        );
        // Still default-deny overall.
        assert!(p.contains("(deny default)"));
    }

    #[test]
    fn sbpl_string_escaping_neutralizes_quotes() {
        // A path with a quote must not break out of the SBPL string literal.
        let escaped = sbpl_str(Path::new("/tmp/a\"b\\c"));
        assert_eq!(escaped, "\"/tmp/a\\\"b\\\\c\"");
    }

    /// Regression-lock the PRODUCTION profile: generate it from the shipped
    /// global-scan manifest with a realistic project root and assert the
    /// invariants the app actually depends on to launch and stay contained.
    #[test]
    fn sbpl_for_shipped_global_scan_manifest_is_correct() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("apps")
            .join("global-scan");
        let m = AppManifest::load(&base).unwrap();
        let root = Path::new("/Users/op/darwin");
        let interp = root.join(".venv/bin/python3");
        let app_dir = root.join("apps/global-scan");
        let sock = root.join("state/ipc/apps/global-scan.sock");
        let p = generate_sbpl(&m, root, &interp, &app_dir, &sock);

        // Boots: default-deny + the Apple base profile import (so python can
        // actually start) + exec on the configured interpreter literal (the
        // symlinked venv python). Exec is LITERAL-only — never a broad
        // Homebrew/usr-local subpath (finding #2).
        assert!(p.contains("(deny default)"));
        // The bsd.sb import is emitted whenever that stock macOS profile exists
        // (it does on the M-series targets); the generator gates on it, so the
        // test gates the same way to stay portable to a stripped CI image.
        if Path::new(BSD_BASE_PROFILE).exists() {
            assert!(p.contains("(import \"/System/Library/Sandbox/Profiles/bsd.sb\")"));
        }
        assert!(p.contains("(allow process-exec* (literal \"/Users/op/darwin/.venv/bin/python3\"))"));
        assert!(!p.contains("(allow process-exec* (subpath \"/opt/homebrew\"))"));
        assert!(!p.contains("(allow process-exec* (subpath \"/usr/local\"))"));
        // Reads: the app dir, the venv (read prefix), and its one declared
        // fs_read — the daemon-mediated generate PROXY socket, NOT the raw
        // inference.sock (finding #4 fix); writes: only its own app state dir.
        assert!(p.contains("(allow file-read* (subpath \"/Users/op/darwin/.venv\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/op/darwin/apps/global-scan\"))"));
        assert!(p.contains("(allow file-read* (subpath \"/Users/op/darwin/state/ipc/apps/generate.sock\"))"));
        // The raw inference socket is NO LONGER reachable by the app.
        assert!(
            !p.contains("inference.sock"),
            "the app must have no grant of any kind to the raw inference.sock"
        );
        assert!(p.contains("(allow file-write* (subpath \"/Users/op/darwin/state/apps/global-scan\"))"));
        // Connects to its own host socket...
        assert!(p.contains("(allow network-outbound (literal \"/Users/op/darwin/state/ipc/apps/global-scan.sock\"))"));
        // ...and gets the AF_UNIX connect() grant for the .sock fs_read entry
        // (file-read alone does not permit connect() on this macOS).
        assert!(p.contains("(allow network-outbound (literal \"/Users/op/darwin/state/ipc/apps/generate.sock\"))"));
        // Network is deny-then-allow-listed: every feed host is granted, and
        // nothing else. Assert all nine declared hosts are host-filtered.
        assert!(p.contains("(deny network*)"));
        for host in &m.permissions.net_hosts {
            assert!(
                p.contains(&format!("(remote tcp (host-name \"{host}\")))")),
                "missing host-filter for {host}"
            );
        }
        // No write grant outside the declared app dir.
        let write_subpaths: Vec<&str> = p.lines().filter(|l| l.contains("file-write* (subpath")).collect();
        assert_eq!(write_subpaths.len(), 1, "exactly one write subpath: {write_subpaths:?}");
        // Mic + GPU denied (audio=false, gpu=false).
        assert!(p.contains("(deny device-microphone)"));
        assert!(p.contains("AGXDeviceUserClient"));
    }

    // -- token mint / verify --------------------------------------------

    const TEST_KEY: &[u8] = b"unit-test-session-key-not-the-real-one";

    fn perms(net: &[&str]) -> PermissionsSection {
        PermissionsSection {
            audio: false,
            gpu: false,
            net_hosts: net.iter().map(|s| s.to_string()).collect(),
            fs_read: vec!["state/ipc/inference.sock".to_string()],
            fs_write: vec!["state/apps/global-scan".to_string()],
            // camera/screen default false (Default) — these token tests model an
            // existing app that declares neither, so the canonical form keeps the
            // camera=false;screen=false suffix.
            ..Default::default()
        }
    }

    #[test]
    fn token_roundtrips_and_is_deterministic() {
        let p = perms(&["feeds.npr.org"]);
        let t1 = compute_token(TEST_KEY, "global-scan", &p, "nonce-A");
        let t2 = compute_token(TEST_KEY, "global-scan", &p, "nonce-A");
        assert_eq!(t1, t2, "same inputs -> same token");
        assert!(verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-A", &t1));
    }

    #[test]
    fn token_forgery_is_rejected() {
        let p = perms(&["feeds.npr.org"]);
        // A made-up token never verifies.
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-A", "deadbeef"));
        // A valid token under a DIFFERENT key fails (the secret is the gate).
        let other = compute_token(b"some-other-key", "global-scan", &p, "nonce-A");
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-A", &other));
    }

    #[test]
    fn token_is_bound_to_nonce_name_and_permissions() {
        let p = perms(&["feeds.npr.org"]);
        let t = compute_token(TEST_KEY, "global-scan", &p, "nonce-A");
        // Stale nonce (a leaked token after a restart rotated the nonce).
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "nonce-B", &t));
        // Cross-app: another app presenting global-scan's token.
        assert!(!verify_token_with_key(TEST_KEY, "algo-core", &p, "nonce-A", &t));
        // Tampered permission set (a manifest that widened net_hosts after the
        // token was minted).
        let widened = perms(&["feeds.npr.org", "evil.com"]);
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &widened, "nonce-A", &t));
    }

    #[test]
    fn token_is_bound_to_camera_and_screen_flags() {
        // camera/screen join the bound set: a token minted for a camera-less
        // app must NOT verify for the same app after it flips camera (or screen)
        // on — the same anti-privilege-escalation discipline as net_hosts.
        let base = perms(&["feeds.npr.org"]);
        let t = compute_token(TEST_KEY, "vision", &base, "nonce-A");
        assert!(verify_token_with_key(TEST_KEY, "vision", &base, "nonce-A", &t));

        let mut cam = base.clone();
        cam.camera = true;
        assert!(
            !verify_token_with_key(TEST_KEY, "vision", &cam, "nonce-A", &t),
            "flipping camera on must invalidate a token minted without it"
        );
        let mut scr = base.clone();
        scr.screen = true;
        assert!(
            !verify_token_with_key(TEST_KEY, "vision", &scr, "nonce-A", &t),
            "flipping screen on must invalidate a token minted without it"
        );
    }

    #[test]
    fn capability_summary_lists_only_granted_caps_with_counts() {
        // A locked-down app reads short.
        let bare = PermissionsSection::default();
        assert_eq!(capability_summary(&bare), "sandboxed (no extra capabilities)");

        // A grant set lists only what's granted, counts for the list-valued ones,
        // and never the paths/hosts themselves (secret-free).
        let p = PermissionsSection {
            audio: true,
            gpu: false,
            camera: true,
            screen: false,
            jit: true,
            net_hosts: vec!["a.com".into(), "b.com".into()],
            fs_read: vec!["state/x".into()],
            fs_write: vec![],
        };
        let s = capability_summary(&p);
        assert_eq!(s, "audio, camera, jit, net(2), fs_read(1)");
        assert!(!s.contains("a.com"), "must not leak the actual hosts");
        assert!(!s.contains("gpu"), "an ungranted cap is omitted");
        assert!(!s.contains("fs_write"), "an empty list is omitted");
    }

    #[test]
    fn token_is_bound_to_jit_flag() {
        // jit joins the bound set: a token minted for a non-JIT app must NOT
        // verify after the manifest flips jit on — same anti-privilege-escalation
        // discipline as camera/screen/net_hosts. This is what makes auto-promoting
        // an app to jit=true detectable rather than silent.
        let base = perms(&["feeds.npr.org"]);
        let t = compute_token(TEST_KEY, "algo-core", &base, "nonce-A");
        assert!(verify_token_with_key(TEST_KEY, "algo-core", &base, "nonce-A", &t));
        let mut jit = base.clone();
        jit.jit = true;
        assert!(
            !verify_token_with_key(TEST_KEY, "algo-core", &jit, "nonce-A", &t),
            "flipping jit on must invalidate a token minted without it"
        );
    }

    #[test]
    fn canonical_permissions_is_order_independent() {
        let a = perms(&["b.com", "a.com"]);
        let b = perms(&["a.com", "b.com"]);
        assert_eq!(canonical_permissions(&a), canonical_permissions(&b));
        // ...so the token is identical regardless of declaration order.
        assert_eq!(
            compute_token(TEST_KEY, "x", &a, "n"),
            compute_token(TEST_KEY, "x", &b, "n")
        );
        // But a genuinely different set differs.
        let c = perms(&["a.com"]);
        assert_ne!(canonical_permissions(&a), canonical_permissions(&c));
    }

    #[test]
    fn token_rejects_non_hex_input() {
        let p = perms(&["feeds.npr.org"]);
        // Garbage that is not even hex must be rejected before the MAC compare.
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "not-hex-zz", &compute_token(TEST_KEY, "global-scan", &p, "n")[..1]));
        assert!(!verify_token_with_key(TEST_KEY, "global-scan", &p, "n", "zzzz"));
    }

    // -- restart governor math ------------------------------------------

    #[test]
    fn governor_allows_up_to_max_then_gives_up() {
        let mut g = RestartGovernor::with_limits(Duration::from_secs(300), 3);
        let t0 = Instant::now();
        // 3 restarts allowed within the window.
        assert!(g.should_restart(t0));
        g.record_restart(t0);
        assert!(g.should_restart(t0));
        g.record_restart(t0);
        assert!(g.should_restart(t0));
        g.record_restart(t0);
        // The 4th is refused.
        assert!(!g.should_restart(t0), "4th restart within the window is refused");
        assert_eq!(g.count(t0), 3);
    }

    #[test]
    fn governor_forgets_restarts_outside_the_window() {
        let window = Duration::from_secs(300);
        let t0 = Instant::now();

        // Just past the window: all three have aged out, budget is full again.
        let mut g = RestartGovernor::with_limits(window, 3);
        g.record_restart(t0);
        g.record_restart(t0);
        g.record_restart(t0);
        let later = t0 + window + Duration::from_secs(1);
        assert!(g.should_restart(later), "restarts outside the window are forgotten");
        assert_eq!(g.count(later), 0);

        // At exactly the window boundary they are still counted (the retain
        // keeps marks whose age is <= window). Fresh governor: count() mutates
        // (it evicts), so this must not run after the past-window eviction
        // above.
        let mut g = RestartGovernor::with_limits(window, 3);
        g.record_restart(t0);
        g.record_restart(t0);
        g.record_restart(t0);
        let boundary = t0 + window;
        assert_eq!(g.count(boundary), 3, "marks exactly at the window edge still count");
    }

    // -- name normalization / resolution --------------------------------

    #[test]
    fn app_ref_normalization_collapses_spacing_and_case() {
        assert_eq!(normalize_app_ref("global scan"), "globalscan");
        assert_eq!(normalize_app_ref("Global-Scan"), "globalscan");
        assert_eq!(normalize_app_ref("  GLOBAL  SCAN  "), "globalscan");
        assert_eq!(normalize_app_ref("global-scan"), normalize_app_ref("global scan"));
        assert_eq!(normalize_app_ref(""), "");
    }

    // -- inbound line classification (post-auth, pure) ------------------

    #[test]
    fn inbound_items_relay_as_data_on_the_default_topic() {
        let m = sample_manifest(); // telemetry_topics = ["feed"]
        let line = r#"{"token":"x","type":"items","data":{"brief":"b","items":[]}}"#;
        match classify_inbound_line(&m, "feed", line) {
            RelayDecision::Data { topic, payload } => {
                assert_eq!(topic, "feed");
                assert_eq!(payload["brief"], "b");
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn inbound_cannot_publish_to_an_undeclared_topic() {
        let m = sample_manifest(); // only "feed" is declared
        // The app asks for a topic it never declared -> falls back to default.
        let line = r#"{"token":"x","type":"status","data":{"topic":"secrets","feeds_ok":3}}"#;
        match classify_inbound_line(&m, "feed", line) {
            RelayDecision::Data { topic, .. } => {
                assert_eq!(topic, "feed", "undeclared topic must not be honored");
            }
            other => panic!("expected Data, got {other:?}"),
        }
        // A DECLARED topic the app names is honored.
        let mut m2 = m.clone();
        m2.ui.telemetry_topics = vec!["feed".into(), "alerts".into()];
        let line = r#"{"token":"x","type":"items","data":{"topic":"alerts"}}"#;
        match classify_inbound_line(&m2, "feed", line) {
            RelayDecision::Data { topic, .. } => assert_eq!(topic, "alerts"),
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn inbound_log_and_junk_are_classified_correctly() {
        let m = sample_manifest();
        assert_eq!(
            classify_inbound_line(&m, "feed", r#"{"type":"log","data":"hello"}"#),
            RelayDecision::Log { line: "hello".into() }
        );
        // The shape every shipped app actually sends: data={"line":str}.
        assert_eq!(
            classify_inbound_line(&m, "feed", r#"{"type":"log","data":{"line":"hello"}}"#),
            RelayDecision::Log { line: "hello".into() }
        );
        // Empty, non-JSON, and unknown types all drop.
        assert_eq!(classify_inbound_line(&m, "feed", "   "), RelayDecision::Drop);
        assert_eq!(classify_inbound_line(&m, "feed", "not json"), RelayDecision::Drop);
        assert_eq!(
            classify_inbound_line(&m, "feed", r#"{"type":"exec","data":{}}"#),
            RelayDecision::Drop
        );
    }

    #[test]
    fn inbound_modules_report_classifies_as_modules() {
        let m = sample_manifest();
        let line = r#"{"token":"x","type":"modules","data":{"modules":[
            {"path":"/usr/lib/libSystem.B.dylib","uuid":"AAAA"},
            {"path":"/app/main"}
        ]}}"#;
        match classify_inbound_line(&m, "feed", line) {
            RelayDecision::Modules { modules } => {
                assert_eq!(modules.len(), 2);
                assert_eq!(modules[0].path, "/usr/lib/libSystem.B.dylib");
                assert_eq!(modules[0].uuid.as_deref(), Some("AAAA"));
                assert_eq!(modules[1].uuid, None);
            }
            other => panic!("expected Modules, got {other:?}"),
        }
        // A modules report with no usable entries still classifies as Modules
        // (empty) — it is a valid type, just an empty inventory, not a Drop.
        match classify_inbound_line(&m, "feed", r#"{"type":"modules","data":{}}"#) {
            RelayDecision::Modules { modules } => assert!(modules.is_empty()),
            other => panic!("expected empty Modules, got {other:?}"),
        }
    }

    // -- hermetic socket + token handshake + relay + stop integration ---
    //
    // ONE integration test, hermetic and fast: a tempdir project root and a
    // discovered manifest, the host's REAL per-app socket bound by start(),
    // and a plain in-process UnixStream standing in for the sandboxed app. It
    // exercises the full host path that the seatbelt child would otherwise
    // drive — bind+accept, the "start" command the host sends, token verify on
    // every inbound line, telemetry relay of a VALID line, drop+auth_failed for
    // a FORGED line, and stop() teardown (socket removed, token dead).
    //
    // The APP role (the socket peer) is played in-process for a deterministic
    // relay; the sandboxed child is a stand-in idle /bin/sleep, so we do NOT
    // depend on a real sandboxed Python booting (that bootstrap is environment-
    // coupled and is instead validated by the manual seatbelt probes during
    // development and the pure sbpl_* unit tests above). The test is a macOS
    // seatbelt integration test and skips cleanly where sandbox-exec is absent.
    #[tokio::test]
    async fn socket_token_handshake_relay_and_stop_round_trip() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader as TokioBufReader};

        // macOS-only: needs the seatbelt wrapper + Apple's base profile so the
        // stand-in child can launch. Skip cleanly anywhere they are absent.
        if !(Path::new(SANDBOX_EXEC).exists() && Path::new(BSD_BASE_PROFILE).exists()) {
            eprintln!("skipping: sandbox-exec / bsd.sb not present on this host");
            return;
        }

        // A SHORT, NON-SYMLINKED root: AF_UNIX socket paths must fit in SUN_LEN
        // (~104 bytes on macOS) — the default temp dir under /var/folders blows
        // that with the app subpath appended — and /tmp is a symlink to
        // /private/tmp, so seatbelt path filters (which see the resolved path)
        // wouldn't match a /tmp grant. /private/tmp is short and real.
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-it-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/echo-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::create_dir_all(root.join("state/ipc/apps")).unwrap();
        std::fs::create_dir_all(root.join("state/apps")).unwrap();

        let manifest = r#"
            [app]
            name = "echo-app"
            version = "0.1.0"
            description = "hermetic test echo app"
            entry = "apps/echo-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/echo-app"]
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#;
        std::fs::write(app_dir.join("manifest.toml"), manifest).unwrap();

        // Subscribe to telemetry BEFORE launch so we catch the relay.
        let mut events = crate::telemetry::subscribe_for_test();

        let mut registry = AppRegistry::discover(&root);
        // Override the interpreter to a stand-in idle child: the host spawns a
        // real sandboxed `/bin/sleep` (proving the live launch path), while the
        // app role over the socket is played in-process below for determinism.
        Arc::get_mut(&mut registry).unwrap().interpreter_override =
            Some(PathBuf::from("/bin/sleep"));
        assert!(registry.resolve_name("echo app").await.is_some(), "app discovered");

        start(&registry, "echo-app").await.unwrap();

        let sock_path = root.join("state/ipc/apps/echo-app.sock");
        let mut waited = 0;
        while !sock_path.exists() && waited < 60 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        assert!(sock_path.exists(), "host bound the app socket");

        // The minted token verifies; a forged one is rejected (the exact gate
        // relay_line applies to every inbound line).
        let good_token = {
            let apps = registry.apps.lock().await;
            apps.get("echo-app").unwrap().token.clone()
        };
        assert!(!good_token.is_empty(), "token minted at launch");
        assert!(registry.verify_token("echo-app", &good_token).await);
        assert!(!registry.verify_token("echo-app", "deadbeef").await);

        // Play the app: connect to the host socket, read the host's "start"
        // command, then send a VALID token-stamped items line and a FORGED one.
        let stream = UnixStream::connect(&sock_path).await.expect("connect to host socket");
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = TokioBufReader::new(read_half);

        // The host immediately sends {"type":"start"}.
        let mut start_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut start_line))
            .await
            .expect("host sends a command promptly")
            .expect("read host command");
        let cmd: Value = serde_json::from_str(start_line.trim()).unwrap();
        assert_eq!(cmd["type"], "start", "host kicks the app with a start command");

        // HOST -> APP op forwarding: the router queues a structured op via
        // send_op; the live connection handler must write it VERBATIM to the
        // app socket (after the start command, JSONL-framed). This is the seam
        // the Silicon Canvas voice routing drives.
        send_op(&registry, "echo-app", r#"{"op":"select.net","name":"3V3"}"#)
            .await
            .expect("queue op for a running app");
        let mut op_line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut op_line))
            .await
            .expect("host forwards the op promptly")
            .expect("read forwarded op");
        let forwarded: Value = serde_json::from_str(op_line.trim()).unwrap();
        assert_eq!(forwarded["op"], "select.net", "the op tag is forwarded verbatim");
        assert_eq!(forwarded["name"], "3V3", "the op body is forwarded verbatim");

        let good = serde_json::json!({
            "token": good_token, "type": "items",
            "data": {"brief": "hello", "items": [{"title": "t"}]}
        });
        let forged = serde_json::json!({
            "token": "deadbeef", "type": "items", "data": {"brief": "EVIL"}
        });
        write_half
            .write_all(format!("{good}\n{forged}\n").as_bytes())
            .await
            .unwrap();
        write_half.flush().await.unwrap();

        // Drain telemetry: the VALID line relays as app.data on the declared
        // topic; the FORGED line emits app.auth_failed and its payload NEVER
        // appears on the wire.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_data = false;
        let mut saw_auth_failed = false;
        let mut saw_evil = false;
        while Instant::now() < deadline && !(saw_data && saw_auth_failed) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, events.recv()).await {
                Ok(Ok(line)) => {
                    if line.contains("EVIL") {
                        saw_evil = true;
                    }
                    let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if v["event"] == "app.data" && v["data"]["name"] == "echo-app" {
                        saw_data = true;
                        assert_eq!(v["data"]["topic"], "feed", "relayed on the declared topic");
                        assert_eq!(v["data"]["payload"]["brief"], "hello");
                    }
                    if v["event"] == "app.auth_failed" && v["data"]["name"] == "echo-app" {
                        saw_auth_failed = true;
                    }
                }
                _ => break,
            }
        }
        assert!(saw_data, "the valid token-stamped items line was relayed as app.data");
        assert!(saw_auth_failed, "the forged line emitted app.auth_failed");
        assert!(!saw_evil, "a forged line's payload must NEVER be relayed");

        // Stop: the lifecycle task wakes on the notify, reaps the sandboxed
        // child (kill_on_drop) and removes the socket; the token dies with the
        // nonce so a previously-valid token no longer verifies.
        stop(&registry, "echo-app").await.unwrap();
        let mut waited = 0;
        while sock_path.exists() && waited < 80 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            waited += 1;
        }
        assert!(!sock_path.exists(), "socket removed on stop");
        assert!(
            !registry.verify_token("echo-app", &good_token).await,
            "token is dead after stop (nonce cleared)"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// discover() SKIPS a manifest whose [app].entry resolves outside the app's
    /// own directory (the legacy "python3 main.py" command form / a bare binary
    /// name), reporting it as app.manifest_invalid instead of registering an app
    /// that would fail silently at spawn. A within-dir entry registers normally,
    /// even when the target file is not present (build-state independent — a
    /// binary artifact registers before it is built).
    #[tokio::test]
    async fn discover_rejects_entry_outside_app_dir_and_keeps_valid_ones() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-entryguard-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        // good-app: entry resolves inside its dir (no main.py on disk — the guard
        // is structural, not an existence check).
        let good = root.join("apps/good-app");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::write(
            good.join("manifest.toml"),
            r#"
            [app]
            name = "good-app"
            version = "0.1.0"
            description = "valid entry"
            entry = "apps/good-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = []
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#,
        )
        .unwrap();
        // bad-app: the legacy command form resolves to <root>/python3 main.py,
        // OUTSIDE apps/bad-app -> must be skipped.
        let bad = root.join("apps/bad-app");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(
            bad.join("manifest.toml"),
            r#"
            [app]
            name = "bad-app"
            version = "0.1.0"
            description = "entry resolves outside the app dir"
            entry = "python3 main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = []
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#,
        )
        .unwrap();

        let mut events = crate::telemetry::subscribe_for_test();
        let registry = AppRegistry::discover(&root);

        assert!(
            registry.resolve_name("good app").await.is_some(),
            "a within-dir entry registers"
        );
        assert!(
            registry.resolve_name("bad app").await.is_none(),
            "an entry resolving outside the app dir is skipped"
        );

        // The skip is REPORTED (not silent): app.manifest_invalid for bad-app.
        let mut saw_invalid = false;
        while let Ok(line) = events.try_recv() {
            if line.contains("app.manifest_invalid") && line.contains("bad-app") {
                saw_invalid = true;
            }
        }
        assert!(saw_invalid, "the skipped manifest is reported as app.manifest_invalid");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// send_op rejects an unknown app and an app that is not running, and drops
    /// the line rather than queueing it for a future launch — a stale op must
    /// never fire on the next start. No socket / no child needed: the gate is
    /// the registry's running flag.
    #[tokio::test]
    async fn send_op_rejects_unknown_and_not_running_apps() {
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-sendop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        let app_dir = root.join("apps/echo-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        let manifest = r#"
            [app]
            name = "echo-app"
            version = "0.1.0"
            description = "hermetic test echo app"
            entry = "apps/echo-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = ["state/apps/echo-app"]
            [ui]
            surface = "panel"
            telemetry_topics = ["feed"]
        "#;
        std::fs::write(app_dir.join("manifest.toml"), manifest).unwrap();

        let registry = AppRegistry::discover(&root);

        // Unknown app -> error.
        let err = send_op(&registry, "no-such-app", r#"{"op":"erc.run"}"#)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no micro-app named"), "{err}");

        // Registered but NOT running -> error (the line is dropped, not queued).
        let err = send_op(&registry, "echo-app", r#"{"op":"erc.run"}"#)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running"), "{err}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// read_line_bounded must be CANCELLATION-SAFE: dropped mid-read (a select!
    /// arm losing the race) it must not lose bytes it already pulled off the
    /// reader. We prove it by driving a partial line, cancelling the read via a
    /// timer that wins a select!, then resuming — the reassembled line must be
    /// WHOLE. (With the accumulator local to the future, as it was before, the
    /// prefix would be consumed-then-dropped and the resumed read would return
    /// only the tail — the exact desync this guards.)
    #[tokio::test]
    async fn a_spec_only_app_registers_but_is_labeled_not_runnable_and_refuses_to_start() {
        // Regression (full-OS sweep): a manifest whose entry doesn't exist (a
        // spec-only app, or an unbuilt compiled one) used to register as fully
        // runnable, then flip `running` + spawn + die with a confusing exec
        // error. It must register (visible in the deck), report entry_present
        // false, and refuse to start with a clear reason.
        let root = PathBuf::from(format!(
            "/private/tmp/jrv-specapp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() % 1_000_000
        ));
        let app_dir = root.join("apps/spec-app");
        std::fs::create_dir_all(&app_dir).unwrap();
        // manifest + SPEC.md, but NO main.py at the declared entry.
        std::fs::write(
            app_dir.join("manifest.toml"),
            r#"
            [app]
            name = "spec-app"
            version = "0.1.0"
            description = "spec-only, no code yet"
            entry = "apps/spec-app/main.py"
            runtime = "python"
            [permissions]
            audio = false
            gpu = false
            net_hosts = []
            fs_read = []
            fs_write = []
            "#,
        )
        .unwrap();

        let registry = AppRegistry::discover(&root);
        let info = registry.list().await;
        let spec = info.iter().find(|a| a.name == "spec-app").expect("registers despite no entry");
        assert!(!spec.entry_present, "labeled not-runnable (entry absent)");
        let err = start(&registry, "spec-app").await.expect_err("start refuses a spec-only app");
        assert!(err.to_string().contains("isn't runnable yet"), "honest refusal: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn read_line_bounded_is_cancellation_safe_across_a_dropped_read() {
        let (mut client, server) = UnixStream::pair().expect("unix socketpair");
        let (read_half, _write_half) = server.into_split();
        let mut reader = BufReader::new(read_half);
        let mut pending: Vec<u8> = Vec::new();
        let mut line = String::new();

        // App sends the FIRST half of a line — no newline yet.
        client.write_all(b"hello wor").await.expect("write prefix");

        // A read races a 50ms timer. read_line_bounded consumes "hello wor" (no
        // newline) then awaits more data; the timer wins, so the read future is
        // DROPPED. The consumed prefix must survive in `pending`.
        tokio::select! {
            _ = read_line_bounded(&mut reader, &mut pending, &mut line, MAX_APP_LINE_BYTES) => {
                panic!("read must not complete before a newline arrives");
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }
        assert_eq!(pending, b"hello wor", "consumed prefix must persist across the cancel");

        // The rest of the line arrives; the next read must return the WHOLE line.
        client.write_all(b"ld\n").await.expect("write suffix");
        let n = read_line_bounded(&mut reader, &mut pending, &mut line, MAX_APP_LINE_BYTES)
            .await
            .expect("read completes");
        assert_eq!(line, "hello world\n", "line reassembled whole after cancellation");
        assert_eq!(n, "hello world\n".len());
        assert!(pending.is_empty(), "pending is cleared once a full line is returned");
    }

    /// DoS DEFENSE (shared by the app-relay socket AND the generate proxy). A line
    /// that exceeds `max` with NO newline must ERROR (so the caller drops the
    /// connection) rather than buffer unboundedly — a hostile app cannot OOM the
    /// daemon by streaming a newline-free flood. We use a tiny `max` and a modest
    /// over-cap write to prove the bound without allocating megabytes.
    #[tokio::test]
    async fn read_line_bounded_errors_on_an_overlong_line_with_no_newline() {
        let (mut client, server) = UnixStream::pair().expect("unix socketpair");
        let (read_half, _write_half) = server.into_split();
        let mut reader = BufReader::new(read_half);
        let mut pending: Vec<u8> = Vec::new();
        let mut line = String::new();
        // 64 bytes, no newline, cap = 16 -> must exceed and error. Keep `client`
        // alive so the reader sees data (not EOF, which would return a trailing line).
        client.write_all(&[b'x'; 64]).await.expect("write flood");
        let err = read_line_bounded(&mut reader, &mut pending, &mut line, 16)
            .await
            .expect_err("an over-cap no-newline line must error, not buffer unboundedly");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidData,
            "overflow must be InvalidData so the caller drops the connection"
        );
    }

    /// The cap is EXACT even when the over-long line ends in a newline within one
    /// read: a line whose bytes exceed `max` is rejected, not returned (closes the
    /// one-buffer overshoot on the newline branch).
    #[tokio::test]
    async fn read_line_bounded_errors_on_an_overlong_line_that_ends_in_a_newline() {
        let (mut client, server) = UnixStream::pair().expect("unix socketpair");
        let (read_half, _write_half) = server.into_split();
        let mut reader = BufReader::new(read_half);
        let mut pending: Vec<u8> = Vec::new();
        let mut line = String::new();
        // 40 bytes then a newline, cap = 16 -> the whole line (incl. its terminator)
        // arrives in one chunk; it must ERROR, not return a 40-byte line.
        client.write_all(&[b'x'; 40]).await.expect("write body");
        client.write_all(b"\n").await.expect("write newline");
        let err = read_line_bounded(&mut reader, &mut pending, &mut line, 16)
            .await
            .expect_err("an over-cap line ending in a newline must still error");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    /// DoS DEFENSE for the stdout/stderr LOG relay. An over-long line is TRUNCATED
    /// to the cap (memory bounded), the rest is drained, and logging RESYNCS on the
    /// next line — a hostile app's newline-free flood can't OOM the daemon, and a
    /// normal line after it is still relayed whole.
    #[tokio::test]
    async fn read_capped_log_line_truncates_flood_and_resyncs() {
        // A 100-byte no-newline flood, a newline, then a normal line, then EOF.
        let mut data = vec![b'A'; 100];
        data.push(b'\n');
        data.extend_from_slice(b"next line\n");
        let mut reader = BufReader::new(&data[..]);
        let mut buf: Vec<u8> = Vec::new();

        // First line: capped to 16 bytes (the flood is truncated, not buffered whole).
        read_capped_log_line(&mut reader, &mut buf, 16).await.unwrap().unwrap();
        assert_eq!(buf.len(), 16, "over-long line truncated to the cap");
        assert!(buf.iter().all(|&b| b == b'A'), "kept the leading bytes: {buf:?}");

        // Second line: resynced past the flood, relayed WHOLE.
        read_capped_log_line(&mut reader, &mut buf, 16).await.unwrap().unwrap();
        assert_eq!(buf, b"next line", "logging resyncs on the next line");

        // Clean EOF.
        assert!(
            read_capped_log_line(&mut reader, &mut buf, 16).await.unwrap().is_none(),
            "clean EOF returns None"
        );
    }
}
