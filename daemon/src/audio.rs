use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::telemetry;
use crate::Event;

const FRAME_MS: u64 = 30;
/// Hard cap on one VAD segment. Continuous above-threshold sound (music, a
/// fan, the TV) must not grow the buffer without bound (~11.5 MB/min at
/// 48kHz) or produce WAVs too long to transcribe inside the request timeout.
const MAX_SEGMENT_SECS: usize = 30;
/// Echo-settle window measured from the moment DARWIN goes QUIET after a barge
/// — i.e. from when `is_speaking()` drops, which itself already trails the last
/// audio by the reply loop's MUTE_TAIL. Room echo + the speaker's draining audio
/// linger a little past that, so the capture gate stays shut for this ADDITIONAL
/// margin before feeding the VAD — DARWIN's own tail can never be segmented into
/// an utterance, transcribed, and re-routed (the echo-feedback / triple-open
/// bug). This is ON TOP of MUTE_TAIL (the clock starts where MUTE_TAIL ends), so
/// the total post-audio cushion is MUTE_TAIL + this. Acoustic length is
/// device-gated; the clock START POINT (quiet-onset, not the barge instant) is
/// the load-bearing part — measuring from the barge instant gave ~zero cushion.
const BARGE_SETTLE_MS: u64 = 350;
/// Minimum spacing between audio.level telemetry events (~15 Hz): plenty for
/// a 60fps HUD waveform that interpolates between samples, while guaranteeing
/// the WS broadcast channel is never flooded by the audio thread.
const LEVEL_INTERVAL: Duration = Duration::from_millis(66);

pub fn spawn_capture(root: PathBuf, cfg: Arc<Config>, tx: UnboundedSender<Event>) {
    // cpal::Stream is !Send, so the stream must live on the thread that owns
    // the capture loop, not in a tokio task.
    std::thread::Builder::new()
        .name("audio-capture".to_string())
        .spawn(move || {
            if let Err(e) = capture_loop(root, cfg, tx) {
                error!(error = %e, "audio capture stopped");
            }
        })
        .expect("spawn audio thread");
}

