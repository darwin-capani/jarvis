//! HERALD-EARS — LIVE CAPTIONS (captions.rs): the PURE caption-ASSEMBLY seam.
//!
//! Turns the EXISTING on-device STT transcript feed into a `captions.line` telemetry
//! stream — one line per diarized turn carrying `{ text, speaker_label, optional
//! translation, ts }` for the HUD's LIVE CAPTIONS band. The ASSEMBLY / LABEL /
//! TRANSLATE-DECISION logic is a PURE, hermetically-tested seam; the live STT tap
//! ([`emit_captions_live`]) is the DEVICE-GATED runner that reuses the existing mic/TCC
//! grant — NO new recording to disk, no extra audio leaves the device (the transcript
//! already exists on the run_pipeline path).
//!
//! ## What is PURE here and what is DEVICE-GATED
//!
//! [`assemble_captions`] (and its helpers [`caption_line`], [`wants_translation`],
//! [`turns_for`]) is the PURE, hermetically-tested core: feed it diarized turns + a
//! target language + an injected translator, and it returns the [`CaptionLine`]s (and
//! fires the secret-free-shaped `captions.line` telemetry the HUD reads). The LIVE
//! captions mode (driving this from the freshly-transcribed utterance on the
//! device-gated mic path) is [`emit_captions_live`] — it touches the inference socket
//! for the optional translation, so it is NOT claimed measured; only the pure core is
//! proven headlessly.
//!
//! ## Reuse (no duplication)
//!   * [`crate::diarize`] — `Turn` / `UNKNOWN_SPEAKER` / `diarize` / `single_stream` for
//!     the HONEST speaker labels: distinct per-speaker turns ONLY when the EL-Scribe
//!     backend actually diarized; otherwise a SINGLE "unknown" stream (on-device whisper
//!     has no diarization model) — NEVER a fabricated speaker.
//!   * [`crate::interpret`] — `SegmentTranslator` + `build_segment_prompt` +
//!     `OnDeviceSegmentTranslator` (the SAME on-device Babel translate path) for the
//!     optional per-line translation.
//!
//! ## Honesty (load-bearing — never a fabricated caption)
//!   * unseparated audio -> ONE `unknown` line, never invented speakers (diarize.rs);
//!   * empty target language -> passthrough: the line carries the transcript text with
//!     NO translation (`translation = None`), never a fabricated rendering;
//!   * a translate failure (model unreachable / offline) or an empty model reply ->
//!     the line STILL shows, `translation = None` — the local ~4B model's limit is stated
//!     honestly, never a fabricated translation;
//!   * READ-ONLY DISPLAY — no action, no routing, no network beyond the injected
//!     translator; a blank turn is dropped, never emitted as an empty caption.

use serde_json::json;

use crate::config::Config;
use crate::diarize::{ScribeResponse, ScribeWord, Turn};
use crate::interpret::{build_segment_prompt, OnDeviceSegmentTranslator, SegmentTranslator};
use crate::telemetry;

/// One assembled caption line. `translation` is `Some` ONLY when a real, non-empty
/// on-device translation landed (empty target / offline / an empty model reply leave it
/// `None` — an honest passthrough, never a fabricated rendering). `ts` is an epoch-ms
/// timestamp for ordering within the band.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionLine {
    /// The transcript text of this turn (trimmed). Always safe to show.
    pub text: String,
    /// The speaker label EXACTLY as diarize.rs reported it (e.g. "speaker_0"), or
    /// [`crate::diarize::UNKNOWN_SPEAKER`] ("unknown") on the honest single stream.
    /// NEVER fabricated.
    pub speaker_label: String,
    /// The on-device translation of `text`, when `[captions].translate_to` is set AND a
    /// real translation was produced. `None` on passthrough (empty target) or an honest
    /// degrade (offline / empty reply) — never a fabricated translation.
    pub translation: Option<String>,
    /// Epoch-ms timestamp for this line (ordering within the band).
    pub ts: u64,
}

/// PURE: build ONE caption line from a diarized [`Turn`] + an already-resolved optional
/// translation. Trims the text and (when present) the translation, folding a
/// blank/whitespace translation back to `None` so a garbled model reply never renders as
/// an empty translation. No I/O. This is the assembly/label seam — the speaker label is
/// carried VERBATIM from diarize.rs (`unknown` on the single stream), never invented.
pub fn caption_line(turn: &Turn, translation: Option<String>, ts: u64) -> CaptionLine {
    CaptionLine {
        text: turn.text.trim().to_string(),
        speaker_label: turn.speaker_id.clone(),
        translation: translation
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        ts,
    }
}

