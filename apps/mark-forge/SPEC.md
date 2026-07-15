# Mark-Forge — SPEC

A **deterministic 3D rigid-body physics sandbox**: a fixed-timestep impulse-based
engine in Rust (the verifiable jewel — built + tested entirely headlessly) emitting
compact body-transform telemetry over the per-app JSONL socket, which a small
React-Three-Fiber (R3F) panel in the HUD renders as a live sandbox. Phase-4
micro-app against `docs/SANDBOX.md`; panel surface per `docs/HUD.md`.

The engine is the contract. The HUD render is **device-gated** (§8): the R3F render
loop SUSPENDS headlessly, so on-screen 60 fps motion is verified only on the real
Tauri app on a real Apple Silicon Mac — never claimed-measured here.

## Sandbox contract (binding: `manifest.toml`)

- Runtime `binary` (prebuilt native Rust binary in the app dir — mirrors
  silicon-canvas; darwind execs it directly under `sandbox-exec`, `apps.rs`
  `Runtime::Binary`).
- `gpu = false`: the **engine is CPU-only** (f64 scalar math). The HUD renders the
  bodies; the app never touches a GPU device.
- `net_hosts = []` — fully offline. No file reads/writes are needed by the engine
  (scenes are constructed via IPC ops, not loaded from disk); `fs_write` is a single
  scratch dir reserved for an optional deterministic replay-trace dump.
- IPC: JSONL over `state/ipc/apps/mark-forge.sock`, capability token per outbound
  line (`DARWIN_APP_TOKEN`); socket + name + token arrive via the env, NEVER argv
  (`daemon/src/apps.rs`).
- UI: `surface = "panel"` — the HUD renders the bodies in an R3F sandbox panel from
  the `physics.bodies` transforms; input (spawn/step/reset) arrives as JSONL ops
  routed by the daemon over the app socket. Mark-Forge opens NO window and reads NO
  input devices.
- Topics: `physics.bodies`, `physics.step`, `physics.scene`.

## 1. Determinism guarantees (the headline property)

Every behavior below is a **pure function of the prior `World` state + the op stream**
— bit-for-bit reproducible across runs and machines, which is exactly what makes the
whole engine `cargo test`-verifiable:

- **No RNG, no wall-clock, no threads.** The integrator never reads `Instant::now`,
  never samples an RNG, never parallelizes. Spawn positions/velocities come only from
  op parameters.
- **`f64` throughout.** All math is double precision; no `f32` anywhere in the engine
  core. No transcendental shortcuts whose result varies by libm — only `+ - * /`,
  `sqrt`, and a fixed-form quaternion normalize.
- **Fixed iteration order.** Bodies are a `Vec` (stable insertion order); broadphase
  emits candidate pairs in a deterministic order; the solver iterates contacts in a
  fixed order for a fixed iteration count. No `HashMap` iteration feeds any
  numeric-affecting loop.
- **Fixed timestep + substeps.** `world.step{n}` advances exactly `n` frames of
  `dt = 1/hz`, each split into `substeps` equal sub-frames. The simulated time
  advanced is `n * dt` regardless of how long the call took.
- A golden-trace test seeds a known scene (stacked boxes on a plane), steps a fixed
  count, and asserts the final transforms equal a checked-in snapshot to the last
  representable `f64` digit. Re-running must reproduce it exactly.

## 2. Body model

Struct-of-arrays-friendly `Vec<Body>` (stable indices = `BodyId`). Each `Body` carries:

- `pos: Vec3`, `orientation: Quat` (unit quaternion; the engine renormalizes each step).
- `lin_vel: Vec3`, `ang_vel: Vec3` (world-space angular velocity).
- `inv_mass: f64` — `0.0` ⇒ a static/infinite-mass body (planes, anchors).
- `inv_inertia: Mat3` — inverse inertia tensor in **body space**; the solver rotates it
  to world space each step via `R · I⁻¹ · Rᵀ`. `Mat3::ZERO` ⇒ no angular response.
- `shape: Shape` — `Sphere{radius}`, `Cuboid{half_extents: Vec3}`, or
  `Plane{normal: Vec3, offset: f64}` (a static half-space; `inv_mass`/`inv_inertia` are
  forced to zero on spawn).
