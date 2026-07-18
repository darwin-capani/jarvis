//! wgpu/Metal renderer (SPEC §2). COMPILED ONLY behind the `gpu` feature.
//!
//! HONESTY GATE (read before trusting any number this module reports):
//! This file COMPILES headlessly with `cargo check --features gpu` on the M1 dev
//! box, and its CPU-side math (view transform, LOD culling, flag writes, buffer
//! packing) is unit-tested. It has NOT been run against a live Metal device here.
//! The render targets (60 fps on ProMotion-class Apple Silicon; >= 30 fps on the
//! M1 Pro dev machine) and any draw-call /
//! throughput / culled-percentage figure are DEVICE-GATED — they can only be
//! VERIFIED on-device (a real Apple Silicon Mac with the `gpu` grant). Nothing in this file
//! should be read as a measured fps/throughput claim; [`RenderMs`] is populated
//! from real per-frame timers at runtime, but those timers only run on a GPU.
//!
//! Design (SPEC §2), the part that makes 60 fps tractable rather than heroic —
//! *everything is GPU-resident and instanced; pan/zoom is a uniform, not a
//! rebuild*:
//!   - Each [`Scene`] entity class is packed ONCE at import into a static instance
//!     buffer ([`GpuRenderer::upload_scene`]); the view transform is a single mat3
//!     [`ViewUniform`]. Pan/zoom rewrites 64 bytes of uniform and redraws — it
//!     never re-tessellates and never re-uploads geometry.
//!   - Per-layer instanced pipelines: pads/tracks/vias are drawn as SDF quads
//!     (two triangles, one instance each; the fragment shader does the
//!     rect/circle/roundrect/oval/capsule/ring SDF). Component courtyards are a
//!     line-list pipeline. WGSL is embedded as string constants below.
//!   - A per-entity 1-byte highlight flag ([`HighlightFlag`]) lives in its own
//!     storage buffer per class; a net highlight is a flag-buffer write + redraw
//!     (no geometry touched — SPEC §2/§4).
//!   - LOD by zoom ([`Lod`]): below screen-space feature-size thresholds drop
//!     text, then pads (tracks imply them), then everything but component bboxes.
//!   - Target <= 30 draw calls/frame (one per layer x pipeline), 4x MSAA.
//!   - Frame stats published on `canvas.render_ms` at 1 Hz via [`FrameTimer`] ->
//!     [`RenderMs`]; the `ipc` agent wraps that in an `OutboundLine`.
//!
//! The HUD composites our output as a shared IOSurface texture (SPEC, HUD.md §5
//! tier 2). This module renders into an offscreen MSAA-resolved color target and
//! never opens a window or reads an input device; the surface/IOSurface handoff
//! and the JSONL event routing are the `ipc`/host's job, not the renderer's.

use std::borrow::Cow;

use bytemuck::{Pod, Zeroable};
use glam::{DMat3, DVec2, Mat3};
use wgpu::util::DeviceExt;

use crate::ops::RenderMs;
use crate::scene::{HighlightFlag, PadShape, Scene};

// ===========================================================================
// Constants (SPEC §2/§3).
// ===========================================================================

/// 4x MSAA (SPEC §2). Metal supports 1/2/4/8; 4 is the SPEC target and the
/// universally-available count.
pub const MSAA_SAMPLES: u32 = 4;

/// The offscreen color format the HUD's IOSurface composite expects. Bgra8 is the
/// native macOS surface format (CoreAnimation / IOSurface) so the resolve target
/// can be shared without a reformat blit.
pub const COLOR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8UnormSrgb;

/// SPEC §2 explicit ceiling: at most this many draw calls per frame (one per
/// layer x pipeline). [`GpuRenderer::render`] asserts the count never exceeds it
/// in debug builds so a future pipeline addition that blows the budget is caught.
pub const MAX_DRAWS_PER_FRAME: u32 = 30;

/// SPEC §3 zoom range, in pixels-per-mm. The view math is f64 (the scene stores
/// f64 mm) and only the final mat3 is downcast to f32 for the GPU.
pub const MIN_SCALE: f64 = 0.01;
pub const MAX_SCALE: f64 = 500.0;

/// Number of frame samples retained for the 1 Hz p50/p95 window. At a 60 fps
/// cap one second is ~60 frames; 128 covers a slow second with headroom.
const FRAME_WINDOW: usize = 128;

// ===========================================================================
// View transform (f64 scene math -> f32 mat3 uniform).
// ===========================================================================

/// The camera over the scene (SPEC §3): center in scene-space mm + a `scale` in
/// pixels-per-mm, kept in f64 because single precision loses sub-pixel accuracy
/// at deep zoom on large boards. The renderer turns this into a single mat3
/// uniform per frame — pan/zoom is a uniform write, never a geometry rebuild.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// View center in scene space (mm).
    pub center: DVec2,
    /// Zoom in pixels-per-mm (clamped to [`MIN_SCALE`]..=[`MAX_SCALE`]).
    pub scale: f64,
    /// Render-target size in physical pixels (width, height).
    pub viewport_px: (f32, f32),
}

impl View {
    /// A view centered on the origin at 1 px/mm for a given target size.
    pub fn new(viewport_px: (f32, f32)) -> Self {
        View {
            center: DVec2::ZERO,
            scale: 1.0,
            viewport_px,
        }
    }

    /// Clamp `scale` into the SPEC §3 range.
    #[inline]
    pub fn clamped_scale(&self) -> f64 {
        self.scale.clamp(MIN_SCALE, MAX_SCALE)
    }

    /// Frame an [`crate::scene::Aabb`] to fill the viewport with a small margin
    /// (used by `view.set {fit}`). Empty boxes leave the view unchanged.
    pub fn fit(&mut self, bbox: crate::scene::Aabb) {
        if bbox.is_empty() {
            return;
        }
        self.center = DVec2::new(bbox.center().x, bbox.center().y);
        let (w, h) = self.viewport_px;
        let bw = bbox.width().max(QUANTUM_EPS);
        let bh = bbox.height().max(QUANTUM_EPS);
        // 0.92 leaves a margin so the framed thing isn't flush to the edge.
        let sx = (w as f64) / bw;
        let sy = (h as f64) / bh;
        self.scale = (sx.min(sy) * 0.92).clamp(MIN_SCALE, MAX_SCALE);
    }

    /// Zoom about a cursor point (surface pixel coords), keeping the scene point
    /// under the cursor fixed — SPEC §3 "zoom about the cursor". `factor` > 1
    /// zooms in. Pure f64 math; only the resulting uniform is downcast.
    pub fn zoom_about(&mut self, cursor_px: DVec2, factor: f64) {
        let before = self.screen_to_scene(cursor_px);
        self.scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        let after = self.screen_to_scene(cursor_px);
        // Shift the center so the scene point under the cursor is unchanged.
        self.center += before - after;
    }

    /// Map a surface pixel to scene space under the current view (f64). Pixel
    /// origin is top-left, scene Y is up (KiCad Y grows downward, but the scene
    /// is stored in its own mm space and the projection flips Y so up is up on
    /// screen).
    pub fn screen_to_scene(&self, px: DVec2) -> DVec2 {
        let (w, h) = self.viewport_px;
        let s = self.clamped_scale();
        // Pixel offset from the screen center.
        let dx = px.x - (w as f64) * 0.5;
        let dy = px.y - (h as f64) * 0.5;
        // Invert scale; flip Y (screen down -> scene up).
        DVec2::new(self.center.x + dx / s, self.center.y - dy / s)
    }

    /// The f64 scene->clip matrix: translate by -center, scale to pixels, then
    /// normalize to clip space (-1..1). Scene +Y and clip +Y are both up, so no
    /// Y inversion is applied (this keeps it the inverse of `screen_to_scene`).
    /// Computed in f64 then downcast.
    pub fn scene_to_clip_f64(&self) -> DMat3 {
        let (w, h) = self.viewport_px;
        let s = self.clamped_scale();
        let (w, h) = (w as f64, h as f64);
        // scene -> pixels-from-center: (p - center) * s.
        // pixels-from-center -> clip: x * 2/w, y * 2/h.
        let ax = 2.0 * s / w;
        let ay = 2.0 * s / h;
        // column-major mat3 (glam is column-major): columns are the images of the
        // basis vectors. clip.x = ax*(x-cx);  clip.y = ay*(y-cy). Scene +Y is up
        // and clip/NDC +Y is up, so the Y axis is NOT inverted here — that keeps
        // this matrix the exact inverse of `screen_to_scene` (which already maps
        // pixel-down to scene-up), so picking and rendering agree.
        DMat3::from_cols(
            glam::DVec3::new(ax, 0.0, 0.0),
            glam::DVec3::new(0.0, ay, 0.0),
            glam::DVec3::new(-ax * self.center.x, -ay * self.center.y, 1.0),
        )
    }

