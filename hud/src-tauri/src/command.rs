//! HUD -> daemon COMMAND CHANNEL — the Tauri BACKEND side (the trust boundary).
//!
//! The React layer never speaks to the daemon socket directly and NEVER holds
//! the capability token. It calls the single `send_command` Tauri command with a
//! bounded `{cmd, …}` request; THIS backend:
//!
//!   1. validates the `cmd` against the SAME structural allowlist the daemon
//!      enforces (defense-in-depth — an unknown cmd never even reaches the wire),
//!   2. reads the per-boot capability token from its `0600` handoff file inside
//!      the daemon's confined `state/ipc/` dir (the out-of-band handshake; the
//!      token is never exposed to JS, never logged, never echoed back),
//!   3. opens the local Unix socket `state/ipc/command.sock`, writes ONE JSONL
//!      line carrying the token, reads ONE JSONL reply, and returns the parsed
//!      reply to the UI — token stripped.
//!
//! It can do NOTHING the daemon's command channel cannot: every consequential
//! action STILL parks via the daemon's cross-turn confirmation gate + the
//! OFF-by-default master switch; `confirm {id}` only replays a genuinely-parked
//! action; `dismiss_forge` clears a marker only (apply stays
//! scripts/apply_forge.sh). This module adds NO authority — it is a typed,
//! token-injecting relay over a local socket.
//!
//! SHAPE: [`build_request`] (request assembly + allowlist) and [`parse_reply`]
//! (defensive reply narrowing) are PURE and unit-tested without any socket. The
//! socket round-trip ([`round_trip`]) is the only I/O and is exercised only by
//! the live app, never by a test that binds a daemon.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Value};

/// Cap on a single command line written to the socket (matches the daemon's
/// MAX_LINE_BYTES). A request larger than this is rejected here before any I/O.
const MAX_LINE_BYTES: usize = 8 * 1024;
/// Cap on the free-text payload (ask.text / mission.goal) — matches the daemon's
/// MAX_TEXT_CHARS; we trim here so an oversized field never rides the wire.
const MAX_TEXT_CHARS: usize = 4 * 1024;
/// Read/connect timeout on the socket round-trip. The pipeline bounds its own
/// work; this is the backstop so a hung daemon never wedges the UI thread.
const SOCKET_TIMEOUT: Duration = Duration::from_secs(120);
/// Cap on the reply we read back (a prose reply, never bulk data).
const MAX_REPLY_BYTES: usize = 64 * 1024;
/// The EL voice-design prompt floor — matches daemon
/// `command.rs::MIN_VOICE_DESCRIPTION_CHARS`. A `design_voice` description below
/// this is rejected HERE (defense-in-depth; the daemon also rejects it) so a
/// too-thin request never rides the wire.
const MIN_VOICE_DESCRIPTION_CHARS: usize = 20;
/// The bounded track-length band (milliseconds) for `compose_music`. An OPTIONAL
/// length outside this band is CLAMPED here (not rejected) so a request never
/// rides an absurd duration; an absent length is omitted (the daemon defaults it).
/// Mirrors the HUD core's seconds band (3..=600s).
const MIN_LENGTH_MS: u32 = 3_000;
const MAX_LENGTH_MS: u32 = 600_000;

/// The bounded command set the backend will relay — the SAME structural
/// allowlist as daemon/src/command.rs. An unknown `cmd` from JS is rejected here
/// (defense-in-depth) and never reaches the socket.
const ALLOWED_COMMANDS: &[&str] = &[
    "ask", "brief", "mission", "roster", "state", "pending", "confirm", "deny", "dismiss_forge",
    // `policy` is the USER-SET-ONLY consequential-policy write verb: the backend
    // relays the anchored phrase text; the daemon classifies + applies it via the
    // user-only write path (NOT the model tool loop).
    "policy",
    // Task #12 — the panic/lockdown emergency stop. Both are DEDICATED, bare verbs
    // (no fields): the daemon calls lockdown::panic()/unlock() DIRECTLY, never the
    // model. The HUD PANIC button sends `panic`; the deliberate UNLOCK control
    // sends `unlock` (the authenticated-local USER path — there is no model/agent
    // route to unlock). Each reply carries `locked` so the HUD flips its indicator
    // immediately on the button press.
    "panic", "unlock",
    // Phase-2 SFX cue — play a BUILT-IN named cue (confirm/alert/error/success/
    // notify/wake; the daemon's sfx_cue::CATALOG is the source of truth). Carries
    // ONLY the cue name. The daemon gates it on its already-shipped
    // voice_tier::sfx_enabled (cloud_sfx + an ElevenLabs key, online) and returns
    // an honest silent no-op when the gate is closed — this adds NO new authority
    // and NO new tier. Relayed through the SAME token-injecting socket as every
    // other verb (defense-in-depth: an unknown cue name is rejected daemon-side).
    "play_cue",
    // Phase-2 Voice Lab — the two ElevenLabs provisioning verbs.
    //   `design_voice` designs a voice for an agent from a TEXT description and
    //     carries {agent, description, name?}. Only the text description leaves the
    //     device; the returned voice id is stored daemon-side.
    //   `create_pronunciation` mints ONE alias pronunciation rule (word -> say) and
    //     carries {word, say, name?}. Text rules only; no audio leaves the device.
    // The daemon gates BOTH on its already-shipped key + non-Local-tier gate and
    // returns an HONEST `Err` (never a fabricated voice/dictionary) when the gate is
    // closed. These add NO new authority and NO new tier — they ride the SAME
    // token-injecting socket as every other verb (defense-in-depth: a too-thin /
    // empty request is rejected daemon-side).
    "design_voice", "create_pronunciation",
    // Phase-3 Compose music — generate a FULL music track from a text PROMPT (the
    // daemon `compose_music` verb). Carries {prompt, length_ms?}: only the text
    // prompt (+ an optional bounded track length) leaves the device; the generated
    // track is a cloud generation handled daemon-side. The daemon gates it on its
    // already-shipped `[voice].cloud_music` + an ElevenLabs key + a non-Local tier
    // and returns honest "unavailable" / "didn't go through, nothing was created"
    // prose (never a fabricated track) when the gate is closed. It adds NO new
    // authority and NO new tier — it rides the SAME token-injecting socket as every
    // other verb (defense-in-depth: an empty prompt is rejected daemon-side).
    "compose_music",
];

