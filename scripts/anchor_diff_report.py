#!/usr/bin/env python3
"""Compare one lane across shifted anchors and explain drift."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from api_auth import add_api_auth_args
from backtest_trade_report_common import (
    NUMERIC_FIELDS,
    optional_numeric_value,
    summary_metric_delta,
    add_common_backtest_args,
    analyze_trades,
    csv_ints,
    resolve_args,
    run_backtest,
    top_bucket_rows,
)


def metric_text(value: Any, decimals: int = 2) -> str:
    from backtest_trade_report_common import metric_text as evidence_text
    return evidence_text(value, decimals)


def summary_delta(left: dict[str, Any], right: dict[str, Any], key: str) -> float | None:
    return summary_metric_delta(left, right, key)


def numeric_delta_table(left: dict[str, Any], right: dict[str, Any]) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for _, label in NUMERIC_FIELDS:
        left_row, right_row = left.get(label, {}), right.get(label, {})
        left_mean = left_row.get("mean") if left_row.get("count") != 0 else None
        right_mean = right_row.get("mean") if right_row.get("count") != 0 else None
        delta = summary_delta({"mean": left_mean}, {"mean": right_mean}, "mean")
        rows.append({"field": label, "left_mean": left_mean,
                     "right_mean": right_mean, "delta": delta})
    rows.sort(key=lambda row: (row["delta"] is not None,
                              abs(row["delta"]) if row["delta"] is not None else 0.0), reverse=True)
    return rows


def summarize_run(shift_days: int, payload: dict[str, Any], analysis: dict[str, Any], artifact_label: str) -> dict[str, Any]:
    summary = analysis["summary"]
    return {
        "shift_days": shift_days,
        "artifact_label": artifact_label,
        "end_time_ms": payload.get("end_time_ms"),
        "summary": summary,
        "numeric_profile": analysis["numeric_profile"],
        "winner_loser_profile": analysis["winner_loser_profile"],
        "by_regime": analysis["by_regime"],
        "by_session": analysis["by_session"],
        "by_hour_utc": analysis["by_hour_utc"],
        "by_volatility_bucket": analysis["by_volatility_bucket"],
        "by_confidence_margin_bucket": analysis["by_confidence_margin_bucket"],
        "by_edge_margin_bucket": analysis["by_edge_margin_bucket"],
        "worst_trades": analysis["worst_trades"],
        "best_trades": analysis["best_trades"],
    }


def write_outputs(report: dict[str, Any], out_dir: Path, stem: str) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_{stem}.json"
    md_path = out_dir / f"{ts}_{stem}.md"

    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    current = report["current_anchor"]
    best = report["best_anchor"]
    worst = report["worst_anchor"]
    lines = [
        "# Anchor Diff Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- preset: {report['preset']}",
        f"- anchor_shifts_days: {report['anchor_shifts_days']}",
        f"- current_shift_days: {current['shift_days']}",
        f"- best_shift_days: {best['shift_days']}",
        f"- worst_shift_days: {worst['shift_days']}",
        "",
        "## Current Anchor",
        "",
        f"- trades: {current['summary']['trades']}",
        f"- total_pnl: {metric_text(current['summary']['total_pnl'], 4)}",
        f"- win_rate: {metric_text(current['summary']['win_rate'], 2)}%",
        f"- edge_gap_pp: {metric_text(current['summary'].get('edge_gap_pp'))}",
        "",
        "## Best Anchor",
        "",
        f"- trades: {best['summary']['trades']}",
        f"- total_pnl: {metric_text(best['summary']['total_pnl'], 4)}",
        f"- win_rate: {metric_text(best['summary']['win_rate'], 2)}%",
        f"- edge_gap_pp: {metric_text(best['summary'].get('edge_gap_pp'))}",
        "",
        "## Worst Anchor",
        "",
        f"- trades: {worst['summary']['trades']}",
        f"- total_pnl: {metric_text(worst['summary']['total_pnl'], 4)}",
        f"- win_rate: {metric_text(worst['summary']['win_rate'], 2)}%",
        f"- edge_gap_pp: {metric_text(worst['summary'].get('edge_gap_pp'))}",
        "",
        "## Current Vs Best Numeric Drift",
        "",
        "| field | current_mean | best_mean | delta |",
        "| --- | ---: | ---: | ---: |",
    ]
    for row in report["current_vs_best"]["numeric_mean_deltas"][:10]:
        lines.append(
            f"| {row['field']} | {metric_text(row.get('left_mean'), 4)} | {metric_text(row.get('right_mean'), 4)} | {metric_text(row.get('delta'), 4)} |"
        )

    lines.extend(
        [
            "",
            "## Current Worst Hours",
            "",
            "| hour | trades | edge_gap_pp | total_pnl |",
            "| --- | ---: | ---: | ---: |",
        ]
    )
    for row in current["worst_hours"]:
        lines.append(
            f"| {row['bucket']} | {row['trades']} | {metric_text(row.get('edge_gap_pp'))} | {metric_text(row['total_pnl'], 4)} |"
        )

    lines.extend(
        [
            "",
            "## Worst-Anchor Worst Hours",
            "",
            "| hour | trades | edge_gap_pp | total_pnl |",
            "| --- | ---: | ---: | ---: |",
        ]
    )
    for row in worst["worst_hours"]:
        lines.append(
            f"| {row['bucket']} | {row['trades']} | {metric_text(row.get('edge_gap_pp'))} | {metric_text(row['total_pnl'], 4)} |"
        )

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Explain lane drift across shifted anchors")
    parser.add_argument("--anchor-shifts", default="0,1,2,3,4,5,6")
    parser.add_argument("--days", type=int, default=7)
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    add_common_backtest_args(parser)
    add_api_auth_args(parser)

    defaults = parser.parse_args([])
    args = parser.parse_args()
    resolve_args(args, defaults)

    shifts = csv_ints(args.anchor_shifts)
    if not shifts:
        raise SystemExit("no anchor shifts provided")

    runs: list[dict[str, Any]] = []
    for shift in shifts:
        payload, trades = run_backtest(args, days=args.days, shift_days=shift)
        analysis = analyze_trades(trades, high_vol_threshold=args.high_volatility_threshold)
        run = summarize_run(shift, payload, analysis, artifact_label=f"shift_{shift}")
        run["worst_hours"] = top_bucket_rows(run["by_hour_utc"], limit=5)
        run["best_hours"] = top_bucket_rows(run["by_hour_utc"], limit=5, reverse=True)
        run["worst_regimes"] = top_bucket_rows(run["by_regime"], limit=3)
        run["best_regimes"] = top_bucket_rows(run["by_regime"], limit=3, reverse=True)
        runs.append(run)

    runs.sort(key=lambda row: row["shift_days"])
    current = next((run for run in runs if run["shift_days"] == 0), runs[0])
    rankable = [run for run in runs if optional_numeric_value(run["summary"], "total_pnl") is not None]
    if not rankable:
        raise SystemExit("no anchors have complete finite P&L evidence; cannot rank them")
    worst = min(rankable, key=lambda row: row["summary"]["total_pnl"])
    best = max(rankable, key=lambda row: row["summary"]["total_pnl"])

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "preset": args.preset,
        "days": args.days,
        "anchor_shifts_days": shifts,
        "ranking_unavailable_anchor_shifts": [run["shift_days"] for run in runs if run not in rankable],
        "config": {
            "execution_mode": args.mode,
            "exit_mode": args.exit_mode,
            "entry_threshold": args.entry_threshold,
            "min_confidence_margin": args.min_confidence_margin,
            "regime_allowlist": args.regime_allowlist,
            "session_allowlist": args.session_allowlist,
            "blocked_hours_utc": args.blocked_hours_utc,
            "filter_regimes": args.filter_regimes,
            "filter_signals": args.filter_signals,
        },
        "runs": runs,
        "current_anchor": current,
        "best_anchor": best,
        "worst_anchor": worst,
        "current_vs_best": {
            "summary_delta": {
                key: summary_delta(current["summary"], best["summary"], key)
                for key in ("trades", "win_rate", "edge_gap_pp", "total_pnl", "avg_pnl")
            },
            "numeric_mean_deltas": numeric_delta_table(
                current["numeric_profile"],
                best["numeric_profile"],
            ),
            "winner_loser_delta_shift": numeric_delta_table(
                {key: {"mean": value} for key, value in current["winner_loser_profile"]["winner_minus_loser_mean"].items()},
                {key: {"mean": value} for key, value in best["winner_loser_profile"]["winner_minus_loser_mean"].items()},
            ),
        },
        "current_vs_worst": {
            "summary_delta": {
                key: summary_delta(current["summary"], worst["summary"], key)
                for key in ("trades", "win_rate", "edge_gap_pp", "total_pnl", "avg_pnl")
            },
            "numeric_mean_deltas": numeric_delta_table(
                current["numeric_profile"],
                worst["numeric_profile"],
            ),
            "winner_loser_delta_shift": numeric_delta_table(
                {key: {"mean": value} for key, value in current["winner_loser_profile"]["winner_minus_loser_mean"].items()},
                {key: {"mean": value} for key, value in worst["winner_loser_profile"]["winner_minus_loser_mean"].items()},
            ),
        },
    }

    out_dir = Path(args.output_dir)
    stem = f"{(args.preset or 'lane').lower()}_anchor_diff_report"
    json_path, md_path = write_outputs(report, out_dir, stem)
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
