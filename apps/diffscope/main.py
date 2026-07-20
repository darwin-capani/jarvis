#!/usr/bin/env python3
"""Read-only unified text diff: line-diff between two texts with added/removed counts. Pure, offline."""
import difflib
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
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
