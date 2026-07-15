//! Linear-algebra primitives — the deterministic f64 foundation (SPEC §1).
//!
//! This module is the CONTRACT. The integrator (`world`), broadphase,
//! narrowphase, and solver all build on these exact types; downstream agents
//! USE them and must NOT change the field layout, signatures, or numerics.
//!
//! Determinism (SPEC §1): everything here is `f64` and uses ONLY `+ - * /`,
//! `sqrt`, and a fixed-form quaternion normalize — no transcendental shortcuts
//! whose result varies by libm, no RNG, no wall-clock. Operations are written
//! in a fixed evaluation order so a build never reorders the float ops.

use serde::{Deserialize, Serialize};

// ===========================================================================
// Vec3
// ===========================================================================

/// A 3-component f64 vector — position, velocity, force, axis. Serializes as a
/// 3-element JSON array `[x, y, z]` so telemetry payloads stay compact (SPEC §8).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "[f64; 3]", into = "[f64; 3]")]
pub struct Vec3 {
    pub x: f64,
    pub y: f64,
    pub z: f64,
}

impl From<[f64; 3]> for Vec3 {
    fn from(a: [f64; 3]) -> Self {
        Vec3 { x: a[0], y: a[1], z: a[2] }
    }
}
impl From<Vec3> for [f64; 3] {
    fn from(v: Vec3) -> Self {
        [v.x, v.y, v.z]
    }
}

impl Vec3 {
    pub const ZERO: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
    pub const X: Vec3 = Vec3 { x: 1.0, y: 0.0, z: 0.0 };
    pub const Y: Vec3 = Vec3 { x: 0.0, y: 1.0, z: 0.0 };
    pub const Z: Vec3 = Vec3 { x: 0.0, y: 0.0, z: 1.0 };

    #[inline]
    pub const fn new(x: f64, y: f64, z: f64) -> Self {
        Vec3 { x, y, z }
    }

    /// Same value in every component.
    #[inline]
    pub const fn splat(v: f64) -> Self {
        Vec3 { x: v, y: v, z: v }
    }

    #[inline]
    pub fn dot(self, o: Vec3) -> f64 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    #[inline]
    pub fn cross(self, o: Vec3) -> Vec3 {
        Vec3 {
            x: self.y * o.z - self.z * o.y,
            y: self.z * o.x - self.x * o.z,
            z: self.x * o.y - self.y * o.x,
        }
    }

    /// Squared length — no `sqrt`, for cheap comparisons (broadphase, thresholds).
    #[inline]
    pub fn length_squared(self) -> f64 {
        self.dot(self)
    }

    #[inline]
    pub fn length(self) -> f64 {
        self.length_squared().sqrt()
    }

    /// Unit vector. Returns [`Vec3::ZERO`] for a (near-)zero vector rather than
    /// producing NaN/inf — deterministic and total (SPEC §1).
    #[inline]
    pub fn normalized(self) -> Vec3 {
        let len = self.length();
        if len > 0.0 {
            self * (1.0 / len)
        } else {
            Vec3::ZERO
        }
    }

    /// Component-wise minimum (AABB construction).
    #[inline]
    pub fn min(self, o: Vec3) -> Vec3 {
        Vec3 {
            x: self.x.min(o.x),
            y: self.y.min(o.y),
            z: self.z.min(o.z),
        }
    }

    /// Component-wise maximum (AABB construction).
    #[inline]
    pub fn max(self, o: Vec3) -> Vec3 {
        Vec3 {
            x: self.x.max(o.x),
            y: self.y.max(o.y),
            z: self.z.max(o.z),
        }
    }

    /// Component-wise absolute value (oriented-box AABB extent).
    #[inline]
    pub fn abs(self) -> Vec3 {
        Vec3 { x: self.x.abs(), y: self.y.abs(), z: self.z.abs() }
    }

