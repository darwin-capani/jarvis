//! The EPISODIC STORE — DARWIN's redacted, agent-scoped, bounded memory of
//! completed interactions, and the recall over it.
//!
//! An EPISODE is a single durable record of one completed turn: { id, ts,
//! agent_namespace, utterance_redacted, topic, salient_entities, outcome,
//! summary }. The row + the raw temporal Db readers (recent/since/around) live
//! in [`crate::memory`] (it owns the SQLite connection + the bounded
//! retention_pass); THIS module is the higher-level surface the daemon and the
//! Mnemosyne recall tool use:
//!   * [`record_episode`] — the per-turn WRITE path, GATED (see below) and
//!     REDACTING every field through [`crate::optimize::redact`] before store;
//!   * [`episodic_recall`] — the combined recall: a TOPICAL BM25 ranking (reusing
//!     [`crate::recall`], the same ranker Mnemosyne's fact recall uses) over the
//!     agent-scoped recent window, optionally narrowed to a temporal window
//!     (since / around) first — newest-first when there is no query.
//!
//! ## The CONTRACT (non-negotiable, mirrors the rest of the memory layer)
//!   * PRIVACY / HONESTY: an episode is built ONLY from an OBSERVED interaction —
//!     `topic`/`salient_entities`/`summary` are DERIVED from the real utterance +
//!     routing, never invented; recall returns ONLY episodes that were really
//!     recorded (an empty / no-match recall says so, it never fabricates one).
//!   * REDACTED: every free-text field is passed through [`crate::optimize::redact`]
//!     BEFORE store (and the Db re-redacts defensively), so a secret in an
//!     utterance never reaches the stored episode.
//!   * AGENT-SCOPED: an episode recorded under one agent stays in THAT agent's
//!     recall (plus the shared orchestrator tier), never another specialist's —
//!     mirroring `agent_scoped_facts`.
//!   * BOUNDED: the table is capped (evict-oldest) in `memory::retention_pass`;
//!     the store remembers the RECENT past, NOT "everything forever".
//!   * GATED at the recording site: a SCREEN-READ-TRANSIENT turn (sensitive,
//!     never durable) and a VOICE-ID-UNVERIFIED turn (not confidently the owner)
//!     are NOT recorded; nor is an empty/abandoned turn. The gate is
//!     [`should_record`], unit-tested here, and called from main.rs exactly where
//!     record_transcript + spawn_learning_task already run.
//!
//! Nothing here speaks, acts, or reaches the network. It records observed turns
//! and ranks recorded ones.

use anyhow::Result;

use crate::config::Config;
use crate::memory::{Episode, Memory};
use crate::recall::{rank_runtime_selected, Embedder, Fact, RankMethod};

/// How many salient content words an episode keeps. A handful — enough to anchor
/// what the turn was about for the topical ranker, bounded so the row stays tiny.
const MAX_SALIENT_ENTITIES: usize = 5;

/// Minimum length of a salient content word (shorter tokens are glue/noise).
const SALIENT_MIN_LEN: usize = 4;

/// Default / max number of episodes a single `episodic_recall` returns. Bounded
/// so one recall is small and the surface stays focused on the relevant few.
pub const EPISODIC_DEFAULT_K: usize = 5;
pub const EPISODIC_MAX_K: usize = 20;

/// The shared/orchestrator episodic namespace — the scope conversational turns
/// record under by default (the "agent.darwin" tier; see the namespace docs at
/// the top of this module). The proactive-intelligence live pass mines THIS
/// scope. A specialist's private episodes live under their own namespace and are
/// never mixed in here.
pub const DEFAULT_NAMESPACE: &str = "agent.darwin";

/// The generous agent-scoped window the topical ranker scores over. Recall ranks
/// the recent past, not the entire (capped) store — bounded twice over (window
/// here, k at the result).
const RECALL_WINDOW: usize = 200;

/// A combined episodic recall result: the ranked episodes plus the backend that
/// actually produced the ranking (so the caller reports the method honestly —
/// neural on-device embeddings, or lexical BM25 on fallback — exactly like
/// Mnemosyne's fact recall), and whether a temporal narrowing was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodicRecall {
    /// The matched episodes, most-relevant first (or newest-first for a pure
    /// temporal recall). Only ever REAL recorded episodes.
    pub episodes: Vec<Episode>,
    /// The ranking backend that ACTUALLY ran (Lexical unless the neural embedder
    /// answered). For a pure temporal recall (no query) there is no ranking, so
    /// this is the temporal marker.
    pub method: RecallMethod,
}

/// Which mechanism produced a recall — named honestly for the caller's copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallMethod {
    /// Newest-first temporal recall (no topical query): the episodes are ordered
    /// by recency, no relevance ranking was applied.
    Temporal,
    /// Lexical BM25 topical ranking (the honest fallback when the on-device
    /// embedder is unavailable).
    Lexical,
    /// Neural on-device embedding topical ranking (the embedder answered).
    Embedding,
}

