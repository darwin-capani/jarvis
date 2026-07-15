//! MULTI-SPEAKER DIARIZATION (#31) — the PURE, honest speaker-label mapper.
//!
//! This module turns a transcription result into a SPEAKER-LABELED transcript, with
//! no I/O, no globals, no network, and no model of its own. It has exactly two
//! honest modes:
//!
//!   * EL-SCRIBE path — when the active STT backend is ElevenLabs Scribe (the gated
//!     cloud-STT tier), the Scribe response CARRIES per-word speaker labels. [`diarize`]
//!     CONSUMES those labels: it groups the word stream into contiguous per-speaker
//!     turns and renders `[{speaker_id, text, start, end}]`. It reports ONLY the
//!     speakers Scribe actually reported.
//!   * ON-DEVICE path — on-device whisper has NO diarization model, so there is no way
//!     to know who spoke. [`single_stream`] returns ONE honest segment labeled
//!     "unknown" (a single stream). It NEVER fabricates distinct speakers the backend
//!     did not report.
//!
//! ## Posture (ON by default; INERT ON-DEVICE; EL-Scribe-gated)
//!
//! `[voice].diarize` SHIPS ON (full-power default) but is INERT ON-DEVICE: with it
//! false the transcript is rendered exactly as today (no labels). When ON,
//! diarization is still EL-SCRIBE-GATED — only when the
//! EL-Scribe backend actually returned a per-word `words` stream does [`diarize`] run on
//! the real labels (live-wired via `InferenceClient::transcribe_diarized` ->
//! `run_pipeline`); the on-device whisper path (no `words`) gets the honest single-stream
//! labeling, never a fabricated multi-speaker transcript. That limitation is stated
//! honestly here and in the `transcript.diarized` telemetry the HUD reads, never faked.

use serde::Deserialize;

/// The honest label used when no diarization model reported a speaker (the on-device
/// whisper path, or an EL response that carried no per-word speaker ids). NEVER a
/// fabricated distinct-speaker id.
pub const UNKNOWN_SPEAKER: &str = "unknown";

/// One diarized turn: a contiguous run of words attributed to ONE speaker. `start`/`end`
/// are the turn's time bounds in seconds when the backend reported them (None when it
/// did not — we never invent timings). On the on-device path there is exactly one turn
/// labeled [`UNKNOWN_SPEAKER`].
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// The speaker id EXACTLY as the backend reported it (e.g. "speaker_0"), or
    /// [`UNKNOWN_SPEAKER`] when none was reported. Never fabricated.
    pub speaker_id: String,
    /// The text of this turn (the words attributed to `speaker_id`, joined).
    pub text: String,
    /// Turn start in seconds, when the backend reported per-word timings; else None.
    pub start: Option<f64>,
    /// Turn end in seconds, when the backend reported per-word timings; else None.
    pub end: Option<f64>,
}

/// A SUBSET of the ElevenLabs Scribe speech-to-text response we care about for
/// diarization: the top-level `text` plus the per-`words` stream, each word optionally
/// carrying a `speaker_id` and `start`/`end` timing. We deserialize permissively
/// (`#[serde(default)]`, unknown fields ignored) so a Scribe response shape evolution
/// never breaks the mapper — a word with no `speaker_id` simply falls into the
/// honest-unknown bucket.
///
/// This struct is the PURE seam the daemon hands to [`diarize`]: the inference server
/// (server.py) is the only thing that talks to the Scribe network; it surfaces the
/// per-word `words` stream on the transcribe response (when Scribe diarized), the daemon
/// deserializes it into this shape (`InferenceClient::transcribe_diarized` ->
/// `run_pipeline`'s `[voice].diarize` block), and this mapper turns it into [`Turn`]s
/// without any I/O. On the on-device whisper path (no `words`) the wiring uses
/// [`single_stream`] instead — the honest single stream, never a fabricated speaker.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScribeResponse {
    /// The full transcript text (Scribe's `text`). The fallback rendering when no
    /// per-word speaker labels are present.
    #[serde(default)]
    pub text: String,
    /// The per-word stream with optional speaker labels + timings. Empty when Scribe
    /// did not return word-level detail.
    #[serde(default)]
    pub words: Vec<ScribeWord>,
}

