#!/usr/bin/env python3.11
"""Nexus — DARWIN micro-app: low-latency audio routing matrix + studio control.

The CONTROL PLANE for Project Nexus (SPEC.md §2). It runs as a separate,
seatbelt-sandboxed process launched by darwind, talks to the daemon over a
per-app Unix socket using newline-delimited JSON, and authenticates every line
it SENDS with the capability token darwind minted for this launch.

It does NOT do the realtime audio itself — Python cannot sit on the CoreAudio
IOProc (SPEC §2). The realtime/DSP core is a small Rust `cdylib`
(apps/nexus/core, crate `nexus_core`) loaded here via `ctypes`. This file:
  - loads the cdylib and binds its C-ABI surface (see `core/src/ffi.rs`),
  - owns ONE opaque `Engine` handle,
  - dispatches the SPEC §5 IPC ops (route.set / gain.set / chain.set /
    preset.* / monitor.* / state.get) onto the engine,
  - folds + rate-limits the engine's meters/LUFS/FFT into the SPEC §6 telemetry
    topics (audio.levels / audio.routes / audio.gain / audio.clipping /
    audio.spectrum) and ships them to the HUD via the daemon socket.

Protocol (must match the daemon's app host — daemon/src/apps.rs, and the shipped
global-scan/main.py CONTRACT):
  env DARWIN_APP_SOCKET  abs path to state/ipc/apps/nexus.sock (the daemon owns
                         and binds it; this app CONNECTS, never binds).
  env DARWIN_APP_TOKEN   capability token to stamp on every OUTBOUND line.
  env DARWIN_APP_NAME    this app's name (defaults to "nexus").

  app -> host (one JSON object per line, token-stamped):
    {"token": <env>, "type": "items", "data": {"topic", **fields}}     telemetry
    {"token": <env>, "type": "log",   "data": {"line"}}               a log line
  The daemon's app host only relays "items"/"status"/"log" types (apps.rs
  `classify_inbound_line`); a telemetry drop therefore rides "items" with its
  payload fields FLATTENED into `data` alongside `topic` (the host routes on
  `data.topic`, which must be a manifest-declared topic, and relays the whole
  `data`). Flat — not a nested `payload` object — so the HUD parsers read the
  fields directly, exactly like Vision and the flattened binary apps
  (silicon-canvas / mark-forge `OutboundLine::telemetry`). The
  daemon VERIFIES the token on every inbound line (HMAC over name||perms||nonce)
  and drops anything that does not match; we simply stamp + write.

  host -> app (one JSON object per line — these are NOT token-stamped; the
  daemon authenticates host->app by owning the 0600 socket, per apps.rs
  `send_op`):
    {"type": "start"}                 begin / resume.
    {"type": "stop"}                  stop and exit cleanly.
    {"type": "refresh"}               re-emit current state immediately.
    {"op": "route.set", "in":0, "out":1, "gain_db":0.0}   SPEC §5 ops — the
    {"op": "gain.set",  "channel":0, "gain_db":-3.0}      router forwards an
    {"op": "monitor.set", "in":0, "out":0, "on":true}     already-classified op
    {"op": "monitor.measure"}                             line VERBATIM (it
    {"op": "chain.set", "channel":0, "chain":[...]}       never parses NL — the
    {"op": "preset.load", "name":"vocal"}                 app exposes ops only,
    {"op": "preset.save", "name":"vocal"}                 SPEC §6).
    {"op": "state.get"}

HARD SAFETY (never violated here): this control plane opens NO CoreAudio device,
plays NO audio, and binds NO socket (it CONNECTS to the daemon's). The realtime
device path lives behind the Rust `coreaudio` feature and is DEVICE-GATED — it
is never entered on this headless dev box. Stdlib only on the Python side
(ctypes + socket + json + tomllib); the heavy lifting is the native core.

This is a Foundation SKELETON: the op handlers and the telemetry fold are wired
to the engine where the FFI is already real (routes/gain/mute/monitor/state) and
STUBBED where the native DSP/metering agents have not filled their bodies yet
(levels/spectrum/lufs return engine defaults until then). Downstream agents fill
the disjoint pieces; the socket/token/dispatch wiring here is the stable part.
"""

from __future__ import annotations

import ctypes
import json
import math
import os
import re
import socket
import sys
import threading
import time
from pathlib import Path

try:  # py3.11 stdlib (the venv interpreter is normally 3.11)
    import tomllib
except ModuleNotFoundError:  # py<3.11: fall back to the tomli backport
    try:
        # tomli exposes the identical load(fh)/loads(text) API, so the reader
        # functions below work unchanged — this makes preset LOAD robust on a
        # 3.9/3.10 interpreter instead of hard-failing "tomllib unavailable".
        import tomli as tomllib  # type: ignore[no-redef]
    except ModuleNotFoundError:  # pragma: no cover - no TOML reader at all
        tomllib = None  # type: ignore[assignment]

