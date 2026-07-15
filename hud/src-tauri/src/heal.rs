//! Self-heal GUI apply path — the HUD-Tauri BACKEND side of the human-gated
//! Accept button on the SELF-REPAIR // PROPOSALS modal.
//!
//! SAFETY CONTRACT (do not regress): this is NOT auto-heal. self_heal still
//! ships `enabled = false`. These commands let a human, after REVIEWING the
//! actual diff in the modal and a TWO-STEP confirm in the UI, run the existing
//! gated `scripts/apply_heal.sh <ts> --yes`. That script re-validates the patch
//! (fresh staging copy + `/usr/bin/patch -p1 --batch` + `cargo check` +
//! `cargo test`) and refuses to touch `daemon/src` if anything fails. The
//! `--yes` flag skips ONLY the script's `read -r` prompt — the UI's two-step
//! confirm replaces that keystroke. The re-validation gate is non-negotiable
//! and is owned by the script, not by this process.
//!
//! `ts` arrives from the frontend and is used as a path component, so it is
//! validated to be DIGITS ONLY here (and again in the script) — no traversal.
//! No secrets pass through these commands (there are none in a heal proposal),
//! so nothing here logs or returns secret material.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Serialize;
use tokio::process::Command;

/// Cap on the diff / report text returned to the UI (~200 KB each). A pathological
/// proposal artifact cannot balloon the IPC payload.
const ARTIFACT_CAP_BYTES: usize = 200 * 1024;
/// Cap on the apply-script log tail returned to the UI.
const LOG_TAIL_BYTES: usize = 16 * 1024;

/// The artifacts of a proposal, for HUMAN REVIEW in the modal.
#[derive(Debug, Serialize)]
pub struct HealProposalDetail {
    /// `state/heal/proposals/<ts>/patch.diff`, capped.
    diff: String,
    /// `state/heal/proposals/<ts>/report.md`, capped (empty string if absent —
    /// the diff is the load-bearing artifact; the report is supplementary).
    report: String,
}

/// Result of running the gated apply script.
#[derive(Debug, Serialize)]
pub struct HealApplyResult {
    /// True only when the script exited 0 (every gate green, patch applied,
    /// release rebuilt, marker cleared).
    ok: bool,
    /// The final `STAGE:` the script reached (e.g. "revalidating", "applying",
    /// "rebuilding"), or a terminal label. Lets the UI show where it ended.
    stage: String,
    /// Trimmed tail of stdout+stderr — the structured progress + the terminal
    /// RESULT line. Never contains secrets (a heal proposal has none).
    log: String,
}

/* --------------------------------------------------------------- validation */

/// `ts` must be digits only — it becomes a path component
/// (`state/heal/proposals/<ts>/`). This is the SAME guard the shell script
/// applies; enforcing it here too means a malformed value never even reaches a
/// filesystem path. PURE — unit-tested.
pub fn is_valid_ts(ts: &str) -> bool {
    !ts.is_empty() && ts.bytes().all(|b| b.is_ascii_digit())
}

/// Take the last `cap` bytes of `s` on a char boundary, prefixing an ellipsis
/// marker when truncated. PURE.
pub fn tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    // Walk back from (len - cap) to the next char boundary so we never split a
    // UTF-8 sequence.
    let mut start = s.len() - cap;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…(truncated)\n{}", &s[start..])
}

/// Take the first `cap` bytes of `s` on a char boundary, appending an ellipsis
/// marker when truncated. PURE.
pub fn head(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n…(truncated)", &s[..end])
}

/// Extract the last `STAGE:` token (or the `RESULT:` outcome) from the script's
/// combined output, for the UI's stage label. PURE — unit-tested. Falls back to
/// "done" so the field is never empty.
pub fn last_stage(output: &str) -> String {
    let mut stage = String::from("done");
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("STAGE:") {
            stage = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("RESULT:") {
            // RESULT is terminal — surface "ok" / "failed <reason>".
            return rest.trim().to_string();
        }
    }
    stage
}

