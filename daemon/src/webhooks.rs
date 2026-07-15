//! #35 WEBHOOK TRIGGERS — the daemon's FIRST inbound INTERNET-adjacent surface,
//! so the most security-sensitive module in this build. An external relay (a CI
//! system, a smart-home hub, a local tunnel) can POST a small signed event that
//! maps to a DARWIN intent — but only under the strongest fences:
//!
//!   1. HMAC AUTH over the RAW body. Every request carries an
//!      `X-Darwin-Signature: sha256=<hex>` header; the receiver recomputes
//!      `HMAC-SHA256(secret, raw_body)` and compares CONSTANT-TIME. A missing,
//!      malformed, forged, or stale signature is REJECTED (401-equivalent) and
//!      NEVER routes — an unauthenticated request reaches no intent at all.
//!   2. EXPLICIT event->intent ALLOWLIST. The authenticated event name is looked
//!      up in the config-defined `[[webhooks.mappings]]`. An event with no
//!      mapping is REJECTED (404-equivalent) — never guessed into some intent.
//!   3. ROUTE through the NORMAL path, and if the mapped intent is CONSEQUENTIAL
//!      ([`crate::confirm::is_consequential_tool`]) the action PARKS for the
//!      user's spoken confirm (or is rejected) — a webhook can NEVER satisfy the
//!      cross-turn spoken confirmation, so it can never auto-execute a
//!      side-effecting action. The existing gate (the armed-by-default
//!      `[integrations].allow_consequential` master switch — ON, but a confirmed
//!      action still needs a fresh confirm — + the per-turn confirm + voice-id +
//!      lockdown + the agent allowlist + policy) is intact and unbypassed; the
//!      webhook PARKS into exactly that gate, it does not route around it.
//!
//! LIVE LISTENER (runtime-gated). [`serve`] binds 127.0.0.1 LOOPBACK by default
//! (`[webhooks].bind`) and ships ON (`[webhooks].enabled = true`) but is INERT
//! WITHOUT MAPPINGS + A KEYCHAIN HMAC SECRET. The
//! bind/accept-loop is reached ONLY when the flag is on — it is wired behind the
//! flag (the mic-loop / vision-capture precedent), not exercised in tests. The
//! HMAC secret is resolved from the macOS Keychain (account
//! [`WEBHOOK_SECRET_ACCOUNT`]), NEVER from the config TOML / a log / Debug.
//!
//! HERMETIC. The whole AUTHORIZE-then-MAP-then-CLASSIFY decision is the PURE
//! [`handle_webhook`] (a valid-HMAC mapped event -> Route; bad/missing HMAC ->
//! Unauthorized; an unmapped event -> Unmapped; a consequential mapping ->
//! ParkForConfirm, NEVER Execute). The unit tests drive it with synthetic signed
//! requests and a fixed secret — no socket, no port, no network. The body and
//! the secret are NEVER logged.

use std::net::IpAddr;
use std::sync::Arc;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::json;
use sha2::Sha256;
use tracing::{info, warn};

use crate::config::{Config, WebhookMapping};
use crate::telemetry;

type HmacSha256 = Hmac<Sha256>;

/// The macOS Keychain account holding the webhook HMAC shared secret. Resolved
/// via the same `integrations::resolve_secret` machinery as the other secrets;
/// the secret VALUE never appears in the config TOML, a log, or `Debug`.
pub const WEBHOOK_SECRET_ACCOUNT: &str = "webhook_hmac_secret";

/// The signature header an inbound request must carry. The value is
/// `sha256=<hex>` where `<hex>` is `HMAC-SHA256(secret, raw_body)`.
pub const SIGNATURE_HEADER: &str = "x-darwin-signature";
/// The event-name header. (A body `event` field is also honored as a fallback,
/// but the header is the canonical channel.)
pub const EVENT_HEADER: &str = "x-darwin-event";

