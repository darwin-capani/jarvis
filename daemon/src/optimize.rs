//! Optimization-from-usage — the TRACE STORE.
//!
//! This module is the FIRST half of the optimization-from-usage loop: a local,
//! PII-REDACTED record of "what the user said (shape only) -> what DARWIN chose
//! (agent/mode/tool) -> how it went (success / corrected-next-turn / failed)".
//! A later Optimizer phase reads this corpus to PROPOSE a measured tuning of
//! routing/selection (agent-pick, mode classification, lexical cue weights), and
//! adopts it ONLY IF it MEASURABLY beats the current baseline on HELD-OUT traces.
//! That phase mirrors self-heal's posture exactly: propose-only, human-applied,
//! reversible. NONE of that lives here — this module only records and reads.
//!
//! SAFETY / PRIVACY CONTRACT (non-negotiable, mirrors [self_heal]):
//!   * Ships OFF: [optimize].enabled = false. With it false [`record_trace`] is a
//!     pure NO-OP — nothing is written, no corpus accrues, so the optimizer has
//!     nothing to learn from. The live recording is RUNTIME-gated (traces accrue
//!     only while the daemon runs with this ON); tests insert mock traces directly.
//!   * PII-REDACTED at the source: every utterance is passed through [`redact`]
//!     BEFORE it is ever stored. The redactor strips emails, phone numbers, long
//!     digit runs (>=6), URLs carrying embedded credentials, and anything
//!     api-key-/token-shaped. A secret or PII NEVER reaches the table — the store
//!     only ever sees the redacted form (enforced by [`TraceStore::record`], which
//!     redacts again defensively even though the recorder already did).
//!   * Bounded retention: the table is capped (`MAX_TRACES`); recording evicts the
//!     OLDEST rows past the cap, so the corpus cannot grow without bound on an
//!     always-on appliance.
//!
//! The store is a DEDICATED SQLite file (state/optimize.db), opened with the same
//! WAL + busy-timeout pattern as memory.rs, kept separate from the consolidated
//! memory tables so the bounded, evict-oldest trace corpus never entangles with
//! the reflection-bounded facts/transcripts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::Config;

/// Hard cap on stored traces. The recorder evicts the oldest rows past this so
/// the corpus is bounded on the always-on appliance (mirrors memory.rs's
/// transcripts cap). Generous enough for a meaningful held-out split, small
/// enough that the file stays tiny.
pub const MAX_TRACES: usize = 5_000;

/// Digit-run threshold: a run of this many or more ASCII digits is redacted
/// (account numbers, OTPs, long ids, the digit core of a phone number). 6 is
/// below any real PII length while leaving small counts ("page 3", "iphone 15",
/// "2026") untouched.
const DIGIT_RUN_MIN: usize = 6;

/// The placeholder a redacted span is replaced with. Constant so the redactor is
/// deterministic and the round-trip tests assert exact output.
const REDACTED: &str = "[redacted]";

// ---------------------------------------------------------------------------
// Trace record
// ---------------------------------------------------------------------------

/// How one interaction turned out — the success signal the optimizer learns
/// from. A trace's outcome is the LABEL: the optimizer's objective is to raise
/// the rate of `Success` (and lower `CorrectedNextTurn`) on held-out traces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    /// The turn completed and the user did NOT correct/redirect on the next
    /// turn (or gave explicit positive feedback) — the routing/selection was
    /// right.
    Success,
    /// The user corrected or redirected on the very next turn ("no, the OTHER
    /// one", re-asking, switching agent) — the clearest learnable signal that
    /// the chosen agent/mode/tool was WRONG.
    CorrectedNextTurn,
    /// The turn failed outright (error, no usable result).
    Failed,
    /// No signal yet (e.g. the next turn has not happened, or feedback is
    /// ambiguous). Recorded but weighted out by the optimizer.
    Unknown,
}

impl Outcome {
    /// Stable wire token for the DB column (so the schema is human-readable and
    /// the enum can evolve without a numeric remap).
    fn as_token(self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::CorrectedNextTurn => "corrected_next_turn",
            Outcome::Failed => "failed",
            Outcome::Unknown => "unknown",
        }
    }

    /// Inverse of [`Self::as_token`]; unknown/garbled tokens degrade to
    /// `Unknown` (a corrupt row never silently reads as a Success the optimizer
    /// would reward).
    fn from_token(s: &str) -> Self {
        match s {
            "success" => Outcome::Success,
            "corrected_next_turn" => Outcome::CorrectedNextTurn,
            "failed" => Outcome::Failed,
            _ => Outcome::Unknown,
        }
    }
}

/// One recorded interaction. `utterance_redacted` is ALWAYS the PII-redacted
/// form — the raw utterance never lives in a Trace. The remaining fields are the
/// decisions the optimizer may later tune (agent/mode/tool) plus the outcome
/// label and latency, with a unix-seconds timestamp for ordering + retention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Trace {
    /// The user's utterance with all PII/secret spans stripped (see [`redact`]).
    /// This is the ONLY representation of the utterance the store ever holds.
    pub utterance_redacted: String,
    /// The intent the router inferred for this turn (e.g. "action", "memory",
    /// "conversation").
    pub intent: String,
    /// The agent selected to handle the turn (the routing decision under test).
    pub agent: String,
    /// The mode classified for the turn (selector.rs classify_mode output).
    pub mode: String,
    /// The tool or skill invoked, if any ("" when none).
    pub tool_or_skill: String,
    /// How the turn went — the learnable label.
    pub outcome: Outcome,
    /// End-to-end turn latency in milliseconds.
    pub latency_ms: u64,
    /// Unix seconds when the turn was recorded.
    pub ts: u64,
}

impl Trace {
    /// Build a trace from RAW interaction fields, redacting the utterance at
    /// construction so a caller cannot accidentally hold a Trace carrying PII.
    /// `record_trace` uses this; tests may also build directly with an
    /// already-redacted string for known-answer assertions.
    // The args are the raw interaction fields captured at construction; grouping
    // them would just re-create Trace's own field set.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        raw_utterance: &str,
        intent: impl Into<String>,
        agent: impl Into<String>,
        mode: impl Into<String>,
        tool_or_skill: impl Into<String>,
        outcome: Outcome,
        latency_ms: u64,
        ts: u64,
    ) -> Self {
        Self {
            utterance_redacted: redact(raw_utterance),
            intent: intent.into(),
            agent: agent.into(),
            mode: mode.into(),
            tool_or_skill: tool_or_skill.into(),
            outcome,
            latency_ms,
            ts,
        }
    }
}

// ---------------------------------------------------------------------------
// PII / secret redactor — pure, exhaustively unit-tested
// ---------------------------------------------------------------------------

/// Strip every PII/secret span from `s` BEFORE it can be stored, returning the
/// safe-to-persist form. Pure and deterministic (token-by-token; no regex crate,
/// matching the codebase's hand-scanned style). Order matters: secret-/token-
/// and credential-URL detection run BEFORE the digit-run pass so a token's
/// digits are subsumed by the broader `[redacted]` rather than leaving a partial
/// tail.
///
/// What it removes (each replaced with `[redacted]`):
///   * EMAILS — any `local@domain.tld` token.
///   * URLs WITH CREDENTIALS — `scheme://user:pass@host/...` (the whole URL, so
///     the embedded password never survives).
///   * API-KEY / TOKEN-SHAPED tokens — a token that looks like a secret:
///     a known secret prefix (sk-, pk-, ghp_, xoxb-, AKIA…, Bearer payloads,
///     "api_key=…", "token=…"), OR a long high-entropy-ish alphanumeric run
///     (>= 20 chars mixing letters and digits) that is not ordinary prose.
///   * PHONE NUMBERS — a token whose digit content (ignoring +(). -) is a
///     plausible phone length (>= 7 digits), e.g. +1 (415) 555-0123.
///   * LONG DIGIT RUNS — any remaining run of >= 6 consecutive digits.
///
/// Ordinary words, short numbers, and punctuation pass through unchanged.
pub fn redact(s: &str) -> String {
    // Split on whitespace but preserve the original spacing so the redacted
    // utterance still reads naturally. We rebuild token-by-token.
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for token in s.split_whitespace() {
        if !first {
            out.push(' ');
        }
        first = false;
        out.push_str(&redact_token(token));
    }
    // Final passes over the rebuilt string. `redact_grouped_secrets` FIRST collapses
    // SEPARATOR-GROUPED card/account numbers (Luhn-valid 13-19 digit) and SSNs
    // (3-2-4 shape) — the normal displayed form "4242 4242 4242 4242" / "123 45 6789"
    // that the whitespace split scatters into sub-6-digit tokens no other rule
    // catches. Then `redact_digit_runs` collapses long CONTIGUOUS numeric ids. Both
    // are idempotent on already-redacted text.
    redact_digit_runs(&redact_grouped_secrets(&out))
}

/// Standard Luhn (mod-10) checksum over a pure-digit string — the industry check
/// for a payment card number. Used to distinguish a real PAN from an incidental
/// long numeric grouping (a year list rarely passes Luhn), so redaction stays
/// precise. Empty / non-digit input is not valid.
fn passes_luhn(digits: &str) -> bool {
    if digits.len() < 13 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let mut sum = 0u32;
    for (i, c) in digits.chars().rev().enumerate() {
        let mut d = c.to_digit(10).unwrap_or(0);
        if i % 2 == 1 {
            d *= 2;
            if d > 9 {
                d -= 9;
            }
        }
        sum += d;
    }
    sum.is_multiple_of(10)
}

/// Whether a digit-and-separator `span` is exactly the U.S. SSN shape: three groups
/// of 3-2-4 digits separated by a single space or hyphen ("123 45 6789" /
/// "123-45-6789"). SSNs carry no checksum, so the shape is the tell.
fn is_ssn_shape(span: &str) -> bool {
    let groups: Vec<&str> = span.split([' ', '-']).filter(|g| !g.is_empty()).collect();
    groups.len() == 3
        && [3usize, 2, 4] == [groups[0].len(), groups[1].len(), groups[2].len()]
        && groups.iter().all(|g| g.chars().all(|c| c.is_ascii_digit()))
}

/// Collapse separator-grouped SECRETS the whitespace-token rules miss: a payment
/// card (13-19 digits with single space/hyphen separators that passes [`passes_luhn`])
/// or an SSN (the 3-2-4 shape). Only the digit-bounded span is replaced, so
/// surrounding text/spacing is preserved. Precise by construction — a benign long
/// numeric grouping (a list of years) fails both Luhn and the SSN shape, so it
/// survives; a real card / SSN in its normal displayed form does not.
fn redact_grouped_secrets(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        // Extend a maximal run of digits joined by SINGLE space/hyphen separators
        // (a separator only continues the run when a digit follows it).
        let start = i;
        let mut last_digit = i;
        let mut j = i;
        while j < chars.len() {
            if chars[j].is_ascii_digit() {
                last_digit = j;
                j += 1;
            } else if matches!(chars[j], ' ' | '-')
                && j + 1 < chars.len()
                && chars[j + 1].is_ascii_digit()
            {
                j += 1;
            } else {
                break;
            }
        }
        let span: String = chars[start..=last_digit].iter().collect();
        let digits: String = span.chars().filter(|c| c.is_ascii_digit()).collect();
        let is_card = (13..=19).contains(&digits.len()) && passes_luhn(&digits);
        if is_card || is_ssn_shape(&span) {
            out.push_str(REDACTED);
        } else {
            out.push_str(&span);
        }
        i = last_digit + 1;
    }
    out
}

/// Per-token redaction: classify a single whitespace-delimited token and either
/// pass it through, or collapse it (optionally preserving a trailing sentence
/// punctuation mark so "...token." stays a sentence). Leading/trailing ASCII
/// punctuation is peeled so "(sk-LIVE...)" or "email@x.com," still matches.
fn redact_token(token: &str) -> String {
    // Peel symmetric surrounding punctuation so the CORE is classified, then
    // restore the wrapper around the result.
    let lead: String = token
        .chars()
        .take_while(|c| is_wrap_punct(*c))
        .collect();
    let core_and_trail = &token[lead.len()..];
    let trail: String = core_and_trail
        .chars()
        .rev()
        .take_while(|c| is_wrap_punct(*c))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let core = &core_and_trail[..core_and_trail.len() - trail.len()];

    let replaced = if core.is_empty() {
        return token.to_string();
    } else if is_email(core)
        || is_credentialed_url(core)
        || is_secret_shaped(core)
        || is_phone(core)
    {
        REDACTED.to_string()
    } else {
        core.to_string()
    };
    format!("{lead}{replaced}{trail}")
}

/// Punctuation we peel from a token's edges before classifying its core.
fn is_wrap_punct(c: char) -> bool {
    matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | '.' | ';' | ':' | '!' | '?' | '"' | '\'' | '`')
}

/// An email: exactly one '@', a non-empty local part, and a domain with a dot
/// and a >=2-char TLD made of letters.
fn is_email(token: &str) -> bool {
    let mut parts = token.splitn(2, '@');
    let (local, domain) = match (parts.next(), parts.next()) {
        (Some(l), Some(d)) => (l, d),
        _ => return false,
    };
    if local.is_empty() || domain.contains('@') {
        return false;
    }
    // domain has at least one dot and a final segment of >=2 letters.
    match domain.rsplit_once('.') {
        Some((host, tld)) => {
            !host.is_empty()
                && tld.len() >= 2
                && tld.chars().all(|c| c.is_ascii_alphabetic())
                && local.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-'))
        }
        None => false,
    }
}

/// A URL carrying embedded credentials: `scheme://user:pass@host...`. The
/// userinfo (the part before the FIRST '@' after `://`) must contain a ':'
/// (user:pass) for this to be a CREDENTIALED url — a bare `https://host/path`
/// (no userinfo) is NOT redacted here (it is not PII/secret-shaped by itself).
fn is_credentialed_url(token: &str) -> bool {
    let Some(after_scheme) = token.find("://").map(|i| &token[i + 3..]) else {
        return false;
    };
    // userinfo is everything before the first '@' (if any) — and must hold a ':'.
    match after_scheme.split_once('@') {
        Some((userinfo, _host)) => userinfo.contains(':') && !userinfo.is_empty(),
        None => false,
    }
}

