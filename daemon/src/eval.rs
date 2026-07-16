//! EVAL FRAMEWORK — the measured scorecard for a live DARWIN.
//!
//! This module turns the raw instrumentation the daemon already collects into
//! three HONESTLY-MEASURED aggregates the HUD's Eval/Optimizer panel renders:
//!
//!   1. LATENCY  — p50/p95 (+ count) over a BOUNDED rolling window of REAL
//!      per-turn durations measured by the existing pipeline clocks
//!      ([`crate::PipelineTiming`] queue/stt/classify/route + the end-to-end
//!      total). Nothing here estimates latency: it records the milliseconds the
//!      turn loop already measured.
//!   2. COST     — rolling token SUMS (input / output / cache-read) over a
//!      bounded window of REAL cloud `usage` counters, plus a transparently
//!      LABELLED dollar ESTIMATE (the published $/1M rate is a multiplier, not a
//!      billed number). Tokens are the truth; dollars are a clearly-marked
//!      estimate.
//!   3. ACCURACY — routing accuracy via [`crate::optimize::score_config`] over
//!      the SAME held-out trace split the optimizer judges candidates on
//!      ([`optimize::baseline_held_out_score`]), PLUS the live CORRECTION RATE
//!      (corrected-next-turn / usable) straight from the trace store. Both are
//!      MEASURED from real recorded traces.
//!
//! HONESTY / RUNTIME-GATING (non-negotiable):
//!   * The LATENCY and COST aggregates are fed from LIVE turns/cloud calls, so a
//!     fresh daemon has none until real turns happen — every aggregate reports
//!     `awaiting turns` (empty), NEVER a fabricated value. The hermetic tests in
//!     this module prove the MATH on SYNTHETIC durations/usage/traces; they do
//!     NOT measure a live turn (which would need the mic + the cloud and is
//!     out-of-scope for a hermetic test).
//!   * COST has a second runtime gate: the live token `usage` counters are only
//!     available once the cloud reply path surfaces them per turn. Until that
//!     wiring lands the cost window simply stays empty (`awaiting turns`) — the
//!     machinery is proven here, the live feed is gated on real cloud calls.
//!   * NO PII ever leaves this module: the emitted `eval.*` telemetry and the
//!     report snapshot are AGGREGATE-ONLY (percentiles, sums, rates, counts).
//!     No utterance, no per-turn row, no identifier.
//!
//! The state is a small in-memory [`EvalState`] (bounded ring buffers) the daemon
//! holds for its lifetime and updates per turn — mirroring how `telemetry`'s
//! snapshot is held. It is intentionally NOT persisted: the scorecard is a live,
//! rolling view of the current run, not a durable corpus (the trace store is the
//! durable corpus; accuracy is recomputed from it).

use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::optimize::{self, Outcome, Trace};
use crate::telemetry;
use crate::PipelineTiming;

/// Bounded rolling window for the latency + cost aggregates. Large enough for a
/// stable p95, small enough that the in-memory ring stays tiny on the always-on
/// appliance. Past this the OLDEST sample is evicted (the scorecard reflects the
/// RECENT run, not all-time).
pub const EVAL_WINDOW: usize = 256;

// ---------------------------------------------------------------------------
// Per-model cost rates — TRANSPARENT, clearly an ESTIMATE
// ---------------------------------------------------------------------------

/// Published $/1M-token rates used ONLY as a transparent multiplier to turn the
/// measured token SUMS into a dollar ESTIMATE. These are list prices, not a
/// billed figure — the emitted cost is always LABELLED an estimate. Kept as plain
/// constants (no network, no per-request price lookup) so the math is hermetic
/// and the assumption is auditable in one place.
///
/// Defaults track the daemon's primary cloud model's published pricing; a
/// caller may pass its own [`CostRates`] if the model differs. Cache-read tokens
/// are billed at the discounted cache rate.
#[derive(Debug, Clone, Copy)]
pub struct CostRates {
    /// $ per 1,000,000 input (uncached) tokens.
    pub input_per_m: f64,
    /// $ per 1,000,000 output tokens.
    pub output_per_m: f64,
    /// $ per 1,000,000 cache-READ tokens (the discounted rate).
    pub cache_read_per_m: f64,
}

impl Default for CostRates {
    fn default() -> Self {
        // Published list pricing for the primary cloud model, in USD per 1M
        // tokens. A transparent multiplier ONLY — the emitted dollar figure is
        // always marked an estimate, never a billed number.
        Self {
            input_per_m: 3.00,
            output_per_m: 15.00,
            cache_read_per_m: 0.30,
        }
    }
}

impl CostRates {
    /// The dollar ESTIMATE for ONE call's [`TokenUsage`] under these rates — the
    /// shared "rates × measured counts" formula. A transparent multiplier, never a
    /// billed figure. Used by the eval cost aggregate AND the OBOL spend ledger so
    /// both derive their dollars from the SAME arithmetic (no drift).
    pub fn cost_of(&self, usage: &TokenUsage) -> f64 {
        (usage.input_tokens as f64) / 1_000_000.0 * self.input_per_m
            + (usage.output_tokens as f64) / 1_000_000.0 * self.output_per_m
            + (usage.cache_read_tokens as f64) / 1_000_000.0 * self.cache_read_per_m
    }
}

