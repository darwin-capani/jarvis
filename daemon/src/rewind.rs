//! SESSION REWIND (F12) — REVIEW-ONLY time travel: "what happened at 2pm" /
//! "rewind the last hour" / "walk me through this morning" reconstructs a
//! bounded timeline of the window from the RECORDED stores and narrates it.
//!
//! REVIEW-ONLY, EMPHATICALLY: this module reconstructs and DISPLAYS the past;
//! it never re-executes anything. (In this codebase "replay" already means
//! re-execution twice over — the confirmed-action replay and the macro replay —
//! which is exactly what a rewind is NOT.)
//!
//! SOURCES (both already redacted at write — the payload is secret-free by
//! construction):
//!   * EPISODES (episodic.rs / memory.rs) — the redacted, agent-scoped record
//!     of completed turns; `summary` is documented as "the human-readable line
//!     a timeline surface shows". Deliberately NOT raw transcripts: transcripts
//!     retain raw recipients and include the transient turns (screen reads,
//!     describes) that the episodic gate excludes by privacy design — a rewind
//!     must not resurrect what the gate withheld.
//!   * AUDIT entries — the hash-chained consequential-action record, with its
//!     REDACTED target summary (never chain hashes on the wire).
//!
//! HONESTY: the narration says "recorded" — episodes keep only completed,
//! non-transient, (voice-gated) turns, so absence of evidence is stated as
//! exactly that. Items past the cap are DISCLOSED via `items_omitted`, never
//! silently dropped. A failed read degrades to an honest empty, never an error.
//!
//! The time parser is PURE over an injectable `now` with a fixed offset, so
//! every window computation is hermetically tested regardless of machine TZ.
//! KNOWN, ACCEPTED EDGE: the offset is frozen at ask time, so a window that
//! crosses a DST transition (a "yesterday" window on a changeover day) is
//! skewed by the DST delta — at most one hour, twice a year, on windows that
//! are deliberately fuzzy (clock asks are ±45 minutes). Rebuilding around a
//! per-day tz database was judged not worth the dependency for that edge.

use chrono::{DateTime, Duration, FixedOffset, TimeZone, Utc};
use serde_json::{json, Value};

use crate::audit::AuditEntry;
use crate::memory::Episode;

/// Timeline items carried on the wire / narrated. Anything beyond is disclosed
/// via `items_omitted`.
const MAX_ITEMS: usize = 20;
/// Per-string bound on wire text (sources are pre-redacted; bound anyway).
const ITEM_CHARS: usize = 200;
/// The widest "last N hours/minutes" window accepted (a rewind is a glance at
/// recent history, not an archive export).
const MAX_LOOKBACK_HOURS: i64 = 24;
/// Half-width of the window for a clock-time ask ("around 2pm" -> 2pm ± 45m).
const CLOCK_HALF_WINDOW_MIN: i64 = 45;

/// A resolved rewind window: UTC RFC3339 bounds (lexically comparable against
/// every store's ts column) plus the human label spoken back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub from_utc: String,
    pub to_utc: String,
    pub label: String,
}

// ---------------------------------------------------------------------------
// Intent — explicit, phrase-anchored, deliberately deferential
// ---------------------------------------------------------------------------

/// Detect a session-rewind intent and resolve its time window. PURE over the
/// injectable `now` (with the caller's UTC offset), and CONSERVATIVE:
///   * a GATE phrase is required ("what happened", "rewind", "walk me
///     through", "replay") AND a TIME QUALIFIER — a bare "what happened" stays
///     a conversational question for the model;
///   * lifelog's own-activity phrases ("what did I do", "my day", "my week",
///     "my activity") are never matched here — that arm runs FIRST and owns
///     them; macro replay's "replay (the) macro X" prefix is disjoint too.
pub fn classify_rewind_intent(text: &str, now: DateTime<FixedOffset>) -> Option<Window> {
    let t = text.trim().to_lowercase();
    let t = t.trim_end_matches(['.', '!', '?']).trim();

    let gated = ["what happened", "rewind", "walk me through", "replay"]
        .iter()
        .any(|g| t.contains(g));
    if !gated {
        return None;
    }
    // Never shadow the macro-replay verb ("replay the macro X").
    if t.contains("macro") {
        return None;
    }

    // "the last hour" / "the last N minutes|hours" / "the past ..."
    if let Some(w) = parse_last_window(t, now) {
        return Some(w);
    }
    // "at 2pm" / "around 2:30 pm" / "at noon"
    if let Some(w) = parse_clock_window(t, now) {
        return Some(w);
    }
    // Day parts: today / yesterday / this|yesterday morning|afternoon|evening
    parse_daypart_window(t, now)
}

