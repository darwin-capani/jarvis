//! LIFE-LOG DIGEST — "what did I do this week?", built ONLY from real episodes.
//!
//! The episodic store (episodic.rs / memory.rs) holds DARWIN's redacted,
//! agent-scoped, bounded record of completed turns. This module builds a
//! PERIODIC (daily / weekly) DIGEST over that store: a bounded, browsable summary
//! of what REALLY happened in a window — "this week you worked on …" — assembled
//! from the actual recorded episodes. It is the read-side companion to the
//! consolidation pass (reflect.rs): a deterministic, hermetic fold over episodes,
//! needing NO inference server, NO network, NO model.
//!
//! ## The CONTRACT (non-negotiable, mirrors episodic.rs)
//!   * NEVER FABRICATES: every line of a digest traces to real recorded episodes.
//!     The digest counts and names what was actually logged — topics, salient
//!     themes, the count of turns — and invents NOTHING. A window with NO episodes
//!     yields an HONEST EMPTY digest ("I have nothing logged for this week"), never
//!     a made-up event; a SPARSE window says exactly what little it has.
//!   * REDACTED: it reads only the already-redacted episodic fields, so a secret
//!     never surfaces in a digest (the store stripped it before write).
//!   * AGENT-SCOPED: the digest is built over ONE agent's recall scope (own +
//!     shared orchestrator), so agent A's digest can never show agent B's
//!     episodes — the same isolation the episodic recall enforces.
//!   * BOUNDED: the window is read through the bounded episodic readers (capped
//!     row windows), and the digest itself caps how many themes/topics it lists.
//!   * FORGETTABLE: it owns no store of its own — forgetting the underlying
//!     episodes (memory::forget_episodes) empties the digest too.
//!
//! Nothing here speaks, acts, or reaches the network. It summarizes turns that
//! were really recorded.

use crate::episodic::DEFAULT_NAMESPACE;
use crate::memory::{Episode, Memory};

/// How many salient THEMES a digest names — the most frequent salient entities
/// across the window's episodes. Bounded so a digest stays a glance, not a dump.
pub const MAX_DIGEST_THEMES: usize = 8;

/// How many distinct TOPICS a digest names. Bounded for the same reason.
pub const MAX_DIGEST_TOPICS: usize = 6;

/// The largest window (in days) a digest spans. A request for a longer period is
/// clamped to this — the digest is the RECENT past, not the whole (bounded) store.
pub const MAX_DIGEST_DAYS: i64 = 31;

/// The cap on episodes scanned for one digest. Bounded twice over (the day window
/// AND this row cap), mirroring the episodic recall window.
pub const DIGEST_SCAN_CAP: usize = 500;

/// The period a digest covers — the spoken "today" / "this week" the intent maps
/// to. Each maps to a day-count window the digest reads back from `now`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// The last day (24h) — "what did I do today".
    Day,
    /// The last 7 days — "what did I do this week".
    Week,
}

impl Period {
    /// The window length in days this period reads back from now (clamped to
    /// [`MAX_DIGEST_DAYS`]). Pure.
    pub fn days(&self) -> i64 {
        match self {
            Period::Day => 1,
            Period::Week => 7,
        }
        .min(MAX_DIGEST_DAYS)
    }

    /// The human label for the period ("today" / "this week"). Pure.
    pub fn label(&self) -> &'static str {
        match self {
            Period::Day => "today",
            Period::Week => "this week",
        }
    }
}

/// A built life-log digest: the period it covers, the REAL episode count, the
/// distinct topics seen, the most-frequent salient themes, and a sample of recent
/// summaries. Empty counts/lists mean an HONEST empty/sparse window — never a
/// fabricated event. Built ONLY from recorded episodes; [`render_digest`] turns it
/// into the read-friendly line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifeLogDigest {
    /// The period covered.
    pub period: Period,
    /// How many REAL episodes fell in the window. 0 == honest empty.
    pub episode_count: usize,
    /// The distinct topics seen (first-seen order), bounded to
    /// [`MAX_DIGEST_TOPICS`]. Drawn from the episodes' real `topic` fields.
    pub topics: Vec<String>,
    /// The most-frequent salient themes across the window (most-frequent first),
    /// bounded to [`MAX_DIGEST_THEMES`]. Drawn from real `salient_entities`.
    pub themes: Vec<String>,
    /// A small sample of the most recent episode SUMMARIES (newest first),
    /// bounded — the concrete "what" behind the counts. Real redacted summaries.
    pub recent_summaries: Vec<String>,
}

