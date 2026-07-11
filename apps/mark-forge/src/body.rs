//! The rigid-body model (SPEC §2) — the FROZEN shared body types.
//!
//! This module is the CONTRACT. The integrator/broadphase/narrowphase/solver
//! all read and write [`Body`]; downstream agents must NOT change the field
//! layout, the [`Shape`] variants, or the [`Body::new`] spawn semantics (the
//! inertia/inverse-mass derivation is part of the determinism guarantee).

use serde::{Deserialize, Serialize};

use crate::math::{Mat3, Quat, Vec3};

/// Stable handle to a body — its index in [`crate::world::World::bodies`].
/// Insertion order is stable (a `Vec`), so the id is the fixed iteration key the
/// solver and telemetry use (SPEC §1 fixed iteration order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BodyId(pub u32);

impl BodyId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// A collision shape (SPEC §2). The three primitives the whole engine supports
/// — sphere, oriented box (cuboid), and an infinite static plane (half-space).
///
/// Serialized externally-tagged on `kind` (snake_case) so the wire form is
/// `{"kind":"sphere","radius":0.5}` / `{"kind":"cuboid","half_extents":[..]}` /
/// `{"kind":"plane","normal":[..],"offset":0.0}` — the shape an op carries and
/// the shape the `physics.scene`/`physics.bodies` telemetry echoes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Shape {
    /// A sphere of the given radius, centered on the body's `pos`.
    Sphere { radius: f64 },
    /// An oriented box; `half_extents` are half the side lengths along the
    /// body's local axes (rotated by the body's `orientation`).
    Cuboid { half_extents: Vec3 },
    /// A static infinite half-space: the set of points `p` with
    /// `normal · p <= offset`. `normal` is unit; the surface is at
    /// `normal · p == offset`. Always static (forced `inv_mass = 0`).
    Plane { normal: Vec3, offset: f64 },
}

impl Shape {
    /// Is this an inherently-static shape? A [`Shape::Plane`] is always static
    /// (the spawn path forces its inverse mass/inertia to zero, SPEC §2).
    #[inline]
    pub fn is_static_shape(&self) -> bool {
        matches!(self, Shape::Plane { .. })
    }

    /// The stable wire/telemetry tag for this shape's kind.
    #[inline]
    pub fn kind_str(&self) -> &'static str {
        match self {
            Shape::Sphere { .. } => "sphere",
            Shape::Cuboid { .. } => "cuboid",
            Shape::Plane { .. } => "plane",
        }
    }
}

/// Contact-surface material (SPEC §2/§6). Combined PAIRWISE at contact time:
/// restitution = `max(a, b)`, friction = `sqrt(a * b)` (fixed deterministic
/// rules; see [`Material::combine_restitution`]/[`combine_friction`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Material {
    /// Coefficient of restitution (bounciness), clamped sane on spawn to `0..=1`.
    pub restitution: f64,
    /// Coulomb friction coefficient μ (≥ 0).
    pub friction: f64,
}

impl Default for Material {
    /// A reasonable default surface: no bounce, moderate friction.
    fn default() -> Self {
        Material { restitution: 0.0, friction: 0.5 }
    }
}

impl Material {
    /// Pairwise-combined restitution for a contact between two materials
    /// (SPEC §6: `max(a, b)`).
    #[inline]
    pub fn combine_restitution(a: Material, b: Material) -> f64 {
        a.restitution.max(b.restitution)
    }

    /// Pairwise-combined friction for a contact between two materials
    /// (SPEC §6: `sqrt(a * b)`).
    #[inline]
    pub fn combine_friction(a: Material, b: Material) -> f64 {
        (a.friction * b.friction).sqrt()
    }
}

/// A single rigid body (SPEC §2). Position + orientation are the transform the
/// telemetry publishes; the velocities + inverse mass/inertia are the dynamic
/// state the integrator and solver evolve.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Body {
    /// World-space center of mass.
    pub pos: Vec3,
    /// Orientation as a unit quaternion (renormalized each step).
    pub orientation: Quat,
    /// World-space linear velocity.
    pub lin_vel: Vec3,
    /// World-space angular velocity.
    pub ang_vel: Vec3,
    /// `1 / mass`. `0.0` ⇒ static / infinite mass (planes, anchors): immovable.
    pub inv_mass: f64,
    /// Inverse inertia tensor in BODY space (SPEC §2). The solver rotates it to
    /// world each step via [`Mat3::rotated`]. [`Mat3::ZERO`] ⇒ no angular
    /// response (static body or a fully-locked rotation).
    pub inv_inertia: Mat3,
    /// Collision shape.
    pub shape: Shape,
    /// Contact-surface material.
    pub material: Material,
    /// Sleeping flag — a settled body the integrator may skip (SPEC §8 echoes it
    /// on `physics.bodies`). The Foundation defaults it `false`; a later agent may
    /// add the sleep heuristic without a contract change.
    pub sleeping: bool,
}

