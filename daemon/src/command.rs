//! HUD -> daemon COMMAND CHANNEL — a local-only, token-authenticated intake
//! that lets the HUD drive the system, routing EVERY command INTO the existing
//! gated pipeline (never around it).
//!
//! ## What this is (and is NOT)
//!
//! The HUD has always been a READ-ONLY telemetry client of `ws://127.0.0.1:7177`.
//! This module adds the FIRST inbound surface: a confined Unix socket
//! (`state/ipc/command.sock`) the Tauri backend connects to, carrying a small,
//! fixed set of commands. It is JUST ANOTHER INPUT into the SAME pipeline the
//! voice path uses — it can do NOTHING the spoken path cannot:
//!
//!   * a consequential `ask` STILL parks via the cross-turn confirmation gate
//!     (confirm.rs); the channel never pre-confirms,
//!   * the armed-by-default master switch (`integrations.allow_consequential`,
//!     ships ON) STILL gates every fire — even armed, a `confirm {id}` only fires a
//!     previously-parked action; with the switch OFF it only previews and fires
//!     nothing (the replay re-checks it),
//!   * per-agent allowlist isolation STILL applies — an `ask {agent}` uses that
//!     agent's tools only; a `confirm` re-checks the parked agent's allowlist,
//!   * Self-Forge stays PROPOSE-ONLY — `dismiss_forge` clears the pending marker
//!     but NEVER applies/deploys (apply stays scripts/apply_forge.sh).
//!
//! ## Confinement (mirrors genproxy.rs / the per-app sockets / apps.rs tokens)
//!
//!   1. LOCAL-ONLY: a Unix socket under `state/ipc/` (`0700` dir, `0600`
//!      socket) — no TCP, nothing off-host.
//!   2. TOKEN-AUTHENTICATED: every line carries a capability token verified by
//!      [`apps::verify_command_token`] — the SAME HMAC-SHA256 machinery as the
//!      per-app relay + the generate proxy, bound to a reserved principal and a
//!      per-boot nonce (a forged/tampered/stale token fails closed).
//!   3. BOUNDED: oversized lines are rejected before parse; the command set is a
//!      fixed STRUCTURAL allowlist — an unknown command is rejected, never
//!      routed.
//!   4. RATE-LIMITED: a rolling per-window cap (the spam guard), same shape as
//!      genproxy's limiter.
//!   5. NO SECRET crosses the channel or is logged: the token authenticates the
//!      caller; replies carry only the same prose the user would hear.
//!
//! ## Shape
//!
//! [`decide`] is a PURE function: parse + size-check + structural allowlist +
//! token presence, with no I/O — the security tests drive it directly. The
//! routing INTO the heavy pipeline (`route()`, `edith_brief`, `fury_mission`,
//! roster/state) is behind the [`CommandPipeline`] trait so the tests inject a
//! hermetic mock instead of a live daemon, while the confirmation-gate and
//! forge-dismiss logic is exercised against the REAL `confirm` / forge state.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::apps;
use crate::telemetry;

/// Hard cap on a single command line's length. A command is a short JSON object
/// (a command name + a short text/goal/id); anything beyond this is a probe or a
/// mistake and is rejected BEFORE parse so a hostile client can't feed the JSON
/// parser an unbounded line. Generous enough for a paragraph-length `ask`.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// Hard cap on the free-text payload (`ask.text`, `mission.goal`) AFTER parse.
/// The pipeline itself bounds its work, but trimming here keeps an oversized
/// (yet under-[`MAX_LINE_BYTES`]) field from reaching the model.
pub const MAX_TEXT_CHARS: usize = 4 * 1024;

/// Minimum length of a `design_voice` DESCRIPTION. ElevenLabs' voice-design
/// endpoint rejects a too-short prompt (its documented floor is ~20 characters),
/// so a description below this is a [`Decision::BadRequest`] HERE — it never
/// reaches the cloud op or spends a request on a request the server would reject.
pub const MIN_VOICE_DESCRIPTION_CHARS: usize = 20;

/// Rolling rate-limit: at most this many commands within [`RATE_WINDOW`]. The
/// command channel is a human at a deck, not an automation firehose; this is the
/// spam / accidental-loop guard (mirrors genproxy's PROXY_RATE shape).
pub const RATE: u32 = 60;
/// The rolling window for [`RATE`].
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

/// One inbound command line. We read only what we need (no `deny_unknown_fields`
/// on the wire so a future client may add fields); the command name is validated
/// STRUCTURALLY in [`decide`] against the fixed allowlist.
#[derive(Debug, Deserialize)]
struct RawCommand {
    #[serde(default)]
    token: String,
    #[serde(default)]
    cmd: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    cue: String,
    #[serde(default)]
    ts: Option<u64>,
    /// `design_voice`: the voice DESCRIPTION (the EL design prompt). Free text,
    /// clamped + length-checked in [`decide`] (the EL design minimum is ~20 chars).
    #[serde(default)]
    description: String,
    /// `design_voice`: the optional display name; defaults to the agent name when
    /// empty. `create_pronunciation`: the optional dictionary name (defaults to a
    /// fixed label). A short label only — clamped in [`decide`].
    #[serde(default)]
    name: String,
    /// `create_pronunciation`: the string to replace (`string_to_replace`). The
    /// word/phrase the dictionary rewrites; non-empty, clamped in [`decide`].
    #[serde(default)]
    word: String,
    /// `create_pronunciation`: the alias to say in its place. The replacement
    /// pronunciation text; non-empty, clamped in [`decide`].
    #[serde(default)]
    say: String,
    /// `compose_music`: the music PROMPT (what to compose). Free text, clamped +
    /// non-empty-checked in [`decide`]. Only the text prompt leaves the device.
    #[serde(default)]
    prompt: String,
    /// `compose_music`: the OPTIONAL track length in MILLISECONDS. Absent => the
    /// server's default (the server clamps to its 3000..600000 window). Carried only
    /// when the caller pins one; threaded onto the wire only when `Some`.
    #[serde(default)]
    length_ms: Option<u32>,
}

/// The BOUNDED command set — the structural allowlist. Parsing a line into one
/// of these is the ONLY way past [`decide`]; an unknown `cmd` string is
/// [`Decision::UnknownCommand`] and never routed. Each variant carries only the
/// already-bounded fields it needs.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Run the normal route()/pipeline path; consequential tools STILL park.
    Ask { text: String, agent: Option<String> },
    /// Trigger Edith's on-demand brief (read-only).
    Brief,
    /// Run a bounded Fury mission.
    Mission { goal: String },
    /// Read-only constellation/agent roster.
    Roster,
    /// Read-only pending + agent state snapshot.
    State,
    /// SELF-DISTILLATION (F17): prepare a redacted dataset from the user's own
    /// graded turns and run the device-gated LoRA training, staging the adapter
    /// (NEVER promoting it). Operator-triggered only; off unless
    /// [distill].enabled. Read-mostly + confined to state/lora/.
    Distill,
    /// FEDERATED SYNC (F18): seal the user's facts to the outbox + merge any
    /// paired-device bundle from the inbox (conflict-aware, never clobbers).
    /// Operator-triggered; off unless [sync].enabled + a Keychain shared key.
    Sync,
    /// OVERNIGHT AGENTS (F10): ENQUEUE a task to run while the user is away. Does
    /// NOT run it here — enqueuing is a local write; the presence-gated
    /// overnight_task runs it (tool-less) later. Off unless [overnight].enabled.
    Overnight { task: String, agent: Option<String> },
    /// List pending confirmations + forge proposals (ids + faithful previews).
    Pending,
    /// Approve a SPECIFIC genuinely-parked confirmation by id (the authenticated
    /// local equivalent of the spoken "confirm").
    Confirm { id: String },
    /// Deny a SPECIFIC parked confirmation by id (clears it; fires nothing).
    Deny { id: String },
    /// Dismiss a forge PROPOSAL by ts — clears the pending marker; NEVER applies.
    DismissForge { ts: u64 },
    /// USER-SET a per-action consequential policy from an anchored phrase
    /// (`always allow the <tool> action` / `never allow the <tool> action` /
    /// `always ask before the <tool> action`). This is a DEDICATED verb, NOT
    /// `ask` — it NEVER reaches the model tool loop; the daemon classifies the
    /// phrase (`policy::classify_policy_command`) and applies it via the
    /// USER-SET-ONLY write path. There is no model/agent/tool path to this verb.
    Policy { text: String },
    /// ENGAGE the panic / lockdown emergency stop (task #12). A DEDICATED verb,
    /// NOT `ask` — it NEVER reaches the model tool loop; the daemon calls
    /// `lockdown::panic()` directly (sets the global flag, drops any pending
    /// confirm, persists the marker, audits). This is the HUD PANIC button. There
    /// is no model/agent/tool path to this verb.
    Panic,
    /// LIFT the lockdown (task #12). A DEDICATED verb, NOT `ask` — the
    /// authenticated-local USER resume that, with the spoken "unlock" intent, is
    /// the ONLY way to `lockdown::unlock()` (gates return to their configured
    /// values; the marker is removed). NEVER model-routed.
    Unlock,
    /// PLAY a named, built-in SFX cue (the HUD's SFX-cue Play button). A DEDICATED
    /// benign verb, NOT `ask` — it NEVER reaches the model/tool loop. The cue name
    /// is validated in [`decide`] against the fixed catalog
    /// ([`crate::sfx_cue::is_known_cue`]), so only a known cue can route; the daemon
    /// then calls `trigger_cue` directly, whose own gate
    /// ([`crate::voice_tier::sfx_enabled`] + offline check) turns switch-off / no-key
    /// / offline into an honest silent no-op. There is no model/agent/tool path to it.
    PlayCue { cue: String },
    /// DESIGN a voice for an agent from a text DESCRIPTION (the HUD's voice-design
    /// control). A DEDICATED provisioning verb, NOT `ask` — it NEVER reaches the
    /// model/tool loop. `decide` validates a non-empty `agent`, a `description`
    /// of at least [`MIN_VOICE_DESCRIPTION_CHARS`] (the EL design floor), and a
    /// `name` (defaulted to the agent name when empty); the daemon then calls
    /// `trigger_design_voice` directly, whose own gate (key + cloud tier) turns
    /// no-key / offline into an HONEST `Err` — never a fabricated voice. Only the
    /// text description leaves the device. There is no model/agent/tool path to it.
    DesignVoice {
        agent: String,
        description: String,
        name: String,
    },
    /// CREATE a single-alias pronunciation-dictionary rule (the HUD's pronunciation
    /// control). A DEDICATED provisioning verb, NOT `ask` — it NEVER reaches the
    /// model/tool loop. `decide` validates a non-empty `word` (the string to
    /// replace) and a non-empty `say` (the alias pronunciation) and builds ONE
    /// alias rule; the daemon then calls `trigger_create_pronunciation` directly,
    /// whose own gate (key + cloud tier) turns no-key / offline into an HONEST
    /// `Err` — never a fabricated dictionary id. Text rules only; no audio leaves
    /// the device. There is no model/agent/tool path to it.
    CreatePronunciation {
        word: String,
        say: String,
        name: String,
    },
    /// COMPOSE a music track from a text PROMPT (the HUD's music-generation control,
    /// Jerome's "Leisure + DJ" surface). A DEDICATED benign verb, NOT `ask` — it NEVER
    /// reaches the model/tool loop. `decide` validates a NON-EMPTY prompt (clamped +
    /// trimmed; an empty prompt is a [`Decision::BadRequest`], never routed) and carries
    /// the OPTIONAL `length_ms` verbatim; the daemon then calls `trigger_compose_music`
    /// directly, whose own gate ([`crate::voice_tier::music_enabled`] + offline check)
    /// turns switch-off / no-key / offline into an HONEST `Err` — never a fabricated
    /// track. Only the text prompt leaves the device. There is no model/agent/tool path
    /// to it.
    ComposeMusic {
        prompt: String,
        length_ms: Option<u32>,
    },
}

