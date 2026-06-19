//! Self-Forge: JARVIS authoring a NEW sandboxed micro-app from a goal.
//!
//! This is heal.rs generalized — from "patch a daemon bug" to "author a whole
//! micro-app" — behind the EXACT same hard gates. The self-heal drafter feeds
//! source context to Opus and gets a unified diff back, stages + validates it,
//! and PROPOSES it for a human to apply. Self-Forge feeds a GOAL to Opus and
//! gets a complete app back (a manifest, source file(s), tests), stages +
//! validates it in a confined dir, and PROPOSES it for a human to deploy.
//!
//! Pipeline (every gate is hard and NEVER weakened):
//!   1. CONFIG GATE — [forge] enabled must be true (else the watchdog/entry
//!      does nothing). mode = "propose" (default) | "auto". CRUCIAL: even in
//!      "auto" there is NO auto-DEPLOY of a forged app into apps/ — DEPLOY is
//!      ALWAYS a separate human step (scripts/apply_forge.sh). mode only ever
//!      governs the forge's OWN staged artifact; no daemon code path moves a
//!      proposal into apps/, period.
//!   2. DRAFT — forge_draft(goal) asks the heavy model (via the ForgeBrain
//!      seam, MOCK in tests) to AUTHOR a complete micro-app: a manifest.toml
//!      with MINIMAL permissions, the source file(s), and tests. The structured
//!      response is parsed into staged files. Requires the cloud key at runtime
//!      (a friendly blocked outcome when absent, exactly like heal). NEVER run
//!      a real draft in tests — only the #[ignore]'d real-cloud drill does.
//!   3. STAGE — write the authored files to a CONFINED state/forge/staging-<ts>/
//!      dir (NOT apps/). Validate the manifest: the app name is a safe
//!      identifier; the permissions are MINIMAL and on the allowed minimal set
//!      (over-broad requests — device access, writes outside the app's own
//!      state dir, escaping reads — are REJECTED); a default-deny SBPL can be
//!      generated from it.
//!   4. VALIDATE — in the staging dir, build + run the app's tests under a
//!      TIMEOUT cap (cargo check+test for a Rust app, py_compile for python),
//!      plus a manifest+SBPL sanity check. Fail -> quarantine to
//!      state/forge/rejected/<ts>/ with the reason. Pass -> proceed.
//!   5. PROPOSE — write the validated app + a human-readable report to
//!      state/forge/proposals/<ts>/, stamp meta.forge_pending, and STOP. The
//!      app is NOT in apps/, NOT registered, NOT running. scripts/apply_forge.sh
//!      <ts> is the SEPARATE human step that moves the proposed app into apps/
//!      after the human reviews it.
//!
//! SAFETY CONTRACT (non-negotiable, mirrors self-heal):
//!   - ships enabled=false / mode=propose (config gate);
//!   - PROPOSE-ONLY: the daemon NEVER writes into apps/, NEVER registers a
//!     forged app, NEVER runs generated code live; the one place generated code
//!     runs is the CONFINED staging build/test (a throwaway dir, timeout-capped,
//!     kill_on_drop);
//!   - born sandboxed: the proposed app carries a default-deny SBPL derived from
//!     a MINIMAL manifest + a capability token at deploy time (the existing
//!     apps.rs runtime), so deployment lands it in exactly the micro-app box;
//!   - DEPLOY is a separate HUMAN step (scripts/apply_forge.sh) that re-validates
//!     and refuses anything outside state/forge/proposals/.
//!   The cloud is reached ONLY through the ForgeBrain trait — unit tests mock
//!   it; the only real cloud path is the #[ignore]'d forge_drill_real_cloud.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::json;

use crate::anthropic;
use crate::apps::{self, AppManifest, PermissionsSection, Runtime};
use crate::config::Config;
use crate::memory::Memory;
use crate::telemetry;

// ---------------------------------------------------------------------------
// Tunables (mirror heal.rs).
// ---------------------------------------------------------------------------

/// Draft call: heavy model, latency-insensitive, room for thinking + a whole
/// small app's worth of source.
const DRAFT_MAX_TOKENS: u32 = 8192;
const DRAFT_TIMEOUT: Duration = Duration::from_secs(300);

/// Staging build+test deadline (cargo check && cargo test, or py_compile).
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(600);

/// Report tail kept from validation output (chars).
const REPORT_TAIL_CHARS: usize = 4000;

/// A forged app may declare at most this many outbound hosts. A larger ask is
/// rejected as over-broad (a freshly-authored app with no track record has no
/// business reaching dozens of hosts).
const MAX_NET_HOSTS: usize = 6;

/// Largest single authored source file we accept (bytes). A wildly large file
/// is a sign the model went off the rails; reject rather than stage it.
const MAX_AUTHORED_FILE_BYTES: usize = 256 * 1024;

const DRAFT_SYSTEM: &str = "You are JARVIS's app forge: an expert engineer who authors a small, \
     SELF-CONTAINED, sandboxable micro-app from a goal. You output a strict file manifest and \
     nothing else — no prose outside the requested structure. The app must build and pass its own \
     tests offline, and request the MINIMAL permissions it truly needs.";

const META_FORGE_PENDING: &str = "meta.forge_pending";

// ---------------------------------------------------------------------------
// Cloud seam (trait) — the ONLY route to the cloud. Production uses CloudBrain
// (anthropic::complete_plain); unit tests inject a mock so no cloud call is
// ever made under `cargo test`. The #[ignore]'d forge_drill_real_cloud is the
// one real cloud path.
// ---------------------------------------------------------------------------

/// A `Send` future returned by the trait method, spelled out so the trait stays
/// object-safe (`&dyn ForgeBrain`) WITHOUT the async-trait crate (no new deps).
type BrainFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// The app-author seam — the ONLY route to the cloud. `goal` in, the raw model
/// text out (a `=== FILE: path ===`-delimited bundle, parsed by the caller via
/// [`parse_authored_app`]). Unit tests inject a mock so no cloud call is made
/// under `cargo test`.
pub trait ForgeBrain: Send + Sync {
    fn author<'a>(&'a self, goal: &'a str) -> BrainFuture<'a>;
}

/// Production ForgeBrain: the heavy Anthropic model via anthropic.rs.
pub struct CloudBrain {
    pub model: String,
}

impl ForgeBrain for CloudBrain {
    fn author<'a>(&'a self, goal: &'a str) -> BrainFuture<'a> {
        Box::pin(async move {
            anthropic::complete_plain(
                &self.model,
                DRAFT_MAX_TOKENS,
                DRAFT_SYSTEM,
                &draft_prompt(goal),
                DRAFT_TIMEOUT,
            )
            .await
        })
    }
}

// ---------------------------------------------------------------------------
// (2) Draft prompt — pure, unit-tested.
// ---------------------------------------------------------------------------

/// The drafting prompt: a goal in, a strict file-bundle out. The format is
/// `=== FILE: <relative/path> ===` markers, each followed by that file's body,
/// so [`parse_authored_app`] can reconstruct the staged tree. The permission
/// guidance is explicit so the model authors a MINIMAL manifest the validator
/// will accept (the validator is still the hard gate; the prompt only nudges).
fn draft_prompt(goal: &str) -> String {
    format!(
        "Author a small, self-contained JARVIS micro-app that accomplishes this goal:\n\n\
         GOAL: {goal}\n\n\
         A micro-app is one directory under apps/<name>/ with a manifest.toml (the SANDBOX.md \
         schema) plus its source and tests. It runs under a default-deny macOS seatbelt profile \
         AUTO-GENERATED from the manifest, so it gets ONLY what the manifest declares.\n\n\
         Output a file bundle and NOTHING else. Each file is introduced by a line of the form:\n\
         === FILE: <relative/path> ===\n\
         followed by that file's exact contents, until the next such marker or end of output.\n\n\
         REQUIRED files for a Rust (runtime = \"binary\") app:\n\
         - manifest.toml\n\
         - Cargo.toml  (name = the app name; add `[workspace]` so it does not join an outer \
           workspace; `[[bin]]` name = the app name)\n\
         - src/main.rs (and any modules)\n\
         - at least one #[test] (unit tests in src or a tests/ integration test)\n\
         For a python (runtime = \"python\") app: manifest.toml, main.py, and a test_*.py — but \
         python apps are only py_compile-checked, so prefer a Rust app when tests matter.\n\n\
         MANIFEST + PERMISSION RULES (the validator REJECTS violations):\n\
         - [app].name: lowercase letters, digits and single hyphens only (a safe identifier), \
           and it MUST equal the app directory name.\n\
         - Request the MINIMAL permissions the goal truly needs. Default to NONE.\n\
         - You may NOT request device permissions: audio, gpu, camera, screen MUST all be false \
           (a forged app cannot be born with hardware/privacy access).\n\
         - fs_write: at most the app's own state dir, exactly \"state/apps/<name>\". Nothing else.\n\
         - fs_read: only paths inside the project (relative, no \"..\", no leading \"/\"). Prefer none.\n\
         - net_hosts: only the exact hostnames the app must reach (at most {MAX_NET_HOSTS}); prefer none.\n\
         - The app MUST build and pass `cargo test` (Rust) / py_compile (python) OFFLINE, with no \
           network and no new system dependencies.\n\n\
         Author the complete bundle now.",
    )
}

// ---------------------------------------------------------------------------
// (2) Parse the authored bundle — pure, unit-tested.
// ---------------------------------------------------------------------------

/// One authored file: a project-relative-ish path (relative to the app dir) and
/// its body. The path is validated for traversal before anything is written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuthoredFile {
    pub path: String,
    pub body: String,
}

/// The complete authored app, as parsed from the model bundle: a manifest body
/// plus the source/test files. Nothing here is trusted yet — staging validates
/// the manifest and the paths before any file is written, and validation
/// build+tests it.
#[derive(Debug, Clone, Serialize)]
pub struct AuthoredApp {
    /// The manifest.toml body verbatim (validated separately).
    pub manifest_toml: String,
    /// Every NON-manifest file the model authored, in document order.
    pub files: Vec<AuthoredFile>,
}