impl Body {
    /// Build a DYNAMIC body of finite `mass` with the given shape, deriving the
    /// inverse mass and the body-space inverse inertia tensor from the shape +
    /// mass (SPEC §2). A `mass <= 0` or a non-finite mass, or an inherently-
    /// static shape ([`Shape::Plane`]), yields a STATIC body (`inv_mass = 0`,
    /// `inv_inertia = ZERO`). Velocities start at zero; orientation at identity.
    ///
    /// This is the single spawn constructor `body.spawn` (SPEC §7) and the tests
    /// use, so the mass→inverse-inertia derivation lives in exactly one place.
    pub fn new(shape: Shape, pos: Vec3, mass: f64, material: Material) -> Body {
        // Enforce the material contract ON SPAWN (see the field docs + SPEC §2/§6):
        // restitution ∈ 0..=1, friction ≥ 0, both finite. Without this a hostile
        // or degenerate value survives to contact time and poisons the pairwise
        // combine: a negative friction makes `combine_friction = sqrt(a*b)` = NaN,
        // which crashes the solver's `f64::clamp(-max, max)` (assert min<=max fires
        // in release too); an unclamped restitution injects energy every bounce
        // until velocities/positions overflow to Inf (both break the SPEC §1
        // no-NaN/Inf invariant). clamp() alone can't be trusted — it returns NaN
        // for a NaN input — so map non-finite fields to the sane default first.
        let material = Material {
            restitution: if material.restitution.is_finite() {
                material.restitution.clamp(0.0, 1.0)
            } else {
                0.0
            },
            friction: if material.friction.is_finite() {
                material.friction.max(0.0)
            } else {
                0.0
            },
        };
        // DYNAMIC only when the reciprocal mass is guaranteed finite: a positive,
        // finite mass whose `1.0 / mass` does not overflow. A subnormal mass is
        // positive and finite yet `1.0 / mass` = +Inf, and an infinite mass has a
        // finite (0.0) reciprocal — so all three checks are needed. Everything else
        // (inherently-static shapes, mass <= 0, NaN, ±Inf, or an overflowing
        // reciprocal) is STATIC, so no infinite inv_mass reaches the integrator.
        let has_finite_inv_mass = mass > 0.0 && mass.is_finite() && (1.0 / mass).is_finite();
        let static_body = shape.is_static_shape() || !has_finite_inv_mass;
        let (inv_mass, inv_inertia) = if static_body {
            (0.0, Mat3::ZERO)
        } else {
            let tensor = match shape {
                Shape::Sphere { radius } => Mat3::solid_sphere_inertia(mass, radius),
                Shape::Cuboid { half_extents } => Mat3::solid_box_inertia(mass, half_extents),
                // Unreachable: a plane is always static above. Keep total.
                Shape::Plane { .. } => Mat3::ZERO,
            };
            (1.0 / mass, tensor.inverse_diagonal())
        };
        Body {
            pos,
            orientation: Quat::IDENTITY,
            lin_vel: Vec3::ZERO,
            ang_vel: Vec3::ZERO,
            inv_mass,
            inv_inertia,
            shape,
            material,
            sleeping: false,
        }
    }

    /// A static plane half-space at the given `normal`/`offset` (SPEC §2). The
    /// normal is normalized; the body is always static.
    pub fn plane(normal: Vec3, offset: f64) -> Body {
        Body::new(
            Shape::Plane { normal: normal.normalized(), offset },
            Vec3::ZERO,
            0.0,
            Material::default(),
        )
    }

    /// Is this body static (infinite mass)? Used by broadphase (skip
    /// static–static pairs) and the solver (only dynamic bodies get impulses).
    #[inline]
    pub fn is_static(&self) -> bool {
        self.inv_mass == 0.0
    }

    /// The body→world rotation matrix for the current orientation. The solver
    /// uses it to bring [`Body::inv_inertia`] into world space each step.
    #[inline]
    pub fn rotation(&self) -> Mat3 {
        self.orientation.to_mat3()
    }

