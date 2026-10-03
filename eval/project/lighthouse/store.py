"""Where results are kept."""

import sqlite3
import time
from pathlib import Path

DB_PATH = Path.home() / ".local" / "share" / "lighthouse" / "results.db"

# Results older than this are deleted at start.
KEEP_DAYS = 90

SCHEMA = """
CREATE TABLE IF NOT EXISTS probes (
    target TEXT NOT NULL,
    at REAL NOT NULL,
    up INTEGER NOT NULL,
    detail TEXT
)
"""


def open_db(path: Path = DB_PATH) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    db = sqlite3.connect(path)
    db.execute(SCHEMA)
    db.execute("DELETE FROM probes WHERE at < ?", (time.time() - KEEP_DAYS * 86400,))
    return db


def record(db: sqlite3.Connection, target: str, up: bool, detail: str) -> None:
    db.execute("INSERT INTO probes VALUES (?, ?, ?, ?)", (target, time.time(), int(up), detail))
    db.commit()


def recent(db: sqlite3.Connection, target: str, limit: int = 20) -> list[tuple]:
    return db.execute(
        "SELECT at, up, detail FROM probes WHERE target = ? ORDER BY at DESC LIMIT ?",
        (target, limit),
    ).fetchall()