/// A token that looks like an API key / token / secret. Two routes:
///   1. A known secret PREFIX / inline-assignment shape, OR
///   2. A long mixed-alphanumeric run (>= 20 chars containing BOTH a letter and
///      a digit) — the entropy floor below which ordinary prose words never
///      reach. Pure words ("congratulations") and pure numbers (handled by the
///      digit-run pass) do NOT trip this.
fn is_secret_shaped(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    // Inline assignments: api_key=..., token=..., apikey:..., secret=...
    for key in ["api_key=", "apikey=", "api-key=", "token=", "secret=", "password=", "api_key:", "token:"] {
        if lower.starts_with(key) {
            return true;
        }
    }
    // Known provider secret prefixes (case-sensitive where the real ones are).
    const PREFIXES: &[&str] = &["sk-", "pk-", "rk-", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "xoxa-", "akia", "asia"];
    let lead_lower = lower.as_str();
    for p in PREFIXES {
        if lead_lower.starts_with(p) && token.len() >= p.len() + 8 {
            return true;
        }
    }
    // A "Bearer <token>" fragment glued without a space is rare; the assignment
    // forms above cover the spaced case via the per-token "token=" check.

    // Generic high-entropy-ish run: long, mixed letters+digits, no spaces.
    let len = token.chars().count();
    if len >= 20 {
        let has_alpha = token.chars().any(|c| c.is_ascii_alphabetic());
        let has_digit = token.chars().any(|c| c.is_ascii_digit());
        let all_token_chars = token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/' | '='));
        if has_alpha && has_digit && all_token_chars {
            return true;
        }
    }
    false
}

/// A phone number: once '+', '(', ')', '-', '.', and spaces-within-token are
/// removed, the token is ALL digits and has a plausible phone length (>= 7).
/// "555-0123" (7), "+1 (415) 555-0123" tokens, etc. A short "3-2" or a date
/// "2026-06-15" (digits 8 but contains separators -> still 8 digits >=7) —
/// guard the date case by requiring the token to be DOMINATED by digits and
/// phone-punctuation only (no letters), which a date satisfies, so we ALSO
/// exclude pure ISO dates by length heuristic: phone digit count is 7..=15.
fn is_phone(token: &str) -> bool {
    let digits: String = token.chars().filter(|c| c.is_ascii_digit()).collect();
    let non_phone = token
        .chars()
        .any(|c| !(c.is_ascii_digit() || matches!(c, '+' | '(' | ')' | '-' | '.' | ' ')));
    if non_phone {
        return false;
    }
    // 7..=15 digits is the E.164-ish phone band. (>=6 pure-digit runs are caught
    // by the digit-run pass anyway; this rule additionally collapses formatted
    // numbers like 555-0123 whose individual digit runs are each < 6.)
    let n = digits.len();
    (7..=15).contains(&n)
}

