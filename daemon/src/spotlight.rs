//! SPOTLIGHT CANDIDATE GENERATION + METADATA ENRICHMENT — a STRICTLY READ-ONLY
//! bridge from macOS Spotlight (`mdfind` / `mdls`) into the confined docsearch
//! subsystem ([`crate::docsearch`]).
//!
//! DARWIN already keeps its own bounded, path-confined file index; Spotlight is
//! the OS's. This module lets a file search additionally ask Spotlight "which
//! files under my ALLOWLISTED roots match this query?" and feeds the answers into
//! docsearch as ADDITIONAL CANDIDATES — every one of which then passes the SAME
//! confinement / extension-allowlist / size-cap / honest-skip extraction pipeline
//! the walk-discovered files do ([`crate::docsearch::DocIndex::absorb_candidates`]).
//! Spotlight NEVER bypasses a docsearch rule; it only proposes paths.
//!
//! ## Contract: READ-ONLY, root-confined, bounded, honest
//!
//!   * READ-ONLY — the ONLY commands here are `/usr/bin/mdfind` (query the
//!     Spotlight index) and `/usr/bin/mdls` (read one file's metadata). Both are
//!     pure reads of an index macOS maintains anyway. There is NO `mdutil`, NO
//!     command that enables/disables/erases Spotlight state, and no code path
//!     that could grow one — the two program constants are the whole surface.
//!   * ROOT-CONFINED — an mdfind invocation ALWAYS carries `-onlyin <root>` for
//!     every configured `[docsearch].roots` entry and is NEVER issued without at
//!     least one root (no unrestricted global query, ever). Every returned path
//!     is then re-confined ([`crate::docsearch::confine`]): canonicalized and
//!     required to resolve INSIDE a root — a symlink escape, a `..` traversal,
//!     or an absolute-elsewhere line from a hostile output is DROPPED. Hidden
//!     entries (dotfiles/dotdirs) are dropped too, mirroring the walk's rule.
//!   * BOUNDED — fixed argv (never a shell string), hard timeout, kill_on_drop,
//!     a byte ceiling on the bytes EVER HELD from the child (an incremental
//!     capped read, [`read_capped`] — the child is killed the moment the ceiling
//!     is reached, never buffered whole then truncated), and a config-bounded
//!     candidate count (`[docsearch].spotlight_max_candidates`, default 64).
//!   * HONEST — metadata that cannot be read (mdls missing, a `(null)` attribute,
//!     malformed output) is ABSENT, never fabricated. Availability
//!     ([`spotlight_available`]) reports the MOST RECENT real mdfind attempt:
//!     true only while mdfind is present AND the last real query succeeded — a
//!     machine with Spotlight indexing disabled, or one where mdfind STOPS
//!     answering mid-session, honestly reports false (never a stale claim). The
//!     status emission additionally reports false whenever the config flag is
//!     off ([`reported_available`]).
//!
//! ## PURE seam vs DEVICE-GATED runner (the power.rs / posture.rs house pattern)
//!
//! The argv builders ([`mdfind_args`] / [`mdls_args`]) and parsers
//! ([`parse_mdfind_paths`] / [`parse_mdls`]) are PURE and unit-tested on canned
//! (including hostile) text. The command RUNNER is injected — a function
//! matching [`run_real_command`]'s signature — so [`candidates`] and
//! [`file_metadata`] are driven in tests with canned output and the real
//! subprocesses are NEVER spawned under test. Only the live entry points
//! ([`augment_index`] / [`enrich_hits`]) bind the real runner.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::docsearch::{DocHit, DocIndex, Discovered, IndexBounds};
use crate::recall::Embedder;

/// The ONLY two programs this module ever runs — both strictly READ-ONLY queries
/// of the Spotlight index / a file's metadata. Absolute paths, fixed argv, never
/// a shell string (the vitals.rs / posture.rs discipline). Anything that MUTATES
/// Spotlight state (`mdutil`, ...) is deliberately absent and must stay so.
const MDFIND: &str = "/usr/bin/mdfind";
const MDLS: &str = "/usr/bin/mdls";

/// Hard wall-clock ceilings per spawned query — same bounded-subprocess
/// discipline as posture.rs / vitals.rs. mdfind can chew on a cold index; mdls
/// reads one file's already-materialized attributes and is near-instant.
const MDFIND_TIMEOUT: Duration = Duration::from_secs(5);
const MDLS_TIMEOUT: Duration = Duration::from_secs(3);

/// Ceiling on the stdout bytes EVER HELD from one query. This bounds CAPTURE,
/// not merely retention: the live runner reads the child's pipe incrementally
/// ([`read_capped`]) and KILLS the child the moment this many bytes have
/// arrived — a broad query over a large root can never spike the daemon's
/// memory for the duration of the timeout. 1 MiB comfortably holds thousands
/// of paths; the candidate/line caps below bound the parse again.
const MAX_OUTPUT_BYTES: usize = 1 << 20;

/// A single output line longer than this is garbage, not a path (macOS caps
/// paths at 1024 bytes; be generous) — dropped by the parser.
const MAX_PATH_BYTES: usize = 4096;

/// Hard ceiling the config knob is clamped under, so a typo'd
/// `spotlight_max_candidates = 999999` can never turn one search into a bulk
/// disk ingestion. The default knob is 64.
const MAX_CANDIDATES_CEILING: usize = 512;

