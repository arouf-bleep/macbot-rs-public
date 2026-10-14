#!/usr/bin/env python3
"""Train and evaluate a shadow-only meta-fusion learner on exact archived rows."""

from __future__ import annotations

import argparse
import json
import math
import re
import sqlite3
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

import numpy as np
from sklearn.linear_model import LogisticRegression
from sklearn.metrics import brier_score_loss, roc_auc_score
from sklearn.pipeline import Pipeline
from sklearn.preprocessing import StandardScaler


ROOT = Path(__file__).resolve().parent.parent
RUST_MODEL = ROOT / "src" / "ml" / "model.rs"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Evaluate a shadow meta-fusion learner")
    parser.add_argument("--db", default="trades.db", help="SQLite DB containing ml_model_inputs")
    parser.add_argument(
        "--min-rows",
        type=int,
        default=40,
        help="Minimum labeled rows required to train/evaluate",
    )
    parser.add_argument(
        "--test-fraction",
        type=float,
        default=0.30,
        help="Chronological holdout fraction",
    )
    parser.add_argument(
        "--output-dir",
        default="artifacts/meta-fusion-reports",
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


def load_rows(db_path: Path, feature_names: list[str]) -> list[dict[str, Any]]:
    if not db_path.is_file():
        raise FileNotFoundError(db_path)
    conn = sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True, timeout=30.0)
    conn.row_factory = sqlite3.Row
    has_table = conn.execute(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ml_model_inputs'"
    ).fetchone()[0]
    if not has_table:
        conn.close()
        return []

    rows = conn.execute(
        """
        SELECT
            timestamp,
            market_id,
            signal,
            regime_label,
            phase,
            fusion_score,
            p_up,
            features_json,
            outcome
        FROM ml_model_inputs
        WHERE outcome IN (0,1)
          AND COALESCE(run_id,'') NOT GLOB 'reconstructed:*'
          AND COALESCE(run_id,'') NOT GLOB 'backfill_*'
        ORDER BY timestamp
        """
    ).fetchall()
    conn.close()

    out: list[dict[str, Any]] = []
    for row in rows:
        try:
            features = json.loads(row["features_json"] or "{}")
        except (json.JSONDecodeError, TypeError):
            continue
        if not isinstance(features, dict):
            continue
        try:
            p_up = float(row["p_up"])
            score = float(row["fusion_score"])
            feature_values = {name: float(features[name]) for name in feature_names}
        except (ValueError, TypeError, KeyError):
            continue
        if not 0 <= p_up <= 1 or not math.isfinite(score) or not all(math.isfinite(v) for v in feature_values.values()):
            continue
        record = {
            "timestamp": row["timestamp"],
            "market_id": row["market_id"],
            "signal": str(row["signal"] or "unknown"),
            "regime_label": str(row["regime_label"] or "unknown"),
            "phase": str(row["phase"] or "unknown"),
            "fusion_score": score,
            "p_up": p_up,
            "outcome": float(row["outcome"]),
        }
        for feature_name in feature_names:
            record[f"feat::{feature_name}"] = feature_values[feature_name]
        out.append(record)
    return out


def one_hot(value: str, choices: list[str]) -> list[float]:
    return [1.0 if value == choice else 0.0 for choice in choices]


def chronological_market_split(rows: list[dict[str, Any]], fraction: float) -> tuple[list[int], list[int]]:
    if not math.isfinite(fraction) or not 0 < fraction < 1:
        raise ValueError("test fraction must be between zero and one")
    split = max(1, min(len(rows) - 1, int(round(len(rows) * (1 - fraction)))))
    test = list(range(split, len(rows)))
    test_markets = {rows[i]["market_id"] for i in test}
    train = [i for i in range(split) if rows[i]["market_id"] not in test_markets]
    if not train or not test:
        raise ValueError("not enough independent markets for a chronological holdout")
    return train, test


def build_matrix(rows: list[dict[str, Any]], feature_names: list[str]) -> tuple[np.ndarray, np.ndarray, list[str]]:
    signals = sorted({row["signal"] for row in rows})
    regimes = sorted({row["regime_label"] for row in rows})
    phases = sorted({row["phase"] for row in rows})

    columns = (
        ["p_up", "fusion_score_norm"]
        + [f"signal::{name}" for name in signals]
        + [f"regime::{name}" for name in regimes]
        + [f"phase::{name}" for name in phases]
        + [f"feat::{name}" for name in feature_names]
    )

    x: list[list[float]] = []
    y: list[float] = []
    for row in rows:
        vector = [
            float(row["p_up"]),
            float(row["fusion_score"]) / 100.0,
            *one_hot(row["signal"], signals),
            *one_hot(row["regime_label"], regimes),
            *one_hot(row["phase"], phases),
            *[float(row[f"feat::{name}"]) for name in feature_names],
        ]
        x.append(vector)
        y.append(float(row["outcome"]))
    return np.array(x, dtype=np.float32), np.array(y, dtype=np.float32), columns


