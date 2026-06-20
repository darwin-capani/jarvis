use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use serde_json::json;
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::warn;

use crate::config::Config;
use crate::inference::{InferenceClient, SentenceEvent};
use crate::playback;
use crate::telemetry;

/// Nonzero while JARVIS is speaking. The audio capture loop checks this and
/// discards mic input for the duration, so the daemon never transcribes its
/// own voice and feeds back into itself.
///
/// A reference COUNT, not a flag (audit fix): a short-lived opener guard and
/// a content guard — possibly from two overlapping replies — must coexist;
/// with a plain bool, whichever guard dropped first unmuted the mic while
/// the other reply was still speaking.
static SPEAKING: AtomicUsize = AtomicUsize::new(0);

pub fn is_speaking() -> bool {
    SPEAKING.load(Ordering::Relaxed) > 0
}

/// Set true when the user BARGES IN — speaks over JARVIS to cut him off. The
/// audio capture loop sets it (via [`request_barge_in`]) the instant it detects
/// the user talking over playback; the reply loops below check it and stop
/// synthesizing further sentences, and it keeps the mic-drop gate OPEN so the
/// user's interrupting utterance is captured instead of dropped. Cleared at the
/// start of the next turn ([`clear_barge_in`]).
static BARGE_IN: AtomicBool = AtomicBool::new(false);

/// Whether the user has barged in over the current reply.
pub fn barge_in_requested() -> bool {
    BARGE_IN.load(Ordering::Relaxed)
}

/// Cut JARVIS off NOW: flag the barge-in, stop the audio mid-clip, and halt any
/// roll-call in progress. The reply loops see the flag and stop; the capture
/// loop stops dropping so it can hear the user out. Called from the audio thread.
pub fn request_barge_in() {
    BARGE_IN.store(true, Ordering::Relaxed);
    playback::cancel_all();
    crate::router::interrupt_roll_call();
    // A barge-in shares this interrupt lifecycle: cutting JARVIS off mid-reply
    // also DROPS any action awaiting a spoken "yes" (the parked confirmation),
    // so an interrupted turn never leaves a consequential action armed for a
    // later, unrelated affirmation to trigger.
    crate::confirm::clear();
}

/// Reset the barge-in flag at the start of a new turn (the next utterance), so a
/// past interruption never suppresses the next reply. Also clears the roll-call
/// cancel flag (RC-9) so both interrupt flags share one lifecycle — a barge over
/// a non-roll-call reply can no longer leave ROLL_CALL_CANCEL latched true and
/// abort the next roll-call before its first agent.
pub fn clear_barge_in() {
    BARGE_IN.store(false, Ordering::Relaxed);
    crate::router::clear_roll_call_interrupt();
}

/// RAII guard for the SPEAKING refcount: decremented on drop, so an early
/// return, timeout, or future refactor into a spawned task can never leave
/// the mic muted forever.
#[derive(Debug)]
struct SpeakingGuard;

impl SpeakingGuard {
    fn engage() -> Self {
        SPEAKING.fetch_add(1, Ordering::Relaxed);
        SpeakingGuard
    }
}

impl Drop for SpeakingGuard {
    fn drop(&mut self) {
        SPEAKING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Floor for playback timeouts: afplay startup, device wakeup, margin.
const PLAYBACK_MARGIN: Duration = Duration::from_secs(10);
/// Mic-mute span for an opener whose WAV header would not parse — openers
/// are short canned clips, so a generous couple of seconds covers them.
const OPENER_FALLBACK_LEN: Duration = Duration::from_secs(3);
/// Echo-mute tail after a reply finishes: room echo outlives the speaker.
const MUTE_TAIL: Duration = Duration::from_millis(400);
/// Poll cadence while watching for a barge during a fallback (afplay/say) clip
/// (RC-4): cancel_all() cannot reach a process-spawned child, so the reply loop
/// races delivery against this poll and DROPS the play future on a barge —
/// play_wav/say_fallback set kill_on_drop(true), so the child is reaped at once.
const BARGE_POLL: Duration = Duration::from_millis(25);
/// Suppression window: no opener within this span of the previous one
/// (rapid follow-ups would otherwise get acknowledgement spam).
const OPENER_SUPPRESS_MS: u64 = 6_000;

/// Unix-ms when the last opener fired (0 = never), for the 6s suppression
/// window across replies.
static LAST_OPENER_FIRE_MS: AtomicU64 = AtomicU64::new(0);
/// Filename index of the previous opener, so consecutive replies never open
/// with the same line. usize::MAX = none played yet.
static LAST_OPENER_INDEX: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Timing for one spoken reply, everything relative to utterance pickup.
#[derive(Debug, Clone, Copy)]
pub struct SpeakReport {
    /// Pickup -> first audio: the opener append when one fired, else the
    /// first content clip (first sink append / afplay spawn). None when
    /// nothing became audible.
    pub first_audio_ms: Option<u64>,
    /// First audio -> playback drained, excluding the 400ms mute tail.
    pub speak_ms: u64,
    /// Pickup -> playback drained: the pipeline total.
    pub total_ms: u64,
}

fn build_report(pipeline_started: Instant, first_audio: Option<Instant>, end: Instant) -> SpeakReport {
    SpeakReport {
        first_audio_ms: first_audio.map(|t| t.duration_since(pipeline_started).as_millis() as u64),
        speak_ms: first_audio
            .map(|t| end.duration_since(t).as_millis() as u64)
            .unwrap_or(0),
        total_ms: end.duration_since(pipeline_started).as_millis() as u64,
    }
}

fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, y) => x.or(y),
    }
}

/// An instant acknowledgment that already went out aloud for this reply.
#[derive(Debug)]
struct OpenerFired {
    /// The configured [speech].openers entry for the WAV's filename index;
    /// None when the file on disk has no matching entry (stale dir) — the
    /// WAV still plays but no opener_spoken hint goes to the server.
    text: Option<String>,
}

/// One spoken reply, owned from utterance receipt to the end of playback:
/// the SPEAKING mic-mute guard, the gapless playback session, the optional
/// instant opener, and the afplay/say fallback state. The event loop creates
/// it the moment the VAD hands over an utterance — the opener fires BEFORE
/// transcription — and carries it through STT/classify/route so converse
/// content, cloud sentences, and every fallback rung append to the SAME
/// session/sink the opener started.
#[derive(Debug)]
pub struct ReplySession {
    /// The CONTENT guard: engaged when reply audio starts (ensure_guard);
    /// released after the echo tail in complete()/abandon(). The opener
    /// holds its own short-lived guard, timed to its clip, so the mic is
    /// live during long silent waits (cloud round trips) between the two.
    /// The Drop impl backstops every other exit path.
    guard: Option<SpeakingGuard>,
    session: playback::Session,
    /// False until rodio fails; afplay then carries the rest of the reply.
    fellback: bool,
    /// First process-spawned audio start (afplay/say), for first_audio when
    /// the sink never played.
    proc_first: Option<Instant>,
    /// Pure silence between consecutive clips ([speech].sentence_pause_ms).
    pause: Duration,
    opener: Option<OpenerFired>,
}

