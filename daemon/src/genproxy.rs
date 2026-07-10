//! Daemon-mediated `generate` proxy — the fix for security-review finding #4
//! (confused-deputy via the inference socket), the daemon-side of "option 2".
//!
//! BACKGROUND. A micro-app that was granted `fs_read = ["state/ipc/inference.sock"]`
//! could reach the inference server's ONE multiplexed socket, which routes ALL
//! ops (`transcribe`/`classify`/`generate`/`extract_facts`/`speak`/`converse`/
//! `consolidate`) with NO caller authorization. The seatbelt grant was socket
//! *reachability*, not op *scope*: a compromised app could make JARVIS talk
//! (`op=speak`), write the user's memory DB by proxy (`extract_facts`/
//! `consolidate`), or spam the local LLM to exhaustion.
//!
//! THE FIX. The inference server is left UNCHANGED. Instead the daemon listens
//! on a SEPARATE, restricted Unix socket `state/ipc/apps/generate.sock` and
//! micro-apps are granted ONLY that socket — never `inference.sock`. The proxy:
//!   1. Accepts ONLY `op == "generate"`. Privileged ops are not blocklisted —
//!      they are STRUCTURALLY UNROUTEABLE: this module contains no code path
//!      that constructs or forwards any op but generate (it calls
//!      [`InferenceClient::generate`] directly, never a generic op dispatch).
//!   2. Verifies the caller's capability token via [`AppRegistry::verify_token`]
//!      — the SAME HMAC/nonce machinery as the per-app relay, not a copy.
//!   3. Clamps `max_tokens` to a hard cap ([`PROXY_MAX_TOKENS`]) regardless of
//!      what the caller asks for (and floors a missing/zero/negative value to a
//!      sane default).
//!   4. Rate-limits to [`PROXY_RATE`] calls / [`PROXY_WINDOW`] per app name
//!      (rolling), addressing the LLM-exhaustion vector.
//!   5. On pass, forwards to `inference.sock` and relays the reply.
//!
//! Everything before the forward is decided by [`decide`], a PURE function the
//! unit tests drive with no socket and no LLM; the forward is behind the
//! [`Forwarder`] trait so a test can supply a loopback responder.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::apps::AppRegistry;
use crate::inference::InferenceClient;
use crate::telemetry;

/// Hard ceiling on the tokens a proxied generate may request, regardless of
/// what the caller sends. Micro-app summaries are one or two sentences; this
/// dwarfs that while bounding any single call's cost on the local model.
pub const PROXY_MAX_TOKENS: u32 = 256;
/// Hard cap on ONE inbound proxy request line, mirroring `apps::MAX_APP_LINE_BYTES`
/// (1 MiB). A generate request is small JSON; this bounds a malicious/compromised
/// micro-app from OOMing the daemon by streaming a newline-free byte flood on this
/// pre-auth socket. `read_line_bounded` errors once a line exceeds it, which drops
/// the connection.
const MAX_PROXY_LINE_BYTES: usize = 1024 * 1024;
/// Floor for a missing / zero / non-positive `max_tokens`: a sane default so a
/// caller that omits the field (or sends 0 or a negative number) still gets a
/// usable generation rather than an empty one.
pub const PROXY_MIN_TOKENS: u32 = 64;
/// Per-app rate limit: at most this many proxied generate calls within
/// [`PROXY_WINDOW`] (rolling), keyed by app name. Beyond it the call is
/// rejected with `rate_limited` — the LLM-exhaustion guard.
pub const PROXY_RATE: u32 = 30;
/// Rolling window for [`PROXY_RATE`].
pub const PROXY_WINDOW: Duration = Duration::from_secs(60);

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
    text: String,
    /// Optional; clamped in [`decide`]. Signed so a hostile negative value is
    /// representable and provably clamped (an unsigned field would reject it at
    /// parse time and hide the case the test must cover).
    #[serde(default)]
    max_tokens: Option<i64>,
}

