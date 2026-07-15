//! KNOWLEDGE GRAPH FROM DOCUMENTS — mine the user's OWN indexed files for the
//! structured ENTITIES and RELATIONSHIPS that populate the shared World Model.
//!
//! This is the WRITE-from-documents counterpart to [`crate::docsearch`] (which
//! READS indexed chunks for cited search) and [`crate::world_model`] (the shared,
//! bounded, structured picture every agent reasons over). The graph build walks
//! the chunks the confined, allowlisted indexer already produced, runs a
//! pluggable [`Extractor`] over each chunk, and UPSERTs the grounded results into
//! the SHARED `user.world.*` tier — provenance-tagged, deduped, and bounded.
//!
//! ## The CONTRACT (non-negotiable — honesty first)
//!   * GROUNDED, NEVER FABRICATED. Every entity/relationship the build writes is
//!     returned by the extractor with a real SOURCE SPAN inside a real indexed
//!     chunk. Entity-less text yields NOTHING. There is no path that invents an
//!     entity the document text did not contain.
//!   * PROVENANCE. Each written entity carries a `source` attribute (the citing
//!     `file:offset`), so a user can trace any node back to the exact place it was
//!     mined from. The build only ever sees chunks the confined indexer produced,
//!     so a source is always an allowlisted file.
//!   * HEURISTIC, said plainly. The shipped [`DeterministicExtractor`] is a
//!     CONSERVATIVE pattern matcher (capitalized noun phrases + a few cue words +
//!     date shapes), NOT a trained NER. It deliberately prefers to MISS over to
//!     invent. The richer [`Extractor`] seam (an LLM-backed extractor) is
//!     RUNTIME-GATED and the deterministic one is always the fallback; the seam is
//!     never exercised by a test (a test that hit a model/socket would fail).
//!   * SHARED TIER ONLY. The build writes via [`crate::world_model::set_attribute`]
//!     / [`set_relationship`], which compose only `user.world.*` keys — so it can
//!     NEVER write an agent's private `agent.<ns>.*` namespace, and a runaway
//!     extractor cannot grow the model past [`crate::world_model::MAX_ENTITIES`] /
//!     `MAX_RELATIONS` (a NEW node past the cap is refused, honestly skipped).
//!   * DEDUP. Two chunks naming the same entity collapse to ONE node (the slug is
//!     stable); a re-run merges rather than duplicates. The source attribute keeps
//!     the FIRST grounding (re-running is idempotent, never a churned provenance).
//!   * ON by default but INERT WITHOUT INDEXED DOCS. Gated by
//!     `[docsearch].build_graph` (ships true) on top of the `[docsearch].enabled`
//!     master switch (also true) — it runs only over chunks the confined indexer
//!     already produced, so it does nothing until docsearch has roots + an index.
//!
//! Nothing here speaks, acts, or reaches the network. It reads stored chunks and
//! writes the shared world tier. The extraction is PURE; only the OPTIONAL LLM
//! seam would make a runtime/MLX-gated call, and it is never called in tests.

use anyhow::Result;

use crate::memory::Memory;
use crate::world_model::{self, EntityType};

/// How many chunks a single build pass will mine, regardless of the docsearch
/// store size. A generous-but-finite ceiling so a huge index can never make one
/// build pass unbounded; the world-model caps bound the WRITES on top of this.
pub const MAX_BUILD_CHUNKS: usize = 50_000;

/// The longest substring (in chars) the deterministic extractor treats as a
/// candidate entity NAME. A noun phrase longer than this is almost certainly a
/// run-on, not a name — clamped out conservatively.
const MAX_NAME_CHARS: usize = 64;

/// One entity the extractor found in a chunk, with the SOURCE SPAN that grounds
/// it. The span is (start_char, end_char) into the chunk text; combined with the
/// chunk's file + byte offset it yields a real provenance citation. Attributes
/// are extra (name, value) facts the extractor is confident about (the
/// deterministic one ships none beyond the implicit display name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedEntity {
    pub entity_type: EntityType,
    /// The human display name exactly as it appeared in the text (slugged on
    /// write by world_model, which also stores this verbatim as the `name` attr).
    pub name: String,
    /// (attribute, value) pairs the extractor is confident about. The
    /// deterministic extractor ships none; the LLM seam may add some.
    pub attributes: Vec<(String, String)>,
    /// Char span [start, end) within the chunk text — the real grounding offset.
    pub span: (usize, usize),
}

/// One relationship the extractor found, grounded by a source span. The endpoints
/// are NAMES (slugged on write); both endpoints must themselves be entities the
/// SAME extraction returned, so an edge never dangles to an un-grounded node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedRel {
    pub from_name: String,
    pub relation: String,
    pub to_name: String,
    /// Char span [start, end) within the chunk text grounding the co-occurrence.
    pub span: (usize, usize),
}

/// The full result of extracting over ONE chunk: the grounded entities and the
/// grounded relationships among them. An empty result is the honest answer for
/// entity-less text — never a fabricated node.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extraction {
    pub entities: Vec<ExtractedEntity>,
    pub relationships: Vec<ExtractedRel>,
}

/// The injectable EXTRACTOR seam. The shipped [`DeterministicExtractor`] is a
/// pure, hermetic heuristic; an LLM-backed extractor would implement this same
/// trait (runtime/MLX-gated) and the build loop would not change. Object-safe so
/// the build takes `&dyn Extractor` and a test can inject a mock without touching
/// any model or socket.
///
/// CONTRACT: `extract` must only ever return entities/relationships GROUNDED in
/// `chunk_text` (each carrying a real `span` into it). It must NEVER fabricate.
pub trait Extractor: Send + Sync {
    /// Extract grounded entities + relationships from one chunk's text. Pure for
    /// the deterministic impl; the LLM impl would make the one runtime-gated call.
    fn extract(&self, chunk_text: &str) -> Extraction;