/// Split the model bundle on `=== FILE: <path> ===` markers into per-file
/// blocks, pulling the manifest out by name. Returns an error when no manifest
/// appears or when a marker has no path. Bodies keep their interior newlines; a
/// single trailing newline introduced by the marker split is trimmed.
pub fn parse_authored_app(raw: &str) -> Result<AuthoredApp> {
    let mut current_path: Option<String> = None;
    let mut current_body = String::new();
    let mut blocks: Vec<AuthoredFile> = Vec::new();

    let flush = |path: &mut Option<String>, body: &mut String, blocks: &mut Vec<AuthoredFile>| {
        if let Some(p) = path.take() {
            // Strip code fences the model may wrap a body in, and a single
            // leading/trailing blank line introduced by the marker layout.
            let cleaned = strip_fences(body);
            blocks.push(AuthoredFile { path: p, body: cleaned });
        }
        body.clear();
    };

    for line in raw.lines() {
        if let Some(path) = parse_file_marker(line) {
            flush(&mut current_path, &mut current_body, &mut blocks);
            current_path = Some(path);
            continue;
        }
        if current_path.is_some() {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    flush(&mut current_path, &mut current_body, &mut blocks);

    if blocks.is_empty() {
        bail!("authored bundle contained no `=== FILE: ... ===` markers");
    }

    // Pull the manifest out (matched by basename so apps/<name>/manifest.toml
    // or a bare manifest.toml both work).
    let manifest_idx = blocks
        .iter()
        .position(|f| basename(&f.path) == "manifest.toml")
        .ok_or_else(|| anyhow!("authored bundle has no manifest.toml"))?;
    let manifest = blocks.remove(manifest_idx);

    Ok(AuthoredApp {
        manifest_toml: manifest.body,
        files: blocks,
    })
}

/// `=== FILE: <path> ===` -> Some(path). Tolerant of surrounding whitespace and
/// an optional trailing ` ===`.
fn parse_file_marker(line: &str) -> Option<String> {
    let t = line.trim();
    let rest = t.strip_prefix("=== FILE:")?;
    let rest = rest.trim();
    let path = rest.strip_suffix("===").unwrap_or(rest).trim();
    (!path.is_empty()).then(|| path.to_string())
}

/// Drop a single wrapping ```lang ... ``` fence if the whole body is fenced,
/// and trim one leading + one trailing blank line. Conservative: only strips a
/// fence that opens on the first non-blank line and closes on the last.
fn strip_fences(body: &str) -> String {
    let trimmed = body.trim_matches('\n');
    let lines: Vec<&str> = trimmed.lines().collect();
    if lines.len() >= 2
        && lines[0].trim_start().starts_with("```")
        && lines[lines.len() - 1].trim() == "```"
    {
        return lines[1..lines.len() - 1].join("\n") + "\n";
    }
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// (3) Manifest + permission validation — pure, unit-tested. THE permission
// minimization gate.
// ---------------------------------------------------------------------------

/// A forged app's name must be a safe identifier: lowercase ASCII letters,
/// digits, and single internal hyphens, 2..=40 chars, starting with a letter.
/// This keeps it usable as a directory name, a socket name, and an SBPL/token
/// identity with no traversal or shell surprises.
fn is_safe_app_name(name: &str) -> bool {
    let n = name.as_bytes();
    if n.len() < 2 || n.len() > 40 {
        return false;
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return false;
    }
    if name.ends_with('-') {
        return false;
    }
    let mut prev_hyphen = false;
    for c in name.chars() {
        match c {
            'a'..='z' | '0'..='9' => prev_hyphen = false,
            '-' => {
                if prev_hyphen {
                    return false; // no "--"
                }
                prev_hyphen = true;
            }
            _ => return false,
        }
    }
    true
}

/// A relative path is project-confined when it is non-empty, not absolute, and
/// has no `..` component — so it can never escape the dir it is resolved under.
fn is_confined_relpath(p: &str) -> bool {
    let p = p.trim();
    if p.is_empty() || p.starts_with('/') {
        return false;
    }
    !Path::new(p)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir))
}

/// THE permission-minimization gate. Reject any forged manifest whose
/// permissions exceed what a freshly-authored, untrusted app may be born with:
///   - no device access at all (audio/gpu/camera/screen must be false);
///   - fs_write only to the app's OWN state dir ("state/apps/<name>");
///   - fs_read only to confined, in-project relative paths (no escapes);
///   - net_hosts capped, each a plausible hostname (no schemes, no slashes).
/// Returns Ok(()) on a minimal manifest, Err(reason) on an over-broad one.
fn validate_permissions(name: &str, p: &PermissionsSection) -> Result<()> {
    if p.audio {
        bail!("over-broad permission: a forged app may not request `audio` (microphone)");
    }
    if p.gpu {
        bail!("over-broad permission: a forged app may not request `gpu`");
    }
    if p.camera {
        bail!("over-broad permission: a forged app may not request `camera`");
    }
    if p.screen {
        bail!("over-broad permission: a forged app may not request `screen`");
    }

    let own_state = format!("state/apps/{name}");
    for w in &p.fs_write {
        let w = w.trim().trim_end_matches('/');
        if w != own_state {
            bail!(
                "over-broad permission: fs_write {:?} is outside the app's own state dir \
                 (only {:?} is allowed)",
                w,
                own_state
            );
        }
    }

    for r in &p.fs_read {
        if !is_confined_relpath(r) {
            bail!("over-broad permission: fs_read {:?} is not a confined in-project relative path", r);
        }
    }

    if p.net_hosts.len() > MAX_NET_HOSTS {
        bail!(
            "over-broad permission: net_hosts requests {} hosts (max {} for a forged app)",
            p.net_hosts.len(),
            MAX_NET_HOSTS
        );
    }
    for h in &p.net_hosts {
        let h = h.trim();
        if h.is_empty()
            || h.contains('/')
            || h.contains(':')
            || h.contains(' ')
            || h.contains("..")
        {
            bail!("over-broad permission: net_hosts entry {:?} is not a bare hostname", h);
        }
    }
    Ok(())
}

/// Parse + validate the manifest body against `dir_name`, then run the
/// permission-minimization gate, then prove a default-deny SBPL can be
/// generated from it (the born-sandboxed guarantee — if the profile cannot be
/// derived the app cannot be safely deployed). Returns the parsed manifest.
fn validate_manifest(manifest_toml: &str, dir_name: &str, project_root: &Path) -> Result<AppManifest> {
    if !is_safe_app_name(dir_name) {
        bail!("app name {:?} is not a safe identifier", dir_name);
    }
    // AppManifest::parse enforces the SANDBOX.md schema (deny_unknown_fields,
    // name == dir_name, non-empty version/entry).
    let manifest = AppManifest::parse(manifest_toml, dir_name)?;

    validate_permissions(dir_name, &manifest.permissions)?;

    // Born sandboxed: a default-deny SBPL must be derivable. We generate it with
    // representative absolute paths (the deploy-time runtime substitutes the
    // real interpreter + app dir); the point is that generation is total and the
    // profile opens with (deny default).
    let app_dir = project_root.join("apps").join(dir_name);
    let interp = match manifest.app.runtime {
        Runtime::Python => project_root.join(".venv/bin/python3"),
        Runtime::Node => PathBuf::from("/usr/bin/node"),
        Runtime::Binary => app_dir.join(&manifest.app.name),
    };
    let socket_path = project_root
        .join("state/ipc/apps")
        .join(format!("{dir_name}.sock"));
    let sbpl = apps::generate_sbpl(&manifest, project_root, &interp, &app_dir, &socket_path);
    if !sbpl.contains("(deny default)") {
        bail!("generated SBPL is not default-deny (refusing to propose)");
    }
    Ok(manifest)
}

/// Deploy-time re-validation gate, callable from the `jarvisd
/// --validate-forge-manifest <manifest_path> <app_name>` CLI dispatch.
///
/// scripts/apply_forge.sh calls THIS instead of re-implementing the
/// permission-minimization gate as a textual scan. The textual scan was a
/// TOML parser-differential: it only understood a literal `[permissions]`
/// header + scalar `key = true` / inline-or-multiline arrays under it, so a
/// hand-edited proposal using top-level dotted keys
/// (`permissions.fs_write = [...]`) or an inline table
/// (`permissions = { gpu = true }`) parsed clean on the daemon's `toml` crate
/// — yielding the over-broad grants the daemon honors at launch — yet slipped
/// past every text check. This entry point parses the manifest with the SAME
/// `toml` crate AppManifest::parse uses and runs the SAME
/// `validate_manifest` sequence (schema + name == dir + permission
/// minimization + default-deny-SBPL derivability), so the deploy gate can
/// never diverge from what the daemon would parse and grant.
///
/// `project_root` is used only to derive representative SBPL paths during the
/// born-sandboxed check; it does NOT need to be the live root (no app is
/// deployed here — this is a pure read+validate). Returns Ok(()) on a minimal
/// manifest and Err(reason) on a parse error or any over-broad grant.
pub fn validate_manifest_file(
    manifest_path: &Path,
    app_name: &str,
    project_root: &Path,
) -> Result<()> {
    let raw = std::fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest {}", manifest_path.display()))?;
    // SAME parse + minimization + SBPL-derivability gate the draft path runs.
    // A divergent parse (dotted key, inline table, deny_unknown_fields, etc.)
    // is now decided by the daemon's own toml crate, not a text scan.
    validate_manifest(&raw, app_name, project_root)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Pure pipeline helpers (gating, layout) — unit-tested.
// ---------------------------------------------------------------------------

/// What the enabled/mode pair permits. Unknown modes degrade to Propose — never
/// to Auto — so a typo can only make the forge safer. NOTE: even Auto NEVER
/// deploys into apps/; it only governs the forge's own staged artifact (and in
/// this core build, Auto behaves exactly like Propose — there is no separate
/// auto path that does anything more dangerous than write a proposal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForgeAction {
    Disabled,
    Propose,
    Auto,
}

fn forge_action(enabled: bool, mode: &str) -> ForgeAction {
    if !enabled {
        return ForgeAction::Disabled;
    }
    match mode.trim() {
        "auto" => ForgeAction::Auto,
        _ => ForgeAction::Propose, // "propose" and anything unknown
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The last `n` chars of `s` (validation output can be huge).
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

// ---------------------------------------------------------------------------
// Artifact rendering — pure, unit-tested.
// ---------------------------------------------------------------------------

/// report.md for a forge proposal: the goal, the app, its permissions, the
/// validation tail, and the EXACT deploy command.
fn render_report(
    ts: u64,
    model: &str,
    goal: &str,
    manifest: &AppManifest,
    files: &[AuthoredFile],
    validation_tail: &str,
) -> String {
    let p = &manifest.permissions;
    let perms = format!(
        "audio={}, gpu={}, camera={}, screen={}, net_hosts={:?}, fs_read={:?}, fs_write={:?}",
        p.audio, p.gpu, p.camera, p.screen, p.net_hosts, p.fs_read, p.fs_write
    );
    let file_list = files
        .iter()
        .map(|f| format!("- {}", f.path))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# Self-Forge proposal — {ts}\n\n\
         - verdict: VALIDATED (manifest minimal, default-deny SBPL generates, build + tests passed in staging)\n\
         - model: {model}\n\
         - app: {name} (v{version}, runtime {runtime:?})\n\
         - description: {desc}\n\
         - permissions (MINIMAL, validated): {perms}\n\n\
         ## Goal\n\n{goal}\n\n\
         ## Authored files\n\n{manifest_line}\n{file_list}\n\n\
         ## Validation output (tail)\n\n```\n{validation_tail}\n```\n\n\
         ## To deploy\n\n\
         This app was validated in a CONFINED staging copy only; it is NOT in apps/, NOT \
         registered, and NOT running. Review the manifest and source above, then deploy it with:\n\n\
         ```\nscripts/apply_forge.sh {ts}\n```\n\n\
         apply_forge.sh re-validates the manifest + permissions, re-runs the build/tests, and ONLY \
         then moves the app into apps/<name>/ so AppRegistry::discover picks it up on the next \
         start. There is no daemon code path that deploys it automatically.\n",
        name = manifest.app.name,
        version = manifest.app.version,
        runtime = manifest.app.runtime,
        desc = manifest.app.description,
        manifest_line = "- manifest.toml",
    )
}

/// A short report.md for a rejected attempt (manifest/permission/validation
/// failure), for the quarantine dir.
fn render_rejection_report(ts: u64, model: &str, goal: &str, stage: &str, reason: &str) -> String {
    format!(
        "# Self-Forge REJECTED — {ts}\n\n\
         - verdict: REJECTED (did not pass the {stage} gate)\n\
         - model: {model}\n\n\
         ## Goal\n\n{goal}\n\n\
         ## Why it was rejected\n\n{reason}\n\n\
         The draft is quarantined under state/forge/rejected/{ts}/ for audit. Nothing was deployed.\n",
    )
}

// ---------------------------------------------------------------------------
// (3)+(4) Stage + validate — impure, exercised hermetically against a PLANTED
// app spec (a mock-brain bundle), NEVER a real cloud draft and NEVER apps/.
// ---------------------------------------------------------------------------

/// How one forge attempt ended.
#[derive(Debug)]
enum AttemptResult {
    /// Validated: a minimal, born-sandboxed, building+passing app. Ready to
    /// PROPOSE (write under state/forge/proposals/<ts>/). The daemon still does
    /// NOT deploy it — that is the human apply step.
    Proposed {
        manifest: AppManifest,
        files: Vec<AuthoredFile>,
        manifest_toml: String,
        report: String,
    },
    /// A model/manifest/permission/build/test failure — quarantined.
    Rejected { stage: &'static str, report: String },
    /// Infra trouble before any verdict (draft call failed). No statement about
    /// the app.
    Aborted { stage: &'static str },
}

/// Output of one child process: combined stdout+stderr and its success bit.
struct CmdOutput {
    ok: bool,
    output: String,
}

/// The full attempt, factored out so the drill reuses it verbatim. `forge_root`
/// is where staging/proposals/rejected dirs go; `project_root` is the project
/// root used only to derive the SBPL (NEVER written to). NEVER writes into
/// apps/. The app name comes from the authored manifest.
async fn run_attempt(
    project_root: &Path,
    forge_root: &Path,
    ts: u64,
    model: &str,
    brain: &dyn ForgeBrain,
    goal: &str,
) -> AttemptResult {
    telemetry::emit("system", "forge.drafting", json!({"ts": ts, "goal": goal}));

    // (2) Draft.
    let raw = match brain.author(goal).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "forge: draft call failed");
            return AttemptResult::Aborted { stage: "draft" };
        }
    };
    let authored = match parse_authored_app(&raw) {
        Ok(a) => a,
        Err(e) => {
            let report = render_rejection_report(ts, model, goal, "parse", &e.to_string());
            return AttemptResult::Rejected { stage: "parse", report };
        }
    };

    // The app name is the manifest's [app].name; derive the dir name from it.
    let name = match parse_manifest_name(&authored.manifest_toml) {
        Some(n) => n,
        None => {
            let report =
                render_rejection_report(ts, model, goal, "manifest", "manifest has no [app].name");
            return AttemptResult::Rejected { stage: "manifest", report };
        }
    };

    // (3) Manifest + permission-minimization + SBPL-derivable gate.
    let manifest = match validate_manifest(&authored.manifest_toml, &name, project_root) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(error = %e, app = %name, "forge: manifest rejected");
            let report = render_rejection_report(ts, model, goal, "manifest", &e.to_string());
            return AttemptResult::Rejected { stage: "manifest", report };
        }
    };

    // Author file paths must be confined (no traversal) before anything is
    // written into the staging dir.
    for f in &authored.files {
        if !is_confined_relpath(&f.path) {
            let report = render_rejection_report(
                ts,
                model,
                goal,
                "manifest",
                &format!("authored file path {:?} is not confined (traversal)", f.path),
            );
            return AttemptResult::Rejected { stage: "manifest", report };
        }
        if f.body.len() > MAX_AUTHORED_FILE_BYTES {
            let report = render_rejection_report(
                ts,
                model,
                goal,
                "manifest",
                &format!("authored file {:?} exceeds the size cap", f.path),
            );
            return AttemptResult::Rejected { stage: "manifest", report };
        }
    }

    // (4) Stage the files into a CONFINED staging dir (NEVER apps/).
    let staging = forge_root.join(format!("staging-{ts}"));
    if let Err(e) = stage_files(&staging, &authored) {
        tracing::warn!(error = %e, "forge: staging infrastructure failed");
        return AttemptResult::Aborted { stage: "stage" };
    }

    // (4) Validate the staged app (build + tests, timeout-capped).
    let validation = match validate_staged(&staging, &manifest).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "forge: validation infrastructure failed");
            return AttemptResult::Aborted { stage: "validate" };
        }
    };
    let validation_tail = match validation {
        ValidationOutcome::Passed { tail } => tail,
        ValidationOutcome::Failed { stage, detail } => {
            let report = render_rejection_report(
                ts,
                model,
                goal,
                stage,
                &format!("staged validation failed at {stage}:\n{}", tail_chars(&detail, 1500)),
            );
            return AttemptResult::Rejected { stage, report };
        }
    };

    let report = render_report(ts, model, goal, &manifest, &authored.files, &validation_tail);
    AttemptResult::Proposed {
        manifest,
        files: authored.files,
        manifest_toml: authored.manifest_toml,
        report,
    }
}