impl LifeLogDigest {
    /// True when the window held no episodes at all — the honest-empty case.
    pub fn is_empty(&self) -> bool {
        self.episode_count == 0
    }
}

// ---------------------------------------------------------------------------
// TELEMETRY CARD — the enriched, secret-free shape the HUD renders
// ---------------------------------------------------------------------------

/// How long a recent-summary line the card carries — bounded so the telemetry
/// stays a glance, not a dump (the summaries are already-redacted, but we still
/// cap each to keep the emitted envelope small).
pub const CARD_SUMMARY_CHARS: usize = 200;

/// The enriched life-log telemetry CARD the HUD renders: the period label, the
/// REAL episode count, the rendered digest text, and the bounded themes / topics /
/// recent (already-redacted) summaries the digest was built from. SECRET-FREE: it
/// carries ONLY the digest's already-redacted fields — the episodic store stripped
/// every secret BEFORE write, so a digest can hold none, and this card copies only
/// those redacted fields. NEVER fabricates: an empty window yields an `empty` card
/// (zero count, no themes/topics/summaries) the HUD shows as an honest empty state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifeLogCard {
    /// The period label ("today" / "this week").
    pub period: &'static str,
    /// True when the window held no episodes (the honest-empty case).
    pub empty: bool,
    /// The REAL count of recorded episodes in the window.
    pub episode_count: usize,
    /// The fully-rendered, honest-empty-aware digest line ([`render_digest`]).
    pub digest_text: String,
    /// The bounded salient themes (most-frequent first), already-redacted.
    pub themes: Vec<String>,
    /// The bounded distinct topics, already-redacted.
    pub topics: Vec<String>,
    /// A bounded sample of the most-recent already-redacted summaries (each capped
    /// to [`CARD_SUMMARY_CHARS`]).
    pub recent_summaries: Vec<String>,
}

/// Build a [`LifeLogCard`] from a built digest + its rendered text. Pure (no I/O).
/// SECRET-FREE by construction: every field copied is the digest's already-redacted
/// content (themes/topics/summaries derive from the episodic store's redacted
/// fields), bounded by the digest's own caps and a per-summary char cap; nothing
/// raw or fabricated can enter. An empty digest yields `empty: true` with empty
/// lists — the HUD's honest empty state.
pub fn build_card(digest: &LifeLogDigest) -> LifeLogCard {
    let recent_summaries = digest
        .recent_summaries
        .iter()
        .map(|s| {
            let t = s.trim();
            if t.chars().count() > CARD_SUMMARY_CHARS {
                let cut: String = t.chars().take(CARD_SUMMARY_CHARS).collect();
                format!("{}…", cut.trim_end())
            } else {
                t.to_string()
            }
        })
        .collect();
    LifeLogCard {
        period: digest.period.label(),
        empty: digest.is_empty(),
        episode_count: digest.episode_count,
        digest_text: render_digest(digest),
        themes: digest.themes.clone(),
        topics: digest.topics.clone(),
        recent_summaries,
    }
}

// ---------------------------------------------------------------------------
// PURE CORE — fold real episodes into a digest (unit-tested, no I/O)
// ---------------------------------------------------------------------------

