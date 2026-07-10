//! Local security posture check for agent "aegis" (Defense & Privacy).
//!
//! DEFENSIVE-ONLY, READ-ONLY, the LOCAL machine. This module reports the Mac's own
//! security posture — FileVault (disk encryption) on/off, the application firewall
//! on/off, System Integrity Protection (SIP) status, and whether software updates
//! are pending — by READING the system with the SAME explicit-args, bounded
//! subprocess discipline `actions.rs` uses (`run_command`-style: a fixed program +
//! fixed args, NEVER a shell string; 5s timeout; kill_on_drop). It CHANGES nothing:
//! Aegis reports where the user is exposed; turning FileVault or the firewall ON is
//! the user's own action, taken in System Settings — there is no remediation path
//! here, not even a gated one.
//!
//! NO offensive capability: it inspects ONLY this machine, never another host. The
//! four read commands are the standard macOS posture queries, each a read-only
//! status command:
//!   * FileVault: `/usr/bin/fdesetup status`
//!   * Application firewall: `/usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate`
//!   * SIP: `/usr/bin/csrutil status`
//!   * Pending updates: `/usr/sbin/softwareupdate -l --no-scan`
//!
//! The command RUNNER is injected (a function pointer), so the report builder is
//! driven in tests with CANNED command output — the real privileged/system commands
//! are NEVER spawned under test. The pure PARSERS (one per command's output shape)
//! are unit-tested directly on hand-written canned text.

use std::time::Duration;

use anyhow::Result;
use tokio::process::Command;
use tracing::warn;

/// Hard ceiling per spawned posture command — same 5s discipline as actions.rs.
/// `softwareupdate -l` can be slow, so it gets a longer budget below.
const POSTURE_TIMEOUT: Duration = Duration::from_secs(5);
/// `softwareupdate -l` is the one slow read; give it more headroom but still bound
/// it so a wedged check can never hang the caller.
const SOFTWAREUPDATE_TIMEOUT: Duration = Duration::from_secs(30);

// Read-only system posture commands. Each is an absolute program path + fixed
// args — never a shell string, exactly like actions.rs::run_command.
const FDESETUP: &str = "/usr/bin/fdesetup";
const SOCKETFILTERFW: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";
const CSRUTIL: &str = "/usr/bin/csrutil";
const SOFTWAREUPDATE: &str = "/usr/sbin/softwareupdate";

/// A read-only system command (program + args) plus its per-call timeout.
struct PostureCmd {
    program: &'static str,
    args: &'static [&'static str],
    timeout: Duration,
}

/// The four posture reads, in report order. Factored out so the exact, read-only
/// invocations are assertable in tests WITHOUT running them.
fn posture_commands() -> [PostureCmd; 4] {
    [
        PostureCmd { program: FDESETUP, args: &["status"], timeout: POSTURE_TIMEOUT },
        PostureCmd {
            program: SOCKETFILTERFW,
            args: &["--getglobalstate"],
            timeout: POSTURE_TIMEOUT,
        },
        PostureCmd { program: CSRUTIL, args: &["status"], timeout: POSTURE_TIMEOUT },
        PostureCmd {
            program: SOFTWAREUPDATE,
            args: &["-l", "--no-scan"],
            timeout: SOFTWAREUPDATE_TIMEOUT,
        },
    ]
}

/// The captured outcome of one posture read: either the command's combined
/// stdout+stderr text, or a note that the read itself could not run (the command
/// is missing, timed out, or errored). The builder degrades honestly per check —
/// one unreadable check never sinks the whole report.
enum ReadOutput {
    Text(String),
    Unavailable(String),
}

// ---------------------------------------------------------------------------
// Public entry: the read-only posture report
// ---------------------------------------------------------------------------

/// Read the local machine's security posture and return a concise human summary —
/// what Aegis would say. READ-ONLY: it runs only the status commands above and
/// changes nothing. Each check degrades honestly: if one command can't run, that
/// line says "couldn't read …" rather than guessing. Remediation (turning a
/// protection ON) is the user's own action and is named as such.
pub async fn local_posture() -> Result<String> {
    Ok(build_report(run_real_command).await)
}

/// Compose the three READ-ONLY defensive reads into one "full security check"
/// readout. Pure (the fetches live in `security_report`), so the section layout
/// and honest framing are unit-tested. Each input is already a human summary that
/// degrades honestly on its own; this only labels and orders them.
fn compose_security_report(posture: &str, tcc: &str, introspect: &str) -> String {
    format!(
        "Full security check — READ-ONLY: I report where you stand; turning a \
protection on or changing a permission is yours to do in System Settings.\n\n\
1) Machine posture — {posture}\n\n\
2) App privacy grants (TCC) — {tcc}\n\n\
3) Micro-app introspection — {introspect}"
    )
}

