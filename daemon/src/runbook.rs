//! RUNBOOKS: a benign-only, TYPED, INSPECTABLE automation DAG — a step up from the
//! opaque record/replay [`crate::macros`]. A runbook is a `*.runbook.toml` file whose
//! steps form a directed acyclic graph: each step names a capability it `uses` (a
//! tool / skill / agent), a typed `with` input map, the step ids it `needs`, and an
//! optional named `out`. The delta over macros is DATA FLOW — a step's `out` can feed
//! a later step's `with` — everything else about the safety posture is IDENTICAL to
//! macros, and the two invariants below are enforced HERE.
//!
//! ## The safety model — the runbook carries NO AUTHORITY (mirrors [`crate::macros`])
//!
//! - **PURE, INSPECTABLE ANALYSIS.** [`parse`], [`check`], [`topological_order`] and
//!   [`plan`] touch no store, no network, no clock, no gate. `check` produces
//!   LSP-style [`Diagnostic`]s (unknown capability, missing `needs`, a cycle, a type
//!   mismatch, an over-privileged step) and `plan` renders the WHOLE DAG — every step,
//!   its data-flow edges, and which steps will PARK — BEFORE anything runs. You can
//!   read exactly what a runbook will do without running it.
//!
//! - **NO GATE BYPASS.** [`run`] walks the DAG in topological order and re-issues EACH
//!   step through the injected [`RunbookRouter`] — the SAME router + gate path a live
//!   command takes — ONE AT A TIME. A consequential step therefore hits the cross-turn
//!   confirmation gate + the `[integrations].allow_consequential` master switch +
//!   voice-id + lockdown FRESH, exactly as if issued live: no pre-approval, no
//!   batching, no "the DAG already reached this step so skip the gate". The privilege
//!   classification is derived from the SAME [`crate::confirm::is_consequential_tool`]
//!   set the gate itself uses, so a runbook can never UNDER-classify a consequential
//!   tool into looking benign.
//!
//! - **DATA FLOW SMUGGLES NO AUTHORITY.** A step's `out` feeding a later step's `with`
//!   passes a VALUE, never an authorization. When a consequential step PARKS it
//!   produces NO output, so a downstream step that consumes that output is BLOCKED —
//!   the executor NEVER fabricates a parked step's result to keep the DAG flowing, and
//!   NEVER routes a downstream step on a value that was never actually produced. Each
//!   step re-earns its own gate.
//!
//! Ships OFF (`[runbook].enabled = false`): with it off nothing plans or runs. The
//! whole subsystem is HERMETIC — the parser/typechecker/planner are pure functions and
//! the executor is generic over an injected router, so every property below is proven
//! by unit tests with no store, network, or real client.

// INTEGRATION SEAMS (where the live daemon wiring lands — the PURE seams below are the
// substance + are covered by the hermetic tests in this file; the `runbook_plan` /
// `runbook_run` op-dispatch is reconciled at integration, so the pending-wiring public
// API stays `#![allow(dead_code)]` as the auditable contract surface, mirroring
// lumen.rs / policy.rs / prosody.rs / triage.rs):
//   * router.rs — a `classify_runbook_command` arm (mirroring the macro arm) hands a
//     runbook to [`plan`] / [`run`], where [`run`] re-issues each step through the SAME
//     router + gate a live command takes (a consequential step PARKS FRESH).
//   * config.rs — `[runbook]` (RunbookConfig, SHIPS OFF) gates the whole subsystem.
//   * telemetry.rs — [`emit_plan`] / [`emit_run`] / [`status_frame`] are the SECRET-FREE
//     frames the HUD renders (`status_frame` is already wired at daemon startup).
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use serde::Deserialize;
use serde_json::{json, Value};

/// Default cap on the number of steps one runbook may hold (a bounded DAG). Mirrors
/// the macro `max_steps` bound so a runbook can never grow unbounded.
pub const DEFAULT_MAX_STEPS: usize = 32;

// ---------------------------------------------------------------------------
// Types: the small, closed type lattice for a step's inputs/outputs
// ---------------------------------------------------------------------------

/// The typed kind of a runbook value — a small, closed lattice. A `with` literal is
/// typed from its TOML value; a capability declares the type of each parameter it
/// accepts and the type of the value it `out`puts, so the checker can prove a data
/// edge (`${producer.out}` -> a consuming `with` param) is type-sound BEFORE anything
/// runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Str,
    Int,
    Float,
    Bool,
    List,
    Map,
}

impl Ty {
    /// Human label used in diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Ty::Str => "string",
            Ty::Int => "integer",
            Ty::Float => "float",
            Ty::Bool => "bool",
            Ty::List => "list",
            Ty::Map => "map",
        }
    }

    /// The type of a concrete TOML literal (the checker types a `with` value from
    /// this). A datetime is treated as a string; a nested table is a map.
    fn of_toml(v: &toml::Value) -> Ty {
        match v {
            toml::Value::String(_) | toml::Value::Datetime(_) => Ty::Str,
            toml::Value::Integer(_) => Ty::Int,
            toml::Value::Float(_) => Ty::Float,
            toml::Value::Boolean(_) => Ty::Bool,
            toml::Value::Array(_) => Ty::List,
            toml::Value::Table(_) => Ty::Map,
        }
    }
}

/// A resolved input EXPRESSION for a `with` parameter: either an inline literal (with
/// its inferred type + the original TOML value for execution) or a REFERENCE to an
/// earlier step's output (`${producer.out}`), carried structurally so the checker can
/// type it and the executor can substitute the produced value at run time.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// An inline literal: its inferred type + the original TOML value (converted to
    /// JSON for execution). Carries the value so the executor can pass it through; the
    /// telemetry frame NEVER includes it (only the param name + kind + type).
    Literal { ty: Ty, value: Value },
    /// `${producer.output}` — a data edge from an earlier step. Resolved to the
    /// producer's actual output value at run time; type-checked against the producer's
    /// declared `out` type at plan time.
    Ref { producer: String, output: String },
    /// A malformed `${...}` reference (empty / not exactly `producer.output`). Retained
    /// so the checker can flag it precisely rather than silently treating it as a
    /// string literal.
    BadRef { raw: String },
}

// ---------------------------------------------------------------------------
// The parsed, typed runbook
// ---------------------------------------------------------------------------

/// One typed step in the DAG.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    /// The step's unique id (its addressing label within the runbook).
    pub id: String,
    /// The capability this step invokes — a tool / skill / agent name. Resolved
    /// against the [`Registry`] by the checker.
    pub uses: String,
    /// The typed input expressions, keyed by parameter name (sorted for a stable
    /// render).
    pub with: BTreeMap<String, Expr>,
    /// The step ids this step depends on (its incoming DAG edges).
    pub needs: Vec<String>,
    /// The name this step publishes its output under, if any (what a later
    /// `${id.out}` references).
    pub out: Option<String>,
}

/// A parsed, typed runbook (a DAG of [`Step`]s). Produced by [`parse`]; still needs
/// [`check`] to be proven sound.
#[derive(Debug, Clone, PartialEq)]
pub struct Runbook {
    /// The runbook's name (from the `[runbook] name = ...` header).
    pub name: String,
    /// The steps, in file order.
    pub steps: Vec<Step>,
}

impl Runbook {
    /// Look up a step by id. Pure.
    pub fn step(&self, id: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.id == id)
    }
}

// ---------------------------------------------------------------------------
// Parser (PURE): *.runbook.toml -> typed Runbook
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawFile {
    runbook: RawHeader,
    #[serde(default, rename = "step")]
    steps: Vec<RawStep>,
}

