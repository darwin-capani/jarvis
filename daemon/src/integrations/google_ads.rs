//! Google Ads client for agents "stark" (Business Intel) and "gecko"
//! (Markets + Capital) — the spend-and-campaigns surface, the second money-touching
//! integration this round alongside Meta Ads.
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]) and the provider-parameterized OAuth2 core
//! ([`crate::integrations::oauth2`]). It is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests) and gets its access token from a
//! shared Google-Ads [`ProviderAuth`] handle via [`ProviderAuth::bearer`]. The
//! Google Ads REST API needs MORE than a bearer on every call: a `developer-token`
//! header (a SECRET), the customer id in the resource path (`customers/<id>/...`),
//! and an OPTIONAL `login-customer-id` header (the manager account). Those ride in
//! a [`GoogleAdsCall`] the client owns, resolved once from the Keychain by
//! [`oauth2::google_ads_call`]. The client holds NO OAuth secret of its own — the
//! bearer is fetched per request from `auth.bearer()` at the moment of the send and
//! attached as the `Authorization` header. The bearer VALUE and the developer token
//! are NEVER logged, never stored on the transport, never in an error/Debug field —
//! only presence (a bool) is ever recorded.
//!
//! Two tiers of methods, mirroring the foundation's safety model:
//!   * READ (safe, never gated): [`GoogleAdsClient::report_campaigns`] runs a GAQL
//!     query via `GoogleAdsService.search` and returns a concise spend report
//!     (cost in major currency units, impressions, clicks per campaign);
//!     [`GoogleAdsClient::list_campaigns`] returns just names + status. Plain
//!     reporting reads, no side effects.
//!   * CONSEQUENTIAL (gated by [`ActionMode`] — these touch MONEY):
//!     [`GoogleAdsClient::pause_campaign`] / [`GoogleAdsClient::enable_campaign`]
//!     flip a campaign's status, and [`GoogleAdsClient::set_campaign_budget`]
//!     changes a campaign budget's amount. In [`ActionMode::DryRun`] each issues NO
//!     request and returns a clear PREVIEW of the exact change (which resource,
//!     old -> new); only in [`ActionMode::Execute`] does it issue exactly one
//!     `campaigns:mutate` / `campaignBudgets:mutate`. Call sites get `mode` from the
//!     foundation's `gate(confirm)`, so with `[integrations].allow_consequential`
//!     false (the shipped default) every mutation previews and changes nothing.
//!
//! Non-2xx responses map to friendly, secret-free errors via [`map_status`]
//! (401 -> reconnect; 403 -> the developer-token-not-approved / no-access hint;
//! 400 -> a GAQL/validation hint), never echoing the provider body (which can carry
//! account data or token-bearing fields).

use serde::Deserialize;
use tracing::info;

use super::oauth2::{google_ads_call, GoogleAdsCall, ProviderAuth, GOOGLE_ADS};
use super::{
    status_outcome, ActionMode, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    StatusOutcome,
};

/// The Google Ads REST API version this client is pinned to. Bumping it in one
/// place moves every endpoint together. (`v17` is a current GA version of the
/// Google Ads API at time of writing.)
pub const GOOGLE_ADS_VERSION: &str = "v17";

/// The Google Ads REST API base, with the version baked in.
pub const API_BASE: &str = "https://googleads.googleapis.com/v17";

/// How many campaigns one report/list read may ask for. The GAQL `LIMIT` is
/// clamped into this band so a bad `max` can never produce a pathological query.
const MIN_LIMIT: u32 = 1;
const MAX_LIMIT: u32 = 200;
const DEFAULT_LIMIT: u32 = 25;

/// Micros -> major-currency divisor. The Google Ads API reports money in "micros"
/// (millionths of the account currency unit): cost_micros / 1_000_000 = the amount
/// in the account's currency. Used for both the report math and the budget mutate.
const MICROS_PER_UNIT: i64 = 1_000_000;

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields the agents actually surface are
// decoded. `#[serde(default)]` keeps parsing resilient to the API's many extra
// keys and to objects that omit an optional field. The search response is
// `{"results":[{"campaign":{...},"metrics":{...}}, ...]}`.
// ---------------------------------------------------------------------------

/// The `GoogleAdsService.search` response envelope: a `results` array of rows, each
/// carrying the selected `campaign` and (for the report) `metrics` objects. Absent
/// when there are no rows, so it defaults to empty.
#[derive(Debug, Clone, Deserialize, Default)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<SearchRow>,
}

/// One `search` result row: the campaign fields plus, on the report query, its
/// metrics. `metrics` is absent on the list query, so it is optional.
#[derive(Debug, Clone, Deserialize, Default)]
struct SearchRow {
    #[serde(default)]
    campaign: Campaign,
    #[serde(default)]
    metrics: Option<Metrics>,
}

/// The selected `campaign` fields. The API returns numbers (id) as JSON strings
/// and the status as an enum string ("ENABLED"/"PAUSED"/"REMOVED").
#[derive(Debug, Clone, Deserialize, Default)]
struct Campaign {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    status: String,
}

/// The selected `metrics`. `cost_micros` is a string-encoded integer of micros;
/// impressions/clicks are string-encoded integers too. All default to "0". The
/// Google Ads REST API returns these fields in camelCase (`costMicros`), so the
/// whole struct renames camelCase -> the snake_case Rust fields.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Metrics {
    #[serde(default)]
    cost_micros: String,
    #[serde(default)]
    impressions: String,
    #[serde(default)]
    clicks: String,
}

/// One campaign's spend, as surfaced to a caller of
/// [`GoogleAdsClient::report_campaigns`].
#[derive(Debug, Clone, PartialEq)]
pub struct CampaignSpend {
    /// The campaign id (digits, as a string — the API returns it as a string).
    pub id: String,
    /// The campaign name.
    pub name: String,
    /// The campaign status ("ENABLED" / "PAUSED" / "REMOVED").
    pub status: String,
    /// Cost in micros (the raw API integer). Kept so a caller can do exact math;
    /// the human report converts it to major units.
    pub cost_micros: i64,
    /// Impressions over the queried window.
    pub impressions: i64,
    /// Clicks over the queried window.
    pub clicks: i64,
}