/// Replace every maximal run of >= DIGIT_RUN_MIN ASCII digits with `[redacted]`.
/// The final safety net for long numeric ids/account numbers/OTPs not already
/// collapsed by a higher-priority rule. Short numbers (years, small counts)
/// survive.
fn redact_digit_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        if run.chars().count() >= DIGIT_RUN_MIN {
            out.push_str(REDACTED);
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii_digit() {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

// ---------------------------------------------------------------------------
// Persistence — bounded, evict-oldest SQLite store
// ---------------------------------------------------------------------------

/// The local trace corpus. A dedicated SQLite file (state/optimize.db) opened
/// with the same WAL + busy-timeout pattern as memory.rs. `rusqlite::Connection`
/// is Send-not-Sync, so an async Mutex serializes access (statements are short).
pub struct TraceStore {
    conn: Mutex<Connection>,
}

impl TraceStore {
    /// Open (creating if needed) the trace store at `path` PLAINTEXT (today's
    /// behavior, byte-for-byte). Reached when `[security].encrypt_memory` is OFF
    /// (the default). Idempotent — re-open re-runs the CREATE TABLE IF NOT EXISTS.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init_conn(conn)
    }

    /// Open the trace store ENCRYPTED (transparent whole-file SQLCipher AES-256).
    /// `key` is applied via `PRAGMA key` immediately after open, before any other
    /// pragma/statement. Reached only when `[security].encrypt_memory` is ON;
    /// tests pass an explicit in-test key (no Keychain).
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// Shared setup (pragmas + schema), run AFTER any `PRAGMA key`.
    fn init_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS traces(
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL,
                utterance_redacted TEXT NOT NULL,
                intent TEXT NOT NULL,
                agent TEXT NOT NULL,
                mode TEXT NOT NULL,
                tool_or_skill TEXT NOT NULL,
                outcome TEXT NOT NULL,
                latency_ms INTEGER NOT NULL
            );",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert one trace, then evict the oldest rows beyond `MAX_TRACES` so the
    /// corpus stays bounded. The utterance is redacted AGAIN here (defense in
    /// depth) so a caller that built a Trace by hand from a raw string still
    /// cannot persist PII — the table is guaranteed to hold only redacted text.
    ///
    /// The LIVE recorder uses [`Self::record_returning_id`] (it needs the row id
    /// for cross-turn correction labeling); this id-less convenience wrapper is
    /// retained as the store's hermetic test-facing API.
    #[allow(dead_code)] // test-facing wrapper; live code uses record_returning_id
    pub async fn record(&self, trace: &Trace) -> Result<()> {
        self.record_returning_id(trace).await.map(|_| ())
    }

    /// Like [`Self::record`] but returns the inserted row's monotonic `id`. The
    /// live recorder holds this id so that, IF the very next turn turns out to be
    /// a CORRECTION of this one's routing, it can re-label THIS exact row's
    /// outcome to [`Outcome::CorrectedNextTurn`] (the learnable signal) via
    /// [`Self::label_outcome`] — without re-scanning or guessing which row was
    /// last. Same redaction + eviction guarantees as `record`.
    pub async fn record_returning_id(&self, trace: &Trace) -> Result<i64> {
        let conn = self.conn.lock().await;
        // Defensive re-redaction: NEVER trust that the caller already redacted.
        let safe = redact(&trace.utterance_redacted);
        conn.execute(
            "INSERT INTO traces(ts, utterance_redacted, intent, agent, mode, tool_or_skill, outcome, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                trace.ts as i64,
                safe,
                trace.intent,
                trace.agent,
                trace.mode,
                trace.tool_or_skill,
                trace.outcome.as_token(),
                trace.latency_ms as i64,
            ],
        )?;
        let id = conn.last_insert_rowid();
        // Evict oldest beyond the cap (by id, which is monotonic with insert
        // order — robust even when many rows share a ts second).
        conn.execute(
            "DELETE FROM traces WHERE id NOT IN
             (SELECT id FROM traces ORDER BY id DESC LIMIT ?1)",
            params![MAX_TRACES as i64],
        )?;
        Ok(id)
    }

    /// Re-label the outcome of the row with `id` (used by the live recorder to
    /// mark the PRIOR turn's trace as a [`Outcome::CorrectedNextTurn`] once the
    /// next turn reveals the routing was wrong). A no-op (zero rows affected) if
    /// that row has already been evicted by the bounded-retention cap — the
    /// correction signal for a long-ago turn is simply dropped, never errors.
    /// Returns the number of rows updated (0 or 1) for test assertions.
    pub async fn label_outcome(&self, id: i64, outcome: Outcome) -> Result<usize> {
        let conn = self.conn.lock().await;
        let n = conn.execute(
            "UPDATE traces SET outcome = ?1 WHERE id = ?2",
            params![outcome.as_token(), id],
        )?;
        Ok(n)
    }

    /// The most recent `limit` traces, NEWEST first — the optimizer's read path.
    /// (The optimizer splits this into train/held-out itself; the store just
    /// hands back the bounded recent window.)
    pub async fn recent(&self, limit: usize) -> Result<Vec<Trace>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT ts, utterance_redacted, intent, agent, mode, tool_or_skill, outcome, latency_ms
             FROM traces ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |row| {
                Ok(Trace {
                    ts: row.get::<_, i64>(0)? as u64,
                    utterance_redacted: row.get(1)?,
                    intent: row.get(2)?,
                    agent: row.get(3)?,
                    mode: row.get(4)?,
                    tool_or_skill: row.get(5)?,
                    outcome: Outcome::from_token(&row.get::<_, String>(6)?),
                    latency_ms: row.get::<_, i64>(7)? as u64,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Total stored traces (for tests / telemetry / retention assertions).
    #[allow(dead_code)] // hermetic test + future-telemetry helper
    pub async fn count(&self) -> Result<u64> {
        let conn = self.conn.lock().await;
        let n: i64 = conn.query_row("SELECT COUNT(*) FROM traces", [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }
}

// ---------------------------------------------------------------------------
// Recorder — the daemon's per-turn entry point (RUNTIME-gated; no-op when OFF)
// ---------------------------------------------------------------------------

/// Record one interaction's trace IF (and only if) the optimizer is enabled,
/// returning the inserted row's `id` (`None` when disabled).
///
/// This is the function the daemon calls per turn (the LIVE recording is
/// runtime-gated: it only ever fires while the real daemon runs with
/// [optimize].enabled = true). When `cfg.optimize.enabled` is false this is a
/// pure NO-OP — it returns `Ok(None)` immediately and writes NOTHING, so the
/// shipped-OFF default never accrues a corpus. The raw utterance is REDACTED
/// here before it reaches the store. The returned id lets the live recorder hold
/// the PRIOR turn's row so a next-turn correction can re-label it (see
/// [`is_correction`] + [`TraceStore::label_outcome`]). Tests insert mock traces
/// via [`TraceStore::record`] directly; this gate is itself unit-tested.
#[allow(clippy::too_many_arguments)]
pub async fn record_trace(
    cfg: &Config,
    store: &TraceStore,
    raw_utterance: &str,
    intent: &str,
    agent: &str,
    mode: &str,
    tool_or_skill: &str,
    outcome: Outcome,
    latency_ms: u64,
    ts: u64,
) -> Result<Option<i64>> {
    if !cfg.optimize.enabled {
        return Ok(None); // shipped-OFF default: record NOTHING.
    }
    let trace = Trace::new(
        raw_utterance,
        intent,
        agent,
        mode,
        tool_or_skill,
        outcome,
        latency_ms,
        ts,
    );
    store.record_returning_id(&trace).await.map(Some)
}

// ---------------------------------------------------------------------------
// CROSS-TURN CORRECTION DETECTION — the conservative predicate
// ---------------------------------------------------------------------------

/// The PRIOR turn's bookkeeping the live recorder carries forward so the NEXT
/// turn can decide whether it corrected the prior routing. Only the fields the
/// correction predicate needs (the prior trace's row id + the intent + the agent
/// it was routed to). Held in `main`'s turn loop, exactly like `last_reply`.
#[derive(Debug, Clone)]
pub struct PriorTurn {
    /// The prior trace's row id (so we re-label the EXACT row, not a guess).
    pub trace_id: i64,
    /// The intent the prior turn was classified as (a correction is, by
    /// definition, the SAME intent re-routed — a different intent is just the
    /// conversation moving on, never a correction).
    pub intent: String,
    /// The agent the prior turn was routed to (a correction sends the same
    /// intent to a DIFFERENT agent; same-agent is not a correction).
    pub agent: String,
}

/// CONSERVATIVE correction predicate. Returns true IFF `current` is best read as
/// the user CORRECTING the routing of `prior` — the single learnable signal the
/// optimizer trains on. Deliberately strict: a normal follow-up, a topic change,
/// or a satisfied next request must NOT be labelled a correction (over-labelling
/// would teach the optimizer noise). PURE + deterministic — a function of the two
/// turns' fields and the current utterance text only (no clock, no I/O).
///
/// ALL of these must hold:
///   1. The current utterance carries an explicit REDIRECT/NEGATION cue — a
///      phrase that means "that was the wrong handler" ("no, ask <other>", "not
///      <agent>", "I meant", "that's wrong", "try again", "wrong"). A follow-up
///      WITHOUT such a cue is never a correction, however close in topic.
///   2. The two turns share the SAME inferred intent (the user is re-aiming the
///      same request, not starting a new one).
///   3. The current turn routed to a DIFFERENT agent than the prior turn (the
///      route actually CHANGED — a re-ask that landed on the same agent corrected
///      nothing).
///      Failing ANY of the three -> not a correction (the safe default is "no
///      signal", which leaves the prior trace as Success).
pub fn is_correction(prior: &PriorTurn, current_intent: &str, current_agent: &str, current_utterance: &str) -> bool {
    // (2) same intent and (3) the route actually changed.
    if prior.intent != current_intent {
        return false;
    }
    if prior.agent == current_agent {
        return false;
    }
    // (1) an explicit redirect/negation cue. Whole-word / phrase match on the
    // lowercased utterance so an incidental substring ("another", "knot") can
    // never trip it.
    has_redirect_cue(&current_utterance.to_lowercase())
}

/// Hard, explicit redirect/negation cues that mark a turn as a correction of the
/// prior routing. Conservative by design: only phrasings that unambiguously mean
/// "that was handled wrong / aim it elsewhere". Phrase cues are matched as
/// substrings (they are multi-word and unambiguous); single-word cues match on a
/// word boundary so "wrong" matches but "wronged" does not.
fn has_redirect_cue(lower: &str) -> bool {
    const PHRASE_CUES: &[&str] = &[
        "no, ask",
        "no ask",
        "not that",
        "that's wrong",
        "thats wrong",
        "that is wrong",
        "that's not right",
        "i meant",
        "i didn't mean",
        "i did not mean",
        "wrong one",
        "wrong agent",
        "not what i",
        "instead ask",
        "ask the other",
        "ask someone else",
        "try again",
        "no i wanted",
        "no i want",
    ];
    // BOTH phrase and single-word cues match on WORD BOUNDARIES (contains_word
    // boundary-checks the literal needle's outer edges, internal spaces included),
    // so a cue can never trip inside a longer word: "no ask" no longer matches
    // "pia[no ask]s", nor "try again" inside "re[try again]". A benign follow-up
    // must not be mislabeled a routing correction in the optimizer corpus.
    for c in PHRASE_CUES {
        if agents::contains_word(lower, c) {
            return true;
        }
    }
    const WORD_CUES: &[&str] = &["wrong", "incorrect"];
    WORD_CUES.iter().any(|w| agents::contains_word(lower, w))
}

// ===========================================================================
// THE OPTIMIZER — scoring harness + bounded candidate search + gated proposal
// ===========================================================================
//
// The SECOND half of the optimization-from-usage loop. It reads the recorded
// corpus (above) and PROPOSES a measured tuning of routing/selection that is
// ADOPTED ONLY IF it MEASURABLY beats the current baseline on HELD-OUT traces.
// It mirrors self-heal's posture EXACTLY: propose-only, human-applied,
// reversible; ships ON ([optimize].enabled = true) — live trace recording is
// runtime-gated + PII-redacted, and the optimizer still only PROPOSES (mode stays
// "propose"); never silently mutates a live config.
//
// WHAT IT TUNES (honest + bounded): the ROUTING decision — which agent a turn
// is delegated to. The shipped router (agents.rs `select`/`select_with_fallback`)
// keys on per-agent CUE vocabulary ([`crate::agents::CUE_VOCAB`]). The optimizer
// tunes a small, interpretable LAYER over that vocabulary: a per-agent map of
// {cue word -> weight} (baseline = every shipped cue at weight 1.0), plus the
// ability to ADD a learned cue word (lifted from a corrected trace's utterance)
// for the agent that trace reveals as correct. A candidate is just "the shipped
// cues, with these few weights nudged / these few words added" — minimal and
// human-readable. It is NOT a neural model, NOT a prompt rewrite, NOT magic.
//
// WHAT A TRACE REVEALS (the label):
//   * Success(utterance, agent)            => routing utterance -> agent was RIGHT.
//   * CorrectedNextTurn(utterance, agent)  => routing utterance -> agent was WRONG
//                                             (the user redirected next turn).
//   * Failed / Unknown                     => no routing signal; weighted out.
// The scorer REPLAYS each labelled utterance through a candidate routing config
// and checks: did it reproduce the confirmed-right pick (Success) / did it AVOID
// the confirmed-wrong pick (CorrectedNextTurn)? Accuracy over a HELD-OUT split
// is the objective. Held-out is what makes "can't make it worse" real: a
// candidate that only fits the train split but not held-out is rejected.
//
// RUNTIME vs TESTS: the live recording that fills the corpus is runtime-gated
// (it accrues only while the real daemon runs with [optimize].enabled = true).
// Every test here injects MOCK traces and an INJECTED clock; nothing reaches the
// network, the mic, or a live config.

use std::collections::BTreeMap;

use serde_json::json;

use crate::agents::{self, CUE_VOCAB};
use crate::telemetry;

/// Minimum number of usable (Success/CorrectedNextTurn) traces before the
/// optimizer will even attempt a proposal. Below this the corpus is too thin to
/// trust a held-out estimate — propose nothing (silence beats a noisy nudge).
pub const MIN_USABLE_TRACES: usize = 12;

/// Fraction of the (chronologically newest-first) usable corpus reserved as the
/// HELD-OUT split the candidate is JUDGED on. The remainder is the TRAIN split
/// the candidate is DERIVED from. A candidate is fit on train and must prove
/// itself on held-out it never saw — the overfitting guard.
const HELD_OUT_FRACTION: f64 = 0.4;

/// The adoption MARGIN: a candidate must beat the baseline's held-out accuracy
/// by at least this absolute amount to be proposed. A bare tie or a sub-margin
/// gain is NOT enough — the gate is deliberately strict so noise never trips it.
const ADOPTION_MARGIN: f64 = 0.05;

/// Cap on how many candidates the search will generate + score. Bounded so the
/// optimizer's own cost stays trivial and the candidate space stays
/// interpretable (a handful of minimal nudges, not a sprawling grid).
const MAX_CANDIDATES: usize = 24;

/// The weight an UPWEIGHT nudge or an ADDED learned cue gets. Larger than the
/// 1.0 baseline so a single tuned cue can break a tie toward the
/// revealed-correct agent, but bounded (no runaway weights).
const TUNED_WEIGHT: f64 = 2.0;

// ---------------------------------------------------------------------------
// Routing config under test — a tunable LAYER over the shipped cue vocabulary
// ---------------------------------------------------------------------------

/// A candidate (or the baseline) routing configuration: for each agent, the map
/// of {cue word -> weight} the REPLAY router scores with. The baseline is every
/// shipped [`CUE_VOCAB`] cue at weight 1.0; a candidate differs from it by a few
/// nudged weights and/or a few ADDED learned cue words — minimal + interpretable
/// so a human reviewing the proposal can read exactly what changed and why.
///
/// This is a LAYER, not a fork: it is always SEEDED from the shipped vocabulary
/// (`baseline`), so the optimizer can only adjust the existing routing signal,
/// never invent an unrelated one.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingConfig {
    /// agent name -> (cue word -> weight). Deterministic ordering (BTreeMap) so
    /// replay, scoring, and the rendered diff are all reproducible.
    weights: BTreeMap<String, BTreeMap<String, f64>>,
}

impl RoutingConfig {
    /// The current BASELINE: the shipped cue vocabulary, every cue at weight
    /// 1.0. This is the config the live router's semantic signal reflects, and
    /// the bar every candidate must beat on held-out traces.
    pub fn baseline() -> Self {
        let mut weights: BTreeMap<String, BTreeMap<String, f64>> = BTreeMap::new();
        for (agent, cues) in CUE_VOCAB {
            let entry = weights.entry((*agent).to_string()).or_default();
            for cue in cues.split_whitespace() {
                entry.insert(cue.to_string(), 1.0);
            }
        }
        RoutingConfig { weights }
    }

    /// The diff from the baseline as a list of human-readable change lines
    /// (empty when this IS the baseline). Each line is one nudged weight or one
    /// added cue, e.g. "gecko: cue 'crypto' weight 1.0 -> 2.0" or
    /// "midas: + learned cue 'venmo' (weight 2.0)". Drives the proposal artifact.
    pub fn diff_from(&self, base: &RoutingConfig) -> Vec<String> {
        let mut lines = Vec::new();
        for (agent, cues) in &self.weights {
            let base_cues = base.weights.get(agent);
            for (cue, w) in cues {
                match base_cues.and_then(|b| b.get(cue)) {
                    Some(bw) if (bw - w).abs() > f64::EPSILON => {
                        lines.push(format!("{agent}: cue '{cue}' weight {bw:.1} -> {w:.1}"));
                    }
                    Some(_) => {}
                    None => {
                        lines.push(format!("{agent}: + learned cue '{cue}' (weight {w:.1})"));
                    }
                }
            }
        }
        lines.sort();
        lines
    }
}

/// REPLAY one utterance through a routing config: score every agent by summing
/// the weights of its cue words that appear (whole-word) in the utterance, and
/// return the highest-scoring agent. Ties (including the all-zero "no cue
/// matched" case) resolve to the orchestrator fallback "darwin" — exactly the
/// live router's safe default. Pure + deterministic.
///
/// Matching uses [`crate::agents::contains_word`] — the SAME word-boundary rule
/// the live router uses — so the replay is an honest stand-in for the real
/// routing decision, not a re-implementation that could quietly disagree.
fn replay_route(cfg: &RoutingConfig, utterance: &str) -> String {
    let lower = utterance.to_lowercase();
    let mut best: Option<(String, f64)> = None;
    // Iterate agents in deterministic (BTreeMap) order so ties are broken by
    // first-seen, which we then collapse to the orchestrator below.
    for (agent, cues) in &cfg.weights {
        let mut score = 0.0;
        for (cue, w) in cues {
            if agents::contains_word(&lower, cue) {
                score += w;
            }
        }
        if score <= 0.0 {
            continue;
        }
        match &best {
            Some((_, bs)) if score > *bs => best = Some((agent.clone(), score)),
            Some((_, bs)) if (score - *bs).abs() < f64::EPSILON => {
                // A genuine tie between two specialists is ambiguous — fall back
                // to the orchestrator rather than guess (mirrors the live
                // semantic-pick tie -> orchestrator rule).
                best = Some(("darwin".to_string(), *bs));
            }
            Some(_) => {}
            None => best = Some((agent.clone(), score)),
        }
    }
    best.map(|(a, _)| a).unwrap_or_else(|| "darwin".to_string())
}

// ---------------------------------------------------------------------------
// (1) SCORING HARNESS — pure, deterministic, held-out-aware
// ---------------------------------------------------------------------------

/// The result of scoring a routing config against a set of labelled traces. It
/// keeps the two CLASSES separate (Success vs CorrectedNextTurn) so the adoption
/// gate can require "not worse on ANY class" — a candidate that fixes
/// corrections but breaks confirmed-good routes is NOT an improvement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    /// Success traces whose confirmed-right pick the config REPRODUCED.
    pub success_hits: usize,
    /// Total Success traces scored.
    pub success_total: usize,
    /// CorrectedNextTurn traces whose confirmed-WRONG pick the config AVOIDED.
    pub corrected_hits: usize,
    /// Total CorrectedNextTurn traces scored.
    pub corrected_total: usize,
}

impl Score {
    /// Overall accuracy across both classes (0.0 when nothing scorable).
    pub fn accuracy(&self) -> f64 {
        let total = self.success_total + self.corrected_total;
        if total == 0 {
            return 0.0;
        }
        (self.success_hits + self.corrected_hits) as f64 / total as f64
    }

    /// Accuracy on the Success class alone (confirmed-good routes preserved).
    fn success_accuracy(&self) -> f64 {
        if self.success_total == 0 {
            return 1.0; // no confirmed-good routes to preserve -> vacuously fine
        }
        self.success_hits as f64 / self.success_total as f64
    }

    /// Accuracy on the CorrectedNextTurn class alone (corrections avoided).
    fn corrected_accuracy(&self) -> f64 {
        if self.corrected_total == 0 {
            return 1.0;
        }
        self.corrected_hits as f64 / self.corrected_total as f64
    }
}

/// SCORE `cfg` against `traces`: replay each Success/CorrectedNextTurn trace and
/// tally per-class hits. Failed/Unknown traces carry no routing signal and are
/// skipped. Pure + deterministic — the proto-eval Round 4 extends this.
///
///   * Success(u, a)            => HIT iff replay_route(cfg, u) == a.
///   * CorrectedNextTurn(u, a)  => HIT iff replay_route(cfg, u) != a
///     (avoiding the pick the user redirected away from).
pub fn score_config(cfg: &RoutingConfig, traces: &[Trace]) -> Score {
    let mut s = Score {
        success_hits: 0,
        success_total: 0,
        corrected_hits: 0,
        corrected_total: 0,
    };
    for t in traces {
        match t.outcome {
            Outcome::Success => {
                s.success_total += 1;
                if replay_route(cfg, &t.utterance_redacted) == t.agent {
                    s.success_hits += 1;
                }
            }
            Outcome::CorrectedNextTurn => {
                s.corrected_total += 1;
                if replay_route(cfg, &t.utterance_redacted) != t.agent {
                    s.corrected_hits += 1;
                }
            }
            Outcome::Failed | Outcome::Unknown => {}
        }
    }
    s
}

/// EVAL ACCURACY PRIMITIVE: score the CURRENT BASELINE routing config against the
/// HELD-OUT split of `traces` and return that held-out `Score` (or `None` when
/// there are NO usable traces at all — an empty held-out split). With one usable
/// trace [`split_usable`] carves a thin held-out of one, so a single trace yields a
/// thin-but-honestly-measured score; only a truly empty usable corpus is `None`.
///
/// This is the honest routing-accuracy number for the eval framework: it reuses
/// the SAME [`split_usable`] held-out carve the optimizer judges candidates on and
/// the SAME [`score_config`] scorer, applied to the shipped baseline. So "routing
/// accuracy" in the eval report is *exactly* the bar the optimizer must beat — not
/// a parallel re-implementation that could quietly disagree. Pure + deterministic;
/// `traces` arrive newest-first (the store's `recent` order).
pub fn baseline_held_out_score(traces: &[Trace]) -> Option<Score> {
    let (_train, held_out) = split_usable(traces);
    if held_out.is_empty() {
        return None;
    }
    Some(score_config(&RoutingConfig::baseline(), &held_out))
}

// ---------------------------------------------------------------------------
// (2) OPTIMIZER — bounded candidate search over a TRAIN/HELD-OUT split
// ---------------------------------------------------------------------------

/// A proposed optimization: the candidate config, the measured before/after on
/// the HELD-OUT split, the human-readable diff, and which corrected traces drove
/// it. This is the reviewable artifact — NOTHING here mutates a live config.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// The candidate routing config (a small layer over the baseline). The
    /// rendered artifacts persist its `diff`, not the struct; the live optimizer
    /// only ever PROPOSES (it never applies the candidate from Rust), so this
    /// field is read by the optimizer's own tests + held for the reserved
    /// auto-adopt path.
    #[allow(dead_code)] // proposal provenance; auto-adopt path is reserved
    pub candidate: RoutingConfig,
    /// Baseline held-out accuracy (the bar).
    pub baseline_accuracy: f64,
    /// Candidate held-out accuracy (must beat the bar by ADOPTION_MARGIN).
    pub candidate_accuracy: f64,
    /// Human-readable diff lines (what changed vs the baseline).
    pub diff: Vec<String>,
    /// Redacted utterances of the corrected traces that motivated the change
    /// (provenance for the human reviewer; already PII-redacted in the store).
    pub driven_by: Vec<String>,
}

impl Proposal {
    /// The measured improvement (held-out accuracy delta). Always > the margin
    /// for a real proposal.
    pub fn improvement(&self) -> f64 {
        self.candidate_accuracy - self.baseline_accuracy
    }
}

/// Split usable traces into (train, held_out). `traces` arrive newest-first
/// (the store's `recent` order); the HELD-OUT split is the NEWEST
/// `HELD_OUT_FRACTION` (the most recent behavior — the freshest test of whether
/// a candidate generalizes), and TRAIN is the older remainder the candidate is
/// derived from. Only Success/CorrectedNextTurn traces are usable.
fn split_usable(traces: &[Trace]) -> (Vec<Trace>, Vec<Trace>) {
    let usable: Vec<Trace> = traces
        .iter()
        .filter(|t| matches!(t.outcome, Outcome::Success | Outcome::CorrectedNextTurn))
        .cloned()
        .collect();
    let held_n = ((usable.len() as f64) * HELD_OUT_FRACTION).round() as usize;
    let held_n = held_n.clamp(1, usable.len().saturating_sub(1).max(1));
    let held_out = usable[..held_n.min(usable.len())].to_vec();
    let train = usable[held_n.min(usable.len())..].to_vec();
    (train, held_out)
}

/// Generate a BOUNDED set of candidate configs from the TRAIN split. Each
/// candidate is the baseline with a MINIMAL, interpretable change motivated by a
/// CorrectedNextTurn trace in train:
///
///   A. UPWEIGHT an existing cue: for a corrected trace whose utterance contains
///      a cue word that ALREADY belongs to some OTHER agent (not the wrongly
///      chosen one), upweight that cue for its owner — nudging the route away
///      from the corrected-wrong pick toward the cue's rightful owner.
///   B. ADD a learned cue: lift a salient content word from a corrected trace's
///      utterance and add it as a new cue for the agent the OTHER train traces
///      most associate that vocabulary with — a learned routing example.
///
/// Candidates are deduplicated and capped at MAX_CANDIDATES. The space is
/// deliberately small + minimal so every proposal is a handful of readable
/// nudges, never an opaque rewrite.
fn generate_candidates(base: &RoutingConfig, train: &[Trace]) -> Vec<RoutingConfig> {
    let mut out: Vec<RoutingConfig> = Vec::new();
    let mut seen: Vec<RoutingConfig> = Vec::new();

    // The corrected traces are the learnable signal: each one says "this
    // utterance should NOT have gone to `agent`". For each, try the minimal
    // nudges that could move replay_route away from that wrong pick.
    for t in train.iter().filter(|t| t.outcome == Outcome::CorrectedNextTurn) {
        if out.len() >= MAX_CANDIDATES {
            break;
        }
        let lower = t.utterance_redacted.to_lowercase();

        // Strategy A: upweight an existing cue OWNED BY ANOTHER AGENT that the
        // utterance contains. This strengthens the rightful owner so it can beat
        // the wrongly-chosen agent on this kind of utterance.
        for (agent, cues) in &base.weights {
            if *agent == t.agent {
                continue; // never reinforce the route the user corrected away from
            }
            for cue in cues.keys() {
                if agents::contains_word(&lower, cue) {
                    let mut cand = base.clone();
                    if let Some(m) = cand.weights.get_mut(agent) {
                        m.insert(cue.clone(), TUNED_WEIGHT);
                    }
                    push_unique(&mut out, &mut seen, cand);
                    if out.len() >= MAX_CANDIDATES {
                        break;
                    }
                }
            }
            if out.len() >= MAX_CANDIDATES {
                break;
            }
        }

        // Strategy B: add a LEARNED cue. Find the agent the SUCCESS traces in
        // train most associate this utterance's vocabulary with, and add the
        // utterance's most salient new word as a cue for that agent. A learned
        // routing example, lifted from real corrected usage.
        if let Some((target, word)) = learned_cue(&lower, &t.agent, base, train) {
            let mut cand = base.clone();
            cand.weights
                .entry(target)
                .or_default()
                .insert(word, TUNED_WEIGHT);
            push_unique(&mut out, &mut seen, cand);
        }
    }
    out.truncate(MAX_CANDIDATES);
    out
}

/// Push `cand` only if not already generated (dedup by structural equality).
fn push_unique(out: &mut Vec<RoutingConfig>, seen: &mut Vec<RoutingConfig>, cand: RoutingConfig) {
    if seen.contains(&cand) {
        return;
    }
    seen.push(cand.clone());
    out.push(cand);
}

/// Derive a LEARNED (target_agent, cue_word) from a corrected trace: among the
/// train SUCCESS traces that share a salient content word with this utterance,
/// the agent they most went to is the likely correct owner; the shared word
/// becomes a new cue for that agent. Returns None when no confident association
/// exists (the conservative default — add nothing). `wrong_agent` is excluded as
/// a target so we never re-learn the corrected-away route.
///
/// PRIVACY: candidate words come only from [`salient_words`], which applies
/// [`is_eligible_cue_word`], so a redactor-surviving secret (a no-digit
/// passphrase, a high-entropy fragment) can never be promoted into the returned
/// (agent, cue) pair — and thus never into the candidate config or the proposal.
fn learned_cue(
    lower: &str,
    wrong_agent: &str,
    base: &RoutingConfig,
    train: &[Trace],
) -> Option<(String, String)> {
    // Candidate content words: utterance tokens that are not stopwords, not
    // already a baseline cue for ANY agent, long enough to be meaningful.
    let existing: std::collections::HashSet<&str> = base
        .weights
        .values()
        .flat_map(|m| m.keys().map(|s| s.as_str()))
        .collect();
    for word in salient_words(lower) {
        if existing.contains(word) {
            continue;
        }
        // Which agent do train SUCCESS traces containing this word go to?
        let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
        for s in train.iter().filter(|t| t.outcome == Outcome::Success) {
            if s.agent == wrong_agent {
                continue;
            }
            if agents::contains_word(&s.utterance_redacted.to_lowercase(), word) {
                *tally.entry(s.agent.as_str()).or_default() += 1;
            }
        }
        // A confident association needs at least two corroborating success
        // traces pointing at ONE agent (single anecdotes do not get learned).
        if let Some((agent, &n)) = tally.iter().max_by_key(|(_, n)| **n) {
            if n >= 2 {
                return Some(((*agent).to_string(), word.to_string()));
            }
        }
    }
    None
}

/// Upper length bound for a learnable cue word. Real routing cues are short
/// natural words ("calendar"=8, "translate"=9, "forecast"=8, "notification"=12,
/// "authentication"=14 all fit). A longer all-alphabetic token that survived the
/// redactor is almost certainly a passphrase or secret (e.g. a no-digit
/// "correcthorsebatterystaple"), which must NEVER be lifted into a learned cue
/// or a human-read proposal artifact. 18 leaves headroom over the longest real
/// cue while still excluding such tokens. Lower bound stays at the existing >= 4.
const CUE_MAX_LEN: usize = 18;

/// Privacy gate: may `w` become a LEARNED routing cue (i.e. be inserted into a
/// candidate config and rendered verbatim into the human-read proposal)? This is
/// the single OUTCOME guard between a redactor-surviving token and a human's
/// eyes; it is intentionally conservative (defense in depth on top of `redact`):
///   * LENGTH in [4, CUE_MAX_LEN] — keeps the existing >= 4 floor and adds the
///     upper cap that rejects passphrase-length tokens the redactor's
///     letters+DIGITS entropy rule cannot catch (a no-digit passphrase).
///   * ALL ascii_lowercase — `salient_words` already lowercases and splits on
///     non-alphanumerics, but pin it so a future refactor cannot admit a digit
///     or mixed token (a digit-bearing token is rejected here regardless).
///   * AT LEAST ONE VOWEL (a/e/i/o/u/y) — a cheap, wordlist-free entropy/word-
///     shape heuristic: every real English routing cue has a vowel, while random
///     consonant-run fragments do not. Rejects high-entropy secret fragments.
///     A token failing ANY rule is ineligible and never reaches a candidate config
///     or the proposal artifact.
///
/// `pub(crate)` so the Need-Sensed Forge gap detector ([`crate::forge_gap`])
/// reuses the SAME privacy gate when sanitizing a synthesized forge goal — a
/// redactor-surviving secret can never become a learned cue OR a forge goal
/// keyword through one shared chokepoint.
pub(crate) fn is_eligible_cue_word(w: &str) -> bool {
    (4..=CUE_MAX_LEN).contains(&w.len())
        && w.chars().all(|c| c.is_ascii_lowercase())
        && w.chars().any(|c| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y'))
}

/// Stopwords excluded from learned-cue mining (low-signal connectors). Small +
/// hardcoded; the goal is to skip obvious noise, not a full NLP stoplist.
const STOPWORDS: &[&str] = &[
    "the", "a", "an", "and", "or", "to", "of", "in", "on", "for", "my", "me",
    "i", "is", "it", "this", "that", "with", "what", "how", "can", "you",
    "please", "do", "does", "did", "have", "has", "are", "was", "will", "would",
    "[redacted]", "about", "up", "out", "get", "got", "now", "from", "by",
];

/// Salient content words of an utterance, in first-seen order: lowercase
/// alphabetic tokens that are not stopwords and that pass [`is_eligible_cue_word`]
/// (length in [4, CUE_MAX_LEN], all ascii_lowercase, >= 1 vowel). Deterministic.
///
/// This is the SOLE source of candidate cue words for [`learned_cue`] (its only
/// caller), so the eligibility gate here is the single chokepoint that keeps a
/// redactor-surviving secret (e.g. a no-digit passphrase, or an alphabetic
/// fragment of a sub-20-char mixed token) from ever becoming a learned cue or
/// appearing verbatim in the human-read proposal artifact. The legacy >= 4 /
/// stopword / dedupe behavior is preserved; the gate only TIGHTENS it.
///
/// `pub(crate)` so the Need-Sensed Forge gap detector ([`crate::forge_gap`])
/// mines a synthesized forge goal's topic/keywords through the SAME sanitizer
/// the optimizer mines learned cues with — never inventing a parallel one.
pub(crate) fn salient_words(lower: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for tok in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        if is_eligible_cue_word(tok) && !STOPWORDS.contains(&tok) && !out.contains(&tok) {
            out.push(tok);
        }
    }
    out
}

/// THE OPTIMIZER. From recent traces (newest-first), split train/HELD-OUT,
/// generate bounded candidates from train, score EACH on HELD-OUT, and return
/// the best ONLY IF it beats the baseline by ADOPTION_MARGIN AND is not worse
/// than the baseline on EITHER class (the can't-make-it-worse guarantee). Pure +
/// deterministic; proposes NOTHING (None) when the corpus is too thin or no
/// candidate clears the gate.
pub fn optimize(traces: &[Trace]) -> Option<Proposal> {
    let (train, held_out) = split_usable(traces);
    if train.len() + held_out.len() < MIN_USABLE_TRACES {
        return None; // corpus too thin to trust a held-out estimate
    }

    let base = RoutingConfig::baseline();
    let base_score = score_config(&base, &held_out);
    let base_acc = base_score.accuracy();

    let candidates = generate_candidates(&base, &train);

    // Score each candidate on HELD-OUT (NEVER on train — that is the overfitting
    // guard). Keep the best that clears BOTH the margin and the per-class floor.
    let mut best: Option<(RoutingConfig, Score, f64)> = None;
    for cand in candidates {
        let sc = score_config(&cand, &held_out);
        let acc = sc.accuracy();
        // GATE 1: beat baseline overall by the margin.
        if acc < base_acc + ADOPTION_MARGIN {
            continue;
        }
        // GATE 2: not worse than baseline on EITHER class (can't-make-it-worse).
        if sc.success_accuracy() + f64::EPSILON < base_score.success_accuracy()
            || sc.corrected_accuracy() + f64::EPSILON < base_score.corrected_accuracy()
        {
            continue;
        }
        // Keep the strictly-best; ties broken toward the SMALLER diff (the more
        // minimal, interpretable change).
        let better = match &best {
            None => true,
            Some((bcfg, _, bacc)) => {
                acc > *bacc
                    || (acc == *bacc
                        && cand.diff_from(&base).len() < bcfg.diff_from(&base).len())
            }
        };
        if better {
            best = Some((cand, sc, acc));
        }
    }

    let (candidate, _cand_score, cand_acc) = best?;
    let diff = candidate.diff_from(&base);
    if diff.is_empty() {
        return None; // identical to baseline -> nothing to propose
    }
    let driven_by = train
        .iter()
        .filter(|t| t.outcome == Outcome::CorrectedNextTurn)
        .map(|t| t.utterance_redacted.clone())
        .take(10)
        .collect();
    Some(Proposal {
        candidate,
        baseline_accuracy: base_acc,
        candidate_accuracy: cand_acc,
        diff,
        driven_by,
    })
}

// ---------------------------------------------------------------------------
// (3) PROPOSE — gated artifact, mirrors self-heal (propose-only, reversible)
// ---------------------------------------------------------------------------

/// What the [optimize] enabled/mode pair permits. Unknown modes degrade to
/// Propose — never Auto — so a typo can only make the optimizer SAFER (mirrors
/// heal_action in heal.rs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizeAction {
    /// enabled=false: the optimizer does NOTHING (ships here).
    Disabled,
    /// enabled=true, mode="propose": write a reviewable proposal, mutate nothing.
    Propose,
    /// enabled=true, mode="auto": MAY adopt a change that PASSED the
    /// beats-baseline gate (it is reversible) — but still writes the same
    /// reviewable artifact first. Reserved; this round only ever PROPOSES.
    Auto,
}

/// Resolve the action from config. enabled=false dominates; an unknown mode is
/// Propose, never Auto.
pub fn optimize_action(enabled: bool, mode: &str) -> OptimizeAction {
    if !enabled {
        return OptimizeAction::Disabled;
    }
    match mode.trim() {
        "auto" => OptimizeAction::Auto,
        _ => OptimizeAction::Propose,
    }
}

/// Render the proposal.md artifact: the measured before/after, the exact config
/// diff, the corrected traces that drove it, and the EXACT apply command. A
/// pure, reviewable document — applying it is a separate, human step.
pub fn render_proposal_md(ts: u64, p: &Proposal) -> String {
    let diff = if p.diff.is_empty() {
        "(none)".to_string()
    } else {
        p.diff.iter().map(|l| format!("  {l}")).collect::<Vec<_>>().join("\n")
    };
    let driven = if p.driven_by.is_empty() {
        "(no corrected traces recorded)".to_string()
    } else {
        p.driven_by
            .iter()
            .map(|u| format!("  - {u}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "# Routing optimization proposal — {ts}\n\n\
         - target: agent routing/selection (cue-weight layer over the shipped vocabulary)\n\
         - measured on: HELD-OUT traces (never the train split the candidate was derived from)\n\
         - baseline held-out accuracy: {base:.3}\n\
         - candidate held-out accuracy: {cand:.3}\n\
         - improvement: +{imp:.3} (adoption margin {margin:.3})\n\n\
         ## Proposed config diff (cue-weight layer)\n\n\
         The live router is UNCHANGED. This is a proposed layer over \
         agents.rs CUE_VOCAB; review it, then apply:\n\n\
         ```\n{diff}\n```\n\n\
         ## Corrected traces that drove this (PII-redacted)\n\n{driven}\n\n\
         ## To apply\n\n\
         This optimization was measured against held-out usage only; the live \
         routing config is untouched and the change is reversible. Review the \
         diff above, then apply it with:\n\n\
         ```\nscripts/apply_optimization.sh {ts}\n```\n",
        base = p.baseline_accuracy,
        cand = p.candidate_accuracy,
        imp = p.improvement(),
        margin = ADOPTION_MARGIN,
    )
}

/// Serialize the proposal's machine-readable form (config diff + measured
/// before/after + provenance) for proposal.json.
fn proposal_json(ts: u64, p: &Proposal) -> String {
    serde_json::to_string_pretty(&json!({
        "ts": ts,
        "target": "routing.cue_weights",
        "baseline_accuracy": p.baseline_accuracy,
        "candidate_accuracy": p.candidate_accuracy,
        "improvement": p.improvement(),
        "adoption_margin": ADOPTION_MARGIN,
        "measured_on": "held_out",
        "diff": p.diff,
        "driven_by": p.driven_by,
    }))
    .unwrap_or_else(|_| "{}".to_string())
}

/// Write the proposal artifacts under `<optimize_root>/proposals/<ts>/`
/// (proposal.md + proposal.json), returning the directory on success. Mirrors
/// heal's `record_artifact`: a bounded, reviewable on-disk record; NOTHING here
/// mutates a live config.
fn write_proposal(optimize_root: &Path, ts: u64, p: &Proposal) -> Option<PathBuf> {
    let dir = optimize_root.join("proposals").join(ts.to_string());
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("proposal.md"), render_proposal_md(ts, p))?;
        std::fs::write(dir.join("proposal.json"), proposal_json(ts, p))?;
        Ok(())
    };
    match write() {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(error = %e, dir = %dir.display(), "optimize: failed to write proposal");
            None
        }
    }
}