- `material: Material` — `restitution` (0..1) and `friction` (Coulomb μ ≥ 0), combined
  pairwise at contact time.

Inertia helpers (in `math`): `Mat3::solid_sphere_inertia(mass, r)` and
`Mat3::solid_box_inertia(mass, half_extents)` return the body-space tensor; the spawn
path inverts it (a zero/`inf` mass ⇒ zero inverse).

## 3. Integrator (fixed timestep + substeps)

Semi-implicit (symplectic) Euler, sub-stepped:

```
for each of n frames:
  h = dt / substeps
  for each of substeps:
    for each dynamic body:
      lin_vel += gravity * h                    // external accel
      apply velocity damping (per-substep factor)
    broadphase  -> candidate AABB pairs
    narrowphase -> contact manifolds
    solver      -> sequential-impulse velocity solve (restitution + friction)
    for each dynamic body:
      pos        += lin_vel * h
      orientation = normalize(orientation + 0.5 * quat(0, ang_vel) * orientation * h)
    position-correction pass (Baumgarte / split-impulse, §6)
```

Substeps improve stability of stacks and fast contacts without shrinking the reported
frame rate. `World` stores `gravity: Vec3`, `dt: f64` (= `1/hz`), `substeps: u32`,
and the solver `SolverParams` (§6).

## 4. Broadphase (AABB)

Each body's world AABB is computed from its shape + transform (a `Cuboid`'s AABB is the
oriented box's enclosing axis-aligned box; a `Plane` is unbounded and is paired against
every dynamic body). Default broadphase is a **uniform spatial-hash grid** over the
dynamic AABBs producing candidate pairs; a brute-force `O(n²)` AABB-overlap path is the
reference fallback and the determinism oracle (the grid must emit the same overlapping
set). Pairs are emitted in a deterministic, sorted `(min_id, max_id)` order.

## 5. Narrowphase

Produces a `Manifold` (0..N `Contact` points, each with world `point`, unit `normal`
pointing from A→B, and `penetration` depth ≥ 0) for each candidate pair:

- **sphere–sphere** — center distance vs radius sum.
- **sphere–plane** — signed distance to the half-space.
- **box–plane** — the 8 box corners tested against the half-space; one contact per
  penetrating corner (up to 4 kept, deepest first, for a stable face rest).
- **box–box (SAT)** — Separating-Axis Test over the 6 face normals + 9 edge-cross axes;
  least-penetration axis ⇒ reference/incident face clip ⇒ contact manifold.

Contact ordering within a manifold is deterministic (by the generating feature index).

## 6. Solver (sequential impulse + friction + position correction)

A **sequential-impulse constraint solver** over the contact set:

- **Normal impulses** with restitution: target normal velocity is
  `-e * v_rel_n` (bounce), `e` the pairwise-combined restitution, applied above a small
  restitution velocity threshold (resting contacts don't jitter-bounce).
- **Coulomb friction**: per-contact tangent impulse clamped to `±μ * normalImpulse`
  (`μ` the pairwise-combined friction), two tangent directions per contact.
- **Iteration**: `velocity_iterations` passes over all contacts in fixed order,
  accumulating impulses (warm-startable; the foundation keeps accumulated impulses on
  the manifold so a later agent can add warm-starting without a contract change).
- **Position correction**: split-impulse / Baumgarte — penetration beyond a `slop` is
  resolved by a positional bias (`beta` factor) so stacks settle without injecting
  energy into the velocity solve. `SolverParams { velocity_iterations, restitution_threshold, baumgarte_beta, penetration_slop, linear_damping, angular_damping }`.

All combinations (restitution, friction) use fixed pairwise rules: restitution =
`max(a,b)`, friction = `sqrt(a*b)`. Deterministic, no branches on float identity.

## 7. IPC ops (JSONL, token-bearing)

Externally tagged on `op` (mirrors silicon-canvas `ops.rs`):

| op | request | effect |
|---|---|---|
| `world.reset` | `{}` | Clear all bodies; keep gravity/dt/substeps/params |
| `body.spawn` | `{shape, pos, lin_vel?, ang_vel?, mass?, material?}` | Add one body; returns its `BodyId` in the next `physics.scene` |
| `world.step` | `{n}` | Advance exactly `n` fixed frames (each `substeps` sub-frames) |
| `set.gravity` | `{x, y, z}` | Replace the gravity vector |
| `set.params` | `{ …SolverParams subset… }` | Update timestep/substeps/solver params |
| `state.get` | `{}` | Re-publish `physics.scene` + `physics.bodies` now |

The daemon also sends the generic `{"type":"start"|"refresh"|"stop"}` control verbs
(`apps.rs::send_command`); `start`/`refresh` re-publish current state, `stop` exits
cleanly.

## 8. Telemetry (`physics.*`)

App→host lines are the daemon's `{"token","type":"items","data":{topic,payload}}`
shape (`apps.rs::classify_inbound_line`); `topic` must be a declared topic.

- **`physics.bodies`** — the per-frame render feed the R3F panel consumes:
  `{ frame, sim_time, bodies: [{ id, shape, pos:[x,y,z], quat:[x,y,z,w], sleeping }] }`.
  Emitted after a `world.step` (the last frame's transforms; a later agent may stream
  intermediate frames).
- **`physics.step`** — solver/step stats for the HUD readout:
  `{ frames, substeps, bodies, contacts, solver_iterations, last_penetration,
     contact_cap_hit, pairs_cap_hit }`. The two trailing booleans (both
  `#[serde(default)]`, additive — absent on older producers ⇒ `false`) each signal
  that a per-substep budget bit on at least one substep during the step — a
  deterministic bound that is SIGNALLED, never a silent mis-simulation:
  - `contact_cap_hit` — the per-substep contact budget (`MAX_CONTACTS_PER_SUBSTEP`)
    bit: a degenerate/over-dense scene's contacts were truncated to keep solver work bounded.
  - `pairs_cap_hit` — the per-substep candidate-pair budget (`MAX_PAIRS_PER_SUBSTEP`)
    bit: broadphase short-circuited its pair enumeration on a collapsed scene (the upstream O(n²) bound).
- **`physics.scene`** — scene topology on change (spawn/reset):
  `{ gravity:[x,y,z], dt, substeps, bodies: [{ id, shape, static }] }`.

## 9. Milestones

1. **Foundation** (this doc): crate skeleton, SPEC, manifest, ALL shared types
   (math, body, world, contact, ops, telemetry, AppEnv) frozen; module stubs build;
   `cargo build` + `cargo test` green.
2. `math` + `body` + `world` integrator (semi-implicit Euler + substeps); golden free-fall trace.
3. broadphase (grid + brute-force oracle) + narrowphase (sphere/plane pairs, then box SAT).
4. solver (sequential impulse + restitution + friction + position correction); stacked-box golden trace.
5. `ipc` op/telemetry loop over the socket; voice-driven spawn/step through the daemon.
6. R3F sandbox panel in the HUD (device-gated render of `physics.bodies`).

## Non-goals

- Soft bodies, cloth, fluids, ragdolls, joints/constraints beyond contacts (no hinges,
  springs, motors in this phase).
- Continuous collision detection (CCD) / speculative contacts — discrete substepped
  contacts only; a fast small body may tunnel a thin static at extreme speed.
- GJK/EPA convex–convex for arbitrary meshes — only sphere/box/plane primitives.
- GPU compute, multithreading, SIMD — the engine is single-threaded scalar f64 by
  design (determinism > raw throughput at this scale).
- Loading scenes from files, saving/serializing worlds to disk (scenes are built via IPC ops).

## Device-gated (verified only on the real Tauri app on a real Apple Silicon Mac)

The HUD's R3F sandbox panel rendering the bodies at 60 fps. The HUD SUSPENDS the R3F
render loop headlessly (no WebGL context, no requestAnimationFrame in test), so
on-screen motion, the live transform interpolation, and the 60 fps target are verified
**only on-device**, never claimed-measured in this repo. Everything in §1–§8 (the entire
engine + the telemetry serialization) is verified headlessly via `cargo build` +
`cargo test`.
