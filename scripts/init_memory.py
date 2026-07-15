#!/usr/bin/env python3
"""DARWIN persistent-memory initializer (stdlib only).

Creates the runtime state directory tree and state/darwin.db with the schema
shared with the Rust daemon. Idempotent: safe to run repeatedly.

SCHEMA — keep in sync with the Rust daemon:
  events(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, source TEXT NOT NULL,
         kind TEXT NOT NULL, payload TEXT)
  facts(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, key TEXT NOT NULL,
        value TEXT NOT NULL, confidence REAL DEFAULT 1.0)
    + index idx_facts_key on facts(key)  — upsert_fact() updates by key
  transcripts(id INTEGER PRIMARY KEY, ts TEXT NOT NULL, wav_path TEXT,
              text TEXT NOT NULL, intent TEXT, routed_to TEXT,
              response TEXT)
    — response is what DARWIN replied; recent_exchanges(n) selects rows
      WHERE response IS NOT NULL. The daemon applies the same column as an
      idempotent ALTER TABLE migration for databases created before it.
"""

import argparse
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]

STATE_DIRS = (
    "state",
    "state/ipc",
    "state/ipc/apps",
    "state/logs",
    "state/tmp",
)

SCHEMA = """
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY,
    ts TEXT NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT
);

CREATE TABLE IF NOT EXISTS facts (
    id INTEGER PRIMARY KEY,
    ts TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    confidence REAL DEFAULT 1.0
);

CREATE TABLE IF NOT EXISTS transcripts (
    id INTEGER PRIMARY KEY,
    ts TEXT NOT NULL,
    wav_path TEXT,
    text TEXT NOT NULL,
    intent TEXT,
    routed_to TEXT,
    response TEXT
);

CREATE INDEX IF NOT EXISTS idx_facts_key ON facts(key);
"""

# Idempotent migrations for databases created by an older schema. CREATE
# TABLE IF NOT EXISTS does not touch existing tables, so added columns are
# applied here: (table, column, type).
MIGRATIONS = (
    ("transcripts", "response", "TEXT"),
)


def apply_migrations(conn):
    for table, column, col_type in MIGRATIONS:
        existing = {row[1] for row in conn.execute(f"PRAGMA table_info({table})")}
        if column not in existing:
            conn.execute(f"ALTER TABLE {table} ADD COLUMN {column} {col_type}")
            print(f"[db] migrated: added {table}.{column} {col_type}")
    # Audit fix: the phase marker used to be stored as 'darwin.phase', which
    # the daemon's meta.%-only prompt filter does NOT exclude — system
    # bookkeeping was injected into every persona prompt as a "user fact"
    # and was deletable by the consolidation pass. Rename in place; the
    # 'meta.' prefix is filtered from prompts and protected from
    # model-driven writes everywhere.
    migrated = conn.execute(
        "UPDATE facts SET key = 'meta.phase' WHERE key = 'darwin.phase'"
    ).rowcount
    if migrated:
        print("[db] migrated: renamed fact darwin.phase -> meta.phase")


def init_memory(root):
    root = Path(root)

    for rel in STATE_DIRS:
        path = root / rel
        path.mkdir(parents=True, exist_ok=True)
        print(f"[dirs] ok: {path}")

    db_path = root / "state" / "darwin.db"
    conn = sqlite3.connect(db_path)
    try:
        conn.executescript(SCHEMA)
        apply_migrations(conn)
        ts = datetime.now(timezone.utc).isoformat()

        # Bootstrap rows — skipped if already present (idempotent).
        row = conn.execute(
            "SELECT 1 FROM events WHERE source = ? AND kind = ? LIMIT 1",
            ("system", "memory.initialized"),
        ).fetchone()
        if row is None:
            conn.execute(
                "INSERT INTO events (ts, source, kind, payload) VALUES (?, ?, ?, ?)",
                (ts, "system", "memory.initialized", None),
            )
            print("[db] inserted bootstrap event (system / memory.initialized)")
        else:
            print("[db] bootstrap event already present; skipping")

        row = conn.execute(
            "SELECT 1 FROM facts WHERE key = ? LIMIT 1",
            ("meta.phase",),
        ).fetchone()
        if row is None:
            conn.execute(
                "INSERT INTO facts (ts, key, value) VALUES (?, ?, ?)",
                (ts, "meta.phase", "1"),
            )
            print("[db] inserted bootstrap fact (meta.phase = 1)")
        else:
            print("[db] bootstrap fact already present; skipping")

        conn.commit()

        tables = [
            name
            for (name,) in conn.execute(
                "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name"
            )
        ]
        print(f"[db] {db_path} ready; tables: {', '.join(tables)}")
    finally:
        conn.close()

    return db_path


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="init_memory.py",
        description="Create the DARWIN state/ tree and initialize state/darwin.db (idempotent).",
    )
    parser.add_argument(
        "--root",
        default=str(PROJECT_ROOT),
        help="project root containing state/ (default: repository root inferred from this script)",
    )
    args = parser.parse_args(argv)
    init_memory(args.root)
    return 0


if __name__ == "__main__":
    sys.exit(main())
