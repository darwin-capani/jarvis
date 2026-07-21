//! screen_context.rs — CONTINUOUS SCREEN CONTEXT (#42): the PURE, testable core.
//!
//! This is the MOST privacy-sensitive READ feature in the system, so the design
//! is privacy-first and the core is a PURE value type with no I/O:
//!
//!   * [`ScreenContextRing`] — a BOUNDED, evict-oldest ring of recent on-screen
//!     OCR snapshots. Every pushed text is REDACTED (the optimizer/audit redactor,
//!     so an on-screen secret never survives into the ring) BEFORE it enters. The
//!     ring is in-RAM + TRANSIENT: it is NEVER written to lifelong memory, the
//!     optimizer traces, or disk by default — it lives only as long as the process
//!     and is wiped by `clear` ("forget my screen context").
//!   * [`classify_screen_context_intent`] — a CONSERVATIVE pure classifier mapping
//!     a spoken utterance to RECALL ("what was I working on" / "recall my screen
//!     context") or FORGET ("forget my screen context") — or None so an ordinary
//!     sentence never triggers. Both intents are READ-ONLY: recall describes the
//!     bounded redacted recent context (an empty ring => an HONEST "no recent
//!     screen context", never fabricated); forget wipes the ring.
//!
//! The CONTINUOUS capture loop that FEEDS the ring is DEVICE-gated (it grabs a
//! ScreenCaptureKit frame, which requires runtime macOS Screen-Recording consent —
//! TCC, NOT SBPL-grantable) and is wired behind [screen_context].enabled in the
//! Vision app (Pipeline.continuousScreenContextLoop) + the daemon push path; that
//! live loop is NOT exercised here (no real capture in a test). The ring + recall
//! + forget are proven PURELY over SYNTHETIC snapshots.
//!
//! HONESTY: recall NEVER fabricates context (an empty ring is honest); the context
//! is bounded + redacted + transient + forgettable + glyph-only; the WATCHING
//! indicator (a `screen_context.watching` telemetry envelope) is honest about
//! whether the loop is active.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::optimize::redact;
use crate::recall::{self, Embedder, Fact, LexicalProvider};

// ===========================================================================
// ContextEntry — one redacted on-screen OCR snapshot in the ring.
// ===========================================================================

/// One recent on-screen OCR snapshot. The `redacted_text` is ALWAYS the output of
/// the optimizer redactor (a secret never enters the ring); the entry carries NO
/// face/person id/embedding — glyph text only. `ts` is the capture time (unix
/// seconds, monotonic-ish for ordering) and `source_tag` is the capture source
/// ("screen") so a recall can attribute the snapshot honestly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry {
    pub ts: u64,
    pub redacted_text: String,
    pub source_tag: String,
}

// ===========================================================================
// ScreenContextRing — the PURE bounded/redacted/transient ring.
// ===========================================================================

/// A BOUNDED, evict-oldest ring of recent redacted OCR snapshots. PURE + in-RAM +
/// TRANSIENT: pushing redacts-then-stores, the ring never exceeds `cap` (the
/// oldest entry is evicted past it), recall returns the bounded recent context,
/// and clear wipes it. Nothing here touches disk, lifelong memory, or the
/// optimizer traces — the ring lives only in RAM for the process lifetime.
#[derive(Debug, Clone)]
pub struct ScreenContextRing {
    cap: usize,
    entries: VecDeque<ContextEntry>,
}

