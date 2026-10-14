#!/usr/bin/env python3
"""Audit trade/signal field validity through the Rust backtest contract."""

from __future__ import annotations

import argparse
import json
import math
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

from api_auth import add_api_auth_args
from backtest_trade_report_common import (
    add_common_backtest_args,
    analyze_trades,
    optional_numeric_value,
    resolve_args,
    run_backtest,
    summary_from_trades,
)


ROOT = Path(__file__).resolve().parent.parent

CORE_FIELDS: list[tuple[str, Callable[[dict[str, Any]], float | None]]] = [
    (name, (lambda trade, field=name: directional_score(trade) if field == "directional_score"
            else optional_numeric_value(trade, field)))
    for name in ("directional_score", "score", "confidence_margin", "edge_margin_pp",
                 "rsi", "macd_hist", "macd_hist_delta", "momentum_pct", "volatility",
                 "next_return_pct", "realized_return_pct", "cost_pct")
]


def metric_text(value: float | None, decimals: int = 4) -> str:
    from backtest_trade_report_common import metric_text as evidence_text
    return evidence_text(value, decimals)


def boolean_text(value: bool | None) -> str:
    return "unavailable" if value is None else str(value)


def mean(values: list[float]) -> float:
    return sum(values) / len(values) if values else 0.0


def median(values: list[float]) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2.0


def percentile(values: list[float], q: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    pos = (len(ordered) - 1) * q
    low = math.floor(pos)
    high = math.ceil(pos)
    if low == high:
        return ordered[low]
    frac = pos - low
    return ordered[low] * (1.0 - frac) + ordered[high] * frac


def pearson(xs: list[float], ys: list[float]) -> float | None:
    if len(xs) != len(ys) or len(xs) < 2:
        return None
    mean_x = mean(xs)
    mean_y = mean(ys)
    cov = sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys))
    var_x = sum((x - mean_x) ** 2 for x in xs)
    var_y = sum((y - mean_y) ** 2 for y in ys)
    if var_x <= 1e-12 or var_y <= 1e-12:
        return None
    return cov / math.sqrt(var_x * var_y)


def directional_score(trade: dict[str, Any]) -> float | None:
    score = optional_numeric_value(trade, "score")
    signal = str(trade.get("signal", "")).upper()
    if score is None or signal not in {"BUY_YES", "BUY_NO"}:
        return None
    return -score if signal == "BUY_NO" else score


def field_report(
    trades: list[dict[str, Any]],
    value_fn: Callable[[dict[str, Any]], float | None],
) -> dict[str, Any]:
    measured = [(value, trade) for trade in trades if (value := value_fn(trade)) is not None]
    values = [value for value, _ in measured]
    win_pairs = [(value, float(trade["won"])) for value, trade in measured if isinstance(trade.get("won"), bool)]
    pnl_pairs = [(value, pnl) for value, trade in measured
                 if (pnl := optional_numeric_value(trade, "pnl")) is not None]
    rounded = {round(value, 10) for value in values}
    return {
        "count": len(values),
        "missing_count": len(trades) - len(values),
        "mean": mean(values) if values else None,
        "median": median(values) if values else None,
        "min": min(values) if values else None,
        "max": max(values) if values else None,
        "p10": percentile(values, 0.10) if values else None,
        "p90": percentile(values, 0.90) if values else None,
        "unique_rounded": len(rounded),
        "zero_variance": len(rounded) <= 1 if values else None,
        "correlation_with_win": pearson([x for x, _ in win_pairs], [y for _, y in win_pairs]),
        "correlation_with_pnl": pearson([x for x, _ in pnl_pairs], [y for _, y in pnl_pairs]),
    }


def quartile_summary(
    trades: list[dict[str, Any]],
    value_fn: Callable[[dict[str, Any]], float | None],
) -> list[dict[str, Any]]:
    measured = [trade for trade in trades if value_fn(trade) is not None]
    if not measured:
        return []
    ordered = sorted(measured, key=value_fn)
    rows: list[dict[str, Any]] = []
    for idx in range(4):
        start = len(ordered) * idx // 4
        end = len(ordered) * (idx + 1) // 4
        bucket = ordered[start:end]
        if not bucket:
            continue
        values = [value_fn(trade) for trade in bucket]
        rows.append(
            {
                "quartile": f"Q{idx + 1}",
                "value_min": min(values),
                "value_max": max(values),
                "value_mean": mean(values),
                **summary_from_trades(bucket),
            }
        )
    return rows


def monotonic_win_rate(rows: list[dict[str, Any]]) -> bool | None:
    if len(rows) < 2 or any(optional_numeric_value(row, "win_rate") is None for row in rows):
        return None
    return all(
        float(rows[i]["win_rate"]) <= float(rows[i + 1]["win_rate"]) + 1e-9
        for i in range(len(rows) - 1)
    )