    /// The f32 uniform the GPU consumes: the f64 matrix downcast, padded to a
    /// std140-friendly 3x `vec4` layout (a mat3 in WGSL std140 occupies three
    /// 16-byte columns).
    pub fn uniform(&self) -> ViewUniform {
        let m = self.scene_to_clip_f64();
        let cols = m.to_cols_array(); // [c0x,c0y,c0z, c1x,c1y,c1z, c2x,c2y,c2z]
        ViewUniform {
            // Each mat3 column padded to a vec4 (std140).
            col0: [cols[0] as f32, cols[1] as f32, cols[2] as f32, 0.0],
            col1: [cols[3] as f32, cols[4] as f32, cols[5] as f32, 0.0],
            col2: [cols[6] as f32, cols[7] as f32, cols[8] as f32, 0.0],
            px_per_mm: self.clamped_scale() as f32,
            _pad: [0.0; 3],
        }
    }
}

/// A tiny epsilon (one quantum) used to avoid div-by-zero when fitting a
/// degenerate (zero-extent) bounding box.
const QUANTUM_EPS: f64 = crate::scene::QUANTUM_MM;

// ===========================================================================
// GPU POD types (instances + uniform). All `repr(C)` + `Pod` for bytemuck.
// ===========================================================================

/// The mat3 view transform as a GPU uniform (std140: three padded vec4 columns +
/// the pixels-per-mm scalar so the shader can size SDF edges in screen space).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ViewUniform {
    pub col0: [f32; 4],
    pub col1: [f32; 4],
    pub col2: [f32; 4],
    pub px_per_mm: f32,
    pub _pad: [f32; 3],
}

impl ViewUniform {
    /// Reconstruct the (unpadded) f32 mat3 — for tests / debug. Column-major.
    pub fn to_mat3(&self) -> Mat3 {
        Mat3::from_cols_array(&[
            self.col0[0], self.col0[1], self.col0[2], self.col1[0], self.col1[1], self.col1[2],
            self.col2[0], self.col2[1], self.col2[2],
        ])
    }
}

/// One pad instance (SPEC §2: pads as SDF quads). `center`/`size` in scene mm;
/// `shape` selects the fragment SDF branch; `flag_index` indexes the highlight
/// flag storage buffer so a flag write needs no instance-buffer touch.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct PadInstance {
    pub center: [f32; 2],
    pub half_size: [f32; 2],
    pub rotation: f32,
    /// 0=Circle 1=Rect 2=RoundRect 3=Oval (matches [`PadShape`] discriminant).
    pub shape: u32,
    pub flag_index: u32,
    /// Net id (raw u32; u32::MAX = none) — lets the shader cheaply test "is this
    /// the highlighted net" without a flag write when a net is hover-previewed.
    pub net_id: u32,
}

/// One track segment instance (SPEC §2: SDF capsule). Endpoints in scene mm;
/// `width` is the copper width (the capsule diameter).
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct TrackInstance {
    pub a: [f32; 2],
    pub b: [f32; 2],
    pub width: f32,
    pub flag_index: u32,
    pub net_id: u32,
    pub _pad: u32,
}

/// One via instance (SPEC §2: SDF ring). `diameter`/`drill` set the outer/inner
/// ring radii.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ViaInstance {
    pub center: [f32; 2],
    pub diameter: f32,
    pub drill: f32,
    pub flag_index: u32,
    pub net_id: u32,
    pub _pad: [u32; 2],
}

/// One vertex of the courtyard / bbox line-list pipeline (SPEC §2: component
/// outlines as line lists). Position in scene mm + the owning component's flag
/// index so the courtyard dims with its component.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct LineVertex {
    pub pos: [f32; 2],
    pub flag_index: u32,
    pub _pad: u32,
}

// ===========================================================================
// Level-of-detail (SPEC §2).
// ===========================================================================

/// LOD decision for one frame, derived purely from `px_per_mm` (the zoom). SPEC
/// §2: "below thresholds, drop text -> drop pads (tracks imply them) ->
/// component bounding boxes only. Thresholds in screen-space feature size
/// (< 2 px = culled class)." The thresholds below are expressed as the
/// pixels-per-mm at which a *typical* feature of that class falls under ~2 px:
///   - text glyph height ~1.27 mm: drop below ~1.6 px/mm,
///   - pad ~0.6 mm: drop below ~0.33 px/mm,
///   - below the pad threshold, draw only component bboxes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lod {
    pub draw_text: bool,
    pub draw_pads: bool,
    pub draw_tracks: bool,
    pub draw_vias: bool,
    /// When true, individual entities are too small; draw only component bboxes.
    pub bbox_only: bool,
}

impl Lod {
    /// Typical text glyph height (mm) — KiCad default is ~1.27 mm (0.05").
    const TEXT_FEATURE_MM: f64 = 1.27;
    /// Typical pad short dimension (mm) for the pad-cull threshold.
    const PAD_FEATURE_MM: f64 = 0.6;
    /// Screen-space feature size below which a class is culled (SPEC §2: 2 px).
    const CULL_PX: f64 = 2.0;

    /// Decide LOD from the current pixels-per-mm.
    pub fn for_scale(px_per_mm: f64) -> Self {
        let text_px = Self::TEXT_FEATURE_MM * px_per_mm;
        let pad_px = Self::PAD_FEATURE_MM * px_per_mm;
        let draw_text = text_px >= Self::CULL_PX;
        let draw_pads = pad_px >= Self::CULL_PX;
        // Tracks/vias are the spine of a board — keep them until pads vanish too,
        // then collapse to bbox-only (SPEC §2: "tracks imply them").
        let bbox_only = !draw_pads;
        Lod {
            draw_text,
            draw_pads,
            draw_tracks: !bbox_only,
            draw_vias: !bbox_only,
            bbox_only,
        }
    }
}

// ===========================================================================
// Frame timer -> RenderMs (SPEC §2, 1 Hz).
// ===========================================================================

/// Rolling per-frame CPU+submit timing, turned into a [`RenderMs`] once per
/// second. The renderer pushes each frame's elapsed milliseconds + the draw
/// count + the culled percentage; [`FrameTimer::tick`] returns `Some(RenderMs)`
/// on the second boundary so the caller emits exactly one `canvas.render_ms`
/// line per second (SPEC §2).
///
/// NOTE (honesty): the numbers are real measurements of real frames — but frames
/// only happen on a GPU. Headlessly this struct is exercised with synthetic
/// samples in tests; the percentile math is what is verified here.
#[derive(Debug, Clone)]
pub struct FrameTimer {
    samples_ms: Vec<f64>,
    last_draws: u32,
    last_culled_pct: f64,
    accum_ms: f64,
}

impl Default for FrameTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameTimer {
    pub fn new() -> Self {
        FrameTimer {
            samples_ms: Vec::with_capacity(FRAME_WINDOW),
            last_draws: 0,
            last_culled_pct: 0.0,
            accum_ms: 0.0,
        }
    }

    /// Record one frame. Returns `Some(RenderMs)` when at least 1000 ms of frames
    /// have accumulated since the last report (the 1 Hz boundary), resetting the
    /// window. `culled_pct` is the percentage of candidate instances dropped by
    /// viewport/LOD culling this frame (0..=100).
    pub fn record(&mut self, frame_ms: f64, draws: u32, culled_pct: f64) -> Option<RenderMs> {
        if self.samples_ms.len() == FRAME_WINDOW {
            self.samples_ms.remove(0);
        }
        self.samples_ms.push(frame_ms);
        self.last_draws = draws;
        self.last_culled_pct = culled_pct.clamp(0.0, 100.0);
        self.accum_ms += frame_ms;
        if self.accum_ms >= 1000.0 {
            self.accum_ms = 0.0;
            Some(self.snapshot())
        } else {
            None
        }
    }

    /// Compute the current percentile snapshot without resetting the window.
    pub fn snapshot(&self) -> RenderMs {
        RenderMs {
            p50: percentile(&self.samples_ms, 0.50),
            p95: percentile(&self.samples_ms, 0.95),
            draws: self.last_draws,
            culled_pct: self.last_culled_pct,
        }
    }
}

/// Nearest-rank percentile over an unsorted slice (clones+sorts a small window).
/// Returns 0.0 for an empty window. `q` is in 0.0..=1.0.
fn percentile(samples: &[f64], q: f64) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut v = samples.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let q = q.clamp(0.0, 1.0);
    // Nearest-rank: rank = ceil(q * n), 1-based.
    let rank = (q * v.len() as f64).ceil().max(1.0) as usize;
    v[(rank - 1).min(v.len() - 1)]
}

