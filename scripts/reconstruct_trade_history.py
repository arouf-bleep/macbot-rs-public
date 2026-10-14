#!/usr/bin/env python3
"""
Reconstruct a best-effort trade ledger from trade_fills + analysis_snapshots.

This is meant for cases where the top-level `trades` table was cleared or reset
but lower-level execution/fill history still exists.
"""

from __future__ import annotations

import argparse
import csv
import json
import sqlite3
from collections import Counter, defaultdict
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


def parse_ts(value: str | None) -> datetime | None:
    if not value:
        return None
    value = value.replace("Z", "+00:00")
    try:
        dt = datetime.fromisoformat(value)
    except ValueError:
        return None
    if dt.tzinfo is None:
        dt = dt.replace(tzinfo=timezone.utc)
    return dt


def iso_now() -> str:
    return datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")


@dataclass
class Segment:
    trade_id: int
    segment_index: int
    entry: sqlite3.Row
    fills: list[sqlite3.Row]


def build_segments(conn: sqlite3.Connection) -> list[Segment]:
    cur = conn.cursor()
    cur.execute("SELECT * FROM trade_fills ORDER BY trade_id, julianday(timestamp), id")
    rows = cur.fetchall()
    by_trade: dict[int, list[sqlite3.Row]] = defaultdict(list)
    for row in rows:
        by_trade[int(row["trade_id"])].append(row)

    segments: list[Segment] = []
    for trade_id, seq in by_trade.items():
        current: Segment | None = None
        segment_index = 0
        for row in seq:
            if row["phase"] == "entry":
                if current is not None and row["order_id"] == current.entry["order_id"]:
                    current.fills.append(row)
                    continue
                if current is not None:
                    segments.append(current)
                segment_index += 1
                current = Segment(
                    trade_id=trade_id,
                    segment_index=segment_index,
                    entry=row,
                    fills=[row],
                )
            elif current is not None:
                current.fills.append(row)
        if current is not None:
            segments.append(current)
    return segments


def load_best_snapshot(
    conn: sqlite3.Connection,
    market_id: str,
    entry_ts: str,
    max_age_seconds: int,
) -> dict[str, Any] | None:
    cur = conn.cursor()
    cur.execute(
        """
        SELECT timestamp, question, signal, confidence, net_ev, p_up, price, regime, phase, reason
        FROM analysis_snapshots
        WHERE market_id = ? AND julianday(timestamp) <= julianday(?)
        ORDER BY julianday(timestamp) DESC
        LIMIT 25
        """,
        (market_id, entry_ts),
    )
    rows = [dict(r) for r in cur.fetchall()]
    if not rows:
        return None

    entry_dt = parse_ts(entry_ts)
    for row in rows:
        snap_dt = parse_ts(row["timestamp"])
        if entry_dt is None or snap_dt is None:
            age_seconds = None
        else:
            age_seconds = (entry_dt - snap_dt).total_seconds()
        row["snapshot_age_seconds"] = age_seconds
        if row.get("signal") in ("BUY_YES", "BUY_NO"):
            if age_seconds is not None and 0 <= age_seconds <= max_age_seconds:
                return row
    return None


def classify_segment(entry_shares: float, closed_shares: float) -> str:
    if closed_shares <= 1e-9:
        return "entry_only_missing_close"
    ratio = closed_shares / entry_shares if entry_shares > 1e-9 else 0.0
    if ratio > 1.0 + 1e-6:
        return "inconsistent_exit_quantity"
    if ratio >= 1.0 - 1e-6:
        return "closed_reconstructed"
    return "partial_or_truncated"


def fmt_money(value: float | None) -> str:
    if value is None:
        return "—"
    return f"${value:,.2f}"


