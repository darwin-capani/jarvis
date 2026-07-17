//! PLUMBLINE — an HONEST confidence-calibration self-report.
//!
//! DARWIN's classifier attaches a `confidence` to every turn (the number the
//! router already reads to decide the model tier + the low-confidence-to-cloud /
//! clarify contract). PLUMBLINE asks the one question that number invites: *when
//! DARWIN says it is 80% sure, is it actually right 80% of the time?* It folds the
//! outcome-labelled turns into a RELIABILITY CURVE (per predicted-confidence
//! bucket: how often DARWIN actually succeeded) and a single scalar
//! over/under-confidence GAP (ECE-style), so miscalibration is measured, not felt.
//!
//! WHAT A TURN'S OUTCOME MEANS (identical convention to [`crate::attribution`], so
//! "success" means the same thing everywhere): a turn is GRADED iff its outcome is
//! definite (`Success` / `CorrectedNextTurn` / `Failed`); an `Unknown` turn carries
//! no signal and is EXCLUDED from every rate (counted separately, never masqueraded
//! as a success or a failure). `CorrectedNextTurn` and `Failed` both count AGAINST
//! success — a turn the user had to correct did not work.
//!
//! HONEST STATISTICS (the whole point — mirrors attribution's sample-size floor):
//!   * A confidence bucket with fewer than `min_sample` GRADED turns is reported
//!     "insufficient data" — never scored good or bad on a handful of turns, and
//!     EXCLUDED from the ECE / gap so a thin bucket can never move the headline
//!     number. `coverage` surfaces how much of the corpus was actually judgeable.
//!   * The ECE / gap are `None` (not `0.0`) when nothing is judgeable yet — an
//!     empty corpus reports "no evidence", it never fabricates a calibrated-looking
//!     zero.
//!
//! PURITY (fully unit-testable): the whole analysis — [`reliability`], [`ece`-style
//! math on `Reliability`], the [`adjusted_clarify_threshold`] hook — is PURE over
//! INJECTED `&[Sample]` / config. No I/O, no clock, no store lives in the core, so
//! every property (binning, the ECE math, the MIN_SAMPLE floor, the reduce-only
//! hook) is proven on synthetic samples. The only runtime state is a bounded,
//! in-memory [`CalibrationState`] rolling window (mirroring `eval`'s posture: the
//! trace store is the durable corpus, this is a live rolling view) fed per-turn
//! alongside the optimizer's own trace recording.
//!
//! REDUCE-ONLY ROUTING HOOK (gated OFF; the first landing is pure analytics):
//! [`adjusted_clarify_threshold`] lets the router WIDEN its clarify band — ask MORE
//! clarifying questions — ONLY in a confidence bucket PLUMBLINE measured as
//! overconfident. It can only ever RAISE the threshold (never lower it), so the
//! worst it can do is make DARWIN ask a question it need not have; it can NEVER make
//! DARWIN act more boldly. It is inert unless `[calibrate].influence_routing` is
//! set, so the shipped default is byte-for-byte today's routing.
//!
//! SECRET-FREE: a `Sample` is `{confidence: f64, outcome}` — a number and an enum.
//! No utterance, no identifier, no content ever enters this module or the emitted
//! `calibrate.report` frame.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::{CalibrateConfig, Config};
use crate::optimize::Outcome;
use crate::telemetry;

/// Default reliability resolution — deciles. Ten buckets is the honest sweet spot:
/// fine enough to see a miscalibration bend, coarse enough that each bucket can
/// reach `min_sample` on a real corpus.
pub const DEFAULT_BINS: usize = 10;

/// Default MIN_SAMPLE floor per bucket. Below this many GRADED turns a bucket is
/// "insufficient data" and is excluded from the ECE / gap. Higher than
/// attribution's flat floor (5) because a decile is a NARROWER slice — judging a
/// single decile's calibration on five turns would be noise dressed as a verdict.
pub const DEFAULT_MIN_SAMPLE: usize = 20;

/// Bounded in-memory rolling window of recent (confidence, outcome) samples. Large
/// enough to fill several deciles past the MIN_SAMPLE floor, tiny in RAM (~24 bytes
/// per entry). Past this the OLDEST sample is evicted — the report reflects the
/// RECENT run, exactly like `eval`'s rolling window (the durable corpus is the
/// trace store; this is a live view).
const CALIBRATE_WINDOW: usize = 4_000;

/// Report cadence (runtime-only; never run in tests). A startup delay keeps it out
/// of the first exchanges; a slow tick since the curve only shifts as turns accrue.
const REPORT_STARTUP_DELAY: Duration = Duration::from_secs(40);
const REPORT_INTERVAL: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Sample — the injected unit of the pure analysis
// ---------------------------------------------------------------------------

