//! SELF-DISTILLATION (F17) — an ARMED-BUT-INERT on-device LoRA pipeline that
//! learns a personal adapter from the user's OWN graded interactions.
//!
//! THE THREE HARD RULES:
//!   1. SHIPS OFF. Training MUTATES weights (produces an adapter) — a
//!      consequential, device-heavy op — so `[distill].enabled` defaults false,
//!      exactly like `[security].encrypt_memory`. With it off, every entry
//!      point here is a no-op and the status honestly reports "off".
//!   2. NEVER AUTO-PROMOTES. A trained adapter is written to a STAGING dir
//!      under `state/` and recorded in a manifest — and that is where it stops.
//!      NOTHING here points the inference server at a staged adapter; making an
//!      adapter live is a separate, deliberate act, never a side effect of
//!      training. (This mirrors heal.rs: propose/stage, human applies.)
//!   3. DEVICE-GATED TRAINING IS INERT, NEVER FAKED. The actual `mlx_lm.lora`
//!      run needs Apple Silicon + mlx-lm; the daemon can't import Python to
//!      verify that, so the capability is reported `verified=false` (Unverified)
//!      — never a fabricated "ready". The training actuator is BUILT here (via
//!      the injected-runner seam, like posture.rs) and hermetically tested with
//!      a canned runner; the REAL subprocess is spawned only on-device behind
//!      the gate, never in any test (the shell.rs discipline).
//!
//! HONEST DATA QUALITY (stated, not hidden): the only real (prompt -> full
//! response) pairs live in the raw transcript store; the only grade is the
//! optimizer's routing outcome. So a positive example is a transcript turn the
//! user did NOT redirect on the next turn (no `CorrectedNextTurn` trace for its
//! redacted utterance) — an honest "kept answer" signal, NOT a quality score.
//! Every prompt + response is REDACTED (optimize::redact — the transcript store
//! keeps raw recipients) before it can land in a dataset that leaves nothing
//! the redactor would strip.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Hard cap on examples per dataset — a personal adapter, not a corpus.
const MAX_EXAMPLES: usize = 500;
/// Below this the dataset is too thin to bother; status reports "not enough
/// examples yet" rather than training on noise.
const MIN_EXAMPLES: usize = 32;
/// Bound on each prompt/response char length in the dataset (post-redaction).
const FIELD_CHARS: usize = 4000;

/// One redacted training pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    pub prompt: String,
    pub response: String,
}

/// Redact-then-bound a field for the dataset. Redaction FIRST (a later
/// truncation can't split a token the redactor would have caught).
fn clean_field(s: &str) -> String {
    let redacted = crate::optimize::redact(s.trim());
    if redacted.chars().count() <= FIELD_CHARS {
        redacted
    } else {
        redacted.chars().take(FIELD_CHARS).collect()
    }
}

/// Select positive distillation examples from raw transcript pairs. PURE and
/// exhaustively tested — the whole selection policy lives here:
///   * both fields must be non-empty AFTER redaction (a turn that redacts to
///     nothing teaches nothing);
///   * the turn's redacted prompt must NOT be in `corrected` — the user
///     redirected on the next turn, the clearest "this answer was wrong" signal
///     the corpus has;
///   * dedup by redacted prompt (repeated asks shouldn't over-weight);
///   * bounded to [`MAX_EXAMPLES`], newest-first input preserved.
///
/// `corrected` MUST hold keys produced by the SAME [`clean_field`] transform
/// (redact + the FIELD_CHARS bound) as the prompt — otherwise the membership
/// test would compare a truncated prompt against an untruncated corrected
/// entry and silently miss a long redirected turn (`corrected_utterances`
/// builds the set that way).
pub fn select_candidates(
    transcripts: &[(String, String)],
    corrected: &HashSet<String>,
) -> Vec<Example> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (text, response) in transcripts {
        let prompt = clean_field(text);
        let resp = clean_field(response);
        if prompt.is_empty() || resp.is_empty() {
            continue;
        }
        if corrected.contains(&prompt) {
            continue; // the user redirected — never a positive example
        }
        if !seen.insert(prompt.clone()) {
            continue; // dedup repeated asks
        }
        out.push(Example { prompt, response: resp });
        if out.len() >= MAX_EXAMPLES {
            break;
        }
    }
    out
}

/// Render examples as mlx-lm chat JSONL (one `{"messages":[user,assistant]}`
/// object per line — the format `mlx_lm.lora --data` expects). PURE. Every
/// field is already redacted + bounded by [`select_candidates`].
pub fn render_jsonl(examples: &[Example]) -> String {
    let mut out = String::new();
    for e in examples {
        let line = json!({
            "messages": [
                {"role": "user", "content": e.prompt},
                {"role": "assistant", "content": e.response},
            ]
        });
        out.push_str(&line.to_string());
        out.push('\n');
    }
    out
}

// ---------------------------------------------------------------------------
// The adapter manifest — the honest record of what was (or would be) trained
// ---------------------------------------------------------------------------

/// Where a distillation run stands. `Prepared` = a dataset was assembled and
/// staged; `Trained` = the device-gated run wrote an adapter to staging;
/// `Failed` = the run was attempted and did not produce an adapter. There is no
/// `Promoted` — promotion is deliberately not this module's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Prepared,
    Trained,
    Failed,
}

impl RunStatus {
    fn wire(self) -> &'static str {
        match self {
            RunStatus::Prepared => "prepared",
            RunStatus::Trained => "trained",
            RunStatus::Failed => "failed",
        }
    }
}

/// The manifest written beside a staged adapter. SECRET-FREE by construction:
/// counts + the base-model id + coarse status + the MEASURED held-out losses,
/// never an example's text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// RFC3339 stamp of the run.
    pub created: String,
    /// The base model the adapter attaches to.
    pub base_model: String,
    /// How many redacted examples fed the run.
    pub example_count: usize,
    pub status: RunStatus,
    /// The staging path (under `state/`); the adapter is NOT live.
    pub staging_dir: String,
    /// Whether this adapter is LIVE. Flips true ONLY after a deliberate,
    /// MEASURED promotion (adapter beat base on the held-out split by the
    /// configured margin). Training alone never sets it.
    pub promoted: bool,
    /// The BASE model's held-out (valid split) loss, when the promotion eval ran.
    /// None until an eval happens. The honest denominator of the win.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_out_base_loss: Option<f64>,
    /// The trained ADAPTER's held-out loss over the SAME split. None until eval.
    /// promotion requires this to beat `held_out_base_loss` by the margin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_out_adapter_loss: Option<f64>,
}

// ---------------------------------------------------------------------------
// The device-gated training actuator — BUILT here, spawned only on-device
// ---------------------------------------------------------------------------

