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

use std::collections::BTreeMap;

use anyhow::{anyhow, Result};

use crate::optimize::{Outcome, Trace};

/// Upper bound on how many recent traces we pull + analyze (bounds cost + output).
const MAX_TRACES: usize = 5000;
/// Below this many GRADED turns, a capability is "insufficient data" — reported,
/// never judged.
const MIN_SAMPLE: usize = 5;
/// Rows rendered per section before an "and N more" elision.
const MAX_ROWS_SHOWN: usize = 12;

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
    let agents = tally(traces, |t| {
        let a = t.agent.trim();
        (!a.is_empty()).then(|| a.to_string())
    });
    let tools = tally(traces, |t| {
        let k = t.tool_or_skill.trim();
        (!k.is_empty()).then(|| k.to_string())
    });

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
}
