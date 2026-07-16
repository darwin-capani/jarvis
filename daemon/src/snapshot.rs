//! SAFETY SNAPSHOT (snapshot.rs) — APFS restore-point anchoring in the
//! reversible journal.
//!
//! BEFORE a consequential, hard-to-reverse step — a self-heal APPLY (heal.rs
//! auto-apply) or a consequential mission step (the FURY mission engine) —
//! DARWIN takes a bounded, fixed-arg `tmutil localsnapshot` and RECORDS the
//! resulting APFS-snapshot label + timestamp into the reversible journal
//! (journal.rs), so a later "undo that" can name a CONCRETE OS-level restore
//! point ("there's an APFS snapshot from 14:32 you can roll back to").
//!
//! ## Benign-only, armed by default
//!
//! Snapshot CREATION is ADDITIVE-BENIGN: `tmutil localsnapshot` writes and
//! deletes NONE of the user's data (it records a copy-on-write APFS marker;
//! macOS thins local snapshots automatically), so the feature ships ON within
//! the benign-only contract ([snapshot].enabled = true).
//!
//! ## Restore is NEVER automated
//!
//! DARWIN only SURFACES the restore point plus the exact user-run `tmutil`
//! command — it NEVER rolls back on its own. There is NO restore subprocess in
//! this module at all: the only `tmutil` invocation it can ever spawn is the
//! benign `localsnapshot` create; the "restore command" is a plain STRING the
//! user runs themselves.
//!
//! ## Honesty
//!
//! If the OS refuses `localsnapshot` (the volume is not APFS, no free space, or
//! no permission), we report the anchor we WOULD have taken and change nothing —
//! NEVER a fabricated label.
//!
//! ## Testable seam
//!
//! The snapshot-label PARSE and the journal-anchor RECORD are a PURE seam
//! (unit-tested on canned `tmutil` output); the `tmutil` exec is the
//! DEVICE-GATED runner (injected, never spawned under test) — the same
//! discipline as posture.rs / power.rs.

use std::time::Duration;

use serde_json::json;

use crate::config::Config;
use crate::journal;

/// Hard ceiling on the one snapshot subprocess — same bounded-subprocess
/// discipline as posture.rs / power.rs (a fixed program + fixed args, never a
/// shell string; kill_on_drop). `tmutil localsnapshot` is fast (a COW marker)
/// but bound it so a wedged call can never hang the caller.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(30);

/// The snapshot command: an absolute program + one fixed arg (never a shell
/// string), exactly like posture.rs::FDESETUP. This is the ONLY `tmutil`
/// invocation this module ever constructs — a create, never a restore/delete.
const TMUTIL: &str = "/usr/bin/tmutil";
const LOCALSNAPSHOT_ARG: &str = "localsnapshot";

/// WHY a snapshot is being anchored — a FIXED, secret-free label (never a path
/// or user content). Rendered into the journal reason + the telemetry frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Before a self-heal APPLY touches the live daemon/ (heal.rs auto_apply).
    HealApply,
    /// Before a consequential mission step runs (the FURY mission engine).
    MissionStep,
}

impl Reason {
    /// The fixed, human sentence fragment for the journal reason + telemetry.
    /// Secret-free by construction (a compile-time constant per variant).
    pub fn label(self) -> &'static str {
        match self {
            Reason::HealApply => "before a self-heal apply",
            Reason::MissionStep => "before a consequential mission step",
        }
    }
}

/// The captured result of one snapshot subprocess: its combined stdout+stderr
/// text plus whether the command exited successfully. `tmutil` signals a
/// refusal (not APFS / no space / no permission) with a non-zero exit, so the
/// success bit is load-bearing — a fabricated label must never survive it.
pub struct RunOutput {
    pub success: bool,
    pub text: String,
}

/// The honest outcome of an anchor attempt. PURE value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotOutcome {
    /// A real APFS local snapshot was created; `label` is the date token
    /// `tmutil` printed (e.g. "2026-07-15-143200").
    Created { label: String },
    /// The OS refused, or the output was unparseable. We changed nothing;
    /// `reason` is the honest, secret-free "would-have" note. NEVER carries a
    /// fabricated label.
    WouldHave { reason: String },
}

