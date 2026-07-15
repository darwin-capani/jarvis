//! Plaid personal-finance client for agent "midas" (Personal Treasury).
//!
//! Thin, typed wrapper over the shared integration foundation
//! ([`crate::integrations`]): it is generic over the foundation's
//! [`HttpTransport`] (so production wires [`ReqwestTransport`] and tests wire
//! `MockTransport` — zero network in tests), and holds the three Plaid secrets
//! (client_id, secret, access_token) it attaches per request at the moment of
//! the send. Plaid does NOT use an Authorization header: client_id + secret
//! authenticate the APP and access_token names the linked institution; all three
//! ride in the JSON request BODY. None of the three is ever logged, ever stored
//! on the transport, ever put in an error or a `Debug` field — only their
//! presence (a bool) is ever recorded.
//!
//! HARD RULE — MIDAS NEVER MOVES MONEY. This client is READ + INSIGHT ONLY.
//! There is deliberately NO transfer, payment, trade, or any money-moving method
//! on it — not even a gated one — so it holds NO [`super::ActionMode`] surface and
//! never touches the foundation's `gate()`. There is nothing to gate because no
//! money action exists. A test (`no_money_moving_method_exists`) pins that the
//! whole module names no transfer/payment/trade endpoint.
//!
//! HONESTY: Plaid needs TWO things from the user, both obtained outside DARWIN.
//! First, the user's own Plaid app credentials (client_id + secret), pasted in
//! Settings. Second, a per-institution `access_token`, which is minted by Plaid
//! LINK — a frontend/sandbox token-exchange step DARWIN does NOT perform. Until
//! all three are configured, every method returns the friendly secret-free
//! "no linked accounts — connect via Plaid in Settings" message; it never
//! pretends to read an account that was never linked.
//!
//! Three READ methods, every one a plain POST that fetches and reports:
//!   * [`PlaidClient::balances`] — POST /accounts/balance/get -> each account's
//!     name + available/current balance.
//!   * [`PlaidClient::transactions`] — POST /transactions/get over a date window
//!     -> date, name, amount, category per transaction.
//!   * [`PlaidClient::spending_summary`] — fetches the same transactions and
//!     aggregates spend by category (pure local fold; no extra call).
//!
//! Each method returns a concise human-facing `String` — what midas would say —
//! while parsing only the typed fields it needs. Non-2xx responses map to
//! friendly, secret-free errors via [`map_status`], with Plaid's own error_code
//! (ITEM_LOGIN_REQUIRED -> relink, INVALID_* -> creds) mapped first.

use serde::Deserialize;
use serde_json::json;
use tracing::info;

use super::{
    resolve_secret, status_outcome, HttpMethod, HttpRequest, HttpTransport, IntegrationResult,
    ReqwestTransport, StatusOutcome,
};

/// The Keychain account holding the user's Plaid client_id (from their own Plaid
/// developer app). Pasted in Settings. Rides only the request body at call time.
pub const ACCOUNT_CLIENT_ID: &str = "plaid_client_id";
/// The Keychain account holding the user's Plaid secret (from their own Plaid
/// developer app). Pasted in Settings. Rides only the request body at call time.
pub const ACCOUNT_SECRET: &str = "plaid_secret";
/// The Keychain account holding a linked institution's Plaid access_token, minted
/// by Plaid Link (a frontend/sandbox step DARWIN does not perform) and pasted in
/// Settings. Rides only the request body at call time.
pub const ACCOUNT_ACCESS_TOKEN: &str = "plaid_access_token";

/// Default Plaid base URL — the SANDBOX environment. The user's own app
/// credentials determine which environment is real; sandbox is the safe default
/// for a read-only insight agent and matches the honest "connect via Plaid"
/// onboarding. Production swaps the host with no code change (the secrets are the
/// same shape). No trailing slash so `{base}/accounts/...` is clean.
pub const DEFAULT_BASE: &str = "https://sandbox.plaid.com";

/// How many transactions to pull by default when the caller does not specify.
const DEFAULT_TXN_COUNT: u32 = 50;
/// How many transactions/categories to name in a summary before collapsing.
const LIST_PREVIEW: usize = 8;

// ---------------------------------------------------------------------------
// Typed response shapes — only the fields midas actually needs are decoded.
// `#[serde(default)]` keeps parsing resilient to the many extra keys Plaid
// returns (account masks, ids, iso_currency_code we don't read, …).
// ---------------------------------------------------------------------------