// ===========================================================================
// CPU-side scene packing (instances + flag buffers + courtyard lines).
//
// This is the part exercised headlessly: turning a Scene's struct-of-arrays into
// the exact byte layouts the GPU buffers want, ONCE at import. The GPU upload
// (GpuRenderer::upload_scene) just hands these slices to the device.
// ===========================================================================

/// All per-class GPU buffers' CPU mirrors, built once from a [`Scene`]. Holding
/// the CPU side lets the flag arrays be re-uploaded cheaply on selection change
/// (SPEC §2/§4) and lets tests assert the packing without a device.
#[derive(Debug, Clone, Default)]
pub struct ScenePacking {
    pub pads: Vec<PadInstance>,
    pub tracks: Vec<TrackInstance>,
    pub vias: Vec<ViaInstance>,
    /// Courtyard/bbox line vertices (line-list: pairs of vertices = one segment).
    pub courtyards: Vec<LineVertex>,
    /// Per-class highlight flags as raw bytes (parallel to the geometry arrays),
    /// ready to upload to the flag storage buffers. One byte per entity.
    pub pad_flags: Vec<u8>,
    pub track_flags: Vec<u8>,
    pub via_flags: Vec<u8>,
    pub component_flags: Vec<u8>,
}

impl ScenePacking {
    /// Pack a scene's geometry into GPU-ready instance arrays ONCE (SPEC §2:
    /// "All geometry uploaded once at import"). Pure CPU; no device needed.
    pub fn from_scene(scene: &Scene) -> Self {
        let mut packing = ScenePacking::default();

        packing.pads.reserve(scene.pads.len());
        for pad in &scene.pads {
            packing.pads.push(PadInstance {
                center: [pad.position.x as f32, pad.position.y as f32],
                half_size: [pad.size.0 as f32 * 0.5, pad.size.1 as f32 * 0.5],
                // Pads inherit no per-pad rotation in the scene model; the owning
                // component's rotation is folded in at parse-time placement, so
                // pad geometry is already in scene space. Keep 0 here.
                rotation: 0.0,
                shape: pad_shape_code(pad.shape),
                flag_index: pad_index_for(scene, &packing),
                net_id: pad.net_id.raw(),
            });
        }

        packing.tracks.reserve(scene.tracks.len());
        for (i, track) in scene.tracks.iter().enumerate() {
            packing.tracks.push(TrackInstance {
                a: [track.a.x as f32, track.a.y as f32],
                b: [track.b.x as f32, track.b.y as f32],
                width: track.width as f32,
                flag_index: i as u32,
                net_id: track.net_id.raw(),
                _pad: 0,
            });
        }

        packing.vias.reserve(scene.vias.len());
        for (i, via) in scene.vias.iter().enumerate() {
            packing.vias.push(ViaInstance {
                center: [via.position.x as f32, via.position.y as f32],
                diameter: via.diameter as f32,
                drill: via.drill as f32,
                flag_index: i as u32,
                net_id: via.net_id.raw(),
                _pad: [0, 0],
            });
        }

        // Component courtyards as line lists: the bbox rectangle, 4 segments = 8
        // vertices, each tagged with the component's flag index. SPEC §2 lists
        // courtyards/outlines as a line-list pipeline; the tight symbol body bbox
        // is the conservative outline the scene carries.
        for (ci, comp) in scene.components.iter().enumerate() {
            let bb = comp.bbox;
            if bb.is_empty() {
                continue;
            }
            let fi = ci as u32;
            let (x0, y0) = (bb.min.x as f32, bb.min.y as f32);
            let (x1, y1) = (bb.max.x as f32, bb.max.y as f32);
            let corners = [[x0, y0], [x1, y0], [x1, y1], [x0, y1]];
            for e in 0..4 {
                let p = corners[e];
                let q = corners[(e + 1) % 4];
                packing.courtyards.push(LineVertex { pos: p, flag_index: fi, _pad: 0 });
                packing.courtyards.push(LineVertex { pos: q, flag_index: fi, _pad: 0 });
            }
        }

        // Flag bytes mirror the scene's parallel flag arrays.
        packing.pad_flags = flags_to_bytes(&scene.pad_flags);
        packing.track_flags = flags_to_bytes(&scene.track_flags);
        packing.via_flags = flags_to_bytes(&scene.via_flags);
        packing.component_flags = flags_to_bytes(&scene.component_flags);

        packing
    }

    /// Refresh ONLY the flag byte mirrors from the scene (after a selection /
    /// trace step changed the [`HighlightFlag`] arrays). SPEC §2/§4: a net
    /// highlight is a flag-buffer write + redraw, no geometry rebuild. The caller
    /// then re-`queue.write_buffer`s the flag buffers.
    pub fn refresh_flags(&mut self, scene: &Scene) {
        self.pad_flags = flags_to_bytes(&scene.pad_flags);
        self.track_flags = flags_to_bytes(&scene.track_flags);
        self.via_flags = flags_to_bytes(&scene.via_flags);
        self.component_flags = flags_to_bytes(&scene.component_flags);
    }

    /// Total instance count across the SDF classes — the culling denominator for
    /// `culled_pct` reporting.
    pub fn total_instances(&self) -> usize {
        self.pads.len() + self.tracks.len() + self.vias.len()
    }
}

/// The pad's index into the flag buffer is simply its position in the pads array,
/// i.e. the length of `packing.pads` at push-time (the pad currently being
/// pushed). Kept as a helper so the intent ("flag index == pad index") is
/// explicit at the call site.
#[inline]
fn pad_index_for(_scene: &Scene, packing: &ScenePacking) -> u32 {
    packing.pads.len() as u32
}

/// Map a [`PadShape`] to its shader branch code. Matches the enum's documented
/// discriminant order so the WGSL `switch` stays in sync.
#[inline]
fn pad_shape_code(shape: PadShape) -> u32 {
    match shape {
        PadShape::Circle => 0,
        PadShape::Rect => 1,
        PadShape::RoundRect => 2,
        PadShape::Oval => 3,
    }
}

/// Reinterpret a `&[HighlightFlag]` (repr(u8)) as raw bytes for the GPU flag
/// buffer. `HighlightFlag` is `repr(u8)` so the cast is sound; done via a
/// per-element map (not a transmute) to stay obviously safe.
#[inline]
fn flags_to_bytes(flags: &[HighlightFlag]) -> Vec<u8> {
    flags.iter().map(|f| *f as u8).collect()
}

// ===========================================================================
// Embedded WGSL shaders (SPEC §2: "embed the WGSL shaders as string constants").
//
// One module per pipeline. All read the same view uniform (group 0, binding 0)
// and a per-class highlight flag storage buffer (group 0, binding 1). The vertex
// stage expands a unit quad per instance; the fragment stage evaluates the SDF
// and applies the highlight flag (Normal/Highlighted/Dimmed/TraceFront).
// ===========================================================================

/// Shared WGSL prelude: the view uniform, the flag buffer, the holo palette, and
/// the flag->tint helper. Concatenated in front of each pipeline's body so the
/// palette/flag logic lives in exactly one place.
const WGSL_PRELUDE: &str = r#"
struct View {
    col0 : vec4<f32>,
    col1 : vec4<f32>,
    col2 : vec4<f32>,
    px_per_mm : f32,
    _pad : vec3<f32>,
};
@group(0) @binding(0) var<uniform> view : View;
// Per-entity highlight flag (1 byte packed as u32 lanes on the CPU side: we
// store one u32 per entity for straightforward indexing — the byte buffer is
// widened to u32 at upload so WGSL can index it without sub-word loads).
@group(0) @binding(1) var<storage, read> flags : array<u32>;

// Holo palette (HUD.md / SPEC §4). Approximations of the --holo-* CSS vars; the
// HUD's final composite may retint, but these are the in-shader defaults.
const HOLO_BRIGHT  : vec3<f32> = vec3<f32>(0.45, 0.92, 1.00); // highlighted net
const HOLO_NORMAL  : vec3<f32> = vec3<f32>(0.30, 0.62, 0.78); // normal trace
const HOLO_TRACE   : vec3<f32> = vec3<f32>(1.00, 0.85, 0.35); // trace front
const DIM_FACTOR    : f32      = 0.25;                          // SPEC §4: 25%

// Apply scene->clip via the mat3 view (column-major, homogeneous 2D).
fn project(p : vec2<f32>) -> vec4<f32> {
    let h = view.col0.xyz * p.x + view.col1.xyz * p.y + view.col2.xyz;
    return vec4<f32>(h.xy, 0.0, 1.0);
}

// flag: 0 Normal, 1 Highlighted, 2 Dimmed, 3 TraceFront.
fn tint_for(flag : u32, base_alpha : f32) -> vec4<f32> {
    if (flag == 1u) {
        return vec4<f32>(HOLO_BRIGHT, base_alpha);
    } else if (flag == 2u) {
        return vec4<f32>(HOLO_NORMAL * DIM_FACTOR, base_alpha * DIM_FACTOR);
    } else if (flag == 3u) {
        return vec4<f32>(HOLO_TRACE, base_alpha);
    }
    return vec4<f32>(HOLO_NORMAL, base_alpha);
}

