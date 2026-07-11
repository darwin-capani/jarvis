# Plugin SDK — the capability-module contract (#36)

Status: **IMPLEMENTED (validator + register-on-launch handshake).** The pure
manifest validator (`daemon/src/plugin_sdk.rs::validate_manifest`) and the
register-on-launch handshake (`register_plugin`) are live; the LIVE handshake on
autostart is gated by `[plugin_sdk].enabled` (ships **ON**). A reference plugin
ships at `apps/example-plugin/`.

A *plugin* is just a **micro-app** (see [`SANDBOX.md`](SANDBOX.md)) — a separate
process under a default-deny seatbelt profile, holding a per-launch HMAC
capability token, reachable only over its own JSONL socket. The SDK adds the
*formalized contract* that lets a plugin **declare** what it answers and what it
exposes, and the daemon **validate + scope** those declarations. Declaring an
intent or a tool **grants nothing** — the SBPL profile and the capability token
are still derived from `[permissions]`, and a consequential tool still rides the
confirmation gate.

## The contract block

A plugin's `manifest.toml` is the existing `[app]` / `[permissions]` / `[ui]`
schema (unchanged) **plus** two optional blocks:

```toml
[intents]
# The intent names this plugin answers. Each is a well-formed dotted-lowercase
# identifier: segments of [a-z0-9_]+ joined by single dots; no leading/trailing
# dot, no empty segment.
provides = ["example.status"]

[[tools.exposes]]
# A tool the plugin exposes. Its name is well-formed (same rule). Each requested
# scope must be in plugin_sdk::ALLOWED_SCOPES AND backed by [permissions].
name          = "example.read_status"
scopes        = ["fs_read"]
consequential = false
```

Both blocks are `#[serde(default)]`, so **every existing manifest** (global-scan,
vision, nexus, silicon-canvas, mark-forge) that omits them still parses unchanged
and is treated as a plugin that declares no intents and exposes no tools.

## Capability scopes

`plugin_sdk::ALLOWED_SCOPES` is the **complete** set a tool may request — a scope
outside it is rejected. Each scope mirrors a seatbelt-grantable permission
dimension and must be **backed** by the `[permissions]` block (the over-privilege
cross-check):

| scope | backed by | meaning |
|---|---|---|
| `net` | `net_hosts` non-empty | outbound network to the declared hosts |
| `fs_read` | `fs_read` non-empty | filesystem read of the declared subpaths |
| `fs_write` | `fs_write` non-empty | filesystem write of the declared subpaths |
| `audio` | `audio = true` | the daemon's audio API |
| `gpu` | `gpu = true` | Metal / GPU |
| `camera` | `camera = true` | TCC-gated camera **declaration** (TCC is the real gate) |
| `screen` | `screen = true` | TCC-gated screen **declaration** (TCC is the real gate) |
| `generate` | — (no extra permission) | the daemon-mediated generate proxy (op=generate only) |

A tool requesting `net` while `net_hosts = []` is **over-privileged**: it claims a
network the SBPL profile would deny. `validate_manifest` rejects it with a precise
error (`tool "x": tool scope "net" is over-privileged: …`). This is the literal
guarantee *"a plugin can NOT request a capability outside the allowed set."*

## (a) The validator — `validate_manifest(raw, dir_name)`

A **pure** function (no I/O, no spawn). It:

1. parses the manifest via `AppManifest::parse` — which enforces the base
   contract: `[app].name` must equal the directory name (it keys the socket and
   the token), runtime known, non-empty version/entry, and `deny_unknown_fields`
   on every section (a typo'd key is a parse error, never a silently-dropped
   scope);
2. checks every `[intents].provides` name is a well-formed dotted-lowercase id;
3. checks every `[[tools.exposes]]` has a well-formed name and only scopes that
   are **(i)** in `ALLOWED_SCOPES` and **(ii)** backed by `[permissions]`.

It returns the validated manifest, or a **precise error string** (surfaced to the
operator / HUD verbatim — never an opaque code). It is always callable regardless
of `[plugin_sdk].enabled`.

## (b) The register-on-launch handshake — `register_plugin(...)`

At launch a plugin presents its manifest + the per-launch capability token it was
handed. The daemon:

1. **re-validates** the manifest with `validate_manifest` (defense in depth:
   checked at discovery, and again at the handshake);
2. **verifies the token constant-time** via `apps::verify_token_with_key` — the
   **same** HMAC/nonce machinery the per-app relay and the generate proxy use (no
   new crypto). The token binds `name ‖ canonical(permissions) ‖ nonce`, so a
   plugin that **widened its permissions** after the token was minted, a token
   **lifted from another app**, or a **stale** (pre-restart) token all fail
   closed;
3. on success, returns the **scoped intent set** the plugin may answer
   (`HandshakeOutcome::Admitted`). An invalid manifest →
   `InvalidManifest(reason)`; a bad token → `Unauthorized`.

The live wiring is `AppRegistry::register_on_launch`, called from the autostart
loop in `main.rs` **only when `[plugin_sdk].enabled` is true** (ships ON). It
emits a secret-free `plugin.handshake` telemetry event (`{name, status, detail}`
— never the token).

## (c) Capability-token scoping

The token *is* the authority; the manifest is its auditable description. Because
the token binds the exact permission set (`apps::canonical_permissions`), the
declared tool scopes can never *exceed* what the token is bound to — the validator
guarantees each scope is backed by a permission, and the permission set is exactly
what the token signs. A widened manifest mints a different token and fails the
handshake. See `SANDBOX.md` → *Capability tokens*.

## What a plugin still cannot do

- **Escape the SBPL default-deny profile.** The `AppManifest → generate_sbpl`
  derivation is unchanged: declaring an intent/tool opens no filesystem, network,
  mic, or GPU. The profile is exactly the `[permissions]` grant.
- **Request a capability outside the allowed set.** The validator rejects an
  unknown or over-privileged scope before the plugin is admitted.
- **Auto-execute a consequential action.** A tool marked `consequential = true`
  (or any consequential intent it answers) still **parks** behind the cross-turn
  spoken-confirmation gate + the armed-by-default `[integrations].allow_consequential`
  master switch (ON, but a confirmed action still needs a fresh per-action confirm)
  when invoked. Declaring it in the manifest only makes the contract
  auditable — it never bypasses the gate.

## The reference plugin

`apps/example-plugin/` is a minimal plugin: `manifest.toml` declares one intent
(`example.status`) and two read-only tools (`example.read_status` with `fs_read`,
`example.summarize` with `generate`), and `main.py` is a tiny JSONL handler that
runs under the seatbelt profile and carries its capability token on every line.
The test `plugin_sdk::tests::the_example_plugin_manifest_validates` proves the
shipped manifest validates against this contract.

## Config

```toml
[plugin_sdk]
enabled = true    # SHIPS ON (full-power default). The live register-on-launch handshake
                  # scopes a plugin's declared intents/tools; the validator rejects
                  # over-privileged manifests. The pure validator is always available.
```

## Honesty

The validator and the handshake are **proven hermetically** — the unit tests
validate a good manifest (accepts), a malformed one (precise error), an
over-privileged one (rejected), drive the handshake with synthetic tokens
(forged / cross-permission / stale → `Unauthorized`), and assert the shipped
example plugin validates. **No real spawn, no socket.** The daemon never
fabricates a loaded plugin; a plugin is admitted only after its manifest
validates **and** its token verifies.
