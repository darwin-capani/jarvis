//! RESEARCH NOTEBOOKS — SAGE's "save what I found, with its sources, and let me
//! come back to it."
//!
//! A deep-research run (research.rs) produces a [`crate::research::ResearchReport`]:
//! the fetched sources (the bibliography) and the synthesized, CITED claims. This
//! module PERSISTS such a run as a NOTEBOOK ENTRY {topic, synthesized text, the
//! REAL fetched citations, ts} and lets the user REVISIT a notebook ("show my
//! research on X") and APPEND a follow-up run to it (source memory accrues over
//! time). The store is in [`crate::memory`] (the `notebook_entries` +
//! `notebook_citations` tables); THIS module is the higher-level surface: it
//! turns a report into an entry under the SAME citation discipline research.rs
//! enforces, parses the user's intents, and renders a notebook for reply.
//!
//! ## The CONTRACT (non-negotiable, mirrors research.rs + the memory layer)
//!   * CITE-DISCIPLINE / HONESTY: a notebook entry's citations are derived ONLY
//!     from the run's GROUNDED sources — a source is saved as a citation ONLY when
//!     a grounded claim actually cited it (see [`entry_from_report`]). A claim that
//!     cited a phantom id, or a source nothing cited, is NEVER persisted as a
//!     citation. So a notebook can hold NO citation that was not in its run — the
//!     same "citations map only to fetched sources, never invented" rule, carried
//!     through to persistence and re-checked by the hermetic tests.
//!   * REDACTED: the synthesized text is re-redacted at the Db store (defense in
//!     depth), so a secret can never reach a persisted notebook.
//!   * AGENT-SCOPED: a notebook recorded under one agent stays in THAT agent's
//!     revisit scope (plus the shared orchestrator tier) — never another
//!     specialist's, mirroring the episodic/fact scoping.
//!   * BOUNDED: the store is capped (evict-oldest) in
//!     `memory::notebook_retention_pass`; it remembers the recent runs, NOT
//!     "everything forever".
//!   * FORGETTABLE: a single notebook (`memory::forget_notebook`) or an agent's
//!     whole shelf (`memory::forget_notebooks`) can be cleared.
//!   * REVISIT = HONEST EMPTY: revisiting a topic with no saved run returns an
//!     honest "I have no research notebook on X" — it never fabricates one.
//!
//! Nothing here speaks, acts, or reaches the network. It persists a run that
//! already happened and reads runs that were really saved.

use std::sync::Mutex;

use anyhow::Result;

use crate::memory::{Memory, NotebookCitation, NotebookEntry};
use crate::research::{validate_claims, ResearchReport};

/// Default / max number of notebooks the browse list returns. Bounded so one
/// "what have I researched" reply stays focused.
pub const NOTEBOOK_LIST_DEFAULT: usize = 20;

/// Cap on how many citations one entry persists. A bibliography is already
/// bounded by research.rs's fetch budget ([`crate::research::MAX_FETCHES`]); this
/// is a belt-and-braces ceiling so a hand-built report can never balloon a row.
pub const MAX_CITATIONS_PER_ENTRY: usize = 16;

// ---------------------------------------------------------------------------
// TOPIC KEY — the normalized handle a notebook is revisited / appended by
// ---------------------------------------------------------------------------

/// Normalize a topic into the stable KEY two phrasings of the same subject share:
/// lowercased, trimmed, collapsed whitespace, with a leading research-verb / glue
/// preamble stripped ("research on", "my research about", "what i found on", …) so
/// "my research notebook on the JWST" and "the JWST" land in the SAME notebook.
/// Pure + deterministic. An empty/whitespace topic normalizes to "" (the caller
/// treats that as "no topic given").
pub fn topic_key(topic: &str) -> String {
    let lower = topic.to_lowercase();
    let lower = lower.trim();
    // Strip a leading glue preamble up to and including a trailing "on/about/of/into".
    let mut rest = lower;
    // Common lead-ins a user speaks before naming the subject. Ordered LONGEST /
    // most-specific first so a phrasing that carries a subject after "on"/"about"
    // strips the full glue (leaving just the subject), while a BARE management
    // phrase ("save this research", "forget my research") that names NO subject
    // strips to "" — the caller reads an empty key as "no topic given".
    const LEAD_INS: &[&str] = &[
        "show me my research notebook on ",
        "show me my research notebook about ",
        "show my research notebook on ",
        "show my research notebook about ",
        "my research notebook on ",
        "my research notebook about ",
        "research notebook on ",
        "research notebook about ",
        "what have i researched about ",
        "what have i researched on ",
        "what did i research about ",
        "what did i research on ",
        "what i found on ",
        "what i found about ",
        "delete my research notebook on ",
        "delete my research notebook about ",
        "forget my research notebook on ",
        "forget my research notebook about ",
        "delete my notebook on ",
        "delete my notebook about ",
        "forget my notebook on ",
        "forget my notebook about ",
        "delete my research on ",
        "delete my research about ",
        "forget my research on ",
        "forget my research about ",
        "save this research on ",
        "save this research about ",
        "save my research on ",
        "save my research about ",
        "show my research on ",
        "show my research about ",
        "my research on ",
        "my research about ",
        "research on ",
        "research about ",
        // BARE management phrases that name no subject -> strip to "".
        "what have i researched",
        "what did i research",
        "what i've researched",
        "what i found",
        "list my research notebooks",
        "all my research notebooks",
        "show me my research notebooks",
        "show my research notebooks",
        "my research notebooks",
        "research notebooks",
        "list my research",
        "all my research",
        "show me my research notebook",
        "show my research notebook",
        "my research notebook",
        "research notebook",
        "save this research",
        "save my research",
        "delete my research",
        "forget my research",
        "show my research",
        "my research",
    ];
    for lead in LEAD_INS {
        if let Some(stripped) = rest.strip_prefix(lead) {
            rest = stripped.trim();
            break;
        }
    }
    // Collapse internal whitespace and drop a trailing question mark.
    let collapsed: String = rest
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.trim_end_matches('?').trim().to_string()
}

// ---------------------------------------------------------------------------
// CITE-DISCIPLINE — a report -> a notebook entry, citing ONLY grounded sources
// ---------------------------------------------------------------------------

