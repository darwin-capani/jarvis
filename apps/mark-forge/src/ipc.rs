//! IPC (SPEC §7/§8) — the launch env, the op enum, the `physics.*` telemetry
//! shapes, the wire envelope, and the JSONL socket loop.
//!
//! This module is the CONTRACT for everything that crosses the per-app Unix
//! socket (`daemon/src/apps.rs`). The op/telemetry TYPES + the [`AppEnv`]
//! env-var contract + the [`OutboundLine`] wire shape are FROZEN; the dispatcher
//! BODY ([`dispatch`]) is fully wired to the [`World`] engine — every op drives
//! real physics. Downstream agents and the HUD R3F panel match these field names
//! verbatim.
//!
//! Runtime contract (`daemon/src/apps.rs`, runtime = "binary", mirror of
//! silicon-canvas): jarvisd execs the prebuilt `mark-forge` binary under
//! `sandbox-exec`, handing it the socket + token via the ENVIRONMENT (NEVER argv
//! — argv is world-readable via `ps`):
//!   - `JARVIS_APP_SOCKET` — abs path of `state/ipc/apps/mark-forge.sock`
//!   - `JARVIS_APP_TOKEN`  — the capability token stamped on every outbound line
//!   - `JARVIS_APP_NAME`   — "mark-forge"
//!
//! Inbound (host → app), one JSON object per line:
//!   - `{"type":"start"|"refresh"|"stop"}` — daemon control verbs
//!     ([`HostControl`]); `stop` exits cleanly.
//!   - `{"op":"…", …}` — Mark-Forge ops ([`Op`], SPEC §7).
//!   [`parse_command`] classifies each line by presence of `op` vs `type`.
//!
//! Outbound (app → host), stamped with the token on EVERY line: the daemon
//! relays `type:"items"` lines whose `data.topic` is a declared topic
//! (`physics.bodies` / `physics.step` / `physics.scene`).
//!
//! HARD SAFETY (never violated): this binds NO listener (it CONNECTS to the
//! daemon's socket), opens NO window, touches NO GPU, plays NO audio. The engine
//! is pure CPU/f64; the HUD renders.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::body::{BodyId, Material, Shape};
use crate::error::{MarkForgeError, Result};
use crate::math::Vec3;
use crate::world::{SolverParams, StepStats, World};

// ===========================================================================
// AppEnv — the launch environment (daemon/src/apps.rs).
// ===========================================================================

/// The launch environment the daemon hands the app (`daemon/src/apps.rs`): the
/// per-app socket path + the capability token to stamp on every outbound line +
/// the app name. Read once at startup from the env (NEVER argv). Identical
/// contract to silicon-canvas's `AppEnv` so the binary's `main` maps an absent
/// socket/token to exit code 2.
#[derive(Debug, Clone)]
pub struct AppEnv {
    /// Absolute path of `state/ipc/apps/mark-forge.sock`.
    pub socket_path: PathBuf,
    /// The capability token, stamped on every App→host line.
    pub token: String,
    /// This app's name ("mark-forge").
    pub name: String,
}

impl AppEnv {
    /// Read the launch env exactly as `apps.rs` establishes it. Returns
    /// [`MarkForgeError::Unauthorized`] when the socket or token is absent — the
    /// app only runs under the daemon (the binary's `main` maps this to exit
    /// code 2).
    pub fn from_env() -> Result<Self> {
        let socket_path = std::env::var_os("JARVIS_APP_SOCKET")
            .map(PathBuf::from)
            .ok_or(MarkForgeError::Unauthorized)?;
        let token = std::env::var("JARVIS_APP_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
            .ok_or(MarkForgeError::Unauthorized)?;
        let name = std::env::var("JARVIS_APP_NAME").unwrap_or_else(|_| "mark-forge".to_string());
        Ok(AppEnv { socket_path, token, name })
    }
}

// ===========================================================================
// HOST → APP : the op request enum (SPEC §7).
// ===========================================================================

/// A Mark-Forge operation request (SPEC §7, the `op` table). Externally tagged
/// on the `op` field with the SPEC's dotted names, so the wire form is e.g.
/// `{"op":"world.step","n":10}` or
/// `{"op":"body.spawn","shape":{"kind":"sphere","radius":0.5},"pos":[0,5,0]}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Op {
    /// `world.reset {}` — clear all bodies, keep gravity/dt/substeps/params.
    #[serde(rename = "world.reset")]
    WorldReset,

    /// `body.spawn {shape, pos, lin_vel?, ang_vel?, mass?, material?}` — add one
    /// body and report its new [`BodyId`] in the next `physics.scene`.
    #[serde(rename = "body.spawn")]
    BodySpawn(SpawnSpec),

    /// `world.step {n}` — advance exactly `n` fixed frames (each `substeps`
    /// sub-frames).
    #[serde(rename = "world.step")]
    WorldStep { n: u32 },

    /// `set.gravity {x, y, z}` — replace the gravity vector.
    #[serde(rename = "set.gravity")]
    SetGravity { x: f64, y: f64, z: f64 },

    /// `set.params {…}` — update timestep/substeps/solver params (every field
    /// optional; absent fields keep their current value, see [`ParamsPatch`]).
    #[serde(rename = "set.params")]
    SetParams(ParamsPatch),

    /// `state.get {}` — re-publish `physics.scene` + `physics.bodies` now.
    #[serde(rename = "state.get")]
    StateGet,
}

/// The `body.spawn` payload (SPEC §7). `shape` + `pos` are required; velocities,
/// mass, and material default (a `mass` of `None` or `<= 0` ⇒ a static body, per
/// [`crate::body::Body::new`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// The collision shape ([`Shape`], tagged on `kind`).
    pub shape: Shape,
    /// Initial world-space position.
    pub pos: Vec3,
    /// Initial linear velocity (default zero).
    #[serde(default)]
    pub lin_vel: Option<Vec3>,
    /// Initial angular velocity (default zero).
    #[serde(default)]
    pub ang_vel: Option<Vec3>,
    /// Mass; `None` or `<= 0` ⇒ static (default `None`).
    #[serde(default)]
    pub mass: Option<f64>,
    /// Contact material (default [`Material::default`]).
    #[serde(default)]
    pub material: Option<Material>,
}

/// The `set.params` patch (SPEC §7): every field is optional so an op can tweak
/// one knob without resending the whole [`SolverParams`] + timestep set. Applied
/// by [`ParamsPatch::apply`] onto the live [`World`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ParamsPatch {
    /// New fixed timestep `dt = 1/hz`.
    #[serde(default)]
    pub dt: Option<f64>,
    /// New substep count (clamped `>= 1` on apply).
    #[serde(default)]
    pub substeps: Option<u32>,
    #[serde(default)]
    pub velocity_iterations: Option<u32>,
    #[serde(default)]
    pub restitution_threshold: Option<f64>,
    #[serde(default)]
    pub baumgarte_beta: Option<f64>,
    #[serde(default)]
    pub penetration_slop: Option<f64>,
    #[serde(default)]
    pub linear_damping: Option<f64>,
    #[serde(default)]
    pub angular_damping: Option<f64>,
}

impl ParamsPatch {
    /// Apply the present fields onto the world's timestep + [`SolverParams`],
    /// leaving absent fields unchanged. `substeps` is clamped to `>= 1`.
    pub fn apply(&self, world: &mut World) {
        if let Some(dt) = self.dt {
            world.dt = dt;
        }
        if let Some(s) = self.substeps {
            world.substeps = s.max(1);
        }
        let p: &mut SolverParams = &mut world.params;
        if let Some(v) = self.velocity_iterations {
            p.velocity_iterations = v;
        }
        if let Some(v) = self.restitution_threshold {
            p.restitution_threshold = v;
        }
        if let Some(v) = self.baumgarte_beta {
            p.baumgarte_beta = v;
        }
        if let Some(v) = self.penetration_slop {
            p.penetration_slop = v;
        }
        if let Some(v) = self.linear_damping {
            p.linear_damping = v;
        }
        if let Some(v) = self.angular_damping {
            p.angular_damping = v;
        }
    }
}