/// "the last hour", "the last 30 minutes", "the past 2 hours". The lookback
/// clamp is applied BEFORE the label is built, so the spoken/HUD window claim
/// always states the ACTUAL coverage ("the last 500 hours" resolves to — and
/// says — "the last 24 hours").
fn parse_last_window(t: &str, now: DateTime<FixedOffset>) -> Option<Window> {
    let rest = t
        .split_once("the last ")
        .or_else(|| t.split_once("the past "))
        .map(|(_, r)| r)?;
    let minutes = if rest.starts_with("hour") {
        60
    } else {
        let mut parts = rest.splitn(2, ' ');
        let n: i64 = parts.next()?.parse().ok()?;
        let unit = parts.next()?;
        if n <= 0 {
            return None;
        }
        if unit.starts_with("minute") {
            n
        } else if unit.starts_with("hour") {
            // Clamp BEFORE the multiply: a huge spoken hour count (an i64 up to
            // ~9.2e18 parses fine) would overflow `n * 60` — panic in debug,
            // wrap NEGATIVE in release, where the negative survives the .min
            // cap below and produces a future-dated window with a false label.
            n.min(MAX_LOOKBACK_HOURS) * 60
        } else {
            return None;
        }
    };
    let minutes = minutes.min(MAX_LOOKBACK_HOURS * 60);
    let label = if minutes == 60 {
        "the last hour".to_string()
    } else if minutes % 60 == 0 {
        format!("the last {} hours", minutes / 60)
    } else {
        format!("the last {minutes} minutes")
    };
    Some(window(now - Duration::minutes(minutes), now, label))
}

/// "at 2pm", "around 2:30 pm", "to 9am", "at noon" — with trailing words
/// tolerated ("at 2pm today") and an explicit "yesterday" anywhere in the
/// utterance honored ("yesterday at 2pm" means YESTERDAY's 2pm, not today's).
/// Absent an explicit day, a clock time that hasn't happened yet today means
/// the previous day's occurrence. Keywords are space-delimited words (a bare
/// "at " substring also lives inside "what ") and each candidate must PARSE to
/// count — a keyword followed by non-clock text falls through to the next
/// form rather than swallowing the utterance.
fn parse_clock_window(t: &str, now: DateTime<FixedOffset>) -> Option<Window> {
    let explicit_yesterday = t.contains("yesterday");
    for keyword in [" at ", " around ", " to "] {
        if let Some((_, rest)) = t.rsplit_once(keyword) {
            if let Some(w) = clock_window_from(rest, explicit_yesterday, now) {
                return Some(w);
            }
        }
    }
    None
}