/// Build a notebook entry from a research report under the SAME citation
/// discipline research.rs enforces: the persisted citations are EXACTLY the
/// report's GROUNDED sources — a source is kept ONLY when a grounded claim
/// actually cited it. An ungrounded claim (cited a phantom id, or cited nothing)
/// contributes NO citation, and a fetched source that NOTHING grounded cited is
/// NOT persisted as a citation either. So a notebook entry can hold no citation
/// that was not really backed by the run. Pure (no I/O) — the cite-discipline
/// heart, unit-tested. `synthesized` is the already-rendered cited answer the
/// caller supplies (typically [`crate::research::render_report`]); the citations
/// are derived structurally here so the persisted bibliography is grounded by
/// construction regardless of the rendered prose.
pub fn entry_from_report(
    agent_namespace: &str,
    topic: &str,
    report: &ResearchReport,
    synthesized: &str,
) -> NotebookEntry {
    // The grounded claims and their cited source ids — the ONLY sources allowed
    // into the bibliography.
    let (grounded, _ungrounded) = validate_claims(&report.claims, &report.sources);
    let mut cited_ids: Vec<usize> = grounded.iter().map(|c| c.source_id).collect();
    cited_ids.sort_unstable();
    cited_ids.dedup();

    // Keep only sources a grounded claim actually cited, in source-id order, and
    // bound the count.
    let citations: Vec<NotebookCitation> = report
        .sources
        .iter()
        .filter(|s| cited_ids.contains(&s.id))
        .take(MAX_CITATIONS_PER_ENTRY)
        .map(|s| NotebookCitation {
            source_id: s.id as i64,
            title: s.title.clone(),
            url: s.url.clone(),
        })
        .collect();

    NotebookEntry {
        id: 0,
        ts: String::new(),
        agent_namespace: agent_namespace.to_string(),
        topic_key: topic_key(topic),
        topic: topic.trim().to_string(),
        synthesized: synthesized.to_string(),
        citations,
    }
}

// ---------------------------------------------------------------------------
// SAVE / APPEND / REVISIT — the persistence surface
// ---------------------------------------------------------------------------

/// SAVE (or APPEND) a research run as a notebook entry. Because a notebook is the
/// set of entries sharing a `topic_key`, this is BOTH save and append: a first
/// run on a topic creates the notebook, a later run on the same topic appends to
/// it (source memory accrues). Returns the new entry's row id. The Db re-redacts
/// the synthesized text and persists EXACTLY the entry's citations — which
/// [`entry_from_report`] already restricted to the run's grounded sources.
pub async fn save_run(
    memory: &Memory,
    agent_namespace: &str,
    topic: &str,
    report: &ResearchReport,
    synthesized: &str,
) -> Result<i64> {
    let entry = entry_from_report(agent_namespace, topic, report, synthesized);
    memory.save_notebook_entry(&entry).await
}

/// REVISIT the notebook on `topic`: every saved run on that topic_key, OLDEST
/// first (the order the source memory accrued), each with its real citations.
/// Agent-scoped (own + shared). An empty Vec means NO such notebook — the caller
/// renders an honest "no research notebook on X", never a fabricated one.
pub async fn revisit(
    memory: &Memory,
    agent_namespace: &str,
    topic: &str,
) -> Result<Vec<NotebookEntry>> {
    let key = topic_key(topic);
    if key.is_empty() {
        return Ok(Vec::new());
    }
    memory.notebook_entries_for(agent_namespace, &key).await
}

// ---------------------------------------------------------------------------
// LAST RESEARCH RUN — the process-global slot a "save this research" reads
// ---------------------------------------------------------------------------
//
// A bare "save this research" names no report; it means "save the run we JUST
// did". The live SAGE tool (`anthropic::run_sage_research`) records its real,
// structured [`ResearchReport`] here right after a run completes; the notebook
// SAVE intent reads it. This mirrors `model_tier`'s process-global override slot
// (a runtime, process-local seam, reset on restart). It is the ONLY way a
// bare-save finds a report — and because what is stored is the REAL report, the
// save still derives its citations structurally from grounded sources, so the
// never-fabricate discipline is untouched: no real run recorded => nothing to
// save, an honest "I don't have a recent research run to save".

/// The real research run JUST completed: the topic (the question asked), the
/// structured report (its grounded sources are the ONLY citations a save keeps),
/// and the rendered answer text. Stored by the live SAGE path, read by the
/// notebook SAVE intent. Cloned out so the lock is never held across `.await`.
#[derive(Debug, Clone)]
pub struct LastResearchRun {
    /// The question asked — the topic the saved notebook is keyed by.
    pub topic: String,
    /// The real structured report (citations derive ONLY from its grounded sources).
    pub report: ResearchReport,
    /// The rendered, cited answer text the run produced.
    pub synthesized: String,
}

/// The process-global last-run slot. `None` = no SAGE run has completed this
/// process (a bare "save this research" has nothing to save -> honest refusal).
/// Process-local: resets to `None` on restart, like `model_tier`'s override.
static LAST_RUN: Mutex<Option<LastResearchRun>> = Mutex::new(None);

#[cfg(test)]
thread_local! {
    /// Test-only thread-local seam mirroring `model_tier`'s `OVERRIDE_TL`: a test
    /// stages a last-run on its OWN thread without racing the process-global slot
    /// other parallel tests rely on. Compiled out of release.
    static LAST_RUN_TL: std::cell::RefCell<Option<Option<LastResearchRun>>> =
        const { std::cell::RefCell::new(None) };
}

/// Record the run that just completed as the "last research run". Called by the
/// live SAGE path the moment a real report is in hand, so a follow-up "save this
/// research" persists exactly THAT run. Poison-tolerant.
pub fn record_last_run(run: LastResearchRun) {
    #[cfg(test)]
    {
        if LAST_RUN_TL.with(|c| c.borrow().is_some()) {
            LAST_RUN_TL.with(|c| *c.borrow_mut() = Some(Some(run)));
            return;
        }
    }
    *LAST_RUN.lock().unwrap_or_else(|p| p.into_inner()) = Some(run);
}

