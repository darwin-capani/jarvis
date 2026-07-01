//! Secret Exposure Scanner — a READ-ONLY sweep of a project folder for
//! accidentally-left credentials (API keys, tokens, private keys, secret-looking
//! assignments). It answers "did I commit a secret?" — the defensive complement
//! to Egress (network) and TCC (permissions), on the source-code vector.
//!
//! SAFETY / HONESTY CONTRACT:
//!   * READ-ONLY. It walks + reads files and reports; it changes nothing, moves
//!     nothing, and never transmits a file anywhere.
//!   * NEVER EMITS A SECRET. Every finding is REDACTED at the source — the output
//!     carries the kind + file:line + a fingerprint (a few leading chars + the
//!     length), NEVER the secret value. The raw match exists only transiently in
//!     memory during the scan and is never logged or returned.
//!   * CONFINED + BOUNDED. It scans only inside the given root (canonical
//!     starts-with check, so a symlink cannot walk it out), skips VCS/build/vendor
//!     dirs and oversized/binary files, and caps the file count + finding count so
//!     a huge tree cannot wedge or flood it.
//!   * HONEST. A match is a LIKELY secret, reported as such; the detectors are
//!     conservative (placeholder values are skipped) but this is a heuristic, not
//!     a proof — it says so.
//!
//! It does NOT default to $HOME (walking a whole home dir is slow + noisy); the
//! caller names the project folder to scan.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Directories never worth scanning (VCS internals, dependency/build output).
const SKIP_DIRS: &[&str] = &[
    ".git", "node_modules", "target", "dist", "build", "vendor", ".venv", "venv", "__pycache__",
    ".next", ".cache", ".gradle", ".idea", "Pods", ".terraform",
];
/// Skip a file larger than this (a leaked secret lives in a small text/config
/// file, not a multi-megabyte blob).
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Bound the walk so a giant tree cannot wedge the scan.
const MAX_FILES: usize = 5000;
/// Bound the report so a pathological file cannot flood the model/HUD context.
const MAX_FINDINGS: usize = 100;

/// One redacted hit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    kind: &'static str,
    /// A safe fingerprint (leading chars + length) — NEVER the secret value.
    redacted: String,
}

/// One hit located in the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Located {
    /// Path relative to the scan root.
    path: String,
    line: usize,
    kind: &'static str,
    redacted: String,
}

/// Scan the project rooted at `root` for likely-exposed secrets and render a
/// redacted report. READ-ONLY. Requires a concrete root (it never walks $HOME).
pub async fn scan(root: &str) -> Result<String> {
    let root = root.trim();
    if root.is_empty() {
        return Err(anyhow!("secret_scan needs a project folder to scan (the 'root' path)"));
    }
    let root_path = PathBuf::from(root);
    let root_canon = root_path
        .canonicalize()
        .map_err(|e| anyhow!("secret_scan: cannot read project root '{}': {e}", root_path.display()))?;
    if !root_canon.is_dir() {
        return Err(anyhow!("secret_scan: '{}' is not a folder", root_canon.display()));
    }

    // The blocking file walk runs off the async worker.
    let found = tokio::task::spawn_blocking(move || walk_and_scan(&root_canon))
        .await
        .map_err(|e| anyhow!("secret_scan: scan task failed: {e}"))?;
    Ok(format_report(&found))
}

/// Recursively walk `root` (confined + bounded), scanning each readable text file
/// for secret patterns. Sync (driven under spawn_blocking). Returns located,
/// already-REDACTED hits.
fn walk_and_scan(root: &Path) -> Vec<Located> {
    let mut out = Vec::new();
    let mut files_seen = 0usize;
    walk_dir(root, root, &mut files_seen, &mut out);
    out
}

fn walk_dir(root: &Path, dir: &Path, files_seen: &mut usize, out: &mut Vec<Located>) {
    if *files_seen >= MAX_FILES || out.len() >= MAX_FINDINGS {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if *files_seen >= MAX_FILES || out.len() >= MAX_FINDINGS {
            return;
        }
        let path = entry.path();
        let Ok( file_type) = entry.file_type() else { continue };
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if SKIP_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk_dir(root, &path, files_seen, out);
        } else if file_type.is_file() {
            *files_seen += 1;
            scan_file(root, &path, out);
        }
        // Symlinks (is_dir/is_file false on the entry's own type) are not followed.
    }
}

