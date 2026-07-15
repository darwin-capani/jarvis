//! Traffic-Interception Integrity Check — "is anything MITMing me?" for agent
//! "aegis" (Defense & Privacy).
//!
//! DEFENSIVE-ONLY, READ-ONLY, the LOCAL machine. This module answers one blunt
//! question — is something sitting between you and the network, quietly reading or
//! rerouting your traffic? — by reading THIS Mac's OWN local configuration. It
//! sends NO packets, contacts NO other host, and touches NOTHING outside the
//! machine. It reads five local surfaces:
//!
//!   * `scutil --proxy` — a configured system / PAC HTTP(S)/SOCKS proxy (the
//!     classic "route all my web traffic through here" MITM);
//!   * `/etc/hosts` — non-default entries that hijack a domain to a routable IP
//!     (a redirect) or pin one to loopback (a block); the default localhost /
//!     broadcasthost lines are expected and never flagged;
//!   * non-Apple trusted ROOT CAs via `security dump-trust-settings -d` (the
//!     ADMIN/system trust-settings domain) + the certs sitting in the System
//!     keychain (`security find-certificate -a`) — a rogue trusted root CA is THE
//!     artifact that silently breaks ALL your TLS, so it is surfaced loudly;
//!   * `scutil --dns` — the configured resolvers (a LOOPBACK resolver means
//!     something on-box is handling every lookup, the local-interception signal);
//!   * `profiles show` — installed configuration / MDM profiles (a profile can
//!     add a root CA and a proxy silently), user-domain readable.
//!
//! SAME DISCIPLINE as posture.rs / tcc.rs / persistence.rs / exposure.rs and it
//! CHANGES NOTHING:
//!   * every subprocess read is a FIXED-ARG bounded command (an absolute program
//!     path + fixed args, NEVER a shell string), 5s timeout, kill_on_drop;
//!   * the command RUNNER is INJECTED (a function value), so the PURE PARSERS —
//!     one per surface — are unit-tested on hand-written canned output and the
//!     real system commands are NEVER spawned under test; `/etc/hosts` is read via
//!     `std::fs` behind an injectable path so its parser + skip path are tested too;
//!   * HONESTY: a surface that needs a privilege the no-sudo daemon lacks (the
//!     admin trust-settings domain, system-domain profiles) degrades to an explicit
//!     SKIP — never a fabricated "clean";
//!   * STRICTLY READ-ONLY: no writes, no config changes, no remediation, not even
//!     a gated one. Removing a rogue root or a proxy is the user's own action in
//!     Keychain Access / System Settings, and this only names it.
//!
//! It emits a secret-free-structured `security.interception` telemetry frame (each
//! finding also rendered in PLAIN SPEECH) and folds a one-line summary into
//! posture.rs's "am I secure?" readout.

use std::future::Future;
use std::path::Path;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;
use tracing::warn;

// ---------------------------------------------------------------------------
// Fixed read-only command set. Each is an absolute program path + fixed args —
// never a shell string, exactly like posture.rs / exposure.rs.
// ---------------------------------------------------------------------------

/// System/PAC proxy configuration + configured DNS resolvers.
const SCUTIL: &str = "/usr/sbin/scutil";
/// Trust-settings dump + System-keychain certificate listing (READ-ONLY).
const SECURITY: &str = "/usr/bin/security";
/// Installed configuration / MDM profiles (user-domain readable).
const PROFILES: &str = "/usr/bin/profiles";
/// The static hosts map (read via `std::fs`, no subprocess).
const HOSTS_PATH: &str = "/etc/hosts";
/// The admin System keychain (where an admin-installed CA cert would live).
const SYSTEM_KEYCHAIN: &str = "/Library/Keychains/System.keychain";

const PROXY_ARGS: &[&str] = &["--proxy"];
const DNS_ARGS: &[&str] = &["--dns"];
/// `-d` selects the ADMIN (system) trust-settings domain — the machine-wide roots.
const TRUST_ARGS: &[&str] = &["dump-trust-settings", "-d"];
const CERTS_ARGS: &[&str] = &["find-certificate", "-a", SYSTEM_KEYCHAIN];
const PROFILES_ARGS: &[&str] = &["show"];

/// Hard ceiling per spawned read — the same 5s discipline as posture.rs.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Generous startup delay (keep housekeeping out of the first exchanges, and
/// stagger a little later than the sibling sentinels) + a slow tick (local
/// interception config moves on the order of installs, not seconds).
const DEFAULT_STARTUP_DELAY_SECS: u64 = 50;
const DEFAULT_INTERVAL_SECS: u64 = 300;

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

/// The captured outcome of one subprocess read: either combined stdout+stderr
/// text, or a note that the read itself could not run (missing binary, timed
/// out). Honest degradation — an unreadable surface never fabricates "clean".
enum ReadOutput {
    Text(String),
    Unavailable(String),
}

/// A surface that was read (`Ok`, possibly empty = an HONEST clean) or honestly
/// SKIPPED (`Err(reason)` — it needed a privilege we lack / couldn't be parsed).
/// The reason is never coerced into a fabricated empty inventory.
type Surface<T> = Result<T, String>;

/// A configured proxy that would sit between you and the network.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Proxy {
    /// "HTTP" / "HTTPS" / "SOCKS" / "PAC".
    kind: &'static str,
    /// `host:port`, or the PAC auto-config URL.
    target: String,
}

/// Whether a non-default `/etc/hosts` entry redirects a name to a routable host
/// (the loud MITM/redirection artifact) or pins it to loopback/null (a block —
/// usually an ad-blocker or dev override, reported but not alarmed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostsKind {
    Redirect,
    Block,
}

impl HostsKind {
    fn wire(self) -> &'static str {
        match self {
            HostsKind::Redirect => "redirect",
            HostsKind::Block => "block",
        }
    }
}

/// One non-default hosts entry (a default localhost/broadcasthost line never
/// produces one).
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostsEntry {
    ip: String,
    host: String,
    kind: HostsKind,
}

/// A cert with an ADMIN-domain trust override (`dump-trust-settings -d`). Every
/// entry here is an admin-installed trust setting; a NON-Apple one is the rogue
/// root artifact we surface loudly.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrustedRoot {
    name: String,
    apple: bool,
}

/// A certificate sitting in the admin System keychain. A non-Apple one is worth
/// confirming (context, not itself proof of a trusted-root MITM).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SysCert {
    label: String,
    apple: bool,
}

/// One configured DNS resolver's nameserver. `loopback` == an on-box resolver
/// (the local-interception signal).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolver {
    server: String,
    loopback: bool,
}

/// An installed configuration / MDM profile (identified by its identifier).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Profile {
    id: String,
}

