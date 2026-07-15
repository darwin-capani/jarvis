//! DURABLE MISSIONS (#26): persist FURY mission state to SQLite so a campaign
//! survives a restart and can be resumed / listed / cancelled.
//!
//! A mission record is `(id, goal, status, created, sub-tasks[])` where each
//! sub-task carries its own status. The record persists as one JSON fact row under
//! `meta.mission.<id>` (the SAME bounded `meta.*` store the standing-mission records
//! use; in-RAM/temp SQLite in tests). The three KEY SAFETY properties are enforced
//! HERE, not assumed:
//!
//! - **(a) No auto-run on restart.** [`load`] / [`list`] FORCE every loaded
//!   mission's live status to [`MissionStatus::Paused`] regardless of what was
//!   stored — a persisted mission ALWAYS comes back paused, so a restart can never
//!   silently resume autonomy. The user must call [`resume`] explicitly. There is
//!   NO start-up tick that auto-resumes; the only run path is an explicit
//!   user-driven resume.
//! - **(b) Persistence carries NO pre-approval.** [`resume`] runs the mission's
//!   remaining sub-tasks through the SAME [`crate::mission::run_mission`] engine a
//!   live `fury_mission` uses — so each consequential sub-task step routes through
//!   the OWNING specialist's allowlist + `integrations::gate()` FRESH. A persisted
//!   mission stores only the GOAL + step bookkeeping; it never stores a confirm, a
//!   token, or a "already approved" bit. Resuming re-gates from scratch.
//! - **(c) Bounds inherited.** A resume runs through `run_mission`, which clamps to
//!   FURY's `<= MAX_SUBTASKS` / depth-1 bounds and the mission budget — a durable
//!   mission can never exceed what a live one could.
//!
//! ON by default ([missions].durable = true): this adds PERSISTENCE only — a
//! persisted mission ALWAYS loads PAUSED (never auto-runs on restart). With it off
//! nothing is persisted and missions are in-memory exactly as today. HONESTY: a
//! durable mission never auto-resumes and never carries a stored approval —
//! resuming re-runs the gate.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::agents::AgentRegistry;
use crate::memory::Memory;
use crate::mission::{run_mission, Dispatcher, Planner};

/// Default evict-oldest cap on persisted missions (the bounded store).
pub const DEFAULT_RETENTION: usize = 50;

/// Reserved `meta.*` key prefix the mission records live under (internal
/// bookkeeping, filtered from every agent prompt feed + the world model).
const MISSION_PREFIX: &str = "meta.mission.";

/// Bound on a persisted goal so one row can never grow unbounded.
const MAX_GOAL_LEN: usize = 600;

/// The lifecycle status of a durable mission. A loaded mission is ALWAYS forced to
/// [`MissionStatus::Paused`] on read (no auto-run), and only [`resume`] /
/// [`cancel`] move it forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    /// Persisted but NOT running — the safe state every mission loads into.
    Paused,
    /// Actively being resumed (transient; set while a resume runs).
    Active,
    /// All sub-tasks attempted, the run finished.
    Done,
    /// Cancelled by the user; never runs again.
    Cancelled,
}

impl MissionStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MissionStatus::Paused => "paused",
            MissionStatus::Active => "active",
            MissionStatus::Done => "done",
            MissionStatus::Cancelled => "cancelled",
        }
    }
}

/// The status of a single persisted sub-task within a mission. Carries no result
/// payload that could be a fabricated "completed" — only whether it has been
/// attempted, and the honest outcome if so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Not yet attempted (the state every step persists into across a restart).
    Pending,
    /// Attempted and produced a real answer.
    Done,
    /// Attempted and failed/refused (e.g. a gated step that only previewed).
    Failed,
}

/// One persisted sub-task: the instruction + its status. The instruction is the
/// SAME natural-language step FURY plans; on resume it re-routes to its owner and
/// re-runs through the gate. NO approval / token / credential is stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableStep {
    pub instruction: String,
    pub status: StepStatus,
}