/// The exact `mlx_lm.lora` invocation as DATA (program + args, never a shell
/// string), so the argv is asserted in tests WITHOUT running it. `data_dir`
/// holds the rendered `train.jsonl`; `adapter_dir` is the staging output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrainCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// Build the training command. PURE. `python` is the operator's interpreter
/// (config), `base_model` the checkpoint to adapt, `iters` the (bounded) step
/// count. The flags are mlx-lm 0.31's LoRA CLI; the daemon assembles them but
/// never runs them here.
pub fn train_command(
    python: &str,
    base_model: &str,
    data_dir: &str,
    adapter_dir: &str,
    iters: u32,
) -> TrainCommand {
    TrainCommand {
        program: python.to_string(),
        args: vec![
            "-m".into(),
            "mlx_lm.lora".into(),
            "--model".into(),
            base_model.into(),
            "--train".into(),
            "--data".into(),
            data_dir.into(),
            "--adapter-path".into(),
            adapter_dir.into(),
            "--iters".into(),
            iters.to_string(),
            "--batch-size".into(),
            "1".into(),
        ],
    }
}

/// The outcome of an attempted training run (from the injected runner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrainOutcome {
    /// The subprocess exited 0 and the adapter file is present.
    Ok,
    /// The run could not start / exited non-zero / produced no adapter — with a
    /// secret-free reason. NEVER fabricates success.
    Failed(String),
}

/// Run the training command through an INJECTED runner and map its result to a
/// [`TrainOutcome`]. The runner seam (like posture.rs::build_report) lets the
/// full fold be hermetically tested with a canned runner; the REAL runner
/// (spawn + timeout + kill_on_drop) is passed only on-device. This fn does NOT
/// promote and does NOT touch the inference server — it produces a staged
/// adapter and returns.
pub async fn run_training<F, Fut, P>(cmd: &TrainCommand, adapter_present: P, run: F) -> TrainOutcome
where
    F: FnOnce(String, Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<i32, String>>,
    P: FnOnce() -> bool,
{
    match run(cmd.program.clone(), cmd.args.clone()).await {
        // Probe the adapter AFTER the run — the training writes it during the run.
        Ok(0) if adapter_present() => TrainOutcome::Ok,
        Ok(0) => TrainOutcome::Failed("training exited 0 but wrote no adapter".into()),
        Ok(code) => TrainOutcome::Failed(format!("training exited with code {code}")),
        Err(e) => TrainOutcome::Failed(format!("training could not run: {e}")),
    }
}

// ---------------------------------------------------------------------------
// MEASURED PROMOTION — the honest gate: a trained adapter goes LIVE only after
// it beats the base model on the user's held-out turns. No measured win, no
// promotion. This is what makes rule #2 (never auto-promote) a MEASURED,
// reversible, opt-in act instead of a permanent refusal.
// ---------------------------------------------------------------------------

/// Build the HELD-OUT EVAL command: `mlx_lm.lora --model <base> --data <dir>
/// --test --adapter-path <adapter-or-empty>`. mlx-lm computes + prints the test
/// loss over the run dir's `test.jsonl` (the held-out split, never in
/// `train.jsonl`). The `--adapter-path` is ALWAYS passed: an EMPTY string for the
/// BASE (mlx_lm's documented "test without LoRA layers" path — omitting it makes
/// mlx_lm default to the literal dir "adapters" and fail), the staged run dir for
/// the ADAPTER. The two losses are then directly comparable. Reuses the argv
/// container (program + args, never a shell string). PURE + tested, never run here.
pub fn eval_command(
    python: &str,
    base_model: &str,
    data_dir: &str,
    adapter_dir: Option<&str>,
) -> TrainCommand {
    TrainCommand {
        program: python.to_string(),
        args: vec![
            "-m".into(),
            "mlx_lm.lora".into(),
            "--model".into(),
            base_model.into(),
            "--data".into(),
            data_dir.into(),
            "--test".into(),
            // Empty => test the BASE (no LoRA layers); a dir => test that adapter.
            "--adapter-path".into(),
            adapter_dir.unwrap_or("").into(),
            "--batch-size".into(),
            "1".into(),
        ],
    }
}

/// Parse the `Test loss <f>` line mlx_lm.lora prints at the end of a `--test`
/// run (e.g. "Test loss 2.345, Test ppl 10.434"). Returns the loss, or None when
/// the line is absent/unparseable — an UNMEASURABLE result NEVER counts as a win
/// (the gate rejects on None). PURE + tested. Case-insensitive on the label;
/// tolerant of the trailing ppl and surrounding whitespace. Parses from a
/// lowercased copy so a multibyte glyph earlier in the line can't desync the
/// byte offset (the number itself is ASCII, unaffected by lowercasing).
pub fn parse_test_loss(stdout: &str) -> Option<f64> {
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if let Some(idx) = lower.find("test loss") {
            let after = &lower[idx + "test loss".len()..];
            let tok = after
                .trim_start_matches(|c: char| c == ':' || c == '=' || c.is_whitespace())
                .split(|c: char| c == ',' || c.is_whitespace())
                .next()?;
            if let Ok(v) = tok.trim().parse::<f64>() {
                return Some(v);
            }
        }
    }
    None
}

/// The MEASURED promotion decision. `min_improvement` is the minimum held-out
/// loss reduction (`base - adapter`, in nats/token) required to go live.
#[derive(Debug, Clone, PartialEq)]
pub enum PromotionDecision {
    /// The adapter beat base by at least the margin — eligible to go live.
    Promote { base_loss: f64, adapter_loss: f64, improvement: f64 },
    /// NOT promoted, with the honest reason + whatever was measured.
    Reject {
        base_loss: Option<f64>,
        adapter_loss: Option<f64>,
        improvement: Option<f64>,
        reason: &'static str,
    },
}

/// Decide promotion PURELY from the two measured held-out losses. Promote ONLY
/// when BOTH are finite AND `(base - adapter) >= min_improvement`. A missing or
/// non-finite measurement REJECTS — the gate never promotes on an unmeasurable
/// result, and (with a non-negative margin) never on a tie or a regression. This
/// is the honesty core of self-personalization. PURE + exhaustively tested.
pub fn promotion_decision(
    base_loss: Option<f64>,
    adapter_loss: Option<f64>,
    min_improvement: f64,
) -> PromotionDecision {
    match (base_loss, adapter_loss) {
        (Some(b), Some(a)) if b.is_finite() && a.is_finite() => {
            let improvement = b - a;
            if improvement >= min_improvement && min_improvement >= 0.0 {
                PromotionDecision::Promote { base_loss: b, adapter_loss: a, improvement }
            } else {
                PromotionDecision::Reject {
                    base_loss: Some(b),
                    adapter_loss: Some(a),
                    improvement: Some(improvement),
                    reason: "the adapter did not beat the base model on your held-out turns",
                }
            }
        }
        _ => PromotionDecision::Reject {
            base_loss,
            adapter_loss,
            improvement: None,
            reason: "the held-out loss was not measurable",
        },
    }
}