/// The TRANSPARENT per-model $/1M-token price table (published list prices used
/// ONLY as an estimate multiplier, never a billed number). A coarse match on the
/// model id: the heavy (Opus) family is the most expensive, the fast (Haiku)
/// family the cheapest, the Sonnet family in between; anything unrecognized (an
/// empty/absent model string, a future model) falls back to [`CostRates::default`]
/// so the estimate is honest-but-conservative rather than free. PURE — the single
/// place the daemon maps a model string to its rate, so the eval scorecard and the
/// OBOL ledger agree on what a given model costs.
pub fn rates_for_model(model: &str) -> CostRates {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") {
        // Heavy tier — the most capable + the most expensive.
        CostRates { input_per_m: 15.00, output_per_m: 75.00, cache_read_per_m: 1.50 }
    } else if m.contains("haiku") {
        // Fast tier — quick + cheap.
        CostRates { input_per_m: 1.00, output_per_m: 5.00, cache_read_per_m: 0.10 }
    } else if m.contains("sonnet") {
        CostRates { input_per_m: 3.00, output_per_m: 15.00, cache_read_per_m: 0.30 }
    } else {
        // Unknown / absent model id — the conservative primary-model default.
        CostRates::default()
    }
}

/// One turn's REAL cloud token usage (the COST primitive). Surfaced from the
/// cloud reply path's `usage` counters per turn. A turn that made no cloud call
/// (on-device only) contributes nothing — it is simply never recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Uncached input (prompt) tokens billed at the input rate.
    pub input_tokens: u64,
    /// Output (completion) tokens billed at the output rate.
    pub output_tokens: u64,
    /// Cache-READ tokens billed at the discounted cache rate.
    pub cache_read_tokens: u64,
}

/// Read the Messages API response's `usage` block into a [`TokenUsage`] — the
/// AGGREGATE token-COUNT primitive the cost window consumes. AGGREGATE-ONLY by
/// construction: it reads ONLY the numeric `usage.*` counters and can never see
/// `content`/utterance text, so NO PII can enter the cost path through here.
///
/// Contract (honesty-first, no-op on absence):
///   * The whole `usage` object ABSENT (a malformed / usage-less reply) ->
///     `None`, so the live feed is a safe NO-OP rather than recording a fake
///     all-zero turn.
///   * A present `usage` object with a missing or non-integer counter -> that
///     counter reads 0 (the field simply did not bill), but the turn IS counted.
///   * Cache-READ is read from `cache_read_input_tokens` (the Messages API name);
///     cache-CREATION tokens are intentionally not summed here — the cost window
///     tracks input / output / cache-read, matching [`CostRates`].
pub fn extract_token_usage(resp: &Value) -> Option<TokenUsage> {
    let usage = resp.get("usage")?;
    if !usage.is_object() {
        return None;
    }
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    Some(TokenUsage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_read_tokens: field("cache_read_input_tokens"),
    })
}

// ---------------------------------------------------------------------------
// LIVE COST FEED — process-global usage sink (mirrors `telemetry`'s global hub)
// ---------------------------------------------------------------------------

/// The daemon's live [`EvalState`] handle, registered ONCE at startup so the
/// cloud reply path (`anthropic.rs`) can feed measured `usage` into the SAME
/// rolling cost window the periodic report task reads — WITHOUT threading an
/// `Arc<Mutex<EvalState>>` through every `complete_*` signature and caller. This
/// mirrors `telemetry`'s process-global `HUB`/`SNAPSHOT`: the cloud path already
/// reaches `telemetry::emit` the same handle-free way, so the cost feed uses the
/// same pattern for the least-invasive wiring. Unset (every unit test, and any
/// run before `install_usage_sink`) -> the feed is a silent no-op.
static USAGE_SINK: OnceLock<Arc<Mutex<EvalState>>> = OnceLock::new();

/// Register the daemon's live [`EvalState`] as the global cost-feed sink. Called
/// ONCE from `main` with the SAME handle the report task reads, so cloud `usage`
/// recorded via [`record_cloud_usage`] lands in the rolling window the scorecard
/// renders. Idempotent: a second call is ignored (the first registration wins).
pub fn install_usage_sink(state: Arc<Mutex<EvalState>>) {
    let _ = USAGE_SINK.set(state);
}

/// LIVE COST FEED entry point for the cloud reply path. Parses the response's
/// `usage` block via [`extract_token_usage`] and records the measured token
/// counts into the global [`EvalState`] cost window.
///
/// HONESTY / SAFETY:
///   * AGGREGATE-ONLY: only the numeric `usage.*` counters are read — never
///     `content`, never the utterance — so NO PII can enter the cost window.
///   * NO-OP on a usage-less / malformed reply (`extract_token_usage` -> `None`)
///     and a NO-OP when the sink is unregistered (unit tests / pre-startup), so a
///     missing usage block or a daemon without the sink simply records nothing.
///   * Called per CLOUD ROUND-TRIP (each tool-loop round, plain, persona) — the
///     real production call site that lights up the runtime-gated cost metric.
pub async fn record_cloud_usage(resp: &Value) {
    let Some(usage) = extract_token_usage(resp) else {
        return;
    };
    // OBOL: feed the durable spend LEDGER + the live dollar-budget from the SAME
    // point, with the SAME measured token COUNTS (aggregate-only; no content; a
    // no-op durable append until the ledger sink is installed). The eval cost
    // window and the OBOL ledger therefore share one feed and one price table.
    crate::obol::record_cloud_spend(resp, usage).await;
    let Some(sink) = USAGE_SINK.get() else {
        return;
    };
    sink.lock().await.record_usage(usage);
}

