//! Cartographer — a READ-ONLY crash/error → source mapper.
//!
//! Paste a stack trace or error dump and Cartographer maps every cited frame
//! back onto the terrain of your code: it parses frames across languages
//! (Rust, Python, JavaScript/TypeScript, Go, Java/Kotlin, and a generic
//! `path:line[:col]` form), resolves each frame against a project root
//! (CONFINED — never escapes the root via `..` or an out-of-root absolute
//! path), reads a small window of source around each cited line, flags which
//! frames live in your project vs. a library, and names the most likely
//! culprit (the first in-project frame).
//!
//! It is the read-only CORE of the Cartographer: it CHANGES NOTHING — it only
//! reads + reports (the same discipline as posture.rs / egress.rs). The
//! fix-drafting layer (repurposing heal.rs's stage→validate→propose pipeline
//! onto user code) is a deliberate, separate follow-on; this module has no
//! consequential surface and never mutates a single file.

use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};

/// Source-file extensions we recognize in a `path:line` reference. Anything not
/// ending in one of these is treated as not-a-source-frame (so a bare `12:30`
/// timestamp or a `host:port` never masquerades as a frame).
const KNOWN_EXTS: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "mjs", "cjs", "go", "java", "kt", "kts", "c", "cc",
    "cpp", "cxx", "h", "hpp", "hh", "rb", "swift", "cs", "php", "scala", "m", "mm", "lua", "ex",
    "exs", "dart", "zig",
];

/// Lines of source shown on EACH side of a cited line.
const CONTEXT: usize = 4;
/// Hard cap on frames parsed from one trace (a runaway trace can't flood us).
const MAX_FRAMES: usize = 40;
/// Cap on frames we actually open + excerpt (the rest are still listed).
const MAX_WINDOWS: usize = 15;
/// Refuse to read a "source" file larger than this (something is wrong; a real
/// source frame points at a human-edited file).
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// One frame parsed from a stack trace.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    /// The file path exactly as it appeared in the trace.
    file: String,
    line: u32,
    col: Option<u32>,
    /// The function/method symbol, when the trace format carried one.
    symbol: Option<String>,
    /// Which parser matched ("rust"/"python"/"node"/"java"/"generic").
    lang: &'static str,
}

/// A frame after resolution against the project root.
#[derive(Debug, Clone)]
struct Mapped {
    frame: Frame,
    /// True when the frame resolved to a readable file INSIDE the root.
    in_project: bool,
    /// The excerpt around the cited line (the line marked with `>`), or None
    /// when the file could not be confined/read.
    window: Option<String>,
}

/// Map a stack trace onto source. READ-ONLY: parses the trace, resolves frames
/// against `root` (or `$HOME` when `root` is None), reads bounded windows, and
/// renders the map. Confined — a frame pointing outside the root is listed but
/// never read. Returns a friendly message (never a panic) when nothing parses.
pub async fn map_trace(trace: &str, root: Option<&str>) -> Result<String> {
    let root_path = match root {
        Some(r) if !r.trim().is_empty() => PathBuf::from(r.trim()),
        _ => {
            let home = std::env::var("HOME")
                .map_err(|_| anyhow!("cartographer: HOME is not set and no root was given"))?;
            PathBuf::from(home)
        }
    };
    // Canonicalize the root once so the per-frame confinement check is lexical
    // against a real, normalized base (belt-and-braces with resolve_in_root).
    let root_canon = root_path
        .canonicalize()
        .map_err(|e| anyhow!("cartographer: project root '{}' is unreadable: {e}", root_path.display()))?;

    let frames = parse_frames(trace);
    if frames.is_empty() {
        return Ok(
            "No source frames found in that text. Cartographer maps stack traces that cite \
             `file.ext:line` (Rust/JS/Go/...), `File \"x.py\", line N` (Python), or \
             `at pkg.Class(File.java:N)` (Java/Kotlin)."
                .to_string(),
        );
    }

    let headline = error_headline(trace);
    let mut mapped: Vec<Mapped> = Vec::with_capacity(frames.len());
    let mut windows_used = 0usize;
    for frame in frames {
        let resolved = resolve_in_root(&frame.file, &root_canon);
        let window = match resolved {
            Some(path) if windows_used < MAX_WINDOWS => match read_window(&path, frame.line, &root_canon) {
                Some(w) => {
                    windows_used += 1;
                    Some(w)
                }
                None => None,
            },
            _ => None,
        };
        mapped.push(Mapped {
            in_project: window.is_some(),
            window,
            frame,
        });
    }

    Ok(render_map(&headline, &mapped, &root_canon))
}

