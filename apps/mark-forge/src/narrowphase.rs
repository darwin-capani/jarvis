//! Narrowphase (SPEC §5) — exact contact generation per candidate pair.
//!
//! Owns the FROZEN [`Contact`] / [`Manifold`] types + the [`collide`] contract.
//! The per-primitive tests (sphere–sphere, sphere–plane, box–plane, box–box via
//! SAT) are filled here against the frozen signatures. The SOLVER reads
//! [`Manifold`]/[`Contact`] verbatim, so these types are NOT changed.
//!
//! Determinism (SPEC §1/§5): contact ordering within a manifold is fixed (by the
//! generating feature index); no RNG, no float-identity branching. The normal of
//! every contact points from body A toward body B (A/B = the broadphase
//! [`Pair`]'s ascending id order), and `penetration >= 0`.
//!
//! Geometry implemented:
//!   - sphere–sphere : center distance vs radius sum.
//!   - sphere–plane  : signed distance of the center to the half-space surface.
//!   - box–plane     : the box's 8 oriented corners vs the half-space; every
//!     penetrating corner becomes a contact (a resting box on a
//!     plane yields its 4 face corners — a stable manifold).
//!   - box–box (SAT) : separating-axis test over the 6 face normals (3 per box)
//!     and the 9 edge–edge cross axes; the minimum-penetration
//!     axis is the contact normal. Face contacts are produced by
//!     reference/incident face clipping (Sutherland–Hodgman);
//!     edge contacts by the closest points of the two edges.

use crate::body::{Body, BodyId, Shape};
use crate::broadphase::Pair;
use crate::math::Vec3;
use crate::world::World;

/// A single contact point between two bodies (SPEC §5). `normal` is the unit
/// contact normal pointing from body A toward body B; `penetration` is the
/// overlap depth (≥ 0; the amount A and B interpenetrate along `normal`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Contact {
    /// World-space contact point (on the contact surface, SPEC §5).
    pub point: Vec3,
    /// Unit contact normal, pointing from A to B.
    pub normal: Vec3,
    /// Penetration depth (≥ 0). The position-correction pass resolves this
    /// beyond [`crate::world::SolverParams::penetration_slop`].
    pub penetration: f64,
    /// Accumulated normal impulse across solver iterations — kept on the contact
    /// so a later agent can add warm-starting without a contract change (SPEC §6).
    /// The Foundation/narrowphase initializes it to `0.0`; the solver accumulates.
    pub normal_impulse: f64,
    /// Accumulated tangent (friction) impulse magnitude, same warm-start purpose.
    pub tangent_impulse: f64,
}

impl Contact {
    /// A fresh contact with zeroed accumulated impulses (the narrowphase
    /// constructor — the solver fills the impulses).
    #[inline]
    pub fn new(point: Vec3, normal: Vec3, penetration: f64) -> Contact {
        Contact {
            point,
            normal,
            penetration,
            normal_impulse: 0.0,
            tangent_impulse: 0.0,
        }
    }
}

/// The set of contact points between one body pair (SPEC §5). A box-on-plane
/// rest produces up to 4 contacts (a stable face); a sphere produces 1. `a`/`b`
/// are the [`BodyId`]s the [`Contact::normal`] runs A→B between (same order as
/// the broadphase [`Pair`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Manifold {
    /// Body A — the `normal` points away from this body, toward B.
    pub a: BodyId,
    /// Body B.
    pub b: BodyId,
    /// 1..=N contact points, in a deterministic feature order (SPEC §5). A
    /// manifold is never produced empty (a non-colliding pair yields `None` from
    /// [`collide`]).
    pub contacts: Vec<Contact>,
}

impl Manifold {
    /// Start an empty manifold for an ordered body pair (`a < b`). The
    /// narrowphase pushes contacts; a manifold with no contacts is dropped
    /// (returned as `None` from [`collide`]).
    #[inline]
    pub fn new(a: BodyId, b: BodyId) -> Manifold {
        Manifold { a, b, contacts: Vec::new() }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.contacts.is_empty()
    }
}

/// Resolve one broadphase [`Pair`] into a [`Manifold`], or `None` if the bodies
/// are not actually touching (SPEC §5). Dispatches on the two shapes:
/// sphere–sphere, sphere–plane, box–plane, box–box (SAT), in either argument
/// order (the result's `a`/`b` follow the pair's ascending id order so the
/// normal direction is well-defined).
pub fn collide(world: &World, pair: Pair) -> Option<Manifold> {
    let Pair(id_a, id_b) = pair;
    // A pair from a well-formed broadphase has a < b and both ids exist; be
    // total about a missing body or a degenerate (a == b) pair rather than
    // panic — keeps the narrowphase a pure, total function (SPEC §1).
    if id_a == id_b {
        return None;
    }
    let body_a = world.body(id_a)?;
    let body_b = world.body(id_b)?;

    // Combined material lives on the manifold's contacts implicitly via the
    // solver, but contact generation itself is purely geometric. We hand the
    // ordered (A, B) bodies to each primitive routine; every routine emits the
    // normal pointing A->B so the solver's impulse sign is consistent.
    let contacts = collide_shapes(body_a, body_b);
    if contacts.is_empty() {
        return None;
    }
    Some(Manifold { a: id_a, b: id_b, contacts })
}

/// Collide every broadphase pair, returning all non-empty manifolds in the
/// broadphase's deterministic pair order (SPEC §5). This is the thin driver the
/// integrator calls between broadphase and the solver.
pub fn collide_all(world: &World, pairs: &[Pair]) -> Vec<Manifold> {
    pairs.iter().filter_map(|&p| collide(world, p)).collect()
}

// ===========================================================================
// Shape dispatch — normal always points A -> B.
// ===========================================================================

/// Tiny tolerance so a contact that is *exactly* touching (zero penetration,
/// surfaces coincident) still registers as a contact rather than being lost to
/// strict `<` comparisons. Generous enough to admit resting contacts, tight
/// enough not to invent contacts between clearly separated bodies.
const CONTACT_EPS: f64 = 1.0e-9;

