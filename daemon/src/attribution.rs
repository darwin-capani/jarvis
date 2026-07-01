//! Capability Attribution — a READ-ONLY analysis over the optimizer trace corpus:
//! which AGENTS and which TOOLS/SKILLS are load-bearing, mixed, failing, or
//! low-sample, computed from the outcome-labelled turns the daemon already
//! records. It answers "which of my capabilities actually work?" from evidence,
//! so roster / skill decisions rest on data rather than vibes.
//!
//! HONEST STATISTICS (the whole point):
//!   * A capability below MIN_SAMPLE graded turns is reported "insufficient data"
//!     — never ranked good or bad on one or two samples.
//!   * `unknown` outcomes are EXCLUDED from the success rate (but counted), so an
//!     unlabelled turn never masquerades as a success or a failure.
//!   * `corrected_next_turn` and `failed` both count AGAINST success (a turn the
//!     user had to correct did not work).
//!   * A small overall corpus is called out so no rate is over-read.
//!
//! It CHANGES NOTHING — a read-only pass over recent traces (reusing the same
//! process-global trace store as `oracle_ask`). Distinct from `oracle_ask` (raw
//! SQL the model writes): this is a curated, opinionated report with built-in
//! sample-size honesty. The router-side per-step attribution enrichment
//! (was_delegated / contributed_to_success) and eval-gated AUTO-promotion of
//! proven skills are documented follow-ons; this v1 analyzes what already exists.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde_json::json;

use crate::optimize::{Outcome, Trace};

/// Upper bound on how many recent traces we pull + analyze (bounds cost + output).
const MAX_TRACES: usize = 5000;
/// Below this many GRADED turns, a capability is "insufficient data" — reported,
/// never judged.
const MIN_SAMPLE: usize = 5;
/// Rows rendered per section before an "and N more" elision.
const MAX_ROWS_SHOWN: usize = 12;
/// Rate at or above which a well-sampled capability is "reliable".
const RELIABLE_RATE: f64 = 0.8;
/// Rate below which a well-sampled capability is "failing" (the propose-only flag).
const FAILING_RATE: f64 = 0.5;
/// Rate at or above which an eval-verified skill is a PROMOTION candidate — it
/// must be excellent in live use, not merely reliable, to earn first-class status.
const PROMOTE_RATE: f64 = 0.9;

/// Ambient health cadence (runtime-only; never run in tests). A generous startup
/// delay keeps it out of the first exchanges; a slow tick since the trace corpus
/// only grows as the user interacts.
const HEALTH_STARTUP_DELAY: Duration = Duration::from_secs(45);
const HEALTH_INTERVAL: Duration = Duration::from_secs(600);

/// Outcome tallies for one agent or one tool/skill.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Stat {
    successes: usize,
    corrected: usize,
    failed: usize,
    unknown: usize,
}

impl Stat {
    /// Turns with a definite signal (success/corrected/failed) — the rate's base.
    fn graded(&self) -> usize {
        self.successes + self.corrected + self.failed
    }

    fn total(&self) -> usize {
        self.graded() + self.unknown
    }

    /// Success fraction of the GRADED turns, or None when nothing is graded yet.
    fn rate(&self) -> Option<f64> {
        let g = self.graded();
        (g > 0).then(|| self.successes as f64 / g as f64)
    }

    fn add(&mut self, o: &Outcome) {
        match o {
            Outcome::Success => self.successes += 1,
            Outcome::CorrectedNextTurn => self.corrected += 1,
            Outcome::Failed => self.failed += 1,
            Outcome::Unknown => self.unknown += 1,
        }
    }

    /// Honest classification: below MIN_SAMPLE graded turns is NEVER judged.
    fn flag(&self) -> &'static str {
        if self.graded() < MIN_SAMPLE {
            return "insufficient data";
        }
        match self.rate() {
            Some(r) if r >= 0.8 => "reliable",
            Some(r) if r >= 0.5 => "mixed",
            _ => "failing",
        }
    }
}

/// The agent that handled a trace, or None when unattributed. Shared by the
/// on-demand report and the ambient health pass.
fn agent_key(t: &Trace) -> Option<String> {
    let a = t.agent.trim();
    (!a.is_empty()).then(|| a.to_string())
}

/// The tool/skill a trace invoked, or None when the turn used neither.
fn tool_key(t: &Trace) -> Option<String> {
    let k = t.tool_or_skill.trim();
    (!k.is_empty()).then(|| k.to_string())
}