impl ScreenContextRing {
    /// A ring bounded at `cap` (floored to >= 1 — a 0 cap would make it useless).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            entries: VecDeque::new(),
        }
    }

    /// The hard bound — the ring never holds more than this many entries.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// How many entries are currently held (<= `cap`).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the ring is empty (no recent context).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Push ONE recognized-text snapshot. The text is REDACTED here (the optimizer
    /// redactor) BEFORE it is stored, so an on-screen secret never enters the ring.
    /// Past `cap` the OLDEST entry is evicted (BOUNDED — no unbounded growth). The
    /// stored entry is TRANSIENT: it lives only in this in-RAM ring (never written
    /// to lifelong memory / optimizer / disk). An empty/whitespace-only snapshot is
    /// dropped (nothing read => nothing stored; never a fabricated entry).
    pub fn push(&mut self, ts: u64, raw_text: &str, source_tag: &str) {
        let redacted_text = redact(raw_text);
        if redacted_text.trim().is_empty() {
            return;
        }
        self.entries.push_back(ContextEntry {
            ts,
            redacted_text,
            source_tag: source_tag.to_string(),
        });
        // Evict from the FRONT (oldest) until within the hard cap.
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    /// Recall up to the most-recent `n` entries (newest LAST, reading order). When
    /// `n` exceeds what's held, all held entries are returned. PURE + bounded; an
    /// empty ring yields an empty slice (the caller renders the honest "no recent
    /// screen context" — recall NEVER fabricates an entry). READ-ONLY.
    pub fn recall_recent(&self, n: usize) -> Vec<ContextEntry> {
        let take = n.min(self.entries.len());
        self.entries
            .iter()
            .skip(self.entries.len() - take)
            .cloned()
            .collect()
    }

    /// Recall the entries most relevant to `query`, RANKED by lexical (BM25)
    /// relevance — MOST-RELEVANT FIRST, bounded to `n`. Reuses recall.rs's BM25
    /// ranker (the same one the semantic pasteboard + aperture rings use), so a
    /// natural multi-word query ("the terminal cargo build error") now surfaces the
    /// entries that share its TERMS instead of requiring the exact phrase to appear
    /// as a verbatim substring. An entry with ZERO query-term overlap scores 0 and
    /// is DROPPED — recall never invents a match, so a no-match query returns an
    /// empty vec. An empty query falls back to the plain recent recall. PURE +
    /// READ-ONLY.
    pub fn recall_matching(&self, query: &str, n: usize) -> Vec<ContextEntry> {
        if query.trim().is_empty() {
            return self.recall_recent(n);
        }
        let entries: Vec<ContextEntry> = self.entries.iter().cloned().collect();
        rank_entries_lexical(query, &entries, n)
    }

    /// Render a bounded, redacted, HONEST recall string from the most-recent `n`
    /// entries — the text the recall intent speaks. An EMPTY ring is an honest "I
    /// have no recent screen context" (never a fabricated context). READ-ONLY.
    pub fn render_recall(&self, n: usize) -> String {
        let recent = self.recall_recent(n);
        if recent.is_empty() {
            return "I have no recent screen context, sir — \
                    nothing's been captured."
                .to_string();
        }
        let mut out = String::from("Here's your recent screen context, sir:");
        for e in &recent {
            out.push('\n');
            out.push_str("• ");
            out.push_str(e.redacted_text.trim());
        }
        out
    }

    /// FORGET: wipe the ring ("forget my screen context"). After this the ring is
    /// empty and a recall is the honest "no recent screen context".
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ===========================================================================
// Ranked recall — bring screen context to parity with the semantic pasteboard
// + aperture rings (which already rank a transient in-RAM ring by relevance
// rather than plain substring/recency). PURE; reuses recall.rs's ranker.
// ===========================================================================

/// Build one ranking [`Fact`] per ring entry, carrying the entry's already-redacted
/// text as the value (the key is a low-signal constant so it never skews the
/// ranker). PARALLEL to `entries` — a hit's `index` maps straight back to its
/// entry. Mirrors the semantic pasteboard's `build_recall_facts`.
fn build_recall_facts(entries: &[ContextEntry]) -> Vec<Fact> {
    entries
        .iter()
        .map(|e| Fact {
            key: "screen".to_string(),
            value: e.redacted_text.clone(),
        })
        .collect()
}

/// PURE lexical (BM25) ranking of `entries` by relevance to `query`, returning up
/// to `k` entries MOST-RELEVANT FIRST. Reuses recall.rs's `LexicalProvider` +
/// `rank` over the parallel facts, mapping each hit's index back to its entry. An
/// entry with ZERO query-term overlap scores 0 and is DROPPED by `rank` (honest
/// no-match — recall never invents a hit); a no-match query returns an empty vec.
fn rank_entries_lexical(query: &str, entries: &[ContextEntry], k: usize) -> Vec<ContextEntry> {
    let facts = build_recall_facts(entries);
    recall::rank(query, &facts, k, &LexicalProvider::default())
        .into_iter()
        .map(|h| entries[h.index].clone())
        .collect()
}

// ===========================================================================
// Process-global ring — the in-RAM home of the recent screen context.
//
// A poison-tolerant `Mutex` global (mirrors the prosody/lockdown process-global
// precedent). It is the ONLY place the recent screen context lives: in-RAM,
// TRANSIENT, never persisted. The continuous loop's daemon push path writes here
// (behind [screen_context].enabled + the TCC-gated capture); the recall intent
// reads here; the forget intent clears it.
// ===========================================================================

static RING: Mutex<Option<ScreenContextRing>> = Mutex::new(None);

/// The process-global continuous-loop SETTINGS, set once at daemon startup from
/// `[screen_context]`. A poison-tolerant `Mutex` (mirrors the prosody/lockdown
/// process-global precedent). DEFAULTS to OFF (enabled=false) so that — until the
/// daemon explicitly installs the configured settings — the continuous push path
/// is INERT (no ring growth), exactly like the config default. This is the single
/// gate the relay-side `ingest_continuous_snapshot` reads.
static SETTINGS: Mutex<LoopSettings> = Mutex::new(LoopSettings::off());

/// The continuous-loop settings the daemon honours: whether the loop is enabled
/// and the hard ring cap. (The interval governs the SWIFT-side loop cadence; the
/// daemon push path only needs the enable gate + the cap.)
#[derive(Debug, Clone, Copy)]
struct LoopSettings {
    enabled: bool,
    cap: usize,
}

impl LoopSettings {
    const fn off() -> Self {
        Self { enabled: false, cap: 1 }
    }
}

/// Install the continuous-loop settings at daemon startup (from `[screen_context]`).
/// Until this is called the global is OFF (no continuous push ever fires). With
/// `enabled=false` the continuous push path stays inert regardless of any frame —
/// the OFF-default guarantee.
pub fn install_settings(enabled: bool, cap: usize) {
    let mut guard = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = LoopSettings {
        enabled,
        cap: cap.max(1),
    };
}

/// Whether the continuous capture loop is enabled (the WATCHING gate). False by
/// default (OFF) and until `install_settings` is called with `enabled=true`.
pub fn is_enabled() -> bool {
    SETTINGS.lock().unwrap_or_else(|e| e.into_inner()).enabled
}

/// Ingest ONE continuous-loop OCR snapshot into the ring — the DAEMON push path
/// for the device-gated continuous capture loop. GATED on `[screen_context]`
/// .enabled: when OFF this is a NO-OP (the ring never grows on its own — the
/// OFF-default guarantee), and it emits NOTHING. When ON it redacts-then-pushes
/// the recognized text (bounded by the configured cap) and returns whether the
/// snapshot was actually ingested (so the relay can emit the honest WATCHING
/// indicator only for an active loop). The capture itself (the ScreenCaptureKit
/// frame + OCR) happens DEVICE-side (TCC-gated) in the Vision app; this is the
/// daemon-side bounded/redacted/transient store of the result. Returns false when
/// the loop is OFF or the snapshot was empty after redaction.
pub fn ingest_continuous_snapshot(ts: u64, raw_text: &str, source_tag: &str) -> bool {
    let settings = *SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    if !settings.enabled {
        // Disabled — never grow the ring. (Ships ON by default but is inert without
        // Screen-Recording TCC consent; this guards the explicit-disable case.)
        return false;
    }
    let before = global_len();
    global_push(settings.cap, ts, raw_text, source_tag);
    let after = global_len();
    // Ingested if a new entry landed OR the ring was already at its hard cap (an
    // eviction keeps len stable but still ingested a redacted snapshot).
    after > before || (after == settings.cap.max(1) && !redact(raw_text).trim().is_empty())
}

#[cfg(test)]
pub fn install_settings_for_test(enabled: bool, cap: usize) {
    install_settings(enabled, cap);
}

/// Lazily initialize (if needed) the process-global ring at `cap` and run `f`
/// against it. Poison-tolerant: a poisoned lock is recovered (the ring is in-RAM
/// transient context, not a security gate — losing/recovering it is safe). The
/// `cap` is applied on first init; a later differing cap does NOT shrink an
/// existing ring mid-run (the loop start sets the cap from config).
fn with_global<R>(cap: usize, f: impl FnOnce(&mut ScreenContextRing) -> R) -> R {
    let mut guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    let ring = guard.get_or_insert_with(|| ScreenContextRing::new(cap));
    f(ring)
}

/// Push a captured-and-OCR'd snapshot into the process-global ring (redacted +
/// bounded inside `push`). Called by the daemon's continuous-loop push path ONLY
/// when [screen_context].enabled is on AND a TCC-gated frame produced text.
pub fn global_push(cap: usize, ts: u64, raw_text: &str, source_tag: &str) {
    with_global(cap, |r| r.push(ts, raw_text, source_tag));
}

/// Render the bounded, redacted, honest recall string from the process-global
/// ring (the recall intent). READ-ONLY. An un-fed ring => the honest "no recent
/// screen context".
pub fn global_render_recall(n: usize) -> String {
    let guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(ring) => ring.render_recall(n),
        None => ScreenContextRing::new(1).render_recall(n),
    }
}