impl RecallMethod {
    /// One honest human sentence naming the method — what a recall surface
    /// reports so a user is never misled about whether recall was neural.
    pub fn description(&self) -> &'static str {
        match self {
            RecallMethod::Temporal => {
                "temporal recall: I returned your most recent recorded episodes \
                 in time order — no relevance ranking was applied."
            }
            RecallMethod::Lexical => {
                "lexical-semantic recall: I ranked your recorded episodes by BM25 \
                 term relevance — keyword-semantic, not a neural embedding model."
            }
            RecallMethod::Embedding => {
                "neural (on-device embeddings) recall: I ranked your recorded \
                 episodes by cosine similarity over on-device embedding vectors; \
                 it needs the inference server running and falls back to lexical \
                 BM25 when that server is down."
            }
        }
    }
}

impl From<RankMethod> for RecallMethod {
    fn from(m: RankMethod) -> Self {
        match m {
            RankMethod::Lexical => RecallMethod::Lexical,
            RankMethod::Embedding => RecallMethod::Embedding,
        }
    }
}

/// An OPTIONAL temporal narrowing for [`episodic_recall`]: restrict the candidate
/// episodes to a time window BEFORE the topical ranking (or, with no query, just
/// return that window newest-first). `None` means "the whole recent window".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum When {
    /// Episodes recorded strictly after this RFC3339 instant.
    Since(String),
    /// Episodes recorded within the inclusive [from, to] RFC3339 window.
    Around { from: String, to: String },
}

// ---------------------------------------------------------------------------
// The RECORDING GATE — what may be durably remembered as an episode
// ---------------------------------------------------------------------------

/// The per-turn voice-id posture as the episodic gate sees it. Built from the
/// daemon's runtime state: whether voice-id is enabled at all, whether an owner
/// profile is enrolled, and (when both) whether THIS turn verified as the owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceGate {
    /// `[voice_id].enabled` — the master switch.
    pub enabled: bool,
    /// Whether an owner profile is enrolled (verification is only meaningful
    /// when one exists).
    pub enrolled: bool,
    /// Whether THIS turn's speaker verified as the owner (only consulted when
    /// enabled && enrolled).
    pub owner_verified: bool,
}

impl VoiceGate {
    /// Build the episodic voice posture from the daemon's per-turn
    /// [`crate::voiceid::OwnerGate`] (the SAME process-global gate
    /// `execute_tool`/`replay_confirmed_action` read deep in the tool loop). The
    /// mapping is exact: `enforcing` == (voice-id enabled AND a profile enrolled),
    /// and `verified` == this turn verified as the owner. So an episode is
    /// recorded under EXACTLY the posture a consequential action may fire under —
    /// an unrecognized speaker on an enforcing turn can neither act outwardly nor
    /// seed the owner's durable episodic memory.
    pub fn from_owner_gate(g: crate::voiceid::OwnerGate) -> Self {
        Self {
            // `enabled && enrolled` is exactly OwnerGate::enforcing; we fold both
            // into the pair so `permits_recording` reads the same as today.
            enabled: g.enforcing,
            enrolled: g.enforcing,
            owner_verified: g.verified,
        }
    }

    /// The voice-id posture under which an episode MAY be recorded:
    ///   * voice-id OFF, or no owner enrolled  -> record (behavior is unchanged
    ///     from a no-voice-id appliance; we never silently stop remembering just
    ///     because the feature exists but is unconfigured); OR
    ///   * voice-id ON and enrolled AND this turn OWNER-VERIFIED -> record.
    ///     A voice-id-ON, enrolled, but UNVERIFIED turn (an unrecognized speaker) is
    ///     NOT recorded — that turn is not confidently the owner's, so it must not
    ///     seed the owner's durable episodic memory.
    fn permits_recording(&self) -> bool {
        if self.enabled && self.enrolled {
            self.owner_verified
        } else {
            true
        }
    }
}

/// THE GATE. May this completed turn be recorded as a durable episode? It is a
/// PURE predicate (unit-tested) so the recording decision is auditable in one
/// place. An episode is recorded ONLY when ALL hold:
///   * `enabled` — `[episodic].enabled` (the master switch);
///   * NOT `transient` — a screen-read turn is sensitive + ephemeral and is
///     NEVER made durable (mirrors the transcript/learning-loop transient skip);
///   * the turn is NOT empty/abandoned — there is a real utterance AND a real
///     response (an abandoned turn carries no usable interaction to remember);
///   * the voice-id posture permits it ([`VoiceGate::permits_recording`]).
pub fn should_record(
    enabled: bool,
    transient: bool,
    utterance: &str,
    response: &str,
    voice: VoiceGate,
) -> bool {
    enabled
        && !transient
        && !utterance.trim().is_empty()
        && !response.trim().is_empty()
        && voice.permits_recording()
}

// ---------------------------------------------------------------------------
// FIELD DERIVATION — topic / salient entities / summary, from OBSERVED text
// ---------------------------------------------------------------------------

