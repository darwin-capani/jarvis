// The static `tool_defs()` JSON literal in `anthropic.rs` is a single large
// `json!([...])` array; each tool def expands the `json!` macro recursively, and
// with the ads action tools added this turn the surface crossed the default 128
// macro-recursion limit. Raise it (a compile-time-only knob with no runtime cost)
// so the literal keeps its established one-array-per-def style.
#![recursion_limit = "256"]

mod actions;
mod agents;
mod anthropic;
mod anticipate;
mod apps;
mod audio;
mod audit;
mod brief;
// DATA -> CHART (#41): a daemon ChartSpec {kind, series:[{label, points:[(x,y)]}],
// x_axis, y_axis, title} emitted as a `chart.data` telemetry envelope from a data
// path. The HUD's Chart component renders the EXACT emitted points (no
// interpolation, no invented/extrapolated point, honest axes), with an honest-empty
// state. NEUTRAL presentation (fire-and-forget telemetry, dropped with no HUD); the
// "chart this" op ships ON ([chart].enabled; a neutral presentation act, safe to enable). Hermetically tested in chart.rs.
mod chart;
mod code;
mod command;
mod config;
mod confirm;
mod crypto;
// MULTI-SPEAKER DIARIZATION (#31): the PURE mapper that CONSUMES the speaker labels the
// ElevenLabs Scribe STT backend reports into a per-speaker diarized transcript, and the
// HONEST single-stream "speaker: unknown" labeling on the on-device whisper path (which
// has no diarization model — NEVER fabricated speakers). ON by default ([voice].diarize)
// but INERT ON-DEVICE; EL-Scribe-gated. Wired on the transcript path (main.rs run_pipeline); the pure
// label-mapper + the on-device honest fallback are hermetically tested in diarize.rs.
mod diarize;
mod docsearch;
// AUTO-DRAFT (#25): compose a REVIEWABLE pending draft (email reply / message /
// doc) the user reads and sends THEMSELVES via the existing gated send. The draft
// module has NO send path — a draft is always a suggestion, never auto-sent. ON by
// default ([drafts].enabled; a draft has no send path). Hermetically tested in drafts.rs.
mod drafts;
// DURABLE MISSIONS (#26): persist FURY mission state to SQLite (resume/list/cancel).
// A persisted mission loads PAUSED (no auto-run on restart) and re-runs each
// consequential step through the SAME gate on resume (the persistence carries no
// pre-approval); inherits FURY's <=6 / 1-deep bounds. ON by default
// ([missions].durable; persistence only — a persisted mission loads PAUSED).
// Hermetically tested in durable_missions.rs.
mod durable_missions;
mod episodic;
mod eval;
mod focus;
mod forecast;
mod forge;
mod genproxy;
mod heal;
mod inference;
mod integrations;
// CONTINUOUS LIVE INTERPRETATION (#30): the PURE per-segment interpret pipeline
// (interpret_segment: transcript -> on-device-LLM translate -> rendered translation +
// optional speak request) using an injectable translator; offline/unavailable degrades
// HONESTLY (never a fabricated translation). ON by default ([interpret].live) but INERT WITHOUT MIC/TCC. The
// continuous live-interpret mode that feeds each VAD segment through it is DEVICE-GATED:
// wired behind the flag at the audio.rs segment site; only the pure core is proven
// headlessly (hermetic tests in interpret.rs).
mod interpret;
mod knowledge_graph;
// LIFE-LOG DIGEST (#20): a periodic (daily/weekly) browsable summary built ONLY
// from the agent-scoped, redacted EPISODIC store — bounded, never-fabricating
// (empty/sparse window -> honest empty), forgettable. Read-only over real
// episodes; needs no model/network. NOW WIRED LIVE: the router classifies a
// life-log utterance ("what did I do this week") and calls lifelog::dispatch,
// which builds the agent-scoped digest over the real episodes and renders it —
// so the intent reaches the digest end-to-end. Hermetically tested in lifelog.rs.
mod lifelog;
mod lockdown;
// MACRO RECORD/REPLAY (#27): record a NAMED sequence of commands (utterances +
// intent names ONLY — never secrets) and replay it. Replay re-runs each command
// through the NORMAL router path + the gate FRESH (a consequential step re-hits the
// confirmation gate + master switch, no pre-approval, no batching). ON by default
// ([macros].enabled; replay re-gates each consequential step). Hermetically tested in macros.rs.
mod macros;
mod mcp;
mod memory;
mod mission;
// MODEL TIER + RUNTIME OVERRIDE: the swap-only "which brain answers" layer
// (Local / Fast / Heavy) + the process-global voice override + the conservative
// model-control intent classifier. Refines today's binary cloud-vs-local
// contract; changes NO safety gate. Hermetically tested in model_tier.rs.
mod model_tier;
// RESEARCH NOTEBOOKS (#19): a persistent, redacted, agent-scoped, bounded store
// of SAGE research runs — a run is saved as a CITED notebook entry {topic, text,
// the real fetched citations, ts}; the user can REVISIT a notebook and APPEND a
// follow-up run (source memory accrues). Cite-discipline carries through from
// research.rs: a notebook holds NO citation that was not in its run. Forgettable.
// NOW WIRED LIVE: the live SAGE path (anthropic::run_sage_research) records each
// real completed run into the process-global last-run slot, and the router
// classifies a notebook utterance ("save this research" / "show my research on X"
// / "what have I researched" / "forget my research on X") and calls
// notebook::dispatch, which save/revisit/list/forgets against the agent-scoped
// store — so the intent reaches the notebook end-to-end. Hermetically tested in
// notebook.rs.
mod notebook;
// The Trace Store + Optimizer: the local, PII-redacted record the
// optimization-from-usage loop learns from, plus the propose-only optimizer that
// reads it. Now WIRED LIVE: the turn loop calls optimize::record_trace at the
// bookkeeping site (gated by [optimize].enabled, ships ON; live recording is
// runtime-gated + PII-redacted) and a periodic
// optimize_task calls optimize::run_optimizer (propose-only, never mutates the
// live config). Exercised in full by optimize.rs's own hermetic tests.
mod optimize;
mod playback;
mod plugin_sdk;
mod policy;
mod posture;
mod power;
mod proactive;
// Expressiveness layer (#33 adaptive prosody + #34 whisper/discreet mode). PURE +
// ON by default ([voice].adaptive_prosody / [voice].whisper / whisper_auto all ship
// true); EXPRESSIVENESS-ONLY (delivery, never a gate). Rich prosody is EL-v3-GATED (Kokoro gets a coarse/neutral mapping,
// stated honestly, never faked); whisper changes DELIVERY only — it NEVER silences a
// safety confirmation the gate requires. No audio/EL/mic touched here: this is the
// pure context->profile classifier + shape_speak_request + whisper state machine,
// tested in isolation. The shaped params thread to inference::speak (server seam).
mod prosody;
// The proactive-intelligence core (habit detector #13 + predictive suggester
// #14). NOW WIRED LIVE: the anticipation tick calls proactive_intel::surface_pass
// every tick, GATED by [proactive].suggest (ships ON) — when on it mines the
// agent-scoped redacted episodes and emits each over-threshold pattern as a
// `proactive.suggestion` HUD card (a SUGGESTION only; it never acts/speaks/
// creates). The accept-mapper (accept_request / AcceptRequest::to_standing_create_input)
// is the propose->gated-create bridge the accept path uses to route a habit offer
// through the EXISTING gated standing_create; the dismiss-ledger types persist the
// suppressed-id set. The whole surface is exercised by the module's own hermetic
// tests. allow(dead_code) covers the accept-mapper + ledger-mutation helpers
// whose runtime callers (the HUD-driven accept/dismiss write-back) are not yet
// wired in the binary — the detect/surface path itself IS live.
#[cfg_attr(not(test), allow(dead_code))]
mod proactive_intel;
mod recall;
mod reflect;
// REPORT GENERATION (#40): a PURE build_report(title, sources:[SourcedClaim], cfg)
// -> Report {sections:[{heading, body, citations}], all_citations, empty} +
// render_markdown. Assembles already-cited notebook/research material into one
// structured, BOUNDED markdown report, REUSING research.rs's cite discipline: every
// citation is a REAL source ref carried by an input claim (an uncited claim is
// DROPPED, never given a fabricated source), and with no citable source the report
// is HONEST-EMPTY. NOW WIRED LIVE: the router classifies a "generate a report on X"
// utterance (read-only, orchestrator-scoped), pulls the agent-scoped notebook
// entries on X (the already-cited runs), and builds + renders the report — so the
// intent reaches the report end-to-end. ON by default ([report].enabled; read-only,
// folds already-cited material, honest-empty when none). Hermetic tests in report.rs.
mod report;
mod research;
mod router;
mod screen_context;
mod selector;
mod selfcheck;
mod shell;
mod signals;
mod skills;
mod speech;
mod standing;
mod telemetry;
// GATED UI AUTOMATION (#44, the CAPSTONE): the single most dangerous capability —
// actually ACTUATING the macOS UI (click/type/key). A PURE single-action planner
// (ONE plan = ONE actuation, can't batch) + the device-gated CGEvent/AX seam
// (built, never run in a test). Ships ON but NEVER auto-runs: per-action gated
// (master + confirm + voice-id + lockdown), device-gated (Accessibility TCC; inert
// without it). Hermetically tested.
mod ui_automation;
mod unified_search;
mod user_model;
mod voiceid;
// VOICE CLONING (build 2/2): the CONSENT-GATED, authorization-bound capability that
// uploads an owner-authorized sample to ElevenLabs and stores the returned voice id.
// Pure intent/consent/confinement/store logic; the upload itself is the inference
// clone seam. Hermetically tested in voiceclone.rs.
mod voiceclone;
// VOICE TIER: the ON-by-default (INERT WITHOUT A KEY) ElevenLabs cloud-TTS layer on top of on-device
// Kokoro. Pure tier-decision brain (resolve_voice_backend / resolve_stt_backend) +
// the Backend contracts the speak/transcribe paths thread to the inference server.
// Changes NO safety gate; Kokoro/whisper stay the default + fallback. Hermetically
// tested in voice_tier.rs.
mod voice_tier;
// CUSTOM WAKE-WORD (#32): the PURE, conservative wake-phrase matcher (wake_match) +
// the activation gate (wake_gate) that folds in the [wake].enabled switch. ON by
// default; the default phrase ("jarvis") preserves today's activation behavior. The
// matcher is case/punct/whitespace-insensitive with a small edit-distance tolerance,
// never matches an empty/blank phrase, and never triggers on a substring of a larger
// unrelated word. Wired into the activation path (router.rs); the always-listening loop
// that produces the transcript is DEVICE-GATED. Hermetically tested in wake.rs.
mod wake;
// #35 WEBHOOK TRIGGERS — an inbound, HMAC-authenticated, loopback-default surface
// that maps a signed event to a JARVIS intent and PARKS a consequential mapping
// (never auto-executes). Ships ON but INERT WITHOUT MAPPINGS + A KEYCHAIN HMAC
// SECRET; the pure handle_webhook is tested, the live loopback bind is RUNTIME-gated
// (the mic-loop / vision-capture precedent).
mod webhooks;
// #36 PLUGIN SDK — the capability-module contract validator + register-on-launch
// handshake + capability-token scoping. The validator is pure + tested; the live
// handshake ships ON behind [plugin_sdk].enabled (the validator rejects over-privileged manifests).
// (declared above as `mod plugin_sdk;` near the alphabetical p-block)
mod world_model;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::Config;
use crate::inference::InferenceClient;
use crate::memory::Memory;

/// Per-stage pipeline latencies for one utterance. `queue_ms` is VAD finish
/// (the utterance WAV's mtime) -> event-loop pickup — visible queue wait
/// behind an in-flight turn (audit fix: the clock used to start at dequeue,
/// hiding it). `route_ms` covers routing AND reply generation: for the
/// streamed converse path it runs to the server's done event; for cloud it
/// is time inside router::route. `first_audio_ms` is utterance pickup ->
/// first audio — the instant-opener append when one fired, else the first
/// content clip; the latency the user actually perceives — and `speak_ms`
/// is first audio -> playback drained (excl. the 400ms tail).
#[derive(Debug, Default, Clone, Copy)]
pub struct PipelineTiming {
    pub queue_ms: u64,
    pub stt_ms: u64,
    pub classify_ms: u64,
    pub route_ms: u64,
    pub first_audio_ms: Option<u64>,
    pub speak_ms: u64,
}

/// Audit fix: Transcript/Classified are gone. Pipeline stages of different
/// utterances used to interleave on this single FIFO queue — utterance B,
/// captured during A's multi-second STT, was dequeued BEFORE the queued
/// Transcript-A event, so B's ReplySession (and possibly its opener) began
/// while session A was still alive inside a queued event: two live sessions
/// on the shared playback sink. Each utterance's pipeline now runs to
/// completion inline (run_pipeline) before the next Utterance dequeues,
/// removing ReplySession overlap entirely.
#[derive(Debug)]
pub enum Event {
    /// A finished utterance WAV from the VAD, plus the on-device speaker
    /// embedding computed from the SAME segment samples (round G, voice-id).
    /// The embedding is a fixed-dim, L2-normalized feature VECTOR — never raw
    /// audio; `None` when the segment had no usable audio to embed (fail-closed
    /// for the consequential path). It is computed in `audio.rs` from the raw f32
    /// segment at the captured sample rate (NOT from the lossy i16 WAV), so the
    /// turn handler verifies against the owner profile without re-reading audio.
    Utterance {
        wav: PathBuf,
        embedding: Option<Vec<f32>>,
    },
}

/// An utterance that waited longer than this between VAD finish (WAV mtime)
/// and pickup is discarded as stale: it sat out a long in-flight turn (a
/// cloud round trip can run ~75s), and answering it minutes later — with a
/// fresh opener — reads as the assistant talking to itself. (A deep
/// multi-step cloud tool loop can now run up to TOOL_LOOP_BUDGET = 400s in
/// the worst case; this 30s pickup window is deliberately tighter so a turn
/// that out-waited even an ordinary cloud round trip is dropped.) Normal
/// back-to-back requests wait only as long as one local turn, well under
/// this bound.
const STALE_UTTERANCE_WAIT: Duration = Duration::from_secs(30);

/// How long the utterance WAV sat queued before pickup, from its mtime (the
/// VAD writes the file the instant the segment ends). None when the
/// filesystem cannot say — treated as "no wait" everywhere.
fn utterance_queue_wait(wav: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(wav).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Staleness policy, pure for tests: only a KNOWN wait past the bound
/// discards (an unreadable mtime must never eat an utterance).
fn is_stale_wait(wait: Option<Duration>) -> bool {
    wait.is_some_and(|w| w > STALE_UTTERANCE_WAIT)
}

/// Minimum word count for a transcript to be ACTED on. A one-word fragment is
/// almost always an echo shard or a misfire, never a deliberate command worth
/// actuating (RC-5). Kept low so real short commands ("system status", "open
/// safari") still pass.
const MIN_TRANSCRIPT_WORDS: usize = 2;

/// Lowercased alphanumeric word tokens of a string — the comparison unit for
/// the self-echo check, so punctuation and the spoken " dot " expansion of a
/// URL don't defeat a substring match.
fn echo_tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Defense-in-depth self-echo / plausibility reject (RC-5): should this
/// transcript be DROPPED before classify+route, even though STT produced text?
///
/// A transcript is rejected when it is implausibly short (a single token — an
/// echo shard like "apple"), OR when its words are wholly contained in JARVIS's
/// just-spoken reply (he heard himself). This breaks the echo-feedback loop even
/// if some gate window leaks: a fragment of JARVIS's own reply can never
/// actuate. Pure, so the policy is unit-testable. `last_reply` is JARVIS's
/// previous spoken response (None on the first turn).
///
/// Conservative by construction: a genuine multi-word user command whose words
/// are NOT all present in the last reply passes untouched, so normal back-to-
/// back conversation is unaffected.
fn is_self_echo(transcript: &str, last_reply: Option<&str>) -> bool {
    let words = echo_tokens(transcript);
    // Too short to be a deliberate actionable command.
    if words.len() < MIN_TRANSCRIPT_WORDS {
        return true;
    }
    // Substring-of-the-last-reply (token containment): every word of the
    // transcript appears in JARVIS's previous reply -> he re-heard himself.
    match last_reply {
        Some(reply) if !reply.trim().is_empty() => {
            let reply_words: std::collections::HashSet<String> =
                echo_tokens(reply).into_iter().collect();
            !reply_words.is_empty() && words.iter().all(|w| reply_words.contains(w))
        }
        _ => false,
    }
}

/// Human formatting for latency log lines: 850ms, 1.2s.
fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// JARVIS_ROOT wins; otherwise walk up from the executable (which normally
/// lives at <root>/daemon/target/<profile>/jarvisd) looking for a directory
/// that contains config/jarvis.toml or state/, falling back to the
/// executable's grandparent, then the current directory.
fn resolve_root() -> PathBuf {
    if let Ok(root) = std::env::var("JARVIS_ROOT") {
        return PathBuf::from(root);
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1) {
            if ancestor.join("config").join("jarvis.toml").exists()
                || ancestor.join("state").is_dir()
            {
                return ancestor.to_path_buf();
            }
        }
        if let Some(grandparent) = exe.parent().and_then(Path::parent) {
            return grandparent.to_path_buf();
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// AT-REST ENCRYPTION key resolution + migration ([security].encrypt_memory, #11)
// ---------------------------------------------------------------------------

/// The four sensitive SQLite stores that whole-file SQLCipher encryption covers,
/// as paths under `state/`. The migration on enable re-keys each existing one.
fn sensitive_db_paths(state_dir: &Path) -> [PathBuf; 4] {
    [
        state_dir.join("jarvis.db"),            // memory.rs main Db
        state_dir.join("docsearch.db"),         // docsearch.rs
        state_dir.join("audit.db"),             // audit.rs
        state_dir.join("optimize").join("optimize.db"), // optimize.rs trace store
    ]
}

/// Resolve the at-rest master key for this run and install it as the process-
/// global so on-demand opens (docsearch from the tool loop / router) reach it.
///
///   * `encrypt = false` (the shipped default): returns `None`, installs `None` —
///     every store opens PLAINTEXT (byte-for-byte today's behavior). No key is
///     generated or read; the Keychain is never touched.
///   * `encrypt = true`: read the master key from the Keychain. If absent, this is
///     the FIRST enable — generate a fresh 256-bit key, write it to the Keychain,
///     and MIGRATE each existing plaintext store to encrypted (read-plaintext ->
///     write-encrypted via `sqlcipher_export`) plus the voiceid profile. Absent
///     stores are simply created encrypted on first open (honest fresh-start).
///     Returns the key. On a key-store/Keychain failure, returns `None` and the
///     caller falls back to plaintext rather than wedging startup (honest: the
///     warning says encryption could not be enabled this run).
async fn resolve_encryption_key(encrypt: bool, state_dir: &Path) -> Option<crypto::SecretKey> {
    if !encrypt {
        crypto::install_master_key(None);
        return None;
    }
    // Encryption ON. Already keyed?
    if let Some(key) = crypto::read_master_key().await {
        crypto::install_master_key(Some(key.clone()));
        return Some(key);
    }
    // First enable: generate + store the key, then migrate the existing stores.
    let key = match crypto::generate_and_store_master_key() {
        Ok(k) => k,
        Err(e) => {
            warn!(error = %e, "security: could not generate/store the master key; running PLAINTEXT this session");
            crypto::install_master_key(None);
            return None;
        }
    };
    for path in sensitive_db_paths(state_dir) {
        let tmp = path.with_extension("enc-migrate");
        if let Err(e) = crypto::migrate_plaintext_to_encrypted(&path, &tmp, &key) {
            // A migration failure is logged but not fatal: the store will open
            // encrypted (or be created encrypted) below; the operator sees the warn.
            warn!(path = %path.display(), error = %e, "security: store migration to encrypted failed");
        }
    }
    // Voiceid owner.json -> encrypted vault (its own wrapper; sqlcipher_export does
    // not cover a JSON file).
    if let Err(e) = voiceid::migrate_profile_to_vault(&resolve_root(), &key) {
        warn!(error = %e, "security: voiceid profile migration to encrypted vault failed");
    }
    info!("security: at-rest encryption ENABLED; sensitive stores migrated to SQLCipher");
    crypto::install_master_key(Some(key.clone()));
    Some(key)
}

/// Open the main memory Db honoring the resolved encryption state: encrypted when
/// a key is present, else plaintext (today's behavior).
fn open_memory(path: &Path, key: Option<&crypto::SecretKey>) -> Result<Memory> {
    match key {
        Some(k) => Memory::open_encrypted(path, k),
        None => Memory::open(path),
    }
}

/// Open the audit log honoring the resolved encryption state.
fn open_audit(path: &Path, key: Option<&crypto::SecretKey>) -> Result<audit::AuditLog> {
    match key {
        Some(k) => audit::AuditLog::open_encrypted(path, k),
        None => audit::AuditLog::open(path),
    }
}

/// Open the optimizer trace store honoring the resolved encryption state.
fn open_trace_store(path: &Path, key: Option<&crypto::SecretKey>) -> Result<optimize::TraceStore> {
    match key {
        Some(k) => optimize::TraceStore::open_encrypted(path, k),
        None => optimize::TraceStore::open(path),
    }
}

/// daemon.log rotation bound (audit fix: the log was opened append-only and
/// grew without bound on the always-on appliance, at DEBUG for the daemon's
/// own crate). At the bound the live file is renamed to daemon.log.1
/// (replacing the previous rotation — total footprint stays bounded) and a
/// fresh daemon.log is opened. The heal watchdog tails only the live file;
/// a rotation just shortens its tail for one tick.
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Append-only writer that rotates by size. Implements io::Write, so the
/// existing Mutex<W> MakeWriter wrapping keeps working.
struct RotatingLogWriter {
    path: PathBuf,
    file: std::fs::File,
    len: u64,
    max: u64,
}

impl RotatingLogWriter {
    fn open(path: PathBuf, max: u64) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self { path, file, len, max })
    }

    /// Rename-and-reopen once the bound is reached. Best-effort by design:
    /// when the rename or reopen fails we keep appending to the current
    /// handle — losing rotation is recoverable, losing log lines is not
    /// (and this runs inside the logger, so it cannot log its own failure).
    fn rotate_if_due(&mut self) {
        if self.len < self.max {
            return;
        }
        let rotated = self.path.with_extension("log.1");
        if std::fs::rename(&self.path, &rotated).is_err() {
            return;
        }
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&self.path) {
            self.file = file;
            self.len = 0;
        }
    }
}