// ---------------------------------------------------------------------------
// Aggregate result types (what the HUD renders / the report carries)
// ---------------------------------------------------------------------------

/// Per-stage + total latency percentiles over the rolling window. Every field is
/// MEASURED milliseconds; `n` is how many turns are in the window. `n == 0` means
/// no turns yet — the aggregate is `awaiting turns`, never a fabricated value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LatencyAggregate {
    pub n: usize,
    pub total_p50_ms: u64,
    pub total_p95_ms: u64,
    pub queue_p50_ms: u64,
    pub queue_p95_ms: u64,
    pub stt_p50_ms: u64,
    pub stt_p95_ms: u64,
    pub classify_p50_ms: u64,
    pub classify_p95_ms: u64,
    pub route_p50_ms: u64,
    pub route_p95_ms: u64,
}

/// Rolling token SUMS over the window + the dollar ESTIMATE derived from them.
/// `n` is how many cloud turns contributed; `n == 0` means no cloud usage yet
/// (`awaiting turns`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostAggregate {
    pub n: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    /// SUM of all three token classes — the headline "tokens this window".
    pub total_tokens: u64,
    /// Dollar ESTIMATE from the published $/1M rates (a labelled estimate, not a
    /// billed number).
    pub est_cost_usd: f64,
}

/// Routing accuracy + live correction rate, MEASURED from the trace store.
/// `held_out_n == 0` means there were no usable traces to carve a held-out split
/// from (`awaiting turns`); `usable_n == 0` means no usable traces for a
/// correction rate yet.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AccuracyAggregate {
    /// Held-out routing accuracy of the baseline config (0..=1), or `None` when
    /// there are no usable traces to carve a held-out split from.
    pub routing_accuracy: Option<f64>,
    /// Number of held-out traces the accuracy was measured over.
    pub held_out_n: usize,
    /// Correction rate = corrected-next-turn / usable (0..=1), or `None` when
    /// there are no usable traces yet.
    pub correction_rate: Option<f64>,
    /// CorrectedNextTurn traces in the corpus.
    pub corrections: usize,
    /// Usable (Success + CorrectedNextTurn) traces — the correction-rate
    /// denominator.
    pub usable_n: usize,
}

// ---------------------------------------------------------------------------
// Percentile — nearest-rank, on a sorted copy
// ---------------------------------------------------------------------------

/// Nearest-rank percentile of `samples` for `p` in 0..=100. Returns 0 on an empty
/// slice (the caller treats `n == 0` as `awaiting turns`, so a 0 here is never
/// rendered as a real value). Sorts a COPY (the window order is preserved for
/// eviction). p50 of an even count takes the lower-middle's nearest rank — a
/// deterministic, well-known choice the tests pin exactly.
fn percentile(samples: &[u64], p: u8) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    // Nearest-rank: rank = ceil(p/100 * n), clamped to [1, n], 1-indexed.
    let n = sorted.len();
    let rank = (((p as f64) / 100.0) * (n as f64)).ceil() as usize;
    let idx = rank.clamp(1, n) - 1;
    sorted[idx]
}

// ---------------------------------------------------------------------------
// Rolling state — bounded in-memory ring buffers
// ---------------------------------------------------------------------------

/// The live eval scorecard's mutable state: bounded rolling windows of measured
/// latencies and cloud token usage. Held by the daemon for its lifetime and
/// updated per turn. Accuracy is NOT stored here — it is recomputed on demand
/// from the durable trace store (the source of truth for routing outcomes).
#[derive(Debug, Default)]
pub struct EvalState {
    latencies: VecDeque<PipelineTiming>,
    /// Parallel to `latencies`: the end-to-end total_ms for the same turn.
    totals: VecDeque<u64>,
    usage: VecDeque<TokenUsage>,
}

