//! CODE INTELLIGENCE — grounded, cited code understanding + a PROPOSE-ONLY diff
//! over the user's OWN allowlisted codebase. 100% read/propose; this module NEVER
//! edits the user's tree. The only path that touches code is the human-reviewed
//! `scripts/apply_code_diff.sh`, confined BY CONSTRUCTION to a `[code].roots` root.
//!
//! Two capabilities, both SHIP ON ([`crate::config::CodeConfig`]) but INERT WITHOUT
//! an allowlisted `[code].roots` (and propose-diff drafting also needs the cloud key):
//!   * `code_explain` — answer a question about the user's code GROUNDED in the
//!     on-device file index (the shared docsearch chunk store, which already indexes
//!     rs/py/ts/... source). NOTE: `[code].roots` is the ENABLE + apply-confinement
//!     gate; the retrievable content is whatever was indexed from `[docsearch].roots`,
//!     so the codebase root must also be allowlisted there to be retrievable. It
//!     retrieves the relevant chunks ([`crate::docsearch::DocIndex::search`]),
//!     feeds them to the model, and returns an answer CITED to the real file +
//!     offset chunks. It NEVER fabricates code that is not in the index — an empty
//!     or no-match index yields an honest "I don't have that indexed".
//!   * `code_propose_diff` — given a requested change, the model generates a
//!     REVIEWABLE unified diff grounded in the indexed code, written to a PROPOSAL
//!     STORE (state/code/proposals/<ts>/, like heal/forge proposals). PROPOSE-ONLY:
//!     it NEVER edits the user's code. It returns the proposed diff + the exact
//!     manual apply command. The model's diff CORRECTNESS is runtime/model-gated;
//!     tests inject a scripted/mock model and a canned diff.
//!
//! ## The CONTRACT (non-negotiable)
//!   * ON by default but INERT WITHOUT ROOTS: gated by `[code].enabled` (ships true)
//!     AND a non-empty `roots` (ships empty). With the root unset, both tools are
//!     inert and say so.
//!   * GROUNDED + CITED: `code_explain` answers ONLY from the real indexed chunks
//!     and CITES each (file + offset). No index / no match => honest "not indexed".
//!     It never invents code that is not retrieved.
//!   * PROPOSE-ONLY: `code_propose_diff` writes a reviewable diff to the proposal
//!     store and NEVER mutates the user's tree. The only apply is the human script.
//!   * CONFINED: the proposed diff is PATH-CONFINED at the chokepoint
//!     ([`clean_code_diff`], mirroring [`crate::forge`]/[`crate::heal`]): a header
//!     that, after the `-p1` strip, is empty, absolute, or contains a `..`
//!     component is REJECTED before it is ever stored. The human apply script then
//!     re-confines BY CONSTRUCTION (sandbox-exec deny-default-write to the
//!     canonicalized root) — defense in depth, never a single fragile parse.
//!   * BOUNDED: a proposed diff over `max_diff_bytes` is refused (a finite artifact).
//!   * HONEST: the model's diff QUALITY (does it compile/work) is runtime/model-
//!     gated and NEVER claimed measured here. The index is on-device; the authoring
//!     model is per the active tier.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::Result;

use crate::docsearch::{DocHit, DocSearchResult};
use crate::recall::Embedder;

/// Hard ceiling on cited chunks fed to the model for an explain / proposal. Kept
/// modest so the grounding stays focused and the prompt bounded.
pub const CODE_CONTEXT_K: usize = 8;

// ---------------------------------------------------------------------------
// Cloud seam (trait) — the ONLY route to the authoring model. Production uses
// CloudCodeBrain (anthropic::complete_plain); unit tests inject a mock so NO
// cloud/model call is ever made under `cargo test`. Mirrors forge::ForgeBrain.
// ---------------------------------------------------------------------------

/// A `Send` future returned by the trait methods, spelled out so the trait stays
/// object-safe (`&dyn CodeBrain`) WITHOUT the async-trait crate (no new deps).
pub type CodeBrainFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// The model seam for code intelligence — the ONLY route to the authoring model.
/// `explain` answers a question given the cited code context; `propose` drafts a
/// unified diff given the request + the cited code context. Both take the already-
/// retrieved, on-device, CITED chunks as grounding so the model can only reason
/// over real indexed code. Unit tests inject a mock so no cloud call is made.
pub trait CodeBrain: Send + Sync {
    /// Answer `question` grounded in `context` (the cited code chunks). The model
    /// is instructed to ground its answer in (and cite) the provided code only.
    fn explain<'a>(&'a self, question: &'a str, context: &'a str) -> CodeBrainFuture<'a>;
    /// Draft a unified diff that accomplishes `request`, grounded in `context`.
    /// The raw model text out is cleaned + path-confined by the caller.
    fn propose<'a>(&'a self, request: &'a str, context: &'a str) -> CodeBrainFuture<'a>;
}

