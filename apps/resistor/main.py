#!/usr/bin/env python3
"""Resistor color-band decoder and E-series (E24/E96) resolver. Pure, offline."""
import json
import math
import os
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


# --- E-series standard values, normalized to one decade (integers). ---
E24 = [10, 11, 12, 13, 15, 16, 18, 20, 22, 24, 27, 30,
       33, 36, 39, 43, 47, 51, 56, 62, 68, 75, 82, 91]

E96 = [100, 102, 105, 107, 110, 113, 115, 118, 121, 124, 127, 130,
       133, 137, 140, 143, 147, 150, 154, 158, 162, 165, 169, 174,
       178, 182, 187, 191, 196, 200, 205, 210, 215, 221, 226, 232,
       237, 243, 249, 255, 261, 267, 274, 280, 287, 294, 301, 309,
       316, 324, 332, 340, 348, 357, 365, 374, 383, 392, 402, 412,
       422, 432, 442, 453, 464, 475, 487, 499, 511, 523, 536, 549,
       562, 576, 590, 604, 619, 634, 649, 665, 681, 698, 715, 732,
       750, 768, 787, 806, 825, 845, 866, 887, 909, 931, 953, 976]


def _fmt(x):
    """Render a number with trailing zeros trimmed (pure)."""
    return "%.6g" % x


def _clean(x):
    """Round a float to 12 significant figures to kill IEEE noise (pure)."""
    if x == 0:
        return 0.0
    d = 11 - int(math.floor(math.log10(abs(x))))
    return float(round(x, d))


def _display(ohms):
    """Human-friendly resistance string, e.g. '4.7 kΩ', '330 Ω', '1 MΩ' (pure)."""
    for scale, unit in ((1e9, "GΩ"), (1e6, "MΩ"), (1e3, "kΩ")):
        if ohms >= scale:
            return "%s %s" % (_fmt(ohms / scale), unit)
    return "%s Ω" % _fmt(ohms)


def _nearest(target, series):
    """Return (value_int, exponent) of the series entry closest to target in log space (pure)."""
    lt = math.log10(target)
    base = int(math.floor(lt))
    best_d = None
    bv = None
    bexp = None
    for e in range(base - 4, base + 5):
        for v in series:
            d = abs(math.log10(v) + e - lt)
            if best_d is None or d < best_d:
                best_d = d
                bv = v
                bexp = e
    return bv, bexp


