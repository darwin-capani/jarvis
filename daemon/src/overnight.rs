//! F10 — OVERNIGHT ASYNC AGENTS + MORNING FOLD-IN.
//!
//! Queue low-priority "look into this" tasks; while you are AWAY, run them
//! through the cloud model and fold the results into a morning brief waiting for
//! you when you return.
//!
//! THREE HARD RULES (each pinned by a test):
//!   1. SHIPS OFF. `[overnight].enabled` defaults false — autonomous unattended
//!      work is opt-in like `[distill]`/`[sync]`/`[scene]`.
//!   2. NEVER ACTS UNATTENDED. Overnight tasks are TOOL-LESS completions: they
//!      research, summarize, and draft, but are STRUCTURALLY incapable of any
//!      consequential action — no tools are ever passed to the model, so it
//!      cannot send, buy, delete, or change anything. Whatever would need an
//!      action is drafted and left in the brief for your spoken confirmation
//!      when you wake — never executed while you sleep. `runs_tools` is a
//!      pinned-false wire field.
//!   3. RUNS ONLY WHEN AWAY. The scheduler fires at most once per away-window,
//!      gated on presence == Away (main.rs); a present/at-keyboard user is
//!      never interrupted, and a run never repeats until the window resets.
//!
//! Cloud-gated: the real run needs an Anthropic key (capability Probed). The
//! queue + away-gate + brief machinery is PURE and hermetically tested; the
//! cloud completion is injected in tests (a canned runner), never hit.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Cap on the queued task backlog — a to-do list, not a spool. Enqueuing past
/// the cap drops the oldest already-finished task, else refuses.
const MAX_QUEUE: usize = 20;
/// Defensive bounds (chars) on stored/surfaced text.
const PROMPT_MAX: usize = 2000;
const RESULT_MAX: usize = 4000;
/// Brief preview cap — a glance, not a transcript.
const BRIEF_ITEMS: usize = 8;
/// Generous-but-bounded budget for one overnight completion.
const RUN_MAX_TOKENS: u32 = 2048;
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// The tool-less system prompt: think, draft, defer — never claim to have acted.
const OVERNIGHT_SYSTEM: &str = "You are an overnight assistant running while the user is away. You have NO tools and cannot take any action — you can only research, reason, summarize, and draft. If the task would require sending, buying, changing, or deleting anything, draft it and clearly note that it awaits the user's confirmation. Be concise.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Done,
    Failed,
}

impl TaskStatus {
    pub fn wire(self) -> &'static str {
        match self {
            TaskStatus::Queued => "queued",
            TaskStatus::Done => "done",
            TaskStatus::Failed => "failed",
        }
    }
}

/// One overnight task. `result` is the redacted, bounded completion (or error);
/// it never contains raw audio/tools — overnight work is tool-less by design.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OvernightTask {
    pub id: String,
    pub prompt: String,
    pub agent: String,
    pub enqueued: String,
    pub status: TaskStatus,
    pub result: String,
}

/// A folded morning brief. PURE derivation from the queue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MorningBrief {
    pub done: usize,
    pub failed: usize,
    pub pending: usize,
    pub items: Vec<BriefItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BriefItem {
    pub prompt: String,
    pub result: String,
    pub status: &'static str,
}

fn bound(s: &str, max: usize) -> String {
    let red = crate::optimize::redact(s);
    red.chars().take(max).collect()
}

fn queue_root(root: &std::path::Path) -> std::path::PathBuf {
    root.join("state").join("overnight")
}

fn queue_path(root: &std::path::Path) -> std::path::PathBuf {
    queue_root(root).join("queue.json")
}

fn last_run_path(root: &std::path::Path) -> std::path::PathBuf {
    queue_root(root).join("last_run.txt")
}

