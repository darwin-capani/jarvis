//! Daemon-mediated `fetch` proxy — the Phase-4 fix for the two INHERENT network
//! caveats the seatbelt sandbox cannot close (docs/SANDBOX.md → *Honest
//! limitations*).
//!
//! BACKGROUND. Global-Scan was the ONLY shipped micro-app with direct network
//! egress: its manifest granted `net_hosts` = 9 public RSS hosts, and the
//! generated SBPL allowed outbound TCP to those `host-name`s plus DNS. That
//! left two caveats OPEN by construction:
//!   1. COARSE HOST FILTERING — `(remote tcp (host-name ...))` matches the
//!      connect-time NAME, not the resolved IP, so a feed host on a shared CDN
//!      shares its allow with unrelated co-tenant names on that CDN.
//!   2. DNS EXFIL — permitting DNS at all lets a compromised app encode data in
//!      query labels to an attacker-controlled nameserver, bypassing the
//!      allow-list entirely.
//!
//! THE FIX. The daemon fronts micro-app HTTP with a **daemon-mediated fetch
//! proxy** on a SEPARATE, restricted Unix socket `state/ipc/apps/fetch.sock`.
//! The app declares the hostnames it needs (`fetch_hosts` in its manifest), is
//! granted NO direct network or DNS at all (its SBPL collapses to a flat
//! `(deny network*)` plus the AF_UNIX literal grants), and asks the daemon to
//! fetch each URL. The proxy:
//!   1. Accepts ONLY `op == "fetch"`. Privileged ops are not blocklisted —
//!      they are STRUCTURALLY UNROUTEABLE: this module contains no code path
//!      that constructs or forwards any op but fetch (it calls
//!      [`UrlFetcher::fetch_once`] directly, never a generic op dispatch).
//!   2. Verifies the caller's capability token via [`AppRegistry::verify_token`]
//!      — the SAME HMAC/nonce machinery as the per-app relay and the generate
//!      proxy, not a copy.
//!   3. AUTHORIZES the URL against the app's OWN `fetch_hosts` allow-list with a
//!      pure [`authorize_url`]: https-only, exact case-insensitive host match
//!      (no subdomain/wildcard), no userinfo, port absent or 443, DNS name (no
//!      IP literal).
//!   4. GUARDS against SSRF / DNS rebinding: it resolves the host, requires
//!      every returned address to be [`ip_is_public`], and PINS the verified
//!      address into the reqwest client (`.resolve`) so nothing can rebind
//!      between the check and the connect.
//!   5. Follows redirects MANUALLY (reqwest redirect policy is `none`), re-running
//!      [`authorize_url`] against the SAME allow-list on every hop (a cross-host
//!      or unlisted redirect is denied), for at most [`FETCH_MAX_REDIRECTS`] hops.
//!   6. Streams the body incrementally, capping at [`FETCH_MAX_BYTES`] (never
//!      buffering more), and returns a lossy-UTF-8 string.
//!   7. Rate-limits to [`FETCH_RATE`] calls / [`FETCH_WINDOW`] per app name.
//!
//! Everything before the wire is decided by pure functions the unit tests drive
//! with no socket and no network ([`decide`], [`authorize_url`], [`ip_is_public`]);
//! the single network leg sits behind the [`UrlFetcher`] trait so the redirect
//! loop and every error path are tested with a mock fetcher — NO test touches
//! the wire. Errors are SECRET-FREE: the URL and body are never echoed.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};
use url::{Host, Url};

use crate::apps::AppRegistry;
use crate::telemetry;

/// Hard cap on ONE fetched body, streamed incrementally and never buffered past
/// this. A feed body is small; this dwarfs that while bounding a single fetch's
/// memory cost. Past it the fetch fails with `body_too_large`.
pub const FETCH_MAX_BYTES: usize = 2 * 1024 * 1024;
/// Per-request wall-clock timeout for the whole HTTP leg (connect + read).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Per-app rate limit: at most this many proxied fetches within [`FETCH_WINDOW`]
/// (rolling), keyed by app name. Beyond it the call is `rate_limited`.
pub const FETCH_RATE: u32 = 60;
/// Rolling window for [`FETCH_RATE`].
pub const FETCH_WINDOW: Duration = Duration::from_secs(60);
/// Hard cap on ONE inbound proxy request line, mirroring `apps::MAX_APP_LINE_BYTES`
/// (1 MiB). A fetch request is small JSON; this bounds a malicious/compromised
/// micro-app from OOMing the daemon by streaming a newline-free byte flood on
/// this pre-auth socket. `read_line_bounded` errors once a line exceeds it,
/// which drops the connection.
const MAX_PROXY_LINE_BYTES: usize = 1024 * 1024;
/// Maximum redirect hops the proxy will follow before returning
/// `too_many_redirects`. Each hop is re-authorized against the app's allow-list.
pub const FETCH_MAX_REDIRECTS: u32 = 3;