/// Pull `[app].name` out of a manifest body without full validation (so a
/// missing/garbled manifest still produces a clean rejection rather than a
/// panic). Uses the TOML parser, falling back to None.
fn parse_manifest_name(manifest_toml: &str) -> Option<String> {
    let table: toml::Table = manifest_toml.parse().ok()?;
    let name = table.get("app")?.as_table()?.get("name")?.as_str()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Write the authored files (manifest + sources) into the staging dir. Confined
/// paths only (the caller already rejected traversal). Creates parent dirs.
fn stage_files(staging: &Path, app: &AuthoredApp) -> Result<()> {
    if staging.exists() {
        std::fs::remove_dir_all(staging)?;
    }
    std::fs::create_dir_all(staging)?;
    write_confined(staging, "manifest.toml", &app.manifest_toml)?;
    for f in &app.files {
        write_confined(staging, &f.path, &f.body)?;
    }
    Ok(())
}

/// Write `<base>/<rel>` after a final defense-in-depth confinement check (the
/// written path must canonicalize/normalize to within `base`). `rel` is already
/// validated by the caller; this is belt-and-braces.
fn write_confined(base: &Path, rel: &str, body: &str) -> Result<()> {
    if !is_confined_relpath(rel) {
        bail!("refusing to write non-confined path {:?}", rel);
    }
    let dest = base.join(rel);
    // The joined path must still start with base (no symlink/.. trickery).
    if !dest.starts_with(base) {
        bail!("path {:?} escapes the staging dir", rel);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest, body)?;
    Ok(())
}

/// The outcome of staged validation.
enum ValidationOutcome {
    Passed { tail: String },
    Failed { stage: &'static str, detail: String },
}

/// Build + test the staged app under a TIMEOUT cap, per runtime. Rust: cargo
/// check && cargo test. Python: py_compile every .py. Binary apps without a
/// Cargo.toml cannot be built from source and are rejected (a forged app must
/// be buildable+testable from its authored sources). NEVER runs the app's own
/// process beyond the build/test harness; the build/test runs in the confined
/// staging dir with kill_on_drop + a deadline.
async fn validate_staged(staging: &Path, manifest: &AppManifest) -> Result<ValidationOutcome> {
    // Manifest + SBPL sanity were already proven in validate_manifest. Here we
    // build + test the authored sources.
    match manifest.app.runtime {
        Runtime::Binary | Runtime::Node => {
            // A from-source forged app is a cargo crate (Node is accepted by the
            // manifest schema but we cannot author+test a node app hermetically
            // here, so require a Cargo.toml — i.e. a Rust binary crate).
            if !staging.join("Cargo.toml").exists() {
                return Ok(ValidationOutcome::Failed {
                    stage: "build",
                    detail: "no Cargo.toml in the staged app; a forged app must build + test from \
                             its authored Rust sources"
                        .to_string(),
                });
            }
            validate_cargo(staging).await
        }
        Runtime::Python => validate_python(staging).await,
    }
}

/// cargo check && cargo test in the staging dir under one deadline.
async fn validate_cargo(staging: &Path) -> Result<ValidationOutcome> {
    let deadline = tokio::time::Instant::now() + VALIDATE_TIMEOUT;
    let mut combined = String::new();
    for (stage, args) in [("check", ["check"]), ("test", ["test"])] {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(ValidationOutcome::Failed {
                stage,
                detail: format!("{combined}\n[validation deadline exhausted before cargo {stage}]"),
            });
        }
        match run_cargo(staging, &args, remaining).await {
            Ok(out) => {
                combined.push_str(&format!("\n$ cargo {stage}\n"));
                combined.push_str(&out.output);
                if !out.ok {
                    return Ok(ValidationOutcome::Failed { stage, detail: combined });
                }
            }
            Err(e) => {
                return Ok(ValidationOutcome::Failed {
                    stage,
                    detail: format!("{combined}\n[cargo {stage} failed to run: {e}]"),
                })
            }
        }
    }
    Ok(ValidationOutcome::Passed {
        tail: tail_chars(&combined, REPORT_TAIL_CHARS),
    })
}

/// py_compile every .py under the staging dir (recursively) under the deadline.
/// A python app's "tests" are at least syntactically validated; the daemon
/// never executes the python (that would run generated code live).
async fn validate_python(staging: &Path) -> Result<ValidationOutcome> {
    let mut py_files: Vec<PathBuf> = Vec::new();
    collect_py(staging, &mut py_files);
    if py_files.is_empty() {
        return Ok(ValidationOutcome::Failed {
            stage: "build",
            detail: "no .py files in the staged python app".to_string(),
        });
    }
    let mut combined = String::new();
    for f in &py_files {
        let arg = f.to_string_lossy().to_string();
        match run_python_compile(staging, &arg).await {
            Ok(out) => {
                combined.push_str(&format!("\n$ python -m py_compile {arg}\n"));
                combined.push_str(&out.output);
                if !out.ok {
                    return Ok(ValidationOutcome::Failed { stage: "build", detail: combined });
                }
            }
            Err(e) => {
                return Ok(ValidationOutcome::Failed {
                    stage: "build",
                    detail: format!("{combined}\n[py_compile failed to run: {e}]"),
                })
            }
        }
    }
    Ok(ValidationOutcome::Passed {
        tail: tail_chars(&combined, REPORT_TAIL_CHARS),
    })
}

fn collect_py(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_py(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            out.push(path);
        }
    }
}

