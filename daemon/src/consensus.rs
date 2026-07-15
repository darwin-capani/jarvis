//! ADVERSARIAL SECOND LOOK (F9) — an ADVISORY-ONLY critique of a consequential
//! action at the moment it parks for spoken confirmation.
//!
//! THE HARD SAFETY RULE: this module NEVER changes gate behavior. It cannot
//! approve, cannot deny, cannot re-rank policy, cannot touch the parked
//! `PendingConfirmation` — it only composes short spoken notes that ride WITH
//! the confirmation prompt (and a redacted telemetry event for the HUD), so
//! the user decides with more information. Every failure FAILS OPEN: a memory
//! read error or degenerate input simply produces no note; the park proceeds
//! exactly as it would without this module. (Philosophical siblings:
//! `[answers].cross_check`, which "NEVER removes or relaxes a confirmation
//! gate", and the undo arm's spoken "Heads up:" caveat.)
//!
//! WHAT IT LOOKS AT (all zero-fabrication, data the daemon already holds):
//!   * REVERSIBILITY — the undo journal's pure inverse derivation
//!     (journal::derive_inverse): if the action, once executed, will have no
//!     safe mechanical inverse, the user hears that BEFORE confirming, with
//!     the specific reason ("sent mail can't be unsent").
//!   * FIRST-TIME RECIPIENT — a bounded scan of the raw recent conversational
//!     record (memory::transcript_mentions, the one store that retains raw
//!     recipients). Honestly worded: it proves "not in my recent record",
//!     never "never contacted" (the window is bounded and records mentions,
//!     not deliveries).
//!   * TARGETED RISK NOTES — a physical-security note for `dume_control`
//!     unlock (mechanically reversible, but not risk-symmetric).
//!
//! SCOPE: the built-in consequential park site (the 19 gated tools + gated
//! skills). The MCP park site (opaque inputs), the standing-mission selector
//! (reversible by design), and the webhook park (null input) get no advisory
//! — a note derived from nothing would be fabrication.
//!
//! SPOKEN COMPOSITION: notes are PREPENDED to the returned prompt ("Second
//! look: … . <prompt>") so the say-'confirm' clause stays last and prominent;
//! `pending.preview` is never touched, keeping policy recipient-matching, the
//! audit target, the pending listing, and the journal byte-identical.

use serde_json::Value;

/// At most this many notes ride a single confirmation — the second look is a
/// glance, not a lecture.
const MAX_NOTES: usize = 2;
/// Recipient lookups per action (an attendee list can be long; the advisory
/// only needs the first few to be meaningful).
const MAX_RECIPIENT_LOOKUPS: usize = 3;
/// Bound on each note as emitted to telemetry (chars, post-redaction).
const NOTE_WIRE_CHARS: usize = 200;

/// Assemble the advisory notes for a consequential action about to park.
/// ADVISORY-ONLY and FAIL-OPEN: any error path yields fewer notes, never a
/// blocked or altered park. Bounded to [`MAX_NOTES`].
pub async fn second_look(tool: &str, input: &Value, memory: &crate::memory::Memory) -> Vec<String> {
    let mut notes = static_advisories(tool, input);

    for recipient in extract_recipients(tool, input).into_iter().take(MAX_RECIPIENT_LOOKUPS) {
        // Ok(false) = genuinely not in the bounded recent record -> advisory.
        // Ok(true) (seen) and Err (read failed) both stay silent: fail-open,
        // a note is never fabricated from a failed lookup.
        if let Ok(false) = memory.transcript_mentions(&recipient).await {
            notes.push(format!(
                "'{}' isn't in my recent record — that looks like a first-time recipient as far as I can recall",
                bound_recipient(&recipient)
            ));
            break; // one recipient note is enough for a glance
        }
    }

    notes.truncate(MAX_NOTES);
    notes
}

