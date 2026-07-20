#!/usr/bin/env python3
"""Wave / RF / resonance solver (EM, LC, RC). Pure, offline."""
import math
import os
import sys

# Shared host-link plumbing (socket loop, token stamping, frame bound, the
# agent-tool id echo) from apps/_sdk — fs_read-granted. The path is resolved
# relative to THIS file (apps/<app>/main.py -> ../_sdk), so it works both when
# darwind launches the app (cwd = project root) and when the tests run from the
# app dir. Bytecode writes are disabled since apps/_sdk is read-only in the
# sandbox. Re-importing drain_lines/MAX_FRAME_BYTES/TOKEN keeps them resolvable
# off `main` for the framing/contract tests.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
from harness import (  # noqa: E402 — must follow the sys.path insert above
    MAX_FRAME_BYTES,
    TOKEN,
    drain_lines,
    reply_result,
    run,
    send,
)

# --- physical constants (SI) ---
LIGHT_C = 299792458.0          # speed of light, m/s
PLANCK_H = 6.62607015e-34      # Planck constant, J*s
EV_J = 1.602176634e-19         # electronvolt, J

# SI prefix multipliers (case-sensitive so mega 'M' != milli 'm')
_PREFIX = {
    "": 1.0,
    "T": 1e12, "G": 1e9, "M": 1e6, "k": 1e3, "K": 1e3,
    "m": 1e-3, "u": 1e-6, "µ": 1e-6, "μ": 1e-6,
    "n": 1e-9, "p": 1e-12, "f": 1e-15,
}

# unit tokens stripped from a value's tail (compared lower-cased, longest first)
_UNITS = ["ohm", "hz", "ω", "f", "h", "s", "m", "v", "a"]


def _split_num(s):
    """Return (number_str, suffix_str) or (None, None). Pure, never raises."""
    n = len(s)
    i = 0
    if i < n and s[i] in "+-":
        i += 1
    seen_digit = False
    seen_dot = False
    while i < n and (s[i].isdigit() or s[i] == "."):
        if s[i] == ".":
            if seen_dot:
                break
            seen_dot = True
        else:
            seen_digit = True
        i += 1
    if seen_digit and i < n and s[i] in "eE":
        j = i + 1
        if j < n and s[j] in "+-":
            j += 1
        if j < n and s[j].isdigit():
            j += 1
            while j < n and s[j].isdigit():
                j += 1
            i = j
    if not seen_digit:
        return None, None
    return s[:i], s[i:]


def _strip_unit(suffix):
    low = suffix.lower()
    for u in _UNITS:
        if len(suffix) >= len(u) and low.endswith(u):
            return suffix[:len(suffix) - len(u)]
    return suffix


def _parse_si(s):
    """Parse an SI-prefixed magnitude string (e.g. '2.4GHz','100pF'). None on failure."""
    s = s.strip()
    if not s:
        return None
    num_str, suffix = _split_num(s)
    if num_str is None:
        return None
    try:
        num = float(num_str)
    except (ValueError, OverflowError):
        return None
    prefix = _strip_unit(suffix.strip()).strip()
    if prefix not in _PREFIX:
        return None
    val = num * _PREFIX[prefix]
    if not math.isfinite(val):
        return None
    return val


def _num(v):
    """Coerce int/float/SI-string to a finite float. None on failure. Rejects bool."""
    if isinstance(v, bool):
        return None
    if isinstance(v, (int, float)):
        f = float(v)
        return f if math.isfinite(f) else None
    if isinstance(v, str):
        return _parse_si(v)
    return None


def _pos(v):
    """Coerce to a strictly-positive finite float, else None."""
    n = _num(v)
    if n is None or n <= 0:
        return None
    return n