// ===========================================================================
// HOST → APP : control verbs + the unified command line.
// ===========================================================================

/// The daemon's generic control verbs (`daemon/src/apps.rs::send_command`):
/// emitted on connect / refresh / stop regardless of app type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostControl {
    /// Begin / resume work (re-publish current state).
    Start,
    /// Act now (re-publish current state).
    Refresh,
    /// Stop and exit cleanly.
    Stop,
}

/// One parsed inbound line: a daemon control verb or a Mark-Forge op.
/// [`parse_command`] decides which by presence of `op` vs `type`.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// A daemon control verb (`{"type":"start"|"refresh"|"stop"}`).
    Control(HostControl),
    /// A Mark-Forge op (`{"op":"…", …}`).
    Op(Op),
}

/// Classify one inbound JSONL line into a [`Command`]. A line carrying `"op"` is
/// a Mark-Forge op; a line carrying `"type"` is a daemon control verb. Anything
/// else — malformed JSON, neither field, an unknown verb/op — is a clean
/// [`MarkForgeError::Protocol`] (NEVER a panic), so the caller drops the line and
/// keeps serving. `op` is checked first (a line with both is treated as an op).
pub fn parse_command(line: &str) -> Result<Command> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(MarkForgeError::protocol("empty line"));
    }
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| MarkForgeError::protocol(format!("not JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| MarkForgeError::protocol("inbound line is not a JSON object"))?;

    if obj.contains_key("op") {
        let op: Op = serde_json::from_value(value)
            .map_err(|e| MarkForgeError::protocol(format!("unknown or malformed op: {e}")))?;
        return Ok(Command::Op(op));
    }
    if let Some(ty) = obj.get("type").and_then(|v| v.as_str()) {
        let ctrl = match ty {
            "start" => HostControl::Start,
            "refresh" => HostControl::Refresh,
            "stop" => HostControl::Stop,
            other => {
                return Err(MarkForgeError::protocol(format!(
                    "unknown host control verb {other:?}"
                )))
            }
        };
        return Ok(Command::Control(ctrl));
    }
    Err(MarkForgeError::protocol(
        "inbound line has neither an `op` nor a `type` field",
    ))
}

// ===========================================================================
// APP → HOST : the `physics.*` telemetry payloads (SPEC §8).
// ===========================================================================

/// The three telemetry topics Mark-Forge declares in its manifest
/// (`apps/mark-forge/manifest.toml` `telemetry_topics`). The HUD's R3F panel
/// keys off these exact strings.
pub const TOPIC_BODIES: &str = "physics.bodies";
pub const TOPIC_STEP: &str = "physics.step";
pub const TOPIC_SCENE: &str = "physics.scene";

/// Hard cap on the number of bodies a single world may hold. A `body.spawn` op
/// once the world is at the cap is REFUSED (the body is not added) rather than
/// allowed to grow unbounded — a defensive bound so a hostile or buggy op
/// stream cannot exhaust memory or wedge the O(n²)-ish broadphase/solver. The
/// cap is generous (a sandbox scene is tens of bodies, not thousands) but
/// finite. `dispatch` enforces it WITHOUT changing its frozen signature: an
/// over-cap spawn simply re-publishes the unchanged scene (a no-op spawn), and
/// the binary's socket loop logs the refusal as a `log` line so the operator
/// sees it. Never a panic.
pub const MAX_BODIES: u32 = 4096;

/// Largest fixed timestep `set.params {dt}` may set. `dt` must be finite and in
/// `(0, MAX_DT]`: a non-positive `dt` stalls (or reverses) time and a
/// finite-but-absurd `dt` (e.g. `1e308`) drives `pos += vel*dt` straight to
/// infinity in one substep — the same poison `body.spawn`'s finite gate keeps
/// out. `1.0` s/frame is already extreme slow-motion (the default is `1/60`),
/// far past any interactive use, yet finite — a defensible upper bound.
pub const MAX_DT: f64 = 1.0;

/// Largest per-axis magnitude `set.gravity` (or any single gravity component)
/// may take. Gravity must be finite and each component within `±MAX_GRAVITY`: a
/// finite-but-absurd gravity (e.g. `1e308` on Y) accelerates every body to
/// infinity within a step, the same way a non-finite one would. `1e6` m/s² is
/// ~10⁵ g — beyond any plausible sandbox scenario — but finite and bounded.
pub const MAX_GRAVITY: f64 = 1.0e6;

/// One body's render transform on the `physics.bodies` feed (SPEC §8). `pos` is
/// `[x,y,z]`, `quat` is `[x,y,z,w]` (the R3F panel feeds these straight into
/// `THREE.Vector3`/`THREE.Quaternion`). `shape` is the shape's kind string so the
/// panel knows which mesh to draw without a separate lookup; `sleeping` dims a
/// settled body.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BodyTransform {
    pub id: BodyId,
    /// The shape kind tag (`"sphere"`/`"cuboid"`/`"plane"`).
    pub shape: ShapeTag,
    pub pos: Vec3,
    /// Orientation as `[x, y, z, w]`.
    pub quat: crate::math::Quat,
    pub sleeping: bool,
}

/// The compact shape descriptor echoed on telemetry so the HUD can size + pick
/// the mesh for each body (SPEC §8). Carries the kind + the one size field that
/// kind needs; serialized like [`Shape`] (tagged on `kind`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ShapeTag {
    Sphere { radius: f64 },
    Cuboid { half_extents: Vec3 },
    Plane { normal: Vec3, offset: f64 },
}

impl From<Shape> for ShapeTag {
    fn from(s: Shape) -> Self {
        match s {
            Shape::Sphere { radius } => ShapeTag::Sphere { radius },
            Shape::Cuboid { half_extents } => ShapeTag::Cuboid { half_extents },
            Shape::Plane { normal, offset } => ShapeTag::Plane { normal, offset },
        }
    }
}

/// `physics.bodies` — the per-frame render feed the R3F panel consumes (SPEC §8).
/// `frame` is the cumulative frame counter; `sim_time` is the simulated seconds
/// elapsed; `bodies` is every body's transform in stable [`BodyId`] order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BodiesFrame {
    pub frame: u64,
    pub sim_time: f64,
    pub bodies: Vec<BodyTransform>,
}

/// `physics.step` — solver/step stats for the HUD readout (SPEC §8). A serde
/// mirror of [`crate::world::StepStats`] with the wire field names.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StepReport {
    pub frames: u32,
    pub substeps: u32,
    pub bodies: u32,
    pub contacts: u32,
    pub solver_iterations: u32,
    pub last_penetration: f64,
    /// `true` if the per-substep contact budget
    /// ([`crate::world::MAX_CONTACTS_PER_SUBSTEP`]) bit during this step — a
    /// degenerate / over-dense scene whose contacts were truncated to keep the
    /// per-step work bounded. A SIGNAL the HUD/operator can surface; never a
    /// silent mis-simulation. `#[serde(default)]` so a line without it (older
    /// producers / hand-rolled fixtures) still deserializes as `false`.
    #[serde(default)]
    pub contact_cap_hit: bool,
    /// `true` if the per-substep candidate-pair budget
    /// ([`crate::world::MAX_PAIRS_PER_SUBSTEP`]) bit during this step — the
    /// broadphase short-circuited its pair enumeration because the scene was too
    /// dense (every body collapsed into one cell ⇒ would-be O(n²) AABB tests). The
    /// UPSTREAM signal that bounds the work before narrowphase. Same SIGNAL
    /// discipline as [`StepReport::contact_cap_hit`]; `#[serde(default)]` so older
    /// producers / fixtures without the field still deserialize as `false`.
    #[serde(default)]
    pub pairs_cap_hit: bool,
}

