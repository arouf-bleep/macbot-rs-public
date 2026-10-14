#!/usr/bin/env python3
"""Evaluate a deep-model ONNX artifact by regime on the chronological holdout.

Also reports whether the model improves a narrow specialist lane such as
TREND_UP + YES by filtering for higher-confidence predictions.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from pathlib import Path

import numpy as np
from sklearn.metrics import brier_score_loss, roc_auc_score


ROOT = Path(__file__).resolve().parents[1]
TRAINING_DIR = ROOT / "training"
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))
if str(TRAINING_DIR) not in sys.path:
    sys.path.insert(0, str(TRAINING_DIR))

import onnxruntime as ort  # noqa: E402
from training.train import BTCDataset  # noqa: E402


def csv_floats(raw: str) -> list[float]:
    values: list[float] = []
    for part in raw.split(","):
        s = part.strip()
        if s:
            values.append(float(s))
    return values


def compute_metrics(labels: np.ndarray, preds: np.ndarray) -> dict[str, float | int | None]:
    if len(labels) == 0:
        return {
            "samples": 0,
            "base_rate": None,
            "avg_pred": None,
            "brier": None,
            "auc": None,
            "accuracy": None,
        }
    labels = labels.astype(np.float32)
    preds = preds.astype(np.float32)
    out: dict[str, float | int | None] = {
        "samples": int(len(labels)),
        "base_rate": float(labels.mean()),
        "avg_pred": float(preds.mean()),
        "brier": float(brier_score_loss(labels, preds)),
        "accuracy": float(np.mean((preds >= 0.5) == (labels >= 0.5))),
    }
    try:
        out["auc"] = float(roc_auc_score(labels, preds)) if len(np.unique(labels)) > 1 else None
    except ValueError:
        out["auc"] = None
    return out


def load_holdout_predictions(
    db_path: str,
    model_path: str,
    holdout_ratio: float,
    head: str,
    batch_size: int,
) -> tuple[BTCDataset, list[int], np.ndarray]:
    if not math.isfinite(holdout_ratio) or not 0 < holdout_ratio < 1 or batch_size <= 0:
        raise ValueError("holdout ratio must be between zero and one; batch size must be positive")
    dataset = BTCDataset(db_path)
    n = len(dataset.samples)
    holdout_start = max(0, min(n, int(n * (1.0 - holdout_ratio))))
    indices = list(range(holdout_start, n))
    ort.disable_telemetry_events()
    session = ort.InferenceSession(model_path, providers=["CPUExecutionProvider"])
    input_names = {inp.name for inp in session.get_inputs()}
    output_names = [out.name for out in session.get_outputs()]
    if head not in output_names:
        raise SystemExit(
            f"requested head '{head}' not in ONNX outputs {output_names}"
        )

    preds: list[np.ndarray] = []
    for start in range(0, len(indices), batch_size):
        batch_idx = indices[start : start + batch_size]
        feed: dict[str, np.ndarray] = {}

        def stack(key: str) -> np.ndarray:
            return np.stack([dataset.samples[i][key] for i in batch_idx]).astype(np.float32)

        if "candles_1m" in input_names:
            feed["candles_1m"] = stack("candles_1m")
        if "candles_5m" in input_names:
            feed["candles_5m"] = stack("candles_5m")
        if "candles_15m" in input_names:
            feed["candles_15m"] = stack("candles_15m")
        if "orderbook_seq" in input_names:
            feed["orderbook_seq"] = stack("orderbook_seq")
        if "scalars" in input_names:
            feed["scalars"] = stack("scalars")
        if "staleness" in input_names:
            feed["staleness"] = stack("staleness")
        if "regime" in input_names:
            feed["regime"] = stack("regime")

        outputs = session.run(None, feed)
        out_map = dict(zip(output_names, outputs))
        preds.append(np.asarray(out_map[head], dtype=np.float32).reshape(-1))

    all_preds = np.concatenate(preds) if preds else np.empty((0,), dtype=np.float32)
    if len(all_preds) != len(indices) or not np.all(np.isfinite(all_preds)) or not np.all((all_preds >= 0) & (all_preds <= 1)):
        raise ValueError("model did not return one finite probability per evaluation row")
    return dataset, indices, all_preds


def specialist_confidence(preds: np.ndarray, side: str) -> np.ndarray:
    if side == "yes_only":
        return preds
    if side == "no_only":
        return 1.0 - preds
    raise ValueError(f"unsupported specialist side '{side}'")


def specialist_wins(labels: np.ndarray, side: str) -> np.ndarray:
    if side == "yes_only":
        return labels
    if side == "no_only":
        return 1.0 - labels
    raise ValueError(f"unsupported specialist side '{side}'")


def main() -> int:
    p = argparse.ArgumentParser(description="Deep-model regime holdout report")
    p.add_argument("--db", required=True, help="historical SQLite DB")
    p.add_argument("--model", required=True, help="ONNX artifact path")
    p.add_argument("--head", choices=("p_up", "calibration"), default="p_up")
    p.add_argument("--holdout-ratio", type=float, default=0.15)
    p.add_argument("--batch-size", type=int, default=256)
    p.add_argument("--specialist-regime", default="TREND_UP")
    p.add_argument("--specialist-side", choices=("yes_only", "no_only"), default="yes_only")
    p.add_argument("--thresholds", default="0.50,0.52,0.54,0.56,0.58,0.60")
    p.add_argument("--min-selected", type=int, default=20)
    p.add_argument("--output-json", default="")
    args = p.parse_args()

    thresholds = csv_floats(args.thresholds)
    if not thresholds:
        raise SystemExit("no thresholds provided")

    dataset, holdout_idx, preds = load_holdout_predictions(
        db_path=args.db,
        model_path=args.model,
        holdout_ratio=args.holdout_ratio,
        head=args.head,
        batch_size=args.batch_size,
    )

    labels = np.array(
        [float(dataset.samples[i]["label_p_up"]) for i in holdout_idx],
        dtype=np.float32,
    )
    regimes = [
        str(dataset.meta[i]["regime_label"]).upper()
        for i in holdout_idx
    ]

    global_metrics = compute_metrics(labels, preds)
    by_regime: dict[str, dict[str, float | int | None]] = {}
    available_regimes = sorted(set(regimes))
    for regime in available_regimes:
        mask = np.array([r == regime for r in regimes], dtype=bool)
        by_regime[regime] = compute_metrics(labels[mask], preds[mask])

    target_regime = args.specialist_regime.strip().upper()
    eligible = np.array([r == target_regime for r in regimes], dtype=bool)
    eligible_count = int(eligible.sum())
    eligible_labels = specialist_wins(labels[eligible], args.specialist_side)
    eligible_conf = specialist_confidence(preds[eligible], args.specialist_side)
    baseline_win_rate = float(eligible_labels.mean()) if eligible_count else None

    threshold_rows: list[dict[str, float | int | None]] = []
    for th in thresholds:
        conf = specialist_confidence(preds, args.specialist_side)
        selected = eligible & (conf >= th)
        selected_count = int(selected.sum())
        selected_labels = specialist_wins(labels[selected], args.specialist_side)
        selected_conf = specialist_confidence(preds[selected], args.specialist_side)
        win_rate = float(selected_labels.mean()) if selected_count else None
        row: dict[str, float | int | None] = {
            "threshold": float(th),
            "eligible": eligible_count,
            "selected": selected_count,
            "coverage": float(selected_count / eligible_count) if eligible_count else None,
            "baseline_win_rate": baseline_win_rate,
            "selected_win_rate": win_rate,
            "win_rate_lift_pp": (
                float((win_rate - baseline_win_rate) * 100.0)
                if win_rate is not None and baseline_win_rate is not None
                else None
            ),
            "avg_confidence": float(selected_conf.mean()) if selected_count else None,
            "selected_brier": (
                float(brier_score_loss(selected_labels, selected_conf))
                if selected_count
                else None
            ),
        }
        threshold_rows.append(row)

    viable = [
        row for row in threshold_rows if (row["selected"] or 0) >= args.min_selected
    ]
    viable.sort(
        key=lambda row: (
            row["win_rate_lift_pp"] if row["win_rate_lift_pp"] is not None else float("-inf"),
            row["selected_win_rate"] if row["selected_win_rate"] is not None else float("-inf"),
            row["selected"] if row["selected"] is not None else 0,
        ),
        reverse=True,
    )

    report = {
        "methodology": "chronological evaluation slice; model training isolation not verified; selected thresholds reuse this slice",
        "db": str(Path(args.db)),
        "model": str(Path(args.model)),
        "head": args.head,
        "holdout_ratio": args.holdout_ratio,
        "holdout_start_index": int(len(dataset.samples) * (1.0 - args.holdout_ratio)),
        "holdout_samples": len(holdout_idx),
        "available_regimes": available_regimes,
        "global": global_metrics,
        "by_regime": by_regime,
        "specialist": {
            "regime": target_regime,
            "side": args.specialist_side,
            "available_regimes": available_regimes,
            "baseline_eligible": eligible_count,
            "baseline_win_rate": baseline_win_rate,
            "threshold_rows": threshold_rows,
            "best_viable_threshold": viable[0] if viable else None,
        },
    }

    if args.output_json:
        out_path = Path(args.output_json)
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