#[derive(Debug, Deserialize)]
struct RawHeader {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    id: String,
    uses: String,
    #[serde(default)]
    with: toml::Table,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    out: Option<String>,
}

/// A hard, structural parse failure (as distinct from a semantic [`Diagnostic`], which
/// [`check`] reports on a WELL-FORMED runbook). Malformed TOML, a missing required
/// field, or an empty name is a parse error; an unknown capability or a type mismatch
/// is not — those are diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ParseError {}

/// Parse the boundary between a `${...}` reference and a plain string literal. Returns
/// the [`Expr`] for a `with` value: a well-formed `${producer.output}` becomes a
/// [`Expr::Ref`], a malformed `${...}` becomes a [`Expr::BadRef`], and anything else
/// is a typed [`Expr::Literal`]. Pure.
fn to_expr(v: &toml::Value) -> Expr {
    if let toml::Value::String(s) = v {
        let trimmed = s.trim();
        if let Some(inner) = trimmed.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
            let inner = inner.trim();
            let parts: Vec<&str> = inner.split('.').collect();
            if parts.len() == 2 && !parts[0].trim().is_empty() && !parts[1].trim().is_empty() {
                return Expr::Ref {
                    producer: parts[0].trim().to_string(),
                    output: parts[1].trim().to_string(),
                };
            }
            return Expr::BadRef { raw: s.clone() };
        }
    }
    Expr::Literal {
        ty: Ty::of_toml(v),
        value: toml_to_json(v),
    }
}

/// Convert a TOML value to the JSON value the executor passes to the router. Pure and
/// total.
fn toml_to_json(v: &toml::Value) -> Value {
    match v {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => json!(i),
        toml::Value::Float(f) => json!(f),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => {
            Value::Object(t.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect())
        }
    }
}

/// PARSE a `*.runbook.toml` document into a typed [`Runbook`]. PURE: no store, no
/// clock, no network. Returns a [`ParseError`] only for a STRUCTURAL failure (bad
/// TOML, a missing `id`/`uses`, an empty name); every SEMANTIC problem (unknown
/// capability, type mismatch, cycle, ...) is left for [`check`] so a partially-broken
/// runbook still parses far enough to be diagnosed richly.
pub fn parse(source: &str) -> Result<Runbook, ParseError> {
    let raw: RawFile = toml::from_str(source).map_err(|e| ParseError(e.message().to_string()))?;
    let name = raw.runbook.name.trim().to_string();
    if name.is_empty() {
        return Err(ParseError("runbook name must not be empty".to_string()));
    }
    let mut steps = Vec::with_capacity(raw.steps.len());
    for rs in raw.steps {
        let id = rs.id.trim().to_string();
        if id.is_empty() {
            return Err(ParseError("a step id must not be empty".to_string()));
        }
        let uses = rs.uses.trim().to_string();
        if uses.is_empty() {
            return Err(ParseError(format!("step \"{id}\": `uses` must not be empty")));
        }
        let with: BTreeMap<String, Expr> =
            rs.with.iter().map(|(k, v)| (k.clone(), to_expr(v))).collect();
        let needs = rs
            .needs
            .into_iter()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect();
        let out = rs.out.map(|o| o.trim().to_string()).filter(|o| !o.is_empty());
        steps.push(Step { id, uses, with, needs, out });
    }
    Ok(Runbook { name, steps })
}

// ---------------------------------------------------------------------------
// Capability registry (the checker's ground truth)
// ---------------------------------------------------------------------------

/// What a capability IS — surfaced in diagnostics/plan so a reader knows whether a
/// step invokes a tool, a skill, or an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapKind {
    Tool,
    Skill,
    Agent,
}

impl CapKind {
    fn label(self) -> &'static str {
        match self {
            CapKind::Tool => "tool",
            CapKind::Skill => "skill",
            CapKind::Agent => "agent",
        }
    }
}

/// A capability's privilege class. A [`Privilege::Consequential`] capability side-
/// effects the world (posts / spends / actuates), so a step that uses one will PARK
/// for a fresh spoken confirmation at run time — the runbook can never pre-approve it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    Benign,
    Consequential,
}

/// One declared parameter of a capability.
#[derive(Debug, Clone)]
pub struct Param {
    pub name: &'static str,
    pub ty: Ty,
    pub required: bool,
}

/// A capability the checker knows about: its kind, privilege, typed parameter schema,
/// and the type of the value it produces (so a downstream `${id.out}` can be typed).
#[derive(Debug, Clone)]
pub struct Capability {
    pub name: &'static str,
    pub kind: CapKind,
    pub privilege: Privilege,
    pub params: Vec<Param>,
    pub out: Ty,
}

impl Capability {
    fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }
}

/// The set of capabilities a runbook may reference. The checker resolves each step's
/// `uses` against this. Injectable so tests drive a precise, minimal registry; the
/// [`Registry::builtin`] seed mirrors a representative slice of the daemon's real tool
/// surface AND ties every tool's privilege to the SAME
/// [`crate::confirm::is_consequential_tool`] source of truth the gate uses.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    caps: HashMap<String, Capability>,
}

impl Registry {
    /// An empty registry (every `uses` will resolve as unknown). Mostly for tests that
    /// build a bespoke set via [`Registry::with`].
    pub fn empty() -> Self {
        Registry { caps: HashMap::new() }
    }

    /// Build a registry from an explicit capability list (tests).
    pub fn with(caps: Vec<Capability>) -> Self {
        let mut map = HashMap::new();
        for c in caps {
            map.insert(c.name.to_string(), c);
        }
        Registry { caps: map }
    }

    /// Resolve a capability by name.
    pub fn get(&self, name: &str) -> Option<&Capability> {
        self.caps.get(name)
    }

    /// A representative built-in registry over a slice of the daemon's real capability
    /// surface. Every TOOL's privilege is taken from the SAME
    /// [`crate::confirm::is_consequential_tool`] registry the cross-turn gate consults,
    /// so runbook's "will PARK" verdict can never disagree with the gate about which
    /// tools are consequential. Skills/agents (not in that set) declare their own
    /// privilege. This seed is intentionally small; integration extends it from the
    /// live tool catalog.
    pub fn builtin() -> Self {
        // Read-only tools (BENIGN).
        let mut caps = vec![
            Capability {
                name: "github_list_prs",
                kind: CapKind::Tool,
                privilege: Privilege::Benign,
                params: vec![
                    Param { name: "owner", ty: Ty::Str, required: true },
                    Param { name: "repo", ty: Ty::Str, required: true },
                    Param { name: "state", ty: Ty::Str, required: false },
                ],
                out: Ty::List,
            },
            Capability {
                name: "gmail_list_recent",
                kind: CapKind::Tool,
                privilege: Privilege::Benign,
                params: vec![
                    Param { name: "max", ty: Ty::Int, required: false },
                    Param { name: "query", ty: Ty::Str, required: false },
                ],
                out: Ty::List,
            },
            Capability {
                name: "recall_semantic",
                kind: CapKind::Tool,
                privilege: Privilege::Benign,
                params: vec![Param { name: "query", ty: Ty::Str, required: true }],
                out: Ty::List,
            },
            // A pure skill (BENIGN): summarize text.
            Capability {
                name: "distill",
                kind: CapKind::Skill,
                privilege: Privilege::Benign,
                params: vec![Param { name: "text", ty: Ty::Str, required: true }],
                out: Ty::Str,
            },
            // Consequential tools — privilege pinned below from confirm.rs.
            Capability {
                name: "slack_post_message",
                kind: CapKind::Tool,
                privilege: Privilege::Consequential,
                params: vec![
                    Param { name: "channel", ty: Ty::Str, required: true },
                    Param { name: "text", ty: Ty::Str, required: true },
                ],
                out: Ty::Str,
            },
            Capability {
                name: "gmail_send",
                kind: CapKind::Tool,
                privilege: Privilege::Consequential,
                params: vec![
                    Param { name: "to", ty: Ty::Str, required: true },
                    Param { name: "subject", ty: Ty::Str, required: true },
                    Param { name: "body", ty: Ty::Str, required: true },
                ],
                out: Ty::Str,
            },
            Capability {
                name: "github_open_pr",
                kind: CapKind::Tool,
                privilege: Privilege::Consequential,
                params: vec![
                    Param { name: "owner", ty: Ty::Str, required: true },
                    Param { name: "repo", ty: Ty::Str, required: true },
                    Param { name: "title", ty: Ty::Str, required: true },
                ],
                out: Ty::Str,
            },
        ];
        // INTEGRITY: pin every TOOL capability's privilege to the gate's own
        // consequential registry, so a runbook can never disagree with confirm.rs
        // about what parks. A capability we seeded as Benign but that the gate treats
        // as consequential is promoted here (fail-safe toward MORE gating, never less).
        for c in &mut caps {
            if c.kind == CapKind::Tool && crate::confirm::is_consequential_tool(c.name) {
                c.privilege = Privilege::Consequential;
            }
        }
        Registry::with(caps)
    }
}