/// The full set of local interception surfaces, each independently readable or
/// honestly skipped.
struct Findings {
    proxies: Surface<Vec<Proxy>>,
    hosts: Surface<Vec<HostsEntry>>,
    trusted_roots: Surface<Vec<TrustedRoot>>,
    system_certs: Surface<Vec<SysCert>>,
    resolvers: Surface<Vec<Resolver>>,
    profiles: Surface<Vec<Profile>>,
}

// ---------------------------------------------------------------------------
// Pure parsers — one per surface, unit-tested on canned output. No I/O.
// ---------------------------------------------------------------------------

/// PURE: does a certificate name match a RECOGNIZED Apple identity? Deliberately
/// STRICT: a loose `contains("apple")` is trivially spoofable — an attacker names a
/// rogue root "Apple Internal Root CA" or "Pineapple Security CA" to be waved
/// through as benign — so we recognize ONLY the reverse-DNS `com.apple.` prefix or
/// an EXACT known Apple root CN. HONESTY: this is still a NAME heuristic, not
/// cryptographic issuer verification — it only suppresses the LOUDEST warning for
/// the common genuine case; anything unmatched is surfaced for the user to confirm,
/// so the failure direction is a benign false-positive, never a false clean.
fn name_is_apple(name: &str) -> bool {
    let l = name.trim().to_lowercase();
    l.starts_with("com.apple.")
        || matches!(
            l.as_str(),
            "apple root ca"
                | "apple root ca - g2"
                | "apple root ca - g3"
                | "apple root certificate authority"
        )
}

/// PURE: an IPv4/IPv6 loopback address (the `127.0.0.0/8` block, `::1`, or the
/// `fe80::1` link-local loopback alias — EXACT, optionally with a zone id like
/// `fe80::1%en0`). NOT a prefix match: `fe80::1abc` / `fe80::1234` are ordinary
/// link-local hosts, not loopback.
fn is_loopback_ip(ip: &str) -> bool {
    ip.starts_with("127.") || ip == "::1" || ip == "fe80::1" || ip.starts_with("fe80::1%")
}

/// PURE: loopback OR the null/unspecified addresses a "block" hosts entry uses.
fn is_loopback_or_null(ip: &str) -> bool {
    is_loopback_ip(ip) || ip == "0.0.0.0" || ip == "::"
}

/// PURE: parse `scutil --proxy` output into the ENABLED proxies. scutil prints a
/// `<dictionary>` of `  Key : Value` lines; the key is the token before the first
/// " : " (values like a PAC URL contain `:` but never a spaced " : "). We surface
/// a proxy only when its `*Enable` flag is `1`, so a machine with no proxy yields
/// an empty list (an honest clean, not a fabrication).
fn parse_proxy(text: &str) -> Vec<Proxy> {
    let mut map: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(" : ") {
            map.insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let enabled = |k: &str| map.get(k).is_some_and(|v| v == "1");
    let hostport = |host_k: &str, port_k: &str| {
        let host = map.get(host_k).cloned().unwrap_or_default();
        match map.get(port_k) {
            Some(p) if !p.is_empty() => format!("{host}:{p}"),
            _ => host,
        }
    };
    let mut out = Vec::new();
    if enabled("HTTPEnable") {
        out.push(Proxy { kind: "HTTP", target: hostport("HTTPProxy", "HTTPPort") });
    }
    if enabled("HTTPSEnable") {
        out.push(Proxy { kind: "HTTPS", target: hostport("HTTPSProxy", "HTTPSPort") });
    }
    if enabled("SOCKSEnable") {
        out.push(Proxy { kind: "SOCKS", target: hostport("SOCKSProxy", "SOCKSPort") });
    }
    if enabled("ProxyAutoConfigEnable") {
        let url = map
            .get("ProxyAutoConfigURLString")
            .cloned()
            .unwrap_or_else(|| "(unspecified URL)".to_string());
        out.push(Proxy { kind: "PAC", target: url });
    }
    out
}

/// PURE: parse `/etc/hosts` into the NON-DEFAULT entries only. Comments (`#…`) and
/// blank lines are dropped; the default `localhost` / `broadcasthost` loopback and
/// broadcast lines are expected and skipped. Every surviving entry is notable —
/// classified as a `Redirect` (a name pointed at a routable IP, the loud
/// redirection artifact) or a `Block` (a name pinned to loopback/null).
fn parse_hosts(text: &str) -> Vec<HostsEntry> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = match raw.split_once('#') {
            Some((before, _)) => before,
            None => raw,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let Some(ip) = fields.next() else {
            continue;
        };
        for host in fields {
            if is_default_hosts_entry(ip, host) {
                continue;
            }
            let kind = if is_loopback_or_null(ip) { HostsKind::Block } else { HostsKind::Redirect };
            out.push(HostsEntry { ip: ip.to_string(), host: host.to_string(), kind });
        }
    }
    out
}

/// PURE: an expected default hosts line — `localhost` on a loopback IP, or
/// `broadcasthost` on the broadcast address. Anything else is notable.
fn is_default_hosts_entry(ip: &str, host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    (h == "localhost" && is_loopback_ip(ip)) || (h == "broadcasthost" && ip == "255.255.255.255")
}

/// PURE: parse `scutil --dns` into the configured resolvers' nameservers,
/// deduped in first-seen order. A `nameserver[N] : <ip>` line carries the address;
/// a loopback address is flagged (something on-box is resolving every lookup).
fn parse_dns(text: &str) -> Vec<Resolver> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("nameserver[") else {
            continue;
        };
        // `0] : 8.8.8.8` — the address is the tail after the FIRST ':'. IPv6
        // addresses contain more ':' but sit entirely in that tail, so a single
        // split is correct.
        let Some((_, val)) = rest.split_once(':') else {
            continue;
        };
        let server = val.trim().to_string();
        if server.is_empty() {
            continue;
        }
        if seen.insert(server.clone()) {
            let loopback = is_loopback_ip(&server);
            out.push(Resolver { server, loopback });
        }
    }
    out
}

/// PURE: parse `security dump-trust-settings -d` for the ADMIN domain. A clean
/// machine reports "No Trust Settings were found." → `Ok(empty)`. A populated dump
/// announces "Number of trusted certs = N" and lists `Cert N: <name>` — each is an
/// admin-installed trust override. Neither marker means the read failed for lack
/// of privilege (or an unrecognized shape) → an HONEST SKIP, never a fake clean.
fn parse_trust_settings(text: &str) -> Surface<Vec<TrustedRoot>> {
    let low = text.to_lowercase();
    if low.contains("no trust settings were found") {
        return Ok(Vec::new());
    }
    if !low.contains("number of trusted certs") {
        return Err(
            "couldn't read the admin trust-settings domain (may need elevated privileges)"
                .to_string(),
        );
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("Cert ") else {
            continue;
        };
        // `0: mitmproxy` — the name is the tail after the first ':'.
        let Some((_, name)) = rest.split_once(':') else {
            continue;
        };
        let name = name.trim().to_string();
        if name.is_empty() {
            continue;
        }
        let apple = name_is_apple(&name);
        out.push(TrustedRoot { name, apple });
    }
    Ok(out)
}

