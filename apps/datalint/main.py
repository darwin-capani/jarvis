#!/usr/bin/env python3
"""Read-only JSON inspector/validator: validity, root type, key/node counts, max depth. Pure, offline (json stdlib only)."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def _root_type(value):
    # bool must be checked before int: in Python bool is a subclass of int.
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "bool"
    if isinstance(value, dict):
        return "object"
    if isinstance(value, list):
        return "array"
    if isinstance(value, str):
        return "string"
    if isinstance(value, (int, float)):
        return "number"
    return "unknown"


def _count_and_depth(value):
    """Return (total_nodes, max_depth) for a decoded JSON value.

    total_nodes counts every value including the root and all nested values.
    max_depth is the deepest nesting of containers (root scalar -> 1).
    Uses an explicit stack so deeply nested input cannot blow the recursion limit.
    """
    total = 0
    max_depth = 0
    # stack holds (value, depth) pairs; root sits at depth 1.
    stack = [(value, 1)]
    while stack:
        node, depth = stack.pop()
        total += 1
        if depth > max_depth:
            max_depth = depth
        if isinstance(node, dict):
            for child in node.values():
                stack.append((child, depth + 1))
        elif isinstance(node, list):
            for child in node:
                stack.append((child, depth + 1))
    return total, max_depth


def compute(payload):
    """PURE, offline, no I/O, never raises. Inspect payload['json'] and report structure."""
    if not isinstance(payload, dict):
        return {"valid": False, "error": "payload must be an object"}
    raw = payload.get("json", "")
    if not isinstance(raw, str):
        raw = ""
    try:
        decoded = json.loads(raw)
    except Exception as e:  # noqa: BLE001 — surface parse failure as data, never raise
        return {"valid": False, "error": str(e)}

    root_type = _root_type(decoded)
    if isinstance(decoded, dict):
        top_level_keys = len(decoded)
    elif isinstance(decoded, list):
        top_level_keys = len(decoded)
    else:
        top_level_keys = 0

    total_nodes, max_depth = _count_and_depth(decoded)
    return {
        "valid": True,
        "root_type": root_type,
        "top_level_keys": top_level_keys,
        "total_nodes": total_nodes,
        "max_depth": max_depth,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "datalint.inspect", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "datalint.inspect":
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
            except Exception as e:  # noqa: BLE001 — a plugin never crashes the host
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
        if overflowed:
            send(conn, {"type": "log", "data": {"line": f"input frame exceeded {MAX_FRAME_BYTES} bytes; dropped"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
