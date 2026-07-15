# DARWIN-OS — Advanced Capability Matrix: Doability & Phase 1

This document assesses the "Canonical Behavioral & Task Matrix" and "Autonomous
Self-Healing Core" specification against (a) what DARWIN already is, and (b)
what is physically and computationally possible on Apple Silicon (M1 or later;
developed on an M4 Mac Mini). It then defines Phase 1 of building toward that spec.

**Verdict: doable as a capability framework, not as a literal transcription.**
Three claims are cinematic and are reframed to their real engineering
equivalents below. Nothing here requires pretending a camera can do what a
camera cannot.

---

## 1. The Self-Healing Core — already ~90% built

The entire second section of the spec describes what darwind already is.

| Spec requirement | Status | Where it lives |
|---|---|---|
| Self-monitoring Rust daemon that "runs completely on itself" | ✅ **Built** | `daemon/` — darwind, LaunchAgent boot, KeepAlive |
| Local ultra-fast MLX model matrix for intent classification + offline tasks | ✅ **Built** | `inference/server.py` classify (Qwen3-4B on Metal), local handlers |
| Dynamic fallback that routes to Anthropic **only when the task exceeds local hardware** | ✅ **Built** | `router.rs` — cloud iff `complexity=="heavy" \|\| confidence<0.6`; `anthropic::complete_with_tools` |
| Continuously ingest its own error logs | ✅ **Built** | `heal.rs` — tails `state/logs/daemon.log`, edge-triggered error-burst detection |
| Read-only introspection of its OWN sandboxed apps (integrity + resource + module attestation) | ✅ **Built** | `introspect.rs` — SBPL profile-drift, RSS/CPU anomalies, cooperative dyld module attestation, capability inventory; surfaced via `aegis_introspect`/`aegis_report` + HUD. See `docs/INTROSPECT.md` |
| Write its own patches | ✅ **Built** (propose-only) | `heal.rs` — on a confirmed crash-loop, Opus drafts a unified diff; it is validated and written to `state/heal/proposals/<ts>/`; a human applies it via `scripts/apply_heal.sh` |
| Dynamically compile its modules on the fly | ✅ **Built**, human-gated | `scripts/apply_heal.sh` — revalidates the patch in a staging copy (`cargo check` + `cargo test`), then applies to `daemon/` + `--release` rebuild; never automatic |

**The honest carve-out on self-healing:** the mechanical version of "writes its
own patches and recompiles unsupervised" is buildable, but a daemon that
rewrites and reloads its own code with no gate is how you get a bricked machine
or a silent takeover of its own safety checks. The committed design (and the
standing project rule) is: detect crash-loop → request a diff from Opus → apply
to a **staging copy** → `cargo check` must pass → **human approves** the
hot-swap. Ships ON (`[self_heal] enabled=true`) but **PROPOSE-ONLY** (`mode = "propose"`)
and inert without a cloud key; it never auto-applies, and the
verification gates are never removed. Phase 1 activates the *pipeline up to the
gate*, not past it.

---

## 2. The Task Matrix — what's real, what's reframed

Every capability below plugs into the **existing spine**: classify → route →
either a local actuator (`actions.rs`) or the cloud tool-loop
(`complete_with_tools`). Adding a capability = adding a sandboxed module that
registers tools + intents. We already proved this spine with app-launch, file
search, web open, volume, and memory. The task matrix is "more tools on the
same rail," plus a few genuinely new subsystems.

| # | Spec area | Verdict | Real engineering form |
|---|---|---|---|
| 1 | **Environmental & IoT Automation** | ✅ Real | Smart-home read + gated control through the user's OWN Home Assistant (or compatible) hub over its local REST API — raw HomeKit/Matter is not cleanly reachable from a macOS daemon, so DARWIN relays to the hub and the hub talks to the devices; control previews (dry-run) by default. "Predictive hardware maintenance" → SMART/thermal/battery-cycle trend monitoring of *this Mac*. |
| 2 | **R&D & Prototyping** | 🟡 Real, scoped | Parsing architectural/engineering notes → LLM (trivial). "Structural stress simulation" → a real FEA pipeline (you supply a meshed CAD model + loads + materials; it runs a solver) — **not** "glance at a part and know if it holds." 3D schematic render → already specced as the **Silicon Canvas** app. |
| 3 | **Global Data Aggregation** | ✅ Real | Scheduled public-API polling, multi-source search, stream ingestion, LLM filtering for macro-trends. Direct extension of the web tools already built. |
| 4 | **Predictive Kinematics & Spatial Analysis** | 🔴 Mostly cinematic | **Cannot** assess "material structural integrity" from a camera — a lens sees surfaces, not internal fatigue/load capacity (that needs ultrasound/X-ray/strain gauges). Real form: a vision model that detects *visible surface defects* (cracks, wear) and does *narrow* object tracking + simple ballistic/kinematic prediction in a controlled scene. General "decode any motion and predict it" is not buildable. |
| 5 | **Physiological Telemetry** | 🟡 Real, non-medical | Ingest authorized-wearable data (e.g. WHOOP) via the user's own developer app + OAuth consent — Apple Health/HealthKit is iOS/watchOS only and is **NOT** reachable from macOS, so it is out of scope here. Compute exertion/stress *indicators*; surface threshold *notifications*. **Not** a diagnostic device and won't be presented as one. |
| 6 | **Autonomous Subsystem Coordination** | ✅ Real | Process priority (`nice`/`renice`), power management (`pmset`/`caffeinate`), background-workflow prioritization, on-command data-protection (trigger Time Machine, lock screen, eject/encrypt). All real macOS levers. |

