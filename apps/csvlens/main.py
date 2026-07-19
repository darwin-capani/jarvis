#!/usr/bin/env python3
"""Read-only CSV profiler: row/column counts, headers, and per-column non-empty/empty tallies. Pure, offline (csv + io stdlib)."""
import csv
import io
import json
import os
import socket
import sys

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


def compute(payload):
    """PURE, offline, no I/O, never raises. Profile payload['csv'] into row/col counts and per-column tallies."""
    if not isinstance(payload, dict):
        return {"error": "payload must be an object"}
    raw = payload.get("csv", "")
    if not isinstance(raw, str):
        return {"error": "csv must be a string"}
    if raw == "":
        return {"error": "empty csv"}

    delimiter = payload.get("delimiter", ",")
    # csv.reader requires a single-character delimiter; guard bad input.
    if not isinstance(delimiter, str) or len(delimiter) != 1:
        return {"error": "delimiter must be a single character"}

    try:
        reader = csv.reader(io.StringIO(raw), delimiter=delimiter)
        parsed = list(reader)
    except Exception as e:  # noqa: BLE001 — surface parse failure as data, never raise
        return {"error": str(e)}

    if not parsed:
        return {"error": "empty csv"}

    header_row = parsed[0]
    data_rows = parsed[1:]
    columns = len(header_row)
    if columns == 0:
        return {"error": "no columns in header"}

    # Per-column tallies over the data rows only (header excluded).
    non_empty = [0] * columns
    empty = [0] * columns
    for row in data_rows:
        for i in range(columns):
            # Cells missing from a short row count as empty for that column.
            cell = row[i] if i < len(row) else ""
            if isinstance(cell, str) and cell.strip() != "":
                non_empty[i] += 1
            else:
                empty[i] += 1

    column_stats = [
        {"name": header_row[i], "non_empty": non_empty[i], "empty": empty[i]}
        for i in range(columns)
    ][:50]

    return {
        "rows": len(data_rows),
        "columns": columns,
        "headers": header_row[:50],
        "column_stats": column_stats,
    }


def handle(conn, msg):
    op = msg.get("type") or msg.get("op")
    if op == "start":
        send(conn, {"type": "status", "data": {"tool": "csvlens.profile", "ready": True}})
    elif op == "refresh":
        send(conn, {"type": "items", "data": {"status": "ok"}})
    elif op == "csvlens.profile":
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