/// One outcome-labelled turn's calibration signal: the predicted `confidence` the
/// classifier attached, paired with how the turn actually went. Secret-free by
/// construction — a float and an enum, no content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// The classifier's predicted confidence for the turn (expected in [0, 1];
    /// clamped defensively before binning so a stray out-of-range / non-finite
    /// value can never panic or skew a bucket).
    pub confidence: f64,
    /// How the turn went — the label the actual-success rate is computed from.
    pub outcome: Outcome,
}

impl Sample {
    pub fn new(confidence: f64, outcome: Outcome) -> Self {
        Self { confidence, outcome }
    }

    /// Whether this turn has a DEFINITE outcome (the rate's denominator). `Unknown`
    /// is not graded — it is counted but excluded from every rate.
    fn is_graded(&self) -> bool {
        !matches!(self.outcome, Outcome::Unknown)
    }

    /// Whether this turn SUCCEEDED. Only `Success` counts; `CorrectedNextTurn` and
    /// `Failed` are graded-but-not-success (they count against the rate).
    fn is_success(&self) -> bool {
        matches!(self.outcome, Outcome::Success)
    }

    /// Confidence pinned to [0, 1] (and non-finite -> 0.0) for the mean + binning.
    fn clamped_confidence(&self) -> f64 {
        clamp_unit(self.confidence)
    }
}

