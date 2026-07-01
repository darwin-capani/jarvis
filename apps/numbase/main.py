#!/usr/bin/env python3
"""Read-only number-base converter: parse an integer in a source base, render it as bin/oct/dec/hex. Pure, offline."""
import json
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

    Reads payload["value"] (string) and payload["from_base"] (int, 2-36, default 10).
    Parses value in from_base via int(value, from_base) supporting an optional leading
    sign, then returns the number rendered in binary, octal, decimal, and hexadecimal.
    On any bad input returns {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        value = payload.get("value", "")
        # Accept ints/other scalars by stringifying; reject containers.
        if isinstance(value, bool) or isinstance(value, (list, dict, tuple, set)):
            return {"error": "value must be a string"}
        if not isinstance(value, str):
            value = str(value)
        value = value.strip()
        if not value:
            return {"error": "value is empty"}

        from_base = payload.get("from_base", 10)
        if isinstance(from_base, bool):
            return {"error": "from_base must be an integer 2-36"}
        if isinstance(from_base, str):
            try:
                from_base = int(from_base.strip(), 10)
            except (ValueError, TypeError):
                return {"error": "from_base must be an integer 2-36"}
        if not isinstance(from_base, int):
            return {"error": "from_base must be an integer 2-36"}
        if from_base < 2 or from_base > 36:
            return {"error": "from_base out of range (2-36)"}

        try:
            decimal = int(value, from_base)
        except (ValueError, TypeError):
            return {"error": "cannot parse value in base %d" % from_base}

        # Render without Python's 0b/0o/0x prefixes; preserve sign for negatives.
        sign = "-" if decimal < 0 else ""
        mag = abs(decimal)
        return {
            "decimal": decimal,
            "binary": sign + format(mag, "b"),
            "octal": sign + format(mag, "o"),
            "hex": sign + format(mag, "x"),
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "numbase.convert", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "numbase.convert":
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
