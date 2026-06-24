//! SYSTEM ACCESS — open the macOS Privacy & Security panes JARVIS needs.
//!
//! WHAT THIS IS: two Tauri commands that take the user to the exact macOS
//! permission switch, because macOS does NOT let any app grant itself a TCC
//! permission (Full Disk Access / Accessibility / Screen Recording / Microphone
//! are a hard security boundary with no programmatic grant). The MOST an app may
//! do is deep-link the Privacy pane + let the OS prompt on first use — so we open
//! the pane and the HUD explains it; the user flips the switch.
//!
//!   * `open_privacy_pane(pane)` -> opens ONE Privacy pane by an ALLOWLISTED key.
//!   * `request_all_permissions()` -> opens the Privacy & Security HUB (every
//!     category listed) — the "re-request all" action.
//!
//! SECURITY: the frontend sends a KEY, never a URL. We map the key to a FIXED
//! macOS anchor via `pane_lookup` and build the `x-apple.systempreferences:` URL
//! OURSELVES; an unknown key is rejected with NO shell-out. The caller can never
//! inject a URL, an `open` flag, or any other scheme — the anchor is a static
//! `&'static str` from the table below, never the caller's bytes. We shell the
//! same `/usr/bin/open` pattern the rest of the backend uses (absolute path).
//!
//! HONESTY: the result reports whether System Settings was actually launched
//! (never a faked "granted"), and the per-pane guidance is ACCURATE to how each
//! pane works — only the panes that have a "+" tell the user to use it; the
//! prompt-on-first-use panes (Microphone / Camera / Automation) say so instead.
//! We never claim the permission is now held.

use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

/// One allowlisted Privacy pane: the key the frontend sends, the FIXED macOS
/// anchor we map it to, the pane's display label, and HONEST per-pane guidance
/// for how JARVIS shows up there (some panes have a "+", others list the app
/// only after it first uses the capability — saying "+" for the latter would be
/// factually wrong).
struct Pane {
    key: &'static str,
    anchor: &'static str,
    label: &'static str,
    /// How JARVIS appears in THIS pane — accurate to the pane's real mechanism.
    guidance: &'static str,
}

/// The ALLOWLIST of Privacy panes the HUD may open. The frontend's
/// `hud/src/core/permissions.ts` mirrors this key set (a test on each side locks
/// the set, so a drift fails CI). Anchors verified live on macOS 26.5.1 (Tahoe):
/// the classic `com.apple.preference.security?Privacy_*` scheme still resolves
/// to each pane.
const PRIVACY_PANES: &[Pane] = &[
    Pane {
        key: "full_disk",
        anchor: "Privacy_AllFiles",
        label: "Full Disk Access",
        guidance: "Click the + to add JARVIS if it isn't listed, then switch it on.",
    },
    Pane {
        key: "accessibility",
        anchor: "Privacy_Accessibility",
        label: "Accessibility",
        guidance: "Switch JARVIS on — click the + to add it first if it isn't listed.",
    },
    Pane {
        key: "screen",
        anchor: "Privacy_ScreenCapture",
        label: "Screen & System Audio Recording",
        guidance: "Switch JARVIS on — click the + to add it first if it isn't listed.",
    },
    Pane {
        key: "microphone",
        anchor: "Privacy_Microphone",
        label: "Microphone",
        guidance: "JARVIS appears here the first time it uses the mic — switch it on then.",
    },
    Pane {
        key: "input_monitoring",
        anchor: "Privacy_ListenEvent",
        label: "Input Monitoring",
        guidance: "Switch JARVIS on — click the + to add it first if it isn't listed.",
    },
    Pane {
        key: "automation",
        anchor: "Privacy_Automation",
        label: "Automation",
        guidance: "JARVIS appears here after it first drives another app — switch it on then.",
    },
    Pane {
        key: "camera",
        anchor: "Privacy_Camera",
        label: "Camera",
        guidance: "JARVIS appears here the first time it uses the camera — switch it on then.",
    },
];

/// The Privacy & Security HUB anchor (lists every category) — the "re-request
/// all" target — plus its label and guidance.
const PRIVACY_HUB_ANCHOR: &str = "Privacy";
const PRIVACY_HUB_LABEL: &str = "Privacy & Security";
const PRIVACY_HUB_GUIDANCE: &str = "Open each category and switch JARVIS on.";