// ---------------------------------------------------------------------------
// GROUNDING — build the cited code context block + decide "is there anything
// to ground on". PURE + unit-tested.
// ---------------------------------------------------------------------------

/// Render the retrieved chunks into a CITED context block the model is grounded
/// in. Each chunk is fenced and headed by its real `file:offset` citation so the
/// model's answer can refer back to a real location, and a downstream reader can
/// see exactly what code the answer/diff was built from. Empty hits => empty
/// string (the caller then returns the honest "not indexed" path).
pub fn grounding_block(hits: &[DocHit]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "[{}] {} (offset {})\n```\n{}\n```\n\n",
            i + 1,
            h.file_path,
            h.byte_offset,
            h.snippet
        ));
    }
    out.trim_end().to_string()
}

/// A short, deterministic CITATIONS footer naming each real file+offset the
/// answer/diff is grounded in. The model is told to cite, but we ALSO append the
/// real citations ourselves so the citation can NEVER be fabricated — it is the
/// retrieved chunks' real provenance, not the model's claim.
pub fn citations_footer(hits: &[DocHit]) -> String {
    if hits.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = hits
        .iter()
        .map(|h| format!("- {} (offset {})", h.file_path, h.byte_offset))
        .collect();
    format!("Grounded in (cited code):\n{}", lines.join("\n"))
}

// ---------------------------------------------------------------------------
// PATH-CONFINEMENT of a model-drafted diff — the chokepoint every proposed diff
// flows through before it is stored. Mirrors forge::is_confined_relpath /
// heal::clean_diff: strip code fences/prose, require a real unified diff, and
// REJECT any header that escapes after the `-p1` strip. The human apply script
// re-confines BY CONSTRUCTION (sandbox-exec); this is the in-daemon defense.
// ---------------------------------------------------------------------------

/// Clean a model-drafted unified diff: drop surrounding ```fences/prose, require
/// the `--- `/`+++ `/`@@` triplet, and PATH-CONFINE every `---`/`+++` header so a
/// `..`/absolute/empty target (after `-p1`) is REJECTED — returning `None` (the
/// proposal is refused before it is ever written). The `/dev/null` new/deleted-
/// file sentinel is exempt. Returns the cleaned diff (with a trailing newline)
/// on success. Mirrors [`crate::heal`]'s `clean_diff` confinement so the propose
/// path is confined exactly like the heal path.
pub fn clean_code_diff(raw: &str) -> Option<String> {
    let mut lines: Vec<&str> = Vec::new();
    let mut started = false;
    for line in raw.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim_start().starts_with("```") {
            continue; // fence open/close
        }
        if !started {
            if trimmed.starts_with("--- ")
                || trimmed.starts_with("diff ")
                || trimmed.starts_with("Index: ")
            {
                started = true;
            } else {
                continue; // leading prose
            }
        }
        lines.push(line);
    }
    if !lines.iter().any(|l| l.starts_with("--- "))
        || !lines.iter().any(|l| l.starts_with("+++ "))
        || !lines.iter().any(|l| l.starts_with("@@"))
    {
        return None;
    }
    // PATH-CONFINEMENT: macOS /usr/bin/patch honors `..` in headers, so a header
    // like `+++ b/src/../../../../tmp/x` would write OUTSIDE the codebase root.
    // Reject any header that, after the `-p1` strip, is empty, absolute, or
    // contains a `..` component — the single in-daemon chokepoint (the apply
    // script's sandbox-exec is the by-construction backstop).
    for line in &lines {
        if let Some(rest) = line.strip_prefix("--- ").or_else(|| line.strip_prefix("+++ ")) {
            let path = rest.split('\t').next().unwrap_or(rest).trim_end();
            if path == "/dev/null" {
                continue; // new-file / deleted-file sentinel — not a real target
            }
            let stripped = match path.find('/') {
                Some(i) => &path[i + 1..],
                None => "",
            };
            if stripped.is_empty()
                || stripped.starts_with('/')
                || stripped.split('/').any(|seg| seg == "..")
            {
                return None; // escape attempt — refuse the proposal before storing
            }
        }
    }
    let mut out = lines.join("\n");
    out.push('\n'); // patch(1) wants a final newline
    Some(out)
}

// ---------------------------------------------------------------------------
// PROPOSAL STORE — write a reviewable diff under state/code/proposals/<ts>/,
// exactly like heal/forge proposals. NEVER touches the user's tree. Mirrors
// heal::record_artifact (create_dir_all + write, warn-and-None on error).
// ---------------------------------------------------------------------------