impl EvalState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one turn's MEASURED pipeline timing + end-to-end total, evicting the
    /// oldest sample past [`EVAL_WINDOW`]. `timing` and `total_ms` come straight
    /// from the turn loop's existing clocks — nothing is estimated.
    pub fn record_latency(&mut self, timing: PipelineTiming, total_ms: u64) {
        self.latencies.push_back(timing);
        self.totals.push_back(total_ms);
        while self.latencies.len() > EVAL_WINDOW {
            self.latencies.pop_front();
        }
        while self.totals.len() > EVAL_WINDOW {
            self.totals.pop_front();
        }
    }

    /// Record one cloud turn's MEASURED token usage, evicting the oldest past
    /// [`EVAL_WINDOW`]. On-device-only turns (no cloud call) never call this.
    ///
    /// LIVE COST FEED: this is the live entry point for cost. It is now driven in
    /// production by [`record_cloud_usage`], which the cloud reply path
    /// (`anthropic.rs`) calls per cloud ROUND-TRIP after parsing the response's
    /// `usage` block via [`extract_token_usage`] — each tool-loop round, plus the
    /// plain and persona completions. A fresh daemon still reads `awaiting turns`
    /// until a real cloud call surfaces a `usage` block (runtime-gated on real
    /// cloud calls); the aggregation MATH is proven by this module's hermetic
    /// tests on synthetic usage. AGGREGATE-ONLY: only token COUNTS enter here, no
    /// utterance / response content — no PII.
    pub fn record_usage(&mut self, usage: TokenUsage) {
        self.usage.push_back(usage);
        while self.usage.len() > EVAL_WINDOW {
            self.usage.pop_front();
        }
    }

    /// Compute the latency percentiles over the current window. `n == 0` => an
    /// all-zero aggregate the caller renders as `awaiting turns`.
    pub fn latency(&self) -> LatencyAggregate {
        let n = self.totals.len();
        if n == 0 {
            return LatencyAggregate::default();
        }
        let totals: Vec<u64> = self.totals.iter().copied().collect();
        let queue: Vec<u64> = self.latencies.iter().map(|t| t.queue_ms).collect();
        let stt: Vec<u64> = self.latencies.iter().map(|t| t.stt_ms).collect();
        let classify: Vec<u64> = self.latencies.iter().map(|t| t.classify_ms).collect();
        let route: Vec<u64> = self.latencies.iter().map(|t| t.route_ms).collect();
        LatencyAggregate {
            n,
            total_p50_ms: percentile(&totals, 50),
            total_p95_ms: percentile(&totals, 95),
            queue_p50_ms: percentile(&queue, 50),
            queue_p95_ms: percentile(&queue, 95),
            stt_p50_ms: percentile(&stt, 50),
            stt_p95_ms: percentile(&stt, 95),
            classify_p50_ms: percentile(&classify, 50),
            classify_p95_ms: percentile(&classify, 95),
            route_p50_ms: percentile(&route, 50),
            route_p95_ms: percentile(&route, 95),
        }
    }

    /// Compute the rolling token SUMS + dollar ESTIMATE over the current window
    /// using `rates`. `n == 0` => an all-zero aggregate (`awaiting turns`).
    pub fn cost(&self, rates: CostRates) -> CostAggregate {
        let n = self.usage.len();
        if n == 0 {
            return CostAggregate::default();
        }
        let mut input = 0u64;
        let mut output = 0u64;
        let mut cache_read = 0u64;
        for u in &self.usage {
            input = input.saturating_add(u.input_tokens);
            output = output.saturating_add(u.output_tokens);
            cache_read = cache_read.saturating_add(u.cache_read_tokens);
        }
        let est_cost_usd = (input as f64) / 1_000_000.0 * rates.input_per_m
            + (output as f64) / 1_000_000.0 * rates.output_per_m
            + (cache_read as f64) / 1_000_000.0 * rates.cache_read_per_m;
        CostAggregate {
            n,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            total_tokens: input.saturating_add(output).saturating_add(cache_read),
            est_cost_usd,
        }
    }
}

// ---------------------------------------------------------------------------
// Accuracy — recomputed from the durable trace store's recent window
// ---------------------------------------------------------------------------

/// Compute routing accuracy + correction rate from a recent trace window. Routing
/// accuracy reuses [`optimize::baseline_held_out_score`] (the SAME held-out carve
/// + scorer the optimizer judges candidates on); the correction rate counts
///   CorrectedNextTurn over all usable (Success + CorrectedNextTurn) traces. Both
///   are `None` when there is no data for them — `awaiting turns`, never fabricated.
pub fn accuracy_from_traces(traces: &[Trace]) -> AccuracyAggregate {
    let held = optimize::baseline_held_out_score(traces);
    let (routing_accuracy, held_out_n) = match held {
        Some(score) => (
            Some(score.accuracy()),
            score.success_total + score.corrected_total,
        ),
        None => (None, 0),
    };

    let mut corrections = 0usize;
    let mut usable_n = 0usize;
    for t in traces {
        match t.outcome {
            Outcome::Success => usable_n += 1,
            Outcome::CorrectedNextTurn => {
                usable_n += 1;
                corrections += 1;
            }
            Outcome::Failed | Outcome::Unknown => {}
        }
    }
    let correction_rate = if usable_n == 0 {
        None
    } else {
        Some(corrections as f64 / usable_n as f64)
    };

    AccuracyAggregate {
        routing_accuracy,
        held_out_n,
        correction_rate,
        corrections,
        usable_n,
    }
}

// ---------------------------------------------------------------------------
// Report snapshot + telemetry — AGGREGATE-ONLY, no PII
// ---------------------------------------------------------------------------

/// Render a `f64` 0..=1 metric as a JSON value, or the string `"awaiting turns"`
/// when there is no data — so a metric with no measurement is NEVER a fabricated
/// number, it honestly reads "awaiting turns".
fn metric_or_awaiting(v: Option<f64>) -> Value {
    match v {
        Some(x) => json!((x * 1000.0).round() / 1000.0),
        None => json!("awaiting turns"),
    }
}