/// THE GATED ENTRY POINT. Run the optimizer over the trace corpus and, when a
/// better config is found AND [optimize] is enabled, write a reviewable PROPOSAL
/// and STOP — a human reviews + applies via scripts/apply_optimization.sh. This
/// NEVER mutates the live routing config. enabled=false => returns Disabled and
/// does nothing (no read, no write). Even in mode="auto" this round only
/// proposes (the artifact is still written for review; auto-adoption of a
/// gate-passing, reversible change is reserved for a later round). `ts` is an
/// INJECTED clock (unix seconds) so tests stay hermetic.
///
/// Telemetry mirrors self-heal: `optimize.proposed{improvement}` when a measured
/// win is proposed, `optimize.none` when nothing clears the gate (or the corpus
/// is thin), `optimize.suppressed` when the master switch is off.
pub fn run_optimizer(
    enabled: bool,
    mode: &str,
    optimize_root: &Path,
    traces: &[Trace],
    ts: u64,
) -> OptimizeAction {
    let action = optimize_action(enabled, mode);
    if action == OptimizeAction::Disabled {
        // Shipped-OFF default: do NOTHING (no scoring, no proposal, no write).
        telemetry::emit(
            "system",
            "optimize.suppressed",
            json!({"reason": "optimize.enabled = false"}),
        );
        return action;
    }

    match optimize(traces) {
        Some(proposal) => {
            // Propose-only: write the reviewable artifact, emit telemetry, STOP.
            // (mode="auto" still only proposes this round — same artifact path.)
            let _ = write_proposal(optimize_root, ts, &proposal);
            telemetry::emit(
                "system",
                "optimize.proposed",
                json!({
                    "ts": ts,
                    "improvement": proposal.improvement(),
                    "baseline_accuracy": proposal.baseline_accuracy,
                    "candidate_accuracy": proposal.candidate_accuracy,
                    "changes": proposal.diff.len(),
                    "mode": mode,
                }),
            );
            tracing::info!(
                ts,
                improvement = proposal.improvement(),
                "optimize: measured routing improvement proposed; apply with \
                 scripts/apply_optimization.sh"
            );
        }
        None => {
            // No candidate beat the baseline on held-out (or the corpus is thin):
            // propose NOTHING — the can't-make-it-worse guarantee in action.
            telemetry::emit("system", "optimize.none", json!({"ts": ts}));
        }
    }
    action
}