/// PURE: parse `security find-certificate -a` label lines. `security` prints each
/// cert's attributes including `"labl"<blob>="<name>"` (and a hex form for
/// non-ASCII labels, always followed by the ASCII rendering in quotes). We take
/// the text inside the LAST quoted pair on each label line, deduped. An empty
/// keychain yields an honest empty list.
fn parse_system_certs(text: &str) -> Vec<SysCert> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("\"labl\"") {
            continue;
        }
        let Some(label) = extract_quoted_tail(line) else {
            continue;
        };
        if label.is_empty() {
            continue;
        }
        if seen.insert(label.clone()) {
            let apple = name_is_apple(&label);
            out.push(SysCert { label, apple });
        }
    }
    out
}

/// PURE: the quoted VALUE of a `security` attribute line, or `None` when the value
/// is unquoted. `security` renders a label as `"labl"<blob>="Name"` (or
/// `…=0x…  "Name"`), so the value is a quoted span AFTER the `=` separator. A NULL
/// attribute prints `"labl"<blob>=<NULL>` (value unquoted) — this must yield `None`,
/// NOT the quotes around the attribute NAME before the `=` (which used to return the
/// bogus literal "labl" and fabricate a certificate finding). The first `=` is
/// always the attr/value separator (an attribute name never contains `=`).
fn extract_quoted_tail(line: &str) -> Option<String> {
    let (_, value) = line.split_once('=')?;
    let end = value.rfind('"')?;
    let start = value[..end].rfind('"')?;
    Some(value[start + 1..end].to_string())
}

/// PURE: parse `profiles show`. "There are no configuration profiles installed" →
/// `Ok(empty)` (an honest clean). A privilege denial → an HONEST SKIP. Otherwise
/// pull each `profileIdentifier: <id>`; if the listing is a shape we can't read
/// any identifier from, SKIP rather than fabricate a clean.
fn parse_profiles(text: &str) -> Surface<Vec<Profile>> {
    let low = text.to_lowercase();
    if low.contains("there are no configuration profiles installed") {
        return Ok(Vec::new());
    }
    if profiles_denied(&low) {
        return Err(
            "needs elevated privileges to list system configuration profiles".to_string(),
        );
    }
    const KEY: &str = "profileIdentifier:";
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for line in text.lines() {
        if let Some(idx) = line.find(KEY) {
            let id = line[idx + KEY.len()..].trim().to_string();
            if !id.is_empty() && seen.insert(id.clone()) {
                out.push(Profile { id });
            }
        }
    }
    if out.is_empty() {
        return Err("couldn't parse the configuration-profiles listing".to_string());
    }
    Ok(out)
}

/// PURE: did the `profiles` read fail for lack of privilege?
fn profiles_denied(low: &str) -> bool {
    low.contains("not authorized")
        || low.contains("requires root")
        || low.contains("must be run as root")
}

// ---------------------------------------------------------------------------
// Pure summary + telemetry frame + plain-speech findings
// ---------------------------------------------------------------------------

/// The headline counts folded into telemetry + the posture readout. A skipped
/// surface contributes a reason (never a fabricated zero silently passed as clean).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Summary {
    proxies: usize,
    host_redirects: usize,
    host_blocks: usize,
    trusted_roots_total: usize,
    trusted_roots_non_apple: usize,
    resolvers_total: usize,
    loopback_resolvers: usize,
    system_certs_non_apple: usize,
    profiles: usize,
    /// Surfaces that could not be read, each with an honest reason.
    skips: Vec<String>,
}

impl Summary {
    /// The loud "something may be intercepting you" count: proxies + host
    /// redirects + non-Apple admin-trusted roots + on-box resolvers. Blocks,
    /// profiles, and non-Apple System-keychain certs are reported as context but
    /// have too many benign causes to alarm on by count alone.
    fn notable(&self) -> usize {
        self.proxies + self.host_redirects + self.trusted_roots_non_apple + self.loopback_resolvers
    }
}

/// PURE: fold the findings into the secret-free headline summary. A skipped
/// surface records its reason and contributes zero to the counts.
fn summarize(f: &Findings) -> Summary {
    let mut skips = Vec::new();

    let proxies = match &f.proxies {
        Ok(v) => v.len(),
        Err(e) => {
            skips.push(format!("proxy config: {e}"));
            0
        }
    };
    let (host_redirects, host_blocks) = match &f.hosts {
        Ok(v) => (
            v.iter().filter(|h| h.kind == HostsKind::Redirect).count(),
            v.iter().filter(|h| h.kind == HostsKind::Block).count(),
        ),
        Err(e) => {
            skips.push(format!("hosts file: {e}"));
            (0, 0)
        }
    };
    let (trusted_roots_total, trusted_roots_non_apple) = match &f.trusted_roots {
        Ok(v) => (v.len(), v.iter().filter(|c| !c.apple).count()),
        Err(e) => {
            skips.push(format!("trusted root CAs: {e}"));
            (0, 0)
        }
    };
    let system_certs_non_apple = match &f.system_certs {
        Ok(v) => v.iter().filter(|c| !c.apple).count(),
        Err(e) => {
            skips.push(format!("System keychain certs: {e}"));
            0
        }
    };
    let (resolvers_total, loopback_resolvers) = match &f.resolvers {
        Ok(v) => (v.len(), v.iter().filter(|r| r.loopback).count()),
        Err(e) => {
            skips.push(format!("DNS resolvers: {e}"));
            (0, 0)
        }
    };
    let profiles = match &f.profiles {
        Ok(v) => v.len(),
        Err(e) => {
            skips.push(format!("config profiles: {e}"));
            0
        }
    };

    Summary {
        proxies,
        host_redirects,
        host_blocks,
        trusted_roots_total,
        trusted_roots_non_apple,
        resolvers_total,
        loopback_resolvers,
        system_certs_non_apple,
        profiles,
        skips,
    }
}

