//! AUTO-DRAFT (#25): compose a REVIEWABLE pending draft the user reads and then
//! sends THEMSELVES through the existing gated send.
//!
//! A DRAFT is a *suggestion*: [`draft`] turns a `(kind, context)` into a
//! [`PendingDraft`] — a subject/preview/body the user can read — persisted with
//! `status = "draft"` and NOTHING ELSE. This module has, by construction, NO SEND
//! PATH:
//!
//! - There is no function here that posts, emails, uploads, or otherwise performs a
//!   side effect. A draft is composed, persisted, listed, and forgotten — full stop.
//! - An actual SEND is a SEPARATE, EXPLICIT action that rides the EXISTING
//!   consequential gate (`integrations::gate(confirm)` == Execute iff
//!   `consequential_allowed()` && a fresh confirm) exactly like a normal
//!   `gmail_send` / `slack_post_message`. Turning [drafts] ON only enables DARWIN
//!   to *compose* drafts proactively; it can never cause an autonomous send.
//! - A persisted draft NEVER carries a pre-approval. It is inert text. To send it,
//!   the user issues the normal gated send tool with the draft's content, which
//!   parks for a spoken yes (master ON) or only previews (master OFF, the default).
//!
//! HONESTY: DARWIN never sends a draft on its own. A draft is the help; the human
//! presses send. The module persists drafts to the SAME `meta.*` fact store the
//! standing-mission records use (in-RAM/temp SQLite in tests), so nothing here
//! touches a real network or a real send.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::memory::Memory;

/// Default evict-oldest cap on persisted pending drafts (the bounded store).
pub const DEFAULT_RETENTION: usize = 200;

/// Reserved `meta.*` key prefix the pending-draft rows live under. `meta.*` keys
/// are internal bookkeeping (filtered from every agent prompt feed + the world
/// model), so a draft never leaks into a model context the way a user fact would.
const DRAFT_PREFIX: &str = "meta.draft.";

/// Bound on a persisted draft body so one row can never grow unbounded.
const MAX_BODY_LEN: usize = 8_000;
/// Bound on the subject/preview lines.
const MAX_SUBJECT_LEN: usize = 200;

/// The KIND of thing a draft targets. This is purely descriptive — it records what
/// surface the user will eventually send through. It is NOT a send path: choosing
/// a kind composes text, it never dispatches anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftKind {
    /// A reply to an email (the user sends it later via the gated `gmail_send`).
    EmailReply,
    /// A chat/DM message (sent later via the gated `slack_post_message` / `x_post`).
    Message,
    /// A document body (uploaded later via the gated `gdrive_upload_text`).
    Doc,
}

impl DraftKind {
    /// Stable lowercase token for storage + the HUD.
    pub fn as_str(&self) -> &'static str {
        match self {
            DraftKind::EmailReply => "email_reply",
            DraftKind::Message => "message",
            DraftKind::Doc => "doc",
        }
    }

    /// Parse a kind token (the tool surface passes a string). Unknown -> Message
    /// (the most generic surface) so a bad hint never errors out a draft.
    pub fn parse(s: &str) -> DraftKind {
        match s.trim().to_lowercase().as_str() {
            "email_reply" | "email" | "reply" => DraftKind::EmailReply,
            "doc" | "document" => DraftKind::Doc,
            _ => DraftKind::Message,
        }
    }
}

/// The ONLY status a draft this module produces ever has. There is deliberately no
/// `Sent` variant here: this module cannot send, so it can never set one. (A future
/// send surface lives elsewhere, behind the gate, and would track its own state.)
pub const STATUS_DRAFT: &str = "draft";

/// A reviewable PENDING draft — a suggestion the user reads and then sends
/// themselves. Persisted as one JSON fact row under `meta.draft.<id>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDraft {
    /// Stable content id (short hex), the addressing label for forget.
    pub id: String,
    /// What surface the user will eventually send through (descriptive only).
    pub kind: DraftKind,
    /// A short subject / summary line for the review list.
    pub subject: String,
    /// A one-line preview for the review list.
    pub preview: String,
    /// The full draft body the user reviews (and copies into the gated send).
    pub body: String,
    /// ALWAYS [`STATUS_DRAFT`]. The module has no send path, so this never changes
    /// to "sent" here — proof, in the persisted shape, that a draft is never sent
    /// by DARWIN.
    pub status: String,
    /// RFC3339 creation timestamp.
    pub created: String,
}