    /// A short, honest token naming WHICH extractor ran — surfaced in telemetry so
    /// the HUD never implies a sophisticated NER when the heuristic ran.
    fn method(&self) -> &'static str;
}

// ===========================================================================
// THE DETERMINISTIC HEURISTIC EXTRACTOR (pure, hermetic, conservative)
// ===========================================================================

/// The shipped, model-free extractor. Conservative by design: it maps a small,
/// auditable set of surface patterns to the six [`EntityType`] kinds and a single
/// co-occurrence relationship. It will MISS plenty (it is a heuristic, not a
/// trained model) — that is the honest trade: better to miss than to invent.
///
/// What it recognizes, in priority order per matched phrase:
///   * DEADLINE — a date-shaped token (ISO `2026-06-30`, `06/30/2026`, or a
///     `Month DD[, YYYY]` form). The most specific shape, claimed first.
///   * TASK — a capitalized phrase directly preceded by a TODO/action cue
///     ("TODO:", "action item", "task:", "need to", "must", "should").
///   * PERSON — a capitalized phrase preceded by a person cue ("met with",
///     "spoke to", "owner:", "assigned to", "by") OR a Title-Case full name
///     (>=2 capitalized words) that is not otherwise claimed.
///   * PROJECT — a capitalized phrase preceded by a project cue ("project",
///     "the X project") or containing an ALL-CAPS code word.
///   * TOPIC — a remaining capitalized multi-word noun phrase (the catch-all for
///     a salient capitalized concept).
///     THREAD is reserved for conversational ingestion and is not mined from generic
///     document prose (claiming it would be a guess), so the deterministic extractor
///     never emits it — honest about what document text can ground.
///
/// RELATIONSHIPS: any two DISTINCT entities found in the SAME chunk get a single
/// `mentions` edge (from the earlier to the later by span). This is the weakest
/// honest claim — "these co-occur in your document" — not an asserted semantic
/// relation. Bounded so a dense chunk cannot emit a quadratic blow-up of edges.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeterministicExtractor;

/// Max distinct entities the deterministic extractor will emit from ONE chunk —
/// keeps a pathological chunk from flooding the build (the world-model cap is the
/// hard ceiling; this is the per-chunk politeness bound).
const MAX_ENTITIES_PER_CHUNK: usize = 32;
/// Max relationships emitted from one chunk (co-occurrence is O(n^2) in entities,
/// so cap it explicitly rather than relying on the per-chunk entity bound).
const MAX_RELS_PER_CHUNK: usize = 48;

impl Extractor for DeterministicExtractor {
    fn extract(&self, chunk_text: &str) -> Extraction {
        let entities = extract_entities(chunk_text);
        let relationships = co_occurrence_rels(&entities);
        Extraction {
            entities,
            relationships,
        }
    }

    fn method(&self) -> &'static str {
        "deterministic-heuristic"
    }
}

/// Person/Task/Project cue words that, when they immediately PRECEDE a
/// capitalized phrase, type it. Lowercased, matched on a word boundary.
const PERSON_CUES: &[&str] = &["with", "to", "by", "owner", "assigned", "from", "met", "spoke"];
const TASK_CUES: &[&str] = &["todo", "task", "action", "need", "must", "should", "fix", "ship"];
const PROJECT_CUES: &[&str] = &["project", "building", "shipping", "launch", "repo", "app"];

/// Extract the grounded entities from one chunk. Walks the text once, finding
/// capitalized noun phrases and date shapes, typing each by its surrounding cue,
/// and recording the real char span. Deterministic, pure, conservative.
fn extract_entities(text: &str) -> Vec<ExtractedEntity> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<ExtractedEntity> = Vec::new();

    // First pass: date-shaped DEADLINES (the most specific shape) so a date is
    // never mis-typed as a topic by the noun-phrase pass.
    for (start, end, raw) in find_date_spans(&chars) {
        push_entity(&mut out, EntityType::Deadline, &raw, (start, end));
        if out.len() >= MAX_ENTITIES_PER_CHUNK {
            return out;
        }
    }

    // Second pass: capitalized noun phrases, typed by the preceding cue word.
    for (start, end) in find_capitalized_phrases(&chars) {
        if out.len() >= MAX_ENTITIES_PER_CHUNK {
            break;
        }
        // Skip a phrase that overlaps an already-claimed date span.
        if out.iter().any(|e| spans_overlap(e.span, (start, end))) {
            continue;
        }
        // Drop a leading common sentence-starter ("The", "A", "This", ...) so a
        // phrase like "The Stuff" reduces to the real candidate "Stuff" and its
        // span shifts to match — keeping provenance pointed at the actual name.
        let (start, end) = trim_leading_common_word(&chars, start, end);
        if start >= end {
            continue;
        }
        let raw: String = chars[start..end].iter().collect();
        let name = raw.trim();
        if name.is_empty() {
            continue;
        }
        let cue = preceding_cue_word(&chars, start);
        let etype = classify_phrase(name, cue.as_deref());
        if let Some(etype) = etype {
            push_entity(&mut out, etype, name, (start, end));
        }
    }

    out
}

