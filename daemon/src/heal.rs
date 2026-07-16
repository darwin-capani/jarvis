//! Self-heal v2: an error-burst watchdog that DIAGNOSES, drafts MULTIPLE
//! candidate fixes, validates each independently behind the same hard gates,
//! adversarially self-reviews the survivors, and proposes the best one for a
//! human to apply.
//!
//! Pipeline (every gate is hard and NEVER weakened):
//!   1. TRIGGER — edge-triggered ERROR burst in state/logs/daemon.log
//!      (>= 5 ERROR-level lines in 60s), or a single total-loss line
//!      ("audio capture stopped": the capture thread died once and is never
//!      respawned — one line, permanent deafness, no burst will follow).
//!   2. GATES — [self_heal] enabled must be true (else heal.suppressed); at
//!      most one draft attempt per 6h (meta.heal_last_attempt); a cloud key
//!      must resolve (else heal.blocked{reason:"no_api_key"}).
//!   3. DIAGNOSIS (v2) — extract the error signature(s), the cited source
//!      files + line numbers, a window of surrounding log context, and the
//!      implicated subsystem (audio/inference/router/...) by module path.
//!      Emits heal.diagnosing{signature, files, subsystem}.
//!   4. MULTI-CANDIDATE DRAFT (v2) — ask the heavy model (claude-opus-4-8) for
//!      N=2-3 ALTERNATIVE minimal unified-diff patches (distinct approaches,
//!      each minimal, no new deps). Each is parsed/cleaned; non-diffs rejected.
//!   5. STAGE + VALIDATE EACH (v2) — every candidate is staged independently
//!      in state/heal/staging-<ts>-c<i>/ (sources copied, diff applied with
//!      /usr/bin/patch -p1 --batch, cargo check && cargo test). Any candidate
//!      that fails a hunk/compile/test is DISCARDED. Gates reused unchanged.
//!   6. ADVERSARIAL SELF-REVIEW (v2) — a second cloud call judges each
//!      surviving (validated) diff against the diagnosis + its test output:
//!      does it fix the ROOT CAUSE (not just silence the symptom)? Returns a
//!      verdict + confidence 0..1.
//!   7. SELECT — prefer the MINIMAL patch with the HIGHEST review confidence
//!      among those that PASSED validation.
//!      8a. mode="propose" (default) — write state/heal/proposals/<ts>/{patch.diff,
//!      report.md, diagnosis.json, candidates.md, review.md}, stamp
//!      meta.heal_pending=<ts>, emit heal.proposal{ts, files, validated:true,
//!      confidence}. scripts/apply_heal.sh <ts> applies it on human request.
//!      8b. mode="auto" (requires enabled=true; documented DANGEROUS) — apply the
//!      same validated diff to the real daemon/, cargo build --release, emit
//!      heal.applied, then EXIT cleanly for a supervised restart. UNCHANGED
//!      from v1: there is still no NEW live-auto-apply path.
//!      Any patch/validation failure of ALL candidates → state/heal/rejected/<ts>/
//!   + heal.rejected{ts, stage}.
//!
//! SAFETY CONTRACT (non-negotiable): self-heal ships enabled=false /
//! mode=propose; there is NO path that touches the live daemon/ without an
//! explicit human running scripts/apply_heal.sh (except the pre-existing,
//! documented-dangerous opt-in auto mode); the staged `cargo check` + full
//! `cargo test` gates are NEVER dropped or weakened. The cloud is reached ONLY
//! through the HealBrain trait — unit tests mock it; the only real cloud path
//! is the verifier's --heal-drill.
//!
//! The watchdog's own output must never feed back into its trigger: every log
//! line this module writes is WARN/INFO level, and the detector matches the
//! level *token*, never message text.

use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

use crate::anthropic;
use crate::config::Config;
use crate::memory::Memory;
use crate::telemetry;

const CHECK_INTERVAL: Duration = Duration::from_secs(10);
const BURST_WINDOW_SECS: i64 = 60;
const BURST_LIMIT: usize = 5;
const TAIL_BYTES: u64 = 64 * 1024;
/// One of these inside an ERROR-level line is an immediate trigger even
/// alone: a total-loss event that emits exactly one line and never recurs
/// (the audio capture thread exits and is not respawned), so the burst
/// counter would never see it (audit fix).
const TOTAL_LOSS_MARKERS: &[&str] = &["audio capture stopped"];

/// Rate limit: at most one draft attempt (cloud call) per this many seconds.
const ATTEMPT_INTERVAL_SECS: u64 = 6 * 3600;
const META_HEAL_LAST_ATTEMPT: &str = "meta.heal_last_attempt";
const META_HEAL_PENDING: &str = "meta.heal_pending";

/// daemon.log context handed to the drafter.
const CONTEXT_LINES: usize = 80;
/// Burst lines kept for the prompt and the report.
const BURST_LINE_CAP: usize = 20;

/// How many alternative candidate diffs we ask the heavy model for (v2).
const CANDIDATE_COUNT: usize = 3;

/// Draft call: heavy model, latency-insensitive, room for thinking + diffs.
const DRAFT_MAX_TOKENS: u32 = 8192;
const DRAFT_TIMEOUT: Duration = Duration::from_secs(240);
/// Review call: a verdict + confidence is short; still allow thinking room.
const REVIEW_MAX_TOKENS: u32 = 4096;
const REVIEW_TIMEOUT: Duration = Duration::from_secs(180);

const DRAFT_SYSTEM: &str = "You are DARWIN's self-repair drafter: an expert Rust engineer who \
     produces minimal unified diffs. Respond with ONLY the diff(s) — no prose outside the \
     requested structure, no code fences inside a diff.";
const REVIEW_SYSTEM: &str = "You are DARWIN's adversarial self-repair reviewer: a skeptical \
     senior Rust engineer. You judge whether a candidate patch fixes the ROOT CAUSE of a fault \
     (not merely silences the symptom) and has no obvious side effects. Be harsh; a passing \
     test suite is necessary but NOT sufficient.";

/// Staging validation: cargo check && cargo test share this deadline.
const VALIDATE_TIMEOUT: Duration = Duration::from_secs(600);
const PATCH_BIN: &str = "/usr/bin/patch";
/// Validation output tail kept in report.md / candidates.md.
const REPORT_TAIL_CHARS: usize = 4000;

// ---------------------------------------------------------------------------
// Cloud seam (trait) — the ONLY route to the cloud. Production uses CloudBrain
// (anthropic::complete_plain); unit tests inject a mock so no cloud call is
// ever made under `cargo test`. The verifier's --heal-drill is the one real
// cloud path.
// ---------------------------------------------------------------------------

/// A `Send` future returned by the trait methods. Spelled out explicitly so
/// the trait stays object-safe (`&dyn HealBrain`) WITHOUT pulling in the
/// async-trait crate (the "no new dependencies" rule applies to the daemon
/// too): the production path and every mock implement these two methods.
type BrainFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// The drafter+reviewer seam. Both methods are latency-insensitive cloud
/// calls; impls own their own timeouts. Errors are surfaced (the pipeline
/// rejects the attempt rather than guessing). This is the ONLY route to the
/// cloud — unit tests inject a mock so no cloud call is made under
/// `cargo test`; the verifier's --heal-drill is the one real cloud path.
pub trait HealBrain: Send + Sync {
    /// Draft up to `n` ALTERNATIVE minimal unified-diff patches for the given
    /// diagnosis. Returns the raw model text (multi-diff, parsed by the
    /// caller via split_candidate_diffs/clean_diff).
    fn draft_candidates<'a>(&'a self, diagnosis: &'a Diagnosis, n: usize) -> BrainFuture<'a>;

    /// Adversarially review one surviving (validated) diff against the
    /// diagnosis + its captured validation output. Returns the raw model text
    /// (parsed by the caller via parse_review).
    fn review<'a>(
        &'a self,
        diagnosis: &'a Diagnosis,
        diff: &'a str,
        validation_tail: &'a str,
    ) -> BrainFuture<'a>;
}

/// Production HealBrain: the heavy Anthropic model via anthropic.rs. Holds the
/// model id so the drill and the watchdog share one impl.
pub struct CloudBrain {
    pub model: String,
}

impl HealBrain for CloudBrain {
    fn draft_candidates<'a>(&'a self, diagnosis: &'a Diagnosis, n: usize) -> BrainFuture<'a> {
        Box::pin(async move {
            anthropic::complete_plain(
                &self.model,
                DRAFT_MAX_TOKENS,
                DRAFT_SYSTEM,
                &draft_prompt(diagnosis, n),
                DRAFT_TIMEOUT,
            )
            .await
        })
    }

    fn review<'a>(
        &'a self,
        diagnosis: &'a Diagnosis,
        diff: &'a str,
        validation_tail: &'a str,
    ) -> BrainFuture<'a> {
        Box::pin(async move {
            anthropic::complete_plain(
                &self.model,
                REVIEW_MAX_TOKENS,
                REVIEW_SYSTEM,
                &review_prompt(diagnosis, diff, validation_tail),
                REVIEW_TIMEOUT,
            )
            .await
        })
    }
}

/// Every 10s, tail state/logs/daemon.log and look for an error burst
/// (>= 5 ERROR-level lines within the last 60s) or a total-loss line.
/// Edge-triggered: one pipeline run per episode, re-armed only after the
/// burst clears.
pub async fn watchdog(root: PathBuf, cfg: Arc<Config>, memory: Arc<Memory>) {
    let log_path = root.join("state").join("logs").join("daemon.log");
    let mut interval = tokio::time::interval(CHECK_INTERVAL);
    let mut in_burst = false;
    let brain = CloudBrain {
        model: cfg.cloud.heavy_model.clone(),
    };
    loop {
        interval.tick().await;
        let scan = match scan_log(&log_path) {
            Ok(scan) => scan,
            Err(_) => continue, // log not written yet; nothing to inspect
        };
        if !scan.triggered() {
            in_burst = false; // episode over; re-arm
            continue;
        }
        if in_burst {
            continue; // already handled this episode
        }
        in_burst = true;
        if !cfg.self_heal.enabled {
            warn!(
                errors_last_60s = scan.burst_count,
                total_loss = scan.total_loss,
                "heal: error burst detected but self_heal.enabled = false; would diagnose, draft \
                 N candidate diffs via the heavy model, stage+validate each, adversarially review \
                 the survivors, and propose (or auto-apply) per [self_heal].mode"
            );
            telemetry::emit(
                "system",
                "heal.suppressed",
                json!({
                    "errors_last_60s": scan.burst_count,
                    "total_loss": scan.total_loss,
                    "reason": "self_heal.enabled = false",
                }),
            );
            continue;
        }
        telemetry::emit(
            "system",
            "heal.triggered",
            json!({"errors_last_60s": scan.burst_count, "total_loss": scan.total_loss}),
        );
        run_pipeline(&root, &cfg, &memory, &brain, &scan).await;
    }
}

// ---------------------------------------------------------------------------
// Trigger detection
// ---------------------------------------------------------------------------

/// What one tail inspection saw.
#[derive(Debug, Default)]
struct LogScan {
    /// ERROR-level lines inside the burst window.
    burst_count: usize,
    /// An in-window ERROR line carried a total-loss marker.
    total_loss: bool,
    /// The in-window ERROR lines, oldest first, capped at BURST_LINE_CAP.
    burst_lines: Vec<String>,
    /// The raw log tail (for the ~80-line drafter context).
    tail: String,
}