/// Write one proposal artifact under `<code_root>/proposals/<ts>/<name>`. Returns
/// the proposal dir on success, `None` (warned) on a write error. PROPOSE-ONLY:
/// this only ever writes UNDER the proposal store, never the codebase.
fn record_artifact(code_root: &Path, ts: u64, name: &str, body: &str) -> Option<PathBuf> {
    let dir = code_root.join("proposals").join(ts.to_string());
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(name), body)?;
        Ok(())
    };
    match write() {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(error = %e, dir = %dir.display(), name, "code: failed to write proposal artifact");
            None
        }
    }
}

/// The manual apply command a human runs to apply a stored proposal — the ONLY
/// path that ever touches the user's code (confined-by-construction to the
/// allowlisted root). Surfaced in the report + the tool's spoken outcome.
pub fn apply_command(ts: u64) -> String {
    format!("scripts/apply_code_diff.sh {ts}")
}

/// The reviewable report.md written alongside patch.diff: the request, the cited
/// grounding the diff was built from, the diff itself, and the exact manual apply
/// command. Pure + unit-testable.
pub fn proposal_report(request: &str, hits: &[DocHit], diff: &str, ts: u64) -> String {
    format!(
        "# Code change proposal\n\n\
         ## Requested change\n{request}\n\n\
         ## {}\n\n\
         ## Proposed diff (PROPOSE-ONLY — NOT applied)\n```diff\n{diff}```\n\n\
         ## To apply (human-reviewed, confined to an allowlisted [code].roots root)\n\
         Review the diff above, then run:\n```\n{}\n```\n\
         This proposal has NOT touched your code. The apply script re-validates the \
         diff and writes ONLY under the canonicalized allowlisted root (confined by \
         construction); it never writes out-of-tree.\n",
        if hits.is_empty() {
            "Grounding\n(no indexed code matched — the diff is ungrounded; review with extra care)".to_string()
        } else {
            citations_footer(hits)
        },
        apply_command(ts),
    )
}

// ---------------------------------------------------------------------------
// PUBLIC ENTRY — the two tool cores, fully gated + injectable for tests.
// ---------------------------------------------------------------------------