/// Load the persisted queue, or an empty queue if absent/unreadable (fail-open —
/// a missing queue is honestly "nothing scheduled", never an error).
pub fn load_queue(root: &std::path::Path) -> Vec<OvernightTask> {
    std::fs::read(queue_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Persist the queue. Returns whether the write actually landed — the enqueue
/// ack is a durable-sounding promise ("queued for tonight"), so a failed write
/// must surface as an honest refusal, never a silent best-effort.
fn save_queue(root: &std::path::Path, tasks: &[OvernightTask]) -> bool {
    let Ok(bytes) = serde_json::to_vec_pretty(tasks) else { return false };
    let _ = std::fs::create_dir_all(queue_root(root));
    std::fs::write(queue_path(root), bytes).is_ok()
}

fn read_last_run(root: &std::path::Path) -> Option<DateTime<Utc>> {
    let s = std::fs::read_to_string(last_run_path(root)).ok()?;
    DateTime::parse_from_rfc3339(s.trim()).ok().map(|d| d.with_timezone(&Utc))
}

fn write_last_run(root: &std::path::Path, now_rfc3339: &str) {
    let _ = std::fs::create_dir_all(queue_root(root));
    let _ = std::fs::write(last_run_path(root), now_rfc3339.as_bytes());
}

/// Deterministic short id from the prompt + enqueue time (no clock/rng in the
/// derivation itself — the caller supplies the timestamp).
fn derive_id(prompt: &str, enqueued: &str) -> String {
    let mut h = DefaultHasher::new();
    prompt.hash(&mut h);
    enqueued.hash(&mut h);
    format!("ov-{:016x}", h.finish())
}

/// PURE queue mutation: append a new Queued task, bounding the prompt. If the
/// queue is at the cap, evict the oldest FINISHED task; if none is finished,
/// the append is refused (returns None) so a backlog can't grow unbounded.
pub fn plan_enqueue(
    mut tasks: Vec<OvernightTask>,
    prompt: &str,
    agent: &str,
    enqueued: &str,
) -> Option<Vec<OvernightTask>> {
    if tasks.len() >= MAX_QUEUE {
        if let Some(pos) = tasks.iter().position(|t| t.status != TaskStatus::Queued) {
            tasks.remove(pos);
        } else {
            return None; // all queued, at cap -> refuse rather than grow
        }
    }
    tasks.push(OvernightTask {
        id: derive_id(prompt, enqueued),
        prompt: bound(prompt, PROMPT_MAX),
        agent: bound(agent, 64),
        enqueued: enqueued.to_string(),
        status: TaskStatus::Queued,
        result: String::new(),
    });
    Some(tasks)
}

/// Enqueue a task to disk. Returns a friendly confirmation, or an honest refusal
/// when the subsystem is off or the backlog is full. Trigger-side (the
/// `overnight` command verb). `enabled` is the LIVE [overnight].enabled — off
/// must REFUSE like distill/sync do, never fabricate "queued for tonight" for
/// work run_pending (gated on the same switch) will never run.
pub fn enqueue(root: &std::path::Path, enabled: bool, prompt: &str, agent: &str, now_rfc3339: &str) -> String {
    if !enabled {
        return "Overnight agents are off, sir — turn on [overnight].enabled and I'll run queued work while you're away.".to_string();
    }
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return "There's nothing to queue, sir — tell me what to look into overnight.".to_string();
    }
    let tasks = load_queue(root);
    match plan_enqueue(tasks, prompt, agent, now_rfc3339) {
        Some(updated) => {
            let n = updated.iter().filter(|t| t.status == TaskStatus::Queued).count();
            if !save_queue(root, &updated) {
                // The ack is a durable promise; a failed persist must never
                // fabricate one (the runner reloads from the unwritten file).
                return "I couldn't save the overnight queue, sir — the task is NOT queued. Check the state directory and try again.".to_string();
            }
            format!("Queued for tonight, sir — {n} task{} waiting for the next time you're away.", if n == 1 { "" } else { "s" })
        }
        None => "The overnight queue is full, sir — I'll run the pending ones first.".to_string(),
    }
}

/// PURE away-gate: run only when the user is AWAY and either we've never run or
/// the last run is older than `min_gap_secs` (once per away-window). Present or
/// recently-active users are never disturbed.
pub fn should_run_now(
    presence: crate::presence::Presence,
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_gap_secs: i64,
) -> bool {
    if presence != crate::presence::Presence::Away {
        return false;
    }
    match last_run {
        None => true,
        Some(prev) => (now - prev).num_seconds() >= min_gap_secs,
    }
}

/// The real, cloud-gated overnight runner: a TOOL-LESS completion. No tools are
/// ever passed, so the model cannot take any action — it can only draft. Needs
/// an Anthropic key (inert without one). Never hit in tests.
pub async fn run_real_task(cfg: &crate::config::Config, prompt: &str) -> Result<String, String> {
    crate::anthropic::complete_plain(&cfg.cloud.heavy_model, RUN_MAX_TOKENS, OVERNIGHT_SYSTEM, prompt, RUN_TIMEOUT)
        .await
        .map_err(|e| e.to_string())
}

/// Run every Queued task through the injected runner, recording a redacted,
/// bounded result. Gated: off => no-op. Generic over the runner so the machinery
/// is hermetically testable; production injects [`run_real_task`], tests inject a
/// canned closure (the cloud is never called in a test). Records the run time so
/// the away-gate won't refire until the window resets. Returns the count run.
pub async fn run_pending<F, Fut>(
    cfg: &crate::config::Config,
    root: &std::path::Path,
    now_rfc3339: &str,
    run: F,
) -> usize
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    if !cfg.overnight.enabled {
        return 0;
    }
    let mut tasks = load_queue(root);
    let mut ran = 0;
    for task in tasks.iter_mut() {
        if task.status != TaskStatus::Queued {
            continue;
        }
        match run(task.prompt.clone()).await {
            Ok(text) => {
                task.result = bound(&text, RESULT_MAX);
                task.status = TaskStatus::Done;
            }
            Err(e) => {
                task.result = bound(&e, 200);
                task.status = TaskStatus::Failed;
            }
        }
        ran += 1;
    }
    if ran > 0 {
        // Best-effort here is the SAFE direction (unlike enqueue's ack): a
        // failed save loses done-markers, so tasks re-run next window — wasted
        // tool-less work, never a false promise to the user.
        let _ = save_queue(root, &tasks);
    }
    // Stamp the run even at zero tasks so the away-gate honours min_gap and
    // doesn't spin every tick on an empty queue.
    write_last_run(root, now_rfc3339);
    ran
}

