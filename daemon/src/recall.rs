//! MNEMOSYNE's recall-RANKING engine: the retrieval half of Self-Learn v2.
//!
//! Given a free-text query and the facts already in the memory store
//! (`memory.rs`), this module ranks the stored facts by relevance and returns
//! the top-k. It is the read-only counterpart to pepper's STORE side: pepper
//! remembers; Mnemosyne surfaces the relevant past on demand ("what did I say
//! about X", "dig up that note", "have we discussed Y").
//!
//! ## What method is actually wired — the honesty that governs the copy
//! The ranker is pluggable behind the [`EmbeddingProvider`] trait, and recall
//! is now RUNTIME-SELECTED between two real backends:
//!   - [`NeuralEmbeddingProvider`] — TRUE on-device neural semantic recall:
//!     it ranks facts by COSINE similarity over embedding VECTORS produced by
//!     the inference server's `embed` op (its `[inference].embedder` backend —
//!     the Core ML bge sentence embedder by default, or the legacy path that
//!     mean-pools the resident MLX model's hidden states). PREFERRED whenever
//!     the inference server is running and the embed op succeeds. Recall embeds
//!     the query and the facts TOGETHER in one call and never persists a
//!     vector, so every comparison is same-space by construction whichever
//!     backend answered (persisting callers — docsearch — key the space via
//!     [`Embedder::embed_with_space`] instead).
//!   - [`LexicalProvider`] — the deterministic in-process BM25 ranker over the
//!     fact text (term overlap, IDF-weighted, length-normalized). It is
//!     keyword-semantic, NOT vector-semantic. The HONEST FALLBACK whenever the
//!     embedder is unavailable (inference server down, or an older server
//!     without the embed op): recall keeps working, just lexically.
//!     This reverses the round-B limitation (then: lexical-only, "not neural,"
//!     because no embed endpoint existed). Now BOTH exist and the active one is
//!     chosen at runtime; [`method_status`] reports WHICH honestly so a user is
//!     never told recall is neural when it silently fell back to BM25. We never
//!     claim measured embedding QUALITY — only which mechanism produced the ranking.
//!     Neural recall NEEDS the inference server running; with it down, recall is
//!     lexical and says so.
//!
//! The embedding CALL itself is runtime/MLX-gated and is NOT exercised by any
//! test (a test that hit MLX or the socket would be an automatic failure).
//! What IS unit-tested is the pure RANKING logic: [`NeuralEmbeddingProvider`]
//! scores by cosine over INJECTED/mocked vectors (relevant > irrelevant,
//! deterministic, cosine correct); the runtime fallback to lexical when the
//! embedder errors; the method-status reporting; and empty/no-match honesty.
//!
//! ## Properties (all unit-tested, all pure)
//! - `rank(query, facts, k)` returns the k most relevant stored facts with
//!   scores, best first. A genuinely relevant fact ranks above an irrelevant
//!   one.
//! - DETERMINISTIC: the same (query, facts, k) always yields the same order
//!   (ties broken by original index, so it never depends on hashmap iteration).
//! - DEDUP: near-duplicate facts (same normalized text, or one a token-subset
//!   of another with the same top relevance) collapse to one hit.
//! - EMPTY-STORE and NO-MATCH are honest: an empty store, or a query that
//!   matches no stored term, yields ZERO hits — the caller then says "nothing
//!   on that yet" and NEVER fabricates a memory.
//!
//! Nothing here speaks, acts, or reaches the network. It reads stored facts and
//! returns a ranking. A proactive-surface helper ([`relevant_context`]) exists
//! for an OPTIONAL read-only "you mentioned this before" hook, but it only
//! returns text — it never auto-speaks (it respects EDITH's conservative
//! posture; the caller decides whether to surface, and the proactive loop's
//! guards still apply).

use std::collections::HashMap;

/// How recall is actually performed, named honestly for the status line and
/// the persona/tool copy. Recall is RUNTIME-SELECTED: [`RankMethod::Embedding`]
/// when the inference server's `embed` op backs [`NeuralEmbeddingProvider`],
/// else [`RankMethod::Lexical`] (the BM25 [`LexicalProvider`] fallback). Both
/// are real and wired; the active one depends on whether the embedder answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankMethod {
    /// BM25 / TF-IDF over the fact text: term overlap, IDF-weighted,
    /// length-normalized. Lexical-semantic, NOT neural. The honest fallback when
    /// the on-device embedder is unavailable (inference server down / no op).
    Lexical,
    /// True neural embedding similarity: cosine over the on-device embedding
    /// vectors the inference server's `embed` op produces (its configured
    /// `[inference].embedder` backend). Active when the embedder is up.
    Embedding,
    /// TWO-STAGE retrieval: neural bi-encoder recall (`Embedding`) THEN a Core ML
    /// cross-encoder RERANK of the top-K candidates (the inference server's
    /// `rerank` op, `[inference].reranker`). Reported ONLY when the cross-encoder
    /// actually re-scored the shortlist — never when it was disabled or fell back
    /// (that stays `Embedding`), so the label never overstates what ran.
    Reranked,
}

impl RankMethod {
    /// A short, stable token for telemetry / structured status.
    // Exercised by the unit tests; reserved for a structured telemetry/status
    // surface (the live path uses the human `description`).
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            RankMethod::Lexical => "lexical-bm25",
            RankMethod::Embedding => "neural-embedding",
            RankMethod::Reranked => "neural-reranked",
        }
    }

    /// One honest human sentence naming the method — what the persona and the
    /// `mnemosyne_recall` tool report so a user is never misled about whether
    /// recall is neural. Mnemosyne states this rather than implying foresight.
    pub fn description(&self) -> &'static str {
        match self {
            RankMethod::Lexical => {
                "lexical-semantic recall: I rank your stored facts by BM25 term \
                 relevance (overlap, weighted by how distinctive each word is, \
                 normalized for length) — not by a neural embedding model. It is \
                 keyword-semantic, not vector-semantic."
            }
            RankMethod::Embedding => {
                "neural (on-device embeddings) recall: I rank your stored facts \
                 by cosine similarity over embedding vectors computed on-device \
                 by the local inference server's embedding backend. This is true \
                 vector-semantic recall (it matches on \
                 meaning, not just words); it needs the inference server running, \
                 and falls back to lexical BM25 when that server is down."
            }
            RankMethod::Reranked => {
                "two-stage neural recall: I first retrieve candidates by cosine \
                 similarity over on-device embedding vectors, then RE-RANK the top \
                 few with an on-device cross-encoder that reads each candidate \
                 together with your query (full query-passage attention) for a \
                 sharper order. Both stages run on the local inference server; if \
                 the reranker is off or unavailable I keep the plain embedding \
                 order and say so."
            }
        }
    }
}

/// One stored fact reduced to what the ranker reasons over: its `key` (the
/// namespaced key, e.g. `user.preference.editor`) and its `value` (the fact
/// text). Both contribute to the ranking — the key often carries the topic
/// word (`user.car`) and the value carries the detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub key: String,
    pub value: String,
}

impl Fact {
    /// Build a fact. The live path constructs `Fact { key, value }` directly
    /// from the memory store rows; this ergonomic constructor is used by the
    /// unit tests and by any future caller building facts inline.
    #[allow(dead_code)]
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    /// The full searchable text of a fact: key + value. The key's dotted
    /// segments are tokenized too (so `user.car` contributes the term `car`),
    /// which is what lets "what do you remember about my car" find it.
    fn searchable(&self) -> String {
        format!("{} {}", self.key, self.value)
    }
}

/// One ranked recall hit: the matched fact, its relevance score (higher is more
/// relevant; always finite and >= 0), and the fact's original index in the
/// input list (the stable tie-breaker, and useful for the caller).
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub fact: Fact,
    pub score: f64,
    pub index: usize,
}

/// A pluggable ranker. Today the only implementation is [`LexicalProvider`]
/// (BM25); a future `NeuralProvider` backed by a real on-device embedding model
/// would implement this same trait and [`rank`] would not change. The method it
/// reports ([`EmbeddingProvider::method`]) is what the honesty copy reflects.
pub trait EmbeddingProvider {
    /// Score every fact against `query`, returning a score per fact in the SAME
    /// ORDER as `facts` (parallel vector). Higher = more relevant; a fact with
    /// no relevance scores 0.0. Pure and deterministic.
    fn score(&self, query: &str, facts: &[Fact]) -> Vec<f64>;

