#!/usr/bin/env python3
"""Dimensional-analysis unit converter across 9 categories plus affine temperature. Pure, offline."""
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


# Each unit maps to its factor in the category's base unit; result = value * f[from] / f[to].
# Keys are matched case-SENSITIVELY (kB vs KB vs Kbit are distinct); an unmatched key is an error.
_CATEGORIES = {
    "length": {  # base metre
        "m": 1.0, "km": 1000.0, "cm": 0.01, "mm": 0.001,
        "um": 1e-6, "nm": 1e-9,
        "mi": 1609.344, "yd": 0.9144, "ft": 0.3048, "in": 0.0254, "nmi": 1852.0,
    },
    "mass": {  # base kilogram
        "kg": 1.0, "g": 0.001, "mg": 1e-6, "ug": 1e-9, "t": 1000.0,
        "lb": 0.45359237, "oz": 0.028349523125,
    },
    "time": {  # base second
        "s": 1.0, "ms": 0.001, "us": 1e-6, "ns": 1e-9,
        "min": 60.0, "h": 3600.0, "day": 86400.0, "week": 604800.0,
    },
    "data": {  # base byte
        "B": 1.0, "KB": 1e3, "MB": 1e6, "GB": 1e9, "TB": 1e12, "PB": 1e15,
        "KiB": 1024.0, "MiB": 1024.0 ** 2, "GiB": 1024.0 ** 3, "TiB": 1024.0 ** 4,
        "bit": 0.125, "Kbit": 125.0, "Mbit": 125000.0, "Gbit": 125000000.0,
    },
    "pressure": {  # base pascal
        "Pa": 1.0, "kPa": 1000.0, "MPa": 1e6, "bar": 100000.0, "mbar": 100.0,
        "psi": 6894.757293168361, "atm": 101325.0,
        "mmHg": 133.322387415, "Torr": 101325.0 / 760.0,
    },
    "energy": {  # base joule
        "J": 1.0, "kJ": 1000.0, "MJ": 1e6,
        "cal": 4.184, "kcal": 4184.0, "Wh": 3600.0, "kWh": 3.6e6, "eV": 1.602176634e-19,
    },
    "power": {  # base watt
        "W": 1.0, "kW": 1000.0, "MW": 1e6, "mW": 0.001, "hp": 745.6998715822702,
    },
    "force": {  # base newton
        "N": 1.0, "kN": 1000.0, "mN": 0.001, "lbf": 4.4482216152605, "dyn": 1e-5,
    },
    "angle": {  # base radian
        "rad": 1.0, "deg": math.pi / 180.0, "grad": math.pi / 200.0,
        "arcmin": math.pi / (180.0 * 60.0), "arcsec": math.pi / (180.0 * 3600.0),
    },
}
_TEMP_UNITS = ("C", "F", "K")


def _category_of(unit):
    if unit in _TEMP_UNITS:
        return "temperature"
    for cat, table in _CATEGORIES.items():
        if unit in table:
            return cat
    return None


def _temp_to_kelvin(value, unit):
    if unit == "C":
        return value + 273.15
    if unit == "F":
        return (value - 32.0) * 5.0 / 9.0 + 273.15
    return value  # K


def _kelvin_to(value_k, unit):
    if unit == "C":
        return value_k - 273.15
    if unit == "F":
        return (value_k - 273.15) * 9.0 / 5.0 + 32.0
    return value_k  # K


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Input: {"value": number, "from": unit str, "to": unit str}. Converts within a single
    dimensional category (length/mass/time/data/pressure/energy/power/force/angle) via base-unit
    factors, or across temperature (C/F/K) via an affine trip through kelvin. Unit matching is
    case-sensitive. Output: {value, from, to, result (float), category}. Errors (as dicts, no raise):
    non-mapping payload, missing/non-number value, non-string units, unknown unit, or a from/to pair
    from different categories (incompatible units).
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}
        if "value" not in payload:
            return {"error": "missing 'value'"}
        value = payload["value"]
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return {"error": "value must be a number"}
        if isinstance(value, float) and not math.isfinite(value):
            return {"error": "value must be a finite number"}
        frm = payload.get("from")
        to = payload.get("to")
        if not isinstance(frm, str) or not isinstance(to, str):
            return {"error": "from and to must be strings"}
        cat_from = _category_of(frm)
        if cat_from is None:
            return {"error": "unknown unit: %s" % frm}
        cat_to = _category_of(to)
        if cat_to is None:
            return {"error": "unknown unit: %s" % to}
        if cat_from != cat_to:
            return {"error": "incompatible units: %s vs %s" % (frm, to)}
        category = cat_from
        if category == "temperature":
            result = _kelvin_to(_temp_to_kelvin(value, frm), to)
        else:
            table = _CATEGORIES[category]
            result = value * table[frm] / table[to]
        return {
            "value": value,
            "from": frm,
            "to": to,
            "result": float(result),
            "category": category,
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "unit.convert", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "unit.convert":
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