// ---------------------------------------------------------------------------
// Diagnostics (LSP-style, PURE)
// ---------------------------------------------------------------------------

/// Diagnostic severity. An [`Severity::Error`] means the runbook is UNSOUND and
/// [`run`] will REFUSE to execute it; a [`Severity::Warning`] is advisory (the runbook
/// still runs, but the flagged step behaves as noted — e.g. an over-privileged step
/// will PARK).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// The kind of a diagnostic — the closed set the checker can raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagKind {
    /// A `uses` names a capability not in the registry.
    UnknownCapability,
    /// A `needs` (or a `${ref}` producer) names a step id that does not exist.
    MissingNeed,
    /// The `needs` edges contain a cycle (the DAG is not acyclic).
    Cycle,
    /// A `with` value's type does not match the capability's declared parameter type,
    /// or a `${ref}` producer's output type does not match the consuming parameter.
    TypeMismatch,
    /// A `with` key that the capability does not declare.
    UnknownParam,
    /// A required parameter the step does not supply.
    MissingParam,
    /// A `${ref}` whose producer is not listed in this step's `needs` (a data edge that
    /// is not backed by a declared dependency), or a malformed `${...}`.
    UndeclaredDataDep,
    /// A step whose capability is consequential: it will PARK for a fresh spoken
    /// confirmation at run time. The runbook grants it no authority (advisory).
    OverPrivileged,
    /// Two steps share an id.
    DuplicateStepId,
}

/// One LSP-style diagnostic against a runbook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub kind: DiagKind,
    /// The step id the diagnostic is about (None for whole-runbook issues like a
    /// cycle spanning several steps).
    pub step: Option<String>,
    pub message: String,
}

impl Diagnostic {
    fn error(kind: DiagKind, step: Option<&str>, message: String) -> Self {
        Diagnostic { severity: Severity::Error, kind, step: step.map(str::to_string), message }
    }
    fn warn(kind: DiagKind, step: Option<&str>, message: String) -> Self {
        Diagnostic { severity: Severity::Warning, kind, step: step.map(str::to_string), message }
    }
}

/// TYPECHECK a runbook against a capability [`Registry`], returning every LSP-style
/// [`Diagnostic`] it finds. PURE — no store, no clock, no network. Never short-
/// circuits: it reports ALL problems in one pass (an unknown capability does not hide
/// a downstream type mismatch), so a single render surfaces the whole picture.
///
/// The presence of ANY [`Severity::Error`] means the runbook is unsound; [`run`]
/// refuses to execute such a runbook. [`DiagKind::OverPrivileged`] is a WARNING — the
/// runbook is still valid, but the flagged step will PARK for a fresh spoken confirm.
pub fn check(rb: &Runbook, reg: &Registry) -> Vec<Diagnostic> {
    let mut diags = Vec::new();

    // Index step ids; flag duplicates.
    let mut ids: HashSet<&str> = HashSet::new();
    let mut dupes: HashSet<&str> = HashSet::new();
    for s in &rb.steps {
        if !ids.insert(s.id.as_str()) {
            dupes.insert(s.id.as_str());
        }
    }
    for d in &dupes {
        diags.push(Diagnostic::error(
            DiagKind::DuplicateStepId,
            Some(d),
            format!("duplicate step id \"{d}\""),
        ));
    }

    // Map id -> the capability's declared out type (for typing data edges). Only
    // recorded for steps whose capability + `out` are both known/present.
    let mut out_ty: HashMap<&str, Ty> = HashMap::new();
    for s in &rb.steps {
        if let (Some(cap), Some(_)) = (reg.get(&s.uses), &s.out) {
            out_ty.insert(s.id.as_str(), cap.out);
        }
    }

    for s in &rb.steps {
        let sid = s.id.as_str();

        // -- needs edges must name existing steps --------------------------------
        for n in &s.needs {
            if !ids.contains(n.as_str()) {
                diags.push(Diagnostic::error(
                    DiagKind::MissingNeed,
                    Some(sid),
                    format!("step \"{sid}\" needs \"{n}\", which is not a step in this runbook"),
                ));
            }
        }

        // -- resolve the capability ---------------------------------------------
        let Some(cap) = reg.get(&s.uses) else {
            diags.push(Diagnostic::error(
                DiagKind::UnknownCapability,
                Some(sid),
                format!("step \"{sid}\" uses unknown capability \"{}\"", s.uses),
            ));
            // Without a schema we cannot type its `with`; still check its refs below.
            check_refs_only(s, &ids, &out_ty, &mut diags);
            continue;
        };

        // -- over-privileged (advisory): a consequential step will PARK ----------
        if cap.privilege == Privilege::Consequential {
            diags.push(Diagnostic::warn(
                DiagKind::OverPrivileged,
                Some(sid),
                format!(
                    "step \"{sid}\" uses consequential {} \"{}\" — it will PARK for a fresh \
                     spoken confirmation at run time; the runbook cannot pre-approve it",
                    cap.kind.label(),
                    cap.name
                ),
            ));
        }

        // -- typecheck each `with` param against the schema ----------------------
        let needs: HashSet<&str> = s.needs.iter().map(String::as_str).collect();
        for (key, expr) in &s.with {
            let Some(param) = cap.param(key) else {
                diags.push(Diagnostic::error(
                    DiagKind::UnknownParam,
                    Some(sid),
                    format!("step \"{sid}\": capability \"{}\" has no parameter \"{key}\"", cap.name),
                ));
                continue;
            };
            match expr {
                Expr::Literal { ty, .. } => {
                    if *ty != param.ty {
                        diags.push(Diagnostic::error(
                            DiagKind::TypeMismatch,
                            Some(sid),
                            format!(
                                "step \"{sid}\": parameter \"{key}\" expects {}, got {} literal",
                                param.ty.label(),
                                ty.label()
                            ),
                        ));
                    }
                }
                Expr::BadRef { raw } => {
                    diags.push(Diagnostic::error(
                        DiagKind::UndeclaredDataDep,
                        Some(sid),
                        format!("step \"{sid}\": malformed reference {raw:?} (want ${{producer.output}})"),
                    ));
                }
                Expr::Ref { producer, output } => {
                    // The producer must exist ...
                    if !ids.contains(producer.as_str()) {
                        diags.push(Diagnostic::error(
                            DiagKind::MissingNeed,
                            Some(sid),
                            format!("step \"{sid}\": \"{key}\" references \"{producer}.{output}\", but \"{producer}\" is not a step"),
                        ));
                        continue;
                    }
                    // ... and be a DECLARED dependency (a data edge with no `needs`
                    // backing is a silent ordering hazard) ...
                    if !needs.contains(producer.as_str()) {
                        diags.push(Diagnostic::error(
                            DiagKind::UndeclaredDataDep,
                            Some(sid),
                            format!("step \"{sid}\": \"{key}\" references \"{producer}\" but does not list it in `needs`"),
                        ));
                    }
                    // ... and expose the referenced output name with a matching type.
                    match rb.step(producer).and_then(|p| p.out.as_deref()) {
                        Some(pname) if pname == output => {
                            if let Some(&pty) = out_ty.get(producer.as_str()) {
                                if pty != param.ty {
                                    diags.push(Diagnostic::error(
                                        DiagKind::TypeMismatch,
                                        Some(sid),
                                        format!(
                                            "step \"{sid}\": \"{key}\" expects {}, but \"{producer}.{output}\" produces {}",
                                            param.ty.label(),
                                            pty.label()
                                        ),
                                    ));
                                }
                            }
                        }
                        _ => {
                            diags.push(Diagnostic::error(
                                DiagKind::MissingNeed,
                                Some(sid),
                                format!("step \"{sid}\": \"{producer}\" does not publish an output named \"{output}\""),
                            ));
                        }
                    }
                }
            }
        }

        // -- required params must be supplied ------------------------------------
        for p in &cap.params {
            if p.required && !s.with.contains_key(p.name) {
                diags.push(Diagnostic::error(
                    DiagKind::MissingParam,
                    Some(sid),
                    format!("step \"{sid}\": required parameter \"{}\" ({}) is missing", p.name, p.ty.label()),
                ));
            }
        }
    }

    // -- cycle detection over the needs edges (existing steps only) --------------
    if let Err(cycle) = topological_order(rb) {
        diags.push(Diagnostic::error(
            DiagKind::Cycle,
            None,
            format!("the runbook has a dependency cycle among: {}", cycle.join(", ")),
        ));
    }

    diags
}