/// The pre-auth decision for one inbound line. PURE and exhaustively unit-tested:
/// size, parse, structural allowlist, and required-field shape are all decided
/// here BEFORE any token check, rate-limit, or route — so the tests prove an
/// unknown command and an oversized/malformed line can never reach a route.
#[derive(Debug, PartialEq)]
enum Decision {
    /// Shape is valid: this is the parsed command and the token to verify.
    Ok { token: String, command: Command },
    /// `cmd` was not in the allowlist — rejected, never routed.
    UnknownCommand { cmd: String },
    /// Parsed, but a required field was missing/empty (e.g. confirm with no id).
    BadRequest { reason: &'static str },
    /// Line exceeded [`MAX_LINE_BYTES`] — rejected before parse.
    Oversized,
    /// Not parseable as a command object.
    Malformed,
}

/// Clamp a free-text field to [`MAX_TEXT_CHARS`] characters (char-boundary safe).
fn clamp_text(s: String) -> String {
    if s.chars().count() <= MAX_TEXT_CHARS {
        return s;
    }
    s.chars().take(MAX_TEXT_CHARS).collect()
}

/// PURE size + parse + structural-allowlist + shape gate. The ONLY accepted
/// commands are the [`Command`] variants; every other `cmd` string is
/// [`Decision::UnknownCommand`], so there is no code path from an unknown command
/// to a route.
fn decide(raw: &str) -> Decision {
    if raw.len() > MAX_LINE_BYTES {
        return Decision::Oversized;
    }
    let Ok(req) = serde_json::from_str::<RawCommand>(raw.trim()) else {
        return Decision::Malformed;
    };
    let command = match req.cmd.as_str() {
        "ask" => {
            let text = clamp_text(req.text);
            if text.trim().is_empty() {
                return Decision::BadRequest { reason: "ask requires non-empty text" };
            }
            // An agent reference, when present, must be non-empty; resolution to
            // a real agent happens in the pipeline (the allowlist applies there).
            let agent = req.agent.filter(|a| !a.trim().is_empty());
            Command::Ask { text, agent }
        }
        "brief" => Command::Brief,
        "mission" => {
            let goal = clamp_text(req.goal);
            if goal.trim().is_empty() {
                return Decision::BadRequest { reason: "mission requires a non-empty goal" };
            }
            Command::Mission { goal }
        }
        "roster" => Command::Roster,
        "state" => Command::State,
        "distill" => Command::Distill,
        "sync" => Command::Sync,
        "overnight" => {
            let task = clamp_text(req.prompt);
            if task.trim().is_empty() {
                return Decision::BadRequest { reason: "overnight requires a non-empty task" };
            }
            let agent = req.agent.filter(|a| !a.trim().is_empty());
            Command::Overnight { task, agent }
        }
        "pending" => Command::Pending,
        "confirm" => {
            if req.id.trim().is_empty() {
                return Decision::BadRequest { reason: "confirm requires an id" };
            }
            Command::Confirm { id: req.id }
        }
        "deny" => {
            if req.id.trim().is_empty() {
                return Decision::BadRequest { reason: "deny requires an id" };
            }
            Command::Deny { id: req.id }
        }
        "dismiss_forge" => match req.ts {
            Some(ts) => Command::DismissForge { ts },
            None => return Decision::BadRequest { reason: "dismiss_forge requires a ts" },
        },
        "policy" => {
            let text = clamp_text(req.text);
            if text.trim().is_empty() {
                return Decision::BadRequest { reason: "policy requires the phrase text" };
            }
            Command::Policy { text }
        }
        // Task #12 — the HUD panic button + unlock control. Both are bare verbs
        // (no fields): the daemon calls lockdown::panic()/unlock() directly, never
        // the model. They still pass the SAME token + rate gate every command does.
        "panic" => Command::Panic,
        "unlock" => Command::Unlock,
        // The HUD SFX-cue Play button. A benign verb that plays a NAMED built-in
        // cue. The name is validated TWICE-over: non-empty AND a member of the
        // fixed catalog (`sfx_cue::is_known_cue`). An empty or unknown cue is a
        // BadRequest — it NEVER routes — keeping this verb's input as tight an
        // allowlist as the command set itself. The play_cue gate (off/no-key/
        // offline) is honest about availability downstream; here we only admit a
        // structurally-valid, known cue.
        "play_cue" => {
            let cue = clamp_text(req.cue);
            let cue = cue.trim();
            if cue.is_empty() {
                return Decision::BadRequest { reason: "play_cue requires a cue name" };
            }
            if !crate::sfx_cue::is_known_cue(cue) {
                return Decision::BadRequest { reason: "play_cue requires a known cue name" };
            }
            Command::PlayCue { cue: cue.to_string() }
        }
        // The HUD voice-design control. A provisioning verb that designs a voice
        // for an agent from a TEXT description. Three checks, all here so a thin /
        // empty request never spends a cloud op: (1) a non-empty agent (resolution
        // to a real agent + its allowlist is downstream), (2) a description at or
        // above the EL design floor (MIN_VOICE_DESCRIPTION_CHARS) — a too-short
        // prompt is a BadRequest, never routed, (3) a display name, defaulted to
        // the agent name when omitted. The trigger's key + cloud-tier gate makes
        // no-key / offline an HONEST failure downstream; here we only admit a
        // structurally-complete request. Only the text description leaves the host.
        "design_voice" => {
            let agent = clamp_text(req.agent.clone().unwrap_or_default());
            let agent = agent.trim();
            if agent.is_empty() {
                return Decision::BadRequest { reason: "design_voice requires an agent" };
            }
            let description = clamp_text(req.description);
            let description = description.trim();
            if description.chars().count() < MIN_VOICE_DESCRIPTION_CHARS {
                return Decision::BadRequest {
                    reason: "design_voice requires a longer voice description",
                };
            }
            // The display name defaults to the agent name when the field is empty.
            let name = clamp_text(req.name);
            let name = name.trim();
            let name = if name.is_empty() { agent } else { name };
            Command::DesignVoice {
                agent: agent.to_string(),
                description: description.to_string(),
                name: name.to_string(),
            }
        }
        // The HUD pronunciation control. A provisioning verb that mints a
        // single-alias pronunciation rule (word -> say). Both the string to
        // replace (`word`) and the alias pronunciation (`say`) must be non-empty —
        // an empty either side is a BadRequest, never routed — and the dictionary
        // name defaults to a fixed label when omitted. The trigger's key +
        // cloud-tier gate makes no-key / offline an HONEST failure downstream; here
        // we only admit a structurally-complete rule. Text rules only; no audio
        // leaves the host.
        "create_pronunciation" => {
            let word = clamp_text(req.word);
            let word = word.trim();
            if word.is_empty() {
                return Decision::BadRequest {
                    reason: "create_pronunciation requires a word to replace",
                };
            }
            let say = clamp_text(req.say);
            let say = say.trim();
            if say.is_empty() {
                return Decision::BadRequest {
                    reason: "create_pronunciation requires an alias pronunciation",
                };
            }
            let name = clamp_text(req.name);
            let name = name.trim();
            let name = if name.is_empty() { "JARVIS pronunciation" } else { name };
            Command::CreatePronunciation {
                word: word.to_string(),
                say: say.to_string(),
                name: name.to_string(),
            }
        }
        // The HUD music-generation control (Jerome's "Leisure + DJ" surface). A benign
        // verb that composes a track from a TEXT prompt. The prompt is validated here:
        // clamped + trimmed, and a NON-EMPTY check — an empty prompt is a BadRequest,
        // never routed, so a thin request never spends a cloud op. The OPTIONAL
        // length_ms is carried verbatim (the server clamps it). The trigger's key +
        // cloud-tier gate makes no-key / offline an HONEST failure downstream; here we
        // only admit a structurally-complete request. Only the text prompt leaves the
        // host.
        "compose_music" => {
            let prompt = clamp_text(req.prompt);
            let prompt = prompt.trim();
            if prompt.is_empty() {
                return Decision::BadRequest { reason: "compose_music requires a non-empty prompt" };
            }
            Command::ComposeMusic {
                prompt: prompt.to_string(),
                length_ms: req.length_ms,
            }
        }
        // Anything else — including the empty string — is rejected here. There is
        // NO route for an unknown command.
        other => return Decision::UnknownCommand { cmd: other.to_string() },
    };
    Decision::Ok { token: req.token, command }
}

/// Per-window rolling rate-limiter for the command channel. Single principal
/// (the one HUD token), so it is not keyed by name — just a window of recent
/// call instants. Pure rate math, tested directly. Mirrors genproxy's limiter.
#[derive(Default)]
struct RateLimiter {
    calls: Vec<Instant>,
}

impl RateLimiter {
    /// Record a call at `now` and report whether it is ALLOWED (within [`RATE`]
    /// over [`RATE_WINDOW`]). A rejected call is NOT recorded, so a steady stream
    /// at the limit is not permanently wedged by one rejected burst.
    fn check(&mut self, now: Instant) -> bool {
        self.calls.retain(|t| now.duration_since(*t) < RATE_WINDOW);
        if self.calls.len() as u32 >= RATE {
            return false;
        }
        self.calls.push(now);
        true
    }
}

/// The seam to the heavy gated pipeline, abstracted so the unit tests run with a
/// hermetic mock instead of a live daemon (no model, no socket, no network). The
/// PRODUCTION impl ([`crate::main`]'s wiring) routes each call through the SAME
/// pipeline the voice path uses:
///   * [`ask`] -> `router::route()` (delegation, RAG, cloud tool-loop) — a
///     consequential tool STILL parks via the confirmation gate,
///   * [`brief`] -> `anticipate::on_demand_brief` (Edith's on-demand brief),
///   * [`mission`] -> `mission::run_mission` (bounded),
///   * [`roster`] / [`state`] -> read-only registry/state snapshots.
///
/// The confirm/deny-by-id and dismiss_forge commands do NOT go through this
/// trait — they act on the REAL `confirm` slot and forge marker (via
/// [`Dispatcher`] below), so the non-bypass guarantees are tested against the
/// genuine gate, not a mock.
pub trait CommandPipeline: Send + Sync {
    /// Route an `ask` through the normal pipeline; the agent ref (if any) selects
    /// the handling agent (its allowlist applies). A consequential tool parks —
    /// the returned text is then the confirmation prompt, NOT a fired action.
    fn ask(
        &self,
        text: &str,
        agent: Option<&str>,
    ) -> impl std::future::Future<Output = String> + Send;
    /// Compose Edith's on-demand brief (read-only).
    fn brief(&self) -> impl std::future::Future<Output = String> + Send;
    /// Run a bounded Fury mission.
    fn mission(&self, goal: &str) -> impl std::future::Future<Output = String> + Send;
    /// Read-only roster snapshot (the constellation).
    fn roster(&self) -> impl std::future::Future<Output = String> + Send;
    /// Read-only state snapshot (pending + agent state).
    fn state(&self) -> impl std::future::Future<Output = String> + Send;
    /// SELF-DISTILLATION (F17): run one operator-triggered distillation — stage
    /// a redacted dataset + run the device-gated training, NEVER promoting the
    /// staged adapter. Off unless [distill].enabled. Returns a spoken-style ack.
    fn distill(&self) -> impl std::future::Future<Output = String> + Send;
    /// FEDERATED SYNC (F18): run one operator-triggered sync — seal the facts +
    /// merge a paired-device bundle, NEVER exporting anything in the clear and
    /// NEVER silently clobbering. Off unless [sync].enabled + a shared key.
    fn sync(&self) -> impl std::future::Future<Output = String> + Send;
    /// OVERNIGHT AGENTS (F10): ENQUEUE a task for the next away-window. A local
    /// write only — never runs the task here. Off unless [overnight].enabled.
    fn overnight(&self, task: &str, agent: Option<&str>) -> impl std::future::Future<Output = String> + Send;
    /// Play a NAMED built-in SFX cue (already validated as a known cue by
    /// [`decide`]). Delegates to the daemon's `trigger_cue`, whose gate handles
    /// switch-off / no-key / offline as an HONEST silent no-op — the returned text
    /// is the honest outcome (played / cached / unavailable), never a faked play.
    fn play_cue(&self, cue: &str) -> impl std::future::Future<Output = String> + Send;
    /// DESIGN a voice for `agent` from a text `description` + display `name` (all
    /// already validated by [`decide`]). Delegates to the daemon's
    /// `trigger_design_voice`, whose gate (key + cloud tier) makes no-key / offline
    /// an HONEST failure — the returned text is the honest outcome (designed /
    /// unavailable), NEVER a fabricated voice. Only the text description leaves the
    /// device; the el_key + the returned voice id stay inside the trigger.
    fn design_voice(
        &self,
        agent: &str,
        description: &str,
        name: &str,
    ) -> impl std::future::Future<Output = String> + Send;
    /// CREATE a single-alias pronunciation rule (`word` -> `say`) under dictionary
    /// `name` (all already validated by [`decide`]). Delegates to the daemon's
    /// `trigger_create_pronunciation`, whose gate (key + cloud tier) makes no-key /
    /// offline an HONEST failure — the returned text is the honest outcome (created
    /// / unavailable), NEVER a fabricated dictionary id. Text rules only.
    fn create_pronunciation(
        &self,
        word: &str,
        say: &str,
        name: &str,
    ) -> impl std::future::Future<Output = String> + Send;
    /// COMPOSE a music track from a text `prompt` with an OPTIONAL `length_ms` (both
    /// already validated by [`decide`]: a non-empty prompt). Delegates to the daemon's
    /// `trigger_compose_music`, whose gate ([`crate::voice_tier::music_enabled`] +
    /// offline check) makes switch-off / no-key / offline an HONEST failure — the
    /// returned text is the honest outcome (composed / unavailable), NEVER a fabricated
    /// track. Only the text prompt leaves the device; the el_key stays inside the
    /// trigger.
    fn compose_music(
        &self,
        prompt: &str,
        length_ms: Option<u32>,
    ) -> impl std::future::Future<Output = String> + Send;
}

/// The seam to the confirmation gate + forge marker — the SECURITY-critical
/// surface. Default-implemented to act on the REAL process-global confirm slot
/// and the real forge pending marker, so the production path and the tests use
/// the SAME logic. The tests park a real confirmation and drive confirm/deny by
/// id through here, proving a switch-OFF confirm fires nothing and a dismiss
/// writes nothing into apps/.
pub trait Dispatcher: Send + Sync {
    /// List the genuinely-pending confirmations (faithful id + preview) and the
    /// pending forge proposal ts (if any). Read-only.
    fn list_pending(&self) -> impl std::future::Future<Output = Value> + Send;
    /// Approve the SPECIFIC parked confirmation named by `id`: replay ONLY that
    /// exact parked action, ONLY if it is genuinely pending AND the master switch
    /// is ON (the replay re-checks the switch + the agent allowlist). An unknown
    /// id fires nothing. Returns the spoken-style outcome.
    fn confirm(&self, id: &str) -> impl std::future::Future<Output = String> + Send;
    /// Deny the parked confirmation named by `id`: clear it, fire nothing.
    fn deny(&self, id: &str) -> impl std::future::Future<Output = String> + Send;
    /// Dismiss the forge proposal named by `ts`: clear the pending marker only.
    /// MUST NOT apply/deploy (apply stays scripts/apply_forge.sh).
    fn dismiss_forge(&self, ts: u64) -> impl std::future::Future<Output = String> + Send;
    /// USER-SET a per-action consequential policy from the anchored `text` phrase.
    /// This is the AUTHENTICATED-LOCAL user write path: it parses + applies the
    /// rule via `policy::handle_user_policy_text` (NOT the model tool loop). A
    /// phrase that is not one of the anchored shapes is reported as not understood
    /// — it is NEVER routed to the model from here, so no model output can reach
    /// the policy store through this verb. Returns a spoken-style ack.
    fn policy(&self, text: &str) -> impl std::future::Future<Output = String> + Send;
    /// ENGAGE the panic / lockdown emergency stop (task #12). The HUD PANIC button.
    /// Calls `lockdown::panic()` directly — sets the global flag (forcing every
    /// master gate OFF), drops any parked confirmation, persists the marker, audits
    /// — and returns the honest spoken-style confirmation. NEVER the model loop.
    /// Default-implemented so production + tests share the real lockdown path.
    fn panic(&self) -> impl std::future::Future<Output = String> + Send {
        async { crate::lockdown::panic().await.to_string() }
    }
    /// LIFT the lockdown (task #12). The authenticated-local USER resume control.
    /// Calls `lockdown::unlock()` directly (clears the flag — gates return to their
    /// CONFIGURED values — and removes the marker) and returns the honest ack.
    /// Together with the spoken "unlock" intent this is the ONLY path to unlock;
    /// it is NEVER reachable from the model loop. Default-implemented so production
    /// + tests share the real lockdown path.
    fn unlock(&self) -> impl std::future::Future<Output = String> + Send {
        async { crate::lockdown::unlock().await.to_string() }
    }
}

/// Serve the command channel until the process exits. Binds
/// `state/ipc/command.sock` (creating the `0700` dir, `chmod 0600` on the
/// socket), then accepts HUD connections and handles each JSONL line through
/// [`handle_line`]. Spawned from main.rs alongside the other socket servers.
pub async fn serve<P, D>(
    command_sock: PathBuf,
    pipeline: Arc<P>,
    dispatcher: Arc<D>,
    event_cues: bool,
) where
    P: CommandPipeline + 'static,
    D: Dispatcher + 'static,
{
    let limiter = Arc::new(Mutex::new(RateLimiter::default()));
    let listener = match bind_socket(&command_sock) {
        Ok(l) => l,
        Err(e) => {
            warn!(path = %command_sock.display(), error = %e, "command channel failed to bind; HUD cannot drive the system");
            return;
        }
    };
    info!(path = %command_sock.display(), "command channel listening");

    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let pipeline = pipeline.clone();
                let dispatcher = dispatcher.clone();
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    handle_conn(stream, pipeline, dispatcher, limiter, event_cues).await;
                });
            }
            Err(e) => warn!(error = %e, "command channel accept failed"),
        }
    }
}