/// Build the AGGREGATE-ONLY eval snapshot the HUD's Eval/Optimizer panel renders
/// and the `eval.report` telemetry carries. Contains ONLY percentiles, sums,
/// rates, counts, and the honest optimizer posture — no utterance, no per-turn
/// row, no identifier. `optimizer_enabled`/`optimizer_mode` describe the
/// PROPOSE-ONLY optimizer (ships ON; mode stays "propose" — there is no
/// auto-apply-to-live path) so the panel's copy is grounded in the real config (the
/// eval framework NEVER changes that posture).
pub fn report_snapshot(
    latency: &LatencyAggregate,
    cost: &CostAggregate,
    accuracy: &AccuracyAggregate,
    optimizer_enabled: bool,
    optimizer_mode: &str,
) -> Value {
    // Latency: a measured value only when the window has turns; else awaiting.
    let latency_json = if latency.n == 0 {
        json!({"status": "awaiting turns", "n": 0})
    } else {
        json!({
            "status": "measured",
            "n": latency.n,
            "total_p50_ms": latency.total_p50_ms,
            "total_p95_ms": latency.total_p95_ms,
            "queue_p50_ms": latency.queue_p50_ms,
            "queue_p95_ms": latency.queue_p95_ms,
            "stt_p50_ms": latency.stt_p50_ms,
            "stt_p95_ms": latency.stt_p95_ms,
            "classify_p50_ms": latency.classify_p50_ms,
            "classify_p95_ms": latency.classify_p95_ms,
            "route_p50_ms": latency.route_p50_ms,
            "route_p95_ms": latency.route_p95_ms,
        })
    };

    let cost_json = if cost.n == 0 {
        json!({"status": "awaiting turns", "n": 0})
    } else {
        json!({
            "status": "measured",
            "n": cost.n,
            "input_tokens": cost.input_tokens,
            "output_tokens": cost.output_tokens,
            "cache_read_tokens": cost.cache_read_tokens,
            "total_tokens": cost.total_tokens,
            // Always LABELLED an estimate (published $/1M multiplier, not billed).
            "est_cost_usd": (cost.est_cost_usd * 10000.0).round() / 10000.0,
            "cost_is_estimate": true,
        })
    };

    let accuracy_json = json!({
        "routing_accuracy": metric_or_awaiting(accuracy.routing_accuracy),
        "held_out_n": accuracy.held_out_n,
        "correction_rate": metric_or_awaiting(accuracy.correction_rate),
        "corrections": accuracy.corrections,
        "usable_n": accuracy.usable_n,
    });

    json!({
        "latency": latency_json,
        "cost": cost_json,
        "accuracy": accuracy_json,
        "optimizer": {
            // Honest posture: PROPOSE-ONLY (ships ON; mode stays "propose"). The eval
            // framework only MEASURES; it never tunes anything.
            "enabled": optimizer_enabled,
            "mode": optimizer_mode,
            "posture": "propose-only",
        },
        // LIVE latency/cost are fed by REAL turns/cloud calls — runtime-gated.
        // The math is proven hermetically; the live feed needs real turns.
        "runtime_gated": ["latency", "cost"],
    })
}