    /// Which method this provider actually uses — drives the honest status.
    fn method(&self) -> RankMethod;
}

// ---------------------------------------------------------------------------
// Tokenization (shared by the lexical ranker and dedup)
// ---------------------------------------------------------------------------

/// Lowercase, split on any non-alphanumeric boundary, drop empties and a small
/// set of stopwords. Deterministic. Dotted keys split naturally here
/// (`user.car` -> ["user", "car"]) because `.` is a non-alphanumeric boundary.
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .filter(|t| !is_stopword(t))
        .collect()
}

/// A compact stopword set so common glue words ("the", "what", "about") do not
/// dominate the ranking — they appear in nearly every fact and carry no topic.
/// Deliberately small and conservative: removing too much would drop real
/// signal. Mirrors what a BM25 setup typically strips.
fn is_stopword(t: &str) -> bool {
    matches!(
        t,
        "the" | "a" | "an" | "of" | "to" | "in" | "on" | "at" | "is" | "are"
            | "was" | "were" | "be" | "been" | "and" | "or" | "but" | "for"
            | "with" | "about" | "what" | "did" | "do" | "does" | "you"
            | "i" | "me" | "my" | "your" | "that" | "this" | "it" | "have"
            | "has" | "had" | "tell" | "say" | "said" | "know" | "remember"
            | "recall" | "any" | "some" | "all"
    )
}

// ---------------------------------------------------------------------------
// LexicalProvider — the SHIPPED BM25 ranker (honest: lexical, not neural)
// ---------------------------------------------------------------------------

/// BM25 free parameters. The standard, well-behaved defaults: `k1` controls
/// term-frequency saturation, `b` controls length normalization. Exposed so a
/// test (or a future tuning pass) can pin them; the default is what ships.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bm25Params {
    pub k1: f64,
    pub b: f64,
}

impl Default for Bm25Params {
    fn default() -> Self {
        // Okapi BM25 textbook defaults.
        Self { k1: 1.5, b: 0.75 }
    }
}

/// The shipped recall ranker: Okapi BM25 over the fact text. Lexical-semantic
/// (term overlap, IDF-weighted, length-normalized), NOT a neural embedding
/// model — and it says so via [`EmbeddingProvider::method`] -> `Lexical`.
#[derive(Debug, Clone, Copy, Default)]
pub struct LexicalProvider {
    pub params: Bm25Params,
}

impl EmbeddingProvider for LexicalProvider {
    fn score(&self, query: &str, facts: &[Fact]) -> Vec<f64> {
        let q_terms = tokenize(query);
        if q_terms.is_empty() || facts.is_empty() {
            return vec![0.0; facts.len()];
        }
        // Tokenize every document once; compute its length and term counts.
        let docs: Vec<Vec<String>> = facts.iter().map(|f| tokenize(&f.searchable())).collect();
        let n = docs.len() as f64;
        let avg_len = {
            let total: usize = docs.iter().map(|d| d.len()).sum();
            if total == 0 {
                return vec![0.0; facts.len()];
            }
            total as f64 / n
        };
        // Document frequency per query term (how many docs contain it).
        let mut df: HashMap<&str, u32> = HashMap::new();
        for term in &q_terms {
            let count = docs
                .iter()
                .filter(|d| d.iter().any(|w| w == term))
                .count() as u32;
            df.insert(term.as_str(), count);
        }
        let k1 = self.params.k1;
        let b = self.params.b;
        docs.iter()
            .map(|doc| {
                let dl = doc.len() as f64;
                if dl == 0.0 {
                    return 0.0;
                }
                let mut score = 0.0;
                for term in &q_terms {
                    let tf = doc.iter().filter(|w| *w == term).count() as f64;
                    if tf == 0.0 {
                        continue;
                    }
                    let n_q = *df.get(term.as_str()).unwrap_or(&0) as f64;
                    // BM25 IDF with the +1 inside the log so it is always >= 0
                    // (no negative contribution from a term in most docs); a
                    // term present in EVERY doc then contributes ~0, which is
                    // the desired "no signal" behavior.
                    let idf = (((n - n_q + 0.5) / (n_q + 0.5)) + 1.0).ln();
                    let denom = tf + k1 * (1.0 - b + b * (dl / avg_len));
                    score += idf * (tf * (k1 + 1.0)) / denom;
                }
                score
            })
            .collect()
    }

    fn method(&self) -> RankMethod {
        RankMethod::Lexical
    }
}

// ---------------------------------------------------------------------------
// NeuralEmbeddingProvider — TRUE on-device neural semantic recall
// ---------------------------------------------------------------------------

/// Cosine similarity between two equal-length vectors. PURE. Returns 0.0 when
/// either vector is empty, has a different length, or is all-zero (so a
/// degenerate embedding never produces a bogus high score). The server already
/// L2-normalizes its vectors, so this is a plain dot product in the live path,
/// but we normalize defensively here too so INJECTED test vectors need not be.
///
/// `pub(crate)` so the on-device file-RAG (`crate::docsearch`) ranks its stored
/// chunk vectors with the EXACT same cosine the neural recall path uses — one
/// shared, degenerate-safe implementation, never a second copy.
pub(crate) fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    let sim = dot / (na.sqrt() * nb.sqrt());
    if sim.is_finite() {
        sim
    } else {
        0.0
    }
}

/// TRUE neural semantic recall: ranks facts by COSINE similarity between the
/// query embedding and each fact embedding. The embeddings are computed
/// ON-DEVICE by the inference server's `embed` op (its configured
/// `[inference].embedder` backend) and INJECTED into this provider — so the provider
/// itself is PURE and DETERMINISTIC, and the (runtime/MLX-gated) embedding call
/// lives in the caller, not here. That split is what makes the ranking logic
/// unit-testable with mocked vectors while keeping the real call out of tests.
///
/// Negative cosine (a fact pointing AWAY from the query) is clamped to 0.0:
/// [`rank`] drops non-positive scores as "no relevance," and a negative
/// similarity is at most "unrelated," never evidence to surface a memory — this
/// preserves the no-fabrication contract under neural scoring too.
pub struct NeuralEmbeddingProvider {
    /// The query's embedding vector (from the embed op).
    query: Vec<f64>,
    /// One embedding vector per fact, in the SAME ORDER as the `facts` slice
    /// passed to [`rank`]. Built by the caller from the embed-op batch.
    fact_vectors: Vec<Vec<f64>>,
}

impl NeuralEmbeddingProvider {
    /// Build the provider from the query embedding and the per-fact embeddings
    /// (parallel to the facts the caller will rank). The caller obtains these
    /// from the inference `embed` op; tests inject mock vectors directly.
    pub fn new(query: Vec<f64>, fact_vectors: Vec<Vec<f64>>) -> Self {
        Self {
            query,
            fact_vectors,
        }
    }
}

impl EmbeddingProvider for NeuralEmbeddingProvider {
    fn score(&self, _query: &str, facts: &[Fact]) -> Vec<f64> {
        // The vectors are precomputed and parallel to `facts`. If the caller
        // somehow handed mismatched counts, score everything 0.0 (no relevance)
        // rather than panic or fabricate — rank() then returns no hits, which is
        // the honest "nothing matched" result.
        if self.fact_vectors.len() != facts.len() || self.query.is_empty() {
            return vec![0.0; facts.len()];
        }
        self.fact_vectors
            .iter()
            .map(|v| {
                let sim = cosine_similarity(&self.query, v);
                // Clamp negatives to 0.0: an anti-correlated fact is not a hit.
                if sim > 0.0 {
                    sim
                } else {
                    0.0
                }
            })
            .collect()
    }

    fn method(&self) -> RankMethod {
        RankMethod::Embedding
    }
}

// ---------------------------------------------------------------------------
// Dedup
// ---------------------------------------------------------------------------

/// The normalized token signature of a fact's text (sorted unique tokens),
/// used to collapse near-duplicates: two facts whose VALUE normalizes to the
/// same token set are duplicates regardless of key punctuation or word order.
/// We use the VALUE (not key+value) so two records of the same fact under
/// slightly different keys still collapse.
fn dedup_signature(fact: &Fact) -> Vec<String> {
    let mut toks = tokenize(&fact.value);
    toks.sort();
    toks.dedup();
    toks
}

