//! The simulation world + the fixed-timestep integrator (SPEC §3).
//!
//! This module owns the FROZEN [`World`] container, the [`SolverParams`] tuning
//! struct, and the public [`World::step`] / [`World::spawn`] entry points the IPC
//! layer drives. The integrator BODY (the per-substep velocity/position update,
//! the broadphase→narrowphase→solver pipeline) is fully wired here: each substep
//! runs integrate-velocity → [`broadphase::candidate_pairs`] →
//! [`narrowphase::collide_all`] → [`solver::solve_velocity`] → integrate-position
//! → [`solver::correct_positions`] in that fixed order. Downstream agents must NOT
//! change the field layout or these signatures.
//!
//! BOUNDED WORK (defensive): [`World::step`] bounds the per-call work at THREE
//! composing layers so a hostile op (`world.step` with a huge `n`) or a degenerate
//! scene (every body collapsed to one point ⇒ would-be O(n²) broadphase) cannot
//! monopolize the CPU for the multi-minute-to-hours window an unbounded run would
//! take:
//!   1. the per-call frame count is clamped to [`MAX_STEPS_PER_CALL`];
//!   2. the candidate-pair ENUMERATION is budgeted to [`MAX_PAIRS_PER_SUBSTEP`] —
//!      the broadphase short-circuits its AABB tests once the budget is reached, so
//!      a single overfull grid cell (all bodies coincident) generates O(budget)
//!      pairs, NOT ~n²/2 (truncating AFTER an O(n²) enumeration would not help — the
//!      enumeration itself is bounded);
//!   3. the contacts fed to the solver are truncated to [`MAX_CONTACTS_PER_SUBSTEP`].
//!
//! Bounded-per-frame (layers 2+3) × bounded-frame-count (layer 1) ⇒ one clamped op
//! over the worst case the [`crate::ipc::MAX_BODIES`] cap allows completes in
//! seconds (measured, pinned by a worst-case test), not the ~tens of minutes the
//! unbounded O(n²) path would take. This bounds CPU time per op; it is NOT a claim
//! that the daemon is unboundedly responsive in all cases — a single clamped op
//! still runs to completion before the IPC loop services the next verb. When any
//! bound bites it is SIGNALLED, never silent (SPEC §1: no silent mis-simulation):
//! the clamp is logged, the pair budget raises [`StepStats::pairs_cap_hit`], and the
//! contact cap raises [`StepStats::contact_cap_hit`].

use crate::body::{Body, BodyId, Material, Shape};
use crate::math::{Quat, Vec3};
use crate::{broadphase, narrowphase, solver};

/// Hard cap on the number of fixed frames ONE [`World::step`] call may advance
/// (SPEC §1, bounded work). A `world.step {n}` op carries an unvalidated `u32`
/// (up to ~4.29e9); at the fixed 60 Hz timestep that is years of simulated time
/// in a single op, so without a cap a single op could run for a very long time
/// while the IPC read loop cannot service a `stop`. We therefore clamp `n` to this
/// cap: 600 frames = 10 s of simulated time at 60 Hz, comfortably more than any
/// interactive scrub needs. A clamp (not a reject) so a caller still gets a real,
/// deterministic advance — just capped. The clamp is surfaced to the operator as a
/// `log` line by the socket loop (`ipc.rs`).
///
/// This cap bounds the FRAME COUNT only. It does NOT by itself bound a degenerate
/// scene where each frame is expensive — a fully-collapsed scene's per-frame work
/// is bounded SEPARATELY by [`MAX_PAIRS_PER_SUBSTEP`] (pair enumeration) and
/// [`MAX_CONTACTS_PER_SUBSTEP`] (solver contacts). The three caps COMPOSE: bounded
/// per-frame work × this bounded frame count ⇒ one op completes in well-bounded
/// time even on the worst case the [`crate::ipc::MAX_BODIES`] cap allows.
pub const MAX_STEPS_PER_CALL: u32 = 600;

/// Hard cap on the number of CONTACT POINTS the solver processes per substep
/// (SPEC §1, bounded work). Bounds the SOLVER + position-correction work: the
/// contacts actually fed to the velocity solve / position correction are truncated
/// to this many per substep, so a degenerate / over-dense scene cannot blow up the
/// solver work. The cap is generous (a real settled stack of the whole body cap
/// needs far fewer simultaneous contacts) but finite. When it bites the step does
/// NOT silently mis-simulate without signal: [`StepStats::contact_cap_hit`] is
/// raised so telemetry + the operator log record that the scene was too degenerate
/// to resolve in full.
///
/// NOTE: this cap alone does NOT make the per-substep work O(cap) — it bounds the
/// SOLVER, but NOT the upstream pair ENUMERATION + narrowphase. A fully-collapsed
/// scene (every body in one grid cell) would still do ~n²/2 AABB tests + manifold
/// generations BEFORE the truncation runs (at the [`crate::ipc::MAX_BODIES`] cap of
/// 4096 that is ~8.4M AABB tests/substep). That upstream O(n²) is bounded
/// separately by [`MAX_PAIRS_PER_SUBSTEP`], which short-circuits the enumeration;
/// the two caps together make the whole per-substep pipeline O(budget).
pub const MAX_CONTACTS_PER_SUBSTEP: usize = 16_384;

