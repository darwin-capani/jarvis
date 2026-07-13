//! The deep-research engine: SAGE's "look into X thoroughly, with sources."
//!
//! A DEEP-RESEARCH run takes a question, plans a bounded set of sub-queries,
//! runs a SEARCH provider for each, FETCHes the top-k results, then SYNTHESIZEs
//! a CITED answer — every claim tagged with the source it came from. The whole
//! thing is bounded and safe by construction, mirroring the mission engine
//! (mission.rs): a pure, deterministic, injectable core with a runtime-gated
//! live wiring on top.
//!
//! - **Every provider is injected.** [`Searcher`], [`Fetcher`], and the
//!   synthesis [`Brain`] are all traits. The real implementations hit the web
//!   and the cloud; tests drive MOCKS that return canned results, so NO test
//!   ever makes a real web or cloud call. The plan, the search results, the
//!   fetched documents, and the synthesized answer are all unit-testable with no
//!   I/O.
//! - **Bounded (unit-tested).** At most [`MAX_SUBQUERIES`] sub-queries, at most
//!   [`MAX_FETCHES`] total document fetches across the whole run, and a
//!   per-sub-query top-k cap. Exceeding any bound TRUNCATES with a clear signal
//!   in the report ([`ResearchReport::truncated`] / the truncation note) — never
//!   a silent drop, never an unbounded crawl.
//! - **Citations map to fetched sources — never invented.** The synthesis brain
//!   is handed the actual fetched [`Source`]s (each with a stable numeric id and
//!   its real URL) and returns [`Claim`]s that each reference a source id. A
//!   claim that cites an id NOT in the fetched set, or cites nothing, is FLAGGED
//!   ([`Claim::is_grounded`] / [`validate_claims`]) rather than presented as
//!   fact — SAGE shows its sources, and a citation that maps to no fetched
//!   source is a defect we surface, not a URL we fabricate.
//! - **Honest about cost.** A real run needs the web AND the cloud and spends
//!   tokens (the synthesis is a cloud call). Offline / providers-unavailable,
//!   [`run_research`] degrades to a friendly "deep research needs the web + the
//!   cloud" line and does NOT pretend it ran.
//!
//! Honesty, as everywhere in the constellation: SAGE is a profile on ONE engine,
//! not a separate research mind; the synthesis is only as good as the sources it
//! actually fetched; and it never reports a source it did not retrieve or a
//! citation it cannot back.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

/// Hard cap on the number of sub-queries one research run plans. A question that
/// decomposes into more is TRUNCATED to this many (the rest reported as not
/// pursued) — deep research is a bounded investigation, never an open-ended
/// crawl.
pub const MAX_SUBQUERIES: usize = 5;

/// Per-sub-query cap on how many search hits are fetched. Keeps each sub-query's
/// fan-out bounded independent of how many results the search provider returns.
pub const MAX_RESULTS_PER_QUERY: usize = 3;

/// Whole-run cap on the TOTAL number of document fetches, across all
/// sub-queries. The binding budget on outward work: even at the max sub-query
/// count and the max per-query top-k, no run fetches more than this many
/// documents. Exceeding it stops fetching and is signalled as truncation.
pub const MAX_FETCHES: usize = 8;

/// The default and maximum research "depth" the tool exposes: depth scales the
/// number of sub-queries planned, clamped to [`MAX_SUBQUERIES`]. Depth 1 is a
/// quick pass; the max is the full bounded investigation. Named so the tool and
/// the persona can speak of "depth" honestly without it ever meaning "unbounded."
pub const DEFAULT_DEPTH: usize = 3;

// ---------------------------------------------------------------------------
// Data shapes (the pure, unit-tested vocabulary)
// ---------------------------------------------------------------------------

/// One planned sub-query: a focused search string the question was decomposed
/// into. The planner proposes these; [`bound_subqueries`] clamps the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubQuery {
    /// The search string handed to the [`Searcher`].
    pub query: String,
}

impl SubQuery {
    /// Build a sub-query from any string-like.
    pub fn new(query: impl Into<String>) -> Self {
        SubQuery { query: query.into() }
    }
}

/// One search result the [`Searcher`] returned for a sub-query: a title and the
/// URL to fetch. Not yet fetched — this is the candidate list the run draws from
/// under the fetch budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// Human-readable title of the result.
    pub title: String,
    /// The URL the [`Fetcher`] would retrieve.
    pub url: String,
}

impl SearchResult {
    /// Build a search result.
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Self {
        SearchResult { title: title.into(), url: url.into() }
    }
}

/// One FETCHED source: a stable numeric id (assigned in fetch order, the id the
/// synthesis cites), the URL it came from, its title, and the retrieved text
/// excerpt the brain reasons over. A `Source` exists ONLY because it was
/// actually fetched — so a citation that maps to a `Source` id is, by
/// construction, backed by a real retrieval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Stable 1-based id assigned in fetch order — what a [`Claim`] cites.
    pub id: usize,
    /// The URL this source was fetched from.
    pub url: String,
    /// The source's title.
    pub title: String,
    /// The fetched text excerpt the synthesis reasons over.
    pub excerpt: String,
}

/// One synthesized claim: a sentence of the answer plus the id of the source it
/// is drawn from. `source_id == 0` means the brain attached NO citation — which
/// [`validate_claims`] flags as ungrounded rather than letting it pass as fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The claim text.
    pub text: String,
    /// The cited source id (1-based). 0 = uncited (flagged ungrounded).
    pub source_id: usize,
}

impl Claim {
    /// Build a cited claim.
    pub fn new(text: impl Into<String>, source_id: usize) -> Self {
        Claim { text: text.into(), source_id }
    }

    /// True when this claim cites a source present in `sources` (a real, fetched
    /// citation). A claim citing id 0 (uncited) or an id not in the fetched set
    /// is NOT grounded — SAGE flags it rather than presenting it as backed.
    pub fn is_grounded(&self, sources: &[Source]) -> bool {
        self.source_id != 0 && sources.iter().any(|s| s.id == self.source_id)
    }
}

/// The full result of a research run: the question, the fetched sources (the
/// bibliography), the synthesized claims, and the bound bookkeeping so the report
/// can disclose truncation honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchReport {
    /// The original question.
    pub question: String,
    /// Every source actually fetched, in fetch order (ids are 1-based indices).
    pub sources: Vec<Source>,
    /// The synthesized claims, each tagged with its cited source id.
    pub claims: Vec<Claim>,
    /// How many sub-queries the planner proposed BEFORE truncation.
    pub planned_subqueries: usize,
    /// How many sub-queries were actually pursued (<= planned, <= the cap).
    pub pursued_subqueries: usize,
    /// True when a bound (sub-query cap OR the fetch budget) truncated the run.
    pub truncated: bool,
}

impl ResearchReport {
    /// The claims that are NOT backed by a fetched source (uncited, or citing an
    /// id absent from `sources`). Empty in a clean run; non-empty ones are
    /// flagged in the synthesis rather than presented as fact. The live render
    /// path uses [`validate_claims`] directly; this accessor is the ergonomic
    /// report-level view (exercised by the unit tests) for any caller that wants
    /// the flagged set without re-validating.
    #[allow(dead_code)]
    pub fn ungrounded_claims(&self) -> Vec<&Claim> {
        self.claims.iter().filter(|c| !c.is_grounded(&self.sources)).collect()
    }
}