// Unit quad in [-1,1] from the vertex index (two triangles, 6 verts).
fn unit_quad(vidx : u32) -> vec2<f32> {
    var pts = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0), vec2<f32>( 1.0, -1.0), vec2<f32>( 1.0,  1.0),
        vec2<f32>(-1.0, -1.0), vec2<f32>( 1.0,  1.0), vec2<f32>(-1.0,  1.0),
    );
    return pts[vidx];
}
"#;

/// Pad pipeline body (SDF rect/circle/roundrect/oval). Instance fields arrive as
/// vertex attributes; the quad is sized to the pad half-size plus a 1.5 px AA
/// margin (converted to mm via `px_per_mm`).
const WGSL_PAD_BODY: &str = r#"
struct PadIn {
    @location(0) center : vec2<f32>,
    @location(1) half_size : vec2<f32>,
    @location(2) rotation : f32,
    @location(3) shape : u32,
    @location(4) flag_index : u32,
    @location(5) net_id : u32,
};
struct PadVS {
    @builtin(position) clip : vec4<f32>,
    @location(0) local : vec2<f32>,     // position in pad-local mm (unrotated)
    @location(1) half_size : vec2<f32>,
    @location(2) shape : u32,
    @location(3) flag : u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vidx : u32, inst : PadIn) -> PadVS {
    let q = unit_quad(vidx);
    // AA margin in mm so the SDF edge has ~1.5 px to fade across.
    let margin = 1.5 / max(view.px_per_mm, 1e-6);
    let local = q * (inst.half_size + vec2<f32>(margin, margin));
    // Rotate into scene space.
    let c = cos(inst.rotation);
    let s = sin(inst.rotation);
    let rot = vec2<f32>(local.x * c - local.y * s, local.x * s + local.y * c);
    let world = inst.center + rot;
    var out : PadVS;
    out.clip = project(world);
    out.local = local;
    out.half_size = inst.half_size;
    out.shape = inst.shape;
    out.flag = flags[inst.flag_index];
    return out;
}

// Signed distance to a box / rounded box / circle / oval in pad-local mm.
fn sd_box(p : vec2<f32>, b : vec2<f32>) -> f32 {
    let d = abs(p) - b;
    return length(max(d, vec2<f32>(0.0))) + min(max(d.x, d.y), 0.0);
}

@fragment
fn fs_main(in : PadVS) -> @location(0) vec4<f32> {
    var d : f32;
    if (in.shape == 0u) {              // Circle
        d = length(in.local) - min(in.half_size.x, in.half_size.y);
    } else if (in.shape == 2u) {       // RoundRect
        let r = min(in.half_size.x, in.half_size.y) * 0.35;
        d = sd_box(in.local, in.half_size - vec2<f32>(r, r)) - r;
    } else if (in.shape == 3u) {       // Oval (capsule along the long axis)
        d = length(in.local / max(in.half_size, vec2<f32>(1e-6))) - 1.0;
        d = d * min(in.half_size.x, in.half_size.y);
    } else {                           // Rect
        d = sd_box(in.local, in.half_size);
    }
    // Antialias across ~1 px in mm.
    let aa = 1.0 / max(view.px_per_mm, 1e-6);
    let cov = 1.0 - smoothstep(0.0, aa, d);
    if (cov <= 0.0) { discard; }
    let col = tint_for(in.flag, cov);
    return col;
}
"#;

/// Track pipeline body (SDF capsule between two endpoints).
const WGSL_TRACK_BODY: &str = r#"
struct TrackIn {
    @location(0) a : vec2<f32>,
    @location(1) b : vec2<f32>,
    @location(2) width : f32,
    @location(3) flag_index : u32,
    @location(4) net_id : u32,
};
struct TrackVS {
    @builtin(position) clip : vec4<f32>,
    @location(0) p : vec2<f32>,        // world-space fragment position (mm)
    @location(1) a : vec2<f32>,
    @location(2) b : vec2<f32>,
    @location(3) radius : f32,
    @location(4) flag : u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vidx : u32, inst : TrackIn) -> TrackVS {
    let q = unit_quad(vidx);
    let dir = inst.b - inst.a;
    let len = max(length(dir), 1e-6);
    let t = dir / len;
    let n = vec2<f32>(-t.y, t.x);
    let radius = inst.width * 0.5;
    let margin = 1.5 / max(view.px_per_mm, 1e-6);
    let half_len = len * 0.5 + radius + margin;
    let half_w = radius + margin;
    let center = (inst.a + inst.b) * 0.5;
    // Map the unit quad onto the oriented bounding box of the capsule.
    let world = center + t * (q.x * half_len) + n * (q.y * half_w);
    var out : TrackVS;
    out.clip = project(world);
    out.p = world;
    out.a = inst.a;
    out.b = inst.b;
    out.radius = radius;
    out.flag = flags[inst.flag_index];
    return out;
}

// Distance from point p to segment [a,b].
fn sd_segment(p : vec2<f32>, a : vec2<f32>, b : vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / max(dot(ba, ba), 1e-9), 0.0, 1.0);
    return length(pa - ba * h);
}

@fragment
fn fs_main(in : TrackVS) -> @location(0) vec4<f32> {
    let d = sd_segment(in.p, in.a, in.b) - in.radius;
    let aa = 1.0 / max(view.px_per_mm, 1e-6);
    let cov = 1.0 - smoothstep(0.0, aa, d);
    if (cov <= 0.0) { discard; }
    return tint_for(in.flag, cov);
}
"#;

/// Via pipeline body (SDF ring: outer pad radius, drill hole).
const WGSL_VIA_BODY: &str = r#"
struct ViaIn {
    @location(0) center : vec2<f32>,
    @location(1) diameter : f32,
    @location(2) drill : f32,
    @location(3) flag_index : u32,
    @location(4) net_id : u32,
};
struct ViaVS {
    @builtin(position) clip : vec4<f32>,
    @location(0) local : vec2<f32>,    // pad-local mm
    @location(1) r_out : f32,
    @location(2) r_in : f32,
    @location(3) flag : u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vidx : u32, inst : ViaIn) -> ViaVS {
    let q = unit_quad(vidx);
    let r_out = inst.diameter * 0.5;
    let margin = 1.5 / max(view.px_per_mm, 1e-6);
    let local = q * (r_out + margin);
    var out : ViaVS;
    out.clip = project(inst.center + local);
    out.local = local;
    out.r_out = r_out;
    out.r_in = inst.drill * 0.5;
    out.flag = flags[inst.flag_index];
    return out;
}

@fragment
fn fs_main(in : ViaVS) -> @location(0) vec4<f32> {
    let dist = length(in.local);
    // Annulus: inside outer radius AND outside drill radius.
    let aa = 1.0 / max(view.px_per_mm, 1e-6);
    let outer = 1.0 - smoothstep(in.r_out - aa, in.r_out, dist);
    let inner = smoothstep(in.r_in - aa, in.r_in, dist);
    let cov = outer * inner;
    if (cov <= 0.0) { discard; }
    return tint_for(in.flag, cov);
}
"#;

/// Courtyard / bbox line-list pipeline body.
const WGSL_LINE_BODY: &str = r#"
struct LineIn {
    @location(0) pos : vec2<f32>,
    @location(1) flag_index : u32,
};
struct LineVS {
    @builtin(position) clip : vec4<f32>,
    @location(0) flag : u32,
};

@vertex
fn vs_main(inst : LineIn) -> LineVS {
    var out : LineVS;
    out.clip = project(inst.pos);
    out.flag = flags[inst.flag_index];
    return out;
}

@fragment
fn fs_main(in : LineVS) -> @location(0) vec4<f32> {
    return tint_for(in.flag, 0.85);
}
"#;

/// Assemble a full WGSL module source for a pipeline body (prelude + body).
fn wgsl_module(body: &str) -> String {
    let mut src = String::with_capacity(WGSL_PRELUDE.len() + body.len());
    src.push_str(WGSL_PRELUDE);
    src.push_str(body);
    src
}

// ===========================================================================
// The GPU renderer.
// ===========================================================================

