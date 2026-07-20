#!/usr/bin/env python3
"""Read-only number-base converter: parse an integer in a source base, render it as bin/oct/dec/hex. Pure, offline."""
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


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["value"] (string) and payload["from_base"] (int, 2-36, default 10).
    Parses value in from_base via int(value, from_base) supporting an optional leading
    sign, then returns the number rendered in binary, octal, decimal, and hexadecimal.
    On any bad input returns {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        value = payload.get("value", "")
        # Accept ints/other scalars by stringifying; reject containers.
        if isinstance(value, bool) or isinstance(value, (list, dict, tuple, set)):
            return {"error": "value must be a string"}
        if not isinstance(value, str):
            value = str(value)
        value = value.strip()
        if not value:
            return {"error": "value is empty"}

        from_base = payload.get("from_base", 10)
        if isinstance(from_base, bool):
            return {"error": "from_base must be an integer 2-36"}
        if isinstance(from_base, str):
            try:
                from_base = int(from_base.strip(), 10)
            except (ValueError, TypeError):
                return {"error": "from_base must be an integer 2-36"}
        if not isinstance(from_base, int):
            return {"error": "from_base must be an integer 2-36"}
        if from_base < 2 or from_base > 36:
            return {"error": "from_base out of range (2-36)"}

        try:
            decimal = int(value, from_base)
        except (ValueError, TypeError):
            return {"error": "cannot parse value in base %d" % from_base}

        # Render without Python's 0b/0o/0x prefixes; preserve sign for negatives.
        sign = "-" if decimal < 0 else ""
        mag = abs(decimal)
        return {
            "decimal": decimal,
            "binary": sign + format(mag, "b"),
            "octal": sign + format(mag, "o"),
            "hex": sign + format(mag, "x"),
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "numbase.convert", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "numbase.convert":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