/// The proxy's decision for one inbound line, BEFORE any token check or
/// forward. Pure and exhaustively unit-tested: the op gate and the clamp live
/// here so the tests prove a privileged op can never reach a forward and that
/// `max_tokens` is always within `[PROXY_MIN_TOKENS, PROXY_MAX_TOKENS]`.
#[derive(Debug, PartialEq)]
enum Decision {
    /// op == generate and shape is sane: verify the token, rate-limit, then
    /// forward with this clamped token budget. The token rides along so the
    /// orchestrator verifies exactly what was on the wire without re-parsing.
    Forward { name: String, token: String, text: String, max_tokens: u32 },
    /// op != generate (privileged or unknown): reject as op_not_permitted and
    /// emit app.proxy_denied. The privileged op never reaches a forward.
    OpNotPermitted { name: String, op: String },
    /// Unparseable line: drop it (no reply, no telemetry — same as the relay).
    Malformed,
}

/// Clamp a requested token budget into `[PROXY_MIN_TOKENS, PROXY_MAX_TOKENS]`.
/// A missing / zero / negative request floors to the default; anything above
/// the cap is pinned to the cap.
fn clamp_tokens(requested: Option<i64>) -> u32 {
    match requested {
        Some(n) if n > 0 => (n as u64).min(PROXY_MAX_TOKENS as u64) as u32,
        _ => PROXY_MIN_TOKENS,
    }
}

/// PURE op-gate + clamp. The ONLY accepted op is `generate`; every other value
/// (the privileged ops AND any unknown string) returns [`Decision::OpNotPermitted`],
/// so there is no code path from a non-generate op to a forward.
fn decide(raw: &str) -> Decision {
    let Ok(req) = serde_json::from_str::<ProxyRequest>(raw.trim()) else {
        return Decision::Malformed;
    };
    if req.op != "generate" {
        return Decision::OpNotPermitted {
            name: req.name,
            op: req.op,
        };
    }
    Decision::Forward {
        name: req.name,
        token: req.token,
        text: req.text,
        max_tokens: clamp_tokens(req.max_tokens),
    }
}

/// Per-app rolling-window rate limiter, keyed by app name. Pure rate math (no
/// I/O), tested directly. Mirrors the RestartGovernor's evict-then-count shape.
#[derive(Default)]
struct RateLimiter {
    /// app name -> call instants still inside the window (oldest first).
    calls: HashMap<String, Vec<Instant>>,
}

impl RateLimiter {
    /// Record a call for `name` at `now` and report whether it is ALLOWED
    /// (i.e. it stays within [`PROXY_RATE`] over [`PROXY_WINDOW`]). A rejected
    /// call is NOT recorded, so a steady stream at the limit is not permanently
    /// wedged by one rejected burst.
    fn check(&mut self, name: &str, now: Instant) -> bool {
        let marks = self.calls.entry(name.to_string()).or_default();
        marks.retain(|t| now.duration_since(*t) < PROXY_WINDOW);
        if marks.len() as u32 >= PROXY_RATE {
            return false;
        }
        marks.push(now);
        true
    }
}