/// The PURE advisories derivable from (tool, input) alone — unit-tested
/// exhaustively. Reuses the undo journal's inverse derivation so the
/// reversibility verdict here can never disagree with what "undo that" will
/// later say.
pub fn static_advisories(tool: &str, input: &Value) -> Vec<String> {
    let mut notes = Vec::new();

    // Physical-security note: unlocking is mechanically reversible (lock) but
    // not risk-symmetric — say so before the confirm, not after.
    let dume_action = input.get("action").and_then(Value::as_str).unwrap_or("");
    if tool == "dume_control" && dume_action == "unlock" {
        notes.push("this unlocks a physical door".to_string());
    }

    // Reversibility: if the executed action will have no safe inverse, the
    // user hears why BEFORE confirming. Special cases where the undo-framed
    // verdict would mislead pre-execution:
    //   * standing_create is reversible by design (standing_cancel, ungated) —
    //     its inverse merely needs the id from the SUCCESS outcome, which
    //     doesn't exist yet. No note.
    //   * dume_control "lock" derives None only because undo refuses to arm an
    //     unlock — locking itself is safe and user-reversible. No note.
    let skip_reversibility =
        tool == "standing_create" || (tool == "dume_control" && dume_action == "lock");
    if !skip_reversibility {
        if let crate::journal::Inverse::None { why } = crate::journal::derive_inverse(tool, input, "")
        {
            notes.push(format!("once done, I can't undo this — {why}"));
        }
    }

    notes
}

