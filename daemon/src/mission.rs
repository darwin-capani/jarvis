//! The mission engine: FURY's "assemble the team for X."
//!
//! A MISSION is a multi-step goal FURY decomposes into a short, ordered list of
//! sub-tasks, dispatches each to the specialist that owns it, then synthesizes
//! the results into one spoken-friendly answer. The whole thing is bounded and
//! safe by construction:
//!
//! - **Planning is injected.** [`decompose`] asks a [`Planner`] to turn a goal
//!   into [`PlannedTask`]s. The real planner ([`CloudPlanner`]) uses the cloud
//!   brain (a planning prompt); tests drive a mock planner, so NO test ever
//!   makes a real cloud call. The plan is then clamped to the engine's bounds
//!   ([`bound_plan`]) — never trusted raw.
//! - **Dispatch reuses the existing isolation.** Each sub-task is routed to its
//!   owner via [`AgentRegistry::select`], then run by a [`Dispatcher`] AS THAT
//!   AGENT — with that agent's persona AND its tool allowlist. The real
//!   dispatcher ([`CloudDispatcher`]) calls the SAME `complete_with_tools` cloud
//!   tool loop a direct request uses, handing it the dispatched agent's `tools`,
//!   so a sub-task dispatched to friday may use ONLY friday's tools and any
//!   consequential tool still routes through `integrations::gate()` exactly as a
//!   direct call would. There is NO escalation and NO bypass: FURY does not lend
//!   a sub-task its own scope, and a sub-task can never reach a tool outside the
//!   owning specialist's allowlist (the cloud loop's `agent_may_use` is the same
//!   gate the direct path uses).
//! - **Bounded.** At most [`MAX_SUBTASKS`] sub-tasks, depth exactly 1 (a
//!   sub-task can never launch its own mission — `fury_mission` is not in any
//!   specialist's allowlist, and the dispatcher refuses it defensively), and an
//!   overall iteration budget. Exceeding a bound TRUNCATES with a clear note in
//!   the synthesis — never a silent drop, never an unbounded run.
//! - **Honest about cost.** A real mission needs the cloud reachable and costs
//!   tokens. Offline, [`run_mission`] degrades to a friendly "missions need the
//!   cloud" line and does NOT pretend it ran.
//!
//! Honesty, as everywhere in the constellation: FURY coordinates profiles on ONE
//! engine, not separate minds, and never reports a sub-task result it did not
//! actually get back.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::agents::AgentRegistry;
use crate::memory::Memory;

/// Hard cap on the number of sub-tasks one mission runs. A goal that decomposes
/// into more is TRUNCATED to this many (the rest are reported as not-attempted in
/// the synthesis) — a mission is a bounded burst of work, never an open-ended
/// agent that spawns indefinitely.
pub const MAX_SUBTASKS: usize = 6;

/// Mission depth is exactly 1: a mission dispatches sub-tasks to specialists, and
/// a sub-task is a SINGLE delegated turn that can never itself launch a mission.
/// Enforced two ways — `fury_mission` is in no specialist's allowlist (so the
/// cloud loop would refuse it), and the dispatcher refuses it defensively
/// regardless of the agent. This constant documents the contract the tests pin.
pub const MAX_DEPTH: usize = 1;

/// Whole-mission iteration budget: the maximum number of sub-task DISPATCHES one
/// mission may perform, independent of [`MAX_SUBTASKS`]. With the per-sub-task
/// cloud tool loop already capped (TOOL_LOOP_MAX_CALLS) this bounds the OUTER
/// fan-out so a mission's total work is finite even if a planner returns the max
/// list. Equal to [`MAX_SUBTASKS`] today (each sub-task dispatched at most once),
/// kept a separate knob so the outer bound can tighten without touching fan-out.
pub const MISSION_BUDGET: usize = MAX_SUBTASKS;

/// One planned sub-task: a short instruction plus the routing hint the planner
/// chose. `intent` mirrors the classifier intents `AgentRegistry::select` keys on
/// ("conversation", "app.launch", ...); the dispatcher still re-resolves the
/// OWNER from (intent, instruction) so a planner that picks a poor hint cannot
/// smuggle a task to the wrong agent — `select` has the final say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTask {
    /// The classifier-style intent hint for routing (e.g. "conversation").
    pub intent: String,
    /// The natural-language instruction handed to the owning specialist.
    pub instruction: String,
}

impl PlannedTask {
    /// Constructor with an explicit intent hint.
    pub fn new(intent: impl Into<String>, instruction: impl Into<String>) -> Self {
        PlannedTask { intent: intent.into(), instruction: instruction.into() }
    }

    /// Convenience constructor for a conversation-intent sub-task (the common
    /// case; most mission pieces are phrased as a request, not a bare intent).
    /// This is what [`parse_plan`] uses, so the production planner reaches it.
    pub fn say(instruction: impl Into<String>) -> Self {
        Self::new("conversation", instruction)
    }
}

/// A sub-task after routing: the resolved OWNER plus what it was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dispatched {
    /// The specialist that owns this sub-task (resolved via `select`).
    pub agent: String,
    /// The instruction handed to that specialist.
    pub instruction: String,
}

