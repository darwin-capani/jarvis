# Nexus — SPEC

Low-latency audio routing matrix and studio control surface for the Shure SM7dB chain. Phase-4 implementation against `docs/SANDBOX.md`; HUD panel contract per `docs/HUD.md` §5.

## Sandbox contract (binding: `manifest.toml`)

- Runtime `python` (control plane) hosting a small native audio core (see §2 — the realtime path cannot be Python).
- `audio = true`: the seatbelt profile permits CoreAudio device access; all routing runs in-process against the HAL. The daemon can still mute/indicate via the app socket.
- `net_hosts = []` — fully offline. `fs_read = apps/nexus/presets`, `fs_write = state/tmp/nexus`.
- IPC: JSONL over `state/ipc/apps/nexus.sock`, capability token in every message.
- UI: `surface = "panel"`. Telemetry topics: `audio.levels`, `audio.routes`, `audio.gain`, `audio.clipping`, `audio.spectrum`.

## 1. Routing matrix

- **Aggregate devices.** Nexus creates/destroys CoreAudio aggregate devices via `AudioHardwareCreateAggregateDevice` to bind physical interfaces (SM7dB front-end, the Mac's built-in speakers/headphone amp, virtual loopbacks) into one clocked device. Drift correction enabled for sub-devices not sharing the master clock.
- **Matrix model.** An N-input × M-output gain grid (float per crosspoint, -inf to +12 dB). Routes are crosspoints above -inf. Persisted as named presets (TOML) in `apps/nexus/presets/`.
- **State machine.** One authoritative `MatrixState` (inputs, outputs, crosspoints, mutes, monitor assignment); every mutation goes through it; the IPC layer and the audio core both read snapshots — no shared mutable state across the realtime boundary (SPSC ring of state snapshots).

## 2. Realtime audio core

Python cannot sit on the IOProc. The realtime core is a small native library (Rust `cdylib` or C, loaded via `ctypes`) owning:

- One `AudioDeviceIOProc` on the aggregate device. Buffer 64 frames @ 48 kHz (1.33 ms/callback).
- Crosspoint mix + DSP chain, all in the callback, no locks/allocations/syscalls on the audio thread. Parameters arrive via lock-free SPSC queues with per-block smoothing (5 ms ramps — no zipper noise).
- Meter taps (per-channel peak/RMS) and a 2048-sample FFT tap pushed to the control plane over a lock-free ring; the Python side folds, rate-limits, and publishes telemetry.

### Monitor path budget (< 10 ms target)

| Stage | Cost |
|---|---|
| Input device buffer (64 frames) | 1.33 ms |
| IOProc mix + DSP | < 0.3 ms |
| Output device buffer (64 frames) | 1.33 ms |
| Interface ADC/DAC latency (typical USB) | 2–5 ms |
| **Total** | **~5–8 ms** |

Measured, not assumed: a loopback impulse measurement (`monitor.measure` op) reports actual round-trip in the `audio.routes` payload. If an interface can't hold 64-frame buffers without overloads, step to 128 and report it.

## 3. SM7dB input chain

Gain staging policy, in order:

1. **SM7dB onboard preamp** at +18 or +28 dB (set on the mic) — get gain as early as possible; the spec assumes +28 for spoken voice at 6–12 in.
2. **Interface preamp** trimmed so speech peaks hit **-18 dBFS nominal, -6 dBFS ceiling**. Nexus drives this via the device's gain control when the interface exposes one (`kAudioDevicePropertyVolumeScalar`), else instructs via the panel.
3. **Optional local DSP** (per-input, bypassable, in the native core): HPF 80 Hz 12 dB/oct → gate (-45 dB threshold, 80 ms release) → de-esser (5–8 kHz, 4:1) → compressor (3:1, 10 ms/120 ms, ~4 dB GR target) → output trim.
4. **Clip detect** at -1 dBFS true-peak (4× oversampled) → `audio.clipping` event + panel flash.

## 4. AUv3 effect hosting

- Per-input and per-output insert slots host **AUv3 effects** via `AVAudioEngine`/`AUAudioUnit`. AUv3 plugins run **out-of-process in the system's extension host** — they live outside Nexus's seatbelt by macOS design; Nexus only exchanges audio buffers and parameter data with them. This is the honest sandbox boundary and it is acceptable: plugin code never runs inside the Nexus process.
- Chains (plugin ids + full parameter state via `fullState`) persist in presets. A plugin that fails to instantiate is skipped with a panel warning, never a chain failure.
- The built-in §3 DSP is the default chain; AUv3 replaces or extends it per slot.

## 5. IPC ops (JSONL, token-bearing)

| op | request | effect |
|---|---|---|
| `route.set` | `{in, out, gain_db}` | Set crosspoint (`-inf` clears) |
| `gain.set` | `{channel, gain_db}` | Input/output trim |
| `chain.set` | `{channel, chain: [...]}` | DSP/AUv3 chain config |
| `preset.load` / `preset.save` | `{name}` | Preset I/O in `apps/nexus/presets/` |
| `monitor.set` | `{in, out, on}` | Direct monitor route |
| `monitor.measure` | `{}` | Loopback latency measurement |
| `state.get` | `{}` | Full `MatrixState` snapshot |

Voice control arrives the same way: the daemon classifies "mute the mic" and forwards a `gain.set`/`route.set` to Nexus — Nexus exposes ops, never parses natural language.

## 6. Telemetry → HUD panel

| Topic | Rate | Payload |
|---|---|---|
| `audio.levels` | 30 Hz | `{ch: [{peak_dbfs, rms_dbfs}], lufs_m, lufs_s, lufs_i}` (BS.1770-4 gating for LUFS-I) |
| `audio.spectrum` | 30 Hz | `{bands: [96 floats]}` — 2048-pt FFT folded to 96 log bands, dBFS |
| `audio.routes` | on change + 1 Hz | matrix snapshot + `measured_rtt_ms` |
| `audio.gain` | on change | `{channel, gain_db, stage}` |
| `audio.clipping` | on event | `{channel, true_peak_dbfs}` |

Panel (HUD widget registry): matrix grid (routes), vertical meter pair + LUFS readout (levels), spectrum strip (spectrum), clip flash. All rendered HUD-side from these payloads — Nexus ships no UI code.

## 7. Milestones

1. Native core: aggregate device + IOProc passthrough, measured RTT < 10 ms on the dev interface.
2. Matrix + gain staging + meters; `audio.levels`/`audio.routes` live in the HUD panel.
3. Local DSP chain + clip detect + spectrum.
4. AUv3 hosting + presets; voice-driven ops end-to-end through the daemon.

Non-goals: MIDI control surfaces, network audio (manifest is offline), recording to disk (scratch meters only in `state/tmp/nexus`).