    /// The world-space inverse inertia tensor this step: `R · I⁻¹_body · Rᵀ`
    /// (SPEC §3). [`Mat3::ZERO`] for a static body.
    #[inline]
    pub fn world_inv_inertia(&self) -> Mat3 {
        if self.is_static() {
            Mat3::ZERO
        } else {
            self.inv_inertia.rotated(self.rotation())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_body_has_finite_inverse_mass() {
        let b = Body::new(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 2.0, Material::default());
        assert_eq!(b.inv_mass, 0.5);
        assert!(!b.is_static());
        // 2/5 m r^2 = 0.8 -> inverse 1.25 on the diagonal.
        assert!((b.inv_inertia.cols[0].x - 1.25).abs() < 1e-12);
    }

    #[test]
    fn zero_mass_is_static() {
        let b = Body::new(Shape::Cuboid { half_extents: Vec3::splat(0.5) }, Vec3::ZERO, 0.0, Material::default());
        assert!(b.is_static());
        assert_eq!(b.inv_mass, 0.0);
        assert_eq!(b.inv_inertia, Mat3::ZERO);
    }

    #[test]
    fn plane_is_always_static_even_with_mass() {
        let b = Body::new(Shape::Plane { normal: Vec3::Y, offset: 0.0 }, Vec3::ZERO, 5.0, Material::default());
        assert!(b.is_static());
        assert_eq!(b.world_inv_inertia(), Mat3::ZERO);
    }

    #[test]
    fn shape_serializes_tagged() {
        let s = Shape::Sphere { radius: 0.5 };
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"kind":"sphere","radius":0.5}"#);
        let p: Shape = serde_json::from_str(r#"{"kind":"plane","normal":[0.0,1.0,0.0],"offset":0.0}"#).unwrap();
        assert!(matches!(p, Shape::Plane { .. }));
    }

    #[test]
    fn material_combines_pairwise() {
        let a = Material { restitution: 0.2, friction: 0.4 };
        let b = Material { restitution: 0.8, friction: 0.9 };
        assert_eq!(Material::combine_restitution(a, b), 0.8);
        assert!((Material::combine_friction(a, b) - (0.4f64 * 0.9).sqrt()).abs() < 1e-12);
    }

    #[test]
    fn negative_mass_falls_back_to_static() {
        let b = Body::new(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, -3.0, Material::default());
        assert!(b.is_static());
    }

    #[test]
    fn hostile_material_is_clamped_on_spawn() {
        // A negative friction makes combine_friction = sqrt(neg) = NaN and crashes
        // the solver's f64::clamp; a restitution > 1 injects energy every bounce.
        // Both must be clamped to their sane range ON SPAWN so they never reach the
        // solver.
        let hostile = Material { restitution: 50.0, friction: -1.0 };
        let b = Body::new(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, hostile);
        assert_eq!(b.material.restitution, 1.0, "restitution clamped to <= 1");
        assert_eq!(b.material.friction, 0.0, "negative friction clamped to 0");
        // The combined friction with any sane surface is now finite (no NaN).
        let mu = Material::combine_friction(b.material, Material::default());
        assert!(mu.is_finite(), "combined friction must be finite: {mu}");
    }

    #[test]
    fn nonfinite_material_falls_back_to_sane_defaults() {
        // f64::clamp returns NaN for a NaN input, so a NaN/Inf field must be mapped
        // to a sane default, not merely clamped.
        let poison = Material { restitution: f64::NAN, friction: f64::INFINITY };
        let b = Body::new(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, poison);
        assert!(b.material.restitution.is_finite() && (0.0..=1.0).contains(&b.material.restitution));
        assert!(b.material.friction.is_finite() && b.material.friction >= 0.0);
    }

    #[test]
    fn subnormal_mass_falls_back_to_static() {
        // A subnormal mass is > 0 and finite, but 1/mass overflows to +Inf; the body
        // must be treated as static so no infinite inv_mass/inv_inertia reaches the
        // integrator (SPEC §1).
        let b = Body::new(Shape::Sphere { radius: 0.5 }, Vec3::ZERO, 1e-320, Material::default());
        assert!(b.is_static(), "a mass whose reciprocal is +Inf is static");
        assert_eq!(b.inv_mass, 0.0);
        assert_eq!(b.inv_inertia, Mat3::ZERO);
    }
}
