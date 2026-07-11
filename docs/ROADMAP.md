# JARVIS Roadmap

## Phase 1 — Scaffold ✅ complete

- `jarvisd` core loop: audio capture → VAD → STT → intent classify → route (local | cloud), per `docs/ARCHITECTURE.md`.
- MLX inference server on python3.11 (Metal GPU): `transcribe` / `classify` / `generate` over `state/ipc/inference.sock`.
- Memory subsystem: SQLite at `state/jarvis.db` (`events`, `facts`, `transcripts`).
- Telemetry WebSocket server on `127.0.0.1:7177`, hosted by the daemon.
- Canonical config at `config/jarvis.toml` with hardcoded fallbacks in both processes.
- Sandbox blueprint (`docs/SANDBOX.md`) and the four launch-app manifests under `apps/`.

## Phase 1.5 — Boot integration ✅ complete

- Boot-to-JARVIS on the target Mac (any Apple Silicon Mac): auto-login + launchd LaunchAgents (`com.jarvis.inference`, `com.jarvis.daemon`, both KeepAlive) wrapping `boot/run_inference.sh` / `boot/run_daemon.sh`. Power-on → daemon + local AI live with no interaction. See `docs/ARCHITECTURE.md` § Boot experience.
- Installer: `scripts/install_boot.sh` (dry-run by default, `--install` to apply, `--uninstall` to remove; also `scripts/uninstall_boot.sh`).
- Secrets convention: `state/env.sh` (gitignored, chmod 600), sourced by the boot wrappers for `ANTHROPIC_API_KEY`.
- ANE groundwork pulled forward from Phase 3: `scripts/ane_probe.py` Core ML probe (model cached under `state/ane/`, `--loop N` for `powermetrics --samplers ane_power` verification). Aux-model selection stays in Phase 3.

## Phase 1.6 — Voice & latency ✅ complete

- Neural TTS: new `speak` op on the inference socket — Kokoro-82M on the Metal GPU via `mlx-audio` (`[speech]`: `engine = "kokoro"`, `voice = "am_michael"`, `speed = 1.05`). The daemon plays the synthesized WAV with `afplay` inside the SPEAKING mute window and deletes it afterward; macOS `say` remains as fallback if the op fails.
- Persona: `inference/prompts/persona.txt` — middle-aged FBI-agent register (calm, terse, authoritative, dry) — used as the style prefix on every `generate` call; the daemon's canned responses re-voiced to match.
- Latency: models preload at server start (`[inference] preload = true`); classifier prompt trimmed and KV-cached, `max_tokens = 80`; VAD end-of-utterance tightened (`silence_ms` 700 → 450, `min_speech_ms` 300 → 250); STT model is config-driven (`[models].stt`, benchmark-selected).
- Telemetry: `pipeline.completed` per utterance — `{stt_ms, classify_ms, route_ms, speak_ms, total_ms}` — broadcast on the feed and logged at info level.
- Voice quality and persona iteration continue with the Phase-2 HUD voice work; this phase ships the plumbing and the first register.

## Phase 1.7 — Mind, voice, and specs ✅ complete

- **No fixed response strings.** Local handlers return data, not prose (`HandlerOutput {data, llm_voice}`); every spoken reply is phrased by the LLM in persona via the extended `generate` op (`history` / `facts` / `data`, persona prefix KV-cached). If the inference server is down, the daemon speaks the raw data string — graceful degradation, not a canned personality. Cloud prompts assemble the same way in `anthropic.rs`.
- **Learning loop.** New `extract_facts` op (≤ 3 durable namespaced facts per exchange, strict-JSON with empty-list fallback). After speaking, a non-blocking daemon task extracts facts → `upsert_fact` → `memory.learned` telemetry. `transcripts` gains a `response` column (idempotent migration); `recent_exchanges(6)` + `all_facts(12)` are folded into every reply, local and cloud. See `docs/ARCHITECTURE.md` § Learning loop.
- **Persona and voice.** `inference/prompts/persona.txt` rewritten to the Iron-Man-films JARVIS register — composed British butler-AI, "sir", dry understatement, concise — and made the single source of truth (read by the daemon for cloud calls too). Default voice `[speech] voice = "bm_george"` (British male).
- **Voice audition workflow.** Sample WAVs at `state/voice-samples/<voice>.wav` (`bm_george`, `bm_fable`, `bm_daniel`, `bm_lewis`, `am_michael`), all the same line, generated server-side. Listen with `afplay state/voice-samples/<voice>.wav`, then set `[speech] voice` in `config/jarvis.toml`.
- **State-of-the-art specs (docs only; implementation Phase 2+).** `docs/HUD.md` — the buildable Phase-2 HUD design + engineering spec — and `apps/<name>/SPEC.md` for the four launch apps.

