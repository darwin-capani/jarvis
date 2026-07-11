//! UI ACTUATOR — the Tauri BACKEND side that POSTS the synthetic CGEvent IN THIS
//! app process, so macOS shows a clean "JARVIS would like to control this computer
//! using accessibility" prompt attributed to JARVIS.app (NOT the background
//! daemon, which is a bare LaunchAgent binary that cannot present that dialog or
//! hold a meaningful Accessibility grant).
//!
//! WHY THE HUD IS THE SERVER HERE (inverted from the command channel): macOS
//! attributes the Accessibility TCC consent to the process that POSTS the CGEvent.
//! Only `JARVIS.app` (a real bundle that can foreground a dialog) can surface the
//! clean prompt and hold the grant — so the HUD must be the one that posts. The
//! daemon stays the brain (it plans + gates + parks the actuation behind its
//! off-by-default switch and confirmation gate), but when it has a single
//! genuinely-approved actuation it hands it to THIS process to execute. Hence the
//! HUD BINDS+LISTENS and the daemon CONNECTS as the client.
//!
//! WIRE CONTRACT (must match the daemon's actuate client byte-for-byte):
//!   - socket: `<root>/state/ipc/actuate.sock` (HUD binds; 0700 dir / 0600 sock,
//!     mirroring `daemon::audio::bind_audio_socket` / the command channel bind).
//!   - REQUEST: the daemon connects and sends EXACTLY ONE '\n'-terminated UTF-8
//!     JSON line:
//!       {"token":"<command.token>","action":{…},"target_desc":"<string>"}
//!     where `action` is one of (internally-tagged on `kind`):
//!       {"kind":"click","x":<i32>,"y":<i32>}
//!       {"kind":"type","text":"<string>"}
//!       {"kind":"key","combo":"<string>"}
//!   - The HUD verifies `token` by a CONSTANT-TIME equality compare against the
//!     SAME per-boot capability token the command channel uses — read HERE via
//!     `crate::command::read_token`. A missing/empty/mismatched token => the HUD
//!     closes the connection and posts NOTHING.
//!   - The HUD posts EXACTLY ONE CGEvent for the single action (one request = one
//!     actuation = one connection) and replies ONE '\n'-terminated JSON line:
//!       {"ok":<bool>,"detail":"<string>"}
//!     `ok:false` with an HONEST detail when Accessibility is not granted
//!     (`AXIsProcessTrusted()==false`) or the post failed — NEVER a fabricated
//!     success.
//!
//! ACCESSIBILITY PROMPT: posting a CGEvent requires the Accessibility grant. The
//! HUD has no special entitlement for it, so the FIRST attempt (when not yet
//! granted) calls `AXIsProcessTrustedWithOptions` with the prompt option ONCE to
//! TRIGGER the clean "JARVIS would like to control this computer" dialog, then
//! replies honestly that it is not granted yet.
//!
//! SHAPE: [`parse_request`] (request narrowing — the EXACT inverse of the daemon's
//! encode), [`map_combo`] (combo → keycode+flags), and [`Reply::encode`] (reply
//! line) are PURE and unit-tested WITHOUT any socket or CGEvent. The socket accept
//! loop + the real CGEvent post are reached only by the live app, never a test.

use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

/// Cap on the one request line we read from the socket (matches the command
/// channel's `MAX_LINE_BYTES`). A request larger than this is rejected before any
/// parse — a `type` action's text is bounded by this.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// The single UI actuation the daemon asks the HUD to perform — EXACTLY ONE of
/// these per request (there is deliberately no batch/sequence variant, so the
/// one-request-one-actuation contract is structural). Mirrors the daemon's
/// `ui_automation::Action` enum, internally tagged on `kind` so the wire shape is
/// `{"kind":"click","x":..,"y":..}` etc.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// A single synthetic left click at one on-screen pixel.
    Click { x: i32, y: i32 },
    /// Typing ONE run of unicode text as a single `type` op (not a batch of
    /// per-key actions). Non-empty.
    Type { text: String },
    /// A single key combo (e.g. "cmd+s", "return", "escape"). Non-empty.
    Key { combo: String },
}