/// PURE: one PLAIN-SPEECH line per finding — the "did you set that up?" prose. The
/// rogue-root line is deliberately loud (a trusted root breaks ALL your TLS). Home
/// paths are redacted (a profile id/label never should, but this is belt-and-braces).
fn human_findings(f: &Findings) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(proxies) = &f.proxies {
        for p in proxies {
            out.push(match p.kind {
                "PAC" => format!(
                    "An auto-proxy config (PAC) at {} is steering your web traffic — did you set that up?",
                    p.target
                ),
                _ => format!(
                    "A {} proxy at {} is intercepting your web traffic — did you set that up?",
                    p.kind, p.target
                ),
            });
        }
    }
    if let Ok(hosts) = &f.hosts {
        for h in hosts {
            out.push(match h.kind {
                HostsKind::Redirect => format!(
                    "Your hosts file points {} at {} — a redirect that could send that site's traffic to another machine; did you add this?",
                    h.host, h.ip
                ),
                HostsKind::Block => format!(
                    "Your hosts file maps {} to {} (a local/blocked address) — usually an ad-blocker or dev override; confirm you added it.",
                    h.host, h.ip
                ),
            });
        }
    }
    if let Ok(roots) = &f.trusted_roots {
        for c in roots {
            out.push(if c.apple {
                format!(
                    "A trust override for '{}' (an Apple certificate) is set in the admin domain — usually benign.",
                    c.name
                )
            } else {
                format!(
                    "'{}' is installed as a trusted root certificate — a rogue root CA can silently decrypt ALL your HTTPS traffic. Remove it in Keychain Access unless you added it deliberately.",
                    c.name
                )
            });
        }
    }
    if let Ok(resolvers) = &f.resolvers {
        for r in resolvers.iter().filter(|r| r.loopback) {
            out.push(format!(
                "DNS is being handled by {} on this machine — a local resolver sees every lookup you make; confirm you set it up (a VPN, Pi-hole, or dnscrypt would).",
                r.server
            ));
        }
    }
    if let Ok(certs) = &f.system_certs {
        for c in certs.iter().filter(|c| !c.apple) {
            out.push(format!(
                "A non-Apple certificate '{}' sits in the System keychain — worth confirming it's one you or your organization installed.",
                c.label
            ));
        }
    }
    if let Ok(profiles) = &f.profiles {
        for p in profiles {
            out.push(format!(
                "A configuration profile '{}' is installed — profiles can add root CAs and proxies; confirm it's from you or your organization.",
                p.id
            ));
        }
    }
    out.into_iter().map(|s| crate::introspect::redact_home(&s)).collect()
}

/// PURE: render one surface as `{available, items}` or `{available:false, reason}`
/// — structured only, NEVER a byte of raw command output.
fn surface_json<T, G: Fn(&T) -> serde_json::Value>(
    surface: &Surface<Vec<T>>,
    render: G,
) -> serde_json::Value {
    match surface {
        Ok(items) => json!({
            "available": true,
            "items": items.iter().map(render).collect::<Vec<_>>(),
        }),
        Err(reason) => json!({
            "available": false,
            "reason": crate::introspect::redact_home(reason),
        }),
    }
}

/// PURE: build the `security.interception` telemetry payload — structured facts
/// (the found proxies / hosts entries / trusted roots / resolvers / certs /
/// profiles), the headline counts, the plain-speech findings, and the honest
/// skips. SECRET-FREE BY CONSTRUCTION: it emits only the parsed findings, never a
/// byte of raw command output.
fn build_frame(f: &Findings) -> serde_json::Value {
    let s = summarize(f);
    json!({
        "available": true,
        "notable": s.notable(),
        "proxies": surface_json(&f.proxies, |p: &Proxy| json!({"kind": p.kind, "target": p.target})),
        "hosts": surface_json(&f.hosts, |h: &HostsEntry| json!({"host": h.host, "ip": h.ip, "kind": h.kind.wire()})),
        "trusted_roots": surface_json(&f.trusted_roots, |c: &TrustedRoot| json!({"name": c.name, "apple": c.apple})),
        "system_certs": surface_json(&f.system_certs, |c: &SysCert| json!({"label": c.label, "apple": c.apple})),
        "resolvers": surface_json(&f.resolvers, |r: &Resolver| json!({"server": r.server, "loopback": r.loopback})),
        "profiles": surface_json(&f.profiles, |p: &Profile| json!({"id": p.id})),
        "counts": {
            "proxies": s.proxies,
            "host_redirects": s.host_redirects,
            "host_blocks": s.host_blocks,
            "trusted_roots": s.trusted_roots_total,
            "trusted_roots_non_apple": s.trusted_roots_non_apple,
            "resolvers": s.resolvers_total,
            "loopback_resolvers": s.loopback_resolvers,
            "system_certs_non_apple": s.system_certs_non_apple,
            "profiles": s.profiles,
        },
        "findings": human_findings(f),
        "skips": s.skips,
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

/// A one-line interception summary for `posture.rs`'s read-only report, or `None`
/// if the check has not run yet (so posture shows nothing stale). SECRET-FREE —
/// counts only, plus an honest note when a surface was skipped for lack of
/// privilege. It names the user's own remediation (Settings / Keychain Access).
pub fn posture_line() -> Option<String> {
    let s = (*LAST_SUMMARY.lock().ok()?).clone()?;
    let skip_note = if s.skips.is_empty() {
        String::new()
    } else {
        format!(
            " ({} surface(s) needed a privilege I don't have — reported honestly, not as clean)",
            s.skips.len()
        )
    };
    if s.notable() == 0 {
        return Some(format!(
            "Interception check: no proxy, no non-Apple admin-trusted root CA, {} DNS resolver(s), \
             hosts file clean — nothing appears to be intercepting your traffic. Read-only.{skip_note}",
            s.resolvers_total
        ));
    }
    let mut parts = Vec::new();
    if s.proxies > 0 {
        parts.push(format!("{} proxy(ies)", s.proxies));
    }
    if s.host_redirects > 0 {
        parts.push(format!("{} hosts redirect(s)", s.host_redirects));
    }
    if s.trusted_roots_non_apple > 0 {
        parts.push(format!("{} non-Apple trusted root CA(s)", s.trusted_roots_non_apple));
    }
    if s.loopback_resolvers > 0 {
        parts.push(format!("{} on-box DNS resolver(s)", s.loopback_resolvers));
    }
    Some(format!(
        "Interception check: {} — something may be sitting between you and the network; review in \
         System Settings / Keychain Access (read-only — I change nothing).{skip_note}",
        parts.join(", ")
    ))
}

// ---------------------------------------------------------------------------
// Collectors — each drives the INJECTED runner and folds through its pure parser.
// Testable with a canned runner (no real subprocess); the real read is elsewhere.
// ---------------------------------------------------------------------------

async fn collect_proxies<F, Fut>(run: &F) -> Surface<Vec<Proxy>>
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    match run(SCUTIL, PROXY_ARGS, READ_TIMEOUT).await {
        ReadOutput::Text(t) => Ok(parse_proxy(&t)),
        ReadOutput::Unavailable(why) => Err(why),
    }
}

async fn collect_dns<F, Fut>(run: &F) -> Surface<Vec<Resolver>>
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    match run(SCUTIL, DNS_ARGS, READ_TIMEOUT).await {
        ReadOutput::Text(t) => Ok(parse_dns(&t)),
        ReadOutput::Unavailable(why) => Err(why),
    }
}

async fn collect_trust<F, Fut>(run: &F) -> Surface<Vec<TrustedRoot>>
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    match run(SECURITY, TRUST_ARGS, READ_TIMEOUT).await {
        ReadOutput::Text(t) => parse_trust_settings(&t),
        ReadOutput::Unavailable(why) => Err(why),
    }
}