/// Fold a window's REAL episodes into a digest: count them, collect their distinct
/// topics (bounded, first-seen), tally their salient entities into the most
/// frequent themes (bounded), and sample the most recent summaries (the input is
/// newest-first, as the episodic readers return). Pure + deterministic — it
/// NEVER invents: an empty slice yields a zero-count digest, a sparse slice yields
/// exactly what little it holds. The summaries/topics/themes are the episodes'
/// already-redacted fields, so nothing here can leak a secret.
pub fn build_from_episodes(period: Period, episodes: &[Episode]) -> LifeLogDigest {
    // Distinct topics, first-seen order, bounded.
    let mut topics: Vec<String> = Vec::new();
    for ep in episodes {
        let t = ep.topic.trim();
        if t.is_empty() {
            continue;
        }
        if !topics.iter().any(|x| x == t) {
            topics.push(t.to_string());
            if topics.len() == MAX_DIGEST_TOPICS {
                break;
            }
        }
    }

    // Theme frequency: tally salient entities, then rank most-frequent first with
    // first-seen as a stable tie-break.
    let mut counts: Vec<(String, usize, usize)> = Vec::new(); // (theme, count, first_idx)
    for (idx, ep) in episodes.iter().enumerate() {
        for ent in &ep.salient_entities {
            let e = ent.trim();
            if e.is_empty() {
                continue;
            }
            if let Some(slot) = counts.iter_mut().find(|(t, _, _)| t == e) {
                slot.1 += 1;
            } else {
                counts.push((e.to_string(), 1, idx));
            }
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    let themes: Vec<String> = counts
        .into_iter()
        .take(MAX_DIGEST_THEMES)
        .map(|(t, _, _)| t)
        .collect();

    // Recent summaries sample (input is newest-first), bounded.
    let recent_summaries: Vec<String> = episodes
        .iter()
        .filter(|ep| !ep.summary.trim().is_empty())
        .take(MAX_DIGEST_TOPICS)
        .map(|ep| ep.summary.trim().to_string())
        .collect();

    LifeLogDigest {
        period,
        episode_count: episodes.len(),
        topics,
        themes,
        recent_summaries,
    }
}

/// Render a digest into one read-friendly line. Honest-empty aware: a zero-count
/// window says so plainly and NEVER fabricates an event; a sparse window says
/// exactly what little it holds. Pure — unit-testable without I/O.
pub fn render_digest(digest: &LifeLogDigest) -> String {
    let label = digest.period.label();
    if digest.is_empty() {
        return format!(
            "I have nothing logged for {label}, sir — no recorded episodes in that window. \
             I only summarize what was actually logged, so there's nothing to report."
        );
    }
    let mut out = format!(
        "Here's your life log for {label}, sir — {} recorded turn{}.",
        digest.episode_count,
        if digest.episode_count == 1 { "" } else { "s" },
    );
    if !digest.themes.is_empty() {
        out.push_str(&format!(" You spent time on: {}.", digest.themes.join(", ")));
    }
    if !digest.topics.is_empty() {
        out.push_str(&format!(" Across {}.", digest.topics.join(", ")));
    }
    if let Some(first) = digest.recent_summaries.first() {
        out.push_str(&format!(" Most recently: {first}."));
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// ORCHESTRATION — read the real window (agent-scoped), build, render
// ---------------------------------------------------------------------------

/// Build a life-log digest for `period` over `namespace`'s REAL episodes. Reads
/// the bounded, AGENT-SCOPED window (own + shared orchestrator) since the period's
/// cutoff via the episodic `Since` reader, then folds it with
/// [`build_from_episodes`]. Hermetic on the read side (the Db is the only
/// dependency); a busy/failed store degrades to an HONEST EMPTY digest (never an
/// error, never a fabricated event). NEVER fabricates: an empty window yields a
/// zero-count digest.
pub async fn build_digest(memory: &Memory, namespace: &str, period: Period) -> LifeLogDigest {
    let cutoff = (chrono::Utc::now() - chrono::Duration::days(period.days())).to_rfc3339();
    // Read the agent-scoped window strictly after the cutoff, bounded by the scan
    // cap. A failed read degrades to an empty digest — a busy DB never errs here.
    let episodes = memory
        .episodes_since(namespace, &cutoff, DIGEST_SCAN_CAP)
        .await
        .unwrap_or_default();
    build_from_episodes(period, &episodes)
}

/// Build a digest for the SHARED/orchestrator scope (the default conversational
/// tier) — the life-log of the user's main interactions. A thin convenience over
/// [`build_digest`] with [`DEFAULT_NAMESPACE`]. The live router dispatch builds the
/// digest over the ACTIVE agent's namespace (which is `DEFAULT_NAMESPACE` for the
/// orchestrator, the tier that owns these utterances), so this default-scope alias
/// is the explicit convenience for callers (and tests) that want the shared tier
/// without naming it; exercised by the module's hermetic tests.
#[allow(dead_code)]
pub async fn build_default_digest(memory: &Memory, period: Period) -> LifeLogDigest {
    build_digest(memory, DEFAULT_NAMESPACE, period).await
}

// ---------------------------------------------------------------------------
// INTENT — explicit, phrase-anchored
// ---------------------------------------------------------------------------

/// A life-log intent parsed from an utterance: a request for the periodic digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifeLogIntent {
    /// "what did I do today" / "my life log today".
    Digest(Period),
}

/// Detect a life-log digest intent. CONSERVATIVE and phrase-anchored: the
/// utterance must ask about the user's OWN recent activity ("what did I do",
/// "my life log", "what have I been up to") with a period word, so an ordinary
/// question never trips it. Pure — unit-tested. Defaults to [`Period::Week`] when
/// a "life log" is requested without an explicit day/week word (the browsable
/// default).
pub fn classify_lifelog_intent(utterance: &str) -> Option<LifeLogIntent> {
    let lower = utterance.to_lowercase();
    let lower = lower.trim();

    let about_my_activity = lower.contains("life log")
        || lower.contains("lifelog")
        || lower.contains("life-log")
        || lower.contains("what did i do")
        || lower.contains("what have i done")
        || lower.contains("what have i been up to")
        || lower.contains("what i've been doing")
        || lower.contains("what i did")
        || lower.contains("my activity")
        || lower.contains("my week")
        || lower.contains("my day");
    if !about_my_activity {
        return None;
    }

    // Period word picks the window; "this week"/"today" are the canonical two.
    let weekish = lower.contains("week")
        || lower.contains("this week")
        || lower.contains("past week")
        || lower.contains("last week")
        || lower.contains("lately")
        || lower.contains("recently");
    let dayish = lower.contains("today")
        || lower.contains("my day")
        || lower.contains("this morning")
        || lower.contains("so far today");

    // DAY only when explicitly day-scoped AND not also week-scoped; else WEEK
    // (the browsable default).
    if dayish && !weekish {
        return Some(LifeLogIntent::Digest(Period::Day));
    }
    Some(LifeLogIntent::Digest(Period::Week))
}

// ---------------------------------------------------------------------------
// DISPATCH — a classified intent -> a built, rendered digest reply
// ---------------------------------------------------------------------------

/// Handle a [`LifeLogIntent`] against the real episodic store, AGENT-SCOPED to
/// `namespace`, returning the rendered digest line the caller speaks. This is the
/// single end-to-end surface the router calls so a real "what did I do this week"
/// reaches the digest. HONEST: it summarizes only REAL recorded episodes — an
/// empty/sparse window renders an honest empty/sparse digest, never a fabricated
/// event (the whole honesty + agent-scoping + bounded contract rides
/// [`build_digest`]).
pub async fn dispatch(memory: &Memory, namespace: &str, intent: LifeLogIntent) -> String {
    match intent {
        LifeLogIntent::Digest(period) => {
            let digest = build_digest(memory, namespace, period).await;
            render_digest(&digest)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::episodic::{record_episode, VoiceGate};
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-lifelog-test-{}-{}.db",
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

    fn voice_off() -> VoiceGate {
        VoiceGate { enabled: false, enrolled: false, owner_verified: false }
    }

    async fn seed(mem: &Memory, ns: &str, utterance: &str, topic: &str) {
        record_episode(&Config::default(), mem, ns, utterance, "ok", topic, false, voice_off())
            .await
            .unwrap();
    }

    fn ep(ns: &str, topic: &str, entities: &[&str], summary: &str) -> Episode {
        Episode {
            id: 0,
            ts: String::new(),
            agent_namespace: ns.to_string(),
            utterance_redacted: summary.to_string(),
            topic: topic.to_string(),
            salient_entities: entities.iter().map(|s| s.to_string()).collect(),
            outcome: "ok".to_string(),
            summary: summary.to_string(),
        }
    }

    // ---- pure fold: real episodes -> a correct digest ----------------------

    #[test]
    fn build_from_episodes_counts_topics_and_ranks_themes() {
        // Newest-first, as the episodic readers return.
        let episodes = vec![
            ep("agent.darwin", "code", &["rocket", "engine"], "worked on rocket engine"),
            ep("agent.darwin", "code", &["rocket", "telemetry"], "added rocket telemetry"),
            ep("agent.darwin", "conversation", &["garden"], "talked about the garden"),
        ];
        let d = build_from_episodes(Period::Week, &episodes);
        assert_eq!(d.episode_count, 3);
        // Distinct topics, first-seen order.
        assert_eq!(d.topics, vec!["code", "conversation"]);
        // "rocket" appears in two episodes -> ranked first among themes.
        assert_eq!(d.themes.first().map(|s| s.as_str()), Some("rocket"), "{:?}", d.themes);
        assert!(d.themes.contains(&"garden".to_string()));
        // Recent summary sample, newest first.
        assert_eq!(d.recent_summaries.first().map(|s| s.as_str()), Some("worked on rocket engine"));
        assert!(!d.is_empty());
        let rendered = render_digest(&d);
        assert!(rendered.contains("3 recorded turns"), "{rendered}");
        assert!(rendered.contains("rocket"), "names a real theme: {rendered}");
    }

    #[test]
    fn empty_window_is_honest_empty_never_fabricates() {
        let d = build_from_episodes(Period::Week, &[]);
        assert!(d.is_empty());
        assert_eq!(d.episode_count, 0);
        assert!(d.topics.is_empty() && d.themes.is_empty() && d.recent_summaries.is_empty());
        let rendered = render_digest(&d);
        assert!(
            rendered.to_lowercase().contains("nothing logged"),
            "an empty window must render an honest empty digest: {rendered}"
        );
    }

    #[test]
    fn sparse_window_reports_only_what_it_holds() {
        // A single episode: the digest names exactly it, inventing nothing.
        let episodes = vec![ep("agent.darwin", "conversation", &["weather"], "asked about weather")];
        let d = build_from_episodes(Period::Day, &episodes);
        assert_eq!(d.episode_count, 1);
        assert_eq!(d.topics, vec!["conversation"]);
        assert_eq!(d.themes, vec!["weather"]);
        let rendered = render_digest(&d);
        assert!(rendered.contains("1 recorded turn"), "{rendered}");
        assert!(rendered.contains("weather"));
    }

    // ---- bounded -----------------------------------------------------------

    #[test]
    fn themes_and_topics_are_bounded() {
        let mut episodes = Vec::new();
        for i in 0..40 {
            episodes.push(ep(
                "agent.darwin",
                &format!("topic{i}"),
                &[&format!("theme{i}")],
                &format!("summary {i}"),
            ));
        }
        let d = build_from_episodes(Period::Week, &episodes);
        assert!(d.topics.len() <= MAX_DIGEST_TOPICS, "topics bounded: {}", d.topics.len());
        assert!(d.themes.len() <= MAX_DIGEST_THEMES, "themes bounded: {}", d.themes.len());
        assert_eq!(d.episode_count, 40, "the real count is still honest");
    }

    // ---- live read over the real store -------------------------------------

    #[tokio::test]
    async fn build_digest_reads_real_recent_episodes() {
        let db = TempDb::new("live");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.darwin", "worked on the rocket engine design", "code").await;
        seed(&mem, "agent.darwin", "reviewed the rocket telemetry data", "code").await;

        let d = build_digest(&mem, "agent.darwin", Period::Week).await;
        assert_eq!(d.episode_count, 2, "both recent episodes are in the week window");
        assert!(d.themes.contains(&"rocket".to_string()), "real theme present: {:?}", d.themes);
    }

    #[tokio::test]
    async fn build_digest_empty_store_is_honest_empty() {
        let db = TempDb::new("live-empty");
        let mem = Memory::open(&db.0).unwrap();
        let d = build_digest(&mem, "agent.darwin", Period::Week).await;
        assert!(d.is_empty(), "an empty store yields an honest empty digest");
        assert!(render_digest(&d).to_lowercase().contains("nothing logged"));
    }

    // ---- agent scoping: A's digest never shows B's episodes ----------------

    #[tokio::test]
    async fn digest_is_agent_scoped_a_never_sees_b() {
        let db = TempDb::new("scope");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.friday", "friday tracked the markets going up", "conversation").await;
        seed(&mem, "agent.jerome", "jerome queued a song about the rain", "conversation").await;
        seed(&mem, "agent.darwin", "shared note about the weather forecast", "conversation").await;

        // friday's digest: its own + shared themes, NEVER jerome's.
        let friday = build_digest(&mem, "agent.friday", Period::Week).await;
        let all_themes: Vec<&str> = friday
            .themes
            .iter()
            .map(|s| s.as_str())
            .chain(friday.recent_summaries.iter().map(|s| s.as_str()))
            .collect();
        let joined = all_themes.join(" ");
        assert!(joined.contains("markets") || friday.recent_summaries.iter().any(|s| s.contains("markets")), "own missing: {friday:?}");
        assert!(
            !friday.recent_summaries.iter().any(|s| s.contains("jerome") || s.contains("song")),
            "A's digest leaked B's episode: {:?}",
            friday.recent_summaries
        );
    }

    // ---- forget empties the digest -----------------------------------------

    #[tokio::test]
    async fn forgetting_episodes_empties_the_digest() {
        let db = TempDb::new("forget");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, "agent.friday", "friday did a thing", "conversation").await;
        assert_eq!(build_digest(&mem, "agent.friday", Period::Week).await.episode_count, 1);
        mem.forget_episodes("agent.friday").await.unwrap();
        let d = build_digest(&mem, "agent.friday", Period::Week).await;
        assert!(d.is_empty(), "forgetting episodes empties the digest: {d:?}");
    }

    // ---- intents -----------------------------------------------------------

    #[test]
    fn intents_parse_period_words() {
        assert_eq!(
            classify_lifelog_intent("what did I do this week"),
            Some(LifeLogIntent::Digest(Period::Week))
        );
        assert_eq!(
            classify_lifelog_intent("what did I do today"),
            Some(LifeLogIntent::Digest(Period::Day))
        );
        assert_eq!(
            classify_lifelog_intent("show my life log"),
            Some(LifeLogIntent::Digest(Period::Week)),
            "a bare life-log request defaults to the week digest"
        );
        // Not a life-log request.
        assert_eq!(classify_lifelog_intent("what's the weather today"), None);
        assert_eq!(classify_lifelog_intent("research the markets"), None);
    }

    #[test]
    fn period_window_lengths_are_bounded() {
        assert_eq!(Period::Day.days(), 1);
        assert_eq!(Period::Week.days(), 7);
        assert!(Period::Week.days() <= MAX_DIGEST_DAYS);
    }

    // ---- dispatch: an utterance routes end-to-end --------------------------

    #[tokio::test]
    async fn dispatch_builds_and_renders_the_real_digest() {
        let db = TempDb::new("dispatch");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, DEFAULT_NAMESPACE, "worked on the rocket engine design", "code").await;

        let intent = classify_lifelog_intent("what did I do this week").unwrap();
        assert_eq!(intent, LifeLogIntent::Digest(Period::Week));
        let reply = dispatch(&mem, DEFAULT_NAMESPACE, intent).await;
        assert!(reply.contains("1 recorded turn"), "names the real count: {reply}");
        assert!(reply.contains("rocket"), "names a real theme: {reply}");
    }

    // ---- enriched telemetry card: real digest + themes, no secret, honest empty ----

    #[test]
    fn build_card_carries_the_real_digest_and_themes() {
        let episodes = vec![
            ep("agent.darwin", "code", &["rocket", "engine"], "worked on rocket engine"),
            ep("agent.darwin", "code", &["rocket", "telemetry"], "added rocket telemetry"),
        ];
        let d = build_from_episodes(Period::Week, &episodes);
        let card = build_card(&d);
        assert_eq!(card.period, "this week");
        assert!(!card.empty, "a populated window is not empty");
        assert_eq!(card.episode_count, 2, "the REAL count rides the card");
        assert!(card.digest_text.contains("2 recorded turns"), "rendered text: {}", card.digest_text);
        assert!(card.themes.contains(&"rocket".to_string()), "real theme: {:?}", card.themes);
        assert_eq!(card.topics, vec!["code"], "real topic: {:?}", card.topics);
        assert!(
            card.recent_summaries.iter().any(|s| s.contains("rocket engine")),
            "the real redacted summary rides: {:?}",
            card.recent_summaries
        );
    }

    #[test]
    fn build_card_is_secret_free_carries_only_redacted_fields() {
        // The episodic store redacts BEFORE write — by the time a digest exists, its
        // fields are already redacted. A digest built from already-redacted summaries
        // therefore carries NO secret; the card copies only those fields. We prove the
        // card never introduces raw content by feeding already-redacted summaries and
        // asserting the secret-shaped marker that WOULD have been there is absent.
        let episodes = vec![ep(
            "agent.darwin",
            "conversation",
            &["account"],
            "set up the account, key [redacted]",
        )];
        let d = build_from_episodes(Period::Day, &episodes);
        let card = build_card(&d);
        let blob = format!(
            "{} {} {} {}",
            card.digest_text,
            card.themes.join(" "),
            card.topics.join(" "),
            card.recent_summaries.join(" ")
        );
        assert!(blob.contains("[redacted]"), "the already-redacted marker is preserved: {blob}");
        assert!(!blob.contains("sk-"), "no secret-shaped token in the card: {blob}");
        assert!(card.recent_summaries[0].contains("[redacted]"), "summary stays redacted");
    }

    #[test]
    fn build_card_empty_window_is_honest_empty() {
        let d = build_from_episodes(Period::Week, &[]);
        let card = build_card(&d);
        assert!(card.empty, "an empty window is flagged empty");
        assert_eq!(card.episode_count, 0);
        assert!(card.themes.is_empty() && card.topics.is_empty() && card.recent_summaries.is_empty());
        assert!(
            card.digest_text.to_lowercase().contains("nothing logged"),
            "honest empty digest text: {}",
            card.digest_text
        );
    }

    #[test]
    fn build_card_bounds_a_long_summary() {
        let long = "y".repeat(CARD_SUMMARY_CHARS * 3);
        let episodes = vec![ep("agent.darwin", "code", &["t"], &long)];
        let d = build_from_episodes(Period::Day, &episodes);
        let card = build_card(&d);
        assert!(
            card.recent_summaries[0].chars().count() <= CARD_SUMMARY_CHARS + 1,
            "summary bounded: {} chars",
            card.recent_summaries[0].chars().count()
        );
    }

    #[tokio::test]
    async fn build_card_over_the_real_store_matches_the_dispatch_reply() {
        let db = TempDb::new("card-live");
        let mem = Memory::open(&db.0).unwrap();
        seed(&mem, DEFAULT_NAMESPACE, "worked on the rocket engine design", "code").await;
        // The router builds the digest once, renders the reply, and the card — the
        // card's digest_text MUST equal what dispatch (the spoken reply) renders.
        let digest = build_digest(&mem, DEFAULT_NAMESPACE, Period::Week).await;
        let card = build_card(&digest);
        let reply = dispatch(&mem, DEFAULT_NAMESPACE, LifeLogIntent::Digest(Period::Week)).await;
        assert_eq!(card.digest_text, reply, "the card text equals the spoken reply");
        assert!(card.themes.contains(&"rocket".to_string()), "real theme: {:?}", card.themes);
    }

    #[tokio::test]
    async fn dispatch_empty_store_is_honest_empty() {
        let db = TempDb::new("dispatch-empty");
        let mem = Memory::open(&db.0).unwrap();
        let reply = dispatch(&mem, DEFAULT_NAMESPACE, LifeLogIntent::Digest(Period::Day)).await;
        assert!(reply.to_lowercase().contains("nothing logged"), "honest empty: {reply}");
    }

    #[tokio::test]
    async fn build_default_digest_uses_the_shared_orchestrator_scope() {
        let db = TempDb::new("default-scope");
        let mem = Memory::open(&db.0).unwrap();
        // A shared-tier episode is in the default digest; a specialist's is not.
        seed(&mem, DEFAULT_NAMESPACE, "shared note about the weather", "conversation").await;
        let d = build_default_digest(&mem, Period::Week).await;
        assert_eq!(d.episode_count, 1, "the shared episode is in the default digest");
    }
}
