//! Mark-Forge — deterministic 3D rigid-body physics sandbox for Project DARWIN.
//!
//! A fixed-timestep, impulse-based rigid-body engine (pure CPU/f64) that emits
//! compact body-transform telemetry over the per-app JSONL socket, which the
//! HUD's React-Three-Fiber sandbox panel renders. The WHOLE engine is verifiable
//! headlessly (`cargo build` + `cargo test`); the live R3F render is device-gated
//! (SPEC §8 — verified only on the real Tauri app on a real Apple Silicon Mac).
//!
//! Crate layout (one module per build phase; SPEC §9 milestones):
//!
//!   shared types (FROZEN — written in Foundation, do NOT change):
//!     - [`math`]        [`math::Vec3`] / [`math::Quat`] / [`math::Mat3`] + ops +
//!                       the inertia helpers (SPEC §1/§2).
//!     - [`body`]        [`body::Body`], [`body::Shape`], [`body::Material`],
//!                       [`body::BodyId`] + the mass→inverse-inertia spawn path.
//!     - [`world`]       [`world::World`] (bodies + gravity + dt + substeps +
//!                       params), [`world::SolverParams`], [`world::StepStats`].
//!     - [`broadphase`]  the [`broadphase::Aabb`] type + the
//!                       [`broadphase::Pair`]/`candidate_pairs` contract.
//!     - [`narrowphase`] the [`narrowphase::Contact`]/[`narrowphase::Manifold`]
//!                       types + the `collide` contract.
//!     - [`solver`]      the `solve_velocity`/`correct_positions` contract.
//!     - [`ipc`]         the [`ipc::AppEnv`] env contract, the SPEC §7 [`ipc::Op`]
//!                       enum, the `physics.*` telemetry types (SPEC §8), the
//!                       [`ipc::OutboundLine`] wire shape, and the PURE op
//!                       [`ipc::dispatch`]er.
//!     - [`error`]       the crate error type ([`error::MarkForgeError`]).
//!
//!   engine pipeline (fully implemented, each in its own file against the above):
//!     - [`world::World::step`] — the fixed-timestep integrator body.
//!     - [`broadphase::candidate_pairs`] — grid + brute-force oracle.
//!     - [`narrowphase::collide`] — sphere/plane/box SAT contacts.
//!     - [`solver::solve_velocity`] / [`solver::correct_positions`] — the
//!       sequential-impulse solver.
//!
//! The whole crate builds and tests headlessly; the integrator, broadphase,
//! narrowphase, and solver are all wired and exercised end-to-end by the
//! conservation-law + golden-trace tests.

// ---- shared type modules (the frozen contract) ----------------------------
pub mod body;
pub mod error;
pub mod ipc;
pub mod math;
pub mod world;

// ---- engine pipeline modules (contract types frozen; algorithm bodies wired) --
pub mod broadphase;
pub mod narrowphase;
pub mod solver;

// ---- key public re-exports ------------------------------------------------
// The types downstream agents + the binary reach for most often, surfaced at the
// crate root so call sites can `use mark_forge::{World, Body, Op, ...}`.
pub use body::{Body, BodyId, Material, Shape};
pub use broadphase::{Aabb, Pair};
pub use error::{MarkForgeError, Result};
pub use math::{Mat3, Quat, Vec3};
pub use narrowphase::{Contact, Manifold};
pub use world::{SolverParams, StepStats, World};

pub use ipc::{
    AppEnv, AppState, BodiesFrame, BodyTransform, Command, HostControl, Op, OutboundData,
    OutboundLine, ParamsPatch, SceneBody, SceneTopology, ShapeTag, SpawnSpec, StepReport, Telemetry,
    TOPIC_BODIES, TOPIC_SCENE, TOPIC_STEP,
};
