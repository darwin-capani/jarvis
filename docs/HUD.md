# JARVIS HUD — Phase-2 Design & Engineering Spec

The fullscreen face of the machine. This document is the buildable spec for Phase 2; `docs/ROADMAP.md` Phase 2 means "implement this file."

Ground rules (inherited from `docs/ARCHITECTURE.md`):

- The HUD is a **telemetry client** of `ws://127.0.0.1:7177` (it holds no state the daemon doesn't have and can crash or restart at any time without affecting the voice pipeline), **PLUS a token-authenticated, local-only command channel** (`state/ipc/command.sock` via the `send_command` Tauri backend) that routes every action INTO the daemon's existing gated pipeline — it can do nothing the voice path cannot: consequential actions still park behind the cross-turn confirmation gate + the armed-by-default master switch (`integrations.allow_consequential`, ships ON — a confirmed action still needs a fresh per-action confirm), per-agent allowlist isolation still applies, and Self-Forge stays propose-only (the deck may dismiss a proposal but apply/deploy stays `scripts/apply_forge.sh`).
- It is a **fullscreen, always-on-top shell replacement**: macOS remains the invisible host; the HUD is the only thing on screen.
- Target hardware: **any Apple Silicon Mac (M1 or later)**; developed against an M4 Mac Mini, ProMotion-class 120 fps. Dev hardware: M1 Pro at 60 fps with the same scene.
- The daemon owns the mic for the pipeline. The HUD opens its **own read-only input tap** for the FFT (macOS allows multiple clients on one input device); it never records, stores, or transmits audio.

---

## 1. Design system

### 1.1 Palette

Deep-space black glass with cyan/ice holographic wireframes. Tokens (single source for both the web and wgpu implementations):

| Token | Value | Use |
|---|---|---|
| `--bg-void` | `#04060A` | Base background (never pure black — avoids OLED smear banding) |
| `--bg-glass` | `rgba(10, 16, 24, 0.55)` | Panel fill, behind blur |
| `--stroke-holo` | `#36C6E3` | Primary wireframe stroke, 1 px |
| `--holo-bright` | `#7DF3FF` | Active/highlight stroke, data in motion |
| `--ice` | `#EAF8FF` | Primary text, core highlights |
| `--ice-dim` | `#8FB4C4` | Secondary text, labels, idle strokes |
| `--cloud-violet` | `#9D7DFF` | Anything cloud-routed (distinct from local cyan) |
| `--warn-amber` | `#FFB454` | Latency over target, heal.suppressed |
| `--alert-red` | `#FF4D5E` | Failures: route.failed, inference.unavailable |
| `--learn-green` | `#5CE0A8` | memory.learned moments only |

Rule: **local = cyan, cloud = violet, learning = green, trouble = amber/red.** No other hues. Glow is bloom on the bright tokens, never a third color.

### 1.2 Typography

| Role | Face | Size/weight |
|---|---|---|
| Numerals & telemetry | SF Mono (fallback JetBrains Mono) | 12–14 px, tabular figures |
| Labels & headers | SF Pro Display | 11 px caps, +8% tracking, `--ice-dim` |
| Transcript / spoken text | SF Pro Text | 16 px, `--ice` |

All-caps labels, sentence-case content. No bold weights above 600; hierarchy comes from color and size.

### 1.3 Glass panels

- Fill `--bg-glass`, backdrop blur 24 px, 1 px stroke at 40% `--stroke-holo`.
- Corner brackets (8 px L-marks) instead of full rounded borders on data panels; 6 px radius on the fill itself.
- Max **3 active backdrop blurs** on screen at once (perf budget §6); additional panels fall back to opaque `--bg-glass` at 0.85 alpha.

### 1.4 Volumetric particle field

- One instanced draw: 30 k particles on M4 (12 k on M1 Pro), point sprites with depth-faded alpha.
- Three parallax depth bands; drift driven by curl noise advanced on the GPU (time uniform only — no per-frame CPU upload).
- The field is an ambient instrument, not decoration: it reacts to pipeline state (§2) and to `system.load` (cpu_percent modulates drift speed ±20%).

### 1.5 Motion rules

**Fast in, soft out.**

| Phase | Duration | Easing |
|---|---|---|
| Enter / state change | 120–180 ms | cubic-out |
| Exit / decay | 300–500 ms | ease (no overshoot) |
| Continuous (core, particles) | — | spring-damped, critically damped, no bounce |

Everything animates outward from the core: panels and overlays stagger 30 ms per 200 px of distance from screen center. Never two simultaneous attention-grabbing animations; failure flashes preempt everything else.

---

## 2. Reactive core

The central element: a wireframe sphere (~3 k line segments, single shader) with an inner volumetric glow, occupying ~22% of screen height at center. Two inputs drive it:

1. **Audio FFT** — HUD-side mic tap, 2048-point FFT at 60 Hz, folded into 48 log bands. Bands displace the sphere's vertices radially (low bands = whole-sphere breathing, high bands = surface shimmer).
2. **Pipeline state** — derived from telemetry events (§3).

### 2.1 State machine

| State | Entered on | Visual signature |
|---|---|---|
| **idle** | startup; `pipeline.completed`; any terminal/error event; 12 s timeout in any transient state | Slow rotation (0.02 rad/s), dim `--ice-dim` wireframe, gentle breathing at ~0.1 Hz, particles at base drift |
| **listening** | HUD-side FFT energy above gate (mirrors the daemon's RMS VAD; there is no "speech started" telemetry event — capture ends, not begins, with `utterance.captured`) | Wireframe brightens to `--holo-bright`; FFT displacement at full gain; a thin equatorial ring expands with input level |
| **thinking-local** | `route.local` (pre-armed dim by `utterance.captured` → `intent.classified`) | Cyan: rotation speeds up 4×, surface shimmer replaced by ordered longitudinal pulses traveling pole-to-pole; particles converge slightly toward the core |
| **thinking-cloud** | `route.cloud` | Same as thinking-local but `--cloud-violet`, plus a single beam line from the core to the top screen edge (data leaving the machine — make routing visible) |
| **speaking** | `response.speaking` | Core modulates to TTS output: HUD taps system output via the FFT path when available, else animates on a synthetic envelope from the spoken text length; wireframe `--ice`, ring pulses outward per phrase |
| **alarm** (overlay, 600 ms) | `route.failed`, `inference.unavailable`, `stt.empty` | One red radial flash from the core, then decay to idle. Never strobes |

Transitions use the motion rules in §1.5. The HUD may connect mid-pipeline; any event maps to its state idempotently, and the 12 s decay-to-idle covers missed terminal events (telemetry is fire-and-forget, per `daemon/src/telemetry.rs`).

---

## 3. Telemetry event map

Every event the daemon actually emits, and what it drives. Sources and payloads verified against `daemon/src/{main,telemetry,router,speech,heal}.rs`.

| Event (source) | Payload | Drives |
|---|---|---|
| `system.load` (system, every 2 s) | `cpu_percent`, `mem_used_bytes`, `mem_total_bytes` | System gauges (bottom-left arc pair); particle drift speed |
| `daemon.started` (system) | `root`, `cloud_key_present` | "JARVISD ONLINE" boot stinger; clears any stale-connection banner |
| `utterance.captured` (audio) | `path` | Ends listening; pre-arms thinking (dim); transcript feed shows a pending slot |
| `stt.transcript` (local) | `text` | Fills the pending transcript slot, typed-on at 80 chars/s |
| `stt.empty` (local) | `path` | Pending slot collapses; brief alarm flick (low intensity) |
| `intent.classified` (local) | `intent`, `confidence`, `complexity` | Intent chip next to the transcript: label + confidence bar; amber if `confidence < 0.6` |
| `route.local` (local) | `intent`, `confidence` | Core → thinking-local; latency ribbon arms the "route" segment in cyan |
| `route.cloud` (cloud) | `intent`, `confidence`, `model`, `deep_reasoning` | Core → thinking-cloud; ribbon segment violet; model id shown on the beam |
| `intent.handled` (local) | `intent`, `text` | Tick mark on the intent chip |
| `route.completed` (local\|cloud) | `routed_to`, `response` | Response text rendered under the transcript in `--ice` |
| `response.speaking` (local) | `text` | Core → speaking; spoken text highlighted word-window style as a karaoke band |
| `pipeline.completed` (system) | `queue_ms`, `stt_ms`, `classify_ms`, `route_ms`, `first_audio_ms`, `speak_ms`, `total_ms` | Latency ribbon fills: per-stage bars vs targets (STT < 1000, classify < 300, first-token < 500 per ARCHITECTURE.md); over-target segments amber; appends to the rolling 50-utterance sparkline; core → idle |
| `route.failed` (system) | `intent`, `error` | Alarm flash; error line in the feed, red |
| `inference.unavailable` (system) | `op`, `error` | Alarm flash; persistent "LOCAL INFERENCE OFFLINE" banner until the next successful event from source `local` |
| `heal.suppressed` / `heal.triggered` (system) | `errors_last_60s`, … | Amber heartbeat dot in the status bar with the burst count |
| `memory.learned` (system, Phase 1.7) | `key`, `value` | Memory ticker (right edge): the fact slides in `--learn-green`, holds 4 s, settles into the known-facts stack |
| `app.*` topics (Phase 4, per SANDBOX.md `telemetry_topics`) | app-defined | Routed to that app's panel (§5); unknown topics drop silently |

Unknown events must never throw — log to the HUD debug console and continue. The daemon will grow events faster than the HUD.

---

## 4. Screen layout

```
┌────────────────────────────────────────────────────────────────────┐
│ status bar: ● daemon  ● inference  ● heal     clock        v-tag   │
│                                                                    │
│   transcript feed                 [ reactive core ]   memory ticker│
│   (last 5 exchanges,                                   (facts as   │
│    newest at bottom,                                    they land) │
│    intent chips inline)                                            │
│                                                                    │
│  system gauges          latency ribbon + sparkline      app panels │
│  (cpu / mem arcs)       (per-stage vs targets)          (Phase 4)  │
└────────────────────────────────────────────────────────────────────┘
```

- The core owns the center; nothing overlaps it in idle.
- All side elements are glass panels (§1.3) that dim to 35% opacity when the core is in a thinking state — attention follows the pipeline.
- A hidden debug overlay (toggled by a keyboard chord, `⌥⌘D`) shows the raw event stream, frame time, and draw-call count.

---

## 5. Panel system (micro-app surfaces)

Per `docs/SANDBOX.md`, micro-apps never open windows; the HUD composites their surfaces. Three surface classes from the manifest `[ui] surface` field:

| Class | Composition |
|---|---|
| `panel` | Floating glass panel in the right-side panel rail; user-arrangeable; max 3 visible, the rest collapse to tabs |
| `overlay` | Translucent layer over the main scene (e.g. Fab-Link toolpath); no fill, strokes only; never occludes the status bar |
| `fullscreen` | Takes the whole stage; core shrinks to a 48 px status orb top-right; exit via the same chord that opened it |

Two transport tiers, matching the roadmap honestly:

1. **Phase 2 (this spec): data-driven panels.** The supervisor and `app.data` relay have shipped (`daemon/src/apps.rs`), and the first micro-app, Global-Scan, runs live with a `panel`-class HUD surface (`hud/src/components/GlobalScanPanel.tsx`); only the IOSurface/embedded-webview compositing below is reserved for Phase 4. The HUD ships the panel framework rendering from **telemetry topics**: a panel definition maps topics (e.g. `fab.progress`, `algo.pnl`) to built-in widgets (time series, level meter, matrix grid, polyline layer). Each app's SPEC.md defines its payload schemas; the HUD renders any topic that has a definition and ignores the rest.
2. **Phase 4: composited surfaces.** Apps that need real rendering (Silicon Canvas) share an **IOSurface** with the HUD: the app draws with its own Metal device (manifest `gpu = true`), the HUD samples the surface as an external texture. Webview-class apps embed as Tauri child webviews. Input: the HUD routes pointer/keyboard to the focused surface and forwards them as JSONL input events over the app's daemon socket — apps never see raw input devices.

Panels carry the owning app's name and a sandbox badge (net hosts count, audio grant) — the security state of an app is always visible.

### 5.1 Settings panel

A `panel`-class glass surface (chord `⌥⌘,`) for the runtime knobs that should not require editing `config/jarvis.toml` over SSH. Settings the daemon owns are applied over a daemon-side channel, not written by the HUD directly — the HUD stays a client.

**Anthropic API key — first-class settings option.** Cloud routing (and Phase-3 self-heal) needs `ANTHROPIC_API_KEY`; entering it must be a first-run settings action, not a shell session:

- A masked key-entry field (`sk-ant-…`, paste-friendly, show/hide toggle) with a **Test** action that round-trips one minimal Messages-API call and reports reachability + model access.
- **Storage: macOS Keychain, never plaintext.** The HUD writes the key as a generic password item (service `com.jarvis.daemon`, account `anthropic_api_key`) via the Security framework; it is never written to disk, config, logs, or telemetry. The panel renders only presence + last-4 characters.
- **Daemon read order at startup:** the `ANTHROPIC_API_KEY` environment variable when set (the existing `state/env.sh` convention keeps working for headless installs and overrides everything), else the Keychain item. After a save from the HUD, the daemon re-reads the key without a restart.
- The status bar shows a cloud-key indicator: green when a key is present and the last test passed, amber when absent — cloud routes then degrade to local, per ARCHITECTURE's routing policy, and the panel says so.

Other v1 entries (all daemon-owned, same apply path): `[speech]` voice pick from the audition bank, `[router]` cloud confidence threshold, `[cloud]` model ids. Everything else stays in `config/jarvis.toml`.

---

## 6. Technology decision

**Recommendation: Tauri 2 + React-Three-Fiber (R3F). Fallback path: native Rust wgpu.**

Rationale:

- **Velocity.** The whole HUD is scene-graph + shader + text + layout work. R3F + drei + postprocessing gives instancing, bloom, SDF text, and a component model on day one; the equivalent wgpu stack (winit, glyphon/cosmic-text, egui or hand-rolled layout, custom bloom) is weeks of scaffolding before the first overlay.
- **Tauri 2, not Electron.** Rust shell matching the project language; WKWebView (system-provided, GPU-composited, supports 120 Hz `requestAnimationFrame` on ProMotion); ~10 MB footprint; native window control via `objc2` for the shell-replacement bits.
- **Shell replacement is shell work, not renderer work.** Fullscreen borderless `NSWindow` at `CGShieldingWindowLevel - 1`, `NSApplicationPresentationOptions` hiding Dock + menu bar, joins all Spaces, `com.jarvis.hud` LaunchAgent installed by `scripts/install_boot.sh` in Phase-2 deployment. Identical regardless of renderer choice.
- **Known risks, named mitigations.** JS GC hitches → zero-allocation render loop (pooled vectors, preallocated typed arrays, no per-frame object literals). WebGL draw-call overhead → everything instanced; target ≤ 40 draw calls. WebSocket parse on the main thread → telemetry decoded in a Worker, state diffs posted to the render thread.

**wgpu fallback trigger** (decided at the M3 gate, not re-litigated before): sustained frame time > 8.3 ms on the M4 at full scene after the M3 optimization pass, or input-to-photon latency > 2 frames on the fullscreen surface class. To keep the fallback cheap, the telemetry client, event→state machine, and panel/topic registry live in a renderer-agnostic core (TypeScript module with no three.js imports; its logic mirrors a future `hud-core` Rust crate) — a renderer swap rewrites drawing, not behavior.

### 6.1 Performance budget

120 fps on M4 = **8.3 ms/frame**. Allocations:

| Layer | Budget |
|---|---|
| Particle field (1 instanced draw, GPU noise) | 2.0 ms |
| Reactive core (wireframe + glow, 2 draws) | 1.5 ms |
| Glass panels + blur (≤ 3 backdrop blurs) | 2.0 ms |
| Text/UI layer (SDF text, DOM only for transcript) | 1.0 ms |
| JS: state machine, FFT fold, telemetry apply | 0.8 ms |
| Headroom (GC, compositor) | 1.0 ms |

Degradation ladder, applied automatically when the 1 s rolling average exceeds budget: 120 → 60 Hz cap, halve particle count, replace backdrop blur with opaque fill, disable bloom. Each step emits a line to the debug overlay. Dev target on M1 Pro: same scene, 60 fps, no ladder steps engaged below step 2.

---

## 7. Implementation milestones

| # | Milestone | Acceptance |
|---|---|---|
| M0 | **Shell spike** — Tauri 2 app: fullscreen always-on-top, Dock/menu bar hidden, WS client connected, frame-time overlay | 120 Hz rAF confirmed on M4 (frame-time histogram); ⌥⌘Q quits; survives daemon restart (auto-reconnect with backoff) |
| M1 | **Core + design system** — tokens, reactive core with all six states (§2.1), mic-tap FFT; driven by a recorded telemetry replay file for development | Every state reachable from replay; FFT visibly drives listening; no per-frame allocations in the render loop (heap snapshot diff flat over 60 s) |
| M2 | **Telemetry overlays** — full §3 event map: transcript feed, intent chips, latency ribbon vs targets, sparkline, memory ticker, status bar, offline banner | Live session against a real `jarvisd`: every emitted event visibly accounted for; unknown-event injection does not throw |
| M3 | **Particle field + perf gate** — volumetric field, bloom, polish pass to the §6.1 budget | 8.3 ms p95 frame time on M4 at full scene; ladder verified by forced load; **go/no-go on wgpu fallback decided here** |
| M4 | **Panel system** — panel rail, three surface classes, topic→widget registry with the widget set needed by the four app SPECs (time series, meter, matrix, polyline, image) | Synthetic publisher exercising each app's documented topics renders correct panels; input routing works on a stub fullscreen surface |
| M5 | **Deployment** — `com.jarvis.hud` LaunchAgent, boot-to-HUD on the Mini, 24 h soak | Power-on → HUD with no interaction; no memory growth > 5% over soak; HUD kill/relaunch leaves pipeline unaffected |
