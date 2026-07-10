# Micro-App Introspection & OS-Internals Blueprint

Status: **IMPLEMENTED.** The observability core is live end-to-end (daemon → telemetry → HUD):

- **`jit` manifest capability** (`daemon/src/apps.rs`) — a declared, token-bound, SBPL-derived JIT/dynamic-code-generation permission (default-deny), mirroring the `gpu`/`camera`/`screen` pattern.
- **`daemon/src/introspect.rs`** — a READ-ONLY sentinel over jarvisd's own sandboxed children: SBPL **profile-drift** detection (fingerprint-vs-on-disk), per-app **RSS/CPU anomaly** classification via `sysinfo`, and cooperative **dyld module attestation** (trust-on-first-use baseline). Relays through the existing telemetry bus; wired in `main.rs` behind `[introspect].enabled` (ships ON).
- **Module attestation protocol** — the app→host wire gains a `modules` message type (`apps.rs`); the reference in-proc stub is `apps/_sdk/dyld_report.py` (public `_dyld_*` API, no entitlement), wired into `apps/example-plugin` and `apps/global-scan`. The Swift micro-app **vision** reports too, via a deliberate *additive* extension to its `VisionEvent` contract (a `modules` relay type + `Sources/vision/DyldReport.swift`) — compile- and test-verified (`swift build` + 205 tests).
- **`posture.rs`** folds a secret-free introspection tally into its read-only report.
- **User-queryable** — the read-only cloud tool `aegis_introspect` (Defense & Privacy agent) answers "are my apps healthy / any tampering?" with `introspect::status_summary()` (counts + recent findings); the daemon retains a bounded, secret-free findings ring for it.
- **Capability inventory** — a static "what can each app *do*" audit (`apps::capability_summary`, emitted as `introspect.capabilities` each tick and rendered per-app in the HUD): the granted capabilities from each manifest (incl. `jit`), counts only, never the paths/hosts. Complements the runtime "what is it doing" with the declared "what is it allowed to do".
- **HUD** — `IntrospectPanel` renders `introspect.snapshot` + accumulated `introspect.profile_drift`/`anomaly`/`module_violation` findings (`hud/src/components/IntrospectPanel.tsx`, parsers/reducer in `core/events.ts` + `core/state.ts`).

- **Endpoint Security — full stack, feature-gated** — the pure, CI-tested classifier (`SecurityEvent` mprotect-exec / MAP_JIT / GET_TASK / signal → `classify_security_event` [a `jit=false` app making memory executable = W^X violation; a `GET_TASK` = attach/inject signal] → `ingest_security_event` → `introspect.security_event` telemetry + HUD finding) is now driven by a **live NOTIFY client** behind the default-off `endpoint-security` Cargo feature. The fragile `es_message_t` parsing lives in a C shim (`daemon/csrc/es_shim.c`) compiled against Apple's real header (struct layouts compiler-verified); Rust sees a flat scalar ABI (`daemon/src/es.rs`) and maps events to OUR tracked apps via `introspect::app_for_pid`. **COMPILE+LINK verified** here (`cargo build --features endpoint-security` builds the shim + links `-lEndpointSecurity`/`-lbsm` — no entitlement needed to *link*); **runtime is device-gated** — `es_new_client` needs root + the restricted `com.apple.developer.endpoint-security.client` entitlement + a notarized host, so `es::start()` returns an honest error off-device and the light path is unaffected. NOTIFY-only (never AUTH — it never blocks/wedges the subject).

Deferred (with the honest cost recorded below): the tamper-resistant out-of-process `task_for_pid`/Mach-port corroboration of the cooperative module report (its `cs.debugger` entitlement wouldn't even yield task ports for jarvisd's own hardened processes).

This document also records the **state-of-the-art macOS/Apple-Silicon (arm64e) OS-internals facts** the design rests on. They were adversarially verified against a live macOS 26.5.1 machine; where the naïve/textbook version is wrong, the correction is stated. Do not "simplify" these back to the Windows/Linux model — this is an Apple-Silicon system.