// ---------------------------------------------------------------------------
// Injected seams (so every test is hermetic — no real web/cloud, ever)
// ---------------------------------------------------------------------------

/// A `Send` future returned by the trait methods. Spelled out explicitly so the
/// traits stay object-safe (`&dyn Searcher` / `&dyn Fetcher` / `&dyn Brain`)
/// WITHOUT the async-trait crate — the "no new dependencies" rule applies, and
/// this mirrors `mission::PlanFuture` / `heal::BrainFuture` exactly.
type PlanFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<SubQuery>>> + Send + 'a>>;
type SearchFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<SearchResult>>> + Send + 'a>>;
type FetchFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
type SynthFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Claim>>> + Send + 'a>>;

/// Decomposes a question into focused sub-queries. The real implementation asks
/// the cloud brain (a planning prompt); tests inject a mock returning a fixed
/// plan. May return MORE than the bounds allow — [`bound_subqueries`] clamps it;
/// the planner is not trusted to self-limit.
pub trait Planner: Send + Sync {
    /// Turn `question` into an ordered list of sub-queries, given the desired
    /// `depth` (a hint; the result is still clamped downstream).
    fn plan<'a>(&'a self, question: &'a str, depth: usize) -> PlanFuture<'a>;
}

/// Runs ONE search for a sub-query, returning candidate results. The real
/// implementation hits a web search provider; tests inject a mock returning
/// canned hits, so no test makes a network call.
pub trait Searcher: Send + Sync {
    /// Search the web for `query`, returning candidate results (title + URL).
    fn search<'a>(&'a self, query: &'a str) -> SearchFuture<'a>;
}

/// Fetches the text of ONE URL. The real implementation retrieves the page;
/// tests inject a mock returning canned text, so no test makes a network call.
pub trait Fetcher: Send + Sync {
    /// Retrieve `url`, returning a text excerpt of its content.
    fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a>;
}

/// Synthesizes a CITED answer from the fetched sources. The real implementation
/// calls the cloud brain with the sources and a citation-discipline prompt;
/// tests inject a mock returning fixed, cited claims — so no test makes a cloud
/// call. The brain is handed the actual [`Source`]s (ids + URLs + excerpts) and
/// must cite only those ids; [`validate_claims`] verifies it did.
pub trait Brain: Send + Sync {
    /// Synthesize claims for `question` over the fetched `sources`. Each returned
    /// [`Claim`] should cite a source id present in `sources`; one that does not
    /// is flagged downstream rather than trusted.
    fn synthesize<'a>(&'a self, question: &'a str, sources: &'a [Source]) -> SynthFuture<'a>;
}

// ---------------------------------------------------------------------------
// Pure core (unit-tested without any I/O)
// ---------------------------------------------------------------------------

/// The effective sub-query cap for a requested `depth`: depth maps 1:1 to a
/// sub-query count, clamped to at least 1 and at most [`MAX_SUBQUERIES`]. So a
/// caller can ask for a shallow or deep pass, but never an unbounded one. Pure.
pub fn subquery_cap_for_depth(depth: usize) -> usize {
    depth.clamp(1, MAX_SUBQUERIES)
}

/// Clamp a raw sub-query plan to the bounds for `depth`: drop blank queries,
/// then take at most `subquery_cap_for_depth(depth)`. Returns the bounded list
/// plus the ORIGINAL non-blank count, so the caller can tell whether truncation
/// occurred. Pure — testable with no planner and no I/O.
pub fn bound_subqueries(raw: Vec<SubQuery>, depth: usize) -> (Vec<SubQuery>, usize) {
    let cap = subquery_cap_for_depth(depth);
    let cleaned: Vec<SubQuery> = raw
        .into_iter()
        .filter(|q| !q.query.trim().is_empty())
        .collect();
    let original = cleaned.len();
    let bounded: Vec<SubQuery> = cleaned.into_iter().take(cap).collect();
    (bounded, original)
}

/// Validate synthesized claims against the fetched sources: partition into
/// (grounded, ungrounded). A grounded claim cites a source id actually present
/// in `sources`; an ungrounded one cites nothing (id 0) or an id not in the
/// fetched set. Pure — the citation-discipline heart, testable with no I/O.
/// SAGE NEVER promotes an ungrounded claim to a backed fact; the caller surfaces
/// the split honestly.
pub fn validate_claims<'a>(claims: &'a [Claim], sources: &[Source]) -> (Vec<&'a Claim>, Vec<&'a Claim>) {
    let mut grounded = Vec::new();
    let mut ungrounded = Vec::new();
    for c in claims {
        if c.is_grounded(sources) {
            grounded.push(c);
        } else {
            ungrounded.push(c);
        }
    }
    (grounded, ungrounded)
}

// ---------------------------------------------------------------------------
// Provenance ledger payload (F4) — the wire view of the grounding split
// ---------------------------------------------------------------------------

/// Bounds for the `research.provenance` payload — the same bounded-snippet
/// discipline every HUD payload follows (answer.annotated snippets are 200
/// chars; notebook cards 280).
const PROVENANCE_QUESTION_CHARS: usize = 160;
const PROVENANCE_CLAIM_CHARS: usize = 200;
const PROVENANCE_TITLE_CHARS: usize = 80;
/// URLs are bounded like every other field — a scraped href can be
/// arbitrarily long (multi-KB tracking parameters), and redaction alone never
/// truncates.
const PROVENANCE_URL_CHARS: usize = 200;
/// Claim rows per payload. Ungrounded rows are placed FIRST so the signal —
/// what was set aside — is never truncated away by grounded volume.
const PROVENANCE_CLAIMS_CAP: usize = 12;

/// Redact-then-bound one text for the wire: redaction first (so a truncation
/// can never split a token the redactor would have caught), then a char bound
/// with an ellipsis.
fn wire_text(s: &str, cap: usize) -> String {
    let redacted = crate::optimize::redact(s);
    if redacted.chars().count() <= cap {
        redacted
    } else {
        let mut out: String = redacted.chars().take(cap).collect();
        out.push('…');
        out
    }
}

