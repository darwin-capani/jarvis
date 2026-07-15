//! SEMANTIC PASTEBOARD — recall-by-MEANING over the macOS clipboard, on-device.
//!
//! An OPT-IN poller watches the system pasteboard; when a NEW clip appears it is
//! PII-REDACTED at the source and stored in a BOUNDED in-RAM ring. A read-only
//! recall surface then ranks the stored clips by MEANING against a free-text query
//! ("the thing I copied about the lease") via the EXISTING recall.rs ranker, and an
//! OPTIONAL, confirm-gated `pasteboard_put` sets the pasteboard (a benign pasteboard
//! SET only — never a keystroke or file mutation).
//!
//! ## Safety / privacy contract (mirrors [screen_context] + [optimize])
//!   * SHIPS OFF: `[pasteboard].enabled = false`. Capturing the clipboard is
//!     privacy-sensitive, so nothing is polled or stored until the user opts in.
//!     With it off [`global_ingest`] is a pure NO-OP (returns false, stores
//!     nothing) and the poll loop is never spawned.
//!   * PII-REDACTED at the source: every clip is passed through
//!     [`crate::optimize::redact`] BEFORE it can enter the store — the raw clip
//!     never lives in a [`Clip`] (enforced by [`capture_clip`], the single
//!     construction seam, AND re-applied inside [`global_ingest`]).
//!   * BOUNDED retention: the ring is capped (`[pasteboard].retention`); insert
//!     evicts the OLDEST clip past the cap, so the clipboard history cannot grow
//!     without bound on an always-on appliance. The ring is TRANSIENT (in-RAM
//!     only — never written to memory / the optimizer corpus / disk).
//!
//! ## The device-gated tap is the RUNNER; the store is a PURE seam
//! The NSPasteboard read (here via the dependency-free `/usr/bin/pbpaste` bounded
//! subprocess — the CLI has no `changeCount`, so a content-hash compare stands in
//! for it) is DEVICE-GATED and NOT exercised by any test. What IS unit-tested is
//! the PURE core: the bounded store (insert / eviction / dedup), the
//! redaction-before-storage seam, the ranking-query CONSTRUCTION (clips ->
//! `recall::Fact`s ranked by recall.rs), the OFF-stores-nothing gate, and the
//! intent classifier. The `pasteboard_put` actuator's DryRun preview is tested;
//! its Execute (a real pbcopy) is device-gated and built-not-run.

use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::integrations::ActionMode;
use crate::optimize::redact;
use crate::recall::{self, Embedder, Fact, LexicalProvider};

/// Max characters of a redacted clip echoed as a PREVIEW to the HUD status frame.
/// Previews are always POST-redaction and truncated — the panel shows a glance,
/// never the full clip, and never anything pre-redaction.
const PREVIEW_LEN: usize = 80;

/// How many recent redacted-clip previews the `pasteboard.status` frame carries
/// for the HUD panel. Small + bounded so a status frame can never flood the hub.
const HUD_PREVIEW_COUNT: usize = 8;

/// Fallback cap/interval for the off-state settings (config owns the real, floored
/// defaults; these only seed the pre-install OFF settings).
const FALLBACK_CAP: usize = 50;
const FALLBACK_INTERVAL_SECS: u64 = 3;

// ---------------------------------------------------------------------------
// Clip + redaction-before-storage seam
// ---------------------------------------------------------------------------

/// One captured clipboard entry, ALREADY PII-REDACTED. `text` is the redacted
/// form — the raw clip NEVER lives in a `Clip` (built only via [`capture_clip`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    /// The clip text with all PII/secret spans stripped (see [`redact`]). This is
    /// the ONLY representation of the clip the store ever holds.
    pub text: String,
    /// Unix seconds when the clip was captured (ordering + retention).
    pub ts: u64,
}

/// Redact a raw clipboard string and wrap it as a [`Clip`]. The single seam that
/// guarantees a clip is redacted BEFORE it can be stored (mirrors
/// [`crate::optimize::Trace::new`], which redacts an utterance at construction).
pub fn capture_clip(raw: &str, ts: u64) -> Clip {
    Clip {
        text: redact(raw),
        ts,
    }
}

