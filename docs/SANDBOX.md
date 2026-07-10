# Micro-App Sandboxing Blueprint

Status: **IMPLEMENTED.** The runtime substrate (`daemon/src/apps.rs` — manifest parsing, SBPL profile generation, capability tokens, per-app socket, supervised lifecycle, telemetry relay) is live, and the first app, **Global-Scan** (`apps/global-scan/`), runs on it. The four other launch apps (Nexus, Algo-Core, Fab-Link, Silicon Canvas) ship spec-only manifests against this schema under `apps/`; they have not been built yet.

Implementation notes (read these — they record the real boundary, not the ideal one):

- **`sandbox-exec` is deprecated-but-functional.** The host launches apps via `/usr/bin/sandbox-exec -f <profile>`. Apple has deprecated the *CLI* (it prints a notice) but the underlying seatbelt *kernel enforcement* is fully live and is what Apple's own daemon profiles use. The manifest→profile derivation in `generate_sbpl` is the stable part; Phase-4+ may migrate the launch mechanism to a `sandboxd` profile or App Sandbox entitlements without changing the derivation.
- **The generated profile is default-deny.** Every profile opens with `(deny default)`, imports Apple's stock `bsd.sb` (so the process can boot — dyld, frameworks, base syscalls — without opening the filesystem, network, mic, or GPU), then adds *only* the grants the manifest declares. See `daemon/src/apps.rs::generate_sbpl` and its unit tests (default-deny asserted, exact allows asserted, no stray grants asserted).
- **Honest SBPL limitations** (documented rather than hidden — see the Threat-model caveats below): coarse host-name network filtering, an inherent DNS side channel, and the fact that same-UID is the trust boundary for the per-app socket.

## Model

- **Process isolation.** Each micro-app is a separate process launched by `jarvisd`. Apps never run in the daemon's address space.
- **Seatbelt sandboxing.** At launch, `jarvisd` generates a macOS `sandbox-exec` (seatbelt) profile derived from the app's manifest permissions and starts the app under it. Anything not granted by the manifest is denied by the profile.
- **IPC.** Newline-delimited JSON (one object per line) over a per-app Unix socket at `state/ipc/apps/<name>.sock`. The daemon creates and owns the socket (bound `0600`, parent dir `0700`); the sandbox profile grants the app access to its own socket path only. The wire protocol (exact, mirrored in `daemon/src/apps.rs`, `apps/global-scan/main.py`, and the HUD reducer):
  - **app → host:** `{"token": <str>, "type": "items"|"status"|"log"|"modules", "data": <obj>}` — every line carries the capability token. (`modules` is the OPTIONAL, READ-ONLY dyld loaded-module self-report — `data.modules = [{path, uuid?}, …]` — attested against a trust-on-first-use baseline in `daemon/src/introspect.rs`; see docs/INTROSPECT.md. The reference stub is `apps/_sdk/dyld_report.py`.)
  - **host → app:** `{"type": "start"|"refresh"|"stop"}` — no token (the daemon is the trust root; the app trusts its own socket).
- **Capability tokens.** Every line an app sends carries a capability token minted by `jarvisd` at launch: `HMAC-SHA256(session_key, name ‖ canonical(permissions) ‖ nonce)`. The session key is 32 bytes of OS entropy generated once per daemon boot, held in a process-lifetime `OnceLock`, and is **never logged, never on telemetry, and never handed to an app** — only the derived per-app token reaches the app's environment (`JARVIS_APP_TOKEN`, alongside `JARVIS_APP_SOCKET`). The daemon verifies the token (constant-time) on **every inbound line**; a bad/missing/forged/stale/cross-app token drops the line and emits `("system","app.auth_failed",{name})`. The nonce rotates per launch, so a leaked token is dead after restart and cannot be replayed by another app or after a permission change.
- **Telemetry relay.** Apps do **not** connect to the `7177` telemetry WS. The host relays each accepted `items`/`status` line as `("system","app.data",{name,topic,payload})` (topic is one the app *declared* in `telemetry_topics`, else its first declared topic, else `"feed"` — an app can never publish to an undeclared topic), `log` lines as `("system","app.log",{name,line})`, and lifecycle as `("system","app.started"|"app.stopped"|"app.crashed",{name,...})`. The HUD panel renders purely from these relayed events.
- **Lifecycle.** The host writes the profile, binds the socket, spawns the sandboxed child, sends `{"type":"start"}`, and supervises it. On child exit it restarts, bounded to **≤3 restarts / 5 min**, then gives up with `app.crashed`. `stop()` kills the child (`kill_on_drop`) and removes the socket.
- **UI.** Micro-apps never open their own windows. UI surfaces render inside the HUD; the app declares which surface class it needs (`panel`|`overlay`|`fullscreen`) and the HUD composites it. (v1 renders `panel` apps as FUI-styled React panels driven by the `app.data` relay — e.g. `hud/src/components/GlobalScanPanel.tsx`; wgpu-texture/embedded-webview compositing is reserved for richer surfaces.)