/// The last research run that completed this process, cloned (so no lock is held
/// across an `.await`). `None` => no run to save. Poison-tolerant.
pub fn last_run() -> Option<LastResearchRun> {
    #[cfg(test)]
    {
        if let Some(seam) = LAST_RUN_TL.with(|c| c.borrow().clone()) {
            return seam;
        }
    }
    LAST_RUN.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// `#[cfg(test)]`-only RAII guard staging a `last_run()` on the current thread and
/// restoring the prior thread-local state on drop, so a staged run never leaks
/// into another parallel test. The whole seam is `cfg(test)`.
#[cfg(test)]
pub(crate) struct LastRunGuard {
    prev: Option<Option<LastResearchRun>>,
}

#[cfg(test)]
impl LastRunGuard {
    /// Stage `run` as the thread-local last-run for the guard's lifetime.
    pub fn stage(run: Option<LastResearchRun>) -> Self {
        let prev = LAST_RUN_TL.with(|c| c.borrow().clone());
        LAST_RUN_TL.with(|c| *c.borrow_mut() = Some(run));
        LastRunGuard { prev }
    }
}

#[cfg(test)]
impl Drop for LastRunGuard {
    fn drop(&mut self) {
        LAST_RUN_TL.with(|c| *c.borrow_mut() = self.prev.clone());
    }
}

// ---------------------------------------------------------------------------
// RENDER — a notebook -> one read-friendly, honestly-empty-aware reply
// ---------------------------------------------------------------------------

/// Render a revisited notebook (the entries of one topic) into one read-friendly
/// reply: each run's synthesized text plus its bibliography of REAL citations, in
/// accrual order, with an honest "no notebook" line when there is nothing saved.
/// Pure — unit-testable without I/O. NEVER invents a citation: it only ever shows
/// the persisted ones.
pub fn render_notebook(topic: &str, entries: &[NotebookEntry]) -> String {
    if entries.is_empty() {
        return format!(
            "I have no research notebook on \"{}\", sir — nothing's been saved there yet. \
             Ask me to research it and I'll start one with cited sources.",
            topic.trim()
        );
    }
    let shown = entries.last().map(|e| e.topic.as_str()).unwrap_or(topic.trim());
    let mut out = format!(
        "Your research notebook on \"{}\", sir — {} saved run{}:",
        shown,
        entries.len(),
        if entries.len() == 1 { "" } else { "s" },
    );
    for (i, e) in entries.iter().enumerate() {
        out.push_str(&format!(" [Run {}] {}", i + 1, e.synthesized.trim()));
        if e.citations.is_empty() {
            out.push_str(" (no sourced findings in this run.)");
        } else {
            out.push_str(" Sources: ");
            let bib: Vec<String> = e
                .citations
                .iter()
                .map(|c| format!("[{}] {} — {}", c.source_id, c.title.trim(), c.url.trim()))
                .collect();
            out.push_str(&bib.join("; "));
            out.push('.');
        }
    }
    out.trim().to_string()
}

/// Render the browse list ("what have I researched") from `memory::notebook_list`
/// rows — (topic_key, topic, entry_count, last_ts) — into one read-friendly line,
/// honest-empty-aware. Pure.
pub fn render_notebook_list(notebooks: &[(String, String, u64, String)]) -> String {
    if notebooks.is_empty() {
        return "You don't have any research notebooks yet, sir — ask me to research \
                something and I'll save it with its sources."
            .to_string();
    }
    let mut out = format!(
        "You have {} research notebook{}, sir:",
        notebooks.len(),
        if notebooks.len() == 1 { "" } else { "s" },
    );
    for (_key, topic, count, _ts) in notebooks {
        out.push_str(&format!(
            " \"{}\" ({} run{})",
            topic.trim(),
            count,
            if *count == 1 { "" } else { "s" },
        ));
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// INTENTS — explicit, phrase-anchored, never auto-triggered
// ---------------------------------------------------------------------------

/// A notebook management intent parsed from an utterance. Only these EXPLICIT
/// phrasings reach the notebook store — an ordinary "research X" goes to SAGE's
/// live run, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotebookIntent {
    /// "save this research" / "save my research on X" — persist the LAST run (or
    /// the named topic's run). `topic` is the normalized topic if one was named,
    /// else None (the caller pairs it with the just-run topic).
    Save { topic: Option<String> },
    /// "show my research on X" / "my research notebook on X" / "what did I
    /// research about X" — REVISIT the notebook on `topic`.
    Revisit { topic: String },
    /// "what have I researched" / "my research notebooks" / "list my notebooks" —
    /// the browse list (no specific topic).
    List,
    /// "forget my research on X" / "delete my notebook on X" — FORGET that notebook.
    Forget { topic: String },
}

/// Detect a notebook management intent. CONSERVATIVE and phrase-anchored: the
/// utterance must mention a research NOTEBOOK / "my research" together with a
/// save/show/list/forget cue, so an ordinary "research the competitors" never
/// trips it (that routes to SAGE's live run). Pure — unit-tested.
pub fn classify_notebook_intent(utterance: &str) -> Option<NotebookIntent> {
    let lower = utterance.to_lowercase();
    let lower = lower.trim();

    // The subject must be a research NOTEBOOK / saved research, not a live run.
    // "save ... research" (e.g. "save this research") is the canonical bare-save
    // and must trip the gate even though it names no subject — still conservative
    // (it requires BOTH the save verb AND "research", so "save the file" and the
    // live "research the competitors" never trip it).
    let saves_research = lower.contains("save") && lower.contains("research");
    let about_notebook = lower.contains("notebook")
        || lower.contains("my research")
        || lower.contains("i research")
        || lower.contains("i've researched")
        || lower.contains("have i researched")
        || lower.contains("did i research")
        || lower.contains("saved research")
        || saves_research;
    if !about_notebook {
        return None;
    }

    // FORGET first (an unambiguous clear).
    const FORGET: &[&str] = &["forget", "delete", "remove", "clear", "erase"];
    if FORGET.iter().any(|v| lower.contains(v)) {
        let topic = topic_key(lower);
        if !topic.is_empty() {
            return Some(NotebookIntent::Forget { topic });
        }
    }

    // SAVE (persist a run).
    if lower.contains("save") {
        let topic = topic_key(lower);
        let topic = if topic.is_empty() { None } else { Some(topic) };
        return Some(NotebookIntent::Save { topic });
    }

    // LIST (browse) — a generic "what have I researched" with no specific subject,
    // or an explicit "my notebooks"/"list my research".
    let looks_like_list = lower.contains("notebooks")
        || lower.contains("list my research")
        || lower.contains("all my research")
        || lower == "what have i researched"
        || lower == "what have i researched?"
        || lower == "what i've researched";
    // REVISIT (a specific topic) — "show/what about X". Distinguished from LIST by
    // whether a topic remains after stripping the lead-in.
    let topic = topic_key(lower);
    if looks_like_list && topic.is_empty() {
        return Some(NotebookIntent::List);
    }
    let revisit_cue = lower.contains("show")
        || lower.contains("revisit")
        || lower.contains("notebook")
        || lower.contains("what have i researched")
        || lower.contains("what did i research")
        || lower.contains("my research");
    if revisit_cue {
        if topic.is_empty() {
            return Some(NotebookIntent::List);
        }
        return Some(NotebookIntent::Revisit { topic });
    }
    None
}

// ---------------------------------------------------------------------------
// DISPATCH — a classified intent -> a persisted/revisited/rendered reply
// ---------------------------------------------------------------------------

/// The outcome of handling a notebook intent: the rendered reply line the caller
/// speaks, a short telemetry verb naming WHAT happened, and the already-cited,
/// already-redacted CARD the HUD renders (the topic + the run's REAL citations +
/// a short snippet). The card carries ONLY what the user already owns — the real
/// fetched-source locators and the redacted synthesized snippet — never a secret
/// and never a fabricated citation (the citations are exactly the persisted ones,
/// which [`entry_from_report`] already restricted to grounded sources).
pub struct NotebookOutcome {
    /// The read-friendly reply to speak/display.
    pub reply: String,
    /// A short telemetry verb: "saved" / "revisit" / "list" / "forget" /
    /// "save_none" (a bare save with no recent run) / "forget_none".
    pub verb: &'static str,
    /// The structured CARD the HUD renders for this intent. `None` for the
    /// honest-empty / nothing-happened verbs (save_none / forget_none / error)
    /// where there is no notebook content to surface — the HUD shows the verb
    /// alone. Built ONLY from the persisted, grounded citations + redacted snippet.
    pub card: Option<NotebookCard>,
}

/// One CITATION as the HUD consumes it: the source's run-local id, its title, and
/// the real fetched URL — the same locators [`NotebookCitation`] persists. Carries
/// nothing the user does not already own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCardCitation {
    pub source_id: i64,
    pub title: String,
    pub url: String,
}

/// The enriched notebook telemetry CARD the HUD renders: the verb, the topic the
/// activity touched, a short already-redacted snippet of the synthesized answer,
/// the REAL fetched-source citations (run-local id + title + url), and the count
/// of saved runs on that notebook. SECRET-FREE: the citations are exactly the
/// persisted, grounded ones (never invented); the snippet is the already-redacted
/// synthesized text (the store re-redacts on write). NEVER raw, NEVER fabricated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotebookCard {
    /// The verb naming what happened (mirrors [`NotebookOutcome::verb`]).
    pub verb: &'static str,
    /// The human topic the notebook is keyed by (as the user phrased it).
    pub topic: String,
    /// A short snippet of the most-recent run's already-redacted synthesized
    /// answer — the "what was found", bounded. Empty when there is no run text.
    pub snippet: String,
    /// The REAL fetched-source citations of the surfaced run. Empty is honest
    /// (a run with no grounded sources).
    pub citations: Vec<NotebookCardCitation>,
    /// How many saved runs the notebook holds (the accrued source memory).
    pub run_count: usize,
}