def classify_surface(
    field_reports: dict[str, dict[str, Any]],
    degenerate_fields: list[str],
    monotonic: dict[str, bool | None],
) -> dict[str, Any]:
    score_surface_constant = (
        "score" in degenerate_fields
        and "confidence_margin" in degenerate_fields
        and "directional_score" in degenerate_fields
    )
    edge_corr = field_reports["edge_margin_pp"]["correlation_with_win"]
    next_corr = field_reports["next_return_pct"]["correlation_with_win"]
    edge_monotonic = monotonic.get("edge_margin_pp")
    if edge_corr is None or next_corr is None:
        return {"label": "insufficient_ranking_evidence", "score_surface_constant": score_surface_constant,
                "edge_margin_corr_win": edge_corr, "edge_margin_monotonic": edge_monotonic,
                "next_return_corr_win": next_corr, "warnings": ["correlations_unavailable_or_undefined"]}

    warnings: list[str] = []
    if score_surface_constant:
        warnings.append("score_surface_constant_within_lane")
    if edge_corr < -0.10:
        warnings.append("edge_margin_inverted_vs_wins")
    if edge_monotonic is None:
        warnings.append("edge_margin_quartiles_unavailable")
    elif edge_monotonic is False and abs(edge_corr) >= 0.20:
        warnings.append("edge_margin_quartiles_non_monotonic")
    if next_corr >= 0.50:
        warnings.append("next_return_is_strongest_separator")

    if edge_monotonic is None:
        label = "insufficient_ranking_evidence"
    elif score_surface_constant and edge_corr < -0.10:
        label = "coarse_gate_inverted_ranking"
    elif score_surface_constant:
        label = "coarse_gate_only"
    elif edge_corr < -0.10:
        label = "ranking_inverted"
    else:
        label = "continuous_ranking_ok"

    return {
        "label": label,
        "score_surface_constant": score_surface_constant,
        "edge_margin_corr_win": edge_corr,
        "edge_margin_monotonic": edge_monotonic,
        "next_return_corr_win": next_corr,
        "warnings": warnings,
    }


def analyze_shift(args: Any, shift_days: int) -> dict[str, Any]:
    payload, trades = run_backtest(args, days=args.days, shift_days=shift_days)
    analysis = analyze_trades(trades, high_vol_threshold=args.high_volatility_threshold)

    field_reports = {
        name: field_report(trades, value_fn) for name, value_fn in CORE_FIELDS
    }
    quartiles = {
        name: quartile_summary(trades, value_fn)
        for name, value_fn in CORE_FIELDS
        if name in {"directional_score", "confidence_margin", "edge_margin_pp"}
    }
    monotonic = {
        name: monotonic_win_rate(rows)
        for name, rows in quartiles.items()
    }
    degenerate_fields = [
        name for name, row in field_reports.items() if row["zero_variance"]
    ]
    surface_assessment = classify_surface(field_reports, degenerate_fields, monotonic)

    return {
        "shift_days": shift_days,
        "payload": payload,
        "summary": analysis["summary"],
        "winner_loser_profile": analysis["winner_loser_profile"],
        "by_regime": analysis["by_regime"],
        "by_session": analysis["by_session"],
        "by_hour_utc": analysis["by_hour_utc"],
        "by_confidence_margin_bucket": analysis["by_confidence_margin_bucket"],
        "by_edge_margin_bucket": analysis["by_edge_margin_bucket"],
        "field_reports": field_reports,
        "quartiles": quartiles,
        "monotonic_win_rate": monotonic,
        "degenerate_fields": degenerate_fields,
        "surface_assessment": surface_assessment,
    }


def write_outputs(report: dict[str, Any], out_dir: Path, stem: str) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_{stem}_signal_validity_report.json"
    md_path = out_dir / f"{ts}_{stem}_signal_validity_report.md"

    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    lines = [
        "# Signal Validity Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- preset: {report['preset']}",
        f"- report_label: {report['report_label']}",
        "",
    ]
    for shift in report["shifts"]:
        lines.extend(
            [
                f"## Shift {shift['shift_days']}",
                "",
                f"- trades: {shift['summary']['trades']}",
                f"- total_pnl: {metric_text(shift['summary']['total_pnl'], 4)}",
                f"- win_rate: {metric_text(shift['summary']['win_rate'], 2)}%",
                f"- edge_gap_pp: {metric_text(shift['summary']['edge_gap_pp'], 2)}",
                f"- surface_label: {shift['surface_assessment']['label']}",
                f"- degenerate_fields: {', '.join(shift['degenerate_fields']) or 'none'}",
                f"- directional_score_corr_win: {metric_text(shift['field_reports']['directional_score']['correlation_with_win'])}",
                f"- confidence_margin_corr_win: {metric_text(shift['field_reports']['confidence_margin']['correlation_with_win'])}",
                f"- edge_margin_corr_win: {metric_text(shift['field_reports']['edge_margin_pp']['correlation_with_win'])}",
                f"- next_return_corr_win: {metric_text(shift['field_reports']['next_return_pct']['correlation_with_win'])}",
                f"- directional_score_monotonic: {boolean_text(shift['monotonic_win_rate'].get('directional_score'))}",
                f"- confidence_margin_monotonic: {boolean_text(shift['monotonic_win_rate'].get('confidence_margin'))}",
                f"- edge_margin_monotonic: {boolean_text(shift['monotonic_win_rate'].get('edge_margin_pp'))}",
                f"- warnings: {', '.join(shift['surface_assessment']['warnings']) or 'none'}",
                "",
            ]
        )

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Audit trade/signal field validity")
    defaults = argparse.Namespace()
    add_common_backtest_args(parser)
    add_api_auth_args(parser)
    parser.add_argument("--days", type=int, default=7)
    parser.add_argument("--shift-days-list", default="0")
    parser.add_argument("--report-label", default="")
    parser.parse_args([], namespace=defaults)
    args = parser.parse_args()
    resolve_args(args, defaults)

    shifts = [int(part.strip()) for part in args.shift_days_list.split(",") if part.strip()]
    stem = args.report_label or args.preset or "custom"

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "preset": args.preset,
        "report_label": stem,
        "days": args.days,
        "filter_regimes": args.filter_regimes,
        "filter_signals": args.filter_signals,
        "shifts": [analyze_shift(args, shift) for shift in shifts],
    }
    json_path, md_path = write_outputs(report, ROOT / "artifacts/data-validity-reports", stem)
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