/// The typed request the React layer hands `send_command`. Every field is
/// optional on the wire so one command shape serves all ten verbs; the
/// per-command requirements are validated in [`build_request`]. There is
/// DELIBERATELY no `token` field — the token is backend-only.
#[derive(Debug, Default, serde::Deserialize)]
pub struct CommandRequest {
    pub cmd: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub ts: Option<u64>,
    /// Phase-2 SFX cue — the BUILT-IN cue NAME to play (`play_cue` only). A
    /// catalog atom (confirm/alert/error/…); never a key, prompt, or path.
    #[serde(default)]
    pub cue: Option<String>,
    /// Phase-2 Voice Lab — the voice DESCRIPTION (`design_voice` only). Free text,
    /// clamped + length-checked in [`build_request`] (the EL design floor is
    /// `MIN_VOICE_DESCRIPTION_CHARS`). Never a key, never an id.
    #[serde(default)]
    pub description: Option<String>,
    /// Phase-2 Voice Lab — the optional display/dictionary NAME (`design_voice` +
    /// `create_pronunciation`). A short label only; omitted when blank (the daemon
    /// defaults it).
    #[serde(default)]
    pub name: Option<String>,
    /// Phase-2 Voice Lab — the string to replace (`create_pronunciation` only). The
    /// word/phrase the rule rewrites; non-empty, clamped in [`build_request`].
    #[serde(default)]
    pub word: Option<String>,
    /// Phase-2 Voice Lab — the alias to say in its place (`create_pronunciation`
    /// only). The replacement pronunciation text; non-empty, clamped.
    #[serde(default)]
    pub say: Option<String>,
    /// Phase-3 Compose music — the track PROMPT (`compose_music` only). Free text,
    /// clamped + non-empty-checked in [`build_request`]. Never a key, never a path.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Phase-3 Compose music — the OPTIONAL track length in milliseconds
    /// (`compose_music` only). Clamped to a sensible band in [`build_request`];
    /// omitted from the wire when absent (the daemon defaults it).
    #[serde(default)]
    pub length_ms: Option<u32>,
}

/// The reply surfaced to the UI. `ok` mirrors the daemon's `{ok}`; `reply` is the
/// prose line (ask/brief/mission/roster/state/confirm/deny/dismiss_forge);
/// `pending` carries the replay-free pending listing (pending command only);
/// `error` is the daemon's rejection vocabulary or a backend-local error. NO
/// token or secret is ever present.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct CommandReply {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Task #12 — the lockdown verdict the daemon attaches to the panic/unlock
    /// replies (`{"locked": is_locked_down()}`), so the HUD can flip its LOCKED
    /// DOWN / NORMAL indicator IMMEDIATELY on the button press without waiting for
    /// the next startup snapshot. Present only when the daemon sends it (every
    /// other verb omits it); narrowed by name like every other field, so no extra
    /// material is forwarded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
}

impl CommandReply {
    fn err(error: impl Into<String>) -> Self {
        Self { ok: false, reply: None, pending: None, error: Some(error.into()), locked: None }
    }
}

/// The replay-FREE pending listing (the `pending` command's payload). Ids +
/// previews only — no input args ever cross the wire, so nothing here can fire
/// an action; only an explicit `confirm {id}` does. Mirrors the daemon's
/// `{confirmation:{id,agent,tool,preview}|null, forge_pending_ts}`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PendingSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation: Option<PendingConfirmation>,
    /// The forge proposal ts (string), or None. The deck shows the manual apply
    /// command for it and offers Dismiss only — never an apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forge_pending_ts: Option<String>,
}

/// One genuinely-parked confirmation: id + agent + tool + a faithful preview.
/// NEVER the input args (those stay daemon-side until an explicit confirm).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PendingConfirmation {
    pub id: String,
    pub agent: String,
    pub tool: String,
    pub preview: String,
}

/* --------------------------------------------------------- request assembly */

/// Trim a free-text field to [`MAX_TEXT_CHARS`] chars (char-boundary safe), the
/// same clamp the daemon applies — so an oversized field never rides the wire.
fn clamp_text(s: &str) -> String {
    if s.chars().count() <= MAX_TEXT_CHARS {
        return s.to_string();
    }
    s.chars().take(MAX_TEXT_CHARS).collect()
}

/// Build the JSONL request OBJECT for the socket from a typed UI request + the
/// capability token, validating against the structural allowlist and the
/// per-command required fields. PURE — unit-tested. Returns a structured error
/// string (the UI-facing rejection) when the request is not well-formed; the
/// token is injected here and ONLY here (it is never part of the UI request).
pub fn build_request(req: &CommandRequest, token: &str) -> Result<Value, String> {
    if !ALLOWED_COMMANDS.contains(&req.cmd.as_str()) {
        return Err("unknown_command".to_string());
    }
    let mut obj = serde_json::Map::new();
    obj.insert("token".to_string(), json!(token));
    obj.insert("cmd".to_string(), json!(req.cmd));

    match req.cmd.as_str() {
        "ask" => {
            let text = req.text.as_deref().map(clamp_text).unwrap_or_default();
            if text.trim().is_empty() {
                return Err("ask requires non-empty text".to_string());
            }
            obj.insert("text".to_string(), json!(text));
            // An agent ref, when present and non-empty, selects the handling
            // agent (ITS allowlist applies daemon-side).
            if let Some(agent) = req.agent.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                obj.insert("agent".to_string(), json!(agent));
            }
        }
        "mission" => {
            let goal = req.goal.as_deref().map(clamp_text).unwrap_or_default();
            if goal.trim().is_empty() {
                return Err("mission requires a non-empty goal".to_string());
            }
            obj.insert("goal".to_string(), json!(goal));
        }
        "confirm" | "deny" => {
            let id = req.id.as_deref().map(str::trim).unwrap_or("");
            if id.is_empty() {
                return Err(format!("{} requires an id", req.cmd));
            }
            obj.insert("id".to_string(), json!(id));
        }
        "dismiss_forge" => {
            let ts = req.ts.ok_or_else(|| "dismiss_forge requires a ts".to_string())?;
            obj.insert("ts".to_string(), json!(ts));
        }
        "policy" => {
            let text = req.text.as_deref().map(clamp_text).unwrap_or_default();
            if text.trim().is_empty() {
                return Err("policy requires the phrase text".to_string());
            }
            obj.insert("text".to_string(), json!(text));
        }
        "play_cue" => {
            // Carry ONLY the cue name (a catalog atom). It is normalized to a lower
            // bound here (trimmed, lowercased) so the daemon's case-insensitive
            // lookup is exact; an unknown name is rejected daemon-side (honest
            // no-cue). No key, prompt, or path ever rides this verb.
            let cue = req.cue.as_deref().map(str::trim).unwrap_or("");
            if cue.is_empty() {
                return Err("play_cue requires a cue name".to_string());
            }
            obj.insert("cue".to_string(), json!(cue.to_ascii_lowercase()));
        }
        "design_voice" => {
            // {agent, description, name?}. Mirror the daemon `decide` floors so a
            // thin request never spends a cloud op: a non-empty agent, a description
            // at/above the EL design floor, and an OPTIONAL display name (omitted
            // when blank — the daemon defaults it to the agent name). Only the text
            // description leaves the device; no key/id ever rides this verb.
            let agent = req.agent.as_deref().map(str::trim).unwrap_or("");
            if agent.is_empty() {
                return Err("design_voice requires an agent".to_string());
            }
            let description = req.description.as_deref().map(clamp_text).unwrap_or_default();
            let description = description.trim();
            if description.chars().count() < MIN_VOICE_DESCRIPTION_CHARS {
                return Err("design_voice requires a longer voice description".to_string());
            }
            obj.insert("agent".to_string(), json!(agent));
            obj.insert("description".to_string(), json!(description));
            if let Some(name) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                obj.insert("name".to_string(), json!(name));
            }
        }
        "create_pronunciation" => {
            // {word, say, name?}. Mirror the daemon `decide` floors: a non-empty word
            // (the string to replace) AND a non-empty say (the alias pronunciation),
            // plus an OPTIONAL dictionary name (omitted when blank — the daemon
            // defaults it). Text rules only; no audio/key/id ever rides this verb.
            let word = req.word.as_deref().map(clamp_text).unwrap_or_default();
            let word = word.trim();
            if word.is_empty() {
                return Err("create_pronunciation requires a word to replace".to_string());
            }
            let say = req.say.as_deref().map(clamp_text).unwrap_or_default();
            let say = say.trim();
            if say.is_empty() {
                return Err("create_pronunciation requires an alias pronunciation".to_string());
            }
            obj.insert("word".to_string(), json!(word));
            obj.insert("say".to_string(), json!(say));
            if let Some(name) = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                obj.insert("name".to_string(), json!(name));
            }
        }
        "compose_music" => {
            // {prompt, length_ms?}. Mirror the daemon floor: a non-empty prompt is
            // required (a blank request never spends a cloud op). The OPTIONAL
            // length is CLAMPED into the bounded band and omitted when absent (the
            // daemon defaults it). Only the text prompt (+ the optional length)
            // leaves the device; no key/path ever rides this verb.
            let prompt = req.prompt.as_deref().map(clamp_text).unwrap_or_default();
            let prompt = prompt.trim();
            if prompt.is_empty() {
                return Err("compose_music requires a non-empty prompt".to_string());
            }
            obj.insert("prompt".to_string(), json!(prompt));
            if let Some(ms) = req.length_ms {
                obj.insert("length_ms".to_string(), json!(ms.clamp(MIN_LENGTH_MS, MAX_LENGTH_MS)));
            }
        }
        // brief / roster / state / pending carry no extra fields. The task #12
        // emergency-stop verbs `panic` / `unlock` are also bare (the daemon calls
        // lockdown::panic()/unlock() directly — no payload to validate), so they
        // fall through here intentionally: just `{token, cmd}` rides the wire.
        _ => {}
    }
    Ok(Value::Object(obj))
}