/// Clamp a confidence to the unit interval, mapping a non-finite value to 0.0
/// (the most conservative bucket) rather than propagating a NaN into a bin index
/// or a bucket mean.
fn clamp_unit(x: f64) -> f64 {
    if x.is_finite() {
        x.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The bucket index for a confidence under `bins` equal-width bins over [0, 1].
/// `bins` is treated as at least 1; a confidence of exactly 1.0 lands in the last
/// bucket (right edge inclusive on the top bin only). PURE.
fn bin_of(confidence: f64, bins: usize) -> usize {
    let bins = bins.max(1);
    let c = clamp_unit(confidence);
    let idx = (c * bins as f64) as usize;
    idx.min(bins - 1)
}

// ---------------------------------------------------------------------------
// Reliability curve — the pure fold
// ---------------------------------------------------------------------------

/// One predicted-confidence bucket of the reliability curve. `predicted` is the
/// MEAN confidence DARWIN CLAIMED for the turns that fell here; `actual` is how
/// often it was actually right — the gap between them is the miscalibration.
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    /// Inclusive lower edge of the bucket's confidence range.
    pub lo: f64,
    /// Exclusive upper edge (inclusive on the top bucket).
    pub hi: f64,
    /// GRADED turns in this bucket (the rate's denominator; excludes `Unknown`).
    pub graded: usize,
    /// Of the graded turns, how many actually succeeded.
    pub successes: usize,
    /// Mean CLAIMED confidence of the graded turns here (`None` when the bucket is
    /// empty). The x-position of the reliability point.
    pub predicted: Option<f64>,
    /// ACTUAL success rate — `Some` ONLY when the bucket clears the MIN_SAMPLE
    /// floor. `None` == "insufficient data": the bucket is reported, never judged.
    pub actual: Option<f64>,
    /// Whether this bucket cleared the MIN_SAMPLE floor (i.e. `actual.is_some()`).
    pub judged: bool,
}

/// The full reliability analysis over a corpus of samples — the reviewable curve
/// plus the honest bookkeeping (what was judgeable, what was excluded).
#[derive(Debug, Clone, PartialEq)]
pub struct Reliability {
    /// One entry per bin, in ascending confidence order.
    pub buckets: Vec<Bucket>,
    /// Number of bins the curve was computed over.
    pub bins: usize,
    /// The MIN_SAMPLE floor applied.
    pub min_sample: usize,
    /// Total GRADED turns across all buckets (excludes `Unknown`).
    pub graded_total: usize,
    /// Turns whose outcome was `Unknown` (counted, never rated).
    pub unknown_total: usize,
    /// GRADED turns that landed in a JUDGED (well-sampled) bucket — the denominator
    /// the ECE / gap are averaged over.
    pub judged_graded: usize,
}

impl Reliability {
    /// EXPECTED CALIBRATION ERROR: the sample-weighted mean absolute gap between
    /// claimed and actual, over the JUDGED buckets only. `None` when nothing is
    /// judgeable (an honest "no evidence", never a fabricated 0.0). In [0, 1];
    /// smaller is better-calibrated.
    pub fn ece(&self) -> Option<f64> {
        self.weighted_gap(|pred, act| (pred - act).abs())
    }

    /// The SIGNED over/under-confidence gap: sample-weighted mean of
    /// (claimed - actual) over judged buckets. POSITIVE => DARWIN is OVERconfident
    /// overall (it claims more than it delivers); NEGATIVE => underconfident. `None`
    /// when nothing is judgeable.
    pub fn gap(&self) -> Option<f64> {
        self.weighted_gap(|pred, act| pred - act)
    }

    /// Shared weighted-average machinery for [`Self::ece`] / [`Self::gap`]: average
    /// `f(predicted, actual)` over judged buckets, weighted by each bucket's graded
    /// share of the judged total. `None` when no bucket is judged.
    fn weighted_gap(&self, f: impl Fn(f64, f64) -> f64) -> Option<f64> {
        if self.judged_graded == 0 {
            return None;
        }
        let mut acc = 0.0;
        for b in &self.buckets {
            if let (true, Some(pred), Some(act)) = (b.judged, b.predicted, b.actual) {
                acc += (b.graded as f64) * f(pred, act);
            }
        }
        Some(acc / self.judged_graded as f64)
    }

    /// Fraction of graded turns that fell in a judged bucket (0.0 when nothing is
    /// graded). Surfaces how much of the corpus the headline number actually rests
    /// on — a low coverage means "treat the ECE as provisional".
    pub fn coverage(&self) -> f64 {
        if self.graded_total == 0 {
            0.0
        } else {
            self.judged_graded as f64 / self.graded_total as f64
        }
    }
}

/// COMPUTE the reliability curve over `samples` with `bins` equal-width confidence
/// buckets and a `min_sample` floor. PURE — a fold over injected data, no I/O, no
/// clock. `Unknown` samples are tallied into `unknown_total` and excluded from
/// every rate; a bucket below the floor is reported with `actual = None` ("judged =
/// false") and left out of the ECE / gap. `bins` is treated as at least 1.
pub fn reliability(samples: &[Sample], bins: usize, min_sample: usize) -> Reliability {
    let bins = bins.max(1);
    let width = 1.0 / bins as f64;

    let mut graded = vec![0usize; bins];
    let mut successes = vec![0usize; bins];
    let mut conf_sum = vec![0.0f64; bins];
    let mut unknown_total = 0usize;

    for s in samples {
        if !s.is_graded() {
            unknown_total += 1;
            continue;
        }
        let b = bin_of(s.confidence, bins);
        graded[b] += 1;
        conf_sum[b] += s.clamped_confidence();
        if s.is_success() {
            successes[b] += 1;
        }
    }

    let mut buckets = Vec::with_capacity(bins);
    let mut graded_total = 0usize;
    let mut judged_graded = 0usize;
    for i in 0..bins {
        let g = graded[i];
        graded_total += g;
        let judged = g > 0 && g >= min_sample;
        let predicted = (g > 0).then(|| conf_sum[i] / g as f64);
        let actual = judged.then(|| successes[i] as f64 / g as f64);
        if judged {
            judged_graded += g;
        }
        let hi = if i + 1 == bins { 1.0 } else { (i + 1) as f64 * width };
        buckets.push(Bucket {
            lo: i as f64 * width,
            hi,
            graded: g,
            successes: successes[i],
            predicted,
            actual,
            judged,
        });
    }

    Reliability {
        buckets,
        bins,
        min_sample,
        graded_total,
        unknown_total,
        judged_graded,
    }
}

/// Convenience: the reliability curve with a config's bins + floor.
pub fn reliability_with(samples: &[Sample], cfg: &CalibrateConfig) -> Reliability {
    reliability(samples, cfg.n_bins, cfg.min_sample)
}

// ---------------------------------------------------------------------------
// REDUCE-ONLY clarify-band hook — can only ever make DARWIN ask MORE
// ---------------------------------------------------------------------------

/// The reduce-only calibration hook the router MAY consult to WIDEN its clarify
/// band. Given the `base` clarify/low-confidence threshold (below which DARWIN asks
/// a clarifying question / escalates rather than acting on its own read), the
/// turn's `confidence`, and the measured `rel` curve, return an ADJUSTED threshold.
///
/// STRICT CONTRACT (asserted by the tests):
///   * The result is ALWAYS `>= base` — the band can only WIDEN, never narrow. The
///     worst case is DARWIN asking a clarifying question it need not have; it can
///     NEVER make DARWIN act more boldly than `base` already would.
///   * It only widens in a bucket that is JUDGED (past the MIN_SAMPLE floor) AND
///     measurably OVERconfident: actual + `overconfidence_margin` < predicted. A
///     well-calibrated, underconfident, empty, or insufficient bucket returns
///     `base` unchanged.
///   * The widen is bounded by `max_widen` and the result is capped at 1.0.
///   * REPORT-ONLY GATE: when `cfg.influence_routing` is false (the shipped
///     default) it returns `base` for EVERY input — routing is byte-for-byte
///     today's, and PLUMBLINE is pure analytics.
///
/// PURE — a function of the arguments only.
// The reduce-only routing hook: it is CONSULTED by the router only when
// `[calibrate].influence_routing` is enabled (OFF by default — the first landing is
// pure analytics), so the shipped build never calls it; it is fully exercised by the
// widen-only tests and is the documented seam the selector/model_tier may read.
#[allow(dead_code)]
pub fn adjusted_clarify_threshold(
    base: f64,
    confidence: f64,
    rel: &Reliability,
    cfg: &CalibrateConfig,
) -> f64 {
    // Report-only default: the hook is inert, routing is unchanged.
    if !cfg.influence_routing {
        return base;
    }
    let b = bin_of(confidence, rel.bins);
    let Some(bucket) = rel.buckets.get(b) else {
        return base;
    };
    // Insufficient data -> never judge, never widen.
    let (Some(pred), Some(act)) = (bucket.predicted, bucket.actual) else {
        return base;
    };
    if !bucket.judged {
        return base;
    }
    // Overconfident IFF actual success lags the claim by more than the margin.
    let margin = cfg.overconfidence_margin.max(0.0);
    if act + margin >= pred {
        return base; // well-calibrated or UNDERconfident: never widen
    }
    // Widen by the measured shortfall, bounded by max_widen and capped at 1.0.
    let widen = (pred - act).min(cfg.max_widen.max(0.0));
    let adjusted = (base + widen).min(1.0);
    // Belt-and-braces: the band can only ever WIDEN.
    if adjusted < base {
        base
    } else {
        adjusted
    }
}

// ---------------------------------------------------------------------------
// Runtime sink — a bounded, in-memory rolling window fed per turn
// ---------------------------------------------------------------------------

/// One recorded turn awaiting (or carrying) its final outcome. Keyed by the SAME
/// monotonic trace id the optimizer's trace store assigns, so a next-turn
/// correction can re-label THIS exact entry (mirroring the store's
/// `record_returning_id` + `label_outcome`).
#[derive(Debug, Clone, Copy)]
struct Entry {
    trace_id: i64,
    confidence: f64,
    outcome: Outcome,
}

/// The live calibration corpus: a bounded rolling window of recent (confidence,
/// outcome) entries. In-memory only (NOT persisted — the trace store is the durable
/// corpus; this is a live rolling view, exactly like `eval::EvalState`). The pure
/// analysis folds over [`Self::samples`]; the window is fed by the daemon's turn
/// loop. Methods are on the struct (not the global) so they are unit-tested on a
/// LOCAL instance without touching process-global state.
#[derive(Debug)]
pub struct CalibrationState {
    entries: VecDeque<Entry>,
}

impl Default for CalibrationState {
    fn default() -> Self {
        Self::new()
    }
}

impl CalibrationState {
    /// An empty window. `const` so it can seed a process-global `Mutex`.
    pub const fn new() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }

    /// Record a turn as it completes: its trace id + claimed confidence, provisional
    /// outcome `Success` (a later turn may re-label it via [`Self::relabel`]). Evicts
    /// the oldest entry past `CALIBRATE_WINDOW` so the window stays bounded.
    pub fn record(&mut self, trace_id: i64, confidence: f64) {
        self.entries.push_back(Entry {
            trace_id,
            confidence,
            outcome: Outcome::Success,
        });
        while self.entries.len() > CALIBRATE_WINDOW {
            self.entries.pop_front();
        }
    }

    /// Re-label the entry for `trace_id` (used when a next-turn correction reveals
    /// the prior turn's routing was wrong — the SAME signal the trace store gets).
    /// Scans newest-first (a correction targets a very recent turn). Returns whether
    /// an entry was found — a no-op (false) for an already-evicted / unknown id,
    /// exactly like the store's `label_outcome`.
    pub fn relabel(&mut self, trace_id: i64, outcome: Outcome) -> bool {
        for e in self.entries.iter_mut().rev() {
            if e.trace_id == trace_id {
                e.outcome = outcome;
                return true;
            }
        }
        false
    }

    /// Snapshot the window as pure [`Sample`]s for the analysis.
    pub fn samples(&self) -> Vec<Sample> {
        self.entries
            .iter()
            .map(|e| Sample::new(e.confidence, e.outcome))
            .collect()
    }
}