/// Dispatch contact generation on the two ordered bodies' shapes. The returned
/// contacts have their normal pointing from `a` toward `b`. Every branch covers
/// the two argument orders so the (A, B) ordering imposed by the broadphase pair
/// is respected without the caller pre-sorting by shape kind.
fn collide_shapes(a: &Body, b: &Body) -> Vec<Contact> {
    match (a.shape, b.shape) {
        (Shape::Sphere { radius: ra }, Shape::Sphere { radius: rb }) => {
            sphere_sphere(a.pos, ra, b.pos, rb)
        }

        // sphere vs plane (either order). Normal must point A->B.
        (Shape::Sphere { radius }, Shape::Plane { normal, offset }) => {
            // A = sphere, B = plane. Plane surface pushes the sphere out along
            // -normal; the A->B normal therefore points INTO the plane (= +plane
            // normal flips: from sphere toward plane).
            sphere_plane(a.pos, radius, normal, offset, /*sphere_is_a=*/ true)
        }
        (Shape::Plane { normal, offset }, Shape::Sphere { radius }) => {
            sphere_plane(b.pos, radius, normal, offset, /*sphere_is_a=*/ false)
        }

        // box vs plane (either order).
        (Shape::Cuboid { half_extents }, Shape::Plane { normal, offset }) => {
            box_plane(a, half_extents, normal, offset, /*box_is_a=*/ true)
        }
        (Shape::Plane { normal, offset }, Shape::Cuboid { half_extents }) => {
            box_plane(b, half_extents, normal, offset, /*box_is_a=*/ false)
        }

        // sphere vs box (either order).
        (Shape::Sphere { radius }, Shape::Cuboid { half_extents }) => {
            sphere_box(a.pos, radius, b, half_extents, /*sphere_is_a=*/ true)
        }
        (Shape::Cuboid { half_extents }, Shape::Sphere { radius }) => {
            sphere_box(b.pos, radius, a, half_extents, /*sphere_is_a=*/ false)
        }

        // box vs box.
        (Shape::Cuboid { half_extents: ha }, Shape::Cuboid { half_extents: hb }) => {
            box_box(a, ha, b, hb)
        }

        // plane vs plane: two infinite static half-spaces never collide for our
        // purposes (and the broadphase skips static-static pairs anyway).
        (Shape::Plane { .. }, Shape::Plane { .. }) => Vec::new(),
    }
}

// ===========================================================================
// Sphere–sphere
// ===========================================================================

/// Two spheres collide when the distance between centers is less than the sum of
/// radii. The normal points from A's center toward B's center; the contact point
/// sits on the midline of the overlap region.
fn sphere_sphere(pos_a: Vec3, ra: f64, pos_b: Vec3, rb: f64) -> Vec<Contact> {
    let delta = pos_b - pos_a; // A -> B
    let dist_sq = delta.length_squared();
    let radius_sum = ra + rb;
    if dist_sq > (radius_sum + CONTACT_EPS) * (radius_sum + CONTACT_EPS) {
        return Vec::new();
    }
    let dist = dist_sq.sqrt();
    // Degenerate coincident centers: pick a stable canonical normal (+Y is the
    // gravity-up axis; deterministic and non-NaN) rather than dividing by zero.
    let normal = if dist > CONTACT_EPS {
        delta * (1.0 / dist)
    } else {
        Vec3::Y
    };
    let penetration = (radius_sum - dist).max(0.0);
    // Contact point: on A's surface advanced toward B, placed at the midpoint of
    // the overlapped surfaces for a symmetric resolution.
    let surface_a = pos_a + normal * ra;
    let surface_b = pos_b - normal * rb;
    let point = (surface_a + surface_b) * 0.5;
    vec![Contact::new(point, normal, penetration)]
}

// ===========================================================================
// Sphere–plane
// ===========================================================================

/// A sphere vs an infinite half-space `{ p : normal·p <= offset }`. The plane's
/// surface is at `normal·p == offset`; the solid region is on the `-normal`
/// side. The sphere penetrates when its center is within `radius` of the
/// surface (on either side, so a sphere whose center has crossed the surface
/// still collides).
///
/// `sphere_is_a` selects the A→B normal direction: when the sphere is body A,
/// the A→B normal points from the sphere INTO the plane (= `-plane_normal`);
/// when the plane is body A, it points from the plane toward the sphere
/// (= `+plane_normal`).
fn sphere_plane(
    sphere_pos: Vec3,
    radius: f64,
    plane_normal: Vec3,
    offset: f64,
    sphere_is_a: bool,
) -> Vec<Contact> {
    let n = plane_normal.normalized();
    // Signed distance of the sphere center to the surface, positive on the
    // outside (+normal) half-space.
    let signed = n.dot(sphere_pos) - offset;
    let penetration = radius - signed;
    if penetration < -CONTACT_EPS {
        // Sphere fully outside the surface by more than its radius — no contact.
        return Vec::new();
    }
    let penetration = penetration.max(0.0);
    // The deepest point of the sphere along -normal (the point pressed into the
    // plane). Contact point reported on the plane surface for stability.
    let deepest = sphere_pos - n * radius;
    let point = deepest + n * penetration.max(0.0) * 0.5; // midway into overlap
    // Plane->sphere direction is +n (pushes sphere out along +n). A->B normal:
    let normal = if sphere_is_a { -n } else { n };
    vec![Contact::new(point, normal, penetration)]
}

// ===========================================================================
// Box–plane
// ===========================================================================

/// The 8 world-space corners of an oriented box. The columns of the rotation
/// matrix are the box's local axes in world space; each corner is the center
/// plus the signed half-extent along each axis.
fn box_corners(body: &Body, half: Vec3) -> [Vec3; 8] {
    let r = body.rotation();
    let ax = r.cols[0] * half.x;
    let ay = r.cols[1] * half.y;
    let az = r.cols[2] * half.z;
    let c = body.pos;
    // Fixed sign order so the corner list is deterministic.
    [
        c - ax - ay - az,
        c + ax - ay - az,
        c - ax + ay - az,
        c + ax + ay - az,
        c - ax - ay + az,
        c + ax - ay + az,
        c - ax + ay + az,
        c + ax + ay + az,
    ]
}

/// An oriented box vs an infinite half-space. Every corner that has crossed (or
/// touches) the plane surface becomes a contact; a box resting flat on the plane
/// therefore yields its 4 bottom corners — a stable rectangular manifold.
///
/// `box_is_a` selects the A→B normal: box-A → normal points box-INTO-plane
/// (`-plane_normal`); plane-A → normal points plane-toward-box (`+plane_normal`).
fn box_plane(
    body: &Body,
    half: Vec3,
    plane_normal: Vec3,
    offset: f64,
    box_is_a: bool,
) -> Vec<Contact> {
    let n = plane_normal.normalized();
    let corners = box_corners(body, half);
    let mut contacts = Vec::new();
    // Iterate corners in fixed order; emit a contact for every penetrating one.
    for &corner in corners.iter() {
        let signed = n.dot(corner) - offset; // >0 outside surface, <0 inside solid
        let penetration = -signed; // depth past the surface into the half-space
        if penetration >= -CONTACT_EPS {
            let pen = penetration.max(0.0);
            // Report the contact point on the plane surface, directly above the
            // corner along +n.
            let point = corner + n * (pen * 0.5);
            let normal = if box_is_a { -n } else { n };
            contacts.push(Contact::new(point, normal, pen));
        }
    }
    contacts
}

