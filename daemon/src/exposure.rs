//! Inbound Exposure Auditor — a defensive "nmap-of-self" for agent "aegis"
//! (Defense & Privacy).
//!
//! DEFENSIVE-ONLY, READ-ONLY, the LOCAL machine. This module answers "what on
//! THIS Mac is reachable from the network?" by reading the machine's OWN socket
//! table — it enumerates the local listening sockets, classifies each as
//! loopback-only (`127.0.0.0/8` / `::1`) vs LAN/all-interfaces EXPOSED, and maps
//! each exposed well-known port to the macOS sharing service that usually opens
//! it (Remote Login / Screen Sharing / File Sharing / …) via a static table.
//!
//! IT SENDS NO PACKETS and NEVER touches another host. The single read is the
//! local socket table via a FIXED-ARG bounded subprocess — `netstat -anv`, which
//! on macOS lists listening sockets WITH their owning pid without root — spawned
//! with the SAME explicit-args discipline `posture.rs` / `persistence.rs` use (an
//! absolute program path + fixed args, NEVER a shell string; 5s timeout;
//! kill_on_drop). It CHANGES nothing: it reports where the user is exposed;
//! turning a sharing service off (or enabling the firewall) is the user's own
//! action, offered through the gated `open_settings_pane` actuator that only
//! deep-links to the relevant System Settings pane.
//!
//! The command RUNNER is INJECTED (a function value), so the PURE PARSER (netstat
//! row -> listener, host -> exposure, port -> service) is CI-tested on canned
//! `netstat -anv` output and the real subprocess is NEVER spawned under test.
//!
//! HONEST ABOUT THE READ'S LIMITS: a service is identified by its WELL-KNOWN
//! PORT, not by correlating the actual owning process — a non-standard program on
//! a well-known port would be mislabeled, and an exposed port with no table entry
//! is reported honestly as an unrecognized exposed port (never hidden, never
//! guessed). When the read itself can't run, this says so and fabricates no
//! inventory (same discipline as posture.rs / tcc.rs).

use std::future::Future;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;
use tracing::warn;

/// The single read-only enumeration command: the machine's OWN socket table.
/// `-a` all sockets, `-n` numeric (no DNS/service-name lookups), `-v` verbose
/// (adds the owning `process:pid` column, available without root on macOS).
const NETSTAT: &str = "/usr/sbin/netstat";
const NETSTAT_ARGS: &[&str] = &["-anv"];

/// Hard ceiling on the read — the same 5s discipline as posture.rs.
const NETSTAT_TIMEOUT: Duration = Duration::from_secs(5);

/// Generous startup delay (keep housekeeping out of the first exchanges) + a slow
/// tick (the listening surface moves on the order of app launches, not seconds).
const DEFAULT_STARTUP_DELAY_SECS: u64 = 40;
const DEFAULT_INTERVAL_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// The captured outcome of the read: either combined stdout+stderr text, or a
/// note that the read itself could not run (missing binary, timed out). Honest
/// degradation — an unreadable table never fabricates an "all clear".
enum ReadOutput {
    Text(String),
    Unavailable(String),
}

/// Whether a listening socket is reachable only from this machine, or from the
/// LAN / any interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exposure {
    /// Bound to loopback (`127.0.0.0/8` or `::1`) — not reachable off-box.
    Loopback,
    /// Bound to a wildcard (`*` / `0.0.0.0` / `::`) or a specific routable
    /// interface — reachable from the network.
    Exposed,
}

/// One listening socket discovered in the local table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Listener {
    /// Protocol token as netstat prints it (`tcp4` / `tcp6` / `tcp46` / `udp4` …).
    proto: String,
    /// The bound local port.
    port: u16,
    /// Loopback-only vs network-exposed.
    exposure: Exposure,
    /// Owning process id when netstat reported one (best-effort; `-v` supplies it
    /// without root, but the column layout varies, so a miss degrades to `None`).
    pid: Option<u32>,
}

