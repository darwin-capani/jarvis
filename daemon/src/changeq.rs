//! CHANGE QUEUE (changeq.rs) — ONE git-native review lane unifying every
//! PROPOSE-ONLY artifact DARWIN produces.
//!
//! DARWIN has four propose-only writers, each already human-gated and each with
//! its OWN on-disk proposal store + its OWN apply script:
//!   * self-heal  ([`crate::heal`])     -> state/heal/proposals/<ts>/     -> scripts/apply_heal.sh
//!   * code       ([`crate::code`])     -> state/code/proposals/<ts>/     -> scripts/apply_code_diff.sh
//!   * app-forge  ([`crate::forge`])    -> state/forge/proposals/<ts>/    -> scripts/apply_forge.sh
//!   * optimizer  ([`crate::optimize`]) -> state/optimize/proposals/<ts>/ -> scripts/apply_optimization.sh
//!
//! This module is PURE BOOKKEEPING over those already-propose-only artifacts. It
//! adds nothing to the daemon's authority: it does not draft, it does not apply,
//! it invents NO new apply path. What it adds is a single REVIEW LANE:
//!   1. When a writer produces a proposal it ALSO registers the artifact into a
//!      bounded, in-memory queue ([`on_proposal`]) and, on-device, commits that
//!      artifact onto a dedicated LOCAL git branch ([`BRANCH`]) with provenance
//!      TRAILERS — via bounded, fixed-arg git subprocesses that NEVER touch the
//!      user's working tree or HEAD (git plumbing over a separate index file).
//!   2. A read-only `changeq_list` surface lists every pending proposal with its
//!      kind, its diff/artifact locator, and its provenance.
//!   3. A uniform `changeq_apply` resolves a chosen proposal to THAT type's
//!      EXISTING apply script ([`ChangeKind::apply_script`]) — it routes to the
//!      existing, human-gated re-validation; it does NOT invent a new authority.
//!   4. Rollback is a safe `git revert` on the review branch ([`revert_command`]).
//!
//! ## Contract (non-negotiable)
//!   * PURE, TESTABLE SEAM. The queue model ([`ChangeQueue`]), the provenance
//!     trailer construction ([`Provenance::trailer_lines`]), and the git-command
//!     construction (the `*_command` builders + [`commit_change`]) are PURE and
//!     unit-tested. The ACTUAL git exec is the RUNNER ([`GitRunner`]) — a mock in
//!     tests, the device-gated [`RealGitRunner`] on-device (built, never invoked
//!     under `cargo test`, exactly like [`crate::realm::SandboxedRealmRunner`]).
//!   * CONFINED. Every committed artifact path is CONFINED under `state/` (never
//!     absolute, never a `..` climb) — enforced at [`is_confined_artifact`] and
//!     by DERIVING the path from `(kind, ts)` so it can only ever be a proposal
//!     store path. The git subprocesses are FIXED-ARG (no shell, no interpolation
//!     into a shell string), mirroring [`crate::realm`]'s `run_fixed`.
//!   * SECRET-FREE PROVENANCE. The four trailers carry ONLY agent / model / run
//!     id / state-hash — never a secret. Each value is sanitized to a single
//!     bounded line and a secret-shaped value is REFUSED ([`is_secret_free`]).
//!   * PROPOSE-ONLY, UNCHANGED. Registering an artifact does not change any
//!     writer's propose-only contract: the artifact was already written; the
//!     branch commit is a mirror for review, and apply still runs each type's
//!     mandatory re-validation + human confirmation.
//!   * ARMED-BY-DEFAULT, INERT WITHOUT A REPO. `[changeq].enabled` ships true, but
//!     with no git repo the commit is a no-op (the in-memory queue + list still
//!     work) — it never fabricates a branch it could not create.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::RwLock;

use serde_json::{json, Value};

use crate::telemetry;

/// The dedicated LOCAL review branch every proposal is committed onto. It is a
/// namespaced ref (never `main`/`master`), created + advanced ONLY by the change
/// queue's plumbing, and never checked out — so it never disturbs the user's
/// working tree.
pub const BRANCH: &str = "darwin/changeq";

/// The full ref name of [`BRANCH`] (what `update-ref` writes / `rev-parse` reads).
pub const BRANCH_REF: &str = "refs/heads/darwin/changeq";

/// `git`, resolved via PATH by the device-gated runner. Used ONLY for the
/// daemon-controlled review-branch plumbing — never for anything user-supplied.
pub const GIT: &str = "git";

/// The telemetry event the read-only list surface emits (live, on-demand — not a
/// retained announce topic, exactly like `artifact.peek`).
pub const LIST_EVENT: &str = "changeq.list";

/// Default queue bound when [`configure`] has not run yet (mirrored by
/// [`crate::config::ChangeqConfig::default`]). A review lane, not a history store.
pub const DEFAULT_QUEUE_BOUND: usize = 64;

/// Hard ceiling on the bound regardless of config — the queue is a bounded review
/// window, never an unbounded ledger. A config asking for more is clamped here.
pub const MAX_QUEUE_BOUND: usize = 512;

/// Bound on one trailer value so a runaway producer cannot blow the commit
/// message (and so the frame stays secret-free-by-size). Trailer values are short
/// identifiers (an agent name, a model id, a ts, a hex fingerprint).
const MAX_TRAILER_VALUE_LEN: usize = 200;

/// Bound on the producer-supplied one-line summary.
const MAX_SUMMARY_LEN: usize = 300;

/// Bound on the (derived) artifact locator string.
const MAX_ARTIFACT_REL_LEN: usize = 400;

// ---------------------------------------------------------------------------
// KIND — the closed vocabulary of propose-only writers this lane unifies
// ---------------------------------------------------------------------------

/// The kind of propose-only artifact. A CLOSED vocabulary: each variant maps to
/// its EXISTING proposal store + its EXISTING apply script, so the change queue
/// can never invent a new store or a new apply authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// A self-heal patch proposal ([`crate::heal`]).
    Heal,
    /// A code-change diff proposal ([`crate::code`]).
    Code,
    /// A forged micro-app proposal ([`crate::forge`]).
    Forge,
    /// A routing-optimization proposal ([`crate::optimize`]).
    Optimize,
}

impl ChangeKind {
    /// The stable, lowercase wire string the HUD switches on.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChangeKind::Heal => "heal",
            ChangeKind::Code => "code",
            ChangeKind::Forge => "forge",
            ChangeKind::Optimize => "optimize",
        }
    }

    /// Inverse of [`Self::as_str`]; an unknown token is `None` (never guessed into
    /// a kind — an unrecognized kind carries no store/apply mapping). Named
    /// `from_wire` (not `from_str`) so it stays a plain fallible parser, not a
    /// `FromStr` impl.
    pub fn from_wire(s: &str) -> Option<ChangeKind> {
        match s {
            "heal" => Some(ChangeKind::Heal),
            "code" => Some(ChangeKind::Code),
            "forge" => Some(ChangeKind::Forge),
            "optimize" => Some(ChangeKind::Optimize),
            _ => None,
        }
    }

    /// The proposal store subdirectory (repo-root-relative, under `state/`) this
    /// kind writes into — the SAME dir the existing writer already uses. Combined
    /// with the `<ts>` this yields the confined artifact locator.
    pub fn proposals_subdir(&self) -> &'static str {
        match self {
            ChangeKind::Heal => "state/heal/proposals",
            ChangeKind::Code => "state/code/proposals",
            ChangeKind::Forge => "state/forge/proposals",
            ChangeKind::Optimize => "state/optimize/proposals",
        }
    }

    /// The EXISTING, human-gated apply script for this kind — the re-validation
    /// AUTHORITY `changeq_apply` routes to. This is the ONE place apply authority
    /// is named, and it is ALWAYS one of the four pre-existing scripts: the change
    /// queue NEVER invents a new apply path.
    pub fn apply_script(&self) -> &'static str {
        match self {
            ChangeKind::Heal => "scripts/apply_heal.sh",
            ChangeKind::Code => "scripts/apply_code_diff.sh",
            ChangeKind::Forge => "scripts/apply_forge.sh",
            ChangeKind::Optimize => "scripts/apply_optimization.sh",
        }
    }

    /// The confined artifact locator for a proposal of this kind at `ts`
    /// (`<proposals_subdir>/<ts>`). Confined BY CONSTRUCTION — the subdir is a
    /// fixed `state/...` constant and `ts` is a number, so the result can only
    /// ever be a proposal-store path.
    pub fn artifact_rel(&self, ts: u64) -> String {
        format!("{}/{}", self.proposals_subdir(), ts)
    }

    /// The exact EXISTING apply command a human runs for this proposal — the
    /// existing script + the proposal `<ts>`. This is what `changeq_apply`
    /// surfaces; it re-validates and applies under that type's own gate.
    pub fn apply_command(&self, ts: u64) -> String {
        format!("{} {}", self.apply_script(), ts)
    }
}