/// What [`anchor_before`] did, returned to the caller (and asserted in tests).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotAnchor {
    /// A real restore point was anchored into the reversible journal.
    Anchored { label: String, ts: String },
    /// The snapshot was refused; nothing was recorded (honest would-have).
    WouldHave { reason: String },
    /// [snapshot].enabled = false — nothing was attempted or spawned.
    Off,
}

// ---------------------------------------------------------------------------
// Pure parse + classify — one source of truth, unit-tested on canned output.
// ---------------------------------------------------------------------------

/// Extract the APFS-snapshot date label from `tmutil localsnapshot` output.
/// The success line is:
///   `Created local snapshot with date: 2026-07-15-143200`
/// PURE — unit-tested on canned text. Returns None when no date-shaped token
/// follows the marker, so a FAILURE can never be mistaken for a real label.
pub fn parse_snapshot_label(out: &str) -> Option<String> {
    const MARKER: &str = "snapshot with date:";
    for line in out.lines() {
        // The tmutil success line is ASCII; requiring ASCII keeps the
        // lowercase byte offset valid in the original line (a non-ASCII line is
        // never the success line anyway).
        if !line.is_ascii() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        let Some(pos) = lower.find(MARKER) else { continue };
        let after = line[pos + MARKER.len()..].trim_start();
        // The date token is digits + dashes (YYYY-MM-DD-HHMMSS); stop at the
        // first other char.
        let token: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
        // Require a date-like shape (>= 3 dashes and some digits) so a stray
        // "date:" with no real token can never pass as a label.
        if token.matches('-').count() >= 3 && token.chars().any(|c| c.is_ascii_digit()) {
            return Some(token);
        }
    }
    None
}

/// The honest, secret-free reason for a refused/garbled snapshot — a FIXED
/// classification of `tmutil`'s failure (never raw output, never a path). PURE.
fn would_have_reason(out: &str) -> String {
    let lower = out.to_lowercase();
    let cause = if lower.contains("not supported")
        || lower.contains("does not support")
        || lower.contains("apfs")
        || lower.contains("hfs")
    {
        "this volume doesn't support APFS local snapshots"
    } else if lower.contains("no space") || lower.contains("not enough") || lower.contains("full") {
        "there isn't enough free space for a snapshot"
    } else if lower.contains("permission")
        || lower.contains("not permitted")
        || lower.contains("denied")
        || lower.contains("privilege")
    {
        "the OS didn't permit the snapshot"
    } else {
        "the snapshot couldn't be created"
    };
    cause.to_string()
}

/// Classify one snapshot run into the honest outcome. A SUCCESSFUL run whose
/// output parses yields Created{label}; anything else — a non-zero exit, or a
/// success with unparseable output — degrades to WouldHave with a fixed reason
/// and NEVER a fabricated label. PURE.
pub fn classify_snapshot(run: &RunOutput) -> SnapshotOutcome {
    if run.success {
        if let Some(label) = parse_snapshot_label(&run.text) {
            return SnapshotOutcome::Created { label };
        }
    }
    SnapshotOutcome::WouldHave { reason: would_have_reason(&run.text) }
}

// ---------------------------------------------------------------------------
// User-facing restore surfacing — a STRING, never a spawned process.
// ---------------------------------------------------------------------------

/// The EXACT user-run command that surfaces this restore point — a plain
/// STRING, never a spawned process. DARWIN never rolls back on its own; the
/// user runs this themselves. Listing the local snapshots is the honest entry
/// point (`tmutil restore` on a live volume is the user's deliberate act), so
/// we name the label and hand back the `tmutil` command for THEM to run. PURE.
pub fn restore_command(label: &str) -> String {
    format!(
        "run `tmutil listlocalsnapshots /` to confirm the {label} snapshot, then restore it \
         yourself via Time Machine or `tmutil restore` — I won't roll back on my own"
    )
}