/// A macOS sharing service that conventionally opens a well-known port. `pane` is
/// the [`crate::actions`] settings-pane id the guided-remediation actuator can
/// deep-link to so the user can flip the switch.
struct SharingService {
    port: u16,
    name: &'static str,
    pane: &'static str,
}

/// The static well-known-port -> macOS sharing service table. Deliberately small
/// and curated to the genuine macOS sharing surfaces (System Settings › General ›
/// Sharing, plus AirPlay): identifying by port is a heuristic, so a broad guess
/// table would mislabel ordinary dev servers. Every `pane` is an id in
/// `actions::SETTINGS_PANES` (mostly "sharing").
const SHARING_SERVICES: &[SharingService] = &[
    SharingService { port: 22, name: "Remote Login (SSH)", pane: "sharing" },
    SharingService { port: 5900, name: "Screen Sharing (VNC)", pane: "sharing" },
    SharingService { port: 3283, name: "Remote Management (ARD)", pane: "sharing" },
    SharingService { port: 445, name: "File Sharing (SMB)", pane: "sharing" },
    SharingService { port: 139, name: "File Sharing (SMB/NetBIOS)", pane: "sharing" },
    SharingService { port: 548, name: "File Sharing (AFP)", pane: "sharing" },
    SharingService { port: 2049, name: "File Sharing (NFS)", pane: "sharing" },
    SharingService { port: 631, name: "Printer Sharing (IPP/CUPS)", pane: "sharing" },
    SharingService { port: 3689, name: "Media Sharing (DAAP)", pane: "sharing" },
    SharingService { port: 7000, name: "AirPlay Receiver", pane: "sharing" },
];

/// PURE: the sharing service conventionally on `port`, or `None` for a port with
/// no table entry (an honest "unrecognized exposed port").
fn service_for_port(port: u16) -> Option<&'static SharingService> {
    SHARING_SERVICES.iter().find(|s| s.port == port)
}

// ---------------------------------------------------------------------------
// Pure parser — one netstat row -> listener; unit-tested on canned output.
// ---------------------------------------------------------------------------

/// PURE: parse `netstat -anv` output into the LISTENING sockets only.
///
/// A TCP row counts when it carries the `LISTEN` state; a UDP row counts when its
/// foreign address is the unconnected wildcard `*.*` (a bound, not a flowing,
/// socket). Header lines, Unix-domain sockets, and established/outbound flows are
/// all skipped. Only the first five columns are positional (Proto, Recv-Q,
/// Send-Q, Local, Foreign) — the owning pid rides in a later `process:pid` column
/// whose process name may itself contain spaces, so the pid is found by CONTENT
/// (a `…:<digits>` token), never a fixed offset.
fn parse_listeners(text: &str) -> Vec<Listener> {
    let mut out = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        // Proto + the four required leading columns (Recv-Q, Send-Q, Local, Foreign).
        if f.len() < 5 {
            continue;
        }
        let proto = f[0];
        let is_tcp = proto.starts_with("tcp");
        let is_udp = proto.starts_with("udp");
        if !is_tcp && !is_udp {
            continue; // header row, Unix-domain socket, or blank
        }
        let local = f[3];
        let foreign = f[4];

        // Keep only LISTENING sockets.
        let listening = if is_tcp {
            f.iter().any(|t| t.eq_ignore_ascii_case("LISTEN"))
        } else {
            // A bound (not connected) UDP socket has the wildcard foreign address.
            foreign == "*.*"
        };
        if !listening {
            continue;
        }

        let Some((host, port)) = split_host_port(local) else {
            continue;
        };
        out.push(Listener {
            proto: proto.to_string(),
            port,
            exposure: classify_exposure(host),
            pid: extract_pid(&f),
        });
    }
    out
}

/// PURE: split a netstat `host.port` local-address token into (host, port).
/// macOS netstat separates the port with a `.` (IPv4 `127.0.0.1.631`, wildcard
/// `*.22`, IPv6 `::1.631`), and IPv6 hosts use `:` internally, so the port is
/// always the numeric tail after the LAST `.`.
fn split_host_port(addr: &str) -> Option<(&str, u16)> {
    let (host, port) = addr.rsplit_once('.')?;
    let port: u16 = port.parse().ok()?;
    Some((host, port))
}