/// Group traces by a key (agent, or tool/skill), tallying outcomes. Rows with no
/// key (an empty agent / no tool) are skipped. Sorted by graded turns desc, then
/// total desc, then name — deterministic. Pure.
fn tally<F: Fn(&Trace) -> Option<String>>(traces: &[Trace], key: F) -> Vec<(String, Stat)> {
    let mut map: BTreeMap<String, Stat> = BTreeMap::new();
    for t in traces {
        if let Some(k) = key(t) {
            map.entry(k).or_default().add(&t.outcome);
        }
    }
    let mut rows: Vec<(String, Stat)> = map.into_iter().collect();
    rows.sort_by(|a, b| {
        b.1.graded()
            .cmp(&a.1.graded())
            .then(b.1.total().cmp(&a.1.total()))
            .then(a.0.cmp(&b.0))
    });
    rows
}

/// Render one section (agents or tools) as an evidence table. Pure.
fn format_section(title: &str, rows: &[(String, Stat)]) -> String {
    if rows.is_empty() {
        return format!("{title}\n  (no data)\n");
    }
    let mut out = format!("{title}\n");
    for (name, s) in rows.iter().take(MAX_ROWS_SHOWN) {
        let rate = match s.rate() {
            Some(r) => format!("{:.0}% success", r * 100.0),
            None => "unrated".to_string(),
        };
        out.push_str(&format!(
            "  {name} — {} turns, {rate} [{}]\n",
            s.graded(),
            s.flag()
        ));
    }
    if rows.len() > MAX_ROWS_SHOWN {
        out.push_str(&format!("  … and {} more\n", rows.len() - MAX_ROWS_SHOWN));
    }
    out
}

/// The full report over a slice of traces. Pure (no I/O), so it is unit-tested
/// against known-outcome corpora.
fn analyze(traces: &[Trace]) -> String {
    if traces.is_empty() {
        return "No traces recorded yet — I have no evidence to attribute capability performance. \
                Use JARVIS for a while and this fills in."
            .to_string();
    }
    let agents = tally(traces, agent_key);
    let tools = tally(traces, tool_key);

    let mut out = format!("Capability attribution over the last {} turns.\n", traces.len());
    if traces.len() < MIN_SAMPLE * 2 {
        out.push_str("(Small corpus — treat every rate as provisional.)\n");
    }
    out.push('\n');
    out.push_str(&format_section("AGENTS (by turns handled):", &agents));
    out.push('\n');
    out.push_str(&format_section("TOOLS / SKILLS (by uses):", &tools));
    out.push_str(
        "\n(Read-only analysis of recorded outcomes — I changed nothing. corrected/failed count \
         against success; `unknown` turns are excluded from the rate; low-sample items are marked, \
         not judged.)",
    );
    out
}

/// The `capability_report` tool: pull the recent trace corpus (read-only, via the
/// process-global store) and analyze it. Honest empty message when there is no
/// store or no data; never fabricates.
pub async fn report() -> Result<String> {
    let store = crate::optimize::global_trace_store()
        .ok_or_else(|| anyhow!("capability_report: the trace store is not available"))?;
    let traces = store.recent(MAX_TRACES).await?;
    Ok(analyze(&traces))
}

// ---------------------------------------------------------------------------
// Ambient health surfacing (PROPOSE-ONLY) — a periodic pass that flags the
// FAILING capabilities so the user notices "skill X keeps failing" without
// asking. It emits telemetry ONLY (attribution.health); it never disables,
// promotes, or reroutes anything (that stays a human decision / a documented
// follow-on). The pure `health` core is unit-tested; the loop is runtime-only.
// ---------------------------------------------------------------------------

/// One well-sampled capability that is currently FAILING (the propose-only flag).
#[derive(Debug, Clone, PartialEq, Eq)]
struct CapFlag {
    /// "agent" or "tool".
    kind: &'static str,
    name: String,
    turns: usize,
    /// Success rate as a whole-number percent.
    rate_pct: u32,
}

/// A snapshot of capability health across the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Health {
    turns: usize,
    /// Distinct well-sampled capabilities that are reliable / failing.
    reliable: usize,
    failing: usize,
    /// The failing ones, detailed (the propose-only "needs attention" list).
    flags: Vec<CapFlag>,
    /// Eval-verified skills that are also live-proven — PROMOTION candidates
    /// (the "ready to elevate" half of the compound-growth loop).
    promote: Vec<CapFlag>,
}