/// A fully-parsed, token-VERIFIED actuation request: the one action plus the
/// human-readable target description (telemetry / honesty only — never used to
/// locate anything; the daemon already resolved the action).
#[derive(Debug, Clone, PartialEq)]
pub struct ActuateRequest {
    pub action: Action,
    pub target_desc: String,
}

/// The one reply line the HUD writes back. `ok` mirrors whether the single CGEvent
/// was actually posted; `detail` is a short, secret-free human line. NEVER claims
/// success when Accessibility is not granted or the post failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reply {
    pub ok: bool,
    pub detail: String,
}

impl Reply {
    fn ok(detail: impl Into<String>) -> Self {
        Self { ok: true, detail: detail.into() }
    }
    fn fail(detail: impl Into<String>) -> Self {
        Self { ok: false, detail: detail.into() }
    }
    /// Serialize to the ONE '\n'-terminated JSON line the daemon reads back. PURE
    /// + unit-tested. serde_json escapes `detail`, so the reply is always a single
    /// physical line.
    pub fn encode(&self) -> String {
        // to_string never fails for this fixed `{ok:bool, detail:string}` shape.
        let body = serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"ok":false,"detail":"reply encode failed"}"#.to_string()
        });
        format!("{body}\n")
    }
}

/* --------------------------------------------------------- request parsing */

/// Narrow ONE request line into a token + [`ActuateRequest`]. The EXACT inverse of
/// the daemon's encode: `{"token":..,"action":{"kind":..,..},"target_desc":..}`.
/// PURE — unit-tested without any socket. Returns the verbatim token string (the
/// caller constant-time-compares it) and the typed action; an
/// unknown/missing/degenerate field is a structured `Err` (the caller closes the
/// connection and posts nothing). Never panics on malformed input.
pub fn parse_request(raw: &str) -> Result<(String, ActuateRequest), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("empty request".to_string());
    }
    let value: Value = serde_json::from_str(trimmed).map_err(|_| "malformed request".to_string())?;

    let token = value
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing token".to_string())?
        .to_string();
    if token.is_empty() {
        return Err("empty token".to_string());
    }

    let target_desc = value
        .get("target_desc")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let action_obj = value.get("action").ok_or_else(|| "missing action".to_string())?;
    let kind = action_obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing action kind".to_string())?;

    let action = match kind {
        "click" => {
            // x / y are required i32 pixels; the daemon already bounded them to the
            // real display, so we only narrow the type here (a non-integer is
            // malformed).
            let x = action_obj
                .get("x")
                .and_then(Value::as_i64)
                .ok_or_else(|| "click missing x".to_string())?;
            let y = action_obj
                .get("y")
                .and_then(Value::as_i64)
                .ok_or_else(|| "click missing y".to_string())?;
            if x < i32::MIN as i64 || x > i32::MAX as i64 || y < i32::MIN as i64 || y > i32::MAX as i64
            {
                return Err("click coordinate out of range".to_string());
            }
            Action::Click { x: x as i32, y: y as i32 }
        }
        "type" => {
            let text = action_obj
                .get("text")
                .and_then(Value::as_str)
                .ok_or_else(|| "type missing text".to_string())?;
            if text.is_empty() {
                return Err("type text is empty".to_string());
            }
            Action::Type { text: text.to_string() }
        }
        "key" => {
            let combo = action_obj
                .get("combo")
                .and_then(Value::as_str)
                .ok_or_else(|| "key missing combo".to_string())?;
            if combo.trim().is_empty() {
                return Err("key combo is empty".to_string());
            }
            Action::Key { combo: combo.to_string() }
        }
        other => return Err(format!("unknown action kind: {other}")),
    };

    Ok((token, ActuateRequest { action, target_desc }))
}

