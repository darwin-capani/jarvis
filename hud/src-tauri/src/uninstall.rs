//! WS4a — the "Uninstall JARVIS" affordance (the SETTINGS / System tab).
//!
//! WHAT THIS IS: one Tauri command (`uninstall_open`) that OPENS Terminal.app
//! running the installed `uninstall.sh` so the user completes the script's TWO
//! typed confirmations IN A REAL TERMINAL.
//!
//! WHY THIS SHAPE (the safety invariant — do NOT weaken it): `uninstall.sh` is a
//! deliberately destructive, two-step typed-confirmation flow ("Delete JARVIS
//! completely? (yes/no)" then "Are you ABSOLUTELY sure? (yes/no)", failing safe
//! to NO on anything else). A button must NEVER auto-run that from a single
//! click. So this command does NOT execute the uninstaller and does NOT pass any
//! "yes"/"--force"/auto-confirm flag — it merely OPENS Terminal.app on the
//! script, and the user types both confirmations themselves in the terminal. The
//! script keeps full control of the destructive decision; the HUD only launches a
//! terminal pointed at it. (We pass NO arguments at all, so the script runs in its
//! normal interactive two-step mode.)
//!
//! PATH SAFETY: the script path is resolved from the SAME JARVIS root the command
//! channel + self-heal use (`heal::resolve_root_for_command`) — never a path from
//! the frontend (the command takes NO argument). We verify the file exists and
//! pass it to Terminal as a quoted POSIX path with embedded quotes escaped, so a
//! root containing spaces (the installed home is
//! `~/Library/Application Support/JARVIS`) is handled correctly and nothing can be
//! injected via the path.

use std::path::PathBuf;
use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

/// The outcome surfaced to the HUD. `opened` is true only when Terminal.app was
/// actually launched on the uninstaller; `detail` is a short human line (never a
/// secret — the only material here is a public file path).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct UninstallOpen {
    pub opened: bool,
    pub detail: String,
}

/// Resolve the installed `uninstall.sh` under the JARVIS root. Reuses the shared
/// resolver (JARVIS_ROOT env / the exe-cwd upward walk to the
/// scripts/apply_heal.sh + config/jarvis.toml markers) so we land on the SAME
/// root the rest of the shell uses; the installed root is
/// `~/Library/Application Support/JARVIS`. We never accept a path from the caller.
fn uninstall_script_path() -> Result<PathBuf, String> {
    let root = crate::heal::resolve_root_for_command()?;
    Ok(root.join("uninstall.sh"))
}

/// Escape a string for safe embedding inside an AppleScript DOUBLE-quoted string
/// literal: a backslash and a double quote are the only metacharacters inside an
/// AppleScript "..." string, so escaping both makes any path inert. The path is
/// then handed to Terminal's `do script`, which runs it as a shell command — the
/// path is itself quoted in that shell command too (see `uninstall_open`), so a
/// space or shell metacharacter in the path can never split into extra args.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// OPEN Terminal.app running the installed `uninstall.sh`. Does NOT run the
/// uninstaller itself and passes NO auto-confirm — the user types the script's
/// two confirmations in the terminal. Honest about failure (script missing /
/// Terminal could not be launched) rather than pretending it ran.
///
/// The command takes NO argument from the frontend (the path is resolved
/// server-side), so there is no injection surface. We shell `osascript` to tell
/// Terminal to `do script "<path>"` — the same osascript path the folder picker
/// already uses, so no new dependency / capability is added.
#[tauri::command]
pub async fn uninstall_open() -> Result<UninstallOpen, String> {
    let script = uninstall_script_path()?;
    if !script.is_file() {
        return Ok(UninstallOpen {
            opened: false,
            detail: format!(
                "uninstall.sh not found at {} — this looks like a dev/source tree, not an install. Run it from the installed JARVIS home.",
                script.display()
            ),
        });
    }

    // Build the shell command Terminal will run. The script path is wrapped in
    // single quotes (with any embedded single quote closed/escaped) so a path with
    // spaces stays ONE argument and no shell metacharacter is interpreted. We pass
    // NO arguments to the script -> it runs its normal interactive two-step
    // typed-confirmation flow; the user does the confirming.
    let script_str = script.to_string_lossy();
    let shell_quoted = format!("'{}'", script_str.replace('\'', "'\\''"));
    // The full AppleScript: bring Terminal forward and run the (single-quoted)
    // script path. The whole `do script` argument is an AppleScript string literal,
    // so we escape it for that context too.
    let applescript = format!(
        "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
        escape_applescript(&shell_quoted)
    );

    let output = Command::new("/usr/bin/osascript")
        .arg("-e")
        .arg(&applescript)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("could not launch Terminal: {e}"))?;

    if output.status.success() {
        Ok(UninstallOpen {
            opened: true,
            detail:
                "Opened Terminal running uninstall.sh. Complete the two typed confirmations there — \
                 nothing is removed until you type 'yes' to BOTH prompts. Type 'no' (or close the \
                 window) to cancel."
                    .into(),
        })
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(UninstallOpen {
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

    #[test]
    fn applescript_escaping_neutralizes_quotes_and_backslashes() {
        assert_eq!(escape_applescript("plain"), "plain");
        assert_eq!(escape_applescript(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_applescript(r"a\b"), r"a\\b");
        // A path that tries to close the AppleScript string + inject is fully
        // escaped. The real invariant: in the escaped output EVERY double-quote is
        // immediately preceded by a backslash, so none can terminate the
        // surrounding "..." literal early. We verify that property directly by
        // scanning the bytes (a `"` at index 0, or one whose predecessor is not a
        // backslash, would be an un-neutralized closing quote).
        let hostile = r#"/x"; rm -rf ~; echo ""#;
        let esc = escape_applescript(hostile);
        let bytes = esc.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b == b'"' {
                assert!(i > 0 && bytes[i - 1] == b'\\', "un-escaped quote at {i}: {esc}");
            }
        }
        // And it did escape the quotes (the hostile input had two).
        assert!(esc.contains("\\\""), "quotes are escaped: {esc}");
    }

    #[test]
    fn shell_single_quoting_keeps_a_spaced_path_as_one_arg() {
        // The installed home has a space; single-quoting must preserve it whole and
        // there is no auto-confirm flag appended (the script stays interactive).
        let path = "/Users/x/Library/Application Support/JARVIS/uninstall.sh";
        let quoted = format!("'{}'", path.replace('\'', "'\\''"));
        assert_eq!(quoted, "'/Users/x/Library/Application Support/JARVIS/uninstall.sh'");
        assert!(!quoted.contains("--yes"));
        assert!(!quoted.contains("yes"));
    }

    #[test]
    fn shell_single_quoting_escapes_an_embedded_single_quote() {
        let path = "/Users/o'brien/JARVIS/uninstall.sh";
        let quoted = format!("'{}'", path.replace('\'', "'\\''"));
        // The embedded ' is closed, escaped, and reopened — a valid shell literal.
        assert_eq!(quoted, "'/Users/o'\\''brien/JARVIS/uninstall.sh'");
    }

    #[test]
    fn open_result_shape() {
        let r = UninstallOpen { opened: true, detail: "ok".into() };
        assert!(r.opened);
    }
}