fn clock_window_from(
    rest: &str,
    explicit_yesterday: bool,
    now: DateTime<FixedOffset>,
) -> Option<Window> {
    // Collapse spaces, then read the LEADING clock token — trailing words
    // ("today", "please") must not break the parse.
    let compact: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == ':' || *c == ' ')
        .filter(|c| *c != ' ')
        .collect();
    let (hour, minute, mut label) = if compact.starts_with("noon") {
        (12u32, 0u32, "around noon".to_string())
    } else {
        let digits: String =
            compact.chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
        if digits.is_empty() {
            return None;
        }
        let after = &compact[digits.len()..];
        let mer = if after.starts_with("pm") {
            true
        } else if after.starts_with("am") {
            false
        } else {
            return None;
        };
        let (h, m) = match digits.split_once(':') {
            Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
            None => (digits.parse::<u32>().ok()?, 0),
        };
        if h == 0 || h > 12 || m > 59 {
            return None;
        }
        let h24 = match (h, mer) {
            (12, true) => 12,
            (12, false) => 0,
            (h, true) => h + 12,
            (h, false) => h,
        };
        let label = if m == 0 {
            format!("around {h} {}", if mer { "pm" } else { "am" })
        } else {
            format!("around {h}:{m:02} {}", if mer { "pm" } else { "am" })
        };
        (h24, m, label)
    };
    let today = now.date_naive();
    let centre_naive = today.and_hms_opt(hour, minute, 0)?;
    let mut centre = now.timezone().from_local_datetime(&centre_naive).single()?;
    if explicit_yesterday {
        centre -= Duration::days(1);
        label.push_str(" yesterday");
    } else if centre > now {
        centre -= Duration::days(1);
    }
    let from = centre - Duration::minutes(CLOCK_HALF_WINDOW_MIN);
    let to = (centre + Duration::minutes(CLOCK_HALF_WINDOW_MIN)).min(now);
    Some(window(from, to, label))
}

/// "today", "yesterday", "this morning", "yesterday afternoon", …
fn parse_daypart_window(t: &str, now: DateTime<FixedOffset>) -> Option<Window> {
    let yesterday = t.contains("yesterday");
    let (start_h, end_h, part) = if t.contains("morning") {
        (5u32, 12u32, Some("morning"))
    } else if t.contains("afternoon") {
        (12, 18, Some("afternoon"))
    } else if t.contains("evening") || t.contains("tonight") {
        (18, 23, Some("evening"))
    } else if t.contains("today") || yesterday {
        (0, 24, None)
    } else {
        return None;
    };
    let day = if yesterday { now.date_naive() - Duration::days(1) } else { now.date_naive() };
    let from = now.timezone().from_local_datetime(&day.and_hms_opt(start_h, 0, 0)?).single()?;
    let to_naive = if end_h == 24 {
        (day + Duration::days(1)).and_hms_opt(0, 0, 0)?
    } else {
        day.and_hms_opt(end_h, 0, 0)?
    };
    let to = now.timezone().from_local_datetime(&to_naive).single()?.min(now);
    if to <= from {
        return None; // e.g. "this evening" asked in the morning
    }
    let label = match (yesterday, part) {
        (false, Some(p)) => format!("this {p}"),
        (true, Some(p)) => format!("yesterday {p}"),
        (false, None) => "today".to_string(),
        (true, None) => "yesterday".to_string(),
    };
    Some(window(from, to, label))
}

fn window(from: DateTime<FixedOffset>, to: DateTime<FixedOffset>, label: String) -> Window {
    Window {
        from_utc: from.with_timezone(&Utc).to_rfc3339(),
        to_utc: to.with_timezone(&Utc).to_rfc3339(),
        label,
    }
}

// ---------------------------------------------------------------------------
// The timeline — a pure fold over the recorded stores
// ---------------------------------------------------------------------------

/// One timeline item: a recorded TURN (episode) or a gated ACTION (audit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindItem {
    pub ts: String,
    /// "turn" | "action".
    pub kind: &'static str,
    pub text: String,
    pub detail: String,
}

/// The reconstructed window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewind {
    pub label: String,
    pub from_utc: String,
    pub to_utc: String,
    pub turn_count: usize,
    pub action_count: usize,
    /// True when a store READ saturated its cap — the counts are then a FLOOR
    /// ("at least N"), disclosed as such, never presented as exact.
    pub counts_floor: bool,
    /// Items dropped past [`MAX_ITEMS`] — DISCLOSED, never silent. The cap
    /// keeps the NEWEST end, so "most recently" stays true; what is dropped
    /// is the window's earliest items.
    pub items_omitted: usize,
    /// Chronological (oldest first), capped newest-biased.
    pub items: Vec<RewindItem>,
}

