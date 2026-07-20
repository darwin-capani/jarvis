#!/usr/bin/env python3
"""Read-only epoch converter: turn a Unix timestamp into a UTC ISO-8601 string and its calendar components. Pure, offline."""
import datetime
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

_WEEKDAYS = ("Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun")


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
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