impl PendingDraft {
    /// Compose a new pending draft from a kind + the model-built body, deriving a
    /// stable content id, bounding the fields, and stamping `status = "draft"`.
    /// Pure (no I/O) — [`draft`] persists it. There is no parameter and no field
    /// that could mark it sent.
    pub fn compose(kind: DraftKind, subject: &str, preview: &str, body: &str) -> PendingDraft {
        let subject = bound(subject, MAX_SUBJECT_LEN);
        let preview = bound(preview, MAX_SUBJECT_LEN);
        let body = bound(body, MAX_BODY_LEN);
        let id = derive_id(kind, &subject, &body);
        PendingDraft {
            id,
            kind,
            subject,
            preview,
            body,
            // Pinned: a draft this module makes is ALWAYS just a draft.
            status: STATUS_DRAFT.to_string(),
            created: chrono::Utc::now().to_rfc3339(),
        }
    }
}

/// Trim + length-bound a field for persistence. Pure.
fn bound(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Derive a stable content id for a draft: a short hex prefix of SHA-256 over
/// `kind || NUL || subject || NUL || body`. Two identical drafts hash the same;
/// `forget` can address a draft by an id reproducible from its content. Pure.
pub fn derive_id(kind: DraftKind, subject: &str, body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(kind.as_str().as_bytes());
    h.update([0u8]);
    h.update(subject.trim().as_bytes());
    h.update([0u8]);
    h.update(body.trim().as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..6]) // 12 hex chars (48 bits)
}

/// Render the reviewable preview line a user (or the HUD) sees for a draft. Names
/// the kind + subject + preview and is EXPLICIT that nothing was sent — the user
/// reviews it and sends it themselves through the gated send. Pure.
pub fn review_line(d: &PendingDraft) -> String {
    format!(
        "Draft ready ({}) — \"{}\": {} [{}] Review it and send it yourself; I won't send a draft for you.",
        d.kind.as_str(),
        d.subject,
        d.preview,
        d.id,
    )
}

// ---------------------------------------------------------------------------
// The store (persisted via Memory; round-trips against a temp/in-RAM DB in tests)
// ---------------------------------------------------------------------------

/// Compose a draft from `(kind, subject, preview, body)` and PERSIST it as a
/// pending draft (`status = "draft"`). Returns the stored [`PendingDraft`]. This is
/// the ONLY way a draft enters the store, and it does exactly one thing: write a
/// reviewable suggestion. It performs NO send and touches NO integration. Enforces
/// the evict-oldest retention cap so the store stays bounded.
pub async fn draft(
    memory: &Memory,
    retention: usize,
    kind: DraftKind,
    subject: &str,
    preview: &str,
    body: &str,
) -> Result<PendingDraft> {
    let pending = PendingDraft::compose(kind, subject, preview, body);
    save(memory, &pending).await?;
    enforce_retention(memory, retention.max(1)).await?;
    Ok(pending)
}

/// Persist a pending draft as a single JSON fact row under `meta.draft.<id>`.
/// Trusted internal write (`upsert_fact`, never the model-driven `upsert_user_fact`
/// which rejects the reserved `meta.*` prefix). Idempotent on id.
async fn save(memory: &Memory, d: &PendingDraft) -> Result<()> {
    let key = format!("{DRAFT_PREFIX}{}", d.id);
    let json = serde_json::to_string(d)?;
    memory.upsert_fact(&key, &json).await
}

/// Every persisted pending draft, newest first. Malformed rows are skipped, never
/// panic. Bounded by the store scan; the retention cap keeps the real count small.
pub async fn list(memory: &Memory) -> Result<Vec<PendingDraft>> {
    let rows = memory.recall_facts_limited(DRAFT_PREFIX, 512).await?;
    Ok(rows
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_str::<PendingDraft>(&v).ok())
        .collect())
}

/// Forget (delete) the pending draft with `id`. Returns whether a row existed. This
/// is the only mutation besides compose — a draft is composed, reviewed, then
/// forgotten; it is never "sent" from here.
pub async fn forget(memory: &Memory, id: &str) -> Result<bool> {
    let key = format!("{DRAFT_PREFIX}{}", id.trim());
    memory.delete_fact(&key).await
}

