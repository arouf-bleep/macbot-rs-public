#!/usr/bin/env python3
"""Fetch and summarize the dashboard/backend integrity report."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path

from api_auth import add_api_auth_args, get_json


ROOT = Path(__file__).resolve().parent.parent


def write_outputs(report: dict, out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_dashboard_integrity_report.json"
    md_path = out_dir / f"{ts}_dashboard_integrity_report.md"

    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    checks = report.get("checks", {})
    balance = checks.get("balance", {})
    open_positions = checks.get("open_positions", {})
    recent = checks.get("recent_trades", {})
    maker = checks.get("maker", {})
    reconcile = checks.get("reconcile", {})
    mismatches = report.get("mismatches", []) or []

    lines = [
        "# Dashboard Integrity Report",
        "",
        f"- generated_at: {report.get('generated_at')}",
        f"- ok: {report.get('ok')}",
        f"- recent_limit: {report.get('recent_limit')}",
        f"- sync_authoritative: {report.get('sync_authoritative')}",
        "",
        "## Balance",
        "",
        f"- runtime_balance: {balance.get('runtime_balance')}",
        f"- fast_balance: {balance.get('fast_balance')}",
        f"- stats_current_balance: {balance.get('stats_current_balance')}",
        f"- accounting_current_balance: {balance.get('accounting_current_balance')}",
        f"- accounting_derived_hot_balance: {balance.get('accounting_derived_hot_balance')}",
        f"- derived_hot_drift_usd: {balance.get('derived_hot_drift_usd')}",
        "",
        "## Open Positions",
        "",
        f"- hot_open_count: {open_positions.get('hot_open_count')}",
        f"- db_open_count: {open_positions.get('db_open_count')}",
        f"- stats_open_trades: {open_positions.get('stats_open_trades')}",
        f"- fast_open_positions: {open_positions.get('fast_open_positions')}",
        f"- hot_open_notional_usd: {open_positions.get('hot_open_notional_usd')}",
        f"- fast_open_notional_usd: {open_positions.get('fast_open_notional_usd')}",
        "",
        "## Recent Trades",
        "",
        f"- fast_last_trade_id: {recent.get('fast_last_trade_id')}",
        f"- db_last_trade_id: {recent.get('db_last_trade_id')}",
        f"- slow_trade_ids: {recent.get('slow_trade_ids')}",
        f"- db_trade_ids: {recent.get('db_trade_ids')}",
        "",
        "## Maker",
        "",
        f"- fast_quote_count: {maker.get('fast_quote_count')}",
        f"- direct_quote_count: {maker.get('direct_quote_count')}",
        "",
        "## Reconcile",
        "",
        f"- last_reconcile_summary: {json.dumps(reconcile.get('last_reconcile_summary'))}",
        f"- last_reconcile_error: {json.dumps(reconcile.get('last_reconcile_error'))}",
        "",
        "## Mismatches",
        "",
    ]
    if mismatches:
        for item in mismatches:
            lines.append(f"- {item.get('check')}: {item.get('detail')}")
    else:
        lines.append("- none")

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Fetch dashboard/backend integrity report")
    parser.add_argument("--base-url", default="http://localhost:8000")
    parser.add_argument("--output-dir", default="artifacts/integrity-reports")
    parser.add_argument("--request-timeout-secs", type=int, default=30)
    parser.add_argument("--recent-limit", type=int, default=12)
    parser.add_argument("--sync-authoritative", action="store_true")
    add_api_auth_args(parser)
    args = parser.parse_args()

    url = (
        args.base_url.rstrip("/")
        + f"/api/integrity/report?recent_limit={args.recent_limit}"
        + ("&sync_authoritative=true" if args.sync_authoritative else "")
    )
    report = get_json(
        url,
        timeout=args.request_timeout_secs,
        api_key=args.api_key,
        api_secret=args.api_secret,
        read_auth=True,
    )
    json_path, md_path = write_outputs(report, ROOT / args.output_dir)

    print(
        json.dumps(
            {
                "status": "ok" if report.get("ok") else "mismatch",
                "mismatch_count": len(report.get("mismatches", []) or []),
                "artifacts": {
                    "json": str(json_path),
                    "markdown": str(md_path),
                },
                "report": report,
            },
            indent=2,
        )
    )
    return 0 if report.get("ok") else 2


if __name__ == "__main__":
    raise SystemExit(main())