fn capture_loop(root: PathBuf, cfg: Arc<Config>, tx: UnboundedSender<Event>) -> Result<()> {
    // The processing loop below reads `raw_rx` — a stream of interleaved-f32
    // chunks — and is IDENTICAL regardless of where those chunks come from. Only
    // the SOURCE branches on [voice].mic_source:
    //
    //   * "device" (the default): open the local input device with cpal and let
    //     the realtime callback push frames into raw_tx — today's behavior,
    //     byte-for-byte. The cpal Stream is !Send and must outlive the loop, so it
    //     is held in `_cpal_stream` (a guard dropped only when the loop returns).
    //   * "app": instead of opening the mic, bind+accept the audio_in.sock, verify
    //     the token handshake, take (sample_rate, channels) from the header, and
    //     spawn a reader thread that decodes length-prefixed f32 frames into the
    //     SAME raw_tx. The cpal device is never touched.
    //
    // Either way we end up with the same (raw_rx, sample_rate, channels) and fall
    // through to the UNCHANGED processing loop.
    let (raw_tx, raw_rx) = std_mpsc::channel::<Vec<f32>>();

    // Kept alive for the whole loop: in device mode it's the cpal Stream guard; in
    // app mode it stays None (the reader thread owns the socket). Dropping it stops
    // the device.
    let _cpal_stream: Option<cpal::Stream>;
    let sample_rate: u32;
    let channels: usize;

    if mic_source_is_app(&cfg.voice.mic_source) {
        // APP MODE: route the mic in over state/ipc/audio_in.sock from the HUD.
        // The daemon binds the socket (0700 dir / 0600 socket), accepts ONE HUD
        // connection, verifies the token in the JSON handshake line, and reads the
        // (sample_rate, channels) the HUD declares. A reader thread then decodes
        // length-prefixed f32 frames and pushes each Vec<f32> into raw_tx — the
        // SAME channel the cpal callback feeds. A bind/handshake/token failure is
        // logged and returns Err cleanly (the loop never panics or wedges).
        let header = accept_app_audio(&root, raw_tx)?;
        sample_rate = header.sample_rate;
        channels = (header.channels as usize).max(1);
        _cpal_stream = None;
        info!(sample_rate, channels, "audio capture running (app source)");
    } else {
        // DEVICE MODE (default): today's cpal path, byte-for-byte unchanged.
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?;
        let supported = device
            .default_input_config()
            .context("querying default input config")?;
        sample_rate = supported.sample_rate();
        channels = supported.channels() as usize;
        let stream_config: cpal::StreamConfig = supported.into();

        // The cpal callback runs on a realtime audio thread and must not block or
        // allocate heavily: ship raw frames over a std channel and do VAD + WAV
        // writing here instead.
        let stream = device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let _ = raw_tx.send(data.to_vec());
            },
            |err| warn!(error = %err, "cpal stream error"),
            None,
        )?;
        stream.play().context("starting input stream")?;
        info!(sample_rate, channels, "audio capture running");
        _cpal_stream = Some(stream);
    }

    let tmp_dir = root.join("state").join("tmp");
    let mut vad = Vad::new(&cfg, sample_rate);
    let mut barge = BargeDetector::new(&cfg, sample_rate);
    let mut meter = LevelMeter::new();
    // Barge-in tuning aid: rate-limited log of the mic level DURING playback, so
    // barge_in_rms can be set from real data instead of guessed.
    let mut last_barge_log = Instant::now();
    // Echo-safety state machine (RC-1): set the moment a barge fires; capture
    // stays gated shut until DARWIN is no longer speaking AND BARGE_SETTLE_MS
    // has elapsed since this instant, so the gate can never feed DARWIN's own
    // draining audio / room echo into the VAD. None when no barge is pending.
    let mut barge_armed_at: Option<Instant> = None;
    // A barge fired but DARWIN is still speaking: the echo-settle clock
    // (barge_armed_at) is started only once he goes quiet, so the settle is
    // measured from quiet-onset, not the barge instant (which falls during
    // speech and would give ~zero real post-speech cushion).
    let mut barge_pending = false;
    let settle = Duration::from_millis(BARGE_SETTLE_MS);
    // #34 WHISPER AUTO-ENGAGE energy series. A SMALL ring of recent per-chunk mono
    // RMS values over genuine user-capture windows (never DARWIN's echo). The PURE
    // `prosody::apply_auto_engage_global` heuristic reads it behind BOTH
    // [voice].whisper && [voice].whisper_auto; with EITHER off (the shipped default)
    // the call is a no-op and we never even build the series, so capture is
    // byte-for-byte today's. This NEVER opens the mic — it folds over the RMS the
    // loop already computes for capture frames.
    let mut whisper_energies: std::collections::VecDeque<f32> = std::collections::VecDeque::new();
    const WHISPER_ENERGY_WINDOW: usize = 12;
    // PID in the filename keeps names collision-free across daemon restarts
    // (the counter restarts at 0 every launch and would silently overwrite
    // the previous run's WAVs, invalidating transcript wav_path references).
    let pid = std::process::id();

    while let Ok(chunk) = raw_rx.recv() {
        // LOCKDOWN OVERLAY (task #12 — THE mic kill). While the emergency stop is
        // engaged the capture loop IGNORES every frame: drop it, reset the VAD so
        // no partial utterance survives, and never emit/segment/transcribe. This
        // is checked FIRST — before the level meter, the speaking gate, the
        // barge detector, and the VAD — so panic silences the microphone the very
        // next chunk, with no path that lets audio through. The cpal stream stays
        // open (so unlock resumes instantly without re-acquiring the device), but
        // nothing it captures is processed. With lockdown OFF (the shipped
        // default) this branch is never taken and capture is byte-for-byte today.
        if mic_capture_suppressed(crate::lockdown::is_locked_down()) {
            vad.reset();
            continue;
        }
        // Live mic level for the HUD waveform, rate-limited to one event per
        // LEVEL_INTERVAL. Emitted BEFORE the speaking gate on purpose: while
        // DARWIN talks the mic still hears the room (mostly DARWIN itself)
        // and the HUD waveform must stay alive, flagged speaking=true.
        if let Some(rms) = meter.push_frames(&chunk, channels, Instant::now()) {
            telemetry::emit(
                "audio",
                "audio.level",
                json!({"rms": rms, "speaking": crate::speech::is_speaking()}),
            );
        }
        // Capture gate (RC-1 — the echo-safety invariant). The decision is NOT
        // "is a barge requested": that is what let DARWIN hear himself (the
        // gate reopened while is_speaking() was still true through MUTE_TAIL,
        // so his draining audio + echo was segmented, transcribed and
        // re-routed — the triple-open / "glitching" bug). The rule now:
        //
        //   * While DARWIN is speaking: ALWAYS drop + reset. No exception for a
        //     pending barge — BARGE_IN only means "stop synthesizing the rest
        //     of THIS reply", never "start capturing".
        //   * After a barge, once DARWIN has gone quiet: keep dropping until the
        //     echo-settle window has elapsed, THEN arm capture (reset the VAD so
        //     the user's real utterance starts from a clean onset).
        //   * No barge, not speaking: capture normally.
        let speaking = crate::speech::is_speaking();
        if speaking {
            let rms = chunk_rms(&chunk, channels);
            let frames = chunk.len() / channels.max(1);
            // Tuning aid: when the mic rises above the silence floor WHILE
            // DARWIN speaks (his echo, or you talking over him), log the level
            // at most ~2x/sec. Talk over him and read the peak here, then set
            // barge_in_rms to just under it.
            let now = Instant::now();
            if rms > cfg.audio.rms_threshold as f32
                && now.duration_since(last_barge_log) > Duration::from_millis(500)
            {
                last_barge_log = now;
                info!(
                    mic_rms = round4(rms as f64),
                    barge_in_rms = cfg.audio.barge_in_rms,
                    "barge: mic level during playback (set barge_in_rms just under your interruption level)"
                );
            }
            // Track DARWIN's echo level so the detector's threshold stays
            // adaptive (RC-8): every dropped frame feeds the rolling baseline.
            barge.observe_baseline(rms);
            // Only run the detector while no barge is already pending — once one
            // has fired, the reply is already being cut and re-firing is moot.
            if !crate::speech::barge_in_requested() && barge.observe(rms, frames) {
                info!(rms, "barge-in: user spoke over DARWIN; cutting the reply");
                telemetry::emit("audio", "barge_in", json!({"rms": round4(rms as f64)}));
                crate::speech::request_barge_in();
                barge.reset();
                // Mark the barge pending; the echo-settle clock starts only once
                // DARWIN goes quiet (below), so capture resumes AFTER he stops
                // AND the echo tail clears — never on his own draining audio.
                barge_pending = true;
            }
            // Speaking: drop every frame, no matter the barge state. This is the
            // single rule that makes echo-feedback impossible.
            vad.reset();
            continue;
        }

        // Not speaking. If a barge is pending (it fired while DARWIN was still
        // talking), START the echo-settle clock NOW — this first quiet chunk —
        // so the settle is measured from when he actually went quiet, not from
        // the barge instant (which fell during speech and gave ~zero cushion).
        // The pure `gate_decision` then keeps dropping until the settle elapses.
        if barge_pending {
            barge_armed_at = Some(Instant::now());
            barge_pending = false;
        }
        barge.reset();
        let since_barge = barge_armed_at.map(|t| t.elapsed());
        match gate_decision(false, since_barge, settle) {
            Gate::Drop => {
                vad.reset();
                continue;
            }
            Gate::Capture => {
                // First capturable chunk after a barge: clear the pending state
                // and start the user's utterance from a clean VAD onset.
                if barge_armed_at.take().is_some() {
                    vad.reset();
                }
            }
        }

        // #34 WHISPER AUTO-ENGAGE (mic-gated live site; inert by flag). On a genuine
        // user-capture chunk, fold its mono RMS into the bounded energy ring and let
        // the PURE heuristic decide whether SUSTAINED-quiet input should engage
        // whisper. Both gates ([voice].whisper && [voice].whisper_auto) are honoured
        // INSIDE apply_auto_engage_global, so with the shipped defaults (both OFF)
        // this is a no-op and the capture path is byte-for-byte today's. It NEVER
        // opens the mic (the RMS is computed from `chunk`, which the loop already has)
        // and NEVER auto-DISENGAGES (only the explicit command turns whisper off). The
        // PURE is_sustained_quiet heuristic is unit-tested in prosody.rs; this is the
        // headlessly-untestable live call, wired inert-by-flag.
        if cfg.voice.whisper && cfg.voice.whisper_auto {
            let rms = chunk_rms(&chunk, channels);
            if whisper_energies.len() == WHISPER_ENERGY_WINDOW {
                whisper_energies.pop_front();
            }
            whisper_energies.push_back(rms);
            let series: Vec<f32> = whisper_energies.iter().copied().collect();
            crate::prosody::apply_auto_engage_global(&cfg, &series);
        }

        // Downmix interleaved frames to mono by averaging channels.
        for frame in chunk.chunks(channels.max(1)) {
            let mono = frame.iter().copied().sum::<f32>() / frame.len() as f32;
            if let Some(segment) = vad.push(mono) {
                let path = tmp_dir.join(format!("utterance-{pid}-{}.wav", vad.take_counter()));
                // #30 CONTINUOUS LIVE INTERPRETATION (device-gated segment feed; inert by
                // flag). When [interpret].live is on, THIS finished VAD segment enters the
                // continuous-interpret feed: the segment is transcribed and run through the
                // PURE `interpret::interpret_segment` pipeline downstream in run_pipeline
                // (where the InferenceClient + ReplySession live), which translates it and
                // — when [interpret].speak is on — voices it through the SINGLE echo-safe
                // speech path (mic-mute guard + barge-in + the is_speaking() capture gate
                // all cover it; never a parallel audio path). Here at the segment site we
                // only emit the honest device-gated marker so the HUD shows the live
                // interpret feed is active; the mic loop is DEVICE-GATED and the pipeline
                // wiring is behind the SAME flag. With [interpret].live OFF (the shipped
                // default) this is a no-op and the segment path is byte-for-byte today's
                // (the segment is emitted as an ordinary Event::Utterance and routed
                // normally) — exactly the Batch-C auto-engage / vision runSocketServed
                // inert-by-flag precedent.
                if cfg.interpret.live {
                    telemetry::emit(
                        "audio",
                        "interpret.segment_fed",
                        json!({"target": cfg.interpret.target_lang, "speak": cfg.interpret.speak}),
                    );
                }
                match write_wav(&path, sample_rate, &segment) {
                    Ok(()) => {
                        // Voice-id (round G): compute the on-device speaker
                        // embedding from the SAME raw f32 segment (not the lossy
                        // i16 WAV), only when [voice_id].enabled — when off we
                        // skip the work entirely and pass None (unchanged
                        // behavior; the turn handler treats None as not-enforced
                        // unless enabled+enrolled, where it is the fail-closed
                        // unverified case). The embedding is a feature VECTOR,
                        // never raw audio.
                        let embedding = if cfg.voice_id.enabled {
                            crate::voiceid::embed(&segment, sample_rate)
                        } else {
                            None
                        };
                        if tx.send(Event::Utterance { wav: path, embedding }).is_err() {
                            return Ok(()); // main loop gone; shut down
                        }
                    }
                    Err(e) => warn!(error = %e, "failed to write utterance wav"),
                }
            }
        }
    }
    Ok(())
}