/// One word in a Scribe response. `speaker_id` is present only when Scribe diarized;
/// `text` is the word; `start`/`end` are seconds. A `type` of "spacing"/"audio_event"
/// (vs "word") is tolerated — non-word tokens are skipped so spacing never becomes its
/// own "turn". Deserialized from the transcribe response's `words` array
/// (`InferenceClient::transcribe_diarized`) and consumed by [`diarize`] at runtime.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScribeWord {
    #[serde(default)]
    pub text: String,
    /// The diarized speaker id, EXACTLY as Scribe reported it. None => this word has no
    /// reported speaker (folded into the honest-unknown labeling, never fabricated).
    #[serde(default)]
    pub speaker_id: Option<String>,
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub end: Option<f64>,
    /// Scribe word `type` ("word" | "spacing" | "audio_event" | ...). Absent => treated
    /// as a word. Non-word tokens are skipped so spacing/events never form a turn.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

impl ScribeWord {
    /// Whether this token is an actual WORD (vs spacing / an audio event). A missing
    /// `type` is treated as a word (older/leaner responses).
    fn is_word(&self) -> bool {
        match self.kind.as_deref() {
            None | Some("word") => true,
            Some(_) => false,
        }
    }
}

/// PURE diarizer: map an EL-Scribe response to per-speaker [`Turn`]s by CONSUMING the
/// labels Scribe reported. Contiguous words sharing a `speaker_id` are coalesced into
/// one turn; a change of `speaker_id` starts a new turn. A word with no `speaker_id`
/// is attributed to [`UNKNOWN_SPEAKER`] (never fabricated as a distinct speaker), and
/// joins/extends the surrounding unknown run.
///
/// Honesty rails:
///   * we report ONLY the speaker ids Scribe actually sent — distinct turns appear iff
///     Scribe distinguished them;
///   * when Scribe sent NO per-word detail (empty `words`), we fall back to a SINGLE
///     turn carrying the whole `text` labeled [`UNKNOWN_SPEAKER`] (the response did not
///     diarize, so we don't pretend it did);
///   * timings are carried only when present; we never invent `start`/`end`.
///
/// Word text is joined with single spaces and trimmed; spacing/audio-event tokens are
/// skipped. No I/O, no globals — fully unit-testable from a synthetic response.
///
/// LIVE-WIRED: `run_pipeline`'s gated `[voice].diarize` block calls this on the Scribe
/// `words` stream surfaced by `InferenceClient::transcribe_diarized` whenever the
/// EL-Scribe backend diarized; the on-device whisper path (no `words`) uses
/// [`single_stream`] instead. The PURE mapping logic is proven by this module's tests.
pub fn diarize(resp: &ScribeResponse) -> Vec<Turn> {
    let words: Vec<&ScribeWord> = resp.words.iter().filter(|w| w.is_word()).collect();
    if words.is_empty() {
        // Scribe returned no per-word detail: the response did NOT diarize, so render
        // the whole transcript as one honest single stream (unknown speaker). Empty
        // text => no turns at all.
        let text = resp.text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        return vec![Turn {
            speaker_id: UNKNOWN_SPEAKER.to_string(),
            text: text.to_string(),
            start: None,
            end: None,
        }];
    }

    let mut turns: Vec<Turn> = Vec::new();
    for w in words {
        let speaker = w
            .speaker_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(UNKNOWN_SPEAKER);
        let word_text = w.text.trim();
        match turns.last_mut() {
            // Extend the current turn while the speaker is unchanged.
            Some(last) if last.speaker_id == speaker => {
                if !word_text.is_empty() {
                    if !last.text.is_empty() {
                        last.text.push(' ');
                    }
                    last.text.push_str(word_text);
                }
                // Extend the end bound forward when this word reports a later one.
                if let Some(e) = w.end {
                    last.end = Some(last.end.map_or(e, |cur| cur.max(e)));
                }
            }
            // Speaker changed (or first word): start a new turn.
            _ => turns.push(Turn {
                speaker_id: speaker.to_string(),
                text: word_text.to_string(),
                start: w.start,
                end: w.end,
            }),
        }
    }
    // Drop any turn that ended up textless (e.g. a stray empty-text word run).
    turns.retain(|t| !t.text.trim().is_empty());
    turns
}