/// One inbound proxy request line. `deny_unknown_fields` would be too strict on
/// the wire (a caller may add fields), so we read only what we need; the op is
/// validated structurally in [`decide`].
#[derive(Debug, Deserialize)]
struct ProxyRequest {
    #[serde(default)]
    name: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    op: String,
    #[serde(default)]
    url: String,
}

/// The proxy's decision for one inbound line, BEFORE any token check or fetch.
/// Pure and unit-tested: the op gate lives here so the tests prove a privileged
/// op can never reach a fetch.
#[derive(Debug, PartialEq)]
enum Decision {
    /// op == fetch and shape is sane: verify the token, rate-limit, authorize
    /// the URL, then fetch. The token + url ride along so the orchestrator acts
    /// on exactly what was on the wire without re-parsing.
    Forward { name: String, token: String, url: String },
    /// op != fetch (privileged or unknown): reject as op_not_permitted and emit
    /// app.proxy_denied. The privileged op never reaches a fetch.
    OpNotPermitted { name: String, op: String },
    /// Unparseable line: drop it (no reply, no telemetry — same as the relay).
    Malformed,
}

/// PURE op-gate. The ONLY accepted op is `fetch`; every other value (any
/// privileged op AND any unknown string) returns [`Decision::OpNotPermitted`],
/// so there is no code path from a non-fetch op to a network leg.
fn decide(raw: &str) -> Decision {
    let Ok(req) = serde_json::from_str::<ProxyRequest>(raw.trim()) else {
        return Decision::Malformed;
    };
    if req.op != "fetch" {
        return Decision::OpNotPermitted {
            name: req.name,
            op: req.op,
        };
    }
    Decision::Forward {
        name: req.name,
        token: req.token,
        url: req.url,
    }
}

/// PURE URL authorization against an app's `fetch_hosts` allow-list. Returns the
/// parsed [`Url`] on success, or a stable `&'static str` reason on refusal (the
/// orchestrator maps every refusal to the `url_not_permitted` / `redirect_denied`
/// wire kind — the reason is for logs/tests only, never echoed to the app).
///
/// The rules (exhaustively unit-tested, incl. the userinfo trick, subdomain
/// evasion, an odd port, an IP literal, and an uppercase host):
///   - scheme MUST be `https` (http and everything else refused);
///   - NO userinfo (`user:pass@` refused);
///   - port absent or exactly 443;
///   - host MUST be a DNS name — an IP literal (v4 or v6) is refused, so a
///     caller cannot smuggle a raw address past the host allow-list;
///   - host MUST match one of `allowed_hosts` EXACTLY, case-insensitive, with NO
///     subdomain or wildcard matching (`evil-api.binance.com` and
///     `sub.api.binance.com` are both refused against `api.binance.com`).
fn authorize_url(url_str: &str, allowed_hosts: &[String]) -> Result<Url, &'static str> {
    let url = Url::parse(url_str).map_err(|_| "unparseable")?;
    if url.scheme() != "https" {
        return Err("scheme_not_https");
    }
    // Userinfo (`user:pass@host`) is a phishing/confusion vector and never valid
    // for a feed fetch.
    if !url.username().is_empty() || url.password().is_some() {
        return Err("userinfo_present");
    }
    // Port must be the https default (absent) or an explicit 443.
    match url.port() {
        None => {}
        Some(443) => {}
        Some(_) => return Err("bad_port"),
    }
    // Host must be a DNS NAME. An IP literal is refused so a caller cannot reach
    // an arbitrary address by skipping the name allow-list entirely.
    let host = match url.host() {
        Some(Host::Domain(d)) => d,
        Some(Host::Ipv4(_)) | Some(Host::Ipv6(_)) => return Err("ip_literal"),
        None => return Err("no_host"),
    };
    // EXACT, case-insensitive match — no subdomain, no wildcard. (The url crate
    // already lowercases a domain host on parse; the eq_ignore is belt-and-braces.)
    let allowed = allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host));
    if !allowed {
        return Err("host_not_allowed");
    }
    Ok(url)
}

/// PURE SSRF / DNS-rebinding guard: is `ip` a globally-routable public address?
/// Rejects loopback, private (10/8, 172.16/12, 192.168/16), CGNAT shared
/// (100.64/10), link-local (169.254/16, fe80::/10), unique-local (fc00::/7),
/// unspecified, multicast, broadcast, and documentation ranges. An IPv4-mapped
/// IPv6 address is judged by its embedded v4. Exhaustively table-tested.
fn ip_is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_is_public(v4),
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            // ::ffff:a.b.c.d — judge the embedded v4 (an attacker can otherwise
            // encode a private v4 as a mapped v6).
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ipv4_is_public(v4);
            }
            ipv6_is_public(v6)
        }
    }
}