/* ----------------------------------------------------------- reply parsing */

/// Defensively narrow one daemon reply line into a [`CommandReply`]. NEVER
/// throws; a malformed/empty reply becomes a structured backend error rather
/// than a panic. Strips everything except the contracted fields, so even if a
/// future daemon echoed extra material, no stray field (and certainly no token)
/// is forwarded to the UI. PURE — unit-tested.
pub fn parse_reply(raw: &str) -> CommandReply {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return CommandReply::err("empty reply from the command channel");
    }
    let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
        return CommandReply::err("malformed reply from the command channel");
    };
    let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        let error = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("command_failed")
            .to_string();
        return CommandReply::err(error);
    }
    // ok == true: surface reply OR the pending snapshot (only one is present per
    // command). We re-build each field by name so nothing extra is forwarded.
    let reply = value.get("reply").and_then(Value::as_str).map(str::to_string);
    let pending = value.get("pending").map(parse_pending);
    // Task #12: the panic/unlock replies carry `locked`; every other verb omits it
    // (so it stays None). Read by name so nothing else is forwarded.
    let locked = value.get("locked").and_then(Value::as_bool);
    CommandReply { ok: true, reply, pending, error: None, locked }
}

/// Narrow the `pending` object into a [`PendingSnapshot`]. Defensive: a missing/
/// malformed confirmation becomes None (no card), and the forge ts is coerced to
/// a string whether the daemon sent a number or a string. No input args are read
/// (the daemon never sends them on this path).
fn parse_pending(v: &Value) -> PendingSnapshot {
    let confirmation = v.get("confirmation").and_then(|c| {
        let id = c.get("id").and_then(Value::as_str)?.to_string();
        if id.is_empty() {
            return None;
        }
        Some(PendingConfirmation {
            id,
            agent: c.get("agent").and_then(Value::as_str).unwrap_or("").to_string(),
            tool: c.get("tool").and_then(Value::as_str).unwrap_or("").to_string(),
            preview: c.get("preview").and_then(Value::as_str).unwrap_or("").to_string(),
        })
    });
    let forge_pending_ts = v.get("forge_pending_ts").and_then(|t| match t {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    PendingSnapshot { confirmation, forge_pending_ts }
}

/* ------------------------------------------------------------- token + I/O */

/// Resolve the DARWIN repo root, reusing the self-heal resolver (DARWIN_ROOT env,
/// else the exe/cwd upward walk to the scripts/apply_heal.sh + config/darwin.toml
/// markers). The command socket + token file both live under `<root>/state/ipc/`.
fn darwin_root() -> Result<PathBuf, String> {
    crate::heal::resolve_root_for_command()
}

/// Read the per-boot capability token from its `0600` handoff file inside the
/// daemon's confined `state/ipc/` dir. The token is read ONLY here, held ONLY on
/// the stack for the round-trip, and is NEVER logged, returned to JS, or put in
/// any error string. A missing file means the daemon is not running (or has not
/// finished its handoff) — a structured, secret-free error.
pub(crate) fn read_token(root: &Path) -> Result<String, String> {
    let path = root.join("state").join("ipc").join("command.token");
    let token = std::fs::read_to_string(&path)
        .map_err(|_| "command channel unavailable (is darwind running?)".to_string())?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("command channel token is empty".to_string());
    }
    Ok(token)
}

/// The socket path: `<root>/state/ipc/command.sock`.
fn socket_path(root: &Path) -> PathBuf {
    root.join("state").join("ipc").join("command.sock")
}

/// ONE blocking JSONL round-trip over the local Unix socket: connect, write the
/// request line, read the reply line (bounded). The ONLY I/O in this module. The
/// token is already embedded in `line` (built by [`build_request`]); this fn
/// never logs `line`. Returns the raw reply string for [`parse_reply`].
fn round_trip(sock: &Path, line: &str) -> Result<String, String> {
    if line.len() > MAX_LINE_BYTES {
        return Err("oversized".to_string());
    }
    let mut stream = UnixStream::connect(sock)
        .map_err(|_| "command channel unavailable (is darwind running?)".to_string())?;
    stream
        .set_read_timeout(Some(SOCKET_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(SOCKET_TIMEOUT)))
        .map_err(|e| format!("socket timeout setup failed: {e}"))?;

    let mut out = line.as_bytes().to_vec();
    if !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    stream
        .write_all(&out)
        .and_then(|_| stream.flush())
        .map_err(|_| "failed to send the command".to_string())?;

    // Read up to the first newline (one JSONL reply), bounded so a misbehaving
    // peer cannot stream unbounded bytes into the UI thread.
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => break, // peer closed
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                buf.push(byte[0]);
                if buf.len() > MAX_REPLY_BYTES {
                    return Err("reply exceeded the size cap".to_string());
                }
            }
            Err(_) => return Err("failed to read the command reply".to_string()),
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/* ----------------------------------------------------------------- command */

/// The single Tauri command the React deck calls. It is the trust boundary to
/// the daemon: it validates the request (structural allowlist + required
/// fields), injects the backend-only capability token, performs ONE local
/// socket round-trip off the async runtime (blocking I/O on a worker), and
/// returns the defensively-parsed reply — token stripped, no secret echoed.
///
/// Errors are surfaced as a `CommandReply` with `ok:false` + a secret-free
/// `error` (so the UI renders a clean failure state) rather than a thrown Tauri
/// error, EXCEPT for the not-well-formed-request case which is a programmer/UI
/// bug worth a hard error.
#[tauri::command]
pub async fn send_command(request: CommandRequest) -> Result<CommandReply, String> {
    // Validate + resolve cheaply (no token, no I/O) before touching the socket.
    if !ALLOWED_COMMANDS.contains(&request.cmd.as_str()) {
        return Ok(CommandReply::err("unknown_command"));
    }

    // Run the token read + blocking socket round-trip off the async runtime.
    tauri::async_runtime::spawn_blocking(move || {
        let root = match darwin_root() {
            Ok(r) => r,
            Err(e) => return Ok(CommandReply::err(e)),
        };
        let token = match read_token(&root) {
            Ok(t) => t,
            Err(e) => return Ok(CommandReply::err(e)),
        };
        let line = match build_request(&request, &token) {
            Ok(v) => v.to_string(),
            Err(e) => return Ok(CommandReply::err(e)),
        };
        // `token` is dropped at the end of this scope; it never leaves the stack.
        let raw = match round_trip(&socket_path(&root), &line) {
            Ok(r) => r,
            Err(e) => return Ok(CommandReply::err(e)),
        };
        Ok(parse_reply(&raw))
    })
    .await
    .map_err(|e| format!("command task failed: {e}"))?
}

/* ------------------------------------------------------- SFX cue (Phase-2) */

/// The honest play-cue outcome the HUD maps to prose. Mirrors the daemon's
/// `PlayOutcome` vocabulary (played/cached/disabled/unknown/failed). It NEVER
/// claims a cue played when the daemon reported a silent no-op. `detail` is a
/// short, secret-free human line (never the produced WAV path).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PlayCueReply {
    pub outcome: String,
    pub detail: String,
}

