//! Egress Sentinel — a READ-ONLY view of the host's current outbound network
//! connections (the defensive "what is my Mac talking to right now?" surface).
//!
//! It runs `lsof` with FIXED ARGS (never a shell string), bounded by a timeout
//! with kill_on_drop, and CHANGES NOTHING — it only reads + reports (the same
//! discipline as posture.rs). This is the v1 core of the Egress Sentinel; the
//! longitudinal baseline + new-beacon alerting + a (propose-only) firewall
//! suggestion are follow-ons. Any future "block" stays a user-executed proposal —
//! this module never mutates the firewall and has no consequential surface.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::Command;
use tracing::warn;

/// `lsof` ships at this fixed absolute path on macOS; we invoke it by full path
/// with args only (no shell, no PATH search).
const LSOF: &str = "/usr/sbin/lsof";
const EGRESS_TIMEOUT: Duration = Duration::from_secs(6);
/// Cap the rendered list so a busy host can't flood the model/HUD context.
const MAX_CONNS: usize = 100;

/// One established outbound connection parsed from an lsof row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Conn {
    command: String,
    pid: String,
    remote: String,
    state: String,
}

/// Read the host's current established outbound TCP connections and render them
/// as a compact table. READ-ONLY: lsof changes nothing. Returns a friendly error
/// if lsof is unavailable or times out (never a panic, never a fabrication).
pub async fn snapshot() -> Result<String> {
    let output = run(LSOF, &["-i", "-nP", "-sTCP:ESTABLISHED"], EGRESS_TIMEOUT).await?;
    let mut conns = parse_lsof(&output);
    dedupe(&mut conns);
    Ok(format_egress(&conns))
}

/// Run a fixed read-only command (program + args, NEVER a shell string), bounded
/// by `timeout` + kill_on_drop, and return its stdout as text. Mirrors the
/// posture.rs / actions.rs command discipline.
async fn run(program: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(Err(e)) => Err(anyhow!("egress: failed to run {program}: {e}")),
        Err(_) => {
            warn!(program, secs = timeout.as_secs(), "egress: command timed out");
            Err(anyhow!("egress: {program} timed out after {}s", timeout.as_secs()))
        }
    }
}

/// PURE parser for `lsof -i -nP -sTCP:ESTABLISHED` output. Keeps only rows with a
/// `local->remote` NAME (outbound connections); pulls COMMAND, PID, the REMOTE
/// endpoint (after `->`), and the `(STATE)`. Malformed rows are skipped. Unit
/// tested against a fixed sample — no I/O.
fn parse_lsof(output: &str) -> Vec<Conn> {
    let mut conns = Vec::new();
    for line in output.lines() {
        if line.starts_with("COMMAND") || line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }
        let Some(conn_field) = fields.iter().find(|f| f.contains("->")) else {
            continue;
        };
        let remote = conn_field.split("->").nth(1).unwrap_or("").to_string();
        if remote.is_empty() {
            continue;
        }
        let state = fields
            .iter()
            .rev()
            .find(|f| f.starts_with('(') && f.ends_with(')'))
            .map(|s| s.trim_matches(|c| c == '(' || c == ')').to_string())
            .unwrap_or_default();
        conns.push(Conn {
            command: fields[0].to_string(),
            pid: fields[1].to_string(),
            remote,
            state,
        });
    }
    conns
}

/// Collapse duplicate (command, remote) pairs (a process holding several sockets
/// to the same host) in first-seen order, then cap the list.
fn dedupe(conns: &mut Vec<Conn>) {
    let mut seen = HashSet::new();
    conns.retain(|c| seen.insert((c.command.clone(), c.remote.clone())));
    conns.truncate(MAX_CONNS);
}

/// Render connections as a compact pipe-delimited table with a count footer.
fn format_egress(conns: &[Conn]) -> String {
    if conns.is_empty() {
        return "No established outbound connections.".to_string();
    }
    let mut out = String::from("process | pid | remote | state\n");
    for c in conns {
        out.push_str(&format!("{} | {} | {} | {}\n", c.command, c.pid, c.remote, c.state));
    }
    out.push_str(&format!(
        "({} connection{})",
        conns.len(),
        if conns.len() == 1 { "" } else { "s" }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "COMMAND     PID  USER   FD   TYPE DEVICE SIZE/OFF NODE NAME
firefox    1234  user   45u  IPv4  0x1      0t0  TCP 192.168.1.5:54321->93.184.216.34:443 (ESTABLISHED)
firefox    1234  user   46u  IPv4  0x2      0t0  TCP 192.168.1.5:54322->93.184.216.34:443 (ESTABLISHED)
ssh        5678  user    3u  IPv4  0x3      0t0  TCP 192.168.1.5:60000->203.0.113.7:22 (ESTABLISHED)
launchd       1  root    7u  IPv4  0x4      0t0  TCP *:8080 (LISTEN)";

    #[test]
    fn parse_lsof_extracts_outbound_connections() {
        let conns = parse_lsof(SAMPLE);
        // 3 outbound rows; the LISTEN row (no '->') is skipped.
        assert_eq!(conns.len(), 3);
        assert_eq!(conns[0].command, "firefox");
        assert_eq!(conns[0].pid, "1234");
        assert_eq!(conns[0].remote, "93.184.216.34:443");
        assert_eq!(conns[0].state, "ESTABLISHED");
        assert_eq!(conns[2].remote, "203.0.113.7:22");
        assert!(conns.iter().all(|c| c.command != "launchd"), "LISTEN socket excluded");
    }

    #[test]
    fn dedupe_collapses_same_process_and_remote() {
        let mut conns = parse_lsof(SAMPLE);
        dedupe(&mut conns);
        // firefox's two sockets to the same remote collapse to one.
        assert_eq!(conns.len(), 2);
        assert_eq!(conns[0].command, "firefox");
        assert_eq!(conns[1].command, "ssh");
    }

    #[test]
    fn format_handles_empty_and_nonempty() {
        assert_eq!(format_egress(&[]), "No established outbound connections.");
        let mut conns = parse_lsof(SAMPLE);
        dedupe(&mut conns);
        let out = format_egress(&conns);
        assert!(out.contains("firefox | 1234 | 93.184.216.34:443 | ESTABLISHED"), "got: {out}");
        assert!(out.contains("(2 connections)"), "got: {out}");
    }

    #[test]
    fn parse_skips_junk_lines() {
        assert!(parse_lsof("").is_empty());
        assert!(parse_lsof("garbage\nno arrow here\n").is_empty());
    }
}
