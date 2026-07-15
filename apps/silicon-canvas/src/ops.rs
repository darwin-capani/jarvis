//! The IPC op contract (SPEC §6) + the telemetry payload structs (SPEC §2/§4/§5).
//!
//! Two directions, matching the daemon's app-host protocol (`daemon/src/apps.rs`):
//!
//! 1. HOST → APP (inbound ops). The daemon's generic host sends bare control
//!    lines `{"type":"start"|"refresh"|"stop"}`; richer Silicon-Canvas ops
//!    (`project.open`, `view.set`, `select.net`, …) arrive on the same socket as
//!    JSONL whose `op` field names the operation. [`Op`] is the tagged enum of
//!    those ops; [`Command`] wraps either a host control verb or an [`Op`] so
//!    the app's reader can dispatch a single parsed line.
//!
//! 2. APP → HOST (outbound telemetry). The app emits `{"token","type","data"}`
//!    lines (`type` ∈ `items`/`status`/`log`); the host relays accepted
//!    `items`/`status` onto telemetry under a topic the manifest declared. For
//!    Silicon Canvas those topics are `canvas.render_ms`, `canvas.viewport`,
//!    `canvas.selection`. [`Telemetry`] is the typed union of the three payloads;
//!    [`Telemetry::topic`] gives the declared topic each maps to, and
//!    [`OutboundLine`] is the exact `{token,type,data}` wire shape (`type` =
//!    `"items"`, `data` = `{topic, payload}`) the host classifies.
//!
//! Serde tagging mirrors SPEC §6's `op` column: `#[serde(tag = "op")]` so a line
//! like `{"op":"select.net","name":"3V3"}` deserializes straight into
//! [`Op::SelectNet`]. The op names use the dotted SPEC spelling verbatim.
//!
//! This module is the CONTRACT. The `ipc` agent (de)serializes these; `trace`,
//! `erc`, `render` produce the payloads; none may change the types/field names.

use serde::{Deserialize, Serialize};

use crate::ids::{ComponentId, EntityRef, NetId};
use crate::scene::Point;

// ===========================================================================
// HOST → APP : the op request enum (SPEC §6).
// ===========================================================================

/// A Silicon-Canvas operation request (SPEC §6, the `op` table). Externally
/// tagged on the `op` field with the SPEC's dotted names, so the wire form is
/// e.g. `{"op":"view.set","x":1.0,"y":2.0,"scale":10.0}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    /// `project.open {path}` — import a KiCad file (inside `projects/`), build
    /// the graph + spatial index, run ERC.
    #[serde(rename = "project.open")]
    ProjectOpen { path: String },

    /// `view.set` — camera. Either an explicit `{x,y,scale}` or a `{fit}`
    /// directive; the two forms are distinguished by the `mode` field
    /// ([`ViewSet`]).
    #[serde(rename = "view.set")]
    ViewSet(ViewSet),

    /// `select.net {name}` — programmatic net selection (voice: "show me the 3V3
    /// net" routes here).
    #[serde(rename = "select.net")]
    SelectNet { name: String },

    /// `select.component {name}` — programmatic component selection by reference
    /// designator.
    #[serde(rename = "select.component")]
    SelectComponent { name: String },

    /// `trace.start {}` — enter trace mode on the currently selected net.
    #[serde(rename = "trace.start")]
    TraceStart,

    /// `trace.step {}` — advance the highlight front one electrical edge.
    #[serde(rename = "trace.step")]
    TraceStep,

    /// `trace.stop {}` — exit trace mode.
    #[serde(rename = "trace.stop")]
    TraceStop,

    /// `layer.set {layer, visible}` — toggle a layer's visibility (PCB).
    #[serde(rename = "layer.set")]
    LayerSet { layer: String, visible: bool },

    /// `erc.run {}` — re-run electrical rule checks on the open scene.
    #[serde(rename = "erc.run")]
    ErcRun,
}

