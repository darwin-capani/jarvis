//! IPC loop (SPEC §6, runtime contract) — the JSONL op dispatcher + socket loop.
//!
//! Responsibility: speak the daemon's app-host protocol (`daemon/src/apps.rs`)
//! over the per-app Unix socket. The app learns its socket + token from the env
//! (NEVER argv):
//!   - `JARVIS_APP_SOCKET` — abs path of `state/ipc/apps/silicon-canvas.sock`
//!   - `JARVIS_APP_TOKEN`  — the capability token to stamp on every outbound line
//!   - `JARVIS_APP_NAME`   — "silicon-canvas"
//!
//! Inbound (host → app), one JSON object per line:
//!   - `{"type":"start"|"refresh"|"stop"}` — host control verbs
//!     ([`crate::ops::HostControl`]); `stop` exits cleanly.
//!   - `{"op":"…", …}` — Silicon-Canvas ops ([`crate::ops::Op`], SPEC §6).
//!   [`parse_command`] classifies each line by presence of `op` vs `type`.
//!
//! Outbound (app → host), stamped with the token on EVERY line via
//! [`crate::ops::OutboundLine::telemetry`]; the host relays `type:"items"` lines
//! whose `data.topic` is a declared topic (`canvas.render_ms` /
//! `canvas.viewport` / `canvas.selection`).
//!
//! Design (testability): the op dispatcher is a PURE function over the in-memory
//! [`AppState`] — [`dispatch`] takes `&mut AppState` and returns the telemetry to
//! emit, with no socket involved, so the unit tests drive the whole op surface
//! headlessly. [`run`] is the thin socket shell that reads lines, calls
//! [`dispatch`], and writes the token-stamped telemetry lines back.
//!
//! Safety contract (mirrors global-scan's discipline):
//!   - Stamp the token from the env on every outbound line; never log the token.
//!   - `project.open` paths MUST stay inside the sandbox `projects/` /
//!     `libraries/` roots. This is TWO passes: a FS-free lexical pass
//!     ([`resolve_project_path`]) rejects `..` / absolute / root before any disk
//!     touch, and a FS-aware containment guard ([`confine_real_path`]) at the read
//!     site canonicalizes the resolved path + roots and rejects anything whose
//!     REAL target escapes — closing the symlink-inside-projects/ hole the lexical
//!     pass alone cannot. Both reject with
//!     [`crate::error::CanvasError::PathNotPermitted`] before `read_to_string`
//!     (the seatbelt profile is the real boundary; this is defense in depth + a
//!     clean error instead of an EPERM surprise). A file over
//!     [`MAX_PROJECT_FILE_BYTES`] is likewise rejected before the read.
//!   - Never panic on a malformed line; classify it to a clean
//!     [`crate::error::CanvasError::Protocol`] and drop it.
//!
//! Engine wiring: `project.open` runs the REAL import pipeline —
//! [`crate::parser::parse_document`] → [`crate::graph::Graph::build`] →
//! [`Graph::apply_nets`] (writes `net_id` back into the scene) → [`crate::erc::run`]
//! at import — and stores BOTH the [`Scene`] and the [`crate::graph::Graph`] on the
//! [`AppState`]. `select.net`/`select.component`/`trace.*` then drive a
//! [`crate::trace::Tracer`] over that real graph (via `impl TraceGraph for Graph`),
//! and `erc.run` re-runs [`crate::erc::run`] on the stored scene. The dispatcher
//! stays PURE over the in-memory state (no socket), so the tests exercise the whole
//! pipeline headlessly against a real KiCad fixture.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};

use crate::error::{CanvasError, Result};
use crate::graph::Graph;
use crate::ops::{
    Command, FitTarget, HostControl, LayerVisibility, Op, OutboundLine, Selection, Telemetry,
    Viewport, ViewSet,
};
use crate::scene::{Aabb, HighlightFlag, Scene, SceneKind};
use crate::trace::Tracer;

/// The launch environment the daemon hands the app (`daemon/src/apps.rs`): the
/// per-app socket path + the capability token to stamp on every outbound line +
/// the app name. Read once at startup from the env (NEVER argv).
#[derive(Debug, Clone)]
pub struct AppEnv {
    /// Absolute path of `state/ipc/apps/silicon-canvas.sock`.
    pub socket_path: PathBuf,
    /// The capability token, stamped on every App→host line.
    pub token: String,
    /// This app's name ("silicon-canvas").
    pub name: String,
    /// The sandbox project root the `fs_read` roots resolve against. The daemon
    /// runs the child with `current_dir` = project root, so the default is the
    /// process cwd; `project.open` paths resolve under `<root>/apps/<name>`.
    pub project_root: PathBuf,
}

impl AppEnv {
    /// Read the launch env exactly as `apps.rs`/global-scan establish it. Returns
    /// [`CanvasError::Unauthorized`] when the socket or token is absent — the app
    /// only runs under the daemon (the binary's `main` maps this to exit code 2).
    pub fn from_env() -> Result<Self> {
        let socket_path = std::env::var_os("JARVIS_APP_SOCKET")
            .map(PathBuf::from)
            .ok_or(CanvasError::Unauthorized)?;
        let token = std::env::var("JARVIS_APP_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .ok_or(CanvasError::Unauthorized)?;
        let name =
            std::env::var("JARVIS_APP_NAME").unwrap_or_else(|_| "silicon-canvas".to_string());
        // The daemon launches the child with cwd = project root.
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Ok(AppEnv {
            socket_path,
            token,
            name,
            project_root,
        })
    }

    /// The directory `project.open` paths are confined to: `<root>/apps/<name>`.
    /// The manifest grants `fs_read` on `apps/silicon-canvas/{projects,libraries}`
    /// only, so a path is permitted iff it normalizes to something under this dir
    /// (and, in practice, under its `projects/`/`libraries/` subdirs — enforced by
    /// [`resolve_project_path`]).
    pub fn app_dir(&self) -> PathBuf {
        self.project_root.join("apps").join(&self.name)
    }
}

/// The in-memory app state the op dispatcher mutates. Owns the open [`Scene`],
/// the connectivity [`Graph`] built from it at import, the current [`Selection`]
/// (net/component/erc), the camera [`Viewport`], and the [`Tracer`] selection /
/// trace-mode state machine. Built fresh on launch; `project.open` replaces the
/// scene + graph.
///
/// The dispatcher is PURE over this state (no socket), so the unit tests exercise
/// the full parser → graph → erc → trace pipeline headlessly with a real KiCad
/// fixture (no socket, no env).
#[derive(Debug, Default)]
pub struct AppState {
    /// The open document, or `None` before the first `project.open`.
    pub scene: Option<Scene>,
    /// The connectivity graph built from `scene` at import (SPEC §1), or `None`
    /// before the first `project.open`. Drives `select.net`/`trace.*` via the
    /// `TraceGraph` impl; net lookups (`net_by_name`/`nodes_on_net`/`pin_count`)
    /// resolve against the graph's net ids.
    pub graph: Option<Graph>,
    /// Absolute path of the open document (for diagnostics / `view.set fit`).
    pub open_path: Option<PathBuf>,
    /// The raw source of the open document, parked here so a re-parse needs no
    /// second file read (and as the confined source the parser consumed).
    pub open_source: Option<String>,
    /// The current selection summary published on `canvas.selection`.
    pub selection: Selection,
    /// The camera state published on `canvas.viewport`.
    pub viewport: ViewportState,
    /// The selection + trace state machine (SPEC §4) walking the real graph. Its
    /// `is_tracing()` is the single source of truth for trace mode.
    pub tracer: Tracer,
    /// The current trace front, mirrored from the [`Tracer`]'s last
    /// `start`/`step` so [`self_selection`] can republish it on `canvas.selection`
    /// (SPEC §4 via-flash / "step k of n"). `Some` only while tracing; cleared to
    /// `None` on `select.net`/`select.component`/`trace.stop` and at import. This
    /// is a pure echo of the [`StepInfo`] the Tracer already computed — the
    /// Tracer stays the single source of truth for the walk.
    pub trace_step: Option<crate::ops::TraceStep>,
}

/// The camera pose the dispatcher tracks for `canvas.viewport`. Scene-space
/// center + pixels-per-mm scale, plus the per-layer visibility the `layer.set` op
/// toggles. Kept separate from [`crate::ops::Viewport`] (the wire payload) so the
/// dispatcher can mutate it in place and snapshot a `Viewport` on change.
#[derive(Debug, Clone)]
pub struct ViewportState {
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    /// Layer name → visible, in declared order (PCB). Empty for a schematic.
    pub layers: Vec<LayerVisibility>,
}

impl Default for ViewportState {
    fn default() -> Self {
        ViewportState {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            layers: Vec::new(),
        }
    }
}

impl ViewportState {
    /// Snapshot the current pose as the `canvas.viewport` wire payload.
    fn snapshot(&self) -> Viewport {
        Viewport {
            x: self.x,
            y: self.y,
            scale: self.scale,
            layer_visibility: self.layers.clone(),
        }
    }
}

impl AppState {
    pub fn new() -> Self {
        AppState::default()
    }

    /// Borrow the open scene or fail with [`CanvasError::NoProjectOpen`].
    fn scene(&self) -> Result<&Scene> {
        self.scene.as_ref().ok_or(CanvasError::NoProjectOpen)
    }

    fn scene_mut(&mut self) -> Result<&mut Scene> {
        self.scene.as_mut().ok_or(CanvasError::NoProjectOpen)
    }

