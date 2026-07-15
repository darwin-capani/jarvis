//! Have I Been Pwned (HIBP) breach-check client for agent "aegis" (Defense &
//! Privacy).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests), and holds the user's own HIBP API
//! key, which it attaches per request at the moment of the send.
//!
//! DEFENSIVE-ONLY, the USER'S OWN EMAIL. This client does exactly ONE thing: ask
//! HIBP whether a given email address appears in any known breach, so the user can
//! see their OWN exposure and rotate the affected passwords. It is NOT an offensive
//! tool — it does not scan hosts, crack credentials, enumerate other people, or
//! fetch leaked passwords; it reads a public breach catalog keyed by ONE address.
//! Authorized-use only: the email is the user's own (defaulted from their stored
//! address, or one they explicitly pass).
//!
//! KEY HANDLING — the security crux. The HIBP API authenticates with an API key in
//! the `hibp-api-key` HEADER. The key value is never logged, never stored on the
//! transport, never put in an error or a `Debug` field, and never in the URL — only
//! its presence (a bool) is ever recorded. The queried EMAIL rides only the URL
//! PATH at call time (HIBP's API shape); the response (the breach catalog) carries
//! NO password material, and the breach NAMES/dates/data-classes it returns are the
//! only thing surfaced.
//!
//! READ-ONLY by construction. There is NO consequential surface — this client
//! reports exposure, it never CHANGES a password, revokes a session, or contacts a
//! breached service (remediation is the user's own action). It holds no
//! [`super::ActionMode`] and never touches the foundation gate.
//!
//! One READ method, a plain GET that fetches and reports:
//!   * [`HibpClient::breaches_for`] — GET /breachedaccount/<email>?truncateResponse=false
//!     -> the list of breaches the address appears in (name, breach date, and the
//!     data classes exposed). A 404 is HIBP's "no breaches" answer — surfaced as a
//!     clean "no known breaches", never an error.

use serde::Deserialize;
use tracing::info;

use super::{
    resolve_secret, status_outcome, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    ReqwestTransport, StatusOutcome,
};

/// The Keychain account holding the user's own HIBP API key (from their
/// haveibeenpwned.com subscription). Pasted in Settings. Rides ONLY the
/// `hibp-api-key` request header at call time — never the URL, never a log.
pub const ACCOUNT_API_KEY: &str = "hibp_api_key";

/// Default HIBP API base URL (the v3 REST API). No trailing slash so
/// `{base}/breachedaccount/...` is clean.
pub const DEFAULT_BASE: &str = "https://haveibeenpwned.com/api/v3";

/// The header HIBP reads the API key from. Using the header (not the URL) keeps
/// the key out of every logged/recorded request line.
const API_KEY_HEADER: &str = "hibp-api-key";

/// HIBP requires a descriptive User-Agent on every call; it rejects requests
/// without one. This is NOT a secret — it identifies the calling app.
const USER_AGENT: &str = "darwin-aegis";

/// How many breach names to spell out before collapsing to "and N more".
const LIST_PREVIEW: usize = 6;

// ---------------------------------------------------------------------------
// Typed response shape — only the fields Aegis actually surfaces are decoded.
// `#[serde(default)]` keeps parsing resilient to the many extra keys HIBP
// returns (Description, LogoPath, IsVerified, … we don't read).
// ---------------------------------------------------------------------------

/// One breach record from `/breachedaccount/<email>`. Aegis reports the human
/// name, the breach date, and the kinds of data exposed.
#[derive(Debug, Clone, Deserialize, Default)]
struct Breach {
    /// The breach's human-facing title (e.g. "Adobe").
    #[serde(default, rename = "Name")]
    name: String,
    /// A friendlier display title when present (HIBP's `Title`); falls back to
    /// `Name`.
    #[serde(default, rename = "Title")]
    title: String,
    /// The date of the breach, ISO `YYYY-MM-DD`.
    #[serde(default, rename = "BreachDate")]
    breach_date: String,
    /// The classes of data exposed (e.g. "Email addresses", "Passwords").
    #[serde(default, rename = "DataClasses")]
    data_classes: Vec<String>,
}

