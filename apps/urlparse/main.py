#!/usr/bin/env python3
"""RFC-3986 URL/URI dissector via stdlib urllib.parse. Pure, offline."""
import json
import os
import socket
import sys
from urllib.parse import urlsplit, urlunsplit, parse_qsl

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


MAX_PARAMS = 1000  # listed query params; param_count/params_truncated carry the rest


def compute(payload):
    """PURE, offline, no I/O, never raises. Dissect an RFC-3986 URL/URI.

    Input: payload["url"] (non-empty str). Never fetches — parsing only.
    Output dict: scheme, host, port (explicit else the scheme default for
    http=80/https=443/ftp=21/ssh=22/ws=80/wss=443 else null), path, query,
    fragment, userinfo_present (bool), params ([{key,value}] from parse_qsl
    keep_blank_values=True), is_idn (host has non-ASCII), host_punycode
    (host.encode("idna") decoded, or null when N/A), normalized (scheme +
    host lowercased, default port dropped) and warnings ([...]). A URL with
    no scheme still parses (scheme=""). Empty/non-str url -> {"error": ...}."""
    try:
        if not isinstance(payload, dict):
            return {"error": "payload must be a mapping"}
        url = payload.get("url")
        if not isinstance(url, str):
            return {"error": "url must be a string"}
        if not url.strip():
            return {"error": "url must be non-empty"}

        sr = urlsplit(url)
        scheme = sr.scheme  # urlsplit lowercases the scheme
        host = sr.hostname or ""  # lowercased; brackets stripped for IPv6

        defaults = {"http": 80, "https": 443, "ftp": 21,
                    "ssh": 22, "ws": 80, "wss": 443}

        try:
            explicit_port = sr.port
        except ValueError:
            # The URL DOES carry an explicit port token, but it is non-numeric
            # or out of 0-65535. Reporting the scheme default here would be a
            # false answer (and "normalized" would silently rewrite the
            # authority to a different origin) — refuse honestly instead.
            return {"error": "invalid explicit port in URL authority (must be 0-65535)"}

        if explicit_port is not None:
            port = explicit_port
        else:
            port = defaults.get(scheme)  # None when scheme has no known default

        userinfo_present = sr.username is not None or sr.password is not None

        pairs = parse_qsl(sr.query, keep_blank_values=True)
        # Bounded params: an unbounded list can push the reply past the
        # daemon's 1 MiB app-line budget (measured ~1.44 MB at 40k pairs), so
        # the full count + a truncation flag carry the honest remainder.
        params = [{"key": k, "value": v} for k, v in pairs[:MAX_PARAMS]]

        is_idn = any(ord(c) > 127 for c in host)

        if host:
            try:
                host_punycode = host.encode("idna").decode("ascii")
            except Exception:  # noqa: BLE001 — idna is strict; N/A on failure
                host_punycode = None
        else:
            host_punycode = None

        # normalized: scheme + host lowercased (already lower), default port dropped
        netloc = ""
        if userinfo_present:
            userinfo = sr.username or ""
            if sr.password is not None:
                userinfo = userinfo + ":" + sr.password
            netloc += userinfo + "@"
        if host:
            netloc += ("[" + host + "]") if ":" in host else host
        if explicit_port is not None and explicit_port != defaults.get(scheme):
            netloc += ":" + str(explicit_port)
        normalized = urlunsplit((scheme, netloc, sr.path, sr.query, sr.fragment))

        warnings = []
        if userinfo_present:
            warnings.append("credentials embedded in URL")
        if scheme in ("http", "ws", "ftp"):
            warnings.append("insecure scheme (http/ws/ftp)")

        return {
            "scheme": scheme,
            "host": host,
            "port": port,
            "path": sr.path,
            "query": sr.query,
            "fragment": sr.fragment,
            "userinfo_present": userinfo_present,
            "params": params,
            "param_count": len(pairs),
            "params_truncated": len(pairs) > MAX_PARAMS,
            "is_idn": is_idn,
            "host_punycode": host_punycode,
            "normalized": normalized,
            "warnings": warnings,
        }
    except Exception as e:  # noqa: BLE001 — compute must never raise
        return {"error": "unexpected: %s" % e}


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "url.dissect", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "url.dissect":
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