/// Build the SECRET-FREE `research.provenance` payload for one completed
/// research run: the full grounded/ungrounded COUNTS (never truncated) plus
/// bounded per-claim rows mapping each claim to the source that backs it —
/// or flagging it as set aside. WIRE CONTRACT (mirrored by
/// hud/src/core/events.ts::parseResearchProvenance; exact field names pinned
/// by tests on both sides):
///
///   { "question", "sources_fetched", "claims_total", "claims_grounded",
///     "claims_ungrounded", "claims_omitted", "truncated",
///     "claims": [ { "text", "grounded", "source_id", "source_title",
///                   "source_url" } ] }
///
/// HONESTY NOTES: grounded claim text is already user-visible verbatim in the
/// rendered answer; ungrounded text is today reduced to a bare count in prose
/// ("I set aside N points") — this payload is deliberately the one place the
/// SET-ASIDE claims are visible, because "what was withheld and why" is
/// exactly what a provenance ledger owes the user. All text is redacted
/// (optimize::redact) then bounded; URLs pass through the same redactor (a
/// credentialed URL is masked). PURE — testable with no I/O.
pub fn provenance_payload(report: &ResearchReport) -> serde_json::Value {
    let (grounded, ungrounded) = validate_claims(&report.claims, &report.sources);

    let row = |c: &Claim, is_grounded: bool| -> serde_json::Value {
        let source = report.sources.iter().find(|s| s.id == c.source_id);
        serde_json::json!({
            "text": wire_text(&c.text, PROVENANCE_CLAIM_CHARS),
            "grounded": is_grounded,
            "source_id": c.source_id,
            "source_title": source.map(|s| wire_text(&s.title, PROVENANCE_TITLE_CHARS)).unwrap_or_default(),
            "source_url": source.map(|s| wire_text(&s.url, PROVENANCE_URL_CHARS)).unwrap_or_default(),
        })
    };

    // Ungrounded first — the set-aside claims are the signal and survive the
    // cap ahead of any grounded volume; grounded rows fill the remainder. A
    // run with MORE set-aside claims than the cap itself (a fully-uncited
    // model reply) still drops rows — `claims_omitted` DISCLOSES exactly how
    // many, so the ledger never presents a capped row list as complete.
    let claims: Vec<serde_json::Value> = ungrounded
        .iter()
        .map(|c| row(c, false))
        .chain(grounded.iter().map(|c| row(c, true)))
        .take(PROVENANCE_CLAIMS_CAP)
        .collect();
    let claims_omitted = report.claims.len().saturating_sub(claims.len());

    serde_json::json!({
        "question": wire_text(&report.question, PROVENANCE_QUESTION_CHARS),
        "sources_fetched": report.sources.len(),
        "claims_total": report.claims.len(),
        "claims_grounded": grounded.len(),
        "claims_ungrounded": ungrounded.len(),
        "claims_omitted": claims_omitted,
        "truncated": report.truncated,
        "claims": claims,
    })
}

/// Render a research report into one spoken-/read-friendly CITED answer. Each
/// grounded claim is rendered with a bracketed citation marker `[n]` and the
/// bibliography lists each source `[n] title — url`. Ungrounded claims are NOT
/// presented as fact: they are surfaced under an explicit "couldn't source"
/// note (or dropped from the answer body), so a reader never mistakes an uncited
/// assertion for a sourced one. Truncation is disclosed. With NO grounded claims
/// at all it says so plainly rather than fabricating an answer. Pure — the
/// rendering is unit-testable without any I/O.
pub fn render_report(report: &ResearchReport) -> String {
    let (grounded, ungrounded) = validate_claims(&report.claims, &report.sources);

    if grounded.is_empty() {
        // Nothing we can stand behind. Be honest rather than synthesize an
        // uncited answer.
        let mut out = format!(
            "I couldn't put together a sourced answer on \"{}\", sir — ",
            report.question.trim()
        );
        if report.sources.is_empty() {
            out.push_str("I retrieved no usable sources.");
        } else {
            out.push_str("nothing I drafted mapped cleanly to what I actually fetched, so I won't present it as fact.");
        }
        return out;
    }

    let mut out = format!("On \"{}\", sir, here's what the sources say. ", report.question.trim());
    for c in &grounded {
        out.push_str(&format!("{} [{}] ", c.text.trim(), c.source_id));
    }

    // Surface ungrounded drafts honestly — flagged, never presented as backed.
    if !ungrounded.is_empty() {
        out.push_str(&format!(
            "I set aside {} point{} I couldn't tie to a fetched source rather than state {} unsourced. ",
            ungrounded.len(),
            if ungrounded.len() == 1 { "" } else { "s" },
            if ungrounded.len() == 1 { "it" } else { "them" },
        ));
    }

    // The bibliography: every citation maps to a real fetched source.
    out.push_str("Sources: ");
    let bib: Vec<String> = report
        .sources
        .iter()
        .map(|s| format!("[{}] {} — {}", s.id, s.title.trim(), s.url.trim()))
        .collect();
    out.push_str(&bib.join("; "));
    out.push('.');

    if report.truncated {
        out.push_str(&format!(
            " I kept this bounded: I pursued {} of the {} angles I considered and capped fetches at {}.",
            report.pursued_subqueries, report.planned_subqueries, MAX_FETCHES,
        ));
    }
    out.trim().to_string()
}

/// The honest offline/unavailable degrade line: a real run needs the web AND the
/// cloud and spends tokens, so when either is unreachable SAGE says so plainly
/// rather than pretending. Pure.
pub fn offline_degrade(question: &str) -> String {
    format!(
        "Deep research needs the web and the cloud, sir — I can't run a sourced investigation offline, \
         and it isn't free (the synthesis spends tokens). Reconnect and I'll look into \"{}\" properly.",
        question.trim()
    )
}

// ---------------------------------------------------------------------------
// The orchestration (thin glue over the pure core + the injected seams)
// ---------------------------------------------------------------------------

/// Run a full deep-research pass for `question` at `depth`: plan sub-queries ->
/// bound them -> for each, search and fetch the top-k (under the whole-run fetch
/// budget) -> synthesize a cited answer over the fetched sources -> render.
/// Offline (`available == false`) it short-circuits to [`offline_degrade`]
/// WITHOUT searching, fetching, or synthesizing (no tokens spent, no false claim
/// of work). Generic over the [`Planner`]/[`Searcher`]/[`Fetcher`]/[`Brain`]
/// seams so tests drive it with mocks and the daemon wires the live quartet.
///
/// A single failed search or fetch is skipped (it never aborts the whole run);
/// the run continues with whatever sources it did retrieve. The returned
/// `String` is the rendered, cited answer (or the honest "couldn't source it"
/// line when nothing grounded came back).
// The rendered-only convenience over `run_research_report` (the live SAGE path
// uses the report-returning form so it can record the run for "save this
// research"; this wrapper is the rendered-string API the engine's own hermetic
// tests drive). Kept as the stable string-only entry point.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub async fn run_research(
    question: &str,
    depth: usize,
    available: bool,
    planner: &dyn Planner,
    searcher: &dyn Searcher,
    fetcher: &dyn Fetcher,
    brain: &dyn Brain,
) -> String {
    run_research_report(question, depth, available, planner, searcher, fetcher, brain)
        .await
        .0
}