impl std::io::Write for RotatingLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.rotate_if_due();
        let n = self.file.write(buf)?;
        self.len += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

fn init_tracing(root: &Path) -> Result<()> {
    let log_path = root.join("state").join("logs").join("daemon.log");
    let file = RotatingLogWriter::open(log_path.clone(), LOG_ROTATE_BYTES)
        .with_context(|| format!("opening {}", log_path.display()))?;
    // Without a filter the registry defaults to TRACE and instrumented deps
    // (hyper-util connection pools etc.) spam daemon.log; RUST_LOG overrides.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,jarvis_core=debug"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file)),
        )
        .init();
    Ok(())
}

/// Clear out utterance WAVs left behind by a previous run (crash, kill -9):
/// they are transient pipeline inputs, not durable artifacts.
fn sweep_stale_utterances(root: &Path) {
    let tmp = root.join("state").join("tmp");
    let Ok(entries) = std::fs::read_dir(&tmp) else { return };
    let mut swept = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // utterance-*.wav are pipeline inputs; tts-*.wav are converse reply
        // sentence clips the daemon owns deleting — a barge that drops the
        // remaining sentences can leave some behind, so reclaim them on restart.
        let stale = (name.starts_with("utterance-") || name.starts_with("tts-"))
            && name.ends_with(".wav");
        if stale {
            if std::fs::remove_file(entry.path()).is_ok() {
                swept += 1;
            }
        }
    }
    if swept > 0 {
        info!(swept, "removed stale utterance WAVs from state/tmp");
    }
}

/// Remove a consumed utterance WAV; missing files are fine (already cleaned).
fn discard_wav(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %path.display(), error = %e, "failed to remove utterance wav");
        }
    }
}

/// Non-blocking learning loop: after each spoken reply, ask the inference
/// server for durable facts from the exchange and upsert them. Runs on its
/// own InferenceClient (the main loop owns the other one mutably) and only
/// ever logs on failure — learning must never delay or break a response.
fn spawn_learning_task(sock: PathBuf, memory: Arc<Memory>, utterance: String, response: String) {
    tokio::spawn(async move {
        let mut infer = InferenceClient::new(sock);
        match infer.extract_facts(&utterance, Some(&response)).await {
            Ok(facts) => {
                for (key, value) in facts {
                    // upsert_user_fact: extract_facts output is model-driven,
                    // so reserved "meta." bookkeeping keys are rejected here
                    // exactly like the cloud remember_fact tool (audit fix).
                    match memory.upsert_user_fact(&key, &value).await {
                        Ok(()) => {
                            info!(key, value, "memory: learned fact");
                            telemetry::emit(
                                "system",
                                "memory.learned",
                                json!({"key": key, "value": value}),
                            );
                        }
                        Err(e) => {
                            warn!(error = %e, key, "failed to store learned fact");
                        }
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "fact extraction failed");
                telemetry::emit(
                    "system",
                    "inference.unavailable",
                    json!({"op": "extract_facts", "error": e.to_string()}),
                );
            }
        }
    });
}

/// Retention cadence and limits for the jarvis.db pruning pass (audit fix:
/// events/transcripts otherwise grow without bound on the always-on
/// appliance). 30 days of events and 2000 transcripts comfortably exceed
/// everything any reader consumes (reflection reads 40 exchanges, prompts
/// read 6); the interval matches the reflection task's check cadence.
const RETENTION_STARTUP_DELAY: Duration = Duration::from_secs(120);
const RETENTION_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const RETENTION_EVENTS_MAX_AGE_DAYS: i64 = 30;
const RETENTION_TRANSCRIPTS_KEEP: usize = 2000;

/// Periodic jarvis.db retention. Warn-and-continue like every housekeeping
/// task: a failed pass must never wedge the daemon.
///
/// `notebook_entries_keep` is the [notebooks].retention evict-oldest cap (the
/// bounded RESEARCH-NOTEBOOK contract); `None` when [notebooks].enabled is false
/// (no store to bound). It runs on the SAME cadence as the episodic/event pass.
async fn retention_task(
    memory: Arc<Memory>,
    episodes_keep: usize,
    notebook_entries_keep: Option<usize>,
) {
    tokio::time::sleep(RETENTION_STARTUP_DELAY).await;
    loop {
        match memory
            .retention_pass(
                RETENTION_EVENTS_MAX_AGE_DAYS,
                RETENTION_TRANSCRIPTS_KEEP,
                episodes_keep,
            )
            .await
        {
            Ok((0, 0, 0)) => {}
            Ok((events_deleted, transcripts_deleted, episodes_deleted)) => {
                info!(
                    events_deleted,
                    transcripts_deleted, episodes_deleted, "memory retention pass pruned rows"
                );
                telemetry::emit(
                    "system",
                    "memory.retention",
                    json!({
                        "events_deleted": events_deleted,
                        "transcripts_deleted": transcripts_deleted,
                        "episodes_deleted": episodes_deleted,
                    }),
                );
            }
            Err(e) => {
                warn!(error = %e, "memory retention pass failed; will retry next interval");
            }
        }
        // RESEARCH-NOTEBOOK evict-oldest cap (#19 bounded contract at runtime).
        // Same warn-and-continue posture; skipped entirely when the store is off.
        if let Some(keep) = notebook_entries_keep {
            match memory.notebook_retention_pass(keep).await {
                Ok(0) => {}
                Ok(entries_deleted) => {
                    info!(entries_deleted, "research-notebook retention pass evicted oldest entries");
                    telemetry::emit(
                        "system",
                        "notebook.retention",
                        json!({ "entries_deleted": entries_deleted }),
                    );
                }
                Err(e) => {
                    warn!(error = %e, "research-notebook retention pass failed; will retry next interval");
                }
            }
        }
        tokio::time::sleep(RETENTION_INTERVAL).await;
    }
}

/// Periodic optimizer cadence. The live loop is runtime-only (it only fires when
/// [optimize].enabled is true, which ships ON) and is NOT exercised by tests —
/// the pure [`optimize::run_optimizer`]/[`optimize::optimize`] are. A generous
/// startup delay keeps it out of the first exchanges; a slow tick is plenty
/// because it only ever reads an accruing corpus and PROPOSES — it never blocks
/// a turn and never mutates the live routing config.
const OPTIMIZE_STARTUP_DELAY: Duration = Duration::from_secs(180);
const OPTIMIZE_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// How many recent traces the periodic pass hands the optimizer. Bounded so the
/// pass stays cheap; the optimizer splits this into its own train/held-out.
const OPTIMIZE_RECENT_WINDOW: usize = optimize::MAX_TRACES;

/// The periodic PROPOSE-ONLY optimizer pass (runtime-only; never run in tests).
///
/// Mirrors `retention_task`'s scheduling exactly: startup delay, then a slow
/// loop. Each tick reads the recent trace window and calls
/// [`optimize::run_optimizer`] — which is a complete NO-OP returning `Disabled`
/// when `[optimize].enabled` is false (an operator override; the shipped default is
/// ON), and otherwise
/// writes a REVIEWABLE proposal under `state/optimize/proposals/<ts>/` and STOPS.
/// It NEVER mutates the live routing config (the human applies a proposal via
/// scripts/apply_optimization.sh). Warn-and-continue throughout: a read failure
/// must never wedge the daemon.
async fn optimize_task(cfg: Arc<Config>, store: Arc<optimize::TraceStore>, optimize_root: PathBuf) {
    tokio::time::sleep(OPTIMIZE_STARTUP_DELAY).await;
    loop {
        // When the master switch is OFF (default), run_optimizer short-circuits
        // to Disabled without reading or writing anything — but skip the store
        // read entirely so the OFF path touches no I/O at all.
        if cfg.optimize.enabled {
            match store.recent(OPTIMIZE_RECENT_WINDOW).await {
                Ok(traces) => {
                    let ts = SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    // Propose-only: writes a reviewable artifact at most; never
                    // mutates the live config. Pure aside from that on-disk write.
                    optimize::run_optimizer(
                        cfg.optimize.enabled,
                        &cfg.optimize.mode,
                        &optimize_root,
                        &traces,
                        ts,
                    );
                }
                Err(e) => {
                    warn!(error = %e, "optimize: failed to read trace corpus; will retry next interval");
                }
            }
        }
        tokio::time::sleep(OPTIMIZE_INTERVAL).await;
    }
}

/// EVAL scorecard cadence. The live loop is runtime-only (it reads the live
/// rolling eval state + the trace store and emits aggregate telemetry); it is NOT
/// exercised by tests — the pure aggregation math in `eval.rs` is. A short startup
/// delay lets the hub + first turns settle; a slow tick is plenty because the
/// scorecard is a rolling view, not a per-turn readout (the HUD also gets a fresh
/// snapshot whenever it reconnects to the hub).
const EVAL_STARTUP_DELAY: Duration = Duration::from_secs(20);
const EVAL_INTERVAL: Duration = Duration::from_secs(30);
/// Recent trace window the accuracy/correction-rate is recomputed over each tick.
/// Bounded so the read stays cheap; the held-out accuracy reuses optimize's own
/// held-out carve over this window.
const EVAL_ACCURACY_WINDOW: usize = optimize::MAX_TRACES;

/// The periodic EVAL report pass (runtime-only; never run in tests). Each tick it
/// reads the live rolling latency + cost aggregates from `eval_state`, recomputes
/// routing accuracy + the live correction-rate from the trace store's recent
/// window (reusing optimize::score_config over the SAME held-out carve the
/// optimizer judges on), and emits the AGGREGATE-ONLY `eval.report` telemetry the
/// HUD Eval/Optimizer panel renders. NO PII — only percentiles, token sums,
/// rates, counts, and the honest optimizer posture (propose-only + OFF). A metric
/// with no data is emitted as "awaiting turns", never a fabricated value.
/// Warn-and-continue throughout: a read failure must never wedge the daemon, and
/// the eval framework NEVER changes routing or the optimizer's posture.
async fn eval_report_task(
    cfg: Arc<Config>,
    store: Arc<optimize::TraceStore>,
    eval_state: Arc<tokio::sync::Mutex<eval::EvalState>>,
) {
    tokio::time::sleep(EVAL_STARTUP_DELAY).await;
    loop {
        // Latency + cost from the live rolling window (MEASURED; empty ->
        // "awaiting turns" in the snapshot).
        let (latency, cost) = {
            let st = eval_state.lock().await;
            (st.latency(), st.cost(eval::CostRates::default()))
        };
        // Accuracy + correction-rate from the durable trace corpus. A read
        // failure degrades to an empty (awaiting) accuracy — never wedges.
        let accuracy = match store.recent(EVAL_ACCURACY_WINDOW).await {
            Ok(traces) => eval::accuracy_from_traces(&traces),
            Err(e) => {
                warn!(error = %e, "eval: failed to read traces for accuracy; reporting awaiting");
                eval::AccuracyAggregate::default()
            }
        };
        eval::emit_report(
            &latency,
            &cost,
            &accuracy,
            cfg.optimize.enabled,
            &cfg.optimize.mode,
        );
        tokio::time::sleep(EVAL_INTERVAL).await;
    }
}

/// EDITH anticipation cadence. The live loop is runtime-only — it is NOT
/// exercised by tests (the pure evaluator in `anticipate.rs` is); these
/// constants tune the live tick. A generous startup delay keeps housekeeping
/// out of the first exchanges; a 60s tick is far slower than any signal moves
/// and the in-evaluator debounce/rate-limit/cooldown guards do the real pacing.
const ANTICIPATE_STARTUP_DELAY: Duration = Duration::from_secs(150);
const ANTICIPATE_INTERVAL: Duration = Duration::from_secs(60);
/// The user is considered PRESENT for anticipation if they interacted within
/// this window (vs meta.last_interaction). EDITH never surfaces to an empty
/// room, so presence gates everything (anticipate::Signals.present).
const ANTICIPATE_PRESENCE_WINDOW_SECS: u64 = 10 * 60;

/// EDITH's live anticipation loop (runtime-only; never run in tests). Each tick
/// it builds a verified `Signals` snapshot from the cached telemetry reading and
/// presence, runs the PURE [`anticipate::evaluate`] with the configured policy +
/// the injected clock + the carried `FiredState`, and acts on the decision:
///   - `Nothing`  -> nothing.
///   - `Surface`  -> emit the `proactive.surface` HUD card (the behavior when
///                   `[proactive].speak = false`; the shipped default is `true`,
///                   which ALSO voices the brief via the echo-safe speech path).
///   - `Speak`    -> emit the card AND voice the brief — but ONLY through the
///                   EXISTING speech path (`speech::speak`), and ONLY when
///                   `is_speaking()` is false, so the SPEAKING refcount /
///                   MUTE_TAIL / barge logic all cover it and EDITH can never
///                   open a parallel audio path or talk over a live reply.
/// `FiredState` is carried across ticks here (the evaluator stays pure: it reads
/// the state and the loop advances it via `FiredState::record` only when it
/// actually acts). Warn-and-continue throughout: a tick must never wedge or
/// panic the daemon.
async fn anticipation_task(root: PathBuf, cfg: Arc<Config>, memory: Arc<Memory>, sock: PathBuf) {
    use anticipate::{FiredState, Policy};
    use chrono::Timelike;

    tokio::time::sleep(ANTICIPATE_STARTUP_DELAY).await;
    let policy = Policy::from_config(&cfg.proactive);
    // FOCUS PROFILE (#24): resolve the active profile from [focus].profile and
    // apply it to the base (today's) behavior ONCE — the result is a
    // PERMISSION-NEUTRAL lens that can only QUIET which non-consequential intel
    // surfaces. With the shipped "default" profile this is the IDENTITY (today's
    // behavior byte-for-byte). The tuned behavior carries NO gate/permission/
    // autonomy field, so applying it cannot loosen the gate, enable an action, or
    // raise autonomy — it only filters categories, tightens brief verbosity, and
    // can quiet the suggestion feed.
    let focus_profile = focus::FocusProfile::from_config_str(&cfg.focus.profile);
    let tuned = focus::apply_profile(&focus_profile, &focus::BaseBehavior::default());
    // Surface the active focus posture ONCE so the HUD can show which lens is
    // active and state the permission-neutral contract from the wire (the card
    // carries permission_neutral=true / raises_autonomy=false / loosens_gate=false
    // — not a HUD hardcode). PERMISSION-NEUTRAL by construction: TunedBehavior has
    // no gate/permission/autonomy field to leak.
    telemetry::emit(
        "agent.edith",
        "focus.active",
        tuned.telemetry(focus_profile.clone()),
    );
    let mut fired = FiredState::default();
    // Throttle caches for the external (network) signals, carried across ticks so
    // calendar/mail are refreshed on an interval rather than every 60s tick.
    let mut collector = signals::CollectorState::new();
    // Own InferenceClient for the (gated) spoken path — never contend with the
    // main event loop's client (mirrors reflect.rs's rationale).
    let mut infer = InferenceClient::new(sock);

    loop {
        tokio::time::sleep(ANTICIPATE_INTERVAL).await;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let local_hour = chrono::Local::now().hour() as u8;

        // Build the verified signal snapshot via the LIVE collector
        // (signals::collect_signals — runtime-only; the LOGIC it composes is
        // tested hermetically). Health (memory + REAL disk-free pct) comes from
        // the cached telemetry reading; presence from the recent-interaction
        // stamp; calendar + important-unread mail from the throttled Google reads
        // (absent/degraded silently when Google is not connected — never
        // fabricated); market stays None (no live source exists yet).
        let present = recently_present(&memory).await;
        let now_rfc3339 = chrono::Utc::now().to_rfc3339();
        let signals = signals::collect_signals(
            &mut collector,
            telemetry::latest_snapshot(),
            present,
            now,
            &now_rfc3339,
            signals::DEFAULT_REFRESH_SECS,
        )
        .await;

        // PROACTIVE-INTELLIGENCE SUGGESTIONS (#13 habit detector + #14 predictive
        // suggester). Runs every tick, INDEPENDENT of the EDITH brief decision
        // below (a suggestion is a separate surface). GATED by [proactive].suggest
        // (ships ON) inside surface_pass -> detect: with it off this is a true
        // no-op (no store read, no card). When on it mines the agent-scoped,
        // redacted episodes for a recurring pattern over threshold and emits each
        // as a `proactive.suggestion` card — a SUGGESTION only: it never acts,
        // never speaks, never creates a mission. Accepting a habit offer is a
        // separate HUD action that routes through the gated standing_create path.
        // Mined under the shared/orchestrator episodic scope (agent.jarvis), the
        // same namespace the conversational turns record under; agent-scoping is
        // enforced at the Db recall + in each suggestion's id.
        let pass =
            proactive_intel::surface_pass(&cfg.proactive, &memory, episodic::DEFAULT_NAMESPACE)
                .await;
        // FOCUS (#24): the active profile may QUIET the suggestion feed (Sleep /
        // DeepFocus / a quieting custom). This only ever SUPPRESSES cards — it
        // never surfaces one the `[proactive].suggest` gate already withheld, and
        // never acts. With the default profile `suggestions_quieted` is false, so
        // every suggestion surfaces exactly as today.
        if !tuned.suggestions_quieted {
            for sugg in &pass.suggestions {
                telemetry::emit(&sugg.agent, "proactive.suggestion", sugg.telemetry());
            }
        }

        // SMARTER BRIEF (#23): project the verified snapshot into cited brief
        // signals (over the SAME relevance thresholds the evaluator uses), then
        // build the ranked/capped/cited/honest-empty digest UNDER the focus-tuned
        // behavior. PURE + GROUNDED: every item cites a real origin present in the
        // snapshot; an unconnected source contributes nothing (honestly absent).
        // Emitted as a `proactive.digest` HUD card (a DISTINCT event from the
        // first-contact `proactive.brief` in proactive.rs — different concept,
        // different payload) alongside the EDITH single-card surface below; a
        // non-empty digest is the multi-item glance, an empty one honestly says
        // nothing surfaced (never padded — so we only emit when non-empty).
        let brief_signals = signals::brief_signals_from_snapshot(&signals, &policy);
        let smart_brief = brief::build_brief(&brief_signals, &tuned);
        if !smart_brief.empty {
            telemetry::emit("agent.edith", "proactive.digest", smart_brief.telemetry());
        }

        let decision = anticipate::evaluate(&signals, local_hour, now, &fired, &policy);
        let Some(brief) = decision.brief() else {
            continue; // Nothing / suppressed.
        };

        // FOCUS (#24): the active profile may SILENCE the EDITH single-card
        // surface when its trigger category is not in the tuned surfacing set
        // (e.g. Sleep/DeepFocus silence a routine market card but always pass a
        // critical disk/calendar one — Critical is the never-silenced floor).
        // This only ever DROPS a card; it never enables an action and never
        // speaks. With the default profile every category surfaces, so the card
        // always shows exactly as today. We still advance `fired` so a silenced
        // trigger's cooldown is honored (no churn re-evaluating it every tick).
        let surface_category = focus::category_for_trigger(brief.kind);
        if !tuned.surfaces(surface_category) {
            fired.record(brief.kind, now, policy.window_secs);
            continue; // focus-silenced — no card, no speech this tick.
        }

        // Always surface the HUD card (both Surface and Speak carry one).
        telemetry::emit("agent.edith", "proactive.surface", brief.telemetry());

        // LOCKDOWN OVERLAY (task #12): `should_speak_now()` ANDs in
        // `!lockdown::is_locked_down()`, so while the emergency stop is engaged
        // EDITH never voices a proactive brief — the HUD card already surfaced
        // above, but no unprompted speech fires. With lockdown OFF this is
        // byte-for-byte the prior `should_speak()`.
        if decision.should_speak_now() {
            // Spoken path: STRICTLY the existing speech pipeline, and never
            // while already speaking — so EDITH cannot talk over a live reply
            // and the SPEAKING/MUTE_TAIL/barge invariants hold. If speech is in
            // flight we skip THIS tick's voicing (the card already surfaced);
            // the next tick re-evaluates.
            if speech::is_speaking() {
                warn!("anticipation: speech in flight; surfacing card only this tick");
            } else {
                let mut reply = speech::ReplySession::begin(&root, &cfg).await;
                let started = Instant::now();
                let _ = speech::speak(&brief.text, &mut infer, &cfg, started, &mut reply).await;
            }
        }

        // Advance the carried state ONLY now that we acted on a real decision.
        fired.record(brief.kind, now, policy.window_secs);
    }
}

/// Whether the user interacted recently enough to be considered PRESENT for
/// anticipation (meta.last_interaction within the presence window). Read-only;
/// warn-and-continue — an unreadable stamp reads as "not present" (fail-safe:
/// EDITH stays silent rather than surface to a maybe-empty room).
async fn recently_present(memory: &Memory) -> bool {
    let last = match memory.get_fact("meta.last_interaction").await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "anticipation: cannot read presence stamp; treating as absent");
            return false;
        }
    };
    let Some(secs) = proactive::parse_unix_secs(last.as_deref()) else {
        return false;
    };
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    now.saturating_sub(secs) <= ANTICIPATE_PRESENCE_WINDOW_SECS
}

