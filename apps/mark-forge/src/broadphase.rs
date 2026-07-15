//! Broadphase (SPEC §4) — AABB candidate-pair generation.
//!
//! Owns the FROZEN [`Aabb`] type + the [`candidate_pairs`] contract. The pair
//! generation BODY (the uniform spatial-hash grid + the brute-force `O(n²)`
//! oracle) is fully implemented here and pinned against these frozen signatures.
//! Downstream agents must NOT change [`Aabb`] or [`candidate_pairs`]'s signature.
//!
//! Determinism (SPEC §1/§4): pairs are emitted in a fixed, sorted
//! `(min_id, max_id)` order; the grid path must produce the SAME overlapping set
//! as the brute-force oracle.

use crate::body::{Body, BodyId, Shape};
use crate::math::Vec3;
use crate::world::World;

/// A world-space axis-aligned bounding box. `min`/`max` are the corner extremes;
/// a [`Shape::Plane`] is unbounded and represented by [`Aabb::INFINITE`] (it is
/// paired against every dynamic body rather than grid-binned).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    /// The whole-space AABB used for an infinite plane half-space.
    pub const INFINITE: Aabb = Aabb {
        min: Vec3::splat(f64::NEG_INFINITY),
        max: Vec3::splat(f64::INFINITY),
    };

    /// An AABB from explicit corners (caller guarantees `min <= max`).
    #[inline]
    pub fn new(min: Vec3, max: Vec3) -> Aabb {
        Aabb { min, max }
    }

    /// An AABB centered at `center` with the given non-negative `half` extent.
    #[inline]
    pub fn from_center_half(center: Vec3, half: Vec3) -> Aabb {
        Aabb { min: center - half, max: center + half }
    }

    /// Do two AABBs overlap on all three axes (touching counts as overlap)?
    #[inline]
    pub fn overlaps(&self, o: &Aabb) -> bool {
        self.min.x <= o.max.x
            && self.max.x >= o.min.x
            && self.min.y <= o.max.y
            && self.max.y >= o.min.y
            && self.min.z <= o.max.z
            && self.max.z >= o.min.z
    }

    /// This AABB grown by a uniform `margin` on every side (contact prediction
    /// skin / speculative margin).
    #[inline]
    pub fn expanded(&self, margin: f64) -> Aabb {
        let m = Vec3::splat(margin);
        Aabb { min: self.min - m, max: self.max + m }
    }

    /// The world AABB of a body's shape at its current transform (SPEC §4). A
    /// sphere is `pos ± radius`; an oriented cuboid is the enclosing
    /// axis-aligned box of the rotated half-extents; a plane is
    /// [`Aabb::INFINITE`].
    pub fn of_body(body: &Body) -> Aabb {
        match body.shape {
            Shape::Sphere { radius } => Aabb::from_center_half(body.pos, Vec3::splat(radius)),
            Shape::Cuboid { half_extents } => {
                // Enclosing AABB of an oriented box: rotate each half-extent axis
                // and sum the absolute contributions (the support of the box on
                // each world axis). Standard |R| * h extent.
                let r = body.rotation();
                let ex = (r.cols[0] * half_extents.x).abs();
                let ey = (r.cols[1] * half_extents.y).abs();
                let ez = (r.cols[2] * half_extents.z).abs();
                let half = ex + ey + ez;
                Aabb::from_center_half(body.pos, half)
            }
            Shape::Plane { .. } => Aabb::INFINITE,
        }
    }
}

/// A candidate colliding pair from the broadphase — body indices with
/// `a.0 < b.0` (the fixed sorted order, SPEC §4). The narrowphase resolves each
/// pair into a [`crate::narrowphase::Manifold`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pair(pub BodyId, pub BodyId);

/// A finite (non-plane) world AABB paired with the body it belongs to. Planes
/// are handled separately (they are [`Aabb::INFINITE`] and bin into every cell),
/// so the grid only ever stores bounded boxes.
#[derive(Debug, Clone, Copy)]
struct BoundedBody {
    id: BodyId,
    aabb: Aabb,
    is_static: bool,
}

/// Construct the canonical `(min_id, max_id)` pair for two distinct bodies,
/// upholding the [`Pair`] `a.0 < b.0` convention (SPEC §4). Caller guarantees
/// `x != y`.
#[inline]
fn ordered_pair(x: BodyId, y: BodyId) -> Pair {
    if x.0 < y.0 {
        Pair(x, y)
    } else {
        Pair(y, x)
    }
}

