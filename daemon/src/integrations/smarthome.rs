//! Home Assistant smart-home bridge client for agent "dume" (Home & Environment).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests), and holds a base URL + a long-lived
//! access token it attaches per request at the moment of the send. The token
//! VALUE is never logged, never stored on the transport, never put in an error or
//! a `Debug` field — only its presence (a bool) is ever recorded.
//!
//! HONESTY: control goes through the user's OWN Home Assistant (or compatible)
//! hub over its local REST API. DARWIN does NOT talk HomeKit directly — raw
//! HomeKit is not cleanly reachable from a macOS daemon — so the persona and copy
//! say so plainly. The bridge is the user's hub; DARWIN only relays to it.
//!
//! Two tiers of methods, mirroring the foundation's safety model:
//!   * READ (safe): [`SmartHomeClient::list_devices`] (GET /api/states ->
//!     entity_id, friendly_name, state), [`SmartHomeClient::device_state`]
//!     (GET /api/states/<entity_id>) — no gate, plain GETs.
//!   * CONSEQUENTIAL (gated): [`SmartHomeClient::set_device`] takes an
//!     [`ActionMode`]. In [`ActionMode::DryRun`] it builds and returns a preview
//!     of exactly the service call it WOULD make and issues NO request; only in
//!     [`ActionMode::Execute`] does it POST /api/services/<domain>/<service>
//!     exactly once. Call sites get the mode from the foundation's
//!     `gate(confirm)`, so with `[integrations].allow_consequential` false (the
//!     shipped default) a control always previews — no device moves.
//!
//! Every method returns a concise human-facing `String` — what dume would say —
//! while parsing only the typed fields it needs. Non-2xx responses map to
//! friendly, secret-free errors via [`map_status`].

use serde::Deserialize;
use tracing::info;

use super::{
    resolve_secret, status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport,
    IntegrationResult, ReqwestTransport, StatusOutcome,
};

/// The Keychain account holding the user's Home Assistant base URL (e.g.
/// `http://homeassistant.local:8123`). Pasted in Settings. Not OAuth.
pub const ACCOUNT_URL: &str = "homeassistant_url";
/// The Keychain account holding the user's Home Assistant long-lived access
/// token. Pasted in Settings; rides only the Authorization header at call time.
pub const ACCOUNT_TOKEN: &str = "homeassistant_token";

/// How many devices to name in a list summary before collapsing to "and N more".
const LIST_PREVIEW: usize = 8;

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields dume actually needs are decoded.
// `#[serde(default)]` keeps parsing resilient to the many extra keys Home
// Assistant returns (last_changed, context, attributes we don't read, …).
// ---------------------------------------------------------------------------

/// The slice of an entity's `attributes` dume reads: its human label.
#[derive(Debug, Clone, Deserialize, Default)]
struct EntityAttributes {
    #[serde(default)]
    friendly_name: String,
}

/// One entity state object, as returned by `GET /api/states` (an array) and
/// `GET /api/states/<entity_id>` (a single object).
#[derive(Debug, Clone, Deserialize)]
struct EntityState {
    entity_id: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    attributes: EntityAttributes,
}