/// Run a full deep-research pass exactly like [`run_research`], but return BOTH
/// the rendered cited answer AND the structured [`ResearchReport`] it was rendered
/// from (when one was actually synthesized over fetched sources). The rendered
/// string is byte-for-byte what [`run_research`] returns (which delegates here),
/// so the two never diverge. The `Option<ResearchReport>` is `Some` ONLY when a
/// real report was produced (sources were fetched AND synthesis succeeded); it is
/// `None` on the offline degrade, a planning failure, a no-sources outcome, or a
/// synthesis failure — the cases where there is no grounded run to hand to the
/// notebook. This is what lets the live SAGE path record the REAL last run for a
/// later "save this research" WITHOUT a second pass and WITHOUT ever fabricating a
/// report when none was produced.
#[allow(clippy::too_many_arguments)]
pub async fn run_research_report(
    question: &str,
    depth: usize,
    available: bool,
    planner: &dyn Planner,
    searcher: &dyn Searcher,
    fetcher: &dyn Fetcher,
    brain: &dyn Brain,
) -> (String, Option<ResearchReport>) {
    // Cost/availability honesty: a real run is web + cloud work. Don't even plan
    // when offline.
    if !available {
        return (offline_degrade(question), None);
    }

    // Plan (injected) then clamp to the bounds — the planner is never trusted to
    // self-limit.
    let raw = match planner.plan(question, depth).await {
        Ok(plan) => plan,
        Err(e) => {
            return (format!(
                "I couldn't plan that research, sir — {e}. Give me the question again, more concretely."
            ), None);
        }
    };
    let (subqueries, planned) = bound_subqueries(raw, depth);
    let mut truncated = planned > subqueries.len();

    // Search + fetch under the whole-run fetch budget. Sources get stable
    // 1-based ids in fetch order — the ids the synthesis cites.
    let mut sources: Vec<Source> = Vec::new();
    let mut fetch_budget = MAX_FETCHES;
    let mut seen_urls: Vec<String> = Vec::new();
    let mut pursued = 0usize;

    'outer: for sq in &subqueries {
        if fetch_budget == 0 {
            // Out of fetch budget before pursuing every sub-query -> truncated.
            truncated = true;
            break;
        }
        pursued += 1;

        let results = match searcher.search(&sq.query).await {
            Ok(r) => r,
            Err(_) => continue, // a failed search is skipped, never fatal
        };

        let mut taken_this_query = 0usize;
        for r in results {
            if taken_this_query >= MAX_RESULTS_PER_QUERY {
                break;
            }
            if fetch_budget == 0 {
                truncated = true;
                break 'outer;
            }
            // De-dup URLs across sub-queries so the same page is never fetched
            // (or cited) twice.
            if seen_urls.iter().any(|u| u == &r.url) {
                continue;
            }

            match fetcher.fetch(&r.url).await {
                Ok(text) => {
                    seen_urls.push(r.url.clone());
                    fetch_budget -= 1;
                    taken_this_query += 1;
                    let id = sources.len() + 1;
                    sources.push(Source {
                        id,
                        url: r.url.clone(),
                        title: r.title.clone(),
                        excerpt: text,
                    });
                }
                Err(_) => continue, // a failed fetch is skipped, never fatal
            }
        }
    }

    // Nothing fetched -> no sources to synthesize over. Be honest. No grounded
    // report exists, so the notebook side gets None (nothing to save).
    if sources.is_empty() {
        let report = ResearchReport {
            question: question.to_string(),
            sources,
            claims: Vec::new(),
            planned_subqueries: planned,
            pursued_subqueries: pursued,
            truncated,
        };
        return (render_report(&report), None);
    }

    // Synthesize a cited answer over the ACTUAL fetched sources. The brain is
    // told to cite only the ids it was handed; validate_claims (inside
    // render_report) verifies it and flags any that don't map.
    let claims = match brain.synthesize(question, &sources).await {
        Ok(c) => c,
        Err(e) => {
            return (format!(
                "I fetched {} source{} but couldn't synthesize them, sir — {e}.",
                sources.len(),
                if sources.len() == 1 { "" } else { "s" },
            ), None);
        }
    };

    let report = ResearchReport {
        question: question.to_string(),
        sources,
        claims,
        planned_subqueries: planned,
        pursued_subqueries: pursued,
        truncated,
    };
    let rendered = render_report(&report);
    (rendered, Some(report))
}

// ---------------------------------------------------------------------------
// Cloud/web-backed implementations (wired by the daemon; NOT exercised in tests)
// ---------------------------------------------------------------------------

/// The real planner: asks the cloud brain to decompose the question into focused
/// sub-queries. A thin wrapper around one plain Messages completion with a
/// planning prompt, parsed into [`SubQuery`]s. NOT exercised by any test (tests
/// inject a mock); the daemon constructs it on the live path only.
pub struct CloudPlanner {
    /// The cloud model id to plan with.
    pub model: String,
    /// Per-plan token budget.
    pub max_tokens: u32,
}

/// The planning system prompt: a one-line framing so the model treats the user
/// message purely as a question to decompose into search strings.
const PLAN_SYSTEM: &str =
    "You decompose a research question into a short list of focused web-search \
     queries. Output only the queries, one per line.";
/// Planning is latency-insensitive (it precedes the search/fetch fan-out); give
/// it the same generous ceiling the mission planner uses.
const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

impl Planner for CloudPlanner {
    fn plan<'a>(&'a self, question: &'a str, depth: usize) -> PlanFuture<'a> {
        Box::pin(async move {
            let prompt = planning_prompt(question, depth);
            let raw = crate::anthropic::complete_plain(
                &self.model,
                self.max_tokens,
                PLAN_SYSTEM,
                &prompt,
                PLAN_TIMEOUT,
            )
            .await?;
            Ok(parse_subqueries(&raw))
        })
    }
}

/// The planning prompt: instruct the cloud brain to decompose the question into
/// a short, focused list of distinct web-search queries, one per line. Pure (no
/// I/O) so the instruction text is unit-testable. Names the bound so the model
/// does not over-produce (the result is still clamped by [`bound_subqueries`]).
pub fn planning_prompt(question: &str, depth: usize) -> String {
    let cap = subquery_cap_for_depth(depth);
    format!(
        "You are SAGE, a deep-research orchestrator. Break this question into a SHORT list of at \
         most {cap} focused, DISTINCT web-search queries that together would let you answer it with \
         citations. Each query is a plain search string a search engine could take. Do NOT answer \
         the question; only list the queries. One query per line, no numbering, no commentary.\
         \n\nQUESTION: {question}"
    )
}

/// Parse the planner's line-per-query text into [`SubQuery`]s: each non-blank
/// line (with any leading list marker stripped) becomes a sub-query. Pure and
/// unit-testable. The result is still clamped by [`bound_subqueries`] downstream
/// — this parser does not enforce the count itself.
pub fn parse_subqueries(raw: &str) -> Vec<SubQuery> {
    raw.lines()
        .map(|line| strip_list_marker(line.trim()))
        .filter(|line| !line.is_empty())
        .map(SubQuery::new)
        .collect()
}

/// Strip a leading list marker ("1.", "-", "*", "•") and following space from a
/// line. Pure. (Mirrors mission::strip_list_marker; kept local so research.rs
/// carries no dependency on the mission module's private helper.)
fn strip_list_marker(line: &str) -> String {
    let trimmed = line.trim_start();
    if let Some((head, tail)) = trimmed.split_once(['.', ')']) {
        if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
            return tail.trim_start().to_string();
        }
    }
    for marker in ["- ", "* ", "• ", "– ", "— "] {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return rest.trim_start().to_string();
        }
    }
    trimmed.to_string()
}