// ===========================================================================
// Sphere–box
// ===========================================================================

/// A sphere vs an oriented box. Work in the box's local frame: the closest point
/// on the box to the sphere center is the center clamped to the box half-extents
/// per axis. The sphere collides when that closest point is within `radius`.
///
/// `sphere_is_a` selects the A→B normal direction (sphere-A → sphere-toward-box;
/// box-A → box-toward-sphere).
fn sphere_box(
    sphere_pos: Vec3,
    radius: f64,
    box_body: &Body,
    half: Vec3,
    sphere_is_a: bool,
) -> Vec<Contact> {
    let r = box_body.rotation();
    let rt = r.transpose(); // world -> body (rotation is orthonormal)
    // Sphere center in the box's local frame.
    let rel = sphere_pos - box_body.pos;
    let local = rt.mul_vec3(rel);

    // Closest point on the box (in local space) = clamp per axis.
    let clamped = Vec3::new(
        local.x.max(-half.x).min(half.x),
        local.y.max(-half.y).min(half.y),
        local.z.max(-half.z).min(half.z),
    );

    let to_center_local = local - clamped; // box-surface -> sphere-center (local)
    let dist_sq = to_center_local.length_squared();

    if dist_sq > (radius + CONTACT_EPS) * (radius + CONTACT_EPS) {
        // Center is outside and farther than radius: no contact.
        return Vec::new();
    }

    let (normal_box_to_sphere_local, penetration, contact_local);
    if dist_sq > CONTACT_EPS * CONTACT_EPS {
        // Center outside (or on) the box: normal is the (local) direction from
        // the closest surface point to the sphere center.
        let dist = dist_sq.sqrt();
        let dir = to_center_local * (1.0 / dist);
        normal_box_to_sphere_local = dir;
        penetration = (radius - dist).max(0.0);
        contact_local = clamped;
    } else {
        // Center is INSIDE the box: pick the axis with the least penetration to
        // the nearest face and push out along it (deepest-axis / face exit).
        let dx = half.x - local.x.abs();
        let dy = half.y - local.y.abs();
        let dz = half.z - local.z.abs();
        // Smallest of the three -> the face we exit through.
        let (axis_dir, depth) = if dx <= dy && dx <= dz {
            (
                Vec3::new(local.x.signum_nonzero(), 0.0, 0.0),
                dx,
            )
        } else if dy <= dz {
            (Vec3::new(0.0, local.y.signum_nonzero(), 0.0), dy)
        } else {
            (Vec3::new(0.0, 0.0, local.z.signum_nonzero()), dz)
        };
        normal_box_to_sphere_local = axis_dir;
        penetration = depth + radius;
        // Contact point on the exited face.
        contact_local = clamped;
    }

    // Bring the contact + normal back to world space.
    let normal_box_to_sphere = r.mul_vec3(normal_box_to_sphere_local).normalized();
    let contact_world = box_body.pos + r.mul_vec3(contact_local);

    // A->B normal: sphere-A means A=sphere, normal points A(sphere)->B(box) =
    // -(box->sphere); box-A means A=box -> normal points box->sphere.
    let normal = if sphere_is_a {
        -normal_box_to_sphere
    } else {
        normal_box_to_sphere
    };
    vec![Contact::new(contact_world, normal, penetration)]
}

// ===========================================================================
// Box–box (SAT)
// ===========================================================================

/// Result of the separating-axis search: the minimum-penetration axis (already
/// oriented A->B), the penetration depth on that axis, and which kind of feature
/// produced it (a face of A, a face of B, or an edge-edge cross).
struct SatResult {
    normal: Vec3, // unit, A -> B
    penetration: f64,
    // 0..3 = face of A (axis index), 3..6 = face of B, 6.. = edge cross index.
    code: SatFeature,
}

/// Which feature produced the minimum-penetration axis. The face variants don't
/// carry the axis index — the face-contact path recomputes the reference face
/// from the chosen normal — but the edge variant carries the two box edge axes
/// whose cross was the separating axis, since the edge-contact path needs them.
#[derive(Clone, Copy)]
enum SatFeature {
    FaceA,
    FaceB,
    Edge { axis_a: usize, axis_b: usize },
}

/// Project an oriented box onto a world axis and return its half-width (radius of
/// the projection): `sum |axis·box_axis_i| * half_extent_i`.
fn box_project_radius(axes: &[Vec3; 3], half: Vec3, axis: Vec3) -> f64 {
    axis.dot(axes[0]).abs() * half.x
        + axis.dot(axes[1]).abs() * half.y
        + axis.dot(axes[2]).abs() * half.z
}

/// Penetration of two boxes along a *unit* candidate axis (positive = overlap
/// depth, negative = gap). The axis must already be normalized.
fn overlap_on_axis(
    axis: Vec3,
    center_delta: Vec3, // B.pos - A.pos
    axes_a: &[Vec3; 3],
    half_a: Vec3,
    axes_b: &[Vec3; 3],
    half_b: Vec3,
) -> f64 {
    let ra = box_project_radius(axes_a, half_a, axis);
    let rb = box_project_radius(axes_b, half_b, axis);
    let dist = center_delta.dot(axis).abs();
    ra + rb - dist
}

