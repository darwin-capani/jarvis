//! PLAN-APPLY — a structured, STATE-BOUND diff for a parked consequential action.
//!
//! This upgrades the cross-turn confirmation PREVIEW from a prose sentence to a
//! field-level DIFF ([`Plan`]) for the consequential tools whose effect is a
//! concrete state change (today: `connector_add` — a `[[mcp.servers]]` write to
//! darwin.toml — and `standing_create` — a persisted recurring mission). The plan
//! is bound to a [`Plan::state_hash`]: the hash of the CURRENT relevant state at
//! plan time. When the human later says "yes", the confirm path RECOMPUTES the
//! state hash and only lets the action proceed if it still MATCHES — otherwise it
//! RE-PARKS a fresh plan ("the state changed since I showed you"). That makes the
//! confirmation TOCTOU-safe by construction.
//!
//! ## SAFETY CONTRACT — this layer can ONLY make the gate STRICTER, never looser
//! The state-hash is an ADDITIONAL precondition on TOP of the existing gates —
//! the master switch (`integrations::consequential_allowed`), a fresh spoken
//! confirm (`confirm::classify_confirmation`), voice-id
//! (`voiceid::current_turn_gate`), the per-agent allowlist, and `!lockdown`. It
//! NEVER replaces or weakens any of them. Concretely, the only outcomes of the
//! bind ([`StateBind`]) are:
//!   * [`StateBind::Fresh`]   — the state is unchanged; the caller FALLS THROUGH
//!     to the UNCHANGED replay path (which STILL runs voice-id + allowlist +
//!     `gate(confirm)`). Fresh authorizes NOTHING on its own.
//!   * [`StateBind::Drifted`] — the state changed (or could not be re-read); the
//!     caller RE-PARKS a fresh plan and fires NOTHING. A NEW failure mode.
//!   * [`StateBind::Unplanned`] — the tool has no structured planner; the caller
//!     uses today's text preview path, byte-for-byte UNCHANGED.
//!
//! There is deliberately NO variant that means "execute regardless": the richest
//! thing the bind can say is "the extra precondition passed", and even then the
//! other gates decide whether anything fires. See `state_hash_gate_only_adds_a_failure`.
//!
//! ## Purity
//! The per-tool planners ([`plan_connector_add`], [`plan_standing_create`]) and
//! the bind ([`bind_state`]) are PURE over `(input, state snapshot)` — the state
//! is passed IN, never read from a global — so the before/after fields, the
//! state-hash, and the drift decision are all unit-testable with no config, no
//! store, and no network. The one IMPURE entry, [`plan_for`], snapshots the live
//! state (connector names from the config, missions from the store) and delegates
//! to the pure planners; it returns `None` for a tool with no planner OR when the
//! `[plan]` section is off OR when the state can't be read — all of which fall
//! back to the unchanged text preview (fail-safe).

use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::memory::Memory;

/// One field-level change a plan proposes: a named `resource` moving from
/// `before` to `after`. All three are SECRET-FREE, human-readable summaries (the
/// same discipline as the audit target) — never a raw token or the full input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    /// What is being changed (e.g. "config/darwin.toml [[mcp.servers]] 'files'").
    pub resource: String,
    /// The current value/state of the resource ("(absent)", a count, a summary).
    pub before: String,
    /// The value/state after the action applies.
    pub after: String,
}

/// A structured, state-bound plan for one parked consequential action. `summary`
/// is a one-line human description; `changes` is the field-level diff; and
/// `state_hash` binds the plan to the CURRENT relevant state so a drift between
/// park and confirm is detected (see [`bind_state`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Plan {
    /// The tool this plan is for (mirrors `PendingConfirmation::tool`).
    pub tool: String,
    /// A one-line, secret-free description of the action.
    pub summary: String,
    /// The field-level diff. Never empty for a real planner (an empty diff would
    /// be indistinguishable from "no planner", so a planner that can't derive a
    /// change returns `None` instead).
    pub changes: Vec<Change>,
    /// SHA-256 (short hex) of the CURRENT relevant state at plan time. The confirm
    /// path recomputes this and compares; a mismatch re-parks. It hashes ONLY the
    /// state (not the input) — so ANY change to the relevant state, even one that
    /// would still let the action succeed, conservatively re-parks.
    pub state_hash: String,
}