/// May these two bodies form a candidate pair? Static–static pairs are skipped
/// (two immovable bodies never need a contact, SPEC §4); everything else with at
/// least one dynamic body is eligible.
#[inline]
fn pair_eligible(a_static: bool, b_static: bool) -> bool {
    !(a_static && b_static)
}

/// The reference broadphase: the exhaustive `O(n²)` AABB-overlap scan (SPEC §4).
/// Every distinct, eligible body pair whose world AABBs overlap is emitted. This
/// is the determinism ORACLE the spatial-hash grid is asserted equal to — it has
/// no spatial structure to get wrong, so it pins the exact candidate set.
///
/// Pairs come out sorted ascending by `(a, b)` and deduplicated.
pub fn candidate_pairs_brute_force(world: &World) -> Vec<Pair> {
    let n = world.bodies.len();
    let mut pairs: Vec<Pair> = Vec::new();
    for i in 0..n {
        let bi = &world.bodies[i];
        let ai = Aabb::of_body(bi);
        let id_i = BodyId(i as u32);
        let static_i = bi.is_static();
        for j in (i + 1)..n {
            let bj = &world.bodies[j];
            if !pair_eligible(static_i, bj.is_static()) {
                continue;
            }
            let aj = Aabb::of_body(bj);
            if ai.overlaps(&aj) {
                pairs.push(ordered_pair(id_i, BodyId(j as u32)));
            }
        }
    }
    // i<j with i as the smaller index already yields ascending (a,b) order, but
    // sort + dedup defensively so the contract holds regardless of insertion.
    pairs.sort();
    pairs.dedup();
    pairs
}

/// The cell coordinate a world position hashes into for the uniform grid.
/// `Ord`/`PartialOrd` (lexicographic on `(x, y, z)`) lets the budgeted broadphase
/// visit occupied cells in a DETERMINISTIC order so its short-circuited pair
/// prefix is reproducible (HashMap iteration order is unspecified).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Cell {
    x: i64,
    y: i64,
    z: i64,
}

/// Floor a finite world coordinate to its integer cell index along one axis.
/// `cell_size` is strictly positive. The `floor` keeps negative coordinates on
/// the correct side of the lattice (so `-0.1` and `0.1` land in different cells
/// across the origin boundary).
#[inline]
fn cell_index(coord: f64, cell_size: f64) -> i64 {
    (coord / cell_size).floor() as i64
}

/// Choose the uniform grid cell size from the dynamic bodies' AABB extents. A
/// good cell is about the size of the largest body so each box only touches a
/// handful of cells; we use the largest finite AABB diagonal extent (the max of
/// the per-axis box widths) and fall back to a unit cell when there is nothing
/// finite to measure. Deterministic: a pure max over the (already deterministic)
/// AABBs, no RNG / wall-clock (SPEC §1).
fn choose_cell_size(bounded: &[BoundedBody]) -> f64 {
    let mut max_extent = 0.0_f64;
    for b in bounded {
        let size = b.aabb.max - b.aabb.min;
        max_extent = max_extent.max(size.x).max(size.y).max(size.z);
    }
    if max_extent > 0.0 && max_extent.is_finite() {
        max_extent
    } else {
        1.0
    }
}