    /// Borrow the open scene + connectivity graph together, or fail with
    /// [`CanvasError::NoProjectOpen`]. The two are always set as a pair by
    /// `project.open`, so this is the accessor the trace/select ops use.
    fn scene_graph_mut(&mut self) -> Result<(&mut Scene, &Graph)> {
        match (self.scene.as_mut(), self.graph.as_ref()) {
            (Some(scene), Some(graph)) => Ok((scene, graph)),
            _ => Err(CanvasError::NoProjectOpen),
        }
    }
}

// ===========================================================================
// Inbound line classification.
// ===========================================================================

/// Classify one inbound JSONL line into a [`Command`]. A line carrying `"op"` is
/// a Silicon-Canvas op ([`Op`]); a line carrying `"type"` is a host control verb
/// ([`HostControl`]). Anything else — malformed JSON, neither field, an unknown
/// verb/op — is a clean [`CanvasError::Protocol`] (NEVER a panic), so the caller
/// drops the line and keeps serving.
///
/// `op` is checked first: a line with both fields is treated as an op (the SPEC
/// §6 op envelope is the richer form). The daemon's generic host only ever sends
/// the three control verbs; richer ops arrive on the same socket per SPEC §6.
pub fn parse_command(line: &str) -> Result<Command> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(CanvasError::protocol("empty line"));
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|e| CanvasError::protocol(format!("not JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| CanvasError::protocol("inbound line is not a JSON object"))?;

    if obj.contains_key("op") {
        // A Silicon-Canvas op. Deserialize via the tagged `Op` enum.
        let op: Op = serde_json::from_value(value)
            .map_err(|e| CanvasError::protocol(format!("unknown or malformed op: {e}")))?;
        return Ok(Command::Op(op));
    }
    if let Some(ty) = obj.get("type").and_then(|v| v.as_str()) {
        let ctrl = match ty {
            "start" => HostControl::Start,
            "refresh" => HostControl::Refresh,
            "stop" => HostControl::Stop,
            other => {
                return Err(CanvasError::protocol(format!(
                    "unknown host control verb {other:?}"
                )))
            }
        };
        return Ok(Command::Control(ctrl));
    }
    Err(CanvasError::protocol(
        "inbound line has neither an `op` nor a `type` field",
    ))
}

// ===========================================================================
// project.open path confinement (defense in depth over the seatbelt profile).
// ===========================================================================

/// Resolve + LEXICALLY confine a `project.open` path. The op's `path` is
/// interpreted relative to the app dir (`apps/silicon-canvas`); it MUST land
/// inside the `projects/` or `libraries/` subdir the manifest grants `fs_read`
/// on, and MUST NOT escape via `..` or an absolute path.
///
/// This pass is INTENTIONALLY lexical-only and filesystem-free (it touches no
/// disk and resolves no symlinks), so it can be unit-tested against a fake
/// `app_dir` with no I/O. Rejections (all → [`CanvasError::PathNotPermitted`],
/// before any filesystem touch):
///   - any `..` (parent) component anywhere in the path,
///   - an absolute path (would escape the app dir entirely),
///   - a normalized result not under `projects/` or `libraries/`.
///
/// A leading `projects/` or `libraries/` is accepted, as is a bare relative
/// filename (defaulted under `projects/`), so both `"projects/board.kicad_sch"`
/// and `"board.kicad_sch"` resolve to `.../projects/board.kicad_sch`. The file
/// extension must be a supported KiCad type or it is
/// [`CanvasError::UnsupportedFileType`].
///
/// IMPORTANT — confinement caveat: passing the lexical check does NOT prove the
/// real on-disk target is inside the sandbox. A symlink planted *inside*
/// `projects/` (e.g. `projects/evil.kicad_sch -> /etc/passwd`) has no `..` and is
/// not absolute, so it slips through here. The true containment guard
/// ([`confine_real_path`], canonicalize + `starts_with`) runs at the read site in
/// [`dispatch_project_open`] once the path exists — that is what actually keeps
/// `read_to_string` from following a symlink out of the sandbox.
pub fn resolve_project_path(app_dir: &Path, raw: &str) -> Result<PathBuf> {
    let req = Path::new(raw);

    // Absolute paths escape the app dir outright — reject.
    if req.is_absolute() {
        return Err(CanvasError::PathNotPermitted(req.to_path_buf()));
    }
    // Any parent/`..` component (or a Windows prefix / root) is a traversal
    // attempt — reject BEFORE resolving, so we never canonicalize an escape.
    for comp in req.components() {
        match comp {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(CanvasError::PathNotPermitted(req.to_path_buf()));
            }
            // `.` and normal components are fine.
            Component::CurDir | Component::Normal(_) => {}
        }
    }

    // Confine to projects/ or libraries/. A path already rooted at one of those
    // stays; a bare relative path defaults under projects/.
    let first = req
        .components()
        .find_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .unwrap_or_default();

    let confined: PathBuf = if first == "projects" || first == "libraries" {
        app_dir.join(req)
    } else {
        // Bare filename / sub-path → default into projects/.
        app_dir.join("projects").join(req)
    };

    // Extension gate (supported KiCad document/library types only).
    let ext = confined
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "kicad_sch" | "kicad_pcb" | "kicad_sym" | "kicad_mod" => {}
        _ => return Err(CanvasError::UnsupportedFileType(confined)),
    }

    Ok(confined)
}

/// The in-app cap on a single `project.open` source file. KiCad schematics /
/// boards are small text S-expressions — even a large multi-sheet board is a few
/// MB — so 16 MiB is comfortably above any real document while bounding the
/// in-process heap/parse cost of one open. An adversarial file over this is
/// rejected with [`CanvasError::FileTooLarge`] before it is read (the
/// seatbelt/supervisor is the outer boundary; this is the in-app backstop).
const MAX_PROJECT_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// REAL-path containment guard for the `project.open` read site (the FS-aware
/// complement to the FS-free [`resolve_project_path`]). Canonicalizes the
/// resolved target AND the two granted roots (`<app_dir>/projects` and
/// `<app_dir>/libraries`) — following any symlinks — then requires the real
/// target to live under one of the real roots. This is what stops a symlink
/// planted inside `projects/` (which passes the lexical check) from letting
/// `read_to_string` follow it out of the sandbox.
///
/// Both sides are canonicalized so a symlinked root (e.g. macOS `/tmp ->
/// /private/tmp`) does not produce a false reject: the target and the roots are
/// compared in the same fully-resolved namespace. The target must already exist
/// (canonicalize requires it); callers run this AFTER the `path.exists()` check.
/// Any failure to canonicalize, or a target under neither root, is
/// [`CanvasError::PathNotPermitted`] — the op is rejected before the read.
fn confine_real_path(app_dir: &Path, path: &Path) -> Result<()> {
    let real = path
        .canonicalize()
        .map_err(|_| CanvasError::PathNotPermitted(path.to_path_buf()))?;
    // Canonicalize each granted root; a root that does not exist simply grants
    // nothing (it cannot contain the target), so skip it rather than fail.
    let roots = ["projects", "libraries"];
    let permitted = roots.iter().any(|sub| {
        app_dir
            .join(sub)
            .canonicalize()
            .map(|root| real.starts_with(&root))
            .unwrap_or(false)
    });
    if permitted {
        Ok(())
    } else {
        // Report the requested path (not the resolved-out-of-sandbox target) so
        // the error never leaks where the symlink pointed.
        Err(CanvasError::PathNotPermitted(path.to_path_buf()))
    }
}

/// Infer the [`SceneKind`] from a confined document path's extension. `.kicad_pcb`
/// is a board; everything else supported is schematic-side.
fn scene_kind_for(path: &Path) -> SceneKind {
    match path.extension().and_then(|e| e.to_str()) {
        Some("kicad_pcb") => SceneKind::Pcb,
        _ => SceneKind::Schematic,
    }
}

// ===========================================================================
// Op dispatch — PURE over AppState (no socket; the tests drive this directly).
// ===========================================================================

/// Apply one [`Op`] to the in-memory [`AppState`] and return the telemetry the
/// host should relay (zero or more [`Telemetry`] payloads, in emit order). PURE:
/// no socket, no env — the unit tests call this with a hand-built scene.
///
/// Errors are returned, never panicked: an op on a missing net/component →
/// [`CanvasError::NoSuchEntity`]; an op before `project.open` →
/// [`CanvasError::NoProjectOpen`]; a trace step out of state →
/// [`CanvasError::TraceState`].
pub fn dispatch(state: &mut AppState, op: &Op, env: &AppEnv) -> Result<Vec<Telemetry>> {
    match op {
        Op::ProjectOpen { path } => dispatch_project_open(state, env, path),
        Op::ViewSet(view) => dispatch_view_set(state, view),
        Op::SelectNet { name } => dispatch_select_net(state, name),
        Op::SelectComponent { name } => dispatch_select_component(state, name),
        Op::TraceStart => dispatch_trace_start(state),
        Op::TraceStep => dispatch_trace_step(state),
        Op::TraceStop => dispatch_trace_stop(state),
        Op::LayerSet { layer, visible } => dispatch_layer_set(state, layer, *visible),
        Op::ErcRun => dispatch_erc_run(state),
    }
}

fn dispatch_project_open(state: &mut AppState, env: &AppEnv, raw: &str) -> Result<Vec<Telemetry>> {
    // LEXICAL confinement runs FIRST, before any filesystem touch: it rejects
    // `..` / absolute / root purely on the string (the seatbelt profile is the
    // real boundary; this is defense in depth + a clean error). It is NOT enough
    // on its own — a symlink *inside* projects/ passes the lexical check yet
    // points outside — so the REAL containment guard below (canonicalize +
    // starts_with) closes that hole at the read site, after the path exists.
    let path = resolve_project_path(&env.app_dir(), raw)?;
    if !path.exists() {
        return Err(CanvasError::NotFound(path));
    }
    // SYMLINK confinement (the read-site guard `resolve_project_path` cannot do
    // FS-free): canonicalize the resolved path AND the granted roots, then require
    // the REAL target to live under one of them. This rejects a symlink planted
    // inside projects/ that escapes (e.g. projects/evil.kicad_sch -> /etc/passwd)
    // BEFORE read_to_string ever follows it. Both sides are canonicalized so a
    // symlinked temp root (macOS /tmp -> /private/tmp) does not falsely reject.
    confine_real_path(&env.app_dir(), &path)?;
    // SIZE cap: stat the file and reject anything over the in-app cap BEFORE
    // reading it, so an adversarial multi-GiB "KiCad" file is a clean error rather
    // than an OOM. KiCad sources are small (see MAX_PROJECT_FILE_BYTES).
    let size = std::fs::metadata(&path)?.len();
    if size > MAX_PROJECT_FILE_BYTES {
        return Err(CanvasError::FileTooLarge {
            path,
            size,
            cap: MAX_PROJECT_FILE_BYTES,
        });
    }
    let src = std::fs::read_to_string(&path)?;

    // === real import pipeline (SPEC §1/§5) ================================
    // Parse the KiCad document, build the connectivity graph, write the computed
    // net ids back into the scene (`apply_nets`), then run ERC at import. A parse
    // failure surfaces as CanvasError (the op is logged + dropped by `run`); it
    // never opens a half-built scene — `state` is mutated only after the whole
    // pipeline succeeds.
    let mut scene = crate::parser::parse_document(&path, &src)?;
    // Defense in depth: the parser dispatches by the same extension this helper
    // reads, so the imported scene kind must match the path-inferred kind. A
    // mismatch would mean the two extension maps drifted — fail loudly rather
    // than serve a board as a schematic (or vice versa).
    if scene.kind != scene_kind_for(&path) {
        return Err(CanvasError::parse(
            path,
            "imported scene kind does not match the file extension",
        ));
    }
    let graph = Graph::build(&scene);
    graph.apply_nets(&mut scene); // populates per-entity net_id from the graph
    let markers = crate::erc::run(&scene); // ONE arg (SPEC §5; erc.rs::run)

    // Seed the per-layer visibility list from the scene's layer names (all on).
    let layers: Vec<LayerVisibility> = scene
        .layer_names
        .iter()
        .map(|name| LayerVisibility {
            layer: name.clone(),
            visible: true,
        })
        .collect();

    // Fit the camera to the real document bounds.
    let viewport = fit_viewport(&scene.bounds, layers);

    state.scene = Some(scene);
    state.graph = Some(graph);
    state.open_path = Some(path);
    state.open_source = Some(src);
    state.viewport = viewport;
    // A fresh import resets the selection/trace state machine.
    state.tracer = Tracer::new();
    // ERC runs at import (SPEC §5) — publish the REAL markers on the selection
    // channel. Parked on the state so a later select.net does not clobber the erc
    // list contract, and a later erc.run re-publishes.
    state.selection = Selection {
        erc: Some(markers),
        ..Selection::default()
    };

    // On open: the fitted viewport, then the fresh selection carrying the
    // import-time ERC drop (viewport first so the HUD frames before it paints).
    Ok(vec![
        Telemetry::Viewport(state.viewport.snapshot()),
        Telemetry::Selection(state.selection.clone()),
    ])
}