## manifest.toml schema

Location: `apps/<name>/manifest.toml`. All paths are relative to the project root unless absolute.

```toml
[app]
name        = ""        # string, required — must match the directory name; used for socket and token
version     = ""        # string, required — semver
description = ""        # string, required
entry       = ""        # string, required — command jarvisd executes (relative to the app dir)
runtime     = ""        # "python" | "binary" | "node", required

[permissions]
audio     = false       # bool — microphone / audio-route access via the daemon's audio API
net_hosts = []          # list of host strings the seatbelt profile allows outbound connections to;
                        # empty list = no network
fs_read   = []          # list of paths the app may read (beyond its own app dir, which is implicit)
fs_write  = []          # list of paths the app may write; everything else is read-only or denied
gpu       = false       # bool — Metal/GPU access for the app process
camera    = false       # bool — DECLARES AVFoundation capture of the user's OWN camera (TCC: Camera)
screen    = false       # bool — DECLARES ScreenCaptureKit capture of the user's OWN screen (TCC: Screen Recording)

[ui]
surface          = ""   # "panel" | "overlay" | "fullscreen" — how the HUD composites the app
telemetry_topics = []   # list of topic strings the app may publish to the telemetry stream
```

Derivation rules from manifest to seatbelt profile:

| Manifest field | Seatbelt effect |
|---|---|
| `audio = false` | `(deny device-microphone)`; audio data only via daemon-mediated IPC if granted |
| `net_hosts = []` | `(deny network*)` |
| `net_hosts = [...]` | `(deny network*)` plus allow rules for the listed remote hosts only |
| `fs_read` / `fs_write` | deny-by-default filesystem; allow subpath reads/writes for the listed paths, plus implicit read of the app's own directory and read/write of `state/ipc/apps/<name>.sock` |
| `gpu = false` | deny IOKit GPU clients (no Metal device access) |
| `camera = true` / `screen = true` | **DECLARATION ONLY — TCC IS THE REAL GATE.** macOS Camera / Screen Recording consent is enforced by TCC, which requires a runtime USER-CONSENT prompt and is **not grantable by an SBPL/seatbelt profile** (there is no `(allow camera)` / `(allow screen)` operation). The profile at most grants the best-effort mach-lookup/device plumbing the capture frameworks need to *reach* the consent prompt; it never enables capture. `= false` keeps the deny explicit. So a `true` here lets the daemon surface the need in the launch UI/status and binds it into the per-app token — it grants nothing. No consent → no frames, profile notwithstanding. |

### Vision OCR screen read (`read.screen` — READ ON REQUEST, on-device)

The Vision micro-app (`apps/vision`; needs the screen capability — declared via the proposed `screen = true` manifest key, currently commented out and carried by the `JARVIS_VISION_SCREEN` env until the daemon `AppManifest` accepts the key) exposes a one-shot OCR **screen read** behind the FROZEN op `read.screen`. The daemon routes `"what's on my screen"` / `"read my screen"` / `"read this"` / `"where's the <X> button"` to it (`router::vision_command` → `read.screen`, with an optional `query` for a where-is locate), forwards the structured op verbatim, and the app runs Apple's built-in `VNRecognizeTextRequest` on **one** captured frame, structures the result, and relays a `vision.screen` telemetry event (recognized text in reading order, per-block boxes/centers, control-candidate labels, and — for a where-is — the best-matching located block). Honest properties:

- **On-device OCR, fully offline.** Built-in Apple Vision request; `net_hosts = []`. The recognized glyph text never leaves the device on the on-device brain path.
- **TCC is the real gate.** Live ScreenCaptureKit capture needs the Screen Recording consent prompt — not SBPL-grantable, requested on-device at first use. Headless test environments prove the OCR engine over a synthesized in-memory image; live capture is **device-gated** and never exercised in CI.
- **READ-ONLY = locate, not click.** A where-is query returns a control's box/center so the readout can *describe/locate* it. There is **no click/actuate op anywhere in the contract** — actuation is a separate, out-of-scope, gated surface.
- **DEFENSIVE: glyphs only.** OCR reads text glyphs; it is never turned into a face/person identifier. No identity path.
- **TRANSIENT by default (privacy).** The recognized screen text is sensitive (it can contain on-screen passwords/messages), so the daemon keeps it **off lifelong memory (fact extraction) and optimizer traces** — `router::is_screen_read` gates `main.rs`'s learning loop, and the text rides the `vision.screen` telemetry event (HUD readout only), never the persisted reply. The HUD surfaces it live only and never persists it either, labeled `READ ON REQUEST · TRANSIENT`.
- **Cloud-if-cloud-brain note.** If a turn is answered by the CLOUD brain, any on-screen text the user includes goes to the cloud exactly like any other user content — so the **on-device brain is the privacy-preferring path** for reading your screen.
- **Op-gated, never proactive.** The read fires only on an explicit request; there is no continuous/background screen-watching.

## Worked example

`apps/fab-link/manifest.toml` — a 3D-printing telemetry overlay that polls a Moonraker/OctoPrint endpoint and renders progress inside the HUD:

```toml
[app]
name        = "fab-link"
version     = "0.1.0"
description = "3D-printing telemetry overlay: polls Moonraker/OctoPrint and renders job progress, temperatures, and ETA in the HUD."
entry       = "python3 main.py"
runtime     = "python"

[permissions]
audio     = false
net_hosts = ["voron.local", "octoprint.local"]
fs_read   = ["apps/fab-link/gcode-previews"]
fs_write  = ["state/tmp/fab-link"]
gpu       = false

[ui]
surface          = "overlay"
telemetry_topics = ["fab.progress", "fab.temps", "fab.eta", "fab.alerts"]
```

At launch, `jarvisd`:

1. Parses the manifest and validates it (name matches directory, runtime known, paths inside allowed roots).
2. Mints the capability token: `HMAC-SHA256(secret, "fab-link" || canonical(permissions) || nonce)`.
3. Writes a seatbelt profile allowing: read of `apps/fab-link/`, read of `apps/fab-link/gcode-previews`, write of `state/tmp/fab-link`, socket access to `state/ipc/apps/fab-link.sock`, outbound network to `voron.local` and `octoprint.local` only. Everything else denied — no mic, no GPU, no other filesystem, no other network.
4. Executes `sandbox-exec -f <profile> python3 main.py` with the token passed via the launch environment.
5. The app connects to its socket and includes the token in every JSON request; the daemon verifies before acting. Telemetry the app publishes is accepted only on its declared `telemetry_topics` and re-broadcast on 127.0.0.1:7177 to the HUD.

## Threat model

What the sandbox prevents:

| Escape attempt | Why it fails |
|---|---|
| **Arbitrary filesystem access** — reading `~/.ssh`, `state/jarvis.db`, other apps' dirs; writing outside its grant | Seatbelt is deny-by-default; only manifest-listed `fs_read`/`fs_write` paths (plus the app's own dir and socket) are allowed. The daemon's secrets and the memory DB are never in any app's grant. |
| **Arbitrary network** — exfiltration, C2, scanning the LAN | `(deny network*)` unless `net_hosts` lists the host. An app with an empty list has no network at all; Fab-Link can reach its printer and nothing else. |
| **Mic access without grant** — eavesdropping via the microphone | Direct device access is denied by the profile unless `audio = true`. Even with `audio = true`, audio flows through the daemon's audio API over the app socket — the daemon can mute, indicate, and log it. |
| **IPC impersonation** — one app speaking as another, or replaying old credentials | Per-app sockets plus per-launch HMAC capability tokens bound to name + permission set + session nonce. Wrong app, wrong permission set, or stale nonce → verification fails and the daemon drops the connection. |
| **Privilege escalation via UI** — spawning windows, capturing the screen, key-logging | Apps have no window-server allowance; their only display path is a surface composited by the HUD (`wgpu` texture or embedded webview). Input reaches an app only when the HUD routes it to that surface. |