impl Breach {
    /// The label to show for this breach: its title (or name), its date, and the
    /// data classes exposed. Never includes any password material — HIBP returns
    /// only the CLASSES of data, not the data itself.
    fn label(&self) -> String {
        let who = if !self.title.is_empty() {
            &self.title
        } else if !self.name.is_empty() {
            &self.name
        } else {
            "an unnamed breach"
        };
        let when = if self.breach_date.is_empty() {
            String::new()
        } else {
            format!(" ({})", self.breach_date)
        };
        let what = if self.data_classes.is_empty() {
            String::new()
        } else {
            format!(" — exposed: {}", self.data_classes.join(", "))
        };
        format!("{who}{when}{what}")
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// HIBP breach-check client bound to a transport, a base URL, and the user's API
/// key.
///
/// Construct with [`HibpClient::connect`] (resolves the key from the Keychain,
/// wires the real transport) or, in tests, [`HibpClient::with_key`] (an explicit
/// base URL + fake key + a `MockTransport`). The key is held only to compose the
/// per-request `hibp-api-key` header; it is never logged, never put in the URL,
/// and the `Debug` impl below redacts it.
///
/// READ-ONLY by construction: the only method is the breach READ. There is no
/// remediation/change method — not even a gated one — so this struct has no
/// `ActionMode` surface and never touches the foundation gate. Aegis reports
/// exposure; it never changes a password for you.
pub struct HibpClient<T: HttpTransport> {
    transport: T,
    /// HIBP base URL with any trailing slash trimmed.
    base: String,
    api_key: String,
}

/// Custom `Debug` that NEVER prints the API key — only that one is present, plus
/// the base URL (a public HIBP host, not a secret). So a `{:?}` of a client (in a
/// log line, a panic message, a test) can't leak the key.
impl<T: HttpTransport> std::fmt::Debug for HibpClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HibpClient")
            .field("base", &self.base)
            .field("api_key_present", &!self.api_key.is_empty())
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> HibpClient<T> {
    /// Build a client with an explicitly supplied base URL + API key. Used by tests
    /// (paired with `MockTransport`) and by any caller that has already resolved the
    /// secret. The key is consumed into the client and never logged; the base URL
    /// has its trailing slash trimmed so path joins are clean.
    pub fn with_key(transport: T, base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        Self {
            transport,
            base,
            api_key: api_key.into(),
        }
    }

    // -- READ (the only surface — no gate, no remediation) -------------------

    /// Check whether `email` appears in any known breach
    /// (`GET /breachedaccount/<email>?truncateResponse=false`). Read-only,
    /// defensive, the user's own address. Returns a human summary: a clean "no
    /// known breaches" when HIBP returns 404 (its "not found" answer), or a count
    /// plus the first few breaches (name, date, data classes) when it returns
    /// some. The API key rides the `hibp-api-key` HEADER, never the URL.
    pub async fn breaches_for(&self, email: &str) -> IntegrationResult<String> {
        let email = email.trim();
        if email.is_empty() {
            return Err(anyhow::anyhow!(
                "no email to check — pass your own address, or set it in Settings"
            ));
        }
        let path = format!(
            "/breachedaccount/{}?truncateResponse=false",
            encode(email)
        );
        let req = HttpRequest::new(HttpMethod::Get, format!("{}{path}", self.base))
            .header(API_KEY_HEADER, &self.api_key)
            .header("User-Agent", USER_AGENT);
        let resp = self.transport.send(req).await?;

        // HIBP returns 404 when the address is in NO breach — that is the GOOD,
        // clean answer, not an error. (The presence/absence is reported; the
        // queried address is the user's own.)
        if resp.status == 404 {
            info!(breaches = 0, "hibp: breach check (clean)");
            return Ok(format!(
                "Good news — {email} doesn't appear in any known breach on Have I Been Pwned."
            ));
        }
        map_status(resp.status, "checking your breach exposure")?;

        let breaches: Vec<Breach> = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("the breach check returned an unexpected response"))?;
        info!(breaches = breaches.len(), "hibp: breach check");

        if breaches.is_empty() {
            return Ok(format!(
                "Good news — {email} doesn't appear in any known breach on Have I Been Pwned."
            ));
        }
        let lines: Vec<String> = breaches.iter().take(LIST_PREVIEW).map(Breach::label).collect();
        let more = breaches.len().saturating_sub(lines.len());
        let mut out = format!(
            "{email} appears in {} known breach{}: {}",
            breaches.len(),
            if breaches.len() == 1 { "" } else { "es" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push_str(". Rotate any reused passwords on the affected services and turn on two-factor where you can.");
        Ok(out)
    }
}

impl HibpClient<ReqwestTransport> {
    /// Production constructor: resolve the HIBP API key from the macOS Keychain via
    /// the foundation's allowlisted resolver, and wire the real reqwest transport.
    /// Returns the friendly, secret-free "no api key configured" error when the key
    /// is missing — Aegis relays that to the user without ever surfacing the key.
    pub async fn connect() -> IntegrationResult<Self> {
        let api_key = resolve_secret(ACCOUNT_API_KEY).await.ok_or_else(not_configured)?;
        Ok(Self::with_key(ReqwestTransport::new(), DEFAULT_BASE, api_key))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// The friendly, secret-free "not configured" error the missing-key path returns —
/// points the user at Settings and names what to add. Defensive framing: this
/// checks the user's OWN email only.
fn not_configured() -> anyhow::Error {
    anyhow::anyhow!(
        "no HIBP API key configured — add your Have I Been Pwned API key in Settings (Aegis checks your OWN email's breach exposure; it never scans anyone else)"
    )
}

/// Minimal percent-encoding for the EMAIL that rides the URL path. Encodes
/// everything that is not an unreserved character so an `@`, `+`, or other byte in
/// an address cannot break the path or smuggle a query parameter. This only ever
/// touches the user's own address — NEVER the API key, which rides the header. Pure.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Map a HIBP HTTP status to a friendly, secret-free error. 2xx is `Ok`; 404 is
/// handled by the caller as the clean "no breaches" answer (so it never reaches
/// here). 401 means the API key is missing/invalid; 429 is HIBP's rate limit. The
/// provider body is never included. Pure.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    match status {
        401 => Err(anyhow::anyhow!(
            "{what} failed — your Have I Been Pwned API key was rejected; check it in Settings"
        )),
        _ => match status_outcome(status) {
            StatusOutcome::Success => Ok(()),
            StatusOutcome::Unauthorized => Err(anyhow::anyhow!(
                "{what} failed — your Have I Been Pwned API key was rejected; check it in Settings"
            )),
            StatusOutcome::RateLimited => {
                Err(anyhow::anyhow!("{what} was rate limited by Have I Been Pwned; try again shortly"))
            }
            StatusOutcome::ServerError => {
                Err(anyhow::anyhow!("{what} failed on Have I Been Pwned's side; this is usually transient"))
            }
            other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned HIBP JSON (realistic API SHAPE, never fetched). No network,
// no real Have I Been Pwned, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;

    /// A throwaway base + API key used only to prove the request is shaped and
    /// authed. The key VALUE is never asserted to APPEAR — only its ABSENCE, in the
    /// URL and in every produced string.
    const FAKE_BASE: &str = "https://haveibeenpwned.com/api/v3";
    const FAKE_KEY: &str = "HIBP-FAKE-API-KEY-NEVER-LEAK-abcdef1234";
    const EMAIL: &str = "user@example.com";

    fn client(mock: MockTransport) -> HibpClient<MockTransport> {
        HibpClient::with_key(mock, FAKE_BASE, FAKE_KEY)
    }

    // -- realistic canned payloads (hand-written from the HIBP v3 API shape) --

    /// A breached account: two breaches, each with a name/title, date, and data
    /// classes. HIBP returns only the CLASSES of data, never the data itself.
    fn breached_json() -> &'static str {
        r#"[
          {"Name":"Adobe","Title":"Adobe","BreachDate":"2013-10-04",
           "DataClasses":["Email addresses","Password hints","Passwords","Usernames"]},
          {"Name":"LinkedIn","Title":"LinkedIn","BreachDate":"2012-05-05",
           "DataClasses":["Email addresses","Passwords"]}
        ]"#
    }

    /// HIBP returns 404 (an empty body) when the address is in NO breach — the
    /// clean answer.
    fn clean_404_body() -> &'static str {
        ""
    }

    // -- READ: breached account parses + summarizes --------------------------

    #[tokio::test]
    async fn breaches_for_parses_and_summarizes() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 200, breached_json());
        let out = client(mock).breaches_for(EMAIL).await.unwrap();
        assert!(out.contains("2 known breaches"), "count missing: {out}");
        assert!(out.contains("Adobe (2013-10-04)"), "Adobe missing: {out}");
        assert!(out.contains("LinkedIn (2012-05-05)"), "LinkedIn missing: {out}");
        // The data CLASSES (not the data) are surfaced.
        assert!(out.contains("Passwords"), "data classes missing: {out}");
        // Defensive remediation guidance is offered (rotate passwords / 2FA).
        assert!(out.to_lowercase().contains("rotate"), "remediation hint missing: {out}");
    }