/// When a step's capability is unknown we cannot type its `with`, but its `${ref}`
/// data edges are still worth checking for a dangling producer / undeclared dep.
fn check_refs_only(
    s: &Step,
    ids: &HashSet<&str>,
    _out_ty: &HashMap<&str, Ty>,
    diags: &mut Vec<Diagnostic>,
) {
    let sid = s.id.as_str();
    let needs: HashSet<&str> = s.needs.iter().map(String::as_str).collect();
    for (key, expr) in &s.with {
        match expr {
            Expr::Ref { producer, output } => {
                if !ids.contains(producer.as_str()) {
                    diags.push(Diagnostic::error(
                        DiagKind::MissingNeed,
                        Some(sid),
                        format!("step \"{sid}\": \"{key}\" references \"{producer}.{output}\", but \"{producer}\" is not a step"),
                    ));
                } else if !needs.contains(producer.as_str()) {
                    diags.push(Diagnostic::error(
                        DiagKind::UndeclaredDataDep,
                        Some(sid),
                        format!("step \"{sid}\": \"{key}\" references \"{producer}\" but does not list it in `needs`"),
                    ));
                }
            }
            Expr::BadRef { raw } => diags.push(Diagnostic::error(
                DiagKind::UndeclaredDataDep,
                Some(sid),
                format!("step \"{sid}\": malformed reference {raw:?} (want ${{producer.output}})"),
            )),
            Expr::Literal { .. } => {}
        }
    }
}

/// Whether a set of diagnostics contains any [`Severity::Error`].
pub fn has_errors(diags: &[Diagnostic]) -> bool {
    diags.iter().any(|d| d.severity == Severity::Error)
}

// ---------------------------------------------------------------------------
// Topological order (PURE) — Kahn over the needs edges
// ---------------------------------------------------------------------------

/// TOPOLOGICALLY ORDER the steps by their `needs` edges (Kahn's algorithm). Returns
/// the run order on success, or `Err(cycle_members)` — the ids that could not be
/// ordered because they lie on/behind a cycle — on failure. PURE. Dangling `needs`
/// (to a nonexistent step) are ignored here (they are flagged as [`DiagKind::MissingNeed`]
/// by [`check`]); only real edges among existing steps constrain the order. Ties break
/// on file order, so the plan render is deterministic.
pub fn topological_order(rb: &Runbook) -> Result<Vec<String>, Vec<String>> {
    let ids: HashSet<&str> = rb.steps.iter().map(|s| s.id.as_str()).collect();
    // indegree[id] = number of existing-step predecessors.
    let mut indegree: HashMap<&str, usize> = rb.steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    // adjacency: predecessor -> its dependents, in file order.
    let mut deps: HashMap<&str, Vec<&str>> = HashMap::new();
    for s in &rb.steps {
        // dedup a step's needs so a doubled edge does not inflate the indegree.
        let mut seen: HashSet<&str> = HashSet::new();
        for n in &s.needs {
            let n = n.as_str();
            if ids.contains(n) && n != s.id && seen.insert(n) {
                *indegree.get_mut(s.id.as_str()).expect("id seeded") += 1;
                deps.entry(n).or_default().push(s.id.as_str());
            }
        }
    }
    // Seed the queue with indegree-0 steps in FILE order (stable ties).
    let mut queue: VecDeque<&str> = rb
        .steps
        .iter()
        .map(|s| s.id.as_str())
        .filter(|id| indegree[id] == 0)
        .collect();
    let mut order: Vec<String> = Vec::with_capacity(rb.steps.len());
    while let Some(id) = queue.pop_front() {
        order.push(id.to_string());
        if let Some(children) = deps.get(id) {
            for &c in children {
                let e = indegree.get_mut(c).expect("child seeded");
                *e -= 1;
                if *e == 0 {
                    queue.push_back(c);
                }
            }
        }
    }
    if order.len() == rb.steps.len() {
        Ok(order)
    } else {
        // The unresolved steps are exactly those still carrying indegree > 0.
        let mut cycle: Vec<String> = rb
            .steps
            .iter()
            .map(|s| s.id.as_str())
            .filter(|id| indegree[id] > 0)
            .map(str::to_string)
            .collect();
        cycle.sort();
        Err(cycle)
    }
}

// ---------------------------------------------------------------------------
// Plan render (PURE) — the whole DAG, its data flow, and which steps PARK
// ---------------------------------------------------------------------------

/// Where one `with` parameter's value comes from — for the plan render (values
/// themselves are NEVER surfaced, only the source shape, so the render is secret-free).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSource {
    /// A literal of this type (the value itself is omitted from the plan).
    Literal(Ty),
    /// A data edge from `producer.output`.
    Edge { producer: String, output: String },
    /// A malformed reference (surfaced honestly).
    Bad(String),
}

/// One step as it will run: its capability, whether it will PARK, and where each input
/// comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub id: String,
    pub uses: String,
    /// True iff the capability is consequential — this step PARKS for a fresh spoken
    /// confirm at run time.
    pub will_park: bool,
    /// Whether the capability resolved in the registry at all.
    pub known: bool,
    /// Each `with` parameter and where its value comes from (sorted by param name).
    pub inputs: Vec<(String, InputSource)>,
    /// The output this step publishes, if any.
    pub produces: Option<String>,
}