**Concurrency** ("process all of these inputs concurrently") is real: each
capability is an independent sandboxed module per `docs/SANDBOX.md`, feeding the
telemetry bus; the daemon already runs an async event loop and a 2s telemetry
task. That generalizes cleanly to N capability modules.

### On-device document search (file RAG) — shipped, opt-in, private

A semantic search over the user's OWN files (`daemon/src/docsearch.rs`), on the
same classify→route spine: intents `docsearch.index` / `docsearch.forget`, plus
the read-only `doc_search` tool owned by Mnemosyne. Honest properties:

- **Ships ON, inert without roots** — `[docsearch].enabled = true`, `roots = []`. It indexes
  ONLY the folders the user explicitly allowlists — **never a whole-disk scan** —
  text-like files (markdown / txt / code / json / csv …) plus born-digital PDF +
  Office (`.docx` / `.xlsx` / `.pptx`) text (see below).
- **On-device + private** — file contents and their embeddings **never leave the
  device**. Embedding is the on-device MLX `embed` op; when that model is down,
  search falls back to lexical **BM25** and **reports which method actually ran**
  (it never claims neural when it fell back).
- **Cited, never fabricated** — every result cites the **real** indexed chunk
  (file path + byte offset + snippet); an empty index or no match returns
  nothing, never an invented citation.
- **Bounded + forgettable** — an evict-oldest cap; "forget my file index" wipes
  the whole index.
- **Born-digital PDF + Office text IS extracted on-device** — `.pdf` and
  `.docx / .xlsx / .pptx` text is mined by pure-Rust, on-device extractors
  (`docsearch.rs`); PDFs run inside a **memory-jailed helper subprocess**
  (`src/bin/pdfjail.rs`, `RLIMIT_AS` + decompression-bomb guards) so a malformed
  or bomb file aborts the child, never darwind. Extraction stays within the
  allowlisted roots. **Scanned/image-only or encrypted files yield no text → an
  honest skip, never silently indexed** (do not assume such a PDF was read).
  This is distinct from the Spotlight filename *Find files* lookup, which is a
  one-off name/Spotlight search, not a semantic index of file *contents*.

---

## 3. What this changes architecturally

Today the daemon has **~100+ hardcoded, agent-scoped tools** (`anthropic.rs`
`tool_defs()`). To host an open-ended capability
matrix without rewriting the router each time, three pieces are added:

1. **Capability modules** — each domain (IoT, Health, Data, System) is a
   sandboxed micro-app (existing `SANDBOX.md` model: seatbelt profile, capability
   token, JSONL IPC) that **registers** its intents + tool definitions at launch.
2. **Dynamic tool registry** — classify's intent taxonomy and the cloud
   tool-loop's tool array are assembled from installed modules, not a constant.
   (The Anthropic API supports exactly this; tool-search keeps the prompt small.)
3. **Capability manifest** — `apps/<name>/manifest.toml` (already specced) gains
   an `[intents]` + `[tools]` block declaring what the module exposes.

---

## 4. PHASE 1 — Foundation + first real capabilities

Goal: turn the hardcoded actuator set into a **pluggable capability framework**,
prove it with the capabilities that need *zero new hardware*, and bring the
self-healing pipeline up to (not past) the human gate.

### 1.1 — Capability framework
- Define the capability-module contract: manifest `[intents]`/`[tools]`, the
  register-on-launch handshake, the capability-token scoping per `SANDBOX.md`.
- Refactor `actions.rs`'s existing tools (app/file/web/volume/memory) into the
  first capability module ("core.system") behind the registry — proves the
  framework against known-good behavior, no user-visible change.
- Dynamic tool registry: classify taxonomy + `complete_with_tools` tool array
  built from the installed module set.

### 1.2 — System & Power Coordination module (spec area 6, fully real)
- Tools: `set_process_priority`, `power_mode`, `prevent_sleep`,
  `start_backup` (Time Machine), `lock_screen`, `storage_health` (SMART/thermal).
- Benign-only discipline continues: no destructive ops without explicit
  confirmation; nothing that can brick the machine.

### 1.3 — Health Telemetry (read) module (spec area 5, real, non-medical)
- Ingest an authorized-wearable feed (e.g. WHOOP, via the user's own developer
  app + OAuth consent) into the SQLite store; threshold notifications via
  telemetry. Apple Health/HealthKit is iOS/watchOS only and is **not** reachable
  from macOS, so there is no HealthKit path here.
