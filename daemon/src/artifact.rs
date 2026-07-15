//! ARTIFACT REGISTRY + PEEK — the FOUNDATION for "what did you just do?" (and the
//! surface Share Guard will ride).
//!
//! Every PRODUCER in DARWIN emits its result somewhere (a report's markdown, a
//! chart's `chart.data` frame, a code proposal's diff, a draft, a notebook run, a
//! forecast, a docsearch answer). Those results are ephemeral — once spoken or
//! plotted they are gone. This module keeps a small, BOUNDED, in-memory ledger of
//! the LAST N things the assistant produced: one [`ArtifactRef`] per result,
//! carrying its kind, a human title, an HONEST provenance (the REAL producing
//! agent + the REAL citations the artifact carried — an uncited artifact is shown
//! as UNCITED, never dressed up), a compact secret-free preview, and a timestamp.
//!
//! A producer calls [`register`] when it makes something; the read-only `peek`
//! surface (a voice op "what did you just do" / "peek", and a model-callable
//! `artifact_peek` tool) reads the most recent (or a specific id) back out and
//! emits it as an `artifact.peek` telemetry frame the HUD's QuickLook overlay
//! renders.
//!
//! ## Honesty contract (LOAD-BEARING — Share Guard will depend on it)
//!   * REAL PROVENANCE. The registered provenance is the REAL producing agent and
//!     the REAL citations the artifact carried. The registry NEVER synthesizes an
//!     agent or a citation.
//!   * UNCITED IS UNCITED. An artifact registered with an empty citation list is
//!     reported `uncited: true`. It is never given a fabricated source to look
//!     better-attributed than it is. (A chart of live system metrics genuinely
//!     cites nothing; a report over cited research genuinely does.)
//!   * SECRET-FREE FRAME. The `artifact.peek` frame carries only the kind, title,
//!     ts, the producer-supplied preview, the agent, and the citation LOCATORS
//!     (title + url). It never carries raw bodies or credentials — the producer is
//!     responsible for handing a redacted preview, and the strings are BOUNDED
//!     here so a runaway producer cannot blow the frame.
//!   * BOUNDED + ON-DEVICE. The registry keeps at most N entries (config bound);
//!     past N the OLDEST is evicted. Everything is in-memory and on-device — the
//!     peek surface opens NO outward network and takes NO action; it only reads
//!     back what the assistant already produced.
//!
//! Nothing here speaks, acts, or reaches the network. It remembers, and it shows.

use std::collections::VecDeque;
use std::sync::RwLock;

use chrono::Utc;
use serde_json::{json, Value};

use crate::telemetry;

/// The telemetry event the peek surface emits (one retained slot is NOT wanted —
/// a peek is a live, on-demand read, not an announce topic).
pub const PEEK_EVENT: &str = "artifact.peek";

/// Default registry bound when [`configure`] has not run yet (mirrored by
/// [`crate::config::ArtifactConfig::default`]). Small: the registry is a "what did
/// you JUST do" recency window, not a history store.
pub const DEFAULT_REGISTRY_BOUND: usize = 32;

/// Hard ceiling on the bound regardless of config — the registry is a recency
/// window, never an unbounded history. A config asking for more is clamped here.
pub const MAX_REGISTRY_BOUND: usize = 256;

/// Bounds on one registered artifact's strings so a misbehaving producer cannot
/// blow the peek frame. The producer's own outputs are already bounded; these are
/// belt-and-suspenders (and keep the frame secret-free-by-size).
const MAX_TITLE_LEN: usize = 200;
const MAX_PREVIEW_LEN: usize = 600;
const MAX_CITATIONS: usize = 32;
const MAX_CITATION_FIELD_LEN: usize = 300;

// ---------------------------------------------------------------------------
// KIND — the closed vocabulary of things a producer can register
// ---------------------------------------------------------------------------

