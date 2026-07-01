#!/usr/bin/env python3
"""Read-only Markdown outline extractor: pull the ATX heading structure (table of contents). Pure, offline."""
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

    Reads payload["markdown"] (string). Scans line-by-line for ATX headings
    matching ^#{1,6}\\s+ (one to six leading '#', then required whitespace, then
    text). For each heading records {"level": count of '#', "text": stripped
    heading text}. Lines inside fenced code blocks are skipped: a line whose
    stripped content starts with ``` toggles the fenced state (and is never
    itself treated as a heading). Trailing '#' characters common to closed ATX
    headings (e.g. '## Title ##') are stripped from the text. Returns
    {"outline": [...] capped at 50, "count": total headings found}. On any bad
    input returns {"error": ...}. Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        markdown = payload.get("markdown", "")
        if isinstance(markdown, bool) or isinstance(markdown, (list, dict, tuple, set)):
            return {"error": "markdown must be a string"}
        if not isinstance(markdown, str):
            markdown = str(markdown)

        outline = []
        count = 0
        in_fence = False
        # Splitlines handles \n, \r\n, and lone \r uniformly and drops the terminator.
        for raw in markdown.splitlines():
            stripped = raw.strip()
            # A ``` (or longer) fence marker toggles code-block state; it is
            # never a heading, and headings inside a fence are ignored.
            if stripped.startswith("```") or stripped.startswith("~~~"):
                in_fence = not in_fence
                continue
            if in_fence:
                continue

            # Detect ATX heading: leading '#'s (1-6) followed by whitespace.
            hashes = 0
            for ch in raw:
                if ch == "#":
                    hashes += 1
                else:
                    break
            if hashes < 1 or hashes > 6:
                continue
            rest = raw[hashes:]
            # Require at least one whitespace char after the '#' run.
            if not rest or rest[0] not in (" ", "\t"):
                continue

            text = rest.strip()
            # Strip a trailing run of '#' (closed ATX form) plus surrounding space.
            if text:
                end = len(text)
                while end > 0 and text[end - 1] == "#":
                    end -= 1
                # Only treat trailing '#'s as a closer if separated by whitespace
                # (or the whole remainder was hashes), matching CommonMark.
                if end < len(text) and (end == 0 or text[end - 1] in (" ", "\t")):
                    text = text[:end].strip()

            count += 1
            if len(outline) < 50:
                outline.append({"level": hashes, "text": text})

        return {"outline": outline, "count": count}
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "markmap.outline", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "markmap.outline":
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
