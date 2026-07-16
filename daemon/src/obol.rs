//! OBOL — a durable cloud-spend LEDGER + a dollar-cap ROUTING BUDGET.
//!
//! OBOL answers ONE question honestly: "how much has this run spent on cloud
//! calls, and — if the owner set a daily dollar cap — should the next turn step
//! DOWN to a cheaper / more-local tier to stay under it?" It has two halves, and
//! both are strictly REDUCE-ONLY (mirroring [`crate::focus`] and the vault trim):
//!
//!   1. THE LEDGER (durable, bounded, secret-free). An APPEND-ONLY SQLite spend
//!      record — one row per cloud CALL: model, token counts, dollar cost, the
//!      handling agent, a unix-seconds ts. It is fed from the SAME point
//!      [`crate::eval::record_cloud_usage`] records cloud usage (the cloud reply
//!      path), reusing the SAME [`crate::eval::extract_token_usage`] token
//!      COUNTS — so a spend row is aggregate-only by construction: it can hold a
//!      model id, four integers, a dollar figure, and an agent name, and there is
//!      no field into which a prompt or a response could ever land. Bounded
//!      retention (evict-oldest past [`MAX_LEDGER_ROWS`]) keeps it tiny on the
//!      always-on appliance, exactly like the optimizer's trace store.
//!
//!   2. THE BUDGET (a REDUCE-ONLY routing input). A PURE [`budget_pressure`] maps
//!      (spend_today, daily_cap) to a [`Pressure`] the model-tier resolver reads
//!      as a NEW precedence input: **Override > Budget-floor > Auto > Fallback**.
//!      Under pressure the resolved tier can only step DOWN toward the cheaper /
//!      on-device path (Heavy -> Fast -> Local). It is INCAPABLE of the reverse:
//!      the fold ([`crate::model_tier::budget_floor_tier`]) never raises a tier,
//!      never loosens a gate, and NEVER blocks a call the user EXPLICITLY forces
//!      (a voice/HUD Override beats the budget-floor outright — see the model-tier
//!      precedence tests). It reads the API's OWN token counts only; it decides
//!      nothing consequential.
//!
//! ## Ships INERT (neutral by default)
//! `[obol].daily_usd_cap` ships `0.0` — "no cap". With no cap [`budget_pressure`]
//! is always [`Pressure::None`], so routing is byte-for-byte today's until the
//! owner sets a real dollar cap. The ledger still records (honest accounting is
//! free), but the budget influences NOTHING.
//!
//! ## Honesty
//! The dollar figures are a TRANSPARENT estimate: the per-model $/1M-token rates
//! ([`crate::eval::rates_for_model`]) are published list prices used as a
//! multiplier over the MEASURED token counts, never a billed number. The ledger
//! stores what the API told us (counts) plus that derived estimate — nothing it
//! invented, and nothing the user said.

use std::sync::{Mutex, OnceLock, RwLock};
use std::sync::Arc;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use tracing::warn;

use crate::config::Config;
use crate::eval::{rates_for_model, TokenUsage};

/// Hard cap on stored spend rows. The recorder evicts the oldest rows past this
/// so the ledger is bounded on the always-on appliance (mirrors the optimizer
/// trace store's `MAX_TRACES`). Generous enough for a long spend history, small
/// enough that the file stays tiny.
pub const MAX_LEDGER_ROWS: usize = 50_000;

/// The fraction of the daily cap at which the budget begins EASING routing down
/// (Heavy -> Fast) — a soft shoulder BEFORE the hard cap so the last turns of the
/// day do not slam from Heavy straight to Local. 0.8 == "80% of the cap spent".
pub const EASE_FRACTION: f64 = 0.8;

/// How many recent rows the spend report / meter surfaces.
const REPORT_RECENT: usize = 20;

/// Seconds in a UTC day — the (timezone-free, deterministic) day bucket the
/// rolling daily total is anchored to. A calendar-ish day, reset at 00:00 UTC.
const SECONDS_PER_DAY: u64 = 86_400;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The UTC day index a unix-seconds timestamp falls in.
fn day_index(ts: u64) -> i64 {
    (ts / SECONDS_PER_DAY) as i64
}

/// The unix-seconds start of the UTC day `now` falls in — the lower bound of the
/// "today" daily-sum query.
fn day_start(now: u64) -> u64 {
    (now / SECONDS_PER_DAY) * SECONDS_PER_DAY
}

// ---------------------------------------------------------------------------
// BUDGET PRESSURE — the PURE, REDUCE-ONLY routing input
// ---------------------------------------------------------------------------