/// One sub-task's outcome after the dispatcher ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubResult {
    /// The specialist that handled it.
    pub agent: String,
    /// What it was asked to do (echoed for the synthesis).
    pub instruction: String,
    /// The specialist's answer, or a friendly error string when the dispatch
    /// failed (a single sub-task failing never aborts the whole mission).
    pub outcome: String,
    /// True when `outcome` is an error/degrade rather than a real answer.
    pub failed: bool,
}

/// The full result of a mission: every sub-task outcome, plus whether the plan
/// was truncated to fit the bounds (so the synthesis can say so honestly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissionReport {
    /// The original goal.
    pub goal: String,
    /// Each sub-task's outcome, in dispatch order.
    pub results: Vec<SubResult>,
    /// How many sub-tasks the planner proposed BEFORE truncation (so we can note
    /// "covered N of M" when the plan overran the bounds).
    pub planned: usize,
    /// True when the plan exceeded a bound and was truncated.
    pub truncated: bool,
}

// ---------------------------------------------------------------------------
// Injected seams (so every test is hermetic — no real cloud, ever)
// ---------------------------------------------------------------------------

/// A `Send` future returned by the trait methods. Spelled out explicitly so the
/// traits stay object-safe (`&dyn Planner` / `&dyn Dispatcher`) WITHOUT pulling
/// in the async-trait crate — the "no new dependencies" rule applies here, and
/// this mirrors `heal::BrainFuture` exactly. The production path and every mock
/// implement these methods.
type PlanFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<PlannedTask>>> + Send + 'a>>;
type DispatchFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// Decomposes a goal into sub-tasks. The real implementation asks the cloud
/// brain (a planning prompt); tests inject a mock that returns a fixed plan, so
/// no test makes a network call.
pub trait Planner: Send + Sync {
    /// Turn `goal` into an ordered list of sub-tasks. May return MORE than the
    /// bounds allow — [`bound_plan`] clamps the result; the planner itself is not
    /// trusted to self-limit.
    fn plan<'a>(&'a self, goal: &'a str) -> PlanFuture<'a>;
}

/// Runs ONE sub-task as a named specialist, under that specialist's tool
/// allowlist. The real implementation calls the existing `complete_with_tools`
/// cloud tool loop (so the per-agent allowlist + the consequential-action gate
/// apply exactly as on a direct request); tests inject a mock that records what
/// it was handed and answers without a network call.
pub trait Dispatcher: Send + Sync {
    /// Execute `instruction` as `agent`, with `tools` as the agent's allowlist.
    /// `depth` is the current mission depth (0 for a mission's own sub-tasks);
    /// the dispatcher MUST refuse to run a sub-task whose instruction would
    /// itself launch a mission (depth guard) — see [`MAX_DEPTH`].
    fn dispatch<'a>(
        &'a self,
        agent: &'a str,
        tools: &'a [String],
        instruction: &'a str,
        depth: usize,
    ) -> DispatchFuture<'a>;
}

// ---------------------------------------------------------------------------
// Pure core (unit-tested without any I/O)
// ---------------------------------------------------------------------------

/// Clamp a raw plan to the engine's bounds: at most [`MAX_SUBTASKS`] sub-tasks,
/// each with a non-empty instruction (blank ones are dropped, not run). Returns
/// the bounded list plus the ORIGINAL non-blank count, so the caller can tell
/// whether truncation occurred. Pure — the bound logic is unit-testable with no
/// planner and no I/O.
pub fn bound_plan(raw: Vec<PlannedTask>) -> (Vec<PlannedTask>, usize) {
    let cleaned: Vec<PlannedTask> = raw
        .into_iter()
        .filter(|t| !t.instruction.trim().is_empty())
        .collect();
    let original = cleaned.len();
    let bounded: Vec<PlannedTask> = cleaned.into_iter().take(MAX_SUBTASKS).collect();
    (bounded, original)
}

/// Route a bounded plan to owners via [`AgentRegistry::select`]. Each task's
/// OWNER is resolved from (intent, instruction) — the planner's hint does not get
/// the final say, `select` does — so a sub-task always lands on the specialist
/// that actually holds the relevant tools (or darwin as the fallback). FURY never
/// dispatches a sub-task to ITSELF: a mission that resolved back to fury would be
/// a recursive mission, which depth=1 forbids; such a task is re-pointed at the
/// orchestrator (darwin) instead. `cloud_reachable` is passed through so routing
/// matches what a direct request would do. Pure over the registry — unit-testable
/// without a dispatcher.
pub fn route_plan(
    registry: &AgentRegistry,
    plan: &[PlannedTask],
    cloud_reachable: bool,
) -> Vec<Dispatched> {
    plan.iter()
        .map(|task| {
            let owner = registry.select(&task.intent, &task.instruction, cloud_reachable);
            // A sub-task must never resolve back to the mission orchestrator
            // itself (that would be a recursive mission — depth=1 forbids it).
            // Re-point such a task at the prime orchestrator, which coordinates
            // without the mission tool.
            let agent = if owner.name == "fury" {
                registry.orchestrator().name.clone()
            } else {
                owner.name.clone()
            };
            Dispatched { agent, instruction: task.instruction.clone() }
        })
        .collect()
}

