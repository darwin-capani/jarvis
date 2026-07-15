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

    // Persistence Sentinel (persistence.rs): a secret-free, read-only summary of
    // the host's autostart surfaces (LaunchAgents/Daemons/login-items/cron/kexts)
    // + how many are unsigned + the Gatekeeper switch. None until the sentinel has
    // scanned — then this line joins the posture the same way the others do. It
    // reports; it never acts on a persistence finding.
    if let Some(line) = crate::persistence::posture_line() {
        lines.push(line);
    }

    // Inbound Exposure Auditor (exposure.rs): a secret-free, read-only summary of
    // THIS machine's own listening sockets — how many are loopback-only vs exposed
    // to the network, with the exposed well-known sharing services named. None
    // until the auditor has scanned the local socket table — then this line joins
    // the posture the same way the others do. It reports (and sends no packets); it
    // never closes a port itself.
    if let Some(line) = crate::exposure::posture_line() {
        lines.push(line);
    }

    format!(
        "Security posture (read-only — I report it; turning anything on is yours to do in System Settings): {}",
        lines.join("; ")
    )
}

// ---------------------------------------------------------------------------
// Structured snapshot (F16): the HUD PostureDashboardPanel's wire payload
// ---------------------------------------------------------------------------

/// Build the STRUCTURED `posture.snapshot` payload by running the SAME four
/// read-only commands and folding each through its verdict classifier. WIRE
/// CONTRACT (mirrored by hud/src/core/events.ts::parsePostureSnapshot — exact
/// field names pinned by tests on both sides):
///
///   { "filevault": "on"|"off"|"unclear"|"unreadable",
///     "firewall":  same,
///     "sip":       same,
///     "updates":   "up_to_date"|"pending"|"unclear"|"unreadable",
///     "updates_pending": <count, 0 unless "pending"> }
///
/// SECRET-FREE BY CONSTRUCTION: only these fixed verdict tokens and a count
/// ever reach the wire — never a byte of raw command output. Scope is
/// deliberately ONLY the four machine checks: TCC grant counts and micro-app
/// introspection already ship on their own events (`tcc.snapshot`,
/// `introspect.snapshot`) — re-emitting them here would double-scan those
/// stores and split the source of truth.
async fn build_snapshot<F, Fut>(run: F) -> serde_json::Value
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: std::future::Future<Output = ReadOutput>,
{
    let cmds = posture_commands();
    let filevault = match run(cmds[0].program, cmds[0].args, cmds[0].timeout).await {
        ReadOutput::Text(t) => classify_filevault(&t).wire(),
        ReadOutput::Unavailable(_) => "unreadable",
    };
    let firewall = match run(cmds[1].program, cmds[1].args, cmds[1].timeout).await {
        ReadOutput::Text(t) => classify_firewall(&t).wire(),
        ReadOutput::Unavailable(_) => "unreadable",
    };
    let sip = match run(cmds[2].program, cmds[2].args, cmds[2].timeout).await {
        ReadOutput::Text(t) => classify_sip(&t).wire(),
        ReadOutput::Unavailable(_) => "unreadable",
    };
    let (updates, updates_pending) = match run(cmds[3].program, cmds[3].args, cmds[3].timeout).await
    {
        ReadOutput::Text(t) => match classify_updates(&t) {
            Updates::UpToDate => ("up_to_date", 0),
            Updates::Pending(n) => ("pending", n),
            Updates::Unclear => ("unclear", 0),
        },
        ReadOutput::Unavailable(_) => ("unreadable", 0),
    };
    serde_json::json!({
        "filevault": filevault,
        "firewall": firewall,
        "sip": sip,
        "updates": updates,
        "updates_pending": updates_pending,
    })
}