/// The `view.set` payload. `mode` selects between an explicit camera pose and a
/// fit directive (SPEC §6: `{x,y,scale}` / `{fit:"all"|net|component}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ViewSet {
    /// Set the camera to an explicit center + scale.
    Explicit { x: f64, y: f64, scale: f64 },
    /// Fit the view to a target (whole board, the selected net, or a component).
    Fit { target: FitTarget },
}

/// What `view.set {fit:…}` frames (SPEC §6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitTarget {
    /// Frame the entire document bounds.
    All,
    /// Frame the bounding box of the currently selected net's entities.
    Net,
    /// Frame the bounding box of the currently selected component.
    Component,
}

// ===========================================================================
// HOST → APP : control verbs + the unified command line.
// ===========================================================================

/// The host's generic control verbs (`daemon/src/apps.rs::send_command`): the
/// daemon emits these on connect / refresh / stop regardless of app type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostControl {
    /// Begin / resume work.
    Start,
    /// Act now, ignoring any interval timer.
    Refresh,
    /// Stop and exit cleanly.
    Stop,
}

/// One parsed inbound line, after the app's reader classifies it. The daemon
/// sends control verbs as `{"type":"start"}`; Silicon-Canvas ops arrive as
/// `{"op":"…", …}`. [`crate::ipc`] decides which by presence of `op` vs `type`.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A host control verb (`{"type":"start"|"refresh"|"stop"}`).
    Control(HostControl),
    /// A Silicon-Canvas op (`{"op":"…", …}`).
    Op(Op),
}

// ===========================================================================
// APP → HOST : telemetry payloads (the `data` field, SPEC §2/§4/§5).
// ===========================================================================

/// `canvas.render_ms` — frame stats published at 1 Hz (SPEC §2). `p50`/`p95` are
/// per-frame CPU+submit milliseconds over the last second; `draws` is the draw
/// call count last frame (target ≤ 30); `culled_pct` is the percentage of
/// candidate instances dropped by viewport/LOD culling (0–100).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RenderMs {
    pub p50: f64,
    pub p95: f64,
    pub draws: u32,
    pub culled_pct: f64,
}

/// `canvas.viewport` — camera state on change, throttled to 10 Hz (SPEC §3). The
/// HUD mirrors this into a minimap. `x`/`y` is the view center in scene space;
/// `scale` is pixels-per-mm (the zoom). `layer_visibility` is the per-layer
/// on/off state, by KiCad layer name, so the HUD can show which layers are lit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    /// Layer name → visible. Empty for a schematic (single logical layer).
    pub layer_visibility: Vec<LayerVisibility>,
}

/// One entry of [`Viewport::layer_visibility`] — a (layer name, visible) pair.
/// A Vec of these (not a map) keeps a stable serialized order for the minimap.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LayerVisibility {
    pub layer: String,
    pub visible: bool,
}

/// `canvas.selection` — published on selection change AND once after ERC runs
/// (SPEC §4/§5). It carries the active net selection (if any), the active
/// component selection (if any), and the ERC marker list (after `erc.run` /
/// import). All three are optional so one channel serves selection updates and
/// the ERC result drop without separate topics. Untagged optionals: a pure
/// selection line omits `erc`; a pure ERC drop omits `net`/`component`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Selection {
    /// The currently highlighted net, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net: Option<NetSelection>,
    /// The currently selected component, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<ComponentSelection>,
    /// The ERC marker list — present on an ERC result drop (SPEC §5). Empty list
    /// means "ran clean"; `None`/absent means "this line is a plain selection
    /// update, not an ERC result".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub erc: Option<Vec<ErcMarker>>,
    /// The current trace front, present ONLY while tracing (SPEC §4). `trace.start`
    /// and each `trace.step` populate it so the HUD can drive the via-flash and the
    /// "step k of n" readout; `select.net`/`trace.stop` and every non-trace
    /// selection line leave it `None`/absent. HUD parses defensively — a plain
    /// selection or an ERC drop never carries this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceStep>,
}