APP_NAME = "nexus"
APP_DIR = Path(__file__).resolve().parent
PROJECT_ROOT = APP_DIR.parents[1]
PRESETS_DIR = APP_DIR / "presets"            # manifest fs_read
SCRATCH_DIR = PROJECT_ROOT / "state" / "tmp" / "nexus"  # manifest fs_write

# Default engine geometry. The real device layout (aggregate sub-devices) is
# discovered on hardware; headless we configure a small matrix so the control
# plane + telemetry fold are exercisable. SPEC §2: 64 frames @ 48 kHz.
DEFAULT_INPUTS = 4
DEFAULT_OUTPUTS = 4
DEFAULT_SAMPLE_RATE = 48_000
DEFAULT_BLOCK_FRAMES = 64

# Telemetry rates (SPEC §6): levels/spectrum at 30 Hz; routes on change + 1 Hz.
LEVELS_HZ = 30.0
SPECTRUM_HZ = 30.0
ROUTES_MIN_HZ = 1.0

# The C-ABI status codes from core/src/error.rs `codes` (FROZEN — keep in sync).
OK = 0
NULL_POINTER = -1
OUT_OF_BOUNDS = -2
INVALID_PARAM = -3
INVALID_HANDLE = -4
BUFFER_MISMATCH = -5
PRESET_ERROR = -6
DEVICE_ERROR = -7
INTERNAL = -100

# The ABI version this control plane is written against (core/src/ffi.rs
# NEXUS_ABI_VERSION). A mismatch is a hard refusal — never run a stale core.
# v2 adds the audio.clipping drain surface (nexus_clip_capacity/nexus_drain_clips).
EXPECTED_ABI_VERSION = 2


# --------------------------------------------------------------------------- #
# Native core binding (ctypes over the nexus_core cdylib).
# --------------------------------------------------------------------------- #
def _dylib_candidates() -> list[Path]:
    """Where the built cdylib may live: release first (the shipped artifact),
    then debug (a dev `cargo build`). The filename is platform-specific."""
    base = APP_DIR / "core" / "target"
    if sys.platform == "darwin":
        libname = "libnexus_core.dylib"
    elif sys.platform.startswith("win"):
        libname = "nexus_core.dll"
    else:
        libname = "libnexus_core.so"
    return [base / "release" / libname, base / "debug" / libname]