/// Map a daemon `CommandReply` (the `play_cue` relay's reply) into the bounded
/// [`PlayCueReply`] vocabulary. PURE — unit-tested without any socket.
///
/// HONESTY: an `ok:false` reply NEVER becomes "played". The daemon's
/// `trigger_cue` returns prose, so we classify on the secret-free reply/error
/// text: a "cache"/"cached" success → `cached` (no cloud call), any other success
/// → `played`; a switch-off/no-key/offline rejection → `disabled` (honest silent
/// no-op); an unknown-cue rejection → `unknown`; anything else → `failed`. No path
/// or secret is ever forwarded — only the contracted `{outcome, detail}`.
pub fn cue_outcome(reply: &CommandReply) -> PlayCueReply {
    if reply.ok {
        let line = reply.reply.as_deref().unwrap_or("");
        let lower = line.to_ascii_lowercase();
        let outcome = if lower.contains("cache") { "cached" } else { "played" };
        return PlayCueReply {
            outcome: outcome.to_string(),
            detail: if line.is_empty() { "Cue played.".to_string() } else { line.to_string() },
        };
    }
    let err = reply.error.as_deref().unwrap_or("command_failed");
    let lower = err.to_ascii_lowercase();
    // A closed-gate no-op: switch off / no key / offline. The daemon's copy
    // mentions these explicitly; "disabled"/"off"/"cloud_sfx" are its markers.
    let outcome = if lower.contains("cloud_sfx")
        || lower.contains("cue tier is off")
        || lower.contains("turn on")
        || lower.contains("disabled")
    {
        "disabled"
    } else if lower.contains("no built-in cue") || lower.contains("no cue") {
        // The daemon's honest cue-not-found phrasing — distinct from the
        // protocol-level `unknown_command` (which is a relay failure, not a cue).
        "unknown"
    } else {
        "failed"
    };
    PlayCueReply { outcome: outcome.to_string(), detail: err.to_string() }
}

/// Play a BUILT-IN SFX cue by NAME. Relays a bounded `play_cue {cue}` request
/// through the SAME token-injecting command socket as [`send_command`] — it adds
/// NO new authority and NO new tier. The daemon gates the cue on its
/// already-shipped `voice_tier::sfx_enabled` (`[voice].cloud_sfx` + an ElevenLabs
/// key, online) and returns an honest silent no-op when the gate is closed; this
/// command faithfully maps that into the [`PlayCueReply`] vocabulary. The cue NAME
/// is the only thing that leaves the UI; no key value or path ever crosses here.
#[tauri::command]
pub async fn play_sfx_cue(cue: String) -> Result<PlayCueReply, String> {
    let request = CommandRequest { cmd: "play_cue".to_string(), cue: Some(cue), ..Default::default() };
    let reply = send_command(request).await?;
    Ok(cue_outcome(&reply))
}

/* ------------------------------------------------------ Voice Lab (Phase-2) */

/// The honest Voice-Lab outcome the HUD maps to prose. Mirrors the daemon
/// trigger vocabulary collapsed to a small set: a genuine creation (`created`), a
/// closed-gate / offline / no-key no-op (`unavailable`), or a generic failure
/// (`failed`). It NEVER claims a creation when the daemon reported an honest `Err`.
/// `detail` is the daemon's own secret-free line (never a voice/dictionary id).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct VoiceLabReply {
    pub outcome: String,
    pub detail: String,
}

/// Map a daemon `CommandReply` (the `design_voice` / `create_pronunciation` relay
/// reply) into the bounded [`VoiceLabReply`] vocabulary. PURE — unit-tested.
///
/// HONESTY: an `ok:false` reply NEVER becomes "created". The daemon trigger
/// returns prose, so we classify on the secret-free reply/error text: any `ok:true`
/// success → `created`; a gate-closed / offline / no-key rejection (the trigger's
/// "needs the cloud tier" / "working offline" / "without an ElevenLabs key" copy) →
/// `unavailable` (an honest no-op, nothing created); anything else → `failed`. No id
/// or secret is ever forwarded — only the contracted `{outcome, detail}`.
pub fn voice_lab_outcome(reply: &CommandReply) -> VoiceLabReply {
    // The command channel returns ok:true for any DISPATCHED verb and carries the
    // trigger's honest outcome PROSE in `reply` — a closed gate / failure is reported
    // in that text, NOT via ok:false (the same shape as the SFX cue path). So we
    // classify on the secret-free line and call it "created" ONLY when the prose
    // carries no failure / gate marker: an honest no-op or failure can NEVER read as
    // "created", even though the dispatch itself returned ok:true.
    let line = if reply.ok {
        reply.reply.as_deref().unwrap_or("")
    } else {
        reply.error.as_deref().unwrap_or("command_failed")
    };
    let lower = line.to_ascii_lowercase();
    let detail = if line.is_empty() { "Done.".to_string() } else { line.to_string() };

    // A closed-gate no-op: cloud tier off / offline / no key. The daemon's own copy
    // names these; mirror its markers so an honest no-op never reads as "created".
    let outcome = if lower.contains("needs the cloud tier")
        || lower.contains("working offline")
        || lower.contains("without an elevenlabs key")
        || lower.contains("add one in settings")
    {
        "unavailable"
    } else if !reply.ok
        || lower.contains("couldn't")
        || lower.contains("could not")
        || lower.contains("can't")
        || lower.contains("didn't go through")
        || lower.contains("nothing was created")
    {
        // Any honest failure marker -> failed, even on an ok:true dispatch.
        "failed"
    } else {
        // A genuine creation: dispatched ok with clean success prose.
        "created"
    };
    VoiceLabReply { outcome: outcome.to_string(), detail }
}

