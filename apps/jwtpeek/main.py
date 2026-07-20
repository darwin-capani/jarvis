#!/usr/bin/env python3
"""Read-only JWT inspector: base64url-decode a JWT header and payload for inspection. No signature verification. Pure, offline."""
import base64
import binascii
import json
import os
import re
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

# The strict base64url alphabet (RFC 4648 §5): letters, digits, '-' and '_'.
# A JWT segment carries NO '=' padding, so anything else — standard-base64
# '+'/'/', whitespace, or junk — means the segment is not valid base64url.
_B64URL_RE = re.compile(r"[A-Za-z0-9_-]+\Z")


def _b64url_json(segment):
    """Decode one base64url JWT segment to a JSON value. Returns (value, error)."""
    # segment is guaranteed a non-empty str by the caller.
    # Reject anything outside the base64url alphabet up front: urlsafe_b64decode
    # with the default validate=False silently tolerates standard-base64 '+'/'/'
    # (and discards other stray bytes), which would let a NON-base64url token
    # decode and be reported as valid. An explicit alphabet check is required.
    if _B64URL_RE.match(segment) is None:
        return None, "not valid base64url"
    # base64url uses '-'/'_' and omits '=' padding; restore padding to a multiple of 4.
    padding = (-len(segment)) % 4
    padded = segment + ("=" * padding)
    try:
        raw = base64.urlsafe_b64decode(padded.encode("ascii"))
    except (binascii.Error, ValueError, UnicodeEncodeError):
        return None, "not valid base64url"
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        return None, "decoded bytes are not UTF-8"
    try:
        return json.loads(text), None
    except (ValueError, TypeError):
        return None, "decoded content is not JSON"


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["jwt"] (string): a JWT of the form header.payload.signature.
    Splits on ".", requires exactly 3 parts, base64url-decodes and json.loads the
    header (part 0) and payload (part 1), and reports whether a signature is present.
    The signature is NEVER decoded or verified and NO secret is ever handled.
    Returns {"header", "payload", "signature_present"} or {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        # The agent-tool contract delivers the JWT under the non-reserved param
        # name "jwt"; the wire envelope reserves "token" (plugin_sdk
        # RESERVED_PARAM_NAMES), so it can never be a declared param — accept it
        # only as a legacy fallback for any pre-contract caller.
        jwt = payload["jwt"] if "jwt" in payload else payload.get("token", "")
        if isinstance(jwt, bool) or not isinstance(jwt, str):
            return {"error": "jwt must be a string"}
        jwt = jwt.strip()
        if not jwt:
            return {"error": "jwt is empty"}

        parts = jwt.split(".")
        if len(parts) != 3:
            return {"error": "malformed JWT: expected 3 dot-separated parts, got %d" % len(parts)}

        header_seg, payload_seg, signature_seg = parts
        if not header_seg or not payload_seg:
            return {"error": "malformed JWT: empty header or payload segment"}

        header, err = _b64url_json(header_seg)
        if err is not None:
            return {"error": "header: %s" % err}

        claims, err = _b64url_json(payload_seg)
        if err is not None:
            return {"error": "payload: %s" % err}

        return {
            "header": header,
            "payload": claims,
            "signature_present": bool(signature_seg),
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "jwtpeek.decode", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "jwtpeek.decode":
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