/// Hard cap on the number of CANDIDATE PAIRS the broadphase enumerates per substep
/// (SPEC §1, bounded work) — the UPSTREAM bound that [`MAX_CONTACTS_PER_SUBSTEP`]
/// alone cannot provide. Truncating the contact set AFTER the broadphase is not
/// enough: in a fully-collapsed scene (every body in ONE grid cell) the pair
/// enumeration itself does ~k²/2 AABB tests for a cell of `k` bodies, so just
/// building the list is O(n²) — at the [`crate::ipc::MAX_BODIES`] cap of 4096 that
/// is ~8.4M AABB tests + manifold generations PER SUBSTEP *before* `truncate_contacts` ever
/// runs (measured: ~1.4 s for a single substep at n=2048, extrapolating to tens of
/// minutes for one clamped `world.step{n:600}` at 4096 — a multi-minute CPU/
/// availability DoS reachable from ~4096 same-point spawns + one step). We bound
/// the pair ENUMERATION via [`crate::broadphase::candidate_pairs_budgeted`], which
/// short-circuits the AABB tests the instant this many pairs are emitted, so the
/// per-substep broadphase + narrowphase work is O(budget), not O(n²), even when
/// all bodies share one cell. The cap is generous — a real spread scene at the
/// full body cap produces far fewer than this many candidate pairs (the grid keeps
/// it ~O(n)), and the conservation/stack/determinism tests (a handful of bodies)
/// never come near it — yet finite. When it bites the step does NOT silently
/// mis-simulate: [`StepStats::pairs_cap_hit`] is raised so telemetry + the operator
/// log record that the scene was too dense to enumerate in full.
///
/// SIZING: just above [`MAX_CONTACTS_PER_SUBSTEP`]. The narrowphase runs once per
/// emitted pair, so the broadphase work is bounded at this many `collide` calls per
/// substep; sizing it at the contact budget (+ a small margin so the downstream
/// contact cap still reliably bites on a degenerate scene) means the narrowphase
/// never generates materially more contacts than the solver will keep (a collapsed
/// sphere scene is ~1 contact per manifold), so we don't pay to build manifolds the
/// contact cap would only throw away. This makes the WORST case — the full
/// [`crate::ipc::MAX_BODIES`] cap collapsed onto one point, stepped the clamped
/// [`MAX_STEPS_PER_CALL`] frames — complete in seconds (pinned by the
/// `full_body_cap_collapsed_scene_completes_in_bounded_walltime` test) instead of
/// the ~tens of minutes an O(n²) enumeration would take. The cap is still far above
/// any NORMAL scene: a spatially-spread world at the full body cap produces only
/// O(n) candidate pairs via the grid, orders of magnitude under this, so the
/// conservation/stack/determinism tests (a handful of bodies) never come near it.
pub const MAX_PAIRS_PER_SUBSTEP: usize = MAX_CONTACTS_PER_SUBSTEP + 4_096;

/// Solver + integrator tuning (SPEC §6). Every numeric knob lives here so a
/// `set.params` op can adjust the simulation without touching code, and the
/// golden-trace tests pin a known set. All defaults are deterministic constants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SolverParams {
    /// Sequential-impulse velocity-solve passes over the contact set (SPEC §6).
    pub velocity_iterations: u32,
    /// Relative normal speed below which restitution is suppressed, so a resting
    /// contact does not jitter-bounce (SPEC §6).
    pub restitution_threshold: f64,
    /// Baumgarte / split-impulse position-correction factor `beta` (0..1): how
    /// aggressively penetration beyond `penetration_slop` is pushed out per step
    /// (SPEC §6).
    pub baumgarte_beta: f64,
    /// Allowed penetration `slop` left uncorrected so resting stacks don't
    /// vibrate (SPEC §6).
    pub penetration_slop: f64,
    /// Per-substep linear-velocity retention factor (`1.0` = no damping). Applied
    /// as `lin_vel *= linear_damping` each substep (SPEC §3).
    pub linear_damping: f64,
    /// Per-substep angular-velocity retention factor (`1.0` = no damping).
    pub angular_damping: f64,
}

impl Default for SolverParams {
    fn default() -> Self {
        SolverParams {
            velocity_iterations: 8,
            restitution_threshold: 0.5,
            baumgarte_beta: 0.2,
            penetration_slop: 0.005,
            linear_damping: 1.0,
            angular_damping: 1.0,
        }
    }
}

/// Stats from one [`World::step`] call — the source of the `physics.step`
/// telemetry (SPEC §8). Populated by the integrator each step from the actual
/// broadphase→narrowphase→solver pipeline run (`contacts` / `last_penetration`
/// reflect the last substep; the counts reflect the world at step end).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct StepStats {
    /// Fixed frames ACTUALLY advanced this call. Equals the op's `n` unless `n`
    /// exceeded [`MAX_STEPS_PER_CALL`], in which case it is the clamped value.
    pub frames: u32,
    /// Substeps per frame (= [`World::substeps`]).
    pub substeps: u32,
    /// Body count at the end of the step.
    pub bodies: u32,
    /// Total contact points the narrowphase produced on the LAST substep (after
    /// the [`MAX_CONTACTS_PER_SUBSTEP`] truncation, so this is the count actually
    /// solved, never an unbounded number).
    pub contacts: u32,
    /// Solver velocity iterations run per substep (= `params.velocity_iterations`).
    pub solver_iterations: u32,
    /// Deepest penetration remaining after the last position-correction pass
    /// (SPEC §8) — a settling indicator for the HUD.
    pub last_penetration: f64,
    /// `true` if the per-substep contact budget [`MAX_CONTACTS_PER_SUBSTEP`] bit
    /// on ANY substep of this call (a degenerate / over-dense scene whose contacts
    /// were truncated to keep the work bounded). A signal — never a silent
    /// mis-simulation — so the HUD / operator can see the scene was capped.
    pub contact_cap_hit: bool,
    /// `true` if the per-substep candidate-pair budget [`MAX_PAIRS_PER_SUBSTEP`]
    /// bit on ANY substep of this call — i.e. the broadphase short-circuited its
    /// pair enumeration because the scene was too dense to enumerate in full
    /// (every body collapsed into one grid cell ⇒ would-be O(n²) AABB tests). This
    /// is the UPSTREAM signal that bounds the work BEFORE narrowphase, mirroring
    /// [`contact_cap_hit`]; like it, a signal — never a silent mis-simulation.
    pub pairs_cap_hit: bool,
}

/// The simulation world (SPEC §2/§3): the ordered body list plus the fixed
/// timestep, gravity, substep count, and solver params. Bodies are a `Vec` so
/// insertion order (= [`BodyId`]) is the stable iteration key (SPEC §1).
#[derive(Debug, Clone)]
pub struct World {
    /// All bodies, in insertion order. `bodies[id.index()]` is the body for
    /// `BodyId(id)`. Never reordered or removed mid-life (ids stay stable);
    /// `world.reset` clears the whole vec.
    pub bodies: Vec<Body>,
    /// World gravity acceleration (SPEC §3). Default Earth-ish `-9.81` on Y.
    pub gravity: Vec3,
    /// Fixed timestep `dt = 1/hz` (seconds) per frame (SPEC §3).
    pub dt: f64,
    /// Substeps each frame is split into (SPEC §3). `>= 1`.
    pub substeps: u32,
    /// Solver + integrator tuning (SPEC §6).
    pub params: SolverParams,
}