/// Render a SUBJECT-aware recall from the process-global ring: the `n` entries most
/// RELEVANT to `query`, ranked by lexical (BM25) relevance most-relevant first (via
/// [`ScreenContextRing::recall_matching`]). When `query` is empty this is the plain
/// recent recall. An empty result is the honest "no recent screen context" — recall
/// NEVER invents a match (a zero-overlap query yields nothing). READ-ONLY.
pub fn global_render_recall_matching(query: &str, n: usize) -> String {
    let guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    let hits = match guard.as_ref() {
        Some(ring) => ring.recall_matching(query, n),
        None => Vec::new(),
    };
    if hits.is_empty() {
        if query.trim().is_empty() {
            return "I have no recent screen context, sir — nothing's been captured."
                .to_string();
        }
        return format!(
            "I have no recent screen context about \"{}\", sir.",
            query.trim()
        );
    }
    let mut out = if query.trim().is_empty() {
        String::from("Here's your recent screen context, sir:")
    } else {
        format!("Here's what I have on \"{}\", sir:", query.trim())
    };
    for e in &hits {
        out.push('\n');
        out.push_str("• ");
        out.push_str(e.redacted_text.trim());
    }
    out
}

/// Render a RUNTIME-SELECTED recall over the live ring — the `screen_recall` TOOL
/// surface. Prefers NEURAL on-device embeddings (via the injected `embedder`, the
/// same on-device inference socket the other recall tools use) and FALLS BACK to
/// lexical BM25 when it is unavailable, NAMING whichever method actually ran (so a
/// caller never claims neural on a fallback). It NEVER fabricates: an empty/un-fed
/// ring or a no-match query is reported honestly. READ-ONLY — it ranks the
/// transient in-RAM ring and mutates nothing.
///
/// The ring is snapshotted under the lock and the guard is DROPPED before the async
/// embed call — the `std::sync::Mutex` is never held across an `.await`.
pub async fn global_rank_render_runtime(query: &str, k: usize, embedder: &dyn Embedder) -> String {
    let entries: Vec<ContextEntry> = {
        let guard = RING.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(ring) => ring.recall_recent(ring.len()),
            None => Vec::new(),
        }
    };
    if entries.is_empty() {
        return "I have no recent screen context yet, sir — nothing's been captured \
                since screen context was enabled."
            .to_string();
    }
    let facts = build_recall_facts(&entries);
    let recall = recall::rank_runtime_selected(query, &facts, k, embedder).await;
    let method = recall.method_status;
    if recall.hits.is_empty() {
        return format!(
            "I have no recent screen context that bears on \"{}\", sir. \
             (Recall method: {method})",
            query.trim()
        );
    }
    let lines: Vec<String> = recall
        .hits
        .iter()
        .map(|h| format!("• {}", entries[h.index].redacted_text.trim()))
        .collect();
    format!(
        "Here's what I have on your recent screen that bears on that, most relevant \
         first:\n{}\n(Recall method: {method})",
        lines.join("\n")
    )
}

