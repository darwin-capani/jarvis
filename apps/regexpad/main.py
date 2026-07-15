#!/usr/bin/env python3
"""Read-only regex tester: test a pattern against text and report matches and capture groups. Pure, offline (re stdlib)."""
import json
import os
import re
import signal
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")

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


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


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