What it does not protect against (out of scope): kernel exploits in macOS itself, a compromised `jarvisd` (it is the trust root), and side channels between processes on shared hardware. Manifests are reviewed before an app is installed; the sandbox enforces the manifest, it does not judge it.

### Self-heal GUI apply — a human-gated mutation of the trust root

The one place a model-authored change can reach `jarvisd`'s own source tree is the self-heal apply path (full detail in `docs/ARCHITECTURE.md` → *Self-heal v2*). It is **not** auto-heal and it is **not** in the micro-app sandbox's scope — it is a deliberate, **human-gated** mutation of the daemon source:

- **`self_heal` ships `enabled = true` but PROPOSE-ONLY** (`mode = "propose"`, inert without a cloud key). With the gate off, an error burst only emits `heal.suppressed`; even on, nothing touches the live tree without the human-gated `scripts/apply_heal.sh`.
- **The GUI Accept button is human-gated and two-step.** The HUD's SELF-REPAIR // PROPOSALS modal fetches and shows the **actual staged diff** (`heal_proposal_detail`) for review. **ACCEPT & APPLY** arms a distinct **CONFIRM — APPLY & REBUILD** state; only a second click (after a re-arm window so a double-click cannot skip it) calls `heal_apply`.
- **Re-validation is mandatory and non-bypassable.** `heal_apply` spawns `scripts/apply_heal.sh <ts> --yes` (args-only, `ts` validated **digits-only** — no path traversal). `--yes` skips **only** the script's `read -r` keystroke; the script still stages a fresh copy of `daemon/` and re-runs `/usr/bin/patch -p1 --batch` + `cargo check` + full `cargo test`, and **refuses to touch `daemon/src` if anything fails** (the UI surfaces the failure in alert-red; the live code stays untouched). There is no flag that weakens this gate.
- This GUI apply, and the doubly-opt-in `mode = "auto"`, are the **only** sanctioned paths that change the live `daemon/` tree. The HUD reaches the daemon only over the one-way telemetry WS; the apply work is done by the HUD-Tauri backend spawning the repo script directly (filesystem + `cargo`), after which the daemon restarts to run the healed binary.

## Honest limitations of the current seatbelt implementation

These were surfaced by an isolation review of `daemon/src/apps.rs`. The first three are *closed* in the generator; the last two are *inherent* to permitting network/DNS via SBPL and to the single-UID model, and are documented here as the boundary of what the sandbox claims.