/// An eval subprocess's captured STDOUT (to parse the loss from), or a
/// secret-free failure reason. NEVER fabricates a loss.
pub type EvalResult = Result<String, String>;

/// Run BOTH held-out evals (base, then adapter) through an injected runner that
/// returns each subprocess's captured stdout, and parse the two losses. The
/// runner seam (like the training runner) makes the fold hermetically testable;
/// the live wiring passes [`run_real_eval`]. Returns `(base_loss, adapter_loss)`
/// — either is None when its eval failed or printed no parseable loss (the gate
/// then rejects). `data_dir` holds `test.jsonl`; `adapter_dir` holds the trained
/// `adapters.safetensors`.
pub async fn evaluate_adapter<F, Fut>(
    cfg: &crate::config::Config,
    data_dir: &str,
    adapter_dir: &str,
    mut run: F,
) -> (Option<f64>, Option<f64>)
where
    F: FnMut(String, Vec<String>) -> Fut,
    Fut: std::future::Future<Output = EvalResult>,
{
    let base_cmd = eval_command(&cfg.distill.python, &cfg.distill.base_model, data_dir, None);
    let adapter_cmd =
        eval_command(&cfg.distill.python, &cfg.distill.base_model, data_dir, Some(adapter_dir));
    let base_loss = match run(base_cmd.program, base_cmd.args).await {
        Ok(out) => parse_test_loss(&out),
        Err(_) => None,
    };
    let adapter_loss = match run(adapter_cmd.program, adapter_cmd.args).await {
        Ok(out) => parse_test_loss(&out),
        Err(_) => None,
    };
    (base_loss, adapter_loss)
}

/// The REAL eval runner — spawns `mlx_lm.lora --test` on-device and CAPTURES its
/// output (the "Test loss" line). Reached ONLY behind the gate, NEVER in a test.
/// Same hardening as the training runner (fixed argv, kill_on_drop, bounded
/// timeout). Captures stdout AND stderr (mlx-lm builds differ on which carries
/// the summary line) and returns the concatenation on a clean exit.
pub async fn run_real_eval(program: String, args: Vec<String>) -> EvalResult {
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    match tokio::time::timeout(TRAIN_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            s.push('\n');
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            Ok(s)
        }
        Ok(Ok(out)) => Err(format!("eval exited with code {}", out.status.code().unwrap_or(-1))),
        Ok(Err(e)) => Err(format!("eval spawn failed ({e})")),
        Err(_) => Err("eval timed out".into()),
    }
}

/// The LIVE-adapter pointer the inference server reads: `state/lora/promoted/`.
/// When it holds `adapters.safetensors` + a `manifest.json` whose `base_model`
/// matches the server's resident LLM, the server loads generation WITH the
/// adapter (honest fallback + report when it can't). Absent = base model.
pub fn promoted_dir(root: &std::path::Path) -> std::path::PathBuf {
    staging_root(root).join("promoted")
}