/// The honest ON-DEVICE labeling: on-device whisper has NO diarization model, so we
/// return ONE turn carrying the whole transcript labeled [`UNKNOWN_SPEAKER`] (a single
/// stream). This is the truthful fallback — it NEVER fabricates distinct speakers. An
/// empty/blank transcript yields no turns.
pub fn single_stream(transcript: &str) -> Vec<Turn> {
    let text = transcript.trim();
    if text.is_empty() {
        return Vec::new();
    }
    vec![Turn {
        speaker_id: UNKNOWN_SPEAKER.to_string(),
        text: text.to_string(),
        start: None,
        end: None,
    }]
}

/// Whether a set of turns actually distinguished more than one speaker — i.e. the
/// backend genuinely diarized. Used by the wiring to label telemetry honestly
/// ("diarized: true" only when distinct speakers were reported). [`UNKNOWN_SPEAKER`]
/// alone is NOT a distinct speaker.
pub fn is_multi_speaker(turns: &[Turn]) -> bool {
    let mut seen: Vec<&str> = Vec::new();
    for t in turns {
        if t.speaker_id != UNKNOWN_SPEAKER && !seen.contains(&t.speaker_id.as_str()) {
            seen.push(&t.speaker_id);
        }
    }
    seen.len() > 1
}