- **Metadata side channel — CLOSED.** Earlier the profile emitted a bare `(allow file-read-metadata)` (no path filter), which let an app `stat`/test-existence on the *entire* filesystem (probe that `~/.ssh/id_rsa` exists and its size/mtime) even though contents stayed denied. The generator now scopes `file-read-metadata` to the *same subpaths* it grants `file-read*` on (app dir, runtime install prefix, venv, `fs_read`, socket) — never a blanket grant. dyld's startup stats of `/` and the firmlink ancestors are covered by the `bsd.sb`/`system.sb` import, so no blanket grant is needed to boot.
- **Over-broad exec — CLOSED.** Earlier python/node apps were granted `process-exec*` on the *entire* `/opt/homebrew` and `/usr/local` trees (to reach the symlinked venv interpreter), letting an app exec any `bash`/`curl`/`git`/compiler planted under those user-writable prefixes. The generator now resolves the interpreter once (`std::fs::canonicalize`) and grants `process-exec*` only on the configured interpreter path *literal* plus its *resolved* path literal — never a prefix subpath. Read of the stdlib is scoped to the interpreter's own install prefix (the Cellar version dir holding `lib/pythonX.Y`), not all of Homebrew.
- **Socket ownership — CLOSED (defense-in-depth).** The per-app Unix socket at `state/ipc/apps/<name>.sock` is now `chmod 0600` after bind, and its parent dir `state/ipc/apps` is `0700`, so an unrelated same-UID process cannot casually `connect()` to read the host's start/refresh/stop command stream or wedge the accept path (a local DoS). Token verification already blocked *injection* (a connector cannot forge the per-launch HMAC), so this only closes the casual-connect leak. It does not stop a same-UID attacker who can `chmod` — same-UID is the trust boundary either way.
- **Coarse host-name network filtering — INHERENT.** `(allow network-outbound (remote tcp (host-name ...)))` matches the *connect-time name*, not the resolved IP. A feed host on a shared CDN can therefore share its allow with unrelated co-tenant names on that CDN. It is a meaningful narrowing, not an IP allow-list.
- **DNS exfiltration channel — INHERENT (bar raised, not closed).** Permitting DNS at all lets a malicious app encode data in query labels to an attacker-controlled authoritative nameserver, fully bypassing the `net_hosts` allow-list. The generator pins DNS to the *system resolver address(es)* from `/etc/resolv.conf` (rather than `*:53`) to raise the bar, but the resolver still forwards, so the channel is not closed.

Both inherent network caveats are the **headline justification for the Phase-4 daemon-mediated fetch proxy**: the app declares the URLs it needs, the daemon fetches them and returns the bodies over the app socket, and the app is granted **no direct network or DNS at all** — which collapses both channels.

### Confused-deputy via the inference socket — CLOSED (daemon-mediated generate proxy)

**Was:** Global-Scan's manifest granted `fs_read = ["state/ipc/inference.sock"]` so it could ask the local LLM for a neutral one-line summary (`op=generate`). The seatbelt grant was socket *reachability*, **not** op-level scope, and the inference server multiplexes *all* ops on that one socket (`transcribe`/`classify`/`generate`/`extract_facts`/`speak`/`converse`/`consolidate`) **without caller authorization**. A compromised app holding that grant could call `op=speak` (make JARVIS talk), drive `op=extract_facts`/`consolidate` (write into the user's memory DB by proxy), or spam the model to exhaustion — none of which the manifest implied.

**Now:** the daemon fronts micro-app generation with a **daemon-mediated `generate` proxy** (`daemon/src/genproxy.rs`), and micro-apps are granted **only** that proxy — never the raw `inference.sock`. The inference server is **unchanged**; the gate lives in the daemon:

- **Separate, op-restricted socket.** The proxy listens on `state/ipc/apps/generate.sock` (own JSONL socket, `chmod 0600`, parent dir `0700`), distinct from `inference.sock`. Global-Scan's manifest now reads `fs_read = ["state/ipc/apps/generate.sock"]`; it has **no grant to `inference.sock` at all**.
- **Only `op=generate`, structurally.** The proxy accepts `op == "generate"` and nothing else — every other value (`speak`/`extract_facts`/`consolidate`/`transcribe`/`classify`/`converse`, or any unknown string) returns `ok=false error=op_not_permitted` and emits `app.proxy_denied`. This is not a blocklist: the proxy has **no code path** that forwards anything but generate (it calls `InferenceClient::generate` directly, never a generic op dispatch), so the privileged ops are *unrouteable*, not merely *rejected*.
- **Token-gated.** Every line is verified with `AppRegistry::verify_token` — the *same* per-launch HMAC capability-token machinery the per-app relay uses (no duplicate token logic). A forged/tampered/cross-app/stale/missing token returns `ok=false error=unauthorized` and emits `app.auth_failed {via:genproxy}`. Fail-closed.
- **256-token cap.** `max_tokens` is clamped to a hard `PROXY_MAX_TOKENS = 256` regardless of the requested value (a missing/zero/negative value floors to a sane default), so no single proxied call can request an outsized generation.
- **Rate-limited.** At most `PROXY_RATE = 30` calls / 60 s rolling **per app name** — the LLM-exhaustion guard; beyond it the call returns `ok=false error=rate_limited`. An inference failure relays as `ok=false error=inference_unavailable`.