// ---------------------------------------------------------------------------
// PROVENANCE — the four secret-free commit trailers
// ---------------------------------------------------------------------------

/// The honest, SECRET-FREE provenance stamped onto a review-branch commit as git
/// trailers. Exactly four fields — agent / model / run id / state-hash — none of
/// which is EVER a secret. Values are sanitized to a single bounded line at
/// construction so they cannot break the trailer format or smuggle extra
/// trailers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The agent/component that produced the artifact (e.g. "steve", "self-heal").
    pub agent: String,
    /// The model that authored it (e.g. "claude-opus-4-8"), or a component label
    /// for a deterministic writer. NEVER a key/token — this is a model id.
    pub model: String,
    /// The run id the artifact belongs to (the proposal `<ts>` by default).
    pub run_id: String,
    /// A short, NON-crypto content FINGERPRINT of the proposed change (see
    /// [`fingerprint`]) — a change-detection tag, never a security hash, never a
    /// secret.
    pub state_hash: String,
}

impl Provenance {
    /// Build a provenance, sanitizing every field to a single bounded line. An
    /// empty/unknown field becomes the honest placeholder `"unknown"` (the trailer
    /// is always present so all four trailers are emitted).
    pub fn new(
        agent: impl Into<String>,
        model: impl Into<String>,
        run_id: impl Into<String>,
        state_hash: impl Into<String>,
    ) -> Provenance {
        Provenance {
            agent: sanitize_trailer_value(&agent.into()),
            model: sanitize_trailer_value(&model.into()),
            run_id: sanitize_trailer_value(&run_id.into()),
            state_hash: sanitize_trailer_value(&state_hash.into()),
        }
    }

    /// The four git commit trailers, in a FIXED order, one per line:
    /// `Darwin-Agent:` / `Darwin-Model:` / `Darwin-Run:` / `Darwin-State-Hash:`.
    /// All four are ALWAYS present (an unknown value is `"unknown"`), so a reader
    /// can rely on the shape.
    pub fn trailer_lines(&self) -> Vec<String> {
        vec![
            format!("Darwin-Agent: {}", self.agent),
            format!("Darwin-Model: {}", self.model),
            format!("Darwin-Run: {}", self.run_id),
            format!("Darwin-State-Hash: {}", self.state_hash),
        ]
    }

    /// The trailers as one newline-joined block (the last paragraph of a commit
    /// message — git's trailer format).
    pub fn trailers_block(&self) -> String {
        self.trailer_lines().join("\n")
    }

    /// SECRET-FREE guard: true iff NO trailer value looks like a secret/token.
    /// Trailers are agent/model/run/state-hash — never secrets — so a
    /// secret-shaped value is a bug this refuses (defense in depth on top of the
    /// sanitizer). The write path checks this before committing.
    pub fn is_secret_free(&self) -> bool {
        ![&self.agent, &self.model, &self.run_id, &self.state_hash]
            .into_iter()
            .any(|v| looks_secretish(v))
    }

    /// The secret-free JSON the list frame carries (locators only — no bodies).
    fn to_frame(&self) -> Value {
        json!({
            "agent": self.agent,
            "model": self.model,
            "run": self.run_id,
            "state_hash": self.state_hash,
        })
    }
}

/// Sanitize a trailer value: trim, collapse ALL internal whitespace (incl.
/// newlines/tabs/control chars) to single spaces, drop other control chars, and
/// bound the length. Newlines are the dangerous case — an un-collapsed newline
/// would split the trailer or inject a forged trailer line. Empty after cleaning
/// becomes `"unknown"` so every trailer stays present + honest.
fn sanitize_trailer_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_TRAILER_VALUE_LEN));
    let mut prev_space = false;
    for c in s.trim().chars() {
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        if c.is_control() {
            continue; // drop stray control chars entirely
        }
        out.push(c);
        prev_space = false;
    }
    let trimmed = out.trim_end();
    let clamped = clamp(trimmed, MAX_TRAILER_VALUE_LEN);
    if clamped.is_empty() {
        "unknown".to_string()
    } else {
        clamped
    }
}

/// Whether a value looks like a secret/token (a known secret prefix, an inline
/// `key=`/`token=` assignment, or a long mixed alphanumeric run). Conservative +
/// pure — the change queue's trailers must never carry one. Mirrors the shape of
/// [`crate::optimize`]'s redactor's secret rule, kept local (no cross-module
/// private coupling).
fn looks_secretish(v: &str) -> bool {
    let lower = v.to_ascii_lowercase();
    for key in ["api_key=", "apikey=", "api-key=", "token=", "secret=", "password=", "bearer "] {
        if lower.contains(key) {
            return true;
        }
    }
    for tok in v.split_whitespace() {
        let l = tok.to_ascii_lowercase();
        const PREFIXES: &[&str] =
            &["sk-", "pk-", "rk-", "ghp_", "gho_", "ghs_", "github_pat_", "xoxb-", "xoxp-", "akia"];
        if PREFIXES.iter().any(|p| l.starts_with(p) && tok.len() >= p.len() + 8) {
            return true;
        }
        // A long, high-entropy-ish run mixing letters and digits.
        let n = tok.chars().count();
        if n >= 24 {
            let has_alpha = tok.chars().any(|c| c.is_ascii_alphabetic());
            let has_digit = tok.chars().any(|c| c.is_ascii_digit());
            let token_shaped = tok
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/' | '='));
            if has_alpha && has_digit && token_shaped {
                return true;
            }
        }
    }
    false
}

/// A short, NON-crypto content fingerprint (FNV-1a 64-bit, lowercase hex) of the
/// proposed change bytes. A change-detection tag for the `Darwin-State-Hash`
/// trailer — NOT a security hash, and never a secret. Deterministic + pure.
pub fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// PENDINGCHANGE — one registered proposal in the review lane
// ---------------------------------------------------------------------------

/// One pending proposal in the change queue: which writer produced it, its
/// proposal `<ts>`, the CONFINED artifact locator, its provenance, a secret-free
/// summary, and (once the runner commits it) the review-branch commit sha for
/// rollback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingChange {
    /// Monotonic, queue-assigned id (stable for the life of the entry).
    pub seq: u64,
    /// Which propose-only writer produced this.
    pub kind: ChangeKind,
    /// The proposal `<ts>` (the dir name under the kind's proposal store).
    pub ts: u64,
    /// The CONFINED artifact locator, repo-root-relative under `state/`. Derived
    /// from `(kind, ts)`, so it can only ever be a proposal-store path.
    pub artifact_rel: String,
    /// The honest, secret-free provenance.
    pub provenance: Provenance,
    /// A compact, SECRET-FREE one-line summary the producer chose (bounded).
    pub summary: String,
    /// The review-branch commit sha once the runner mirrored this artifact onto
    /// [`BRANCH`], else `None` (in-memory only / no repo). Drives `git revert`.
    pub commit: Option<String>,
}

impl PendingChange {
    /// The EXISTING apply command a human runs for this proposal (the type's own
    /// gated re-validation script + the `<ts>`). NOT a new authority.
    pub fn apply_command(&self) -> String {
        self.kind.apply_command(self.ts)
    }

    /// The secret-free list-frame JSON for one pending change.
    fn to_frame(&self) -> Value {
        json!({
            "seq": self.seq,
            "kind": self.kind.as_str(),
            "ts": self.ts,
            "artifact": self.artifact_rel,
            "summary": self.summary,
            "apply_command": self.apply_command(),
            "committed": self.commit.is_some(),
            "commit": self.commit,
            "provenance": self.provenance.to_frame(),
        })
    }
}

