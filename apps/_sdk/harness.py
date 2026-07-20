"""Shared micro-app host-link plumbing — the copy-pasted socket loop, ONCE.

Every standard DARWIN micro-app does the identical thing at the wire: connect to
its per-app JSONL socket, stamp its capability TOKEN on every outbound line,
newline-frame inbound lines (bounded against an OOM flood), and dispatch each op
to a per-app `handle(conn, msg)`. That plumbing used to be byte-for-byte
duplicated in ~32 `main.py` files; it now lives here, imported by each app, so
the wire contract (token stamping, the agent-tool request-id echo, the frame
bound) is a ONE-file change instead of ~32.

Domain logic (each app's `compute` + `handle`) stays in the app. Import contract,
mirroring a standard app — the sys.path insert is resolved relative to __file__
(apps/<app>/main.py -> ../_sdk), NOT os.getcwd(), so it works both when darwind
launches the app (cwd = project root) AND when the app's own tests run from the
app dir (where a getcwd path would resolve to the wrong place and ImportError):

    import os, sys
    sys.dont_write_bytecode = True   # apps/_sdk is read-only in the sandbox
    sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "_sdk"))
    from harness import send, reply_result, run, drain_lines, MAX_FRAME_BYTES, TOKEN

    def compute(payload): ...
    def handle(conn, msg): ...          # calls send(conn, ...) / reply_result(conn, msg, ...)
    if __name__ == "__main__":
        sys.exit(run(handle))

Re-importing `drain_lines` / `MAX_FRAME_BYTES` / `TOKEN` into the app's namespace
keeps `main.drain_lines` etc. resolvable, so the per-app framing/contract tests
that read those symbols off `main` need no change.

The daemon side is the trust boundary: it owns + binds the 0600 socket, verifies
the token on every inbound line, and bounds its own reads — this module is the
app-side convenience, it grants nothing.
"""
import json
import os
import socket
import sys

# The per-launch capability token + socket path darwind injects into the app's
# environment. Read once at import; empty when not launched by darwind (tests).
TOKEN = os.environ.get("DARWIN_APP_TOKEN", "")
SOCKET_PATH = os.environ.get("DARWIN_APP_SOCKET", "")

# Cap on one un-newlined frame from the daemon. Mirrors the daemon's own bound
# (apps.rs read_line_bounded / genproxy MAX_PROXY_LINE_BYTES): a peer streaming
# an unframed, unbounded blob can't grow the read buffer without bound (OOM).
MAX_FRAME_BYTES = 8 * 1024 * 1024


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


def run(handle):
    """Connect to the per-app socket and serve it, dispatching each framed op to
    `handle(conn, msg)`. Returns an exit code: 1 when not launched by darwind
    (no token/socket in the environment), else 0 on a clean EOF or a `stop` op
    (which `handle` signals by raising SystemExit). A handler exception is caught
    and relayed as a `log` line — a misbehaving app never crashes the host."""
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