/// One campaign's identity, as surfaced to a caller of
/// [`GoogleAdsClient::list_campaigns`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CampaignSummary {
    pub id: String,
    pub name: String,
    pub status: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Google Ads client bound to a transport, a shared Google-Ads [`ProviderAuth`]
/// handle (for the bearer), and the non-OAuth [`GoogleAdsCall`] params (the
/// developer token + customer id + optional login-customer-id).
///
/// Construct with [`GoogleAdsClient::connect`] (production: a `ReqwestTransport`
/// API client paired with a connected `ProviderAuth` and the Keychain-resolved
/// call params) or, in tests, [`GoogleAdsClient::with_parts`] (a `MockTransport`
/// API client + a `ProviderAuth` wired over its own `MockTransport` + fabricated
/// call params). The client holds NO OAuth secret of its own — every request's
/// bearer comes from `auth.bearer()` at the moment of the send; the developer token
/// rides in the `GoogleAdsCall` and is attached only as the `developer-token`
/// header, never logged.
pub struct GoogleAdsClient<T: HttpTransport, A: HttpTransport> {
    transport: T,
    auth: ProviderAuth<A>,
    call: GoogleAdsCall,
}

/// `Debug` notes only that an auth handle is attached and reports the customer id
/// (an identifier, not a secret) — it never prints the bearer or the developer
/// token. (`GoogleAdsCall`'s own `Debug` already redacts the developer token, but
/// we keep this minimal so a `{:?}` of the client can't widen the surface.)
impl<T: HttpTransport, A: HttpTransport> std::fmt::Debug for GoogleAdsClient<T, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleAdsClient")
            .field("auth_attached", &true)
            .field("customer_id", &self.call.customer_id)
            .field("login_customer_id", &self.call.login_customer_id)
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport, A: HttpTransport> GoogleAdsClient<T, A> {
    /// Build a client over `transport`, taking ownership of a shared Google-Ads
    /// [`ProviderAuth`] handle and the resolved [`GoogleAdsCall`] params. Used by
    /// tests (mock transports, fabricated call params) and by
    /// [`GoogleAdsClient::connect`] internally. No secret is resolved here — the
    /// bearer is fetched per request from `auth`, and the developer token came in
    /// the `call`.
    pub fn with_parts(transport: T, auth: ProviderAuth<A>, call: GoogleAdsCall) -> Self {
        Self {
            transport,
            auth,
            call,
        }
    }

    /// The `customers/<id>/googleAds:search` endpoint for the configured customer.
    fn search_url(&self) -> String {
        format!(
            "{API_BASE}/customers/{}/googleAds:search",
            self.call.customer_id
        )
    }

    /// The `customers/<id>/campaigns:mutate` endpoint for the configured customer.
    fn campaign_mutate_url(&self) -> String {
        format!(
            "{API_BASE}/customers/{}/campaigns:mutate",
            self.call.customer_id
        )
    }

    /// The `customers/<id>/campaignBudgets:mutate` endpoint for the configured
    /// customer.
    fn budget_mutate_url(&self) -> String {
        format!(
            "{API_BASE}/customers/{}/campaignBudgets:mutate",
            self.call.customer_id
        )
    }

    /// Compose a request to `url` with the Bearer token attached HERE — fetched
    /// fresh from `auth` at the moment of the call — plus the `developer-token`
    /// header (a SECRET, from the `GoogleAdsCall`) and, when configured, the
    /// `login-customer-id` header. The bearer and the developer token never land on
    /// the transport or in a log.
    async fn request(&self, method: HttpMethod, url: &str) -> IntegrationResult<HttpRequest> {
        let token = self.auth.bearer().await?;
        let mut req = HttpRequest::new(method, url)
            .header("Authorization", format!("Bearer {token}"))
            .header("developer-token", self.call.developer_token.clone());
        if let Some(login) = &self.call.login_customer_id {
            req = req.header("login-customer-id", login.clone());
        }
        Ok(req)
    }

    // -- READ (safe — no gate) -----------------------------------------------

    /// Run a `GoogleAdsService.search` over `query` (GAQL) and return the raw
    /// search rows. Shared by [`report_campaigns`](Self::report_campaigns) and
    /// [`list_campaigns`](Self::list_campaigns); maps non-2xx to a friendly,
    /// secret-free error.
    async fn search(&self, query: String, what: &str) -> IntegrationResult<Vec<SearchRow>> {
        let body = serde_json::json!({ "query": query });
        let req = self
            .request(HttpMethod::Post, &self.search_url())
            .await?
            .header("Content-Type", "application/json")
            .json_body(body);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, what)?;
        let parsed: SearchResponse = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("{what} returned an unexpected response"))?;
        Ok(parsed.results)
    }

    /// Report the top campaigns by spend: runs a GAQL query selecting
    /// `campaign.{id,name,status}` + `metrics.{cost_micros,impressions,clicks}`
    /// ordered by cost, and returns one [`CampaignSpend`] per campaign with
    /// cost_micros converted in the caller-facing report. Read-only. `max` is
    /// clamped into [`MIN_LIMIT`]..=[`MAX_LIMIT`].
    pub async fn report_campaigns(&self, max: u32) -> IntegrationResult<Vec<CampaignSpend>> {
        let limit = clamp_limit(max);
        let query = format!(
            "SELECT campaign.id, campaign.name, campaign.status, \
             metrics.cost_micros, metrics.impressions, metrics.clicks \
             FROM campaign ORDER BY metrics.cost_micros DESC LIMIT {limit}"
        );
        let rows = self.search(query, "reading your Google Ads spend").await?;
        let spend: Vec<CampaignSpend> = rows
            .into_iter()
            .map(|row| {
                let m = row.metrics.unwrap_or_default();
                CampaignSpend {
                    id: row.campaign.id,
                    name: row.campaign.name,
                    status: row.campaign.status,
                    cost_micros: parse_i64(&m.cost_micros),
                    impressions: parse_i64(&m.impressions),
                    clicks: parse_i64(&m.clicks),
                }
            })
            .collect();
        info!(count = spend.len(), "google_ads: campaign spend report fetched");
        Ok(spend)
    }

    /// List campaigns (names + status only): a lighter GAQL query selecting just
    /// `campaign.{id,name,status}`, returned as [`CampaignSummary`] rows.
    /// Read-only. `max` is clamped into [`MIN_LIMIT`]..=[`MAX_LIMIT`].
    pub async fn list_campaigns(&self, max: u32) -> IntegrationResult<Vec<CampaignSummary>> {
        let limit = clamp_limit(max);
        let query = format!(
            "SELECT campaign.id, campaign.name, campaign.status \
             FROM campaign ORDER BY campaign.name LIMIT {limit}"
        );
        let rows = self.search(query, "listing your Google Ads campaigns").await?;
        let summaries: Vec<CampaignSummary> = rows
            .into_iter()
            .map(|row| CampaignSummary {
                id: row.campaign.id,
                name: row.campaign.name,
                status: row.campaign.status,
            })
            .collect();
        info!(count = summaries.len(), "google_ads: campaigns listed");
        Ok(summaries)
    }

    // -- CONSEQUENTIAL (gated by ActionMode — touches MONEY) -----------------

    /// PAUSE a campaign (set `campaign.status` = PAUSED). Consequential: it stops a
    /// running campaign's spend. In [`ActionMode::DryRun`] it issues NO request and
    /// returns a PREVIEW naming the campaign and the ENABLED -> PAUSED change; in
    /// [`ActionMode::Execute`] it issues exactly one `campaigns:mutate`. Callers get
    /// `mode` from `gate(confirm)`.
    pub async fn pause_campaign(
        &self,
        campaign_id: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        self.set_campaign_status(campaign_id, CampaignStatus::Paused, mode)
            .await
    }

    /// ENABLE a campaign (set `campaign.status` = ENABLED). Consequential: it lets a
    /// paused campaign spend again. DryRun previews the PAUSED -> ENABLED change and
    /// issues no request; Execute issues exactly one `campaigns:mutate`.
    pub async fn enable_campaign(
        &self,
        campaign_id: &str,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        self.set_campaign_status(campaign_id, CampaignStatus::Enabled, mode)
            .await
    }

    /// Shared body of pause/enable: build the campaign-status mutate, gate it, and
    /// (only on Execute) issue exactly one `campaigns:mutate`. The preview names the
    /// resource and the target status; no request is built in DryRun.
    async fn set_campaign_status(
        &self,
        campaign_id: &str,
        status: CampaignStatus,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        let resource = campaign_resource(&self.call.customer_id, campaign_id);
        if mode == ActionMode::DryRun {
            info!(
                dry_run = true,
                target = status.as_str(),
                "google_ads: campaign-status preview (no request issued)"
            );
            return Ok(format!(
                "[dry run] Would set campaign {campaign_id} to {} ({resource}). \
                 Enable consequential actions and confirm to apply.",
                status.as_str()
            ));
        }

        let body = campaign_status_mutate_body(&resource, status);
        let req = self
            .request(HttpMethod::Post, &self.campaign_mutate_url())
            .await?
            .header("Content-Type", "application/json")
            .json_body(body);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "updating your Google Ads campaign")?;
        info!(target = status.as_str(), "google_ads: campaign status updated");
        Ok(format!("Campaign {campaign_id} is now {}.", status.as_str()))
    }

    /// SET a campaign budget's daily amount (in micros). Consequential: it changes
    /// how much a campaign can spend. `budget` may be a full campaignBudget resource
    /// name (`customers/<cid>/campaignBudgets/<bid>`) or a bare budget id — both are
    /// normalized to the resource name. In [`ActionMode::DryRun`] it issues NO
    /// request and previews the new amount; in [`ActionMode::Execute`] it issues
    /// exactly one `campaignBudgets:mutate` with an `update_mask` of `amount_micros`.
    pub async fn set_campaign_budget(
        &self,
        budget: &str,
        amount_micros: i64,
        mode: ActionMode,
    ) -> IntegrationResult<String> {
        let resource = budget_resource(&self.call.customer_id, budget);
        let amount = format_micros(amount_micros);
        if mode == ActionMode::DryRun {
            info!(
                dry_run = true,
                "google_ads: budget preview (no request issued)"
            );
            return Ok(format!(
                "[dry run] Would set budget {resource} to {amount} per day \
                 ({amount_micros} micros). Enable consequential actions and confirm to apply."
            ));
        }

        let body = budget_mutate_body(&resource, amount_micros);
        let req = self
            .request(HttpMethod::Post, &self.budget_mutate_url())
            .await?
            .header("Content-Type", "application/json")
            .json_body(body);
        let resp = self.transport.send(req).await?;
        map_status(resp.status, "updating your Google Ads budget")?;
        info!("google_ads: campaign budget updated");
        Ok(format!("Budget {resource} is now {amount} per day."))
    }
}

