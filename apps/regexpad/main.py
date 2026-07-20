#!/usr/bin/env python3
"""Read-only regex tester: test a pattern against text and report matches and capture groups. Pure, offline (re stdlib)."""
import os
import re
import signal
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

_MATCH_CAP = 50
# ReDoS defense: a user-supplied pattern can catastrophically backtrack (e.g.
# "(a+)+b" on "aaaa…"), which Python's re cannot be asked to bound and which
# raises nothing — it just spins, hanging the app (and the daemon reading it).
# Bound the inputs AND hard-cap wall-clock via SIGALRM (CPython does check
# signals during a match, so this reliably interrupts a runaway pattern).
_MAX_PATTERN = 2000
_MAX_TEXT = 100_000
_MATCH_TIMEOUT_S = 1.0
_HAS_ALARM = hasattr(signal, "setitimer") and hasattr(signal, "SIGALRM")


class _MatchTimeout(Exception):
    pass


def _on_timeout(_signum, _frame):
    raise _MatchTimeout()


def compute(payload):
    """PURE, offline, no I/O, never raises. Compile payload["pattern"] and finditer over payload["text"]."""
    if not isinstance(payload, dict):
        return {"error": "payload must be an object"}

    pattern = payload.get("pattern", "")
    if not isinstance(pattern, str):
        return {"error": "pattern must be a string"}
    if len(pattern) > _MAX_PATTERN:
        return {"error": f"pattern too long (max {_MAX_PATTERN} chars)"}

    text = payload.get("text", "")
    if not isinstance(text, str):
        return {"error": "text must be a string"}
    if len(text) > _MAX_TEXT:
        return {"error": f"text too long (max {_MAX_TEXT} chars)"}

    ignorecase = bool(payload.get("ignmatchcase", False))
    flags = re.IGNORECASE if ignorecase else 0

    try:
        rx = re.compile(pattern, flags)
    except re.error as e:
        return {"error": f"invalid pattern: {e}"}
    except Exception as e:  # noqa: BLE001 — defensive; compute never raises
        return {"error": f"invalid pattern: {e}"}

    matches = []
    count = 0
    # Hard wall-clock cap around the match so a catastrophically-backtracking
    # pattern is interrupted (SIGALRM) instead of hanging the app + the daemon.
    prev = None
    if _HAS_ALARM:
        prev = signal.signal(signal.SIGALRM, _on_timeout)
        signal.setitimer(signal.ITIMER_REAL, _MATCH_TIMEOUT_S)
    try:
        for m in rx.finditer(text):
            count += 1
            if len(matches) < _MATCH_CAP:
                groups = [g if g is not None else None for g in m.groups()]
                matches.append({
                    "match": m.group(0),
                    "start": m.start(),
                    "end": m.end(),
                    "groups": groups,
                })
    except _MatchTimeout:
        return {"error": "match timed out — pattern too slow (possible catastrophic backtracking)"}
    except Exception as e:  # noqa: BLE001 — pathological patterns must not crash
        return {"error": f"match failed: {e}"}
    finally:
        if _HAS_ALARM:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, prev if prev is not None else signal.SIG_DFL)

    return {
        "count": count,
        "truncated": count > _MATCH_CAP,
        "ignorecase": ignorecase,
        "matches": matches,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "regexpad.test", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "regexpad.test":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