/// Stopwords excluded from salient-entity mining — low-signal glue that anchors
/// nothing. Small + hardcoded (same spirit as recall.rs's stoplist); the goal is
/// to drop obvious noise, not run a full NLP pipeline. The redaction placeholder
/// is included so a `[redacted]` span is never mistaken for a salient entity.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "of", "in", "on", "for", "my", "me",
    "i", "is", "it", "this", "that", "with", "what", "how", "can", "you",
    "please", "do", "does", "did", "have", "has", "are", "was", "will", "would",
    "about", "up", "out", "get", "got", "now", "from", "by", "redacted",
    "your", "we", "our", "be", "been", "but", "so", "if", "then", "they",
];

/// Derive a few SALIENT ENTITIES from the (already-redacted) utterance: lowercase
/// alphanumeric content words that are not stopwords and are long enough to carry
/// topic, in first-seen order, deduplicated, capped at [`MAX_SALIENT_ENTITIES`].
/// Pure + deterministic. Because the input is already redacted, a secret span is
/// the `[redacted]` placeholder, which the stoplist drops — so a salient entity
/// is never a secret.
fn derive_salient_entities(redacted_utterance: &str) -> Vec<String> {
    let lower = redacted_utterance.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for tok in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if tok.len() < SALIENT_MIN_LEN {
            continue;
        }
        if STOPWORDS.contains(&tok) {
            continue;
        }
        let owned = tok.to_string();
        if out.contains(&owned) {
            continue;
        }
        out.push(owned);
        if out.len() == MAX_SALIENT_ENTITIES {
            break;
        }
    }
    out
}

/// Build a short, redacted SUMMARY of the turn: the utterance shape glued to a
/// trimmed gist of the response. Bounded length so the row stays tiny. Both
/// halves are redacted by the caller's redaction of the utterance + the
/// response; record_episode re-redacts the summary defensively at the Db.
fn derive_summary(redacted_utterance: &str, redacted_response: &str) -> String {
    /// Trim a string to at most `n` chars on a char boundary, adding an ellipsis
    /// when it was cut.
    fn clip(s: &str, n: usize) -> String {
        let s = s.trim();
        if s.chars().count() <= n {
            return s.to_string();
        }
        let cut: String = s.chars().take(n).collect();
        format!("{}…", cut.trim_end())
    }
    let u = clip(redacted_utterance, 120);
    let r = clip(redacted_response, 160);
    if r.is_empty() {
        u
    } else {
        format!("{u} -> {r}")
    }
}

// ---------------------------------------------------------------------------
// RECORD — the per-turn write path (REDACTED + agent-scoped + bounded)
// ---------------------------------------------------------------------------

