#!/usr/bin/env python3
"""Read-only JWT inspector: base64url-decode a JWT header and payload for inspection. No signature verification. Pure, offline."""
import base64
import binascii
import json
import os
import re
import socket
import sys

# The strict base64url alphabet (RFC 4648 §5): letters, digits, '-' and '_'.
# A JWT segment carries NO '=' padding, so anything else — standard-base64
# '+'/'/', whitespace, or junk — means the segment is not valid base64url.
_B64URL_RE = re.compile(r"[A-Za-z0-9_-]+\Z")

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")


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