/// Hash a state-snapshot string into the stable short-hex `state_hash`. Pure and
/// deterministic: the SAME snapshot always hashes the same, so recomputing at
/// confirm time yields an identical hash iff the state is byte-identical. 16 hex
/// chars (64 bits) — ample to detect drift, short enough to log/echo.
pub fn hash_state(snapshot: &str) -> String {
    let mut h = Sha256::new();
    h.update(snapshot.as_bytes());
    hex::encode(&h.finalize()[..8])
}

/// Read a trimmed string field from a tool-input object (empty when absent).
fn field(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or("").trim().to_string()
}

// ---------------------------------------------------------------------------
// Per-tool PURE planners
// ---------------------------------------------------------------------------

/// PURE planner for `connector_add` given the CURRENT set of configured MCP
/// server names. The relevant state is exactly that set (a new server named the
/// same, or any change to the server list, drifts the plan). Returns `None` for a
/// malformed input (missing name / unknown transport) so the caller falls back to
/// the text preview rather than showing a half-formed diff.
pub fn plan_connector_add(input: &Value, existing_names: &[String]) -> Option<Plan> {
    let name = field(input, "name");
    let transport = field(input, "transport");
    if name.is_empty() {
        return None;
    }
    let after = match transport.as_str() {
        "http" => {
            let url = field(input, "url");
            format!("https endpoint {url} — INERT (agents=[], every tool gated)")
        }
        "stdio" => {
            let command = field(input, "command");
            let args: Vec<String> = input
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let cmd = if args.is_empty() {
                format!("command {command}")
            } else {
                format!("command {command} {}", args.join(" "))
            };
            format!("{cmd} — INERT (agents=[], every tool gated)")
        }
        // An unknown transport can't be diffed faithfully -> text-preview fallback.
        _ => return None,
    };
    Some(Plan {
        tool: "connector_add".to_string(),
        summary: format!("Add MCP connector '{name}' ({transport}) — added inert; no capability granted"),
        changes: vec![Change {
            resource: format!("config/darwin.toml [[mcp.servers]] '{name}'"),
            before: "(absent)".to_string(),
            after,
        }],
        state_hash: hash_state(&connector_snapshot(existing_names)),
    })
}

/// The relevant-state snapshot for a `connector_add`: the SORTED set of existing
/// server names. Sorting makes the hash order-independent (config re-ordering is
/// not a semantic drift); a NUL separator keeps names unambiguous.
fn connector_snapshot(existing_names: &[String]) -> String {
    let mut names: Vec<&str> = existing_names.iter().map(String::as_str).collect();
    names.sort_unstable();
    format!("mcp.servers\u{0}{}", names.join("\u{0}"))
}

/// PURE planner for `standing_create` given the goals of the CURRENT standing
/// missions. The relevant state is the set of existing mission goals (a mission
/// added/removed drifts the plan). Returns `None` for a missing goal.
pub fn plan_standing_create(input: &Value, existing_goals: &[String]) -> Option<Plan> {
    let goal = field(input, "goal");
    if goal.is_empty() {
        return None;
    }
    let schedule = field(input, "schedule");
    let on = if schedule.is_empty() {
        String::new()
    } else {
        format!(" on {schedule}")
    };
    Some(Plan {
        tool: "standing_create".to_string(),
        summary: format!("Establish a standing mission: {goal}"),
        changes: vec![Change {
            resource: "standing missions".to_string(),
            before: format!("{} mission(s)", existing_goals.len()),
            after: format!("+1 '{goal}'{on} — recurring autonomy (each run still parks)"),
        }],
        state_hash: hash_state(&standing_snapshot(existing_goals)),
    })
}

/// The relevant-state snapshot for a `standing_create`: the SORTED set of current
/// mission goals. Sorted (order-independent) + NUL-separated for unambiguity.
fn standing_snapshot(existing_goals: &[String]) -> String {
    let mut goals: Vec<&str> = existing_goals.iter().map(String::as_str).collect();
    goals.sort_unstable();
    format!("standing.missions\u{0}{}", goals.join("\u{0}"))
}