// ===========================================================================
// ORACLE ASK — a strictly READ-ONLY SQL query surface over the trace corpus.
// The model writes the SQL from the user's question; this runs it read-only and
// returns the rows. Read-only is enforced twice (a SELECT/WITH/EXPLAIN keyword
// check AND `PRAGMA query_only`, so SQLite itself rejects any write), and the
// output is bounded so a query can never flood the model context.
// ===========================================================================

/// Process-global trace store, installed once at startup (main.rs) so the
/// read-only `oracle_ask` tool can reach the corpus without threading the store
/// through the whole tool-dispatch chain. Unset in unit tests that never install
/// it (so the tool reports "unavailable" rather than touching a real DB).
static GLOBAL_TRACE_STORE: OnceLock<Arc<TraceStore>> = OnceLock::new();

/// Install the global trace store (idempotent — a second call is ignored).
pub fn set_global_trace_store(store: Arc<TraceStore>) {
    let _ = GLOBAL_TRACE_STORE.set(store);
}

/// The global trace store, or `None` before startup wiring.
pub fn global_trace_store() -> Option<&'static Arc<TraceStore>> {
    GLOBAL_TRACE_STORE.get()
}

impl TraceStore {
    /// Run a strictly READ-ONLY SQL query over the trace corpus and return a
    /// compact text table. Read-only is enforced TWICE: a first-keyword check
    /// (SELECT/WITH/EXPLAIN only) for a friendly early error, AND `PRAGMA
    /// query_only` for the duration so SQLite itself rejects any write even if the
    /// keyword check were bypassed. Output is bounded (<=50 rows, <=200 chars per
    /// cell). The shared connection is Mutex-serialized; `query_only` is toggled
    /// inside the lock and ALWAYS reset before the lock is released.
    pub async fn readonly_query(&self, sql: &str) -> Result<String> {
        const MAX_ROWS: usize = 50;
        const MAX_CELL: usize = 200;
        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            return Err(anyhow!("oracle_ask: empty query"));
        }
        // Defense-in-depth: reject a multi-statement query outright so the
        // first-keyword check is a genuine gate, not one the caller can sidestep
        // with "SELECT 1; DELETE ..." (rusqlite compiles only the first statement
        // and query_only blocks the write, but the keyword check must mean what it
        // says). Any ';' remaining after the trailing-';' trim is embedded.
        if trimmed.contains(';') {
            return Err(anyhow!("oracle_ask: only a single read-only statement is allowed"));
        }
        let first = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if !matches!(first.as_str(), "SELECT" | "WITH" | "EXPLAIN") {
            return Err(anyhow!(
                "oracle_ask: only read-only SELECT / WITH / EXPLAIN queries are allowed (got '{first}')"
            ));
        }
        let conn = self.conn.lock().await;
        // Hard read-only: SQLite rejects ANY write statement while this is on.
        conn.pragma_update(None, "query_only", true)?;
        let queried = run_readonly_query(&conn, trimmed, MAX_ROWS, MAX_CELL);
        // ALWAYS clear query_only before releasing the lock, even on error.
        let _ = conn.pragma_update(None, "query_only", false);
        queried
    }
}