/// Cap on the user query text forwarded to mdfind (chars). A longer query is
/// truncated on a char boundary — Spotlight relevance does not improve past this.
const MAX_QUERY_CHARS: usize = 256;

/// Caps on parsed metadata fields, so a hostile xattr can never bloat a result.
const MAX_META_FIELD_CHARS: usize = 256;
const MAX_AUTHORS: usize = 8;

// ---------------------------------------------------------------------------
// ARGV BUILDERS — pure, fixed-shape, root-confined
// ---------------------------------------------------------------------------

/// Build the EXACT argv (after the program) for one root-confined mdfind query:
/// `-onlyin <root>` for EVERY canonical root (config order, so the invocation is
/// deterministic) followed by the query as the single final positional arg.
///
/// Returns `None` — NO invocation — when the query cannot be issued safely:
///   * `canonical_roots` is empty (an unrestricted, whole-Mac Spotlight query is
///     NEVER built — the same no-whole-disk-scan posture as the walk);
///   * the trimmed query is empty (nothing to ask);
///   * the trimmed query starts with `-` (it could parse as an mdfind FLAG; the
///     fixed-argv discipline forbids user text becoming an option).
///     Control characters are flattened to spaces and the query is capped to
///     [`MAX_QUERY_CHARS`], so a pasted multi-line blob stays one bounded arg.
pub fn mdfind_args(query: &str, canonical_roots: &[PathBuf]) -> Option<Vec<String>> {
    if canonical_roots.is_empty() {
        return None; // NEVER an unrestricted global query
    }
    let cleaned: String = query
        .trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_QUERY_CHARS)
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned.starts_with('-') {
        return None;
    }
    let mut args = Vec::with_capacity(canonical_roots.len() * 2 + 1);
    for root in canonical_roots {
        args.push("-onlyin".to_string());
        args.push(root.display().to_string());
    }
    args.push(cleaned);
    Some(args)
}

/// The EXACT argv (after the program) for one mdls metadata read: the three
/// fixed `-name` attribute selectors, then the file path. The path is always a
/// canonical ABSOLUTE path (it starts with `/`), so it can never parse as a flag.
pub fn mdls_args(path: &Path) -> Vec<String> {
    vec![
        "-name".to_string(),
        "kMDItemContentType".to_string(),
        "-name".to_string(),
        "kMDItemLastUsedDate".to_string(),
        "-name".to_string(),
        "kMDItemAuthors".to_string(),
        path.display().to_string(),
    ]
}

// ---------------------------------------------------------------------------
// PARSERS — pure, defensive, unit-tested on hostile text
// ---------------------------------------------------------------------------

/// Parse mdfind's newline-separated path list DEFENSIVELY: keep only ABSOLUTE
/// lines (a relative/garbage/injected line is dropped — a path that does not
/// start at `/` cannot be confined and is never trusted), trim a CR, drop
/// absurdly long lines, dedupe, and CAP the count at `cap` so a runaway output
/// can never turn into a runaway candidate list. Confinement happens LATER
/// (canonicalize + in-root check) — this only pre-filters the obvious garbage.
pub fn parse_mdfind_paths(out: &str, cap: usize) -> Vec<PathBuf> {
    let cap = cap.max(1);
    let mut seen: HashSet<&str> = HashSet::new();
    let mut paths = Vec::new();
    for line in out.lines() {
        if paths.len() >= cap {
            break;
        }
        let line = line.trim_end_matches('\r');
        if !line.starts_with('/') || line.len() > MAX_PATH_BYTES {
            continue;
        }
        if seen.insert(line) {
            paths.push(PathBuf::from(line));
        }
    }
    paths
}

/// One file's Spotlight metadata, every field OPTIONAL. HONEST DEGRADATION is
/// the whole design: an attribute mdls reports as `(null)`, a malformed line, or
/// a failed read all leave the field `None` — a value is present ONLY when
/// Spotlight really returned one. Never fabricated, never guessed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileMetadata {
    /// `kMDItemContentType` — the UTI (e.g. `net.daringfireball.markdown`).
    pub content_type: Option<String>,
    /// `kMDItemLastUsedDate` — Spotlight's last-used timestamp, verbatim text.
    pub last_used: Option<String>,
    /// `kMDItemAuthors` — the authors list, joined ", " (bounded).
    pub authors: Option<String>,
}

impl FileMetadata {
    /// Whether NO field could be read — the caller shows nothing rather than an
    /// empty decoration.
    pub fn is_empty(&self) -> bool {
        self.content_type.is_none() && self.last_used.is_none() && self.authors.is_none()
    }
}

/// Strip one layer of surrounding double quotes (mdls quotes string values) and
/// cap the field length on a char boundary. `(null)` — mdls's "attribute not
/// present" marker — and an empty remainder read as `None`.
fn clean_meta_value(raw: &str, cap: usize) -> Option<String> {
    let v = raw.trim().trim_matches('"').trim();
    if v.is_empty() || v == "(null)" {
        return None;
    }
    Some(v.chars().take(cap).collect())
}

