//! First-run INSTALLER affordance — the backend side of the FIRST-RUN SETUP gate.
//!
//! WHAT THIS IS: two Tauri commands that make a freshly-downloaded JARVIS .app a
//! complete first-run installer.
//!
//!   * `backend_installed()` -> bool: an HONEST presence check of the installed
//!     JARVIS backend in the per-user install home
//!     (`~/Library/Application Support/JARVIS`). It is true ONLY when BOTH the
//!     built daemon binary (`daemon/target/release/jarvisd`) AND the Python venv
//!     interpreter (`.venv/bin/python`) exist under that home — the two artifacts
//!     install.sh produces that prove a real install. It never guesses: a missing
//!     either-one reads false.
//!
//!   * `open_setup_install()` -> opens Terminal.app running the PUBLIC install
//!     one-liner
//!     curl -fsSL https://raw.githubusercontent.com/darwin-capani/jarvis/main/install.sh | bash -s -- -y
//!     This MIRRORS uninstall.rs's `open_uninstall` osascript/Terminal pattern.
//!
//! WHY TERMINAL (do NOT change to a GUI-spawned child): the installer provisions
//! Homebrew, which needs a `sudo` password prompt. A process spawned by the GUI
//! app has NO controlling tty, so that prompt would fail outright. Terminal.app
//! gives the script a real tty (and shows the cinematic install). We REUSE the
//! real, working `install.sh` here — we do NOT reimplement provisioning in Rust.
//!
//! HONESTY: this opens a terminal on the public installer; it does NOT itself
//! install anything, and it returns whether Terminal was actually launched rather
//! than pretending. The command takes NO argument from the frontend — the URL +
//! flags are constants, so there is no injection surface.

use std::path::PathBuf;
use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

/// The PUBLIC install one-liner the Terminal runs. `-y` answers the JARVIS
/// actuation consent prompts; Homebrew's own `sudo` prompt still asks once (which
/// is exactly why we need the Terminal tty). This is a fixed constant — there is
/// no caller-supplied component, so nothing can be injected through it.
pub const INSTALL_ONE_LINER: &str =
    "curl -fsSL https://raw.githubusercontent.com/darwin-capani/jarvis/main/install.sh | bash -s -- -y";

/// The per-user JARVIS install home: `~/Library/Application Support/JARVIS`. We
/// resolve `~` from `$HOME` (always set for a GUI/login session on macOS); when it
/// is somehow unset we return None and `backend_installed` reads false (we never
/// guess a home).
fn install_home() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(
        PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("JARVIS"),
    )
}

/// PURE presence decision: given the install home and a "does this path exist?"
/// probe, the backend is "installed" iff BOTH the built daemon binary AND the
/// venv python interpreter exist under it. Split out from the filesystem so the
/// AND logic is unit-testable without touching disk. The two relative paths are
/// exactly the artifacts install.sh produces:
///   * `daemon/target/release/jarvisd` — the release daemon binary (BUILD stage)
///   * `.venv/bin/python`              — the Python venv interpreter (PYTHON ENV stage)
fn is_installed_at(home: &std::path::Path, exists: impl Fn(&std::path::Path) -> bool) -> bool {
    let daemon = home.join("daemon").join("target").join("release").join("jarvisd");
    let venv_python = home.join(".venv").join("bin").join("python");
    exists(&daemon) && exists(&venv_python)
}

/// HONEST presence check of the installed JARVIS backend. True ONLY when the
/// per-user install home has BOTH the built daemon binary and the venv python —
/// the two artifacts that prove a real install. A missing `$HOME`, a missing
/// home directory, or either artifact absent all read false.
///
/// NOTE the gate is deliberately CONSERVATIVE in the HUD: even when this returns
/// false, the FIRST-RUN SETUP screen only shows if the command channel ALSO is
/// not connected (a running daemon means the channel connects). So a quirky
/// false-negative here can never nag a user whose JARVIS is actually running.
#[tauri::command]
pub async fn backend_installed() -> Result<bool, String> {
    let Some(home) = install_home() else {
        return Ok(false);
    };
    // Off the async runtime: a few stat()s, but keep the UI thread clean.
    let installed = tauri::async_runtime::spawn_blocking(move || {
        is_installed_at(&home, |p| p.exists())
    })
    .await
    .map_err(|e| format!("install check task failed: {e}"))?;
    Ok(installed)
}

/// The outcome surfaced to the HUD. `opened` is true only when Terminal.app was
/// actually launched on the installer; `detail` is a short human line (never a
/// secret — the only material is a public URL).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SetupOpen {
    pub opened: bool,
    pub detail: String,
}

/// Escape a string for safe embedding inside an AppleScript DOUBLE-quoted string
/// literal — a backslash and a double quote are the only metacharacters inside an
/// AppleScript "..." string. Identical to uninstall.rs's escape so the inert
/// property holds: after escaping, no `"` can terminate the surrounding literal
/// early. (Our payload is a fixed constant with no quotes, but we escape it
/// anyway so the construction is correct by the same rule the uninstaller uses.)
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build the AppleScript that brings Terminal forward and runs `command`. Split
/// from the I/O so the exact Terminal command we send is unit-testable. Mirrors
/// uninstall.rs: `tell application "Terminal" / activate / do script "<cmd>" / end tell`.
fn build_terminal_applescript(command: &str) -> String {
    format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        escape_applescript(command)
    )
}

