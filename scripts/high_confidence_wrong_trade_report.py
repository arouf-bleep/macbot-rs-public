#!/usr/bin/env python3
"""Review the model's most confident wrong exact-input rows."""

from __future__ import annotations

import argparse
import json
import math
import re
import sqlite3
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
RUST_MODEL = ROOT / "src" / "ml" / "model.rs"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Review high-confidence wrong exact-input rows")
    parser.add_argument("--db", default="trades.db", help="SQLite DB with ml_model_inputs")
    parser.add_argument(
        "--confidence-threshold",
        type=float,
        default=0.70,
        help="Minimum p_up / (1-p_up) confidence to count as high-confidence",
    )
    parser.add_argument(
        "--top",
        type=int,
        default=25,
        help="Top wrong rows to include in the detailed section",
    )
    parser.add_argument(
        "--output-dir",
        default="artifacts/trade-review-reports",
        help="Directory for JSON/Markdown artifacts",
    )
    return parser.parse_args()


def load_feature_names(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(
        r'FEATURE_NAMES:\s*\[&str;\s*NUM_FEATURES\]\s*=\s*\[(.*?)\];',
        text,
        re.DOTALL,
    )
    if not match:
        raise RuntimeError(f"FEATURE_NAMES not found in {path}")
    return re.findall(r'"([^"]+)"', match.group(1))


def pct(part: int, total: int) -> float:
    return (part / total) * 100.0 if total else 0.0


def mean(values: list[float]) -> float | None:
    if not values:
        return None
    return sum(values) / len(values)


def summarize_counter(counter: Counter[str], total: int, top: int = 10) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for label, count in counter.most_common(top):
        rows.append({"label": label, "count": count, "pct": pct(count, total)})
    return rows


def summarize_group_stats(
    totals: dict[str, int],
    wrongs: dict[str, int],
    top: int = 10,
) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    for label, total in totals.items():
        wrong = wrongs.get(label, 0)
        rows.append(
            {
                "label": label,
                "rows": total,
                "wrong_rows": wrong,
                "wrong_rate_pct": pct(wrong, total),
            }
        )
    rows.sort(key=lambda item: (float(item["wrong_rate_pct"]), int(item["rows"])), reverse=True)
    return rows[:top]


def top_feature_snapshot(features: dict[str, Any], top_n: int = 5) -> list[dict[str, Any]]:
    ranked: list[tuple[str, float]] = []
    for key, value in features.items():
        try:
            magnitude = abs(float(value))
            if math.isfinite(magnitude):
                ranked.append((str(key), magnitude))
        except (TypeError, ValueError):
            continue
    ranked.sort(key=lambda item: item[1], reverse=True)
    out: list[dict[str, Any]] = []
    for key, magnitude in ranked[:top_n]:
        out.append({"feature": key, "abs_value": magnitude, "value": features.get(key)})
    return out


def analyze_rows(db_path: Path, feature_names: list[str], confidence_threshold: float, top: int) -> dict[str, Any]:
    if not 0.5 <= confidence_threshold <= 1:
        raise ValueError("confidence threshold must be finite and between .5 and 1")
    if not db_path.is_file():
        raise FileNotFoundError(db_path)
    conn = sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True, timeout=30.0)
    conn.row_factory = sqlite3.Row

    has_table = conn.execute(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ml_model_inputs'"
    ).fetchone()[0]
    if not has_table:
        conn.close()
        return {"status": "missing_table", "table": "ml_model_inputs", "db": str(db_path)}

    rows = conn.execute(
        """
        SELECT
            timestamp,
            market_id,
            run_id,
            signal,
            regime_label,
            phase,
            fusion_score,
            p_up,
            features_json,
            outcome
        FROM ml_model_inputs
        WHERE outcome IN (0,1) AND p_up BETWEEN 0 AND 1
          AND COALESCE(run_id,'') NOT GLOB 'reconstructed:*'
          AND COALESCE(run_id,'') NOT GLOB 'backfill_*'
        ORDER BY timestamp
        """
    ).fetchall()
    conn.close()

    total_labeled = len(rows)
    if total_labeled == 0:
        return {"status": "empty", "db": str(db_path), "labeled_rows": 0}

    confident_rows = 0
    wrong_rows_all: list[dict[str, Any]] = []
    confident_wrong_rows: list[dict[str, Any]] = []
    signal_counts: Counter[str] = Counter()
    regime_counts: Counter[str] = Counter()
    phase_counts: Counter[str] = Counter()
    hour_counts: Counter[str] = Counter()
    market_counts: Counter[str] = Counter()
    run_counts: Counter[str] = Counter()
    total_signal_counts: Counter[str] = Counter()
    total_regime_counts: Counter[str] = Counter()
    total_phase_counts: Counter[str] = Counter()
    total_signal_regime_counts: Counter[str] = Counter()
    wrong_signal_counts: Counter[str] = Counter()
    wrong_regime_counts: Counter[str] = Counter()
    wrong_phase_counts: Counter[str] = Counter()
    wrong_signal_regime_counts: Counter[str] = Counter()
    feature_loss: dict[str, list[float]] = defaultdict(list)

    for row in rows:
        p_up = float(row["p_up"])
        outcome = float(row["outcome"])
        predicted_up = p_up >= 0.5
        actual_up = outcome >= 0.5
        confidence = max(p_up, 1.0 - p_up)
        confidence_margin = abs(p_up - 0.5)
        was_wrong = predicted_up != actual_up
        brier = (p_up - outcome) ** 2
        signal = str(row["signal"] or "unknown")
        regime = str(row["regime_label"] or "unknown")
        phase = str(row["phase"] or "unknown")
        signal_regime = f"{signal} | {regime}"
        ts = str(row["timestamp"])
        hour = "unknown"
        try:
            hour = str(datetime.fromisoformat(ts.replace("Z", "+00:00")).astimezone(timezone.utc).hour)
        except ValueError:
            pass

        total_signal_counts[signal] += 1
        total_regime_counts[regime] += 1
        total_phase_counts[phase] += 1
        total_signal_regime_counts[signal_regime] += 1

        features = {}
        try:
            features = json.loads(row["features_json"] or "{}")
        except (json.JSONDecodeError, TypeError):
            features = {}
        if not isinstance(features, dict):
            features = {}

        wrong_row = {
            "timestamp": ts,
            "market_id": row["market_id"],
            "run_id": row["run_id"],
            "signal": signal,
            "regime_label": regime,
            "phase": phase,
            "fusion_score": row["fusion_score"],
            "p_up": p_up,
            "outcome": outcome,
            "predicted_direction": "UP" if predicted_up else "DOWN",
            "actual_direction": "UP" if actual_up else "DOWN",
            "confidence": confidence,
            "confidence_margin": confidence_margin,
            "brier": brier,
            "top_abs_features": top_feature_snapshot(features),
        }

        if confidence >= confidence_threshold:
            confident_rows += 1
            if was_wrong:
                confident_wrong_rows.append(wrong_row)
        elif was_wrong:
            # still count lower-confidence mistakes in the broader review
            pass

        if not was_wrong:
            continue

        wrong_signal_counts[signal] += 1
        wrong_regime_counts[regime] += 1
        wrong_phase_counts[phase] += 1
        wrong_signal_regime_counts[signal_regime] += 1
        signal_counts[signal] += 1
        regime_counts[regime] += 1
        phase_counts[phase] += 1
        hour_counts[hour] += 1
        market_counts[str(row["market_id"] or "unknown")] += 1
        run_counts[str(row["run_id"] or "unknown")] += 1

        for feature_name in feature_names:
            value = features.get(feature_name)
            try:
                magnitude = abs(float(value))
                if math.isfinite(magnitude):
                    feature_loss[feature_name].append(magnitude)
            except (TypeError, ValueError):
                continue

        wrong_rows_all.append(wrong_row)

    confident_wrong_rows.sort(
        key=lambda row: (float(row["confidence"]), float(row["brier"])), reverse=True
    )
    wrong_rows_all.sort(key=lambda row: (float(row["confidence"]), float(row["brier"])), reverse=True)

    average_feature_magnitude = sorted(
        (
            {"feature": feature, "avg_abs_value": mean(values), "rows": len(values)}
            for feature, values in feature_loss.items()
        ),
        key=lambda item: (float(item["avg_abs_value"] or 0.0), int(item["rows"])),
        reverse=True,
    )

    return {
        "status": "ok",
        "db": str(db_path),
        "confidence_threshold": confidence_threshold,
        "labeled_rows": total_labeled,
        "confident_rows": confident_rows,
        "wrong_rows": len(wrong_rows_all),
        "wrong_row_pct_of_labeled": pct(len(wrong_rows_all), total_labeled),
        "confident_wrong_rows": len(confident_wrong_rows),
        "confident_row_pct": pct(confident_rows, total_labeled),
        "confident_wrong_pct_of_labeled": pct(len(confident_wrong_rows), total_labeled),
        "confident_wrong_pct_of_confident": pct(len(confident_wrong_rows), confident_rows),
        "avg_confidence_margin_wrong_all": mean(
            [float(row["confidence_margin"]) for row in wrong_rows_all]
        ),
        "avg_confidence_margin_wrong": mean(
            [float(row["confidence_margin"]) for row in confident_wrong_rows]
        ),
        "avg_brier_wrong_all": mean([float(row["brier"]) for row in wrong_rows_all]),
        "avg_brier_wrong": mean([float(row["brier"]) for row in confident_wrong_rows]),
        "by_signal": summarize_counter(signal_counts, len(wrong_rows_all)),
        "by_regime": summarize_counter(regime_counts, len(wrong_rows_all)),
        "by_phase": summarize_counter(phase_counts, len(wrong_rows_all)),
        "signal_wrong_rates": summarize_group_stats(total_signal_counts, wrong_signal_counts),
        "regime_wrong_rates": summarize_group_stats(total_regime_counts, wrong_regime_counts),
        "phase_wrong_rates": summarize_group_stats(total_phase_counts, wrong_phase_counts),
        "signal_regime_wrong_rates": summarize_group_stats(
            total_signal_regime_counts,
            wrong_signal_regime_counts,
        ),
        "by_hour_utc": summarize_counter(hour_counts, len(wrong_rows_all), top=24),
        "by_market": summarize_counter(market_counts, len(wrong_rows_all), top=10),
        "by_run_id": summarize_counter(run_counts, len(wrong_rows_all), top=10),
        "top_feature_magnitudes": average_feature_magnitude[:10],
        "top_wrong_rows": wrong_rows_all[: max(1, top)],
        "top_confident_wrong_rows": confident_wrong_rows[: max(1, top)],
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# High-Confidence Wrong Trade Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- db: `{report['db']}`",
        f"- status: `{report['status']}`",
    ]
    if report["status"] != "ok":
        return "\n".join(lines) + "\n"

    lines.extend(
        [
            f"- confidence_threshold: `{report['confidence_threshold']:.2f}`",
            f"- labeled_rows: `{report['labeled_rows']}`",
            f"- wrong_rows: `{report['wrong_rows']}` (`{report['wrong_row_pct_of_labeled']:.2f}%` of labeled)",
            f"- confident_rows: `{report['confident_rows']}` (`{report['confident_row_pct']:.2f}%` of labeled)",
            f"- confident_wrong_rows: `{report['confident_wrong_rows']}` (`{report['confident_wrong_pct_of_labeled']:.2f}%` of labeled, `{report['confident_wrong_pct_of_confident']:.2f}%` of confident)",
            f"- avg_confidence_margin_wrong_all: `{report['avg_confidence_margin_wrong_all']}`",
            f"- avg_brier_wrong_all: `{report['avg_brier_wrong_all']}`",
            f"- avg_confidence_margin_wrong: `{report['avg_confidence_margin_wrong']}`",
            f"- avg_brier_wrong: `{report['avg_brier_wrong']}`",
            "",
            "## By Signal",
            "",
        ]
    )
    for row in report["by_signal"]:
        lines.append(f"- `{row['label']}`: `{row['count']}` (`{row['pct']:.2f}%`)")
    lines.extend(["", "## By Regime", ""])
    for row in report["by_regime"]:
        lines.append(f"- `{row['label']}`: `{row['count']}` (`{row['pct']:.2f}%`)")
    lines.extend(["", "## Signal Wrong Rates", ""])
    for row in report["signal_wrong_rates"]:
        lines.append(
            f"- `{row['label']}`: wrong `{row['wrong_rows']}` / `{row['rows']}` (`{row['wrong_rate_pct']:.2f}%`)"
        )
    lines.extend(["", "## Regime Wrong Rates", ""])
    for row in report["regime_wrong_rates"]:
        lines.append(
            f"- `{row['label']}`: wrong `{row['wrong_rows']}` / `{row['rows']}` (`{row['wrong_rate_pct']:.2f}%`)"
        )
    lines.extend(["", "## Signal + Regime Wrong Rates", ""])
    for row in report["signal_regime_wrong_rates"]:
        lines.append(
            f"- `{row['label']}`: wrong `{row['wrong_rows']}` / `{row['rows']}` (`{row['wrong_rate_pct']:.2f}%`)"
        )
    lines.extend(["", "## By Hour UTC", ""])
    for row in report["by_hour_utc"]:
        lines.append(f"- hour `{row['label']}`: `{row['count']}` (`{row['pct']:.2f}%`)")
    lines.extend(["", "## Top Wrong Rows", ""])
    for row in report["top_wrong_rows"]:
        lines.append(
            f"- `{row['timestamp']}` market=`{row['market_id']}` signal=`{row['signal']}` regime=`{row['regime_label']}` "
            f"pred=`{row['predicted_direction']}` actual=`{row['actual_direction']}` "
            f"p_up=`{row['p_up']:.3f}` confidence=`{row['confidence']:.3f}` brier=`{row['brier']:.4f}`"
        )
    lines.extend(["", "## Top Confident Wrong Rows", ""])
    for row in report["top_confident_wrong_rows"]:
        lines.append(
            f"- `{row['timestamp']}` market=`{row['market_id']}` signal=`{row['signal']}` regime=`{row['regime_label']}` "
            f"pred=`{row['predicted_direction']}` actual=`{row['actual_direction']}` "
            f"p_up=`{row['p_up']:.3f}` confidence=`{row['confidence']:.3f}` brier=`{row['brier']:.4f}`"
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    db_path = Path(args.db)
    if not db_path.is_absolute():
        db_path = (ROOT / db_path).resolve()
    output_dir = Path(args.output_dir)
    if not output_dir.is_absolute():
        output_dir = (ROOT / output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    report = analyze_rows(
        db_path=db_path,
        feature_names=load_feature_names(RUST_MODEL),
        confidence_threshold=args.confidence_threshold,
        top=args.top,
    )
    report["generated_at_utc"] = datetime.now(timezone.utc).isoformat()

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = output_dir / f"{stamp}_high_confidence_wrong_trade_report.json"
    md_path = output_dir / f"{stamp}_high_confidence_wrong_trade_report.md"
    latest_json_path = output_dir / "latest_high_confidence_wrong_trade_report.json"
    latest_md_path = output_dir / "latest_high_confidence_wrong_trade_report.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    md_path.write_text(render_markdown(report), encoding="utf-8")
    latest_json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    latest_md_path.write_text(render_markdown(report), encoding="utf-8")

    print(
        json.dumps(
            {
                "json": str(json_path),
                "md": str(md_path),
                "status": report["status"],
                "confident_wrong_rows": report.get("confident_wrong_rows"),
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
