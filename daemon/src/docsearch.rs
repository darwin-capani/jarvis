//! ON-DEVICE FILE RAG — the confined indexer + the cited search over the user's
//! OWN text-like files. 100% on-device; nothing here ever reaches the network.
//!
//! This is the read-only document-retrieval counterpart to MNEMOSYNE's fact +
//! episodic recall. It walks the EXPLICITLY-ALLOWLISTED `[docsearch].roots` with
//! `std::fs` (no new dependency), chunks each accepted file into overlapping
//! windows that keep a citation offset, embeds the chunks ON-DEVICE via the
//! inference `embed` op (falling back to lexical BM25 when that server is down),
//! stores the chunks (and any vectors) in a BOUNDED local SQLite table, and
//! answers a query with CITED top-k results (file path + snippet + score),
//! reporting WHICH ranking backend actually ran.
//!
//! ## The CONTRACT (non-negotiable — this reads the user's OWN files)
//!   * PRIVACY: file CONTENTS + EMBEDDINGS NEVER LEAVE THE DEVICE. Embedding is the
//!     on-device MLX embed op ([`crate::inference::InferenceClient::embed`]);
//!     nothing is uploaded. Search degrades to lexical BM25 when the embedder is
//!     down and reports which ran ([`crate::recall::RankMethod`]) — it never claims
//!     neural on fallback.
//!   * CONFINED: the index reads ONLY files under an allowlisted root. Every
//!     candidate is PATH-CONFINED ([`confine`]): canonicalize it, then assert the
//!     real path starts_with a canonicalized allowed root. A symlink that escapes a
//!     root, a `..` traversal, and an absolute-elsewhere path all RESOLVE OUTSIDE
//!     the root and are REJECTED. There is NO whole-disk scan: an empty `roots`
//!     allowlist indexes nothing even with `enabled` true.
//!   * BOUNDED: total files / total chunks / total bytes are capped, plus a
//!     per-file size cap and a recursion-depth bound. The store is finite.
//!   * FORGETTABLE: [`DocIndex::forget`] clears the index (a user can make JARVIS
//!     forget every indexed file).
//!   * HONEST: a search returns ONLY chunks that were really indexed (the snippet
//!     is the stored chunk text, the citation is its real file + offset). An empty
//!     index or a no-match query returns NOTHING — never a fabricated citation.
//!   * ON by default but INERT WITHOUT ROOTS: gated by `[docsearch].enabled` (ships
//!     true) AND a non-empty `roots` (ships empty). The daemon checks both before ever
//!     indexing, so even enabled it indexes NOTHING until a folder is allowlisted.
//!
//! Beyond TEXT-LIKE files (an extension allowlist), the indexer also extracts
//! TEXT from born-digital PDFs and Office documents (.docx / .xlsx / .pptx) via
//! pure-Rust, ON-DEVICE extractors ([`extract_text`]). Every extractor runs behind
//! a PANIC-SAFE HONEST-SKIP boundary ([`extract_guarded`]): a malformed / encrypted
//! / scanned / image-only file — or one whose parser PANICS — is SKIPPED with a
//! logged reason and is NEVER indexed as empty/garbage and NEVER crashes the walk.
//! Other binaries (images, archives, ...) remain out of scope and are skipped.

use std::collections::HashSet;
use std::io::{Cursor, Read};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use crate::recall::{cosine_similarity, Bm25Params, Embedder, Fact, LexicalProvider, RankMethod};

/// The TEXT-LIKE extension allowlist (lowercased, no dot): prose/notes + common
/// source/config formats whose bytes ARE the text (read directly, no extractor).
/// Images, archives, and other binaries are skipped. PDFs and Office documents are
/// handled separately via their on-device extractors ([`extract_text`]).
pub const ALLOWED_EXTENSIONS: &[&str] = &[
    // prose / notes
    "md", "markdown", "txt", "text", "rst", "org", "tex", "log",
    // config / data
    "toml", "yaml", "yml", "json", "ini", "cfg", "conf", "csv", "tsv", "env",
    // code
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "kt", "c", "h", "cpp",
    "cc", "hpp", "rb", "sh", "bash", "zsh", "sql", "php", "swift", "scala",
    "lua", "pl", "r", "m", "html", "htm", "css", "scss", "xml",
];

/// Born-digital PDF: extracted to UTF-8 text on-device via [`pdf_text`]. A
/// scanned/image-only/encrypted/malformed PDF yields no usable text and is
/// HONEST-SKIPPED (never indexed empty).
pub const PDF_EXTENSIONS: &[&str] = &["pdf"];

/// Office Open XML documents (ZIP-of-XML): word processing, spreadsheet,
/// presentation. Text is pulled from their XML parts on-device via [`office_text`].
/// The legacy binary formats (.doc/.xls/.ppt) are NOT OOXML and remain out of scope.
pub const OFFICE_EXTENSIONS: &[&str] = &["docx", "xlsx", "pptx"];

/// Default / max number of results a single [`DocIndex::search`] returns. Bounded
/// so a search is small and focused on the relevant few.
pub const DOCSEARCH_DEFAULT_K: usize = 5;
pub const DOCSEARCH_MAX_K: usize = 20;

/// How many characters of a chunk are returned as the citation SNIPPET (the full
/// chunk is stored; the snippet is a bounded preview for display).
const SNIPPET_CHARS: usize = 280;

// ---------------------------------------------------------------------------
// Bounds — the finite ceilings on a walk/index, mirrored from [docsearch] config
// ---------------------------------------------------------------------------

/// The bounded parameters of one index pass, lifted from [`crate::config::DocSearchConfig`]
/// so the indexer is testable without a full Config. All are real, finite ceilings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexBounds {
    pub max_files: usize,
    pub max_chunks: usize,
    pub max_file_bytes: usize,
    pub max_depth: usize,
    pub chunk_chars: usize,
    pub chunk_overlap: usize,
}

impl IndexBounds {
    /// Build bounds from the parsed config section, clamping to safe minimums so a
    /// degenerate config (e.g. chunk_chars 0, or overlap >= chunk size) can never
    /// produce a non-terminating chunker or a zero-progress walk.
    pub fn from_config(c: &crate::config::DocSearchConfig) -> Self {
        let chunk_chars = c.chunk_chars.max(64);
        // Overlap must be strictly less than the window or chunking never advances.
        let chunk_overlap = c.chunk_overlap.min(chunk_chars.saturating_sub(1));
        Self {
            max_files: c.max_files.max(1),
            max_chunks: c.max_chunks.max(1),
            max_file_bytes: c.max_file_bytes.max(1),
            max_depth: c.max_depth.max(1),
            chunk_chars,
            chunk_overlap,
        }
    }
}

#[cfg(test)]
impl Default for IndexBounds {
    fn default() -> Self {
        Self::from_config(&crate::config::DocSearchConfig::default())
    }
}

// ---------------------------------------------------------------------------
// PATH CONFINEMENT — the red-team-validated no-escape check
// ---------------------------------------------------------------------------

/// Canonicalize each configured root once. A root that does not exist / cannot be
/// canonicalized is DROPPED (it can confine nothing), so a typo'd or missing root
/// silently indexes nothing rather than widening the surface. Returns the real,
/// absolute, symlink-resolved roots.
pub fn canonical_roots(roots: &[String]) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect()
}

/// PATH CONFINEMENT (the security primitive). Given a candidate path and the
/// already-canonicalized allowed roots, return the candidate's REAL path IFF it
/// resolves to a location inside one of the roots — else `None` (REJECTED).
///
/// `std::fs::canonicalize` resolves symlinks and `..` and makes the path
/// absolute, so:
///   * a symlink under a root that points OUTSIDE the root canonicalizes to its
///     real (outside) location, which fails the `starts_with` -> REJECTED;
///   * a `..` traversal resolves to the real parent -> REJECTED if outside;
///   * an absolute-elsewhere path canonicalizes to itself -> REJECTED;
///   * a file genuinely under a root canonicalizes to under the (canonicalized)
///     root -> ACCEPTED.
/// A non-existent path cannot be canonicalized -> `None` (we never index a path we
/// cannot prove resolves inside a root). The check is on the REAL path, never the
/// lexical/symlink path, so it cannot be fooled by a crafted name.
pub fn confine(candidate: &Path, canonical_roots: &[PathBuf]) -> Option<PathBuf> {
    let real = std::fs::canonicalize(candidate).ok()?;
    if canonical_roots.iter().any(|root| real.starts_with(root)) {
        Some(real)
    } else {
        None
    }
}

/// Whether a path component is a hidden entry (dotfile/dotdir) we skip — except
/// the root itself, which the walk never passes here. `.` / `..` are never walked
/// (read_dir yields neither), so any leading-dot name is a real hidden entry.
fn is_hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// How a discovered, allowlisted file is turned into indexable text. The walk only
/// accepts files that classify to a non-`Unsupported` kind; the indexer routes each
/// to the matching reader/extractor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Bytes ARE the text — read directly + UTF-8-lossy decoded (today's path).
    Text,
    /// Born-digital PDF — text via the on-device [`pdf_text`] extractor.
    Pdf,
    /// Office Open XML (.docx/.xlsx/.pptx) — text via the on-device [`office_text`]
    /// extractor. Carries which OOXML family so the extractor reads the right parts.
    Office(OfficeKind),
}

/// Which Office Open XML family a `.docx/.xlsx/.pptx` file is, selecting which ZIP
/// member XML parts the extractor mines for text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeKind {
    /// Word: text runs (`<w:t>`) in `word/document.xml`.
    Docx,
    /// Excel: shared strings (`<t>`) + inline cell strings across the workbook.
    Xlsx,
    /// PowerPoint: text runs (`<a:t>`) across `ppt/slides/slide*.xml`.
    Pptx,
}

/// Classify a path by its (lowercased) extension into the [`FileKind`] that decides
/// how it is read, or `None` when the extension is on no indexable list (binaries,
/// images, archives, no-extension files — all skipped). Pure metadata: never reads
/// the file.
fn classify(path: &Path) -> Option<FileKind> {
    let ext = path.extension().and_then(|e| e.to_str())?.to_lowercase();
    let ext = ext.as_str();
    if ALLOWED_EXTENSIONS.contains(&ext) {
        Some(FileKind::Text)
    } else if PDF_EXTENSIONS.contains(&ext) {
        Some(FileKind::Pdf)
    } else if OFFICE_EXTENSIONS.contains(&ext) {
        match ext {
            "docx" => Some(FileKind::Office(OfficeKind::Docx)),
            "xlsx" => Some(FileKind::Office(OfficeKind::Xlsx)),
            "pptx" => Some(FileKind::Office(OfficeKind::Pptx)),
            _ => None, // unreachable: OFFICE_EXTENSIONS is exactly these three
        }
    } else {
        None
    }
}

/// Whether a file's extension is on ANY indexable list — text-like OR a format with
/// an on-device extractor (PDF / Office). No extension, or an unlisted extension
/// (image/archive/other binary), is rejected by the walk before any read.
fn extension_allowed(path: &Path) -> bool {
    classify(path).is_some()
}

/// A fast binary sniff: a file is treated as binary (and skipped) if its first
/// bytes contain a NUL. Text-like files never carry an interior NUL; this catches
/// a mislabeled binary that slipped through the extension allowlist.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

// ---------------------------------------------------------------------------
// EXTRACTORS — born-digital PDF + Office (.docx/.xlsx/.pptx) -> UTF-8 text
//
// PANIC-SAFE + HONEST-SKIP CONTRACT: every extractor is reached only through
// [`extract_guarded`], which runs it inside `catch_unwind` and maps a panic, an
// extractor error, or an empty/whitespace-only result to `Ok(None)` (a logged
// HONEST SKIP). The indexer NEVER indexes such a file empty/garbage, and a single
// bad file NEVER crashes the walk. The text is then capped to the bounds before
// chunking, so a huge document cannot blow the chunk store.
// ---------------------------------------------------------------------------

/// The ZIP member XML parts an Office family exposes text in, and the local element
/// names whose `Text` events carry the visible text. Returning `None` for a part
/// list means "scan every `*.xml` part matching the prefix predicate".
struct OfficeSpec {
    /// `(member-path-predicate, text-element-local-names)`. A member is mined when
    /// the predicate matches its name; within it, only `Text` directly inside one of
    /// the listed local element names is collected (so styles/metadata are ignored).
    text_elements: &'static [&'static str],
    /// Whether a given ZIP member path should be mined for this family.
    wants_member: fn(&str) -> bool,
}

fn docx_wants(name: &str) -> bool {
    // The main document body, plus headers/footers which also hold visible text.
    name == "word/document.xml"
        || (name.starts_with("word/header") && name.ends_with(".xml"))
        || (name.starts_with("word/footer") && name.ends_with(".xml"))
}