/// PURE translate DECISION: attempt a translation ONLY when a non-blank target language
/// is set. An empty/whitespace target => passthrough (no translation). Unit-testable
/// with no translator.
pub fn wants_translation(translate_to: &str) -> bool {
    !translate_to.trim().is_empty()
}

/// Translate one caption's text on the on-device Babel path via the injected
/// [`SegmentTranslator`], reusing the SAME faithful-translation prompt as interpret.rs.
/// Returns `Some(translation)` ONLY on a real, non-empty model reply; `None` when
/// translation is not wanted (empty target), the text is blank, the model is
/// unreachable/offline, or the reply is empty — an HONEST degrade, NEVER a fabricated
/// translation. Hermetic with a mock translator (no socket, no network).
async fn translate_text(
    translator: &dyn SegmentTranslator,
    text: &str,
    src_lang: Option<&str>,
    translate_to: &str,
) -> Option<String> {
    if !wants_translation(translate_to) {
        return None;
    }
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let prompt = build_segment_prompt(text, src_lang, translate_to.trim());
    match translator.translate(&prompt).await {
        Ok(reply) if !reply.trim().is_empty() => Some(reply.trim().to_string()),
        // Offline / model unreachable / empty reply: the caption STILL shows (passthrough)
        // — the local-model limit is honest, never a fabricated translation.
        _ => None,
    }
}

/// PURE seam: resolve the HONEST diarized turns for a transcript. Consume the EL-Scribe
/// per-word labels when present (distinct speakers appear iff Scribe distinguished them,
/// never fabricated); otherwise render the on-device SINGLE "unknown" stream (on-device
/// whisper has no diarization model). Reuses diarize.rs so captions labels speakers with
/// the SAME proven mapper — independent of the `[voice].diarize` flag (captions is its
/// own read-only feature).
pub fn turns_for(text: &str, scribe_words: &[ScribeWord]) -> Vec<Turn> {
    if scribe_words.is_empty() {
        crate::diarize::single_stream(text)
    } else {
        let resp = ScribeResponse {
            text: text.to_string(),
            words: scribe_words.to_vec(),
        };
        crate::diarize::diarize(&resp)
    }
}

/// Fire the `captions.line` telemetry frame the HUD's LIVE CAPTIONS band reads. The hub
/// already carries live user content (transcript/replies), so the caption text +
/// speaker + optional translation ride the wire; a `null` translation is an honest
/// passthrough/degrade. Fire-and-forget (dropped when no HUD).
fn emit_line(line: &CaptionLine) {
    telemetry::emit(
        "local",
        "captions.line",
        json!({
            "text": line.text,
            "speaker": line.speaker_label,
            "translation": line.translation,
            "ts": line.ts,
        }),
    );
}

/// PURE per-utterance caption ASSEMBLY: map diarized `turns` into [`CaptionLine`]s — one
/// per turn, each labeled with the turn's speaker (or "unknown" on the single stream),
/// each carrying an OPTIONAL on-device translation via the injected translator when
/// `translate_to` is set. A blank turn is skipped (never an empty caption). Emits one
/// `captions.line` telemetry frame per assembled line. Fully hermetic with a mock
/// translator (no socket, no mic, no network) — the assembly/label/translate-decision
/// logic is proven here headlessly; only the live socket wiring is device-gated.
pub async fn assemble_captions(
    translator: &dyn SegmentTranslator,
    turns: &[Turn],
    translate_to: &str,
    src_lang: Option<&str>,
    ts: u64,
) -> Vec<CaptionLine> {
    let mut lines = Vec::with_capacity(turns.len());
    for turn in turns {
        if turn.text.trim().is_empty() {
            continue;
        }
        let translation = translate_text(translator, &turn.text, src_lang, translate_to).await;
        let line = caption_line(turn, translation, ts);
        emit_line(&line);
        lines.push(line);
    }
    lines
}