/* ----------------------------------------------------------- token compare */

/// CONSTANT-TIME byte equality. We compare the presented token against the token
/// the HUD itself read from `state/ipc/command.token` without an early-exit branch
/// on the first differing byte, so the compare time does not leak how many leading
/// bytes matched. PURE — unit-tested.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/* ------------------------------------------------------- combo → keycode map */

// CGEventFlags modifier masks (CGEventTypes.h) — the SAME bit values the
// `core-graphics` crate's `CGEventFlags` exposes and the daemon's mirror uses.
// Kept as bare consts so [`map_combo`] is pure and testable with NO framework.
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_ALTERNATE: u64 = 0x0008_0000; // option / alt
const FLAG_COMMAND: u64 = 0x0010_0000;

/// Map ONE combo string (e.g. `"cmd+s"`, `"return"`, `"shift+tab"`) to a
/// `(keycode, modifier_flags)` pair using the ANSI virtual keycodes
/// (`Carbon/HIToolbox/Events.h`). A BYTE-FOR-BYTE mirror of the daemon's
/// `ui_automation::map_combo` so both sides agree on every keycode + flag.
///
/// HONESTY OVER COMPLETENESS: if the base key cannot be mapped, returns `None` and
/// the caller posts NO event — it NEVER guesses a wrong key. Modifiers
/// (`cmd`/`command`/`ctrl`/`control`/`opt`/`option`/`alt`/`shift`) in any order are
/// folded into the flag mask; the LAST non-modifier token is the base key.
/// Case-insensitive. PURE — unit-tested.
pub fn map_combo(combo: &str) -> Option<(u16, u64)> {
    let mut flags: u64 = 0;
    let mut base: Option<u16> = None;
    for raw in combo.split('+') {
        let tok = raw.trim().to_ascii_lowercase();
        if tok.is_empty() {
            // A stray "+" with nothing around it is malformed — refuse honestly.
            return None;
        }
        match tok.as_str() {
            "cmd" | "command" | "super" | "win" | "meta" => flags |= FLAG_COMMAND,
            "ctrl" | "control" => flags |= FLAG_CONTROL,
            "opt" | "option" | "alt" => flags |= FLAG_ALTERNATE,
            "shift" => flags |= FLAG_SHIFT,
            other => {
                // Only one base key is allowed; a second non-modifier token is a
                // malformed combo (refuse honestly, do not guess).
                if base.is_some() {
                    return None;
                }
                base = Some(keycode_for(other)?);
            }
        }
    }
    base.map(|kc| (kc, flags))
}