// ---------------------------------------------------------------------------
// State-bind — the TOCTOU decision (PURE)
// ---------------------------------------------------------------------------

/// The confirm-time state-bind decision. The ONLY positive outcome is
/// [`StateBind::Fresh`], and even that authorizes NOTHING by itself — it means
/// "the additional state precondition is satisfied; the caller STILL runs the
/// existing gates". There is deliberately no "execute regardless" variant, which
/// is why this layer can only ever ADD a failure (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateBind {
    /// The recomputed state hash MATCHES the parked plan's — the state is
    /// unchanged. The caller falls through to the UNCHANGED replay path (voice-id
    /// + allowlist + `gate(confirm)` still apply).
    Fresh,
    /// The state CHANGED since the plan was shown (or could not be re-read). The
    /// caller RE-PARKS this fresh plan and fires NOTHING. Fail-safe: an
    /// un-recomputable state is treated as drifted, never as a silent proceed.
    Drifted(Plan),
    /// The parked action had NO structured plan (a tool without a planner). The
    /// caller uses today's text preview path, byte-for-byte UNCHANGED.
    Unplanned,
}

/// PURE state-bind: compare the plan bound at PARK time against a freshly recomputed
/// plan (same tool + input, CURRENT state). A byte-identical `state_hash` is
/// [`StateBind::Fresh`]; any difference — OR an inability to recompute the fresh
/// plan (`fresh` is `None` while a plan WAS parked) — is [`StateBind::Drifted`]
/// (the fail-safe: never proceed on an unverifiable state). No parked plan is
/// [`StateBind::Unplanned`].
pub fn bind_state(parked: Option<&Plan>, fresh: Option<Plan>) -> StateBind {
    match (parked, fresh) {
        (None, _) => StateBind::Unplanned,
        (Some(p), Some(f)) if p.state_hash == f.state_hash => StateBind::Fresh,
        (Some(_), Some(f)) => StateBind::Drifted(f),
        // A plan was parked but the current state can't be re-read: fail SAFE by
        // treating it as drift (re-park), never as a silent proceed. Re-park the
        // ORIGINAL plan so the user still sees a faithful diff.
        (Some(p), None) => StateBind::Drifted(p.clone()),
    }
}

// ---------------------------------------------------------------------------
// Telemetry frame
// ---------------------------------------------------------------------------

/// Build the `plan.diff` telemetry frame the HUD's PLAN // DIFF panel renders.
/// SECRET-FREE by construction (the plan carries only human-readable summaries).
/// `phase` is "park" (a fresh plan just parked) or "confirm" (a drift re-park);
/// `drift` marks whether the state changed since the plan was first shown.
pub fn telemetry_frame(plan: &Plan, agent: &str, phase: &str, drift: bool) -> Value {
    json!({
        "tool": plan.tool,
        "agent": agent,
        "summary": plan.summary,
        "changes": plan.changes,
        "state_hash": plan.state_hash,
        "phase": phase,
        "drift": drift,
    })
}

// ---------------------------------------------------------------------------
// Impure entry — snapshot the live state, delegate to the pure planners
// ---------------------------------------------------------------------------

/// Build a plan for `tool`+`input` at PARK time by reading the CURRENT relevant
/// state. Returns `None` — falling back to the unchanged text preview — when:
///   * the `[plan]` section is OFF (armed-by-default, so this is opt-out);
///   * `tool` has no structured planner;
///   * the input is malformed for the planner, or the state can't be read.
///
/// This is the PARK-time entry: it honours the `[plan].enabled` gate (the opt-out).
/// The CONFIRM-time recompute uses [`recompute_plan`], which shares the same
/// builder but does NOT re-check the gate — once a plan was parked, its state is
/// verified regardless of a later toggle, so disabling `[plan]` mid-flight can
/// never wedge a parked action into an endless re-park (it either verifies and
/// proceeds, or fails safe on a genuinely-unreadable state).
pub async fn plan_for(tool: &str, input: &Value, memory: &Memory, cfg_path: &Path) -> Option<Plan> {
    let (cfg, _issues) = crate::config::Config::load(cfg_path);
    if !cfg.plan.enabled {
        return None;
    }
    build_plan(tool, input, memory, &cfg).await
}