/// Standing-missions cadence. Runtime-only — NOT exercised by any test (the PURE
/// scheduler `standing::due_missions` + the run logic `standing::run_one` are, with
/// an injected clock + the mission engine's mocks). A generous startup delay keeps
/// it out of the first exchanges; a slow tick is plenty — the smallest schedule a
/// mission may run on is one hour, so a 5-minute tick never misses a due window.
const STANDING_STARTUP_DELAY: Duration = Duration::from_secs(180);
const STANDING_INTERVAL: Duration = Duration::from_secs(300);

/// The LIVE standing-missions loop (runtime-only; never run in tests). Each tick:
/// load the saved missions, run the PURE scheduler [`standing::due_missions`] with
/// the injected clock + the subsystem master switch ([standing].enabled, OFF by
/// default — with it off NOTHING is ever due), and for each DUE mission RUN it
/// through the SAME bounded FURY engine `fury_mission` uses (so every consequential
/// step still parks behind the confirmation gate + the master switch — a standing
/// mission can never auto-send/post/spend). Each run surfaces a `standing.run` HUD
/// card and is SPOKEN only when [proactive].speak is on AND the daemon is not
/// already speaking (strictly through the existing speech path, so the
/// SPEAKING/MUTE_TAIL/barge invariants hold and a mission can never open a parallel
/// audio path or talk over a live reply). After a run it stamps last_run so the
/// scheduler's next due-check is correct. Warn-and-continue throughout: a tick must
/// never wedge or panic the daemon.
async fn standing_task(root: PathBuf, cfg: Arc<Config>, memory: Arc<Memory>, sock: PathBuf) {
    use chrono::Timelike;

    tokio::time::sleep(STANDING_STARTUP_DELAY).await;
    // Own InferenceClient for the (gated) spoken path — never contend with the main
    // event loop's client (mirrors anticipation_task / reflect.rs).
    let mut infer = InferenceClient::new(sock);
    let registry = agents::AgentRegistry::canonical();

    loop {
        tokio::time::sleep(STANDING_INTERVAL).await;

        // Re-read the master switch from config each tick so flipping [standing]
        // on/off takes without a restart (cheap; mirrors run_forge_app's reload).
        // LOCKDOWN OVERLAY (task #12): while the emergency stop is engaged the
        // standing-missions subsystem is FORCED off — `due_missions` is handed
        // master_enabled=false, so NOTHING is ever due (no recurring autonomy
        // fires when locked). With lockdown OFF this is byte-for-byte the
        // configured `[standing].enabled`.
        let enabled = {
            let (live, _issues) =
                Config::load(&root.join("config").join("jarvis.toml"));
            live.standing.enabled && !lockdown::is_locked_down()
        };

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let local = chrono::Local::now();
        let local_hour = local.hour() as u8;
        let local_minute = local.minute() as u8;

        let missions = match standing::list(&memory).await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "standing: could not read the mission store this tick");
                continue;
            }
        };
        // No live signal source is wired for on-signal standing missions yet, so
        // the present-signals set is empty (on-signal missions simply don't fire
        // until a source exists — never fabricated). Daily/interval are unaffected.
        let signals_present: Vec<String> = Vec::new();
        let due = standing::due_missions(
            &missions,
            now,
            local_hour,
            local_minute,
            &signals_present,
            enabled,
        );
        if due.is_empty() {
            continue; // OFF, or nothing due this tick.
        }

        let cloud_reachable = anthropic::resolve_api_key().await.is_some();
        let model = cfg.cloud.heavy_model.clone();
        for mission in due {
            // Run through the SAME cloud-backed planner/dispatcher fury_mission
            // uses — each sub-task runs as its OWNING specialist under that
            // specialist's allowlist + the consequential gate. No escalation.
            let planner = mission::CloudPlanner {
                model: model.clone(),
                max_tokens: 1024,
            };
            let dispatcher = mission::CloudDispatcher {
                model: model.clone(),
                max_tokens: 1024,
                memory: &memory,
                orchestrator: registry.orchestrator().name.clone(),
            };
            let run = standing::run_one(mission, &registry, &planner, &dispatcher, cloud_reachable).await;

            // Always surface the HUD card.
            telemetry::emit("agent.fury", "standing.run", run.telemetry());

            // Spoken only when proactive speech is on AND not already speaking —
            // strictly the existing pipeline, so the SPEAKING/MUTE_TAIL/barge
            // invariants hold and a mission never talks over a live reply.
            if cfg.proactive.speak && !speech::is_speaking() {
                let mut reply = speech::ReplySession::begin(&root, &cfg).await;
                let started = Instant::now();
                let _ = speech::speak(&run.report, &mut infer, &cfg, started, &mut reply).await;
            }

            // Stamp last_run so the scheduler's next due-check is correct.
            if let Err(e) = standing::mark_ran(&memory, mission, now).await {
                warn!(error = %e, id = %mission.id, "standing: could not stamp last_run after a run");
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Non-production entrypoint: `jarvisd --heal-drill` runs the FULL real
    // self-heal v2 pipeline (diagnose -> Opus draft -> stage -> validate ->
    // review -> propose) against a PLANTED FAULT in a throwaway temp crate,
    // battle-testing the loop end to end through the real cloud drafter. It
    // requires the API key, writes a proposal artifact under a temp sandbox,
    // and NEVER touches the live daemon/ sources. This is the one sanctioned
    // cloud-spending verification path; it does not start the daemon.
    // Operator entrypoint: `jarvisd --selftest` (alias `--health`) validates the
    // installed environment WITHOUT starting the full daemon (no audio, no MCP
    // connect, no model, no mic). It resolves the root EXACTLY like a normal
    // start, runs the honest PASS/SKIP/FAIL board (root/config/venv/binary/state
    // dirs/0700 ipc perms/inference-reachability via connect-probe/telemetry-port
    // bindability/cloud-key), prints it, and EXITS NON-ZERO on any hard FAIL.
    // Mirrors `inference/server.py --selftest`'s honesty: a check that could not
    // actually run is SKIP, never PASS — it never claims healthy when a check was
    // skipped or a dep is missing. Spends no cloud/model call beyond resolving
    // whether a key EXISTS (bool only; the key never leaves the resolver).
    if std::env::args().any(|a| a == "--selftest" || a == "--health") {
        let root = selfcheck::resolve_root_like_daemon();
        let (cfg, _issues) = Config::load(&root.join("config").join("jarvis.toml"));
        let cloud_key_present = anthropic::resolve_api_key().await.is_some();
        let checks = selfcheck::run_selftest(&root, cfg.telemetry.port, cloud_key_present).await;
        selfcheck::print_board("JARVIS daemon selftest", &checks);
        if selfcheck::any_failed(&checks) {
            std::process::exit(1);
        }
        return Ok(());
    }

    if std::env::args().any(|a| a == "--heal-drill") {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::io::stderr)
            .init();
        let (cfg, _issues) = Config::load(&resolve_root().join("config").join("jarvis.toml"));
        anthropic::resolve_api_key().await; // resolve once (logs nothing)
        let dir = heal::run_heal_drill(&cfg.cloud.heavy_model).await?;
        println!("heal drill PASSED — proposal written to {}", dir.display());
        return Ok(());
    }

    // Non-production entrypoint: `jarvisd --forge-drill` runs the FULL real
    // Self-Forge pipeline (draft -> stage -> validate -> propose) against a
    // FIXED benign goal in a throwaway temp root, battle-testing the loop end to
    // end through the real cloud author. It requires the API key, writes a
    // proposal artifact under a temp sandbox, and NEVER touches the real apps/
    // (nothing is deployed; deploy is the human scripts/apply_forge.sh step).
    if std::env::args().any(|a| a == "--forge-drill") {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::io::stderr)
            .init();
        let (cfg, _issues) = Config::load(&resolve_root().join("config").join("jarvis.toml"));
        anthropic::resolve_api_key().await; // resolve once (logs nothing)
        let dir = forge::run_forge_drill(&cfg.cloud.heavy_model).await?;
        println!("forge drill PASSED — proposal written to {}", dir.display());
        return Ok(());
    }

    // Deploy-time gate entrypoint: `jarvisd --validate-forge-manifest <manifest_path> <app_name>`
    // parses the manifest with the daemon's OWN toml crate + runs the SAME
    // forge permission-minimization + default-deny-SBPL gate the draft path
    // runs (forge::validate_manifest_file -> validate_manifest), exiting 0 on a
    // minimal manifest and NON-ZERO on a parse error or any over-broad grant.
    // scripts/apply_forge.sh calls this in place of a textual permission scan,
    // closing the TOML parser-differential (dotted keys / inline tables /
    // deny_unknown_fields) the text scan could not see. Side-effect-free: it
    // only reads the manifest and validates — it deploys NOTHING and touches
    // neither apps/ nor any config. No daemon starts; no tracing/Config/Keychain.
    if let Some(pos) = std::env::args().position(|a| a == "--validate-forge-manifest") {
        let manifest_path = std::env::args().nth(pos + 1).unwrap_or_default();
        let app_name = std::env::args().nth(pos + 2).unwrap_or_default();
        if manifest_path.trim().is_empty() || app_name.trim().is_empty() {
            eprintln!(
                "usage: jarvisd --validate-forge-manifest <manifest_path> <app_name>"
            );
            std::process::exit(2);
        }
        // Representative root for the SBPL-derivability check only; nothing is
        // deployed and the live tree is never read here.
        let root = resolve_root();
        match forge::validate_manifest_file(
            Path::new(&manifest_path),
            &app_name,
            &root,
        ) {
            Ok(()) => {
                println!("FORGE MANIFEST OK: minimal permissions, default-deny SBPL derivable");
                return Ok(());
            }
            Err(e) => {
                eprintln!("FORGE MANIFEST REJECTED: {e}");
                std::process::exit(1);
            }
        }
    }

    // Operator entrypoint: `jarvisd --forge-goal "<goal>"` runs the GATED,
    // PROPOSE-ONLY Self-Forge production path (forge::forge_app) against the live
    // project root + config + Memory. It respects [forge].enabled (does nothing
    // when OFF), requires the cloud key, and on success writes a proposal under
    // state/forge/proposals/<ts>/ + stamps meta.forge_pending. It NEVER deploys
    // into apps/ — the human runs scripts/apply_forge.sh <ts> after reviewing.
    // This is the same function a future "build me an app" voice command calls;
    // exposing it as a CLI keeps the trigger explicit and out of the always-on
    // path. The feature ships ON but PROPOSE-ONLY (inert without a cloud key).
    if let Some(pos) = std::env::args().position(|a| a == "--forge-goal") {
        let goal = std::env::args().nth(pos + 1).unwrap_or_default();
        if goal.trim().is_empty() {
            anyhow::bail!("--forge-goal requires a goal argument, e.g. --forge-goal \"a URL shortener app\"");
        }
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(std::io::stderr)
            .init();
        let root = resolve_root();
        let (cfg, _issues) = Config::load(&root.join("config").join("jarvis.toml"));
        // Honor [security].encrypt_memory on the CLI forge path too: open the main
        // Db encrypted when enabled (reads the same Keychain key), else plaintext.
        let fg_key = resolve_encryption_key(cfg.security.encrypt_memory, &root.join("state")).await;
        let memory = open_memory(&root.join("state").join("jarvis.db"), fg_key.as_ref())?;
        anthropic::resolve_api_key().await;
        match forge::forge_app(&root, &cfg, &memory, &goal).await {
            forge::ForgeOutcome::Disabled => {
                println!("forge is disabled ([forge].enabled = false); nothing was drafted.");
            }
            forge::ForgeOutcome::Blocked => {
                println!("forge is blocked: no Anthropic API key resolved.");
            }
            forge::ForgeOutcome::Proposed { dir } => {
                println!(
                    "forge PROPOSED a validated app at {} — review it, then deploy with scripts/apply_forge.sh",
                    dir.display()
                );
            }
            forge::ForgeOutcome::Rejected { stage, dir } => {
                println!("forge REJECTED the draft at `{stage}`; quarantined at {}", dir.display());
            }
            forge::ForgeOutcome::Aborted { stage } => {
                println!("forge aborted at `{stage}` (cloud/infra failure).");
            }
        }
        return Ok(());
    }

    let root = resolve_root();
    for sub in ["state/ipc", "state/logs", "state/tmp"] {
        std::fs::create_dir_all(root.join(sub))
            .with_context(|| format!("creating {sub} under {}", root.display()))?;
    }
    init_tracing(&root)?;
    // Build marker — a visible "which binary am I" line so a stale daemon is
    // obvious at a glance (this has bitten restarts repeatedly). Bump the tag
    // when shipping a behavior change worth confirming live.
    info!(build = "2026-06-14-echosafe-sweep", "jarvisd starting");
    info!(root = %root.display(), "jarvisd root resolved");

    // STARTUP SELF-CHECK (WS2): validate the STRUCTURAL preconditions
    // (root/config readable, the three state subdirs exist, state/ipc is 0700)
    // BEFORE doing heavy work, so a broken tree aborts with an ACTIONABLE
    // message instead of limping into the mic loop behind a healthy-looking
    // process. This is deliberately NARROW: the inference + cloud-key legs are
    // NOT blocking (lazy-connect + local-only are resilient by design) — only a
    // genuinely structural fault (e.g. an unwritable/looser-than-0700 confined
    // socket dir, or an unreadable config) stops startup. The same engine backs
    // `jarvisd --selftest`. Honest: each failing check carries its remediation.
    {
        let startup = selfcheck::startup_blocking_checks(&root);
        if selfcheck::any_failed(&startup) {
            for c in &startup {
                if c.status == selfcheck::Status::Fail {
                    error!(check = c.name, detail = %c.detail, "startup self-check FAILED");
                }
            }
            selfcheck::print_board("jarvisd startup self-check", &startup);
            anyhow::bail!(
                "jarvisd startup self-check failed — a structural precondition did not hold (see the board above); refusing to start in a broken state"
            );
        }
        let (pass, skip, _fail) = selfcheck::tally(&startup);
        info!(pass, skip, "startup self-check passed (structural preconditions hold)");
    }

    sweep_stale_utterances(&root);
    anthropic::init_persona(&root);

    let (mut cfg, config_issues) = Config::load(&root.join("config").join("jarvis.toml"));
    // VOICE CLONING (build 2/2): merge any previously-CONFIRMED cloned voice ids
    // (state/voice/cloned.json) into the effective [voice.voices] map so a cloned
    // voice is usable exactly like a config-mapped EL voice — but WITHOUT clobbering
    // an explicit operator mapping (config wins). With the cloud voice tier OFF the
    // merged id is simply unused (Kokoro speaks), exactly like an unmapped agent. A
    // clone confirmed mid-session is persisted and takes effect from here next boot.
    voiceclone::load_clones(&root).merge_into(&mut cfg.voice.voices);
    let cfg = Arc::new(cfg);
    // Install the consequential-action gate from config ([integrations]
    // allow_consequential, default false) ONCE at startup, so every integration
    // call site reads one process-global. Only the bool is logged.
    integrations::init(&cfg);
    // CONTINUOUS SCREEN CONTEXT (#42): install the [screen_context] settings ONCE
    // so the relay-side continuous-snapshot push path reads one process-global gate
    // (mirrors integrations::init). SHIPS ON (enabled=true) but INERT WITHOUT
    // Screen-Recording TCC consent — the live capture is TCC-device-gated in the
    // Vision app, so without consent the daemon push path captures NOTHING (the
    // bounded in-RAM ring never grows on its own) and no WATCHING indicator fires.
    // The flag cannot grant the consent. With consent the live capture
    // is TCC-device-gated in the Vision app; the ring stays bounded/redacted/
    // transient + forgettable. Only the bool + cap are installed (no secrets).
    screen_context::install_settings(
        cfg.screen_context.enabled,
        cfg.screen_context.effective_cap(),
    );
    telemetry::emit(
        "system",
        "screen_context.configured",
        // Secret-free: only the gate + bounds. With enabled=false (the default)
        // the continuous loop never runs and the ring never grows.
        serde_json::json!({
            "enabled": cfg.screen_context.enabled,
            "cap": cfg.screen_context.effective_cap(),
            "interval_secs": cfg.screen_context.effective_interval_secs(),
        }),
    );
    // FURY's mission engine plans + dispatches with the heavy cloud model; wire
    // it from config ONCE so the fury_mission tool arm reads one process-global
    // (mirrors init_persona — no model threading through execute_tool).
    anthropic::init_mission(&cfg.cloud.heavy_model);
    // Self-Forge gate ([forge].enabled / mode, ships ON; PROPOSE-ONLY, inert without
    // a cloud key): wire it ONCE so the
    // forge_app tool arm reads one process-global to decide whether to run the
    // gated PROPOSE-ONLY pipeline (forge::forge_app still owns every gate) —
    // mirrors init_mission, no Config threading through execute_tool.
    anthropic::init_forge(cfg.forge.enabled, &cfg.forge.mode);
    // ANSWER ANNOTATIONS + SELF-VERIFICATION gate ([answers].cite / confidence /
    // verify, all ship ON): wire it ONCE so the prompt-building path (the
    // confidence instruction), the response path (the cite annotation), and the
    // self-verification pass (the gated, bounded critique-revise loop) read one
    // process-global — mirrors init_forge, no Config threading through
    // execute_tool. With every flag off the response is byte-for-byte today's.
    anthropic::init_answers(
        cfg.answers.cite,
        cfg.answers.confidence,
        cfg.answers.verify,
        cfg.answers.cross_check,
        cfg.answers.cross_check_model_pass,
        cfg.answers.debate,
    );
    // MCP CLIENT (docs/SANDBOX.md): connect every configured external tool server
    // ONCE at startup, then install the connected manager as the process-global so
    // the cloud tool loop can offer + route its tools WITHOUT threading a manager
    // through `execute_tool` (mirrors `init_mission` / `init_forge`). SHIPS ON but
    // INERT WITHOUT SERVERS: the `servers` list ships EMPTY, so even enabled
    // `connect_all` is a no-op and the installed manager is inert — no server
    // connects, no MCP tool is offered until a [[mcp.servers]] entry is added. Each
    // stdio server is spawned under a default-deny sandbox-exec profile; a server
    // that fails to connect is logged and skipped (one bad server never blocks the
    // rest). The connect is the ONLY place a real subprocess is spawned.
    let mut mcp_manager = mcp::McpManager::new(cfg.mcp.clone());
    if let Err(e) = mcp_manager.connect_all(&root).await {
        warn!(error = %e, "mcp: connect_all failed; continuing with no MCP tools");
    }
    // Publish a SECRET-FREE status snapshot for the HUD MCP panel (servers, their
    // connection status, exposed tools, allowed agents — never a token). Emitted
    // once after connect; the panel renders it read-only. Deferred behind
    // telemetry::init below would lose it, so capture it now and emit after init.
    let mcp_status = mcp_manager.status_snapshot();
    mcp::install(mcp_manager);

    // AT-REST ENCRYPTION ([security].encrypt_memory, ships OFF — crypto.rs). Resolve
    // the encryption key ONCE here, then every sensitive store below opens through
    // `open_store_path` / encrypted vs plaintext accordingly:
    //   * OFF (the default): `master_key` is None; every store opens via its
    //     plaintext `open(path)` with NO `PRAGMA key` — byte-for-byte today's
    //     plaintext SQLite (no key generated, no migration, no behavior change).
    //   * ON: ensure a 256-bit master key exists in the Keychain (generate +
    //     re-key/migrate the existing plaintext stores the FIRST time), then open
    //     every store ENCRYPTED with that key. Honest scope: the four SQLite stores
    //     + the voiceid profile are encrypted AT REST ON DISK; the config TOML, the
    //     Keychain item, and the in-RAM working set/key are NOT (see crypto.rs).
    let state_dir = root.join("state");

    // PANIC / LOCKDOWN emergency stop (task #12, lockdown.rs). Wire the persistence
    // marker path and RE-ENTER lockdown if a prior panic left the marker on disk —
    // BEFORE any consequential subsystem below can act, so a restart can never
    // silently drop the emergency stop. With no marker (the normal cold start, and
    // the shipped default) this comes up unlocked and every gate is byte-for-byte
    // today. Lockdown is an OVERLAY: it forces gates OFF while engaged but never
    // mutates the operator's [integrations]/[mcp]/[standing]/... switches, so an
    // unlock restores their CONFIGURED state exactly.
    let came_up_locked = lockdown::init(&state_dir);

    let master_key = resolve_encryption_key(cfg.security.encrypt_memory, &state_dir).await;

    let memory = Arc::new(open_memory(&state_dir.join("jarvis.db"), master_key.as_ref())?);

    // CONSEQUENTIAL POLICY STORE ([policy], ships EMPTY): load the USER's per-action
    // rules (state/policy.json — written ONLY by Settings / the command channel,
    // never by the tool loop) and install the process-global the three consequential
    // chokepoints read via `policy::evaluate_global`. With an empty store every
    // action evaluates to Ask, so the gate behaves byte-for-byte as today
    // (ASK/park everywhere). USER-SET ONLY: there is no policy-write tool, and the
    // chokepoints only ever READ this global.
    let policy_store = policy::PolicyStore::load(&root.join("state").join("policy.json"));
    policy::install(cfg.policy.enabled, policy_store);
    // CONSEQUENTIAL AUDIT LOG ([audit], ships ON — read-only accountability): open
    // the append-only, hash-chained, tamper-EVIDENT log in its OWN SQLite file
    // (state/audit.db) and install the process-global the chokepoints record to via
    // `audit::record_global`. It never takes an action — it records the decisions
    // the gate already makes, secret-free + bounded. With [audit].enabled false the
    // record calls are no-ops and the chokepoints behave exactly as today.
    let audit_log = Arc::new(open_audit(&state_dir.join("audit.db"), master_key.as_ref())?);
    audit::install(cfg.audit.enabled, audit_log);

    // The optimizer Trace Store: opened + held for the daemon's life exactly like
    // Memory, in its OWN dedicated SQLite file (state/optimize.db). The turn loop
    // records a redacted trace per turn THROUGH this handle — but ONLY when
    // [optimize].enabled (ships ON; live recording is runtime-gated + PII-redacted),
    // so with it disabled the store stays empty and nothing is written. Threaded into
    // run_pipeline alongside Memory and read by
    // the periodic propose-only optimize_task below.
    let optimize_root = root.join("state").join("optimize");
    if let Err(e) = std::fs::create_dir_all(&optimize_root) {
        warn!(error = %e, "optimize: failed to create state/optimize dir");
    }
    let trace_store = Arc::new(open_trace_store(
        &optimize_root.join("optimize.db"),
        master_key.as_ref(),
    )?);

    // The EVAL scorecard's live, in-memory rolling state (eval.rs): bounded
    // windows of MEASURED per-turn latencies + cloud token usage. Held for the
    // daemon's life like Memory/the trace store, updated per turn in run_pipeline
    // (latency always; cloud usage when a turn surfaces it), and read by the
    // periodic eval_report_task below — which recomputes routing accuracy +
    // correction-rate from the trace store and emits the AGGREGATE-ONLY
    // `eval.report` telemetry the HUD Eval/Optimizer panel renders. The eval
    // framework only MEASURES — it never tunes routing or changes the optimizer's
    // propose-only + OFF posture.
    let eval_state = Arc::new(tokio::sync::Mutex::new(eval::EvalState::new()));
    // LIVE COST FEED: register this same handle as the process-global usage sink
    // so the cloud reply path (anthropic.rs) can feed measured token `usage` into
    // the SAME rolling cost window eval_report_task reads — handle-free, mirroring
    // telemetry's global hub (no Arc threaded through every complete_* signature).
    eval::install_usage_sink(eval_state.clone());

    // The agent constellation: 17 profiles on the one engine. Jarvis-Prime
    // delegates each request to a specialist (router::route). Missing/malformed
    // agents.toml falls back to the canonical roster so the team always loads.
    let (agents, agent_issues) =
        agents::AgentRegistry::load(&root.join("config").join("agents.toml"));
    let agents = Arc::new(agents);

    // Micro-app runtime substrate (docs/SANDBOX.md): scan apps/ for manifests
    // so voice ("open global scan") and [apps].autostart can resolve them. The
    // session HMAC key is initialized lazily on first token mint and never
    // logged.
    let app_registry = apps::AppRegistry::discover(&root);

    telemetry::init();
    tokio::spawn(telemetry::serve(cfg.telemetry.port));
    tokio::spawn(telemetry::system_load_task());
    // Deferred from Config::load (audit fix): telemetry did not exist yet
    // when the config was parsed, so misconfiguration was a buried log WARN.
    // Emitted once the hub is up so the HUD can surface it.
    if !config_issues.is_empty() {
        telemetry::emit("system", "config.invalid", json!({"issues": config_issues}));
    }
    if !agent_issues.is_empty() {
        telemetry::emit("system", "agents.invalid", json!({"issues": agent_issues}));
    }
    // LOCKDOWN status for the HUD indicator (secret-free): the current emergency-
    // stop state, plus whether THIS start re-entered lockdown from a persisted
    // marker (a prior panic that was never unlocked). The shipped default emits
    // {locked:false, restored:false} — normal operation.
    telemetry::emit(
        "system",
        "lockdown.status",
        json!({"locked": lockdown::is_locked_down(), "restored_from_marker": came_up_locked}),
    );
    // RESIDENT LOCAL MODELS plan for the HUD indicator (#17, item 3; secret-free,
    // CONFIG-DERIVED — no model, no load). The Local tier's budget-bounded warm-set
    // PLAN: the always-resident base ([models].llm), the warm-set the policy admits
    // under the RAM budget (base first), whether multi-resident is in effect, and
    // the budget. The shipped default is CONSERVATIVE single-resident
    // (multi_resident=false) — the safe behavior on a low-RAM Mac, unchanged from
    // today. This mirrors server.py's InferenceEngine.local_warm_status but is the
    // PLAN, not what is actually resident (only the server knows that at runtime),
    // and it claims NO measured speed benefit (the swap benefit is device/RAM-gated).
    {
        let tel = crate::model_tier::local_warm_telemetry(&cfg);
        telemetry::emit(
            "system",
            "model.local_warm",
            json!({
                "base": tel.base,
                "planned": tel.planned,
                "multi_resident": tel.multi_resident,
                "budget_gib": tel.budget_gib,
            }),
        );
    }
    // MCP panel status (secret-free): configured servers, connection status,
    // exposed tools, allowed agents. Emitted once the hub is up so the HUD MCP
    // panel can render the external-tool surface read-only. Shipped-OFF default
    // (enabled=false, no servers) yields an empty, honest "MCP off" snapshot.
    telemetry::emit("system", "mcp.status", mcp_status);
    // AT-REST ENCRYPTION status (#11; secret-free — NEVER the key). Drives the HUD
    // ENCRYPTED AT REST / NOT ENCRYPTED indicator + the honest scope copy. `active`
    // is the GROUND TRUTH (the master key actually resolved this run), not just the
    // config flag — so a config-on-but-key-failed session reads honestly as NOT
    // active. The shape enumerates EXACTLY what is / isn't covered so the panel
    // never overclaims. The key itself is never in this payload.
    telemetry::emit(
        "system",
        "security.status",
        json!({
            // The [security].encrypt_memory switch (config intent).
            "encrypt_memory_config": cfg.security.encrypt_memory,
            // GROUND TRUTH: at-rest encryption is actually engaged this run.
            "active": master_key.is_some(),
            // EXACTLY which stores SQLCipher (+ the voiceid wrapper) covers.
            "encrypted_stores": [
                "memory (facts/transcripts/episodes/events + world-model facts)",
                "docsearch index (chunk text + vectors)",
                "audit log",
                "optimizer trace store",
                "voiceid owner profile (encrypted blob wrapper)"
            ],
            // EXACTLY what is NOT covered — honest scope.
            "not_encrypted": [
                "the config TOML",
                "the macOS Keychain item itself (already OS-protected)",
                "the in-RAM working set + decrypted pages + the key while jarvisd runs"
            ],
            // The honest one-liners the panel surfaces verbatim.
            "honesty": "SQLCipher protects AT REST ON DISK only — not against a live-process/root attacker (key + plaintext are in RAM while running). The master key lives only in the macOS Keychain (account memory_encryption_key); lose it and the encrypted DBs are unrecoverable. Enabling changes the on-disk format (a one-time migration).",
            // Where the key lives (NOT the key) — for the panel's key-management note.
            "key_location": "macOS Keychain (account memory_encryption_key)",
            "cipher": "SQLCipher AES-256 (transparent, whole-file, page-level)"
        }),
    );
    // SKILLS MARKETPLACE status (secret-free): the hand-written in-tree skill
    // catalog the HUD Skills panel browses — every skill's name, category,
    // one-line "when to use", and the consequential / source-gated markers, plus
    // the per-category counts, the REAL shipped total, and the live [skills]
    // master-switch state. Pure in-tree skills carry nothing secret; the snapshot
    // is bounded to that discovery surface so the panel can never render anything
    // but the catalog. Emitted once the hub is up so the panel renders read-only.
    telemetry::emit(
        "system",
        "skills.catalog",
        crate::skills::global().catalog_snapshot(cfg.skills.enabled),
    );
    // The watchdog owns the heal pipeline; it needs Memory for the
    // meta.heal_last_attempt rate limit and the meta.heal_pending marker.
    tokio::spawn(heal::watchdog(root.clone(), cfg.clone(), memory.clone()));

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    // The capture thread gets the ONLY sender: if audio capture ever dies
    // for good, the channel closes and the recv loop below ends instead of
    // idling deaf forever behind a healthy-looking process.
    audio::spawn_capture(root.clone(), cfg.clone(), tx);

    // AMBIENT SOUND MONITOR (task #15). The continuous ambient
    // sound-class monitor starts when [audio].sound_monitor is on (SHIPS ON, but
    // INERT WITHOUT MIC/TCC consent). With it off the monitor
    // NEVER starts and no microphone is opened for ambient classification; the
    // one-shot "what was that sound" intent (over a clip already captured above)
    // works regardless. Even when opted in, the actual continuous ambient capture
    // is DEVICE-GATED behind macOS mic/TCC consent the daemon cannot grant, so it
    // is not driven here; the gate decision + the monitor STATE telemetry (for the
    // HUD indicator) are what run. PRIVACY: continuous ambient listening without
    // explicit consent is a liability — this opt-in is the only path to it, and
    // there is no auto-arm anywhere. Only sound-class LABELS would ever be emitted.
    let sound_monitor_on = router::ambient_monitor_should_start(cfg.audio.sound_monitor);
    if sound_monitor_on {
        info!(
            "ambient sound monitor: OPTED IN — periodic on-device sound-class \
             classification will run once macOS mic/TCC consent is granted (labels only; audio never leaves)"
        );
    } else {
        info!("ambient sound monitor: OFF (shipped default) — never auto-starts; the mic stays closed for ambient classification");
    }
    telemetry::emit(
        "local",
        "audio.sound_monitor",
        json!({"enabled": sound_monitor_on, "consent": "device_gated", "labels_only": true, "audio_left_device": false}),
    );

    let sock_path = root.join("state").join("ipc").join("inference.sock");
    let mut infer = InferenceClient::new(sock_path.clone());
    // BACKGROUND INFERENCE LIVENESS (WS2): a lightweight connect-probe loop that
    // publishes shared inference health + an `inference.health` telemetry frame
    // every few seconds, and a coherent ONE-SHOT `inference.degraded` /
    // `inference.recovered` edge signal — so the system (and HUD) KNOW the
    // inference server is down BEFORE a user turn is lost, instead of discovering
    // it per-turn. It spends NO model call (connect + close) and never blocks or
    // panics the pipeline. The per-turn lazy-connect + honest abort path is
    // unchanged; this is the proactive half of degraded-mode honesty.
    tokio::spawn(inference::liveness_task(
        sock_path.clone(),
        inference::LIVENESS_INTERVAL,
    ));
    // Daemon-mediated generate proxy (security finding #4): a SEPARATE,
    // op-restricted socket micro-apps reach instead of the multiplexed
    // inference.sock. Only op=generate, token-gated, 256-token-capped, and
    // rate-limited; the privileged ops are structurally unreachable through it.
    // Started before autostart so it is listening when the first app launches.
    {
        let generate_sock = root.join("state").join("ipc").join("apps").join("generate.sock");
        let registry = app_registry.clone();
        let inference_sock = sock_path.clone();
        tokio::spawn(async move {
            genproxy::serve(registry, generate_sock, inference_sock).await;
        });
    }
    // HUD -> daemon COMMAND CHANNEL (the first inbound surface): a local-only,
    // token-authenticated Unix socket the HUD connects to, routing every command
    // INTO the existing gated pipeline (never around it). It can do nothing the
    // voice path cannot — a consequential ask still parks, confirm honors the
    // master switch + the agent allowlist, dismiss_forge clears a marker only
    // (apply stays scripts/apply_forge.sh). The capability token is minted once
    // here from the same HMAC machinery as the per-app tokens; it is handed to
    // the Tauri backend out-of-band (the keychain/handshake the HUD already uses)
    // and is NEVER logged. We emit only that the channel is up, never the token.
    {
        let command_sock = command::command_socket_path(&root);
        let pipeline = Arc::new(command::LivePipeline {
            memory: memory.clone(),
            agents: agents.clone(),
            heavy_model: cfg.cloud.heavy_model.clone(),
            max_tokens: cfg.cloud.max_tokens,
        });
        let dispatcher = Arc::new(command::LiveDispatcher {
            memory: memory.clone(),
            root: root.clone(),
        });
        // Mint the per-boot command token (off the logged path) and hand it to
        // the Tauri backend OUT-OF-BAND via a 0600 file inside the same 0700
        // confined state/ipc/ dir as the socket. The token is NEVER printed,
        // never put on telemetry, and never on argv/env — only its handoff
        // succeeded/failed is observable.
        let command_token = apps::mint_command_token();
        let token_written = command::write_command_token(&root, &command_token);
        telemetry::emit(
            "system",
            "command.channel_up",
            json!({"path": command_sock.display().to_string(), "token_handoff": token_written}),
        );
        tokio::spawn(async move {
            command::serve(command_sock, pipeline, dispatcher).await;
        });
    }
    // #35 WEBHOOK TRIGGERS — the inbound, HMAC-authenticated, loopback-default
    // receiver. RUNTIME-GATED: webhooks::serve returns immediately (never opens a
    // port) unless [webhooks].enabled is true (ships ON, but INERT WITHOUT MAPPINGS +
    // A KEYCHAIN HMAC SECRET), the bind is loopback,
    // AND a Keychain HMAC secret is configured. Every request it would accept is
    // verified (constant-time HMAC over the raw body), mapped via the explicit
    // event->intent allowlist, and — if the mapped intent is consequential —
    // PARKED into the existing confirmation gate (a webhook never auto-executes).
    // The pure handle_webhook is what the hermetic tests prove; this live bind is
    // the runtime-gated leg (the mic-loop / vision-capture precedent).
    {
        let webhook_cfg = cfg.clone();
        tokio::spawn(async move {
            webhooks::serve(webhook_cfg).await;
        });
    }
    // Self-learning reflection: periodically consolidates facts from recent
    // transcripts (own InferenceClient; never blocks or panics the pipeline).
    tokio::spawn(reflect::reflection_task(sock_path.clone(), memory.clone()));
    // EDITH anticipation: the runtime-only proactive loop. The pure evaluator
    // (anticipate.rs) is what the tests cover; this live tick surfaces a HUD
    // card unprompted and, ONLY when [proactive].speak is on (ships ON) and the
    // daemon is not already speaking, voices it through the existing speech path.
    tokio::spawn(anticipation_task(
        root.clone(),
        cfg.clone(),
        memory.clone(),
        sock_path.clone(),
    ));
    // Standing Missions: the runtime-only scheduled-autonomy loop. The pure
    // scheduler (standing::due_missions) + the run logic (standing::run_one) are
    // what the tests cover; this live tick loads the saved missions, runs the
    // scheduler against the subsystem master switch ([standing].enabled, OFF by
    // default — nothing fires when off), and RUNS each due mission through the
    // SAME bounded FURY engine fury_mission uses, so every consequential step
    // still parks behind the confirmation gate. Surfaces a standing.run HUD card;
    // speaks only when [proactive].speak is on and not already speaking.
    tokio::spawn(standing_task(
        root.clone(),
        cfg.clone(),
        memory.clone(),
        sock_path.clone(),
    ));
    // jarvis.db retention: prune old events, cap transcripts + episodes (audit
    // fix). The episodes cap is the [episodic].retention bound (bounded memory).
    // The notebook cap is the [notebooks].retention evict-oldest bound (#19), or
    // None when the store is off — the SAME bounded-memory posture as episodic.
    tokio::spawn(retention_task(
        memory.clone(),
        cfg.episodic.retention,
        cfg.notebooks.enabled.then_some(cfg.notebooks.retention),
    ));
    // Periodic PROPOSE-ONLY optimizer pass: reads the accruing redacted trace
    // corpus and, ONLY when [optimize].enabled (ships ON), writes a reviewable
    // routing-tuning proposal under state/optimize/proposals/<ts>/. It NEVER
    // mutates the live routing config — a human reviews + applies via
    // scripts/apply_optimization.sh. A no-op while the master switch is off.
    tokio::spawn(optimize_task(
        cfg.clone(),
        trace_store.clone(),
        optimize_root.clone(),
    ));
    // Periodic EVAL report pass: reads the live rolling latency/cost window +
    // recomputes routing accuracy + correction-rate from the trace corpus and
    // emits the aggregate-only `eval.report` telemetry for the HUD Eval/Optimizer
    // panel. MEASURE-ONLY — it never tunes routing or changes the optimizer's
    // propose-only + OFF posture; a metric with no data reads "awaiting turns".
    tokio::spawn(eval_report_task(
        cfg.clone(),
        trace_store.clone(),
        eval_state.clone(),
    ));
    // Resolve the Anthropic API key eagerly (env var, then macOS Keychain) so
    // daemon.started reports whether the cloud path is available. Only the
    // bool ever leaves this call — the key itself stays out of logs and
    // telemetry. The cloud router reuses this cached resolution later.
    let cloud_key_present = anthropic::resolve_api_key().await.is_some();
    info!(cloud_key_present, "anthropic API key resolution complete");
    telemetry::emit(
        "system",
        "daemon.started",
        json!({
            "root": root.display().to_string(),
            "cloud_key_present": cloud_key_present,
        }),
    );

    // AGGREGATED READINESS (WS2): one coherent `daemon.ready` frame AFTER the
    // spawns, so readiness is OBSERVABLE without blocking startup (lazy-connect
    // resilience is preserved — we do NOT gate the mic loop on inference being
    // up). A non-fatal connect-probe of inference.sock seeds the initial
    // reachability; `None` means UNKNOWN (socket absent / probe skipped), never
    // a silent "false". The background liveness loop keeps it fresh after this.
    {
        let inference_reachable: Option<bool> = if sock_path.exists() {
            Some(
                InferenceClient::new(sock_path.clone())
                    .probe_reachable()
                    .await
                    .is_ok(),
            )
        } else {
            None // honest UNKNOWN: the server hasn't created its socket yet
        };
        // The hub was initialized + its WS server spawned earlier in startup;
        // this frame reaching subscribers at all is itself evidence the hub is
        // up. We report true honestly (a failed bind logs an error in serve()
        // and the HUD simply never connects — it would not see this frame).
        let telemetry_bound = true;
        let ready = selfcheck::ready_frame(
            &root,
            inference_reachable,
            cloud_key_present,
            telemetry_bound,
            true, // the startup self-check already passed (we'd have bailed otherwise)
        );
        info!(
            inference_reachable = ?inference_reachable,
            cloud_key_present,
            "daemon ready (lazy-connect resilient: startup is not gated on inference)"
        );
        telemetry::emit("system", "daemon.ready", ready);
    }

    // Announce the installed micro-app registry to the HUD: which apps exist
    // and whether each is running. Lets the HUD render an OFFLINE placeholder
    // per app before any of them starts.
    {
        let installed = app_registry.list().await;
        let names: Vec<&str> = installed.iter().map(|a| a.name.as_str()).collect();
        info!(apps = ?names, "micro-app registry discovered");
        telemetry::emit(
            "system",
            "app.registry",
            json!({
                "apps": installed
                    .iter()
                    .map(|a| json!({"name": a.name, "description": a.description, "running": a.running}))
                    .collect::<Vec<_>>(),
            }),
        );
    }

    // [apps].autostart: launch each named micro-app under its seatbelt
    // profile. Empty by default — nothing autostarts unless the operator opts
    // in. An unknown name is reported, never fatal.
    for name in &cfg.apps.autostart {
        match apps::start(&app_registry, name).await {
            Ok(()) => {
                info!(app = %name, "autostarted micro-app");
                // #36 PLUGIN SDK — the register-on-launch HANDSHAKE, RUNTIME-GATED
                // by [plugin_sdk].enabled (ships ON). When on, re-validate the
                // launched plugin's manifest contract ([intents]/[tools]) AND
                // verify its just-minted capability token (the SAME HMAC machinery
                // the per-app relay uses). The outcome is surfaced secret-free to
                // the HUD — never the token. A forged token / invalid manifest is
                // NOT admitted. The pure register_plugin is what the tests prove;
                // this is its live wiring.
                if cfg.plugin_sdk.enabled {
                    let outcome = app_registry.register_on_launch(name).await;
                    let (status, detail) = match &outcome {
                        plugin_sdk::HandshakeOutcome::Admitted { intents, .. } => {
                            ("admitted", format!("{} intents", intents.len()))
                        }
                        plugin_sdk::HandshakeOutcome::InvalidManifest(e) => {
                            ("invalid_manifest", e.clone())
                        }
                        plugin_sdk::HandshakeOutcome::Unauthorized => {
                            ("unauthorized", String::new())
                        }
                    };
                    info!(app = %name, status, "plugin handshake (#36)");
                    telemetry::emit(
                        "system",
                        "plugin.handshake",
                        json!({"name": name, "status": status, "detail": detail}),
                    );
                }
            }
            Err(e) => {
                warn!(app = %name, error = %e, "autostart skipped");
                telemetry::emit(
                    "system",
                    "app.autostart_failed",
                    json!({"name": name, "error": e.to_string()}),
                );
            }
        }
    }

    // CONTINUOUS SCREEN CONTEXT (#42): if [screen_context].enabled, START the
    // device-gated continuous OCR loop in the Vision app by sending it the
    // `screen.context.start` op. STRICTLY GATED: this fires ONLY when
    // [screen_context].enabled is on (ships ON — with enabled=false this whole block
    // is skipped and no loop ever arms; even ON it is INERT WITHOUT Screen-Recording
    // TCC consent). Best-effort: if the Vision app is not autostarted/running the op is
    // dropped with a warning (the live capture still requires the app + runtime
    // Screen-Recording TCC consent — the daemon flag alone can grant neither). The
    // app side honors the device gate; a TCC denial stops the loop cleanly,
    // capturing nothing. Only the secret-free interval/source rides the op.
    if cfg.screen_context.enabled {
        let interval = cfg.screen_context.effective_interval_secs();
        let op_line = json!({
            "op": "screen.context.start",
            "source": "screen",
            "interval_secs": interval,
        })
        .to_string();
        match apps::send_op(&app_registry, "vision", &op_line).await {
            Ok(()) => {
                info!(interval_secs = interval, "started continuous screen-context loop (#42)");
                telemetry::emit(
                    "system",
                    "screen_context.loop_started",
                    json!({"interval_secs": interval}),
                );
            }
            Err(e) => {
                warn!(error = %e, "screen-context loop not started (Vision app not running?)");
                telemetry::emit(
                    "system",
                    "screen_context.loop_start_failed",
                    json!({"error": e.to_string()}),
                );
            }
        }
    }

    // One utterance at a time, driven to completion (audit fix: see the
    // Event doc comment — no stage of a later utterance may start while an
    // earlier one's ReplySession is alive). `last_reply` carries JARVIS's most
    // recent spoken response into the next pipeline for the self-echo reject
    // (RC-5): a captured fragment of his own reply is dropped before it can
    // actuate, even if a gate window leaked.
    let mut last_reply: Option<String> = None;
    // The enrolled owner profile (round G, voice-id). Loaded ONCE at boot and
    // re-loaded when an enroll/forget intent changes it. `None` = unenrolled =
    // voice-id gates nothing (unchanged behavior). Held across turns so each
    // verification is against the live profile without a per-turn disk read.
    // AT-REST WIRING (security finding #2): when [security].encrypt_memory is ON the
    // resolved master key is installed process-globally (crypto::install_master_key),
    // and the owner profile lives in its ENCRYPTED vault (owner.enc.db), NOT the
    // plaintext owner.json — so the boot read MUST go through the vault when a key is
    // present, else it reads an absent file and silently loses the enrolled owner.
    // With encryption OFF (no global key) this is the plaintext load_profile, exactly
    // today's behavior.
    let mut owner_profile: Option<voiceid::OwnerProfile> = if cfg.voice_id.enabled {
        match crypto::global_key() {
            Some(key) => voiceid::load_profile_encrypted(&root, &key),
            None => voiceid::load_profile(&root),
        }
    } else {
        None
    };
    // An in-progress explicit enrollment session, if the owner said "enroll my
    // voice". Driven by subsequent owner utterances until min_enroll_samples are
    // captured, then persisted. Never automatic.
    let mut enrollment: Option<voiceid::Enrollment> = None;
    // VOICE CLONING (build 2/2): the CROSS-TURN consent state for a proposed clone
    // (Idle until "clone my voice" parks a Pending; a confirming "yes" next turn
    // uploads), and the LOCALLY-stored cloned voice ids (loaded once at boot,
    // re-saved on a confirmed clone / forget). Consent-gated, never automatic.
    let mut clone_state: voiceclone::CloneState = voiceclone::CloneState::Idle;
    let mut cloned_voices: voiceclone::ClonedVoices = voiceclone::load_clones(&root);
    // CROSS-TURN CORRECTION LABELING (optimizer): the PRIOR turn's recorded trace
    // (row id + intent + agent), carried forward so THIS turn can re-label it
    // Corrected IFF it corrected the prior routing (see optimize::is_correction).
    // `None` until the first trace is recorded; only ever Some while
    // [optimize].enabled. Held here exactly like `last_reply`, serially.
    let mut prior_turn: Option<optimize::PriorTurn> = None;
    while let Some(event) = rx.recv().await {
        let Event::Utterance { wav, embedding } = event;
        let response = run_pipeline(
            wav,
            embedding,
            &root,
            &cfg,
            &memory,
            &trace_store,
            &eval_state,
            &mut prior_turn,
            &mut infer,
            &sock_path,
            &app_registry,
            &agents,
            last_reply.as_deref(),
            &mut owner_profile,
            &mut enrollment,
            &mut clone_state,
            &mut cloned_voices,
        )
        .await;
        // Keep the last NON-empty spoken reply; a turn that produced nothing
        // (dropped/abandoned) leaves the prior reply as the echo reference.
        if let Some(r) = response {
            if !r.trim().is_empty() {
                last_reply = Some(r);
            }
        }
    }
    Ok(())
}

