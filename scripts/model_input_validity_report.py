#!/usr/bin/env python3
"""Audit historical model-input fidelity against the runtime feature contract."""

from __future__ import annotations

import argparse
import ast
import json
import math
import re
import sqlite3
import statistics
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent

CONTEXT_KEYS = [
    "time_remaining",
    "balance",
    "positions",
    "contract_price",
    "price_discovery_lag",
    "btc_price",
]

TRAIN_SYNTHETIC_FEATURES = {
    "hour_of_day",
    "day_of_week",
    "distance_from_round_1000",
    "distance_from_round_500",
    "volatility_10",
    "volatility_30",
    "window_momentum",
}

TRAIN_HARD_ZERO_FEATURES = {
    "long_liq_volume",
    "short_liq_volume",
    "depth_imbalance_5",
}

HISTORICAL_NEUTRAL_PLACEHOLDERS = {
    "fear_greed",
    "coingecko_sentiment",
    "finnhub_sentiment",
    "lunarcrush_sentiment",
    "polymarket_flow",
    "latency_arb",
}

HISTORICAL_CONTEXT_DEPENDENT = {
    "funding_rate",
    "oi_momentum",
    "deribit_pcr",
    "options_advanced",
    "cross_market",
    "long_liq_volume",
    "short_liq_volume",
}

HISTORICAL_PROXY_MAPPED = {
    "orderbook_imbalance",
    "tick_velocity",
    "vwap_confluence",
    "price_divergence",
    "spike_detector",
    "rsi_macd_ha",
    "aggressor_ratio",
    "vpin_microstructure",
    "whale_flow",
    "news_sentiment",
    "btc_final_momentum",
    "kyle_lambda",
    "hawkes_flow",
}


@dataclass
class NumericSummary:
    present: int
    missing: int
    nonzero: int
    mean: float
    std: float
    min_value: float
    max_value: float
    unique_rounded: int
    zero_variance: bool
    all_zero: bool

    def as_json(self) -> dict[str, Any]:
        return {
            "present": self.present,
            "missing": self.missing,
            "coverage_pct": pct(self.present, self.present + self.missing),
            "nonzero": self.nonzero,
            "nonzero_pct": pct(self.nonzero, self.present),
            "mean": self.mean,
            "std": self.std,
            "min": self.min_value,
            "max": self.max_value,
            "unique_rounded": self.unique_rounded,
            "zero_variance": self.zero_variance,
            "all_zero": self.all_zero,
        }


def pct(num: int | float, den: int | float) -> float:
    if not den:
        return 0.0
    return float(num) / float(den) * 100.0


def parse_iso(ts: str) -> datetime:
    value = ts
    if value.endswith("Z"):
        value = value[:-1] + "+00:00"
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        return dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def load_python_feature_names(path: Path) -> list[str]:
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name) and target.id == "FEATURE_NAMES":
                    return list(ast.literal_eval(node.value))
    raise RuntimeError(f"FEATURE_NAMES not found in {path}")


