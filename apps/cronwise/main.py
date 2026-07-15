#!/usr/bin/env python3
"""Read-only cron explainer: parse a 5-field cron string and describe each field in plain English."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


# Per-field metadata: (label, unit_singular, min, max, names).
# names maps lowercased month/day-of-week abbreviations to their numeric value.
_MONTHS = {
    "jan": 1, "feb": 2, "mar": 3, "apr": 4, "may": 5, "jun": 6,
    "jul": 7, "aug": 8, "sep": 9, "oct": 10, "nov": 11, "dec": 12,
}
_DOW = {
    "sun": 0, "mon": 1, "tue": 2, "wed": 3, "thu": 4, "fri": 5, "sat": 6,
}
# 5 fields in cron order: minute, hour, day-of-month, month, day-of-week.
_FIELDS = [
    ("minute", "minute", 0, 59, {}),
    ("hour", "hour", 0, 23, {}),
    ("day_of_month", "day-of-month", 1, 31, {}),
    ("month", "month", 1, 12, _MONTHS),
    # Standard cron accepts day-of-week 0-7 where BOTH 0 and 7 mean Sunday.
    ("day_of_week", "day-of-week", 0, 7, _DOW),
]
_UNIT_PLURAL = {
    "minute": "minutes",
    "hour": "hours",
    "day-of-month": "days-of-month",
    "month": "months",
    "day-of-week": "days-of-week",
}
_MONTH_NAMES = {
    1: "January", 2: "February", 3: "March", 4: "April", 5: "May", 6: "June",
    7: "July", 8: "August", 9: "September", 10: "October", 11: "November", 12: "December",
}
_DOW_NAMES = {
    0: "Sunday", 1: "Monday", 2: "Tuesday", 3: "Wednesday",
    4: "Thursday", 5: "Friday", 6: "Saturday",
    7: "Sunday",  # cron's alternate Sunday (0 and 7 both denote Sunday)
}


def _resolve(token, names, lo, hi):
    """Resolve a single value token to an int within [lo, hi]. Raises ValueError on bad input."""
    key = token.strip().lower()
    if key in names:
        return names[key]
    val = int(token)  # raises ValueError on non-numeric
    if val < lo or val > hi:
        raise ValueError("out of range %d-%d" % (lo, hi))
    return val


def _pretty_value(unit, val):
    """Render a resolved numeric value using friendly names for months/days-of-week."""
    if unit == "month":
        return _MONTH_NAMES.get(val, str(val))
    if unit == "day-of-week":
        return _DOW_NAMES.get(val, str(val))
    return str(val)


def _describe_field(label, unit, lo, hi, names, raw):
    """Return a plain-English phrase for one cron field. Raises ValueError on invalid syntax."""
    raw = raw.strip()
    if raw == "":
        raise ValueError("empty field")
    plural = _UNIT_PLURAL[unit]

    # Comma list: describe each part and join.
    if "," in raw:
        parts = [p for p in raw.split(",")]
        phrases = [_describe_field(label, unit, lo, hi, names, p) for p in parts]
        return "; ".join(phrases)

    # Step form: base/step  (base may be "*", a value, or a range).
    if "/" in raw:
        base, _, step_s = raw.partition("/")
        base = base.strip()
        step = int(step_s)  # raises ValueError on non-numeric
        if step <= 0:
            raise ValueError("step must be positive")
        if base == "*":
            if step == 1:
                return "every %s" % unit
            return "every %d %s" % (step, plural)
        if "-" in base:
            a_s, _, b_s = base.partition("-")
            a = _resolve(a_s, names, lo, hi)
            b = _resolve(b_s, names, lo, hi)
            if a > b:
                raise ValueError("range start > end")
            return "every %d %s from %s through %s" % (
                step, plural, _pretty_value(unit, a), _pretty_value(unit, b))
        # base is a single value: e.g. 5/10 -> every 10 <unit> starting at <value>
        start = _resolve(base, names, lo, hi)
        return "every %d %s starting at %s %s" % (
            step, plural, unit, _pretty_value(unit, start))

    # Wildcard.
    if raw == "*":
        return "every %s" % unit

    # Range.
    if "-" in raw:
        a_s, _, b_s = raw.partition("-")
        a = _resolve(a_s, names, lo, hi)
        b = _resolve(b_s, names, lo, hi)
        if a > b:
            raise ValueError("range start > end")
        return "every %s from %s through %s" % (
            unit, _pretty_value(unit, a), _pretty_value(unit, b))

    # Single value.
    val = _resolve(raw, names, lo, hi)
    if unit == "minute":
        return "at minute %d" % val
    if unit == "hour":
        return "at hour %d" % val
    return "on %s %s" % (unit, _pretty_value(unit, val))


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["cron"] (a 5-field cron string like "*/5 * * * *"), splits on
    whitespace, and returns a per-field plain-English description plus a joined
    summary. If the string is not exactly 5 fields (or a field is invalid) it
    returns {"valid": False, "error": ...}.
    """
    try:
        cron = payload.get("cron", "") if isinstance(payload, dict) else ""
    except Exception:  # noqa: BLE001 — never raise on hostile input
        cron = ""
    if not isinstance(cron, str):
        return {"valid": False, "error": "cron must be a string"}

    fields = cron.split()
    if len(fields) != 5:
        return {
            "valid": False,
            "error": "expected 5 whitespace-separated fields, got %d" % len(fields),
        }

    out = {}
    phrases = []
    for (label, unit, lo, hi, names), raw in zip(_FIELDS, fields):
        try:
            phrase = _describe_field(label, unit, lo, hi, names, raw)
        except ValueError as e:
            return {
                "valid": False,
                "error": "invalid %s field %r: %s" % (label, raw, e),
            }
        except Exception as e:  # noqa: BLE001 — defensive; never raise
            return {
                "valid": False,
                "error": "invalid %s field %r: %s" % (label, raw, e),
            }
        out[label] = phrase
        phrases.append(phrase)

    out["valid"] = True
    out["summary"] = ", ".join(phrases)
    return out


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "cronwise.explain", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "cronwise.explain":
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