fn ipv4_is_public(v4: Ipv4Addr) -> bool {
    if v4.is_unspecified()
        || v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_multicast()
    {
        return false;
    }
    let o = v4.octets();
    // 100.64.0.0/10 — CGNAT shared address space.
    if o[0] == 100 && (o[1] & 0xc0) == 64 {
        return false;
    }
    // 0.0.0.0/8 — "this network".
    if o[0] == 0 {
        return false;
    }
    // 192.0.0.0/24 — IETF protocol assignments.
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return false;
    }
    // 240.0.0.0/4 — reserved for future use (255.255.255.255 already caught by
    // is_broadcast above).
    if o[0] >= 240 {
        return false;
    }
    true
}

fn ipv6_is_public(v6: Ipv6Addr) -> bool {
    if v6.is_unspecified() || v6.is_loopback() || v6.is_multicast() {
        return false;
    }
    let seg = v6.segments();
    // fc00::/7 — unique local addresses.
    if (seg[0] & 0xfe00) == 0xfc00 {
        return false;
    }
    // fe80::/10 — link-local.
    if (seg[0] & 0xffc0) == 0xfe80 {
        return false;
    }
    // 2001:db8::/32 — documentation.
    if seg[0] == 0x2001 && seg[1] == 0x0db8 {
        return false;
    }
    true
}

/// One completed HTTP round-trip (NO redirect following): the status, the
/// `Location` header if any, and the (already body-capped) response text.
#[derive(Debug)]
struct FetchOnce {
    status: u16,
    location: Option<String>,
    body: String,
}

/// The failure kinds a single fetch leg can produce, each mapping to a distinct
/// secret-free wire error kind. Kept separate from a generic error so the mock
/// fetcher can drive the SSRF and body-cap paths hermetically.
#[derive(Debug, PartialEq)]
enum FetchError {
    /// The host resolved to no address, or to any non-public address (SSRF /
    /// rebinding guard) -> `host_resolves_private`.
    HostResolvesPrivate,
    /// The streamed body exceeded [`FETCH_MAX_BYTES`] -> `body_too_large`.
    BodyTooLarge,
    /// Any other network-leg failure (connect / TLS / timeout / read) ->
    /// `fetch_failed`.
    Failed,
}

/// Is this an HTTP redirect status the proxy follows manually?
fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

/// The single network leg, abstracted so the unit tests supply a scripted mock
/// instead of a live network. Production uses [`ReqwestFetcher`]. Native
/// `async fn` in traits (Rust 1.75+); the handler is generic over the impl so no
/// `dyn`/boxing is needed.
trait UrlFetcher: Send {
    /// Perform ONE HTTP GET (no redirect following) against an already-authorized
    /// URL, returning the status/location/capped-body or a typed [`FetchError`].
    fn fetch_once(
        &mut self,
        url: &Url,
    ) -> impl std::future::Future<Output = Result<FetchOnce, FetchError>> + Send;
}

/// Production fetcher: resolves the host, enforces the SSRF/rebinding guard,
/// PINS the verified address into a per-request reqwest client, GETs with
/// redirect policy `none`, and streams the body under [`FETCH_MAX_BYTES`].
#[derive(Default)]
struct ReqwestFetcher;

impl UrlFetcher for ReqwestFetcher {
    async fn fetch_once(&mut self, url: &Url) -> Result<FetchOnce, FetchError> {
        // authorize_url guarantees a DNS-name host; this is defense in depth.
        let host = url.host_str().ok_or(FetchError::Failed)?.to_string();

        // Resolve + SSRF/rebinding guard: EVERY returned address must be public,
        // and there must be at least one. Empty resolution is treated as
        // "resolves private" (nothing safe to connect to).
        let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), 443u16)).await {
            Ok(it) => it.collect(),
            // A resolution failure is a plain fetch failure (host may be down /
            // NXDOMAIN); it is not an SSRF signal.
            Err(_) => return Err(FetchError::Failed),
        };
        if addrs.is_empty() || !addrs.iter().all(|sa| ip_is_public(sa.ip())) {
            return Err(FetchError::HostResolvesPrivate);
        }
        // Pin the VERIFIED address so nothing rebinds between the check and the
        // connect. reqwest validates TLS against `host` (SNI + cert) while
        // connecting to this exact IP.
        let chosen = addrs[0];

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT)
            .https_only(true)
            .resolve(&host, chosen)
            .build()
            .map_err(|_| FetchError::Failed)?;

        let mut resp = client.get(url.clone()).send().await.map_err(|_| FetchError::Failed)?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        // Stream the body incrementally, NEVER buffering past the cap.
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    if buf.len().saturating_add(chunk.len()) > FETCH_MAX_BYTES {
                        return Err(FetchError::BodyTooLarge);
                    }
                    buf.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Err(FetchError::Failed),
            }
        }
        Ok(FetchOnce {
            status,
            location,
            body: String::from_utf8_lossy(&buf).into_owned(),
        })
    }
}