/// Emit the AGGREGATE-ONLY `eval.report` telemetry envelope (no PII) so the HUD's
/// Eval/Optimizer panel can render the live scorecard. Fire-and-forget through
/// the existing telemetry hub; dropped silently when no HUD is connected.
pub fn emit_report(
    latency: &LatencyAggregate,
    cost: &CostAggregate,
    accuracy: &AccuracyAggregate,
    optimizer_enabled: bool,
    optimizer_mode: &str,
) {
    let snapshot = report_snapshot(
        latency,
        cost,
        accuracy,
        optimizer_enabled,
        optimizer_mode,
    );
    telemetry::emit("system", "eval.report", snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimize::Trace;

    // -----------------------------------------------------------------------
    // LATENCY — synthetic durations, assert p50/p95
    // -----------------------------------------------------------------------

    fn timing(queue: u64, stt: u64, classify: u64, route: u64) -> PipelineTiming {
        PipelineTiming {
            queue_ms: queue,
            stt_ms: stt,
            classify_ms: classify,
            route_ms: route,
            first_audio_ms: None,
            speak_ms: 0,
        }
    }

    #[test]
    fn percentile_nearest_rank_known_answers() {
        // 1..=10: p50 nearest-rank = ceil(0.5*10)=5th -> value 5; p95 = ceil(9.5)
        // = 10th -> value 10; p100 -> 10; p0 clamps to rank 1 -> value 1.
        let xs: Vec<u64> = (1..=10).collect();
        assert_eq!(percentile(&xs, 50), 5);
        assert_eq!(percentile(&xs, 95), 10);
        assert_eq!(percentile(&xs, 100), 10);
        assert_eq!(percentile(&xs, 0), 1);
        // Empty -> 0 (caller treats n==0 as awaiting turns).
        assert_eq!(percentile(&[], 50), 0);
    }

    #[test]
    fn latency_p50_p95_over_synthetic_window() {
        let mut st = EvalState::new();
        // total_ms 100,200,...,1000 (10 turns). p50 -> 5th smallest = 500,
        // p95 -> 10th = 1000. Per-stage echo a known constant so we pin them too.
        for i in 1..=10u64 {
            st.record_latency(timing(i, 2 * i, 3 * i, 4 * i), i * 100);
        }
        let agg = st.latency();
        assert_eq!(agg.n, 10);
        assert_eq!(agg.total_p50_ms, 500);
        assert_eq!(agg.total_p95_ms, 1000);
        // queue = 1..=10 -> p50 5, p95 10; route = 4..=40 by 4 -> p50 20, p95 40.
        assert_eq!(agg.queue_p50_ms, 5);
        assert_eq!(agg.queue_p95_ms, 10);
        assert_eq!(agg.route_p50_ms, 20);
        assert_eq!(agg.route_p95_ms, 40);
    }

    #[test]
    fn latency_empty_is_awaiting_not_fabricated() {
        let st = EvalState::new();
        let agg = st.latency();
        assert_eq!(agg, LatencyAggregate::default());
        assert_eq!(agg.n, 0);
        // The snapshot reports awaiting turns, never a value.
        let snap = report_snapshot(
            &agg,
            &CostAggregate::default(),
            &AccuracyAggregate::default(),
            false,
            "propose",
        );
        assert_eq!(snap["latency"]["status"], "awaiting turns");
        assert_eq!(snap["cost"]["status"], "awaiting turns");
        assert_eq!(snap["accuracy"]["routing_accuracy"], "awaiting turns");
        assert_eq!(snap["accuracy"]["correction_rate"], "awaiting turns");
    }

    #[test]
    fn latency_window_evicts_oldest_past_cap() {
        let mut st = EvalState::new();
        // Fill past the cap with a tiny value, then push EVAL_WINDOW big values:
        // the small ones must be evicted, so the window is all-big.
        for _ in 0..(EVAL_WINDOW + 50) {
            st.record_latency(timing(1, 1, 1, 1), 1);
        }
        for _ in 0..EVAL_WINDOW {
            st.record_latency(timing(900, 900, 900, 900), 900);
        }
        let agg = st.latency();
        assert_eq!(agg.n, EVAL_WINDOW);
        assert_eq!(agg.total_p50_ms, 900);
        assert_eq!(agg.total_p95_ms, 900);
    }

    // -----------------------------------------------------------------------
    // COST — synthetic usage, assert token sums + estimate
    // -----------------------------------------------------------------------

    #[test]
    fn cost_token_sums_and_estimate() {
        let mut st = EvalState::new();
        // 3 cloud turns. input sum 600, output 60, cache_read 6000.
        st.record_usage(TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 1000,
        });
        st.record_usage(TokenUsage {
            input_tokens: 200,
            output_tokens: 20,
            cache_read_tokens: 2000,
        });
        st.record_usage(TokenUsage {
            input_tokens: 300,
            output_tokens: 30,
            cache_read_tokens: 3000,
        });
        let rates = CostRates {
            input_per_m: 3.0,
            output_per_m: 15.0,
            cache_read_per_m: 0.3,
        };
        let agg = st.cost(rates);
        assert_eq!(agg.n, 3);
        assert_eq!(agg.input_tokens, 600);
        assert_eq!(agg.output_tokens, 60);
        assert_eq!(agg.cache_read_tokens, 6000);
        assert_eq!(agg.total_tokens, 6660);
        // est = 600/1e6*3 + 60/1e6*15 + 6000/1e6*0.3
        //     = 0.0018 + 0.0009 + 0.0018 = 0.0045
        let expected = 600.0 / 1e6 * 3.0 + 60.0 / 1e6 * 15.0 + 6000.0 / 1e6 * 0.3;
        assert!((agg.est_cost_usd - expected).abs() < 1e-12);
        assert!((agg.est_cost_usd - 0.0045).abs() < 1e-9);
    }

    #[test]
    fn cost_empty_is_awaiting() {
        let st = EvalState::new();
        let agg = st.cost(CostRates::default());
        assert_eq!(agg, CostAggregate::default());
        assert_eq!(agg.n, 0);
        assert_eq!(agg.total_tokens, 0);
    }

    #[test]
    fn cost_estimate_is_labelled_estimate_in_snapshot() {
        let mut st = EvalState::new();
        st.record_usage(TokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            cache_read_tokens: 0,
        });
        let agg = st.cost(CostRates::default());
        // 1M input @ $3/M = $3.00 exactly.
        assert!((agg.est_cost_usd - 3.0).abs() < 1e-9);
        let snap = report_snapshot(
            &LatencyAggregate::default(),
            &agg,
            &AccuracyAggregate::default(),
            false,
            "propose",
        );
        assert_eq!(snap["cost"]["status"], "measured");
        assert_eq!(snap["cost"]["cost_is_estimate"], true);
        assert_eq!(snap["cost"]["total_tokens"], 1_000_000);
    }

    // -----------------------------------------------------------------------
    // LIVE COST FEED — extract_token_usage + record_cloud_usage (HERMETIC)
    //
    // Proves the cloud-reply -> cost-window path WITHOUT any cloud call: a
    // SYNTHETIC Anthropic response `Value` carrying a `usage` block is fed
    // through extract_token_usage + record_cloud_usage, and the cost aggregate is
    // asserted to reflect N/M/K (token sums + estimated dollars). Also pins the
    // no-op-on-missing/malformed guarantee and the no-content/no-PII guarantee.
    // -----------------------------------------------------------------------

    /// A synthetic Messages API response carrying a `usage` block AND `content`
    /// text — exactly the shape the real cloud reply path parses. The `content`
    /// text is present specifically to prove it can NEVER leak into the usage
    /// path (the no-PII guarantee).
    fn synthetic_resp_with_usage(input: u64, output: u64, cache_read: u64) -> Value {
        json!({
            "id": "msg_synthetic",
            "type": "message",
            "role": "assistant",
            "stop_reason": "end_turn",
            "content": [{"type": "text", "text": "secret utterance echo SHOULD NOT LEAK"}],
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": cache_read,
                // A field the cost window does NOT consume — present to prove the
                // extractor reads only the three counters it claims to.
                "cache_creation_input_tokens": 4242,
            },
        })
    }

    #[test]
    fn extract_token_usage_parses_synthetic_usage_block() {
        let resp = synthetic_resp_with_usage(123, 45, 6789);
        let usage = extract_token_usage(&resp).expect("usage block present -> Some");
        assert_eq!(usage.input_tokens, 123);
        assert_eq!(usage.output_tokens, 45);
        assert_eq!(usage.cache_read_tokens, 6789);
    }

    #[test]
    fn extract_token_usage_missing_or_malformed_is_none_noop() {
        // Whole usage object absent -> None (a safe no-op, not a fake zero turn).
        let no_usage = json!({"stop_reason": "end_turn", "content": []});
        assert_eq!(extract_token_usage(&no_usage), None);
        // usage present but NOT an object (malformed) -> None.
        let bad_usage = json!({"usage": "not-an-object"});
        assert_eq!(extract_token_usage(&bad_usage), None);
        // usage object present but counters missing/non-integer -> counted turn,
        // each absent/garbage counter reads 0 (the field simply did not bill).
        let partial = json!({"usage": {"input_tokens": 10, "output_tokens": "oops"}});
        let u = extract_token_usage(&partial).expect("usage object present -> Some");
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.cache_read_tokens, 0);
    }

    #[test]
    fn extract_token_usage_carries_no_content_no_pii() {
        // The extractor's output is three integers — it is STRUCTURALLY incapable
        // of carrying utterance/response text. Prove a usage block sitting next to
        // a content block surfaces ONLY the numbers, never the text.
        let resp = synthetic_resp_with_usage(7, 8, 9);
        let usage = extract_token_usage(&resp).expect("Some");
        // TokenUsage is a Copy struct of three u64s; serialize the recorded shape
        // and assert the leak-canary content string is nowhere in it.
        let st_json = json!({
            "input_tokens": usage.input_tokens,
            "output_tokens": usage.output_tokens,
            "cache_read_tokens": usage.cache_read_tokens,
        })
        .to_string();
        assert!(!st_json.contains("SHOULD NOT LEAK"));
        assert!(!st_json.contains("utterance"));
    }

    #[tokio::test]
    async fn live_feed_drives_cost_window_from_synthetic_resp() {
        // Drive the REAL production feed path end-to-end, hermetically: install
        // the daemon's EvalState as the global sink, then push synthetic cloud
        // responses through record_cloud_usage (the SAME function anthropic.rs
        // calls at each cloud round-trip) and assert the cost aggregate reflects
        // the summed token counts + the estimated dollars.
        //
        // NOTE: the global USAGE_SINK is an OnceLock (first-install wins), so ALL
        // live-feed assertions live in this ONE test to avoid a cross-test race on
        // the process-global. The pure extract_token_usage tests above need no
        // sink and stay independent.
        let state = Arc::new(Mutex::new(EvalState::new()));
        install_usage_sink(state.clone());

        // Three cloud round-trips (e.g. two tool-loop rounds + a final), each with
        // its own usage block. input sum 600, output 60, cache_read 6000.
        record_cloud_usage(&synthetic_resp_with_usage(100, 10, 1000)).await;
        record_cloud_usage(&synthetic_resp_with_usage(200, 20, 2000)).await;
        record_cloud_usage(&synthetic_resp_with_usage(300, 30, 3000)).await;

        // A usage-less / malformed reply MUST be a no-op: it adds no turn.
        record_cloud_usage(&json!({"stop_reason": "end_turn", "content": []})).await;
        record_cloud_usage(&json!({"usage": "garbage"})).await;

        let rates = CostRates {
            input_per_m: 3.0,
            output_per_m: 15.0,
            cache_read_per_m: 0.3,
        };
        let agg = state.lock().await.cost(rates);
        // Exactly the 3 well-formed turns landed (the 2 malformed were no-ops).
        assert_eq!(agg.n, 3);
        assert_eq!(agg.input_tokens, 600);
        assert_eq!(agg.output_tokens, 60);
        assert_eq!(agg.cache_read_tokens, 6000);
        assert_eq!(agg.total_tokens, 6660);
        // est = 600/1e6*3 + 60/1e6*15 + 6000/1e6*0.3 = 0.0045 (a labelled estimate).
        assert!((agg.est_cost_usd - 0.0045).abs() < 1e-9);

        // NO-PII: render the aggregate snapshot and assert the leak-canary content
        // string from the synthetic `content` block is nowhere in the cost path.
        let snap = report_snapshot(
            &LatencyAggregate::default(),
            &agg,
            &AccuracyAggregate::default(),
            false,
            "propose",
        );
        let s = snap.to_string();
        assert!(!s.contains("SHOULD NOT LEAK"));
        assert!(!s.contains("utterance"));
        assert_eq!(snap["cost"]["status"], "measured");
        assert_eq!(snap["cost"]["cost_is_estimate"], true);
    }

    // -----------------------------------------------------------------------
    // ACCURACY — synthetic traces, assert score_config accuracy + correction rate
    // -----------------------------------------------------------------------

    /// A redacted-form trace built directly (test-facing — the utterance is
    /// already safe). `cue` is the routing cue word; `agent` the recorded pick.
    fn trace(cue: &str, agent: &str, outcome: Outcome) -> Trace {
        Trace {
            utterance_redacted: cue.to_string(),
            intent: "action".to_string(),
            agent: agent.to_string(),
            mode: "do".to_string(),
            tool_or_skill: String::new(),
            outcome,
            latency_ms: 100,
            ts: 0,
        }
    }

    #[test]
    fn accuracy_correction_rate_from_synthetic_traces() {
        // 8 Success + 2 CorrectedNextTurn = 10 usable. correction rate = 2/10.
        let mut traces = Vec::new();
        for _ in 0..8 {
            traces.push(trace("hello there", "darwin", Outcome::Success));
        }
        traces.push(trace("switch agent now", "gecko", Outcome::CorrectedNextTurn));
        traces.push(trace("no the other one", "midas", Outcome::CorrectedNextTurn));
        // A couple of non-usable traces must NOT count in the denominator.
        traces.push(trace("errored", "darwin", Outcome::Failed));
        traces.push(trace("ambiguous", "darwin", Outcome::Unknown));

        let agg = accuracy_from_traces(&traces);
        assert_eq!(agg.usable_n, 10);
        assert_eq!(agg.corrections, 2);
        let rate = agg.correction_rate.expect("usable traces -> a rate");
        assert!((rate - 0.2).abs() < 1e-12);
    }

    #[test]
    fn accuracy_routing_matches_score_config_on_held_out() {
        // Build a corpus where the baseline replay-router gets every Success
        // right: use utterances with NO routing cue, which the baseline routes to
        // the orchestrator "darwin"; record them as darwin Success. The held-out
        // accuracy must then equal score_config's accuracy on the held-out split.
        let mut traces = Vec::new();
        for _ in 0..20 {
            traces.push(trace("plain words only", "darwin", Outcome::Success));
        }
        let agg = accuracy_from_traces(&traces);
        // Held-out carve is non-empty for 20 usable traces.
        assert!(agg.held_out_n > 0);
        let routing = agg.routing_accuracy.expect("held-out -> accuracy");
        // Cross-check directly against the optimize primitive: the eval number
        // is EXACTLY score_config's held-out accuracy, not a re-implementation.
        let direct = optimize::baseline_held_out_score(&traces)
            .expect("held-out split")
            .accuracy();
        assert!((routing - direct).abs() < 1e-12);
        // No-cue utterances all route to darwin -> every Success reproduced -> 1.0.
        assert!((routing - 1.0).abs() < 1e-12);
    }

    #[test]
    fn accuracy_empty_corpus_is_awaiting_not_fabricated() {
        // The true "no data" case is an EMPTY corpus: split_usable yields an empty
        // held-out split, so routing accuracy is None ("awaiting turns") and the
        // correction rate is None (no usable denominator) — never a fabricated 0.
        let empty = accuracy_from_traces(&[]);
        assert_eq!(empty.routing_accuracy, None);
        assert_eq!(empty.held_out_n, 0);
        assert_eq!(empty.correction_rate, None);
        assert_eq!(empty.usable_n, 0);
        assert_eq!(empty.corrections, 0);
    }

    #[test]
    fn accuracy_single_usable_trace_is_thin_but_measured() {
        // A single usable trace carves a thin held-out split of one (split_usable
        // clamps held_n to >=1). That is a thin-but-HONESTLY-MEASURED number, not a
        // fabricated value: routing accuracy is Some over held_out_n == 1, and the
        // correction rate over the one usable trace is 0.0 (a Success, no
        // correction). It cross-checks exactly against the optimize primitive.
        let traces = vec![trace("plain words only", "darwin", Outcome::Success)];
        let agg = accuracy_from_traces(&traces);
        assert_eq!(agg.held_out_n, 1);
        let routing = agg.routing_accuracy.expect("a thin held-out -> a measured value");
        let direct = optimize::baseline_held_out_score(&traces)
            .expect("held-out split")
            .accuracy();
        assert!((routing - direct).abs() < 1e-12);
        // No-cue utterance routes to darwin, the recorded pick -> reproduced -> 1.0.
        assert!((routing - 1.0).abs() < 1e-12);
        assert_eq!(agg.correction_rate, Some(0.0));
        assert_eq!(agg.usable_n, 1);
    }

    // -----------------------------------------------------------------------
    // REPORT — aggregate-only, no PII; optimizer posture honest
    // -----------------------------------------------------------------------

    #[test]
    fn report_snapshot_is_aggregate_only_and_honest() {
        let mut st = EvalState::new();
        for i in 1..=10u64 {
            st.record_latency(timing(i, i, i, i), i * 10);
        }
        st.record_usage(TokenUsage {
            input_tokens: 500,
            output_tokens: 50,
            cache_read_tokens: 5000,
        });
        let traces: Vec<Trace> = (0..20)
            .map(|_| trace("plain words only", "darwin", Outcome::Success))
            .collect();
        let lat = st.latency();
        let cost = st.cost(CostRates::default());
        let acc = accuracy_from_traces(&traces);
        let snap = report_snapshot(&lat, &cost, &acc, false, "propose");

        // Optimizer posture is honestly OFF + propose-only.
        assert_eq!(snap["optimizer"]["enabled"], false);
        assert_eq!(snap["optimizer"]["posture"], "propose-only");
        assert_eq!(snap["optimizer"]["mode"], "propose");
        // Runtime-gated metrics are honestly flagged.
        assert_eq!(snap["runtime_gated"][0], "latency");
        assert_eq!(snap["runtime_gated"][1], "cost");
        // Measured sections present.
        assert_eq!(snap["latency"]["status"], "measured");
        assert_eq!(snap["cost"]["status"], "measured");
        // The whole snapshot serializes with NO utterance text anywhere (no PII):
        // the only strings are our known aggregate keys/labels.
        let s = snap.to_string();
        assert!(!s.contains("plain words only"));
        assert!(!s.contains("hello"));
    }

    #[test]
    fn emit_report_does_not_panic_without_hud() {
        // Fire-and-forget: with no HUD subscribed the emit is a silent no-op.
        let st = EvalState::new();
        emit_report(
            &st.latency(),
            &st.cost(CostRates::default()),
            &accuracy_from_traces(&[]),
            false,
            "propose",
        );
    }
}