fn xlsx_wants(name: &str) -> bool {
    // Shared strings hold most cell text; worksheets hold inline strings + the
    // (numeric/string) cell values. Both expose visible text via `<t>`.
    name == "xl/sharedStrings.xml"
        || (name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
}

fn pptx_wants(name: &str) -> bool {
    // Slides plus their notes — all the readable text of a deck.
    (name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
        || (name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml"))
}

fn office_spec(kind: OfficeKind) -> OfficeSpec {
    match kind {
        OfficeKind::Docx => OfficeSpec {
            text_elements: &["t"], // <w:t> — local name after namespace strip is "t"
            wants_member: docx_wants,
        },
        OfficeKind::Xlsx => OfficeSpec {
            text_elements: &["t"], // <t> in sharedStrings + inline <is><t>
            wants_member: xlsx_wants,
        },
        OfficeKind::Pptx => OfficeSpec {
            text_elements: &["t"], // <a:t>
            wants_member: pptx_wants,
        },
    }
}

/// The local element name of a (possibly namespace-prefixed) XML tag: the part
/// after the last `:` (so `w:t` -> `t`, `a:t` -> `t`, `t` -> `t`). Bytes only, no
/// allocation in the common path.
fn local_name(qname: &[u8]) -> &[u8] {
    match qname.iter().rposition(|&b| b == b':') {
        Some(i) => &qname[i + 1..],
        None => qname,
    }
}

/// Pull the visible text out of ONE Office XML part. Only `Text` events that sit
/// directly inside an element whose local name is in `text_elements` are collected;
/// a `<w:p>` paragraph end (docx) and any worksheet/slide boundary insert a newline
/// so words/cells/runs do not run together. Bounded: stops once `cap` chars are
/// gathered (the caller caps again, but stopping early avoids buffering a giant
/// part). Returns the gathered text. Pure XML — never reads the disk or network.
fn extract_office_part(xml: &[u8], text_elements: &[&str], cap: usize) -> String {
    use quick_xml::events::Event;
    use quick_xml::reader::Reader;
    let mut reader = Reader::from_reader(xml);
    // Tolerate the slightly-off XML real documents carry; we only read text.
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let mut depth_in_text: u32 = 0;
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let ln = local_name(name.as_ref());
                if text_elements.iter().any(|t| t.as_bytes() == ln) {
                    depth_in_text += 1;
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let ln = local_name(name.as_ref());
                if text_elements.iter().any(|t| t.as_bytes() == ln) {
                    depth_in_text = depth_in_text.saturating_sub(1);
                } else if ln == b"p" || ln == b"br" || ln == b"tab" {
                    // Word paragraph / break / tab -> whitespace boundary.
                    out.push('\n');
                } else if ln == b"row" || ln == b"c" {
                    // Spreadsheet row / cell -> whitespace boundary so cells split.
                    out.push(' ');
                }
            }
            Ok(Event::Text(t)) if depth_in_text > 0 => {
                if let Ok(s) = t.unescape() {
                    // Append only up to the remaining budget so a single huge text
                    // run cannot blow past `cap` (truncated on a char boundary).
                    let remaining = cap.saturating_sub(out.len());
                    if s.len() <= remaining {
                        out.push_str(&s);
                        if out.len() < cap {
                            out.push(' ');
                        }
                    } else {
                        let mut end = remaining;
                        while end > 0 && !s.is_char_boundary(end) {
                            end -= 1;
                        }
                        out.push_str(&s[..end]);
                    }
                }
            }
            Ok(Event::Eof) => break,
            // A malformed event ends THIS part's extraction with whatever we have —
            // never a panic, never garbage (we only ever appended real text).
            Err(_) => break,
            _ => {}
        }
        buf.clear();
        if out.len() >= cap {
            break;
        }
    }
    out
}

/// Extract text from an Office Open XML document already loaded into `bytes`.
/// Opens the ZIP in memory, reads ONLY the family's text-bearing parts (sorted for
/// determinism), and concatenates their extracted text up to `cap` chars. Returns
/// the text (possibly empty — the guard treats empty as an HONEST SKIP). An
/// encrypted OOXML file (these are actually a CDF/OLE container, not a ZIP) fails
/// `ZipArchive::new` and yields an error here -> skip. Pure: no disk/network.
fn office_text(bytes: &[u8], kind: OfficeKind, cap: usize) -> Result<String> {
    let spec = office_spec(kind);
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .context("not a readable OOXML zip (corrupt or encrypted)")?;
    // Collect + sort the wanted member names so slide1 precedes slide2, etc.
    let mut members: Vec<String> = archive
        .file_names()
        .filter(|n| (spec.wants_member)(n))
        .map(|n| n.to_string())
        .collect();
    members.sort();
    // ZIP-BOMB GUARD. A member's UNCOMPRESSED size is attacker-controlled — a zip
    // bomb inflates a few KB of compressed data to gigabytes, and the compressed
    // file passed only the `max_file_bytes` gate (that bounds the ZIP on disk, NOT
    // any member's decompressed stream). An unbounded `read_to_end` on the
    // decompressing reader would follow the bomb to OOM. So we cap TOTAL
    // decompressed bytes read across the whole document with a running budget and
    // `Read::take` (which bounds the ACTUAL bytes read regardless of the member's
    // declared, forgeable uncompressed size). We never need more XML than yields
    // `cap` text chars (markup inflates that), so 64× cap is generous headroom;
    // floored so a small cap still reads real parts whole, ceilinged so no
    // document — however many members, however large each claims — exhausts memory.
    // A bomb hits the budget and we return the real text extracted so far.
    let mut budget_left: u64 = (cap as u64).saturating_mul(64).clamp(16 << 20, 256 << 20);
    let mut out = String::new();
    for name in members {
        if out.len() >= cap || budget_left == 0 {
            break;
        }
        let Ok(part) = archive.by_name(&name) else {
            continue;
        };
        let mut xml = Vec::new();
        if part.take(budget_left).read_to_end(&mut xml).is_err() {
            continue; // a bad member is skipped, not fatal
        }
        budget_left = budget_left.saturating_sub(xml.len() as u64);
        let remaining = cap.saturating_sub(out.len());
        let piece = extract_office_part(&xml, spec.text_elements, remaining);
        if !piece.is_empty() {
            out.push_str(&piece);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Ceiling on TOTAL decompressed FlateDecode output for one PDF — the same 256 MiB
/// bound `office_text` caps its ZIP members to. A born-digital PDF's real streams
/// (text/fonts) inflate far below this; a decompression bomb does not.
const PDF_DECOMPRESS_CEILING: u64 = 256 << 20;

/// The MEMORY-JAIL helper binary name — a sibling of `jarvisd` in the shipped
/// layout (`daemon/target/release/`). See [`locate_pdfjail`] and `src/bin/pdfjail.rs`.
const PDFJAIL_BIN: &str = "pdfjail";

/// Hard wall-clock ceiling on ONE jailed extraction. A memory bomb aborts the
/// child almost immediately (its RLIMIT_AS trips), so this really bounds a
/// CPU-bound hostile PDF that neither finishes nor allocates — killed + skipped
/// here. Generous for any real born-digital document.
const PDFJAIL_TIMEOUT: Duration = Duration::from_secs(20);

/// Ceiling on the extracted TEXT accepted back from the helper. The text is capped
/// again to the index bounds before chunking; this bounds what the parent buffers
/// from a runaway child and flags an implausibly large result as a skip.
const PDFJAIL_MAX_OUTPUT: usize = 64 << 20;

/// Inflate one FlateDecode (zlib) stream's RAW `content` through the REMAINING
/// budget, adding decompressed bytes to `spent`. Returns `false` the moment `spent`
/// exceeds `budget` (a suspected bomb). Bounded to O(8 KiB) memory via a fixed
/// scratch buffer; `.take(remaining+1)` caps this one stream's contribution and the
/// `+1` makes hitting the budget from a single stream detectable. A non-Flate blob
/// (image / other filter / raw) fails to inflate and contributes 0 — the caller
/// treats that as "no measurable inflation," not "safe."
fn flate_stream_within_budget(content: &[u8], budget: u64, spent: &mut u64) -> bool {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let remaining = budget.saturating_sub(*spent).saturating_add(1);
    let mut dec = ZlibDecoder::new(content).take(remaining);
    let mut scratch = [0u8; 8192];
    loop {
        match dec.read(&mut scratch) {
            Ok(0) => return true, // stream finished within budget
            Ok(k) => {
                *spent = spent.saturating_add(k as u64);
                if *spent > budget {
                    return false; // suspected decompression bomb
                }
            }
            // Partial/invalid stream: we have counted whatever inflated so far, which
            // is enough for the budget check; stop this stream.
            Err(_) => return true,
        }
    }
}

/// DECOMPRESSION-BOMB GUARD for PDFs. `pdf-extract` (via lopdf) inflates a PDF's
/// FlateDecode content/object streams with NO ratio cap, so a ~2 MiB PDF whose
/// stream inflates to ~2 GB (DEFLATE reaches ~1032x) passes the compressed-size gate
/// yet OOMs the daemon during reindex — and a Rust allocation abort does NOT unwind,
/// so the `catch_unwind` skip boundary cannot contain it (unlike a parser panic). So
/// BEFORE handing bytes to pdf-extract we parse the PDF STRUCTURE with lopdf (which
/// does NOT decompress on load, and is bounded by the file size), iterate the REAL
/// stream objects — whose `content` has correct `/Length`-parsed boundaries, so
/// there is no "endstream-in-data" byte-scan evasion and no unbounded stream count —
/// and inflate each through a shared byte BUDGET with flate2 (O(8 KiB) memory). If
/// the total would exceed `budget` the PDF is a suspected bomb and we return false
/// -> pdf_text errors -> honest skip. If lopdf cannot PARSE the file we return true:
/// pdf-extract (which also wraps lopdf) cannot inflate what it cannot parse, so it
/// will error safely on the same input.
///
/// RESIDUALS this in-process guard cannot reach (both CLOSED in production by the
/// memory-jail subprocess — see [`pdf_text`] / `src/bin/pdfjail.rs`; they persist
/// ONLY on the in-process FALLBACK path, a dev/test build without the built helper):
///   1. FILTER-CHAIN ARMOR — a bomb behind a non-Flate chain (`ASCII85Decode` /
///      `LZWDecode` -> `FlateDecode`) has non-zlib `content` here, so its inner
///      inflation is not measured and a *crafted* filter-chained bomb can still
///      reach pdf-extract's uncapped inflate.
///   2. STRUCTURAL-STREAM (parse-time) BOMBS — a bomb inside a cross-reference
///      stream (XRefStm) or object stream (ObjStm) is decompressed by the PARSER
///      itself (both this `load_mem` AND pdf-extract's own load must decode those to
///      read the file). lopdf 0.38 exposes no decompression-size limit on load, so
///      such a bomb OOMs during parse — and NO in-process guard can prevent it,
///      because pdf-extract re-parses the same bytes unbounded regardless of what we
///      check first (a Rust alloc-abort does not unwind, so `catch_unwind` can't
///      contain it either).
/// The complete, filter- and structure-agnostic fix — a memory-jailed extraction
/// subprocess (`RLIMIT_AS` on a short-lived child, [`pdf_text_jailed`]) — is now
/// the PRODUCTION path: a bomb of any shape aborts the CHILD, never jarvisd. This
/// in-process guard remains the cheap FIRST-LINE defense on the fallback path
/// (closing the common single-FlateDecode CONTENT-stream bomb with reliable
/// boundaries); the two residuals above only matter when the helper is absent.
fn pdf_decompression_within_budget(bytes: &[u8], budget: u64) -> bool {
    let Ok(doc) = lopdf::Document::load_mem(bytes) else {
        return true; // unparseable -> pdf-extract will error safely, not OOM
    };
    let mut spent: u64 = 0;
    for obj in doc.objects.values() {
        if let lopdf::Object::Stream(stream) = obj {
            if !flate_stream_within_budget(&stream.content, budget, &mut spent) {
                return false;
            }
        }
    }
    true
}

/// Extract UTF-8 text from a born-digital PDF already loaded into `bytes`.
///
/// PRODUCTION uses a MEMORY-JAILED HELPER SUBPROCESS ([`pdf_text_jailed`]): the
/// child arms `RLIMIT_AS` before decoding, so a decompression bomb of ANY shape
/// (single-Flate content, filter-chain-armored, or a parse-time XRef/ObjStm
/// structural bomb) aborts the CHILD, never jarvisd — closing the residuals the
/// in-process guard cannot ([`pdf_decompression_within_budget`]). When the helper
/// is absent (a dev/test build whose `current_exe` is the test harness, or a
/// broken install) we FALL BACK to the in-process guard + `pdf-extract`
/// ([`pdf_text_in_process`]) and warn once.
///
/// A scanned/image-only PDF yields (effectively) empty text -> HONEST SKIP; an
/// encrypted/malformed PDF errors -> skip; a suspected bomb is refused -> skip.
/// Pure w.r.t. disk/network here (`bytes` were already read by the caller).
fn pdf_text(bytes: &[u8]) -> Result<String> {
    match locate_pdfjail() {
        Some(helper) => pdf_text_jailed(&helper, bytes),
        None => {
            warn_missing_jail_once();
            pdf_text_in_process(bytes)
        }
    }
}

/// The IN-PROCESS fallback (a dev/test build without the built helper, or a broken
/// install). Runs the cheap flate2 decompression-budget probe as a first-line
/// defense, then `pdf-extract`. This path carries the KNOWN RESIDUALS documented on
/// [`pdf_decompression_within_budget`]; the shipped daemon always spawns the jail.
fn pdf_text_in_process(bytes: &[u8]) -> Result<String> {
    if !pdf_decompression_within_budget(bytes, PDF_DECOMPRESS_CEILING) {
        anyhow::bail!(
            "pdf skipped: FlateDecode streams exceed the {PDF_DECOMPRESS_CEILING}-byte \
             decompression budget (suspected decompression bomb)"
        );
    }
    pdf_extract::extract_text_from_mem(bytes).context("pdf text extraction failed")
}

/// Locate the memory-jail helper next to the RUNNING executable. In the shipped
/// layout `jarvisd` and `pdfjail` are siblings in `daemon/target/release/`, so the
/// helper is `current_exe().parent()/pdfjail`. Returns `None` when it is not there
/// (a `cargo test` build, whose `current_exe` is the test harness under
/// `target/.../deps/` with no `pdfjail` sibling, or a broken install) -> the caller
/// uses the in-process fallback. We check ONLY the direct sibling and NEVER re-exec
/// `current_exe` (which under `cargo test` is the test harness and would recurse).
fn locate_pdfjail() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let helper = exe.parent()?.join(PDFJAIL_BIN);
    helper.is_file().then_some(helper)
}

/// Whether the memory-jail helper is present next to the RUNNING executable — the
/// runtime counterpart to selfcheck's install-tree path probe (this one answers
/// "will THIS process find the jail", which differs in a dev checkout). Surfaced
/// on the HUD/telemetry status so a process silently on the weaker in-process
/// guard is visible; unit-tested here.
#[allow(dead_code)] // consumed by the status/telemetry surface + the unit test below
pub fn pdfjail_available() -> bool {
    locate_pdfjail().is_some()
}

/// Warn ONCE per process that the memory-jail helper is absent and PDF extraction
/// is on the weaker in-process guard. Expected + benign in a dev/test build;
/// alarming in a production install (also surfaced on the selftest board).
fn warn_missing_jail_once() {
    use std::sync::Once;
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            target: "docsearch",
            helper = PDFJAIL_BIN,
            "PDF memory-jail helper not found next to the executable; falling back to \
             the in-process guard (known residuals: filter-chain + parse-time \
             structural-stream bombs). A production install should ship the helper."
        );
    });
}