/// Compute a fitted [`ViewportState`] for a document bounds. Centers on the
/// bounds and picks a scale that frames it; an empty bounds falls back to origin
/// at unit scale (the seam case until the parser populates geometry).
fn fit_viewport(bounds: &Aabb, layers: Vec<LayerVisibility>) -> ViewportState {
    if bounds.is_empty() {
        return ViewportState {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            layers,
        };
    }
    let center = bounds.center();
    // A simple framing scale: fit the larger extent into a nominal 1000-px
    // surface with a 10% margin. The renderer owns the real surface size; this
    // is a reasonable headless default the HUD can refine.
    let extent = bounds.width().max(bounds.height()).max(QUANTUM_FALLBACK);
    let scale = (1000.0 * 0.9) / extent;
    ViewportState {
        x: center.x,
        y: center.y,
        scale,
        layers,
    }
}

/// Guard against a divide-by-zero when a bounds has zero extent on both axes.
const QUANTUM_FALLBACK: f64 = 1.0e-6;

fn dispatch_view_set(state: &mut AppState, view: &ViewSet) -> Result<Vec<Telemetry>> {
    // view.set is meaningful only with a scene (fit needs bounds/selection).
    let _ = state.scene()?;
    match view {
        ViewSet::Explicit { x, y, scale } => {
            state.viewport.x = *x;
            state.viewport.y = *y;
            // Clamp the zoom into the SPEC §3 range (0.01×–500×) defensively so a
            // bogus op cannot drive the renderer to a degenerate transform.
            state.viewport.scale = scale.clamp(0.01, 500.0);
        }
        ViewSet::Fit { target } => {
            let bounds = fit_bounds(state, target)?;
            let layers = state.viewport.layers.clone();
            let fitted = fit_viewport(&bounds, layers);
            state.viewport.x = fitted.x;
            state.viewport.y = fitted.y;
            state.viewport.scale = fitted.scale;
        }
    }
    Ok(vec![Telemetry::Viewport(state.viewport.snapshot())])
}

/// The bounds a `view.set {fit:…}` should frame. `All` → document bounds; `Net`
/// → the bbox of the highlighted net's entities; `Component` → the selected
/// component's bbox. Falls back to the document bounds when the target selection
/// is absent.
fn fit_bounds(state: &AppState, target: &FitTarget) -> Result<Aabb> {
    let scene = state.scene()?;
    match target {
        FitTarget::All => Ok(scene.bounds),
        FitTarget::Net => {
            // Frame every entity currently flagged Highlighted (the selected net).
            let mut bb = Aabb::EMPTY;
            for (i, f) in scene.pad_flags.iter().enumerate() {
                if *f == HighlightFlag::Highlighted {
                    if let Some(p) = scene.pads.get(i) {
                        bb.expand_point(p.position);
                    }
                }
            }
            for (i, f) in scene.wire_flags.iter().enumerate() {
                if *f == HighlightFlag::Highlighted {
                    if let Some(w) = scene.wires.get(i) {
                        bb.expand_point(w.a);
                        bb.expand_point(w.b);
                    }
                }
            }
            for (i, f) in scene.track_flags.iter().enumerate() {
                if *f == HighlightFlag::Highlighted {
                    if let Some(t) = scene.tracks.get(i) {
                        bb.expand_point(t.a);
                        bb.expand_point(t.b);
                    }
                }
            }
            Ok(if bb.is_empty() { scene.bounds } else { bb })
        }
        FitTarget::Component => {
            if let Some(sel) = &state.selection.component {
                if let Some(c) = scene.components.get(sel.component.index()) {
                    if !c.bbox.is_empty() {
                        return Ok(c.bbox);
                    }
                }
            }
            Ok(scene.bounds)
        }
    }
}

fn dispatch_select_net(state: &mut AppState, name: &str) -> Result<Vec<Telemetry>> {
    // Resolve the net id against the REAL graph (its net ids are the trace/graph
    // id space the Tracer walks; the scene's `net_id` arrays are `graph + 1`).
    let net = {
        let (_scene, graph) = state.scene_graph_mut()?;
        let net = graph.net_by_name(name);
        if net.is_none() {
            return Err(CanvasError::NoSuchEntity {
                kind: "net",
                name: name.to_string(),
            });
        }
        net
    };

    // Drive the Tracer over the real graph: it writes the HighlightFlag arrays
    // (net → Highlighted, everything else → Dimmed) and returns the summary
    // `{net, name, entity_count, pin_count}` — counts straight from the graph's
    // node membership / pin_count (SPEC §4). No geometry is touched. Borrow the
    // `scene`/`graph`/`tracer` fields disjointly so the Tracer can mutate the
    // scene while reading the graph.
    let mut summary = {
        let scene = state.scene.as_mut().ok_or(CanvasError::NoProjectOpen)?;
        let graph = state.graph.as_ref().ok_or(CanvasError::NoProjectOpen)?;
        state.tracer.select_net(scene, graph, net)
    };
    // The Tracer names the net via `scene.net_name(net)`, which indexes the scene
    // table with the GRAPH id (off by one from the scene's `graph + 1` ids); take
    // the human name from the graph instead so it is always the requested name.
    summary.name = state
        .graph
        .as_ref()
        .map(|g| g.net_name(net).to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| name.to_string());

    // Selecting a net leaves any in-progress trace (the Tracer reset to Selected
    // inside select_net), so the trace front is stale — clear it (SPEC §4).
    state.trace_step = None;
    state.selection = Selection {
        net: Some(summary.clone()),
        component: None,
        erc: state.selection.erc.clone(),
        trace: None,
    };
    Ok(vec![Telemetry::Selection(Selection {
        net: Some(summary),
        component: None,
        erc: None,
        trace: None,
    })])
}

fn dispatch_select_component(state: &mut AppState, name: &str) -> Result<Vec<Telemetry>> {
    let (comp_id, reference, value, pin_count) = {
        let scene = state.scene()?;
        let comp_id = scene
            .component_by_reference(name)
            .ok_or_else(|| CanvasError::NoSuchEntity {
                kind: "component",
                name: name.to_string(),
            })?;
        let c = &scene.components[comp_id.index()];
        let pin_count = scene
            .pads
            .iter()
            .filter(|p| p.component == comp_id)
            .count() as u32;
        (comp_id, c.reference.clone(), c.value.clone(), pin_count)
    };

    // A component selection leaves any net-trace state: reset the Tracer to idle
    // so a following trace.start correctly reports "no net selected" (SPEC §4).
    state.tracer = Tracer::new();
    // The trace front (if any) is gone with the trace — clear it too.
    state.trace_step = None;

    // Highlight the component + its pads; dim the rest (SPEC §4 cross-probe).
    {
        let scene = state.scene_mut()?;
        scene.clear_highlights();
        if comp_id.index() < scene.component_flags.len() {
            scene.component_flags[comp_id.index()] = HighlightFlag::Highlighted;
        }
        for (i, pad) in scene.pads.iter().enumerate() {
            if i >= scene.pad_flags.len() {
                break;
            }
            scene.pad_flags[i] = if pad.component == comp_id {
                HighlightFlag::Highlighted
            } else {
                HighlightFlag::Dimmed
            };
        }
    }

    let summary = crate::ops::ComponentSelection {
        component: comp_id,
        reference,
        value,
        pin_count,
    };
    state.selection = Selection {
        net: None,
        component: Some(summary.clone()),
        erc: state.selection.erc.clone(),
        trace: None,
    };
    Ok(vec![Telemetry::Selection(Selection {
        net: None,
        component: Some(summary),
        erc: None,
        trace: None,
    })])
}

fn dispatch_trace_start(state: &mut AppState) -> Result<Vec<Telemetry>> {
    // A trace needs a net selected (SPEC §4: trace mode with a net selected).
    if state.selection.net.is_none() {
        // Confirm a scene is open first, so "no project" still maps cleanly.
        let _ = state.scene_graph_mut()?;
        return Err(CanvasError::TraceState(
            "trace.start with no net selected".to_string(),
        ));
    }
    if state.tracer.is_tracing() {
        return Err(CanvasError::TraceState(
            "trace already in progress".to_string(),
        ));
    }
    // Enter trace mode on the real graph: the Tracer precomputes the BFS walk and
    // marks the seed as the front (SPEC §4). Errors (e.g. an empty net) surface as
    // CanvasError::TraceState. The seed front is the current selection; the front
    // advances on trace.step, so we republish the selection so the HUD repaints.
    let step = {
        let scene = state.scene.as_mut().ok_or(CanvasError::NoProjectOpen)?;
        let graph = state.graph.as_ref().ok_or(CanvasError::NoProjectOpen)?;
        state.tracer.start(scene, graph)?
    };
    // Mirror the seed front onto the selection so the HUD gets the SPEC §4
    // via-flash / "step k of n" data (step 1 of N at the seed).
    state.trace_step = Some(step.into());
    Ok(vec![Telemetry::Selection(self_selection(state))])
}

fn dispatch_trace_step(state: &mut AppState) -> Result<Vec<Telemetry>> {
    let _ = state.scene_graph_mut()?;
    if !state.tracer.is_tracing() {
        return Err(CanvasError::TraceState(
            "trace.step before trace.start".to_string(),
        ));
    }
    // Advance the BFS front one electrical edge (SPEC §4): the Tracer demotes the
    // previous front back to Highlighted and flags the new front TraceFront, all
    // flag-array writes — no geometry touched. Republish the selection so the HUD
    // glides the camera to the new front. Borrow the `scene` and `tracer` fields
    // disjointly (Tracer::step needs only &mut Scene, not the graph).
    let step = {
        let scene = state.scene.as_mut().ok_or(CanvasError::NoProjectOpen)?;
        state.tracer.step(scene)?
    };
    // Carry the new front (distance / step k of n / crosses_layer) onto the
    // selection so the HUD can drive the via-flash and progress readout (SPEC §4).
    state.trace_step = Some(step.into());
    Ok(vec![Telemetry::Selection(self_selection(state))])
}

