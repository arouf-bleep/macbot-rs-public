#!/usr/bin/env python3
"""Lightweight live watch report for an in-flight soak run."""

from __future__ import annotations

import argparse
import json
import math
import sqlite3
import urllib.error
import urllib.request
from collections import Counter
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

from api_auth import get_json, resolve_api_key


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build a live watch report for an active soak")
    parser.add_argument("--db", required=True, help="SQLite DB path for the running soak")
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:8000",
        help="Local API base URL for the running bot",
    )
    parser.add_argument(
        "--minutes",
        type=float,
        default=15.0,
        help="Recent lookback window for outbox/rejection summaries",
    )
    parser.add_argument(
        "--outdir",
        default="artifacts/soak-watch-reports",
        help="Directory for JSON/Markdown artifacts",
    )
    return parser.parse_args()


def fetch_json(url: str, timeout_secs: float = 5.0) -> dict[str, Any] | None:
    try:
        return get_json(url, timeout=timeout_secs, read_auth=bool(resolve_api_key(None)))
    except (urllib.error.URLError, RuntimeError, TimeoutError, ValueError, json.JSONDecodeError):
        return None


def mean(values: list[float]) -> float | None:
    values = [value for item in values if (value := finite_float(item)) is not None]
    if not values:
        return None
    return sum(values) / len(values)


def finite_float(value: Any) -> float | None:
    try:
        number = float(value)
        return number if math.isfinite(number) else None
    except (TypeError, ValueError):
        return None


def load_recent_maker_events(db_path: Path, minutes: float) -> list[dict[str, Any]]:
    cutoff = datetime.now(timezone.utc) - timedelta(minutes=max(0.0, minutes))
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        rows = conn.execute(
            """
            SELECT timestamp, details
            FROM order_outbox
            WHERE stage = 'maker_quote_telemetry'
            ORDER BY id ASC
            """
        ).fetchall()
    finally:
        conn.close()

    events: list[dict[str, Any]] = []
    for row in rows:
        try:
            ts = datetime.fromisoformat(str(row["timestamp"]).replace("Z", "+00:00"))
        except ValueError:
            continue
        ts = (ts.replace(tzinfo=timezone.utc) if ts.tzinfo is None else ts).astimezone(timezone.utc)
        if ts < cutoff:
            continue
        try:
            payload = json.loads(row["details"])
        except (json.JSONDecodeError, TypeError):
            continue
        if not isinstance(payload, dict):
            continue
        payload["_timestamp"] = ts.isoformat()
        events.append(payload)
    return events


def load_db_counts(db_path: Path, minutes: float) -> dict[str, Any]:
    cutoff = datetime.now(timezone.utc) - timedelta(minutes=max(0.0, minutes))
    cutoff_str = cutoff.isoformat()
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        total_trades = conn.execute("SELECT COUNT(*) FROM trades").fetchone()[0]
        open_trades = conn.execute("SELECT COUNT(*) FROM trades WHERE status IN ('OPEN','CLOSING')").fetchone()[0]
        total_rejections = conn.execute("SELECT COUNT(*) FROM trade_rejections").fetchone()[0]
        recent_rejections = conn.execute(
            "SELECT COUNT(*) FROM trade_rejections WHERE julianday(timestamp) >= julianday(?)",
            (cutoff_str,),
        ).fetchone()[0]
        recent_rejection_reasons = [
            row[0]
            for row in conn.execute(
                """
                SELECT rejection_reason
                FROM trade_rejections
                WHERE julianday(timestamp) >= julianday(?)
                ORDER BY id DESC
                LIMIT 5
                """,
                (cutoff_str,),
            ).fetchall()
        ]
    finally:
        conn.close()

    return {
        "total_trades": int(total_trades),
        "open_trades": int(open_trades),
        "total_trade_rejections": int(total_rejections),
        "recent_trade_rejections": int(recent_rejections),
        "recent_rejection_reasons": recent_rejection_reasons,
    }


