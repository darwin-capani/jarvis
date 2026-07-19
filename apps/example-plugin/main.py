#!/usr/bin/env python3
"""Example-Plugin — the #36 PLUGIN SDK reference handler.

A minimal micro-app illustrating the capability-module contract. It runs under
the daemon-generated default-deny seatbelt profile (docs/SANDBOX.md), connects
to its own per-app JSONL socket, and includes its per-launch capability token on
EVERY line — exactly like global-scan. It SERVES the READ-ONLY tool its manifest
declares:

  - example.read_status : reports a tiny status object (no side effect).

THE AGENT-TOOL CONTRACT (the canonical reference): a non-consequential
[[tools.exposes]] declaration is offered to the agent loop as an invocable
app__<tool> def, so a declared tool MUST be served by handle() — and a request
carrying an `id` MUST be answered with a `type:"result"` line echoing that id
(see reply_result below) so the daemon's request_op can route the payload back
to the caller. A declared-but-unserved tool would be offered to the model and
time out; declaration-only entries are no longer inert documentation.

This handler is intentionally tiny: the SDK's value is the VALIDATED CONTRACT
(daemon/src/plugin_sdk.rs) and the sandbox, not the handler. It has NO
consequential surface — nothing it can do reaches the confirmation gate, by
construction. The daemon never spawns it in tests; this file is the live runtime
the seatbelt profile launches.
"""
import json
import os
import socket
import sys

TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")

# Optional, READ-ONLY dyld module self-report (docs/INTROSPECT.md). darwind runs
# the app with the project root as CWD, and the manifest grants fs_read of
# apps/_sdk, so the shared reference stub is importable. Bytecode writes are
# disabled (no fs_write there) and every step is guarded — if the stub is absent
# or import fails, the plugin runs exactly as before, just without attestation.
dyld_report = None
try:
    sys.dont_write_bytecode = True
    sys.path.insert(0, os.path.join(os.getcwd(), "apps", "_sdk"))
    import dyld_report  # noqa: E402 — optional, best-effort
except Exception:  # noqa: BLE001 — attestation must never stop the plugin
    dyld_report = None


def send(conn, obj):
    """Send one JSONL line; every app->host line carries the capability token."""
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


def read_status():
    """example.read_status — a tiny, side-effect-free status object."""
    return {"status": "ok", "uptime_note": "example plugin alive"}


def handle(conn, msg):
    """Dispatch one host->app op. Only the manifest-declared tools are handled."""
    op = msg.get("type")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "example.read_status", "ready": True}})
        # Attest our own loaded dyld modules once at start (READ-ONLY, best-effort).
        # The daemon seeds a baseline on this first report, then flags any module a
        # later report adds (injection / unexpected dlopen) — see introspect.rs.
        if dyld_report is not None:
            try:
                send(conn, {"type": "modules", "data": dyld_report.modules_payload()})
            except Exception:  # noqa: BLE001 — never break the plugin over telemetry
                pass
    elif op == "refresh":
        send(conn, {"type": "items", "data": read_status()})
    elif op == "example.read_status":
        # The declared tool, served: correlated when the host sent an id.
        reply_result(conn, msg, read_status())
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
            except Exception as e:  # noqa: BLE001 — a plugin never crashes the host
                send(conn, {"type": "log", "data": {"line": f"handler error: {e}"}})
        if overflowed:
            send(conn, {"type": "log", "data": {"line": f"input frame exceeded {MAX_FRAME_BYTES} bytes; dropped"}})
    return 0


if __name__ == "__main__":
    sys.exit(main())