/// The forward step, abstracted so the unit tests can supply a loopback
/// responder instead of a live inference server. Production uses
/// [`InferenceForwarder`], which calls [`InferenceClient::generate`] — the ONLY
/// op this module ever forwards. Native `async fn` in traits (Rust 1.75+); the
/// handler is generic over the impl so no `dyn`/boxing is needed.
trait Forwarder: Send {
    /// Forward an already-validated, already-clamped generate to the inference
    /// server. Returns the reply text, or Err on any inference failure.
    fn generate(
        &mut self,
        text: &str,
        max_tokens: u32,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}

/// Production forwarder: owns an [`InferenceClient`] over `inference.sock` and
/// forwards via its typed `generate` — never a generic op. History/facts/data
/// are empty: a micro-app summary is a one-shot, context-free generation.
struct InferenceForwarder {
    client: InferenceClient,
}

impl Forwarder for InferenceForwarder {
    async fn generate(&mut self, text: &str, max_tokens: u32) -> anyhow::Result<String> {
        // Micro-app one-shot summaries run on the base model (local_model=None).
        self.client
            .generate(text, max_tokens, &[], &[], None, None)
            .await
    }
}

/// Serve the daemon-mediated generate proxy until the process exits. Binds
/// `generate.sock` under `state/ipc/apps` (creating the dir, `0700`, and
/// `chmod 0600` on the socket), then accepts micro-app connections and handles
/// each line. Spawned from main.rs alongside the app host.
pub async fn serve(registry: Arc<AppRegistry>, generate_sock: PathBuf, inference_sock: PathBuf) {
    let forwarder = Arc::new(Mutex::new(InferenceForwarder {
        client: InferenceClient::new(inference_sock),
    }));
    let limiter = Arc::new(Mutex::new(RateLimiter::default()));

    let listener = match bind_socket(&generate_sock) {
        Ok(l) => l,
        Err(e) => {
            warn!(path = %generate_sock.display(), error = %e, "generate proxy failed to bind; micro-apps lose LLM summaries");
            return;
        }
    };
    info!(path = %generate_sock.display(), "generate proxy listening");

    loop {
        match listener.accept().await {
            Ok((stream, _peer)) => {
                let registry = registry.clone();
                let forwarder = forwarder.clone();
                let limiter = limiter.clone();
                tokio::spawn(async move {
                    handle_conn(stream, &registry, forwarder, limiter).await;
                });
            }
            Err(e) => {
                warn!(error = %e, "generate proxy accept failed");
            }
        }
    }
}

/// One [`InferenceClient`] is shared across all proxy connections behind a
/// single [`Mutex`], so concurrent micro-app generates serialize through the
/// one connection to inference.sock — matching the inference server's own
/// single-engine-lock model rather than fanning out unbounded connections.

/// Bind the proxy socket: remove any stale one, create the `0700` parent dir,
/// bind, then `chmod 0600` the socket. The dir is shared with the per-app
/// sockets, which the app host also tightens to `0700`.
fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!(path = %path.display(), error = %e, "could not remove stale generate proxy socket");
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
        warn!(path = %path.display(), error = %e, "could not tighten generate proxy permissions");
    }
}

/// Serve one accepted proxy connection: read JSONL lines and reply to each.
async fn handle_conn<F: Forwarder>(
    stream: UnixStream,
    registry: &Arc<AppRegistry>,
    forwarder: Arc<Mutex<F>>,
    limiter: Arc<Mutex<RateLimiter>>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    // BOUNDED read: this socket is spoken to by SANDBOXED, potentially-compromised
    // micro-apps, and the read happens BEFORE any token/rate check. An unbounded
    // `read_line` would grow `line` without limit until a newline arrives, so a
    // hostile app streaming a gigabyte with no `\n` could OOM (abort) the daemon.
    // We share `apps::read_line_bounded` (caps at MAX_PROXY_LINE_BYTES, errors on
    // overflow) — the same defense the sibling app-relay socket uses. `pending`
    // carries partial bytes across reads (unused here — no select! cancellation —
    // but required by the shared signature).
    let mut pending: Vec<u8> = Vec::new();
    loop {
        line.clear();
        match crate::apps::read_line_bounded(&mut reader, &mut pending, &mut line, MAX_PROXY_LINE_BYTES).await {
            Ok(0) => return, // app closed the socket
            Ok(_) => {
                let reply = handle_line(line.trim(), registry, &forwarder, &limiter).await;
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
                warn!(error = %e, "reading generate proxy socket failed");
                return;
            }
        }
    }
}