- The original spec's placeholder tools `health_summary` / `exertion_today` are
  **superseded by the shipped WHOOP implementation** — the read surface is the
  read-only `vitalis_recovery` / `vitalis_sleep` / `vitalis_strain` tools (plus
  `connect_whoop` for the one-time consent), all behind the Vitalis agent.
- Framed as indicators, never diagnosis.

### 1.4 — Data Aggregation module (spec area 3, real)
- Scheduled public-API monitors (config-defined sources); LLM trend-filtering;
  `watch_source` / `latest_signals` tools. Builds directly on the web tools.

### 1.5 — Self-healing pipeline to the gate (self-heal core)
- The patch drafter (`heal.rs`, shipped): on a confirmed crash-loop, send the
  error excerpt + relevant module source to Opus, get a unified diff.
- Apply to a **staging copy** of the tree, run `cargo check`, capture the result.
- **Stop there**: emit a "patch proposed, cargo check {passed/failed}" telemetry
  event for human review. No autonomous application, no hot-swap. Ships ON
  (`[self_heal] enabled=true`), **propose-only** (`mode = "propose"`), and inert
  without a cloud key.

### Explicitly deferred / reframed (not in Phase 1, stated honestly)
- **IoT/Home control** (area 1): shipped as the **Dum-E** agent over the user's
  OWN Home Assistant (or compatible) hub — no HomeKit entitlements; the hub owns
  the device link and DARWIN relays reads + gated (dry-run-by-default) control to
  it over the local REST API.
- **FEA / 3D schematics** (area 2): real but heavyweight → rides on the
  Silicon Canvas app, post-HUD.
- **Vision kinematics / material-integrity scan** (area 4): the camera-based
  "structural integrity" claim is **not buildable** and is replaced by a narrow
  surface-defect + object-tracking scope, scheduled only after a vision model
  runs on the ANE (Core ML, the Phase-3 ANE work). The cinematic version is not
  promised.
- **On-device screen reading (OCR)** (area 4, shipped): the Vision app reads the
  user's OWN screen on request ("what's on my screen" / "read my screen" /
  "where's the `<X>` button") via Apple's built-in `VNRecognizeTextRequest` over a
  single ScreenCaptureKit frame — fully **offline**, ANE/GPU-eligible. **TCC
  (Screen Recording) is the real gate** — runtime user consent, not SBPL-grantable
  — so live capture is device-gated (the OCR engine is proven headlessly over a
  synthesized image in CI). **The Vision app stays READ-ONLY: it LOCATES/DESCRIBES
  a control (returns a center point as a "where", explicitly *not* a click
  target), it never clicks** — there is deliberately no actuation API inside the
  Vision app, and that does not change. **DEFENSIVE: glyph text only, never a
  face/ person id.** The recognized text is sensitive and **TRANSIENT** — kept off
  lifelong memory and optimizer traces by default; the **on-device brain is the
  privacy-preferring path** (if the cloud brain answers, that text reaches the
  cloud like any user content). Op-gated; never continuous screen-watching. See
  docs/SANDBOX.md "Vision OCR screen read".
- **Gated UI automation / actuation** (area 4, #44 — the capstone): a **SEPARATE,
  maximally-gated `ui_actuate` op now exists** in the daemon (NOT in the Vision
  app, which stays read-only above). It pairs with the OCR locate — *OCR locates a
  control → the user confirms → the daemon actuates* — but the actuation is its
  own op: it performs **exactly ONE** UI action (a single CGEvent mouse click / a
  keyboard type / a key combo). It **ships ON by default**
  (`[ui_automation].enabled = true`) but NEVER auto-runs and is **inert without
  Accessibility TCC consent + a real display**; with it off the actuate intent is
  never classified and the tool is inert. A PURE single-action **planner** validates +
  bounds each action (a click must land on a real on-screen pixel; an empty
  type/key or a degenerate instruction is refused) and **can never carry a batch**
  — one plan is one actuation by construction. It is **PER-ACTION gated**: every
  actuation is consequential, so it **parks for a spoken human "yes"** and **ONE
  confirmation authorizes EXACTLY ONE actuation** — a second action re-parks for
  its own confirm; **never batched, never an autonomous loop**. It actuates only
  under the consequential-actions **master switch ON + the confirm + voice-id +
  `!lockdown`**. The actuation itself is **DEVICE-gated**: it requires the macOS
  **Accessibility (TCC) permission** — runtime user consent DARWIN cannot
  self-grant (not SBPL-grantable) — and a real display; the CGEvent/AX seam is
  **built but never invoked in any test** (the planner + the gate routing are
  proven hermetically). An actuation result is **never fabricated**: when consent
  is absent the op says so honestly and acts on nothing.

---

## 5. One-line summary

The self-healing brain you specced is mostly built; the task matrix is a
framework-plus-modules job that the existing classify/route/tool-loop spine
already supports; and the only parts that aren't real are the camera-sees-
material-fatigue and glance-and-simulate-physics claims, which become a narrow
vision module and a bring-your-own-model FEA pipeline respectively. Phase 1
builds the framework and the three hardware-free capabilities, and arms the
self-healing pipeline up to the human approval gate.