/// Add an entity to the accumulator IFF the name is usable and non-duplicate.
/// Dedup is by (type, slug) so two surface forms of the same entity in one chunk
/// collapse; the FIRST span wins (stable provenance within a chunk). A name that
/// slugs to nothing is dropped (never a fabricated node).
fn push_entity(
    out: &mut Vec<ExtractedEntity>,
    etype: EntityType,
    name: &str,
    span: (usize, usize),
) {
    let name = name.trim();
    let name: String = if name.chars().count() > MAX_NAME_CHARS {
        name.chars().take(MAX_NAME_CHARS).collect::<String>().trim().to_string()
    } else {
        name.to_string()
    };
    // Must slug to a stable id, else it is not a usable entity (drop, don't invent).
    let Some(slug) = world_model::slugify(&name) else {
        return;
    };
    if out
        .iter()
        .any(|e| e.entity_type == etype && world_model::slugify(&e.name).as_deref() == Some(slug.as_str()))
    {
        return; // already have this entity from an earlier span — dedup.
    }
    out.push(ExtractedEntity {
        entity_type: etype,
        name,
        attributes: Vec::new(),
        span,
    });
}

/// Decide the [`EntityType`] for a capitalized phrase given the (optional) cue
/// word immediately before it. Returns `None` to DROP the phrase (the
/// conservative default: an ambiguous lone capitalized word is not forced into a
/// type). Priority: explicit cue > multi-word proper name (Person) > all-caps code
/// (Project) > nothing.
fn classify_phrase(name: &str, cue: Option<&str>) -> Option<EntityType> {
    let word_count = name.split_whitespace().count();
    if let Some(cue) = cue {
        if TASK_CUES.contains(&cue) {
            return Some(EntityType::Task);
        }
        if PROJECT_CUES.contains(&cue) {
            return Some(EntityType::Project);
        }
        if PERSON_CUES.contains(&cue) {
            return Some(EntityType::Person);
        }
    }
    // A phrase that LOOKS like a code/project (contains an ALL-CAPS word of >=2
    // letters, e.g. "Project DARWIN", "ACME").
    if name
        .split_whitespace()
        .any(|w| w.chars().count() >= 2 && w.chars().all(|c| c.is_ascii_uppercase()))
    {
        return Some(EntityType::Project);
    }
    // A multi-word Title-Case phrase is most likely a proper name (Person).
    if word_count >= 2 {
        return Some(EntityType::Person);
    }
    // A lone single capitalized word with no cue is too ambiguous to type —
    // dropping it is the conservative, never-fabricate choice.
    None
}

/// Common capitalized sentence-starters that are NOT part of a proper name. A
/// phrase beginning with one of these has it stripped so "The Stuff" -> "Stuff"
/// (then dropped as ambiguous) and "The DARWIN Project" -> "DARWIN Project".
const LEADING_COMMON_WORDS: &[&str] = &[
    "the", "a", "an", "this", "that", "these", "those", "it", "we", "i", "they",
    "he", "she", "our", "my", "his", "her", "their", "its", "then", "and", "but",
    "so", "if", "as", "at", "in", "on", "for", "to", "of", "by", "with",
];

/// If the capitalized phrase `[start, end)` begins with a common sentence-starter
/// word followed by a space, advance `start` past it (and the space). Returns the
/// adjusted span. Only strips ONE leading common word (conservative).
fn trim_leading_common_word(chars: &[char], start: usize, end: usize) -> (usize, usize) {
    // read the first word
    let mut j = start;
    while j < end && (chars[j].is_alphanumeric() || chars[j] == '_') {
        j += 1;
    }
    let first: String = chars[start..j].iter().collect::<String>().to_lowercase();
    if LEADING_COMMON_WORDS.contains(&first.as_str()) && j < end && chars[j] == ' ' {
        return (j + 1, end);
    }
    (start, end)
}

/// Find date-shaped spans in the chunk: ISO `YYYY-MM-DD`, US `MM/DD/YYYY`, and
/// `Month DD[, YYYY]`. Returns (start_char, end_char, raw_text). Conservative:
/// only well-formed shapes match, so a stray number is never a "deadline".
fn find_date_spans(chars: &[char]) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        // ISO  YYYY-MM-DD  (exactly 4-2-2 digits with '-')
        if let Some(end) = match_iso_date(chars, i) {
            out.push((i, end, chars[i..end].iter().collect()));
            i = end;
            continue;
        }
        // US  M(M)/D(D)/YYYY
        if let Some(end) = match_slash_date(chars, i) {
            out.push((i, end, chars[i..end].iter().collect()));
            i = end;
            continue;
        }
        // Month DD[, YYYY]
        if let Some(end) = match_month_name_date(chars, i) {
            out.push((i, end, chars[i..end].iter().collect()));
            i = end;
            continue;
        }
        i += 1;
    }
    out
}

/// Match `YYYY-MM-DD` starting at `i` on a word boundary; return the end index.
fn match_iso_date(chars: &[char], i: usize) -> Option<usize> {
    if !at_word_boundary(chars, i) {
        return None;
    }
    let pat = [4usize, 2, 2];
    let mut j = i;
    for (k, &len) in pat.iter().enumerate() {
        for _ in 0..len {
            if j >= chars.len() || !chars[j].is_ascii_digit() {
                return None;
            }
            j += 1;
        }
        if k < 2 {
            if j >= chars.len() || chars[j] != '-' {
                return None;
            }
            j += 1;
        }
    }
    // must not be immediately followed by another digit (not a longer number)
    if j < chars.len() && chars[j].is_ascii_digit() {
        return None;
    }
    Some(j)
}