/// SAT over an oriented box pair. Tests the 6 face axes (3 per box) and the 9
/// edge-cross axes; returns the minimum-penetration axis oriented A→B, or `None`
/// if any axis separates the boxes. Determinism: axes are tested in a fixed
/// order and ties keep the earliest (face axes preferred over edge axes, which
/// matches the usual face-contact preference).
fn box_box_sat(
    axes_a: &[Vec3; 3],
    half_a: Vec3,
    pos_a: Vec3,
    axes_b: &[Vec3; 3],
    half_b: Vec3,
    pos_b: Vec3,
) -> Option<SatResult> {
    let center_delta = pos_b - pos_a; // A -> B

    let mut best_pen = f64::INFINITY;
    let mut best_axis = Vec3::ZERO;
    let mut best_feature = SatFeature::FaceA;
    // Small bias so face axes win ties over numerically-equal edge axes (more
    // stable face contacts). Applied only in the comparison, not the stored pen.
    let face_bias = 1.0e-5;

    // ---- face axes of A (indices 0..3) ----
    for i in 0..3 {
        let axis = axes_a[i].normalized();
        if axis.length_squared() < CONTACT_EPS {
            continue;
        }
        let pen = overlap_on_axis(axis, center_delta, axes_a, half_a, axes_b, half_b);
        if pen < -CONTACT_EPS {
            return None; // separating axis found
        }
        if pen < best_pen - face_bias {
            best_pen = pen;
            best_axis = axis;
            best_feature = SatFeature::FaceA;
        }
    }

    // ---- face axes of B (indices 3..6) ----
    for i in 0..3 {
        let axis = axes_b[i].normalized();
        if axis.length_squared() < CONTACT_EPS {
            continue;
        }
        let pen = overlap_on_axis(axis, center_delta, axes_a, half_a, axes_b, half_b);
        if pen < -CONTACT_EPS {
            return None;
        }
        if pen < best_pen - face_bias {
            best_pen = pen;
            best_axis = axis;
            best_feature = SatFeature::FaceB;
        }
    }

    // ---- edge-edge cross axes (9 of them) ----
    for i in 0..3 {
        for j in 0..3 {
            let cross = axes_a[i].cross(axes_b[j]);
            let len_sq = cross.length_squared();
            if len_sq < 1.0e-12 {
                // Parallel edges — degenerate axis, skip (the face axes already
                // cover the parallel-box case).
                continue;
            }
            let axis = cross * (1.0 / len_sq.sqrt());
            let pen = overlap_on_axis(axis, center_delta, axes_a, half_a, axes_b, half_b);
            if pen < -CONTACT_EPS {
                return None;
            }
            // Edge axes do NOT get the face bias; a strict improvement to win.
            if pen < best_pen {
                best_pen = pen;
                best_axis = axis;
                best_feature = SatFeature::Edge { axis_a: i, axis_b: j };
            }
        }
    }

    if best_axis.length_squared() < CONTACT_EPS {
        return None;
    }

    // Orient the chosen axis to point A -> B (so the normal sign is consistent).
    let normal = if best_axis.dot(center_delta) < 0.0 {
        -best_axis
    } else {
        best_axis
    };

    Some(SatResult {
        normal,
        penetration: best_pen.max(0.0),
        code: best_feature,
    })
}

/// Box–box contact generation. Runs SAT to find the separating/minimum axis;
/// builds contacts by reference/incident face clipping for a face axis, or the
/// edge–edge closest-point for an edge axis.
fn box_box(a: &Body, half_a: Vec3, b: &Body, half_b: Vec3) -> Vec<Contact> {
    let ra = a.rotation();
    let rb = b.rotation();
    let axes_a = [ra.cols[0], ra.cols[1], ra.cols[2]];
    let axes_b = [rb.cols[0], rb.cols[1], rb.cols[2]];

    let sat = match box_box_sat(&axes_a, half_a, a.pos, &axes_b, half_b, b.pos) {
        Some(s) => s,
        None => return Vec::new(),
    };

    match sat.code {
        SatFeature::FaceA | SatFeature::FaceB => {
            box_box_face_contacts(a, half_a, &axes_a, b, half_b, &axes_b, &sat)
        }
        SatFeature::Edge { axis_a, axis_b } => {
            box_box_edge_contact(a, half_a, &axes_a, b, half_b, &axes_b, axis_a, axis_b, &sat)
        }
    }
}

/// Face-contact path: clip the incident face against the side planes of the
/// reference face (Sutherland–Hodgman), keep the points below the reference
/// face, and emit a contact per kept point.
fn box_box_face_contacts(
    a: &Body,
    half_a: Vec3,
    axes_a: &[Vec3; 3],
    b: &Body,
    half_b: Vec3,
    axes_b: &[Vec3; 3],
    sat: &SatResult,
) -> Vec<Contact> {
    // Choose the reference box as the one whose face axis was selected. The
    // reference normal points OUT of the reference box toward the incident box.
    let normal = sat.normal; // A -> B

    let (ref_is_a, ref_body, ref_half, ref_axes, inc_body, inc_half, inc_axes, ref_normal) =
        match sat.code {
            SatFeature::FaceA => (
                true, a, half_a, axes_a, b, half_b, axes_b, normal, // out of A toward B
            ),
            SatFeature::FaceB => (
                false, b, half_b, axes_b, a, half_a, axes_a, -normal, // out of B toward A
            ),
            SatFeature::Edge { .. } => unreachable!("face path called for edge feature"),
        };

    // Reference face: the face of `ref_body` whose outward normal is closest to
    // `ref_normal`.
    let (ref_axis_idx, ref_sign) = best_face_axis(ref_axes, ref_normal);
    let ref_face_normal = ref_axes[ref_axis_idx] * ref_sign;

    // Incident face: the face of `inc_body` most anti-parallel to the reference
    // normal (its outward normal points most against ref_normal).
    let (inc_axis_idx, inc_sign) = most_antiparallel_face(inc_axes, ref_face_normal);

    // Build the incident face polygon (4 corners) in world space.
    let inc_poly = face_polygon(inc_body, inc_half, inc_axes, inc_axis_idx, inc_sign);

    // Reference face center + the two in-plane axis indices.
    let ref_half_comp = component(ref_half, ref_axis_idx);
    let ref_face_center = ref_body.pos + ref_face_normal * ref_half_comp;
    let (u_idx, v_idx) = other_two(ref_axis_idx);
    let u_axis = ref_axes[u_idx];
    let v_axis = ref_axes[v_idx];
    let u_half = component(ref_half, u_idx);
    let v_half = component(ref_half, v_idx);

    // Clip the incident polygon against the 4 side planes of the reference face.
    let mut poly = inc_poly.to_vec();
    poly = clip_to_slab(&poly, ref_face_center, u_axis, u_half);
    poly = clip_to_slab(&poly, ref_face_center, v_axis, v_half);

    // Keep points below the reference face (penetrating), emit a contact each.
    let mut contacts = Vec::new();
    for p in poly.iter() {
        let depth = ref_face_normal.dot(ref_face_center - *p); // >0 = below face
        if depth >= -CONTACT_EPS {
            let pen = depth.max(0.0);
            // Place the contact point on the reference face surface for stability.
            let on_face = *p + ref_face_normal * depth;
            let point = (*p + on_face) * 0.5;
            // Normal must point A -> B regardless of which box is reference.
            // ref_normal already points out of the reference toward the incident:
            //   ref=A: ref_normal = A->B  (correct)
            //   ref=B: ref_normal = B->A, so A->B = -ref_normal
            let n_ab = if ref_is_a { ref_face_normal } else { -ref_face_normal };
            contacts.push(Contact::new(point, n_ab, pen));
        }
    }

    // Fallback: if clipping produced nothing (numerical edge case), emit a single
    // contact at the midpoint along the SAT normal so the solver still resolves.
    if contacts.is_empty() {
        let mid = (a.pos + b.pos) * 0.5;
        contacts.push(Contact::new(mid, normal, sat.penetration));
    }
    contacts
}