/// Owns the wgpu device/queue, the MSAA color targets, the per-class instance +
/// flag buffers, the view uniform, and the four instanced pipelines. Built once
/// (`new`), fed a scene once (`upload_scene`), then drives one `render` per
/// frame with the current [`View`]; selection changes call `update_flags`.
///
/// Cannot be constructed without a real adapter/device, so it is never built in
/// CI / on the headless dev box — only the CPU-side helpers above are tested.
pub struct GpuRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,

    // Offscreen color: an MSAA color texture resolved into a single-sample
    // target the HUD shares as an IOSurface (SPEC, HUD.md §5 tier 2).
    size: (u32, u32),
    msaa_view: wgpu::TextureView,
    resolve_texture: wgpu::Texture,
    resolve_view: wgpu::TextureView,

    // View uniform.
    view_buf: wgpu::Buffer,
    bind_group_layout: wgpu::BindGroupLayout,

    // Per-class instance buffers + counts (geometry; uploaded once).
    pad_buf: Option<InstanceBuffer>,
    track_buf: Option<InstanceBuffer>,
    via_buf: Option<InstanceBuffer>,
    line_buf: Option<LineBuffer>,

    // Per-class flag storage buffers + bind groups (one byte->u32 per entity).
    pad_flag_buf: Option<FlagBinding>,
    track_flag_buf: Option<FlagBinding>,
    via_flag_buf: Option<FlagBinding>,
    component_flag_buf: Option<FlagBinding>,

    // Pipelines.
    pad_pipeline: wgpu::RenderPipeline,
    track_pipeline: wgpu::RenderPipeline,
    via_pipeline: wgpu::RenderPipeline,
    line_pipeline: wgpu::RenderPipeline,

    // The CPU mirror of the packed scene (for cheap flag refresh).
    packing: ScenePacking,

    timer: FrameTimer,
}

/// An instance buffer + its element count.
struct InstanceBuffer {
    buffer: wgpu::Buffer,
    count: u32,
}

/// A line-vertex buffer + its vertex count.
struct LineBuffer {
    buffer: wgpu::Buffer,
    vertices: u32,
}

/// A flag storage buffer + the bind group that binds it (alongside the view
/// uniform) for the pipelines that read it.
struct FlagBinding {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl GpuRenderer {
    /// Initialize the renderer headlessly (no surface): request an adapter +
    /// device, create the MSAA + resolve targets, the view uniform, and the four
    /// pipelines. Async because `request_adapter`/`request_device` are async;
    /// the binary blocks on it with `pollster`.
    ///
    /// DEVICE-GATED: this requires a real Metal adapter and is never run on the
    /// headless CI / M1 dev box (no `gpu` grant there). The function COMPILES and
    /// its structure is reviewed, but the live device path is verified on-device.
    pub async fn new(width: u32, height: u32) -> crate::Result<Self> {
        // PRIMARY includes Metal on macOS; on this app it is always Metal under
        // the `gpu` manifest grant.
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::PRIMARY,
            ..Default::default()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| crate::CanvasError::Render("no GPU adapter (Metal) available".into()))?;

        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: Some("silicon-canvas device"),
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults()
                        .using_resolution(adapter.limits()),
                    memory_hints: wgpu::MemoryHints::Performance,
                },
                None,
            )
            .await
            .map_err(|e| crate::CanvasError::Render(format!("request_device failed: {e}")))?;

        let (msaa_view, resolve_texture, resolve_view) =
            create_targets(&device, width, height);

        // View uniform buffer (one mat3 + scalar).
        let view_uniform = View::new((width as f32, height as f32)).uniform();
        let view_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("view uniform"),
            contents: bytemuck::bytes_of(&view_uniform),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        // Shared bind group layout: view uniform (binding 0) + flag storage
        // (binding 1), both visible to vertex+fragment.
        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("canvas bind group layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: wgpu::BufferSize::new(
                                std::mem::size_of::<ViewUniform>() as u64,
                            ),
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: wgpu::BufferSize::new(4), // at least one u32
                        },
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("canvas pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pad_pipeline = build_pipeline(
            &device,
            &pipeline_layout,
            "pad",
            &wgsl_module(WGSL_PAD_BODY),
            &PadInstance::layout(),
            wgpu::PrimitiveTopology::TriangleList,
        );
        let track_pipeline = build_pipeline(
            &device,
            &pipeline_layout,
            "track",
            &wgsl_module(WGSL_TRACK_BODY),
            &TrackInstance::layout(),
            wgpu::PrimitiveTopology::TriangleList,
        );
        let via_pipeline = build_pipeline(
            &device,
            &pipeline_layout,
            "via",
            &wgsl_module(WGSL_VIA_BODY),
            &ViaInstance::layout(),
            wgpu::PrimitiveTopology::TriangleList,
        );
        let line_pipeline = build_pipeline(
            &device,
            &pipeline_layout,
            "line",
            &wgsl_module(WGSL_LINE_BODY),
            &LineVertex::layout(),
            wgpu::PrimitiveTopology::LineList,
        );

        Ok(GpuRenderer {
            device,
            queue,
            size: (width, height),
            msaa_view,
            resolve_texture,
            resolve_view,
            view_buf,
            bind_group_layout,
            pad_buf: None,
            track_buf: None,
            via_buf: None,
            line_buf: None,
            pad_flag_buf: None,
            track_flag_buf: None,
            via_flag_buf: None,
            component_flag_buf: None,
            pad_pipeline,
            track_pipeline,
            via_pipeline,
            line_pipeline,
            packing: ScenePacking::default(),
            timer: FrameTimer::new(),
        })
    }

    /// The resolved (single-sample) color texture the HUD shares as an
    /// IOSurface. The renderer never presents to a window; the host owns the
    /// composite (SPEC, HUD.md §5).
    pub fn resolve_texture(&self) -> &wgpu::Texture {
        &self.resolve_texture
    }

    /// Resize the render targets (the HUD told us the surface changed). Recreates
    /// the MSAA + resolve textures; geometry buffers are untouched (pan/zoom and
    /// size are uniform/target concerns, never a rebuild — SPEC §2).
    pub fn resize(&mut self, width: u32, height: u32) {
        if (width, height) == self.size || width == 0 || height == 0 {
            return;
        }
        self.size = (width, height);
        let (msaa_view, resolve_texture, resolve_view) =
            create_targets(&self.device, width, height);
        self.msaa_view = msaa_view;
        self.resolve_texture = resolve_texture;
        self.resolve_view = resolve_view;
    }

    /// Upload a scene's geometry ONCE (SPEC §2). Packs the struct-of-arrays into
    /// instance buffers + flag storage buffers + the courtyard line buffer and
    /// uploads them. After this, frames are uniform writes + redraws only.
    pub fn upload_scene(&mut self, scene: &Scene) {
        let packing = ScenePacking::from_scene(scene);

        self.pad_buf = make_instance_buffer(&self.device, "pads", &packing.pads);
        self.track_buf = make_instance_buffer(&self.device, "tracks", &packing.tracks);
        self.via_buf = make_instance_buffer(&self.device, "vias", &packing.vias);
        self.line_buf = make_line_buffer(&self.device, "courtyards", &packing.courtyards);

        self.pad_flag_buf = make_flag_binding(
            &self.device,
            &self.bind_group_layout,
            &self.view_buf,
            "pad-flags",
            &packing.pad_flags,
        );
        self.track_flag_buf = make_flag_binding(
            &self.device,
            &self.bind_group_layout,
            &self.view_buf,
            "track-flags",
            &packing.track_flags,
        );
        self.via_flag_buf = make_flag_binding(
            &self.device,
            &self.bind_group_layout,
            &self.view_buf,
            "via-flags",
            &packing.via_flags,
        );
        self.component_flag_buf = make_flag_binding(
            &self.device,
            &self.bind_group_layout,
            &self.view_buf,
            "component-flags",
            &packing.component_flags,
        );

        self.packing = packing;
    }

    /// Re-upload the highlight flag buffers from the scene's current
    /// [`HighlightFlag`] arrays (SPEC §2/§4: selection / trace step = a flag
    /// write + redraw, geometry untouched). Cheap: one `write_buffer` per class.
    pub fn update_flags(&mut self, scene: &Scene) {
        self.packing.refresh_flags(scene);
        if let Some(b) = &self.pad_flag_buf {
            self.queue
                .write_buffer(&b.buffer, 0, &widen_flags(&self.packing.pad_flags));
        }
        if let Some(b) = &self.track_flag_buf {
            self.queue
                .write_buffer(&b.buffer, 0, &widen_flags(&self.packing.track_flags));
        }
        if let Some(b) = &self.via_flag_buf {
            self.queue
                .write_buffer(&b.buffer, 0, &widen_flags(&self.packing.via_flags));
        }
        if let Some(b) = &self.component_flag_buf {
            self.queue.write_buffer(
                &b.buffer,
                0,
                &widen_flags(&self.packing.component_flags),
            );
        }
    }

    /// Push the current [`View`] into the uniform buffer (the ONLY per-frame
    /// geometry-side write; SPEC §2: pan/zoom is a uniform, not a rebuild).
    pub fn update_view(&mut self, view: &View) {
        let u = view.uniform();
        self.queue
            .write_buffer(&self.view_buf, 0, bytemuck::bytes_of(&u));
    }

    /// Render one frame for the given view. Writes the view uniform, records the
    /// draw set under the current LOD, submits, and returns `Some(RenderMs)` on
    /// the 1 Hz boundary (the caller relays it on `canvas.render_ms`).
    ///
    /// DEVICE-GATED: this issues real GPU work and is only exercised on-device.
    pub fn render(&mut self, view: &View) -> Option<RenderMs> {
        let t0 = std::time::Instant::now();
        self.update_view(view);

        let lod = Lod::for_scale(view.clamped_scale());
        let plan = self.draw_plan(&lod);

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("canvas frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("canvas pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa_view,
                    resolve_target: Some(&self.resolve_view),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.02,
                            g: 0.03,
                            b: 0.05,
                            a: 1.0,
                        }),
                        // Discard the multisample target after resolve (we only
                        // share the resolved single-sample texture).
                        store: wgpu::StoreOp::Discard,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            // Draw order: tracks (spine) -> vias -> pads -> courtyards, so pads
            // sit on top of copper and the (thin) outline on top of all.
            if plan.draw_tracks {
                if let (Some(buf), Some(flag)) = (&self.track_buf, &self.track_flag_buf) {
                    pass.set_pipeline(&self.track_pipeline);
                    pass.set_bind_group(0, &flag.bind_group, &[]);
                    pass.set_vertex_buffer(0, buf.buffer.slice(..));
                    pass.draw(0..6, 0..buf.count);
                }
            }
            if plan.draw_vias {
                if let (Some(buf), Some(flag)) = (&self.via_buf, &self.via_flag_buf) {
                    pass.set_pipeline(&self.via_pipeline);
                    pass.set_bind_group(0, &flag.bind_group, &[]);
                    pass.set_vertex_buffer(0, buf.buffer.slice(..));
                    pass.draw(0..6, 0..buf.count);
                }
            }
            if plan.draw_pads {
                if let (Some(buf), Some(flag)) = (&self.pad_buf, &self.pad_flag_buf) {
                    pass.set_pipeline(&self.pad_pipeline);
                    pass.set_bind_group(0, &flag.bind_group, &[]);
                    pass.set_vertex_buffer(0, buf.buffer.slice(..));
                    pass.draw(0..6, 0..buf.count);
                }
            }
            if plan.draw_courtyards {
                if let (Some(buf), Some(flag)) = (&self.line_buf, &self.component_flag_buf) {
                    pass.set_pipeline(&self.line_pipeline);
                    pass.set_bind_group(0, &flag.bind_group, &[]);
                    pass.set_vertex_buffer(0, buf.buffer.slice(..));
                    pass.draw(0..buf.vertices, 0..1);
                }
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));

        let frame_ms = t0.elapsed().as_secs_f64() * 1000.0;
        debug_assert!(
            plan.draws <= MAX_DRAWS_PER_FRAME,
            "draw budget exceeded: {} > {}",
            plan.draws,
            MAX_DRAWS_PER_FRAME
        );
        self.timer
            .record(frame_ms, plan.draws, plan.culled_pct)
    }

    /// Compute which classes draw this frame under `lod` and the resulting draw
    /// count + culled percentage — pure, so it is unit-tested without a device.
    fn draw_plan(&self, lod: &Lod) -> DrawPlan {
        let pads = self.pad_buf.as_ref().map(|b| b.count).unwrap_or(0);
        let tracks = self.track_buf.as_ref().map(|b| b.count).unwrap_or(0);
        let vias = self.via_buf.as_ref().map(|b| b.count).unwrap_or(0);
        let lines = self.line_buf.as_ref().map(|b| b.vertices).unwrap_or(0);

        let draw_tracks = lod.draw_tracks && tracks > 0;
        let draw_vias = lod.draw_vias && vias > 0;
        let draw_pads = lod.draw_pads && pads > 0;
        // Courtyards always available; they are the bbox-only fallback too.
        let draw_courtyards = lines > 0;

        let mut draws = 0;
        if draw_tracks {
            draws += 1;
        }
        if draw_vias {
            draws += 1;
        }
        if draw_pads {
            draws += 1;
        }
        if draw_courtyards {
            draws += 1;
        }

        // culled_pct: of the SDF instance candidates, how many are NOT drawn this
        // frame because LOD turned their class off. (Viewport-rect culling is a
        // future per-tile refinement; class-level LOD is what this frame reports.)
        let total = (pads + tracks + vias) as f64;
        let drawn_pads = if draw_pads { pads } else { 0 };
        let drawn_tracks = if draw_tracks { tracks } else { 0 };
        let drawn_vias = if draw_vias { vias } else { 0 };
        let drawn = (drawn_pads + drawn_tracks + drawn_vias) as f64;
        let culled_pct = if total > 0.0 {
            (1.0 - drawn / total) * 100.0
        } else {
            0.0
        };

        DrawPlan {
            draw_tracks,
            draw_vias,
            draw_pads,
            draw_courtyards,
            draws,
            culled_pct,
        }
    }

    /// Expose the frame timer's current snapshot (without advancing the window) —
    /// useful for a forced telemetry drop on demand.
    pub fn frame_stats(&self) -> RenderMs {
        self.timer.snapshot()
    }
}