/// How hard the daily dollar cap should push routing DOWN this turn. Ordered by
/// severity; every non-`None` value can ONLY make routing cheaper/more-local, and
/// the model-tier fold that reads it ([`crate::model_tier::budget_floor_tier`])
/// can only ever step a tier DOWN — never up, never past a user Override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    /// Below the ease shoulder (or no cap configured): no pressure, routing
    /// unchanged. This is the shipped default (cap = 0 -> always `None`).
    None,
    /// At/above [`EASE_FRACTION`] of the cap but under it: EASE routing one notch
    /// (a Heavy turn steps down to Fast). Fast/Local turns are already at or below
    /// that floor and are untouched.
    Ease,
    /// At or over the cap: FLOOR routing to on-device Local (no further cloud
    /// SPEND this turn). A hard turn still answers — just on-device — and a user
    /// Override still beats this outright.
    Floor,
}

impl Pressure {
    /// Stable identifier for telemetry / the HUD meter.
    pub fn as_str(&self) -> &'static str {
        match self {
            Pressure::None => "none",
            Pressure::Ease => "ease",
            Pressure::Floor => "floor",
        }
    }

    /// Whether this pressure actually steps routing down (Ease/Floor). `false` for
    /// `None` — the neutral default.
    pub fn is_active(&self) -> bool {
        !matches!(self, Pressure::None)
    }
}

/// PURE. Map today's measured cloud spend + the configured daily cap to a
/// [`Pressure`]. The single place the budget's severity is decided, so it is
/// unit-tested exhaustively over the (spend, cap) space.
///
/// Contract (honesty + reduce-only):
///   * `daily_cap <= 0` (the shipped default, or a NaN/negative misconfig) ->
///     [`Pressure::None`]. No cap == the feature is INERT: routing is today's.
///   * spend `>= cap`               -> [`Pressure::Floor`] (at/over cap).
///   * spend `>= EASE_FRACTION*cap` -> [`Pressure::Ease`]  (soft shoulder).
///   * otherwise                    -> [`Pressure::None`].
///   * A negative / non-finite `spend_today` is treated as 0 (never fabricated as
///     over-cap), so a garbled reading can only ever UNDER-report pressure — it
///     can never invent a step-down that isn't warranted.
pub fn budget_pressure(spend_today: f64, daily_cap: f64) -> Pressure {
    // No cap (or a NaN/negative cap) => inert, so a garbled cap never throttles.
    // (`is_nan() || <= 0.0` catches NaN and non-positive; a +inf cap falls through
    // but yields frac 0 below -> None, so it is inert too.)
    if daily_cap.is_nan() || daily_cap <= 0.0 {
        return Pressure::None;
    }
    let spend = if spend_today.is_finite() && spend_today > 0.0 {
        spend_today
    } else {
        0.0
    };
    let frac = spend / daily_cap;
    if frac >= 1.0 {
        Pressure::Floor
    } else if frac >= EASE_FRACTION {
        Pressure::Ease
    } else {
        Pressure::None
    }
}

// ---------------------------------------------------------------------------
// TODAY'S SPEND — a cheap, synchronous in-memory day total for the hot path
// ---------------------------------------------------------------------------

/// A single UTC-day running total, kept in memory so the per-turn budget read is
/// SYNCHRONOUS and allocation-free (the router's `conversation_brain` is a pure,
/// hot-path helper — it must not block on a DB sum every turn). The durable
/// ledger is the source of truth for the report/meter; this cache is only the
/// fast live budget read, seeded from the ledger at startup and rolled at the day
/// boundary. PURE over an explicit slot so the roll/accumulate logic is unit-
/// tested without touching the process-global.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DaySpend {
    day: i64,
    total_usd: f64,
}

impl DaySpend {
    const fn empty() -> Self {
        DaySpend {
            day: i64::MIN,
            total_usd: 0.0,
        }
    }

    /// Add one call's `cost_usd` to the total, rolling to a fresh day (total 0)
    /// when `ts` crosses into a new UTC day. A non-finite/negative cost is ignored
    /// (never lowers or NaNs the total) but the day-roll still applies.
    fn note(&mut self, ts: u64, cost_usd: f64) {
        let d = day_index(ts);
        if self.day != d {
            self.day = d;
            self.total_usd = 0.0;
        }
        if cost_usd.is_finite() && cost_usd > 0.0 {
            self.total_usd += cost_usd;
        }
    }

    /// Today's total as of `now` — 0 when the cached day is not `now`'s day (a new
    /// day with no spend yet), never a stale yesterday total.
    fn total_for(&self, now: u64) -> f64 {
        if self.day == day_index(now) {
            self.total_usd
        } else {
            0.0
        }
    }