fn dispatch_trace_stop(state: &mut AppState) -> Result<Vec<Telemetry>> {
    let _ = state.scene_graph_mut()?;
    if !state.tracer.is_tracing() {
        return Err(CanvasError::TraceState(
            "trace.stop with no trace in progress".to_string(),
        ));
    }
    // Exit trace mode: drop the front back to a plain net highlight (SPEC §4).
    {
        let scene = state.scene.as_mut().ok_or(CanvasError::NoProjectOpen)?;
        let graph = state.graph.as_ref().ok_or(CanvasError::NoProjectOpen)?;
        state.tracer.stop(scene, graph);
    }
    // The trace front no longer exists — clear it so any later selection republish
    // (start/refresh) does not re-emit a stale front (SPEC §4: trace.stop ends it).
    state.trace_step = None;
    Ok(Vec::new())
}

fn dispatch_layer_set(
    state: &mut AppState,
    layer: &str,
    visible: bool,
) -> Result<Vec<Telemetry>> {
    // Confirm the layer exists in the open scene before toggling.
    {
        let scene = state.scene()?;
        let known = scene.layer_names.iter().any(|n| n == layer);
        if !known {
            return Err(CanvasError::NoSuchEntity {
                kind: "layer",
                name: layer.to_string(),
            });
        }
    }
    // Update (or insert) the layer's visibility in the viewport state.
    if let Some(entry) = state
        .viewport
        .layers
        .iter_mut()
        .find(|l| l.layer == layer)
    {
        entry.visible = visible;
    } else {
        state.viewport.layers.push(LayerVisibility {
            layer: layer.to_string(),
            visible,
        });
    }
    Ok(vec![Telemetry::Viewport(state.viewport.snapshot())])
}

fn dispatch_erc_run(state: &mut AppState) -> Result<Vec<Telemetry>> {
    // Re-run the real electrical rule checks on the stored scene (SPEC §5;
    // erc::run is ONE arg). The scene's `net_id` arrays were populated by
    // `apply_nets` at import, which is exactly what ERC reasons over. Publish the
    // findings on canvas.selection as `{erc:[…]}` (an empty list = ran clean).
    let markers = {
        let scene = state.scene()?;
        crate::erc::run(scene)
    };
    state.selection.erc = Some(markers.clone());
    Ok(vec![Telemetry::Selection(Selection {
        net: None,
        component: None,
        erc: Some(markers),
        trace: None,
    })])
}

/// Snapshot the current selection (net + component + trace front, no erc) for a
/// republish. The trace front rides along ONLY while tracing (`state.trace_step`
/// is `Some` between `trace.start` and `trace.stop`); a plain selection republish
/// carries `trace: None`.
fn self_selection(state: &AppState) -> Selection {
    Selection {
        net: state.selection.net.clone(),
        component: state.selection.component.clone(),
        erc: None,
        trace: state.trace_step,
    }
}

// ===========================================================================
// The socket loop.
// ===========================================================================

/// Connect to the per-app Unix socket, then read JSONL lines and dispatch them
/// until the host sends `stop`, closes the socket, or an unrecoverable I/O error
/// occurs. Every accepted op's telemetry is written back as a token-stamped
/// [`OutboundLine`] (`type:"items"`, `data.topic` = the payload's declared topic).
///
/// A malformed line ([`CanvasError::Protocol`]) or an op error
/// ([`CanvasError::NoSuchEntity`] etc.) is logged to the host as a `log` line and
/// the loop CONTINUES — one bad op never tears the app down (mirrors
/// global-scan's "never let the loop die on one bad cycle").
///
/// HARD SAFETY: this binds NO listener (it connects to the daemon's socket), opens
/// NO window, touches NO GPU, plays NO audio.
pub fn run(env: AppEnv) -> Result<()> {
    let stream = UnixStream::connect(&env.socket_path)?;
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    let mut state = AppState::new();

    // Announce we are online (a log line; not telemetry).
    log_line(&mut writer, &env.token, "silicon-canvas online");

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // socket closed / read error → exit cleanly
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_command(trimmed) {
            Ok(Command::Control(HostControl::Stop)) => {
                // Clean shutdown.
                log_line(&mut writer, &env.token, "silicon-canvas stopping");
                break;
            }
            Ok(Command::Control(HostControl::Start))
            | Ok(Command::Control(HostControl::Refresh)) => {
                // Start/refresh: re-publish the current viewport + selection so
                // the HUD repaints. Nothing to fetch (unlike global-scan); the
                // scene is already resident.
                if state.scene.is_some() {
                    emit(&mut writer, &env.token, Telemetry::Viewport(state.viewport.snapshot()));
                    emit(&mut writer, &env.token, Telemetry::Selection(self_selection(&state)));
                }
            }
            Ok(Command::Op(op)) => match dispatch(&mut state, &op, &env) {
                Ok(payloads) => {
                    for p in payloads {
                        emit(&mut writer, &env.token, p);
                    }
                }
                Err(e) => {
                    // One bad op is logged and skipped — the loop survives.
                    log_line(&mut writer, &env.token, &format!("op error: {e}"));
                }
            },
            Err(e) => {
                // Malformed line → log + drop, never panic, never exit.
                log_line(&mut writer, &env.token, &format!("dropped line: {e}"));
            }
        }
    }
    Ok(())
}

/// Write one token-stamped telemetry line to the host.
fn emit(writer: &mut UnixStream, token: &str, payload: Telemetry) {
    let line = OutboundLine::telemetry(token, payload);
    // Best-effort: if serialization or the write fails, the host went away — the
    // read loop will observe EOF and exit. We never panic on a write error.
    if let Ok(mut s) = serde_json::to_string(&line) {
        s.push('\n');
        let _ = writer.write_all(s.as_bytes());
        let _ = writer.flush();
    }
}

/// Write one token-stamped `log` line to the host (NOT telemetry). Mirrors the
/// host's `{"token","type":"log","data":{"line":…}}` log shape. The token is
/// stamped but NEVER included in the log message itself.
fn log_line(writer: &mut UnixStream, token: &str, message: &str) {
    let line = serde_json::json!({
        "token": token,
        "type": "log",
        "data": { "line": message },
    });
    let mut s = line.to_string();
    s.push('\n');
    let _ = writer.write_all(s.as_bytes());
    let _ = writer.flush();
}