/// Per-app rolling-window rate limiter, keyed by app name. Pure rate math (no
/// I/O), tested directly. Mirrors the generate proxy's evict-then-count shape.
#[derive(Default)]
struct RateLimiter {
    /// app name -> call instants still inside the window (oldest first).
    calls: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    /// Record a call for `name` at `now` and report whether it is ALLOWED (stays
    /// within [`FETCH_RATE`] over [`FETCH_WINDOW`]). A rejected call is NOT
    /// recorded, so a steady stream at the limit is not permanently wedged by one
    /// rejected burst.
    fn check(&mut self, name: &str, now: Instant) -> bool {
        let marks = self.calls.entry(name.to_string()).or_default();
        marks.retain(|t| now.duration_since(*t) < FETCH_WINDOW);
        if marks.len() as u32 >= FETCH_RATE {
            return false;
        }
        marks.push(now);
        true
    }
}

/// Serve the daemon-mediated fetch proxy until the process exits. Binds
/// `fetch.sock` under `state/ipc/apps` (creating the dir `0700` and `chmod 0600`
/// on the socket), then accepts micro-app connections and handles each line.
/// Spawned from main.rs alongside the app host and the generate proxy.
pub async fn serve(registry: Arc<AppRegistry>, fetch_sock: PathBuf) {
    let fetcher = Arc::new(Mutex::new(ReqwestFetcher));
    let limiter = Arc::new(Mutex::new(RateLimiter::default()));

    let listener = match bind_socket(&fetch_sock) {
        Ok(l) => l,
        Err(e) => {
            warn!(path = %fetch_sock.display(), error = %e, "fetch proxy failed to bind; micro-apps lose proxied fetches");
            return;
        }
    };
    info!(path = %fetch_sock.display(), "fetch proxy listening");

    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let registry = registry.clone();
                let fetcher = fetcher.clone();
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    handle_conn(stream, &registry, fetcher, limiter).await;
                });
            }
            Err(e) => {
                warn!(error = %e, "fetch proxy accept failed");
            }
        }
    }
}

/// Bind the proxy socket: remove any stale one, create the `0700` parent dir,
/// bind, then `chmod 0600` the socket. The dir is shared with the per-app and
/// generate-proxy sockets, which also tighten it to `0700`.
fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %path.display(), error = %e, "could not remove stale fetch proxy socket");
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        set_mode(parent, 0o700);
    }
    let listener = UnixListener::bind(path)?;
    set_mode(path, 0o600);
    Ok(listener)
}

/// chmod best-effort: a failed tightening is defense-in-depth, not load-bearing
/// (token verification is the real gate), so warn and continue.
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        warn!(path = %path.display(), error = %e, "could not tighten fetch proxy permissions");
    }
}

/// Serve one accepted proxy connection: read JSONL lines and reply to each.
async fn handle_conn<F: UrlFetcher>(
    stream: UnixStream,
    registry: &Arc<AppRegistry>,
    fetcher: Arc<Mutex<F>>,
    limiter: Arc<Mutex<RateLimiter>>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    // BOUNDED read: this pre-auth socket is spoken to by SANDBOXED,
    // potentially-compromised micro-apps. We share `apps::read_line_bounded`
    // (caps at MAX_PROXY_LINE_BYTES, errors on overflow) — the same defense the
    // sibling app-relay and generate-proxy sockets use. `pending` carries partial
    // bytes across reads (required by the shared signature).
    let mut pending: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match crate::apps::read_line_bounded(&mut reader, &mut pending, &mut line, MAX_PROXY_LINE_BYTES).await {
            Ok(0) => return, // app closed the socket
            Ok(_) => {
                let reply = handle_line(line.trim(), registry, &fetcher, &limiter).await;
                if let Some(reply) = reply {
                    let mut out = reply.to_string();
                    out.push('\n');
                    if write_half.write_all(out.as_bytes()).await.is_err() {
                        return;
                    }
                    if write_half.flush().await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                warn!(error = %e, "reading fetch proxy socket failed");
                return;
            }
        }
    }
}

/// Emit an `app.proxy_denied` for a security denial and return its reply JSON.
/// SECRET-FREE: only the app name, the op, and a stable reason — never the URL
/// or the body.
fn deny(name: &str, reason: &str) -> Option<Value> {
    telemetry::emit(
        "system",
        "app.proxy_denied",
        json!({"name": name, "op": "fetch", "reason": reason}),
    );
    Some(json!({"ok": false, "error": reason}))
}