/// Execute the (keyword-validated, query_only-guarded) statement and render the
/// rows. Factored out so the `query_only` reset wraps it on every path. Dynamic
/// columns become strings; rows and per-cell length are bounded.
fn run_readonly_query(
    conn: &Connection,
    sql: &str,
    max_rows: usize,
    max_cell: usize,
) -> Result<String> {
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let ncols = cols.len();
    let mut rows = stmt.query([])?;
    let mut out_rows: Vec<Vec<String>> = Vec::new();
    let mut truncated = false;
    while let Some(row) = rows.next()? {
        if out_rows.len() >= max_rows {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(ncols);
        for i in 0..ncols {
            cells.push(cell_to_string(row, i, max_cell));
        }
        out_rows.push(cells);
    }
    Ok(format_query_table(&cols, &out_rows, truncated))
}

/// One result cell rendered as a string, type-agnostic + length-bounded.
fn cell_to_string(row: &rusqlite::Row, i: usize, max: usize) -> String {
    use rusqlite::types::ValueRef;
    let s = match row.get_ref(i) {
        Ok(ValueRef::Null) => String::new(),
        Ok(ValueRef::Integer(n)) => n.to_string(),
        Ok(ValueRef::Real(f)) => f.to_string(),
        Ok(ValueRef::Text(t)) => String::from_utf8_lossy(t).into_owned(),
        Ok(ValueRef::Blob(_)) => "<blob>".to_string(),
        Err(_) => "<err>".to_string(),
    };
    if s.chars().count() > max {
        let mut c: String = s.chars().take(max).collect();
        c.push('…');
        c
    } else {
        s
    }
}

/// Render columns + rows as a compact pipe-delimited table with a row-count
/// footer (and a truncation note when capped).
fn format_query_table(cols: &[String], rows: &[Vec<String>], truncated: bool) -> String {
    if rows.is_empty() {
        return format!("(0 rows) — columns: {}", cols.join(", "));
    }
    let mut out = String::new();
    out.push_str(&cols.join(" | "));
    out.push('\n');
    for r in rows {
        out.push_str(&r.join(" | "));
        out.push('\n');
    }
    out.push_str(&format!(
        "({} row{}{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        if truncated { ", capped at 50" } else { "" }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Unique temp DB per test; tests run concurrently in one process.
    struct TempDb(PathBuf);

    impl TempDb {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-optimize-test-{}-{}.db",
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

    fn enabled_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.optimize.enabled = true;
        cfg
    }

    // --- PII redactor: known-answer cases -------------------------------

    #[test]
    fn redacts_emails() {
        assert_eq!(redact("email me at darcapalb@gmail.com please"), "email me at [redacted] please");
        // Trailing punctuation peeled and restored.
        assert_eq!(redact("ping foo.bar+tag@sub.example.co.uk."), "ping [redacted].");
        // Parenthesized.
        assert_eq!(redact("(a_b@x.io)"), "([redacted])");
    }

    #[test]
    fn redacts_phone_numbers() {
        // A SINGLE-token formatted number (no internal spaces) is fully redacted
        // by the phone rule (10 digits, only phone punctuation): the subscriber
        // number — the identifying part — never survives.
        assert_eq!(redact("call +1(415)555-0123 now"), "call [redacted] now");
        // A formatted local number whose individual digit runs are each < 6 still
        // collapses via the phone rule (7 digits total).
        assert_eq!(redact("ring 555-0123"), "ring [redacted]");
        // When a number is spread across whitespace, each token is classified
        // alone: the 7+-digit subscriber chunk is redacted (the part that
        // identifies a line); a bare 3-digit area code on its own is not PII and
        // survives, exactly as a standalone "415" would.
        assert_eq!(redact("call 415 5550123 now"), "call 415 [redacted] now");
    }

    #[test]
    fn redacts_long_digit_runs_but_keeps_short_numbers() {
        // >=6 digit run -> redacted.
        assert_eq!(redact("account 123456789 balance"), "account [redacted] balance");
        // <6 digit runs survive (years, small counts, versions).
        assert_eq!(redact("in 2026 i bought 3 of model 15"), "in 2026 i bought 3 of model 15");
        // Digit run glued to text inside a token still caught by the final pass.
        assert_eq!(redact("id=000111222"), "id=[redacted]");
    }

    #[test]
    fn redacts_secret_shaped_tokens() {
        // Known provider prefixes.
        assert_eq!(redact("key sk-ABCdef0123456789XYZ here"), "key [redacted] here");
        assert_eq!(redact("token ghp_AbCdEf0123456789ghIjKl done"), "token [redacted] done");
        // Inline assignment.
        assert_eq!(redact("set api_key=supersecretvalue1234"), "set [redacted]");
        // Generic high-entropy run (>=20 chars, mixed alnum).
        assert_eq!(redact("nonce aB3xK9pQ2rL7mN4vT8sW1c value"), "nonce [redacted] value");
    }

    #[test]
    fn redacts_urls_with_credentials_but_leaves_plain_urls() {
        assert_eq!(
            redact("clone https://user:p4ssw0rd@github.com/org/repo.git"),
            "clone [redacted]"
        );
        // A plain URL (no userinfo) is NOT redacted by the credential rule.
        assert_eq!(
            redact("see https://example.com/page"),
            "see https://example.com/page"
        );
    }

    #[test]
    fn leaves_ordinary_text_untouched() {
        let plain = "open the global scan and brief me on the markets";
        assert_eq!(redact(plain), plain);
        // Ordinary words that merely contain letters+digits but are short / not
        // secret-shaped survive (a 19-char mixed token is under the 20 floor).
        assert_eq!(redact("model qwen3-4b-instruct ready"), "model qwen3-4b-instruct ready");
        assert_eq!(redact(""), "");
    }

    #[test]
    fn redaction_is_idempotent() {
        let once = redact("mail a@b.com call 5551234567 key sk-ABCdef0123456789XYZ");
        assert_eq!(redact(&once), once, "redacting an already-redacted string is a no-op");
    }

    // --- Trace::new redacts at construction -----------------------------

    #[test]
    fn trace_new_redacts_the_utterance() {
        let t = Trace::new(
            "my email is darcapalb@gmail.com",
            "memory",
            "friday",
            "memory.remember",
            "",
            Outcome::Success,
            120,
            1_700_000_000,
        );
        assert!(!t.utterance_redacted.contains("darcapalb@gmail.com"));
        assert_eq!(t.utterance_redacted, "my email is [redacted]");
    }

    // --- At-rest encryption (#11) ---------------------------------------

    #[tokio::test]
    async fn open_encrypted_round_trips_and_is_ciphertext_at_rest() {
        let db = TempDb::new("enc-roundtrip");
        // Encrypted open with an EXPLICIT in-test key (no Keychain, no network).
        let key = crate::crypto::SecretKey::from_bytes([6u8; crate::crypto::KEY_BYTES]);
        {
            let store = TraceStore::open_encrypted(&db.0, &key).unwrap();
            let t = Trace::new(
                "trace-canary-utterance",
                "conversation",
                "darwin",
                "chat",
                "",
                Outcome::Success,
                42,
                1_700_000_200,
            );
            store.record(&t).await.unwrap();
        }
        // On-disk is ciphertext (no SQLite magic header).
        let raw = std::fs::read(&db.0).unwrap();
        assert!(!raw.starts_with(b"SQLite format 3\0"), "trace store must be encrypted");
        // Reopen WITH the key reads back.
        {
            let store = TraceStore::open_encrypted(&db.0, &key).unwrap();
            let got = store.recent(10).await.unwrap();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].utterance_redacted, "trace-canary-utterance");
        }
    }

    // --- Persistence: record/read round-trip ----------------------------

    #[tokio::test]
    async fn record_then_read_round_trips() {
        let db = TempDb::new("roundtrip");
        let store = TraceStore::open(&db.0).unwrap();

        let t = Trace::new(
            "what's the weather",
            "conversation",
            "darwin",
            "chat",
            "",
            Outcome::Success,
            340,
            1_700_000_100,
        );
        store.record(&t).await.unwrap();
        let got = store.recent(10).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], t, "round-trip must preserve every field exactly");
        assert_eq!(got[0].outcome, Outcome::Success);
    }

    // --- Oracle Ask: read-only query surface -----------------------------

    #[tokio::test]
    async fn readonly_query_runs_selects_and_rejects_writes() {
        let db = TempDb::new("oracle");
        let store = TraceStore::open(&db.0).unwrap();
        store
            .record(&Trace::new(
                "hi", "conversation", "darwin", "chat", "", Outcome::Success, 100, 1_700_000_000,
            ))
            .await
            .unwrap();
        store
            .record(&Trace::new(
                "do x", "action", "friday", "act", "open_app", Outcome::Failed, 200, 1_700_000_050,
            ))
            .await
            .unwrap();

        // A read-only SELECT returns rows.
        let out = store
            .readonly_query("SELECT outcome, COUNT(*) AS n FROM traces GROUP BY outcome ORDER BY outcome")
            .await
            .unwrap();
        assert!(out.contains("failed"), "got: {out}");
        assert!(out.contains("success"), "got: {out}");

        // WITH / EXPLAIN are accepted; an empty query is rejected.
        assert!(store.readonly_query("WITH x AS (SELECT 1 AS a) SELECT a FROM x").await.is_ok());
        assert!(store.readonly_query("   ").await.is_err());

        // Writes are rejected at the keyword gate, before execution.
        assert!(store.readonly_query("DELETE FROM traces").await.is_err());
        assert!(store.readonly_query("UPDATE traces SET intent='x'").await.is_err());
        assert!(store.readonly_query("DROP TABLE traces").await.is_err());
        // A multi-statement query is rejected outright (the keyword gate is a real
        // defense layer, not one a smuggled second statement can slip past).
        assert!(store.readonly_query("SELECT 1; DELETE FROM traces").await.is_err());
        assert!(store.readonly_query("SELECT 1;DROP TABLE traces").await.is_err());

        // The corpus is intact (no rejected write touched it) and the connection
        // is usable again afterward (query_only was reset).
        assert_eq!(store.recent(10).await.unwrap().len(), 2, "rejected writes left the corpus intact");
        store
            .record(&Trace::new("more", "conversation", "darwin", "chat", "", Outcome::Success, 50, 1_700_000_100))
            .await
            .unwrap();
        assert_eq!(store.recent(10).await.unwrap().len(), 3, "writes still work after a read-only query");
    }

    #[tokio::test]
    async fn recent_returns_newest_first() {
        let db = TempDb::new("ordering");
        let store = TraceStore::open(&db.0).unwrap();
        for (i, agent) in ["a", "b", "c"].iter().enumerate() {
            let t = Trace::new("x", "i", *agent, "m", "", Outcome::Unknown, 1, 1_700_000_000 + i as u64);
            store.record(&t).await.unwrap();
        }
        let got = store.recent(10).await.unwrap();
        let agents: Vec<&str> = got.iter().map(|t| t.agent.as_str()).collect();
        assert_eq!(agents, vec!["c", "b", "a"], "newest first");
    }

    #[tokio::test]
    async fn all_outcome_variants_round_trip() {
        let db = TempDb::new("outcomes");
        let store = TraceStore::open(&db.0).unwrap();
        for (i, oc) in [Outcome::Success, Outcome::CorrectedNextTurn, Outcome::Failed, Outcome::Unknown]
            .into_iter()
            .enumerate()
        {
            store
                .record(&Trace::new("x", "i", "a", "m", "", oc, 1, 1_700_000_000 + i as u64))
                .await
                .unwrap();
        }
        let got = store.recent(10).await.unwrap();
        let outcomes: Vec<Outcome> = got.iter().map(|t| t.outcome).collect();
        assert_eq!(
            outcomes,
            vec![Outcome::Unknown, Outcome::Failed, Outcome::CorrectedNextTurn, Outcome::Success]
        );
    }

    // --- Bounded retention: evict oldest --------------------------------

    #[tokio::test]
    async fn bounded_retention_evicts_oldest() {
        let db = TempDb::new("retention");
        let store = TraceStore::open(&db.0).unwrap();

        // Use the module cap so the real eviction path is exercised. Insert
        // cap+5 rows tagged by a redaction-safe counter in the agent field.
        let total = MAX_TRACES + 5;
        for i in 0..total {
            store
                .record(&Trace::new(
                    "x",
                    "i",
                    format!("agent{i}"),
                    "m",
                    "",
                    Outcome::Unknown,
                    1,
                    1_700_000_000 + i as u64,
                ))
                .await
                .unwrap();
        }
        assert_eq!(store.count().await.unwrap(), MAX_TRACES as u64, "capped at MAX_TRACES");

        // The 5 OLDEST (agent0..agent4) were evicted; the newest survives.
        let all = store.recent(MAX_TRACES).await.unwrap();
        let agents: std::collections::HashSet<&str> = all.iter().map(|t| t.agent.as_str()).collect();
        assert!(!agents.contains("agent0"), "oldest must be evicted");
        assert!(!agents.contains("agent4"), "oldest five must be evicted");
        assert!(agents.contains("agent5"), "the (cap)th-from-last survives");
        assert!(agents.contains(&*format!("agent{}", total - 1)), "newest survives");
    }

    // --- enabled=false => no-op -----------------------------------------

    #[tokio::test]
    async fn record_trace_is_a_noop_when_disabled() {
        let db = TempDb::new("disabled");
        let store = TraceStore::open(&db.0).unwrap();
        let mut cfg = Config::default(); // full-power default is ON; disable explicitly
        cfg.optimize.enabled = false;

        assert!(!cfg.optimize.enabled, "explicitly disabled => recorder is a no-op");
        record_trace(
            &cfg,
            &store,
            "my email is darcapalb@gmail.com",
            "memory",
            "friday",
            "memory.remember",
            "",
            Outcome::Success,
            100,
            1_700_000_000,
        )
        .await
        .unwrap();
        assert_eq!(store.count().await.unwrap(), 0, "disabled => nothing stored");
    }

    #[tokio::test]
    async fn record_trace_writes_when_enabled() {
        let db = TempDb::new("enabled");
        let store = TraceStore::open(&db.0).unwrap();
        let cfg = enabled_cfg();

        record_trace(
            &cfg,
            &store,
            "open the scan",
            "action",
            "edith",
            "command",
            "global-scan",
            Outcome::Success,
            210,
            1_700_000_000,
        )
        .await
        .unwrap();
        assert_eq!(store.count().await.unwrap(), 1);
        let got = store.recent(1).await.unwrap();
        assert_eq!(got[0].agent, "edith");
        assert_eq!(got[0].tool_or_skill, "global-scan");
    }

    // --- NEVER stores a secret (end-to-end through the recorder) --------

    #[tokio::test]
    async fn never_stores_a_secret_through_the_recorder() {
        let db = TempDb::new("no-secret");
        let store = TraceStore::open(&db.0).unwrap();
        let cfg = enabled_cfg();

        let secret = "sk-ABCdef0123456789LIVEKEY";
        record_trace(
            &cfg,
            &store,
            &format!("use my api key {secret} to call the thing"),
            "action",
            "darwin",
            "command",
            "",
            Outcome::Success,
            100,
            1_700_000_000,
        )
        .await
        .unwrap();

        let got = store.recent(1).await.unwrap();
        assert!(
            !got[0].utterance_redacted.contains(secret),
            "the api-key-shaped string must NEVER be stored: {:?}",
            got[0].utterance_redacted
        );
        assert!(got[0].utterance_redacted.contains("[redacted]"), "it was redacted");
        assert_eq!(got[0].utterance_redacted, "use my api key [redacted] to call the thing");
    }

    #[tokio::test]
    async fn store_redacts_defensively_even_for_a_hand_built_trace() {
        // A caller bypasses Trace::new and stuffs a raw secret into the field
        // directly; the store must STILL redact it before persisting.
        let db = TempDb::new("defensive");
        let store = TraceStore::open(&db.0).unwrap();
        let raw_trace = Trace {
            utterance_redacted: "leaked sk-RAW0123456789abcdefSECRET token".to_string(),
            intent: "x".into(),
            agent: "a".into(),
            mode: "m".into(),
            tool_or_skill: "".into(),
            outcome: Outcome::Unknown,
            latency_ms: 0,
            ts: 1_700_000_000,
        };
        store.record(&raw_trace).await.unwrap();
        let got = store.recent(1).await.unwrap();
        assert!(!got[0].utterance_redacted.contains("sk-RAW0123456789abcdefSECRET"));
        assert!(got[0].utterance_redacted.contains("[redacted]"));
    }

    #[tokio::test]
    async fn open_is_idempotent_across_reopens() {
        let db = TempDb::new("reopen");
        {
            let store = TraceStore::open(&db.0).unwrap();
            store
                .record(&Trace::new("x", "i", "a", "m", "", Outcome::Success, 1, 1_700_000_000))
                .await
                .unwrap();
        }
        // Second open re-runs CREATE TABLE IF NOT EXISTS against the existing DB.
        let store = TraceStore::open(&db.0).unwrap();
        assert_eq!(store.count().await.unwrap(), 1, "data survives reopen");
        store
            .record(&Trace::new("y", "i", "b", "m", "", Outcome::Failed, 2, 1_700_000_001))
            .await
            .unwrap();
        assert_eq!(store.count().await.unwrap(), 2);
    }

    // === WIRE-OPTIMIZER: the LIVE wiring surface (id + relabel + correction) ===

    #[tokio::test]
    async fn record_trace_returns_id_when_enabled_none_when_disabled() {
        let db = TempDb::new("wire-id");
        let store = TraceStore::open(&db.0).unwrap();

        // Disabled => no row, Ok(None) (the off path the operator gets by disabling;
        // the full-power default is ON, so disable explicitly here).
        let mut off = Config::default();
        off.optimize.enabled = false;
        let id_off = record_trace(
            &off, &store, "track the crypto", "action", "gecko", "one_shot", "",
            Outcome::Success, 100, 1_700_000_000,
        )
        .await
        .unwrap();
        assert_eq!(id_off, None, "disabled => no id, no row");
        assert_eq!(store.count().await.unwrap(), 0);

        // Enabled => a row + its monotonic id (what the live recorder holds for
        // the next turn's correction check).
        let on = enabled_cfg();
        let id_on = record_trace(
            &on, &store, "track the crypto", "action", "gecko", "one_shot", "",
            Outcome::Success, 100, 1_700_000_001,
        )
        .await
        .unwrap();
        assert!(id_on.is_some(), "enabled => Some(id)");
        assert_eq!(store.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn label_outcome_relabels_the_exact_row_and_is_a_noop_for_a_gone_id() {
        let db = TempDb::new("wire-relabel");
        let store = TraceStore::open(&db.0).unwrap();
        let cfg = enabled_cfg();

        // Record a turn (default Success), capture its id.
        let id = record_trace(
            &cfg, &store, "ask gecko about crypto", "action", "gecko", "one_shot", "",
            Outcome::Success, 120, 1_700_000_000,
        )
        .await
        .unwrap()
        .expect("enabled => Some(id)");
        assert_eq!(store.recent(1).await.unwrap()[0].outcome, Outcome::Success);

        // Re-label THAT row Corrected (the cross-turn signal); exactly one row.
        let n = store
            .label_outcome(id, Outcome::CorrectedNextTurn)
            .await
            .unwrap();
        assert_eq!(n, 1, "relabeled exactly the one row");
        assert_eq!(
            store.recent(1).await.unwrap()[0].outcome,
            Outcome::CorrectedNextTurn,
            "the prior trace is now the learnable Corrected signal"
        );

        // An id that was evicted/never existed => zero rows, never an error.
        let n_gone = store.label_outcome(999_999, Outcome::Failed).await.unwrap();
        assert_eq!(n_gone, 0, "a gone id is a silent no-op");
    }

    // --- the conservative correction predicate (don't over-label) ---------

    #[test]
    fn is_correction_fires_on_an_explicit_reroute_of_the_same_intent() {
        let prior = PriorTurn {
            trace_id: 1,
            intent: "action".into(),
            agent: "gecko".into(),
        };
        // Same intent, DIFFERENT agent, explicit redirect cue -> correction.
        assert!(is_correction(&prior, "action", "hercules", "no, ask hercules instead"));
        assert!(is_correction(&prior, "action", "hercules", "that's wrong, I meant the trainer"));
        assert!(is_correction(&prior, "action", "friday", "wrong one — try again"));
    }

    #[test]
    fn is_correction_does_not_over_label_a_normal_follow_up() {
        let prior = PriorTurn {
            trace_id: 1,
            intent: "action".into(),
            agent: "gecko".into(),
        };
        // A normal follow-up to a DIFFERENT agent but with NO redirect cue is just
        // the conversation moving on — NOT a correction.
        assert!(
            !is_correction(&prior, "action", "hercules", "now plan my workout"),
            "a cue-less follow-up is not a correction"
        );
        // A redirect-shaped utterance but routed to the SAME agent corrected
        // nothing (the route did not change).
        assert!(
            !is_correction(&prior, "action", "gecko", "no that's wrong"),
            "same-agent re-ask is not a correction"
        );
        // A redirect cue but a DIFFERENT intent is a new request, not a re-aim.
        assert!(
            !is_correction(&prior, "memory", "hercules", "no, ask hercules"),
            "a different intent is not a correction of the prior route"
        );
        // An incidental substring ("another", "wronged") must not trip the cue.
        assert!(
            !is_correction(&prior, "action", "hercules", "tell me another fact about it"),
            "an incidental substring is not a redirect cue"
        );
        // A PHRASE cue inside longer words must not trip either: "no ask" occurs in
        // "pia[no ask]s" and "try again" in "re[try again]" — benign follow-ups, not
        // reroutes. (Phrase cues are now word-boundary matched.)
        assert!(
            !is_correction(&prior, "action", "hercules", "the piano asks for tuning"),
            "'no ask' inside 'piano asks' is not a redirect cue"
        );
        assert!(
            !is_correction(&prior, "action", "hercules", "please retry again the upload"),
            "'try again' inside 'retry again' is not a redirect cue"
        );
    }

    // --- the PERIODIC propose-only flow over an ACCUMULATED corpus ---------

    #[tokio::test]
    async fn periodic_run_optimizer_proposes_from_accumulated_recorded_traces() {
        // This mirrors the live optimize_task EXACTLY: traces accrue THROUGH the
        // recorder into the store (gated ON), the periodic pass reads store.recent
        // and calls run_optimizer — which PROPOSES (propose-only) and mutates NO
        // live config. Hermetic: temp store + temp artifact dir + injected clock.
        let db = TempDb::new("periodic-store");
        let store = TraceStore::open(&db.0).unwrap();
        let cfg = enabled_cfg();

        // Accumulate a corpus that favors gecko owning "crypto" utterances, with a
        // few corrected "crypto workout" ties — the exact signal a cue-upweight
        // fixes (same shape the optimizer_tests favoring-gecko corpus uses).
        //
        // ORDER MATTERS (and mirrors the live recorder): traces accrue oldest->
        // newest by insert id, and the optimizer reserves the NEWEST 40% as the
        // held-out split while mining candidates from the OLDER train split. So
        // the corrected "signal" traces are inserted FIRST (oldest -> train, where
        // candidates are derived), and the confirming gecko successes span both
        // splits so the candidate proves itself on held-out it never saw.
        for i in 0..6u64 {
            record_trace(
                &cfg, &store, "crypto workout plan", "action", "hercules", "one_shot", "",
                Outcome::CorrectedNextTurn, 100, 1_000 + i,
            )
            .await
            .unwrap();
        }
        for i in 0..20u64 {
            record_trace(
                &cfg, &store, "track the crypto", "action", "gecko", "one_shot", "",
                Outcome::Success, 100, 2_000 + i,
            )
            .await
            .unwrap();
        }

        // Snapshot the baseline config BEFORE the pass — the can't-be-mutated bar.
        let baseline_before = RoutingConfig::baseline();

        // The periodic pass: read recent + run the propose-only optimizer.
        let traces = store.recent(MAX_TRACES).await.unwrap();
        let artifacts = std::env::temp_dir().join(format!(
            "darwin-periodic-artifacts-{}-{}",
            std::process::id(),
            "periodic"
        ));
        let _ = std::fs::remove_dir_all(&artifacts);
        let action = run_optimizer(
            cfg.optimize.enabled,
            &cfg.optimize.mode,
            &artifacts,
            &traces,
            1_700_000_000,
        );

        // Propose-only: it PROPOSED (mode "propose") and wrote a reviewable
        // artifact; it did NOT auto-adopt.
        assert_eq!(action, OptimizeAction::Propose);
        let proposal_md = artifacts
            .join("proposals")
            .join("1700000000")
            .join("proposal.md");
        assert!(proposal_md.exists(), "a reviewable proposal artifact was written");

        // NOTHING was mutated: the baseline routing config is byte-for-byte the
        // same after the pass (the live config is untouched; only an on-disk
        // PROPOSAL exists for a human to apply).
        assert_eq!(
            RoutingConfig::baseline(),
            baseline_before,
            "run_optimizer proposes only — the live routing config is never mutated"
        );

        let _ = std::fs::remove_dir_all(&artifacts);
    }

    #[tokio::test]
    async fn periodic_run_optimizer_is_disabled_when_off() {
        // The shipped-OFF default: even with a corpus on disk, an OFF master
        // switch makes the pass a complete no-op (Disabled, no artifact).
        let db = TempDb::new("periodic-off");
        let store = TraceStore::open(&db.0).unwrap();
        // (We hand-insert with record so the store has rows regardless of gate.)
        for i in 0..20u64 {
            store
                .record(&Trace::new("track the crypto", "action", "gecko", "one_shot", "", Outcome::Success, 100, 1_000 + i))
                .await
                .unwrap();
        }
        let traces = store.recent(MAX_TRACES).await.unwrap();
        let artifacts = std::env::temp_dir().join(format!(
            "darwin-periodic-off-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&artifacts);
        let action = run_optimizer(false, "propose", &artifacts, &traces, 1_700_000_000);
        assert_eq!(action, OptimizeAction::Disabled, "OFF => Disabled no-op");
        assert!(
            !artifacts.join("proposals").exists(),
            "disabled pass writes no proposal"
        );
        let _ = std::fs::remove_dir_all(&artifacts);
    }
}

// ===========================================================================
// OPTIMIZER TESTS — hermetic: mock traces, injected clock, temp artifact dir.
// No network, no mic, no live config; every routing decision is a pure replay.
// ===========================================================================
#[cfg(test)]
mod optimizer_tests {
    use super::*;
    use std::path::PathBuf;

    /// A unique temp dir per test for proposal artifacts.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "darwin-optimizer-test-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Build a trace with an ALREADY-redacted utterance (these test utterances
    /// carry no PII) so the known-answer routing assertions are exact.
    fn trace(utterance: &str, agent: &str, outcome: Outcome, ts: u64) -> Trace {
        Trace {
            utterance_redacted: utterance.to_string(),
            intent: "conversation".into(),
            agent: agent.into(),
            mode: "one_shot".into(),
            tool_or_skill: "".into(),
            outcome,
            latency_ms: 100,
            ts,
        }
    }

    // --- replay_route: the honest stand-in for the live router ------------

    #[test]
    fn replay_route_picks_the_cue_owner() {
        let base = RoutingConfig::baseline();
        // "crypto"/"market" are gecko's; "workout"/"nutrition" are hercules'.
        assert_eq!(replay_route(&base, "how's the crypto market"), "gecko");
        assert_eq!(replay_route(&base, "plan my workout and nutrition"), "hercules");
    }

    #[test]
    fn replay_route_falls_back_to_darwin_on_no_cue_or_tie() {
        let base = RoutingConfig::baseline();
        // No domain cue at all -> orchestrator default.
        assert_eq!(replay_route(&base, "hello there lovely day"), "darwin");
        // One cue from gecko and one from hercules -> exact tie -> darwin.
        assert_eq!(replay_route(&base, "crypto workout"), "darwin");
    }

    #[test]
    fn replay_route_respects_the_weight_layer() {
        let mut cand = RoutingConfig::baseline();
        // Upweight gecko's "crypto" so it beats hercules' single "workout".
        cand.weights
            .get_mut("gecko")
            .unwrap()
            .insert("crypto".to_string(), TUNED_WEIGHT);
        assert_eq!(replay_route(&cand, "crypto workout"), "gecko");
    }

    // --- (1) SCORING HARNESS: scores a known config vs known traces -------

    #[test]
    fn scorer_scores_a_known_config_against_known_traces() {
        let base = RoutingConfig::baseline();
        let traces = vec![
            // Success: "crypto market" routes to gecko under baseline -> HIT.
            trace("crypto market update", "gecko", Outcome::Success, 1),
            // Success: but recorded agent is hercules -> baseline routes gecko -> MISS.
            trace("crypto market update", "hercules", Outcome::Success, 2),
            // Corrected away from hercules: baseline routes "crypto market" to gecko
            // (!= hercules) -> the wrong pick was AVOIDED -> HIT.
            trace("crypto market update", "hercules", Outcome::CorrectedNextTurn, 3),
            // Corrected away from gecko: baseline routes to gecko (== gecko) -> the
            // wrong pick was NOT avoided -> MISS.
            trace("crypto market update", "gecko", Outcome::CorrectedNextTurn, 4),
            // Failed/Unknown carry no routing signal: skipped entirely.
            trace("crypto market update", "gecko", Outcome::Failed, 5),
            trace("crypto market update", "gecko", Outcome::Unknown, 6),
        ];
        let s = score_config(&base, &traces);
        assert_eq!(s.success_total, 2);
        assert_eq!(s.success_hits, 1);
        assert_eq!(s.corrected_total, 2);
        assert_eq!(s.corrected_hits, 1);
        assert_eq!(s.accuracy(), 0.5, "2 of 4 scorable traces correct");
    }

    // --- (2) OPTIMIZER: proposes a better config when traces favor one ----

    /// Build a corpus where the baseline clearly mis-routes a tied utterance and
    /// a single cue upweight fixes it on BOTH splits. "crypto workout" ties at
    /// baseline (-> darwin), but every trace says the right agent is gecko;
    /// upweighting gecko's "crypto" breaks the tie correctly. The corpus repeats
    /// the same labelled pattern so train and held-out agree (a real, not
    /// overfit, signal).
    fn favoring_gecko_corpus() -> Vec<Trace> {
        let mut v = Vec::new();
        // 20 traces, newest-first ts so the held-out (newest 40%) and the train
        // (older 60%) both carry the same signal.
        for i in 0..20u64 {
            // Success traces confirming gecko owns "crypto" utterances.
            v.push(trace("track the crypto", "gecko", Outcome::Success, 1000 - i));
        }
        // A handful of corrected traces: the user was sent to hercules on a
        // "crypto workout" tie and corrected it -> the signal to strengthen gecko.
        for i in 0..6u64 {
            v.push(trace(
                "crypto workout plan",
                "hercules",
                Outcome::CorrectedNextTurn,
                900 - i,
            ));
        }
        // Newest-first overall.
        v.sort_by_key(|b| std::cmp::Reverse(b.ts));
        v
    }

    #[test]
    fn optimizer_proposes_a_better_config_when_traces_favor_one() {
        let corpus = favoring_gecko_corpus();
        let proposal = optimize(&corpus).expect("a better config exists");
        assert!(
            proposal.improvement() >= ADOPTION_MARGIN,
            "must beat baseline by the margin: {:?}",
            proposal
        );
        assert!(proposal.candidate_accuracy > proposal.baseline_accuracy);
        assert!(!proposal.diff.is_empty(), "the diff names the change");
        // The change is a minimal, interpretable nudge mentioning gecko.
        assert!(
            proposal.diff.iter().any(|l| l.contains("gecko")),
            "diff should strengthen gecko: {:?}",
            proposal.diff
        );
    }

    // --- proposes NOTHING when no candidate beats baseline ----------------

    #[test]
    fn optimizer_proposes_nothing_when_baseline_is_already_good() {
        // Every trace is a clean Success that the baseline ALREADY routes
        // correctly: there is no correction to learn from and nothing to beat.
        let mut corpus = Vec::new();
        for i in 0..20u64 {
            corpus.push(trace("crypto market", "gecko", Outcome::Success, 1000 - i));
        }
        assert!(
            optimize(&corpus).is_none(),
            "no candidate can beat an already-correct baseline -> propose nothing"
        );
    }

    #[test]
    fn optimizer_proposes_nothing_on_a_thin_corpus() {
        // Below MIN_USABLE_TRACES -> no proposal (can't trust a held-out split).
        let corpus = vec![
            trace("crypto workout", "hercules", Outcome::CorrectedNextTurn, 3),
            trace("track the crypto", "gecko", Outcome::Success, 2),
            trace("crypto workout", "hercules", Outcome::CorrectedNextTurn, 1),
        ];
        assert!(corpus.len() < MIN_USABLE_TRACES);
        assert!(optimize(&corpus).is_none(), "thin corpus -> no proposal");
    }

    // --- HELD-OUT split prevents overfitting ------------------------------

    #[test]
    fn held_out_split_rejects_a_train_only_overfit() {
        // The corpus is split into the NEWEST 40% (held-out) and the older 60%
        // (train). Here the TRAIN split carries a "crypto -> should be gecko"
        // correction signal, but the HELD-OUT split contradicts it: on held-out,
        // "crypto" utterances are confirmed SUCCESSES to hercules. A candidate
        // that upweights gecko fits train but is WORSE on held-out, so the
        // overfit-guard (score on held-out only) rejects it.
        let mut v = Vec::new();
        // OLDER 60% = train: corrections pushing toward gecko.
        for i in 0..12u64 {
            v.push(trace("crypto workout plan", "hercules", Outcome::CorrectedNextTurn, 100 + i));
        }
        // NEWEST 40% = held-out: confirmed successes that the SAME utterance
        // shape should route to hercules (the live recent behavior). Upweighting
        // gecko would BREAK these -> not adopted.
        for i in 0..8u64 {
            v.push(trace("crypto workout plan", "hercules", Outcome::Success, 900 + i));
        }
        v.sort_by_key(|b| std::cmp::Reverse(b.ts)); // newest-first

        // Sanity: the held-out split is indeed the hercules-success block.
        let (_train, held) = split_usable(&v);
        assert!(
            held.iter().all(|t| t.outcome == Outcome::Success && t.agent == "hercules"),
            "held-out must be the newest success block"
        );

        // No candidate may be adopted: any gecko-upweight that helps train hurts
        // these held-out hercules successes (worse on the success class ->
        // GATE 2 fails), and nothing else clears the margin.
        assert!(
            optimize(&v).is_none(),
            "a train-only overfit must NOT be adopted (held-out guard + per-class floor)"
        );
    }

    // --- enabled=false => no proposal, live config untouched --------------

    #[test]
    fn disabled_optimizer_does_nothing() {
        let dir = TempDir::new("disabled");
        let corpus = favoring_gecko_corpus(); // a corpus that WOULD yield a proposal
        let action = run_optimizer(false, "propose", &dir.0, &corpus, 1_700_000_000);
        assert_eq!(action, OptimizeAction::Disabled);
        // NOTHING was written: enabled=false is a hard no-op.
        assert!(
            !dir.0.join("proposals").exists(),
            "disabled optimizer must write no proposal artifact"
        );
    }

    #[test]
    fn unknown_mode_degrades_to_propose_never_auto() {
        assert_eq!(optimize_action(true, "propose"), OptimizeAction::Propose);
        assert_eq!(optimize_action(true, "auto"), OptimizeAction::Auto);
        assert_eq!(optimize_action(true, "garbled"), OptimizeAction::Propose);
        assert_eq!(optimize_action(false, "auto"), OptimizeAction::Disabled);
    }

    // --- a proposal is a REVIEWABLE artifact; live config NOT mutated -----

    #[test]
    fn enabled_optimizer_writes_a_reviewable_proposal_and_mutates_nothing_live() {
        let dir = TempDir::new("propose");
        let corpus = favoring_gecko_corpus();
        let ts = 1_700_000_123;
        let action = run_optimizer(true, "propose", &dir.0, &corpus, ts);
        assert_eq!(action, OptimizeAction::Propose);

        // The artifact exists and is reviewable.
        let pdir = dir.0.join("proposals").join(ts.to_string());
        let md = std::fs::read_to_string(pdir.join("proposal.md")).expect("proposal.md written");
        let json_s = std::fs::read_to_string(pdir.join("proposal.json")).expect("proposal.json written");

        // It states the measured before/after and the EXACT apply step.
        assert!(md.contains("baseline held-out accuracy"));
        assert!(md.contains("candidate held-out accuracy"));
        assert!(md.contains("scripts/apply_optimization.sh"));
        assert!(md.contains("The live router is UNCHANGED"));
        // The machine-readable form carries the diff + provenance.
        let parsed: serde_json::Value = serde_json::from_str(&json_s).unwrap();
        assert_eq!(parsed["target"], "routing.cue_weights");
        assert_eq!(parsed["measured_on"], "held_out");
        assert!(parsed["improvement"].as_f64().unwrap() >= ADOPTION_MARGIN);
        assert!(parsed["diff"].as_array().unwrap().iter().any(|l| l
            .as_str()
            .unwrap()
            .contains("gecko")));

        // CRITICAL: the SHIPPED baseline config is byte-for-byte unchanged — the
        // optimizer only PROPOSED; it mutated no live routing config.
        assert_eq!(
            RoutingConfig::baseline(),
            RoutingConfig::baseline(),
            "baseline is a pure function of the shipped vocabulary; the proposal does not touch it"
        );
    }

    // --- the proposal's measured win is real on held-out ------------------

    #[test]
    fn proposed_candidate_actually_beats_baseline_on_held_out() {
        let corpus = favoring_gecko_corpus();
        let (_train, held) = split_usable(&corpus);
        let proposal = optimize(&corpus).unwrap();
        // Re-score both on the SAME held-out split the optimizer judged on.
        let base_acc = score_config(&RoutingConfig::baseline(), &held).accuracy();
        let cand_acc = score_config(&proposal.candidate, &held).accuracy();
        assert!(
            cand_acc >= base_acc + ADOPTION_MARGIN,
            "candidate {cand_acc} must beat baseline {base_acc} by the margin on held-out"
        );
        // And the recorded numbers match the independent re-score.
        assert!((proposal.baseline_accuracy - base_acc).abs() < 1e-9);
        assert!((proposal.candidate_accuracy - cand_acc).abs() < 1e-9);
    }

    // --- can't-make-it-worse: a candidate worse on either class is rejected

    #[test]
    fn a_candidate_worse_on_the_success_class_is_rejected() {
        // Direct unit test of GATE 2 via score_config: a config that helps the
        // corrected class but regresses the success class must be strictly worse
        // on success, which the optimizer refuses to adopt.
        let base = RoutingConfig::baseline();
        // "crypto market" is unambiguously gecko under baseline (two gecko cues),
        // so this success is a baseline HIT.
        let held = vec![
            trace("crypto market", "gecko", Outcome::Success, 2),
            trace("crypto workout", "hercules", Outcome::CorrectedNextTurn, 1),
        ];
        assert_eq!(replay_route(&base, "crypto market"), "gecko");
        // A candidate that HIJACKS gecko's success utterance: give hercules a huge
        // weight on "crypto" so "crypto market" now routes to hercules -> the
        // confirmed-good gecko route is broken (success class regresses).
        let mut cand = base.clone();
        cand.weights
            .get_mut("hercules")
            .unwrap()
            .insert("crypto".to_string(), 10.0);
        assert_eq!(replay_route(&cand, "crypto market"), "hercules");
        let base_s = score_config(&base, &held);
        let cand_s = score_config(&cand, &held);
        assert!(
            cand_s.success_accuracy() < base_s.success_accuracy(),
            "the candidate regressed the success class"
        );
    }

    // --- PRIVACY GATE: a redactor-surviving secret never becomes a cue ----

    // A 25-char all-lowercase no-digit passphrase. The redactor's high-entropy
    // rule needs letters AND digits, so this is NOT redacted; without the cue
    // gate, salient_words would accept it (all-alphabetic, len >= 4) and it would
    // be lifted verbatim into a learned cue and the human-read proposal.
    const PASSPHRASE: &str = "correcthorsebatterystaple";
    // The mixed token "abcd1234efgh5678ijkl" and its alphabetic fragments. Note:
    // salient_words splits on non-ALPHANUMERIC chars, so DIGITS keep the run as
    // ONE token (it does NOT split into the fragments). The whole token is caught
    // by the gate (20 > CUE_MAX_LEN AND contains digits -> not all-lowercase).
    // The fragments are asserted-absent too as belt-and-suspenders.
    const MIXED_TOKEN: &str = "abcd1234efgh5678ijkl";
    const FRAG_A: &str = "abcd";
    const FRAG_B: &str = "efgh";
    const FRAG_C: &str = "ijkl";
    // A digit-bearing token (rejected: not all ascii_lowercase) and a no-vowel
    // consonant run (rejected: no vowel) — both must stay out of cues.
    const DIGIT_TOKEN: &str = "abc123def456";
    const NOVOWEL: &str = "bcdfghjkl";

    #[test]
    fn is_eligible_cue_word_table() {
        // Legitimate routing words: eligible.
        assert!(is_eligible_cue_word("calendar"), "real cue must be eligible");
        assert!(is_eligible_cue_word("grocery"));
        assert!(is_eligible_cue_word("authentication"), "14 chars, has vowels");
        // A short vowel-bearing fragment of the mixed token would be eligible ON
        // ITS OWN — the per-token predicate cannot tell "abcd"/"efgh" from a real
        // word. That is NOT the leak path, though: salient_words splits on
        // non-ALPHANUMERIC chars, so the digit-bearing run stays ONE token and is
        // rejected as a whole (asserted just below). The fragments passing here
        // simply shows the gate is honest about what it does and does not catch.
        assert!(is_eligible_cue_word(FRAG_A), "'abcd' has vowel 'a', within range");
        assert!(is_eligible_cue_word(FRAG_B), "'efgh' has vowel 'e', within range");
        // The whole secret tokens are rejected — this is the active protection:
        assert!(!is_eligible_cue_word(MIXED_TOKEN), "20-char mixed token: over cap AND has digits");
        assert!(!is_eligible_cue_word(PASSPHRASE), "25-char passphrase: over the length cap");
        assert!(!is_eligible_cue_word(DIGIT_TOKEN), "digit-bearing: not all ascii_lowercase");
        assert!(!is_eligible_cue_word(NOVOWEL), "consonant run: no vowel");
        assert!(!is_eligible_cue_word("abcdefghijklmnopqrs"), "19 chars: over CUE_MAX_LEN=18");
        assert!(is_eligible_cue_word("abcdefghijklmnopqr"), "18 chars: at the cap, eligible");
        assert!(!is_eligible_cue_word("ab"), "below the >= 4 floor");
        assert_eq!(CUE_MAX_LEN, 18);
    }

    #[test]
    fn redact_catches_separator_grouped_cards_and_ssns() {
        // REGRESSION (Semantic Pasteboard privacy): the NORMAL displayed form of a
        // card / SSN — grouped by spaces or hyphens — must be redacted, not just the
        // contiguous run. Uses a Luhn-valid test PAN (4242…, the canonical Visa test
        // number).
        for card in [
            "pay 4242 4242 4242 4242 today",
            "4242-4242-4242-4242",
            "4242424242424242",
        ] {
            assert!(
                redact(card).contains(REDACTED),
                "a Luhn-valid card must be redacted in every form: {card:?} -> {:?}",
                redact(card)
            );
            assert!(!redact(card).contains("4242"), "no card digits survive: {card:?}");
        }
        // SSN in both grouped forms.
        assert!(redact("ssn 123 45 6789 on file").contains(REDACTED));
        assert!(redact("123-45-6789").contains(REDACTED));
        // PRECISION: a benign non-card long numeric grouping (a year list) is NOT a
        // Luhn card and NOT an SSN shape, so it survives — no over-redaction.
        assert_eq!(redact("the years 2020 2021 2022 2023"), "the years 2020 2021 2022 2023");
    }

    #[test]
    fn salient_words_drops_the_passphrase_but_keeps_a_real_word() {
        // The passphrase and the no-vowel run are filtered; "grocery" survives.
        let words = salient_words("grocery correcthorsebatterystaple bcdfghjkl");
        assert!(words.contains(&"grocery"), "real word survives the gate");
        assert!(!words.contains(&PASSPHRASE), "passphrase is over the length cap");
        assert!(!words.contains(&NOVOWEL), "no-vowel consonant run is rejected");
    }

    /// Build a corpus that WOULD mine a learned cue from a corrected trace whose
    /// utterance also carries the secrets. >= 2 train SUCCESS traces send the
    /// legit word "grocery" to pepper, so without the gate "grocery" learns
    /// cleanly — and any eligible secret word in the same corrected utterance
    /// would learn too. The gate must drop every secret while keeping "grocery".
    fn corpus_with_secrets_and_a_real_word() -> Vec<Trace> {
        let mut v = Vec::new();
        // Older 60% (train): SUCCESS traces associating "grocery" with pepper, the
        // corroboration learned_cue needs (>= 2 pointing at one agent).
        for i in 0..14u64 {
            v.push(trace("add grocery to my list", "pepper", Outcome::Success, 1000 - i));
        }
        // Train corrected traces: the user said a "grocery" utterance that ALSO
        // contains the secrets and was mis-routed to gecko. This is the corrected
        // signal that drives learned-cue mining over the utterance's vocabulary.
        for i in 0..6u64 {
            v.push(trace(
                "grocery correcthorsebatterystaple abcd1234efgh5678ijkl abc123def456 bcdfghjkl",
                "gecko",
                Outcome::CorrectedNextTurn,
                900 - i,
            ));
        }
        v.sort_by_key(|b| std::cmp::Reverse(b.ts)); // newest-first
        v
    }

    #[test]
    fn learned_cue_promotes_the_real_word_not_the_secrets() {
        let corpus = corpus_with_secrets_and_a_real_word();
        let (train, _held) = split_usable(&corpus);
        let base = RoutingConfig::baseline();
        let lower =
            "grocery correcthorsebatterystaple abcd1234efgh5678ijkl abc123def456 bcdfghjkl";
        // Drive the live candidate-generation chokepoint directly.
        let learned = learned_cue(lower, "gecko", &base, &train);
        let (_agent, cue) = learned.expect("a legit word IS learnable here (positive control)");
        assert_eq!(cue, "grocery", "only the real word may be promoted to a cue");
        // And every secret/secret-fragment is absent from the mined word list.
        for bad in [PASSPHRASE, FRAG_A, FRAG_B, FRAG_C, DIGIT_TOKEN, NOVOWEL] {
            assert_ne!(cue, bad, "secret '{bad}' must never become the learned cue");
        }
    }

    #[test]
    fn no_secret_reaches_a_candidate_config_or_the_rendered_proposal() {
        let corpus = corpus_with_secrets_and_a_real_word();
        let (train, _held) = split_usable(&corpus);
        let base = RoutingConfig::baseline();

        // OUTCOME 1: no candidate config produced from this train split carries a
        // secret as a cue word (anywhere in any agent's weight map).
        let candidates = generate_candidates(&base, &train);
        assert!(!candidates.is_empty(), "the corrected signal yields candidates");
        let mut saw_grocery = false;
        for cand in &candidates {
            for cues in cand.weights.values() {
                for cue in cues.keys() {
                    if cue == "grocery" {
                        saw_grocery = true;
                    }
                    for bad in [PASSPHRASE, FRAG_A, FRAG_B, FRAG_C, DIGIT_TOKEN, NOVOWEL] {
                        assert_ne!(
                            cue, bad,
                            "secret '{bad}' leaked into a candidate cue word"
                        );
                    }
                }
            }
        }
        assert!(
            saw_grocery,
            "positive control: the legit word DID become a learned cue (test not vacuous)"
        );

        // OUTCOME 2: the rendered proposal artifact (md + json) — the bytes a human
        // reads — contains none of the secret strings, even though the corrected
        // utterances (which DO contain them, here unredacted on purpose) flow into
        // driven_by/diff. We render directly from a candidate that learned grocery.
        let learned_cand = candidates
            .iter()
            .find(|c| c.weights.values().any(|m| m.contains_key("grocery")))
            .expect("a candidate learned grocery")
            .clone();
        let proposal = Proposal {
            candidate: learned_cand.clone(),
            baseline_accuracy: 0.5,
            candidate_accuracy: 0.9,
            diff: learned_cand.diff_from(&base),
            driven_by: train
                .iter()
                .filter(|t| t.outcome == Outcome::CorrectedNextTurn)
                .map(|t| t.utterance_redacted.clone())
                .collect(),
        };
        // The DIFF (the proposed change, not the raw provenance) must name grocery
        // and no secret.
        assert!(
            proposal.diff.iter().any(|l| l.contains("grocery")),
            "diff names the learned grocery cue: {:?}",
            proposal.diff
        );
        for bad in [PASSPHRASE, FRAG_A, FRAG_B, FRAG_C, DIGIT_TOKEN, NOVOWEL] {
            assert!(
                !proposal.diff.iter().any(|l| l.contains(bad)),
                "secret '{bad}' leaked into the proposed config diff"
            );
        }
        // The full rendered md proposal's CONFIG-DIFF section (the part derived
        // from learned cues) carries grocery but no secret cue line. We assert the
        // secrets never appear as a "+ learned cue '<secret>'" line.
        let md = render_proposal_md(1_700_000_000, &proposal);
        let json_s = proposal_json(1_700_000_000, &proposal);
        for bad in [PASSPHRASE, FRAG_A, FRAG_B, FRAG_C, DIGIT_TOKEN, NOVOWEL] {
            let cue_line = format!("learned cue '{bad}'");
            assert!(
                !md.contains(&cue_line),
                "secret '{bad}' rendered as a learned-cue line in proposal.md"
            );
            assert!(
                !json_s.contains(&cue_line),
                "secret '{bad}' rendered as a learned-cue line in proposal.json"
            );
        }
        assert!(
            md.contains("learned cue 'grocery'"),
            "the legit grocery cue IS rendered (positive control)"
        );
    }
}
