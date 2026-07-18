//! UNIFIED PERSONAL SEARCH — one query, fanned out across every AVAILABLE
//! source, merged into a single ranked list where each hit is ATTRIBUTED to its
//! source and carries a real CITATION, plus an HONEST coverage summary of which
//! sources were searched vs skipped (and why).
//!
//! The sources, and the rules that govern each:
//!
//!   * ON-DEVICE (always searched when present; content NEVER leaves the device):
//!       - DOCSEARCH  — [`crate::docsearch::DocIndex::search`] over the user's
//!         OWN indexed files (cited to file path + byte offset + snippet). Only
//!         contributes when an index exists.
//!       - EPISODIC   — [`crate::episodic::episodic_recall`], AGENT-SCOPED (own
//!         namespace + the shared orchestrator tier), cited to an episode.
//!       - FACTS      — [`crate::memory::Memory::agent_scoped_facts`], AGENT-
//!         SCOPED, cited to a fact key.
//!       - WORLD      — [`crate::world_model::query`] over the SHARED structured
//!         model, cited to an entity/relationship.
//!
//!   * CLOUD (searched ONLY when CONNECTED — `resolve_secret` returned `Some` —
//!     and ONLY via the EXISTING gated READ-ONLY reads; NO new outward surface,
//!     NO consequential action). The existing reads return a human SUMMARY line
//!     (the same one the dedicated tool surfaces), NOT per-item ids — so each is
//!     attributed as a hit cited at the SOURCE LEVEL ([`Citation::CloudSource`]):
//!     the honest anchor names the gated READ the user can reproduce, never a
//!     fabricated per-item id. (If a future per-item read supplies a genuine id,
//!     the per-item variants below carry it instead.)
//!       - GMAIL      — recent-messages read (cited to that read for the query).
//!       - CALENDAR   — upcoming-events read (cited to that read for the query).
//!       - SLACK      — channel-list read (cited to that read; the live read is a
//!         channel LIST, so the citation is honest about being the
//!         channel-list read, not a non-existent message coordinate).
//!         A DISCONNECTED cloud source is SKIPPED with a reason — never silently
//!         dropped, never fabricated as if it had been searched.
//!
//! HONESTY (load-bearing):
//!   * SCOPING is preserved exactly as each underlying source already enforces
//!     it — this module never WIDENS a scope. Agent A's unified search can never
//!     surface agent B's private episodes/facts (the episodic/facts reads are
//!     agent-scoped at their own Db layer; this module merely merges what they
//!     return). The unit tests pin this.
//!   * Every hit cites a REAL item; nothing is ever fabricated. An all-empty
//!     fan-out returns an empty hit list (the coverage line still reports which
//!     sources were searched), so the caller can say "I searched X and Y and
//!     found nothing" rather than inventing a result.
//!   * COVERAGE is reported truthfully: a [`Coverage`] records exactly which
//!     sources were SEARCHED and which were SKIPPED (each with a machine-stable
//!     reason), so the answer can say "searched files + memory; skipped Gmail
//!     (not connected)".
//!
//! DESIGN — testable core vs. live fan-out. The MERGE/RANK/ATTRIBUTE/COVERAGE
//! logic is a PURE function over already-fetched per-source results plus the
//! per-cloud-source connection verdict ([`fold`]). The live tool layer
//! (anthropic.rs) does the ACTUAL reads — the on-device searches and, ONLY for a
//! connected cloud source, the existing gated read — then hands the results to
//! [`fold`]. So the hermetic tests inject MOCK per-source results + mock
//! connected/disconnected verdicts and exercise the entire merge/rank/coverage/
//! citation contract with NO real source, network, or MLX call.

use std::cmp::Ordering;

use crate::docsearch::DocHit;
use crate::memory::Episode;
use crate::recall::{cosine_similarity, Bm25Params, EmbeddingProvider, Fact, LexicalProvider};
use crate::world_model::{Entity, Relationship};

/// Default / max number of merged hits a single unified search returns. Bounded
/// so the surface stays focused on the relevant few across all sources.
pub const UNIFIED_DEFAULT_K: usize = 8;
pub const UNIFIED_MAX_K: usize = 40;

/// How many candidates we pull from each source BEFORE the cross-source merge.
/// Generous (so a strongly-relevant item in any one source survives into the
/// final cut) but bounded (so one source cannot swamp the merge). The final list
/// is still capped at `k` after ranking.
pub const PER_SOURCE_CANDIDATES: usize = 12;

/// The fraction of a hit's final score that comes from RECENCY (vs. relevance).
/// Relevance dominates — recency is a gentle, deterministic tie-shaper so that,
/// among comparably-relevant hits, the fresher one ranks higher. Recency is only
/// meaningful for the time-stamped sources (episodes / gmail / calendar / slack);
/// docsearch / facts / world carry no per-hit timestamp and contribute 0 recency
/// (relevance-only), which is honest — we do not invent a recency signal.
const RECENCY_WEIGHT: f64 = 0.15;

// ---------------------------------------------------------------------------
// Source taxonomy
// ---------------------------------------------------------------------------

/// Which source a hit (or a coverage entry) belongs to. ON-DEVICE sources are
/// always available; CLOUD sources are gated by a connected-check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// On-device file RAG (the user's OWN indexed files).
    Docsearch,
    /// On-device, agent-scoped episode store.
    Episodic,
    /// On-device, agent-scoped fact memory.
    Facts,
    /// On-device shared structured world model.
    World,
    /// Cloud: Gmail (read-only; connected-gated).
    Gmail,
    /// Cloud: Google Calendar (read-only; connected-gated).
    Calendar,
    /// Cloud: Slack (read-only; connected-gated).
    Slack,
}

impl Source {
    /// A short, stable token for telemetry / structured status / the HUD.
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Docsearch => "docsearch",
            Source::Episodic => "episodic",
            Source::Facts => "facts",
            Source::World => "world",
            Source::Gmail => "gmail",
            Source::Calendar => "calendar",
            Source::Slack => "slack",
        }
    }

    /// Human label for the HUD panel grouping + the spoken coverage line.
    pub fn label(&self) -> &'static str {
        match self {
            Source::Docsearch => "Files",
            Source::Episodic => "Past conversations",
            Source::Facts => "Memory",
            Source::World => "World model",
            Source::Gmail => "Gmail",
            Source::Calendar => "Calendar",
            Source::Slack => "Slack",
        }
    }

    /// Whether this source is ON-DEVICE (content never leaves the device). The
    /// cloud sources are the only ones gated by a connected-check.
    // Exercised by the unit tests + a structured-status surface; the live merge
    // partitions sources by the fixed iteration order rather than this predicate.
    #[allow(dead_code)]
    pub fn is_on_device(&self) -> bool {
        matches!(
            self,
            Source::Docsearch | Source::Episodic | Source::Facts | Source::World
        )
    }
}

