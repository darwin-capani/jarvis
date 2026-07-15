//! WS4a — the in-app "Check for updates" affordance (the SETTINGS / System tab).
//!
//! WHAT THIS IS: one Tauri command (`check_for_updates`) that drives the
//! `tauri-plugin-updater` API. The plugin fetches the GitHub Releases
//! `latest.json` endpoint (configured in tauri.conf.json -> plugins.updater),
//! compares versions, and — if a newer signed bundle exists — verifies its
//! minisign signature against the OWNER's PUBLIC updater key before offering it.
//!
//! HONEST HARD GATE (do NOT fake this): a working update flow requires TWO things
//! the OWNER provides ONCE, neither of which can live in this repo:
//!   1. a real updater keypair — the owner runs `npm run tauri signer generate`,
//!      keeps the PRIVATE key secret (it becomes the CI secret
//!      TAURI_SIGNING_PRIVATE_KEY), and pastes the PUBLIC key into
//!      tauri.conf.json's `plugins.updater.pubkey`;
//!   2. a published GitHub Release carrying a signed bundle + a `latest.json`
//!      manifest the release CI generates.
//!
//! Until BOTH exist, the committed `pubkey` is the PLACEHOLDER string and there is
//! no real `latest.json` to fetch. This command DETECTS that and returns a clean,
//! honest "not configured / no update" result — it NEVER pretends an update is
//! available and NEVER downloads or installs anything on its own. The moment the
//! owner adds their key + cuts a release, the SAME command starts returning real
//! results with no code change.
//!
//! This command only CHECKS + (when the user explicitly clicks) downloads+installs
//! a properly SIGNED update. It adds no other authority: an unsigned or
//! wrong-key bundle is rejected by the plugin's signature check, so a hostile
//! endpoint cannot push code onto the machine.

use serde::Serialize;
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

/// The committed PLACEHOLDER public key in tauri.conf.json. While the configured
/// pubkey is empty OR still equals this sentinel, the updater is NOT armed: we
/// return the honest "not configured yet" state instead of hitting the network
/// with a key that can verify nothing. Kept in lockstep with tauri.conf.json.
const PUBKEY_PLACEHOLDER: &str = "REPLACE_WITH_TAURI_UPDATER_PUBLIC_KEY";

/// The outcome surfaced to the HUD. `status` is a small contract vocabulary the
/// UI switches on; `detail` is a short human line (never a secret — there is no
/// secret on this path; the public key + version are public). `version` carries
/// the available version string only when `status == "available"`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UpdateCheck {
    /// "not_configured" — no real updater key/endpoint yet (the shipped state):
    ///   the owner has not run the one-time signer setup + cut a release.
    /// "up_to_date"     — the endpoint was reachable and no newer version exists.
    /// "available"      — a newer SIGNED version is published (see `version`).
    /// "installed"      — a newer version was downloaded, verified, and installed
    ///                    (the app should be relaunched to finish).
    /// "error"          — the check could not complete (offline / endpoint down);
    ///                    `detail` says why. Never a secret.
    pub status: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl UpdateCheck {
    fn not_configured() -> Self {
        Self {
            status: "not_configured".into(),
            detail:
                "Auto-update is not armed yet — it turns on once the owner adds their updater \
                 public key and publishes a signed release. See docs/RELEASE.md."
                    .into(),
            version: None,
        }
    }
    fn up_to_date() -> Self {
        Self { status: "up_to_date".into(), detail: "JARVIS is on the latest version.".into(), version: None }
    }
    fn error(detail: impl Into<String>) -> Self {
        Self { status: "error".into(), detail: detail.into(), version: None }
    }
}

/// Read the configured updater public key out of the resolved Tauri config so we
/// can detect the committed placeholder (or an empty key) BEFORE touching the
/// network. Returns None when no updater config / pubkey is present at all.
fn configured_pubkey(app: &AppHandle) -> Option<String> {
    let cfg = app.config();
    let updater = cfg.plugins.0.get("updater")?;
    let pubkey = updater.get("pubkey")?.as_str()?.trim().to_string();
    if pubkey.is_empty() {
        None
    } else {
        Some(pubkey)
    }
}

/// True while the updater is NOT armed: no pubkey configured, or the pubkey is
/// still the committed PLACEHOLDER. In that state every check short-circuits to
/// the honest `not_configured` result without any network call.
fn updater_unarmed(app: &AppHandle) -> bool {
    match configured_pubkey(app) {
        None => true,
        Some(key) => key == PUBKEY_PLACEHOLDER,
    }
}