    /// All components finite (not NaN/inf) — used by spawn validation.
    #[inline]
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl std::ops::Add for Vec3 {
    type Output = Vec3;
    #[inline]
    fn add(self, o: Vec3) -> Vec3 {
        Vec3 { x: self.x + o.x, y: self.y + o.y, z: self.z + o.z }
    }
}
impl std::ops::Sub for Vec3 {
    type Output = Vec3;
    #[inline]
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3 { x: self.x - o.x, y: self.y - o.y, z: self.z - o.z }
    }
}
impl std::ops::Neg for Vec3 {
    type Output = Vec3;
    #[inline]
    fn neg(self) -> Vec3 {
        Vec3 { x: -self.x, y: -self.y, z: -self.z }
    }
}
impl std::ops::Mul<f64> for Vec3 {
    type Output = Vec3;
    #[inline]
    fn mul(self, s: f64) -> Vec3 {
        Vec3 { x: self.x * s, y: self.y * s, z: self.z * s }
    }
}
impl std::ops::Mul<Vec3> for f64 {
    type Output = Vec3;
    #[inline]
    fn mul(self, v: Vec3) -> Vec3 {
        v * self
    }
}
impl std::ops::AddAssign for Vec3 {
    #[inline]
    fn add_assign(&mut self, o: Vec3) {
        *self = *self + o;
    }
}
impl std::ops::SubAssign for Vec3 {
    #[inline]
    fn sub_assign(&mut self, o: Vec3) {
        *self = *self - o;
    }
}

// ===========================================================================
// Quat (unit quaternion orientation)
// ===========================================================================

/// A quaternion orientation `(x, y, z, w)` (vector-first storage; `w` is the
/// scalar part). Serializes as `[x, y, z, w]` (SPEC §8 — the R3F panel feeds
/// this straight into `THREE.Quaternion`). The engine keeps it unit-length by
/// renormalizing each step (SPEC §3).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "[f64; 4]", into = "[f64; 4]")]
pub struct Quat {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub w: f64,
}

impl From<[f64; 4]> for Quat {
    fn from(a: [f64; 4]) -> Self {
        Quat { x: a[0], y: a[1], z: a[2], w: a[3] }
    }
}
impl From<Quat> for [f64; 4] {
    fn from(q: Quat) -> Self {
        [q.x, q.y, q.z, q.w]
    }
}

impl Quat {
    /// The identity rotation `(0, 0, 0, 1)`.
    pub const IDENTITY: Quat = Quat { x: 0.0, y: 0.0, z: 0.0, w: 1.0 };

    #[inline]
    pub const fn new(x: f64, y: f64, z: f64, w: f64) -> Self {
        Quat { x, y, z, w }
    }

    /// Build a unit quaternion from an axis + angle (radians). `sin`/`cos` are
    /// the ONLY transcendentals in the math module and appear only on this
    /// construction path (used by spawn/tests, never inside the integrator loop,
    /// so the per-step trace stays free of libm-variance — SPEC §1).
    ///
    /// A degenerate (zero / non-finite) axis has no rotation direction, so it
    /// collapses to [`Quat::IDENTITY`] (a no-op rotation) rather than producing
    /// the non-unit `(0,0,0,cos θ/2)` that scales vectors — total and
    /// deterministic (SPEC §1, no garbage from a pathological input). Every
    /// non-zero axis takes the unchanged `sin`/`cos` path, bit-for-bit.
    #[inline]
    pub fn from_axis_angle(axis: Vec3, angle: f64) -> Quat {
        let a = axis.normalized();
        // `Vec3::normalized` returns ZERO for a zero/degenerate axis; with no
        // axis there is no rotation — return the identity.
        if a == Vec3::ZERO {
            return Quat::IDENTITY;
        }
        let half = angle * 0.5;
        let s = half.sin();
        Quat { x: a.x * s, y: a.y * s, z: a.z * s, w: half.cos() }
    }