/// Edge-contact path: the minimum axis was an edge–edge cross. Find the closest
/// points of the two contacting edges and emit a single contact between them.
#[allow(clippy::too_many_arguments)]
fn box_box_edge_contact(
    a: &Body,
    half_a: Vec3,
    axes_a: &[Vec3; 3],
    b: &Body,
    half_b: Vec3,
    axes_b: &[Vec3; 3],
    axis_a: usize,
    axis_b: usize,
    sat: &SatResult,
) -> Vec<Contact> {
    let normal = sat.normal; // A -> B

    // The contacting edge of A runs along axes_a[axis_a]; pick the edge on the
    // +normal side of A (the side facing B). Its midpoint = box center + the
    // signed half-extents on the two OTHER axes, chosen to maximize alignment
    // with +normal, with the edge's own axis free.
    let edge_a_dir = axes_a[axis_a];
    let edge_a_point = support_edge_point(a.pos, axes_a, half_a, axis_a, normal);

    let edge_b_dir = axes_b[axis_b];
    // B's contacting edge faces A, i.e. the -normal side of B.
    let edge_b_point = support_edge_point(b.pos, axes_b, half_b, axis_b, -normal);

    let (pa, pb) = closest_points_segments(
        edge_a_point,
        edge_a_dir,
        component(half_a, axis_a),
        edge_b_point,
        edge_b_dir,
        component(half_b, axis_b),
    );

    let point = (pa + pb) * 0.5;
    vec![Contact::new(point, normal, sat.penetration)]
}

// ===========================================================================
// Box-box helpers
// ===========================================================================

/// The face axis (index + sign) of a box whose outward normal best aligns with
/// `dir`. Returns the axis whose `±axis · dir` is maximal.
fn best_face_axis(axes: &[Vec3; 3], dir: Vec3) -> (usize, f64) {
    let mut best_idx = 0;
    let mut best_sign = 1.0;
    let mut best_dot = f64::NEG_INFINITY;
    for (i, &axis) in axes.iter().enumerate() {
        let d = axis.dot(dir);
        if d.abs() > best_dot {
            best_dot = d.abs();
            best_idx = i;
            best_sign = if d >= 0.0 { 1.0 } else { -1.0 };
        }
    }
    (best_idx, best_sign)
}

/// The face axis (index + sign) whose outward normal is most ANTI-parallel to
/// `dir` (the incident face of a box against a reference normal `dir`).
fn most_antiparallel_face(axes: &[Vec3; 3], dir: Vec3) -> (usize, f64) {
    let mut best_idx = 0;
    let mut best_sign = 1.0;
    let mut best_dot = f64::INFINITY;
    for (i, &axis) in axes.iter().enumerate() {
        // Outward face normals are ±axis; we want the one with the most
        // negative dot with `dir`.
        for &sign in &[1.0_f64, -1.0_f64] {
            let d = (axis * sign).dot(dir);
            if d < best_dot {
                best_dot = d;
                best_idx = i;
                best_sign = sign;
            }
        }
    }
    (best_idx, best_sign)
}

/// The 4 world-space corners of a box face: the face whose outward normal is
/// `axes[axis_idx] * sign`. Corners are emitted in a fixed winding so clipping is
/// deterministic.
fn face_polygon(
    body: &Body,
    half: Vec3,
    axes: &[Vec3; 3],
    axis_idx: usize,
    sign: f64,
) -> [Vec3; 4] {
    let n = axes[axis_idx] * sign;
    let (u_idx, v_idx) = other_two(axis_idx);
    let u = axes[u_idx] * component(half, u_idx);
    let v = axes[v_idx] * component(half, v_idx);
    let center = body.pos + n * component(half, axis_idx);
    [
        center - u - v,
        center + u - v,
        center + u + v,
        center - u + v,
    ]
}

/// Clip a convex polygon to the slab `|(<p - center>)·axis| <= half` along a
/// single in-plane axis (one Sutherland–Hodgman pass for two parallel planes).
fn clip_to_slab(poly: &[Vec3], center: Vec3, axis: Vec3, half: f64) -> Vec<Vec3> {
    // Clip against the +half plane then the -half plane.
    let pos = clip_to_plane(poly, center, axis, half);
    clip_to_plane(&pos, center, -axis, half)
}

/// Clip a polygon to the half-space `(<p - center>)·axis <= half` (keep points
/// on the `axis` side within `half`).
fn clip_to_plane(poly: &[Vec3], center: Vec3, axis: Vec3, half: f64) -> Vec<Vec3> {
    let n = poly.len();
    if n == 0 {
        return Vec::new();
    }
    let mut out = Vec::with_capacity(n + 1);
    let dist = |p: Vec3| axis.dot(p - center) - half; // <=0 inside
    for i in 0..n {
        let cur = poly[i];
        let next = poly[(i + 1) % n];
        let dc = dist(cur);
        let dn = dist(next);
        let cur_in = dc <= CONTACT_EPS;
        let next_in = dn <= CONTACT_EPS;
        if cur_in {
            out.push(cur);
        }
        // Edge crosses the plane: add the intersection.
        if cur_in != next_in {
            let denom = dc - dn;
            if denom.abs() > CONTACT_EPS {
                let t = dc / denom;
                out.push(cur + (next - cur) * t);
            }
        }
    }
    out
}

/// Pick the corner-of-an-edge point on the side of the box facing `dir`. The
/// edge runs along `axes[edge_axis]`; the two other axes are pinned to the
/// half-extent corner that maximizes `· dir`. Returns the MIDPOINT of that edge
/// (its own axis at 0), which `closest_points_segments` then slides along.
fn support_edge_point(
    center: Vec3,
    axes: &[Vec3; 3],
    half: Vec3,
    edge_axis: usize,
    dir: Vec3,
) -> Vec3 {
    let (u_idx, v_idx) = other_two(edge_axis);
    let u_sign = if axes[u_idx].dot(dir) >= 0.0 { 1.0 } else { -1.0 };
    let v_sign = if axes[v_idx].dot(dir) >= 0.0 { 1.0 } else { -1.0 };
    center
        + axes[u_idx] * (component(half, u_idx) * u_sign)
        + axes[v_idx] * (component(half, v_idx) * v_sign)
}