// ===========================================================================
// APP-MODE MIC INGEST — route the mic in over state/ipc/audio_in.sock.
//
// The HUD app captures the microphone and streams it to the daemon over a
// confined, token-authenticated Unix socket, instead of the daemon opening the
// device with cpal. The WIRE CONTRACT (both sides match byte-for-byte):
//
//   * Unix stream socket at  <root>/state/ipc/audio_in.sock  (the daemon binds
//     it; the ipc dir is 0700, the socket 0600).
//   * HANDSHAKE: the HUD sends EXACTLY ONE line of UTF-8 JSON terminated by '\n':
//        {"token":"<command.token>","sample_rate":<u32>,"channels":<u16>}
//     The daemon verifies `token` with `apps::verify_command_token` (the SAME
//     per-boot HMAC capability token the command channel uses). An invalid token
//     closes the connection with NO audio ingested.
//   * FRAMES: after the handshake, the HUD streams repeated binary frames, each:
//        [4 bytes: u32 little-endian = N, the number of f32 samples]
//        [N * 4 bytes: the samples as f32 little-endian]
//     The daemon decodes each into a Vec<f32> and pushes it down the SAME channel
//     the cpal callback feeds. On EOF / read error the reader stops ingest.
//
// This runs on the capture thread (a plain std::thread, NOT a tokio task), so it
// uses blocking std::os::unix::net sockets — no async runtime is involved.
// ===========================================================================

/// Hard cap on the JSON handshake line. The handshake is a tiny fixed object
/// (token + two integers); anything larger is a probe or mistake and is rejected
/// BEFORE parse so a hostile client can't feed the JSON parser an unbounded line.
const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;
/// Hard cap on a single frame's sample count (the u32 length prefix). At 48 kHz
/// stereo this is ~10 s of audio per frame — far above any real capture buffer —
/// so a corrupt/hostile prefix can never make the daemon attempt a multi-gigabyte
/// allocation. A frame above this closes the connection.
const MAX_FRAME_SAMPLES: u32 = 1_000_000;

/// The parsed, token-VERIFIED app-audio handshake header. Produced ONLY after
/// `apps::verify_command_token` accepts the presented token, so reaching this
/// struct already means the connection is authenticated.
#[derive(Debug, Clone, Copy)]
struct AppAudioHeader {
    sample_rate: u32,
    channels: u16,
}

/// The on-the-wire handshake line shape. We read only what we need; the token is
/// verified (and never stored past the check / logged), the rate/channels are
/// taken as the capture format.
#[derive(serde::Deserialize)]
struct AppAudioHandshake {
    #[serde(default)]
    token: String,
    #[serde(default)]
    sample_rate: u32,
    #[serde(default)]
    channels: u16,
}

/// The app-audio socket path: `<root>/state/ipc/audio_in.sock`, alongside the
/// command socket (same confined 0700 ipc dir).
fn audio_in_sock_path(root: &std::path::Path) -> PathBuf {
    root.join("state").join("ipc").join("audio_in.sock")
}

/// Bind the app-audio socket: remove a stale one, create the 0700 parent dir,
/// bind, chmod 0600. Mirrors the command channel's bind (defense-in-depth on top
/// of the token gate).
fn bind_audio_socket(path: &std::path::Path) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %path.display(), error = %e, "could not remove stale audio_in socket");
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

/// Read + verify the handshake line from an accepted connection. Reads ONE
/// '\n'-terminated line (bounded by [`MAX_HANDSHAKE_BYTES`]), parses it, and
/// verifies the token with `apps::verify_command_token`. Returns the header ONLY
/// on a valid token; an oversized/malformed line or a bad token is an `Err` (the
/// caller closes the connection — no audio). The token value is never logged.
fn read_app_handshake<R: std::io::BufRead>(reader: &mut R) -> Result<AppAudioHeader> {
    use std::io::BufRead as _;
    let mut line = Vec::new();
    // Bounded read: take at most MAX_HANDSHAKE_BYTES+1 so an unterminated flood
    // can't grow the buffer without bound, then require a terminating newline.
    let n = std::io::Read::take(&mut *reader, MAX_HANDSHAKE_BYTES as u64 + 1)
        .read_until(b'\n', &mut line)
        .context("reading app-audio handshake")?;
    if n == 0 {
        return Err(anyhow!("app-audio connection closed before handshake"));
    }
    if line.len() > MAX_HANDSHAKE_BYTES {
        return Err(anyhow!("app-audio handshake line oversized"));
    }
    let hs: AppAudioHandshake =
        serde_json::from_slice(&line).context("parsing app-audio handshake JSON")?;
    if !crate::apps::verify_command_token(&hs.token) {
        // No token value is logged — only that verification failed.
        return Err(anyhow!("app-audio handshake token failed verification"));
    }
    Ok(AppAudioHeader {
        sample_rate: hs.sample_rate,
        channels: hs.channels,
    })
}

/// Bind the app-audio socket, accept ONE HUD connection, verify the handshake,
/// and spawn a reader thread that decodes length-prefixed f32 frames into
/// `raw_tx`. Returns the verified header (sample_rate/channels) so the caller can
/// run the processing loop. A bind/handshake/token failure is an `Err` (logged by
/// the caller) — the loop never panics or wedges.
///
/// The accept BLOCKS until the HUD connects (the capture thread has nothing to do
/// until there is an audio source), mirroring how device mode blocks until cpal
/// delivers the first frame.
fn accept_app_audio(
    root: &std::path::Path,
    raw_tx: std_mpsc::Sender<Vec<f32>>,
) -> Result<AppAudioHeader> {
    let sock = audio_in_sock_path(root);
    let listener = bind_audio_socket(&sock)
        .with_context(|| format!("binding app-audio socket {}", sock.display()))?;
    info!(path = %sock.display(), "app-audio ingest listening");

    // Accept the HUD connection. We serve a single audio source at a time (one
    // mic), so we take the first connection and read it to EOF.
    let (stream, _peer) = listener.accept().context("accepting app-audio connection")?;
    // Bound the HANDSHAKE: a same-user client that connects then stalls before
    // sending its token must not pin the capture thread in read_until forever.
    // Cleared right after the handshake so continuous frame reads block normally.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = std::io::BufReader::new(stream);
    let header = read_app_handshake(&mut reader)?;
    let _ = reader.get_ref().set_read_timeout(None);

    // Reader thread: owns the connection + a clone-free move of raw_tx, decodes
    // frames, pushes each Vec<f32> into the SAME channel the cpal callback feeds.
    // Ends on EOF / read error / a frame over the cap (logged, then stops ingest).
    std::thread::Builder::new()
        .name("audio-app-ingest".to_string())
        .spawn(move || {
            if let Err(e) = read_app_frames(&mut reader, &raw_tx) {
                info!(error = %e, "app-audio ingest ended");
            }
        })
        .context("spawning app-audio reader thread")?;

    Ok(header)
}