/// Wall-clock epoch-ms for a freshly-assembled line. Falls back to 0 if the clock is
/// before the epoch (never panics).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// DEVICE-GATED live captions runner: gated by `[captions].enabled` at the transcript
/// site (`main.rs::run_pipeline`), it turns ONE freshly-transcribed utterance into
/// caption lines and emits them as `captions.line` telemetry for the HUD band. It reuses
/// the EXISTING transcript (the mic/TCC grant already produced it) — NO new recording to
/// disk, no extra audio leaves the device. Speaker labels come from diarize.rs (the
/// honest single "unknown" stream on-device, distinct speakers only when EL-Scribe
/// diarized); the optional translation (when `[captions].translate_to` is set) runs on
/// the SAME on-device Babel path as interpret, degrading HONESTLY offline (the line still
/// shows, translation omitted). READ-ONLY DISPLAY — it never classifies/routes/speaks.
///
/// With `[captions].enabled` OFF (the shipped default) this is never called from
/// run_pipeline. When ON but `translate_to` is empty (also shipped), no translation is
/// attempted so the inference socket is never touched — pure passthrough captions. NOT
/// itself hermetically tested (the translate arm touches the socket); the PURE
/// assembly/label/translate-decision logic is proven by [`assemble_captions`] + the
/// diarize.rs reuse.
#[allow(dead_code)] // live captions arm; the mic loop that drives it is device-gated
pub async fn emit_captions_live(
    text: &str,
    scribe_words: &[ScribeWord],
    cfg: &Config,
) -> Vec<CaptionLine> {
    let turns = turns_for(text, scribe_words);
    let src = {
        let s = cfg.captions.source_lang.trim();
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    };
    // On-device translation runs on the base model over the daemon inference socket. When
    // translate_to is empty (the shipped default) assemble_captions never calls it, so
    // the socket is never contacted — construction is just a PathBuf, no I/O.
    let translator = OnDeviceSegmentTranslator::over_inference_socket();
    assemble_captions(&translator, &turns, &cfg.captions.translate_to, src, now_ms()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::diarize::{single_stream, UNKNOWN_SPEAKER};
    use crate::interpret::TranslateFuture;

    /// A recording mock translator: returns a canned translation. No socket, no network.
    struct MockTranslator {
        reply: String,
    }
    impl SegmentTranslator for MockTranslator {
        fn translate<'a>(&'a self, _prompt: &'a str) -> TranslateFuture<'a> {
            let reply = self.reply.clone();
            Box::pin(async move { Ok(reply) })
        }
    }

    /// A translator that always fails — models the on-device model being unreachable.
    struct FailingTranslator;
    impl SegmentTranslator for FailingTranslator {
        fn translate<'a>(&'a self, _prompt: &'a str) -> TranslateFuture<'a> {
            Box::pin(async move { Err(anyhow::anyhow!("inference socket unreachable")) })
        }
    }

    /// A translator that must NEVER be called (asserts if it is) — proves passthrough
    /// never touches the model when no target language is set.
    struct NeverTranslator;
    impl SegmentTranslator for NeverTranslator {
        fn translate<'a>(&'a self, _prompt: &'a str) -> TranslateFuture<'a> {
            Box::pin(async move { panic!("translator must not be called on passthrough") })
        }
    }

    fn turn(speaker: &str, text: &str) -> Turn {
        Turn {
            speaker_id: speaker.to_string(),
            text: text.to_string(),
            start: None,
            end: None,
        }
    }

    #[test]
    fn caption_line_carries_the_speaker_label_verbatim_and_trims() {
        // The assembly seam: transcript turn -> caption line. The speaker label is carried
        // VERBATIM from diarize.rs; text/translation are trimmed; a blank translation folds
        // to None (never an empty translation).
        let line = caption_line(&turn("speaker_1", "  hello there  "), Some("  hola  ".into()), 42);
        assert_eq!(line.text, "hello there");
        assert_eq!(line.speaker_label, "speaker_1");
        assert_eq!(line.translation.as_deref(), Some("hola"));
        assert_eq!(line.ts, 42);

        let blank_tr = caption_line(&turn("speaker_0", "hi"), Some("   ".into()), 1);
        assert_eq!(blank_tr.translation, None, "a blank translation folds to None, never empty");
    }

    #[test]
    fn wants_translation_only_when_a_target_is_set() {
        assert!(wants_translation("Spanish"));
        assert!(wants_translation("  French  "));
        assert!(!wants_translation(""), "empty target => passthrough");
        assert!(!wants_translation("   "), "whitespace target => passthrough");
    }

    #[test]
    fn turns_for_falls_back_to_a_single_unknown_stream_on_device() {
        // On-device whisper: no per-word Scribe stream -> ONE honest "unknown" turn, never
        // a fabricated speaker.
        let turns = turns_for("hello world", &[]);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].speaker_id, UNKNOWN_SPEAKER);
        assert_eq!(turns[0].text, "hello world");
        // Blank transcript -> no turns (no empty caption).
        assert!(turns_for("   ", &[]).is_empty());
    }

    #[test]
    fn turns_for_consumes_real_scribe_labels_when_present() {
        // EL-Scribe carried per-word speaker ids: captions gets the REAL distinct turns via
        // the SAME diarize.rs mapper — distinct speakers iff Scribe distinguished them.
        let words = vec![
            ScribeWord { text: "hi".into(), speaker_id: Some("speaker_0".into()), start: Some(0.0), end: Some(0.2), kind: Some("word".into()) },
            ScribeWord { text: "hey".into(), speaker_id: Some("speaker_1".into()), start: Some(0.3), end: Some(0.5), kind: Some("word".into()) },
        ];
        let turns = turns_for("hi hey", &words);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].speaker_id, "speaker_0");
        assert_eq!(turns[1].speaker_id, "speaker_1");
    }

    #[tokio::test]
    async fn passthrough_when_no_target_language_never_touches_the_translator() {
        // Empty target => the single "unknown" stream renders as a passthrough caption
        // (translation None), and the translator is NEVER called (NeverTranslator asserts).
        let turns = single_stream("the meeting starts now");
        let lines = assemble_captions(&NeverTranslator, &turns, "", None, 100).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "the meeting starts now");
        assert_eq!(lines[0].speaker_label, UNKNOWN_SPEAKER);
        assert_eq!(lines[0].translation, None, "no target => passthrough, no translation");
        assert_eq!(lines[0].ts, 100);
    }

    #[tokio::test]
    async fn target_set_produces_a_translated_line_per_turn() {
        // A target language is set: each caption line carries the on-device translation.
        let mock = MockTranslator { reply: "hola".into() };
        let turns = vec![turn("speaker_0", "hello"), turn("speaker_1", "hi")];
        let lines = assemble_captions(&mock, &turns, "Spanish", Some("English"), 7).await;
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].speaker_label, "speaker_0");
        assert_eq!(lines[0].text, "hello");
        assert_eq!(lines[0].translation.as_deref(), Some("hola"));
        assert_eq!(lines[1].speaker_label, "speaker_1");
        assert_eq!(lines[1].translation.as_deref(), Some("hola"));
    }

    #[tokio::test]
    async fn offline_translator_degrades_honestly_line_still_shows_no_fabrication() {
        // The model is unreachable: the caption STILL shows the transcript text, but
        // translation is None — an honest degrade, never a fabricated translation.
        let turns = single_stream("bonjour tout le monde");
        let lines = assemble_captions(&FailingTranslator, &turns, "English", None, 3).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "bonjour tout le monde");
        assert_eq!(lines[0].speaker_label, UNKNOWN_SPEAKER);
        assert_eq!(lines[0].translation, None, "offline => no translation, never fabricated");
    }

    #[tokio::test]
    async fn empty_model_reply_degrades_never_fabricates() {
        // The model returned nothing usable: the line shows, translation stays None.
        let mock = MockTranslator { reply: "   ".into() };
        let turns = single_stream("hello");
        let lines = assemble_captions(&mock, &turns, "Spanish", None, 0).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].translation, None, "empty reply => passthrough, no fabricated translation");
    }

    #[tokio::test]
    async fn unseparated_audio_is_labeled_one_unknown_never_fabricated() {
        // The core honesty rail: when diarization cannot separate speakers, captions carry
        // exactly ONE "unknown" line for the whole utterance — never invented speakers.
        let turns = single_stream("everyone talking at once here");
        let lines = assemble_captions(&NeverTranslator, &turns, "", None, 5).await;
        assert_eq!(lines.len(), 1, "one honest stream, not fabricated speakers");
        assert_eq!(lines[0].speaker_label, UNKNOWN_SPEAKER);
    }

    #[tokio::test]
    async fn blank_turns_are_skipped_never_an_empty_caption() {
        let turns = vec![turn("speaker_0", "  "), turn("speaker_1", "real words")];
        let lines = assemble_captions(&NeverTranslator, &turns, "", None, 1).await;
        assert_eq!(lines.len(), 1, "the blank turn is dropped");
        assert_eq!(lines[0].text, "real words");
    }

    #[test]
    fn captions_ship_off_by_default() {
        // HERALD-EARS ships OFF (opt-in): no captions.line until enabled, and passthrough
        // (empty translate_to) until a target is set. source auto-detects.
        let cfg = Config::default();
        assert!(!cfg.captions.enabled, "live captions ship OFF (opt-in)");
        assert_eq!(cfg.captions.translate_to, "", "empty target => passthrough by default");
        assert_eq!(cfg.captions.source_lang, "", "empty source => auto-detect");
    }
}