class NexusCore:
    """Thin ctypes wrapper over the `nexus_core` cdylib (core/src/ffi.rs).

    Owns exactly one opaque `Engine*`. Every method declares `argtypes`/
    `restype` to match the C-ABI signatures VERBATIM and raises `NexusCoreError`
    on a negative status code. The realtime `process_block` is NOT driven from
    Python in normal operation (the device IOProc calls into the core directly);
    it is bound here for headless self-test with synthesized buffers only."""

    def __init__(self, lib_path: Path) -> None:
        self._lib = ctypes.CDLL(str(lib_path))
        self._bind_signatures()
        ver = self._lib.nexus_abi_version()
        if ver != EXPECTED_ABI_VERSION:
            raise NexusCoreError(
                f"nexus_core ABI mismatch: core={ver}, control-plane expects "
                f"{EXPECTED_ABI_VERSION}"
            )
        self._engine = self._lib.nexus_engine_create(
            ctypes.c_size_t(DEFAULT_INPUTS),
            ctypes.c_size_t(DEFAULT_OUTPUTS),
            ctypes.c_uint32(DEFAULT_SAMPLE_RATE),
        )
        if not self._engine:
            raise NexusCoreError("nexus_engine_create returned NULL")
        # Pre-size the clip-drain receive buffers ONCE to the core's fixed
        # accumulator capacity (a single drain empties it), so the telemetry poll
        # reuses them instead of reallocating every tick.
        self._clip_cap = int(self._lib.nexus_clip_capacity())
        self._clip_chans = (ctypes.c_uint16 * self._clip_cap)()
        self._clip_peaks = (ctypes.c_float * self._clip_cap)()

    def _bind_signatures(self) -> None:
        L = self._lib
        c_void_p, c_size_t = ctypes.c_void_p, ctypes.c_size_t
        c_i32, c_u32, c_u64 = ctypes.c_int32, ctypes.c_uint32, ctypes.c_uint64
        c_f32, c_bool = ctypes.c_float, ctypes.c_bool
        p_f32, p_size = ctypes.POINTER(ctypes.c_float), ctypes.POINTER(c_size_t)
        c_char_p = ctypes.c_char_p

        L.nexus_abi_version.argtypes = []
        L.nexus_abi_version.restype = c_u32

        L.nexus_engine_create.argtypes = [c_size_t, c_size_t, c_u32]
        L.nexus_engine_create.restype = c_void_p
        L.nexus_engine_destroy.argtypes = [c_void_p]
        L.nexus_engine_destroy.restype = None
        L.nexus_engine_inputs.argtypes = [c_void_p, p_size]
        L.nexus_engine_inputs.restype = c_i32
        L.nexus_engine_outputs.argtypes = [c_void_p, p_size]
        L.nexus_engine_outputs.restype = c_i32

        L.nexus_set_crosspoint.argtypes = [c_void_p, c_size_t, c_size_t, c_f32]
        L.nexus_set_crosspoint.restype = c_i32
        L.nexus_set_input_trim.argtypes = [c_void_p, c_size_t, c_f32]
        L.nexus_set_input_trim.restype = c_i32
        L.nexus_set_output_trim.argtypes = [c_void_p, c_size_t, c_f32]
        L.nexus_set_output_trim.restype = c_i32
        L.nexus_set_input_mute.argtypes = [c_void_p, c_size_t, c_bool]
        L.nexus_set_input_mute.restype = c_i32
        L.nexus_set_output_mute.argtypes = [c_void_p, c_size_t, c_bool]
        L.nexus_set_output_mute.restype = c_i32
        L.nexus_set_monitor_output.argtypes = [c_void_p, c_i32]
        L.nexus_set_monitor_output.restype = c_i32

        L.nexus_process_block.argtypes = [
            c_void_p,
            ctypes.POINTER(p_f32), c_size_t,   # inputs, input_count
            ctypes.POINTER(p_f32), c_size_t,   # outputs, output_count
            c_size_t, ctypes.c_uint16, c_u32,  # frames, channels, sample_rate
        ]
        L.nexus_process_block.restype = c_i32

        L.nexus_get_channel_meter.argtypes = [c_void_p, c_size_t, p_f32, p_f32]
        L.nexus_get_channel_meter.restype = c_i32
        L.nexus_get_loudness.argtypes = [c_void_p, p_f32, p_f32, p_f32]
        L.nexus_get_loudness.restype = c_i32
        L.nexus_get_spectrum.argtypes = [c_void_p, p_f32, c_size_t]
        L.nexus_get_spectrum.restype = c_i32
        L.nexus_spectrum_band_count.argtypes = []
        L.nexus_spectrum_band_count.restype = c_size_t
        # audio.clipping drain surface (ABI v2). Parallel arrays out: channels
        # (uint16) + true-peaks (float), plus an out-count. See core/src/ffi.rs.
        p_u16 = ctypes.POINTER(ctypes.c_uint16)
        L.nexus_clip_capacity.argtypes = []
        L.nexus_clip_capacity.restype = c_size_t
        L.nexus_drain_clips.argtypes = [c_void_p, p_u16, p_f32, c_size_t, p_size]
        L.nexus_drain_clips.restype = c_i32
        L.nexus_get_crosspoint.argtypes = [c_void_p, c_size_t, c_size_t, p_f32]
        L.nexus_get_crosspoint.restype = c_i32
        L.nexus_matrix_revision.argtypes = [c_void_p]
        L.nexus_matrix_revision.restype = c_u64

        L.nexus_preset_save_path.argtypes = [c_void_p, c_char_p]
        L.nexus_preset_save_path.restype = c_i32
        L.nexus_preset_load_path.argtypes = [c_void_p, c_char_p]
        L.nexus_preset_load_path.restype = c_i32

    # --- control ops ------------------------------------------------------- #
    def set_crosspoint(self, inp: int, out: int, gain_db: float) -> None:
        self._check(self._lib.nexus_set_crosspoint(self._engine, inp, out, gain_db), "set_crosspoint")

    def set_input_trim(self, channel: int, gain_db: float) -> None:
        self._check(self._lib.nexus_set_input_trim(self._engine, channel, gain_db), "set_input_trim")

    def set_output_trim(self, channel: int, gain_db: float) -> None:
        self._check(self._lib.nexus_set_output_trim(self._engine, channel, gain_db), "set_output_trim")

    def set_input_mute(self, channel: int, muted: bool) -> None:
        self._check(self._lib.nexus_set_input_mute(self._engine, channel, muted), "set_input_mute")

    def set_output_mute(self, channel: int, muted: bool) -> None:
        self._check(self._lib.nexus_set_output_mute(self._engine, channel, muted), "set_output_mute")

    def set_monitor_output(self, output: int | None) -> None:
        # Negative clears the assignment (FFI contract).
        sel = -1 if output is None else int(output)
        self._check(self._lib.nexus_set_monitor_output(self._engine, sel), "set_monitor_output")

    def process_block(
        self,
        inputs: list[list[float]],
        out_channels: int,
        channels: int = 1,
        sample_rate: int = DEFAULT_SAMPLE_RATE,
    ) -> list[list[float]]:
        """Drive ONE realtime block through the core with synthesized buffers
        (headless self-test only — on hardware the CoreAudio IOProc calls the core
        directly, never Python). Each `inputs[i]` is one interleaved input buffer of
        `frames * channels` samples; returns `out_channels` output buffers."""
        n = len(inputs[0]) if inputs else 0
        frames = n // max(channels, 1)
        PF = ctypes.POINTER(ctypes.c_float)
        in_bufs = [(ctypes.c_float * n)(*chan) for chan in inputs]
        out_bufs = [(ctypes.c_float * n)() for _ in range(out_channels)]
        in_arr = (PF * len(in_bufs))(*[ctypes.cast(b, PF) for b in in_bufs])
        out_arr = (PF * len(out_bufs))(*[ctypes.cast(b, PF) for b in out_bufs])
        self._check(
            self._lib.nexus_process_block(
                self._engine, in_arr, len(in_bufs), out_arr, len(out_bufs),
                ctypes.c_size_t(frames), ctypes.c_uint16(channels), ctypes.c_uint32(sample_rate),
            ),
            "process_block",
        )
        return [list(b) for b in out_bufs]

    def inputs(self) -> int:
        n = ctypes.c_size_t(0)
        self._check(self._lib.nexus_engine_inputs(self._engine, ctypes.byref(n)), "inputs")
        return n.value

    def outputs(self) -> int:
        n = ctypes.c_size_t(0)
        self._check(self._lib.nexus_engine_outputs(self._engine, ctypes.byref(n)), "outputs")
        return n.value

    def crosspoint(self, inp: int, out: int) -> float:
        g = ctypes.c_float(0.0)
        self._check(self._lib.nexus_get_crosspoint(self._engine, inp, out, ctypes.byref(g)), "crosspoint")
        return g.value

    def matrix_revision(self) -> int:
        return int(self._lib.nexus_matrix_revision(self._engine))

    # --- meter getters (SPEC §6) ------------------------------------------ #
    def channel_meter(self, channel: int) -> tuple[float, float]:
        peak, rms = ctypes.c_float(0.0), ctypes.c_float(0.0)
        self._check(
            self._lib.nexus_get_channel_meter(self._engine, channel, ctypes.byref(peak), ctypes.byref(rms)),
            "channel_meter",
        )
        return peak.value, rms.value

    def loudness(self) -> tuple[float, float, float]:
        m, s, i = ctypes.c_float(0.0), ctypes.c_float(0.0), ctypes.c_float(0.0)
        self._check(
            self._lib.nexus_get_loudness(self._engine, ctypes.byref(m), ctypes.byref(s), ctypes.byref(i)),
            "loudness",
        )
        return m.value, s.value, i.value

    def spectrum(self) -> list[float]:
        n = int(self._lib.nexus_spectrum_band_count())
        buf = (ctypes.c_float * n)()
        self._check(self._lib.nexus_get_spectrum(self._engine, buf, n), "spectrum")
        return list(buf)

    def drain_clips(self) -> list[tuple[int, float]]:
        """Drain the true-peak clip events accumulated by the realtime core since
        the last call (SPEC §6 audio.clipping). Returns a list of
        (channel, true_peak_dbfs); empty when nothing clipped. Each event is
        reported once (the core clears them on drain)."""
        count = ctypes.c_size_t(0)
        self._check(
            self._lib.nexus_drain_clips(
                self._engine, self._clip_chans, self._clip_peaks,
                ctypes.c_size_t(self._clip_cap), ctypes.byref(count),
            ),
            "drain_clips",
        )
        return [
            (int(self._clip_chans[i]), float(self._clip_peaks[i]))
            for i in range(count.value)
        ]

    def _check(self, code: int, op: str) -> None:
        if code != OK:
            raise NexusCoreError(f"{op} failed: status {code}")

    def close(self) -> None:
        if getattr(self, "_engine", None):
            self._lib.nexus_engine_destroy(self._engine)
            self._engine = None