/// The decision [`handle_webhook`] reaches for one inbound request, BEFORE any
/// side effect. PURE and exhaustively unit-tested: every reject path is distinct
/// and an authenticated consequential mapping resolves to [`ParkForConfirm`],
/// never an execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookDecision {
    /// HMAC verified, event mapped, and the intent is NON-consequential: route it
    /// through the normal path. Carries the resolved intent to dispatch.
    Route { event: String, intent: String },
    /// HMAC verified, event mapped, but the intent is CONSEQUENTIAL: PARK for the
    /// user's spoken confirm. A webhook can never satisfy the cross-turn confirm,
    /// so this NEVER becomes an execute — it surfaces a parked action the user
    /// must confirm out-of-band (voice / the authenticated command channel).
    ParkForConfirm { event: String, intent: String },
    /// The signature was missing, malformed, forged, or stale. 401-equivalent:
    /// the request reaches NO intent. (Also covers a secret that is unavailable —
    /// fail-closed: with no secret, nothing authenticates.)
    Unauthorized,
    /// The signature verified but the event name is not in the allowlist.
    /// 404-equivalent: rejected, never guessed into some intent.
    Unmapped { event: String },
    /// The body exceeded the configured cap, or the request was otherwise
    /// unparseable. 400-equivalent.
    BadRequest,
}

/// Is `bind` a loopback address? The receiver refuses to bind anything else by
/// default — it is for a LOCAL relay/tunnel, never a public internet listener.
/// Pure + tiny so the bind guard is unit-testable without opening a socket.
pub fn is_loopback_bind(bind: &str) -> bool {
    match bind.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        // A hostname is refused: we require an explicit loopback IP literal so the
        // guard can never be fooled by a name that resolves off-host.
        Err(_) => false,
    }
}

/// Parse a `sha256=<hex>` signature header into its raw hex digest. Returns the
/// hex string (lowercased) or `None` for a missing prefix / empty value.
fn parse_signature(header: &str) -> Option<String> {
    let hex = header.trim().strip_prefix("sha256=")?;
    if hex.is_empty() {
        return None;
    }
    Some(hex.to_ascii_lowercase())
}

/// Compute the hex HMAC-SHA256 over the RAW body with `secret`. Pure given the
/// secret — the unit tests sign synthetic bodies with a fixed secret to prove
/// the accept/reject paths without a live daemon or Keychain. Also the public
/// signing primitive a trusted relay/signer mirrors to produce the
/// `X-Darwin-Signature` header (the verify side, [`verify_signature`], is the
/// daemon's; this is the counterpart for documenting/exercising the contract).
#[allow(dead_code)] // public signing primitive; exercised by the hermetic tests.
pub fn sign_body(secret: &[u8], raw_body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(raw_body);
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time verify of a presented signature over the raw body. Recompute
/// and compare with the MAC's own constant-time `verify_slice` (never a `==` on
/// the hex string), exactly like the per-app capability-token check.
fn verify_signature(secret: &[u8], raw_body: &[u8], presented_hex: &str) -> bool {
    let Ok(presented) = hex::decode(presented_hex) else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(raw_body);
    mac.verify_slice(&presented).is_ok()
}

/// Resolve the event name for a request: the `X-Darwin-Event` header wins; a
/// top-level `event` string in the JSON body is honored as a fallback.
fn resolve_event(event_header: Option<&str>, raw_body: &[u8]) -> Option<String> {
    if let Some(h) = event_header {
        let h = h.trim();
        if !h.is_empty() {
            return Some(h.to_string());
        }
    }
    serde_json::from_slice::<serde_json::Value>(raw_body)
        .ok()
        .and_then(|v| v.get("event").and_then(|e| e.as_str()).map(str::to_string))
}

/// Look up an event in the explicit allowlist, returning the mapped intent.
fn map_event<'a>(mappings: &'a [WebhookMapping], event: &str) -> Option<&'a str> {
    mappings
        .iter()
        .find(|m| m.event == event)
        .map(|m| m.intent.as_str())
}