/// The last emitted snapshot payload (with its `checked_ts` stamp). The
/// telemetry hub is fire-and-forget with no replay-on-connect, and this is the
/// daemon's slowest feed (30 min) — without a cache, a freshly launched HUD
/// would show no posture board until the next scan. The fast snapshot pass
/// re-broadcasts THIS cached value (a pure in-process read; the heavy
/// subprocess scan stays on its own slow task).
static LAST_SNAPSHOT: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);

/// Lock the cache, recovering from poisoning rather than panicking.
fn cache_lock() -> std::sync::MutexGuard<'static, Option<serde_json::Value>> {
    LAST_SNAPSHOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Build the snapshot, stamp `checked_ts` (WHEN the commands actually ran —
/// the re-broadcast below reuses this payload, so the envelope ts alone would
/// misstate the data's age), and cache it. Factored from [`emit_snapshot`] so
/// tests drive it with the canned runner — the real commands never spawn under
/// test.
async fn snapshot_and_cache<F, Fut>(run: F) -> serde_json::Value
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: std::future::Future<Output = ReadOutput>,
{
    let mut payload = build_snapshot(run).await;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("checked_ts".to_string(), serde_json::json!(chrono::Utc::now().to_rfc3339()));
    }
    *cache_lock() = Some(payload.clone());
    payload
}

/// Run the four checks and emit `posture.snapshot` for the HUD dashboard.
/// READ-ONLY (the same status commands as [`local_posture`]) and SECRET-FREE
/// (fixed verdict tokens, never raw output). Called on its own SLOW cadence
/// (main.rs::posture_snapshot_task) — each tick spawns real subprocesses with
/// a 30s budget for `softwareupdate`, far too heavy for the 15s snapshot pass.
pub async fn emit_snapshot() {
    let payload = snapshot_and_cache(run_real_command).await;
    crate::telemetry::emit("system", "posture.snapshot", payload);
}

/// Re-broadcast the CACHED snapshot (if any scan has completed). A pure
/// in-process read — no subprocess — so the fast audit-snapshot pass can call
/// it every tick and a freshly connected HUD gets the board within seconds
/// instead of waiting out the 30-minute scan interval. The payload's own
/// `checked_ts` keeps the data's true age honest. No-op before the first scan
/// (never fabricates a board).
pub fn re_emit_cached() {
    if let Some(payload) = cache_lock().clone() {
        crate::telemetry::emit("system", "posture.snapshot", payload);
    }
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
// Pure classifiers + prose parsers — one per command, unit-tested on canned
// output. The CLASSIFIER is the single source of truth for a check's verdict;
// the prose parser (the spoken Aegis reply) and the structured snapshot payload
// (the HUD dashboard) are both thin views over it, so they can never disagree.
// ---------------------------------------------------------------------------

/// The three-way verdict for one protection check. `Unclear` is the honest
/// can't-confirm reading for unrecognized output — never coerced to On.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protection {
    On,
    Off,
    Unclear,
}

impl Protection {
    /// The wire token for `posture.snapshot` (mirrored by the HUD parser).
    fn wire(self) -> &'static str {
        match self {
            Protection::On => "on",
            Protection::Off => "off",
            Protection::Unclear => "unclear",
        }
    }
}

/// The verdict for the pending-updates check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Updates {
    UpToDate,
    Pending(usize),
    Unclear,
}

/// Classify `fdesetup status` output. "FileVault is On." / "FileVault is Off."
fn classify_filevault(out: &str) -> Protection {
    let lower = out.to_lowercase();
    if lower.contains("filevault is on") {
        Protection::On
    } else if lower.contains("filevault is off") {
        Protection::Off
    } else {
        Protection::Unclear
    }
}

/// Classify `socketfilterfw --getglobalstate` output. "Firewall is enabled.
/// (State = 1)" / "Firewall is disabled. (State = 0)". (Branch-order subtlety:
/// "disabled" does NOT contain "enabled", so the first branch is safe.)
fn classify_firewall(out: &str) -> Protection {
    let lower = out.to_lowercase();
    if lower.contains("enabled") || lower.contains("state = 1") || lower.contains("state = 2") {
        Protection::On
    } else if lower.contains("disabled") || lower.contains("state = 0") {
        Protection::Off
    } else {
        Protection::Unclear
    }
}