/// Render diarized turns into a human/HUD-readable transcript: one "speaker: text" line
/// per turn. A single unknown-speaker turn renders as just the bare text (no spurious
/// "unknown:" prefix on the on-device single-stream path — that would be noise). Used to
/// surface the diarized transcript to the router/log/telemetry.
pub fn render(turns: &[Turn]) -> String {
    if turns.len() == 1 && turns[0].speaker_id == UNKNOWN_SPEAKER {
        return turns[0].text.clone();
    }
    turns
        .iter()
        .map(|t| format!("{}: {}", t.speaker_id, t.text))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic Scribe word.
    fn word(text: &str, speaker: Option<&str>, start: f64, end: f64) -> ScribeWord {
        ScribeWord {
            text: text.to_string(),
            speaker_id: speaker.map(String::from),
            start: Some(start),
            end: Some(end),
            kind: Some("word".to_string()),
        }
    }

    #[test]
    fn maps_two_speakers_into_contiguous_turns_with_timings() {
        // A synthetic Scribe-style response: speaker_0 says "hello there", speaker_1
        // replies "hi darwin", speaker_0 again "all good".
        let resp = ScribeResponse {
            text: "hello there hi darwin all good".to_string(),
            words: vec![
                word("hello", Some("speaker_0"), 0.0, 0.4),
                word("there", Some("speaker_0"), 0.5, 0.9),
                word("hi", Some("speaker_1"), 1.2, 1.4),
                word("darwin", Some("speaker_1"), 1.5, 1.9),
                word("all", Some("speaker_0"), 2.2, 2.4),
                word("good", Some("speaker_0"), 2.5, 2.9),
            ],
        };
        let turns = diarize(&resp);
        assert_eq!(turns.len(), 3, "three contiguous turns: s0, s1, s0");
        assert_eq!(turns[0], Turn { speaker_id: "speaker_0".into(), text: "hello there".into(), start: Some(0.0), end: Some(0.9) });
        assert_eq!(turns[1], Turn { speaker_id: "speaker_1".into(), text: "hi darwin".into(), start: Some(1.2), end: Some(1.9) });
        assert_eq!(turns[2], Turn { speaker_id: "speaker_0".into(), text: "all good".into(), start: Some(2.2), end: Some(2.9) });
        assert!(is_multi_speaker(&turns), "two distinct speakers reported");
        // Rendered transcript carries the real labels.
        assert_eq!(
            render(&turns),
            "speaker_0: hello there\nspeaker_1: hi darwin\nspeaker_0: all good"
        );
    }

    #[test]
    fn coalesces_a_single_speaker_into_one_turn() {
        let resp = ScribeResponse {
            text: "what is the time".to_string(),
            words: vec![
                word("what", Some("speaker_0"), 0.0, 0.3),
                word("is", Some("speaker_0"), 0.4, 0.5),
                word("the", Some("speaker_0"), 0.6, 0.7),
                word("time", Some("speaker_0"), 0.8, 1.1),
            ],
        };
        let turns = diarize(&resp);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker_id, "speaker_0");
        assert_eq!(turns[0].text, "what is the time");
        assert!(!is_multi_speaker(&turns), "one speaker is not multi-speaker");
    }

    #[test]
    fn words_without_a_speaker_id_are_unknown_never_fabricated() {
        // Scribe returned words but NO speaker ids (it did not diarize): every word is
        // honestly UNKNOWN, coalesced into a single unknown turn — NEVER fabricated as
        // distinct speakers.
        let resp = ScribeResponse {
            text: "hello world".to_string(),
            words: vec![
                word("hello", None, 0.0, 0.4),
                word("world", None, 0.5, 0.9),
            ],
        };
        let turns = diarize(&resp);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker_id, UNKNOWN_SPEAKER);
        assert_eq!(turns[0].text, "hello world");
        assert!(!is_multi_speaker(&turns), "unknown alone is not multi-speaker");
        // Rendered as the bare text (no spurious "unknown:" prefix).
        assert_eq!(render(&turns), "hello world");
    }

    #[test]
    fn empty_words_falls_back_to_one_unknown_turn_from_the_text() {
        // No per-word detail at all: render the whole `text` as a single honest stream.
        let resp = ScribeResponse {
            text: "the response had only text".to_string(),
            words: vec![],
        };
        let turns = diarize(&resp);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker_id, UNKNOWN_SPEAKER);
        assert_eq!(turns[0].text, "the response had only text");
        assert_eq!(turns[0].start, None, "no timing invented");
        assert_eq!(turns[0].end, None);
    }

    #[test]
    fn skips_spacing_and_audio_event_tokens() {
        let resp = ScribeResponse {
            text: "hi there".to_string(),
            words: vec![
                word("hi", Some("speaker_0"), 0.0, 0.2),
                ScribeWord { text: " ".into(), speaker_id: Some("speaker_0".into()), start: Some(0.2), end: Some(0.3), kind: Some("spacing".into()) },
                ScribeWord { text: "(laughter)".into(), speaker_id: None, start: Some(0.3), end: Some(0.5), kind: Some("audio_event".into()) },
                word("there", Some("speaker_0"), 0.6, 0.9),
            ],
        };
        let turns = diarize(&resp);
        assert_eq!(turns.len(), 1, "spacing/audio_event tokens are skipped, not turns");
        assert_eq!(turns[0].text, "hi there");
    }

    #[test]
    fn single_stream_is_the_honest_on_device_fallback() {
        // The on-device whisper path: no diarization model -> exactly one unknown turn.
        let turns = single_stream("  whatever the user said  ");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker_id, UNKNOWN_SPEAKER);
        assert_eq!(turns[0].text, "whatever the user said");
        assert!(!is_multi_speaker(&turns));
        // Blank input -> no turns.
        assert!(single_stream("   ").is_empty());
    }

    #[test]
    fn deserializes_a_real_scribe_style_json_payload() {
        // Prove the serde shape matches a Scribe response (text + words with
        // speaker_id/type/timings, unknown fields ignored).
        let raw = r#"{
            "language_code": "en",
            "text": "hello hi",
            "words": [
                {"text": "hello", "type": "word", "speaker_id": "speaker_0", "start": 0.0, "end": 0.4, "logprob": -0.1},
                {"text": "hi", "type": "word", "speaker_id": "speaker_1", "start": 0.5, "end": 0.7}
            ]
        }"#;
        let resp: ScribeResponse = serde_json::from_str(raw).expect("parse scribe json");
        let turns = diarize(&resp);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].speaker_id, "speaker_0");
        assert_eq!(turns[1].speaker_id, "speaker_1");
        assert!(is_multi_speaker(&turns));
    }
}
