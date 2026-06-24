//! SYSTEM ACCESS — fire the REAL macOS TCC permission prompts from THIS app
//! bundle (JARVIS.app), so the user sees a clean "JARVIS" dialog rather than
//! nothing.
//!
//! WHY HERE (not the daemon): macOS attributes a permission to the binary that
//! requests it, and only shows a prompt for a process that (a) carries the
//! matching usage-description string and (b) can present a foreground dialog.
//! JARVIS.app satisfies both (its Info.plist has NSMicrophoneUsageDescription /
//! NSCameraUsageDescription / NSAppleEventsUsageDescription and it is a real app
//! bundle). The background `jarvisd` is a bare LaunchAgent binary and cannot. So
//! the APP requests each device permission here; Stage 2 routes the captured data
//! from the app to the daemon so the features actually use the app's grant.
//!
//! Each request only PROMPTS when the permission is still "not determined";
//! macOS never re-prompts once decided (the SYSTEM ACCESS panel's Settings
//! deep-link is the path to change an already-decided permission). These calls
//! are honest: they report the status, never a faked grant.

use serde::Serialize;

/// The honest outcome of a permission request, surfaced to the HUD.
///   * `fired`  — true when a NATIVE OS PROMPT was actually triggered this call
///     (only possible from the "not determined" state).
///   * `status` — `not_determined` | `granted` | `denied` | `restricted` |
///     `no_prompt_api` | `error`.
///   * `detail` — a short human line (no secret).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PromptResult {
    pub fired: bool,
    pub status: String,
    pub detail: String,
}

impl PromptResult {
    fn new(fired: bool, status: &str, detail: &str) -> Self {
        Self { fired, status: status.to_string(), detail: detail.to_string() }
    }
}

#[cfg(target_os = "macos")]
pub use imp::request_permission;

#[cfg(not(target_os = "macos"))]
pub use stub::request_permission;

