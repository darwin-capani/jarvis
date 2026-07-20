#!/usr/bin/env python3
"""Read-only secret-strength estimator: charset size, length, Shannon entropy bits, and strength class."""
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


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["text"] (a candidate secret). Determines the charset size from
    the character classes present (lowercase 26, uppercase 26, digits 10,
    other/symbols 32) and estimates Shannon entropy as length * log2(charset).
    Returns only aggregate stats -- the input text is never echoed.
    """
    try:
        text = payload.get("text", "") if isinstance(payload, dict) else ""
    except Exception:  # noqa: BLE001 -- never raise on hostile input
        text = ""
    if not isinstance(text, str):
        text = ""

    has_lower = has_upper = has_digit = has_other = False
    for ch in text:
        if "a" <= ch <= "z":
            has_lower = True
        elif "A" <= ch <= "Z":
            has_upper = True
        elif "0" <= ch <= "9":
            has_digit = True
        else:
            has_other = True

    charset_size = 0
    if has_lower:
        charset_size += 26
    if has_upper:
        charset_size += 26
    if has_digit:
        charset_size += 10
    if has_other:
        charset_size += 32

    length = len(text)
    if length == 0 or charset_size == 0:
        bits = 0.0
    else:
        bits = round(length * math.log2(charset_size), 2)

    if bits < 28:
        strength = "very weak"
    elif bits < 36:
        strength = "weak"
    elif bits < 60:
        strength = "fair"
    elif bits < 128:
        strength = "strong"
    else:
        strength = "very strong"

    return {
        "length": length,
        "charset_size": charset_size,
        "bits": bits,
        "strength": strength,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "entropy.assess", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "entropy.assess":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