/// Classify `csrutil status` output. "System Integrity Protection status:
/// enabled." / "… disabled."
fn classify_sip(out: &str) -> Protection {
    let lower = out.to_lowercase();
    if lower.contains("status: enabled") || lower.contains("status : enabled") {
        Protection::On
    } else if lower.contains("status: disabled") || lower.contains("status : disabled") {
        Protection::Off
    } else {
        Protection::Unclear
    }
}

/// Classify `softwareupdate -l --no-scan` output. When updates are pending the
/// output lists labelled entries ("* Label:" on modern macOS, "* " on older);
/// "No new software available." means up to date.
fn classify_updates(out: &str) -> Updates {
    let lower = out.to_lowercase();
    if lower.contains("no new software available") {
        return Updates::UpToDate;
    }
    // Count the labelled update entries softwareupdate prints. Each is one
    // pending update.
    let count = out
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("* Label:") || l.starts_with("* "))
        .count();
    if count > 0 {
        Updates::Pending(count)
    } else {
        Updates::Unclear
    }
}

/// Prose view of the FileVault verdict (the spoken Aegis reply).
fn parse_filevault(out: &str) -> String {
    match classify_filevault(out) {
        Protection::On => "FileVault: ON (disk encrypted)".to_string(),
        Protection::Off => {
            "FileVault: OFF — your disk is not encrypted; turn FileVault on in System Settings"
                .to_string()
        }
        Protection::Unclear => "FileVault: status unclear".to_string(),
    }
}

/// Prose view of the firewall verdict.
fn parse_firewall(out: &str) -> String {
    match classify_firewall(out) {
        Protection::On => "Firewall: ON".to_string(),
        Protection::Off => {
            "Firewall: OFF — turn the application firewall on in System Settings".to_string()
        }
        Protection::Unclear => "Firewall: status unclear".to_string(),
    }
}

/// Prose view of the SIP verdict.
fn parse_sip(out: &str) -> String {
    match classify_sip(out) {
        Protection::On => "System Integrity Protection: ON".to_string(),
        Protection::Off => {
            "System Integrity Protection: OFF — re-enable SIP unless you have a specific reason not to"
                .to_string()
        }
        Protection::Unclear => "System Integrity Protection: status unclear".to_string(),
    }
}

