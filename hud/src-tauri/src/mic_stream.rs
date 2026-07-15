//! IN-PROCESS microphone capture → daemon app-audio ingest (the HUD side).
//!
//! WHY this lives in the HUD, not the daemon: only `DARWIN.app` carries
//! `NSMicrophoneUsageDescription` + the audio-input entitlement, so opening the
//! mic from THIS process is the only way macOS shows a clean "DARWIN" prompt
//! (the daemon opening cpal would surface an opaque/again-and-again prompt or be
//! denied). So we capture here and STREAM the raw f32 PCM to the daemon over a
//! local Unix socket; the daemon feeds those exact `Vec<f32>` chunks into the
//! SAME capture-processing path its cpal callback used to produce.
//!
//! WIRE CONTRACT (must match `daemon/src/...` app-audio ingest byte-for-byte):
//!   - socket: `<root>/state/ipc/audio_in.sock` (daemon binds; 0700 dir / 0600 sock)
//!   - HANDSHAKE: exactly one '\n'-terminated UTF-8 JSON line:
//!     {"token":"<command.token>","sample_rate":<u32>,"channels":<u16>}
//!     verified daemon-side with `apps::verify_command_token` — the SAME per-boot
//!     HMAC capability token the command channel uses (read here from
//!     `state/ipc/command.token`, reusing `command::read_token`).
//!   - FRAMES (repeated, after the handshake line):
//!     [4 bytes u32 LE = N, the f32 sample count][N * 4 bytes f32 LE samples]
//!     i.e. exactly what [`encode_frame`] produces — the inverse of the daemon's
//!     parse. On EOF / write error the daemon stops ingest for the connection.
//!
//! ORDERING is load-bearing for the clean prompt: we QUERY the default input
//! config (sample_rate + channels) WITHOUT opening the stream, then CONNECT to
//! the socket; ONLY on a successful connect do we build+play the cpal stream
//! (the act that fires the mic prompt). If the daemon is not in app-mode / not
//! listening, the connect fails and we return cleanly WITHOUT ever touching the
//! mic — the prompt fires only when the daemon actually wants the audio.
//!
//! `cpal::Stream` is `!Send` (see `daemon/src/audio.rs spawn_capture`'s note), so
//! the Stream + its socket live on a DEDICATED owner thread and are never held
//! across an `.await`. The Tauri-managed [`MicState`] only holds a control handle
//! (a stop channel + the join handle); teardown signals that thread.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;

/// Honest outcome of a start/stop request, returned to the UI as-is.
#[derive(Debug, Clone, Serialize)]
pub struct MicStatus {
    /// True iff a live capture stream is running and writing to the daemon.
    pub streaming: bool,
    /// Human-readable, secret-free explanation (never carries the token).
    pub detail: String,
}

impl MicStatus {
    fn on(detail: impl Into<String>) -> Self {
        Self { streaming: true, detail: detail.into() }
    }
    fn off(detail: impl Into<String>) -> Self {
        Self { streaming: false, detail: detail.into() }
    }
}

/// Tauri-managed handle to the (at most one) capture owner thread. Holds ONLY a
/// stop signal + the join handle — never the `!Send` Stream itself, which stays
/// on the owner thread. `Mutex<Option<…>>` makes start/stop idempotent and
/// serialized: starting while live is a no-op-ish re-report; stopping when idle
/// is a clean no-op.
#[derive(Default)]
pub struct MicState {
    inner: Mutex<Option<Running>>,
}

struct Running {
    /// Dropping this (or sending) tells the owner thread to tear down.
    stop: std_mpsc::Sender<()>,
    handle: std::thread::JoinHandle<()>,
}

