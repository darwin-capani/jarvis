use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

static HUB: OnceLock<broadcast::Sender<String>> = OnceLock::new();

pub fn init() {
    let (tx, _) = broadcast::channel(256);
    let _ = HUB.set(tx);
}

/// Test-only: ensure the hub exists and return a receiver so a test can
/// observe emitted envelopes (the app host's relay path) without a WS client.
#[cfg(test)]
pub fn subscribe_for_test() -> broadcast::Receiver<String> {
    let hub = HUB.get_or_init(|| broadcast::channel(256).0);
    hub.subscribe()
}

/// Fire-and-forget: events are dropped silently when no HUD is connected.
pub fn emit(source: &str, event: &str, data: Value) {
    let Some(hub) = HUB.get() else { return };
    let payload = json!({
        "ts": Utc::now().to_rfc3339(),
        "source": source,
        "event": event,
        "data": data,
    });
    let _ = hub.send(payload.to_string());
}

/// Whether a WebSocket Origin header may subscribe to the telemetry hub. The hub
/// carries live user content (voice transcript, spoken replies, app logs), so a
/// REMOTE web page — which always sends its own public-domain Origin and cannot
/// forge it — must be refused. Allowed: an absent Origin (native/CLI clients; the
/// Tauri webview may omit it), the `tauri://` custom scheme (the prod HUD), and a
/// LOOPBACK http(s) host (the dev server `http://localhost:1420` and Tauri's
/// `*.localhost` http hosts). Rejected: any http(s) Origin with a non-loopback
/// host, or any other scheme — exactly the "a page you visited connected" attack.
/// Note: DNS-rebinding can't defeat this — the Origin carries the page's own
/// hostname, never the resolved loopback IP.
fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(o) = origin.map(str::trim) else {
        return true;
    };
    if o.is_empty() || o.starts_with("tauri://") {
        return true;
    }
    match o.strip_prefix("http://").or_else(|| o.strip_prefix("https://")) {
        Some(rest) => {
            let hostport = rest.split('/').next().unwrap_or("");
            let host = hostport.split(':').next().unwrap_or(hostport);
            host == "localhost"
                || host == "127.0.0.1"
                || host == "tauri.localhost"
                || host == "ipc.localhost"
        }
        None => false,
    }
}

pub async fn serve(port: u16) {
    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            error!(addr, error = %e, "telemetry server failed to bind");
            return;
        }
    };
    info!(addr, "telemetry websocket server listening");

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(error = %e, "telemetry accept failed");
                continue;
            }
        };
        let mut rx = HUB.get().expect("telemetry::init called first").subscribe();
        tokio::spawn(async move {
            // SECURITY: validate the WebSocket Origin at the handshake BEFORE
            // subscribing. The hub carries live user content (voice transcript,
            // spoken replies, app logs), and a browser page the user visits can
            // open ws://127.0.0.1:7177 (WS is exempt from same-origin at connect
            // time) — a confused-deputy exfiltration channel. A browser ALWAYS
            // sends its own public-domain Origin and cannot forge it; the Tauri HUD
            // sends a tauri:// / loopback origin (or none for native clients). So we
            // refuse any Origin that isn't absent, tauri://, or loopback.
            use tokio_tungstenite::tungstenite::handshake::server::{Request, Response, ErrorResponse};
            let check = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
                let origin = req.headers().get("origin").and_then(|v| v.to_str().ok());
                if origin_allowed(origin) {
                    Ok(resp)
                } else {
                    warn!(?origin, "telemetry WS rejected: disallowed Origin");
                    Err(tokio_tungstenite::tungstenite::http::Response::builder()
                        .status(tokio_tungstenite::tungstenite::http::StatusCode::FORBIDDEN)
                        .body(Some("origin not allowed".to_string()))
                        .expect("static 403 response builds"))
                }
            };
            let ws = match tokio_tungstenite::accept_hdr_async(stream, check).await {
                Ok(ws) => ws,
                Err(e) => {
                    warn!(%peer, error = %e, "websocket handshake failed");
                    return;
                }
            };
            info!(%peer, "telemetry client connected");
            let (mut sink, mut source) = ws.split();
            loop {
                tokio::select! {
                    msg = rx.recv() => match msg {
                        Ok(text) => {
                            if sink.send(Message::Text(text)).await.is_err() {
                                break;
                            }
                        }
                        // Slow client missed events; keep streaming the rest.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => break,
                    },
                    inbound = source.next() => match inbound {
                        Some(Ok(_)) => continue, // HUD is read-only; ignore input
                        _ => break,
                    },
                }
            }
            info!(%peer, "telemetry client disconnected");
        });
    }
}