/// One utterance's full pipeline, inline: staleness gate -> reply session +
/// STT -> classify -> (proactive brief) -> route -> speak -> bookkeeping.
/// Every early return closes the reply session via abandon(), so the mic
/// guard and the sink can never leak.
#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    wav: PathBuf,
    embedding: Option<Vec<f32>>,
    root: &Path,
    cfg: &Arc<Config>,
    memory: &Arc<Memory>,
    trace_store: &Arc<optimize::TraceStore>,
    eval_state: &Arc<tokio::sync::Mutex<eval::EvalState>>,
    prior_turn: &mut Option<optimize::PriorTurn>,
    infer: &mut InferenceClient,
    sock_path: &Path,
    app_registry: &Arc<apps::AppRegistry>,
    agents: &Arc<agents::AgentRegistry>,
    last_reply: Option<&str>,
    owner_profile: &mut Option<voiceid::OwnerProfile>,
    enrollment: &mut Option<voiceid::Enrollment>,
    clone_state: &mut voiceclone::CloneState,
    cloned_voices: &mut voiceclone::ClonedVoices,
) -> Option<String> {
    let started = Instant::now();
    let wav_str = wav.display().to_string();
    // Voice-id is a per-turn process-global gate (like consequential_allowed):
    // whatever path this turn takes, clear it on the way out so a later turn that
    // skips verification never inherits a stale verified=true. A guard makes every
    // early return safe.
    let _gate_guard = TurnGateGuard;

    // Per-turn RESPONSE-VOICE-LANGUAGE: the babel_interpret TOOL records the language
    // it translated INTO (to_lang) in a per-turn process-global so the response-speak
    // site below can voice the translated text in that language (EL multilingual when
    // the cloud voice tier is on; inert otherwise). This guard CLEARS that slot on
    // EVERY return path so a Babel turn's target language never leaks into a later
    // (non-Babel) turn's voicing — the exact analogue of TurnGateGuard for voice-id.
    let _lang_guard = anthropic::TurnLangGuard;

    // Per-turn ANSWER-SOURCES accumulator: the cloud tool loop appends the REAL
    // tool-result sources (citation-carrying reads) that fed THIS turn into a
    // process-global Vec; the response path below surfaces them as the answer's
    // "Sources:" line when [answers].cite is on. This guard CLEARS that accumulator
    // on EVERY return path so a retrieval turn's sources can never annotate the
    // NEXT turn (the no-cross-turn-leak contract) — the exact analogue of
    // TurnGateGuard / TurnLangGuard. Inert when [answers] is disabled.
    let _sources_guard = anthropic::TurnSourcesGuard;

    // Per-turn SELF-VERIFICATION outcome ([answers].verify, ships ON): the cloud
    // path records THIS turn's verify outcome (off | verified-clean | revised |
    // flagged) into a process-global the response path below surfaces as the HUD
    // badge. This guard CLEARS it on EVERY return path so turn N's outcome can never
    // label turn N+1 — the exact analogue of TurnSourcesGuard. Inert when verify
    // is disabled (the outcome stays `Off` and the HUD renders nothing).
    let _verify_guard = anthropic::TurnVerifyGuard;

    // Per-turn TOOL-RESULT CROSS-CHECK outcome (#21, [answers].cross_check, ships ON)
    // and MULTI-MODEL DEBATE outcome (#22, [answers].debate, ships ON): the
    // cloud path records THIS turn's outcome into a process-global the response path
    // surfaces as a SECRET-FREE HUD badge. Each guard CLEARS its slot on EVERY return
    // path so turn N's outcome can never label turn N+1 — the exact analogues of
    // TurnVerifyGuard. Inert when disabled (the outcomes stay `Off`).
    let _cross_check_guard = anthropic::TurnCrossCheckGuard;
    let _debate_guard = anthropic::TurnDebateGuard;

    // Clear any barge-in flag left from interrupting the PREVIOUS reply, FIRST
    // of all (RC-3). This MUST precede the staleness gate: a barge whose
    // interrupting utterance then waits out STALE_UTTERANCE_WAIT (the common
    // cloud-round-trip case) used to early-return below WITHOUT clearing,
    // wedging BARGE_IN latched true forever — which held the capture gate's
    // old "barge means capture" path open across all future playback, so
    // JARVIS permanently re-transcribed himself. clear_barge_in is idempotent
    // and cheap; by the time any utterance is processed the old reply is
    // already cancelled, so clearing first is always correct.
    speech::clear_barge_in();

    // Staleness gate BEFORE any reply session exists (audit fix: utterances
    // piled up behind a long cloud turn used to be answered serially minutes
    // later, each with a fresh opener). queue_ms makes the wait visible in
    // pipeline.completed either way.
    let queue_wait = utterance_queue_wait(&wav);
    let queue_ms = queue_wait.map(|w| w.as_millis() as u64).unwrap_or(0);
    if is_stale_wait(queue_wait) {
        info!(path = %wav_str, waited_ms = queue_ms, "utterance waited out a long in-flight turn; discarding as stale");
        telemetry::emit(
            "audio",
            "utterance.stale",
            json!({"path": wav_str, "waited_ms": queue_ms}),
        );
        discard_wav(&wav);
        return None;
    }

    telemetry::emit("audio", "utterance.captured", json!({"path": wav_str}));
    // Resolve the STT backend ONCE for this utterance BEFORE the concurrent legs:
    // with the cloud-STT tier inactive (no key / offline; the flag ships ON but is
    // INERT WITHOUT A KEY) this is on-device whisper
    // with ZERO Keychain access and the EXACT pre-tier transcribe wire; the Scribe
    // branch is reached only when [voice].cloud_stt is on + a key is present + the
    // operator is not offline. The resolved key (Scribe only) is threaded straight
    // into the request body and never logged/telemetried. Cheap + key-free on the
    // default path, so it can run before the join without borrowing `infer`.
    let (stt_backend, stt_key) = speech::resolve_transcribe_backend(cfg).await;
    // Three concurrent legs from the moment of pickup:
    //   1. the reply session (sleeps the opener_delay_ms breath,
    //      then fires the acknowledgment — BEFORE transcription
    //      finishes, owning guard+sink for everything downstream),
    //   2. STT, which must never wait behind that breath,
    //   3. event bookkeeping (a busy jarvis.db must not delay
    //      either of the above).
    let (mut reply, (transcribed, stt_ms), ()) = tokio::join!(
        speech::ReplySession::begin(root, cfg),
        async {
            let stt_started = Instant::now();
            // #31: the DIARIZED transcribe returns the Scribe per-word stream too (only
            // when EL Scribe diarized); on the on-device whisper path `words` is empty
            // and the diarize block below renders the honest single stream. Same single
            // request — no extra audio leaves the device.
            let result = infer
                .transcribe_diarized(&wav, &stt_backend, stt_key.as_deref())
                .await;
            (result, stt_started.elapsed().as_millis() as u64)
        },
        async {
            if let Err(e) = memory.record_event("audio", "utterance.captured", &wav_str).await {
                warn!(error = %e, "failed to record utterance event");
            }
        },
    );
    let (text, scribe_words) = match transcribed {
        Ok((text, words)) if !text.trim().is_empty() => {
            telemetry::emit("local", "stt.transcript", json!({"text": text}));
            (text, words)
        }
        Ok(_) => {
            telemetry::emit("local", "stt.empty", json!({"path": wav_str}));
            discard_wav(&wav);
            // The opener (if any) played for nothing — close the
            // session; telemetry records it as orphaned.
            reply.abandon("stt.empty").await;
            return None;
        }
        Err(e) => {
            // error! (not warn!): a dead inference server is the
            // canonical recurring outage the self-heal watchdog's
            // ERROR-burst detector must see (audit fix — every
            // recurring failure used to log at WARN, so the
            // detector could never fire).
            error!(error = %e, "transcription failed; is the inference server up?");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "transcribe", "error": e.to_string()}),
            );
            discard_wav(&wav);
            reply.abandon("stt.failed").await;
            return None;
        }
    };

    // Self-echo / plausibility reject (RC-5), BEFORE classify+route. A
    // one-word fragment, or a transcript whose words are wholly contained in
    // JARVIS's just-spoken reply, is dropped here so a leaked echo shard can
    // never reach an actuator — defense-in-depth behind the capture gate.
    //
    // EXCEPTION: while a confirmation is parked, skip this reject. The parked
    // prompt ("... say 'confirm' to proceed or 'cancel' to drop it.") IS the
    // last_reply, so the exact replies it invites ("confirm"/"cancel", one
    // word, and "confirm it"/"cancel it", token-contained) would otherwise be
    // dropped here — the system would discard the very words it just asked for,
    // and a "cancel" the user believes landed would leave the action armed for
    // its TTL. With a pending live we let the utterance reach router::route ->
    // confirm::take_live + classify_confirmation, whose own conservative rules
    // handle short/echo-shaped replies (a stray real echo classifies Unrelated
    // and merely drops the pending — acceptable). is_live is side-effect-safe
    // (it only self-clears an expired slot).
    if !crate::confirm::is_live(std::time::Instant::now()) && is_self_echo(&text, last_reply) {
        info!(text = %text, "dropping implausible/self-echo transcript before route");
        telemetry::emit(
            "audio",
            "utterance.self_echo",
            json!({"text": text, "path": wav_str}),
        );
        discard_wav(&wav);
        reply.abandon("self_echo").await;
        return None;
    }

    // #31 MULTI-SPEAKER DIARIZATION (diarize.rs), on the transcript path. ON by default
    // ([voice].diarize) but INERT ON-DEVICE; EL-Scribe-gated. When active, a diarized transcript flows to the
    // HUD/telemetry: on the EL-Scribe STT backend (which carries speaker labels) the PURE
    // `diarize` mapper would render distinct per-speaker turns; on the on-device whisper
    // backend (no diarization model) we use the HONEST single-stream "speaker: unknown"
    // labeling — NEVER fabricating distinct speakers. The live transcribe wire returns a
    // single `text` stream today, so this surfaces the honest single-stream view + flags
    // whether the active backend can diarize at all (EL Scribe) — the multi-speaker
    // consumer is the same PURE `diarize` proven in diarize.rs, ready for the labeled
    // seam. With the flag OFF this whole block is skipped and the transcript is
    // byte-for-byte today's (no labels, no telemetry).
    if cfg.voice.diarize {
        // Whether the resolved STT backend even CARRIES speaker labels (EL Scribe does;
        // on-device whisper does not). Honest signal for the HUD — diarization is
        // EL-Scribe-gated.
        let scribe_active = stt_backend.is_cloud();
        // When the EL-Scribe backend returned a per-word stream (it diarized), CONSUME
        // those REAL labels through the PURE `diarize::diarize` mapper — distinct turns
        // appear iff Scribe distinguished speakers, never fabricated. Otherwise (the
        // on-device whisper path, or a Scribe response with no word detail) render the
        // HONEST single stream — on-device whisper has no diarization model, so we never
        // invent speakers.
        let turns = if !scribe_words.is_empty() {
            let resp = diarize::ScribeResponse {
                text: text.clone(),
                words: scribe_words.clone(),
            };
            diarize::diarize(&resp)
        } else {
            diarize::single_stream(&text)
        };
        telemetry::emit(
            "local",
            "transcript.diarized",
            json!({
                "transcript": diarize::render(&turns),
                "turns": turns.len(),
                "multi_speaker": diarize::is_multi_speaker(&turns),
                // Honesty: on-device whisper cannot diarize. Only the EL-Scribe backend
                // carries the labels the PURE `diarize` consumer renders; this is true
                // only when Scribe actually returned the word stream consumed above.
                "backend_can_diarize": scribe_active,
                "from_scribe_labels": !scribe_words.is_empty(),
            }),
        );
    }

    // #30 CONTINUOUS LIVE INTERPRETATION (interpret.rs), the DEVICE-GATED live feed of
    // each VAD segment. ON by default ([interpret].live) but INERT WITHOUT MIC/TCC — with it off this whole branch
    // is skipped and the segment is classified/routed exactly as today. When ON, the
    // freshly-transcribed segment is run through the PURE `interpret::interpret_segment`
    // pipeline (transcript -> on-device-LLM translate -> rendered translation), which
    // degrades HONESTLY offline (never a fabricated translation); when [interpret].speak
    // is on the bare translation is voiced through the SINGLE echo-safe speech path
    // (`speak_in_lang`), so the mic-mute guard, barge-in, and the is_speaking() capture
    // gate ALL cover it. The interpreter renders the other party's words turn-by-turn —
    // it does NOT classify/route them as commands — so this returns once the segment is
    // interpreted. The always-listening mic loop that drives this is DEVICE-GATED (wired
    // at the audio.rs segment site behind the SAME flag); only the pure core is proven
    // headlessly. NOTE: this is BEFORE the wake gate on purpose — an interpreter session
    // renders every segment, it is not "addressed to JARVIS" speech.
    if cfg.interpret.live {
        let outcome = interpret::interpret_segment_live(
            &text,
            cfg,
            infer,
            started,
            &mut reply,
        )
        .await;
        // When speak was off (render-only) the session never voiced anything; close it
        // so the mic guard/sink never leak. When speak was on, interpret_segment_live
        // already drove speak_in_lang (which completes the session via reply.complete);
        // an extra complete() is an idempotent drain.
        if outcome.speak.is_none() {
            reply.abandon("interpret.render_only").await;
        }
        discard_wav(&wav);
        return Some(outcome.translated_text);
    }

    // #32 CUSTOM WAKE-WORD gate (wake.rs), AFTER STT and the self-echo reject. OFF by
    // default ([wake].enabled) — with it off `wake_gate` returns true unconditionally and
    // activation is byte-for-byte today's. When ON, an utterance that does NOT contain the
    // configured wake phrase (default "jarvis", which preserves today's behavior) is
    // dropped here as "not for JARVIS" — the conservative PURE matcher never triggers on a
    // substring of a larger word and never matches an empty/blank phrase. EXCEPTION: while
    // a confirmation is parked we skip the gate, exactly like the self-echo reject above —
    // the invited "confirm"/"cancel" reply need not repeat the wake word. The
    // always-listening loop that produced `text` is DEVICE-GATED; this gate over the
    // already-produced transcript is the PURE wiring.
    if !crate::confirm::is_live(std::time::Instant::now()) && !wake::wake_gate(cfg, &text) {
        info!(text = %text, phrase = %cfg.wake.phrase, "utterance lacks the wake word; not for JARVIS");
        telemetry::emit(
            "audio",
            "utterance.no_wake",
            json!({"phrase": cfg.wake.phrase, "path": wav_str}),
        );
        discard_wav(&wav);
        reply.abandon("no_wake").await;
        return None;
    }

    // VOICE-ID (round G), AFTER STT (the enroll/forget intents need the text)
    // and BEFORE routing/classification. Three responsibilities, all no-ops when
    // [voice_id].enabled is false:
    //   1. ENROLL / FORGET intents — explicit, never automatic — are handled
    //      here and return a spoken acknowledgment without routing.
    //   2. Otherwise verify THIS utterance's embedding against the owner profile,
    //      install the per-turn owner gate (consulted deep in execute_tool /
    //      replay_confirmed_action), and emit the secret-free voiceid.verify
    //      telemetry.
    // With voice-id off, OR with no profile enrolled (and not enrolling), the
    // gate stays OFF and behavior is unchanged.
    if cfg.voice_id.enabled {
        if let Some(resp) = handle_voice_id(
            &text,
            embedding.as_deref(),
            root,
            cfg,
            owner_profile,
            enrollment,
        ) {
            // An enroll/forget intent (or an enrollment-in-progress capture):
            // speak the acknowledgment through the already-open session and end
            // the turn — these never route.
            let report = speech::speak(&resp, infer, cfg, started, &mut reply).await;
            let _ = report;
            discard_wav(&wav);
            return Some(resp);
        }
    }

    // VOICE CLONING (build 2/2), AFTER STT and BEFORE routing. CONSENT-GATED across
    // turns: a "clone my voice" intent PROPOSES a clone and parks (nothing leaves the
    // device); a confirming "yes" on the NEXT turn performs the upload via the
    // inference clone seam and stores the returned voice id. A "forget the clone"
    // intent drops the stored id. All three return a spoken acknowledgment without
    // routing; an ordinary utterance returns None and falls through. NEVER automatic.
    if let Some(resp) = handle_voice_clone(
        &text,
        root,
        clone_state,
        cloned_voices,
        infer,
    )
    .await
    {
        let report = speech::speak(&resp, infer, cfg, started, &mut reply).await;
        let _ = report;
        discard_wav(&wav);
        return Some(resp);
    }

    // MACRO REPLAY (#27), AFTER STT and BEFORE classify/route. A "replay macro X"
    // utterance re-runs each recorded command through the FULL classify -> route ->
    // gate pipeline, ONE AT A TIME — exactly as if the user spoke each command live.
    // SAFETY: a consequential recorded step therefore hits the cross-turn
    // confirmation gate + the [integrations] master switch FRESH (it parks for a
    // spoken yes; replay grants NO pre-approval and NEVER batches past the gate).
    // ON by default ([macros].enabled): with it off this reports the subsystem is
    // off and runs nothing. The recorded steps hold only utterances + intent names
    // (redacted at record time), never a secret.
    if let Some(crate::macros::MacroCommand::Replay { name }) =
        crate::macros::classify_macro_command(&text)
    {
        // Compute the same brief + cloud-reachability the normal route path uses, so
        // each replayed step routes identically to a live command.
        let brief = proactive::first_contact_brief(cfg, memory).await;
        let cloud_reachable = anthropic::resolve_api_key().await.is_some();
        let resp = replay_macro_live(
            &name,
            cfg,
            memory,
            infer,
            started,
            &mut reply,
            brief.as_deref(),
            app_registry,
            agents,
            cloud_reachable,
            root,
        )
        .await;
        discard_wav(&wav);
        return Some(resp);
    }

    let mut timing = PipelineTiming { queue_ms, stt_ms, ..Default::default() };

    let classify_started = Instant::now();
    let class = match infer.classify(&text).await {
        Ok(class) => {
            timing.classify_ms = classify_started.elapsed().as_millis() as u64;
            telemetry::emit(
                "local",
                "intent.classified",
                json!({
                    "intent": class.intent,
                    "confidence": class.confidence,
                    "complexity": class.complexity,
                }),
            );
            class
        }
        Err(e) => {
            // error!: recurring hard failure; feeds the self-heal
            // burst detector (see the transcription arm).
            error!(error = %e, "classification failed");
            telemetry::emit(
                "system",
                "inference.unavailable",
                json!({"op": "classify", "error": e.to_string()}),
            );
            discard_wav(&wav);
            reply.abandon("classify.failed").await;
            return None;
        }
    };

    // Proactive learning: when this utterance ends an away gap longer than
    // [proactive].idle_gap_hours, the first-contact brief rides into the
    // converse data and the persona phrases it (emits proactive.brief).
    let brief = proactive::first_contact_brief(cfg, memory).await;

    // Cloud reachability for Jarvis-Prime delegation: the API key is the
    // honest signal (resolved once, cached). With no key, conversational turns
    // route to hulk's all-local survival profile instead of the cloud.
    let cloud_reachable = anthropic::resolve_api_key().await.is_some();

    let route_started = Instant::now();
    match router::route(
        &class,
        &text,
        cfg,
        memory,
        infer,
        started,
        &mut reply,
        brief.as_deref(),
        app_registry,
        agents,
        cloud_reachable,
        root,
    )
    .await
    {
        Ok(mut outcome) => {
            // Converse replies were already spoken inside route()
            // with route_ms measured to the server's done event;
            // otherwise route_ms is simply time inside route()
            // (in-persona generate or cloud completion).
            let prespoken = match outcome.spoken {
                Some(spoken) => {
                    timing.route_ms = spoken.route_ms;
                    Some(spoken.report)
                }
                None => {
                    timing.route_ms = route_started.elapsed().as_millis() as u64;
                    None
                }
            };
            // ANSWER ANNOTATIONS ([answers], ships ON): surface the REAL
            // tool-result SOURCES that fed this turn (cite) — or "from my own
            // knowledge" when no retrieval ran — and the model's SELF-REPORTED
            // confidence (parsed off the answer + carried on the structured HUD
            // field). Reads the per-turn source accumulator the cloud tool loop
            // populated (cleared each turn by the TurnSourcesGuard above). With
            // BOTH gates off (the default) annotate_answer returns the response
            // byte-for-byte unchanged and an empty payload, so EVERY downstream use
            // (telemetry, transcript, speak, episodic, learning) is unchanged. The
            // SECRET-FREE annotation payload rides answer.annotated for the HUD: the
            // real source locators + bounded snippets (never an embedding/audio),
            // the from-my-knowledge flag, and the parsed confidence level + reason.
            let annotated = anthropic::annotate_answer(&outcome.response);
            outcome.response = annotated.response;
            telemetry::emit("system", "answer.annotated", annotated.telemetry);
            // SELF-VERIFICATION badge ([answers].verify, ships ON): surface THIS
            // turn's verify outcome (off | verified-clean | revised | flagged) the
            // cloud path recorded, as a SECRET-FREE HUD payload (the outcome token +
            // badge + honest copy — never the flagged-claim text or any content
            // beyond the answer). With verify off the outcome is `Off` and the HUD
            // renders nothing, so today's behavior is unchanged. Cleared each turn by
            // the TurnVerifyGuard above. HONEST: a second self-check REDUCES — does
            // not eliminate — errors; it is never a correctness guarantee.
            let verify_outcome: anthropic::VerifyOutcome = anthropic::current_outcome();
            telemetry::emit(
                "system",
                "answer.verified",
                anthropic::verify_telemetry(cfg.answers.verify, verify_outcome),
            );
            // TOOL-RESULT CROSS-CHECK badge (#21, [answers].cross_check, ships ON):
            // surface THIS turn's cross-check outcome (off | plausible | flagged) the
            // cloud path recorded, as a SECRET-FREE HUD payload (the outcome token +
            // badge + honest copy — never the raw tool result; the flag reasons +
            // caveat ride the answer text). With cross_check off the outcome is `Off`
            // and the HUD renders nothing. Cleared each turn by the guard above.
            // HONEST: it only DOWNGRADES confidence + FLAGS; it NEVER removes a gate.
            let cross_check_outcome: anthropic::CrossCheckOutcome =
                anthropic::cross_check_current_outcome();
            telemetry::emit(
                "system",
                "answer.cross_checked",
                anthropic::cross_check_badge_telemetry(cfg.answers.cross_check, cross_check_outcome),
            );
            // MULTI-MODEL DEBATE badge (#22, [answers].debate, ships ON): surface
            // THIS turn's debate outcome (off | agree | disagree | fallback). With
            // debate off the outcome is `Off` and the HUD renders nothing. Cleared
            // each turn by the guard above. HONEST: agreement raises confidence;
            // disagreement surfaces BOTH (never picked/averaged); an unavailable
            // second brain falls back to one and says so.
            let debate_outcome: anthropic::DebateOutcome = anthropic::debate_current_outcome();
            telemetry::emit(
                "system",
                "answer.debated",
                anthropic::debate_badge_telemetry(cfg.answers.debate, debate_outcome),
            );
            let source = if outcome.routed_to == "cloud" { "cloud" } else { "local" };
            telemetry::emit(
                source,
                "route.completed",
                json!({
                    "routed_to": outcome.routed_to,
                    "agent": outcome.agent,
                    "namespace": outcome.namespace,
                    "response": outcome.response,
                }),
            );
            if let Err(e) = memory
                .record_transcript(
                    Some(&wav.display().to_string()),
                    &text,
                    &class.intent,
                    outcome.routed_to,
                    Some(&outcome.response),
                )
                .await
            {
                warn!(error = %e, "failed to record transcript");
            }
            // MACRO CAPTURE (#27): while a recording is in progress, append THIS
            // command (the utterance + its classified intent) to the buffer — but
            // never a macro CONTROL command itself (start/stop/list/forget/replay),
            // so a recording captures only the real work, not the verbs that bracket
            // it. The captured text is redacted at PERSIST time (macros.rs), so a
            // secret never reaches the store. Capturing changes no gate — the command
            // already ran (and re-gated) normally above.
            if crate::macros::is_recording()
                && crate::macros::classify_macro_command(&text).is_none()
            {
                crate::macros::capture(&text, &class.intent);
            }
            // The WAV has served its purpose (transcribed and
            // logged); without cleanup state/tmp grows unbounded.
            discard_wav(&wav);
            info!(response = %outcome.response, routed_to = outcome.routed_to, "responding");
            // Cloud (and converse-fallback) replies are spoken
            // here through the speak op + the same reply session
            // the opener started; converse replies already
            // played (and closed the session) inside route().
            // Reports time against `started` (utterance pickup),
            // measured before the 400ms echo-mute tail.
            let report = match prespoken {
                Some(report) => report,
                None => {
                    // RESPONSE-VOICE-LANGUAGE: a babel_interpret TOOL turn recorded the
                    // language it translated INTO; voice the translated response in that
                    // language so the ElevenLabs backend can pick a MULTILINGUAL model
                    // (when the cloud voice tier is on). None (every non-Babel turn) =>
                    // speak_in_lang with None == speech::speak == today's behavior, and
                    // with the tier OFF / no key / offline the hint is inert (Kokoro).
                    let response_lang = anthropic::current_response_voice_lang();
                    speech::speak_in_lang(
                        &outcome.response,
                        response_lang.as_deref(),
                        infer,
                        cfg,
                        started,
                        &mut reply,
                        // The generic turn-reply site: an ordinary spoken answer ->
                        // Routine (=> Neutral prosody). Conservative default; the few
                        // sites that know a more specific kind (greeting roll-call,
                        // alerts) pass it explicitly.
                        crate::prosody::ReplyKind::Routine,
                    )
                    .await
                }
            };
            timing.first_audio_ms = report.first_audio_ms;
            timing.speak_ms = report.speak_ms;
            let total_ms = report.total_ms;
            // Learning loop: fire-and-forget fact extraction so
            // the next utterance is never blocked behind it.
            // PRIVACY — TRANSIENT SCREEN READS: a screen-read turn
            // ("read my screen" / "what's on my screen" / "where's
            // the <X> button") is NEVER fed to fact extraction. The
            // recognized on-screen text rides the vision.screen
            // telemetry event (HUD only) and never enters
            // outcome.response, but the utterance + acknowledgment
            // are ALSO kept out of lifelong memory here so a screen
            // read can never seed a durable fact / optimizer trace.
            // The text is sensitive (it can contain on-screen
            // passwords/messages); the on-device path is the
            // privacy-preferring one (a cloud-brain answer would send
            // that text to the cloud exactly like any user content).
            // A VLM DESCRIBE (task #2) is ALSO transient: describing the screen
            // or a private photo can surface sensitive VISUAL content, so its
            // utterance + acknowledgment must stay out of lifelong memory /
            // optimizer traces exactly like an OCR screen read. (The description
            // text itself is never persisted regardless; this keeps the turn's
            // utterance from seeding a durable fact too.)
            // An IDENTIFY-SOUND turn (task #15) is likewise a transient perception
            // read: like the OCR/VLM reads it should not seed lifelong memory or
            // optimizer traces (its acknowledgment is content-free about the sound
            // classes — the labels ride the async vision.sound relay, never this
            // reply — but the turn stays out of durable memory for consistency with
            // the other on-device perception reads, and the audio never leaves).
            // An IMAGE-GENERATION turn (task #18) is likewise transient: the
            // prompt + the generated image can be personal, and the prompt + the
            // pixels stay ON-DEVICE (saved under state/images/; nothing goes to the
            // cloud). Its utterance + acknowledgment must stay out of lifelong
            // memory / optimizer traces exactly like the other on-device perception
            // reads above.
            // A SCREEN-CONTEXT RECALL turn (task #42) is likewise a transient
            // perception read: "what was I working on" / "recall my screen context"
            // surfaces the BOUNDED, REDACTED recent on-screen context from the in-RAM
            // ring — which can carry sensitive on-screen content exactly like a
            // one-shot OCR read — so its utterance + acknowledgment must stay out of
            // lifelong memory / optimizer traces, mirroring the screen-read transience.
            // (The ring itself is in-RAM + transient by construction; this keeps the
            // RECALL TURN from seeding a durable fact too.)
            let transient = router::is_screen_read(&text)
                || router::is_describe_request(&text)
                || router::is_generate_image_request(&text)
                || router::is_identify_sound_request(&text)
                || screen_context::is_screen_context_recall(&text);
            if !outcome.response.is_empty() && !transient {
                spawn_learning_task(
                    sock_path.to_path_buf(),
                    memory.clone(),
                    text.clone(),
                    outcome.response.clone(),
                );
            }
            if transient {
                telemetry::emit(
                    "system",
                    "privacy.transient_screen_read",
                    json!({"persisted": false}),
                );
            }
            // EPISODIC STORE: record THIS completed turn as a durable, redacted,
            // agent-scoped, bounded EPISODE — GATED by exactly the same posture
            // the transcript/learning loop above uses. record_episode is a no-op
            // unless [episodic].enabled AND the turn is NOT a transient screen
            // read AND there is a real utterance + response AND (voice-id off /
            // unenrolled OR this turn verified as the owner) — the owner gate is
            // read from the SAME per-turn voiceid::current_turn_gate() the deep
            // tool gate consults, so an unrecognized speaker's turn never seeds
            // the owner's episodic memory. Every field is redacted before store.
            let voice = episodic::VoiceGate::from_owner_gate(voiceid::current_turn_gate());
            match episodic::record_episode(
                cfg,
                memory,
                &outcome.namespace,
                &text,
                &outcome.response,
                &class.intent,
                transient,
                voice,
            )
            .await
            {
                Ok(recorded) => telemetry::emit(
                    "system",
                    "episodic.recorded",
                    json!({"recorded": recorded, "agent": outcome.agent}),
                ),
                Err(e) => warn!(error = %e, "failed to record episode"),
            }

            // OPTIMIZER TRACE (round: WireOptimizer). Record THIS completed turn
            // as a redacted optimizer trace — GATED by exactly the same posture as
            // the transcript/episodic/learning loop above: a NO-OP unless
            // [optimize].enabled (ships ON) AND the turn is NOT a transient
            // screen read (a screen read can carry on-screen secrets and must
            // never seed the corpus, mirroring the episodic gate). The mode is the
            // selector's pure classification of this utterance (no I/O); the agent
            // + intent are the live routing decision. CROSS-TURN CORRECTION
            // LABELING: BEFORE recording this turn, if it corrected the PRIOR
            // turn's routing (same intent re-aimed to a different agent with an
            // explicit redirect cue — optimize::is_correction), re-label the prior
            // trace Corrected (the learnable signal). The recorder + labeler are
            // pure no-ops when disabled, so the shipped-OFF default does nothing.
            if cfg.optimize.enabled && !transient {
                let mode = selector::classify_mode(&text, &agents::LexicalAgentScorer)
                    .mode()
                    .map(|m| m.as_str())
                    .unwrap_or("clarify");
                // Cross-turn correction: did THIS turn correct the prior route?
                if let Some(prior) = prior_turn.as_ref() {
                    if optimize::is_correction(prior, &class.intent, &outcome.agent, &text) {
                        match trace_store
                            .label_outcome(prior.trace_id, optimize::Outcome::CorrectedNextTurn)
                            .await
                        {
                            Ok(n) => telemetry::emit(
                                "system",
                                "optimize.trace_corrected",
                                json!({"relabeled": n}),
                            ),
                            Err(e) => warn!(error = %e, "optimize: failed to label prior trace corrected"),
                        }
                    }
                }
                // Record this turn (default outcome Success; a future turn may
                // re-label it Corrected). The redacted utterance is built inside
                // record_trace; the returned id seeds the next turn's correction
                // check.
                let ts = SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                match optimize::record_trace(
                    cfg,
                    trace_store,
                    &text,
                    &class.intent,
                    &outcome.agent,
                    mode,
                    "", // tool/skill is not carried on RouteOutcome; honestly empty
                    optimize::Outcome::Success,
                    total_ms,
                    ts,
                )
                .await
                {
                    Ok(Some(id)) => {
                        *prior_turn = Some(optimize::PriorTurn {
                            trace_id: id,
                            intent: class.intent.clone(),
                            agent: outcome.agent.clone(),
                        });
                    }
                    Ok(None) => {} // disabled mid-flight: no trace, leave prior as-is
                    Err(e) => warn!(error = %e, "optimize: failed to record turn trace"),
                }
            }

            // EVAL latency: record this turn's MEASURED pipeline timing + the
            // end-to-end total into the rolling window (eval.rs). Pure timing —
            // no utterance, no content — so it is recorded for EVERY completed
            // turn (not gated by optimize.enabled and safe on transient turns).
            // The periodic eval_report_task reads these aggregates; the cost
            // window stays empty until a turn surfaces cloud token usage
            // (runtime-gated on real cloud calls).
            eval_state.lock().await.record_latency(timing, total_ms);

            // Proactive learning: the reply completed, so the away-gap clock
            // restarts now (meta.last_interaction, unix seconds).
            proactive::record_interaction(memory).await;
            telemetry::emit(
                "system",
                "pipeline.completed",
                json!({
                    "queue_ms": timing.queue_ms,
                    "stt_ms": timing.stt_ms,
                    "classify_ms": timing.classify_ms,
                    "route_ms": timing.route_ms,
                    "first_audio_ms": timing.first_audio_ms,
                    "speak_ms": timing.speak_ms,
                    "total_ms": total_ms,
                }),
            );
            info!(
                "pipeline '{}' queue={} stt={} classify={} route={} first_audio={} speak={} total={}",
                text,
                fmt_ms(timing.queue_ms),
                fmt_ms(timing.stt_ms),
                fmt_ms(timing.classify_ms),
                fmt_ms(timing.route_ms),
                timing
                    .first_audio_ms
                    .map(fmt_ms)
                    .unwrap_or_else(|| "n/a".to_string()),
                fmt_ms(timing.speak_ms),
                fmt_ms(total_ms),
            );
            // Hand this spoken reply back so the next pipeline can reject a
            // captured echo of it (RC-5). Empty replies leave the prior
            // reference untouched (filtered in the main loop).
            Some(outcome.response)
        }
        Err(e) => {
            // error!: recurring hard failure; feeds the self-heal
            // burst detector (see the transcription arm).
            error!(error = %e, "routing failed");
            telemetry::emit(
                "system",
                "route.failed",
                json!({"intent": class.intent, "error": e.to_string()}),
            );
            discard_wav(&wav);
            reply.abandon("route.failed").await;
            None
        }
    }
}