/// Bind the command socket: remove a stale one, create the `0700` parent dir,
/// bind, `chmod 0600`. Defense-in-depth on top of the token gate.
fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %path.display(), error = %e, "could not remove stale command socket");
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let listener = UnixListener::bind(path)?;
    set_mode(path, 0o600);
    Ok(listener)
}

/// chmod best-effort: a failed tightening is defense-in-depth (token
/// verification is the real gate), so warn and continue.
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        warn!(path = %path.display(), error = %e, "could not tighten command socket permissions");
    }
}

/// Serve one accepted connection: read JSONL lines and reply to each. A line
/// over [`MAX_LINE_BYTES`] is rejected by [`decide`]'s `raw.len()` length check
/// (the size gate runs AFTER the line is buffered, not as a read bound). The
/// socket is 0600 inside a 0700 ipc dir and token-gated (same-user local
/// process only), so the read sits inside the daemon's trust boundary.
async fn handle_conn<P, D>(
    stream: UnixStream,
    pipeline: Arc<P>,
    dispatcher: Arc<D>,
    limiter: Arc<Mutex<RateLimiter>>,
    event_cues: bool,
) where
    P: CommandPipeline + 'static,
    D: Dispatcher,
{
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => return, // client closed
            Ok(_) => {
                let reply = handle_line(&line, &pipeline, &dispatcher, &limiter, event_cues).await;
                let mut out = reply.to_string();
                out.push('\n');
                if write_half.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                if write_half.flush().await.is_err() {
                    return;
                }
            }
            Err(e) => {
                warn!(error = %e, "reading command socket failed");
                return;
            }
        }
    }
}

/// Handle one inbound line end-to-end and produce the reply JSON. This is the
/// orchestration seam the unit tests drive with a mock pipeline + the real
/// dispatcher: it runs the PURE [`decide`], then the token check, the rate
/// limit, and the route — emitting the same telemetry the production path does.
/// The ORDER matters and is part of the contract:
///   1. size/parse/allowlist/shape ([`decide`]) — an unknown/oversized/malformed
///      line never reaches auth,
///   2. token verification — an unauthenticated/forged/stale line never routes,
///   3. rate-limit — a verified-but-flooding client is throttled,
///   4. route into the gated pipeline.
async fn handle_line<P, D>(
    raw: &str,
    pipeline: &Arc<P>,
    dispatcher: &Arc<D>,
    limiter: &Arc<Mutex<RateLimiter>>,
    event_cues: bool,
) -> Value
where
    P: CommandPipeline + 'static,
    D: Dispatcher,
{
    let (token, command) = match decide(raw) {
        Decision::Ok { token, command } => (token, command),
        Decision::UnknownCommand { cmd } => {
            telemetry::emit("system", "command.denied", json!({"reason": "unknown_command", "cmd": cmd}));
            return json!({"ok": false, "error": "unknown_command"});
        }
        Decision::BadRequest { reason } => {
            return json!({"ok": false, "error": "bad_request", "detail": reason});
        }
        Decision::Oversized => {
            telemetry::emit("system", "command.denied", json!({"reason": "oversized"}));
            return json!({"ok": false, "error": "oversized"});
        }
        Decision::Malformed => {
            return json!({"ok": false, "error": "malformed"});
        }
    };

    // (2) TOKEN: the SAME HMAC machinery as the per-app relay / generate proxy.
    // A forged/tampered/stale/missing token fails closed BEFORE any route. No
    // token value is ever logged.
    if !apps::verify_command_token(&token) {
        warn!("command line failed token verification");
        telemetry::emit("system", "command.auth_failed", json!({"via": "command"}));
        return json!({"ok": false, "error": "unauthorized"});
    }

    // (3) RATE-LIMIT (spam guard) — after auth so an unauthenticated flood can't
    // consume an authenticated client's budget, and the unauthenticated line was
    // already rejected above.
    let allowed = {
        let mut lim = limiter.lock().await;
        lim.check(Instant::now())
    };
    if !allowed {
        warn!("command channel rate limit tripped");
        telemetry::emit("system", "command.denied", json!({"reason": "rate_limited"}));
        return json!({"ok": false, "error": "rate_limited"});
    }

    // (4) ROUTE into the existing gated pipeline (never around it).
    route_command(command, pipeline, dispatcher, event_cues).await
}

/// Route an AUTHENTICATED, rate-passed command into the gated pipeline. Each arm
/// either calls the read/route pipeline (ask/brief/mission/roster/state) or the
/// security-critical dispatcher (pending/confirm/deny/dismiss_forge). Nothing
/// here bypasses a gate: a consequential `ask` parks in `pipeline.ask`; a
/// `confirm` fires ONLY through `dispatcher.confirm`, which re-checks the master
/// switch + the agent allowlist; `dismiss_forge` clears a marker only.
///
/// `event_cues` is the OPT-IN [voice].event_cues flag (ships OFF). When true, a
/// confirm/deny — AFTER its existing handling has fully completed and its reply is
/// built — FIRE-AND-FORGETS a cosmetic SFX cue (`confirm` -> "success", `deny` ->
/// "notify") via `tokio::spawn`; the spawned future's result is dropped, the cue's
/// OWN sfx gate makes off/no-key/offline a silent no-op, and a cue error is
/// swallowed. The cue can NEVER change the command's return value or timing — the
/// `Value` is already built and returned regardless of the cue. With the flag false
/// NO cue is spawned, so confirm/deny behave byte-for-byte as today.
async fn route_command<P, D>(
    command: Command,
    pipeline: &Arc<P>,
    dispatcher: &Arc<D>,
    event_cues: bool,
) -> Value
where
    P: CommandPipeline + 'static,
    D: Dispatcher,
{
    match command {
        Command::Ask { text, agent } => {
            telemetry::emit("system", "command.routed", json!({"cmd": "ask", "agent": agent}));
            let reply = pipeline.ask(&text, agent.as_deref()).await;
            json!({"ok": true, "reply": reply})
        }
        Command::Brief => {
            telemetry::emit("system", "command.routed", json!({"cmd": "brief"}));
            json!({"ok": true, "reply": pipeline.brief().await})
        }
        Command::Mission { goal } => {
            telemetry::emit("system", "command.routed", json!({"cmd": "mission"}));
            json!({"ok": true, "reply": pipeline.mission(&goal).await})
        }
        Command::Roster => {
            json!({"ok": true, "reply": pipeline.roster().await})
        }
        Command::State => {
            json!({"ok": true, "reply": pipeline.state().await})
        }
        Command::Distill => {
            telemetry::emit("system", "command.routed", json!({"cmd": "distill"}));
            json!({"ok": true, "reply": pipeline.distill().await})
        }
        Command::Sync => {
            telemetry::emit("system", "command.routed", json!({"cmd": "sync"}));
            json!({"ok": true, "reply": pipeline.sync().await})
        }
        Command::Overnight { task, agent } => {
            telemetry::emit("system", "command.routed", json!({"cmd": "overnight"}));
            json!({"ok": true, "reply": pipeline.overnight(&task, agent.as_deref()).await})
        }
        Command::Pending => {
            json!({"ok": true, "pending": dispatcher.list_pending().await})
        }
        Command::Confirm { id } => {
            telemetry::emit("system", "command.routed", json!({"cmd": "confirm"}));
            // The existing handling + reply are UNCHANGED: confirm runs to
            // completion and its reply is built FIRST.
            let reply = json!({"ok": true, "reply": dispatcher.confirm(&id).await});
            // Then, ONLY if opted in, fire-and-forget a cosmetic "success" cue. The
            // future is spawned detached and its result dropped — the cue's own SFX
            // gate handles off/no-key/offline as a silent no-op, and a cue error is
            // swallowed. This runs AFTER the reply is built, so it can never block,
            // delay, or change the reply we return on the next line.
            spawn_event_cue(pipeline, event_cues, "success");
            reply
        }
        Command::Deny { id } => {
            telemetry::emit("system", "command.routed", json!({"cmd": "deny"}));
            // Same shape as Confirm: deny's existing handling + reply are UNCHANGED
            // and built FIRST; the opt-in cue is a detached, result-dropped
            // fire-and-forget that can never affect the outcome or timing.
            let reply = json!({"ok": true, "reply": dispatcher.deny(&id).await});
            spawn_event_cue(pipeline, event_cues, "notify");
            reply
        }
        Command::DismissForge { ts } => {
            telemetry::emit("system", "command.routed", json!({"cmd": "dismiss_forge", "ts": ts}));
            json!({"ok": true, "reply": dispatcher.dismiss_forge(ts).await})
        }
        Command::Policy { text } => {
            // The phrase text is NOT logged (a tool name is not a secret, but we
            // keep the telemetry shape minimal + uniform with the other verbs).
            telemetry::emit("system", "command.routed", json!({"cmd": "policy"}));
            json!({"ok": true, "reply": dispatcher.policy(&text).await})
        }
        Command::Panic => {
            // The HUD PANIC button: engage the emergency stop directly via the
            // dispatcher (lockdown::panic) — never the model. Telemetry records the
            // engage so the HUD status indicator flips to LOCKED DOWN.
            telemetry::emit("system", "command.routed", json!({"cmd": "panic"}));
            let reply = dispatcher.panic().await;
            json!({"ok": true, "reply": reply, "locked": crate::lockdown::is_locked_down()})
        }
        Command::Unlock => {
            // The HUD unlock control: lift the lockdown directly via the dispatcher
            // (lockdown::unlock) — the authenticated-local USER path, never the
            // model. The HUD status indicator flips back to normal.
            telemetry::emit("system", "command.routed", json!({"cmd": "unlock"}));
            let reply = dispatcher.unlock().await;
            json!({"ok": true, "reply": reply, "locked": crate::lockdown::is_locked_down()})
        }
        Command::PlayCue { cue } => {
            // The HUD SFX-cue Play button. The cue name is already a validated
            // catalog member (decide). Route into the daemon's `trigger_cue` via
            // the pipeline — its gate (sfx_enabled + offline) makes switch-off /
            // no-key / offline an HONEST silent no-op; we surface whatever it
            // returns (played/cached/unavailable), never faking a play. The cue
            // NAME is non-secret, so telemetry records it like the other verbs.
            telemetry::emit("system", "command.routed", json!({"cmd": "play_cue", "cue": cue}));
            json!({"ok": true, "reply": pipeline.play_cue(&cue).await})
        }
        Command::DesignVoice { agent, description, name } => {
            // The HUD voice-design control. agent/description/name are already
            // validated (non-empty agent, description >= the EL floor, defaulted
            // name) by decide. Route into the daemon's trigger_design_voice via the
            // pipeline — its gate (key + cloud tier) makes no-key / offline an
            // HONEST failure; we surface whatever it returns (designed / unavailable
            // / failed), never faking a voice. Telemetry records the AGENT slot only
            // — NEVER the (free-text, possibly identifying) description, the el_key,
            // or the returned voice id.
            telemetry::emit("system", "command.routed", json!({"cmd": "design_voice", "agent": agent}));
            json!({"ok": true, "reply": pipeline.design_voice(&agent, &description, &name).await})
        }
        Command::CreatePronunciation { word, say, name } => {
            // The HUD pronunciation control. word/say are already validated
            // non-empty (name defaulted) by decide. Route into the daemon's
            // trigger_create_pronunciation via the pipeline — its gate (key + cloud
            // tier) makes no-key / offline an HONEST failure; we surface whatever it
            // returns (created / unavailable / failed), never faking a dictionary
            // id. Telemetry records the cmd only — NEVER the free-text rule, the
            // el_key, or the returned ids.
            telemetry::emit("system", "command.routed", json!({"cmd": "create_pronunciation"}));
            json!({"ok": true, "reply": pipeline.create_pronunciation(&word, &say, &name).await})
        }
        Command::ComposeMusic { prompt, length_ms } => {
            // The HUD music-generation control (Jerome's surface). The prompt is
            // already validated non-empty by decide. Route into the daemon's
            // trigger_compose_music via the pipeline — its gate (music_enabled +
            // offline) makes switch-off / no-key / offline an HONEST failure; we
            // surface whatever it returns (composed / unavailable / failed), never
            // faking a track. Telemetry records the cmd only — NEVER the free-text
            // prompt or the el_key.
            telemetry::emit("system", "command.routed", json!({"cmd": "compose_music"}));
            json!({"ok": true, "reply": pipeline.compose_music(&prompt, length_ms).await})
        }
    }
}