impl From<StepStats> for StepReport {
    fn from(s: StepStats) -> Self {
        StepReport {
            frames: s.frames,
            substeps: s.substeps,
            bodies: s.bodies,
            contacts: s.contacts,
            solver_iterations: s.solver_iterations,
            last_penetration: s.last_penetration,
            contact_cap_hit: s.contact_cap_hit,
            pairs_cap_hit: s.pairs_cap_hit,
        }
    }
}

/// One body's topology entry on the `physics.scene` feed (SPEC §8): its id, its
/// shape descriptor, and whether it is static (so the HUD can render planes /
/// anchors differently).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SceneBody {
    pub id: BodyId,
    pub shape: ShapeTag,
    pub r#static: bool,
}

/// `physics.scene` — scene topology published on change (spawn/reset, SPEC §8).
/// Carries the simulation params + every body's static topology so the HUD can
/// (re)build its meshes; the per-frame motion then arrives on `physics.bodies`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneTopology {
    pub gravity: Vec3,
    pub dt: f64,
    pub substeps: u32,
    pub bodies: Vec<SceneBody>,
}

/// The typed union of everything Mark-Forge publishes (SPEC §8). [`Telemetry::topic`]
/// maps each to its declared manifest topic so the daemon's `resolve_topic`
/// accepts it (an app can never publish to an undeclared topic — `apps.rs`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Telemetry {
    Bodies(BodiesFrame),
    Step(StepReport),
    Scene(SceneTopology),
}

impl Telemetry {
    /// The manifest topic this payload publishes under.
    pub fn topic(&self) -> &'static str {
        match self {
            Telemetry::Bodies(_) => TOPIC_BODIES,
            Telemetry::Step(_) => TOPIC_STEP,
            Telemetry::Scene(_) => TOPIC_SCENE,
        }
    }
}

/// The exact App→host wire line shape the daemon expects
/// (`apps.rs::classify_inbound_line`): `{"token","type":"items","data":{...}}`.
/// The daemon reads `data.topic` (must be a declared topic) and relays the whole
/// `data` as the payload. Mark-Forge uses `type:"items"` for every telemetry
/// drop and nests `{topic, payload}` under `data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboundLine {
    /// The capability token from `JARVIS_APP_TOKEN`, stamped on every line.
    pub token: String,
    /// Always `"items"` for a telemetry drop (`"log"` is used only for logs).
    #[serde(rename = "type")]
    pub kind: String,
    pub data: OutboundData,
}

/// The `data` field of an [`OutboundLine`]: the topic the daemon routes on plus
/// the typed payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutboundData {
    pub topic: String,
    // FLATTENED into `data` alongside `topic` so the wire shape is
    // {topic, <payload fields>} — matching Vision's convention and the HUD parsers,
    // which read fields FLAT. A nested {topic, payload:{…}} leaves every HUD field
    // one level too deep and renders blank panels (the bug this closes).
    #[serde(flatten)]
    pub payload: Telemetry,
}

impl OutboundLine {
    /// Build a telemetry line stamped with the capability token, routed to the
    /// payload's declared topic. The single constructor the `ipc` agent uses to
    /// emit any of the three telemetry kinds.
    pub fn telemetry(token: impl Into<String>, payload: Telemetry) -> Self {
        let topic = payload.topic().to_string();
        OutboundLine {
            token: token.into(),
            kind: "items".to_string(),
            data: OutboundData { topic, payload },
        }
    }
}

// ===========================================================================
// Telemetry snapshots from the live World.
// ===========================================================================

/// Snapshot the world's body transforms as a [`BodiesFrame`] for `physics.bodies`
/// (SPEC §8). `frame`/`sim_time` are the cumulative counters [`AppState`] tracks.
pub fn bodies_frame(world: &World, frame: u64, sim_time: f64) -> BodiesFrame {
    let bodies = world
        .bodies
        .iter()
        .enumerate()
        .map(|(i, b)| BodyTransform {
            id: BodyId(i as u32),
            shape: b.shape.into(),
            pos: b.pos,
            quat: b.orientation,
            sleeping: b.sleeping,
        })
        .collect();
    BodiesFrame { frame, sim_time, bodies }
}

/// Snapshot the world topology as a [`SceneTopology`] for `physics.scene` (SPEC §8).
pub fn scene_topology(world: &World) -> SceneTopology {
    let bodies = world
        .bodies
        .iter()
        .enumerate()
        .map(|(i, b)| SceneBody {
            id: BodyId(i as u32),
            shape: b.shape.into(),
            r#static: b.is_static(),
        })
        .collect();
    SceneTopology {
        gravity: world.gravity,
        dt: world.dt,
        substeps: world.substeps,
        bodies,
    }
}

// ===========================================================================
// App state + the PURE op dispatcher.
// ===========================================================================

/// The in-memory app state the op dispatcher mutates: the live [`World`] plus the
/// cumulative frame counter + simulated time (SPEC §8 telemetry counters). The
/// dispatcher is PURE over this state (no socket), so the unit tests drive the
/// whole op surface headlessly.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The simulation world.
    pub world: World,
    /// Cumulative fixed frames advanced since launch / last reset.
    pub frame: u64,
    /// Cumulative simulated seconds (`frame * dt`, tracked incrementally).
    pub sim_time: f64,
}

impl Default for AppState {
    fn default() -> Self {
        AppState { world: World::new(), frame: 0, sim_time: 0.0 }
    }
}

impl AppState {
    pub fn new() -> Self {
        AppState::default()
    }
}

/// Dispatch one [`Op`] against the [`AppState`], returning the [`Telemetry`]
/// payloads to emit (SPEC §7/§8). PURE: it mutates `state` and returns the
/// payloads with NO socket involved, so the tests exercise the whole op surface
/// headlessly.
///
/// FOUNDATION CONTRACT: the op→state→telemetry MAPPING below is the frozen wiring
/// the `ipc` agent keeps; it drives the real [`World`] entry points
/// ([`World::step`]/`spawn`/`reset`/`set_gravity`). The integrator pipeline is
/// fully wired (SPEC §3), so a `world.step` advances real motion and reports the
/// resulting counts — the wiring is correct, testable, and exercised end-to-end
/// by the conservation-law tests.
pub fn dispatch(state: &mut AppState, op: &Op) -> Vec<Telemetry> {
    match op {
        Op::WorldReset => {
            state.world.reset();
            state.frame = 0;
            state.sim_time = 0.0;
            vec![Telemetry::Scene(scene_topology(&state.world))]
        }
        Op::BodySpawn(spec) => {
            // DEFENSIVE GATES — refuse a spawn that would poison the world,
            // WITHOUT changing dispatch's frozen Vec<Telemetry> signature: an
            // invalid/over-cap spawn is a no-op that re-publishes the unchanged
            // scene (the body is never added), so the determinism invariant
            // (no NaN/inf reaching the integrator, SPEC §1) and the body-count
            // bound both hold. `spawn_refusal` returns the human-readable
            // reason a refusal happened so the socket loop can log it.
            if spawn_refusal(state, spec).is_none() {
                let material = spec.material.unwrap_or_default();
                let mass = spec.mass.unwrap_or(0.0);
                let id = state.world.spawn_shape(spec.shape, spec.pos, mass, material);
                // Apply initial velocities to the freshly-spawned body.
                if let Some(b) = state.world.bodies.get_mut(id.index()) {
                    if let Some(v) = spec.lin_vel {
                        b.lin_vel = v;
                    }
                    if let Some(w) = spec.ang_vel {
                        b.ang_vel = w;
                    }
                }
            }
            vec![Telemetry::Scene(scene_topology(&state.world))]
        }
        Op::WorldStep { n } => {
            // `World::step` CLAMPS the per-call frame count to
            // `world::MAX_STEPS_PER_CALL`; advance the cumulative counters by the
            // count it ACTUALLY ran (`stats.frames`), not the raw `n`, so the
            // HUD's frame / sim_time counters never overstate an over-cap op.
            let stats = state.world.step(*n);
            state.frame += stats.frames as u64;
            state.sim_time += stats.frames as f64 * state.world.dt;
            vec![
                Telemetry::Step(stats.into()),
                Telemetry::Bodies(bodies_frame(&state.world, state.frame, state.sim_time)),
            ]
        }
        Op::SetGravity { x, y, z } => {
            // DEFENSIVE GATE (mirror of spawn_refusal): a non-finite or
            // absurd-magnitude gravity is a no-op that re-publishes the unchanged
            // scene, so a poisoned value never reaches the integrator (SPEC §1).
            if gravity_refusal(*x, *y, *z).is_none() {
                state.world.set_gravity(Vec3::new(*x, *y, *z));
            }
            vec![Telemetry::Scene(scene_topology(&state.world))]
        }
        Op::SetParams(patch) => {
            // DEFENSIVE GATE: a patch with a non-finite / out-of-range dt or a
            // non-finite solver knob is refused WHOLE (no partial apply), so a
            // finite-but-absurd dt=1e308 can never drive pos to infinity.
            if params_refusal(patch).is_none() {
                patch.apply(&mut state.world);
            }
            vec![Telemetry::Scene(scene_topology(&state.world))]
        }
        Op::StateGet => vec![
            Telemetry::Scene(scene_topology(&state.world)),
            Telemetry::Bodies(bodies_frame(&state.world, state.frame, state.sim_time)),
        ],
    }
}

