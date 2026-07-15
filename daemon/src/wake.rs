//! CUSTOM WAKE-WORD (#32) — the PURE, conservative wake-phrase matcher.
//!
//! This module answers ONE question, with no I/O, no globals, and no mic: does a
//! transcript contain the configured wake phrase (e.g. "darwin", or a user-chosen
//! "computer", "hey edith")? It is the gate the always-listening activation path
//! consults to decide "is this utterance for DARWIN" — but the always-listening
//! mic loop itself is DEVICE-GATED; only this pure matcher is proven headlessly.
//!
//! ## Posture (ON by default; the default phrase preserves today's behavior)
//!
//! `[wake].enabled` SHIPS ON (full-power default): since `[wake].phrase` defaults to
//! "darwin", activation is byte-for-byte today's unless the phrase is changed (the
//! always-listening loop that consults the matcher is DEVICE-GATED on mic/TCC). With
//! it false the matcher is never consulted. [`wake_gate`] folds the switch in so a caller can pass the
//! config straight through.
//!
//! ## Conservatism (the whole point — never a false wake)
//!
//! [`wake_match`] is deliberately strict so an ambient transcript never spuriously
//! activates DARWIN:
//!   * case / punctuation / whitespace-insensitive (normalized to lowercase
//!     alphanumeric tokens), so "Darwin,", "DARWIN" and "darwin" all match;
//!   * the phrase must appear as a CONTIGUOUS run of whole tokens — it NEVER matches
//!     on a substring of a larger unrelated word ("dar win" or "darwinated" do not
//!     match "darwin"); the token boundary is the protection;
//!   * a SMALL per-token edit-distance tolerance (Levenshtein <= 1 for tokens long
//!     enough to afford it) absorbs a one-character STT slip ("darvin" for "darwin"),
//!     but never collapses two genuinely different short words;
//!   * an EMPTY / blank phrase NEVER matches (fail-safe: a misconfigured empty phrase
//!     cannot turn every utterance into a wake).
//!
//! HONESTY: this is a lexical matcher over an already-produced transcript, NOT a
//! trained acoustic keyword spotter. It never fabricates a wake — a match means the
//! configured phrase's tokens really appear (modulo the bounded fuzz) in the text.

use crate::config::Config;

/// The longest token (in chars) for which a one-edit fuzzy match is allowed BELOW the
/// usual length floor would still be safe — but we instead gate fuzz on a minimum
/// token length so short words ("hi", "go") are matched EXACTLY. A 1-edit tolerance on
/// a 2-char token would conflate too many distinct words.
const FUZZY_MIN_TOKEN_LEN: usize = 4;

/// Normalize text into lowercase alphanumeric tokens, dropping all punctuation and
/// collapsing whitespace. This is what makes the match case/punct/whitespace-
/// insensitive: "Darwin, are you there?" -> ["darwin", "are", "you", "there"].
fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect()
}

/// Levenshtein edit distance between two token strings, used ONLY to absorb a single
/// STT slip on a sufficiently long token. Bounded and allocation-light (two rolling
/// rows); the inputs are single words, so this is cheap.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Whether a transcript TOKEN matches a PHRASE TOKEN: exactly, or within ONE edit when
/// the phrase token is long enough to afford the tolerance ([`FUZZY_MIN_TOKEN_LEN`]).
/// Short phrase tokens are matched EXACTLY so a one-edit fuzz never conflates two
/// distinct short words.
fn token_matches(transcript_tok: &str, phrase_tok: &str) -> bool {
    if transcript_tok == phrase_tok {
        return true;
    }
    if phrase_tok.chars().count() < FUZZY_MIN_TOKEN_LEN {
        return false;
    }
    edit_distance(transcript_tok, phrase_tok) <= 1
}

/// PURE wake matcher: does `transcript` contain `phrase` as a contiguous run of whole
/// tokens (case/punct/whitespace-insensitive, with a small per-token edit tolerance on
/// long tokens)?
///
/// Conservative by design — see the module docs:
///   * an EMPTY / blank phrase returns false (a misconfigured empty phrase can never
///     wake on every utterance);
///   * the phrase tokens must appear as a CONTIGUOUS token subsequence, so a substring
///     of a larger unrelated word ("darwinated", "dar win") never matches "darwin";
///   * each token may differ by at most one edit, and only when the phrase token is at
///     least [`FUZZY_MIN_TOKEN_LEN`] chars (short tokens are exact).
///
/// No I/O, no globals, no mic — fully unit-testable.
pub fn wake_match(transcript: &str, phrase: &str) -> bool {
    let phrase_toks = tokenize(phrase);
    // Fail-safe: an empty/blank phrase (no tokens) never matches.
    if phrase_toks.is_empty() {
        return false;
    }
    let text_toks = tokenize(transcript);
    if text_toks.len() < phrase_toks.len() {
        return false;
    }
    // Slide the phrase over the transcript token stream; a contiguous run of token
    // matches anywhere is a wake.
    for window in text_toks.windows(phrase_toks.len()) {
        if window
            .iter()
            .zip(phrase_toks.iter())
            .all(|(t, p)| token_matches(t, p))
        {
            return true;
        }
    }
    false
}

