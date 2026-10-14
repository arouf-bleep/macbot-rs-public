"""Prevent shared database writes with SQLite versions affected by the WAL race.

The native SQLite library comes from the Python interpreter, not pip's lock file.
The project requires 3.51.3 or newer for these tools, even though upstream also
released fixes for a few older branches: https://www.sqlite.org/wal.html
"""
from __future__ import annotations

import sqlite3

MIN_WRITE_VERSION = (3, 51, 3)


def connect_for_write(path: str, *, operation: str) -> sqlite3.Connection:
    """Refuse before opening or changing a possibly shared WAL database."""
    if sqlite3.sqlite_version_info < MIN_WRITE_VERSION:
        required = ".".join(map(str, MIN_WRITE_VERSION))
        raise RuntimeError(
            f"{operation} requires SQLite {required} or newer for database writes; "
            f"this Python uses {sqlite3.sqlite_version}. Upgrade the Python/SQLite "
            "runtime before retrying. No database was opened."
        )
    return sqlite3.connect(path)