/// Why a source was SKIPPED — machine-stable so the HUD and the spoken copy can
/// render an honest reason rather than a silent drop. NEVER used to pretend a
/// source was searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// A cloud source's `resolve_secret` returned `None` — not connected in
    /// Settings. The honest, common case.
    NotConnected,
    /// An on-device source that has no data to search (e.g. the file index has
    /// not been built / file search is off). Distinct from "searched and found
    /// nothing": this source could not be searched at all.
    NoIndex,
    /// The caller explicitly excluded this source from the fan-out (e.g. a
    /// scoped "search my files" request). Honest: we did not search it.
    NotRequested,
}

impl SkipReason {
    /// A short, stable token for telemetry / structured status.
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::NotConnected => "not_connected",
            SkipReason::NoIndex => "no_index",
            SkipReason::NotRequested => "not_requested",
        }
    }

    /// One honest human clause for the coverage line (e.g. "not connected").
    pub fn human(&self) -> &'static str {
        match self {
            SkipReason::NotConnected => "not connected",
            SkipReason::NoIndex => "no index built",
            SkipReason::NotRequested => "not requested",
        }
    }
}

// ---------------------------------------------------------------------------
// Citations — each hit cites a REAL item, never fabricated
// ---------------------------------------------------------------------------

/// A CITATION to the real underlying item a hit came from. Each variant carries
/// exactly the anchor a user (or the HUD) needs to verify the hit is real.
///
/// Two honesty tiers, never blurred:
///   * PER-ITEM anchors — [`Citation::File`], [`Episode`], [`Fact`], [`World`],
///     and the cloud [`GmailMessage`]/[`CalendarEvent`]/[`SlackMessage`] — each
///     names ONE concrete item (file+offset, episode id, fact key, entity, a
///     genuine message/event id+ts). These are constructed ONLY when a real
///     per-item id is actually in hand. The on-device adapters always have one;
///     the cloud per-item variants are used only when a structured per-item read
///     supplies a genuine id (e.g. a future `channel_history`/message-id read),
///     never from a synthesized string.
///   * SOURCE-LEVEL anchor — [`Citation::CloudSource`] — names the EXISTING gated
///     read that produced a hit ("the Gmail recent-messages read for <query>")
///     when that read returns only a human SUMMARY, not per-item ids. This is the
///     honest anchor for the live cloud-summary path: it does NOT claim a fake
///     message/event id, it points at the real read the user can reproduce. The
///     underlying summary text is real; the citation is truthful about being
///     source-level, not item-level.
///
/// NEVER constructed except from a genuine source item or a genuine read.
///
/// [`Episode`]: Citation::Episode
/// [`Fact`]: Citation::Fact
/// [`World`]: Citation::World
/// [`GmailMessage`]: Citation::GmailMessage
/// [`CalendarEvent`]: Citation::CalendarEvent
/// [`SlackMessage`]: Citation::SlackMessage
#[derive(Debug, Clone, PartialEq)]
pub enum Citation {
    /// A real indexed file chunk: its path + byte offset (the citation anchor).
    File { path: String, byte_offset: i64 },
    /// A real recorded episode: its timestamp (the temporal anchor) + id.
    Episode { ts: String, id: i64 },
    /// A real stored fact: its namespaced key.
    Fact { key: String },
    /// A real world-model entity (by type + id) or relationship.
    World { kind: String, id: String },
    // The three per-item cloud anchors below are the HONEST per-message/per-event
    // citations for when a structured read actually supplies a genuine id (the
    // finding's fix-option (a); e.g. a future per-message-id Gmail read or a
    // `conversations.history` Slack read). The current live cloud reads return a
    // SUMMARY string, so the live path cites [`Citation::CloudSource`] instead and
    // these are not constructed off the live path yet — but they are part of the
    // contract and are exercised by the hermetic unit-test mocks (where the ids ARE
    // real). Kept (not deleted) so a genuine per-item id is cited as one when a
    // read exposes it, rather than ever being forced into a source-level anchor.
    #[allow(dead_code)]
    /// A real Gmail message: its genuine message id. Constructed ONLY from a
    /// per-message read that exposes the id — never from a synthesized string.
    GmailMessage { message_id: String },
    #[allow(dead_code)]
    /// A real calendar event: its genuine event id + start time. Constructed
    /// ONLY from a per-event read that exposes the id.
    CalendarEvent { event_id: String, start: String },
    #[allow(dead_code)]
    /// A real Slack message: its channel + ts (the unique message coordinate).
    /// Constructed ONLY from a per-message read (`conversations.history`) that
    /// exposes a real ts — never from a channel-list summary.
    SlackMessage { channel: String, ts: String },
    /// A SOURCE-LEVEL anchor for a cloud hit whose gated read returned a human
    /// SUMMARY rather than per-item ids. Names the read honestly ("the Gmail
    /// recent-messages read for <query>") so the user can reproduce it — it does
    /// NOT pretend to be a per-item id/ts. `read` is a short, stable label for
    /// the specific gated read (e.g. "gmail recent messages", "calendar upcoming
    /// events", "slack channel list").
    CloudSource { source: Source, read: String, query: String },
}