/// Extract PDF text in the MEMORY-JAILED helper subprocess. Spawns `pdfjail`,
/// streams the PDF bytes to its stdin, and reads the extracted text from its
/// stdout under a wall-clock timeout ([`PDFJAIL_TIMEOUT`]) and an output-size cap
/// ([`PDFJAIL_MAX_OUTPUT`]). The child arms `RLIMIT_AS` before decoding, so ANY
/// decompression bomb makes an allocation in the CHILD fail and ABORT it; the
/// abort NEVER reaches jarvisd. A non-zero/killed exit, a timeout, or oversize
/// output all become an error the guard treats as an HONEST SKIP. The writer and
/// reader run on their own threads so a full stdin pipe can never deadlock against
/// a full stdout pipe.
fn pdf_text_jailed(helper: &Path, bytes: &[u8]) -> Result<String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning the pdf memory-jail helper at {}", helper.display()))?;
    let mut stdin = child.stdin.take().context("pdfjail stdin was not captured")?;
    let stdout = child.stdout.take().context("pdfjail stdout was not captured")?;

    // Feed the PDF bytes on a writer thread. A broken pipe (the child aborted
    // early on a bomb) is expected and ignored — the exit status decides the
    // outcome. stdin is dropped at the end of the closure -> EOF for the child.
    let payload = bytes.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(&payload);
    });
    // Drain stdout on a reader thread, bounded to the cap + 1 byte so an oversize
    // result is DETECTED without buffering unbounded text, and so the child never
    // blocks on a full stdout pipe while we wait.
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take(PDFJAIL_MAX_OUTPUT as u64 + 1)
            .read_to_end(&mut buf);
        buf
    });

    // Watchdog: poll for exit up to the timeout, then kill a hung child. Killing
    // closes the pipes, which unblocks the reader/writer threads below.
    let deadline = std::time::Instant::now() + PDFJAIL_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None; // timed out
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let _ = writer.join();
    let out = reader.join().unwrap_or_default();

    match status {
        Some(s) if s.success() => {
            if out.len() > PDFJAIL_MAX_OUTPUT {
                anyhow::bail!(
                    "pdf skipped: jailed extraction produced more than the \
                     {PDFJAIL_MAX_OUTPUT}-byte text cap (suspected bomb)"
                );
            }
            Ok(String::from_utf8_lossy(&out).into_owned())
        }
        Some(_) => anyhow::bail!(
            "pdf skipped: memory-jailed extraction exited non-zero (a bomb aborted \
             the child, or the PDF is corrupt/encrypted/panicked the parser)"
        ),
        None => anyhow::bail!("pdf skipped: memory-jailed extraction timed out"),
    }
}

/// THE PANIC-SAFE HONEST-SKIP BOUNDARY for every non-text-like extractor.
///
/// Runs `f` (a PDF/Office extractor over already-read bytes) inside `catch_unwind`,
/// so even a parser PANIC on a hostile/malformed file is contained — it becomes an
/// `Ok(None)` skip, never a crash of the indexer. Mapping:
///   * `Ok(Ok(text))` with non-whitespace text -> `Some(text)` (INDEX it);
///   * `Ok(Ok(text))` that is empty/whitespace -> `None` (HONEST SKIP: a
///     scanned/image-only doc has no text layer — we never index it empty);
///   * `Ok(Err(e))` (extractor error: corrupt/encrypted/unreadable) -> `None`;
///   * `Err(panic)` (the parser panicked) -> `None`.
/// Every `None` is logged with `path` + `reason` so a skip is HONEST + auditable.
/// `AssertUnwindSafe` is sound here: `f` borrows only `&[u8]` and builds a fresh
/// `String`; nothing observable is left in a broken state after a unwound panic.
fn extract_guarded<F>(path: &Path, label: &str, f: F) -> Option<String>
where
    F: FnOnce() -> Result<String>,
{
    let caught = catch_unwind(AssertUnwindSafe(f));
    match caught {
        Ok(Ok(text)) if !text.trim().is_empty() => Some(text),
        Ok(Ok(_)) => {
            tracing::debug!(
                target: "docsearch",
                path = %path.display(),
                kind = label,
                "skipped: no extractable text (likely scanned/image-only or empty)"
            );
            None
        }
        Ok(Err(e)) => {
            tracing::warn!(
                target: "docsearch",
                path = %path.display(),
                kind = label,
                error = %e,
                "skipped: extraction failed (corrupt/encrypted/unreadable)"
            );
            None
        }
        Err(_) => {
            tracing::warn!(
                target: "docsearch",
                path = %path.display(),
                kind = label,
                "skipped: extractor PANICKED on this file (contained, not fatal)"
            );
            None
        }
    }
}

/// Turn one discovered file's RAW BYTES into indexable UTF-8 text per its
/// [`FileKind`], or `None` to HONEST-SKIP it. The single per-file extraction entry
/// the indexer calls:
///   * `Text`  -> a mislabeled-binary NUL sniff then UTF-8-lossy decode (today's
///               behavior, byte-for-byte — text-like handling UNCHANGED);
///   * `Pdf`   -> [`pdf_text`] behind the panic-safe guard;
///   * `Office`-> [`office_text`] behind the panic-safe guard.
/// `cap` bounds the extracted text length BEFORE chunking (the chunk store's
/// `max_chunks` is the other ceiling). Pure w.r.t. disk/network: `bytes` were
/// already read by the caller; nothing here reads a file or reaches the network.
fn extract_text(path: &Path, kind: FileKind, bytes: &[u8], cap: usize) -> Option<String> {
    let text = match kind {
        FileKind::Text => {
            if looks_binary(bytes) {
                // A mislabeled binary that slipped the extension allowlist — skip
                // (unchanged from the original text-like path).
                tracing::debug!(
                    target: "docsearch",
                    path = %path.display(),
                    "skipped: text-like file is actually binary (NUL sniff)"
                );
                return None;
            }
            // Lossy UTF-8: a stray invalid byte becomes U+FFFD rather than dropping
            // the whole file — we still index the readable text (unchanged).
            String::from_utf8_lossy(bytes).into_owned()
        }
        FileKind::Pdf => extract_guarded(path, "pdf", || pdf_text(bytes))?,
        FileKind::Office(k) => {
            let label = match k {
                OfficeKind::Docx => "docx",
                OfficeKind::Xlsx => "xlsx",
                OfficeKind::Pptx => "pptx",
            };
            extract_guarded(path, label, || office_text(bytes, k, cap))?
        }
    };
    // Cap the extracted text to the bound BEFORE chunking, on a char boundary so a
    // multibyte codepoint is never split. An empty result is an honest skip.
    let capped = cap_chars(&text, cap);
    if capped.trim().is_empty() {
        return None;
    }
    Some(capped)
}

/// Truncate `s` to at most `cap` BYTES, backing off to the previous char boundary
/// so a UTF-8 codepoint is never split. Cheap when `s` already fits.
fn cap_chars(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---------------------------------------------------------------------------
// CHUNKING — overlapping windows with a citation offset
// ---------------------------------------------------------------------------

/// One chunk carved from a file: its text and the BYTE offset of its start within
/// the EXTRACTED TEXT, kept as a stable citation anchor ("path:offset"). Chunking is
/// over CHARACTERS (so a window never splits a UTF-8 codepoint); the offset is the
/// byte position of the window's first character IN THE EXTRACTED CONTENT. For a
/// plain-text source that is valid UTF-8 this equals the original file byte offset
/// (the extraction is identity); for a PDF/Office source, or a text file that needed
/// LOSSY UTF-8 decoding (a stray invalid byte becomes a 3-byte U+FFFD, shifting later
/// offsets), it is an offset into the extracted text, NOT the raw file — it is a
/// citation anchor, not a guaranteed raw-file seek position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub text: String,
    pub byte_offset: usize,
}

/// Split `content` into overlapping character windows of `chunk_chars` with
/// `overlap` carried between consecutive windows. Deterministic and TERMINATING:
/// `overlap` is clamped below `chunk_chars` by the caller (IndexBounds), so the
/// stride (`chunk_chars - overlap`) is always >= 1 and the walk always advances.
/// Empty / whitespace-only content yields no chunks. Each chunk records the BYTE
/// offset of its first character.
pub fn chunk_text(content: &str, chunk_chars: usize, overlap: usize) -> Vec<Chunk> {
    if content.trim().is_empty() || chunk_chars == 0 {
        return Vec::new();
    }
    let stride = chunk_chars.saturating_sub(overlap).max(1);
    // (char_index, byte_offset) for every character — lets us map a window's
    // start char to its byte position for the citation offset.
    let indices: Vec<(usize, char)> = content.char_indices().collect();
    let n = indices.len();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < n {
        let end = (start + chunk_chars).min(n);
        let byte_start = indices[start].0;
        let byte_end = if end < n { indices[end].0 } else { content.len() };
        let text = content[byte_start..byte_end].to_string();
        if !text.trim().is_empty() {
            chunks.push(Chunk {
                text,
                byte_offset: byte_start,
            });
        }
        if end == n {
            break;
        }
        start += stride;
    }
    chunks
}

// ---------------------------------------------------------------------------
// THE WALK — confined, bounded discovery of indexable files
// ---------------------------------------------------------------------------

/// One discovered, confined, accepted file ready to chunk: its REAL canonical
/// path (the citation path) and the allowlisted root it lives under (so a result
/// can name which root surfaced it). Discovery NEVER reads file CONTENTS — only
/// metadata — so a non-text file is rejected before any content is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    pub path: PathBuf,
    pub root: PathBuf,
}