/// Prose view of the updates verdict.
fn parse_updates(out: &str) -> String {
    match classify_updates(out) {
        Updates::UpToDate => "Software updates: up to date".to_string(),
        Updates::Pending(count) => format!(
            "Software updates: {count} pending — install them; updates close known security holes"
        ),
        Updates::Unclear => "Software updates: status unclear".to_string(),
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

    // -- the structured posture.snapshot payload (F16, HUD wire contract) -----

    /// Protected machine: every verdict token is the exact wire string the HUD
    /// parser (events.ts::parsePostureSnapshot) matches on — pinned here.
    #[tokio::test]
    async fn snapshot_reports_wire_verdict_tokens_when_protected() {
        let run = canned(
            "FileVault is On.",
            "Firewall is enabled. (State = 1)",
            "System Integrity Protection status: enabled.",
            "No new software available.",
        );
        let snap = build_snapshot(run).await;
        assert_eq!(snap["filevault"], "on");
        assert_eq!(snap["firewall"], "on");
        assert_eq!(snap["sip"], "on");
        assert_eq!(snap["updates"], "up_to_date");
        assert_eq!(snap["updates_pending"], 0);
        // Exact field set — the wire contract has precisely these five keys.
        let keys: Vec<&String> = snap.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["filevault", "firewall", "sip", "updates", "updates_pending"]);
    }

    #[tokio::test]
    async fn snapshot_flags_exposure_and_counts_pending_updates() {
        let run = canned(
            "FileVault is Off.",
            "Firewall is disabled. (State = 0)",
            "System Integrity Protection status: disabled.",
            "Software Update Tool\n\n* Label: macOS Sequoia 15.5\n* Label: Safari17.5",
        );
        let snap = build_snapshot(run).await;
        assert_eq!(snap["filevault"], "off");
        assert_eq!(snap["firewall"], "off");
        assert_eq!(snap["sip"], "off");
        assert_eq!(snap["updates"], "pending");
        assert_eq!(snap["updates_pending"], 2);
    }

    /// An unreadable check degrades to "unreadable" (never a fabricated verdict),
    /// unrecognized output degrades to "unclear" (never coerced to "on"), and —
    /// the secret-free pin — NOT ONE BYTE of raw command output reaches the
    /// payload, only the fixed verdict tokens.
    #[tokio::test]
    async fn snapshot_degrades_honestly_and_never_carries_raw_output() {
        let run = |program: &'static str, _args: &'static [&'static str], _to: Duration| {
            let out = if program == "/usr/bin/fdesetup" {
                ReadOutput::Unavailable("SENTINEL-REASON not available".to_string())
            } else if program == "/usr/libexec/ApplicationFirewall/socketfilterfw" {
                ReadOutput::Text("SENTINEL-GIBBERISH unrecognized output".to_string())
            } else if program == "/usr/bin/csrutil" {
                ReadOutput::Text("System Integrity Protection status: enabled.".to_string())
            } else {
                ReadOutput::Text("No new software available. SENTINEL-TAIL".to_string())
            };
            std::future::ready(out)
        };
        let snap = build_snapshot(run).await;
        assert_eq!(snap["filevault"], "unreadable");
        assert_eq!(snap["firewall"], "unclear", "gibberish is can't-confirm, never on/off");
        assert_eq!(snap["sip"], "on");
        assert_eq!(snap["updates"], "up_to_date");
        let text = snap.to_string();
        assert!(!text.contains("SENTINEL"), "raw command output must never reach the wire: {text}");
    }

    /// The connect-gap fix: a scan stamps `checked_ts` and caches the payload
    /// (the fast pass re-broadcasts it so a fresh HUD isn't blank for up to
    /// 30 minutes), and the cache is honestly empty before the first scan.
    /// This is the ONLY test that touches the process-global cache.
    #[tokio::test]
    async fn snapshot_stamps_checked_ts_and_caches_for_the_connect_re_emit() {
        // Before any scan the cache re-emit has nothing to send (no-op — a
        // fabricated board must never appear).
        *super::cache_lock() = None;
        re_emit_cached(); // must not panic or emit junk
        assert!(super::cache_lock().is_none());

        let run = canned(
            "FileVault is On.",
            "Firewall is enabled. (State = 1)",
            "System Integrity Protection status: enabled.",
            "No new software available.",
        );
        let payload = snapshot_and_cache(run).await;
        // checked_ts records WHEN the commands ran; the verdicts ride with it.
        assert!(payload["checked_ts"].as_str().is_some_and(|t| t.contains('T')));
        assert_eq!(payload["filevault"], "on");
        // The cache holds the exact emitted payload for the fast-pass re-emit.
        assert_eq!(super::cache_lock().clone().unwrap(), payload);
    }

    /// The classifiers are the single source of truth: prose parser and wire
    /// token can never disagree on the same canned output.
    #[test]
    fn prose_and_wire_views_agree_on_the_same_verdict() {
        for (out, wire, prose_frag) in [
            ("FileVault is On.", "on", "FileVault: ON"),
            ("FileVault is Off.", "off", "FileVault: OFF"),
            ("mystery", "unclear", "status unclear"),
        ] {
            assert_eq!(classify_filevault(out).wire(), wire);
            assert!(parse_filevault(out).contains(prose_frag));
        }
    }
}