/// One reading of the machine's vitals, refreshed every 2s by
/// system_load_task and consumed by the router's system.query handler —
/// answering from this cache keeps the ~250ms double-refresh CPU sleep out
/// of the request path (values up to one tick stale are fine).
#[derive(Debug, Clone, Copy)]
pub struct SystemSnapshot {
    pub cpu_percent: f32,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    /// Free space on the root volume (or the first disk found); None when no
    /// disk is visible at all.
    pub disk_free_bytes: Option<u64>,
    /// Total capacity of the SAME volume `disk_free_bytes` was read from; None
    /// when no disk is visible. Carried alongside free so a disk-free PERCENTAGE
    /// can be grounded (free/total) — EDITH's anticipation tick needs the ratio,
    /// not just the absolute bytes.
    pub disk_total_bytes: Option<u64>,
    pub uptime_secs: u64,
}

/// Latest snapshot published by system_load_task; None until its first tick
/// (callers then fall back to a direct sysinfo read).
static SNAPSHOT: RwLock<Option<SystemSnapshot>> = RwLock::new(None);

pub fn latest_snapshot() -> Option<SystemSnapshot> {
    SNAPSHOT.read().ok().and_then(|guard| *guard)
}

fn publish_snapshot(snapshot: SystemSnapshot) {
    if let Ok(mut guard) = SNAPSHOT.write() {
        *guard = Some(snapshot);
    }
}

/// (free, total) bytes on the root volume, falling back to the first listed
/// disk. None when no disk is visible at all. Free and total are read from the
/// SAME disk so a disk-free PERCENTAGE (free/total) is grounded on one
/// consistent volume — EDITH's anticipation tick needs the ratio, not just the
/// absolute free bytes.
pub fn root_disk_free_and_total(disks: &sysinfo::Disks) -> Option<(u64, u64)> {
    disks
        .iter()
        .find(|d| d.mount_point() == std::path::Path::new("/"))
        .or_else(|| disks.iter().next())
        .map(|d| (d.available_space(), d.total_space()))
}

/// Emits cpu/memory/disk/uptime every 2s for the HUD's system gauges and
/// publishes the same reading as the SystemSnapshot cache.
pub async fn system_load_task() {
    let mut sys = sysinfo::System::new_all();
    let mut interval = tokio::time::interval(Duration::from_secs(2));
    loop {
        interval.tick().await;
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        let disks = sysinfo::Disks::new_with_refreshed_list();
        let (disk_free_bytes, disk_total_bytes) = match root_disk_free_and_total(&disks) {
            Some((free, total)) => (Some(free), Some(total)),
            None => (None, None),
        };
        let snapshot = SystemSnapshot {
            cpu_percent: sys.global_cpu_usage(),
            mem_used_bytes: sys.used_memory(),
            mem_total_bytes: sys.total_memory(),
            disk_free_bytes,
            disk_total_bytes,
            uptime_secs: sysinfo::System::uptime(),
        };
        publish_snapshot(snapshot);
        emit(
            "system",
            "system.load",
            json!({
                "cpu_percent": snapshot.cpu_percent,
                "mem_used_bytes": snapshot.mem_used_bytes,
                "mem_total_bytes": snapshot.mem_total_bytes,
                "disk_free_bytes": snapshot.disk_free_bytes,
                "uptime_secs": snapshot.uptime_secs,
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::origin_allowed;

    #[test]
    fn origin_allowed_blocks_web_pages_and_allows_the_hud() {
        // ALLOWED: the HUD + native clients.
        assert!(origin_allowed(None), "no Origin (native/CLI) is allowed");
        assert!(origin_allowed(Some("tauri://localhost")), "prod Tauri webview");
        assert!(origin_allowed(Some("http://localhost:1420")), "dev vite server");
        assert!(origin_allowed(Some("http://tauri.localhost")), "Tauri http-scheme host");
        assert!(origin_allowed(Some("http://127.0.0.1:5173")), "loopback dev");

        // REJECTED: the actual attack — a web page you visited connecting.
        assert!(!origin_allowed(Some("https://evil.com")));
        assert!(!origin_allowed(Some("http://evil.com")));
        assert!(!origin_allowed(Some("https://ads.example.net:443")));
        // A page can't dodge it via a look-alike host — the Origin carries the
        // page's real hostname, not the resolved loopback IP.
        assert!(!origin_allowed(Some("https://localhost.evil.com")));
        assert!(!origin_allowed(Some("https://127.0.0.1.evil.com")));
        // Non-http schemes from a page are refused too.
        assert!(!origin_allowed(Some("ws://evil.com")));
        assert!(!origin_allowed(Some("file://")));
    }
}