/// The current number of entries held in the process-global ring (0 when un-fed) —
/// a bounded, secret-free count for the HUD/telemetry.
pub fn global_len() -> usize {
    let guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(ScreenContextRing::len).unwrap_or(0)
}

/// The hard CAP of the process-global ring (the configured bound, or 0 when the
/// ring has not been initialized yet) — a secret-free bound for the HUD/telemetry,
/// so a consumer can show "held N / cap M" honestly.
pub fn global_cap() -> usize {
    let guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(ScreenContextRing::cap).unwrap_or(0)
}

/// FORGET: wipe the process-global ring ("forget my screen context"). Returns
/// whether anything was actually cleared (so the ack is honest — "nothing to
/// forget" on an empty/un-fed ring).
pub fn global_clear() -> bool {
    let mut guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_mut() {
        Some(ring) if !ring.is_empty() => {
            ring.clear();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
pub fn global_reset_for_test() {
    let mut guard = RING.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

// ===========================================================================
// Intent classification — RECALL / FORGET (PURE, conservative).
// ===========================================================================

/// A spoken screen-context intent. Both are READ-ONLY: RECALL describes the
/// bounded redacted recent context (never fabricated; an empty ring is honest);
/// FORGET wipes the ring. Neither actuates anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenContextIntent {
    /// "what was I working on" / "recall my screen context" / "what was on my
    /// screen earlier" -> render the bounded recent context (read-only). The
    /// optional `subject` narrows the recall to entries mentioning it ("what was I
    /// working on about the budget" -> subject "budget"); None => the recent
    /// context.
    Recall { subject: Option<String> },
    /// "forget my screen context" / "wipe my screen context" -> clear the ring.
    Forget,
}

/// Extract an optional recall SUBJECT from a recall utterance ("...about the
/// budget" / "...on the report" / "...regarding X"). Returns the trimmed,
/// bounded phrase or None. PURE; used to narrow a recall to matching entries.
fn extract_recall_subject(lower: &str) -> Option<String> {
    for lead in ["about the ", "about ", "regarding the ", "regarding ", "on the ", " on "] {
        if let Some(idx) = lower.find(lead) {
            let tail = lower[idx + lead.len()..].trim();
            // Drop a trailing question mark / punctuation.
            let phrase = tail.trim_end_matches(|c: char| !c.is_alphanumeric()).trim();
            if !phrase.is_empty() && phrase.len() <= 64 {
                return Some(phrase.to_string());
            }
        }
    }
    None
}

/// Map a spoken utterance to a screen-context intent, or None when it is not one
/// (the turn falls through to normal routing). CONSERVATIVE: it requires the
/// explicit "screen context" phrase (or the narrow "what was I working on" recall
/// cue) so an ordinary sentence that merely mentions "screen" never triggers — in
/// particular it must NOT collide with the one-shot OCR `read.screen` routing
/// ("read my screen" / "what's on my screen") which is handled by the Vision
/// router. PURE + unit-tested.
pub fn classify_screen_context_intent(utterance: &str) -> Option<ScreenContextIntent> {
    let lower = utterance.to_lowercase();

    // FORGET takes precedence so a "forget my screen context" is never read as a
    // recall. Requires the explicit "screen context" phrase + a forget/wipe/clear
    // verb so an ordinary sentence never wipes the ring.
    let mentions_screen_context = lower.contains("screen context");
    let is_forget = lower.contains("forget")
        || lower.contains("wipe")
        || lower.contains("clear")
        || lower.contains("delete");
    if mentions_screen_context && is_forget {
        return Some(ScreenContextIntent::Forget);
    }

    // RECALL: the explicit "screen context" phrase with a recall/show cue, OR the
    // narrow "what was I working on" / "what was I doing" recall cue. Guarded so a
    // one-shot "read my screen" (handled by the Vision OCR router) never reaches
    // here: this recall is about RECENT context over time, not a fresh read.
    let is_recall_verb = lower.contains("recall")
        || lower.contains("remind")
        || lower.contains("show")
        || lower.contains("what was")
        || lower.contains("what were");
    if mentions_screen_context && is_recall_verb {
        return Some(ScreenContextIntent::Recall {
            subject: extract_recall_subject(&lower),
        });
    }
    // The "what was I working on / doing" recall cue (no "screen context" phrase
    // needed — this is the natural way to ask). Anchored on "working on"/"doing"
    // with a past-tense "was/were I" so a present "what am I working on" (a
    // different, live question) does not capture.
    let working_recall = (lower.contains("what was i") || lower.contains("what were i"))
        && (lower.contains("working on")
            || lower.contains("doing")
            || lower.contains("looking at"));
    if working_recall {
        return Some(ScreenContextIntent::Recall {
            subject: extract_recall_subject(&lower),
        });
    }
    None
}

/// Whether an utterance is a screen-context RECALL (so the pipeline can keep the
/// recalled text TRANSIENT — off lifelong memory / optimizer traces — exactly like
/// the one-shot screen-read). PUBLIC so main.rs can union it into the transient
/// gate. Pure over `classify_screen_context_intent`, so this and the routing agree
/// by construction. (FORGET is a control verb with no recalled content, so it need
/// not be flagged transient, but recall surfaces the bounded redacted context.)
pub fn is_screen_context_recall(utterance: &str) -> bool {
    matches!(
        classify_screen_context_intent(utterance),
        Some(ScreenContextIntent::Recall { .. })
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // The process-global RING + SETTINGS are shared in-RAM state; tests that touch
    // them must not race under cargo's parallel runner. A dedicated serial mutex
    // (poison-tolerant) lets those few tests run one-at-a-time. The PURE ring tests
    // (their own ScreenContextRing) and the pure classifier tests need no guard.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- PURE ring: push/evict/cap -----------------------------------------

    #[test]
    fn push_appends_in_order_and_recall_is_newest_last() {
        let mut ring = ScreenContextRing::new(10);
        ring.push(1, "alpha", "screen");
        ring.push(2, "beta", "screen");
        ring.push(3, "gamma", "screen");
        let recent = ring.recall_recent(10);
        let texts: Vec<&str> = recent.iter().map(|e| e.redacted_text.as_str()).collect();
        assert_eq!(texts, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn cap_is_a_hard_bound_and_oldest_is_evicted() {
        let mut ring = ScreenContextRing::new(3);
        for i in 0..10u64 {
            ring.push(i, &format!("snapshot {i}"), "screen");
        }
        // Never exceeds the cap (BOUNDED — no unbounded accumulation).
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.cap(), 3);
        // Only the 3 NEWEST survive (oldest evicted).
        let recent = ring.recall_recent(100);
        let texts: Vec<&str> = recent.iter().map(|e| e.redacted_text.as_str()).collect();
        assert_eq!(texts, vec!["snapshot 7", "snapshot 8", "snapshot 9"]);
    }

    #[test]
    fn zero_cap_is_floored_to_one() {
        let mut ring = ScreenContextRing::new(0);
        assert_eq!(ring.cap(), 1);
        ring.push(1, "a", "screen");
        ring.push(2, "b", "screen");
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.recall_recent(10)[0].redacted_text, "b");
    }

    // -- REDACTION: a secret never survives into the ring ------------------

    #[test]
    fn a_secret_never_enters_the_ring() {
        let mut ring = ScreenContextRing::new(10);
        // A line carrying an API key + an email + a long digit run — exactly the
        // kind of on-screen secret a screen OCR could surface.
        ring.push(
            1,
            "login token sk-LIVE-abc123def456ghi789 for alice@example.com card 4111111111111111",
            "screen",
        );
        let stored = &ring.recall_recent(1)[0].redacted_text;
        // The secret-shaped token, the email, and the long digit run are gone.
        assert!(!stored.contains("sk-LIVE-abc123def456ghi789"), "api key leaked: {stored}");
        assert!(!stored.contains("alice@example.com"), "email leaked: {stored}");
        assert!(!stored.contains("4111111111111111"), "card leaked: {stored}");
        // It WAS redacted (the placeholder is present), proving redaction ran.
        assert!(stored.contains("[redacted]"), "expected redaction marker: {stored}");
    }

    #[test]
    fn redaction_runs_before_storage_not_after() {
        // The stored text is ALREADY redacted — there is no raw copy in the ring.
        let mut ring = ScreenContextRing::new(10);
        ring.push(1, "my password is hunter2 and key ghp_0123456789abcdefABCDEF0123", "screen");
        let stored = &ring.recall_recent(1)[0].redacted_text;
        assert!(!stored.contains("ghp_0123456789abcdefABCDEF0123"), "github token leaked: {stored}");
    }

    // -- RECALL: bounded + honest-empty ------------------------------------

    #[test]
    fn recall_recent_is_bounded_by_n() {
        let mut ring = ScreenContextRing::new(100);
        for i in 0..20u64 {
            ring.push(i, &format!("entry {i}"), "screen");
        }
        let recent = ring.recall_recent(5);
        assert_eq!(recent.len(), 5);
        // Newest 5, newest last.
        assert_eq!(recent.last().unwrap().redacted_text, "entry 19");
        assert_eq!(recent.first().unwrap().redacted_text, "entry 15");
    }

    #[test]
    fn recall_matching_filters_case_insensitively_and_is_bounded() {
        let mut ring = ScreenContextRing::new(100);
        ring.push(1, "Inbox: meeting at noon", "screen");
        ring.push(2, "Terminal cargo build", "screen");
        ring.push(3, "Inbox: lunch plans", "screen");
        let hits = ring.recall_matching("inbox", 10);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|e| e.redacted_text.to_lowercase().contains("inbox")));
    }

    // -- RANKED recall: BM25 relevance, not whole-phrase substring ----------

    #[test]
    fn recall_matching_ranks_by_term_overlap_most_relevant_first() {
        let mut ring = ScreenContextRing::new(100);
        ring.push(1, "terminal cargo build succeeded", "screen");
        ring.push(2, "browser reading the cargo docs", "screen");
        ring.push(3, "inbox lunch plans", "screen");
        // "cargo build" — entry 1 has BOTH terms, entry 2 shares only "cargo",
        // entry 3 shares neither (dropped). Most-relevant FIRST.
        let hits = ring.recall_matching("cargo build", 10);
        assert_eq!(hits.len(), 2, "the zero-overlap entry is dropped, not returned");
        assert_eq!(hits[0].redacted_text, "terminal cargo build succeeded");
        assert_eq!(hits[1].redacted_text, "browser reading the cargo docs");
        assert!(
            !hits.iter().any(|e| e.redacted_text.contains("lunch")),
            "an entry with no shared term must never be fabricated into the recall"
        );
    }

    #[test]
    fn recall_matching_matches_terms_not_the_verbatim_phrase() {
        // The value over the OLD substring filter: a natural multi-word query
        // whose words are SCATTERED (not a contiguous substring) still recalls.
        let mut ring = ScreenContextRing::new(100);
        ring.push(1, "the cargo build failed with an error", "screen");
        // "build error" is NOT a substring of the entry (the words are apart), so
        // the old `.contains("build error")` filter returned NOTHING. BM25 matches
        // on the shared terms and recalls it.
        let hits = ring.recall_matching("build error", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].redacted_text, "the cargo build failed with an error");
    }

    #[test]
    fn recall_matching_zero_overlap_is_empty_never_fabricated() {
        let mut ring = ScreenContextRing::new(100);
        ring.push(1, "editor writing the report", "screen");
        ring.push(2, "calendar standup at ten", "screen");
        // Nothing shares a term with "quarterly taxes" -> honest empty.
        assert!(ring.recall_matching("quarterly taxes", 10).is_empty());
    }

    #[test]
    fn recall_matching_bounds_to_k_by_relevance() {
        let mut ring = ScreenContextRing::new(100);
        ring.push(1, "cargo build cargo build cargo", "screen"); // most "cargo"
        ring.push(2, "cargo build once", "screen");
        ring.push(3, "a single cargo mention", "screen");
        let hits = ring.recall_matching("cargo", 2);
        assert_eq!(hits.len(), 2, "bounded to k");
        assert_eq!(hits[0].redacted_text, "cargo build cargo build cargo");
    }

    #[test]
    fn build_recall_facts_is_parallel_and_carries_the_redacted_text() {
        let entries = vec![
            ContextEntry { ts: 1, redacted_text: "alpha one".into(), source_tag: "screen".into() },
            ContextEntry { ts: 2, redacted_text: "beta two".into(), source_tag: "screen".into() },
        ];
        let facts = build_recall_facts(&entries);
        assert_eq!(facts.len(), entries.len());
        assert_eq!(facts[0].value, "alpha one");
        assert_eq!(facts[1].value, "beta two");
    }

    #[test]
    fn empty_ring_render_is_honest_never_fabricated() {
        let ring = ScreenContextRing::new(10);
        let rendered = ring.render_recall(5);
        assert!(rendered.to_lowercase().contains("no recent screen context"));
        // Nothing was invented.
        assert!(!rendered.contains("•"));
    }

    #[test]
    fn render_recall_lists_bounded_redacted_recent() {
        let mut ring = ScreenContextRing::new(10);
        ring.push(1, "draft email to the team", "screen");
        ring.push(2, "spreadsheet of Q3 numbers", "screen");
        let rendered = ring.render_recall(5);
        assert!(rendered.contains("draft email to the team"));
        assert!(rendered.contains("spreadsheet of Q3 numbers"));
    }

    // -- CLEAR: forget wipes the ring --------------------------------------

    #[test]
    fn clear_wipes_the_ring() {
        let mut ring = ScreenContextRing::new(10);
        ring.push(1, "something", "screen");
        ring.push(2, "else", "screen");
        assert_eq!(ring.len(), 2);
        ring.clear();
        assert!(ring.is_empty());
        // After forget, recall is the honest empty.
        assert!(ring.render_recall(5).to_lowercase().contains("no recent screen context"));
    }

    #[test]
    fn whitespace_only_snapshot_is_dropped_never_stored() {
        let mut ring = ScreenContextRing::new(10);
        ring.push(1, "   \n  ", "screen");
        assert!(ring.is_empty(), "an empty OCR read must not seed a fabricated entry");
    }

    // -- INTENT CLASSIFICATION (pure, conservative) ------------------------

    #[test]
    fn classifies_recall_intents() {
        for u in [
            "recall my screen context",
            "show my screen context",
            "what was I working on",
            "what was I doing earlier",
            "what was i looking at",
            "remind me of my screen context",
        ] {
            assert!(
                matches!(
                    classify_screen_context_intent(u),
                    Some(ScreenContextIntent::Recall { .. })
                ),
                "{u:?} should be a recall"
            );
        }
    }

    #[test]
    fn recall_extracts_an_optional_subject() {
        assert_eq!(
            classify_screen_context_intent("recall my screen context about the budget"),
            Some(ScreenContextIntent::Recall {
                subject: Some("budget".to_string())
            })
        );
        assert_eq!(
            classify_screen_context_intent("what was I working on regarding the report"),
            Some(ScreenContextIntent::Recall {
                subject: Some("report".to_string())
            })
        );
        // A bare recall carries no subject.
        assert_eq!(
            classify_screen_context_intent("recall my screen context"),
            Some(ScreenContextIntent::Recall { subject: None })
        );
    }

    #[test]
    fn global_subject_recall_filters_and_is_honest_empty() {
        let _g = serial();
        global_reset_for_test();
        global_push(10, 1, "inbox: budget review meeting", "screen");
        global_push(10, 2, "editor: vacation plans", "screen");
        let hit = global_render_recall_matching("budget", 10);
        assert!(hit.contains("budget review"));
        assert!(!hit.contains("vacation"));
        // A subject with no match is honest, not fabricated.
        let miss = global_render_recall_matching("quarterly taxes", 10);
        assert!(miss.to_lowercase().contains("no recent screen context"));
        global_reset_for_test();
    }

    #[test]
    fn global_len_reflects_the_ring() {
        let _g = serial();
        global_reset_for_test();
        assert_eq!(global_len(), 0);
        global_push(10, 1, "one", "screen");
        global_push(10, 2, "two", "screen");
        assert_eq!(global_len(), 2);
        global_reset_for_test();
    }

    #[test]
    fn ingest_is_off_by_default_and_grows_only_when_enabled() {
        let _g = serial();
        global_reset_for_test();
        // OFF (the default) — ingest is a no-op, the ring never grows.
        install_settings_for_test(false, 50);
        assert!(!ingest_continuous_snapshot(1, "secret work", "screen"));
        assert_eq!(global_len(), 0);
        assert!(!is_enabled());

        // ON — ingest redacts-then-pushes (bounded by the cap).
        install_settings_for_test(true, 3);
        assert!(is_enabled());
        assert!(ingest_continuous_snapshot(1, "editor: writing", "screen"));
        assert!(ingest_continuous_snapshot(2, "browser: docs token sk-LIVE-aaa111bbb222ccc333", "screen"));
        assert_eq!(global_len(), 2);
        // The secret never survived the redaction-on-ingest.
        let rendered = global_render_recall(10);
        assert!(!rendered.contains("sk-LIVE-aaa111bbb222ccc333"), "secret leaked: {rendered}");
        // Bounded — past the cap the oldest is evicted.
        ingest_continuous_snapshot(3, "terminal: cargo test", "screen");
        ingest_continuous_snapshot(4, "calendar: standup", "screen");
        assert_eq!(global_len(), 3, "ring must stay bounded at the cap");

        // Reset the global so OFF-default is restored for any later test.
        install_settings_for_test(false, 50);
        global_reset_for_test();
    }

    #[test]
    fn classifies_forget_intents() {
        for u in [
            "forget my screen context",
            "wipe my screen context",
            "clear my screen context",
            "delete my screen context",
        ] {
            assert_eq!(
                classify_screen_context_intent(u),
                Some(ScreenContextIntent::Forget),
                "{u:?} should be a forget"
            );
        }
    }

    #[test]
    fn forget_takes_precedence_over_recall() {
        // "forget" + "screen context" must wipe, never recall.
        assert_eq!(
            classify_screen_context_intent("forget my screen context"),
            Some(ScreenContextIntent::Forget)
        );
    }

    #[test]
    fn ordinary_and_oneshot_read_utterances_do_not_trigger() {
        for u in [
            // One-shot OCR reads — handled by the Vision router, NOT here.
            "read my screen",
            "what's on my screen",
            "read this",
            // Present-tense / unrelated — not a recall.
            "what am I working on",
            "what's the weather",
            "open my screen saver",
            "set the screen brightness",
        ] {
            assert_eq!(
                classify_screen_context_intent(u),
                None,
                "{u:?} must NOT trigger a screen-context intent"
            );
        }
    }

    #[test]
    fn recall_is_flagged_transient_forget_is_not() {
        assert!(is_screen_context_recall("recall my screen context"));
        assert!(is_screen_context_recall("what was I working on"));
        // Forget carries no recalled content.
        assert!(!is_screen_context_recall("forget my screen context"));
        // An ordinary turn is not transient on this account.
        assert!(!is_screen_context_recall("what's the weather"));
    }

    // -- PROCESS-GLOBAL ring: feed -> recall -> forget ---------------------

    #[test]
    fn global_ring_feeds_recalls_and_forgets_transiently() {
        let _g = serial();
        global_reset_for_test();
        // Un-fed ring => honest empty + nothing to forget.
        assert!(global_render_recall(5).to_lowercase().contains("no recent screen context"));
        assert!(!global_clear(), "an un-fed ring has nothing to forget");

        // Feed redacted snapshots (a secret never survives).
        global_push(10, 1, "inbox: standup at 10 token sk-LIVE-zzz111yyy222www333", "screen");
        global_push(10, 2, "editor: writing the report", "screen");
        let rendered = global_render_recall(5);
        assert!(rendered.contains("inbox: standup at 10"));
        assert!(!rendered.contains("sk-LIVE-zzz111yyy222www333"), "secret leaked into recall: {rendered}");
        assert!(rendered.contains("editor: writing the report"));

        // Forget wipes it — recall is honest-empty again.
        assert!(global_clear(), "a fed ring is forgettable");
        assert!(global_render_recall(5).to_lowercase().contains("no recent screen context"));
        global_reset_for_test();
    }

    // -- RUNTIME-SELECTED recall (the `screen_recall` TOOL surface) ---------
    //
    // Hermetic mock embedders (deterministic keyword->vector, mirroring the
    // docsearch/recall mock pattern) drive the neural path; a "down" embedder
    // drives the BM25 fallback. The report must NAME whichever method ran.

    /// axis 0 = terminal/cargo/build/compile, axis 1 = inbox/email/meeting/budget,
    /// axis 2 = other — so a query's keyword pins which entry is "near".
    fn keyword_vectors(texts: &[String]) -> Vec<Vec<f64>> {
        texts
            .iter()
            .map(|t| {
                let l = t.to_lowercase();
                let dev = l.contains("terminal")
                    || l.contains("cargo")
                    || l.contains("build")
                    || l.contains("compile");
                let mail = l.contains("inbox")
                    || l.contains("email")
                    || l.contains("meeting")
                    || l.contains("budget");
                if dev {
                    vec![1.0, 0.0, 0.0]
                } else if mail {
                    vec![0.0, 1.0, 0.0]
                } else {
                    vec![0.0, 0.0, 1.0]
                }
            })
            .collect()
    }

    /// A deterministic mock [`Embedder`] — the neural path ranks by keyword cosine.
    struct KeywordEmbedder;
    impl Embedder for KeywordEmbedder {
        fn embed<'a>(&'a self, texts: &'a [String]) -> crate::recall::EmbedFuture<'a> {
            let vecs = keyword_vectors(texts);
            Box::pin(async move { Ok(vecs) })
        }
    }

    /// The inference socket is DOWN — `embed` errors, so `rank_runtime_selected`
    /// falls back to lexical BM25 and must NAME that (never claim neural).
    struct DownEmbedder;
    impl Embedder for DownEmbedder {
        fn embed<'a>(&'a self, _texts: &'a [String]) -> crate::recall::EmbedFuture<'a> {
            Box::pin(async move { Err(anyhow::anyhow!("inference socket down")) })
        }
    }

    #[tokio::test]
    async fn runtime_recall_empty_ring_is_honest_never_fabricated() {
        let _g = serial();
        global_reset_for_test();
        let out = global_rank_render_runtime("anything", 5, &KeywordEmbedder).await;
        assert!(
            out.to_lowercase().contains("no recent screen context"),
            "an un-fed ring must be honest, not fabricated: {out}"
        );
        global_reset_for_test();
    }

    #[tokio::test]
    async fn runtime_recall_ranks_neurally_and_names_the_method() {
        let _g = serial();
        global_reset_for_test();
        global_push(10, 1, "terminal cargo build succeeded", "screen");
        global_push(10, 2, "inbox budget meeting notes", "screen");
        // A car/dev-axis query — neural cosine surfaces the terminal entry and
        // DROPS the orthogonal inbox entry (cosine 0).
        let out = global_rank_render_runtime("the cargo compile output", 5, &KeywordEmbedder).await;
        assert!(out.contains("terminal cargo build"), "neural should rank the dev entry: {out}");
        assert!(!out.contains("inbox budget"), "an orthogonal entry must be dropped: {out}");
        let lo = out.to_lowercase();
        assert!(
            lo.contains("neural") || lo.contains("embedding"),
            "the method must be named as neural/embedding: {out}"
        );
        global_reset_for_test();
    }

    #[tokio::test]
    async fn runtime_recall_falls_back_to_bm25_and_names_it() {
        let _g = serial();
        global_reset_for_test();
        global_push(10, 1, "terminal cargo build succeeded", "screen");
        global_push(10, 2, "inbox budget meeting notes", "screen");
        // Socket down -> lexical BM25. "cargo build" shares terms with entry 1 only.
        let out = global_rank_render_runtime("cargo build", 5, &DownEmbedder).await;
        assert!(out.contains("terminal cargo build"), "bm25 should recall the shared-term entry: {out}");
        assert!(!out.contains("inbox budget"), "no shared term -> dropped: {out}");
        let lo = out.to_lowercase();
        // The honest fallback status NAMES BM25/lexical. (It may reference "neural"
        // only to say it is NOT that — the recall.rs status literally reads "not by
        // a neural embedding model" — so the honesty check is the POSITIVE presence
        // of bm25/lexical, not the absence of the word "neural".)
        assert!(
            lo.contains("bm25") || lo.contains("lexical"),
            "fallback must be named honestly as BM25/lexical: {out}"
        );
        global_reset_for_test();
    }

    #[tokio::test]
    async fn runtime_recall_no_match_is_honest() {
        let _g = serial();
        global_reset_for_test();
        global_push(10, 1, "terminal cargo build succeeded", "screen");
        // Nothing shares a keyword/term with the query -> honest no-match.
        let out = global_rank_render_runtime("quarterly tax filing", 5, &DownEmbedder).await;
        assert!(
            out.to_lowercase().contains("no recent screen context"),
            "a no-match query is honest, not fabricated: {out}"
        );
        global_reset_for_test();
    }
}