class NexusCoreError(RuntimeError):
    """A negative status code from the native core, or a load/ABI failure."""


def load_core() -> NexusCore:
    """Locate + load the cdylib, or raise with a clear message."""
    errors: list[str] = []
    for cand in _dylib_candidates():
        if cand.exists():
            try:
                return NexusCore(cand)
            except OSError as exc:  # ctypes.CDLL failure
                errors.append(f"{cand}: {exc}")
    searched = ", ".join(str(c) for c in _dylib_candidates())
    detail = ("; ".join(errors)) if errors else "not found"
    raise NexusCoreError(
        f"could not load nexus_core cdylib ({detail}). Build it with "
        f"`cargo build` (or `--release`) in apps/nexus/core. Searched: {searched}"
    )


# --------------------------------------------------------------------------- #
# Host IPC: an authenticated writer + a command reader (mirrors global-scan).
# --------------------------------------------------------------------------- #
class HostLink:
    """Newline-delimited JSON link to the daemon's app host over a Unix socket.

    Every OUTBOUND line carries the capability token from the environment; the
    daemon verifies it and drops anything that does not match (apps.rs
    handle_conn). Writes are serialized with a lock because the telemetry thread
    and the command thread can both emit."""

    def __init__(self, sock_path: str, token: str) -> None:
        self._token = token
        self._sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._sock.connect(sock_path)
        self._wlock = threading.Lock()
        self._rfile = self._sock.makefile("r", encoding="utf-8", newline="\n")

    def send(self, msg_type: str, data: dict) -> None:
        line = json.dumps(
            {"token": self._token, "type": msg_type, "data": data},
            ensure_ascii=False,
            separators=(",", ":"),
        )
        payload = (line + "\n").encode("utf-8")
        with self._wlock:
            try:
                self._sock.sendall(payload)
            except (BrokenPipeError, OSError):
                pass  # host went away; the reader will observe EOF and stop us

    def telemetry(self, topic: str, payload: dict) -> None:
        """Emit one SPEC §6 telemetry line, routed by `topic`.

        The daemon's app host (daemon/src/apps.rs `classify_inbound_line`) ONLY
        relays lines whose `type` is "items"/"status"/"log" — a "telemetry" type
        is DROPPED. It routes on `data.topic` (which must be one of the manifest's
        declared topics) and relays the whole `data` object as the payload. The
        payload fields are FLATTENED into `data` alongside `topic` — i.e.
        `data = {topic, **payload}`, NOT a nested `{topic, payload}` — because the
        HUD parsers read the fields FLAT (`data["ch"]`, ...), exactly like Vision
        and the flattened binary apps (silicon-canvas / mark-forge). A nested
        `payload` object would leave every HUD field one level too deep and render
        blank panels. `topic` is applied AFTER `**payload` so the trusted routing
        topic can never be overridden by a stray `topic` key in the payload."""
        self.send("items", {**payload, "topic": topic})

    def log(self, line: str) -> None:
        self.send("log", {"line": line})

    def commands(self):
        """Yield host->app command/op dicts until the connection closes."""
        for raw in self._rfile:
            raw = raw.strip()
            if not raw:
                continue
            try:
                yield json.loads(raw)
            except json.JSONDecodeError:
                continue

    def close(self) -> None:
        for closer in (self._rfile.close, self._sock.close):
            try:
                closer()
            except OSError:
                pass