/// A short, redaction-safe PREVIEW of an ALREADY-redacted clip: trimmed to
/// [`PREVIEW_LEN`] with a trailing ellipsis when clipped. Pure.
fn preview(redacted: &str) -> String {
    let trimmed = redacted.trim();
    if trimmed.chars().count() <= PREVIEW_LEN {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(PREVIEW_LEN).collect();
    format!("{}…", head.trim_end())
}

// ---------------------------------------------------------------------------
// Bounded, in-RAM clip store — the PURE, unit-tested heart
// ---------------------------------------------------------------------------

/// A BOUNDED clip ring: newest at the back, oldest at the front. Evicts the oldest
/// clip past `cap`, and DEDUPES an identical (redacted) clip by refreshing it to
/// newest rather than storing a second copy. PURE + deterministic.
pub struct PasteboardStore {
    clips: VecDeque<Clip>,
    cap: usize,
}

impl PasteboardStore {
    /// A store bounded to `cap` clips (floored to >= 1 so a misconfigured 0 never
    /// makes the ring useless).
    pub fn new(cap: usize) -> Self {
        Self {
            clips: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    /// The retention cap (>= 1). A public accessor exercised by the unit tests;
    /// the live path reads the cap through the config / global settings.
    #[allow(dead_code)]
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Number of stored clips.
    pub fn len(&self) -> usize {
        self.clips.len()
    }

    /// Whether the ring is empty.
    pub fn is_empty(&self) -> bool {
        self.clips.is_empty()
    }

    /// Insert an (already-redacted) clip. DEDUP: if a clip with identical text is
    /// already present it is REMOVED first, so the clip is refreshed to newest
    /// rather than duplicated. EVICT-OLDEST: any clip past `cap` is dropped from
    /// the front (oldest). Pure.
    pub fn insert(&mut self, clip: Clip) {
        if let Some(pos) = self.clips.iter().position(|c| c.text == clip.text) {
            self.clips.remove(pos);
        }
        self.clips.push_back(clip);
        while self.clips.len() > self.cap {
            self.clips.pop_front();
        }
    }

    /// Retune the cap in place, evicting the oldest clips if it shrank. Floored to
    /// >= 1 (a 0 cap would make the ring useless).
    pub fn set_cap(&mut self, cap: usize) {
        self.cap = cap.max(1);
        while self.clips.len() > self.cap {
            self.clips.pop_front();
        }
    }

    /// Up to `n` clips, NEWEST first.
    pub fn recent(&self, n: usize) -> Vec<Clip> {
        self.clips.iter().rev().take(n).cloned().collect()
    }

    /// Every stored clip, NEWEST first (the full window the ranker reasons over).
    pub fn snapshot(&self) -> Vec<Clip> {
        self.clips.iter().rev().cloned().collect()
    }

    /// Drop every clip.
    pub fn clear(&mut self) {
        self.clips.clear();
    }
}

// ---------------------------------------------------------------------------
// Ranking-query CONSTRUCTION seam (clips -> recall::Fact, ranked by recall.rs)
// ---------------------------------------------------------------------------

/// Build [`recall::Fact`] rows from `clips`, PARALLEL to the input slice (fact `i`
/// is clip `i`). The RANKING-QUERY-CONSTRUCTION seam: each clip becomes a Fact
/// whose VALUE is the redacted clip text, so recall.rs ranks clips by MEANING
/// against a query. The key is the constant `"clipboard"` (low-signal, so it never
/// dominates BM25); the value carries the searchable content. Pure.
pub fn build_recall_facts(clips: &[Clip]) -> Vec<Fact> {
    clips
        .iter()
        .map(|c| Fact {
            key: "clipboard".to_string(),
            value: c.text.clone(),
        })
        .collect()
}

/// PURE lexical ranking of `clips` by meaning against `query`, returning up to `k`
/// clips most-relevant first. Reuses recall.rs's BM25 [`LexicalProvider`] + `rank`
/// over the constructed facts, mapping each hit's parallel index back to its clip.
/// A no-match query returns an EMPTY vec (honest "nothing bears on that" — never a
/// fabricated clip).
pub fn rank_clips_lexical(query: &str, clips: &[Clip], k: usize) -> Vec<Clip> {
    let facts = build_recall_facts(clips);
    recall::rank(query, &facts, k, &LexicalProvider::default())
        .into_iter()
        .map(|h| clips[h.index].clone())
        .collect()
}

/// Render a lexical recall over `clips` for `query` as a spoken reply — honest
/// about an empty history and an honest no-match (never fabricates a clip).
pub fn render_recall(query: &str, clips: &[Clip], k: usize) -> String {
    if clips.is_empty() {
        return "Nothing has been copied since the semantic pasteboard was enabled, sir \
                — there is no clipboard history to search yet."
            .to_string();
    }
    let ranked = rank_clips_lexical(query, clips, k);
    if ranked.is_empty() {
        return format!(
            "I have nothing in your clipboard history that bears on \"{}\", sir.",
            query.trim()
        );
    }
    let lines: Vec<String> = ranked.iter().map(|c| format!("- {}", c.text)).collect();
    format!(
        "Here is what you copied that bears on that, most relevant first:\n{}",
        lines.join("\n")
    )
}

// ---------------------------------------------------------------------------
// Process-global ring + settings (mirrors screen_context's poison-tolerant slot)
// ---------------------------------------------------------------------------

/// The live `[pasteboard]` settings, installed ONCE at startup. A poison-tolerant
/// `Mutex` global (mirrors screen_context / lockdown process-global state).
#[derive(Debug, Clone, Copy)]
struct PasteSettings {
    enabled: bool,
    cap: usize,
    poll_interval_secs: u64,
}

impl PasteSettings {
    /// The shipped OFF default: nothing polled or stored.
    const fn off() -> Self {
        Self {
            enabled: false,
            cap: FALLBACK_CAP,
            poll_interval_secs: FALLBACK_INTERVAL_SECS,
        }
    }
}

static RING: Mutex<Option<PasteboardStore>> = Mutex::new(None);
static SETTINGS: Mutex<PasteSettings> = Mutex::new(PasteSettings::off());

fn ring_lock() -> MutexGuard<'static, Option<PasteboardStore>> {
    RING.lock().unwrap_or_else(|p| p.into_inner())
}

fn settings_lock() -> MutexGuard<'static, PasteSettings> {
    SETTINGS.lock().unwrap_or_else(|p| p.into_inner())
}

/// Install the `[pasteboard]` settings ONCE from config at startup. When ENABLED,
/// the bounded ring is created (or retuned to the new cap). When OFF (the shipped
/// default), the ring is DROPPED entirely — so nothing is retained and the poll
/// loop (which checks [`is_enabled`]) ingests nothing.
pub fn install_settings(enabled: bool, cap: usize, poll_interval_secs: u64) {
    let cap = cap.max(1);
    *settings_lock() = PasteSettings {
        enabled,
        cap,
        poll_interval_secs: poll_interval_secs.max(1),
    };
    let mut ring = ring_lock();
    if enabled {
        match ring.as_mut() {
            Some(store) => store.set_cap(cap),
            None => *ring = Some(PasteboardStore::new(cap)),
        }
    } else {
        // OFF: retain nothing (a runtime disable also wipes the transient ring).
        *ring = None;
    }
}

/// Whether the semantic pasteboard is currently on (the poll loop checks this each
/// tick, so a runtime disable stops ingestion immediately).
pub fn is_enabled() -> bool {
    settings_lock().enabled
}

/// Ingest a RAW clip IFF enabled: redact it, then store it. Returns whether it was
/// stored. When OFF this is a pure NO-OP — it stores NOTHING and returns `false`
/// (the privacy guarantee). An empty/whitespace-only clip is skipped. The clip is
/// REDACTED here (via [`capture_clip`]) before it can enter the ring.
pub fn global_ingest(raw: &str, ts: u64) -> bool {
    let settings = *settings_lock();
    if !settings.enabled {
        return false; // shipped-OFF default: capture NOTHING.
    }
    if raw.trim().is_empty() {
        return false;
    }
    let clip = capture_clip(raw, ts);
    let mut ring = ring_lock();
    let store = ring.get_or_insert_with(|| PasteboardStore::new(settings.cap));
    store.insert(clip);
    true
}

/// Up to `n` stored clips, newest first (empty when off / un-fed).
pub fn global_recent(n: usize) -> Vec<Clip> {
    ring_lock().as_ref().map(|s| s.recent(n)).unwrap_or_default()
}

/// The full stored window, newest first (what the ranker reasons over).
fn global_snapshot() -> Vec<Clip> {
    ring_lock().as_ref().map(|s| s.snapshot()).unwrap_or_default()
}

/// Number of stored clips (0 when off / un-fed).
pub fn global_len() -> usize {
    ring_lock().as_ref().map(|s| s.len()).unwrap_or(0)
}

/// Wipe the clipboard history. Returns whether anything was cleared.
pub fn global_clear() -> bool {
    let mut ring = ring_lock();
    match ring.as_mut() {
        Some(store) => {
            let had = !store.is_empty();
            store.clear();
            had
        }
        None => false,
    }
}

/// Render a LEXICAL recall over the live ring — the router-op recall surface.
pub fn global_render_recall(query: &str, k: usize) -> String {
    render_recall(query, &global_snapshot(), k)
}

/// Render a RUNTIME-SELECTED recall over the live ring — the `pasteboard_recall`
/// TOOL surface. Prefers NEURAL on-device embeddings (via the injected `embedder`,
/// the same on-device inference socket mnemosyne_recall uses) and FALLS BACK to
/// lexical BM25 when it is unavailable, naming whichever method actually ran. It
/// NEVER fabricates: an empty history or a no-match query is reported honestly.
pub async fn global_rank_render_runtime(query: &str, k: usize, embedder: &dyn Embedder) -> String {
    let clips = global_snapshot();
    if clips.is_empty() {
        return "Nothing has been copied since the semantic pasteboard was enabled, sir \
                — there is no clipboard history to search yet."
            .to_string();
    }
    let facts = build_recall_facts(&clips);
    let recall = recall::rank_runtime_selected(query, &facts, k, embedder).await;
    let method = recall.method_status;
    if recall.hits.is_empty() {
        return format!(
            "I have nothing in your clipboard history that bears on \"{}\", sir. \
             Note: this is {method}",
            query.trim()
        );
    }
    let lines: Vec<String> = recall
        .hits
        .iter()
        .map(|h| format!("- {}", clips[h.index].text))
        .collect();
    format!(
        "Here is what you copied that bears on that, most relevant first:\n{}\n\
         (Recall method: {method})",
        lines.join("\n")
    )
}

// ---------------------------------------------------------------------------
// Telemetry — the secret-free `pasteboard.status` frame for the HUD panel
// ---------------------------------------------------------------------------

/// Build the `pasteboard.status` payload: the enabled flag, the clip COUNT, the
/// cap, the poll interval, and up to [`HUD_PREVIEW_COUNT`] recent clip PREVIEWS —
/// each ALREADY PII-redacted (at capture) and truncated (never the full clip, never
/// anything pre-redaction). When off, `recent` is empty and the count is 0.
pub fn status_frame() -> Value {
    let settings = *settings_lock();
    let recent: Vec<String> = if settings.enabled {
        global_recent(HUD_PREVIEW_COUNT)
            .iter()
            .map(|c| preview(&c.text))
            .collect()
    } else {
        Vec::new()
    };
    json!({
        "enabled": settings.enabled,
        "count": global_len(),
        "cap": settings.cap,
        "poll_interval_secs": settings.poll_interval_secs,
        "recent": recent,
    })
}

/// Emit the `pasteboard.status` telemetry frame for the HUD.
pub fn emit_status() {
    crate::telemetry::emit("pasteboard", "pasteboard.status", status_frame());
}

// ---------------------------------------------------------------------------
// The OPTIONAL, confirm-gated pasteboard_put actuator (a benign pasteboard SET)
// ---------------------------------------------------------------------------

/// Set the pasteboard to `text`. In [`ActionMode::DryRun`] it returns a faithful
/// PREVIEW and performs NO side effect; in [`ActionMode::Execute`] it writes the
/// text via `/usr/bin/pbcopy` (macOS; device-gated, built-not-run in tests).
///
/// BENIGN by construction: this ONLY sets the pasteboard — it never posts a
/// keystroke, mutates a file, or reaches the network. It is nonetheless
/// CONSEQUENTIAL (a visible side effect on the user's clipboard), so it is in
/// [`crate::confirm::CONSEQUENTIAL_TOOLS`] and parks for a spoken human "yes" —
/// exactly one confirm authorizes exactly one set.
pub async fn put_actuator(text: &str, mode: ActionMode) -> anyhow::Result<String> {
    if text.is_empty() {
        return Err(anyhow::anyhow!("nothing to copy: the text to place on the clipboard is empty"));
    }
    let shown = preview(text);
    if mode == ActionMode::DryRun {
        return Ok(format!(
            "[dry run] Would copy to your clipboard: \"{shown}\". \
             Enable consequential actions and confirm to copy."
        ));
    }
    write_pasteboard_text(text).await?;
    Ok(format!("Copied to your clipboard, sir: \"{shown}\"."))
}

// ---------------------------------------------------------------------------
// The DEVICE-GATED tap + poll loop (the RUNNER — not exercised by any test)
// ---------------------------------------------------------------------------

/// Read the current pasteboard text via `/usr/bin/pbpaste`. Returns `None` on any
/// error, an empty pasteboard, or a non-text pasteboard. DEVICE-GATED: it reads the
/// user's real clipboard, so it only ever runs inside the enabled poll loop.
#[cfg(target_os = "macos")]
async fn read_pasteboard_text() -> Option<String> {
    let out = tokio::process::Command::new("/usr/bin/pbpaste")
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(not(target_os = "macos"))]
async fn read_pasteboard_text() -> Option<String> {
    None
}

/// Write `text` to the pasteboard via `/usr/bin/pbcopy`. The ONLY mutation
/// `pasteboard_put` performs — a pasteboard SET, never a keystroke or file write.
#[cfg(target_os = "macos")]
async fn write_pasteboard_text(text: &str) -> anyhow::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("/usr/bin/pbcopy")
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::anyhow!("pbcopy exited with a failure status"))
    }
}