/// An artifact locator is CONFINED when it is a RELATIVE path whose first
/// component is `state`, with NO `..` component and NO root/prefix — so a commit
/// can only ever mirror a proposal-store path, never climb out of the repo or
/// reach an absolute path. Mirrors the confinement discipline in
/// [`crate::realm`]/[`crate::code`].
pub fn is_confined_artifact(rel: &str) -> bool {
    if rel.is_empty() || rel.len() > MAX_ARTIFACT_REL_LEN {
        return false;
    }
    let p = Path::new(rel);
    if !p.is_relative() {
        return false;
    }
    let mut comps = p.components();
    if comps.next() != Some(Component::Normal(OsStr::new("state"))) {
        return false; // must live under state/
    }
    // No `..` (a climb) and no absolute/prefix component anywhere.
    p.components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

// ---------------------------------------------------------------------------
// CHANGEQUEUE — the bounded, in-memory review ledger
// ---------------------------------------------------------------------------

/// A bounded in-memory ledger of pending proposals. Oldest at the FRONT, newest at
/// the BACK; past `bound` the oldest is evicted. Owns its own monotonic seq so a
/// fresh queue is deterministic. `enabled` mirrors `[changeq].enabled`: when off,
/// [`ChangeQueue::add`] is a no-op — the master gate.
#[derive(Debug)]
pub struct ChangeQueue {
    enabled: bool,
    bound: usize,
    next_seq: u64,
    items: VecDeque<PendingChange>,
}

impl ChangeQueue {
    /// The const initializer for the process-global static (armed by default).
    const fn const_new() -> Self {
        ChangeQueue {
            enabled: true,
            bound: DEFAULT_QUEUE_BOUND,
            next_seq: 1,
            items: VecDeque::new(),
        }
    }

    /// A fresh queue with an explicit bound (clamped to `[1, MAX_QUEUE_BOUND]`) —
    /// used by tests to exercise the logic deterministically (the live process uses
    /// the process-global static + [`configure`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(bound: usize) -> Self {
        ChangeQueue {
            enabled: true,
            bound: clamp_bound(bound),
            next_seq: 1,
            items: VecDeque::new(),
        }
    }

    /// Apply live config: the master gate + the retention bound. Shrinking the
    /// bound evicts the oldest over the new cap immediately.
    pub fn configure(&mut self, enabled: bool, bound: usize) {
        self.enabled = enabled;
        self.bound = clamp_bound(bound);
        self.evict_to_bound();
    }

    /// Register a produced proposal. Derives + CONFINES the artifact locator from
    /// `(kind, ts)`, dedups by `(kind, ts)` (a re-registered proposal REPLACES its
    /// prior entry rather than doubling it), stamps a fresh seq, and evicts the
    /// oldest beyond `bound`. Returns the assigned seq, or `None` when the queue is
    /// DISABLED, the provenance is not secret-free, or the derived locator is not
    /// confined (belt-and-suspenders — the derivation is confined by construction).
    pub fn add(
        &mut self,
        kind: ChangeKind,
        ts: u64,
        provenance: Provenance,
        summary: impl Into<String>,
    ) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        if !provenance.is_secret_free() {
            return None; // never mirror a secret-shaped trailer
        }
        let artifact_rel = kind.artifact_rel(ts);
        if !is_confined_artifact(&artifact_rel) {
            return None; // unreachable by construction; refuse rather than trust
        }
        // Dedup: a proposal is keyed by (kind, ts). Re-registering replaces the
        // prior entry (e.g. once the commit sha is known) instead of doubling it.
        self.items.retain(|c| !(c.kind == kind && c.ts == ts));
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.items.push_back(PendingChange {
            seq,
            kind,
            ts,
            artifact_rel,
            provenance,
            summary: clamp(summary.into().trim(), MAX_SUMMARY_LEN),
            commit: None,
        });
        self.evict_to_bound();
        Some(seq)
    }

    /// Record the review-branch commit sha for the pending change with `seq`
    /// (after the runner mirrors it onto [`BRANCH`]). No-op (false) if that entry
    /// was evicted. Returns whether it was updated.
    pub fn mark_committed(&mut self, seq: u64, commit: impl Into<String>) -> bool {
        let commit = sanitize_trailer_value(&commit.into());
        if let Some(c) = self.items.iter_mut().find(|c| c.seq == seq) {
            c.commit = Some(commit);
            true
        } else {
            false
        }
    }

    /// A snapshot of the pending proposals, OLDEST first (insertion order).
    pub fn list(&self) -> Vec<PendingChange> {
        self.items.iter().cloned().collect()
    }

    /// Get a pending change by its queue seq. (Inspection API — exercised by the
    /// tests; the live apply path resolves by `(kind, ts)` via [`Self::find`].)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, seq: u64) -> Option<&PendingChange> {
        self.items.iter().find(|c| c.seq == seq)
    }

    /// Find a pending change by `(kind, ts)` — how a `changeq_apply` selector
    /// naming a kind + ts resolves.
    pub fn find(&self, kind: ChangeKind, ts: u64) -> Option<&PendingChange> {
        self.items.iter().find(|c| c.kind == kind && c.ts == ts)
    }

    /// Number of pending proposals currently retained. (Inspection API — exercised
    /// by the tests + available to the mirror task.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when nothing is pending.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether registration is currently armed. Public accessor for the integration
    /// seam (the live gate reads `self.enabled` directly); allowed dead in all builds.
    #[allow(dead_code)]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn evict_to_bound(&mut self) {
        while self.items.len() > self.bound {
            self.items.pop_front();
        }
    }
}

/// Clamp a requested bound into `[1, MAX_QUEUE_BOUND]`.
fn clamp_bound(bound: usize) -> usize {
    bound.clamp(1, MAX_QUEUE_BOUND)
}

/// Trim + truncate a string to a byte bound WITHOUT splitting a UTF-8 char.
fn clamp(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---------------------------------------------------------------------------
// GIT-COMMAND CONSTRUCTION — pure, fixed-arg, confined
// ---------------------------------------------------------------------------

/// One FIXED-ARG git invocation: `program` is always [`GIT`]; `args` is a fixed
/// argv (no shell, no interpolation into a shell string). The runner spawns it
/// directly. Mirrors [`crate::realm`]'s `run_fixed` argv discipline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommand {
    args: Vec<OsString>,
}

