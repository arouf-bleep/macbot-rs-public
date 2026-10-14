#!/usr/bin/env python3
"""Compute win-rate by hour-of-day from SQLite trades.

Usage:
  python3 scripts/time_of_day_report.py --db trades.db
"""
from __future__ import annotations

import argparse
import sqlite3
import sys
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def parse_ts(raw: str) -> datetime | None:
    for fmt in ("%Y-%m-%dT%H:%M:%S%z", "%Y-%m-%dT%H:%M:%S.%f%z"):
        try:
            return datetime.strptime(raw, fmt).astimezone(timezone.utc)
        except ValueError:
            continue
    try:
        parsed = datetime.fromisoformat(raw)
        if parsed.tzinfo is None:
            parsed = parsed.replace(tzinfo=timezone.utc)
        return parsed.astimezone(timezone.utc)
    except ValueError:
        return None


def main() -> int:
    p = argparse.ArgumentParser(description="Time-of-day win rate report")
    p.add_argument("--db", default="trades.db")
    args = p.parse_args()

    try:
        conn = sqlite3.connect(Path(args.db).resolve().as_uri() + "?mode=ro", uri=True)
    except sqlite3.Error as exc:
        print(f"error: cannot read trade history: {exc}", file=sys.stderr)
        return 1
    cur = conn.cursor()
    cur.execute(
        """
        SELECT timestamp, pnl, fee_paid, accounting_version
        FROM trades
        WHERE status = 'CLOSED' AND pnl IS NOT NULL
        """
    )

    buckets: dict[int, dict[str, Any]] = defaultdict(lambda: {"trades": 0, "wins": 0})

    rows = cur.fetchall()
    conn.close()
    for ts, pnl, fee_paid, accounting_version in rows:
        dt = parse_ts(ts)
        if dt is None:
            continue
        hour = dt.hour
        buckets[hour]["trades"] += 1
        won = float(pnl) - (0.0 if accounting_version == 1 else float(fee_paid or 0.0)) > 0.0
        if won:
            buckets[hour]["wins"] += 1

    print("hour,trades,win_rate")
    for hour in range(24):
        b = buckets.get(hour, {"trades": 0, "wins": 0})
        trades = b["trades"]
        wins = b["wins"]
        win_rate = wins / trades if trades else 0.0
        print(f"{hour:02d},{trades},{win_rate:.3f}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