impl ReplySession {
    /// Begin the reply at utterance receipt.
    ///
    /// Default ([speech].instant_opener = false): NO canned opener fires and
    /// NO breath is taken — the session stays cold and the guard engages only
    /// when actual content audio starts (ensure_guard). The converse stream
    /// is then the whole reply, so the persona greets/answers naturally from
    /// its first word instead of leading with a programmed task-ack. This is
    /// exactly the pre-opener behavior, and `opener_text()` stays None so
    /// converse never receives an `opener_spoken` hint.
    ///
    /// When instant_opener = true the prior behavior holds unchanged: fire an
    /// instant opener when allowed (suppression window clear and
    /// state/openers/ has WAVs), else stay cold. Opener breath: the
    /// acknowledgment waits [speech].opener_delay_ms so it lands a natural
    /// beat after the user stops talking. The caller MUST run this future
    /// concurrently with transcription (tokio::join!) — the delay never
    /// serializes in front of STT. first_audio_ms includes the delay
    /// naturally because the pipeline clock starts at pickup.
    pub async fn begin(root: &Path, cfg: &Config) -> Self {
        let mut reply = Self {
            guard: None,
            session: playback::Session::new(),
            fellback: false,
            proc_first: None,
            pause: Duration::from_millis(cfg.speech.sentence_pause_ms),
            opener: None,
        };
        if !cfg.speech.instant_opener {
            // Canned opener gated off: no breath, no acknowledgment. The
            // mic-mute/SPEAKING guard still engages the moment content audio
            // starts (ensure_guard), so nothing downstream regresses.
            return reply;
        }
        let breath = Duration::from_millis(cfg.speech.opener_delay_ms);
        if !breath.is_zero() {
            tokio::time::sleep(breath).await;
        }
        reply
            .try_opener(&root.join("state").join("openers"), cfg)
            .await;
        reply
    }

    /// The opener text already played for this reply, if a configured one
    /// fired — passed to converse as opener_spoken so the LLM continues
    /// from it instead of acknowledging twice.
    pub fn opener_text(&self) -> Option<&str> {
        self.opener.as_ref().and_then(|o| o.text.as_deref())
    }

    async fn try_opener(&mut self, dir: &Path, cfg: &Config) {
        if opener_suppressed(unix_ms(), LAST_OPENER_FIRE_MS.load(Ordering::Relaxed)) {
            return;
        }
        let files = opener_files(dir);
        if files.is_empty() {
            // Missing/unreadable/empty dir: openers stay dormant and the
            // reply behaves exactly as before the feature existed.
            return;
        }
        let last_pos = {
            let last_idx = LAST_OPENER_INDEX.load(Ordering::Relaxed);
            files.iter().position(|(idx, _)| *idx == last_idx)
        };
        let (idx, path) = &files[pick_position(files.len(), last_pos, entropy())];
        // Openers are reusable assets synthesized at server startup — read,
        // never delete.
        let bytes = match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to read opener wav; skipping opener");
                return;
            }
        };
        // Mute the mic before the speaker makes a sound.
        let opener_guard = SpeakingGuard::engage();
        let opener_len = playback::wav_duration(&bytes).unwrap_or(OPENER_FALLBACK_LEN);
        if !self.session.append(bytes).await {
            warn!("rodio rejected the opener; reply continues unacknowledged");
            return; // opener_guard drops here -> mic live again
        }
        // Audit fix: the mic comes back once the opener clip has drained
        // (plus the echo tail), NOT at reply end — a cloud-routed reply can
        // sit silent for the better part of two minutes, and holding the
        // guard through that wait silently destroyed everything the user
        // said ("cancel that", corrections). The SPEAKING refcount lets this
        // short-lived guard coexist with the content guard that
        // ensure_guard() engages when actual reply audio starts.
        tokio::spawn(async move {
            tokio::time::sleep(opener_len + MUTE_TAIL).await;
            drop(opener_guard);
        });
        LAST_OPENER_FIRE_MS.store(unix_ms(), Ordering::Relaxed);
        LAST_OPENER_INDEX.store(*idx, Ordering::Relaxed);
        let text = cfg.speech.openers.get(*idx).cloned();
        telemetry::emit(
            "local",
            "opener.played",
            json!({"index": idx, "text": text}),
        );
        self.opener = Some(OpenerFired { text });
    }

    fn ensure_guard(&mut self) {
        if self.guard.is_none() {
            self.guard = Some(SpeakingGuard::engage());
        }
    }

    /// RC-11 — close the pre-speech capture window of a LOCAL ACTION. Mute the
    /// mic the instant the router commits to actuating a local turn, BEFORE the
    /// mutating handler runs (`open_url`, app launch) and well before any audio
    /// starts. With instant_opener off (the shipped default) the content guard
    /// otherwise engages only when the FIRST reply clip plays — so for a
    /// `web.open` the sequence was: pickup -> STT -> classify -> `/usr/bin/open`
    /// fires (open #1) -> converse setup -> ensure_guard(). Through that whole
    /// 1-2s span `is_speaking()` stayed FALSE, so the capture gate was wide open
    /// and the user's own command (its room reverberation, or a continued word)
    /// was segmented into a fresh utterance, transcribed back to "...apple.com",
    /// and re-routed — opening the URL a second and third time (the live
    /// triple-open). The reply's self-echo reject (RC-5) cannot catch this: it
    /// rejects fragments of JARVIS's *reply*, not a re-capture of the *user's
    /// command*. Muting here removes the window outright.
    ///
    /// Scoped to the LOCAL actuation path on purpose: it is NOT engaged on the
    /// cloud path, so a long silent cloud round trip stays mic-LIVE and the user
    /// can still barge/correct during the wait (the opener-guard audit fix this
    /// must not regress). The content guard is shared via the SPEAKING refcount,
    /// so the later ensure_guard() in converse/speak is an idempotent no-op and
    /// complete()/abandon() releases the single guard after the echo tail — no
    /// double-count, no leak.
    pub fn mute_for_action(&mut self) {
        self.ensure_guard();
    }

    /// Roll-call (router): engage the content guard and append one already-
    /// synthesized intro WAV to the gapless sink, paced like any other content
    /// clip. Returns whether the clip became audible. Public so the
    /// constellation roll-call can stream each agent's self-introduction
    /// through the SAME reply session the opener started.
    pub async fn push_clip(&mut self, wav: &Path) -> bool {
        self.ensure_guard();
        self.deliver(wav).await
    }

    /// Roll-call (router): close the reply and return its timing report, the
    /// same way the converse/cloud paths end — drains the sink, holds the mic
    /// through the echo tail, releases the guard.
    pub async fn finish_report(&mut self, pipeline_started: Instant) -> SpeakReport {
        self.complete(pipeline_started).await
    }

    /// Hand one synthesized WAV to the gapless sink (primary) or afplay
    /// (fallback for the rest of the reply once rodio has failed), inserting
    /// the configured pause beforehand when audio is already queued — after
    /// the opener and between content sentences; nothing ever trails the
    /// last clip. The WAV is read fully into memory and deleted as soon as
    /// the sink owns it; on rodio failure the still-on-disk file feeds
    /// afplay instead, after draining whatever the sink already holds so the
    /// two paths never talk over each other. Returns whether the clip was
    /// queued/played.
    async fn deliver(&mut self, wav: &Path) -> bool {
        if !self.fellback {
            let bytes = match tokio::fs::read(wav).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(path = %wav.display(), error = %e, "failed to read TTS wav; skipping clip");
                    let _ = tokio::fs::remove_file(wav).await;
                    return false;
                }
            };
            if self.session.has_audio() {
                // Sentence pacing: pure silence, generated in memory.
                self.session.append_silence(self.pause).await;
            }
            if self.session.append(bytes).await {
                if let Err(e) = tokio::fs::remove_file(wav).await {
                    warn!(path = %wav.display(), error = %e, "failed to remove TTS wav");
                }
                return true;
            }
            warn!("rodio playback failed; using afplay for the rest of this reply");
            self.fellback = true;
            self.session.finish().await;
        }
        let candidate = Instant::now();
        // RC-4: the afplay child is awaited to completion and cancel_all()
        // cannot reach it, so race it against a barge and drop it (reaping the
        // kill_on_drop child) the instant the user interrupts — otherwise
        // JARVIS keeps talking and the now-open capture gate re-records him.
        let played = match play_unless_barged(play_wav(wav)).await {
            Delivered::Played(played) => played,
            Delivered::Barged => {
                warn!("barge-in: aborting the fallback clip mid-play");
                false
            }
        };
        // Clean up the tmp WAV regardless — a barge-aborted clip leaves the
        // file behind too (play_and_remove's removal no longer runs).
        if let Err(e) = tokio::fs::remove_file(wav).await {
            warn!(path = %wav.display(), error = %e, "failed to remove TTS wav");
        }
        if played {
            self.proc_first.get_or_insert(candidate);
        }
        played
    }

    /// End of a spoken reply: drain the sink, build the timing report, hold
    /// the mic mute through the echo tail, then release the guard.
    async fn complete(&mut self, pipeline_started: Instant) -> SpeakReport {
        self.session.finish().await;
        let first_audio = earliest(self.session.first_append(), self.proc_first);
        let report = build_report(pipeline_started, first_audio, Instant::now());
        if self.guard.is_some() {
            tokio::time::sleep(MUTE_TAIL).await;
            self.guard = None;
        }
        report
    }

    /// The pipeline died before producing content (empty/failed STT, failed
    /// classify or route): drain whatever played — normally just the opener,
    /// which is acceptable to orphan — and release the guard after the echo
    /// tail.
    pub async fn abandon(mut self, reason: &str) {
        if let Some(opener) = &self.opener {
            telemetry::emit(
                "local",
                "opener.orphaned",
                json!({"reason": reason, "text": opener.text}),
            );
        }
        self.session.finish().await;
        if self.guard.is_some() {
            tokio::time::sleep(MUTE_TAIL).await;
            self.guard = None;
        }
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Whether an opener fired recently enough to suppress the next one.
fn opener_suppressed(now_ms: u64, last_fire_ms: u64) -> bool {
    last_fire_ms != 0 && now_ms.saturating_sub(last_fire_ms) < OPENER_SUPPRESS_MS
}

/// Cheap entropy for the uniform opener pick — sub-second clock noise is
/// plenty for choosing among a handful of canned lines (no rand dependency).
fn entropy() -> usize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as usize)
        .unwrap_or(0)
}

