#!/usr/bin/env python3
"""Create a verified, atomic SQLite snapshot without modifying the source."""

from __future__ import annotations

import argparse
from contextlib import closing
import os
from pathlib import Path
import sqlite3
import tempfile


SCHEMA_VERSION = 8
CRITICAL_TABLES = (
    "trades", "order_outbox", "trade_fills", "analysis_snapshots",
    "idempotency_cache", "live_orders", "live_matches", "maker_orders",
    "maker_matches", "maker_match_allocations", "maker_market_controls",
    "paper_maker_orders", "paper_maker_fill_events", "meta",
    "realized_pnl_events",
    "redemption_receipts",
)


def critical_table_counts(conn: sqlite3.Connection) -> dict[str, int]:
    return {table: int(conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0])
            for table in CRITICAL_TABLES}


def quick_check(conn: sqlite3.Connection, label: str) -> None:
    rows = [row[0] for row in conn.execute("PRAGMA quick_check")]
    if not rows or any(str(row).lower() != "ok" for row in rows):
        raise RuntimeError(f"{label} quick_check failed: {rows}")


def check_destination(source: Path, dest: Path, overwrite: bool) -> None:
    if source == dest or (dest.exists() and source.samefile(dest)):
        raise ValueError("source and destination identify the same database")
    if dest.exists() and (not overwrite or not dest.is_file()):
        raise ValueError("destination exists; use --overwrite only for an inactive backup file")
    if any(Path(str(dest) + suffix).exists() for suffix in ("-wal", "-shm", "-journal")):
        raise ValueError("destination has SQLite sidecars; stop its users before replacement")


def create_backup(source: Path, dest: Path, overwrite: bool = False) -> dict:
    source = source.expanduser().resolve(strict=True)
    dest = dest.expanduser().resolve()
    if not source.is_file():
        raise ValueError("source must be an existing SQLite file")
    check_destination(source, dest, overwrite)
    dest.parent.mkdir(parents=True, exist_ok=True)
    # Build beside the destination so publication is atomic on its filesystem.
    fd, temporary = tempfile.mkstemp(prefix=".macbot-backup-", suffix=".sqlite", dir=dest.parent)
    os.close(fd)
    stage = Path(temporary)
    try:
        # mode=ro prevents accidentally creating a missing source. A read
        # transaction gives counts and backup the same snapshot under WAL writes.
        with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as src:
            src.execute("BEGIN")
            quick_check(src, "source")
            version = src.execute("PRAGMA user_version").fetchone()[0]
            if version != SCHEMA_VERSION:
                raise ValueError(f"unsupported source schema: expected {SCHEMA_VERSION}, got {version}")
            counts = critical_table_counts(src)
            with closing(sqlite3.connect(stage)) as dst:
                src.backup(dst)
                quick_check(dst, "backup")
                if dst.execute("PRAGMA user_version").fetchone()[0] != version:
                    raise RuntimeError("backup schema version mismatch")
                if critical_table_counts(dst) != counts:
                    raise RuntimeError("backup critical table counts mismatch")
                tables = dst.execute("SELECT COUNT(*) FROM sqlite_master WHERE type='table'").fetchone()[0]
            src.rollback()
        with stage.open("rb") as stream:
            os.fsync(stream.fileno())
        check_destination(source, dest, overwrite)
        if overwrite:
            os.replace(stage, dest)
        else:
            # Unlike rename, link refuses a destination created concurrently.
            os.link(stage, dest)
            stage.unlink()
        if os.name == "posix":
            directory_fd = os.open(dest.parent, os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
        return {"schema_version": version, "tables": tables, "critical_counts": counts}
    finally:
        for path in (stage, *(Path(str(stage) + suffix) for suffix in ("-wal", "-shm", "-journal"))):
            path.unlink(missing_ok=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--dest", required=True, type=Path)
    parser.add_argument("--overwrite", action="store_true", help="Atomically replace an inactive backup")
    args = parser.parse_args()
    try:
        result = create_backup(args.source, args.dest, args.overwrite)
    except (OSError, ValueError, RuntimeError, sqlite3.Error) as error:
        parser.exit(1, f"backup failed: {error}\n")
    print(f"backup ok: {result}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