/// Parse `mdls -name kMDItemContentType -name kMDItemLastUsedDate -name
/// kMDItemAuthors <file>` output DEFENSIVELY into a [`FileMetadata`]. The shape:
///
///   kMDItemAuthors      = (
///       "A. Author",
///       "B. Author"
///   )
///   kMDItemContentType  = "net.daringfireball.markdown"
///   kMDItemLastUsedDate = 2026-07-01 09:14:22 +0000
///
/// Any attribute may instead read `= (null)`. A line that does not match, a
/// value we cannot make sense of, or garbage input all degrade to absent fields
/// — NEVER a fabricated value. Fields and the author list are capped.
pub fn parse_mdls(out: &str) -> FileMetadata {
    let mut meta = FileMetadata::default();
    let mut lines = out.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "kMDItemContentType" => {
                meta.content_type = clean_meta_value(value, MAX_META_FIELD_CHARS);
            }
            "kMDItemLastUsedDate" => {
                meta.last_used = clean_meta_value(value, MAX_META_FIELD_CHARS);
            }
            "kMDItemAuthors" => {
                let mut authors: Vec<String> = Vec::new();
                if value == "(" {
                    // Multi-line list: collect quoted entries until the `)`.
                    for entry in lines.by_ref() {
                        let entry = entry.trim();
                        if entry == ")" {
                            break;
                        }
                        if authors.len() >= MAX_AUTHORS {
                            continue; // keep consuming to the `)`, keep the cap
                        }
                        if let Some(a) =
                            clean_meta_value(entry.trim_end_matches(','), MAX_META_FIELD_CHARS)
                        {
                            authors.push(a);
                        }
                    }
                } else if let Some(inner) =
                    value.strip_prefix('(').and_then(|v| v.strip_suffix(')'))
                {
                    // Single-line list: ("A", "B"). `(null)` never reaches here
                    // (no closing paren strip on "null)")... it does — guard below.
                    for part in inner.split(',').take(MAX_AUTHORS) {
                        if let Some(a) = clean_meta_value(part, MAX_META_FIELD_CHARS) {
                            if a != "null" {
                                authors.push(a);
                            }
                        }
                    }
                }
                if !authors.is_empty() {
                    meta.authors = Some(authors.join(", "));
                }
            }
            _ => {}
        }
    }
    meta
}

// ---------------------------------------------------------------------------
// CANDIDATE GENERATION — mdfind through the injected runner, then CONFINEMENT
// ---------------------------------------------------------------------------

/// Whether `real` (already canonical) has a HIDDEN component under `root` —
/// mirrors the walk's dotfile/dotdir privacy rule ([`crate::docsearch`]), so a
/// Spotlight candidate can never surface a hidden file the walk would refuse.
/// Fails CLOSED: a path we cannot relativize reads as hidden.
fn hidden_under(real: &Path, root: &Path) -> bool {
    real.strip_prefix(root)
        .map(|rel| {
            rel.components().any(
                |c| matches!(c, Component::Normal(n) if n.to_string_lossy().starts_with('.')),
            )
        })
        .unwrap_or(true)
}

/// Ask Spotlight for candidate files matching `query` under the ALLOWLISTED
/// `roots`, through the injected `run`ner. Returns candidates ALREADY CONFINED:
/// each returned [`Discovered`] is a canonicalized real path proven to resolve
/// inside a canonical root ([`crate::docsearch::confine`] — a symlink escape /
/// `..` / absolute-elsewhere line is DROPPED), with hidden entries dropped
/// (the walk's privacy rule) and the count capped. An unissuable query (no
/// roots / empty / flag-like) or a failed run yields the honest empty list.
///
/// The candidates are PROPOSALS ONLY — the indexing pipeline
/// ([`DocIndex::absorb_candidates`]) re-applies every gate (confinement again,
/// extension allowlist, size cap, honest-skip extraction) before anything is
/// stored.
pub async fn candidates<F, Fut>(
    query: &str,
    roots: &[String],
    max_candidates: usize,
    run: F,
) -> Vec<Discovered>
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    let canon = crate::docsearch::canonical_roots(roots);
    let Some(args) = mdfind_args(query, &canon) else {
        return Vec::new();
    };
    let Some(out) = run(MDFIND, args, MDFIND_TIMEOUT).await else {
        return Vec::new(); // mdfind absent / failed / timed out -> honestly none
    };
    let cap = max_candidates.clamp(1, MAX_CANDIDATES_CEILING);
    let mut found: Vec<Discovered> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for path in parse_mdfind_paths(&out, cap) {
        // PATH CONFINEMENT — the same primitive the walk uses. A candidate that
        // resolves outside every allowlisted root is DROPPED here, whatever
        // Spotlight (or a hostile output) claimed.
        let Some(real) = crate::docsearch::confine(&path, &canon) else {
            continue;
        };
        let Some(root) = canon.iter().find(|r| real.starts_with(r)) else {
            continue; // unreachable after confine; kept for defense in depth
        };
        if hidden_under(&real, root) {
            continue; // the walk never surfaces hidden entries; neither do we
        }
        if seen.insert(real.clone()) {
            found.push(Discovered {
                path: real,
                root: root.clone(),
            });
        }
    }
    found
}