# --------------------------------------------------------------------------- #
# Op dispatch (SPEC §5). The router forwards an already-classified op line; we
# map it onto the engine. The app NEVER parses natural language (SPEC §6).
# --------------------------------------------------------------------------- #
class OpDispatcher:
    """Maps SPEC §5 op dicts onto the native engine, with validation. Each
    handler returns a small ack dict logged for the HUD; failures are caught and
    surfaced as a log line, never a crash (one bad op must not kill the app)."""

    def __init__(self, core: NexusCore, link: HostLink) -> None:
        self._core = core
        self._link = link
        self._handlers = {
            "route.set": self._route_set,
            "gain.set": self._gain_set,
            "chain.set": self._chain_set,
            "monitor.set": self._monitor_set,
            "monitor.measure": self._monitor_measure,
            "preset.load": self._preset_load,
            "preset.save": self._preset_save,
            "state.get": self._state_get,
        }

    def dispatch(self, op: str, msg: dict) -> None:
        handler = self._handlers.get(op)
        if handler is None:
            self._link.log(f"unknown op {op!r} (ignored)")
            return
        try:
            handler(msg)
        except (NexusCoreError, KeyError, ValueError, TypeError) as exc:
            self._link.log(f"op {op} failed: {exc}")

    # --- handlers ---------------------------------------------------------- #
    def _route_set(self, msg: dict) -> None:
        # {"op":"route.set","in":0,"out":1,"gain_db":0.0}; -inf clears.
        gain = msg.get("gain_db", float("-inf"))
        gain = float("-inf") if gain in (None, "-inf", "off") else float(gain)
        self._core.set_crosspoint(int(msg["in"]), int(msg["out"]), gain)
        self.emit_routes()

    def _gain_set(self, msg: dict) -> None:
        # {"op":"gain.set","channel":0,"gain_db":-3.0,"stage":"input"|"output"}
        # or "mute the mic" -> {"op":"gain.set","channel":0,"mute":true}.
        channel = int(msg["channel"])
        stage = msg.get("stage", "input")
        if "mute" in msg:
            muted = bool(msg["mute"])
            (self._core.set_input_mute if stage != "output" else self._core.set_output_mute)(channel, muted)
            # A mute/unmute rides a DISTINCT payload: {muted} instead of
            # {gain_db}. The HUD's parseNexusGain accepts either; the old
            # gain_db=null frame was rejected wholesale, so mutes never showed.
            self._link.telemetry("audio.gain", {"channel": channel, "muted": muted, "stage": stage})
        else:
            gain = float(msg["gain_db"])
            (self._core.set_input_trim if stage != "output" else self._core.set_output_trim)(channel, gain)
            self._link.telemetry("audio.gain", {"channel": channel, "gain_db": gain, "stage": stage})

    def _monitor_set(self, msg: dict) -> None:
        # {"op":"monitor.set","in":0,"out":0,"on":true} — a direct monitor route.
        on = bool(msg.get("on", True))
        out = int(msg["out"])
        self._core.set_monitor_output(out if on else None)
        if "in" in msg:
            self._core.set_crosspoint(int(msg["in"]), out, 0.0 if on else float("-inf"))
        self.emit_routes()

    def _monitor_measure(self, _msg: dict) -> None:
        # SPEC §2: loopback RTT — DEVICE-GATED. No headless number exists; report
        # null so the HUD shows "unmeasured" rather than a fabricated latency.
        self._link.telemetry(
            "audio.routes",
            {"measured_rtt_ms": None, "note": "device-gated; measured only on hardware"},
        )

    def _chain_set(self, msg: dict) -> None:
        # {"op":"chain.set","channel":0,"chain":[...]} — DSP/AUv3 chain config.
        # The chain->engine plumbing is filled by the dsp module agent (the FFI
        # chain setter is not yet exposed); acknowledge so the HUD reflects it.
        self._link.log(f"chain.set on channel {msg.get('channel')} accepted (chain plumbing pending)")

    def _preset_load(self, msg: dict) -> None:
        name = _safe_preset_name(msg["name"])
        path = PRESETS_DIR / f"{name}.toml"
        if not path.exists():
            raise ValueError(f"preset {name!r} not found")
        # Foundation: preset I/O lives on the Python side (it holds the
        # apps/nexus/presets fs_read grant). The preset agent fills the apply
        # loop that replays the TOML onto the engine via set_crosspoint/etc.
        doc = _read_toml(path)
        self._apply_preset(doc)
        self.emit_routes()

    def _preset_save(self, msg: dict) -> None:
        name = _safe_preset_name(msg["name"])
        SCRATCH_DIR.mkdir(parents=True, exist_ok=True)
        # Saved to scratch (fs_write); a curated copy into presets/ is a manual
        # step (presets/ is fs_read only — we cannot write it). Serialize the
        # current matrix from the engine getters.
        doc = self._serialize_state()
        out = SCRATCH_DIR / f"{name}.toml"
        out.write_text(_to_toml(doc), encoding="utf-8")
        self._link.log(f"preset {name!r} saved to scratch ({out})")

    def _state_get(self, _msg: dict) -> None:
        self.emit_routes()

    # --- helpers ----------------------------------------------------------- #
    def _apply_preset(self, doc: dict) -> None:
        # Minimal replay: a [[route]] array of {in, out, gain_db}. The preset
        # agent extends this (chains, monitor, trims). Tolerant of absence.
        for route in doc.get("route", []):
            self._core.set_crosspoint(int(route["in"]), int(route["out"]), float(route["gain_db"]))

    def _serialize_state(self) -> dict:
        routes = []
        for i in range(self._core.inputs()):
            for o in range(self._core.outputs()):
                g = self._core.crosspoint(i, o)
                if g != float("-inf"):
                    routes.append({"in": i, "out": o, "gain_db": g})
        return {"route": routes}

    def emit_routes(self) -> None:
        """SPEC §6 audio.routes: the matrix snapshot (+ measured_rtt_ms=None
        headlessly). Sent on every routing change and on the 1 Hz heartbeat."""
        snap = self._serialize_state()
        # The WIRE key is "matrix" (the HUD's parseNexusRoutes reads
        # data["matrix"]; under "route" the crosspoints were dropped and the
        # panel grid rendered empty). The preset TOML keeps its [[route]]
        # tables (_serialize_state / _apply_preset) — the rename is wire-only.
        snap["matrix"] = snap.pop("route")
        snap["inputs"] = self._core.inputs()
        snap["outputs"] = self._core.outputs()
        snap["revision"] = self._core.matrix_revision()
        snap["measured_rtt_ms"] = None  # device-gated; never fabricated
        self._link.telemetry("audio.routes", snap)