// ---------------------------------------------------------------------------
// The ranking entry point (PURE — the unit-tested heart)
// ---------------------------------------------------------------------------

/// Rank `facts` against `query` with `provider`, returning at most `k` hits,
/// most-relevant first, with near-duplicates collapsed and zero-score
/// (irrelevant) facts dropped. PURE and DETERMINISTIC:
///   - the score comes from the injected `provider`;
///   - ties (equal score) break by original index, so the order never depends
///     on hashmap iteration or input shuffling beyond what the scores imply;
///   - a fact with score <= 0 is NOT returned (no-match honesty: an irrelevant
///     fact is never surfaced as a "memory");
///   - `k == 0` or an empty store yields an empty result (the caller then says
///     "nothing on that yet" — it never fabricates).
///     Dedup keeps the HIGHEST-scoring representative of each duplicate group (ties
///     within a group keep the earliest index), so a relevant fact is never hidden
///     behind a lower-scoring duplicate.
pub fn rank<P: EmbeddingProvider>(
    query: &str,
    facts: &[Fact],
    k: usize,
    provider: &P,
) -> Vec<Hit> {
    if k == 0 || facts.is_empty() {
        return Vec::new();
    }
    let scores = provider.score(query, facts);
    debug_assert_eq!(scores.len(), facts.len(), "provider must score every fact");

    // Build candidate hits, dropping non-positive (irrelevant) scores.
    let mut hits: Vec<Hit> = facts
        .iter()
        .zip(scores.iter())
        .enumerate()
        .filter(|(_, (_, &s))| s.is_finite() && s > 0.0)
        .map(|(index, (fact, &score))| Hit {
            fact: fact.clone(),
            score,
            index,
        })
        .collect();

    // Sort by score DESC, then index ASC (the deterministic tie-break).
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.index.cmp(&b.index))
    });

    // Dedup near-duplicates: walk the sorted hits and keep the first (highest
    // scoring, earliest index) per signature; later duplicates are dropped.
    let mut seen: Vec<Vec<String>> = Vec::new();
    let mut deduped: Vec<Hit> = Vec::new();
    for hit in hits {
        let sig = dedup_signature(&hit.fact);
        // An all-stopword / empty value has an empty signature; treat each such
        // fact as distinct (do not collapse unrelated empties together).
        let is_dup = !sig.is_empty() && seen.contains(&sig);
        if is_dup {
            continue;
        }
        seen.push(sig);
        deduped.push(hit);
        if deduped.len() == k {
            break;
        }
    }
    deduped
}

/// The honest one-line status of HOW recall ranks — the string the tool and
/// any HUD status surface so the method is always reported truthfully. Names the
/// ACTUALLY-USED backend: neural on-device embeddings when the embedder answered,
/// or BM25 lexical recall when it fell back. Never claims neural when lexical ran.
pub fn method_status<P: EmbeddingProvider>(provider: &P) -> String {
    provider.method().description().to_string()
}

// ---------------------------------------------------------------------------
// Runtime backend selection: PREFER neural, FALL BACK to lexical — honestly
// ---------------------------------------------------------------------------

/// The result of a runtime-selected recall: the ranked hits plus WHICH backend
/// actually produced them, so the caller can report the method truthfully.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedRecall {
    pub hits: Vec<Hit>,
    /// The backend that ACTUALLY ran (Embedding only if the embedder answered).
    pub method: RankMethod,
    /// The honest one-line method status string for the backend that ran.
    pub method_status: String,
}

/// Fetches on-device embeddings for a batch of strings. The REAL implementation
/// calls the inference `embed` op (runtime/MLX-gated, NOT exercised by tests);
/// tests inject a mock that returns canned vectors — or an error, to drive the
/// fallback. Returns one vector per input, in input order; Err means the
/// embedder is unavailable (server down, no embed op, or a transport failure),
/// which makes recall fall back to lexical BM25.
///
/// Object-safe + `Send`/`Sync` and spelled with an explicit boxed future (no
/// async-trait crate — matching the codebase's "no new deps" pattern used by
/// the Babel `Translator` and SAGE `Brain` traits).
pub trait Embedder: Send + Sync {
    fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a>;

    /// SPACE-AWARE embed: the vectors PLUS the OPAQUE space-id of which embedder
    /// produced them, so a caller that PERSISTS vectors (docsearch) can stamp
    /// its store's space and refuse a meaningless cross-space cosine. The
    /// provided default delegates to [`Self::embed`] and reports NO metadata
    /// (`embedder: None`) — exactly what an old inference server sends. A
    /// persisting caller keys such a metadata-less batch to its OWN opaque
    /// placeholder; it does NOT assume the batch is any particular backend
    /// (ids are opaque + model-derived). The live inference-socket embedder
    /// (anthropic.rs) overrides this with the real op=embed metadata; mocks
    /// and callers that only need `embed` keep compiling and behave as before.
    fn embed_with_space<'a>(&'a self, texts: &'a [String]) -> EmbedSpaceFuture<'a> {
        Box::pin(async move {
            let vectors = self.embed(texts).await?;
            Ok(EmbeddedBatch {
                vectors,
                embedder: None,
                dim: None,
                fell_back: false,
            })
        })
    }

    /// STAGE TWO of the two-stage retrieval stack: whether a Core ML cross-encoder
    /// RERANK should run after dense retrieval (config-gated by
    /// `[inference].reranker`). The DEFAULT is `false`, so mock embedders and any
    /// caller that only embeds keep today's single-stage behavior untouched; the
    /// LIVE inference-socket embedder overrides this from the daemon's reranker
    /// gate. Reranking rides the SAME backend (the inference server) as `embed`, so
    /// it is exposed on this trait rather than threading a second object through
    /// every recall/RAG call site.
    fn rerank_enabled(&self) -> bool {
        false
    }

    /// STAGE TWO: score `passages` against `query` with the on-device cross-encoder
    /// (the inference server's `rerank` op) — one relevance score per passage, in
    /// INPUT order (higher = more relevant; the caller re-orders). A caller passes
    /// the dense top-K candidate texts. `fell_back=true` (or an `Err`) means the
    /// reranker was configured but unavailable, so the caller KEEPS the dense order
    /// (honest fallback, never silent). The provided default returns the
    /// unavailable outcome; the live socket embedder overrides it. Only ever called
    /// when [`Self::rerank_enabled`] is true.
    fn rerank<'a>(&'a self, query: &'a str, passages: &'a [String]) -> RerankFuture<'a> {
        let _ = (query, passages);
        Box::pin(async { Ok(RerankOutcome::unavailable()) })
    }
}

/// The boxed future [`Embedder::embed`] returns, kept object-safe for `&dyn`.
pub type EmbedFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Vec<Vec<f64>>>> + Send + 'a>>;

/// The boxed future [`Embedder::embed_with_space`] returns, kept object-safe
/// for `&dyn` (same pattern as [`EmbedFuture`]).
pub type EmbedSpaceFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<EmbeddedBatch>> + Send + 'a>>;

/// The boxed future [`Embedder::rerank`] returns, kept object-safe for `&dyn`
/// (same pattern as [`EmbedFuture`]).
pub type RerankFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<RerankOutcome>> + Send + 'a>>;

/// One op=rerank round trip: one relevance score per passage (INPUT order, higher
/// = more relevant) plus which cross-encoder produced them + the honest-fallback
/// flag. Mirrors [`crate::inference::RerankOutcome`] — kept as its own recall-layer
/// struct so trait mocks build it without touching the socket client. When the
/// reranker is unavailable (or disabled), [`RerankOutcome::unavailable`] carries
/// `fell_back=true` so the caller KEEPS the dense order.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankOutcome {
    /// One cross-encoder relevance score per passage, in input order.
    pub scores: Vec<f64>,
    /// The OPAQUE reranker model id that produced the scores, or `None` when it
    /// fell back (no model scored). Compared only by equality, never interpreted.
    pub reranker: Option<String>,
    /// Advisory: the reranker was configured but unavailable, so the scores are
    /// order-preserving and the caller must keep the dense order.
    pub fell_back: bool,
}