impl EntityState {
    /// The label to show: the friendly_name when present, else the entity_id.
    fn label(&self) -> &str {
        if self.attributes.friendly_name.is_empty() {
            &self.entity_id
        } else {
            &self.attributes.friendly_name
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Home Assistant client bound to a transport, a base URL, and a long-lived
/// token.
///
/// Construct with [`SmartHomeClient::connect`] (resolves URL + token from the
/// Keychain, wires the real transport) or, in tests, [`SmartHomeClient::with_token`]
/// (an explicit base URL + fake token + a `MockTransport`). The token is held only
/// to compose the per-request `Authorization` header; it is never logged and the
/// `Debug` impl below redacts it.
pub struct SmartHomeClient<T: HttpTransport> {
    transport: T,
    /// Hub base URL with any trailing slash trimmed (so `{base}/api/...` is clean).
    base: String,
    token: String,
}

/// Custom `Debug` that NEVER prints the token — only that one is present, plus the
/// base URL (a host address the user typed, not a secret). So a `{:?}` of a client
/// (in a log line, a panic message, a test) can't leak the access token.
impl<T: HttpTransport> std::fmt::Debug for SmartHomeClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmartHomeClient")
            .field("base", &self.base)
            .field("token_present", &!self.token.is_empty())
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> SmartHomeClient<T> {
    /// Build a client with an explicitly supplied base URL + token. Used by tests
    /// (paired with `MockTransport`) and by any caller that has already resolved
    /// the secrets. The token is consumed into the client and never logged; the
    /// base URL has its trailing slash trimmed so path joins are clean.
    pub fn with_token(transport: T, base_url: impl Into<String>, token: impl Into<String>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        Self {
            transport,
            base,
            token: token.into(),
        }
    }

    /// Compose a request with the Home Assistant Authorization header, attaching
    /// the Bearer token HERE — at the moment of the call — and nowhere else. The
    /// token never lands on the transport or in a log.
    fn request(&self, method: HttpMethod, path: &str) -> HttpRequest {
        HttpRequest::new(method, format!("{}{path}", self.base))
            .header("Authorization", format!("Bearer {}", self.token))
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// List the hub's devices and their states (`GET /api/states`). Read-only.
    /// Returns a count plus the first few "<friendly name> (<state>)".
    pub async fn list_devices(&self) -> IntegrationResult<String> {
        let req = self.request(HttpMethod::Get, "/api/states");
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "listing your devices")?;

        let states: Vec<EntityState> = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("listing your devices returned an unexpected response"))?;
        info!(count = states.len(), "smarthome: listed devices");

        if states.is_empty() {
            return Ok("Your Home Assistant hub reports no devices.".to_string());
        }
        let lines: Vec<String> = states
            .iter()
            .take(LIST_PREVIEW)
            .map(|s| format!("{} ({})", s.label(), s.state))
            .collect();
        let more = states.len().saturating_sub(lines.len());
        let mut out = format!(
            "Your hub has {} device{}: {}",
            states.len(),
            if states.len() == 1 { "" } else { "s" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    /// Read one entity's current state (`GET /api/states/<entity_id>`). Read-only.
    /// Returns "<friendly name> is <state>".
    pub async fn device_state(&self, entity_id: &str) -> IntegrationResult<String> {
        let req = self.request(HttpMethod::Get, &format!("/api/states/{entity_id}"));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "reading that device")?;

        let entity: EntityState = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("that device's state was not in the expected shape"))?;
        info!(entity = %entity.entity_id, "smarthome: read device state");
        Ok(format!("{} is {}.", entity.label(), entity.state))
    }

    // -- CONSEQUENTIAL (gated by ActionMode) ---------------------------------

    /// Control a device by calling a Home Assistant service on its domain.
    ///
    /// `entity_id` is the target (e.g. `light.living_room`); its domain (the part
    /// before the dot) selects the service namespace. `service` is the action
    /// (`turn_on`, `turn_off`, `set`, …). `data` is an optional JSON object of
    /// extra service fields (e.g. `{"brightness": 180}`) merged alongside
    /// `entity_id` in the POST body.
    ///
    /// In [`ActionMode::DryRun`] this issues NO request — it returns a preview of
    /// exactly the service call it would make. In [`ActionMode::Execute`] it POSTs
    /// `/api/services/<domain>/<service>` exactly once. Callers obtain `mode` from
    /// the foundation's `gate(confirm)`, so the shipped default (gate OFF) always
    /// previews — no device moves.
    pub async fn set_device(
        &self,
        entity_id: &str,
        service: &str,
        data: Option<&serde_json::Value>,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        let domain = entity_domain(entity_id)
            .ok_or_else(|| anyhow::anyhow!("'{entity_id}' is not a valid entity id (expected '<domain>.<name>')"))?;

        if mode == ActionMode::DryRun {
            info!(entity = entity_id, service, "smarthome: dry-run device control (no request issued)");
            let extra = preview_data(data);
            return Ok(format!(
                "[dry run] Would call {domain}.{service} on {entity_id}{extra}. \
                 Enable consequential actions and confirm to make the change."
            ));
        }

        let body = build_service_body(entity_id, data);
        let req = self
            .request(HttpMethod::Post, &format!("/api/services/{domain}/{service}"))
            .json_body(body);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "controlling that device")?;

        info!(entity = entity_id, service, "smarthome: device control executed");
        Ok(format!("Done — called {domain}.{service} on {entity_id}."))
    }
}

impl SmartHomeClient<ReqwestTransport> {
    /// Production constructor: resolve the base URL + long-lived token from the
    /// macOS Keychain via the foundation's allowlisted resolver, and wire the real
    /// reqwest transport. Returns the friendly, secret-free "smart home isn't
    /// configured" error when either value is missing — dume relays that to the
    /// user without ever surfacing a token.
    pub async fn connect() -> IntegrationResult<Self> {
        let base_url = resolve_secret(ACCOUNT_URL).await.ok_or_else(not_configured)?;
        let token = resolve_secret(ACCOUNT_TOKEN).await.ok_or_else(not_configured)?;
        Ok(Self::with_token(ReqwestTransport::new(), base_url, token))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// The friendly, secret-free "not configured" error both missing-secret paths
/// return — points the user at Settings and names what to add.
fn not_configured() -> anyhow::Error {
    anyhow::anyhow!("smart home isn't configured — add your Home Assistant URL + token in Settings")
}

/// The domain (the part before the first dot) of a Home Assistant entity id, or
/// `None` when the id is not in `<domain>.<name>` shape. Pure.
fn entity_domain(entity_id: &str) -> Option<&str> {
    let (domain, name) = entity_id.split_once('.')?;
    if domain.is_empty() || name.is_empty() {
        None
    } else {
        Some(domain)
    }
}

/// Build the POST body for a service call: always the `entity_id`, plus any
/// caller-supplied `data` object's keys merged in. A non-object `data` (or `None`)
/// contributes nothing beyond the entity id. Pure.
fn build_service_body(entity_id: &str, data: Option<&serde_json::Value>) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    map.insert("entity_id".to_string(), serde_json::Value::String(entity_id.to_string()));
    if let Some(serde_json::Value::Object(obj)) = data {
        for (k, v) in obj {
            // Never let a caller override the target entity_id via data.
            if k == "entity_id" {
                continue;
            }
            map.insert(k.clone(), v.clone());
        }
    }
    serde_json::Value::Object(map)
}

/// A short " with <data>" clause for a dry-run preview, or "" when there is no
/// extra data. Renders the JSON compactly; never echoes a token (data is
/// caller-supplied device fields, not auth). Pure.
fn preview_data(data: Option<&serde_json::Value>) -> String {
    match data {
        Some(serde_json::Value::Object(obj)) if !obj.is_empty() => {
            format!(" with {}", serde_json::Value::Object(obj.clone()))
        }
        _ => String::new(),
    }
}

/// Map a Home Assistant status to a friendly, secret-free error. 2xx is `Ok`.
/// 401 -> the token was rejected (reconnect in Settings); 404 -> no such entity/
/// service; 429/5xx -> transient. The provider body is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::Unauthorized => Err(anyhow::anyhow!(
            "Home Assistant rejected the token — check the URL and token in Settings"
        )),
        StatusOutcome::NotFound => Err(anyhow::anyhow!(
            "{what} failed — Home Assistant has no such device or service"
        )),
        StatusOutcome::RateLimited => {
            Err(anyhow::anyhow!("{what} was rate limited by Home Assistant; try again shortly"))
        }
        StatusOutcome::ServerError => {
            Err(anyhow::anyhow!("{what} failed on the hub's side; this is usually transient"))
        }
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned Home Assistant JSON (realistic API SHAPE, never fetched).
// No network, no real hub, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;
    use serde_json::json;