/// The trace front sub-payload on `canvas.selection` (SPEC §4). Emitted by
/// `trace.start`/`trace.step` so the HUD can flash the via on a cross-layer step
/// and show progress ("step k of n"). Mirrors [`crate::trace::StepInfo`] on the
/// wire; absent on plain selections / ERC drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep {
    /// The entity now at the trace front (`{kind, index}`; `kind` is snake_case,
    /// e.g. `"via"`/`"track"`/`"pad"`). The HUD glides the camera to this entity.
    pub at: EntityRef,
    /// Electrical distance (BFS depth in edges) of `at` from the seed; the seed
    /// (`trace.start`) is `0`.
    pub distance: u32,
    /// True when this step lands on a via / crosses to a different copper layer —
    /// the HUD flashes the via and toggles layer emphasis (SPEC §4).
    pub crosses_layer: bool,
    /// 1-based ordinal of this front within the walk (`1..=of`): the "k" in
    /// "step k of n".
    pub step: u32,
    /// Total nodes in the walk: the "n" in "step k of n".
    pub of: u32,
    /// True when the front is on the last node — a further `trace.step` re-reports
    /// the same node (the walk does not wrap).
    pub at_end: bool,
}

/// The selected-net summary (SPEC §4: `{net, name, entity_count, pin_count}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetSelection {
    /// The net's raw id (index), for cross-probe correlation.
    pub net: NetId,
    /// Human net name, e.g. "3V3".
    pub name: String,
    /// Total entities highlighted on this net.
    pub entity_count: u32,
    /// Pads/pins on this net.
    pub pin_count: u32,
}

/// The selected-component summary, for the HUD's info panel + cross-probe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentSelection {
    pub component: ComponentId,
    /// Reference designator, e.g. "U3".
    pub reference: String,
    /// Component value, e.g. "STM32F405".
    pub value: String,
    /// Pad/pin count on the component.
    pub pin_count: u32,
}

// ===========================================================================
// ERC (SPEC §5).
// ===========================================================================

/// ERC marker severity (SPEC §5: amber warning / red error).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ErcSeverity {
    Warning,
    Error,
}

/// One ERC finding (SPEC §5: `{code, severity, at, message}`). `code` is a
/// stable machine code (e.g. `"unconnected_pin"`, `"output_conflict"`); `at` is
/// the fault coordinate in scene space (where the badge renders); `message` is
/// the human description for the panel-side list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErcMarker {
    /// Stable machine code. See [`ErcCode`] for the enumerated set.
    pub code: String,
    pub severity: ErcSeverity,
    /// Fault location in scene space (the badge anchor; SPEC §5).
    pub at: Point,
    /// Human-readable description for the panel list.
    pub message: String,
}

/// The enumerated ERC check codes (SPEC §5). Kept as an enum so the `erc` agent
/// emits stable, documented `code` strings (via [`ErcCode::as_str`]) rather than
/// ad-hoc literals; [`ErcMarker::code`] stays a `String` on the wire for forward
/// compatibility. ERC is advisory and conservative — only what is provable from
/// the netlist + pin metadata (SPEC §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErcCode {
    /// A non-NC pin with no electrical connection.
    UnconnectedPin,
    /// Two outputs (or multiple drivers) tied to one net.
    OutputConflict,
    /// A power-input pin on a net with no driving source / power flag.
    PowerNoDriver,
    /// A wire end that connects to nothing.
    DanglingWire,
    /// Two components share a reference designator.
    DuplicateReference,
    /// A single-ended named net (label used once) — a likely typo.
    LabelTypo,
}