/// One combined defensive readout: machine posture (FileVault/firewall/SIP/updates),
/// macOS app-privacy grants (TCC), and the micro-app introspection sentinel. Each
/// sub-read degrades honestly to a "couldn't read" line rather than failing the
/// whole report. READ-ONLY — it reports; it changes nothing and holds no
/// remediation path (turning a protection on is the user's own action).
pub async fn security_report() -> Result<String> {
    let posture = build_report(run_real_command).await;
    let tcc = crate::tcc::snapshot()
        .await
        .unwrap_or_else(|e| format!("couldn't read the privacy database ({e})"));
    let introspect = crate::introspect::status_summary();
    Ok(compose_security_report(&posture, &tcc, &introspect))
}

/// Build the report by running each posture command through `run` (injected so
/// tests can feed canned output) and folding each result through its pure parser.
/// Pure of any real subprocess itself — the subprocess lives behind `run`.
async fn build_report<F, Fut>(run: F) -> String
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: std::future::Future<Output = ReadOutput>,
{
    let cmds = posture_commands();
    let mut lines: Vec<String> = Vec::with_capacity(cmds.len());

    // FileVault
    lines.push(match run(cmds[0].program, cmds[0].args, cmds[0].timeout).await {
        ReadOutput::Text(t) => parse_filevault(&t),
        ReadOutput::Unavailable(why) => format!("FileVault: couldn't read ({why})"),
    });
    // Firewall
    lines.push(match run(cmds[1].program, cmds[1].args, cmds[1].timeout).await {
        ReadOutput::Text(t) => parse_firewall(&t),
        ReadOutput::Unavailable(why) => format!("Firewall: couldn't read ({why})"),
    });
    // SIP
    lines.push(match run(cmds[2].program, cmds[2].args, cmds[2].timeout).await {
        ReadOutput::Text(t) => parse_sip(&t),
        ReadOutput::Unavailable(why) => format!("System Integrity Protection: couldn't read ({why})"),
    });
    // Pending updates
    lines.push(match run(cmds[3].program, cmds[3].args, cmds[3].timeout).await {
        ReadOutput::Text(t) => parse_updates(&t),
        ReadOutput::Unavailable(why) => format!("Software updates: couldn't read ({why})"),
    });

    // Micro-app introspection (introspect.rs): a secret-free, read-only tally of
    // the sandboxed-child sentinel (profile-drift / resource-anomalies / module-
    // violations). None until the sentinel has ticked — then this line joins the
    // posture the same way FileVault/Firewall/SIP do. It reports; it never acts.
    if let Some(line) = crate::introspect::posture_line() {
        lines.push(line);
    }

    format!(
        "Security posture (read-only — I report it; turning anything on is yours to do in System Settings): {}",
        lines.join("; ")
    )
}

// ---------------------------------------------------------------------------
// Real command runner (NEVER reached in tests — they inject canned output)
// ---------------------------------------------------------------------------