/// The rendered plan of a whole runbook — the topological run order, each step with
/// its data flow + PARK verdict, and the diagnostics. Produced BEFORE anything runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub name: String,
    /// The steps in topological run order (or, if the DAG has a cycle, file order — the
    /// cycle is reported in `diagnostics` and [`run`] refuses).
    pub steps: Vec<PlanStep>,
    /// How many steps will PARK (consequential steps).
    pub park_count: usize,
    /// The LSP-style diagnostics (a superset of [`check`]).
    pub diagnostics: Vec<Diagnostic>,
}

impl Plan {
    /// Is this plan safe to run — i.e. free of [`Severity::Error`] diagnostics?
    pub fn is_runnable(&self) -> bool {
        !has_errors(&self.diagnostics)
    }
}

/// Render the whole runbook as an inspectable [`Plan`]: the topological run order (file
/// order if the DAG has a cycle — flagged, and [`run`] refuses), each step's data flow,
/// and which steps will PARK. PURE — this is what a `runbook_plan` op shows the user
/// BEFORE anything runs. The render carries NO input VALUES (only source shapes +
/// types), so it is secret-free.
pub fn plan(rb: &Runbook, reg: &Registry) -> Plan {
    let diagnostics = check(rb, reg);
    let order = topological_order(rb).unwrap_or_else(|_| rb.steps.iter().map(|s| s.id.clone()).collect());

    let mut steps = Vec::with_capacity(rb.steps.len());
    let mut park_count = 0;
    for id in &order {
        let Some(s) = rb.step(id) else { continue };
        let cap = reg.get(&s.uses);
        let will_park = matches!(cap.map(|c| c.privilege), Some(Privilege::Consequential));
        if will_park {
            park_count += 1;
        }
        let mut inputs: Vec<(String, InputSource)> = s
            .with
            .iter()
            .map(|(k, e)| {
                let src = match e {
                    Expr::Literal { ty, .. } => InputSource::Literal(*ty),
                    Expr::Ref { producer, output } => InputSource::Edge {
                        producer: producer.clone(),
                        output: output.clone(),
                    },
                    Expr::BadRef { raw } => InputSource::Bad(raw.clone()),
                };
                (k.clone(), src)
            })
            .collect();
        inputs.sort_by(|a, b| a.0.cmp(&b.0));
        steps.push(PlanStep {
            id: s.id.clone(),
            uses: s.uses.clone(),
            will_park,
            known: cap.is_some(),
            inputs,
            produces: s.out.clone(),
        });
    }
    Plan { name: rb.name.clone(), steps, park_count, diagnostics }
}

/// Emit the secret-free `runbook.plan` telemetry frame for a rendered plan (step ids,
/// capability names, per-step PARK verdict + data-edge shape, diagnostic counts — NEVER
/// an input value). Thin effectful wrapper over the pure [`plan`]; called by the
/// eventual `runbook_plan` op wiring.
pub fn emit_plan(plan: &Plan) {
    let steps: Vec<Value> = plan
        .steps
        .iter()
        .map(|s| {
            json!({
                "id": s.id,
                "uses": s.uses,
                "will_park": s.will_park,
                "known": s.known,
                "edges": s.inputs.iter().filter_map(|(k, src)| match src {
                    InputSource::Edge { producer, output } => Some(json!({"param": k, "from": format!("{producer}.{output}")})),
                    _ => None,
                }).collect::<Vec<_>>(),
                "produces": s.produces,
            })
        })
        .collect();
    crate::telemetry::emit(
        "system",
        "runbook.plan",
        json!({
            "name": plan.name,
            "step_count": plan.steps.len(),
            "park_count": plan.park_count,
            "errors": plan.diagnostics.iter().filter(|d| d.severity == Severity::Error).count(),
            "warnings": plan.diagnostics.iter().filter(|d| d.severity == Severity::Warning).count(),
            "runnable": plan.is_runnable(),
            "steps": steps,
        }),
    );
}

/// The secret-free `[runbook]` subsystem status frame for the HUD (emitted once at
/// startup like every other subsystem's `.status`): whether the subsystem is enabled
/// and its step bound. Carries no runbook content.
pub fn status_frame(enabled: bool, max_steps: usize) -> Value {
    json!({
        "enabled": enabled,
        "max_steps": max_steps,
        "honesty": "OFF by default; a runbook carries no authority — every consequential \
                    step PARKS for a fresh spoken confirmation at run time, one at a time.",
    })
}

// ---------------------------------------------------------------------------
// Executor (the gate-honoring core, generic over an injected router)
// ---------------------------------------------------------------------------

/// A step resolved for execution: its id, the capability, whether it is consequential,
/// and its input map with every `${ref}` substituted for the producer's actual output
/// value. This is what the [`RunbookRouter`] re-issues through the live router + gate.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedStep {
    pub id: String,
    pub uses: String,
    pub consequential: bool,
    pub input: serde_json::Map<String, Value>,
}

/// What the router did with one step. The executor NEVER fabricates any of these — each
/// is exactly what the injected router reported.
#[derive(Debug, Clone, PartialEq)]
pub enum StepResult {
    /// A benign step executed live and produced this output value.
    Produced(Value),
    /// A consequential step PARKED for a fresh spoken confirmation (the gate fired) —
    /// it produced NO output and nothing executed. The string is the spoken prompt.
    Parked(String),
    /// The router refused the step (master switch off / lockdown / voice-id) — nothing
    /// executed and no output was produced.
    Refused(String),
}

/// The router seam [`run`] re-issues each step through. The PRODUCTION implementation
/// translates a [`ResolvedStep`] into a tool/skill/agent invocation and routes it
/// through the SAME `anthropic::execute_tool` + gate path a live tool call takes — so a
/// consequential step PARKS for a fresh spoken confirm. Tests inject a mock that models
/// the gate. Each call routes EXACTLY ONE step; [`run`] never batches, so a
/// consequential step re-hits the gate FRESH every time.
pub trait RunbookRouter: Send + Sync {
    fn route_step<'a>(
        &'a self,
        step: &'a ResolvedStep,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = StepResult> + Send + 'a>>;
}

/// The final classification of a step after the walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// Routed and produced an output.
    Done,
    /// Routed and PARKED for a fresh spoken confirm (consequential) — nothing ran.
    Parked,
    /// Routed and refused by the gate/master-switch/lockdown — nothing ran.
    Refused,
    /// NOT routed: an input `${ref}` was never produced (the producer parked / was
    /// refused / was itself blocked). The executor refuses to route a step on a value
    /// that was never actually produced — no fabrication.
    Blocked,
}

/// One step's outcome from a [`run`] walk.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    pub id: String,
    pub uses: String,
    pub outcome: RunOutcome,
    /// A human-facing detail (the park prompt, the refusal reason, or the blocked
    /// dependency).
    pub detail: String,
}

/// The result of a whole [`run`] walk.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub name: String,
    /// Per-step outcomes in the topological run order actually walked.
    pub steps: Vec<StepOutcome>,
    /// True when the runbook was UNSOUND (had error diagnostics) and so NOTHING was
    /// routed — [`run`] refuses to execute a runbook that does not typecheck.
    pub refused_unsound: bool,
}