/// Compute capability health across agents + tools, plus promotion candidates.
/// Only WELL-SAMPLED capabilities (>= MIN_SAMPLE graded turns) are classified —
/// the same sample-size honesty as the report. `eval_skills` is the set of skill
/// names that declared eval vectors (proven-correct at registry build); a skill
/// in that set with a >= PROMOTE_RATE live record is a promotion candidate. Pure,
/// so the flag + promote policy is unit-tested.
fn health(traces: &[Trace], eval_skills: &HashSet<String>) -> Health {
    let mut reliable = 0;
    let mut failing = 0;
    let mut flags = Vec::new();
    let agents = tally(traces, agent_key);
    let tools = tally(traces, tool_key);
    for (kind, rows) in [("agent", &agents), ("tool", &tools)] {
        for (name, s) in rows {
            if s.graded() < MIN_SAMPLE {
                continue; // never judged on a thin sample
            }
            let Some(r) = s.rate() else { continue };
            if r >= RELIABLE_RATE {
                reliable += 1;
            } else if r < FAILING_RATE {
                failing += 1;
                flags.push(CapFlag {
                    kind,
                    name: name.clone(),
                    turns: s.graded(),
                    rate_pct: (r * 100.0).round() as u32,
                });
            }
        }
    }
    Health {
        turns: traces.len(),
        reliable,
        failing,
        flags,
        promote: promotion_candidates(traces, eval_skills),
    }
}

/// Eval-verified skills that are ALSO live-proven (>= PROMOTE_RATE over >=
/// MIN_SAMPLE graded turns) — the promotion candidates. Only skills whose names
/// are in `eval_skills` (declared eval vectors) qualify: correctness is proven at
/// registry build, and this adds the live-performance evidence. Pure.
fn promotion_candidates(traces: &[Trace], eval_skills: &HashSet<String>) -> Vec<CapFlag> {
    tally(traces, tool_key)
        .into_iter()
        .filter_map(|(name, s)| {
            if !eval_skills.contains(&name) || s.graded() < MIN_SAMPLE {
                return None;
            }
            s.rate().filter(|r| *r >= PROMOTE_RATE).map(|r| CapFlag {
                kind: "skill",
                name,
                turns: s.graded(),
                rate_pct: (r * 100.0).round() as u32,
            })
        })
        .collect()
}

/// The set of skill names that declared eval vectors (proven-correct at registry
/// build). Impure (reads the process-global registry); kept out of the pure
/// analysis so the policy stays unit-testable with a synthetic set.
fn eval_verified_skill_names() -> HashSet<String> {
    crate::skills::global()
        .all()
        .iter()
        .filter(|s| !s.eval_vectors.is_empty())
        .map(|s| s.name.to_string())
        .collect()
}

/// Render the promotion-candidate report (the `promotion_candidates` tool). Honest
/// when nothing qualifies — it says exactly what a candidate needs.
fn format_promotions(cands: &[CapFlag], eval_total: usize) -> String {
    if eval_total == 0 {
        return "No skills declare eval vectors yet, so none can be promotion-graded.".to_string();
    }
    if cands.is_empty() {
        return format!(
            "No skill has earned promotion yet. A candidate must be eval-verified AND have >= {} \
             graded turns at >= {:.0}% success. ({eval_total} skills are eval-verified.)",
            MIN_SAMPLE,
            PROMOTE_RATE * 100.0
        );
    }
    let mut out = format!(
        "Promotion candidates — eval-verified skills that are ALSO live-proven (>= {:.0}% over >= {} \
         turns):\n",
        PROMOTE_RATE * 100.0,
        MIN_SAMPLE
    );
    for c in cands {
        out.push_str(&format!("  {} — {} turns, {}% success\n", c.name, c.turns, c.rate_pct));
    }
    out.push_str(
        "\n(PROPOSE-ONLY: these have earned first-class treatment — promote them yourself; I change \
         nothing.)",
    );
    out
}

/// The `promotion_candidates` tool: cross-reference the eval-verified skills with
/// the live trace corpus and report which have earned promotion. READ-ONLY.
pub async fn promotion_report() -> Result<String> {
    let store = crate::optimize::global_trace_store()
        .ok_or_else(|| anyhow!("promotion_candidates: the trace store is not available"))?;
    let traces = store.recent(MAX_TRACES).await?;
    let eval = eval_verified_skill_names();
    let cands = promotion_candidates(&traces, &eval);
    Ok(format_promotions(&cands, eval.len()))
}