/// True when an instruction would itself launch a mission — the depth guard's
/// predicate. A sub-task is a SINGLE delegated turn; one that asks to "run a
/// mission" / "assemble the team" / "orchestrate" all of something is refused so
/// a mission cannot recurse. Pure and unit-testable. Deliberately conservative:
/// it keys on the same multi-step phrasings FURY's own delegation cues use.
pub fn would_recurse(instruction: &str) -> bool {
    let lower = instruction.to_lowercase();
    const RECURSION_CUES: &[&str] = &[
        "run a mission",
        "launch a mission",
        "start a mission",
        "another mission",
        "fury_mission",
        "assemble the team",
        "orchestrate everything",
    ];
    RECURSION_CUES.iter().any(|c| lower.contains(c))
}

/// Combine sub-task results into one spoken-friendly answer. Names the goal, then
/// each specialist's contribution, and — when the plan was truncated — states
/// honestly how many pieces were covered out of how many were proposed. With no
/// results at all (an empty plan) it says so plainly rather than fabricating an
/// outcome. Pure — the synthesis shape is unit-testable without any I/O.
pub fn synthesize(report: &MissionReport) -> String {
    if report.results.is_empty() {
        return format!(
            "I couldn't break \"{}\" into anything actionable, sir — give me a more concrete objective.",
            report.goal.trim()
        );
    }
    let mut out = format!("Mission on \"{}\", sir. ", report.goal.trim());
    for r in &report.results {
        if r.failed {
            out.push_str(&format!("{} couldn't complete its piece ({}). ", r.agent, r.outcome.trim()));
        } else {
            out.push_str(&format!("{}: {} ", r.agent, r.outcome.trim()));
        }
    }
    if report.truncated {
        out.push_str(&format!(
            "I capped the mission at {} of the {} steps to keep it bounded; the rest weren't attempted.",
            report.results.len(),
            report.planned,
        ));
    } else {
        out.push_str("That's the team's report.");
    }
    out.trim().to_string()
}

/// The honest offline degrade line: a mission needs the cloud and costs tokens,
/// so with the cloud unreachable FURY says so plainly rather than pretending. Pure.
pub fn offline_degrade(goal: &str) -> String {
    format!(
        "Missions need the cloud, sir — I can't assemble the team offline. Reconnect and I'll run \"{}\" for you.",
        goal.trim()
    )
}

// ---------------------------------------------------------------------------
// The orchestration (thin glue over the pure core + the injected seams)
// ---------------------------------------------------------------------------

/// Run a full mission for `goal`: plan -> bound -> route -> dispatch each
/// sub-task as its owner (depth-guarded, budget-capped) -> synthesize. Offline,
/// it short-circuits to [`offline_degrade`] WITHOUT planning or dispatching (no
/// tokens spent, no false claim of work). Generic over the [`Planner`] and
/// [`Dispatcher`] seams so tests drive it with mocks and the daemon wires the
/// cloud-backed pair.
///
/// A single sub-task failing is recorded as a failed [`SubResult`] and the
/// mission continues — one bad piece never aborts the whole campaign. The
/// returned `String` is the synthesized, spoken-friendly answer.
pub async fn run_mission(
    goal: &str,
    registry: &AgentRegistry,
    planner: &dyn Planner,
    dispatcher: &dyn Dispatcher,
    cloud_reachable: bool,
) -> String {
    // Cost/offline honesty: a mission is real cloud work. Don't even plan offline.
    if !cloud_reachable {
        return offline_degrade(goal);
    }

    // Plan (injected) then clamp to the bounds — the planner is never trusted to
    // self-limit.
    let raw = match planner.plan(goal).await {
        Ok(plan) => plan,
        Err(e) => {
            return format!(
                "I couldn't plan that mission, sir — {}. Give me the objective again, more concretely.",
                e
            );
        }
    };
    let (bounded, planned) = bound_plan(raw);
    let truncated = planned > bounded.len();

    let dispatched = route_plan(registry, &bounded, cloud_reachable);

    let mut results = Vec::with_capacity(dispatched.len());
    let mut budget = MISSION_BUDGET;
    for d in &dispatched {
        // Outer iteration budget: a hard ceiling on dispatches independent of the
        // sub-task count, so a mission's total work is finite. Hitting it stops
        // dispatching and is reported as truncation in the synthesis.
        if budget == 0 {
            break;
        }
        budget -= 1;

        // Depth guard: a sub-task that would itself launch a mission is refused
        // here (defense in depth — the agent also lacks fury_mission). depth=0:
        // these are the mission's own sub-tasks; the dispatcher enforces MAX_DEPTH.
        if would_recurse(&d.instruction) {
            results.push(SubResult {
                agent: d.agent.clone(),
                instruction: d.instruction.clone(),
                outcome: "a sub-task cannot launch its own mission".to_string(),
                failed: true,
            });
            continue;
        }

        // The dispatched agent's REAL allowlist — the cloud loop offers/accepts
        // only these tools, so the sub-task is constrained to the owning
        // specialist's scope (no escalation through FURY).
        let tools = registry
            .get(&d.agent)
            .map(|a| a.tools.clone())
            .unwrap_or_default();

        let (outcome, failed) = match dispatcher.dispatch(&d.agent, &tools, &d.instruction, 0).await {
            Ok(answer) => (answer, false),
            Err(e) => (e.to_string(), true),
        };
        results.push(SubResult {
            agent: d.agent.clone(),
            instruction: d.instruction.clone(),
            outcome,
            failed,
        });
    }

    // If the outer budget stopped us before all bounded tasks ran, that is also a
    // truncation the synthesis must disclose.
    let truncated = truncated || results.len() < bounded.len();

    let report = MissionReport {
        goal: goal.to_string(),
        results,
        planned,
        truncated,
    };
    synthesize(&report)
}