/// Encode ONE capture chunk into the wire frame the daemon parses:
/// `[u32 LE sample count][f32 LE samples]`. PURE + unit-tested; the exact
/// inverse of the daemon's frame parse (read a u32 LE count, then count*4 bytes
/// of f32 LE). Keep these two in lockstep.
fn encode_frame(samples: &[f32]) -> Vec<u8> {
    let n = samples.len() as u32;
    let mut out = Vec::with_capacity(4 + samples.len() * 4);
    out.extend_from_slice(&n.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Build the '\n'-terminated handshake line. PURE + unit-tested. The token is
/// embedded here and this line is NEVER logged.
fn handshake_line(token: &str, sample_rate: u32, channels: u16) -> String {
    // serde_json escapes the token correctly; field order matches the daemon's
    // struct only by name (it parses JSON, not positional), but we keep the
    // documented order for readability.
    let v = serde_json::json!({
        "token": token,
        "sample_rate": sample_rate,
        "channels": channels,
    });
    format!("{v}\n")
}

/// Path of the app-audio ingest socket: `<root>/state/ipc/audio_in.sock`.
fn audio_socket_path(root: &Path) -> std::path::PathBuf {
    root.join("state").join("ipc").join("audio_in.sock")
}

/// Start in-process capture and stream PCM to the daemon. See module docs for
/// the strict ORDERING (query config → connect → only then open the mic). Never
/// opens the mic if the daemon is not listening.
#[tauri::command]
pub fn start_mic_stream(state: tauri::State<'_, MicState>) -> MicStatus {
    let mut guard = match state.inner.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };

    // Already streaming? Idempotent: report the live state without re-prompting.
    if let Some(run) = guard.as_ref() {
        if !run.handle.is_finished() {
            return MicStatus::on("already streaming");
        }
    }
    // A finished/dead owner thread is stale; clear it before (re)starting.
    if guard.as_ref().map(|r| r.handle.is_finished()).unwrap_or(false) {
        *guard = None;
    }

    // 1) Resolve root + read the capability token (reusing the command channel's
    //    single token read — no path duplication). A missing token means the
    //    daemon hasn't finished its handoff; bail cleanly, no mic.
    let root = match crate::heal::resolve_root_for_command() {
        Ok(r) => r,
        Err(e) => return MicStatus::off(format!("cannot resolve DARWIN root: {e}")),
    };
    let token = match crate::command::read_token(&root) {
        Ok(t) => t,
        Err(e) => return MicStatus::off(e),
    };

    // 2) QUERY the default input device's default config WITHOUT opening a
    //    stream. Querying config does NOT fire the mic prompt; building+playing
    //    the stream does, and we defer that until after a successful connect.
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => return MicStatus::off("no default input device"),
    };
    let supported = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => return MicStatus::off(format!("querying default input config failed: {e}")),
    };
    // cpal 0.18: `sample_rate()` returns the raw `u32` (no `SampleRate` newtype).
    let sample_rate = supported.sample_rate();
    let channels = supported.channels();

    // 3) CONNECT to the daemon's ingest socket. If this fails, the daemon is not
    //    in app-mode / not listening — return cleanly and DO NOT open the mic, so
    //    the prompt fires only when the daemon actually wants the audio.
    let sock_path = audio_socket_path(&root);
    let mut sock = match UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(_) => return MicStatus::off("daemon not accepting app audio"),
    };

    // 4) Send the handshake line BEFORE any audio. If the write fails the daemon
    //    won't ingest; bail without opening the mic. The line carries the token
    //    and is never logged.
    let line = handshake_line(&token, sample_rate, channels);
    if sock.write_all(line.as_bytes()).is_err() {
        return MicStatus::off("daemon not accepting app audio (handshake failed)");
    }
    let _ = sock.flush();

    // 5) Spawn the dedicated OWNER thread that holds the !Send Stream + the
    //    socket and lives for the duration of the capture. The cpal callback
    //    (realtime thread) must not block: it ships raw chunks over a std
    //    channel; the owner thread encodes + writes them to the socket. This is
    //    the act that fires the clean DARWIN mic prompt.
    let stream_config: cpal::StreamConfig = supported.into();
    let (stop_tx, stop_rx) = std_mpsc::channel::<()>();
    // Report channel: the owner thread tells us whether the stream actually
    // opened+played, so the command result is HONEST about the mic state.
    let (ready_tx, ready_rx) = std_mpsc::channel::<Result<(), String>>();

    let handle = match std::thread::Builder::new()
        .name("hud-mic-stream".to_string())
        .spawn(move || owner_thread(device, stream_config, sock, stop_rx, ready_tx))
    {
        Ok(h) => h,
        // Thread spawn is a fallible syscall (EAGAIN / resource exhaustion); surface
        // it as an honest off-status rather than panicking across the command boundary.
        Err(e) => return MicStatus::off(format!("could not start capture thread: {e}")),
    };

    // Wait briefly for the owner thread to report whether the stream opened. The
    // stream build/play is fast; if it errors we surface that and don't leave a
    // zombie "streaming" claim.
    match ready_rx.recv() {
        Ok(Ok(())) => {
            *guard = Some(Running { stop: stop_tx, handle });
            MicStatus::on("streaming microphone to daemon")
        }
        Ok(Err(e)) => {
            let _ = handle.join();
            MicStatus::off(format!("could not open microphone: {e}"))
        }
        Err(_) => {
            // Owner thread died before reporting — treat as failure.
            let _ = handle.join();
            MicStatus::off("could not open microphone (capture thread exited)")
        }
    }
}