/// Decide whether a `body.spawn` must be REFUSED, returning `Some(reason)` when
/// it must (and `None` when it may proceed). Pure over the world state so the
/// tests can drive every refusal path without a socket. Two gates:
///
///   1. BODY-COUNT CAP — at [`MAX_BODIES`] the world is full; a further spawn is
///      refused so a runaway op stream cannot grow the body list (and the
///      O(n²)-ish broadphase/solver work) without bound.
///   2. NON-FINITE TRANSFORM — a NaN/inf position, velocity, or shape size would
///      propagate through the integrator and break determinism (SPEC §1, "no NaN
///      propagation"). Such a spawn is refused before the body enters the world.
///
/// The reason string is for the operator-facing `log` line only; it never
/// reaches telemetry and never carries the token.
fn spawn_refusal(state: &AppState, spec: &SpawnSpec) -> Option<String> {
    if state.world.body_count() >= MAX_BODIES {
        return Some(format!(
            "body-count cap reached ({MAX_BODIES}); spawn refused"
        ));
    }
    if !spec.pos.is_finite() {
        return Some("non-finite spawn position; spawn refused".to_string());
    }
    if let Some(v) = spec.lin_vel {
        if !v.is_finite() {
            return Some("non-finite linear velocity; spawn refused".to_string());
        }
    }
    if let Some(w) = spec.ang_vel {
        if !w.is_finite() {
            return Some("non-finite angular velocity; spawn refused".to_string());
        }
    }
    if !shape_is_finite(&spec.shape) {
        return Some("non-finite shape parameter; spawn refused".to_string());
    }
    None
}

/// Is every numeric field of a [`Shape`] finite? Guards against a NaN/inf radius
/// / half-extent / plane normal entering the world (SPEC §1 determinism).
fn shape_is_finite(shape: &Shape) -> bool {
    match shape {
        Shape::Sphere { radius } => radius.is_finite(),
        Shape::Cuboid { half_extents } => half_extents.is_finite(),
        Shape::Plane { normal, offset } => normal.is_finite() && offset.is_finite(),
    }
}

/// Decide whether a `set.gravity` must be REFUSED, returning `Some(reason)` when
/// it must (and `None` when it may proceed). Mirrors [`spawn_refusal`]'s
/// discipline for the gravity entry point: a NaN/inf component, or a
/// finite-but-absurd magnitude beyond [`MAX_GRAVITY`], would accelerate every
/// body toward infinity within a step and break determinism (SPEC §1) just as a
/// poisoned spawn would. The reason string is operator-facing (`log`) only.
fn gravity_refusal(x: f64, y: f64, z: f64) -> Option<String> {
    if !(x.is_finite() && y.is_finite() && z.is_finite()) {
        return Some("non-finite gravity component; set.gravity refused".to_string());
    }
    if x.abs() > MAX_GRAVITY || y.abs() > MAX_GRAVITY || z.abs() > MAX_GRAVITY {
        return Some(format!(
            "gravity component exceeds ±{MAX_GRAVITY:e}; set.gravity refused"
        ));
    }
    None
}

/// Decide whether a `set.params` patch must be REFUSED, returning `Some(reason)`
/// when it must (and `None` when it may proceed). Only the fields that can poison
/// the integrator are gated; absent fields are unconstrained. Mirrors
/// [`spawn_refusal`]: a non-finite or out-of-range `dt` (outside `(0, MAX_DT]`)
/// would drive `pos += vel*dt` to infinity in one substep, and a non-finite
/// solver knob would seed NaN into the impulse solve — both break determinism
/// (SPEC §1). A patch is refused WHOLE (no partial apply) so a single bad field
/// never sneaks a poison value past while a sibling field is accepted. The reason
/// string is operator-facing (`log`) only.
fn params_refusal(patch: &ParamsPatch) -> Option<String> {
    if let Some(dt) = patch.dt {
        if !dt.is_finite() {
            return Some("non-finite dt; set.params refused".to_string());
        }
        if dt <= 0.0 {
            return Some("dt must be > 0; set.params refused".to_string());
        }
        if dt > MAX_DT {
            return Some(format!("dt exceeds MAX_DT ({MAX_DT}); set.params refused"));
        }
    }
    // The remaining solver knobs are real-valued tunings; a NaN/inf in any of
    // them would propagate into the velocity solve / position correction. Reject
    // any non-finite value (the magnitudes themselves are solver-bounded — only
    // finiteness can poison determinism).
    let finite_knobs: [(Option<f64>, &str); 5] = [
        (patch.restitution_threshold, "restitution_threshold"),
        (patch.baumgarte_beta, "baumgarte_beta"),
        (patch.penetration_slop, "penetration_slop"),
        (patch.linear_damping, "linear_damping"),
        (patch.angular_damping, "angular_damping"),
    ];
    for (value, name) in finite_knobs {
        if let Some(v) = value {
            if !v.is_finite() {
                return Some(format!("non-finite {name}; set.params refused"));
            }
        }
    }
    None
}

// ===========================================================================
// The socket loop.
// ===========================================================================

/// Connect to the per-app Unix socket, then read JSONL lines and dispatch them
/// until the daemon sends `stop`, closes the socket, or an unrecoverable I/O
/// error occurs. Every accepted op's telemetry is written back as token-stamped
/// [`OutboundLine`]s. A malformed line or an op error is logged to the host as a
/// `log` line and the loop CONTINUES — one bad op never tears the app down
/// (mirrors silicon-canvas / global-scan).
///
/// HARD SAFETY: binds NO listener (connects to the daemon's socket), opens NO
/// window, touches NO GPU, plays NO audio.
pub fn run(env: AppEnv) -> Result<()> {
    let stream = UnixStream::connect(&env.socket_path)?;
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);

    let mut state = AppState::new();

    log_line(&mut writer, &env.token, "mark-forge online");
    // Publish the initial (empty) scene so the HUD panel can mount.
    emit(&mut writer, &env.token, Telemetry::Scene(scene_topology(&state.world)));

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break, // socket closed / read error → exit cleanly
        };
        // One bad line never tears the app down (mirrors silicon-canvas /
        // global-scan): handle_line classifies + dispatches + writes back, and
        // returns LoopAction::Stop only on a clean `stop` verb.
        if let LoopAction::Stop = handle_line(&mut state, &env.token, &line, &mut writer) {
            break;
        }
    }
    Ok(())
}