impl Citation {
    /// A short, stable, human-readable anchor string for the HUD / spoken cite.
    pub fn anchor(&self) -> String {
        match self {
            Citation::File { path, byte_offset } => format!("{path} (offset {byte_offset})"),
            Citation::Episode { ts, .. } => format!("episode @ {ts}"),
            Citation::Fact { key } => key.clone(),
            Citation::World { kind, id } => format!("{kind}:{id}"),
            Citation::GmailMessage { message_id } => format!("gmail:{message_id}"),
            Citation::CalendarEvent { event_id, start } => format!("event:{event_id} @ {start}"),
            Citation::SlackMessage { channel, ts } => format!("slack:{channel}#{ts}"),
            // Honest source-level anchor: the read that produced this hit, not a
            // fabricated item id. e.g. "gmail recent messages (search: launch)".
            Citation::CloudSource { read, query, .. } => {
                let q = query.trim();
                if q.is_empty() {
                    read.clone()
                } else {
                    format!("{read} (search: {q})")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A unified hit + the candidate it is built from
// ---------------------------------------------------------------------------

/// One MERGED, ATTRIBUTED, CITED hit in the unified result. `source` says which
/// source it came from; `citation` anchors it to the real underlying item;
/// `title`/`snippet` are the display text; `score` is the final blended rank
/// score (higher = more relevant); `ts` is the RFC3339 timestamp when the source
/// carries one (drives the recency component), else `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct UnifiedHit {
    pub source: Source,
    pub citation: Citation,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub ts: Option<String>,
}

/// A per-source CANDIDATE before the cross-source merge. The fan-out layer
/// produces these (one source at a time); [`fold`] blends relevance + recency
/// into the final [`UnifiedHit`] and ranks across all sources together.
///
/// `relevance` is the source's OWN relevance signal in whatever scale it ranks
/// on (BM25 / cosine / lexical-overlap), which differs across sources; [`fold`]
/// NORMALIZES per-source before merging so no one source's raw scale dominates.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub source: Source,
    pub citation: Citation,
    pub title: String,
    pub snippet: String,
    /// The source's own relevance score (>= 0). Normalized per-source in [`fold`].
    pub relevance: f64,
    /// RFC3339 timestamp when the source carries one (episodes / cloud items);
    /// `None` for the timeless sources (facts / world), and for docsearch.
    pub ts: Option<String>,
}

// ---------------------------------------------------------------------------
// Coverage — which sources searched vs skipped (with reason)
// ---------------------------------------------------------------------------

/// One skipped source + the honest reason it was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Skipped {
    pub source: Source,
    pub reason: SkipReason,
}

/// The HONEST coverage summary of a unified search: exactly which sources were
/// SEARCHED and which were SKIPPED (each with a reason). The caller renders this
/// so a user always knows the answer's reach ("searched files + memory; skipped
/// Gmail — not connected"). NEVER claims a skipped source was searched.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Coverage {
    /// Sources that were actually searched (in a stable order).
    pub searched: Vec<Source>,
    /// Sources that were skipped, each with the honest reason.
    pub skipped: Vec<Skipped>,
}

impl Coverage {
    /// One honest human sentence for the spoken / HUD coverage line. Reports the
    /// searched sources and, separately, each skipped source with its reason —
    /// never conflating the two.
    pub fn summary(&self) -> String {
        let searched: Vec<&str> = self.searched.iter().map(|s| s.label()).collect();
        let searched_part = if searched.is_empty() {
            "Searched no sources".to_string()
        } else {
            format!("Searched {}", searched.join(", "))
        };
        if self.skipped.is_empty() {
            return format!("{searched_part}.");
        }
        let skipped: Vec<String> = self
            .skipped
            .iter()
            .map(|s| format!("{} ({})", s.source.label(), s.reason.human()))
            .collect();
        format!("{searched_part}. Skipped {}.", skipped.join(", "))
    }
}

/// The complete result of a unified search: the merged + ranked + attributed +
/// cited hits, plus the honest coverage summary. An empty `hits` with a
/// non-empty `coverage.searched` is the HONEST "I searched these sources and
/// found nothing" result — never a fabricated hit.
#[derive(Debug, Clone, PartialEq)]
pub struct UnifiedResult {
    pub hits: Vec<UnifiedHit>,
    pub coverage: Coverage,
}

// ---------------------------------------------------------------------------
// The per-source fan-out inputs (already fetched by the live layer / mocked)
// ---------------------------------------------------------------------------

/// The cloud-source connection verdict + (when connected) the items the EXISTING
/// gated read-only read returned. A `None` payload means NOT CONNECTED
/// (`resolve_secret` returned `None`) -> the source is SKIPPED with
/// [`SkipReason::NotConnected`]. A `Some(items)` means CONNECTED and read; the
/// items (possibly empty) are ranked against the query.
///
/// This is the ONLY place the connected-gating crosses into the core: the live
/// layer resolves the secret and, only if `Some`, performs the existing gated
/// read and passes the resulting items here. The core never resolves a secret or
/// makes a network call — so a disconnected cloud source can never be searched.
#[derive(Debug, Clone, Default)]
pub struct CloudInput {
    /// `None` => not connected (skip with NotConnected). `Some` => connected; the
    /// items the gated read returned (may be empty -> searched, found nothing).
    pub items: Option<Vec<Candidate>>,
}

impl CloudInput {
    /// A connected cloud source carrying the items its gated read returned.
    pub fn connected(items: Vec<Candidate>) -> Self {
        Self { items: Some(items) }
    }

    /// A NOT-connected cloud source (resolve_secret returned None).
    pub fn not_connected() -> Self {
        Self { items: None }
    }
}

/// The on-device source's candidates, or `None` when the source could not be
/// searched at all (e.g. no file index built -> [`SkipReason::NoIndex`]). A
/// `Some(items)` (possibly empty) means searched. On-device sources are never
/// connection-gated; this only distinguishes "searched (maybe empty)" from
/// "could not search this source".
#[derive(Debug, Clone, Default)]
pub struct DeviceInput {
    pub items: Option<Vec<Candidate>>,
}

impl DeviceInput {
    pub fn searched(items: Vec<Candidate>) -> Self {
        Self { items: Some(items) }
    }