    /// Replace the cache with an explicit (day, total) — the startup seed from the
    /// durable ledger.
    fn set(&mut self, now: u64, total_usd: f64) {
        self.day = day_index(now);
        self.total_usd = if total_usd.is_finite() && total_usd > 0.0 {
            total_usd
        } else {
            0.0
        };
    }
}

/// The process-global today-spend cache (mirrors the model-tier override / eval
/// usage-sink runtime-state pattern). Poison-tolerant.
static DAY_SPEND: Mutex<DaySpend> = Mutex::new(DaySpend::empty());

fn day_lock() -> std::sync::MutexGuard<'static, DaySpend> {
    DAY_SPEND.lock().unwrap_or_else(|p| p.into_inner())
}

/// The current budget [`Pressure`] for THIS turn, read SYNCHRONOUSLY from the
/// in-memory day total + `[obol].daily_usd_cap`. Returns [`Pressure::None`] when
/// no cap is configured (the shipped default) so the model-tier resolver is a
/// byte-for-byte no-op until the owner sets a cap. The router threads this into
/// [`crate::model_tier::resolve_tier`] as the Budget-floor precedence input.
pub fn current_budget_pressure(cfg: &Config) -> Pressure {
    let cap = cfg.obol.daily_usd_cap;
    if cap.is_nan() || cap <= 0.0 {
        return Pressure::None;
    }
    let now = now_secs();
    let spend = day_lock().total_for(now);
    budget_pressure(spend, cap)
}

// ---------------------------------------------------------------------------
// ACTIVE AGENT — the secret-free agent label for a spend row
// ---------------------------------------------------------------------------

/// The agent the current turn was routed to, noted by the router at agent
/// selection so a spend row can attribute cost to it. `None` until the first
/// turn selects an agent -> the honest default label "cloud".
static ACTIVE_AGENT: RwLock<Option<String>> = RwLock::new(None);

/// Note the agent handling the current turn (the router calls this once per turn
/// at agent selection). Secret-free by contract: it is an AGENT NAME, never an
/// utterance. Poison-tolerant.
pub fn note_active_agent(agent: &str) {
    if let Ok(mut g) = ACTIVE_AGENT.write() {
        *g = Some(agent.to_string());
    }
}

/// The agent to attribute the next spend row to — the last-noted agent, or the
/// honest default "cloud" before any turn has selected one.
fn active_agent() -> String {
    ACTIVE_AGENT
        .read()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_else(|| "cloud".to_string())
}

// ---------------------------------------------------------------------------
// SPEND ROW + the bounded, append-only ledger store
// ---------------------------------------------------------------------------

/// One recorded cloud CALL's spend. SECRET-FREE BY CONSTRUCTION: it holds a model
/// id, four token/cost numbers, and an agent name — there is NO field that could
/// carry a prompt, a response, or any utterance. The token counts come straight
/// from the API `usage` block via [`crate::eval::extract_token_usage`]; the
/// dollar `cost_usd` is the transparent estimate (published $/1M rates over those
/// counts, never a billed figure).
#[derive(Debug, Clone, PartialEq)]
pub struct SpendRow {
    /// Unix seconds when the call was recorded.
    pub ts: u64,
    /// The cloud model string (e.g. the Messages API's echoed `model`), or "" if
    /// absent — never anything but the model id.
    pub model: String,
    /// Uncached input (prompt) token COUNT — a number, never the prompt.
    pub input_tokens: u64,
    /// Output (completion) token COUNT — a number, never the response.
    pub output_tokens: u64,
    /// Cache-READ token count.
    pub cache_read_tokens: u64,
    /// The transparent dollar ESTIMATE for this call (rates × counts).
    pub cost_usd: f64,
    /// The agent the turn was routed to (a name, never an utterance).
    pub agent: String,
}

/// The durable, bounded, append-only spend ledger. A dedicated SQLite file
/// (state/obol/obol.db) opened with the same WAL + busy-timeout pattern as the
/// optimizer trace store. `rusqlite::Connection` is Send-not-Sync, so an async
/// Mutex serializes access (the statements are short).
pub struct SpendLedger {
    conn: AsyncMutex<Connection>,
}