// ===========================================================================
// Tests — drive the PURE dispatcher with hand-built scenes; no socket, no env.
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ComponentId, NetId};
    use crate::ops::NetSelection;
    use crate::scene::{
        Component, Label, LabelKind, LayerId, Pad, PadShape, PinType, Point, Wire,
    };

    fn test_env() -> AppEnv {
        AppEnv {
            socket_path: PathBuf::from("/tmp/does-not-exist.sock"),
            token: "test-token".to_string(),
            name: "silicon-canvas".to_string(),
            project_root: PathBuf::from("/tmp/jarvis-root"),
        }
    }

    fn pin(component: u32, name: &str, at: Point, pin_type: PinType) -> Pad {
        Pad {
            component: ComponentId::new(component),
            name: name.into(),
            position: at,
            size: (1.0, 1.0),
            shape: PadShape::Circle,
            pin_type,
            layer: LayerId::SCHEMATIC,
            net_id: NetId::NONE,
        }
    }

    fn comp(reference: &str, value: &str, at: Point) -> Component {
        Component {
            reference: reference.into(),
            value: value.into(),
            lib_id: "Device:U".into(),
            position: at,
            rotation: 0.0,
            mirror: false,
            bbox: Aabb::new(Point::new(at.x - 2.0, at.y - 2.0), Point::new(at.x + 2.0, at.y + 2.0)),
            layer: LayerId::SCHEMATIC,
        }
    }

    /// A small but REAL schematic whose GEOMETRY actually connects, so the real
    /// `graph::Graph::build` + `apply_nets` compute the nets (rather than relying
    /// on hand-set ids, which the engine ignores). Two named nets:
    ///
    ///   net "SIG": pad0(U1.1,Input) -wire0- (10,0) -wire1- pad1(U2.1,Output),
    ///             named by a "SIG" label at (0,0). 5 entities, 2 pins — a chain
    ///             that traces deterministically pad0→wire0→…→pad1.
    ///   net "GND": pad2(U3.1) -wire2- pad3(U3.2), named by a "GND" label at
    ///             (100,0). 4 entities, 2 pins.
    ///
    /// The scene is run through the SAME pipeline the dispatcher uses
    /// (`Graph::build` + `apply_nets`), so the scene `net_id` arrays and the graph
    /// agree. ERC is clean on this scene (every pin sits on a multi-occupancy net;
    /// no dangling wires; labels share their nets).
    fn fixture_scene() -> Scene {
        let mut s = Scene::new(SceneKind::Schematic);
        s.layer_names = vec!["schematic".to_string()];

        s.components.push(comp("U1", "MCU", Point::new(0.0, 0.0))); // 0
        s.components.push(comp("U2", "BUF", Point::new(20.0, 0.0))); // 1
        s.components.push(comp("U3", "RES", Point::new(105.0, 0.0))); // 2

        // --- net SIG: a traceable chain pad0 - wire0 - wire1 - pad1 ---
        s.pads.push(pin(0, "1", Point::new(0.0, 0.0), PinType::Input)); // pad0 U1.1
        s.pads.push(pin(1, "1", Point::new(20.0, 0.0), PinType::Output)); // pad1 U2.1
        s.wires.push(Wire {
            a: Point::new(0.0, 0.0),
            b: Point::new(10.0, 0.0),
            net_id: NetId::NONE,
        }); // wire0
        s.wires.push(Wire {
            a: Point::new(10.0, 0.0),
            b: Point::new(20.0, 0.0),
            net_id: NetId::NONE,
        }); // wire1
        s.labels.push(Label {
            text: "SIG".into(),
            position: Point::new(0.0, 0.0),
            rotation: 0.0,
            kind: LabelKind::Local,
            net_id: NetId::NONE,
        }); // label0 names SIG

        // --- net GND: pad2 - wire2 - pad3 ---
        s.pads.push(pin(2, "1", Point::new(100.0, 0.0), PinType::Passive)); // pad2 U3.1
        s.pads.push(pin(2, "2", Point::new(110.0, 0.0), PinType::Passive)); // pad3 U3.2
        s.wires.push(Wire {
            a: Point::new(100.0, 0.0),
            b: Point::new(110.0, 0.0),
            net_id: NetId::NONE,
        }); // wire2
        s.labels.push(Label {
            text: "GND".into(),
            position: Point::new(100.0, 0.0),
            rotation: 0.0,
            kind: LabelKind::Local,
            net_id: NetId::NONE,
        }); // label1 names GND

        let mut bb = Aabb::EMPTY;
        bb.expand_point(Point::new(0.0, 0.0));
        bb.expand_point(Point::new(110.0, 12.0));
        s.bounds = bb;
        s.init_flags();
        s
    }

    /// Build the dispatcher state by running the fixture scene through the REAL
    /// import pipeline (`Graph::build` + `apply_nets`), exactly as `project.open`
    /// does — so `select.net`/`trace.*` walk the same graph the running app would.
    fn state_with_fixture() -> AppState {
        let mut scene = fixture_scene();
        let graph = Graph::build(&scene);
        graph.apply_nets(&mut scene);

        let mut st = AppState::new();
        st.scene = Some(scene);
        st.graph = Some(graph);
        st.open_path = Some(PathBuf::from(
            "/tmp/jarvis-root/apps/silicon-canvas/projects/x.kicad_sch",
        ));
        st.viewport.layers = vec![LayerVisibility {
            layer: "schematic".into(),
            visible: true,
        }];
        st
    }

    /// The highlight flag for an entity ref (test-only readback over the scene's
    /// parallel flag arrays).
    fn flag_of(scene: &Scene, e: crate::ids::EntityRef) -> HighlightFlag {
        use crate::ids::EntityKind;
        match e.kind {
            EntityKind::Pad => scene.pad_flags[e.idx()],
            EntityKind::Wire => scene.wire_flags[e.idx()],
            EntityKind::Junction => scene.junction_flags[e.idx()],
            EntityKind::Label => scene.label_flags[e.idx()],
            EntityKind::Track => scene.track_flags[e.idx()],
            EntityKind::Via => scene.via_flags[e.idx()],
            EntityKind::Zone => scene.zone_flags[e.idx()],
            EntityKind::Component => scene.component_flags[e.idx()],
            EntityKind::Sheet => HighlightFlag::Normal,
        }
    }

    // --- parse_command classification ------------------------------------

    #[test]
    fn parse_command_classifies_op_and_control() {
        assert_eq!(
            parse_command(r#"{"op":"select.net","name":"3V3"}"#).unwrap(),
            Command::Op(Op::SelectNet { name: "3V3".into() })
        );
        assert_eq!(
            parse_command(r#"{"type":"start"}"#).unwrap(),
            Command::Control(HostControl::Start)
        );
        assert_eq!(
            parse_command(r#"{"type":"stop"}"#).unwrap(),
            Command::Control(HostControl::Stop)
        );
    }

    #[test]
    fn parse_command_rejects_garbage_without_panicking() {
        // Not JSON.
        assert!(matches!(
            parse_command("not json at all"),
            Err(CanvasError::Protocol(_))
        ));
        // JSON, but not an object.
        assert!(matches!(
            parse_command("[1,2,3]"),
            Err(CanvasError::Protocol(_))
        ));
        // An object with neither `op` nor `type`.
        assert!(matches!(
            parse_command(r#"{"hello":"world"}"#),
            Err(CanvasError::Protocol(_))
        ));
        // Unknown control verb.
        assert!(matches!(
            parse_command(r#"{"type":"explode"}"#),
            Err(CanvasError::Protocol(_))
        ));
        // Empty line.
        assert!(matches!(parse_command("   "), Err(CanvasError::Protocol(_))));
    }

    #[test]
    fn unknown_op_is_clean_error_not_panic() {
        // An `op` field naming an op that does not exist → Protocol error, never
        // a panic (the daemon contract: drop the line, keep serving).
        let r = parse_command(r#"{"op":"format.disk","target":"/"}"#);
        assert!(matches!(r, Err(CanvasError::Protocol(_))), "{r:?}");
    }

    // --- project.open path confinement (the security-critical test) -------

    #[test]
    fn project_open_rejects_path_traversal() {
        let mut st = AppState::new();
        let env = test_env();
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "../etc/passwd".to_string(),
            },
            &env,
        );
        assert!(
            matches!(r, Err(CanvasError::PathNotPermitted(_))),
            "path traversal must be rejected, got {r:?}"
        );
        // The scene must NOT have been opened.
        assert!(st.scene.is_none());
    }

    #[test]
    fn project_open_rejects_nested_and_absolute_escapes() {
        let app_dir = Path::new("/root/apps/silicon-canvas");
        // `..` buried mid-path.
        assert!(matches!(
            resolve_project_path(app_dir, "projects/../../etc/passwd"),
            Err(CanvasError::PathNotPermitted(_))
        ));
        // Absolute path.
        assert!(matches!(
            resolve_project_path(app_dir, "/etc/passwd"),
            Err(CanvasError::PathNotPermitted(_))
        ));
        // A sneaky `..` inside libraries/.
        assert!(matches!(
            resolve_project_path(app_dir, "libraries/../../../secret.kicad_sym"),
            Err(CanvasError::PathNotPermitted(_))
        ));
    }

    #[test]
    fn project_open_accepts_confined_paths_and_kinds() {
        let app_dir = Path::new("/root/apps/silicon-canvas");
        // projects/-rooted.
        assert_eq!(
            resolve_project_path(app_dir, "projects/board.kicad_sch").unwrap(),
            app_dir.join("projects/board.kicad_sch")
        );
        // bare filename defaults under projects/.
        assert_eq!(
            resolve_project_path(app_dir, "board.kicad_pcb").unwrap(),
            app_dir.join("projects/board.kicad_pcb")
        );
        // libraries/-rooted symbol library.
        assert_eq!(
            resolve_project_path(app_dir, "libraries/Device.kicad_sym").unwrap(),
            app_dir.join("libraries/Device.kicad_sym")
        );
        // Unsupported extension is rejected (not a path-traversal, a type error).
        assert!(matches!(
            resolve_project_path(app_dir, "projects/board.txt"),
            Err(CanvasError::UnsupportedFileType(_))
        ));
    }

    #[test]
    fn scene_kind_inferred_from_extension() {
        assert_eq!(
            scene_kind_for(Path::new("x.kicad_pcb")),
            SceneKind::Pcb
        );
        assert_eq!(
            scene_kind_for(Path::new("x.kicad_sch")),
            SceneKind::Schematic
        );
    }

    // --- select.net returns the expected selection payload ----------------

    #[test]
    fn select_net_returns_expected_selection_payload() {
        // Real-engine path: select.net resolves the name against the real graph
        // and the Tracer counts node membership over that graph.
        let mut st = state_with_fixture();
        let env = test_env();
        let out = dispatch(&mut st, &Op::SelectNet { name: "SIG".into() }, &env).unwrap();
        assert_eq!(out.len(), 1);
        let Telemetry::Selection(sel) = &out[0] else {
            panic!("expected a Selection payload, got {:?}", out[0]);
        };
        let ns = sel.net.as_ref().expect("net selection present");
        assert_eq!(ns.name, "SIG");
        // The reported id is the graph net id resolved by name (not hand-set).
        assert_eq!(ns.net, st.graph.as_ref().unwrap().net_by_name("SIG"));
        // SIG = pad0 + pad1 + wire0 + wire1 + label0 = 5 entities; 2 are pins.
        assert_eq!(ns.pin_count, 2, "two pins on SIG (U1.1, U2.1)");
        assert_eq!(ns.entity_count, 5, "2 pads + 2 wires + 1 label");
        // The payload omits component + erc (pure net selection line).
        assert!(sel.component.is_none());
        assert!(sel.erc.is_none());
    }

    #[test]
    fn select_net_writes_highlight_flags_not_geometry() {
        let mut st = state_with_fixture();
        let env = test_env();
        let before_wire = st.scene.as_ref().unwrap().wires[0];
        dispatch(&mut st, &Op::SelectNet { name: "SIG".into() }, &env).unwrap();
        let scene = st.scene.as_ref().unwrap();
        // SIG entities highlighted (pad0/pad1, wire0/wire1, label0).
        assert_eq!(scene.pad_flags[0], HighlightFlag::Highlighted);
        assert_eq!(scene.pad_flags[1], HighlightFlag::Highlighted);
        assert_eq!(scene.wire_flags[0], HighlightFlag::Highlighted);
        assert_eq!(scene.wire_flags[1], HighlightFlag::Highlighted);
        assert_eq!(scene.label_flags[0], HighlightFlag::Highlighted);
        // The GND net's entities are dimmed (off the selected net).
        assert_eq!(scene.pad_flags[2], HighlightFlag::Dimmed);
        assert_eq!(scene.pad_flags[3], HighlightFlag::Dimmed);
        assert_eq!(scene.wire_flags[2], HighlightFlag::Dimmed);
        // Geometry untouched (SPEC §2: flag write only).
        assert_eq!(scene.wires[0], before_wire);
    }

    #[test]
    fn select_net_unknown_is_no_such_entity_not_panic() {
        let mut st = state_with_fixture();
        let env = test_env();
        let r = dispatch(&mut st, &Op::SelectNet { name: "NOPE".into() }, &env);
        assert!(
            matches!(r, Err(CanvasError::NoSuchEntity { kind: "net", .. })),
            "{r:?}"
        );
    }

    #[test]
    fn select_before_open_is_no_project_open() {
        let mut st = AppState::new();
        let env = test_env();
        let r = dispatch(&mut st, &Op::SelectNet { name: "SIG".into() }, &env);
        assert!(matches!(r, Err(CanvasError::NoProjectOpen)), "{r:?}");
    }

    // --- select.component -------------------------------------------------

    #[test]
    fn select_component_returns_summary() {
        let mut st = state_with_fixture();
        let env = test_env();
        let out = dispatch(
            &mut st,
            &Op::SelectComponent { name: "U3".into() },
            &env,
        )
        .unwrap();
        let Telemetry::Selection(sel) = &out[0] else {
            panic!("expected Selection");
        };
        let cs = sel.component.as_ref().unwrap();
        assert_eq!(cs.reference, "U3");
        assert_eq!(cs.value, "RES");
        assert_eq!(cs.pin_count, 2, "U3 owns pad2 + pad3");
        assert_eq!(cs.component, ComponentId::new(2));
    }

    #[test]
    fn select_component_unknown_is_no_such_entity() {
        let mut st = state_with_fixture();
        let env = test_env();
        let r = dispatch(&mut st, &Op::SelectComponent { name: "ZZ9".into() }, &env);
        assert!(
            matches!(r, Err(CanvasError::NoSuchEntity { kind: "component", .. })),
            "{r:?}"
        );
    }

    // --- view.set ---------------------------------------------------------

    #[test]
    fn view_set_explicit_clamps_and_reports() {
        let mut st = state_with_fixture();
        let env = test_env();
        let out = dispatch(
            &mut st,
            &Op::ViewSet(ViewSet::Explicit {
                x: 3.0,
                y: 4.0,
                scale: 9999.0, // above the 500x cap
            }),
            &env,
        )
        .unwrap();
        let Telemetry::Viewport(vp) = &out[0] else {
            panic!("expected Viewport");
        };
        assert_eq!(vp.x, 3.0);
        assert_eq!(vp.y, 4.0);
        assert_eq!(vp.scale, 500.0, "zoom clamped to the SPEC range");
    }

    #[test]
    fn view_set_fit_all_centers_on_bounds() {
        let mut st = state_with_fixture();
        let env = test_env();
        let out = dispatch(
            &mut st,
            &Op::ViewSet(ViewSet::Fit {
                target: FitTarget::All,
            }),
            &env,
        )
        .unwrap();
        let Telemetry::Viewport(vp) = &out[0] else {
            panic!("expected Viewport");
        };
        // bounds = (0,0)..(110,12) → center (55,6).
        assert_eq!(vp.x, 55.0);
        assert_eq!(vp.y, 6.0);
        assert!(vp.scale > 0.0);
    }

    #[test]
    fn view_set_before_open_is_no_project_open() {
        let mut st = AppState::new();
        let env = test_env();
        let r = dispatch(
            &mut st,
            &Op::ViewSet(ViewSet::Fit { target: FitTarget::All }),
            &env,
        );
        assert!(matches!(r, Err(CanvasError::NoProjectOpen)), "{r:?}");
    }

    // --- layer.set --------------------------------------------------------

    #[test]
    fn layer_set_toggles_known_layer() {
        let mut st = state_with_fixture();
        let env = test_env();
        let out = dispatch(
            &mut st,
            &Op::LayerSet {
                layer: "schematic".into(),
                visible: false,
            },
            &env,
        )
        .unwrap();
        let Telemetry::Viewport(vp) = &out[0] else {
            panic!("expected Viewport");
        };
        let entry = vp
            .layer_visibility
            .iter()
            .find(|l| l.layer == "schematic")
            .unwrap();
        assert!(!entry.visible);
    }

    #[test]
    fn layer_set_unknown_layer_is_no_such_entity() {
        let mut st = state_with_fixture();
        let env = test_env();
        let r = dispatch(
            &mut st,
            &Op::LayerSet {
                layer: "B.Cu".into(),
                visible: true,
            },
            &env,
        );
        assert!(
            matches!(r, Err(CanvasError::NoSuchEntity { kind: "layer", .. })),
            "{r:?}"
        );
    }

    // --- trace state machine ---------------------------------------------

    #[test]
    fn trace_step_before_start_is_trace_state_error() {
        let mut st = state_with_fixture();
        let env = test_env();
        let r = dispatch(&mut st, &Op::TraceStep, &env);
        assert!(matches!(r, Err(CanvasError::TraceState(_))), "{r:?}");
    }

    #[test]
    fn trace_start_requires_a_selected_net() {
        let mut st = state_with_fixture();
        let env = test_env();
        // No net selected yet.
        let r = dispatch(&mut st, &Op::TraceStart, &env);
        assert!(matches!(r, Err(CanvasError::TraceState(_))), "{r:?}");
    }

    #[test]
    fn trace_start_step_stop_happy_path() {
        use crate::ids::EntityRef;
        let mut st = state_with_fixture();
        let env = test_env();
        dispatch(&mut st, &Op::SelectNet { name: "SIG".into() }, &env).unwrap();

        // trace.start marks the seed (the first pad on SIG, pad0) as the front.
        dispatch(&mut st, &Op::TraceStart, &env).unwrap();
        assert!(st.tracer.is_tracing());
        let front_of = |st: &AppState, e: EntityRef| flag_of(st.scene.as_ref().unwrap(), e);
        assert_eq!(
            front_of(&st, EntityRef::pad(0u32.into())),
            HighlightFlag::TraceFront,
            "seed pad0 is the front at start"
        );

        // trace.step advances the BFS front DETERMINISTICALLY one electrical edge
        // along the chain pad0 → wire0 → wire1 → pad1. (label0 also sits at (0,0);
        // the (kind,index) tiebreak puts Wire before Label, so wire0 is visited at
        // depth 1 ahead of the label.)
        dispatch(&mut st, &Op::TraceStep, &env).unwrap();
        assert_eq!(
            front_of(&st, EntityRef::wire(0u32.into())),
            HighlightFlag::TraceFront,
            "first step lands on wire0"
        );
        assert_eq!(
            front_of(&st, EntityRef::pad(0u32.into())),
            HighlightFlag::Highlighted,
            "the previous front (pad0) demotes back to Highlighted"
        );

        // Exactly one TraceFront cell across the whole scene at any step.
        let fronts = |st: &AppState| {
            let s = st.scene.as_ref().unwrap();
            s.pad_flags
                .iter()
                .chain(&s.wire_flags)
                .chain(&s.label_flags)
                .filter(|f| **f == HighlightFlag::TraceFront)
                .count()
        };
        assert_eq!(fronts(&st), 1);

        dispatch(&mut st, &Op::TraceStop, &env).unwrap();
        assert!(!st.tracer.is_tracing());
        // After stop, the front is gone — the net is back to a plain highlight.
        assert_eq!(fronts(&st), 0);
        assert_eq!(
            front_of(&st, EntityRef::pad(0u32.into())),
            HighlightFlag::Highlighted
        );

        // A step after stop errors again (the state machine is back to Selected).
        assert!(matches!(
            dispatch(&mut st, &Op::TraceStep, &env),
            Err(CanvasError::TraceState(_))
        ));
    }

    // --- erc.run ----------------------------------------------------------

    #[test]
    fn erc_run_publishes_selection_with_erc_list() {
        let mut st = state_with_fixture();
        let env = test_env();
        let out = dispatch(&mut st, &Op::ErcRun, &env).unwrap();
        let Telemetry::Selection(sel) = &out[0] else {
            panic!("expected Selection");
        };
        // The erc field is present (empty list = ran clean) — distinguishes an
        // ERC drop from a plain selection update (ops.rs contract).
        assert!(sel.erc.is_some());
        assert!(sel.net.is_none());
        assert!(sel.component.is_none());
    }

    // --- outbound wire shape (round-trips through the host's classifier) --

    #[test]
    fn telemetry_serializes_to_the_host_wire_shape() {
        let line = OutboundLine::telemetry(
            "tok",
            Telemetry::Selection(Selection {
                net: Some(NetSelection {
                    net: NetId::new(1),
                    name: "3V3".into(),
                    entity_count: 5,
                    pin_count: 2,
                }),
                component: None,
                erc: None,
                trace: None,
            }),
        );
        let j = serde_json::to_string(&line).unwrap();
        // The host (apps.rs classify_inbound_line) reads type + data.topic.
        assert!(j.contains(r#""type":"items""#));
        assert!(j.contains(r#""topic":"canvas.selection""#));
        assert!(j.contains(r#""token":"tok""#));
        // FLAT wire contract: the payload fields sit DIRECTLY in `data` alongside
        // `topic` (like Vision), NOT under a nested `payload` object. The HUD parsers
        // read fields flat (num(data,"p50"), ...), so a `"payload":` wrapper here
        // would render blank panels — assert it is absent.
        assert!(
            !j.contains(r#""payload""#),
            "telemetry must be FLAT in data (no nested payload wrapper): {j}"
        );
        assert!(j.contains(r#""net""#), "the selection's fields must be flattened into data: {j}");
        // And the payload round-trips back through the flattened shape.
        let back: OutboundLine = serde_json::from_str(&j).unwrap();
        assert_eq!(back.data.topic, crate::ops::TOPIC_SELECTION);
    }

    // =======================================================================
    // End-to-end: a REAL .kicad_sch written under the confined projects/ dir,
    // imported through the SAME dispatch_project_open path the running app uses
    // (parse → graph → apply_nets → erc), then exercised with select.net /
    // trace.start+step. No socket, no daemon, no GPU.
    // =======================================================================

    /// A real-shaped `.kicad_sch`: lib_symbols defines `Device:RL` with two pins
    /// (pin 1 passive, pin 2 input). One placed R1 at (100,100): pin 1 lands at
    /// (100,103.81), pin 2 at (100,96.19). Pin 1 is wired up to a "NETA" label;
    /// pin 2 is wired up to a "NETB" label — every wire end terminates on a pad or
    /// a label, so the board is electrically CLEAN (ERC yields []).
    const CLEAN_SCH: &str = r#"
(kicad_sch (version 20230121) (generator eeschema)
  (lib_symbols
    (symbol "Device:RL"
      (symbol "RL_1_1"
        (pin passive line (at 0 3.81 270) (length 1.27)
          (name "~" (effects (font (size 1.27 1.27))))
          (number "1" (effects (font (size 1.27 1.27)))))
        (pin input line (at 0 -3.81 90) (length 1.27)
          (name "~" (effects (font (size 1.27 1.27))))
          (number "2" (effects (font (size 1.27 1.27))))))))
  (wire (pts (xy 100 103.81) (xy 100 110)) (stroke (width 0)))
  (label "NETA" (at 100 110 0) (effects (font (size 1.27 1.27))))
  (wire (pts (xy 100 96.19) (xy 100 90)) (stroke (width 0)))
  (label "NETB" (at 100 90 0) (effects (font (size 1.27 1.27))))
  (symbol (lib_id "Device:RL") (at 100 100 0) (unit 1)
    (property "Reference" "R1" (at 102 98 0))
    (property "Value" "10k" (at 102 102 0))))
"#;

    /// Same symbol/placement as [`CLEAN_SCH`], but pin 2 (the INPUT pin at
    /// (100,96.19)) is left entirely floating — no wire, no label. The real graph
    /// hands that isolated pin its own synthetic singleton net (occupancy 1), so
    /// the UnconnectedPin rule must flag exactly that one pin (R1.2), at its
    /// coordinate, with Warning severity. Pin 1 stays wired to NETA (clean).
    const DEFECT_SCH: &str = r#"
(kicad_sch (version 20230121) (generator eeschema)
  (lib_symbols
    (symbol "Device:RL"
      (symbol "RL_1_1"
        (pin passive line (at 0 3.81 270) (length 1.27)
          (name "~" (effects (font (size 1.27 1.27))))
          (number "1" (effects (font (size 1.27 1.27)))))
        (pin input line (at 0 -3.81 90) (length 1.27)
          (name "~" (effects (font (size 1.27 1.27))))
          (number "2" (effects (font (size 1.27 1.27))))))))
  (wire (pts (xy 100 103.81) (xy 100 110)) (stroke (width 0)))
  (label "NETA" (at 100 110 0) (effects (font (size 1.27 1.27))))
  (symbol (lib_id "Device:RL") (at 100 100 0) (unit 1)
    (property "Reference" "R1" (at 102 98 0))
    (property "Value" "10k" (at 102 102 0))))
"#;

    /// Create a fresh confined `projects/` dir under a unique temp root and write
    /// `name` with `contents`; return `(env, app_dir)`. The env's `app_dir()` is
    /// `<root>/apps/silicon-canvas`, so `project.open "<name>"` resolves under
    /// `<root>/apps/silicon-canvas/projects/<name>` — exactly the confined root.
    fn write_project_file(name: &str, contents: &str) -> (AppEnv, PathBuf) {
        // Unique per-test root: temp_dir + pid + a process-wide counter.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sc-ipc-e2e-{}-{}",
            std::process::id(),
            n
        ));
        let projects = root.join("apps").join("silicon-canvas").join("projects");
        std::fs::create_dir_all(&projects).expect("create confined projects dir");
        std::fs::write(projects.join(name), contents).expect("write project file");
        let env = AppEnv {
            socket_path: PathBuf::from("/tmp/does-not-exist.sock"),
            token: "test-token".to_string(),
            name: "silicon-canvas".to_string(),
            project_root: root.clone(),
        };
        (env, root)
    }

    #[test]
    fn project_open_imports_real_schematic_and_runs_erc_clean() {
        let (env, _root) = write_project_file("clean.kicad_sch", CLEAN_SCH);
        let mut st = AppState::new();

        let out = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "clean.kicad_sch".into(),
            },
            &env,
        )
        .unwrap();

        // The real scene + graph are now resident (not an empty seam scene).
        let scene = st.scene.as_ref().expect("scene imported");
        assert!(st.graph.is_some(), "graph built at import");
        assert!(scene.entity_count() > 0, "real entities imported");
        assert_eq!(scene.components.len(), 1, "R1 placed");
        assert_eq!(scene.pads.len(), 2, "two pins resolved from lib_symbols");
        assert_eq!(scene.wires.len(), 2, "two wire segments");
        assert_eq!(scene.labels.len(), 2, "NETA + NETB labels");

        // project.open emits a fitted viewport then the import-time ERC selection.
        assert!(matches!(out[0], Telemetry::Viewport(_)));
        let Telemetry::Selection(sel) = &out[1] else {
            panic!("expected the import-time ERC Selection drop");
        };
        let erc = sel.erc.as_ref().expect("erc list published at import");
        assert!(
            erc.is_empty(),
            "a clean board yields no ERC findings, got {:?}",
            erc.iter().map(|m| m.code.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn project_open_flags_planted_unconnected_pin() {
        let (env, _root) = write_project_file("defect.kicad_sch", DEFECT_SCH);
        let mut st = AppState::new();

        let out = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "defect.kicad_sch".into(),
            },
            &env,
        )
        .unwrap();

        // The import-time ERC drop carries exactly the planted unconnected pin.
        let Telemetry::Selection(sel) = &out[1] else {
            panic!("expected the import-time ERC Selection drop");
        };
        let erc = sel.erc.as_ref().expect("erc list published at import");
        let unconnected: Vec<_> = erc
            .iter()
            .filter(|m| m.code == crate::ops::ErcCode::UnconnectedPin.as_str())
            .collect();
        assert_eq!(
            unconnected.len(),
            1,
            "exactly the floating INPUT pin (R1.2) flags: {:?}",
            erc.iter().map(|m| m.code.as_str()).collect::<Vec<_>>()
        );
        let m = unconnected[0];
        assert_eq!(m.severity, crate::ops::ErcSeverity::Warning);
        assert_eq!(m.at, crate::scene::Point::new(100.0, 96.19), "anchored at the floating pin");
        assert!(m.message.contains("R1.2"), "names the pin: {}", m.message);

        // erc.run re-runs on the stored scene and re-publishes the same finding.
        let out2 = dispatch(&mut st, &Op::ErcRun, &env).unwrap();
        let Telemetry::Selection(sel2) = &out2[0] else {
            panic!("expected Selection");
        };
        let erc2 = sel2.erc.as_ref().expect("erc re-published");
        assert_eq!(
            erc2.iter()
                .filter(|m| m.code == crate::ops::ErcCode::UnconnectedPin.as_str())
                .count(),
            1,
            "erc.run reproduces the planted finding on the stored scene"
        );
    }

    #[test]
    fn project_open_then_select_net_and_trace_on_real_graph() {
        let (env, _root) = write_project_file("clean2.kicad_sch", CLEAN_SCH);
        let mut st = AppState::new();
        dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "clean2.kicad_sch".into(),
            },
            &env,
        )
        .unwrap();

        // select.net "NETA": the real graph resolves the name; the net carries
        // pin 1 (R1.1), its wire, and the NETA label (entity_count 3, pin_count 1).
        let out = dispatch(&mut st, &Op::SelectNet { name: "NETA".into() }, &env).unwrap();
        let Telemetry::Selection(sel) = &out[0] else {
            panic!("expected Selection");
        };
        let ns = sel.net.as_ref().expect("net selection present");
        assert_eq!(ns.name, "NETA");
        assert_eq!(ns.pin_count, 1, "one pin on NETA (R1.1)");
        assert_eq!(ns.entity_count, 3, "1 pad + 1 wire + 1 label on NETA");

        // trace.start seeds at the pad on NETA and marks it the front; trace.step
        // advances deterministically one edge to the wire. Both run on the real
        // imported graph (not a stub).
        dispatch(&mut st, &Op::TraceStart, &env).unwrap();
        assert!(st.tracer.is_tracing());
        let scene = st.scene.as_ref().unwrap();
        let fronts: usize = scene
            .pad_flags
            .iter()
            .chain(&scene.wire_flags)
            .chain(&scene.label_flags)
            .filter(|f| **f == HighlightFlag::TraceFront)
            .count();
        assert_eq!(fronts, 1, "exactly one trace-front cell at start");
        // The seed is a pad (pick_seed prefers a pad): the single pin on NETA.
        assert_eq!(
            flag_of(scene, crate::ids::EntityRef::pad(0u32.into())),
            HighlightFlag::TraceFront,
            "trace seeds at the NETA pad (R1.1, pad index 0)"
        );

        // Step once: there is still exactly one front (it advanced one edge).
        dispatch(&mut st, &Op::TraceStep, &env).unwrap();
        let scene = st.scene.as_ref().unwrap();
        let fronts_after: usize = scene
            .pad_flags
            .iter()
            .chain(&scene.wire_flags)
            .chain(&scene.label_flags)
            .filter(|f| **f == HighlightFlag::TraceFront)
            .count();
        assert_eq!(fronts_after, 1, "still exactly one front after a step");
    }

    #[test]
    fn project_open_missing_file_is_not_found() {
        let (env, _root) = write_project_file("present.kicad_sch", CLEAN_SCH);
        let mut st = AppState::new();
        // A confined-but-absent file inside projects/ → NotFound (the confinement
        // check passes; the file just is not there). Scene stays closed.
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "absent.kicad_sch".into(),
            },
            &env,
        );
        assert!(matches!(r, Err(CanvasError::NotFound(_))), "{r:?}");
        assert!(st.scene.is_none());
        assert!(st.graph.is_none());
    }

    // =======================================================================
    // (1) MEDIUM — symlink confinement at the read site.
    // A symlink planted INSIDE projects/ passes the FS-free lexical check, so the
    // read-site guard (confine_real_path) must catch it: canonicalize + roots
    // starts_with. A real fixture under projects/ still opens; the escaping
    // symlink is PathNotPermitted and its target is NEVER read.
    // =======================================================================

    #[test]
    fn project_open_real_fixture_under_projects_opens() {
        // Baseline: a real file physically inside projects/ passes the read-site
        // containment guard and imports (so the guard is not over-rejecting).
        let (env, _root) = write_project_file("inside.kicad_sch", CLEAN_SCH);
        let mut st = AppState::new();
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "inside.kicad_sch".into(),
            },
            &env,
        );
        assert!(r.is_ok(), "a real file inside projects/ must open: {r:?}");
        assert!(st.scene.is_some());
    }

    #[test]
    fn project_open_rejects_symlink_escape_inside_projects_without_reading_target() {
        // Plant a SECRET file OUTSIDE the sandbox, with a recognizable canary.
        let (env, root) = write_project_file("decoy.kicad_sch", CLEAN_SCH);
        let secret_dir = root.join("outside");
        std::fs::create_dir_all(&secret_dir).unwrap();
        let secret = secret_dir.join("secret.txt");
        const CANARY: &str = "TOP-SECRET-CANARY-must-never-be-read";
        std::fs::write(&secret, CANARY).unwrap();

        // Plant a symlink INSIDE projects/ that points at the outside secret but
        // carries a supported KiCad extension (so the lexical + extension gates
        // pass and only the real-path guard can stop it).
        let projects = root.join("apps").join("silicon-canvas").join("projects");
        let link = projects.join("escape.kicad_sch");
        std::os::unix::fs::symlink(&secret, &link)
            .expect("create symlink inside projects/");

        // Sanity: the symlink lexically resolves to inside projects/ (the hole).
        assert_eq!(
            resolve_project_path(&env.app_dir(), "escape.kicad_sch").unwrap(),
            link,
            "the lexical pass alone treats the symlink as confined"
        );

        let mut st = AppState::new();
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "escape.kicad_sch".into(),
            },
            &env,
        );
        // The read-site guard rejects the symlink escape BEFORE read_to_string.
        assert!(
            matches!(r, Err(CanvasError::PathNotPermitted(_))),
            "symlink escaping the sandbox must be PathNotPermitted, got {r:?}"
        );
        // The target was never opened: no scene, and the error must not leak the
        // secret's contents (it reports the requested path, not the resolved one).
        assert!(st.scene.is_none(), "the escaping target must not be imported");
        if let Err(e) = &r {
            let msg = e.to_string();
            assert!(
                !msg.contains(CANARY),
                "the error must never leak the symlink target's bytes: {msg}"
            );
        }
    }

    #[test]
    fn project_open_rejects_directory_symlink_component_escape() {
        // The most dangerous vector: the escape hides in a path COMPONENT, not the
        // leaf. A symlinked DIRECTORY `projects/evildir -> <outside>`; opening
        // `evildir/loot.kicad_sch` is lexically confined (no `..`, defaults under
        // projects/), but canonicalize() resolves the intermediate dir symlink to
        // outside the sandbox, so confine_real_path must reject it before any read.
        let (env, root) = write_project_file("decoy.kicad_sch", CLEAN_SCH);
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        const CANARY: &str = "DIR-SYMLINK-CANARY-must-never-be-read";
        std::fs::write(outside.join("loot.kicad_sch"), CANARY).unwrap();

        let projects = root.join("apps").join("silicon-canvas").join("projects");
        std::os::unix::fs::symlink(&outside, projects.join("evildir"))
            .expect("create directory symlink inside projects/");

        let mut st = AppState::new();
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "evildir/loot.kicad_sch".into(),
            },
            &env,
        );
        assert!(
            matches!(r, Err(CanvasError::PathNotPermitted(_))),
            "a symlinked directory component escaping the sandbox must be PathNotPermitted, got {r:?}"
        );
        assert!(st.scene.is_none(), "the escaping target must not be imported");
        if let Err(e) = &r {
            assert!(
                !e.to_string().contains(CANARY),
                "the error must never leak the symlink target's bytes"
            );
        }
    }

    #[test]
    fn project_open_allows_symlink_whose_target_is_inside_projects() {
        // No false-reject: a leaf symlink that stays IN-SANDBOX
        // (`projects/alias.kicad_sch -> projects/real.kicad_sch`) canonicalizes to
        // a path still under projects/, so the guard must open it.
        let (env, root) = write_project_file("real.kicad_sch", CLEAN_SCH);
        let projects = root.join("apps").join("silicon-canvas").join("projects");
        std::os::unix::fs::symlink(projects.join("real.kicad_sch"), projects.join("alias.kicad_sch"))
            .expect("create in-sandbox symlink");

        let mut st = AppState::new();
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "alias.kicad_sch".into(),
            },
            &env,
        );
        assert!(r.is_ok(), "a symlink pointing inside projects/ must open: {r:?}");
        assert!(st.scene.is_some());
    }

    #[test]
    fn project_open_under_a_symlinked_root_opens() {
        // No false-reject when the project ROOT itself is reached via a symlink
        // (mirrors macOS `/tmp -> /private/tmp`). Both-sides canonicalization must
        // normalize the symlinked root so a real confined file still opens.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "sc-ipc-symroot-{}-{}",
            std::process::id(),
            n
        ));
        let real_root = base.join("real");
        let projects = real_root.join("apps").join("silicon-canvas").join("projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("board.kicad_sch"), CLEAN_SCH).unwrap();
        let link_root = base.join("link");
        std::os::unix::fs::symlink(&real_root, &link_root).expect("symlink the project root");

        let env = AppEnv {
            socket_path: PathBuf::from("/tmp/does-not-exist.sock"),
            token: "test-token".to_string(),
            name: "silicon-canvas".to_string(),
            project_root: link_root, // the root is reached THROUGH a symlink
        };
        let mut st = AppState::new();
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "board.kicad_sch".into(),
            },
            &env,
        );
        assert!(r.is_ok(), "a confined file under a symlinked root must open: {r:?}");
        assert!(st.scene.is_some());
    }

    // =======================================================================
    // (2) LOW — file-size cap. A file over MAX_PROJECT_FILE_BYTES is rejected by
    // the metadata stat BEFORE read_to_string ever loads it.
    // =======================================================================

    #[test]
    fn project_open_rejects_oversize_file_before_reading() {
        let (env, root) = write_project_file("big.kicad_sch", CLEAN_SCH);
        // Overwrite the confined file with > the cap of bytes (cap + 1).
        let big = root
            .join("apps")
            .join("silicon-canvas")
            .join("projects")
            .join("big.kicad_sch");
        // Write the file sparsely to the over-cap length without holding it all in
        // RAM: seek past the cap and write one byte (a real on-disk size, cheap).
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&big)
                .unwrap();
            f.seek(SeekFrom::Start(MAX_PROJECT_FILE_BYTES)).unwrap();
            f.write_all(b"X").unwrap(); // file is now cap+1 bytes
            f.flush().unwrap();
        }
        let size = std::fs::metadata(&big).unwrap().len();
        assert!(size > MAX_PROJECT_FILE_BYTES, "fixture is over the cap");

        let mut st = AppState::new();
        let r = dispatch(
            &mut st,
            &Op::ProjectOpen {
                path: "big.kicad_sch".into(),
            },
            &env,
        );
        match r {
            Err(CanvasError::FileTooLarge { size: got, cap, .. }) => {
                assert_eq!(got, size, "reports the real size");
                assert_eq!(cap, MAX_PROJECT_FILE_BYTES, "reports the cap");
            }
            other => panic!("over-cap file must be FileTooLarge, got {other:?}"),
        }
        assert!(st.scene.is_none(), "the oversize file must not be imported");
    }

    // =======================================================================
    // (3) LOW — trace StepInfo forwarded on canvas.selection.
    // A real PCB net pad0 - track0 - via0 - track1 - pad1 (track1 on a different
    // copper layer, stitched by via0) run through the REAL Graph::build. trace.step
    // onto the via crosses a layer, so the published canvas.selection's `trace`
    // field must carry crosses_layer=true with the right distance/step/of.
    // =======================================================================

    /// Build dispatcher state from a REAL PCB scene (pad0-track0-via0-track1-pad1,
    /// named "NET1") run through `Graph::build` + `apply_nets`, exactly as
    /// `project.open` does. track1 sits on a different copper layer so the via
    /// stitches l0 -> l1 and the BFS marks the cross-layer step.
    fn pcb_via_state() -> AppState {
        use crate::scene::{Pad, PadShape, PinType, Track, Via};
        let l0 = crate::scene::LayerId::new(0);
        let l1 = crate::scene::LayerId::new(1);

        let mut s = Scene::new(SceneKind::Pcb);
        s.layer_names = vec!["F.Cu".to_string(), "B.Cu".to_string()];
        // net_names index 1 = "NET1"; the pads carry that net id so the built
        // graph reuses the name (graph.rs name_hint reads pad.net_id -> net_names).
        s.net_names = vec![String::new(), "NET1".to_string()];

        let pad = |x: f64, layer| Pad {
            component: ComponentId::new(0),
            name: "1".into(),
            position: Point::new(x, 0.0),
            size: (1.0, 1.0),
            shape: PadShape::Circle,
            pin_type: PinType::Passive,
            layer,
            net_id: NetId::new(1),
        };
        s.pads.push(pad(0.0, l0)); // pad0 at (0,0)
        s.pads.push(pad(20.0, l1)); // pad1 at (20,0)

        s.tracks.push(Track {
            a: Point::new(0.0, 0.0),
            b: Point::new(10.0, 0.0),
            width: 0.25,
            layer: l0,
            net_id: NetId::new(1),
        }); // track0 (l0): pad0 .. via
        s.tracks.push(Track {
            a: Point::new(10.0, 0.0),
            b: Point::new(20.0, 0.0),
            width: 0.25,
            layer: l1,
            net_id: NetId::new(1),
        }); // track1 (l1): via .. pad1
        s.vias.push(Via {
            position: Point::new(10.0, 0.0),
            diameter: 0.6,
            drill: 0.3,
            layer_from: l0,
            layer_to: l1,
            net_id: NetId::new(1),
        }); // via0 stitches l0 -> l1

        let mut bb = Aabb::EMPTY;
        bb.expand_point(Point::new(0.0, 0.0));
        bb.expand_point(Point::new(20.0, 1.0));
        s.bounds = bb;
        s.init_flags();

        let graph = Graph::build(&s);
        graph.apply_nets(&mut s);

        let mut st = AppState::new();
        st.scene = Some(s);
        st.graph = Some(graph);
        st.open_path = Some(PathBuf::from(
            "/tmp/jarvis-root/apps/silicon-canvas/projects/board.kicad_pcb",
        ));
        st.viewport.layers = vec![
            LayerVisibility { layer: "F.Cu".into(), visible: true },
            LayerVisibility { layer: "B.Cu".into(), visible: true },
        ];
        st
    }

    #[test]
    fn trace_step_forwards_step_info_on_selection_with_layer_crossing() {
        use crate::ids::EntityKind;
        let mut st = pcb_via_state();
        let env = test_env();

        // Select the real net and enter trace mode: trace.start seeds at distance 0
        // and the canvas.selection carries trace step 1 of N (no crossing yet).
        dispatch(&mut st, &Op::SelectNet { name: "NET1".into() }, &env).unwrap();
        let out = dispatch(&mut st, &Op::TraceStart, &env).unwrap();
        let Telemetry::Selection(sel) = &out[0] else {
            panic!("trace.start must publish a Selection, got {:?}", out[0]);
        };
        let t0 = sel.trace.as_ref().expect("trace.start carries the seed front");
        let total = t0.of;
        assert_eq!(t0.step, 1, "the seed is step 1");
        assert_eq!(t0.distance, 0, "the seed is at distance 0");
        assert!(!t0.crosses_layer, "the seed step does not cross a layer");
        assert_eq!(total, 5, "walk = pad0,track0,via0,track1,pad1");

        // Walk the trace to its end, capturing each published trace sub-payload.
        // The walk order is pad0 -> track0 -> via0 -> track1 -> pad1. The step onto
        // the via (and onto the back-layer track1) crosses a layer.
        let mut steps = vec![*t0];
        loop {
            let out = dispatch(&mut st, &Op::TraceStep, &env).unwrap();
            let Telemetry::Selection(sel) = &out[0] else {
                panic!("trace.step must publish a Selection");
            };
            let t = *sel.trace.as_ref().expect("trace.step carries the front");
            steps.push(t);
            if t.at_end {
                break;
            }
        }

        // step / of / distance are monotonic and well-formed across the walk.
        assert_eq!(steps.len() as u32, total, "one published step per walk node");
        for (i, t) in steps.iter().enumerate() {
            assert_eq!(t.step, i as u32 + 1, "1-based step index");
            assert_eq!(t.of, total, "of = total nodes for every step");
        }
        assert!(steps.last().unwrap().at_end, "the last step reports at_end");

        // At least one step crossed a layer, and EVERY crossing step lands on a via
        // OR on a node whose predecessor was on a different layer (the via0 step is
        // the canonical one). Assert the via step specifically crosses.
        let via_step = steps
            .iter()
            .find(|t| t.at.kind == EntityKind::Via)
            .expect("the walk visits the via");
        assert!(
            via_step.crosses_layer,
            "the step onto the via must report crosses_layer=true: {via_step:?}"
        );
        assert!(
            steps.iter().any(|t| t.crosses_layer),
            "the walk has at least one cross-layer step"
        );
    }

    #[test]
    fn trace_stop_and_select_net_clear_the_trace_front() {
        let mut st = pcb_via_state();
        let env = test_env();
        dispatch(&mut st, &Op::SelectNet { name: "NET1".into() }, &env).unwrap();
        dispatch(&mut st, &Op::TraceStart, &env).unwrap();
        assert!(st.trace_step.is_some(), "tracing carries a front");

        // trace.stop clears the front so a later republish does not re-emit it.
        dispatch(&mut st, &Op::TraceStop, &env).unwrap();
        assert!(st.trace_step.is_none(), "trace.stop clears the front");
        // A republish (self_selection) now carries no trace field.
        assert!(self_selection(&st).trace.is_none());

        // Re-enter, then select.net must also drop the stale front.
        dispatch(&mut st, &Op::TraceStart, &env).unwrap();
        assert!(st.trace_step.is_some());
        let out = dispatch(&mut st, &Op::SelectNet { name: "NET1".into() }, &env).unwrap();
        assert!(st.trace_step.is_none(), "select.net clears the front");
        let Telemetry::Selection(sel) = &out[0] else {
            panic!("expected Selection");
        };
        assert!(sel.trace.is_none(), "a plain net selection carries no trace");
    }
}