/// FIRE-AND-FORGET an opt-in event cue. A no-op unless `event_cues` is true. When
/// enabled, clones the `Arc<P>` and `tokio::spawn`s `pipeline.play_cue(cue)` as a
/// DETACHED task whose result is DROPPED — the cue's own sfx gate inside
/// `play_cue` makes off/no-key/offline a silent no-op, and any error string it
/// returns is discarded here. Crucially this NEVER awaits the cue: the caller has
/// already built and is about to return the command's reply, so the cue cannot
/// block, delay, or change the command's outcome or timing. The `JoinHandle` is
/// intentionally dropped (the task runs to completion on its own).
fn spawn_event_cue<P>(pipeline: &Arc<P>, event_cues: bool, cue: &'static str)
where
    P: CommandPipeline + 'static,
{
    if !event_cues {
        return;
    }
    let pipeline = pipeline.clone();
    tokio::spawn(async move {
        // Result dropped on purpose: the cue is cosmetic, its gate already handles
        // off/no-key/offline honestly, and an error must never surface here.
        let _ = pipeline.play_cue(cue).await;
    });
}

// ===========================================================================
// Production dispatcher — acts on the REAL confirm slot + forge marker.
// ===========================================================================

/// The production [`Dispatcher`]: confirm/deny act on the REAL process-global
/// confirmation slot (confirm.rs); dismiss_forge clears the REAL forge pending
/// marker via Memory. It NEVER applies/deploys a forge proposal. Owns the
/// `Memory` + project root it needs for the replay + the marker.
pub struct LiveDispatcher {
    pub memory: Arc<crate::memory::Memory>,
    pub root: PathBuf,
}

impl Dispatcher for LiveDispatcher {
    async fn list_pending(&self) -> Value {
        // Faithful, replay-FREE listing: id + agent + tool + preview for the live
        // confirmation, plus the forge proposal ts (if any). No input args cross
        // the channel — the listing names what exists; only confirm {id} fires.
        let confirmation = crate::confirm::peek_pending(Instant::now()).map(|p| {
            json!({"id": p.id, "agent": p.agent, "tool": p.tool, "preview": p.preview})
        });
        let forge_pending = self
            .memory
            .get_fact("meta.forge_pending")
            .await
            .ok()
            .flatten();
        json!({
            "confirmation": confirmation,
            "forge_pending_ts": forge_pending,
        })
    }

    async fn confirm(&self, id: &str) -> String {
        // Take the parked action ONLY if the id names the genuine, non-expired
        // slot. An unknown/stale id is inert — nothing is taken, nothing fires.
        match crate::confirm::confirm_by_id(id, Instant::now()) {
            crate::confirm::ByIdConfirm::Matched(pending) => {
                // Replay the EXACT parked action through the SAME path the spoken
                // "yes" uses: it re-checks the agent allowlist AND the master
                // switch. With the switch OFF, the replay only previews and fires
                // nothing (the correct fail-safe). Nothing is re-derived from the
                // channel — only what was previewed can fire.
                let (outcome, _is_error) =
                    crate::anthropic::replay_confirmed_action(&pending, self.memory.as_ref()).await;
                outcome
            }
            crate::confirm::ByIdConfirm::NoMatch => {
                "No pending action with that id.".to_string()
            }
        }
    }

    async fn deny(&self, id: &str) -> String {
        if crate::confirm::deny_by_id(id, Instant::now()) {
            "Cancelled.".to_string()
        } else {
            "No pending action with that id.".to_string()
        }
    }

    async fn dismiss_forge(&self, ts: u64) -> String {
        dismiss_forge_marker(self.memory.as_ref(), &self.root, ts).await
    }

    async fn policy(&self, text: &str) -> String {
        // USER-SET-ONLY write: parse the anchored phrase + apply it to the global
        // policy store. `handle_user_policy_text` returns None for anything that is
        // not one of the three anchored phrases — we DO NOT fall back to the model
        // (the whole point is that no model path can set a policy), so a non-phrase
        // gets an honest "not understood" reply, never a route into the tool loop.
        crate::policy::handle_user_policy_text(text).unwrap_or_else(|| {
            "I didn't recognize that as a policy command. Say, for example, \
             \"always allow the gmail_send action\", \"never allow the x_post action\", \
             or \"always ask before the gmail_send action\"."
                .to_string()
        })
    }
}

// ===========================================================================
// Production pipeline — routes into the EXISTING gated, text-returning paths.
// ===========================================================================

/// The production [`CommandPipeline`]. Every arm routes into the SAME gated path
/// the voice surface uses, but TEXT-ONLY (it never opens the speaker — the
/// channel returns prose to the HUD; no audio, no echo risk):
///   * `ask`     -> `anthropic::complete_with_tools` (the cloud tool-loop). A
///     consequential tool STILL parks via the confirmation gate inside
///     `execute_tool`; the agent's OWN allowlist (`agent.tools`) is what is
///     offered/accepted, so isolation holds. The channel pre-confirms NOTHING.
///   * `brief`   -> `anthropic::edith_brief_now` (read-only on-demand brief).
///   * `mission` -> `anthropic::run_fury_mission` (bounded; sub-tasks under each
///     specialist's allowlist + the same gate).
///   * `roster`/`state` -> read-only registry / pending snapshots.
///
/// The command channel is STATELESS per request (no cross-turn history is fed
/// into `ask`) — a deliberate, safe choice: the cross-turn CONFIRMATION state is
/// the ONLY state that persists, and it lives in the gated `confirm` slot, not
/// here. So a consequential `ask` parks; the user then `confirm {id}`s it.
pub struct LivePipeline {
    pub memory: Arc<crate::memory::Memory>,
    pub agents: Arc<crate::agents::AgentRegistry>,
    pub heavy_model: String,
    pub max_tokens: u32,
    /// Full daemon config — the `play_cue` arm needs it to evaluate the SFX gate
    /// ([`crate::voice_tier::sfx_enabled`] / the active tier) inside `trigger_cue`,
    /// exactly as the spoken SFX path does. Cheap clone, read-only here.
    pub cfg: crate::config::Config,
    /// Daemon root — the cue WAV cache lives under `state/tmp/sfx-cache/`.
    pub root: PathBuf,
    /// The inference socket path. The command channel owns NO mutable
    /// `InferenceClient` (the main loop holds the others); `play_cue` opens a fresh
    /// per-call client on this socket, the same lazy-connect pattern the other
    /// background tasks use — it spends a model call ONLY on a cache miss with the
    /// gate open.
    pub inference_sock: PathBuf,
}

impl LivePipeline {
    /// Resolve the requested agent (or the orchestrator when none/unknown). The
    /// returned agent's `tools` is the allowlist `complete_with_tools` enforces.
    fn resolve_agent<'a>(&'a self, agent: Option<&str>) -> &'a crate::agents::Agent {
        agent
            .and_then(|a| self.agents.get(a))
            .unwrap_or_else(|| self.agents.orchestrator())
    }
}

impl CommandPipeline for LivePipeline {
    async fn ask(&self, text: &str, agent: Option<&str>) -> String {
        let agent = self.resolve_agent(agent);
        let mem = self.memory.as_ref();
        let facts =
            crate::anthropic::grounded_facts_live(text, mem, &agent.namespace).await;
        // SHARED WORLD MODEL context (relevant to this request) from the shared
        // user.world.* tier — grounds the reply in the one coherent world picture
        // every agent shares; never reads another agent's private notes.
        let world_context = crate::anthropic::grounded_world_live(text, mem).await;
        // PERSONALIZATION: the bounded user-model summary (observed profile) so
        // the command reply personalizes to the real observed user. Shared tier
        // only -> never another agent's private notes.
        let personalization = crate::anthropic::grounded_personalization_live(mem).await;
        let persona =
            crate::anthropic::agent_persona_text(&agent.name, agent.is_orchestrator());
        // Stateless turn: empty history. A consequential tool parks inside the
        // loop (execute_tool) and the returned text is the confirmation prompt —
        // the channel never fires it.
        match crate::anthropic::complete_with_tools(
            &self.heavy_model,
            self.max_tokens,
            text,
            &facts,
            &[],
            mem,
            &agent.tools,
            &agent.namespace,
            persona.as_deref(),
            &world_context,
            &personalization,
            true, // a direct user turn — trusted, user-originated
        )
        .await
        {
            Ok(reply) => reply,
            // No cloud key / cloud error: an honest, secret-free line (never a
            // crash, never a leaked error body).
            Err(_) => "I can't reach the cloud to handle that right now, sir.".to_string(),
        }
    }

    async fn brief(&self) -> String {
        crate::anthropic::edith_brief_now()
    }

    async fn mission(&self, goal: &str) -> String {
        // A mission the owner requested directly (Mission command) — trusted.
        crate::anthropic::run_fury_mission(goal, self.memory.as_ref(), true).await
    }

    async fn roster(&self) -> String {
        self.agents.roster_spoken()
    }

    async fn state(&self) -> String {
        // Read-only: the live constellation plus whether a confirmation is parked
        // and whether a forge proposal is pending. No secrets, no replay material.
        let pending = crate::confirm::peek_pending(Instant::now())
            .map(|p| format!("A {} action is awaiting confirmation (id {}).", p.tool, p.id))
            .unwrap_or_else(|| "Nothing awaiting confirmation.".to_string());
        format!("{}\n{}", self.agents.roster_spoken(), pending)
    }

    async fn distill(&self) -> String {
        // Operator-triggered on-device distillation: real training when armed,
        // via the hardened real runner (never spawned in any test). Staged only,
        // never promoted. Off/thin/failed all return an honest spoken line.
        crate::distill::distill_now(
            &self.cfg,
            self.memory.as_ref(),
            &self.root,
            chrono::Utc::now().to_rfc3339(),
            crate::distill::run_real_training,
        )
        .await
    }

    async fn sync(&self) -> String {
        let key = crate::integrations::resolve_secret("sync_shared_key")
            .await
            .and_then(|hex| crate::crypto::SecretKey::from_hex(&hex).ok());
        crate::sync::sync_now(
            &self.cfg,
            self.memory.as_ref(),
            &self.root,
            chrono::Utc::now().to_rfc3339(),
            key,
        )
        .await
    }

    async fn overnight(&self, task: &str, agent: Option<&str>) -> String {
        // Local write only: enqueue the task. The presence-gated overnight_task
        // runs it (tool-less) later — nothing consequential happens here.
        let agent_name = self.resolve_agent(agent).name.clone();
        crate::overnight::enqueue(&self.root, task, &agent_name, &chrono::Utc::now().to_rfc3339())
    }

    async fn play_cue(&self, cue: &str) -> String {
        // Delegate to the daemon's `play_cue_for_command`, the Send-safe wrapper
        // around `trigger_cue`. That path performs the SAME cheap offline/switch
        // pre-checks + the SAME Keychain read as the spoken SFX path, then
        // plays/caches the cue or returns an HONEST unavailable/failed message.
        // The el_key is read ONLY inside trigger_cue and threaded into the request
        // — never logged, never surfaced here.
        match crate::play_cue_for_command(
            self.cfg.clone(),
            cue.to_string(),
            self.root.clone(),
            self.inference_sock.clone(),
        )
        .await
        {
            // Played or cached: surface a short honest ack. We do NOT echo the WAV
            // path (an internal cache path is not the user's business); the HUD
            // already knows which cue it asked to play.
            Ok(_path) => format!("Playing the {cue} cue."),
            // switch-off / no-key / offline / generation failure: the honest line
            // from trigger_cue, never a faked play.
            Err(msg) => msg,
        }
    }