/// One account's balances, from `/accounts/balance/get`.
#[derive(Debug, Clone, Deserialize, Default)]
struct Balances {
    #[serde(default)]
    available: Option<f64>,
    #[serde(default)]
    current: Option<f64>,
}

/// One account object, as returned in the `accounts` array of
/// `/accounts/balance/get`.
#[derive(Debug, Clone, Deserialize)]
struct Account {
    #[serde(default)]
    name: String,
    #[serde(default)]
    balances: Balances,
}

impl Account {
    /// The label to show: the account name when present, else a generic noun.
    fn label(&self) -> &str {
        if self.name.is_empty() {
            "account"
        } else {
            &self.name
        }
    }
}

/// The `/accounts/balance/get` response shape midas reads.
#[derive(Debug, Clone, Deserialize, Default)]
struct BalanceGetResponse {
    #[serde(default)]
    accounts: Vec<Account>,
}

/// One transaction object, as returned in the `transactions` array of
/// `/transactions/get`.
#[derive(Debug, Clone, Deserialize)]
struct Transaction {
    #[serde(default)]
    date: String,
    #[serde(default)]
    name: String,
    /// Plaid amounts: a POSITIVE number is money OUT of the account (a debit /
    /// spend); a negative number is money IN (a credit). midas reports the value
    /// as Plaid gives it and labels spend in the summary fold.
    #[serde(default)]
    amount: f64,
    /// Plaid's hierarchical category, most-general first. Empty when Plaid has no
    /// category for the transaction.
    #[serde(default)]
    category: Vec<String>,
}

impl Transaction {
    /// The label to show for a transaction: its merchant/name, else a generic.
    fn label(&self) -> &str {
        if self.name.is_empty() {
            "transaction"
        } else {
            &self.name
        }
    }

    /// The primary (most-general) category, or "Uncategorized" when Plaid gave
    /// none — never invented, just a clear placeholder.
    fn primary_category(&self) -> &str {
        self.category
            .first()
            .map(String::as_str)
            .unwrap_or("Uncategorized")
    }
}

/// The `/transactions/get` response shape midas reads.
#[derive(Debug, Clone, Deserialize, Default)]
struct TransactionsGetResponse {
    #[serde(default)]
    transactions: Vec<Transaction>,
}

/// The slice of a Plaid ERROR body midas maps to friendly language. Plaid returns
/// an error_code (e.g. `ITEM_LOGIN_REQUIRED`, `INVALID_ACCESS_TOKEN`) on a non-2xx
/// — we read only the code to choose the message, never echoing the body.
#[derive(Debug, Clone, Deserialize, Default)]
struct PlaidError {
    #[serde(default)]
    error_code: String,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Plaid READ client bound to a transport, a base URL, and the three Plaid
/// secrets.
///
/// Construct with [`PlaidClient::connect`] (resolves the three secrets from the
/// Keychain, wires the real transport) or, in tests, [`PlaidClient::with_creds`]
/// (an explicit base URL + fake secrets + a `MockTransport`). The secrets are held
/// only to compose the per-request JSON body; none is ever logged and the `Debug`
/// impl below redacts all three.
///
/// READ-ONLY by construction: the only methods are the three reads. There is no
/// transfer/payment/trade method — not even a gated one — so this struct has no
/// `ActionMode` surface and never touches the foundation gate. Midas watches the
/// books; it never moves the money.
pub struct PlaidClient<T: HttpTransport> {
    transport: T,
    /// Plaid base URL with any trailing slash trimmed.
    base: String,
    client_id: String,
    secret: String,
    access_token: String,
}

/// Custom `Debug` that NEVER prints any of the three secrets — only that they are
/// present, plus the base URL (a public Plaid host, not a secret). So a `{:?}` of a
/// client (in a log line, a panic message, a test) can't leak the credentials.
impl<T: HttpTransport> std::fmt::Debug for PlaidClient<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlaidClient")
            .field("base", &self.base)
            .field("client_id_present", &!self.client_id.is_empty())
            .field("secret_present", &!self.secret.is_empty())
            .field("access_token_present", &!self.access_token.is_empty())
            .finish_non_exhaustive()
    }
}

impl<T: HttpTransport> PlaidClient<T> {
    /// Build a client with explicitly supplied base URL + the three secrets. Used
    /// by tests (paired with `MockTransport`) and by any caller that has already
    /// resolved the secrets. The secrets are consumed into the client and never
    /// logged; the base URL has its trailing slash trimmed so path joins are clean.
    pub fn with_creds(
        transport: T,
        base_url: impl Into<String>,
        client_id: impl Into<String>,
        secret: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        Self {
            transport,
            base,
            client_id: client_id.into(),
            secret: secret.into(),
            access_token: access_token.into(),
        }
    }