def write_markdown(
    path: Path,
    report: dict[str, Any],
    rows: list[dict[str, Any]],
) -> None:
    def fmt_price(value: float | None) -> str:
        return f"{value:.3f}" if value is not None else "—"

    status_lines = "\n".join(
        f"- `{status}`: `{count}`" for status, count in report["status_counts"].items()
    )
    direction_lines = "\n".join(
        f"- `{direction}`: `{count}`"
        for direction, count in report["direction_counts"].items()
    )
    sample_rows = rows[:10]
    sample_table = "\n".join(
        f"| {row['reconstructed_id']} | {row['entry_timestamp']} | {row['direction'] or 'UNKNOWN'} | "
        f"{row['question'][:48] if row['question'] else row['market_id'][:16]} | "
        f"{fmt_price(row['entry_price'])} | {fmt_price(row['exit_price_est'])} | "
        f"{row['status']} | {row['realized_pnl_est']:.4f} |"
        for row in sample_rows
    )
    text = f"""# Reconstructed Trade History

## Summary
- Source DB: `{report['db_path']}`
- Reconstructed trade instances: `{report['segments_total']}`
- Fill-backed unique entry orders: `{report['entry_order_count']}`
- Snapshot-enriched rows: `{report['snapshot_match_count']}`
- Entry time range: `{report['entry_time_min']}` -> `{report['entry_time_max']}`
- Distinct markets: `{report['distinct_markets']}`
- Estimated realized PnL across rows with close legs: `{report['estimated_realized_pnl_sum']:.4f}`

## Status
{status_lines}

## Directions
{direction_lines}

## Validation
- Surviving top-level trades matched by entry timestamp/market: `{report['validation']['matched_surviving_trades']}` / `{report['validation']['surviving_trade_rows']}`
- Exact direction matches on surviving rows: `{report['validation']['direction_matches']}`
- Max entry timestamp drift on surviving rows (seconds): `{report['validation']['max_entry_time_drift_seconds']}`
- Max entry price drift on surviving rows: `{report['validation']['max_entry_price_drift']}`

## Caveats
- `trade_id` was reused after the top-level `trades` table was cleared, so reconstruction is keyed off entry-fill lifecycles, not `trade_id` alone.
- Exit coverage is incomplete for part of the history; rows marked `entry_only_missing_close` or `partial_or_truncated` should be treated as approximate.
- Reconstructed PnL is a gross price estimate from recorded close legs, before fees; it is not authoritative cash accounting.

## Sample
| ID | Entry Time | Direction | Question / Market | Entry | Exit Est | Status | PnL Est |
| --- | --- | --- | --- | --- | --- | --- | --- |
{sample_table}
"""
    path.write_text(text)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", default="trades.db", help="SQLite database path")
    parser.add_argument(
        "--out-dir",
        default="artifacts/reconstructed-trades",
        help="Directory for CSV/JSON/Markdown outputs",
    )
    parser.add_argument(
        "--snapshot-max-age-seconds",
        type=int,
        default=300,
        help="Preferred max age for matching a BUY snapshot to an entry fill",
    )
    args = parser.parse_args()

    db_path = Path(args.db)
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = iso_now()

    if not db_path.is_file():
        raise FileNotFoundError(db_path)
    conn = sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    conn.execute("BEGIN")

    segments = build_segments(conn)
    rows: list[dict[str, Any]] = []
    status_counts: Counter[str] = Counter()
    direction_counts: Counter[str] = Counter()
    entry_order_count = 0
    snapshot_match_count = 0
    estimated_realized_pnl_sum = 0.0

    for idx, seg in enumerate(sorted(segments, key=lambda s: (s.entry["timestamp"], s.trade_id, s.segment_index)), start=1):
        entry = seg.entry
        entry_order_count += 1
        closes = [r for r in seg.fills if r["phase"] in ("partial_exit", "exit")]
        entry_fills = [r for r in seg.fills if r["phase"] == "entry"]
        entry_shares = sum(float(r["delta_size"] or 0.0) for r in entry_fills)
        if entry_shares <= 0:
            entry_shares = float(entry["cumulative_size"] or 0.0)
            entry_price = float(entry["avg_price"])
        else:
            entry_price = sum(float(r["avg_price"]) * float(r["delta_size"] or 0.0) for r in entry_fills) / entry_shares
        closed_shares = sum(float(r["delta_size"] or 0.0) for r in closes)
        completion_ratio = (closed_shares / entry_shares) if entry_shares > 1e-9 else 0.0
        realized_pnl_est = sum(
            (float(r["avg_price"]) - entry_price) * float(r["delta_size"] or 0.0)
            for r in closes
        )
        estimated_realized_pnl_sum += realized_pnl_est
        exit_price_est = (
            sum(float(r["avg_price"]) * float(r["delta_size"] or 0.0) for r in closes) / closed_shares
            if closed_shares > 1e-9
            else None
        )
        close_timestamp = closes[-1]["timestamp"] if closes else None

        snap = load_best_snapshot(
            conn,
            market_id=str(entry["market_id"]),
            entry_ts=str(entry["timestamp"]),
            max_age_seconds=args.snapshot_max_age_seconds,
        )
        if snap is not None:
            snapshot_match_count += 1

        direction = snap.get("signal") if snap else None
        if direction:
            direction_counts[direction] += 1

        status = classify_segment(entry_shares, closed_shares)
        status_counts[status] += 1

        row = {
            "reconstructed_id": idx,
            "trade_id_reused": int(seg.trade_id),
            "segment_index": int(seg.segment_index),
            "entry_order_id": str(entry["order_id"]),
            "market_id": str(entry["market_id"]),
            "question": snap.get("question") if snap else None,
            "direction": direction,
            "entry_timestamp": str(entry["timestamp"]),
            "entry_price": entry_price,
            "entry_shares": entry_shares,
            "amount_est_usd": entry_price * entry_shares,
            "exit_timestamp_est": close_timestamp,
            "exit_price_est": exit_price_est,
            "closed_shares": closed_shares,
            "close_completion_ratio": completion_ratio,
            "close_leg_count": len(closes),
            "has_exit_leg": any(r["phase"] == "exit" for r in closes),
            "has_partial_exit_leg": any(r["phase"] == "partial_exit" for r in closes),
            "realized_pnl_est": realized_pnl_est,
            "status": status,
            "snapshot_timestamp": snap.get("timestamp") if snap else None,
            "snapshot_age_seconds": snap.get("snapshot_age_seconds") if snap else None,
            "confidence": snap.get("confidence") if snap else None,
            "net_ev": snap.get("net_ev") if snap else None,
            "p_up": snap.get("p_up") if snap else None,
            "price_at_signal": snap.get("price") if snap else None,
            "regime": snap.get("regime") if snap else None,
            "phase": snap.get("phase") if snap else None,
            "reason": snap.get("reason") if snap else None,
            "fill_count": len(seg.fills),
        }
        rows.append(row)

    # Validate against surviving top-level trades.
    cur = conn.cursor()
    cur.execute("SELECT * FROM trades ORDER BY id")
    surviving_trades = [dict(r) for r in cur.fetchall()]
    matched_survivors = 0
    direction_matches = 0
    max_entry_time_drift_seconds = 0.0
    max_entry_price_drift = 0.0
    for trade in surviving_trades:
        trade_ts = parse_ts(trade.get("timestamp"))
        candidates = [
            row
            for row in rows
            if row["trade_id_reused"] == trade["id"] and row["market_id"] == trade["market_id"]
        ]
        best = None
        best_drift = None
        for row in candidates:
            entry_dt = parse_ts(row["entry_timestamp"])
            if trade_ts is None or entry_dt is None:
                continue
            drift = abs((entry_dt - trade_ts).total_seconds())
            if best_drift is None or drift < best_drift:
                best = row
                best_drift = drift
        if best is None:
            continue
        matched_survivors += 1
        max_entry_time_drift_seconds = max(max_entry_time_drift_seconds, best_drift or 0.0)
        max_entry_price_drift = max(
            max_entry_price_drift,
            abs(float(trade["entry_price"]) - float(best["entry_price"])),
        )
        if best.get("direction") == trade.get("direction"):
            direction_matches += 1

    conn.close()
    entry_times = [row["entry_timestamp"] for row in rows]
    distinct_markets = len({row["market_id"] for row in rows})
    report = {
        "db_path": str(db_path.resolve()),
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "segments_total": len(rows),
        "entry_order_count": entry_order_count,
        "snapshot_match_count": snapshot_match_count,
        "entry_time_min": min(entry_times) if entry_times else None,
        "entry_time_max": max(entry_times) if entry_times else None,
        "distinct_markets": distinct_markets,
        "status_counts": dict(status_counts),
        "direction_counts": dict(direction_counts),
        "estimated_realized_pnl_sum": estimated_realized_pnl_sum,
        "validation": {
            "surviving_trade_rows": len(surviving_trades),
            "matched_surviving_trades": matched_survivors,
            "direction_matches": direction_matches,
            "max_entry_time_drift_seconds": max_entry_time_drift_seconds,
            "max_entry_price_drift": max_entry_price_drift,
        },
    }

    csv_path = out_dir / f"{stamp}_reconstructed_trade_history.csv"
    json_path = out_dir / f"{stamp}_reconstructed_trade_history.json"
    md_path = out_dir / f"{stamp}_reconstructed_trade_history.md"

    fieldnames = list(rows[0].keys()) if rows else []
    with csv_path.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fieldnames)
        writer.writeheader()
        writer.writerows(rows)

    json_path.write_text(json.dumps({"report": report, "rows": rows}, indent=2))
    write_markdown(md_path, report, rows)

    print(json.dumps({"report": report, "csv": str(csv_path), "json": str(json_path), "md": str(md_path)}, indent=2))


if __name__ == "__main__":
    main()