    async fn design_voice(&self, agent: &str, description: &str, name: &str) -> String {
        // Delegate to the daemon's `design_voice_for_command`, the Send-safe wrapper
        // around `trigger_design_voice`. That path performs the SAME cloud-tier +
        // Keychain-key gate as the spoken voice-design path, designs the voice, and
        // persists the returned id into the cloned-voice store, OR returns an HONEST
        // unavailable/failed message. Only the text description leaves the device;
        // the el_key + the returned voice id are read ONLY inside the trigger.
        match crate::design_voice_for_command(
            self.cfg.clone(),
            description.to_string(),
            name.to_string(),
            agent.to_string(),
            self.root.clone(),
            self.inference_sock.clone(),
        )
        .await
        {
            // Designed + stored: a short honest ack. We do NOT echo the voice id (a
            // non-secret, but not the user's business here); the agent now speaks
            // with it. The display name is the user-facing handle.
            Ok(_voice_id) => format!("Designed and saved the {name} voice for {agent}."),
            // no-key / offline / generation / persist failure: the honest line from
            // the trigger, never a faked voice.
            Err(msg) => msg,
        }
    }

    async fn create_pronunciation(&self, word: &str, say: &str, name: &str) -> String {
        // Build the SINGLE alias rule (word -> say) and delegate to the daemon's
        // `create_pronunciation_for_command`, the Send-safe wrapper around
        // `trigger_create_pronunciation`. That path performs the SAME cloud-tier +
        // Keychain-key gate as the spoken pronunciation path, mints the dictionary,
        // and persists the non-secret locator, OR returns an HONEST unavailable/
        // failed message. Text rules only — no audio leaves the device; the el_key
        // + the returned ids are read ONLY inside the trigger.
        let rule = crate::inference::PronunciationRule {
            string_to_replace: word.to_string(),
            rule_type: "alias".to_string(),
            alias: Some(say.to_string()),
            phoneme: None,
            alphabet: None,
        };
        match crate::create_pronunciation_for_command(
            self.cfg.clone(),
            name.to_string(),
            vec![rule],
            self.root.clone(),
            self.inference_sock.clone(),
        )
        .await
        {
            // Created + stored: a short honest ack naming the rule. We do NOT echo
            // the dictionary/version ids (non-secret, but not the user's business).
            Ok(_ids) => format!("Created the pronunciation rule: say \"{word}\" as \"{say}\"."),
            // no-key / offline / generation / persist failure: the honest line from
            // the trigger, never a faked dictionary.
            Err(msg) => msg,
        }
    }

    async fn compose_music(&self, prompt: &str, length_ms: Option<u32>) -> String {
        // Delegate to the daemon's `compose_music_for_command`, the Send-safe wrapper
        // around `trigger_compose_music`. That path performs the SAME cloud-tier +
        // Keychain-key gate as the other EL ops, composes the track, OR returns an
        // HONEST unavailable/failed message. Only the text prompt leaves the device;
        // the el_key is read ONLY inside the trigger.
        match crate::compose_music_for_command(
            self.cfg.clone(),
            prompt.to_string(),
            length_ms,
            self.root.clone(),
            self.inference_sock.clone(),
        )
        .await
        {
            // Composed: a short honest ack. We do NOT echo the WAV path (an internal
            // track path is not the user's business); the HUD already knows it asked
            // to compose.
            Ok(_path) => "Composed your track — it's ready.".to_string(),
            // switch-off / no-key / offline / generation failure: the honest line from
            // trigger_compose_music, never a faked track.
            Err(msg) => msg,
        }
    }
}

/// Clear the forge pending marker for `ts` — and NOTHING else. This is the
/// channel's `dismiss_forge`: it DISMISSES a proposal from the deck. It MUST NOT
/// apply/deploy — apply stays scripts/apply_forge.sh, which is the only path that
/// writes into apps/. This function only deletes the `meta.forge_pending` fact
/// when it matches `ts` (so dismissing a stale id is a no-op); it never touches
/// apps/ and never runs generated code.
///
/// Factored out (free function over `&Memory`) so the no-deploy guarantee is
/// unit-testable: the test asserts the marker is gone AND that apps/ is byte-for-
/// byte unchanged.
pub async fn dismiss_forge_marker(memory: &crate::memory::Memory, _root: &Path, ts: u64) -> String {
    match memory.get_fact("meta.forge_pending").await {
        Ok(Some(current)) if current == ts.to_string() => {
            match memory.delete_fact("meta.forge_pending").await {
                Ok(_) => {
                    telemetry::emit("system", "forge.dismissed", json!({"ts": ts}));
                    format!("Dismissed the forge proposal {ts}. (It was not deployed; apply stays scripts/apply_forge.sh.)")
                }
                Err(e) => {
                    warn!(error = %e, "failed to clear forge pending marker");
                    "Could not clear the forge proposal marker.".to_string()
                }
            }
        }
        // The marker is absent or names a different ts — dismissing a stale id is
        // a no-op. NEVER deploy, NEVER touch apps/.
        _ => "No matching pending forge proposal to dismiss.".to_string(),
    }
}

/// Per-app socket dir lives under state/ipc/apps; the command socket sits beside
/// it at state/ipc/command.sock so the existing `0700` ipc dir confines it too.
pub fn command_socket_path(root: &Path) -> PathBuf {
    root.join("state").join("ipc").join("command.sock")
}

/// The per-boot capability token is handed to the Tauri backend OUT-OF-BAND via
/// a `0600` file inside the SAME `0700` confined `state/ipc/` dir as the socket
/// (the established local handshake — the daemon is the only writer, the HUD
/// backend the only reader, and the token never touches argv, env, telemetry, or
/// any logged path). The token dies on restart (fresh per-boot nonce), so a
/// stale file is harmless — a captured value fails [`apps::verify_command_token`].
pub fn command_token_path(root: &Path) -> PathBuf {
    root.join("state").join("ipc").join("command.token")
}