    /// The source had nothing to search (e.g. file index not built).
    pub fn no_index() -> Self {
        Self { items: None }
    }
}

/// All per-source inputs to one unified search, already fetched (or mocked). The
/// live layer fills these by doing the real reads; the tests inject mock values.
/// A `None` field means the caller did NOT request that source -> it is recorded
/// as SKIPPED with [`SkipReason::NotRequested`] (honest: we did not search it).
#[derive(Debug, Clone, Default)]
pub struct FanoutInputs {
    pub docsearch: Option<DeviceInput>,
    pub episodic: Option<DeviceInput>,
    pub facts: Option<DeviceInput>,
    pub world: Option<DeviceInput>,
    pub gmail: Option<CloudInput>,
    pub calendar: Option<CloudInput>,
    pub slack: Option<CloudInput>,
}

// ---------------------------------------------------------------------------
// MERGE + RANK + COVERAGE — the pure core
// ---------------------------------------------------------------------------

/// FOLD the per-source fan-out inputs into one ranked, attributed, cited result
/// + an honest coverage summary. PURE + deterministic (no clock, no network, no
///   embed call) so the entire merge/rank/coverage/citation contract is unit-
///   testable with injected mock inputs.
///
/// For each source:
///   * an on-device source with `Some(items)` is SEARCHED; with `None` it is
///     SKIPPED (NoIndex); absent it is SKIPPED (NotRequested);
///   * a cloud source with `Some(items)` is connected -> SEARCHED; with
///     `None` it is not connected -> SKIPPED (NotConnected); absent ->
///     SKIPPED (NotRequested).
///
/// The candidates from every searched source are then blended: each source's
/// `relevance` is NORMALIZED to [0,1] within its own source (so BM25's scale
/// never dominates cosine's), then combined with a deterministic RECENCY score
/// (newer ts -> higher, only for time-stamped sources) as
/// `(1-RECENCY_WEIGHT)*relevance_norm + RECENCY_WEIGHT*recency_norm`. The merged
/// hits are sorted by final score DESC, with a STABLE deterministic tie-break
/// (source order, then citation anchor) so the ranking is reproducible. Capped
/// at `k`.
pub fn fold(inputs: &FanoutInputs, k: usize) -> UnifiedResult {
    let k = k.clamp(1, UNIFIED_MAX_K);
    let mut coverage = Coverage::default();
    let mut all: Vec<Candidate> = Vec::new();

    // Stable iteration order: on-device first (always-available), then cloud.
    // This is also the deterministic tie-break order in the final sort.
    let device_sources = [
        (Source::Docsearch, &inputs.docsearch),
        (Source::Episodic, &inputs.episodic),
        (Source::Facts, &inputs.facts),
        (Source::World, &inputs.world),
    ];
    for (src, input) in device_sources {
        match input {
            Some(DeviceInput { items: Some(items) }) => {
                coverage.searched.push(src);
                all.extend(items.iter().cloned());
            }
            Some(DeviceInput { items: None }) => {
                coverage.skipped.push(Skipped {
                    source: src,
                    reason: SkipReason::NoIndex,
                });
            }
            None => {
                coverage.skipped.push(Skipped {
                    source: src,
                    reason: SkipReason::NotRequested,
                });
            }
        }
    }

    let cloud_sources = [
        (Source::Gmail, &inputs.gmail),
        (Source::Calendar, &inputs.calendar),
        (Source::Slack, &inputs.slack),
    ];
    for (src, input) in cloud_sources {
        match input {
            Some(CloudInput { items: Some(items) }) => {
                coverage.searched.push(src);
                all.extend(items.iter().cloned());
            }
            Some(CloudInput { items: None }) => {
                // Connected-check failed (resolve_secret -> None): skip honestly.
                coverage.skipped.push(Skipped {
                    source: src,
                    reason: SkipReason::NotConnected,
                });
            }
            None => {
                coverage.skipped.push(Skipped {
                    source: src,
                    reason: SkipReason::NotRequested,
                });
            }
        }
    }

    let hits = rank_candidates(all, k);
    UnifiedResult { hits, coverage }
}

/// STAGE TWO for unified search: after [`fold`] has blended + ranked the
/// cross-source hits, re-score the top shortlist with the on-device cross-encoder
/// (config-gated) and re-order by it — the two-stage stack applied to the FUSED
/// candidate set. Returns `(hits, reranked)`; `reranked=false` (the blended order
/// is kept unchanged) when the reranker is off / unavailable / the shortlist is
/// trivial, so the caller reports the ranking honestly. Each hit is reranked on
/// its display text (`title` + `snippet` — the best text a heterogeneous hit
/// carries; a cloud summary or fact has no fuller body here). The tail below the
/// bounded shortlist keeps its fused order. Runtime-gated only by the injected
/// `embedder` (mocks keep the trait default of no rerank), so the pure `fold` core
/// and its tests are untouched.
pub async fn rerank_hits(
    query: &str,
    mut hits: Vec<UnifiedHit>,
    embedder: &dyn crate::recall::Embedder,
) -> (Vec<UnifiedHit>, bool) {
    if hits.len() < 2 || !embedder.rerank_enabled() {
        return (hits, false);
    }
    let head_n = hits.len().min(crate::recall::DEFAULT_RERANK_K);
    let passages: Vec<String> = hits[..head_n]
        .iter()
        .map(|h| {
            if h.snippet.trim().is_empty() {
                h.title.clone()
            } else {
                format!("{}\n{}", h.title, h.snippet)
            }
        })
        .collect();
    match crate::recall::rerank_permutation(query, &passages, embedder).await {
        Some(perm) => {
            let tail = hits.split_off(head_n); // `hits` is now the head [0, head_n)
            let mut reordered: Vec<UnifiedHit> =
                perm.iter().map(|&i| hits[i].clone()).collect();
            reordered.extend(tail);
            (reordered, true)
        }
        None => (hits, false),
    }
}

/// Blend + rank candidates from all sources into the final hit list. PURE +
/// deterministic. Per-source relevance is normalized to [0,1] (max-normalization
/// within each source), recency is normalized across the whole candidate set,
/// and the two are blended by [`RECENCY_WEIGHT`]. Sorted by final score DESC with
/// a stable tie-break (source order, then citation anchor). Capped at `k`.
fn rank_candidates(candidates: Vec<Candidate>, k: usize) -> Vec<UnifiedHit> {
    if candidates.is_empty() {
        return Vec::new();
    }

    // 1. Per-source max relevance, for max-normalization within each source.
    let mut per_source_max: std::collections::HashMap<Source, f64> =
        std::collections::HashMap::new();
    for c in &candidates {
        let e = per_source_max.entry(c.source).or_insert(0.0);
        if c.relevance > *e {
            *e = c.relevance;
        }
    }

    // 2. Recency normalization across the whole set. Only time-stamped
    //    candidates carry a parseable ts; the rest contribute 0 recency (pure
    //    relevance — we never invent a recency signal for a timeless source).
    //    We rank ts lexically over canonical RFC3339 (the stores write UTC), so
    //    no clock is read — the newest among the candidates gets recency 1.0,
    //    the oldest 0.0; a single timestamp gets 1.0.
    // Only the candidates that SURVIVE the relevance filter (step 3) shape the
    // recency scale. Including a dropped (relevance <= 0) candidate's timestamp
    // shifts the normalization denominator + positions of the surviving hits, which
    // can reorder — even flip the top — results that ARE returned, for an item that
    // never appears in the output.
    let mut timestamps: Vec<String> = candidates
        .iter()
        .filter(|c| c.relevance.is_finite() && c.relevance > 0.0)
        .filter_map(|c| c.ts.clone())
        .collect();
    timestamps.sort_unstable();
    timestamps.dedup();
    let recency_of = |ts: Option<&str>| -> f64 {
        let Some(ts) = ts else { return 0.0 };
        if timestamps.len() <= 1 {
            return 1.0;
        }
        // position 0 = oldest .. last = newest -> normalize to [0,1].
        match timestamps.iter().position(|t| t == ts) {
            Some(pos) => pos as f64 / (timestamps.len() - 1) as f64,
            None => 0.0,
        }
    };

    // 3. Blend, drop non-positive (irrelevant) candidates so we never surface a
    //    zero-relevance item, build the cited hit.
    let mut hits: Vec<UnifiedHit> = candidates
        .into_iter()
        .filter(|c| c.relevance.is_finite() && c.relevance > 0.0)
        .map(|c| {
            let src_max = per_source_max.get(&c.source).copied().unwrap_or(0.0);
            let rel_norm = if src_max > 0.0 { c.relevance / src_max } else { 0.0 };
            let rec_norm = recency_of(c.ts.as_deref());
            let score = (1.0 - RECENCY_WEIGHT) * rel_norm + RECENCY_WEIGHT * rec_norm;
            UnifiedHit {
                source: c.source,
                citation: c.citation,
                title: c.title,
                snippet: c.snippet,
                score,
                ts: c.ts,
            }
        })
        .collect();

    // 4. Deterministic sort: score DESC, then source order ASC, then citation
    //    anchor ASC — fully reproducible (no float-equality ambiguity left to
    //    chance, no reliance on input order).
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| source_order(a.source).cmp(&source_order(b.source)))
            .then_with(|| a.citation.anchor().cmp(&b.citation.anchor()))
    });
    hits.truncate(k);
    hits
}