/// The kind of thing that was produced. A closed vocabulary so the HUD can render
/// a kind-aware preview; an unrecognized wire string maps to [`ArtifactKind::Other`]
/// (shown honestly as a generic artifact, never guessed into a richer kind). The
/// representative producers wired today register `Report` / `Chart` / `CodeDiff`;
/// the rest are the intended vocabulary the remaining producers register as they
/// are wired (and the tests exercise), so they read as unused in the binary while
/// pinned by the module's contract.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactKind {
    /// A structured markdown report (report.rs).
    Report,
    /// A plotted data series (chart.rs).
    Chart,
    /// A generated image (screen_context / image path).
    Image,
    /// A drafted message/document (drafts.rs).
    Draft,
    /// A proposed code change — a reviewable unified diff (code.rs).
    CodeDiff,
    /// A research-notebook run (notebook.rs).
    Notebook,
    /// A forecast / simulation (forecast.rs / cassandra).
    Forecast,
    /// A cited answer over the on-device document index (docsearch.rs).
    DocSearch,
    /// Any other producer, carried by its own wire label — rendered as a generic
    /// artifact. NEVER upgraded to a richer kind it did not claim.
    Other(String),
}

impl ArtifactKind {
    /// The stable, lowercase wire string the HUD switches on.
    pub fn as_str(&self) -> &str {
        match self {
            ArtifactKind::Report => "report",
            ArtifactKind::Chart => "chart",
            ArtifactKind::Image => "image",
            ArtifactKind::Draft => "draft",
            ArtifactKind::CodeDiff => "code_diff",
            ArtifactKind::Notebook => "notebook",
            ArtifactKind::Forecast => "forecast",
            ArtifactKind::DocSearch => "docsearch",
            ArtifactKind::Other(label) => label,
        }
    }
}

// ---------------------------------------------------------------------------
// PROVENANCE — the honest attribution an artifact carries
// ---------------------------------------------------------------------------

/// One CITATION an artifact rests on: a human title and a real locator (a URL, a
/// file path, a byte offset string, …). At least one field is non-empty (an empty
/// citation is dropped by [`Provenance::new`]). NEVER fabricated — a citation is
/// only ever the REAL locator the producing path carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// The human-readable title/label of the source.
    pub title: String,
    /// The real locator — a URL, a file path, an id. May be empty when the source
    /// is title-only (e.g. a named document with no URL); both empty is dropped.
    pub url: String,
}

impl Citation {
    /// Build a bounded citation, trimming + clamping both fields. Returns `None`
    /// when BOTH fields are blank — there is nothing to point at, so it is dropped
    /// rather than kept as an empty (would-be-fabricated) source.
    pub fn new(title: impl Into<String>, url: impl Into<String>) -> Option<Citation> {
        let title = clamp(title.into().trim(), MAX_CITATION_FIELD_LEN);
        let url = clamp(url.into().trim(), MAX_CITATION_FIELD_LEN);
        if title.is_empty() && url.is_empty() {
            return None;
        }
        Some(Citation { title, url })
    }
}

/// The honest attribution of an artifact: the REAL producing agent, and the REAL
/// citations it carried. An empty `citations` is honestly UNCITED — never padded
/// with a fabricated source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    /// The REAL agent that produced the artifact (its name or namespace). Never
    /// invented — the caller passes the agent that actually did the work.
    pub agent: String,
    /// The REAL citations the artifact carried, bounded by [`MAX_CITATIONS`]. Empty
    /// => the artifact is UNCITED (and reported as such).
    pub citations: Vec<Citation>,
}

impl Provenance {
    /// Build a provenance from the real agent + the real citations. Blank citations
    /// are dropped ([`Citation::new`]) and the list is bounded — but a genuinely
    /// uncited artifact stays uncited (no fabricated fill).
    pub fn new(agent: impl Into<String>, citations: Vec<Citation>) -> Provenance {
        let mut cites = citations;
        cites.truncate(MAX_CITATIONS);
        Provenance {
            agent: clamp(agent.into().trim(), MAX_CITATION_FIELD_LEN),
            citations: cites,
        }
    }