impl RerankOutcome {
    /// The honest "no rerank happened" outcome: empty scores, no model id,
    /// `fell_back=true` — so a caller keeps its dense order.
    pub fn unavailable() -> Self {
        Self {
            scores: Vec::new(),
            reranker: None,
            fell_back: true,
        }
    }
}

/// The bounded rerank shortlist depth K: after dense retrieval, at most this many
/// top candidates are handed to the cross-encoder to re-score. MEASURED optimum on
/// the committed two-stage eval (inference/benchmarks/coreml_rerank_eval/): K=20
/// reaches the SAME reranked quality as K=50 at ~2.5x LESS latency (dense recall@20
/// was already 1.0, so a deeper shortlist bought nothing but cost). Bounding K also
/// caps the per-query rerank cost (one cross-encoder forward per candidate).
pub const DEFAULT_RERANK_K: usize = 20;

/// STAGE TWO applied to an already-dense-ranked candidate list: if `embedder` has
/// reranking ENABLED (config-gated) and there are >=2 candidates, re-score the
/// `passages` (the caller's dense top-K texts, best-first) with the on-device
/// cross-encoder and return the PERMUTATION of `0..passages.len()` to apply to the
/// caller's own parallel items (sorted by rerank score DESC, stable by original
/// dense position on ties). Returns `None` — meaning KEEP the dense order — when
/// reranking is disabled, the shortlist is trivial, or the reranker is unavailable
/// / fell back / returned a mismatched count (honest fallback, never silent).
/// Generic over the caller's item type: recall, docsearch, and unified_search all
/// pass their dense-ordered passage texts and re-order their own items by the
/// returned permutation.
pub async fn rerank_permutation(
    query: &str,
    passages: &[String],
    embedder: &dyn Embedder,
) -> Option<Vec<usize>> {
    if passages.len() < 2 || !embedder.rerank_enabled() {
        return None;
    }
    match embedder.rerank(query, passages).await {
        Ok(out) if !out.fell_back && out.scores.len() == passages.len() => {
            let scores = out.scores;
            let mut idx: Vec<usize> = (0..passages.len()).collect();
            // Sort by score DESC; ties keep the earlier DENSE position (stable,
            // deterministic — the same tie-break rank() uses).
            idx.sort_by(|&a, &b| {
                scores[b]
                    .partial_cmp(&scores[a])
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.cmp(&b))
            });
            Some(idx)
        }
        // Unavailable / fell back / wrong count / Err: keep the dense order.
        _ => None,
    }
}

/// One space-aware embed batch: the vectors plus the OPAQUE space-id of which
/// embedder produced them. Mirrors [`crate::inference::EmbedOutcome`] — kept as
/// its own struct so trait mocks build it directly without touching the socket
/// client. `embedder`/`dim` are `None` when the response predates the op=embed
/// space metadata; a persisting caller keys such a batch to its own opaque
/// placeholder rather than assuming a backend (ids are opaque + model-derived).
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedBatch {
    /// One L2-normalized vector per input text, in input order.
    pub vectors: Vec<Vec<f64>>,
    /// The OPAQUE, model-accurate space-id string the backend reports (e.g. the
    /// Core ML bge id, or a model-derived mean-pool id), or `None` on a
    /// metadata-less old server. Compared only by equality, never interpreted.
    pub embedder: Option<String>,
    /// The vector dimension the backend produces; `None` on an old server or an
    /// empty batch.
    pub dim: Option<u64>,
    /// Advisory: the server fell back to the mean-pool backend although the Core
    /// ML one was configured. `false` when absent.
    pub fell_back: bool,
}

/// Rank `facts` against `query`, RUNTIME-SELECTING the backend: try the neural
/// on-device embedder FIRST, and FALL BACK to lexical BM25 when it is
/// unavailable. Reports which backend actually ran (honesty: never says neural
/// when it fell back).
///
/// The neural path asks `embedder` for one vector for the query plus one per
/// fact (a single batched call, query first), then ranks by cosine similarity
/// via [`NeuralEmbeddingProvider`]. ANY of the following falls back to
/// [`LexicalProvider`] (BM25), cleanly and silently to the user (the status
/// then names lexical):
///   - the embedder returns Err (inference server down / no embed op);
///   - it returns the wrong number of vectors;
///   - it returns empty/degenerate vectors.
///     An empty store still returns zero hits under either backend (no fabrication).
///
/// ISOLATION is unaffected: the caller passes only the facts visible to the
/// active agent (agent_scoped_facts); this ranks exactly those, never more.
///
/// The embedding CALL is the only runtime/MLX-gated part; the ranking, fallback,
/// and status logic are pure and unit-tested with a mock `embedder`.
pub async fn rank_runtime_selected(
    query: &str,
    facts: &[Fact],
    k: usize,
    embedder: &dyn Embedder,
) -> RankedRecall {
    // Empty store / k==0: zero hits regardless of backend (no embed call made).
    // We still report the PREFERRED backend's availability honestly — but with
    // no facts there is nothing to embed, so we report lexical (what would run).
    let lexical = LexicalProvider::default();
    let lexical_result = |hits: Vec<Hit>| RankedRecall {
        hits,
        method: lexical.method(),
        method_status: method_status(&lexical),
    };

    if k == 0 || facts.is_empty() {
        return lexical_result(Vec::new());
    }

    // TWO-STAGE retrieval: when a cross-encoder rerank is enabled (config-gated),
    // stage ONE retrieves a DEEPER shortlist (top max(k, DEFAULT_RERANK_K)) so
    // stage TWO has candidates to re-order; the head is then reranked and the
    // result truncated back to `k`. When reranking is off, retrieve exactly `k`
    // (today's single-stage path, byte-for-byte).
    let rerank_on = embedder.rerank_enabled();
    let retrieve_k = if rerank_on {
        k.max(DEFAULT_RERANK_K)
    } else {
        k
    };

    // Try neural first: one batched embed call, query at index 0, then facts.
    let mut batch: Vec<String> = Vec::with_capacity(facts.len() + 1);
    batch.push(query.to_string());
    for f in facts {
        batch.push(f.searchable());
    }

    let dense = match embedder.embed(&batch).await {
        Ok(vectors) if vectors.len() == batch.len() => {
            let mut iter = vectors.into_iter();
            let query_vec = iter.next().unwrap_or_default();
            let fact_vectors: Vec<Vec<f64>> = iter.collect();
            // Degenerate (empty) query embedding -> not usable; fall back.
            if query_vec.is_empty() {
                return lexical_result(rank(query, facts, k, &lexical));
            }
            let provider = NeuralEmbeddingProvider::new(query_vec, fact_vectors);
            let hits = rank(query, facts, retrieve_k, &provider);
            RankedRecall {
                hits,
                method: provider.method(),
                method_status: method_status(&provider),
            }
        }
        // Wrong vector count OR an error: fall back to lexical BM25, honestly.
        // (The reranker rides the same server, so when embed is down rerank is too
        // and the fallback below keeps this lexical order.)
        _ => lexical_result(rank(query, facts, retrieve_k, &lexical)),
    };

    // STAGE TWO (config-gated): rerank the shortlist head, re-order, truncate to k.
    finish_with_optional_rerank(query, dense, k, rerank_on, embedder).await
}

/// Apply the optional cross-encoder RERANK to a dense-ranked [`RankedRecall`] and
/// truncate to `k`. When `rerank_on` and the reranker re-scores the top
/// [`DEFAULT_RERANK_K`] hits, the head is re-ordered by the cross-encoder score
/// (the tail below K keeps its dense order) and the method is upgraded to
/// [`RankMethod::Reranked`]. On any honest fallback (reranking off, unavailable,
/// or fell back) the dense order stands and the method is unchanged — the label
/// never overstates what ran. The dense result may carry a DEEPER shortlist than
/// `k` (see `retrieve_k`), so this always truncates to `k` last.
async fn finish_with_optional_rerank(
    query: &str,
    dense: RankedRecall,
    k: usize,
    rerank_on: bool,
    embedder: &dyn Embedder,
) -> RankedRecall {
    if rerank_on && dense.hits.len() >= 2 {
        let head_n = dense.hits.len().min(DEFAULT_RERANK_K);
        // Rerank the SAME text stage one embedded (key + value), so the two stages
        // score the identical unit.
        let passages: Vec<String> = dense.hits[..head_n]
            .iter()
            .map(|h| h.fact.searchable())
            .collect();
        if let Some(perm) = rerank_permutation(query, &passages, embedder).await {
            let mut reordered: Vec<Hit> = perm.iter().map(|&i| dense.hits[i].clone()).collect();
            reordered.extend_from_slice(&dense.hits[head_n..]);
            reordered.truncate(k);
            return RankedRecall {
                hits: reordered,
                method: RankMethod::Reranked,
                method_status: RankMethod::Reranked.description().to_string(),
            };
        }
    }
    // No rerank: keep the dense order + method, just truncate the shortlist to k.
    let mut hits = dense.hits;
    hits.truncate(k);
    RankedRecall { hits, ..dense }
}