    /// Hamilton product `self * o` (apply `o` then `self`).
    // `Quat` is a FROZEN type with a frozen inherent API (see module docs); this
    // is a named inherent method callers invoke as `q.mul(other)`, deliberately
    // NOT a `std::ops::Mul` impl, so keep it inherent rather than a trait.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn mul(self, o: Quat) -> Quat {
        Quat {
            w: self.w * o.w - self.x * o.x - self.y * o.y - self.z * o.z,
            x: self.w * o.x + self.x * o.w + self.y * o.z - self.z * o.y,
            y: self.w * o.y - self.x * o.z + self.y * o.w + self.z * o.x,
            z: self.w * o.z + self.x * o.y - self.y * o.x + self.z * o.w,
        }
    }

    /// Renormalize to unit length (called each step on the integrated
    /// orientation). Returns [`Quat::IDENTITY`] if the quaternion collapses to
    /// zero — total and deterministic (SPEC §1/§3).
    ///
    /// The common, in-range path computes the norm directly from the raw
    /// components (bit-identical to a naive `(Σ cᵢ²).sqrt()` normalize, so the
    /// integrator's per-step renormalize and the golden traces are unchanged).
    /// Only when that direct norm is non-finite — i.e. a component is large
    /// enough that `cᵢ²` overflows `f64` to `inf` (e.g. a pathological spawn
    /// `ang_vel`) — do we fall back to a scaled computation that divides out the
    /// largest-magnitude component first, so the squares stay in range and the
    /// result is still a true unit quaternion rather than collapsing to zero
    /// (SPEC §1 — total, no NaN/inf propagation).
    #[inline]
    pub fn normalized(self) -> Quat {
        let n = (self.x * self.x + self.y * self.y + self.z * self.z + self.w * self.w).sqrt();
        if n > 0.0 && n.is_finite() {
            let inv = 1.0 / n;
            Quat { x: self.x * inv, y: self.y * inv, z: self.z * inv, w: self.w * inv }
        } else if n.is_finite() {
            // n == 0.0 (or sub-normal underflow to zero): the quaternion has no
            // length to normalize — collapse to the identity rotation.
            Quat::IDENTITY
        } else {
            // Overflow: `Σ cᵢ²` saturated to `inf`. Scale by the largest |component|
            // so the squares are O(1), then normalize the scaled quaternion (the
            // scale cancels, so the direction is preserved exactly).
            let m = self
                .x
                .abs()
                .max(self.y.abs())
                .max(self.z.abs())
                .max(self.w.abs());
            if m > 0.0 && m.is_finite() {
                let inv_m = 1.0 / m;
                let (x, y, z, w) = (
                    self.x * inv_m,
                    self.y * inv_m,
                    self.z * inv_m,
                    self.w * inv_m,
                );
                let n2 = (x * x + y * y + z * z + w * w).sqrt();
                if n2 > 0.0 && n2.is_finite() {
                    let inv = 1.0 / n2;
                    Quat { x: x * inv, y: y * inv, z: z * inv, w: w * inv }
                } else {
                    Quat::IDENTITY
                }
            } else {
                // The largest component is itself non-finite (a NaN/inf slipped
                // in): there is no meaningful direction — identity, never NaN.
                Quat::IDENTITY
            }
        }
    }

    /// The conjugate (inverse for a unit quaternion).
    #[inline]
    pub fn conjugate(self) -> Quat {
        Quat { x: -self.x, y: -self.y, z: -self.z, w: self.w }
    }

    /// Rotate a vector by this orientation: `q * v * q⁻¹`, expanded to the
    /// standard branch-free form.
    #[inline]
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let u = Vec3 { x: self.x, y: self.y, z: self.z };
        let two_dot = 2.0 * u.dot(v);
        let w_sq_m = self.w * self.w - u.dot(u);
        u * two_dot + v * w_sq_m + u.cross(v) * (2.0 * self.w)
    }

    /// The 3×3 rotation matrix for this orientation (used by the solver to take
    /// the body-space inverse inertia tensor into world space, `R · I⁻¹ · Rᵀ`).
    #[inline]
    pub fn to_mat3(self) -> Mat3 {
        let Quat { x, y, z, w } = self.normalized();
        let (xx, yy, zz) = (x * x, y * y, z * z);
        let (xy, xz, yz) = (x * y, x * z, y * z);
        let (wx, wy, wz) = (w * x, w * y, w * z);
        // Row-major; see Mat3 storage note.
        Mat3 {
            cols: [
                Vec3::new(1.0 - 2.0 * (yy + zz), 2.0 * (xy + wz), 2.0 * (xz - wy)),
                Vec3::new(2.0 * (xy - wz), 1.0 - 2.0 * (xx + zz), 2.0 * (yz + wx)),
                Vec3::new(2.0 * (xz + wy), 2.0 * (yz - wx), 1.0 - 2.0 * (xx + yy)),
            ],
        }
    }
}

// ===========================================================================
// Mat3 (3×3 — inertia tensors + rotation)
// ===========================================================================

