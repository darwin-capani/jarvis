#!/usr/bin/env python3
"""Read-only regex tester: test a pattern against text and report matches and capture groups. Pure, offline (re stdlib)."""
import json
import os
import re
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")

_MATCH_CAP = 50


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def compute(payload):
    """PURE, offline, no I/O, never raises. Compile payload["pattern"] and finditer over payload["text"]."""
    if not isinstance(payload, dict):
        return {"error": "payload must be an object"}

    pattern = payload.get("pattern", "")
    if not isinstance(pattern, str):
        return {"error": "pattern must be a string"}

    text = payload.get("text", "")
    if not isinstance(text, str):
        return {"error": "text must be a string"}

    ignorecase = bool(payload.get("ignmatchcase", False))
    flags = re.IGNORECASE if ignorecase else 0

    try:
        rx = re.compile(pattern, flags)
    except re.error as e:
        return {"error": f"invalid pattern: {e}"}
    except Exception as e:  # noqa: BLE001 — defensive; compute never raises
        return {"error": f"invalid pattern: {e}"}

    matches = []
    count = 0
    try:
        for m in rx.finditer(text):
            count += 1
            if len(matches) < _MATCH_CAP:
                groups = [g if g is not None else None for g in m.groups()]
                matches.append({
                    "match": m.group(0),
                    "start": m.start(),
                    "end": m.end(),
                    "groups": groups,
                })
    except Exception as e:  # noqa: BLE001 — pathological patterns must not crash
        return {"error": f"match failed: {e}"}

    return {
        "count": count,
        "truncated": count > _MATCH_CAP,
        "ignorecase": ignorecase,
        "matches": matches,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "regexpad.test", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "regexpad.test":
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