#[cfg(not(target_os = "macos"))]
async fn write_pasteboard_text(_text: &str) -> anyhow::Result<()> {
    Err(anyhow::anyhow!(
        "setting the pasteboard is only supported on macOS"
    ))
}

/// A fast, non-cryptographic content hash — the dependency-free stand-in for
/// NSPasteboard's `changeCount` (the CLI `pbpaste` exposes no changeCount, so we
/// detect a NEW clip by a change in content hash between polls).
fn content_hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Unix seconds now (the capture timestamp).
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The OPT-IN pasteboard poll loop — spawned by main ONLY when
/// `[pasteboard].enabled`. Each tick it reads the pasteboard; when the content
/// CHANGES (a new clip), it redacts + stores it and emits `pasteboard.status`. It
/// re-checks [`is_enabled`] every tick so a runtime disable stops ingestion at
/// once. The read itself is device-gated (real clipboard); this loop is never
/// exercised by a test.
pub async fn poll_loop(interval_secs: u64) {
    let mut last_hash: Option<u64> = None;
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
    // Announce the initial (empty) status so the HUD panel renders immediately.
    emit_status();
    loop {
        ticker.tick().await;
        if !is_enabled() {
            continue;
        }
        let Some(text) = read_pasteboard_text().await else {
            continue;
        };
        let h = content_hash(&text);
        if last_hash == Some(h) {
            continue; // unchanged since the last poll -> not a new clip
        }
        last_hash = Some(h);
        if global_ingest(&text, now_unix()) {
            emit_status();
        }
    }
}

