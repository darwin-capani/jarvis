#!/usr/bin/env python3
"""Read-only JSON path query: parse a JSON document and return the value at a dotted/bracket path. Pure, offline."""
import json
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
        reply_result(conn, msg, compute(msg))
    elif op == "stop":
        raise SystemExit(0)


if __name__ == "__main__":
    sys.exit(run(handle))