/* ------------------------------------------------------------- root resolve */

/// A directory is the DARWIN root iff it contains BOTH `scripts/apply_heal.sh`
/// and `config/darwin.toml`. PURE (filesystem read only). Used by the upward
/// walk and unit-tested with a synthetic tree.
pub fn is_darwin_root(dir: &Path) -> bool {
    dir.join("scripts/apply_heal.sh").is_file() && dir.join("config/darwin.toml").is_file()
}

/// Walk up from `start`, returning the first ancestor that `is_darwin_root`.
/// PURE-ish (filesystem reads). Unit-tested against a synthetic tree so the
/// walk logic is verified without depending on where the test binary lives.
pub fn find_root_from(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if is_darwin_root(dir) {
            return Some(dir.to_path_buf());
        }
        cur = dir.parent();
    }
    None
}

/// The canonical install tree, relative to a given home dir:
/// `<home>/Library/Application Support/DARWIN` — exactly where install.sh places
/// the repo (`DARWIN_HOME="$HOME/Library/Application Support/DARWIN"`). Returns
/// it only when it is actually a DARWIN root. Split from `install_root` so the
/// path logic is unit-testable against a synthetic home WITHOUT mutating the
/// process-global `$HOME`.
fn install_root_under(home: &Path) -> Option<PathBuf> {
    let p = home.join("Library/Application Support/DARWIN");
    if is_darwin_root(&p) {
        Some(p)
    } else {
        None
    }
}

/// The canonical install location of the DARWIN tree on this machine. An
/// INSTALLED app is copied to /Applications/DARWIN.app — OUTSIDE the install
/// tree — so the exe/cwd upward walks below cannot reach it; this is the real
/// production resolution path. GUI apps reliably inherit `$HOME` (launchd/Finder
/// set it), so it needs no shell environment.
fn install_root() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    install_root_under(&PathBuf::from(home))
}

/// Resolve the DARWIN repo root ONCE per call:
///   1. `DARWIN_ROOT` env var when set AND it looks like the root.
///   2. otherwise walk up from the current exe's directory (dev: exe lives in
///      the source tree).
///   3. otherwise the canonical install tree `~/Library/Application Support/
///      DARWIN` (production: the app was copied to /Applications, divorced from
///      the install tree, so the walk above can't find it).
///   4. otherwise walk up from the current working directory (dev fallback).
///
/// Returns a structured error string when nothing qualifies.
fn resolve_darwin_root() -> Result<PathBuf, String> {
    if let Ok(env_root) = std::env::var("DARWIN_ROOT") {
        let p = PathBuf::from(&env_root);
        if is_darwin_root(&p) {
            return Ok(p);
        }
        return Err(format!(
            "DARWIN_ROOT is set ({env_root}) but does not contain scripts/apply_heal.sh + config/darwin.toml"
        ));
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            if let Some(root) = find_root_from(dir) {
                return Ok(root);
            }
        }
    }

    if let Some(root) = install_root() {
        return Ok(root);
    }

    if let Ok(cwd) = std::env::current_dir() {
        if let Some(root) = find_root_from(&cwd) {
            return Ok(root);
        }
    }

    Err("could not locate the DARWIN root (looked at $DARWIN_ROOT, above the app, ~/Library/Application Support/DARWIN, and the working dir for scripts/apply_heal.sh + config/darwin.toml)".to_string())
}

/// Public root resolver for siblings (the command channel) that need the SAME
/// repo root the heal apply uses — the daemon's confined `state/ipc/` socket +
/// token live under it. Reuses the identical DARWIN_ROOT env / exe-cwd upward
/// walk to the scripts/apply_heal.sh + config/darwin.toml markers.
pub fn resolve_root_for_command() -> Result<PathBuf, String> {
    resolve_darwin_root()
}

/// Resolve and validate the proposal directory for `ts`. Centralizes the ts
/// guard + root resolve so both commands share one path.
fn proposal_dir(ts: &str) -> Result<PathBuf, String> {
    if !is_valid_ts(ts) {
        return Err(format!("invalid timestamp '{ts}' (must be digits only)"));
    }
    let root = resolve_darwin_root()?;
    Ok(root.join("state/heal/proposals").join(ts))
}

