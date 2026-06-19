# JARVIS — Apple Silicon Bring-Up Checklist

Everything in the repo is logic-verified and hermetically tested. The credential- and
device-gated layers (live OAuth, mic/audio, MLX inference, outward actions) can only be
*proven* on the real machine. This is the safe order to bring it up and prove one gated
action end to end.

Project root assumed at `~/Downloads/jarvis`. Use absolute paths — a bare `$J`/relative
path in a fresh terminal is the #1 "file not found" gotcha.

---

## 0. Prerequisites (one-time)

- macOS on Apple Silicon (M1 or later; this build was brought up on an M4 Mac Mini).
- Homebrew **Python 3.11** at `/opt/homebrew/bin/python3.11` (MLX has no 3.14 wheels).
- Rust toolchain (`brew install rustup && rustup-init`).
- Node 20+ (`node`, `npm` on PATH) — only for the HUD.

---

## 1. One-time setup

```sh
cd ~/Downloads/jarvis

# Python env (MLX needs 3.11)
/opt/homebrew/bin/python3.11 -m venv .venv
.venv/bin/pip install -r inference/requirements.txt

# Memory DB (creates state/ and state/jarvis.db)
.venv/bin/python scripts/init_memory.py

# Download the on-device models (MLX LLM + Whisper STT + Kokoro TTS)
.venv/bin/python inference/deploy_models.py

# Build the daemon (release)
cargo build --release --manifest-path daemon/Cargo.toml
```

---

## 2. Boot order — **inference first, then daemon, then HUD**

The daemon talks to the inference server over `state/ipc/inference.sock`. If you start the
daemon first you'll see `transcription failed; is the inference server up?` — so always:

```sh
# Terminal 1 — inference server (stays in foreground; listens on state/ipc/inference.sock)
cd ~/Downloads/jarvis && .venv/bin/python inference/server.py

# Terminal 2 — daemon (telemetry on 127.0.0.1:7177)
cd ~/Downloads/jarvis && ./daemon/target/release/jarvisd

# Terminal 3 (optional) — HUD
cd ~/Downloads/jarvis/hud && npm install && npm run tauri dev
```

**Confirm a fresh binary:** the daemon logs a build marker at startup, e.g.
`INFO jarvisd: ... build="..."`. If you rebuilt, make sure the marker matches — a stale
binary is the second-most-common surprise.

---

## 3. Credentials — paste order (safest → most setup)

All secrets live in the macOS Keychain (service `com.jarvis.daemon`), entered once in the
**HUD → gear → Settings → Credentials** panel (masked, paste + Enter → verified → stored;
never logged, never on disk). The daemon reads keys **once at startup** — **restart the
daemon after adding/changing any credential.**

1. **Anthropic API key** (do this first — it's the brain). Paste it → it round-trips
   against the Anthropic API → the HUD status bar's **CLOUD KEY** light turns on. (Or
   `export ANTHROPIC_API_KEY=...` in `state/env.sh`, which the boot wrappers source.)
2. **GitHub (PAT)** and **Slack (bot token)** — these are *paste-and-go*: they verify over
   HTTP and store immediately, no developer-app dance. Best first integrations to prove.
3. **OAuth services** (Google Workspace, X, LinkedIn, Google Ads, Meta Ads, WHOOP) — paste
   the **client ID + secret** in Settings, then **say "connect &lt;service&gt;"** to JARVIS
   (e.g. *"connect Google"*, *"connect WHOOP"*). The daemon opens the browser, you approve,
   and the refresh token lands in the Keychain. Each needs its one-time developer-app setup
   first (see the per-service go-live notes from the integration rounds).

> You do **not** need every service. Reads light up the moment a service is connected;
> nothing fires outward until step 5.

---

## 4. Smoke test (read-only)

The master switch ships `allow_consequential = true` (armed), but a consequential action
still requires a fresh per-action confirmation, so read-only lookups are the safe first
smoke test. (To run the daemon disarmed during bring-up, set `allow_consequential = false`,
which reverts every consequential action to a dry-run preview.) Verify the basics:

- Ask **"list my agents"** → JARVIS names the 27-agent roster (deterministic, grounded).
- A pure read: **"what's on my calendar"** (Friday/Pepper, needs Google connected) or
  **"list my open PRs on &lt;owner/repo&gt;"** (Steve, needs GitHub).
- A consequential request **while the switch is still OFF** → JARVIS replies with a faithful
  **dry-run preview** ("I would post … to #channel") and fires nothing. Confirm the preview
  names the exact target/content — that's what you'll be confirming later.

---

## 5. Enable consequential actions + prove ONE gated action end to end

Do this only once reads work and the previews look right.

1. **Pick the safest first action.** Use something reversible and free — e.g. a **Slack
   message to a throwaway test channel** you own, or a **GitHub issue comment on a personal
   test repo**. **Do not** make ad-spend, email-send, or a public post your first live test.