/// opener-<idx>.wav files under state/openers/, sorted by index. Empty when
/// the dir is missing or unreadable (the opener feature then stays dormant).
fn opener_files(dir: &Path) -> Vec<(usize, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(usize, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let idx = name
                .strip_prefix("opener-")?
                .strip_suffix(".wav")?
                .parse()
                .ok()?;
            Some((idx, entry.path()))
        })
        .collect();
    files.sort_by_key(|(idx, _)| *idx);
    files
}

/// Uniform pick of a position in 0..n, avoiding `last` whenever n > 1: the
/// draw is over the n-1 other positions, shifted past `last`.
fn pick_position(n: usize, last: Option<usize>, entropy: usize) -> usize {
    match last {
        Some(last) if last < n && n > 1 => {
            let pos = entropy % (n - 1);
            if pos >= last {
                pos + 1
            } else {
                pos
            }
        }
        _ => entropy % n,
    }
}

/// Resolve the TTS backend for `agent_name` (Kokoro voice `kokoro_voice`) AND the
/// ElevenLabs key when the cloud voice tier is selected, then emit one
/// `voice.tier` telemetry line. Returns `(Backend, Option<el_key>)` ready to hand
/// to [`crate::inference::InferenceClient::speak`].
///
/// This is the ONE place the daemon binds the pure tier decision
/// ([`crate::voice_tier::resolve_voice_backend`]) to its runtime inputs: the active
/// model-swap tier ([`crate::model_tier::active_tier`] — a `Local` override/route
/// means "work offline", so voice stays on-device) and key PRESENCE (a Keychain
/// read of the allowlisted `elevenlabs_api_key`).
///
/// SECURITY: the resolved key is fetched here ONLY to thread into the request
/// body; it is returned to the caller, never logged, and the `voice.tier`
/// telemetry carries ONLY {backend, agent} — never the key, never the voice id.
/// On the Kokoro path no key is read at all. When the tier picks ElevenLabs but the
/// key vanished between the presence check and the read (a race), it degrades to
/// Kokoro honestly rather than send a keyless cloud request.
pub async fn resolve_speak_backend(
    cfg: &Config,
    agent_name: &str,
    kokoro_voice: &str,
) -> (crate::voice_tier::Backend, Option<String>) {
    use crate::voice_tier::{Backend, ELEVENLABS_ACCOUNT};

    // Cheap pre-check: only consult the Keychain at all when the master switch is
    // on AND the operator is not offline. This keeps the default (tier OFF) path
    // byte-for-byte today's behavior with ZERO Keychain access.
    let active = crate::model_tier::active_tier(cfg, crate::model_tier::current_override());
    let consider_cloud = cfg.voice.cloud_tier && active != crate::model_tier::Tier::Local;

    // Resolve the key ONLY when the cloud path is even a candidate.
    let key = if consider_cloud {
        crate::integrations::resolve_secret(ELEVENLABS_ACCOUNT).await
    } else {
        None
    };

    let backend = crate::voice_tier::resolve_voice_backend(
        cfg,
        agent_name,
        kokoro_voice,
        active,
        key.is_some(),
    );

    // voice.tier telemetry — {backend, agent} ONLY. NEVER the key/voice id. The
    // HUD indicator reads this to show CLOUD vs ON-DEVICE voice honestly.
    telemetry::emit(
        "local",
        "voice.tier",
        json!({ "backend": backend.as_str(), "agent": agent_name }),
    );

    match backend {
        // Only the gated cloud path carries the key onward.
        Backend::ElevenLabs { .. } => (backend, key),
        Backend::Kokoro { .. } => (backend, None),
    }
}