/// Match `M(M)/D(D)/YYYY` starting at `i`; return the end index.
fn match_slash_date(chars: &[char], i: usize) -> Option<usize> {
    if !at_word_boundary(chars, i) {
        return None;
    }
    let mut j = i;
    let take_num = |chars: &[char], start: usize, max: usize| -> Option<usize> {
        let mut e = start;
        while e < chars.len() && e - start < max && chars[e].is_ascii_digit() {
            e += 1;
        }
        if e > start {
            Some(e)
        } else {
            None
        }
    };
    j = take_num(chars, j, 2)?;
    if j >= chars.len() || chars[j] != '/' {
        return None;
    }
    j += 1;
    j = take_num(chars, j, 2)?;
    if j >= chars.len() || chars[j] != '/' {
        return None;
    }
    j += 1;
    // year: exactly 4 digits
    let ys = j;
    j = take_num(chars, j, 4)?;
    if j - ys != 4 {
        return None;
    }
    if j < chars.len() && chars[j].is_ascii_digit() {
        return None;
    }
    Some(j)
}

const MONTHS: &[&str] = &[
    "january", "february", "march", "april", "may", "june", "july", "august",
    "september", "october", "november", "december",
];

/// Match `Month DD[, YYYY]` (e.g. "June 30", "June 30, 2026") starting at `i`.
fn match_month_name_date(chars: &[char], i: usize) -> Option<usize> {
    if !at_word_boundary(chars, i) {
        return None;
    }
    // read an alphabetic word
    let mut j = i;
    while j < chars.len() && chars[j].is_ascii_alphabetic() {
        j += 1;
    }
    if j == i {
        return None;
    }
    let word: String = chars[i..j].iter().collect::<String>().to_lowercase();
    if !MONTHS.contains(&word.as_str()) {
        return None;
    }
    // single space
    if j >= chars.len() || chars[j] != ' ' {
        return None;
    }
    j += 1;
    // day: 1-2 digits
    let ds = j;
    while j < chars.len() && j - ds < 2 && chars[j].is_ascii_digit() {
        j += 1;
    }
    if j == ds {
        return None;
    }
    // optional ", YYYY"
    let mut end = j;
    if j < chars.len() && chars[j] == ',' {
        let mut k = j + 1;
        if k < chars.len() && chars[k] == ' ' {
            k += 1;
        }
        let ys = k;
        while k < chars.len() && k - ys < 4 && chars[k].is_ascii_digit() {
            k += 1;
        }
        if k - ys == 4 {
            end = k;
        }
    }
    Some(end)
}