/// Read length-prefixed f32 frames from an authenticated connection until EOF /
/// error, pushing each decoded `Vec<f32>` into `raw_tx`. Each frame is a 4-byte
/// LE `u32` sample count `N` (bounded by [`MAX_FRAME_SAMPLES`]) followed by
/// `N * 4` LE-f32 bytes. A clean EOF (the HUD closed) returns `Ok(())`; a partial
/// frame / oversized prefix is an `Err`. Stops the moment the receiver is gone
/// (the processing loop returned).
fn read_app_frames<R: std::io::Read>(
    reader: &mut R,
    raw_tx: &std_mpsc::Sender<Vec<f32>>,
) -> Result<()> {
    loop {
        let mut len_buf = [0u8; 4];
        // A clean EOF exactly at a frame boundary is the normal end of stream.
        match read_full_or_eof(reader, &mut len_buf)? {
            ReadFrame::Eof => return Ok(()),
            ReadFrame::Got => {}
        }
        let n = u32::from_le_bytes(len_buf);
        if n > MAX_FRAME_SAMPLES {
            return Err(anyhow!("app-audio frame sample count {n} exceeds cap"));
        }
        let mut payload = vec![0u8; n as usize * 4];
        reader
            .read_exact(&mut payload)
            .context("reading app-audio frame payload")?;
        let frame = decode_frame(&payload);
        // The receiver is gone once the processing loop returns — stop cleanly.
        if raw_tx.send(frame).is_err() {
            return Ok(());
        }
    }
}

/// Outcome of an at-a-frame-boundary read: a clean EOF vs. a full buffer.
enum ReadFrame {
    Eof,
    Got,
}

/// Fill `buf` fully, but report a CLEAN EOF (no bytes read at all) as
/// [`ReadFrame::Eof`] rather than an error — that is the normal end of stream at
/// a frame boundary. A partial read (some bytes then EOF) IS an error (a
/// truncated length prefix).
fn read_full_or_eof<R: std::io::Read>(reader: &mut R, buf: &mut [u8]) -> Result<ReadFrame> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                if filled == 0 {
                    return Ok(ReadFrame::Eof);
                }
                return Err(anyhow!("app-audio stream ended mid length-prefix"));
            }
            Ok(k) => filled += k,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(anyhow::Error::new(e).context("reading app-audio length prefix")),
        }
    }
    Ok(ReadFrame::Got)
}

