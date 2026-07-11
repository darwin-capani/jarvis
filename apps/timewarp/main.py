#!/usr/bin/env python3
"""Read-only epoch converter: turn a Unix timestamp into a UTC ISO-8601 string and its calendar components. Pure, offline."""
import datetime
import json
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")

_WEEKDAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["epoch"] (int or numeric string; seconds since 1970 UTC), converts it
    to a timezone-aware UTC datetime via datetime.datetime.fromtimestamp(epoch, tz=utc),
    and returns {iso_utc, year, month, day, hour, minute, second, weekday (Mon..Sun)}.
    On non-numeric or out-of-range input (guarding OverflowError/OSError/ValueError)
    returns {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        raw = payload.get("epoch")
        # Reject booleans (bool is an int subclass) and containers outright.
        if isinstance(raw, bool):
            return {"error": "epoch must be a number"}
        if isinstance(raw, (list, dict, tuple, set)):
            return {"error": "epoch must be a number"}
        if raw is None:
            return {"error": "epoch is required"}

        # Accept int/float directly; parse strings as int or float.
        if isinstance(raw, (int, float)):
            epoch = raw
        elif isinstance(raw, str):
            text = raw.strip()
            if not text:
                return {"error": "epoch is empty"}
            try:
                epoch = int(text, 10)
            except ValueError:
                try:
                    epoch = float(text)
                except ValueError:
                    return {"error": "epoch is not numeric"}
        else:
            return {"error": "epoch must be a number"}

        # Guard NaN/inf which pass isinstance(float) but break fromtimestamp.
        if isinstance(epoch, float) and epoch != epoch:
            return {"error": "epoch is not a finite number"}
        if epoch in (float("inf"), float("-inf")):
            return {"error": "epoch is not a finite number"}

        try:
            dt = datetime.datetime.fromtimestamp(epoch, tz=datetime.timezone.utc)
        except (OverflowError, OSError, ValueError):
            return {"error": "epoch out of representable range"}

        return {
            "iso_utc": dt.isoformat(),
            "year": dt.year,
            "month": dt.month,
            "day": dt.day,
            "hour": dt.hour,
            "minute": dt.minute,
            "second": dt.second,
            "weekday": _WEEKDAYS[dt.weekday()],
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "timewarp.convert", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "timewarp.convert":
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
        print("missing JARVIS_APP_TOKEN / JARVIS_APP_SOCKET; not launched by jarvisd", file=sys.stderr)
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