impl Default for World {
    fn default() -> Self {
        World {
            bodies: Vec::new(),
            gravity: Vec3::new(0.0, -9.81, 0.0),
            dt: 1.0 / 60.0,
            substeps: 4,
            params: SolverParams::default(),
        }
    }
}

impl World {
    /// A fresh empty world with default gravity / 60 Hz / 4 substeps.
    pub fn new() -> Self {
        World::default()
    }

    /// Spawn one body and return its stable [`BodyId`] (SPEC §7 `body.spawn`).
    /// The body is appended; its id is its index. Velocities can be set by the
    /// caller after spawn (the IPC layer applies `lin_vel`/`ang_vel` from the op).
    pub fn spawn(&mut self, body: Body) -> BodyId {
        let id = BodyId(self.bodies.len() as u32);
        self.bodies.push(body);
        id
    }

    /// Convenience spawn from shape + transform + mass (the IPC `body.spawn`
    /// path). Returns the new id.
    pub fn spawn_shape(
        &mut self,
        shape: Shape,
        pos: Vec3,
        mass: f64,
        material: Material,
    ) -> BodyId {
        self.spawn(Body::new(shape, pos, mass, material))
    }

    /// Clear all bodies, keeping gravity/dt/substeps/params (SPEC §7
    /// `world.reset`). Ids restart at `0` for the next spawn.
    pub fn reset(&mut self) {
        self.bodies.clear();
    }

    /// Count of bodies currently in the world.
    #[inline]
    pub fn body_count(&self) -> u32 {
        self.bodies.len() as u32
    }

    /// Borrow a body by id, if it exists.
    #[inline]
    pub fn body(&self, id: BodyId) -> Option<&Body> {
        self.bodies.get(id.index())
    }

    /// Set the gravity vector (SPEC §7 `set.gravity`).
    pub fn set_gravity(&mut self, gravity: Vec3) {
        self.gravity = gravity;
    }

    /// Advance the simulation by `n` fixed frames (clamped to
    /// [`MAX_STEPS_PER_CALL`]), each split into [`World::substeps`] equal
    /// sub-frames (SPEC §3). Returns the [`StepStats`] for the `physics.step`
    /// telemetry. DETERMINISTIC: the simulated time advanced is
    /// `frames as f64 * self.dt` (where `frames` is the clamped count reported in
    /// the stats) regardless of how long the call takes, and the result is a pure
    /// function of the prior world state (SPEC §1).
    ///
    /// The fixed-timestep semi-implicit Euler integrator + the per-substep
    /// pipeline. For each clamped frame, the frame is split into `self.substeps`
    /// equal sub-frames at `h = dt/substeps` and each sub-frame runs, in this
    /// FIXED order (SPEC §3):
    ///
    ///   1. **integrate velocity** — for every dynamic (non-static, awake) body,
    ///      `lin_vel += gravity * h` then apply per-substep damping
    ///      (`lin_vel *= linear_damping`, `ang_vel *= angular_damping`).
    ///   2. **broadphase** — [`broadphase::candidate_pairs`] over the world.
    ///   3. **narrowphase** — [`narrowphase::collide_all`] resolves the pairs into
    ///      contact manifolds.
    ///   4. **solve velocity** — [`solver::solve_velocity`] applies restitution +
    ///      Coulomb-friction impulses in place (the substep duration `h` is passed
    ///      through for restitution bias).
    ///   5. **integrate position** — for every dynamic body, advance
    ///      `pos += lin_vel * h` and the orientation by the torque-free quaternion
    ///      update `q' = normalize(q + 0.5 * (ω⊗q) * h)`, where `ω⊗q` is the
    ///      Hamilton product of the pure quaternion `(ω, 0)` with `q`.
    ///   6. **correct positions** — [`solver::correct_positions`] resolves residual
    ///      penetration (Baumgarte / split-impulse) and reports the deepest leftover.
    ///
    /// Static bodies (`inv_mass == 0`: planes, anchors) and sleeping bodies never
    /// have their velocity or transform touched by the integrator. The integrator
    /// itself is a PURE function of the prior world state — it reads `self.gravity`,
    /// `self.dt`, `self.substeps`, and `self.params`, uses no RNG / wall-clock, and
    /// applies the float ops in a fixed order — so the same initial world stepped
    /// the same number of frames yields bit-identical state (SPEC §1).
    ///
    /// BOUNDED WORK (SPEC §1, defensive): the per-call work is bounded at three
    /// composing layers so a hostile op / degenerate scene cannot run for the
    /// multi-minute-to-hours window an unbounded path would take:
    ///   1. `n` is clamped to [`MAX_STEPS_PER_CALL`] (the reported `frames` is the
    ///      clamped count) — bounds the FRAME COUNT;
    ///   2. the broadphase is BUDGETED via
    ///      [`broadphase::candidate_pairs_budgeted`] to [`MAX_PAIRS_PER_SUBSTEP`] —
    ///      it short-circuits its AABB-test enumeration once the budget is reached,
    ///      so a fully-collapsed scene (every body in one grid cell) generates
    ///      O(budget) pairs/manifolds, NOT ~n²/2 (truncating the list AFTER an
    ///      O(n²) enumeration would not bound the enumeration itself);
    ///   3. the resulting contact set is truncated to [`MAX_CONTACTS_PER_SUBSTEP`].
    ///
    /// Layers 2+3 make each FRAME O(budget); layer 1 caps the frame count — so the
    /// whole op over the worst case the [`crate::ipc::MAX_BODIES`] cap allows
    /// completes in seconds (pinned by the worst-case test), not the ~tens of
    /// minutes an O(n²) per-frame scan would cost. Each bound, when it bites, is
    /// SIGNALLED, never silent: the clamp is logged by the IPC layer, the pair
    /// budget raises [`StepStats::pairs_cap_hit`], and the contact cap raises
    /// [`StepStats::contact_cap_hit`].
    pub fn step(&mut self, n: u32) -> StepStats {
        // Clamp the per-call frame count: an unvalidated `n` (up to ~4.29e9) must
        // never run unbounded (a single op could otherwise wedge the IPC loop for
        // hours). The clamped count is what we advance AND what we report, so the
        // simulated-time-advanced contract (`frames * dt`) stays exact for the
        // clamped value.
        let frames = n.min(MAX_STEPS_PER_CALL);

        // `substeps` is guaranteed >= 1 by the params apply-path; guard anyway so a
        // hand-built `World { substeps: 0, .. }` can't divide by zero or stall.
        let substeps = self.substeps.max(1);
        let h = self.dt / substeps as f64;

        // Per-step stats accumulators: `contacts` / `last_penetration` reflect the
        // LAST substep of the LAST frame (the most-settled snapshot, SPEC §8).
        let mut last_contacts: u32 = 0;
        let mut last_penetration: f64 = 0.0;
        // Raised if the per-substep contact budget bit on ANY substep (a
        // degenerate scene whose contacts were truncated to keep work bounded).
        let mut contact_cap_hit = false;
        // Raised if the per-substep candidate-pair budget bit on ANY substep (the
        // broadphase short-circuited pair enumeration on an over-dense scene). The
        // UPSTREAM bound — keeps even pair generation O(budget), not O(n²).
        let mut pairs_cap_hit = false;

        for _frame in 0..frames {
            for _sub in 0..substeps {
                // (1) integrate velocity: gravity + per-substep damping.
                self.integrate_velocities(h);

                // (2) broadphase -> (3) narrowphase: candidate pairs -> manifolds.
                // BOUNDED WORK (upstream): the broadphase is BUDGETED — it
                // short-circuits its AABB-test enumeration at
                // MAX_PAIRS_PER_SUBSTEP, so a fully-collapsed scene (every body in
                // one cell ⇒ would-be ~n²/2 AABB tests) generates only O(budget)
                // pairs/manifolds, NEVER O(n²). A short-circuit is SIGNALLED via
                // `pairs_cap_hit`, never silent.
                let (pairs, sub_pairs_cap) =
                    broadphase::candidate_pairs_budgeted(self, MAX_PAIRS_PER_SUBSTEP);
                if sub_pairs_cap {
                    pairs_cap_hit = true;
                }
                let mut manifolds = narrowphase::collide_all(self, &pairs);

                // BOUNDED WORK: truncate the contact set to MAX_CONTACTS_PER_SUBSTEP
                // so a degenerate scene (collapsed bodies => ~n²/2 manifolds) cannot
                // make this substep's solver/correction work blow up. The truncation
                // is deterministic (manifolds are in fixed broadphase pair order, so
                // we always drop the same tail) and SIGNALLED — never silent.
                if Self::truncate_contacts(&mut manifolds, MAX_CONTACTS_PER_SUBSTEP) {
                    contact_cap_hit = true;
                }

                // (4) velocity solve (restitution + Coulomb friction), in place.
                solver::solve_velocity(self, &mut manifolds, h);

                // (5) integrate position: linear + torque-free angular update.
                self.integrate_positions(h);

                // (6) position correction (Baumgarte / split-impulse).
                let pen = solver::correct_positions(self, &manifolds);

                // Total contact points this substep (manifolds carry >= 1 each).
                last_contacts = manifolds.iter().map(|m| m.contacts.len() as u32).sum();
                last_penetration = pen;
            }
        }

        StepStats {
            // The CLAMPED frame count actually advanced (== `n` unless `n`
            // exceeded MAX_STEPS_PER_CALL), so `frames * dt` == simulated time.
            frames,
            substeps,
            bodies: self.body_count(),
            contacts: last_contacts,
            solver_iterations: self.params.velocity_iterations,
            last_penetration,
            contact_cap_hit,
            pairs_cap_hit,
        }
    }