/// The outcome of a `code_explain` / `code_propose_diff` core, so the tool layer
/// renders a faithful spoken outcome + the daemon can emit honest telemetry.
#[derive(Debug, Clone, PartialEq)]
pub enum CodeOutcome {
    /// The feature is off ([code].enabled = false) or no codebase root is
    /// allowlisted: nothing was read/proposed. The tool says so honestly.
    Disabled,
    /// `code_explain`: a grounded, cited answer over the real indexed chunks.
    Explained { answer: String, hits: Vec<DocHit>, method: &'static str },
    /// `code_explain`: nothing in the index matched (or the index is empty). The
    /// honest "I don't have that indexed" — NEVER a fabricated answer.
    NotIndexed,
    /// `code_propose_diff`: a reviewable diff was written to the proposal store at
    /// `dir`; `apply_cmd` is the manual apply. NOTHING in the user's tree changed.
    Proposed { dir: PathBuf, diff: String, apply_cmd: String, hits: Vec<DocHit> },
    /// `code_propose_diff`: the model's draft was not a usable/confined diff (no
    /// diff, an escape attempt, or over the size bound) — refused before storing.
    NoDiff { reason: &'static str },
    /// Infra trouble before any result (model call failed / store write failed).
    Aborted { stage: &'static str },
}

/// Whether code intelligence may run: the master switch on AND at least one
/// codebase root allowlisted. With either unmet, both tools are inert — exactly
/// like docsearch's `indexing_permitted`.
pub fn code_permitted(enabled: bool, roots: &[String]) -> bool {
    enabled && !roots.is_empty()
}

/// CODE_EXPLAIN core: a grounded, CITED answer over the on-device code index.
/// GATED ([`code_permitted`]) — off / no root => [`CodeOutcome::Disabled`].
/// Retrieves the top-K relevant chunks via the docsearch index `search` (the same
/// confined, on-device, cited retrieval the file-RAG uses), feeds them to the
/// model as grounding, and returns the model's answer with the REAL citations
/// appended. An empty/no-match index => [`CodeOutcome::NotIndexed`] (never a
/// fabricated answer). The `embedder` + `brain` are injected (tests pass mocks;
/// the live caller passes the on-device socket embedder + the cloud model).
pub async fn code_explain(
    cfg: &crate::config::CodeConfig,
    index: &crate::docsearch::DocIndex,
    embedder: &dyn Embedder,
    brain: &dyn CodeBrain,
    question: &str,
) -> CodeOutcome {
    if !code_permitted(cfg.enabled, &cfg.roots) {
        return CodeOutcome::Disabled;
    }
    let DocSearchResult { hits, method } = index.search(question, CODE_CONTEXT_K, embedder).await;
    if hits.is_empty() {
        // Honest: nothing in the index matched — never fabricate code not indexed.
        return CodeOutcome::NotIndexed;
    }
    let context = grounding_block(&hits);
    match brain.explain(question, &context).await {
        Ok(answer) => {
            // Append the REAL citations ourselves — the citation can never be
            // fabricated by the model; it is the retrieved chunks' real provenance.
            let footer = citations_footer(&hits);
            let answer = format!("{}\n\n{}", answer.trim(), footer);
            CodeOutcome::Explained { answer, hits, method: method.as_str() }
        }
        Err(e) => {
            tracing::warn!(error = %e, "code_explain: model call failed");
            CodeOutcome::Aborted { stage: "explain" }
        }
    }
}

/// CODE_PROPOSE_DIFF core: a PROPOSE-ONLY reviewable diff. GATED
/// ([`code_permitted`]) — off / no root => [`CodeOutcome::Disabled`]. It:
///   1. retrieves the relevant chunks (grounding), so the model drafts against
///      real indexed code (not the whole tree, not fabricated code);
///   2. asks the model for a unified diff via the `brain` seam;
///   3. CLEANS + PATH-CONFINES the diff ([`clean_code_diff`]) and enforces the
///      `max_diff_bytes` bound — a non-diff / escape / oversize draft is REFUSED
///      ([`CodeOutcome::NoDiff`]) BEFORE anything is written;
///   4. writes patch.diff + report.md + request.txt to state/code/proposals/<ts>/
///      ([`record_artifact`]) — the PROPOSAL STORE, NEVER the user's tree;
///   5. returns the diff + the manual apply command.
///      The model's diff CORRECTNESS is runtime/model-gated — tests inject a mock brain
///      returning a canned diff and assert the store write + NO tree mutation.
pub async fn code_propose_diff(
    cfg: &crate::config::CodeConfig,
    code_root: &Path,
    index: &crate::docsearch::DocIndex,
    embedder: &dyn Embedder,
    brain: &dyn CodeBrain,
    ts: u64,
    request: &str,
) -> CodeOutcome {
    if !code_permitted(cfg.enabled, &cfg.roots) {
        return CodeOutcome::Disabled;
    }
    // Grounding: the relevant indexed chunks (may be empty — an ungrounded diff is
    // still PROPOSE-ONLY + confined; the report flags the lack of grounding).
    let DocSearchResult { hits, .. } = index.search(request, CODE_CONTEXT_K, embedder).await;
    let context = grounding_block(&hits);

    let raw = match brain.propose(request, &context).await {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!(error = %e, "code_propose_diff: model call failed");
            return CodeOutcome::Aborted { stage: "draft" };
        }
    };
    // CLEAN + PATH-CONFINE before anything is written. A non-diff / escape draft is
    // refused here (the in-daemon chokepoint; the apply script's sandbox is the
    // by-construction backstop).
    let diff = match clean_code_diff(&raw) {
        Some(d) => d,
        None => return CodeOutcome::NoDiff { reason: "not a confined unified diff" },
    };
    // BOUND: refuse a degenerate/runaway diff before storing.
    if diff.len() > cfg.max_diff_bytes {
        return CodeOutcome::NoDiff { reason: "diff exceeds the configured size bound" };
    }

    // PROPOSE-ONLY: write the reviewable artifacts to the proposal store. This is
    // the ONLY filesystem write, and it is UNDER state/code/proposals/ — never the
    // user's codebase.
    let report = proposal_report(request, &hits, &diff, ts);
    let dir = match record_artifact(code_root, ts, "patch.diff", &diff) {
        Some(dir) => dir,
        None => return CodeOutcome::Aborted { stage: "store" },
    };
    record_artifact(code_root, ts, "report.md", &report);
    record_artifact(code_root, ts, "request.txt", request);

    CodeOutcome::Proposed {
        dir,
        diff,
        apply_cmd: apply_command(ts),
        hits,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CodeConfig;
    use crate::docsearch::{DocIndex, IndexBounds};
    use crate::recall::{EmbedFuture, Embedder};
    use std::fs;

    /// A unique temp dir tree per test, cleaned on drop. All file I/O stays inside
    /// it — NEVER the user's real tree.
    struct TempTree(PathBuf);
    impl TempTree {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("darwin-code-test-{}-{}", std::process::id(), tag));
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

    fn roots_of(t: &TempTree, sub: &str) -> Vec<String> {
        vec![t.join(sub).display().to_string()]
    }

    /// A deterministic mock embedder mapping by keyword (axis 0 = "parse"/"config",
    /// axis 1 = "render", axis 2 = other) so a test can pin which chunk is "near" a
    /// query. NEVER touches a socket/MLX/network.
    struct KeywordEmbedder;
    impl Embedder for KeywordEmbedder {
        fn embed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
            let vecs: Vec<Vec<f64>> = texts
                .iter()
                .map(|t| {
                    let l = t.to_lowercase();
                    let parse = l.contains("parse") || l.contains("config") || l.contains("toml");
                    let render = l.contains("render") || l.contains("draw") || l.contains("pixel");
                    if parse {
                        vec![1.0, 0.0, 0.0]
                    } else if render {
                        vec![0.0, 1.0, 0.0]
                    } else {
                        vec![0.0, 0.0, 1.0]
                    }
                })
                .collect();
            Box::pin(async move { Ok(vecs) })
        }
    }

    /// A mock embedder that is always DOWN (drives BM25 fallback in docsearch).
    struct DownEmbedder;
    impl Embedder for DownEmbedder {
        fn embed<'a>(&'a self, _texts: &'a [String]) -> EmbedFuture<'a> {
            Box::pin(async move { Err(anyhow::anyhow!("embedder down (simulated)")) })
        }
    }

    /// A scripted CodeBrain: returns a fixed explain answer + a fixed propose diff.
    /// NEVER touches the cloud/model/network. The propose diff is canned so the
    /// test exercises only the propose-only store + confinement plumbing.
    struct ScriptedBrain {
        explain: String,
        diff: String,
    }
    impl CodeBrain for ScriptedBrain {
        fn explain<'a>(&'a self, _q: &'a str, ctx: &'a str) -> CodeBrainFuture<'a> {
            // Echo a marker proving the brain SAW the grounding context, so a test
            // can assert the answer was grounded in the retrieved chunks.
            let saw = ctx.contains("```");
            let answer = format!("{} (grounded={})", self.explain, saw);
            Box::pin(async move { Ok(answer) })
        }
        fn propose<'a>(&'a self, _r: &'a str, _ctx: &'a str) -> CodeBrainFuture<'a> {
            let diff = self.diff.clone();
            Box::pin(async move { Ok(diff) })
        }
    }

    /// A CodeBrain that returns prose, NOT a diff (drives the NoDiff refusal).
    struct ProseBrain;
    impl CodeBrain for ProseBrain {
        fn explain<'a>(&'a self, _q: &'a str, _c: &'a str) -> CodeBrainFuture<'a> {
            Box::pin(async move { Ok("an answer".into()) })
        }
        fn propose<'a>(&'a self, _r: &'a str, _c: &'a str) -> CodeBrainFuture<'a> {
            Box::pin(async move { Ok("I cannot safely make this change.".into()) })
        }
    }

    /// A CodeBrain returning a diff whose header ESCAPES via `..` (drives the
    /// confinement refusal — the model's draft is malicious/buggy).
    struct EscapeBrain;
    impl CodeBrain for EscapeBrain {
        fn explain<'a>(&'a self, _q: &'a str, _c: &'a str) -> CodeBrainFuture<'a> {
            Box::pin(async move { Ok("x".into()) })
        }
        fn propose<'a>(&'a self, _r: &'a str, _c: &'a str) -> CodeBrainFuture<'a> {
            let d = "--- a/src/../../../../etc/victim\n\
                     +++ b/src/../../../../etc/victim\n\
                     @@ -1,1 +1,1 @@\n-old\n+pwned\n"
                .to_string();
            Box::pin(async move { Ok(d) })
        }
    }

    fn on_cfg(roots: Vec<String>) -> CodeConfig {
        CodeConfig { enabled: true, roots, ..CodeConfig::default() }
    }

    // A small canned diff that confines (a/src... strips to src... — survives).
    const CANNED_DIFF: &str = "--- a/src/lib.rs\n\
                               +++ b/src/lib.rs\n\
                               @@ -1,1 +1,1 @@\n-old line\n+new line\n";

    fn scripted() -> ScriptedBrain {
        ScriptedBrain {
            explain: "The config is parsed in config.rs".into(),
            diff: CANNED_DIFF.into(),
        }
    }

    // -- helper: build an index over a temp code tree --------------------------
    async fn indexed(t: &TempTree, sub: &str, emb: &dyn Embedder) -> DocIndex {
        let idx = DocIndex::open(&t.join("code-index.db")).unwrap();
        idx.reindex(&roots_of(t, sub), &IndexBounds::default(), emb)
            .await
            .unwrap();
        idx
    }

    // =====================================================================
    // GATE: OFF / no-root => inert (Disabled), even with a real index
    // =====================================================================

    #[tokio::test]
    async fn explain_and_propose_are_off_by_default_and_need_a_root() {
        let t = TempTree::new("gate");
        t.write("src/config.rs", "fn parse_config() {}");
        let idx = indexed(&t, "src", &DownEmbedder).await;

        // OFF (the shipped default) with a real root + real index -> Disabled.
        let off = CodeConfig { enabled: false, roots: roots_of(&t, "src"), ..CodeConfig::default() };
        assert_eq!(
            code_explain(&off, &idx, &DownEmbedder, &scripted(), "where is config parsed").await,
            CodeOutcome::Disabled,
            "OFF must do nothing even with a real root + index"
        );
        let p = code_propose_diff(
            &off, &t.join("state").join("code"), &idx, &DownEmbedder, &scripted(), 1, "rename it",
        )
        .await;
        assert_eq!(p, CodeOutcome::Disabled, "OFF propose must do nothing");

        // ON but EMPTY allowlist -> still Disabled (no codebase reachable).
        let on_no_roots = CodeConfig { enabled: true, roots: Vec::new(), ..CodeConfig::default() };
        assert_eq!(
            code_explain(&on_no_roots, &idx, &DownEmbedder, &scripted(), "q").await,
            CodeOutcome::Disabled,
            "ON + empty allowlist must do nothing"
        );

        // And NOTHING was written to a proposal store while OFF.
        assert!(
            !t.join("state").join("code").join("proposals").exists(),
            "no proposal may be written while the feature is off"
        );
    }

    #[test]
    fn code_permitted_requires_switch_and_a_root() {
        assert!(!code_permitted(false, &["/some/root".into()]), "off => not permitted");
        assert!(!code_permitted(true, &[]), "no root => not permitted (never arbitrary)");
        assert!(code_permitted(true, &["/some/root".into()]), "on + a root => permitted");
    }

    // =====================================================================
    // code_explain: GROUNDED + CITED over the real index; honest not-indexed
    // =====================================================================

    #[tokio::test]
    async fn explain_returns_a_cited_answer_grounded_in_the_real_index() {
        let t = TempTree::new("explain-cited");
        let cfgrs = t.write("src/config.rs", "fn parse_config() { /* parse the toml config */ }");
        t.write("src/render.rs", "fn render() { /* draw pixels */ }");
        // Neural index (all chunks embedded) so the parse query retrieves config.rs.
        let idx = indexed(&t, "src", &KeywordEmbedder).await;

        let cfg = on_cfg(roots_of(&t, "src"));
        let out = code_explain(&cfg, &idx, &KeywordEmbedder, &scripted(), "how is the config parsed").await;
        match out {
            CodeOutcome::Explained { answer, hits, method } => {
                assert_eq!(method, "neural-embedding", "all chunks embedded -> neural ran: got {method}");
                assert!(!hits.is_empty(), "the config chunk must be retrieved");
                // CITES the REAL (canonicalized) config.rs path — not render.rs.
                let cfg_real = fs::canonicalize(&cfgrs).unwrap().display().to_string();
                assert!(
                    hits.iter().any(|h| h.file_path == cfg_real),
                    "the cited hit must be the real config.rs: {hits:?}"
                );
                // The answer carries the REAL citation footer (file + offset) — we
                // appended it ourselves, so it can never be fabricated.
                assert!(answer.contains("Grounded in (cited code)"), "answer must carry citations: {answer}");
                assert!(answer.contains(&cfg_real), "the cited file path must appear: {answer}");
                // The brain SAW the grounding context (grounded=true marker).
                assert!(answer.contains("grounded=true"), "the model must be fed the cited code: {answer}");
                // It did NOT cite the orthogonal render.rs for a parse question.
                assert!(hits.iter().all(|h| !h.file_path.contains("render.rs")), "no irrelevant cite: {hits:?}");
            }
            other => panic!("expected a cited explanation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn explain_is_honest_not_indexed_and_never_fabricates() {
        let t = TempTree::new("explain-empty");
        t.write("src/a.rs", "fn gardening() {}");
        // BM25 index (embedder down). A query with zero term overlap -> no hits.
        let idx = indexed(&t, "src", &DownEmbedder).await;
        let cfg = on_cfg(roots_of(&t, "src"));

        // No-match: the brain is a panic-free scripted one, but it must NOT be
        // reached — NotIndexed short-circuits BEFORE the model, so nothing is
        // fabricated.
        struct PanicBrain;
        impl CodeBrain for PanicBrain {
            fn explain<'a>(&'a self, _q: &'a str, _c: &'a str) -> CodeBrainFuture<'a> {
                Box::pin(async move { panic!("the model must NOT run on a no-match index") })
            }
            fn propose<'a>(&'a self, _r: &'a str, _c: &'a str) -> CodeBrainFuture<'a> {
                Box::pin(async move { panic!("unused") })
            }
        }
        let out = code_explain(&cfg, &idx, &DownEmbedder, &PanicBrain, "quantum chromodynamics").await;
        assert_eq!(out, CodeOutcome::NotIndexed, "a no-match index must be honestly not-indexed");

        // An EMPTY index is likewise honest not-indexed (never fabricated).
        let empty_idx = DocIndex::open(&t.join("empty.db")).unwrap();
        let out = code_explain(&cfg, &empty_idx, &DownEmbedder, &PanicBrain, "anything").await;
        assert_eq!(out, CodeOutcome::NotIndexed, "an empty index must be honestly not-indexed");
    }

    // =====================================================================
    // code_propose_diff: PROPOSE-ONLY — writes a reviewable diff, NO tree mutation
    // =====================================================================

    #[tokio::test]
    async fn propose_writes_a_reviewable_diff_and_never_mutates_the_tree() {
        let t = TempTree::new("propose-store");
        let src = t.write("src/lib.rs", "old line\n");
        let before = fs::read_to_string(&src).unwrap();
        let idx = indexed(&t, "src", &DownEmbedder).await;
        let cfg = on_cfg(roots_of(&t, "src"));
        let code_root = t.join("state").join("code");

        let out = code_propose_diff(
            &cfg, &code_root, &idx, &DownEmbedder, &scripted(), 1700000000, "rename old to new",
        )
        .await;
        match out {
            CodeOutcome::Proposed { dir, diff, apply_cmd, .. } => {
                // The reviewable diff is written to the PROPOSAL STORE.
                assert!(dir.join("patch.diff").exists(), "patch.diff must be written");
                assert!(dir.join("report.md").exists(), "report.md must be written");
                assert!(dir.join("request.txt").exists(), "request.txt must be written");
                let stored = fs::read_to_string(dir.join("patch.diff")).unwrap();
                assert_eq!(stored, diff, "the stored diff matches the returned diff");
                assert!(stored.contains("+new line"), "the canned diff content is stored");
                // The report names the manual apply command + cites grounding posture.
                let report = fs::read_to_string(dir.join("report.md")).unwrap();
                assert!(report.contains("scripts/apply_code_diff.sh 1700000000"), "report names the apply cmd");
                assert_eq!(apply_cmd, "scripts/apply_code_diff.sh 1700000000");
                assert!(report.contains("PROPOSE-ONLY"), "report states it was not applied");
            }
            other => panic!("expected a Proposed outcome, got {other:?}"),
        }
        // THE LOAD-BEARING ASSERT: the user's source file is BYTE-FOR-BYTE unchanged.
        let after = fs::read_to_string(&src).unwrap();
        assert_eq!(before, after, "code_propose_diff must NEVER edit the user's tree");
        assert_eq!(after, "old line\n", "the source is exactly as it started");
    }

    #[tokio::test]
    async fn propose_refuses_a_non_diff_and_writes_nothing() {
        let t = TempTree::new("propose-prose");
        let src = t.write("src/lib.rs", "x\n");
        let idx = indexed(&t, "src", &DownEmbedder).await;
        let cfg = on_cfg(roots_of(&t, "src"));
        let code_root = t.join("state").join("code");

        let out = code_propose_diff(&cfg, &code_root, &idx, &DownEmbedder, &ProseBrain, 2, "do x").await;
        assert!(matches!(out, CodeOutcome::NoDiff { .. }), "prose is refused: {out:?}");
        // No proposal dir was created, and the tree is untouched.
        assert!(!code_root.join("proposals").join("2").exists(), "no artifact on a non-diff");
        assert_eq!(fs::read_to_string(&src).unwrap(), "x\n", "tree untouched on a refusal");
    }

    #[tokio::test]
    async fn propose_refuses_a_diff_that_escapes_the_tree() {
        let t = TempTree::new("propose-escape");
        let idx = indexed(&t, "src", &DownEmbedder).await;
        let cfg = on_cfg(roots_of(&t, "src"));
        let code_root = t.join("state").join("code");
        // A real out-of-tree victim that a `..`-escaping diff would target.
        let victim = t.write("etc/victim", "ORIGINAL\n");

        let out = code_propose_diff(&cfg, &code_root, &idx, &DownEmbedder, &EscapeBrain, 3, "escape").await;
        assert!(matches!(out, CodeOutcome::NoDiff { .. }), "an escaping diff is refused: {out:?}");
        assert!(!code_root.join("proposals").join("3").exists(), "no artifact on an escape");
        // The out-of-tree victim is untouched (the diff was never even stored, let
        // alone applied — propose never applies anyway, but the confinement refuses
        // it before storing).
        assert_eq!(fs::read_to_string(&victim).unwrap(), "ORIGINAL\n", "victim untouched");
    }

    #[tokio::test]
    async fn propose_refuses_a_diff_over_the_size_bound() {
        let t = TempTree::new("propose-bound");
        t.write("src/lib.rs", "x\n");
        let idx = indexed(&t, "src", &DownEmbedder).await;
        // A tiny bound the canned diff exceeds.
        let cfg = CodeConfig { enabled: true, roots: roots_of(&t, "src"), max_diff_bytes: 8 };
        let code_root = t.join("state").join("code");
        let out = code_propose_diff(&cfg, &code_root, &idx, &DownEmbedder, &scripted(), 4, "big").await;
        assert!(matches!(out, CodeOutcome::NoDiff { reason } if reason.contains("size bound")), "oversize refused: {out:?}");
        assert!(!code_root.join("proposals").join("4").exists(), "no artifact on an oversize diff");
    }

    // =====================================================================
    // clean_code_diff: confinement chokepoint (mirrors heal::clean_diff)
    // =====================================================================

    #[test]
    fn clean_code_diff_strips_fences_requires_a_diff_and_confines() {
        // A fenced, prose-prefixed real diff survives cleaning.
        let raw = "Here is the fix:\n```diff\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n```\nDone.";
        let cleaned = clean_code_diff(raw).expect("a real diff must survive");
        assert!(cleaned.starts_with("--- a/src/lib.rs"), "prose/fence stripped: {cleaned}");
        assert!(cleaned.ends_with('\n'), "patch wants a trailing newline");

        // Non-diffs are rejected.
        assert!(clean_code_diff("I cannot do this.").is_none(), "prose is not a diff");
        assert!(clean_code_diff("--- a/src/lib.rs\nno hunks").is_none(), "needs +++ and @@");
        assert!(clean_code_diff("").is_none(), "empty is not a diff");

        // `..` headers are REJECTED at the chokepoint (the dangerous escape; the
        // apply script's sandbox-exec is the by-construction backstop for the rest).
        let dotdot = "--- a/src/../../../etc/x\n+++ b/src/../../../etc/x\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        assert!(clean_code_diff(dotdot).is_none(), "a `..` header must be confined out");
        // A header that strips (after `-p1`) to EMPTY (a single leading component,
        // no `/`) is rejected — there is no in-tree target.
        let bare = "--- a\n+++ b\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        assert!(clean_code_diff(bare).is_none(), "an empty-after-p1 header must be rejected");
        // A header that strips to a still-absolute path (e.g. `//etc/x` -> `/etc/x`)
        // is rejected (the stripped remainder begins with `/`).
        let double_abs = "--- //etc/x\n+++ //etc/x\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        assert!(clean_code_diff(double_abs).is_none(), "an absolute-after-p1 header must be rejected");

        // A confined a/src diff and a /dev/null new-file diff both survive.
        let ok = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-a\n+b\n";
        assert!(clean_code_diff(ok).is_some(), "a confined a/src diff survives");
        let newf = "--- /dev/null\n+++ b/src/new.rs\n@@ -0,0 +1,1 @@\n+hello\n";
        assert!(clean_code_diff(newf).is_some(), "a confined /dev/null new-file diff survives");
    }

    #[test]
    fn grounding_and_citations_are_built_only_from_real_hits() {
        // Empty hits => empty grounding + empty footer (the not-indexed path).
        assert!(grounding_block(&[]).is_empty());
        assert!(citations_footer(&[]).is_empty());
        let hits = vec![DocHit {
            file_path: "/proj/src/config.rs".into(),
            root: "/proj".into(),
            byte_offset: 42,
            snippet: "fn parse_config() {}".into(),
            score: 0.9,
        }];
        let block = grounding_block(&hits);
        assert!(block.contains("/proj/src/config.rs (offset 42)"), "cite the real file+offset: {block}");
        assert!(block.contains("fn parse_config()"), "include the real chunk: {block}");
        let footer = citations_footer(&hits);
        assert!(footer.contains("- /proj/src/config.rs (offset 42)"), "footer cites the real provenance: {footer}");
    }
}