/// The activation gate the live (device-gated) listening loop consults, folding the
/// `[wake].enabled` switch in: when the switch is OFF this returns true unconditionally
/// (activation is byte-for-byte today's — the matcher gates nothing), and when ON it
/// returns whether the configured phrase matches the transcript. Default phrase
/// ("darwin") preserves today's wake behavior when the feature is turned on.
///
/// PURE: a function of the config + the transcript, no I/O. Wired at the activation
/// site (see audio.rs / router.rs); the always-listening loop that produces the
/// transcript is DEVICE-GATED.
pub fn wake_gate(cfg: &Config, transcript: &str) -> bool {
    if !cfg.wake.enabled {
        // Switch off (the shipped default): the matcher gates nothing.
        return true;
    }
    wake_match(transcript, &cfg.wake.phrase)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn matches_the_exact_phrase_case_and_punct_insensitive() {
        assert!(wake_match("Darwin, are you there?", "darwin"));
        assert!(wake_match("DARWIN", "darwin"));
        assert!(wake_match("hey darwin what's the time", "darwin"));
        // A multi-word phrase, contiguous run of tokens, mixed case + punctuation.
        assert!(wake_match("Hey, EDITH — open the door", "hey edith"));
        assert!(wake_match("computer, lights on", "computer"));
    }

    #[test]
    fn absorbs_a_single_stt_slip_on_a_long_token() {
        // One-character substitution / deletion on a long token (>= FUZZY_MIN_TOKEN_LEN)
        // is tolerated — a realistic STT mishearing of the wake word.
        assert!(wake_match("darvin turn on the lights", "darwin"), "1-sub slip");
        assert!(wake_match("darwi are you up", "darwin"), "1-del slip");
        assert!(wake_match("darwins status", "darwin"), "1-ins slip");
    }

    #[test]
    fn rejects_unrelated_transcripts() {
        assert!(!wake_match("turn on the kitchen lights", "darwin"));
        assert!(!wake_match("what is the weather today", "darwin"));
        // A genuinely different word several edits away never matches.
        assert!(!wake_match("the harvest was good", "darwin"));
    }

    #[test]
    fn never_matches_a_substring_of_a_larger_word_or_split_tokens() {
        // Substring of a larger unrelated word: the token boundary protects us.
        assert!(!wake_match("the darwinated report is ready", "darwin"));
        // The phrase split across two tokens must NOT match (each token compared whole).
        assert!(!wake_match("say dar win to me slowly", "darwin"));
    }

    #[test]
    fn empty_or_blank_phrase_never_matches() {
        assert!(!wake_match("anything at all", ""));
        assert!(!wake_match("anything at all", "   "));
        assert!(!wake_match("darwin", ",.!"), "punctuation-only phrase has no tokens");
        // And an empty transcript never matches a real phrase.
        assert!(!wake_match("", "darwin"));
    }

    #[test]
    fn short_phrase_tokens_are_matched_exactly_no_fuzz() {
        // A 2-char phrase token must match exactly — a 1-edit fuzz would conflate too
        // many distinct short words.
        assert!(wake_match("go now", "go"));
        assert!(!wake_match("so now", "go"), "short token must not fuzzy-match");
        assert!(!wake_match("do it", "go"));
    }

    #[test]
    fn multi_word_phrase_requires_contiguous_run() {
        // The two phrase tokens must be adjacent and in order.
        assert!(wake_match("ok hey edith", "hey edith"));
        assert!(!wake_match("edith, hey there", "hey edith"), "out of order");
        assert!(!wake_match("hey there edith", "hey edith"), "not contiguous");
    }

    #[test]
    fn gate_ships_on_by_default_with_phrase_darwin_and_off_path_opens_the_gate() {
        // Shipped default: wake.enabled = true with phrase "darwin" -> activation
        // requires the "darwin" wake word, which IS today's behavior (the phrase
        // default preserves it). Turning the switch OFF opens the gate regardless of
        // text (the off path still exists).
        let cfg = Config::default();
        assert!(cfg.wake.enabled, "wake gating ships ON (full-power default)");
        assert_eq!(cfg.wake.phrase, "darwin", "default phrase preserves today's behavior");
        assert!(wake_gate(&cfg, "darwin status report"), "default phrase present -> gate open");
        assert!(!wake_gate(&cfg, "status report"), "default phrase absent -> gate closed");

        // Explicitly OFF: the gate is OPEN regardless of text.
        let mut off = Config::default();
        off.wake.enabled = false;
        assert!(wake_gate(&off, "turn on the lights"), "off => gate open");
        assert!(wake_gate(&off, ""), "off => gate open even for empty text");
    }

    #[test]
    fn gate_enforces_the_phrase_when_enabled() {
        let mut cfg = Config::default();
        cfg.wake.enabled = true;
        // Default phrase still "darwin": activation requires the wake word now.
        assert!(wake_gate(&cfg, "darwin status report"), "phrase present -> gate open");
        assert!(!wake_gate(&cfg, "status report"), "phrase absent -> gate closed");
        // A custom phrase gates on that phrase instead.
        cfg.wake.phrase = "computer".to_string();
        assert!(wake_gate(&cfg, "Computer, lights"));
        assert!(!wake_gate(&cfg, "darwin are you there"), "old phrase no longer wakes");
        // An empty configured phrase, even with the switch on, never opens the gate
        // (fail-safe) — a misconfigured empty phrase can't wake on everything.
        cfg.wake.phrase = "  ".to_string();
        assert!(!wake_gate(&cfg, "anything"), "blank phrase + enabled => never wakes");
    }

    #[test]
    fn edit_distance_is_correct_for_the_fuzz_boundary() {
        assert_eq!(edit_distance("darwin", "darwin"), 0);
        assert_eq!(edit_distance("darwin", "darvin"), 1);
        assert_eq!(edit_distance("darwin", "darwi"), 1);
        assert_eq!(edit_distance("darwin", "marvin"), 2);
        assert_eq!(edit_distance("", "abc"), 3);
    }
}