    /// True when the artifact carries NO citation — reported honestly as UNCITED,
    /// never dressed up. This is the honesty pivot the peek frame turns on.
    pub fn is_uncited(&self) -> bool {
        self.citations.is_empty()
    }
}

// ---------------------------------------------------------------------------
// ARTIFACTREF — one registered thing the assistant produced
// ---------------------------------------------------------------------------

/// One registered artifact: everything the peek surface needs to show WHAT was
/// produced and WHO/WHAT backs it. `id` + `ts` are stamped by the registry at
/// [`Registry::register`]; the producer supplies the rest. Clean + documented on
/// purpose: Share Guard will read exactly this shape to decide what may be shared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRef {
    /// Monotonic, registry-assigned id (stable for the life of the entry). The peek
    /// surface addresses an artifact by this id (or asks for the most recent).
    pub id: u64,
    /// What was produced.
    pub kind: ArtifactKind,
    /// A human title for the artifact (bounded).
    pub title: String,
    /// The HONEST attribution — the real producing agent + real citations (or
    /// UNCITED).
    pub provenance: Provenance,
    /// A compact, SECRET-FREE preview the producer chose (bounded). Never a raw
    /// credential/body — the producer hands a redacted line; the HUD shows it.
    pub preview_payload: String,
    /// RFC3339 timestamp the registry stamped at registration.
    pub ts: String,
}

impl ArtifactRef {
    /// Serialize to the EXACT `artifact.peek` telemetry JSON the HUD consumes.
    /// SECRET-FREE by construction: only the id, kind, title, ts, preview, agent,
    /// the `uncited` honesty flag, the citation count, and the citation LOCATORS
    /// ride the wire. `uncited` is derived from the real citation list (never a
    /// separate wire claim), so the HUD can trust it.
    pub fn to_frame(&self) -> Value {
        json!({
            "id": self.id,
            "kind": self.kind.as_str(),
            "title": self.title,
            "ts": self.ts,
            "preview": self.preview_payload,
            "agent": self.provenance.agent,
            // The honesty pivot: an artifact with no citations is UNCITED, shown as
            // such, never dressed up with a fabricated source.
            "uncited": self.provenance.is_uncited(),
            "citation_count": self.provenance.citations.len(),
            "citations": self
                .provenance
                .citations
                .iter()
                .map(|c| json!({"title": c.title, "url": c.url}))
                .collect::<Vec<_>>(),
        })
    }

    /// A short, honest spoken/text summary for the voice op + the tool reply. Names
    /// the kind, the title, and the attribution — and says UNCITED plainly when the
    /// artifact carries no source (never implies one it lacks).
    pub fn summary(&self) -> String {
        let attribution = if self.provenance.is_uncited() {
            "uncited".to_string()
        } else {
            let n = self.provenance.citations.len();
            format!("{n} citation{}", if n == 1 { "" } else { "s" })
        };
        let agent = if self.provenance.agent.is_empty() {
            "an agent".to_string()
        } else {
            self.provenance.agent.clone()
        };
        format!(
            "The last thing I produced was a {} titled \"{}\", by {} ({}).",
            self.kind.as_str(),
            self.title,
            agent,
            attribution,
        )
    }
}

// ---------------------------------------------------------------------------
// REGISTRY — the bounded, in-memory ledger
// ---------------------------------------------------------------------------

/// A bounded in-memory ledger of recent artifacts. Oldest at the FRONT, newest at
/// the BACK; past `bound` the oldest is evicted. Owns its own monotonic id counter
/// so a fresh `Registry` is fully deterministic (the process-global instance is
/// just this behind a lock). `enabled` mirrors `[artifact].enabled`: when off,
/// [`Registry::register`] is a no-op (nothing is remembered) — the master gate.
#[derive(Debug)]
pub struct Registry {
    enabled: bool,
    bound: usize,
    next_id: u64,
    items: VecDeque<ArtifactRef>,
}