/// PURE fold of the queue into a morning brief: counts by status + a bounded,
/// newest-first preview of finished work (done + failed).
pub fn plan_brief(tasks: &[OvernightTask]) -> MorningBrief {
    let done = tasks.iter().filter(|t| t.status == TaskStatus::Done).count();
    let failed = tasks.iter().filter(|t| t.status == TaskStatus::Failed).count();
    let pending = tasks.iter().filter(|t| t.status == TaskStatus::Queued).count();
    let items: Vec<BriefItem> = tasks
        .iter()
        .rev()
        .filter(|t| t.status != TaskStatus::Queued)
        .take(BRIEF_ITEMS)
        .map(|t| BriefItem {
            prompt: bound(&t.prompt, 160),
            result: bound(&t.result, 400),
            status: t.status.wire(),
        })
        .collect();
    MorningBrief { done, failed, pending, items }
}

/// The `overnight.status` wire payload. PURE + total. `runs_tools` is pinned
/// FALSE — overnight work is tool-less and can never act. Results are the user's
/// own drafted content, redacted + bounded, shown only on their own HUD.
pub fn status_payload(enabled: bool, key_present: bool, brief: &MorningBrief) -> Value {
    json!({
        "enabled": enabled,
        "cloud_key_present": key_present,
        "dep_verified": key_present,
        "dependency": "an Anthropic API key (in the Keychain)",
        // PINNED honest: overnight agents get NO tools, so they can never act.
        "runs_tools": false,
        "queued": brief.pending,
        "done": brief.done,
        "failed": brief.failed,
        "items": brief.items.iter().map(|i| json!({
            "prompt": i.prompt,
            "result": i.result,
            "status": i.status,
        })).collect::<Vec<_>>(),
    })
}

/// Emit `overnight.status` for the HUD on the audit-snapshot cadence. READ-ONLY:
/// loads the queue, folds a brief, probes the key; runs nothing. Fail-open.
pub async fn emit_status(cfg: &crate::config::Config, root: &std::path::Path) {
    let brief = plan_brief(&load_queue(root));
    let key_present = crate::anthropic::resolve_api_key().await.is_some();
    crate::telemetry::emit("system", "overnight.status", status_payload(cfg.overnight.enabled, key_present, &brief));
}

