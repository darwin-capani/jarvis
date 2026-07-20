#!/usr/bin/env python3
"""Ohm's law + power solver with SI-unit parsing: two knowns of V/I/R/P give the rest. Pure, offline."""
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

# Standard SI prefixes -> multiplier in base units. Case matters (m milli vs M mega).
_PREFIXES = {
    "p": 1e-12,
    "n": 1e-9,
    "u": 1e-6,
    "µ": 1e-6,  # MICRO SIGN
    "μ": 1e-6,  # GREEK SMALL LETTER MU
    "m": 1e-3,
    "k": 1e3,
    "M": 1e6,
    "G": 1e9,
}
_OHM_SYMBOLS = ("Ω", "Ω")  # GREEK CAPITAL OMEGA, OHM SIGN


def _parse_si(value):
    """Parse a quantity into base SI units. Accepts a finite number, or a string
    like '5V', '10mA', '2.2kohm', '2.2kΩ', '0.25W', '470uA': an optional trailing
    unit (V/A/W/ohm/Ω) then an optional prefix from {p,n,u,µ,m,k,M,G}. Raises
    ValueError on anything it cannot parse (caller wraps into an {"error": ...})."""
    if isinstance(value, bool):
        raise ValueError("boolean is not a numeric quantity")
    if isinstance(value, (int, float)):
        f = float(value)
        if not math.isfinite(f):
            raise ValueError("value is not finite")
        return f
    if not isinstance(value, str):
        raise ValueError("value must be a number or a string")
    s = value.strip()
    if not s:
        raise ValueError("empty string")
    # Strip a single trailing unit token, if present.
    if s.lower().endswith("ohm"):
        s = s[:-3]
    elif s.endswith(_OHM_SYMBOLS):
        s = s[:-1]
    elif s.endswith(("V", "A", "W")):
        s = s[:-1]
    s = s.strip()
    if not s:
        raise ValueError("no numeric part")
    # Strip an optional SI prefix (only ever a single leading-of-magnitude letter).
    mult = 1.0
    if s[-1] in _PREFIXES:
        mult = _PREFIXES[s[-1]]
        s = s[:-1].strip()
    if not s:
        raise ValueError("no numeric part before prefix")
    try:
        num = float(s)
    except (ValueError, TypeError):
        raise ValueError("could not parse number %r" % s)
    if not math.isfinite(num):
        raise ValueError("value is not finite")
    return num * mult


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
    """PURE, offline, no I/O, never raises.

    payload holds EXACTLY two of "voltage"/"current"/"resistance"/"power", each a
    number or an SI-unit string (see _parse_si). Solves the remaining two via
    R=V/I, V=I*R, I=V/R, P=V*I=I^2*R=V^2/R and returns floats in base units plus
    formulas_used. Fewer/more than two knowns, a divide-by-zero-forcing zero, or a
    negative resistance/power -> {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        fields = ("voltage", "current", "resistance", "power")
        provided = {}
        for f in fields:
            if f in payload and payload[f] is not None:
                try:
                    provided[f] = _parse_si(payload[f])
                except ValueError as e:
                    return {"error": "bad %s: %s" % (f, e)}

        if len(provided) != 2:
            return {"error": "need exactly two of V/I/R/P"}

        if "resistance" in provided and provided["resistance"] < 0:
            return {"error": "resistance must be non-negative"}
        if "power" in provided and provided["power"] < 0:
            return {"error": "power must be non-negative"}

        V = provided.get("voltage")
        I = provided.get("current")
        R = provided.get("resistance")
        P = provided.get("power")
        keys = frozenset(provided)
        formulas = []

        if keys == {"voltage", "current"}:
            if I == 0:
                return {"error": "current is zero: R = V/I divides by zero"}
            R = V / I
            formulas.append("R=V/I")
            P = V * I
            formulas.append("P=V*I")
        elif keys == {"voltage", "resistance"}:
            if R == 0:
                return {"error": "resistance is zero: I = V/R divides by zero"}
            I = V / R
            formulas.append("I=V/R")
            P = V * V / R
            formulas.append("P=V^2/R")
        elif keys == {"voltage", "power"}:
            if V == 0:
                return {"error": "voltage is zero: I = P/V divides by zero"}
            I = P / V
            formulas.append("I=P/V")
            if P == 0:
                return {"error": "power is zero: R = V^2/P divides by zero"}
            R = V * V / P
            formulas.append("R=V^2/P")
        elif keys == {"current", "resistance"}:
            V = I * R
            formulas.append("V=I*R")
            P = I * I * R
            formulas.append("P=I^2*R")
        elif keys == {"current", "power"}:
            if I == 0:
                return {"error": "current is zero: V = P/I divides by zero"}
            V = P / I
            formulas.append("V=P/I")
            R = P / (I * I)
            formulas.append("R=P/I^2")
        elif keys == {"resistance", "power"}:
            if R == 0:
                return {"error": "resistance is zero: I = sqrt(P/R) divides by zero"}
            I = math.sqrt(P / R)
            formulas.append("I=sqrt(P/R)")
            V = math.sqrt(P * R)
            formulas.append("V=sqrt(P*R)")
        else:  # pragma: no cover - the len==2 check makes this unreachable
            return {"error": "need exactly two of V/I/R/P"}

        # A resolved resistance or power that comes out negative is unphysical.
        if R is None or R < 0:
            return {"error": "resistance resolves negative"}
        if P is None or P < 0:
            return {"error": "power resolves negative"}

        return _finite_checked({
            "voltage": float(V),
            "current": float(I),
            "resistance": float(R),
            "power": float(P),
            "formulas_used": formulas,
        })
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "ohm.solve", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "ohm.solve":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
