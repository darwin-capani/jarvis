# Silicon Canvas — SPEC

GPU schematic/PCB renderer: KiCad import, net highlighting and interactive tracing, ERC overlay, 60 fps pan/zoom on 10k+ component boards. Phase-4 implementation against `docs/SANDBOX.md`; fullscreen surface per `docs/HUD.md` §5.

## Sandbox contract (binding: `manifest.toml`)

- Runtime `binary` (prebuilt native binary in the app dir — Rust).
- `gpu = true`: Metal device access for wgpu. The only launch app with a GPU grant.
- `net_hosts = []` — fully offline. `fs_read`: `apps/silicon-canvas/libraries` (symbols/footprints), `apps/silicon-canvas/projects` (KiCad files). `fs_write = state/tmp/silicon-canvas` (tessellation/render cache).
- IPC: JSONL over `state/ipc/apps/silicon-canvas.sock`, capability token per message.
- UI: `surface = "fullscreen"` — composited by the HUD as a shared **IOSurface** texture (HUD.md §5 tier 2); input arrives as JSONL events routed by the HUD over the app socket. Silicon Canvas never opens a window or reads input devices.
- Topics: `canvas.render_ms`, `canvas.viewport`, `canvas.selection`.

## 1. Document model

- **Import**: KiCad 7/8 S-expression formats — `.kicad_sch` (schematics, hierarchical sheets), `.kicad_pcb` (boards), `.kicad_sym`/`.kicad_mod` libraries. The parser is split into a generic S-expression layer (`sexpr`) and a KiCad document layer (`parser`), both contracted to be total over arbitrary input (return `Result`, never panic). Panic-freedom is covered two ways: a cargo-fuzz target at `fuzz/fuzz_targets/parse_document.rs` (run with `cargo +nightly fuzz run parse_document`; needs the nightly toolchain + libFuzzer, so it does not run on a stable-only box), plus stable, deterministic in-tree randomized panic-freedom tests (`sexpr::tests::fuzz_*`, `parser::tests::fuzz_*`) that drive seeded-LCG adversarial inputs through both layers and run under plain `cargo test`. Imports are read-only (Silicon Canvas is a viewer/analyzer, not an editor — non-goal §7).
- **Scene**: flat typed arrays (struct-of-arrays) per entity class — components, pads, tracks, vias, zones, sheet symbols, wires, junctions, labels — each carrying `net_id` where applicable. No per-entity heap objects; ids are indices.
- **Connectivity graph**: built at import — nodes are pads/wire-segments/vias/labels, edges are electrical contact (position-quantized endpoint matching for schematics; copper overlap per layer + via stitching for PCBs). Powers tracing (§4) and ERC (§5).
- **Spatial index**: one R-tree per layer/class for hit-testing and viewport culling; built once at import, immutable afterward.

## 2. Renderer (wgpu/Metal)

Render-scale target: **10k components / 100k+ pads / 200k track segments at 60 fps sustained pan/zoom** (target on ProMotion-class Apple Silicon; the same scene runs ≥ 30 fps on the M1 Pro dev machine — the measured floor).

The technique that makes this trivial rather than heroic: **everything is GPU-resident and instanced; pan/zoom is a uniform, not a rebuild.**

- All geometry uploaded once at import into static vertex/instance buffers. The view transform is a single mat3 uniform — pan/zoom re-renders, never re-tessellates, never re-uploads.
- Per-layer instanced pipelines: pads (rect/circle/roundrect/oval as SDF quads — one instance each), track segments (SDF capsule quads), vias (SDF rings), component courtyards/outlines (line lists). Text via SDF glyph atlas (reference designators, values, net labels).
- Zones/polygons tessellated once at import (`lyon`), cached in `state/tmp/silicon-canvas/` keyed by file hash.
- Target ≤ 30 draw calls/frame: one per (layer × pipeline). 4× MSAA.
- **LOD by zoom**: below thresholds, drop text → drop pads (tracks imply them) → component bounding boxes only. Thresholds in screen-space feature size (< 2 px = culled class).
- Highlight/selection state lives in a per-entity 1-byte flag buffer (GPU): selection changes write flags, no geometry touched — net highlight on a 100k-pad board is a buffer write + redraw.

Frame stats published on `canvas.render_ms` at 1 Hz: `{p50, p95, draws, culled_pct}`.

## 3. Interaction

- Input events from the HUD: pointer (position in surface coords), scroll/pinch, modifier keys. Pan = drag; zoom = pinch/scroll about the cursor; zoom range 0.01×–500× double-precision view math (single precision breaks at deep zoom on large boards).
- Hit-test via R-tree (point query, layer-priority order); hover shows entity tooltip data published with `canvas.selection`.
- `canvas.viewport` `{x, y, scale, layer_visibility}` on change (throttled 10 Hz) — the HUD can mirror a minimap.

## 4. Net highlighting + interactive tracing

- **Click a pad/track/wire** → its net's entities get highlight flags: net in `--holo-bright`, everything else dimmed to 25%. `canvas.selection` publishes `{net, name, entity_count, pin_count}`.
- **Trace mode** (key `t` with a net selected): step-walk the connectivity graph from the selected node; each step advances the highlight front one edge (pad → track → via → track on inner layer → pad), camera glides to follow. Cross-layer steps flash the via and toggle layer emphasis. Walk order: BFS by electrical distance. Esc exits.
- Cross-probe between views: selecting a component/net in the schematic highlights it in the PCB view and vice versa (shared net/component ids from import).

## 5. ERC overlay

Electrical rules run at import (and on demand via the `erc.run` op), schematic-side:

- Unconnected pins (non-NC), pin-type conflicts (output↔output, multiple drivers), power pins without a driving source/power flag, dangling wires, duplicate reference designators, label typos (single-ended named nets).
- Results render as a marker layer: amber (warning) / red (error) badges at fault coordinates, clusterable when zoomed out, click-to-zoom from a panel-side list (published once on `canvas.selection` channel as `{erc: [{code, severity, at, message}]}`).
- ERC is advisory and conservative — it flags what is provable from the netlist and pin metadata in the libraries, nothing heuristic.

## 6. IPC ops (JSONL, token-bearing)

| op | request | effect |
|---|---|---|
| `project.open` | `{path}` (inside `projects/`) | Import + build graph/index |
| `view.set` | `{x, y, scale}` / `{fit: "all"\|net\|component}` | Camera |
| `select.net` / `select.component` | `{name}` | Programmatic selection (voice: "show me the 3V3 net" routes here) |
| `trace.start` / `trace.step` / `trace.stop` | `{}` | Trace mode |
| `layer.set` | `{layer, visible}` | Layer visibility |
| `erc.run` | `{}` | Re-run ERC |

## 7. Milestones

1. Parser (sch+pcb+libs; panic-freedom via a cargo-fuzz target + stable in-tree randomized tests, see §1) + scene/graph/R-tree build; headless import benchmark on a 10k-component reference board.
2. Renderer: instanced pipelines + LOD + flag-buffer highlight; 60 fps pan/zoom on the reference board (measured via `canvas.render_ms`).
3. Interaction + net highlight + trace mode + cross-probe.
4. ERC overlay + IPC op surface + voice-driven selection through the daemon.

Non-goals: editing/saving KiCad files, autorouting, 3D board view, DRC (copper-clearance checking — ERC only in this phase), Gerber import.