impl GitCommand {
    fn new<I, S>(args: I) -> GitCommand
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        GitCommand {
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The fixed argv the runner passes to git (after any `-C <repo>` the runner
    /// injects). The program is ALWAYS [`GIT`], never anything user-supplied.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// The argv as lossy strings (for tests + display).
    pub fn arg_strs(&self) -> Vec<String> {
        self.args.iter().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    /// The git SUBCOMMAND (the first arg after any `-C <repo>` the runner injects;
    /// here the args never include `-C`, so it is simply `args[0]`).
    fn subcommand(&self) -> Option<&str> {
        self.args.first().and_then(|a| a.to_str())
    }
}

/// `git rev-parse --verify --quiet <BRANCH_REF>` — read the branch tip sha (empty
/// output + non-zero exit when the branch does not exist yet). Read-only.
pub fn rev_parse_branch_command() -> GitCommand {
    GitCommand::new(["rev-parse", "--verify", "--quiet", BRANCH_REF])
}

/// `git read-tree --empty` — start the (separate) index empty, for the FIRST
/// commit onto a not-yet-existing review branch.
pub fn read_tree_empty_command() -> GitCommand {
    GitCommand::new(["read-tree", "--empty"])
}

/// `git read-tree <treeish>` — seed the separate index from the branch tip so a
/// new commit ACCUMULATES onto the branch rather than replacing its tree.
pub fn read_tree_command(treeish: &str) -> GitCommand {
    GitCommand::new(["read-tree", treeish])
}

/// `git hash-object -w -- <abs>` — write `abs`'s bytes into the object store,
/// returning the blob sha. `--` guards a path that might start with `-`.
pub fn hash_object_command(abs: &Path) -> GitCommand {
    let mut args: Vec<OsString> = vec!["hash-object".into(), "-w".into(), "--".into()];
    args.push(abs.as_os_str().to_os_string());
    GitCommand { args }
}

/// `git update-index --add --cacheinfo 100644,<blob>,<rel>` — stage the blob at
/// the confined `rel` path in the separate index. Regular-file mode (100644).
pub fn update_index_command(blob: &str, rel: &str) -> GitCommand {
    GitCommand::new([
        "update-index".to_string(),
        "--add".to_string(),
        "--cacheinfo".to_string(),
        format!("100644,{blob},{rel}"),
    ])
}

/// `git write-tree` — write the separate index out as a tree object, returning its
/// sha.
pub fn write_tree_command() -> GitCommand {
    GitCommand::new(["write-tree"])
}

/// `git commit-tree <tree> [-p <parent>] -m <message>` — create the commit object
/// carrying `message` (subject + the provenance trailer block). No parent for the
/// first commit. Does NOT move any ref (that is `update-ref`) and NEVER touches
/// the working tree or HEAD.
pub fn commit_tree_command(tree: &str, parent: Option<&str>, message: &str) -> GitCommand {
    let mut args: Vec<OsString> = vec!["commit-tree".into(), tree.into()];
    if let Some(p) = parent {
        args.push("-p".into());
        args.push(p.into());
    }
    args.push("-m".into());
    args.push(message.into());
    GitCommand { args }
}

/// `git update-ref <BRANCH_REF> <new> [<expected_old>]` — advance the review
/// branch to `new`. When `expected_old` is given it is a compare-and-swap (the
/// update fails if the ref moved), so a concurrent write can never be clobbered.
pub fn update_ref_command(new: &str, expected_old: Option<&str>) -> GitCommand {
    let mut args: Vec<OsString> = vec!["update-ref".into(), BRANCH_REF.into(), new.into()];
    if let Some(old) = expected_old {
        args.push(old.into());
    }
    GitCommand { args }
}

/// `git revert --no-edit <commit>` — the SAFE rollback: it creates a NEW commit
/// that inverts `commit` on the review branch (history-preserving), never a
/// destructive reset. This is how a mirrored proposal is rolled back.
pub fn revert_command(commit: &str) -> GitCommand {
    GitCommand::new(["revert", "--no-edit", commit])
}

/// The commit message for a mirrored artifact: a subject line, a blank line, and
/// the four provenance trailers as the final paragraph (git's trailer format).
/// Secret-free — the trailers are sanitized at [`Provenance::new`].
pub fn commit_message(change_kind: ChangeKind, ts: u64, provenance: &Provenance) -> String {
    format!(
        "changeq: {} proposal {}\n\n{}",
        change_kind.as_str(),
        ts,
        provenance.trailers_block()
    )
}

// ---------------------------------------------------------------------------
// GIT RUNNER — the device-gated exec seam (mock in tests, real on-device)
// ---------------------------------------------------------------------------

/// The outcome of running ONE git command through the runner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitRunOutcome {
    /// The command ran: `ok` is a zero exit; `stdout` is its trimmed stdout (a
    /// sha, a tree id, or empty).
    Ran { ok: bool, stdout: String },
    /// The command could not run (git missing, spawn failed, timed out). Says
    /// nothing about the branch — the caller aborts the commit honestly.
    Unavailable { reason: String },
}

/// A `Send` future returned by the runner. Spelled out so the trait stays
/// object-safe (`&dyn GitRunner`) WITHOUT the async-trait crate (the daemon's
/// "no new deps" rule), exactly like [`crate::heal::HealBrain`].
pub type GitRunFuture<'a> = Pin<Box<dyn Future<Output = GitRunOutcome> + Send + 'a>>;

/// The git EXEC seam — the ONLY route to a real git subprocess. Production uses
/// [`RealGitRunner`]; unit tests inject a mock so NO git process is ever spawned
/// under `cargo test`. Mirrors [`crate::heal::HealBrain`] / the realm runner.
pub trait GitRunner: Send + Sync {
    /// Run `cmd` (a fixed-arg git command) with cwd = `repo`, optionally using
    /// `index_file` as `GIT_INDEX_FILE` (so the working tree's index is NEVER
    /// touched). Returns the captured outcome.
    fn run<'a>(
        &'a self,
        repo: &'a Path,
        index_file: Option<&'a Path>,
        cmd: &'a GitCommand,
    ) -> GitRunFuture<'a>;
}

/// Orchestrate the full plumbing to mirror ONE artifact (its `files`) onto
/// [`BRANCH`] with the provenance trailers — WITHOUT touching the working tree or
/// HEAD. Sequence: read the branch tip (rev-parse) -> seed a SEPARATE index
/// (read-tree of the tip, or --empty) -> for each file: hash-object + update-index
/// -> write-tree -> commit-tree (with the parent + trailer message) -> update-ref
/// (CAS on the old tip). Returns the new commit sha.
///
/// PURE ORCHESTRATION over the [`GitRunner`] seam: every subprocess goes through
/// the runner (a mock in tests, [`RealGitRunner`] on-device), and the sequence
/// uses ONLY plumbing — it NEVER runs a working-tree-mutating porcelain command
/// (checkout/reset/add/commit/merge). `files` is `(confined_rel, abs_on_disk)`.
pub async fn commit_change(
    runner: &dyn GitRunner,
    repo: &Path,
    index_file: &Path,
    kind: ChangeKind,
    ts: u64,
    provenance: &Provenance,
    files: &[(String, PathBuf)],
) -> Result<String, String> {
    if !provenance.is_secret_free() {
        return Err("provenance is not secret-free; refusing to commit".to_string());
    }
    if files.is_empty() {
        return Err("no artifact files to mirror".to_string());
    }
    for (rel, _) in files {
        if !is_confined_artifact(rel) {
            return Err(format!("artifact path not confined under state/: {rel}"));
        }
    }

    // (1) The current branch tip (parent), if the branch exists.
    let parent = match run_ok(runner, repo, None, &rev_parse_branch_command()).await {
        RunStep::Ok(sha) if !sha.is_empty() => Some(sha),
        RunStep::Ok(_) => None,      // branch not created yet
        RunStep::Err(e) => return Err(e),
    };

    // (2) Seed the SEPARATE index (never the working-tree index) from the tip.
    let seed = match &parent {
        Some(p) => read_tree_command(p),
        None => read_tree_empty_command(),
    };
    if let RunStep::Err(e) = run_ok(runner, repo, Some(index_file), &seed).await {
        return Err(e);
    }

    // (3) hash-object + update-index for each artifact file.
    for (rel, abs) in files {
        let blob = match run_ok(runner, repo, Some(index_file), &hash_object_command(abs)).await {
            RunStep::Ok(sha) if !sha.is_empty() => sha,
            RunStep::Ok(_) => return Err(format!("empty blob sha for {rel}")),
            RunStep::Err(e) => return Err(e),
        };
        if let RunStep::Err(e) =
            run_ok(runner, repo, Some(index_file), &update_index_command(&blob, rel)).await
        {
            return Err(e);
        }
    }

    // (4) write-tree.
    let tree = match run_ok(runner, repo, Some(index_file), &write_tree_command()).await {
        RunStep::Ok(sha) if !sha.is_empty() => sha,
        RunStep::Ok(_) => return Err("write-tree returned no tree".to_string()),
        RunStep::Err(e) => return Err(e),
    };

    // (5) commit-tree with the parent + the provenance-trailer message.
    let message = commit_message(kind, ts, provenance);
    let commit_cmd = commit_tree_command(&tree, parent.as_deref(), &message);
    let commit = match run_ok(runner, repo, None, &commit_cmd).await {
        RunStep::Ok(sha) if !sha.is_empty() => sha,
        RunStep::Ok(_) => return Err("commit-tree returned no commit".to_string()),
        RunStep::Err(e) => return Err(e),
    };

    // (6) update-ref (compare-and-swap on the old tip so a race never clobbers).
    let update = update_ref_command(&commit, parent.as_deref());
    if let RunStep::Err(e) = run_ok(runner, repo, None, &update).await {
        return Err(e);
    }
    Ok(commit)
}

/// The result of one runner step, normalized: Ok(trimmed stdout) on a zero exit,
/// Err(reason) on a non-zero exit or an unavailable runner.
enum RunStep {
    Ok(String),
    Err(String),
}

/// Run one command and normalize its outcome. A non-zero exit is an error EXCEPT
/// for `rev-parse --verify --quiet`, whose non-zero+empty result is the honest
/// "branch does not exist yet" signal the caller handles.
async fn run_ok(
    runner: &dyn GitRunner,
    repo: &Path,
    index_file: Option<&Path>,
    cmd: &GitCommand,
) -> RunStep {
    match runner.run(repo, index_file, cmd).await {
        GitRunOutcome::Ran { ok: true, stdout } => RunStep::Ok(stdout.trim().to_string()),
        GitRunOutcome::Ran { ok: false, stdout } => {
            // rev-parse --verify --quiet exits non-zero with empty output when the
            // ref is absent — that is a valid "no branch yet", not an error.
            if cmd.subcommand() == Some("rev-parse") && stdout.trim().is_empty() {
                RunStep::Ok(String::new())
            } else {
                RunStep::Err(format!("git {} exited non-zero", cmd.subcommand().unwrap_or("?")))
            }
        }
        GitRunOutcome::Unavailable { reason } => RunStep::Err(reason),
    }
}

/// The device-gated production runner: a real, bounded, fixed-arg `git`
/// subprocess. BUILT, NEVER invoked under `cargo test` — the orchestration,
/// confinement, and message construction are proven hermetically with a mock
/// runner; this actuator only ever runs on-device (mirrors
/// [`crate::realm::SandboxedRealmRunner`] / the heal drill). It runs git with
/// `-C <repo>`, an optional `GIT_INDEX_FILE`, stdin closed, output captured,
/// killed on drop, under a wall-clock timeout.
pub struct RealGitRunner;

impl GitRunner for RealGitRunner {
    fn run<'a>(
        &'a self,
        repo: &'a Path,
        index_file: Option<&'a Path>,
        cmd: &'a GitCommand,
    ) -> GitRunFuture<'a> {
        Box::pin(async move {
            use tokio::process::Command;
            let mut c = Command::new(GIT);
            c.arg("-C").arg(repo);
            c.args(cmd.args());
            if let Some(idx) = index_file {
                c.env("GIT_INDEX_FILE", idx);
            }
            c.stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let child = match c.spawn() {
                Ok(child) => child,
                Err(e) => return GitRunOutcome::Unavailable { reason: format!("git spawn failed: {e}") },
            };
            match tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output()).await {
                Ok(Ok(out)) => GitRunOutcome::Ran {
                    ok: out.status.success(),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                },
                Ok(Err(e)) => GitRunOutcome::Unavailable { reason: format!("git wait failed: {e}") },
                Err(_) => GitRunOutcome::Unavailable { reason: "git timed out".to_string() },
            }
        })
    }
}