/// The process-global live window. Fed by the daemon's turn loop (record on
/// completion, relabel on a next-turn correction) and read by
/// [`calibrate_report_task`]. Poison-tolerant. The critical sections are tiny
/// (a push / a short scan), so a `std::sync::Mutex` is ample.
static SINK: Mutex<CalibrationState> = Mutex::new(CalibrationState::new());

/// Record a completed turn into the live window (global). No-op-safe under lock
/// poisoning.
pub fn record(trace_id: i64, confidence: f64) {
    // THRESHOLD write-integrity chokepoint: a GUEST turn leaves NO durable trace.
    // The calibration window is owner-influencing state — it shifts the OWNER's
    // clarify threshold (adjusted_clarify_threshold) — so a bystander's turn must
    // never enter it. Background tasks read false here and record as usual.
    if crate::threshold::guest_write_blocked() {
        return;
    }
    let mut s = SINK.lock().unwrap_or_else(|p| p.into_inner());
    s.record(trace_id, confidence);
}

/// Re-label a recorded turn in the live window (global) when a correction is
/// detected. Poison-tolerant.
pub fn relabel(trace_id: i64, outcome: Outcome) {
    // THRESHOLD write-integrity chokepoint: a GUEST turn mutates NO owner-influencing
    // calibration window. Background tasks read false here and relabel as usual.
    if crate::threshold::guest_write_blocked() {
        return;
    }
    let mut s = SINK.lock().unwrap_or_else(|p| p.into_inner());
    s.relabel(trace_id, outcome);
}