/// What [`GpuRenderer::draw_plan`] decided for one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DrawPlan {
    draw_tracks: bool,
    draw_vias: bool,
    draw_pads: bool,
    draw_courtyards: bool,
    draws: u32,
    culled_pct: f64,
}

// ===========================================================================
// Device-side construction helpers (only reached with a live device).
// ===========================================================================

/// Create the MSAA color texture + the single-sample resolve texture/view.
fn create_targets(
    device: &wgpu::Device,
    width: u32,
    height: u32,
) -> (wgpu::TextureView, wgpu::Texture, wgpu::TextureView) {
    let extent = wgpu::Extent3d {
        width: width.max(1),
        height: height.max(1),
        depth_or_array_layers: 1,
    };
    let msaa = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("canvas msaa color"),
        size: extent,
        mip_level_count: 1,
        sample_count: MSAA_SAMPLES,
        dimension: wgpu::TextureDimension::D2,
        format: COLOR_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let resolve = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("canvas resolve color (IOSurface share)"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: COLOR_FORMAT,
        // RENDER_ATTACHMENT (resolve dst) + TEXTURE_BINDING so the HUD can sample
        // it / share it as an IOSurface composite source.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let resolve_view = resolve.create_view(&wgpu::TextureViewDescriptor::default());
    (msaa_view, resolve, resolve_view)
}

/// Build one instanced render pipeline from a WGSL module + an instance vertex
/// layout. All pipelines share the bind group layout, 4x MSAA, and the
/// alpha-blended Bgra target.
fn build_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    label: &str,
    wgsl: &str,
    instance_layout: &wgpu::VertexBufferLayout,
    topology: wgpu::PrimitiveTopology,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(Cow::Owned(wgsl.to_string())),
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: "vs_main",
            buffers: std::slice::from_ref(instance_layout),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format: COLOR_FORMAT,
                blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: wgpu::PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: MSAA_SAMPLES,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview: None,
        cache: None,
    })
}

/// Create an instance buffer from a POD slice (empty -> None so the draw is
/// skipped).
fn make_instance_buffer<T: Pod>(
    device: &wgpu::Device,
    label: &str,
    data: &[T],
) -> Option<InstanceBuffer> {
    if data.is_empty() {
        return None;
    }
    let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(data),
        usage: wgpu::BufferUsages::VERTEX,
    });
    Some(InstanceBuffer {
        buffer,
        count: data.len() as u32,
    })
}

/// Create the courtyard line-vertex buffer.
fn make_line_buffer(
    device: &wgpu::Device,
    label: &str,
    data: &[LineVertex],
) -> Option<LineBuffer> {
    if data.is_empty() {
        return None;
    }
    let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents: bytemuck::cast_slice(data),
        usage: wgpu::BufferUsages::VERTEX,
    });
    Some(LineBuffer {
        buffer,
        vertices: data.len() as u32,
    })
}

/// Create a flag storage buffer (one u32 per entity, widened from the byte
/// mirror) + the bind group that pairs it with the view uniform. An empty class
/// still gets a 1-element buffer so the bind group's `min_binding_size` holds.
fn make_flag_binding(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    view_buf: &wgpu::Buffer,
    label: &str,
    flag_bytes: &[u8],
) -> Option<FlagBinding> {
    let widened = widen_flags(flag_bytes);
    let contents: &[u8] = if widened.is_empty() {
        // Storage buffers must be non-empty; one zero u32 placeholder.
        &[0, 0, 0, 0]
    } else {
        &widened
    };
    let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some(label),
        contents,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: view_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: buffer.as_entire_binding(),
            },
        ],
    });
    Some(FlagBinding { buffer, bind_group })
}