_PRESET_NAME_RE = re.compile(r"^[A-Za-z0-9._-]+$")


def _safe_preset_name(name: str) -> str:
    """Reject anything but a strict slug in a preset name (we only ever touch
    presets/ and scratch/). Allowlist `[A-Za-z0-9._-]` only — this rejects path
    traversal (`/`, `\\`, leading `.`) AND the looser characters the old
    blocklist let through (spaces, `=`, quotes, newlines, NUL, `*`, glob/shell
    metacharacters) that could confuse the TOML/path layer downstream. A leading
    dot is still rejected explicitly so `.`/`..`/dotfiles can never slip through
    even though `.` is in the allowed set."""
    name = str(name).strip()
    if not name or name.startswith(".") or not _PRESET_NAME_RE.match(name):
        raise ValueError(f"invalid preset name {name!r}")
    return name


def _read_toml(path: Path) -> dict:
    if tomllib is None:
        raise ValueError("tomllib unavailable (need py3.11+)")
    with path.open("rb") as fh:
        return tomllib.load(fh)


def _read_toml_from_str(text: str) -> dict:
    """Parse TOML from an in-memory string (the inverse of `_to_toml`, used to
    prove save/load round-trip identity without touching the filesystem)."""
    if tomllib is None:
        raise ValueError("tomllib unavailable (need py3.11+)")
    return tomllib.loads(text)