/// MACRO REPLAY (#27): re-run a saved macro's recorded commands, ONE AT A TIME,
/// through the FULL classify -> route -> gate pipeline — exactly as if the user
/// spoke each command live. Returns the spoken summary.
///
/// SAFETY: there is NO shortcut here. Each recorded utterance is re-classified and
/// re-routed by the SAME `router::route` a live utterance uses, so a consequential
/// recorded step PARKS for a spoken confirmation (master ON) or only previews
/// (master OFF) FRESH — replay carries no pre-approval and never batches past the
/// gate. A recorded step is the WORDS only; the gate decides afresh each time. OFF
/// by default ([macros].enabled): with it off this runs nothing and says so.
#[allow(clippy::too_many_arguments)]
async fn replay_macro_live(
    name: &str,
    cfg: &Config,
    memory: &Memory,
    infer: &mut InferenceClient,
    started: Instant,
    reply: &mut speech::ReplySession,
    brief: Option<&str>,
    app_registry: &Arc<apps::AppRegistry>,
    agents: &Arc<agents::AgentRegistry>,
    cloud_reachable: bool,
    root: &Path,
) -> String {
    if !cfg.macros.enabled {
        telemetry::emit("system", "macro.blocked", json!({"reason": "disabled"}));
        let msg = "Macros are off ([macros].enabled = false), sir — there's nothing to replay.";
        let _ = speech::speak(msg, infer, cfg, started, reply).await;
        return msg.to_string();
    }
    let m = match macros::load(memory, name).await {
        Ok(Some(m)) => m,
        Ok(None) => {
            let msg = format!("I have no macro called \"{name}\" to replay, sir.");
            let _ = speech::speak(&msg, infer, cfg, started, reply).await;
            return msg;
        }
        Err(e) => {
            warn!(error = %e, "macro replay could not read the store");
            let msg = "I couldn't read that macro just now, sir.".to_string();
            let _ = speech::speak(&msg, infer, cfg, started, reply).await;
            return msg;
        }
    };
    let step_count = m.steps.len();
    telemetry::emit(
        "system",
        "macro.replay_started",
        json!({"name": m.name, "steps": step_count}),
    );

    // The LIVE router seam: a `MacroRouter` that re-classifies + re-routes each
    // recorded utterance through the SAME `router::route` pipeline a live command
    // hits. The mutable turn context (`infer`, `reply`) lives behind a per-replay
    // async Mutex so the trait's `&self` method can borrow it one step at a time —
    // replay never batches, so each step (consequential or not) re-hits the gate
    // fresh. This is the SAME `macros::replay` the hermetic test drives with a mock,
    // so the gate-honoring property is exercised both live and in test.
    struct LiveMacroRouter<'a> {
        ctx: tokio::sync::Mutex<LiveCtx<'a>>,
        cfg: &'a Config,
        memory: &'a Memory,
        brief: Option<&'a str>,
        app_registry: &'a Arc<apps::AppRegistry>,
        agents: &'a Arc<agents::AgentRegistry>,
        cloud_reachable: bool,
        root: &'a Path,
        started: Instant,
    }
    struct LiveCtx<'a> {
        infer: &'a mut InferenceClient,
        reply: &'a mut speech::ReplySession,
    }
    impl macros::MacroRouter for LiveMacroRouter<'_> {
        fn route_once<'b>(
            &'b self,
            utterance: &'b str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'b>> {
            Box::pin(async move {
                let mut guard = self.ctx.lock().await;
                // Split the two &mut borrows so route() can take both at once.
                let LiveCtx { infer, reply } = &mut *guard;
                let class = match infer.classify(utterance).await {
                    Ok(c) => c,
                    Err(e) => return format!("couldn't classify \"{utterance}\" ({e})"),
                };
                telemetry::emit(
                    "system",
                    "macro.replay_step",
                    json!({"intent": class.intent, "utterance": utterance}),
                );
                // Re-route through the FULL router path: a consequential step re-hits
                // the confirmation gate + master switch exactly as if spoken live.
                match router::route(
                    &class,
                    utterance,
                    self.cfg,
                    self.memory,
                    infer,
                    self.started,
                    reply,
                    self.brief,
                    self.app_registry,
                    self.agents,
                    self.cloud_reachable,
                    self.root,
                )
                .await
                {
                    Ok(o) => o.response,
                    Err(e) => format!("step failed: {e}"),
                }
            })
        }
    }

    let live = LiveMacroRouter {
        ctx: tokio::sync::Mutex::new(LiveCtx { infer, reply }),
        cfg,
        memory,
        brief,
        app_registry,
        agents,
        cloud_reachable,
        root,
        started,
    };
    // The SAME macros::replay the hermetic test drives — re-route each step one at a
    // time through the gate-honoring seam.
    let steps = macros::replay(memory, name, &live).await.unwrap_or_default();

    let mut summary = format!("Replayed macro \"{}\" ({step_count} steps), sir.\n", m.name);
    for s in &steps {
        summary.push_str(&format!("- {}\n", s.outcome.trim()));
    }
    telemetry::emit("system", "macro.replay_done", json!({"name": m.name}));
    let summary = summary.trim_end().to_string();
    // Reclaim the borrows from the seam, then speak the combined summary once.
    let LiveCtx { infer, reply } = live.ctx.into_inner();
    let _ = speech::speak(&summary, infer, cfg, started, reply).await;
    summary
}

