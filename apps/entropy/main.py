#!/usr/bin/env python3
"""Read-only secret-strength estimator: charset size, length, Shannon entropy bits, and strength class."""
import json
import math
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["text"] (a candidate secret). Determines the charset size from
    the character classes present (lowercase 26, uppercase 26, digits 10,
    other/symbols 32) and estimates Shannon entropy as length * log2(charset).
    Returns only aggregate stats -- the input text is never echoed.
    """
    try:
        text = payload.get("text", "") if isinstance(payload, dict) else ""
    except Exception:  # noqa: BLE001 -- never raise on hostile input
        text = ""
    if not isinstance(text, str):
        text = ""

    has_lower = has_upper = has_digit = has_other = False
    for ch in text:
        if "a" <= ch <= "z":
            has_lower = True
        elif "A" <= ch <= "Z":
            has_upper = True
        elif "0" <= ch <= "9":
            has_digit = True
        else:
            has_other = True

    charset_size = 0
    if has_lower:
        charset_size += 26
    if has_upper:
        charset_size += 26
    if has_digit:
        charset_size += 10
    if has_other:
        charset_size += 32

    length = len(text)
    if length == 0 or charset_size == 0:
        bits = 0.0
    else:
        bits = round(length * math.log2(charset_size), 2)

    if bits < 28:
        strength = "very weak"
    elif bits < 36:
        strength = "weak"
    elif bits < 60:
        strength = "fair"
    elif bits < 128:
        strength = "strong"
    else:
        strength = "very strong"

    return {
        "length": length,
        "charset_size": charset_size,
        "bits": bits,
        "strength": strength,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "entropy.assess", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "entropy.assess":
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