/// DESIGN a voice for `agent` from a text `description` (+ an optional display
/// `name`). Relays a bounded `design_voice` request through the SAME
/// token-injecting command socket as [`send_command`] — it adds NO new authority
/// and NO new tier. The daemon gates it on its already-shipped key + non-Local-tier
/// gate and returns an honest `Err` (never a fabricated voice) when the gate is
/// closed; this command faithfully maps that into the [`VoiceLabReply`] vocabulary.
/// Only the text description leaves the UI; no key value or id ever crosses here.
#[tauri::command]
pub async fn design_voice(
    agent: String,
    description: String,
    name: Option<String>,
) -> Result<VoiceLabReply, String> {
    let request = CommandRequest {
        cmd: "design_voice".to_string(),
        agent: Some(agent),
        description: Some(description),
        name,
        ..Default::default()
    };
    let reply = send_command(request).await?;
    Ok(voice_lab_outcome(&reply))
}

/// CREATE a single-alias pronunciation rule (`word` -> `say`, under an optional
/// dictionary `name`). Relays a bounded `create_pronunciation` request through the
/// SAME token-injecting command socket as [`send_command`] — NO new authority, NO
/// new tier. The daemon gates it on its already-shipped key + non-Local-tier gate
/// and returns an honest `Err` (never a fabricated dictionary) when the gate is
/// closed; this command maps that into the [`VoiceLabReply`] vocabulary. Text rules
/// only; no audio, key value, or id ever crosses here.
#[tauri::command]
pub async fn create_pronunciation(
    word: String,
    say: String,
    name: Option<String>,
) -> Result<VoiceLabReply, String> {
    let request = CommandRequest {
        cmd: "create_pronunciation".to_string(),
        word: Some(word),
        say: Some(say),
        name,
        ..Default::default()
    };
    let reply = send_command(request).await?;
    Ok(voice_lab_outcome(&reply))
}

/* ----------------------------------------------------- Compose music (Phase-3) */

/// The honest Compose-music outcome the HUD maps to prose. Mirrors the daemon
/// trigger vocabulary collapsed to a small set: a genuine creation (`created`), a
/// closed-gate / offline / no-key no-op (`unavailable`), or a generic failure
/// (`failed`). It NEVER claims a track when the daemon reported an honest no-op /
/// failure. `detail` is the daemon's own secret-free line (never the audio path).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct MusicReply {
    pub outcome: String,
    pub detail: String,
}

/// Map a daemon `CommandReply` (the `compose_music` relay reply) into the bounded
/// [`MusicReply`] vocabulary. PURE — unit-tested.
///
/// HONESTY (FAIL-SAFE, exactly like [`voice_lab_outcome`]): the command channel
/// returns `ok:true` for any DISPATCHED verb and carries the trigger's honest
/// outcome PROSE in `reply` — a closed gate / failure is reported in that TEXT, NOT
/// via `ok:false`. So we classify on the secret-free line and call it "created"
/// ONLY when the prose carries no gate / failure marker: an honest no-op or failure
/// can NEVER read as "created", even though the dispatch itself returned ok:true. A
/// gate-closed / offline / no-key rejection → `unavailable`; anything else honestly
/// failed → `failed`. No path or secret is ever forwarded — only the contracted
/// `{outcome, detail}`.
pub fn music_outcome(reply: &CommandReply) -> MusicReply {
    let line = if reply.ok {
        reply.reply.as_deref().unwrap_or("")
    } else {
        reply.error.as_deref().unwrap_or("command_failed")
    };
    let lower = line.to_ascii_lowercase();
    let detail = if line.is_empty() { "Done.".to_string() } else { line.to_string() };

    // A closed-gate no-op: cloud music off / offline / no key. The daemon's own copy
    // names these; mirror its markers so an honest no-op never reads as "created".
    let outcome = if lower.contains("needs the cloud tier")
        || lower.contains("needs cloud music")
        || lower.contains("working offline")
        || lower.contains("without an elevenlabs key")
        || lower.contains("add one in settings")
        || lower.contains("unavailable")
    {
        "unavailable"
    } else if !reply.ok
        || lower.contains("couldn't")
        || lower.contains("could not")
        || lower.contains("can't")
        || lower.contains("didn't go through")
        || lower.contains("nothing was created")
    {
        // Any honest failure marker -> failed, even on an ok:true dispatch.
        "failed"
    } else {
        // A genuine creation: dispatched ok with clean success prose.
        "created"
    };
    MusicReply { outcome: outcome.to_string(), detail }
}

/// COMPOSE a full music track from a text `prompt` (+ an OPTIONAL `length_ms`).
/// Relays a bounded `compose_music` request through the SAME token-injecting
/// command socket as [`send_command`] — it adds NO new authority and NO new tier.
/// The daemon gates it on its already-shipped `[voice].cloud_music` + an ElevenLabs
/// key + a non-Local tier and returns an honest "unavailable" / "didn't go through,
/// nothing was created" prose (never a fabricated track) when the gate is closed;
/// this command faithfully maps that into the [`MusicReply`] vocabulary. Only the
/// text prompt (+ the optional length) leaves the UI; no key value or path ever
/// crosses here.
#[tauri::command]
pub async fn compose_music(prompt: String, length_ms: Option<u32>) -> Result<MusicReply, String> {
    let request = CommandRequest {
        cmd: "compose_music".to_string(),
        prompt: Some(prompt),
        length_ms,
        ..Default::default()
    };
    let reply = send_command(request).await?;
    Ok(music_outcome(&reply))
}