/// Snapshot the live window's samples (global). Poison-tolerant.
fn snapshot() -> Vec<Sample> {
    SINK.lock()
        .map(|s| s.samples())
        .unwrap_or_else(|p| p.into_inner().samples())
}

// ---------------------------------------------------------------------------
// Telemetry — the secret-free calibrate.report frame
// ---------------------------------------------------------------------------

/// Render the `calibrate.report` payload: the reliability curve (per-bucket
/// predicted/actual/counts), the scalar ECE + signed gap, and coverage. AGGREGATE
/// numbers only — no utterance, no id, nothing but floats + counts. PURE.
pub fn report_json(rel: &Reliability) -> Value {
    let buckets: Vec<Value> = rel
        .buckets
        .iter()
        .map(|b| {
            json!({
                "lo": round3(b.lo),
                "hi": round3(b.hi),
                "n": b.graded,
                "successes": b.successes,
                // predicted / actual are null (not a fake 0) when unknown / insufficient.
                "predicted": b.predicted.map(round3),
                "actual": b.actual.map(round3),
                "judged": b.judged,
            })
        })
        .collect();
    json!({
        "bins": rel.bins,
        "min_sample": rel.min_sample,
        "graded_total": rel.graded_total,
        "unknown_total": rel.unknown_total,
        "judged_graded": rel.judged_graded,
        "coverage": round3(rel.coverage()),
        // ECE / gap are null when nothing is judgeable — honest "no evidence yet".
        "ece": rel.ece().map(round3),
        "gap": rel.gap().map(round3),
        "buckets": buckets,
    })
}

/// Round to 3 decimals for a compact, stable telemetry frame.
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Emit one `calibrate.report` frame from a reliability curve.
pub fn emit_report(rel: &Reliability) {
    telemetry::emit("system", "calibrate.report", report_json(rel));
}

/// The periodic report pass (runtime-only; never run in tests). Each tick it folds
/// the live window into a reliability curve and emits the secret-free
/// `calibrate.report`. REPORT-ONLY: it changes nothing (the reduce-only routing
/// hook is a separate, config-gated read the router performs, not this loop). When
/// `[calibrate].enabled` is false the loop is a no-op — the pure analytics never
/// runs, exactly like the other propose/report cadences.
pub async fn calibrate_report_task(cfg: Arc<Config>) {
    tokio::time::sleep(REPORT_STARTUP_DELAY).await;
    loop {
        if cfg.calibrate.enabled {
            let rel = reliability_with(&snapshot(), &cfg.calibrate);
            emit_report(&rel);
        }
        tokio::time::sleep(REPORT_INTERVAL).await;
    }
}