def load_rust_feature_names(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(
        r"FEATURE_NAMES:\s*\[&str;\s*NUM_FEATURES\]\s*=\s*\[(.*?)\];",
        text,
        re.DOTALL,
    )
    if not match:
        raise RuntimeError(f"FEATURE_NAMES not found in {path}")
    return re.findall(r'"([^"]+)"', match.group(1))


def summarize_numeric(values: list[float], *, total_rows: int) -> NumericSummary:
    values = [value for value in values if math.isfinite(value)]
    present = len(values)
    missing = total_rows - present
    if not values:
        return NumericSummary(
            present=0,
            missing=missing,
            nonzero=0,
            mean=0.0,
            std=0.0,
            min_value=0.0,
            max_value=0.0,
            unique_rounded=0,
            zero_variance=True,
            all_zero=True,
        )
    rounded = {round(v, 10) for v in values}
    nonzero = sum(1 for v in values if abs(v) > 1e-12)
    return NumericSummary(
        present=present,
        missing=missing,
        nonzero=nonzero,
        mean=statistics.fmean(values),
        std=statistics.pstdev(values) if len(values) > 1 else 0.0,
        min_value=min(values),
        max_value=max(values),
        unique_rounded=len(rounded),
        zero_variance=len(rounded) <= 1,
        all_zero=nonzero == 0,
    )


def timestamp_series_report(
    conn: sqlite3.Connection, table: str, expected_gap_seconds: int
) -> dict[str, Any]:
    try:
        rows = conn.execute(f"SELECT timestamp FROM {table} ORDER BY timestamp").fetchall()
    except sqlite3.OperationalError:
        return {"status": "missing_table", "rows": 0, "duplicates": 0, "gap_events": 0, "largest_gap_seconds": None, "coverage_window_hours": None}
    values = sorted(parse_iso(row[0]).timestamp() for row in rows if row and row[0])
    if not values:
        return {
            "rows": 0,
            "duplicates": 0,
            "gap_events": 0,
            "largest_gap_seconds": 0.0,
            "coverage_window_hours": 0.0,
        }
    duplicates = 0
    last = None
    gap_events = 0
    largest_gap = 0.0
    for value in values:
        if last is not None:
            delta = value - last
            if delta <= 0:
                duplicates += 1
            if delta > expected_gap_seconds * 1.5:
                gap_events += 1
                largest_gap = max(largest_gap, delta)
        last = value
    return {
        "rows": len(values),
        "duplicates": duplicates,
        "gap_events": gap_events,
        "largest_gap_seconds": largest_gap,
        "coverage_window_hours": (values[-1] - values[0]) / 3600.0 if len(values) > 1 else 0.0,
    }


def table_count(conn: sqlite3.Connection, table: str) -> int:
    try:
        return int(conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0])
    except sqlite3.OperationalError:
        return 0


def feature_class(name: str) -> str:
    if name in TRAIN_SYNTHETIC_FEATURES:
        return "train_synthetic_override"
    if name in TRAIN_HARD_ZERO_FEATURES:
        return "train_hard_zero_placeholder"
    if name in HISTORICAL_NEUTRAL_PLACEHOLDERS:
        return "historical_neutral_placeholder"
    if name in HISTORICAL_CONTEXT_DEPENDENT:
        return "context_dependent_historical"
    if name in HISTORICAL_PROXY_MAPPED:
        return "projected_historical_proxy"
    return "direct_or_projected_historical"