impl Registry {
    /// The const initializer for the process-global static (armed by default, with
    /// the default bound). Reconfigured from config by [`configure`] at startup.
    const fn const_new() -> Self {
        Registry {
            enabled: true,
            bound: DEFAULT_REGISTRY_BOUND,
            next_id: 1,
            items: VecDeque::new(),
        }
    }

    /// A fresh registry with an explicit bound (clamped to `[1, MAX_REGISTRY_BOUND]`)
    /// — used by tests to exercise the logic deterministically (the live process uses
    /// the process-global static + [`configure`]).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(bound: usize) -> Self {
        Registry {
            enabled: true,
            bound: clamp_bound(bound),
            next_id: 1,
            items: VecDeque::new(),
        }
    }

    /// Apply live config: the master gate + the retention bound. Shrinking the bound
    /// evicts the oldest over the new cap immediately, so the invariant `len <= bound`
    /// always holds.
    pub fn configure(&mut self, enabled: bool, bound: usize) {
        self.enabled = enabled;
        self.bound = clamp_bound(bound);
        self.evict_to_bound();
    }

    /// Register a produced artifact. Stamps a fresh monotonic id + the current ts,
    /// bounds the strings + citations, pushes to the back, and evicts the oldest
    /// beyond `bound`. Returns the assigned id, or `None` when the registry is
    /// DISABLED (nothing is remembered). Honest: the provenance is stored verbatim —
    /// an uncited artifact stays uncited.
    pub fn register(
        &mut self,
        kind: ArtifactKind,
        title: impl Into<String>,
        provenance: Provenance,
        preview: impl Into<String>,
    ) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let artifact = ArtifactRef {
            id,
            kind,
            title: clamp(title.into().trim(), MAX_TITLE_LEN),
            provenance,
            preview_payload: clamp(preview.into().trim(), MAX_PREVIEW_LEN),
            ts: Utc::now().to_rfc3339(),
        };
        self.items.push_back(artifact);
        self.evict_to_bound();
        Some(id)
    }

    /// Get a registered artifact by id, or `None` if it was never registered or has
    /// been evicted.
    pub fn get(&self, id: u64) -> Option<&ArtifactRef> {
        self.items.iter().find(|a| a.id == id)
    }

    /// The MOST RECENTLY registered artifact still in the window, or `None` when the
    /// registry is empty.
    pub fn most_recent(&self) -> Option<&ArtifactRef> {
        self.items.back()
    }

    /// Number of artifacts currently retained. (Part of the registry's inspection
    /// API — exercised by the tests + available to Share Guard.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// True when nothing is retained.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether registration is currently armed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Drop the oldest entries until `len <= bound`.
    fn evict_to_bound(&mut self) {
        while self.items.len() > self.bound {
            self.items.pop_front();
        }
    }
}