def _to_toml(doc: dict) -> str:
    """Minimal TOML emitter for the preset shape we save ([[route]] tables).
    Avoids a third-party tomli-w dependency (stdlib has no TOML writer)."""
    lines: list[str] = []
    for route in doc.get("route", []):
        lines.append("[[route]]")
        for key in ("in", "out", "gain_db"):
            if key in route:
                lines.append(f"{key} = {route[key]!r}" if not isinstance(route[key], float)
                             else f"{key} = {route[key]}")
        lines.append("")
    return "\n".join(lines) + "\n"


# --------------------------------------------------------------------------- #
# Telemetry loop (SPEC §6): fold + rate-limit the engine meters, publish.
# --------------------------------------------------------------------------- #
def telemetry_loop(core: NexusCore, link: HostLink, dispatcher: OpDispatcher, stop: threading.Event) -> None:
    """Poll the engine's meter getters and publish levels/spectrum/routes at
    their SPEC §6 rates. Headlessly the native DSP/metering agents have not
    filled their bodies, so the getters return engine defaults (silence) — the
    FOLD + RATE-LIMIT + WIRE shape is what this skeleton establishes; the numbers
    light up once the metering agent lands."""
    next_levels = 0.0
    next_spectrum = 0.0
    next_routes = 0.0
    while not stop.is_set():
        now = time.monotonic()
        try:
            if now >= next_levels:
                _emit_levels(core, link)
                # Drain + publish any true-peak clip events on the same 30 Hz tick
                # (they are audio-thread-fed like the levels). On-event: emits only
                # when the core actually clipped, so quiet audio ships nothing.
                _emit_clipping(core, link)
                next_levels = now + 1.0 / LEVELS_HZ
            if now >= next_spectrum:
                _emit_spectrum(core, link)
                next_spectrum = now + 1.0 / SPECTRUM_HZ
            if now >= next_routes:
                dispatcher.emit_routes()  # 1 Hz heartbeat (also fires on change)
                next_routes = now + 1.0 / ROUTES_MIN_HZ
        except NexusCoreError as exc:
            link.log(f"telemetry error: {exc}")
        # Sleep to the nearest pending deadline (bounded so stop() is responsive).
        sleep_for = max(0.0, min(next_levels, next_spectrum, next_routes) - time.monotonic())
        stop.wait(timeout=min(sleep_for, 0.1))


def _emit_levels(core: NexusCore, link: HostLink) -> None:
    ch = []
    for c in range(core.inputs()):
        peak, rms = core.channel_meter(c)
        ch.append({"peak_dbfs": _finite(peak), "rms_dbfs": _finite(rms)})
    m, s, i = core.loudness()
    link.telemetry(
        "audio.levels",
        {"ch": ch, "lufs_m": _finite(m), "lufs_s": _finite(s), "lufs_i": _finite(i)},
    )


def _emit_spectrum(core: NexusCore, link: HostLink) -> None:
    bands = [_finite(b) for b in core.spectrum()]
    link.telemetry("audio.spectrum", {"bands": bands})


def _emit_clipping(core: NexusCore, link: HostLink) -> None:
    """SPEC §6 audio.clipping (on event): drain the core's true-peak clip events
    and ship one telemetry line per event. The HUD (hud/src/core/events.ts
    parseNexusClipping) reads `channel` + `true_peak_dbfs` FLAT and flashes the
    Nexus panel. Nothing is emitted when nothing clipped."""
    for channel, true_peak_dbfs in core.drain_clips():
        link.telemetry(
            "audio.clipping",
            {"channel": channel, "true_peak_dbfs": _finite(true_peak_dbfs)},
        )


def _finite(x: float) -> float | None:
    """JSON has no -inf; map silence (-inf / NaN) to None so the HUD shows an
    empty meter rather than a malformed payload."""
    return None if (x is None or math.isinf(x) or math.isnan(x)) else x