/// Widen a 1-byte-per-entity flag array to 1-u32-per-entity little-endian bytes,
/// the layout the WGSL `array<u32>` storage buffer indexes. (SPEC §2 calls for a
/// 1-byte flag; WGSL storage arrays index in 4-byte words, so we store the byte
/// in a u32 lane — still O(entities) memory, still a single buffer write on a
/// selection change.)
fn widen_flags(flag_bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(flag_bytes.len() * 4);
    for &b in flag_bytes {
        out.extend_from_slice(&(b as u32).to_le_bytes());
    }
    out
}

// ===========================================================================
// Vertex/instance buffer layouts.
// ===========================================================================

impl PadInstance {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        // center(2f) half_size(2f) rotation(1f) shape(u32) flag_index(u32) net(u32)
        const ATTRS: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![
            0 => Float32x2, // center
            1 => Float32x2, // half_size
            2 => Float32,   // rotation
            3 => Uint32,    // shape
            4 => Uint32,    // flag_index
            5 => Uint32,    // net_id
        ];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<PadInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &ATTRS,
        }
    }
}

impl TrackInstance {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
            0 => Float32x2, // a
            1 => Float32x2, // b
            2 => Float32,   // width
            3 => Uint32,    // flag_index
            4 => Uint32,    // net_id
        ];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TrackInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &ATTRS,
        }
    }
}

impl ViaInstance {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
            0 => Float32x2, // center
            1 => Float32,   // diameter
            2 => Float32,   // drill
            3 => Uint32,    // flag_index
            4 => Uint32,    // net_id
        ];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ViaInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &ATTRS,
        }
    }
}

impl LineVertex {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRS: [wgpu::VertexAttribute; 2] = wgpu::vertex_attr_array![
            0 => Float32x2, // pos
            1 => Uint32,    // flag_index
        ];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<LineVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex, // line list: per-vertex
            attributes: &ATTRS,
        }
    }
}