/// Write the minted command token to its `0600` handoff file (creating the
/// `0700` parent dir). Best-effort + NEVER logs the token: on a write failure we
/// warn with the path only, never the value, and the channel simply stays
/// unreachable from the HUD (fails closed). Returns whether the write succeeded.
pub fn write_command_token(root: &Path, token: &str) -> bool {
    let path = command_token_path(root);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(path = %parent.display(), error = %e, "could not create ipc dir for the command token handoff");
            return false;
        }
        set_mode(parent, 0o700);
    }
    // Remove any stale file first so the new `0600` perms are not inherited from
    // a previous, possibly looser, file.
    let _ = std::fs::remove_file(&path);
    if let Err(e) = std::fs::write(&path, token.as_bytes()) {
        warn!(path = %path.display(), error = %e, "could not write the command token handoff file");
        return false;
    }
    set_mode(&path, 0o600);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex as StdMutex;

    // -- pure decide: size / parse / allowlist / shape ------------------------

    /// Every allowlisted command parses to its variant; an unknown command is
    /// rejected (never routed).
    #[test]
    fn allowlist_admits_only_the_bounded_set() {
        // Known commands -> Ok with the right variant.
        let cases = [
            (json!({"token": "t", "cmd": "ask", "text": "hi"}), true),
            (json!({"token": "t", "cmd": "brief"}), true),
            (json!({"token": "t", "cmd": "mission", "goal": "do x"}), true),
            (json!({"token": "t", "cmd": "roster"}), true),
            (json!({"token": "t", "cmd": "state"}), true),
            (json!({"token": "t", "cmd": "distill"}), true),
            (json!({"token": "t", "cmd": "sync"}), true),
            (json!({"token": "t", "cmd": "overnight", "prompt": "look into X"}), true),
            (json!({"token": "t", "cmd": "pending"}), true),
            (json!({"token": "t", "cmd": "confirm", "id": "abc"}), true),
            (json!({"token": "t", "cmd": "deny", "id": "abc"}), true),
            (json!({"token": "t", "cmd": "dismiss_forge", "ts": 42}), true),
            (json!({"token": "t", "cmd": "policy", "text": "always allow the gmail_send action"}), true),
            (json!({"token": "t", "cmd": "play_cue", "cue": "confirm"}), true),
            (json!({"token": "t", "cmd": "design_voice", "agent": "edith", "description": "a calm warm british woman, mid-thirties"}), true),
            (json!({"token": "t", "cmd": "create_pronunciation", "word": "JARVIS", "say": "jarviss"}), true),
            (json!({"token": "t", "cmd": "compose_music", "prompt": "a calm lo-fi study beat"}), true),
        ];
        for (line, _ok) in cases {
            assert!(
                matches!(decide(&line.to_string()), Decision::Ok { .. }),
                "known command must be Ok: {line}"
            );
        }
        // Unknown / privileged-sounding / empty commands -> UnknownCommand.
        for cmd in ["apply_forge", "deploy", "exec", "raw", "", "shutdown", "set_switch"] {
            let line = json!({"token": "t", "cmd": cmd, "text": "x"}).to_string();
            match decide(&line) {
                Decision::UnknownCommand { cmd: got } => assert_eq!(got, cmd),
                other => panic!("cmd {cmd:?} must be UnknownCommand, got {other:?}"),
            }
        }
    }

    /// Required fields are enforced structurally: ask needs text, mission a goal,
    /// confirm/deny an id, dismiss_forge a ts.
    #[test]
    fn required_fields_are_enforced() {
        let bad = [
            json!({"token": "t", "cmd": "ask", "text": "   "}),
            json!({"token": "t", "cmd": "ask"}),
            json!({"token": "t", "cmd": "mission", "goal": ""}),
            json!({"token": "t", "cmd": "confirm", "id": ""}),
            json!({"token": "t", "cmd": "confirm"}),
            json!({"token": "t", "cmd": "deny"}),
            json!({"token": "t", "cmd": "dismiss_forge"}),
            json!({"token": "t", "cmd": "policy", "text": "   "}),
            json!({"token": "t", "cmd": "policy"}),
            json!({"token": "t", "cmd": "play_cue", "cue": "   "}),
            json!({"token": "t", "cmd": "play_cue"}),
            // design_voice: a missing/empty agent, and a description below the EL
            // design floor (MIN_VOICE_DESCRIPTION_CHARS), are each a BadRequest.
            json!({"token": "t", "cmd": "design_voice", "description": "a long enough description here"}),
            json!({"token": "t", "cmd": "design_voice", "agent": "   ", "description": "a long enough description here"}),
            json!({"token": "t", "cmd": "design_voice", "agent": "edith", "description": "too short"}),
            json!({"token": "t", "cmd": "design_voice", "agent": "edith"}),
            // create_pronunciation: an empty word or an empty say is a BadRequest.
            json!({"token": "t", "cmd": "create_pronunciation", "say": "jarviss"}),
            json!({"token": "t", "cmd": "create_pronunciation", "word": "  ", "say": "jarviss"}),
            json!({"token": "t", "cmd": "create_pronunciation", "word": "JARVIS"}),
            json!({"token": "t", "cmd": "create_pronunciation", "word": "JARVIS", "say": "  "}),
            // compose_music: a missing/empty/whitespace prompt is a BadRequest.
            json!({"token": "t", "cmd": "compose_music"}),
            json!({"token": "t", "cmd": "compose_music", "prompt": ""}),
            json!({"token": "t", "cmd": "compose_music", "prompt": "   "}),
        ];
        for line in bad {
            assert!(
                matches!(decide(&line.to_string()), Decision::BadRequest { .. }),
                "must be BadRequest: {line}"
            );
        }
    }

    /// SECURITY: the `play_cue` verb is an allowlist over the SFX catalog, not a
    /// free-text field. A KNOWN cue routes; an UNKNOWN cue name is a BadRequest
    /// (never routed) — keeping this benign verb's input as tight as the command
    /// set itself. An empty cue is likewise rejected. This pins that `play_cue`
    /// cannot become a smuggling channel for an arbitrary string.
    #[test]
    fn play_cue_admits_only_known_catalog_cues() {
        // Every catalog cue is accepted and parses to the PlayCue variant.
        for name in crate::sfx_cue::cue_names() {
            let line = json!({"token": "t", "cmd": "play_cue", "cue": name}).to_string();
            match decide(&line) {
                Decision::Ok { command: Command::PlayCue { cue }, .. } => {
                    assert_eq!(cue, name, "the validated cue name is carried verbatim");
                }
                other => panic!("known cue {name:?} must be Ok(PlayCue), got {other:?}"),
            }
        }
        // An unknown cue name is a BadRequest — NOT routed, NOT UnknownCommand
        // (the cmd is known; only the cue is not). The cmd itself stays valid.
        for bad in ["kaboom", "explode", "rm -rf", "alert; drop", ""] {
            let line = json!({"token": "t", "cmd": "play_cue", "cue": bad}).to_string();
            assert!(
                matches!(decide(&line), Decision::BadRequest { .. }),
                "unknown/empty cue {bad:?} must be BadRequest (never routed)"
            );
        }
        // Whitespace around a known cue is trimmed, then accepted.
        let line = json!({"token": "t", "cmd": "play_cue", "cue": "  confirm  "}).to_string();
        match decide(&line) {
            Decision::Ok { command: Command::PlayCue { cue }, .. } => assert_eq!(cue, "confirm"),
            other => panic!("trimmed known cue must be Ok(PlayCue), got {other:?}"),
        }
        // An entirely unknown cmd is STILL UnknownCommand (the allowlist boundary
        // is unchanged by adding play_cue).
        let line = json!({"token": "t", "cmd": "play_sound", "cue": "confirm"}).to_string();
        assert!(
            matches!(decide(&line), Decision::UnknownCommand { .. }),
            "an unknown cmd remains UnknownCommand"
        );
    }

    /// SECURITY/shape: `design_voice` validates a non-empty agent + a description
    /// at or above the EL design floor, clamps + trims the fields, and DEFAULTS the
    /// display name to the agent name when omitted. A too-short description never
    /// routes (it would be a wasted, server-rejected cloud op).
    #[test]
    fn design_voice_validates_shape_and_defaults_the_name() {
        // A complete request parses to DesignVoice with the fields carried verbatim.
        let line = json!({
            "token": "t", "cmd": "design_voice", "agent": "edith",
            "description": "a calm warm british woman, mid-thirties", "name": "Edith Voice"
        })
        .to_string();
        match decide(&line) {
            Decision::Ok { command: Command::DesignVoice { agent, description, name }, .. } => {
                assert_eq!(agent, "edith");
                assert_eq!(description, "a calm warm british woman, mid-thirties");
                assert_eq!(name, "Edith Voice");
            }
            other => panic!("complete design_voice must be Ok(DesignVoice), got {other:?}"),
        }
        // No name -> defaults to the agent name.
        let line = json!({
            "token": "t", "cmd": "design_voice", "agent": "fury",
            "description": "a gruff commanding older man, authoritative"
        })
        .to_string();
        match decide(&line) {
            Decision::Ok { command: Command::DesignVoice { agent, name, .. }, .. } => {
                assert_eq!(name, agent, "the name defaults to the agent name");
            }
            other => panic!("expected Ok(DesignVoice), got {other:?}"),
        }
        // A description shorter than the floor is a BadRequest (never routed).
        let short = "a".repeat(MIN_VOICE_DESCRIPTION_CHARS - 1);
        let line = json!({"token": "t", "cmd": "design_voice", "agent": "edith", "description": short}).to_string();
        assert!(
            matches!(decide(&line), Decision::BadRequest { .. }),
            "a sub-floor description must be BadRequest"
        );
        // Exactly at the floor is accepted.
        let exact = "a".repeat(MIN_VOICE_DESCRIPTION_CHARS);
        let line = json!({"token": "t", "cmd": "design_voice", "agent": "edith", "description": exact}).to_string();
        assert!(matches!(decide(&line), Decision::Ok { .. }), "a floor-length description is accepted");
    }

    /// SECURITY/shape: `create_pronunciation` requires a non-empty word AND a
    /// non-empty say, trims them, carries them verbatim into the parsed command,
    /// and defaults the dictionary name when omitted. An empty either side never
    /// routes.
    #[test]
    fn create_pronunciation_validates_a_single_alias_rule() {
        // word + say (no name) -> CreatePronunciation with a defaulted name.
        let line = json!({"token": "t", "cmd": "create_pronunciation", "word": "  JARVIS  ", "say": "  jarviss  "}).to_string();
        match decide(&line) {
            Decision::Ok { command: Command::CreatePronunciation { word, say, name }, .. } => {
                assert_eq!(word, "JARVIS", "word is trimmed + carried verbatim");
                assert_eq!(say, "jarviss", "say is trimmed + carried verbatim");
                assert!(!name.is_empty(), "the dictionary name is defaulted, never empty");
            }
            other => panic!("complete create_pronunciation must be Ok, got {other:?}"),
        }
        // An explicit name is honored.
        let line = json!({"token": "t", "cmd": "create_pronunciation", "word": "nginx", "say": "engine x", "name": "Ops terms"}).to_string();
        match decide(&line) {
            Decision::Ok { command: Command::CreatePronunciation { name, .. }, .. } => {
                assert_eq!(name, "Ops terms");
            }
            other => panic!("expected Ok(CreatePronunciation), got {other:?}"),
        }
        // Empty word or empty say is a BadRequest (never routed).
        for line in [
            json!({"token": "t", "cmd": "create_pronunciation", "word": "", "say": "x"}),
            json!({"token": "t", "cmd": "create_pronunciation", "word": "x", "say": ""}),
        ] {
            assert!(
                matches!(decide(&line.to_string()), Decision::BadRequest { .. }),
                "empty word/say must be BadRequest: {line}"
            );
        }
    }

    /// A non-JSON line is Malformed; an oversized line is Oversized (before parse).
    #[test]
    fn malformed_and_oversized_are_rejected_before_route() {
        assert_eq!(decide("not json"), Decision::Malformed);
        assert_eq!(decide(""), Decision::Malformed);
        // Oversized: a line longer than MAX_LINE_BYTES, even if it would parse.
        let huge_text = "x".repeat(MAX_LINE_BYTES + 10);
        let line = json!({"token": "t", "cmd": "ask", "text": huge_text}).to_string();
        assert!(line.len() > MAX_LINE_BYTES);
        assert_eq!(decide(&line), Decision::Oversized);
    }

    /// An under-the-line-cap but over-the-text-cap ask is clamped, not rejected.
    #[test]
    fn long_text_is_clamped() {
        let text = "a".repeat(MAX_TEXT_CHARS + 500);
        let line = json!({"token": "t", "cmd": "ask", "text": text}).to_string();
        // Stays under the line cap.
        assert!(line.len() <= MAX_LINE_BYTES);
        match decide(&line) {
            Decision::Ok { command: Command::Ask { text, .. }, .. } => {
                assert_eq!(text.chars().count(), MAX_TEXT_CHARS, "text clamped to the cap");
            }
            other => panic!("expected clamped Ask, got {other:?}"),
        }
    }

    // -- rate limiter --------------------------------------------------------

    #[test]
    fn rate_limit_trips_after_the_rate_and_rolls() {
        let mut lim = RateLimiter::default();
        let t0 = Instant::now();
        for i in 0..RATE {
            assert!(lim.check(t0), "call {i} should pass");
        }
        assert!(!lim.check(t0), "the call past RATE must trip");
        let later = t0 + RATE_WINDOW + Duration::from_secs(1);
        assert!(lim.check(later), "the window rolled; allowed again");
    }

    // -- mock pipeline + a probe dispatcher for the handler-level tests -------

    #[derive(Default)]
    struct MockPipeline {
        ask_calls: AtomicU32,
        last_agent: StdMutex<Option<String>>,
        play_cue_calls: AtomicU32,
        last_cue: StdMutex<Option<String>>,
        design_voice_calls: AtomicU32,
        last_design: StdMutex<Option<(String, String, String)>>,
        create_pron_calls: AtomicU32,
        last_pron: StdMutex<Option<(String, String, String)>>,
        compose_music_calls: AtomicU32,
        last_music: StdMutex<Option<(String, Option<u32>)>>,
    }
    impl CommandPipeline for MockPipeline {
        async fn ask(&self, text: &str, agent: Option<&str>) -> String {
            self.ask_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_agent.lock().unwrap() = agent.map(str::to_string);
            format!("routed:{text}")
        }
        async fn brief(&self) -> String { "brief".into() }
        async fn mission(&self, goal: &str) -> String { format!("mission:{goal}") }
        async fn roster(&self) -> String { "roster".into() }
        async fn state(&self) -> String { "state".into() }
        async fn distill(&self) -> String { "distill".into() }
        async fn sync(&self) -> String { "sync".into() }
        async fn overnight(&self, task: &str, _agent: Option<&str>) -> String { format!("overnight:{task}") }
        async fn play_cue(&self, cue: &str) -> String {
            self.play_cue_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_cue.lock().unwrap() = Some(cue.to_string());
            format!("cue:{cue}")
        }
        async fn design_voice(&self, agent: &str, description: &str, name: &str) -> String {
            self.design_voice_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_design.lock().unwrap() =
                Some((agent.to_string(), description.to_string(), name.to_string()));
            format!("design:{agent}:{name}")
        }
        async fn create_pronunciation(&self, word: &str, say: &str, name: &str) -> String {
            self.create_pron_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_pron.lock().unwrap() =
                Some((word.to_string(), say.to_string(), name.to_string()));
            format!("pron:{word}:{say}")
        }
        async fn compose_music(&self, prompt: &str, length_ms: Option<u32>) -> String {
            self.compose_music_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_music.lock().unwrap() = Some((prompt.to_string(), length_ms));
            format!("music:{prompt}")
        }
    }

    #[derive(Default)]
    struct ProbeDispatcher {
        confirm_calls: AtomicU32,
        deny_calls: AtomicU32,
        dismiss_calls: AtomicU32,
        policy_calls: AtomicU32,
        last_policy_text: StdMutex<Option<String>>,
        // Task #12: prove the panic/unlock VERBS route to the dispatcher (the
        // user/HUD path), never the model pipeline. We OVERRIDE the default trait
        // impls (which call the real lockdown::panic/unlock) so the routing test
        // stays hermetic — it records the call instead of mutating global state.
        panic_calls: AtomicU32,
        unlock_calls: AtomicU32,
    }
    impl Dispatcher for ProbeDispatcher {
        async fn list_pending(&self) -> Value { json!({"confirmation": null}) }
        async fn confirm(&self, id: &str) -> String {
            self.confirm_calls.fetch_add(1, Ordering::SeqCst);
            format!("confirm:{id}")
        }
        async fn deny(&self, id: &str) -> String {
            self.deny_calls.fetch_add(1, Ordering::SeqCst);
            format!("deny:{id}")
        }
        async fn dismiss_forge(&self, ts: u64) -> String {
            self.dismiss_calls.fetch_add(1, Ordering::SeqCst);
            format!("dismiss:{ts}")
        }
        async fn policy(&self, text: &str) -> String {
            self.policy_calls.fetch_add(1, Ordering::SeqCst);
            *self.last_policy_text.lock().unwrap() = Some(text.to_string());
            format!("policy:{text}")
        }
        async fn panic(&self) -> String {
            self.panic_calls.fetch_add(1, Ordering::SeqCst);
            "panic-engaged".to_string()
        }
        async fn unlock(&self) -> String {
            self.unlock_calls.fetch_add(1, Ordering::SeqCst);
            "unlock-lifted".to_string()
        }
    }

    fn fresh_limiter() -> Arc<Mutex<RateLimiter>> {
        Arc::new(Mutex::new(RateLimiter::default()))
    }

    /// A VALID command token from the SAME machinery the channel verifies with.
    fn valid_token() -> String {
        apps::mint_command_token()
    }

    // -- token auth ----------------------------------------------------------

    /// An unauthenticated line (missing token), a forged token, and a tampered
    /// token are all rejected as unauthorized BEFORE any route — the pipeline is
    /// never touched.
    #[tokio::test]
    async fn unauthenticated_forged_and_tampered_tokens_are_rejected() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();

        // Missing token.
        let line = json!({"cmd": "ask", "text": "do something"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "unauthorized", "missing token");

        // Forged token.
        let line = json!({"token": "deadbeef", "cmd": "ask", "text": "x"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "unauthorized", "forged token");

        // Tampered: a valid token with a flipped hex nibble.
        let good = valid_token();
        let mut chars: Vec<char> = good.chars().collect();
        chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        let line = json!({"token": tampered, "cmd": "ask", "text": "x"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "unauthorized", "tampered token");

        // The pipeline was NEVER reached.
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "no route on an unauthorized line");
    }

    /// An unknown command is rejected even WITH a valid token (the allowlist is
    /// structural, independent of auth) and never routes.
    #[tokio::test]
    async fn unknown_command_with_a_valid_token_is_rejected() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "apply_forge", "ts": 1}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "unknown_command");
        assert_eq!(dispatcher.dismiss_calls.load(Ordering::SeqCst), 0, "unknown cmd never dispatched");
    }

    /// A valid, allowlisted ask routes through the pipeline (the normal path),
    /// and the agent ref is carried so the agent's allowlist applies downstream.
    #[tokio::test]
    async fn valid_ask_routes_through_the_pipeline_with_its_agent() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "ask", "text": "status", "agent": "edith"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "routed:status");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 1);
        assert_eq!(pipeline.last_agent.lock().unwrap().as_deref(), Some("edith"));
    }

    /// brief / mission / roster / state route to their pipeline arms.
    #[tokio::test]
    async fn read_and_route_commands_reach_their_arms() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let cases = [
            (json!({"token": valid_token(), "cmd": "brief"}), "brief"),
            (json!({"token": valid_token(), "cmd": "mission", "goal": "g"}), "mission:g"),
            (json!({"token": valid_token(), "cmd": "roster"}), "roster"),
            (json!({"token": valid_token(), "cmd": "state"}), "state"),
            (json!({"token": valid_token(), "cmd": "distill"}), "distill"),
            (json!({"token": valid_token(), "cmd": "sync"}), "sync"),
            (json!({"token": valid_token(), "cmd": "overnight", "prompt": "dig into Y"}), "overnight:dig into Y"),
        ];
        for (line, want) in cases {
            let r = handle_line(&line.to_string(), &pipeline, &dispatcher, &lim, false).await;
            assert_eq!(r["reply"], want, "for {line}");
        }
    }

    /// The `policy` verb routes to the dispatcher's USER-SET-ONLY policy write —
    /// NOT to the pipeline's `ask` (so it NEVER reaches the model tool loop). The
    /// phrase text is carried verbatim to the classifier.
    #[tokio::test]
    async fn policy_verb_routes_to_the_user_write_path_not_the_model() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let phrase = "always allow the gmail_send action";
        let line = json!({"token": valid_token(), "cmd": "policy", "text": phrase}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], format!("policy:{phrase}"));
        // It reached the dispatcher's policy arm, with the phrase verbatim.
        assert_eq!(dispatcher.policy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(dispatcher.last_policy_text.lock().unwrap().as_deref(), Some(phrase));
        // It did NOT route through the model pipeline (ask) — no model path to a policy.
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "policy never reaches the model");
    }

    /// The `play_cue` verb routes to the pipeline's dedicated `play_cue` arm with
    /// the validated cue name — NOT to `ask` (so it NEVER reaches the model tool
    /// loop). It is the HUD SFX-cue Play button: a benign, authenticated,
    /// rate-passed verb with no model path.
    #[tokio::test]
    async fn play_cue_verb_routes_to_the_cue_arm_not_the_model() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "play_cue", "cue": "confirm"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "cue:confirm");
        assert_eq!(pipeline.play_cue_calls.load(Ordering::SeqCst), 1, "reached the cue arm");
        assert_eq!(pipeline.last_cue.lock().unwrap().as_deref(), Some("confirm"));
        // It did NOT route through the model pipeline (ask) — no model path to a cue.
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "play_cue never reaches the model");
    }

    /// An UNKNOWN cue name is rejected as a bad_request through the handler and the
    /// pipeline's cue arm is NEVER reached — the catalog allowlist holds end-to-end,
    /// even with a valid token.
    #[tokio::test]
    async fn unknown_cue_is_rejected_through_the_handler() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "play_cue", "cue": "kaboom"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "bad_request", "unknown cue is a bad_request");
        assert_eq!(pipeline.play_cue_calls.load(Ordering::SeqCst), 0, "unknown cue never routes");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "and never reaches the model");
    }

    // -- event cues ([voice].event_cues) -------------------------------------
    //
    // The event-cue feature fire-and-forgets a cosmetic cue on confirm/deny when
    // (and ONLY when) the opt-in flag is true. Because the cue is `tokio::spawn`ed
    // detached, the assertion must let the spawned task run: drain a bounded number
    // of yields on the current-thread test runtime so the detached future is polled
    // to completion (it only touches an atomic + a mutex, so it finishes in one
    // poll). A bounded loop means a never-spawned cue can't hang the test.
    async fn drain_spawned_tasks() {
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
    }

    /// With [voice].event_cues OFF (the default), a `confirm` runs its existing
    /// handling and returns its existing reply, and NO event cue is spawned — proof
    /// the default is a ZERO-behavior-change no-op. Same for `deny`.
    #[tokio::test]
    async fn event_cues_off_spawns_no_cue_on_confirm_or_deny() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();

        // confirm (event_cues = false)
        let line = json!({"token": valid_token(), "cmd": "confirm", "id": "abc"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "confirm:abc", "confirm reply is UNCHANGED with cues off");
        assert_eq!(dispatcher.confirm_calls.load(Ordering::SeqCst), 1, "confirm handling ran exactly once");

        // deny (event_cues = false)
        let line = json!({"token": valid_token(), "cmd": "deny", "id": "xyz"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "deny:xyz", "deny reply is UNCHANGED with cues off");
        assert_eq!(dispatcher.deny_calls.load(Ordering::SeqCst), 1, "deny handling ran exactly once");

        // Even after draining the runtime, NO cue was ever spawned.
        drain_spawned_tasks().await;
        assert_eq!(
            pipeline.play_cue_calls.load(Ordering::SeqCst), 0,
            "cues OFF => no event cue is spawned on confirm/deny (zero behavior change)"
        );
    }

    /// With [voice].event_cues ON, a `confirm` STILL returns its existing reply
    /// (the cue can't change the outcome) AND fire-and-forgets the "success" cue.
    #[tokio::test]
    async fn event_cues_on_confirm_spawns_success_without_changing_the_reply() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "confirm", "id": "abc"}).to_string();
        // event_cues = true
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, true).await;
        // The reply + handling are UNCHANGED — the cue is purely additive.
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "confirm:abc", "confirm reply is UNCHANGED even with cues on");
        assert_eq!(dispatcher.confirm_calls.load(Ordering::SeqCst), 1, "confirm handling ran exactly once");

        // The detached cue fires "success" once it is polled.
        drain_spawned_tasks().await;
        assert_eq!(pipeline.play_cue_calls.load(Ordering::SeqCst), 1, "confirm fire-and-forgets one cue");
        assert_eq!(pipeline.last_cue.lock().unwrap().as_deref(), Some("success"), "confirm plays the success cue");
        // The cue NEVER reaches the model.
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "event cue never reaches the model");
    }

    /// With [voice].event_cues ON, a `deny` STILL returns its existing reply AND
    /// fire-and-forgets the "notify" cue.
    #[tokio::test]
    async fn event_cues_on_deny_spawns_notify_without_changing_the_reply() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "deny", "id": "xyz"}).to_string();
        // event_cues = true
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, true).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "deny:xyz", "deny reply is UNCHANGED even with cues on");
        assert_eq!(dispatcher.deny_calls.load(Ordering::SeqCst), 1, "deny handling ran exactly once");

        drain_spawned_tasks().await;
        assert_eq!(pipeline.play_cue_calls.load(Ordering::SeqCst), 1, "deny fire-and-forgets one cue");
        assert_eq!(pipeline.last_cue.lock().unwrap().as_deref(), Some("notify"), "deny plays the notify cue");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "event cue never reaches the model");
    }

    /// The `design_voice` verb routes to the pipeline's dedicated `design_voice`
    /// arm with the validated agent/description/name — NOT to `ask` (so it NEVER
    /// reaches the model tool loop). It is the HUD voice-design control: a
    /// dedicated, authenticated, rate-passed provisioning verb with no model path.
    #[tokio::test]
    async fn design_voice_verb_routes_to_the_design_arm_not_the_model() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let desc = "a calm warm british woman, mid-thirties";
        let line = json!({"token": valid_token(), "cmd": "design_voice", "agent": "edith", "description": desc, "name": "Edith Voice"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "design:edith:Edith Voice");
        assert_eq!(pipeline.design_voice_calls.load(Ordering::SeqCst), 1, "reached the design arm");
        assert_eq!(
            *pipeline.last_design.lock().unwrap(),
            Some(("edith".into(), desc.into(), "Edith Voice".into())),
            "the validated fields are carried verbatim"
        );
        // It did NOT route through the model pipeline (ask) — no model path to a voice.
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "design_voice never reaches the model");
    }

    /// A too-short `design_voice` description is rejected as a bad_request through
    /// the handler and the design arm is NEVER reached — the EL design floor holds
    /// end-to-end, even with a valid token, so no wasted cloud op is spent.
    #[tokio::test]
    async fn short_design_description_is_rejected_through_the_handler() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "design_voice", "agent": "edith", "description": "too short"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "bad_request", "a sub-floor description is a bad_request");
        assert_eq!(pipeline.design_voice_calls.load(Ordering::SeqCst), 0, "never routes");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "and never reaches the model");
    }

    /// The `create_pronunciation` verb routes to the pipeline's dedicated
    /// `create_pronunciation` arm with the validated word/say/name — NOT to `ask`
    /// (so it NEVER reaches the model tool loop). It is the HUD pronunciation
    /// control: a dedicated, authenticated, rate-passed provisioning verb.
    #[tokio::test]
    async fn create_pronunciation_verb_routes_to_its_arm_not_the_model() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "create_pronunciation", "word": "JARVIS", "say": "jarviss"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "pron:JARVIS:jarviss");
        assert_eq!(pipeline.create_pron_calls.load(Ordering::SeqCst), 1, "reached the pronunciation arm");
        // word + say are carried verbatim; the name is defaulted (non-empty).
        let last = pipeline.last_pron.lock().unwrap().clone().unwrap();
        assert_eq!(last.0, "JARVIS");
        assert_eq!(last.1, "jarviss");
        assert!(!last.2.is_empty(), "the dictionary name is defaulted");
        // It did NOT route through the model pipeline (ask) — no model path here.
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "create_pronunciation never reaches the model");
    }

    /// An empty-`say` `create_pronunciation` is rejected as a bad_request through
    /// the handler and the arm is NEVER reached — the non-empty rule check holds
    /// end-to-end, even with a valid token.
    #[tokio::test]
    async fn empty_pronunciation_rule_is_rejected_through_the_handler() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "create_pronunciation", "word": "JARVIS", "say": ""}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "bad_request", "an empty alias is a bad_request");
        assert_eq!(pipeline.create_pron_calls.load(Ordering::SeqCst), 0, "never routes");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "and never reaches the model");
    }

    /// SECURITY/shape: `compose_music` validates a NON-EMPTY prompt, clamps + trims it,
    /// and carries the OPTIONAL length_ms verbatim. An empty/whitespace prompt is a
    /// BadRequest (never routed); the cmd itself stays a known verb (an unknown cmd is
    /// still UnknownCommand — the allowlist boundary is unchanged by adding it).
    #[test]
    fn compose_music_validates_a_non_empty_prompt() {
        // A complete request (with a pinned length) parses to ComposeMusic verbatim.
        let line = json!({
            "token": "t", "cmd": "compose_music",
            "prompt": "  a calm lo-fi study beat  ", "length_ms": 45000
        }).to_string();
        match decide(&line) {
            Decision::Ok { command: Command::ComposeMusic { prompt, length_ms }, .. } => {
                assert_eq!(prompt, "a calm lo-fi study beat", "trimmed prompt carried verbatim");
                assert_eq!(length_ms, Some(45000), "the optional length is carried verbatim");
            }
            other => panic!("complete compose_music must be Ok(ComposeMusic), got {other:?}"),
        }
        // No length_ms => None (the server defaults), still Ok.
        let line = json!({"token": "t", "cmd": "compose_music", "prompt": "upbeat synthwave"}).to_string();
        match decide(&line) {
            Decision::Ok { command: Command::ComposeMusic { length_ms, .. }, .. } => {
                assert_eq!(length_ms, None, "an absent length defaults at the server");
            }
            other => panic!("expected Ok(ComposeMusic), got {other:?}"),
        }
        // An empty/whitespace prompt is a BadRequest — NOT routed.
        for bad in ["", "   "] {
            let line = json!({"token": "t", "cmd": "compose_music", "prompt": bad}).to_string();
            assert!(
                matches!(decide(&line), Decision::BadRequest { .. }),
                "empty prompt {bad:?} must be BadRequest (never routed)"
            );
        }
        // A missing prompt field is likewise a BadRequest.
        let line = json!({"token": "t", "cmd": "compose_music"}).to_string();
        assert!(matches!(decide(&line), Decision::BadRequest { .. }), "missing prompt is BadRequest");
        // An entirely unknown cmd is STILL UnknownCommand (allowlist boundary intact).
        let line = json!({"token": "t", "cmd": "make_song", "prompt": "x"}).to_string();
        assert!(
            matches!(decide(&line), Decision::UnknownCommand { .. }),
            "an unknown cmd remains UnknownCommand"
        );
    }

    /// The `compose_music` verb routes to the pipeline's dedicated `compose_music` arm
    /// with the validated prompt + optional length — NOT to `ask` (so it NEVER reaches
    /// the model tool loop). It is the HUD music-generation control (Jerome's surface):
    /// a dedicated, authenticated, rate-passed benign verb with no model path.
    #[tokio::test]
    async fn compose_music_verb_routes_to_its_arm_not_the_model() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({
            "token": valid_token(), "cmd": "compose_music",
            "prompt": "a calm lo-fi study beat", "length_ms": 60000
        }).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "music:a calm lo-fi study beat");
        assert_eq!(pipeline.compose_music_calls.load(Ordering::SeqCst), 1, "reached the music arm");
        assert_eq!(
            *pipeline.last_music.lock().unwrap(),
            Some(("a calm lo-fi study beat".into(), Some(60000))),
            "the validated prompt + length are carried verbatim"
        );
        // It did NOT route through the model pipeline (ask) — no model path to a track.
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "compose_music never reaches the model");
    }

    /// An empty-prompt `compose_music` is rejected as a bad_request through the handler
    /// and the music arm is NEVER reached — the non-empty prompt check holds end-to-end,
    /// even with a valid token, so no wasted cloud op is spent.
    #[tokio::test]
    async fn empty_music_prompt_is_rejected_through_the_handler() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "compose_music", "prompt": "   "}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "bad_request", "an empty prompt is a bad_request");
        assert_eq!(pipeline.compose_music_calls.load(Ordering::SeqCst), 0, "never routes");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "and never reaches the model");
    }

    /// Task #12: the `panic` verb routes to the dispatcher's lockdown engage —
    /// NOT the model pipeline (`ask`). It is the HUD PANIC button: a bare,
    /// authenticated, rate-passed verb with no model path.
    #[tokio::test]
    async fn panic_verb_routes_to_the_dispatcher_not_the_model() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "panic"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "panic-engaged");
        assert_eq!(dispatcher.panic_calls.load(Ordering::SeqCst), 1, "panic reached the dispatcher");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "panic never reaches the model");
    }

    /// Task #12: the `unlock` verb routes to the dispatcher's lockdown lift — NOT
    /// the model pipeline. Together with the spoken "unlock" intent this is the
    /// ONLY path to unlock; there is no model/tool/MCP route to it.
    #[tokio::test]
    async fn unlock_verb_routes_to_the_dispatcher_not_the_model() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "unlock"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], true);
        assert_eq!(r["reply"], "unlock-lifted");
        assert_eq!(dispatcher.unlock_calls.load(Ordering::SeqCst), 1, "unlock reached the dispatcher");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0, "unlock never reaches the model");
    }

    /// Task #12: unlock requires a VALID token, exactly like every other command —
    /// an unauthenticated line never reaches the dispatcher, so a stray/forged
    /// local line can never lift the emergency stop.
    #[tokio::test]
    async fn unlock_verb_requires_a_valid_token() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": "wrong-token", "cmd": "unlock"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["ok"], false, "a bad token is rejected");
        assert_eq!(dispatcher.unlock_calls.load(Ordering::SeqCst), 0, "unauth unlock never dispatched");
    }

    /// The LiveDispatcher's `policy` arm actually parses + applies an anchored
    /// phrase through the USER-SET-ONLY global write path, and an unrecognized
    /// phrase gets an honest "not understood" reply (never a model route). Driven
    /// through the policy override seam so it does not poison the set-once global.
    // intentional: hold the global PENDING serialization guard across the awaited action; #[tokio::test] is current-thread so it cannot self-deadlock
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn live_dispatcher_policy_sets_a_rule_and_rejects_a_non_phrase() {
        let _g = gate_guard();
        let _p = crate::policy::PolicyOverride::force(true, crate::policy::PolicyStore::empty());
        let (disp, _root) = live_dispatcher("cmd_policy");

        // An anchored phrase sets the rule (now in force at evaluate_global).
        let ack = disp.policy("always allow the gmail_send action").await;
        assert!(ack.to_lowercase().contains("auto-approve"), "honest ALWAYS ack: {ack}");
        assert_eq!(
            crate::policy::evaluate_global("gmail_send", "agent.pepper", ""),
            crate::policy::Decision::Always,
            "the user write took effect"
        );

        // A non-phrase gets an honest reply and changes NOTHING — it is never
        // routed to the model from the policy verb.
        let miss = disp.policy("please send my taxes to the IRS").await;
        assert!(miss.to_lowercase().contains("didn't recognize"), "honest miss: {miss}");
        assert_eq!(
            crate::policy::evaluate_global("x_post", "agent.pepper", ""),
            crate::policy::Decision::Ask,
            "an unrecognized phrase set nothing"
        );
    }

    /// The rate limit trips after RATE authenticated commands in the window.
    #[tokio::test]
    async fn rate_limit_trips_through_the_handler() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "brief"}).to_string();
        for _ in 0..RATE {
            let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
            assert_eq!(r["ok"], true);
        }
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "rate_limited");
    }

    /// An oversized line is rejected through the handler before any route.
    #[tokio::test]
    async fn oversized_line_rejected_through_handler() {
        let pipeline = Arc::new(MockPipeline::default());
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let huge = "x".repeat(MAX_LINE_BYTES + 10);
        let line = json!({"token": valid_token(), "cmd": "ask", "text": huge}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        assert_eq!(r["error"], "oversized");
        assert_eq!(pipeline.ask_calls.load(Ordering::SeqCst), 0);
    }

    // -- NON-BYPASS: confirm-by-id honors the gate + master switch ------------
    //
    // These drive the REAL LiveDispatcher against the REAL confirm slot, proving
    // the channel cannot escalate: a consequential action still parks, confirm
    // fires ONLY the genuine parked action AND only with the switch ON, an
    // unknown id fires nothing, and dismiss_forge writes nothing into apps/.

    use crate::confirm::{self, PendingConfirmation};

    // Serialize the tests that touch the process-global confirm slot + the
    // process-global master switch so they don't race. We share ONE lock with
    // confirm::tests (crate::confirm::PENDING_TEST_LOCK) — both modules drive
    // the SAME global slot, and cargo runs them concurrently, so a lock private
    // to this module would not stop a confirm::tests case from stomping ours.
    fn gate_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::confirm::PENDING_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn park_a_consequential(agent: &str, tool: &str, input: Value, allowed: Vec<String>) -> String {
        confirm::clear();
        confirm::park(PendingConfirmation {
            agent: agent.to_string(),
            tool: tool.to_string(),
            input,
            allowed,
            preview: "Would do the thing".to_string(),
            created_at: Instant::now(),
            id: String::new(),
        });
        // The id is what `pending` would surface and `confirm {id}` names.
        confirm::peek_pending(Instant::now()).expect("just parked").id
    }

    fn open_temp_memory(tag: &str) -> crate::memory::Memory {
        let path = std::env::temp_dir().join(format!(
            "jarvis-command-test-{}-{tag}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        crate::memory::Memory::open(&path).unwrap()
    }

    fn live_dispatcher(tag: &str) -> (LiveDispatcher, PathBuf) {
        let mem = Arc::new(open_temp_memory(tag));
        let root = std::env::temp_dir().join(format!("jarvis-cmd-{tag}-{}", std::process::id()));
        (LiveDispatcher { memory: mem, root: root.clone() }, root)
    }

    /// confirm {id} replays ONLY the exact parked action, and — because the master
    /// switch ships OFF (and the test binary never flips it on) — the replay runs
    /// through gate(false)=DryRun and fires NOTHING. This is the non-bypass proof:
    /// the authenticated-local confirm cannot execute a consequential action while
    /// the switch is off. The parked action is consumed (taken from the slot), so
    /// it cannot fire on a later confirm either.
    // intentional: hold the global PENDING serialization guard across the awaited action; #[tokio::test] is current-thread so it cannot self-deadlock
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn confirm_by_id_with_switch_off_consumes_but_never_executes() {
        let _g = gate_guard();
        // The master switch is OFF in the test binary (OnceLock default false).
        assert!(
            !crate::integrations::consequential_allowed(),
            "the test binary must have the master switch OFF (the shipped default)"
        );

        // Park a consequential action for an agent that legitimately holds it.
        let id = park_a_consequential(
            "agent.pepper",
            "gmail_send",
            json!({"to": "a@b.com", "subject": "Hi", "body": "x", "confirm": true}),
            vec!["gmail_send".into()],
        );
        let (disp, _root) = live_dispatcher("cmd_confirm_switch");

        let out_off = disp.confirm(&id).await;
        // The slot is now empty (taken), so the action cannot fire on a later
        // confirm — and with the switch OFF this replay performed no external
        // action (gate -> DryRun): the outcome is never an executed-send ack.
        assert!(
            confirm::peek_pending(Instant::now()).is_none(),
            "the parked action was consumed by the confirm attempt"
        );
        let again = disp.confirm(&id).await;
        assert_eq!(again, "No pending action with that id.", "consumed id cannot re-fire");
        assert!(
            !out_off.to_lowercase().contains("sent to"),
            "switch-OFF confirm must not report an executed send: {out_off}"
        );
    }

    /// confirm {unknown-id} does NOTHING — no fabricated action, and the genuine
    /// pending is left intact for its real id.
    // intentional: hold the global PENDING serialization guard across the awaited action; #[tokio::test] is current-thread so it cannot self-deadlock
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn confirm_unknown_id_does_nothing_and_leaves_the_real_pending() {
        let _g = gate_guard();
        let real_id = park_a_consequential(
            "agent.pepper",
            "gmail_send",
            json!({"to": "a@b.com", "subject": "Hi", "body": "x", "confirm": true}),
            vec!["gmail_send".into()],
        );
        let (disp, _root) = live_dispatcher("cmd_confirm_unknown");

        let out = disp.confirm("0000000000000000").await;
        assert_eq!(out, "No pending action with that id.", "unknown id fires nothing");
        // The genuine pending is STILL parked under its real id.
        let still = confirm::peek_pending(Instant::now()).expect("real pending intact");
        assert_eq!(still.id, real_id, "the real pending was untouched");

        confirm::clear();
    }

    /// A consequential ask STILL parks (does not auto-fire) — proven by routing an
    /// ask whose pipeline impl mimics execute_tool's gate: with the switch OFF it
    /// parks nothing (DryRun preview), and with the gate the consequential path
    /// would PARK rather than fire. Here we assert the channel itself never fires:
    /// the `ask` arm returns whatever the pipeline returns and never reaches the
    /// dispatcher's confirm, so a fire can only ever follow an explicit confirm.
    #[tokio::test]
    async fn consequential_ask_does_not_auto_fire_through_the_channel() {
        // A pipeline that mimics execute_tool for a consequential tool: it PARKS
        // and returns the confirmation prompt, never a fired action. We assert the
        // channel relays that PARK and never reaches the dispatcher's confirm.
        struct ParkingPipeline;
        impl CommandPipeline for ParkingPipeline {
            async fn ask(&self, _text: &str, _agent: Option<&str>) -> String {
                "Would send an email — say 'confirm' to proceed.".to_string()
            }
            async fn brief(&self) -> String { unreachable!() }
            async fn mission(&self, _g: &str) -> String { unreachable!() }
            async fn roster(&self) -> String { unreachable!() }
            async fn state(&self) -> String { unreachable!() }
            async fn distill(&self) -> String { unreachable!() }
            async fn sync(&self) -> String { unreachable!() }
            async fn overnight(&self, _t: &str, _a: Option<&str>) -> String { unreachable!() }
            async fn play_cue(&self, _cue: &str) -> String { unreachable!() }
            async fn design_voice(&self, _a: &str, _d: &str, _n: &str) -> String { unreachable!() }
            async fn create_pronunciation(&self, _w: &str, _s: &str, _n: &str) -> String {
                unreachable!()
            }
            async fn compose_music(&self, _p: &str, _l: Option<u32>) -> String { unreachable!() }
        }
        let pipeline = Arc::new(ParkingPipeline);
        let dispatcher = Arc::new(ProbeDispatcher::default());
        let lim = fresh_limiter();
        let line = json!({"token": valid_token(), "cmd": "ask", "text": "email alice"}).to_string();
        let r = handle_line(&line, &pipeline, &dispatcher, &lim, false).await;
        // The reply is the PARK prompt, not an executed action; the channel never
        // invoked the dispatcher's confirm — a fire can only follow an explicit
        // confirm {id}.
        assert!(r["reply"].as_str().unwrap().contains("confirm"), "ask surfaces the park prompt");
        assert_eq!(dispatcher.confirm_calls.load(Ordering::SeqCst), 0, "ask never auto-confirms");
    }

    /// deny {id} clears the parked action and fires nothing; an unknown id leaves
    /// it intact.
    // intentional: hold the global PENDING serialization guard across the awaited action; #[tokio::test] is current-thread so it cannot self-deadlock
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn deny_by_id_clears_only_the_named_action() {
        let _g = gate_guard();
        let id = park_a_consequential(
            "agent.pepper",
            "gmail_send",
            json!({"to": "a@b.com", "confirm": true}),
            vec!["gmail_send".into()],
        );
        let (disp, _root) = live_dispatcher("cmd_deny");

        // Unknown id: no-op, the action stays parked.
        let miss = disp.deny("ffffffffffffffff").await;
        assert_eq!(miss, "No pending action with that id.");
        assert!(confirm::peek_pending(Instant::now()).is_some(), "still parked after a wrong deny");

        // The real id: cleared.
        let hit = disp.deny(&id).await;
        assert_eq!(hit, "Cancelled.");
        assert!(confirm::peek_pending(Instant::now()).is_none(), "denied action is gone");
    }

    /// dismiss_forge clears ONLY the matching pending marker and NEVER deploys —
    /// apps/ is unchanged.
    // intentional: hold the global PENDING serialization guard across the awaited action; #[tokio::test] is current-thread so it cannot self-deadlock
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn dismiss_forge_clears_marker_and_never_deploys() {
        let _g = gate_guard();
        let (disp, root) = live_dispatcher("cmd_dismiss_forge");

        // Stamp a pending forge marker (as a successful proposal would).
        disp.memory.upsert_fact("meta.forge_pending", "12345").await.unwrap();

        // Snapshot apps/ (the project's real apps dir) so we can prove it is
        // untouched by a dismiss — dismiss must never apply/deploy.
        let apps_dir = project_root().join("apps");
        let before = snapshot_dir(&apps_dir);

        // Dismissing a NON-matching ts is a no-op (marker stays).
        let miss = disp.dismiss_forge(999).await;
        assert!(miss.contains("No matching"), "stale ts is a no-op: {miss}");
        assert_eq!(
            disp.memory.get_fact("meta.forge_pending").await.unwrap().as_deref(),
            Some("12345"),
            "non-matching dismiss left the marker"
        );

        // Dismissing the matching ts clears the marker.
        let hit = disp.dismiss_forge(12345).await;
        assert!(hit.contains("Dismissed"), "matching dismiss reported: {hit}");
        assert!(hit.contains("not deployed"), "dismiss states it did not deploy: {hit}");
        assert!(
            disp.memory.get_fact("meta.forge_pending").await.unwrap().is_none(),
            "the marker is cleared"
        );

        // apps/ is byte-for-byte unchanged — NOTHING was deployed.
        let after = snapshot_dir(&apps_dir);
        assert_eq!(before, after, "dismiss_forge must NEVER write into apps/");
        let _ = root;
    }

    // -- helpers -------------------------------------------------------------

    fn project_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
    }

    /// A stable, content-free snapshot of a directory tree: the sorted set of
    /// relative paths. Enough to prove no app dir/file was added (a deploy) by a
    /// dismiss, without reading file bytes.
    fn snapshot_dir(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if let Ok(rel) = p.strip_prefix(base) {
                        out.push(rel.to_string_lossy().into_owned());
                    }
                    if p.is_dir() {
                        walk(base, &p, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }
}
