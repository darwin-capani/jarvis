#!/usr/bin/env python3
"""Read-only JSON path query: parse a JSON document and return the value at a dotted/bracket path. Pure, offline."""
import json
import os
import socket
import sys

TOKEN = os.environ.get("JARVIS_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("JARVIS_APP_SOCKET", "")


def send(conn, obj):
    obj["token"] = TOKEN
    conn.sendall((json.dumps(obj) + "\n").encode("utf-8"))


def _tokenize_path(path):
    """Split a path string into a list of ('key', name) / ('index', int) steps.

    Grammar (informal):
      path   := segment ( ('.' segment) | ('[' subscript ']') )*
      segment:= bare identifier chars (no '.', '[', ']')
      subscript := integer (possibly negative) | 'quoted key' | "quoted key"
    A leading '$' or '.' is tolerated and ignored. Returns a list of steps or
    raises ValueError on malformed syntax (caller converts to an {"error": ...}).
    """
    steps = []
    i = 0
    n = len(path)

    # Tolerate a leading root marker "$" and/or a leading dot.
    if i < n and path[i] == "$":
        i += 1
    if i < n and path[i] == ".":
        i += 1

    while i < n:
        ch = path[i]
        if ch == ".":
            # A dot must be followed by a key segment (not end, not another dot/bracket).
            i += 1
            if i >= n or path[i] in ".[]":
                raise ValueError("empty key after '.'")
            j = i
            while j < n and path[j] not in ".[]":
                j += 1
            steps.append(("key", path[i:j]))
            i = j
        elif ch == "[":
            if i + 1 < n and path[i + 1] in ("'", '"'):
                # Quoted key: it may contain ']' literally, so find the MATCHING
                # close quote first, then require the ']' immediately after it. A
                # naive find(']') would stop at a ']' inside the quotes and make a
                # key like 'a]b' unreachable.
                quote = path[i + 1]
                q = path.find(quote, i + 2)
                if q == -1:
                    raise ValueError("unterminated quoted key")
                if q + 1 >= n or path[q + 1] != "]":
                    raise ValueError("expected ']' after quoted key")
                steps.append(("key", path[i + 2:q]))
                i = q + 2
                continue
            # Unquoted subscript: an integer array index.
            close = path.find("]", i + 1)
            if close == -1:
                raise ValueError("unclosed '['")
            inner = path[i + 1:close]
            if inner == "":
                raise ValueError("empty subscript '[]'")
            stripped = inner.strip()
            try:
                steps.append(("index", int(stripped, 10)))
            except ValueError:
                raise ValueError("bad array index %r" % inner)
            i = close + 1
        elif ch == "]":
            raise ValueError("unexpected ']'")
        else:
            # A bare key segment at the start or right after a subscript.
            j = i
            while j < n and path[j] not in ".[]":
                j += 1
            steps.append(("key", path[i:j]))
            i = j

    return steps


def compute(payload):
    """PURE, offline, no I/O, never raises.

    Reads payload["json"] (a JSON document as a string) and payload["path"]
    (e.g. "a.b[0].c"). Parses the document, then walks the path: dotted keys
    index into objects, [n] indices into arrays (negative indices allowed),
    and ['key'] / ["key"] index objects with literal keys. Returns
    {"value": <json-able>} on success, or {"error": ...} for bad JSON, a
    malformed path, a missing key, an out-of-range/bad index, or an attempt to
    index into a non-container. An empty path returns the whole document.
    Never raises.
    """
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}

        doc_str = payload.get("json", "")
        if isinstance(doc_str, bool) or isinstance(doc_str, (int, float)):
            return {"error": "json must be a string"}
        if not isinstance(doc_str, str):
            return {"error": "json must be a string"}
        if doc_str.strip() == "":
            return {"error": "json is empty"}

        try:
            doc = json.loads(doc_str)
        except (ValueError, TypeError) as e:
            return {"error": "invalid json: %s" % e}

        path = payload.get("path", "")
        if isinstance(path, bool) or not isinstance(path, str):
            return {"error": "path must be a string"}

        try:
            steps = _tokenize_path(path)
        except ValueError as e:
            return {"error": "bad path: %s" % e}

        cur = doc
        walked = ""
        for kind, arg in steps:
            if kind == "key":
                if not isinstance(cur, dict):
                    return {"error": "cannot index non-object with key %r at %r"
                            % (arg, walked or "$")}
                if arg not in cur:
                    return {"error": "missing key %r at %r" % (arg, walked or "$")}
                cur = cur[arg]
                walked = "%s.%s" % (walked, arg) if walked else arg
            else:  # index
                if isinstance(cur, dict) or not isinstance(cur, list):
                    return {"error": "cannot index non-array with [%d] at %r"
                            % (arg, walked or "$")}
                length = len(cur)
                idx = arg
                if idx < 0:
                    idx += length
                if idx < 0 or idx >= length:
                    return {"error": "index %d out of range (len %d) at %r"
                            % (arg, length, walked or "$")}
                cur = cur[idx]
                walked = "%s[%d]" % (walked, arg) if walked else "[%d]" % arg

        # Cap list output to 50 items so a huge array can't bloat the response.
        # Report the true length and whether the value was truncated.
        result = {"value": cur, "type": type(cur).__name__}
        if isinstance(cur, list):
            full_len = len(cur)
            result["length"] = full_len
            if full_len > 50:
                result["value"] = cur[:50]
                result["truncated"] = True
        return result
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "jsonpath.query", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "jsonpath.query":
        send(conn, {"type": "items", "data": compute(msg)})
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