    /// Compose a POST request to a Plaid endpoint with the standard auth body.
    /// Plaid authenticates in the BODY (no Authorization header): client_id +
    /// secret + access_token go HERE — at the moment of the call — merged with any
    /// endpoint-specific fields. The secrets never land on the transport or in a log.
    fn post(&self, path: &str, extra: serde_json::Value) -> HttpRequest {
        let mut body = json!({
            "client_id": self.client_id,
            "secret": self.secret,
            "access_token": self.access_token,
        });
        if let serde_json::Value::Object(extra_obj) = extra {
            if let serde_json::Value::Object(map) = &mut body {
                for (k, v) in extra_obj {
                    map.insert(k, v);
                }
            }
        }
        HttpRequest::new(HttpMethod::Post, format!("{}{path}", self.base))
            .header("Content-Type", "application/json")
            .json_body(body)
    }

    // -- READ (the only surface — no gate, no money movement) ----------------

    /// Read each account's balances (`POST /accounts/balance/get`). Read-only.
    /// Returns a count plus the first few "<account> (available $X, current $Y)".
    pub async fn balances(&self) -> IntegrationResult<String> {
        let req = self.post("/accounts/balance/get", json!({}));
        let resp = self.transport.send(req).await?;
        map_status(resp.status, &resp.body, "reading your balances")?;

        let parsed: BalanceGetResponse = serde_json::from_str(&resp.body)
            .map_err(|_| anyhow::anyhow!("reading your balances returned an unexpected response"))?;
        info!(count = parsed.accounts.len(), "plaid: read balances");

        if parsed.accounts.is_empty() {
            return Ok(no_accounts_message());
        }
        let lines: Vec<String> = parsed
            .accounts
            .iter()
            .take(LIST_PREVIEW)
            .map(|a| format!("{} ({})", a.label(), balance_phrase(&a.balances)))
            .collect();
        let more = parsed.accounts.len().saturating_sub(lines.len());
        let mut out = format!(
            "You have {} account{}: {}",
            parsed.accounts.len(),
            if parsed.accounts.len() == 1 { "" } else { "s" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    /// Read recent transactions (`POST /transactions/get`) over `[since, today]`.
    /// Read-only. `since` is an ISO date (YYYY-MM-DD); `count` caps how many Plaid
    /// returns (defaulted/bounded). Returns the first few "<date> <name> $<amount>
    /// (<category>)".
    pub async fn transactions(&self, since: &str, count: Option<u32>) -> IntegrationResult<String> {
        let count = count.unwrap_or(DEFAULT_TXN_COUNT).clamp(1, 500);
        let today = "2099-12-31"; // an open upper bound; Plaid clamps to available data.
        let req = self.post(
            "/transactions/get",
            json!({
                "start_date": since,
                "end_date": today,
                "options": { "count": count },
            }),
        );
        let resp = self.transport.send(req).await?;
        map_status(resp.status, &resp.body, "reading your transactions")?;

        let parsed: TransactionsGetResponse = serde_json::from_str(&resp.body).map_err(|_| {
            anyhow::anyhow!("reading your transactions returned an unexpected response")
        })?;
        info!(count = parsed.transactions.len(), "plaid: read transactions");

        if parsed.transactions.is_empty() {
            return Ok(format!("No transactions on your linked accounts since {since}."));
        }
        let lines: Vec<String> = parsed
            .transactions
            .iter()
            .take(LIST_PREVIEW)
            .map(|t| {
                format!(
                    "{} {} {} ({})",
                    t.date,
                    t.label(),
                    amount_phrase(t.amount),
                    t.primary_category()
                )
            })
            .collect();
        let more = parsed.transactions.len().saturating_sub(lines.len());
        let mut out = format!(
            "{} transaction{} since {since}: {}",
            parsed.transactions.len(),
            if parsed.transactions.len() == 1 { "" } else { "s" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }

    /// Summarize SPENDING by category (`POST /transactions/get`, then a pure local
    /// fold). Read-only. Aggregates the OUTGOING amount (Plaid positive = spend)
    /// per primary category and reports the categories by descending spend. Money
    /// IN (credits, Plaid negative) is excluded from the spend total.
    pub async fn spending_summary(&self, since: &str, count: Option<u32>) -> IntegrationResult<String> {
        let count = count.unwrap_or(DEFAULT_TXN_COUNT).clamp(1, 500);
        let today = "2099-12-31";
        let req = self.post(
            "/transactions/get",
            json!({
                "start_date": since,
                "end_date": today,
                "options": { "count": count },
            }),
        );
        let resp = self.transport.send(req).await?;
        map_status(resp.status, &resp.body, "summarizing your spending")?;

        let parsed: TransactionsGetResponse = serde_json::from_str(&resp.body).map_err(|_| {
            anyhow::anyhow!("summarizing your spending returned an unexpected response")
        })?;
        info!(count = parsed.transactions.len(), "plaid: summarized spending");

        let summary = aggregate_spending(&parsed.transactions);
        if summary.is_empty() {
            return Ok(format!("No spending to summarize on your linked accounts since {since}."));
        }
        let total: f64 = summary.iter().map(|(_, amt)| amt).sum();
        let lines: Vec<String> = summary
            .iter()
            .take(LIST_PREVIEW)
            .map(|(cat, amt)| format!("{cat} ${amt:.2}"))
            .collect();
        let more = summary.len().saturating_sub(lines.len());
        let mut out = format!(
            "Since {since} you spent ${total:.2} across {} categor{}: {}",
            summary.len(),
            if summary.len() == 1 { "y" } else { "ies" },
            lines.join("; ")
        );
        if more > 0 {
            out.push_str(&format!("; and {more} more"));
        }
        out.push('.');
        Ok(out)
    }
}

impl PlaidClient<ReqwestTransport> {
    /// Production constructor: resolve client_id + secret + access_token from the
    /// macOS Keychain via the foundation's allowlisted resolver, and wire the real
    /// reqwest transport. Returns the friendly, secret-free "no linked accounts"
    /// error when ANY of the three is missing — midas relays that to the user
    /// without ever surfacing a secret. The access_token in particular is minted by
    /// Plaid LINK (a frontend step DARWIN does not perform), so its absence is the
    /// common "not linked yet" case.
    pub async fn connect() -> IntegrationResult<Self> {
        let client_id = resolve_secret(ACCOUNT_CLIENT_ID).await.ok_or_else(not_configured)?;
        let secret = resolve_secret(ACCOUNT_SECRET).await.ok_or_else(not_configured)?;
        let access_token = resolve_secret(ACCOUNT_ACCESS_TOKEN).await.ok_or_else(not_configured)?;
        Ok(Self::with_creds(
            ReqwestTransport::new(),
            DEFAULT_BASE,
            client_id,
            secret,
            access_token,
        ))
    }
}

// ---------------------------------------------------------------------------
// Helpers (pure — unit-testable without a transport)
// ---------------------------------------------------------------------------

/// The friendly, secret-free "not configured / not linked" error every missing-
/// secret path returns — points the user at Settings + Plaid Link, names that
/// DARWIN reads only, and never surfaces a token.
fn not_configured() -> anyhow::Error {
    anyhow::anyhow!(
        "no linked accounts — connect via Plaid in Settings (your Plaid client id and secret, plus a linked-institution access token from Plaid Link)"
    )
}

/// The friendly "no accounts came back" message a successful-but-empty read
/// returns — distinct from the not-configured error: the creds worked, the read
/// just returned nothing.
fn no_accounts_message() -> String {
    "No linked accounts came back from Plaid — link an institution via Plaid in Settings.".to_string()
}

/// Render one account's balances as a spoken phrase. Uses whichever of
/// available/current Plaid returned; "balance unavailable" when neither is set.
/// Pure.
fn balance_phrase(b: &Balances) -> String {
    match (b.available, b.current) {
        (Some(a), Some(c)) => format!("available ${a:.2}, current ${c:.2}"),
        (Some(a), None) => format!("available ${a:.2}"),
        (None, Some(c)) => format!("current ${c:.2}"),
        (None, None) => "balance unavailable".to_string(),
    }
}

/// Render a transaction amount as a spoken phrase. Plaid positive = money OUT
/// (a spend); negative = money IN (a credit). Pure.
fn amount_phrase(amount: f64) -> String {
    if amount < 0.0 {
        format!("+${:.2}", -amount)
    } else {
        format!("-${amount:.2}")
    }
}

/// Aggregate OUTGOING spend (Plaid positive amounts) by primary category,
/// returning (category, total) sorted by descending total. Money IN (negative
/// amounts) is excluded. Pure — the whole fold is unit-tested without a network.
fn aggregate_spending(transactions: &[Transaction]) -> Vec<(String, f64)> {
    use std::collections::HashMap;
    let mut by_cat: HashMap<String, f64> = HashMap::new();
    for t in transactions {
        // Only outgoing money counts as spend.
        if t.amount > 0.0 {
            *by_cat.entry(t.primary_category().to_string()).or_insert(0.0) += t.amount;
        }
    }
    let mut out: Vec<(String, f64)> = by_cat.into_iter().collect();
    // Descending by amount; ties broken by category name for stable output.
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    out
}

/// Map a Plaid response to a friendly, secret-free error. 2xx is `Ok`. A non-2xx
/// is mapped FIRST by Plaid's own error_code (read from the body) so the user gets
/// the actionable cause — ITEM_LOGIN_REQUIRED means the linked institution needs
/// re-linking; an INVALID_* code means the app credentials or access token are
/// wrong — then falls back to the coarse HTTP-status mapping. The provider body is
/// NEVER included in the message (only its parsed error_code routes the wording).
fn map_status(status: u16, body: &str, what: &str) -> IntegrationResult<()> {
    if status_outcome(status) == StatusOutcome::Success {
        return Ok(());
    }
    // Plaid returns a structured error_code on failures — prefer it for wording.
    if let Ok(err) = serde_json::from_str::<PlaidError>(body) {
        match err.error_code.as_str() {
            "ITEM_LOGIN_REQUIRED" => {
                return Err(anyhow::anyhow!(
                    "{what} failed — your bank needs you to re-link it; reconnect the institution via Plaid in Settings"
                ));
            }
            code if code.starts_with("INVALID_") => {
                return Err(anyhow::anyhow!(
                    "{what} failed — your Plaid credentials were rejected; check your client id, secret, and linked access token in Settings"
                ));
            }
            _ => {}
        }
    }
    // Fall back to the coarse HTTP-status classification.
    match status_outcome(status) {
        StatusOutcome::Unauthorized => Err(anyhow::anyhow!(
            "{what} failed — your Plaid credentials were rejected; check them in Settings"
        )),
        StatusOutcome::RateLimited => {
            Err(anyhow::anyhow!("{what} was rate limited by Plaid; try again shortly"))
        }
        StatusOutcome::ServerError => {
            Err(anyhow::anyhow!("{what} failed on Plaid's side; this is usually transient"))
        }
        other => Err(anyhow::anyhow!("{what} {}", other.friendly())),
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic: every case drives the foundation's MockTransport with
// hand-written canned Plaid JSON (realistic API SHAPE, never fetched). No
// network, no real Plaid, no Keychain.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::testing::MockTransport;

    /// Throwaway base + secrets used only to prove requests are shaped and authed.
    /// None of the secret values is asserted to APPEAR — only their ABSENCE.
    const FAKE_BASE: &str = "https://sandbox.plaid.com";
    const FAKE_CLIENT_ID: &str = "PLAID-FAKE-CLIENT-ID-NEVER-LEAK";
    const FAKE_SECRET: &str = "PLAID-FAKE-SECRET-NEVER-LEAK";
    const FAKE_ACCESS_TOKEN: &str = "access-sandbox-FAKE-TOKEN-NEVER-LEAK";

    fn client(mock: MockTransport) -> PlaidClient<MockTransport> {
        PlaidClient::with_creds(mock, FAKE_BASE, FAKE_CLIENT_ID, FAKE_SECRET, FAKE_ACCESS_TOKEN)
    }

    // -- realistic canned payloads (hand-written from the Plaid API shape) -----

    fn balance_json() -> &'static str {
        r#"{
          "accounts": [
            {"account_id":"a1","name":"Plaid Checking","mask":"0000",
             "balances":{"available":1203.45,"current":1250.00,"iso_currency_code":"USD"}},
            {"account_id":"a2","name":"Plaid Saving","mask":"1111",
             "balances":{"available":8500.00,"current":8500.00,"iso_currency_code":"USD"}}
          ],
          "item":{"item_id":"item-123"},
          "request_id":"req-1"
        }"#
    }

    fn transactions_json() -> &'static str {
        r#"{
          "accounts": [{"account_id":"a1","name":"Plaid Checking"}],
          "transactions": [
            {"transaction_id":"t1","date":"2026-06-10","name":"Starbucks","amount":5.75,
             "category":["Food and Drink","Coffee Shop"],"account_id":"a1"},
            {"transaction_id":"t2","date":"2026-06-09","name":"Whole Foods","amount":82.10,
             "category":["Food and Drink","Groceries"],"account_id":"a1"},
            {"transaction_id":"t3","date":"2026-06-08","name":"Payroll","amount":-2500.00,
             "category":["Transfer","Payroll"],"account_id":"a1"},
            {"transaction_id":"t4","date":"2026-06-07","name":"Uber","amount":18.40,
             "category":["Travel","Ride Share"],"account_id":"a1"}
          ],
          "total_transactions": 4,
          "request_id":"req-2"
        }"#
    }

    // -- READ: balances parsing ----------------------------------------------

    #[tokio::test]
    async fn balances_parses_and_summarizes() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/accounts/balance/get", 200, balance_json());
        let out = client(mock).balances().await.unwrap();
        assert!(out.contains("2 accounts"), "got: {out}");
        assert!(out.contains("Plaid Checking (available $1203.45, current $1250.00)"), "got: {out}");
        assert!(out.contains("Plaid Saving"), "got: {out}");
    }

    #[tokio::test]
    async fn balances_empty_is_friendly_not_configured_style() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/accounts/balance/get",
            200,
            r#"{"accounts":[],"request_id":"r"}"#,
        );
        let out = client(mock).balances().await.unwrap();
        assert!(out.contains("No linked accounts"), "got: {out}");
    }

    // -- READ: transactions parsing ------------------------------------------

    #[tokio::test]
    async fn transactions_parses_and_summarizes() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/transactions/get", 200, transactions_json());
        let out = client(mock).transactions("2026-06-01", None).await.unwrap();
        assert!(out.contains("4 transactions since 2026-06-01"), "got: {out}");
        assert!(out.contains("2026-06-10 Starbucks -$5.75 (Food and Drink)"), "got: {out}");
        // A credit (negative Plaid amount) reads as money IN.
        assert!(out.contains("Payroll +$2500.00"), "got: {out}");
    }

    #[tokio::test]
    async fn transactions_empty_is_friendly() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/transactions/get",
            200,
            r#"{"transactions":[],"request_id":"r"}"#,
        );
        let out = client(mock).transactions("2026-06-01", None).await.unwrap();
        assert!(out.contains("No transactions"), "got: {out}");
    }

    // -- READ: spending summary aggregation ----------------------------------

    #[tokio::test]
    async fn spending_summary_aggregates_by_category_excluding_credits() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/transactions/get", 200, transactions_json());
        let out = client(mock).spending_summary("2026-06-01", None).await.unwrap();
        // Spend = 5.75 + 82.10 + 18.40 = 106.25 (the -2500 payroll credit is excluded).
        assert!(out.contains("$106.25"), "total spend wrong: {out}");
        // Food and Drink (5.75 + 82.10 = 87.85) leads Travel (18.40).
        assert!(out.contains("Food and Drink $87.85"), "got: {out}");
        assert!(out.contains("Travel $18.40"), "got: {out}");
        // The payroll credit category must NOT appear as spend.
        assert!(!out.contains("Transfer"), "credits must not count as spend: {out}");
    }

    #[tokio::test]
    async fn spending_summary_empty_when_only_credits() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/transactions/get",
            200,
            r#"{"transactions":[
              {"transaction_id":"t","date":"2026-06-08","name":"Payroll","amount":-2500.00,
               "category":["Transfer","Payroll"]}
            ],"request_id":"r"}"#,
        );
        let out = client(mock).spending_summary("2026-06-01", None).await.unwrap();
        assert!(out.contains("No spending to summarize"), "got: {out}");
    }

    // -- request SHAPE: auth in the BODY, never a header, never asserting value

    #[tokio::test]
    async fn read_request_carries_creds_in_body_and_correct_url() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/accounts/balance/get", 200, balance_json());
        let c = client(mock);
        c.balances().await.unwrap();
        let req = c.transport.last_request();
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(req.url, "https://sandbox.plaid.com/accounts/balance/get");
        let body = req.body.as_ref().expect("balance read has a JSON body");
        // The three auth fields are present in the body (shape asserted, not value).
        assert!(body.get("client_id").is_some(), "client_id in body");
        assert!(body.get("secret").is_some(), "secret in body");
        assert!(body.get("access_token").is_some(), "access_token in body");
        // Plaid does NOT use an Authorization header.
        assert!(!req.has_header("authorization"), "Plaid authenticates in the body, not a header");
    }

    #[tokio::test]
    async fn transactions_request_includes_date_window_and_count() {
        let mock =
            MockTransport::new().on(HttpMethod::Post, "/transactions/get", 200, transactions_json());
        let c = client(mock);
        c.transactions("2026-05-01", Some(25)).await.unwrap();
        let body = c.transport.last_request().body.unwrap();
        assert_eq!(body["start_date"], "2026-05-01");
        assert_eq!(body["options"]["count"], 25);
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_trimmed() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/accounts/balance/get",
            200,
            r#"{"accounts":[]}"#,
        );
        let c = PlaidClient::with_creds(
            mock,
            "https://sandbox.plaid.com/",
            FAKE_CLIENT_ID,
            FAKE_SECRET,
            FAKE_ACCESS_TOKEN,
        );
        c.balances().await.unwrap();
        assert_eq!(
            c.transport.last_request().url,
            "https://sandbox.plaid.com/accounts/balance/get"
        );
    }

    // -- error mapping: Plaid error_code first, then HTTP status --------------

    #[tokio::test]
    async fn item_login_required_maps_to_relink() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/accounts/balance/get",
            400,
            r#"{"error_type":"ITEM_ERROR","error_code":"ITEM_LOGIN_REQUIRED","request_id":"r"}"#,
        );
        let err = client(mock).balances().await.unwrap_err().to_string();
        assert!(err.contains("re-link"), "ITEM_LOGIN_REQUIRED -> relink: {err}");
    }

    #[tokio::test]
    async fn invalid_access_token_maps_to_creds_hint() {
        let mock = MockTransport::new().on(
            HttpMethod::Post,
            "/transactions/get",
            400,
            r#"{"error_type":"INVALID_INPUT","error_code":"INVALID_ACCESS_TOKEN","request_id":"r"}"#,
        );
        let err = client(mock).transactions("2026-06-01", None).await.unwrap_err().to_string();
        assert!(err.contains("credentials were rejected"), "INVALID_* -> creds hint: {err}");
    }

    #[tokio::test]
    async fn unauthorized_without_error_code_maps_to_creds() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/accounts/balance/get", 401, "{}");
        let err = client(mock).balances().await.unwrap_err().to_string();
        assert!(err.contains("credentials were rejected"), "401 -> creds hint: {err}");
    }

    #[tokio::test]
    async fn server_error_maps_transient() {
        let mock = MockTransport::new().on(HttpMethod::Post, "/accounts/balance/get", 503, "down");
        let err = client(mock).balances().await.unwrap_err().to_string();
        assert!(err.contains("transient"), "got: {err}");
    }

    // -- the SECRETS never leak ----------------------------------------------

    #[tokio::test]
    async fn secrets_never_appear_in_any_produced_output() {
        // Debug of the client.
        let dbg = format!(
            "{:?}",
            PlaidClient::with_creds(
                MockTransport::new(),
                FAKE_BASE,
                FAKE_CLIENT_ID,
                FAKE_SECRET,
                FAKE_ACCESS_TOKEN
            )
        );
        for secret in [FAKE_CLIENT_ID, FAKE_SECRET, FAKE_ACCESS_TOKEN] {
            assert!(!dbg.contains(secret), "Debug leaked a secret: {dbg}");
        }
        assert!(dbg.contains("client_id_present"), "Debug should note presence");
        assert!(dbg.contains("sandbox.plaid.com"), "Debug may show the base URL (not a secret)");

        // Success outcome strings + an error outcome.
        let mock = MockTransport::new()
            .on(HttpMethod::Post, "/accounts/balance/get", 200, balance_json())
            .on(HttpMethod::Post, "/transactions/get", 200, transactions_json());
        let c = client(mock);
        let ok1 = c.balances().await.unwrap();
        let ok2 = c.transactions("2026-06-01", None).await.unwrap();
        let ok3 = c.spending_summary("2026-06-01", None).await.unwrap();
        for s in [&ok1, &ok2, &ok3] {
            for secret in [FAKE_CLIENT_ID, FAKE_SECRET, FAKE_ACCESS_TOKEN] {
                assert!(!s.contains(secret), "outcome leaked a secret: {s}");
            }
        }

        let err_mock = MockTransport::new().on(HttpMethod::Post, "/accounts/balance/get", 401, "{}");
        let err = client(err_mock).balances().await.unwrap_err().to_string();
        for secret in [FAKE_CLIENT_ID, FAKE_SECRET, FAKE_ACCESS_TOKEN] {
            assert!(!err.contains(secret), "error leaked a secret: {err}");
        }
    }

    // -- HARD RULE: no money-moving method or code path exists ----------------

    /// MIDAS NEVER MOVES MONEY. This is the structural guard: the whole plaid.rs
    /// source must name NO transfer/payment/trade endpoint or method — not even a
    /// gated one — and must not import the foundation's consequential gate. If a
    /// future edit adds a money-moving path, this test fails loudly. The source is
    /// read at test time from the crate's own file (no network).
    #[test]
    fn no_money_moving_method_exists() {
        // Scan only the PRODUCTION half of the module — everything before the test
        // module marker. This is what keeps the guard honest: the test's own
        // assertion literals (which necessarily NAME the forbidden endpoints) live
        // below the split, so they can never satisfy or trip the check.
        let full = include_str!("plaid.rs");
        let prod = full
            .split("#[cfg(test)]")
            .next()
            .expect("module has a production section before the tests");

        // No money-moving Plaid endpoint may appear anywhere in the production code.
        for forbidden in [
            "/transfer/",
            "/payment_initiation/",
            "transfer/create",
            "transfer/authorization",
            "payment/create",
            "/processor/",
        ] {
            assert!(
                !prod.contains(forbidden),
                "plaid.rs production code must contain NO money-moving endpoint, found: {forbidden}"
            );
        }
        // The client must NOT pull in the consequential gate or ActionMode — there
        // is nothing to gate because no money action exists. The doc comments
        // EXPLAIN the absence in prose (e.g. "no ActionMode surface"), so we check
        // for the actual code use of the type/function, not the bare words.
        assert!(
            !prod.contains("ActionMode,")
                && !prod.contains("ActionMode}")
                && !prod.contains(": ActionMode")
                && !prod.contains("ActionMode::"),
            "plaid.rs must not import or use ActionMode — Midas has no consequential surface"
        );
        assert!(
            !prod.contains("super::gate(") && !prod.contains("integrations::gate("),
            "plaid.rs must never call the consequential gate — there is no money action to gate"
        );
        // The three public methods are exactly the read trio — assert each exists.
        for read_method in [
            "pub async fn balances",
            "pub async fn transactions",
            "pub async fn spending_summary",
        ] {
            assert!(prod.contains(read_method), "missing read method: {read_method}");
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn balance_phrase_handles_each_combination() {
        assert_eq!(
            balance_phrase(&Balances { available: Some(10.0), current: Some(12.0) }),
            "available $10.00, current $12.00"
        );
        assert_eq!(balance_phrase(&Balances { available: Some(10.0), current: None }), "available $10.00");
        assert_eq!(balance_phrase(&Balances { available: None, current: Some(12.0) }), "current $12.00");
        assert_eq!(balance_phrase(&Balances { available: None, current: None }), "balance unavailable");
    }

    #[test]
    fn amount_phrase_signs_spend_and_credit() {
        assert_eq!(amount_phrase(5.75), "-$5.75");
        assert_eq!(amount_phrase(-2500.0), "+$2500.00");
        assert_eq!(amount_phrase(0.0), "-$0.00");
    }

    #[test]
    fn aggregate_spending_folds_and_sorts_descending() {
        let txns = vec![
            Transaction { date: "d".into(), name: "a".into(), amount: 5.0, category: vec!["Food".into()] },
            Transaction { date: "d".into(), name: "b".into(), amount: 20.0, category: vec!["Travel".into()] },
            Transaction { date: "d".into(), name: "c".into(), amount: 10.0, category: vec!["Food".into()] },
            // a credit must be excluded entirely.
            Transaction { date: "d".into(), name: "d".into(), amount: -100.0, category: vec!["Transfer".into()] },
            // no category -> Uncategorized.
            Transaction { date: "d".into(), name: "e".into(), amount: 3.0, category: vec![] },
        ];
        let agg = aggregate_spending(&txns);
        assert_eq!(agg[0], ("Travel".to_string(), 20.0), "highest spend leads");
        assert_eq!(agg[1], ("Food".to_string(), 15.0), "Food folded to 15");
        assert_eq!(agg[2], ("Uncategorized".to_string(), 3.0));
        assert!(!agg.iter().any(|(c, _)| c == "Transfer"), "credit excluded");
    }

    #[test]
    fn map_status_table() {
        assert!(map_status(200, "{}", "x").is_ok());
        assert!(map_status(201, "{}", "x").is_ok());
        assert!(map_status(
            400,
            r#"{"error_code":"ITEM_LOGIN_REQUIRED"}"#,
            "x"
        )
        .unwrap_err()
        .to_string()
        .contains("re-link"));
        assert!(map_status(
            400,
            r#"{"error_code":"INVALID_ACCESS_TOKEN"}"#,
            "x"
        )
        .unwrap_err()
        .to_string()
        .contains("credentials were rejected"));
        assert!(map_status(401, "{}", "x").unwrap_err().to_string().contains("credentials were rejected"));
        assert!(map_status(429, "{}", "x").unwrap_err().to_string().contains("rate limited"));
        assert!(map_status(503, "{}", "x").unwrap_err().to_string().contains("transient"));
    }
}