/// CHECK for an update and, when `install` is true and a newer SIGNED version
/// exists, download + verify + install it. Returns an honest [`UpdateCheck`].
///
/// Guarded: while the updater is unarmed (placeholder pubkey / no release) this
/// returns `not_configured` with NO network call — it can never fake an update.
/// When armed, the plugin verifies the bundle's minisign signature against the
/// owner's public key; an unsigned / wrong-key bundle is rejected before install.
#[tauri::command]
pub async fn check_for_updates(app: AppHandle, install: bool) -> Result<UpdateCheck, String> {
    // No real key/endpoint yet -> honest "not armed", no network.
    if updater_unarmed(&app) {
        return Ok(UpdateCheck::not_configured());
    }

    // Build the updater (reads endpoints + pubkey from plugins.updater). A
    // builder error here means the config is malformed, not that an update
    // exists — surface it honestly rather than throwing.
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => return Ok(UpdateCheck::error(format!("updater unavailable: {e}"))),
    };

    match updater.check().await {
        // A newer signed version is published.
        Ok(Some(update)) => {
            let version = update.version.clone();
            if !install {
                return Ok(UpdateCheck {
                    status: "available".into(),
                    detail: format!("Version {version} is available."),
                    version: Some(version),
                });
            }
            // Explicit install: the plugin downloads, VERIFIES the minisign
            // signature against the owner's public key, then installs. A bad
            // signature aborts here (Err) — nothing unsigned is ever installed.
            match update.download_and_install(|_chunk, _total| {}, || {}).await {
                Ok(()) => Ok(UpdateCheck {
                    status: "installed".into(),
                    detail: format!(
                        "Version {version} was downloaded, verified, and installed — relaunch JARVIS to finish."
                    ),
                    version: Some(version),
                }),
                Err(e) => Ok(UpdateCheck::error(format!("update install failed: {e}"))),
            }
        }
        // Reachable, nothing newer.
        Ok(None) => Ok(UpdateCheck::up_to_date()),
        // Offline / endpoint down / no published release yet — honest error, no
        // secret (the only material on this path is the public key + a version).
        Err(e) => Ok(UpdateCheck::error(format!("could not check for updates: {e}"))),
    }
}

/// RELAUNCH the app to finish applying an already-installed update.
///
/// This adds NO install authority: it does not download, verify, or install
/// anything — `check_for_updates(install=true)` already did the signed download
/// + minisign verification + install. This command only restarts the binary
/// that is already on disk, using Tauri's built-in `AppHandle::restart()`. It is
/// the explicit "relaunch to finish updating" step the HUD offers after a
/// successful install. `restart()` replaces the process and never returns; the
/// `Result` signature exists only so the command type-checks in the handler.
#[tauri::command]
pub fn relaunch_app(app: AppHandle) -> Result<(), String> {
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_config_carries_a_real_updater_pubkey() {
        // v1.0.0 ARMED: the owner has swapped the placeholder for their real updater
        // PUBLIC key, so the shipped build's updater is live (it no longer
        // short-circuits to `not_configured`). This tripwire now guards the OPPOSITE
        // of before — the committed config must NOT regress to the placeholder and
        // must not carry an empty key. PUBKEY_PLACEHOLDER is still used at runtime by
        // updater_unarmed() to defensively detect an empty/placeholder key.
        let cfg = include_str!("../tauri.conf.json");
        assert!(
            !cfg.contains(PUBKEY_PLACEHOLDER),
            "tauri.conf.json regressed to the updater pubkey PLACEHOLDER — paste the real \
             public key (see docs/RELEASE.md)"
        );
        assert!(
            !cfg.contains("\"pubkey\": \"\""),
            "the updater pubkey is empty — the auto-updater would be unarmed"
        );
    }

    #[test]
    fn not_configured_result_is_honest_and_carries_no_version() {
        let r = UpdateCheck::not_configured();
        assert_eq!(r.status, "not_configured");
        assert!(r.version.is_none());
        // It points the user at the one-time setup rather than pretending.
        assert!(r.detail.contains("docs/RELEASE.md"));
    }

    #[test]
    fn up_to_date_and_error_shapes() {
        assert_eq!(UpdateCheck::up_to_date().status, "up_to_date");
        let e = UpdateCheck::error("offline");
        assert_eq!(e.status, "error");
        assert!(e.detail.contains("offline"));
        assert!(e.version.is_none());
    }
}