/// ANSI virtual keycode for a single base-key token (lowercase). Mirrors the
/// daemon's `ui_automation::keycode_for`. Returns `None` for any token we cannot
/// map to the CORRECT keycode — the caller refuses honestly rather than post a
/// wrong key. PURE — unit-tested.
fn keycode_for(token: &str) -> Option<u16> {
    // Letters (kVK_ANSI_A …). Layout-position codes, not ASCII.
    let letter = |c: char| -> Option<u16> {
        Some(match c {
            'a' => 0x00, 's' => 0x01, 'd' => 0x02, 'f' => 0x03, 'h' => 0x04,
            'g' => 0x05, 'z' => 0x06, 'x' => 0x07, 'c' => 0x08, 'v' => 0x09,
            'b' => 0x0B, 'q' => 0x0C, 'w' => 0x0D, 'e' => 0x0E, 'r' => 0x0F,
            'y' => 0x10, 't' => 0x11, 'o' => 0x1F, 'u' => 0x20, 'i' => 0x22,
            'p' => 0x23, 'l' => 0x25, 'j' => 0x26, 'k' => 0x28, 'n' => 0x2D,
            'm' => 0x2E,
            _ => return None,
        })
    };
    if token.chars().count() == 1 {
        let c = token.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return letter(c);
        }
        // Digit row + common punctuation (kVK_ANSI_*).
        return Some(match c {
            '1' => 0x12, '2' => 0x13, '3' => 0x14, '4' => 0x15, '5' => 0x17,
            '6' => 0x16, '7' => 0x1A, '8' => 0x1C, '9' => 0x19, '0' => 0x1D,
            '-' => 0x1B, '=' => 0x18, '[' => 0x21, ']' => 0x1E, '\\' => 0x2A,
            ';' => 0x29, '\'' => 0x27, ',' => 0x2B, '.' => 0x2F, '/' => 0x2C,
            '`' => 0x32, ' ' => 0x31,
            _ => return None,
        });
    }
    // Named keys (kVK_*). Only well-known, correctly-mapped names; anything else is
    // an HONEST miss (None) — never guess.
    Some(match token {
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "space" | "spacebar" => 0x31,
        "delete" | "backspace" => 0x33,
        "escape" | "esc" => 0x35,
        "forwarddelete" | "fwddelete" => 0x75,
        "home" => 0x73,
        "end" => 0x77,
        "pageup" => 0x74,
        "pagedown" => 0x79,
        "left" | "leftarrow" => 0x7B,
        "right" | "rightarrow" => 0x7C,
        "down" | "downarrow" => 0x7D,
        "up" | "uparrow" => 0x7E,
        "f1" => 0x7A, "f2" => 0x78, "f3" => 0x63, "f4" => 0x76,
        "f5" => 0x60, "f6" => 0x61, "f7" => 0x62, "f8" => 0x64,
        "f9" => 0x65, "f10" => 0x6D, "f11" => 0x67, "f12" => 0x6F,
        _ => return None,
    })
}

/* ------------------------------------------------------------- socket paths */

/// Path of the actuate socket: `<root>/state/ipc/actuate.sock`.
fn actuate_socket_path(root: &Path) -> PathBuf {
    root.join("state").join("ipc").join("actuate.sock")
}

/// Bind the actuate socket: remove a stale one, create the 0700 parent dir, bind,
/// chmod 0600. Mirrors `daemon::audio::bind_audio_socket` — defense-in-depth on
/// top of the token gate (the dir + socket are owner-only, so a non-owner process
/// cannot even connect, and a connecting client still has to present the token).
fn bind_actuate_socket(path: &Path) -> std::io::Result<UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            // A stale socket we cannot remove is fatal to the bind below; surface it.
            return Err(e);
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    let listener = UnixListener::bind(path)?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(listener)
}

/* ----------------------------------------------------------- the listener */

/// START the actuator listener on a dedicated background thread. NON-BLOCKING:
/// returns immediately; the thread binds the socket and accepts connections for
/// the life of the process. It NEVER actuates on its own — it only posts a CGEvent
/// when a token-authenticated daemon request arrives. A bind failure (e.g. the
/// daemon hasn't created `state/ipc/` yet, or another HUD instance holds it) is a
/// clean no-op: the thread logs nothing sensitive and exits, leaving the app
/// otherwise healthy.
pub fn start_actuator_listener() {
    let _ = std::thread::Builder::new()
        .name("hud-actuator".to_string())
        .spawn(listen_loop);
}

/// Bind + accept loop. Resolves the root, binds the socket, then serves one
/// request per accepted connection. Off the async runtime entirely (no `!Send`
/// type ever touches Tauri managed state or an `.await`).
fn listen_loop() {
    let root = match crate::heal::resolve_root_for_command() {
        Ok(r) => r,
        // Cannot resolve the root → nothing to bind. Clean exit.
        Err(_) => return,
    };
    let sock_path = actuate_socket_path(&root);
    let listener = match bind_actuate_socket(&sock_path) {
        Ok(l) => l,
        // Bind failed (dir missing / already held) → clean no-op exit.
        Err(_) => return,
    };

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                // One request = one actuation = one connection. Handle inline (the
                // post is fast and we want strictly one-at-a-time actuation). The
                // catch_unwind only isolates a handler panic in DEV builds; the
                // release profile sets panic = "abort" (see src-tauri/Cargo.toml),
                // so in the shipped binary a handler panic aborts the process
                // instead of being caught here. handle_connection is kept total
                // so no panic path exists in practice.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle_connection(&root, stream);
                }));
            }
            // A transient accept error shouldn't tear down the whole listener.
            Err(_) => continue,
        }
    }
}