/// Evict the oldest drafts past `keep` so the store stays bounded. The fact rows
/// carry a ts; `recall_facts_limited` returns newest-first, so the tail past `keep`
/// is the oldest — delete those. Synchronous-ish over the async store.
async fn enforce_retention(memory: &Memory, keep: usize) -> Result<()> {
    let rows = memory.recall_facts_limited(DRAFT_PREFIX, 1024).await?;
    if rows.len() <= keep {
        return Ok(());
    }
    for (key, _) in rows.into_iter().skip(keep) {
        let _ = memory.delete_fact(&key).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempDb(PathBuf);
    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-drafts-test-{}-{}.db",
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

    fn mem(tag: &str) -> (Memory, TempDb) {
        let db = TempDb::new(tag);
        let m = Memory::open(&db.0).unwrap();
        (m, db)
    }

    // ---- compose produces a PENDING draft, status=draft --------------------

    #[test]
    fn compose_marks_status_draft_and_never_sent() {
        let d = PendingDraft::compose(
            DraftKind::EmailReply,
            "Re: launch timing",
            "Confirming Thursday works.",
            "Hi Sam,\n\nThursday works on my end. Let's lock it in.\n\nBest,",
        );
        assert_eq!(d.status, STATUS_DRAFT, "a draft is ALWAYS status=draft");
        assert_eq!(d.kind, DraftKind::EmailReply);
        assert!(d.body.contains("Thursday works"));
        // There is no API on the struct or module to mark it sent.
    }

    #[tokio::test]
    async fn draft_persists_a_pending_draft_and_roundtrips() {
        let (m, _db) = mem("roundtrip");
        let d = draft(
            &m,
            DEFAULT_RETENTION,
            DraftKind::Message,
            "ping",
            "asking about the deck",
            "Hey — do you have the deck ready?",
        )
        .await
        .unwrap();
        assert_eq!(d.status, STATUS_DRAFT);

        let listed = list(&m).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, d.id);
        assert_eq!(listed[0].status, STATUS_DRAFT, "persisted as a draft, never sent");
        assert!(listed[0].body.contains("deck ready"));

        // Forget removes it; a second forget is a no-op.
        assert!(forget(&m, &d.id).await.unwrap());
        assert!(!forget(&m, &d.id).await.unwrap());
        assert!(list(&m).await.unwrap().is_empty());
    }

    // ---- the module has NO send path (proven by its surface) ---------------

    #[tokio::test]
    async fn module_has_no_send_path_only_review_and_forget() {
        // The CRUX of #25: composing/persisting a draft performs NO side effect and
        // never flips a draft to "sent". The only mutations are compose + forget;
        // there is no function here that sends. We prove the persisted state can
        // only ever be a draft, and the review line says so explicitly.
        let (m, _db) = mem("nosend");
        let d = draft(
            &m,
            DEFAULT_RETENTION,
            DraftKind::EmailReply,
            "Re: invoice",
            "Will pay by Friday.",
            "I'll have this paid by Friday — thanks for the nudge.",
        )
        .await
        .unwrap();

        // Every stored draft is status=draft — nothing in this module can make it
        // "sent" (a send is a SEPARATE gated action elsewhere).
        for stored in list(&m).await.unwrap() {
            assert_eq!(stored.status, STATUS_DRAFT, "no draft is ever sent by this module");
            assert_ne!(stored.status, "sent");
        }

        let line = review_line(&d);
        assert!(
            line.to_lowercase().contains("send it yourself")
                && line.to_lowercase().contains("won't send"),
            "the review line is explicit that DARWIN won't send the draft: {line}"
        );
    }

    // ---- retention bounds the store ----------------------------------------

    #[tokio::test]
    async fn retention_evicts_oldest_drafts() {
        let (m, _db) = mem("retention");
        for i in 0..5 {
            draft(
                &m,
                3, // keep only 3
                DraftKind::Message,
                &format!("subject {i}"),
                "preview",
                &format!("body number {i}"),
            )
            .await
            .unwrap();
        }
        let listed = list(&m).await.unwrap();
        assert!(listed.len() <= 3, "retention must bound the store: {}", listed.len());
    }

    #[test]
    fn kind_parse_is_lenient_and_stable() {
        assert_eq!(DraftKind::parse("email"), DraftKind::EmailReply);
        assert_eq!(DraftKind::parse("email_reply"), DraftKind::EmailReply);
        assert_eq!(DraftKind::parse("doc"), DraftKind::Doc);
        assert_eq!(DraftKind::parse("whatever"), DraftKind::Message);
        assert_eq!(DraftKind::EmailReply.as_str(), "email_reply");
    }
}