// ===========================================================================
// TESTS — hermetic: synthetic samples, injected config; the pure core only.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    /// THRESHOLD write-integrity chokepoint (calibrate::record / relabel). The live
    /// calibration window shifts the OWNER's clarify threshold — a GUEST turn must
    /// add or relabel NOTHING in it. The global SINK is not touched by any other
    /// test; the crate lock is held for good measure. Assertions are deltas, so a
    /// leftover owner sample never affects another test.
    #[test]
    fn guest_turn_records_and_relabels_nothing_in_the_calibration_window() {
        let _g = crate::confirm::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let before = snapshot().len();
        {
            let guest = crate::threshold::guest_from(
                &crate::threshold::Scope::owner(vec!["*".to_string()], crate::focus::FocusProfile::Default),
                &crate::focus::FocusProfile::DeepFocus,
            );
            let _o = crate::threshold::ScopeOverride::guest(guest);
            assert!(crate::threshold::is_guest_turn());
            record(424242, 0.91);
            relabel(424242, Outcome::CorrectedNextTurn);
        }
        assert_eq!(snapshot().len(), before, "a guest turn wrote into the calibration window");
        // OWNER path records normally.
        record(424242, 0.91);
        assert_eq!(snapshot().len(), before + 1, "the owner sample was not recorded");
    }

    /// A config with sensible calibration defaults + the routing hook ON, for the
    /// hook tests (the pure-math tests do not touch it).
    fn cfg_hook_on() -> CalibrateConfig {
        CalibrateConfig {
            influence_routing: true,
            ..CalibrateConfig::default()
        }
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// N graded samples at a fixed confidence with `succ` of them successful (the
    /// rest `Failed`), so a bucket has a known predicted (== conf) and actual.
    fn block(conf: f64, n: usize, succ: usize) -> Vec<Sample> {
        (0..n)
            .map(|i| {
                let outcome = if i < succ { Outcome::Success } else { Outcome::Failed };
                Sample::new(conf, outcome)
            })
            .collect()
    }

    // --- binning ---------------------------------------------------------

    #[test]
    fn bin_of_maps_confidence_to_deciles_and_clamps() {
        assert_eq!(bin_of(0.0, 10), 0);
        assert_eq!(bin_of(0.05, 10), 0);
        assert_eq!(bin_of(0.09, 10), 0);
        assert_eq!(bin_of(0.10, 10), 1);
        assert_eq!(bin_of(0.55, 10), 5);
        assert_eq!(bin_of(0.99, 10), 9);
        // Right edge of the top bucket is inclusive.
        assert_eq!(bin_of(1.0, 10), 9);
        // Out-of-range / non-finite are clamped, never panic.
        assert_eq!(bin_of(-0.5, 10), 0);
        assert_eq!(bin_of(2.0, 10), 9);
        assert_eq!(bin_of(f64::NAN, 10), 0);
        assert_eq!(bin_of(f64::INFINITY, 10), 0);
        // bins is treated as at least 1.
        assert_eq!(bin_of(0.5, 0), 0);
    }

    // --- the fold: per-bucket counts + outcome semantics -----------------

    #[test]
    fn reliability_bins_and_computes_actual_rate_per_bucket() {
        // Bucket 9 (conf 0.95): 10 turns, 8 success -> actual 0.8, predicted 0.95.
        let mut samples = block(0.95, 10, 8);
        // Bucket 1 (conf 0.15): 10 turns, 5 success -> actual 0.5.
        samples.extend(block(0.15, 10, 5));
        let rel = reliability(&samples, 10, 5);

        let b9 = &rel.buckets[9];
        assert_eq!(b9.graded, 10);
        assert_eq!(b9.successes, 8);
        assert!(approx(b9.predicted.unwrap(), 0.95));
        assert!(approx(b9.actual.unwrap(), 0.8));
        assert!(b9.judged);

        let b1 = &rel.buckets[1];
        assert_eq!(b1.graded, 10);
        assert!(approx(b1.actual.unwrap(), 0.5));

        // Empty buckets report nothing (never a fake 0-rate).
        let b5 = &rel.buckets[5];
        assert_eq!(b5.graded, 0);
        assert!(b5.predicted.is_none());
        assert!(b5.actual.is_none());
        assert!(!b5.judged);

        assert_eq!(rel.graded_total, 20);
        assert_eq!(rel.unknown_total, 0);
    }

    #[test]
    fn corrected_and_failed_count_against_success_unknown_is_excluded() {
        // One bucket (conf 0.85): 2 success, 1 corrected, 1 failed, 3 unknown.
        let samples = vec![
            Sample::new(0.85, Outcome::Success),
            Sample::new(0.85, Outcome::Success),
            Sample::new(0.85, Outcome::CorrectedNextTurn),
            Sample::new(0.85, Outcome::Failed),
            Sample::new(0.85, Outcome::Unknown),
            Sample::new(0.85, Outcome::Unknown),
            Sample::new(0.85, Outcome::Unknown),
        ];
        let rel = reliability(&samples, 10, 4);
        let b8 = &rel.buckets[8];
        // graded = success + corrected + failed = 4 (the 3 unknown are excluded).
        assert_eq!(b8.graded, 4);
        assert_eq!(b8.successes, 2);
        // actual = 2/4 = 0.5 (corrected + failed both counted against).
        assert!(approx(b8.actual.unwrap(), 0.5));
        assert_eq!(rel.graded_total, 4);
        assert_eq!(rel.unknown_total, 3);
    }

    // --- the MIN_SAMPLE floor -------------------------------------------

    #[test]
    fn thin_bucket_is_insufficient_data_not_judged() {
        // 4 graded turns in one bucket, floor 5 -> insufficient (reported, unjudged).
        let samples = block(0.65, 4, 3);
        let rel = reliability(&samples, 10, 5);
        let b6 = &rel.buckets[6];
        assert_eq!(b6.graded, 4);
        // predicted is still shown (it is the claim, not a judgement)...
        assert!(b6.predicted.is_some());
        // ...but actual is withheld and the bucket is not judged.
        assert!(b6.actual.is_none());
        assert!(!b6.judged);
        // A thin-only corpus yields NO headline number (never a fabricated 0).
        assert_eq!(rel.judged_graded, 0);
        assert!(rel.ece().is_none());
        assert!(rel.gap().is_none());
        assert_eq!(rel.coverage(), 0.0);
    }

    #[test]
    fn at_the_floor_the_bucket_is_judged() {
        // Exactly at the floor -> judged.
        let rel = reliability(&block(0.65, 5, 3), 10, 5);
        assert!(rel.buckets[6].judged);
        assert_eq!(rel.judged_graded, 5);
        assert!(rel.ece().is_some());
    }

    // --- ECE + signed gap (known-answer) --------------------------------

    #[test]
    fn ece_and_gap_are_known_answer_over_judged_buckets() {
        // One judged bucket, predicted 0.9, actual 0.5 -> |0.9-0.5| = 0.4 ECE,
        // +0.4 gap (overconfident: claims more than it delivers).
        let rel = reliability(&block(0.9, 10, 5), 10, 5);
        assert!(approx(rel.ece().unwrap(), 0.4));
        assert!(approx(rel.gap().unwrap(), 0.4), "positive => overconfident");
        assert!(approx(rel.coverage(), 1.0));
    }

    #[test]
    fn underconfidence_makes_the_gap_negative() {
        // predicted 0.3, actual 0.9 -> gap -0.6 (underconfident), ECE +0.6.
        let rel = reliability(&block(0.3, 10, 9), 10, 5);
        assert!(approx(rel.gap().unwrap(), -0.6), "negative => underconfident");
        assert!(approx(rel.ece().unwrap(), 0.6));
    }

    #[test]
    fn perfect_calibration_has_near_zero_ece_and_gap() {
        // conf 0.5, exactly 50% success -> perfectly calibrated in that bucket.
        let rel = reliability(&block(0.5, 20, 10), 10, 5);
        assert!(approx(rel.ece().unwrap(), 0.0));
        assert!(approx(rel.gap().unwrap(), 0.0));
    }

    #[test]
    fn ece_gap_are_sample_weighted_across_buckets_and_skip_insufficient_ones() {
        // Judged bucket A: conf 0.9, 30 turns, 15 success -> gap +0.4, weight 30.
        let mut samples = block(0.9, 30, 15);
        // Judged bucket B: conf 0.1, 10 turns, 2 success (actual 0.2) -> gap -0.1,
        // weight 10.
        samples.extend(block(0.1, 10, 2));
        // Insufficient bucket C: conf 0.5, 3 turns -> excluded from the average.
        samples.extend(block(0.5, 3, 0));
        let rel = reliability(&samples, 10, 5);

        // judged_graded = 40 (C excluded).
        assert_eq!(rel.judged_graded, 40);
        // gap = (30*0.4 + 10*(-0.1)) / 40 = (12 - 1)/40 = 0.275.
        assert!(approx(rel.gap().unwrap(), 0.275));
        // ece = (30*0.4 + 10*0.1)/40 = 13/40 = 0.325.
        assert!(approx(rel.ece().unwrap(), 0.325));
        // coverage = 40 / 43 graded.
        assert!(approx(rel.coverage(), 40.0 / 43.0));
    }

    #[test]
    fn empty_corpus_reports_no_evidence() {
        let rel = reliability(&[], 10, 5);
        assert_eq!(rel.graded_total, 0);
        assert!(rel.ece().is_none());
        assert!(rel.gap().is_none());
        assert_eq!(rel.coverage(), 0.0);
        assert_eq!(rel.buckets.len(), 10);
    }

    // --- report_json is secret-free + null-honest ------------------------

    #[test]
    fn report_json_is_aggregate_and_null_when_no_evidence() {
        let rel = reliability(&[], 10, 5);
        let v = report_json(&rel);
        // ECE / gap are null (not 0) when nothing is judgeable.
        assert!(v["ece"].is_null());
        assert!(v["gap"].is_null());
        assert_eq!(v["graded_total"], json!(0));
        assert_eq!(v["bins"], json!(10));
        assert!(v["buckets"].as_array().unwrap().len() == 10);

        // With evidence, the frame carries only numbers/counts + a null for the
        // still-insufficient buckets' predicted/actual.
        let rel2 = reliability(&block(0.9, 10, 5), 10, 5);
        let v2 = report_json(&rel2);
        assert!(approx(v2["ece"].as_f64().unwrap(), 0.4));
        let b9 = &v2["buckets"].as_array().unwrap()[9];
        // block(0.9, 10, 5): claim ~0.9, 5/10 success -> actual 0.5.
        assert!(approx(b9["actual"].as_f64().unwrap(), 0.5));
        let b0 = &v2["buckets"].as_array().unwrap()[0];
        assert!(b0["actual"].is_null(), "empty bucket stays null, not 0");
    }

    // --- the REDUCE-ONLY clarify-widen hook ------------------------------

    #[test]
    fn hook_is_inert_when_routing_influence_is_off() {
        // The shipped default: influence_routing = false -> base for EVERY input,
        // even a wildly overconfident bucket.
        let rel = reliability(&block(0.9, 40, 4), 10, 5); // actual 0.1, claim 0.9
        let cfg = CalibrateConfig::default(); // influence_routing = false
        assert!(!cfg.influence_routing);
        for c in [0.0, 0.3, 0.5, 0.9, 1.0] {
            assert_eq!(
                adjusted_clarify_threshold(0.6, c, &rel, &cfg),
                0.6,
                "report-only default must never change the threshold"
            );
        }
    }

    #[test]
    fn hook_widens_only_in_a_measurably_overconfident_bucket() {
        // Bucket 9 (conf ~0.9) is overconfident: actual 0.2, claim 0.9.
        let rel = reliability(&block(0.9, 40, 8), 10, 5);
        let cfg = cfg_hook_on();
        let base = 0.6;
        let widened = adjusted_clarify_threshold(base, 0.9, &rel, &cfg);
        assert!(widened > base, "overconfident bucket must widen: {widened}");
        // Bounded by max_widen (default 0.15).
        assert!(widened <= base + cfg.max_widen + 1e-9);
    }

    #[test]
    fn hook_never_widens_a_well_calibrated_or_underconfident_bucket() {
        let cfg = cfg_hook_on();
        // Well-calibrated: conf 0.6, actual 0.6.
        let calibrated = reliability(&block(0.6, 40, 24), 10, 5);
        assert_eq!(adjusted_clarify_threshold(0.5, 0.6, &calibrated, &cfg), 0.5);
        // Underconfident: conf 0.3, actual 0.9 -> never widen (DARWIN under-claimed).
        let under = reliability(&block(0.3, 40, 36), 10, 5);
        assert_eq!(adjusted_clarify_threshold(0.5, 0.3, &under, &cfg), 0.5);
    }

    #[test]
    fn hook_never_widens_an_insufficient_bucket() {
        let cfg = cfg_hook_on();
        // Overconfident-looking but only 3 turns -> insufficient -> no widen.
        let thin = reliability(&block(0.9, 3, 0), 10, 5);
        assert_eq!(adjusted_clarify_threshold(0.6, 0.9, &thin, &cfg), 0.6);
    }

    #[test]
    fn hook_is_reduce_only_across_a_sweep_of_curves_and_confidences() {
        // THE headline invariant: for ANY curve, ANY confidence, ANY flag state, the
        // adjusted threshold is NEVER below base — the band can only ever WIDEN.
        let cfg = cfg_hook_on();
        for (conf, n, succ) in [(0.9, 40, 2), (0.5, 40, 20), (0.1, 40, 39), (0.7, 3, 0)] {
            let rel = reliability(&block(conf, n, succ), 10, 5);
            for base in [0.0, 0.3, 0.6, 0.95, 1.0] {
                for probe in [0.0, 0.2, 0.5, 0.9, 1.0] {
                    let adj = adjusted_clarify_threshold(base, probe, &rel, &cfg);
                    assert!(adj >= base, "must never narrow: base={base} probe={probe} adj={adj}");
                    assert!(adj <= 1.0, "must stay a valid threshold: {adj}");
                }
            }
        }
    }

    #[test]
    fn hook_widen_is_capped_at_one() {
        let cfg = cfg_hook_on();
        // Extreme overconfidence with a high base -> capped at 1.0, still >= base.
        let rel = reliability(&block(0.95, 40, 0), 10, 5); // actual 0.0, claim ~0.95
        let adj = adjusted_clarify_threshold(0.95, 0.95, &rel, &cfg);
        assert!((0.95..=1.0).contains(&adj), "capped at 1.0, never below base: {adj}");
    }

    // --- the in-memory sink: record / relabel / bounded ------------------

    #[test]
    fn sink_records_success_then_relabels_by_id() {
        let mut st = CalibrationState::new();
        st.record(1, 0.8);
        st.record(2, 0.4);
        assert_eq!(st.samples().len(), 2);
        // Fresh entries are provisional Success.
        assert!(st.samples().iter().all(|s| s.outcome == Outcome::Success));
        // Relabel the exact id (a next-turn correction of turn 1).
        assert!(st.relabel(1, Outcome::CorrectedNextTurn));
        let s = st.samples();
        assert_eq!(s[0].outcome, Outcome::CorrectedNextTurn);
        assert_eq!(s[1].outcome, Outcome::Success);
        // An unknown / evicted id is a silent no-op.
        assert!(!st.relabel(999, Outcome::Failed));
    }

    #[test]
    fn sink_is_bounded_evicting_oldest() {
        let mut st = CalibrationState::new();
        let total = CALIBRATE_WINDOW + 5;
        for i in 0..total {
            st.record(i as i64, 0.5);
        }
        assert_eq!(st.samples().len(), CALIBRATE_WINDOW, "window is bounded");
        // The 5 oldest ids were evicted; relabeling one is a no-op.
        assert!(!st.relabel(0, Outcome::Failed), "oldest evicted");
        assert!(st.relabel((total - 1) as i64, Outcome::Failed), "newest present");
    }

    #[test]
    fn sink_samples_feed_the_pure_analysis_end_to_end() {
        // A tiny end-to-end: record turns, relabel some, fold -> a real curve.
        let mut st = CalibrationState::new();
        for i in 0..10 {
            st.record(i, 0.9);
        }
        // Correct 6 of the 10 high-confidence turns -> actual 0.4 in bucket 9.
        for i in 0..6 {
            st.relabel(i, Outcome::CorrectedNextTurn);
        }
        let rel = reliability(&st.samples(), 10, 5);
        let b9 = &rel.buckets[9];
        assert_eq!(b9.graded, 10);
        assert_eq!(b9.successes, 4);
        assert!(approx(b9.actual.unwrap(), 0.4));
        // Overconfident: claim ~0.9, actual 0.4 -> positive gap.
        assert!(rel.gap().unwrap() > 0.0);
    }
}