/// Closest points between two finite segments, each given by a midpoint, a unit
/// direction, and a half-length. Returns `(point_on_a, point_on_b)`.
fn closest_points_segments(
    mid_a: Vec3,
    dir_a: Vec3,
    half_a: f64,
    mid_b: Vec3,
    dir_b: Vec3,
    half_b: f64,
) -> (Vec3, Vec3) {
    let da = dir_a.normalized();
    let db = dir_b.normalized();
    let r = mid_a - mid_b;
    let a = 1.0; // da·da
    let b = da.dot(db);
    let c = da.dot(r);
    let e = 1.0; // db·db
    let f = db.dot(r);
    let denom = a * e - b * b;

    // Parameters along each segment from its midpoint (clamped to ±half).
    let mut s = if denom.abs() > CONTACT_EPS {
        ((b * f - c * e) / denom).max(-half_a).min(half_a)
    } else {
        0.0 // parallel — midpoint is as good as any
    };
    let mut t = (b * s + f) / e;
    if t < -half_b || t > half_b {
        t = t.max(-half_b).min(half_b);
        s = ((b * t - c) / a).max(-half_a).min(half_a);
    }
    let pa = mid_a + da * s;
    let pb = mid_b + db * t;
    (pa, pb)
}

/// The two axis indices other than `i`, in ascending order.
#[inline]
fn other_two(i: usize) -> (usize, usize) {
    match i {
        0 => (1, 2),
        1 => (0, 2),
        _ => (0, 1),
    }
}

