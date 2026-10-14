#!/usr/bin/env python3
"""Generate a compact attribution report from /api/attribution.

Usage:
  python3 scripts/attribution_report.py
  python3 scripts/attribution_report.py --base-url http://localhost:8000 --top 5 --format markdown
"""

from __future__ import annotations

import argparse
import json
import sys
from typing import Any
from api_auth import add_api_auth_args, get_json


def fetch_json(url: str, *, api_key: str | None = None) -> dict[str, Any]:
    return get_json(url, timeout=10, api_key=api_key, read_auth=True)


def top_rows(rows: Any, top: int) -> list[dict[str, Any]]:
    if not isinstance(rows, list):
        return []
    out: list[dict[str, Any]] = []
    for row in rows:
        if isinstance(row, dict):
            out.append(row)
    return out[: max(1, top)]


def pct(v: Any) -> str:
    try:
        return f"{float(v) * 100.0:.2f}%"
    except (TypeError, ValueError):
        return "n/a"


def usd(v: Any) -> str:
    try:
        return f"{float(v):.4f}"
    except (TypeError, ValueError):
        return "n/a"


def print_markdown(payload: dict[str, Any], top: int) -> None:
    closed = payload.get("closed_trades", 0)
    current_cal = payload.get("current_calibration_version", "unknown")
    current_run = payload.get("current_run_id", "unknown")

    print("# Attribution Report")
    print()
    print(f"- closed_trades: {closed}")
    print(f"- current_calibration_version: {current_cal}")
    print(f"- current_run_id: {current_run}")
    print()

    sections = [
        ("by_mode", "mode"),
        ("by_source", "source"),
        ("by_calibration", "calibration_version"),
        ("by_run_id", "run_id"),
    ]

    for key, label_key in sections:
        rows = top_rows(payload.get(key), top)
        print(f"## {key}")
        if not rows:
            print("(no data)")
            print()
            continue
        print("| key | trades | wins | win_rate | total_pnl | avg_pnl |")
        print("| --- | ---: | ---: | ---: | ---: | ---: |")
        for r in rows:
            print(
                "| {k} | {trades} | {wins} | {wr} | {pnl} | {avg} |".format(
                    k=r.get(label_key, "unknown"),
                    trades=r.get("trades", 0),
                    wins=r.get("wins", 0),
                    wr=pct(r.get("win_rate")),
                    pnl=usd(r.get("total_pnl")),
                    avg=usd(r.get("avg_pnl")),
                )
            )
        print()


def print_json(payload: dict[str, Any], top: int) -> None:
    report = {
        "closed_trades": payload.get("closed_trades", 0),
        "current_calibration_version": payload.get("current_calibration_version"),
        "current_run_id": payload.get("current_run_id"),
        "by_mode": top_rows(payload.get("by_mode"), top),
        "by_source": top_rows(payload.get("by_source"), top),
        "by_calibration": top_rows(payload.get("by_calibration"), top),
        "by_run_id": top_rows(payload.get("by_run_id"), top),
    }
    print(json.dumps(report, indent=2))


def main() -> int:
    parser = argparse.ArgumentParser(description="Build a compact attribution report")
    parser.add_argument("--base-url", default="http://localhost:8000", help="API base URL")
    parser.add_argument("--top", type=int, default=10, help="Top N rows per section")
    parser.add_argument(
        "--format",
        choices=("json", "markdown"),
        default="json",
        help="Output format",
    )
    add_api_auth_args(parser)
    args = parser.parse_args()

    url = args.base_url.rstrip("/") + "/api/attribution"
    try:
        payload = fetch_json(url, api_key=args.api_key)
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    if args.format == "markdown":
        print_markdown(payload, args.top)
    else:
        print_json(payload, args.top)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