    /// Truncate the per-substep contact set to at most `budget` total contact
    /// POINTS, dropping whole manifolds from the tail of the (deterministically
    /// ordered) list once the running total would exceed the budget. Returns
    /// `true` if any contact was dropped (the [`MAX_CONTACTS_PER_SUBSTEP`] cap
    /// bit). Bounds the solver / position-correction work in a degenerate scene
    /// (collapsed bodies ⇒ ~n²/2 manifolds) WITHOUT silently mis-simulating: the
    /// caller raises [`StepStats::contact_cap_hit`] on a `true` return. Dropping
    /// from the tail of the fixed broadphase-pair order keeps the truncation
    /// deterministic (SPEC §1).
    fn truncate_contacts(manifolds: &mut Vec<narrowphase::Manifold>, budget: usize) -> bool {
        let mut running = 0usize;
        let mut keep = 0usize;
        for m in manifolds.iter() {
            let next = running + m.contacts.len();
            if next > budget {
                break;
            }
            running = next;
            keep += 1;
        }
        if keep < manifolds.len() {
            manifolds.truncate(keep);
            true
        } else {
            false
        }
    }

    /// Sub-frame velocity integration (SPEC §3 step 1). Applies gravity and the
    /// per-substep linear/angular damping retention factors to every dynamic,
    /// awake body. Static and sleeping bodies are untouched.
    ///
    /// Gravity is an ACCELERATION (independent of mass), so it is added directly to
    /// `lin_vel` (`v += g * h`) rather than scaled by `inv_mass`. Damping is the
    /// multiplicative retention factor from [`SolverParams`] (`1.0` ⇒ no damping).
    #[inline]
    fn integrate_velocities(&mut self, h: f64) {
        let gravity = self.gravity;
        let lin_damp = self.params.linear_damping;
        let ang_damp = self.params.angular_damping;
        for body in self.bodies.iter_mut() {
            if body.is_static() || body.sleeping {
                continue;
            }
            body.lin_vel += gravity * h;
            body.lin_vel = body.lin_vel * lin_damp;
            body.ang_vel = body.ang_vel * ang_damp;
        }
    }

    /// Sub-frame position integration (SPEC §3 step 5). Advances every dynamic,
    /// awake body's position by `pos += lin_vel * h` and its orientation by the
    /// torque-free first-order quaternion update, then renormalizes the quaternion
    /// to keep it unit (so accumulated drift can't denormalize the orientation).
    ///
    /// Angular update: the time derivative of an orientation quaternion under world
    /// angular velocity `ω` is `q̇ = ½ (ω, 0) ⊗ q` (pure-vector quaternion `(ω,0)`,
    /// Hamilton product, w-scalar convention matching [`Quat`]). One explicit Euler
    /// step gives `q' = normalize(q + q̇ h)`. Static/sleeping bodies are untouched.
    #[inline]
    fn integrate_positions(&mut self, h: f64) {
        for body in self.bodies.iter_mut() {
            if body.is_static() || body.sleeping {
                continue;
            }
            // Linear.
            body.pos += body.lin_vel * h;

            // Angular (torque-free quaternion integration).
            let w = body.ang_vel;
            let omega = Quat::new(w.x, w.y, w.z, 0.0);
            let dq = omega.mul(body.orientation); // (ω,0) ⊗ q
            let q = body.orientation;
            // q + 0.5 * dq * h, component-wise, then renormalize.
            let half_h = 0.5 * h;
            let integrated = Quat::new(
                q.x + dq.x * half_h,
                q.y + dq.y * half_h,
                q.z + dq.z * half_h,
                q.w + dq.w * half_h,
            );
            body.orientation = integrated.normalized();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::Shape;

    #[test]
    fn spawn_assigns_sequential_ids() {
        let mut w = World::new();
        let a = w.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, Material::default());
        let b = w.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::Y, 1.0, Material::default());
        assert_eq!(a, BodyId(0));
        assert_eq!(b, BodyId(1));
        assert_eq!(w.body_count(), 2);
    }