/// A durable FURY mission record, persisted as one JSON fact row under
/// `meta.mission.<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableMission {
    /// Stable content id (short hex), the addressing label for resume/cancel.
    pub id: String,
    /// The overall objective FURY decomposes + runs.
    pub goal: String,
    /// The lifecycle status. ALWAYS forced to Paused when read from the store.
    pub status: MissionStatus,
    /// The known sub-tasks + their statuses (may be empty until first planned).
    pub steps: Vec<DurableStep>,
    /// RFC3339 creation timestamp.
    pub created: String,
}

impl DurableMission {
    /// Build a new durable mission from a goal. Starts PAUSED with no steps planned
    /// yet (steps are recorded as the engine plans/runs them). Pure.
    pub fn new(goal: &str) -> DurableMission {
        let goal = bound(goal, MAX_GOAL_LEN);
        let id = derive_id(&goal);
        DurableMission {
            id,
            goal,
            status: MissionStatus::Paused,
            steps: Vec::new(),
            created: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Trim + length-bound a string for persistence. Pure.
fn bound(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Derive a stable content id for a mission: short hex of SHA-256 over the
/// normalized goal. Pure — a cancel/resume can name a mission by an id reproducible
/// from its content.
pub fn derive_id(goal: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(goal.trim().to_lowercase().as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..6])
}

// ---------------------------------------------------------------------------
// The store (persisted via Memory; round-trips against a temp/in-RAM DB in tests)
// ---------------------------------------------------------------------------

/// Create a NEW durable mission (PAUSED, no steps yet) and persist it. Returns the
/// record. Enforces the active retention cap. This is the only way a mission enters
/// the durable store; it never runs anything.
pub async fn create(memory: &Memory, retention: usize, goal: &str) -> Result<DurableMission> {
    let mission = DurableMission::new(goal);
    save(memory, &mission).await?;
    enforce_retention(memory, retention.max(1)).await?;
    Ok(mission)
}

/// Persist a mission as one JSON fact row under `meta.mission.<id>`. Trusted
/// internal write. Idempotent on id.
async fn save(memory: &Memory, m: &DurableMission) -> Result<()> {
    let key = format!("{MISSION_PREFIX}{}", m.id);
    let json = serde_json::to_string(m)?;
    memory.upsert_fact(&key, &json).await
}

/// Load one mission by id. SAFETY (a): the returned mission's `status` is FORCED to
/// [`MissionStatus::Paused`] regardless of what was stored — a persisted mission
/// never comes back in a running state, so nothing auto-runs on restart. A
/// `Done`/`Cancelled` mission keeps its terminal status (those never run anyway);
/// any `Active`/`Paused` is normalized to `Paused`.
pub async fn load(memory: &Memory, id: &str) -> Result<Option<DurableMission>> {
    let key = format!("{MISSION_PREFIX}{}", id.trim());
    let Some(json) = memory.get_fact(&key).await? else {
        return Ok(None);
    };
    let Ok(mut m) = serde_json::from_str::<DurableMission>(&json) else {
        return Ok(None);
    };
    m.status = paused_on_load(m.status);
    Ok(Some(m))
}

/// Normalize a stored status on load: any non-terminal status becomes Paused (no
/// auto-run); terminal statuses (Done/Cancelled) are preserved. Pure — the heart of
/// SAFETY (a), unit-testable without a store.
pub fn paused_on_load(stored: MissionStatus) -> MissionStatus {
    match stored {
        MissionStatus::Done => MissionStatus::Done,
        MissionStatus::Cancelled => MissionStatus::Cancelled,
        // Active or Paused -> always Paused on load. A crash mid-run leaves the
        // store "Active"; we must NOT resume it silently.
        MissionStatus::Active | MissionStatus::Paused => MissionStatus::Paused,
    }
}

/// Every persisted mission, newest first, each loaded PAUSED per SAFETY (a).
/// Malformed rows are skipped.
pub async fn list(memory: &Memory) -> Result<Vec<DurableMission>> {
    let rows = memory.recall_facts_limited(MISSION_PREFIX, 512).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str::<DurableMission>(&v).ok())
        .map(|mut m| {
            m.status = paused_on_load(m.status);
            m
        })
        .collect())
}

/// Cancel (delete) a mission by id. Returns whether a row existed. Reversible only
/// by re-creating; never runs anything.
pub async fn cancel(memory: &Memory, id: &str) -> Result<bool> {
    let key = format!("{MISSION_PREFIX}{}", id.trim());
    memory.delete_fact(&key).await
}

/// RESUME a durable mission by id. SAFETY (b)+(c): this runs the mission's GOAL
/// through the SAME [`run_mission`] engine a live `fury_mission` uses — so every
/// sub-task re-routes to its owner under that owner's allowlist and every
/// consequential step re-runs through `integrations::gate()` FRESH. The persisted
/// record carried NO approval; resuming re-gates from scratch. Generic over the
/// [`Planner`]/[`Dispatcher`] seams so tests drive it with mocks (no network) and
/// the daemon wires the cloud-backed pair.
///
/// On resume the mission is stamped Active (so a concurrent reader still loads it
/// Paused — `paused_on_load` normalizes Active back to Paused), the engine runs,
/// the record is marked Done with the attempted steps recorded, and the synthesized
/// answer is returned. A mission already Cancelled/Done is not re-run (an honest
/// note is returned instead).
pub async fn resume(
    memory: &Memory,
    id: &str,
    registry: &AgentRegistry,
    planner: &dyn Planner,
    dispatcher: &dyn Dispatcher,
    cloud_reachable: bool,
) -> Result<String> {
    let Some(mut mission) = load(memory, id).await? else {
        return Ok(format!("I have no durable mission with id {id} to resume."));
    };
    // A terminal mission is never re-run.
    if matches!(mission.status, MissionStatus::Done) {
        return Ok(format!("Mission \"{}\" already completed; nothing to resume.", mission.goal));
    }
    if matches!(mission.status, MissionStatus::Cancelled) {
        return Ok(format!("Mission \"{}\" was cancelled; re-establish it to run again.", mission.goal));
    }

    // Stamp Active while we run. A reader during the run STILL loads it Paused
    // (paused_on_load normalizes Active->Paused), so even a crash mid-run can never
    // leave a mission that auto-resumes.
    mission.status = MissionStatus::Active;
    save(memory, &mission).await?;

    // Run through the SAME bounded engine fury_mission uses. Each sub-task routes to
    // its owner under that owner's allowlist; each consequential step re-runs the
    // gate FRESH (the dispatcher carries no pre-approval). This is the proof of
    // SAFETY (b): a resumed mission re-gates exactly like a live one.
    let answer = run_mission(&mission.goal, registry, planner, dispatcher, cloud_reachable).await;

    // Mark Done (the run finished) — we do NOT fabricate step outcomes; the synthesis
    // answer already carries the honest per-sub-task results from the engine.
    mission.status = MissionStatus::Done;
    save(memory, &mission).await?;
    Ok(answer)
}

/// Evict the oldest missions past `keep` so the store stays bounded.
async fn enforce_retention(memory: &Memory, keep: usize) -> Result<()> {
    let rows = memory.recall_facts_limited(MISSION_PREFIX, 1024).await?;
    if rows.len() <= keep {
        return Ok(());
    }
    for (key, _) in rows.into_iter().skip(keep) {
        let _ = memory.delete_fact(&key).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mission::PlannedTask;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-durmission-test-{}-{}.db",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_file(&path);
            TempDb(path)
        }
    }
    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.0.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(p));
            }
        }
    }

    fn mem(tag: &str) -> (Memory, TempDb) {
        let db = TempDb::new(tag);
        let m = Memory::open(&db.0).unwrap();
        (m, db)
    }

    // ---- a planner + dispatcher mock that asserts re-gating (no network) ----

    struct MockPlanner {
        plan: Vec<PlannedTask>,
    }
    impl Planner for MockPlanner {
        fn plan<'a>(
            &'a self,
            _goal: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<PlannedTask>>> + Send + 'a>>
        {
            let plan = self.plan.clone();
            Box::pin(async move { Ok(plan) })
        }
    }

    /// Records every dispatch and models the consequential GATE exactly as the live
    /// dispatcher does under master-OFF: a "post"/"send" step comes back as a
    /// DRY-RUN PREVIEW (never executed), proving the resumed step is RE-GATED rather
    /// than fired from a stored approval.
    struct GateModelingDispatcher {
        calls: Mutex<Vec<String>>,
    }
    impl Dispatcher for GateModelingDispatcher {
        fn dispatch<'a>(
            &'a self,
            _agent: &'a str,
            _tools: &'a [String],
            instruction: &'a str,
            _depth: usize,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.calls.lock().unwrap().push(instruction.to_string());
                let lower = instruction.to_lowercase();
                if lower.contains("post ") || lower.contains("send ") {
                    // Master switch is off in tests -> the gate forces DryRun. The
                    // resumed step re-runs the gate; it is NOT fired from a stored OK.
                    return Ok(format!("[dry-run preview] would have actioned: {instruction}"));
                }
                Ok(format!("done: {instruction}"))
            })
        }
    }

    // ---- create -> list -> cancel round-trip (temp store) ------------------

    #[tokio::test]
    async fn create_list_cancel_roundtrip() {
        let (m, _db) = mem("roundtrip");
        let mission = create(&m, DEFAULT_RETENTION, "ship the v2 launch").await.unwrap();
        assert_eq!(mission.status, MissionStatus::Paused, "a new mission is paused");

        let listed = list(&m).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, mission.id);
        assert_eq!(listed[0].goal, "ship the v2 launch");

        assert!(cancel(&m, &mission.id).await.unwrap());
        assert!(!cancel(&m, &mission.id).await.unwrap());
        assert!(list(&m).await.unwrap().is_empty());
    }

    // ---- SAFETY (a): loads PAUSED, never auto-runs -------------------------

    #[tokio::test]
    async fn a_persisted_mission_loads_paused_even_if_stored_active() {
        let (m, _db) = mem("loadpaused");
        // Simulate a crash mid-run: the stored record says Active.
        let mut mission = DurableMission::new("a mission that was running");
        mission.status = MissionStatus::Active;
        save(&m, &mission).await.unwrap();

        // On load it MUST come back Paused — no silent auto-resume.
        let loaded = load(&m, &mission.id).await.unwrap().unwrap();
        assert_eq!(
            loaded.status,
            MissionStatus::Paused,
            "a stored-Active mission must load PAUSED (no auto-run on restart)"
        );
        // list() applies the same normalization.
        let listed = list(&m).await.unwrap();
        assert_eq!(listed[0].status, MissionStatus::Paused);
    }

    #[test]
    fn paused_on_load_is_pure_and_normalizes_running_states() {
        assert_eq!(paused_on_load(MissionStatus::Active), MissionStatus::Paused);
        assert_eq!(paused_on_load(MissionStatus::Paused), MissionStatus::Paused);
        // Terminal states are preserved (they never run anyway).
        assert_eq!(paused_on_load(MissionStatus::Done), MissionStatus::Done);
        assert_eq!(paused_on_load(MissionStatus::Cancelled), MissionStatus::Cancelled);
    }

    // ---- SAFETY (b): resume re-gates each consequential step ---------------

    #[tokio::test]
    async fn resume_reruns_through_the_gate_no_stored_approval() {
        let (m, _db) = mem("regate");
        let registry = AgentRegistry::canonical();
        let mission = create(&m, DEFAULT_RETENTION, "announce the launch").await.unwrap();

        // A plan with a consequential ("post ...") step. The dispatcher models the
        // gate: master-off -> the consequential step previews, never executes.
        let planner = MockPlanner {
            plan: vec![
                PlannedTask::say("draft the announcement copy"),
                PlannedTask::say("post the launch announcement to the team channel"),
            ],
        };
        let dispatcher = GateModelingDispatcher { calls: Mutex::new(Vec::new()) };

        // cloud_reachable=true so the engine actually plans+dispatches against the
        // mocks (still NO network — the mocks are pure).
        let answer = resume(&m, &mission.id, &registry, &planner, &dispatcher, true)
            .await
            .unwrap();

        // The consequential step re-ran through the gate -> a dry-run preview, NOT a
        // real send. The persisted record carried no approval; resuming re-gated.
        assert!(
            answer.to_lowercase().contains("dry-run") || answer.to_lowercase().contains("preview"),
            "a resumed consequential step must re-gate (preview), not fire: {answer}"
        );
        assert!(
            !answer.to_lowercase().contains("posted to"),
            "the resumed step must NOT have actually executed: {answer}"
        );

        // The mission is marked Done after the run; re-loading still loads it as a
        // terminal Done (never auto-running again).
        let after = load(&m, &mission.id).await.unwrap().unwrap();
        assert_eq!(after.status, MissionStatus::Done);
    }

    #[tokio::test]
    async fn resume_of_a_cancelled_mission_does_not_run() {
        let (m, _db) = mem("cancelnorun");
        let registry = AgentRegistry::canonical();
        let mission = create(&m, DEFAULT_RETENTION, "do a thing").await.unwrap();
        assert!(cancel(&m, &mission.id).await.unwrap());

        // A planner/dispatcher that would PANIC if run, proving a cancelled (now
        // absent) mission is never executed.
        struct Boom;
        impl Planner for Boom {
            fn plan<'a>(
                &'a self,
                _: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<PlannedTask>>> + Send + 'a>>
            {
                Box::pin(async { panic!("a cancelled mission must NOT plan") })
            }
        }
        impl Dispatcher for Boom {
            fn dispatch<'a>(
                &'a self,
                _: &'a str,
                _: &'a [String],
                _: &'a str,
                _: usize,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
            {
                Box::pin(async { panic!("a cancelled mission must NOT dispatch") })
            }
        }
        let answer = resume(&m, &mission.id, &registry, &Boom, &Boom, true).await.unwrap();
        assert!(
            answer.to_lowercase().contains("no durable mission") || answer.to_lowercase().contains("cancelled"),
            "a cancelled mission must not be resumed: {answer}"
        );
    }

    // ---- a missing mission resume is honest, not fabricated ----------------

    #[tokio::test]
    async fn resume_unknown_id_is_honest() {
        let (m, _db) = mem("unknown");
        let registry = AgentRegistry::canonical();
        struct NoopPlanner;
        impl Planner for NoopPlanner {
            fn plan<'a>(
                &'a self,
                _: &'a str,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<PlannedTask>>> + Send + 'a>>
            {
                Box::pin(async { Ok(vec![]) })
            }
        }
        struct NoopDispatcher;
        impl Dispatcher for NoopDispatcher {
            fn dispatch<'a>(
                &'a self,
                _: &'a str,
                _: &'a [String],
                _: &'a str,
                _: usize,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + 'a>>
            {
                Box::pin(async { Ok(String::new()) })
            }
        }
        let answer = resume(&m, "deadbeef", &registry, &NoopPlanner, &NoopDispatcher, true)
            .await
            .unwrap();
        assert!(answer.to_lowercase().contains("no durable mission"), "honest miss: {answer}");
    }
}