impl Rewind {
    pub fn is_empty(&self) -> bool {
        self.turn_count == 0 && self.action_count == 0
    }
}

fn bound(s: &str) -> String {
    if s.chars().count() <= ITEM_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(ITEM_CHARS).collect();
        out.push('…');
        out
    }
}

/// Fold the window's episodes + audit entries into one chronological timeline.
/// PURE — hermetically tested. Both reads are window-scoped by the caller;
/// `counts_floor` says a read SATURATED its cap (the counts are then a floor,
/// disclosed in the narration and on the wire). This fn trusts its inputs'
/// window membership but NOT their order.
pub fn build_timeline(
    window: &Window,
    episodes: &[Episode],
    actions: &[AuditEntry],
    counts_floor: bool,
) -> Rewind {
    let mut items: Vec<RewindItem> = Vec::with_capacity(episodes.len() + actions.len());
    for e in episodes {
        let text = if e.summary.trim().is_empty() { &e.topic } else { &e.summary };
        items.push(RewindItem {
            ts: e.ts.clone(),
            kind: "turn",
            text: bound(text),
            detail: bound(&e.topic),
        });
    }
    for a in actions {
        items.push(RewindItem {
            ts: a.ts.clone(),
            kind: "action",
            // The gate verdict IS the story: "gmail_send — parked/denied/executed".
            text: bound(&format!("{} — {}", a.tool, a.outcome)),
            detail: bound(&a.target_redacted),
        });
    }
    items.sort_by(|x, y| x.ts.cmp(&y.ts));
    let total = items.len();
    if total > MAX_ITEMS {
        // Keep the NEWEST end: "most recently" in the narration must always
        // name the window's true latest item; the drop (of the EARLIEST
        // items) is disclosed via items_omitted.
        items.drain(..total - MAX_ITEMS);
    }
    Rewind {
        label: window.label.clone(),
        from_utc: window.from_utc.clone(),
        to_utc: window.to_utc.clone(),
        turn_count: episodes.len(),
        action_count: actions.len(),
        counts_floor,
        items_omitted: total - items.len(),
        items,
    }
}

/// The spoken narration (lifelog register: first-person, honest-empty, the
/// full detail lives on the HUD).
pub fn render_spoken(r: &Rewind) -> String {
    if r.is_empty() {
        return format!(
            "I have nothing recorded for {}, sir — my episode log keeps only completed, \
             non-transient turns, and no gated action ran in that window.",
            r.label
        );
    }
    let floor = if r.counts_floor { "at least " } else { "" };
    let mut out = format!(
        "Rewinding {}, sir: {floor}{} recorded turn{} and {floor}{} gated action{}.",
        r.label,
        r.turn_count,
        if r.turn_count == 1 { "" } else { "s" },
        r.action_count,
        if r.action_count == 1 { "" } else { "s" },
    );
    // "First:" only when the timeline is complete — with the newest-biased cap
    // the first SHOWN item is not the window's first happening.
    if r.items_omitted == 0 {
        if let Some(first) = r.items.first() {
            out.push_str(&format!(" First: {}.", first.text));
        }
    }
    if r.items.len() > 1 || r.items_omitted > 0 {
        if let Some(last) = r.items.last() {
            out.push_str(&format!(" Most recently: {}.", last.text));
        }
    }
    if r.items_omitted > 0 {
        out.push_str(&format!(
            " The latest {} of the window are on the HUD; {} earlier aren't shown.",
            r.items.len(),
            r.items_omitted
        ));
    } else {
        out.push_str(" The full timeline is on the HUD.");
    }
    out
}