On any `ok=false` reply, or an unreachable proxy, the app falls back to extractive summaries exactly as before — the enhancement is best-effort, never required.

**Residual (honest register):** this closes the confused-deputy vector *for micro-apps* — the sandboxed, untrusted processes the threat model is about. It does **not** harden `inference.sock` itself: the server still trusts any local caller, so `jarvisd` (the trust root) and anything else able to reach that socket retain the full unauthenticated op surface. That is by design — the daemon *is* the trust root — and is the same single-UID boundary called out above. If a future non-micro-app component needs scoped LLM access, it should route through this proxy (or a sibling) rather than the raw socket.

## MCP external tool servers

JARVIS is an **opt-in MCP _host_**: it can connect to external [Model Context Protocol](https://modelcontextprotocol.io) tool servers and expose their tools to agents. This is the most dangerous surface in the system — an MCP server is **external code running on your machine as you**, not a sandboxed micro-app — so it is fenced by four independent layers and an honest residual-trust note. The per-server sandbox profiles generated by `daemon/src/mcp.rs` (`stdio_sandbox_profile`) cite this section.

**Ships ON, but INERT WITH ZERO SERVERS by default.** `[mcp].enabled = true` is the default, but the `servers` list ships EMPTY — so no server connects and no MCP tool exists for any agent until you add at least one `[[mcp.servers]]` entry. There is no auto-discovery and no bundled server.

**Layer 1 — per-server default-deny sandbox (stdio).** Each stdio server is wrapped by the **same `sandbox-exec`/SBPL machinery as micro-apps** (`apps.rs`). `stdio_sandbox_profile` derives a per-server `.sb` profile that is **deny-by-default** and grants only: exec of the configured `command`, the paths the server's config declares in `fs_read`/`fs_write`, and outbound network to the hosts the server declares in `net_hosts` (empty list ⇒ no network at all). The profile filename stem is the strict-validated server name. **Honest residual trust:** `sandbox-exec` is Apple-deprecated-but-functional; host-name network rules match the *connect-time name*, not the resolved IP (CDN co-tenant bleed); and permitting any DNS leaves the **DNS-label side channel** open — exactly the two *inherent* caveats in *Honest limitations* above. The profile **bounds** an untrusted server; it does **not** make a malicious server binary *safe*.

**Layer 1 (remote `http`) — TLS + token, NOT sandboxed.** A server configured with `transport = "http"` is a **remote** MCP server speaking MCP Streamable-HTTP/SSE (`daemon/src/mcp.rs::HttpTransport`, wired into `McpManager::connect_one`). It runs on **someone else's machine**, so — stated plainly — it **cannot be SBPL-sandboxed**: there is no local process to wrap in seatbelt, and we do **not** claim a remote server is sandboxed. Its protections are a *different, still-layered* set: **TLS-only** (the url **must** be `https://` — a non-https url is refused at construction so a bearer token never rides plaintext); **Keychain bearer auth** (the token resolves from `mcp_<server>_token` and rides the `Authorization` header **only** — never the URL, a log, or `Debug`); the **same** confirmation gate + per-agent allowlist + per-call bounds (timeout / output-size cap, plus a hard cap on SSE events and total bytes) as stdio; and a friendly, **secret-free** error map (a 4xx/5xx body is never echoed). **Honest residual trust:** the layers above bound the blast radius and keep the secret clean, but ultimately **you trust the remote operator** with the arguments you send and the results you receive — they do not neutralize a malicious operator. The single network leg (`HttpTransport::post`) is **runtime-gated**: it is reached only when `[mcp].enabled = true` **and** an `http` server with an `https://` url is configured; **no test ever touches the wire** — the SSE/JSON-RPC reply parsing is a pure function (`parse_sse_events` / `extract_rpc_response`) unit-tested with canned bytes, and the manager path is driven by a `MockTransport`.

**Layer 2 — confirmation gate + armed-by-default master switch.** A **CONSEQUENTIAL** MCP tool **parks** behind the cross-turn spoken-confirmation gate, identical to the built-in consequential tools, and only acts after the user confirms. Parking is additionally fenced by the master switch `[integrations].allow_consequential`, which ships **ON** — but even armed, a confirmed action still requires a fresh per-action confirm + voice-id + `!lockdown`; the switch alone never executes. **Fail-safe classification:** any unknown or mutating MCP tool defaults to CONSEQUENTIAL — a tool is treated as read-only (ungated) **only** when the server config explicitly marks it so.

**Layer 3 — per-agent allowlist.** A server is usable only by agents on its allowlist. The default is **the orchestrator plus an explicitly-listed agent** — the 27 personas are **never** auto-granted. An unlisted agent (or an unknown server name) is refused before any tool dispatch.

**Layer 4 — bounds.** Per-call timeout, output-size cap, max servers, and max tools/server are all enforced, so a slow or chatty server cannot wedge or flood the host.

**Secrets — Keychain only.** A server's auth token resolves from the macOS Keychain under the allowlisted account stem `mcp_<server>_token`, where `<server>` must pass `integrations::is_safe_mcp_server_name` (strict `[a-z0-9_-]+`, no leading/trailing or consecutive separator — the `__` ban keeps the flat tool id `mcp__<server>__<tool>` unambiguous). The token is **never** logged, never in `Debug`, never on argv, and never in a URL. A name that fails validation mints no account and the server is filtered out of `connectable_servers` — so a hostile name never spawns a subprocess or reaches `security(1)`.

**Out of scope (unchanged):** a malicious server binary's behavior *within* its granted fs/net bounds, kernel exploits, a compromised `jarvisd`, and cross-process side channels on shared hardware — the same boundary the rest of this document claims. The MCP layers **bound and gate** an untrusted server; they do not vouch for it.

## Plugin SDK — the formalized capability-module contract (#36)

The optional `[intents]` / `[tools]` block a plugin's `manifest.toml` may declare — *what intents it answers and what tools it exposes, with the capability scopes each requests* — is formalized and **validated** by `daemon/src/plugin_sdk.rs`. Full detail in [`PLUGIN_SDK.md`](PLUGIN_SDK.md). In short: `validate_manifest` (pure) rejects a malformed manifest (bad intent/tool name) and an **over-privileged** one (a tool requesting a scope outside `ALLOWED_SCOPES`, or a scope the `[permissions]` block does not back — e.g. `net` with `net_hosts = []`); the register-on-launch handshake (`register_plugin`, gated by `[plugin_sdk].enabled`, ships **OFF**) re-validates the manifest and **verifies the capability token** with the same HMAC/nonce machinery the per-app relay uses before scoping the plugin's intents. Declaring an intent grants nothing — the `generate_sbpl` derivation above is unchanged, and a consequential tool still rides the confirmation gate. Reference plugin: `apps/example-plugin/`.

## Webhook triggers — an inbound, authenticated, loopback-default surface (#35)

`daemon/src/webhooks.rs` adds the daemon's first **inbound** network surface, the most security-sensitive thing in this layer. It **ships ON** (`[webhooks].enabled = true`) but is **INERT WITHOUT MAPPINGS + a Keychain HMAC secret** — `mappings` ships empty (an unmapped event is rejected) and the secret resolves from the Keychain; the live receiver binds **127.0.0.1 loopback** by default (a non-loopback bind is refused) and is **runtime-gated** (the bind/accept-loop is wired behind the flag, not exercised in tests — the mic-loop / vision-capture precedent). Every request is authenticated by a **constant-time HMAC-SHA256 over the raw body** (`X-Jarvis-Signature: sha256=<hex>`, secret from the Keychain at `webhook_hmac_secret` — never in config/log/Debug); a missing/forged/stale signature **never routes**. An authenticated event is mapped to an intent only via the **explicit `[[webhooks.mappings]]` allowlist** (an unmapped event is rejected, not guessed), and a mapped **consequential** intent **PARKS** for the user's spoken confirm — a webhook can never satisfy the cross-turn confirm, so it can never auto-execute a side-effecting action. The pure decision (`handle_webhook`) is proven hermetically with synthetic signed requests; the body and the secret are never logged.