def _em(payload):
    has_f = payload.get("frequency") is not None
    has_w = payload.get("wavelength") is not None
    if has_f == has_w:
        return {"error": "provide exactly one of 'frequency' or 'wavelength'"}
    vf_in = payload.get("velocity_factor", 1)
    if vf_in is None:
        vf_in = 1
    vf = _num(vf_in)
    if vf is None:
        return {"error": "velocity_factor must be a number"}
    if not (0 < vf <= 1):
        return {"error": "velocity_factor must satisfy 0 < vf <= 1"}
    v = vf * LIGHT_C
    if has_f:
        f = _pos(payload.get("frequency"))
        if f is None:
            return {"error": "frequency must be a positive number"}
        wavelength = v / f
    else:
        wavelength = _pos(payload.get("wavelength"))
        if wavelength is None:
            return {"error": "wavelength must be a positive number"}
        f = v / wavelength
    photon_energy_j = PLANCK_H * f
    return {
        "frequency": f,
        "wavelength": wavelength,
        "period": 1.0 / f,
        "velocity_factor": vf,
        "photon_energy_ev": photon_energy_j / EV_J,
        "photon_energy_j": photon_energy_j,
    }


def _lc(payload):
    ind = payload.get("inductance")
    cap = payload.get("capacitance")
    freq = payload.get("frequency")
    has_l = ind is not None
    has_c = cap is not None
    has_f = freq is not None
    if has_l and has_c and not has_f:
        lv = _pos(ind)
        if lv is None:
            return {"error": "inductance must be a positive number"}
        cv = _pos(cap)
        if cv is None:
            return {"error": "capacitance must be a positive number"}
        return {"resonant_frequency": 1.0 / (2.0 * math.pi * math.sqrt(lv * cv))}
    if has_f and (has_l != has_c):
        fv = _pos(freq)
        if fv is None:
            return {"error": "frequency must be a positive number"}
        w2 = (2.0 * math.pi * fv) ** 2
        if has_l:
            lv = _pos(ind)
            if lv is None:
                return {"error": "inductance must be a positive number"}
            return {"capacitance": 1.0 / (w2 * lv)}
        cv = _pos(cap)
        if cv is None:
            return {"error": "capacitance must be a positive number"}
        return {"inductance": 1.0 / (w2 * cv)}
    return {"error": "lc needs (inductance & capacitance) or (frequency & exactly one of inductance/capacitance)"}


def _rc(payload):
    r = _pos(payload.get("resistance"))
    if r is None:
        return {"error": "resistance must be a positive number"}
    cap = _pos(payload.get("capacitance"))
    if cap is None:
        return {"error": "capacitance must be a positive number"}
    tau = r * cap
    return {"time_constant": tau, "cutoff_frequency": 1.0 / (2.0 * math.pi * tau)}


def _finite_checked(out):
    """Reject any non-finite float in a result dict: json.dumps would emit a
    bare Infinity/NaN token (invalid JSON per RFC 8259), and the daemon's
    strict parser would DROP the frame — the caller would get silence instead
    of an answer. An honest error dict is the correct reply. Never raises."""
    if isinstance(out, dict) and "error" not in out:
        for k, v in out.items():
            if isinstance(v, float) and not math.isfinite(v):
                return {"error": "%s overflows the representable range (result is not finite)" % k}
    return out


def compute(payload):
    """PURE, offline, no I/O, never raises. Wave/RF/resonance solver.

    Dispatches on payload['mode'] (default 'em' when frequency/wavelength present):
      em -> exactly one of frequency(Hz)/wavelength(m), optional velocity_factor(0<vf<=1);
            returns frequency, wavelength, period, velocity_factor, photon_energy_ev/_j.
      lc -> resonant_frequency from L&C, or the missing L/C from frequency + one of them.
      rc -> time_constant and cutoff_frequency from resistance & capacitance.
    SI-prefixed strings ('2.4GHz','100pF','10uH','1kohm') are accepted. Bad/missing/
    zero/negative inputs, or both/neither of a mutually-exclusive pair -> {'error': ...}."""
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}
        mode = payload.get("mode")
        if mode is None:
            if payload.get("frequency") is not None or payload.get("wavelength") is not None:
                mode = "em"
            else:
                return {"error": "missing 'mode' (expected 'em', 'lc', or 'rc')"}
        if not isinstance(mode, str):
            return {"error": "mode must be a string"}
        mode = mode.strip().lower()
        if mode == "em":
            return _finite_checked(_em(payload))
        if mode == "lc":
            return _finite_checked(_lc(payload))
        if mode == "rc":
            return _finite_checked(_rc(payload))
        return {"error": "unknown mode: %r" % mode}
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "wave.solve", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "wave.solve":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