impl LogScan {
    fn triggered(&self) -> bool {
        self.burst_count >= BURST_LIMIT || self.total_loss
    }
}

/// True only when the line's level field is ERROR. The tracing fmt layout is
/// `<rfc3339-ts> <LEVEL> <target>: <msg>` (the level may be space-padded), so
/// the level is the second whitespace-separated token — substring-matching
/// the whole line would also count INFO lines whose message text quotes
/// "ERROR" (logged responses/utterances) and the watchdog's own warnings.
fn is_error_line(line: &str) -> bool {
    let mut fields = line.split_whitespace();
    let _ts = fields.next();
    fields.next() == Some("ERROR")
}

/// An ERROR line announcing an unrecoverable one-shot loss.
fn is_total_loss_line(line: &str) -> bool {
    is_error_line(line) && TOTAL_LOSS_MARKERS.iter().any(|m| line.contains(m))
}

/// Inspect the log tail: count ERROR-level lines whose leading RFC3339
/// timestamp falls within the burst window, collect them for the drafter,
/// and flag total-loss lines. Lines without a parseable timestamp count
/// conservatively (better a false trigger than a missed one in a watchdog).
fn scan_log(path: &Path) -> std::io::Result<LogScan> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)?;
    let tail = String::from_utf8_lossy(&raw).into_owned();
    Ok(scan_tail(tail))
}

/// Pure half of scan_log, separable for tests.
fn scan_tail(tail: String) -> LogScan {
    let cutoff = Utc::now() - chrono::Duration::seconds(BURST_WINDOW_SECS);
    let mut scan = LogScan::default();
    for line in tail.lines().rev() {
        if !is_error_line(line) {
            continue;
        }
        let ts = line
            .split_whitespace()
            .next()
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok());
        match ts {
            Some(t) if t.with_timezone(&Utc) >= cutoff => {}
            // Older than the window; everything before is older still.
            Some(_) => break,
            // Unparseable timestamp: count conservatively.
            None => {}
        }
        scan.burst_count += 1;
        scan.total_loss = scan.total_loss || is_total_loss_line(line);
        if scan.burst_lines.len() < BURST_LINE_CAP {
            scan.burst_lines.push(line.to_string());
        }
    }
    scan.burst_lines.reverse(); // collected newest-first; report oldest-first
    scan.tail = tail;
    scan
}

// ---------------------------------------------------------------------------
// (3) Root-cause diagnosis (v2) — pure, unit-tested
// ---------------------------------------------------------------------------

/// A structured root-cause diagnosis built from the burst, before any cloud
/// work. Serialized verbatim to state/heal/proposals/<ts>/diagnosis.json.
#[derive(Debug, Clone, Serialize)]
pub struct Diagnosis {
    /// The dominant error signature(s) — the message text of the ERROR lines
    /// with volatile tails (timestamps, paths, "error=...") trimmed, so a
    /// recurring fault collapses to one stable line per distinct cause.
    pub signatures: Vec<String>,
    /// Cited daemon source files (and any `:line`s found alongside them).
    pub files: Vec<String>,
    /// Line numbers cited next to a src/<file>.rs:<line> reference, in
    /// first-seen order (a hint for the drafter; may be empty).
    pub line_numbers: Vec<u32>,
    /// The implicated subsystem inferred from the module path in the ERROR
    /// target field (audio/inference/router/...) or "unknown".
    pub subsystem: String,
    /// The window of surrounding log context (the last CONTEXT_LINES of tail).
    pub log_context: String,
    /// The burst lines verbatim, oldest first (also in the report).
    pub burst_lines: Vec<String>,
    /// Current contents of the cited source files, read from the crate being
    /// healed (path -> body), so the drafter can produce a unified diff whose
    /// hunk context actually matches the tree and applies cleanly with
    /// `patch -p1`. Empty until attach_source_excerpts() runs (build_diagnosis
    /// stays pure/IO-free); a file that cannot be read is simply omitted.
    #[serde(default)]
    pub source_excerpts: Vec<(String, String)>,
}

impl Diagnosis {
    /// The one-line signature the heal.diagnosing event carries (the first /
    /// dominant signature, or a fallback when none parsed).
    fn primary_signature(&self) -> String {
        self.signatures
            .first()
            .cloned()
            .unwrap_or_else(|| "unclassified error burst".to_string())
    }
}

/// Known daemon subsystems, matched against the module-path target token of an
/// ERROR line (`darwin_core::<subsystem>::...`). First match in burst order
/// wins; "unknown" when nothing matches (e.g. a bare `darwin_core` target).
const SUBSYSTEMS: &[&str] = &[
    "audio",
    "inference",
    "router",
    "speech",
    "playback",
    "actions",
    "anthropic",
    "memory",
    "apps",
    "genproxy",
    "proactive",
    "reflect",
    "heal",
];

/// The tracing target token of a log line: the 3rd whitespace field, stripped
/// of a trailing ':'. For `<ts> ERROR darwin_core::router: msg` that is
/// `darwin_core::router`.
fn target_token(line: &str) -> Option<&str> {
    let mut fields = line.split_whitespace();
    fields.next()?; // ts
    fields.next()?; // level
    fields.next().map(|t| t.trim_end_matches(':'))
}

/// Infer the subsystem from the module path of the first burst line whose
/// target names a known subsystem.
fn infer_subsystem(burst_lines: &[String]) -> String {
    for line in burst_lines {
        if let Some(target) = target_token(line) {
            for sub in SUBSYSTEMS {
                // Match `darwin_core::<sub>` or `darwin_core::<sub>::...`.
                let needle = format!("::{sub}");
                if target.ends_with(&needle) || target.contains(&format!("{needle}::")) {
                    return (*sub).to_string();
                }
            }
        }
    }
    "unknown".to_string()
}

/// Reduce one ERROR line to a stable signature: drop the leading timestamp +
/// level + target, then trim the volatile `error=...`/`err=...` tail so the
/// same recurring fault collapses to one signature regardless of the exact
/// transient detail.
fn error_signature(line: &str) -> Option<String> {
    if !is_error_line(line) {
        return None;
    }
    // Everything after the first ": " (the message), else after the target.
    let msg = line.split_once(": ").map(|x| x.1).unwrap_or(line).trim();
    // Trim a volatile detail tail introduced by " error=" / " err=".
    let cut = msg
        .find(" error=")
        .or_else(|| msg.find(" err="))
        .unwrap_or(msg.len());
    let sig = msg[..cut].trim().to_string();
    (!sig.is_empty()).then_some(sig)
}

/// Distinct error signatures across the burst, in first-seen order.
fn extract_signatures(burst_lines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in burst_lines {
        if let Some(sig) = error_signature(line) {
            if !out.contains(&sig) {
                out.push(sig);
            }
        }
    }
    out
}

/// Line numbers cited as `src/<file>.rs:<line>` across the text, first-seen,
/// deduplicated.
fn extract_line_numbers(text: &str) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::new();
    for (idx, _) in text.match_indices(".rs:") {
        let rest = &text[idx + 4..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(n) = digits.parse::<u32>() {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// Build the structured diagnosis from a scan. Pure (no cloud, no IO).
fn build_diagnosis(scan: &LogScan) -> Diagnosis {
    let burst_lines = scan.burst_lines.clone();
    let burst_excerpt = burst_lines.join("\n");
    Diagnosis {
        signatures: extract_signatures(&burst_lines),
        files: extract_source_files(&burst_excerpt),
        line_numbers: extract_line_numbers(&burst_excerpt),
        subsystem: infer_subsystem(&burst_lines),
        log_context: last_lines(&scan.tail, CONTEXT_LINES),
        burst_lines,
        source_excerpts: Vec::new(),
    }
}

/// Largest source file body handed to the drafter, per file (chars). A patch
/// drafter needs the real lines to produce an applying hunk; cap so a huge file
/// cannot blow the prompt budget — the cited line numbers still point the model
/// at the right region.
const SOURCE_EXCERPT_CAP: usize = 12_000;

/// Read the current contents of each cited source file from `crate_dir`
/// (impure; kept OUT of build_diagnosis so that stays unit-testable without
/// IO). Files that cannot be read are skipped. Paths are crate-root-relative
/// (e.g. "src/router.rs"), exactly as they appear in the burst — the same form
/// the drafted diff's a//b/ headers use, so the model sees and patches the same
/// path. Reading is confined to <crate_dir>/src to avoid escaping the tree via
/// a crafted log path.
fn attach_source_excerpts(d: &mut Diagnosis, crate_dir: &Path) {
    let src_root = crate_dir.join("src");
    for rel in &d.files {
        // Only files under src/ (the crate sources we ever patch); a path that
        // does not normalize to within src_root is ignored.
        let full = crate_dir.join(rel);
        let Ok(canon) = full.canonicalize() else { continue };
        let Ok(src_canon) = src_root.canonicalize() else { continue };
        if !canon.starts_with(&src_canon) {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&canon) {
            let body = first_chars(&body, SOURCE_EXCERPT_CAP);
            d.source_excerpts.push((rel.clone(), body));
        }
    }
}

/// The first `n` chars of `s` (mirrors anthropic::first_chars, kept local).
fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

// ---------------------------------------------------------------------------
// Pure pipeline helpers (each unit-tested)
// ---------------------------------------------------------------------------

/// What the enabled/mode pair permits. Unknown modes degrade to Propose —
/// never to Auto — so a typo can only make self-heal safer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealAction {
    Disabled,
    Propose,
    Auto,
}

fn heal_action(enabled: bool, mode: &str) -> HealAction {
    if !enabled {
        return HealAction::Disabled;
    }
    match mode.trim() {
        "auto" => HealAction::Auto,
        _ => HealAction::Propose, // "propose" and anything unknown
    }
}

/// Rate-limit math: a draft attempt is allowed when no stamp exists, the
/// stamp is unparseable, or it is older than ATTEMPT_INTERVAL_SECS. A stamp
/// from the future (clock skew) blocks — saturating_sub yields 0.
fn attempt_allowed(last_attempt: Option<&str>, now_secs: u64) -> bool {
    match last_attempt.and_then(|v| v.trim().parse::<u64>().ok()) {
        Some(last) => now_secs.saturating_sub(last) > ATTEMPT_INTERVAL_SECS,
        None => true,
    }
}

/// Daemon source files named in log text: every "src/<path>.rs" occurrence
/// (the panic/log convention is "src/<file>.rs:<line>"), deduplicated in
/// first-seen order. Also applied to a drafted diff to list files touched.
fn extract_source_files(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for (idx, _) in text.match_indices("src/") {
        let rest = &text[idx..];
        let mut end = 0;
        for (i, c) in rest.char_indices() {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '-') {
                end = i + c.len_utf8();
            } else {
                break;
            }
        }
        let token = &rest[..end];
        if let Some(pos) = token.find(".rs") {
            let path = token[..pos + 3].to_string();
            if !found.contains(&path) {
                found.push(path);
            }
        }
    }
    found
}

/// Staging directory name for candidate `i` (0-based) of one attempt. v2
/// stages each candidate independently so survivors never collide.
fn staging_dir_name(ts: u64, candidate: usize) -> String {
    format!("staging-{ts}-c{candidate}")
}

/// The last `n` lines of `text`, newline-joined.
fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// The last `n` chars of `s` (validation output can be huge).
fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

/// Crude size of a diff for the min-patch tiebreak: number of added/removed
/// lines (lines starting with a single +/- that are not the ---/+++ headers).
fn diff_size(diff: &str) -> usize {
    diff.lines()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
        })
        .count()
}