impl GoogleAdsClient<super::ReqwestTransport, super::ReqwestTransport> {
    /// Production constructor: pair the real reqwest transport for the Google Ads
    /// API with a connected Google-Ads [`ProviderAuth`] handle (which resolves the
    /// OAuth trio from the Keychain and wires its own reqwest transport) and the
    /// non-OAuth [`GoogleAdsCall`] params resolved from the Keychain.
    ///
    /// Returns the OAuth core's friendly "Google Ads isn't connected" error when the
    /// OAuth app/refresh token is missing, or the "isn't fully configured — add the
    /// developer token + customer id in Settings" error when those non-OAuth pieces
    /// are missing.
    pub async fn connect() -> IntegrationResult<Self> {
        let auth = ProviderAuth::<super::ReqwestTransport>::connect(GOOGLE_ADS).await?;
        let call = google_ads_call().await?;
        Ok(Self::with_parts(super::ReqwestTransport::new(), auth, call))
    }
}

// ---------------------------------------------------------------------------
// Pure helpers — unit-testable, no transport, no secret.
// ---------------------------------------------------------------------------

/// A campaign status the mutate may set. Restricted to the two reversible states a
/// pause/enable touches; REMOVED (a destructive delete) is deliberately NOT
/// reachable from this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CampaignStatus {
    Enabled,
    Paused,
}