def metric_block(y_true: np.ndarray, probs: np.ndarray) -> dict[str, float | None]:
    probs = np.clip(probs.astype(float), 1e-6, 1.0 - 1e-6)
    labels = y_true.astype(float)
    unique = set(float(v) for v in labels.tolist())
    return {
        "rows": int(len(labels)),
        "brier": float(brier_score_loss(labels, probs)) if len(labels) else None,
        "auc": float(roc_auc_score(labels, probs)) if len(unique) > 1 else None,
        "mean_p_up": float(np.mean(probs)) if len(probs) else None,
        "mean_outcome": float(np.mean(labels)) if len(labels) else None,
    }


def top_coefficients(columns: list[str], coefs: np.ndarray, top: int = 12) -> dict[str, list[dict[str, Any]]]:
    pairs = [{"feature": name, "coef": float(coef)} for name, coef in zip(columns, coefs)]
    positive = sorted([row for row in pairs if row["coef"] > 0.0], key=lambda row: row["coef"], reverse=True)
    negative = sorted([row for row in pairs if row["coef"] < 0.0], key=lambda row: row["coef"])
    return {
        "positive": positive[:top],
        "negative": negative[:top],
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Meta-Fusion Shadow Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- db: `{report['db']}`",
        f"- status: `{report['status']}`",
    ]
    if report["status"] != "ok":
        return "\n".join(lines) + "\n"

    base = report["baseline_test"]
    meta = report["meta_test"]
    lines.extend(
        [
            f"- train_rows: `{report['train_rows']}`",
            f"- test_rows: `{report['test_rows']}`",
            "",
            "## Test Metrics",
            "",
            f"- baseline_brier: `{base['brier']}`",
            f"- baseline_auc: `{base['auc']}`",
            f"- meta_brier: `{meta['brier']}`",
            f"- meta_auc: `{meta['auc']}`",
            f"- delta_brier: `{report['delta_brier']}`",
            f"- delta_auc: `{report['delta_auc']}`",
            "",
            "## Top Positive Coefficients",
            "",
        ]
    )
    for row in report["top_coefficients"]["positive"]:
        lines.append(f"- `{row['feature']}`: `{row['coef']:.4f}`")
    lines.extend(["", "## Top Negative Coefficients", ""])
    for row in report["top_coefficients"]["negative"]:
        lines.append(f"- `{row['feature']}`: `{row['coef']:.4f}`")
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    if not math.isfinite(args.test_fraction) or not 0 < args.test_fraction < 1 or args.min_rows < 2:
        raise SystemExit("test fraction must be between zero and one; min rows must be at least two")
    db_path = Path(args.db)
    if not db_path.is_absolute():
        db_path = (ROOT / db_path).resolve()
    output_dir = Path(args.output_dir)
    if not output_dir.is_absolute():
        output_dir = (ROOT / output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    feature_names = load_feature_names(RUST_MODEL)
    rows = load_rows(db_path, feature_names)
    report: dict[str, Any] = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "db": str(db_path),
    }

    if len(rows) < args.min_rows:
        report.update(
            {
                "status": "insufficient_rows",
                "rows": len(rows),
                "min_rows": args.min_rows,
                "summary_line": "Not enough labeled exact rows yet to train an honest meta-fusion learner.",
            }
        )
    else:
        x, y, columns = build_matrix(rows, feature_names)
        try:
            train_indices, test_indices = chronological_market_split(rows, args.test_fraction)
        except ValueError as exc:
            raise SystemExit(str(exc)) from exc
        x_train, x_test = x[train_indices], x[test_indices]
        y_train, y_test = y[train_indices], y[test_indices]
        if len(np.unique(y_train)) < 2:
            raise SystemExit("training markets contain only one outcome; cannot fit a binary learner")

        model = Pipeline(
            steps=[
                ("scale", StandardScaler()),
                (
                    "logreg",
                    LogisticRegression(
                        max_iter=2000,
                        C=0.5,
                        class_weight="balanced",
                        random_state=42,
                    ),
                ),
            ]
        )
        model.fit(x_train, y_train)
        meta_probs = model.predict_proba(x_test)[:, 1]
        baseline_probs = x_test[:, 0]

        coef = model.named_steps["logreg"].coef_[0]
        report.update(
            {
                "status": "ok",
                "rows": len(rows),
                "train_rows": int(len(y_train)),
                "test_rows": int(len(y_test)),
                "purged_shared_market_rows": len(rows) - len(train_indices) - len(test_indices),
                "baseline_test": metric_block(y_test, baseline_probs),
                "meta_test": metric_block(y_test, meta_probs),
                "delta_brier": float(brier_score_loss(y_test, baseline_probs) - brier_score_loss(y_test, meta_probs)),
                "delta_auc": (
                    float(roc_auc_score(y_test, meta_probs) - roc_auc_score(y_test, baseline_probs))
                    if len(set(float(v) for v in y_test.tolist())) > 1
                    else None
                ),
                "top_coefficients": top_coefficients(columns, coef),
                "summary_line": "Meta-fusion learner evaluated against archived p_up on a chronological holdout.",
            }
        )

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = output_dir / f"{stamp}_meta_fusion_shadow_report.json"
    md_path = output_dir / f"{stamp}_meta_fusion_shadow_report.md"
    latest_json_path = output_dir / "latest_meta_fusion_shadow_report.json"
    latest_md_path = output_dir / "latest_meta_fusion_shadow_report.md"
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
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