impl SpendLedger {
    /// Open (creating if needed) the ledger at `path` PLAINTEXT — reached when
    /// `[security].encrypt_memory` is OFF (the default). Idempotent.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init_conn(conn)
    }

    /// Open the ledger ENCRYPTED (transparent whole-file SQLCipher AES-256), the
    /// same custody seam the trace store uses. `key` is applied via `PRAGMA key`
    /// immediately after open, before any other statement.
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// A private in-memory ledger for hermetic tests (no file, no Keychain).
    #[cfg(test)]
    fn open_in_memory() -> Result<Self> {
        Self::init_conn(Connection::open_in_memory()?)
    }

    /// Shared setup (pragmas + schema), run AFTER any `PRAGMA key`.
    fn init_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS spend(
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_read_tokens INTEGER NOT NULL,
                cost_usd REAL NOT NULL,
                agent TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_spend_ts ON spend(ts);",
        )?;
        Ok(Self {
            conn: AsyncMutex::new(conn),
        })
    }

    /// Append one spend row, then evict the oldest rows beyond [`MAX_LEDGER_ROWS`]
    /// so the ledger stays bounded (evict by monotonic `id`, robust when many rows
    /// share a ts second). Returns the inserted row id for test assertions.
    pub async fn record(&self, row: &SpendRow) -> Result<i64> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO spend(ts, model, input_tokens, output_tokens, cache_read_tokens, cost_usd, agent)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                row.ts as i64,
                row.model,
                row.input_tokens as i64,
                row.output_tokens as i64,
                row.cache_read_tokens as i64,
                row.cost_usd,
                row.agent,
            ],
        )?;
        let id = conn.last_insert_rowid();
        conn.execute(
            "DELETE FROM spend WHERE id NOT IN
             (SELECT id FROM spend ORDER BY id DESC LIMIT ?1)",
            params![MAX_LEDGER_ROWS as i64],
        )?;
        Ok(id)
    }

    /// The DAILY-SUM primitive: total `cost_usd` of every row at or after
    /// `ts_start` (0.0 when none). The budget's "spend today" is
    /// `spend_since(day_start(now))`.
    pub async fn spend_since(&self, ts_start: u64) -> Result<f64> {
        let conn = self.conn.lock().await;
        let sum: f64 = conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0.0) FROM spend WHERE ts >= ?1",
            params![ts_start as i64],
            |r| r.get(0),
        )?;
        Ok(sum)
    }

    /// Count of rows at or after `ts_start` (calls today, for the report).
    pub async fn count_since(&self, ts_start: u64) -> Result<u64> {
        let conn = self.conn.lock().await;
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM spend WHERE ts >= ?1",
            params![ts_start as i64],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// The most recent `limit` rows, NEWEST first — the read path for the spend
    /// report / meter.
    pub async fn recent(&self, limit: usize) -> Result<Vec<SpendRow>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT ts, model, input_tokens, output_tokens, cache_read_tokens, cost_usd, agent
             FROM spend ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(SpendRow {
                    ts: row.get::<_, i64>(0)? as u64,
                    model: row.get(1)?,
                    input_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    cache_read_tokens: row.get::<_, i64>(4)? as u64,
                    cost_usd: row.get(5)?,
                    agent: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Total stored rows (tests / telemetry / retention assertions).
    #[allow(dead_code)] // hermetic test + future-telemetry helper
    pub async fn count(&self) -> Result<u64> {
        let conn = self.conn.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM spend", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }
}

// ---------------------------------------------------------------------------
// LIVE FEED — process-global ledger sink (mirrors eval::USAGE_SINK)
// ---------------------------------------------------------------------------

/// The daemon's live [`SpendLedger`] handle, registered ONCE at startup so the
/// cloud reply path can feed measured spend into the SAME durable ledger the
/// report/meter reads — WITHOUT threading an `Arc<SpendLedger>` through every
/// cloud-call signature. Mirrors `eval::USAGE_SINK` exactly. Unset (every unit
/// test, and any run before install) -> the durable append is a silent no-op (the
/// in-memory day total still updates so the budget math stays live).
static LEDGER_SINK: OnceLock<Arc<SpendLedger>> = OnceLock::new();

/// Register the daemon's live [`SpendLedger`] as the global spend sink. Called
/// ONCE from `main`. Idempotent: a second call is ignored.
pub fn install_ledger(ledger: Arc<SpendLedger>) {
    let _ = LEDGER_SINK.set(ledger);
}

/// Seed the in-memory today-total from the DURABLE ledger at startup so the live
/// budget is correct immediately after a restart (rather than resetting to 0 and
/// briefly under-counting the day's spend). A read failure leaves the cache at 0
/// (a safe under-count — the budget can only ever be too lenient, never invent a
/// step-down). Called once from `main` after the ledger is opened.
pub async fn seed_day_cache(ledger: &SpendLedger) {
    let now = now_secs();
    match ledger.spend_since(day_start(now)).await {
        Ok(today) => day_lock().set(now, today),
        Err(e) => warn!(error = %e, "obol: failed to seed day cache from ledger; budget starts at 0"),
    }
}

/// LIVE SPEND FEED for the cloud reply path. Fed from the SAME point
/// [`crate::eval::record_cloud_usage`] records cloud usage, with the SAME already-
/// extracted [`TokenUsage`] counts.
///
/// It (1) always updates the in-memory today-total (so the budget read stays live
/// even in a test/pre-install run) and (2) appends a durable row IF the ledger
/// sink is installed. AGGREGATE-ONLY / SECRET-FREE: it reads only the numeric
/// `usage` counts (already parsed) + the response's `model` string — never
/// `content`, never the utterance — so NO PII can enter the ledger.
pub async fn record_cloud_spend(resp: &Value, usage: TokenUsage) {
    let now = now_secs();
    // The model string the API echoes (the ONLY string we read off the response),
    // used to pick the transparent per-model rate and label the row.
    let model = resp
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let cost_usd = rates_for_model(&model).cost_of(&usage);

    // Always keep the in-memory day total live (drives current_budget_pressure).
    day_lock().note(now, cost_usd);

    // Durable append only when the sink is installed (no-op in tests/pre-startup).
    let Some(ledger) = LEDGER_SINK.get() else {
        return;
    };
    let row = SpendRow {
        ts: now,
        model,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cost_usd,
        agent: active_agent(),
    };
    if let Err(e) = ledger.record(&row).await {
        warn!(error = %e, "obol: spend ledger append failed");
    }
}

// ---------------------------------------------------------------------------
// SPEND REPORT / METER — the read-only surface (SECRET-FREE)
// ---------------------------------------------------------------------------

fn round_usd(v: f64) -> f64 {
    if v.is_finite() {
        (v * 10_000.0).round() / 10_000.0
    } else {
        0.0
    }
}

/// Build the AGGREGATE-ONLY spend report the HUD's SPEND // CLOUD METER renders
/// (and the periodic `obol.spend` telemetry carries). SECRET-FREE: only the
/// day/cap dollars, the derived pressure, the headroom, a call count, and the
/// recent rows' NON-SECRET fields (model + counts + cost + agent + ts). PURE —
/// a function of its inputs, so it is unit-tested directly. The dollar figures are
/// always LABELLED an estimate (published rates × measured counts, not billed).
pub fn build_spend_report(
    day_spend_usd: f64,
    daily_cap_usd: f64,
    calls_today: u64,
    recent: &[SpendRow],
    now: u64,
) -> Value {
    let pressure = budget_pressure(day_spend_usd, daily_cap_usd);
    let cap_configured = daily_cap_usd > 0.0;
    let headroom = if cap_configured {
        (daily_cap_usd - day_spend_usd).max(0.0)
    } else {
        0.0
    };
    let fraction = if cap_configured {
        (day_spend_usd / daily_cap_usd).clamp(0.0, f64::INFINITY)
    } else {
        0.0
    };
    let rows: Vec<Value> = recent
        .iter()
        .map(|r| {
            json!({
                "ts": r.ts,
                "model": r.model,
                "input_tokens": r.input_tokens,
                "output_tokens": r.output_tokens,
                "cache_read_tokens": r.cache_read_tokens,
                "cost_usd": round_usd(r.cost_usd),
                "agent": r.agent,
            })
        })
        .collect();
    json!({
        "day_spend_usd": round_usd(day_spend_usd),
        "daily_cap_usd": round_usd(daily_cap_usd),
        "cap_configured": cap_configured,
        "headroom_usd": round_usd(headroom),
        "fraction": (fraction * 1000.0).round() / 1000.0,
        // The REDUCE-ONLY budget posture on the wire so the HUD copy is grounded.
        "pressure": pressure.as_str(),
        "will_step_down": pressure.is_active(),
        "reduce_only": true,
        "calls_today": calls_today,
        "now": now,
        "recent": rows,
        // The dollars are a transparent estimate (rates × measured counts).
        "cost_is_estimate": true,
    })
}

/// Assemble + return the read-only spend report from the DURABLE ledger (the
/// `spend_report` op's core + the meter task's payload). Uses `[obol].daily_usd_cap`
/// for the cap and the ledger's own daily-sum for "today", so the report and the
/// live budget agree on the same UTC-day window. Read failures degrade to an
/// honest zero report (never wedges, never fabricates spend).
pub async fn spend_report(cfg: &Config, ledger: &SpendLedger) -> Value {
    let now = now_secs();
    let start = day_start(now);
    let today = ledger.spend_since(start).await.unwrap_or(0.0);
    let calls = ledger.count_since(start).await.unwrap_or(0);
    let recent = ledger.recent(REPORT_RECENT).await.unwrap_or_default();
    build_spend_report(today, cfg.obol.daily_usd_cap, calls, &recent, now)
}

/// Interval between `obol.spend` meter emits.
const METER_INTERVAL: Duration = Duration::from_secs(30);
/// Startup delay before the first meter emit (mirrors the eval report task).
const METER_STARTUP_DELAY: Duration = Duration::from_secs(20);

/// The periodic SPEND // CLOUD METER feed: emit the read-only `obol.spend` report
/// so the HUD gauge can render the current day spend vs cap + the reduce-only
/// budget posture. Fire-and-forget through the existing telemetry hub; a read
/// failure inside `spend_report` degrades to an honest zero report, never wedges.
pub async fn spend_meter_task(cfg: Arc<Config>, ledger: Arc<SpendLedger>) {
    tokio::time::sleep(METER_STARTUP_DELAY).await;
    loop {
        let report = spend_report(&cfg, &ledger).await;
        crate::telemetry::emit("system", "obol.spend", report);
        tokio::time::sleep(METER_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // budget_pressure — PURE, exhaustive over the (spend, cap) space
    // -----------------------------------------------------------------------

    #[test]
    fn budget_pressure_below_cap_is_none() {
        // Well under the ease shoulder -> no pressure.
        assert_eq!(budget_pressure(0.0, 10.0), Pressure::None);
        assert_eq!(budget_pressure(1.0, 10.0), Pressure::None);
        // Just below the 80% ease shoulder.
        assert_eq!(budget_pressure(7.99, 10.0), Pressure::None);
    }

    #[test]
    fn budget_pressure_near_cap_eases_over_cap_floors() {
        // At/above 80% but under 100% -> EASE (step Heavy down to Fast).
        assert_eq!(budget_pressure(8.0, 10.0), Pressure::Ease);
        assert_eq!(budget_pressure(9.99, 10.0), Pressure::Ease);
        // At/over the cap -> FLOOR (force on-device Local, no more cloud spend).
        assert_eq!(budget_pressure(10.0, 10.0), Pressure::Floor);
        assert_eq!(budget_pressure(25.0, 10.0), Pressure::Floor);
    }

    #[test]
    fn budget_pressure_no_cap_is_inert() {
        // The shipped default (cap 0) is ALWAYS None -> routing is byte-for-byte
        // today's, no matter the spend. This is the ships-neutral contract.
        assert_eq!(budget_pressure(0.0, 0.0), Pressure::None);
        assert_eq!(budget_pressure(1_000.0, 0.0), Pressure::None);
        // A negative / NaN cap is a misconfig -> also inert (never throttles).
        assert_eq!(budget_pressure(1_000.0, -5.0), Pressure::None);
        assert_eq!(budget_pressure(1_000.0, f64::NAN), Pressure::None);
    }

    #[test]
    fn budget_pressure_garbled_spend_never_over_reports() {
        // A negative / non-finite spend reading is treated as 0 — the budget can
        // only ever UNDER-report pressure, never invent a step-down.
        assert_eq!(budget_pressure(-100.0, 10.0), Pressure::None);
        assert_eq!(budget_pressure(f64::NAN, 10.0), Pressure::None);
        assert_eq!(budget_pressure(f64::INFINITY, 10.0), Pressure::None);
    }

    #[test]
    fn pressure_labels_and_activity_are_stable() {
        assert_eq!(Pressure::None.as_str(), "none");
        assert_eq!(Pressure::Ease.as_str(), "ease");
        assert_eq!(Pressure::Floor.as_str(), "floor");
        assert!(!Pressure::None.is_active());
        assert!(Pressure::Ease.is_active());
        assert!(Pressure::Floor.is_active());
    }

    // -----------------------------------------------------------------------
    // DaySpend — PURE roll/accumulate over an explicit slot (no global)
    // -----------------------------------------------------------------------

    #[test]
    fn day_spend_accumulates_within_a_day_and_rolls_over() {
        let base = 5 * SECONDS_PER_DAY + 100; // some ts inside UTC-day 5
        let mut d = DaySpend::empty();
        d.note(base, 1.5);
        d.note(base + 10, 2.5);
        assert!((d.total_for(base + 20) - 4.0).abs() < 1e-9, "same day accumulates");
        // A ts in the NEXT day resets the total to just that call.
        let next = base + SECONDS_PER_DAY;
        d.note(next, 0.75);
        assert!((d.total_for(next) - 0.75).abs() < 1e-9, "new day rolls the total");
        // Reading with a `now` in a different day than the cache reads 0.
        assert_eq!(d.total_for(base), 0.0, "yesterday's total is not surfaced today");
    }

    #[test]
    fn day_spend_ignores_garbled_cost_but_still_rolls() {
        let base = 9 * SECONDS_PER_DAY + 42;
        let mut d = DaySpend::empty();
        d.note(base, 3.0);
        d.note(base + 1, f64::NAN); // ignored
        d.note(base + 2, -1.0); // ignored
        assert!((d.total_for(base) - 3.0).abs() < 1e-9);
        // set() seeds an explicit total (the startup seed), clamping garbage to 0.
        d.set(base, 12.5);
        assert!((d.total_for(base) - 12.5).abs() < 1e-9);
        d.set(base, f64::NAN);
        assert_eq!(d.total_for(base), 0.0);
    }

    // -----------------------------------------------------------------------
    // LEDGER — append + bounded retention + daily-sum query
    // -----------------------------------------------------------------------

    fn row(ts: u64, model: &str, cost: f64, agent: &str) -> SpendRow {
        SpendRow {
            ts,
            model: model.to_string(),
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 1000,
            cost_usd: cost,
            agent: agent.to_string(),
        }
    }

    #[tokio::test]
    async fn ledger_appends_and_reads_back_newest_first() {
        let led = SpendLedger::open_in_memory().unwrap();
        led.record(&row(100, "claude-opus-4-8", 0.5, "darwin")).await.unwrap();
        led.record(&row(200, "claude-haiku-4-5", 0.1, "gecko")).await.unwrap();
        assert_eq!(led.count().await.unwrap(), 2);
        let recent = led.recent(10).await.unwrap();
        assert_eq!(recent.len(), 2);
        // Newest first.
        assert_eq!(recent[0].ts, 200);
        assert_eq!(recent[0].agent, "gecko");
        assert_eq!(recent[1].ts, 100);
    }

    #[tokio::test]
    async fn ledger_daily_sum_query_sums_the_window() {
        let led = SpendLedger::open_in_memory().unwrap();
        let day = 100 * SECONDS_PER_DAY;
        // Two rows today, one row yesterday.
        led.record(&row(day + 10, "m", 1.25, "a")).await.unwrap();
        led.record(&row(day + 20, "m", 0.75, "a")).await.unwrap();
        led.record(&row(day - 5, "m", 9.99, "a")).await.unwrap();
        // Sum since the start of today == only today's two rows.
        let today = led.spend_since(day).await.unwrap();
        assert!((today - 2.0).abs() < 1e-9, "daily sum = 1.25 + 0.75");
        assert_eq!(led.count_since(day).await.unwrap(), 2);
        // Sum since 0 == everything.
        let all = led.spend_since(0).await.unwrap();
        assert!((all - 11.99).abs() < 1e-9);
    }

    #[tokio::test]
    async fn ledger_empty_daily_sum_is_zero_not_error() {
        let led = SpendLedger::open_in_memory().unwrap();
        assert_eq!(led.spend_since(0).await.unwrap(), 0.0);
        assert_eq!(led.count_since(0).await.unwrap(), 0);
        assert!(led.recent(5).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ledger_evicts_oldest_past_the_cap() {
        // Prove bounded retention WITHOUT inserting MAX_LEDGER_ROWS: exercise the
        // same evict-by-id DELETE with a tiny explicit cap via a hand-rolled store.
        let led = SpendLedger::open_in_memory().unwrap();
        // Insert 5 rows; then manually enforce a cap of 3 using the SAME eviction
        // SQL the recorder runs, and assert only the newest 3 survive.
        for i in 0..5u64 {
            led.record(&row(1000 + i, "m", 0.1, "a")).await.unwrap();
        }
        {
            let conn = led.conn.lock().await;
            conn.execute(
                "DELETE FROM spend WHERE id NOT IN
                 (SELECT id FROM spend ORDER BY id DESC LIMIT ?1)",
                params![3i64],
            )
            .unwrap();
        }
        let recent = led.recent(100).await.unwrap();
        assert_eq!(recent.len(), 3, "cap enforced: only newest 3 survive");
        // The survivors are the 3 newest ts (1004, 1003, 1002).
        assert_eq!(recent.iter().map(|r| r.ts).collect::<Vec<_>>(), vec![1004, 1003, 1002]);
    }

    #[tokio::test]
    async fn ledger_record_is_bounded_by_max_rows_constant() {
        // The recorder's OWN eviction keeps the store at <= MAX_LEDGER_ROWS. We do
        // not insert 50k rows here (slow); instead assert the recorder never GROWS
        // the store beyond the cap by checking the invariant holds for a small run
        // (the SQL is identical whether the cap is 3 or 50k — proven above).
        let led = SpendLedger::open_in_memory().unwrap();
        for i in 0..10u64 {
            led.record(&row(i, "m", 0.01, "a")).await.unwrap();
        }
        assert!(led.count().await.unwrap() <= MAX_LEDGER_ROWS as u64);
        assert_eq!(led.count().await.unwrap(), 10);
    }

    // -----------------------------------------------------------------------
    // SECRET-FREE — a spend row / report can never carry an utterance
    // -----------------------------------------------------------------------

    /// A synthetic Messages API response carrying a `usage` block AND `content`
    /// text — the exact shape the cloud reply path parses. The content string is a
    /// leak canary: it must NEVER appear in a recorded spend row or the report.
    fn synthetic_resp(model: &str, input: u64, output: u64, cache: u64) -> Value {
        json!({
            "id": "msg_x",
            "model": model,
            "role": "assistant",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "SECRET UTTERANCE CANARY do-not-leak"}],
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": cache,
            },
        })
    }

    #[tokio::test]
    async fn spend_row_and_report_are_secret_free() {
        let led = SpendLedger::open_in_memory().unwrap();
        // Feed a synthetic response whose content carries the canary; the ledger
        // must record ONLY the model + counts + cost + agent.
        note_active_agent("darwin");
        let resp = synthetic_resp("claude-opus-4-8", 500, 50, 5000);
        let usage = crate::eval::extract_token_usage(&resp).unwrap();
        // Record straight into this ledger (the record path builds the row from
        // resp+usage, never from content).
        let cost = rates_for_model("claude-opus-4-8").cost_of(&usage);
        led.record(&SpendRow {
            ts: 42,
            model: resp["model"].as_str().unwrap().to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cost_usd: cost,
            agent: active_agent(),
        })
        .await
        .unwrap();

        let recent = led.recent(10).await.unwrap();
        let row = &recent[0];
        assert_eq!(row.model, "claude-opus-4-8");
        assert_eq!(row.input_tokens, 500);
        assert!(row.cost_usd > 0.0);
        // The whole row serialized: NO canary anywhere.
        let s = format!("{row:?}");
        assert!(!s.contains("CANARY"), "spend row leaked utterance content");
        assert!(!s.contains("UTTERANCE"));

        // The read-only report is likewise secret-free.
        let report = build_spend_report(cost, 0.0, 1, &recent, 100).to_string();
        assert!(!report.contains("CANARY"), "spend report leaked utterance content");
        assert!(!report.contains("do-not-leak"));
    }

    // -----------------------------------------------------------------------
    // REPORT SHAPE — honest cap/pressure/headroom
    // -----------------------------------------------------------------------

    #[test]
    fn report_states_cap_pressure_and_headroom() {
        let rows = vec![row(10, "claude-haiku-4-5", 0.25, "gecko")];
        // Cap $10, spent $9 -> EASE (>=80%), headroom $1.
        let v = build_spend_report(9.0, 10.0, 3, &rows, 500);
        assert_eq!(v["cap_configured"], true);
        assert_eq!(v["pressure"], "ease");
        assert_eq!(v["will_step_down"], true);
        assert!((v["headroom_usd"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(v["calls_today"], 3);
        assert_eq!(v["reduce_only"], true);
        assert_eq!(v["cost_is_estimate"], true);
        // The recent row is present with its non-secret fields.
        assert_eq!(v["recent"][0]["agent"], "gecko");
        assert_eq!(v["recent"][0]["model"], "claude-haiku-4-5");
    }

    #[test]
    fn report_no_cap_is_pure_accounting_no_pressure() {
        // No cap configured (the default) -> accounting only, never a step-down.
        let v = build_spend_report(123.45, 0.0, 7, &[], 1);
        assert_eq!(v["cap_configured"], false);
        assert_eq!(v["pressure"], "none");
        assert_eq!(v["will_step_down"], false);
        assert_eq!(v["headroom_usd"], 0.0);
        assert!((v["day_spend_usd"].as_f64().unwrap() - 123.45).abs() < 1e-9);
    }

    #[test]
    fn report_over_cap_floors() {
        let v = build_spend_report(12.0, 10.0, 40, &[], 1);
        assert_eq!(v["pressure"], "floor");
        assert_eq!(v["will_step_down"], true);
        assert_eq!(v["headroom_usd"], 0.0, "over cap -> no headroom (clamped >=0)");
    }
}