/* --------------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cmd: &str) -> CommandRequest {
        CommandRequest { cmd: cmd.to_string(), ..Default::default() }
    }

    #[test]
    fn build_request_admits_only_the_allowlist() {
        // Every allowlisted verb with its required fields builds a valid object.
        let ok = [
            { let mut r = req("ask"); r.text = Some("hi".into()); r },
            req("brief"),
            { let mut r = req("mission"); r.goal = Some("do x".into()); r },
            req("roster"),
            req("state"),
            req("pending"),
            { let mut r = req("confirm"); r.id = Some("abc".into()); r },
            { let mut r = req("deny"); r.id = Some("abc".into()); r },
            { let mut r = req("dismiss_forge"); r.ts = Some(42); r },
            { let mut r = req("policy"); r.text = Some("always allow the gmail_send action".into()); r },
            // Task #12 — the bare emergency-stop verbs build with just {token, cmd}.
            req("panic"),
            req("unlock"),
            // Phase-2 SFX cue — carries the cue name.
            { let mut r = req("play_cue"); r.cue = Some("confirm".into()); r },
            // Phase-2 Voice Lab — design_voice carries agent + a long-enough
            // description; create_pronunciation carries word + say.
            {
                let mut r = req("design_voice");
                r.agent = Some("friday".into());
                r.description = Some("a calm, warm British concierge voice".into());
                r
            },
            {
                let mut r = req("create_pronunciation");
                r.word = Some("DARWIN".into());
                r.say = Some("jar viss".into());
                r
            },
            // Phase-3 Compose music — carries a non-empty prompt (length optional).
            {
                let mut r = req("compose_music");
                r.prompt = Some("an 8-bit happy birthday".into());
                r
            },
        ];
        for r in ok {
            let v = build_request(&r, "TOK").expect("known verb builds");
            assert_eq!(v["cmd"], r.cmd);
            assert_eq!(v["token"], "TOK", "token is injected by the backend");
        }
        // Unknown / privileged-sounding verbs are rejected before any I/O.
        for cmd in ["apply_forge", "deploy", "exec", "", "shutdown", "set_switch"] {
            assert_eq!(build_request(&req(cmd), "TOK"), Err("unknown_command".into()));
        }
    }

    #[test]
    fn build_request_enforces_required_fields() {
        assert!(build_request(&req("ask"), "T").is_err()); // no text
        let mut blank = req("ask");
        blank.text = Some("   ".into());
        assert!(build_request(&blank, "T").is_err()); // whitespace text
        assert!(build_request(&req("mission"), "T").is_err()); // no goal
        assert!(build_request(&req("confirm"), "T").is_err()); // no id
        assert!(build_request(&req("deny"), "T").is_err()); // no id
        assert!(build_request(&req("dismiss_forge"), "T").is_err()); // no ts
        assert!(build_request(&req("policy"), "T").is_err()); // no phrase text
        let mut blank_policy = req("policy");
        blank_policy.text = Some("   ".into());
        assert!(build_request(&blank_policy, "T").is_err()); // whitespace phrase
        assert!(build_request(&req("play_cue"), "T").is_err()); // no cue name
        let mut blank_cue = req("play_cue");
        blank_cue.cue = Some("   ".into());
        assert!(build_request(&blank_cue, "T").is_err()); // whitespace cue

        // Voice Lab — design_voice needs an agent AND a long-enough description.
        assert!(build_request(&req("design_voice"), "T").is_err()); // no agent
        let mut no_desc = req("design_voice");
        no_desc.agent = Some("friday".into());
        assert!(build_request(&no_desc, "T").is_err()); // no description
        let mut short_desc = req("design_voice");
        short_desc.agent = Some("friday".into());
        short_desc.description = Some("too short".into()); // < the EL floor
        assert!(build_request(&short_desc, "T").is_err());
        let mut blank_agent = req("design_voice");
        blank_agent.agent = Some("   ".into());
        blank_agent.description = Some("a calm, warm British concierge voice".into());
        assert!(build_request(&blank_agent, "T").is_err()); // whitespace agent

        // Voice Lab — create_pronunciation needs BOTH word and say.
        assert!(build_request(&req("create_pronunciation"), "T").is_err()); // neither
        let mut no_say = req("create_pronunciation");
        no_say.word = Some("DARWIN".into());
        assert!(build_request(&no_say, "T").is_err()); // no say
        let mut no_word = req("create_pronunciation");
        no_word.say = Some("jar viss".into());
        assert!(build_request(&no_word, "T").is_err()); // no word
        let mut blank_say = req("create_pronunciation");
        blank_say.word = Some("DARWIN".into());
        blank_say.say = Some("   ".into());
        assert!(build_request(&blank_say, "T").is_err()); // whitespace say

        // Compose music — needs a non-empty prompt (the length is optional).
        assert!(build_request(&req("compose_music"), "T").is_err()); // no prompt
        let mut blank_prompt = req("compose_music");
        blank_prompt.prompt = Some("   ".into());
        assert!(build_request(&blank_prompt, "T").is_err()); // whitespace prompt
    }

    #[test]
    fn build_request_design_voice_carries_only_the_text_fields() {
        let mut r = req("design_voice");
        r.agent = Some("  friday  ".into());
        r.description = Some("  a calm, warm British concierge voice  ".into());
        r.name = Some("Concierge".into());
        let v = build_request(&r, "TOK").unwrap();
        assert_eq!(v["cmd"], "design_voice");
        // Agent is trimmed; the description is trimmed; the name rides when present.
        assert_eq!(v["agent"], "friday");
        assert_eq!(v["description"], "a calm, warm British concierge voice");
        assert_eq!(v["name"], "Concierge");
        assert_eq!(v["token"], "TOK", "token injected by the backend");
        // ONLY token + cmd + agent + description + name — no key/id/path field.
        assert_eq!(v.as_object().unwrap().len(), 5);

        // A blank name is OMITTED entirely (the daemon defaults it to the agent).
        let mut no_name = req("design_voice");
        no_name.agent = Some("friday".into());
        no_name.description = Some("a calm, warm British concierge voice".into());
        no_name.name = Some("   ".into());
        let v = build_request(&no_name, "T").unwrap();
        assert!(v.get("name").is_none(), "blank name omitted");
        assert_eq!(v.as_object().unwrap().len(), 4); // token+cmd+agent+description
    }

    #[test]
    fn build_request_create_pronunciation_carries_only_the_text_rule() {
        let mut r = req("create_pronunciation");
        r.word = Some("  DARWIN  ".into());
        r.say = Some("  jar viss  ".into());
        r.name = Some("My dictionary".into());
        let v = build_request(&r, "TOK").unwrap();
        assert_eq!(v["cmd"], "create_pronunciation");
        assert_eq!(v["word"], "DARWIN");
        assert_eq!(v["say"], "jar viss");
        assert_eq!(v["name"], "My dictionary");
        assert_eq!(v["token"], "TOK", "token injected by the backend");
        // ONLY token + cmd + word + say + name — no key/id/audio field.
        assert_eq!(v.as_object().unwrap().len(), 5);

        // A blank name is OMITTED (the daemon defaults it to a fixed label).
        let mut no_name = req("create_pronunciation");
        no_name.word = Some("DARWIN".into());
        no_name.say = Some("jar viss".into());
        let v = build_request(&no_name, "T").unwrap();
        assert!(v.get("name").is_none(), "blank name omitted");
        assert_eq!(v.as_object().unwrap().len(), 4); // token+cmd+word+say
    }

    #[test]
    fn voice_lab_outcome_maps_the_daemon_reply_honestly() {
        // A successful creation → created (the agent now speaks with it).
        let designed = parse_reply(r#"{"ok":true,"reply":"Designed and saved the Concierge voice for friday."}"#);
        assert_eq!(voice_lab_outcome(&designed).outcome, "created");
        let pron = parse_reply(r#"{"ok":true,"reply":"Created the pronunciation rule: say \"DARWIN\" as \"jar viss\"."}"#);
        assert_eq!(voice_lab_outcome(&pron).outcome, "created");
        // A gate-closed rejection NEVER reads as created — it is an honest no-op.
        let offline = parse_reply(
            r#"{"ok":false,"error":"Designing a voice needs the cloud tier, but you're working offline — nothing was created."}"#,
        );
        assert_eq!(voice_lab_outcome(&offline).outcome, "unavailable");
        let no_key = parse_reply(
            r#"{"ok":false,"error":"I can't design a voice without an ElevenLabs key — add one in Settings. No voice was created."}"#,
        );
        assert_eq!(voice_lab_outcome(&no_key).outcome, "unavailable");
        // A generation/persist failure → failed, never a fabricated success.
        let failed = parse_reply(
            r#"{"ok":false,"error":"I couldn't design that voice just now — the cloud request didn't go through. Nothing was created."}"#,
        );
        assert_eq!(voice_lab_outcome(&failed).outcome, "failed");
        // A protocol-level relay failure also reads as failed (never created).
        let relay = parse_reply(r#"{"ok":false,"error":"unknown_command"}"#);
        assert_eq!(voice_lab_outcome(&relay).outcome, "failed");
        // FAIL-SAFE: the command channel returns ok:TRUE for any dispatched verb,
        // carrying a gated/failure outcome in the PROSE. Such a reply must NOT read
        // as "created" — we classify on the text, never the bare ok flag.
        let ok_true_offline = parse_reply(
            r#"{"ok":true,"reply":"Designing a voice needs the cloud tier, but you're working offline — nothing was created."}"#,
        );
        assert_eq!(
            voice_lab_outcome(&ok_true_offline).outcome, "unavailable",
            "ok:true with gated prose is an honest no-op, never created"
        );
        let ok_true_failed = parse_reply(
            r#"{"ok":true,"reply":"I couldn't design that voice just now — the cloud request didn't go through. Nothing was created."}"#,
        );
        assert_eq!(
            voice_lab_outcome(&ok_true_failed).outcome, "failed",
            "ok:true with failure prose is failed, never created"
        );
        // The mapped detail never carries an id/secret-shaped marker.
        for r in [&designed, &pron, &offline, &no_key, &failed, &relay, &ok_true_offline, &ok_true_failed] {
            let mapped = voice_lab_outcome(r);
            assert!(!mapped.detail.contains("sk-"));
            assert!(!mapped.detail.contains(".wav"));
        }
    }

    #[test]
    fn build_request_compose_music_carries_only_the_prompt_and_clamped_length() {
        // Prompt trimmed; a given length rides as a clamped length_ms; no key/path.
        let mut r = req("compose_music");
        r.prompt = Some("  an 8-bit happy birthday  ".into());
        r.length_ms = Some(30_000);
        let v = build_request(&r, "TOK").unwrap();
        assert_eq!(v["cmd"], "compose_music");
        assert_eq!(v["prompt"], "an 8-bit happy birthday");
        assert_eq!(v["length_ms"], 30_000);
        assert_eq!(v["token"], "TOK", "token injected by the backend");
        // ONLY token + cmd + prompt + length_ms ride the wire — no key/path field.
        assert_eq!(v.as_object().unwrap().len(), 4);

        // A blank length is OMITTED entirely (the daemon defaults it).
        let mut no_len = req("compose_music");
        no_len.prompt = Some("lo-fi beat".into());
        let v = build_request(&no_len, "T").unwrap();
        assert!(v.get("length_ms").is_none(), "absent length omitted");
        assert_eq!(v.as_object().unwrap().len(), 3); // token+cmd+prompt

        // An out-of-band length is CLAMPED, never rejected: too short -> floor,
        // too long -> ceiling.
        let mut short = req("compose_music");
        short.prompt = Some("x".into());
        short.length_ms = Some(1);
        assert_eq!(build_request(&short, "T").unwrap()["length_ms"], MIN_LENGTH_MS);
        let mut long = req("compose_music");
        long.prompt = Some("x".into());
        long.length_ms = Some(u32::MAX);
        assert_eq!(build_request(&long, "T").unwrap()["length_ms"], MAX_LENGTH_MS);
    }

    #[test]
    fn music_outcome_maps_the_daemon_reply_honestly() {
        // A successful composition → created (a real track was generated).
        let composed = parse_reply(r#"{"ok":true,"reply":"Composed a 30s track from \"an 8-bit happy birthday\"."}"#);
        assert_eq!(music_outcome(&composed).outcome, "created");
        // A gate-closed / offline / no-key rejection NEVER reads as created.
        let offline = parse_reply(
            r#"{"ok":false,"error":"Composing music needs the cloud tier, but you're working offline — nothing was created."}"#,
        );
        assert_eq!(music_outcome(&offline).outcome, "unavailable");
        let no_key = parse_reply(
            r#"{"ok":false,"error":"I can't compose music without an ElevenLabs key — add one in Settings. Nothing was created."}"#,
        );
        assert_eq!(music_outcome(&no_key).outcome, "unavailable");
        let unavail = parse_reply(r#"{"ok":false,"error":"Music generation is unavailable right now."}"#);
        assert_eq!(music_outcome(&unavail).outcome, "unavailable");
        // A generation failure → failed, never a fabricated success.
        let failed = parse_reply(
            r#"{"ok":false,"error":"I couldn't compose that track just now — the cloud request didn't go through. Nothing was created."}"#,
        );
        assert_eq!(music_outcome(&failed).outcome, "failed");
        // A protocol-level relay failure also reads as failed (never created).
        let relay = parse_reply(r#"{"ok":false,"error":"unknown_command"}"#);
        assert_eq!(music_outcome(&relay).outcome, "failed");
        // FAIL-SAFE: the command channel returns ok:TRUE for any dispatched verb,
        // carrying a gated/failure outcome in the PROSE. Such a reply must NOT read
        // as "created" — we classify on the text, never the bare ok flag.
        let ok_true_offline = parse_reply(
            r#"{"ok":true,"reply":"Composing music needs the cloud tier, but you're working offline — nothing was created."}"#,
        );
        assert_eq!(
            music_outcome(&ok_true_offline).outcome, "unavailable",
            "ok:true with gated prose is an honest no-op, never created"
        );
        let ok_true_failed = parse_reply(
            r#"{"ok":true,"reply":"I couldn't compose that track just now — the cloud request didn't go through. Nothing was created."}"#,
        );
        assert_eq!(
            music_outcome(&ok_true_failed).outcome, "failed",
            "ok:true with failure prose is failed, never created"
        );
        // The mapped detail never carries a path/secret-shaped marker.
        for r in [&composed, &offline, &no_key, &unavail, &failed, &relay, &ok_true_offline, &ok_true_failed] {
            let mapped = music_outcome(r);
            assert!(!mapped.detail.contains("sk-"));
            assert!(!mapped.detail.contains(".wav"));
            assert!(!mapped.detail.contains(".mp3"));
        }
    }

    #[test]
    fn build_request_play_cue_carries_only_the_normalized_cue_name() {
        let mut r = req("play_cue");
        r.cue = Some("  Confirm  ".into());
        let v = build_request(&r, "TOK").unwrap();
        assert_eq!(v["cmd"], "play_cue");
        // Normalized to the catalog atom (trimmed, lowercased) for an exact lookup.
        assert_eq!(v["cue"], "confirm");
        assert_eq!(v["token"], "TOK", "token injected by the backend");
        // ONLY token + cmd + cue ride the wire — no key/prompt/path field.
        assert_eq!(v.as_object().unwrap().len(), 3);
    }

    #[test]
    fn cue_outcome_maps_the_daemon_reply_honestly() {
        // A successful generation → played; a cache-mentioning success → cached.
        let played = parse_reply(r#"{"ok":true,"reply":"Cue played."}"#);
        assert_eq!(cue_outcome(&played).outcome, "played");
        let cached = parse_reply(r#"{"ok":true,"reply":"Served from cache — no cloud call."}"#);
        assert_eq!(cue_outcome(&cached).outcome, "cached");
        // A closed-gate rejection NEVER reads as played — it is an honest no-op.
        let off = parse_reply(
            r#"{"ok":false,"error":"Cue tier is off. Turn on [voice].cloud_sfx and add an ElevenLabs key."}"#,
        );
        assert_eq!(cue_outcome(&off).outcome, "disabled");
        // An unknown cue is an honest no-cue, not a play.
        let unknown = parse_reply(r#"{"ok":false,"error":"There's no built-in cue called 'kaboom'."}"#);
        assert_eq!(cue_outcome(&unknown).outcome, "unknown");
        // Anything else (e.g. the daemon hasn't wired the verb yet) → failed,
        // never a fabricated success.
        let other = parse_reply(r#"{"ok":false,"error":"unknown_command"}"#);
        assert_eq!(cue_outcome(&other).outcome, "failed");
        // The mapped detail never carries a path/secret-shaped marker.
        for r in [&played, &cached, &off, &unknown, &other] {
            let mapped = cue_outcome(r);
            assert!(!mapped.detail.contains(".wav"));
            assert!(!mapped.detail.contains("sk-"));
        }
    }

    #[test]
    fn build_request_carries_the_policy_phrase_verbatim() {
        let mut r = req("policy");
        r.text = Some("never allow the x_post action".into());
        let v = build_request(&r, "T").unwrap();
        assert_eq!(v["cmd"], "policy");
        assert_eq!(v["text"], "never allow the x_post action");
        assert_eq!(v["token"], "T", "token injected by the backend");
    }

    #[test]
    fn build_request_carries_the_agent_only_when_present() {
        let mut with = req("ask");
        with.text = Some("status".into());
        with.agent = Some("edith".into());
        let v = build_request(&with, "T").unwrap();
        assert_eq!(v["agent"], "edith");

        // A blank agent is dropped (routes to the orchestrator daemon-side).
        let mut blank = req("ask");
        blank.text = Some("status".into());
        blank.agent = Some("   ".into());
        let v = build_request(&blank, "T").unwrap();
        assert!(v.get("agent").is_none(), "blank agent omitted");
    }

    #[test]
    fn build_request_clamps_oversized_text() {
        let mut r = req("ask");
        r.text = Some("a".repeat(MAX_TEXT_CHARS + 500));
        let v = build_request(&r, "T").unwrap();
        assert_eq!(
            v["text"].as_str().unwrap().chars().count(),
            MAX_TEXT_CHARS,
            "text clamped to the cap before the wire"
        );
    }

    #[test]
    fn parse_reply_narrows_ok_prose() {
        let r = parse_reply(r#"{"ok":true,"reply":"Roll call complete."}"#);
        assert!(r.ok);
        assert_eq!(r.reply.as_deref(), Some("Roll call complete."));
        assert!(r.error.is_none());
        assert!(r.pending.is_none());
    }

    #[test]
    fn parse_reply_narrows_pending_snapshot() {
        let r = parse_reply(
            r#"{"ok":true,"pending":{"confirmation":{"id":"deadbeef","agent":"agent.pepper","tool":"gmail_send","preview":"Would email Alice"},"forge_pending_ts":"1770000000"}}"#,
        );
        assert!(r.ok);
        let p = r.pending.expect("pending present");
        let c = p.confirmation.expect("confirmation present");
        assert_eq!(c.id, "deadbeef");
        assert_eq!(c.tool, "gmail_send");
        assert_eq!(c.preview, "Would email Alice");
        assert_eq!(p.forge_pending_ts.as_deref(), Some("1770000000"));
    }

    #[test]
    fn parse_reply_coerces_a_numeric_forge_ts() {
        let r = parse_reply(r#"{"ok":true,"pending":{"confirmation":null,"forge_pending_ts":1770000000}}"#);
        let p = r.pending.unwrap();
        assert!(p.confirmation.is_none(), "null confirmation -> no card");
        assert_eq!(p.forge_pending_ts.as_deref(), Some("1770000000"));
    }

    #[test]
    fn parse_reply_surfaces_daemon_rejections() {
        for err in ["unauthorized", "unknown_command", "rate_limited", "oversized", "malformed"] {
            let line = format!(r#"{{"ok":false,"error":"{err}"}}"#);
            let r = parse_reply(&line);
            assert!(!r.ok);
            assert_eq!(r.error.as_deref(), Some(err));
        }
    }

    #[test]
    fn build_request_admits_the_bare_panic_and_unlock_verbs() {
        // Task #12: panic/unlock are DEDICATED bare verbs — they build with just
        // {token, cmd} and carry NO payload (the daemon calls lockdown directly).
        for cmd in ["panic", "unlock"] {
            let v = build_request(&req(cmd), "TOK").expect("bare verb builds");
            assert_eq!(v["cmd"], cmd);
            assert_eq!(v["token"], "TOK");
            // No stray fields beyond token + cmd.
            assert_eq!(v.as_object().unwrap().len(), 2, "{cmd} carries only token+cmd");
        }
    }

    #[test]
    fn parse_reply_surfaces_the_lockdown_locked_flag() {
        // Task #12: the panic reply flips the indicator -> locked true; the unlock
        // reply -> locked false. Every other verb omits the field (stays None).
        let panic = parse_reply(r#"{"ok":true,"reply":"Lockdown engaged.","locked":true}"#);
        assert!(panic.ok);
        assert_eq!(panic.locked, Some(true));
        let unlock = parse_reply(r#"{"ok":true,"reply":"Lockdown lifted.","locked":false}"#);
        assert_eq!(unlock.locked, Some(false));
        // A plain reply (no locked field) leaves it absent so it never falsely flips.
        let plain = parse_reply(r#"{"ok":true,"reply":"Roll call complete."}"#);
        assert_eq!(plain.locked, None);
    }

    #[test]
    fn parse_reply_never_throws_on_junk() {
        for junk in ["", "   ", "not json", "[1,2,3]", "{", "null", "42"] {
            let r = parse_reply(junk);
            assert!(!r.ok, "junk yields a clean error for {junk:?}");
            assert!(r.error.is_some());
        }
    }

    #[test]
    fn parse_reply_drops_a_confirmation_with_no_id() {
        // A confirmation object lacking a usable id is not surfaced as a card.
        let r = parse_reply(r#"{"ok":true,"pending":{"confirmation":{"id":"","tool":"x"}}}"#);
        let p = r.pending.unwrap();
        assert!(p.confirmation.is_none());
    }

    #[test]
    fn reply_serialization_never_carries_a_token_or_extra_fields() {
        // Even if the daemon echoed a token (it must not), parse_reply rebuilds
        // by name, so the serialized reply has no token/secret-shaped field.
        let r = parse_reply(r#"{"ok":true,"reply":"hi","token":"LEAK","secret":"sk-XXX"}"#);
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("LEAK"));
        assert!(!s.contains("sk-XXX"));
        assert!(!s.contains("token"));
    }
}