/// The v2 multi-candidate drafter prompt: diagnosis in, N labelled diffs out.
fn draft_prompt(d: &Diagnosis, n: usize) -> String {
    let file_list = if d.files.is_empty() {
        "(no src/<file>.rs paths appeared in the burst; infer the most likely file from the log \
         and touch only that one)"
            .to_string()
    } else {
        d.files.join(", ")
    };
    let sigs = if d.signatures.is_empty() {
        "(no clean signature extracted; read the burst lines below)".to_string()
    } else {
        d.signatures.join("\n  - ")
    };
    let burst_excerpt = d.burst_lines.join("\n");
    let sources = if d.source_excerpts.is_empty() {
        "(source contents unavailable; infer the surrounding code from the log)".to_string()
    } else {
        d.source_excerpts
            .iter()
            .map(|(path, body)| format!("--- {path} (current contents) ---\n{body}"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    format!(
        "The DARWIN daemon (a Rust crate; sources under src/) hit an error burst and needs a \
         minimal source fix.\n\n\
         Diagnosis:\n\
         - subsystem: {subsystem}\n\
         - implicated files: {file_list}\n\
         - error signature(s):\n  - {sigs}\n\n\
         Error-burst lines:\n{burst_excerpt}\n\n\
         Current contents of the implicated source file(s) — your diff MUST match these exact \
         lines so it applies with `patch -p1`:\n{sources}\n\n\
         Recent daemon.log context (last {CONTEXT_LINES} lines):\n{log_context}\n\n\
         Propose {n} ALTERNATIVE, DISTINCT minimal fixes that address the ROOT CAUSE (not just \
         silence the symptom). Output EXACTLY {n} unified diffs, each preceded by a header line \
         of the form `=== CANDIDATE i ===` (i = 1..{n}). Rules for every diff:\n\
         - Paths relative to the crate root with a/ and b/ prefixes (e.g. --- a/src/router.rs).\n\
         - Touch only the implicated files; make the smallest change that fixes the cause.\n\
         - No new dependencies; do not modify Cargo.toml or Cargo.lock.\n\
         - Each diff must apply cleanly with `patch -p1` and pass `cargo check` and `cargo test`.\n\
         - No prose inside or between the diffs beyond the `=== CANDIDATE i ===` markers.",
        subsystem = d.subsystem,
        sigs = sigs,
        log_context = d.log_context,
    )
}

/// The v2 adversarial review prompt: diagnosis + one validated diff + its test
/// output in, a strict verdict + confidence out.
fn review_prompt(d: &Diagnosis, diff: &str, validation_tail: &str) -> String {
    let sigs = if d.signatures.is_empty() {
        "(none extracted)".to_string()
    } else {
        d.signatures.join("; ")
    };
    format!(
        "A candidate patch PASSED staged validation (`cargo check` + full `cargo test`). Judge \
         whether it fixes the ROOT CAUSE of the fault below, not merely silences the symptom, and \
         whether it has any obvious side effects or regressions.\n\n\
         Fault diagnosis:\n\
         - subsystem: {subsystem}\n\
         - signature(s): {sigs}\n\n\
         Candidate diff:\n{diff}\n\n\
         Staged validation output (tail):\n{validation_tail}\n\n\
         Respond on EXACTLY two lines, nothing else:\n\
         VERDICT: <one sentence: does it fix the root cause, and any side-effect concerns>\n\
         CONFIDENCE: <a single number 0.0-1.0>",
        subsystem = d.subsystem,
        sigs = sigs,
    )
}

/// Belt-and-braces cleanup of one model diff: strip code fences, any leading
/// prose before the first diff header, and a trailing `=== CANDIDATE ... ===`
/// marker. None when no unified diff is present at all (a refusal or prose
/// answer must never reach patch).
fn clean_diff(raw: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    let mut started = false;
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue; // fence open/close
        }
        // A candidate marker terminates this diff (defensive: split should
        // already have removed it).
        if trimmed.trim_start().starts_with("=== CANDIDATE") {
            if started {
                break;
            }
            continue;
        }
        if !started {
            if trimmed.starts_with("--- ")
                || trimmed.starts_with("diff ")
                || trimmed.starts_with("Index: ")
            {
                started = true;
            } else {
                continue; // leading prose
            }
        }
        lines.push(line);
    }
    if !lines.iter().any(|l| l.starts_with("--- "))
        || !lines.iter().any(|l| l.starts_with("+++ "))
        || !lines.iter().any(|l| l.starts_with("@@"))
    {
        return None;
    }
    // Path-confinement: `patch -p1` is run with cwd = the target dir on this
    // model-drafted diff. macOS /usr/bin/patch honors `..` in `---`/`+++` hunk
    // headers, so a header like `+++ b/src/../../../../tmp/x` would write OUTSIDE
    // the staging dir (and, on auto_apply, outside daemon/). This is the single
    // chokepoint every candidate flows through (split_candidate_diffs ->
    // clean_diff), so reject any header that, after the `-p1` strip, is empty,
    // absolute, or contains a `..` component — mirroring forge::is_confined_relpath
    // and dropping the candidate exactly like any non-diff. Legitimate heal diffs
    // use `a/src/...`/`b/src/...` headers, which strip to `src/...` and survive.
    for line in &lines {
        if let Some(rest) = line.strip_prefix("--- ").or_else(|| line.strip_prefix("+++ ")) {
            // The path token is the field before any trailing tab/whitespace+timestamp.
            let path = rest.split('\t').next().unwrap_or(rest).trim_end();
            if path == "/dev/null" {
                continue; // new-file / deleted-file sentinel — not a real target
            }
            // Mirror `-p1`: strip exactly one leading path component (up to and
            // including the first '/').
            let stripped = match path.find('/') {
                Some(i) => &path[i + 1..],
                None => "",
            };
            if stripped.is_empty()
                || stripped.starts_with('/')
                || stripped.split('/').any(|seg| seg == "..")
            {
                return None; // escape attempt — drop the candidate before patch runs
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n'); // patch(1) wants a final newline
    Some(out)
}

/// Split the multi-candidate model response on `=== CANDIDATE i ===` markers
/// and clean each block into a diff. Blocks that are not valid diffs are
/// dropped. If NO markers appear at all, fall back to treating the whole
/// response as a single diff (a model that ignored the format still gives us
/// one candidate). Returns diffs in document order, deduplicated.
fn split_candidate_diffs(raw: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut saw_marker = false;
    for line in raw.lines() {
        if line.trim_start().starts_with("=== CANDIDATE") {
            saw_marker = true;
            if !current.trim().is_empty() {
                blocks.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        blocks.push(current);
    }
    if !saw_marker {
        // No markers: the whole thing is at most one candidate.
        blocks = vec![raw.to_string()];
    }
    let mut diffs: Vec<String> = Vec::new();
    for block in blocks {
        if let Some(d) = clean_diff(&block) {
            if !diffs.contains(&d) {
                diffs.push(d);
            }
        }
    }
    diffs
}

// ---------------------------------------------------------------------------
// (6)+(7) Adversarial review parsing + survivor selection — pure, unit-tested
// ---------------------------------------------------------------------------

/// A surviving candidate that PASSED staged validation, plus its review.
#[derive(Debug, Clone)]
struct Survivor {
    /// 1-based candidate index, for the report.
    index: usize,
    diff: String,
    files: Vec<String>,
    validation_tail: String,
    review_verdict: String,
    confidence: f64,
    /// Added/removed line count — the min-patch tiebreak.
    size: usize,
}

/// Parse the reviewer's `VERDICT:`/`CONFIDENCE:` reply into (verdict,
/// confidence). A missing/garbled confidence is treated as 0.0 (conservative:
/// an unparseable review never wins selection over a clearly-scored peer). The
/// confidence is clamped to 0..1.
fn parse_review(raw: &str) -> (String, f64) {
    let mut verdict = String::new();
    let mut confidence = 0.0f64;
    for line in raw.lines() {
        let t = line.trim();
        if let Some(rest) = strip_label(t, "VERDICT") {
            verdict = rest.trim().to_string();
        } else if let Some(rest) = strip_label(t, "CONFIDENCE") {
            confidence = parse_confidence(rest);
        }
    }
    if verdict.is_empty() {
        verdict = raw.trim().lines().next().unwrap_or("").trim().to_string();
    }
    (verdict, confidence.clamp(0.0, 1.0))
}

/// Case-insensitive `LABEL:` / `LABEL ` prefix strip.
fn strip_label<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let lower = line.to_ascii_lowercase();
    let lab = label.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix(&lab) {
        // Re-slice the ORIGINAL (preserve case) past the matched prefix and a
        // following ':' / whitespace.
        let consumed = line.len() - rest.len();
        let after = line[consumed..].trim_start_matches([':', ' ', '\t']);
        Some(after)
    } else {
        None
    }
}

/// First float-looking token in `s`, clamped later by the caller.
fn parse_confidence(s: &str) -> f64 {
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else if !num.is_empty() {
            break;
        }
    }
    num.parse::<f64>().unwrap_or(0.0)
}

/// Selection policy (v2): among PASSED candidates, prefer the HIGHEST review
/// confidence; break ties toward the MINIMAL patch (smallest add/remove count).
/// Returns the index into `survivors` of the winner, or None when empty. Pure
/// so the rule is unit-tested without the cloud.
fn select_winner(survivors: &[Survivor]) -> Option<usize> {
    survivors
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                // Higher confidence wins; on a tie, SMALLER size wins, so
                // reverse the size comparison.
                .then(b.size.cmp(&a.size))
        })
        .map(|(i, _)| i)
}

// ---------------------------------------------------------------------------
// Artifact rendering — pure, unit-tested
// ---------------------------------------------------------------------------

/// report.md for a v2 proposal: diagnosis, chosen diff, validation tail,
/// review verdict + confidence, and the EXACT apply command.
fn render_report(ts: u64, model: &str, d: &Diagnosis, winner: &Survivor) -> String {
    let files = if winner.files.is_empty() {
        "(none parsed from the diff)".to_string()
    } else {
        winner.files.join(", ")
    };
    let sigs = if d.signatures.is_empty() {
        "(none extracted)".to_string()
    } else {
        d.signatures.join("\n  - ")
    };
    format!(
        "# Self-heal proposal — {ts}\n\n\
         - verdict: VALIDATED (cargo check + cargo test passed in staging)\n\
         - model: {model}\n\
         - subsystem: {subsystem}\n\
         - files touched: {files}\n\
         - chosen candidate: #{index}\n\
         - review confidence: {confidence:.2}\n\n\
         ## Diagnosis\n\n\
         - signature(s):\n  - {sigs}\n\
         - cited line numbers: {lines}\n\n\
         ## Chosen diff\n\n```diff\n{diff}```\n\n\
         ## Adversarial review verdict\n\n{verdict}\n\n\
         ## Validation output (tail)\n\n```\n{validation_tail}\n```\n\n\
         ## To apply\n\n\
         This patch was validated in a STAGING copy only; the live daemon/ is untouched.\n\
         Review the diff above, then apply it with:\n\n\
         ```\nscripts/apply_heal.sh {ts}\n```\n",
        subsystem = d.subsystem,
        index = winner.index,
        confidence = winner.confidence,
        sigs = sigs,
        lines = if d.line_numbers.is_empty() {
            "(none)".to_string()
        } else {
            d.line_numbers
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        },
        diff = winner.diff,
        verdict = winner.review_verdict,
        validation_tail = winner.validation_tail,
    )
}

/// A short report.md for a fully-rejected attempt (no candidate validated).
fn render_rejection_report(ts: u64, model: &str, d: &Diagnosis, summary: &str) -> String {
    let sigs = if d.signatures.is_empty() {
        "(none extracted)".to_string()
    } else {
        d.signatures.join("\n  - ")
    };
    format!(
        "# Self-heal REJECTED — {ts}\n\n\
         - verdict: REJECTED (no candidate passed every gate)\n\
         - model: {model}\n\
         - subsystem: {subsystem}\n\n\
         ## Diagnosis\n\n- signature(s):\n  - {sigs}\n\n\
         ## Why every candidate was discarded\n\n{summary}\n",
        subsystem = d.subsystem,
    )
}

/// candidates.md: every candidate diff with why it was kept or discarded.
fn render_candidates_md(outcomes: &[CandidateOutcome]) -> String {
    let mut out = String::from("# Self-heal candidates\n\n");
    for o in outcomes {
        out.push_str(&format!(
            "## Candidate #{index} — {verdict}\n\n{detail}\n\n```diff\n{diff}```\n\n",
            index = o.index,
            verdict = o.verdict_label(),
            detail = o.detail,
            diff = o.diff,
        ));
    }
    out
}

/// review.md: the chosen candidate's adversarial review verdict + confidence.
fn render_review_md(winner: &Survivor) -> String {
    format!(
        "# Adversarial self-review — chosen candidate #{index}\n\n\
         - confidence: {confidence:.2}\n\n## Verdict\n\n{verdict}\n",
        index = winner.index,
        confidence = winner.confidence,
        verdict = winner.review_verdict,
    )
}

// ---------------------------------------------------------------------------
// Pipeline (impure half)
// ---------------------------------------------------------------------------

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One candidate's fate, for candidates.md.
struct CandidateOutcome {
    index: usize,
    diff: String,
    /// "validated" | "rejected"
    validated: bool,
    /// e.g. "kept (review confidence 0.82)", "discarded at cargo test".
    detail: String,
}

impl CandidateOutcome {
    fn verdict_label(&self) -> &'static str {
        if self.validated {
            "VALIDATED"
        } else {
            "DISCARDED"
        }
    }
}

async fn run_pipeline(
    root: &Path,
    cfg: &Config,
    memory: &Memory,
    brain: &dyn HealBrain,
    scan: &LogScan,
) {
    let ts = now_secs();
    // LOCKDOWN OVERLAY (task #12): self-heal is autonomy, so it is FORCED off
    // while the emergency stop is engaged — the enabled bit is ANDed with
    // `!is_locked_down()`, so the pure `heal_action` returns Disabled and the
    // pipeline exits before any cloud drafting. `heal_action` itself stays pure
    // (the global read lives here, at the one live call site). With lockdown OFF
    // this is byte-for-byte the configured `[self_heal].enabled`.
    let enabled = cfg.self_heal.enabled && !crate::lockdown::is_locked_down();
    let action = heal_action(enabled, &cfg.self_heal.mode);
    if action == HealAction::Disabled {
        return; // caller already gates; belt and braces
    }

    // Rate limit BEFORE any cloud work: one draft attempt per 6h.
    let last = match memory.get_fact(META_HEAL_LAST_ATTEMPT).await {
        Ok(last) => last,
        Err(e) => {
            // Conservative: broken bookkeeping must not unleash unmetered
            // cloud drafting.
            warn!(error = %e, "heal: cannot read the attempt stamp; skipping this episode");
            return;
        }
    };
    if !attempt_allowed(last.as_deref(), ts) {
        info!("heal: rate-limited (one draft attempt per 6h); skipping this episode");
        telemetry::emit("system", "heal.blocked", json!({"reason": "rate_limited", "ts": ts}));
        return;
    }

    // Drafting needs the cloud: no key, no pipeline.
    if anthropic::resolve_api_key().await.is_none() {
        warn!("heal: triggered but no Anthropic API key is available; cannot draft a patch");
        telemetry::emit("system", "heal.blocked", json!({"reason": "no_api_key", "ts": ts}));
        return;
    }

    // Stamp the attempt right before any cloud call so failed attempts count
    // toward the limit too (each one is a paid cloud call).
    if let Err(e) = memory.upsert_fact(META_HEAL_LAST_ATTEMPT, &ts.to_string()).await {
        // FAIL-SAFE (mirrors forge_gap): if the attempt stamp cannot be persisted
        // we CANNOT enforce the one-draft-per-6h rate limit, so we must NOT make
        // the paid cloud draft call — broken bookkeeping must never unleash
        // unmetered drafting (the conservative rule above). Skip this episode.
        warn!(error = %e, "heal: failed to stamp the attempt time; skipping to avoid unmetered drafting");
        return;
    }

    let daemon_dir = root.join("daemon");
    let heal_root = root.join("state").join("heal");
    match run_attempt(&daemon_dir, &heal_root, ts, &cfg.cloud.heavy_model, brain, scan).await {
        AttemptResult::Proposed { diff, report, files, confidence, .. } => match action {
            HealAction::Propose => {
                propose(memory, &heal_root, ts, &diff, &report, &files, confidence).await;
                // CHANGE QUEUE (changeq.rs): ALSO register this propose-only artifact
                // into the unified git-native review lane. Pure bookkeeping — the
                // validated patch was already written to state/heal/proposals/<ts>/;
                // this mirrors it into the queue (and, on-device, onto darwin/changeq)
                // with secret-free provenance. It changes NOTHING about the
                // propose-only contract; apply still routes to scripts/apply_heal.sh.
                crate::changeq::on_proposal(
                    crate::changeq::ChangeKind::Heal,
                    ts,
                    crate::changeq::Provenance::new(
                        "self-heal",
                        cfg.cloud.heavy_model.clone(),
                        ts.to_string(),
                        crate::changeq::fingerprint(diff.as_bytes()),
                    ),
                    format!(
                        "validated patch, {} file{}, review confidence {confidence:.2}",
                        files.len(),
                        if files.len() == 1 { "" } else { "s" }
                    ),
                );
            }
            HealAction::Auto => {
                // SAFETY SNAPSHOT (snapshot.rs): anchor an APFS restore point
                // BEFORE the validated diff is applied to the live daemon/, so a
                // later "undo that" can name a concrete OS-level rollback target.
                // Additive-benign (a COW marker; writes/deletes none of the user's
                // data) and armed by default; a non-APFS/no-space/no-permission
                // volume degrades to an honest would-have and changes nothing. It
                // NEVER rolls back on its own — auto_apply still applies exactly as
                // before; the snapshot is only a recorded restore point.
                crate::snapshot::anchor_before(crate::snapshot::Reason::HealApply, cfg).await;
                auto_apply(&daemon_dir, &heal_root, ts, &diff, &report).await;
            }
            HealAction::Disabled => unreachable!("gated above"),
        },
        AttemptResult::Rejected { stage, diff, report } => {
            warn!(stage, "heal: all candidates rejected");
            // record a best-effort patch.diff (last attempted) + report for audit.
            let dir = heal_root.join("rejected");
            record_artifact(&dir, ts, "patch.diff", &diff);
            record_artifact(&dir, ts, "report.md", &report);
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": stage}));
        }
        AttemptResult::Aborted { stage } => {
            warn!(stage, "heal: attempt aborted (no verdict on any patch)");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": stage}));
        }
    }
}

/// The full v2 attempt, factored out so the --heal-drill reuses it verbatim
/// against a planted-fault crate. `daemon_dir` is the crate to heal (the live
/// daemon/ for the watchdog; a throwaway temp crate for the drill); `heal_root`
/// is where staging dirs and artifacts go. NEVER applies to `daemon_dir`.
async fn run_attempt(
    daemon_dir: &Path,
    heal_root: &Path,
    ts: u64,
    model: &str,
    brain: &dyn HealBrain,
    scan: &LogScan,
) -> AttemptResult {
    // (3) Diagnosis. build_diagnosis is pure; attach the current contents of
    // the cited source files (impure IO, confined to <crate_dir>/src) so the
    // drafter can produce a hunk whose context matches the tree and applies
    // cleanly. This strengthens drafting only — every staged gate is unchanged.
    let mut diagnosis = build_diagnosis(scan);
    attach_source_excerpts(&mut diagnosis, daemon_dir);
    info!(
        subsystem = %diagnosis.subsystem,
        files = ?diagnosis.files,
        "heal: diagnosed the burst"
    );
    telemetry::emit(
        "system",
        "heal.diagnosing",
        json!({
            "signature": diagnosis.primary_signature(),
            "files": diagnosis.files,
            "subsystem": diagnosis.subsystem,
        }),
    );

    // (4) Multi-candidate draft.
    let raw = match brain.draft_candidates(&diagnosis, CANDIDATE_COUNT).await {
        Ok(raw) => raw,
        Err(e) => {
            warn!(error = %e, "heal: draft call failed");
            return AttemptResult::Aborted { stage: "draft" };
        }
    };
    let candidate_diffs = split_candidate_diffs(&raw);
    if candidate_diffs.is_empty() {
        warn!("heal: the model returned no usable unified diff");
        let report = render_rejection_report(
            ts,
            model,
            &diagnosis,
            "The model returned no parseable unified diff in any candidate.",
        );
        return AttemptResult::Rejected {
            stage: "draft",
            diff: tail_chars(&raw, REPORT_TAIL_CHARS),
            report,
        };
    }

    // (5) Stage + validate EACH candidate independently (gates unchanged).
    let mut survivors: Vec<Survivor> = Vec::new();
    let mut outcomes: Vec<CandidateOutcome> = Vec::new();
    let mut last_stage = "patch";
    for (i, diff) in candidate_diffs.iter().enumerate() {
        let files = extract_source_files(diff);
        match stage_and_validate(daemon_dir, heal_root, ts, i, diff).await {
            Ok(StageResult::Validated { validation_tail }) => {
                // (6) Adversarial review of this survivor.
                let (verdict, confidence) =
                    match brain.review(&diagnosis, diff, &validation_tail).await {
                        Ok(raw) => parse_review(&raw),
                        Err(e) => {
                            warn!(error = %e, candidate = i + 1, "heal: review call failed; \
                                 treating as zero-confidence");
                            ("review call failed".to_string(), 0.0)
                        }
                    };
                outcomes.push(CandidateOutcome {
                    index: i + 1,
                    diff: diff.clone(),
                    validated: true,
                    detail: format!(
                        "kept — passed cargo check + cargo test; review confidence {confidence:.2}"
                    ),
                });
                survivors.push(Survivor {
                    index: i + 1,
                    diff: diff.clone(),
                    files,
                    validation_tail,
                    review_verdict: verdict,
                    confidence,
                    size: diff_size(diff),
                });
            }
            Ok(StageResult::Rejected { stage, detail }) => {
                last_stage = stage;
                outcomes.push(CandidateOutcome {
                    index: i + 1,
                    diff: diff.clone(),
                    validated: false,
                    detail: format!(
                        "discarded at {stage}:\n```\n{}\n```",
                        tail_chars(&detail, 1200)
                    ),
                });
            }
            Err(e) => {
                warn!(error = %e, candidate = i + 1, "heal: staging infrastructure failed");
                outcomes.push(CandidateOutcome {
                    index: i + 1,
                    diff: diff.clone(),
                    validated: false,
                    detail: format!("discarded: staging infrastructure error: {e}"),
                });
            }
        }
    }

    let candidates_md = render_candidates_md(&outcomes);

    // (7) Select the winner.
    let Some(win_idx) = select_winner(&survivors) else {
        warn!("heal: no candidate passed validation");
        let report = render_rejection_report(
            ts,
            model,
            &diagnosis,
            "No candidate passed the staged cargo check + cargo test gates.",
        );
        // Persist candidates.md alongside the rejection so the human can see
        // what was tried.
        let dir = heal_root.join("rejected");
        record_artifact(&dir, ts, "candidates.md", &candidates_md);
        record_artifact(&dir, ts, "diagnosis.json", &diagnosis_json(&diagnosis));
        return AttemptResult::Rejected {
            stage: last_stage,
            diff: candidate_diffs
                .last()
                .cloned()
                .unwrap_or_default(),
            report,
        };
    };
    let winner = survivors[win_idx].clone();
    let report = render_report(ts, model, &diagnosis, &winner);
    let review_md = render_review_md(&winner);
    let diagnosis_json = diagnosis_json(&diagnosis);

    AttemptResult::Proposed {
        diff: winner.diff.clone(),
        report,
        files: winner.files.clone(),
        confidence: winner.confidence,
        extra: ProposalArtifacts {
            diagnosis_json,
            candidates_md,
            review_md,
        },
    }
}

/// Outcome of one heal attempt, decoupled from how it is acted on (propose,
/// auto, drill).
enum AttemptResult {
    Proposed {
        diff: String,
        report: String,
        files: Vec<String>,
        confidence: f64,
        extra: ProposalArtifacts,
    },
    /// A model/patch/validation failure — we have a verdict (no candidate is
    /// good), so it is recorded as a rejection.
    Rejected {
        stage: &'static str,
        diff: String,
        report: String,
    },
    /// Infrastructure trouble before any verdict (draft call failed). No
    /// statement about any patch.
    Aborted { stage: &'static str },
}

/// The supplementary proposal artifacts written alongside patch.diff/report.md.
struct ProposalArtifacts {
    diagnosis_json: String,
    candidates_md: String,
    review_md: String,
}

fn diagnosis_json(d: &Diagnosis) -> String {
    serde_json::to_string_pretty(d).unwrap_or_else(|_| "{}".to_string())
}

/// (8a) Propose: artifacts + meta.heal_pending + heal.proposal. The
/// first-contact brief reads meta.heal_pending, so DARWIN tells the user.
#[allow(clippy::too_many_arguments)]
async fn propose(
    memory: &Memory,
    heal_root: &Path,
    ts: u64,
    diff: &str,
    report: &str,
    files: &[String],
    confidence: f64,
) {
    propose_with_extra(memory, heal_root, ts, diff, report, files, confidence, None).await;
}

/// Shared propose body; `extra` carries the v2 supplementary artifacts (the
/// watchdog supplies them; kept Optional so the surface is small).
#[allow(clippy::too_many_arguments)]
async fn propose_with_extra(
    memory: &Memory,
    heal_root: &Path,
    ts: u64,
    diff: &str,
    report: &str,
    files: &[String],
    confidence: f64,
    extra: Option<&ProposalArtifacts>,
) {
    let dir = heal_root.join("proposals");
    if record_artifact(&dir, ts, "patch.diff", diff).is_none() {
        return; // already warned
    }
    record_artifact(&dir, ts, "report.md", report);
    if let Some(extra) = extra {
        record_artifact(&dir, ts, "diagnosis.json", &extra.diagnosis_json);
        record_artifact(&dir, ts, "candidates.md", &extra.candidates_md);
        record_artifact(&dir, ts, "review.md", &extra.review_md);
    }
    if let Err(e) = memory.upsert_fact(META_HEAL_PENDING, &ts.to_string()).await {
        warn!(error = %e, "heal: proposal written but meta.heal_pending could not be stamped");
    }
    info!(ts, confidence, "heal: validated proposal written; apply with scripts/apply_heal.sh");
    telemetry::emit(
        "system",
        "heal.proposal",
        json!({"ts": ts, "files": files, "validated": true, "confidence": confidence}),
    );
}

/// (8b) Auto: apply the validated diff to the REAL daemon/, rebuild release,
/// emit heal.applied, then exit cleanly. UNCHANGED from v1 — no NEW
/// live-auto-apply path. Under launchd KeepAlive the exit is a restart into
/// the new binary; under `cargo run` it is a stop.
async fn auto_apply(daemon_dir: &Path, heal_root: &Path, ts: u64, diff: &str, report: &str) {
    let dir = heal_root.join("applied");
    record_artifact(&dir, ts, "patch.diff", diff); // audit trail
    record_artifact(&dir, ts, "report.md", report);
    match apply_patch(daemon_dir, diff).await {
        Ok(out) if out.ok => {}
        Ok(out) => {
            warn!(output = %tail_chars(&out.output, 800), "heal: auto-apply patch failed on the live tree");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "apply"}));
            return;
        }
        Err(e) => {
            warn!(error = %e, "heal: auto-apply could not run patch");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "apply"}));
            return;
        }
    }
    match run_cargo(daemon_dir, &["build", "--release"], VALIDATE_TIMEOUT).await {
        Ok(out) if out.ok => {}
        Ok(out) => {
            warn!(output = %tail_chars(&out.output, 800), "heal: release rebuild failed after auto-apply");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "build"}));
            return;
        }
        Err(e) => {
            warn!(error = %e, "heal: release rebuild could not run");
            telemetry::emit("system", "heal.rejected", json!({"ts": ts, "stage": "build"}));
            return;
        }
    }
    telemetry::emit("system", "heal.applied", json!({"ts": ts}));
    info!(
        ts,
        "heal: patch applied and rebuilt; exiting for a clean restart (launchd KeepAlive \
         restarts darwind into the new binary; under `cargo run` this is a stop)"
    );
    // Give the telemetry hub a beat to flush heal.applied to the HUD.
    tokio::time::sleep(Duration::from_millis(500)).await;
    std::process::exit(0);
}