/// Serve ONE connection: read the single request line, verify the token
/// (constant-time vs. the token the HUD itself reads), post EXACTLY ONE CGEvent,
/// write back the one reply line, close. An invalid token => close with NO reply
/// and NO actuation (we do not even acknowledge — a silent close, exactly like an
/// unauthorized command-channel peer).
fn handle_connection(root: &Path, stream: UnixStream) {
    // Bound the read: a slow/idle local peer must not wedge the inline accept
    // loop (the read loop's Err arm returns on the resulting timeout). 5s is
    // generous for one small request line — mirrors the command channel's timeout.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;

    // Read ONE '\n'-terminated line, byte-by-byte so a misbehaving peer cannot
    // stream unbounded bytes at us — we stop at the cap and refuse honestly.
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break, // peer closed before a newline
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > MAX_LINE_BYTES {
                    // Oversized request — refuse honestly (do not parse / post).
                    let _ = writer
                        .write_all(Reply::fail("request exceeded the size cap").encode().as_bytes());
                    return;
                }
            }
            Err(_) => return,
        }
    }
    let raw = String::from_utf8_lossy(&line);

    let (token, request) = match parse_request(&raw) {
        Ok(parsed) => parsed,
        Err(detail) => {
            let _ = writer.write_all(Reply::fail(detail).encode().as_bytes());
            return;
        }
    };

    // VERIFY the token by constant-time equality against the SAME per-boot
    // capability token the command channel uses, read HERE by the HUD. A missing
    // token file (daemon not running / handoff incomplete) or a mismatch => silent
    // close, NO actuation. The token never leaves this stack and is never logged.
    let expected = match crate::command::read_token(root) {
        Ok(t) => t,
        Err(_) => return, // no token to compare against → close, do nothing
    };
    if !constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        return; // unauthorized → silent close, no reply, no actuation
    }

    // Authorized: post EXACTLY ONE CGEvent for the single action.
    let reply = post_action(&request.action);
    let _ = writer.write_all(reply.encode().as_bytes());
    let _ = writer.flush();
}

/* --------------------------------------------------- CGEvent post (macOS) */

#[cfg(target_os = "macos")]
fn post_action(action: &Action) -> Reply {
    imp::post_action(action)
}

#[cfg(not(target_os = "macos"))]
fn post_action(_action: &Action) -> Reply {
    Reply::fail("UI actuation is macOS-only")
}