/// What the socket loop does after one inbound line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopAction {
    /// Keep serving the next line.
    Continue,
    /// A clean `stop` verb arrived — exit the loop.
    Stop,
}

/// Handle ONE inbound JSONL line against the app state, writing every resulting
/// telemetry / log line (each token-stamped) to `writer`. Factored out of
/// [`run`] so the unit tests can drive the full classify → dispatch → write-back
/// path against an in-memory writer — no socket bind, no env (HARD SAFETY).
///
/// Behaviour (mirrors silicon-canvas / global-scan "never die on one bad line"):
///   - empty line                         → no-op, [`LoopAction::Continue`];
///   - `{"type":"stop"}`                  → a `stopping` log, [`LoopAction::Stop`];
///   - `{"type":"start"|"refresh"}`       → re-publish scene + bodies;
///   - a valid op                         → dispatch + emit its telemetry;
///   - a spawn the world REFUSES (cap /   → still emits the unchanged scene AND a
///     non-finite, see [`spawn_refusal`])   `log` line explaining the refusal;
///   - an UNKNOWN / malformed line        → a `dropped line: …` log, never a panic,
///                                           always [`LoopAction::Continue`].
///
/// EVERY line written here is token-stamped (telemetry via [`emit`], logs via
/// [`log_line`]); the token is the one constructor argument both take, so a line
/// can never leave this app unstamped.
fn handle_line<W: Write>(
    state: &mut AppState,
    token: &str,
    raw: &str,
    writer: &mut W,
) -> LoopAction {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return LoopAction::Continue;
    }
    match parse_command(trimmed) {
        Ok(Command::Control(HostControl::Stop)) => {
            log_line(writer, token, "mark-forge stopping");
            LoopAction::Stop
        }
        Ok(Command::Control(HostControl::Start)) | Ok(Command::Control(HostControl::Refresh)) => {
            // Re-publish the current scene + body transforms so the HUD
            // repaints. Nothing to fetch; the world is already resident.
            for p in dispatch(state, &Op::StateGet) {
                emit(writer, token, p);
            }
            LoopAction::Continue
        }
        Ok(Command::Op(op)) => {
            // Surface a refused op (body-count cap / non-finite transform, or a
            // non-finite/out-of-range set.gravity / set.params) as a host-visible
            // log BEFORE dispatch, so the operator sees WHY the world did not
            // change. dispatch itself then no-ops the refused op and re-publishes
            // the unchanged scene.
            match &op {
                Op::BodySpawn(spec) => {
                    if let Some(reason) = spawn_refusal(state, spec) {
                        log_line(writer, token, &format!("body.spawn refused: {reason}"));
                    }
                }
                Op::SetGravity { x, y, z } => {
                    if let Some(reason) = gravity_refusal(*x, *y, *z) {
                        log_line(writer, token, &format!("set.gravity refused: {reason}"));
                    }
                }
                Op::SetParams(patch) => {
                    if let Some(reason) = params_refusal(patch) {
                        log_line(writer, token, &format!("set.params refused: {reason}"));
                    }
                }
                // A `world.step` with an over-cap `n` is CLAMPED (not refused) in
                // World::step; the resulting Step telemetry below carries the
                // clamped `frames`, and the operator sees the clamp via the
                // contact-cap / clamp note emitted after dispatch.
                _ => {}
            }
            for p in dispatch(state, &op) {
                // SIGNAL the bounded-work guards on a step: a clamped frame count
                // or a contact-budget truncation is logged so the operator never
                // sees a silently mis-simulated / shortened step.
                if let (Op::WorldStep { n }, Telemetry::Step(report)) = (&op, &p) {
                    if *n > report.frames {
                        log_line(
                            writer,
                            token,
                            &format!(
                                "world.step n={n} clamped to {} frames (per-op step cap)",
                                report.frames
                            ),
                        );
                    }
                    if report.pairs_cap_hit {
                        log_line(
                            writer,
                            token,
                            "world.step pair budget hit: scene too dense, candidate-pair enumeration short-circuited this step",
                        );
                    }
                    if report.contact_cap_hit {
                        log_line(
                            writer,
                            token,
                            "world.step contact budget hit: scene too dense, contacts truncated this step",
                        );
                    }
                }
                emit(writer, token, p);
            }
            LoopAction::Continue
        }
        Err(e) => {
            // Unknown / malformed line → log + drop, never panic, never exit.
            log_line(writer, token, &format!("dropped line: {e}"));
            LoopAction::Continue
        }
    }
}

/// Write one token-stamped telemetry line to the host. Best-effort: a write
/// failure means the host went away — the read loop observes EOF and exits. We
/// never panic on a write error.
fn emit<W: Write>(writer: &mut W, token: &str, payload: Telemetry) {
    let line = OutboundLine::telemetry(token, payload);
    if let Ok(mut s) = serde_json::to_string(&line) {
        s.push('\n');
        let _ = writer.write_all(s.as_bytes());
        let _ = writer.flush();
    }
}