/// `cargo <args>` in `dir`, output captured, bounded by `timeout`. Uses the
/// $CARGO that invoked us when set (tests run under cargo) else PATH lookup.
async fn run_cargo(dir: &Path, args: &[&str], timeout: Duration) -> Result<CmdOutput> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    run_capture(&cargo, args, dir, timeout).await
}

/// `python3 -m py_compile <file>` in `dir`, bounded by a fixed short timeout.
async fn run_python_compile(dir: &Path, file: &str) -> Result<CmdOutput> {
    run_capture("python3", &["-m", "py_compile", file], dir, Duration::from_secs(60)).await
}

/// Spawn `cmd args` in `dir`, capture combined stdout+stderr, bounded by
/// `timeout`, stdin null, kill_on_drop.
async fn run_capture(cmd: &str, args: &[&str], dir: &Path, timeout: Duration) -> Result<CmdOutput> {
    let child = tokio::process::Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => bail!("{cmd} {} timed out after {}s", args.join(" "), timeout.as_secs()),
    };
    Ok(CmdOutput {
        ok: out.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    })
}

// ---------------------------------------------------------------------------
// (5) Propose / quarantine — impure. NEVER touches apps/.
// ---------------------------------------------------------------------------

/// Write `<dir_root>/<ts>/<name>` with `body`. Returns the file's directory on
/// success.
fn record_artifact(dir_root: &Path, ts: u64, name: &str, body: &str) -> Option<PathBuf> {
    let dir = dir_root.join(ts.to_string());
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(name), body)?;
        Ok(())
    };
    match write() {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(error = %e, dir = %dir.display(), name, "forge: failed to write artifact");
            None
        }
    }
}

/// Write the validated app under `<forge_root>/proposals/<ts>/app/<name>/` with
/// the manifest + every authored file, plus report.md at the proposal root.
/// Returns the proposal dir. Does NOT touch apps/ and does NOT register or run
/// anything. The app layout under app/<name>/ is exactly what apply_forge.sh
/// moves into apps/<name>/.
fn write_proposal(
    forge_root: &Path,
    ts: u64,
    manifest: &AppManifest,
    manifest_toml: &str,
    files: &[AuthoredFile],
    report: &str,
) -> Result<PathBuf> {
    let proposal = forge_root.join("proposals").join(ts.to_string());
    let app_dir = proposal.join("app").join(manifest.app.name.as_str());
    std::fs::create_dir_all(&app_dir)?;
    write_confined(&app_dir, "manifest.toml", manifest_toml)?;
    for f in files {
        write_confined(&app_dir, &f.path, &f.body)?;
    }
    std::fs::write(proposal.join("report.md"), report)?;
    std::fs::write(
        proposal.join("manifest.toml"),
        manifest_toml, // a copy at the proposal root for quick review
    )?;
    Ok(proposal)
}

// ---------------------------------------------------------------------------
// Public entry: forge_draft(goal) — the gated pipeline. Returns the proposal
// dir on a validated proposal, or a typed outcome explaining why not.
// ---------------------------------------------------------------------------