#[cfg(target_os = "macos")]
mod imp {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton,
    };
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;

    use super::{map_combo, Action, Reply};

    // Accessibility — ApplicationServices. `AXIsProcessTrusted` is the
    // non-prompting check; `AXIsProcessTrustedWithOptions` + the prompt key shows
    // the clean "JARVIS would like to control this computer using accessibility"
    // dialog (which offers to open System Settings). Mirrors tcc.rs's link
    // pattern.
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    /// Fire the clean Accessibility prompt ONCE (no-op if already decided). macOS
    /// shows the "JARVIS would like to control this computer using accessibility"
    /// dialog only from the not-determined state; afterward this is inert.
    fn trigger_accessibility_prompt() {
        unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let value = CFBoolean::true_value();
            let dict = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
            let _ = AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef());
        }
    }

    /// A fresh HID-state event source for each post. `Err(())` if the source
    /// cannot be created (a stripped / non-GUI host) — we then fail honestly.
    fn source() -> Result<CGEventSource, Reply> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|_| Reply::fail("could not create the event source"))
    }

    pub fn post_action(action: &Action) -> Reply {
        // DEVICE GATE: the Accessibility TCC consent. Without it macOS silently
        // drops every synthetic event — so we refuse HONESTLY rather than pretend
        // the action landed, AND fire the clean prompt once so the user can grant
        // it. The next request (after they approve + enable JARVIS) actuates.
        if !unsafe { AXIsProcessTrusted() } {
            trigger_accessibility_prompt();
            return Reply::fail(
                "accessibility not granted — approve the JARVIS prompt then enable JARVIS in System Settings",
            );
        }
        match action {
            Action::Click { x, y } => post_click(*x, *y),
            Action::Type { text } => post_type(text),
            Action::Key { combo } => post_key(combo),
        }
    }

    /// Post a left mouse-down + mouse-up CGEvent at (x, y) into the HID stream.
    fn post_click(x: i32, y: i32) -> Reply {
        let src = match source() {
            Ok(s) => s,
            Err(r) => return r,
        };
        let point = CGPoint::new(x as f64, y as f64);
        let down = match CGEvent::new_mouse_event(
            src.clone(),
            CGEventType::LeftMouseDown,
            point,
            CGMouseButton::Left,
        ) {
            Ok(e) => e,
            Err(_) => return Reply::fail("could not create the mouse-down event"),
        };
        let up = match CGEvent::new_mouse_event(
            src,
            CGEventType::LeftMouseUp,
            point,
            CGMouseButton::Left,
        ) {
            Ok(e) => e,
            Err(_) => return Reply::fail("could not create the mouse-up event"),
        };
        down.post(CGEventTapLocation::HID);
        up.post(CGEventTapLocation::HID);
        Reply::ok("clicked")
    }

    /// Post ONE synthetic keyboard CGEvent carrying the whole unicode run via
    /// `CGEventKeyboardSetUnicodeString` (the `set_string` wrapper). One `type` op
    /// = one event pair, not per-keystroke.
    fn post_type(text: &str) -> Reply {
        let src = match source() {
            Ok(s) => s,
            Err(r) => return r,
        };
        // keycode 0 + the unicode-string override: the posted character(s) come
        // from the unicode string, not the (ignored) keycode.
        let down = match CGEvent::new_keyboard_event(src.clone(), 0, true) {
            Ok(e) => e,
            Err(_) => return Reply::fail("could not create the keyboard-down event"),
        };
        down.set_string(text);
        let up = match CGEvent::new_keyboard_event(src, 0, false) {
            Ok(e) => e,
            Err(_) => return Reply::fail("could not create the keyboard-up event"),
        };
        up.set_string(text);
        down.post(CGEventTapLocation::HID);
        up.post(CGEventTapLocation::HID);
        Reply::ok("typed")
    }

    /// Post a key-down + key-up CGEvent for the ONE parsed combo (with modifier
    /// flags). An unmappable combo posts NOTHING and returns an HONEST error —
    /// never a fabricated / wrong key.
    fn post_key(combo: &str) -> Reply {
        let Some((keycode, flag_bits)) = map_combo(combo) else {
            return Reply::fail("could not map that key combo to a key");
        };
        let src = match source() {
            Ok(s) => s,
            Err(r) => return r,
        };
        let flags = CGEventFlags::from_bits_truncate(flag_bits);
        let down = match CGEvent::new_keyboard_event(src.clone(), keycode, true) {
            Ok(e) => e,
            Err(_) => return Reply::fail("could not create the key-down event"),
        };
        down.set_flags(flags);
        let up = match CGEvent::new_keyboard_event(src, keycode, false) {
            Ok(e) => e,
            Err(_) => return Reply::fail("could not create the key-up event"),
        };
        up.set_flags(flags);
        down.post(CGEventTapLocation::HID);
        up.post(CGEventTapLocation::HID);
        Reply::ok("pressed")
    }
}