/// RAII guard that clears the per-turn voice-id gate when `run_pipeline` returns
/// by ANY path (every early return drops it). Without this, a turn that installs
/// `verified=true` and then a LATER turn that skips verification (voice-id off,
/// or an enroll/forget turn) would otherwise inherit the stale verified flag.
struct TurnGateGuard;
impl Drop for TurnGateGuard {
    fn drop(&mut self) {
        voiceid::clear_turn_gate();
    }
}

/// VOICE-ID per-turn handling (round G). Returns `Some(ack)` when this utterance
/// was an enroll/forget intent (or fed an in-progress enrollment) and the turn is
/// DONE (the caller speaks the ack and returns — no routing). Returns `None` for
/// an ordinary utterance, having FIRST installed the per-turn owner gate from the
/// verification result and emitted the secret-free `voiceid.verify` telemetry.
///
/// Only ever called when `[voice_id].enabled`. ENROLLMENT IS EXPLICIT: an
/// ordinary utterance never enrolls. The profile and enrollment session are owned
/// by the turn loop and threaded by `&mut`, so an enroll persists across turns.
fn handle_voice_id(
    text: &str,
    embedding: Option<&[f32]>,
    root: &Path,
    cfg: &Arc<Config>,
    owner_profile: &mut Option<voiceid::OwnerProfile>,
    enrollment: &mut Option<voiceid::Enrollment>,
) -> Option<String> {
    // 1. EXPLICIT intents first. A "forget" clears the profile + any session;
    //    an "enroll" starts (or restarts) a capture session.
    match voiceid::classify_intent(text) {
        Some(voiceid::VoiceIntent::Forget) => {
            *enrollment = None;
            let had = owner_profile.as_ref().is_some_and(|p| p.is_enrolled());
            *owner_profile = None;
            // Route the forget through the SAME store the profile lives in: the
            // encrypted vault when a master key is installed, else the plaintext
            // owner.json. (We delete BOTH defensively so a forget always leaves no
            // residue regardless of which mode last wrote — harmless when one is
            // absent.) Security finding #2.
            let del = match crypto::global_key() {
                Some(_) => {
                    let v = voiceid::delete_vault(root);
                    let _ = voiceid::delete_profile(root);
                    v
                }
                None => voiceid::delete_profile(root),
            };
            if let Err(e) = del {
                warn!(error = %e, "voice-id: failed to delete the owner profile");
            }
            telemetry::emit("system", "voiceid.forgot", json!({"had_profile": had}));
            return Some(if had {
                "Done — I've forgotten your voice. Voice recognition is now off until you enroll again.".to_string()
            } else {
                "There was no enrolled voice to forget.".to_string()
            });
        }
        Some(voiceid::VoiceIntent::Enroll) => {
            *enrollment = Some(voiceid::Enrollment::begin(
                cfg.voice_id.min_enroll_samples,
                cfg.voice_id.threshold,
            ));
            telemetry::emit(
                "system",
                "voiceid.enroll_started",
                json!({"need": cfg.voice_id.min_enroll_samples}),
            );
            return Some(format!(
                "Let's enroll your voice. Say {} short phrases and I'll learn how you sound. \
                 This is a lightweight on-device match, not a high-assurance lock — it raises the \
                 bar but can be fooled by a recording or a good impression.",
                cfg.voice_id.min_enroll_samples
            ));
        }
        None => {}
    }

    // 2. An enrollment is IN PROGRESS: this utterance is a capture sample. Feed
    //    its embedding; on the final sample, persist the profile. A no-audio
    //    capture (None embedding) is skipped with a re-prompt — we never enroll a
    //    degenerate vector.
    if let Some(session) = enrollment.as_mut() {
        let Some(emb) = embedding else {
            return Some("I didn't catch that clearly — say another short phrase to enroll.".to_string());
        };
        match session.feed(emb.to_vec()) {
            voiceid::EnrollStep::Progress { captured, need } => {
                telemetry::emit(
                    "system",
                    "voiceid.enroll_progress",
                    json!({"captured": captured, "need": need}),
                );
                return Some(format!("Got it ({captured} captured). {need} more to go — keep talking."));
            }
            voiceid::EnrollStep::Complete(profile) => {
                // Persist into the SAME store the boot read uses: the ENCRYPTED
                // vault when a master key is installed (so a re-enrollment never
                // writes a fresh PLAINTEXT owner.json at rest), else the plaintext
                // owner.json — today's behavior with encryption OFF. Security
                // finding #2.
                let saved = match crypto::global_key() {
                    Some(key) => voiceid::save_profile_encrypted(root, &profile, &key),
                    None => voiceid::save_profile(root, &profile),
                };
                if let Err(e) = saved {
                    warn!(error = %e, "voice-id: failed to persist the owner profile");
                    *enrollment = None;
                    return Some("I captured your voice but couldn't save the profile to disk. Voice recognition stays off.".to_string());
                }
                let n = profile.n_samples;
                *owner_profile = Some(profile);
                *enrollment = None;
                telemetry::emit("system", "voiceid.enrolled", json!({"n_samples": n}));
                return Some(format!(
                    "Your voice is enrolled ({n} samples). With voice-id enabled, outward actions \
                     and confirmations now check that it's you — a layer on top of the \
                     confirmation gate, not a replacement."
                ));
            }
        }
    }

    // 3. ORDINARY utterance: verify against the profile (if enrolled) and install
    //    the per-turn gate. UNENROLLED -> the OFF gate (no gating; unchanged
    //    behavior). ENROLLED -> verify; a missing embedding (no usable audio /
    //    embed error) is FAIL-CLOSED: verified=false (an unverified speaker for
    //    the consequential path), but ordinary replies still flow.
    let enrolled = owner_profile.as_ref().is_some_and(|p| p.is_enrolled());
    let (gate, outcome) = if let (true, Some(profile)) = (enrolled, owner_profile.as_ref()) {
        let outcome = match embedding {
            Some(emb) => profile.verify(emb),
            // Fail-closed: no usable audio while enforcing -> unverified.
            None => voiceid::VerifyOutcome { verified: false, score: 0.0 },
        };
        let gate = voiceid::OwnerGate {
            enforcing: true,
            verified: outcome.verified,
            scope: voiceid::GateScope::from_config(&cfg.voice_id.gate_scope),
        };
        (gate, outcome)
    } else {
        // Enabled but UNENROLLED: nothing to verify against -> OFF gate.
        (voiceid::OwnerGate::OFF, voiceid::VerifyOutcome { verified: false, score: 0.0 })
    };
    voiceid::set_turn_gate(gate);
    telemetry::emit(
        "system",
        "voiceid.verify",
        voiceid::verify_telemetry(outcome, cfg.voice_id.enabled, enrolled),
    );
    None
}

