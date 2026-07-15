# Security Policy

DARWIN is an autonomous, on-device-first AI desktop environment that can act on
your machine and reach the network. Security is a first-class concern, and the
project ships **full-power by default — consequential power is ARMED**, with every
consequential action held behind multiple independent per-action gates that stay
enforced at the runtime chokepoints (a per-action confirmation, on-device voice-id,
per-action policy, and lockdown). Arming the master switch does **not** bypass any
of them: the switch alone never executes anything.

## Reporting a vulnerability

**Please report security issues privately. Do not open a public issue, PR, or
discussion for anything exploitable.**

- Email the maintainer at **darcapalb@gmail.com** with the subject line
  `DARWIN SECURITY`.
- Include: a description, affected component/path, reproduction steps or a PoC,
  and the impact you observed.
- You will get an acknowledgement; please allow time for a fix before any public
  disclosure (coordinated disclosure preferred).

If you found a leaked secret in the git history (an API key, token, or state DB
that should have been ignored), report it the same way and **rotate the
credential immediately** — do not wait for a response.

## Security posture (what protects you)

DARWIN ships full-power, but the consequential surface is layered behind multiple
independent gates that stay enforced at the runtime chokepoints — each of which must
pass, and none of which is a default you can flip away:

1. **Master switch — ARMED by default, still per-action gated.** Side-effecting /
   outward action is gated by `[integrations].allow_consequential`, which ships
   `true`. Arming it does **not** bypass anything: a confirmed action still requires
   a fresh per-action confirmation + voice-id (if enrolled) + per-action policy +
   `!lockdown`. With the master switch armed, a consequential action without a fresh
   confirm is still a dry-run preview — the switch alone never executes. With it off
   (lockdown, or an operator who disarms it) everything reverts to dry-run preview.
   Every consequential subsystem (self-heal, app forge, standing missions, MCP, trace
   optimizer, voice cloud tiers, doc search, screen capture, proactive speech, shell,
   UI automation) has its **own** independent master switch; several are **honestly
   inert** until you supply a dependency (an API key, a downloaded model, a macOS TCC
   grant, an allowlisted folder, or a configured server/mapping). Self-heal, app
   forge, and the optimizer ship ON but are **PROPOSE-ONLY** — they write a validated
   proposal a human applies; there is no auto-apply path. The one deliberate OFF
   default is **`[voice_id]`** (a fail-closed gate, enrolled explicitly), plus at-rest
   encryption (`[security].encrypt_memory`, an irreversible on-disk migration).
2. **Per-action confirmation gate.** Even with the master switch armed, each
   consequential action parks behind a fresh, cross-turn spoken confirmation.
   There is no batching past the gate: a macro or standing mission re-runs every
   consequential step through the gate individually, exactly as if spoken live.
   `ui_actuate` (UI automation) and `shell_run` (sandboxed shell) are pinned
   **never-auto-approve**: they re-park per action even under an "Always" policy, so
   one confirmation authorizes exactly one actuation/command.
3. **On-device voice identity (optional).** When enrolled and enabled
   (`[voice_id]`, ships OFF), an unrecognized speaker cannot trigger or confirm a
   consequential action. It is **fail-closed**: an embedding error or unusable
   audio is treated as unverified for the consequential path, while ordinary
   replies are never bricked. The voice profile is a local feature vector only;
   no audio leaves the device.
4. **Lockdown.** A single command (`lockdown`) hard-disables the consequential
   surface regardless of the other switches.
5. **Per-action policy + allowlists.** A policy layer plus per-feature allowlists
   (e.g. MCP server `agents` allowlists, the doc-search `roots` allowlist which
   ships **empty** so it is never a whole-disk scan) bound what each path may
   touch.
6. **Benign-only core actuator.** The built-in actuator
   (`daemon/src/actions.rs`) is benign-only by hard contract: no shell
   passthrough, no deleting/moving/writing user files, no keystroke synthesis.
   Only `http`/`https` URLs reach `open`; `file:`/`javascript:`/`data:` schemes
   are refused.
7. **Sandboxed micro-apps.** Apps run under a default-deny sandbox profile with
   minimal declared permissions (see `docs/SANDBOX.md`).
8. **Secrets never on disk in plaintext.** API keys (Anthropic, ElevenLabs) live
   in the macOS Keychain, resolved once at startup, never logged, never placed in
   a URL/argv/telemetry event.
9. **PII redaction.** Where any learning corpus or trace is stored (only when its
   own switch is on), content is PII-redacted before storage (emails, phone
   numbers, long digit runs, credentialed URLs, key/token-shaped strings) and
   retention is bounded.

## What needs your consent (and cannot be granted by a flag)

Some capabilities require macOS to grant runtime consent (TCC) — a config flag
**cannot** substitute for it:

- **Microphone** — the always-on audio loop.
- **Screen Recording** — the screen-context ring (ships ON, but inert without this
  consent — the flag cannot grant it).
- **Accessibility / Automation** — any UI automation path.
- **Full Disk / Files & Folders** — beyond the home-folder + `/Applications`
  defaults.

These prompts come from the OS and are yours to approve or deny.

## Reporting scope

In scope: the daemon, inference server, HUD, micro-app runtime, installer, and
the gating/safety model described above. Out of scope: vulnerabilities that
require an attacker to have already disabled the safety gates with the owner's
explicit consent, or issues in third-party dependencies (report those upstream,
though a heads-up here is welcome).

Thank you for helping keep DARWIN users safe.