/// A column-major 3×3 f64 matrix: `cols[i]` is the i-th basis-vector image, so
/// `m * v = cols[0]*v.x + cols[1]*v.y + cols[2]*v.z`. Used for inertia tensors
/// (and their inverses) and the body→world rotation. NOT serialized — it never
/// crosses the wire (transforms are sent as pos+quat); it is pure engine state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mat3 {
    /// The three columns. `cols[0]`, `cols[1]`, `cols[2]`.
    pub cols: [Vec3; 3],
}

impl Mat3 {
    /// The all-zero matrix — an `inv_inertia` of `ZERO` means "no angular
    /// response" (static body / locked rotation).
    pub const ZERO: Mat3 = Mat3 {
        cols: [Vec3::ZERO, Vec3::ZERO, Vec3::ZERO],
    };

    /// The identity matrix.
    pub const IDENTITY: Mat3 = Mat3 {
        cols: [Vec3::X, Vec3::Y, Vec3::Z],
    };

    /// A diagonal matrix from three diagonal entries (a principal-axes inertia
    /// tensor is diagonal in body space).
    #[inline]
    pub const fn diagonal(d: Vec3) -> Mat3 {
        Mat3 {
            cols: [
                Vec3::new(d.x, 0.0, 0.0),
                Vec3::new(0.0, d.y, 0.0),
                Vec3::new(0.0, 0.0, d.z),
            ],
        }
    }

    /// Matrix·vector.
    #[inline]
    pub fn mul_vec3(self, v: Vec3) -> Vec3 {
        self.cols[0] * v.x + self.cols[1] * v.y + self.cols[2] * v.z
    }

    /// Matrix·matrix (`self` applied after `o`).
    #[inline]
    pub fn mul_mat3(self, o: Mat3) -> Mat3 {
        Mat3 {
            cols: [
                self.mul_vec3(o.cols[0]),
                self.mul_vec3(o.cols[1]),
                self.mul_vec3(o.cols[2]),
            ],
        }
    }

    /// The transpose.
    #[inline]
    pub fn transpose(self) -> Mat3 {
        let c = &self.cols;
        Mat3 {
            cols: [
                Vec3::new(c[0].x, c[1].x, c[2].x),
                Vec3::new(c[0].y, c[1].y, c[2].y),
                Vec3::new(c[0].z, c[1].z, c[2].z),
            ],
        }
    }

    /// Take a body-space inertia (inverse) tensor into world space:
    /// `R · M · Rᵀ`. The solver calls this each step with the body's current
    /// rotation matrix so angular impulses act in world space (SPEC §2/§3).
    #[inline]
    pub fn rotated(self, r: Mat3) -> Mat3 {
        r.mul_mat3(self).mul_mat3(r.transpose())
    }

    // ---- inertia helpers (SPEC §2) ----------------------------------------

    /// Body-space inertia tensor of a SOLID sphere of `mass` and `radius`:
    /// `I = (2/5) m r²` on every axis. Returns the (diagonal) tensor itself —
    /// the spawn path inverts it (a zero/infinite mass ⇒ zero inverse).
    #[inline]
    pub fn solid_sphere_inertia(mass: f64, radius: f64) -> Mat3 {
        let i = 0.4 * mass * radius * radius;
        Mat3::diagonal(Vec3::splat(i))
    }

    /// Body-space inertia tensor of a SOLID box with the given `half_extents`
    /// (so full side lengths are `2*half_extents`): the standard
    /// `I_x = (1/12) m (h_y² + h_z²)` with `h = 2*half_extent`. Returns the
    /// (diagonal) tensor; the spawn path inverts it.
    #[inline]
    pub fn solid_box_inertia(mass: f64, half_extents: Vec3) -> Mat3 {
        // Full side lengths.
        let w = 2.0 * half_extents.x;
        let h = 2.0 * half_extents.y;
        let d = 2.0 * half_extents.z;
        let k = mass / 12.0;
        Mat3::diagonal(Vec3::new(
            k * (h * h + d * d),
            k * (w * w + d * d),
            k * (w * w + h * h),
        ))
    }