/// The dedicated owner of the `!Send` cpal Stream + the socket. Builds + plays
/// the stream (firing the mic prompt), reports readiness, then blocks until a
/// stop signal arrives (or the stop sender is dropped). On exit the Stream and
/// the socket are dropped here, on this thread — never moved across threads.
fn owner_thread(
    device: cpal::Device,
    config: cpal::StreamConfig,
    sock: UnixStream,
    stop_rx: std_mpsc::Receiver<()>,
    ready_tx: std_mpsc::Sender<Result<(), String>>,
) {
    // The realtime cpal callback ships raw frames over this std channel; the
    // owner thread drains it and writes encoded frames to the socket, so the
    // audio thread never blocks on socket I/O.
    let (frame_tx, frame_rx) = std_mpsc::channel::<Vec<f32>>();

    // cpal 0.18: `build_input_stream` takes the config by value.
    let stream = match device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            // Best-effort: if the drain side is gone, drop the frame.
            let _ = frame_tx.send(data.to_vec());
        },
        |_err| { /* stream error: nothing safe to do from RT thread */ },
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e.to_string()));
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(e.to_string()));
        return;
    }
    // Stream is live — the prompt has fired (or access is already granted).
    let _ = ready_tx.send(Ok(()));

    let mut sock = sock;
    loop {
        // Stop requested (explicit signal or sender dropped)?
        match stop_rx.try_recv() {
            Ok(()) | Err(std_mpsc::TryRecvError::Disconnected) => break,
            Err(std_mpsc::TryRecvError::Empty) => {}
        }
        // Block for the next chunk with a small timeout so we re-check the stop
        // signal promptly even during silence.
        match frame_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(chunk) => {
                let bytes = encode_frame(&chunk);
                if sock.write_all(&bytes).is_err() {
                    // Daemon closed the connection / ingest stopped: tear down.
                    break;
                }
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // Explicit drop order: stop the Stream first (no more callbacks), then the
    // socket (signals EOF to the daemon). Both drop on THIS thread.
    drop(stream);
    drop(sock);
}

/// Tear down the capture stream + socket from managed state. Idempotent: a
/// stop with nothing running is a clean no-op.
#[tauri::command]
pub fn stop_mic_stream(state: tauri::State<'_, MicState>) -> MicStatus {
    let mut guard = match state.inner.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match guard.take() {
        None => MicStatus::off("not streaming"),
        Some(run) => {
            // Signal stop; ignore send error (thread may have already exited).
            let _ = run.stop.send(());
            // Join so the Stream + socket are fully dropped before we return.
            let _ = run.handle.join();
            MicStatus::off("stopped")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The frame encoding is the EXACT inverse of the daemon's parse:
    /// `[u32 LE count][count * f32 LE]`. This test pins both halves so the two
    /// sides cannot drift.
    #[test]
    fn encode_frame_matches_the_le_count_then_f32_le_contract() {
        let samples = vec![0.0f32, 1.0, -1.0, 0.5];
        let bytes = encode_frame(&samples);

        // 4-byte count + 4 bytes per sample.
        assert_eq!(bytes.len(), 4 + samples.len() * 4);

        // Count prefix is u32 LE.
        let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(count as usize, samples.len());

        // Each sample is f32 LE, in order — reparse and compare bit-for-bit.
        let mut decoded = Vec::new();
        for chunk in bytes[4..].chunks_exact(4) {
            decoded.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
        assert_eq!(decoded, samples);
    }

    #[test]
    fn encode_frame_empty_chunk_is_a_bare_zero_count() {
        let bytes = encode_frame(&[]);
        assert_eq!(bytes, vec![0u8, 0, 0, 0]);
        let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(count, 0);
    }

    #[test]
    fn encode_frame_uses_little_endian_not_native_assumption() {
        // 1.0f32 == 0x3F800000; LE byte order is [00, 00, 80, 3F].
        let bytes = encode_frame(&[1.0]);
        assert_eq!(&bytes[4..], &[0x00, 0x00, 0x80, 0x3F]);
    }

    #[test]
    fn handshake_line_carries_the_three_contract_fields_and_a_newline() {
        let line = handshake_line("tok-abc", 48_000, 2);
        assert!(line.ends_with('\n'), "handshake must be newline-terminated");
        let parsed: serde_json::Value =
            serde_json::from_str(line.trim_end()).expect("handshake is valid JSON");
        assert_eq!(parsed["token"], "tok-abc");
        assert_eq!(parsed["sample_rate"], 48_000);
        assert_eq!(parsed["channels"], 2);
        // Exactly these three keys — no stray fields on the wire.
        assert_eq!(parsed.as_object().unwrap().len(), 3);
    }

    #[test]
    fn handshake_line_escapes_token_so_it_stays_one_line() {
        // A token can never contain a raw newline (it's base64-ish), but if it
        // did, JSON escaping keeps the handshake to a single physical line.
        let line = handshake_line("a\"b", 16_000, 1);
        assert_eq!(line.matches('\n').count(), 1, "only the terminator newline");
        let parsed: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["token"], "a\"b");
    }

    #[test]
    fn audio_socket_path_is_under_state_ipc() {
        let p = audio_socket_path(Path::new("/darwin"));
        assert_eq!(p, Path::new("/darwin/state/ipc/audio_in.sock"));
    }
}
