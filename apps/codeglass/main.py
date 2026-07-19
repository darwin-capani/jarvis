#!/usr/bin/env python3
"""Codeglass — read-only code-metrics panel for a pasted snippet (pure, offline)."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")

# Prefixes that mark a line as a comment (after stripping leading whitespace).
_COMMENT_PREFIXES = ("#", "//", "/*", "*")
# Case-sensitive markers that flag a work-item line.
_TODO_MARKERS = ("TODO", "FIXME", "XXX")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def reply_result(conn, msg, data):
    """Answer one domain op, correlated when the host asked for correlation.

    THE AGENT-TOOL CONTRACT: a request carrying a non-empty string `id` (the
    daemon's request_op) is answered with a `type:"result"` line ECHOING that id
    so the host can route the payload back to the waiting caller. A request
    without an id (the voice router / legacy paths) keeps the uncorrelated
    `type:"items"` telemetry line — byte-identical to the pre-contract wire."""
    rid = msg.get("id")
    if isinstance(rid, str) and rid:
        send(conn, {"type": "result", "id": rid, "data": data})
    else:
        send(conn, {"type": "items", "data": data})


def compute(payload):
    """PURE, offline, no I/O, never raises. Compute line-metrics for a snippet.

    Reads payload["code"] (missing/non-string -> treated as ""). Returns a dict:
      lines            total number of lines
      blank_lines      lines that are empty or whitespace-only
      comment_lines    lines whose stripped form starts with #, //, /*, or *
      code_lines       lines - blank_lines - comment_lines
      longest_line_len length (in chars) of the longest line
      todo_count       lines containing TODO, FIXME, or XXX
    """
    try:
        code = payload.get("code", "") if isinstance(payload, dict) else ""
    except Exception:  # noqa: BLE001 — compute never raises
        code = ""
    if not isinstance(code, str):
        code = ""

    # splitlines() yields no element for "" (0 lines) and does not fabricate a
    # trailing empty line for a final newline, matching intuitive "line" counts.
    rows = code.splitlines()

    blank_lines = 0
    comment_lines = 0
    longest_line_len = 0
    todo_count = 0

    for row in rows:
        row_len = len(row)
        if row_len > longest_line_len:
            longest_line_len = row_len

        stripped = row.strip()
        if not stripped:
            blank_lines += 1
        elif stripped.startswith(_COMMENT_PREFIXES):
            comment_lines += 1

        if any(marker in row for marker in _TODO_MARKERS):
            todo_count += 1

    total = len(rows)
    code_lines = total - blank_lines - comment_lines

    return {
        "lines": total,
        "blank_lines": blank_lines,
        "comment_lines": comment_lines,
        "code_lines": code_lines,
        "longest_line_len": longest_line_len,
        "todo_count": todo_count,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "codeglass.metrics", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "codeglass.metrics":
        reply_result(conn, msg, compute(msg))
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
            except Exception as e:  # noqa: BLE001 — a plugin never crashes the host
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
        if overflowed:
            send(conn, {"type": "log", "data": {"line": f"input frame exceeded {MAX_FRAME_BYTES} bytes; dropped"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