// ---------------------------------------------------------------------------
// Parsing (pure, unit-tested)
// ---------------------------------------------------------------------------

/// Parse frames from a trace, in stack order (top frame first), deduplicated by
/// (file, line) and capped at MAX_FRAMES. Each line is tried against the
/// language-specific shapes, then the generic `path:line[:col]` matcher.
fn parse_frames(trace: &str) -> Vec<Frame> {
    let mut out: Vec<Frame> = Vec::new();
    for line in trace.lines() {
        let Some(frame) = parse_line(line) else { continue };
        // Dedup by (file, line): the same site re-appearing (e.g. a recursive
        // frame) collapses to its first occurrence.
        if out.iter().any(|f| f.file == frame.file && f.line == frame.line) {
            continue;
        }
        out.push(frame);
        if out.len() >= MAX_FRAMES {
            break;
        }
    }
    out
}

/// Try every shape on one line; return the first frame found.
fn parse_line(line: &str) -> Option<Frame> {
    parse_python(line)
        .or_else(|| parse_java(line))
        .or_else(|| parse_generic(line))
}

/// Python: `  File "/path/app.py", line 42, in handler`.
fn parse_python(line: &str) -> Option<Frame> {
    let after = line.trim_start().strip_prefix("File \"")?;
    let (file, rest) = after.split_once('"')?;
    if !looks_like_source(file) {
        return None;
    }
    let rest = rest.trim_start_matches([',', ' ']);
    let rest = rest.strip_prefix("line ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let line_no = digits.parse::<u32>().ok()?;
    // Optional "in <symbol>" tail.
    let symbol = rest
        .split_once(", in ")
        .map(|(_, s)| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(Frame {
        file: file.to_string(),
        line: line_no,
        col: None,
        symbol,
        lang: "python",
    })
}

/// Java/Kotlin: `\tat com.foo.Bar.method(Bar.java:42)`. The `path:line` lives
/// inside the parentheses; the symbol is the dotted name before the `(`.
fn parse_java(line: &str) -> Option<Frame> {
    let t = line.trim_start();
    let t = t.strip_prefix("at ")?;
    let open = t.find('(')?;
    let close = t.find(')')?;
    if close <= open + 1 {
        return None;
    }
    let inner = &t[open + 1..close];
    let (file, ln, col) = parse_pathline(inner)?;
    let symbol = t[..open].trim();
    Some(Frame {
        file,
        line: ln,
        col,
        symbol: (!symbol.is_empty()).then(|| symbol.to_string()),
        lang: "java",
    })
}

/// Generic / Rust / Node / Go: find the first whitespace-or-paren-delimited
/// token on the line that parses as `path:line[:col]` with a known source ext.
/// Captures a `<symbol> (…)` Node-style name when one precedes the token.
fn parse_generic(line: &str) -> Option<Frame> {
    // Split on whitespace AND parentheses/brackets so `at fn (a.js:1:2)` yields
    // the bare `a.js:1:2` token.
    let tokens: Vec<&str> = line
        .split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']'))
        .filter(|t| !t.is_empty())
        .collect();
    let (idx, (file, ln, col)) = tokens
        .iter()
        .enumerate()
        .find_map(|(i, tok)| parse_pathline(tok).map(|p| (i, p)))?;
    // Node frames read `at <symbol> (path:line:col)`: the symbol is the token
    // right before the path when the line begins with `at`.
    let symbol = if line.trim_start().starts_with("at ") && idx >= 1 {
        let s = tokens[idx - 1];
        (s != "at").then(|| s.to_string())
    } else {
        None
    };
    Some(Frame {
        file,
        line: ln,
        col,
        symbol,
        lang: "generic",
    })
}

