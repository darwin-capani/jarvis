//! Live Endpoint Security NOTIFY client — feature `endpoint-security`, DEVICE-GATED.
//!
//! The fragile part (parsing `es_message_t`) lives in a C shim (`csrc/es_shim.c`)
//! compiled against Apple's REAL header, so the struct layouts are compiler-
//! verified; Rust only sees a flat scalar ABI. This module is COMPILE+LINK
//! verified in a normal `cargo build --features endpoint-security` (linking ES
//! needs no entitlement — that check is at runtime), but it is NOT runtime-
//! verified here: `es_new_client` cannot instantiate without root + the restricted
//! `com.apple.developer.endpoint-security.client` entitlement + a notarized host,
//! so `start()` returns an honest error off-device and the light introspect path
//! keeps working.
//!
//! NOTIFY-ONLY. We subscribe to notify events only — never auth — so we never call
//! `es_respond` and can never block/wedge the subject (an AUTH client that misses
//! its deadline is killed and can stall the machine). We observe and report
//! through the tested `introspect::ingest_security_event` seam; we take no action.

use std::ffi::CStr;
use std::os::raw::{c_char, c_int};

use tracing::{info, warn};

use crate::introspect::{ingest_security_event, SecurityEvent};

/// Mirrors `jarvis_es_event` in csrc/es_shim.c (the flat C ABI).
#[repr(C)]
struct FlatEvent {
    kind: c_int,
    subject_pid: c_int,
    subject_path: *const c_char,
    actor_pid: c_int,
    actor_path: *const c_char,
    signal_number: c_int,
}

type EsCallback = extern "C" fn(*const FlatEvent);

// Link, in order: the shim archive (force-loaded via +whole-archive — a
// build-script static lib referenced only by the binary is otherwise dropped by
// the linker), then the ES + libbsm dylibs the shim calls into. build.rs compiles
// the archive and adds its OUT_DIR to the search path; declaring the links here
// keeps rustc's ordering + the whole-archive modifier exact. Linking needs no
// entitlement — es_new_client's entitlement check is a RUNTIME gate.
#[link(name = "jarvis_es_shim", kind = "static", modifiers = "+whole-archive")]
#[link(name = "EndpointSecurity", kind = "dylib")]
#[link(name = "bsm", kind = "dylib")]
extern "C" {
    fn jarvis_es_start(cb: EsCallback) -> c_int;
    fn jarvis_es_stop();
}

/// Map the flat C event kind to a `SecurityEvent`. Pure — unit-tested without the
/// framework. `None` for an unrecognized kind.
fn map_event(kind: i32, actor_pid: i32, actor_path: String, signal: i32) -> Option<SecurityEvent> {
    match kind {
        0 => Some(SecurityEvent::MprotectExec),
        1 => Some(SecurityEvent::MapJit),
        2 => Some(SecurityEvent::GetTask {
            by_pid: actor_pid,
            by_path: actor_path,
        }),
        3 => Some(SecurityEvent::Signal {
            signal,
            by_pid: actor_pid,
        }),
        _ => None,
    }
}

/// C callback (runs on ES's dispatch queue). Attribute the event to one of OUR
/// tracked micro-apps by pid, ignore everything else, and feed the tested
/// classifier. READ-ONLY.
extern "C" fn on_event(ev: *const FlatEvent) {
    let ev = match unsafe { ev.as_ref() } {
        Some(e) => e,
        None => return,
    };
    if ev.subject_pid < 0 {
        return;
    }
    // Only act on jarvisd's own children; a non-app pid is not attributed.
    let Some((app, jit)) = crate::introspect::app_for_pid(ev.subject_pid as u32) else {
        return;
    };
    let actor_path = cstr(ev.actor_path);
    if let Some(sec) = map_event(ev.kind, ev.actor_pid, actor_path, ev.signal_number) {
        ingest_security_event(&app, jit, &sec);
    }
}

fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: the C shim passes NUL-terminated `es_string_token_t.data`, valid for
    // the duration of this synchronous callback; we copy it out immediately.
    unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() }
}

/// Start the ES NOTIFY client. `Ok` when the kernel accepts the client; `Err` with
/// an honest reason when the entitlement/root/notarization aren't present (the
/// light introspect path is unaffected either way). Device-gated.
pub fn start() -> Result<(), String> {
    let rc = unsafe { jarvis_es_start(on_event) };
    match rc {
        0 => Ok(()),
        -1 => Err("es_new_client failed — needs root + the com.apple.developer.endpoint-security.client entitlement + a notarized host".to_string()),
        -2 => Err("es_subscribe failed".to_string()),
        n => Err(format!("endpoint security failed to start (code {n})")),
    }
}

#[allow(dead_code)]
pub fn stop() {
    // SAFETY: idempotent in the shim (guards a NULL client).
    unsafe { jarvis_es_stop() }
}

/// Try to start the ES client and report the outcome honestly on telemetry + the
/// log. Called once from `main` under the feature; never fails the daemon.
pub fn start_and_report() {
    match start() {
        Ok(()) => {
            info!("endpoint-security NOTIFY client active (watching mprotect/mmap/get_task/signal)");
            crate::telemetry::emit("system", "introspect.es", serde_json::json!({"active": true}));
        }
        Err(reason) => {
            warn!(%reason, "endpoint-security unavailable; the light introspect path continues");
            crate::telemetry::emit(
                "system",
                "introspect.es",
                serde_json::json!({"active": false, "reason": reason}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_event_covers_each_kind_and_rejects_unknown() {
        assert_eq!(map_event(0, -1, String::new(), 0), Some(SecurityEvent::MprotectExec));
        assert_eq!(map_event(1, -1, String::new(), 0), Some(SecurityEvent::MapJit));
        assert_eq!(
            map_event(2, 42, "/usr/bin/lldb".to_string(), 0),
            Some(SecurityEvent::GetTask { by_pid: 42, by_path: "/usr/bin/lldb".to_string() })
        );
        assert_eq!(
            map_event(3, 7, String::new(), 9),
            Some(SecurityEvent::Signal { signal: 9, by_pid: 7 })
        );
        assert_eq!(map_event(99, 0, String::new(), 0), None);
    }
}