/// Handle one inbound line end-to-end and produce the reply JSON (None for a
/// malformed line, which is dropped with no reply). This is the orchestration
/// seam the unit tests drive with a mock fetcher + a fresh registry: it runs the
/// pure [`decide`], the token check, the rate limit, the pure [`authorize_url`],
/// then the redirect loop over the fetcher — emitting the same telemetry the
/// production path does.
async fn handle_line<F: UrlFetcher>(
    raw: &str,
    registry: &Arc<AppRegistry>,
    fetcher: &Arc<Mutex<F>>,
    limiter: &Arc<Mutex<RateLimiter>>,
) -> Option<Value> {
    let (name, token, url_str) = match decide(raw) {
        Decision::Malformed => {
            warn!("dropping non-JSON line on fetch proxy");
            return None;
        }
        Decision::OpNotPermitted { name, op } => {
            // Privileged/unknown op: rejected here, never fetched. The op string
            // is echoed onto telemetry so a probe is visible to the HUD.
            telemetry::emit("system", "app.proxy_denied", json!({"name": name, "op": op}));
            return Some(json!({"ok": false, "error": "op_not_permitted"}));
        }
        Decision::Forward { name, token, url } => (name, token, url),
    };

    // Token check via the SAME machinery as the per-app relay. A
    // forged/tampered/cross-app/stale/missing token fails closed.
    if !registry.verify_token(&name, &token).await {
        warn!(app = %name, "fetch proxy line failed token verification");
        telemetry::emit(
            "system",
            "app.auth_failed",
            json!({"name": name, "via": "fetchproxy"}),
        );
        return Some(json!({"ok": false, "error": "unauthorized"}));
    }

    // Per-app rate limit.
    let allowed = {
        let mut lim = limiter.lock().await;
        lim.check(&name, Instant::now())
    };
    if !allowed {
        warn!(app = %name, "fetch proxy rate limit tripped");
        return deny(&name, "rate_limited");
    }

    // Authorize the URL against THIS app's own fetch_hosts allow-list. An app
    // with an empty (or absent) list gets url_not_permitted for everything.
    let allowed_hosts = registry.fetch_hosts_for(&name).await.unwrap_or_default();
    let mut current = match authorize_url(&url_str, &allowed_hosts) {
        Ok(u) => u,
        Err(reason) => {
            warn!(app = %name, reason, "fetch proxy url rejected");
            return deny(&name, "url_not_permitted");
        }
    };

    // Manual redirect loop: fetch, and on a redirect re-authorize the target
    // against the SAME allow-list, up to FETCH_MAX_REDIRECTS hops.
    let mut hops: u32 = 0;
    loop {
        let fetched = {
            let mut f = fetcher.lock().await;
            f.fetch_once(&current).await
        };
        let fetched = match fetched {
            Ok(f) => f,
            Err(FetchError::HostResolvesPrivate) => {
                warn!(app = %name, "fetch proxy host resolves to a non-public address");
                return deny(&name, "host_resolves_private");
            }
            Err(FetchError::BodyTooLarge) => {
                warn!(app = %name, "fetch proxy body exceeded the cap");
                return deny(&name, "body_too_large");
            }
            Err(FetchError::Failed) => {
                warn!(app = %name, "fetch proxy network leg failed");
                return Some(json!({"ok": false, "error": "fetch_failed"}));
            }
        };

        if is_redirect(fetched.status) {
            let Some(loc) = fetched.location else {
                // A redirect status with no Location is unusable.
                warn!(app = %name, "fetch proxy redirect without a Location");
                return Some(json!({"ok": false, "error": "fetch_failed"}));
            };
            // Resolve relative against the current URL, then re-authorize.
            let target = match current.join(&loc) {
                Ok(t) => t,
                Err(_) => return deny(&name, "redirect_denied"),
            };
            let next = match authorize_url(target.as_str(), &allowed_hosts) {
                Ok(u) => u,
                Err(reason) => {
                    warn!(app = %name, reason, "fetch proxy redirect target rejected");
                    return deny(&name, "redirect_denied");
                }
            };
            hops += 1;
            if hops > FETCH_MAX_REDIRECTS {
                warn!(app = %name, "fetch proxy exceeded the redirect limit");
                return deny(&name, "too_many_redirects");
            }
            current = next;
            continue;
        }

        // A non-redirect status: success. The app parses the body; a non-2xx
        // status simply won't parse as a feed and the app tolerates that.
        return Some(json!({"ok": true, "status": fetched.status, "body": fetched.body}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppRegistry;
    use std::collections::VecDeque;

    // -- pure op-gate --------------------------------------------------------

    /// Every privileged op (and an unknown one) is op_not_permitted — never a
    /// Forward. This is the structural restriction: no non-fetch op has a path to
    /// a network leg.
    #[test]
    fn op_gate_permits_only_fetch() {
        for op in [
            "generate",
            "speak",
            "extract_facts",
            "consolidate",
            "transcribe",
            "classify",
            "converse",
            "totally_unknown_op",
            "",
        ] {
            let raw = json!({"name": "global-scan", "token": "x", "op": op, "url": "https://feeds.npr.org/x"})
                .to_string();
            match decide(&raw) {
                Decision::OpNotPermitted { name, op: got } => {
                    assert_eq!(name, "global-scan");
                    assert_eq!(got, op);
                }
                other => panic!("op {op:?} must be op_not_permitted, got {other:?}"),
            }
        }
        // fetch is the one accepted op.
        let raw = json!({"name": "global-scan", "token": "x", "op": "fetch", "url": "https://feeds.npr.org/x"})
            .to_string();
        assert!(matches!(decide(&raw), Decision::Forward { .. }));
    }

    /// A non-JSON line is Malformed (dropped, no reply).
    #[test]
    fn malformed_line_is_dropped() {
        assert_eq!(decide("not json at all"), Decision::Malformed);
        assert_eq!(decide(""), Decision::Malformed);
    }

    // -- authorize_url exhaustive -------------------------------------------

    fn hosts(hs: &[&str]) -> Vec<String> {
        hs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn authorize_url_accepts_an_exact_https_host() {
        let allow = hosts(&["api.binance.com"]);
        let u = authorize_url("https://api.binance.com/api/v3/time", &allow).expect("exact host ok");
        assert_eq!(u.host_str(), Some("api.binance.com"));
        // Explicit :443 is fine too.
        assert!(authorize_url("https://api.binance.com:443/x", &allow).is_ok());
    }

    #[test]
    fn authorize_url_matches_host_case_insensitively() {
        let allow = hosts(&["api.binance.com"]);
        // Uppercase in the URL host must still match the lowercase allow entry.
        assert!(authorize_url("https://API.BINANCE.COM/x", &allow).is_ok());
        // ...and an uppercase allow entry matches a lowercase host.
        let allow_uc = hosts(&["API.BINANCE.COM"]);
        assert!(authorize_url("https://api.binance.com/x", &allow_uc).is_ok());
    }

    #[test]
    fn authorize_url_rejects_http_and_other_schemes() {
        let allow = hosts(&["api.binance.com"]);
        assert_eq!(authorize_url("http://api.binance.com/x", &allow), Err("scheme_not_https"));
        assert_eq!(authorize_url("ftp://api.binance.com/x", &allow), Err("scheme_not_https"));
        assert_eq!(authorize_url("file:///etc/passwd", &allow), Err("scheme_not_https"));
    }

    #[test]
    fn authorize_url_rejects_userinfo_trick() {
        let allow = hosts(&["api.binance.com"]);
        // user:pass@allowed-host — the real host IS allowed, but userinfo is a
        // confusion vector and must be refused outright.
        assert_eq!(
            authorize_url("https://user:pass@api.binance.com/x", &allow),
            Err("userinfo_present")
        );
        // The classic evasion: allowed host in the userinfo, attacker host as the
        // real authority. Refused (userinfo present is caught before host match).
        assert_eq!(
            authorize_url("https://api.binance.com@evil.com/x", &allow),
            Err("userinfo_present")
        );
    }

    #[test]
    fn authorize_url_rejects_subdomain_and_prefix_evasion() {
        let allow = hosts(&["api.binance.com"]);
        // A different registrable label that merely CONTAINS the allowed host.
        assert_eq!(authorize_url("https://evil-api.binance.com/x", &allow), Err("host_not_allowed"));
        // A subdomain OF the allowed host is NOT the allowed host.
        assert_eq!(authorize_url("https://sub.api.binance.com/x", &allow), Err("host_not_allowed"));
        // A suffix trick.
        assert_eq!(authorize_url("https://api.binance.com.evil.com/x", &allow), Err("host_not_allowed"));
    }

    #[test]
    fn authorize_url_rejects_nonstandard_port() {
        let allow = hosts(&["api.binance.com"]);
        assert_eq!(authorize_url("https://api.binance.com:8443/x", &allow), Err("bad_port"));
        assert_eq!(authorize_url("https://api.binance.com:80/x", &allow), Err("bad_port"));
    }

    #[test]
    fn authorize_url_rejects_ip_literals() {
        // Even if an operator somehow listed an IP, an IP-literal URL is refused
        // (host must be a DNS name), closing the raw-address escape.
        let allow = hosts(&["api.binance.com", "127.0.0.1", "::1"]);
        assert_eq!(authorize_url("https://127.0.0.1/x", &allow), Err("ip_literal"));
        assert_eq!(authorize_url("https://[::1]/x", &allow), Err("ip_literal"));
        assert_eq!(authorize_url("https://169.254.169.254/latest/meta-data", &allow), Err("ip_literal"));
    }

    #[test]
    fn authorize_url_rejects_unlisted_host_and_empty_allowlist() {
        assert_eq!(authorize_url("https://evil.com/x", &hosts(&["api.binance.com"])), Err("host_not_allowed"));
        // Empty allow-list -> everything is url_not_permitted.
        assert_eq!(authorize_url("https://api.binance.com/x", &[]), Err("host_not_allowed"));
    }

    // -- ip_is_public table --------------------------------------------------

    #[test]
    fn ip_is_public_table() {
        let private: &[&str] = &[
            // loopback
            "127.0.0.1", "::1",
            // private v4
            "10.0.0.1", "10.255.255.255", "172.16.0.1", "172.31.255.255", "192.168.0.1", "192.168.255.255",
            // link-local
            "169.254.0.1", "169.254.169.254", "fe80::1",
            // unique-local v6
            "fc00::1", "fd12:3456:789a::1",
            // unspecified
            "0.0.0.0", "::",
            // multicast
            "224.0.0.1", "239.255.255.255", "ff02::1",
            // broadcast
            "255.255.255.255",
            // documentation
            "192.0.2.1", "198.51.100.1", "203.0.113.1", "2001:db8::1",
            // CGNAT shared
            "100.64.0.1", "100.127.255.255",
            // 0.0.0.0/8 and reserved
            "0.1.2.3", "240.0.0.1",
            // IPv4-mapped private
            "::ffff:10.0.0.1", "::ffff:192.168.1.1",
        ];
        for s in private {
            let ip: IpAddr = s.parse().unwrap();
            assert!(!ip_is_public(ip), "{s} must be judged NON-public");
        }
        let public: &[&str] = &[
            "8.8.8.8", "1.1.1.1", "93.184.216.34",
            // 172.15/172.32 are OUTSIDE 172.16/12 -> public
            "172.15.255.255", "172.32.0.1",
            // public v6
            "2606:2800:220:1:248:1893:25c8:1946",
            // IPv4-mapped public
            "::ffff:8.8.8.8",
        ];
        for s in public {
            let ip: IpAddr = s.parse().unwrap();
            assert!(ip_is_public(ip), "{s} must be judged public");
        }
    }

    // -- rate limiter --------------------------------------------------------

    #[test]
    fn rate_limit_trips_after_the_rate_and_is_per_app() {
        let mut lim = RateLimiter::default();
        let t0 = Instant::now();
        for i in 0..FETCH_RATE {
            assert!(lim.check("a", t0), "call {i} for a should be allowed");
        }
        assert!(!lim.check("a", t0), "the call past FETCH_RATE must trip");
        assert!(lim.check("b", t0), "app b has its own budget");
        let later = t0 + FETCH_WINDOW + Duration::from_secs(1);
        assert!(lim.check("a", later), "the window rolled; a is allowed again");
    }

    // -- orchestration with a mock fetcher + real registry -------------------

    /// A scripted fetcher: pops one canned outcome per `fetch_once` and records
    /// the URLs it was called with (to assert a redirect chain). No real network.
    struct MockFetcher {
        responses: VecDeque<Result<FetchOnce, FetchError>>,
        seen: Vec<String>,
    }

    impl UrlFetcher for MockFetcher {
        async fn fetch_once(&mut self, url: &Url) -> Result<FetchOnce, FetchError> {
            self.seen.push(url.as_str().to_string());
            self.responses
                .pop_front()
                .unwrap_or(Err(FetchError::Failed))
        }
    }

    fn ok_body(status: u16, body: &str) -> Result<FetchOnce, FetchError> {
        Ok(FetchOnce { status, location: None, body: body.to_string() })
    }

    fn redirect_to(status: u16, location: &str) -> Result<FetchOnce, FetchError> {
        Ok(FetchOnce { status, location: Some(location.to_string()), body: String::new() })
    }

    fn mock(responses: Vec<Result<FetchOnce, FetchError>>) -> Arc<Mutex<MockFetcher>> {
        Arc::new(Mutex::new(MockFetcher { responses: responses.into(), seen: Vec::new() }))
    }

    /// Register the shipped global-scan (its manifest now declares the 9
    /// fetch_hosts) and mint a REAL token for it via the test-only seam — so the
    /// proxy's verify path runs the SAME HMAC/nonce machinery production uses,
    /// without spawning a sandboxed child.
    async fn registry_with_started_app(name: &str) -> (Arc<AppRegistry>, String) {
        let root = project_root();
        let registry = AppRegistry::discover(&root);
        let token = registry.mint_for_test(name).await.expect("app registered");
        (registry, token)
    }

    fn project_root() -> PathBuf {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.parent().unwrap().to_path_buf()
    }

    #[tokio::test]
    async fn valid_fetch_round_trips_ok() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![ok_body(200, "<rss>ok</rss>")]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/1001/rss.xml"
        })
        .to_string();
        let reply = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["status"], 200);
        assert_eq!(reply["body"], "<rss>ok</rss>");
        assert_eq!(fwd.lock().await.seen, vec!["https://feeds.npr.org/1001/rss.xml".to_string()]);
    }

    #[tokio::test]
    async fn each_privileged_op_is_denied_and_never_fetched() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![ok_body(200, "should never be returned")]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        for op in ["generate", "speak", "extract_facts", "consolidate", "transcribe", "classify", "converse"] {
            let raw = json!({
                "name": "global-scan", "token": token, "op": op,
                "url": "https://feeds.npr.org/x"
            })
            .to_string();
            let reply = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
            assert_eq!(reply["ok"], false, "op {op} must be denied");
            assert_eq!(reply["error"], "op_not_permitted", "op {op}");
        }
        // The fetcher was NEVER touched.
        assert!(fwd.lock().await.seen.is_empty(), "no privileged op reached the fetch");
    }

    #[tokio::test]
    async fn forged_tampered_crossapp_and_missing_tokens_are_unauthorized() {
        let (registry, good_token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));

        let forged = json!({
            "name": "global-scan", "token": "deadbeef", "op": "fetch", "url": "https://feeds.npr.org/x"
        })
        .to_string();
        let r = handle_line(&forged, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "forged token");

        // Tampered: a valid token with a flipped hex nibble.
        let mut chars: Vec<char> = good_token.chars().collect();
        chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        let tampered = json!({
            "name": "global-scan", "token": tampered, "op": "fetch", "url": "https://feeds.npr.org/x"
        })
        .to_string();
        let r = handle_line(&tampered, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "tampered token");

        // Cross-app: a valid global-scan token presented under another name.
        let crossapp = json!({
            "name": "some-other-app", "token": good_token, "op": "fetch", "url": "https://feeds.npr.org/x"
        })
        .to_string();
        let r = handle_line(&crossapp, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "cross-app token");

        // Missing token.
        let missing = json!({"name": "global-scan", "op": "fetch", "url": "https://feeds.npr.org/x"}).to_string();
        let r = handle_line(&missing, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "missing token");

        assert!(fwd.lock().await.seen.is_empty(), "no unauthorized line reached the fetch");
    }

    #[tokio::test]
    async fn unlisted_host_is_url_not_permitted() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        // A host NOT in global-scan's fetch_hosts.
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch", "url": "https://evil.com/x"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "url_not_permitted");
        assert!(fwd.lock().await.seen.is_empty(), "a rejected URL never reaches the fetch");
    }

    #[tokio::test]
    async fn http_scheme_is_url_not_permitted() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch", "url": "http://feeds.npr.org/x"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "url_not_permitted");
    }

    #[tokio::test]
    async fn a_same_host_redirect_is_followed_up_to_the_limit() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        // 3 same-host redirects then a 200 — followed (<=3 hops).
        let fwd = mock(vec![
            redirect_to(301, "https://feeds.npr.org/step2"),
            redirect_to(302, "https://feeds.npr.org/step3"),
            redirect_to(307, "https://feeds.npr.org/final"),
            ok_body(200, "<rss>final</rss>"),
        ]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/start"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(r["body"], "<rss>final</rss>");
        // All four legs were fetched, in order, ending at the final URL.
        assert_eq!(
            fwd.lock().await.seen,
            vec![
                "https://feeds.npr.org/start".to_string(),
                "https://feeds.npr.org/step2".to_string(),
                "https://feeds.npr.org/step3".to_string(),
                "https://feeds.npr.org/final".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_relative_redirect_resolves_against_the_current_url() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![
            redirect_to(302, "/moved/rss.xml"), // relative Location
            ok_body(200, "<rss>ok</rss>"),
        ]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/1001/rss.xml"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], true);
        assert_eq!(
            fwd.lock().await.seen,
            vec![
                "https://feeds.npr.org/1001/rss.xml".to_string(),
                "https://feeds.npr.org/moved/rss.xml".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn a_cross_host_redirect_is_denied() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        // The redirect target is NOT in the allow-list.
        let fwd = mock(vec![redirect_to(301, "https://evil.com/x")]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/start"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "redirect_denied");
        // Only the first leg was fetched; the cross-host target never was.
        assert_eq!(fwd.lock().await.seen, vec!["https://feeds.npr.org/start".to_string()]);
    }

    #[tokio::test]
    async fn a_redirect_to_http_is_denied() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        // Same host but downgraded to http -> re-authorization refuses it.
        let fwd = mock(vec![redirect_to(301, "http://feeds.npr.org/x")]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/start"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "redirect_denied");
    }

    #[tokio::test]
    async fn too_many_redirects_is_denied() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        // 4 same-host redirects -> the 4th hop trips the limit.
        let fwd = mock(vec![
            redirect_to(301, "https://feeds.npr.org/2"),
            redirect_to(301, "https://feeds.npr.org/3"),
            redirect_to(301, "https://feeds.npr.org/4"),
            redirect_to(301, "https://feeds.npr.org/5"),
            ok_body(200, "unreached"),
        ]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/1"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "too_many_redirects");
    }

    #[tokio::test]
    async fn body_too_large_is_reported() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![Err(FetchError::BodyTooLarge)]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/huge"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "body_too_large");
    }

    #[tokio::test]
    async fn host_resolving_private_is_reported() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![Err(FetchError::HostResolvesPrivate)]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/x"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "host_resolves_private");
    }

    #[tokio::test]
    async fn network_failure_relays_fetch_failed() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let fwd = mock(vec![Err(FetchError::Failed)]);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/x"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "fetch_failed");
    }

    #[tokio::test]
    async fn rate_limit_trips_after_the_rate_through_the_handler() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        // Enough OK responses to satisfy every permitted call.
        let mut responses = Vec::new();
        for _ in 0..FETCH_RATE {
            responses.push(ok_body(200, "<rss/>"));
        }
        let fwd = mock(responses);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "fetch",
            "url": "https://feeds.npr.org/x"
        })
        .to_string();
        for _ in 0..FETCH_RATE {
            let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
            assert_eq!(r["ok"], true);
        }
        // The next is rate_limited (and never reaches the fetcher).
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "rate_limited");
    }
}