/// Read ONE file's Spotlight metadata through the injected `run`ner. A failed
/// read (mdls absent / non-zero / timeout) is the honest EMPTY metadata — every
/// field absent, nothing fabricated.
pub async fn file_metadata<F, Fut>(path: &Path, run: F) -> FileMetadata
where
    F: Fn(&'static str, Vec<String>, Duration) -> Fut,
    Fut: Future<Output = Option<String>>,
{
    match run(MDLS, mdls_args(path), MDLS_TIMEOUT).await {
        Some(out) => parse_mdls(&out),
        None => FileMetadata::default(),
    }
}

// ---------------------------------------------------------------------------
// AVAILABILITY — honest "is Spotlight actually answering" for the status pill
// ---------------------------------------------------------------------------

/// The outcome of the MOST RECENT real mdfind attempt — availability is a LIVE
/// health signal, never a sticky "worked once" claim: a success that is later
/// followed by a failure (Spotlight indexing turned off mid-session, mdfind
/// wedged/timing out) flips it back to false on that very attempt. A tiny
/// struct (not a bare static) so the record/read transition is unit-testable
/// on a LOCAL instance without ever touching the process-wide flag.
struct MdfindHealth(AtomicBool);

impl MdfindHealth {
    const fn new() -> Self {
        Self(AtomicBool::new(false))
    }
    /// Record the outcome of one REAL mdfind attempt (success OR failure —
    /// last-attempt-wins). Called ONLY by the live runner, never by an
    /// injected test runner.
    fn record(&self, ok: bool) {
        self.0.store(ok, Ordering::Relaxed);
    }
    /// Whether the most recent real attempt succeeded (false before any).
    fn last_ok(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// The process-wide health flag the LIVE runner records into.
static MDFIND_HEALTH: MdfindHealth = MdfindHealth::new();

/// The pure availability rule: Spotlight is claimed available ONLY when the
/// mdfind binary is present AND the MOST RECENT real query succeeded. Split out
/// so the honest-false paths are unit-testable without touching the
/// process-wide flag.
fn availability(mdfind_present: bool, last_attempt_ok: bool) -> bool {
    mdfind_present && last_attempt_ok
}

/// Whether Spotlight candidate generation is ACTUALLY working right now: mdfind
/// present next to where macOS ships it AND the most recent real query returned
/// successfully. HONEST FALSE before any query, after a failed/timed-out one
/// (Spotlight indexing disabled mid-session included), or without the binary —
/// the claim tracks the LAST attempt, never sticking on a stale success.
pub fn spotlight_available() -> bool {
    availability(Path::new(MDFIND).is_file(), MDFIND_HEALTH.last_ok())
}

/// What `docsearch.status` should REPORT for the Spotlight leg: the integration
/// is claimed only when `[docsearch].spotlight` is ON *and* the bridge is
/// actually answering ([`spotlight_available`]) — an operator who turned the
/// flag off sees an honest false even if mdfind last answered fine. Surfaced on
/// every status tick ([`crate::docsearch::emit_status`]) for the HUD pill.
pub fn reported_available(spotlight_enabled: bool) -> bool {
    spotlight_enabled && spotlight_available()
}

// ---------------------------------------------------------------------------
// Real command runner (NEVER reached in tests — they inject canned output)
// ---------------------------------------------------------------------------

/// Read from `reader` holding AT MOST `cap` bytes, incrementally through a
/// fixed 8 KiB scratch — the bounded-CAPTURE primitive of the live runner.
/// Returns `(bytes, hit_cap)`: `hit_cap` true means the ceiling was reached
/// and the caller must stop consuming (and kill the producing child). Unlike a
/// buffer-then-truncate, the bytes EVER HELD never exceed `cap` (+ the scratch),
/// whatever the stream tries to push. EOF or a read error ends the read with
/// whatever real bytes arrived. Pure over any `AsyncRead`, so it is unit-tested
/// on synthetic huge/small streams without spawning anything.
async fn read_capped<R>(reader: &mut R, cap: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut scratch = [0u8; 8192];
    loop {
        let remaining = cap.saturating_sub(buf.len());
        if remaining == 0 {
            return (buf, true); // ceiling reached — stop capturing NOW
        }
        let want = scratch.len().min(remaining);
        match reader.read(&mut scratch[..want]).await {
            Ok(0) => return (buf, false), // EOF within the ceiling
            Ok(n) => buf.extend_from_slice(&scratch[..n]),
            Err(_) => return (buf, false), // keep the real bytes we have
        }
    }
}

/// Spawn one READ-ONLY Spotlight query with explicit args (never a shell
/// string), bounded by `timeout` + kill_on_drop + the CAPTURE ceiling: stdout
/// is read incrementally ([`read_capped`]) and the child is KILLED the moment
/// [`MAX_OUTPUT_BYTES`] have arrived — the daemon never holds more than the
/// ceiling however much a broad query produces (stderr is discarded, stdin
/// closed). Returns the (lossy-decoded) stdout on a successful exit — a
/// ceiling-hit is OUR kill, not a child failure, so the capped output still
/// returns (the parser's candidate cap needs far less than 1 MiB of paths).
/// A spawn error, a real non-zero exit (Spotlight indexing disabled), or a
/// timeout (kill_on_drop reaps the child) is `None` and the caller degrades
/// honestly. Every REAL mdfind attempt — success OR failure — records into
/// [`MDFIND_HEALTH`] (last-attempt-wins); an injected test runner never
/// touches that flag.
async fn run_real_command(
    program: &'static str,
    args: Vec<String>,
    timeout: Duration,
) -> Option<String> {
    use std::process::Stdio;
    use tokio::process::Command;
    let mut cmd = Command::new(program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let outcome = tokio::time::timeout(timeout, async {
        let mut child = cmd.spawn().ok()?;
        let mut stdout = child.stdout.take()?;
        let (buf, hit_cap) = read_capped(&mut stdout, MAX_OUTPUT_BYTES).await;
        if hit_cap {
            // Stop the producer the moment the ceiling is reached — the bytes
            // already captured are plenty (the candidate cap is far smaller).
            let _ = child.kill().await;
        }
        let status = child.wait().await.ok()?;
        // A ceiling-hit exit is OUR kill (not a child failure); otherwise the
        // child must have exited cleanly for the output to be trusted.
        (status.success() || hit_cap).then_some(buf)
    })
    .await;
    let ok = matches!(&outcome, Ok(Some(_)));
    if program == MDFIND {
        // Last-attempt-wins health: a failure here (spawn error, non-zero
        // exit, timeout) flips availability back to false immediately.
        MDFIND_HEALTH.record(ok);
    }
    match outcome {
        Ok(Some(buf)) => Some(String::from_utf8_lossy(&buf).into_owned()),
        _ => None, // absent / failed / timed out -> the caller degrades honestly
    }
}

// ---------------------------------------------------------------------------
// LIVE ENTRY POINTS — config-gated; the only places the real runner binds
// ---------------------------------------------------------------------------

/// Feed Spotlight candidates for `query` into the docsearch index — the LIVE,
/// CONFIG-GATED wiring the `doc_search` tool calls before ranking. Gated on
/// `[docsearch].spotlight` AND the same permit the indexer itself requires
/// (`enabled` + a non-empty `roots` — Spotlight NEVER widens what docsearch may
/// touch; with no allowlisted root there is no query at all). The candidates go
/// through [`DocIndex::absorb_candidates`] — the EXISTING confined, bounded,
/// honest-skip indexing pipeline — so a Spotlight-surfaced file is stored (and
/// later cited) exactly as if the walk had found it. Returns how many files were
/// absorbed; any failure degrades to 0 (the search proceeds over the index as it
/// was — never an error, never a fabricated result).
pub async fn augment_index(
    cfg: &crate::config::DocSearchConfig,
    index: &DocIndex,
    query: &str,
    embedder: &dyn Embedder,
) -> u64 {
    if !cfg.spotlight || !crate::docsearch::indexing_permitted(cfg.enabled, &cfg.roots) {
        return 0;
    }
    let found = candidates(query, &cfg.roots, cfg.spotlight_max_candidates, run_real_command).await;
    if found.is_empty() {
        return 0;
    }
    let bounds = IndexBounds::from_config(cfg);
    match index.absorb_candidates(&cfg.roots, found, &bounds, embedder).await {
        Ok(n) => {
            if n > 0 {
                tracing::debug!(target: "spotlight", files = n, "absorbed Spotlight candidates");
            }
            n
        }
        Err(e) => {
            tracing::warn!(target: "spotlight", error = %e, "candidate absorption failed");
            0
        }
    }
}

/// Enrich CITED search hits with Spotlight metadata — LIVE + CONFIG-GATED
/// (`[docsearch].spotlight`). Returns one `Option<FileMetadata>` PER HIT (same
/// order): `Some` only when the hit's file still confines under the CURRENT
/// allowlisted roots (a stale citation from a since-removed root gets no mdls
/// read) AND mdls actually returned at least one field. Bounded: hits are
/// already capped at the search `k`, and repeat paths are read once.
pub async fn enrich_hits(
    cfg: &crate::config::DocSearchConfig,
    hits: &[DocHit],
) -> Vec<Option<FileMetadata>> {
    if !cfg.spotlight || hits.is_empty() {
        return vec![None; hits.len()];
    }
    let canon = crate::docsearch::canonical_roots(&cfg.roots);
    let mut memo: HashMap<&str, Option<FileMetadata>> = HashMap::new();
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        if let Some(m) = memo.get(h.file_path.as_str()) {
            out.push(m.clone());
            continue;
        }
        let path = Path::new(&h.file_path);
        // Only paths ALREADY in scope are enriched (the same confinement rule
        // as everything else); an out-of-scope path reads nothing.
        let meta = if crate::docsearch::confine(path, &canon).is_some() {
            let m = file_metadata(path, run_real_command).await;
            if m.is_empty() {
                None
            } else {
                Some(m)
            }
        } else {
            None
        };
        memo.insert(h.file_path.as_str(), meta.clone());
        out.push(meta);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// A unique temp dir tree per test, cleaned on drop — the docsearch tests'
    /// fixture, mirrored so confinement runs over REAL canonicalizable paths
    /// without ever touching the user's home.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "darwin-spotlight-test-{}-{}",
                std::process::id(),
                tag
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            TempTree(path)
        }
        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, contents).unwrap();
            p
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The boxed future an injected test runner returns (the runner seam's Fut).
    type RunFuture = std::pin::Pin<Box<dyn Future<Output = Option<String>> + Send>>;

    /// An injected runner that returns `out` as a successful invocation and
    /// records every (program, args) it was handed — the canned-output seam.
    fn canned<'a>(
        out: &str,
        calls: &'a Mutex<Vec<(String, Vec<String>)>>,
    ) -> impl Fn(&'static str, Vec<String>, Duration) -> RunFuture + 'a {
        let out = out.to_string();
        move |program, args, _timeout| {
            calls.lock().unwrap().push((program.to_string(), args));
            let out = out.clone();
            Box::pin(async move { Some(out) })
        }
    }

    /// An injected runner that always FAILS (mdfind absent / Spotlight off).
    fn down(_program: &'static str, _args: Vec<String>, _timeout: Duration) -> RunFuture {
        Box::pin(async { None })
    }

    // =====================================================================
    // READ-ONLY INVARIANT: mdfind + mdls are the WHOLE command surface
    // =====================================================================

    #[test]
    fn read_only_invariant_only_mdfind_and_mdls_exist() {
        // The two program constants ARE the command surface — both pure reads.
        // No mdutil, no state-mutating Spotlight command exists in this module
        // (grep-provable), and the argv builders only ever target these two.
        assert_eq!(MDFIND, "/usr/bin/mdfind");
        assert_eq!(MDLS, "/usr/bin/mdls");
        // And no built argv smuggles a mutating subcommand: mdls args are the
        // three fixed -name selectors + the path, nothing else.
        let args = mdls_args(Path::new("/tmp/a.md"));
        assert_eq!(
            args,
            vec![
                "-name",
                "kMDItemContentType",
                "-name",
                "kMDItemLastUsedDate",
                "-name",
                "kMDItemAuthors",
                "/tmp/a.md"
            ]
        );
    }

    // =====================================================================
    // ARGV CONSTRUCTION — roots order, -onlyin per root, refusal paths
    // =====================================================================

    #[test]
    fn mdfind_args_carry_onlyin_per_root_in_order_then_the_query_last() {
        let roots = vec![PathBuf::from("/Users/me/Documents"), PathBuf::from("/Users/me/notes")];
        let args = mdfind_args("quarterly budget", &roots).expect("issuable");
        assert_eq!(
            args,
            vec![
                "-onlyin",
                "/Users/me/Documents",
                "-onlyin",
                "/Users/me/notes",
                "quarterly budget"
            ],
            "every root gets its -onlyin, config order, query strictly last"
        );
    }

    #[test]
    fn mdfind_args_refuse_unrestricted_empty_and_flag_like_queries() {
        let roots = vec![PathBuf::from("/Users/me/Documents")];
        // NO roots -> NO invocation, ever (never an unrestricted global query).
        assert_eq!(mdfind_args("anything", &[]), None);
        // An empty / whitespace query is unissuable.
        assert_eq!(mdfind_args("", &roots), None);
        assert_eq!(mdfind_args("   \n  ", &roots), None);
        // A flag-like query could parse as an mdfind OPTION -> refused.
        assert_eq!(mdfind_args("-interpret evil", &roots), None);
        // Control chars are flattened, the query stays one bounded arg.
        let args = mdfind_args("line one\nline two", &roots).unwrap();
        assert_eq!(args.last().unwrap(), "line one line two");
        // An overlong query is capped, not refused.
        let long = "q".repeat(10_000);
        let args = mdfind_args(&long, &roots).unwrap();
        assert!(args.last().unwrap().chars().count() <= MAX_QUERY_CHARS);
    }

    // =====================================================================
    // PARSER — hostile output: relative lines, floods, lossy bytes, empties
    // =====================================================================

    #[test]
    fn parse_mdfind_drops_relative_garbage_and_caps_the_count() {
        // Relative paths, empty lines, an overlong line, CRLF, and lossy
        // replacement chars (the runner decodes non-UTF8 lossily) — only the
        // absolute, sane lines survive, deduped.
        let overlong = format!("/{}", "x".repeat(MAX_PATH_BYTES + 10));
        let out = format!(
            "/Users/me/docs/a.md\r\nrelative/path.md\n\n../escape.md\n{overlong}\n\
             /Users/me/docs/b\u{fffd}.md\n/Users/me/docs/a.md\nnot a path at all\n"
        );
        let paths = parse_mdfind_paths(&out, 64);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/Users/me/docs/a.md"),
                PathBuf::from("/Users/me/docs/b\u{fffd}.md"),
            ],
            "only absolute, bounded, deduped lines survive"
        );
        // A flood of lines is CAPPED, never followed to the end.
        let flood: String = (0..10_000).map(|i| format!("/tmp/f{i}.md\n")).collect();
        assert_eq!(parse_mdfind_paths(&flood, 5).len(), 5, "the count cap holds");
        // Empty / pathless output parses to nothing (never a fabricated path).
        assert!(parse_mdfind_paths("", 64).is_empty());
        assert!(parse_mdfind_paths("no paths here\njust noise\n", 64).is_empty());
    }

    // =====================================================================
    // MDLS PARSER — real shape, (null) fields, garbage -> honest absence
    // =====================================================================

    #[test]
    fn parse_mdls_reads_the_real_shape_and_null_degrades_to_absent() {
        let out = r#"kMDItemAuthors      = (
    "A. Author",
    "B. Author"
)
kMDItemContentType  = "net.daringfireball.markdown"
kMDItemLastUsedDate = 2026-07-01 09:14:22 +0000
"#;
        let meta = parse_mdls(out);
        assert_eq!(meta.content_type.as_deref(), Some("net.daringfireball.markdown"));
        assert_eq!(meta.last_used.as_deref(), Some("2026-07-01 09:14:22 +0000"));
        assert_eq!(meta.authors.as_deref(), Some("A. Author, B. Author"));
        assert!(!meta.is_empty());

        // Unreadable attributes are (null) -> ABSENT fields, never fabricated.
        let nulls = "kMDItemAuthors      = (null)\nkMDItemContentType  = (null)\n\
                     kMDItemLastUsedDate = (null)\n";
        let meta = parse_mdls(nulls);
        assert_eq!(meta, FileMetadata::default(), "(null) everywhere -> all absent");
        assert!(meta.is_empty());

        // Garbage input parses to honest absence, never a panic or a guess.
        assert!(parse_mdls("").is_empty());
        assert!(parse_mdls("complete = nonsense\nno known = keys\n").is_empty());
        assert!(parse_mdls("kMDItemContentType\nno equals sign").is_empty());

        // The author list is CAPPED — a hostile 1000-author xattr stays bounded.
        let mut hostile = String::from("kMDItemAuthors = (\n");
        for i in 0..1000 {
            hostile.push_str(&format!("    \"author {i}\",\n"));
        }
        hostile.push_str(")\n");
        let meta = parse_mdls(&hostile);
        let joined = meta.authors.expect("authors parsed");
        assert!(
            joined.matches("author ").count() <= MAX_AUTHORS,
            "the author cap holds: {joined}"
        );
    }

    // =====================================================================
    // CANDIDATES — the injected-runner path: confinement, hidden, dedupe
    // =====================================================================

    #[tokio::test]
    async fn candidates_confine_to_roots_and_drop_escapes_hidden_and_garbage() {
        let t = TempTree::new("candidates");
        let root = t.join("vault");
        fs::create_dir_all(&root).unwrap();
        let inside = t.write("vault/note.md", "an in-vault note");
        let outside = t.write("outside/secret.md", "OUTSIDE — must never be a candidate");
        let hidden = t.write("vault/.secret.md", "hidden — the walk's privacy rule");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // A symlink INSIDE the root escaping to the outside file: its lexical
            // path is in-root, its REAL path is not -> must be dropped.
            symlink(&outside, root.join("escape.md")).unwrap();
        }

        // Canned mdfind output: the good file (twice — dedupe), the outside
        // file, the hidden file, the escaping symlink, and relative garbage.
        let mut out = format!(
            "{in_p}\n{out_p}\n{hid_p}\nrelative/junk.md\n{in_p}\n",
            in_p = inside.display(),
            out_p = outside.display(),
            hid_p = hidden.display(),
        );
        #[cfg(unix)]
        out.push_str(&format!("{}\n", root.join("escape.md").display()));

        let calls = Mutex::new(Vec::new());
        let roots = vec![root.display().to_string()];
        let found = candidates("note", &roots, 64, canned(&out, &calls)).await;

        // ONLY the genuinely-in-root, non-hidden file survives, once.
        let real_inside = fs::canonicalize(&inside).unwrap();
        assert_eq!(found.len(), 1, "exactly the confined candidate: {found:?}");
        assert_eq!(found[0].path, real_inside);
        assert!(found[0].root.ends_with("vault") || found[0].path.starts_with(&found[0].root));

        // And the invocation was the EXACT fixed argv: -onlyin <canonical root>,
        // then the query — never an unrestricted query.
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (program, args) = &calls[0];
        assert_eq!(program, MDFIND);
        let canon_root = fs::canonicalize(&root).unwrap().display().to_string();
        assert_eq!(args, &vec!["-onlyin".to_string(), canon_root, "note".to_string()]);
    }

    #[tokio::test]
    async fn candidates_cap_holds_against_a_flood() {
        let t = TempTree::new("cand-cap");
        let root = t.join("vault");
        fs::create_dir_all(&root).unwrap();
        // 12 real files, but a cap of 3 candidates.
        let mut out = String::new();
        for i in 0..12 {
            let p = t.write(&format!("vault/f{i}.md"), "x");
            out.push_str(&format!("{}\n", p.display()));
        }
        let calls = Mutex::new(Vec::new());
        let roots = vec![root.display().to_string()];
        let found = candidates("x", &roots, 3, canned(&out, &calls)).await;
        assert_eq!(found.len(), 3, "the candidate cap bounds the flood");
    }

    #[tokio::test]
    async fn candidates_are_empty_when_the_runner_fails_or_roots_are_empty() {
        let t = TempTree::new("cand-down");
        let root = t.join("vault");
        fs::create_dir_all(&root).unwrap();
        // The runner fails (mdfind absent / Spotlight indexing off) -> honestly
        // NO candidates, and availability stays false (only the LIVE runner may
        // ever arm it; the injected one never touches the flag).
        let roots = vec![root.display().to_string()];
        assert!(candidates("query", &roots, 64, down).await.is_empty());
        assert!(!spotlight_available(), "no live success ever ran under test");
        // Empty roots -> no invocation at all (the runner is never called).
        let calls = Mutex::new(Vec::new());
        let found = candidates("query", &[], 64, canned("/tmp/x.md\n", &calls)).await;
        assert!(found.is_empty());
        assert!(calls.lock().unwrap().is_empty(), "no roots -> mdfind is NEVER run");
    }

    // =====================================================================
    // FILE METADATA — injected runner; failure = honest absence
    // =====================================================================

    #[tokio::test]
    async fn file_metadata_reads_via_the_injected_runner_and_degrades_honestly() {
        let calls = Mutex::new(Vec::new());
        let out = "kMDItemContentType  = \"public.plain-text\"\n\
                   kMDItemLastUsedDate = (null)\nkMDItemAuthors = (null)\n";
        let meta = file_metadata(Path::new("/tmp/a.txt"), canned(out, &calls)).await;
        assert_eq!(meta.content_type.as_deref(), Some("public.plain-text"));
        assert_eq!(meta.last_used, None, "(null) stays absent — never fabricated");
        assert_eq!(meta.authors, None);
        {
            let calls = calls.lock().unwrap();
            assert_eq!(calls[0].0, MDLS, "metadata reads use mdls only");
            assert_eq!(calls[0].1.last().unwrap(), "/tmp/a.txt");
        }
        // A failed mdls run -> EVERY field absent (honest degradation).
        let meta = file_metadata(Path::new("/tmp/a.txt"), down).await;
        assert!(meta.is_empty(), "an unreadable file has NO metadata, not fake metadata");
    }

    // =====================================================================
    // AVAILABILITY — honest false paths
    // =====================================================================

    #[test]
    fn availability_requires_presence_and_a_recent_success() {
        // Both legs required: a present binary whose last attempt failed (or
        // that never answered) is NOT available, and a recorded success without
        // the binary (deleted since) is not either.
        assert!(!availability(false, false));
        assert!(!availability(true, false), "present but last attempt failed -> false");
        assert!(!availability(false, true), "succeeded but binary gone -> false");
        assert!(availability(true, true));
    }

    #[test]
    fn health_tracks_the_most_recent_attempt_never_sticking_on_a_success() {
        // A LOCAL health flag (the process-wide static is never touched by a
        // test): availability must follow the LAST attempt, so a success that
        // is later followed by a failure — Spotlight indexing turned off
        // mid-session, mdfind wedged — flips honestly back to false.
        let health = MdfindHealth::new();
        assert!(!health.last_ok(), "no attempt yet -> false");
        health.record(true);
        assert!(availability(true, health.last_ok()), "a real success claims it");
        health.record(false);
        assert!(
            !availability(true, health.last_ok()),
            "a later FAILURE must flip availability back to false — never sticky"
        );
        health.record(true);
        assert!(availability(true, health.last_ok()), "…and a recovery re-claims it");
    }

    #[test]
    fn reported_availability_is_off_when_the_config_flag_is_off() {
        // The status emission must never claim the integration while the
        // operator has [docsearch].spotlight off — whatever the live health
        // says. (Under test the live health is untouched, so the enabled leg
        // simply mirrors spotlight_available().)
        assert!(!reported_available(false), "config OFF -> reported unavailable");
        assert_eq!(reported_available(true), spotlight_available());
    }

    // =====================================================================
    // BOUNDED CAPTURE — the live runner's read primitive on synthetic streams
    // =====================================================================

    #[tokio::test]
    async fn read_capped_bounds_the_bytes_ever_held_from_a_huge_stream() {
        // A 4 MiB "child stdout" against a 1 KiB ceiling: the read stops AT the
        // ceiling and reports the hit (the live runner then kills the child) —
        // the bytes held never exceed the cap, however large the stream.
        let huge = vec![b'x'; 4 << 20];
        let mut reader: &[u8] = &huge;
        let (buf, hit_cap) = read_capped(&mut reader, 1024).await;
        assert_eq!(buf.len(), 1024, "exactly the ceiling is ever held");
        assert!(hit_cap, "the ceiling hit must be reported so the child is killed");

        // A small stream is read whole, ceiling untouched.
        let mut small: &[u8] = b"/Users/me/docs/a.md\n";
        let (buf, hit_cap) = read_capped(&mut small, 1024).await;
        assert_eq!(buf, b"/Users/me/docs/a.md\n");
        assert!(!hit_cap, "EOF within the ceiling is not a cap hit");

        // An empty stream yields empty, honestly.
        let mut empty: &[u8] = b"";
        let (buf, hit_cap) = read_capped(&mut empty, 1024).await;
        assert!(buf.is_empty() && !hit_cap);
    }

    #[tokio::test]
    async fn read_capped_survives_a_dribbling_stream_without_exceeding_the_cap() {
        // A slow producer that dribbles one small chunk per read (tokio's
        // chained reader yields each part separately) still lands exactly at
        // the ceiling — the cap holds ACROSS reads, not just per read.
        let part1: &[u8] = &[b'a'; 300];
        let part2: &[u8] = &[b'b'; 300];
        let part3: &[u8] = &[b'c'; 300];
        let mut chained = tokio::io::AsyncReadExt::chain(
            part1,
            tokio::io::AsyncReadExt::chain(part2, part3),
        );
        let (buf, hit_cap) = read_capped(&mut chained, 500).await;
        assert_eq!(buf.len(), 500, "the cap holds across incremental reads");
        assert!(hit_cap);
    }
}