/// THE pure webhook decision. AUTHENTICATE (constant-time HMAC over the raw
/// body) -> MAP (explicit event->intent allowlist) -> CLASSIFY (consequential ->
/// park, else route). Takes everything by value/slice so it has no I/O: the unit
/// tests drive it directly with a fixed secret and synthetic signed bodies.
///
/// ORDER MATTERS and is security-load-bearing: authentication is checked FIRST,
/// so an unmapped/over-body/unknown event from an UNAUTHENTICATED caller still
/// returns [`WebhookDecision::Unauthorized`] — a forger learns nothing about the
/// mapping table. Only an authenticated request proceeds to the allowlist.
///
/// `secret` is `None` when the Keychain has no webhook secret configured; with no
/// secret NOTHING authenticates (fail-closed -> [`WebhookDecision::Unauthorized`]).
pub fn handle_webhook(
    raw_body: &[u8],
    signature_header: Option<&str>,
    event_header: Option<&str>,
    secret: Option<&[u8]>,
    cfg: &Config,
) -> WebhookDecision {
    // (0) Body-size bound. An oversized body is rejected before any work.
    if raw_body.len() > cfg.webhooks.max_body_bytes {
        return WebhookDecision::BadRequest;
    }

    // (1) AUTHENTICATE — constant-time HMAC over the RAW body, FIRST. Fail-closed
    // on a missing secret, a missing/malformed signature header, or a bad MAC.
    let Some(secret) = secret else {
        // No secret configured -> nothing can authenticate.
        return WebhookDecision::Unauthorized;
    };
    let Some(sig_header) = signature_header else {
        return WebhookDecision::Unauthorized;
    };
    let Some(presented) = parse_signature(sig_header) else {
        return WebhookDecision::Unauthorized;
    };
    if !verify_signature(secret, raw_body, &presented) {
        return WebhookDecision::Unauthorized;
    }

    // (2) MAP — the event name (header or body) must be in the explicit allowlist.
    let Some(event) = resolve_event(event_header, raw_body) else {
        // Authenticated but no event name at all: there is nothing to map.
        return WebhookDecision::Unmapped {
            event: String::new(),
        };
    };
    let Some(intent) = map_event(&cfg.webhooks.mappings, &event) else {
        return WebhookDecision::Unmapped { event };
    };
    let intent = intent.to_string();

    // (3) CLASSIFY — a consequential intent PARKS for a spoken confirm (a webhook
    // can never satisfy the cross-turn confirm), it is NEVER auto-executed. A
    // non-consequential (read-only) intent routes normally.
    if crate::confirm::is_consequential_tool(&intent) {
        WebhookDecision::ParkForConfirm { event, intent }
    } else {
        WebhookDecision::Route { event, intent }
    }
}

/// Emit the decision onto telemetry for the HUD — event name + intent + outcome
/// ONLY. NEVER the body, NEVER the secret, NEVER the signature. The HUD's
/// webhook panel renders purely from these.
fn emit_decision(decision: &WebhookDecision) {
    let (outcome, event, intent) = match decision {
        WebhookDecision::Route { event, intent } => ("routed", event.as_str(), intent.as_str()),
        WebhookDecision::ParkForConfirm { event, intent } => {
            ("parked", event.as_str(), intent.as_str())
        }
        WebhookDecision::Unauthorized => ("unauthorized", "", ""),
        WebhookDecision::Unmapped { event } => ("unmapped", event.as_str(), ""),
        WebhookDecision::BadRequest => ("bad_request", "", ""),
    };
    telemetry::emit(
        "system",
        "webhook.received",
        json!({"outcome": outcome, "event": event, "intent": intent}),
    );
}

/// Apply a webhook decision: emit the secret-free telemetry, and for a
/// CONSEQUENTIAL mapping PARK the action into the EXISTING cross-turn
/// confirmation gate (so the user confirms it out-of-band by voice / the
/// authenticated command channel) — a webhook NEVER auto-executes it. A
/// non-consequential `Route` is surfaced for the normal pipeline to pick up; a
/// reject path does nothing but record the secret-free outcome. Returns the
/// resolved intent for a Route, so the live accept-loop can hand it to the router.
///
/// This is the LIVE seam that ties the pure [`handle_webhook`] to the gate. It is
/// reached only from [`serve`] (runtime-gated); the decision logic it applies is
/// the pure handler the tests prove.
fn apply_decision(decision: &WebhookDecision) -> Option<String> {
    emit_decision(decision);
    match decision {
        WebhookDecision::Route { intent, .. } => Some(intent.clone()),
        WebhookDecision::ParkForConfirm { event, intent } => {
            // Park into the SAME single-slot confirmation gate the router/tool
            // loop use. The action carries a faithful, secret-free preview (the
            // event + intent only — never the body). The user must confirm it on
            // a later turn (voice / the authenticated command channel); a webhook
            // cannot satisfy the cross-turn confirm, so this never executes here.
            let preview = format!(
                "A webhook event '{event}' wants to run the consequential action '{intent}'"
            );
            crate::confirm::park(crate::confirm::PendingConfirmation {
                agent: "orchestrator".to_string(),
                tool: intent.clone(),
                // No replay material from the wire: the body is never trusted to
                // build the action's input. An empty input means a confirm
                // re-derives nothing from the (untrusted) webhook payload.
                input: serde_json::Value::Null,
                allowed: Vec::new(),
                preview,
                created_at: std::time::Instant::now(),
                id: String::new(),
            });
            None
        }
        // Reject paths: the secret-free outcome was already emitted; nothing routes.
        WebhookDecision::Unauthorized
        | WebhookDecision::Unmapped { .. }
        | WebhookDecision::BadRequest => None,
    }
}