// ---------------------------------------------------------------------------
// Cloud-backed implementations (wired by the daemon; NOT exercised in tests)
// ---------------------------------------------------------------------------

/// The real planner: asks the cloud brain to decompose the goal. A thin wrapper
/// around a single plain Messages completion with a planning prompt, parsed into
/// [`PlannedTask`]s. NOT exercised by any test (tests inject a mock) — the daemon
/// constructs it on the live path only.
pub struct CloudPlanner {
    /// The cloud model id to plan with (the heavy model — planning benefits from
    /// reasoning).
    pub model: String,
    /// Per-plan token budget.
    pub max_tokens: u32,
}

/// The planning system prompt: a one-line framing so the model treats the user
/// message purely as a goal to decompose.
const PLAN_SYSTEM: &str =
    "You decompose a goal into a short ordered list of single-step sub-tasks. \
     Output only the list, one sub-task per line.";
/// Planning is latency-insensitive (it precedes the dispatch fan-out); give it
/// the same generous ceiling the heal drafter uses.
const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Planner for CloudPlanner {
    fn plan<'a>(&'a self, goal: &'a str) -> PlanFuture<'a> {
        Box::pin(async move {
            let prompt = planning_prompt(goal);
            // A plain (tool-free) completion: planning produces TEXT (the task
            // list), it does not act. The sub-tasks act later, each as its owner.
            let raw = crate::anthropic::complete_plain(
                &self.model,
                self.max_tokens,
                PLAN_SYSTEM,
                &prompt,
                PLAN_TIMEOUT,
            )
            .await?;
            Ok(parse_plan(&raw))
        })
    }
}

/// The planning prompt: instruct the cloud brain to decompose the goal into a
/// short, ordered list of single-step sub-tasks, one per line, no recursion. Pure
/// (no I/O) so the instruction text is unit-testable.
pub fn planning_prompt(goal: &str) -> String {
    format!(
        "You are FURY, a mission orchestrator. Break this goal into a SHORT ordered list \
         of at most {MAX_SUBTASKS} concrete, single-step sub-tasks, each a plain \
         instruction a specialist could carry out in ONE turn. Do NOT nest missions or \
         create a sub-task that itself orchestrates others. Output ONE sub-task per line, \
         no numbering, no commentary.\n\nGOAL: {goal}"
    )
}

/// Parse the planner's line-per-task text into [`PlannedTask`]s: each non-blank
/// line (with any leading list marker stripped) becomes a conversation-intent
/// sub-task. Pure and unit-testable. The result is still clamped by
/// [`bound_plan`] downstream — this parser does not enforce the count itself.
pub fn parse_plan(raw: &str) -> Vec<PlannedTask> {
    raw.lines()
        .map(|line| strip_list_marker(line.trim()))
        .filter(|line| !line.is_empty())
        .map(PlannedTask::say)
        .collect()
}

/// Strip a leading list marker ("1.", "-", "*", "•") and following space from a
/// line, so the instruction text is clean regardless of how the model formatted
/// the list. Pure.
fn strip_list_marker(line: &str) -> String {
    let trimmed = line.trim_start();
    // "1." / "1)" style numbering.
    if let Some(rest) = trimmed.split_once(['.', ')']) {
        if !rest.0.is_empty() && rest.0.chars().all(|c| c.is_ascii_digit()) {
            return rest.1.trim_start().to_string();
        }
    }
    // Bullet markers.
    for marker in ["- ", "* ", "• ", "–  ", "— "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return rest.trim_start().to_string();
        }
    }
    trimmed.to_string()
}

/// The real dispatcher: runs a sub-task through the SAME cloud tool loop a direct
/// request uses (`complete_with_tools`), handing it the dispatched agent's tool
/// allowlist — so the per-agent isolation and the consequential-action gate apply
/// exactly as they would on a direct call. NOT exercised by any test (tests
/// inject a mock); the daemon constructs it on the live path only.
pub struct CloudDispatcher<'a> {
    /// The cloud model id sub-tasks run under.
    pub model: String,
    /// Per-sub-task token budget.
    pub max_tokens: u32,
    /// Backing memory for the per-sub-task tool loop (remember/recall, namespaced
    /// by the caller as needed).
    pub memory: &'a Memory,
    /// The orchestrator's agent name (from the registry), so the dispatcher can
    /// tell whether a dispatched agent voices the global persona (orchestrator)
    /// or its own per-agent persona (specialist) — threaded into the cloud
    /// system so each sub-task speaks in the right persona and caches per-agent.
    pub orchestrator: String,
    /// Whether the mission was spawned from a TRUSTED, user-originated request
    /// (`true`) or an UNTRUSTED origin (`false`: a `fury_mission` requested on a
    /// tool continuation — possibly injected content — or a resumed/standing
    /// mission). Passed to each sub-task's `complete_with_tools` as `context_trusted`
    /// so an untrusted mission cannot reset the prompt-injection egress guard to
    /// open on its sub-tasks' call 0. See anthropic.rs `tool_loop`.
    pub context_trusted: bool,
}