/// Record one completed turn as an episode IF (and only if) [`should_record`]
/// permits it. This is the function the daemon calls per completed turn, right
/// where `record_transcript` + `spawn_learning_task` already run and under the
/// SAME transient/voice-id gates. When the gate refuses, this is a pure NO-OP
/// (returns `Ok(false)` and writes nothing).
///
/// Every field is REDACTED here (utterance + response → derived summary +
/// entities) via [`crate::optimize::redact`] BEFORE the Episode is built, and the
/// Db re-redacts the free-text fields defensively — so a secret in the utterance
/// (or response) NEVER reaches the stored episode. The episode is recorded under
/// `agent_namespace` so recall stays agent-scoped.
///
/// Returns `Ok(true)` when an episode was recorded, `Ok(false)` when the gate
/// refused (so the caller can emit honest telemetry either way).
#[allow(clippy::too_many_arguments)]
pub async fn record_episode(
    cfg: &Config,
    memory: &Memory,
    agent_namespace: &str,
    raw_utterance: &str,
    raw_response: &str,
    topic: &str,
    transient: bool,
    voice: VoiceGate,
) -> Result<bool> {
    if !should_record(
        cfg.episodic.enabled,
        transient,
        raw_utterance,
        raw_response,
        voice,
    ) {
        return Ok(false); // gated out: record NOTHING.
    }
    // Redact at the source — the utterance and the response — BEFORE deriving any
    // field from them, so nothing downstream (entities, summary) can carry a
    // secret the redactor would have stripped.
    let utterance_redacted = crate::optimize::redact(raw_utterance);
    let response_redacted = crate::optimize::redact(raw_response);
    let salient_entities = derive_salient_entities(&utterance_redacted);
    let summary = derive_summary(&utterance_redacted, &response_redacted);
    let episode = Episode {
        id: 0,
        ts: String::new(), // stamped by the Db at insert
        agent_namespace: agent_namespace.to_string(),
        utterance_redacted,
        topic: topic.to_string(),
        salient_entities,
        outcome: "ok".to_string(),
        summary,
    };
    memory.record_episode(&episode).await?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// RECALL — temporal + topical, combined, agent-scoped, honest
// ---------------------------------------------------------------------------

/// The searchable text of an episode for the TOPICAL ranker: its summary +
/// utterance + salient entities + topic. (The summary already folds in the
/// response gist, so a query phrased in the assistant's words still matches.)
fn episode_searchable(ep: &Episode) -> String {
    format!(
        "{} {} {} {}",
        ep.summary,
        ep.utterance_redacted,
        ep.salient_entities.join(" "),
        ep.topic
    )
}

/// Normalize a `When` bound to canonical UTC RFC3339 before it reaches the SQL
/// string comparison in `episodes_since` / `episodes_around`. Those compare the
/// bound LEXICALLY against the stored episode ts (which is always UTC, "...Z" /
/// "+00:00"), so a bound carrying a different offset (e.g. "...+02:00") would
/// string-compare to the WRONG side of the window. We parse with
/// `parse_from_rfc3339` (accepts ANY offset), convert to UTC, and reformat as
/// canonical UTC RFC3339 — so the bound selects by TRUE INSTANT, not by lexical
/// offset. UNPARSEABLE-bound policy: pass the original string through UNCHANGED
/// (honest, non-panicking degrade — a malformed bound simply recalls as today's
/// behavior rather than panicking or silently widening/dropping the window).
fn to_utc_bound(bound: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(bound) {
        Ok(dt) => dt.with_timezone(&chrono::Utc).to_rfc3339(),
        Err(_) => bound.to_string(),
    }
}

/// COMBINED episodic recall, AGENT-SCOPED, HONEST. The single entry point the
/// `episodic_recall` tool and the user-model stage consume.
///
///   * Candidate set: the agent-scoped recent window (own namespace + shared
///     orchestrator tier), OPTIONALLY narrowed by `when` (Since / Around) FIRST.
///   * If `query` is empty/whitespace: a pure TEMPORAL recall — the candidates
///     newest-first, capped at `k`. No ranking, reported as `Temporal`.
///   * Otherwise: a TOPICAL ranking of the candidates via
///     [`crate::recall::rank_runtime_selected`] (neural on-device embeddings when
///     the injected `embedder` answers, else lexical BM25 — the SAME runtime
///     selection Mnemosyne's fact recall uses). The reported method NAMES the
///     backend that ACTUALLY ran. Zero-score (irrelevant) episodes are dropped,
///     so a no-match query returns NOTHING — never a fabricated episode.
///
/// ISOLATION: scoping is enforced at the Db read (own + shared only), so this can
/// never surface another specialist's private episodes. A failed store read
/// degrades to an EMPTY recall (a busy DB must never kill a reply), reported as
/// the temporal/lexical default — it never errs and never invents an episode.
pub async fn episodic_recall(
    memory: &Memory,
    namespace: &str,
    query: &str,
    when: Option<&When>,
    k: usize,
    embedder: &dyn Embedder,
) -> EpisodicRecall {
    let k = k.clamp(1, EPISODIC_MAX_K);

    // 1. Candidate set: temporal narrowing first, else the recent window. The
    //    Since/Around bounds are NORMALIZED to canonical UTC RFC3339 here, the
    //    single chokepoint, so a bound carrying a non-UTC offset compares against
    //    the stored (UTC) ts by TRUE INSTANT — never by lexical offset (see
    //    `to_utc_bound`). episodes_since/episodes_around do a SQL string compare,
    //    so an un-normalized "+02:00" bound would otherwise select wrongly.
    let candidates: Vec<Episode> = match when {
        Some(When::Since(ts)) => memory
            .episodes_since(namespace, &to_utc_bound(ts), RECALL_WINDOW)
            .await
            .unwrap_or_default(),
        Some(When::Around { from, to }) => memory
            .episodes_around(namespace, &to_utc_bound(from), &to_utc_bound(to), RECALL_WINDOW)
            .await
            .unwrap_or_default(),
        None => memory
            .episodes_scoped(namespace, RECALL_WINDOW)
            .await
            .unwrap_or_default(),
    };

    // 2. No query -> pure temporal recall (newest-first, capped at k).
    if query.trim().is_empty() {
        let mut episodes = candidates;
        episodes.truncate(k);
        return EpisodicRecall {
            episodes,
            method: RecallMethod::Temporal,
        };
    }

    // 3. Topical ranking over the candidates. Map each episode to a recall::Fact
    //    (key = the row id so we can map a hit back to its episode; value = the
    //    searchable text), rank runtime-selected, then re-materialize the ranked
    //    episodes in hit order. Zero-score episodes are dropped by rank() — no
    //    fabrication on a no-match query.
    if candidates.is_empty() {
        return EpisodicRecall {
            episodes: Vec::new(),
            method: RecallMethod::Lexical, // what would have ranked (nothing to rank)
        };
    }
    let facts: Vec<Fact> = candidates
        .iter()
        .map(|ep| Fact {
            key: ep.id.to_string(),
            value: episode_searchable(ep),
        })
        .collect();
    let recall = rank_runtime_selected(query, &facts, k, embedder).await;
    // rank() returns hits in relevance order, each carrying the original index
    // into `facts` (parallel to `candidates`), so map index -> episode directly.
    let episodes: Vec<Episode> = recall
        .hits
        .iter()
        .filter_map(|h| candidates.get(h.index).cloned())
        .collect();
    EpisodicRecall {
        episodes,
        method: recall.method.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recall::{EmbedFuture, Embedder};
    use std::path::PathBuf;

    // ---- a mock embedder so recall tests stay hermetic (no socket/MLX) ------

    /// Always errs, so `rank_runtime_selected` falls back to lexical BM25 — the
    /// backend the hermetic tests exercise (the neural path is runtime-gated).
    struct LexicalOnlyEmbedder;
    impl Embedder for LexicalOnlyEmbedder {
        fn embed<'a>(&'a self, _texts: &'a [String]) -> EmbedFuture<'a> {
            Box::pin(async move { Err(anyhow::anyhow!("no embedder in tests (BM25 fallback)")) })
        }
    }

    /// Unique temp DB per test; tests run concurrently in one process.
    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-episodic-test-{}-{}.db",
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

    /// A config with [episodic].enabled = true (the shipped default) for the
    /// recording-path tests.
    fn cfg_on() -> Config {
        Config::default()
    }

    /// The voice posture for an ordinary turn on an appliance with voice-id OFF —
    /// recording is permitted (behavior unchanged from a no-voice-id box).
    fn voice_off() -> VoiceGate {
        VoiceGate { enabled: false, enrolled: false, owner_verified: false }
    }

    // =====================================================================
    // The recording GATE (pure predicate)
    // =====================================================================

    #[test]
    fn gate_records_an_ordinary_completed_owner_turn() {
        // voice-id off: record. voice-id on + enrolled + verified: record.
        assert!(should_record(true, false, "what's the weather", "It's sunny.", voice_off()));
        assert!(should_record(
            true,
            false,
            "what's the weather",
            "It's sunny.",
            VoiceGate { enabled: true, enrolled: true, owner_verified: true },
        ));
    }

    #[test]
    fn gate_skips_a_screen_read_transient_turn() {
        // A screen-read turn is sensitive + ephemeral: NEVER recorded, even with
        // a real utterance/response and a verified owner.
        assert!(!should_record(
            true,
            true, // transient
            "what's on my screen",
            "I see a login form.",
            VoiceGate { enabled: true, enrolled: true, owner_verified: true },
        ));
    }

    #[test]
    fn gate_skips_a_voice_id_unverified_turn() {
        // voice-id ON and enrolled but THIS turn did not verify as the owner:
        // not confidently the owner -> NOT recorded.
        assert!(!should_record(
            true,
            false,
            "transfer money",
            "Done.",
            VoiceGate { enabled: true, enrolled: true, owner_verified: false },
        ));
        // But voice-id on with NO enrolled profile records (verification is
        // meaningless without a profile — behavior unchanged from off).
        assert!(should_record(
            true,
            false,
            "transfer money",
            "Done.",
            VoiceGate { enabled: true, enrolled: false, owner_verified: false },
        ));
    }

    #[test]
    fn gate_skips_empty_or_abandoned_turns() {
        // No usable utterance, or no usable response -> nothing to remember.
        assert!(!should_record(true, false, "   ", "a reply", voice_off()));
        assert!(!should_record(true, false, "a question", "  ", voice_off()));
    }

    #[test]
    fn gate_skips_when_episodic_disabled() {
        assert!(!should_record(false, false, "q", "r", voice_off()));
    }

    // =====================================================================
    // RECORD — redaction is enforced at the store
    // =====================================================================

    #[tokio::test]
    async fn a_secret_in_an_utterance_is_redacted_in_the_stored_episode() {
        let db = TempDb::new("redact");
        let mem = Memory::open(&db.0).unwrap();
        let recorded = record_episode(
            &cfg_on(),
            &mem,
            "agent.darwin",
            "my api key is sk-ABCDEF0123456789ABCD and email me at darwin@example.com",
            "Noted, I won't repeat it.",
            "conversation",
            false,
            voice_off(),
        )
        .await
        .unwrap();
        assert!(recorded, "an ordinary owner turn must be recorded");

        let eps = mem.episodes_recent("agent.darwin", 10).await.unwrap();
        assert_eq!(eps.len(), 1);
        let ep = &eps[0];
        // The secret + the email are GONE from every stored free-text field.
        assert!(
            !ep.utterance_redacted.contains("sk-ABCDEF0123456789ABCD"),
            "secret leaked into the stored utterance: {}",
            ep.utterance_redacted
        );
        assert!(
            !ep.utterance_redacted.contains("darwin@example.com"),
            "email leaked into the stored utterance: {}",
            ep.utterance_redacted
        );
        assert!(ep.utterance_redacted.contains("[redacted]"), "redaction placeholder present");
        assert!(!ep.summary.contains("sk-ABCDEF0123456789ABCD"), "secret leaked into summary");
        // The salient entities never include a secret span either.
        assert!(
            ep.salient_entities.iter().all(|e| !e.contains("sk")),
            "secret fragment leaked into salient entities: {:?}",
            ep.salient_entities
        );
    }

    #[tokio::test]
    async fn record_is_a_noop_when_gated_out() {
        let db = TempDb::new("noop");
        let mem = Memory::open(&db.0).unwrap();
        // transient -> not recorded.
        let r = record_episode(
            &cfg_on(), &mem, "agent.darwin", "what's on my screen", "a form", "conversation",
            true, voice_off(),
        )
        .await
        .unwrap();
        assert!(!r, "a transient turn returns false");
        assert_eq!(mem.episodes_count().await.unwrap(), 0, "nothing was written");
    }

    // =====================================================================
    // TEMPORAL recall — newest-first / since / around
    // =====================================================================

    /// Record n episodes under a namespace with explicit timestamps by writing
    /// rows directly (the Db stamps ts=now on record_episode, so for ordering
    /// tests we insert with controlled timestamps through a tiny helper).
    async fn seed(mem: &Memory, ns: &str, utterance: &str, topic: &str) {
        record_episode(&cfg_on(), mem, ns, utterance, "ok", topic, false, voice_off())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn temporal_recall_returns_newest_first() {
        let db = TempDb::new("temporal");
        let mem = Memory::open(&db.0).unwrap();
        // id is monotonic with insert order, and episodes_recent orders by id
        // DESC, so the LAST inserted is first regardless of same-second ts.
        seed(&mem, "agent.darwin", "first about the boat", "conversation").await;
        seed(&mem, "agent.darwin", "second about the plane", "conversation").await;
        seed(&mem, "agent.darwin", "third about the train", "conversation").await;

        let out = episodic_recall(&mem, "agent.darwin", "", None, 5, &LexicalOnlyEmbedder).await;
        assert_eq!(out.method, RecallMethod::Temporal);
        let utterances: Vec<&str> =
            out.episodes.iter().map(|e| e.utterance_redacted.as_str()).collect();
        assert_eq!(
            utterances,
            vec!["third about the train", "second about the plane", "first about the boat"],
            "newest episode must come first"
        );
    }

    #[tokio::test]
    async fn since_and_around_narrow_the_temporal_window() {
        let db = TempDb::new("since");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.darwin", "old episode about gardens", "conversation").await;
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        let cutoff = chrono::Utc::now().to_rfc3339();
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        seed(&mem, "agent.darwin", "new episode about rockets", "conversation").await;

        // since(cutoff): only the new episode.
        let out = episodic_recall(
            &mem, "agent.darwin", "", Some(&When::Since(cutoff.clone())), 5, &LexicalOnlyEmbedder,
        )
        .await;
        assert_eq!(out.episodes.len(), 1, "only the post-cutoff episode: {:?}", out.episodes);
        assert!(out.episodes[0].utterance_redacted.contains("rockets"));

        // around [epoch, cutoff]: only the old episode.
        let out = episodic_recall(
            &mem,
            "agent.darwin",
            "",
            Some(&When::Around {
                from: "1970-01-01T00:00:00+00:00".to_string(),
                to: cutoff.clone(),
            }),
            5,
            &LexicalOnlyEmbedder,
        )
        .await;
        assert_eq!(out.episodes.len(), 1, "only the pre-cutoff episode: {:?}", out.episodes);
        assert!(out.episodes[0].utterance_redacted.contains("gardens"));
    }

    /// The helper that normalizes a `When` bound to canonical UTC: a "+02:00" and
    /// a "-05:00" offset both resolve to the SAME UTC instant; an unparseable
    /// bound passes through unchanged (the safe, non-panicking degrade).
    #[test]
    fn to_utc_bound_normalizes_offsets_and_passes_garbage_through() {
        // Same wall-clock instant, three ways of writing it.
        let utc = to_utc_bound("2026-06-16T10:00:00+00:00");
        let plus2 = to_utc_bound("2026-06-16T12:00:00+02:00"); // == 10:00Z
        let minus5 = to_utc_bound("2026-06-16T05:00:00-05:00"); // == 10:00Z
        // All three normalize to the SAME canonical UTC instant.
        assert_eq!(plus2, utc, "+02:00 must normalize to the equivalent UTC instant");
        assert_eq!(minus5, utc, "-05:00 must normalize to the equivalent UTC instant");
        // And it is genuinely a UTC ("+00:00") rendering, not the original offset.
        assert!(utc.ends_with("+00:00"), "canonical UTC rendering: {utc}");
        // Unparseable bound: passed through UNCHANGED, never panics.
        assert_eq!(to_utc_bound("not-a-timestamp"), "not-a-timestamp");
        assert_eq!(to_utc_bound(""), "");
    }

    /// A Since/Around bound carrying a non-UTC offset must select episodes by the
    /// TRUE INSTANT, not by lexical offset. We record one episode, capture a UTC
    /// cutoff just after it, then express that SAME instant as a "+02:00" and a
    /// "-05:00" bound — both must narrow exactly as the canonical UTC bound does.
    /// An unparseable bound must not panic (degrades to today's behavior).
    #[tokio::test]
    async fn since_and_around_bounds_normalize_non_utc_offsets() {
        let db = TempDb::new("offset-bounds");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.darwin", "old episode about gardens", "conversation").await;
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        // A single UTC instant strictly between the two seeds.
        let cutoff_utc = chrono::Utc::now();
        tokio::time::sleep(std::time::Duration::from_millis(8)).await;
        seed(&mem, "agent.darwin", "new episode about rockets", "conversation").await;

        // The SAME instant, re-expressed in two non-UTC offsets. A naive lexical
        // compare against the UTC-stored ts would mis-window these; normalization
        // makes them select by true instant.
        let cutoff_plus2 =
            cutoff_utc.with_timezone(&chrono::FixedOffset::east_opt(2 * 3600).unwrap()).to_rfc3339();
        let cutoff_minus5 = cutoff_utc
            .with_timezone(&chrono::FixedOffset::west_opt(5 * 3600).unwrap())
            .to_rfc3339();
        assert!(cutoff_plus2.contains("+02:00"), "sanity: bound carries +02:00: {cutoff_plus2}");
        assert!(cutoff_minus5.contains("-05:00"), "sanity: bound carries -05:00: {cutoff_minus5}");

        // Since(+02:00): only the post-cutoff episode (rockets), exactly as a UTC
        // bound would. Without normalization the lexical compare would be wrong.
        for bound in [&cutoff_plus2, &cutoff_minus5] {
            let out = episodic_recall(
                &mem,
                "agent.darwin",
                "",
                Some(&When::Since(bound.to_string())),
                5,
                &LexicalOnlyEmbedder,
            )
            .await;
            assert_eq!(out.episodes.len(), 1, "since({bound}) -> only the post-cutoff episode");
            assert!(
                out.episodes[0].utterance_redacted.contains("rockets"),
                "since({bound}) must select by true instant: {:?}",
                out.episodes
            );
        }

        // Around [epoch, +02:00 cutoff]: only the pre-cutoff episode (gardens).
        let out = episodic_recall(
            &mem,
            "agent.darwin",
            "",
            Some(&When::Around {
                from: "1970-01-01T00:00:00-05:00".to_string(), // non-UTC low bound too
                to: cutoff_plus2.clone(),
            }),
            5,
            &LexicalOnlyEmbedder,
        )
        .await;
        assert_eq!(out.episodes.len(), 1, "around[.., +02:00] -> only the pre-cutoff episode");
        assert!(out.episodes[0].utterance_redacted.contains("gardens"));

        // An UNPARSEABLE bound must not panic — it degrades safely (pass-through).
        let out = episodic_recall(
            &mem,
            "agent.darwin",
            "",
            Some(&When::Since("garbage-bound".to_string())),
            5,
            &LexicalOnlyEmbedder,
        )
        .await;
        // No assertion on the selection here — only that the call returned without
        // panicking (the safe-degrade contract). It is a normal EpisodicRecall.
        let _ = out.episodes.len();
    }

    // =====================================================================
    // TOPICAL recall — BM25 ranks the right episode
    // =====================================================================

    #[tokio::test]
    async fn topical_recall_ranks_the_relevant_episode_first() {
        let db = TempDb::new("topical");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.darwin", "I drive a blue Subaru Outback", "conversation").await;
        seed(&mem, "agent.darwin", "my corgi is named Watson", "conversation").await;
        seed(&mem, "agent.darwin", "I prefer neovim over vscode", "conversation").await;

        let out =
            episodic_recall(&mem, "agent.darwin", "what did I say about my car subaru", None, 3, &LexicalOnlyEmbedder)
                .await;
        assert_eq!(out.method, RecallMethod::Lexical, "BM25 fallback in tests");
        assert!(!out.episodes.is_empty(), "the car episode must be retrieved");
        assert!(
            out.episodes[0].utterance_redacted.contains("Subaru"),
            "the car episode must rank first: {:?}",
            out.episodes[0]
        );
    }

    #[tokio::test]
    async fn topical_recall_no_match_returns_nothing_never_fabricates() {
        let db = TempDb::new("nomatch");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.darwin", "I drive a blue Subaru", "conversation").await;
        let out = episodic_recall(
            &mem, "agent.darwin", "quantum chromodynamics lecture", None, 5, &LexicalOnlyEmbedder,
        )
        .await;
        assert!(
            out.episodes.is_empty(),
            "a query matching no episode returns NOTHING (no fabrication): {:?}",
            out.episodes
        );
    }

    // =====================================================================
    // AGENT SCOPING — agent A never sees agent B's episodes
    // =====================================================================

    #[tokio::test]
    async fn recall_is_agent_scoped_own_plus_shared_never_cross_agent() {
        let db = TempDb::new("scope");
        let mem = Memory::open(&db.0).unwrap();
        // friday's private episode, jerome's private episode, a shared (darwin) one.
        seed(&mem, "agent.friday", "friday noted the market is up", "conversation").await;
        seed(&mem, "agent.jerome", "jerome queued a song about rain", "conversation").await;
        seed(&mem, "agent.darwin", "shared note about the weather", "conversation").await;

        // friday sees its OWN + the SHARED orchestrator episode, never jerome's.
        let friday = episodic_recall(&mem, "agent.friday", "", None, 10, &LexicalOnlyEmbedder).await;
        let ftexts: Vec<&str> = friday.episodes.iter().map(|e| e.utterance_redacted.as_str()).collect();
        assert!(ftexts.iter().any(|t| t.contains("friday noted")), "own episode missing: {ftexts:?}");
        assert!(ftexts.iter().any(|t| t.contains("shared note")), "shared episode missing: {ftexts:?}");
        assert!(
            !ftexts.iter().any(|t| t.contains("jerome queued")),
            "leaked another agent's episode: {ftexts:?}"
        );

        // jerome likewise: own + shared, never friday's.
        let jerome = episodic_recall(&mem, "agent.jerome", "", None, 10, &LexicalOnlyEmbedder).await;
        let jtexts: Vec<&str> = jerome.episodes.iter().map(|e| e.utterance_redacted.as_str()).collect();
        assert!(jtexts.iter().any(|t| t.contains("jerome queued")));
        assert!(jtexts.iter().any(|t| t.contains("shared note")));
        assert!(!jtexts.iter().any(|t| t.contains("friday noted")), "{jtexts:?}");

        // Topical recall is scoped too: friday querying "song rain" (jerome's
        // content) finds NOTHING — it cannot reach across agents.
        let cross = episodic_recall(&mem, "agent.friday", "song about rain", None, 10, &LexicalOnlyEmbedder).await;
        assert!(
            cross.episodes.iter().all(|e| !e.utterance_redacted.contains("jerome")),
            "topical recall must not cross agents: {:?}",
            cross.episodes
        );
    }

    // =====================================================================
    // RETENTION — evict oldest past the cap; FORGET clears
    // =====================================================================

    #[tokio::test]
    async fn retention_evicts_oldest_episodes_past_the_cap() {
        let db = TempDb::new("retain");
        let mem = Memory::open(&db.0).unwrap();
        for i in 0..5 {
            seed(&mem, "agent.darwin", &format!("episode number {i} about topic"), "conversation").await;
        }
        assert_eq!(mem.episodes_count().await.unwrap(), 5);
        // Keep the newest 2 episodes (events 30d, transcripts 100, episodes 2).
        let (_e, _t, episodes_deleted) = mem.retention_pass(30, 100, 2).await.unwrap();
        assert_eq!(episodes_deleted, 3, "the 3 oldest episodes were evicted");
        let kept = mem.episodes_recent("agent.darwin", 10).await.unwrap();
        assert_eq!(kept.len(), 2);
        // The SURVIVORS are the NEWEST (episodes 3 and 4).
        assert!(kept[0].utterance_redacted.contains("number 4"));
        assert!(kept[1].utterance_redacted.contains("number 3"));
    }

    #[tokio::test]
    async fn forget_clears_an_agents_episodes() {
        let db = TempDb::new("forget");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.friday", "friday episode one", "conversation").await;
        seed(&mem, "agent.friday", "friday episode two", "conversation").await;
        seed(&mem, "agent.jerome", "jerome episode", "conversation").await;

        let cleared = mem.forget_episodes("agent.friday").await.unwrap();
        assert_eq!(cleared, 2, "both of friday's episodes were forgotten");
        // friday's recall is now empty; jerome's is untouched.
        let friday = episodic_recall(&mem, "agent.friday", "", None, 10, &LexicalOnlyEmbedder).await;
        assert!(friday.episodes.is_empty(), "friday's episodes are gone: {:?}", friday.episodes);
        let jerome = episodic_recall(&mem, "agent.jerome", "", None, 10, &LexicalOnlyEmbedder).await;
        assert_eq!(jerome.episodes.len(), 1, "jerome keeps its own episode");
    }

    // =====================================================================
    // FIELD DERIVATION — honest, bounded, redaction-safe
    // =====================================================================

    #[test]
    fn salient_entities_are_bounded_content_words_never_secrets() {
        // Input is the ALREADY-redacted utterance (a secret is the placeholder).
        let ents = derive_salient_entities(
            "remind me about the [redacted] launch deadline and the budget review meeting",
        );
        assert!(ents.len() <= MAX_SALIENT_ENTITIES, "bounded: {ents:?}");
        assert!(!ents.iter().any(|e| e.contains("redacted")), "placeholder excluded: {ents:?}");
        assert!(ents.contains(&"launch".to_string()), "{ents:?}");
        assert!(ents.contains(&"deadline".to_string()), "{ents:?}");
        // Stopwords + short tokens dropped.
        assert!(!ents.contains(&"the".to_string()));
        assert!(!ents.contains(&"me".to_string()));
    }

    #[test]
    fn summary_is_bounded_and_includes_both_sides() {
        let s = derive_summary("what's the weather", "It is sunny and 72 degrees");
        assert!(s.contains("weather"));
        assert!(s.contains("->"));
        assert!(s.contains("sunny"));
        // A very long utterance is clipped (bounded row).
        let long = "word ".repeat(80);
        let s = derive_summary(&long, "ok");
        assert!(s.chars().count() < long.chars().count(), "long utterance is clipped");
    }
}