2. **Flip the master switch.** Edit `config/jarvis.toml`:
   ```toml
   [integrations]
   allow_consequential = true
   ```
   **Restart the daemon** (config is read at startup).
3. **Run the end-to-end gated flow:**
   - You: *"Post 'JARVIS online' to #jarvis-test on Slack."*
   - JARVIS **parks the exact action**, speaks the faithful preview, and asks:
     *"…— say 'confirm' to proceed or 'cancel' to drop it."*
   - You: **"confirm"** → it replays that exact parked action and posts → verify it landed
     in Slack.
   - Try the negatives too: say **"cancel"** (drops it), or say something **unrelated**
     after a park (also drops it — a stray command can never confirm a stale action), or let
     it sit **>120 s** (expires).
4. **Decide.** Leave `allow_consequential = true` only if you want JARVIS able to act after a
   spoken confirm. The three independent factors are now all in your hands: service connected
   → master switch on → spoken confirm of the specific action.

---

## 6. Optional gates (leave OFF until comfortable)

- **Proactive speech** — `config/jarvis.toml [proactive] speak = true` lets Edith *voice*
  briefs (set false for a HUD card only). Ships ON. (`enabled = true` is the non-spoken
  first-contact brief and is fine to leave on.)
- **Self-heal** — `[self_heal] enabled = true`, `mode = "propose"`. Ships ON but
  PROPOSE-ONLY (inert without a cloud key); "auto" is dangerous and is its own deliberate
  decision (see `docs/SANDBOX.md` / the self-heal notes). Keep `mode = "propose"`.

---

## 7. Make it permanent (LaunchAgents)

Once it's proven by hand, install the boot agents so inference + daemon start on login and
restart on crash:

```sh
cd ~/Downloads/jarvis && scripts/install_boot.sh --install   # builds the binary + installs both plists
# logs: state/logs/launchd-inference.log, state/logs/launchd-daemon.log
# uninstall: scripts/uninstall_boot.sh
```

The wrappers `boot/run_inference.sh` / `boot/run_daemon.sh` resolve the project root from
their own location and source `state/env.sh` for secrets — so the plists never hard-code
paths or keys.

---

## Automated bring-up + doctor (one command each)

The manual order above is the source of truth, but two scripts automate the check legs.
Both resolve the JARVIS root exactly like the daemon (honor `JARVIS_ROOT`, else their own
parent dir) and are **honest**: a check that cannot run is reported `SKIP`/`UNKNOWN` with the
reason — never a faked pass.

```sh
# Read-only environment diagnostic — starts/stops/changes NOTHING. Reports the venv,
# binary, on-device models (in BOTH the install models/ cache AND ~/.cache/huggingface,
# flagging the HF_HOME split), the two sockets + telemetry port, and whether the
# LaunchAgents are loaded. Safe to run anytime.
scripts/doctor.sh

# One-command bring-up + read-only smoke: starts inference then the daemon (or detects an
# already-running pair and leaves it alone), waits for readiness with bounded timeouts,
# runs ONE token-gated `roster` round-trip on the command socket (inference-free — it does
# NOT spend a model call), prints a per-subsystem PASS/SKIP/FAIL board, then tears down only
# what IT started. Exits non-zero on a hard failure.
scripts/bringup.sh

# Smoke an ALREADY-running pair only (start nothing):
scripts/bringup.sh --no-start
```

Also: `jarvisd --selftest` (alias `--health`) validates the installed environment WITHOUT
starting the daemon (root/config/venv/binary/state dirs/0700 ipc perms/inference
reachability/telemetry-port bindability/cloud-key) and exits non-zero on a hard failure.

The daemon now also runs a **background inference-liveness probe** (publishing
`inference.health` + a one-shot `inference.degraded`/`inference.recovered` edge to the HUD)
and emits a single aggregated `daemon.ready` frame at startup — so a down inference server is
visible *before* a turn is lost, and the inference IPC client reconnects with bounded
exponential backoff + jitter instead of failing the first op after a server restart.

---

## Troubleshooting quick hits

- **`transcription failed; is the inference server up?`** → start the inference server first
  (step 2).
- **`No such file or directory` on a run script** → you're in the wrong dir / used a relative
  path; use the absolute `~/Downloads/jarvis/...` paths.
- **Behaviour didn't change after editing config or a credential** → the daemon reads both
  **once at startup**; restart it.
- **Code change didn't take** → stale binary; `cargo build --release --manifest-path
  daemon/Cargo.toml` and confirm the `build="..."` marker in the startup log.
- **An agent says "&lt;service&gt; isn't connected"** → that service has no credential in the
  Keychain yet (step 3), or (for OAuth) you haven't said "connect &lt;service&gt;".
- **A consequential action only previews, never fires** → by design until
  `allow_consequential = true` **and** you give a spoken **"confirm"**.

---

*Safety posture recap:* nothing outward-facing fires without (1) you connecting the
service's credentials, (2) `allow_consequential = true`, and (3) a spoken confirmation of
the specific action. All three are off/absent by default.