#[cfg(target_os = "macos")]
mod imp {
    use block2::RcBlock;
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, Bool};
    use objc2_foundation::NSString;

    use super::PromptResult;

    // AVFoundation media-type constants (NSString*). Referencing them forces the
    // framework to link AND gives us the real AVMediaType values.
    #[link(name = "AVFoundation", kind = "framework")]
    extern "C" {
        static AVMediaTypeAudio: *const NSString;
        static AVMediaTypeVideo: *const NSString;
    }

    // Screen Recording — CoreGraphics. Preflight checks status without prompting;
    // Request fires the prompt when not determined and returns whether granted.
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }

    // Accessibility — ApplicationServices. WithOptions + the prompt key shows the
    // "grant Accessibility" dialog (which offers to open System Settings).
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    // Input Monitoring — IOKit. Check is non-prompting; Request fires the prompt.
    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOHIDRequestAccess(request_type: u32) -> bool;
        fn IOHIDCheckAccess(request_type: u32) -> u32;
    }
    const KIOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
    const KIOHID_ACCESS_TYPE_GRANTED: u32 = 0;
    const KIOHID_ACCESS_TYPE_DENIED: u32 = 1;

    /// AVCaptureDevice authorization for a media type. 0=notDetermined,
    /// 1=restricted, 2=denied, 3=authorized. Fires the prompt (and returns
    /// `fired=true`) only from notDetermined.
    fn av_request(media: *const NSString, label: &str) -> PromptResult {
        let Some(cls) = AnyClass::get(c"AVCaptureDevice") else {
            return PromptResult::new(false, "error", "AVFoundation unavailable");
        };
        let mt: &NSString = unsafe { &*media };
        let status: isize = unsafe { msg_send![cls, authorizationStatusForMediaType: mt] };
        match status {
            0 => {
                // notDetermined → fire the prompt. The completion handler is a
                // no-op; AVFoundation copies the block, so dropping our RcBlock
                // after the call is safe.
                let handler = RcBlock::new(|_granted: Bool| {});
                let _: () = unsafe {
                    msg_send![cls, requestAccessForMediaType: mt, completionHandler: &*handler]
                };
                PromptResult::new(
                    true,
                    "not_determined",
                    &format!("Asked macOS for {label} access — approve the JARVIS prompt."),
                )
            }
            3 => PromptResult::new(false, "granted", &format!("{label} access is already granted.")),
            2 => PromptResult::new(
                false,
                "denied",
                &format!("{label} was previously denied — re-enable JARVIS in System Settings."),
            ),
            _ => PromptResult::new(
                false,
                "restricted",
                &format!("{label} access is restricted by this Mac's policy."),
            ),
        }
    }

    fn request_microphone() -> PromptResult {
        av_request(unsafe { AVMediaTypeAudio }, "Microphone")
    }

    fn request_camera() -> PromptResult {
        av_request(unsafe { AVMediaTypeVideo }, "Camera")
    }

    fn request_screen() -> PromptResult {
        let granted = unsafe { CGPreflightScreenCaptureAccess() };
        if granted {
            return PromptResult::new(false, "granted", "Screen Recording is already granted.");
        }
        // Not granted: this fires the prompt the first time (notDetermined) and
        // is a no-op afterward (returns the cached denial).
        let now = unsafe { CGRequestScreenCaptureAccess() };
        if now {
            PromptResult::new(true, "granted", "Screen Recording granted.")
        } else {
            PromptResult::new(
                true,
                "not_determined",
                "Asked macOS for Screen Recording — approve the JARVIS prompt (you may need to quit & reopen JARVIS).",
            )
        }
    }

    fn request_accessibility() -> PromptResult {
        if unsafe { AXIsProcessTrusted() } {
            return PromptResult::new(false, "granted", "Accessibility is already granted.");
        }
        let prompt = unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let value = CFBoolean::true_value();
            let dict = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);
            AXIsProcessTrustedWithOptions(dict.as_concrete_TypeRef())
        };
        if prompt {
            PromptResult::new(false, "granted", "Accessibility is already granted.")
        } else {
            PromptResult::new(
                true,
                "not_determined",
                "Asked macOS for Accessibility — approve the JARVIS prompt, then enable JARVIS in the pane.",
            )
        }
    }

    fn request_input_monitoring() -> PromptResult {
        let access = unsafe { IOHIDCheckAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) };
        if access == KIOHID_ACCESS_TYPE_GRANTED {
            return PromptResult::new(false, "granted", "Input Monitoring is already granted.");
        }
        if access == KIOHID_ACCESS_TYPE_DENIED {
            return PromptResult::new(
                false,
                "denied",
                "Input Monitoring was previously denied — re-enable JARVIS in System Settings.",
            );
        }
        let granted = unsafe { IOHIDRequestAccess(KIOHID_REQUEST_TYPE_LISTEN_EVENT) };
        PromptResult::new(
            true,
            if granted { "granted" } else { "not_determined" },
            "Asked macOS for Input Monitoring — approve the JARVIS prompt.",
        )
    }

    /// Fire the native prompt for an allowlisted permission key. Keys without a
    /// programmatic request API (Full Disk Access, Automation) return
    /// `no_prompt_api` so the caller falls back to opening System Settings.
    pub fn request_permission(key: &str) -> PromptResult {
        match key {
            "microphone" => request_microphone(),
            "camera" => request_camera(),
            "screen" => request_screen(),
            "accessibility" => request_accessibility(),
            "input_monitoring" => request_input_monitoring(),
            "full_disk" | "automation" => PromptResult::new(
                false,
                "no_prompt_api",
                "macOS has no prompt for this one — opening System Settings instead.",
            ),
            _ => PromptResult::new(false, "error", "unknown permission"),
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod stub {
    use super::PromptResult;

    /// Off macOS there is no TCC; every permission reports `no_prompt_api`.
    pub fn request_permission(_key: &str) -> PromptResult {
        PromptResult::new(false, "no_prompt_api", "macOS-only")
    }
}