async fn collect_system_certs<F, Fut>(run: &F) -> Surface<Vec<SysCert>>
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    match run(SECURITY, CERTS_ARGS, READ_TIMEOUT).await {
        ReadOutput::Text(t) => Ok(parse_system_certs(&t)),
        ReadOutput::Unavailable(why) => Err(why),
    }
}

async fn collect_profiles<F, Fut>(run: &F) -> Surface<Vec<Profile>>
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    match run(PROFILES, PROFILES_ARGS, READ_TIMEOUT).await {
        ReadOutput::Text(t) => parse_profiles(&t),
        ReadOutput::Unavailable(why) => Err(why),
    }
}

/// Read `/etc/hosts` (or, in tests, an injected path) via `std::fs`. An
/// unreadable file is an HONEST SKIP, never a fabricated "no entries".
fn collect_hosts_from(path: &Path) -> Surface<Vec<HostsEntry>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(parse_hosts(&text)),
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}

/// Drive every surface through the injected runner (subprocess reads) + the given
/// hosts path (`std::fs`), assembling the `Findings`. Each surface degrades
/// independently. Pure of any real subprocess (they live behind `run`); the only
/// real I/O is the hosts file read, injectable for tests.
async fn scan<F, Fut>(run: F, hosts_path: &Path) -> Findings
where
    F: Fn(&'static str, &'static [&'static str], Duration) -> Fut,
    Fut: Future<Output = ReadOutput>,
{
    Findings {
        proxies: collect_proxies(&run).await,
        hosts: collect_hosts_from(hosts_path),
        trusted_roots: collect_trust(&run).await,
        system_certs: collect_system_certs(&run).await,
        resolvers: collect_dns(&run).await,
        profiles: collect_profiles(&run).await,
    }
}

// ---------------------------------------------------------------------------
// Real command runner (NEVER reached in tests — they inject canned output)
// ---------------------------------------------------------------------------

/// Spawn one read-only command with explicit args (never a shell string), capture
/// its combined stdout+stderr, and bound it with the timeout + kill_on_drop —
/// mirroring posture.rs / exposure.rs. A spawn error, non-UTF8 output, or timeout
/// becomes a `ReadOutput::Unavailable` so that surface degrades honestly.
async fn run_real_command(
    program: &'static str,
    args: &'static [&'static str],
    timeout: Duration,
) -> ReadOutput {
    let mut cmd = Command::new(program);
    cmd.args(args).kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => {
            // Some of these tools (security, profiles) write meaningful output —
            // or their "No Trust Settings" / authorization note — to stderr;
            // combine both so the parser sees the full text.
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
            warn!(program, error = %e, "interception: command could not run");
            ReadOutput::Unavailable("not available on this machine".to_string())
        }
        Err(_) => {
            warn!(program, secs = timeout.as_secs(), "interception: command timed out");
            ReadOutput::Unavailable("the read timed out".to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// Sentinel tick + loop (the live reads are runtime-only; the cores are tested).
// ---------------------------------------------------------------------------

/// One check tick: read every local interception surface, cache the posture
/// summary, and emit the secret-free `security.interception` frame. READ-ONLY over
/// the OS (it only reads local config and sends no packets); nothing is written.
/// Runtime-only (the live reads make this inspection-verified; its parser +
/// summary + frame cores are unit-tested).
async fn sentinel_tick() {
    let findings = scan(run_real_command, Path::new(HOSTS_PATH)).await;
    set_last_summary(summarize(&findings));
    crate::telemetry::emit("system", "security.interception", build_frame(&findings));
}

/// The ambient Traffic-Interception Integrity Check loop (runtime-only; never run
/// in tests). Mirrors `exposure::sentinel_task`: a startup delay, then a slow
/// periodic `sentinel_tick`. READ-ONLY throughout — it reads local config and
/// sends no packets.
pub async fn sentinel_task(startup_delay_secs: u64, interval_secs: u64) {
    let startup =
        if startup_delay_secs == 0 { DEFAULT_STARTUP_DELAY_SECS } else { startup_delay_secs };
    let interval = if interval_secs == 0 { DEFAULT_INTERVAL_SECS } else { interval_secs };
    tokio::time::sleep(Duration::from_secs(startup)).await;
    loop {
        sentinel_tick().await;
        tokio::time::sleep(Duration::from_secs(interval)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: the parsers are tested on hand-written canned output;
// the scan fold is driven by an INJECTED runner + an injected hosts path. The real
// system commands are NEVER spawned here, and no packet is ever sent.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- proxy parser --------------------------------------------------------

    /// A faithful clean `scutil --proxy` dictionary (no proxy enabled).
    const PROXY_CLEAN: &str = "\
<dictionary> {
  ExceptionsList : <array> {
    0 : *.local
    1 : 169.254/16
  }
  FTPPassive : 1
}";

    /// A `scutil --proxy` dictionary with an HTTP + HTTPS proxy AND a PAC URL.
    const PROXY_SET: &str = "\
<dictionary> {
  ExceptionsList : <array> {
    0 : *.local
  }
  FTPPassive : 1
  HTTPEnable : 1
  HTTPPort : 8080
  HTTPProxy : 10.0.0.5
  HTTPSEnable : 1
  HTTPSPort : 8080
  HTTPSProxy : 10.0.0.5
  ProxyAutoConfigEnable : 1
  ProxyAutoConfigURLString : http://wpad.corp/wpad.dat
}";

    #[test]
    fn proxy_parser_reads_none_when_unset_and_all_when_set() {
        assert!(parse_proxy(PROXY_CLEAN).is_empty(), "no *Enable=1 => no proxy (honest clean)");

        let proxies = parse_proxy(PROXY_SET);
        assert_eq!(proxies.len(), 3, "HTTP + HTTPS + PAC: {proxies:?}");
        let http = proxies.iter().find(|p| p.kind == "HTTP").unwrap();
        assert_eq!(http.target, "10.0.0.5:8080");
        let https = proxies.iter().find(|p| p.kind == "HTTPS").unwrap();
        assert_eq!(https.target, "10.0.0.5:8080");
        let pac = proxies.iter().find(|p| p.kind == "PAC").unwrap();
        assert_eq!(pac.target, "http://wpad.corp/wpad.dat", "PAC URL kept intact despite its ':'");
    }

    #[test]
    fn proxy_parser_ignores_a_disabled_flag() {
        // A proxy present in the dict but with Enable=0 is NOT surfaced.
        let disabled = "<dictionary> {\n  HTTPEnable : 0\n  HTTPProxy : 10.0.0.5\n  HTTPPort : 8080\n}";
        assert!(parse_proxy(disabled).is_empty(), "Enable=0 must not be reported as a live proxy");
    }

    // -- hosts parser --------------------------------------------------------

    /// The stock macOS `/etc/hosts`.
    const HOSTS_DEFAULT: &str = "\
##
# Host Database
##
127.0.0.1\tlocalhost
255.255.255.255\tbroadcasthost
::1             localhost";

    /// A hijacked hosts file: a domain redirected to a routable IP (loud), a
    /// domain blocked to loopback, plus an inline comment and the defaults.
    const HOSTS_HIJACKED: &str = "\
127.0.0.1\tlocalhost
255.255.255.255\tbroadcasthost
::1             localhost
93.184.216.34   www.apple.com   # sneaky redirect
0.0.0.0 ads.tracker.example";

    #[test]
    fn hosts_parser_ignores_defaults_and_classifies_custom_entries() {
        assert!(parse_hosts(HOSTS_DEFAULT).is_empty(), "the stock hosts file is all-default");

        let entries = parse_hosts(HOSTS_HIJACKED);
        assert_eq!(entries.len(), 2, "only the two non-default entries: {entries:?}");
        let redirect = entries.iter().find(|e| e.host == "www.apple.com").unwrap();
        assert_eq!(redirect.ip, "93.184.216.34");
        assert_eq!(redirect.kind, HostsKind::Redirect, "routable IP => loud redirect");
        let block = entries.iter().find(|e| e.host == "ads.tracker.example").unwrap();
        assert_eq!(block.kind, HostsKind::Block, "0.0.0.0 => a block, not a redirect");
        // The default localhost/broadcasthost lines never appear.
        assert!(entries.iter().all(|e| e.host != "localhost" && e.host != "broadcasthost"));
    }

    // -- dns parser ----------------------------------------------------------

    /// A faithful `scutil --dns` excerpt: a normal LAN resolver, a loopback
    /// resolver (the local-interception signal), and a duplicate to dedupe.
    const DNS_SAMPLE: &str = "\
DNS configuration

resolver #1
  search domain[0] : example.com
  nameserver[0] : 2600:4041:4133::1
  nameserver[1] : 192.168.1.1
  flags    : Request A records

resolver #2
  nameserver[0] : 127.0.0.1
  nameserver[1] : 192.168.1.1
  flags    : Request A records";

    #[test]
    fn dns_parser_collects_dedupes_and_flags_loopback() {
        let resolvers = parse_dns(DNS_SAMPLE);
        // Three distinct nameservers (192.168.1.1 appears twice, deduped).
        let servers: Vec<&str> = resolvers.iter().map(|r| r.server.as_str()).collect();
        assert_eq!(servers, vec!["2600:4041:4133::1", "192.168.1.1", "127.0.0.1"], "{servers:?}");
        let loopback = resolvers.iter().find(|r| r.server == "127.0.0.1").unwrap();
        assert!(loopback.loopback, "127.0.0.1 is an on-box resolver");
        let lan = resolvers.iter().find(|r| r.server == "192.168.1.1").unwrap();
        assert!(!lan.loopback, "a LAN resolver is not loopback");
        let v6 = resolvers.iter().find(|r| r.server == "2600:4041:4133::1").unwrap();
        assert!(!v6.loopback, "the IPv6 address survived the single ':' split intact");
    }

    // -- trusted-root parser -------------------------------------------------

    #[test]
    fn trust_parser_reads_clean_rogue_apple_and_skip() {
        // Clean admin domain (the common case) => Ok(empty), NOT a skip.
        let clean = "SecTrustSettingsCopyCertificates: No Trust Settings were found.";
        assert_eq!(parse_trust_settings(clean).unwrap(), Vec::new());

        // A rogue MITM root + an Apple cert.
        let dump = "\
Number of trusted certs = 2
Cert 0: mitmproxy
   Number of trust settings : 1
   Trust Setting 0:
      Result Type            : kSecTrustSettingsResultTrustRoot
Cert 1: Apple Root CA - G3
   Number of trust settings : 1
   Trust Setting 0:
      Result Type            : kSecTrustSettingsResultTrustRoot";
        let roots = parse_trust_settings(dump).unwrap();
        assert_eq!(roots.len(), 2, "{roots:?}");
        let rogue = roots.iter().find(|c| c.name == "mitmproxy").unwrap();
        assert!(!rogue.apple, "a non-Apple trusted root is the loud artifact");
        // A GENUINE Apple root CN is recognized (a spoofable "Apple Corporate/Internal
        // Root CA" would NOT be — see apple_recognition_is_strict_and_not_name_spoofable).
        let apple = roots.iter().find(|c| c.name == "Apple Root CA - G3").unwrap();
        assert!(apple.apple, "a genuine Apple root is classified benign");

        // An unrecognized / privilege-error shape => HONEST SKIP, never a fake clean.
        let err = parse_trust_settings("SecTrustSettingsCopyCertificates: authorization denied");
        assert!(err.is_err(), "no marker => skip, never fabricate 'clean': {err:?}");
    }

    // -- system-keychain cert parser -----------------------------------------

    #[test]
    fn system_certs_parser_extracts_labels_and_flags_non_apple() {
        let out = "\
keychain: \"/Library/Keychains/System.keychain\"
    \"alis\"<blob>=\"com.apple.systemdefault\"
    \"labl\"<blob>=\"com.apple.systemdefault\"
    \"labl\"<blob>=\"Apple Worldwide Developer Relations Certification Authority\"
    \"labl\"<blob>=\"Corporate Proxy CA\"
    \"labl\"<blob>=\"com.apple.systemdefault\"";
        let certs = parse_system_certs(out);
        // Three distinct labels (the duplicate systemdefault is deduped; the "alis"
        // line is not a label line).
        assert_eq!(certs.len(), 3, "{certs:?}");
        let corp = certs.iter().find(|c| c.label == "Corporate Proxy CA").unwrap();
        assert!(!corp.apple, "a non-Apple System-keychain cert is notable");
        assert!(certs.iter().find(|c| c.label == "com.apple.systemdefault").unwrap().apple);
    }

    // -- profiles parser -----------------------------------------------------

    #[test]
    fn profiles_parser_reads_none_present_and_denied() {
        // No profiles installed => Ok(empty), an honest clean.
        let none = "There are no configuration profiles installed for user 'darwin'";
        assert_eq!(parse_profiles(none).unwrap(), Vec::new());

        // A present MDM profile.
        let present = "\
System configuration profiles:
_computerlevel[1] attribute: profileIdentifier: com.acme.mdm.rootca
_computerlevel[1] attribute: name: Acme Device Management";
        let profiles = parse_profiles(present).unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, "com.acme.mdm.rootca");

        // A privilege denial => HONEST SKIP.
        assert!(parse_profiles("profiles: Not authorized to list system profiles").is_err());
    }

    // -- summary + frame + plain-speech --------------------------------------

    /// A findings set with one of everything notable plus one skipped surface.
    fn mixed_findings() -> Findings {
        Findings {
            proxies: Ok(vec![Proxy { kind: "HTTP", target: "10.0.0.5:8080".into() }]),
            hosts: Ok(vec![
                HostsEntry { ip: "93.184.216.34".into(), host: "www.apple.com".into(), kind: HostsKind::Redirect },
                HostsEntry { ip: "0.0.0.0".into(), host: "ads.example".into(), kind: HostsKind::Block },
            ]),
            trusted_roots: Ok(vec![
                TrustedRoot { name: "mitmproxy".into(), apple: false },
                TrustedRoot { name: "Apple Root CA".into(), apple: true },
            ]),
            system_certs: Ok(vec![SysCert { label: "Corporate Proxy CA".into(), apple: false }]),
            resolvers: Ok(vec![
                Resolver { server: "127.0.0.1".into(), loopback: true },
                Resolver { server: "192.168.1.1".into(), loopback: false },
            ]),
            // A surface that needed a privilege we lack — must be an honest skip.
            profiles: Err("needs elevated privileges to list system configuration profiles".into()),
        }
    }

    #[test]
    fn summarize_counts_notable_and_records_skips() {
        let s = summarize(&mixed_findings());
        assert_eq!(s.proxies, 1);
        assert_eq!(s.host_redirects, 1);
        assert_eq!(s.host_blocks, 1);
        assert_eq!(s.trusted_roots_total, 2);
        assert_eq!(s.trusted_roots_non_apple, 1, "only the non-Apple root is loud");
        assert_eq!(s.resolvers_total, 2);
        assert_eq!(s.loopback_resolvers, 1);
        assert_eq!(s.system_certs_non_apple, 1);
        assert_eq!(s.profiles, 0, "the skipped surface contributes 0, not a fabricated count");
        assert_eq!(s.skips.len(), 1, "the skipped profiles surface is recorded honestly");
        assert!(s.skips[0].contains("config profiles"));
        // notable = proxy + redirect + non-Apple root + loopback resolver.
        assert_eq!(s.notable(), 4);
    }

    #[test]
    fn human_findings_are_plain_speech_and_the_rogue_root_is_loud() {
        let lines = human_findings(&mixed_findings());
        assert!(lines.iter().any(|l| l.contains("A HTTP proxy at 10.0.0.5:8080 is intercepting")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("points www.apple.com at 93.184.216.34")), "{lines:?}");
        // The rogue root warning is loud and names the remediation.
        let root = lines.iter().find(|l| l.contains("mitmproxy")).unwrap();
        assert!(root.contains("decrypt ALL your HTTPS traffic"), "{root}");
        assert!(root.contains("Keychain Access"), "names the user's own remediation: {root}");
        assert!(lines.iter().any(|l| l.contains("DNS is being handled by 127.0.0.1")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("Corporate Proxy CA")), "{lines:?}");
    }

    #[test]
    fn frame_is_structured_secret_free_and_carries_counts_and_skips() {
        let frame = build_frame(&mixed_findings());
        assert_eq!(frame["available"], true);
        assert_eq!(frame["notable"], 4);
        assert_eq!(frame["counts"]["trusted_roots_non_apple"], 1);
        assert_eq!(frame["counts"]["loopback_resolvers"], 1);
        // The skipped surface is available:false with a reason (never a fake empty).
        assert_eq!(frame["profiles"]["available"], false);
        assert!(frame["profiles"]["reason"].as_str().unwrap().contains("privileges"));
        // The proxy detail is present and structured.
        let proxies = frame["proxies"]["items"].as_array().unwrap();
        assert_eq!(proxies[0]["target"], "10.0.0.5:8080");
        // The plain-speech findings ride in the frame.
        let findings = frame["findings"].as_array().unwrap();
        assert!(findings.iter().any(|f| f.as_str().unwrap().contains("mitmproxy")));
    }

    #[test]
    fn frame_never_carries_raw_command_output() {
        // A readable surface whose raw text has a sentinel line must not leak that
        // raw text into the frame — only the parsed, structured facts.
        let findings = Findings {
            proxies: Ok(parse_proxy("<dictionary> {\n  HTTPEnable : 1\n  HTTPProxy : 1.2.3.4\n  HTTPPort : 3128\n  SENTINEL_RAW : leak-me\n}")),
            hosts: Ok(Vec::new()),
            trusted_roots: Ok(Vec::new()),
            system_certs: Ok(Vec::new()),
            resolvers: Ok(Vec::new()),
            profiles: Ok(Vec::new()),
        };
        let text = build_frame(&findings).to_string();
        assert!(!text.contains("SENTINEL_RAW"), "raw command output must never reach the frame: {text}");
        assert!(text.contains("1.2.3.4:3128"), "but the parsed proxy target is present");
    }

    // -- posture line --------------------------------------------------------

    #[test]
    fn posture_line_reports_clean_and_notable_with_skip_note() {
        // Notable present: names the loud findings + remediation + skip honesty.
        set_last_summary(summarize(&mixed_findings()));
        let line = posture_line().expect("a cached summary");
        assert!(line.contains("1 proxy(ies)"), "{line}");
        assert!(line.contains("1 non-Apple trusted root CA(s)"), "{line}");
        assert!(line.to_lowercase().contains("read-only"), "{line}");
        assert!(line.contains("Keychain Access") || line.contains("System Settings"), "{line}");
        assert!(line.contains("privilege I don't have"), "skip honesty surfaced: {line}");

        // All clear: an honest "nothing appears to be intercepting" line.
        set_last_summary(Summary {
            proxies: 0,
            host_redirects: 0,
            host_blocks: 0,
            trusted_roots_total: 0,
            trusted_roots_non_apple: 0,
            resolvers_total: 2,
            loopback_resolvers: 0,
            system_certs_non_apple: 0,
            profiles: 0,
            skips: Vec::new(),
        });
        let line = posture_line().expect("a cached summary");
        assert!(line.to_lowercase().contains("nothing appears to be intercepting"), "{line}");
        assert!(line.contains("2 DNS resolver(s)"), "{line}");
    }

    // -- the read commands are exactly the read-only local reads -------------

    /// The reads are the standard READ-ONLY local queries — an absolute program
    /// path plus fixed args (never a shell string). Asserted WITHOUT running them.
    /// Pins that the check never gains a write/remediation/scan-another-host argv.
    #[test]
    fn read_commands_are_read_only_local_reads() {
        assert_eq!(SCUTIL, "/usr/sbin/scutil");
        assert_eq!(SECURITY, "/usr/bin/security");
        assert_eq!(PROFILES, "/usr/bin/profiles");
        assert_eq!(PROXY_ARGS, &["--proxy"]);
        assert_eq!(DNS_ARGS, &["--dns"]);
        assert_eq!(TRUST_ARGS, &["dump-trust-settings", "-d"]);
        assert_eq!(CERTS_ARGS, &["find-certificate", "-a", "/Library/Keychains/System.keychain"]);
        assert_eq!(PROFILES_ARGS, &["show"]);
        // No read carries a mutating/remediation verb. (Substrings that legitimately
        // appear in READ subcommands — e.g. "set" inside "dump-trust-settings" — are
        // deliberately not in this list; these are the write verbs the tools would
        // use to CHANGE state, none of which appear in our fixed argv.)
        for args in [PROXY_ARGS, DNS_ARGS, TRUST_ARGS, CERTS_ARGS, PROFILES_ARGS] {
            for a in args {
                let a = a.to_lowercase();
                for verb in ["add", "delete", "remove", "install", "import", "modify"] {
                    assert!(!a.contains(verb), "no mutating verb '{verb}' in argv: {a}");
                }
            }
        }
    }

    // -- collectors + scan fold, driven by an INJECTED runner ----------------

    /// A canned runner keyed by (program, first-arg). The real system commands are
    /// NEVER spawned.
    fn canned(
        map: std::collections::HashMap<(&'static str, &'static str), ReadStub>,
    ) -> impl Fn(&'static str, &'static [&'static str], Duration) -> std::future::Ready<ReadOutput>
    {
        move |program: &'static str, args: &'static [&'static str], _to| {
            let key = (program, *args.first().unwrap_or(&""));
            let out = match map.get(&key) {
                Some(ReadStub::Text(t)) => ReadOutput::Text(t.clone()),
                Some(ReadStub::Unavail(w)) => ReadOutput::Unavailable(w.clone()),
                None => ReadOutput::Unavailable("no stub".to_string()),
            };
            std::future::ready(out)
        }
    }

    #[derive(Clone)]
    enum ReadStub {
        Text(String),
        Unavail(String),
    }

    #[tokio::test]
    async fn scan_folds_canned_reads_without_spawning_a_subprocess() {
        use std::io::Write;
        // A temp hosts file so the scan fold is fully hermetic (never the real /etc/hosts).
        let dir = std::env::temp_dir().join(format!("darwind-intercept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hosts_path = dir.join("hosts");
        let mut fh = std::fs::File::create(&hosts_path).unwrap();
        write!(fh, "{HOSTS_HIJACKED}").unwrap();
        drop(fh);

        let mut m = std::collections::HashMap::new();
        m.insert((SCUTIL, "--proxy"), ReadStub::Text(PROXY_SET.to_string()));
        m.insert((SCUTIL, "--dns"), ReadStub::Text(DNS_SAMPLE.to_string()));
        m.insert(
            (SECURITY, "dump-trust-settings"),
            ReadStub::Text("Number of trusted certs = 1\nCert 0: mitmproxy".to_string()),
        );
        m.insert((SECURITY, "find-certificate"), ReadStub::Text(String::new()));
        m.insert((PROFILES, "show"), ReadStub::Unavail("the read timed out".to_string()));
        let run = canned(m);

        let findings = scan(run, &hosts_path).await;
        let s = summarize(&findings);
        assert_eq!(s.proxies, 3, "HTTP+HTTPS+PAC parsed from the canned proxy read");
        assert_eq!(s.host_redirects, 1, "the temp hosts file's redirect was folded in");
        assert_eq!(s.trusted_roots_non_apple, 1, "the rogue root surfaced");
        assert_eq!(s.loopback_resolvers, 1);
        // The unavailable profiles read is an HONEST SKIP, not a fabricated clean.
        assert!(findings.profiles.is_err(), "an unavailable subprocess read must SKIP");
        assert!(s.skips.iter().any(|k| k.contains("config profiles")));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn collectors_skip_honestly_when_a_read_is_unavailable() {
        // Every subprocess collector must SKIP (Err) on an unavailable read —
        // never fabricate an empty "clean".
        let run = |_p: &'static str, _a: &'static [&'static str], _t: Duration| {
            std::future::ready(ReadOutput::Unavailable("not available on this machine".to_string()))
        };
        assert!(collect_proxies(&run).await.is_err());
        assert!(collect_dns(&run).await.is_err());
        assert!(collect_trust(&run).await.is_err());
        assert!(collect_system_certs(&run).await.is_err());
        assert!(collect_profiles(&run).await.is_err());
    }

    #[test]
    fn collect_hosts_from_reads_parses_and_skips_a_missing_file() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("darwind-hosts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A default hosts file => no custom entries.
        let default_path = dir.join("default");
        write!(std::fs::File::create(&default_path).unwrap(), "{HOSTS_DEFAULT}").unwrap();
        assert!(collect_hosts_from(&default_path).unwrap().is_empty());
        // A hijacked one => the redirect is surfaced.
        let hijack_path = dir.join("hijack");
        write!(std::fs::File::create(&hijack_path).unwrap(), "{HOSTS_HIJACKED}").unwrap();
        let entries = collect_hosts_from(&hijack_path).unwrap();
        assert!(entries.iter().any(|e| e.host == "www.apple.com" && e.kind == HostsKind::Redirect));
        // A missing file => an honest SKIP, never "no entries".
        assert!(collect_hosts_from(&dir.join("nope")).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn apple_recognition_is_strict_and_not_name_spoofable() {
        // REGRESSION: a loose contains("apple") let an attacker name a rogue root
        // "Apple Internal Root CA" / "Pineapple Security CA" to be waved through as
        // benign — a false clean. Only the reverse-DNS prefix or an exact known Apple
        // root CN counts now.
        assert!(name_is_apple("com.apple.kerberos.kdc"));
        assert!(name_is_apple("Apple Root CA"));
        assert!(name_is_apple("Apple Root CA - G3"));
        assert!(!name_is_apple("Apple Internal Root CA"), "spoofed name not benign");
        assert!(!name_is_apple("Pineapple Security CA"), "substring 'apple' not benign");
        assert!(!name_is_apple("Corporate Proxy CA"));
        assert!(!name_is_apple("mitmproxy"));
    }

    #[test]
    fn extract_quoted_tail_handles_a_null_label_value() {
        // REGRESSION: `"labl"<blob>=<NULL>` must yield None (no quoted value), NOT the
        // quotes around the attribute NAME (which fabricated a bogus "labl" cert).
        assert_eq!(extract_quoted_tail(r#""labl"<blob>="My Cert""#).as_deref(), Some("My Cert"));
        assert_eq!(extract_quoted_tail(r#""labl"<blob>=<NULL>"#), None);
        assert_eq!(extract_quoted_tail(r#""labl"<blob>=0x1A2B  "Trailing Name""#).as_deref(), Some("Trailing Name"));
    }

    #[test]
    fn loopback_ip_match_is_exact_not_a_prefix() {
        assert!(is_loopback_ip("127.0.0.1"));
        assert!(is_loopback_ip("::1"));
        assert!(is_loopback_ip("fe80::1"));
        assert!(is_loopback_ip("fe80::1%en0"));
        // Ordinary link-local hosts are NOT loopback (the old prefix match said yes).
        assert!(!is_loopback_ip("fe80::1abc"));
        assert!(!is_loopback_ip("fe80::1234:5678"));
        assert!(!is_loopback_ip("10.0.0.5"));
    }
}