/// Decode the PAYLOAD of one frame — `4 * k` little-endian bytes — into `k`
/// `f32` samples. PURE: it folds little-endian byte quads into f32s. Trailing
/// bytes that don't complete a 4-byte sample are dropped (a well-formed frame has
/// exactly `4 * N` payload bytes, so this only guards a malformed tail). Inverse
/// of [`encode_frame`] over the sample region.
fn decode_frame(payload: &[u8]) -> Vec<f32> {
    payload
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// Encode `samples` into a full length-prefixed frame: a 4-byte LE `u32` count
/// followed by the samples as LE f32. Inverse of the wire read in
/// [`read_app_frames`] (and of [`decode_frame`] over the payload). Used by the
/// roundtrip unit tests; mirrors what the HUD writes.
#[cfg(test)]
fn encode_frame(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + samples.len() * 4);
    out.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    for &s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// True when the configured `[voice].mic_source` selects the APP socket ingest.
/// Any value other than the exact "app" (including the default "device" and any
/// typo) is the safe device default — the cpal path is never disabled by a
/// mistyped value.
fn mic_source_is_app(mic_source: &str) -> bool {
    mic_source == "app"
}

fn write_wav(path: &std::path::Path, sample_rate: u32, samples: &[f32]) -> Result<()> {
    // i16 PCM, not f32: the inference server decodes these with Python's
    // stdlib `wave`, which cannot read IEEE-float WAVs.
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)?;
    for &s in samples {
        writer.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Energy-based VAD over ~30ms frames. Speech begins once RMS stays above the
/// threshold for min_speech_ms (the run-up is buffered so the segment keeps
/// its onset); it ends after silence_ms below the threshold.
struct Vad {
    threshold: f32,
    frame_len: usize,
    min_speech_samples: usize,
    silence_limit_samples: usize,
    max_segment_samples: usize,
    frame: Vec<f32>,
    in_speech: bool,
    voiced_run: usize,
    silent_run: usize,
    pending: Vec<f32>,
    segment: Vec<f32>,
    counter: u64,
}

impl Vad {
    fn new(cfg: &Config, sample_rate: u32) -> Self {
        let per_ms = sample_rate as usize / 1000;
        Self {
            threshold: cfg.audio.rms_threshold as f32,
            frame_len: (per_ms * FRAME_MS as usize).max(1),
            min_speech_samples: (per_ms * cfg.audio.min_speech_ms as usize).max(1),
            silence_limit_samples: (per_ms * cfg.audio.silence_ms as usize).max(1),
            max_segment_samples: (sample_rate as usize * MAX_SEGMENT_SECS).max(1),
            frame: Vec::new(),
            in_speech: false,
            voiced_run: 0,
            silent_run: 0,
            pending: Vec::new(),
            segment: Vec::new(),
            counter: 0,
        }
    }

    /// Feed one mono sample; returns a finished segment when an utterance ends.
    fn push(&mut self, sample: f32) -> Option<Vec<f32>> {
        self.frame.push(sample);
        if self.frame.len() < self.frame_len {
            return None;
        }
        let rms = (self.frame.iter().map(|s| s * s).sum::<f32>() / self.frame.len() as f32).sqrt();
        let frame = std::mem::take(&mut self.frame);
        self.step(rms, frame)
    }

    fn step(&mut self, rms: f32, frame: Vec<f32>) -> Option<Vec<f32>> {
        if !self.in_speech {
            if rms > self.threshold {
                self.voiced_run += frame.len();
                self.pending.extend_from_slice(&frame);
                if self.voiced_run >= self.min_speech_samples {
                    self.in_speech = true;
                    self.silent_run = 0;
                    self.segment = std::mem::take(&mut self.pending);
                }
            } else {
                self.voiced_run = 0;
                self.pending.clear();
            }
            return None;
        }

        self.segment.extend_from_slice(&frame);
        // Force-emit at the cap: better to transcribe the first 30s than to
        // buffer forever waiting for silence that may be minutes away.
        if self.segment.len() >= self.max_segment_samples {
            warn!(
                samples = self.segment.len(),
                "VAD segment hit the {MAX_SEGMENT_SECS}s cap; force-emitting"
            );
            telemetry::emit(
                "audio",
                "vad.segment_capped",
                json!({"samples": self.segment.len(), "cap_secs": MAX_SEGMENT_SECS}),
            );
            return Some(self.finish_segment());
        }
        if rms > self.threshold {
            self.silent_run = 0;
            return None;
        }
        self.silent_run += frame.len();
        if self.silent_run < self.silence_limit_samples {
            return None;
        }
        Some(self.finish_segment())
    }

    fn finish_segment(&mut self) -> Vec<f32> {
        self.in_speech = false;
        self.voiced_run = 0;
        self.silent_run = 0;
        self.counter += 1;
        std::mem::take(&mut self.segment)
    }

    fn take_counter(&self) -> u64 {
        self.counter
    }

    /// Discard any in-progress capture state (used while DARWIN speaks).
    fn reset(&mut self) {
        self.frame.clear();
        self.pending.clear();
        self.segment.clear();
        self.in_speech = false;
        self.voiced_run = 0;
        self.silent_run = 0;
    }
}

/// The capture gate's verdict for one mic chunk.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Gate {
    /// Discard the chunk (and reset the VAD): DARWIN is speaking, or his echo
    /// has not yet settled after a barge.
    Drop,
    /// Feed the chunk to the VAD: a genuine user-capture window.
    Capture,
}

/// PURE capture-gate decision (RC-1), factored out of the realtime loop so the
/// echo-safety invariant is unit-testable without a mic.
///
/// The ONLY way to reach `Capture` is: DARWIN is NOT speaking, AND either no
/// barge is pending, OR a barge fired and the echo-settle window has fully
/// elapsed since it armed. A barge alone NEVER opens the gate — that was the
/// echo-feedback bug (the gate reopened while is_speaking() was still true
/// through the reply's MUTE_TAIL, so DARWIN's own draining audio + room echo
/// was captured, transcribed, and re-routed, re-running the action).
///
/// `since_barge` is `None` when no barge is pending, else the time elapsed
/// since the barge armed. `settle` is BARGE_SETTLE_MS as a Duration.
fn gate_decision(speaking: bool, since_barge: Option<Duration>, settle: Duration) -> Gate {
    if speaking {
        return Gate::Drop;
    }
    match since_barge {
        Some(elapsed) if elapsed < settle => Gate::Drop,
        _ => Gate::Capture,
    }
}

/// PURE mic-suppression decision for the LOCKDOWN overlay (task #12), factored
/// out of the realtime loop so the "panic silences the mic" invariant is
/// unit-testable without a microphone. `locked` is
/// [`crate::lockdown::is_locked_down`] at the top of the loop.
///
/// Returns `true` when the chunk must be DROPPED (and the VAD reset) because the
/// emergency stop is engaged — checked FIRST in the loop, before every other gate,
/// so no audio is metered, segmented, or transcribed while locked. Returns
/// `false` (capture proceeds normally) when unlocked — the shipped default, so
/// capture is byte-for-byte today.
fn mic_capture_suppressed(locked: bool) -> bool {
    locked
}

/// Mono RMS of one interleaved chunk (channels averaged) — for barge-in
/// detection. One cheap pass, no allocation.
fn chunk_rms(chunk: &[f32], channels: usize) -> f32 {
    let ch = channels.max(1);
    let mut sum = 0.0f64;
    let mut n = 0usize;
    for frame in chunk.chunks(ch) {
        let mono = frame.iter().copied().sum::<f32>() / frame.len() as f32;
        sum += f64::from(mono) * f64::from(mono);
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64).sqrt() as f32
    }
}

/// Watches the mic DURING DARWIN's playback for the user barging in. Fires once
/// the ACCUMULATED loud time (above the effective threshold) reaches
/// `dwell_samples`. Two echo-safety guards (RC-8) keep DARWIN's OWN voice from
/// tripping it, since his echo through the speakers has syllable gaps shorter
/// than the dip-tolerance window and would otherwise slowly integrate to dwell:
///
///   1. ADAPTIVE THRESHOLD. While dropped frames stream by, the detector tracks
///      a rolling baseline of the mic RMS it sees (DARWIN's echo level) and
///      requires barge frames to exceed `baseline + margin`, not just the fixed
///      configured floor — so a louder-than-expected reply cannot creep over a
///      static threshold. The effective threshold is `max(configured, baseline
///      + margin)`.
///   2. CONTIGUOUS ARMING BURST. Dip tolerance only engages AFTER one
///      un-bridged run of `arm_samples` over-threshold audio. Steady-state echo
///      (loud/quiet/loud at syllable cadence) never produces that contiguous
///      burst, so it can never reach the dip-tolerant accumulation phase. A real
///      interruption — a person actually talking over DARWIN — clears the short
///      arming burst easily, after which the original late-fire-fixing dip
///      tolerance applies unchanged.
///
/// `threshold`/dwell/quiet-reset constants stay device-gated; only the state
/// machine around them is changed here.
struct BargeDetector {
    enabled: bool,
    threshold: f32,
    dwell_samples: usize,
    quiet_reset_samples: usize,
    arm_samples: usize,
    /// Rolling baseline of observed echo RMS while DARWIN speaks; barge frames
    /// must clear `baseline + BASELINE_MARGIN` as well as the fixed threshold.
    baseline: f32,
    /// Whether the contiguous arming burst has been seen this run; until then
    /// only contiguous loud counts (no dip tolerance).
    armed: bool,
    /// Contiguous over-threshold run toward the arming burst (reset by any dip).
    arm_run: usize,
    loud_run: usize,
    quiet_run: usize,
}

impl BargeDetector {
    /// Contiguous quiet that counts as "the user stopped" and resets the loud
    /// accumulator. Longer than an inter-word gap, shorter than a real pause.
    const RESET_QUIET_MS: usize = 220;
    /// Contiguous over-threshold audio required to ARM dip-tolerant
    /// accumulation. Long enough that DARWIN's syllable-gapped echo never
    /// sustains it, short enough that a real interruption clears it instantly.
    const ARM_BURST_MS: usize = 120;
    /// How much a barge frame must exceed the rolling echo baseline. Keeps the
    /// detector adaptive when DARWIN is louder than the static threshold.
    const BASELINE_MARGIN: f32 = 0.02;
    /// EMA weight for the echo baseline (per chunk). Small: a slow average so a
    /// single loud transient does not yank the baseline up and self-suppress.
    const BASELINE_ALPHA: f32 = 0.05;

    fn new(cfg: &Config, sample_rate: u32) -> Self {
        let per_ms = sample_rate as usize / 1000;
        Self {
            enabled: cfg.audio.barge_in,
            threshold: cfg.audio.barge_in_rms as f32,
            dwell_samples: (per_ms * cfg.audio.barge_in_ms as usize).max(1),
            quiet_reset_samples: (per_ms * Self::RESET_QUIET_MS).max(1),
            arm_samples: (per_ms * Self::ARM_BURST_MS).max(1),
            baseline: 0.0,
            armed: false,
            arm_run: 0,
            loud_run: 0,
            quiet_run: 0,
        }
    }