/// Resolve the STT backend AND the ElevenLabs key when the Scribe cloud-STT tier is
/// selected, then emit one `stt.tier` telemetry line. Returns
/// `(SttBackend, Option<el_key>)` ready to hand to
/// [`crate::inference::InferenceClient::transcribe`].
///
/// This mirrors [`resolve_speak_backend`] but on the SEPARATE `[voice].cloud_stt`
/// switch (build 2/2): it binds the pure tier decision
/// ([`crate::voice_tier::resolve_stt_backend`]) to the active model-swap tier (a
/// `Local` override means "work offline", so STT stays on-device) and key PRESENCE.
///
/// SECURITY: the resolved key is fetched here ONLY to thread into the request body;
/// it is returned to the caller, never logged, and the `stt.tier` telemetry carries
/// ONLY {backend} — never the key. On the whisper path no key is read at all. When
/// the tier picks Scribe but the key vanished between the presence check and the
/// read (a race), it degrades to whisper honestly rather than send a keyless cloud
/// request.
///
/// HONESTY: STT sends the user's VOICE AUDIO to the cloud when Scribe is chosen —
/// MORE sensitive than the TTS text leg. On-device whisper is the private/offline
/// default and the server's fallback on ANY Scribe error.
pub async fn resolve_transcribe_backend(
    cfg: &Config,
) -> (crate::voice_tier::SttBackend, Option<String>) {
    use crate::voice_tier::{SttBackend, ELEVENLABS_ACCOUNT};

    // Cheap pre-check: only consult the Keychain at all when the cloud-STT switch is
    // on AND the operator is not offline. This keeps the default (cloud_stt OFF)
    // path byte-for-byte today's behavior with ZERO Keychain access.
    let active = crate::model_tier::active_tier(cfg, crate::model_tier::current_override());
    let consider_cloud = cfg.voice.cloud_stt && active != crate::model_tier::Tier::Local;

    let key = if consider_cloud {
        crate::integrations::resolve_secret(ELEVENLABS_ACCOUNT).await
    } else {
        None
    };

    let backend = crate::voice_tier::resolve_stt_backend(cfg, active, key.is_some());

    // stt.tier telemetry — {backend} ONLY. NEVER the key. The HUD indicator reads
    // this to show CLOUD vs ON-DEVICE transcription honestly (audio leaving the
    // device is more sensitive than text, so this signal matters).
    telemetry::emit(
        "local",
        "stt.tier",
        json!({ "backend": backend.as_str() }),
    );

    match backend {
        // Only the gated cloud path carries the key onward.
        SttBackend::ElevenLabsScribe { .. } => (backend, key),
        SttBackend::Whisper => (backend, None),
    }
}

/// Speak a response aloud, sentence by sentence (the cloud / fallback path —
/// the local LLM path streams through converse_speak instead). Each sentence
/// goes through the inference server's "speak" op and the resulting WAV is
/// appended to the reply's gapless rodio sink, which plays while the next
/// sentence synthesizes. If rodio fails the remainder of the reply degrades
/// to the old afplay path, and only if no sentence produces audio at all do
/// we fall back to macOS `say`, so the daemon is never mute. Always finishes
/// the reply: drains the sink (opener included) and releases the mic after
/// the echo tail.
pub async fn speak(
    text: &str,
    infer: &mut InferenceClient,
    cfg: &Config,
    pipeline_started: Instant,
    reply: &mut ReplySession,
) -> SpeakReport {
    // The base JARVIS speech path speaks JARVIS's own (English-centric) voice with
    // NO target-language hint — exactly today's behavior. The reply kind defaults to
    // Routine (=> Neutral prosody): the conservative classification for an ordinary
    // spoken reply. The few callers that KNOW the kind (an alert/heal, a wellness
    // reply, a greeting) use `speak_kind` to colour the delivery.
    speak_in_lang(text, None, infer, cfg, pipeline_started, reply, crate::prosody::ReplyKind::Routine).await
}

/// Speak a reply whose CONTEXT (alert/heal vs wellness vs greeting vs routine) is
/// known to the caller, so #33 adaptive prosody can colour the delivery. Identical to
/// [`speak`] except the caller passes the [`crate::prosody::ReplyKind`]; with
/// `[voice].adaptive_prosody` OFF (the default) the kind is ignored and the request is
/// byte-for-byte today's neutral request on every backend. The base [`speak`] is the
/// Routine entry; this is the kind-aware entry for the few callers that know a more
/// specific context (a wellness reply, an alert/heal). The roll-call path shapes
/// Greeting inline (it resolves a per-agent backend), so this convenience entry is
/// exercised by the hermetic test rather than a current single caller.
#[allow(dead_code)] // kind-aware speak entry; exercised by tests, kept for specific-kind callers
pub async fn speak_kind(
    text: &str,
    infer: &mut InferenceClient,
    cfg: &Config,
    pipeline_started: Instant,
    reply: &mut ReplySession,
    kind: crate::prosody::ReplyKind,
) -> SpeakReport {
    speak_in_lang(text, None, infer, cfg, pipeline_started, reply, kind).await
}