    /// Inverse of a DIAGONAL matrix (every inertia tensor we build is diagonal
    /// in body space; this is all the inversion the engine needs). A zero
    /// diagonal entry inverts to `0.0` (locked axis / infinite inertia) rather
    /// than `inf` — keeps a static body's inverse tensor [`Mat3::ZERO`] and the
    /// math total/deterministic (SPEC §1).
    #[inline]
    pub fn inverse_diagonal(self) -> Mat3 {
        #[inline]
        fn inv(v: f64) -> f64 {
            if v != 0.0 && v.is_finite() {
                1.0 / v
            } else {
                0.0
            }
        }
        Mat3::diagonal(Vec3::new(
            inv(self.cols[0].x),
            inv(self.cols[1].y),
            inv(self.cols[2].z),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec3_ops() {
        let a = Vec3::new(1.0, 2.0, 3.0);
        let b = Vec3::new(4.0, 5.0, 6.0);
        assert_eq!(a + b, Vec3::new(5.0, 7.0, 9.0));
        assert_eq!(b - a, Vec3::new(3.0, 3.0, 3.0));
        assert_eq!(a * 2.0, Vec3::new(2.0, 4.0, 6.0));
        assert_eq!(2.0 * a, Vec3::new(2.0, 4.0, 6.0));
        assert_eq!(a.dot(b), 32.0);
        assert_eq!(Vec3::X.cross(Vec3::Y), Vec3::Z);
        assert_eq!(Vec3::ZERO.normalized(), Vec3::ZERO);
    }

    #[test]
    fn vec3_serializes_as_array() {
        let v = Vec3::new(1.0, -2.0, 3.5);
        assert_eq!(serde_json::to_string(&v).unwrap(), "[1.0,-2.0,3.5]");
        assert_eq!(serde_json::from_str::<Vec3>("[1.0,-2.0,3.5]").unwrap(), v);
    }

    #[test]
    fn quat_identity_rotates_to_self() {
        let v = Vec3::new(1.0, 2.0, 3.0);
        assert_eq!(Quat::IDENTITY.rotate(v), v);
    }

    #[test]
    fn quat_axis_angle_90deg_about_z() {
        let q = Quat::from_axis_angle(Vec3::Z, std::f64::consts::FRAC_PI_2);
        let r = q.rotate(Vec3::X);
        assert!((r.x - 0.0).abs() < 1e-12, "x={}", r.x);
        assert!((r.y - 1.0).abs() < 1e-12, "y={}", r.y);
        assert!((r.z - 0.0).abs() < 1e-12, "z={}", r.z);
    }

    #[test]
    fn quat_serializes_xyzw() {
        let q = Quat::new(0.0, 0.0, 0.0, 1.0);
        assert_eq!(serde_json::to_string(&q).unwrap(), "[0.0,0.0,0.0,1.0]");
    }

    #[test]
    fn quat_normalize_collapses_to_identity() {
        assert_eq!(Quat::new(0.0, 0.0, 0.0, 0.0).normalized(), Quat::IDENTITY);
    }

    #[test]
    fn mat3_identity_mul() {
        let v = Vec3::new(3.0, -1.0, 2.0);
        assert_eq!(Mat3::IDENTITY.mul_vec3(v), v);
    }

    #[test]
    fn box_inertia_inverts_to_zero_for_zero_mass() {
        let tensor = Mat3::solid_box_inertia(0.0, Vec3::splat(0.5));
        assert_eq!(tensor.inverse_diagonal(), Mat3::ZERO);
    }

    #[test]
    fn sphere_inertia_value() {
        // I = 2/5 m r^2 = 0.4 * 2 * 9 = 7.2 on each axis (within f64 rounding).
        let t = Mat3::solid_sphere_inertia(2.0, 3.0);
        assert!((t.cols[0].x - 7.2).abs() < 1e-12);
        assert!((t.cols[1].y - 7.2).abs() < 1e-12);
        assert!((t.cols[2].z - 7.2).abs() < 1e-12);
    }

    #[test]
    fn diagonal_inverse_roundtrip() {
        let t = Mat3::diagonal(Vec3::new(2.0, 4.0, 8.0));
        let inv = t.inverse_diagonal();
        assert_eq!(inv.cols[0].x, 0.5);
        assert_eq!(inv.cols[1].y, 0.25);
        assert_eq!(inv.cols[2].z, 0.125);
    }
}