/* ----------------------------------------------------------------- commands */

/// Read the staged diff + report for `ts`, for HUMAN REVIEW in the modal. The
/// diff is the load-bearing artifact (the human reviews the real code change);
/// the report is supplementary. Each is capped at ~200 KB. A missing patch.diff
/// is a structured error (there is nothing to review); a missing report.md is
/// tolerated as an empty string.
#[tauri::command]
pub async fn heal_proposal_detail(ts: String) -> Result<HealProposalDetail, String> {
    let dir = proposal_dir(&ts)?;
    let patch_path = dir.join("patch.diff");
    let report_path = dir.join("report.md");

    let diff_raw = tokio::fs::read_to_string(&patch_path)
        .await
        .map_err(|e| format!("could not read patch.diff for {ts}: {e}"))?;
    let report_raw = tokio::fs::read_to_string(&report_path)
        .await
        .unwrap_or_default();

    Ok(HealProposalDetail {
        diff: head(&diff_raw, ARTIFACT_CAP_BYTES),
        report: head(&report_raw, ARTIFACT_CAP_BYTES),
    })
}

/// Run the GATED apply script for `ts`. This is the human-gated apply path:
/// the UI already showed the diff and required a two-step confirm; here we
/// spawn `scripts/apply_heal.sh <ts> --yes` (args-only Command — NOT a shell
/// string), which RE-VALIDATES (fresh staging copy + patch + cargo check +
/// cargo test) and refuses to touch daemon/src on any failure.
///
/// Long-running (re-validate + release build); async so it does not block the
/// runtime. We return only the final result — the UI shows a spinner.
#[tauri::command]
pub async fn heal_apply(ts: String) -> Result<HealApplyResult, String> {
    if !is_valid_ts(&ts) {
        return Err(format!("invalid timestamp '{ts}' (must be digits only)"));
    }
    let root = resolve_darwin_root()?;
    let script = root.join("scripts/apply_heal.sh");
    if !script.is_file() {
        return Err(format!(
            "apply script not found at {}",
            script.display()
        ));
    }

    // args-only: ts is already validated digits-only, and it is passed as a
    // discrete argument, never interpolated into a shell string. Run from the
    // repo root so the script's own relative resolution is unambiguous.
    let output = Command::new("/bin/bash")
        .arg(&script)
        .arg(&ts)
        .arg("--yes")
        .current_dir(&root)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("failed to spawn apply script: {e}"))?;

    let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    let ok = output.status.success();
    let stage = last_stage(&combined);
    Ok(HealApplyResult {
        ok,
        stage,
        log: tail(&combined, LOG_TAIL_BYTES),
    })
}

