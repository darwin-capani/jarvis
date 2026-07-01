#!/usr/bin/env python3
"""Codeglass — read-only code-metrics panel for a pasted snippet (pure, offline)."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")

# Prefixes that mark a line as a comment (after stripping leading whitespace).
_COMMENT_PREFIXES = ("#", "//", "/*", "*")
# Case-sensitive markers that flag a work-item line.
_TODO_MARKERS = ("TODO", "FIXME", "XXX")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


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
        send(conn, {"type": "items", "data": compute(msg)})
    elif op == "stop":
        raise SystemExit(0)


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
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if not line.strip():
                continue
            try:
                handle(conn, json.loads(line))
            except SystemExit:
                return 0
            except Exception as e:  # noqa: BLE001 — a plugin never crashes the host
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