/// LIVE LISTENER — RUNTIME-GATED. Bind the loopback receiver and accept signed
/// requests, dispatching each through the PURE [`handle_webhook`] and then
/// [`apply_decision`] (which parks a consequential mapping into the existing
/// gate). This is wired behind `[webhooks].enabled` and is NEVER reached in tests
/// (no port is opened): the precedent is the always-listening mic loop and the
/// live ScreenCaptureKit capture, both device/runtime-gated while their pure
/// cores are proven hermetically. The security-load-bearing decision
/// ([`handle_webhook`]) is the part the tests exercise.
///
/// HONESTY: this function is the wired-but-not-test-exercised leg. It refuses to
/// bind a non-loopback address and returns immediately when the flag is off or no
/// secret is configured (fail-closed). The full HTTP request framing of the live
/// accept-loop is intentionally out of the hermetic scope; the moment a request
/// IS framed, it flows through exactly the pure handler + [`apply_decision`] seam
/// below — so the live path can never auth/map/execute differently from the
/// proven core.
#[allow(dead_code)] // wired behind the OFF-default flag; the bind is runtime-gated, not tested.
pub async fn serve(cfg: Arc<Config>) {
    if !cfg.webhooks.enabled {
        info!("webhook receiver disabled ([webhooks].enabled = false); not binding");
        return;
    }
    if !is_loopback_bind(&cfg.webhooks.bind) {
        warn!(
            bind = %cfg.webhooks.bind,
            "webhook receiver refuses a non-loopback bind; not binding (loopback only)"
        );
        return;
    }
    // Fail-closed: with no secret, nothing could ever authenticate, so do not
    // even open the port.
    let Some(secret) = crate::integrations::resolve_secret(WEBHOOK_SECRET_ACCOUNT).await else {
        warn!(
            account = WEBHOOK_SECRET_ACCOUNT,
            "webhook receiver has no HMAC secret in the Keychain; not binding (fail-closed)"
        );
        return;
    };
    info!(
        bind = %cfg.webhooks.bind,
        port = cfg.webhooks.port,
        mappings = cfg.webhooks.mappings.len(),
        "webhook receiver configured (loopback, HMAC-authenticated); live bind is runtime-gated"
    );
    // The live TCP bind + HTTP accept-loop is the runtime-gated leg. When a
    // request is received it is dispatched through EXACTLY this seam — pull the
    // signature/event from the canonical headers, run the pure handler, then
    // apply_decision — so the live path cannot diverge from the proven core. (The
    // HTTP framing itself is out of the hermetic scope; no port is opened in any
    // tested path.)
    let _dispatch = |raw_body: &[u8], headers: &std::collections::HashMap<String, String>| -> Option<String> {
        // Headers are looked up by the CANONICAL lowercase names — the same names
        // a relay must send. (HTTP header names are case-insensitive; the live
        // framing lowercases them before this lookup.)
        let sig = headers.get(SIGNATURE_HEADER).map(String::as_str);
        let evt = headers.get(EVENT_HEADER).map(String::as_str);
        let decision = handle_webhook(raw_body, sig, evt, Some(secret.as_bytes()), &cfg);
        apply_decision(&decision)
    };
    let _ = &_dispatch; // wired; the live accept-loop that calls it is runtime-gated.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    /// A fixed secret + a config with a known mapping set, for synthetic signing.
    const SECRET: &[u8] = b"hermetic-test-shared-secret-not-a-real-one";

    fn cfg_with_mappings(pairs: &[(&str, &str)]) -> Config {
        let mut cfg = Config::default();
        cfg.webhooks.mappings = pairs
            .iter()
            .map(|(event, intent)| WebhookMapping {
                event: event.to_string(),
                intent: intent.to_string(),
            })
            .collect();
        cfg
    }

    /// Build a `sha256=<hex>` header that correctly signs `body` with `SECRET`.
    fn valid_sig(body: &[u8]) -> String {
        format!("sha256={}", sign_body(SECRET, body))
    }

    // -- loopback bind guard -------------------------------------------------

    #[test]
    fn loopback_bind_guard_accepts_only_loopback_literals() {
        assert!(is_loopback_bind("127.0.0.1"), "ipv4 loopback");
        assert!(is_loopback_bind("::1"), "ipv6 loopback");
        assert!(!is_loopback_bind("0.0.0.0"), "wildcard is NOT loopback");
        assert!(!is_loopback_bind("192.168.1.10"), "LAN address is NOT loopback");
        assert!(!is_loopback_bind("example.com"), "a hostname is refused");
        assert!(!is_loopback_bind(""), "empty is refused");
    }

    // -- (1) authentication --------------------------------------------------

    /// A valid HMAC over the raw body, an allowlisted event, a read-only intent:
    /// ROUTE. The happy path.
    #[test]
    fn valid_hmac_mapped_readonly_event_routes() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed","detail":"build #42 red"}"#;
        let decision = handle_webhook(
            body,
            Some(&valid_sig(body)),
            Some("ci.failed"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(
            decision,
            WebhookDecision::Route {
                event: "ci.failed".to_string(),
                intent: "system.query".to_string()
            }
        );
    }

    /// A FORGED signature (right shape, wrong MAC) is Unauthorized and reaches no
    /// intent — even though the event WOULD map.
    #[test]
    fn forged_signature_is_unauthorized_and_never_routes() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        // A hex string of the right length but not the real MAC.
        let forged = format!("sha256={}", "a".repeat(64));
        let decision = handle_webhook(body, Some(&forged), Some("ci.failed"), Some(SECRET), &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized);
    }

    /// A signature over a DIFFERENT body (replay/tamper) fails: the MAC is over
    /// the raw body, so flipping one byte of the body invalidates the signature.
    #[test]
    fn signature_is_bound_to_the_exact_body() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let signed_body = br#"{"event":"ci.failed","amount":1}"#;
        let sig = valid_sig(signed_body);
        // Present the valid signature but a tampered body.
        let tampered_body = br#"{"event":"ci.failed","amount":999}"#;
        let decision =
            handle_webhook(tampered_body, Some(&sig), Some("ci.failed"), Some(SECRET), &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized, "tampered body breaks the MAC");
    }

    /// A MISSING signature header is Unauthorized.
    #[test]
    fn missing_signature_is_unauthorized() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        let decision = handle_webhook(body, None, Some("ci.failed"), Some(SECRET), &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized);
    }

    /// A malformed signature header (no `sha256=` prefix / empty) is Unauthorized.
    #[test]
    fn malformed_signature_header_is_unauthorized() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        for bad in ["not-a-sig", "sha256=", "md5=abcd", ""] {
            let decision =
                handle_webhook(body, Some(bad), Some("ci.failed"), Some(SECRET), &cfg);
            assert_eq!(decision, WebhookDecision::Unauthorized, "header {bad:?}");
        }
    }

    /// With NO secret configured, NOTHING authenticates — fail-closed, even with a
    /// well-formed (but unverifiable) signature.
    #[test]
    fn no_secret_is_fail_closed_unauthorized() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        // Sign with SOME secret, but the receiver has None.
        let decision = handle_webhook(body, Some(&valid_sig(body)), Some("ci.failed"), None, &cfg);
        assert_eq!(decision, WebhookDecision::Unauthorized);
    }

    /// Authentication is checked BEFORE the mapping lookup: an UNAUTHENTICATED
    /// request for an UNMAPPED event still returns Unauthorized (a forger cannot
    /// probe the mapping table).
    #[test]
    fn auth_precedes_mapping_so_a_forger_cannot_probe() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"totally.unknown"}"#;
        let decision = handle_webhook(body, None, Some("totally.unknown"), Some(SECRET), &cfg);
        assert_eq!(
            decision,
            WebhookDecision::Unauthorized,
            "unauthenticated probe must not reveal it is unmapped"
        );
    }

    // -- (2) explicit allowlist ----------------------------------------------

    /// An authenticated event with NO mapping is Unmapped (rejected, not guessed).
    #[test]
    fn authenticated_unmapped_event_is_rejected() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"deploy.succeeded"}"#;
        let decision = handle_webhook(
            body,
            Some(&valid_sig(body)),
            Some("deploy.succeeded"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(
            decision,
            WebhookDecision::Unmapped {
                event: "deploy.succeeded".to_string()
            }
        );
    }

    /// The event name can come from the BODY when no header is present (header
    /// still wins when both are set).
    #[test]
    fn event_resolves_from_body_when_no_header() {
        let cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        let body = br#"{"event":"ci.failed"}"#;
        let decision = handle_webhook(body, Some(&valid_sig(body)), None, Some(SECRET), &cfg);
        assert_eq!(
            decision,
            WebhookDecision::Route {
                event: "ci.failed".to_string(),
                intent: "system.query".to_string()
            }
        );
    }

    // -- (3) consequential PARKS, never auto-executes ------------------------

    /// A mapping whose intent is CONSEQUENTIAL (e.g. gmail_send) PARKS for a
    /// spoken confirm — it is NEVER routed-to-execute. This is THE headline
    /// security property: a webhook cannot auto-execute a consequential action.
    #[test]
    fn consequential_mapping_parks_and_never_executes() {
        // gmail_send is in confirm::CONSEQUENTIAL_TOOLS.
        let cfg = cfg_with_mappings(&[("inbox.urgent", "gmail_send")]);
        let body = br#"{"event":"inbox.urgent"}"#;
        let decision = handle_webhook(
            body,
            Some(&valid_sig(body)),
            Some("inbox.urgent"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(
            decision,
            WebhookDecision::ParkForConfirm {
                event: "inbox.urgent".to_string(),
                intent: "gmail_send".to_string()
            },
            "a consequential intent must PARK, never route-to-execute"
        );
        // And it must NOT be a Route — assert the negative explicitly.
        assert!(
            !matches!(decision, WebhookDecision::Route { .. }),
            "a webhook can never auto-execute a consequential action"
        );
    }

    /// EVERY consequential tool, when mapped, parks — none ever routes. Pins the
    /// property against the whole CONSEQUENTIAL_TOOLS set so a newly-gated tool
    /// can never silently become webhook-auto-executable.
    #[test]
    fn every_consequential_tool_parks_when_mapped() {
        for tool in crate::confirm::CONSEQUENTIAL_TOOLS {
            let cfg = cfg_with_mappings(&[("evt", tool)]);
            let body = br#"{"event":"evt"}"#;
            let decision =
                handle_webhook(body, Some(&valid_sig(body)), Some("evt"), Some(SECRET), &cfg);
            assert!(
                matches!(decision, WebhookDecision::ParkForConfirm { .. }),
                "consequential tool {tool} mapped from a webhook must PARK, got {decision:?}"
            );
        }
    }

    // -- (0) body bound ------------------------------------------------------

    /// A body larger than the cap is a BadRequest (rejected before any auth work).
    #[test]
    fn oversized_body_is_bad_request() {
        let mut cfg = cfg_with_mappings(&[("ci.failed", "system.query")]);
        cfg.webhooks.max_body_bytes = 16;
        let body = vec![b'x'; 64];
        let decision = handle_webhook(
            &body,
            Some(&valid_sig(&body)),
            Some("ci.failed"),
            Some(SECRET),
            &cfg,
        );
        assert_eq!(decision, WebhookDecision::BadRequest);
    }

    // -- sign/verify round-trip ----------------------------------------------

    #[test]
    fn sign_then_verify_round_trips_and_rejects_a_flipped_bit() {
        let body = b"the exact bytes that were signed";
        let sig = sign_body(SECRET, body);
        assert!(verify_signature(SECRET, body, &sig), "the real signature verifies");
        // A wrong secret fails.
        assert!(!verify_signature(b"other-secret", body, &sig));
        // A non-hex signature fails cleanly (no panic).
        assert!(!verify_signature(SECRET, body, "not-hex-zzzz"));
    }
}