    /// A throwaway base URL + token used only to prove the request is shaped and
    /// authed. The token value is never asserted to APPEAR — only its ABSENCE.
    const FAKE_BASE: &str = "http://hub.local:8123";
    const FAKE_TOKEN: &str = "HA-FAKE-LONG-LIVED-TOKEN-NEVER-LEAK";

    fn client(mock: MockTransport) -> SmartHomeClient<MockTransport> {
        SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN)
    }

    // -- realistic canned payloads (hand-written from the HA API shape) -------

    fn states_json() -> &'static str {
        r#"[
          {"entity_id":"light.living_room","state":"on",
           "attributes":{"friendly_name":"Living Room","brightness":180},
           "last_changed":"2026-06-14T08:00:00+00:00"},
          {"entity_id":"lock.front_door","state":"locked",
           "attributes":{"friendly_name":"Front Door"}},
          {"entity_id":"climate.thermostat","state":"heat",
           "attributes":{"friendly_name":"Thermostat","temperature":69}}
        ]"#
    }

    fn one_state_json() -> &'static str {
        r#"{"entity_id":"light.living_room","state":"on",
            "attributes":{"friendly_name":"Living Room","brightness":180}}"#
    }

    // -- READ: parsing -------------------------------------------------------

    #[tokio::test]
    async fn list_devices_parses_and_summarizes() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/api/states", 200, states_json());
        let out = client(mock).list_devices().await.unwrap();
        assert!(out.contains("3 devices"), "got: {out}");
        assert!(out.contains("Living Room (on)"), "got: {out}");
        assert!(out.contains("Front Door (locked)"), "got: {out}");
    }

    #[tokio::test]
    async fn list_devices_empty_is_friendly() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/api/states", 200, "[]");
        let out = client(mock).list_devices().await.unwrap();
        assert!(out.contains("no devices"), "got: {out}");
    }

    #[tokio::test]
    async fn device_state_parses_friendly_name_and_state() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/api/states/light.living_room",
            200,
            one_state_json(),
        );
        let out = client(mock).device_state("light.living_room").await.unwrap();
        assert_eq!(out, "Living Room is on.");
    }

    #[tokio::test]
    async fn device_state_without_friendly_name_falls_back_to_entity_id() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/api/states/switch.fan",
            200,
            r#"{"entity_id":"switch.fan","state":"off","attributes":{}}"#,
        );
        let out = client(mock).device_state("switch.fan").await.unwrap();
        assert_eq!(out, "switch.fan is off.");
    }

    // -- READ: header SHAPE on the recorded request (never the token) --------

    #[tokio::test]
    async fn read_request_carries_auth_header_and_correct_url() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/api/states", 200, states_json());
        let c = SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN);
        c.list_devices().await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        assert_eq!(req.url, "http://hub.local:8123/api/states");
        assert!(req.has_header("authorization"), "auth header attached");
        assert!(req.body.is_none(), "no body on a read");
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_trimmed() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/api/states", 200, "[]");
        let c = SmartHomeClient::with_token(mock, "http://hub.local:8123/", FAKE_TOKEN);
        c.list_devices().await.unwrap();
        // No double slash before /api.
        assert_eq!(c.transport.last_request().url, "http://hub.local:8123/api/states");
    }

    // -- CONSEQUENTIAL: DryRun issues NO request -----------------------------

    #[tokio::test]
    async fn set_device_dry_run_issues_no_request() {
        let mock = MockTransport::new(); // no canned responses on purpose
        let c = SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN);
        let out = c
            .set_device("light.living_room", "turn_on", None, ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("light.turn_on"), "names the service: {out}");
        assert!(out.contains("light.living_room"), "names the target: {out}");
        assert_eq!(c.transport.requests().len(), 0, "DryRun must not touch the transport");
    }

    #[tokio::test]
    async fn set_device_dry_run_previews_optional_data() {
        let mock = MockTransport::new();
        let c = SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN);
        let out = c
            .set_device(
                "light.living_room",
                "turn_on",
                Some(&json!({"brightness": 180})),
                ActionMode::DryRun,
            )
            .await
            .unwrap();
        assert!(out.contains("brightness"), "preview includes data: {out}");
        assert!(out.contains("180"), "got: {out}");
        assert_eq!(c.transport.requests().len(), 0);
    }

    // -- CONSEQUENTIAL: Execute issues EXACTLY one correct request -----------

    #[tokio::test]
    async fn set_device_execute_posts_one_service_call() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/api/services/light/turn_off",
            200,
            "[]",
        );
        let c = SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN);
        let out = c
            .set_device("light.living_room", "turn_off", None, ActionMode::Execute)
            .await
            .unwrap();
        assert!(out.contains("Done"), "got: {out}");

        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 1, "exactly one service call");
        let req = &reqs[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(req.url, "http://hub.local:8123/api/services/light/turn_off");
        assert_eq!(req.body.as_ref().unwrap()["entity_id"], "light.living_room");
        assert!(req.has_header("authorization"));
    }

    #[tokio::test]
    async fn set_device_execute_merges_data_into_body() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/api/services/light/turn_on",
            200,
            "[]",
        );
        let c = SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN);
        c.set_device(
            "light.living_room",
            "turn_on",
            Some(&json!({"brightness": 200})),
            ActionMode::Execute,
        )
        .await
        .unwrap();
        let body = c.transport.last_request().body.unwrap();
        assert_eq!(body["entity_id"], "light.living_room");
        assert_eq!(body["brightness"], 200);
    }

    #[tokio::test]
    async fn set_device_rejects_bad_entity_id() {
        let mock = MockTransport::new();
        let c = SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN);
        // DryRun so we prove the validation fires before any request.
        let err = c
            .set_device("not_an_entity", "turn_on", None, ActionMode::DryRun)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("valid entity id"), "got: {err}");
        assert_eq!(c.transport.requests().len(), 0);
    }

    // -- error mapping -------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_maps_to_token_hint() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/api/states", 401, "{}");
        let err = client(mock).list_devices().await.unwrap_err().to_string();
        assert!(err.contains("rejected the token"), "401 -> token hint: {err}");
    }

    #[tokio::test]
    async fn not_found_maps_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Get,
            "/api/states/light.ghost",
            404,
            r#"{"message":"Entity not found."}"#,
        );
        let err = client(mock).device_state("light.ghost").await.unwrap_err().to_string();
        assert!(err.contains("no such device"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/api/states", 503, "down");
        let err = client(mock).list_devices().await.unwrap_err().to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- the TOKEN never leaks ----------------------------------------------

    #[tokio::test]
    async fn token_never_appears_in_any_produced_output() {
        // Debug of the client.
        let dbg = format!(
            "{:?}",
            SmartHomeClient::with_token(MockTransport::new(), FAKE_BASE, FAKE_TOKEN)
        );
        assert!(!dbg.contains(FAKE_TOKEN), "Debug leaked the token: {dbg}");
        assert!(dbg.contains("token_present"), "Debug should note presence");
        assert!(dbg.contains("hub.local"), "Debug may show the base URL (not a secret)");

        // Success + dry-run + error outcome strings.
        let mock = MockTransport::new()
            .on(HttpMethod::Get, "/api/states", 200, states_json())
            .on(HttpMethod::Post, "/api/services/light/turn_on", 200, "[]");
        let c = SmartHomeClient::with_token(mock, FAKE_BASE, FAKE_TOKEN);
        let ok1 = c.list_devices().await.unwrap();
        let ok2 = c
            .set_device("light.living_room", "turn_on", None, ActionMode::Execute)
            .await
            .unwrap();
        let dry = c
            .set_device("light.living_room", "turn_off", None, ActionMode::DryRun)
            .await
            .unwrap();
        for s in [&ok1, &ok2, &dry] {
            assert!(!s.contains(FAKE_TOKEN), "outcome leaked token: {s}");
        }

        let err_mock = MockTransport::new().on(HttpMethod::Get, "/api/states", 401, "{}");
        let err = SmartHomeClient::with_token(err_mock, FAKE_BASE, FAKE_TOKEN)
            .list_devices()
            .await
            .unwrap_err()
            .to_string();
        assert!(!err.contains(FAKE_TOKEN), "error leaked token: {err}");
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn entity_domain_extracts_or_rejects() {
        assert_eq!(entity_domain("light.living_room"), Some("light"));
        assert_eq!(entity_domain("climate.thermostat"), Some("climate"));
        assert_eq!(entity_domain("no_dot"), None);
        assert_eq!(entity_domain(".name"), None);
        assert_eq!(entity_domain("domain."), None);
        assert_eq!(entity_domain(""), None);
    }

    #[test]
    fn build_service_body_always_carries_entity_id_and_cannot_be_overridden() {
        let body = build_service_body("light.x", Some(&json!({"brightness": 10, "entity_id": "evil.y"})));
        assert_eq!(body["entity_id"], "light.x", "data must not override the target");
        assert_eq!(body["brightness"], 10);
        let bare = build_service_body("lock.front", None);
        assert_eq!(bare["entity_id"], "lock.front");
        assert_eq!(bare.as_object().unwrap().len(), 1);
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(201, "x").is_ok());
        assert!(map_status(401, "x").unwrap_err().to_string().contains("rejected the token"));
        assert!(map_status(404, "x").unwrap_err().to_string().contains("no such device"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(503, "x").unwrap_err().to_string().contains("transient"));
    }
}