/// Produce the deterministic, sorted set of candidate colliding pairs for the
/// world's bodies (SPEC §4). Static–static pairs are skipped (two immovable
/// bodies never need a contact). Pairs are returned ascending by `(a, b)`.
///
/// Strategy (SPEC §4): each body's world AABB is [`Aabb::of_body`]. Bounded
/// (non-plane) bodies are binned into a uniform spatial-hash grid keyed by
/// integer cell; only bodies sharing a cell are AABB-tested, collapsing the
/// `O(n²)` scan to roughly `O(n)` for spatially-spread scenes. Planes are
/// [`Aabb::INFINITE`] and cannot be binned, so every plane is paired against
/// every dynamic body directly. The grid is asserted EQUAL to
/// [`candidate_pairs_brute_force`] (the oracle) in the tests — same overlapping
/// set, same order — so the solver downstream is reproducible.
pub fn candidate_pairs(world: &World) -> Vec<Pair> {
    let n = world.bodies.len();
    if n < 2 {
        return Vec::new();
    }

    // Split bodies into bounded (grid-binned) bodies and infinite-AABB planes.
    let mut bounded: Vec<BoundedBody> = Vec::with_capacity(n);
    let mut planes: Vec<(BodyId, bool)> = Vec::new();
    for (i, body) in world.bodies.iter().enumerate() {
        let id = BodyId(i as u32);
        let aabb = Aabb::of_body(body);
        let is_static = body.is_static();
        // A non-finite (infinite) AABB on any corner is the plane half-space; it
        // cannot be grid-binned and is paired against everything separately.
        if aabb.min.is_finite() && aabb.max.is_finite() {
            bounded.push(BoundedBody { id, aabb, is_static });
        } else {
            planes.push((id, is_static));
        }
    }

    let cell_size = choose_cell_size(&bounded);

    // Bin each bounded body into every grid cell its AABB spans. A box larger
    // than a cell occupies several cells; bodies that share ANY cell become
    // overlap-test candidates. Insertion order into each cell follows ascending
    // BodyId because we iterate `bounded` in id order (SPEC §1 fixed order).
    let mut grid: std::collections::HashMap<Cell, Vec<usize>> = std::collections::HashMap::new();
    for (slot, b) in bounded.iter().enumerate() {
        let min_x = cell_index(b.aabb.min.x, cell_size);
        let max_x = cell_index(b.aabb.max.x, cell_size);
        let min_y = cell_index(b.aabb.min.y, cell_size);
        let max_y = cell_index(b.aabb.max.y, cell_size);
        let min_z = cell_index(b.aabb.min.z, cell_size);
        let max_z = cell_index(b.aabb.max.z, cell_size);
        for cx in min_x..=max_x {
            for cy in min_y..=max_y {
                for cz in min_z..=max_z {
                    grid.entry(Cell { x: cx, y: cy, z: cz }).or_default().push(slot);
                }
            }
        }
    }

    let mut pairs: Vec<Pair> = Vec::new();

    // Within each occupied cell, AABB-test every distinct slot pair. A body that
    // spans multiple shared cells is found in more than one cell, hence the
    // final sort + dedup. HashMap iteration order is unspecified, so we collect
    // first and sort at the end — the OUTPUT order is fully deterministic.
    for slots in grid.values() {
        let m = slots.len();
        for a in 0..m {
            let ba = &bounded[slots[a]];
            for b in (a + 1)..m {
                let bb = &bounded[slots[b]];
                if !pair_eligible(ba.is_static, bb.is_static) {
                    continue;
                }
                if ba.aabb.overlaps(&bb.aabb) {
                    pairs.push(ordered_pair(ba.id, bb.id));
                }
            }
        }
    }

    // Planes (infinite AABBs) pair against every eligible body. Plane-vs-plane
    // is static-static and skipped; plane-vs-bounded always "overlaps" (the
    // plane AABB is whole-space), so the narrowphase decides actual contact.
    for (pi, &(plane_id, plane_static)) in planes.iter().enumerate() {
        // plane vs every bounded body
        for b in &bounded {
            if !pair_eligible(plane_static, b.is_static) {
                continue;
            }
            pairs.push(ordered_pair(plane_id, b.id));
        }
        // plane vs other planes (later in the list to avoid double counting)
        for &(other_id, other_static) in planes.iter().skip(pi + 1) {
            if !pair_eligible(plane_static, other_static) {
                continue;
            }
            pairs.push(ordered_pair(plane_id, other_id));
        }
    }

    pairs.sort();
    pairs.dedup();
    pairs
}