/// Walk the allowlisted roots with `std::fs` (recursive, bounded depth/count) and
/// return the confined, accepted files. PURE w.r.t. the network. The walk:
///   * descends only into directories that CONFINE under a canonical root (so a
///     symlinked-out subdir is never entered);
///   * skips hidden entries (dotfiles/dotdirs);
///   * accepts a FILE only when: it confines under a root, its extension is on the
///     allowlist, and its size is within `max_file_bytes` (a metadata stat, no read);
///   * stops at `max_files` total and `max_depth` recursion depth.
/// Symlink loops cannot run away: a visited-set of real paths plus the depth bound
/// terminate the walk. Errors on any single entry are skipped, never fatal.
pub fn walk(roots: &[String], bounds: &IndexBounds) -> Vec<Discovered> {
    let canon = canonical_roots(roots);
    let mut out: Vec<Discovered> = Vec::new();
    let mut visited: HashSet<PathBuf> = HashSet::new();
    for root in &canon {
        walk_dir(root, root, 0, bounds, &canon, &mut visited, &mut out);
        if out.len() >= bounds.max_files {
            break;
        }
    }
    out.truncate(bounds.max_files);
    out
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    dir: &Path,
    root: &Path,
    depth: usize,
    bounds: &IndexBounds,
    canon: &[PathBuf],
    visited: &mut HashSet<PathBuf>,
    out: &mut Vec<Discovered>,
) {
    if depth > bounds.max_depth || out.len() >= bounds.max_files {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= bounds.max_files {
            return;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_hidden(&name) {
            continue;
        }
        let path = entry.path();
        // Metadata WITHOUT following the final symlink, so we classify the link
        // itself; the confine() canonicalization below resolves it for the real
        // location check (a symlink escaping the root is rejected there).
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            // Confine the directory: a symlinked-out subdir resolves outside the
            // root and is never descended into.
            let Some(real_dir) = confine(&path, canon) else {
                continue;
            };
            if !visited.insert(real_dir.clone()) {
                continue; // symlink loop / already visited
            }
            walk_dir(&real_dir, root, depth + 1, bounds, canon, visited, out);
        } else if meta.is_file() || meta.file_type().is_symlink() {
            // A symlink whose target is a file is handled here; confine resolves
            // it and rejects an escape.
            if !extension_allowed(&path) {
                continue;
            }
            let Some(real) = confine(&path, canon) else {
                continue; // symlink-escape / outside-root -> REJECTED
            };
            // Re-stat the REAL path for the size cap (the link's own metadata may
            // be the link size, not the target's).
            let Ok(real_meta) = std::fs::metadata(&real) else {
                continue;
            };
            if !real_meta.is_file() || real_meta.len() as usize > bounds.max_file_bytes {
                continue;
            }
            if visited.insert(real.clone()) {
                out.push(Discovered {
                    path: real,
                    root: root.to_path_buf(),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// THE STORE — a bounded SQLite chunk/vector table + forget
// ---------------------------------------------------------------------------

/// One stored chunk row, materialized for search/citation. `vector` is `Some`
/// only when the chunk was embedded on-device at index time; `None` means it will
/// be ranked by lexical BM25 (and the whole search reports lexical honestly).
#[derive(Debug, Clone)]
struct ChunkRow {
    /// The SQLite row id. Selected for completeness + future incremental-update /
    /// per-file delete by id; the search path ranks by index into the loaded
    /// slice, so it is not read today.
    #[allow(dead_code)]
    id: i64,
    root: String,
    file_path: String,
    byte_offset: i64,
    chunk_text: String,
    vector: Option<Vec<f64>>,
}

/// Citation + vector metadata for one cached chunk, kept PARALLEL to
/// [`CachedCorpus::facts`] (same length, same order — `meta[i]` and `facts[i]`
/// describe the same chunk).
struct ChunkMeta {
    root: String,
    file_path: String,
    byte_offset: i64,
    /// The on-device embedding, ALREADY DESERIALIZED (the JSON parse happened once
    /// at corpus-build time, not per query). `None` => the chunk is BM25-ranked.
    vector: Option<Vec<f64>>,
}

/// The materialized search corpus, built ONCE from the store and reused across
/// queries (see [`StoreState::cache`]). Every stored chunk arrives here with its
/// embedding vector already parsed out of JSON and its BM25 [`Fact`] already
/// constructed, so the interactive [`DocIndex::search`] path no longer re-reads
/// the whole `doc_chunks` table, re-parses every JSON vector, and re-clones every
/// chunk text on EVERY query.
struct CachedCorpus {
    /// Per-chunk citation anchor + optional on-device vector.
    meta: Vec<ChunkMeta>,
    /// Per-chunk BM25 document: `key` empty (docsearch has no namespaced key),
    /// `value` OWNS the chunk text (moved out of the row once). The lexical path
    /// scores over this slice directly — no per-query clone — and a hit's snippet
    /// is derived from `value`.
    facts: Vec<Fact>,
}

impl CachedCorpus {
    fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }
}

/// One CITED search result: the file it came from, the chunk's byte offset (the
/// citation anchor), a bounded snippet of the chunk, and the relevance score.
/// Only ever built from a REAL stored chunk — never fabricated.
#[derive(Debug, Clone, PartialEq)]
pub struct DocHit {
    pub file_path: String,
    pub root: String,
    pub byte_offset: i64,
    pub snippet: String,
    pub score: f64,
}

/// A complete search result: the cited hits plus the ranking backend that
/// ACTUALLY ran, so the caller reports the method honestly (neural on-device
/// embeddings, or lexical BM25 on fallback) — never claims neural when it fell
/// back.
#[derive(Debug, Clone, PartialEq)]
pub struct DocSearchResult {
    pub hits: Vec<DocHit>,
    pub method: RankMethod,
}

/// The status of the index, for the HUD telemetry surface: how many files and
/// chunks are stored, and how many of those chunks carry an on-device vector
/// (vs. will be ranked by BM25). All read from the live store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStatus {
    pub files: u64,
    pub chunks: u64,
    pub embedded_chunks: u64,
}

/// The mutable store state behind ONE async mutex: the SQLite connection PLUS the
/// materialized-corpus cache for the hot search path. They share a single lock so
/// invalidation is trivially correct — every write path clears `cache` in the SAME
/// critical section that mutates `conn`, so a search can never observe a corpus
/// that predates a committed write (nor keep serving a citation a FORGET removed).
struct StoreState {
    conn: Connection,
    /// The deserialized corpus, built ONCE and reused across queries (paying the
    /// per-vector JSON parse + chunk-text alloc once, not per query). `None` means
    /// "cold" — the next search rebuilds it from `conn`. Set to `None` by every
    /// write ([`DocIndex::insert_chunk`], [`DocIndex::forget`]).
    cache: Option<Arc<CachedCorpus>>,
}

/// The bounded, local, FORGETTABLE chunk-vector store. Mirrors the `memory.rs`
/// SQLite pattern: open/migrate, WAL, an async Mutex so `&DocIndex` is shareable.
/// The store NEVER reaches the network; it only persists chunks the confined
/// indexer produced.
pub struct DocIndex {
    state: Mutex<StoreState>,
}

impl DocIndex {
    /// Open (creating + migrating) the chunk store at `path` PLAINTEXT (today's
    /// behavior, byte-for-byte). Reached when `[security].encrypt_memory` is OFF
    /// (the default). Same pragmas as the memory store (busy_timeout + WAL).
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open docsearch index at {}", path.display()))?;
        Self::init_conn(conn)
    }

    /// Open the chunk store ENCRYPTED (transparent whole-file SQLCipher AES-256).
    /// `key` is applied via `PRAGMA key` immediately after open, before any other
    /// pragma/statement. Reached only when `[security].encrypt_memory` is ON;
    /// tests pass an explicit in-test key (no Keychain).
    pub fn open_encrypted(path: &Path, key: &crate::crypto::SecretKey) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open docsearch index at {}", path.display()))?;
        crate::crypto::apply_key(&conn, key)?;
        Self::init_conn(conn)
    }

    /// Shared setup (pragmas + schema), run AFTER any `PRAGMA key`.
    fn init_conn(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_millis(250))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS doc_chunks(
                id INTEGER PRIMARY KEY,
                root TEXT NOT NULL,
                file_path TEXT NOT NULL,
                byte_offset INTEGER NOT NULL,
                chunk_text TEXT NOT NULL,
                vector TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_doc_chunks_file ON doc_chunks(file_path);",
        )?;
        Ok(Self {
            state: Mutex::new(StoreState { conn, cache: None }),
        })
    }

    /// Insert one chunk. `vector` is the on-device embedding when available (stored
    /// as a JSON array of f64), else `None` (the chunk is BM25-ranked). Returns the
    /// new row id. Internal: the public write path is [`Self::reindex`].
    async fn insert_chunk(
        &self,
        root: &str,
        file_path: &str,
        byte_offset: usize,
        chunk_text: &str,
        vector: Option<&[f64]>,
    ) -> Result<i64> {
        let vec_json = match vector {
            Some(v) => Some(serde_json::to_string(v)?),
            None => None,
        };
        let mut st = self.state.lock().await;
        st.conn.execute(
            "INSERT INTO doc_chunks(root, file_path, byte_offset, chunk_text, vector)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![root, file_path, byte_offset as i64, chunk_text, vec_json],
        )?;
        let id = st.conn.last_insert_rowid();
        // A write invalidates the search cache IN THE SAME critical section as the
        // insert: the next search rebuilds from the store, so a freshly inserted
        // chunk is never missed and a concurrent search cannot pin a pre-insert
        // corpus (this also drops any partial corpus a search cached mid-reindex).
        st.cache = None;
        Ok(id)
    }

    /// FORGET: clear the entire index (every stored chunk + vector), returning how
    /// many chunk rows were removed and VACUUMing so the file actually shrinks. The
    /// forgettable contract — a user can make JARVIS forget every indexed file.
    pub async fn forget(&self) -> Result<u64> {
        let mut st = self.state.lock().await;
        let deleted = st.conn.execute("DELETE FROM doc_chunks", [])?;
        if deleted > 0 {
            st.conn.execute_batch("VACUUM")?;
        }
        // Invalidate the cache in the SAME critical section as the delete, so no
        // search can serve a citation that FORGET just removed (the forgettable
        // contract must hold for the in-memory corpus, not only the on-disk store).
        st.cache = None;
        Ok(deleted as u64)
    }

    /// The current index status (files / chunks / embedded-chunks) for telemetry.
    pub async fn status(&self) -> Result<IndexStatus> {
        let st = self.state.lock().await;
        let chunks: i64 = st.conn.query_row("SELECT COUNT(*) FROM doc_chunks", [], |r| r.get(0))?;
        let files: i64 = st
            .conn
            .query_row("SELECT COUNT(DISTINCT file_path) FROM doc_chunks", [], |r| r.get(0))?;
        let embedded: i64 = st.conn.query_row(
            "SELECT COUNT(*) FROM doc_chunks WHERE vector IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(IndexStatus {
            files: files.max(0) as u64,
            chunks: chunks.max(0) as u64,
            embedded_chunks: embedded.max(0) as u64,
        })
    }

    /// Load every stored chunk (bounded by the store's own size), materializing the
    /// vectors from their JSON. Used by the graph build ([`Self::chunks_for_graph`])
    /// and the tests; the hot SEARCH path instead reuses the cached corpus
    /// ([`Self::cached_corpus`]) so it does not re-run this whole-table read per query.
    async fn all_chunks(&self) -> Result<Vec<ChunkRow>> {
        let st = self.state.lock().await;
        Self::read_all_chunk_rows(&st.conn)
    }

    /// The raw SELECT-all + per-row JSON vector parse, shared by [`Self::all_chunks`]
    /// and [`Self::load_corpus`]. This is the EXPENSIVE step (a whole-table scan that
    /// re-parses every embedding out of JSON TEXT and allocates every chunk text) that
    /// the search cache exists to run ONCE per generation instead of once per query.
    fn read_all_chunk_rows(conn: &Connection) -> Result<Vec<ChunkRow>> {
        let mut stmt = conn.prepare(
            "SELECT id, root, file_path, byte_offset, chunk_text, vector FROM doc_chunks",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let vec_json: Option<String> = row.get(5)?;
                let vector = vec_json
                    .and_then(|s| serde_json::from_str::<Vec<f64>>(&s).ok())
                    .filter(|v| !v.is_empty());
                Ok(ChunkRow {
                    id: row.get(0)?,
                    root: row.get(1)?,
                    file_path: row.get(2)?,
                    byte_offset: row.get(3)?,
                    chunk_text: row.get(4)?,
                    vector,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Build the materialized [`CachedCorpus`] from the store, CONSUMING each row so
    /// the chunk text is MOVED into its BM25 [`Fact`] (no clone) and the vector is
    /// carried already-deserialized. Called only on a cache miss.
    fn load_corpus(conn: &Connection) -> Result<CachedCorpus> {
        let rows = Self::read_all_chunk_rows(conn)?;
        let mut meta = Vec::with_capacity(rows.len());
        let mut facts = Vec::with_capacity(rows.len());
        for r in rows {
            meta.push(ChunkMeta {
                root: r.root,
                file_path: r.file_path,
                byte_offset: r.byte_offset,
                vector: r.vector,
            });
            // `key` empty (docsearch has no namespaced key); `value` OWNS the chunk
            // text, moved out of the row. The snippet is later derived from `value`.
            facts.push(Fact {
                key: String::new(),
                value: r.chunk_text,
            });
        }
        Ok(CachedCorpus { meta, facts })
    }

    /// Return the materialized corpus for the search path, building + caching it on
    /// a miss. The state lock is held across the (cold-path) DB read, so the cache
    /// check, the rebuild, and every write's invalidation are ALL serialized by the
    /// one mutex: a `Some` cache is therefore always consistent with the committed
    /// store (any write since would have taken this lock and cleared it). The
    /// returned `Arc` is scored OUTSIDE the lock, so concurrent queries never
    /// serialize on the CPU-bound ranking work.
    async fn cached_corpus(&self) -> Result<Arc<CachedCorpus>> {
        let mut st = self.state.lock().await;
        if let Some(corpus) = &st.cache {
            return Ok(Arc::clone(corpus));
        }
        let corpus = Arc::new(Self::load_corpus(&st.conn)?);
        st.cache = Some(Arc::clone(&corpus));
        Ok(corpus)
    }

    /// Every stored chunk reduced to what the KNOWLEDGE-GRAPH build
    /// ([`crate::knowledge_graph`]) needs: the citation `file_path`, the chunk's
    /// `byte_offset` (the provenance anchor), and the chunk `text` the extractor
    /// mines. READ-ONLY: it returns exactly the chunks the confined, allowlisted
    /// indexer already produced (so the graph build never re-walks the disk and can
    /// only ever see allowlisted content), bounded by the store's own `max_chunks`
    /// ceiling. Vectors are not needed for extraction, so they are not loaded.
    pub async fn chunks_for_graph(&self) -> Result<Vec<(String, i64, String)>> {
        let rows = self.all_chunks().await?;
        Ok(rows
            .into_iter()
            .map(|c| (c.file_path, c.byte_offset, c.chunk_text))
            .collect())
    }

    /// REINDEX: clear the store and rebuild it from the allowlisted roots. This is
    /// the public WRITE path the daemon's "index my documents" / "reindex" intent
    /// calls. It:
    ///   1. forgets the old index (reindex is a full rebuild — bounded + idempotent);
    ///   2. walks the CONFINED, bounded roots ([`walk`]);
    ///   3. reads + EXTRACTS text from each accepted file per its [`FileKind`]
    ///      (text-like = read+decode; PDF/Office = on-device extractor behind the
    ///      panic-safe HONEST-SKIP guard), caps it to `max_file_bytes`, then chunks
    ///      it — a corrupt/encrypted/scanned/image-only/binary file is SKIPPED;
    ///   4. embeds the chunks ON-DEVICE in one batched call via `embedder`; if that
    ///      errs (server down / no embed op), stores the chunks WITHOUT vectors so
    ///      search falls back to BM25 — never failing the index;
    ///   5. enforces the `max_chunks` total bound.
    /// Returns the resulting [`IndexStatus`]. NETWORK: never — embedding is the
    /// on-device op; file contents + embeddings never leave the device.
    pub async fn reindex(
        &self,
        roots: &[String],
        bounds: &IndexBounds,
        embedder: &dyn Embedder,
    ) -> Result<IndexStatus> {
        self.forget().await?;
        let discovered = walk(roots, bounds);

        // Gather (root, path, chunk) triples up to the chunk cap, reading content
        // ONLY here (after the confined+extension+size gates already passed).
        struct Pending {
            root: String,
            file_path: String,
            byte_offset: usize,
            text: String,
        }
        let mut pending: Vec<Pending> = Vec::new();
        'files: for d in &discovered {
            if pending.len() >= bounds.max_chunks {
                break;
            }
            // Route by kind. A path the walk accepted always classifies, but a
            // disappeared/renamed file mid-walk is skipped honestly.
            let Some(kind) = classify(&d.path) else {
                continue;
            };
            let Ok(bytes) = std::fs::read(&d.path) else {
                continue;
            };
            // Per-file extraction (text-like = read+decode unchanged; PDF/Office =
            // on-device extractor behind the panic-safe HONEST-SKIP guard). The
            // extracted text is capped to `max_file_bytes` BEFORE chunking — the
            // same ceiling the raw bytes already respect — so a document cannot
            // exceed the established per-file bound. `None` => HONEST SKIP (logged):
            // a corrupt/encrypted/scanned/image-only/empty file is never indexed.
            let Some(content) = extract_text(&d.path, kind, &bytes, bounds.max_file_bytes) else {
                continue;
            };
            let chunks = chunk_text(&content, bounds.chunk_chars, bounds.chunk_overlap);
            let root = d.root.display().to_string();
            let file_path = d.path.display().to_string();
            for c in chunks {
                if pending.len() >= bounds.max_chunks {
                    break 'files;
                }
                pending.push(Pending {
                    root: root.clone(),
                    file_path: file_path.clone(),
                    byte_offset: c.byte_offset,
                    text: c.text,
                });
            }
        }

        // Embed all chunk texts ON-DEVICE in one batched call. On ANY error (server
        // down / no embed op / wrong count), store WITHOUT vectors -> BM25 search.
        let texts: Vec<String> = pending.iter().map(|p| p.text.clone()).collect();
        let vectors: Option<Vec<Vec<f64>>> = if texts.is_empty() {
            None
        } else {
            match embedder.embed(&texts).await {
                Ok(v) if v.len() == texts.len() && v.iter().all(|x| !x.is_empty()) => Some(v),
                _ => None,
            }
        };

        for (i, p) in pending.iter().enumerate() {
            let vec = vectors.as_ref().map(|vs| vs[i].as_slice());
            self.insert_chunk(&p.root, &p.file_path, p.byte_offset, &p.text, vec)
                .await?;
        }
        self.status().await
    }

    /// SEARCH: rank the stored chunks against `query` and return at most `k` CITED
    /// hits, most-relevant first, reporting WHICH backend ran. NEURAL when EVERY
    /// stored chunk carries an on-device vector AND the query embeds — cosine over
    /// the stored vectors; otherwise LEXICAL BM25 over the chunk text (the honest
    /// fallback, used whenever the embedder is/was unavailable). Zero-score
    /// (irrelevant) chunks are dropped, so an empty index or a no-match query
    /// returns NOTHING — never a fabricated citation.
    ///
    /// The query embedding is the ONE runtime/MLX-gated call here; tests inject a
    /// mock `embedder`. A failed store read degrades to an empty result.
    pub async fn search(
        &self,
        query: &str,
        k: usize,
        embedder: &dyn Embedder,
    ) -> DocSearchResult {
        let k = k.clamp(1, DOCSEARCH_MAX_K);
        // Reuse the materialized corpus (deserialized ONCE, cached until a write) —
        // no whole-table scan / JSON re-parse / chunk-text re-alloc per query. A
        // failed store read degrades to an honest empty result.
        let Ok(corpus) = self.cached_corpus().await else {
            return DocSearchResult {
                hits: Vec::new(),
                method: RankMethod::Lexical,
            };
        };
        if corpus.is_empty() || query.trim().is_empty() {
            // Nothing to rank (or a contentless query): honest empty. Report the
            // backend that WOULD run lexically (no embed call made).
            return DocSearchResult {
                hits: Vec::new(),
                method: RankMethod::Lexical,
            };
        }

        // Prefer NEURAL only when every chunk has a stored vector — a mixed store
        // (some embedded, some not) cannot be ranked coherently by cosine, so it
        // falls back to BM25 wholesale (honest: the method names what actually ran).
        let all_embedded = corpus.meta.iter().all(|m| m.vector.is_some());
        if all_embedded {
            if let Ok(qvecs) = embedder.embed(&[query.to_string()]).await {
                if qvecs.len() == 1 && !qvecs[0].is_empty() {
                    let qvec = &qvecs[0];
                    let mut scored: Vec<(usize, f64)> = corpus
                        .meta
                        .iter()
                        .enumerate()
                        .map(|(i, m)| {
                            let sim = m
                                .vector
                                .as_ref()
                                .map(|v| cosine_similarity(qvec, v))
                                .unwrap_or(0.0);
                            // Clamp negatives to 0 (anti-correlated is not a hit).
                            (i, if sim > 0.0 { sim } else { 0.0 })
                        })
                        .collect();
                    return DocSearchResult {
                        hits: rank_and_cite(&corpus, &mut scored, k),
                        method: RankMethod::Embedding,
                    };
                }
            }
            // Query embed failed / degenerate -> fall through to BM25, honestly.
        }

        // LEXICAL BM25 over the chunk text (reusing recall.rs's shipped ranker). The
        // BM25 documents are the corpus's cached `facts` — built ONCE when the corpus
        // was materialized — so the search path no longer clones every chunk text
        // into a fresh `Fact` per query; it scores over the borrowed slice directly.
        let lexical = LexicalProvider {
            params: Bm25Params::default(),
        };
        use crate::recall::EmbeddingProvider;
        let scores = lexical.score(query, &corpus.facts);
        let mut scored: Vec<(usize, f64)> = scores.into_iter().enumerate().collect();
        DocSearchResult {
            hits: rank_and_cite(&corpus, &mut scored, k),
            method: RankMethod::Lexical,
        }
    }
}

/// Sort `(chunk_index, score)` by score DESC then index ASC (deterministic tie
/// break), drop non-positive (irrelevant) scores, take `k`, and materialize a
/// CITED [`DocHit`] for each from the real chunk. No-match -> empty (no
/// fabrication). The snippet is a bounded, char-boundary-safe preview of the
/// stored chunk text.
fn rank_and_cite(corpus: &CachedCorpus, scored: &mut [(usize, f64)], k: usize) -> Vec<DocHit> {
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    scored
        .iter()
        .filter(|(_, s)| s.is_finite() && *s > 0.0)
        .take(k)
        .filter_map(|(i, s)| {
            let m = corpus.meta.get(*i)?;
            // `facts[i].value` is the chunk text (moved in at build time), parallel
            // to `meta[i]`; the snippet is a bounded preview of it.
            let chunk_text = &corpus.facts.get(*i)?.value;
            Some(DocHit {
                file_path: m.file_path.clone(),
                root: m.root.clone(),
                byte_offset: m.byte_offset,
                snippet: snippet_of(chunk_text),
                score: *s,
            })
        })
        .collect()
}

/// A bounded, char-boundary-safe snippet of a chunk for display/citation.
fn snippet_of(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() <= SNIPPET_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(SNIPPET_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// Whether a configured roots list + enabled flag actually permit any indexing:
/// the master switch must be on AND at least one root must be configured. The
/// daemon checks this before ever walking — an OFF subsystem or an empty allowlist
/// indexes NOTHING (no whole-disk scan).
pub fn indexing_permitted(enabled: bool, roots: &[String]) -> bool {
    enabled && !roots.is_empty()
}

/// The DAEMON ENTRY POINT for the "index my documents" / "reindex" intent:
/// CONFIG-GATED reindex over the allowlisted roots. This is the single function
/// the daemon's index/reindex trigger calls — it enforces the gate (ON by default
/// but inert without roots; [`indexing_permitted`]: `[docsearch].enabled` AND a
/// non-empty `roots`) BEFORE
/// touching the disk, so an OFF subsystem or an empty allowlist indexes NOTHING
/// (never a whole-disk scan). When permitted, it lifts the bounds from config and
/// runs [`DocIndex::reindex`] (the confined, bounded, on-device walk+chunk+embed).
///
/// Returns `Ok(None)` when indexing is NOT permitted (the daemon then tells the
/// user file search is off / no folder is allowlisted — it never silently scans),
/// or `Ok(Some(status))` with the resulting index status. The `embedder` is the
/// on-device socket in the live path (runtime/MLX-gated) and a mock in tests; on
/// any embed error the chunks are stored vector-less and search falls back to BM25.
pub async fn index_documents(
    cfg: &crate::config::DocSearchConfig,
    index: &DocIndex,
    embedder: &dyn Embedder,
) -> Result<Option<IndexStatus>> {
    if !indexing_permitted(cfg.enabled, &cfg.roots) {
        return Ok(None); // OFF / no allowlisted root -> index NOTHING.
    }
    let bounds = IndexBounds::from_config(cfg);
    let status = index.reindex(&cfg.roots, &bounds, embedder).await?;
    Ok(Some(status))
}

/// Defensive: reject a configured root that is not an absolute path or that
/// contains a `..` component (a relative or traversing root is a misconfiguration
/// that could widen the surface). The walk additionally canonicalizes every root,
/// but this catches an obviously-unsafe entry early for an honest config warning.
#[allow(dead_code)] // surfaced by the HUD/config validation path; unit-tested here
pub fn root_is_safe(root: &str) -> bool {
    let p = Path::new(root);
    p.is_absolute() && !p.components().any(|c| matches!(c, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// A unique temp dir tree per test, cleaned on drop. All file I/O in these
    /// tests stays inside this dir — never the user's real home.
    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "jarvis-docsearch-test-{}-{}",
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
            self.write_bytes(rel, contents.as_bytes())
        }
        /// Write raw bytes (for the binary PDF/Office fixtures).
        fn write_bytes(&self, rel: &str, contents: &[u8]) -> PathBuf {
            let p = self.join(rel);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, contents).unwrap();
            p
        }
        fn db_path(&self) -> PathBuf {
            self.join("index.db")
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn roots_of(t: &TempTree, sub: &str) -> Vec<String> {
        vec![t.join(sub).display().to_string()]
    }

    // ---- a mock embedder: NEVER touches a socket/MLX/network -----------------

    /// A deterministic mock [`Embedder`]. Each input text maps to a fixed small
    /// vector by a keyword rule so a test can pin which chunk is "near" a query.
    struct KeywordEmbedder;
    impl Embedder for KeywordEmbedder {
        fn embed<'a>(&'a self, texts: &'a [String]) -> crate::recall::EmbedFuture<'a> {
            // axis 0 = "subaru"/"car", axis 1 = "corgi"/"pet", axis 2 = other.
            let vecs: Vec<Vec<f64>> = texts
                .iter()
                .map(|t| {
                    let l = t.to_lowercase();
                    let car = l.contains("subaru") || l.contains("car") || l.contains("outback");
                    let pet = l.contains("corgi") || l.contains("pet") || l.contains("watson");
                    if car {
                        vec![1.0, 0.0, 0.0]
                    } else if pet {
                        vec![0.0, 1.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0]
                    }
                })
                .collect();
            Box::pin(async move { Ok(vecs) })
        }
    }

    /// A mock embedder that is always DOWN (Err) — drives the BM25 fallback.
    struct DownEmbedder;
    impl Embedder for DownEmbedder {
        fn embed<'a>(&'a self, _texts: &'a [String]) -> crate::recall::EmbedFuture<'a> {
            Box::pin(async move { Err(anyhow::anyhow!("inference socket unavailable (simulated)")) })
        }
    }

    // =====================================================================
    // SECURITY: path confinement REJECTS every escape
    // =====================================================================

    #[test]
    fn confinement_rejects_symlink_escape_dotdot_and_absolute_elsewhere() {
        let t = TempTree::new("confine");
        // An allowlisted root with one real file inside.
        let root = t.join("vault");
        fs::create_dir_all(&root).unwrap();
        let inside = t.write("vault/note.md", "a secret note inside the vault");
        // A file OUTSIDE the root (a sibling) the index must never reach.
        let outside = t.write("outside/secret.md", "OUTSIDE the vault — must never index");

        let canon = canonical_roots(&[root.display().to_string()]);
        assert!(!canon.is_empty(), "the real root must canonicalize");

        // 1. A genuine in-root file is ACCEPTED (its real path is under the root).
        let accepted = confine(&inside, &canon).expect("an in-root file must confine");
        assert!(accepted.starts_with(&canon[0]), "accepted path must be under the root");

        // 2. A `..` traversal that climbs OUT of the root is REJECTED.
        let traversal = root.join("..").join("outside").join("secret.md");
        assert!(
            confine(&traversal, &canon).is_none(),
            "a `..` escape to a sibling must be rejected"
        );

        // 3. An absolute-elsewhere path (the outside file directly) is REJECTED.
        assert!(
            confine(&outside, &canon).is_none(),
            "an absolute path outside every root must be rejected"
        );

        // 4. A SYMLINK inside the root pointing OUTSIDE it is REJECTED — the
        //    canonicalized real target is outside the root.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let escape_link = root.join("escape.md");
            symlink(&outside, &escape_link).unwrap();
            // The link's lexical path is under the root, but its REAL target is not.
            assert!(
                confine(&escape_link, &canon).is_none(),
                "a symlink whose target escapes the root must be rejected"
            );
        }
    }

    #[test]
    fn walk_never_indexes_a_symlink_escape_or_outside_file() {
        let t = TempTree::new("walk-confine");
        let root = t.join("vault");
        fs::create_dir_all(&root).unwrap();
        t.write("vault/keep.md", "in-vault note, indexable");
        let outside = t.write("outside/secret.md", "OUTSIDE — must never appear");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // A symlink inside the vault that escapes to the outside file, and a
            // symlinked subdir that escapes the vault.
            symlink(&outside, root.join("escape.md")).unwrap();
            symlink(t.join("outside"), root.join("escape_dir")).unwrap();
        }

        let bounds = IndexBounds::default();
        let found = walk(&roots_of(&t, "vault"), &bounds);
        let paths: Vec<String> = found.iter().map(|d| d.path.display().to_string()).collect();
        // The in-vault file is found...
        assert!(
            paths.iter().any(|p| p.ends_with("keep.md")),
            "the in-vault file must be indexed: {paths:?}"
        );
        // ...and NOTHING resolving to the outside file ever is.
        assert!(
            !paths.iter().any(|p| p.contains("secret.md") || p.contains("/outside/")),
            "no escape may be indexed: {paths:?}"
        );
    }

    // =====================================================================
    // INDEXING + CHUNKING over a temp dir
    // =====================================================================

    #[test]
    fn chunking_produces_overlapping_windows_with_offsets() {
        // 10 chars, window 4, overlap 1 -> stride 3 -> starts at 0,3,6; the window
        // at start=6 reaches the end (chars 6..10 = "ghij") and terminates, so the
        // overlapping windows are "abcd","defg","ghij" — consecutive windows share
        // their `overlap` boundary char (d, g) for citation continuity.
        let content = "abcdefghij";
        let chunks = chunk_text(content, 4, 1);
        assert_eq!(chunks.len(), 3, "{chunks:?}");
        assert_eq!(chunks[0].text, "abcd");
        assert_eq!(chunks[0].byte_offset, 0);
        assert_eq!(chunks[1].text, "defg");
        assert_eq!(chunks[1].byte_offset, 3);
        assert_eq!(chunks[2].text, "ghij");
        assert_eq!(chunks[2].byte_offset, 6);
        // Overlap: window 1 starts at 'd' (the last char of window 0) -> the
        // overlap of 1 char is preserved between consecutive windows.
        assert!(chunks[1].text.starts_with('d'), "the overlap char carries over");
        // A longer run yields a final short tail window.
        let tail = chunk_text("abcdefghijkl", 4, 1); // starts 0,3,6,9 -> last is "jkl"
        assert_eq!(tail.last().unwrap().text, "jkl");
        // Empty / whitespace content yields no chunks (never a fabricated chunk).
        assert!(chunk_text("", 100, 10).is_empty());
        assert!(chunk_text("   \n  ", 100, 10).is_empty());
    }

    #[test]
    fn extension_allowlist_skips_binaries_and_unknown_types() {
        assert!(extension_allowed(Path::new("notes.md")));
        assert!(extension_allowed(Path::new("main.rs")));
        assert!(extension_allowed(Path::new("config.TOML"))); // case-insensitive
        // Now in scope via on-device extractors: born-digital PDF + Office OOXML.
        assert!(extension_allowed(Path::new("paper.pdf")));
        assert!(extension_allowed(Path::new("memo.docx")));
        assert!(extension_allowed(Path::new("book.XLSX"))); // case-insensitive
        // Still out of scope: images/archives/legacy-binary-office/no-extension.
        assert!(!extension_allowed(Path::new("photo.png")));
        assert!(!extension_allowed(Path::new("archive.zip")));
        assert!(!extension_allowed(Path::new("legacy.doc")));
        assert!(!extension_allowed(Path::new("Makefile")));
    }

    #[tokio::test]
    async fn reindex_walks_chunks_and_stores_only_allowlisted_files() {
        let t = TempTree::new("reindex");
        t.write("docs/a.md", "the quarterly budget meeting covered the launch plan");
        t.write("docs/b.txt", "a corgi named Watson sleeps on the rug");
        // A .pdf extension but NOT a valid PDF — the extractor errors -> HONEST SKIP
        // (a malformed file is never indexed as garbage even though .pdf is in scope).
        t.write("docs/skip.pdf", "this PDF is not a valid pdf and must be skipped");
        // An image binary is out of scope entirely (extension not indexable).
        t.write_bytes("docs/photo.png", &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]);
        t.write("docs/sub/c.rs", "fn main() { println!(\"hello rust\"); }");

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let bounds = IndexBounds::default();
        let status = idx
            .reindex(&roots_of(&t, "docs"), &bounds, &KeywordEmbedder)
            .await
            .unwrap();
        // 3 indexable files (md, txt, rs); the malformed pdf + the png are skipped.
        assert_eq!(status.files, 3, "only the readable files are indexed: {status:?}");
        assert!(status.chunks >= 3, "each small file is at least one chunk: {status:?}");
        // The malformed pdf's raw content must never appear in any stored chunk.
        let all = idx.all_chunks().await.unwrap();
        assert!(
            all.iter().all(|c| !c.chunk_text.contains("not a valid pdf")),
            "the skipped PDF's content must not be stored"
        );
    }

    // =====================================================================
    // AT-REST ENCRYPTION (#11)
    // =====================================================================

    #[tokio::test]
    async fn open_encrypted_round_trips_and_chunk_text_is_ciphertext_at_rest() {
        let t = TempTree::new("enc");
        t.write("docs/a.md", "the doc-canary phrase lives in this chunk");
        // Encrypted open with an EXPLICIT in-test key (no Keychain, no network).
        let key = crate::crypto::SecretKey::from_bytes([2u8; crate::crypto::KEY_BYTES]);
        {
            let idx = DocIndex::open_encrypted(&t.db_path(), &key).unwrap();
            idx.reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &KeywordEmbedder)
                .await
                .unwrap();
        }
        // On-disk bytes are ciphertext: the chunk text is not in the clear, and the
        // SQLite magic header is absent (it's a SQLCipher file).
        let raw = fs::read(t.db_path()).unwrap();
        assert!(
            !raw.windows(b"doc-canary".len()).any(|w| w == b"doc-canary"),
            "chunk text must not appear in plaintext on disk"
        );
        assert!(!raw.starts_with(b"SQLite format 3\0"), "index must be encrypted");
        // Reopen WITH the key: the chunk reads back.
        {
            let idx = DocIndex::open_encrypted(&t.db_path(), &key).unwrap();
            let all = idx.all_chunks().await.unwrap();
            assert!(
                all.iter().any(|c| c.chunk_text.contains("doc-canary")),
                "the chunk must read back with the key"
            );
        }
        // The WRONG key cannot open it.
        let wrong = crate::crypto::SecretKey::from_bytes([1u8; crate::crypto::KEY_BYTES]);
        assert!(
            DocIndex::open_encrypted(&t.db_path(), &wrong).is_err(),
            "wrong key must fail"
        );
    }

    // =====================================================================
    // SEARCH: ranks the right chunk + CITES the right file
    // =====================================================================

    #[tokio::test]
    async fn search_neural_ranks_and_cites_the_right_file() {
        let t = TempTree::new("search-neural");
        let car = t.write("docs/car.md", "I drive a blue Subaru Outback wagon");
        t.write("docs/pet.md", "a corgi named Watson is my dog");

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let bounds = IndexBounds::default();
        idx.reindex(&roots_of(&t, "docs"), &bounds, &KeywordEmbedder)
            .await
            .unwrap();

        // A car query: the KeywordEmbedder puts the car chunk on axis 0, the query
        // on axis 0 too -> cosine 1; the pet chunk is orthogonal -> dropped.
        // The store cites the REAL (canonicalized, symlink-resolved) path — on
        // macOS /var canonicalizes to /private/var — so compare against that.
        let car_real = fs::canonicalize(&car).unwrap().display().to_string();
        let result = idx.search("what kind of car do I drive", 5, &KeywordEmbedder).await;
        assert_eq!(result.method, RankMethod::Embedding, "all chunks embedded -> neural ran");
        assert!(!result.hits.is_empty(), "the car chunk must be retrieved");
        assert_eq!(
            result.hits[0].file_path, car_real,
            "the top hit must CITE the real car file: {:?}",
            result.hits
        );
        assert!(result.hits[0].snippet.contains("Subaru"), "snippet is the real chunk text");
        assert!(result.hits[0].score > 0.0, "only positive hits are returned");
        // The orthogonal pet file is NOT surfaced for a car query.
        assert!(
            result.hits.iter().all(|h| !h.file_path.contains("pet.md")),
            "an irrelevant file must not be cited: {:?}",
            result.hits
        );
    }

    #[tokio::test]
    async fn search_falls_back_to_bm25_when_embedder_is_down_and_reports_it() {
        let t = TempTree::new("search-bm25");
        let budget = t.write("docs/budget.md", "the quarterly budget review and forecast");
        t.write("docs/pet.md", "a corgi named Watson naps a lot");

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let bounds = IndexBounds::default();
        // Index with the embedder DOWN: chunks are stored WITHOUT vectors.
        idx.reindex(&roots_of(&t, "docs"), &bounds, &DownEmbedder)
            .await
            .unwrap();
        let status = idx.status().await.unwrap();
        assert_eq!(status.embedded_chunks, 0, "no vectors stored when the embedder is down");

        // Search (embedder still down) -> BM25, reported honestly.
        let budget_real = fs::canonicalize(&budget).unwrap().display().to_string();
        let result = idx.search("quarterly budget forecast", 5, &DownEmbedder).await;
        assert_eq!(result.method, RankMethod::Lexical, "no vectors -> BM25 fallback");
        assert_eq!(result.method.as_str(), "lexical-bm25");
        assert!(!result.hits.is_empty(), "BM25 still ranks the budget file");
        assert_eq!(
            result.hits[0].file_path, budget_real,
            "BM25 must cite the budget file: {:?}",
            result.hits
        );
    }

    #[tokio::test]
    async fn search_no_match_returns_nothing_never_fabricates_a_citation() {
        let t = TempTree::new("no-match");
        t.write("docs/a.md", "notes about gardening and tomatoes");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        idx.reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &DownEmbedder)
            .await
            .unwrap();
        // A query with zero term overlap -> BM25 scores 0 -> no hits.
        let result = idx.search("quantum chromodynamics lecture", 5, &DownEmbedder).await;
        assert!(
            result.hits.is_empty(),
            "a no-match query must cite nothing: {:?}",
            result.hits
        );
    }

    #[tokio::test]
    async fn search_empty_index_is_honest_empty() {
        let t = TempTree::new("empty");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        let result = idx.search("anything at all", 5, &KeywordEmbedder).await;
        assert!(result.hits.is_empty(), "an empty index returns nothing");
        assert_eq!(result.method, RankMethod::Lexical);
    }

    // =====================================================================
    // BOUNDED: caps are enforced
    // =====================================================================

    #[tokio::test]
    async fn bounds_cap_files_chunks_and_skip_oversize() {
        let t = TempTree::new("bounds");
        // 5 files, but max_files = 2.
        for i in 0..5 {
            t.write(&format!("docs/f{i}.md"), &format!("file number {i} content here"));
        }
        // One oversize file that the per-file byte cap must skip.
        t.write("docs/big.md", &"x ".repeat(10_000));

        let bounds = IndexBounds {
            max_files: 2,
            max_chunks: 3,
            max_file_bytes: 100, // skips big.md (and any file > 100 bytes)
            max_depth: 8,
            chunk_chars: 64,
            chunk_overlap: 8,
        };
        let found = walk(&roots_of(&t, "docs"), &bounds);
        assert!(found.len() <= 2, "max_files caps the walk: {}", found.len());
        assert!(
            found.iter().all(|d| !d.path.ends_with("big.md")),
            "the oversize file must be skipped: {found:?}"
        );

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let status = idx
            .reindex(&roots_of(&t, "docs"), &bounds, &DownEmbedder)
            .await
            .unwrap();
        assert!(status.files <= 2, "file cap honored: {status:?}");
        assert!(status.chunks <= 3, "chunk cap honored: {status:?}");
    }

    // =====================================================================
    // FORGET clears the index
    // =====================================================================

    #[tokio::test]
    async fn forget_clears_the_entire_index() {
        let t = TempTree::new("forget");
        t.write("docs/a.md", "something to index then forget");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        idx.reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &DownEmbedder)
            .await
            .unwrap();
        assert!(idx.status().await.unwrap().chunks > 0, "index has chunks before forget");

        let cleared = idx.forget().await.unwrap();
        assert!(cleared > 0, "forget removes the stored chunks");
        let status = idx.status().await.unwrap();
        assert_eq!(status.chunks, 0, "the index is empty after forget");
        assert_eq!(status.files, 0);
        // A search after forget is honestly empty (never a stale citation).
        let result = idx.search("something", 5, &DownEmbedder).await;
        assert!(result.hits.is_empty(), "no citation survives a forget");
    }

    // =====================================================================
    // SEARCH CACHE FRESHNESS: a materialized corpus must NEVER outlive a write.
    // Each test WARMS the cache (a search first), THEN mutates the store, THEN
    // searches again — so a missing invalidation would surface as a STALE hit
    // (the whole correctness risk of caching the corpus).
    // =====================================================================

    #[tokio::test]
    async fn search_cache_refreshes_after_reindex_bm25() {
        let t = TempTree::new("cache-reindex-bm25");
        t.write("docs/a.md", "the quarterly budget review and forecast");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        let roots = roots_of(&t, "docs");
        let bounds = IndexBounds::default();
        idx.reindex(&roots, &bounds, &DownEmbedder).await.unwrap();

        // WARM the cache: the first search materializes + caches the corpus.
        let first = idx.search("quarterly budget forecast", 5, &DownEmbedder).await;
        assert!(!first.hits.is_empty(), "the budget chunk is found before reindex");

        // Replace the file's content entirely, then reindex (a full rebuild).
        t.write("docs/a.md", "notes about gardening tomatoes and basil");
        idx.reindex(&roots, &bounds, &DownEmbedder).await.unwrap();

        // The stale "budget" query must now cite NOTHING — reindex invalidated the
        // warm cache, so this search rebuilt from the fresh store (no budget chunk).
        let stale = idx.search("quarterly budget forecast", 5, &DownEmbedder).await;
        assert!(
            stale.hits.is_empty(),
            "reindex must invalidate the cache: a stale budget hit survived: {:?}",
            stale.hits
        );

        // ...and the NEW content is retrievable from the refreshed corpus.
        let fresh = idx.search("gardening tomatoes basil", 5, &DownEmbedder).await;
        assert!(!fresh.hits.is_empty(), "the reindexed content must be searchable");
        assert!(
            fresh.hits[0].snippet.contains("gardening"),
            "the fresh snippet is the NEW chunk text: {:?}",
            fresh.hits
        );
    }

    #[tokio::test]
    async fn search_cache_refreshes_after_forget() {
        let t = TempTree::new("cache-forget");
        t.write("docs/a.md", "a corgi named Watson naps in the sun");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        idx.reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &DownEmbedder)
            .await
            .unwrap();

        // WARM the cache with a search that finds the corgi chunk.
        let warm = idx.search("corgi watson", 5, &DownEmbedder).await;
        assert!(!warm.hits.is_empty(), "the corgi chunk is found before forget");

        // FORGET must invalidate the warm cache in the same breath as the delete.
        let cleared = idx.forget().await.unwrap();
        assert!(cleared > 0, "forget removed the stored chunks");

        // A search after forget is honestly empty — never served from the warm cache.
        let after = idx.search("corgi watson", 5, &DownEmbedder).await;
        assert!(
            after.hits.is_empty(),
            "forget must invalidate the warm cache: a stale hit survived: {:?}",
            after.hits
        );
    }

    #[tokio::test]
    async fn search_neural_cache_refreshes_after_reindex() {
        let t = TempTree::new("cache-reindex-neural");
        t.write("docs/car.md", "I drive a blue Subaru Outback wagon");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        let roots = roots_of(&t, "docs");
        let bounds = IndexBounds::default();
        idx.reindex(&roots, &bounds, &KeywordEmbedder).await.unwrap();

        // WARM the cache on the NEURAL path (every chunk is embedded).
        let warm = idx.search("what car do I drive", 5, &KeywordEmbedder).await;
        assert_eq!(warm.method, RankMethod::Embedding, "all chunks embedded -> neural");
        assert!(!warm.hits.is_empty(), "the car chunk is found before reindex");

        // Remove the car file, add a pet file, and reindex — the cached car vector
        // must not survive.
        fs::remove_file(t.join("docs/car.md")).unwrap();
        t.write("docs/pet.md", "a corgi named Watson is my dog");
        idx.reindex(&roots, &bounds, &KeywordEmbedder).await.unwrap();

        // The car chunk is gone: a car query (orthogonal to the pet chunk) cites
        // nothing — the stale cached car vector did NOT survive the reindex.
        let after_car = idx.search("what car do I drive", 5, &KeywordEmbedder).await;
        assert!(
            after_car.hits.is_empty(),
            "reindex must invalidate the neural cache: a stale car hit survived: {:?}",
            after_car.hits
        );

        // ...and the NEW pet chunk is retrievable + cites the new file.
        let pet = idx.search("my pet corgi", 5, &KeywordEmbedder).await;
        assert!(!pet.hits.is_empty(), "the reindexed pet chunk is searchable");
        assert!(
            pet.hits[0].file_path.contains("pet.md"),
            "the fresh hit cites the NEW pet file: {:?}",
            pet.hits
        );
    }

    // =====================================================================
    // OFF / no-whole-disk-scan gate + safe roots
    // =====================================================================

    #[test]
    fn indexing_is_not_permitted_when_off_or_no_roots() {
        // OFF -> never index, even with a root configured.
        assert!(!indexing_permitted(false, &["/some/root".to_string()]));
        // ON but EMPTY allowlist -> still nothing (no whole-disk scan).
        assert!(!indexing_permitted(true, &[]));
        // ON + a root -> permitted.
        assert!(indexing_permitted(true, &["/some/root".to_string()]));
    }

    #[test]
    fn root_safety_rejects_relative_and_traversing_roots() {
        assert!(root_is_safe("/Users/me/Documents"));
        assert!(!root_is_safe("relative/path"), "a relative root is unsafe");
        assert!(!root_is_safe("/Users/me/../etc"), "a traversing root is unsafe");
        assert!(!root_is_safe(""), "an empty root is unsafe");
    }

    #[tokio::test]
    async fn reindex_with_no_roots_indexes_nothing() {
        let t = TempTree::new("no-roots");
        let idx = DocIndex::open(&t.db_path()).unwrap();
        // An empty allowlist walks/indexes nothing — the no-whole-disk-scan guard
        // at the store layer (the daemon also checks indexing_permitted upstream).
        let status = idx.reindex(&[], &IndexBounds::default(), &DownEmbedder).await.unwrap();
        assert_eq!(status.files, 0, "empty roots -> no files indexed");
        assert_eq!(status.chunks, 0);
    }

    #[tokio::test]
    async fn index_documents_is_config_gated_off_by_default() {
        let t = TempTree::new("gated");
        t.write("docs/a.md", "real content that exists on disk");
        let idx = DocIndex::open(&t.db_path()).unwrap();

        // OFF (the shipped default) with a REAL root present -> still indexes
        // NOTHING (the gate runs before any disk walk; no whole-disk scan).
        let off = crate::config::DocSearchConfig {
            enabled: false,
            roots: roots_of(&t, "docs"),
            ..crate::config::DocSearchConfig::default()
        };
        let status = index_documents(&off, &idx, &DownEmbedder).await.unwrap();
        assert!(status.is_none(), "OFF must index nothing even with a real root");
        assert_eq!(idx.status().await.unwrap().chunks, 0, "nothing stored while OFF");

        // ON but EMPTY allowlist -> still nothing (no whole-disk scan).
        let on_no_roots = crate::config::DocSearchConfig {
            enabled: true,
            roots: Vec::new(),
            ..crate::config::DocSearchConfig::default()
        };
        assert!(
            index_documents(&on_no_roots, &idx, &DownEmbedder).await.unwrap().is_none(),
            "ON + empty allowlist must index nothing"
        );

        // ON + a real allowlisted root -> indexes the confined files.
        let on = crate::config::DocSearchConfig {
            enabled: true,
            roots: roots_of(&t, "docs"),
            ..crate::config::DocSearchConfig::default()
        };
        let status = index_documents(&on, &idx, &DownEmbedder).await.unwrap();
        let status = status.expect("ON + a root indexes");
        assert_eq!(status.files, 1, "the one allowlisted file is indexed: {status:?}");
        assert!(status.chunks >= 1);
    }

    // =====================================================================
    // HONESTY: binary sniff + hidden files
    // =====================================================================

    #[test]
    fn binary_sniff_catches_a_nul_blob() {
        assert!(looks_binary(b"text\0with a nul"));
        assert!(!looks_binary(b"plain readable text"));
    }

    #[tokio::test]
    async fn hidden_files_and_dirs_are_skipped() {
        let t = TempTree::new("hidden");
        t.write("docs/visible.md", "visible note");
        t.write("docs/.secret.md", "a dotfile that must be skipped");
        t.write("docs/.hidden/inside.md", "inside a hidden dir, skipped");
        let found = walk(&roots_of(&t, "docs"), &IndexBounds::default());
        let paths: Vec<String> = found.iter().map(|d| d.path.display().to_string()).collect();
        assert!(paths.iter().any(|p| p.ends_with("visible.md")), "{paths:?}");
        assert!(
            paths.iter().all(|p| !p.contains(".secret") && !p.contains(".hidden")),
            "hidden entries must be skipped: {paths:?}"
        );
    }

    // =====================================================================
    // PDF + OFFICE EXTRACTION — born-digital extract + index + search, and
    // PANIC-SAFE HONEST-SKIP of corrupt/garbage files.
    // =====================================================================

    /// Build a minimal BORN-DIGITAL PDF (one page, Helvetica, a single text show
    /// operator) carrying `body` as visible text. Hand-rolled so the test owns the
    /// bytes — no external fixture file. This is exactly the shape `pdf-extract`
    /// decodes from a real born-digital PDF's content stream.
    fn make_pdf(body: &str) -> Vec<u8> {
        use std::fmt::Write as _;
        let objs: Vec<String> = vec![
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_string(),
            {
                let stream = format!("BT /F1 24 Tf 72 700 Td ({body}) Tj ET");
                format!("<< /Length {} >>\nstream\n{}\nendstream", stream.len(), stream)
            },
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            let mut s = String::new();
            write!(s, "{} 0 obj\n{}\nendobj\n", i + 1, o).unwrap();
            out.extend_from_slice(s.as_bytes());
        }
        let xref_pos = out.len();
        let mut s = String::new();
        write!(s, "xref\n0 {}\n", objs.len() + 1).unwrap();
        s.push_str("0000000000 65535 f \n");
        for off in &offsets {
            writeln!(s, "{off:010} 00000 n ").unwrap();
        }
        write!(
            s,
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF",
            objs.len() + 1,
            xref_pos
        )
        .unwrap();
        out.extend_from_slice(s.as_bytes());
        out
    }

    /// Build a minimal valid OOXML package (a ZIP of `parts`, STORED so no
    /// compression backend feature is needed to write the fixture). The reader path
    /// (`office_text`) handles both stored and DEFLATE'd real-world documents.
    fn make_ooxml(parts: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Cursor;
        use zip::write::{SimpleFileOptions, ZipWriter};
        use zip::CompressionMethod;
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = ZipWriter::new(&mut buf);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            for (name, body) in parts {
                zw.start_file(*name, opts).unwrap();
                std::io::Write::write_all(&mut zw, body.as_bytes()).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    /// A minimal valid .docx: [Content_Types].xml + word/document.xml with two
    /// paragraphs of `<w:t>` runs.
    fn make_docx() -> Vec<u8> {
        make_ooxml(&[
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"></Types>"#,
            ),
            (
                "word/document.xml",
                r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>the quarterly budget</w:t></w:r><w:r><w:t xml:space="preserve"> review and forecast</w:t></w:r></w:p><w:p><w:r><w:t>covers the Subaru Outback fleet</w:t></w:r></w:p></w:body></w:document>"#,
            ),
        ])
    }

    /// A minimal valid .xlsx: shared strings + one worksheet.
    fn make_xlsx() -> Vec<u8> {
        make_ooxml(&[
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0"?><Types></Types>"#,
            ),
            (
                "xl/sharedStrings.xml",
                r#"<?xml version="1.0"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>Revenue</t></si><si><t>quarterly forecast 2026</t></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0"?><worksheet><sheetData><row><c t="inlineStr"><is><t>inline cell text</t></is></c></row></sheetData></worksheet>"#,
            ),
        ])
    }

    /// A minimal valid .pptx: one slide with `<a:t>` text runs.
    fn make_pptx() -> Vec<u8> {
        make_ooxml(&[
            (
                "[Content_Types].xml",
                r#"<?xml version="1.0"?><Types></Types>"#,
            ),
            (
                "ppt/slides/slide1.xml",
                r#"<?xml version="1.0"?><p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><a:t>Project JARVIS</a:t><a:t>quarterly roadmap</a:t></p:spTree></p:cSld></p:sld>"#,
            ),
        ])
    }

    // ---- the unit-level extractor: born-digital files yield real text ----------

    #[test]
    fn pdf_extractor_pulls_born_digital_text() {
        let pdf = make_pdf("Quarterly budget Subaru Outback");
        let got = pdf_text(&pdf).expect("a born-digital PDF must extract");
        assert!(got.contains("Quarterly"), "extracted PDF text: {got:?}");
        assert!(got.contains("Subaru"), "extracted PDF text: {got:?}");
    }

    /// Under `cargo test`, `current_exe` is the test harness in `target/.../deps/`,
    /// which has NO `pdfjail` sibling — so the memory-jail is reported absent and
    /// `pdf_text` transparently uses the in-process fallback. This locks the
    /// dispatch: unit tests exercise the fallback; the jail itself is covered by the
    /// `tests/pdf_memory_jail.rs` integration tests (one `#[ignore]` on-device bomb).
    #[test]
    fn pdf_dispatch_uses_in_process_fallback_when_the_jail_helper_is_absent() {
        assert!(
            !pdfjail_available(),
            "the test harness dir must have no pdfjail sibling"
        );
        // A valid born-digital PDF still extracts via the fallback...
        let pdf = make_pdf("Quarterly budget Subaru Outback");
        let got = pdf_text(&pdf).expect("fallback extraction must still work");
        assert!(got.contains("Quarterly") && got.contains("Subaru"), "got: {got:?}");
        // ...and the fallback's flate2 decompression-budget probe is still the
        // first-line defense (the primitive is covered by
        // `flate_stream_budget_flags_a_decompression_bomb`).
    }

    #[test]
    fn office_extractors_pull_text_from_each_family() {
        let docx = office_text(&make_docx(), OfficeKind::Docx, 1 << 20).unwrap();
        assert!(docx.contains("quarterly budget"), "docx: {docx:?}");
        // Adjacent runs in one paragraph are concatenated, not glued.
        assert!(docx.contains("review and forecast"), "docx run join: {docx:?}");
        assert!(docx.contains("Subaru Outback"), "docx p2: {docx:?}");

        let xlsx = office_text(&make_xlsx(), OfficeKind::Xlsx, 1 << 20).unwrap();
        assert!(xlsx.contains("Revenue"), "xlsx shared string: {xlsx:?}");
        assert!(xlsx.contains("quarterly forecast 2026"), "xlsx: {xlsx:?}");
        assert!(xlsx.contains("inline cell text"), "xlsx inline string: {xlsx:?}");

        let pptx = office_text(&make_pptx(), OfficeKind::Pptx, 1 << 20).unwrap();
        assert!(pptx.contains("Project JARVIS"), "pptx: {pptx:?}");
        assert!(pptx.contains("quarterly roadmap"), "pptx: {pptx:?}");
    }

    // ---- (1) a real docx/pdf INDEXES + its text is SEARCHABLE -------------------

    #[tokio::test]
    async fn pdf_and_docx_are_indexed_and_searchable() {
        let t = TempTree::new("doc-index");
        let pdf_path = t.write_bytes("docs/report.pdf", &make_pdf("the quarterly budget forecast review"));
        let docx_path = t.write_bytes("docs/memo.docx", &make_docx());
        let xlsx_path = t.write_bytes("docs/sheet.xlsx", &make_xlsx());
        let pptx_path = t.write_bytes("docs/deck.pptx", &make_pptx());

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let bounds = IndexBounds::default();
        // DownEmbedder -> chunks stored vector-less -> BM25 search (no MLX needed).
        let status = idx
            .reindex(&roots_of(&t, "docs"), &bounds, &DownEmbedder)
            .await
            .unwrap();
        assert_eq!(status.files, 4, "pdf+docx+xlsx+pptx all indexed: {status:?}");
        assert!(status.chunks >= 4, "each doc is at least one chunk: {status:?}");

        // BM25 search finds the PDF for a budget query and CITES the real pdf path.
        let pdf_real = fs::canonicalize(&pdf_path).unwrap().display().to_string();
        let r = idx.search("quarterly budget forecast", 5, &DownEmbedder).await;
        assert_eq!(r.method, RankMethod::Lexical);
        assert!(!r.hits.is_empty(), "the pdf must be retrievable");
        assert_eq!(r.hits[0].file_path, pdf_real, "top hit cites the real pdf: {:?}", r.hits);

        // The docx text is searchable too (its own distinctive phrase).
        let docx_real = fs::canonicalize(&docx_path).unwrap().display().to_string();
        let r = idx.search("Subaru Outback fleet", 5, &DownEmbedder).await;
        assert!(
            r.hits.iter().any(|h| h.file_path == docx_real),
            "the docx text must be searchable + cited: {:?}",
            r.hits
        );

        // The xlsx + pptx text is present in the store (searchable content).
        let xlsx_real = fs::canonicalize(&xlsx_path).unwrap().display().to_string();
        let pptx_real = fs::canonicalize(&pptx_path).unwrap().display().to_string();
        let all = idx.all_chunks().await.unwrap();
        assert!(
            all.iter().any(|c| c.file_path == xlsx_real && c.chunk_text.contains("Revenue")),
            "xlsx text indexed"
        );
        assert!(
            all.iter().any(|c| c.file_path == pptx_real && c.chunk_text.contains("Project JARVIS")),
            "pptx text indexed"
        );
    }

    // ---- (2) a CORRUPT/GARBAGE pdf/docx is SKIPPED (no index, NO PANIC) ---------

    #[tokio::test]
    async fn corrupt_pdf_and_docx_are_skipped_without_panic_or_empty_rows() {
        let t = TempTree::new("doc-corrupt");
        // A good text file so the index is non-empty (proves the walk continued).
        let good = t.write("docs/ok.md", "a perfectly good readable note");
        // Garbage with a .pdf extension (not a real PDF).
        t.write("docs/broken.pdf", "%PDF-1.4 this is absolutely not a valid pdf stream");
        // Pure garbage bytes with a .pdf extension.
        t.write_bytes("docs/junk.pdf", &[0x25, 0x50, 0x44, 0x46, 0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02]);
        // Garbage with a .docx extension (not a zip at all).
        t.write("docs/broken.docx", "this is plainly not a zip-of-xml docx package");
        // A .docx that IS a zip but is MISSING word/document.xml -> no text -> skip.
        let empty_docx = make_ooxml(&[(
            "[Content_Types].xml",
            r#"<?xml version="1.0"?><Types></Types>"#,
        )]);
        t.write_bytes("docs/noparts.docx", &empty_docx);

        let idx = DocIndex::open(&t.db_path()).unwrap();
        // This call must NOT panic even though three extractors hit malformed input.
        let status = idx
            .reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &DownEmbedder)
            .await
            .unwrap();

        // Only the one good .md file is indexed — every malformed doc is skipped.
        assert_eq!(status.files, 1, "only the good file is indexed: {status:?}");
        let good_real = fs::canonicalize(&good).unwrap().display().to_string();
        let all = idx.all_chunks().await.unwrap();
        assert!(all.iter().all(|c| c.file_path == good_real), "only the good file's chunks: {all:?}");
        // NO empty/garbage row from any skipped doc.
        assert!(
            all.iter().all(|c| !c.chunk_text.contains("not a valid pdf")
                && !c.chunk_text.contains("not a zip-of-xml")),
            "no skipped-file garbage was indexed: {all:?}"
        );
        assert!(
            all.iter().all(|c| !c.chunk_text.trim().is_empty()),
            "no empty chunk row was stored"
        );
    }

    /// The panic-safe boundary itself: even an extractor that PANICS becomes an
    /// honest skip (`None`), never an unwind that crosses the indexer. This is the
    /// catch_unwind proof at the unit level.
    #[test]
    fn extract_guarded_contains_a_panicking_extractor() {
        let p = Path::new("/tmp/whatever.pdf");
        let got = extract_guarded(p, "pdf", || panic!("simulated parser explosion"));
        assert!(got.is_none(), "a panicking extractor must be SKIPPED, not propagate");
        // An extractor that returns empty text is also a skip (scanned/image-only).
        let empty = extract_guarded(p, "pdf", || Ok(String::new()));
        assert!(empty.is_none(), "empty extraction is an honest skip");
        let ws = extract_guarded(p, "pdf", || Ok("   \n\t  ".to_string()));
        assert!(ws.is_none(), "whitespace-only extraction is an honest skip");
        // A real-text extraction is kept.
        let ok = extract_guarded(p, "pdf", || Ok("real extracted text".to_string()));
        assert_eq!(ok.as_deref(), Some("real extracted text"));
    }

    /// A scanned/image-only PDF (no text layer) is HONEST-SKIPPED, never indexed
    /// empty. We model "no text layer" with a valid PDF whose page has no text show
    /// operator at all.
    #[tokio::test]
    async fn image_only_pdf_yields_no_text_and_is_skipped() {
        let t = TempTree::new("scanned-pdf");
        // A structurally-valid PDF with an empty content stream (no Tj) -> no text.
        let no_text = make_pdf("");
        t.write_bytes("docs/scan.pdf", &no_text);
        t.write("docs/real.md", "a real note so the index is not empty");

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let status = idx
            .reindex(&roots_of(&t, "docs"), &IndexBounds::default(), &DownEmbedder)
            .await
            .unwrap();
        // The scanned PDF contributes nothing; only the .md is indexed.
        assert_eq!(status.files, 1, "image-only PDF must not be indexed: {status:?}");
        let all = idx.all_chunks().await.unwrap();
        assert!(
            all.iter().all(|c| c.file_path.ends_with("real.md")),
            "no scanned-PDF row: {all:?}"
        );
    }

    // ---- (3) BOUNDS still hold for the new extractors --------------------------

    #[tokio::test]
    async fn oversize_pdf_is_skipped_by_the_byte_cap_before_parsing() {
        let t = TempTree::new("pdf-oversize");
        // A born-digital PDF padded past the per-file byte cap. The walk's metadata
        // size gate must skip it BEFORE any extractor reads/parses it.
        let big_body = "padding ".repeat(20_000); // ~160 KB of show-text
        let big_pdf = make_pdf(&big_body);
        assert!(big_pdf.len() > 50_000, "fixture is genuinely large: {}", big_pdf.len());
        t.write_bytes("docs/big.pdf", &big_pdf);
        t.write("docs/small.md", "tiny note within the cap");

        let bounds = IndexBounds {
            max_files: 100,
            max_chunks: 1000,
            max_file_bytes: 4096, // smaller than the big pdf, larger than small.md
            max_depth: 8,
            chunk_chars: 256,
            chunk_overlap: 32,
        };
        // The walk drops the oversize pdf at the metadata gate (no read/parse).
        let found = walk(&roots_of(&t, "docs"), &bounds);
        assert!(
            found.iter().all(|d| !d.path.ends_with("big.pdf")),
            "the oversize pdf must be skipped before parsing: {found:?}"
        );

        let idx = DocIndex::open(&t.db_path()).unwrap();
        let status = idx.reindex(&roots_of(&t, "docs"), &bounds, &DownEmbedder).await.unwrap();
        assert_eq!(status.files, 1, "only the within-cap file is indexed: {status:?}");
    }

    #[tokio::test]
    async fn extracted_text_is_capped_before_chunking() {
        // A within-byte-cap docx whose EXTRACTED text far exceeds a tiny text cap.
        // The cap bounds the text fed to the chunker, so the chunk count stays
        // proportional to the cap, not the (much larger) extracted text.
        let long_run = "alpha ".repeat(2000); // ~12 KB of extracted text
        let docx = make_ooxml(&[
            ("[Content_Types].xml", r#"<?xml version="1.0"?><Types></Types>"#),
            (
                "word/document.xml",
                &format!(
                    r#"<?xml version="1.0"?><w:document xmlns:w="http://x"><w:body><w:p><w:r><w:t>{long_run}</w:t></w:r></w:p></w:body></w:document>"#
                ),
            ),
        ]);
        // Extract with a tiny cap directly and assert the bound holds.
        let capped = office_text(&docx, OfficeKind::Docx, 512).unwrap();
        assert!(capped.len() <= 512 + 8, "office_text honored its cap: {} bytes", capped.len());

        // And through extract_text the final text never exceeds the cap.
        let bytes = docx.clone();
        let out = extract_text(Path::new("docs/x.docx"), FileKind::Office(OfficeKind::Docx), &bytes, 300)
            .expect("non-empty");
        assert!(out.len() <= 300, "extract_text honored the cap: {} bytes", out.len());
    }

    #[test]
    fn flate_stream_budget_flags_a_decompression_bomb() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        // Compress 4 MiB of zeros -> a few-KB blob that inflates back to 4 MiB: the
        // shape of a decompression bomb (tiny compressed, large inflated). This is a
        // real FlateDecode stream's raw `content` — the exact bytes lopdf hands us.
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::best());
        enc.write_all(&vec![0u8; 4 << 20]).unwrap();
        let compressed = enc.finish().unwrap();
        assert!(compressed.len() < 64 * 1024, "zeros compress tiny: {} B", compressed.len());

        // A 1 MiB budget: the 4 MiB inflation exceeds it -> flagged as a bomb.
        let mut spent = 0u64;
        assert!(
            !flate_stream_within_budget(&compressed, 1 << 20, &mut spent),
            "4 MiB of inflation must exceed a 1 MiB budget"
        );

        // An 8 MiB budget: 4 MiB fits -> allowed (a legit stream is not false-flagged).
        let mut spent2 = 0u64;
        assert!(
            flate_stream_within_budget(&compressed, 8 << 20, &mut spent2),
            "4 MiB of inflation fits an 8 MiB budget"
        );
        assert_eq!(spent2, 4 << 20, "the full stream was counted");

        // A non-Flate blob contributes nothing (fails to inflate, counts 0).
        let mut spent3 = 0u64;
        assert!(flate_stream_within_budget(b"not a zlib stream at all", 1 << 20, &mut spent3));
        assert_eq!(spent3, 0, "a non-Flate blob adds no inflated bytes");

        // The budget ACCUMULATES across streams: two 4 MiB streams exceed a 6 MiB
        // total budget even though each fits alone.
        let mut acc = 0u64;
        assert!(flate_stream_within_budget(&compressed, 6 << 20, &mut acc)); // 4 MiB spent
        assert!(
            !flate_stream_within_budget(&compressed, 6 << 20, &mut acc),
            "cumulative inflation across streams must trip the shared budget"
        );
    }

    #[test]
    fn office_text_bounds_zip_bomb_decompression() {
        // ZIP-BOMB DEFENSE. A member whose DECOMPRESSED stream dwarfs the read
        // budget must be TRUNCATED, never followed to OOM. We prove the bound
        // POSITIONALLY: a text run sits at the very start of word/document.xml and
        // another past the 16 MiB floor budget, with a ~23 MiB run of text-free
        // elements between them. With a small `cap` the per-document budget floors
        // at 16 MiB, so `Read::take` cuts the decompressed read before the trailing
        // marker is ever seen — the extractor returns the LEADING text but NEVER the
        // trailing one. (Remove the take() bound and the whole member is read, so
        // BOTH markers appear — that is exactly the regression this guards.)
        let filler = "<w:noop></w:noop>".repeat(1_400_000); // ~23 MiB, yields no text
        let document = format!(
            r#"<?xml version="1.0"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>LEADING_MARKER_TEXT</w:t></w:r></w:p>{filler}<w:p><w:r><w:t>TRAILING_MARKER_TEXT</w:t></w:r></w:p></w:body></w:document>"#
        );
        let docx = make_ooxml(&[
            ("[Content_Types].xml", r#"<?xml version="1.0"?><Types></Types>"#),
            ("word/document.xml", &document),
        ]);
        // cap=4096 -> budget floors at 16 MiB, below the ~23 MiB member.
        let out = office_text(&docx, OfficeKind::Docx, 4096).expect("office_text must not error");
        assert!(
            out.contains("LEADING_MARKER_TEXT"),
            "the leading text (before the budget) must still be extracted: {out:?}"
        );
        assert!(
            !out.contains("TRAILING_MARKER_TEXT"),
            "the trailing marker sits past the decompression budget — take() must cut before it"
        );
    }

    #[test]
    fn classify_routes_extensions_to_the_right_kind() {
        assert_eq!(classify(Path::new("a.md")), Some(FileKind::Text));
        assert_eq!(classify(Path::new("a.RS")), Some(FileKind::Text));
        assert_eq!(classify(Path::new("a.pdf")), Some(FileKind::Pdf));
        assert_eq!(classify(Path::new("a.PDF")), Some(FileKind::Pdf));
        assert_eq!(classify(Path::new("a.docx")), Some(FileKind::Office(OfficeKind::Docx)));
        assert_eq!(classify(Path::new("a.xlsx")), Some(FileKind::Office(OfficeKind::Xlsx)));
        assert_eq!(classify(Path::new("a.pptx")), Some(FileKind::Office(OfficeKind::Pptx)));
        // Out of scope: legacy binary office, images, no-extension.
        assert_eq!(classify(Path::new("a.doc")), None);
        assert_eq!(classify(Path::new("a.png")), None);
        assert_eq!(classify(Path::new("Makefile")), None);
        // The new formats are walk-discoverable now.
        assert!(extension_allowed(Path::new("a.pdf")));
        assert!(extension_allowed(Path::new("a.docx")));
        assert!(!extension_allowed(Path::new("a.png")));
    }
}
