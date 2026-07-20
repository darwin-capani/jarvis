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


if __name__ == "__main__":
    sys.exit(run(handle))