/// Read the live promotion manifest, or None when no adapter is promoted.
pub fn read_promoted_manifest(root: &std::path::Path) -> Option<Manifest> {
    let bytes = std::fs::read(promoted_dir(root).join("manifest.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Install the trained adapter as the LIVE `promoted/` pointer. ATOMIC-ish:
/// stage into a sibling temp dir, then rename over `promoted/`, so the server
/// never reads a half-copied adapter. Called ONLY after [`promotion_decision`]
/// returned Promote — it does not re-decide.
fn install_promotion(
    root: &std::path::Path,
    run_dir: &std::path::Path,
    manifest: &Manifest,
) -> std::io::Result<()> {
    let promoted = promoted_dir(root);
    let staging = staging_root(root);
    std::fs::create_dir_all(&staging)?;
    let tmp = staging.join(format!("promoting-{}", manifest.created.replace([':', '.'], "-")));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    // Copy the adapter file(s) mlx_lm writes: the weights + its adapter_config.
    let mut copied_weights = false;
    for name in ["adapters.safetensors", "adapter_config.json"] {
        let src = run_dir.join(name);
        if src.exists() {
            std::fs::copy(&src, tmp.join(name))?;
            if name == "adapters.safetensors" {
                copied_weights = true;
            }
        }
    }
    if !copied_weights {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no adapter weights to promote",
        ));
    }
    std::fs::write(
        tmp.join("manifest.json"),
        serde_json::to_vec_pretty(manifest).unwrap_or_default(),
    )?;
    let _ = std::fs::remove_dir_all(&promoted);
    std::fs::rename(&tmp, &promoted)?;
    Ok(())
}

/// Clear the live adapter — roll back to the base model. Removes the `promoted/`
/// pointer so the next server model-load serves base. Idempotent (absent = ok).
pub fn clear_promotion(root: &std::path::Path) -> std::io::Result<()> {
    let promoted = promoted_dir(root);
    if promoted.exists() {
        std::fs::remove_dir_all(&promoted)?;
    }
    Ok(())
}

/// EVALUATE the last TRAINED run against base on its held-out split and promote
/// the adapter ONLY on a MEASURED win (>= `[distill].min_improvement`). Reversible
/// ([`clear_promotion`]). HONEST at every step: no trained run -> says so; an
/// unmeasurable or losing eval -> base stays live, adapter stays staged, and the
/// measured (non-)result is recorded in the manifest. `run_eval` is injected so
/// the whole orchestration is hermetically tested; the live wiring passes
/// [`run_real_eval`]. NEVER promotes without a measured win.
pub async fn promote_last<F, Fut>(
    cfg: &crate::config::Config,
    root: &std::path::Path,
    run_eval: F,
) -> String
where
    F: FnMut(String, Vec<String>) -> Fut,
    Fut: std::future::Future<Output = EvalResult>,
{
    if !cfg.distill.enabled {
        return "Self-distillation is off, sir — nothing to promote.".to_string();
    }
    let Some(mut manifest) = read_last_manifest(root) else {
        return "There's no trained adapter to promote yet, sir.".to_string();
    };
    if manifest.status != RunStatus::Trained {
        return "The last run didn't produce a trained adapter, sir — nothing to promote."
            .to_string();
    }
    let run_dir = std::path::PathBuf::from(&manifest.staging_dir);
    if !run_dir.join("adapters.safetensors").exists() {
        return "The staged adapter file is missing, sir — I won't promote a phantom.".to_string();
    }
    // MEASURE: base vs adapter held-out loss over the run's test.jsonl split.
    let (base_loss, adapter_loss) =
        evaluate_adapter(cfg, &manifest.staging_dir, &manifest.staging_dir, run_eval).await;
    manifest.held_out_base_loss = base_loss;
    manifest.held_out_adapter_loss = adapter_loss;
    match promotion_decision(base_loss, adapter_loss, cfg.distill.min_improvement) {
        PromotionDecision::Promote { base_loss, adapter_loss, improvement } => {
            manifest.promoted = true;
            if let Err(e) = install_promotion(root, &run_dir, &manifest) {
                manifest.promoted = false;
                write_manifest(root, &run_dir, &manifest);
                return format!(
                    "The adapter beat base ({adapter_loss:.3} vs {base_loss:.3}) but I couldn't install it, sir — {e}. Base stays live."
                );
            }
            write_manifest(root, &run_dir, &manifest);
            format!(
                "Promoted your personal adapter, sir — it beat the base model on your held-out turns ({adapter_loss:.3} vs {base_loss:.3}, a {improvement:.3} nats/token improvement). It's live now; say \"roll back my adapter\" to revert to base."
            )
        }
        PromotionDecision::Reject { base_loss, adapter_loss, improvement, reason } => {
            write_manifest(root, &run_dir, &manifest);
            let measured = match (base_loss, adapter_loss, improvement) {
                (Some(b), Some(a), Some(d)) => format!(" (adapter {a:.3} vs base {b:.3}, Δ{d:.3})"),
                _ => String::new(),
            };
            format!(
                "I did NOT promote the adapter, sir — {reason}{measured}. The base model stays live; the adapter is kept staged."
            )
        }
    }
}

// ---------------------------------------------------------------------------
// The honest status surface (capability-map sibling; its own event too)
// ---------------------------------------------------------------------------

/// The `distill.status` wire payload the HUD renders. PURE + total. SECRET-FREE:
/// coarse readiness, counts, and the last run's manifest summary — never an
/// example, never raw text.
///
///   enabled          — `[distill].enabled` (ships false)
///   dep_verified     — false: the daemon cannot import Python to confirm
///                      mlx-lm + Apple Silicon; only the on-device run can.
///   examples_ready   — how many redacted positive examples are available NOW
///   min_examples     — the floor below which a run won't be worthwhile
///   ready_to_train   — enabled AND examples_ready >= min_examples (the dataset
///                      is ready; the DEVICE gate is still separate + unverified)
///   last_run         — the most recent manifest summary, or null
///   promoted         — the LIVE adapter summary (base id + measured held-out
///                      losses), or null when the base model is live
///   gated_promotion  — always true: an adapter goes live ONLY on a measured win
pub fn status_payload(
    enabled: bool,
    examples_ready: usize,
    last_run: Option<&Manifest>,
    promoted: Option<&Manifest>,
) -> Value {
    let summary = |m: &Manifest| json!({
        "created": m.created,
        "base_model": m.base_model,
        "example_count": m.example_count,
        "status": m.status.wire(),
        "promoted": m.promoted,
        "held_out_base_loss": m.held_out_base_loss,
        "held_out_adapter_loss": m.held_out_adapter_loss,
    });
    json!({
        "enabled": enabled,
        "dep_verified": false,
        "dependency": "Apple Silicon + mlx-lm (verified only on-device)",
        "examples_ready": examples_ready,
        "min_examples": MIN_EXAMPLES,
        "ready_to_train": enabled && examples_ready >= MIN_EXAMPLES,
        // Promotion is GATED on a measured held-out win — never automatic on a
        // trained adapter (it stays staged until it beats base).
        "gated_promotion": true,
        "adapter_live": promoted.is_some(),
        "last_run": last_run.map(&summary),
        "promoted": promoted.map(&summary),
    })
}

// ---------------------------------------------------------------------------
// Thin async wrappers over the daemon stores (logic lives in the pure fns)
// ---------------------------------------------------------------------------

/// Gather the redacted positive-example dataset from the live stores: recent
/// raw transcript pairs, minus any turn the optimizer flagged
/// `CorrectedNextTurn`. Thin — all policy is in [`select_candidates`]. A failed
/// read degrades to an empty dataset (honest "not enough examples"), never an
/// error.
pub async fn gather_examples(memory: &crate::memory::Memory) -> Vec<Example> {
    let transcripts = memory.recent_exchanges(MAX_EXAMPLES.saturating_mul(2)).await.unwrap_or_default();
    let corrected = corrected_utterances().await;
    // recent_exchanges returns oldest-first; a dataset wants the freshest
    // signal first so the cap keeps the most recent turns.
    let mut newest_first = transcripts;
    newest_first.reverse();
    select_candidates(&newest_first, &corrected)
}

/// The redacted utterances the optimizer graded `CorrectedNextTurn` — the
/// negative signal. Empty when the trace store is off/absent (fail-open: with
/// no negatives, selection simply keeps more turns, never fabricates one).
async fn corrected_utterances() -> HashSet<String> {
    let Some(store) = crate::optimize::global_trace_store() else {
        return HashSet::new();
    };
    let traces = store.recent(crate::optimize::MAX_TRACES).await.unwrap_or_default();
    traces
        .into_iter()
        .filter(|t| t.outcome == crate::optimize::Outcome::CorrectedNextTurn)
        // Key through the SAME clean_field transform the prompt uses (redact is
        // idempotent; the FIELD_CHARS bound is what matters), so the membership
        // test in select_candidates is symmetric and never misses a long
        // redirected turn.
        .map(|t| clean_field(&t.utterance_redacted))
        .collect()
}

/// Emit `distill.status` for the HUD, on the audit-snapshot cadence. READ-ONLY:
/// counts the ready examples and reads the last manifest; runs no training.
/// When `[distill].enabled` is false it still emits the honest off payload so
/// the panel shows the inert state. Warn-free, fail-open.
pub async fn emit_status(cfg: &crate::config::Config, memory: &crate::memory::Memory, root: &std::path::Path) {
    let examples_ready = if cfg.distill.enabled {
        gather_examples(memory).await.len()
    } else {
        0
    };
    let last = read_last_manifest(root);
    let promoted = read_promoted_manifest(root);
    crate::telemetry::emit(
        "system",
        "distill.status",
        status_payload(cfg.distill.enabled, examples_ready, last.as_ref(), promoted.as_ref()),
    );
}

/// The staging root under the daemon-owned, gitignored `state/` tree.
pub fn staging_root(root: &std::path::Path) -> std::path::PathBuf {
    root.join("state").join("lora")
}

/// Generous training budget — an mlx-lm LoRA run is minutes, unlike every other
/// bounded status subprocess. Still bounded so a wedged run can never hang the
/// caller forever; kill_on_drop reaps the child if the future is dropped.
const TRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 30);