/* --------------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the daemon-side request line for an action, EXACTLY as the daemon
    /// encodes it, so parse_request is tested against the real wire shape.
    fn encode_request(token: &str, action_json: &str, target: &str) -> String {
        format!(r#"{{"token":"{token}","action":{action_json},"target_desc":"{target}"}}"#)
    }

    #[test]
    fn parse_request_decodes_each_action_kind() {
        // click → typed i32 coords.
        let (tok, req) = parse_request(&encode_request(
            "TOK",
            r#"{"kind":"click","x":120,"y":-3}"#,
            "the Save button",
        ))
        .expect("click parses");
        assert_eq!(tok, "TOK");
        assert_eq!(req.action, Action::Click { x: 120, y: -3 });
        assert_eq!(req.target_desc, "the Save button");

        // type → the whole unicode run as one action.
        let (_, req) = parse_request(&encode_request(
            "TOK",
            r#"{"kind":"type","text":"hello world"}"#,
            "the search field",
        ))
        .expect("type parses");
        assert_eq!(req.action, Action::Type { text: "hello world".to_string() });

        // key → the combo string verbatim (mapped later by map_combo).
        let (_, req) = parse_request(&encode_request(
            "TOK",
            r#"{"kind":"key","combo":"cmd+s"}"#,
            "the document",
        ))
        .expect("key parses");
        assert_eq!(req.action, Action::Key { combo: "cmd+s".to_string() });
    }

    #[test]
    fn parse_request_rejects_malformed_and_degenerate_requests() {
        // Junk / empty / non-object.
        for junk in ["", "   ", "not json", "[1,2,3]", "{", "null", "42"] {
            assert!(parse_request(junk).is_err(), "junk rejected: {junk:?}");
        }
        // Missing token.
        assert!(parse_request(r#"{"action":{"kind":"click","x":1,"y":1}}"#).is_err());
        // Empty token.
        assert!(parse_request(r#"{"token":"","action":{"kind":"click","x":1,"y":1}}"#).is_err());
        // Missing action / kind.
        assert!(parse_request(r#"{"token":"T"}"#).is_err());
        assert!(parse_request(r#"{"token":"T","action":{}}"#).is_err());
        // Unknown action kind (a privileged-sounding one must NOT slip through).
        assert!(parse_request(r#"{"token":"T","action":{"kind":"exec","cmd":"rm"}}"#).is_err());
        // click missing a coordinate / non-integer coordinate.
        assert!(parse_request(r#"{"token":"T","action":{"kind":"click","x":1}}"#).is_err());
        assert!(parse_request(r#"{"token":"T","action":{"kind":"click","x":1.5,"y":2}}"#).is_err());
        // empty type text / empty key combo are degenerate no-ops.
        assert!(parse_request(r#"{"token":"T","action":{"kind":"type","text":""}}"#).is_err());
        assert!(parse_request(r#"{"token":"T","action":{"kind":"key","combo":"  "}}"#).is_err());
    }

    #[test]
    fn parse_request_defaults_missing_target_desc_to_empty() {
        let (_, req) =
            parse_request(r#"{"token":"T","action":{"kind":"click","x":0,"y":0}}"#).unwrap();
        assert_eq!(req.target_desc, "");
    }

    #[test]
    fn parse_request_preserves_unicode_and_escaped_text() {
        // serde_json handles the escaping; an emoji + a quote survive the round-trip.
        let line = r#"{"token":"T","action":{"kind":"type","text":"café \"x\""},"target_desc":"f"}"#;
        let (_, req) = parse_request(line).unwrap();
        assert_eq!(req.action, Action::Type { text: "café \"x\"".to_string() });
    }

    #[test]
    fn map_combo_matches_the_daemon_mapping_for_common_combos() {
        // A bare named key → its ANSI keycode, no flags.
        assert_eq!(map_combo("return"), Some((0x24, 0)));
        assert_eq!(map_combo("enter"), Some((0x24, 0)));
        assert_eq!(map_combo("escape"), Some((0x35, 0)));
        assert_eq!(map_combo("esc"), Some((0x35, 0)));
        assert_eq!(map_combo("tab"), Some((0x30, 0)));
        assert_eq!(map_combo("space"), Some((0x31, 0)));

        // cmd+s → the 's' keycode (0x01) with the COMMAND flag bit.
        assert_eq!(map_combo("cmd+s"), Some((0x01, FLAG_COMMAND)));
        assert_eq!(map_combo("command+s"), Some((0x01, FLAG_COMMAND)));
        // Case-insensitive + whitespace tolerant.
        assert_eq!(map_combo("  CMD + S "), Some((0x01, FLAG_COMMAND)));

        // shift+tab → tab keycode with the SHIFT flag.
        assert_eq!(map_combo("shift+tab"), Some((0x30, FLAG_SHIFT)));

        // Multiple modifiers fold (order-independent): cmd+shift+z.
        assert_eq!(map_combo("cmd+shift+z"), Some((0x06, FLAG_COMMAND | FLAG_SHIFT)));
        assert_eq!(map_combo("shift+cmd+z"), Some((0x06, FLAG_COMMAND | FLAG_SHIFT)));

        // ctrl / opt aliases fold to the right bits.
        assert_eq!(map_combo("ctrl+c"), Some((0x08, FLAG_CONTROL)));
        assert_eq!(map_combo("control+c"), Some((0x08, FLAG_CONTROL)));
        assert_eq!(map_combo("opt+a"), Some((0x00, FLAG_ALTERNATE)));
        assert_eq!(map_combo("alt+a"), Some((0x00, FLAG_ALTERNATE)));
    }

    #[test]
    fn map_combo_refuses_to_guess_unmappable_or_malformed_combos() {
        // An unknown base key is an HONEST miss, never a fabricated keycode.
        assert_eq!(map_combo("cmd+kaboom"), None);
        assert_eq!(map_combo("nope"), None);
        // Two base keys is malformed.
        assert_eq!(map_combo("a+b"), None);
        // A stray "+" with nothing around it is malformed.
        assert_eq!(map_combo("cmd+"), None);
        assert_eq!(map_combo("+s"), None);
        // Modifiers only, no base key → no event to post.
        assert_eq!(map_combo("cmd+shift"), None);
    }

    #[test]
    fn reply_encode_is_one_newline_terminated_json_line() {
        let ok = Reply::ok("clicked");
        let line = ok.encode();
        assert!(line.ends_with('\n'), "reply must be newline-terminated");
        assert_eq!(line.matches('\n').count(), 1, "exactly one physical line");
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["detail"], "clicked");
        // Exactly the two contracted keys — no stray field on the wire.
        assert_eq!(parsed.as_object().unwrap().len(), 2);

        // A failure reply is honest: ok:false with the detail.
        let fail = Reply::fail("accessibility not granted — approve the JARVIS prompt then enable JARVIS in System Settings");
        let parsed: Value = serde_json::from_str(fail.encode().trim_end()).unwrap();
        assert_eq!(parsed["ok"], false);
        assert!(parsed["detail"].as_str().unwrap().contains("accessibility not granted"));
    }

    #[test]
    fn reply_encode_escapes_detail_so_it_stays_one_line() {
        // A detail with a quote/newline stays a single physical JSON line.
        let r = Reply::fail("line1\nline2 \"q\"");
        let line = r.encode();
        assert_eq!(line.matches('\n').count(), 1, "only the terminator newline");
        let parsed: Value = serde_json::from_str(line.trim_end()).unwrap();
        assert_eq!(parsed["detail"], "line1\nline2 \"q\"");
    }

    #[test]
    fn constant_time_eq_compares_full_byte_equality() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        // Length mismatch is never equal (and short-circuits before the loop).
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
        // Two empty tokens are equal, but the caller rejects an empty token upstream.
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn actuate_socket_path_is_under_state_ipc() {
        let p = actuate_socket_path(Path::new("/jarvis"));
        assert_eq!(p, Path::new("/jarvis/state/ipc/actuate.sock"));
    }
}