impl Dispatcher for CloudDispatcher<'_> {
    fn dispatch<'a>(
        &'a self,
        agent: &'a str,
        tools: &'a [String],
        instruction: &'a str,
        depth: usize,
    ) -> DispatchFuture<'a> {
        Box::pin(async move {
            // Depth guard (defense in depth): a sub-task is a SINGLE delegated
            // turn; it can never run at or beyond MAX_DEPTH, and a sub-task that
            // would launch its own mission is refused outright.
            if depth >= MAX_DEPTH || would_recurse(instruction) {
                return Err(anyhow::anyhow!(
                    "a sub-task cannot launch its own mission (depth guard)"
                ));
            }
            // A sub-task must NEVER be handed the mission tool, even if a roster
            // edit someday adds it to a specialist — strip it so a sub-task can't
            // recurse through the tool surface.
            let scoped: Vec<String> = tools
                .iter()
                .filter(|t| t.as_str() != "fury_mission")
                .cloned()
                .collect();
            // The sub-task's memory namespace ("agent.<name>"), so the in-loop
            // recall tools stay scoped to this agent's own namespace + shared
            // facts — constellation isolation rides along on a sub-task exactly
            // as on a direct call (matches AgentProfile::namespace's format).
            let namespace = format!("agent.{agent}");
            // The dispatched specialist's own persona (the shared grounding
            // preamble + this persona), so the sub-task's cloud reply is voiced
            // in that agent's persona and caches per-agent; the orchestrator (if
            // ever dispatched) voices the global persona (None). The shared
            // preamble always carries the no-fabrication grounding.
            let is_orchestrator = agent == self.orchestrator;
            let agent_persona = crate::anthropic::agent_persona_text(agent, is_orchestrator);
            // SHARED WORLD MODEL context relevant to this sub-task instruction, from
            // the shared user.world.* tier — so a dispatched sub-task reasons over
            // the one coherent world picture every agent shares, just like a direct
            // call. The world model reads only the shared tier, so a sub-task can
            // never see another agent's private notes.
            let world_context =
                crate::anthropic::grounded_world_live(instruction, self.memory).await;
            // PERSONALIZATION: the bounded user-model summary (observed profile),
            // shared-tier only, so a dispatched sub-task personalizes to the real
            // observed user exactly like a direct call — never another agent's
            // private notes.
            let personalization =
                crate::anthropic::grounded_personalization_live(self.memory).await;
            // The SAME cloud tool loop the direct path uses — per-agent allowlist
            // + gate + recall isolation ride along unchanged. No history/facts
            // threaded here: a sub-task is a fresh, self-contained instruction.
            crate::anthropic::complete_with_tools(
                &self.model,
                self.max_tokens,
                instruction,
                &[],
                &[],
                self.memory,
                &scoped,
                &namespace,
                agent_persona.as_deref(),
                &world_context,
                &personalization,
                // Inherit the mission's trust: an untrusted mission keeps every
                // sub-task's egress guard armed on its own call 0.
                self.context_trusted,
            )
            .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentRegistry;
    use std::sync::Mutex;

    // ---- Mock planner + dispatcher (NO network, NO cloud) ------------------

    /// A planner that returns a fixed plan, recording the goal it saw.
    struct MockPlanner {
        plan: Vec<PlannedTask>,
        seen_goal: Mutex<Option<String>>,
    }
    impl MockPlanner {
        fn new(plan: Vec<PlannedTask>) -> Self {
            MockPlanner { plan, seen_goal: Mutex::new(None) }
        }
    }
    impl Planner for MockPlanner {
        fn plan<'a>(&'a self, goal: &'a str) -> PlanFuture<'a> {
            Box::pin(async move {
                *self.seen_goal.lock().unwrap() = Some(goal.to_string());
                Ok(self.plan.clone())
            })
        }
    }

    /// One recorded dispatch: exactly what the engine handed the dispatcher.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Call {
        agent: String,
        tools: Vec<String>,
        instruction: String,
        depth: usize,
    }

    /// A dispatcher that records every call and answers deterministically. It
    /// ENFORCES the allowlist exactly like the real cloud loop's `agent_may_use`:
    /// a sub-task whose instruction asks for a tool NOT in the handed `tools` is
    /// refused — so the test can prove a friday sub-task cannot reach a steve-only
    /// tool. It also models the consequential GATE: a "post"/"send" style action
    /// comes back as a DRY-RUN PREVIEW (never executed) because the master switch
    /// is off in tests, exactly as `integrations::gate` would force.
    struct MockDispatcher {
        calls: Mutex<Vec<Call>>,
    }
    impl MockDispatcher {
        fn new() -> Self {
            MockDispatcher { calls: Mutex::new(Vec::new()) }
        }
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl Dispatcher for MockDispatcher {
        fn dispatch<'a>(
            &'a self,
            agent: &'a str,
            tools: &'a [String],
            instruction: &'a str,
            depth: usize,
        ) -> DispatchFuture<'a> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(Call {
                    agent: agent.to_string(),
                    tools: tools.to_vec(),
                    instruction: instruction.to_string(),
                    depth,
                });
                // Defense-in-depth: the dispatcher must NEVER be handed the
                // mission tool (no recursion through the tool surface).
                assert!(
                    !tools.iter().any(|t| t == "fury_mission"),
                    "a sub-task must never receive fury_mission"
                );
                // Model the per-agent allowlist: a sub-task that names a specific
                // tool it needs is only allowed if the handed allowlist has it.
                // The trigger phrase ("ship a github fix") deliberately carries NO
                // routing cue so the test can pin a friday-routed sub-task that
                // still reaches for a steve-only tool.
                let lower = instruction.to_lowercase();
                if lower.contains("ship a github fix") && !tools.iter().any(|t| t == "github_open_pr")
                {
                    return Err(anyhow::anyhow!(
                        "This agent is not permitted to use the 'github_open_pr' tool."
                    ));
                }
                // Model the consequential gate: a posting/sending action previews
                // (DryRun) rather than executes, because the master switch is off.
                if lower.contains("post ") || lower.contains("send ") {
                    return Ok(format!("[dry-run preview] would have actioned: {instruction}"));
                }
                Ok(format!("done: {instruction}"))
            })
        }
    }

    fn reg() -> AgentRegistry {
        AgentRegistry::canonical()
    }

    // ---- decompose / bounds ------------------------------------------------

    #[tokio::test]
    async fn decompose_returns_bounded_subtasks() {
        // A planner that proposes WAY more than the cap.
        let big: Vec<PlannedTask> = (0..20)
            .map(|i| PlannedTask::say(format!("step {i}")))
            .collect();
        let planner = MockPlanner::new(big);
        let raw = planner.plan("do a lot").await.unwrap();
        let (bounded, planned) = bound_plan(raw);
        assert_eq!(planned, 20, "the original count is preserved for honest reporting");
        assert_eq!(bounded.len(), MAX_SUBTASKS, "the plan is clamped to the cap");
        assert!(bounded.len() <= MAX_SUBTASKS);
        assert_eq!(planner.seen_goal.lock().unwrap().as_deref(), Some("do a lot"));
    }

    #[test]
    fn bound_plan_drops_blank_instructions() {
        let raw = vec![
            PlannedTask::say("real one"),
            PlannedTask::say("   "),
            PlannedTask::say(""),
            PlannedTask::say("another"),
        ];
        let (bounded, planned) = bound_plan(raw);
        assert_eq!(planned, 2, "blank instructions are not counted");
        assert_eq!(bounded.len(), 2);
        assert!(bounded.iter().all(|t| !t.instruction.trim().is_empty()));
    }

    // ---- routing: each sub-task to the right specialist via select() -------

    #[test]
    fn route_plan_sends_each_subtask_to_the_correct_specialist() {
        let registry = reg();
        let plan = vec![
            PlannedTask::say("research our competitors and their ad trends"),
            PlannedTask::say("draft a caption for the launch post"),
            PlannedTask::say("investigate the build bug blocking release"),
            PlannedTask::new("app.launch", "open safari"),
        ];
        let routed = route_plan(&registry, &plan, true);
        let owners: Vec<&str> = routed.iter().map(|d| d.agent.as_str()).collect();
        assert_eq!(owners, vec!["vision", "veronica", "steve", "oracle"]);
    }

    #[test]
    fn route_plan_never_dispatches_back_to_fury() {
        // An instruction that itself reads as a mission would resolve to fury via
        // select(); route_plan must re-point it at the orchestrator, not fury,
        // because a sub-task can never be a recursive mission.
        let registry = reg();
        let plan = vec![PlannedTask::say("orchestrate the whole campaign end to end")];
        let routed = route_plan(&registry, &plan, true);
        assert_eq!(routed[0].agent, "darwin", "a fury-resolved sub-task is re-pointed at the orchestrator");
        assert_ne!(routed[0].agent, "fury", "a sub-task must never be dispatched to fury (no recursion)");
    }

    // ---- dispatch carries the OWNER's allowlist (isolation) ----------------

    #[tokio::test]
    async fn dispatch_constrains_each_subtask_to_its_owner_allowlist() {
        let registry = reg();
        let planner = MockPlanner::new(vec![
            PlannedTask::say("give me the morning brief"), // -> friday
            PlannedTask::say("investigate this bug in the build"), // -> steve
        ]);
        let dispatcher = MockDispatcher::new();
        let _ = run_mission("launch prep", &registry, &planner, &dispatcher, true).await;

        let calls = dispatcher.calls();
        assert_eq!(calls.len(), 2);

        // The friday sub-task was handed friday's REAL allowlist — and that
        // allowlist does NOT contain a steve-only tool.
        let friday = &calls[0];
        assert_eq!(friday.agent, "friday");
        assert_eq!(friday.tools, registry.get("friday").unwrap().tools);
        assert!(
            !friday.tools.iter().any(|t| t == "github_open_pr"),
            "a friday sub-task must NOT carry a steve-only tool: {:?}",
            friday.tools
        );
        // depth is 0 for a mission's own sub-tasks.
        assert_eq!(friday.depth, 0);

        // The steve sub-task carries steve's allowlist, which DOES hold the
        // github tool.
        let steve = &calls[1];
        assert_eq!(steve.agent, "steve");
        assert!(steve.tools.iter().any(|t| t == "github_open_pr"));
    }

    #[tokio::test]
    async fn a_friday_subtask_cannot_use_a_steve_only_tool() {
        // PROVE the isolation end to end: a sub-task routed to friday that tries
        // to open a GitHub PR (a steve-only tool) is REFUSED — the dispatcher
        // (modeling the cloud loop's agent_may_use) rejects it because friday's
        // allowlist lacks github_open_pr.
        let registry = reg();
        // "morning brief" routes this to friday (no steve routing cue in the
        // text); the instruction then reaches for a steve-only action via a
        // phrase that is NOT itself a routing cue.
        let planner = MockPlanner::new(vec![PlannedTask::say(
            "in the morning brief, ship a github fix for the typo",
        )]);
        let dispatcher = MockDispatcher::new();
        let answer = run_mission("brief and patch", &registry, &planner, &dispatcher, true).await;

        let calls = dispatcher.calls();
        assert_eq!(calls[0].agent, "friday", "routed to friday by the brief cue");
        assert!(
            !calls[0].tools.iter().any(|t| t == "github_open_pr"),
            "friday was not handed the steve-only tool"
        );
        // The sub-task came back as a failure with the explicit refusal — never a
        // silent success, never an escalation.
        assert!(
            answer.contains("not permitted") && answer.contains("github_open_pr"),
            "the friday sub-task must be refused the steve-only tool: {answer}"
        );
    }

    // ---- consequential sub-task honors the gate (DryRun) -------------------

    #[tokio::test]
    async fn consequential_subtask_previews_under_the_gate() {
        // veronica owns slack_post_message, but with the master switch off the
        // gate forces a DRY-RUN preview — the action is NOT executed. The mission
        // path must not escalate that.
        let registry = reg();
        let planner = MockPlanner::new(vec![PlannedTask::say(
            "post the launch announcement to the team channel",
        )]);
        let dispatcher = MockDispatcher::new();
        let answer = run_mission("announce launch", &registry, &planner, &dispatcher, true).await;
        assert_eq!(dispatcher.calls()[0].agent, "veronica");
        assert!(
            answer.to_lowercase().contains("dry-run") || answer.to_lowercase().contains("preview"),
            "a consequential sub-task must preview under the gate, not execute: {answer}"
        );
        assert!(
            !answer.to_lowercase().contains("posted to"),
            "the action must NOT have actually executed: {answer}"
        );
    }

    // ---- bounds: fan-out / depth / budget, with signalled truncation -------

    #[tokio::test]
    async fn fan_out_is_capped_and_truncation_is_signalled() {
        let registry = reg();
        // 10 plain sub-tasks; the engine must run at most MAX_SUBTASKS and SAY so.
        let plan: Vec<PlannedTask> = (0..10)
            .map(|i| PlannedTask::say(format!("handle step {i}")))
            .collect();
        let planner = MockPlanner::new(plan);
        let dispatcher = MockDispatcher::new();
        let answer = run_mission("big rollout", &registry, &planner, &dispatcher, true).await;
        assert_eq!(
            dispatcher.calls().len(),
            MAX_SUBTASKS,
            "no more than the cap is ever dispatched"
        );
        assert!(
            answer.contains(&format!("{} of the 10", MAX_SUBTASKS)),
            "truncation must be disclosed honestly: {answer}"
        );
    }

    #[test]
    fn depth_is_one_recursion_is_refused() {
        assert_eq!(MAX_DEPTH, 1, "missions are exactly one level deep");
        // A sub-task whose instruction would itself launch a mission is caught.
        assert!(would_recurse("run a mission to do everything"));
        assert!(would_recurse("assemble the team for the offsite"));
        assert!(would_recurse("call fury_mission again"));
        // Ordinary single-step instructions are not flagged.
        assert!(!would_recurse("draft the launch email"));
        assert!(!would_recurse("open safari"));
    }

    #[tokio::test]
    async fn a_recursive_subtask_is_refused_not_run() {
        let registry = reg();
        // The plan contains a sub-task that would recurse; it must be recorded as
        // a failure and NOT dispatched.
        let planner = MockPlanner::new(vec![
            PlannedTask::say("draft the agenda"),
            PlannedTask::say("then run a mission to handle the rest"),
        ]);
        let dispatcher = MockDispatcher::new();
        let answer = run_mission("plan offsite", &registry, &planner, &dispatcher, true).await;
        // Only the non-recursive sub-task reached the dispatcher.
        let calls = dispatcher.calls();
        assert_eq!(calls.len(), 1, "the recursive sub-task is never dispatched");
        assert_eq!(calls[0].instruction, "draft the agenda");
        assert!(
            answer.contains("cannot launch its own mission"),
            "the refusal is surfaced, not silent: {answer}"
        );
    }

    #[test]
    fn budget_constant_bounds_total_dispatches() {
        // The outer budget never exceeds the fan-out cap, so a mission's total
        // dispatches are finite even with a max-length plan.
        const { assert!(MISSION_BUDGET <= MAX_SUBTASKS) };
        const { assert!(MISSION_BUDGET >= 1) };
    }

    // ---- offline -> friendly degrade (no planning, no dispatch) ------------

    #[tokio::test]
    async fn offline_degrades_without_planning_or_dispatching() {
        let registry = reg();
        // A planner/dispatcher that would PANIC if called — proving the offline
        // path never touches them (no tokens spent, no false claim of work).
        struct Boom;
        impl Planner for Boom {
            fn plan<'a>(&'a self, _: &'a str) -> PlanFuture<'a> {
                Box::pin(async { panic!("planner must NOT run offline") })
            }
        }
        impl Dispatcher for Boom {
            fn dispatch<'a>(
                &'a self,
                _: &'a str,
                _: &'a [String],
                _: &'a str,
                _: usize,
            ) -> DispatchFuture<'a> {
                Box::pin(async { panic!("dispatcher must NOT run offline") })
            }
        }
        let answer = run_mission("ship the release", &registry, &Boom, &Boom, false).await;
        assert!(
            answer.to_lowercase().contains("need the cloud")
                || answer.to_lowercase().contains("offline"),
            "offline must degrade to a friendly cloud-needed line: {answer}"
        );
        assert!(answer.contains("ship the release"), "the goal is echoed back: {answer}");
    }

    // ---- synthesis combines results ----------------------------------------

    #[test]
    fn synthesize_combines_subtask_results() {
        let report = MissionReport {
            goal: "launch the product".to_string(),
            results: vec![
                SubResult {
                    agent: "vision".to_string(),
                    instruction: "research competitors".to_string(),
                    outcome: "Found three rival launches this week.".to_string(),
                    failed: false,
                },
                SubResult {
                    agent: "veronica".to_string(),
                    instruction: "draft the post".to_string(),
                    outcome: "Drafted the announcement copy.".to_string(),
                    failed: false,
                },
            ],
            planned: 2,
            truncated: false,
        };
        let spoken = synthesize(&report);
        assert!(spoken.contains("launch the product"), "names the goal: {spoken}");
        assert!(spoken.contains("vision"), "names the first specialist: {spoken}");
        assert!(spoken.contains("Found three rival launches"), "carries vision's result: {spoken}");
        assert!(spoken.contains("veronica"), "names the second specialist: {spoken}");
        assert!(spoken.contains("Drafted the announcement copy"), "carries veronica's result: {spoken}");
    }

    #[test]
    fn synthesize_reports_truncation_and_empty_plans_honestly() {
        // Truncated mission: the synthesis discloses the cap.
        let report = MissionReport {
            goal: "do everything".to_string(),
            results: vec![SubResult {
                agent: "vision".to_string(),
                instruction: "first".to_string(),
                outcome: "did the first thing".to_string(),
                failed: false,
            }],
            planned: 9,
            truncated: true,
        };
        let spoken = synthesize(&report);
        assert!(spoken.contains("1 of the 9"), "discloses how much was covered: {spoken}");
        assert!(spoken.contains("weren't attempted"), "is explicit the rest were skipped: {spoken}");

        // Empty plan: honest "couldn't break it down", never a fabricated outcome.
        let empty = MissionReport {
            goal: "vibes".to_string(),
            results: vec![],
            planned: 0,
            truncated: false,
        };
        let spoken = synthesize(&empty);
        assert!(spoken.contains("couldn't break"), "honest about an unactionable goal: {spoken}");
    }

    #[test]
    fn synthesize_marks_a_failed_subtask_without_aborting() {
        let report = MissionReport {
            goal: "two-part job".to_string(),
            results: vec![
                SubResult {
                    agent: "steve".to_string(),
                    instruction: "open a pr".to_string(),
                    outcome: "This agent is not permitted to use the 'x' tool.".to_string(),
                    failed: true,
                },
                SubResult {
                    agent: "vision".to_string(),
                    instruction: "research".to_string(),
                    outcome: "Found the data.".to_string(),
                    failed: false,
                },
            ],
            planned: 2,
            truncated: false,
        };
        let spoken = synthesize(&report);
        assert!(spoken.contains("couldn't complete"), "the failed piece is reported: {spoken}");
        assert!(spoken.contains("Found the data"), "the succeeding piece still lands: {spoken}");
    }

    // ---- planning prompt / parsing (pure, used by the real CloudPlanner) ----

    #[test]
    fn planning_prompt_states_the_bounds_and_no_recursion() {
        let p = planning_prompt("ship the app");
        assert!(p.contains(&MAX_SUBTASKS.to_string()), "states the sub-task cap: {p}");
        assert!(p.to_lowercase().contains("single-step"), "asks for single-step tasks: {p}");
        assert!(p.to_lowercase().contains("do not nest"), "forbids nested missions: {p}");
        assert!(p.contains("ship the app"), "carries the goal: {p}");
    }

    #[test]
    fn parse_plan_handles_markers_and_blanks() {
        let raw = "1. research the market\n- draft the copy\n* open the deck\n\n  • send for review  \nfinal step";
        let tasks = parse_plan(raw);
        let instrs: Vec<&str> = tasks.iter().map(|t| t.instruction.as_str()).collect();
        assert_eq!(
            instrs,
            vec![
                "research the market",
                "draft the copy",
                "open the deck",
                "send for review",
                "final step",
            ],
            "list markers are stripped and blank lines dropped"
        );
        // Every parsed task defaults to the conversation intent.
        assert!(tasks.iter().all(|t| t.intent == "conversation"));
    }
}