/// Write `<dir_root>/<ts>/<name>` with `body`. Returns the file's directory on
/// success (so a missing dir is created once and reused for sibling files).
fn record_artifact(dir_root: &Path, ts: u64, name: &str, body: &str) -> Option<PathBuf> {
    let dir = dir_root.join(ts.to_string());
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(name), body)?;
        Ok(())
    };
    match write() {
        Ok(()) => Some(dir),
        Err(e) => {
            warn!(error = %e, dir = %dir.display(), name, "heal: failed to write artifact");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// (5) Staging + validation (real patch, real cargo — exercised in tests
// against a synthetic crate in a tempdir, NEVER against the real daemon/).
// REUSED UNCHANGED from v1; the only difference is the per-candidate staging
// dir name (staging-<ts>-c<i>).
// ---------------------------------------------------------------------------

/// How staging ended: a verdict on the patch, or Err for infrastructure
/// trouble (copy failed, spawn failed) that says nothing about the patch.
#[derive(Debug)]
enum StageResult {
    Validated { validation_tail: String },
    Rejected { stage: &'static str, detail: String },
}

/// Output of one child process: combined stdout+stderr and its success bit.
struct CmdOutput {
    ok: bool,
    output: String,
}

/// Copy the crate sources (src/, Cargo.toml, Cargo.lock if present — NOT
/// target/) into the staging dir, apply the diff with patch -p1 --batch
/// (reject on any hunk failure), then cargo check && cargo test under one
/// 10-minute deadline.
async fn stage_and_validate(
    source_dir: &Path,
    heal_root: &Path,
    ts: u64,
    candidate: usize,
    diff: &str,
) -> anyhow::Result<StageResult> {
    let staging = heal_root.join(staging_dir_name(ts, candidate));
    stage_sources(source_dir, &staging)?;

    let patched = apply_patch(&staging, diff).await?;
    if !patched.ok {
        return Ok(StageResult::Rejected {
            stage: "patch",
            detail: patched.output,
        });
    }

    let deadline = tokio::time::Instant::now() + VALIDATE_TIMEOUT;
    let mut combined = String::new();
    for (stage, args) in [("check", ["check"]), ("test", ["test"])] {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(StageResult::Rejected {
                stage,
                detail: format!("{combined}\n[validation deadline exhausted before cargo {stage}]"),
            });
        }
        match run_cargo(&staging, &args, remaining).await {
            Ok(out) => {
                combined.push_str(&format!("\n$ cargo {stage}\n"));
                combined.push_str(&out.output);
                if !out.ok {
                    return Ok(StageResult::Rejected { stage, detail: combined });
                }
            }
            Err(e) => {
                return Ok(StageResult::Rejected {
                    stage,
                    detail: format!("{combined}\n[cargo {stage} failed to run: {e}]"),
                })
            }
        }
    }
    Ok(StageResult::Validated {
        validation_tail: tail_chars(&combined, REPORT_TAIL_CHARS),
    })
}

/// Copy src/ (recursively), Cargo.toml, and Cargo.lock (when present) from
/// `source_dir` into `staging`. target/ is never touched.
fn stage_sources(source_dir: &Path, staging: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(staging)?;
    copy_tree(&source_dir.join("src"), &staging.join("src"))?;
    std::fs::copy(source_dir.join("Cargo.toml"), staging.join("Cargo.toml"))?;
    let lock = source_dir.join("Cargo.lock");
    if lock.exists() {
        std::fs::copy(&lock, staging.join("Cargo.lock"))?;
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// /usr/bin/patch -p1 --batch with the diff on stdin, cwd = `dir`. Exit
/// status != 0 (any failed hunk, malformed input) is a rejection.
async fn apply_patch(dir: &Path, diff: &str) -> anyhow::Result<CmdOutput> {
    let mut child = tokio::process::Command::new(PATCH_BIN)
        .args(["-p1", "--batch"])
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(diff.as_bytes()).await?;
        // Dropping stdin closes the pipe so patch sees EOF.
    }
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output()).await??;
    Ok(CmdOutput {
        ok: out.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    })
}

/// `cargo <args>` in `dir`, output captured, bounded by `timeout`. Uses the
/// $CARGO that invoked us when set (tests run under cargo) else PATH lookup.
async fn run_cargo(dir: &Path, args: &[&str], timeout: Duration) -> anyhow::Result<CmdOutput> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let child = tokio::process::Command::new(cargo)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let out = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => anyhow::bail!("cargo {} timed out after {}s", args.join(" "), timeout.as_secs()),
    };
    Ok(CmdOutput {
        ok: out.status.success(),
        output: format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    })
}