/// PURE: is a bound host loopback-only, or reachable from the network? Loopback =
/// the `127.0.0.0/8` block or IPv6 `::1` (incl. the v4-mapped form). Everything
/// else — the `*` / `0.0.0.0` / `::` wildcards and any specific routable
/// interface address — is network-EXPOSED.
fn classify_exposure(host: &str) -> Exposure {
    let h = host.trim();
    if h == "127.0.0.1"
        || h.starts_with("127.")
        || h == "::1"
        || h.eq_ignore_ascii_case("::ffff:127.0.0.1")
    {
        Exposure::Loopback
    } else {
        Exposure::Exposed
    }
}

/// PURE: recover the owning pid from a netstat `-v` row. The pid rides in the
/// `process:pid` column — but the process name can contain spaces (`Comet
/// Helper:1927`), so we never key off a fixed index. We scan the columns AFTER
/// the positional Local/Foreign addresses (indices 0-4) for the first token whose
/// text after its last `:` is all digits. The Local/Foreign IPv6 addresses also
/// contain `:` but end in `.port`, never `:<digits>`, so skipping them avoids a
/// false match; the literal `process:pid` header token ends in `pid`, not digits.
fn extract_pid(fields: &[&str]) -> Option<u32> {
    fields.iter().skip(5).find_map(|t| {
        let (_, tail) = t.rsplit_once(':')?;
        if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
            tail.parse::<u32>().ok()
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// Pure summary + telemetry frame (secret-free)
// ---------------------------------------------------------------------------

/// The headline counts folded into telemetry + the posture readout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Summary {
    /// All listening sockets found.
    total: usize,
    /// How many are loopback-only.
    loopback: usize,
    /// How many are network-exposed.
    exposed: usize,
    /// The exposed sockets whose port maps to a known sharing service, deduped by
    /// port and sorted — the short "here's what's reachable" list for the human
    /// posture line.
    exposed_services: Vec<(u16, &'static str)>,
}

/// PURE: fold the listeners into the secret-free headline summary.
fn summarize(listeners: &[Listener]) -> Summary {
    let total = listeners.len();
    let loopback = listeners.iter().filter(|l| l.exposure == Exposure::Loopback).count();
    let exposed = listeners.iter().filter(|l| l.exposure == Exposure::Exposed).count();

    let mut exposed_services: Vec<(u16, &'static str)> = Vec::new();
    for l in listeners.iter().filter(|l| l.exposure == Exposure::Exposed) {
        if let Some(svc) = service_for_port(l.port) {
            if !exposed_services.iter().any(|(p, _)| *p == svc.port) {
                exposed_services.push((svc.port, svc.name));
            }
        }
    }
    exposed_services.sort_by_key(|(p, _)| *p);

    Summary { total, loopback, exposed, exposed_services }
}

/// PURE: build the secret-free `security.exposure` telemetry payload. Carries
/// only protocol/port/service/pid facts and the counts — NEVER a byte of raw
/// command output and no full bind address (the machine's own pid is not
/// sensitive; a specific interface IP is not emitted).
fn build_frame(listeners: &[Listener]) -> serde_json::Value {
    let summary = summarize(listeners);
    let exposed_detail: Vec<serde_json::Value> = listeners
        .iter()
        .filter(|l| l.exposure == Exposure::Exposed)
        .map(|l| {
            let svc = service_for_port(l.port);
            json!({
                "proto": l.proto,
                "port": l.port,
                "service": svc.map(|s| s.name),
                // The allowlisted settings-pane id the guided-remediation actuator
                // (`open_settings_pane`) would deep-link to for THIS service, so the
                // HUD can offer a gated "open Settings" jump. Null for an
                // unrecognized port (no known remediation pane).
                "pane": svc.map(|s| s.pane),
                "pid": l.pid,
            })
        })
        .collect();
    json!({
        "available": true,
        "listeners": summary.total,
        "loopback": summary.loopback,
        "exposed": summary.exposed,
        "exposed_detail": exposed_detail,
    })
}

// ---------------------------------------------------------------------------
// Posture fold — a cached one-line summary for posture.rs's readout.
// ---------------------------------------------------------------------------

/// The last summary the sentinel computed, for the posture readout.
static LAST_SUMMARY: StdMutex<Option<Summary>> = StdMutex::new(None);

fn set_last_summary(summary: Summary) {
    if let Ok(mut g) = LAST_SUMMARY.lock() {
        *g = Some(summary);
    }
}

/// A one-line inbound-exposure summary for `posture.rs`'s read-only report, or
/// `None` if the auditor has not scanned yet (so posture shows nothing stale).
/// SECRET-FREE — counts + the known exposed service names only. Honest about the
/// read's limit: it names Sharing / the firewall as the user's own remediation.
pub fn posture_line() -> Option<String> {
    let s = (*LAST_SUMMARY.lock().ok()?).clone()?;
    if s.exposed == 0 {
        return Some(format!(
            "Inbound exposure: {} listening socket(s), all loopback-only — nothing reachable from \
             the network. Read-only.",
            s.total
        ));
    }
    let named = if s.exposed_services.is_empty() {
        String::new()
    } else {
        let list: Vec<String> =
            s.exposed_services.iter().map(|(p, n)| format!("{n}:{p}")).collect();
        format!(" ({})", list.join(", "))
    };
    Some(format!(
        "Inbound exposure: {} listening socket(s) — {} loopback-only, {} exposed to the \
         network{named} — read-only; close a service in System Settings › Sharing or turn on \
         the firewall.",
        s.total, s.loopback, s.exposed
    ))
}

// ---------------------------------------------------------------------------
// Real command runner (NEVER reached in tests — they inject canned output)
// ---------------------------------------------------------------------------

/// The read-only enumeration command (program + fixed args) plus its timeout,
/// factored out so the exact invocation is assertable in tests WITHOUT running
/// it — pinning that the auditor never gains a scan-another-host argv.
fn netstat_command() -> (&'static str, &'static [&'static str]) {
    (NETSTAT, NETSTAT_ARGS)
}

/// Spawn the read-only `netstat` with explicit args (never a shell string),
/// capture its combined stdout+stderr, and bound it with the timeout +
/// kill_on_drop — mirroring posture.rs::run_real_command. A spawn error,
/// non-UTF8 output, or timeout becomes a `ReadOutput::Unavailable` so the tick
/// degrades honestly rather than fabricating an empty table.
async fn run_real_command(
    program: &'static str,
    args: &'static [&'static str],
    timeout: Duration,
) -> ReadOutput {
    let mut cmd = Command::new(program);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
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
            warn!(program, error = %e, "exposure: netstat could not run");
            ReadOutput::Unavailable("not available on this machine".to_string())
        }
        Err(_) => {
            warn!(program, secs = timeout.as_secs(), "exposure: netstat timed out");
            ReadOutput::Unavailable("the read timed out".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Sentinel tick + loop (the live read is runtime-only; the parser is tested).
// ---------------------------------------------------------------------------

/// Run the read through `run` (injected so tests feed canned output) and fold it
/// into the listener list. `None` when the read itself could not run — the caller
/// degrades honestly. Pure of any real subprocess (the subprocess lives behind
/// `run`).
async fn scan<F, Fut>(run: F) -> Result<Vec<Listener>, String>
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    let (program, args) = netstat_command();
    match run(program, args, NETSTAT_TIMEOUT).await {
        ReadOutput::Text(t) => Ok(parse_listeners(&t)),
        ReadOutput::Unavailable(why) => Err(why),
    }
}

/// One auditor tick: read the local socket table, cache the posture summary, and
/// emit the secret-free `security.exposure` frame. READ-ONLY over the OS (it only
/// reads the local socket table and sends no packets); nothing is written.
/// Runtime-only (the live netstat read makes this inspection-verified; its parser
/// + summary cores are unit-tested).
async fn sentinel_tick() {
    match scan(run_real_command).await {
        Ok(listeners) => {
            set_last_summary(summarize(&listeners));
            crate::telemetry::emit("system", "security.exposure", build_frame(&listeners));
        }
        Err(why) => {
            // The read couldn't run — report honestly (with the generic reason),
            // don't fabricate a table and don't clobber the last good summary.
            crate::telemetry::emit(
                "system",
                "security.exposure",
                json!({"available": false, "reason": why}),
            );
        }
    }
}

/// The ambient Inbound Exposure Auditor loop (runtime-only; never run in tests).
/// Mirrors `persistence::sentinel_task`: a startup delay, then a slow periodic
/// `sentinel_tick`. READ-ONLY throughout — it enumerates the local socket table
/// and sends no packets.
pub async fn sentinel_task(startup_delay_secs: u64, interval_secs: u64) {
    let startup = if startup_delay_secs == 0 { DEFAULT_STARTUP_DELAY_SECS } else { startup_delay_secs };
    let interval = if interval_secs == 0 { DEFAULT_INTERVAL_SECS } else { interval_secs };
    tokio::time::sleep(Duration::from_secs(startup)).await;
    loop {
        sentinel_tick().await;
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: the parser + summary are tested on hand-written canned
// `netstat -anv` output; the scan fold is driven by an INJECTED runner. The real
// netstat is NEVER spawned here, and no packet is ever sent.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A faithful slice of real macOS `netstat -anv` output: the header, a couple
    /// of loopback listeners, a couple of network-exposed ones (a well-known SSH
    /// port + unrecognized ports), a UDP bound listener, and — importantly — a
    /// process name with a SPACE in the `process:pid` column, plus a CONNECTED
    /// (non-listening) UDP flow that must be ignored.
    const CANNED: &str = "\
Active Internet connections (including servers)
Proto Recv-Q Send-Q  Local Address          Foreign Address        (state)  rxbytes txbytes rhiwat shiwat          process:pid state options
tcp4       0      0  127.0.0.1.56177        *.*                    LISTEN   0       0       131072 131072      app_inkwell:53558 00100 00000006
tcp6       0      0  ::1.631                *.*                    LISTEN   0       0       131072 131072            cupsd:412 00100 00000006
tcp4       0      0  *.22                   *.*                    LISTEN   0       0       131072 131072          launchd:1 00100 00000006
tcp46      0      0  *.7348                 *.*                    LISTEN   0       0       327680 327680 backburnerManage:96562 00180 00000006
udp4       0      0  *.137                  *.*                             0       0       786432 786432         netbiosd:321 00000000
tcp4       0      0  192.168.1.20.51000     93.184.216.34.443      ESTABLISHED 0    0       131072 131072          Safari:600 00102 00000000
udp6       0      0  2600:4041:4133:0.58881 2001:4860:4847:4.443            13382   6623    1048576 29040    Comet Helper:1927 00102 00000000";

    #[test]
    fn parser_keeps_only_listeners_and_reads_host_port_pid() {
        let listeners = parse_listeners(CANNED);
        // Five listeners: two loopback (56177, 631), three exposed (22, 7348, 137).
        // The ESTABLISHED tcp flow and the connected udp6 flow are NOT listeners.
        assert_eq!(listeners.len(), 5, "got: {listeners:?}");

        let by_port = |p: u16| listeners.iter().find(|l| l.port == p).unwrap();

        // Loopback classification.
        assert_eq!(by_port(56177).exposure, Exposure::Loopback);
        assert_eq!(by_port(631).exposure, Exposure::Loopback);
        // Exposed classification (wildcard bind).
        assert_eq!(by_port(22).exposure, Exposure::Exposed);
        assert_eq!(by_port(7348).exposure, Exposure::Exposed);
        assert_eq!(by_port(137).exposure, Exposure::Exposed);

        // Pid recovered from the `process:pid` column, even with a spaced name.
        assert_eq!(by_port(22).pid, Some(1));
        assert_eq!(by_port(56177).pid, Some(53558));
        assert_eq!(by_port(7348).pid, Some(96562), "spaced-name pid read by content");

        // The connected flows never appear.
        assert!(listeners.iter().all(|l| l.port != 51000), "ESTABLISHED tcp is not a listener");
        assert!(listeners.iter().all(|l| l.port != 58881), "connected udp is not a listener");
    }

    #[test]
    fn classify_exposure_splits_loopback_from_network() {
        assert_eq!(classify_exposure("127.0.0.1"), Exposure::Loopback);
        assert_eq!(classify_exposure("127.94.0.2"), Exposure::Loopback, "whole 127/8 is loopback");
        assert_eq!(classify_exposure("::1"), Exposure::Loopback);
        // Wildcards and specific routable interfaces are exposed.
        assert_eq!(classify_exposure("*"), Exposure::Exposed);
        assert_eq!(classify_exposure("0.0.0.0"), Exposure::Exposed);
        assert_eq!(classify_exposure("::"), Exposure::Exposed);
        assert_eq!(classify_exposure("192.168.1.20"), Exposure::Exposed);
        assert_eq!(classify_exposure("fe80::1%en0"), Exposure::Exposed);
    }

    #[test]
    fn split_host_port_handles_v4_v6_and_wildcard() {
        assert_eq!(split_host_port("127.0.0.1.631"), Some(("127.0.0.1", 631)));
        assert_eq!(split_host_port("*.22"), Some(("*", 22)));
        assert_eq!(split_host_port("::1.5900"), Some(("::1", 5900)));
        // A non-numeric or missing port is not a listener local address.
        assert_eq!(split_host_port("*.*"), None);
        assert_eq!(split_host_port("nodot"), None);
        // Port out of range fails to parse into u16.
        assert_eq!(split_host_port("*.99999"), None);
    }

    #[test]
    fn service_map_names_well_known_ports_and_admits_unknowns() {
        assert_eq!(service_for_port(22).unwrap().name, "Remote Login (SSH)");
        assert_eq!(service_for_port(5900).unwrap().name, "Screen Sharing (VNC)");
        assert_eq!(service_for_port(445).unwrap().name, "File Sharing (SMB)");
        assert_eq!(service_for_port(139).unwrap().name, "File Sharing (SMB/NetBIOS)");
        assert_eq!(service_for_port(7000).unwrap().name, "AirPlay Receiver");
        // Every mapped pane id is one the actuator's allowlist actually accepts —
        // guards against a remediation link that would be refused.
        for svc in SHARING_SERVICES {
            assert!(
                crate::actions::is_known_settings_pane(svc.pane),
                "service {} maps to unknown pane id {}",
                svc.name,
                svc.pane
            );
        }
        // An arbitrary/dev port has no sharing service — reported honestly, not guessed.
        assert!(service_for_port(7348).is_none());
        assert!(service_for_port(8080).is_none());
    }

    #[test]
    fn summarize_counts_and_lists_only_exposed_known_services() {
        let listeners = parse_listeners(CANNED);
        let s = summarize(&listeners);
        assert_eq!(s.total, 5);
        assert_eq!(s.loopback, 2);
        assert_eq!(s.exposed, 3);
        // Only the EXPOSED well-known service (SSH:22) is listed — the loopback
        // Printer Sharing on 631 is NOT (it isn't reachable), and the exposed
        // unrecognized ports (7348, 137) are not named.
        assert_eq!(s.exposed_services, vec![(22, "Remote Login (SSH)")]);
    }

    #[test]
    fn frame_is_secret_free_and_carries_the_exposed_detail() {
        let listeners = parse_listeners(CANNED);
        let frame = build_frame(&listeners);
        assert_eq!(frame["available"], true);
        assert_eq!(frame["listeners"], 5);
        assert_eq!(frame["loopback"], 2);
        assert_eq!(frame["exposed"], 3);
        let detail = frame["exposed_detail"].as_array().unwrap();
        assert_eq!(detail.len(), 3, "one entry per exposed socket");
        // The SSH entry names its service + pid; an unrecognized port has a null service.
        let ssh = detail.iter().find(|d| d["port"] == 22).unwrap();
        assert_eq!(ssh["service"], "Remote Login (SSH)");
        assert_eq!(ssh["pane"], "sharing", "known service carries its remediation pane id");
        assert_eq!(ssh["pid"], 1);
        let unknown = detail.iter().find(|d| d["port"] == 7348).unwrap();
        assert!(unknown["service"].is_null(), "unrecognized port keeps a null service");
        assert!(unknown["pane"].is_null(), "unrecognized port has no remediation pane");
        // No raw command text, no bind IP leaked into the frame.
        let text = frame.to_string();
        assert!(!text.contains("Foreign"), "no raw netstat header/output in the frame");
        assert!(!text.contains("192.168"), "no bind interface address in the frame");
    }

    // -- the read command is exactly the read-only local enumeration ----------

    /// The read is the local socket-table enumeration only — an absolute program +
    /// fixed args. Asserted WITHOUT running it. Pins that the auditor never gains a
    /// scan-another-host / send-packets argv (no host operand, no `-r`oute change).
    #[test]
    fn netstat_command_is_the_read_only_local_enumeration() {
        let (program, args) = netstat_command();
        assert_eq!(program, "/usr/sbin/netstat");
        assert_eq!(args, &["-anv"]);
        // No target host operand and no non-read flag ever rides the argv.
        for a in args {
            assert!(!a.contains('.'), "no host operand: {a}");
            assert!(!a.contains(':'), "no host:port operand: {a}");
        }
    }

    // -- the scan fold, driven by an INJECTED canned runner -------------------

    #[tokio::test]
    async fn scan_folds_canned_output_without_spawning_netstat() {
        let run = |_p: &'static str, _a: &'static [&'static str], _t: Duration| {
            std::future::ready(ReadOutput::Text(CANNED.to_string()))
        };
        let listeners = scan(run).await.expect("a readable table");
        assert_eq!(listeners.len(), 5);
        assert_eq!(summarize(&listeners).exposed, 3);
    }

    /// An unreadable table degrades to `Err(reason)` (the tick then reports
    /// available:false with that reason) — it never fabricates an "all clear"
    /// empty inventory.
    #[tokio::test]
    async fn scan_degrades_honestly_when_the_read_is_unavailable() {
        let run = |_p: &'static str, _a: &'static [&'static str], _t: Duration| {
            std::future::ready(ReadOutput::Unavailable("not available".to_string()))
        };
        let err = scan(run).await.expect_err("an unreadable table is an error, never an empty list");
        assert_eq!(err, "not available", "the honest reason is preserved");
    }

    // -- posture line ---------------------------------------------------------

    #[test]
    fn posture_line_reports_exposed_and_all_clear() {
        // Exposed present: names the count + the known service, points at Sharing.
        set_last_summary(Summary {
            total: 5,
            loopback: 2,
            exposed: 3,
            exposed_services: vec![(22, "Remote Login (SSH)")],
        });
        let line = posture_line().expect("a cached summary");
        assert!(line.contains("3 exposed"), "{line}");
        assert!(line.contains("Remote Login (SSH):22"), "{line}");
        assert!(line.to_lowercase().contains("read-only"), "{line}");
        assert!(line.contains("Sharing") || line.contains("firewall"), "names remediation: {line}");

        // All loopback: an honest "nothing reachable" line.
        set_last_summary(Summary { total: 4, loopback: 4, exposed: 0, exposed_services: vec![] });
        let line = posture_line().expect("a cached summary");
        assert!(line.contains("all loopback-only"), "{line}");
        assert!(line.to_lowercase().contains("nothing reachable"), "{line}");
    }
}
