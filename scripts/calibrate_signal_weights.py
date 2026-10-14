#!/usr/bin/env python3
"""Fit signal weights from trades.signals_json and was_correct.

Requires: numpy, scikit-learn

Usage:
  python3 scripts/calibrate_signal_weights.py --db trades.db --out calibration.json
"""
from __future__ import annotations

import argparse
import json
import math
import sqlite3
import sys
from pathlib import Path
from typing import Any

try:
    import numpy as np
except ImportError:  # pragma: no cover
    print("error: numpy is required (pip install numpy)", file=sys.stderr)
    raise

try:
    from sklearn.linear_model import LogisticRegression
except ImportError:  # pragma: no cover
    print("error: scikit-learn is required (pip install scikit-learn)", file=sys.stderr)
    raise


def load_rows(conn: sqlite3.Connection) -> list[dict[str, Any]]:
    cur = conn.cursor()
    cur.execute(
        """
        SELECT direction, was_correct, confidence, raw_confidence, signals_json
        FROM trades
        WHERE status = 'CLOSED'
          AND exit_type IN ('auto_resolved', 'resolution')
          AND direction IN ('BUY_YES', 'BUY_NO')
          AND signals_json IS NOT NULL
          AND signals_json != ''
          AND was_correct IN (0, 1)
        """
    )
    rows = []
    for direction, was_correct, confidence, raw_confidence, signals_json in cur.fetchall():
        try:
            signals = json.loads(signals_json)
        except (json.JSONDecodeError, TypeError):
            continue
        if not isinstance(signals, dict) or not signals:
            continue
        try:
            signals = {str(name): float(value) for name, value in signals.items()}
        except (TypeError, ValueError, OverflowError):
            continue
        if not all(math.isfinite(value) for value in signals.values()):
            continue
        rows.append(
            {
                "direction": direction,
                "was_correct": int(was_correct),
                "confidence": raw_confidence if raw_confidence is not None else confidence,
                "signals": signals,
            }
        )
    return rows


def outcome_up(direction: str, was_correct: int) -> int:
    if direction == "BUY_YES":
        return 1 if was_correct == 1 else 0
    if direction == "BUY_NO":
        return 0 if was_correct == 1 else 1
    return 0


def build_dataset(rows: list[dict[str, Any]]) -> tuple[np.ndarray, np.ndarray, list[str]]:
    feature_names = sorted({k for r in rows for k in r["signals"].keys()})
    if not feature_names:
        return np.zeros((0, 0)), np.zeros((0,)), feature_names

    x = np.zeros((len(rows), len(feature_names)), dtype=float)
    y = np.zeros((len(rows),), dtype=float)
    for i, r in enumerate(rows):
        for j, name in enumerate(feature_names):
            x[i, j] = float(r["signals"].get(name, 0.0))
        y[i] = outcome_up(r["direction"], r["was_correct"])
    return x, y, feature_names


def main() -> int:
    parser = argparse.ArgumentParser(description="Fit signal weights from trade history")
    parser.add_argument("--db", default="trades.db", help="SQLite DB path")
    parser.add_argument("--min-trades", type=int, default=200)
    parser.add_argument("--out", default="", help="Output JSON file (optional)")
    args = parser.parse_args()

    if args.min_trades < 1:
        parser.error("--min-trades must be positive")
    try:
        conn = sqlite3.connect(Path(args.db).resolve().as_uri() + "?mode=ro", uri=True)
        try:
            rows = load_rows(conn)
        finally:
            conn.close()
    except sqlite3.Error as exc:
        print(f"error: cannot read trade history: {exc}", file=sys.stderr)
        return 1
    if len(rows) < args.min_trades:
        print(f"error: need at least {args.min_trades} trades, found {len(rows)}", file=sys.stderr)
        return 1

    x, y, feature_names = build_dataset(rows)
    if x.size == 0:
        print("error: no features found in signals_json", file=sys.stderr)
        return 1
    if len(np.unique(y)) < 2:
        print("error: calibration requires both resolved market outcomes", file=sys.stderr)
        return 1

    clf = LogisticRegression(max_iter=500, solver="lbfgs")
    clf.fit(x, y)
    probs = clf.predict_proba(x)[:, 1]
    brier = float(np.mean((probs - y) ** 2))

    coefs = clf.coef_[0]
    abs_coefs = np.abs(coefs)
    if abs_coefs.sum() == 0:
        print("error: zero coefficients", file=sys.stderr)
        return 1

    weights = {name: float(w) for name, w in zip(feature_names, abs_coefs / abs_coefs.sum())}
    logit_scale = float(abs_coefs.sum())

    payload = {
        "signal_weights": weights,
        "fusion_calibration": {
            "logit_scale": logit_scale,
        },
        "stats": {
            "trades": len(rows),
            "brier": brier,
            "brier_scope": "training_sample_only",
            "labels": "resolved_market_outcomes",
        },
    }

    if args.out:
        with open(args.out, "w", encoding="utf-8") as f:
            json.dump(payload, f, indent=2)
        print(f"wrote {args.out}")
    else:
        print(json.dumps(payload, indent=2))

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