/// The REAL training runner — spawns `mlx_lm.lora` on-device. Reached ONLY from
/// [`distill_now`] behind the `[distill].enabled` gate, and NEVER in any test
/// (tests pass a canned runner to `run_training`, exactly like posture.rs). It
/// hardens the spawn like shell.rs::run_sandboxed: fixed argv (never a shell
/// string), kill_on_drop, and a bounded timeout. Returns the exit code, or a
/// secret-free reason it could not run.
pub async fn run_real_training(program: String, args: Vec<String>) -> Result<i32, String> {
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args).kill_on_drop(true);
    match tokio::time::timeout(TRAIN_TIMEOUT, cmd.status()).await {
        Ok(Ok(status)) => Ok(status.code().unwrap_or(-1)),
        Ok(Err(e)) => Err(format!("spawn failed ({e})")),
        Err(_) => Err("training timed out".into()),
    }
}

/// ORCHESTRATE one distillation run — the operator-triggered entry point
/// (authenticated command channel; NEVER a background cadence — training is
/// heavy and only ever starts from an explicit act). Prepares the redacted
/// dataset + a manifest, then runs the device-gated training, and STOPS at a
/// staged adapter: it NEVER promotes. Returns a spoken-style summary. Fail-open
/// + honest at every step; nothing outside `state/lora/` is touched.
///
/// `run` is injected so the whole orchestration is hermetically tested with a
/// canned runner; the live wiring passes [`run_real_training`].
pub async fn distill_now<F, Fut>(
    cfg: &crate::config::Config,
    memory: &crate::memory::Memory,
    root: &std::path::Path,
    now_rfc3339: String,
    run: F,
) -> String
where
    F: FnOnce(String, Vec<String>) -> Fut,
    Fut: std::future::Future<Output = Result<i32, String>>,
{
    if !cfg.distill.enabled {
        return "Self-distillation is off, sir — turn on [distill].enabled to build a personal adapter from your own graded conversations.".to_string();
    }
    let examples = gather_examples(memory).await;
    if examples.len() < MIN_EXAMPLES {
        return format!(
            "Not enough graded examples yet, sir — {} of the {} I'd want. I only train on turns you didn't redirect, so this grows as we talk.",
            examples.len(),
            MIN_EXAMPLES
        );
    }

    // Stage a fresh run dir under the daemon-owned, gitignored state tree.
    let run_dir = staging_root(root).join(format!("run-{}", now_rfc3339.replace([':', '.'], "-")));
    if let Err(e) = std::fs::create_dir_all(&run_dir) {
        return format!("I couldn't create the staging directory, sir — {e}.");
    }
    // mlx_lm.lora reads train.jsonl (+ valid.jsonl) from --data for --train, and
    // test.jsonl for --test. The held-out split feeds BOTH valid.jsonl (training
    // log) and test.jsonl (the promotion eval) — those examples are NOT in
    // train.jsonl, so the eval loss is a genuine held-out measurement.
    let (held_out, train) = examples.split_at(examples.len() / 10);
    if let Err(e) = std::fs::write(run_dir.join("train.jsonl"), render_jsonl(train)) {
        return format!("I couldn't write the training data, sir — {e}.");
    }
    let _ = std::fs::write(run_dir.join("valid.jsonl"), render_jsonl(held_out));
    let _ = std::fs::write(run_dir.join("test.jsonl"), render_jsonl(held_out));

    let mut manifest = Manifest {
        created: now_rfc3339,
        base_model: cfg.distill.base_model.clone(),
        example_count: examples.len(),
        status: RunStatus::Prepared,
        staging_dir: run_dir.to_string_lossy().to_string(),
        promoted: false, // training NEVER promotes; promote_last does, on a measured win.
        held_out_base_loss: None,
        held_out_adapter_loss: None,
    };
    write_manifest(root, &run_dir, &manifest);

    // Device-gated training. On a machine without mlx-lm / Apple Silicon this
    // fails honestly (Failed manifest + spoken reason); the staged dataset
    // stays, ready to train on-device.
    let cmd = train_command(
        &cfg.distill.python,
        &cfg.distill.base_model,
        &run_dir.to_string_lossy(),
        &run_dir.to_string_lossy(),
        cfg.distill.iters,
    );
    let adapter = run_dir.join("adapters.safetensors");
    let outcome = run_training(&cmd, || adapter.exists(), run).await;
    // Re-check the adapter after the run (the canned/real runner may have
    // written it); the outcome already folded adapter_present, so trust it.
    match outcome {
        TrainOutcome::Ok => {
            manifest.status = RunStatus::Trained;
            write_manifest(root, &run_dir, &manifest);
            format!(
                "Trained a personal adapter from {} of your redacted turns, sir — it's STAGED under state/lora, not live. Promotion is a separate, deliberate step; I never swap the live model on my own.",
                manifest.example_count
            )
        }
        TrainOutcome::Failed(why) => {
            manifest.status = RunStatus::Failed;
            write_manifest(root, &run_dir, &manifest);
            format!(
                "I staged {} redacted examples under state/lora, sir, but the training run didn't complete: {why}. On-device training needs Apple Silicon + mlx-lm.",
                manifest.example_count
            )
        }
    }
}

/// Write the run manifest into its dir AND update `last.json` (what the status
/// surface reads). Best-effort — a failed write leaves the run un-recorded, not
/// an error.
fn write_manifest(root: &std::path::Path, run_dir: &std::path::Path, manifest: &Manifest) {
    if let Ok(json) = serde_json::to_vec_pretty(manifest) {
        let _ = std::fs::write(run_dir.join("manifest.json"), &json);
        let staging = staging_root(root);
        let _ = std::fs::create_dir_all(&staging);
        let _ = std::fs::write(staging.join("last.json"), &json);
    }
}