/// The citation-discipline synthesis prompt: the system framing that tells the
/// cloud brain to answer ONLY from the supplied sources and to cite each claim
/// by its source id, never inventing a URL or a source. Pure so the instruction
/// is unit-testable. The live [`Brain`] would format the sources into the user
/// message and parse `[id]` markers out of the reply; that parsing/format pair
/// is wired on the live path only (NOT exercised by tests).
pub const SYNTH_SYSTEM: &str =
    "You are SAGE, a deep-research synthesizer. Answer ONLY from the numbered sources provided. \
     Tag every claim with the id of the source it comes from, like [2]. If the sources do not \
     support a point, do NOT state it — never invent a fact, a source, or a URL. If the sources \
     are insufficient, say so. Citations must map to the supplied source ids and nothing else.";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- Mocks (NO network, NO cloud) --------------------------------------

    /// A planner returning a fixed plan, recording the (question, depth) it saw.
    struct MockPlanner {
        plan: Vec<SubQuery>,
        seen: Mutex<Option<(String, usize)>>,
    }
    impl MockPlanner {
        fn new(plan: Vec<SubQuery>) -> Self {
            MockPlanner { plan, seen: Mutex::new(None) }
        }
    }
    impl Planner for MockPlanner {
        fn plan<'a>(&'a self, question: &'a str, depth: usize) -> PlanFuture<'a> {
            Box::pin(async move {
                *self.seen.lock().unwrap() = Some((question.to_string(), depth));
                Ok(self.plan.clone())
            })
        }
    }

    /// A searcher that returns canned results per query and records every query
    /// it was asked. Unknown queries return a single generic hit.
    struct MockSearcher {
        per_query: Vec<(String, Vec<SearchResult>)>,
        queries: Mutex<Vec<String>>,
    }
    impl MockSearcher {
        fn new(per_query: Vec<(&str, Vec<SearchResult>)>) -> Self {
            MockSearcher {
                per_query: per_query.into_iter().map(|(q, r)| (q.to_string(), r)).collect(),
                queries: Mutex::new(Vec::new()),
            }
        }
        fn queries(&self) -> Vec<String> {
            self.queries.lock().unwrap().clone()
        }
    }
    impl Searcher for MockSearcher {
        fn search<'a>(&'a self, query: &'a str) -> SearchFuture<'a> {
            Box::pin(async move {
                self.queries.lock().unwrap().push(query.to_string());
                let hits = self
                    .per_query
                    .iter()
                    .find(|(q, _)| q == query)
                    .map(|(_, r)| r.clone())
                    .unwrap_or_else(|| {
                        vec![SearchResult::new(
                            format!("generic for {query}"),
                            format!("https://example.test/{}", query.replace(' ', "-")),
                        )]
                    });
                Ok(hits)
            })
        }
    }

    /// A fetcher that returns canned text per URL and records every URL fetched.
    /// A URL in `fail` returns an error (to prove a failed fetch is skipped).
    struct MockFetcher {
        fail: Vec<String>,
        fetched: Mutex<Vec<String>>,
    }
    impl MockFetcher {
        fn new() -> Self {
            MockFetcher { fail: Vec::new(), fetched: Mutex::new(Vec::new()) }
        }
        fn failing(fail: &[&str]) -> Self {
            MockFetcher {
                fail: fail.iter().map(|s| s.to_string()).collect(),
                fetched: Mutex::new(Vec::new()),
            }
        }
        fn fetched(&self) -> Vec<String> {
            self.fetched.lock().unwrap().clone()
        }
    }
    impl Fetcher for MockFetcher {
        fn fetch<'a>(&'a self, url: &'a str) -> FetchFuture<'a> {
            Box::pin(async move {
                if self.fail.iter().any(|u| u == url) {
                    return Err(anyhow::anyhow!("mock fetch failure for {url}"));
                }
                self.fetched.lock().unwrap().push(url.to_string());
                Ok(format!("excerpt of {url}"))
            })
        }
    }

    /// A brain that cites the FIRST fetched source for every claim it makes — so
    /// its claims are grounded by construction (it only ever cites a handed id).
    /// Records the sources it was handed. Proves the brain reasons over the
    /// ACTUAL fetched set.
    struct CitingBrain {
        seen_source_ids: Mutex<Vec<usize>>,
    }
    impl CitingBrain {
        fn new() -> Self {
            CitingBrain { seen_source_ids: Mutex::new(Vec::new()) }
        }
    }
    impl Brain for CitingBrain {
        fn synthesize<'a>(&'a self, _question: &'a str, sources: &'a [Source]) -> SynthFuture<'a> {
            Box::pin(async move {
                *self.seen_source_ids.lock().unwrap() = sources.iter().map(|s| s.id).collect();
                // One claim per source, each citing that source's real id.
                let claims = sources
                    .iter()
                    .map(|s| Claim::new(format!("Finding from {}", s.title), s.id))
                    .collect();
                Ok(claims)
            })
        }
    }

    /// A brain that FABRICATES a citation: it cites an id (999) that was never
    /// fetched. Proves validate_claims/render flag it rather than presenting it.
    struct FabricatingBrain;
    impl Brain for FabricatingBrain {
        fn synthesize<'a>(&'a self, _q: &'a str, _s: &'a [Source]) -> SynthFuture<'a> {
            Box::pin(async move {
                Ok(vec![
                    Claim::new("A grounded point", 1),
                    Claim::new("An invented point citing a phantom source", 999),
                    Claim::new("An uncited point", 0),
                ])
            })
        }
    }

    fn results(urls: &[(&str, &str)]) -> Vec<SearchResult> {
        urls.iter().map(|(t, u)| SearchResult::new(*t, *u)).collect()
    }

    // ---- bounds: sub-query cap & depth -------------------------------------

    #[test]
    fn bound_subqueries_clamps_to_depth_and_drops_blanks() {
        let raw = vec![
            SubQuery::new("a"),
            SubQuery::new("   "), // blank, dropped
            SubQuery::new("b"),
            SubQuery::new("c"),
            SubQuery::new("d"),
        ];
        // depth 2 -> cap 2.
        let (bounded, original) = bound_subqueries(raw.clone(), 2);
        assert_eq!(original, 4, "blank not counted in the original");
        assert_eq!(bounded.len(), 2, "clamped to depth-2 cap");
        // A huge depth is clamped to MAX_SUBQUERIES.
        let (bounded, _) = bound_subqueries(raw, 99);
        assert!(bounded.len() <= MAX_SUBQUERIES);
        assert_eq!(bounded.len(), 4, "all four non-blank fit under the max cap");
    }

    #[test]
    fn depth_maps_to_a_bounded_subquery_cap() {
        assert_eq!(subquery_cap_for_depth(0), 1, "depth floors at 1");
        assert_eq!(subquery_cap_for_depth(1), 1);
        assert_eq!(subquery_cap_for_depth(3), 3);
        assert_eq!(subquery_cap_for_depth(999), MAX_SUBQUERIES, "depth ceils at the cap");
    }

    // ---- the happy path: plan -> search -> fetch -> cited synthesis ---------

    #[tokio::test]
    async fn full_run_fetches_sources_and_cites_them() {
        let planner = MockPlanner::new(vec![SubQuery::new("q1"), SubQuery::new("q2")]);
        let searcher = MockSearcher::new(vec![
            ("q1", results(&[("Doc A", "https://a.test"), ("Doc B", "https://b.test")])),
            ("q2", results(&[("Doc C", "https://c.test")])),
        ]);
        let fetcher = MockFetcher::new();
        let brain = CitingBrain::new();

        let answer = run_research("what is X", DEFAULT_DEPTH, true, &planner, &searcher, &fetcher, &brain).await;

        // The planner saw the question + depth.
        assert_eq!(
            planner.seen.lock().unwrap().as_ref().unwrap(),
            &("what is X".to_string(), DEFAULT_DEPTH)
        );
        // Both sub-queries were searched.
        assert_eq!(searcher.queries(), vec!["q1", "q2"]);
        // Three distinct URLs fetched (top-k under budget).
        assert_eq!(fetcher.fetched().len(), 3);
        // The brain was handed the real fetched ids 1..=3.
        assert_eq!(*brain.seen_source_ids.lock().unwrap(), vec![1, 2, 3]);
        // The answer carries citation markers and a bibliography of the real URLs.
        assert!(answer.contains("[1]"), "cited claim present: {answer}");
        assert!(answer.contains("Sources:"), "bibliography present: {answer}");
        assert!(answer.contains("https://a.test"), "real fetched URL cited: {answer}");
        assert!(answer.contains("https://c.test"), "real fetched URL cited: {answer}");
    }

    // ---- bound: whole-run fetch budget, signalled truncation ---------------

    #[tokio::test]
    async fn fetch_budget_caps_total_fetches_and_signals_truncation() {
        // Five sub-queries, each with three rich results -> 15 candidates, but
        // the whole-run fetch budget caps total fetches at MAX_FETCHES.
        let planner = MockPlanner::new(
            (0..5).map(|i| SubQuery::new(format!("q{i}"))).collect(),
        );
        let per_query: Vec<(&str, Vec<SearchResult>)> = vec![
            ("q0", results(&[("A0", "https://0a.test"), ("B0", "https://0b.test"), ("C0", "https://0c.test")])),
            ("q1", results(&[("A1", "https://1a.test"), ("B1", "https://1b.test"), ("C1", "https://1c.test")])),
            ("q2", results(&[("A2", "https://2a.test"), ("B2", "https://2b.test"), ("C2", "https://2c.test")])),
            ("q3", results(&[("A3", "https://3a.test"), ("B3", "https://3b.test"), ("C3", "https://3c.test")])),
            ("q4", results(&[("A4", "https://4a.test"), ("B4", "https://4b.test"), ("C4", "https://4c.test")])),
        ];
        let searcher = MockSearcher::new(per_query);
        let fetcher = MockFetcher::new();
        let brain = CitingBrain::new();

        let answer = run_research("deep one", MAX_SUBQUERIES, true, &planner, &searcher, &fetcher, &brain).await;

        // Never more than the whole-run fetch budget.
        assert_eq!(fetcher.fetched().len(), MAX_FETCHES, "fetches capped at the budget");
        // Truncation disclosed honestly.
        assert!(
            answer.to_lowercase().contains("bounded") && answer.contains(&MAX_FETCHES.to_string()),
            "truncation must be disclosed: {answer}"
        );
    }

    // ---- citation discipline: invented/uncited claims are FLAGGED -----------

    #[test]
    fn validate_claims_separates_grounded_from_ungrounded() {
        let sources = vec![
            Source { id: 1, url: "https://a.test".into(), title: "A".into(), excerpt: "x".into() },
            Source { id: 2, url: "https://b.test".into(), title: "B".into(), excerpt: "y".into() },
        ];
        let claims = vec![
            Claim::new("backed by 1", 1),
            Claim::new("backed by 2", 2),
            Claim::new("cites a phantom", 999),
            Claim::new("uncited", 0),
        ];
        let (grounded, ungrounded) = validate_claims(&claims, &sources);
        assert_eq!(grounded.len(), 2, "two real citations");
        assert_eq!(ungrounded.len(), 2, "the phantom and the uncited are flagged");
        assert!(ungrounded.iter().any(|c| c.source_id == 999));
        assert!(ungrounded.iter().any(|c| c.source_id == 0));
    }

    #[tokio::test]
    async fn a_fabricated_citation_is_never_presented_as_fact() {
        let planner = MockPlanner::new(vec![SubQuery::new("q1")]);
        let searcher = MockSearcher::new(vec![("q1", results(&[("Doc A", "https://a.test")]))]);
        let fetcher = MockFetcher::new();
        let brain = FabricatingBrain;

        let answer = run_research("verify me", 1, true, &planner, &searcher, &fetcher, &brain).await;

        // The one grounded claim (cites id 1, a real fetched source) is present.
        assert!(answer.contains("[1]"), "the grounded claim is cited: {answer}");
        // The phantom URL is NEVER fabricated into the bibliography — only the
        // real fetched source appears.
        assert!(answer.contains("https://a.test"), "real source in bib: {answer}");
        assert!(!answer.contains("999"), "the phantom source id must not appear as a citation: {answer}");
        // The two ungrounded points (phantom + uncited) are set aside, flagged.
        assert!(
            answer.to_lowercase().contains("set aside") && answer.contains("2"),
            "ungrounded points must be flagged, not presented as fact: {answer}"
        );
    }

    #[tokio::test]
    async fn all_ungrounded_yields_an_honest_no_sourced_answer() {
        // The brain returns ONLY uncited/phantom claims -> nothing grounded ->
        // SAGE refuses to present an answer and says so.
        struct AllPhantom;
        impl Brain for AllPhantom {
            fn synthesize<'a>(&'a self, _q: &'a str, _s: &'a [Source]) -> SynthFuture<'a> {
                Box::pin(async move { Ok(vec![Claim::new("unbacked", 0), Claim::new("phantom", 42)]) })
            }
        }
        let planner = MockPlanner::new(vec![SubQuery::new("q1")]);
        let searcher = MockSearcher::new(vec![("q1", results(&[("Doc A", "https://a.test")]))]);
        let fetcher = MockFetcher::new();
        let answer = run_research("nope", 1, true, &planner, &searcher, &fetcher, &AllPhantom).await;
        assert!(
            answer.to_lowercase().contains("couldn't put together a sourced answer"),
            "no grounded claim -> honest refusal, never a fabricated answer: {answer}"
        );
    }

    // ---- a failed search / fetch is skipped, never fatal --------------------

    #[tokio::test]
    async fn a_failed_fetch_is_skipped_and_the_run_continues() {
        let planner = MockPlanner::new(vec![SubQuery::new("q1")]);
        let searcher = MockSearcher::new(vec![(
            "q1",
            results(&[("Bad", "https://bad.test"), ("Good", "https://good.test")]),
        )]);
        // The first URL fails; the run must still fetch and cite the second.
        let fetcher = MockFetcher::failing(&["https://bad.test"]);
        let brain = CitingBrain::new();
        let answer = run_research("resilient", 1, true, &planner, &searcher, &fetcher, &brain).await;
        assert_eq!(fetcher.fetched(), vec!["https://good.test"], "only the good URL was fetched");
        assert!(answer.contains("https://good.test"), "the surviving source is cited: {answer}");
        assert!(!answer.contains("https://bad.test"), "the failed source is never cited: {answer}");
    }

    #[tokio::test]
    async fn no_sources_fetched_yields_an_honest_message() {
        // Every fetch fails -> no sources -> honest "no usable sources," never a
        // synthesized answer (the brain is never even called).
        struct BoomBrain;
        impl Brain for BoomBrain {
            fn synthesize<'a>(&'a self, _q: &'a str, _s: &'a [Source]) -> SynthFuture<'a> {
                Box::pin(async move { panic!("brain must NOT run with zero sources") })
            }
        }
        let planner = MockPlanner::new(vec![SubQuery::new("q1")]);
        let searcher = MockSearcher::new(vec![("q1", results(&[("Bad", "https://bad.test")]))]);
        let fetcher = MockFetcher::failing(&["https://bad.test"]);
        let answer = run_research("dead ends", 1, true, &planner, &searcher, &fetcher, &BoomBrain).await;
        assert!(
            answer.to_lowercase().contains("no usable sources")
                || answer.to_lowercase().contains("couldn't put together"),
            "no fetched sources -> honest message, brain never called: {answer}"
        );
    }

    // ---- offline -> friendly degrade (no plan, search, fetch, or synth) ----

    #[tokio::test]
    async fn offline_degrades_without_touching_any_provider() {
        // Providers that PANIC if touched — proving the offline path never runs
        // them (no tokens spent, no false claim of work).
        struct Boom;
        impl Planner for Boom {
            fn plan<'a>(&'a self, _: &'a str, _: usize) -> PlanFuture<'a> {
                Box::pin(async { panic!("planner must NOT run offline") })
            }
        }
        impl Searcher for Boom {
            fn search<'a>(&'a self, _: &'a str) -> SearchFuture<'a> {
                Box::pin(async { panic!("searcher must NOT run offline") })
            }
        }
        impl Fetcher for Boom {
            fn fetch<'a>(&'a self, _: &'a str) -> FetchFuture<'a> {
                Box::pin(async { panic!("fetcher must NOT run offline") })
            }
        }
        impl Brain for Boom {
            fn synthesize<'a>(&'a self, _: &'a str, _: &'a [Source]) -> SynthFuture<'a> {
                Box::pin(async { panic!("brain must NOT run offline") })
            }
        }
        let answer = run_research("ship it", DEFAULT_DEPTH, false, &Boom, &Boom, &Boom, &Boom).await;
        assert!(
            answer.to_lowercase().contains("needs the web and the cloud")
                || answer.to_lowercase().contains("offline"),
            "offline must degrade to a friendly web+cloud-needed line: {answer}"
        );
        assert!(answer.contains("ship it"), "the question is echoed back: {answer}");
        // Honest about cost.
        assert!(answer.to_lowercase().contains("token"), "states the cost honestly: {answer}");
    }

    // ---- de-dup: the same URL is never fetched or cited twice ----------------

    #[tokio::test]
    async fn the_same_url_across_subqueries_is_fetched_once() {
        let planner = MockPlanner::new(vec![SubQuery::new("q1"), SubQuery::new("q2")]);
        // Both sub-queries surface the SAME URL; it must be fetched/cited once.
        let searcher = MockSearcher::new(vec![
            ("q1", results(&[("Shared", "https://shared.test")])),
            ("q2", results(&[("Shared again", "https://shared.test"), ("Unique", "https://unique.test")])),
        ]);
        let fetcher = MockFetcher::new();
        let brain = CitingBrain::new();
        let answer = run_research("dedup", DEFAULT_DEPTH, true, &planner, &searcher, &fetcher, &brain).await;
        assert_eq!(
            fetcher.fetched(),
            vec!["https://shared.test", "https://unique.test"],
            "the shared URL is fetched exactly once"
        );
        assert!(answer.contains("https://unique.test"));
    }

    // ---- rendering: bibliography, citations, ungrounded handling -----------

    #[test]
    fn render_report_shows_citations_and_a_grounded_bibliography() {
        let report = ResearchReport {
            question: "what causes Y".into(),
            sources: vec![
                Source { id: 1, url: "https://one.test".into(), title: "First".into(), excerpt: "e1".into() },
                Source { id: 2, url: "https://two.test".into(), title: "Second".into(), excerpt: "e2".into() },
            ],
            claims: vec![Claim::new("Y is caused by Z", 1), Claim::new("and also by W", 2)],
            planned_subqueries: 2,
            pursued_subqueries: 2,
            truncated: false,
        };
        let out = render_report(&report);
        assert!(out.contains("Y is caused by Z [1]"), "claim carries its citation: {out}");
        assert!(out.contains("[1] First — https://one.test"), "bibliography entry: {out}");
        assert!(out.contains("[2] Second — https://two.test"), "bibliography entry: {out}");
        assert!(!out.to_lowercase().contains("set aside"), "no ungrounded note when all grounded: {out}");
    }

    #[test]
    fn ungrounded_claims_accessor_lists_unbacked_claims() {
        let report = ResearchReport {
            question: "q".into(),
            sources: vec![Source { id: 1, url: "u".into(), title: "t".into(), excerpt: "e".into() }],
            claims: vec![Claim::new("ok", 1), Claim::new("bad", 9)],
            planned_subqueries: 1,
            pursued_subqueries: 1,
            truncated: false,
        };
        let ung = report.ungrounded_claims();
        assert_eq!(ung.len(), 1);
        assert_eq!(ung[0].source_id, 9);
    }

    // ---- planning prompt / parsing (pure, used by the real CloudPlanner) ----

    #[test]
    fn planning_prompt_states_the_bound_and_asks_for_queries_not_an_answer() {
        let p = planning_prompt("how does photosynthesis work", 3);
        assert!(p.contains(&subquery_cap_for_depth(3).to_string()), "states the sub-query cap: {p}");
        assert!(p.to_lowercase().contains("search"), "asks for search queries: {p}");
        assert!(p.to_lowercase().contains("do not answer"), "forbids answering at plan time: {p}");
        assert!(p.contains("how does photosynthesis work"), "carries the question: {p}");
    }

    #[test]
    fn synth_system_enforces_citation_discipline_and_no_fabrication() {
        let s = SYNTH_SYSTEM.to_lowercase();
        assert!(s.contains("only from"), "answers only from the sources: {SYNTH_SYSTEM}");
        assert!(s.contains("citation") || s.contains("tag every claim"), "requires citations: {SYNTH_SYSTEM}");
        assert!(
            s.contains("never invent") || s.contains("do not state"),
            "forbids fabrication: {SYNTH_SYSTEM}"
        );
        assert!(s.contains("url"), "explicitly forbids invented URLs: {SYNTH_SYSTEM}");
    }

    #[test]
    fn parse_subqueries_handles_markers_and_blanks() {
        let raw = "1. causes of inflation\n- monetary policy 2020\n* supply chain shocks\n\n  • wage growth data  \nfinal angle";
        let qs = parse_subqueries(raw);
        let strs: Vec<&str> = qs.iter().map(|q| q.query.as_str()).collect();
        assert_eq!(
            strs,
            vec![
                "causes of inflation",
                "monetary policy 2020",
                "supply chain shocks",
                "wage growth data",
                "final angle",
            ],
            "list markers stripped and blank lines dropped"
        );
    }

    // ---- provenance_payload (F4 wire contract) ------------------------------

    fn prov_report() -> ResearchReport {
        ResearchReport {
            question: "What causes inflation?".to_string(),
            sources: vec![
                Source {
                    id: 1,
                    url: "https://example.com/a".to_string(),
                    title: "Money and prices".to_string(),
                    excerpt: "…".to_string(),
                },
                Source {
                    id: 2,
                    url: "https://example.com/b".to_string(),
                    title: "Supply shocks".to_string(),
                    excerpt: "…".to_string(),
                },
            ],
            claims: vec![
                Claim::new("Monetary expansion raises prices", 1),
                Claim::new("An unsourced assertion", 0),
                Claim::new("Supply shocks push costs up", 2),
                Claim::new("Cites a source that was never fetched", 9),
            ],
            planned_subqueries: 3,
            pursued_subqueries: 3,
            truncated: false,
        }
    }

    /// The exact wire shape the HUD parser mirrors: field names, the honest
    /// counts, and ungrounded rows FIRST (the set-aside signal survives any cap).
    #[test]
    fn provenance_payload_pins_the_wire_shape_with_ungrounded_first() {
        let payload = provenance_payload(&prov_report());
        assert_eq!(payload["question"], "What causes inflation?");
        assert_eq!(payload["sources_fetched"], 2);
        assert_eq!(payload["claims_total"], 4);
        assert_eq!(payload["claims_grounded"], 2);
        assert_eq!(payload["claims_ungrounded"], 2);
        assert_eq!(payload["claims_omitted"], 0);
        assert_eq!(payload["truncated"], false);
        let keys: Vec<&String> = payload.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "claims",
                "claims_grounded",
                "claims_omitted",
                "claims_total",
                "claims_ungrounded",
                "question",
                "sources_fetched",
                "truncated"
            ]
        );

        let claims = payload["claims"].as_array().unwrap();
        assert_eq!(claims.len(), 4);
        // Ungrounded first: the uncited claim and the phantom-source claim.
        assert_eq!(claims[0]["grounded"], false);
        assert_eq!(claims[0]["text"], "An unsourced assertion");
        assert_eq!(claims[0]["source_id"], 0);
        assert_eq!(claims[0]["source_title"], "");
        assert_eq!(claims[1]["grounded"], false);
        assert_eq!(claims[1]["source_id"], 9);
        assert_eq!(claims[1]["source_title"], "", "a phantom source id resolves to nothing");
        // Grounded rows carry the real backing source.
        assert_eq!(claims[2]["grounded"], true);
        assert_eq!(claims[2]["source_title"], "Money and prices");
        assert_eq!(claims[2]["source_url"], "https://example.com/a");
        let row_keys: Vec<&String> = claims[0].as_object().unwrap().keys().collect();
        assert_eq!(row_keys, ["grounded", "source_id", "source_title", "source_url", "text"]);
    }

    /// The payload is bounded and redacted: long texts truncate with an
    /// ellipsis, the row cap holds with ungrounded rows surviving, and a
    /// credentialed URL or an email inside a claim is masked.
    #[test]
    fn provenance_payload_is_bounded_and_redacted() {
        let mut report = prov_report();
        report.claims = Vec::new();
        // A credentialed source URL: any grounded row citing it must mask it.
        report.sources[1].url = "https://user:hunter2@example.com/b".to_string();
        report.claims.push(Claim::new("cites the credentialed source", 2));
        // 19 more grounded: far past the row cap.
        for i in 0..19 {
            report.claims.push(Claim::new(format!("grounded fact {i}"), 1));
        }
        // 5 ungrounded — including a 500-char text and an email-bearing one —
        // ALL must survive the cap (ungrounded-first) and be bounded/redacted.
        for i in 0..3 {
            report.claims.push(Claim::new(format!("set aside {i}"), 0));
        }
        report.claims.push(Claim::new("x".repeat(500), 0));
        report.claims.push(Claim::new("Mail alice@example.com about this", 0));

        let payload = provenance_payload(&report);
        let rows = payload["claims"].as_array().unwrap();
        assert_eq!(rows.len(), 12, "row cap");
        // Every ungrounded row survives, ahead of the grounded volume.
        assert_eq!(
            rows.iter().filter(|r| r["grounded"] == false).count(),
            5,
            "set-aside rows are never truncated away"
        );
        assert!(rows[..5].iter().all(|r| r["grounded"] == false), "ungrounded first");
        // The honest counts are NOT capped, and the row drop is DISCLOSED.
        assert_eq!(payload["claims_total"], 25);
        assert_eq!(payload["claims_grounded"], 20);
        assert_eq!(payload["claims_omitted"], 13, "25 claims, 12 rows shipped");
        let text = payload.to_string();
        assert!(!text.contains("alice@example.com"), "emails masked: {text}");
        assert!(!text.contains("hunter2"), "credentialed URL masked");
        // Long text bounded with an ellipsis.
        let long_row = rows.iter().find(|r| r["text"].as_str().unwrap().starts_with('x')).unwrap();
        assert!(long_row["text"].as_str().unwrap().chars().count() <= 201);
        assert!(long_row["text"].as_str().unwrap().ends_with('…'));
        // URLs are bounded like every other field (a scraped href can carry
        // multi-KB tracking params; redaction alone never truncates).
        let grounded_row = rows.iter().find(|r| r["grounded"] == true).unwrap();
        assert!(grounded_row["source_url"].as_str().unwrap().chars().count() <= 201);
    }

    /// A fully-uncited model reply: MORE set-aside claims than the row cap.
    /// The rows that ship are all set-aside, and the drop is DISCLOSED via
    /// claims_omitted — a capped list is never presented as complete.
    #[test]
    fn provenance_payload_discloses_when_even_set_aside_rows_exceed_the_cap() {
        let mut report = prov_report();
        report.claims = (0..20).map(|i| Claim::new(format!("uncited line {i}"), 0)).collect();
        let payload = provenance_payload(&report);
        let rows = payload["claims"].as_array().unwrap();
        assert_eq!(rows.len(), 12);
        assert!(rows.iter().all(|r| r["grounded"] == false));
        assert_eq!(payload["claims_ungrounded"], 20);
        assert_eq!(payload["claims_omitted"], 8, "the 8 dropped set-aside rows are disclosed");
        // A long URL on a source is also bounded (belt for the url cap).
        let mut r2 = prov_report();
        r2.sources[0].url = format!("https://example.com/?q={}", "y".repeat(1000));
        let p2 = provenance_payload(&r2);
        let g = p2["claims"].as_array().unwrap().iter().find(|r| r["source_id"] == 1).unwrap();
        assert!(g["source_url"].as_str().unwrap().chars().count() <= 201);
    }
}