/// Clamp a requested bound into `[1, MAX_REGISTRY_BOUND]` — a registry must hold at
/// least one entry and never grow without limit.
fn clamp_bound(bound: usize) -> usize {
    bound.clamp(1, MAX_REGISTRY_BOUND)
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
// PROCESS-GLOBAL — the one live registry the producers + peek surface share
// ---------------------------------------------------------------------------

/// The one live registry, mirroring telemetry.rs's static-store pattern. Armed by
/// default with the default bound; reconfigured by [`configure`] at startup.
static REGISTRY: RwLock<Registry> = RwLock::new(Registry::const_new());

/// Apply live `[artifact]` config to the process-global registry (called once at
/// startup, next to `telemetry::init`).
pub fn configure(enabled: bool, bound: usize) {
    if let Ok(mut reg) = REGISTRY.write() {
        reg.configure(enabled, bound);
    }
}

/// PRODUCER ENTRYPOINT — register a produced artifact into the process-global
/// registry. Returns the assigned id (or `None` when the subsystem is disabled or
/// the lock is poisoned). Fire-and-forget for producers: a `None` just means the
/// artifact was not remembered, never an error the producer must handle.
///
/// `agent` is the REAL producing agent; `citations` are the REAL citations the
/// artifact carried (pass an EMPTY vec for a genuinely uncited artifact — it will
/// be shown as UNCITED, never fabricated a source). `preview` is a compact,
/// SECRET-FREE line the producer chose.
pub fn register(
    kind: ArtifactKind,
    title: impl Into<String>,
    agent: impl Into<String>,
    citations: Vec<Citation>,
    preview: impl Into<String>,
) -> Option<u64> {
    let provenance = Provenance::new(agent, citations);
    REGISTRY
        .write()
        .ok()
        .and_then(|mut reg| reg.register(kind, title, provenance, preview))
}

/// Read an artifact back out of the process-global registry — a specific `id`, or
/// (when `id` is `None`) the MOST RECENT. Returns an owned clone so the lock is not
/// held across the caller's work. `None` when there is nothing to peek (empty
/// registry, unknown id, or a poisoned lock).
pub fn peek(id: Option<u64>) -> Option<ArtifactRef> {
    let reg = REGISTRY.read().ok()?;
    match id {
        Some(id) => reg.get(id).cloned(),
        None => reg.most_recent().cloned(),
    }
}

/// Emit an [`ArtifactRef`] as the `artifact.peek` telemetry frame the HUD's
/// QuickLook overlay renders. Fire-and-forget over the existing telemetry hub;
/// dropped silently when no HUD is connected. Read-only presentation — no action,
/// no network.
pub fn emit_peek(artifact: &ArtifactRef) {
    telemetry::emit("system", PEEK_EVENT, artifact.to_frame());
}

/// The peek surface's one call: read the addressed artifact (id or most recent),
/// emit its `artifact.peek` frame for the HUD, and return the owned ref for the
/// caller to phrase a reply from. `None` when there is nothing to peek — the caller
/// then says so honestly (never fabricates an artifact).
pub fn peek_and_emit(id: Option<u64>) -> Option<ArtifactRef> {
    let artifact = peek(id)?;
    emit_peek(&artifact);
    Some(artifact)
}

/// The honest "nothing to peek yet" reply for the voice op + the tool.
pub fn empty_reply() -> String {
    "I haven't produced anything to peek at yet, sir — no report, chart, draft, or \
     proposal in this session's registry."
        .to_string()
}

// ---------------------------------------------------------------------------
// INTENT — "what did you just do" / "peek" (explicit, phrase-anchored)
// ---------------------------------------------------------------------------

/// Detect a "show me what you just produced" intent. CONSERVATIVE + phrase-anchored
/// so an ordinary question never trips it: an explicit "peek"/"quick look" cue, or a
/// "what did you (just) do/make/produce/create" recall phrase. Pure — unit-tested.
pub fn classify_peek_intent(utterance: &str) -> bool {
    let lower = utterance.to_lowercase();
    let lower = lower.trim();
    // Explicit peek cues.
    if lower == "peek"
        || lower.contains("quick look")
        || lower.contains("quicklook")
        || lower.contains("peek at what")
        || lower.contains("let me peek")
    {
        return true;
    }
    // "what did you just do / make / produce / create / build / draft" — the
    // recall phrasing the overlay is summoned by. Anchored to a "what did/have you"
    // stem + a production verb so "what did you say" / "what do you think" don't
    // trip it.
    let stem = lower.contains("what did you")
        || lower.contains("what have you")
        || lower.contains("what'd you")
        || lower.contains("what you just");
    if stem {
        const VERBS: &[&str] = &[
            "just do", "just make", "just produce", "just create", "just build",
            "just draft", "just generate", "do", "make", "produce", "create",
            "build", "draft", "generate",
        ];
        if VERBS.iter().any(|v| lower.contains(v)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cite(title: &str, url: &str) -> Citation {
        Citation::new(title, url).expect("non-empty citation")
    }

    // ---- registry: register + monotonic ids + get-by-id + most-recent --------

    #[test]
    fn register_assigns_monotonic_ids_and_get_by_id_and_most_recent() {
        let mut reg = Registry::new(8);
        assert!(reg.is_empty());
        assert!(reg.most_recent().is_none(), "empty registry has no most-recent");

        let id1 = reg
            .register(ArtifactKind::Report, "R1", Provenance::new("darwin", vec![]), "p1")
            .unwrap();
        let id2 = reg
            .register(ArtifactKind::Chart, "C1", Provenance::new("darwin", vec![]), "p2")
            .unwrap();
        assert!(id2 > id1, "ids are monotonic: {id1} then {id2}");
        assert_eq!(reg.len(), 2);

        // get-by-id returns the exact artifact.
        assert_eq!(reg.get(id1).unwrap().title, "R1");
        assert_eq!(reg.get(id2).unwrap().kind, ArtifactKind::Chart);
        assert!(reg.get(9999).is_none(), "unknown id -> None");

        // most-recent is the last registered.
        assert_eq!(reg.most_recent().unwrap().id, id2);
        assert_eq!(reg.most_recent().unwrap().title, "C1");
    }

    // ---- registry: bounded eviction (keep last N, drop oldest) ---------------

    #[test]
    fn register_evicts_oldest_beyond_the_bound() {
        let mut reg = Registry::new(3);
        let mut ids = Vec::new();
        for i in 0..5 {
            let id = reg
                .register(
                    ArtifactKind::Draft,
                    format!("D{i}"),
                    Provenance::new("veronica", vec![]),
                    "",
                )
                .unwrap();
            ids.push(id);
        }
        // Only the last 3 survive; the first 2 were evicted (oldest-first).
        assert_eq!(reg.len(), 3, "bounded to N=3");
        assert!(reg.get(ids[0]).is_none(), "oldest evicted");
        assert!(reg.get(ids[1]).is_none(), "second-oldest evicted");
        assert!(reg.get(ids[2]).is_some(), "survivor");
        assert!(reg.get(ids[3]).is_some(), "survivor");
        assert!(reg.get(ids[4]).is_some(), "newest survivor");
        assert_eq!(reg.most_recent().unwrap().title, "D4");
    }

    #[test]
    fn configure_shrinking_the_bound_evicts_immediately() {
        let mut reg = Registry::new(10);
        for i in 0..6 {
            reg.register(ArtifactKind::Chart, format!("c{i}"), Provenance::new("a", vec![]), "")
                .unwrap();
        }
        assert_eq!(reg.len(), 6);
        reg.configure(true, 2);
        assert_eq!(reg.len(), 2, "shrinking the bound evicts the oldest immediately");
        assert_eq!(reg.most_recent().unwrap().title, "c5");
    }

    #[test]
    fn bound_is_clamped_and_never_zero() {
        // Zero clamps up to 1 (a registry must hold at least one).
        let mut reg = Registry::new(0);
        reg.register(ArtifactKind::Report, "only", Provenance::new("a", vec![]), "")
            .unwrap();
        assert_eq!(reg.len(), 1);
        // A huge bound clamps down to the ceiling.
        let reg2 = Registry::new(usize::MAX);
        assert!(reg2.bound <= MAX_REGISTRY_BOUND);
    }

    #[test]
    fn disabled_registry_registers_nothing() {
        let mut reg = Registry::new(4);
        assert!(reg.is_enabled(), "armed by default");
        reg.configure(false, 4);
        assert!(!reg.is_enabled(), "configure(false) disarms");
        let out = reg.register(ArtifactKind::Report, "x", Provenance::new("a", vec![]), "");
        assert!(out.is_none(), "disabled -> no id");
        assert!(reg.is_empty(), "disabled -> nothing remembered");
    }

    // ---- kind vocabulary: every variant serializes to its stable wire string --

    #[test]
    fn every_kind_serializes_to_its_stable_wire_string() {
        // Pin the whole closed vocabulary the HUD switches on — every variant is a
        // real, intended kind a producer may register.
        assert_eq!(ArtifactKind::Report.as_str(), "report");
        assert_eq!(ArtifactKind::Chart.as_str(), "chart");
        assert_eq!(ArtifactKind::Image.as_str(), "image");
        assert_eq!(ArtifactKind::Draft.as_str(), "draft");
        assert_eq!(ArtifactKind::CodeDiff.as_str(), "code_diff");
        assert_eq!(ArtifactKind::Notebook.as_str(), "notebook");
        assert_eq!(ArtifactKind::Forecast.as_str(), "forecast");
        assert_eq!(ArtifactKind::DocSearch.as_str(), "docsearch");
        assert_eq!(ArtifactKind::Other("custom".into()).as_str(), "custom");
    }

    // ---- provenance honesty: uncited stays uncited; cited carries the real ---

    #[test]
    fn uncited_artifact_is_reported_uncited_never_fabricated() {
        let prov = Provenance::new("edith", vec![]);
        assert!(prov.is_uncited(), "no citations -> uncited");
        let mut reg = Registry::new(4);
        let id = reg
            .register(ArtifactKind::Chart, "System load", prov, "cpu 42%, mem 50%")
            .unwrap();
        let a = reg.get(id).unwrap();
        let frame = a.to_frame();
        assert_eq!(frame["uncited"], true, "uncited artifact frames as uncited");
        assert_eq!(frame["citation_count"], 0);
        assert_eq!(frame["citations"].as_array().unwrap().len(), 0);
        // The summary says UNCITED plainly — never implies a source.
        assert!(a.summary().contains("uncited"), "summary is honest: {}", a.summary());
    }

    #[test]
    fn cited_artifact_carries_the_real_citations_verbatim() {
        let cites = vec![
            cite("JWST overview", "https://nasa.gov/jwst"),
            cite("Deep field", "https://nasa.gov/deepfield"),
        ];
        let prov = Provenance::new("darwin", cites);
        assert!(!prov.is_uncited(), "has citations -> cited");
        let mut reg = Registry::new(4);
        let id = reg
            .register(ArtifactKind::Report, "JWST", prov, "3 sections")
            .unwrap();
        let frame = reg.get(id).unwrap().to_frame();
        assert_eq!(frame["uncited"], false);
        assert_eq!(frame["citation_count"], 2);
        let arr = frame["citations"].as_array().unwrap();
        assert_eq!(arr[0]["title"], "JWST overview");
        assert_eq!(arr[0]["url"], "https://nasa.gov/jwst");
        assert_eq!(arr[1]["url"], "https://nasa.gov/deepfield");
    }

    #[test]
    fn blank_citations_are_dropped_but_uncited_is_never_padded() {
        // A citation with both fields blank is not a real source -> dropped.
        assert!(Citation::new("  ", "").is_none());
        assert!(Citation::new("", "  ").is_none());
        // A title-only or url-only citation is real -> kept.
        assert!(Citation::new("Doc", "").is_some());
        assert!(Citation::new("", "file:///x").is_some());
        // A provenance built from only-blank citations is honestly uncited (no fill).
        let prov = Provenance::new("a", vec![]);
        assert!(prov.is_uncited());
    }

    // ---- frame shape: secret-free, exactly the known keys --------------------

    #[test]
    fn frame_is_secret_free_and_has_exactly_the_known_keys() {
        let mut reg = Registry::new(4);
        let id = reg
            .register(
                ArtifactKind::CodeDiff,
                "rename parse_config",
                Provenance::new("steve", vec![cite("src/config.rs", "src/config.rs:12")]),
                "diff: 3 files, 2 hunks",
            )
            .unwrap();
        let frame = reg.get(id).unwrap().to_frame();
        let obj = frame.as_object().unwrap();
        // EXACTLY the known secret-free keys — no internal field leaks onto the wire.
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "agent", "citation_count", "citations", "id", "kind", "preview",
                "title", "ts", "uncited",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>(),
        );
        assert_eq!(frame["kind"], "code_diff");
        assert_eq!(frame["agent"], "steve");
        // The preview is the producer's redacted line, verbatim (bounded).
        assert_eq!(frame["preview"], "diff: 3 files, 2 hunks");
        assert!(frame["ts"].as_str().unwrap().len() >= 10, "ts is stamped");
    }

    #[test]
    fn strings_are_bounded_so_a_runaway_producer_cannot_blow_the_frame() {
        let mut reg = Registry::new(2);
        let big_title = "T".repeat(5000);
        let big_preview = "P".repeat(5000);
        let id = reg
            .register(ArtifactKind::Report, big_title, Provenance::new("a", vec![]), big_preview)
            .unwrap();
        let a = reg.get(id).unwrap();
        assert!(a.title.len() <= MAX_TITLE_LEN, "title bounded");
        assert!(a.preview_payload.len() <= MAX_PREVIEW_LEN, "preview bounded");
    }

    #[test]
    fn citations_are_bounded_per_artifact() {
        let many: Vec<Citation> = (0..100).map(|i| cite(&format!("s{i}"), &format!("u{i}"))).collect();
        let prov = Provenance::new("a", many);
        assert!(prov.citations.len() <= MAX_CITATIONS, "citations bounded");
    }

    #[test]
    fn kind_other_carries_its_own_label_never_upgraded() {
        let k = ArtifactKind::Other("forecast_v2".to_string());
        assert_eq!(k.as_str(), "forecast_v2");
        let mut reg = Registry::new(2);
        let id = reg
            .register(k, "x", Provenance::new("cassandra", vec![]), "")
            .unwrap();
        assert_eq!(reg.get(id).unwrap().to_frame()["kind"], "forecast_v2");
    }

    // ---- global surface: register -> peek -> emit round-trip -----------------

    #[test]
    fn global_register_peek_and_emit_round_trip() {
        // The process-global registry is shared across tests, so address our OWN
        // artifact by the id register() returns (never assume most-recent is ours).
        let mut rx = telemetry::subscribe_for_test();
        let id = register(
            ArtifactKind::Report,
            "global round-trip",
            "darwin",
            vec![cite("Source", "https://example.com/s")],
            "1 section",
        )
        .expect("armed-by-default registry accepts the register");

        // peek by id returns our exact artifact.
        let got = peek(Some(id)).expect("registered artifact is peekable");
        assert_eq!(got.id, id);
        assert_eq!(got.title, "global round-trip");

        // peek_and_emit publishes the artifact.peek frame.
        let emitted = peek_and_emit(Some(id)).unwrap();
        assert_eq!(emitted.id, id);
        // Drain the hub until we see OUR frame (other tests may share the bus).
        let mut saw = false;
        while let Ok(raw) = rx.try_recv() {
            let env: Value = serde_json::from_str(&raw).unwrap();
            if env["event"] == PEEK_EVENT && env["data"]["id"] == id {
                assert_eq!(env["data"]["title"], "global round-trip");
                assert_eq!(env["data"]["uncited"], false);
                saw = true;
                break;
            }
        }
        assert!(saw, "the artifact.peek frame reached the hub");
    }

    #[test]
    fn peek_unknown_id_is_none() {
        assert!(peek(Some(u64::MAX)).is_none(), "an id never registered -> None");
    }

    // ---- intent classification ----------------------------------------------

    #[test]
    fn classifies_peek_and_what_did_you_just_do() {
        assert!(classify_peek_intent("peek"));
        assert!(classify_peek_intent("quick look"));
        assert!(classify_peek_intent("what did you just do"));
        assert!(classify_peek_intent("what did you just make?"));
        assert!(classify_peek_intent("what have you produced"));
        assert!(classify_peek_intent("let me peek at that"));
        // NOT a peek: ordinary questions.
        assert!(!classify_peek_intent("what's the weather"));
        assert!(!classify_peek_intent("what did you say"));
        assert!(!classify_peek_intent("what do you think about jazz"));
        assert!(!classify_peek_intent("how are you"));
    }
}