/// Write one token-stamped `log` line to the host (NOT telemetry). Mirrors the
/// daemon's `{"token","type":"log","data":{"line":…}}` log shape. The token is
/// stamped but NEVER included in the log message itself.
fn log_line<W: Write>(writer: &mut W, token: &str, message: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::Quat;

    #[test]
    fn op_deserializes_with_dotted_names() {
        assert_eq!(serde_json::from_str::<Op>(r#"{"op":"world.reset"}"#).unwrap(), Op::WorldReset);
        assert_eq!(serde_json::from_str::<Op>(r#"{"op":"world.step","n":10}"#).unwrap(), Op::WorldStep { n: 10 });
        assert_eq!(serde_json::from_str::<Op>(r#"{"op":"state.get"}"#).unwrap(), Op::StateGet);
        assert_eq!(
            serde_json::from_str::<Op>(r#"{"op":"set.gravity","x":0.0,"y":-9.81,"z":0.0}"#).unwrap(),
            Op::SetGravity { x: 0.0, y: -9.81, z: 0.0 }
        );
    }

    #[test]
    fn body_spawn_deserializes_with_optional_fields() {
        let op: Op = serde_json::from_str(
            r#"{"op":"body.spawn","shape":{"kind":"sphere","radius":0.5},"pos":[0.0,5.0,0.0]}"#,
        )
        .unwrap();
        match op {
            Op::BodySpawn(spec) => {
                assert_eq!(spec.shape, Shape::Sphere { radius: 0.5 });
                assert_eq!(spec.pos, Vec3::new(0.0, 5.0, 0.0));
                assert!(spec.mass.is_none());
                assert!(spec.lin_vel.is_none());
            }
            other => panic!("wrong op: {other:?}"),
        }
    }

    #[test]
    fn set_params_patch_applies_present_fields_only() {
        let op: Op = serde_json::from_str(r#"{"op":"set.params","substeps":8,"dt":0.01}"#).unwrap();
        let mut state = AppState::new();
        let before_iters = state.world.params.velocity_iterations;
        if let Op::SetParams(p) = op {
            p.apply(&mut state.world);
        }
        assert_eq!(state.world.substeps, 8);
        assert_eq!(state.world.dt, 0.01);
        // Untouched fields keep their value.
        assert_eq!(state.world.params.velocity_iterations, before_iters);
    }

    #[test]
    fn parse_command_classifies_op_and_control() {
        assert_eq!(
            parse_command(r#"{"type":"stop"}"#).unwrap(),
            Command::Control(HostControl::Stop)
        );
        assert_eq!(
            parse_command(r#"{"op":"world.step","n":1}"#).unwrap(),
            Command::Op(Op::WorldStep { n: 1 })
        );
        assert!(parse_command("not json").is_err());
        assert!(parse_command(r#"{"neither":true}"#).is_err());
    }

    #[test]
    fn dispatch_spawn_then_step_emits_scene_and_bodies() {
        let mut state = AppState::new();
        // Static ground plane.
        let scene = dispatch(
            &mut state,
            &Op::BodySpawn(SpawnSpec {
                shape: Shape::Plane { normal: Vec3::Y, offset: 0.0 },
                pos: Vec3::ZERO,
                lin_vel: None,
                ang_vel: None,
                mass: None,
                material: None,
            }),
        );
        assert!(matches!(scene[0], Telemetry::Scene(_)));
        // A dynamic sphere above it.
        dispatch(
            &mut state,
            &Op::BodySpawn(SpawnSpec {
                shape: Shape::Sphere { radius: 0.5 },
                pos: Vec3::new(0.0, 5.0, 0.0),
                lin_vel: Some(Vec3::new(1.0, 0.0, 0.0)),
                ang_vel: None,
                mass: Some(1.0),
                material: None,
            }),
        );
        assert_eq!(state.world.body_count(), 2);
        // The dynamic body got its initial velocity.
        assert_eq!(state.world.bodies[1].lin_vel, Vec3::new(1.0, 0.0, 0.0));

        let out = dispatch(&mut state, &Op::WorldStep { n: 3 });
        assert!(matches!(out[0], Telemetry::Step(_)));
        match &out[1] {
            Telemetry::Bodies(f) => {
                assert_eq!(f.frame, 3);
                assert_eq!(f.bodies.len(), 2);
                assert_eq!(f.bodies[1].id, BodyId(1));
            }
            other => panic!("expected bodies frame, got {other:?}"),
        }
    }

    #[test]
    fn reset_clears_and_resets_counters() {
        let mut state = AppState::new();
        dispatch(&mut state, &Op::BodySpawn(SpawnSpec {
            shape: Shape::Sphere { radius: 1.0 },
            pos: Vec3::ZERO,
            lin_vel: None,
            ang_vel: None,
            mass: Some(1.0),
            material: None,
        }));
        dispatch(&mut state, &Op::WorldStep { n: 5 });
        assert_eq!(state.frame, 5);
        dispatch(&mut state, &Op::WorldReset);
        assert_eq!(state.frame, 0);
        assert_eq!(state.sim_time, 0.0);
        assert_eq!(state.world.body_count(), 0);
    }

    #[test]
    fn telemetry_topic_mapping() {
        let bodies = Telemetry::Bodies(BodiesFrame { frame: 0, sim_time: 0.0, bodies: vec![] });
        assert_eq!(bodies.topic(), TOPIC_BODIES);
        let step = Telemetry::Step(StepReport {
            frames: 0, substeps: 0, bodies: 0, contacts: 0, solver_iterations: 0, last_penetration: 0.0,
            contact_cap_hit: false, pairs_cap_hit: false,
        });
        assert_eq!(step.topic(), TOPIC_STEP);
        let scene = Telemetry::Scene(scene_topology(&World::new()));
        assert_eq!(scene.topic(), TOPIC_SCENE);
    }

    #[test]
    fn outbound_line_wire_shape() {
        let line = OutboundLine::telemetry(
            "tok",
            Telemetry::Step(StepReport {
                frames: 1, substeps: 4, bodies: 2, contacts: 0, solver_iterations: 8, last_penetration: 0.0,
                contact_cap_hit: false, pairs_cap_hit: false,
            }),
        );
        assert_eq!(line.token, "tok");
        assert_eq!(line.kind, "items");
        assert_eq!(line.data.topic, TOPIC_STEP);
        // Round-trips through JSON.
        let s = serde_json::to_string(&line).unwrap();
        assert!(s.contains(r#""type":"items""#));
        assert!(s.contains(r#""topic":"physics.step""#));
        // FLAT wire contract: payload fields sit DIRECTLY in `data` (like Vision +
        // the HUD parsers), NOT under a nested `payload` object.
        assert!(!s.contains(r#""payload""#), "telemetry must be flat in data: {s}");
        assert!(s.contains(r#""frames":1"#), "the step fields must be flattened into data: {s}");
    }

    #[test]
    fn body_transform_serializes_pos_and_quat_arrays() {
        let t = BodyTransform {
            id: BodyId(0),
            shape: ShapeTag::Sphere { radius: 1.0 },
            pos: Vec3::new(1.0, 2.0, 3.0),
            quat: Quat::IDENTITY,
            sleeping: false,
        };
        let s = serde_json::to_string(&t).unwrap();
        assert!(s.contains(r#""pos":[1.0,2.0,3.0]"#));
        assert!(s.contains(r#""quat":[0.0,0.0,0.0,1.0]"#));
    }

    // ----- helpers for the socket-loop (handle_line) tests -----------------

    /// A dynamic sphere spawn at the given position with a given linear velocity.
    fn sphere_spawn(pos: Vec3, lin_vel: Vec3) -> Op {
        Op::BodySpawn(SpawnSpec {
            shape: Shape::Sphere { radius: 0.5 },
            pos,
            lin_vel: Some(lin_vel),
            ang_vel: None,
            mass: Some(1.0),
            material: None,
        })
    }

    /// Drive one inbound line through [`handle_line`] against the given state +
    /// token, returning `(action, written_lines)`. Each written line is one
    /// already-newline-framed JSONL string the app would have sent the host.
    fn feed(state: &mut AppState, token: &str, raw: &str) -> (LoopAction, Vec<String>) {
        let mut buf: Vec<u8> = Vec::new();
        let action = handle_line(state, token, raw, &mut buf);
        let text = String::from_utf8(buf).expect("written lines are utf8");
        let lines = text
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        (action, lines)
    }

    // ----- op parse + dispatch: spawn -> step -> state.get reflects motion --

    #[test]
    fn loop_spawn_step_state_get_reflects_motion() {
        // Spawn a dynamic sphere with a horizontal velocity, step the world via
        // the FULL inbound-line path (parse_command -> dispatch -> emit), then
        // state.get; the published bodies frame must reflect the advanced
        // frame/sim_time counters and the body's id — the wiring is verified
        // independent of the (separately-built) integrator producing motion.
        let mut state = AppState::new();
        let token = "tok-motion";

        // 1) spawn (a body.spawn line) — emits a scene topology.
        let spawn_line = serde_json::to_string(&sphere_spawn(
            Vec3::new(0.0, 5.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
        ))
        .unwrap();
        let (a, scene_out) = feed(&mut state, token, &spawn_line);
        assert_eq!(a, LoopAction::Continue);
        assert_eq!(state.world.body_count(), 1);
        // The freshly-spawned body carries its initial velocity (dispatch wired
        // lin_vel through).
        assert_eq!(state.world.bodies[0].lin_vel, Vec3::new(2.0, 0.0, 0.0));
        // Exactly one scene line was published.
        assert_eq!(scene_out.len(), 1);
        assert!(scene_out[0].contains(r#""topic":"physics.scene""#));

        // 2) step n=3 — emits a physics.step then a physics.bodies line.
        let (_, step_out) = feed(&mut state, token, r#"{"op":"world.step","n":3}"#);
        assert_eq!(state.frame, 3, "the cumulative frame counter advanced by n");
        assert!((state.sim_time - 3.0 * state.world.dt).abs() < 1e-12);
        assert_eq!(step_out.len(), 2);
        assert!(step_out[0].contains(r#""topic":"physics.step""#));
        assert!(step_out[1].contains(r#""topic":"physics.bodies""#));
        // The step report carries n as `frames` and the world's substeps.
        assert!(step_out[0].contains(r#""frames":3"#));

        // 3) state.get — re-publishes scene + bodies reflecting the advanced
        // counters, so the panel can re-sync at any time.
        let (_, get_out) = feed(&mut state, token, r#"{"op":"state.get"}"#);
        assert_eq!(get_out.len(), 2);
        assert!(get_out[0].contains(r#""topic":"physics.scene""#));
        assert!(get_out[1].contains(r#""topic":"physics.bodies""#));
        // The bodies frame's `frame` mirrors the cumulative counter (=3).
        let bodies: serde_json::Value = serde_json::from_str(&get_out[1]).unwrap();
        // FLAT wire: payload fields sit directly in `data` (not under `data.payload`),
        // matching the HUD parsers.
        assert!(bodies["data"]["payload"].is_null(), "telemetry must be flat (no payload wrapper)");
        assert_eq!(bodies["data"]["frame"], 3);
        assert_eq!(bodies["data"]["bodies"][0]["id"], 0);
    }

    // ----- token-on-every-line ---------------------------------------------

    #[test]
    fn every_outbound_line_is_token_stamped() {
        // Across a representative op stream — spawn (scene), step (step+bodies),
        // state.get (scene+bodies), an over-cap/invalid refusal (log), and a
        // malformed line (log) — EVERY single line the app writes back carries
        // the exact JARVIS_APP_TOKEN as its top-level "token" field. A line can
        // never leave this app unstamped (the daemon drops unstamped lines).
        let mut state = AppState::new();
        let token = "cap-tok-7f3a";

        let mut all: Vec<String> = Vec::new();
        let spawn = serde_json::to_string(&sphere_spawn(Vec3::ZERO, Vec3::ZERO)).unwrap();
        for line in [
            spawn.as_str(),
            r#"{"op":"world.step","n":2}"#,
            r#"{"op":"state.get"}"#,
            r#"{"op":"set.gravity","x":0.0,"y":-1.0,"z":0.0}"#,
            r#"{"type":"refresh"}"#,
            // A malformed line (neither op nor type) -> a `dropped line` log
            // (still token-stamped).
            r#"{"totally":"bogus"}"#,
            // An unknown op name -> a `dropped line` log (still token-stamped).
            r#"{"op":"world.explode"}"#,
        ] {
            let (_, out) = feed(&mut state, token, line);
            all.extend(out);
        }
        assert!(!all.is_empty(), "the stream produced output to check");
        for line in &all {
            let v: serde_json::Value =
                serde_json::from_str(line).expect("every line is well-formed JSON");
            assert_eq!(
                v["token"].as_str(),
                Some(token),
                "line missing/garbled token: {line}"
            );
        }
    }

    // ----- unknown / malformed op -> clean error, never a panic ------------

    #[test]
    fn unknown_op_is_logged_and_dropped_not_panicked() {
        let mut state = AppState::new();
        let token = "tok-unknown";

        // An unknown op name.
        let (a, out) = feed(&mut state, token, r#"{"op":"world.explode"}"#);
        assert_eq!(a, LoopAction::Continue, "the loop keeps serving");
        assert_eq!(out.len(), 1, "exactly one log line back");
        let v: serde_json::Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(v["type"], "log");
        assert_eq!(v["token"], token);
        assert!(
            v["data"]["line"].as_str().unwrap().contains("dropped line"),
            "unknown op is reported as a dropped line: {}",
            out[0]
        );

        // Non-JSON garbage is likewise dropped cleanly.
        let (a2, out2) = feed(&mut state, token, "this is not json at all");
        assert_eq!(a2, LoopAction::Continue);
        assert_eq!(out2.len(), 1);
        assert!(serde_json::from_str::<serde_json::Value>(&out2[0]).unwrap()["data"]["line"]
            .as_str()
            .unwrap()
            .contains("dropped line"));

        // A line that is neither op nor type is dropped.
        let (a3, _out3) = feed(&mut state, token, r#"{"neither":true}"#);
        assert_eq!(a3, LoopAction::Continue);

        // The world is untouched by any of the bad lines.
        assert_eq!(state.world.body_count(), 0);
    }

    #[test]
    fn stop_verb_ends_the_loop_after_logging() {
        let mut state = AppState::new();
        let (a, out) = feed(&mut state, "tok", r#"{"type":"stop"}"#);
        assert_eq!(a, LoopAction::Stop);
        assert_eq!(out.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(v["type"], "log");
        assert!(v["data"]["line"].as_str().unwrap().contains("stopping"));
    }

    // ----- body-count cap enforced -----------------------------------------

    #[test]
    fn body_count_cap_refuses_over_cap_spawn() {
        // Fill the world to exactly MAX_BODIES via the spawn op, then assert the
        // next spawn is REFUSED: the body is not added, the body count holds at
        // the cap, and the loop emits BOTH the refusal log and the (unchanged)
        // scene — never a panic, never unbounded growth.
        let mut state = AppState::new();
        let token = "tok-cap";

        // Seed directly to the cap (going through the op MAX_BODIES times is
        // correct but slow; the cap math lives in spawn_refusal, exercised by
        // the refused op below).
        for _ in 0..MAX_BODIES {
            state
                .world
                .spawn_shape(Shape::Sphere { radius: 0.1 }, Vec3::ZERO, 1.0, Material::default());
        }
        assert_eq!(state.world.body_count(), MAX_BODIES);

        // spawn_refusal flags the cap directly.
        let spec = match sphere_spawn(Vec3::ZERO, Vec3::ZERO) {
            Op::BodySpawn(s) => s,
            _ => unreachable!(),
        };
        assert!(
            spawn_refusal(&state, &spec)
                .unwrap()
                .contains("body-count cap"),
            "at the cap, a spawn is refused with a cap reason"
        );

        // Through the full inbound-line path: the body count does NOT grow, and
        // both a refusal log and the unchanged scene are written.
        let spawn_line = serde_json::to_string(&sphere_spawn(Vec3::ZERO, Vec3::ZERO)).unwrap();
        let (a, out) = feed(&mut state, token, &spawn_line);
        assert_eq!(a, LoopAction::Continue);
        assert_eq!(
            state.world.body_count(),
            MAX_BODIES,
            "an over-cap spawn must NOT add a body"
        );
        // One refusal log + one (unchanged) scene line.
        assert_eq!(out.len(), 2);
        let log: serde_json::Value = serde_json::from_str(&out[0]).unwrap();
        assert_eq!(log["type"], "log");
        assert!(log["data"]["line"].as_str().unwrap().contains("refused"));
        assert!(out[1].contains(r#""topic":"physics.scene""#));
    }

    #[test]
    fn non_finite_spawn_is_refused() {
        // A NaN/inf position, velocity, or shape size must never reach the
        // integrator (SPEC §1 determinism). spawn_refusal flags each, and the
        // dispatch path leaves the world empty.
        let mut state = AppState::new();

        let bad_pos = SpawnSpec {
            shape: Shape::Sphere { radius: 1.0 },
            pos: Vec3::new(0.0, f64::NAN, 0.0),
            lin_vel: None,
            ang_vel: None,
            mass: Some(1.0),
            material: None,
        };
        assert!(spawn_refusal(&state, &bad_pos).unwrap().contains("position"));

        let bad_vel = SpawnSpec {
            shape: Shape::Sphere { radius: 1.0 },
            pos: Vec3::ZERO,
            lin_vel: Some(Vec3::new(f64::INFINITY, 0.0, 0.0)),
            ang_vel: None,
            mass: Some(1.0),
            material: None,
        };
        assert!(spawn_refusal(&state, &bad_vel)
            .unwrap()
            .contains("linear velocity"));

        let bad_shape = SpawnSpec {
            shape: Shape::Cuboid { half_extents: Vec3::new(1.0, f64::NAN, 1.0) },
            pos: Vec3::ZERO,
            lin_vel: None,
            ang_vel: None,
            mass: Some(1.0),
            material: None,
        };
        assert!(spawn_refusal(&state, &bad_shape)
            .unwrap()
            .contains("shape"));

        // Dispatching a non-finite spawn is a no-op (world stays empty).
        dispatch(&mut state, &Op::BodySpawn(bad_pos));
        assert_eq!(state.world.body_count(), 0, "a poisoned spawn never enters the world");
    }

    // ----- a valid spawn still proceeds (the gates don't over-reject) -------

    #[test]
    fn valid_spawn_proceeds_through_dispatch() {
        let mut state = AppState::new();
        let spec = match sphere_spawn(Vec3::new(0.0, 3.0, 0.0), Vec3::new(1.0, 2.0, 3.0)) {
            Op::BodySpawn(s) => s,
            _ => unreachable!(),
        };
        assert!(spawn_refusal(&state, &spec).is_none(), "a finite, under-cap spawn is allowed");
        dispatch(&mut state, &Op::BodySpawn(spec));
        assert_eq!(state.world.body_count(), 1);
        assert_eq!(state.world.bodies[0].ang_vel, Vec3::ZERO);
        assert_eq!(state.world.bodies[0].lin_vel, Vec3::new(1.0, 2.0, 3.0));
    }

    // ----- world.step clamp: over-cap n is bounded through the IPC path -----

    #[test]
    fn over_cap_world_step_is_clamped_and_logged() {
        // A hostile `world.step` with an enormous `n` must NOT run unbounded: the
        // engine clamps it to MAX_STEPS_PER_CALL, dispatch advances the cumulative
        // counters by the CLAMPED frame count (not the raw n), and the socket loop
        // emits a host-visible clamp log so the operator sees the bound bite.
        use crate::world::MAX_STEPS_PER_CALL;
        let mut state = AppState::new();
        let token = "tok-clamp";

        let n: u64 = u32::MAX as u64;
        let line = format!(r#"{{"op":"world.step","n":{}}}"#, n as u32);
        let (a, out) = feed(&mut state, token, &line);
        assert_eq!(a, LoopAction::Continue);

        // The cumulative frame counter advanced by the CLAMPED count, not ~4.29e9.
        assert_eq!(
            state.frame, MAX_STEPS_PER_CALL as u64,
            "an over-cap world.step advances only the clamped frame count"
        );

        // A clamp log was emitted, plus the step + bodies telemetry.
        let clamp_log = out
            .iter()
            .find(|l| l.contains(r#""type":"log""#) && l.contains("clamped"))
            .expect("a clamp note is logged when the per-op step cap bites");
        let v: serde_json::Value = serde_json::from_str(clamp_log).unwrap();
        assert_eq!(v["token"], token);
        // The step telemetry reports the clamped `frames`.
        let step_line = out
            .iter()
            .find(|l| l.contains(r#""topic":"physics.step""#))
            .expect("a physics.step line is published");
        let step: serde_json::Value = serde_json::from_str(step_line).unwrap();
        assert_eq!(step["data"]["frames"], MAX_STEPS_PER_CALL); // flat wire (no payload wrapper)
    }

    #[test]
    fn under_cap_world_step_is_not_logged_as_clamped() {
        // A normal, under-cap world.step advances exactly n and emits no clamp log
        // (the guard must not fire on ordinary stepping).
        let mut state = AppState::new();
        let token = "tok-normal";
        let (_, out) = feed(&mut state, token, r#"{"op":"world.step","n":3}"#);
        assert_eq!(state.frame, 3);
        assert!(
            !out.iter().any(|l| l.contains("clamped")),
            "an under-cap step must not log a clamp"
        );
    }

    // ----- set.params finite gate: absurd/non-finite dt rejected -----------

    #[test]
    fn set_params_absurd_or_nonpositive_dt_is_refused() {
        // A finite-but-absurd dt (1e308) drives `pos += vel*dt` to infinity in one
        // substep — the same poison body.spawn's finite gate keeps out. set.params
        // must refuse it (and a non-positive / non-finite dt) WHOLE: the world's dt
        // is left unchanged, and the socket loop logs the refusal.
        let mut state = AppState::new();
        let token = "tok-params";
        let dt_before = state.world.dt;

        for bad in [
            r#"{"op":"set.params","dt":1e308}"#,
            r#"{"op":"set.params","dt":0.0}"#,
            r#"{"op":"set.params","dt":-0.01}"#,
        ] {
            let (a, out) = feed(&mut state, token, bad);
            assert_eq!(a, LoopAction::Continue);
            assert_eq!(
                state.world.dt, dt_before,
                "a refused set.params must not change dt: {bad}"
            );
            assert!(
                out.iter()
                    .any(|l| l.contains(r#""type":"log""#) && l.contains("set.params refused")),
                "a refused set.params is logged: {bad}"
            );
        }

        // params_refusal flags a non-finite solver knob too (NaN would seed the
        // impulse solve with NaN).
        let nan_knob = ParamsPatch { baumgarte_beta: Some(f64::NAN), ..Default::default() };
        assert!(params_refusal(&nan_knob).unwrap().contains("baumgarte_beta"));
    }

    #[test]
    fn set_params_normal_dt_is_accepted() {
        // A sane dt/substeps patch applies normally (the gate must not over-reject).
        let mut state = AppState::new();
        let token = "tok-params-ok";
        let (a, _out) = feed(&mut state, token, r#"{"op":"set.params","dt":0.01,"substeps":8}"#);
        assert_eq!(a, LoopAction::Continue);
        assert_eq!(state.world.dt, 0.01, "a sane dt is accepted");
        assert_eq!(state.world.substeps, 8);
        // dt at exactly the cap boundary is accepted (inclusive upper bound).
        let boundary = ParamsPatch { dt: Some(MAX_DT), ..Default::default() };
        assert!(params_refusal(&boundary).is_none(), "dt == MAX_DT is accepted");
    }

    // ----- set.gravity finite gate: non-finite/absurd gravity rejected -----

    #[test]
    fn set_gravity_nonfinite_or_absurd_is_refused() {
        // A non-finite or absurd-magnitude gravity accelerates every body to
        // infinity within a step. set.gravity must refuse it; the world's gravity
        // is unchanged and the refusal is logged.
        let mut state = AppState::new();
        let token = "tok-grav";
        let g_before = state.world.gravity;

        for bad in [
            r#"{"op":"set.gravity","x":0.0,"y":1e308,"z":0.0}"#,
            r#"{"op":"set.gravity","x":0.0,"y":null,"z":0.0}"#, // null -> parse drop, world unchanged
        ] {
            let (a, _out) = feed(&mut state, token, bad);
            assert_eq!(a, LoopAction::Continue);
            assert_eq!(state.world.gravity, g_before, "gravity unchanged for: {bad}");
        }
        // The absurd-but-parseable one must specifically produce a refusal log.
        let (_, out) = feed(&mut state, token, r#"{"op":"set.gravity","x":0.0,"y":1e308,"z":0.0}"#);
        assert!(
            out.iter()
                .any(|l| l.contains(r#""type":"log""#) && l.contains("set.gravity refused")),
            "an absurd gravity is logged as refused"
        );

        // gravity_refusal flags a NaN component directly.
        assert!(gravity_refusal(f64::NAN, 0.0, 0.0).unwrap().contains("non-finite"));
        assert!(gravity_refusal(0.0, f64::INFINITY, 0.0).is_some());
    }

    #[test]
    fn set_gravity_normal_value_is_accepted() {
        // A sane gravity applies normally (the gate must not over-reject).
        let mut state = AppState::new();
        let token = "tok-grav-ok";
        let (a, _out) = feed(&mut state, token, r#"{"op":"set.gravity","x":0.0,"y":-3.0,"z":0.0}"#);
        assert_eq!(a, LoopAction::Continue);
        assert_eq!(state.world.gravity, Vec3::new(0.0, -3.0, 0.0), "a sane gravity is accepted");
        assert!(gravity_refusal(0.0, -9.81, 0.0).is_none());
    }
}