/// Parse a `path:line` or `path:line:col` token (after stripping wrapping
/// punctuation), requiring the path to end in a known source extension. None
/// when it is not a source-frame reference (a `host:port`, a `HH:MM` clock, a
/// non-source file). Unix-style paths only (a Windows `C:\` drive letter is not
/// a source frame in our traces).
fn parse_pathline(raw: &str) -> Option<(String, u32, Option<u32>)> {
    let tok = raw.trim_matches(|c: char| "()[]{}\"'`,;:".contains(c));
    let (head, last) = tok.rsplit_once(':')?;
    if last.is_empty() || !last.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    // `path:line:col` — the middle field is the line, `last` is the column.
    if let Some((path, mid)) = head.rsplit_once(':') {
        if !mid.is_empty() && mid.chars().all(|c| c.is_ascii_digit()) && looks_like_source(path) {
            return Some((path.to_string(), mid.parse().ok()?, last.parse().ok()));
        }
    }
    // `path:line`.
    if looks_like_source(head) {
        return Some((head.to_string(), last.parse().ok()?, None));
    }
    None
}

/// True when `path`'s extension is one we recognize as source (case-insensitive).
fn looks_like_source(path: &str) -> bool {
    let ext = match path.rsplit_once('.') {
        Some((_, e)) => e,
        None => return false,
    };
    // The extension must be the final path segment's suffix (no '/' after the
    // dot) — guards against `a.b/c` matching.
    if ext.contains('/') || ext.contains('\\') {
        return false;
    }
    let lower = ext.to_ascii_lowercase();
    KNOWN_EXTS.contains(&lower.as_str())
}

/// Pick the error headline: the first line carrying a common fault marker, else
/// the first non-empty line. Trimmed and length-bounded for the report header.
fn error_headline(trace: &str) -> String {
    const MARKERS: &[&str] = &[
        "panicked at",
        "Traceback",
        "Exception",
        "Error:",
        "error:",
        "error[",
        "fatal",
        "FATAL",
        "goroutine",
        "Caused by",
        "uncaught",
        "Uncaught",
        "RuntimeError",
        "segmentation fault",
    ];
    let marked = trace
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && MARKERS.iter().any(|m| l.contains(m)));
    let chosen = marked
        .or_else(|| trace.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or("(no headline)");
    let one: String = chosen.chars().take(200).collect();
    one
}

// ---------------------------------------------------------------------------
// Resolution + excerpting (confined; IO kept out of the pure parsers)
// ---------------------------------------------------------------------------

/// Resolve a trace file reference to a path INSIDE `root`, or None if it cannot
/// be confined. Lexical + confined: a relative path with a `..` component is
/// rejected; an absolute path is accepted only when it already lives under
/// `root`. Pure (no IO) so the confinement rule is unit-tested.
fn resolve_in_root(file: &str, root: &Path) -> Option<PathBuf> {
    let p = Path::new(file);
    if p.is_absolute() {
        return p.starts_with(root).then(|| p.to_path_buf());
    }
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return None; // `../` escape attempt
    }
    Some(root.join(p))
}

/// Read a bounded window of source around `line` (1-based) and render it with
/// line numbers, marking the cited line with `>`. None when the file is missing,
/// too large, not valid UTF-8, escapes the root (canonical belt-and-braces —
/// catches a symlink inside the root that points out of it), or the line is out
/// of range. READ-ONLY.
fn read_window(path: &Path, line: u32, root: &Path) -> Option<String> {
    let canon = path.canonicalize().ok()?;
    // Belt-and-braces over the lexical resolve_in_root: the REAL path (symlinks
    // followed) must still live inside the project root, or we do not read it.
    if !canon.starts_with(root) {
        return None;
    }
    let meta = std::fs::metadata(&canon).ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let body = std::fs::read_to_string(&canon).ok()?;
    let lines: Vec<&str> = body.lines().collect();
    let target = line as usize;
    if target == 0 || target > lines.len() {
        return None;
    }
    Some(format_window(&lines, target))
}