// ---------------------------------------------------------------------------
// Router-op intent classifier (the "recall the thing I copied about X" surface)
// ---------------------------------------------------------------------------

/// A recognized semantic-pasteboard voice command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PasteboardIntent {
    /// Recall clips by meaning; `subject` narrows to a topic when the user named
    /// one ("what did I copy ABOUT the lease" -> subject = "the lease").
    Recall { subject: Option<String> },
    /// Wipe the clipboard history.
    Forget,
}

/// CONSERVATIVE classifier for the semantic-pasteboard router op. Recognizes only
/// explicit RECALL ("what did I copy…", "the thing I copied about…", "search my
/// clipboard history…", "recall my clipboard") and FORGET ("forget/clear/wipe my
/// clipboard [history]") phrasings. It deliberately does NOT match an imperative
/// COPY ("copy this to my clipboard") — that is the `pasteboard_put` tool, not a
/// recall — so a put request never lands on the read op. Pure + deterministic.
pub fn classify_pasteboard_intent(utterance: &str) -> Option<PasteboardIntent> {
    let lower = utterance.trim().to_lowercase();
    if lower.is_empty() {
        return None;
    }
    // A pasteboard command must reference the clipboard OR a past copy.
    let mentions_clipboard = lower.contains("clipboard");
    let mentions_copied = lower.contains("copied") || lower.contains("i copy");

    // FORGET: an explicit wipe of the clipboard history. Requires "clipboard" so a
    // generic "forget that" never wipes the ring.
    if mentions_clipboard
        && (lower.contains("forget") || lower.contains("clear") || lower.contains("wipe"))
    {
        return Some(PasteboardIntent::Forget);
    }

    // RECALL: an explicit retrieval cue over the clipboard / past copies. An
    // imperative "copy X to my clipboard" (a PUT) has neither "copied"/"i copy"
    // nor a recall verb, so it is excluded here.
    let recall_cue = lower.contains("what did i copy")
        || lower.contains("what have i copied")
        || lower.contains("thing i copied")
        || lower.contains("things i copied")
        || lower.contains("clipboard history")
        || lower.contains("recall")
        || lower.contains("find")
        || lower.contains("search");
    if (mentions_clipboard || mentions_copied) && recall_cue {
        return Some(PasteboardIntent::Recall {
            subject: extract_subject(&lower),
        });
    }
    None
}