    /// Update the rolling echo baseline with one observed RMS. Called for every
    /// dropped frame while DARWIN speaks, so the baseline tracks his echo level.
    fn observe_baseline(&mut self, rms: f32) {
        self.baseline += Self::BASELINE_ALPHA * (rms - self.baseline);
    }

    /// The threshold a frame must clear to count as loud: the larger of the
    /// configured floor and the adaptive `baseline + margin`.
    fn effective_threshold(&self) -> f32 {
        self.threshold.max(self.baseline + Self::BASELINE_MARGIN)
    }

    /// Feed a chunk's mono RMS + its frame count; returns true once the user has
    /// accumulated `dwell_samples` of loud audio. Dip tolerance applies ONLY
    /// after a contiguous arming burst (guard 2); the effective threshold is
    /// adaptive (guard 1). A disabled detector never fires.
    fn observe(&mut self, rms: f32, frames: usize) -> bool {
        if !self.enabled {
            return false;
        }
        let loud = rms > self.effective_threshold();
        if loud {
            // Arming phase: require ONE contiguous over-threshold burst before
            // dip-tolerant accumulation engages, so syllable-gapped echo never
            // integrates to dwell.
            if !self.armed {
                self.arm_run += frames;
                if self.arm_run >= self.arm_samples {
                    self.armed = true;
                    // Seed the dwell accumulator with the arming burst so a long
                    // single shout still fires at the same total dwell.
                    self.loud_run = self.arm_run;
                }
                self.quiet_run = 0;
                return self.armed && self.loud_run >= self.dwell_samples;
            }
            self.loud_run += frames;
            self.quiet_run = 0;
            self.loud_run >= self.dwell_samples
        } else {
            // A dip below threshold. Before arming, ANY dip breaks the
            // contiguous burst (this is what rejects syllable-gapped echo).
            // After arming, only a SUSTAINED quiet resets the run, so genuine
            // inter-word gaps don't keep zeroing it (the late-fire fix).
            if !self.armed {
                self.arm_run = 0;
                return false;
            }
            self.quiet_run += frames;
            if self.quiet_run >= self.quiet_reset_samples {
                self.loud_run = 0;
                self.armed = false;
                self.arm_run = 0;
            }
            false
        }
    }

    fn reset(&mut self) {
        self.loud_run = 0;
        self.quiet_run = 0;
        self.arm_run = 0;
        self.armed = false;
        // Baseline is intentionally NOT reset: it is a property of the room/
        // device echo, persisting across replies for a stable adaptive floor.
    }
}

/// Accumulates mono RMS between rate-limited audio.level emissions. The
/// capture loop feeds every raw chunk through `push_frames`; the meter only
/// surfaces a value once LEVEL_INTERVAL has elapsed since the last one, so
/// the telemetry WS sees at most ~15 events/s no matter the device's chunk
/// cadence. Time is an explicit parameter so the rate limit is unit-testable
/// without sleeping.
struct LevelMeter {
    last_emit: Instant,
    sum_squares: f64,
    samples: usize,
}

impl LevelMeter {
    fn new() -> Self {
        Self {
            last_emit: Instant::now(),
            sum_squares: 0.0,
            samples: 0,
        }
    }

    /// Feed one interleaved chunk (downmixed to mono internally, matching the
    /// VAD's averaging downmix). Returns the RMS over everything accumulated
    /// since the last emission — rounded to 4 decimal places — once at least
    /// LEVEL_INTERVAL has elapsed; None otherwise.
    fn push_frames(&mut self, chunk: &[f32], channels: usize, now: Instant) -> Option<f64> {
        for frame in chunk.chunks(channels.max(1)) {
            let mono = frame.iter().copied().sum::<f32>() / frame.len() as f32;
            self.sum_squares += f64::from(mono) * f64::from(mono);
            self.samples += 1;
        }
        if self.samples == 0 || now.duration_since(self.last_emit) < LEVEL_INTERVAL {
            return None;
        }
        let rms = (self.sum_squares / self.samples as f64).sqrt();
        self.last_emit = now;
        self.sum_squares = 0.0;
        self.samples = 0;
        Some(round4(rms))
    }
}