impl ErcCode {
    /// The stable wire string for this code (matches the serde `snake_case`).
    pub fn as_str(self) -> &'static str {
        match self {
            ErcCode::UnconnectedPin => "unconnected_pin",
            ErcCode::OutputConflict => "output_conflict",
            ErcCode::PowerNoDriver => "power_no_driver",
            ErcCode::DanglingWire => "dangling_wire",
            ErcCode::DuplicateReference => "duplicate_reference",
            ErcCode::LabelTypo => "label_typo",
        }
    }

    /// The default severity SPEC §5 assigns this check (conflicts/power/dupes are
    /// errors; unconnected/dangling/typo are warnings). The `erc` agent may
    /// override per-instance, but this is the documented baseline.
    pub fn default_severity(self) -> ErcSeverity {
        match self {
            ErcCode::OutputConflict
            | ErcCode::PowerNoDriver
            | ErcCode::DuplicateReference => ErcSeverity::Error,
            ErcCode::UnconnectedPin | ErcCode::DanglingWire | ErcCode::LabelTypo => {
                ErcSeverity::Warning
            }
        }
    }
}

// ===========================================================================
// APP → HOST : the unified telemetry union + the wire envelope.
// ===========================================================================

/// The typed union of everything Silicon Canvas publishes. [`Telemetry::topic`]
/// maps each to its declared manifest topic so the host's `resolve_topic` accepts
/// it (an app can never publish to an undeclared topic — `apps.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Telemetry {
    RenderMs(RenderMs),
    Viewport(Viewport),
    Selection(Selection),
}

/// The three telemetry topics Silicon Canvas declares in its manifest.
pub const TOPIC_RENDER_MS: &str = "canvas.render_ms";
pub const TOPIC_VIEWPORT: &str = "canvas.viewport";
pub const TOPIC_SELECTION: &str = "canvas.selection";

impl Telemetry {
    /// The manifest topic this payload publishes under.
    pub fn topic(&self) -> &'static str {
        match self {
            Telemetry::RenderMs(_) => TOPIC_RENDER_MS,
            Telemetry::Viewport(_) => TOPIC_VIEWPORT,
            Telemetry::Selection(_) => TOPIC_SELECTION,
        }
    }
}

/// The exact App→host wire line shape the daemon expects (`apps.rs`
/// `classify_inbound_line`): `{"token", "type":"items", "data":{...}}`. The
/// host reads `data.topic` (must be a declared topic) and relays the whole
/// `data` as the payload. Silicon Canvas uses `type:"items"` for every telemetry
/// drop and nests `{topic, payload}` under `data` so the host routes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboundLine {
    /// The capability token from `DARWIN_APP_TOKEN`, stamped on every line.
    pub token: String,
    /// Always `"items"` for a telemetry drop (the host relays items/status as
    /// app.data; `"log"` is used only for log lines).
    #[serde(rename = "type")]
    pub kind: String,
    pub data: OutboundData,
}

/// The `data` field of an [`OutboundLine`]: the topic the host routes on plus the
/// typed payload. `topic` MUST be one of the manifest's declared topics or the
/// host falls back to its default topic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboundData {
    pub topic: String,
    // FLATTENED into `data` alongside `topic`, so the wire shape is
    // {topic, <payload fields>} — matching Vision's convention and the HUD parsers,
    // which read the fields FLAT (num(data,"p50"), ...). A nested {topic, payload:{…}}
    // leaves every HUD field one level too deep and renders blank panels (the bug
    // this closes). The daemon relays `data` verbatim; the HUD ignores `topic`.
    #[serde(flatten)]
    pub payload: Telemetry,
}