/// Scan one file, appending located redacted hits. Confined (the real path must
/// stay under the root), size-capped, and UTF-8-only (a non-text file is skipped).
fn scan_file(root: &Path, path: &Path, out: &mut Vec<Located>) {
    let Ok(canon) = path.canonicalize() else { return };
    if !canon.starts_with(root) {
        return; // a symlink pointing outside the root — do not read it
    }
    let Ok(meta) = std::fs::metadata(&canon) else { return };
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return;
    }
    let Ok(body) = std::fs::read_to_string(&canon) else {
        return; // binary / non-UTF-8 — skip
    };
    let rel = canon.strip_prefix(root).unwrap_or(&canon).to_string_lossy().to_string();
    for (i, line) in body.lines().enumerate() {
        for hit in scan_line(line) {
            out.push(Located { path: rel.clone(), line: i + 1, kind: hit.kind, redacted: hit.redacted });
            if out.len() >= MAX_FINDINGS {
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Detectors (PURE, unit-tested) — each returns already-REDACTED hits.
// ---------------------------------------------------------------------------

/// All secret hits on one line. Pure.
fn scan_line(line: &str) -> Vec<Hit> {
    let mut hits = Vec::new();

    // Private-key PEM header — an unmistakable, high-signal marker.
    if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
        hits.push(Hit { kind: "private-key", redacted: "-----BEGIN … PRIVATE KEY-----".to_string() });
    }
    // Vendor-prefixed tokens: prefix + a run of the given charset, length-gated.
    for (kind, prefix, min_tail, cs) in PREFIXED {
        for tok in scan_prefixed(line, prefix, *min_tail, *cs) {
            hits.push(Hit { kind, redacted: redact(&tok) });
        }
    }
    // Generic secret-looking assignment (KEY/TOKEN/SECRET/PASSWORD = "<value>").
    if let Some(value) = scan_generic_assignment(line) {
        hits.push(Hit { kind: "secret-assignment", redacted: redact(&value) });
    }
    hits
}

/// (kind, literal prefix, min tail length, tail charset). AWS access-key ids,
/// GitHub PATs, Google API keys, Slack tokens, Stripe live keys.
type Charset = fn(char) -> bool;
const PREFIXED: &[(&str, &str, usize, Charset)] = &[
    ("aws-access-key", "AKIA", 16, is_upper_alnum),
    ("github-token", "ghp_", 36, is_alnum),
    ("github-token", "gho_", 36, is_alnum),
    ("github-token", "ghs_", 36, is_alnum),
    ("github-token", "ghu_", 36, is_alnum),
    ("github-token", "ghr_", 36, is_alnum),
    ("google-api-key", "AIza", 35, is_key_char),
    ("slack-token", "xoxb-", 10, is_slack_char),
    ("slack-token", "xoxp-", 10, is_slack_char),
    ("stripe-key", "sk_live_", 20, is_alnum),
];

fn is_alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}
fn is_upper_alnum(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}
fn is_slack_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// Every `prefix<tail>` token on the line whose tail (chars matching `cs`) is at
/// least `min_tail` long. Returns the FULL tokens (prefix included).
fn scan_prefixed(line: &str, prefix: &str, min_tail: usize, cs: Charset) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, _) in line.match_indices(prefix) {
        let rest = &line[idx + prefix.len()..];
        let tail: String = rest.chars().take_while(|c| cs(*c)).collect();
        if tail.len() >= min_tail {
            out.push(format!("{prefix}{tail}"));
        }
    }
    out
}

/// Names (case-insensitive substring) that make an assignment secret-suspicious.
const SECRET_NAME_HINTS: &[&str] =
    &["secret", "token", "password", "passwd", "apikey", "api_key", "access_key", "private_key", "auth"];

/// A generic `NAME = "VALUE"` / `NAME: "VALUE"` where NAME looks secret and VALUE
/// is a long, mixed (letters+digits), non-placeholder quoted string. Returns the
/// VALUE (to be redacted), or None. Conservative to avoid noise.
fn scan_generic_assignment(line: &str) -> Option<String> {
    // Split at the first '=' or ':' — the name is the left, the value the right.
    let (name, rhs) = split_assignment(line)?;
    let lname = name.to_ascii_lowercase();
    if !SECRET_NAME_HINTS.iter().any(|h| lname.contains(h)) {
        return None;
    }
    let value = extract_quoted(rhs)?;
    if !looks_secret_value(&value) {
        return None;
    }
    Some(value)
}

/// Split a line into (name, rhs) at the first top-level `=` or `:`. Only the last
/// path segment of a dotted/indexed name is what SECRET_NAME_HINTS matches, but we
/// return the whole left side and let the caller lower+substring it.
fn split_assignment(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let pos = bytes.iter().position(|&b| b == b'=' || b == b':')?;
    let name = line[..pos].trim();
    let rhs = line[pos + 1..].trim();
    (!name.is_empty() && !rhs.is_empty()).then_some((name, rhs))
}

/// The first single/double-quoted string in `s`, or None.
fn extract_quoted(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'"' || b == b'\'')?;
    let quote = bytes[start];
    let rest = &s[start + 1..];
    let end = rest.as_bytes().iter().position(|&b| b == quote)?;
    Some(rest[..end].to_string())
}

/// A value that looks like a real secret: long enough, MIXED (letters AND digits),
/// and not an obvious placeholder.
fn looks_secret_value(v: &str) -> bool {
    if v.len() < 20 || v.len() > 200 {
        return false;
    }
    let has_alpha = v.chars().any(|c| c.is_ascii_alphabetic());
    let has_digit = v.chars().any(|c| c.is_ascii_digit());
    if !(has_alpha && has_digit) {
        return false;
    }
    let lower = v.to_ascii_lowercase();
    const PLACEHOLDERS: &[&str] =
        &["example", "your", "xxxx", "changeme", "placeholder", "dummy", "sample", "...", "<", "redacted"];
    !PLACEHOLDERS.iter().any(|p| lower.contains(p))
}