def summarize_events(events: list[dict[str, Any]]) -> dict[str, Any]:
    counts = Counter(str(event.get("event") or "unknown") for event in events)
    cancel_events = [event for event in events if event.get("event") == "cancel"]
    adverse_moves = [
        value
        for event in cancel_events
        if (value := finite_float(event.get("adverse_move_bps"))) is not None
    ]
    quote_ages = [
        value
        for event in cancel_events
        if (value := finite_float(event.get("quote_age_ms"))) is not None and value >= 0
    ]
    return {
        "maker_events_recent": len(events),
        "event_counts": dict(counts),
        "avg_recent_adverse_move_bps": mean(adverse_moves),
        "avg_recent_quote_age_ms": mean(quote_ages),
    }


def render_markdown(report: dict[str, Any]) -> str:
    health = report.get("health") or {}
    loop = report.get("loop_status") or {}
    maker = report.get("maker_snapshot") or {}
    maker_core = maker.get("maker") or {}
    db_counts = report.get("db_counts") or {}
    recent = report.get("recent_event_summary") or {}
    rejection_lines = [
        f"- {reason}" for reason in (db_counts.get("recent_rejection_reasons") or [])
    ] or ["- None"]

    return "\n".join(
        [
            "# Soak Watch Report",
            "",
            f"- Generated: `{report['generated_at']}`",
            f"- Health: `{health.get('status')}`",
            f"- Loop: `{loop.get('mode')}` / running=`{loop.get('running')}`",
            f"- Execution mode: `{maker.get('execution_mode')}`",
            f"- L2 stale: `{maker.get('l2_feed_stale')}`",
            f"- L2 age ms: `{maker.get('l2_feed_age_ms')}`",
            f"- Maker markets: `{maker_core.get('market_count')}`",
            f"- Live quote count: `{maker_core.get('quote_count')}`",
            f"- Replace events last min: `{maker_core.get('replace_events_last_min')}`",
            f"- Trades / open: `{db_counts.get('total_trades')}` / `{db_counts.get('open_trades')}`",
            f"- Trade rejections total / recent: `{db_counts.get('total_trade_rejections')}` / `{db_counts.get('recent_trade_rejections')}`",
            f"- Recent maker events: `{recent.get('maker_events_recent')}`",
            f"- Recent avg adverse move bps: `{recent.get('avg_recent_adverse_move_bps')}`",
            f"- Recent avg quote age ms: `{recent.get('avg_recent_quote_age_ms')}`",
            "",
            "## Recent Rejection Reasons",
            *rejection_lines,
        ]
    )


def main() -> int:
    args = parse_args()
    if not math.isfinite(args.minutes) or args.minutes <= 0:
        raise SystemExit("lookback minutes must be finite and positive")
    db_path = Path(args.db).expanduser().resolve()
    outdir = Path(args.outdir).expanduser().resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    health = fetch_json(f"{args.base_url}/api/health")
    loop_status = fetch_json(f"{args.base_url}/api/loop/status")
    maker_snapshot = fetch_json(f"{args.base_url}/api/maker/state")
    recent_events = load_recent_maker_events(db_path, args.minutes)
    db_counts = load_db_counts(db_path, args.minutes)

    report = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "db": str(db_path),
        "base_url": args.base_url,
        "lookback_minutes": float(args.minutes),
        "health": health,
        "loop_status": loop_status,
        "maker_snapshot": maker_snapshot,
        "db_counts": db_counts,
        "recent_event_summary": summarize_events(recent_events),
    }

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_soak_watch_report.json"
    md_path = outdir / f"{stamp}_soak_watch_report.md"
    latest_json = outdir / "latest_soak_watch_report.json"
    latest_md = outdir / "latest_soak_watch_report.md"

    markdown = render_markdown(report)
    json_text = json.dumps(report, indent=2)

    json_path.write_text(json_text, encoding="utf-8")
    md_path.write_text(markdown, encoding="utf-8")
    latest_json.write_text(json_text, encoding="utf-8")
    latest_md.write_text(markdown, encoding="utf-8")

    print(json_text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