/// The `i`-th component (0=x,1=y,2=z) of a [`Vec3`].
#[inline]
fn component(v: Vec3, i: usize) -> f64 {
    match i {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}

// A small extension so an inside-the-box sphere center never produces a zero
// push-out direction (signum of 0.0 is 0.0, which would lose the axis).
trait SignumNonZero {
    fn signum_nonzero(self) -> f64;
}
impl SignumNonZero for f64 {
    #[inline]
    fn signum_nonzero(self) -> f64 {
        if self >= 0.0 {
            1.0
        } else {
            -1.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{Body, Material, Shape};
    use crate::math::Quat;

    // --- helpers -----------------------------------------------------------

    fn world_with(bodies: Vec<Body>) -> World {
        let mut w = World::new();
        for body in bodies {
            w.spawn(body);
        }
        w
    }

    fn sphere(pos: Vec3, r: f64) -> Body {
        Body::new(Shape::Sphere { radius: r }, pos, 1.0, Material::default())
    }

    fn cuboid(pos: Vec3, half: Vec3) -> Body {
        Body::new(Shape::Cuboid { half_extents: half }, pos, 1.0, Material::default())
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn approx_vec(a: Vec3, b: Vec3) -> bool {
        approx(a.x, b.x) && approx(a.y, b.y) && approx(a.z, b.z)
    }

    // --- frozen-type sanity (kept from Foundation) -------------------------

    #[test]
    fn manifold_starts_empty() {
        let m = Manifold::new(BodyId(0), BodyId(1));
        assert!(m.is_empty());
        assert_eq!(m.a, BodyId(0));
        assert_eq!(m.b, BodyId(1));
    }

    #[test]
    fn contact_zeros_accumulated_impulses() {
        let c = Contact::new(Vec3::ZERO, Vec3::Y, 0.1);
        assert_eq!(c.normal_impulse, 0.0);
        assert_eq!(c.tangent_impulse, 0.0);
        assert_eq!(c.penetration, 0.1);
    }

    // --- separation yields none --------------------------------------------

    #[test]
    fn separated_spheres_yield_none() {
        let w = world_with(vec![sphere(Vec3::ZERO, 1.0), sphere(Vec3::new(5.0, 0.0, 0.0), 1.0)]);
        assert_eq!(collide(&w, Pair(BodyId(0), BodyId(1))), None);
    }

    #[test]
    fn collide_all_filters_none_and_keeps_order() {
        // Two pairs: one separated (none), one overlapping (some). Order kept.
        let w = world_with(vec![
            sphere(Vec3::ZERO, 1.0),
            sphere(Vec3::new(10.0, 0.0, 0.0), 1.0), // far from 0
            sphere(Vec3::new(1.5, 0.0, 0.0), 1.0),  // overlaps 0
        ]);
        let pairs = [Pair(BodyId(0), BodyId(1)), Pair(BodyId(0), BodyId(2))];
        let manifolds = collide_all(&w, &pairs);
        assert_eq!(manifolds.len(), 1);
        assert_eq!(manifolds[0].a, BodyId(0));
        assert_eq!(manifolds[0].b, BodyId(2));
    }

    // --- sphere-sphere -----------------------------------------------------

    #[test]
    fn overlapping_spheres_normal_and_depth() {
        // Centers 1.5 apart, radii 1 + 1 = 2 -> penetration 0.5, normal +X.
        let w = world_with(vec![sphere(Vec3::ZERO, 1.0), sphere(Vec3::new(1.5, 0.0, 0.0), 1.0)]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        assert_eq!(m.contacts.len(), 1);
        let c = m.contacts[0];
        assert!(approx_vec(c.normal, Vec3::X), "normal {:?}", c.normal);
        assert!(approx(c.penetration, 0.5), "pen {}", c.penetration);
        // Contact point on the overlap midline: surfaces at x=1.0 (A) and x=0.5 (B),
        // midpoint x=0.75.
        assert!(approx(c.point.x, 0.75), "point {:?}", c.point);
    }

    #[test]
    fn touching_spheres_register_contact() {
        // Exactly touching: distance == radius sum -> zero penetration contact.
        let w = world_with(vec![sphere(Vec3::ZERO, 1.0), sphere(Vec3::new(2.0, 0.0, 0.0), 1.0)]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("touching contact");
        assert!(approx(m.contacts[0].penetration, 0.0));
        assert!(approx_vec(m.contacts[0].normal, Vec3::X));
    }

    #[test]
    fn coincident_spheres_pick_stable_normal() {
        let w = world_with(vec![sphere(Vec3::ZERO, 1.0), sphere(Vec3::ZERO, 1.0)]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        let c = m.contacts[0];
        assert!(c.normal.is_finite() && approx(c.normal.length(), 1.0));
        assert!(approx(c.penetration, 2.0));
    }

    // --- sphere-plane ------------------------------------------------------

    #[test]
    fn sphere_resting_on_plane_normal_and_penetration() {
        // Plane y=0 (normal +Y, offset 0). Sphere radius 1 centered at y=0.5 ->
        // center 0.5 above surface, radius 1 -> penetration 0.5.
        let plane = Body::plane(Vec3::Y, 0.0);
        let s = sphere(Vec3::new(0.0, 0.5, 0.0), 1.0);
        // Pair ascending: plane is body 0 (A), sphere is body 1 (B).
        let w = world_with(vec![plane, s]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        assert_eq!(m.contacts.len(), 1);
        let c = m.contacts[0];
        // A = plane, B = sphere -> A->B normal points from plane toward sphere = +Y.
        assert!(approx_vec(c.normal, Vec3::Y), "normal {:?}", c.normal);
        assert!(approx(c.penetration, 0.5), "pen {}", c.penetration);
    }

    #[test]
    fn sphere_above_plane_yields_none() {
        let plane = Body::plane(Vec3::Y, 0.0);
        let s = sphere(Vec3::new(0.0, 5.0, 0.0), 1.0); // far above
        let w = world_with(vec![plane, s]);
        assert_eq!(collide(&w, Pair(BodyId(0), BodyId(1))), None);
    }

    #[test]
    fn sphere_first_plane_second_normal_points_into_plane() {
        // Reverse order: sphere is A, plane is B. A->B normal points sphere->plane
        // = -Y.
        let s = sphere(Vec3::new(0.0, 0.5, 0.0), 1.0);
        let plane = Body::plane(Vec3::Y, 0.0);
        let w = world_with(vec![s, plane]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        assert!(approx_vec(m.contacts[0].normal, -Vec3::Y), "normal {:?}", m.contacts[0].normal);
        assert!(approx(m.contacts[0].penetration, 0.5));
    }

    // --- box-plane ---------------------------------------------------------

    #[test]
    fn box_resting_on_plane_four_contacts() {
        // Unit box (half 0.5) resting so its bottom face dips 0.1 below y=0.
        // Center at y = 0.4 -> bottom corners at y = -0.1, penetration 0.1.
        let plane = Body::plane(Vec3::Y, 0.0);
        let b = cuboid(Vec3::new(0.0, 0.4, 0.0), Vec3::splat(0.5));
        let w = world_with(vec![plane, b]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        // 4 bottom corners penetrate (top corners are above the surface).
        assert_eq!(m.contacts.len(), 4, "contacts {:?}", m.contacts.len());
        for c in &m.contacts {
            // A = plane, B = box -> normal plane->box = +Y.
            assert!(approx_vec(c.normal, Vec3::Y), "normal {:?}", c.normal);
            assert!(approx(c.penetration, 0.1), "pen {}", c.penetration);
        }
        // The four contact points are at the four bottom-corner x/z positions.
        let xs: Vec<f64> = m.contacts.iter().map(|c| c.point.x).collect();
        assert!(xs.iter().any(|&x| approx(x, 0.5)));
        assert!(xs.iter().any(|&x| approx(x, -0.5)));
    }

    #[test]
    fn box_above_plane_yields_none() {
        let plane = Body::plane(Vec3::Y, 0.0);
        let b = cuboid(Vec3::new(0.0, 5.0, 0.0), Vec3::splat(0.5));
        let w = world_with(vec![plane, b]);
        assert_eq!(collide(&w, Pair(BodyId(0), BodyId(1))), None);
    }

    #[test]
    fn tilted_box_on_plane_bottom_edge_two_contacts() {
        // Box rotated 45deg about Z rests on a horizontal bottom EDGE: the two
        // corners on that edge (which differ only along the un-rotated Z axis)
        // share the lowest Y, so exactly two corners cross the plane.
        let plane = Body::plane(Vec3::Y, 0.0);
        let mut b = cuboid(Vec3::new(0.0, 0.0, 0.0), Vec3::splat(0.5));
        b.orientation = Quat::from_axis_angle(Vec3::Z, std::f64::consts::FRAC_PI_4);
        // Half-diagonal in the XY plane = 0.5*sqrt(2) ≈ 0.7071. Center at y=0.65
        // dips the bottom edge ≈ 0.0571 below the plane; the next corners up sit
        // at y ≈ 0.65 (well above), so only the bottom edge penetrates.
        b.pos = Vec3::new(0.0, 0.65, 0.0);
        let w = world_with(vec![plane, b]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        assert_eq!(m.contacts.len(), 2, "got {} contacts", m.contacts.len());
        let expected_pen = 0.5 * 2.0_f64.sqrt() - 0.65;
        for c in &m.contacts {
            assert!(approx_vec(c.normal, Vec3::Y), "normal {:?}", c.normal);
            assert!(approx(c.penetration, expected_pen), "pen {}", c.penetration);
        }
    }

    #[test]
    fn corner_down_box_on_plane_single_contact() {
        // Compose two 45deg tilts so a single corner is the unique lowest point,
        // yielding exactly one box-plane contact. We verify the count against an
        // independent recomputation of which corners cross the plane.
        let plane = Body::plane(Vec3::Y, 0.0);
        let half = Vec3::splat(0.5);
        let mut b = cuboid(Vec3::ZERO, half);
        let q1 = Quat::from_axis_angle(Vec3::Z, std::f64::consts::FRAC_PI_4);
        let q2 = Quat::from_axis_angle(Vec3::X, std::f64::consts::FRAC_PI_4);
        b.orientation = q2.mul(q1).normalized();
        // Independently find the lowest corner Y to place the box so only it dips
        // under y=0.
        let r = b.rotation();
        let ax = r.cols[0] * half.x;
        let ay = r.cols[1] * half.y;
        let az = r.cols[2] * half.z;
        let mut corner_ys = Vec::new();
        for sx in [-1.0, 1.0] {
            for sy in [-1.0, 1.0] {
                for sz in [-1.0, 1.0] {
                    corner_ys.push((ax * sx + ay * sy + az * sz).y);
                }
            }
        }
        corner_ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let lowest = corner_ys[0];
        let second = corner_ys[1];
        // Place the center so the lowest corner is below y=0 but the second is
        // above: center.y between -second and -lowest.
        let center_y = -(lowest + second) * 0.5;
        b.pos = Vec3::new(0.0, center_y, 0.0);
        let w = world_with(vec![plane, b]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        assert_eq!(m.contacts.len(), 1, "got {} contacts", m.contacts.len());
        assert!(approx_vec(m.contacts[0].normal, Vec3::Y));
        assert!(m.contacts[0].penetration > 0.0);
    }

    // --- sphere-box --------------------------------------------------------

    #[test]
    fn sphere_resting_on_box_face() {
        // Box half 0.5 centered at origin; sphere radius 1 above it touching the
        // top face. Top face at y=0.5; sphere center at y=1.4 -> closest point
        // y=0.5, distance 0.9, radius 1 -> penetration 0.1, normal +Y (box->sphere).
        let b = cuboid(Vec3::ZERO, Vec3::splat(0.5));
        let s = sphere(Vec3::new(0.0, 1.4, 0.0), 1.0);
        // box is body 0 (A), sphere body 1 (B). normal A(box)->B(sphere) = +Y.
        let w = world_with(vec![b, s]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        let c = m.contacts[0];
        assert!(approx_vec(c.normal, Vec3::Y), "normal {:?}", c.normal);
        assert!(approx(c.penetration, 0.1), "pen {}", c.penetration);
    }

    #[test]
    fn sphere_far_from_box_yields_none() {
        let b = cuboid(Vec3::ZERO, Vec3::splat(0.5));
        let s = sphere(Vec3::new(0.0, 5.0, 0.0), 1.0);
        let w = world_with(vec![b, s]);
        assert_eq!(collide(&w, Pair(BodyId(0), BodyId(1))), None);
    }

    #[test]
    fn sphere_center_inside_box_pushes_out_nearest_face() {
        // Sphere center inside the box, closest to the +X face.
        let b = cuboid(Vec3::ZERO, Vec3::new(1.0, 1.0, 1.0));
        let s = sphere(Vec3::new(0.9, 0.0, 0.0), 0.2);
        let w = world_with(vec![b, s]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        let c = m.contacts[0];
        // A=box, B=sphere; nearest face is +X, push sphere out +X -> normal +X.
        assert!(approx_vec(c.normal, Vec3::X), "normal {:?}", c.normal);
        // depth to +X face = 1.0 - 0.9 = 0.1; + radius 0.2 = 0.3.
        assert!(approx(c.penetration, 0.3), "pen {}", c.penetration);
    }

    // --- box-box SAT -------------------------------------------------------

    #[test]
    fn aabb_box_box_face_overlap() {
        // Two unit boxes (half 0.5) overlapping along X by 0.2.
        // A at x=0, B at x=0.8 -> gap of centers 0.8, sum of half-widths 1.0 ->
        // overlap 0.2 on X. Min axis is +X.
        let a = cuboid(Vec3::ZERO, Vec3::splat(0.5));
        let bb = cuboid(Vec3::new(0.8, 0.0, 0.0), Vec3::splat(0.5));
        let w = world_with(vec![a, bb]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        assert!(!m.contacts.is_empty());
        for c in &m.contacts {
            assert!(approx_vec(c.normal, Vec3::X), "normal {:?}", c.normal);
            assert!(approx(c.penetration, 0.2), "pen {}", c.penetration);
        }
        // A face-on-face overlap should clip to multiple stable contacts.
        assert!(m.contacts.len() >= 2, "expected face manifold, got {}", m.contacts.len());
    }

    #[test]
    fn separated_boxes_yield_none() {
        let a = cuboid(Vec3::ZERO, Vec3::splat(0.5));
        let bb = cuboid(Vec3::new(3.0, 0.0, 0.0), Vec3::splat(0.5));
        let w = world_with(vec![a, bb]);
        assert_eq!(collide(&w, Pair(BodyId(0), BodyId(1))), None);
    }

    #[test]
    fn box_box_picks_min_penetration_axis() {
        // Overlap deeper on X than on Y: min axis must be Y (smaller overlap).
        // A half 1.0 at origin; B half 1.0 at (1.5, 1.8, 0).
        //   X overlap = (1.0+1.0) - 1.5 = 0.5
        //   Y overlap = (1.0+1.0) - 1.8 = 0.2  <- minimum -> normal +Y
        let a = cuboid(Vec3::ZERO, Vec3::splat(1.0));
        let bb = cuboid(Vec3::new(1.5, 1.8, 0.0), Vec3::splat(1.0));
        let w = world_with(vec![a, bb]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        let c = m.contacts[0];
        assert!(approx_vec(c.normal, Vec3::Y), "normal {:?}", c.normal);
        assert!(approx(c.penetration, 0.2), "pen {}", c.penetration);
    }

    #[test]
    fn box_box_edge_contact_via_cross_axis() {
        // Two boxes rotated 45deg about different axes so the separating min axis
        // is an edge-edge cross rather than a face. We mainly assert a contact is
        // produced with a finite unit normal pointing A->B and positive depth.
        let mut a = cuboid(Vec3::ZERO, Vec3::splat(0.5));
        a.orientation = Quat::from_axis_angle(Vec3::Z, std::f64::consts::FRAC_PI_4);
        let mut bb = cuboid(Vec3::new(0.9, 0.0, 0.0), Vec3::splat(0.5));
        bb.orientation = Quat::from_axis_angle(Vec3::Y, std::f64::consts::FRAC_PI_4);
        let w = world_with(vec![a, bb]);
        let m = collide(&w, Pair(BodyId(0), BodyId(1))).expect("contact");
        let c = m.contacts[0];
        assert!(c.normal.is_finite() && approx(c.normal.length(), 1.0), "normal {:?}", c.normal);
        // Normal points roughly A->B (positive X component) for these positions.
        assert!(c.normal.dot(bb_center_minus_a()) > 0.0);
        assert!(c.penetration >= 0.0);
    }

    fn bb_center_minus_a() -> Vec3 {
        Vec3::new(0.9, 0.0, 0.0)
    }

    // --- determinism -------------------------------------------------------

    #[test]
    fn collide_is_deterministic() {
        let build = || {
            world_with(vec![
                cuboid(Vec3::ZERO, Vec3::splat(0.5)),
                cuboid(Vec3::new(0.8, 0.1, 0.0), Vec3::splat(0.5)),
            ])
        };
        let w1 = build();
        let w2 = build();
        let m1 = collide(&w1, Pair(BodyId(0), BodyId(1))).unwrap();
        let m2 = collide(&w2, Pair(BodyId(0), BodyId(1))).unwrap();
        assert_eq!(m1.contacts.len(), m2.contacts.len());
        for (c1, c2) in m1.contacts.iter().zip(m2.contacts.iter()) {
            assert_eq!(c1.normal, c2.normal);
            assert_eq!(c1.penetration, c2.penetration);
            assert_eq!(c1.point, c2.point);
        }
    }

    #[test]
    fn missing_body_yields_none() {
        let w = world_with(vec![sphere(Vec3::ZERO, 1.0)]);
        // BodyId(1) does not exist.
        assert_eq!(collide(&w, Pair(BodyId(0), BodyId(1))), None);
    }

    #[test]
    fn degenerate_self_pair_yields_none() {
        let w = world_with(vec![sphere(Vec3::ZERO, 1.0)]);
        assert_eq!(collide(&w, Pair(BodyId(0), BodyId(0))), None);
    }
}