/// OPTIONAL read-only proactive-surface helper: given the current conversation
/// text and the stored facts, return up to `k` relevant past facts as plain
/// "you mentioned ..." lines — or `None` when nothing is relevant. This NEVER
/// speaks and NEVER acts; it only composes text the caller MAY choose to
/// surface (and the proactive loop's own guards — quiet hours, debounce,
/// HUD-card-only default — still gate whether it ever reaches the user). It
/// respects EDITH's conservative posture: nothing relevant -> nothing offered.
// The OPTIONAL proactive-surface hook the contract asked for: built, pure, and
// unit-tested, but NOT yet wired into the live proactive loop (which ships
// conservative — see anticipate.rs). A later round can call this from the
// proactive tick under the same guards; for now it is read-only API surface.
#[allow(dead_code)]
pub fn relevant_context<P: EmbeddingProvider>(
    conversation: &str,
    facts: &[Fact],
    k: usize,
    provider: &P,
) -> Option<String> {
    let hits = rank(conversation, facts, k, provider);
    if hits.is_empty() {
        return None;
    }
    let lines: Vec<String> = hits
        .iter()
        .map(|h| format!("- {}: {}", h.fact.key, h.fact.value))
        .collect();
    Some(format!(
        "You've mentioned something relevant before:\n{}",
        lines.join("\n")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_facts() -> Vec<Fact> {
        vec![
            Fact::new("user.car", "I drive a blue Subaru Outback"),
            Fact::new("user.pet", "a corgi named Watson"),
            Fact::new("user.preference.editor", "prefers neovim over vscode"),
            Fact::new("user.coffee", "drinks oat-milk flat whites"),
            Fact::new("project.darwin", "building a local AI assistant on a Mac mini"),
        ]
    }

    fn lex() -> LexicalProvider {
        LexicalProvider::default()
    }

    // ---- the core property: a relevant fact outranks irrelevant ones --------

    #[test]
    fn a_relevant_fact_ranks_above_irrelevant_ones() {
        let facts = sample_facts();
        let hits = rank("what do you remember about my car", &facts, 3, &lex());
        assert!(!hits.is_empty(), "the car fact must be retrieved");
        assert_eq!(
            hits[0].fact.key, "user.car",
            "the car fact must rank first, got {:?}",
            hits[0].fact
        );
        // It must outrank the pet/editor facts (they have no "car" term).
        assert!(hits.iter().all(|h| h.score > 0.0), "only positive hits returned");
    }

    #[test]
    fn different_topics_retrieve_their_own_fact() {
        let facts = sample_facts();
        // "neovim" only appears in the editor preference.
        let hits = rank("which editor do i use", &facts, 1, &lex());
        assert_eq!(hits[0].fact.key, "user.preference.editor");
        // "corgi"/"pet" -> the pet fact.
        let hits = rank("what's my pet again", &facts, 1, &lex());
        assert_eq!(hits[0].fact.key, "user.pet");
    }

    #[test]
    fn key_terms_are_searchable_not_just_values() {
        // The topic word lives in the KEY (user.coffee), the value has no
        // "coffee" token — recall must still find it via the key tokenization.
        let facts = sample_facts();
        let hits = rank("coffee", &facts, 1, &lex());
        assert_eq!(hits[0].fact.key, "user.coffee", "key term must be searchable");
    }

    // ---- determinism --------------------------------------------------------

    #[test]
    fn ranking_is_deterministic() {
        let facts = sample_facts();
        let a = rank("what about my car and coffee", &facts, 5, &lex());
        let b = rank("what about my car and coffee", &facts, 5, &lex());
        assert_eq!(a, b, "the same query yields the identical ranking every time");
    }

    #[test]
    fn ties_break_by_original_index() {
        // Two facts with IDENTICAL searchable content but distinct keys would
        // score equally on a shared term; the earlier index must come first.
        // (They are NOT duplicates by value here — different values — so both
        // survive dedup.)
        let facts = vec![
            Fact::new("a.topic", "alpha widget gadget"),
            Fact::new("b.topic", "alpha sprocket cog"),
        ];
        // "alpha" is in both; "widget" only in the first -> first wins on score.
        let hits = rank("alpha widget", &facts, 2, &lex());
        assert_eq!(hits[0].fact.key, "a.topic");
        // A query of ONLY the shared term scores them equally -> index order.
        let hits = rank("alpha", &facts, 2, &lex());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].index, 0, "equal scores break by earliest index");
        assert!(hits[0].index < hits[1].index);
    }

    // ---- empty store / no match: honest zero, never a fabrication -----------

    #[test]
    fn empty_store_returns_nothing() {
        let hits = rank("anything at all", &[], 5, &lex());
        assert!(hits.is_empty(), "an empty store retrieves nothing");
    }

    #[test]
    fn no_match_returns_nothing_never_fabricates() {
        let facts = sample_facts();
        // A topic with zero overlap with any stored term.
        let hits = rank("quantum chromodynamics lecture notes", &facts, 5, &lex());
        assert!(
            hits.is_empty(),
            "a query matching no stored term retrieves NOTHING (no fabricated memory), got {hits:?}"
        );
    }

    #[test]
    fn all_stopword_query_matches_nothing() {
        let facts = sample_facts();
        // Every token is a stopword -> no query terms -> no hits (we never
        // surface a random fact for a contentless query).
        let hits = rank("what do you remember about", &facts, 5, &lex());
        assert!(hits.is_empty(), "a contentless query retrieves nothing: {hits:?}");
    }

    #[test]
    fn k_zero_returns_nothing() {
        let facts = sample_facts();
        assert!(rank("car", &facts, 0, &lex()).is_empty());
    }

    // ---- k limiting ---------------------------------------------------------

    #[test]
    fn k_limits_the_number_of_hits() {
        // A query that matches several facts; k caps the returned count.
        let facts = sample_facts();
        let hits = rank("i drive a blue car and a corgi and neovim", &facts, 2, &lex());
        assert!(hits.len() <= 2, "k caps the result count: {}", hits.len());
        assert!(!hits.is_empty());
    }

    // ---- dedup --------------------------------------------------------------

    #[test]
    fn near_duplicates_collapse_to_one() {
        // The same fact stored twice under different keys / word order: dedup
        // collapses them to a single hit, keeping the highest-scoring (earliest)
        // representative.
        let facts = vec![
            Fact::new("user.car", "blue Subaru Outback"),
            Fact::new("vehicle.note", "Outback Subaru blue"), // same token set
            Fact::new("user.pet", "a corgi named Watson"),
        ];
        let hits = rank("subaru outback", &facts, 5, &lex());
        let car_hits = hits
            .iter()
            .filter(|h| {
                let s = dedup_signature(&h.fact);
                s.contains(&"subaru".to_string())
            })
            .count();
        assert_eq!(car_hits, 1, "near-duplicate facts collapse to one hit: {hits:?}");
        // The surviving representative is the earlier (user.car) entry.
        assert_eq!(hits[0].fact.key, "user.car");
    }

    #[test]
    fn distinct_facts_are_not_collapsed() {
        let facts = sample_facts();
        let hits = rank("car corgi", &facts, 5, &lex());
        // car and corgi are different facts; both should be present.
        let keys: Vec<&str> = hits.iter().map(|h| h.fact.key.as_str()).collect();
        assert!(keys.contains(&"user.car"), "{keys:?}");
        assert!(keys.contains(&"user.pet"), "{keys:?}");
    }

    // ---- honesty: the method is reported truthfully ------------------------

    #[test]
    fn shipped_provider_reports_lexical_not_neural() {
        let p = lex();
        assert_eq!(p.method(), RankMethod::Lexical);
        let status = method_status(&p);
        let lower = status.to_lowercase();
        assert!(lower.contains("lexical"), "status must name lexical: {status}");
        assert!(lower.contains("bm25"), "status must name the method: {status}");
        // It must DISCLAIM neural embeddings (the honest framing explicitly says
        // it is NOT a neural embedding model and is keyword- not vector-semantic),
        // and it must never AFFIRM being embedding-based.
        assert!(
            lower.contains("not by a neural embedding") || lower.contains("not a neural"),
            "lexical recall must explicitly disclaim neural embeddings: {status}"
        );
        assert!(
            lower.contains("keyword-semantic") && lower.contains("not vector-semantic"),
            "must frame recall as keyword- not vector-semantic: {status}"
        );
        // And the stable token is the lexical one.
        assert_eq!(p.method().as_str(), "lexical-bm25");
    }

    #[test]
    fn embedding_method_description_is_distinct_and_reserved() {
        // The Embedding variant exists for a future neural provider; its copy
        // is honestly different and is NOT what the shipped provider reports.
        assert_ne!(RankMethod::Embedding, RankMethod::Lexical);
        assert!(RankMethod::Embedding.description().to_lowercase().contains("embedding"));
        assert_eq!(RankMethod::Embedding.as_str(), "neural-embedding");
    }

    // ---- the pluggable trait: a different provider swaps in cleanly --------

    #[test]
    fn rank_uses_the_injected_provider() {
        // A trivial stub provider that scores by fact INDEX (last fact wins),
        // proving rank() is provider-driven and would accept a NeuralProvider
        // unchanged. It reports Embedding so we can also see method plumbs through.
        struct IndexProvider;
        impl EmbeddingProvider for IndexProvider {
            fn score(&self, _query: &str, facts: &[Fact]) -> Vec<f64> {
                (0..facts.len()).map(|i| (i + 1) as f64).collect()
            }
            fn method(&self) -> RankMethod {
                RankMethod::Embedding
            }
        }
        let facts = sample_facts();
        let hits = rank("ignored by this provider", &facts, 2, &IndexProvider);
        // Highest index scores highest -> the last fact ranks first.
        assert_eq!(hits[0].index, facts.len() - 1);
        assert!(method_status(&IndexProvider).to_lowercase().contains("embedding"));
    }

    // ---- proactive surface helper (read-only, no speak) --------------------

    #[test]
    fn relevant_context_offers_text_only_when_relevant() {
        let facts = sample_facts();
        // Relevant -> a composed, grounded "you've mentioned" block.
        let ctx = relevant_context("we were talking about my car", &facts, 2, &lex())
            .expect("relevant context for a matching topic");
        assert!(ctx.contains("user.car"), "grounded in the real fact: {ctx}");
        assert!(ctx.to_lowercase().contains("mentioned"), "{ctx}");
        // Nothing relevant -> None (offers nothing; never fabricates).
        assert!(
            relevant_context("the weather on neptune", &facts, 2, &lex()).is_none(),
            "no relevant fact -> no offer"
        );
        // Empty store -> None.
        assert!(relevant_context("car", &[], 2, &lex()).is_none());
    }

    // ---- length normalization sanity ---------------------------------------

    #[test]
    fn length_normalization_does_not_let_a_long_fact_drown_a_focused_one() {
        // A short focused fact about "rust" vs a long fact that mentions "rust"
        // once among many words: BM25's length normalization should keep the
        // focused fact competitive (it is the more relevant of the two).
        let facts = vec![
            Fact::new("user.lang", "rust"),
            Fact::new(
                "note.long",
                "today I went to the market and bought apples bread milk and \
                 also briefly thought about rust while walking the long way home",
            ),
        ];
        let hits = rank("rust", &facts, 2, &lex());
        assert_eq!(
            hits[0].fact.key, "user.lang",
            "the focused fact should rank above the long incidental mention: {hits:?}"
        );
    }

    // =====================================================================
    // NeuralEmbeddingProvider — cosine ranking over INJECTED/mocked vectors
    // (the embedding CALL is runtime/MLX-gated and never exercised here)
    // =====================================================================

    // ---- cosine helper correctness -----------------------------------------

    #[test]
    fn cosine_similarity_is_correct_and_bounded() {
        // Identical direction -> 1.0; orthogonal -> 0.0; opposite -> -1.0.
        assert!((cosine_similarity(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-9);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]) - 0.0).abs() < 1e-9);
        assert!((cosine_similarity(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-9);
        // A general 45-degree case: cos(45deg) = 1/sqrt(2).
        let s = cosine_similarity(&[1.0, 0.0], &[1.0, 1.0]);
        assert!((s - (1.0 / 2f64.sqrt())).abs() < 1e-9, "got {s}");
        // Degenerate inputs are honest zeros, never NaN/Inf.
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0]), 0.0, "length mismatch -> 0");
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0, "zero vector -> 0");
    }

    /// Build a NeuralEmbeddingProvider over the given facts with mock vectors.
    /// The query points along axis 0; each fact's vector is supplied so we can
    /// pin exactly which fact is "semantically near" the query.
    fn neural(query: Vec<f64>, fact_vectors: Vec<Vec<f64>>) -> NeuralEmbeddingProvider {
        NeuralEmbeddingProvider::new(query, fact_vectors)
    }

    #[test]
    fn neural_ranks_the_relevant_fact_above_irrelevant_ones() {
        let facts = vec![
            Fact::new("user.car", "blue Subaru Outback"),
            Fact::new("user.pet", "a corgi named Watson"),
            Fact::new("user.coffee", "oat-milk flat whites"),
        ];
        // Query embedding points along axis 0. The car fact's vector is nearly
        // parallel (high cosine); pet/coffee are orthogonal-ish (low cosine).
        let query = vec![1.0, 0.0, 0.0];
        let fact_vectors = vec![
            vec![0.9, 0.1, 0.0],  // car: near the query
            vec![0.0, 1.0, 0.0],  // pet: orthogonal
            vec![0.1, 0.0, 0.95], // coffee: mostly orthogonal
        ];
        let p = neural(query, fact_vectors);
        let hits = rank("ignored: vectors drive the score", &facts, 3, &p);
        assert!(!hits.is_empty(), "the relevant fact must be retrieved");
        assert_eq!(
            hits[0].fact.key, "user.car",
            "the semantically nearest fact must rank first: {hits:?}"
        );
        // Relevant strictly outranks the orthogonal pet fact.
        let car = hits.iter().find(|h| h.fact.key == "user.car").unwrap().score;
        let pet = hits.iter().find(|h| h.fact.key == "user.pet").map(|h| h.score);
        if let Some(pet) = pet {
            assert!(car > pet, "relevant cosine ({car}) must exceed irrelevant ({pet})");
        }
    }

    #[test]
    fn neural_scoring_is_deterministic() {
        let facts = vec![
            Fact::new("a", "alpha"),
            Fact::new("b", "beta"),
            Fact::new("c", "gamma"),
        ];
        let query = vec![0.2, 0.5, 0.83];
        let fv = vec![
            vec![0.2, 0.5, 0.83],
            vec![0.9, 0.1, 0.0],
            vec![0.0, 0.0, 1.0],
        ];
        let a = rank("q", &facts, 3, &neural(query.clone(), fv.clone()));
        let b = rank("q", &facts, 3, &neural(query, fv));
        assert_eq!(a, b, "identical mocked vectors yield the identical ranking");
    }

    #[test]
    fn neural_clamps_negative_cosine_so_anti_correlated_is_not_a_hit() {
        // A fact whose embedding points AWAY from the query (negative cosine)
        // must NOT be surfaced — rank() drops non-positive scores, so a fact
        // that is at best "unrelated" is never returned as a fabricated memory.
        let facts = vec![
            Fact::new("user.car", "blue Subaru"),
            Fact::new("user.opposite", "points the other way"),
        ];
        let query = vec![1.0, 0.0];
        let fv = vec![
            vec![1.0, 0.0],  // cosine +1
            vec![-1.0, 0.0], // cosine -1 -> clamped to 0 -> dropped
        ];
        let hits = rank("q", &facts, 5, &neural(query, fv));
        assert_eq!(hits.len(), 1, "only the positively-related fact is a hit: {hits:?}");
        assert_eq!(hits[0].fact.key, "user.car");
    }

    #[test]
    fn neural_no_match_returns_nothing_never_fabricates() {
        // All facts orthogonal to the query (cosine ~0) -> no positive hits ->
        // zero results: the neural backend honors the no-fabrication contract.
        let facts = vec![Fact::new("user.pet", "corgi"), Fact::new("user.coffee", "latte")];
        let query = vec![1.0, 0.0, 0.0];
        let fv = vec![vec![0.0, 1.0, 0.0], vec![0.0, 0.0, 1.0]];
        let hits = rank("q", &facts, 5, &neural(query, fv));
        assert!(hits.is_empty(), "orthogonal facts are not surfaced: {hits:?}");
    }

    #[test]
    fn neural_reports_embedding_method_honestly() {
        let p = neural(vec![1.0], vec![vec![1.0]]);
        assert_eq!(p.method(), RankMethod::Embedding);
        let status = method_status(&p);
        let lower = status.to_lowercase();
        assert!(lower.contains("neural"), "status must name neural: {status}");
        assert!(
            lower.contains("on-device") && lower.contains("embedding"),
            "status must name on-device embeddings: {status}"
        );
        assert!(
            lower.contains("inference server"),
            "must state neural needs the inference server: {status}"
        );
        // Honesty: it names the MECHANISM, never a measured quality claim.
        assert!(
            !lower.contains("better") && !lower.contains("more accurate"),
            "must not claim measured quality: {status}"
        );
    }

    #[test]
    fn neural_mismatched_vector_count_scores_zero_not_panic() {
        // Defensive: if the caller hands fewer vectors than facts, score 0.0 for
        // all (no hits) rather than panic or fabricate.
        let facts = vec![Fact::new("a", "x"), Fact::new("b", "y")];
        let p = neural(vec![1.0], vec![vec![1.0]]); // 1 vector for 2 facts
        let hits = rank("q", &facts, 5, &p);
        assert!(hits.is_empty(), "mismatched counts -> no hits: {hits:?}");
    }

    // =====================================================================
    // Runtime backend selection: prefer neural, fall back to lexical (honest)
    // =====================================================================

    /// A mock embedder driven by a canned outcome — NEVER touches a socket or
    /// MLX (the real call is runtime-gated and untested by contract).
    /// How the mock's STAGE-TWO rerank behaves (None on the plain mock = rerank
    /// disabled, the trait default). Lets a test drive a KNOWN reorder or an honest
    /// fallback without a socket.
    #[derive(Clone)]
    enum MockRerank {
        /// Enabled; each passage scores +1.0 iff it contains `boost`, else 0.0 —
        /// so a test forces a deterministic reorder (the boosted passage floats up).
        Boost(&'static str),
        /// Enabled but the reranker FELL BACK (configured-but-unavailable): the
        /// caller must keep the dense order (method stays Embedding/Lexical).
        FellBack,
    }

    struct MockEmbedder {
        /// `Ok(vectors)` to simulate the embed op answering; `Err` to simulate
        /// the inference server being down / lacking the op.
        outcome: anyhow::Result<Vec<Vec<f64>>>,
        /// STAGE-TWO rerank behavior; `None` = reranking disabled (trait default).
        rerank: Option<MockRerank>,
    }

    impl MockEmbedder {
        fn answering(vectors: Vec<Vec<f64>>) -> Self {
            Self {
                outcome: Ok(vectors),
                rerank: None,
            }
        }
        fn down() -> Self {
            Self {
                outcome: Err(anyhow::anyhow!("inference socket unavailable (simulated)")),
                rerank: None,
            }
        }
        /// Enable stage-two rerank that boosts any passage containing `kw`.
        fn with_rerank_boost(mut self, kw: &'static str) -> Self {
            self.rerank = Some(MockRerank::Boost(kw));
            self
        }
        /// Enable stage-two rerank that HONESTLY falls back (dense order kept).
        fn with_rerank_fellback(mut self) -> Self {
            self.rerank = Some(MockRerank::FellBack);
            self
        }
    }

    impl Embedder for MockEmbedder {
        fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
            // Clone the canned outcome (and validate the daemon batched query+facts).
            let n = texts.len();
            let outcome = match &self.outcome {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            };
            Box::pin(async move {
                // The live caller sends [query, fact0, fact1, ...]; assert the
                // batch shape so the mock mirrors the real wire contract.
                assert!(n >= 1, "embed batch must include at least the query");
                outcome
            })
        }

        fn rerank_enabled(&self) -> bool {
            self.rerank.is_some()
        }

        fn rerank<'a>(&'a self, _query: &'a str, passages: &'a [String]) -> RerankFuture<'a> {
            let beh = self.rerank.clone();
            let passages = passages.to_vec();
            Box::pin(async move {
                match beh {
                    Some(MockRerank::Boost(kw)) => Ok(RerankOutcome {
                        scores: passages
                            .iter()
                            .map(|p| if p.contains(kw) { 1.0 } else { 0.0 })
                            .collect(),
                        reranker: Some("mock-reranker".to_string()),
                        fell_back: false,
                    }),
                    // Fell back / disabled -> the honest "keep dense order" outcome.
                    _ => Ok(RerankOutcome::unavailable()),
                }
            })
        }
    }

    /// The PROVIDED `embed_with_space` default delegates to `embed` and reports
    /// NO space metadata (embedder/dim None, fell_back false) — byte-for-byte
    /// what an old inference server sends, which docsearch keys as the legacy
    /// mean-pool space. A mock implementing only `embed` therefore behaves as a
    /// space-unaware (legacy) embedder without any change.
    #[tokio::test]
    async fn embed_with_space_default_delegates_and_reports_no_metadata() {
        let embedder = MockEmbedder::answering(vec![vec![1.0, 0.0]]);
        let batch = embedder
            .embed_with_space(&["query".to_string()])
            .await
            .expect("delegates to the answering embed");
        assert_eq!(batch.vectors, vec![vec![1.0, 0.0]]);
        assert_eq!(batch.embedder, None, "no metadata on the default path");
        assert_eq!(batch.dim, None);
        assert!(!batch.fell_back);

        // A down embedder's Err propagates through the default unchanged.
        assert!(MockEmbedder::down()
            .embed_with_space(&["query".to_string()])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn runtime_prefers_neural_when_embedder_answers() {
        let facts = vec![
            Fact::new("user.car", "blue Subaru Outback"),
            Fact::new("user.pet", "a corgi named Watson"),
        ];
        // Batch is [query, car, pet]; query parallel to the car vector.
        let vectors = vec![
            vec![1.0, 0.0], // query
            vec![1.0, 0.0], // car: cosine 1
            vec![0.0, 1.0], // pet: cosine 0
        ];
        let embedder = MockEmbedder::answering(vectors);
        let out = rank_runtime_selected("about my car", &facts, 3, &embedder).await;
        assert_eq!(out.method, RankMethod::Embedding, "neural is preferred when it answers");
        assert!(
            out.method_status.to_lowercase().contains("neural"),
            "status reports neural: {}",
            out.method_status
        );
        assert_eq!(out.hits.len(), 1, "only the car is positively related: {:?}", out.hits);
        assert_eq!(out.hits[0].fact.key, "user.car");
    }

    #[tokio::test]
    async fn runtime_falls_back_to_lexical_when_embedder_is_down() {
        let facts = sample_facts();
        let embedder = MockEmbedder::down();
        let out = rank_runtime_selected("what about my car", &facts, 3, &embedder).await;
        assert_eq!(
            out.method,
            RankMethod::Lexical,
            "embedder down -> lexical BM25 fallback"
        );
        let lower = out.method_status.to_lowercase();
        assert!(lower.contains("lexical") && lower.contains("bm25"), "{}", out.method_status);
        assert!(
            lower.contains("not by a neural embedding") || lower.contains("not a neural"),
            "fallback status must NOT claim neural: {}",
            out.method_status
        );
        // Recall still WORKS lexically: the car fact is found.
        assert!(!out.hits.is_empty(), "lexical fallback still ranks");
        assert_eq!(out.hits[0].fact.key, "user.car");
    }

    // ---- STAGE TWO: cross-encoder rerank (config-gated, honest fallback) -----

    #[tokio::test]
    async fn rerank_reorders_the_dense_head_and_upgrades_the_method() {
        // Dense (cosine) order is [car, pet]; the reranker BOOSTS "corgi" (the pet),
        // so the final order is [pet, car] and the method upgrades to Reranked.
        let facts = vec![
            Fact::new("user.car", "blue Subaru Outback"),
            Fact::new("user.pet", "a corgi named Watson"),
        ];
        let vectors = vec![
            vec![1.0, 1.0], // query: equidistant -> dense ties break by index
            vec![1.0, 0.0], // car: index 0 -> dense first
            vec![0.0, 1.0], // pet: index 1 -> dense second
        ];
        let embedder = MockEmbedder::answering(vectors).with_rerank_boost("corgi");
        let out = rank_runtime_selected("about my things", &facts, 3, &embedder).await;
        assert_eq!(
            out.method,
            RankMethod::Reranked,
            "the cross-encoder re-scored the shortlist -> Reranked"
        );
        assert!(
            out.method_status.to_lowercase().contains("re-rank")
                || out.method_status.to_lowercase().contains("rerank"),
            "status names the rerank: {}",
            out.method_status
        );
        assert_eq!(out.hits.len(), 2);
        assert_eq!(
            out.hits[0].fact.key, "user.pet",
            "the boosted (corgi) fact is promoted to the top"
        );
        assert_eq!(out.hits[1].fact.key, "user.car");
    }

    #[tokio::test]
    async fn rerank_fellback_keeps_dense_order_and_method() {
        // The reranker is ENABLED but falls back (unavailable): the dense neural
        // order stands and the method stays Embedding — never mislabeled Reranked.
        let facts = vec![
            Fact::new("user.car", "blue Subaru Outback"),
            Fact::new("user.pet", "a corgi named Watson"),
        ];
        let vectors = vec![
            vec![1.0, 0.0], // query parallel to the car
            vec![1.0, 0.0], // car: cosine 1 -> dense first
            vec![0.6, 0.8], // pet: cosine 0.6 -> dense second (still positive)
        ];
        let embedder = MockEmbedder::answering(vectors).with_rerank_fellback();
        let out = rank_runtime_selected("about my things", &facts, 3, &embedder).await;
        assert_eq!(
            out.method,
            RankMethod::Embedding,
            "a fell-back reranker keeps the dense order + Embedding label"
        );
        assert_eq!(out.hits[0].fact.key, "user.car", "dense order preserved");
    }

    #[tokio::test]
    async fn rerank_disabled_is_byte_for_byte_the_dense_path() {
        // A mock with rerank DISABLED (trait default) must behave exactly like the
        // single-stage neural path (method Embedding, dense order).
        let facts = vec![
            Fact::new("user.car", "blue Subaru Outback"),
            Fact::new("user.pet", "a corgi named Watson"),
        ];
        let vectors = vec![vec![1.0, 0.0], vec![1.0, 0.0], vec![0.0, 1.0]];
        let embedder = MockEmbedder::answering(vectors); // no .with_rerank_*
        let out = rank_runtime_selected("about my car", &facts, 3, &embedder).await;
        assert_eq!(out.method, RankMethod::Embedding);
        assert_eq!(out.hits[0].fact.key, "user.car");
    }

    #[tokio::test]
    async fn rerank_permutation_sorts_by_score_desc_stable() {
        // The pure permutation helper: boost the middle passage; it must float to
        // the front, the rest keep their dense order (stable tie-break).
        let passages = vec![
            "the user drives a car".to_string(),
            "the user has a corgi".to_string(),
            "the user drinks coffee".to_string(),
        ];
        let embedder = MockEmbedder::answering(vec![]).with_rerank_boost("corgi");
        let perm = rerank_permutation("pets", &passages, &embedder)
            .await
            .expect("reranker answered -> Some(permutation)");
        assert_eq!(perm[0], 1, "the boosted (corgi) passage is ranked first");
        // The two 0-scored passages keep their dense order (stable): 0 before 2.
        assert_eq!(&perm[1..], &[0, 2]);
    }

    #[tokio::test]
    async fn rerank_permutation_is_none_when_disabled_or_trivial() {
        let passages = vec!["a".to_string(), "b".to_string()];
        // Disabled reranker -> None (keep dense).
        assert!(
            rerank_permutation("q", &passages, &MockEmbedder::answering(vec![]))
                .await
                .is_none()
        );
        // Trivial shortlist (<2) -> None even when enabled.
        let one = vec!["only".to_string()];
        assert!(
            rerank_permutation("q", &one, &MockEmbedder::answering(vec![]).with_rerank_boost("x"))
                .await
                .is_none()
        );
    }

    #[test]
    fn reranked_method_token_and_description_are_honest() {
        assert_eq!(RankMethod::Reranked.as_str(), "neural-reranked");
        let d = RankMethod::Reranked.description();
        assert!(d.contains("cross-encoder"), "names the cross-encoder: {d}");
        assert!(
            d.contains("keep the plain embedding order") || d.contains("unavailable"),
            "states the honest fallback: {d}"
        );
    }

    #[tokio::test]
    async fn runtime_falls_back_when_embedder_returns_wrong_vector_count() {
        let facts = sample_facts(); // 5 facts -> batch of 6 (query + 5)
        // The embedder answers but returns too few vectors -> treat as broken,
        // fall back to lexical honestly rather than rank on garbage.
        let embedder = MockEmbedder::answering(vec![vec![1.0, 0.0], vec![1.0, 0.0]]);
        let out = rank_runtime_selected("my car", &facts, 3, &embedder).await;
        assert_eq!(out.method, RankMethod::Lexical, "bad vector count -> lexical fallback");
        assert!(!out.hits.is_empty(), "fallback still produces the lexical ranking");
        assert_eq!(out.hits[0].fact.key, "user.car");
    }

    #[tokio::test]
    async fn runtime_empty_store_is_honest_under_either_backend() {
        // No facts: zero hits, no embed call needed, reported as lexical (what
        // would run). Never fabricates a memory.
        let embedder = MockEmbedder::answering(vec![]);
        let out = rank_runtime_selected("anything", &[], 5, &embedder).await;
        assert!(out.hits.is_empty(), "empty store -> no hits");
        assert_eq!(out.method, RankMethod::Lexical);

        // k == 0 likewise yields nothing.
        let facts = sample_facts();
        let out = rank_runtime_selected("car", &facts, 0, &MockEmbedder::down()).await;
        assert!(out.hits.is_empty(), "k=0 -> no hits");
    }

    #[tokio::test]
    async fn runtime_neural_no_match_does_not_fabricate() {
        // Embedder answers, but every fact is orthogonal to the query: neural
        // ran, yet honestly returns ZERO hits (no fabricated memory).
        let facts = vec![Fact::new("user.pet", "corgi"), Fact::new("user.coffee", "latte")];
        let vectors = vec![
            vec![1.0, 0.0, 0.0], // query
            vec![0.0, 1.0, 0.0], // pet: orthogonal
            vec![0.0, 0.0, 1.0], // coffee: orthogonal
        ];
        let out = rank_runtime_selected("space telescopes", &facts, 5, &MockEmbedder::answering(vectors)).await;
        assert_eq!(out.method, RankMethod::Embedding, "neural ran");
        assert!(out.hits.is_empty(), "neural no-match returns nothing: {:?}", out.hits);
    }

    #[tokio::test]
    async fn runtime_neural_degenerate_query_vector_falls_back() {
        // The embedder answers the right count but the QUERY vector is empty
        // (degenerate) -> unusable -> fall back to lexical honestly.
        let facts = vec![Fact::new("user.car", "blue Subaru")];
        let vectors = vec![vec![], vec![1.0, 0.0]]; // query empty, fact present
        let out = rank_runtime_selected("my car", &facts, 3, &MockEmbedder::answering(vectors)).await;
        assert_eq!(out.method, RankMethod::Lexical, "degenerate query embedding -> fallback");
        assert_eq!(out.hits[0].fact.key, "user.car");
    }
}
