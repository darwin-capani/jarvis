#!/usr/bin/env python3
"""Read-only CSV profiler: row/column counts, headers, and per-column non-empty/empty tallies. Pure, offline (csv + io stdlib)."""
import csv
import io
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