/// Convenience for the scheduler: the last time an overnight run stamped, for
/// the away-gate. Public so main.rs's overnight task can consult it.
pub fn last_run(root: &std::path::Path) -> Option<DateTime<Utc>> {
    read_last_run(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::Presence;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("jarvis-overnight-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn task(id: &str, status: TaskStatus) -> OvernightTask {
        OvernightTask { id: id.into(), prompt: "p".into(), agent: "jarvis".into(), enqueued: "2026-07-13T20:00:00Z".into(), status, result: String::new() }
    }

    #[test]
    fn away_gate_runs_only_when_away_and_respects_the_window() {
        let now = ts("2026-07-13T03:00:00Z");
        // Present / focused users are never disturbed.
        assert!(!should_run_now(Presence::Present, None, now, 3600));
        assert!(!should_run_now(Presence::Focused, None, now, 3600));
        // Away + never run -> go.
        assert!(should_run_now(Presence::Away, None, now, 3600));
        // Away but ran 10 min ago with a 1h window -> wait.
        assert!(!should_run_now(Presence::Away, Some(ts("2026-07-13T02:50:00Z")), now, 3600));
        // Away and the window has elapsed -> go again.
        assert!(should_run_now(Presence::Away, Some(ts("2026-07-13T01:00:00Z")), now, 3600));
    }

    #[test]
    fn enqueue_bounds_the_prompt_and_caps_the_backlog() {
        // A prompt longer than the cap is stored truncated.
        let long = "x".repeat(PROMPT_MAX + 500);
        let tasks = plan_enqueue(vec![], &long, "jarvis", "2026-07-13T20:00:00Z").unwrap();
        assert!(tasks[0].prompt.chars().count() <= PROMPT_MAX);

        // A full backlog of all-Queued tasks refuses further enqueue.
        let full: Vec<OvernightTask> = (0..MAX_QUEUE).map(|i| task(&format!("q{i}"), TaskStatus::Queued)).collect();
        assert!(plan_enqueue(full, "another", "jarvis", "2026-07-13T20:00:00Z").is_none());

        // But a finished task is evicted to make room.
        let mut mixed: Vec<OvernightTask> = (0..MAX_QUEUE - 1).map(|i| task(&format!("q{i}"), TaskStatus::Queued)).collect();
        mixed.insert(0, task("old-done", TaskStatus::Done));
        let after = plan_enqueue(mixed, "fresh", "jarvis", "2026-07-13T20:00:00Z").unwrap();
        assert_eq!(after.len(), MAX_QUEUE);
        assert!(!after.iter().any(|t| t.id == "old-done"), "the finished task was evicted");
    }

    #[test]
    fn enqueue_refuses_honestly_when_off_and_writes_nothing() {
        // Regression for the post-merge audit finding: the verb used to reply
        // "Queued for tonight, sir" while [overnight].enabled=false, though the
        // gated runner would never execute the task. Off must REFUSE (like
        // distill/sync) and persist nothing.
        let dir = tempdir("offq");
        let reply = enqueue(&dir, false, "look into X", "jarvis", "2026-07-13T20:00:00Z");
        assert!(reply.contains("off"), "honest refusal, not a fabricated ack: {reply}");
        assert!(load_queue(&dir).is_empty(), "no task persisted while off");
        // On, the same call queues.
        let reply = enqueue(&dir, true, "look into X", "jarvis", "2026-07-13T20:00:00Z");
        assert!(reply.contains("Queued"), "{reply}");
        assert_eq!(load_queue(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enqueue_refuses_honestly_when_the_queue_cannot_be_persisted() {
        // Regression (CodeRabbit sweep): a failed save used to still reply
        // "Queued for tonight" — a fabricated durable promise the runner would
        // never see. A directory squatting on the queue path makes the write
        // fail deterministically.
        let dir = tempdir("nosave");
        std::fs::create_dir_all(queue_path(&dir)).unwrap(); // queue.json as a DIR
        let reply = enqueue(&dir, true, "look into X", "jarvis", "2026-07-13T20:00:00Z");
        assert!(reply.contains("NOT queued"), "honest refusal on failed persist: {reply}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_pending_is_off_by_default_and_never_calls_the_runner() {
        let dir = tempdir("off");
        save_queue(&dir, &[task("a", TaskStatus::Queued)]);
        let cfg = crate::config::Config::default();
        let ran = run_pending(&cfg, &dir, "2026-07-13T03:00:00Z", |_p| async { panic!("must NOT run when off") }).await;
        assert_eq!(ran, 0);
        assert_eq!(load_queue(&dir)[0].status, TaskStatus::Queued, "untouched while off");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn run_pending_runs_each_queued_task_via_the_canned_runner_and_records_results() {
        let dir = tempdir("run");
        save_queue(&dir, &[task("a", TaskStatus::Queued), task("b", TaskStatus::Queued), task("c", TaskStatus::Done)]);
        let mut cfg = crate::config::Config::default();
        cfg.overnight.enabled = true;
        // Canned runner: succeeds for everything, no cloud call.
        let ran = run_pending(&cfg, &dir, "2026-07-13T03:00:00Z", |p| async move { Ok(format!("drafted: {p}")) }).await;
        assert_eq!(ran, 2, "only the two Queued tasks ran; the Done one was skipped");
        let q = load_queue(&dir);
        assert!(q.iter().filter(|t| t.status == TaskStatus::Done).count() == 3);
        assert!(q.iter().any(|t| t.result.contains("drafted: p")));
        // The run time was stamped so the away-gate won't immediately refire.
        assert!(last_run(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_failing_task_is_recorded_failed_not_lost() {
        let dir = tempdir("fail");
        save_queue(&dir, &[task("a", TaskStatus::Queued)]);
        let mut cfg = crate::config::Config::default();
        cfg.overnight.enabled = true;
        run_pending(&cfg, &dir, "2026-07-13T03:00:00Z", |_p| async { Err("cloud unreachable".to_string()) }).await;
        let q = load_queue(&dir);
        assert_eq!(q[0].status, TaskStatus::Failed);
        assert!(q[0].result.contains("unreachable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn brief_folds_counts_and_previews_finished_work_newest_first() {
        let tasks = vec![
            OvernightTask { id: "a".into(), prompt: "look into A".into(), agent: "jarvis".into(), enqueued: "t".into(), status: TaskStatus::Done, result: "found A".into() },
            task("q", TaskStatus::Queued),
            OvernightTask { id: "b".into(), prompt: "look into B".into(), agent: "jarvis".into(), enqueued: "t".into(), status: TaskStatus::Failed, result: "err".into() },
        ];
        let brief = plan_brief(&tasks);
        assert_eq!(brief.done, 1);
        assert_eq!(brief.failed, 1);
        assert_eq!(brief.pending, 1);
        assert_eq!(brief.items.len(), 2, "only finished work is previewed");
        assert_eq!(brief.items[0].status, "failed", "newest-first: B (last) leads");
        assert_eq!(brief.items[1].prompt, "look into A");
    }

    #[test]
    fn status_pins_tool_less_and_reports_honest_off_state() {
        let empty = plan_brief(&[]);
        let p = status_payload(false, false, &empty);
        assert_eq!(p["enabled"], false);
        assert_eq!(p["runs_tools"], false, "PINNED: overnight is tool-less, can never act");
        assert_eq!(p["dep_verified"], false, "no key -> unverified");
        assert_eq!(p["queued"], 0);
        // Enabled + key present -> verified true, but still tool-less.
        let p2 = status_payload(true, true, &empty);
        assert_eq!(p2["dep_verified"], true);
        assert_eq!(p2["runs_tools"], false);
    }

    #[test]
    fn stored_text_is_redacted() {
        // A prompt carrying an email is redacted before it is stored.
        let tasks = plan_enqueue(vec![], "email bob@example.com about lunch", "jarvis", "2026-07-13T20:00:00Z").unwrap();
        assert!(!tasks[0].prompt.contains("bob@example.com"), "PII redacted at store time");
    }
}