/// The outcome the public entry reports (mirrors heal's telemetry verbs).
#[derive(Debug)]
pub enum ForgeOutcome {
    /// Gate off ([forge].enabled = false): nothing happened.
    Disabled,
    /// No cloud key resolved: nothing drafted.
    Blocked,
    /// A validated app was PROPOSED at this dir (NOT deployed).
    Proposed { dir: PathBuf },
    /// The draft was rejected at `stage` and quarantined under `dir`.
    Rejected { stage: &'static str, dir: PathBuf },
    /// Infra trouble before any verdict (draft/stage/validate failed).
    Aborted { stage: &'static str },
}

/// Forge a new micro-app from `goal`, fully gated. PROPOSE-ONLY: on success the
/// validated app lands under state/forge/proposals/<ts>/ and meta.forge_pending
/// is stamped; the app is NOT in apps/, NOT registered, NOT running. A human
/// deploys it with scripts/apply_forge.sh <ts>. This function NEVER writes into
/// apps/ and NEVER runs generated code beyond the confined staging build/test.
///
/// `set_pending` is the hook the daemon uses to stamp meta.forge_pending (it
/// owns Memory); the drill passes a no-op. Returns the outcome.
pub async fn forge_draft(
    project_root: &Path,
    enabled: bool,
    mode: &str,
    model: &str,
    brain: &dyn ForgeBrain,
    goal: &str,
    set_pending: impl Fn(u64),
) -> ForgeOutcome {
    let action = forge_action(enabled, mode);
    if action == ForgeAction::Disabled {
        // No draft, no stage, no propose — the whole pipeline is inert.
        telemetry::emit("system", "forge.suppressed", json!({"reason": "forge.enabled = false"}));
        return ForgeOutcome::Disabled;
    }

    // Drafting needs the cloud: no key, no pipeline (friendly blocked outcome,
    // exactly like heal).
    if anthropic::resolve_api_key().await.is_none() {
        telemetry::emit("system", "forge.blocked", json!({"reason": "no_api_key"}));
        return ForgeOutcome::Blocked;
    }

    let ts = now_secs();
    let forge_root = project_root.join("state").join("forge");

    match run_attempt(project_root, &forge_root, ts, model, brain, goal).await {
        AttemptResult::Proposed {
            manifest,
            files,
            manifest_toml,
            report,
            ..
        } => {
            // (5) PROPOSE — write under state/forge/proposals/<ts>/, stamp the
            // pending marker, STOP. mode "auto" reaches HERE too: there is NO
            // separate, more-dangerous auto path — DEPLOY is always the human
            // apply step, so Auto and Propose both end at a written proposal.
            let _ = action; // Auto == Propose for the deploy decision (by design)
            match write_proposal(&forge_root, ts, &manifest, &manifest_toml, &files, &report) {
                Ok(dir) => {
                    set_pending(ts);
                    telemetry::emit(
                        "system",
                        "forge.proposal",
                        json!({"ts": ts, "app": manifest.app.name, "validated": true}),
                    );
                    tracing::info!(
                        ts,
                        app = %manifest.app.name,
                        "forge: validated app proposed; deploy with scripts/apply_forge.sh"
                    );
                    ForgeOutcome::Proposed { dir }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "forge: failed to write proposal");
                    ForgeOutcome::Aborted { stage: "propose" }
                }
            }
        }
        AttemptResult::Rejected { stage, report } => {
            let dir_root = forge_root.join("rejected");
            let dir = record_artifact(&dir_root, ts, "report.md", &report)
                .unwrap_or_else(|| dir_root.join(ts.to_string()));
            telemetry::emit("system", "forge.rejected", json!({"ts": ts, "stage": stage}));
            tracing::warn!(stage, ts, "forge: draft rejected and quarantined");
            ForgeOutcome::Rejected { stage, dir }
        }
        AttemptResult::Aborted { stage } => {
            telemetry::emit("system", "forge.aborted", json!({"ts": ts, "stage": stage}));
            ForgeOutcome::Aborted { stage }
        }
    }
}

/// Daemon-facing production entry: forge a micro-app from a spoken/typed `goal`,
/// reading the gate from `[forge]` config and stamping `meta.forge_pending`
/// through Memory on a successful PROPOSAL. This is the production call site
/// (e.g. a "build me an app that ..." command); it owns the CloudBrain and the
/// pending marker so the rest of the pipeline stays Memory-free + unit-testable.
///
/// PROPOSE-ONLY and gated, exactly like [self_heal]: when [forge].enabled is
/// false this is inert; on success it writes a proposal under
/// state/forge/proposals/<ts>/ and stamps meta.forge_pending so the first-
/// contact brief can tell the user a forged app is awaiting review. It NEVER
/// deploys — the human runs scripts/apply_forge.sh.
pub async fn forge_app(root: &Path, cfg: &Config, memory: &Memory, goal: &str) -> ForgeOutcome {
    let brain = CloudBrain {
        model: cfg.cloud.heavy_model.clone(),
    };
    // Capture the proposal ts from the (synchronous) pending hook so the async
    // meta.forge_pending upsert can run AFTER forge_draft returns (Memory's
    // Mutex<Connection> is not Clone/Send-into-a-closure-friendly, and the
    // proposal artifact on disk is the source of truth regardless).
    let pending = std::cell::Cell::new(None::<u64>);
    // LOCKDOWN OVERLAY (task #12): self-forge is autonomy, so it is FORCED off
    // while the emergency stop is engaged — the enabled bit is ANDed with
    // `!is_locked_down()`, so `forge_draft`/`forge_action` see Disabled and the
    // pipeline is inert (no cloud authoring, no proposal). `forge_action` stays
    // pure (the global read lives here, at the live entry). With lockdown OFF
    // this is byte-for-byte the configured `[forge].enabled`.
    let enabled = cfg.forge.enabled && !crate::lockdown::is_locked_down();
    let outcome = forge_draft(
        root,
        enabled,
        &cfg.forge.mode,
        &cfg.cloud.heavy_model,
        &brain,
        goal,
        |ts| pending.set(Some(ts)),
    )
    .await;
    if let Some(ts) = pending.get() {
        if let Err(e) = memory.upsert_fact(META_FORGE_PENDING, &ts.to_string()).await {
            tracing::warn!(error = %e, "forge: proposal written but meta.forge_pending stamp failed");
        }
    }
    outcome
}

// ---------------------------------------------------------------------------
// FORGE DRILL — the ONE real cloud path, invoked by the verifier via
// `jarvisd --forge-drill`. It runs the FULL real pipeline (draft -> stage ->
// validate -> propose) against a FIXED benign goal in a throwaway temp root. It
// NEVER touches the real apps/ and NEVER deploys.
// ---------------------------------------------------------------------------

/// A small, safe, fully-offline goal that a competent model can satisfy with a
/// zero-permission Rust app — so the drill exercises the whole loop without
/// needing any grant.
const DRILL_GOAL: &str = "A tiny offline utility micro-app that reverses an ASCII string. \
     Zero permissions (no network, no filesystem writes beyond its own state dir, no devices). \
     Include unit tests for the reverse function.";

/// Run the full real forge pipeline against a FIXED benign goal in a temp root,
/// drafting via the REAL cloud (CloudBrain). Requires the Anthropic key. Writes
/// a real proposal artifact under `<tmp>/state/forge/proposals/<ts>/`. Returns
/// the proposal dir on success. The real apps/ is never touched, nothing is
/// deployed, nothing is run beyond the confined staging build/test.
///
/// Invoked by `jarvisd --forge-drill` (see main.rs); the model id is the
/// configured heavy model so the drill exercises exactly the production path.
pub async fn run_forge_drill(model: &str) -> Result<PathBuf> {
    if anthropic::resolve_api_key().await.is_none() {
        bail!("forge drill requires an Anthropic API key (none resolved)");
    }
    telemetry::init(); // safe if already initialized (OnceLock no-op)

    let ts = now_secs();
    let sandbox = std::env::temp_dir().join(format!("jarvis-forge-drill-{}-{ts}", std::process::id()));
    // A real .venv is not present in the sandbox; the drill goal yields a Rust
    // app, so the python interpreter path is never exercised.
    std::fs::create_dir_all(sandbox.join("apps"))?;
    let forge_root = sandbox.join("state").join("forge");

    tracing::info!(
        sandbox = %sandbox.display(),
        model,
        "forge drill: running the FULL pipeline against a benign goal (cloud)"
    );

    let brain = CloudBrain { model: model.to_string() };
    let result = run_attempt(&sandbox, &forge_root, ts, model, &brain, DRILL_GOAL).await;

    // SAFETY: the real apps/ under the project root must be untouched (the drill
    // works in a temp sandbox), and nothing may have been written into the
    // sandbox's apps/ dir either (propose-only never deploys).
    let sandbox_apps = sandbox.join("apps");
    if std::fs::read_dir(&sandbox_apps).map(|mut d| d.next().is_some()).unwrap_or(false) {
        bail!("forge drill SAFETY VIOLATION: something was written into apps/ (deploy must be a human step)");
    }

    match result {
        AttemptResult::Proposed {
            manifest,
            files,
            manifest_toml,
            report,
            ..
        } => {
            let dir = write_proposal(&forge_root, ts, &manifest, &manifest_toml, &files, &report)?;
            telemetry::emit(
                "system",
                "forge.proposal",
                json!({"ts": ts, "app": manifest.app.name, "validated": true, "drill": true}),
            );
            tracing::info!(
                proposal = %dir.display(),
                app = %manifest.app.name,
                "forge drill: PASSED — full pipeline produced a validated proposal"
            );
            Ok(dir)
        }
        AttemptResult::Rejected { stage, .. } => {
            bail!("forge drill: pipeline rejected the draft at stage `{stage}`")
        }
        AttemptResult::Aborted { stage } => {
            bail!("forge drill: pipeline aborted at stage `{stage}` (cloud/infra failure)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- (1) gating ----------------------------------------------------------

    /// The forge gate degrades only toward the safer Propose; "auto" requires
    /// enabled = true; and — by construction — there is no path where Auto
    /// deploys (see no_auto_deploy_path_exists). UNCHANGED contract vs heal.
    #[test]
    fn forge_action_gating_truth_table() {
        assert_eq!(forge_action(false, "propose"), ForgeAction::Disabled);
        assert_eq!(forge_action(false, "auto"), ForgeAction::Disabled);
        assert_eq!(forge_action(false, ""), ForgeAction::Disabled);
        assert_eq!(forge_action(true, "propose"), ForgeAction::Propose);
        assert_eq!(forge_action(true, "auto"), ForgeAction::Auto);
        assert_eq!(forge_action(true, " auto "), ForgeAction::Auto);
        assert_eq!(forge_action(true, ""), ForgeAction::Propose);
        assert_eq!(forge_action(true, "AUTO"), ForgeAction::Propose, "no case games");
        assert_eq!(forge_action(true, "yolo"), ForgeAction::Propose);
    }

    /// SOURCE-LEVEL no-auto-deploy proof. The daemon must NEVER build a path
    /// under the project `apps/` dir and write to it — deploy is ONLY the human
    /// scripts/apply_forge.sh. forge.rs constructs an `apps/<name>` path in
    /// EXACTLY ONE place, and only to DERIVE the SBPL (a read-only argument to
    /// apps::generate_sbpl); it must never be handed to a filesystem-write API.
    /// This test pins both halves: there is one `.join("apps")`, and no line
    /// that both joins "apps" and calls a write helper. A future edit that adds
    /// an apps/ write must update this test, forcing a conscious decision.
    #[test]
    fn no_auto_deploy_path_exists() {
        let src = include_str!("forge.rs");

        // The LIVE project apps/ dir is only ever derived from `project_root`.
        // `project_root.join("apps")` appears EXACTLY ONCE — the read-only SBPL
        // app_dir derivation in validate_manifest — and that line must not call
        // a filesystem-write API. (The drill's throwaway temp sandbox and the
        // tests use `sandbox.join("apps")` / `root.0.join("apps")`; those are NOT
        // the live project apps/ and are exercised to PROVE emptiness.)
        let live_apps_lines: Vec<&str> = src
            .lines()
            .filter(|l| {
                let code = l.trim_start();
                !code.starts_with("//") && code.contains("project_root.join(\"apps\")")
            })
            .collect();
        assert_eq!(
            live_apps_lines.len(),
            1,
            "expected exactly ONE live-apps path construction (the read-only SBPL \
             derivation); found: {live_apps_lines:?}"
        );
        let line = live_apps_lines[0];
        assert!(
            line.contains("app_dir") && !writes_to(line),
            "the sole live-apps path must be the SBPL app_dir derivation, not a write: {line:?}"
        );

        // No NON-comment code line both names a `project_root` apps/ path AND
        // calls a filesystem-write API. Every real write in forge.rs targets
        // state/forge/, never the live apps/.
        for l in src.lines() {
            let code = l.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if code.contains("project_root.join(\"apps\")") && writes_to(code) {
                panic!("a forge.rs code line writes to the live apps/ dir: {l:?}");
            }
        }

        // The report must point the human at the apply script (the only deploy
        // path), never an automatic one.
        assert!(
            src.contains("scripts/apply_forge.sh"),
            "the report must point the human at the apply script"
        );
    }

    /// True when a source line invokes a filesystem-write API.
    fn writes_to(line: &str) -> bool {
        ["std::fs::write", "std::fs::copy", "std::fs::rename", "create_dir_all", "write_confined"]
            .iter()
            .any(|w| line.contains(w))
    }

    // -- (2) bundle parsing --------------------------------------------------

    #[test]
    fn parse_file_marker_handles_variants() {
        assert_eq!(parse_file_marker("=== FILE: src/main.rs ==="), Some("src/main.rs".into()));
        assert_eq!(parse_file_marker("  === FILE: manifest.toml ==="), Some("manifest.toml".into()));
        assert_eq!(parse_file_marker("=== FILE: Cargo.toml"), Some("Cargo.toml".into()));
        assert_eq!(parse_file_marker("not a marker"), None);
        assert_eq!(parse_file_marker("=== FILE:  ==="), None);
    }

    #[test]
    fn strip_fences_unwraps_a_whole_body_fence() {
        let body = "```rust\nfn main() {}\n```";
        assert_eq!(strip_fences(body), "fn main() {}\n");
        // Unfenced bodies are returned with a single trailing newline.
        assert_eq!(strip_fences("x = 1"), "x = 1\n");
        assert_eq!(strip_fences("\n\n"), "");
    }

    #[test]
    fn parse_authored_app_splits_manifest_and_files() {
        let raw = "Here is the app.\n\
            === FILE: manifest.toml ===\n[app]\nname = \"x\"\n\
            === FILE: Cargo.toml ===\n[package]\nname = \"x\"\n\
            === FILE: src/main.rs ===\nfn main() {}\n";
        let app = parse_authored_app(raw).unwrap();
        assert!(app.manifest_toml.contains("name = \"x\""));
        assert_eq!(app.files.len(), 2, "manifest pulled out, two files remain");
        assert_eq!(app.files[0].path, "Cargo.toml");
        assert_eq!(app.files[1].path, "src/main.rs");
        assert!(app.files[1].body.contains("fn main()"));
    }

    #[test]
    fn parse_authored_app_rejects_no_manifest_and_no_markers() {
        assert!(parse_authored_app("just prose, no markers").is_err());
        let no_manifest = "=== FILE: src/main.rs ===\nfn main() {}\n";
        assert!(parse_authored_app(no_manifest).is_err(), "no manifest -> error");
    }

    // -- (3) name + path + PERMISSION minimization ---------------------------

    #[test]
    fn safe_app_name_accepts_identifiers_and_rejects_the_rest() {
        for ok in ["global-scan", "ab", "tool42", "a-b-c"] {
            assert!(is_safe_app_name(ok), "{ok:?} should be valid");
        }
        for bad in ["", "a", "-lead", "trail-", "Up", "two__under", "a--b", "x/y", "..", "a.b", "név"] {
            assert!(!is_safe_app_name(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn confined_relpath_rejects_traversal_and_absolute() {
        assert!(is_confined_relpath("src/main.rs"));
        assert!(is_confined_relpath("manifest.toml"));
        assert!(!is_confined_relpath("/etc/passwd"));
        assert!(!is_confined_relpath("../secret"));
        assert!(!is_confined_relpath("a/../../b"));
        assert!(!is_confined_relpath(""));
    }

    fn perms(f: impl FnOnce(&mut PermissionsSection)) -> PermissionsSection {
        let mut p = PermissionsSection::default();
        f(&mut p);
        p
    }

    #[test]
    fn permission_minimization_accepts_minimal_and_rejects_overbroad() {
        // Minimal (all-default, zero permissions) is accepted.
        assert!(validate_permissions("tool", &PermissionsSection::default()).is_ok());
        // The app's own state dir as the only write target is accepted.
        assert!(validate_permissions("tool", &perms(|p| p.fs_write = vec!["state/apps/tool".into()])).is_ok());
        // A small confined read + a couple of hosts is accepted.
        assert!(validate_permissions(
            "tool",
            &perms(|p| {
                p.fs_read = vec!["apps/tool/data".into()];
                p.net_hosts = vec!["example.com".into(), "api.example.org".into()];
            })
        )
        .is_ok());

        // Device permissions are ALWAYS rejected for a forged app.
        assert!(validate_permissions("tool", &perms(|p| p.audio = true)).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.gpu = true)).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.camera = true)).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.screen = true)).is_err());

        // fs_write outside the app's own state dir -> rejected.
        assert!(validate_permissions("tool", &perms(|p| p.fs_write = vec!["state/apps/other".into()])).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.fs_write = vec!["state".into()])).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.fs_write = vec!["/tmp".into()])).is_err());

        // Escaping reads -> rejected.
        assert!(validate_permissions("tool", &perms(|p| p.fs_read = vec!["../../etc".into()])).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.fs_read = vec!["/etc/passwd".into()])).is_err());

        // Too many hosts / malformed host -> rejected.
        let many: Vec<String> = (0..MAX_NET_HOSTS + 1).map(|i| format!("h{i}.example.com")).collect();
        assert!(validate_permissions("tool", &perms(|p| p.net_hosts = many)).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.net_hosts = vec!["http://x.com".into()])).is_err());
        assert!(validate_permissions("tool", &perms(|p| p.net_hosts = vec!["x.com:443".into()])).is_err());
    }

    #[test]
    fn validate_manifest_accepts_minimal_and_proves_default_deny_sbpl() {
        let root = std::env::temp_dir();
        let manifest = "[app]\nname = \"reverser\"\nversion = \"0.1.0\"\n\
            description = \"reverse a string\"\nentry = \"reverser\"\nruntime = \"binary\"\n\
            [permissions]\n";
        let m = validate_manifest(manifest, "reverser", &root).expect("minimal manifest must validate");
        assert_eq!(m.app.name, "reverser");
    }

    #[test]
    fn validate_manifest_rejects_overbroad_and_name_mismatch() {
        let root = std::env::temp_dir();
        // gpu = true is over-broad.
        let overbroad = "[app]\nname = \"x\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
            entry = \"x\"\nruntime = \"binary\"\n[permissions]\ngpu = true\n";
        // name "x" is too short for a safe identifier, caught first.
        assert!(validate_manifest(overbroad, "x", &root).is_err());

        let overbroad2 = "[app]\nname = \"toolx\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
            entry = \"toolx\"\nruntime = \"binary\"\n[permissions]\ngpu = true\n";
        let err = validate_manifest(overbroad2, "toolx", &root).unwrap_err().to_string();
        assert!(err.contains("gpu"), "gpu must be rejected as over-broad: {err}");

        // name != dir_name is rejected by AppManifest::parse.
        let mismatch = "[app]\nname = \"toolx\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
            entry = \"toolx\"\nruntime = \"binary\"\n[permissions]\n";
        assert!(validate_manifest(mismatch, "different", &root).is_err());
    }

    // -- deploy-time gate: the parser-differential the textual scan missed ----
    //
    // scripts/apply_forge.sh used to re-implement the permission gate as a TEXT
    // scan that only understood a literal `[permissions]` header + `key = true`
    // / `key = [..]` lines under it. A hand-edited proposal using top-level
    // DOTTED keys (`permissions.gpu = true`) or an INLINE TABLE
    // (`permissions = { gpu = true }`) parses CLEAN on the daemon's toml crate —
    // the daemon honors the over-broad grant at launch — yet slid past every
    // text check. validate_manifest_file (the CLI gate the script now calls)
    // parses with that SAME toml crate, so both forms are caught.
    #[test]
    fn validate_manifest_file_catches_toml_parser_differential() {
        let dir = std::env::temp_dir().join(format!("jarvis-forge-gate-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let root = std::env::temp_dir();
        let write = |body: &str| {
            let p = dir.join("manifest.toml");
            std::fs::write(&p, body).unwrap();
            p
        };

        // (a) TOP-LEVEL DOTTED keys before [app]: the text scan saw no
        // `[permissions]` header and no `gpu = true` line, so it passed. The
        // daemon parses gpu=true + escaping grants. validate_manifest_file MUST
        // reject it.
        let dotted = "permissions.fs_write = [\"state/apps/evil\", \"/Users/x\"]\n\
            permissions.net_hosts = [\"a.com\",\"b.com\",\"c.com\",\"d.com\",\"e.com\",\"f.com\",\"g.com\"]\n\
            permissions.fs_read = [\"/Users/x/.ssh\"]\n\
            permissions.gpu = true\n\n\
            [app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
            entry = \"evil\"\nruntime = \"binary\"\n";
        let p = write(dotted);
        let err = validate_manifest_file(&p, "evil", &root)
            .expect_err("dotted-key over-broad manifest must be rejected")
            .to_string();
        assert!(err.contains("gpu"), "dotted-key gpu must be rejected: {err}");

        // (b) INLINE TABLE: `permissions = { gpu = true, ... }`. Same — the text
        // scan never saw a `[permissions]` section; the daemon parses gpu=true.
        let inline = "permissions = { gpu = true, fs_write = [\"/etc\"] }\n\n\
            [app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
            entry = \"evil\"\nruntime = \"binary\"\n";
        let p = write(inline);
        let err = validate_manifest_file(&p, "evil", &root)
            .expect_err("inline-table over-broad manifest must be rejected")
            .to_string();
        assert!(err.contains("gpu"), "inline-table gpu must be rejected: {err}");

        // (c) MINIMAL valid manifest still ACCEPTS (the gate is not a blanket no).
        let good = "[app]\nname = \"reverser\"\nversion = \"0.1.0\"\n\
            description = \"reverse a string\"\nentry = \"reverser\"\nruntime = \"binary\"\n\
            [permissions]\nfs_write = [\"state/apps/reverser\"]\nnet_hosts = [\"api.example.com\"]\n";
        let p = write(good);
        validate_manifest_file(&p, "reverser", &root)
            .expect("minimal manifest must pass the deploy-time gate");

        // (d) a manifest path that does not exist -> a clean error, never a pass.
        let missing = dir.join("does-not-exist.toml");
        assert!(
            validate_manifest_file(&missing, "reverser", &root).is_err(),
            "a missing manifest must fail-closed, never pass the gate"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- artifact rendering --------------------------------------------------

    #[test]
    fn report_carries_goal_app_perms_and_deploy_command() {
        let root = std::env::temp_dir();
        let manifest_toml = "[app]\nname = \"reverser\"\nversion = \"0.1.0\"\n\
            description = \"reverse a string\"\nentry = \"reverser\"\nruntime = \"binary\"\n[permissions]\n";
        let m = validate_manifest(manifest_toml, "reverser", &root).unwrap();
        let files = vec![AuthoredFile { path: "src/main.rs".into(), body: "fn main(){}".into() }];
        let report = render_report(
            1_770_000_000,
            "claude-opus-4-8",
            "reverse a string",
            &m,
            &files,
            "$ cargo test\ntest result: ok",
        );
        assert!(report.contains("1770000000"));
        assert!(report.contains("claude-opus-4-8"));
        assert!(report.contains("reverser"));
        assert!(report.contains("reverse a string"), "goal in report");
        assert!(report.contains("src/main.rs"), "authored file listed");
        assert!(report.contains("test result: ok"), "validation tail");
        assert!(report.contains("VALIDATED"));
        assert!(report.contains("scripts/apply_forge.sh 1770000000"), "exact deploy command");
        assert!(report.contains("NOT in apps/"), "must state it is not deployed");
    }

    // -- (4) MOCK-BRAIN drills: stage -> validate (REAL build/test of a
    //        PLANTED fixture) -> propose / reject. No cloud. No apps/. ----------

    struct MockBrain {
        bundle: String,
    }
    impl ForgeBrain for MockBrain {
        fn author<'a>(&'a self, _goal: &'a str) -> BrainFuture<'a> {
            let bundle = self.bundle.clone();
            Box::pin(async move { Ok(bundle) })
        }
    }

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("jarvis-forge-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRoot(dir)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A PLANTED GOOD app spec: a zero-permission Rust "reverser" with a passing
    /// unit test. `[workspace]` keeps cargo from walking into an outer workspace.
    fn good_bundle() -> String {
        "=== FILE: manifest.toml ===\n\
         [app]\nname = \"reverser\"\nversion = \"0.1.0\"\n\
         description = \"reverse an ASCII string\"\nentry = \"reverser\"\nruntime = \"binary\"\n\
         [permissions]\naudio = false\ngpu = false\nnet_hosts = []\nfs_read = []\nfs_write = []\n\
         === FILE: Cargo.toml ===\n\
         [package]\nname = \"reverser\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n\
         [[bin]]\nname = \"reverser\"\npath = \"src/main.rs\"\n\
         === FILE: src/main.rs ===\n\
         pub fn reverse(s: &str) -> String { s.chars().rev().collect() }\n\
         fn main() { println!(\"{}\", reverse(\"jarvis\")); }\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn reverses() {\n        \
         assert_eq!(reverse(\"abc\"), \"cba\");\n    }\n}\n"
            .to_string()
    }

    /// A PLANTED BAD app spec: its test FAILS (the reverse is wrong). Same
    /// minimal permissions, so it is the VALIDATION gate that must reject it.
    fn bad_test_bundle() -> String {
        "=== FILE: manifest.toml ===\n\
         [app]\nname = \"reverser\"\nversion = \"0.1.0\"\n\
         description = \"reverse an ASCII string\"\nentry = \"reverser\"\nruntime = \"binary\"\n\
         [permissions]\n\
         === FILE: Cargo.toml ===\n\
         [package]\nname = \"reverser\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n\n\
         [[bin]]\nname = \"reverser\"\npath = \"src/main.rs\"\n\
         === FILE: src/main.rs ===\n\
         pub fn reverse(s: &str) -> String { s.to_string() }\n\
         fn main() {}\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n    #[test]\n    fn reverses() {\n        \
         assert_eq!(reverse(\"abc\"), \"cba\");\n    }\n}\n"
            .to_string()
    }

    /// A PLANTED OVER-BROAD app spec: it asks for gpu = true. Rejected at the
    /// MANIFEST gate, before any build.
    fn overbroad_bundle() -> String {
        "=== FILE: manifest.toml ===\n\
         [app]\nname = \"reverser\"\nversion = \"0.1.0\"\n\
         description = \"reverse an ASCII string\"\nentry = \"reverser\"\nruntime = \"binary\"\n\
         [permissions]\ngpu = true\n\
         === FILE: Cargo.toml ===\n[package]\nname = \"reverser\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n\
         === FILE: src/main.rs ===\nfn main() {}\n"
            .to_string()
    }

    /// Drive the hermetic pipeline WITHOUT the cloud key gate: run_attempt is the
    /// post-gate core (exactly what forge_draft runs after the gate + key check),
    /// then perform the same propose/quarantine writes forge_draft does. This
    /// keeps the mock-brain drills deterministic whether or not an API key
    /// happens to resolve in the test environment (the mock NEVER calls the
    /// cloud). The forge_draft gate itself is tested separately.
    async fn drive_hermetic(root: &Path, brain: &dyn ForgeBrain, goal: &str) -> ForgeOutcome {
        let ts = now_secs();
        let forge_root = root.join("state").join("forge");
        match run_attempt(root, &forge_root, ts, "mock-model", brain, goal).await {
            AttemptResult::Proposed { manifest, files, manifest_toml, report } => {
                let dir = write_proposal(&forge_root, ts, &manifest, &manifest_toml, &files, &report)
                    .expect("write_proposal");
                ForgeOutcome::Proposed { dir }
            }
            AttemptResult::Rejected { stage, report } => {
                let dir = record_artifact(&forge_root.join("rejected"), ts, "report.md", &report)
                    .unwrap_or_else(|| forge_root.join("rejected").join(ts.to_string()));
                ForgeOutcome::Rejected { stage, dir }
            }
            AttemptResult::Aborted { stage } => ForgeOutcome::Aborted { stage },
        }
    }

    /// THE hermetic happy-path drill: a planted GOOD app -> stage -> REAL build
    /// + test of the fixture -> PROPOSE. Assert files land under
    /// state/forge/proposals/<ts>/app/<name>/, NOTHING is under apps/, nothing
    /// is running. (No cloud: the mock returns the planted bundle.)
    #[tokio::test]
    async fn forge_pipeline_proposes_a_validated_app_and_touches_no_apps_dir() {
        let root = TempRoot::new("propose");
        // The project root also gets an apps/ dir to PROVE nothing is written there.
        std::fs::create_dir_all(root.0.join("apps")).unwrap();
        let brain = MockBrain { bundle: good_bundle() };

        let outcome = drive_hermetic(&root.0, &brain, "reverse a string").await;
        let dir = match outcome {
            ForgeOutcome::Proposed { dir } => dir,
            other => panic!("expected a proposal, got {other:?}"),
        };

        // Proposal artifacts landed under state/forge/proposals/<ts>/.
        assert!(dir.to_string_lossy().contains("proposals"), "lands under proposals/");
        assert!(dir.join("report.md").exists(), "report.md written");
        assert!(dir.join("manifest.toml").exists(), "manifest copy at proposal root");
        let app_dir = dir.join("app").join("reverser");
        assert!(app_dir.join("manifest.toml").exists());
        assert!(app_dir.join("Cargo.toml").exists());
        assert!(app_dir.join("src").join("main.rs").exists());

        // SAFETY: the project apps/ dir is still EMPTY (nothing deployed,
        // nothing registered, nothing running).
        let apps_dir = root.0.join("apps");
        let empty = std::fs::read_dir(&apps_dir).unwrap().next().is_none();
        assert!(empty, "propose-only: nothing may be written into apps/");
    }

    /// A planted BAD app (failing test) -> rejected at `test` + quarantined,
    /// NOT proposed, and apps/ untouched.
    #[tokio::test]
    async fn forge_pipeline_quarantines_an_app_that_fails_its_tests() {
        let root = TempRoot::new("badtest");
        std::fs::create_dir_all(root.0.join("apps")).unwrap();
        let brain = MockBrain { bundle: bad_test_bundle() };

        let outcome = drive_hermetic(&root.0, &brain, "reverse").await;
        match outcome {
            ForgeOutcome::Rejected { stage, dir } => {
                assert_eq!(stage, "test", "a failing test must reject at the test gate");
                assert!(dir.join("report.md").exists(), "rejection report quarantined");
                assert!(dir.to_string_lossy().contains("rejected"), "lands under rejected/");
            }
            other => panic!("expected rejection, got {other:?}"),
        }
        // No proposal, nothing deployed.
        assert!(
            !root.0.join("state/forge/proposals").exists()
                || std::fs::read_dir(root.0.join("state/forge/proposals"))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true),
            "a rejected draft must not be proposed"
        );
        assert!(std::fs::read_dir(root.0.join("apps")).unwrap().next().is_none(), "apps/ untouched");
    }

    /// A planted OVER-BROAD app (gpu = true) -> rejected at the `manifest` gate
    /// BEFORE any build, and quarantined. The permission-minimization gate.
    #[tokio::test]
    async fn forge_pipeline_rejects_an_overbroad_manifest_before_building() {
        let root = TempRoot::new("overbroad");
        std::fs::create_dir_all(root.0.join("apps")).unwrap();
        let brain = MockBrain { bundle: overbroad_bundle() };

        let outcome = drive_hermetic(&root.0, &brain, "reverse").await;
        match outcome {
            ForgeOutcome::Rejected { stage, dir } => {
                assert_eq!(stage, "manifest", "over-broad permissions reject at the manifest gate");
                let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
                assert!(report.contains("gpu"), "reason names the over-broad permission:\n{report}");
            }
            other => panic!("expected manifest rejection, got {other:?}"),
        }
        // No staging build happened and apps/ is untouched.
        assert!(std::fs::read_dir(root.0.join("apps")).unwrap().next().is_none(), "apps/ untouched");
    }

    /// THE gate test: forge_draft with enabled = false is fully inert — no draft
    /// (the mock would PANIC if called), no stage, no propose. And with enabled =
    /// true but no key it Blocks before drafting. We assert both branches without
    /// depending on the ambient API key: the disabled branch returns before the
    /// key check, so it is deterministic; the key-blocked branch is asserted only
    /// when no key resolves.
    #[tokio::test]
    async fn forge_draft_gate_disabled_is_inert_and_no_key_blocks() {
        struct PanicBrain;
        impl ForgeBrain for PanicBrain {
            fn author<'a>(&'a self, _g: &'a str) -> BrainFuture<'a> {
                Box::pin(async { panic!("draft must NOT be called when forge is disabled") })
            }
        }
        let root = TempRoot::new("gate");
        std::fs::create_dir_all(root.0.join("apps")).unwrap();

        // enabled = false short-circuits BEFORE the key check and BEFORE any
        // draft — the PanicBrain proves author() is never called.
        let outcome = forge_draft(&root.0, false, "propose", "mock-model", &PanicBrain, "x", |_| {}).await;
        assert!(matches!(outcome, ForgeOutcome::Disabled), "disabled gate must short-circuit");
        // Nothing under state/forge, nothing in apps/.
        assert!(!root.0.join("state").join("forge").exists(), "no state/forge dir when disabled");
        assert!(std::fs::read_dir(root.0.join("apps")).unwrap().next().is_none());

        // enabled = true but no key -> Blocked (drafting needs the cloud). Only
        // assertable when the environment has no key; otherwise the PanicBrain
        // would be reached, so skip the assertion when a key IS present.
        if anthropic::resolve_api_key().await.is_none() {
            let outcome =
                forge_draft(&root.0, true, "propose", "mock-model", &PanicBrain, "x", |_| {}).await;
            assert!(matches!(outcome, ForgeOutcome::Blocked), "no key must Block before drafting");
        }
    }

    /// Even in mode = "auto", the pipeline still only PROPOSES — it never
    /// deploys into apps/. (The deploy decision is identical for Auto and
    /// Propose: write a proposal and stop. Driven hermetically; the Auto gate
    /// itself is covered by forge_action_gating_truth_table.)
    #[tokio::test]
    async fn forge_auto_mode_still_only_proposes_never_deploys() {
        let root = TempRoot::new("auto");
        std::fs::create_dir_all(root.0.join("apps")).unwrap();
        // forge_action(true, "auto") == Auto, and the propose path is identical
        // for Auto and Propose — both write a proposal and stop, neither deploys.
        assert_eq!(forge_action(true, "auto"), ForgeAction::Auto);
        let brain = MockBrain { bundle: good_bundle() };
        let outcome = drive_hermetic(&root.0, &brain, "reverse").await;
        match outcome {
            ForgeOutcome::Proposed { dir } => {
                assert!(dir.to_string_lossy().contains("proposals"), "still writes a PROPOSAL");
                assert!(
                    std::fs::read_dir(root.0.join("apps")).unwrap().next().is_none(),
                    "auto mode must NOT deploy into apps/"
                );
            }
            other => panic!("expected a proposal, got {other:?}"),
        }
    }

    // -- (5) apply_forge.sh re-validation: the deploy script is the LAST line of
    //        defense against a hand-edited proposal. It must refuse over-broad
    //        grants even when they are smuggled across MULTI-LINE TOML arrays
    //        (whose opening `key = [` line carries no quoted tokens), and it must
    //        enforce the net_hosts COUNT cap. These tests shell out to the REAL
    //        scripts/apply_forge.sh against a HERMETIC temp root (a copy of the
    //        script is run so its `ROOT=$(dirname)/..` resolves to the temp dir,
    //        never the live tree) and assert non-zero exit + apps/ untouched. ---

    /// Plant a forge proposal under `<root>/state/forge/proposals/<ts>/app/<name>/`
    /// with the given manifest body + a trivial python entry, and copy the live
    /// apply_forge.sh into `<root>/scripts/` so it operates on the temp tree.
    /// Returns the path to the copied script.
    fn plant_proposal_and_script(root: &Path, ts: u64, name: &str, manifest_toml: &str) -> PathBuf {
        let app_dir = root
            .join("state/forge/proposals")
            .join(ts.to_string())
            .join("app")
            .join(name);
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("manifest.toml"), manifest_toml).unwrap();
        std::fs::write(app_dir.join("main.py"), "print(\"hi\")\n").unwrap();
        std::fs::create_dir_all(root.join("apps")).unwrap();

        // The real deploy script lives at <repo>/scripts/apply_forge.sh; the repo
        // root is the parent of this crate (CARGO_MANIFEST_DIR = daemon/).
        let live_script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("scripts").join("apply_forge.sh");
        let scripts = root.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        let copied = scripts.join("apply_forge.sh");
        std::fs::copy(&live_script, &copied)
            .unwrap_or_else(|e| panic!("copy apply_forge.sh from {}: {e}", live_script.display()));
        copied
    }

    /// Resolve an already-built jarvisd that implements the deploy-time gate, for
    /// the hermetic apply_forge.sh harness (which runs the script in a temp ROOT
    /// with no daemon/ tree to build). Prefer an existing release/debug binary
    /// under this crate's target/; build the release binary once if neither
    /// exists. The script PROVES the binary rejects an over-broad probe before
    /// trusting it, so this only supplies a binary — it cannot weaken the gate.
    fn resolve_gate_binary() -> PathBuf {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
        for prof in ["release", "debug"] {
            let cand = target.join(prof).join("jarvisd");
            if cand.is_file() {
                return cand;
            }
        }
        // Neither exists in this checkout — build the release binary once.
        let status = std::process::Command::new(env!("CARGO"))
            .args(["build", "--release", "--bin", "jarvisd"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("build jarvisd for the apply_forge gate test");
        assert!(status.success(), "building jarvisd for the gate test failed");
        target.join("release").join("jarvisd")
    }

    /// Run the copied apply_forge.sh `<ts> --yes` and return (success, combined
    /// stdout+stderr). Points the script's deploy-time gate at an already-built
    /// jarvisd via JARVISD_VALIDATE_BIN so the hermetic temp ROOT (which has no
    /// daemon/ tree) need not build one.
    fn run_apply_forge(script: &Path, ts: u64) -> (bool, String) {
        let gate_bin = resolve_gate_binary();
        let out = std::process::Command::new("bash")
            .arg(script)
            .arg(ts.to_string())
            .arg("--yes")
            .env("JARVISD_VALIDATE_BIN", &gate_bin)
            .output()
            .expect("spawn apply_forge.sh");
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), s)
    }

    /// REGRESSION (BLOCKING finding): a hand-edited proposal that smuggles an
    /// over-broad grant across a MULTI-LINE TOML array must be REFUSED by
    /// apply_forge.sh — the script can no longer be fooled by the opening
    /// `key = [` line carrying no quoted tokens. Covers fs_write, fs_read, and
    /// net_hosts (escape + count cap), plus a non-bare host. apps/ stays empty.
    #[test]
    fn apply_forge_refuses_multiline_overbroad_manifests() {
        // Each case: (tag, name, manifest, expected substring of the refusal).
        let cases: &[(&str, &str, String, &str)] = &[
            (
                // The exact finding scenario: fs_write to /etc/cron.d + ".." escape.
                "fs_write",
                "evil",
                "[app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                 runtime = \"python\"\nentry = \"main.py\"\n\n\
                 [permissions]\naudio = false\ngpu = false\n\
                 fs_write = [\n  \"/etc/cron.d/evil\",\n  \"../../tmp/escape\"\n]\n\
                 fs_read = []\nnet_hosts = []\n"
                    .to_string(),
                "fs_write",
            ),
            (
                // Multi-line fs_read escape (an SSH private key) past a minimal fs_write.
                "fs_read",
                "evil",
                "[app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                 runtime = \"python\"\nentry = \"main.py\"\n\n\
                 [permissions]\nfs_write = [\"state/apps/evil\"]\n\
                 fs_read = [\n  \"ok/path\",\n  \"../../etc/shadow\"\n]\nnet_hosts = []\n"
                    .to_string(),
                "fs_read",
            ),
            (
                // Multi-line net_hosts COUNT > MAX_NET_HOSTS (each bare, too many).
                "net_count",
                "evil",
                "[app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                 runtime = \"python\"\nentry = \"main.py\"\n\n\
                 [permissions]\nfs_write = [\"state/apps/evil\"]\nfs_read = []\n\
                 net_hosts = [\n  \"h1.test\", \"h2.test\",\n  \"h3.test\", \"h4.test\",\n  \
                 \"h5.test\", \"h6.test\",\n  \"h7.test\"\n]\n"
                    .to_string(),
                "net_hosts requests 7 hosts",
            ),
            (
                // Multi-line net_hosts with a non-bare host (scheme/port).
                "net_bare",
                "evil",
                "[app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                 runtime = \"python\"\nentry = \"main.py\"\n\n\
                 [permissions]\nfs_write = [\"state/apps/evil\"]\nfs_read = []\n\
                 net_hosts = [\n  \"ok.test\",\n  \"attacker.test:4444\"\n]\n"
                    .to_string(),
                "not a bare hostname",
            ),
            (
                // NEW: the parser-differential the OLD text scan missed — top-level
                // DOTTED keys (no [permissions] header) the daemon's toml crate
                // parses as gpu=true. The text scan saw nothing; the real-parse gate
                // must reject it.
                "dotted_gpu",
                "evil",
                "permissions.gpu = true\n\n\
                 [app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                 runtime = \"python\"\nentry = \"main.py\"\n"
                    .to_string(),
                "gpu",
            ),
            (
                // NEW: the INLINE-TABLE form of the same bypass.
                "inline_gpu",
                "evil",
                "permissions = { gpu = true }\n\n\
                 [app]\nname = \"evil\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
                 runtime = \"python\"\nentry = \"main.py\"\n"
                    .to_string(),
                "gpu",
            ),
        ];

        let mut ts = now_secs().saturating_mul(1000); // spread distinct stamps
        for (tag, name, manifest, expect) in cases {
            ts += 1;
            let root = TempRoot::new(&format!("apply-multiline-{tag}"));
            let script = plant_proposal_and_script(&root.0, ts, name, manifest);
            let (ok, output) = run_apply_forge(&script, ts);
            assert!(
                !ok,
                "[{tag}] over-broad multi-line manifest MUST be refused; script succeeded:\n{output}"
            );
            assert!(
                output.contains("RESULT: failed"),
                "[{tag}] expected a RESULT: failed line:\n{output}"
            );
            assert!(
                output.contains(expect),
                "[{tag}] refusal should mention {expect:?}; got:\n{output}"
            );
            // The gate failed during re-validation: nothing was deployed.
            assert!(
                std::fs::read_dir(root.0.join("apps")).unwrap().next().is_none(),
                "[{tag}] apps/ must be untouched after a refused deploy:\n{output}"
            );
        }
    }

    /// A LEGIT minimal manifest expressed with MULTI-LINE arrays must PASS the
    /// permission re-validation gate (the normalization must not over-reject) —
    /// it reaches the deploy stage and lands the app under the temp apps/. Proves
    /// the multi-line hardening is value-correct, not a blanket refusal.
    #[test]
    fn apply_forge_accepts_legit_multiline_manifest() {
        let ts = now_secs().saturating_mul(1000).saturating_add(900);
        let root = TempRoot::new("apply-multiline-ok");
        let manifest = "[app]\nname = \"reverser\"\nversion = \"0.1.0\"\ndescription = \"d\"\n\
             runtime = \"python\"\n\
             entry = \"main.py\"\n\n[permissions]\naudio = false\ngpu = false\n\
             fs_write = [\n  \"state/apps/reverser\"\n]\n\
             fs_read = [\n  \"apps/reverser/data\"\n]\n\
             net_hosts = [\n  \"example.com\",\n  \"api.example.org\"\n]\n"
            .to_string();
        let script = plant_proposal_and_script(&root.0, ts, "reverser", &manifest);
        let (ok, output) = run_apply_forge(&script, ts);
        // py_compile on the trivial main.py passes, so the deploy completes.
        assert!(ok, "legit minimal multi-line manifest must deploy; got:\n{output}");
        assert!(output.contains("RESULT: ok"), "expected RESULT: ok:\n{output}");
        assert!(
            root.0.join("apps/reverser/manifest.toml").exists(),
            "legit app should be deployed into apps/:\n{output}"
        );
    }

    /// THE real-cloud forge drill. #[ignore] by default — the ONLY cloud path in
    /// this module, run explicitly by the verifier:
    ///   cargo test --release forge_drill_real_cloud -- --ignored --nocapture
    /// (or `jarvisd --forge-drill`). It authors a benign zero-permission app via
    /// the REAL cloud, proving draft -> stage -> validate -> propose end to end.
    /// Skips gracefully (passes) when no API key is present.
    #[tokio::test]
    #[ignore = "real cloud spend; run by the verifier with --ignored"]
    async fn forge_drill_real_cloud() {
        if anthropic::resolve_api_key().await.is_none() {
            eprintln!("forge_drill_real_cloud: no API key resolved; skipping (run with the key set)");
            return;
        }
        let model = "claude-opus-4-8";
        let dir = run_forge_drill(model).await.expect("forge drill must produce a proposal");
        assert!(dir.join("report.md").exists(), "drill must write report.md");
        assert!(dir.join("manifest.toml").exists());
        let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(report.contains("VALIDATED"), "drill proposal must be validated:\n{report}");
        // Clean up the throwaway sandbox.
        if let Some(sandbox) = dir.ancestors().find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("jarvis-forge-drill-"))
        }) {
            let _ = std::fs::remove_dir_all(sandbox);
        }
    }
}