/// Re-derive a plan for a tool+input at CONFIRM time, reading the CURRENT state.
/// Unlike [`plan_for`] this does NOT gate on `[plan].enabled` — a plan already
/// exists (it was parked), so the recompute's only job is to re-read the state and
/// let [`bind_state`] detect drift. Returns `None` only when the state genuinely
/// can't be read (which [`bind_state`] treats as drift, the fail-safe).
pub async fn recompute_plan(tool: &str, input: &Value, memory: &Memory, cfg_path: &Path) -> Option<Plan> {
    let (cfg, _issues) = crate::config::Config::load(cfg_path);
    build_plan(tool, input, memory, &cfg).await
}

/// The shared plan builder: snapshot the relevant live state for `tool` from the
/// already-loaded `cfg` (+ the mission store), and delegate to the pure per-tool
/// planner. Neither the park gate nor the confirm recompute duplicates the match.
async fn build_plan(tool: &str, input: &Value, memory: &Memory, cfg: &crate::config::Config) -> Option<Plan> {
    match tool {
        "connector_add" => {
            let names: Vec<String> = cfg.mcp.servers.iter().map(|s| s.name.clone()).collect();
            plan_connector_add(input, &names)
        }
        "standing_create" => {
            let goals: Vec<String> = crate::standing::list(memory)
                .await
                .ok()?
                .into_iter()
                .map(|m| m.goal)
                .collect();
            plan_standing_create(input, &goals)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- connector_add planner ------------------------------------------------

    #[test]
    fn plan_connector_add_http_before_after_are_correct() {
        let input = json!({
            "name": "files",
            "transport": "http",
            "url": "https://mcp.example.com/sse",
            "uses_token": true,
            "confirm": false,
        });
        let plan = plan_connector_add(&input, &["existing".into()]).expect("a plan");
        assert_eq!(plan.tool, "connector_add");
        assert!(plan.summary.contains("files"), "summary names the connector: {}", plan.summary);
        assert!(plan.summary.contains("inert"), "summary states the inert posture");
        assert_eq!(plan.changes.len(), 1);
        let c = &plan.changes[0];
        assert_eq!(c.resource, "config/darwin.toml [[mcp.servers]] 'files'");
        assert_eq!(c.before, "(absent)", "a new connector's BEFORE is absent");
        assert!(c.after.contains("https://mcp.example.com/sse"), "AFTER names the endpoint: {}", c.after);
        assert!(c.after.contains("INERT"), "AFTER states the inert posture: {}", c.after);
        assert!(!plan.state_hash.is_empty());
    }

    #[test]
    fn plan_connector_add_stdio_before_after_are_correct() {
        let input = json!({
            "name": "local-fs",
            "transport": "stdio",
            "command": "/usr/local/bin/mcp-fs",
            "args": ["--root", "/data"],
        });
        let plan = plan_connector_add(&input, &[]).expect("a plan");
        let c = &plan.changes[0];
        assert_eq!(c.before, "(absent)");
        assert!(c.after.contains("/usr/local/bin/mcp-fs"), "AFTER names the command: {}", c.after);
        assert!(c.after.contains("--root /data"), "AFTER names the args: {}", c.after);
    }

    #[test]
    fn plan_connector_add_rejects_malformed_input() {
        // No name -> no plan (text-preview fallback), never a half-formed diff.
        assert!(plan_connector_add(&json!({"transport": "http"}), &[]).is_none());
        // Unknown transport -> no plan.
        assert!(plan_connector_add(&json!({"name": "x", "transport": "ws"}), &[]).is_none());
    }

    // -- standing_create planner ----------------------------------------------

    #[test]
    fn plan_standing_create_before_after_are_correct() {
        let input = json!({"goal": "brief me on overnight PRs", "schedule": "daily at 8am"});
        let plan = plan_standing_create(&input, &["some existing goal".into()]).expect("a plan");
        assert_eq!(plan.tool, "standing_create");
        assert!(plan.summary.contains("brief me on overnight PRs"));
        let c = &plan.changes[0];
        assert_eq!(c.resource, "standing missions");
        assert_eq!(c.before, "1 mission(s)", "BEFORE reports the current mission count");
        assert!(c.after.contains("brief me on overnight PRs"), "AFTER names the goal: {}", c.after);
        assert!(c.after.contains("daily at 8am"), "AFTER names the schedule: {}", c.after);
    }

    #[test]
    fn plan_standing_create_rejects_missing_goal() {
        assert!(plan_standing_create(&json!({"schedule": "daily"}), &[]).is_none());
    }

    // -- state_hash: stable + state-bound -------------------------------------

    #[test]
    fn state_hash_is_stable_for_the_same_state() {
        let input = json!({"name": "files", "transport": "http", "url": "https://x/y"});
        let a = plan_connector_add(&input, &["one".into(), "two".into()]).unwrap();
        // Same set, different order -> same hash (order-independent).
        let b = plan_connector_add(&input, &["two".into(), "one".into()]).unwrap();
        assert_eq!(a.state_hash, b.state_hash, "the hash is order-independent over the server set");
    }

    #[test]
    fn state_hash_changes_when_the_state_changes() {
        let input = json!({"name": "files", "transport": "http", "url": "https://x/y"});
        let before = plan_connector_add(&input, &["one".into()]).unwrap();
        // A new server appears in the set -> the state hash must differ (drift).
        let after = plan_connector_add(&input, &["one".into(), "two".into()]).unwrap();
        assert_ne!(before.state_hash, after.state_hash, "a changed server set drifts the hash");
    }

    // -- bind_state: match -> Fresh; drift -> re-park; no-plan -> Unplanned ----

    fn sample_plan(hash: &str) -> Plan {
        Plan {
            tool: "connector_add".into(),
            summary: "Add MCP connector 'files' (http) — inert".into(),
            changes: vec![Change {
                resource: "config/darwin.toml [[mcp.servers]] 'files'".into(),
                before: "(absent)".into(),
                after: "https endpoint https://x/y — INERT".into(),
            }],
            state_hash: hash.into(),
        }
    }

    #[test]
    fn bind_matches_when_state_unchanged() {
        let parked = sample_plan("abc123");
        let fresh = sample_plan("abc123");
        assert_eq!(bind_state(Some(&parked), Some(fresh)), StateBind::Fresh);
    }

    #[test]
    fn bind_reparks_a_drifted_plan_with_the_fresh_diff() {
        let parked = sample_plan("abc123");
        let fresh = sample_plan("def456");
        match bind_state(Some(&parked), Some(fresh.clone())) {
            StateBind::Drifted(p) => assert_eq!(p, fresh, "re-park carries the FRESH plan"),
            other => panic!("expected Drifted, got {other:?}"),
        }
    }

    #[test]
    fn bind_fail_safe_reparks_when_state_cannot_be_read() {
        // A plan was parked but the current state can't be recomputed -> DRIFT,
        // never a silent proceed. Re-park the original plan so the user still sees
        // a faithful diff.
        let parked = sample_plan("abc123");
        match bind_state(Some(&parked), None) {
            StateBind::Drifted(p) => assert_eq!(p, parked, "fail-safe re-parks the original plan"),
            other => panic!("expected Drifted on an unreadable state, got {other:?}"),
        }
    }

    #[test]
    fn bind_unplanned_when_no_plan_was_parked() {
        // A tool without a structured planner -> today's text-preview path.
        assert_eq!(bind_state(None, None), StateBind::Unplanned);
        assert_eq!(bind_state(None, Some(sample_plan("x"))), StateBind::Unplanned);
    }

    /// THE STRICTNESS INVARIANT: the state-hash bind can ONLY add a failure — it
    /// never approves a drifted action and never yields a "just execute" verdict
    /// that could bypass voice-id / the master switch / the allowlist. We prove it
    /// structurally by enumerating every bind outcome and asserting NONE of them
    /// authorizes execution on its own:
    ///   * Fresh — the extra precondition PASSED, but it is only "fall through to
    ///     the UNCHANGED gates"; it carries no action and fires nothing.
    ///   * Drifted — re-park; fires nothing (a NEW failure mode vs. today).
    ///   * Unplanned — today's path, unchanged.
    ///
    /// A drifted state is ALWAYS Drifted regardless of how "yes" the confirm was.
    #[test]
    fn state_hash_gate_only_adds_a_failure() {
        // 1. A drift is ALWAYS a re-park (never an approve), whatever the plans say.
        for (parked_hash, fresh_hash) in [("a", "b"), ("same", "changed"), ("0", "1")] {
            let bind = bind_state(Some(&sample_plan(parked_hash)), Some(sample_plan(fresh_hash)));
            assert!(matches!(bind, StateBind::Drifted(_)), "a drift must never approve: {parked_hash}->{fresh_hash}");
        }
        // 2. An unreadable state is a re-park (fail-safe), never a proceed.
        assert!(matches!(bind_state(Some(&sample_plan("a")), None), StateBind::Drifted(_)));
        // 3. The ONLY non-refusing outcome is Fresh, and Fresh is NOT "execute":
        //    it is the caller's cue to run the UNCHANGED gates. There is no bind
        //    variant that says "execute regardless" — so this layer cannot make
        //    the gate looser, only add the drift/unreadable failure modes above.
        assert_eq!(bind_state(Some(&sample_plan("a")), Some(sample_plan("a"))), StateBind::Fresh);
        // Fresh is unit-only (no payload) — it can carry no execute authorization.
        let _: () = match bind_state(Some(&sample_plan("a")), Some(sample_plan("a"))) {
            StateBind::Fresh => (),
            _ => unreachable!(),
        };
    }

    // -- telemetry frame ------------------------------------------------------

    // -- the [plan].enabled gate applies at PARK, not at CONFIRM recompute ------

    /// `plan_for` (PARK) honours the `[plan].enabled` opt-out, but `recompute_plan`
    /// (CONFIRM) does NOT — so disabling `[plan]` WHILE an action is parked can never
    /// wedge that action into an endless re-park: the confirm recompute still reads
    /// the state and either verifies (proceed) or fails safe on a real drift.
    #[tokio::test]
    async fn recompute_ignores_the_enabled_gate_that_park_honours() {
        // A temp config with [plan] DISABLED and one existing MCP server.
        let dir = std::env::temp_dir().join(format!("plan_gate_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let cfg_path = dir.join("darwin.toml");
        std::fs::write(
            &cfg_path,
            "[plan]\nenabled = false\n\n[[mcp.servers]]\nname = \"existing\"\n",
        )
        .unwrap();
        let mem = crate::memory::Memory::open(&dir.join("mem.db")).unwrap();
        let input = json!({"name": "files", "transport": "http", "url": "https://x/y"});

        // PARK: the enabled gate is off -> no plan (text-preview fallback).
        assert!(
            plan_for("connector_add", &input, &mem, &cfg_path).await.is_none(),
            "plan_for honours [plan].enabled=false at park time"
        );
        // CONFIRM: the recompute ignores the gate -> a plan IS produced, so a
        // mid-flight disable verifies the state rather than re-parking forever.
        let re = recompute_plan("connector_add", &input, &mem, &cfg_path).await;
        assert!(re.is_some(), "recompute_plan ignores the enabled gate at confirm time");
        // And its state_hash reflects the ACTUAL current server set (["existing"]).
        assert_eq!(re.unwrap().state_hash, hash_state(&connector_snapshot(&["existing".into()])));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn telemetry_frame_carries_the_diff_and_phase() {
        let plan = sample_plan("abc123");
        let frame = telemetry_frame(&plan, "agent.pepper", "park", false);
        assert_eq!(frame["tool"], "connector_add");
        assert_eq!(frame["agent"], "agent.pepper");
        assert_eq!(frame["state_hash"], "abc123");
        assert_eq!(frame["phase"], "park");
        assert_eq!(frame["drift"], false);
        assert_eq!(frame["changes"][0]["before"], "(absent)");
        assert!(frame["summary"].as_str().unwrap().contains("files"));
        // A confirm-phase drift frame flips the marker.
        let drift = telemetry_frame(&plan, "agent.pepper", "confirm", true);
        assert_eq!(drift["phase"], "confirm");
        assert_eq!(drift["drift"], true);
    }
}