/// Extract the recipient/target strings worth a first-time check. PURE.
/// gmail_send `to` (comma-separated, bare or "Name <addr>" forms),
/// gcal_create_event `attendees` (array), slack_post_message `channel`.
/// Anything else: none — a check against a made-up field would be fabrication.
pub fn extract_recipients(tool: &str, input: &Value) -> Vec<String> {
    match tool {
        "gmail_send" => input
            .get("to")
            .and_then(Value::as_str)
            .map(|to| to.split(',').filter_map(extract_address).collect())
            .unwrap_or_default(),
        "gcal_create_event" => input
            .get("attendees")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter_map(extract_address)
                    .collect()
            })
            .unwrap_or_default(),
        "slack_post_message" => input
            .get("channel")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| vec![c.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The longest recipient the spoken note (and the memory lookup) will carry —
/// a model-supplied `to` can be arbitrarily long, and it is interpolated into
/// speech, so it is bounded like every other wire string.
const MAX_RECIPIENT_CHARS: usize = 120;

/// Pull the address out of one recipient token: "Name <a@b.c>" -> "a@b.c",
/// "Name <a@b.c" (unclosed) -> "a@b.c", a bare "a@b.c" passes through. A token
/// with no '@' is not an address.
fn extract_address(token: &str) -> Option<String> {
    let t = token.trim();
    // Everything after the LAST '<' (up to a closing '>' if present) is the
    // address; an unclosed bracket still yields the address, not the display
    // name (which would spuriously widen the memory lookup).
    let addr = match t.rfind('<') {
        Some(open) => {
            let rest = &t[open + 1..];
            rest.split('>').next().unwrap_or(rest).trim()
        }
        None => t,
    };
    (addr.contains('@') && addr.len() >= 3).then(|| addr.to_string())
}

/// Bound a recipient string for spoken output (chars, with an ellipsis).
fn bound_recipient(recipient: &str) -> String {
    if recipient.chars().count() <= MAX_RECIPIENT_CHARS {
        recipient.to_string()
    } else {
        let mut out: String = recipient.chars().take(MAX_RECIPIENT_CHARS).collect();
        out.push('…');
        out
    }
}

/// Compose the spoken prompt: notes PREPENDED so the say-'confirm' clause
/// stays last and prominent (the undo arm's "Heads up:" precedent). With no
/// notes the prompt passes through byte-identical. PURE.
pub fn compose_prompt(prompt: &str, notes: &[String]) -> String {
    if notes.is_empty() {
        prompt.to_string()
    } else {
        format!("Second look: {}. {prompt}", notes.join("; "))
    }
}

/// The notes as emitted on the `consensus.advisory` telemetry event: each
/// REDACTED (the spoken note may name a raw recipient — speech is ephemeral,
/// but the wire follows the audit log's redaction discipline) then bounded.
pub fn wire_notes(notes: &[String]) -> Vec<String> {
    notes
        .iter()
        .map(|n| {
            let redacted = crate::optimize::redact(n);
            if redacted.chars().count() <= NOTE_WIRE_CHARS {
                redacted
            } else {
                let mut out: String = redacted.chars().take(NOTE_WIRE_CHARS).collect();
                out.push('…');
                out
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests — the pure parts exhaustively; the memory-backed path via TempDb.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- static advisories -----------------------------------------------------

    #[test]
    fn irreversible_actions_get_the_honest_cant_undo_note() {
        let notes = static_advisories("gmail_send", &json!({"to": "a@b.co"}));
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("can't undo"), "{notes:?}");
        assert!(notes[0].contains("unsent"), "the specific reason rides along: {notes:?}");

        let notes = static_advisories("gads_set_budget", &json!({"budget_id": "b", "amount": 5}));
        assert!(notes[0].contains("previous budget"), "{notes:?}");
    }

    #[test]
    fn reversible_actions_get_no_reversibility_note() {
        for (tool, input) in [
            ("gads_pause_campaign", json!({"campaign_id": "1"})),
            ("gads_enable_campaign", json!({"campaign_id": "1"})),
            ("meta_pause_campaign", json!({"campaign_id": "1"})),
            ("dume_control", json!({"entity_id": "light.x", "action": "turn_on"})),
        ] {
            assert!(
                static_advisories(tool, &input).is_empty(),
                "{tool} is reversible — a note would be noise"
            );
        }
    }

    /// The undo-framed special cases must not leak misleading pre-execution
    /// notes: standing_create IS reversible by design, and dume "lock" is only
    /// None because undo refuses to arm an unlock.
    #[test]
    fn undo_framed_verdicts_are_special_cased_not_parroted() {
        assert!(static_advisories(
            "standing_create",
            &json!({"goal": "g", "schedule": "daily"})
        )
        .is_empty());
        assert!(static_advisories(
            "dume_control",
            &json!({"entity_id": "lock.door", "action": "lock"})
        )
        .is_empty());
    }

    #[test]
    fn unlock_carries_the_physical_security_note() {
        let notes =
            static_advisories("dume_control", &json!({"entity_id": "lock.door", "action": "unlock"}));
        assert_eq!(notes.len(), 1, "unlock is Ready (re-lockable), so ONLY the door note: {notes:?}");
        assert!(notes[0].contains("physical door"));
    }

    // -- recipient extraction ---------------------------------------------------

    #[test]
    fn recipients_extract_from_bare_named_and_list_forms() {
        assert_eq!(
            extract_recipients("gmail_send", &json!({"to": "a@b.co"})),
            vec!["a@b.co"]
        );
        assert_eq!(
            extract_recipients("gmail_send", &json!({"to": "Bob <bob@x.io>, carol@y.z "})),
            vec!["bob@x.io", "carol@y.z"]
        );
        assert_eq!(
            extract_recipients("gcal_create_event", &json!({"attendees": ["Dee <d@e.fm>", "nope"]})),
            vec!["d@e.fm"]
        );
        assert_eq!(
            extract_recipients("slack_post_message", &json!({"channel": " #ops "})),
            vec!["#ops"]
        );
        // An UNCLOSED angle bracket still yields the address, not the display
        // name (which would spuriously widen the lookup to a known contact).
        assert_eq!(
            extract_recipients("gmail_send", &json!({"to": "Carol <carol@y.z"})),
            vec!["carol@y.z"]
        );
        // No fabricated fields for other tools; malformed inputs degrade empty.
        assert!(extract_recipients("shell_run", &json!({"command": "ls"})).is_empty());
        assert!(extract_recipients("gmail_send", &json!({})).is_empty());
        assert!(extract_recipients("gmail_send", &json!({"to": "not-an-address"})).is_empty());
    }

    #[test]
    fn a_pathologically_long_recipient_is_bounded_in_the_spoken_note() {
        let long = format!("{}@evil.tld", "a".repeat(500));
        assert!(bound_recipient(&long).chars().count() <= MAX_RECIPIENT_CHARS + 1);
        assert!(bound_recipient(&long).ends_with('…'));
        assert_eq!(bound_recipient("bob@x.io"), "bob@x.io");
    }

    // -- composition --------------------------------------------------------------

    #[test]
    fn compose_prepends_and_never_buries_the_confirm_clause() {
        let prompt = "I'd send an email to a@b.co — say 'confirm' to proceed or 'cancel' to drop it.";
        // No notes: byte-identical pass-through.
        assert_eq!(compose_prompt(prompt, &[]), prompt);
        let composed = compose_prompt(prompt, &["once done, I can't undo this — sent mail can't be unsent".to_string()]);
        assert!(composed.starts_with("Second look: once done"));
        assert!(composed.ends_with("or 'cancel' to drop it."), "confirm clause stays last");
    }

    #[test]
    fn wire_notes_are_redacted_and_bounded() {
        let notes = vec![
            "'alice@example.com' isn't in my recent record — first-time recipient".to_string(),
            format!("x{}", "y".repeat(500)),
        ];
        let wire = wire_notes(&notes);
        assert!(!wire[0].contains("alice@example.com"), "emails masked on the wire: {}", wire[0]);
        assert!(wire[0].contains("[redacted]"));
        assert!(wire[1].chars().count() <= 201);
        assert!(wire[1].ends_with('…'));
    }

    // -- the memory-backed second look (hermetic TempDb) --------------------------

    struct TempDb(std::path::PathBuf);
    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            for suffix in ["-wal", "-shm"] {
                let mut p = self.0.as_os_str().to_owned();
                p.push(suffix);
                let _ = std::fs::remove_file(std::path::PathBuf::from(p));
            }
        }
    }
    fn mem(tag: &str) -> (crate::memory::Memory, TempDb) {
        let path = std::env::temp_dir()
            .join(format!("darwin-consensus-test-{}-{tag}.db", std::process::id()));
        let db = TempDb(path.clone());
        (crate::memory::Memory::open(&path).unwrap(), db)
    }

    #[tokio::test]
    async fn second_look_flags_a_first_time_recipient_and_stays_silent_for_known_ones() {
        let (memory, _db) = mem("recipient");
        // A recipient the user has spoken about before (case-insensitive).
        memory
            .record_transcript(None, "email Bob@X.io about the launch", "intent", "local", Some("Done."))
            .await
            .unwrap();

        // Known recipient: only the irreversibility note.
        let notes = second_look("gmail_send", &serde_json::json!({"to": "bob@x.io"}), &memory).await;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("can't undo"));

        // Unseen recipient: irreversibility + first-time, bounded at MAX_NOTES.
        let notes =
            second_look("gmail_send", &serde_json::json!({"to": "stranger@new.tld"}), &memory).await;
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(notes[1].contains("stranger@new.tld"));
        assert!(notes[1].contains("recent record"), "honest bounded wording: {notes:?}");
    }

    #[tokio::test]
    async fn second_look_is_silent_for_reversible_actions_with_no_recipients() {
        let (memory, _db) = mem("silent");
        let notes =
            second_look("gads_pause_campaign", &serde_json::json!({"campaign_id": "7"}), &memory)
                .await;
        assert!(notes.is_empty(), "no note, no prompt change: {notes:?}");
    }
}
