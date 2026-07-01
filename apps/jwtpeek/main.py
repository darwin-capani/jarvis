#!/usr/bin/env python3
"""Read-only JWT inspector: base64url-decode a JWT header and payload for inspection. No signature verification. Pure, offline."""
import base64
import binascii
import json
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def _b64url_json(segment):
    """Decode one base64url JWT segment to a JSON value. Returns (value, error)."""
    # segment is guaranteed a non-empty str by the caller.
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

    Reads payload["token"] (string): a JWT of the form header.payload.signature.
    Splits on ".", requires exactly 3 parts, base64url-decodes and json.loads the
    header (part 0) and payload (part 1), and reports whether a signature is present.
    The signature is NEVER decoded or verified and NO secret is ever handled.
    Returns {"header", "payload", "signature_present"} or {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        token = payload.get("token", "")
        if isinstance(token, bool) or not isinstance(token, str):
            return {"error": "token must be a string"}
        token = token.strip()
        if not token:
            return {"error": "token is empty"}

        parts = token.split(".")
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
            except Exception as e:  # noqa: BLE001
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