/// OPEN Terminal.app running the public `install.sh` one-liner. This REUSES the
/// real installer (its self-bootstrap + provisioning + sudo handling) rather than
/// reimplementing any of it. Honest about failure (Terminal could not be
/// launched) rather than pretending it ran. The command takes NO argument from
/// the frontend (the one-liner is a constant), so there is no injection surface;
/// we shell the same `/usr/bin/osascript` the uninstaller uses, so no new
/// capability is added.
#[tauri::command]
pub async fn open_setup_install() -> Result<SetupOpen, String> {
    let applescript = build_terminal_applescript(INSTALL_ONE_LINER);

    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&applescript)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not launch Terminal: {e}"))?;

    if output.status.success() {
        Ok(SetupOpen {
            opened: true,
            detail:
                "Opened Terminal running the JARVIS installer. It installs the dependencies, builds \
                 the daemon, creates the Python environment, and downloads the on-device models \
                 (several GB) — macOS will ask once for your password. When it finishes, JARVIS \
                 connects automatically (or relaunch the app)."
                    .into(),
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(SetupOpen {
            opened: false,
            detail: format!(
                "could not open Terminal: {}",
                stderr.trim().lines().next().unwrap_or("no GUI session")
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn install_one_liner_is_the_public_curl_bash_yes_command() {
        // The exact Terminal command: the public raw install.sh piped to bash with
        // the -y auto-confirm. This is the REAL working installer one-liner.
        assert_eq!(
            INSTALL_ONE_LINER,
            "curl -fsSL https://raw.githubusercontent.com/darwin-capani/jarvis/main/install.sh | bash -s -- -y"
        );
        // The load-bearing pieces, asserted individually so a typo can't slip by.
        assert!(INSTALL_ONE_LINER.starts_with("curl -fsSL "));
        assert!(INSTALL_ONE_LINER
            .contains("https://raw.githubusercontent.com/darwin-capani/jarvis/main/install.sh"));
        assert!(INSTALL_ONE_LINER.contains("| bash -s -- -y"));
    }

    #[test]
    fn terminal_applescript_runs_the_one_liner_in_terminal() {
        let script = build_terminal_applescript(INSTALL_ONE_LINER);
        // It targets Terminal, activates it, and `do script`s the installer — the
        // SAME shape uninstall.rs uses to open the uninstaller.
        assert!(script.contains("tell application \"Terminal\""));
        assert!(script.contains("activate"));
        assert!(script.contains("do script"));
        assert!(script.contains("end tell"));
        // The actual installer command rides inside the AppleScript string.
        assert!(script.contains(INSTALL_ONE_LINER));
    }

    #[test]
    fn applescript_escaping_neutralizes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("plain"), "plain");
        assert_eq!(escape_applescript(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_applescript(r"a\b"), r"a\\b");
        // The inert property: in the escaped output every double-quote is preceded
        // by a backslash, so none can terminate the surrounding "..." literal early.
        let hostile = r#"x"; do shell script "rm -rf ~"#;
        let esc = escape_applescript(hostile);
        let bytes = esc.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'"' {
                assert!(i > 0 && bytes[i - 1] == b'\\', "un-escaped quote at {i}: {esc}");
            }
        }
    }

    #[test]
    fn is_installed_requires_both_the_daemon_binary_and_the_venv_python() {
        let home = Path::new("/Users/x/Library/Application Support/JARVIS");
        let daemon = home.join("daemon").join("target").join("release").join("jarvisd");
        let venv = home.join(".venv").join("bin").join("python");

        // BOTH present -> installed.
        assert!(is_installed_at(home, |p| p == daemon || p == venv));
        // Only the daemon -> NOT installed (no venv = no working backend).
        assert!(!is_installed_at(home, |p| p == daemon));
        // Only the venv -> NOT installed (no daemon binary = nothing to run).
        assert!(!is_installed_at(home, |p| p == venv));
        // Neither -> NOT installed (the fresh-download / dev-tree case).
        assert!(!is_installed_at(home, |_| false));
    }

    #[test]
    fn is_installed_probes_exactly_the_install_sh_artifacts() {
        // Pin the two relative paths so a refactor cannot silently start checking a
        // different (wrong) artifact. These are EXACTLY what install.sh produces.
        // `exists` is an `Fn`, so record probed paths via interior mutability.
        let home = Path::new("/H");
        let probed = std::cell::RefCell::new(Vec::<String>::new());
        let _ = is_installed_at(home, |p| {
            probed.borrow_mut().push(p.to_string_lossy().into_owned());
            true
        });
        let probed = probed.into_inner();
        assert!(probed.iter().any(|p| p.ends_with("/H/daemon/target/release/jarvisd")));
        assert!(probed.iter().any(|p| p.ends_with("/H/.venv/bin/python")));
    }

    #[test]
    fn setup_open_result_shape() {
        let r = SetupOpen { opened: true, detail: "ok".into() };
        assert!(r.opened);
    }
}