/// Speak a response aloud in a specific TARGET LANGUAGE (Babel, build 2/2). Same
/// echo-safe pipeline as [`speak`], but `lang` is threaded to the speak op so the
/// ElevenLabs backend can pick a MULTILINGUAL model (eleven_multilingual_v2 /
/// eleven_v3) for a non-English target instead of the English-centric default.
///
/// `lang = None` is byte-for-byte [`speak`] (ordinary English reply). When the
/// cloud voice tier is OFF this is unchanged from today — on-device Kokoro, which
/// is English-centric (the multilingual quality lift is the EL-backend benefit;
/// Kokoro stays the offline default + fallback). The interpreter (Babel) passes the
/// target language it already knows (`to_lang`).
pub async fn speak_in_lang(
    text: &str,
    lang: Option<&str>,
    infer: &mut InferenceClient,
    cfg: &Config,
    pipeline_started: Instant,
    reply: &mut ReplySession,
    kind: crate::prosody::ReplyKind,
) -> SpeakReport {
    let speakable = clip_for_speech(&normalize_for_speech(text));
    if speakable.is_empty() {
        // Nothing to say — but an opener may already be in the air; close
        // the reply like any other so the guard never leaks.
        return reply.complete(pipeline_started).await;
    }
    reply.ensure_guard();
    telemetry::emit("local", "response.speaking", json!({ "text": speakable }));

    // Resolve the TTS backend ONCE for this reply: this is the base JARVIS speech
    // path (the cloud-LLM / fallback voicing), so it speaks in JARVIS's own voice
    // ([speech].voice). With the cloud voice tier OFF (the shipped default) this is
    // on-device Kokoro with exactly today's voice and ZERO Keychain access; the
    // ElevenLabs branch is reached only when the tier is on + a key is present +
    // the operator is not offline + JARVIS is mapped.
    let (backend, el_key) = resolve_speak_backend(cfg, "jarvis", &cfg.speech.voice).await;
    // Only carry a real, non-empty target language onward (Babel non-English path);
    // an English / absent target leaves the wire exactly as today.
    let lang = lang.filter(|l| !l.trim().is_empty());

    // EXPRESSIVENESS (#33 prosody + #34 whisper). This base JARVIS speech path is
    // NEVER a required-confirmation utterance — a consequential confirmation is
    // previewed + spoken through the router's confirm gate, not here — so
    // required_confirm is false at this site (conservative + correct). The pure
    // expressiveness brain then:
    //   * classify_prosody(kind, false) -> a ProsodyProfile (Neutral for Routine),
    //   * shape_speak_request(cfg, profile, &backend) -> the per-backend shape
    //     (EL-v3 rich surface, or coarse rate on Kokoro/non-v3; Neutral on every
    //     backend when [voice].adaptive_prosody is OFF -> SpeakShape::neutral()),
    //   * apply_whisper(shape, whisper_state_is_on(), false) -> soft+terse delivery
    //     when the process-global whisper state is engaged (never on a required
    //     confirm — false here keeps the guard moot but explicit).
    // With BOTH features OFF (the shipped default) this resolves to the IDENTITY
    // SpeakShape::neutral(), and `infer.speak` then sends a byte-for-byte-today wire.
    let required_confirm = false;
    let profile = crate::prosody::classify_prosody(kind, required_confirm);
    let mut shape = crate::prosody::shape_speak_request(cfg, profile, &backend);
    let whisper_on = crate::prosody::whisper_state_is_on();
    shape = crate::prosody::apply_whisper(shape, whisper_on, required_confirm);
    // Light up the wired HUD `voice.prosody` indicator (secret-free; dropped when no
    // HUD). A neutral shape still emits an honest "neutral" line.
    crate::prosody::emit_telemetry(profile, &backend, &shape, whisper_on);

    // ADDITIVE (Phase-2): the streaming opt-in + pronunciation locator, derived once
    // from [voice]. With the shipped defaults (stream_tts OFF, empty dictionary id)
    // this is SpeakExtras::none() -> the speak wire is byte-for-byte today's.
    let extras = crate::inference::SpeakExtras::from_config(cfg);

    let sentences = split_sentences(&speakable);
    let mut played_any = false;
    for sentence in &sentences {
        // Barge-in: the user spoke over JARVIS — stop here, don't synthesize the
        // rest of the reply (its audio was already cut by request_barge_in).
        if barge_in_requested() {
            warn!("barge-in: halting reply mid-stream");
            break;
        }
        match infer.speak(sentence, &backend, el_key.as_deref(), lang, &shape, &extras).await {
            Ok(wav) => {
                played_any |= reply.deliver(&wav).await;
            }
            Err(e) => {
                warn!(error = %e, text = %sentence, "sentence synthesis failed; skipping it");
            }
        }
    }
    if !played_any {
        warn!("no sentence produced audio; falling back to `say`");
        // Drain anything already queued (the opener) so `say` and the sink
        // never talk over each other.
        reply.session.finish().await;
        let candidate = Instant::now();
        // RC-4: `say` is a spawned child cancel_all() cannot reach — race it
        // against a barge so an interruption drops it (kill_on_drop reaps it).
        if let Delivered::Played(true) = play_unless_barged(say_fallback(&speakable)).await {
            reply.proc_first.get_or_insert(candidate);
        }
    }
    reply.complete(pipeline_started).await
}

/// What converse_speak hands back to the router on success.
pub struct ConverseSpoken {
    /// The full reply text (done.text) — recorded, logged, and fed to the
    /// learning task. On a mid-stream failure: the sentences that played.
    pub response: String,
    /// When the server's done event landed; the router turns this into
    /// route_ms (contract: route_ms for converse = time to the done event).
    pub done_at: Instant,
    pub report: SpeakReport,
}

/// Streamed reply: ONE converse round trip generates and synthesizes the
/// answer sentence-by-sentence, and each WAV is appended to the reply's
/// gapless sink the moment its event arrives — first audio starts while the
/// model is still decoding the rest (and the opener, when one fired, has
/// already covered the wait; its text rides along as opener_spoken so the
/// model never acknowledges twice). No clip_for_speech here: the persona
/// keeps replies short and the server caps synthesis at 5 sentences.
///
/// `voice` and `persona` are the ACTIVE agent's — the selected agent speaks
/// in its own Kokoro voice and persona prefix (router.rs delegation), so the
/// constellation is audible per agent. `persona` is the agent name the server
/// maps to its persona file (None = the server's default persona).
///
/// `local_model` is the multi-resident LOCAL sub-choice (task #17): the warm
/// local model id the Local tier picked for THIS on-device turn (a "local-fast"
/// model vs the capable base). `None` (the single-resident default, and every
/// CLOUD-degraded-to-local fallback) -> the base model. It only matters when the
/// server actually kept >1 model warm (RAM-bounded, OFF by default).
///
/// Returns Err ONLY when no content was played, so the router can fall back
/// to the old generate+speak path and the daemon is never mute; the reply
/// session is left open in that case — the fallback keeps appending to the
/// same sink behind the same opener. A failure after content started keeps
/// what played and reports it as the response.
#[allow(clippy::too_many_arguments)] // mirrors the converse wire request
pub async fn converse_speak(
    text: &str,
    max_tokens: u32,
    history: &[(String, String)],
    facts: &[String],
    data: Option<&str>,
    voice: &str,
    persona: Option<&str>,
    local_model: Option<&str>,
    infer: &mut InferenceClient,
    pipeline_started: Instant,
    reply: &mut ReplySession,
) -> Result<ConverseSpoken> {
    reply.ensure_guard();
    let (tx, mut rx) = mpsc::unbounded_channel::<SentenceEvent>();
    let mut played: Vec<String> = Vec::new();
    let opener_spoken = reply.opener_text().map(str::to_owned);

    let done = {
        let conv = infer.converse(
            text,
            max_tokens,
            history,
            facts,
            data,
            voice,
            opener_spoken.as_deref(),
            persona,
            local_model,
            tx,
        );
        tokio::pin!(conv);
        // Interleave: play sentence events as they stream in while the
        // converse future keeps pumping the socket toward the done event.
        loop {
            tokio::select! {
                res = &mut conv => break res,
                Some(ev) = rx.recv() => {
                    if barge_in_requested() {
                        // Barge-in: the user cut JARVIS off — stop speaking the
                        // rest (its audio was already cancelled); let conv drain.
                        // Best-effort delete the dropped sentence WAV so the
                        // not-yet-delivered tts-*.wav files don't leak forever.
                        let _ = tokio::fs::remove_file(&ev.path).await;
                    } else if reply.deliver(&ev.path).await {
                        played.push(ev.text);
                    } else {
                        warn!(seq = ev.seq, "converse sentence produced no audio; skipped");
                    }
                }
            }
        }
    };
    let done_at = Instant::now();
    // Sentences that landed in the channel while the done line was read.
    while let Ok(ev) = rx.try_recv() {
        if barge_in_requested() {
            // user barged in — drop the rest silently (audio already cut);
            // delete the dropped sentence WAV so it isn't leaked.
            let _ = tokio::fs::remove_file(&ev.path).await;
        } else if reply.deliver(&ev.path).await {
            played.push(ev.text);
        } else {
            warn!(seq = ev.seq, "converse sentence produced no audio; skipped");
        }
    }

    let response = match done {
        Ok(d) if !played.is_empty() => {
            if d.text.trim().is_empty() {
                played.join(" ")
            } else {
                d.text
            }
        }
        Ok(_) => {
            // Generation finished but no content reached the speakers
            // (synthesis or playback failed for every sentence): let the
            // router regenerate via the non-streamed path rather than stay
            // mute. The session stays open for it.
            return Err(anyhow!("converse completed but no sentence produced audio"));
        }
        Err(e) if played.is_empty() => {
            // No content became audible; safe for the router to fall back
            // onto the same still-open session.
            return Err(e);
        }
        Err(e) => {
            warn!(error = %e, played = played.len(), "converse failed mid-stream; keeping the sentences that played");
            played.join(" ")
        }
    };
    telemetry::emit("local", "response.speaking", json!({ "text": response }));

    let report = reply.complete(pipeline_started).await;
    Ok(ConverseSpoken { response, done_at, report })
}