/// The spoken sentence naming a restore point for the undo path: "There's an
/// APFS snapshot from <ts> …". PURE. Reused by journal::status_line so
/// "undo that" / "what can you undo" can name the concrete OS restore point.
pub fn describe_anchor(anchor: &journal::RestoreAnchor) -> String {
    format!(
        "There's an APFS snapshot from {ts} ({label}), taken {reason}, you can roll back to — {cmd}.",
        ts = anchor.ts,
        label = anchor.label,
        reason = anchor.reason,
        cmd = restore_command(&anchor.label),
    )
}

// ---------------------------------------------------------------------------
// Orchestration — the injected-runner seam (testable) + the public entry.
// ---------------------------------------------------------------------------

/// The testable body: run the (injected) snapshot command, classify it, and —
/// on success — record the restore-point anchor into the reversible journal
/// (so "undo that" can name it) + emit the secret-free `snapshot.anchor`
/// telemetry frame. On a refusal it emits the honest would-have frame and
/// records NOTHING. `now` is injected so the ts is deterministic under test.
/// PURE of any real subprocess — that lives behind `run`.
async fn take_anchor<F, Fut>(reason: Reason, run: F, now: String) -> SnapshotAnchor
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = RunOutput>,
{
    let out = run().await;
    match classify_snapshot(&out) {
        SnapshotOutcome::Created { label } => {
            // Record the OS restore point into the reversible journal so
            // "undo that" can name it. Secret-free (label + ts + fixed reason).
            journal::anchor_restore_point(&label, &now, reason.label());
            crate::telemetry::emit(
                "system",
                "snapshot.anchor",
                json!({
                    "status": "created",
                    "label": label,
                    "ts": now,
                    "for": reason.label(),
                }),
            );
            SnapshotAnchor::Anchored { label, ts: now }
        }
        SnapshotOutcome::WouldHave { reason: why } => {
            crate::telemetry::emit(
                "system",
                "snapshot.anchor",
                json!({
                    "status": "would_have",
                    "reason": why,
                    "for": reason.label(),
                }),
            );
            SnapshotAnchor::WouldHave { reason: why }
        }
    }
}

/// Anchor an APFS restore point BEFORE a consequential step. Reads
/// [snapshot].enabled (armed by default): OFF => a no-op (nothing spawned,
/// nothing recorded). ON => takes the bounded, device-gated `tmutil
/// localsnapshot`, records the restore-point anchor into the journal, and emits
/// the secret-free frame. Additive-benign — it writes/deletes NONE of the
/// user's data. It NEVER rolls back (there is no restore path in this module).
pub async fn anchor_before(reason: Reason, cfg: &Config) -> SnapshotAnchor {
    if !cfg.snapshot.enabled {
        return SnapshotAnchor::Off;
    }
    take_anchor(reason, run_localsnapshot, chrono::Utc::now().to_rfc3339()).await
}

// ---------------------------------------------------------------------------
// Device-gated real runner (NEVER reached in tests — they inject a canned
// runner). Mirrors posture.rs::run_real_command / power.rs::read_power_live.
// ---------------------------------------------------------------------------

/// Spawn the one bounded, fixed-arg `tmutil localsnapshot` with kill_on_drop +
/// a hard timeout, mirroring posture.rs::run_real_command. Captures combined
/// stdout+stderr and the exit-success bit. DEVICE-GATED: reached only through
/// [`anchor_before`] with [snapshot].enabled on, and NEVER under test. A spawn
/// error or timeout reports `success = false` so the classifier degrades to an
/// honest WouldHave, never a fabricated label.
async fn run_localsnapshot() -> RunOutput {
    use tokio::process::Command;
    let mut cmd = Command::new(TMUTIL);
    cmd.arg(LOCALSNAPSHOT_ARG).kill_on_drop(true);
    match tokio::time::timeout(SNAPSHOT_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            RunOutput { success: out.status.success(), text }
        }
        Ok(Err(e)) => RunOutput { success: false, text: format!("tmutil could not run: {e}") },
        Err(_) => RunOutput { success: false, text: "the snapshot timed out".to_string() },
    }
}