impl CampaignStatus {
    /// The Google Ads enum string the API expects in the mutate body.
    fn as_str(&self) -> &'static str {
        match self {
            CampaignStatus::Enabled => "ENABLED",
            CampaignStatus::Paused => "PAUSED",
        }
    }
}

/// Clamp a caller's requested `max` into the legal [`MIN_LIMIT`]..=[`MAX_LIMIT`]
/// band, treating 0 as "use the default".
fn clamp_limit(max: u32) -> u32 {
    if max == 0 {
        DEFAULT_LIMIT
    } else {
        max.clamp(MIN_LIMIT, MAX_LIMIT)
    }
}

/// Parse a string-encoded integer the API returns (cost_micros, impressions,
/// clicks) into an `i64`, defaulting to 0 on an absent/garbled value so a report
/// never fails on one odd row.
fn parse_i64(s: &str) -> i64 {
    s.trim().parse::<i64>().unwrap_or(0)
}

/// Convert micros to a major-currency-ish display string with two decimals, e.g.
/// 1_234_560 micros -> "1.23". The account currency symbol is unknown here (it
/// varies per account), so the caller-facing report adds context; this is the pure
/// numeric conversion. Handles negatives defensively (the API does not return
/// them, but the math stays correct).
fn format_micros(micros: i64) -> String {
    let negative = micros < 0;
    let abs = micros.unsigned_abs();
    let units = abs / MICROS_PER_UNIT as u64;
    // Two-decimal cents: round the remaining micros to the nearest 1/100 unit.
    let frac_micros = abs % MICROS_PER_UNIT as u64;
    let cents = (frac_micros + 5_000) / 10_000; // round to nearest cent
    // Rounding can carry into the next unit (e.g. .999999 -> 1.00).
    let (units, cents) = if cents >= 100 {
        (units + 1, 0)
    } else {
        (units, cents)
    };
    let sign = if negative { "-" } else { "" };
    format!("{sign}{units}.{cents:02}")
}

/// Build the campaign resource name `customers/<cid>/campaigns/<campaign_id>` the
/// mutate's `update` operation addresses.
fn campaign_resource(customer_id: &str, campaign_id: &str) -> String {
    format!("customers/{customer_id}/campaigns/{campaign_id}")
}

/// Normalize a budget reference (a full resource name OR a bare id) to the
/// `customers/<cid>/campaignBudgets/<bid>` resource name. If `budget` already looks
/// like a resource name (contains a slash), it is used verbatim; otherwise it is
/// treated as a bare id under the configured customer.
fn budget_resource(customer_id: &str, budget: &str) -> String {
    if budget.contains('/') {
        budget.to_string()
    } else {
        format!("customers/{customer_id}/campaignBudgets/{budget}")
    }
}

/// Build the `campaigns:mutate` body for a single status update: one `update`
/// operation carrying the campaign resource name + the new status, with an
/// `updateMask` naming exactly the `status` field (the API requires the mask to
/// list every field being changed). PURE — no I/O, no secret.
fn campaign_status_mutate_body(resource: &str, status: CampaignStatus) -> serde_json::Value {
    serde_json::json!({
        "operations": [{
            "update": {
                "resourceName": resource,
                "status": status.as_str()
            },
            "updateMask": "status"
        }]
    })
}

/// Build the `campaignBudgets:mutate` body for a single amount update: one `update`
/// operation carrying the budget resource name + the new `amountMicros` (sent as a
/// string, the API's encoding for the int64 micros field), with an `updateMask` of
/// `amount_micros`. PURE — no I/O, no secret.
fn budget_mutate_body(resource: &str, amount_micros: i64) -> serde_json::Value {
    serde_json::json!({
        "operations": [{
            "update": {
                "resourceName": resource,
                "amountMicros": amount_micros.to_string()
            },
            "updateMask": "amount_micros"
        }]
    })
}

// ---------------------------------------------------------------------------
// Status mapping (pure)
// ---------------------------------------------------------------------------