/// Handle one inbound line end-to-end and produce the reply JSON (None for a
/// malformed line, which is dropped with no reply). This is the orchestration
/// seam the unit tests drive with a mock forwarder + a fresh registry: it runs
/// the pure [`decide`], then the token check, the rate limit, and the forward,
/// emitting the same telemetry the production path does.
async fn handle_line<F: Forwarder>(
    raw: &str,
    registry: &Arc<AppRegistry>,
    forwarder: &Arc<Mutex<F>>,
    limiter: &Arc<Mutex<RateLimiter>>,
) -> Option<Value> {
    match decide(raw) {
        Decision::Malformed => {
            warn!("dropping non-JSON line on generate proxy");
            None
        }
        Decision::OpNotPermitted { name, op } => {
            // Privileged/unknown op: rejected here, never forwarded. The op
            // string is echoed onto telemetry so a probe is visible to the HUD.
            telemetry::emit(
                "system",
                "app.proxy_denied",
                json!({"name": name, "op": op}),
            );
            Some(json!({"ok": false, "error": "op_not_permitted"}))
        }
        Decision::Forward { name, token, text, max_tokens } => {
            // Token check via the SAME machinery as the per-app relay. A
            // forged/tampered/cross-app/stale/missing token fails closed.
            if !registry.verify_token(&name, &token).await {
                warn!(app = %name, "generate proxy line failed token verification");
                telemetry::emit(
                    "system",
                    "app.auth_failed",
                    json!({"name": name, "via": "genproxy"}),
                );
                return Some(json!({"ok": false, "error": "unauthorized"}));
            }
            // Per-app rate limit (LLM-exhaustion guard).
            let allowed = {
                let mut lim = limiter.lock().await;
                lim.check(&name, Instant::now())
            };
            if !allowed {
                warn!(app = %name, "generate proxy rate limit tripped");
                telemetry::emit(
                    "system",
                    "app.proxy_denied",
                    json!({"name": name, "op": "generate", "reason": "rate_limited"}),
                );
                return Some(json!({"ok": false, "error": "rate_limited"}));
            }
            // Forward the ONLY permitted op.
            let result = {
                let mut fwd = forwarder.lock().await;
                fwd.generate(&text, max_tokens).await
            };
            match result {
                Ok(reply) => Some(json!({"ok": true, "text": reply})),
                Err(e) => {
                    warn!(app = %name, error = %e, "generate proxy forward failed");
                    Some(json!({"ok": false, "error": "inference_unavailable"}))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apps::AppRegistry;
    use std::sync::atomic::{AtomicU32, Ordering};

    // -- pure op-gate + clamp ------------------------------------------------

    /// Every privileged op (and an unknown one) is op_not_permitted — never a
    /// Forward. This is the structural restriction: no non-generate op has a
    /// path to a forward.
    #[test]
    fn op_gate_permits_only_generate() {
        for op in [
            "speak",
            "extract_facts",
            "consolidate",
            "transcribe",
            "classify",
            "converse",
            "totally_unknown_op",
            "",
        ] {
            let raw = json!({"name": "global-scan", "token": "x", "op": op, "text": "hi"})
                .to_string();
            match decide(&raw) {
                Decision::OpNotPermitted { name, op: got } => {
                    assert_eq!(name, "global-scan");
                    assert_eq!(got, op);
                }
                other => panic!("op {op:?} must be op_not_permitted, got {other:?}"),
            }
        }
        // generate is the one accepted op.
        let raw = json!({"name": "global-scan", "token": "x", "op": "generate", "text": "hi"})
            .to_string();
        assert!(matches!(decide(&raw), Decision::Forward { .. }));
    }

    /// A non-JSON line is Malformed (dropped, no reply).
    #[test]
    fn malformed_line_is_dropped() {
        assert_eq!(decide("not json at all"), Decision::Malformed);
        assert_eq!(decide(""), Decision::Malformed);
    }

    /// max_tokens is clamped into [MIN, MAX] for huge, negative, zero, missing,
    /// and in-range values.
    #[test]
    fn max_tokens_is_clamped_to_the_cap() {
        assert_eq!(clamp_tokens(Some(10_000)), PROXY_MAX_TOKENS);
        assert_eq!(clamp_tokens(Some(PROXY_MAX_TOKENS as i64 + 1)), PROXY_MAX_TOKENS);
        assert_eq!(clamp_tokens(Some(-5)), PROXY_MIN_TOKENS);
        assert_eq!(clamp_tokens(Some(0)), PROXY_MIN_TOKENS);
        assert_eq!(clamp_tokens(None), PROXY_MIN_TOKENS);
        assert_eq!(clamp_tokens(Some(128)), 128);
        // And through the full decode path: a huge request clamps in Forward.
        let raw = json!({"name": "a", "token": "t", "op": "generate", "text": "x", "max_tokens": 99999})
            .to_string();
        match decide(&raw) {
            Decision::Forward { max_tokens, .. } => assert_eq!(max_tokens, PROXY_MAX_TOKENS),
            other => panic!("expected Forward, got {other:?}"),
        }
        // A negative request through the decode path floors to MIN.
        let raw = json!({"name": "a", "token": "t", "op": "generate", "text": "x", "max_tokens": -42})
            .to_string();
        match decide(&raw) {
            Decision::Forward { max_tokens, .. } => assert_eq!(max_tokens, PROXY_MIN_TOKENS),
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    // -- rate limiter --------------------------------------------------------

    /// The limiter allows exactly PROXY_RATE calls in the window, then trips,
    /// and the count is independent per app name.
    #[test]
    fn rate_limit_trips_after_the_rate_and_is_per_app() {
        let mut lim = RateLimiter::default();
        let t0 = Instant::now();
        // PROXY_RATE allowed for app "a".
        for i in 0..PROXY_RATE {
            assert!(lim.check("a", t0), "call {i} for a should be allowed");
        }
        // The next one trips.
        assert!(!lim.check("a", t0), "the call past PROXY_RATE must trip");
        // A DIFFERENT app is unaffected — per-name budgets.
        assert!(lim.check("b", t0), "app b has its own budget");
        // After the window elapses, app "a" is allowed again (rolling window).
        let later = t0 + PROXY_WINDOW + Duration::from_secs(1);
        assert!(lim.check("a", later), "the window rolled; a is allowed again");
    }

    // -- orchestration with a mock forwarder + real registry -----------------

    /// A loopback forwarder: records the (text, max_tokens) it was handed and
    /// returns a canned reply, or a forced error to exercise the
    /// inference_unavailable path. No real LLM, no real socket.
    struct MockForwarder {
        reply: String,
        fail: bool,
        last_max: Arc<AtomicU32>,
    }

    impl Forwarder for MockForwarder {
        async fn generate(&mut self, _text: &str, max_tokens: u32) -> anyhow::Result<String> {
            self.last_max.store(max_tokens, Ordering::SeqCst);
            if self.fail {
                anyhow::bail!("mock inference down");
            }
            Ok(self.reply.clone())
        }
    }

    /// Build a registry with one launched app so we can mint a real token for
    /// it and drive the proxy's verify path with the SAME machinery production
    /// uses. `apps/global-scan/manifest.toml` already exists under the project
    /// root, so discover() registers it; start() mints the token.
    async fn registry_with_started_app(name: &str) -> (Arc<AppRegistry>, String) {
        let root = project_root();
        let registry = AppRegistry::discover(&root);
        // start() spawns a lifecycle task that tries to sandbox-exec the app —
        // we do NOT want a real child here. Instead mint a token directly via
        // the test-only seam so we get a valid token without launching.
        let token = registry.mint_for_test(name).await.expect("app registered");
        (registry, token)
    }

    fn project_root() -> PathBuf {
        // daemon/ -> project root is its parent.
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        manifest_dir.parent().unwrap().to_path_buf()
    }

    fn mock(reply: &str, fail: bool) -> (Arc<Mutex<MockForwarder>>, Arc<AtomicU32>) {
        let last_max = Arc::new(AtomicU32::new(0));
        let fwd = Arc::new(Mutex::new(MockForwarder {
            reply: reply.to_string(),
            fail,
            last_max: last_max.clone(),
        }));
        (fwd, last_max)
    }

    #[tokio::test]
    async fn valid_generate_round_trips_ok() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let (fwd, last_max) = mock("a neutral one-line summary.", false);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "generate",
            "text": "summarize this", "max_tokens": 40
        })
        .to_string();
        let reply = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["text"], "a neutral one-line summary.");
        // The clamped budget (40, in range) reached the forwarder.
        assert_eq!(last_max.load(Ordering::SeqCst), 40);
    }

    #[tokio::test]
    async fn huge_max_tokens_is_clamped_before_the_forward() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let (fwd, last_max) = mock("ok", false);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "generate",
            "text": "x", "max_tokens": 100000
        })
        .to_string();
        let reply = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(reply["ok"], true);
        assert_eq!(last_max.load(Ordering::SeqCst), PROXY_MAX_TOKENS);
    }

    #[tokio::test]
    async fn each_privileged_op_is_denied_and_never_forwarded() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let (fwd, last_max) = mock("should never be returned", false);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        for op in ["speak", "extract_facts", "consolidate", "transcribe", "classify", "converse"] {
            let raw = json!({
                "name": "global-scan", "token": token, "op": op, "text": "x"
            })
            .to_string();
            let reply = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
            assert_eq!(reply["ok"], false, "op {op} must be denied");
            assert_eq!(reply["error"], "op_not_permitted", "op {op}");
        }
        // The forwarder was NEVER touched — last_max is still its init value.
        assert_eq!(last_max.load(Ordering::SeqCst), 0, "no privileged op reached the forward");
    }

    #[tokio::test]
    async fn forged_tampered_crossapp_and_missing_tokens_are_unauthorized() {
        let (registry, good_token) = registry_with_started_app("global-scan").await;
        let (fwd, _last) = mock("ok", false);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));

        // Forged token.
        let forged = json!({
            "name": "global-scan", "token": "deadbeef", "op": "generate", "text": "x"
        })
        .to_string();
        let r = handle_line(&forged, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "forged token");

        // Tampered: a valid token with a flipped hex nibble.
        let mut chars: Vec<char> = good_token.chars().collect();
        chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
        let tampered: String = chars.into_iter().collect();
        let tampered = json!({
            "name": "global-scan", "token": tampered, "op": "generate", "text": "x"
        })
        .to_string();
        let r = handle_line(&tampered, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "tampered token");

        // Cross-app: a valid global-scan token presented under another name.
        let crossapp = json!({
            "name": "some-other-app", "token": good_token, "op": "generate", "text": "x"
        })
        .to_string();
        let r = handle_line(&crossapp, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "cross-app token");

        // Missing token.
        let missing = json!({"name": "global-scan", "op": "generate", "text": "x"}).to_string();
        let r = handle_line(&missing, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["error"], "unauthorized", "missing token");
    }

    #[tokio::test]
    async fn rate_limit_trips_after_the_rate_through_the_handler() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let (fwd, _last) = mock("ok", false);
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "generate", "text": "x"
        })
        .to_string();
        // PROXY_RATE valid calls all pass.
        for _ in 0..PROXY_RATE {
            let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
            assert_eq!(r["ok"], true);
        }
        // The next is rate_limited.
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "rate_limited");
    }

    #[tokio::test]
    async fn inference_failure_relays_inference_unavailable() {
        let (registry, token) = registry_with_started_app("global-scan").await;
        let (fwd, _last) = mock("", true); // forced failure
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let raw = json!({
            "name": "global-scan", "token": token, "op": "generate", "text": "x"
        })
        .to_string();
        let r = handle_line(&raw, &registry, &fwd, &limiter).await.unwrap();
        assert_eq!(r["ok"], false);
        assert_eq!(r["error"], "inference_unavailable");
    }
}
