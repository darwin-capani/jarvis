# DARWIN HUD

The fullscreen face of the machine: a Tauri 2 + React + TypeScript + React-Three-Fiber
app that renders live `darwind` telemetry as a dark-glass, cyan-holographic heads-up
display. Design spec: [`docs/HUD.md`](../docs/HUD.md).

The HUD is a **pure client** of the daemon's WebSocket telemetry broadcast on
`ws://127.0.0.1:7177` (envelope `{"ts", "source", "event", "data"}`). It holds no
state the daemon doesn't have, never binds the port, never records audio, and can
crash or restart at any time without touching the voice pipeline. When the daemon
is away it idles under a dim **LINK OFFLINE** badge and reconnects with 1s→5s backoff.

## Development

```sh
cd hud
npm install
npm run tauri dev     # full shell (window, keychain commands, F11)
# or: npm run dev     # frontend only, in a browser (settings disabled)
```

## Production build

```sh
npm run tauri build   # bundles DARWIN.app (macOS only)
```

## Tests & checks

```sh
npx vitest run                  # headless state-core suite (69 tests)
npm run build                   # tsc + vite production build
(cd src-tauri && cargo check)   # Rust shell
```

The telemetry reducer, envelope parsing, visual state signatures, and the adaptive
performance governor live in plain TypeScript modules (`src/core/*`) with no
DOM/Tauri/three.js imports — vitest covers them headlessly: every core-state
transition, malformed JSON, unknown events ignored without churn, ring-buffer caps,
reconnect → offline → idle, the 12s stuck-state decay, and the degradation ladder.

> Dependency note: `src-tauri/Cargo.toml` pins `time = "=0.3.47"` — `time 0.3.48`
> breaks `cookie 0.18.1` / `tauri-utils 2.9.2` with E0119 conflicts. Remove the pin
> once upstream ships fixes.

## What's on screen

| Surface | Driven by | Shows |
|---|---|---|
| **Center core** (R3F) | core state machine | Wireframe icosahedron + inner energy sphere + ~6k-particle orbit field. Idle = slow breathing; listening = pulse synced to live mic RMS; processing = accelerated spin; thinking-local = cyan surge; thinking-cloud = violet shift + upward particle stream; speaking = amplitude pulses (synthetic envelope, RMS-modulated when available). Bloom included; particles + bloom degrade automatically if frame time stays over 20ms. |
| **Status bar** (top) | connection, `daemon.started`, `inference.unavailable`, `heal.*` | LINK / INFERENCE / CLOUD KEY indicators, heal burst counter, current core state, clock, fullscreen + settings buttons. |
| **Latency strip** | `pipeline.completed` | Stacked stt / classify / route / speak bar with ms labels, total, and a first-audio marker. |
| **Left panel** | `stt.transcript`, `route.completed`, `intent.classified` | Transcript feed (YOU / DARWIN lines, cloud replies tagged violet, autoscroll) with an intent chip (label + confidence bar, amber under 0.6). |
| **Right panel** | `system.load`, `memory.learned`, `action.executed` | CPU / MEM / DISK / UPTIME animated gauges plus the learned-facts and actions tickers. |
| **Bottom strip** | `audio.level` | 64-bar canvas equalizer folded from the 128-sample RMS history; idle shimmer when silent; violet while DARWIN itself is speaking (mic muted). |
| **Toasts** | `memory.learned`, `action.executed`, `memory.consolidated` | `LEARNED: key = value`, `ACTION: tool — outcome`, consolidation summaries. |

A red **LOCAL INFERENCE OFFLINE** banner appears on `inference.unavailable` and
clears on the next successful event from source `local`.

## Fullscreen

`F11` toggles fullscreen, as does the `⛶` button in the status bar (Tauri window
API). The window opens 1440×900, titled **DARWIN**, on `#05080C`.

### Kiosk takeover (Phase-2, device-gated)

Distinct from the F11 toggle, the **kiosk takeover** turns the HUD into a
full-desktop holographic environment: a fullscreen always-on-top stage
(`TakeoverStage`) into which the command deck and panels re-render, with the
macOS Dock and menu bar suppressed (`HideDock | HideMenuBar` presentation
options, `#[cfg(target_os = "macos")]`). It **ships OFF** — `tauri.conf` sets
`fullscreen: false` and nothing auto-enters it; the user enters explicitly and
the backend records every presentation mutation so exit can reverse it exactly.

**Exit is always reachable — the operator can never be locked out of macOS.**
Three exits are wired today, each calling `exit_takeover`:

1. the **visible `⤢ EXIT TAKEOVER` control**, rendered unconditionally by
   `TakeoverStage` (no prop/state/branch can hide it);
2. the **Esc** key handler in `App`;
3. the **backend reset-on-exit / Drop safety net** — `exit_takeover` reverses
   every recorded mutation in inverse order and then unconditionally restores the
   default presentation options, and macOS auto-restores the Dock/menu bar on
   process death regardless.

A fourth, OS-level **global shortcut** is *optional and not wired today* (the
`global-shortcut` plugin is not a dependency); the three exits above plus macOS
process-death auto-restore already prevent a permanent lock-out.

**Honesty / what is device-gated:** only the command/exit wiring, the enter/exit
state machine, the exit-safety logic, and the React layout are proven here —
hermetically, via `src-tauri` cargo tests + `hud` vitest. The **actual fullscreen
render and the real Dock/menu-bar hide are DEVICE-GATED**: they require a live
Tauri app on a real display and were **never rendered or observed headlessly**.

## Settings — credentials panel

Gear button → Settings (per `docs/HUD.md` §5.1). The panel is a **multi-credential
registry** driven by a single source of truth mirrored Rust↔TS
(`src-tauri/src/credentials.rs` ↔ `src/core/credentials.ts`). Each credential is
`{ id, label, keychain_account, kind }`; the v1 set is:

| id | label | account | kind | verifies now? |
|---|---|---|---|---|
| `anthropic` | Anthropic API Key | `anthropic_api_key` | bearer | **yes** — `GET /v1/models` |
| `github` | GitHub Token (PAT) | `github_pat` | bearer | **yes** — `GET api.github.com/user` |
| `slack` | Slack Bot Token | `slack_bot_token` | bearer | **yes** — `POST auth.test` |
| `google_drive` | Google Drive | `google_drive_oauth` | oauth | deferred (Connect placeholder) |
| `google_calendar` | Google Calendar | `google_calendar_oauth` | oauth | deferred (Connect placeholder) |

The Anthropic account stays **exactly** `anthropic_api_key` — the daemon reads that
item at startup.

**Bearer rows** (CREDENTIALS section): masked password input (`autocomplete=off`),
a status pill, and a Remove (✕). **Pressing Enter** calls `verify_and_store(id, value)`
— the pill shows `VERIFYING…`, then `VALID` + detail (learn-green) / `INVALID`
(alert-red) / `NETWORK ERROR` (warn-amber). A secret is **stored only when it
verifies**; an unverified secret is never written. On valid+stored the input clears
and the pill shows `ON FILE`. On open, `keychain_status(account)` per id shows
`ON FILE` vs empty.

**OAuth rows** (INTEGRATIONS section): honest placeholders — a disabled
`CONNECT (OAUTH)` button and "arrives with the *label* integration". No fake verify.

**Storage and safety.** Keychain access is **in-process** via the Security.framework
bindings (`security-framework` crate) — service `com.darwin.daemon`, account from the
registry — so secret material never lands on any subprocess `argv`. The backend
validates every `account` argument against the registry **allowlist** and rejects
unknown accounts, so the frontend cannot write arbitrary Keychain items. Secrets are
never logged, never echoed back, and only ever leave the process inside the verify
request's auth header (`x-api-key` / `Authorization: Bearer`).

**Storing a token ≠ the integration working.** This build verifies + stores
credentials; the agents that *use* them (e.g. Steve opening GitHub PRs) are a separate
build. The daemon resolves the Anthropic key at startup — `ANTHROPIC_API_KEY` env
first, else the Keychain item — so **restart darwind after changing the Anthropic
key.** The status bar's CLOUD KEY indicator reflects `cloud_key_present` from the
daemon's `daemon.started` event.

## Layout

```
src/core/        pure state core: events.ts (typed envelopes + parsing),
                 state.ts (reducer), visuals.ts (state signatures), perf.ts
src/ws/          WebSocket client with backoff (client.ts)
src/tauri/       guarded invoke/window wrappers (bridge.ts)
src/three/       CoreScene.tsx — R3F core, particles, bloom, perf ladder
src/components/  glass panels: transcript, diagnostics, waveform, latency,
                 status bar, toasts, settings modal
src/test/        vitest suite for src/core
src-tauri/       Tauri 2 shell: keychain_status / keychain_set /
                 keychain_delete / test_api_key commands (security(1),
                 args-only, 5s timeout; reqwest for the key test)
```