/// Wall-clock ceiling for one review-branch git subprocess (plumbing is fast).
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// ---------------------------------------------------------------------------
// PROCESS-GLOBAL QUEUE — the one live review lane the writers + list share
// ---------------------------------------------------------------------------

/// The one live change queue, mirroring [`crate::artifact`]'s static-registry
/// pattern. Armed by default; reconfigured by [`configure`] at startup.
static QUEUE: RwLock<ChangeQueue> = RwLock::new(ChangeQueue::const_new());

/// Apply live `[changeq]` config to the process-global queue (called once at
/// startup, next to `artifact::configure`).
pub fn configure(enabled: bool, bound: usize) {
    if let Ok(mut q) = QUEUE.write() {
        q.configure(enabled, bound);
    }
}

/// PRODUCER ENTRYPOINT — a propose-only writer calls this when it produces a
/// proposal. PURE BOOKKEEPING: it registers the artifact into the bounded queue
/// and emits the read-only `changeq.list` frame. It does NOT run git (the git
/// exec is the device-gated runner) and it changes NOTHING about the writer's
/// propose-only contract. Returns the assigned seq (or `None` when the queue is
/// disabled / the provenance is not secret-free / the lock is poisoned) —
/// fire-and-forget for the producer.
pub fn on_proposal(
    kind: ChangeKind,
    ts: u64,
    provenance: Provenance,
    summary: impl Into<String>,
) -> Option<u64> {
    let seq = QUEUE.write().ok().and_then(|mut q| q.add(kind, ts, provenance, summary))?;
    emit_list();
    Some(seq)
}

/// Record the review-branch commit sha for a pending change (after the device-
/// gated runner mirrors it) and re-emit the list so the HUD shows it committed.
pub fn mark_committed(seq: u64, commit: impl Into<String>) -> bool {
    let updated = QUEUE.write().ok().map(|mut q| q.mark_committed(seq, commit)).unwrap_or(false);
    if updated {
        emit_list();
    }
    updated
}

/// A snapshot of the pending proposals (oldest-first). The read-only
/// `changeq_list` surface's read path.
pub fn list() -> Vec<PendingChange> {
    QUEUE.read().ok().map(|q| q.list()).unwrap_or_default()
}

/// Resolve a `changeq_apply` selector — a `(kind, ts)` — to its pending change
/// (an owned clone so the lock is not held across the caller's work). `None` when
/// no such proposal is queued (the caller then says so honestly, never invents an
/// apply route).
pub fn find(kind: ChangeKind, ts: u64) -> Option<PendingChange> {
    QUEUE.read().ok().and_then(|q| q.find(kind, ts).cloned())
}

/// Emit the current pending set as the `changeq.list` telemetry frame the HUD's
/// Change Queue panel renders. Secret-free by construction (only kinds, ts,
/// confined locators, summaries, apply commands, and sanitized provenance ride
/// the wire). Fire-and-forget over the existing hub.
pub fn emit_list() {
    let items = list();
    telemetry::emit("system", LIST_EVENT, list_frame(&items));
}