/// Round to 4 decimal places. Done in f64 so the JSON wire value is the
/// clean shortest representation (an f32 routed through serde_json's f64
/// conversion would serialize as e.g. 0.012299999594688416).
fn round4(v: f64) -> f64 {
    (v * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::{round4, LevelMeter, LEVEL_INTERVAL};
    use std::time::{Duration, Instant};

    fn meter_at(origin: Instant) -> LevelMeter {
        LevelMeter {
            last_emit: origin,
            sum_squares: 0.0,
            samples: 0,
        }
    }

    #[test]
    fn level_meter_rate_limits_to_the_interval() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        let chunk = [0.5f32; 480];
        // Inside the interval: accumulate silently.
        assert_eq!(meter.push_frames(&chunk, 1, t0 + Duration::from_millis(10)), None);
        assert_eq!(meter.push_frames(&chunk, 1, t0 + Duration::from_millis(65)), None);
        // Interval elapsed: the accumulated window is emitted...
        let rms = meter
            .push_frames(&chunk, 1, t0 + Duration::from_millis(66))
            .expect("emit at the interval boundary");
        assert_eq!(rms, 0.5);
        // ...and the clock restarts: the very next chunk is silent again.
        assert_eq!(meter.push_frames(&chunk, 1, t0 + Duration::from_millis(70)), None);
        assert!(meter
            .push_frames(&chunk, 1, t0 + Duration::from_millis(66) + LEVEL_INTERVAL)
            .is_some());
    }

    #[test]
    fn level_meter_rms_covers_the_whole_window_and_resets() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        // Half the window silent, half at 0.8: rms = sqrt(0.8^2 / 2).
        assert_eq!(meter.push_frames(&[0.0f32; 100], 1, t0), None);
        let rms = meter
            .push_frames(&[0.8f32; 100], 1, t0 + LEVEL_INTERVAL)
            .expect("emit");
        let expected = round4((0.8f64 * 0.8 * 100.0 / 200.0).sqrt());
        assert_eq!(rms, expected);
        // Accumulators were reset: a silent follow-up window reads 0.
        let rms = meter
            .push_frames(&[0.0f32; 100], 1, t0 + LEVEL_INTERVAL * 2)
            .expect("emit");
        assert_eq!(rms, 0.0);
    }

    #[test]
    fn level_meter_downmixes_interleaved_channels_by_averaging() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        // Stereo frames [1.0, 0.0] average to mono 0.5 -> rms 0.5.
        let chunk = [1.0f32, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        let rms = meter
            .push_frames(&chunk, 2, t0 + LEVEL_INTERVAL)
            .expect("emit");
        assert_eq!(rms, 0.5);
    }

    #[test]
    fn level_meter_emits_nothing_for_an_empty_window() {
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        assert_eq!(meter.push_frames(&[], 1, t0 + LEVEL_INTERVAL * 10), None);
    }

    #[test]
    fn level_meter_treats_zero_channels_as_mono() {
        // channels=0 must not panic (chunks(0) would); it clamps to 1.
        let t0 = Instant::now();
        let mut meter = meter_at(t0);
        let rms = meter
            .push_frames(&[0.25f32; 16], 0, t0 + LEVEL_INTERVAL)
            .expect("emit");
        assert_eq!(rms, 0.25);
    }

    #[test]
    fn round4_rounds_to_four_decimal_places() {
        assert_eq!(round4(0.123456), 0.1235);
        assert_eq!(round4(0.00004), 0.0);
        assert_eq!(round4(0.00005), 0.0001);
        assert_eq!(round4(1.0), 1.0);
        assert_eq!(round4(0.0), 0.0);
    }

    #[test]
    fn chunk_rms_downmixes_and_measures() {
        use super::chunk_rms;
        // Mono full-scale -> rms 1.0.
        assert!((chunk_rms(&[1.0, 1.0, 1.0], 1) - 1.0).abs() < 1e-6);
        // Stereo [1,0] frames average to mono 0.5 -> rms 0.5.
        assert!((chunk_rms(&[1.0, 0.0, 1.0, 0.0], 2) - 0.5).abs() < 1e-6);
        // Empty chunk -> 0.0, no divide-by-zero panic.
        assert_eq!(chunk_rms(&[], 1), 0.0);
    }

    /// Build a detector with explicit timing samples for tests, bypassing the
    /// Config-derived `new()`. `arm` is the contiguous arming burst in samples.
    fn detector(enabled: bool, threshold: f32, dwell: usize, quiet_reset: usize, arm: usize) -> super::BargeDetector {
        super::BargeDetector {
            enabled,
            threshold,
            dwell_samples: dwell,
            quiet_reset_samples: quiet_reset,
            arm_samples: arm,
            baseline: 0.0,
            armed: false,
            arm_run: 0,
            loud_run: 0,
            quiet_run: 0,
        }
    }

    #[test]
    fn barge_detector_accumulates_loud_and_tolerates_inter_word_dips() {
        // dwell 1000, quiet-reset 500, threshold 0.06 (above DARWIN's ~0.04
        // echo), arm 200 (a short contiguous burst arms dip tolerance).
        let mut d = detector(true, 0.06, 1000, 500, 200);
        // Echo-level frames (below threshold) never accumulate or arm.
        assert!(!d.observe(0.04, 480));
        assert!(!d.observe(0.05, 480));
        // First contiguous loud burst (>= arm) ARMS dip tolerance and seeds the
        // dwell accumulator; loud_run = 400 < dwell, so no fire yet.
        assert!(!d.observe(0.12, 400)); // armed, loud_run = 400
        assert!(!d.observe(0.03, 300)); // brief dip (300 < 500): run kept (armed)
        assert!(d.observe(0.12, 600)); // loud_run = 1000 -> FIRES
        // A SUSTAINED quiet (>= quiet-reset) means the user stopped: reset +
        // disarm. The next loud run must re-arm before it can fire.
        d.reset();
        assert!(!d.observe(0.12, 400)); // armed, loud_run = 400
        assert!(!d.observe(0.02, 500)); // sustained quiet -> reset + disarm
        assert!(!d.observe(0.12, 600)); // re-armed, loud_run = 600 < 1000
        // Disabled never fires.
        let mut off = detector(false, 0.0, 1, 1, 1);
        assert!(!off.observe(1.0, 1_000_000));
    }

    /// RC-8: DARWIN's own echo at syllable cadence (loud/quiet/loud, each gap
    /// shorter than the dip-reset but the loud runs shorter than the arming
    /// burst) must NEVER arm, so it can never integrate to a false barge — the
    /// mechanism that fired the detector on DARWIN himself and caused the
    /// echo-feedback triple-open. A sustained run DOES fire.
    #[test]
    fn barge_detector_rejects_syllable_cadence_echo_but_fires_on_a_sustained_run() {
        // arm 200: needs a 200-sample CONTIGUOUS loud burst to engage dip
        // tolerance. dwell 1000, quiet-reset 500, threshold 0.06.
        let mut d = detector(true, 0.06, 1000, 500, 200);
        // Syllable cadence: 100-sample loud bursts (each < arm) split by 150ms
        // dips. Each dip breaks the (unarmed) contiguous run, so it never arms
        // and never accumulates — no matter how many cycles.
        for _ in 0..50 {
            assert!(!d.observe(0.12, 100), "loud burst < arm must not fire");
            assert!(!d.observe(0.01, 150), "dip resets the unarmed burst");
        }
        assert!(!d.armed, "syllable-cadence echo must never arm the detector");

        // A genuine sustained interruption: one long contiguous loud run arms
        // (>= 200) and reaches dwell (>= 1000), firing as it should.
        let mut g = detector(true, 0.06, 1000, 500, 200);
        assert!(!g.observe(0.2, 300)); // arms (300 >= 200), loud_run = 300
        assert!(g.observe(0.2, 700)); // loud_run = 1000 -> FIRES
    }

    /// RC-8 guard 1: the threshold is adaptive — a frame must clear both the
    /// fixed floor and `baseline + margin`. A loud reply that drives the
    /// baseline up must not let same-level echo trip the detector.
    #[test]
    fn barge_detector_threshold_adapts_to_the_echo_baseline() {
        let mut d = detector(true, 0.06, 1000, 500, 200);
        // Drive the rolling baseline up to ~0.15 (a loud reply through the
        // speakers) by feeding the echo level repeatedly.
        for _ in 0..200 {
            d.observe_baseline(0.15);
        }
        // effective_threshold is now max(0.06, baseline + 0.02) > 0.16, so a
        // 0.15 echo frame is BELOW it and cannot accumulate.
        assert!(d.effective_threshold() > 0.15, "baseline must raise the floor");
        assert!(!d.observe(0.15, 1_000_000), "echo at the baseline must not fire");
        // A frame well above the adaptive floor still works normally.
        assert!(d.observe(0.30, 1000), "a frame above the adaptive floor fires");
    }
}

#[cfg(test)]
mod gate_tests {
    use super::{gate_decision, mic_capture_suppressed, Gate};
    use std::time::Duration;

