<div align="center">

# ⟁ Project JARVIS

### An on-device-first autonomous AI desktop OS for Apple Silicon.

**A local MLX brain with cloud fallback · a 27-agent constellation · a holographic HUD · a sandboxed micro-app runtime.**
**Consequential power ships ARMED by default — still behind a per-action confirm, on-device voice-id, per-action policy, and lockdown, every one of which stays enforced.**

</div>

---

JARVIS turns an Apple Silicon Mac into a voice-driven, always-on AI environment that **owns the machine end to end** — it can act consequentially by default, but never **without a fresh per-action confirmation** (plus voice-id, per-action policy, and lockdown, all still enforced). A Rust daemon (`jarvisd`) runs the always-on audio/intent loop; MLX serves local inference on the Apple GPU; the Anthropic API handles what the local models can't; and a fullscreen HUD (Tauri 2 + React-Three-Fiber) renders the machine's live state as a dark-glass, cyan-holographic heads-up display. macOS stays underneath as an invisible host kernel.

It is built **honestly**: the headline features below are real and tested, the device-gated ones are labeled, and nothing here is a fabricated benchmark.

---

## Install in one command

```sh
curl -fsSL https://raw.githubusercontent.com/darwin-capani/jarvis/main/install.sh | bash
```

The installer presents a **futuristic, full-screen progress UI** and does the "every bit" install per-user, with **no sudo**:

- creates the install home at `~/Library/Application Support/JARVIS`,
- provisions the Python 3.11 venv and installs `inference/requirements.txt`,
- **builds every release artifact fresh** (`jarvisd` + the HUD + the Swift/Rust micro-apps — it never ships a prebuilt binary),
- downloads the MLX LLM + Whisper STT weights into the **install-home** HuggingFace cache (`HF_HOME` → `~/Library/Application Support/JARVIS/models`, never the shared per-machine HF cache),
- leaves the SQLite memory store to be created by `jarvisd` on first start (the installer never seeds it),
- and (optionally) installs the two LaunchAgents so JARVIS comes up on login.

Prefer to read before you run? Clone and use the local entrypoint — same steps, same UI:

```sh
git clone https://github.com/darwin-capani/jarvis.git
cd jarvis
./install.sh                # interactive install with the progress UI
./install.sh --dry-run      # print every action, change nothing
./install.sh --help         # all flags
```

**Requirements:** Apple Silicon Mac (M1 or later), macOS, Homebrew Python 3.11 (`/opt/homebrew/bin/python3.11` — MLX has no wheels for 3.14), a Rust toolchain (`rustup`), and Node 20+ for the HUD. The 4B default model wants **≥ 16 GB unified memory** (8 GB works but is tight). Cloud fallback and premium voice need an `ANTHROPIC_API_KEY` (and optionally an ElevenLabs key) — entered once in the HUD and stored in the macOS Keychain, never on disk.

### Enable image generation (optional — FLUX is a gated model)

