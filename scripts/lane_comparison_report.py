#!/usr/bin/env python3
"""Compare trade and signal composition across multiple named lanes."""

from __future__ import annotations

import argparse
import json
from copy import deepcopy
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from api_auth import add_api_auth_args
from backtest_trade_report_common import (
    NUMERIC_FIELDS,
    summary_metric_delta,
    add_common_backtest_args,
    analyze_trades,
    csv_strings,
    resolve_args,
    run_backtest,
    top_bucket_rows,
)


def clone_with_preset(base_args: Any, defaults: Any, preset: str) -> Any:
    cloned = deepcopy(base_args)
    cloned.preset = preset
    resolve_args(cloned, defaults)
    return cloned


def numeric_delta_table(left: dict[str, Any], right: dict[str, Any]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for _, label in NUMERIC_FIELDS:
        left_row, right_row = left.get(label, {}), right.get(label, {})
        left_mean = left_row.get("mean") if left_row.get("count", 1) else None
        right_mean = right_row.get("mean") if right_row.get("count", 1) else None
        rows.append(
            {
                "field": label,
                "left_mean": left_mean,
                "right_mean": right_mean,
                "delta": left_mean - right_mean if left_mean is not None and right_mean is not None else None,
            }
        )
    rows.sort(key=lambda row: abs(row["delta"]) if row["delta"] is not None else -1.0, reverse=True)
    return rows


def metric_text(value: float | None, decimals: int = 4) -> str:
    from backtest_trade_report_common import metric_text as evidence_text
    return evidence_text(value, decimals)


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_lane_comparison_report.json"
    md_path = out_dir / f"{ts}_lane_comparison_report.md"

    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    lines = [
        "# Lane Comparison Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- presets: {', '.join(report['presets'])}",
        f"- days: {report['days']}",
        f"- shift_days: {report['shift_days']}",
        "",
        "## Lane Summaries",
        "",
        "| preset | trades | total_pnl | win_rate | edge_gap_pp | avg_cost_pct |",
        "| --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    for lane in report["lanes"]:
        summary = lane["summary"]
        lines.append(
            f"| {lane['preset']} | {summary['trades']} | {metric_text(summary['total_pnl'], 4)} | "
            f"{metric_text(summary['win_rate'], 2)} | {metric_text(summary['edge_gap_pp'], 2)} | {metric_text(summary['avg_cost_pct'], 2)} |"
        )

    for comparison in report["comparisons"]:
        lines.extend(
            [
                "",
                f"## {comparison['left_preset']} vs {comparison['right_preset']}",
                "",
                "| field | left_mean | right_mean | delta |",
                "| --- | ---: | ---: | ---: |",
            ]
        )
        for row in comparison["numeric_mean_deltas"][:10]:
            lines.append(
                f"| {row['field']} | {metric_text(row['left_mean'])} | {metric_text(row['right_mean'])} | {metric_text(row['delta'])} |"
            )

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Compare maker/taker lane composition on the same anchor")
    parser.add_argument("--presets", default="")
    parser.add_argument("--days", type=int, default=7)
    parser.add_argument("--shift-days", type=int, default=0)
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    add_common_backtest_args(parser)
    add_api_auth_args(parser)

    defaults = parser.parse_args([])
    args = parser.parse_args()

    presets = csv_strings(args.presets)
    if len(presets) < 2:
        raise SystemExit("need at least two presets")

    lanes: list[dict[str, Any]] = []
    for preset in presets:
        lane_args = clone_with_preset(args, defaults, preset)
        payload, trades = run_backtest(lane_args, days=args.days, shift_days=args.shift_days)
        analysis = analyze_trades(trades, high_vol_threshold=lane_args.high_volatility_threshold)
        lanes.append(
            {
                "preset": preset,
                "payload": payload,
                "summary": analysis["summary"],
                "numeric_profile": analysis["numeric_profile"],
                "winner_loser_profile": analysis["winner_loser_profile"],
                "top_positive_hours": top_bucket_rows(analysis["by_hour_utc"], limit=5, reverse=True),
                "top_negative_hours": top_bucket_rows(analysis["by_hour_utc"], limit=5),
                "top_positive_regimes": top_bucket_rows(analysis["by_regime"], limit=3, reverse=True),
                "top_negative_regimes": top_bucket_rows(analysis["by_regime"], limit=3),
            }
        )

    comparisons: list[dict[str, Any]] = []
    for idx, left in enumerate(lanes):
        for right in lanes[idx + 1 :]:
            comparisons.append(
                {
                    "left_preset": left["preset"],
                    "right_preset": right["preset"],
                    "summary_delta": {
                        key: summary_metric_delta(left["summary"], right["summary"], key)
                        for key in ("trades", "win_rate", "edge_gap_pp", "total_pnl", "avg_pnl")
                    },
                    "numeric_mean_deltas": numeric_delta_table(
                        left["numeric_profile"],
                        right["numeric_profile"],
                    ),
                    "winner_loser_delta_shift": numeric_delta_table(
                        {key: {"mean": value} for key, value in left["winner_loser_profile"]["winner_minus_loser_mean"].items()},
                        {key: {"mean": value} for key, value in right["winner_loser_profile"]["winner_minus_loser_mean"].items()},
                    ),
                }
            )

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "presets": presets,
        "days": args.days,
        "shift_days": args.shift_days,
        "lanes": lanes,
        "comparisons": comparisons,
    }

    json_path, md_path = write_outputs(report, Path(args.output_dir))
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