/// Map a Google Ads HTTP status to a friendly, secret-free error. 2xx is `Ok`. The
/// task's mappings: 401 -> reconnect (the OAuth token was rejected/expired);
/// 403 -> the Google-Ads "developer token not approved / no access to this account"
/// hint; 400 -> a GAQL/validation hint; plus the foundation's 404/429/5xx phrasing.
/// The provider body (which can echo account data) is never included.
fn map_status(status: u16, what: &str) -> IntegrationResult<()> {
    // 401 and 403 both classify as `Unauthorized` in the foundation, but Google Ads
    // means different things by them, and 400 is its validation code, so we branch
    // on the raw code first.
    match status {
        400 => {
            return Err(anyhow::anyhow!(
                "{what} failed — Google Ads rejected the request (a malformed query, id, or amount)"
            ))
        }
        401 => {
            return Err(anyhow::anyhow!(
                "{what} failed — Google Ads access expired; reconnect Google Ads in Settings"
            ))
        }
        403 => {
            return Err(anyhow::anyhow!(
                "{what} failed — Google Ads refused access; your developer token may not be approved or you may lack access to this account"
            ))
        }
        _ => {}
    }
    match status_outcome(status) {
        StatusOutcome::Success => Ok(()),
        StatusOutcome::NotFound => {
            Err(anyhow::anyhow!("{what} failed — that campaign or budget was not found"))
        }
        StatusOutcome::RateLimited => {
            Err(anyhow::anyhow!("{what} was rate limited by Google Ads; try again shortly"))
        }
        StatusOutcome::ServerError => {
            Err(anyhow::anyhow!("{what} failed on Google Ads' side; this is usually transient"))
        }
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned Google Ads JSON (realistic API SHAPE, never fetched), and
// the shared ProviderAuth handle is wired over its OWN MockTransport with a canned
// refresh response so `bearer()` works without a network or real token. The
// developer token + customer id are fabricated. No network, no real Google Ads
// round-trip, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::oauth2::RefreshTokenStore;
    use crate::integrations::testing::MockTransport;

    /// Fake credential values that, if leaked, would be unmistakable in an
    /// assertion. None of these is ever asserted to APPEAR — they are scanned for
    /// ABSENCE in produced output.
    const FAKE_CLIENT_ID: &str = "FAKE-GADS-CLIENT-ID-1234";
    const FAKE_CLIENT_SECRET: &str = "FAKE-GADS-CLIENT-SECRET-NEVER-LEAK";
    const FAKE_REFRESH: &str = "FAKE-GADS-REFRESH-TOKEN-NEVER-LEAK";
    /// The access token the canned refresh response mints. The client puts THIS in
    /// its Authorization header; tests assert it never lands in output, a URL, or a
    /// body.
    const FAKE_ACCESS: &str = "GADS-ACCESS-FAKE-NEVER-LEAK-IN-OUTPUT";
    /// The developer token — a SECRET. Rides only in the `developer-token` header;
    /// tests assert it is present as a header but never in output, a URL, or a body.
    const FAKE_DEV_TOKEN: &str = "FAKE-DEVELOPER-TOKEN-NEVER-LEAK";
    /// A fabricated customer id (an identifier, not a secret).
    const FAKE_CUSTOMER_ID: &str = "1234567890";
    /// A fabricated manager/login customer id.
    const FAKE_LOGIN_CUSTOMER_ID: &str = "9876543210";

    /// A no-op Keychain store so building a `ProviderAuth` never touches the real
    /// Keychain.
    fn noop_store() -> RefreshTokenStore {
        std::sync::Arc::new(|_t: &str| Ok(()))
    }

    /// Canned Google refresh response so `auth.bearer()` mints `FAKE_ACCESS`
    /// without a network call. Google's token endpoint returns an access token +
    /// expiry; we omit a rotated refresh token here.
    fn refresh_ok_json() -> String {
        format!(r#"{{"access_token":"{FAKE_ACCESS}","expires_in":3599,"token_type":"Bearer"}}"#)
    }

    /// Build a Google-Ads `ProviderAuth` handle over its own MockTransport that
    /// answers the token endpoint with a canned access token — the shared handle the
    /// client borrows for `bearer()`.
    fn test_auth() -> ProviderAuth<MockTransport> {
        let token_mock = MockTransport::new().on(
            HttpMethod::Post,
            GOOGLE_ADS.token_endpoint,
            200,
            refresh_ok_json(),
        );
        ProviderAuth::new(
            GOOGLE_ADS,
            token_mock,
            FAKE_CLIENT_ID,
            FAKE_CLIENT_SECRET,
            FAKE_REFRESH,
            noop_store(),
        )
    }

    /// The fabricated `GoogleAdsCall` params (with the optional login-customer-id
    /// set, so the header path is exercised).
    fn test_call() -> GoogleAdsCall {
        GoogleAdsCall {
            developer_token: FAKE_DEV_TOKEN.to_string(),
            customer_id: FAKE_CUSTOMER_ID.to_string(),
            login_customer_id: Some(FAKE_LOGIN_CUSTOMER_ID.to_string()),
        }
    }

    /// The fabricated call params WITHOUT a login-customer-id, to prove the header
    /// is omitted when absent.
    fn test_call_no_login() -> GoogleAdsCall {
        GoogleAdsCall {
            developer_token: FAKE_DEV_TOKEN.to_string(),
            customer_id: FAKE_CUSTOMER_ID.to_string(),
            login_customer_id: None,
        }
    }

    /// A Google Ads client whose API transport is `api_mock`, whose auth handle is a
    /// canned-refresh `ProviderAuth`, and whose call params include a login id.
    fn client(api_mock: MockTransport) -> GoogleAdsClient<MockTransport, MockTransport> {
        GoogleAdsClient::with_parts(api_mock, test_auth(), test_call())
    }

    // -- realistic canned payloads (hand-written from the Google Ads API shape) ---

    fn report_json() -> &'static str {
        // googleAds:search returns {"results":[{"campaign":{...},"metrics":{...}}]}.
        // The API encodes ids and metric integers as JSON strings.
        r#"{
            "results": [
                {
                    "campaign": {
                        "resourceName": "customers/1234567890/campaigns/111",
                        "id": "111",
                        "name": "Brand Search",
                        "status": "ENABLED"
                    },
                    "metrics": {
                        "costMicros": "12500000",
                        "impressions": "40000",
                        "clicks": "1200"
                    }
                },
                {
                    "campaign": {
                        "resourceName": "customers/1234567890/campaigns/222",
                        "id": "222",
                        "name": "Display Remarketing",
                        "status": "PAUSED"
                    },
                    "metrics": {
                        "costMicros": "3250000",
                        "impressions": "9000",
                        "clicks": "150"
                    }
                }
            ]
        }"#
    }

    fn list_json() -> &'static str {
        // The list query omits metrics.
        r#"{
            "results": [
                {"campaign": {"id": "111", "name": "Brand Search", "status": "ENABLED"}},
                {"campaign": {"id": "222", "name": "Display Remarketing", "status": "PAUSED"}}
            ]
        }"#
    }

    fn campaign_mutate_ok_json() -> &'static str {
        // campaigns:mutate 200 echoes the mutated resource name.
        r#"{"results":[{"resourceName":"customers/1234567890/campaigns/111"}]}"#
    }

    fn budget_mutate_ok_json() -> &'static str {
        r#"{"results":[{"resourceName":"customers/1234567890/campaignBudgets/555"}]}"#
    }

    // -- READ: report_campaigns parses + does the cost_micros math ------------

    #[tokio::test]
    async fn report_campaigns_parses_rows_and_cost_micros() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "googleAds:search",
            200,
            report_json(),
        );
        let spend = client(mock).report_campaigns(10).await.unwrap();
        assert_eq!(spend.len(), 2);
        assert_eq!(spend[0].id, "111");
        assert_eq!(spend[0].name, "Brand Search");
        assert_eq!(spend[0].status, "ENABLED");
        // The raw micros are preserved exactly.
        assert_eq!(spend[0].cost_micros, 12_500_000);
        assert_eq!(spend[0].impressions, 40_000);
        assert_eq!(spend[0].clicks, 1_200);
        assert_eq!(spend[1].cost_micros, 3_250_000);
        // cost_micros -> major units: 12_500_000 / 1e6 = 12.50.
        assert_eq!(format_micros(spend[0].cost_micros), "12.50");
        assert_eq!(format_micros(spend[1].cost_micros), "3.25");
    }

    #[tokio::test]
    async fn report_campaigns_sends_gaql_and_required_header_and_path() {
        let mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 200, report_json());
        let c = client(mock);
        c.report_campaigns(7).await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Post);
        // The customer id is in the resource PATH.
        assert!(
            req.url.contains(&format!("customers/{FAKE_CUSTOMER_ID}/googleAds:search")),
            "customer id must be in the path: {}",
            req.url
        );
        // The version is pinned.
        assert!(req.url.contains("/v17/"), "url: {}", req.url);
        // The auth + developer-token headers are present.
        assert!(req.has_header("authorization"), "auth attached (value not asserted)");
        assert!(req.has_header("developer-token"), "developer-token header required");
        // The optional login-customer-id header is present when configured.
        assert!(req.has_header("login-customer-id"), "login-customer-id header present");
        // The GAQL query is in the body and selects the expected fields, ordered by
        // cost, with the clamped LIMIT.
        let body = req.body.as_ref().expect("search has a JSON body");
        let query = body["query"].as_str().expect("query string");
        assert!(query.contains("metrics.cost_micros"), "query: {query}");
        assert!(query.contains("FROM campaign"), "query: {query}");
        assert!(query.contains("ORDER BY metrics.cost_micros DESC"), "query: {query}");
        assert!(query.contains("LIMIT 7"), "query: {query}");
    }

    #[tokio::test]
    async fn report_campaigns_clamps_limit() {
        let mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 200, report_json());
        let c = client(mock);
        // 0 -> default; over-max -> MAX_LIMIT.
        c.report_campaigns(0).await.unwrap();
        c.report_campaigns(99999).await.unwrap();
        let reqs = c.transport.requests();
        let q0 = reqs[0].body.as_ref().unwrap()["query"].as_str().unwrap();
        let q1 = reqs[1].body.as_ref().unwrap()["query"].as_str().unwrap();
        assert!(q0.contains(&format!("LIMIT {DEFAULT_LIMIT}")), "0 -> default: {q0}");
        assert!(q1.contains(&format!("LIMIT {MAX_LIMIT}")), "over-max -> clamp: {q1}");
    }

    // -- READ: list_campaigns -------------------------------------------------

    #[tokio::test]
    async fn list_campaigns_parses_names_and_status_only() {
        let mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 200, list_json());
        let c = client(mock);
        let campaigns = c.list_campaigns(50).await.unwrap();
        assert_eq!(campaigns.len(), 2);
        assert_eq!(campaigns[0].name, "Brand Search");
        assert_eq!(campaigns[0].status, "ENABLED");
        assert_eq!(campaigns[1].name, "Display Remarketing");
        // The list query omits metrics.
        let query = c.transport.last_request().body.as_ref().unwrap()["query"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!query.contains("metrics"), "list query must not select metrics: {query}");
        assert!(query.contains("campaign.name"), "query: {query}");
    }

    // -- CONSEQUENTIAL: DryRun issues NO request, previews --------------------

    #[tokio::test]
    async fn pause_campaign_dry_run_mutates_nothing_and_previews() {
        // Register the mutate endpoint so that, if DryRun mistakenly sent, it would
        // be recorded — proving by absence that nothing went out.
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "campaigns:mutate",
            200,
            campaign_mutate_ok_json(),
        );
        let c = client(mock);
        let out = c.pause_campaign("111", ActionMode::DryRun).await.unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        assert!(out.contains("PAUSED"), "preview names the target status: {out}");
        assert!(out.contains("111"), "preview names the campaign: {out}");
        // The CRUX: NO request was ever issued in DryRun (not even a bearer fetch on
        // the API transport).
        assert_eq!(
            c.transport.requests().len(),
            0,
            "DryRun must not issue any Google Ads request"
        );
    }

    #[tokio::test]
    async fn set_budget_dry_run_mutates_nothing_and_previews_amount() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "campaignBudgets:mutate",
            200,
            budget_mutate_ok_json(),
        );
        let c = client(mock);
        let out = c
            .set_campaign_budget("555", 50_000_000, ActionMode::DryRun)
            .await
            .unwrap();
        assert!(out.contains("[dry run]"), "got: {out}");
        // 50_000_000 micros -> 50.00 per day.
        assert!(out.contains("50.00"), "preview shows the new amount: {out}");
        assert!(out.contains("555"), "preview names the budget: {out}");
        assert_eq!(c.transport.requests().len(), 0, "DryRun must issue no request");
    }

    // -- CONSEQUENTIAL: Execute issues exactly one mutate ---------------------

    #[tokio::test]
    async fn pause_campaign_execute_issues_one_mutate_with_required_shape() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "campaigns:mutate",
            200,
            campaign_mutate_ok_json(),
        );
        let c = client(mock);
        let out = c.pause_campaign("111", ActionMode::Execute).await.unwrap();
        assert!(out.contains("PAUSED"), "got: {out}");

        // Exactly one mutate request.
        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 1, "exactly one mutate");
        let req = &reqs[0];
        assert_eq!(req.method, HttpMethod::Post);
        assert!(
            req.url.contains(&format!("customers/{FAKE_CUSTOMER_ID}/campaigns:mutate")),
            "url: {}",
            req.url
        );
        assert!(req.has_header("authorization"), "auth attached");
        assert!(req.has_header("developer-token"), "developer-token header required");

        // The mutate body carries one update operation: the campaign resource, the
        // new status, and an updateMask naming the status field.
        let body = req.body.as_ref().expect("mutate has a JSON body");
        let op = &body["operations"][0];
        assert_eq!(
            op["update"]["resourceName"],
            format!("customers/{FAKE_CUSTOMER_ID}/campaigns/111")
        );
        assert_eq!(op["update"]["status"], "PAUSED");
        assert_eq!(op["updateMask"], "status");
    }

    #[tokio::test]
    async fn enable_campaign_execute_sets_enabled() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "campaigns:mutate",
            200,
            campaign_mutate_ok_json(),
        );
        let c = client(mock);
        c.enable_campaign("222", ActionMode::Execute).await.unwrap();
        let body = c.transport.last_request().body.unwrap();
        assert_eq!(body["operations"][0]["update"]["status"], "ENABLED");
    }

    #[tokio::test]
    async fn set_budget_execute_issues_one_mutate_with_amount_and_mask() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "campaignBudgets:mutate",
            200,
            budget_mutate_ok_json(),
        );
        let c = client(mock);
        c.set_campaign_budget("555", 25_500_000, ActionMode::Execute)
            .await
            .unwrap();
        let reqs = c.transport.requests();
        assert_eq!(reqs.len(), 1, "exactly one mutate");
        let req = &reqs[0];
        assert!(
            req.url.contains(&format!("customers/{FAKE_CUSTOMER_ID}/campaignBudgets:mutate")),
            "url: {}",
            req.url
        );
        let op = &req.body.as_ref().unwrap()["operations"][0];
        assert_eq!(
            op["update"]["resourceName"],
            format!("customers/{FAKE_CUSTOMER_ID}/campaignBudgets/555")
        );
        // amountMicros is sent as a string (the API's int64 encoding).
        assert_eq!(op["update"]["amountMicros"], "25500000");
        assert_eq!(op["updateMask"], "amount_micros");
    }

    #[tokio::test]
    async fn set_budget_accepts_a_full_resource_name() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "campaignBudgets:mutate",
            200,
            budget_mutate_ok_json(),
        );
        let c = client(mock);
        let full = "customers/1234567890/campaignBudgets/777";
        c.set_campaign_budget(full, 1_000_000, ActionMode::Execute)
            .await
            .unwrap();
        let op = c.transport.last_request().body.unwrap()["operations"][0].clone();
        // A full resource name is used verbatim, not double-prefixed.
        assert_eq!(op["update"]["resourceName"], full);
    }

    // -- the login-customer-id header is omitted when absent ------------------

    #[tokio::test]
    async fn login_customer_id_header_omitted_when_not_configured() {
        let mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 200, list_json());
        let c = GoogleAdsClient::with_parts(mock, test_auth(), test_call_no_login());
        c.list_campaigns(5).await.unwrap();
        let req = c.transport.last_request();
        assert!(req.has_header("developer-token"), "developer-token still present");
        assert!(
            !req.has_header("login-customer-id"),
            "login-customer-id must be omitted when not configured"
        );
    }

    // -- error mapping --------------------------------------------------------

    #[tokio::test]
    async fn unauthorized_401_maps_to_reconnect() {
        let mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 401, "{}");
        let err = client(mock).report_campaigns(5).await.unwrap_err().to_string();
        assert!(err.contains("access expired"), "got: {err}");
        assert!(err.contains("reconnect Google Ads"), "got: {err}");
    }

    #[tokio::test]
    async fn forbidden_403_maps_to_developer_token_hint() {
        let mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 403, "{}");
        let err = client(mock).list_campaigns(5).await.unwrap_err().to_string();
        assert!(err.contains("developer token"), "got: {err}");
    }

    #[tokio::test]
    async fn bad_request_400_maps_to_validation_hint() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "campaigns:mutate",
            400,
            r#"{"error":{"code":400}}"#,
        );
        let err = client(mock)
            .pause_campaign("111", ActionMode::Execute)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("malformed"), "got: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 503, "down");
        let err = client(mock).report_campaigns(5).await.unwrap_err().to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- NOTHING secret is ever logged / leaked into output -------------------

    /// Neither the access token, refresh token, nor the developer token appears in a
    /// produced outcome string, in a mapped error, or in the client's Debug. The
    /// access token must live ONLY in the Authorization header and the developer
    /// token ONLY in the developer-token header on the wire — never in a URL or a
    /// body.
    #[tokio::test]
    async fn no_secret_ever_leaks() {
        // Debug of the client notes nothing secret (customer id is not a secret).
        let dbg = format!("{:?}", client(MockTransport::new()));
        assert!(!dbg.contains(FAKE_ACCESS), "Debug leaked the access token: {dbg}");
        assert!(!dbg.contains(FAKE_REFRESH), "Debug leaked the refresh token: {dbg}");
        assert!(!dbg.contains(FAKE_DEV_TOKEN), "Debug leaked the developer token: {dbg}");
        assert!(dbg.contains(FAKE_CUSTOMER_ID), "customer id is not secret");

        let mock = MockTransport::new()
            .on(HttpMethod::Post, "googleAds:search", 200, report_json())
            .on(HttpMethod::Post, "campaigns:mutate", 200, campaign_mutate_ok_json())
            .on(
                HttpMethod::Post,
                "campaignBudgets:mutate",
                200,
                budget_mutate_ok_json(),
            );
        let c = client(mock);

        let report = format!("{:?}", c.report_campaigns(5).await.unwrap());
        let paused = c.pause_campaign("111", ActionMode::Execute).await.unwrap();
        let budget = c
            .set_campaign_budget("555", 10_000_000, ActionMode::Execute)
            .await
            .unwrap();
        let preview = c.pause_campaign("111", ActionMode::DryRun).await.unwrap();

        for s in [&report, &paused, &budget, &preview] {
            assert!(!s.contains(FAKE_ACCESS), "output leaked the access token: {s}");
            assert!(!s.contains(FAKE_REFRESH), "output leaked the refresh token: {s}");
            assert!(!s.contains(FAKE_DEV_TOKEN), "output leaked the developer token: {s}");
            assert!(!s.contains(FAKE_CLIENT_SECRET), "output leaked the client secret: {s}");
        }

        // On the wire, the secrets ride ONLY in headers — never in a URL or a body.
        for req in c.transport.requests() {
            assert!(!req.url.contains(FAKE_ACCESS), "token must not be in a URL: {}", req.url);
            assert!(
                !req.url.contains(FAKE_DEV_TOKEN),
                "developer token must not be in a URL: {}",
                req.url
            );
            if let Some(body) = &req.body {
                let s = body.to_string();
                assert!(!s.contains(FAKE_ACCESS), "token must not be in a body");
                assert!(!s.contains(FAKE_DEV_TOKEN), "developer token must not be in a body");
            }
        }

        // An error path never leaks a secret either.
        let err_mock = MockTransport::new().on(HttpMethod::Post, "googleAds:search", 401, "{}");
        let err = client(err_mock).report_campaigns(5).await.unwrap_err().to_string();
        assert!(!err.contains(FAKE_ACCESS), "error leaked the access token: {err}");
        assert!(!err.contains(FAKE_DEV_TOKEN), "error leaked the developer token: {err}");
    }

    // -- pure helpers ---------------------------------------------------------

    #[test]
    fn format_micros_converts_and_rounds() {
        assert_eq!(format_micros(0), "0.00");
        assert_eq!(format_micros(1_000_000), "1.00");
        assert_eq!(format_micros(12_500_000), "12.50");
        assert_eq!(format_micros(3_250_000), "3.25");
        // Sub-cent rounding to the nearest cent.
        assert_eq!(format_micros(1_234_560), "1.23");
        assert_eq!(format_micros(1_235_000), "1.24"); // .235 -> .24 (round half up)
        // Carry into the next unit.
        assert_eq!(format_micros(1_999_999), "2.00");
        // Defensive negative handling.
        assert_eq!(format_micros(-2_500_000), "-2.50");
    }

    #[test]
    fn parse_i64_is_resilient() {
        assert_eq!(parse_i64("42"), 42);
        assert_eq!(parse_i64(" 7 "), 7);
        assert_eq!(parse_i64(""), 0);
        assert_eq!(parse_i64("not-a-number"), 0);
    }

    #[test]
    fn clamp_limit_band() {
        assert_eq!(clamp_limit(0), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(1), 1);
        assert_eq!(clamp_limit(10), 10);
        assert_eq!(clamp_limit(MAX_LIMIT + 1000), MAX_LIMIT);
    }

    #[test]
    fn resource_name_builders() {
        assert_eq!(
            campaign_resource("123", "999"),
            "customers/123/campaigns/999"
        );
        // A bare budget id is prefixed under the customer.
        assert_eq!(
            budget_resource("123", "555"),
            "customers/123/campaignBudgets/555"
        );
        // A full resource name is used verbatim.
        assert_eq!(
            budget_resource("123", "customers/999/campaignBudgets/777"),
            "customers/999/campaignBudgets/777"
        );
    }

    #[test]
    fn campaign_status_mutate_body_shape() {
        let body = campaign_status_mutate_body("customers/1/campaigns/9", CampaignStatus::Paused);
        let op = &body["operations"][0];
        assert_eq!(op["update"]["resourceName"], "customers/1/campaigns/9");
        assert_eq!(op["update"]["status"], "PAUSED");
        assert_eq!(op["updateMask"], "status");
    }

    #[test]
    fn budget_mutate_body_shape() {
        let body = budget_mutate_body("customers/1/campaignBudgets/5", 7_000_000);
        let op = &body["operations"][0];
        assert_eq!(op["update"]["resourceName"], "customers/1/campaignBudgets/5");
        assert_eq!(op["update"]["amountMicros"], "7000000");
        assert_eq!(op["updateMask"], "amount_micros");
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "x").is_ok());
        assert!(map_status(400, "x").unwrap_err().to_string().contains("malformed"));
        assert!(map_status(401, "x").unwrap_err().to_string().contains("access expired"));
        assert!(map_status(403, "x").unwrap_err().to_string().contains("developer token"));
        assert!(map_status(404, "x").unwrap_err().to_string().contains("not found"));
        assert!(map_status(429, "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(500, "x").unwrap_err().to_string().contains("transient"));
    }

    #[test]
    fn version_and_base_are_pinned() {
        assert_eq!(GOOGLE_ADS_VERSION, "v17");
        assert!(API_BASE.ends_with("/v17"));
        assert!(API_BASE.starts_with("https://googleads.googleapis.com/"));
    }
}