    // -- READ: a CLEAN address (HIBP 404) is the good answer, never an error --

    #[tokio::test]
    async fn clean_account_404_is_a_clean_no_breaches_answer() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 404, clean_404_body());
        let out = client(mock).breaches_for(EMAIL).await.unwrap();
        assert!(out.contains("doesn't appear in any known breach"), "got: {out}");
        assert!(out.contains(EMAIL), "should name the checked address: {out}");
    }

    /// An empty 200 array is also a clean answer (HIBP can answer 200 with []).
    #[tokio::test]
    async fn empty_200_array_is_also_clean() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 200, "[]");
        let out = client(mock).breaches_for(EMAIL).await.unwrap();
        assert!(out.contains("doesn't appear in any known breach"), "got: {out}");
    }

    // -- the API key rides the HEADER, never the URL -------------------------

    #[tokio::test]
    async fn request_carries_key_in_header_not_in_url() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 200, breached_json());
        let c = client(mock);
        c.breaches_for(EMAIL).await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Get);
        // The key is in the hibp-api-key header (presence asserted, not value).
        assert!(req.has_header("hibp-api-key"), "api key header attached");
        // HIBP requires a User-Agent.
        assert!(req.has_header("user-agent"), "user-agent attached");
        // The URL carries the (encoded) email in the PATH but NOT the key.
        assert!(req.url.contains("/breachedaccount/"), "path shape: {}", req.url);
        assert!(req.url.contains("user%40example.com"), "email encoded in path: {}", req.url);
        assert!(!req.url.to_lowercase().contains("hibp-api-key"), "key must NOT be in the URL: {}", req.url);
    }

    /// THE security pin: the API key value must never appear in any RECORDED URL,
    /// nor in any produced outcome/error/Debug string.
    #[tokio::test]
    async fn api_key_never_appears_in_a_logged_url_or_output() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 200, breached_json());
        let c = client(mock);
        let ok = c.breaches_for(EMAIL).await.unwrap();
        for req in c.transport.requests() {
            assert!(!req.url.contains(FAKE_KEY), "the API key leaked into a recorded URL: {}", req.url);
        }
        assert!(!ok.contains(FAKE_KEY), "outcome leaked the API key: {ok}");
        // Debug of the client redacts the key.
        let dbg = format!("{:?}", HibpClient::with_key(MockTransport::new(), FAKE_BASE, FAKE_KEY));
        assert!(!dbg.contains(FAKE_KEY), "Debug leaked the API key: {dbg}");
        assert!(dbg.contains("api_key_present"), "Debug should note presence");
        assert!(dbg.contains("haveibeenpwned.com"), "Debug may show the base URL (not a secret)");
        // An error path must not leak the key either.
        let err_mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 401, "{}");
        let err = client(err_mock).breaches_for(EMAIL).await.unwrap_err().to_string();
        assert!(!err.contains(FAKE_KEY), "error leaked the API key: {err}");
    }

    // -- error mapping (401 key rejected, 429 rate limited) ------------------

    #[tokio::test]
    async fn unauthorized_maps_to_key_hint() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 401, "{}");
        let err = client(mock).breaches_for(EMAIL).await.unwrap_err().to_string();
        assert!(err.contains("API key was rejected"), "401 -> key hint: {err}");
    }

    #[tokio::test]
    async fn rate_limited_is_friendly() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 429, "{}");
        let err = client(mock).breaches_for(EMAIL).await.unwrap_err().to_string();
        assert!(err.contains("rate limited"), "429 -> rate limited: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Get, "/breachedaccount/", 503, "down");
        let err = client(mock).breaches_for(EMAIL).await.unwrap_err().to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- empty email is refused without a network call -----------------------

    #[tokio::test]
    async fn empty_email_is_refused_without_a_request() {
        let mock = MockTransport::new(); // no canned response — must not be hit
        let c = client(mock);
        let err = c.breaches_for("   ").await.unwrap_err().to_string();
        assert!(err.contains("no email to check"), "got: {err}");
        assert!(c.transport.requests().is_empty(), "no request should have been sent");
    }

    // -- HARD SCOPE: defensive READ-ONLY, no offensive/remediation surface ----

    /// AEGIS's HIBP client is DEFENSIVE and READ-ONLY: the user's OWN email only,
    /// no offensive scanning, no remediation that changes anything. This is the
    /// structural guard: the whole hibp.rs source must name NO offensive or
    /// password-cracking or remediation-action surface — not even a gated one — and
    /// must not import the foundation's consequential gate or ActionMode.
    #[test]
    fn no_offensive_or_remediation_surface_exists() {
        let full = include_str!("hibp.rs");
        let prod = full
            .split("#[cfg(test)]")
            .next()
            .expect("module has a production section before the tests");

        // No offensive / password / remediation SURFACE may appear in production
        // code. These are method/endpoint-SHAPED tokens (a path fragment or `fn`
        // name), chosen so they match an actual surface — never the prose in the
        // doc comments (which legitimately discuss "Passwords" as a data class and
        // explain the absence of any password fetch).
        for forbidden in [
            "/pwnedpassword",     // HIBP's password-fetch endpoint — Aegis must not use it
            "fn crack",
            "fn scan_host",
            "fn portscan",
            "fn exploit",
            "fn change_password",
            "fn reset_password",
            "fn revoke",
        ] {
            assert!(
                !prod.contains(forbidden),
                "hibp.rs production code must contain NO offensive/remediation surface, found: {forbidden}"
            );
        }
        // No consequential gate / ActionMode — there is nothing to gate because the
        // client only READS the breach catalog.
        assert!(
            !prod.contains("ActionMode,")
                && !prod.contains("ActionMode}")
                && !prod.contains(": ActionMode")
                && !prod.contains("ActionMode::"),
            "hibp.rs must not import or use ActionMode — Aegis's breach check has no consequential surface"
        );
        assert!(
            !prod.contains("super::gate(") && !prod.contains("integrations::gate("),
            "hibp.rs must never call the consequential gate — there is no action to gate"
        );
        // The one public read method exists.
        assert!(prod.contains("pub async fn breaches_for"), "missing the breach READ method");
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn encode_escapes_the_email_safely() {
        assert_eq!(encode("user@example.com"), "user%40example.com");
        // A plus-addressed email cannot smuggle a query parameter.
        assert_eq!(encode("a+b@x.io"), "a%2Bb%40x.io");
        // Unreserved chars (letters, digits, -, _, ., ~) are kept.
        assert_eq!(encode("First.Last-99_x@d.com"), "First.Last-99_x%40d.com");
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(401, "x").unwrap_err().to_string().contains("API key was rejected"));
        assert!(map_status(403, "x").unwrap_err().to_string().contains("API key was rejected"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(503, "x").unwrap_err().to_string().contains("transient"));
    }

    #[test]
    fn not_configured_names_settings_and_is_defensive() {
        let e = not_configured().to_string();
        assert!(e.contains("Settings"), "points at Settings: {e}");
        assert!(e.to_lowercase().contains("own email"), "states the defensive scope: {e}");
    }
}