// ---------------------------------------------------------------------------
// Tests — fully hermetic. The real `tmutil` subprocess is NEVER spawned: the
// orchestration is driven with an INJECTED canned runner, and the pure
// parse/classify/describe seams are tested directly on hand-written text.
// Tests that touch the process-global journal anchor serialize on
// confirm::PENDING_TEST_LOCK (the crate-wide global-state lock journal uses).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `tmutil localsnapshot` success line (macOS wording).
    const OK_OUT: &str = "Created local snapshot with date: 2026-07-15-143200";

    fn ok(text: &str) -> RunOutput {
        RunOutput { success: true, text: text.to_string() }
    }
    fn fail(text: &str) -> RunOutput {
        RunOutput { success: false, text: text.to_string() }
    }

    // -- the fixed command is a CREATE, never a restore/delete ----------------

    /// The ONLY subprocess this module can construct is `tmutil localsnapshot` —
    /// an absolute program + one fixed arg. Pinned WITHOUT running it. This is
    /// the structural guarantee that RESTORE is never auto-invoked: there is no
    /// restore/delete/rm argv anywhere in the module's command surface.
    #[test]
    fn the_only_command_is_localsnapshot_create_never_restore() {
        assert_eq!(TMUTIL, "/usr/bin/tmutil");
        assert_eq!(LOCALSNAPSHOT_ARG, "localsnapshot");
        for token in [TMUTIL, LOCALSNAPSHOT_ARG] {
            let t = token.to_lowercase();
            assert!(!t.contains("restore"), "no restore in the command surface: {t}");
            assert!(!t.contains("delete"), "no delete in the command surface: {t}");
            assert!(!t.contains("rm"), "no rm in the command surface: {t}");
        }
    }

    // -- pure parse -----------------------------------------------------------

    #[test]
    fn parse_extracts_the_date_label_and_rejects_non_date_tails() {
        assert_eq!(parse_snapshot_label(OK_OUT).as_deref(), Some("2026-07-15-143200"));
        // Leading noise + trailing prose around the token still parses cleanly.
        assert_eq!(
            parse_snapshot_label("tmutil: Created local snapshot with date: 2025-01-02-030405 (ok)")
                .as_deref(),
            Some("2025-01-02-030405"),
        );
        // A marker with no date-shaped token yields None (never a bogus label).
        assert_eq!(parse_snapshot_label("Created local snapshot with date: soon"), None);
        // Unrelated / error output yields None.
        assert_eq!(parse_snapshot_label("tmutil: unable to create snapshot"), None);
        assert_eq!(parse_snapshot_label(""), None);
    }

    // -- pure classify: a failure NEVER yields a label ------------------------

    #[test]
    fn classify_created_on_success_and_would_have_on_failure() {
        // Success + a parseable label -> Created with the exact label.
        assert_eq!(
            classify_snapshot(&ok(OK_OUT)),
            SnapshotOutcome::Created { label: "2026-07-15-143200".to_string() },
        );
        // Non-zero exit (refusal) -> WouldHave, NEVER a fabricated label. Even if
        // the error text somehow echoed a date, a !success run can't be Created.
        match classify_snapshot(&fail("Failed to create snapshot: not permitted 2026-07-15-143200")) {
            SnapshotOutcome::WouldHave { reason } => {
                assert!(reason.contains("permit"), "honest permission reason: {reason}");
            }
            other => panic!("a failed run must degrade to WouldHave, got {other:?}"),
        }
        // Success but UNPARSEABLE output also degrades (never coerced to a label).
        assert!(matches!(
            classify_snapshot(&ok("Created local snapshot with date: (none)")),
            SnapshotOutcome::WouldHave { .. }
        ));
    }

    #[test]
    fn would_have_reason_classifies_the_known_refusals_honestly() {
        assert!(would_have_reason("Volume is not APFS; snapshots unsupported")
            .contains("APFS local snapshots"));
        assert!(would_have_reason("No space left on device").contains("free space"));
        assert!(would_have_reason("Operation not permitted").contains("didn't permit"));
        assert!(would_have_reason("some other tmutil error").contains("couldn't be created"));
    }

    // -- restore surfacing is a STRING, never a process -----------------------

    #[test]
    fn restore_command_is_a_user_run_string_naming_the_label() {
        let s = restore_command("2026-07-15-143200");
        assert!(s.contains("2026-07-15-143200"), "names the label: {s}");
        assert!(s.contains("tmutil restore"), "surfaces the exact user-run command: {s}");
        assert!(s.to_lowercase().contains("won't roll back"), "states DARWIN never rolls back: {s}");
    }

    #[test]
    fn describe_anchor_names_the_restore_point_for_undo() {
        let anchor = journal::RestoreAnchor {
            label: "2026-07-15-143200".to_string(),
            ts: "2026-07-15T14:32:00Z".to_string(),
            reason: Reason::HealApply.label().to_string(),
        };
        let s = describe_anchor(&anchor);
        assert!(s.contains("APFS snapshot"), "{s}");
        assert!(s.contains("2026-07-15T14:32:00Z"), "names the ts: {s}");
        assert!(s.contains("2026-07-15-143200"), "names the label: {s}");
        assert!(s.to_lowercase().contains("roll back"), "{s}");
        assert!(s.contains("tmutil restore"), "surfaces the user-run command: {s}");
    }

    // -- orchestration via an INJECTED canned runner --------------------------

    /// Take the crate-wide global-state lock and clear the journal (incl. the
    /// restore anchor) so the anchor assertions below are hermetic.
    fn anchor_guard() -> std::sync::MutexGuard<'static, ()> {
        let g = crate::confirm::PENDING_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        journal::clear_for_test();
        g
    }

    /// A success anchor records a retrievable restore point into the journal
    /// (so "undo that" can name it) and returns Anchored with the exact label +
    /// the injected ts. The real tmutil is NEVER spawned — the runner is canned.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // PENDING_TEST_LOCK serializes slot tests (crate convention)
    async fn a_success_anchor_is_recorded_and_retrievable_by_undo() {
        let _g = anchor_guard();
        assert!(journal::latest_restore_anchor().is_none(), "clean slate");

        let out = take_anchor(
            Reason::HealApply,
            || std::future::ready(ok(OK_OUT)),
            "2026-07-15T14:32:00Z".to_string(),
        )
        .await;
        assert_eq!(
            out,
            SnapshotAnchor::Anchored {
                label: "2026-07-15-143200".to_string(),
                ts: "2026-07-15T14:32:00Z".to_string(),
            }
        );

        // Retrievable through the journal's undo surface, with the fixed reason.
        let anchor = journal::latest_restore_anchor().expect("anchor recorded");
        assert_eq!(anchor.label, "2026-07-15-143200");
        assert_eq!(anchor.ts, "2026-07-15T14:32:00Z");
        assert_eq!(anchor.reason, "before a self-heal apply");
        // And the undo status line names the concrete restore point.
        assert!(journal::status_line().contains("APFS snapshot"), "surfaced to 'undo': {}", journal::status_line());
    }

    /// A refused snapshot records NOTHING (never a fabricated label) and returns
    /// the honest would-have.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // PENDING_TEST_LOCK serializes slot tests (crate convention)
    async fn a_refused_anchor_records_nothing_and_is_honest() {
        let _g = anchor_guard();
        let out = take_anchor(
            Reason::MissionStep,
            || std::future::ready(fail("Volume is not APFS; snapshots unsupported")),
            "2026-07-15T14:32:00Z".to_string(),
        )
        .await;
        match out {
            SnapshotAnchor::WouldHave { reason } => assert!(reason.contains("APFS")),
            other => panic!("expected WouldHave, got {other:?}"),
        }
        assert!(
            journal::latest_restore_anchor().is_none(),
            "a refused snapshot must anchor no restore point (never a fabricated label)"
        );
    }

    /// [snapshot].enabled = false is a total no-op: nothing spawned, nothing
    /// recorded. (Proves the gate short-circuits before the device runner.)
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // PENDING_TEST_LOCK serializes slot tests (crate convention)
    async fn disabled_is_a_total_noop() {
        let _g = anchor_guard();
        let mut cfg = Config::default();
        assert!(cfg.snapshot.enabled, "[snapshot].enabled ships ON (armed-by-default)");
        cfg.snapshot.enabled = false;
        let out = anchor_before(Reason::HealApply, &cfg).await;
        assert_eq!(out, SnapshotAnchor::Off);
        assert!(journal::latest_restore_anchor().is_none(), "off => nothing recorded");
    }
}