impl OutboundLine {
    /// Build a telemetry line stamped with the capability token, routed to the
    /// payload's declared topic. This is the single constructor the `ipc` agent
    /// uses to emit any of the three telemetry kinds.
    pub fn telemetry(token: impl Into<String>, payload: Telemetry) -> Self {
        let topic = payload.topic().to_string();
        OutboundLine {
            token: token.into(),
            kind: "items".to_string(),
            data: OutboundData { topic, payload },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_deserializes_with_dotted_names() {
        let j = r#"{"op":"select.net","name":"3V3"}"#;
        let op: Op = serde_json::from_str(j).unwrap();
        assert_eq!(op, Op::SelectNet { name: "3V3".into() });

        let j = r#"{"op":"project.open","path":"projects/board.kicad_sch"}"#;
        assert_eq!(
            serde_json::from_str::<Op>(j).unwrap(),
            Op::ProjectOpen { path: "projects/board.kicad_sch".into() }
        );
    }

    #[test]
    fn view_set_explicit_and_fit() {
        let explicit: Op =
            serde_json::from_str(r#"{"op":"view.set","mode":"explicit","x":1.0,"y":2.0,"scale":10.0}"#)
                .unwrap();
        assert_eq!(
            explicit,
            Op::ViewSet(ViewSet::Explicit { x: 1.0, y: 2.0, scale: 10.0 })
        );

        let fit: Op =
            serde_json::from_str(r#"{"op":"view.set","mode":"fit","target":"all"}"#).unwrap();
        assert_eq!(fit, Op::ViewSet(ViewSet::Fit { target: FitTarget::All }));
    }

    #[test]
    fn unit_ops_roundtrip() {
        for (j, want) in [
            (r#"{"op":"trace.start"}"#, Op::TraceStart),
            (r#"{"op":"trace.step"}"#, Op::TraceStep),
            (r#"{"op":"trace.stop"}"#, Op::TraceStop),
            (r#"{"op":"erc.run"}"#, Op::ErcRun),
        ] {
            assert_eq!(serde_json::from_str::<Op>(j).unwrap(), want);
        }
    }

    #[test]
    fn layer_set_op() {
        let op: Op =
            serde_json::from_str(r#"{"op":"layer.set","layer":"B.Cu","visible":false}"#).unwrap();
        assert_eq!(op, Op::LayerSet { layer: "B.Cu".into(), visible: false });
    }

    #[test]
    fn render_ms_payload_shape() {
        let r = RenderMs { p50: 4.1, p95: 9.0, draws: 22, culled_pct: 73.5 };
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, r#"{"p50":4.1,"p95":9.0,"draws":22,"culled_pct":73.5}"#);
    }

    #[test]
    fn selection_omits_absent_fields() {
        // A pure net-selection line: no component, no erc.
        let sel = Selection {
            net: Some(NetSelection {
                net: NetId::new(3),
                name: "3V3".into(),
                entity_count: 12,
                pin_count: 5,
            }),
            component: None,
            erc: None,
            trace: None,
        };
        let j = serde_json::to_string(&sel).unwrap();
        assert!(j.contains("\"net\""));
        assert!(!j.contains("component"));
        assert!(!j.contains("erc"));
        // The optional trace sub-payload is omitted on a plain selection line so
        // the HUD can parse it defensively (absent == not tracing).
        assert!(!j.contains("trace"));
    }

    #[test]
    fn erc_result_drop_shape() {
        // An ERC result line: just the erc list (SPEC §5: {erc:[{code,severity,at,message}]}).
        let sel = Selection {
            net: None,
            component: None,
            erc: Some(vec![ErcMarker {
                code: ErcCode::UnconnectedPin.as_str().to_string(),
                severity: ErcSeverity::Warning,
                at: Point::new(10.0, 20.0),
                message: "pin 3 of U1 unconnected".into(),
            }]),
            trace: None,
        };
        let j = serde_json::to_string(&sel).unwrap();
        assert!(j.contains(r#""erc":[{"code":"unconnected_pin","severity":"warning""#), "{j}");
        assert!(j.contains(r#""at":{"x":10.0,"y":20.0}"#), "{j}");
        // An ERC result drop carries no trace front.
        assert!(!j.contains("trace"));
    }

    #[test]
    fn selection_trace_subpayload_wire_shape() {
        // The trace sub-payload on canvas.selection (SPEC §4 via-flash / step k of
        // n). This locks the EXACT field names + JSON types the HUD parses:
        // trace:{at:{kind,index}, distance:u32, crosses_layer:bool, step:u32,
        // of:u32, at_end:bool}.
        let sel = Selection {
            net: None,
            component: None,
            erc: None,
            trace: Some(TraceStep {
                at: EntityRef::new(crate::ids::EntityKind::Via, 7),
                distance: 2,
                crosses_layer: true,
                step: 3,
                of: 5,
                at_end: false,
            }),
        };
        let j = serde_json::to_string(&sel).unwrap();
        assert!(
            j.contains(r#""trace":{"at":{"kind":"via","index":7},"distance":2,"crosses_layer":true,"step":3,"of":5,"at_end":false}"#),
            "{j}"
        );
        // And it round-trips back to the same struct (HUD-side parse contract).
        let back: Selection = serde_json::from_str(&j).unwrap();
        assert_eq!(back, sel);
        // A plain selection (no trace) still omits the field entirely.
        let plain = Selection::default();
        assert!(!serde_json::to_string(&plain).unwrap().contains("trace"));
    }

    #[test]
    fn erc_code_strings_and_severity() {
        assert_eq!(ErcCode::OutputConflict.as_str(), "output_conflict");
        assert_eq!(ErcCode::OutputConflict.default_severity(), ErcSeverity::Error);
        assert_eq!(ErcCode::UnconnectedPin.default_severity(), ErcSeverity::Warning);
        // serde rename matches as_str.
        let j = serde_json::to_string(&ErcCode::PowerNoDriver).unwrap();
        assert_eq!(j, "\"power_no_driver\"");
    }

    #[test]
    fn outbound_line_routes_to_topic() {
        let line = OutboundLine::telemetry(
            "tok123",
            Telemetry::RenderMs(RenderMs { p50: 1.0, p95: 2.0, draws: 10, culled_pct: 0.0 }),
        );
        assert_eq!(line.kind, "items");
        assert_eq!(line.data.topic, TOPIC_RENDER_MS);
        let j = serde_json::to_string(&line).unwrap();
        assert!(j.contains(r#""type":"items""#));
        assert!(j.contains(r#""topic":"canvas.render_ms""#));
        assert!(j.contains(r#""token":"tok123""#));
    }

    /// CONTRACT LOCK with the daemon voice router. The daemon
    /// (`daemon/src/router.rs::silicon_canvas_command`) builds these EXACT op
    /// strings from spoken phrases and forwards them verbatim — it never imports
    /// this standalone crate, so this test is the load-bearing guarantee that
    /// the wire form the router emits deserializes into the intended [`Op`]. If
    /// either side renames a tag/field, this fails. Keep these literals byte-for-
    /// byte identical to the router's `op_*` builders.
    #[test]
    fn daemon_voice_router_op_strings_deserialize() {
        let cases: &[(&str, Op)] = &[
            // "show me the 3V3 net" -> select.net
            (r#"{"op":"select.net","name":"3V3"}"#, Op::SelectNet { name: "3V3".into() }),
            // "select component U3" -> select.component
            (
                r#"{"op":"select.component","name":"U3"}"#,
                Op::SelectComponent { name: "U3".into() },
            ),
            // "trace this net" / "step the trace" / "stop tracing"
            (r#"{"op":"trace.start"}"#, Op::TraceStart),
            (r#"{"op":"trace.step"}"#, Op::TraceStep),
            (r#"{"op":"trace.stop"}"#, Op::TraceStop),
            // "run ERC"
            (r#"{"op":"erc.run"}"#, Op::ErcRun),
            // "fit the board"
            (
                r#"{"op":"view.set","mode":"fit","target":"all"}"#,
                Op::ViewSet(ViewSet::Fit { target: FitTarget::All }),
            ),
        ];
        for (json, want) in cases {
            let got: Op = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("router op string {json:?} must deserialize: {e}"));
            assert_eq!(&got, want, "router op string {json:?} -> wrong Op");
        }
    }

    #[test]
    fn telemetry_topic_mapping() {
        assert_eq!(
            Telemetry::Viewport(Viewport {
                x: 0.0,
                y: 0.0,
                scale: 1.0,
                layer_visibility: vec![],
            })
            .topic(),
            TOPIC_VIEWPORT
        );
        assert_eq!(Telemetry::Selection(Selection::default()).topic(), TOPIC_SELECTION);
    }
}