## Phase 2 — HUD ✅ shipped v1

The HUD from `docs/HUD.md` is real: a Tauri 2 + Vite + React + TypeScript + React-Three-Fiber app at `hud/` (see `hud/README.md` for dev/build/test commands and the panel map). It is a **pure telemetry client** of `ws://127.0.0.1:7177` — no state the daemon doesn't have, auto-reconnect with 1 s → 5 s backoff, a dim LINK OFFLINE idle when the daemon is away.

**Shipped in v1 (of `docs/HUD.md`):**

- Design system §1: dark glass (`#05080C`), cyan holographic wireframes, glass-morphism panels (blur, 1 px cyan borders, scanlines, monospace accents).
- Reactive core §2: R3F wireframe icosahedron + inner energy sphere + ~6k-particle orbit field with all six state signatures — idle breathing, listening pulse synced to live mic RMS, processing spin, thinking-local cyan surge, thinking-cloud violet shift + upward particle stream, speaking amplitude pulses. The state machine is a **pure, vitest-covered TypeScript reducer** (`hud/src/core/`, 69 tests).
- Telemetry map §3: transcript feed, CPU/MEM/DISK/UPTIME gauges, 64-bar waveform equalizer from the new `audio.level` events, pipeline latency strip (`pipeline.completed`), learned-fact/action toasts, inference-offline + heal indicators.
- Settings panel §5.1: masked Anthropic API key entry, Save/Test/Remove against the macOS Keychain (`com.jarvis.daemon` / `anthropic_api_key`, `security(1)` args-only, never plaintext, never logged); the daemon resolves env `ANTHROPIC_API_KEY` first, else the Keychain item, once at startup — `daemon.started` reports `cloud_key_present`. Entering the cloud key never requires a shell.
- Performance: bloom postprocessing with an **adaptive governor** — particle count and bloom degrade in tiers when frame time stays over 20 ms.
- F11 + button fullscreen toggle on a 1440×900 window; daemon-side `audio.level` (≤ 1 per 66 ms) feeds the waveform; the consolidation pass now also mines `user.habit.<slug>` facts (lifelong-learning habit mining, ≥ 3 user-line occurrences, deterministically backstopped).

**Deferred from `docs/HUD.md`:**

- §4/§6.1 kiosk takeover is now **BUILT but DEVICE-GATED** (no longer a pure deferral): the `enter_takeover`/`exit_takeover` commands, the enter/exit state machine, the exit-safety logic (macOS `HideDock|HideMenuBar` presentation-options under `#[cfg(target_os = "macos")]`, reset-on-exit/Drop), and the `TakeoverStage` React layout are implemented and tested (`hud/src-tauri` cargo + `hud` vitest). What remains DEVICE-GATED is the actual fullscreen render + real Dock/menu-bar suppression on a live display — proven only via cargo/vitest, never rendered or observed headlessly. It ships OFF (`tauri.conf` `fullscreen: false`) and is never auto-entered. Still deferred: the §6.1 always-on-top shell replacement and the 120 fps ProMotion budget (v1 targets 60 fps with the adaptive tiers).
- §2 audio-FFT-driven visuals — v1 drives the core and equalizer from the daemon's RMS `audio.level` feed, not a spectral FFT.
- §5 micro-app panel surfaces — **now landed** for `panel`-class apps: the Global-Scan panel renders from the Phase-4 `app.data` telemetry relay (`hud/src/components/GlobalScanPanel.tsx`); richer wgpu-texture/embedded-webview compositing (§6) is still reserved — R3F holds the budget.