def compute(payload):
    """PURE, offline, no I/O, never raises.

    payload["bands"]: list of 3/4/5/6 color-name strings ->
        {ohms, display, tolerance, temp_coefficient_ppm_k}.
      3-band = 2 digits + multiplier (default tolerance ±20%);
      4-band = 2 digits + multiplier + tolerance;
      5-band = 3 digits + multiplier + tolerance;
      6-band = 5-band + a temperature-coefficient band (ppm/K).
    payload["ohms"]: a positive number ->
        {input_ohms, nearest_e24, nearest_e96, e24_bands (4-band color list)}.
    Unknown color / bad length / non-positive ohms -> {"error": ...}.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        DIGIT = {"black": 0, "brown": 1, "red": 2, "orange": 3, "yellow": 4,
                 "green": 5, "blue": 6, "violet": 7, "grey": 8, "gray": 8,
                 "white": 9}
        MULT = dict(DIGIT)
        MULT["gold"] = -1
        MULT["silver"] = -2
        TOL = {"brown": 1, "red": 2, "green": 0.5, "blue": 0.25, "violet": 0.1,
               "grey": 0.05, "gray": 0.05, "gold": 5, "silver": 10}
        TEMPCO = {"brown": 100, "red": 50, "orange": 15, "yellow": 25,
                  "blue": 10, "violet": 5}
        DIGIT_REV = {0: "black", 1: "brown", 2: "red", 3: "orange", 4: "yellow",
                     5: "green", 6: "blue", 7: "violet", 8: "grey", 9: "white"}
        MULT_REV = dict(DIGIT_REV)
        MULT_REV[-1] = "gold"
        MULT_REV[-2] = "silver"

        def norm(c):
            return c.strip().lower() if isinstance(c, str) else None

        if "bands" in payload:
            bands = payload["bands"]
            if not isinstance(bands, list):
                return {"error": "'bands' must be a list of color names"}
            n = len(bands)
            if n not in (3, 4, 5, 6):
                return {"error": "bands length must be 3, 4, 5, or 6 (got %d)" % n}
            cols = [norm(c) for c in bands]

            ndig = 2 if n in (3, 4) else 3
            digits = 0
            for i in range(ndig):
                c = cols[i]
                if c is None or c not in DIGIT:
                    return {"error": "unknown digit color: %r" % (bands[i],)}
                digits = digits * 10 + DIGIT[c]

            cm = cols[ndig]
            if cm is None or cm not in MULT:
                return {"error": "unknown multiplier color: %r" % (bands[ndig],)}
            exp = MULT[cm]

            if n == 3:
                tol = 20.0
            else:
                ti = ndig + 1
                ct = cols[ti]
                if ct is None or ct not in TOL:
                    return {"error": "unknown tolerance color: %r" % (bands[ti],)}
                tol = TOL[ct]

            tempco = None
            if n == 6:
                cc = cols[5]
                if cc is None or cc not in TEMPCO:
                    return {"error": "unknown temp-coefficient color: %r" % (bands[5],)}
                tempco = TEMPCO[cc]

            if exp >= 0:
                ohms = float(digits * (10 ** exp))
            else:
                ohms = digits / float(10 ** (-exp))
            ohms = _clean(ohms)

            return {
                "ohms": ohms,
                "display": _display(ohms),
                "tolerance": "±%s%%" % _fmt(tol),
                "temp_coefficient_ppm_k": tempco,
            }

        if "ohms" in payload:
            r = payload["ohms"]
            if isinstance(r, bool) or not isinstance(r, (int, float)):
                return {"error": "'ohms' must be a positive number"}
            if not math.isfinite(r) or not (r > 0):
                return {"error": "'ohms' must be a positive number"}

            v24, e24exp = _nearest(r, E24)
            v96, e96exp = _nearest(r, E96)
            cm = MULT_REV.get(e24exp)
            if cm is None:
                return {"error": "ohms out of standard 4-band color range"}
            e24_bands = [DIGIT_REV[v24 // 10], DIGIT_REV[v24 % 10], cm, "gold"]

            return {
                "input_ohms": float(r),
                "nearest_e24": _clean(v24 * (10.0 ** e24exp)),
                "nearest_e96": _clean(v96 * (10.0 ** e96exp)),
                "e24_bands": e24_bands,
            }

        return {"error": "provide 'bands' (list) or 'ohms' (number)"}
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "resistor.decode", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "resistor.decode":
        send(conn, {"type": "items", "data": compute(msg)})
    elif op == "stop":
        raise SystemExit(0)


MAX_FRAME_BYTES = 8 * 1024 * 1024  # cap on one un-newlined frame from the daemon


def drain_lines(buf, max_frame=MAX_FRAME_BYTES):
    """PURE framing: split every complete newline-terminated line out of buf.

    Returns (lines, remaining, overflowed): the complete lines with their trailing
    newline stripped in arrival order, the leftover partial buffer, and whether
    that leftover grew past max_frame WITHOUT a newline. When it has, the leftover
    is DROPPED (returned as b"") so a peer streaming an unframed, unbounded blob
    can't grow the read buffer without bound (OOM) — the daemon side is already
    bounded (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES). Newline
    framing is otherwise identical to buf.split(b"\\n", 1). Never raises."""
    lines = []
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        lines.append(line)
    overflowed = len(buf) > max_frame
    if overflowed:
        buf = b""
    return lines, buf, overflowed


def main():
    if not TOKEN or not SOCKET_PATH:
        print("missing DARWIN_APP_TOKEN / DARWIN_APP_SOCKET; not launched by darwind", file=sys.stderr)
        return 1
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.connect(SOCKET_PATH)
    buf = b""
    while True:
        chunk = conn.recv(4096)
        if not chunk:
            break
        buf += chunk
        lines, buf, overflowed = drain_lines(buf)
        for line in lines:
            if not line.strip():
                continue
            try:
                handle(conn, json.loads(line))
            except SystemExit:
                return 0
            except Exception as e:  # noqa: BLE001
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
        if overflowed:
            send(conn, {"type": "log", "data": {"line": f"input frame exceeded {MAX_FRAME_BYTES} bytes; dropped"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