/// Pull the topic after an "about"/"regarding" cue, if present, so recall can
/// narrow ("the thing I copied ABOUT the lease" -> "the lease"). Returns `None`
/// when no topic marker is present (a bare recall ranks the whole history).
fn extract_subject(lower: &str) -> Option<String> {
    for marker in [" about ", " regarding ", " on the topic of ", " mentioning "] {
        if let Some(idx) = lower.find(marker) {
            let tail = lower[idx + marker.len()..].trim();
            let subject = tail.trim_end_matches(['.', '?', '!', ',']).trim();
            if !subject.is_empty() {
                return Some(subject.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Test-only reset for the process-global ring/settings
// ---------------------------------------------------------------------------

/// Reset the process-global ring + settings to the shipped OFF default. Tests that
/// touch the globals call this on entry (under the module serial lock) so one case
/// never leaks state into the next.
#[cfg(test)]
pub fn reset_for_test() {
    *settings_lock() = PasteSettings::off();
    *ring_lock() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    // The globals are process-global; serialize every global-touching test.
    fn serial() -> MutexGuard<'static, ()> {
        static SERIAL: Mutex<()> = Mutex::new(());
        let g = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_test();
        g
    }

    // ---- the bounded store: insert + eviction + dedup ----------------------

    #[test]
    fn insert_stores_newest_last_and_recent_is_newest_first() {
        let mut store = PasteboardStore::new(10);
        store.insert(Clip { text: "one".into(), ts: 1 });
        store.insert(Clip { text: "two".into(), ts: 2 });
        store.insert(Clip { text: "three".into(), ts: 3 });
        assert_eq!(store.len(), 3);
        let recent = store.recent(2);
        assert_eq!(recent[0].text, "three", "newest first");
        assert_eq!(recent[1].text, "two");
    }

    #[test]
    fn eviction_drops_the_oldest_past_the_cap() {
        let mut store = PasteboardStore::new(2);
        store.insert(Clip { text: "a".into(), ts: 1 });
        store.insert(Clip { text: "b".into(), ts: 2 });
        store.insert(Clip { text: "c".into(), ts: 3 });
        assert_eq!(store.len(), 2, "cap holds the ring to 2");
        let snap = store.snapshot();
        let texts: Vec<&str> = snap.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["c", "b"], "the oldest (a) was evicted, newest first");
    }

    #[test]
    fn dedup_collapses_an_identical_clip_and_refreshes_recency() {
        let mut store = PasteboardStore::new(10);
        store.insert(Clip { text: "same".into(), ts: 1 });
        store.insert(Clip { text: "other".into(), ts: 2 });
        // Re-copying the identical clip must NOT create a second entry...
        store.insert(Clip { text: "same".into(), ts: 3 });
        assert_eq!(store.len(), 2, "identical clip deduped to one entry");
        // ...and it is refreshed to newest (its ts updated to the latest copy).
        let recent = store.recent(2);
        assert_eq!(recent[0].text, "same", "the re-copied clip is now newest");
        assert_eq!(recent[0].ts, 3, "the refreshed clip carries the newest ts");
    }

    #[test]
    fn set_cap_shrinks_and_evicts_oldest() {
        let mut store = PasteboardStore::new(5);
        for i in 0..5 {
            store.insert(Clip { text: format!("c{i}"), ts: i });
        }
        store.set_cap(2);
        assert_eq!(store.len(), 2);
        let snap = store.snapshot();
        let texts: Vec<&str> = snap.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["c4", "c3"], "shrink keeps the newest two");
    }

    #[test]
    fn cap_is_floored_to_one() {
        let store = PasteboardStore::new(0);
        assert_eq!(store.cap(), 1, "a 0 cap is floored so the ring is never useless");
    }

    // ---- redaction BEFORE storage ------------------------------------------

    #[test]
    fn capture_clip_redacts_a_secret_shaped_clip_before_storage() {
        // An api-key-shaped token + an email must be stripped at capture — the raw
        // secret NEVER reaches a Clip.
        let clip = capture_clip("token sk-livexayz1234567890abcdef email me@example.com", 7);
        assert!(clip.text.contains("[redacted]"), "the secret is redacted: {}", clip.text);
        assert!(!clip.text.contains("sk-livexayz1234567890abcdef"), "raw key must not survive: {}", clip.text);
        assert!(!clip.text.contains("me@example.com"), "raw email must not survive: {}", clip.text);
    }

    #[test]
    fn global_ingest_redacts_before_it_reaches_the_ring() {
        let _g = serial();
        install_settings(true, 10, 3);
        let stored = global_ingest("my card is 4111111111111111 and pin 123456", 1);
        assert!(stored, "an enabled ingest stores the clip");
        let recent = global_recent(1);
        assert_eq!(recent.len(), 1);
        assert!(recent[0].text.contains("[redacted]"), "long digit runs redacted: {}", recent[0].text);
        assert!(!recent[0].text.contains("4111111111111111"), "raw PAN must not survive: {}", recent[0].text);
    }

    // ---- OFF stores nothing (the privacy gate) -----------------------------

    #[test]
    fn off_stores_nothing() {
        let _g = serial();
        install_settings(false, 10, 3); // shipped default posture
        assert!(!is_enabled());
        let stored = global_ingest("a secret I copied", 1);
        assert!(!stored, "an ingest while OFF must store nothing");
        assert_eq!(global_len(), 0, "the ring stays empty when off");
        assert!(global_recent(5).is_empty());
        // The status frame is honest: off, empty, no previews.
        let frame = status_frame();
        assert_eq!(frame["enabled"], json!(false));
        assert_eq!(frame["count"], json!(0));
        assert_eq!(frame["recent"], json!([]));
    }

    #[test]
    fn a_runtime_disable_wipes_the_transient_ring() {
        let _g = serial();
        install_settings(true, 10, 3);
        assert!(global_ingest("something", 1));
        assert_eq!(global_len(), 1);
        // Re-installing OFF drops the ring — nothing is retained.
        install_settings(false, 10, 3);
        assert_eq!(global_len(), 0, "disabling wipes the in-RAM ring");
    }

    // ---- ranking-query construction (reuses recall.rs) ---------------------

    #[test]
    fn build_recall_facts_is_parallel_and_carries_the_redacted_text() {
        let clips = vec![
            Clip { text: "the office lease renews in March".into(), ts: 1 },
            Clip { text: "buy oat milk and coffee".into(), ts: 2 },
        ];
        let facts = build_recall_facts(&clips);
        assert_eq!(facts.len(), 2, "one fact per clip, parallel");
        assert_eq!(facts[0].value, clips[0].text, "fact value is the clip text");
        assert_eq!(facts[0].key, "clipboard", "low-signal constant key");
    }

    #[test]
    fn rank_clips_lexical_recalls_by_meaning() {
        let clips = vec![
            Clip { text: "the office lease renews in March and rent goes up".into(), ts: 1 },
            Clip { text: "buy oat milk, coffee, and bread".into(), ts: 2 },
            Clip { text: "call the dentist about the appointment".into(), ts: 3 },
        ];
        // "the thing I copied about the lease" -> the lease clip ranks first.
        let ranked = rank_clips_lexical("lease rent", &clips, 3);
        assert!(!ranked.is_empty(), "the lease clip must be recalled");
        assert!(ranked[0].text.contains("lease"), "the lease clip ranks first: {:?}", ranked[0]);
    }

    #[test]
    fn rank_clips_no_match_returns_nothing_never_fabricates() {
        let clips = vec![Clip { text: "buy oat milk and coffee".into(), ts: 1 }];
        let ranked = rank_clips_lexical("quantum chromodynamics", &clips, 5);
        assert!(ranked.is_empty(), "a no-match query recalls nothing (no fabrication)");
    }

    #[test]
    fn render_recall_is_honest_about_empty_and_no_match() {
        // Empty history.
        let empty = render_recall("anything", &[], 5);
        assert!(empty.to_lowercase().contains("nothing has been copied"), "{empty}");
        // A non-empty history with no matching clip.
        let clips = vec![Clip { text: "buy oat milk".into(), ts: 1 }];
        let miss = render_recall("the lease", &clips, 5);
        assert!(miss.to_lowercase().contains("nothing"), "{miss}");
        // A match renders the clip.
        let hit = render_recall("oat milk", &clips, 5);
        assert!(hit.contains("oat milk"), "{hit}");
    }

    #[test]
    fn global_render_recall_ranks_the_live_ring() {
        let _g = serial();
        install_settings(true, 10, 3);
        global_ingest("the office lease renews in March", 1);
        global_ingest("buy oat milk and coffee", 2);
        let rendered = global_render_recall("lease", 5);
        assert!(rendered.contains("lease"), "the live ring is ranked: {rendered}");
    }

    // ---- eviction + dedup through the global ingest path --------------------

    #[test]
    fn global_ingest_evicts_and_dedups_through_the_ring() {
        let _g = serial();
        install_settings(true, 2, 3);
        global_ingest("first", 1);
        global_ingest("second", 2);
        global_ingest("third", 3); // evicts "first"
        assert_eq!(global_len(), 2, "the cap bounds the live ring");
        let texts: Vec<String> = global_recent(5).into_iter().map(|c| c.text).collect();
        assert_eq!(texts, vec!["third".to_string(), "second".to_string()]);
        // Re-copying an existing clip dedups (no growth).
        global_ingest("third", 4);
        assert_eq!(global_len(), 2, "a re-copied clip does not grow the ring");
    }

    // ---- the status frame is secret-free (counts + truncated previews) ------

    #[test]
    fn status_frame_carries_counts_and_redacted_truncated_previews() {
        let _g = serial();
        install_settings(true, 10, 5);
        global_ingest("email me@example.com about the lease renewal terms", 1);
        let frame = status_frame();
        assert_eq!(frame["enabled"], json!(true));
        assert_eq!(frame["count"], json!(1));
        assert_eq!(frame["cap"], json!(10));
        assert_eq!(frame["poll_interval_secs"], json!(5));
        let recent = frame["recent"].as_array().expect("recent array");
        assert_eq!(recent.len(), 1);
        let p = recent[0].as_str().unwrap();
        assert!(p.contains("[redacted]"), "preview is post-redaction: {p}");
        assert!(!p.contains("me@example.com"), "preview never leaks a raw email: {p}");
    }

    #[test]
    fn preview_truncates_a_long_clip() {
        let long = "x".repeat(200);
        let p = preview(&long);
        assert!(p.chars().count() <= PREVIEW_LEN + 1, "preview is bounded: {} chars", p.chars().count());
        assert!(p.ends_with('…'), "a clipped preview ends with an ellipsis");
    }

    // ---- the pasteboard_put actuator: benign preview vs gated execute -------

    #[tokio::test]
    async fn put_actuator_dry_run_previews_and_sets_nothing() {
        let out = put_actuator("copy this text", ActionMode::DryRun).await.unwrap();
        assert!(out.starts_with("[dry run]"), "dry run leads with the preview marker: {out}");
        assert!(out.contains("copy this text"), "the preview names what would be copied: {out}");
        assert!(out.to_lowercase().contains("clipboard"), "the preview names the clipboard: {out}");
    }

    #[tokio::test]
    async fn put_actuator_rejects_empty_text() {
        assert!(put_actuator("", ActionMode::DryRun).await.is_err(), "empty text is nothing to copy");
    }

    // ---- the router-op intent classifier -----------------------------------

    #[test]
    fn classify_recall_and_forget_and_subject() {
        // Recall, no subject.
        assert_eq!(
            classify_pasteboard_intent("recall my clipboard"),
            Some(PasteboardIntent::Recall { subject: None })
        );
        assert_eq!(
            classify_pasteboard_intent("what did i copy earlier"),
            Some(PasteboardIntent::Recall { subject: None })
        );
        // Recall WITH a subject ("the thing I copied about the lease").
        assert_eq!(
            classify_pasteboard_intent("find the thing i copied about the lease"),
            Some(PasteboardIntent::Recall { subject: Some("the lease".to_string()) })
        );
        // Forget.
        assert_eq!(classify_pasteboard_intent("forget my clipboard history"), Some(PasteboardIntent::Forget));
        assert_eq!(classify_pasteboard_intent("clear my clipboard"), Some(PasteboardIntent::Forget));
    }

    #[test]
    fn classify_ignores_ordinary_utterances_and_a_put_request() {
        // A PUT ("copy X to my clipboard") is NOT a recall — it must not match the
        // read op (it routes to the pasteboard_put tool instead).
        assert_eq!(classify_pasteboard_intent("copy this to my clipboard"), None);
        assert_eq!(classify_pasteboard_intent("put my address on the clipboard"), None);
        // Ordinary sentences never trip it.
        assert_eq!(classify_pasteboard_intent("what's the weather"), None);
        assert_eq!(classify_pasteboard_intent("remind me about the lease"), None);
        assert_eq!(classify_pasteboard_intent(""), None);
    }
}
