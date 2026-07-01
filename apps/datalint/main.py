#!/usr/bin/env python3
"""Read-only JSON inspector/validator: validity, root type, key/node counts, max depth. Pure, offline (json stdlib only)."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")


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