/// Render the ±CONTEXT window around `target` (1-based) with right-aligned line
/// numbers, the cited line prefixed `> `. Pure.
fn format_window(lines: &[&str], target: usize) -> String {
    let start = target.saturating_sub(CONTEXT).max(1);
    let end = (target + CONTEXT).min(lines.len());
    let width = end.to_string().len();
    let mut out = String::new();
    for n in start..=end {
        let marker = if n == target { ">" } else { " " };
        out.push_str(&format!("{marker} {n:>width$} | {}\n", lines[n - 1], width = width));
    }
    out
}

/// Render the full map: headline, a frame census, the likely culprit, and each
/// frame with its window (or a one-line "library / unresolved" note). Pure.
fn render_map(headline: &str, mapped: &[Mapped], root: &Path) -> String {
    let in_project = mapped.iter().filter(|m| m.in_project).count();
    let total = mapped.len();
    let mut out = String::new();
    out.push_str(&format!("error: {headline}\n"));
    out.push_str(&format!(
        "frames: {total} ({in_project} in project, {} library/unresolved)\n",
        total - in_project
    ));
    out.push_str(&format!("root: {}\n", root.display()));
    if let Some(culprit) = mapped.iter().find(|m| m.in_project) {
        out.push_str(&format!(
            "likely culprit: {}:{}{}\n",
            culprit.frame.file,
            culprit.frame.line,
            culprit
                .frame
                .symbol
                .as_deref()
                .map(|s| format!("  in {s}"))
                .unwrap_or_default()
        ));
    }
    out.push('\n');
    for (i, m) in mapped.iter().enumerate() {
        let col = m.frame.col.map(|c| format!(":{c}")).unwrap_or_default();
        let sym = m
            .frame
            .symbol
            .as_deref()
            .map(|s| format!("  in {s}"))
            .unwrap_or_default();
        let tag = if m.in_project { "[project]" } else { "[library]" };
        out.push_str(&format!(
            "#{} {}:{}{}{}  {tag}\n",
            i + 1,
            m.frame.file,
            m.frame.line,
            col,
            sym
        ));
        match &m.window {
            Some(w) => {
                out.push_str(w);
            }
            None => {
                out.push_str("    (source unavailable — outside project root or not on disk)\n");
            }
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "({total} frame{} mapped; read-only — Cartographer changed nothing)",
        if total == 1 { "" } else { "s" }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_rust_panic_backtrace() {
        let trace = "thread 'main' panicked at 'index out of bounds', src/router.rs:142:9\n\
                     note: run with `RUST_BACKTRACE=1`\n\
                     stack backtrace:\n\
                        0: jarvis_core::route   at src/router.rs:142:9\n\
                        1: jarvis_core::main    at src/main.rs:88";
        let frames = parse_frames(trace);
        // src/router.rs:142 (deduped across the two occurrences) + src/main.rs:88.
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].file, "src/router.rs");
        assert_eq!(frames[0].line, 142);
        assert_eq!(frames[0].col, Some(9));
        assert_eq!(frames[1].file, "src/main.rs");
        assert_eq!(frames[1].line, 88);
        assert_eq!(frames[1].col, None);
    }

    #[test]
    fn parses_a_python_traceback() {
        let trace = "Traceback (most recent call last):\n\
                     \x20 File \"/app/server.py\", line 31, in handle\n\
                     \x20   return self.dispatch(req)\n\
                     \x20 File \"/app/router.py\", line 12, in dispatch\n\
                     RuntimeError: no route";
        let frames = parse_frames(trace);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].file, "/app/server.py");
        assert_eq!(frames[0].line, 31);
        assert_eq!(frames[0].symbol.as_deref(), Some("handle"));
        assert_eq!(frames[1].file, "/app/router.py");
        assert_eq!(frames[1].line, 12);
    }

    #[test]
    fn parses_a_node_stack() {
        let trace = "TypeError: x is not a function\n\
                     \x20   at handler (/srv/app/index.js:42:13)\n\
                     \x20   at /srv/app/lib/run.js:8:5";
        let frames = parse_frames(trace);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].file, "/srv/app/index.js");
        assert_eq!(frames[0].line, 42);
        assert_eq!(frames[0].col, Some(13));
        assert_eq!(frames[0].symbol.as_deref(), Some("handler"));
        assert_eq!(frames[1].file, "/srv/app/lib/run.js");
        assert_eq!(frames[1].line, 8);
    }

    #[test]
    fn parses_a_java_stack() {
        let trace = "Exception in thread \"main\" java.lang.NullPointerException\n\
                     \tat com.example.Service.run(Service.java:88)\n\
                     \tat com.example.Main.main(Main.java:12)";
        let frames = parse_frames(trace);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].file, "Service.java");
        assert_eq!(frames[0].line, 88);
        assert_eq!(frames[0].symbol.as_deref(), Some("com.example.Service.run"));
    }

    #[test]
    fn pathline_rejects_non_source_and_clocks() {
        assert!(parse_pathline("localhost:8080").is_none());
        assert!(parse_pathline("12:30").is_none());
        assert!(parse_pathline("notes.txt:5").is_none());
        assert_eq!(
            parse_pathline("a/b/c.rs:10:4"),
            Some(("a/b/c.rs".to_string(), 10, Some(4)))
        );
        assert_eq!(parse_pathline("main.go:7"), Some(("main.go".to_string(), 7, None)));
    }

    #[test]
    fn headline_prefers_a_fault_marker() {
        let t = "some setup line\nRuntimeError: boom\nmore text";
        assert_eq!(error_headline(t), "RuntimeError: boom");
        let none = "just a plain first line\nand another";
        assert_eq!(error_headline(none), "just a plain first line");
    }

    #[test]
    fn resolve_confines_to_root() {
        let root = Path::new("/proj");
        // Relative, in-root.
        assert_eq!(
            resolve_in_root("src/a.rs", root),
            Some(PathBuf::from("/proj/src/a.rs"))
        );
        // `..` escape rejected.
        assert!(resolve_in_root("../../etc/passwd", root).is_none());
        assert!(resolve_in_root("src/../../secret.rs", root).is_none());
        // Absolute outside root rejected; absolute inside accepted.
        assert!(resolve_in_root("/etc/passwd", root).is_none());
        assert_eq!(
            resolve_in_root("/proj/src/b.rs", root),
            Some(PathBuf::from("/proj/src/b.rs"))
        );
    }

    #[test]
    fn window_marks_the_cited_line() {
        let lines = vec!["one", "two", "three", "four", "five", "six", "seven"];
        let w = format_window(&lines, 4);
        // The cited line (4 = "four") is marked with `>`; neighbors are not.
        assert!(w.contains("> 4 | four"), "got:\n{w}");
        assert!(w.contains("  3 | three"), "got:\n{w}");
        assert!(w.contains("  5 | five"), "got:\n{w}");
        // CONTEXT=4 each side, clamped to the slice bounds.
        assert!(!w.contains("eight"));
    }

    #[test]
    fn empty_and_junk_traces_yield_no_frames() {
        assert!(parse_frames("").is_empty());
        assert!(parse_frames("no frames here\njust prose\nport 8080 open").is_empty());
    }

    #[test]
    fn render_reports_census_and_culprit() {
        let mapped = vec![
            Mapped {
                frame: Frame {
                    file: "src/router.rs".into(),
                    line: 142,
                    col: Some(9),
                    symbol: Some("route".into()),
                    lang: "generic",
                },
                in_project: true,
                window: Some("> 142 | bail!()\n".into()),
            },
            Mapped {
                frame: Frame {
                    file: "/usr/lib/std.rs".into(),
                    line: 5,
                    col: None,
                    symbol: None,
                    lang: "generic",
                },
                in_project: false,
                window: None,
            },
        ];
        let out = render_map("panicked at index out of bounds", &mapped, Path::new("/proj"));
        assert!(out.contains("frames: 2 (1 in project, 1 library/unresolved)"));
        assert!(out.contains("likely culprit: src/router.rs:142  in route"));
        assert!(out.contains("#1 src/router.rs:142:9  in route  [project]"));
        assert!(out.contains("(source unavailable"));
        assert!(out.contains("read-only — Cartographer changed nothing"));
    }
}