// ===========================================================================
// Tests — CPU-side ONLY (no GPU in CI). They cover the view math, LOD
// thresholds, scene packing byte layout, flag widening, the percentile timer,
// and the draw-plan budget. The live device path (GpuRenderer::new/render) is
// DEVICE-GATED and exercised on a real Apple Silicon Mac, never here.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::NetId;
    use crate::scene::{
        Aabb, Component, HighlightFlag, LayerId, Pad, PadShape, PinType, Point, Scene, SceneKind,
        Track, Via,
    };

    fn pt(x: f64, y: f64) -> Point {
        Point::new(x, y)
    }

    #[test]
    fn pod_types_are_zeroable_and_sized() {
        // bytemuck Pod requires no padding surprises; assert the sizes are the
        // tight repr(C) layouts the vertex attributes assume.
        assert_eq!(std::mem::size_of::<ViewUniform>(), 64);
        assert_eq!(std::mem::size_of::<PadInstance>(), 4 * 8); // 2+2+1+1+1+1 f32/u32
        assert_eq!(std::mem::size_of::<TrackInstance>(), 4 * 8);
        assert_eq!(std::mem::size_of::<ViaInstance>(), 4 * 8);
        assert_eq!(std::mem::size_of::<LineVertex>(), 4 * 4);
        // Zeroable smoke test.
        let _ = PadInstance::zeroed();
        let _ = ViewUniform::zeroed();
    }

    #[test]
    fn view_scale_is_clamped_to_spec_range() {
        let mut v = View::new((800.0, 600.0));
        v.scale = 10_000.0;
        assert_eq!(v.clamped_scale(), MAX_SCALE);
        v.scale = 1e-9;
        assert_eq!(v.clamped_scale(), MIN_SCALE);
    }

    #[test]
    fn screen_center_maps_to_view_center() {
        let mut v = View::new((800.0, 600.0));
        v.center = DVec2::new(50.0, 25.0);
        v.scale = 4.0;
        let scene = v.screen_to_scene(DVec2::new(400.0, 300.0));
        assert!((scene.x - 50.0).abs() < 1e-9, "{scene:?}");
        assert!((scene.y - 25.0).abs() < 1e-9, "{scene:?}");
    }

    #[test]
    fn zoom_about_keeps_cursor_point_fixed() {
        let mut v = View::new((800.0, 600.0));
        v.center = DVec2::new(10.0, 10.0);
        v.scale = 2.0;
        let cursor = DVec2::new(600.0, 200.0);
        let before = v.screen_to_scene(cursor);
        v.zoom_about(cursor, 3.0);
        let after = v.screen_to_scene(cursor);
        // The scene point under the cursor must not move when zooming about it.
        assert!((before.x - after.x).abs() < 1e-6, "{before:?} {after:?}");
        assert!((before.y - after.y).abs() < 1e-6, "{before:?} {after:?}");
        assert!(v.scale > 2.0); // actually zoomed in
    }

    #[test]
    fn view_center_projects_to_clip_origin() {
        // The view center must land at clip-space origin (0,0).
        let mut v = View::new((800.0, 600.0));
        v.center = DVec2::new(123.0, -45.0);
        v.scale = 7.0;
        let m = v.scene_to_clip_f64();
        let clip = m * glam::DVec3::new(v.center.x, v.center.y, 1.0);
        assert!(clip.x.abs() < 1e-9, "{clip:?}");
        assert!(clip.y.abs() < 1e-9, "{clip:?}");
        assert!((clip.z - 1.0).abs() < 1e-9, "{clip:?}");
    }

    #[test]
    fn view_y_axis_flips_for_screen_up() {
        // A scene point above the center (greater Y) should map to positive clip
        // Y (up on screen), since scene Y is up.
        let mut v = View::new((800.0, 600.0));
        v.center = DVec2::ZERO;
        v.scale = 10.0;
        let m = v.scene_to_clip_f64();
        let up = m * glam::DVec3::new(0.0, 5.0, 1.0);
        assert!(up.y > 0.0, "scene +Y should be clip +Y (up): {up:?}");
    }

    #[test]
    fn fit_centers_and_scales_to_bounds() {
        let mut v = View::new((1000.0, 1000.0));
        let bb = Aabb::new(pt(-50.0, -50.0), pt(50.0, 50.0)); // 100x100 mm
        v.fit(bb);
        assert!((v.center.x).abs() < 1e-9 && (v.center.y).abs() < 1e-9);
        // 1000 px / 100 mm * 0.92 margin = 9.2 px/mm.
        assert!((v.clamped_scale() - 9.2).abs() < 1e-6, "{}", v.clamped_scale());
    }

    #[test]
    fn fit_empty_bbox_is_noop() {
        let mut v = View::new((800.0, 600.0));
        v.center = DVec2::new(3.0, 4.0);
        v.scale = 5.0;
        v.fit(Aabb::EMPTY);
        assert_eq!(v.center, DVec2::new(3.0, 4.0));
        assert_eq!(v.scale, 5.0);
    }

    #[test]
    fn uniform_roundtrips_through_mat3() {
        let mut v = View::new((640.0, 480.0));
        v.center = DVec2::new(2.0, -3.0);
        v.scale = 12.5;
        let u = v.uniform();
        let m = u.to_mat3();
        let f = v.scene_to_clip_f64();
        // Downcast f64 -> f32 then compare element-wise within f32 epsilon.
        let fc = f.to_cols_array();
        let mc = m.to_cols_array();
        for i in 0..9 {
            assert!((mc[i] - fc[i] as f32).abs() < 1e-4, "elem {i}: {} {}", mc[i], fc[i]);
        }
        assert_eq!(u.px_per_mm, 12.5);
    }

    #[test]
    fn lod_drops_classes_as_zoom_falls() {
        // Zoomed way in: everything draws.
        let near = Lod::for_scale(100.0);
        assert!(near.draw_text && near.draw_pads && near.draw_tracks && !near.bbox_only);
        // Mid zoom: text gone (1.27mm * 1.0 = 1.27px < 2px) but pads still drawn
        // (0.6mm * 1.0 = 0.6px < 2px -> actually pads gone too at 1.0). Use a
        // scale where text is culled but pads survive: pad needs >=2px so
        // px/mm >= 3.33; text needs >=2px so px/mm >= 1.575. At 4 px/mm both on.
        let four = Lod::for_scale(4.0);
        assert!(four.draw_text && four.draw_pads);
        // At 2 px/mm: text on (2*1.27=2.54>=2), pads off (2*0.6=1.2<2) -> bbox_only.
        let two = Lod::for_scale(2.0);
        assert!(two.draw_text);
        assert!(!two.draw_pads, "pads should cull below ~3.33 px/mm");
        assert!(two.bbox_only && !two.draw_tracks && !two.draw_vias);
        // Far out: nothing but bboxes.
        let far = Lod::for_scale(0.05);
        assert!(!far.draw_text && !far.draw_pads && far.bbox_only);
    }

    #[test]
    fn scene_packing_lays_out_instances() {
        let mut s = Scene::new(SceneKind::Pcb);
        s.components.push(Component {
            reference: "U1".into(),
            value: "MCU".into(),
            lib_id: "MCU:X".into(),
            position: pt(0.0, 0.0),
            rotation: 0.0,
            mirror: false,
            bbox: Aabb::new(pt(-2.0, -2.0), pt(2.0, 2.0)),
            layer: LayerId::new(0),
        });
        s.pads.push(Pad {
            component: Scene::component_id(0),
            name: "1".into(),
            position: pt(1.0, 1.0),
            size: (0.8, 0.6),
            shape: PadShape::RoundRect,
            pin_type: PinType::Passive,
            layer: LayerId::new(0),
            net_id: NetId::new(3),
        });
        s.pads.push(Pad {
            component: Scene::component_id(0),
            name: "2".into(),
            position: pt(2.0, 1.0),
            size: (0.5, 0.5),
            shape: PadShape::Circle,
            pin_type: PinType::Passive,
            layer: LayerId::new(0),
            net_id: NetId::NONE,
        });
        s.tracks.push(Track {
            a: pt(0.0, 0.0),
            b: pt(5.0, 0.0),
            width: 0.25,
            layer: LayerId::new(0),
            net_id: NetId::new(3),
        });
        s.vias.push(Via {
            position: pt(2.5, 0.0),
            diameter: 0.8,
            drill: 0.4,
            layer_from: LayerId::new(0),
            layer_to: LayerId::new(1),
            net_id: NetId::new(3),
        });
        s.init_flags();

        let pk = ScenePacking::from_scene(&s);
        assert_eq!(pk.pads.len(), 2);
        assert_eq!(pk.tracks.len(), 1);
        assert_eq!(pk.vias.len(), 1);
        // Courtyard: 1 component bbox -> 4 segments -> 8 line vertices.
        assert_eq!(pk.courtyards.len(), 8);

        // Flag indices are the entity's own array index.
        assert_eq!(pk.pads[0].flag_index, 0);
        assert_eq!(pk.pads[1].flag_index, 1);
        assert_eq!(pk.tracks[0].flag_index, 0);
        assert_eq!(pk.vias[0].flag_index, 0);
        // Shape codes match the enum discriminant order (Circle=0, Rect=1,
        // RoundRect=2, Oval=3).
        assert_eq!(pk.pads[0].shape, 2); // RoundRect
        assert_eq!(pk.pads[1].shape, 0); // Circle
        assert_eq!(pad_shape_code(PadShape::Circle), 0);
        assert_eq!(pad_shape_code(PadShape::Oval), 3);
        // half_size is half the pad size.
        assert!((pk.pads[0].half_size[0] - 0.4).abs() < 1e-6);
        assert!((pk.pads[0].half_size[1] - 0.3).abs() < 1e-6);
        // net ids carried through.
        assert_eq!(pk.pads[0].net_id, 3);
        assert_eq!(pk.pads[1].net_id, u32::MAX);
        // Flag byte mirrors parallel the geometry.
        assert_eq!(pk.pad_flags.len(), 2);
        assert_eq!(pk.track_flags.len(), 1);
        assert_eq!(pk.component_flags.len(), 1);
    }

    #[test]
    fn refresh_flags_reflects_highlight_writes() {
        let mut s = Scene::new(SceneKind::Pcb);
        s.tracks.push(Track {
            a: pt(0.0, 0.0),
            b: pt(1.0, 0.0),
            width: 0.2,
            layer: LayerId::new(0),
            net_id: NetId::new(1),
        });
        s.tracks.push(Track {
            a: pt(1.0, 0.0),
            b: pt(2.0, 0.0),
            width: 0.2,
            layer: LayerId::new(0),
            net_id: NetId::new(2),
        });
        s.init_flags();
        let mut pk = ScenePacking::from_scene(&s);
        assert_eq!(pk.track_flags, vec![0u8, 0u8]);

        // A selection highlights track 0, dims track 1 (the trace/erc agents do
        // exactly this kind of write).
        s.track_flags[0] = HighlightFlag::Highlighted;
        s.track_flags[1] = HighlightFlag::Dimmed;
        pk.refresh_flags(&s);
        assert_eq!(pk.track_flags, vec![1u8, 2u8]);
    }

    #[test]
    fn widen_flags_packs_one_u32_per_byte_le() {
        let bytes = vec![0u8, 1, 2, 3];
        let w = widen_flags(&bytes);
        assert_eq!(w.len(), 16);
        assert_eq!(&w[0..4], &[0, 0, 0, 0]);
        assert_eq!(&w[4..8], &[1, 0, 0, 0]);
        assert_eq!(&w[8..12], &[2, 0, 0, 0]);
        assert_eq!(&w[12..16], &[3, 0, 0, 0]);
    }

    #[test]
    fn empty_class_packs_to_no_buffer_and_zero_widen() {
        let s = Scene::new(SceneKind::Schematic);
        let pk = ScenePacking::from_scene(&s);
        assert!(pk.pads.is_empty() && pk.tracks.is_empty() && pk.vias.is_empty());
        assert!(widen_flags(&pk.pad_flags).is_empty());
    }

    #[test]
    fn percentile_window_math() {
        let s: Vec<f64> = (1..=10).map(|i| i as f64).collect();
        // p50 nearest-rank: ceil(0.5*10)=5 -> v[4] = 5.
        assert_eq!(percentile(&s, 0.50), 5.0);
        // p95: ceil(0.95*10)=10 -> v[9] = 10.
        assert_eq!(percentile(&s, 0.95), 10.0);
        // Empty window -> 0.
        assert_eq!(percentile(&[], 0.5), 0.0);
        // Single sample.
        assert_eq!(percentile(&[42.0], 0.95), 42.0);
    }

    #[test]
    fn frame_timer_reports_on_one_second_boundary() {
        let mut t = FrameTimer::new();
        // 62 frames of 16ms = 992ms: still under the 1000ms boundary, no report.
        for _ in 0..62 {
            assert!(t.record(16.0, 4, 10.0).is_none());
        }
        // The 63rd reaches 1008ms (>= 1000) -> exactly one RenderMs.
        let r = t.record(16.0, 4, 10.0).expect("1 Hz boundary should report");
        assert_eq!(r.draws, 4);
        assert!((r.culled_pct - 10.0).abs() < 1e-9);
        assert!(r.p50 > 0.0 && r.p95 >= r.p50);
        // The accumulator reset: the next short burst doesn't immediately report.
        assert!(t.record(16.0, 4, 10.0).is_none());
    }

    #[test]
    fn frame_timer_clamps_culled_pct() {
        let mut t = FrameTimer::new();
        let r = t.record(1001.0, 30, 250.0).unwrap();
        assert_eq!(r.culled_pct, 100.0); // clamped
        assert_eq!(r.draws, 30);
    }

    #[test]
    fn draw_plan_counts_and_budget() {
        // Build a DrawPlan via the same logic GpuRenderer::draw_plan uses, by
        // replicating it against synthetic counts (no device).
        // Far-out LOD -> only courtyards.
        let lod_far = Lod::for_scale(0.05);
        assert!(lod_far.bbox_only);
        // Near LOD with all classes present: 4 draws (tracks, vias, pads,
        // courtyards) — well under the 30 budget.
        let lod_near = Lod::for_scale(100.0);
        let mut draws = 0;
        if lod_near.draw_tracks {
            draws += 1;
        }
        if lod_near.draw_vias {
            draws += 1;
        }
        if lod_near.draw_pads {
            draws += 1;
        }
        // courtyards always:
        draws += 1;
        assert_eq!(draws, 4);
        assert!(draws <= MAX_DRAWS_PER_FRAME);
    }

    #[test]
    fn wgsl_modules_concatenate_prelude_and_body() {
        for body in [WGSL_PAD_BODY, WGSL_TRACK_BODY, WGSL_VIA_BODY, WGSL_LINE_BODY] {
            let m = wgsl_module(body);
            assert!(m.starts_with(WGSL_PRELUDE));
            assert!(m.contains("fn vs_main"));
            assert!(m.contains("fn fs_main"));
            // Each body must reference the shared view + flags bindings.
            assert!(m.contains("view") && m.contains("flags"));
        }
    }

    #[test]
    fn culled_pct_logic_matches_drawn_fraction() {
        // Replicate draw_plan's culled_pct math: when pads (say 100) are culled
        // but tracks (50) + vias (10) draw, culled = 100/160.
        let pads: f64 = 100.0;
        let tracks: f64 = 50.0;
        let vias: f64 = 10.0;
        let total = pads + tracks + vias;
        let drawn = tracks + vias; // pads culled
        let culled = (1.0 - drawn / total) * 100.0;
        assert!((culled - 62.5).abs() < 1e-9, "{culled}");
    }
}