/// Resolve an allowlisted pane key to its pane record. `None` for any unknown
/// key — this is the SECURITY GATE: only a known key yields an anchor, and the
/// anchor returned is a `&'static str` from the table, never the input.
fn pane_lookup(key: &str) -> Option<&'static Pane> {
    PRIVACY_PANES.iter().find(|p| p.key == key)
}

/// Build the System Settings deep-link for a macOS Privacy anchor. The URL
/// scheme + pane id are CONSTANTS; only the (allowlisted, static) anchor varies,
/// so the result is fully backend-controlled — there is no caller-supplied byte
/// in it. Split out so the exact URL we shell is unit-testable.
fn pane_url(anchor: &str) -> String {
    format!("x-apple.systempreferences:com.apple.preference.security?{anchor}")
}

/// The outcome surfaced to the HUD. `opened` is true only when `/usr/bin/open`
/// dispatched the URL (System Settings launched); `label` is the pane name;
/// `detail` is a short human line (no secret — the only material is a public URL
/// / pane name + honest guidance).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PaneOpen {
    pub opened: bool,
    pub label: String,
    pub detail: String,
}

/// Shell `/usr/bin/open <url>` and map the outcome. `url` is ALWAYS a
/// backend-built string (from `pane_url` over a static anchor) — never raw
/// caller input. `guidance` is the pane's honest "how JARVIS shows up here" line.
/// Honest about failure (no GUI session) rather than pretending.
async fn open_url(url: &str, label: &str, guidance: &str) -> Result<PaneOpen, String> {
    let output = Command::new("/usr/bin/open")
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not open System Settings: {e}"))?;

    if output.status.success() {
        Ok(PaneOpen {
            opened: true,
            label: label.to_string(),
            detail: format!(
                "Opened System Settings → {label}. {guidance} \
                 (macOS won't turn this on for you.)"
            ),
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(PaneOpen {
            opened: false,
            label: label.to_string(),
            detail: format!(
                "could not open System Settings: {}",
                stderr.trim().lines().next().unwrap_or("no GUI session")
            ),
        })
    }
}

/// Open ONE Privacy pane by an allowlisted key. An unknown key is rejected
/// WITHOUT shelling anything, so the frontend can never open an arbitrary URL.
#[tauri::command]
pub async fn open_privacy_pane(pane: String) -> Result<PaneOpen, String> {
    let Some(p) = pane_lookup(&pane) else {
        return Err(format!("unknown permission pane: {pane}"));
    };
    open_url(&pane_url(p.anchor), p.label, p.guidance).await
}

/// Open the Privacy & Security HUB (every category listed) — the "re-request
/// all permissions" action. Takes NO argument (the anchor is a constant), so
/// there is no injection surface at all.
#[tauri::command]
pub async fn request_all_permissions() -> Result<PaneOpen, String> {
    open_url(
        &pane_url(PRIVACY_HUB_ANCHOR),
        PRIVACY_HUB_LABEL,
        PRIVACY_HUB_GUIDANCE,
    )
    .await
}

/// REQUEST a permission the right way: fire the NATIVE macOS prompt from this app
/// bundle (so the user sees a clean "JARVIS" dialog) for the permissions that have
/// a request API; for the ones that do NOT (Full Disk Access, Automation) — or one
/// macOS won't re-prompt because it was already denied — fall back to opening the
/// exact Settings pane. The native request runs on a blocking thread (some TCC
/// request calls block until the user answers). An unknown key is rejected.
#[tauri::command]
pub async fn request_access(pane: String) -> Result<PaneOpen, String> {
    let Some(p) = pane_lookup(&pane) else {
        return Err(format!("unknown permission pane: {pane}"));
    };
    let key = pane.clone();
    let prompt = tauri::async_runtime::spawn_blocking(move || crate::tcc::request_permission(&key))
        .await
        .map_err(|e| format!("permission request task failed: {e}"))?;

    // macOS shows the prompt ONLY from "not determined". When it won't (already
    // denied, or no prompt API exists for this pane), open the exact Settings pane
    // so the user can still grant it.
    if matches!(prompt.status.as_str(), "denied" | "no_prompt_api" | "error") {
        let opened = open_url(&pane_url(p.anchor), p.label, p.guidance).await?;
        return Ok(PaneOpen {
            opened: opened.opened,
            label: p.label.to_string(),
            detail: format!("{} {}", prompt.detail, opened.detail),
        });
    }
    Ok(PaneOpen { opened: prompt.fired, label: p.label.to_string(), detail: prompt.detail })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The allowlist is EXACTLY these keys, in this order. This is half of the
    /// cross-language drift lock — `hud/src/core/permissions.ts`'s
    /// `PERMISSION_KEYS` asserts the identical list, so adding/removing/renaming
    /// a pane on one side without the other fails one of the two suites.
    #[test]
    fn allowlist_has_exactly_the_expected_keys() {
        let keys: Vec<&str> = PRIVACY_PANES.iter().map(|p| p.key).collect();
        assert_eq!(
            keys,
            vec![
                "full_disk",
                "accessibility",
                "screen",
                "microphone",
                "input_monitoring",
                "automation",
                "camera",
            ]
        );
    }

    /// The SECURITY GATE: only a known key yields a pane. An anchor string, an
    /// empty string, a path-escape, or any other non-key resolves to None — so
    /// nothing is shelled for it.
    #[test]
    fn unknown_pane_key_yields_no_pane() {
        assert!(pane_lookup("full_disk").is_some());
        assert!(pane_lookup("camera").is_some());
        assert!(pane_lookup("").is_none());
        assert!(pane_lookup("../../etc/passwd").is_none());
        assert!(pane_lookup("Privacy_AllFiles").is_none()); // the ANCHOR is not a KEY
        assert!(pane_lookup("https://evil.example").is_none());
        assert!(pane_lookup("-g").is_none()); // an `open` flag is not a key
    }

    /// The URL is the constant scheme + pane id with ONLY the static anchor
    /// appended — never raw input. Every allowlisted anchor produces a
    /// security-pane Privacy URL.
    #[test]
    fn url_is_constant_scheme_plus_anchor_only() {
        assert_eq!(
            pane_url("Privacy_AllFiles"),
            "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles"
        );
        for p in PRIVACY_PANES {
            let u = pane_url(p.anchor);
            assert!(
                u.starts_with("x-apple.systempreferences:com.apple.preference.security?Privacy_"),
                "anchor {} produced an unexpected URL: {u}",
                p.anchor
            );
            // The URL can never begin with '-', so /usr/bin/open always treats
            // it as a positional URL, never a flag.
            assert!(!u.starts_with('-'));
        }
    }

    /// The "re-request all" target is the Privacy & Security hub root.
    #[test]
    fn hub_anchor_opens_the_privacy_security_root() {
        assert_eq!(
            pane_url(PRIVACY_HUB_ANCHOR),
            "x-apple.systempreferences:com.apple.preference.security?Privacy"
        );
        assert_eq!(PRIVACY_HUB_LABEL, "Privacy & Security");
    }

    /// Every allowlisted pane has a non-empty label + honest guidance, and the
    /// guidance only mentions the "+" affordance for panes that actually have
    /// one (the prompt-on-first-use panes must NOT tell the user to click a "+"
    /// that isn't there).
    #[test]
    fn every_pane_has_a_label_and_accurate_guidance() {
        // Panes WITHOUT a "+" — apps appear only after first use. Their guidance
        // must not instruct the user to click a (non-existent) "+".
        let no_plus = ["microphone", "camera", "automation"];
        for p in PRIVACY_PANES {
            assert!(!p.key.is_empty());
            assert!(p.anchor.starts_with("Privacy_"));
            assert!(!p.label.is_empty());
            assert!(p.guidance.len() > 10);
            if no_plus.contains(&p.key) {
                assert!(
                    !p.guidance.contains('+'),
                    "{} has no + button but its guidance mentions one: {}",
                    p.key,
                    p.guidance
                );
            }
        }
    }

    /// End-to-end SECURITY proof: an unknown key returns Err WITHOUT shelling
    /// anything. (We never call open_privacy_pane with a KNOWN key in a test —
    /// that would actually launch System Settings.) This exercises the real
    /// command's early-return gate, not just the pure `pane_lookup` helper.
    #[tokio::test]
    async fn open_privacy_pane_rejects_unknown_key_without_shelling() {
        for bad in ["", "bogus", "-a /Applications/Calculator.app", "Privacy_AllFiles"] {
            let r = open_privacy_pane(bad.to_string()).await;
            assert!(r.is_err(), "unknown key {bad:?} must be rejected");
            assert!(r.unwrap_err().contains("unknown permission pane"));
        }
    }

    #[test]
    fn pane_open_shape() {
        let p = PaneOpen { opened: true, label: "Microphone".into(), detail: "ok".into() };
        assert!(p.opened);
    }
}