Image generation uses [`black-forest-labs/FLUX.1-schnell`](https://huggingface.co/black-forest-labs/FLUX.1-schnell), a **gated** Hugging Face model — it cannot download anonymously, so the installer **skips it with a warning** and image generation stays **inert** until you authorize it. **Everything else installs and runs normally;** only image generation needs this.

To enable it:

1. Accept the licence (free, one click) at <https://huggingface.co/black-forest-labs/FLUX.1-schnell>.
2. Authenticate with Hugging Face — either log in with a token, or export one before re-running:
   ```bash
   "$HOME/Library/Application Support/JARVIS/.venv/bin/hf" auth login   # paste an HF token
   # …or:
   export HF_TOKEN=hf_xxxxxxxx
   ```
3. Re-run the installer — it resumes from cache and pulls FLUX with your token:
   ```bash
   curl -fsSL https://raw.githubusercontent.com/darwin-capani/jarvis/main/install.sh | bash -s -- -y
   ```

### Enable kernel-level security monitoring (optional — Endpoint Security)

The micro-app introspection subsystem (`docs/INTROSPECT.md`) ships with a **live Endpoint Security (ES) NOTIFY client** behind a **default-off Cargo feature**, `endpoint-security`. With it on, `jarvisd` observes real kernel events about its OWN sandboxed micro-apps — a page made executable (`mprotect`/`MAP_JIT`) by an app that declared `jit=false` (a W^X violation), or another process acquiring an app's task port (`get_task` — the "a debugger/injector is attaching" signal) — and reports them through the introspection HUD/`aegis_report`. **READ-ONLY (NOTIFY-only): it observes and reports, it never blocks or kills anything.** The stock build never links ES, so it adds **zero** attack surface by default; the rest of JARVIS is unaffected either way.

This is **not** installed by the one-command installer because ES is a **restricted Apple capability**: it needs a code-signing entitlement Apple must approve for your Developer Team ID, plus root and Full Disk Access. Without those, `jarvisd` logs an honest "endpoint-security unavailable" and keeps running on the light introspection path.

To enable it (Apple Developer account required):

1. Build the daemon with the feature:
   ```bash
   cargo build --release --features endpoint-security --manifest-path daemon/Cargo.toml
   ```
2. Request the **`com.apple.developer.endpoint-security.client`** entitlement from Apple (a one-time approval against your Team ID) and code-sign the binary with it + your Developer ID:
   ```bash
   # entitlements.plist must contain com.apple.developer.endpoint-security.client = true
   codesign --force --options runtime --timestamp \
     --entitlements entitlements.plist \
     --sign "Developer ID Application: <Your Name> (<TEAMID>)" \
     daemon/target/release/jarvisd
   ```
   Then grant the signed binary **Full Disk Access** (System Settings → Privacy & Security → Full Disk Access) and run it as **root**.
3. On success, `jarvisd` emits `introspect.es {active:true}` and the HUD's introspection surfaces begin showing kernel security findings. (See `docs/INTROSPECT.md` for the full flow and the honest device-gating caveats — the client is compile/link-verified in CI, but only *runs* with the entitlement present.)

### Uninstall — one command, two confirmations

```bash
~/Library/Application\ Support/JARVIS/uninstall.sh
```

Completely removes JARVIS from the machine, behind a **two-step typed confirmation** — it asks *"Delete JARVIS completely? (yes/no)"*, and only if you answer `yes` does it ask *"Are you ABSOLUTELY sure? (yes/no)"*. Either `no` (or any unrecognized input) cancels and deletes nothing. It removes **only** JARVIS's own footprint — the install home, the two LaunchAgents, the JARVIS Keychain items (`com.jarvis.daemon` only), and the logs — each a specific, guarded path (never a broad `rm`). Run it with `--dry-run` first to see exactly what it would remove without touching anything.

---

## What it can do

JARVIS **acts on the machine, not just talks about it.** The built-in actuator (`daemon/src/actions.rs`) is **benign-only by hard contract** — no shell passthrough, no deleting/moving/writing your files, no keystroke synthesis — and backs both the local intent router and the cloud tool loop, so any phrasing gets things done.

| Capability | Phrasing | Notes |
|---|---|---|
| **Open / quit apps** | "open Safari", "quit Chrome" | fuzzy-matched across `/Applications` + `/System/Applications`; ambiguous names come back as a spoken list, never a wrong guess |
| **Open websites & search** | "open the apple website", "search for mechanical keyboards" | only `http`/`https` ever reach `open`; `file:`/`javascript:`/`data:` are refused |
| **Find files** | "find my budget spreadsheet" | Spotlight under your home folder, filenames then contents, newest first |
| **Set volume / report status** | "volume to 40%", "system status" | live CPU/MEM/DISK/UPTIME from the telemetry cache |
| **Remember & recall facts** | "my name is Dar", "what are my projects?" | durable facts in SQLite, folded into every later reply; corrections overwrite |
| **Search your own docs, on-device** | "index my documents", "search my files for the lease clause" | **ON by default but inert until you allowlist a folder** (never a whole-disk scan); embeddings never leave the device; honest BM25 fallback that reports which method ran |
| **Full security check** | "am I secure?", "run a security check" | one READ-ONLY readout combining machine posture (FileVault/firewall/SIP/updates) + app privacy grants (TCC) + micro-app introspection; it reports where you stand and **changes nothing** — turning a protection on is yours to do |
| **Micro-app integrity check** | "are my apps healthy?", "any tampering?" | the read-only introspection sentinel over JARVIS's OWN sandboxed apps: seatbelt profile-drift, runaway CPU/RSS, and unexpected loaded modules (dyld injection) — it observes and reports, never kills or unloads (see [docs/INTROSPECT.md](docs/INTROSPECT.md)) |

Heavy or low-confidence requests route to the cloud and run the **same** actions through an Anthropic Messages-API tool loop (bounded: ≤ 6 model calls, 400 s), so *"could you possibly get that browser thing going"* works as well as *"open Safari."* Every executed action emits an `action.executed` telemetry event.

### The 27-agent constellation

A single local engine + per-agent profiles (not 27 separate models) gives JARVIS a routed "council" — the prime orchestrator (`jarvis`) hears every request and delegates to the right specialist: `friday` (daily intel), `vision` (research/OSINT), `ultron` (defensive security/automation), `steve` (CTO/builds), `gecko` (markets), `pepper` (personal EA/reflection), `hulk` (offline survival mode), and 19 more. Profiles live in `config/agents.toml`; isolation between them is the real security win, not the count.

### The voice & persona

JARVIS answers out loud in the register of the Iron Man films' JARVIS: a composed British butler-AI that addresses you as "sir," with dry understatement, kept to a few sentences. The persona (`inference/prompts/persona.txt`) is the single source of truth; there are no canned replies — every response is phrased live by the LLM from real handler data, and the persona greets and answers naturally from its first word (the old canned "Right away, sir." task-ack now ships **OFF** — set `[speech].instant_opener = true` to bring it back).

Two voice engines back this, consistent with the rest of the system — **armed by default, inert without their dependency:**

- **On-device Kokoro TTS** on the Metal GPU via `mlx-audio` (`bm_george`, British male) is the **private/offline default** and the **fallback on any cloud error or timeout**. It needs nothing — no key, no network — and is what you hear with no ElevenLabs key, offline, or in Local ("work offline" / Hulk) mode.
- An **ElevenLabs cloud-voice tier** is an *added* premium-TTS layer that ships **ON** (`[voice].cloud_tier = true`) but stays **inert until you add an `elevenlabs_api_key` to the Keychain** (and the active tier is non-Local). Jarvis-Prime now ships pre-mapped to the ElevenLabs premade **"George"** voice (a stable, shared British male voice on any account), so the cloud voice engages with **just a key — no manual voice-id step**; other agents stay on Kokoro until mapped in `[voice.voices]`. When this tier is active the text JARVIS is about to speak leaves the device for a cloud round trip to synthesize the audio.

### The HUD

`hud/` is the face of the machine — a fullscreen Tauri 2 + React-Three-Fiber app rendering the live telemetry feed: a glowing wireframe core that breathes when idle, pulses with your voice while listening, surges cyan for local thinking and violet for cloud; a transcript feed, system gauges, a 64-bar waveform equalizer, a pipeline-latency strip, and toasts for learned facts and executed actions. It's a pure client of `ws://127.0.0.1:7177` — it can crash and reconnect without ever touching the voice pipeline.

---

## Architecture at a glance

```
                            voice in ─┐
                                      ▼
  ┌───────────────────────────────────────────────────────────────┐
  │  jarvisd  (Rust, LaunchAgent, always-on)                       │
  │  audio capture → VAD → STT → intent router → actuator          │
  │  telemetry server (ws://127.0.0.1:7177) · micro-app supervisor │
  │  gates: master switch · confirm · voice-id · lockdown · policy │
  └───────┬───────────────────────────────┬──────────────┬────────┘
          │ Unix socket (IPC)             │ tool loop     │ telemetry
          ▼                                ▼              ▼
  ┌───────────────┐              ┌─────────────────┐  ┌──────────────┐
  │ inference/    │              │  Anthropic API  │  │  hud/  (HUD) │
  │ MLX server    │              │  (cloud fallbk) │  │  Tauri 2 +   │
  │ STT·classify· │              └─────────────────┘  │  React-Three │
  │ generate·TTS  │                                    │  -Fiber      │
  │ (Apple GPU)   │              ┌─────────────────┐  └──────────────┘
  └───────────────┘              │  apps/  (micro- │
                                 │  app runtime,   │
                                 │  sandboxed)     │
                                 └─────────────────┘
```

| Component | Path | Language | Role |
|---|---|---|---|
| `jarvisd` | `daemon/` | Rust | Audio capture, VAD, routing, actuation, telemetry server, micro-app supervisor |
| Inference server | `inference/` | Python 3.11 + MLX | STT, intent classification, local generation, fact extraction, TTS over a Unix socket |
| HUD | `hud/` | Tauri 2 + React + TS + R3F | Fullscreen telemetry visualization + settings (API key → Keychain) |
| Micro-apps | `apps/` | mixed (Rust, Swift) | Sandboxed apps per `docs/SANDBOX.md`; specs in `apps/<name>/SPEC.md` |
| Config | `config/` | TOML | `jarvis.toml` (runtime) + `agents.toml` (constellation) |
| Runtime state | `state/` | — | Sockets, logs, tmp, SQLite DBs. **Never committed.** |

### Hardware reality (read before contributing)

JARVIS was developed on an M4 Mac Mini, but the whole stack (arm64 + Metal/MLX + Core ML/ANE + macOS) is present on **every** Apple Silicon chip — local performance simply scales with the chip and unified memory.

- **macOS is the host, not Linux.** MLX (the Apple-GPU Metal backend) and Core ML/ANE access exist **only** on macOS. Asahi Linux is a non-starter on M4-generation silicon, and even where Asahi boots, MLX/Core ML need macOS — so macOS is the host regardless of chip.
- **MLX runs on the Apple GPU via Metal, not the Neural Engine.** LLM decode is memory-bandwidth-bound and the GPU sees full unified-memory bandwidth, so the model stays on the GPU. The ANE — reachable only through Core ML — is reserved for Phase-3 auxiliary models (wake-word, VAD, embeddings).
- **"Zero latency" is marketing, not physics.** Honest targets on M4-class silicon (M1/M2/M3 proportionally slower): local intent classification < 300 ms, STT < 1 s/utterance, first token < 500 ms. These are design targets, not measured claims on your device.
- **Kiosk takeover is Phase-2 BUILT but DEVICE-GATED.** The `enter_takeover`/`exit_takeover` wiring, state machine, exit-safety, and `TakeoverStage` layout are implemented and tested; the *actual* fullscreen render + Dock/menu-bar hide need a live Tauri app on a real display and were never observed headlessly. It ships OFF and is never auto-entered. See `docs/ROADMAP.md`.

---

## Safety model

**Consequential power is ON by default — what protects you is that every consequential action must clear the per-action layers below, and those are NOT defaults you can flip away: they are enforced at the chokepoints.** Read-only lookups always work; anything that posts/sends/spends or controls the machine must clear every layer:

1. **Master switch — ON (armed).** `[integrations].allow_consequential` ships `true`. Each consequential subsystem (self-heal, app forge, standing missions, MCP, trace optimizer, cloud voice/STT, doc search, screen capture, proactive speech, shell, UI automation) has its **own** switch, now ON by default — but several are **honestly inert until you supply a dependency**: an API key (cloud LLM, ElevenLabs voice/STT, self-heal/forge drafting), a downloaded model (vision VLM, image diffusion, speculative draft), a macOS TCC grant (UI automation, mic, screen context), an allowlisted folder (doc search, code), or a configured server/mapping (MCP, webhooks). Enabling a switch ≠ active without its dependency.
2. **Per-action confirm.** Each consequential action parks behind a fresh, cross-turn spoken confirmation. No batching past the gate — a macro or standing mission re-runs every consequential step through the gate individually. `ui_actuate` and `shell_run` are pinned **NEVER-auto-approve**: they re-park per action even under an "Always" policy.
3. **On-device voice-id (optional).** When enrolled + enabled, an unrecognized speaker can't trigger or confirm a consequential action. **Fail-closed**: an embed error is treated as unverified for the consequential path, never bricking ordinary replies. The profile is a local feature vector; no audio leaves the device.
4. **Lockdown.** One command hard-disables the consequential surface regardless of the other switches.
5. **Policy + allowlists.** A per-action policy plus per-feature allowlists bound what each path can touch (e.g. doc-search `roots` ships **empty** — never a whole-disk scan).
6. **Benign-only actuator + sandboxed apps.** The core actuator can't shell out, delete files, or synthesize keystrokes; micro-apps run under a default-deny sandbox with minimal declared permissions.

Secrets live in the macOS Keychain (resolved once at startup, never logged, never in a URL/argv/telemetry event). Where any learning corpus is stored — only when its own switch is on — content is PII-redacted with bounded retention. See [SECURITY.md](SECURITY.md).

---

## Layout

Per-user install home — **no sudo, relocatable** (the daemon is `JARVIS_ROOT`-relative):

```
~/Library/Application Support/JARVIS/
├── daemon/           # jarvisd (Rust) — built fresh at install
├── inference/        # MLX inference server (Python 3.11)
├── hud/              # fullscreen HUD (Tauri 2 + React + R3F)
├── apps/             # sandboxed micro-apps
├── boot/             # boot wrappers + LaunchAgent plist TEMPLATES (__JARVIS_ROOT__)
├── scripts/          # install_boot.sh, init_memory.py, ane_probe.py, …
├── config/           # jarvis.toml (runtime) + agents.toml (constellation)
├── docs/             # ARCHITECTURE · ROADMAP · HUD · SANDBOX · …
├── .venv/            # Python 3.11 environment (not committed)
└── state/            # runtime only, NEVER committed:
    ├── env.sh        #   secrets (chmod 600), sourced by boot wrappers
    ├── ipc/          #   inference.sock, command.sock, command.token, apps/<name>.sock
    ├── logs/  tmp/  images/  voice-samples/  openers/
    ├── ane/          #   Core ML probe model cache (.mlpackage)
    └── *.db          #   SQLite memory / audit / optimize stores
```

The git repo tracks **source only**. The entire `state/` tree, all secrets, every build artifact, model weight, `.venv`, log, `.wav`, SQLite DB, and rendered plist are `.gitignore`d — see [.gitignore](.gitignore). This repo is safe to push public.

---

## What needs you

Some things only **you** can grant — a config flag cannot substitute:

- **macOS consent (TCC).** The features ship enabled, but several stay **inert until the OS grants you the permission**: the always-on **Microphone** loop (wake word, live interpret, sound monitor), **Screen Recording** (screen context), and **Accessibility/Automation** (UI automation). The flag cannot grant these — they are yours to approve or deny in System Settings.
- **API keys in Keychain.** Paste your `ANTHROPIC_API_KEY` (and optional `elevenlabs_api_key`) once in the HUD's Settings — masked, stored in the Keychain (service `com.jarvis.daemon`), never plaintext on disk. The cloud LLM fallback, the ElevenLabs voice/STT tier, and self-heal/forge drafting are enabled but stay inert until the key is present. Restart JARVIS after changing a key.
- **Auto-login (optional).** For true boot-to-JARVIS, enable "Automatically log in as" in System Settings → Users & Groups (requires FileVault off). The LaunchAgents do the rest.
- **Confirming consequential actions.** The features are enabled by default; nothing side-effecting runs until you **confirm each action** (and, where applicable, supply the key / grant the permission / allowlist the folder).

---

## Further reading

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — component diagram, IPC contract, telemetry, learning loop, memory subsystem
- [docs/OS_CAPABILITIES.md](docs/OS_CAPABILITIES.md) — the full capability surface
- [docs/ROADMAP.md](docs/ROADMAP.md) — the phase plan
- [docs/HUD.md](docs/HUD.md) · [hud/README.md](hud/README.md) — HUD spec and the shipped HUD
- [docs/SANDBOX.md](docs/SANDBOX.md) · [docs/PLUGIN_SDK.md](docs/PLUGIN_SDK.md) — micro-app sandboxing + plugin SDK
- [docs/BRINGUP.md](docs/BRINGUP.md) — manual dev bring-up (venv, models, release build, run)
- [SECURITY.md](SECURITY.md) — security posture + private reporting · [LICENSE](LICENSE) — MIT

<div align="center">

*Built on-device. Armed by default, gated per action. Honest about its limits.*

</div>