/// Find capitalized noun-phrase spans: maximal runs of Capitalized words
/// (optionally joined by a single internal space) where each word starts with an
/// uppercase letter. Returns (start_char, end_char) char spans. Skips a
/// sentence-initial lone capitalized word (too likely a normal sentence start) by
/// only emitting it when it is multi-word or ALL-CAPS — but we keep lone words
/// here and let [`classify_phrase`] drop the ambiguous ones, so the boundary
/// logic stays simple and auditable.
fn find_capitalized_phrases(chars: &[char]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let n = chars.len();
    let mut i = 0usize;
    while i < n {
        if is_word_start_upper(chars, i) {
            let start = i;
            let mut end = i;
            loop {
                // consume the current capitalized word
                while end < n && (chars[end].is_alphanumeric() || chars[end] == '_') {
                    end += 1;
                }
                // lookahead: a single space then another capitalized word continues
                let k = end;
                if k < n && chars[k] == ' ' {
                    let after = k + 1;
                    if after < n && chars[after].is_uppercase() {
                        end = after;
                        continue;
                    }
                }
                break;
            }
            out.push((start, end));
            i = end + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// The lowercased cue WORD immediately before the phrase at `start` (skipping
/// punctuation/whitespace), or `None`. Drives [`classify_phrase`].
fn preceding_cue_word(chars: &[char], start: usize) -> Option<String> {
    if start == 0 {
        return None;
    }
    // walk back over separators (space, ':', etc.)
    let mut j = start;
    while j > 0 && !chars[j - 1].is_alphanumeric() {
        j -= 1;
    }
    if j == 0 {
        return None;
    }
    let word_end = j;
    while j > 0 && chars[j - 1].is_alphanumeric() {
        j -= 1;
    }
    if j == word_end {
        return None;
    }
    Some(chars[j..word_end].iter().collect::<String>().to_lowercase())
}

/// Build the co-occurrence relationships among entities found in ONE chunk: each
/// DISTINCT pair gets a single `mentions` edge (earlier span -> later span). The
/// weakest honest claim. Bounded by [`MAX_RELS_PER_CHUNK`].
fn co_occurrence_rels(entities: &[ExtractedEntity]) -> Vec<ExtractedRel> {
    let mut out = Vec::new();
    'outer: for a in 0..entities.len() {
        for b in (a + 1)..entities.len() {
            if out.len() >= MAX_RELS_PER_CHUNK {
                break 'outer;
            }
            let ea = &entities[a];
            let eb = &entities[b];
            // never relate an entity to itself (different surface, same slug+type)
            if ea.entity_type == eb.entity_type
                && world_model::slugify(&ea.name) == world_model::slugify(&eb.name)
            {
                continue;
            }
            // span anchoring the co-occurrence: from the earlier to the later.
            let (from, to, span) = if ea.span.0 <= eb.span.0 {
                (ea, eb, (ea.span.0, eb.span.1))
            } else {
                (eb, ea, (eb.span.0, ea.span.1))
            };
            out.push(ExtractedRel {
                from_name: from.name.clone(),
                relation: "mentions".to_string(),
                to_name: to.name.clone(),
                span,
            });
        }
    }
    out
}

// -- small char helpers ------------------------------------------------------

fn is_word_start_upper(chars: &[char], i: usize) -> bool {
    chars[i].is_uppercase() && at_word_boundary(chars, i)
}

/// True if index `i` begins a word (start of text, or preceded by a non-alnum).
fn at_word_boundary(chars: &[char], i: usize) -> bool {
    i == 0 || !chars[i - 1].is_alphanumeric()
}

fn spans_overlap(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

// ===========================================================================
// THE BUILD PASS — extract over chunks -> upsert into the shared world model
// ===========================================================================

/// What ONE build pass did, for the HUD telemetry + the intent's status line. All
/// counts are REAL outcomes of the bounded write path, so the copy is honest:
/// `entities_written`/`relationships_written` are the grounded nodes/edges that
/// actually landed; `skipped_at_cap` is how many were honestly refused because the
/// world model is at its bound (never silently grown wrong).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BuildStats {
    /// Chunks the extractor ran over.
    pub chunks_scanned: u64,
    /// Distinct entities upserted (new + merged) into the shared world tier.
    pub entities_written: u64,
    /// Distinct relationships upserted into the shared world tier.
    pub relationships_written: u64,
    /// Entities/relationships REFUSED because the world model is at its cap
    /// (honest skip — the model is never grown past its bound).
    pub skipped_at_cap: u64,
}

/// Build (or update) the knowledge graph from a set of already-indexed chunks.
/// Each chunk is `(file_path, byte_offset, chunk_text)` — exactly what
/// [`crate::docsearch::DocIndex::chunks_for_graph`] yields. For every chunk:
///   1. run the (injected) extractor -> grounded entities + relationships;
///   2. UPSERT each entity into the SHARED world tier with a PROVENANCE `source`
///      attribute (`file:offset`), via [`world_model::set_attribute`] (which
///      enforces the entity cap, refusing a NEW node past it — counted, not
///      silently dropped wrong);
///   3. UPSERT each relationship (both endpoints were grounded in the same chunk)
///      via [`world_model::set_relationship`] (which enforces the relation cap).
///      DEDUP is automatic: the slug is stable, so re-seeing an entity merges; the
///      `source` attribute is only set when the entity is NEW (first grounding wins),
///      so a re-run is idempotent and never churns provenance.
///
/// NEVER fabricates: it writes only what the extractor returned GROUNDED in a
/// chunk. NEVER writes a private namespace: every key is `user.world.*` by
/// construction (the world_model write API composes the key).
pub async fn build_from_chunks(
    memory: &Memory,
    extractor: &dyn Extractor,
    chunks: &[(String, i64, String)],
) -> Result<BuildStats> {
    let mut stats = BuildStats::default();
    for (file_path, byte_offset, text) in chunks.iter().take(MAX_BUILD_CHUNKS) {
        stats.chunks_scanned += 1;
        let extraction = extractor.extract(text);

        // Provenance string for this chunk: the citing file + the chunk offset
        // PLUS the entity's char span within the chunk, so a node traces back to
        // an exact place. Bounded by world_model's value cap on write.
        for ent in &extraction.entities {
            let source = format!(
                "{}:{} (chars {}-{})",
                file_path, byte_offset, ent.span.0, ent.span.1
            );
            // Set the PROVENANCE source attribute. This ALSO seeds the entity's
            // display name (world_model seeds `name` for a new entity), so a brand
            // new entity is created here; an existing one is merged (dedup).
            let already = world_model::query(memory, &ent.name).await?;
            let exists = entity_already_present(&already, ent);
            if exists {
                // Already grounded: this is a DEDUP/merge. Do not overwrite the
                // original `source` (first grounding wins — idempotent re-run),
                // and write any NEW confident attributes the extractor returned.
                for (a, v) in &ent.attributes {
                    if write_attr(memory, ent, a, v, &mut stats).await? {
                        stats.entities_written += 1;
                    }
                }
                continue;
            }
            // New entity: write its provenance `source` (which also seeds `name`).
            match world_model::set_attribute(memory, ent.entity_type, &ent.name, "source", &source)
                .await
            {
                Ok(_) => {
                    stats.entities_written += 1;
                    // Any extra confident attributes (LLM seam may add some).
                    for (a, v) in &ent.attributes {
                        let _ = write_attr(memory, ent, a, v, &mut stats).await?;
                    }
                }
                Err(e) => {
                    // The ONLY expected error here is the honest entity-cap refusal
                    // (world model full). Count it as a skip; never silently grow.
                    if is_cap_error(&e) {
                        stats.skipped_at_cap += 1;
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        for rel in &extraction.relationships {
            let detail = format!("source {}:{}", file_path, byte_offset);
            match world_model::set_relationship(
                memory,
                &rel.from_name,
                &rel.relation,
                &rel.to_name,
                &detail,
            )
            .await
            {
                Ok(_) => stats.relationships_written += 1,
                Err(e) => {
                    if is_cap_error(&e) {
                        stats.skipped_at_cap += 1;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
    }
    Ok(stats)
}

/// Write one extra attribute on an entity, returning whether it landed (false on
/// an honest cap refusal). Used for the optional confident attributes an LLM seam
/// might return; the deterministic extractor ships none.
async fn write_attr(
    memory: &Memory,
    ent: &ExtractedEntity,
    attr: &str,
    value: &str,
    stats: &mut BuildStats,
) -> Result<bool> {
    match world_model::set_attribute(memory, ent.entity_type, &ent.name, attr, value).await {
        Ok(_) => Ok(true),
        Err(e) => {
            if is_cap_error(&e) {
                stats.skipped_at_cap += 1;
                Ok(false)
            } else {
                Err(e)
            }
        }
    }
}

/// Whether the entity is ALREADY present in a queried slice of the world model
/// (same type + same slug). Drives dedup: an already-present entity is merged, not
/// re-sourced. The query is lexical by name, so this is a bounded, cheap check.
fn entity_already_present(state: &world_model::WorldState, ent: &ExtractedEntity) -> bool {
    let want = world_model::slugify(&ent.name);
    state.entities.iter().any(|e| {
        e.entity_type == ent.entity_type && Some(e.id.as_str()) == want.as_deref()
    })
}

/// Recognize the world-model's honest "at cap" refusal so the build counts it as a
/// skip rather than aborting. The world model phrases every cap refusal with the
/// word "cap" (entity + relationship), so a substring check is stable + local.
fn is_cap_error(e: &anyhow::Error) -> bool {
    e.to_string().contains("cap")
}

// ===========================================================================
// THE INTENT ENTRY POINT (gated; ON but inert without indexed docs; routed to Mnemosyne/Pepper)
// ===========================================================================

/// Whether building the knowledge graph is PERMITTED: the docsearch master switch
/// must be on (the graph reads its chunks), AND the `build_graph` flag must be on.
/// SHIPS ON (both default true) — exactly like docsearch — but INERT WITHOUT
/// INDEXED DOCS. Checked before any chunk is read so an off subsystem mines nothing.
pub fn build_permitted(enabled: bool, build_graph: bool) -> bool {
    enabled && build_graph
}

/// The "build/map knowledge graph from my documents" intent's core: GATED build
/// over the already-indexed docsearch chunks into the shared world model. Returns
/// `Ok(None)` when the build is NOT permitted (the caller then tells the user the
/// feature is off — it never silently mines), or `Ok(Some(stats))`.
///
/// This takes the chunks the caller read from the live [`crate::docsearch::DocIndex`]
/// (so this fn stays store-agnostic + unit-testable) and the injected extractor
/// (the deterministic one in the live path; a mock in tests; the LLM seam when
/// runtime-gated on). It writes ONLY the shared world tier.
pub async fn map_documents(
    enabled: bool,
    build_graph: bool,
    memory: &Memory,
    extractor: &dyn Extractor,
    chunks: &[(String, i64, String)],
) -> Result<Option<BuildStats>> {
    if !build_permitted(enabled, build_graph) {
        return Ok(None); // OFF -> mine NOTHING.
    }
    let stats = build_from_chunks(memory, extractor, chunks).await?;
    Ok(Some(stats))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-kg-test-{}-{}.db",
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

    fn types_of(ex: &Extraction) -> Vec<(EntityType, String)> {
        ex.entities
            .iter()
            .map(|e| (e.entity_type, e.name.clone()))
            .collect()
    }

    // -- DETERMINISTIC EXTRACTION (pure, hermetic) ---------------------------

    #[test]
    fn deterministic_extracts_expected_entities_grounded_in_text() {
        let ex = DeterministicExtractor;
        let text = "Met with Darwin Capani about Project DARWIN. \
                    The thesis is due 2026-06-30.";
        let out = ex.extract(text);

        // A multi-word proper name -> Person; an ALL-CAPS code word -> Project; an
        // ISO date -> Deadline. Each must be present and correctly typed.
        let kinds = types_of(&out);
        assert!(
            kinds.iter().any(|(t, n)| *t == EntityType::Person && n == "Darwin Capani"),
            "person not extracted: {kinds:?}"
        );
        assert!(
            kinds.iter().any(|(t, n)| *t == EntityType::Project && n.contains("DARWIN")),
            "project not extracted: {kinds:?}"
        );
        assert!(
            kinds.iter().any(|(t, n)| *t == EntityType::Deadline && n == "2026-06-30"),
            "deadline date not extracted: {kinds:?}"
        );

        // Every entity carries a REAL span into the text (provenance grounding).
        for e in &out.entities {
            assert!(e.span.0 < e.span.1, "empty span for {:?}", e.name);
            let slice: String = text.chars().skip(e.span.0).take(e.span.1 - e.span.0).collect();
            assert!(
                !slice.trim().is_empty(),
                "span does not point at real text for {:?}",
                e.name
            );
        }
    }

    #[test]
    fn deterministic_extracts_dates_in_multiple_shapes() {
        let ex = DeterministicExtractor;
        for (text, raw) in [
            ("due 2026-06-30 sharp", "2026-06-30"),
            ("ship by 06/30/2026 ok", "06/30/2026"),
            ("the deadline is June 30, 2026 firm", "June 30, 2026"),
            ("meet on June 30 please", "June 30"),
        ] {
            let out = ex.extract(text);
            assert!(
                out.entities
                    .iter()
                    .any(|e| e.entity_type == EntityType::Deadline && e.name == raw),
                "date shape {raw:?} not extracted from {text:?}: {:?}",
                out.entities
            );
        }
    }

    #[test]
    fn deterministic_yields_nothing_for_entityless_text_never_fabricates() {
        let ex = DeterministicExtractor;
        // All lowercase prose with no proper nouns, no dates, no cues.
        for text in [
            "the quick brown fox jumps over the lazy dog",
            "we should probably think about this later maybe",
            "   ",
            "",
        ] {
            let out = ex.extract(text);
            assert!(
                out.entities.is_empty() && out.relationships.is_empty(),
                "fabricated something from entity-less text {text:?}: {out:?}"
            );
        }
    }

    #[test]
    fn deterministic_drops_ambiguous_lone_capitalized_word() {
        let ex = DeterministicExtractor;
        // "The" is sentence-initial; "Stuff" is a lone capitalized word with no
        // cue and not all-caps -> too ambiguous to type -> dropped (conservative).
        let out = ex.extract("The Stuff happened yesterday somewhere.");
        assert!(
            out.entities.is_empty(),
            "a lone ambiguous capitalized word must be dropped, got {:?}",
            out.entities
        );
    }

    #[test]
    fn deterministic_co_occurrence_relates_distinct_entities_in_a_chunk() {
        let ex = DeterministicExtractor;
        let out = ex.extract("Met with Darwin Capani about Project DARWIN.");
        // Two distinct entities -> exactly one co-occurrence `mentions` edge.
        assert!(out.entities.len() >= 2, "need >=2 entities: {:?}", out.entities);
        assert!(
            out.relationships.iter().any(|r| r.relation == "mentions"),
            "co-occurrence edge missing: {:?}",
            out.relationships
        );
        // The edge endpoints are both real extracted entities (never dangling).
        for r in &out.relationships {
            assert!(
                out.entities.iter().any(|e| e.name == r.from_name),
                "edge 'from' is not a grounded entity: {r:?}"
            );
            assert!(
                out.entities.iter().any(|e| e.name == r.to_name),
                "edge 'to' is not a grounded entity: {r:?}"
            );
        }
    }

    #[test]
    fn deterministic_types_by_cue_word() {
        let ex = DeterministicExtractor;
        // A project cue in front of a single capitalized word types it Project.
        let proj = ex.extract("the project Phoenix is underway");
        assert!(
            proj.entities
                .iter()
                .any(|e| e.entity_type == EntityType::Project && e.name == "Phoenix"),
            "project cue did not type Phoenix: {:?}",
            proj.entities
        );
        // A task cue types the following phrase as a Task.
        let task = ex.extract("TODO: Review Pull Request soon");
        assert!(
            task.entities.iter().any(|e| e.entity_type == EntityType::Task),
            "task cue did not produce a Task: {:?}",
            task.entities
        );
    }

    #[test]
    fn deterministic_never_emits_thread_from_document_prose() {
        let ex = DeterministicExtractor;
        let out = ex.extract("Met with Darwin Capani about Project DARWIN due 2026-06-30.");
        assert!(
            out.entities.iter().all(|e| e.entity_type != EntityType::Thread),
            "document prose must not be mined into a Thread: {:?}",
            out.entities
        );
    }

    // -- BUILD: upsert with provenance, dedup, shared-tier, bounds -----------

    #[tokio::test]
    async fn build_upserts_entities_with_provenance_into_shared_tier() {
        let db = TempDb::new("build-provenance");
        let mem = Memory::open(&db.0).unwrap();
        let chunks = vec![(
            "/Users/darwincapani/Documents/notes.md".to_string(),
            128i64,
            "Met with Darwin Capani about Project DARWIN due 2026-06-30.".to_string(),
        )];
        let stats = build_from_chunks(&mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();
        assert!(stats.entities_written >= 3, "entities written: {stats:?}");

        // The world model now holds the grounded entities WITH a provenance source.
        let state = world_model::query(&mem, "darwin").await.unwrap();
        let proj = state
            .entities
            .iter()
            .find(|e| e.entity_type == EntityType::Project)
            .expect("project must be present");
        let source = proj
            .attributes
            .iter()
            .find(|(a, _)| a == "source")
            .expect("a provenance source attribute must be written");
        assert!(
            source.1.contains("notes.md") && source.1.contains("128"),
            "provenance must cite the real file + offset, got {:?}",
            source.1
        );
    }

    #[tokio::test]
    async fn build_writes_only_shared_world_tier_never_agent_private() {
        let db = TempDb::new("build-shared-only");
        let mem = Memory::open(&db.0).unwrap();
        let chunks = vec![(
            "f.md".to_string(),
            0i64,
            "Met with Darwin Capani about Project DARWIN.".to_string(),
        )];
        build_from_chunks(&mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();

        // Every fact the build wrote is under the SHARED user.world.* prefix; not a
        // single agent.* private key exists.
        let all = mem.all_facts(10_000).await.unwrap();
        let world_rows: Vec<_> = all
            .iter()
            .filter(|(k, _)| k.starts_with(world_model::WORLD_PREFIX))
            .collect();
        assert!(!world_rows.is_empty(), "build must write the shared world tier");
        assert!(
            all.iter().all(|(k, _)| !k.starts_with("agent.")),
            "build must NEVER write an agent.* private namespace: {:?}",
            all.iter().filter(|(k, _)| k.starts_with("agent.")).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn build_dedups_same_entity_across_chunks_keeps_first_provenance() {
        let db = TempDb::new("build-dedup");
        let mem = Memory::open(&db.0).unwrap();
        // The SAME project named in two chunks of two different files.
        let chunks = vec![
            (
                "first.md".to_string(),
                10i64,
                "Project DARWIN kicked off.".to_string(),
            ),
            (
                "second.md".to_string(),
                20i64,
                "Project DARWIN continues.".to_string(),
            ),
        ];
        build_from_chunks(&mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();

        let state = world_model::query(&mem, "darwin").await.unwrap();
        let projects: Vec<_> = state
            .entities
            .iter()
            .filter(|e| e.entity_type == EntityType::Project)
            .collect();
        assert_eq!(projects.len(), 1, "same entity must DEDUP to one node: {projects:?}");
        // First grounding wins (idempotent re-run, no provenance churn).
        let source = projects[0].attributes.iter().find(|(a, _)| a == "source").unwrap();
        assert!(
            source.1.contains("first.md") && source.1.contains("10"),
            "the FIRST chunk's provenance must be kept, got {:?}",
            source.1
        );
    }

    #[tokio::test]
    async fn build_respects_entity_cap_skips_past_max_honestly() {
        let db = TempDb::new("build-cap");
        let mem = Memory::open(&db.0).unwrap();
        // Pre-fill the world model to its entity cap (the fast direct path the
        // world_model tests use).
        for i in 0..world_model::MAX_ENTITIES {
            let key = format!("user.world.entity.topic.t{i}.name");
            mem.upsert_user_fact(&key, &format!("t{i}")).await.unwrap();
        }
        // A chunk with a NEW entity that cannot fit.
        let chunks = vec![(
            "f.md".to_string(),
            0i64,
            "Project DARWIN is here.".to_string(),
        )];
        let stats = build_from_chunks(&mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();
        assert_eq!(stats.entities_written, 0, "no new entity may be written past the cap");
        assert!(stats.skipped_at_cap >= 1, "the over-cap entity must be COUNTED as skipped: {stats:?}");
        // And the model was NOT grown wrong.
        let state = world_model::query(&mem, "darwin").await.unwrap();
        assert!(
            state.entities.iter().all(|e| e.entity_type != EntityType::Project),
            "no project node should exist past the cap"
        );
    }

    #[tokio::test]
    async fn build_writes_co_occurrence_relationship() {
        let db = TempDb::new("build-rel");
        let mem = Memory::open(&db.0).unwrap();
        let chunks = vec![(
            "f.md".to_string(),
            0i64,
            "Met with Darwin Capani about Project DARWIN.".to_string(),
        )];
        let stats = build_from_chunks(&mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();
        assert!(stats.relationships_written >= 1, "a co-occurrence edge must be written: {stats:?}");
        let state = world_model::query(&mem, "darwin").await.unwrap();
        assert!(
            state.relationships.iter().any(|r| r.relation == "mentions"),
            "the shared world model must hold the mentions edge: {:?}",
            state.relationships
        );
    }

    #[tokio::test]
    async fn build_entityless_chunks_write_nothing() {
        let db = TempDb::new("build-empty");
        let mem = Memory::open(&db.0).unwrap();
        let chunks = vec![(
            "f.md".to_string(),
            0i64,
            "the quick brown fox jumps over the lazy dog".to_string(),
        )];
        let stats = build_from_chunks(&mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();
        assert_eq!(stats.entities_written, 0);
        assert_eq!(stats.relationships_written, 0);
        let state = world_model::snapshot(&mem).await.unwrap();
        assert!(state.is_empty(), "entity-less text must write nothing: {state:?}");
    }

    // -- GATING --------------------------------------------------------------

    #[test]
    fn build_is_not_permitted_when_off() {
        // docsearch off -> never, even with build_graph on.
        assert!(!build_permitted(false, true));
        // docsearch on but build_graph off (the shipped default) -> never.
        assert!(!build_permitted(true, false));
        // both off -> never.
        assert!(!build_permitted(false, false));
        // both on -> permitted.
        assert!(build_permitted(true, true));
    }

    #[tokio::test]
    async fn map_documents_is_gated_off_by_default() {
        let db = TempDb::new("map-gated");
        let mem = Memory::open(&db.0).unwrap();
        let chunks = vec![(
            "f.md".to_string(),
            0i64,
            "Project DARWIN is here.".to_string(),
        )];
        // OFF (shipped default) -> mines NOTHING even with real chunks.
        let off = map_documents(false, false, &mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();
        assert!(off.is_none(), "build OFF must mine nothing");
        assert!(
            world_model::snapshot(&mem).await.unwrap().is_empty(),
            "nothing may be written while the graph build is off"
        );
        // ON + ON -> builds.
        let on = map_documents(true, true, &mem, &DeterministicExtractor, &chunks)
            .await
            .unwrap();
        let stats = on.expect("ON must build");
        assert!(stats.entities_written >= 1, "ON must mine the chunks: {stats:?}");
    }

    // -- LLM SEAM: mockable, NOT called in tests -----------------------------

    /// A MOCK extractor standing in for the runtime-gated LLM extractor. It makes
    /// NO model/socket/network call — it returns a fixed grounded extraction so a
    /// test can prove the SEAM is injectable and the build loop is extractor-
    /// agnostic. The real LLM extractor would implement this same trait.
    struct MockLlmExtractor {
        called: std::sync::atomic::AtomicBool,
    }
    impl Extractor for MockLlmExtractor {
        fn extract(&self, chunk_text: &str) -> Extraction {
            self.called.store(true, std::sync::atomic::Ordering::SeqCst);
            // Return a single grounded entity spanning the whole (non-empty) chunk.
            if chunk_text.trim().is_empty() {
                return Extraction::default();
            }
            Extraction {
                entities: vec![ExtractedEntity {
                    entity_type: EntityType::Topic,
                    name: "Mock Topic".to_string(),
                    attributes: vec![("confidence".to_string(), "mock".to_string())],
                    span: (0, chunk_text.chars().count()),
                }],
                relationships: Vec::new(),
            }
        }
        fn method(&self) -> &'static str {
            "mock-llm"
        }
    }

    #[tokio::test]
    async fn llm_seam_is_injectable_and_only_the_injected_extractor_runs() {
        let db = TempDb::new("llm-seam");
        let mem = Memory::open(&db.0).unwrap();
        let det = DeterministicExtractor;
        let mock = MockLlmExtractor {
            called: std::sync::atomic::AtomicBool::new(false),
        };
        // The deterministic extractor is NOT the one we inject here; the mock is.
        assert_ne!(det.method(), mock.method());

        let chunks = vec![(
            "f.md".to_string(),
            0i64,
            "anything at all".to_string(),
        )];
        let stats = build_from_chunks(&mem, &mock, &chunks).await.unwrap();
        // The injected (mock) extractor ran — proving the seam is wired, with NO
        // real model/network call anywhere.
        assert!(
            mock.called.load(std::sync::atomic::Ordering::SeqCst),
            "the injected extractor must be the one the build calls"
        );
        assert!(stats.entities_written >= 1, "the mock's grounded entity must be written");
        let state = world_model::query(&mem, "mock").await.unwrap();
        assert!(
            state.entities.iter().any(|e| e.name == "Mock Topic"),
            "the mock seam's entity must land in the shared world model: {state:?}"
        );
    }
}