/* --------------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn ts_validation_is_digits_only() {
        assert!(is_valid_ts("1700000000"));
        assert!(is_valid_ts("0"));
        // traversal / injection attempts are all rejected
        assert!(!is_valid_ts(""));
        assert!(!is_valid_ts("../../etc/passwd"));
        assert!(!is_valid_ts("17000/0000"));
        assert!(!is_valid_ts("1700000000 --force"));
        assert!(!is_valid_ts("17a00"));
        assert!(!is_valid_ts(".."));
        assert!(!is_valid_ts("1700000000\n"));
        assert!(!is_valid_ts("1700000000;rm -rf"));
    }

    #[test]
    fn last_stage_tracks_progress_and_result() {
        // mid-flight: last STAGE wins
        let out = "STAGE: revalidating\nsome cargo noise\nSTAGE: applying\nSTAGE: rebuilding\n";
        assert_eq!(last_stage(out), "rebuilding");
        // a terminal RESULT short-circuits to the outcome
        let ok = "STAGE: revalidating\nSTAGE: applying\nSTAGE: rebuilding\nRESULT: ok\n";
        assert_eq!(last_stage(ok), "ok");
        let failed = "STAGE: revalidating\nRESULT: failed cargo check failed in staging\n";
        assert_eq!(last_stage(failed), "failed cargo check failed in staging");
        // empty / no markers -> safe default
        assert_eq!(last_stage(""), "done");
        assert_eq!(last_stage("just some output\n"), "done");
    }

    #[test]
    fn head_and_tail_cap_without_splitting_utf8() {
        let small = "hello";
        assert_eq!(head(small, 200 * 1024), "hello");
        assert_eq!(tail(small, 200 * 1024), "hello");

        // multibyte content larger than the cap must not panic / split a char
        let big = "é".repeat(1000); // 2 bytes each => 2000 bytes
        let h = head(&big, 101);
        assert!(h.starts_with('é'));
        assert!(h.ends_with("…(truncated)"));
        let t = tail(&big, 101);
        assert!(t.starts_with("…(truncated)"));
        assert!(t.ends_with('é'));
    }

    #[test]
    fn is_darwin_root_requires_both_markers() {
        let tmp = std::env::temp_dir().join(format!("heal_root_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("scripts")).unwrap();
        fs::create_dir_all(tmp.join("config")).unwrap();

        // only the script -> not a root
        fs::write(tmp.join("scripts/apply_heal.sh"), "#!/bin/bash\n").unwrap();
        assert!(!is_darwin_root(&tmp));

        // both markers -> a root
        fs::write(tmp.join("config/darwin.toml"), "[self_heal]\n").unwrap();
        assert!(is_darwin_root(&tmp));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_root_from_walks_up_to_the_marker_dir() {
        let base =
            std::env::temp_dir().join(format!("heal_walk_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let nested = base.join("a/b/c/d");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(base.join("scripts")).unwrap();
        fs::create_dir_all(base.join("config")).unwrap();
        fs::write(base.join("scripts/apply_heal.sh"), "#!/bin/bash\n").unwrap();
        fs::write(base.join("config/darwin.toml"), "[self_heal]\n").unwrap();

        let found = find_root_from(&nested).expect("should find the root by walking up");
        // canonicalize both sides: temp_dir may be a symlink (/var -> /private/var)
        assert_eq!(found.canonicalize().unwrap(), base.canonicalize().unwrap());

        // a tree with no markers anywhere yields None
        let orphan = std::env::temp_dir().join(format!("heal_orphan_{}", std::process::id()));
        let _ = fs::remove_dir_all(&orphan);
        fs::create_dir_all(&orphan).unwrap();
        // not asserting None broadly (temp_dir ancestors are unknown); just ensure
        // the immediate orphan dir is not mistaken for a root.
        assert!(!is_darwin_root(&orphan));

        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&orphan);
    }

    #[test]
    fn install_root_under_resolves_the_canonical_app_support_tree() {
        // Synthesize a fake $HOME containing the install tree exactly where
        // install.sh places it, WITHOUT touching the process-global $HOME.
        let home =
            std::env::temp_dir().join(format!("heal_install_home_{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        let tree = home.join("Library/Application Support/DARWIN");
        fs::create_dir_all(tree.join("scripts")).unwrap();
        fs::create_dir_all(tree.join("config")).unwrap();

        // markers absent -> not yet a root
        assert!(install_root_under(&home).is_none());

        // both markers present -> resolves to the install tree
        fs::write(tree.join("scripts/apply_heal.sh"), "#!/bin/bash\n").unwrap();
        fs::write(tree.join("config/darwin.toml"), "[self_heal]\n").unwrap();
        let found = install_root_under(&home).expect("install tree should resolve");
        assert_eq!(found.canonicalize().unwrap(), tree.canonicalize().unwrap());

        // a home with no install tree -> None (never a false positive)
        let empty =
            std::env::temp_dir().join(format!("heal_install_empty_{}", std::process::id()));
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir_all(&empty).unwrap();
        assert!(install_root_under(&empty).is_none());

        let _ = fs::remove_dir_all(&home);
        let _ = fs::remove_dir_all(&empty);
    }
}
