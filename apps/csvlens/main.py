#!/usr/bin/env python3
"""Read-only CSV profiler: row/column counts, headers, and per-column non-empty/empty tallies. Pure, offline (csv + io stdlib)."""
import csv
import io
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


if __name__ == "__main__":
    sys.exit(run(handle))