/// EXECUTE a runbook: walk the DAG in topological order and re-issue EACH step through
/// the injected [`RunbookRouter`], one at a time.
///
/// SAFETY (mirrors [`crate::macros::replay`]):
///  * A runbook with ANY [`Severity::Error`] diagnostic is REFUSED whole — nothing is
///    routed (`refused_unsound = true`). An unsound DAG never runs.
///  * Each step routes FRESH through the live router + gate. A consequential step
///    PARKS (the router returns [`StepResult::Parked`]); it is NEVER auto-executed,
///    NEVER pre-approved, NEVER batched with another step.
///  * A parked / refused step produces NO output. A downstream step that consumes that
///    output is BLOCKED — the executor NEVER fabricates a missing value to keep the DAG
///    flowing. Data flow passes values, never authority.
///
/// The executor itself calls nothing consequential; it only asks the injected router to
/// route each step, and records exactly what the router reports.
pub async fn run(rb: &Runbook, reg: &Registry, router: &dyn RunbookRouter) -> RunReport {
    let diags = check(rb, reg);
    if has_errors(&diags) {
        return RunReport { name: rb.name.clone(), steps: Vec::new(), refused_unsound: true };
    }
    // Safe: no error diagnostics => the DAG is acyclic, so topo order exists.
    let order = topological_order(rb).unwrap_or_else(|_| rb.steps.iter().map(|s| s.id.clone()).collect());

    // Outputs actually produced by completed steps (id -> value). A parked / refused /
    // blocked step NEVER writes here, so a downstream ref to it stays unresolved.
    let mut produced: HashMap<String, Value> = HashMap::new();
    let mut outcomes = Vec::with_capacity(order.len());

    for id in &order {
        let Some(s) = rb.step(id) else { continue };
        let consequential =
            matches!(reg.get(&s.uses).map(|c| c.privilege), Some(Privilege::Consequential));

        // Resolve inputs: substitute each ${producer.output} with the producer's ACTUAL
        // produced value. If any referenced value is absent, the step is BLOCKED (we
        // never route it on a fabricated input).
        let mut input = serde_json::Map::new();
        let mut blocked_on: Option<String> = None;
        for (k, expr) in &s.with {
            match expr {
                Expr::Literal { value, .. } => {
                    input.insert(k.clone(), value.clone());
                }
                Expr::Ref { producer, .. } => match produced.get(producer) {
                    Some(v) => {
                        input.insert(k.clone(), v.clone());
                    }
                    None => {
                        blocked_on = Some(producer.clone());
                        break;
                    }
                },
                // A BadRef can only appear in an unsound runbook, already refused above.
                Expr::BadRef { raw } => {
                    blocked_on = Some(raw.clone());
                    break;
                }
            }
        }

        if let Some(dep) = blocked_on {
            outcomes.push(StepOutcome {
                id: s.id.clone(),
                uses: s.uses.clone(),
                outcome: RunOutcome::Blocked,
                detail: format!("blocked: dependency \"{dep}\" produced no output (it parked, was refused, or was itself blocked)"),
            });
            continue;
        }

        let resolved = ResolvedStep {
            id: s.id.clone(),
            uses: s.uses.clone(),
            consequential,
            input,
        };
        // Route THIS step FRESH through the live router + gate. Whatever it does —
        // execute it (benign), PARK it (consequential), or refuse it — is recorded; we
        // never short-circuit the gate.
        let (outcome, detail) = match router.route_step(&resolved).await {
            StepResult::Produced(v) => {
                if s.out.is_some() {
                    produced.insert(s.id.clone(), v);
                }
                (RunOutcome::Done, "done".to_string())
            }
            StepResult::Parked(prompt) => (RunOutcome::Parked, prompt),
            StepResult::Refused(reason) => (RunOutcome::Refused, reason),
        };
        outcomes.push(StepOutcome { id: s.id.clone(), uses: s.uses.clone(), outcome, detail });
    }

    RunReport { name: rb.name.clone(), steps: outcomes, refused_unsound: false }
}