/// The SECRET-FREE `session.rewind` wire payload. Sources are redacted at
/// write (episodes twice; audit targets once) and every string is bounded
/// here; chain hashes never ride. WIRE CONTRACT (mirrored by
/// hud/src/core/events.ts::parseSessionRewind; pinned by tests on both sides):
///
///   { "label", "from", "to", "empty", "turn_count", "action_count",
///     "counts_floor", "items_omitted",
///     "items": [ { "ts", "kind", "text", "detail" } ] }
pub fn payload(r: &Rewind) -> Value {
    json!({
        "label": r.label,
        "from": r.from_utc,
        "to": r.to_utc,
        "empty": r.is_empty(),
        "turn_count": r.turn_count,
        "action_count": r.action_count,
        "counts_floor": r.counts_floor,
        "items_omitted": r.items_omitted,
        "items": r.items.iter().map(|i| json!({
            "ts": i.ts,
            "kind": i.kind,
            "text": i.text,
            "detail": i.detail,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Tests — pure and hermetic (fixed-offset clock; hand-built store rows)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-07-13 15:00:00 -05:00 — a fixed afternoon, machine-TZ-independent.
    fn now() -> DateTime<FixedOffset> {
        FixedOffset::west_opt(5 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 7, 13, 15, 0, 0)
            .unwrap()
    }

    fn classify(text: &str) -> Option<Window> {
        classify_rewind_intent(text, now())
    }

    // -- classifier gating ------------------------------------------------------

    #[test]
    fn requires_a_gate_phrase_and_a_time_qualifier() {
        // Gate without qualifier: stays a conversational question.
        assert!(classify("what happened").is_none());
        assert!(classify("rewind").is_none());
        // Qualifier without gate: not ours.
        assert!(classify("the last hour was rough").is_none());
        // Both -> a window.
        assert!(classify("what happened in the last hour").is_some());
        assert!(classify("rewind the last hour").is_some());
        assert!(classify("walk me through this morning").is_some());
    }

    #[test]
    fn never_shadows_lifelog_or_macro_phrases() {
        // Lifelog owns own-activity phrasing — none of its cue phrases carry a
        // rewind gate, so they can never reach a window here.
        assert!(classify("what did i do this week").is_none());
        assert!(classify("show my activity today").is_none());
        // Macro replay's verb stays macro replay's.
        assert!(classify("replay the macro standup").is_none());
        assert!(classify("replay macro standup").is_none());
    }

    // -- window math (fixed offset, deterministic) -------------------------------

    #[test]
    fn last_window_forms_resolve_and_cap() {
        let w = classify("rewind the last hour").unwrap();
        assert_eq!(w.label, "the last hour");
        assert_eq!(w.from_utc, "2026-07-13T19:00:00+00:00"); // 14:00 -05:00
        assert_eq!(w.to_utc, "2026-07-13T20:00:00+00:00"); // 15:00 -05:00

        let w = classify("what happened in the last 30 minutes").unwrap();
        assert_eq!(w.label, "the last 30 minutes");
        assert_eq!(w.from_utc, "2026-07-13T19:30:00+00:00");

        // A silly lookback clamps to the 24h max — and the LABEL says what the
        // window actually covers, never the unclamped ask.
        let w = classify("rewind the last 500 hours").unwrap();
        assert_eq!(w.from_utc, "2026-07-12T20:00:00+00:00");
        assert_eq!(w.label, "the last 24 hours", "label states actual coverage");
        // Zero/garbage numbers never resolve.
        assert!(classify("rewind the last 0 hours").is_none());

        // OVERFLOW regression (CodeRabbit sweep): an 18-digit hour count parses
        // as i64 and used to overflow `n * 60` — debug panic, release wrap to a
        // NEGATIVE that slipped past the cap and produced a future-dated
        // window. The clamp now precedes the multiply: 24h window, honest label.
        let w = classify("rewind the last 200000000000000000 hours").unwrap();
        assert_eq!(w.from_utc, "2026-07-12T20:00:00+00:00");
        assert_eq!(w.label, "the last 24 hours");
    }

    #[test]
    fn clock_windows_resolve_with_yesterday_fallback() {
        let w = classify("what happened at 2pm").unwrap();
        assert_eq!(w.label, "around 2 pm");
        // 2pm -05:00 = 19:00Z; ±45min, capped at now (15:00 local = 20:00Z).
        assert_eq!(w.from_utc, "2026-07-13T18:15:00+00:00");
        assert_eq!(w.to_utc, "2026-07-13T19:45:00+00:00");

        let w = classify("what happened around 2:30 pm").unwrap();
        assert_eq!(w.label, "around 2:30 pm");

        // Trailing words never break the clock parse into a full-day window.
        let w = classify("what happened at 2pm today").unwrap();
        assert_eq!(w.label, "around 2 pm");
        assert_eq!(w.from_utc, "2026-07-13T18:15:00+00:00");

        // An explicit "yesterday" means YESTERDAY's occurrence — even for a
        // clock time that already happened today.
        let w = classify("what happened yesterday at 2pm").unwrap();
        assert_eq!(w.label, "around 2 pm yesterday");
        assert_eq!(w.from_utc, "2026-07-12T18:15:00+00:00");

        // A time later than now means the previous occurrence.
        let w = classify("what happened at 9pm").unwrap();
        assert!(w.from_utc.starts_with("2026-07-13T01:15"), "yesterday 9pm -05:00: {w:?}");

        let w = classify("what happened at noon").unwrap();
        assert_eq!(w.label, "around noon");
        // Nonsense clock values never resolve.
        assert!(classify("what happened at 13pm").is_none());
        assert!(classify("what happened at 0am").is_none());
    }

    #[test]
    fn daypart_windows_resolve_and_never_run_backwards() {
        let w = classify("walk me through this morning").unwrap();
        assert_eq!(w.label, "this morning");
        assert_eq!(w.from_utc, "2026-07-13T10:00:00+00:00"); // 05:00 -05:00
        assert_eq!(w.to_utc, "2026-07-13T17:00:00+00:00"); // 12:00 -05:00

        let w = classify("what happened yesterday afternoon").unwrap();
        assert_eq!(w.label, "yesterday afternoon");
        assert_eq!(w.from_utc, "2026-07-12T17:00:00+00:00");

        let w = classify("what happened today").unwrap();
        assert_eq!(w.label, "today");
        assert_eq!(w.from_utc, "2026-07-13T05:00:00+00:00"); // local midnight
        assert_eq!(w.to_utc, "2026-07-13T20:00:00+00:00"); // capped at now

        // "this evening" asked at 3pm: the window hasn't begun — no fabrication.
        assert!(classify("what happened this evening").is_none());
    }

    // -- the timeline fold --------------------------------------------------------

    fn ep(ts: &str, summary: &str, topic: &str) -> Episode {
        Episode {
            id: 0,
            ts: ts.to_string(),
            agent_namespace: "agent.darwin".to_string(),
            utterance_redacted: String::new(),
            topic: topic.to_string(),
            salient_entities: Vec::new(),
            outcome: "answered".to_string(),
            summary: summary.to_string(),
        }
    }

    fn act(ts: &str, tool: &str, outcome: &str, target: &str) -> AuditEntry {
        AuditEntry {
            seq: 0,
            ts: ts.to_string(),
            agent: "agent.pepper".to_string(),
            tool: tool.to_string(),
            target_redacted: target.to_string(),
            decision: "ask".to_string(),
            outcome: outcome.to_string(),
            prev_hash: "p".to_string(),
            entry_hash: "e".to_string(),
        }
    }

    fn win() -> Window {
        Window {
            from_utc: "2026-07-13T19:00:00+00:00".to_string(),
            to_utc: "2026-07-13T20:00:00+00:00".to_string(),
            label: "the last hour".to_string(),
        }
    }

    #[test]
    fn timeline_merges_chronologically_and_counts_honestly() {
        let episodes = vec![
            ep("2026-07-13T19:40:00+00:00", "Checked the weather", "weather"),
            ep("2026-07-13T19:10:00+00:00", "Asked about inflation", "economics"),
        ];
        let actions = vec![act(
            "2026-07-13T19:20:00+00:00",
            "gmail_send",
            "parked",
            "an email to [redacted]",
        )];
        let r = build_timeline(&win(), &episodes, &actions, false);
        assert_eq!(r.turn_count, 2);
        assert_eq!(r.action_count, 1);
        assert_eq!(r.items_omitted, 0);
        // Oldest first, sources interleaved by ts.
        assert_eq!(
            r.items.iter().map(|i| i.kind).collect::<Vec<_>>(),
            ["turn", "action", "turn"]
        );
        assert_eq!(r.items[1].text, "gmail_send — parked");
        assert_eq!(r.items[1].detail, "an email to [redacted]");
        // Chain hashes never reach the wire.
        let text = payload(&r).to_string();
        assert!(!text.contains("prev_hash") && !text.contains("entry_hash"));
    }

    #[test]
    fn timeline_caps_and_discloses_and_falls_back_to_topic() {
        let mut episodes: Vec<Episode> = (0..30)
            .map(|i| ep(&format!("2026-07-13T19:{i:02}:00+00:00"), &format!("turn {i}"), "t"))
            .collect();
        episodes.push(ep("2026-07-13T19:59:00+00:00", "", "bare-topic"));
        let r = build_timeline(&win(), &episodes, &[], false);
        assert_eq!(r.items.len(), 20);
        assert_eq!(r.items_omitted, 11, "the drop is disclosed");
        assert_eq!(r.turn_count, 31, "counts are never capped");
        // Newest-biased: the LAST shown item is the window's true latest, so
        // the spoken "Most recently:" is never false; the EARLIEST were dropped.
        assert_eq!(r.items.last().unwrap().text, "bare-topic");
        assert_eq!(r.items.first().unwrap().text, "turn 11");
        // An empty summary falls back to the topic, never an empty line.
        let bare = build_timeline(&win(), &[ep("2026-07-13T19:00:00+00:00", " ", "bare-topic")], &[], false);
        assert_eq!(bare.items[0].text, "bare-topic");
        // Long strings are bounded with an ellipsis.
        let long = build_timeline(&win(), &[ep("2026-07-13T19:00:00+00:00", &"x".repeat(500), "t")], &[], false);
        assert!(long.items[0].text.chars().count() <= ITEM_CHARS + 1);
        assert!(long.items[0].text.ends_with('…'));
    }

    // -- narration + wire shape ----------------------------------------------------

    #[test]
    fn narration_is_honest_for_empty_and_bounded_for_full() {
        let empty = build_timeline(&win(), &[], &[], false);
        let s = render_spoken(&empty);
        assert!(s.contains("nothing recorded for the last hour"), "{s}");
        assert!(s.contains("non-transient"), "says WHY the record can be empty: {s}");

        let r = build_timeline(
            &win(),
            &[
                ep("2026-07-13T19:10:00+00:00", "Asked about inflation", "economics"),
                ep("2026-07-13T19:40:00+00:00", "Checked the weather", "weather"),
            ],
            &[],
            false,
        );
        let s = render_spoken(&r);
        assert!(s.starts_with("Rewinding the last hour, sir: 2 recorded turns"), "{s}");
        assert!(s.contains("First: Asked about inflation."), "{s}");
        assert!(s.contains("Most recently: Checked the weather."), "{s}");
        assert!(s.contains("on the HUD"), "{s}");
    }

    #[test]
    fn payload_pins_the_wire_shape() {
        let r = build_timeline(
            &win(),
            &[ep("2026-07-13T19:10:00+00:00", "Asked about inflation", "economics")],
            &[],
            false,
        );
        let p = payload(&r);
        let keys: Vec<&String> = p.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "action_count",
                "counts_floor",
                "empty",
                "from",
                "items",
                "items_omitted",
                "label",
                "to",
                "turn_count"
            ]
        );
        assert_eq!(p["empty"], false);
        assert_eq!(p["label"], "the last hour");
        let row_keys: Vec<&String> = p["items"][0].as_object().unwrap().keys().collect();
        assert_eq!(row_keys, ["detail", "kind", "text", "ts"]);
    }
}