/// A SAFE fingerprint of a secret: a few leading chars + the length. NEVER the
/// full value — this is the only representation of a match that leaves this module.
fn redact(secret: &str) -> String {
    let head: String = secret.chars().take(4).collect();
    format!("{head}…({} chars)", secret.chars().count())
}

/// Render the located hits as a compact report, grouped honestly. Pure.
fn format_report(hits: &[Located]) -> String {
    if hits.is_empty() {
        return "No likely-exposed secrets found in that folder. (Read-only heuristic scan — a clean \
                result is reassuring but not a guarantee; I changed nothing.)"
            .to_string();
    }
    let mut out = String::from("Possible exposed secrets (REDACTED — review each):\n");
    for h in hits {
        out.push_str(&format!("  {}:{} — {} [{}]\n", h.path, h.line, h.kind, h.redacted));
    }
    let capped = hits.len() >= MAX_FINDINGS;
    out.push_str(&format!(
        "\n({} finding{}{}. Heuristic + REDACTED — I never show the secret itself; read-only, I \
         changed nothing. Rotate anything real and remove it from the repo.)",
        hits.len(),
        if hits.len() == 1 { "" } else { "s" },
        if capped { " (capped)" } else { "" },
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_vendor_prefixed_tokens_redacted() {
        let aws = scan_line("aws_key = AKIAIOSFODNN7EXAMPLE1");
        assert!(aws.iter().any(|h| h.kind == "aws-access-key"));
        // The full secret NEVER appears in the output.
        assert!(aws.iter().all(|h| !h.redacted.contains("AKIAIOSFODNN7EXAMPLE1")));
        assert!(aws.iter().any(|h| h.redacted.starts_with("AKIA…(")));

        let gh = scan_line("token: ghp_0123456789abcdefghijklmnopqrstuvwxyz");
        assert!(gh.iter().any(|h| h.kind == "github-token"));

        let gk = scan_line("key=AIzaSyA1234567890abcdefghijklmnopqrstuvw");
        assert!(gk.iter().any(|h| h.kind == "google-api-key"));
    }

    #[test]
    fn detects_private_key_header() {
        let h = scan_line("-----BEGIN RSA PRIVATE KEY-----");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].kind, "private-key");
        assert!(!h[0].redacted.contains("MII"));
    }

    #[test]
    fn generic_assignment_needs_a_secret_name_and_real_value() {
        // Secret-named + long mixed value -> flagged.
        let hit = scan_line("API_TOKEN = \"a1b2c3d4e5f6g7h8i9j0k1l2\"");
        assert!(hit.iter().any(|h| h.kind == "secret-assignment"));
        // A non-secret name is ignored even with a long value.
        assert!(scan_line("greeting = \"a1b2c3d4e5f6g7h8i9j0k1l2\"").is_empty());
        // A placeholder value is ignored.
        assert!(scan_line("api_key = \"your-api-key-here-000\"").is_empty());
        // A short value is ignored.
        assert!(scan_line("secret = \"abc123\"").is_empty());
        // A value with no digits (likely prose, not a key) is ignored.
        assert!(scan_line("password = \"correcthorsebatterystaple\"").is_empty());
    }

    #[test]
    fn redact_never_reveals_the_secret() {
        let r = redact("AKIAIOSFODNN7EXAMPLE1");
        assert_eq!(r, "AKIA…(21 chars)");
        assert!(!r.contains("IOSFODNN7EXAMPLE1"));
    }

    #[test]
    fn clean_lines_produce_no_hits() {
        assert!(scan_line("let x = 5; // ordinary code").is_empty());
        assert!(scan_line("url = \"https://example.com/path\"").is_empty());
        assert!(scan_line("").is_empty());
    }

    #[test]
    fn report_is_honest_when_empty_and_when_populated() {
        assert!(format_report(&[]).contains("No likely-exposed secrets"));
        let hits = vec![Located {
            path: "src/config.rs".into(),
            line: 12,
            kind: "aws-access-key",
            redacted: "AKIA…(20 chars)".into(),
        }];
        let out = format_report(&hits);
        assert!(out.contains("src/config.rs:12 — aws-access-key [AKIA…(20 chars)]"));
        assert!(out.contains("REDACTED"));
        assert!(out.contains("changed nothing"));
    }

    #[test]
    fn resolve_confinement_predicate_holds() {
        // A path that canonicalizes outside the root is rejected by scan_file's
        // starts_with guard; here we assert the underlying predicate directly.
        assert!(Path::new("/proj/src/a.rs").starts_with(Path::new("/proj")));
        assert!(!Path::new("/etc/passwd").starts_with(Path::new("/proj")));
    }
}