/// Outcome of racing one fallback clip against a barge (RC-4).
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Delivered {
    /// The clip finished on its own; the bool is whether it produced audio.
    Played(bool),
    /// A barge fired mid-clip: the play future was dropped (kill_on_drop reaped
    /// the child). Treated as "did not complete" — the reply loop stops here.
    Barged,
}

/// Run a play future to completion UNLESS a barge fires first. The fallback
/// (afplay/say) path is a spawned child awaited to completion, which
/// `cancel_all()` cannot reach — so when the user barges mid-clip, this drops
/// the future (reaping the kill_on_drop child) instead of letting JARVIS keep
/// talking (and letting the now-open capture gate re-record him). Returns
/// `Barged` the instant a barge is observed; otherwise the play result. The
/// live call passes `barge_in_requested` as the predicate; tests pass their own,
/// so the race logic is verified without touching the process-global flag.
async fn play_unless_barged<F>(play: F) -> Delivered
where
    F: std::future::Future<Output = bool>,
{
    play_unless(play, barge_in_requested).await
}

/// The predicate-injected core of [`play_unless_barged`], so the abort logic is
/// unit-testable with a deterministic `barged` closure (no global flag).
async fn play_unless<F, B>(play: F, barged: B) -> Delivered
where
    F: std::future::Future<Output = bool>,
    B: Fn() -> bool,
{
    // Already barged before we even start: don't spawn the child at all.
    if barged() {
        return Delivered::Barged;
    }
    tokio::pin!(play);
    loop {
        tokio::select! {
            // Bias the poll so a barge is observed promptly even if the play
            // future is also ready; correctness (stopping) beats one last clip.
            biased;
            _ = tokio::time::sleep(BARGE_POLL) => {
                if barged() {
                    // Dropping `play` here reaps the afplay/say child.
                    return Delivered::Barged;
                }
            }
            played = &mut play => return Delivered::Played(played),
        }
    }
}

/// Audio duration of a WAV, for sizing the playback timeout.
fn wav_duration(path: &Path) -> Option<Duration> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    if spec.sample_rate == 0 {
        return None;
    }
    Some(Duration::from_secs_f64(
        reader.duration() as f64 / spec.sample_rate as f64,
    ))
}

/// Plays a WAV via afplay; returns false on any failure so the caller can
/// fall back to `say`. Bounded by the WAV's own duration plus margin —
/// a wedged CoreAudio device must not deadlock the event loop with the
/// SPEAKING flag latched. kill_on_drop reaps the child when the timeout
/// drops the status() future.
async fn play_wav(path: &Path) -> bool {
    let limit = wav_duration(path).unwrap_or(Duration::from_secs(30)) + PLAYBACK_MARGIN;
    let mut cmd = Command::new("/usr/bin/afplay");
    cmd.arg(path).kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.status()).await {
        Ok(Ok(status)) if status.success() => true,
        Ok(Ok(status)) => {
            warn!(%status, "afplay exited non-zero");
            false
        }
        Ok(Err(e)) => {
            warn!(error = %e, "failed to run afplay");
            false
        }
        Err(_) => {
            warn!(limit_s = limit.as_secs(), "afplay timed out; killing it");
            false
        }
    }
}

/// Last-resort TTS; returns whether `say` actually produced audio.
async fn say_fallback(text: &str) -> bool {
    // `say` speaks roughly 12-15 chars/sec; size the bound generously.
    let limit = Duration::from_secs((text.len() / 8) as u64) + PLAYBACK_MARGIN;
    let mut cmd = Command::new("say");
    cmd.arg(text).kill_on_drop(true);
    match tokio::time::timeout(limit, cmd.status()).await {
        Ok(Ok(status)) if status.success() => true,
        Ok(Ok(status)) => {
            warn!(%status, "say exited non-zero");
            false
        }
        Ok(Err(e)) => {
            warn!(error = %e, "failed to run `say`");
            false
        }
        Err(_) => {
            warn!(limit_s = limit.as_secs(), "`say` timed out; killing it");
            false
        }
    }
}

/// Split text into speakable sentences on '.', '!', '?', or newline. A '.'
/// between digits is a decimal point ("11.0"), not a boundary. Trailing text
/// without terminal punctuation is its own sentence; empty fragments are
/// dropped.
fn split_sentences(text: &str) -> Vec<String> {
    let text = text.trim();
    let bytes = text.as_bytes();
    let mut sentences = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        let decimal_point = c == '.'
            && i > 0
            && bytes[i - 1].is_ascii_digit()
            && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit());
        if matches!(c, '.' | '!' | '?' | '\n') && !decimal_point {
            let end = i + c.len_utf8();
            let sentence = text[start..end].trim();
            if !sentence.is_empty() {
                sentences.push(sentence.to_string());
            }
            start = end;
        }
    }
    let tail = text[start..].trim();
    if !tail.is_empty() {
        sentences.push(tail.to_string());
    }
    sentences
}

/// Make written forms speakable for TTS. Today: spoken URLs/domains — a '.'
/// between a word and a 2+-letter segment ("apple.com", "github.io", "npr.org")
/// is read as " dot " so the synth says "apple dot com", not "apple, com" (Kokoro
/// otherwise treats the period as a clause pause). Conservative — left untouched:
/// a digit after the dot (a decimal like 3.14 / v2.0), a space after it (a
/// sentence end, "Mr. Smith"), and single-letter segments (initialisms "e.g.").
fn normalize_for_speech(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        let is_domain_dot = c == '.'
            && i > 0
            && chars[i - 1].is_alphanumeric()
            && chars[i + 1..]
                .iter()
                .take_while(|c| c.is_ascii_alphabetic())
                .count()
                >= 2;
        if is_domain_dot {
            out.push_str(" dot ");
        } else {
            out.push(c);
        }
    }
    out
}