/// Build the secret-free `changeq.list` frame from a pending set. Pure +
/// unit-testable.
fn list_frame(items: &[PendingChange]) -> Value {
    json!({
        "branch": BRANCH,
        "count": items.len(),
        "pending": items.iter().map(PendingChange::to_frame).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// MIRROR TASK — the device-gated live site that runs the git RUNNER on-device
// (built here, NEVER invoked under `cargo test`; the orchestration is proven
// hermetically with the mock runner, exactly like crate::realm's runner).
// ---------------------------------------------------------------------------

/// How often the mirror task sweeps the queue for uncommitted proposals.
const MIRROR_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Cap on files mirrored per proposal (a forge app has a small tree; a heal/code
/// proposal has a handful). Bounds one commit's plumbing on the always-on box.
const MAX_MIRROR_FILES: usize = 64;

/// The DEVICE-GATED live task: periodically mirror any not-yet-committed pending
/// proposal onto the [`BRANCH`] review branch via the real git runner. INERT
/// WITHOUT A REPO — it no-ops until `<root>/.git` exists, and every git call is
/// bounded + fixed-arg. Spawned once at startup (main.rs); NEVER invoked under
/// `cargo test` (the commit orchestration is proven with the mock runner).
pub async fn mirror_task(root: PathBuf) {
    let runner = RealGitRunner;
    let mut interval = tokio::time::interval(MIRROR_INTERVAL);
    loop {
        interval.tick().await;
        if !root.join(".git").exists() {
            continue; // INERT WITHOUT A REPO — never fabricate a branch we can't make
        }
        mirror_pending_once(&runner, &root).await;
    }
}

/// One sweep: mirror every uncommitted pending proposal. Each proposal's artifact
/// files are read from its confined on-disk store and committed onto the review
/// branch; on success the queue entry is stamped with the commit sha. A failure
/// (no repo, git missing) is a warned no-op — the proposal stays queued + retries.
async fn mirror_pending_once(runner: &dyn GitRunner, root: &Path) {
    let uncommitted: Vec<PendingChange> =
        list().into_iter().filter(|c| c.commit.is_none()).collect();
    if uncommitted.is_empty() {
        return;
    }
    let index_file = root.join(".git").join("darwin-changeq.index");
    for change in uncommitted {
        let dir = root.join(&change.artifact_rel);
        let files = collect_artifact_files(&dir, &change.artifact_rel);
        if files.is_empty() {
            continue; // nothing on disk yet (or the dir vanished) — try again later
        }
        let _ = std::fs::remove_file(&index_file); // a fresh SEPARATE index per commit
        match commit_change(
            runner,
            root,
            &index_file,
            change.kind,
            change.ts,
            &change.provenance,
            &files,
        )
        .await
        {
            Ok(sha) => {
                mark_committed(change.seq, sha);
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    kind = change.kind.as_str(),
                    ts = change.ts,
                    "changeq: could not mirror the proposal onto the review branch (will retry)"
                );
            }
        }
        let _ = std::fs::remove_file(&index_file);
    }
}

/// Collect the REGULAR files under a proposal dir as `(confined_rel, abs)`,
/// recursively, bounded by [`MAX_MIRROR_FILES`]. Symlinks are SKIPPED (never
/// followed) so a crafted link inside the store cannot escape confinement, and
/// every produced `rel` is re-checked with [`is_confined_artifact`].
fn collect_artifact_files(dir: &Path, rel_base: &str) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    collect_rec(dir, rel_base, &mut out);
    out.truncate(MAX_MIRROR_FILES);
    out
}

fn collect_rec(dir: &Path, rel_base: &str, out: &mut Vec<(String, PathBuf)>) {
    if out.len() >= MAX_MIRROR_FILES {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= MAX_MIRROR_FILES {
            return;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Never follow a symlink (symlink_metadata does not traverse it).
        let Ok(meta) = entry.path().symlink_metadata() else { continue };
        if meta.file_type().is_symlink() {
            continue;
        }
        let child_rel = format!("{rel_base}/{name}");
        if meta.is_dir() {
            collect_rec(&entry.path(), &child_rel, out);
        } else if meta.is_file() && is_confined_artifact(&child_rel) {
            out.push((child_rel, entry.path()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prov() -> Provenance {
        Provenance::new("steve", "claude-opus-4-8", "1700", "a1b2c3d4e5f60718")
    }

    // =====================================================================
    // KIND vocabulary + the store/apply mappings (never a new authority)
    // =====================================================================

    #[test]
    fn kind_maps_to_its_wire_string_store_and_existing_apply_script() {
        for (k, wire, store, script) in [
            (ChangeKind::Heal, "heal", "state/heal/proposals", "scripts/apply_heal.sh"),
            (ChangeKind::Code, "code", "state/code/proposals", "scripts/apply_code_diff.sh"),
            (ChangeKind::Forge, "forge", "state/forge/proposals", "scripts/apply_forge.sh"),
            (ChangeKind::Optimize, "optimize", "state/optimize/proposals", "scripts/apply_optimization.sh"),
        ] {
            assert_eq!(k.as_str(), wire);
            assert_eq!(ChangeKind::from_wire(wire), Some(k));
            assert_eq!(k.proposals_subdir(), store);
            assert_eq!(k.apply_script(), script);
            // The apply command is the EXISTING script + the ts — a new authority
            // is NEVER invented.
            assert_eq!(k.apply_command(42), format!("{script} 42"));
        }
        assert_eq!(ChangeKind::from_wire("bogus"), None, "unknown kind is never guessed");
    }

    #[test]
    fn apply_routes_only_to_the_four_pre_existing_scripts() {
        // The whole apply surface: every kind's apply command must be one of the
        // four EXISTING human-gated scripts — the change queue invents no new one.
        const EXISTING: &[&str] = &[
            "scripts/apply_heal.sh",
            "scripts/apply_code_diff.sh",
            "scripts/apply_forge.sh",
            "scripts/apply_optimization.sh",
        ];
        for k in [ChangeKind::Heal, ChangeKind::Code, ChangeKind::Forge, ChangeKind::Optimize] {
            let cmd = k.apply_command(7);
            assert!(
                EXISTING.iter().any(|s| cmd.starts_with(s)),
                "apply must route to an EXISTING script, got {cmd}"
            );
            // It NEVER names a changeq-specific apply binary/script.
            assert!(!cmd.contains("changeq"), "apply must not invent a changeq authority: {cmd}");
        }
    }

    #[test]
    fn artifact_rel_is_the_confined_proposal_store_path() {
        assert_eq!(ChangeKind::Code.artifact_rel(1700), "state/code/proposals/1700");
        for k in [ChangeKind::Heal, ChangeKind::Code, ChangeKind::Forge, ChangeKind::Optimize] {
            assert!(is_confined_artifact(&k.artifact_rel(1700)), "derived path must be confined");
        }
    }

    // =====================================================================
    // PROVENANCE — all four trailers, secret-free, format-safe
    // =====================================================================

    #[test]
    fn provenance_builds_exactly_the_four_trailers_in_order() {
        let lines = prov().trailer_lines();
        assert_eq!(lines.len(), 4, "exactly four trailers");
        assert_eq!(lines[0], "Darwin-Agent: steve");
        assert_eq!(lines[1], "Darwin-Model: claude-opus-4-8");
        assert_eq!(lines[2], "Darwin-Run: 1700");
        assert_eq!(lines[3], "Darwin-State-Hash: a1b2c3d4e5f60718");
        // The block is the four lines joined — the commit message's last paragraph.
        assert_eq!(prov().trailers_block(), lines.join("\n"));
    }

    #[test]
    fn unknown_trailer_fields_are_present_as_honest_placeholders() {
        // An empty field is still emitted (all four trailers ALWAYS present) as the
        // honest "unknown" — never dropped, never fabricated.
        let p = Provenance::new("", "  ", "\n\t", "");
        for line in p.trailer_lines() {
            assert!(line.ends_with(": unknown"), "empty field -> honest unknown: {line}");
        }
        assert_eq!(p.trailer_lines().len(), 4);
    }

    #[test]
    fn trailer_values_are_single_line_so_they_cannot_inject_trailers() {
        // A value carrying newlines (a forged-trailer attempt) is collapsed to one
        // line, so it can never split into extra trailers.
        let p = Provenance::new(
            "steve\nDarwin-Model: forged",
            "opus",
            "1700",
            "hash",
        );
        let lines = p.trailer_lines();
        assert_eq!(lines[0], "Darwin-Agent: steve Darwin-Model: forged", "newline collapsed");
        // Still exactly four trailers — no injected fifth.
        assert_eq!(lines.len(), 4);
        assert!(p.trailers_block().lines().count() == 4, "no injected trailer line");
    }

    #[test]
    fn provenance_is_secret_free_and_a_secret_shaped_value_is_flagged() {
        assert!(prov().is_secret_free(), "ordinary agent/model/run/hash is secret-free");
        // A secret-shaped model/agent value is flagged (defense in depth).
        assert!(!Provenance::new("steve", "sk-ABCDEF0123456789", "1700", "h").is_secret_free());
        assert!(!Provenance::new("token=deadbeefcafe", "opus", "1700", "h").is_secret_free());
        assert!(
            !Provenance::new("steve", "opus", "ghp_0123456789abcdef0123", "h").is_secret_free(),
            "a github PAT-shaped run id is flagged"
        );
        // A long non-secret model id (all letters/dashes, no digits) is fine.
        assert!(Provenance::new("steve", "claude-opus-4-8", "1700", "abcdef0123456789").is_secret_free());
    }

    #[test]
    fn fingerprint_is_deterministic_and_change_sensitive() {
        assert_eq!(fingerprint(b"same"), fingerprint(b"same"), "deterministic");
        assert_ne!(fingerprint(b"a"), fingerprint(b"b"), "different bytes -> different tag");
        assert_eq!(fingerprint(b"x").len(), 16, "16 hex chars");
        assert!(fingerprint(b"x").chars().all(|c| c.is_ascii_hexdigit()));
    }

    // =====================================================================
    // CONFINEMENT — only state/ paths, never absolute, never a `..` climb
    // =====================================================================

    #[test]
    fn confinement_accepts_state_paths_and_refuses_escapes() {
        assert!(is_confined_artifact("state/code/proposals/1700"));
        assert!(is_confined_artifact("state/heal/proposals/9"));
        // Refusals: absolute, `..` climb, wrong root, empty, over-long.
        assert!(!is_confined_artifact("/etc/passwd"), "absolute refused");
        assert!(!is_confined_artifact("state/../../etc/x"), "`..` climb refused");
        assert!(!is_confined_artifact("../state/x"), "leading `..` refused");
        assert!(!is_confined_artifact("daemon/src/x"), "non-state root refused");
        assert!(!is_confined_artifact(""), "empty refused");
        assert!(!is_confined_artifact(&format!("state/{}", "a".repeat(500))), "over-long refused");
    }

    // =====================================================================
    // QUEUE model — add / list / dedup / bounded eviction / disabled
    // =====================================================================

    #[test]
    fn add_assigns_monotonic_seqs_and_list_is_oldest_first() {
        let mut q = ChangeQueue::new(8);
        assert!(q.is_empty());
        let s1 = q.add(ChangeKind::Code, 100, prov(), "diff: +1/-0").unwrap();
        let s2 = q.add(ChangeKind::Heal, 200, prov(), "patch").unwrap();
        assert!(s2 > s1, "monotonic seqs");
        assert_eq!(q.len(), 2);
        let items = q.list();
        assert_eq!(items[0].ts, 100, "oldest first");
        assert_eq!(items[1].ts, 200);
        // Derived, confined artifact path + the existing apply command.
        assert_eq!(items[0].artifact_rel, "state/code/proposals/100");
        assert_eq!(items[0].apply_command(), "scripts/apply_code_diff.sh 100");
        assert_eq!(q.get(s2).unwrap().kind, ChangeKind::Heal);
        assert_eq!(q.find(ChangeKind::Code, 100).unwrap().seq, s1);
        assert!(q.find(ChangeKind::Code, 999).is_none());
    }

    #[test]
    fn re_registering_the_same_kind_ts_replaces_not_doubles() {
        let mut q = ChangeQueue::new(8);
        q.add(ChangeKind::Code, 100, prov(), "first").unwrap();
        q.add(ChangeKind::Code, 100, prov(), "second").unwrap();
        assert_eq!(q.len(), 1, "same (kind, ts) is one entry, not two");
        assert_eq!(q.list()[0].summary, "second", "the re-registration wins");
    }

    #[test]
    fn add_evicts_oldest_beyond_the_bound() {
        let mut q = ChangeQueue::new(3);
        let mut seqs = Vec::new();
        for ts in 0..5u64 {
            seqs.push(q.add(ChangeKind::Code, ts, prov(), "d").unwrap());
        }
        assert_eq!(q.len(), 3, "bounded to 3");
        assert!(q.get(seqs[0]).is_none(), "oldest evicted");
        assert!(q.get(seqs[4]).is_some(), "newest kept");
        assert_eq!(q.list()[0].ts, 2, "the three newest survive");
    }

    #[test]
    fn configure_shrinking_evicts_and_disabled_registers_nothing() {
        let mut q = ChangeQueue::new(10);
        for ts in 0..6u64 {
            q.add(ChangeKind::Code, ts, prov(), "d").unwrap();
        }
        q.configure(true, 2);
        assert_eq!(q.len(), 2, "shrinking the bound evicts immediately");
        // Disabled -> add is a no-op.
        q.configure(false, 10);
        assert!(q.add(ChangeKind::Heal, 99, prov(), "x").is_none(), "disabled -> no seq");
    }

    #[test]
    fn add_refuses_a_secret_shaped_provenance() {
        let mut q = ChangeQueue::new(4);
        let bad = Provenance::new("steve", "sk-ABCDEF0123456789", "1700", "h");
        assert!(q.add(ChangeKind::Code, 1, bad, "d").is_none(), "secret-shaped trailer refused");
        assert!(q.is_empty(), "nothing registered");
    }

    #[test]
    fn mark_committed_sets_the_sha_or_no_ops_when_evicted() {
        let mut q = ChangeQueue::new(2);
        let s = q.add(ChangeKind::Code, 1, prov(), "d").unwrap();
        assert!(q.mark_committed(s, "abc123"), "marks the live entry");
        assert_eq!(q.get(s).unwrap().commit.as_deref(), Some("abc123"));
        assert!(!q.mark_committed(9999, "x"), "unknown seq -> no-op");
    }

    // =====================================================================
    // GIT-COMMAND CONSTRUCTION — fixed-arg, confined, correct
    // =====================================================================

    #[test]
    fn git_commands_are_fixed_arg_and_target_the_dedicated_branch() {
        // The program is ALWAYS the fixed `git` const — never anything derived.
        assert_eq!(GIT, "git");
        assert_eq!(
            rev_parse_branch_command().arg_strs(),
            vec!["rev-parse", "--verify", "--quiet", "refs/heads/darwin/changeq"]
        );
        assert_eq!(read_tree_empty_command().arg_strs(), vec!["read-tree", "--empty"]);
        assert_eq!(read_tree_command("TIP").arg_strs(), vec!["read-tree", "TIP"]);
        assert_eq!(
            update_index_command("BLOB", "state/code/proposals/1/patch.diff").arg_strs(),
            vec!["update-index", "--add", "--cacheinfo", "100644,BLOB,state/code/proposals/1/patch.diff"]
        );
        assert_eq!(write_tree_command().arg_strs(), vec!["write-tree"]);
        // update-ref targets ONLY the dedicated ref, with a CAS old-tip when given.
        assert_eq!(
            update_ref_command("NEW", Some("OLD")).arg_strs(),
            vec!["update-ref", "refs/heads/darwin/changeq", "NEW", "OLD"]
        );
        assert_eq!(
            update_ref_command("NEW", None).arg_strs(),
            vec!["update-ref", "refs/heads/darwin/changeq", "NEW"]
        );
        // The rollback surface is a SAFE, history-preserving revert (never a reset).
        assert_eq!(revert_command("DEAD").arg_strs(), vec!["revert", "--no-edit", "DEAD"]);
    }

    #[test]
    fn hash_object_and_commit_tree_are_built_correctly() {
        let h = hash_object_command(Path::new("/repo/state/code/proposals/1/patch.diff"));
        assert_eq!(h.arg_strs()[..3], ["hash-object", "-w", "--"]);
        assert_eq!(h.arg_strs()[3], "/repo/state/code/proposals/1/patch.diff");

        // First commit: no parent. Message carries subject + the four trailers.
        let msg = commit_message(ChangeKind::Code, 1700, &prov());
        let first = commit_tree_command("TREE", None, &msg);
        assert_eq!(first.arg_strs()[..2], ["commit-tree", "TREE"]);
        assert_eq!(first.arg_strs()[2], "-m");
        assert!(first.arg_strs()[3].contains("Darwin-Agent: steve"));
        assert!(!first.arg_strs().iter().any(|a| a == "-p"), "no parent on the first commit");
        // Subsequent commit: -p <parent> before -m.
        let next = commit_tree_command("TREE", Some("PARENT"), &msg);
        assert_eq!(next.arg_strs()[..4], ["commit-tree", "TREE", "-p", "PARENT"]);
    }

    #[test]
    fn commit_message_is_subject_plus_the_secret_free_trailer_paragraph() {
        let msg = commit_message(ChangeKind::Heal, 42, &prov());
        assert!(msg.starts_with("changeq: heal proposal 42\n\n"));
        for line in prov().trailer_lines() {
            assert!(msg.contains(&line), "message carries {line}");
        }
        // The trailer paragraph is the LAST paragraph (git trailer format).
        let last_para = msg.rsplit("\n\n").next().unwrap();
        assert_eq!(last_para.lines().count(), 4, "four-line trailer paragraph");
    }

    // =====================================================================
    // COMMIT ORCHESTRATION — routes ONLY through the runner, plumbing-only,
    // never a working-tree mutation (proven with a MOCK runner)
    // =====================================================================

    /// A mock GitRunner that RECORDS every command's subcommand and returns canned
    /// shas per subcommand. NEVER spawns a process — the real exec is
    /// [`RealGitRunner`], device-gated + built-not-invoked. `first_commit` toggles
    /// whether the branch already exists (rev-parse yields a tip or empty).
    struct MockGit {
        log: std::sync::Mutex<Vec<Vec<String>>>,
        branch_exists: bool,
    }
    impl MockGit {
        fn new(branch_exists: bool) -> Self {
            MockGit { log: std::sync::Mutex::new(Vec::new()), branch_exists }
        }
        fn calls(&self) -> Vec<Vec<String>> {
            self.log.lock().unwrap().clone()
        }
        fn subcommands(&self) -> Vec<String> {
            self.calls().iter().map(|a| a[0].clone()).collect()
        }
    }
    impl GitRunner for MockGit {
        fn run<'a>(
            &'a self,
            _repo: &'a Path,
            _index_file: Option<&'a Path>,
            cmd: &'a GitCommand,
        ) -> GitRunFuture<'a> {
            let args = cmd.arg_strs();
            self.log.lock().unwrap().push(args.clone());
            let sub = args[0].clone();
            let exists = self.branch_exists;
            Box::pin(async move {
                match sub.as_str() {
                    "rev-parse" => {
                        if exists {
                            GitRunOutcome::Ran { ok: true, stdout: "PARENTSHA\n".into() }
                        } else {
                            // absent ref: non-zero exit, empty stdout
                            GitRunOutcome::Ran { ok: false, stdout: String::new() }
                        }
                    }
                    "hash-object" => GitRunOutcome::Ran { ok: true, stdout: "BLOBSHA\n".into() },
                    "write-tree" => GitRunOutcome::Ran { ok: true, stdout: "TREESHA\n".into() },
                    "commit-tree" => GitRunOutcome::Ran { ok: true, stdout: "COMMITSHA\n".into() },
                    _ => GitRunOutcome::Ran { ok: true, stdout: String::new() },
                }
            })
        }
    }

    fn files() -> Vec<(String, PathBuf)> {
        vec![(
            "state/code/proposals/1700/patch.diff".to_string(),
            PathBuf::from("/repo/state/code/proposals/1700/patch.diff"),
        )]
    }

    #[tokio::test]
    async fn commit_change_runs_only_plumbing_and_never_touches_the_working_tree() {
        let runner = MockGit::new(true);
        let sha = commit_change(
            &runner,
            Path::new("/repo"),
            Path::new("/repo/.git/changeq.index"),
            ChangeKind::Code,
            1700,
            &prov(),
            &files(),
        )
        .await
        .expect("commit succeeds against a mock runner");
        assert_eq!(sha, "COMMITSHA", "returns the new commit sha");

        let subs = runner.subcommands();
        // The exact plumbing sequence, parent-seeded (branch exists).
        assert_eq!(
            subs,
            vec![
                "rev-parse",     // read the tip
                "read-tree",     // seed the SEPARATE index from the tip
                "hash-object",   // write the artifact blob
                "update-index",  // stage it in the separate index
                "write-tree",    // tree
                "commit-tree",   // commit with the trailer message
                "update-ref",    // advance the dedicated branch
            ]
        );
        // LOAD-BEARING: NO working-tree-mutating porcelain is ever run.
        for forbidden in ["checkout", "reset", "add", "commit", "merge", "switch", "stash"] {
            assert!(!subs.iter().any(|s| s == forbidden), "must never run `git {forbidden}`");
        }
        // The commit-tree carried the parent (CAS) + the trailer message.
        let commit_call = runner.calls().into_iter().find(|a| a[0] == "commit-tree").unwrap();
        assert!(commit_call.contains(&"-p".to_string()) && commit_call.contains(&"PARENTSHA".to_string()));
        assert!(commit_call.iter().any(|a| a.contains("Darwin-State-Hash")));
        // update-ref did a compare-and-swap on the old tip.
        let ref_call = runner.calls().into_iter().find(|a| a[0] == "update-ref").unwrap();
        assert_eq!(ref_call, vec!["update-ref", "refs/heads/darwin/changeq", "COMMITSHA", "PARENTSHA"]);
    }

    #[tokio::test]
    async fn commit_change_seeds_an_empty_index_for_the_first_commit() {
        let runner = MockGit::new(false); // branch does not exist yet
        commit_change(
            &runner,
            Path::new("/repo"),
            Path::new("/repo/.git/changeq.index"),
            ChangeKind::Heal,
            9,
            &prov(),
            &files(),
        )
        .await
        .expect("first commit succeeds");
        // read-tree --empty (not read-tree <tip>) and commit-tree with NO parent.
        let read_tree = runner.calls().into_iter().find(|a| a[0] == "read-tree").unwrap();
        assert_eq!(read_tree, vec!["read-tree", "--empty"], "first commit seeds an empty index");
        let commit_call = runner.calls().into_iter().find(|a| a[0] == "commit-tree").unwrap();
        assert!(!commit_call.contains(&"-p".to_string()), "no parent on the first commit");
        let ref_call = runner.calls().into_iter().find(|a| a[0] == "update-ref").unwrap();
        assert_eq!(ref_call.len(), 3, "no CAS old-tip when the branch is new");
    }

    #[tokio::test]
    async fn commit_change_refuses_an_unconfined_file_and_a_secret_provenance() {
        let runner = MockGit::new(true);
        // An unconfined artifact path never reaches git.
        let bad_files = vec![("/etc/passwd".to_string(), PathBuf::from("/etc/passwd"))];
        assert!(commit_change(&runner, Path::new("/repo"), Path::new("/i"), ChangeKind::Code, 1, &prov(), &bad_files)
            .await
            .is_err());
        assert!(runner.calls().is_empty(), "nothing ran for an unconfined path");

        // A secret-shaped provenance never reaches git.
        let secret = Provenance::new("steve", "sk-ABCDEF0123456789", "1", "h");
        assert!(commit_change(&runner, Path::new("/repo"), Path::new("/i"), ChangeKind::Code, 1, &secret, &files())
            .await
            .is_err());
        assert!(runner.calls().is_empty(), "nothing ran for a secret provenance");
    }

    // =====================================================================
    // LIST FRAME — secret-free, exactly the known keys
    // =====================================================================

    #[test]
    fn list_frame_is_secret_free_with_the_known_shape() {
        let mut q = ChangeQueue::new(4);
        let s = q.add(ChangeKind::Code, 1700, prov(), "diff: +3/-1 lines, 2 hunks").unwrap();
        q.mark_committed(s, "COMMITSHA");
        let frame = list_frame(&q.list());
        assert_eq!(frame["branch"], "darwin/changeq");
        assert_eq!(frame["count"], 1);
        let item = &frame["pending"][0];
        let obj = item.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["apply_command", "artifact", "commit", "committed", "kind", "provenance", "seq", "summary", "ts"]
        );
        assert_eq!(item["kind"], "code");
        assert_eq!(item["artifact"], "state/code/proposals/1700");
        assert_eq!(item["apply_command"], "scripts/apply_code_diff.sh 1700");
        assert_eq!(item["committed"], true);
        assert_eq!(item["commit"], "COMMITSHA");
        // Provenance carries the four secret-free fields only.
        let p = item["provenance"].as_object().unwrap();
        let mut pkeys: Vec<&str> = p.keys().map(String::as_str).collect();
        pkeys.sort_unstable();
        assert_eq!(pkeys, vec!["agent", "model", "run", "state_hash"]);
        assert_eq!(item["provenance"]["agent"], "steve");
        // No raw diff/token body anywhere on the wire.
        let wire = frame.to_string();
        assert!(!wire.contains("sk-"), "no secret on the wire");
    }

    // =====================================================================
    // PROCESS-GLOBAL surface — on_proposal registers + emits changeq.list
    // =====================================================================

    #[test]
    fn on_proposal_registers_and_emits_the_list_frame() {
        let mut rx = telemetry::subscribe_for_test();
        // A unique ts so we can find OUR entry on the shared global queue.
        let ts = 1_777_000_042;
        let seq = on_proposal(ChangeKind::Code, ts, prov(), "diff: +1/-0 lines, 1 hunk")
            .expect("armed-by-default queue accepts the register");
        // It is now listable by (kind, ts).
        let found = find(ChangeKind::Code, ts).expect("registered proposal is findable");
        assert_eq!(found.seq, seq);
        assert_eq!(found.apply_command(), format!("scripts/apply_code_diff.sh {ts}"));
        // The changeq.list frame reached the hub carrying our entry.
        let mut saw = false;
        while let Ok(raw) = rx.try_recv() {
            let env: Value = serde_json::from_str(&raw).unwrap();
            if env["event"] == LIST_EVENT {
                let pending = env["data"]["pending"].as_array().cloned().unwrap_or_default();
                if pending.iter().any(|p| p["ts"] == ts && p["kind"] == "code") {
                    saw = true;
                    break;
                }
            }
        }
        assert!(saw, "on_proposal emitted a changeq.list frame carrying the new proposal");
    }
}