/// Spawn one read-only posture command with explicit args (never a shell string),
/// capture its combined stdout+stderr as text, and bound it with its timeout +
/// kill_on_drop — mirroring actions.rs::run_command. A spawn error, non-UTF8
/// output, or timeout becomes a `ReadOutput::Unavailable` so the builder degrades
/// to a "couldn't read" line for that one check rather than failing the report.
async fn run_real_command(
    program: &'static str,
    args: &'static [&'static str],
    timeout: Duration,
) -> ReadOutput {
    let mut cmd = Command::new(program);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => {
            // Some posture tools (csrutil, softwareupdate) write to stderr; combine
            // both so the parser sees the full status text.
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            ReadOutput::Text(text)
        }
        Ok(Err(e)) => {
            warn!(program, error = %e, "posture: command could not run");
            ReadOutput::Unavailable("not available on this machine".to_string())
        }
        Err(_) => {
            warn!(program, secs = timeout.as_secs(), "posture: command timed out");
            ReadOutput::Unavailable("the check timed out".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Pure parsers — one per command, unit-tested on canned output
// ---------------------------------------------------------------------------

/// Parse `fdesetup status` output. "FileVault is On." / "FileVault is Off."
fn parse_filevault(out: &str) -> String {
    let lower = out.to_lowercase();
    if lower.contains("filevault is on") {
        "FileVault: ON (disk encrypted)".to_string()
    } else if lower.contains("filevault is off") {
        "FileVault: OFF — your disk is not encrypted; turn FileVault on in System Settings".to_string()
    } else {
        "FileVault: status unclear".to_string()
    }
}

/// Parse `socketfilterfw --getglobalstate` output. "Firewall is enabled. (State =
/// 1)" / "Firewall is disabled. (State = 0)"
fn parse_firewall(out: &str) -> String {
    let lower = out.to_lowercase();
    if lower.contains("enabled") || lower.contains("state = 1") || lower.contains("state = 2") {
        "Firewall: ON".to_string()
    } else if lower.contains("disabled") || lower.contains("state = 0") {
        "Firewall: OFF — turn the application firewall on in System Settings".to_string()
    } else {
        "Firewall: status unclear".to_string()
    }
}

/// Parse `csrutil status` output. "System Integrity Protection status: enabled." /
/// "… disabled."
fn parse_sip(out: &str) -> String {
    let lower = out.to_lowercase();
    if lower.contains("status: enabled") || lower.contains("status : enabled") {
        "System Integrity Protection: ON".to_string()
    } else if lower.contains("status: disabled") || lower.contains("status : disabled") {
        "System Integrity Protection: OFF — re-enable SIP unless you have a specific reason not to".to_string()
    } else {
        "System Integrity Protection: status unclear".to_string()
    }
}

/// Parse `softwareupdate -l --no-scan` output. When updates are pending the output
/// lists labelled entries ("* Label:" / "Title:"); "No new software available."
/// means up to date.
fn parse_updates(out: &str) -> String {
    let lower = out.to_lowercase();
    if lower.contains("no new software available") {
        return "Software updates: up to date".to_string();
    }
    // Count the labelled update entries softwareupdate prints (lines beginning
    // with "* Label:" on modern macOS, or "* " on older). Each is one pending
    // update.
    let count = out
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("* Label:") || l.starts_with("* "))
        .count();
    if count > 0 {
        format!(
            "Software updates: {count} pending — install them; updates close known security holes"
        )
    } else {
        "Software updates: status unclear".to_string()
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: the report builder is driven with an INJECTED command
// runner returning hand-written canned output. The real privileged/system
// commands are NEVER spawned here. The pure parsers are tested directly.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_security_report_labels_all_three_reads_and_stays_read_only() {
        let s = compose_security_report(
            "FileVault: ON; Firewall: ON",
            "12 grants, 2 high-risk allowed",
            "3 app(s) observed, 0 profile-drift",
        );
        // All three sub-reads are present and labeled in order.
        assert!(s.contains("1) Machine posture — FileVault: ON"));
        assert!(s.contains("2) App privacy grants (TCC) — 12 grants"));
        assert!(s.contains("3) Micro-app introspection — 3 app(s) observed"));
        // The read-only framing is stated and no remediation is implied.
        assert!(s.contains("READ-ONLY"));
        assert!(s.to_lowercase().contains("yours to do"));
    }

    // -- the read-only command set is exactly the four status invocations -----

    /// The posture reads are the standard READ-ONLY macOS status commands, each an
    /// absolute program + fixed args (never a shell string). Asserted WITHOUT
    /// running them. This pins that Aegis never gains a write/remediation argv.
    #[test]
    fn posture_commands_are_the_read_only_status_invocations() {
        let cmds = posture_commands();
        assert_eq!(cmds[0].program, "/usr/bin/fdesetup");
        assert_eq!(cmds[0].args, &["status"]);
        assert_eq!(cmds[1].program, "/usr/libexec/ApplicationFirewall/socketfilterfw");
        assert_eq!(cmds[1].args, &["--getglobalstate"]);
        assert_eq!(cmds[2].program, "/usr/bin/csrutil");
        assert_eq!(cmds[2].args, &["status"]);
        assert_eq!(cmds[3].program, "/usr/sbin/softwareupdate");
        assert_eq!(cmds[3].args, &["-l", "--no-scan"]);
        // None of the invocations is a write/enable/disable/remediation action.
        for c in &cmds {
            for a in c.args {
                let a = a.to_lowercase();
                assert!(!a.contains("enable"), "no enable arg: {a}");
                assert!(!a.contains("disable"), "no disable arg: {a}");
                assert!(!a.contains("--install"), "no install arg: {a}");
                assert!(!a.contains("-i"), "no install shorthand: {a}");
            }
        }
    }

    // -- pure parsers --------------------------------------------------------

    #[test]
    fn filevault_parser_reads_on_off_and_unclear() {
        assert!(parse_filevault("FileVault is On.").contains("ON"));
        assert!(parse_filevault("FileVault is Off.").contains("OFF"));
        assert!(parse_filevault("FileVault is Off.").to_lowercase().contains("system settings"));
        assert!(parse_filevault("something else entirely").contains("unclear"));
    }

    #[test]
    fn firewall_parser_reads_on_off_and_unclear() {
        assert!(parse_firewall("Firewall is enabled. (State = 1)").contains("ON"));
        assert!(parse_firewall("Firewall is enabled. (State = 2)").contains("ON"));
        assert!(parse_firewall("Firewall is disabled. (State = 0)").contains("OFF"));
        assert!(parse_firewall("Firewall is disabled. (State = 0)").to_lowercase().contains("system settings"));
        assert!(parse_firewall("mystery").contains("unclear"));
    }

    #[test]
    fn sip_parser_reads_on_off_and_unclear() {
        assert!(parse_sip("System Integrity Protection status: enabled.").contains("ON"));
        assert!(parse_sip("System Integrity Protection status: disabled.").contains("OFF"));
        assert!(parse_sip("System Integrity Protection status: disabled.").to_lowercase().contains("re-enable"));
        assert!(parse_sip("nope").contains("unclear"));
    }

    #[test]
    fn updates_parser_counts_pending_and_reads_up_to_date() {
        assert!(parse_updates("No new software available.").contains("up to date"));
        let pending = "Software Update Tool\n\n* Label: macOS Sequoia 15.5-24F74\n\tTitle: macOS Sequoia, Size: 1234567K\n* Label: Safari17.5\n\tTitle: Safari";
        let out = parse_updates(pending);
        assert!(out.contains("2 pending"), "got: {out}");
        assert!(out.to_lowercase().contains("install"), "remediation hint: {out}");
        // Older-style "* " entries also count.
        let older = "Software Update Tool\n\n   * Command Line Tools\n   * XProtectPlistConfigData";
        assert!(parse_updates(older).contains("2 pending"), "older shape: {}", parse_updates(older));
        assert!(parse_updates("weird output").contains("unclear"));
    }

    // -- the full report builder, driven by an INJECTED canned runner ---------

    /// A canned runner that returns text per program — the real system commands are
    /// NEVER spawned. Tests the whole report fold without privilege or network.
    fn canned(
        fv: &'static str,
        fw: &'static str,
        sip: &'static str,
        upd: &'static str,
    ) -> impl Fn(&'static str, &'static [&'static str], Duration) -> std::future::Ready<ReadOutput>
    {
        move |program: &'static str, _args, _to| {
            let text = match program {
                "/usr/bin/fdesetup" => fv,
                "/usr/libexec/ApplicationFirewall/socketfilterfw" => fw,
                "/usr/bin/csrutil" => sip,
                "/usr/sbin/softwareupdate" => upd,
                _ => "",
            };
            std::future::ready(ReadOutput::Text(text.to_string()))
        }
    }

    #[tokio::test]
    async fn report_folds_all_four_checks_when_protected() {
        let run = canned(
            "FileVault is On.",
            "Firewall is enabled. (State = 1)",
            "System Integrity Protection status: enabled.",
            "No new software available.",
        );
        let report = build_report(run).await;
        assert!(report.contains("read-only"), "states read-only: {report}");
        assert!(report.contains("FileVault: ON"), "{report}");
        assert!(report.contains("Firewall: ON"), "{report}");
        assert!(report.contains("System Integrity Protection: ON"), "{report}");
        assert!(report.contains("up to date"), "{report}");
        // The report says remediation is the user's action, never that Aegis did it.
        assert!(report.to_lowercase().contains("system settings"), "{report}");
    }

    #[tokio::test]
    async fn report_flags_exposure_when_unprotected() {
        let run = canned(
            "FileVault is Off.",
            "Firewall is disabled. (State = 0)",
            "System Integrity Protection status: disabled.",
            "Software Update Tool\n\n* Label: macOS Sequoia 15.5\n* Label: Safari17.5",
        );
        let report = build_report(run).await;
        assert!(report.contains("FileVault: OFF"), "{report}");
        assert!(report.contains("Firewall: OFF"), "{report}");
        assert!(report.contains("System Integrity Protection: OFF"), "{report}");
        assert!(report.contains("2 pending"), "{report}");
    }

    /// One unreadable check degrades to a "couldn't read" line — it never sinks the
    /// whole report, and never fabricates a status.
    #[tokio::test]
    async fn an_unavailable_check_degrades_honestly() {
        let run = |program: &'static str, _args: &'static [&'static str], _to: Duration| {
            let out = if program == "/usr/bin/fdesetup" {
                ReadOutput::Unavailable("not available on this machine".to_string())
            } else if program == "/usr/libexec/ApplicationFirewall/socketfilterfw" {
                ReadOutput::Text("Firewall is enabled. (State = 1)".to_string())
            } else if program == "/usr/bin/csrutil" {
                ReadOutput::Text("System Integrity Protection status: enabled.".to_string())
            } else {
                ReadOutput::Text("No new software available.".to_string())
            };
            std::future::ready(out)
        };
        let report = build_report(run).await;
        assert!(report.contains("FileVault: couldn't read"), "degrades honestly: {report}");
        // The other three still report normally.
        assert!(report.contains("Firewall: ON"), "{report}");
        assert!(report.contains("System Integrity Protection: ON"), "{report}");
    }
}