/// Long answers are for the HUD/log; speech gets the first few sentences.
/// Still applied to the cloud/speak path — converse replies are NOT clipped
/// (the persona keeps them short; the server's 5-sentence synthesis cap
/// guards the rest).
fn clip_for_speech(text: &str) -> String {
    // Read the WHOLE answer aloud. These are a SANITY CEILING for a pathological
    // wall of text, not a "first few sentences" cap — the reply is already bounded
    // upstream by the token budget (conversation ~200 tok ≈ 800 chars; cloud by
    // cfg.cloud.max_tokens), so a normal answer (a full agent list, a paragraph)
    // now reads in full. Only a truly enormous reply trips the ceiling + suffix.
    const MAX_CHARS: usize = 4000;
    const MAX_SENTENCES: usize = 60;
    let sentences = split_sentences(text);
    let kept = sentences
        .iter()
        .take(MAX_SENTENCES)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let mut clipped: String = kept.chars().take(MAX_CHARS).collect();
    if sentences.len() > MAX_SENTENCES || clipped.len() < kept.len() {
        clipped.push_str(" More in the log.");
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::{
        build_report, clip_for_speech, earliest, normalize_for_speech, opener_files,
        opener_suppressed, pick_position, split_sentences,
    };
    use std::time::{Duration, Instant};

    /// Audit fix: SPEAKING is a refcount — a short-lived opener guard
    /// expiring must not unmute the mic while a content guard still lives
    /// (the old bool stored `false` unconditionally on the first drop).
    #[test]
    fn speaking_is_a_refcount_not_a_flag() {
        assert!(!super::is_speaking());
        let opener = super::SpeakingGuard::engage();
        assert!(super::is_speaking());
        let content = super::SpeakingGuard::engage();
        drop(opener); // the opener's timed release fires mid-reply...
        assert!(super::is_speaking()); // ...and the content guard still mutes
        drop(content);
        assert!(!super::is_speaking());
    }

    /// RC-11: mute_for_action() engages the content guard for the local
    /// actuation path even with instant_opener OFF (the shipped default), where
    /// begin() leaves the session cold — closing the pre-speech capture window
    /// in which the user's own command was re-segmented and the action re-fired
    /// (the live triple-open). It is idempotent: the later ensure_guard() in
    /// converse/speak adds no second guard, and dropping the reply returns the
    /// SPEAKING refcount to its prior baseline (no leak, no double-count).
    ///
    /// Parallel-safe: assertions are a DELTA against the refcount's value at
    /// entry, never an absolute is_speaking() reading another test could move.
    #[tokio::test]
    async fn mute_for_action_mutes_the_local_path_idempotently_without_leaking() {
        use std::sync::atomic::Ordering;

        let root = temp_root();
        let mut cfg = crate::config::Config::default();
        // This test asserts the COLD begin path (no opener), so disable the
        // now-ON-by-default instant_opener explicitly.
        cfg.speech.instant_opener = false;
        cfg.speech.opener_delay_ms = 0;

        let base = super::SPEAKING.load(Ordering::Relaxed);
        let mut reply = super::ReplySession::begin(&root, &cfg).await;
        // Cold after begin with instant_opener off: no guard, no mute delta.
        assert!(reply.guard.is_none(), "begin must stay cold when instant_opener is off");
        assert_eq!(super::SPEAKING.load(Ordering::Relaxed), base, "begin adds no mute");

        // Committing to a local action mutes the mic: exactly one guard above
        // the baseline.
        reply.mute_for_action();
        assert!(reply.guard.is_some(), "mute_for_action must engage the content guard");
        assert_eq!(
            super::SPEAKING.load(Ordering::Relaxed),
            base + 1,
            "the action mute adds exactly one to the refcount"
        );

        // Idempotent: the later ensure_guard() (converse/speak) and a second
        // mute_for_action() add NO further guard — the refcount stays at base+1.
        reply.mute_for_action();
        reply.ensure_guard();
        assert_eq!(
            super::SPEAKING.load(Ordering::Relaxed),
            base + 1,
            "re-engaging the guard must not grow the refcount"
        );

        // Dropping the reply releases the single guard: back to baseline, no leak.
        drop(reply);
        assert_eq!(
            super::SPEAKING.load(Ordering::Relaxed),
            base,
            "the guard must be released on drop — no mic-mute leak"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// RC-4: a barge already pending makes play_unless return Barged WITHOUT
    /// polling the play future at all — the fallback child is never even
    /// spawned. Predicate-injected, so no process-global flag is touched (the
    /// race logic is verified deterministically and in parallel-safe isolation).
    #[tokio::test]
    async fn play_unless_short_circuits_when_already_barged() {
        use super::{play_unless, Delivered};
        use std::cell::Cell;

        let polled = Cell::new(false);
        let outcome = play_unless(
            async {
                polled.set(true);
                true
            },
            || true, // already barged
        )
        .await;
        assert_eq!(outcome, Delivered::Barged, "pre-barge must short-circuit");
        assert!(!polled.get(), "the play future must not run when pre-barged");
    }

    /// RC-4: with NO barge, play_unless runs the clip to completion and reports
    /// its result (audio produced or not).
    #[tokio::test]
    async fn play_unless_completes_a_clip_when_not_barged() {
        use super::{play_unless, Delivered};
        assert_eq!(play_unless(async { true }, || false).await, Delivered::Played(true));
        assert_eq!(play_unless(async { false }, || false).await, Delivered::Played(false));
    }

    /// RC-4: a barge that flips true WHILE a (slow) clip plays drops the play
    /// future — the live code's kill_on_drop then reaps the afplay/say child. A
    /// 30s sleep models a still-playing clip; the predicate returns false on the
    /// pre-check and true on the first poll-tick check (mid-play), so play_unless
    /// cuts it on a BARGE_POLL tick instead of awaiting completion. Real time,
    /// but bounded by one ~25ms poll tick.
    #[tokio::test]
    async fn play_unless_aborts_a_clip_mid_play() {
        use super::{play_unless, Delivered};
        use std::cell::Cell;
        use std::time::Duration;

        let finished = Cell::new(false);
        let polls = Cell::new(0u32);
        let outcome = play_unless(
            async {
                // Far longer than any poll tick: it must be dropped, not awaited.
                tokio::time::sleep(Duration::from_secs(30)).await;
                finished.set(true);
                true
            },
            || {
                // Not barged on the pre-check (poll 0); barged on the first
                // poll-tick check (poll 1) — i.e. mid-play.
                let n = polls.get();
                polls.set(n + 1);
                n >= 1
            },
        )
        .await;
        assert_eq!(outcome, Delivered::Barged, "the clip must be cut by the barge");
        assert!(!finished.get(), "the dropped clip must not complete");
        assert!(polls.get() >= 2, "must poll at least the pre-check and one tick");
    }

    #[test]
    fn split_keeps_decimals_inside_sentences() {
        let sentences = split_sentences("Systems are green. CPU at 6.5 percent. Done.");
        assert_eq!(
            sentences,
            vec!["Systems are green.", "CPU at 6.5 percent.", "Done."]
        );
    }

    #[test]
    fn split_emits_trailing_text_without_punctuation() {
        let sentences = split_sentences("All quiet on the bus. Standing by");
        assert_eq!(sentences, vec!["All quiet on the bus.", "Standing by"]);
    }

    #[test]
    fn decimals_are_not_sentence_breaks() {
        let status = "Systems are green. CPU at 6 percent. Memory: 11.0 of 16 gigabytes.";
        assert_eq!(clip_for_speech(status), status);
    }

    #[test]
    fn reads_full_answers_and_only_clips_a_pathological_wall() {
        // A normal multi-sentence answer reads IN FULL now — no "first 3
        // sentences" truncation, no "More in the log." (this is the user-facing
        // fix: the full reply, e.g. the agent roster, is spoken).
        let normal = "One thing. Two things. Three: 4.5 units done. And a fourth sentence.";
        let kept = clip_for_speech(normal);
        assert_eq!(kept, normal, "a normal answer must read in full: {kept}");
        assert!(!kept.contains("More in the log"));
        // Only a truly enormous reply trips the sanity ceiling + suffix.
        let wall = "word. ".repeat(2000); // ~12k chars / 2k sentences
        let clipped = clip_for_speech(&wall);
        assert!(clipped.ends_with("More in the log."), "huge text should clip: ...{}", &clipped[clipped.len().saturating_sub(40)..]);
        assert!(clipped.len() < wall.len());
    }

    #[test]
    fn normalize_for_speech_says_dot_for_domains_only() {
        assert_eq!(normalize_for_speech("go to apple.com"), "go to apple dot com");
        assert_eq!(
            normalize_for_speech("open github.io and npr.org"),
            "open github dot io and npr dot org"
        );
        // Decimals, sentence ends, initialisms: untouched (no spurious "dot").
        assert_eq!(normalize_for_speech("CPU at 11.0 percent."), "CPU at 11.0 percent.");
        assert_eq!(normalize_for_speech("Done. Next, sir."), "Done. Next, sir.");
        assert_eq!(normalize_for_speech("e.g. this one"), "e.g. this one");
        assert_eq!(normalize_for_speech("v2.0 shipped"), "v2.0 shipped");
    }

    #[test]
    fn earliest_prefers_the_earlier_instant() {
        let a = Instant::now();
        let b = a + Duration::from_millis(50);
        assert_eq!(earliest(Some(a), Some(b)), Some(a));
        assert_eq!(earliest(None, Some(b)), Some(b));
        assert_eq!(earliest(Some(a), None), Some(a));
        assert_eq!(earliest(None, None), None);
    }

    #[test]
    fn report_times_against_pickup_and_first_audio() {
        let pickup = Instant::now();
        let first = pickup + Duration::from_millis(900);
        let end = pickup + Duration::from_millis(2400);
        let r = build_report(pickup, Some(first), end);
        assert_eq!(r.first_audio_ms, Some(900));
        assert_eq!(r.speak_ms, 1500);
        assert_eq!(r.total_ms, 2400);

        let silent = build_report(pickup, None, end);
        assert_eq!(silent.first_audio_ms, None);
        assert_eq!(silent.speak_ms, 0);
        assert_eq!(silent.total_ms, 2400);
    }

    #[test]
    fn opener_pick_is_uniform_and_never_repeats_the_last() {
        // With 5 candidates and last=2, every entropy value must map into
        // the other four positions, and all four must be reachable.
        let mut seen = [false; 5];
        for entropy in 0..100 {
            let pos = pick_position(5, Some(2), entropy);
            assert_ne!(pos, 2, "repeated the previous opener");
            assert!(pos < 5);
            seen[pos] = true;
        }
        assert_eq!(seen, [true, true, false, true, true]);
    }

    #[test]
    fn opener_pick_handles_edges() {
        // Single candidate: repetition is unavoidable and allowed.
        assert_eq!(pick_position(1, Some(0), 7), 0);
        // No previous pick: plain uniform draw.
        assert_eq!(pick_position(4, None, 6), 2);
        // Stale last index (file no longer present): plain uniform draw.
        assert_eq!(pick_position(3, Some(9), 7), 1);
    }

    #[test]
    fn opener_suppression_covers_six_seconds() {
        assert!(!opener_suppressed(10_000, 0)); // never fired
        assert!(opener_suppressed(10_000, 9_000)); // 1s ago
        assert!(opener_suppressed(10_000, 4_001)); // 5.999s ago
        assert!(!opener_suppressed(10_000, 4_000)); // exactly 6s ago
        assert!(!opener_suppressed(10_000, 1_000)); // 9s ago
    }

    /// A throwaway root with an EMPTY state/openers/ dir. begin() touches no
    /// audio device: Session::new() only bumps a generation counter, and with
    /// no opener WAVs present try_opener never appends a clip — so this test
    /// stays silent on every path (constraint: never play audio in tests).
    fn temp_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "jarvis-begin-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("state").join("openers")).unwrap();
        root
    }

    /// CONTRACT part A: with [speech].instant_opener = false (the shipped
    /// default) begin() fires NO canned opener — opener_text() is None — and
    /// it does not even take the opener_delay_ms breath, so the converse
    /// stream becomes the whole, naturally phrased reply.
    #[tokio::test]
    async fn instant_opener_off_fires_no_opener_and_skips_the_breath() {
        let root = temp_root();
        let mut cfg = crate::config::Config::default();
        // The shipped DEFAULT is now ON (full-power); disable explicitly to exercise
        // the off path (no opener fires, the breath is skipped).
        cfg.speech.instant_opener = false;
        // A breath this long would dominate the measurement IF it were taken.
        cfg.speech.opener_delay_ms = 5_000;

        let started = std::time::Instant::now();
        let reply = super::ReplySession::begin(&root, &cfg).await;
        let elapsed = started.elapsed();

        assert!(reply.opener_text().is_none(), "no opener may fire when off");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "the {}ms breath must be skipped when instant_opener is off (took {elapsed:?})",
            cfg.speech.opener_delay_ms
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// CONTRACT part A: with instant_opener = true the prior behavior holds —
    /// the opener machinery runs (breath + try_opener). With no opener WAVs on
    /// disk it stays dormant exactly as before the feature could be gated, so
    /// opener_text() is None and no audio plays. opener_delay_ms is zeroed so
    /// the machinery is exercised without a real wait.
    #[tokio::test]
    async fn instant_opener_on_runs_the_machinery_dormant_without_wavs() {
        let root = temp_root();
        let mut cfg = crate::config::Config::default();
        cfg.speech.instant_opener = true;
        cfg.speech.opener_delay_ms = 0; // exercise the path, don't wait

        let reply = super::ReplySession::begin(&root, &cfg).await;
        // Empty openers dir: the opener path is reached but finds nothing to
        // play, the documented dormant behavior — None, and silent.
        assert!(
            reply.opener_text().is_none(),
            "an empty openers dir must leave the reply unacknowledged"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn opener_files_parses_and_sorts_indices() {
        // A unique temp dir; no audio device or playback thread involved.
        let dir = std::env::temp_dir().join(format!(
            "jarvis-opener-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["opener-2.wav", "opener-0.wav", "notes.txt", "opener-x.wav"] {
            std::fs::write(dir.join(name), b"stub").unwrap();
        }
        let files = opener_files(&dir);
        let indices: Vec<usize> = files.iter().map(|(idx, _)| *idx).collect();
        assert_eq!(indices, vec![0, 2]);
        assert!(files[0].1.ends_with("opener-0.wav"));
        std::fs::remove_dir_all(&dir).unwrap();

        // Missing dir: dormant, not an error.
        assert!(opener_files(&dir).is_empty());
    }
}