## Phase 3 — Self-heal + ANE auxiliary models

- Self-heal **v2 has landed** (`daemon/src/heal.rs`, ships **ON** (armed), **PROPOSE-ONLY** (`mode = "propose"`) — INERT without a cloud key: the heavy-model diff draft needs `ANTHROPIC_API_KEY`, else the watchdog emits `heal.blocked{reason:"no_api_key"}` and patches nothing): error-burst detection → root-cause diagnosis (with the cited source attached) → N Opus candidate diffs → per-candidate staged `cargo check`+`cargo test` → adversarial review + confidence → proposal for gated human apply (`scripts/apply_heal.sh <ts>`); the HUD surfaces it in a warn-amber SELF-REPAIR panel. The full loop is battle-tested through the real cloud by `jarvisd --heal-drill` against a planted fault in a throwaway crate. **Remaining for this phase:** soak-test in propose mode against real bursts (auto mode stays opt-in and documented dangerous). See `docs/ARCHITECTURE.md` § Self-heal v2.
- Optimization-from-usage loop **API has landed and is now wired live** (`daemon/src/optimize.rs`, ships **ON**, `mode = "propose"`; live recording is runtime-gated + PII-redacted): a local PII-redacted **Trace Store** records each interaction + outcome, and an **Optimizer** tunes **only agent routing** — a thin cue-weight layer over `agents.rs` `CUE_VOCAB`. It splits traces into train + HELD-OUT, generates a bounded candidate set, and adopts the best **ONLY IF** it beats the baseline by the margin on held-out traces **and** regresses no class — so it can't make JARVIS worse. It is **propose-only**: a passing candidate is written as a reviewable `proposal.md`/`proposal.json` diff for gated human apply (`scripts/apply_optimization.sh <ts>`, a reversible `CUE_VOCAB` source edit) and **never** silently mutates live behavior. Fully hermetically unit-tested with **mock traces**. **Wired (done):** `record_trace` is called from the per-turn bookkeeping path (gated by `[optimize].enabled`, NO-OP when OFF, skipped for transient screen-read turns) with cross-turn `CorrectedNextTurn` labelling, and `run_optimizer` runs in a gated periodic `optimize_task` — both **propose-only** (mode KEEPS `"propose"`; no auto-apply-to-live path), so live wiring only makes the machinery reachable and never auto-tunes routing. **Remaining for this phase:** operational soak — accrue a real corpus at **runtime** with `[optimize] enabled = true` and review proposals in propose mode (the wiring itself is no longer pending). See `docs/ARCHITECTURE.md` § Optimization-from-usage loop.
- Core ML auxiliary models: wake-word, VAD, and embeddings exported to Core ML so Core ML may schedule them onto the Neural Engine, freeing the GPU for MLX. Replaces the RMS-gate VAD with a learned one. (ANE probe groundwork landed in Phase 1.5; what remains here is model selection and export.)

## Phase 3 — Agent constellation (the "council") 🟡 framework v1 shipped

**Shipped (framework v1):** the constellation *experience* runs on the existing engine — 27 named agents as **profiles on one 4B LLM + Kokoro TTS**, orchestrated by Jarvis-Prime, with the central core changing color per active agent. Delivered:

