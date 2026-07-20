#!/usr/bin/env python3
"""Read-only digest panel: MD5/SHA1/SHA256 and byte length of input text via stdlib hashlib."""
import hashlib
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

    Reads payload["text"] (missing/non-string -> ""), encodes utf-8, and returns
    one-way digests plus the byte length. Digests are not secrets.
    """
    try:
        text = payload.get("text", "") if isinstance(payload, dict) else ""
    except Exception:  # noqa: BLE001 — never raise on hostile input
        text = ""
    if not isinstance(text, str):
        text = ""
    try:
        raw = text.encode("utf-8")
    except Exception:  # noqa: BLE001 — encoding must not raise (e.g. lone surrogates)
        raw = text.encode("utf-8", "replace")
    return {
        "length_bytes": len(raw),
        "md5": hashlib.md5(raw).hexdigest(),
        "sha1": hashlib.sha1(raw).hexdigest(),
        "sha256": hashlib.sha256(raw).hexdigest(),
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "hashkit.digest", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "hashkit.digest":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
