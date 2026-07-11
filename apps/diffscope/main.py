#!/usr/bin/env python3
"""Read-only unified text diff: line-diff between two texts with added/removed counts. Pure, offline."""
import difflib
import json
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["a"] and payload["b"] (strings; missing -> "").
    Computes a unified line-diff via difflib.unified_diff on a.splitlines()
    and b.splitlines() (lineterm=""), then returns:
      {"diff": "\\n".join of the diff lines capped at 200 lines,
       "added": count of inserted content lines,
       "removed": count of deleted content lines}.
    The two file-header lines ("--- "/"+++ ") and the "@@" hunk headers are
    excluded from the counts by POSITION, not prefix: a content line whose data
    starts with "++"/"--" (e.g. C's "++i", a "--flag" doc, a YAML "---") is
    emitted as "+++i"/"---x" and must not be mistaken for a file header. On any
    bad input returns {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        def as_text(key):
            val = payload.get(key, "")
            if val is None:
                return ""
            # Reject containers; stringify plain scalars for robustness.
            if isinstance(val, (list, dict, tuple, set)):
                raise ValueError("%s must be a string" % key)
            if isinstance(val, str):
                return val
            return str(val)

        try:
            a = as_text("a")
            b = as_text("b")
        except ValueError as ve:
            return {"error": str(ve)}

        a_lines = a.splitlines()
        b_lines = b.splitlines()

        lines = list(difflib.unified_diff(a_lines, b_lines, lineterm=""))

        added = 0
        removed = 0
        for idx, line in enumerate(lines):
            # unified_diff emits the two file-header lines ("--- "/"+++ ") first,
            # then "@@" hunk headers, then content lines. Classify content by
            # POSITION so a line whose data starts with "++"/"--" (emitted as
            # "+++i"/"---x") is counted, not mistaken for a file header.
            if idx < 2 or line.startswith("@@"):
                continue
            if line.startswith("+"):
                added += 1
            elif line.startswith("-"):
                removed += 1

        capped = lines[:200]
        return {
            "diff": "\n".join(capped),
            "added": added,
            "removed": removed,
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "diffscope.unified", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "diffscope.unified":
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