# --------------------------------------------------------------------------- #
# Main run loop (sandboxed mode, driven by the host).
# --------------------------------------------------------------------------- #
def main() -> int:
    sock_path = os.environ.get("DARWIN_APP_SOCKET")
    token = os.environ.get("DARWIN_APP_TOKEN")
    if not sock_path or not token:
        sys.stderr.write(
            "nexus: DARWIN_APP_SOCKET and DARWIN_APP_TOKEN must be set "
            "(this app runs under darwind, not standalone)\n"
        )
        return 2

    try:
        core = load_core()
    except NexusCoreError as exc:
        sys.stderr.write(f"nexus: {exc}\n")
        return 1

    try:
        link = HostLink(sock_path, token)
    except OSError as exc:
        sys.stderr.write(f"nexus: cannot connect to host socket: {exc}\n")
        core.close()
        return 1

    dispatcher = OpDispatcher(core, link)
    link.log(
        f"nexus online: {core.inputs()}x{core.outputs()} matrix @ "
        f"{DEFAULT_SAMPLE_RATE} Hz, {DEFAULT_BLOCK_FRAMES}-frame blocks "
        "(realtime device path device-gated; not opened headlessly)"
    )

    stop = threading.Event()
    tele = threading.Thread(
        target=telemetry_loop, args=(core, link, dispatcher, stop), name="nexus-telemetry", daemon=True
    )
    tele.start()

    # Command/op reader (blocks on the socket until the host closes it).
    try:
        for msg in link.commands():
            ctype = msg.get("type")
            if ctype == "stop":
                break
            if ctype in ("start", "refresh"):
                dispatcher.emit_routes()
                continue
            op = msg.get("op")
            if op:
                dispatcher.dispatch(op, msg)
    except OSError:
        pass
    finally:
        stop.set()
        tele.join(timeout=2.0)
        link.log("nexus stopping")
        link.close()
        core.close()
    return 0


# --------------------------------------------------------------------------- #
# In-process self-test (no daemon, no socket): load the cdylib, drive the engine
# through the FFI with synthesized buffers, prove the ctypes contract. Opens NO
# device, plays NO audio, binds NO socket.
# --------------------------------------------------------------------------- #
def selftest() -> int:
    try:
        core = load_core()
    except NexusCoreError as exc:
        print(f"FAIL load: {exc}")
        return 1
    print(f"loaded nexus_core; ABI {EXPECTED_ABI_VERSION}")
    print(f"engine: {core.inputs()}x{core.outputs()} matrix")

    # Route input 0 -> output 0 at -3 dB and read it back through the FFI.
    core.set_crosspoint(0, 0, -3.0)
    got = core.crosspoint(0, 0)
    print(f"crosspoint(0,0) set -3.0 -> read {got}")
    assert abs(got - (-3.0)) < 1e-5, "crosspoint readback mismatch"

    # Clear it with -inf and confirm.
    core.set_crosspoint(0, 0, float("-inf"))
    assert core.crosspoint(0, 0) == float("-inf"), "clear failed"
    print("crosspoint clear (-inf) ok")

    # Out-of-range index must surface as a NexusCoreError (status OUT_OF_BOUNDS).
    try:
        core.set_crosspoint(99, 0, 0.0)
        print("FAIL: out-of-range crosspoint did not error")
        return 1
    except NexusCoreError:
        print("out-of-range crosspoint correctly rejected")

    # Mute + monitor ops through the FFI.
    core.set_input_mute(0, True)
    core.set_monitor_output(1)
    print(f"mute/monitor ops ok; matrix revision now {core.matrix_revision()}")

    # Meter getters return valid (silent) values headlessly (metering agent fills
    # the real numbers). Confirm the ctypes out-pointer plumbing works.
    peak, rms = core.channel_meter(0)
    m, s, i = core.loudness()
    bands = core.spectrum()
    print(f"levels: peak={peak} rms={rms} lufs=({m},{s},{i}) spectrum_bands={len(bands)}")
    assert len(bands) == 96, "spectrum must fold to 96 bands"

    # audio.clipping (ABI v2): drive a full-scale block through the realtime path
    # and confirm the true-peak clip detector fires + drains through the FFI.
    core.set_input_mute(0, False)  # undo the earlier mute so ch0 has signal
    core.set_crosspoint(0, 0, 0.0)
    core.set_monitor_output(0)
    assert core.drain_clips() == [], "no clips before any audio"
    core.process_block([[1.0] * DEFAULT_BLOCK_FRAMES], out_channels=1, channels=1)
    clips = core.drain_clips()
    assert clips and clips[0][0] == 0, f"expected a true-peak clip on ch0, got {clips!r}"
    assert clips[0][1] >= -1.0, f"clip true-peak {clips[0][1]} dBFS should meet the -1 dBFS ceiling"
    print(f"clip drain: ch{clips[0][0]} @ {clips[0][1]:.2f} dBTP — audio.clipping path verified")
    assert core.drain_clips() == [], "clip events must be reported once (cleared on drain)"

    core.close()
    print("PASS: ctypes <-> cdylib contract verified (no device, no socket, no audio)")
    return 0


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        raise SystemExit(selftest())
    raise SystemExit(main())