/// The stable ordinal of a source for the deterministic tie-break (on-device
/// first, then cloud — matching the fan-out iteration order).
fn source_order(s: Source) -> u8 {
    match s {
        Source::Docsearch => 0,
        Source::Episodic => 1,
        Source::Facts => 2,
        Source::World => 3,
        Source::Gmail => 4,
        Source::Calendar => 5,
        Source::Slack => 6,
    }
}

// ---------------------------------------------------------------------------
// Adapters — turn each source's native result into ranked Candidates
// ---------------------------------------------------------------------------
//
// These are the bridges the LIVE fan-out layer uses to convert each source's own
// result type into the uniform `Candidate` (source + citation + relevance + ts).
// They are PURE (no IO) so they are unit-tested directly; the live layer pairs
// each with the source's actual read.

/// Adapt on-device DOCSEARCH hits ([`DocHit`]) into candidates. The hit already
/// carries its own relevance score and a real file citation; docsearch chunks
/// have no per-hit timestamp, so they are relevance-only (ts = None — honest, we
/// do not invent a recency for a file chunk).
pub fn docsearch_candidates(hits: &[DocHit]) -> Vec<Candidate> {
    hits.iter()
        .map(|h| Candidate {
            source: Source::Docsearch,
            citation: Citation::File {
                path: h.file_path.clone(),
                byte_offset: h.byte_offset,
            },
            title: h.file_path.clone(),
            snippet: h.snippet.clone(),
            relevance: h.score,
            ts: None,
        })
        .collect()
}

/// Adapt agent-scoped EPISODES into candidates, ranking them against `query`
/// with the SAME lexical BM25 ranker the rest of recall uses (so a cross-source
/// merge compares like with like). Each episode carries its real timestamp (the
/// recency anchor) and an episode citation. SCOPING is the caller's job — pass
/// only the agent-scoped episodes the agent-scoped read returned; this never
/// widens that.
pub fn episodic_candidates(episodes: &[Episode], query: &str) -> Vec<Candidate> {
    if episodes.is_empty() {
        return Vec::new();
    }
    let facts: Vec<Fact> = episodes
        .iter()
        .map(|ep| Fact {
            key: ep.summary.clone(),
            value: format!(
                "{} {} {}",
                ep.utterance_redacted,
                ep.salient_entities.join(" "),
                ep.topic
            ),
        })
        .collect();
    let scores = lexical_scores(query, &facts);
    episodes
        .iter()
        .zip(scores)
        .map(|(ep, score)| Candidate {
            source: Source::Episodic,
            citation: Citation::Episode {
                ts: ep.ts.clone(),
                id: ep.id,
            },
            title: ep.summary.clone(),
            snippet: if ep.summary.trim().is_empty() {
                ep.utterance_redacted.clone()
            } else {
                ep.summary.clone()
            },
            relevance: score,
            ts: Some(ep.ts.clone()),
        })
        .collect()
}

/// Adapt agent-scoped FACTS (key,value rows) into candidates, ranked against
/// `query` by the shared lexical BM25 ranker. Cited to the fact key. Facts carry
/// no timestamp here, so they are relevance-only (ts = None). SCOPING is the
/// caller's job — pass only `agent_scoped_facts`; this never widens it.
pub fn facts_candidates(rows: &[(String, String)], query: &str) -> Vec<Candidate> {
    if rows.is_empty() {
        return Vec::new();
    }
    let facts: Vec<Fact> = rows
        .iter()
        .map(|(k, v)| Fact {
            key: k.clone(),
            value: v.clone(),
        })
        .collect();
    let scores = lexical_scores(query, &facts);
    rows.iter()
        .zip(scores)
        .map(|((key, value), score)| Candidate {
            source: Source::Facts,
            citation: Citation::Fact { key: key.clone() },
            title: key.clone(),
            snippet: value.clone(),
            relevance: score,
            ts: None,
        })
        .collect()
}

/// Adapt SHARED world-model entities + relationships into candidates, ranked by
/// the shared lexical BM25 ranker against `query`. Cited to the entity (type:id)
/// or the relationship (from-relation-to). World rows carry no timestamp, so
/// they are relevance-only (ts = None). The world model is the SHARED tier — it
/// inherently holds no agent's private notes.
pub fn world_candidates(
    entities: &[Entity],
    relationships: &[Relationship],
    query: &str,
) -> Vec<Candidate> {
    let mut facts: Vec<Fact> = Vec::new();
    // Entities: searchable = name + attributes.
    for e in entities {
        let attrs: String = e
            .attributes
            .iter()
            .map(|(a, v)| format!("{a} {v}"))
            .collect::<Vec<_>>()
            .join(" ");
        facts.push(Fact {
            key: format!("{}:{}", e.entity_type.as_str(), e.id),
            value: format!("{} {}", e.name, attrs),
        });
    }
    let entity_count = entities.len();
    // Relationships: searchable = from + relation + to + value.
    for r in relationships {
        facts.push(Fact {
            key: format!("{}-{}-{}", r.from, r.relation, r.to),
            value: format!("{} {} {} {}", r.from, r.relation, r.to, r.value),
        });
    }
    if facts.is_empty() {
        return Vec::new();
    }
    let scores = lexical_scores(query, &facts);

    let mut out: Vec<Candidate> = Vec::new();
    for (i, e) in entities.iter().enumerate() {
        let attrs: String = e
            .attributes
            .iter()
            .map(|(a, v)| format!("{a}={v}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push(Candidate {
            source: Source::World,
            citation: Citation::World {
                kind: e.entity_type.as_str().to_string(),
                id: e.id.clone(),
            },
            title: e.name.clone(),
            snippet: if attrs.is_empty() { e.name.clone() } else { attrs },
            relevance: scores[i],
            ts: None,
        });
    }
    for (j, r) in relationships.iter().enumerate() {
        out.push(Candidate {
            source: Source::World,
            citation: Citation::World {
                kind: "relationship".to_string(),
                id: format!("{}-{}-{}", r.from, r.relation, r.to),
            },
            title: format!("{} {} {}", r.from, r.relation, r.to),
            snippet: if r.value.is_empty() {
                format!("{} {} {}", r.from, r.relation, r.to)
            } else {
                r.value.clone()
            },
            relevance: scores[entity_count + j],
            ts: None,
        });
    }
    out
}

