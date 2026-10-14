#!/usr/bin/env python3
"""Audit live exact-input ML archive rows for contract parity and trainability."""

from __future__ import annotations

import argparse
import ast
import json
import math
import re
import sqlite3
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent
RUST_MODEL = ROOT / "src" / "ml" / "model.rs"
REGIME_CONTRACT = ROOT / "training" / "regime_contract.py"

EXPECTED_CANDLES_1M = 60 * 5
EXPECTED_CANDLES_5M = 24 * 5
EXPECTED_CANDLES_15M = 16 * 5
EXPECTED_ORDERBOOK = 12 * 45
EXPECTED_CONTEXT = 6
FLOAT_TOL = 1e-5


def parse_iso(ts: str) -> datetime:
    value = ts[:-1] + "+00:00" if ts.endswith("Z") else ts
    dt = datetime.fromisoformat(value)
    if dt.tzinfo is None:
        return dt.replace(tzinfo=timezone.utc)
    return dt.astimezone(timezone.utc)


def load_rust_feature_names(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8")
    match = re.search(
        r'FEATURE_NAMES:\s*\[&str;\s*NUM_FEATURES\]\s*=\s*\[(.*?)\];',
        text,
        re.DOTALL,
    )
    if not match:
        raise RuntimeError(f"FEATURE_NAMES not found in {path}")
    return re.findall(r'"([^"]+)"', match.group(1))


def parse_python_literal_assignment(path: Path, name: str):
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    for stmt in tree.body:
        if not isinstance(stmt, ast.Assign) or len(stmt.targets) != 1:
            continue
        target = stmt.targets[0]
        if isinstance(target, ast.Name) and target.id == name:
            return ast.literal_eval(stmt.value)
    raise RuntimeError(f"{name} not found in {path}")


def length_summary(counter: Counter[int]) -> dict[str, int]:
    return {str(k): int(v) for k, v in sorted(counter.items())}


def default_output_paths(db_path: Path) -> tuple[Path, Path]:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    out_dir = ROOT / "artifacts" / "data-validity-reports"
    out_dir.mkdir(parents=True, exist_ok=True)
    base = out_dir / f"{stamp}_{db_path.stem}_model_input_archive_report"
    return base.with_suffix(".json"), base.with_suffix(".md")


def analyze_archive(db_path: Path, feature_names: list[str]) -> dict[str, Any]:
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row

    has_table = conn.execute(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ml_model_inputs'"
    ).fetchone()[0]
    if not has_table:
        conn.close()
        return {
            "db_path": str(db_path),
            "status": "missing_table",
            "table": "ml_model_inputs",
        }

    columns = {
        row["name"]
        for row in conn.execute("PRAGMA table_info(ml_model_inputs)").fetchall()
    }
    outcome_expr = "outcome" if "outcome" in columns else "NULL AS outcome"
    rows = conn.execute(
        f"""
        SELECT timestamp, market_id, run_id, signal, regime_label, regime_value,
               features_json, scalars_json, staleness_json,
               candles_1m_json, candles_5m_json, candles_15m_json,
               orderbook_seq_json, {outcome_expr}
        FROM ml_model_inputs
        ORDER BY timestamp
        """
    ).fetchall()
    total_rows = len(rows)
    if total_rows == 0:
        conn.close()
        return {
            "db_path": str(db_path),
            "status": "empty",
            "rows": 0,
        }

    regime_map = parse_python_literal_assignment(REGIME_CONTRACT, "REGIME_FLOATS")
    default_regime = parse_python_literal_assignment(REGIME_CONTRACT, "DEFAULT_REGIME_FLOAT")

    scalars_lengths: Counter[int] = Counter()
    staleness_lengths: Counter[int] = Counter()
    candles_1m_lengths: Counter[int] = Counter()
    candles_5m_lengths: Counter[int] = Counter()
    candles_15m_lengths: Counter[int] = Counter()
    orderbook_lengths: Counter[int] = Counter()

    labeled_rows = 0
    reconstructed_rows = 0
    unknown_provenance_rows = 0
    invalid_outcome_rows = 0
    parse_error_rows = 0
    exact_dim_rows = 0
    aligned_scalar_rows = 0
    regime_match_rows = 0
    max_scalar_prefix_diff = 0.0
    worst_scalar_row: dict[str, Any] | None = None

    timestamps: list[str] = []
    market_ids: set[str] = set()
    signals: Counter[str] = Counter()
    regimes: Counter[str] = Counter()

    for row in rows:
        reconstructed_rows += str(row["run_id"] or "").startswith(("reconstructed:", "backfill_"))
        unknown_provenance_rows += not str(row["run_id"] or "").strip()
        timestamps.append(row["timestamp"])
        market_ids.add(row["market_id"])
        signals[row["signal"]] += 1
        regimes[row["regime_label"]] += 1
        if row["outcome"] in (0.0, 1.0):
            labeled_rows += 1
        elif row["outcome"] is not None:
            invalid_outcome_rows += 1

        try:
            features = json.loads(row["features_json"])
            scalars = json.loads(row["scalars_json"])
            staleness = json.loads(row["staleness_json"])
            candles_1m = json.loads(row["candles_1m_json"])
            candles_5m = json.loads(row["candles_5m_json"])
            candles_15m = json.loads(row["candles_15m_json"])
            orderbook = json.loads(row["orderbook_seq_json"])
            if not isinstance(features, dict) or not all(isinstance(tensor, list) for tensor in (scalars, staleness, candles_1m, candles_5m, candles_15m, orderbook)):
                raise ValueError("invalid tensor or feature container")
            for tensor in (scalars, staleness, candles_1m, candles_5m, candles_15m, orderbook):
                if any(isinstance(value, bool) or not math.isfinite(float(value)) for value in tensor):
                    raise ValueError("invalid numeric tensor")
            if any(not math.isfinite(float(value)) for value in features.values()):
                raise ValueError("invalid numeric feature")
            parse_iso(str(row["timestamp"]))
            if not math.isfinite(float(row["regime_value"])):
                raise ValueError("invalid regime value")
        except Exception:
            parse_error_rows += 1
            continue

        scalars_lengths[len(scalars)] += 1
        staleness_lengths[len(staleness)] += 1
        candles_1m_lengths[len(candles_1m)] += 1
        candles_5m_lengths[len(candles_5m)] += 1
        candles_15m_lengths[len(candles_15m)] += 1
        orderbook_lengths[len(orderbook)] += 1

        dims_exact = (
            len(scalars) == len(feature_names) + EXPECTED_CONTEXT
            and len(staleness) == len(feature_names)
            and len(candles_1m) == EXPECTED_CANDLES_1M
            and len(candles_5m) == EXPECTED_CANDLES_5M
            and len(candles_15m) == EXPECTED_CANDLES_15M
            and len(orderbook) == EXPECTED_ORDERBOOK
        )
        if dims_exact:
            exact_dim_rows += 1

        row_max_diff = 0.0
        scalar_alignment_ok = len(scalars) >= len(feature_names) and all(name in features for name in feature_names)
        if scalar_alignment_ok:
            for idx, name in enumerate(feature_names):
                expected = float(features.get(name, 0.0))
                actual = float(scalars[idx])
                diff = abs(actual - expected)
                row_max_diff = max(row_max_diff, diff)
            if row_max_diff <= FLOAT_TOL:
                aligned_scalar_rows += 1
            if row_max_diff > max_scalar_prefix_diff:
                max_scalar_prefix_diff = row_max_diff
                worst_scalar_row = {
                    "timestamp": row["timestamp"],
                    "market_id": row["market_id"],
                    "max_scalar_prefix_diff": row_max_diff,
                }

        expected_regime = float(regime_map.get(row["regime_label"], default_regime))
        if abs(float(row["regime_value"]) - expected_regime) <= FLOAT_TOL:
            regime_match_rows += 1

    valid_timestamps = []
    for timestamp in timestamps:
        try:
            valid_timestamps.append(parse_iso(str(timestamp)))
        except (ValueError, TypeError):
            continue
    first_ts = min(valid_timestamps) if valid_timestamps else None
    last_ts = max(valid_timestamps) if valid_timestamps else None
    coverage_hours = (last_ts - first_ts).total_seconds() / 3600.0 if first_ts and last_ts else 0.0

    conn.close()
    return {
        "db_path": str(db_path),
        "status": "ok",
        "rows": total_rows,
        "labeled_rows": labeled_rows,
        "invalid_outcome_rows": invalid_outcome_rows,
        "reconstructed_rows": reconstructed_rows,
        "unknown_provenance_rows": unknown_provenance_rows,
        "runtime_recorded_rows": total_rows - reconstructed_rows - unknown_provenance_rows,
        "runtime_provenance_only": reconstructed_rows == 0 and unknown_provenance_rows == 0,
        "contract_valid": parse_error_rows == 0 and invalid_outcome_rows == 0 and exact_dim_rows == aligned_scalar_rows == regime_match_rows == total_rows,
        "labeled_pct": (labeled_rows / total_rows) * 100.0,
        "distinct_markets": len(market_ids),
        "signals": dict(signals.most_common()),
        "regimes": dict(regimes.most_common()),
        "time_window": {
            "first_timestamp": first_ts.isoformat() if first_ts else None,
            "last_timestamp": last_ts.isoformat() if last_ts else None,
            "coverage_hours": coverage_hours,
        },
        "expected_dimensions": {
            "scalars": len(feature_names) + EXPECTED_CONTEXT,
            "staleness": len(feature_names),
            "candles_1m": EXPECTED_CANDLES_1M,
            "candles_5m": EXPECTED_CANDLES_5M,
            "candles_15m": EXPECTED_CANDLES_15M,
            "orderbook_seq": EXPECTED_ORDERBOOK,
        },
        "observed_dimensions": {
            "scalars": length_summary(scalars_lengths),
            "staleness": length_summary(staleness_lengths),
            "candles_1m": length_summary(candles_1m_lengths),
            "candles_5m": length_summary(candles_5m_lengths),
            "candles_15m": length_summary(candles_15m_lengths),
            "orderbook_seq": length_summary(orderbook_lengths),
        },
        "contract_rows": {
            "exact_dimension_rows": exact_dim_rows,
            "exact_dimension_pct": (exact_dim_rows / total_rows) * 100.0,
            "scalar_prefix_aligned_rows": aligned_scalar_rows,
            "scalar_prefix_aligned_pct": (aligned_scalar_rows / total_rows) * 100.0,
            "regime_value_matched_rows": regime_match_rows,
            "regime_value_matched_pct": (regime_match_rows / total_rows) * 100.0,
            "parse_error_rows": parse_error_rows,
        },
        "max_scalar_prefix_diff": max_scalar_prefix_diff,
        "worst_scalar_row": worst_scalar_row,
    }


def render_markdown(report: dict[str, Any]) -> str:
    if report["status"] != "ok":
        return "\n".join(
            [
                "# Model Input Archive Report",
                "",
                f"- db_path: `{report['db_path']}`",
                f"- status: `{report['status']}`",
            ]
        )

    return "\n".join(
        [
            "# Model Input Archive Report",
            "",
            f"- db_path: `{report['db_path']}`",
            f"- rows: `{report['rows']}`",
            f"- contract_valid: `{report['contract_valid']}`",
            f"- runtime_recorded_rows: `{report['runtime_recorded_rows']}`",
            f"- reconstructed_rows: `{report['reconstructed_rows']}`",
            f"- unknown_provenance_rows: `{report['unknown_provenance_rows']}`",
            f"- runtime_provenance_only: `{report['runtime_provenance_only']}`",
            f"- invalid_outcome_rows: `{report['invalid_outcome_rows']}`",
            f"- labeled_rows: `{report['labeled_rows']}` (`{report['labeled_pct']:.1f}%`)",
            f"- distinct_markets: `{report['distinct_markets']}`",
            f"- time_window: `{report['time_window']['first_timestamp']}` → `{report['time_window']['last_timestamp']}` (`{report['time_window']['coverage_hours']:.2f}h`)",
            f"- exact_dimension_rows: `{report['contract_rows']['exact_dimension_rows']}` (`{report['contract_rows']['exact_dimension_pct']:.1f}%`)",
            f"- scalar_prefix_aligned_rows: `{report['contract_rows']['scalar_prefix_aligned_rows']}` (`{report['contract_rows']['scalar_prefix_aligned_pct']:.1f}%`)",
            f"- regime_value_matched_rows: `{report['contract_rows']['regime_value_matched_rows']}` (`{report['contract_rows']['regime_value_matched_pct']:.1f}%`)",
            f"- parse_error_rows: `{report['contract_rows']['parse_error_rows']}`",
            f"- max_scalar_prefix_diff: `{report['max_scalar_prefix_diff']:.8f}`",
            "",
            "## Signals",
            *(f"- `{k}`: `{v}`" for k, v in report["signals"].items()),
            "",
            "## Regimes",
            *(f"- `{k}`: `{v}`" for k, v in report["regimes"].items()),
            "",
            "## Observed Dimensions",
            *(f"- `{name}`: `{dims}`" for name, dims in report["observed_dimensions"].items()),
        ]
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, help="Path to SQLite DB with ml_model_inputs")
    parser.add_argument("--output-json", help="Optional JSON output path")
    parser.add_argument("--output-md", help="Optional Markdown output path")
    args = parser.parse_args()

    feature_names = load_rust_feature_names(RUST_MODEL)
    db_path = Path(args.db).resolve()
    report = analyze_archive(db_path, feature_names)

    output_json, output_md = default_output_paths(db_path)
    if args.output_json:
        output_json = Path(args.output_json).resolve()
        output_json.parent.mkdir(parents=True, exist_ok=True)
    if args.output_md:
        output_md = Path(args.output_md).resolve()
        output_md.parent.mkdir(parents=True, exist_ok=True)

    output_json.write_text(json.dumps(report, indent=2, sort_keys=True), encoding="utf-8")
    output_md.write_text(render_markdown(report) + "\n", encoding="utf-8")

    print(f"Wrote JSON report to {output_json}")
    print(f"Wrote Markdown report to {output_md}")


if __name__ == "__main__":
    main()