- **Roster + registry** — `config/agents.toml` (27 `[[agent]]`: name, role, voice, hue, persona_file, tools, namespace) → typed `AgentRegistry` (`daemon/src/agents.rs`, serde + `deny_unknown_fields`, validated, canonical-roster fallback). 27 persona files (`inference/personas/*.txt`), each ~120–180 words in role + register on the butler base, each carrying an `INTRO:` self-introduction line. HUD mirror in `hud/src/core/agents.ts`.
- **Jarvis-Prime delegation** — deterministic, unit-tested rule map in `router.rs::select_agent` (intent + whole-word keyword cues → agent; unmatched → jarvis; offline conversation → hulk). Emits `agent.active{name, role, hue}`; the selected agent runs the existing converse/cloud pipeline under its own persona, voice, namespace, and allowlist.
- **Per-agent voice + persona** — the `converse` op now accepts an optional `persona` name (server maps it to `inference/personas/<name>.txt`, bypassing the base KV cache; backward-compatible) and the agent's Kokoro voice, so each agent both phrases and sounds like itself.
- **Tool isolation** — `router.rs::enforce_tool` hands an out-of-allowlist intent to the tool's real owner (least-privilege compartmentalization; the security win). Memory recall is namespaced (`memory.rs::agent_scoped_facts`): own namespace + shared only, `meta.*` filtered.
- **Roll-call** — "roll call / introduce the team / assemble" sequences all 27 agents speaking their `INTRO:` in their own voice, emitting `agent.active` per agent so the HUD highlight + core color cycle; interruptible.
- **HUD team layer** — `CONSTELLATION // AGENTS` roster panel (active agent glows in its hue), per-agent core-color override (lerped, anti-flicker invariants held), `ACTIVE: <agent>` status chip; `scripts/hud_demo_feed.py` emits a roll-call `agent.active` sequence for headless verification. See `docs/ARCHITECTURE.md` → *Agent constellation*.
- **Conversation answers from cloud Opus by default (`[router].conversation_route = "cloud_heavy"`).** The local 4B (Qwen3-4B) is near-deterministic on bare greetings even at max temperature — a model-capacity ceiling — so casual chat / greetings / opinions (the `conversation` intent) now route to a **plain cloud persona completion** (`anthropic::complete_persona`, no tool loop — a greeting must never call tools) on cloud Opus for genuinely varied, human personality; `"cloud_fast"` uses Haiku and `"local"` keeps the resident 4B. The local 4B converse path is the **offline fallback**: with no cloud key or on any cloud error, conversation degrades to it gracefully (never silent). Actions, `system.query`, memory ops, and the existing heavy/low-confidence cloud routing are untouched. One config line reverts chat to the 4B. See `docs/ARCHITECTURE.md` → *Routing policy*.

