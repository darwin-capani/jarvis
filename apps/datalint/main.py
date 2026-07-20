#!/usr/bin/env python3
"""Read-only JSON inspector/validator: validity, root type, key/node counts, max depth. Pure, offline (json stdlib only)."""
import json
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
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