/// Emit the secret-free `runbook.run` telemetry frame for a completed walk (step ids +
/// capability names + per-step outcome — NEVER an input/output value). Thin effectful
/// wrapper; called by the eventual `runbook_run` op wiring.
pub fn emit_run(report: &RunReport) {
    let count = |o: RunOutcome| report.steps.iter().filter(|s| s.outcome == o).count();
    crate::telemetry::emit(
        "system",
        "runbook.run",
        json!({
            "name": report.name,
            "refused_unsound": report.refused_unsound,
            "step_count": report.steps.len(),
            "done": count(RunOutcome::Done),
            "parked": count(RunOutcome::Parked),
            "refused": count(RunOutcome::Refused),
            "blocked": count(RunOutcome::Blocked),
            "steps": report.steps.iter().map(|s| json!({
                "id": s.id,
                "uses": s.uses,
                "outcome": match s.outcome {
                    RunOutcome::Done => "done",
                    RunOutcome::Parked => "parked",
                    RunOutcome::Refused => "refused",
                    RunOutcome::Blocked => "blocked",
                },
            })).collect::<Vec<_>>(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // A compact registry for the parser/checker tests: one benign read tool, one pure
    // skill, and one consequential poster — enough to exercise every diagnostic.
    fn reg() -> Registry {
        Registry::with(vec![
            Capability {
                name: "list_prs",
                kind: CapKind::Tool,
                privilege: Privilege::Benign,
                params: vec![
                    Param { name: "owner", ty: Ty::Str, required: true },
                    Param { name: "repo", ty: Ty::Str, required: true },
                    Param { name: "max", ty: Ty::Int, required: false },
                ],
                out: Ty::List,
            },
            Capability {
                name: "summarize",
                kind: CapKind::Skill,
                privilege: Privilege::Benign,
                params: vec![Param { name: "text", ty: Ty::Str, required: true }],
                out: Ty::Str,
            },
            Capability {
                name: "post",
                kind: CapKind::Tool,
                privilege: Privilege::Consequential,
                params: vec![
                    Param { name: "channel", ty: Ty::Str, required: true },
                    Param { name: "text", ty: Ty::Str, required: true },
                ],
                out: Ty::Str,
            },
        ])
    }

    fn kinds(diags: &[Diagnostic]) -> Vec<DiagKind> {
        diags.iter().map(|d| d.kind).collect()
    }

    // ---- parser: a valid runbook parses to a typed DAG ----------------------

    #[test]
    fn parse_valid_runbook_yields_typed_dag_with_literals_and_refs() {
        // A type-CLEAN pipeline: an Int + Str literal read step, and a Str->Str data
        // edge (summarize outputs a string, fed into summarize's `text` string param).
        let src = r#"
            [runbook]
            name = "morning"

            [[step]]
            id = "prs"
            uses = "list_prs"
            with = { owner = "darwin-capani", repo = "darwin", max = 5 }
            out = "prs"

            [[step]]
            id = "note"
            uses = "summarize"
            with = { text = "raw notes" }
            out = "summary"

            [[step]]
            id = "again"
            uses = "summarize"
            needs = ["note"]
            with = { text = "${note.summary}" }
            out = "final"
        "#;
        let rb = parse(src).expect("valid runbook parses");
        assert_eq!(rb.name, "morning");
        assert_eq!(rb.steps.len(), 3);

        let prs = rb.step("prs").unwrap();
        assert_eq!(prs.uses, "list_prs");
        assert_eq!(prs.out.as_deref(), Some("prs"));
        // Literals are typed from their TOML value.
        assert_eq!(prs.with["owner"], Expr::Literal { ty: Ty::Str, value: json!("darwin-capani") });
        assert_eq!(prs.with["max"], Expr::Literal { ty: Ty::Int, value: json!(5) });

        // The data edge parsed structurally as a Ref (not a string literal).
        let again = rb.step("again").unwrap();
        assert_eq!(
            again.with["text"],
            Expr::Ref { producer: "note".into(), output: "summary".into() }
        );
        assert_eq!(again.needs, vec!["note".to_string()]);

        // The Str->Str edge typechecks clean; the whole runbook has no errors.
        assert!(!has_errors(&check(&rb, &reg())), "the valid runbook has no errors");
    }

    #[test]
    fn parse_rejects_structural_failures() {
        // Missing required `uses`.
        assert!(parse("[runbook]\nname=\"x\"\n[[step]]\nid=\"a\"\n").is_err());
        // Empty name.
        assert!(parse("[runbook]\nname=\"\"\n").is_err());
        // Malformed TOML.
        assert!(parse("not = = toml").is_err());
        // An unknown step key is a structural (deny_unknown_fields) error.
        assert!(parse("[runbook]\nname=\"x\"\n[[step]]\nid=\"a\"\nuses=\"list_prs\"\nbogus=1\n").is_err());
    }

    // ---- diagnostics: each flagged kind -------------------------------------

    #[test]
    fn diagnostic_unknown_capability() {
        let src = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "no_such_tool"
        "#;
        let d = check(&parse(src).unwrap(), &reg());
        assert!(kinds(&d).contains(&DiagKind::UnknownCapability), "got {:?}", kinds(&d));
        assert!(has_errors(&d));
    }

    #[test]
    fn diagnostic_missing_needs() {
        let src = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "summarize"
            needs = ["ghost"]
            with = { text = "hi" }
        "#;
        let d = check(&parse(src).unwrap(), &reg());
        assert!(kinds(&d).contains(&DiagKind::MissingNeed), "got {:?}", kinds(&d));
    }

    #[test]
    fn diagnostic_cycle() {
        let src = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "summarize"
            needs = ["b"]
            with = { text = "t" }
            out = "o"
            [[step]]
            id = "b"
            uses = "summarize"
            needs = ["a"]
            with = { text = "t" }
            out = "o"
        "#;
        let rb = parse(src).unwrap();
        assert!(topological_order(&rb).is_err(), "a<->b is a cycle");
        let d = check(&rb, &reg());
        assert!(kinds(&d).contains(&DiagKind::Cycle), "got {:?}", kinds(&d));
    }

    #[test]
    fn diagnostic_type_mismatch_literal_and_edge() {
        // Literal mismatch: `max` expects Int, given a string.
        let lit = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "list_prs"
            with = { owner = "o", repo = "r", max = "five" }
        "#;
        let d = check(&parse(lit).unwrap(), &reg());
        assert!(kinds(&d).contains(&DiagKind::TypeMismatch), "literal: got {:?}", kinds(&d));

        // Edge mismatch: `list_prs` outputs a List, fed into `summarize.text` (a Str).
        let edge = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "list_prs"
            with = { owner = "o", repo = "r" }
            out = "prs"
            [[step]]
            id = "b"
            uses = "summarize"
            needs = ["a"]
            with = { text = "${a.prs}" }
        "#;
        let d = check(&parse(edge).unwrap(), &reg());
        assert!(kinds(&d).contains(&DiagKind::TypeMismatch), "edge: got {:?}", kinds(&d));
    }

    #[test]
    fn diagnostic_unknown_and_missing_params() {
        let src = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "list_prs"
            with = { owner = "o", bogus = 1 }
        "#;
        let d = check(&parse(src).unwrap(), &reg());
        assert!(kinds(&d).contains(&DiagKind::UnknownParam), "got {:?}", kinds(&d));
        assert!(kinds(&d).contains(&DiagKind::MissingParam), "missing `repo`: got {:?}", kinds(&d));
    }

    #[test]
    fn diagnostic_undeclared_data_dep_and_bad_ref() {
        // A ref whose producer exists but is not in `needs`.
        let undeclared = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "list_prs"
            with = { owner = "o", repo = "r" }
            out = "prs"
            [[step]]
            id = "b"
            uses = "summarize"
            with = { text = "${a.prs}" }
        "#;
        let d = check(&parse(undeclared).unwrap(), &reg());
        assert!(kinds(&d).contains(&DiagKind::UndeclaredDataDep), "got {:?}", kinds(&d));

        // A malformed ${...}.
        let bad = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "summarize"
            with = { text = "${broken}" }
        "#;
        let rb = parse(bad).unwrap();
        assert!(matches!(rb.step("a").unwrap().with["text"], Expr::BadRef { .. }));
        let d = check(&rb, &reg());
        assert!(kinds(&d).contains(&DiagKind::UndeclaredDataDep), "bad ref: got {:?}", kinds(&d));
    }

    #[test]
    fn diagnostic_over_privileged_is_a_warning_not_an_error() {
        let src = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "post"
            with = { channel = "eng", text = "hi" }
        "#;
        let d = check(&parse(src).unwrap(), &reg());
        let op: Vec<&Diagnostic> = d.iter().filter(|x| x.kind == DiagKind::OverPrivileged).collect();
        assert_eq!(op.len(), 1, "the consequential step is flagged once: {:?}", kinds(&d));
        assert_eq!(op[0].severity, Severity::Warning, "over-privileged is advisory, not fatal");
        // A lone consequential step is a VALID runbook (it just parks) — no errors.
        assert!(!has_errors(&d), "over-privileged alone is not an error: {:?}", d);
    }

    #[test]
    fn diagnostic_duplicate_step_id() {
        let src = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "a"
            uses = "summarize"
            with = { text = "t" }
            [[step]]
            id = "a"
            uses = "summarize"
            with = { text = "t" }
        "#;
        let d = check(&parse(src).unwrap(), &reg());
        assert!(kinds(&d).contains(&DiagKind::DuplicateStepId), "got {:?}", kinds(&d));
    }

    // ---- topological order --------------------------------------------------

    #[test]
    fn topological_order_respects_needs_edges() {
        let src = r#"
            [runbook]
            name = "x"
            [[step]]
            id = "c"
            uses = "summarize"
            needs = ["b"]
            with = { text = "${b.o}" }
            out = "o"
            [[step]]
            id = "a"
            uses = "list_prs"
            with = { owner = "o", repo = "r" }
            out = "prs"
            [[step]]
            id = "b"
            uses = "summarize"
            needs = ["a"]
            with = { text = "${a.prs}" }
            out = "o"
        "#;
        let rb = parse(src).unwrap();
        let order = topological_order(&rb).expect("acyclic");
        let pos = |id: &str| order.iter().position(|x| x == id).unwrap();
        assert!(pos("a") < pos("b"), "a before b: {order:?}");
        assert!(pos("b") < pos("c"), "b before c: {order:?}");
    }

    // ---- plan render: which steps PARK --------------------------------------

    #[test]
    fn plan_marks_which_steps_park_and_shows_data_flow() {
        let src = r#"
            [runbook]
            name = "brief-and-post"
            [[step]]
            id = "prs"
            uses = "list_prs"
            with = { owner = "o", repo = "r" }
            out = "prs"
            [[step]]
            id = "post_it"
            uses = "post"
            needs = ["sum"]
            with = { channel = "eng", text = "${sum.summary}" }
            [[step]]
            id = "sum"
            uses = "summarize"
            needs = ["prs"]
            with = { text = "hardcoded" }
            out = "summary"
        "#;
        let rb = parse(src).unwrap();
        let p = plan(&rb, &reg());

        // The consequential poster is the one and only PARK.
        assert_eq!(p.park_count, 1, "exactly one consequential step parks");
        let post = p.steps.iter().find(|s| s.id == "post_it").unwrap();
        assert!(post.will_park, "the poster parks");
        let prs = p.steps.iter().find(|s| s.id == "prs").unwrap();
        assert!(!prs.will_park, "the read step does not park");

        // The plan is in topological order (the poster runs after its `sum` producer).
        let order: Vec<&str> = p.steps.iter().map(|s| s.id.as_str()).collect();
        let pos = |id: &str| order.iter().position(|x| *x == id).unwrap();
        assert!(pos("prs") < pos("sum"), "prs before sum: {order:?}");
        assert!(pos("sum") < pos("post_it"), "sum before poster: {order:?}");

        // The data edge is surfaced (sum.summary -> the poster's text) — VALUE omitted.
        let edge = post.inputs.iter().find(|(k, _)| k == "text").unwrap();
        assert_eq!(
            edge.1,
            InputSource::Edge { producer: "sum".into(), output: "summary".into() }
        );
    }

    // ---- the no-authority invariant -----------------------------------------

    /// A mock router modeling the gate: a benign step executes and produces a value; a
    /// consequential step PARKS (the gate fired) and produces NOTHING. It records the
    /// order it was called so a test can prove steps route ONE AT A TIME and each
    /// consequential step re-gates FRESH.
    struct MockRouter {
        routed: std::sync::Mutex<Vec<String>>,
    }
    impl MockRouter {
        fn new() -> Self {
            MockRouter { routed: std::sync::Mutex::new(Vec::new()) }
        }
        fn routed(&self) -> Vec<String> {
            self.routed.lock().unwrap().clone()
        }
    }
    impl RunbookRouter for MockRouter {
        fn route_step<'a>(
            &'a self,
            step: &'a ResolvedStep,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = StepResult> + Send + 'a>> {
            Box::pin(async move {
                self.routed.lock().unwrap().push(step.id.clone());
                if step.consequential {
                    // The gate fired FRESH: parked for a spoken yes; nothing executed.
                    StepResult::Parked(format!("say 'confirm' to run {}", step.uses))
                } else {
                    // A benign step runs and yields its output value.
                    StepResult::Produced(json!(format!("out-of-{}", step.id)))
                }
            })
        }
    }

    #[tokio::test]
    async fn consequential_step_parks_fresh_and_is_never_batch_preapproved() {
        // prs (benign, produces) -> sum (benign, produces) -> post (consequential).
        let src = r#"
            [runbook]
            name = "no-authority"
            [[step]]
            id = "prs"
            uses = "list_prs"
            with = { owner = "o", repo = "r" }
            out = "prs"
            [[step]]
            id = "sum"
            uses = "summarize"
            needs = ["prs"]
            with = { text = "static" }
            out = "summary"
            [[step]]
            id = "post"
            uses = "post"
            needs = ["sum"]
            with = { channel = "eng", text = "${sum.summary}" }
        "#;
        let rb = parse(src).unwrap();
        let router = MockRouter::new();
        let report = run(&rb, &reg(), &router).await;

        assert!(!report.refused_unsound, "the runbook is sound (over-priv is only a warning)");
        // Each step was routed EXACTLY once, in dependency order — NOT batched.
        assert_eq!(router.routed(), vec!["prs", "sum", "post"], "routed one at a time, in order");

        let by = |id: &str| report.steps.iter().find(|s| s.id == id).unwrap();
        assert_eq!(by("prs").outcome, RunOutcome::Done);
        assert_eq!(by("sum").outcome, RunOutcome::Done);
        // The consequential step PARKED FRESH through the gate — it did NOT execute and
        // was NOT pre-approved by the runbook.
        assert_eq!(by("post").outcome, RunOutcome::Parked, "the consequential step parks");
        assert!(by("post").detail.to_lowercase().contains("confirm"), "it parked for a spoken yes");
    }

    #[tokio::test]
    async fn a_parked_producer_blocks_its_consumer_never_fabricates_a_value() {
        // post (consequential, parks & produces NOTHING) -> after (benign, consumes
        // post's output). `after` must be BLOCKED, never routed on a fabricated value.
        let src = r#"
            [runbook]
            name = "no-fabrication"
            [[step]]
            id = "post"
            uses = "post"
            with = { channel = "eng", text = "hi" }
            out = "receipt"
            [[step]]
            id = "after"
            uses = "summarize"
            needs = ["post"]
            with = { text = "${post.receipt}" }
        "#;
        let rb = parse(src).unwrap();
        let router = MockRouter::new();
        let report = run(&rb, &reg(), &router).await;

        let by = |id: &str| report.steps.iter().find(|s| s.id == id).unwrap();
        assert_eq!(by("post").outcome, RunOutcome::Parked, "the producer parks");
        assert_eq!(by("after").outcome, RunOutcome::Blocked, "its consumer is blocked, not fabricated");
        // The blocked step was NEVER routed (no fabricated input reached the router).
        assert_eq!(router.routed(), vec!["post"], "only the parked producer was routed");
    }

    #[tokio::test]
    async fn two_consequential_steps_each_park_independently_no_shared_approval() {
        // Two independent consequential steps: BOTH must park; one confirm can never
        // cover the other (they are routed separately and each re-gates).
        let src = r#"
            [runbook]
            name = "two-parks"
            [[step]]
            id = "post_a"
            uses = "post"
            with = { channel = "chan-a", text = "one" }
            [[step]]
            id = "post_b"
            uses = "post"
            with = { channel = "chan-b", text = "two" }
        "#;
        let rb = parse(src).unwrap();
        let router = MockRouter::new();
        let report = run(&rb, &reg(), &router).await;

        assert_eq!(report.steps.iter().filter(|s| s.outcome == RunOutcome::Parked).count(), 2, "both park");
        assert_eq!(router.routed().len(), 2, "each was routed separately (never batched)");
    }

    #[tokio::test]
    async fn run_refuses_an_unsound_runbook_whole_routing_nothing() {
        // An unknown capability => an error diagnostic => the whole runbook is refused
        // and NOTHING is routed.
        let src = r#"
            [runbook]
            name = "unsound"
            [[step]]
            id = "a"
            uses = "no_such_tool"
        "#;
        let rb = parse(src).unwrap();
        let router = MockRouter::new();
        let report = run(&rb, &reg(), &router).await;
        assert!(report.refused_unsound, "an unsound runbook is refused whole");
        assert!(report.steps.is_empty(), "no step outcomes for a refused runbook");
        assert!(router.routed().is_empty(), "nothing was routed for an unsound runbook");
    }

    // ---- registry integrity: privilege tracks the gate ----------------------

    #[test]
    fn builtin_registry_privilege_tracks_the_gate() {
        let reg = Registry::builtin();
        // Every tool the gate treats as consequential is Consequential here too.
        for name in ["slack_post_message", "gmail_send", "github_open_pr"] {
            let cap = reg.get(name).unwrap_or_else(|| panic!("{name} seeded"));
            assert_eq!(cap.privilege, Privilege::Consequential, "{name} must be consequential");
            assert!(crate::confirm::is_consequential_tool(name), "{name} is gate-consequential");
        }
        // A read-only tool stays benign.
        let ro = reg.get("github_list_prs").unwrap();
        assert_eq!(ro.privilege, Privilege::Benign);
        assert!(!crate::confirm::is_consequential_tool("github_list_prs"));
    }

    #[test]
    fn status_frame_is_off_and_honest() {
        let f = status_frame(false, DEFAULT_MAX_STEPS);
        assert_eq!(f["enabled"], json!(false));
        assert_eq!(f["max_steps"], json!(DEFAULT_MAX_STEPS));
        assert!(f["honesty"].as_str().unwrap().to_lowercase().contains("park"));
    }
}