/// One health tick: read the recent corpus (read-only) and emit an
/// `attribution.health` telemetry snapshot for the HUD. PROPOSE-ONLY: it changes
/// nothing. No store / read error is fatal — it simply skips this tick.
pub async fn health_tick() {
    let Some(store) = crate::optimize::global_trace_store() else {
        return;
    };
    let traces = match store.recent(MAX_TRACES).await {
        Ok(t) => t,
        Err(_) => return,
    };
    let h = health(&traces, &eval_verified_skill_names());
    let to_json = |v: &[CapFlag]| -> Vec<_> {
        v.iter()
            .map(|f| json!({"kind": f.kind, "name": f.name, "turns": f.turns, "rate": f.rate_pct}))
            .collect()
    };
    crate::telemetry::emit(
        "system",
        "attribution.health",
        json!({
            "turns": h.turns,
            "reliable": h.reliable,
            "failing": h.failing,
            "flags": to_json(&h.flags),
            "promote": to_json(&h.promote),
        }),
    );
}

/// The ambient capability-health loop (runtime-only; never run in tests). Mirrors
/// the other slow housekeeping loops: a startup delay, then a periodic tick.
pub async fn health_task() {
    tokio::time::sleep(HEALTH_STARTUP_DELAY).await;
    loop {
        health_tick().await;
        tokio::time::sleep(HEALTH_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a trace with a given (agent, tool, outcome); other fields are inert.
    fn t(agent: &str, tool: &str, outcome: Outcome) -> Trace {
        Trace::new("hello", "action", agent, "standard", tool, outcome, 100, 0)
    }

    #[test]
    fn stat_rate_and_flag_are_honest_about_sample_size() {
        // 4 graded turns (< MIN_SAMPLE) — never judged, even if all succeed.
        let mut low = Stat::default();
        for _ in 0..4 {
            low.add(&Outcome::Success);
        }
        assert_eq!(low.flag(), "insufficient data");

        // 5 success, 0 fail -> reliable.
        let mut good = Stat::default();
        for _ in 0..5 {
            good.add(&Outcome::Success);
        }
        assert_eq!(good.flag(), "reliable");
        assert_eq!(good.rate(), Some(1.0));

        // 3 success / 3 failed -> 50% -> mixed.
        let mut mixed = Stat::default();
        for _ in 0..3 {
            mixed.add(&Outcome::Success);
        }
        for _ in 0..3 {
            mixed.add(&Outcome::Failed);
        }
        assert_eq!(mixed.flag(), "mixed");

        // 1 success / 5 failed -> failing.
        let mut bad = Stat::default();
        bad.add(&Outcome::Success);
        for _ in 0..5 {
            bad.add(&Outcome::Failed);
        }
        assert_eq!(bad.flag(), "failing");
    }

    #[test]
    fn unknown_outcomes_are_excluded_from_the_rate() {
        let mut s = Stat::default();
        s.add(&Outcome::Success);
        s.add(&Outcome::Success);
        s.add(&Outcome::Unknown);
        // rate is 2/2 (the unknown is not in the denominator), but total is 3.
        assert_eq!(s.rate(), Some(1.0));
        assert_eq!(s.graded(), 2);
        assert_eq!(s.total(), 3);
    }

    #[test]
    fn tally_groups_sorts_and_skips_empty_keys() {
        let traces = vec![
            t("pepper", "gcal_create", Outcome::Success),
            t("pepper", "gcal_create", Outcome::Success),
            t("pepper", "", Outcome::Failed), // no tool -> skipped in tool tally
            t("jarvis", "shell_run", Outcome::Failed),
            t("", "orphan", Outcome::Success), // no agent -> skipped in agent tally
        ];
        let agents = tally(&traces, |x| {
            let a = x.agent.trim();
            (!a.is_empty()).then(|| a.to_string())
        });
        // pepper (3 graded) sorts before jarvis (1); the empty-agent row is skipped.
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].0, "pepper");
        assert_eq!(agents[0].1.graded(), 3);
        assert_eq!(agents[1].0, "jarvis");

        let tools = tally(&traces, |x| {
            let k = x.tool_or_skill.trim();
            (!k.is_empty()).then(|| k.to_string())
        });
        // gcal_create (2), shell_run (1), orphan (1) — the empty-tool row skipped.
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].0, "gcal_create");
    }

    #[test]
    fn analyze_reports_rates_flags_and_honesty_notes() {
        let mut traces = Vec::new();
        // pepper: 5 success + 1 failed -> 83% reliable.
        for _ in 0..5 {
            traces.push(t("pepper", "gcal_create", Outcome::Success));
        }
        traces.push(t("pepper", "gcal_create", Outcome::Failed));
        // jarvis: 2 failed only -> insufficient data (2 < MIN_SAMPLE).
        traces.push(t("jarvis", "shell_run", Outcome::Failed));
        traces.push(t("jarvis", "shell_run", Outcome::Failed));

        let out = analyze(&traces);
        assert!(out.contains("pepper — 6 turns, 83% success [reliable]"), "got:\n{out}");
        assert!(out.contains("jarvis — 2 turns, 0% success [insufficient data]"), "got:\n{out}");
        assert!(out.contains("gcal_create — 6 turns, 83% success [reliable]"), "got:\n{out}");
        assert!(out.contains("changed nothing"));
    }

    #[test]
    fn analyze_handles_empty_corpus_honestly() {
        let out = analyze(&[]);
        assert!(out.contains("No traces recorded yet"));
        assert!(!out.contains("%"), "must not print a fabricated rate");
    }

    #[test]
    fn health_flags_only_well_sampled_failing_capabilities() {
        let mut traces = Vec::new();
        // pepper (agent): 5 success + 1 failed -> 83% -> reliable.
        for _ in 0..5 {
            traces.push(t("pepper", "gcal_create", Outcome::Success));
        }
        traces.push(t("pepper", "gcal_create", Outcome::Failed));
        // karen (agent): 1 success + 5 failed -> 17% -> FAILING (well-sampled).
        traces.push(t("karen", "gmail_send", Outcome::Success));
        for _ in 0..5 {
            traces.push(t("karen", "gmail_send", Outcome::Failed));
        }
        // jarvis (agent): 2 failed only -> below MIN_SAMPLE -> NOT judged.
        traces.push(t("jarvis", "shell_run", Outcome::Failed));
        traces.push(t("jarvis", "shell_run", Outcome::Failed));

        let h = health(&traces, &HashSet::new());
        assert_eq!(h.turns, traces.len());
        // gcal_create (tool) is also reliable; pepper (agent) reliable -> 2 reliable.
        assert_eq!(h.reliable, 2, "pepper + gcal_create");
        // karen (agent) + gmail_send (tool) both failing -> 2 failing.
        assert_eq!(h.failing, 2);
        // The low-sample jarvis/shell_run is neither reliable nor failing nor flagged.
        assert!(h.flags.iter().all(|f| f.name != "jarvis" && f.name != "shell_run"));
        // A failing flag carries an honest rate + count.
        let karen = h.flags.iter().find(|f| f.name == "karen").expect("karen flagged");
        assert_eq!(karen.kind, "agent");
        assert_eq!(karen.turns, 6);
        assert_eq!(karen.rate_pct, 17);
        // No eval-verified skills passed -> no promotion candidates.
        assert!(h.promote.is_empty());
    }

    #[test]
    fn health_of_empty_corpus_is_all_zero_no_flags() {
        let h = health(&[], &HashSet::new());
        assert_eq!(
            h,
            Health { turns: 0, reliable: 0, failing: 0, flags: vec![], promote: vec![] }
        );
    }

    #[test]
    fn promotion_needs_eval_verification_and_excellent_live_record() {
        let mut traces = Vec::new();
        // base64_encode: eval-verified skill, 9 success + 1 corrected -> 90% -> candidate.
        for _ in 0..9 {
            traces.push(t("jarvis", "base64_encode", Outcome::Success));
        }
        traces.push(t("jarvis", "base64_encode", Outcome::CorrectedNextTurn));
        // rot13: eval-verified but only 60% -> reliable-ish, NOT promotion-grade.
        for _ in 0..3 {
            traces.push(t("jarvis", "rot13", Outcome::Success));
        }
        for _ in 0..2 {
            traces.push(t("jarvis", "rot13", Outcome::Failed));
        }
        // word_count: excellent (100%) but NOT eval-verified -> excluded.
        for _ in 0..6 {
            traces.push(t("jarvis", "word_count", Outcome::Success));
        }
        let eval: HashSet<String> =
            ["base64_encode".to_string(), "rot13".to_string()].into_iter().collect();

        let cands = promotion_candidates(&traces, &eval);
        assert_eq!(cands.len(), 1, "only base64_encode qualifies");
        assert_eq!(cands[0].name, "base64_encode");
        assert_eq!(cands[0].kind, "skill");
        assert_eq!(cands[0].turns, 10);
        assert_eq!(cands[0].rate_pct, 90);
    }

    #[test]
    fn format_promotions_is_honest_when_empty() {
        assert!(format_promotions(&[], 0).contains("No skills declare eval vectors"));
        assert!(format_promotions(&[], 5).contains("No skill has earned promotion"));
        let cand = vec![CapFlag { kind: "skill", name: "base64_encode".into(), turns: 10, rate_pct: 95 }];
        let out = format_promotions(&cand, 5);
        assert!(out.contains("base64_encode — 10 turns, 95% success"));
        assert!(out.contains("PROPOSE-ONLY"));
    }
}