/// Read the most recent run's manifest, or None. Best-effort — a missing or
/// malformed manifest is simply "no last run", never an error.
fn read_last_manifest(root: &std::path::Path) -> Option<Manifest> {
    let path = staging_root(root).join("last.json");
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------------------
// Tests — pure selection/rendering/manifest/command/status exhaustively; the
// training fold via a canned runner (the real subprocess is never spawned).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(a: &str, b: &str) -> (String, String) {
        (a.to_string(), b.to_string())
    }

    #[test]
    fn selection_keeps_good_pairs_redacts_dedups_and_drops_corrected() {
        let transcripts = vec![
            pair("what's the capital of France", "Paris."),
            pair("email bob@x.io the deck", "I'll draft that."), // redacted recipient
            pair("what's the capital of France", "Paris again."), // dup prompt
            pair("play jazz", "no, the other playlist"),          // corrected -> dropped
            pair("   ", "empty prompt"),                          // empty after trim
        ];
        // The trace store's redacted form of the corrected utterance.
        let corrected: HashSet<String> = [crate::optimize::redact("play jazz")].into_iter().collect();

        let ex = select_candidates(&transcripts, &corrected);
        assert_eq!(ex.len(), 2, "dedup + corrected + empty all dropped: {ex:?}");
        assert_eq!(ex[0].prompt, "what's the capital of France");
        // The recipient email is masked in the stored prompt.
        assert!(ex[1].prompt.contains("[redacted]"), "PII redacted: {}", ex[1].prompt);
        assert!(!render_jsonl(&ex).contains("bob@x.io"), "no raw PII in the dataset");
    }

    #[test]
    fn a_long_redirected_turn_is_dropped_symmetric_truncation() {
        // A >FIELD_CHARS turn the user redirected: the corrected key is built
        // via the SAME clean_field transform (as corrected_utterances does), so
        // the truncated prompt matches and the confirmed-wrong turn is dropped.
        let long = "please summarize ".to_string() + &"x".repeat(FIELD_CHARS);
        let transcripts = vec![pair(&long, "here you go")];
        let corrected: HashSet<String> = [clean_field(&long)].into_iter().collect();
        let ex = select_candidates(&transcripts, &corrected);
        assert!(ex.is_empty(), "a long redirected turn must not become a positive example: {ex:?}");
        // Sanity: without the corrected flag it IS kept (proves the drop is the
        // filter, not the length).
        assert_eq!(select_candidates(&transcripts, &HashSet::new()).len(), 1);
    }

    #[test]
    fn selection_bounds_the_dataset() {
        let transcripts: Vec<(String, String)> =
            (0..(MAX_EXAMPLES + 50)).map(|i| pair(&format!("q{i}"), "a")).collect();
        let ex = select_candidates(&transcripts, &HashSet::new());
        assert_eq!(ex.len(), MAX_EXAMPLES);
    }

    #[test]
    fn jsonl_is_the_mlx_chat_shape() {
        let ex = vec![Example { prompt: "hi".into(), response: "hello".into() }];
        let jsonl = render_jsonl(&ex);
        let v: Value = serde_json::from_str(jsonl.trim()).unwrap();
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["messages"][0]["content"], "hi");
        assert_eq!(v["messages"][1]["role"], "assistant");
        assert_eq!(v["messages"][1]["content"], "hello");
        assert!(jsonl.ends_with('\n'));
    }

    #[test]
    fn train_command_is_the_exact_mlx_lora_argv_never_a_shell_string() {
        let cmd = train_command("python3", "mlx-community/Qwen3-4B", "/data", "/out", 200);
        assert_eq!(cmd.program, "python3");
        assert_eq!(
            cmd.args,
            [
                "-m", "mlx_lm.lora", "--model", "mlx-community/Qwen3-4B", "--train",
                "--data", "/data", "--adapter-path", "/out", "--iters", "200",
                "--batch-size", "1"
            ]
        );
        // No arg is a shell metacharacter carrier.
        assert!(cmd.args.iter().all(|a| !a.contains("&&") && !a.contains('|') && !a.contains(';')));
    }

    #[tokio::test]
    async fn training_fold_maps_runner_results_and_never_fabricates_success() {
        let cmd = train_command("python3", "m", "/d", "/o", 10);
        // Clean exit + adapter present -> Ok.
        let ok = run_training(&cmd, || true, |_p, _a| async { Ok(0) }).await;
        assert_eq!(ok, TrainOutcome::Ok);
        // Clean exit but NO adapter -> Failed (never a fabricated success).
        let no_adapter = run_training(&cmd, || false, |_p, _a| async { Ok(0) }).await;
        assert!(matches!(no_adapter, TrainOutcome::Failed(ref w) if w.contains("no adapter")));
        // Non-zero exit -> Failed with the code.
        let nonzero = run_training(&cmd, || true, |_p, _a| async { Ok(2) }).await;
        assert!(matches!(nonzero, TrainOutcome::Failed(ref w) if w.contains("code 2")));
        // Could not spawn -> Failed with the reason.
        let errd = run_training(&cmd, || true, |_p, _a| async { Err("no python".into()) }).await;
        assert!(matches!(errd, TrainOutcome::Failed(ref w) if w.contains("could not run")));
    }

    #[test]
    fn status_is_honest_about_off_readiness_and_gated_promotion() {
        // Off: not ready, zero examples reported, dep unverified, no live adapter.
        let off = status_payload(false, 100, None, None);
        assert_eq!(off["enabled"], false);
        assert_eq!(off["ready_to_train"], false);
        assert_eq!(off["dep_verified"], false, "the daemon never fabricates device readiness");
        assert_eq!(off["gated_promotion"], true, "promotion is always gated on a measured win");
        assert_eq!(off["adapter_live"], false);
        assert!(off["last_run"].is_null());
        assert!(off["promoted"].is_null());

        // On + enough examples: dataset ready (device gate still separate).
        let ready = status_payload(true, MIN_EXAMPLES, None, None);
        assert_eq!(ready["ready_to_train"], true);
        // On but thin: not ready.
        let thin = status_payload(true, MIN_EXAMPLES - 1, None, None);
        assert_eq!(thin["ready_to_train"], false);
        assert_eq!(thin["min_examples"], MIN_EXAMPLES);

        // A last run surfaces its secret-free summary; a TRAINED-but-unpromoted
        // adapter reports promoted=false and no live adapter.
        let m = Manifest {
            created: "2026-07-13T10:00:00Z".into(),
            base_model: "mlx-community/Qwen3-4B".into(),
            example_count: 120,
            status: RunStatus::Trained,
            staging_dir: "state/lora/run-1".into(),
            promoted: false,
            held_out_base_loss: None,
            held_out_adapter_loss: None,
        };
        let with_run = status_payload(true, 200, Some(&m), None);
        assert_eq!(with_run["last_run"]["status"], "trained");
        assert_eq!(with_run["last_run"]["example_count"], 120);
        assert_eq!(with_run["last_run"]["promoted"], false);
        assert_eq!(with_run["adapter_live"], false);
        // The staging path (a location, not a secret) is not leaked to the wire.
        assert!(!with_run.to_string().contains("state/lora/run-1"));

        // A PROMOTED adapter surfaces the live summary WITH its measured losses.
        let live = Manifest {
            promoted: true,
            held_out_base_loss: Some(2.5),
            held_out_adapter_loss: Some(2.2),
            ..m.clone()
        };
        let with_live = status_payload(true, 200, Some(&m), Some(&live));
        assert_eq!(with_live["adapter_live"], true);
        assert_eq!(with_live["promoted"]["promoted"], true);
        assert_eq!(with_live["promoted"]["held_out_adapter_loss"], 2.2);
        assert_eq!(with_live["promoted"]["held_out_base_loss"], 2.5);
    }

    // -- the orchestrator, hermetic: canned runner + temp dirs, no DB, no spawn.

    struct TempDir(std::path::PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn tempdir(tag: &str) -> TempDir {
        let p = std::env::temp_dir().join(format!("darwin-distill-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }

    async fn mem_with_turns(tag: &str, n: usize) -> (crate::memory::Memory, TempDir) {
        let dir = tempdir(tag);
        let mem = crate::memory::Memory::open(&dir.0.join("m.db")).unwrap();
        for i in 0..n {
            mem.record_transcript(None, &format!("question number {i}"), "conversation", "local", Some(&format!("answer number {i}")))
                .await
                .unwrap();
        }
        (mem, dir)
    }

    #[tokio::test]
    async fn distill_now_is_off_by_default_and_never_touches_disk() {
        let (mem, _md) = mem_with_turns("off", 100).await;
        let root = tempdir("off-root");
        let cfg = crate::config::Config::default(); // distill ships OFF
        let reply = distill_now(&cfg, &mem, &root.0, "2026-07-13T10:00:00Z".into(), |_p, _a| async {
            panic!("training must NEVER run when off");
        })
        .await;
        assert!(reply.contains("off"), "{reply}");
        assert!(!staging_root(&root.0).exists(), "off path must not stage anything");
    }

    #[tokio::test]
    async fn distill_now_refuses_a_thin_dataset_honestly() {
        let (mem, _md) = mem_with_turns("thin", 5).await;
        let root = tempdir("thin-root");
        let mut cfg = crate::config::Config::default();
        cfg.distill.enabled = true;
        let reply = distill_now(&cfg, &mem, &root.0, "2026-07-13T10:00:00Z".into(), |_p, _a| async {
            panic!("must not train a thin dataset");
        })
        .await;
        assert!(reply.contains("Not enough"), "{reply}");
    }

    #[tokio::test]
    async fn distill_now_stages_trains_via_canned_runner_and_never_promotes() {
        let (mem, _md) = mem_with_turns("full", 80).await;
        let root = tempdir("full-root");
        let mut cfg = crate::config::Config::default();
        cfg.distill.enabled = true;

        // A canned runner that "writes the adapter" (creates the file the fold
        // checks) and exits 0 — the real subprocess is never spawned.
        let root_path = root.0.clone();
        let reply = distill_now(&cfg, &mem, &root.0, "2026-07-13T10:00:00Z".into(), move |_p, args| {
            // The argv is the real mlx_lm.lora command; the adapter path is --data.
            let data_idx = args.iter().position(|a| a == "--data").unwrap() + 1;
            let run_dir = std::path::PathBuf::from(&args[data_idx]);
            async move {
                std::fs::write(run_dir.join("adapters.safetensors"), b"fake").unwrap();
                Ok(0)
            }
        })
        .await;

        assert!(reply.contains("STAGED"), "reply says staged-not-live: {reply}");
        assert!(reply.to_lowercase().contains("not live") || reply.contains("STAGED"));
        // The dataset + a Trained manifest were staged under state/lora, promoted=false.
        let last: Manifest =
            serde_json::from_slice(&std::fs::read(staging_root(&root_path).join("last.json")).unwrap()).unwrap();
        assert_eq!(last.status, RunStatus::Trained);
        assert!(!last.promoted, "training NEVER promotes");
        assert_eq!(last.example_count, 80);
    }

    #[tokio::test]
    async fn distill_now_reports_a_failed_run_honestly_and_keeps_the_dataset() {
        let (mem, _md) = mem_with_turns("fail", 80).await;
        let root = tempdir("fail-root");
        let mut cfg = crate::config::Config::default();
        cfg.distill.enabled = true;
        // Runner exits non-zero (e.g. mlx-lm missing) — honest Failed, dataset kept.
        let reply = distill_now(&cfg, &mem, &root.0, "2026-07-13T10:00:00Z".into(), |_p, _a| async {
            Ok(1)
        })
        .await;
        assert!(reply.contains("didn't complete"), "{reply}");
        assert!(reply.contains("Apple Silicon"), "names the device dependency: {reply}");
        let last: Manifest =
            serde_json::from_slice(&std::fs::read(staging_root(&root.0).join("last.json")).unwrap()).unwrap();
        assert_eq!(last.status, RunStatus::Failed);
        assert!(!last.promoted);
    }

    #[test]
    fn manifest_round_trips_and_never_encodes_a_promoted_true_by_default() {
        let m = Manifest {
            created: "2026-07-13T10:00:00Z".into(),
            base_model: "b".into(),
            example_count: 40,
            status: RunStatus::Prepared,
            staging_dir: "state/lora/x".into(),
            promoted: false,
            held_out_base_loss: None,
            held_out_adapter_loss: None,
        };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"status\":\"prepared\""));
        assert!(s.contains("\"promoted\":false"));
        // The eval fields are omitted until measured (skip_serializing_if None).
        assert!(!s.contains("held_out_base_loss"), "unmeasured loss is omitted, not null: {s}");
        let back: Manifest = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
        // With measured losses they round-trip.
        let measured = Manifest { held_out_base_loss: Some(2.4), held_out_adapter_loss: Some(2.1), ..m };
        let s2 = serde_json::to_string(&measured).unwrap();
        assert!(s2.contains("held_out_adapter_loss"));
        assert_eq!(serde_json::from_str::<Manifest>(&s2).unwrap(), measured);
    }

    // -- MEASURED PROMOTION: the eval command, loss parse, the gate, and the
    // hermetic promote_last orchestration (injected eval runner; no spawn).

    #[test]
    fn eval_command_is_the_exact_mlx_test_argv_base_and_adapter() {
        // BASE: --adapter-path is EMPTY (mlx_lm's "test without LoRA layers"; an
        // omitted flag would default to the dir "adapters" and fail).
        let base = eval_command("python3", "mlx-community/Qwen3-4B", "/run", None);
        assert_eq!(
            base.args,
            ["-m", "mlx_lm.lora", "--model", "mlx-community/Qwen3-4B", "--data", "/run",
             "--test", "--adapter-path", "", "--batch-size", "1"]
        );
        // ADAPTER: --adapter-path is the staged run dir (same base + data + test).
        let adapter = eval_command("python3", "mlx-community/Qwen3-4B", "/run", Some("/run"));
        assert!(adapter.args.windows(2).any(|w| w[0] == "--adapter-path" && w[1] == "/run"));
        assert!(adapter.args.contains(&"--test".to_string()));
    }

    #[test]
    fn parse_test_loss_reads_the_summary_and_rejects_noise() {
        assert_eq!(parse_test_loss("Test loss 2.345, Test ppl 10.434"), Some(2.345));
        assert_eq!(parse_test_loss("...\nIter 100\nTest loss: 1.5\n"), Some(1.5));
        assert_eq!(parse_test_loss("test loss = 0.9"), Some(0.9)); // case + '='
        // No summary line -> None (unmeasurable never counts as a win).
        assert_eq!(parse_test_loss("Iter 200: Val loss 3.1"), None);
        assert_eq!(parse_test_loss(""), None);
        assert_eq!(parse_test_loss("Test loss banana"), None);
    }

    #[test]
    fn promotion_gate_promotes_only_on_a_measured_margin_win() {
        use PromotionDecision::*;
        // Clear win >= margin -> Promote.
        assert!(matches!(
            promotion_decision(Some(2.5), Some(2.2), 0.05),
            Promote { improvement, .. } if (improvement - 0.3).abs() < 1e-9
        ));
        // Win below the margin -> Reject (noise doesn't flip the live model).
        assert!(matches!(promotion_decision(Some(2.5), Some(2.49), 0.05), Reject { .. }));
        // Exactly the margin -> Promote (>=). Exactly-representable floats so the
        // boundary is the gate's `>=`, not float rounding: 3.0 - 2.0 == 1.0.
        assert!(matches!(promotion_decision(Some(3.0), Some(2.0), 1.0), Promote { .. }));
        // A regression (adapter worse) -> Reject.
        assert!(matches!(promotion_decision(Some(2.0), Some(2.4), 0.05), Reject { .. }));
        // An unmeasurable side -> Reject, never promote.
        assert!(matches!(promotion_decision(None, Some(2.0), 0.05), Reject { improvement: None, .. }));
        assert!(matches!(promotion_decision(Some(2.0), None, 0.05), Reject { improvement: None, .. }));
        // NaN -> Reject (non-finite never wins).
        assert!(matches!(promotion_decision(Some(f64::NAN), Some(1.0), 0.05), Reject { .. }));
    }

    #[tokio::test]
    async fn promote_last_promotes_on_a_measured_win_and_is_reversible() {
        let root = tempdir("promote-win");
        let mut cfg = crate::config::Config::default();
        cfg.distill.enabled = true;
        cfg.distill.min_improvement = 0.05;
        // Stage a "trained" run with an adapter file + a Trained last.json.
        let run_dir = staging_root(&root.0).join("run-x");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("adapters.safetensors"), b"weights").unwrap();
        std::fs::write(run_dir.join("test.jsonl"), "{}\n").unwrap();
        let manifest = Manifest {
            created: "2026-07-13T10-00-00Z".into(),
            base_model: cfg.distill.base_model.clone(),
            example_count: 80,
            status: RunStatus::Trained,
            staging_dir: run_dir.to_string_lossy().to_string(),
            promoted: false,
            held_out_base_loss: None,
            held_out_adapter_loss: None,
        };
        write_manifest(&root.0, &run_dir, &manifest);

        // Injected eval runner: base loss 2.5, adapter loss 2.2 (a 0.3 win).
        let reply = promote_last(&cfg, &root.0, |_p, args| {
            let is_adapter = args.windows(2).any(|w| w[0] == "--adapter-path" && !w[1].is_empty());
            async move {
                Ok(if is_adapter { "Test loss 2.200".to_string() } else { "Test loss 2.500".to_string() })
            }
        })
        .await;
        assert!(reply.contains("Promoted"), "a measured win promotes: {reply}");
        // The live pointer exists with the measured losses; adapter_live true.
        let live = read_promoted_manifest(&root.0).expect("promoted manifest");
        assert!(live.promoted);
        assert_eq!(live.held_out_base_loss, Some(2.5));
        assert_eq!(live.held_out_adapter_loss, Some(2.2));
        assert!(promoted_dir(&root.0).join("adapters.safetensors").exists(), "adapter copied live");
        // Reversible: rollback removes the live pointer.
        clear_promotion(&root.0).unwrap();
        assert!(read_promoted_manifest(&root.0).is_none(), "rollback reverts to base");
    }

    #[tokio::test]
    async fn promote_last_refuses_without_a_measured_win_and_keeps_base_live() {
        let root = tempdir("promote-nogo");
        let mut cfg = crate::config::Config::default();
        cfg.distill.enabled = true;
        let run_dir = staging_root(&root.0).join("run-y");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(run_dir.join("adapters.safetensors"), b"weights").unwrap();
        let manifest = Manifest {
            created: "2026-07-13T10-00-00Z".into(),
            base_model: cfg.distill.base_model.clone(),
            example_count: 80,
            status: RunStatus::Trained,
            staging_dir: run_dir.to_string_lossy().to_string(),
            promoted: false,
            held_out_base_loss: None,
            held_out_adapter_loss: None,
        };
        write_manifest(&root.0, &run_dir, &manifest);
        // Adapter is WORSE (2.6 vs base 2.5) -> NO promotion, base stays live.
        let reply = promote_last(&cfg, &root.0, move |_p, args| {
            let is_adapter = args.windows(2).any(|w| w[0] == "--adapter-path" && !w[1].is_empty());
            async move {
                Ok(if is_adapter { "Test loss 2.600".to_string() } else { "Test loss 2.500".to_string() })
            }
        })
        .await;
        assert!(reply.contains("did NOT promote"), "a non-win is refused honestly: {reply}");
        assert!(read_promoted_manifest(&root.0).is_none(), "no adapter goes live without a win");
        // The measured (non-)result is still recorded in the run manifest.
        let last = read_last_manifest(&root.0).unwrap();
        assert_eq!(last.held_out_base_loss, Some(2.5));
        assert_eq!(last.held_out_adapter_loss, Some(2.6));
        assert!(!last.promoted);
    }

    #[tokio::test]
    async fn promote_last_is_off_by_default_and_needs_a_trained_run() {
        let root = tempdir("promote-guards");
        let cfg_off = crate::config::Config::default(); // distill OFF
        let r = promote_last(&cfg_off, &root.0, |_p, _a| async { Ok(String::new()) }).await;
        assert!(r.contains("off"), "{r}");
        // On, but no trained run staged -> honest "nothing to promote".
        let mut cfg = crate::config::Config::default();
        cfg.distill.enabled = true;
        let r2 = promote_last(&cfg, &root.0, |_p, _a| async { Ok(String::new()) }).await;
        assert!(r2.contains("no trained adapter") || r2.contains("nothing to promote"), "{r2}");
    }
}