/// Score `facts` against `query` with the shared BM25 [`LexicalProvider`] — the
/// SAME ranker recall/docsearch use, so cross-source candidates are comparable.
/// Returns one score per fact in input order. PURE.
fn lexical_scores(query: &str, facts: &[Fact]) -> Vec<f64> {
    let provider = LexicalProvider {
        params: Bm25Params::default(),
    };
    provider.score(query, facts)
}

/// Re-export of the shared cosine, in case a future neural cross-source ranker
/// wants to rank cloud-item embeddings here with the EXACT same implementation
/// (one shared, degenerate-safe cosine — never a second copy). Currently the
/// cross-source merge is lexical-relevance + recency; this keeps the door open
/// without a second implementation.
#[allow(dead_code)]
pub(crate) fn unified_cosine(a: &[f64], b: &[f64]) -> f64 {
    cosine_similarity(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world_model::EntityType;

    // -- helpers -------------------------------------------------------------

    fn ep(id: i64, ts: &str, summary: &str, utterance: &str, topic: &str) -> Episode {
        Episode {
            id,
            ts: ts.to_string(),
            agent_namespace: "agent.darwin".to_string(),
            utterance_redacted: utterance.to_string(),
            topic: topic.to_string(),
            salient_entities: vec![],
            outcome: "ok".to_string(),
            summary: summary.to_string(),
        }
    }

    fn doc(path: &str, snippet: &str, score: f64) -> DocHit {
        DocHit {
            file_path: path.to_string(),
            root: "/root".to_string(),
            byte_offset: 0,
            snippet: snippet.to_string(),
            score,
        }
    }

    fn cloud_msg(source: Source, id: &str, ts: &str, text: &str, relevance: f64) -> Candidate {
        let citation = match source {
            Source::Gmail => Citation::GmailMessage {
                message_id: id.to_string(),
            },
            Source::Calendar => Citation::CalendarEvent {
                event_id: id.to_string(),
                start: ts.to_string(),
            },
            Source::Slack => Citation::SlackMessage {
                channel: "general".to_string(),
                ts: ts.to_string(),
            },
            _ => panic!("cloud_msg only for cloud sources"),
        };
        Candidate {
            source,
            citation,
            title: text.to_string(),
            snippet: text.to_string(),
            relevance,
            ts: Some(ts.to_string()),
        }
    }

    // -- MERGE + ATTRIBUTE + CITE -------------------------------------------

    #[test]
    fn merges_attributes_and_cites_across_sources() {
        // One on-device source (docsearch) + one connected cloud source (gmail).
        let inputs = FanoutInputs {
            docsearch: Some(DeviceInput::searched(docsearch_candidates(&[
                doc("/notes/launch.md", "the launch plan is ready", 3.0),
            ]))),
            episodic: Some(DeviceInput::searched(vec![])),
            facts: Some(DeviceInput::searched(vec![])),
            world: Some(DeviceInput::searched(vec![])),
            gmail: Some(CloudInput::connected(vec![cloud_msg(
                Source::Gmail,
                "msg-1",
                "2026-06-15T10:00:00Z",
                "re: launch plan",
                2.0,
            )])),
            calendar: None,
            slack: None,
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        assert_eq!(res.hits.len(), 2, "both real hits merged");
        // Each hit is ATTRIBUTED to its source and CITES a real item.
        let by_src: Vec<Source> = res.hits.iter().map(|h| h.source).collect();
        assert!(by_src.contains(&Source::Docsearch));
        assert!(by_src.contains(&Source::Gmail));
        for h in &res.hits {
            match (&h.source, &h.citation) {
                (Source::Docsearch, Citation::File { path, .. }) => {
                    assert_eq!(path, "/notes/launch.md")
                }
                (Source::Gmail, Citation::GmailMessage { message_id }) => {
                    assert_eq!(message_id, "msg-1")
                }
                other => panic!("hit not cited to its real source: {other:?}"),
            }
        }
    }

    // -- COVERAGE: connected vs not-connected vs not-requested --------------

    #[test]
    fn unconnected_cloud_source_is_skipped_with_reason_not_dropped() {
        let inputs = FanoutInputs {
            docsearch: Some(DeviceInput::searched(vec![])),
            episodic: Some(DeviceInput::searched(vec![])),
            facts: Some(DeviceInput::searched(vec![])),
            world: Some(DeviceInput::searched(vec![])),
            // Gmail connected, Slack NOT connected, Calendar not requested.
            gmail: Some(CloudInput::connected(vec![])),
            slack: Some(CloudInput::not_connected()),
            calendar: None,
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        // Slack appears in skipped with NotConnected — NOT silently dropped.
        let slack_skip = res
            .coverage
            .skipped
            .iter()
            .find(|s| s.source == Source::Slack)
            .expect("slack must be reported as skipped, not dropped");
        assert_eq!(slack_skip.reason, SkipReason::NotConnected);
        // Calendar (absent) -> NotRequested, also honestly recorded.
        let cal_skip = res
            .coverage
            .skipped
            .iter()
            .find(|s| s.source == Source::Calendar)
            .expect("calendar must be reported as skipped");
        assert_eq!(cal_skip.reason, SkipReason::NotRequested);
        // Gmail (connected, empty) -> SEARCHED (searched-but-empty != skipped).
        assert!(res.coverage.searched.contains(&Source::Gmail));
        // The summary names the searched set AND the skip reason, never conflating.
        let s = res.coverage.summary();
        assert!(s.contains("Gmail"), "searched gmail must be named: {s}");
        assert!(
            s.contains("not connected"),
            "skip reason must be honest in copy: {s}"
        );
    }

    #[test]
    fn on_device_no_index_is_skipped_with_no_index_reason() {
        let inputs = FanoutInputs {
            docsearch: Some(DeviceInput::no_index()), // index not built
            episodic: Some(DeviceInput::searched(vec![])),
            facts: Some(DeviceInput::searched(vec![])),
            world: Some(DeviceInput::searched(vec![])),
            gmail: None,
            calendar: None,
            slack: None,
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        let doc_skip = res
            .coverage
            .skipped
            .iter()
            .find(|s| s.source == Source::Docsearch)
            .expect("docsearch must be reported skipped when no index");
        assert_eq!(doc_skip.reason, SkipReason::NoIndex);
        assert!(!res.coverage.searched.contains(&Source::Docsearch));
    }

    // -- EMPTY is HONEST -----------------------------------------------------

    #[test]
    fn empty_fanout_returns_no_hits_but_honest_coverage() {
        // Every on-device source searched but returned nothing; cloud not connected.
        let inputs = FanoutInputs {
            docsearch: Some(DeviceInput::searched(vec![])),
            episodic: Some(DeviceInput::searched(vec![])),
            facts: Some(DeviceInput::searched(vec![])),
            world: Some(DeviceInput::searched(vec![])),
            gmail: Some(CloudInput::not_connected()),
            calendar: Some(CloudInput::not_connected()),
            slack: Some(CloudInput::not_connected()),
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        assert!(res.hits.is_empty(), "no fabricated hit on an empty fan-out");
        // But coverage still HONESTLY says the on-device sources WERE searched.
        assert_eq!(res.coverage.searched.len(), 4);
        assert!(res.coverage.searched.contains(&Source::Facts));
        // And all three cloud sources are skipped (not connected), not dropped.
        assert_eq!(res.coverage.skipped.len(), 3);
        assert!(res
            .coverage
            .skipped
            .iter()
            .all(|s| s.reason == SkipReason::NotConnected));
    }

    #[test]
    fn no_candidate_with_zero_relevance_is_surfaced() {
        // A fact that does not match the query gets BM25 score 0 -> never a hit
        // (no fabrication of an irrelevant item).
        let cands = facts_candidates(
            &[("user.car".to_string(), "I drive a blue Subaru".to_string())],
            "quantum chromodynamics",
        );
        let inputs = FanoutInputs {
            facts: Some(DeviceInput::searched(cands)),
            ..Default::default()
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        assert!(
            res.hits.is_empty(),
            "a zero-relevance fact must not surface as a hit"
        );
        // Facts WAS searched (it just found nothing) — honest coverage.
        assert!(res.coverage.searched.contains(&Source::Facts));
    }

    // -- RANKING: relevant outranks; recency shapes ties; deterministic ------

    #[test]
    fn a_relevant_fact_outranks_an_irrelevant_one() {
        let cands = facts_candidates(
            &[
                ("user.car".to_string(), "I drive a blue Subaru Outback".to_string()),
                ("user.pet".to_string(), "a corgi named Watson".to_string()),
            ],
            "what do you remember about my car",
        );
        let inputs = FanoutInputs {
            facts: Some(DeviceInput::searched(cands)),
            ..Default::default()
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        assert!(!res.hits.is_empty());
        // The car fact must rank first; the pet fact (no "car" term) scores 0 and
        // is dropped.
        assert_eq!(res.hits[0].title, "user.car");
        assert!(res.hits.iter().all(|h| h.citation != Citation::Fact {
            key: "user.pet".to_string()
        }));
    }

    #[test]
    fn recency_breaks_ties_among_equally_relevant_hits() {
        // Two gmail messages with IDENTICAL relevance but different ts: the newer
        // ranks higher (recency component).
        let inputs = FanoutInputs {
            gmail: Some(CloudInput::connected(vec![
                cloud_msg(Source::Gmail, "old", "2026-06-01T10:00:00Z", "budget review", 1.0),
                cloud_msg(Source::Gmail, "new", "2026-06-15T10:00:00Z", "budget review", 1.0),
            ])),
            ..Default::default()
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        assert_eq!(res.hits.len(), 2);
        // Newer message first because recency breaks the relevance tie.
        match &res.hits[0].citation {
            Citation::GmailMessage { message_id } => assert_eq!(message_id, "new"),
            other => panic!("expected newer gmail first, got {other:?}"),
        }
    }

    /// REGRESSION: a dropped (relevance <= 0) candidate must NOT reorder the hits
    /// that ARE returned. The recency scale was built from ALL candidates (including
    /// dropped ones), so a never-surfaced item shifted the normalization and could
    /// flip the order of the real results. The scale is now derived only from the
    /// surviving hits, so adding/removing a dropped candidate leaves the order
    /// unchanged — asserted independently of the exact recency weight.
    #[test]
    fn a_dropped_zero_relevance_candidate_does_not_reorder_the_surviving_hits() {
        let a = cloud_msg(Source::Gmail, "A", "2026-06-20T00:00:00Z", "budget", 0.87);
        let b = cloud_msg(Source::Gmail, "B", "2026-06-15T00:00:00Z", "budget", 1.00);
        let d = cloud_msg(Source::Gmail, "D", "2026-06-10T00:00:00Z", "budget", 0.0);
        let order = |cands: Vec<Candidate>| -> Vec<String> {
            fold(
                &FanoutInputs {
                    gmail: Some(CloudInput::connected(cands)),
                    ..Default::default()
                },
                UNIFIED_DEFAULT_K,
            )
            .hits
            .iter()
            .filter_map(|h| match &h.citation {
                Citation::GmailMessage { message_id } => Some(message_id.clone()),
                _ => None,
            })
            .collect()
        };
        let without_d = order(vec![a.clone(), b.clone()]);
        let with_d = order(vec![a, b, d]);
        assert_eq!(
            without_d, with_d,
            "a dropped candidate must not reorder the surviving hits"
        );
        assert!(
            !with_d.contains(&"D".to_string()),
            "the zero-relevance candidate is dropped from the output"
        );
    }

    #[test]
    fn ranking_is_deterministic_and_reproducible() {
        let build = || FanoutInputs {
            docsearch: Some(DeviceInput::searched(docsearch_candidates(&[
                doc("/a.md", "alpha launch", 2.0),
                doc("/b.md", "beta launch", 2.0),
            ]))),
            facts: Some(DeviceInput::searched(facts_candidates(
                &[("topic.launch".to_string(), "the launch is on track".to_string())],
                "launch",
            ))),
            ..Default::default()
        };
        let r1 = fold(&build(), UNIFIED_DEFAULT_K);
        let r2 = fold(&build(), UNIFIED_DEFAULT_K);
        assert_eq!(r1, r2, "identical inputs must produce identical output");
        // The tie-break is stable (source order then citation anchor): the two
        // equally-scored docsearch hits come out in a fixed order.
        let anchors: Vec<String> = r1
            .hits
            .iter()
            .filter(|h| h.source == Source::Docsearch)
            .map(|h| h.citation.anchor())
            .collect();
        assert_eq!(anchors, vec!["/a.md (offset 0)", "/b.md (offset 0)"]);
    }

    // -- AGENT-SCOPING PRESERVED --------------------------------------------

    #[test]
    fn agent_scoping_preserved_a_never_sees_bs_private_items() {
        // The fan-out layer passes ONLY the agent-scoped read for the active
        // agent. Here we simulate agent A's fan-out: it received only A's own
        // episode + a shared fact. Agent B's private episode/fact were NEVER in
        // the input (the agent-scoped read excluded them at the Db layer), so
        // they cannot appear — this module never widens scope.
        let a_episodes = vec![ep(
            1,
            "2026-06-15T10:00:00Z",
            "A discussed the rocket launch",
            "rocket launch",
            "conversation",
        )];
        let a_facts = vec![("user.shared".to_string(), "the launch is shared knowledge".to_string())];
        let inputs = FanoutInputs {
            episodic: Some(DeviceInput::searched(episodic_candidates(&a_episodes, "launch"))),
            facts: Some(DeviceInput::searched(facts_candidates(&a_facts, "launch"))),
            ..Default::default()
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        // Only A's own + shared items surface; nothing of B's exists in the result.
        assert!(!res.hits.is_empty());
        for h in &res.hits {
            // Every citation traces to an item that WAS in A's scoped input.
            match &h.citation {
                Citation::Episode { id, .. } => assert_eq!(*id, 1, "only A's episode"),
                Citation::Fact { key } => assert_eq!(key, "user.shared", "only the shared fact"),
                other => panic!("unexpected citation in scoped result: {other:?}"),
            }
        }
        // Explicitly: a hypothetical B private fact key never appears.
        assert!(res.hits.iter().all(|h| h.citation
            != Citation::Fact {
                key: "agent.friday.secret".to_string()
            }));
    }

    // -- ADAPTERS produce correct citations ---------------------------------

    #[test]
    fn world_candidates_cite_entities_and_relationships() {
        let entities = vec![Entity {
            entity_type: EntityType::Project,
            id: "darwin".to_string(),
            name: "Project DARWIN".to_string(),
            attributes: vec![("status".to_string(), "active".to_string())],
        }];
        let relationships = vec![Relationship {
            from: "darwin".to_string(),
            relation: "owned_by".to_string(),
            to: "darwin".to_string(),
            value: String::new(),
        }];
        let cands = world_candidates(&entities, &relationships, "darwin");
        assert_eq!(cands.len(), 2);
        // Entity cited as project:darwin.
        assert!(cands.iter().any(|c| c.citation
            == Citation::World {
                kind: "project".to_string(),
                id: "darwin".to_string()
            }));
        // Relationship cited as a relationship anchor.
        assert!(cands.iter().any(|c| matches!(
            &c.citation,
            Citation::World { kind, .. } if kind == "relationship"
        )));
    }

    #[test]
    fn k_caps_the_merged_result() {
        let many: Vec<DocHit> = (0..20)
            .map(|i| doc(&format!("/f{i}.md"), "launch plan", 5.0 - i as f64 * 0.1))
            .collect();
        let inputs = FanoutInputs {
            docsearch: Some(DeviceInput::searched(docsearch_candidates(&many))),
            ..Default::default()
        };
        let res = fold(&inputs, 3);
        assert_eq!(res.hits.len(), 3, "result capped at k");
    }

    #[test]
    fn cloud_source_citation_anchor_names_the_read_not_a_fake_item_id() {
        // The honest source-level cloud citation: the anchor names the gated read
        // + the query the user can reproduce — never a fabricated message/event id.
        let c = Citation::CloudSource {
            source: Source::Gmail,
            read: "gmail recent messages".to_string(),
            query: "launch".to_string(),
        };
        assert_eq!(c.anchor(), "gmail recent messages (search: launch)");
        // An empty query degrades to just the read label (still honest, no fake id).
        let c2 = Citation::CloudSource {
            source: Source::Slack,
            read: "slack channel list".to_string(),
            query: "  ".to_string(),
        };
        assert_eq!(c2.anchor(), "slack channel list");
        // It is NOT mistakable for a per-item anchor.
        assert!(!c.anchor().starts_with("gmail:"));
        assert!(!c2.anchor().contains('#'));
    }

    #[test]
    fn cloud_source_hit_merges_and_attributes_like_any_other() {
        // A source-level cloud candidate still merges/ranks/attributes correctly:
        // it is a real hit (the read returned content), cited to the read.
        let cloud = Candidate {
            source: Source::Gmail,
            citation: Citation::CloudSource {
                source: Source::Gmail,
                read: "gmail recent messages".to_string(),
                query: "launch".to_string(),
            },
            title: "Gmail".to_string(),
            snippet: "3 recent message(s) about the launch".to_string(),
            relevance: 1.5,
            ts: None,
        };
        let inputs = FanoutInputs {
            gmail: Some(CloudInput::connected(vec![cloud])),
            ..Default::default()
        };
        let res = fold(&inputs, UNIFIED_DEFAULT_K);
        assert_eq!(res.hits.len(), 1);
        assert_eq!(res.hits[0].source, Source::Gmail);
        assert!(matches!(
            &res.hits[0].citation,
            Citation::CloudSource { read, .. } if read == "gmail recent messages"
        ));
        assert!(res.coverage.searched.contains(&Source::Gmail));
    }

    #[test]
    fn source_tokens_and_skip_reasons_are_stable() {
        assert_eq!(Source::Docsearch.as_str(), "docsearch");
        assert_eq!(Source::Gmail.as_str(), "gmail");
        assert_eq!(SkipReason::NotConnected.as_str(), "not_connected");
        assert_eq!(SkipReason::NoIndex.as_str(), "no_index");
        assert!(Source::Facts.is_on_device());
        assert!(!Source::Slack.is_on_device());
    }
}