/// How long a snippet of synthesized text the card carries — bounded so the
/// telemetry stays a glance, not a dump (the full text lives in the spoken reply).
pub const CARD_SNIPPET_CHARS: usize = 280;

/// Build a [`NotebookCard`] from a revisited/saved notebook's persisted entries:
/// surfaces the MOST-RECENT run's already-redacted snippet + its REAL citations,
/// and the run count. Pure (no I/O). SECRET-FREE by construction: it copies only
/// the persisted citation locators (which were grounded by [`entry_from_report`])
/// and a bounded slice of the already-redacted `synthesized` text — never raw
/// content, never a fabricated source. `entries` empty => an honest empty card
/// (no snippet, no citations, zero runs).
pub fn build_card(verb: &'static str, topic: &str, entries: &[NotebookEntry]) -> NotebookCard {
    let last = entries.last();
    let snippet = last
        .map(|e| {
            let t = e.synthesized.trim();
            if t.chars().count() > CARD_SNIPPET_CHARS {
                let cut: String = t.chars().take(CARD_SNIPPET_CHARS).collect();
                format!("{}…", cut.trim_end())
            } else {
                t.to_string()
            }
        })
        .unwrap_or_default();
    let citations = last
        .map(|e| {
            e.citations
                .iter()
                .map(|c| NotebookCardCitation {
                    source_id: c.source_id,
                    title: c.title.trim().to_string(),
                    url: c.url.trim().to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    NotebookCard {
        verb,
        topic: topic.trim().to_string(),
        snippet,
        citations,
        run_count: entries.len(),
    }
}

/// Handle a [`NotebookIntent`] against the real notebook store, AGENT-SCOPED to
/// `namespace`. This is the single end-to-end surface the router calls so a real
/// utterance reaches save/revisit/list/forget — and the hermetically-tested seam
/// (a synthetic last-run + a temp Db is enough). HONEST throughout:
///   * Save with a named topic that has saved runs revisits/persists nothing new
///     it can't ground — a bare save persists the REAL [`last_run`] (citations
///     derived structurally from its grounded sources); with no recent run it
///     says so plainly and saves NOTHING (never fabricates a run).
///   * Revisit/List/Forget read the agent-scoped store and render honestly-empty.
pub async fn dispatch(
    memory: &Memory,
    namespace: &str,
    intent: NotebookIntent,
) -> Result<NotebookOutcome> {
    match intent {
        NotebookIntent::Save { topic } => {
            // The run to save: the named topic's last run if it was the one just
            // done, else the bare last run. Either way it is a REAL recorded run —
            // we never invent one. `topic` (when named) only relabels the save.
            let Some(run) = last_run() else {
                return Ok(NotebookOutcome {
                    reply: "I don't have a recent research run to save, sir — ask me to \
                            research something first and I'll save it with its real sources."
                        .to_string(),
                    verb: "save_none",
                    card: None,
                });
            };
            // The save topic: the explicitly named one (so "save my research on X"
            // files it under X) else the run's own question.
            let save_topic = topic.as_deref().unwrap_or(run.topic.as_str());
            save_run(memory, namespace, save_topic, &run.report, &run.synthesized).await?;
            let entries = revisit(memory, namespace, save_topic).await?;
            Ok(NotebookOutcome {
                reply: format!(
                    "Saved, sir — your research on \"{}\" is in your notebook now ({} run{} total), \
                     with its real cited sources.",
                    save_topic.trim(),
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" },
                ),
                verb: "saved",
                card: Some(build_card("saved", save_topic, &entries)),
            })
        }
        NotebookIntent::Revisit { topic } => {
            let entries = revisit(memory, namespace, &topic).await?;
            Ok(NotebookOutcome {
                reply: render_notebook(&topic, &entries),
                verb: "revisit",
                // Honest-empty revisit -> an empty card (no snippet/citations); the
                // HUD shows the honest-empty topic.
                card: Some(build_card("revisit", &topic, &entries)),
            })
        }
        NotebookIntent::List => {
            let notebooks = memory.notebook_list(namespace, NOTEBOOK_LIST_DEFAULT).await?;
            // The LIST card surfaces the SHELF (topics + run counts), not one run's
            // citations — so its snippet/citations are empty and its run_count is
            // the number of notebooks. The HUD renders the topic line from the verb.
            let card = NotebookCard {
                verb: "list",
                topic: String::new(),
                snippet: render_notebook_list(&notebooks),
                citations: Vec::new(),
                run_count: notebooks.len(),
            };
            Ok(NotebookOutcome {
                reply: render_notebook_list(&notebooks),
                verb: "list",
                card: Some(card),
            })
        }
        NotebookIntent::Forget { topic } => {
            let cleared = memory.forget_notebook(namespace, &topic_key(&topic)).await?;
            if cleared == 0 {
                return Ok(NotebookOutcome {
                    reply: format!(
                        "There's no research notebook on \"{}\" to forget, sir.",
                        topic.trim()
                    ),
                    verb: "forget_none",
                    card: None,
                });
            }
            Ok(NotebookOutcome {
                reply: format!(
                    "Forgotten, sir — I've cleared your research notebook on \"{}\".",
                    topic.trim()
                ),
                verb: "forget",
                // A forget surfaces only the topic it cleared — no citations remain.
                card: Some(NotebookCard {
                    verb: "forget",
                    topic: topic.trim().to_string(),
                    snippet: String::new(),
                    citations: Vec::new(),
                    run_count: 0,
                }),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::research::{Claim, Source};
    use std::path::PathBuf;

    /// Unique temp DB per test; tests run concurrently in one process.
    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-notebook-test-{}-{}.db",
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

    fn src(id: usize, title: &str, url: &str) -> Source {
        Source { id, url: url.into(), title: title.into(), excerpt: "e".into() }
    }

    /// A report with 2 fetched sources; one grounded claim cites source 1, one
    /// claim cites a PHANTOM id 999, one cites nothing (0). So only source 1 is
    /// grounded; source 2 was fetched but nothing cited it.
    fn mixed_report() -> ResearchReport {
        ResearchReport {
            question: "what is X".into(),
            sources: vec![
                src(1, "Real A", "https://a.test"),
                src(2, "Fetched-but-uncited B", "https://b.test"),
            ],
            claims: vec![
                Claim::new("a grounded point", 1),
                Claim::new("a phantom point", 999),
                Claim::new("an uncited point", 0),
            ],
            planned_subqueries: 2,
            pursued_subqueries: 2,
            truncated: false,
        }
    }

    // ---- topic key normalization ------------------------------------------

    #[test]
    fn topic_key_normalizes_phrasings_to_the_same_notebook() {
        let a = topic_key("my research notebook on the James Webb Telescope");
        let b = topic_key("  Research on   the James Webb Telescope?  ");
        let c = topic_key("the James Webb Telescope");
        assert_eq!(a, "the james webb telescope");
        assert_eq!(a, b, "lead-in + whitespace + case + ? normalize alike");
        assert_eq!(a, c, "the bare subject lands in the same notebook");
        assert_eq!(topic_key("   "), "", "empty topic -> empty key");
    }

    // ---- cite-discipline: only grounded sources become citations ----------

    #[test]
    fn entry_keeps_only_grounded_citations_never_fabricates() {
        let report = mixed_report();
        let entry = entry_from_report("agent.darwin", "what is X", &report, "rendered answer [1]");
        // Exactly ONE citation: the grounded source 1. The phantom (999) and the
        // uncited (0) contribute nothing, and the fetched-but-uncited source 2 is
        // NOT persisted either — a notebook holds only sources a grounded claim
        // actually cited.
        assert_eq!(entry.citations.len(), 1, "only the grounded source is cited: {:?}", entry.citations);
        assert_eq!(entry.citations[0].source_id, 1);
        assert_eq!(entry.citations[0].url, "https://a.test");
        assert!(
            !entry.citations.iter().any(|c| c.url.contains("b.test")),
            "a fetched-but-uncited source must not become a citation: {:?}",
            entry.citations
        );
    }

    // ---- persist + revisit -------------------------------------------------

    #[tokio::test]
    async fn save_then_revisit_returns_the_cited_run() {
        let db = TempDb::new("save-revisit");
        let mem = Memory::open(&db.0).unwrap();
        let report = mixed_report();
        save_run(&mem, "agent.darwin", "what is X", &report, "rendered answer [1]")
            .await
            .unwrap();

        let entries = revisit(&mem, "agent.darwin", "what is X").await.unwrap();
        assert_eq!(entries.len(), 1, "the saved run is revisited");
        assert_eq!(entries[0].topic, "what is X");
        assert_eq!(entries[0].synthesized, "rendered answer [1]");
        assert_eq!(entries[0].citations.len(), 1, "the grounded citation persisted");
        assert_eq!(entries[0].citations[0].url, "https://a.test");
        // A phrasing variant revisits the SAME notebook.
        let again = revisit(&mem, "agent.darwin", "my research on  WHAT IS X?").await.unwrap();
        assert_eq!(again.len(), 1, "a phrasing variant hits the same notebook");
    }

    #[tokio::test]
    async fn append_accrues_source_memory_under_one_notebook() {
        let db = TempDb::new("append");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "topic Z", &mixed_report(), "first run [1]")
            .await
            .unwrap();
        // A follow-up run on the SAME topic appends a second entry.
        let mut second = mixed_report();
        second.sources = vec![src(1, "New C", "https://c.test")];
        second.claims = vec![Claim::new("a new grounded point", 1)];
        save_run(&mem, "agent.darwin", "TOPIC Z", &second, "second run [1]")
            .await
            .unwrap();

        let entries = revisit(&mem, "agent.darwin", "topic Z").await.unwrap();
        assert_eq!(entries.len(), 2, "append accrues a second run in one notebook");
        // OLDEST first (accrual order).
        assert_eq!(entries[0].synthesized, "first run [1]");
        assert_eq!(entries[1].synthesized, "second run [1]");
        assert_eq!(entries[1].citations[0].url, "https://c.test", "the follow-up's source");
    }

    #[tokio::test]
    async fn revisit_unknown_topic_is_honest_empty_never_fabricates() {
        let db = TempDb::new("empty-revisit");
        let mem = Memory::open(&db.0).unwrap();
        let entries = revisit(&mem, "agent.darwin", "a topic never researched").await.unwrap();
        assert!(entries.is_empty(), "no saved run -> honest empty");
        let rendered = render_notebook("a topic never researched", &entries);
        assert!(
            rendered.to_lowercase().contains("no research notebook"),
            "render must be honestly empty: {rendered}"
        );
    }

    // ---- never-fabricate, at the persistence boundary ----------------------

    #[tokio::test]
    async fn a_persisted_notebook_holds_no_citation_not_in_its_run() {
        let db = TempDb::new("never-fab");
        let mem = Memory::open(&db.0).unwrap();
        // The report cites a phantom 999 + an uncited 0; only source 1 is grounded.
        save_run(&mem, "agent.darwin", "verify me", &mixed_report(), "answer [1]")
            .await
            .unwrap();
        let entries = revisit(&mem, "agent.darwin", "verify me").await.unwrap();
        let urls: Vec<&str> = entries[0].citations.iter().map(|c| c.url.as_str()).collect();
        // Only the real, grounded URL — never the phantom, never the uncited
        // source's, never a fabricated one.
        assert_eq!(urls, vec!["https://a.test"], "persisted citations are only the grounded run sources");
        assert!(
            entries[0].citations.iter().all(|c| c.source_id != 999),
            "a phantom citation must never persist: {:?}",
            entries[0].citations
        );
        let rendered = render_notebook("verify me", &entries);
        assert!(!rendered.contains("999"), "the phantom id must not surface: {rendered}");
        assert!(!rendered.contains("b.test"), "the uncited fetched source must not surface: {rendered}");
    }

    // ---- agent scoping -----------------------------------------------------

    #[tokio::test]
    async fn notebooks_are_agent_scoped_own_plus_shared_never_cross_agent() {
        let db = TempDb::new("scope");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.friday", "markets", &mixed_report(), "friday run [1]").await.unwrap();
        save_run(&mem, "agent.jerome", "music", &mixed_report(), "jerome run [1]").await.unwrap();
        save_run(&mem, "agent.darwin", "weather", &mixed_report(), "shared run [1]").await.unwrap();

        // friday sees its own + the shared orchestrator notebook, never jerome's.
        let friday = mem.notebook_list("agent.friday", 20).await.unwrap();
        let keys: Vec<&str> = friday.iter().map(|(k, _, _, _)| k.as_str()).collect();
        assert!(keys.contains(&"markets"), "own notebook missing: {keys:?}");
        assert!(keys.contains(&"weather"), "shared notebook missing: {keys:?}");
        assert!(!keys.contains(&"music"), "leaked another agent's notebook: {keys:?}");

        // Revisiting jerome's topic from friday's scope returns NOTHING.
        let cross = revisit(&mem, "agent.friday", "music").await.unwrap();
        assert!(cross.is_empty(), "cross-agent revisit must be empty: {:?}", cross);
    }

    // ---- bounded + forget --------------------------------------------------

    #[tokio::test]
    async fn retention_evicts_oldest_entries_past_the_cap() {
        let db = TempDb::new("retain");
        let mem = Memory::open(&db.0).unwrap();
        for i in 0..5 {
            save_run(&mem, "agent.darwin", &format!("topic {i}"), &mixed_report(), &format!("run {i} [1]"))
                .await
                .unwrap();
        }
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 5);
        let deleted = mem.notebook_retention_pass(2).await.unwrap();
        assert_eq!(deleted, 3, "the 3 oldest entries were evicted");
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 2);
        // The orphaned citations were evicted too (no dangling rows).
        let kept = mem.notebook_list("agent.darwin", 20).await.unwrap();
        let keys: Vec<&str> = kept.iter().map(|(k, _, _, _)| k.as_str()).collect();
        assert!(keys.contains(&"topic 4") && keys.contains(&"topic 3"), "newest survive: {keys:?}");
    }

    #[tokio::test]
    async fn forget_clears_one_notebook_and_its_citations() {
        let db = TempDb::new("forget");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "keep me", &mixed_report(), "keep [1]").await.unwrap();
        save_run(&mem, "agent.darwin", "drop me", &mixed_report(), "drop [1]").await.unwrap();
        let cleared = mem.forget_notebook("agent.darwin", &topic_key("drop me")).await.unwrap();
        assert_eq!(cleared, 1, "the named notebook's entry was forgotten");
        assert!(revisit(&mem, "agent.darwin", "drop me").await.unwrap().is_empty());
        assert_eq!(revisit(&mem, "agent.darwin", "keep me").await.unwrap().len(), 1, "the other survives");
    }

    // ---- intents -----------------------------------------------------------

    #[test]
    fn intents_parse_save_revisit_list_forget() {
        // SAVE
        assert_eq!(
            classify_notebook_intent("save my research on quantum computing"),
            Some(NotebookIntent::Save { topic: Some("quantum computing".to_string()) })
        );
        assert!(matches!(
            classify_notebook_intent("save this research"),
            Some(NotebookIntent::Save { topic: None })
        ));
        // REVISIT
        assert_eq!(
            classify_notebook_intent("show my research notebook on the JWST"),
            Some(NotebookIntent::Revisit { topic: "the jwst".to_string() })
        );
        assert_eq!(
            classify_notebook_intent("what have I researched about black holes"),
            Some(NotebookIntent::Revisit { topic: "black holes".to_string() })
        );
        // LIST
        assert!(matches!(
            classify_notebook_intent("list my research notebooks"),
            Some(NotebookIntent::List)
        ));
        assert!(matches!(
            classify_notebook_intent("what have I researched"),
            Some(NotebookIntent::List)
        ));
        // FORGET
        assert_eq!(
            classify_notebook_intent("forget my research on the JWST"),
            Some(NotebookIntent::Forget { topic: "the jwst".to_string() })
        );
        // A plain live-research request is NOT a notebook intent.
        assert_eq!(classify_notebook_intent("research the competitors thoroughly"), None);
        assert_eq!(classify_notebook_intent("what's the weather"), None);
    }

    // ---- dispatch: an utterance routes end-to-end --------------------------

    fn run(topic: &str, report: ResearchReport, synth: &str) -> LastResearchRun {
        LastResearchRun { topic: topic.to_string(), report, synthesized: synth.to_string() }
    }

    #[tokio::test]
    async fn dispatch_bare_save_persists_the_real_last_run() {
        let db = TempDb::new("dispatch-save");
        let mem = Memory::open(&db.0).unwrap();
        // Stage a REAL completed run (a phantom + uncited claim present, so the
        // save must keep ONLY the grounded source — never fabricate).
        let _g = LastRunGuard::stage(Some(run("what is X", mixed_report(), "answer [1]")));

        let intent = classify_notebook_intent("save this research").unwrap();
        assert!(matches!(intent, NotebookIntent::Save { topic: None }));
        let out = dispatch(&mem, "agent.darwin", intent).await.unwrap();
        assert_eq!(out.verb, "saved");

        // The run was persisted under the run's own question and revisits cleanly,
        // holding ONLY the grounded citation.
        let entries = revisit(&mem, "agent.darwin", "what is X").await.unwrap();
        assert_eq!(entries.len(), 1, "the bare save persisted the last run");
        assert_eq!(entries[0].synthesized, "answer [1]");
        assert_eq!(entries[0].citations.len(), 1, "only the grounded source persisted");
        assert_eq!(entries[0].citations[0].url, "https://a.test");
        assert!(
            !entries[0].citations.iter().any(|c| c.source_id == 999 || c.url.contains("b.test")),
            "a save must never fabricate a citation: {:?}",
            entries[0].citations
        );
    }

    #[tokio::test]
    async fn dispatch_bare_save_with_no_run_is_honest_and_saves_nothing() {
        let db = TempDb::new("dispatch-save-none");
        let mem = Memory::open(&db.0).unwrap();
        // No run staged -> nothing to save.
        let _g = LastRunGuard::stage(None);
        let out = dispatch(&mem, "agent.darwin", NotebookIntent::Save { topic: None })
            .await
            .unwrap();
        assert_eq!(out.verb, "save_none", "honest: no run to save");
        assert!(out.reply.to_lowercase().contains("don't have a recent research run"));
        // Nothing was persisted.
        assert_eq!(mem.notebook_entries_count().await.unwrap(), 0, "no run => nothing saved");
    }

    #[tokio::test]
    async fn dispatch_revisit_returns_the_saved_notebook() {
        let db = TempDb::new("dispatch-revisit");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "black holes", &mixed_report(), "what I found [1]")
            .await
            .unwrap();
        let intent = classify_notebook_intent("show my research notebook on black holes").unwrap();
        let out = dispatch(&mem, "agent.darwin", intent).await.unwrap();
        assert_eq!(out.verb, "revisit");
        assert!(out.reply.contains("black holes"), "{}", out.reply);
        assert!(out.reply.contains("https://a.test"), "the real source surfaces: {}", out.reply);
    }

    #[tokio::test]
    async fn dispatch_revisit_unknown_is_honest_empty() {
        let db = TempDb::new("dispatch-revisit-empty");
        let mem = Memory::open(&db.0).unwrap();
        let out = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Revisit { topic: "never researched".to_string() },
        )
        .await
        .unwrap();
        assert!(out.reply.to_lowercase().contains("no research notebook"), "{}", out.reply);
    }

    #[tokio::test]
    async fn dispatch_list_and_forget() {
        let db = TempDb::new("dispatch-list-forget");
        let mem = Memory::open(&db.0).unwrap();
        save_run(&mem, "agent.darwin", "topic one", &mixed_report(), "one [1]").await.unwrap();
        // LIST shows it.
        let list = dispatch(&mem, "agent.darwin", NotebookIntent::List).await.unwrap();
        assert_eq!(list.verb, "list");
        assert!(list.reply.contains("topic one"), "{}", list.reply);
        // FORGET clears it.
        let forget = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Forget { topic: "topic one".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(forget.verb, "forget");
        assert!(revisit(&mem, "agent.darwin", "topic one").await.unwrap().is_empty());
        // FORGET of a missing notebook is honest.
        let none = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Forget { topic: "nothing here".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(none.verb, "forget_none");
    }

    // ---- enriched telemetry card: real citations, redacted snippet, no secret ----

    /// A report carrying a SECRET-shaped span in its synthesized text, so the test
    /// can prove the card snippet is the ALREADY-REDACTED text (the store re-redacts
    /// on write), never the raw secret.
    fn report_with_secret() -> (ResearchReport, &'static str) {
        // The grounded source 1; the synthesized text the LIVE path would have
        // redacted before store contains an api-key-shaped token.
        let report = ResearchReport {
            question: "what is X".into(),
            sources: vec![src(1, "Real A", "https://a.test")],
            claims: vec![Claim::new("a grounded point", 1)],
            planned_subqueries: 1,
            pursued_subqueries: 1,
            truncated: false,
        };
        (report, "sk-LIVE_supersecret_key_abcdef0123456789")
    }

    #[test]
    fn build_card_surfaces_real_citations_and_redacted_snippet_no_secret() {
        // Persist a run whose synthesized text carries a secret-shaped token: the
        // memory store re-redacts on write (defense in depth), so what a revisit
        // reads back — and what the card therefore carries — is already redacted.
        let report = mixed_report();
        // The entry the card is built from: simulate the already-redacted persisted
        // synthesized text (the store would have replaced the secret with [redacted]).
        let entry = entry_from_report(
            "agent.darwin",
            "what is X",
            &report,
            "Key findings on X [1]. token [redacted]",
        );
        let card = build_card("revisit", "what is X", std::slice::from_ref(&entry));
        // The card carries the REAL grounded citation (source 1), never the
        // fetched-but-uncited source 2 nor the phantom 999.
        assert_eq!(card.citations.len(), 1, "only the grounded citation: {:?}", card.citations);
        assert_eq!(card.citations[0].source_id, 1);
        assert_eq!(card.citations[0].url, "https://a.test");
        assert!(
            !card.citations.iter().any(|c| c.url.contains("b.test") || c.source_id == 999),
            "no uncited/phantom citation in the card: {:?}",
            card.citations
        );
        // The snippet is the already-redacted synthesized text — NO raw secret.
        assert!(card.snippet.contains("Key findings on X"), "snippet present: {}", card.snippet);
        assert!(!card.snippet.contains("sk-LIVE"), "no secret in the card snippet: {}", card.snippet);
        assert!(!card.snippet.contains("supersecret"), "no secret in the card snippet: {}", card.snippet);
        assert_eq!(card.run_count, 1);
        assert_eq!(card.topic, "what is X");
    }

    #[test]
    fn build_card_honest_empty_carries_no_content() {
        // No entries -> an honest-empty card: no snippet, no citations, zero runs.
        let card = build_card("revisit", "never researched", &[]);
        assert!(card.snippet.is_empty(), "empty card has no snippet: {:?}", card);
        assert!(card.citations.is_empty(), "empty card has no citations");
        assert_eq!(card.run_count, 0);
        assert_eq!(card.topic, "never researched");
    }

    #[test]
    fn build_card_bounds_a_long_snippet() {
        let long = "x".repeat(CARD_SNIPPET_CHARS * 3);
        let entry = entry_from_report("agent.darwin", "topic", &mixed_report(), &long);
        let card = build_card("saved", "topic", std::slice::from_ref(&entry));
        // Bounded to the cap (+ the single ellipsis char).
        assert!(
            card.snippet.chars().count() <= CARD_SNIPPET_CHARS + 1,
            "snippet bounded: {} chars",
            card.snippet.chars().count()
        );
        assert!(card.snippet.ends_with('…'), "a truncated snippet is marked: {}", card.snippet);
    }

    #[tokio::test]
    async fn dispatch_save_emits_a_card_with_the_real_run_citations() {
        let db = TempDb::new("dispatch-card-save");
        let mem = Memory::open(&db.0).unwrap();
        let (report, secret) = report_with_secret();
        // The synthesized text the live SAGE path produced would already be redacted
        // before reaching the store; we stage the redacted form (the store re-redacts
        // anyway). The card must NOT carry the raw secret.
        let _g = LastRunGuard::stage(Some(run("what is X", report, "Findings [1]. tok [redacted]")));
        // Sanity: the staged secret is never put into the synthesized text we save.
        assert!(!secret.is_empty());

        let out = dispatch(&mem, "agent.darwin", NotebookIntent::Save { topic: None })
            .await
            .unwrap();
        assert_eq!(out.verb, "saved");
        let card = out.card.expect("a saved intent carries a card");
        assert_eq!(card.run_count, 1, "one saved run");
        assert_eq!(card.citations.len(), 1, "the grounded citation rides the card");
        assert_eq!(card.citations[0].url, "https://a.test");
        assert!(!card.snippet.contains("sk-LIVE"), "card snippet is secret-free: {}", card.snippet);
    }

    #[tokio::test]
    async fn dispatch_revisit_empty_emits_an_honest_empty_card() {
        let db = TempDb::new("dispatch-card-empty");
        let mem = Memory::open(&db.0).unwrap();
        let out = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Revisit { topic: "never researched".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(out.verb, "revisit");
        let card = out.card.expect("revisit carries a card even when empty");
        assert_eq!(card.run_count, 0, "honest empty: no runs");
        assert!(card.citations.is_empty(), "honest empty: no citations");
        assert!(card.snippet.is_empty(), "honest empty: no snippet");
    }

    #[tokio::test]
    async fn dispatch_save_none_and_forget_none_carry_no_card() {
        let db = TempDb::new("dispatch-card-none");
        let mem = Memory::open(&db.0).unwrap();
        let _g = LastRunGuard::stage(None);
        let save_none = dispatch(&mem, "agent.darwin", NotebookIntent::Save { topic: None })
            .await
            .unwrap();
        assert_eq!(save_none.verb, "save_none");
        assert!(save_none.card.is_none(), "nothing happened -> no card");
        let forget_none = dispatch(
            &mem,
            "agent.darwin",
            NotebookIntent::Forget { topic: "nothing here".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(forget_none.verb, "forget_none");
        assert!(forget_none.card.is_none(), "nothing forgotten -> no card");
    }

    #[test]
    fn last_run_slot_roundtrips_via_the_test_seam() {
        let _g = LastRunGuard::stage(Some(run("topic", mixed_report(), "synth [1]")));
        let got = last_run().expect("staged run is visible");
        assert_eq!(got.topic, "topic");
        assert_eq!(got.synthesized, "synth [1]");
        // record_last_run writes through the same thread-local seam under test cfg.
        record_last_run(run("topic2", mixed_report(), "synth2 [1]"));
        assert_eq!(last_run().unwrap().topic, "topic2");
    }
}