    /// RC-1, the echo-safety invariant. The capture gate Drops every frame
    /// while DARWIN speaks AND through the echo-settle window after a barge;
    /// only a clean non-speaking, settled window Captures. A barge alone never
    /// opens the gate.
    #[test]
    fn gate_drops_while_speaking_and_through_the_settle_window() {
        let settle = Duration::from_millis(300);

        // Speaking: ALWAYS Drop, regardless of barge state. This is what makes
        // DARWIN's own draining audio impossible to re-capture.
        assert_eq!(gate_decision(true, None, settle), Gate::Drop);
        assert_eq!(gate_decision(true, Some(Duration::ZERO), settle), Gate::Drop);
        assert_eq!(
            gate_decision(true, Some(Duration::from_secs(10)), settle),
            Gate::Drop,
            "even long after a barge, a speaking DARWIN is never captured"
        );

        // Not speaking, no barge pending: a normal capture window.
        assert_eq!(gate_decision(false, None, settle), Gate::Capture);

        // Not speaking, barge pending, INSIDE the settle window: still Drop —
        // this is exactly the residual-echo span that re-ran the action.
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(0)), settle),
            Gate::Drop
        );
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(299)), settle),
            Gate::Drop
        );

        // Not speaking, barge pending, settle window ELAPSED: now a genuine
        // user-capture window.
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(300)), settle),
            Gate::Capture
        );
        assert_eq!(
            gate_decision(false, Some(Duration::from_millis(900)), settle),
            Gate::Capture
        );
    }

    /// LOCKDOWN overlay (task #12): the mic-suppression decision drops every
    /// chunk while locked and captures normally when unlocked — the pure half of
    /// the capture loop's "panic silences the mic" check.
    #[test]
    fn mic_suppressed_only_while_locked() {
        assert!(mic_capture_suppressed(true), "locked => drop the chunk (mic silenced)");
        assert!(
            !mic_capture_suppressed(false),
            "unlocked (shipped default) => capture proceeds byte-for-byte today"
        );
    }
}

#[cfg(test)]
mod app_ingest_tests {
    use super::{
        decode_frame, encode_frame, mic_source_is_app, read_app_frames, read_full_or_eof,
        ReadFrame, MAX_FRAME_SAMPLES,
    };
    use std::sync::mpsc as std_mpsc;

    /// PURE roundtrip: encode samples into a length-prefixed frame, strip the
    /// 4-byte prefix, and decode the payload back to the EXACT same f32s. This is
    /// the wire-contract invariant the HUD relies on (LE u32 count + LE f32s).
    #[test]
    fn frame_payload_roundtrips_through_decode() {
        let samples = vec![0.0f32, 1.0, -1.0, 0.5, -0.25, f32::MIN_POSITIVE, 123.456];
        let frame = encode_frame(&samples);
        // The prefix is the LE sample count.
        let n = u32::from_le_bytes([frame[0], frame[1], frame[2], frame[3]]);
        assert_eq!(n as usize, samples.len(), "prefix encodes the sample count");
        assert_eq!(frame.len(), 4 + samples.len() * 4, "frame is prefix + 4 bytes/sample");
        // The payload decodes back bit-for-bit.
        let decoded = decode_frame(&frame[4..]);
        assert_eq!(decoded, samples, "decode_frame inverts encode_frame's payload");
    }

    /// An empty frame (N = 0) is a valid frame: prefix only, empty payload.
    #[test]
    fn empty_frame_roundtrips() {
        let frame = encode_frame(&[]);
        assert_eq!(frame, vec![0u8, 0, 0, 0], "N=0 prefix, no payload");
        assert!(decode_frame(&frame[4..]).is_empty());
    }

    /// decode_frame drops a malformed trailing partial sample (a guard — a
    /// well-formed payload is always a multiple of 4 bytes).
    #[test]
    fn decode_drops_a_partial_trailing_sample() {
        // 5 bytes = one full f32 (1.0) plus one stray byte.
        let mut payload = 1.0f32.to_le_bytes().to_vec();
        payload.push(0xAB);
        let decoded = decode_frame(&payload);
        assert_eq!(decoded, vec![1.0f32], "the stray trailing byte is dropped");
    }

    /// read_full_or_eof reports a CLEAN boundary EOF (no bytes) as Eof, a full
    /// read as Got, and a PARTIAL read (truncated prefix) as an error.
    #[test]
    fn read_full_or_eof_distinguishes_clean_eof_from_truncation() {
        // Clean EOF: empty reader, asking for 4 bytes.
        let mut empty: &[u8] = &[];
        let mut buf = [0u8; 4];
        assert!(matches!(read_full_or_eof(&mut empty, &mut buf), Ok(ReadFrame::Eof)));

        // Full read: exactly 4 bytes available.
        let mut full: &[u8] = &[1, 2, 3, 4];
        assert!(matches!(read_full_or_eof(&mut full, &mut buf), Ok(ReadFrame::Got)));
        assert_eq!(buf, [1, 2, 3, 4]);

        // Truncated: 2 bytes then EOF while a 4-byte prefix is expected => error.
        let mut partial: &[u8] = &[9, 9];
        assert!(read_full_or_eof(&mut partial, &mut buf).is_err());
    }

    /// read_app_frames decodes a back-to-back stream of frames into raw_tx and
    /// returns Ok on a clean EOF at a frame boundary.
    #[test]
    fn read_app_frames_decodes_a_stream_to_eof() {
        let f1 = vec![0.1f32, 0.2, 0.3];
        let f2 = vec![-0.5f32, 0.5];
        let mut wire = encode_frame(&f1);
        wire.extend(encode_frame(&f2));
        let mut cursor: &[u8] = &wire;
        let (tx, rx) = std_mpsc::channel::<Vec<f32>>();
        let res = read_app_frames(&mut cursor, &tx);
        drop(tx);
        assert!(res.is_ok(), "a clean EOF at a frame boundary is Ok");
        let got: Vec<Vec<f32>> = rx.iter().collect();
        assert_eq!(got, vec![f1, f2], "both frames decoded in order into raw_tx");
    }

    /// A frame whose length prefix exceeds the cap is rejected as an error and
    /// nothing is pushed — the daemon never attempts the huge allocation.
    #[test]
    fn read_app_frames_rejects_an_oversized_prefix() {
        let mut wire = (MAX_FRAME_SAMPLES + 1).to_le_bytes().to_vec();
        // No payload needed: the prefix check fires before any payload read.
        wire.extend_from_slice(&[0u8; 4]);
        let mut cursor: &[u8] = &wire;
        let (tx, rx) = std_mpsc::channel::<Vec<f32>>();
        let res = read_app_frames(&mut cursor, &tx);
        drop(tx);
        assert!(res.is_err(), "an over-cap prefix is an error, not an allocation");
        assert!(rx.iter().next().is_none(), "nothing is ingested from a rejected frame");
    }

    /// A truncated payload (prefix promises more samples than the stream has) is
    /// an error — not a silent short frame.
    #[test]
    fn read_app_frames_errors_on_a_truncated_payload() {
        let mut wire = 4u32.to_le_bytes().to_vec(); // promises 4 samples = 16 bytes
        wire.extend_from_slice(&[0u8; 8]); // only 8 bytes of payload present
        let mut cursor: &[u8] = &wire;
        let (tx, _rx) = std_mpsc::channel::<Vec<f32>>();
        assert!(read_app_frames(&mut cursor, &tx).is_err());
    }

    /// mic_source_is_app selects the socket ingest ONLY for the exact "app"; the
    /// default "device", any typo, and the empty string all stay on cpal — a
    /// mistyped value never disables the device path.
    #[test]
    fn mic_source_app_is_exact_match_only() {
        assert!(mic_source_is_app("app"), "\"app\" selects the socket ingest");
        assert!(!mic_source_is_app("device"), "the default stays on cpal");
        assert!(!mic_source_is_app("App"), "case-sensitive: a typo stays on cpal");
        assert!(!mic_source_is_app(""), "empty stays on cpal (safe default)");
        assert!(!mic_source_is_app("socket"), "an unknown value stays on cpal");
    }
}