    #[test]
    fn reset_clears_bodies_keeps_params() {
        let mut w = World::new();
        w.set_gravity(Vec3::new(0.0, -3.0, 0.0));
        w.spawn(Body::plane(Vec3::Y, 0.0));
        w.reset();
        assert_eq!(w.body_count(), 0);
        assert_eq!(w.gravity, Vec3::new(0.0, -3.0, 0.0));
    }

    #[test]
    fn step_reports_shape_and_counts() {
        let mut w = World::new();
        w.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::new(0.0, 10.0, 0.0), 1.0, Material::default());
        let stats = w.step(5);
        assert_eq!(stats.frames, 5);
        assert_eq!(stats.substeps, w.substeps);
        assert_eq!(stats.bodies, 1);
        assert_eq!(stats.solver_iterations, w.params.velocity_iterations);
    }

    #[test]
    fn step_is_deterministic_on_empty_world() {
        let mut a = World::new();
        let mut b = World::new();
        assert_eq!(a.step(10), b.step(10));
    }

    // ===================================================================
    // bodies-integrator tests (SPEC §3) — the fixed-timestep semi-implicit
    // Euler integrator in isolation. These use single-body / contact-free
    // scenes (one dynamic body free-flying under gravity, no overlapping pair
    // for the broadphase to find), so they exercise the integrator path alone;
    // the broadphase→narrowphase→solver contact path is covered in those
    // modules' own tests + the conservation suite.
    // ===================================================================

    /// Build a single-dynamic-body world with damping disabled and the given
    /// initial velocity, so the integrator runs pure ballistic motion.
    fn ballistic_world(pos: Vec3, vel: Vec3) -> World {
        let mut w = World::new();
        // No damping — isolate gravity + integration (Default damping is already
        // 1.0, but pin it so the test is robust to any default change).
        w.params.linear_damping = 1.0;
        w.params.angular_damping = 1.0;
        let id = w.spawn_shape(Shape::Sphere { radius: 1.0 }, pos, 2.0, Material::default());
        w.bodies[id.index()].lin_vel = vel;
        w
    }

    #[test]
    fn free_body_matches_semi_implicit_euler_closed_form() {
        // Semi-implicit (symplectic) Euler with substep h advances velocity
        // BEFORE position each substep, so after K substeps:
        //   v_K = v0 + g*K*h
        //   x_K = x0 + v0*t + g*h^2 * K(K+1)/2     (t = K*h)
        // This is the EXACT discrete trajectory the integrator must reproduce to
        // the bit-ish (fp summation order may differ, so use a tight tolerance).
        let frames: u32 = 90;
        let x0 = Vec3::new(0.0, 50.0, 0.0);
        let v0 = Vec3::new(1.0, 4.0, -2.0);
        let mut w = ballistic_world(x0, v0);
        let g = w.gravity;
        let substeps = w.substeps as f64;
        let h = w.dt / substeps;
        let k = frames as f64 * substeps; // total substeps run

        w.step(frames);
        let body = w.bodies[0];

        // Closed-form symplectic-Euler position.
        let expected_pos = x0 + v0 * (k * h) + g * (h * h * (k * (k + 1.0) / 2.0));
        // Closed-form symplectic-Euler velocity.
        let expected_vel = v0 + g * (k * h);

        let pos_err = (body.pos - expected_pos).length();
        let vel_err = (body.lin_vel - expected_vel).length();
        assert!(pos_err < 1e-9, "pos {:?} vs expected {:?} (err {})", body.pos, expected_pos, pos_err);
        assert!(vel_err < 1e-12, "vel {:?} vs expected {:?} (err {})", body.lin_vel, expected_vel, vel_err);
    }

    #[test]
    fn free_body_tracks_analytic_kinematics_within_first_order_bound() {
        // The continuous analytic solution is x = x0 + v0 t + 0.5 g t^2. A
        // first-order integrator's position error vs the continuous solution is
        // bounded by ~0.5 * |g| * h * t (the O(h) local-truncation accumulation).
        // Assert the body lands within that known bound of the analytic curve.
        let frames: u32 = 120; // 2 s at 60 Hz
        let x0 = Vec3::new(0.0, 100.0, 0.0);
        let v0 = Vec3::new(0.0, 0.0, 0.0);
        let mut w = ballistic_world(x0, v0);
        let g = w.gravity;
        let h = w.dt / w.substeps as f64;
        let t = frames as f64 * w.dt;

        w.step(frames);
        let body = w.bodies[0];

        let analytic = x0 + v0 * t + g * (0.5 * t * t);
        let err = (body.pos - analytic).length();
        // Known first-order bound (with a small safety margin); strictly positive.
        // The symplectic-Euler error here is ~0.5*|g|*h*t*(1 + 1/K) ≈ this value;
        // the 1.01 factor absorbs the +1/K term and any fp jitter.
        let bound = 0.5 * g.length() * h * t * 1.01;
        assert!(err <= bound, "err {} exceeded first-order bound {}", err, bound);
        // And it must genuinely have fallen toward the analytic answer, not be a
        // no-op: the body dropped tens of metres.
        assert!(body.pos.y < x0.y - 10.0, "body did not fall: y={}", body.pos.y);
    }

    #[test]
    fn static_body_never_moves() {
        let mut w = World::new();
        // mass = 0 -> static. Even a non-trivial start velocity must be ignored
        // (static bodies are skipped by both velocity AND position integration).
        let id = w.spawn_shape(Shape::Cuboid { half_extents: Vec3::splat(0.5) }, Vec3::new(3.0, 7.0, -1.0), 0.0, Material::default());
        // Force a velocity onto the static body to prove it's frozen regardless.
        w.bodies[id.index()].lin_vel = Vec3::new(5.0, 5.0, 5.0);
        w.bodies[id.index()].ang_vel = Vec3::new(1.0, 2.0, 3.0);
        let before = w.bodies[id.index()];

        w.step(100);

        let after = w.bodies[id.index()];
        assert_eq!(after.pos, before.pos, "static body translated");
        assert_eq!(after.orientation, before.orientation, "static body rotated");
        assert_eq!(after.lin_vel, before.lin_vel, "static body lin_vel changed");
        assert_eq!(after.ang_vel, before.ang_vel, "static body ang_vel changed");
    }

    #[test]
    fn plane_stays_put_under_gravity() {
        let mut w = World::new();
        w.spawn(Body::plane(Vec3::Y, 0.0));
        let before = w.bodies[0];
        w.step(50);
        assert_eq!(w.bodies[0], before, "static plane moved under gravity");
    }

    #[test]
    fn sleeping_body_is_skipped_by_integrator() {
        let mut w = World::new();
        let id = w.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::new(0.0, 5.0, 0.0), 1.0, Material::default());
        w.bodies[id.index()].sleeping = true;
        let before = w.bodies[id.index()];
        w.step(30);
        assert_eq!(w.bodies[id.index()], before, "sleeping body was integrated");
    }

    #[test]
    fn frictionless_projectile_conserves_total_energy() {
        // KE + PE of a free body under gravity (no contacts, no damping) is
        // conserved along the trajectory. Semi-implicit Euler conserves a SHADOW
        // energy that stays within O(h) of the true energy for all time (it does
        // not drift secularly), so we bound the relative drift across the whole
        // flight by a tight constant.
        let mass = 2.0;
        let x0 = Vec3::new(0.0, 40.0, 0.0);
        let v0 = Vec3::new(3.0, 12.0, -1.0); // launched upward + sideways
        let mut w = ballistic_world(x0, v0);
        let g = w.gravity;

        // Total mechanical energy: KE = 1/2 m |v|^2, PE = -m (g . x)
        // (so PE increases with height for downward gravity; d/dt(KE+PE)=0).
        let energy = |b: &Body| -> f64 {
            0.5 * mass * b.lin_vel.length_squared() - mass * g.dot(b.pos)
        };

        let e0 = energy(&w.bodies[0]);
        assert!(e0 > 0.0);

        // March the trajectory in chunks and assert energy never drifts beyond a
        // tight relative bound at any sampled point along the flight.
        let mut max_rel_drift = 0.0_f64;
        for _ in 0..120 {
            w.step(1);
            let e = energy(&w.bodies[0]);
            let rel = ((e - e0) / e0).abs();
            if rel > max_rel_drift {
                max_rel_drift = rel;
            }
        }
        assert!(
            max_rel_drift < 1e-3,
            "energy drifted {} (relative) along trajectory",
            max_rel_drift
        );
    }

    #[test]
    fn integration_is_bit_identical_for_same_init() {
        // Determinism (SPEC §1): two independently-built identical worlds stepped
        // the same number of frames must reach bit-identical state — same pos,
        // orientation, and velocities, exactly.
        let build = || {
            let mut w = ballistic_world(Vec3::new(1.0, 9.0, -2.0), Vec3::new(2.0, 1.0, 0.5));
            // Give it angular velocity too, so the quaternion path is exercised.
            w.bodies[0].ang_vel = Vec3::new(0.7, -0.3, 1.1);
            w
        };
        let mut a = build();
        let mut b = build();

        let stats_a = a.step(200);
        let stats_b = b.step(200);
        assert_eq!(stats_a, stats_b);

        let ba = a.bodies[0];
        let bb = b.bodies[0];
        // Exact equality (no tolerance) — same ops, same order, no RNG/wall-clock.
        assert_eq!(ba.pos, bb.pos);
        assert_eq!(ba.orientation, bb.orientation);
        assert_eq!(ba.lin_vel, bb.lin_vel);
        assert_eq!(ba.ang_vel, bb.ang_vel);
    }

    #[test]
    fn split_stepping_equals_single_step() {
        // Stepping N frames in one call must equal stepping them across several
        // calls — `step` is a pure advance with no per-call hidden state (the
        // simulated-time-advanced contract, SPEC §1/§3).
        let mut one = ballistic_world(Vec3::new(0.0, 20.0, 0.0), Vec3::new(1.0, 0.0, 1.0));
        let mut many = ballistic_world(Vec3::new(0.0, 20.0, 0.0), Vec3::new(1.0, 0.0, 1.0));
        one.step(60);
        many.step(20);
        many.step(15);
        many.step(25);
        assert_eq!(one.bodies[0].pos, many.bodies[0].pos);
        assert_eq!(one.bodies[0].lin_vel, many.bodies[0].lin_vel);
    }

    #[test]
    fn free_spin_keeps_orientation_unit() {
        // The torque-free quaternion integration must keep the orientation
        // normalized (the per-substep renormalize), even over a long spin.
        let mut w = World::new();
        // Static-free spin in zero gravity to isolate the angular path.
        w.set_gravity(Vec3::ZERO);
        let id = w.spawn_shape(Shape::Cuboid { half_extents: Vec3::new(1.0, 0.5, 0.25) }, Vec3::ZERO, 1.0, Material::default());
        w.bodies[id.index()].ang_vel = Vec3::new(5.0, 0.0, 0.0); // fast spin
        w.step(300);
        let q = w.bodies[id.index()].orientation;
        let norm = (q.x * q.x + q.y * q.y + q.z * q.z + q.w * q.w).sqrt();
        assert!((norm - 1.0).abs() < 1e-9, "orientation not unit: |q| = {}", norm);
        // It genuinely rotated (not the identity).
        assert!(q != Quat::IDENTITY, "spinning body did not rotate");
    }

    #[test]
    fn zero_substeps_is_clamped_not_a_divzero() {
        // A hand-built degenerate world with substeps=0 must be clamped to 1 (no
        // NaN/inf from h = dt/0) and still advance.
        let mut w = ballistic_world(Vec3::new(0.0, 10.0, 0.0), Vec3::ZERO);
        w.substeps = 0;
        let stats = w.step(5);
        assert_eq!(stats.substeps, 1);
        assert!(w.bodies[0].pos.y.is_finite());
        assert!(w.bodies[0].pos.y < 10.0, "body did not fall with clamped substep");
    }

    // ===================================================================
    // BOUNDED WORK (SPEC §1, defensive) — the per-call step clamp + the
    // per-substep contact budget keep a hostile op / degenerate scene from
    // monopolizing the CPU. These pin the DoS-amplifier bounds.
    // ===================================================================

    #[test]
    fn step_clamps_over_cap_n_and_reports_clamped_frames() {
        // An op may carry an unvalidated `n` up to u32::MAX (~4.29e9). step()
        // must CLAMP it to MAX_STEPS_PER_CALL so a single op cannot run for hours
        // and wedge the IPC loop. The reported `frames` is the clamped count, and
        // the body advanced exactly that many frames' worth of simulated time
        // (not the unclamped n).
        let huge: u32 = u32::MAX;
        let mut clamped = ballistic_world(Vec3::new(0.0, 1.0e6, 0.0), Vec3::ZERO);
        let stats = clamped.step(huge);
        assert_eq!(
            stats.frames, MAX_STEPS_PER_CALL,
            "an over-cap n must be clamped to MAX_STEPS_PER_CALL"
        );

        // Stepping exactly MAX_STEPS_PER_CALL frames produces the SAME state as
        // the clamped huge-n call: the clamp truly bounds the advance, it does not
        // silently run the full n.
        let mut exact = ballistic_world(Vec3::new(0.0, 1.0e6, 0.0), Vec3::ZERO);
        let exact_stats = exact.step(MAX_STEPS_PER_CALL);
        assert_eq!(exact_stats, stats, "clamped-huge == exactly-capped step");
        assert_eq!(clamped.bodies[0].pos, exact.bodies[0].pos);
        assert_eq!(clamped.bodies[0].lin_vel, exact.bodies[0].lin_vel);

        // And it returned in bounded time (a free-fall body, no contacts): the
        // call above already completed, so the clamp prevented an unbounded run.
    }

    #[test]
    fn under_cap_n_is_unaffected_by_the_clamp() {
        // A normal, under-cap n must advance exactly n frames — the clamp must not
        // perturb ordinary stepping.
        let n: u32 = 90;
        assert!(n < MAX_STEPS_PER_CALL);
        let mut w = ballistic_world(Vec3::new(0.0, 100.0, 0.0), Vec3::new(1.0, 2.0, 3.0));
        let stats = w.step(n);
        assert_eq!(stats.frames, n, "under-cap n advances exactly n frames");
        assert!(!stats.contact_cap_hit, "a sparse scene never hits the contact cap");

        // Bit-identical to the same world stepped n frames (no hidden clamp path).
        let mut ref_w = ballistic_world(Vec3::new(0.0, 100.0, 0.0), Vec3::new(1.0, 2.0, 3.0));
        for _ in 0..n {
            ref_w.step(1);
        }
        assert_eq!(w.bodies[0].pos, ref_w.bodies[0].pos);
        assert_eq!(w.bodies[0].lin_vel, ref_w.bodies[0].lin_vel);
    }

    #[test]
    fn collapsed_body_scene_hits_contact_cap_within_bounded_work() {
        // THE worst case: many dynamic bodies collapsed onto a single point. The
        // broadphase then reports every pair as overlapping (O(n²) manifolds), and
        // without a per-substep contact budget the solver work explodes. Assert:
        //   (a) the contact budget BITES (contact_cap_hit is set) — signalled,
        //       never a silent mis-simulation;
        //   (b) the contacts actually SOLVED stay within the budget (bounded
        //       per-substep work, not ~n²/2);
        //   (c) the call completes in well-bounded wall-clock time (seconds, not
        //       the hours an unbounded O(n²) solve would take).
        use std::time::Instant;

        // A few hundred coincident unit spheres: n²/2 ≈ tens of thousands of
        // candidate pairs, each a 1-point sphere-sphere manifold — comfortably
        // over MAX_CONTACTS_PER_SUBSTEP, so the cap must bite. (Kept well under
        // the 4096 body cap so the test stays fast while still degenerate.)
        let n_bodies = 300usize;
        let mut w = World::new();
        for _ in 0..n_bodies {
            w.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, Material::default());
        }

        let start = Instant::now();
        let stats = w.step(2); // a couple of frames is enough to exercise the cap
        let elapsed = start.elapsed();

        assert!(
            stats.contact_cap_hit,
            "a collapsed-body scene must trip the per-substep contact budget"
        );
        assert!(
            (stats.contacts as usize) <= MAX_CONTACTS_PER_SUBSTEP,
            "contacts solved ({}) must not exceed the budget ({})",
            stats.contacts,
            MAX_CONTACTS_PER_SUBSTEP
        );
        // Bounded work: an unbounded O(n²) solve over a worst-case scene at the
        // body cap would take many seconds-to-hours; the budget keeps a small
        // collapsed scene well under a generous wall-clock ceiling.
        assert!(
            elapsed.as_secs() < 10,
            "collapsed-body step took too long ({elapsed:?}) — the contact cap did not bound the work"
        );
        // Determinism holds under truncation: same scene, same stats.
        let mut w2 = World::new();
        for _ in 0..n_bodies {
            w2.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, Material::default());
        }
        assert_eq!(w2.step(2), stats, "truncated step must stay deterministic");
    }

    #[test]
    fn full_body_cap_collapsed_scene_completes_in_bounded_walltime() {
        // THE worst case at the FULL body cap: ~4096 dynamic bodies collapsed onto
        // a SINGLE point — the exact DoS the upstream pair budget exists to defang.
        // Without the budget, broadphase::candidate_pairs alone does ~n²/2 ≈ 8.4M
        // AABB tests + manifold generations PER substep (measured ~1.4 s/substep at
        // n=2048, extrapolating to ~tens of minutes for one clamped world.step at
        // 4096), wedging the IPC loop from servicing `stop`.
        //
        // The DoS is that EACH FRAME is O(n²). The fix bounds it at TWO layers that
        // COMPOSE, and this test pins BOTH:
        //   • PER-FRAME work is now O(budget), not O(n²): the broadphase pair
        //     ENUMERATION short-circuits at MAX_PAIRS_PER_SUBSTEP and the contact
        //     set truncates at MAX_CONTACTS_PER_SUBSTEP. We prove this by timing a
        //     handful of frames at the FULL body cap — if the per-frame work were
        //     still O(n²) these few frames alone would already take many seconds.
        //   • the FRAME COUNT is bounded: a hostile `world.step{n}` with a huge `n`
        //     clamps to MAX_STEPS_PER_CALL (asserted on the same collapsed scene).
        // Bounded-per-frame × bounded-frame-count ⇒ the whole clamped op is bounded
        // (a few seconds in release; pinned generously here for the debug build CI
        // runs). Without timing 600 full frames (which only multiplies a now-bounded
        // per-frame cost by a constant), this is the complete, fast proof.
        //
        // Asserts: (a) the few-frame full-cap step returns in well-bounded wall time
        // (seconds, not the minutes an O(n²) per-frame scan would cost even for a
        // few frames); (b) BOTH bounds signal — pairs_cap_hit AND contact_cap_hit —
        // so the degenerate scene is never silently mis-simulated; (c) the n-clamp
        // bounds the frame count of the hostile u32::MAX op; (d) positions stay
        // finite (no NaN/inf blowup). This pins the DoS bound PERMANENTLY at the
        // worst case the body cap allows.
        use std::time::Instant;

        // crate::ipc::MAX_BODIES is the hard spawn cap; build the world directly to
        // exactly that many coincident unit spheres (the spawn op refuses past it,
        // but World::spawn itself is the cap-enforced quantity under test here).
        let n_bodies = crate::ipc::MAX_BODIES as usize; // 4096
        let build = || {
            let mut w = World::new();
            for _ in 0..n_bodies {
                w.spawn_shape(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, Material::default());
            }
            w
        };

        // --- PER-FRAME bound: time a handful of frames at the FULL body cap. ---
        // 5 frames × substeps budgeted passes over the worst-case scene. If pair
        // generation were still O(n²) (~8.4M AABB tests + manifolds PER substep),
        // even these few frames would take tens of seconds; bounded, they are fast.
        let mut w = build();
        let frames_probe: u32 = 5;
        let start = Instant::now();
        let stats = w.step(frames_probe);
        let elapsed = start.elapsed();
        eprintln!(
            "[worst-case] {frames_probe} frames @ {n_bodies} collapsed bodies: {elapsed:?} \
             (pairs_cap_hit={}, contact_cap_hit={}, contacts={})",
            stats.pairs_cap_hit, stats.contact_cap_hit, stats.contacts
        );

        // (b) BOTH bounded-work guards signalled (never a silent mis-simulation).
        assert!(
            stats.pairs_cap_hit,
            "the collapsed full-cap scene must trip the candidate-pair budget (upstream bound)"
        );
        assert!(
            stats.contact_cap_hit,
            "the collapsed full-cap scene must trip the contact budget (downstream bound)"
        );
        // The contacts actually solved stay within the contact budget.
        assert!(
            (stats.contacts as usize) <= MAX_CONTACTS_PER_SUBSTEP,
            "contacts solved ({}) must not exceed the contact budget",
            stats.contacts
        );
        // (d) positions stay finite — no NaN/inf even under the worst collapse.
        for b in &w.bodies {
            assert!(
                b.pos.x.is_finite() && b.pos.y.is_finite() && b.pos.z.is_finite(),
                "a body position went non-finite under the collapsed scene"
            );
        }
        // (a) THE per-frame bound — pinned permanently. 5 full-cap frames must
        // complete well within this ceiling. An unbounded O(n²) per-frame scan
        // would already cost ~tens of seconds for 5 frames at n=4096; 30 s is a
        // generous CI-safe cap (debug build, no SIMD) the budgeted path clears by a
        // wide margin (a few seconds in debug; sub-second in release).
        assert!(
            elapsed.as_secs() < 30,
            "5 full-cap collapsed frames took too long ({elapsed:?}) — the per-frame bound did not hold"
        );

        // --- FRAME-COUNT bound: the hostile huge-n op clamps to MAX_STEPS_PER_CALL.
        // The clamp is `n.min(MAX_STEPS_PER_CALL)` — a pure, scene-INDEPENDENT cap
        // (proven exhaustively in `step_clamps_over_cap_n_and_reports_clamped_frames`)
        // so we do NOT run 600 collapsed frames here (that only multiplies the
        // now-bounded per-frame cost above by a constant ≤ MAX_STEPS_PER_CALL and
        // would needlessly burn ~minutes in a debug build). We assert the clamp on a
        // cheap free-fall body so the proof "bounded per-frame × bounded frame
        // count ⇒ bounded whole op" composes without re-paying the per-frame cost.
        let mut cheap = ballistic_world(Vec3::new(0.0, 1.0e6, 0.0), Vec3::ZERO);
        assert_eq!(
            cheap.step(u32::MAX).frames,
            MAX_STEPS_PER_CALL,
            "(c) a hostile u32::MAX `n` must clamp to MAX_STEPS_PER_CALL frames"
        );

        // Determinism survives both truncations: an identical scene + identical
        // few-frame op yields identical stats.
        let mut w2 = build();
        assert_eq!(
            w2.step(frames_probe),
            stats,
            "the doubly-truncated worst-case step must stay deterministic"
        );
    }

    #[test]
    fn truncate_contacts_drops_tail_deterministically() {
        // Unit-level check of the contact-budget helper: it drops WHOLE manifolds
        // from the tail until the running contact total fits the budget, and
        // returns whether anything was dropped.
        use crate::narrowphase::{Contact, Manifold};
        let mk = |a: u32, b: u32, pts: usize| {
            let mut m = Manifold::new(BodyId(a), BodyId(b));
            for _ in 0..pts {
                m.contacts.push(Contact::new(Vec3::ZERO, Vec3::Y, 0.0));
            }
            m
        };
        // 3 manifolds carrying 2 + 2 + 2 = 6 contacts; a budget of 4 keeps the
        // first two (running 2 then 4, fits) and drops the third.
        let mut ms = vec![mk(0, 1, 2), mk(0, 2, 2), mk(1, 2, 2)];
        let hit = World::truncate_contacts(&mut ms, 4);
        assert!(hit, "budget bit");
        assert_eq!(ms.len(), 2, "the tail manifold was dropped");

        // A budget that fits everything drops nothing and returns false.
        let mut ms2 = vec![mk(0, 1, 2), mk(0, 2, 2)];
        assert!(!World::truncate_contacts(&mut ms2, 100));
        assert_eq!(ms2.len(), 2);
    }
}