**Next — per-agent muscle (each integration needs the user's own credential, added one at a time):** the framework is the skeleton; the muscle is real external reach. Veronica → email/social **send** (Gmail/X/LinkedIn). Steve → **GitHub** (open PRs, read CI). Oracle/Pepper → **Slack** + calendar/**Drive**. Vision/Stark → ads + competitor data sources. Gecko → Algo-Core (`apps/algo-core/SPEC.md`). Each ships OFF until the user supplies and authorizes the specific credential — no account is assumed, nothing acts on the user's behalf without that explicit grant. Cloud-side allowlist filtering (threading each agent's `tools` into `complete_with_tools`) is a tracked follow-up.

---

The original plan (still the reference for the per-agent build order below):

A roster of specialist sub-agents under JARVIS as **Prime Orchestrator**. NOT 27 separate
processes/models (16 GB cannot hold them) — ONE shared inference engine with a per-agent
**profile**: persona prompt, allowed-intent set, tool/actuator allowlist, memory namespace
(`agent.<name>.*` facts + a shared pool JARVIS sees), model tier, and data scope (local-only
vs cloud vs internet). The existing router becomes the orchestrator: classify intent → select
agent → run the existing converse/cloud pipeline under that agent's profile. The security win
is **compartmentalization** — each agent runs least-privilege with scoped tools and memory
(the SANDBOX.md capability-token model), so a compromised or misled agent can't reach beyond
its lane. That isolation, not the renaming, is what hardens the system.

| Agent | Domain | Notes / scope |
|---|---|---|
| **Jarvis** | Prime Orchestrator | The existing router + delegation layer; owns hand-off, conflict resolution, the shared memory pool. |
| **Friday** | Daily Intel | Morning brief, calendar, news digest, day plan. Builds on the proactive layer + Global-Scan. |
| **Veronica** | Content + Comms | Drafting messages, posts, emails, replies. Cloud-heavy (Opus). |
| **Vision** | Research + OSINT | Deep web research; OSINT **scoped to the user's own footprint / authorized targets only**. Internet + cloud. |
| **Ultron** | Security + Automation | **Defensive** monitoring of this Mac + LAN, automation/macros. Local. No offensive tooling, no third-party targeting. |
| **Athena** | Greek Life Strategy | Events, networking, chapter strategy. Personal/cloud. |
| **Stark** | Business Intel | Market/competitor/company research for the user's ventures. |
| **Steve** | CTO + builds | Coding, scaffolding, dev tasks. Cloud heavy model + tool loop. |
| **Oracle** | Workflows | Multi-step automations, chained actions, scheduled routines. |
| **Gecko** | Markets + capital | Trading/markets; ties to Algo-Core (`apps/algo-core/SPEC.md`). Engineering only — no profitability claims. |
| **Hercules** | Fitness + nutrition | Tracking, plans, logging. |
| **Pepper** | Personal EA + reflection | Scheduling, reminders, journaling/reflection over memory. |
| **Hulk** | Offline survival | Fully-local fallback mode — no internet, local reference knowledge. A resilience feature. |
| **Herald** | Meetings | Capture, transcription, notes, action items. |
| **Jerome** | Leisure + DJ | Music control, entertainment, playlists. |

**Honesty note (recorded with the user):** most of these are capability/persona expansions,
not security per se — the genuine security members are **Ultron** (defensive), **Vision**
(authorized OSINT), and **Hulk** (offline resilience). The system-level security gain comes
from the agent-isolation architecture (least-privilege profiles), which is real and worth
building carefully. Ultron/Vision are dual-use and stay defensive + authorized-only by design.

**Build shape:** agent registry/profile schema first (one engine, swappable profiles, scoped
memory + tools), then agents in value order — Friday/Pepper (ride the proactive layer that
already exists) → Steve/Oracle (ride the tool loop) → Gecko (rides Algo-Core) → Ultron/Vision
(defensive security, scoped) → the rest.

## Phase 4 — Micro-app runtime 🟡 substrate shipped + first app live

**Shipped:** the micro-app runtime substrate from `docs/SANDBOX.md` is **implemented** in `daemon/src/apps.rs` — manifest parsing (`deny_unknown_fields`), per-app `sandbox-exec` (seatbelt) profile generation (default-deny + only the manifest's grants), per-launch HMAC capability tokens (verified on every line, nonce-rotated, key never logged/relayed), per-app Unix socket (`state/ipc/apps/<name>.sock`, `0600`) with a newline-delimited JSON protocol, a supervised lifecycle (bounded restart governor, `kill_on_drop`), and a telemetry relay (`app.started`/`app.stopped`/`app.crashed`/`app.data`/`app.log`/`app.auth_failed`) so HUD panels render without their own socket. Voice "open/close <app>" resolves against the app registry. The runtime is unit-tested (manifest parse, SBPL default-deny + exact-allows, token forgery/tamper/cross-app/stale rejection, restart math, inbound-line classification) plus one hermetic seatbelt integration test.

- **Daemon-mediated `generate` proxy** (`daemon/src/genproxy.rs`) — **SHIPPED**, pulled forward from Phase 4. Closes security-review finding #4 (confused-deputy via the inference socket): micro-apps no longer reach the multiplexed `inference.sock` at all. They are granted only a separate, op-restricted socket `state/ipc/apps/generate.sock` (`0600`) that accepts **only** `op=generate` (privileged ops are structurally unrouteable, not blocklisted), is token-gated via the existing per-launch HMAC machinery, clamps `max_tokens` to 256, and rate-limits to 30 calls/60 s per app. The inference server itself is unchanged. Global-Scan was repointed to the proxy; on any rejection or an unreachable proxy it falls back to extractive summaries. Unit-tested (op gate per privileged op, token forgery/tamper/cross-app/missing rejection, clamp, per-app rate limit, valid round-trip via a loopback responder, SBPL AF_UNIX connect grant for `.sock` `fs_read` entries). See `docs/SANDBOX.md`.

- **Global-Scan** (`apps/global-scan/`) — **SHIPPED**, the first app on the substrate: a world-intel feed aggregator that polls ~9 reputable, non-paywalled public RSS/Atom feeds (NPR, BBC, Ars Technica, Hacker News, The Verge, ScienceDaily, NASA, WSJ/MarketWatch markets), dedupes and ranks newest-first, optionally adds a neutral local-LLM one-line summary (graceful extractive fallback when inference is down), and renders an FUI **GLOBAL-SCAN // INTEL FEED** panel in the HUD (`hud/src/components/GlobalScanPanel.tsx`). `net_hosts` is kept in lockstep with `feeds.toml`; honest framing — aggregates and summarizes public feeds, no prediction, no surveillance. This gives the **Friday** (daily intel / news digest), **Vision** (research), and **Stark** (business/market intel) constellation agents a real, sandboxed data source to build on.

- **Micro-app introspection** (`daemon/src/introspect.rs`, `docs/INTROSPECT.md`) — **SHIPPED**, a DEFENSIVE, READ-ONLY sentinel layer over the runtime: SBPL seatbelt **profile-drift** detection (fingerprint vs on-disk tamper), per-app **RSS/CPU anomaly** classification (via `sysinfo`, same-UID, no entitlement), cooperative **dyld module attestation** (trust-on-first-use baseline; the in-proc `apps/_sdk/dyld_report.py` + a Swift `DyldReport` report the loaded-module set over the existing tokened socket to catch injection / unexpected `dlopen`), a **declared-capability inventory** (what each app is allowed to do, incl. the new `jit` manifest key), and a pure **Endpoint-Security event classifier** seam (the live ES front-end is device-gated/deferred). All read-only, secret-free, surfaced in the HUD `IntrospectPanel` + `posture.rs` + the `aegis_introspect`/`aegis_report` cloud tools; it observes and reports, it never kills/unloads/rewrites. Extensively unit-tested across daemon/Swift/HUD with the wire contract anchored (`introspect.*` telemetry builders).

**Also shipped** — two more launch apps now run on the substrate (SPEC surfaces noted honestly):

- **Nexus** (`apps/nexus/`) — **SHIPPED**: the audio-routing/monitoring app is built and tested (`apps/nexus/main.py` + `core/` + `test_main.py`) with LUFS + audio-clipping telemetry (BS.1770 histogram, alloc-free RT clip detection, `nexus_drain_clips`, PR #18). Still reserved from the SPEC: AUv3 hosting and the sub-10 ms CoreAudio aggregate-device monitor path.
- **Silicon Canvas** (`apps/silicon-canvas/`) — **SHIPPED**: the wgpu schematic/PCB renderer crate is built and tested (bounded IPC read-loops, layer-span PCB stackup, sexpr-finite parsing; PRs #12/#15/#16). Still reserved from the SPEC: KiCad import breadth and the 60 fps / 10k+ component budget.

**Still to build** — the other two launch apps, each against its SPEC.md (spec-only manifests today):

- **Algo-Core** (`apps/algo-core/SPEC.md`) — event-driven engine, WASM-sandboxed strategies, walk-forward validation, risk limits + kill-switch, signed SQLite audit log. Engineering spec only — no profitability claims.
- **Fab-Link** (`apps/fab-link/SPEC.md`) — Moonraker/OctoPrint websocket telemetry, progress-synced toolpath render, thermal/timelapse panels, Phase-3 ANE vision hook reserved.