/// BOUNDED-WORK variant of [`candidate_pairs`] (SPEC §1, defensive): generate the
/// deterministic candidate-pair set but SHORT-CIRCUIT once `budget` pairs have
/// been emitted, returning `(pairs, true)` when the budget bit (and `(pairs,
/// false)` when the full set fit). The point is to bound the *enumeration* itself,
/// not just to truncate after the fact: a fully-collapsed scene (every body in one
/// grid cell) would make the plain [`candidate_pairs`] inner double-loop do ~k²/2
/// AABB tests for a cell of `k` bodies — at the [`crate::ipc::MAX_BODIES`] cap of
/// 4096 that is ~8.4M AABB tests per substep *before* any contact truncation could help. Here
/// we stop emitting (and stop AABB-testing) the instant the running pair count
/// reaches `budget`, so the total AABB tests + manifolds per substep is
/// `O(budget)`, NOT `O(n²)`, even when all bodies share one cell.
///
/// DETERMINISM (SPEC §1): the emitted prefix is identical to the
/// budget-many smallest cells-then-pairs the unbudgeted path would emit, in a
/// FIXED order. To get a deterministic *prefix* (not just a deterministic *set*),
/// we cannot rely on `HashMap` iteration order — so the occupied cells are visited
/// in sorted [`Cell`] order, and within a cell the slot pairs are enumerated in
/// ascending order. The same scene therefore always yields the same prefix and the
/// same `pairs_cap_hit` signal. (Planes are emitted first, before the grid cells,
/// in id order — they are O(planes × bodies) and the normal physics never has more
/// than a handful of planes, but they too count against and respect the budget so
/// a pathological all-planes scene still cannot exceed it.)
///
/// The returned `pairs` are sorted ascending + deduped to match the
/// [`candidate_pairs`] output contract, so the narrowphase sees the same shape of
/// input either way. When the budget does NOT bite, the returned set equals
/// [`candidate_pairs`] exactly (asserted in the tests).
pub fn candidate_pairs_budgeted(world: &World, budget: usize) -> (Vec<Pair>, bool) {
    let n = world.bodies.len();
    if n < 2 {
        return (Vec::new(), false);
    }

    // Split bodies into bounded (grid-binned) bodies and infinite-AABB planes
    // (mirrors `candidate_pairs`).
    let mut bounded: Vec<BoundedBody> = Vec::with_capacity(n);
    let mut planes: Vec<(BodyId, bool)> = Vec::new();
    for (i, body) in world.bodies.iter().enumerate() {
        let id = BodyId(i as u32);
        let aabb = Aabb::of_body(body);
        let is_static = body.is_static();
        if aabb.min.is_finite() && aabb.max.is_finite() {
            bounded.push(BoundedBody { id, aabb, is_static });
        } else {
            planes.push((id, is_static));
        }
    }

    let cell_size = choose_cell_size(&bounded);

    let mut grid: std::collections::HashMap<Cell, Vec<usize>> = std::collections::HashMap::new();
    for (slot, b) in bounded.iter().enumerate() {
        let min_x = cell_index(b.aabb.min.x, cell_size);
        let max_x = cell_index(b.aabb.max.x, cell_size);
        let min_y = cell_index(b.aabb.min.y, cell_size);
        let max_y = cell_index(b.aabb.max.y, cell_size);
        let min_z = cell_index(b.aabb.min.z, cell_size);
        let max_z = cell_index(b.aabb.max.z, cell_size);
        for cx in min_x..=max_x {
            for cy in min_y..=max_y {
                for cz in min_z..=max_z {
                    grid.entry(Cell { x: cx, y: cy, z: cz }).or_default().push(slot);
                }
            }
        }
    }

    let mut pairs: Vec<Pair> = Vec::new();
    let mut cap_hit = false;

    // Planes first (deterministic id order), counting against the budget. The
    // emission helper returns `true` if the budget is now exhausted.
    'planes: for (pi, &(plane_id, plane_static)) in planes.iter().enumerate() {
        for b in &bounded {
            if !pair_eligible(plane_static, b.is_static) {
                continue;
            }
            pairs.push(ordered_pair(plane_id, b.id));
            if pairs.len() >= budget {
                cap_hit = true;
                break 'planes;
            }
        }
        for &(other_id, other_static) in planes.iter().skip(pi + 1) {
            if !pair_eligible(plane_static, other_static) {
                continue;
            }
            pairs.push(ordered_pair(plane_id, other_id));
            if pairs.len() >= budget {
                cap_hit = true;
                break 'planes;
            }
        }
    }

    // Grid cells in SORTED order (so the emitted prefix is deterministic, not
    // HashMap-iteration-order dependent). Within each cell, AABB-test slot pairs
    // in ascending order, SHORT-CIRCUITING the moment the budget is reached — so a
    // single overfull cell does NOT enumerate k²/2 pairs.
    if !cap_hit {
        let mut cells: Vec<&Cell> = grid.keys().collect();
        cells.sort();
        'cells: for cell in cells {
            let slots = &grid[cell];
            let m = slots.len();
            for a in 0..m {
                let ba = &bounded[slots[a]];
                for b in (a + 1)..m {
                    let bb = &bounded[slots[b]];
                    if !pair_eligible(ba.is_static, bb.is_static) {
                        continue;
                    }
                    if ba.aabb.overlaps(&bb.aabb) {
                        pairs.push(ordered_pair(ba.id, bb.id));
                        // Short-circuit enumeration: bound the AABB tests + emitted
                        // pairs to O(budget) even if this one cell holds every body.
                        if pairs.len() >= budget {
                            cap_hit = true;
                            break 'cells;
                        }
                    }
                }
            }
        }
    }

    pairs.sort();
    pairs.dedup();
    (pairs, cap_hit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::{Body, Material, Shape};

    #[test]
    fn aabb_overlap() {
        let a = Aabb::from_center_half(Vec3::ZERO, Vec3::splat(1.0));
        let b = Aabb::from_center_half(Vec3::new(1.5, 0.0, 0.0), Vec3::splat(1.0));
        let c = Aabb::from_center_half(Vec3::new(3.0, 0.0, 0.0), Vec3::splat(1.0));
        assert!(a.overlaps(&b));
        assert!(!a.overlaps(&c));
    }

    #[test]
    fn sphere_aabb() {
        let body = Body::new(Shape::Sphere { radius: 2.0 }, Vec3::new(1.0, 0.0, 0.0), 1.0, Material::default());
        let aabb = Aabb::of_body(&body);
        assert_eq!(aabb.min, Vec3::new(-1.0, -2.0, -2.0));
        assert_eq!(aabb.max, Vec3::new(3.0, 2.0, 2.0));
    }

    #[test]
    fn axis_aligned_box_aabb_is_half_extents() {
        let body = Body::new(Shape::Cuboid { half_extents: Vec3::new(1.0, 2.0, 3.0) }, Vec3::ZERO, 1.0, Material::default());
        let aabb = Aabb::of_body(&body);
        assert_eq!(aabb.min, Vec3::new(-1.0, -2.0, -3.0));
        assert_eq!(aabb.max, Vec3::new(1.0, 2.0, 3.0));
    }

    #[test]
    fn plane_aabb_is_infinite() {
        let body = Body::plane(Vec3::Y, 0.0);
        assert_eq!(Aabb::of_body(&body), Aabb::INFINITE);
    }

    #[test]
    fn pair_orders_ascending() {
        assert!(Pair(BodyId(0), BodyId(1)) < Pair(BodyId(0), BodyId(2)));
        assert!(Pair(BodyId(0), BodyId(5)) < Pair(BodyId(1), BodyId(2)));
    }

    // ---- candidate_pairs: helpers ----------------------------------------

    /// Spawn a unit-radius dynamic sphere at `pos`.
    fn sphere_at(w: &mut World, pos: Vec3) -> BodyId {
        w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, pos, 1.0, Material::default()))
    }

    /// Spawn a half-unit dynamic cuboid at `pos`.
    fn cube_at(w: &mut World, pos: Vec3) -> BodyId {
        w.spawn(Body::new(
            Shape::Cuboid { half_extents: Vec3::splat(0.5) },
            pos,
            1.0,
            Material::default(),
        ))
    }

    /// Assert the grid path produces EXACTLY the brute-force oracle's set (same
    /// pairs, same order). This is the SPEC §4 determinism guarantee.
    fn assert_matches_oracle(w: &World) {
        assert_eq!(
            candidate_pairs(w),
            candidate_pairs_brute_force(w),
            "grid broadphase must equal the brute-force oracle"
        );
    }

    // ---- candidate_pairs: behavior ---------------------------------------

    #[test]
    fn empty_world_has_no_pairs() {
        let w = World::new();
        assert!(candidate_pairs(&w).is_empty());
        assert!(candidate_pairs_brute_force(&w).is_empty());
    }

    #[test]
    fn single_body_has_no_pairs() {
        let mut w = World::new();
        sphere_at(&mut w, Vec3::ZERO);
        assert!(candidate_pairs(&w).is_empty());
        assert_matches_oracle(&w);
    }

    #[test]
    fn two_overlapping_bodies_are_reported() {
        // Two unit spheres 1.0 apart on X -> AABBs (±1) overlap.
        let mut w = World::new();
        let a = sphere_at(&mut w, Vec3::ZERO);
        let b = sphere_at(&mut w, Vec3::new(1.0, 0.0, 0.0));
        let pairs = candidate_pairs(&w);
        assert_eq!(pairs, vec![Pair(a, b)]);
        assert_matches_oracle(&w);
    }

    #[test]
    fn two_distant_bodies_are_not_reported() {
        // Two unit spheres 10 apart -> no AABB overlap.
        let mut w = World::new();
        sphere_at(&mut w, Vec3::ZERO);
        sphere_at(&mut w, Vec3::new(10.0, 0.0, 0.0));
        assert!(candidate_pairs(&w).is_empty());
        assert_matches_oracle(&w);
    }

    #[test]
    fn touching_aabbs_count_as_a_pair() {
        // Spheres exactly 2.0 apart: AABBs touch at x=1 (overlaps() is inclusive).
        let mut w = World::new();
        let a = sphere_at(&mut w, Vec3::ZERO);
        let b = sphere_at(&mut w, Vec3::new(2.0, 0.0, 0.0));
        assert_eq!(candidate_pairs(&w), vec![Pair(a, b)]);
        assert_matches_oracle(&w);
    }

    #[test]
    fn just_separated_aabbs_are_not_a_pair() {
        // Spheres 2.001 apart: AABBs separated by a hair -> no pair.
        let mut w = World::new();
        sphere_at(&mut w, Vec3::ZERO);
        sphere_at(&mut w, Vec3::new(2.001, 0.0, 0.0));
        assert!(candidate_pairs(&w).is_empty());
        assert_matches_oracle(&w);
    }

    #[test]
    fn pairs_are_deterministically_ordered() {
        // A scene that, binned, hits several cells in arbitrary HashMap order;
        // the emitted pair list must be ascending by (a, b) and stable.
        let mut w = World::new();
        // A clump of mutually-overlapping spheres near the origin.
        let _b0 = sphere_at(&mut w, Vec3::new(0.0, 0.0, 0.0));
        let _b1 = sphere_at(&mut w, Vec3::new(0.5, 0.0, 0.0));
        let _b2 = sphere_at(&mut w, Vec3::new(0.0, 0.5, 0.0));
        let _b3 = sphere_at(&mut w, Vec3::new(0.5, 0.5, 0.0));
        let pairs = candidate_pairs(&w);
        // Must be sorted ascending.
        let mut sorted = pairs.clone();
        sorted.sort();
        assert_eq!(pairs, sorted, "pairs must come out ascending");
        // Must be deduplicated.
        let mut deduped = pairs.clone();
        deduped.dedup();
        assert_eq!(pairs, deduped, "pairs must be deduplicated");
        // Repeated calls are identical (no RNG / iteration-order leak).
        assert_eq!(pairs, candidate_pairs(&w));
        assert_matches_oracle(&w);
    }

    #[test]
    fn hand_checked_layout_six_bodies() {
        // Four bodies form two well-separated overlapping clusters plus two
        // isolated bodies. Hand-checked expected candidate set:
        //   cluster L: ids 0,1 overlap (0.5 apart)
        //   cluster R: ids 2,3 overlap (0.5 apart, centered at x=20)
        //   id 4 isolated at x=50, id 5 isolated at x=-50
        // Expected pairs: (0,1), (2,3) only.
        let mut w = World::new();
        let b0 = sphere_at(&mut w, Vec3::new(0.0, 0.0, 0.0));
        let b1 = sphere_at(&mut w, Vec3::new(0.5, 0.0, 0.0));
        let b2 = sphere_at(&mut w, Vec3::new(20.0, 0.0, 0.0));
        let b3 = sphere_at(&mut w, Vec3::new(20.5, 0.0, 0.0));
        let _b4 = sphere_at(&mut w, Vec3::new(50.0, 0.0, 0.0));
        let _b5 = sphere_at(&mut w, Vec3::new(-50.0, 0.0, 0.0));
        let pairs = candidate_pairs(&w);
        assert_eq!(pairs, vec![Pair(b0, b1), Pair(b2, b3)]);
        assert_matches_oracle(&w);
    }

    #[test]
    fn static_static_pairs_are_skipped() {
        // Two overlapping static (zero-mass) cuboids must NOT pair.
        let mut w = World::new();
        w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(1.0) }, Vec3::ZERO, 0.0, Material::default()));
        w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(1.0) }, Vec3::new(0.5, 0.0, 0.0), 0.0, Material::default()));
        assert!(candidate_pairs(&w).is_empty(), "static-static must be skipped");
        assert_matches_oracle(&w);
    }

    #[test]
    fn dynamic_vs_static_overlap_is_a_pair() {
        // One static + one dynamic overlapping cuboid -> a pair (only the dynamic
        // body moves, but the contact is still needed).
        let mut w = World::new();
        let s = w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(1.0) }, Vec3::ZERO, 0.0, Material::default()));
        let d = w.spawn(Body::new(Shape::Cuboid { half_extents: Vec3::splat(1.0) }, Vec3::new(0.5, 0.0, 0.0), 1.0, Material::default()));
        assert_eq!(candidate_pairs(&w), vec![ordered_pair(s, d)]);
        assert_matches_oracle(&w);
    }

    #[test]
    fn plane_pairs_against_every_dynamic_body() {
        // An infinite ground plane + three dynamic spheres scattered far apart.
        // The plane (infinite AABB) must pair against ALL THREE regardless of
        // where they are; the spheres don't pair with each other (far apart).
        let mut w = World::new();
        let plane = w.spawn(Body::plane(Vec3::Y, 0.0));
        let a = sphere_at(&mut w, Vec3::new(0.0, 5.0, 0.0));
        let b = sphere_at(&mut w, Vec3::new(100.0, 5.0, 0.0));
        let c = sphere_at(&mut w, Vec3::new(-100.0, 5.0, 0.0));
        let pairs = candidate_pairs(&w);
        let mut expected = vec![
            ordered_pair(plane, a),
            ordered_pair(plane, b),
            ordered_pair(plane, c),
        ];
        expected.sort();
        assert_eq!(pairs, expected);
        assert_matches_oracle(&w);
    }

    #[test]
    fn plane_vs_plane_is_skipped() {
        // Two planes are both static -> static-static, no pair, no panic.
        let mut w = World::new();
        w.spawn(Body::plane(Vec3::Y, 0.0));
        w.spawn(Body::plane(Vec3::X, 0.0));
        assert!(candidate_pairs(&w).is_empty());
        assert_matches_oracle(&w);
    }

    #[test]
    fn large_box_spanning_many_cells_matches_oracle() {
        // A big dynamic box (half-extent 5) overlaps several small spheres; the
        // box spans many grid cells. The grid must still match the oracle (the
        // multi-cell occupancy + dedup is exercised here).
        let mut w = World::new();
        let big = w.spawn(Body::new(
            Shape::Cuboid { half_extents: Vec3::splat(5.0) },
            Vec3::ZERO,
            1.0,
            Material::default(),
        ));
        let s1 = sphere_at(&mut w, Vec3::new(4.0, 0.0, 0.0));
        let s2 = sphere_at(&mut w, Vec3::new(-4.0, 0.0, 0.0));
        let s3 = sphere_at(&mut w, Vec3::new(0.0, 4.0, 0.0));
        let _far = sphere_at(&mut w, Vec3::new(0.0, 100.0, 0.0));
        let pairs = candidate_pairs(&w);
        let mut expected = vec![
            ordered_pair(big, s1),
            ordered_pair(big, s2),
            ordered_pair(big, s3),
        ];
        expected.sort();
        assert_eq!(pairs, expected);
        assert_matches_oracle(&w);
    }

    #[test]
    fn dense_grid_lattice_matches_oracle() {
        // A 4x4x4 lattice of overlapping cubes (spacing 0.5, half-extent 0.5 ->
        // each neighbour AABB touches). Stresses the grid vs oracle equality on
        // a busy scene with many shared cells.
        let mut w = World::new();
        for ix in 0..4 {
            for iy in 0..4 {
                for iz in 0..4 {
                    cube_at(
                        &mut w,
                        Vec3::new(ix as f64 * 0.5, iy as f64 * 0.5, iz as f64 * 0.5),
                    );
                }
            }
        }
        assert_matches_oracle(&w);
        // Sanity: a lattice this dense produces a non-trivial candidate set.
        assert!(!candidate_pairs(&w).is_empty());
    }

    #[test]
    fn negative_coordinate_cell_binning_matches_oracle() {
        // Bodies straddling the origin on the negative side: exercises the
        // floor()-based cell_index across the lattice boundary at 0.
        let mut w = World::new();
        sphere_at(&mut w, Vec3::new(-0.1, 0.0, 0.0));
        sphere_at(&mut w, Vec3::new(0.1, 0.0, 0.0));
        sphere_at(&mut w, Vec3::new(-3.0, -3.0, -3.0));
        sphere_at(&mut w, Vec3::new(-3.5, -3.0, -3.0));
        assert_matches_oracle(&w);
    }

    #[test]
    fn cell_index_floors_across_origin() {
        assert_eq!(cell_index(0.5, 1.0), 0);
        assert_eq!(cell_index(-0.5, 1.0), -1);
        assert_eq!(cell_index(1.5, 1.0), 1);
        assert_eq!(cell_index(-1.0, 1.0), -1);
        assert_eq!(cell_index(2.0, 2.0), 1);
    }

    #[test]
    fn ordered_pair_upholds_convention() {
        assert_eq!(ordered_pair(BodyId(3), BodyId(1)), Pair(BodyId(1), BodyId(3)));
        assert_eq!(ordered_pair(BodyId(1), BodyId(3)), Pair(BodyId(1), BodyId(3)));
    }

    // ---- candidate_pairs_budgeted: bounded-work upstream guard ------------

    #[test]
    fn budgeted_equals_unbudgeted_when_budget_does_not_bite() {
        // With a budget far above the candidate count, the budgeted path must
        // return EXACTLY the unbudgeted set (same pairs, same order) and NOT flag
        // a cap hit. This is the contract that keeps normal scenes untouched.
        let mut w = World::new();
        // A small clump plus a plane — a realistic mixed scene.
        w.spawn(Body::plane(Vec3::Y, 0.0));
        sphere_at(&mut w, Vec3::new(0.0, 1.0, 0.0));
        sphere_at(&mut w, Vec3::new(0.5, 1.0, 0.0));
        sphere_at(&mut w, Vec3::new(0.0, 1.5, 0.0));
        sphere_at(&mut w, Vec3::new(50.0, 1.0, 0.0)); // far loner (plane-only pair)

        let (budgeted, hit) = candidate_pairs_budgeted(&w, 1_000_000);
        assert!(!hit, "a generous budget must not bite on a small scene");
        assert_eq!(
            budgeted,
            candidate_pairs(&w),
            "below-budget budgeted output must equal the unbudgeted set exactly"
        );
    }

    #[test]
    fn budgeted_short_circuits_a_collapsed_cell_deterministically() {
        // THE worst case the upstream bound exists for: every body collapsed onto
        // one point => all in ONE grid cell => the unbudgeted path would do ~k²/2
        // AABB tests. The budgeted path must STOP at the budget (bounded
        // enumeration), SIGNAL the cap, and do so DETERMINISTICALLY (same prefix
        // every run / on an independently-built identical scene).
        let n = 200usize;
        let budget = 500usize; // << n²/2 ≈ 19900, so the cap must bite
        let build = || {
            let mut w = World::new();
            for _ in 0..n {
                w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, Material::default()));
            }
            w
        };
        let w = build();

        let (pairs, hit) = candidate_pairs_budgeted(&w, budget);
        assert!(hit, "a fully-collapsed scene must trip the pair budget");
        // Bounded enumeration: the emitted set never exceeds the budget (sort+dedup
        // can only shrink it; in a single collapsed cell every pair is unique).
        assert!(
            pairs.len() <= budget,
            "emitted pairs ({}) must not exceed the budget ({budget})",
            pairs.len()
        );
        // Deterministic prefix: an independently-built identical scene yields the
        // SAME truncated pair list and the SAME signal.
        let w2 = build();
        let (pairs2, hit2) = candidate_pairs_budgeted(&w2, budget);
        assert_eq!(pairs, pairs2, "budgeted prefix must be deterministic");
        assert_eq!(hit, hit2);
        // Output still upholds the ascending+deduped contract.
        let mut sorted = pairs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(pairs, sorted, "budgeted output must stay ascending + deduped");
    }

    #[test]
    fn budgeted_enumeration_is_bounded_not_quadratic_at_scale() {
        // The whole point of the UPSTREAM bound: a single overfull cell must not
        // enumerate O(n²) AABB tests. We can't time-assert deterministically in a
        // unit test, but we CAN assert the work is bounded by completing a very
        // large collapsed scene near the body cap near-instantly and confirming
        // the emitted set is bounded by the (small) budget — an O(n²) enumeration
        // of 4096 collapsed bodies (~8.4M tests) would be visibly slower and would
        // ignore the cap.
        let n = 4096usize; // the MAX_BODIES cap
        let budget = 1024usize;
        let mut w = World::new();
        for _ in 0..n {
            w.spawn(Body::new(Shape::Sphere { radius: 1.0 }, Vec3::ZERO, 1.0, Material::default()));
        }
        let (pairs, hit) = candidate_pairs_budgeted(&w, budget);
        assert!(hit, "4096 collapsed bodies must trip the pair budget");
        assert!(
            pairs.len() <= budget,
            "enumeration must be bounded by the budget, not O(n²): got {}",
            pairs.len()
        );
    }
}