// ---------------------------------------------------------------------------
// (6 of contract) HEAL DRILL — the ONE real cloud path, invoked by the
// verifier via `darwind --heal-drill`. It runs the FULL real pipeline
// (diagnose -> Opus draft -> stage -> validate -> review -> propose) against a
// PLANTED FAULT in a throwaway temp crate. It NEVER touches the real daemon/.
// ---------------------------------------------------------------------------

/// A self-contained throwaway crate carrying a PLANTED COMPILE FAULT, and a
/// synthetic ERROR burst that names it. The drill heals THIS, not daemon/.
/// `[workspace]` keeps cargo from walking up into any enclosing workspace.
const DRILL_PLANTED_LIB: &str =
    "/// Multiply by two. (PLANTED FAULT: `y` is undefined — does not compile.)\n\
     pub fn double(x: i32) -> i32 {\n    x * y\n}\n";

fn drill_burst_scan() -> LogScan {
    let now = Utc::now().to_rfc3339();
    let lines = [
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
        format!("{now} ERROR darwin_core::router: compile guard failed in src/lib.rs:3 error=cannot find value `y` in this scope"),
    ];
    scan_tail(lines.join("\n"))
}

/// Run the full real self-heal pipeline against a planted fault in a temp
/// crate, drafting + reviewing via the REAL cloud (CloudBrain). Requires the
/// Anthropic key. Writes a real proposal artifact under `<tmp>/state/heal/
/// proposals/<ts>/`. Returns the proposal dir on success. The real daemon/ is
/// never touched.
///
/// Invoked by `darwind --heal-drill` (see main.rs); the model id is the
/// configured heavy model so the drill exercises exactly the production path.
pub async fn run_heal_drill(model: &str) -> anyhow::Result<PathBuf> {
    if anthropic::resolve_api_key().await.is_none() {
        anyhow::bail!("heal drill requires an Anthropic API key (none resolved)");
    }
    telemetry::init(); // safe if already initialized (OnceLock no-op)

    // Throwaway sandbox: <tmpdir>/darwin-heal-drill-<pid>-<ts>/.
    let ts = now_secs();
    let sandbox = std::env::temp_dir().join(format!(
        "darwin-heal-drill-{}-{ts}",
        std::process::id()
    ));
    let crate_dir = sandbox.join("daemon");
    let heal_root = sandbox.join("state").join("heal");
    std::fs::create_dir_all(crate_dir.join("src"))?;
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        "[package]\nname = \"darwin-heal-drill\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )?;
    std::fs::write(crate_dir.join("src").join("lib.rs"), DRILL_PLANTED_LIB)?;

    info!(sandbox = %sandbox.display(), model, "heal drill: running the FULL pipeline against a planted fault (cloud)");

    let brain = CloudBrain { model: model.to_string() };
    let scan = drill_burst_scan();

    let result = run_attempt(&crate_dir, &heal_root, ts, model, &brain, &scan).await;

    // The drill must end in a real proposal artifact — and must NOT have
    // touched the planted source (propose mode never applies to the source).
    let planted = std::fs::read_to_string(crate_dir.join("src").join("lib.rs"))?;
    if !planted.contains("x * y") {
        anyhow::bail!(
            "heal drill SAFETY VIOLATION: the planted source was modified (propose mode must \
             never touch the source tree)"
        );
    }

    match result {
        AttemptResult::Proposed { diff, report, files, confidence, extra } => {
            // Write the proposal artifacts exactly as the propose path does,
            // WITHOUT touching meta or emitting heal.proposal into a live HUD
            // (no Memory here): the drill proves the loop, it is not a live heal.
            let dir = heal_root.join("proposals").join(ts.to_string());
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("patch.diff"), &diff)?;
            std::fs::write(dir.join("report.md"), &report)?;
            std::fs::write(dir.join("diagnosis.json"), &extra.diagnosis_json)?;
            std::fs::write(dir.join("candidates.md"), &extra.candidates_md)?;
            std::fs::write(dir.join("review.md"), &extra.review_md)?;
            telemetry::emit(
                "system",
                "heal.proposal",
                json!({"ts": ts, "files": files, "validated": true, "confidence": confidence, "drill": true}),
            );
            info!(
                proposal = %dir.display(),
                confidence,
                "heal drill: PASSED — full pipeline produced a validated, reviewed proposal"
            );
            Ok(dir)
        }
        AttemptResult::Rejected { stage, .. } => {
            anyhow::bail!("heal drill: pipeline rejected every candidate at stage `{stage}`")
        }
        AttemptResult::Aborted { stage } => {
            anyhow::bail!("heal drill: pipeline aborted at stage `{stage}` (cloud/infra failure)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- trigger detection ---------------------------------------------------

    #[test]
    fn matches_only_the_level_field() {
        assert!(is_error_line(
            "2026-06-12T01:02:03.456789Z ERROR darwin_core::audio: capture stopped"
        ));
        // Level field is space-padded by the fmt layer; split_whitespace copes.
        assert!(is_error_line(
            "2026-06-12T01:02:03.456789Z  ERROR darwin_core::audio: capture stopped"
        ));
        // INFO line quoting "ERROR" in the message must not count.
        assert!(!is_error_line(
            "2026-06-12T01:02:03.456789Z  INFO darwin_core: responding response=\"The log shows ERROR entries\""
        ));
        // The watchdog's own WARN must not count.
        assert!(!is_error_line(
            "2026-06-12T01:02:03.456789Z  WARN darwin_core::heal: heal: error burst detected but self_heal.enabled = false"
        ));
    }

    /// Audit regression: the detector must fire on the EXACT lines the daemon
    /// now emits during a simulated inference outage (these messages were
    /// WARN-level before the fix, so the watchdog could never trigger).
    #[test]
    fn detector_fires_on_a_simulated_inference_outage() {
        let now = Utc::now().to_rfc3339();
        let tail = [
            format!("{now}  INFO darwin_core: darwind starting"),
            format!("{now} ERROR darwin_core: transcription failed; is the inference server up? error=inference socket unavailable at state/ipc/inference.sock"),
            format!("{now} ERROR darwin_core: classification failed error=inference classify timed out after 30s"),
            format!("{now} ERROR darwin_core::router: converse failed before any audio; falling back to generate+speak error=..."),
            format!("{now} ERROR darwin_core::router: local generate unavailable; falling back to raw data error=..."),
            format!("{now} ERROR darwin_core::router: cloud completion failed; degrading to local generate error=..."),
            format!("{now}  WARN darwin_core: fact extraction failed"),
        ]
        .join("\n");
        let scan = scan_tail(tail);
        assert_eq!(scan.burst_count, 5, "exactly the 5 ERROR lines: {:?}", scan.burst_lines);
        assert!(scan.triggered(), "an inference outage must trigger the pipeline");
        assert!(!scan.total_loss);
        assert_eq!(scan.burst_lines.len(), 5, "burst lines collected for the drafter");
        assert!(
            scan.burst_lines[0].contains("transcription failed"),
            "burst lines must be oldest-first: {:?}",
            scan.burst_lines
        );
    }

    /// The capture thread dies ONCE (it is never respawned) — a single line
    /// must trigger immediately; no burst will ever follow it.
    #[test]
    fn a_single_total_loss_line_triggers() {
        let now = Utc::now().to_rfc3339();
        let tail = format!(
            "{now}  INFO darwin_core::audio: audio capture running\n\
             {now} ERROR darwin_core::audio: audio capture stopped error=no default input device"
        );
        let scan = scan_tail(tail);
        assert_eq!(scan.burst_count, 1);
        assert!(scan.total_loss);
        assert!(scan.triggered());

        // The same words inside an INFO line must NOT trigger.
        let now = Utc::now().to_rfc3339();
        let scan = scan_tail(format!(
            "{now}  INFO darwin_core: responding response=\"audio capture stopped earlier, sir\""
        ));
        assert!(!scan.triggered());
    }

    #[test]
    fn stale_errors_outside_the_window_do_not_trigger() {
        let tail = (0..6)
            .map(|i| {
                format!("2020-01-01T00:00:0{i}.000000Z ERROR darwin_core: transcription failed; is the inference server up?")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let scan = scan_tail(tail);
        assert_eq!(scan.burst_count, 0);
        assert!(!scan.triggered());
    }

    #[test]
    fn four_errors_are_below_the_burst_limit() {
        let now = Utc::now().to_rfc3339();
        let tail = (0..4)
            .map(|_| format!("{now} ERROR darwin_core: classification failed"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!scan_tail(tail).triggered());
    }

    // -- (3) diagnosis extraction (v2) ---------------------------------------

    #[test]
    fn diagnosis_extracts_signature_files_subsystem_from_synthetic_lines() {
        let now = Utc::now().to_rfc3339();
        let tail = [
            format!("{now}  INFO darwin_core: darwind starting"),
            format!("{now} ERROR darwin_core::router: converse failed at src/router.rs:122 error=socket closed"),
            format!("{now} ERROR darwin_core::router: converse failed at src/router.rs:122 error=timed out after 30s"),
            format!("{now} ERROR darwin_core::router: classification failed error=inference classify timed out"),
            format!("{now} ERROR darwin_core::router: cloud completion failed; degrading to local"),
            format!("{now} ERROR darwin_core::router: local generate unavailable in src/inference.rs:88"),
        ]
        .join("\n");
        let scan = scan_tail(tail);
        let d = build_diagnosis(&scan);

        assert_eq!(d.subsystem, "router", "subsystem from the module-path target");
        // The volatile `error=...` tail is trimmed, so the two converse lines
        // collapse to ONE signature.
        assert!(
            d.signatures
                .iter()
                .any(|s| s == "converse failed at src/router.rs:122"),
            "stable signature with the error= tail trimmed: {:?}",
            d.signatures
        );
        // Distinct causes are kept distinct.
        assert!(d.signatures.iter().any(|s| s.contains("classification failed")));
        assert!(d.signatures.iter().any(|s| s.contains("cloud completion failed")));
        // Files cited in the burst, first-seen order, deduped.
        assert_eq!(d.files, vec!["src/router.rs", "src/inference.rs"]);
        // Line numbers cited next to a src/<file>.rs:<line>.
        assert!(d.line_numbers.contains(&122));
        assert!(d.line_numbers.contains(&88));
        // The primary signature feeds heal.diagnosing.
        assert!(!d.primary_signature().is_empty());
    }

    #[test]
    fn diagnosis_subsystem_falls_back_to_unknown() {
        let now = Utc::now().to_rfc3339();
        // Bare `darwin_core` target (no subsystem segment).
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core: transcription failed error=x"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = build_diagnosis(&scan_tail(tail));
        assert_eq!(d.subsystem, "unknown");
        assert_eq!(d.signatures, vec!["transcription failed".to_string()]);
    }

    #[test]
    fn attach_source_excerpts_reads_cited_files_into_the_prompt() {
        // A planted crate whose cited file has KNOWN contents; the drafter
        // prompt must carry those exact lines so the model's diff can apply.
        let root = TempRoot::new("excerpts");
        let crate_dir = root.0.join("daemon");
        write_synthetic_crate(&crate_dir); // src/lib.rs = "pub fn double(x: i32) -> i32 {\n    x * y\n}\n"

        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::router: compile failed in src/lib.rs:2 error=cannot find value `y`"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut d = build_diagnosis(&scan_tail(tail));
        assert_eq!(d.files, vec!["src/lib.rs"]);
        assert!(d.source_excerpts.is_empty(), "build_diagnosis stays IO-free");

        attach_source_excerpts(&mut d, &crate_dir);
        assert_eq!(d.source_excerpts.len(), 1, "the cited file was read");
        assert_eq!(d.source_excerpts[0].0, "src/lib.rs");
        assert!(d.source_excerpts[0].1.contains("x * y"), "real contents present");

        // The prompt now carries the real source so a generated diff can match.
        let prompt = draft_prompt(&d, 3);
        assert!(prompt.contains("current contents"), "prompt advertises the source");
        assert!(prompt.contains("pub fn double(x: i32) -> i32"), "real lines in the prompt");
        assert!(prompt.contains("x * y"));

        // A cited file that does not exist is simply skipped (no panic, no
        // escape outside src/).
        let mut d2 = d.clone();
        d2.source_excerpts.clear();
        d2.files = vec!["src/nonexistent.rs".to_string(), "src/../Cargo.toml".to_string()];
        attach_source_excerpts(&mut d2, &crate_dir);
        assert!(d2.source_excerpts.is_empty(), "missing/escaping paths are skipped");
    }

    #[test]
    fn diagnosis_json_roundtrips() {
        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::audio: audio capture stopped error=device gone"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = build_diagnosis(&scan_tail(tail));
        let json = diagnosis_json(&d);
        assert!(json.contains("\"subsystem\": \"audio\""), "json:\n{json}");
        assert!(json.contains("audio capture stopped"));
        // Valid JSON.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["subsystem"], "audio");
    }

    // -- pure pipeline helpers ------------------------------------------------

    #[test]
    fn source_files_are_extracted_from_log_lines() {
        let text = "thread 'main' panicked at src/router.rs:122: oh no\n\
                    error in daemon/src/heal.rs: bad\n\
                    also src/router.rs:99 again, and src/anthropic.rs too";
        assert_eq!(
            extract_source_files(text),
            vec!["src/router.rs", "src/heal.rs", "src/anthropic.rs"],
            "dedup, first-seen order"
        );
        assert!(extract_source_files("no rust paths here").is_empty());
        // Diff headers parse too (used for the files-touched report field).
        assert_eq!(
            extract_source_files("--- a/src/lib.rs\n+++ b/src/lib.rs"),
            vec!["src/lib.rs"]
        );
    }

    #[test]
    fn staging_dir_name_embeds_ts_and_candidate() {
        assert_eq!(staging_dir_name(1_760_000_000, 0), "staging-1760000000-c0");
        assert_eq!(staging_dir_name(1_760_000_000, 2), "staging-1760000000-c2");
    }

    #[test]
    fn rate_limit_allows_one_attempt_per_six_hours() {
        let now = 1_760_000_000u64;
        assert!(attempt_allowed(None, now), "never attempted -> allowed");
        assert!(attempt_allowed(Some("garbage"), now), "unparseable stamp -> allowed");
        assert!(
            attempt_allowed(Some(&(now - ATTEMPT_INTERVAL_SECS - 1).to_string()), now),
            "older than 6h -> allowed"
        );
        assert!(
            !attempt_allowed(Some(&(now - ATTEMPT_INTERVAL_SECS).to_string()), now),
            "exactly 6h -> still blocked"
        );
        assert!(!attempt_allowed(Some(&(now - 60).to_string()), now), "1min ago -> blocked");
        assert!(
            !attempt_allowed(Some(&(now + 9999).to_string()), now),
            "future stamp (clock skew) must not underflow into allowed"
        );
    }

    /// Gating truth table — UNCHANGED contract: "auto" requires enabled=true,
    /// and any unknown mode degrades only toward the safer Propose.
    #[test]
    fn mode_gating_truth_table() {
        assert_eq!(heal_action(false, "propose"), HealAction::Disabled);
        assert_eq!(heal_action(false, "auto"), HealAction::Disabled);
        assert_eq!(heal_action(false, ""), HealAction::Disabled);
        assert_eq!(heal_action(true, "propose"), HealAction::Propose);
        assert_eq!(heal_action(true, "auto"), HealAction::Auto);
        assert_eq!(heal_action(true, " auto "), HealAction::Auto);
        assert_eq!(heal_action(true, ""), HealAction::Propose);
        assert_eq!(heal_action(true, "AUTO"), HealAction::Propose, "no case games");
        assert_eq!(heal_action(true, "yolo"), HealAction::Propose);
    }

    #[test]
    fn last_lines_takes_the_tail() {
        let text = "a\nb\nc\nd";
        assert_eq!(last_lines(text, 2), "c\nd");
        assert_eq!(last_lines(text, 10), "a\nb\nc\nd");
        assert_eq!(last_lines("", 5), "");
    }

    #[test]
    fn diff_size_counts_added_and_removed_lines() {
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn f() {\n-    a\n+    b\n }\n";
        // one '-' and one '+' content line; headers excluded.
        assert_eq!(diff_size(diff), 2);
    }

    // -- (2) candidate diff cleaning + splitting + rejection (v2) ------------

    #[test]
    fn clean_diff_strips_fences_and_prose_and_rejects_non_diffs() {
        let raw = "Here is the fix you asked for:\n\
                   ```diff\n\
                   --- a/src/lib.rs\n\
                   +++ b/src/lib.rs\n\
                   @@ -1,3 +1,3 @@\n \
                   pub fn double(x: i32) -> i32 {\n\
                   -    x * y\n\
                   +    x * 2\n \
                   }\n\
                   ```";
        let diff = clean_diff(raw).expect("a real diff must survive cleaning");
        assert!(diff.starts_with("--- a/src/lib.rs\n"), "prose/fence must be gone:\n{diff}");
        assert!(diff.ends_with("}\n"), "must keep the final newline:\n{diff:?}");
        assert!(!diff.contains("```"));
        assert!(!diff.contains("Here is"));

        // Prose, refusals, fragments: never reach patch(1).
        assert!(clean_diff("I cannot patch this safely.").is_none());
        assert!(clean_diff("--- a/src/lib.rs\nno hunks here").is_none());
        assert!(clean_diff("").is_none());
    }

    #[test]
    fn clean_diff_rejects_path_traversal_headers() {
        // A `..`-laden target in either header escapes the staging/daemon dir via
        // `patch -p1` (macOS patch honors `..`); such a candidate must be dropped.
        let traverse_plus = "--- a/src/lib.rs\n\
                             +++ b/src/../../../../tmp/escape.txt\n\
                             @@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(traverse_plus).is_none(), "`..` in +++ header must be rejected");

        let traverse_minus = "--- a/src/../../../../tmp/target.txt\n\
                              +++ b/src/lib.rs\n\
                              @@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(traverse_minus).is_none(), "`..` in --- header must be rejected");

        // A `..`-first target (escapes immediately after the `-p1` strip) is rejected.
        let dotdot_first = "--- a/../etc/shadow\n\
                            +++ b/../etc/shadow\n\
                            @@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(dotdot_first).is_none(), "leading `..` after -p1 must be rejected");

        // A legitimate `a/src...`/`b/src...` diff (the only shape heal authors)
        // strips to `src/...` with no `..`/absolute and survives unchanged.
        let ok = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        assert!(clean_diff(ok).is_some(), "a confined a/src diff must survive");

        // New-file creation (`--- /dev/null`) is still allowed when the `+++`
        // target is confined.
        let new_file = "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+y\n";
        assert!(clean_diff(new_file).is_some(), "confined new-file diff must survive");
    }

    #[test]
    fn split_candidate_diffs_parses_labelled_alternatives() {
        let raw = "Here are three options.\n\
                   === CANDIDATE 1 ===\n\
                   --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x * 2\n }\n\
                   === CANDIDATE 2 ===\n\
                   ```diff\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x + x\n }\n```\n\
                   === CANDIDATE 3 ===\n\
                   I could not find a third distinct approach.\n";
        let diffs = split_candidate_diffs(raw);
        assert_eq!(diffs.len(), 2, "two real diffs; the prose candidate is dropped: {diffs:?}");
        assert!(diffs[0].contains("x * 2"));
        assert!(diffs[1].contains("x + x"));
        assert!(diffs.iter().all(|d| !d.contains("CANDIDATE")));
        assert!(diffs.iter().all(|d| !d.contains("```")));
    }

    #[test]
    fn split_candidate_diffs_falls_back_to_single_unlabelled_diff() {
        let raw = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x * 2\n }\n";
        let diffs = split_candidate_diffs(raw);
        assert_eq!(diffs.len(), 1);
        assert!(diffs[0].contains("x * 2"));
    }

    #[test]
    fn split_candidate_diffs_dedups_identical_candidates() {
        let one = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn d() {\n-    x * y\n+    x * 2\n }\n";
        let raw = format!("=== CANDIDATE 1 ===\n{one}=== CANDIDATE 2 ===\n{one}");
        assert_eq!(split_candidate_diffs(&raw).len(), 1, "identical diffs collapse to one");
    }

    #[test]
    fn split_candidate_diffs_rejects_an_all_prose_response() {
        assert!(split_candidate_diffs("I cannot safely patch this; please investigate manually.").is_empty());
    }

    // -- (6) review parsing + (7) survivor selection (v2) --------------------

    #[test]
    fn parse_review_extracts_verdict_and_confidence() {
        let raw = "VERDICT: Fixes the root cause; the undefined binding is replaced, no side effects.\n\
                   CONFIDENCE: 0.88";
        let (verdict, confidence) = parse_review(raw);
        assert!(verdict.contains("root cause"));
        assert!((confidence - 0.88).abs() < 1e-9);

        // Case-insensitive labels, stray text around the number.
        let (_, c2) = parse_review("verdict: ok\nconfidence: about 0.5 maybe");
        assert!((c2 - 0.5).abs() < 1e-9);

        // Garbled confidence -> 0.0 (conservative).
        let (_, c3) = parse_review("VERDICT: unsure\nCONFIDENCE: high");
        assert_eq!(c3, 0.0);

        // Out-of-range clamps to 0..1.
        let (_, c4) = parse_review("VERDICT: ok\nCONFIDENCE: 1.9");
        assert_eq!(c4, 1.0);
    }

    fn survivor(index: usize, confidence: f64, size: usize) -> Survivor {
        Survivor {
            index,
            diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n".to_string(),
            files: vec!["src/lib.rs".to_string()],
            validation_tail: "ok".to_string(),
            review_verdict: "v".to_string(),
            confidence,
            size,
        }
    }

    #[test]
    fn select_winner_prefers_highest_confidence_then_minimal_patch() {
        // Highest confidence wins outright.
        let s = vec![survivor(1, 0.4, 2), survivor(2, 0.9, 10), survivor(3, 0.7, 1)];
        assert_eq!(select_winner(&s), Some(1), "candidate #2 (0.9) wins");

        // Tie on confidence -> the SMALLER patch wins.
        let s = vec![survivor(1, 0.8, 20), survivor(2, 0.8, 3), survivor(3, 0.8, 9)];
        assert_eq!(select_winner(&s), Some(1), "the 3-line patch (#2) wins the tie");

        // No survivors.
        assert_eq!(select_winner(&[]), None);

        // Single survivor.
        assert_eq!(select_winner(&[survivor(1, 0.0, 5)]), Some(0));
    }

    // -- (5) proposal artifact rendering (v2) --------------------------------

    #[test]
    fn report_carries_diagnosis_diff_validation_review_and_apply_command() {
        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::router: classification failed error=x in src/router.rs:42"))
            .collect::<Vec<_>>()
            .join("\n");
        let d = build_diagnosis(&scan_tail(tail));
        let winner = Survivor {
            index: 2,
            diff: "--- a/src/router.rs\n+++ b/src/router.rs\n@@\n-bad\n+good\n".to_string(),
            files: vec!["src/router.rs".to_string()],
            validation_tail: "$ cargo check\n    Finished dev profile\n$ cargo test\ntest result: ok".to_string(),
            review_verdict: "Fixes the root cause; no side effects.".to_string(),
            confidence: 0.91,
            size: 2,
        };
        let report = render_report(1_760_000_000, "claude-opus-4-8", &d, &winner);
        assert!(report.contains("1760000000"));
        assert!(report.contains("claude-opus-4-8"));
        assert!(report.contains("router"), "subsystem in report");
        assert!(report.contains("src/router.rs"));
        assert!(report.contains("classification failed"), "diagnosis signature");
        assert!(report.contains("Finished dev profile"), "validation tail");
        assert!(report.contains("Fixes the root cause"), "review verdict");
        assert!(report.contains("0.91"), "review confidence");
        assert!(report.contains("VALIDATED"));
        assert!(report.contains("chosen candidate: #2"));
        assert!(report.contains("scripts/apply_heal.sh 1760000000"), "exact apply command");
    }

    #[test]
    fn candidates_md_lists_kept_and_discarded() {
        let outcomes = vec![
            CandidateOutcome {
                index: 1,
                diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+x\n".to_string(),
                validated: false,
                detail: "discarded at check:\n```\nerror[E0425]\n```".to_string(),
            },
            CandidateOutcome {
                index: 2,
                diff: "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@\n+y\n".to_string(),
                validated: true,
                detail: "kept — review confidence 0.80".to_string(),
            },
        ];
        let md = render_candidates_md(&outcomes);
        assert!(md.contains("Candidate #1 — DISCARDED"));
        assert!(md.contains("discarded at check"));
        assert!(md.contains("Candidate #2 — VALIDATED"));
        assert!(md.contains("review confidence 0.80"));
    }

    #[test]
    fn review_md_renders_verdict_and_confidence() {
        let winner = survivor(3, 0.77, 4);
        let md = render_review_md(&winner);
        assert!(md.contains("chosen candidate #3"));
        assert!(md.contains("0.77"));
    }

    // -- (4) the trait seam: a MOCK brain drives the full pipeline with NO
    //        cloud, proving multi-candidate -> validate-each -> review ->
    //        select -> propose end to end against a planted-fault temp crate.

    struct MockBrain {
        draft: String,
        reviews: Vec<(String, f64)>,
    }

    impl HealBrain for MockBrain {
        fn draft_candidates<'a>(&'a self, _d: &'a Diagnosis, _n: usize) -> BrainFuture<'a> {
            let draft = self.draft.clone();
            Box::pin(async move { Ok(draft) })
        }
        fn review<'a>(&'a self, _d: &'a Diagnosis, diff: &'a str, _tail: &'a str) -> BrainFuture<'a> {
            // Return a scripted review keyed by which fix the diff carries, so
            // selection is deterministic.
            let mut out = "VERDICT: unknown\nCONFIDENCE: 0.0".to_string();
            for (needle, conf) in &self.reviews {
                if diff.contains(needle.as_str()) {
                    out = format!("VERDICT: mock review for {needle}\nCONFIDENCE: {conf}");
                    break;
                }
            }
            Box::pin(async move { Ok(out) })
        }
    }

    fn write_synthetic_crate(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"synthetic-heal\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src").join("lib.rs"),
            "pub fn double(x: i32) -> i32 {\n    x * y\n}\n",
        )
        .unwrap();
    }

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "darwin-heal-test-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempRoot(dir)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn burst_scan_for_lib() -> LogScan {
        let now = Utc::now().to_rfc3339();
        let tail = (0..5)
            .map(|_| format!("{now} ERROR darwin_core::router: compile failed in src/lib.rs:2 error=cannot find value `y`"))
            .collect::<Vec<_>>()
            .join("\n");
        scan_tail(tail)
    }

    /// THE v2 heart (no cloud): three drafted candidates — one that does not
    /// apply, one that applies but still fails `cargo check`, and one that
    /// truly fixes the planted bug — flow through stage+validate-EACH, the
    /// mock adversarial review, survivor selection, and proposal rendering.
    /// Only the real fix survives, and the source tree is never touched.
    #[tokio::test]
    async fn full_pipeline_via_mock_brain_selects_the_validated_fix() {
        let root = TempRoot::new("mock-e2e");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);
        let ts = 1_760_000_010u64;

        // Candidate 1: wrong context -> rejects at `patch`.
        // Candidate 2: applies but `z` is undefined -> rejects at `check`.
        // Candidate 3: the real fix `x * 2` -> validates.
        let draft = "=== CANDIDATE 1 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn triple(x: i32) -> i32 {\n-    x * q\n+    x * 3\n }\n\
            === CANDIDATE 2 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * z\n }\n\
            === CANDIDATE 3 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * 2\n }\n";
        let brain = MockBrain {
            draft: draft.to_string(),
            reviews: vec![("x * 2".to_string(), 0.93)],
        };

        let result =
            run_attempt(&crate_dir, &heal_root, ts, "mock-model", &brain, &burst_scan_for_lib())
                .await;

        let (diff, report, confidence, extra) = match result {
            AttemptResult::Proposed { diff, report, confidence, extra, .. } => {
                (diff, report, confidence, extra)
            }
            AttemptResult::Rejected { stage, report, .. } => {
                panic!("expected a proposal, rejected at {stage}:\n{report}")
            }
            AttemptResult::Aborted { stage } => panic!("aborted at {stage}"),
        };

        // The winning diff is the real fix; review confidence flowed through.
        assert!(diff.contains("x * 2"), "the validated candidate must win:\n{diff}");
        assert!((confidence - 0.93).abs() < 1e-9);
        assert!(report.contains("scripts/apply_heal.sh 1760000010"));
        // candidates.md records all three with their fates.
        assert!(extra.candidates_md.contains("Candidate #1 — DISCARDED"));
        assert!(extra.candidates_md.contains("Candidate #2 — DISCARDED"));
        assert!(extra.candidates_md.contains("Candidate #3 — VALIDATED"));
        // diagnosis.json is real JSON naming the subsystem.
        assert!(extra.diagnosis_json.contains("\"subsystem\": \"router\""));

        // SAFETY: the source tree was never patched (propose mode).
        assert!(
            std::fs::read_to_string(crate_dir.join("src").join("lib.rs"))
                .unwrap()
                .contains("x * y"),
            "propose mode must never touch the source tree"
        );
        // Each candidate staged into its OWN dir.
        assert!(heal_root.join(staging_dir_name(ts, 2)).join("src").join("lib.rs").exists());
    }

    /// When EVERY candidate fails a gate, the attempt is a rejection (no
    /// proposal, no source touched) and candidates.md still records each fate.
    #[tokio::test]
    async fn full_pipeline_via_mock_brain_rejects_when_no_candidate_validates() {
        let root = TempRoot::new("mock-reject");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);

        // Both candidates apply but leave an undefined binding -> cargo check fails.
        let draft = "=== CANDIDATE 1 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * z\n }\n\
            === CANDIDATE 2 ===\n\
            --- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * w\n }\n";
        let brain = MockBrain { draft: draft.to_string(), reviews: vec![] };

        let result =
            run_attempt(&crate_dir, &heal_root, 1_760_000_011, "mock-model", &brain, &burst_scan_for_lib())
                .await;

        match result {
            AttemptResult::Rejected { stage, .. } => assert_eq!(stage, "check"),
            other => panic!("expected rejection at cargo check, got {:?}", std::mem::discriminant(&other)),
        }
        assert!(
            std::fs::read_to_string(crate_dir.join("src").join("lib.rs"))
                .unwrap()
                .contains("x * y"),
            "a fully-rejected attempt must never touch the source tree"
        );
    }

    /// A non-applying diff rejects at the `patch` stage before any cargo run.
    #[tokio::test]
    async fn staging_pipeline_rejects_a_non_applying_diff() {
        let root = TempRoot::new("badhunk");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);

        let wrong_context_diff = "--- a/src/lib.rs\n\
                                  +++ b/src/lib.rs\n\
                                  @@ -1,3 +1,3 @@\n \
                                  pub fn triple(x: i32) -> i32 {\n\
                                  -    x * q\n\
                                  +    x * 3\n \
                                  }\n";
        let result = stage_and_validate(&crate_dir, &heal_root, 1_760_000_002, 0, wrong_context_diff)
            .await
            .unwrap();
        match result {
            StageResult::Rejected { stage, .. } => assert_eq!(stage, "patch"),
            StageResult::Validated { .. } => panic!("a failed hunk must reject"),
        }
    }

    /// A diff that applies but does NOT fix the planted compile bug must be
    /// rejected by the real `cargo check` in staging (the gate is unchanged).
    #[tokio::test]
    async fn staging_pipeline_rejects_when_check_still_fails() {
        let root = TempRoot::new("stillbroken");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);

        let useless_diff = "--- a/src/lib.rs\n\
                            +++ b/src/lib.rs\n\
                            @@ -1,3 +1,3 @@\n \
                            pub fn double(x: i32) -> i32 {\n\
                            -    x * y\n\
                            +    x * z\n \
                            }\n";
        let result = stage_and_validate(&crate_dir, &heal_root, 1_760_000_003, 0, useless_diff)
            .await
            .unwrap();
        match result {
            StageResult::Rejected { stage, detail } => {
                assert_eq!(stage, "check");
                assert!(detail.contains("cargo check"), "captured output missing:\n{detail}");
            }
            StageResult::Validated { .. } => panic!("cargo check must catch the surviving bug"),
        }
    }

    /// The staging path still validates a genuine fix end to end (real patch,
    /// real cargo, tempdir only) — the v1 guarantee, preserved.
    #[tokio::test]
    async fn staging_pipeline_validates_a_planted_fix() {
        let root = TempRoot::new("e2e");
        let crate_dir = root.0.join("daemon");
        let heal_root = root.0.join("state").join("heal");
        write_synthetic_crate(&crate_dir);
        let fixing = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n pub fn double(x: i32) -> i32 {\n-    x * y\n+    x * 2\n }\n";
        let result = stage_and_validate(&crate_dir, &heal_root, 1_760_000_001, 0, fixing)
            .await
            .expect("staging infrastructure must work");
        match result {
            StageResult::Validated { validation_tail } => {
                assert!(validation_tail.contains("cargo"));
            }
            StageResult::Rejected { stage, detail } => panic!("expected validation, rejected at {stage}:\n{detail}"),
        }
        // staged copy patched; live source NOT.
        let staged = heal_root.join(staging_dir_name(1_760_000_001, 0)).join("src").join("lib.rs");
        assert!(std::fs::read_to_string(&staged).unwrap().contains("x * 2"));
        assert!(std::fs::read_to_string(crate_dir.join("src").join("lib.rs")).unwrap().contains("x * y"));
    }

    /// (6 of contract) THE HEAL DRILL via the REAL cloud. #[ignore] by default
    /// — the ONLY cloud path in this module, run explicitly by the verifier:
    ///   cargo test --release heal_drill_real_cloud -- --ignored --nocapture
    /// (or `darwind --heal-drill`). It heals a planted fault in a TEMP crate,
    /// proving diagnose -> Opus draft -> stage -> validate -> review -> propose
    /// end to end. Skips gracefully (passes) when no API key is present so an
    /// offline `--ignored` run does not spuriously fail.
    #[tokio::test]
    #[ignore = "real cloud spend; run by the verifier with --ignored"]
    async fn heal_drill_real_cloud() {
        if anthropic::resolve_api_key().await.is_none() {
            eprintln!("heal_drill_real_cloud: no API key resolved; skipping (run with the key set)");
            return;
        }
        let model = "claude-opus-4-8";
        let dir = run_heal_drill(model).await.expect("heal drill must produce a proposal");
        assert!(dir.join("patch.diff").exists(), "drill must write patch.diff");
        assert!(dir.join("report.md").exists());
        assert!(dir.join("diagnosis.json").exists());
        assert!(dir.join("candidates.md").exists());
        assert!(dir.join("review.md").exists());
        let report = std::fs::read_to_string(dir.join("report.md")).unwrap();
        assert!(report.contains("VALIDATED"), "drill proposal must be validated:\n{report}");
        // Clean up the throwaway sandbox (it lives under tmp/darwin-heal-drill-*).
        if let Some(sandbox) = dir.ancestors().find(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("darwin-heal-drill-"))
        }) {
            let _ = std::fs::remove_dir_all(sandbox);
        }
    }
}