---

## 1. Why the light path (and NOT Endpoint Security / task_for_pid / ptrace)

The decisive fact: **jarvisd already OWNS the processes it wants to observe.** `apps.rs::run_once` spawns each micro-app as a **same-UID child** under `sandbox-exec` and holds its `tokio::process::Child`. That collapses the cost of introspection:

| Mechanism | Gives | Cost on current macOS | Decision |
|---|---|---|---|
| `sysinfo` (libproc under the hood) | per-child RSS/CPU | **none** for same-UID/owned children | **shipped** |
| SHA-256 of the profile we wrote | tamper detection | none (`sha2` already a dep) | **shipped** |
| Endpoint Security (`ES_EVENT_TYPE_NOTIFY_{EXEC,MMAP,MPROTECT,SIGNAL,GET_TASK}`) | authoritative kernel security events; `GET_TASK` = "someone is attaching/injecting" | **root + Apple-approved `com.apple.developer.endpoint-security.client` + notarization + Full Disk Access**; host may be a launch daemon *or* a system extension (a sysext is NOT strictly required — Apple's own `eslogger` is a daemon); **NOTIFY-only** (AUTH blocks the subject thread until a variable `deadline`, and a slow client is killed) | **deferred** |
| `task_for_pid` + `mach_vm_read` | remote `dyld_all_image_infos`, memory | `com.apple.security.cs.debugger`, which only yields ports for `get-task-allow` (debuggable/non-hardened) targets — it would **not** work on jarvisd's own hardened/notarized processes; blocked by SIP on Apple-signed targets | **deferred** (prefer in-proc self-report) |
| DTrace | `fbt`/`pid`/`syscall` providers | `pid`/`syscall` work on your own processes with SIP on, but `fbt`/kernel need SIP weakened from Recovery (`csrutil enable --without dtrace`) | **never** (not shippable on-device) |
| `ptrace` / `PT_DENY_ATTACH` | — | anti-debug facility; stops the target; exclusive; no memory peek (that's Mach) | **never** (wrong model for cooperating children) |

**Rule:** introspection is observability-only. It reads and reports. Any *reaction* (killing a runaway app beyond the existing restart governor, tightening a profile) is CONSEQUENTIAL and must ride the existing `confirm.rs` + `voiceid.rs` + `policy.rs` + `lockdown.rs` gates — `introspect.rs` itself only observes, exactly as `posture.rs` and `heal.rs` (PROPOSE-ONLY) do.

---

## 2. Focus area 1 — the syscall boundary (context for a profiler)

The verified arm64e reality (drives *what is even observable*):

- A libc call loads the syscall number into **`x16`** (args in `x0–x8` per AAPCS64) and executes **`svc #0x80`**. The `0x80` immediate is a **Darwin software convention the CPU does not dispatch on** — the hardware takes the SVC exception for any immediate, vectors EL0→EL1 via `VBAR_EL1`, sets `ESR_EL1.EC = 0x15` (SVC_64); XNU (`osfmk/arm64/sleh.c`, `handle_svc`) then selects the service from `x16`. The immediate is recoverable from `ESR_EL1.ISS[15:0]` for tracing.
- Dispatch is **three-way**: `x16 == 0x80000000` (`PLATFORM_SYSCALL_TRAP_NO`) → `platform_syscall` (arm64 machine-dependent path; there is **no** `machdep_call_table[]` like x86); `x16 < 0` → negated into `mach_trap_table[]` (except `MACH_ARM_TRAP_ABSTIME` `-3` / `CONTTIME` `-4`, serviced inline); `x16 ≥ 0` → `sysent[]` (`init_sysent.c`).
- **No stable public syscall-number ABI** (like Windows/ntdll, unlike Linux) — hook *semantic* events, never hardcoded `svc` numbers.
- Much "kernel" work is `mach_msg` RPC or **commpage** reads (`mach_absolute_time` reads `CNTVCT_EL0` + a commpage timebase offset with no `svc` at all) — invisible to a syscall tracer. This is why the shipped design samples *resource counters* (via libproc/sysinfo) rather than tracing syscalls.

Unprivileged tracing primitive JARVIS may emit into during development: **`os_signpost`** (`POINTS_OF_INTEREST`), which Instruments surfaces. (`fs_usage`/Instruments Time Profiler consume kdebug via ktrace; `sample(1)` is a Mach stack-walker, not a kdebug consumer.)

---

## 3. Focus area 2 — W^X / JIT and the `jit` manifest key

### Verified Apple-Silicon facts

- **W^X is hardware-enforced for all processes** (not just hardened ones). The sanctioned JIT allocation is `mmap(MAP_JIT)` with `prot = PROT_READ|PROT_WRITE|PROT_EXEC` — the RWX request is accepted *only because* `MAP_JIT` is set; the region is materialized **R-X**. A plain RWX `mmap` *without* `MAP_JIT` is refused.
- `pthread_jit_write_protect_np(int)` toggles a MAP_JIT region between **`rw-` and `r-x`** (there is **no execute-only** mode). Backed by a hardware permission-remap register (**APRR** on A11-class, **SPRR / `S3_6_c15_c1_5`** on M1+), flipped via `msr`+`isb` — no syscall, no `vm_map_protect`, no TLB shootdown. The register is per-core CPU state but **XNU saves/restores it per thread**, so the writable view is thread-local.
- arm64 I-cache is **non-coherent** — after emitting you MUST `sys_icache_invalidate()` / `__builtin___clear_cache()` before executing (a no-op on x86; this is the classic Intel→Apple-Silicon JIT bug).
- Entitlement safety order: `com.apple.security.cs.allow-jit` (narrow) ≫ `allow-unsigned-executable-memory` ≫ `disable-executable-page-protection`.
- **MLX / the inference stack does NOT need `allow-jit`** *(this refuted the first-pass assumption)*. Metal kernel compilation is out-of-process (`MTLCompilerService.xpc`) and produces GPU code, not CPU pages; MLX ships a precompiled `mlx.metallib`, `MLX_METAL_JIT` defaults OFF, and `libmlx.dylib` has no `MAP_JIT`/`pthread_jit_write_protect_np` symbols. The inference process also runs under Homebrew `python3.11` via a plain launchd plist with no hardened runtime. **Do not add `allow-jit` to the inference host.** ObjC/Swift closures likewise need no `allow-jit` (they are AOT code + heap-captured data).

### The `jit` manifest key (shipped)

- `PermissionsSection.jit: bool`, `#[serde(default)]` (=> `false`) — every existing manifest parses unchanged and stays JIT-denied.
- `generate_sbpl`: `jit == false` emits `(deny dynamic-code-generation)` (explicit + reorder-safe, like `gpu`); `jit == true` emits `(allow dynamic-code-generation)` with an honesty comment. **Only `dynamic-code-generation` is emitted** — the legacy `dynamic-signature` is *not* a current seatbelt operation and is never written (a unit test asserts this).
- `canonical_permissions` folds `jit` into the HMAC message, so a manifest that flips `jit=true` after a token was minted fails verification (test: `token_is_bound_to_jit_flag`).
- **Honest limitation** (documented in the SBPL comment): on the current unsigned-interpreter launch the RWX/MAP_JIT deny is *already* enforced by the platform (no `allow-jit` entitlement + arm64e W^X). The SBPL line is defense-in-depth + auditable, token-bound intent — **not** the sole gate. `jit=true` is a CONSEQUENTIAL capability declaration (an authored manifest edit, never a runtime auto-grant).

---

## 4. Focus area 3 — dyld module tracking (SHIPPED, cooperative)

Verified facts for when this is built:

- **dyld4** (`Loader` base, dispatched by a *kind bit* not vtables; `JustInTimeLoader` / `PrebuiltLoader`). System dylibs live in the **shared cache in the cryptex since Ventura** (`/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_arm64e`) — **not individually openable files**, so you cannot file-hash `/usr/lib/libSystem.B.dylib`; attest by `sharedCacheUUID` or in-memory read.
- **Chained fixups** (`LC_DYLD_CHAINED_FIXUPS` + `LC_DYLD_EXPORTS_TRIE`) are the **default for macOS 11+ deployment targets** — chosen by deployment target/linker flags (`-no_fixup_chains`), *not by architecture*. A parser must detect `LC_DYLD_INFO_ONLY` vs chained. **Lazy binding is gone** (`__la_symbol_ptr`/`dyld_stub_binder`-on-first-call obsolete; `dyld_stub_binder` survives only as a vestigial import). arm64e routes `__auth_stubs → __auth_got` with PAC (`braa`).
- Injection is already blocked on hardened + library-validated processes (dyld strips `DYLD_*`; AMFI refuses foreign/adhoc `dlopen`). The `disable-library-validation` / `allow-dyld-environment-variables` entitlements being *present* is itself a signal.
- **Enumeration:** out-of-process via `task_info(TASK_DYLD_INFO)` → `dyld_all_image_infos` (`infoArray` can be transiently NULL — read with the change-timestamp; prefer the `_dyld_process_info_*` API; `imageFilePath` is spoofable and misses anonymous loads → also check UUID). Analogues: Linux `_r_debug`→`link_map`, Windows `PEB_LDR_DATA`→`LDR_DATA_TABLE_ENTRY`.

**Shipped design (cooperative, entitlement-free):** an **in-process SDK stub inside the sandboxed app** enumerates the loaded-module set (image path + `LC_UUID`) via the public `_dyld_image_count`/`_dyld_get_image_name`/`_dyld_get_image_header` API and streams it over the **existing HMAC-tokened per-app socket** as a `modules` line — the daemon (`introspect::attest_or_seed`) seeds a trust-on-first-use baseline on the first report and flags any later-appearing module (injection / unexpected dlopen) as `introspect.module_violation`, without ever calling `task_for_pid` or needing the debugger entitlement. Reference stub: `apps/_sdk/dyld_report.py` (verified on-device to enumerate ~353 images with parsed UUIDs); wired into `apps/example-plugin` (which grants `fs_read` of `apps/_sdk`).

**Live dlopen re-reporting (shipped).** Beyond the one-shot startup report, `dyld_report.watch()` registers a `_dyld_register_func_for_add_image` callback that flips a thread-safe flag on any image loaded *after* the initial set; the app re-sends a fresh report from its own thread when the flag is set (`modules_changed_and_clear()`), so a runtime `dlopen` is caught too — not just the launch-time load set. The callback only sets an event (no cross-thread socket write). global-scan uses this in its poll loop; **vision** uses the Swift equivalent (`DyldReport.watch()`/`consumeChanged()`, `OSAllocatedUnfairLock`-guarded) and re-attests after each host op — which matters most for vision, since Vision/ScreenCaptureKit/CoreML load *lazily when capture starts*, after the one-shot report. Verified: Python on-device (a runtime `dlopen` of Vision.framework flipped the flag: 353→470 images) and Swift in-test (`testWatchFiresOnARuntimeDlopen`). The baseline is re-seeded per launch (`introspect::reset_module_baseline`), so a legitimately-updated app is not false-flagged.

**Honest scope — cooperative, not tamper-proof.** Because the socket is token-authenticated, a *different* process cannot forge a report, so this reliably catches injection into an **otherwise-honest** app and gives an auditable inventory. It is **not** a defense against a **fully-compromised** app that lies about its own modules — that deeper compromise is bounded by the sandbox + token model. The tamper-resistant out-of-process corroboration (`task_for_pid` → `dyld_all_image_infos`) stays deferred: `com.apple.security.cs.debugger` would not even yield task ports for jarvisd's own hardened processes.

---

## 5. The shipped introspect sentinel (`introspect.rs`)

- **Pure cores (CI-tested):** `sbpl_fingerprint`, `detect_profile_drift`, `classify_anomalies` (+ `AnomalyThresholds`: 3× RSS growth over a 64 MiB floor, 95% sustained CPU), and the `record_child`/`PidGuard` RAII lifecycle.
- **Registries** (process-global, populated by `apps.rs`): `record_profile(name, profile)` at `write_profile` records the SHA-256 of exactly what was written; `record_child(name, pid)` at `run_once` returns a `PidGuard` that clears the pid on every return path (so a dead/OS-reused pid is never sampled — the `kill_on_drop` discipline).
- **Sentinel loop** (`sentinel_task`, runtime-only, mirrors `tcc::sentinel_task`: 30 s startup delay, 60 s interval): for each running app it (a) re-reads the on-disk profile and emits `introspect.profile_drift` on mismatch/missing, and (b) samples RSS/CPU via `sysinfo`, seeds a baseline on first sight (silent), then classifies and emits `introspect.anomaly`. An ambient `introspect.snapshot` (`{apps, drift, anomalies}`) closes each tick.
- **Envelope** is byte-identical in shape to the existing `("system", …)` relay, so the HUD renders it with no protocol change; `posture.rs` can fold the counts into its read-only report. Payloads are secret-free (names/counts/fingerprints only).

---

## 6. Safety-model compliance

- **Read-only by construction** — no actuator, no shell passthrough, no keystroke synthesis, no config-write (the deliberate absence of a config-write primitive is honored: the sentinel reads only its own `[introspect].enabled` switch and never mutates config or manifests).
- **Never `ptrace`, never ES AUTH** — the sentinel never signals/kills/injects; if ES is ever added it must be NOTIFY-only.
- **Strengthens the sandbox, never widens it** — profile-drift is a check *on top of* the default-deny generator; it adds no allow-rule. The one new grant-capable key (`jit`) is default-deny, token-bound, and its `true` state is a consequential declaration.
- **No auto-promotion** — flagging is a HUD/posture indicator, never auto-remediation (matching `posture.rs`'s "no remediation path, not even a gated one" and `heal.rs`'s PROPOSE-ONLY discipline).

---

## 7. Honest gaps

**Device-gated (inspection-verified, not CI-run):** the live `sandbox-exec` spawn + real child-pid capture + the `sysinfo` sample all require a real launch (the app tests use a `/bin/sleep` interpreter override). The `sentinel_task` loop itself is runtime-only, like `tcc::sentinel_task`.

**CI-testable (hermetic — and tested):** `sbpl_fingerprint`, `detect_profile_drift`, `classify_anomalies` (all threshold branches incl. the RSS floor and zero-baseline guard), the `PidGuard` clear-on-drop, `record_profile`, the `jit` manifest parse + SBPL derivation + token-binding, and the module attestation core (`parse_module_report`, `attest_modules`, `attest_or_seed` seed-then-detect, the `modules` inbound classification). The HUD side is covered by headless vitest (`hud/src/test/introspect.test.ts`: parsers, reducer, and the review-only panel).

**Built but device-gated at runtime:** the Endpoint Security NOTIFY client (feature `endpoint-security`, off by default). Compile+link verified here; running it needs root + the restricted `com.apple.developer.endpoint-security.client` entitlement + Full Disk Access + a notarized host. To run it on-device: build `cargo build --release --features endpoint-security`, sign the daemon with that entitlement (Apple must approve it against your Team ID) + FDA, and run as root; `es::start()` logs `introspect.es {active:true}` when the kernel accepts the client, or an honest reason otherwise.

**Deferred, with cost:** the tamper-resistant out-of-process module corroboration via `task_for_pid`/`mach2` (debugger entitlement, useless against hardened own-processes) — not required for the shipped self-diagnostics. The live `dyld_report.py` stub and the `sysinfo` sampling are device-gated (the stub is verified on-device; the daemon-side cores are unit-tested).
