//! Silicon Canvas — GPU schematic/PCB renderer micro-app for Project DARWIN.
//!
//! Crate layout (one module per build phase; SPEC milestones §7):
//!
//!   shared types (FROZEN — written in Foundation, do not change):
//!     - [`ids`]     index newtypes + [`ids::EntityKind`] / [`ids::EntityRef`].
//!     - [`scene`]   the struct-of-arrays [`scene::Scene`], [`scene::Point`],
//!                   and the [`scene::Point::quantize`] endpoint-matching key.
//!     - [`sexpr`]   the generic KiCad S-expression lexer + [`sexpr::Value`].
//!     - [`ops`]     the SPEC §6 IPC op enum + telemetry payload structs.
//!     - [`error`]   the crate error types ([`error::CanvasError`]).
//!
//!   module-agent files (stubs here; one agent each builds against the above):
//!     - [`parser`]  KiCad `.kicad_sch`/`.kicad_pcb`/`.kicad_sym`/`.kicad_mod`
//!                   interpretation over `sexpr::Value` → a [`scene::Scene`].
//!     - [`graph`]   the connectivity graph (endpoint-quantize for schematics,
//!                   copper overlap + via stitching for PCBs).
//!     - [`rtree`]   one R-tree per layer/class for hit-test + viewport cull.
//!     - [`erc`]     electrical rule checks → [`ops::ErcMarker`] list (SPEC §5).
//!     - [`trace`]   net highlight + interactive trace walk (SPEC §4).
//!     - [`ipc`]     the JSONL op/telemetry loop over the per-app Unix socket.
//!     - [`render`]  the wgpu/Metal renderer — ONLY with the `gpu` feature.
//!
//! The non-gpu core builds and tests headlessly; the renderer is behind the
//! `gpu` feature so `cargo test` works without a Metal device.

// ---- shared type modules (the frozen contract) ----------------------------
pub mod error;
pub mod ids;
pub mod ops;
pub mod scene;
pub mod sexpr;

// ---- module-agent files (filled by downstream agents) ---------------------
pub mod erc;
pub mod graph;
pub mod ipc;
pub mod parser;
pub mod rtree;
pub mod trace;

// ---- GPU renderer (Metal/wgpu) — only with the `gpu` feature --------------
#[cfg(feature = "gpu")]
pub mod render;

// ---- key public re-exports ------------------------------------------------
// The types downstream agents and the binary reach for most often, surfaced at
// the crate root so call sites can `use silicon_canvas::{Scene, Op, ...}`.
pub use error::{CanvasError, Result, SexprError};
pub use ids::{
    ComponentId, EntityKind, EntityRef, JunctionId, LabelId, NetId, NodeId, PadId, SheetId,
    TrackId, ViaId, WireId, ZoneId,
};
pub use ops::{
    Command, ComponentSelection, ErcCode, ErcMarker, ErcSeverity, FitTarget, HostControl,
    LayerVisibility, NetSelection, Op, OutboundData, OutboundLine, RenderMs, Selection, Telemetry,
    ViewSet, Viewport, TOPIC_RENDER_MS, TOPIC_SELECTION, TOPIC_VIEWPORT,
};
pub use scene::{
    Aabb, Component, DocumentKind, HighlightFlag, Junction, Label, LabelKind, LayerId, Pad,
    PadShape, PinType, Point, QuantKey, Scene, SceneKind, Sheet, Track, Via, Wire, QUANTUM_MM,
};
pub use sexpr::{parse as parse_sexpr, parse_many as parse_sexpr_many, Lexer, Token, Value};