def analyze_dataset(
    db_path: Path,
    canonical_features: list[str],
) -> dict[str, Any]:
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    feature_rows = conn.execute(
        "SELECT timestamp, market_id, features_json, regime, outcome, staleness_json "
        "FROM ml_features_timeseries WHERE outcome IS NOT NULL ORDER BY timestamp"
    ).fetchall()

    regimes = Counter()
    feature_values: dict[str, list[float]] = {name: [] for name in canonical_features}
    staleness_values: dict[str, list[float]] = {name: [] for name in canonical_features}
    context_values: dict[str, list[float]] = {name: [] for name in CONTEXT_KEYS}
    feature_key_counter: Counter[str] = Counter()
    row_key_counts: list[int] = []
    staleness_lengths: Counter[int] = Counter()
    staleness_all_zero_rows = 0

    timestamps: list[str] = []
    market_ids: list[str] = []

    for row in feature_rows:
        timestamps.append(str(row["timestamp"]))
        market_ids.append(str(row["market_id"]))
        regimes[str(row["regime"]).upper()] += 1

        try:
            payload = json.loads(row["features_json"])
        except (json.JSONDecodeError, TypeError):
            payload = {}
        if isinstance(payload, dict):
            feature_key_counter.update(payload.keys())
            row_key_counts.append(len(payload))
            for name in canonical_features:
                value = payload.get(name)
                if value is None:
                    continue
                try:
                    feature_values[name].append(float(value))
                except (TypeError, ValueError):
                    continue
            for name in CONTEXT_KEYS:
                value = payload.get(name)
                if value is None:
                    continue
                try:
                    context_values[name].append(float(value))
                except (TypeError, ValueError):
                    continue

        try:
            stale = json.loads(row["staleness_json"]) if row["staleness_json"] else []
        except (json.JSONDecodeError, TypeError):
            stale = []
        if isinstance(stale, list):
            staleness_lengths[len(stale)] += 1
            try:
                zero_stale = bool(stale) and all(math.isfinite(float(v)) and abs(float(v)) <= 1e-12 for v in stale)
            except (TypeError, ValueError):
                zero_stale = False
            if zero_stale:
                staleness_all_zero_rows += 1
            for idx, name in enumerate(canonical_features):
                if idx >= len(stale):
                    continue
                try:
                    staleness_values[name].append(float(stale[idx]))
                except (TypeError, ValueError):
                    continue

    total_rows = len(feature_rows)
    feature_reports = {
        name: {
            "class": feature_class(name),
            **summarize_numeric(feature_values[name], total_rows=total_rows).as_json(),
        }
        for name in canonical_features
    }
    context_reports = {
        name: summarize_numeric(context_values[name], total_rows=total_rows).as_json()
        for name in CONTEXT_KEYS
    }
    staleness_reports = {
        name: summarize_numeric(staleness_values[name], total_rows=total_rows).as_json()
        for name in canonical_features
    }

    zero_variance_features = [
        name for name, row in feature_reports.items() if row["zero_variance"]
    ]
    zero_variance_non_placeholder = [
        name
        for name, row in feature_reports.items()
        if row["zero_variance"] and row["class"] == "direct_or_projected_historical"
    ]
    strong_nonconstant_features = [
        name
        for name, row in feature_reports.items()
        if not row["zero_variance"] and row["nonzero_pct"] > 50.0
    ]
    explicitly_masked_features = [
        name
        for name, row in staleness_reports.items()
        if row["nonzero_pct"] > 99.0 and row["mean"] >= 0.99
    ]
    partially_masked_features = [
        name
        for name, row in staleness_reports.items()
        if 0.0 < row["nonzero_pct"] < 99.0
    ]

    market_dupes = total_rows - len(set(market_ids))

    tables = {
        "ml_features_timeseries": table_count(conn, "ml_features_timeseries"),
        "ml_candles_1m": table_count(conn, "ml_candles_1m"),
        "ml_orderbook_snapshots": table_count(conn, "ml_orderbook_snapshots"),
        "ml_tick_history": table_count(conn, "ml_tick_history"),
        "ml_aggressor_flow": table_count(conn, "ml_aggressor_flow"),
        "ml_liquidations": table_count(conn, "ml_liquidations"),
        "ml_funding_rates": table_count(conn, "ml_funding_rates"),
        "ml_open_interest_hist": table_count(conn, "ml_open_interest_hist"),
        "ml_long_short_ratios": table_count(conn, "ml_long_short_ratios"),
    }
    timelines = {
        "ml_candles_1m": timestamp_series_report(conn, "ml_candles_1m", 60),
        "ml_aggressor_flow": timestamp_series_report(conn, "ml_aggressor_flow", 60),
        "ml_features_timeseries": timestamp_series_report(conn, "ml_features_timeseries", 15 * 60),
    }
    conn.close()

    critical_missing_streams = [
        table
        for table in ("ml_orderbook_snapshots", "ml_tick_history", "ml_liquidations")
        if tables[table] == 0
    ]
    missing_runtime_context = [
        name for name, row in context_reports.items() if row["coverage_pct"] < 99.0
    ]
    constant_runtime_context = [
        name for name, row in context_reports.items() if row["zero_variance"]
    ]

    availability_masking = "explicit" if explicitly_masked_features or partially_masked_features else "implicit_zero_fill"

    # Matching names and populated tables cannot prove proxy features equal
    # live observations. Historical reconstruction remains research-only.
    fidelity = "partial"

    return {
        "db_path": str(db_path),
        "rows": total_rows,
        "distinct_market_ids": len(set(market_ids)),
        "duplicate_market_ids": market_dupes,
        "tables": tables,
        "timelines": timelines,
        "regimes": dict(regimes),
        "staleness": {
            "length_distribution": {str(k): v for k, v in sorted(staleness_lengths.items())},
            "all_zero_rows": staleness_all_zero_rows,
            "all_zero_rows_pct": pct(staleness_all_zero_rows, total_rows),
        },
        "feature_reports": feature_reports,
        "staleness_reports": staleness_reports,
        "context_reports": context_reports,
        "feature_key_coverage": {
            "median_keys_per_row": statistics.median(row_key_counts)
            if row_key_counts
            else 0.0,
            "min_keys_per_row": min(row_key_counts) if row_key_counts else 0,
            "max_keys_per_row": max(row_key_counts) if row_key_counts else 0,
            "unique_keys_seen": len(feature_key_counter),
        },
        "zero_variance_features": zero_variance_features,
        "zero_variance_non_placeholder": zero_variance_non_placeholder,
        "strong_nonconstant_features": strong_nonconstant_features,
        "explicitly_masked_features": explicitly_masked_features,
        "partially_masked_features": partially_masked_features,
        "availability_masking": availability_masking,
        "critical_missing_streams": critical_missing_streams,
        "missing_runtime_context": missing_runtime_context,
        "constant_runtime_context": constant_runtime_context,
        "historical_tensor_fidelity": fidelity,
        "runtime_equivalence_verified": False,
    }


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_model_input_validity_report.json"
    md_path = out_dir / f"{ts}_model_input_validity_report.md"

    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    lines = [
        "# Model Input Validity Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- contract_alignment_ok: {report['contract_alignment_ok']}",
        f"- overall_historical_fidelity: {report['overall_historical_fidelity']}",
        "",
    ]

    for dataset in report["datasets"]:
        lines.extend(
            [
                f"## {Path(dataset['db_path']).name}",
                "",
                f"- historical_tensor_fidelity: {dataset['historical_tensor_fidelity']}",
                f"- rows: {dataset['rows']}",
                f"- distinct_market_ids: {dataset['distinct_market_ids']}",
                f"- missing_streams: {', '.join(dataset['critical_missing_streams']) or 'none'}",
                f"- missing_runtime_context: {', '.join(dataset['missing_runtime_context']) or 'none'}",
                f"- constant_runtime_context: {', '.join(dataset['constant_runtime_context']) or 'none'}",
                f"- availability_masking: {dataset['availability_masking']}",
                f"- explicitly_masked_features: {', '.join(dataset['explicitly_masked_features'][:10]) or 'none'}",
                f"- zero_variance_non_placeholder: {', '.join(dataset['zero_variance_non_placeholder'][:8]) or 'none'}",
                f"- strong_nonconstant_features: {', '.join(dataset['strong_nonconstant_features'][:10]) or 'none'}",
                "",
            ]
        )

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Audit historical model-input fidelity")
    parser.add_argument(
        "--db",
        action="append",
        default=[],
        help="historical backfill SQLite DB (repeatable)",
    )
    parser.add_argument(
        "--output-dir",
        default="artifacts/data-validity-reports",
    )
    args = parser.parse_args()

    dbs = [Path(db) for db in args.db] if args.db else [
        ROOT / "artifacts/alpha-data/session45/backfill_14d_15m.db",
        ROOT / "artifacts/alpha-data/session46/backfill_30d_15m.db",
    ]

    rust_features = load_rust_feature_names(ROOT / "src/ml/model.rs")
    backfill_features = load_python_feature_names(ROOT / "training/backfill_historical.py")
    train_features = load_python_feature_names(ROOT / "training/train.py")

    contract_alignment_ok = rust_features == backfill_features == train_features
    datasets = [analyze_dataset(db, rust_features) for db in dbs]
    overall_fidelity = (
        "strong"
        if contract_alignment_ok
        and all(dataset["historical_tensor_fidelity"] == "strong" for dataset in datasets)
        else "partial"
    )

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "contract_alignment_ok": contract_alignment_ok,
        "contract_alignment": {
            "rust_feature_count": len(rust_features),
            "backfill_feature_count": len(backfill_features),
            "train_feature_count": len(train_features),
            "rust_equals_backfill": rust_features == backfill_features,
            "rust_equals_train": rust_features == train_features,
            "canonical_features": rust_features,
        },
        "overall_historical_fidelity": overall_fidelity,
        "datasets": datasets,
    }
    json_path, md_path = write_outputs(report, ROOT / args.output_dir)
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