/// VOICE CLONING per-turn handling (build 2/2). Returns `Some(ack)` when this
/// utterance was a clone PROPOSAL, a confirmation of a pending clone, or a
/// forget-clone intent (the caller speaks the ack and returns — no routing).
/// Returns `None` for an ordinary utterance.
///
/// CONSENT-GATED and AUTHORIZATION-BOUND, never automatic:
///   1. "clone my voice" -> select a CONFINED owner sample (default search inside the
///      root), PARK a [`CloneState::Pending`], and ask the user to confirm. NOTHING
///      leaves the device yet.
///   2. The NEXT turn, IF a clone is pending: a clear "yes" performs the upload via
///      the inference clone seam (the SAMPLE leaves the device to ElevenLabs), stores
///      the returned voice id in `state/voice/cloned.json`, and reports honestly; any
///      non-affirmative CANCELS the pending clone (audio never leaves the device).
///   3. "forget my voice clone" -> drop the stored id (the agent falls back to its
///      config/Kokoro voice).
///
/// HONESTY: the resolved ElevenLabs key is read ONLY to thread into the clone request
/// body (server -> xi-api-key header); it is never logged/argv/telemetry. On any
/// clone failure (no key / network / quota) the user keeps Kokoro / their existing
/// voice — nothing is silently changed. The consent prompt says plainly that the
/// audio sample leaves the device and that cloning requires authorization.
async fn handle_voice_clone(
    text: &str,
    root: &Path,
    clone_state: &mut voiceclone::CloneState,
    cloned_voices: &mut voiceclone::ClonedVoices,
    infer: &mut InferenceClient,
) -> Option<String> {
    // (1) A clone is PENDING: this turn either confirms it (upload) or cancels it.
    //     Take the pending state out first so any path below leaves it Idle.
    if let voiceclone::CloneState::Pending { sample, agent } =
        std::mem::replace(clone_state, voiceclone::CloneState::Idle)
    {
        if !voiceclone::is_confirmation(text) {
            // Fail-safe: anything that is not a clear yes cancels — the audio never
            // leaves the device.
            telemetry::emit("system", "voiceclone.cancelled", json!({"agent": agent}));
            return Some(
                "Cancelled — I won't clone your voice. Nothing left the device.".to_string(),
            );
        }
        // CONFIRMED: resolve the key and perform the upload via the inference clone
        // seam. The key rides ONLY the request body; we read it here, pass it, drop
        // it. With no key the clone fails cleanly and the user keeps their voice.
        let key = crate::integrations::resolve_secret(voice_tier::ELEVENLABS_ACCOUNT).await;
        let Some(key) = key else {
            telemetry::emit("system", "voiceclone.no_key", json!({"agent": agent}));
            return Some(
                "I can't clone your voice without an ElevenLabs key — add one in Settings. \
                 Nothing was uploaded; you keep your on-device voice."
                    .to_string(),
            );
        };
        let name = voiceclone::clone_display_name(&agent);
        match infer.clone_voice(&sample, &name, &key).await {
            Ok(voice_id) => {
                cloned_voices.set(&agent, &voice_id);
                if let Err(e) = voiceclone::save_clones(root, cloned_voices) {
                    warn!(error = %e, "voice-clone: failed to persist the cloned voice id");
                    // The id is still in memory for this session; report honestly.
                    return Some(
                        "Your voice was cloned, but I couldn't save it to disk — it'll work \
                         this session and you may need to re-clone later."
                            .to_string(),
                    );
                }
                // Telemetry: the AGENT slot only — NEVER the voice id, never the key.
                telemetry::emit("system", "voiceclone.cloned", json!({"agent": agent}));
                Some(
                    "Your voice is cloned and saved. With the cloud voice tier on, that agent \
                     can now speak in your cloned voice; with it off, on-device Kokoro speaks. \
                     The sample was uploaded to ElevenLabs to make the clone."
                        .to_string(),
                )
            }
            Err(e) => {
                warn!(error = %e, "voice-clone: clone_voice failed");
                telemetry::emit("system", "voiceclone.failed", json!({"agent": agent}));
                Some(
                    "I couldn't clone your voice just now — the cloud clone didn't go through. \
                     Nothing changed; you keep your on-device voice."
                        .to_string(),
                )
            }
        }
    } else {
        // (2) No clone pending: look for an explicit clone/forget-clone intent.
        match voiceclone::classify_intent(text) {
            Some(voiceclone::CloneIntent::Forget) => {
                // Default cloned-owner slot is "jarvis"; forgetting drops it.
                let had = cloned_voices.forget("jarvis");
                if had {
                    if let Err(e) = voiceclone::save_clones(root, cloned_voices) {
                        warn!(error = %e, "voice-clone: failed to persist after forget");
                    }
                }
                telemetry::emit("system", "voiceclone.forgot", json!({"had_clone": had}));
                Some(if had {
                    "Done — I've forgotten your cloned voice. That agent falls back to its \
                     on-device voice."
                        .to_string()
                } else {
                    "There was no cloned voice to forget.".to_string()
                })
            }
            Some(voiceclone::CloneIntent::Clone) => {
                // PROPOSE a clone: select a confined owner sample and PARK for an
                // explicit confirmation. NOTHING leaves the device here.
                let Some(sample) = voiceclone::default_owner_sample(root) else {
                    return Some(
                        "I don't have an authorized voice sample to clone from. Enroll your \
                         voice first (or add a sample), then ask me to clone it."
                            .to_string(),
                    );
                };
                let display = sample
                    .strip_prefix(root)
                    .unwrap_or(&sample)
                    .display()
                    .to_string();
                *clone_state = voiceclone::CloneState::Pending {
                    sample,
                    agent: "jarvis".to_string(),
                };
                telemetry::emit("system", "voiceclone.proposed", json!({"agent": "jarvis"}));
                Some(voiceclone::consent_prompt(&display))
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fmt_ms, is_self_echo, is_stale_wait, RotatingLogWriter, STALE_UTTERANCE_WAIT};
    use std::io::Write;
    use std::time::Duration;

    /// RC-5 self-echo / plausibility reject. A one-word fragment is dropped; a
    /// transcript wholly contained in JARVIS's last reply is dropped; a genuine
    /// new multi-word command (not contained in the last reply) passes.
    #[test]
    fn self_echo_rejects_fragments_and_echoes_but_passes_real_commands() {
        // Single-token fragment (an echo shard) — rejected regardless of context.
        assert!(is_self_echo("apple", None));
        assert!(is_self_echo("apple", Some("Opened apple.com in Safari.")));

        // Wholly contained in JARVIS's last reply -> he re-heard himself.
        let reply = "Opened apple.com in Safari, the default browser.";
        assert!(is_self_echo("apple com", Some(reply)), "echo of the spoken URL");
        assert!(is_self_echo("opened apple com in safari", Some(reply)));

        // A GENUINE new command whose words are NOT all in the last reply passes.
        assert!(!is_self_echo("open google.com", Some(reply)), "real command must pass");
        assert!(!is_self_echo("what time is it", Some(reply)));
        // Real short commands still pass (>= 2 words, not an echo).
        assert!(!is_self_echo("system status", None));
        assert!(!is_self_echo("open safari", None));

        // No last reply: only the length rule applies.
        assert!(!is_self_echo("open safari", None));
        // Empty last reply is treated as no reference.
        assert!(!is_self_echo("open google now", Some("   ")));
    }

    /// RC-3: clear_barge_in() runs at the TOP of run_pipeline, BEFORE the
    /// staleness gate's early return — so even a stale-discarded utterance (a
    /// barge whose interrupting speech waited out the cloud round trip) leaves
    /// BARGE_IN cleared instead of latched true forever. The ordering is
    /// structural (clear is the first statement in run_pipeline, ahead of the
    /// `is_stale_wait` early return). This pins the precondition the wedge
    /// depended on: the cloud-round-trip wait genuinely IS classified stale, so
    /// before the fix that path returned without ever clearing. The clear's own
    /// effect (BARGE_IN -> false) is covered by speech-layer tests; asserting it
    /// here too would mutate a process-global shared with other tests.
    #[test]
    fn cloud_roundtrip_wait_is_stale_so_clear_must_precede_the_gate() {
        let stale = Some(STALE_UTTERANCE_WAIT + Duration::from_secs(60));
        assert!(is_stale_wait(stale), "the cloud-round-trip wait must be stale");
        // Exactly the boundary is NOT stale (kept), confirming the gate is the
        // one a barge's interrupting utterance can trip on a long round trip.
        assert!(!is_stale_wait(Some(STALE_UTTERANCE_WAIT)));
    }

    /// Audit fix: only a KNOWN wait past the bound discards an utterance —
    /// an unreadable mtime must never eat user speech.
    #[test]
    fn stale_policy_discards_only_known_waits_past_the_bound() {
        assert!(!is_stale_wait(None));
        assert!(!is_stale_wait(Some(Duration::ZERO)));
        assert!(!is_stale_wait(Some(Duration::from_secs(8))));
        assert!(!is_stale_wait(Some(STALE_UTTERANCE_WAIT)), "exactly at the bound is kept");
        assert!(is_stale_wait(Some(STALE_UTTERANCE_WAIT + Duration::from_millis(1))));
        assert!(is_stale_wait(Some(Duration::from_secs(75))), "the queued-behind-cloud case");
    }

    #[test]
    fn fmt_ms_switches_to_seconds_at_one_thousand() {
        assert_eq!(fmt_ms(0), "0ms");
        assert_eq!(fmt_ms(850), "850ms");
        assert_eq!(fmt_ms(999), "999ms");
        assert_eq!(fmt_ms(1200), "1.2s");
    }

    /// Audit fix: daemon.log rotates by size instead of growing forever;
    /// exactly one predecessor (daemon.log.1) is kept.
    #[test]
    fn log_writer_rotates_by_size_and_keeps_one_predecessor() {
        let dir = std::env::temp_dir().join(format!(
            "jarvis-logrotate-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("daemon.log");
        let rotated = dir.join("daemon.log.1");

        let mut writer = RotatingLogWriter::open(path.clone(), 64).unwrap();
        writer.write_all(&[b'a'; 60]).unwrap(); // under the bound: no rotation
        writer.flush().unwrap();
        assert!(!rotated.exists());
        writer.write_all(&[b'b'; 10]).unwrap(); // crosses the bound...
        writer.write_all(&[b'c'; 8]).unwrap(); // ...so THIS write rotates first
        writer.flush().unwrap();
        assert_eq!(std::fs::read(&rotated).unwrap().len(), 70, "pre-rotation bytes moved aside");
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'c'; 8], "fresh file holds only new lines");

        // A second rotation REPLACES daemon.log.1: total footprint stays
        // bounded at ~2x the rotation size.
        writer.write_all(&[b'd'; 80]).unwrap();
        writer.write_all(&[b'e'; 4]).unwrap();
        writer.flush().unwrap();
        assert_eq!(std::fs::read(&rotated).unwrap().len(), 88);
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'e'; 4]);

        // Reopening an existing file picks up its size (no blind reset).
        drop(writer);
        let mut writer = RotatingLogWriter::open(path.clone(), 64).unwrap();
        writer.write_all(&[b'f'; 70]).unwrap();
        writer.write_all(&[b'g'; 2]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'g'; 2]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Audit fix: the RC-5 self-echo reject must NOT drop the exact replies the
    /// confirmation prompt invites. When a confirmation is parked, the parked
    /// prompt IS the last_reply, so "confirm"/"cancel" (one word) and "confirm
    /// it"/"cancel it" (token-contained in the prompt) would be self-echo-dropped
    /// — discarding the very words the system asked for and (worse) leaving a
    /// "cancel" unhonored while the action stays armed. run_pipeline guards the
    /// reject with `!confirm::is_live(...)`, so with a pending live the composite
    /// gate is false and the utterance reaches the confirmation gate. This pins
    /// that composite predicate exactly as run_pipeline computes it.
    #[test]
    fn parked_confirmation_replies_are_not_self_echo_dropped() {
        use crate::confirm::{self, PendingConfirmation};
        use std::time::Instant;

        // Serialize on the shared PENDING lock: this test mutates the
        // process-global confirmation slot, which other modules' tests also use.
        let _guard = confirm::PENDING_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        // Park a real consequential action; park() returns the SAME spoken prompt
        // that becomes last_reply in run_pipeline (main.rs:916-919).
        let pending = PendingConfirmation {
            agent: "agent.pepper".to_string(),
            tool: "gmail_send".to_string(),
            input: serde_json::json!({"to": "a@b.com", "body": "hi"}),
            allowed: vec!["gmail_send".to_string()],
            preview: "Send an email to a@b.com.".to_string(),
            created_at: Instant::now(),
            id: String::new(),
        };
        let prompt = confirm::park(pending);
        let last_reply = Some(prompt.as_str());

        // Sanity: the parked prompt is genuinely the kind of reply that WOULD be
        // self-echo-dropped if the guard were absent (proves the bug was real).
        assert!(
            is_self_echo("confirm", last_reply),
            "without the guard, the one-word 'confirm' is self-echo-dropped"
        );
        assert!(
            is_self_echo("cancel it", last_reply),
            "without the guard, 'cancel it' is token-contained and dropped"
        );

        // The composite gate run_pipeline actually evaluates: with a pending
        // live, is_live() is true so the reject is SKIPPED — none of the invited
        // replies are dropped, so they reach confirm::take_live + classify.
        let dropped = |text: &str| {
            !confirm::is_live(Instant::now()) && is_self_echo(text, last_reply)
        };
        assert!(!dropped("confirm"), "'confirm' must reach the gate");
        assert!(!dropped("cancel"), "'cancel' must reach the gate");
        assert!(!dropped("confirm it"), "'confirm it' must reach the gate");
        assert!(!dropped("cancel it"), "'cancel it' must reach the gate");

        // Clean up the global slot so no later test sees a stray pending.
        confirm::clear();
        assert!(!confirm::is_live(Instant::now()), "slot cleared after the test");
    }
}
