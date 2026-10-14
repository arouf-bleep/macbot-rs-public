"""Scalar-only research diagnostic with market-disjoint chronological folds.

AUC describes discrimination on this archive; it does not establish trading
profitability, execution quality, or the absence of useful features.
"""

from __future__ import annotations

import json
import math
import sqlite3
import sys
from pathlib import Path

import numpy as np
from sklearn.ensemble import GradientBoostingClassifier
from sklearn.linear_model import LogisticRegression
from sklearn.metrics import roc_auc_score
from sklearn.preprocessing import StandardScaler

from evaluation_split import chronological_holdout
from materialize_model_inputs import FEATURE_NAMES


def load_samples(db_path):
    uri = Path(db_path).resolve().as_uri() + "?mode=ro"
    with sqlite3.connect(uri, uri=True) as conn:
        rows = conn.execute(
            "SELECT timestamp, market_id, features_json, outcome "
            "FROM ml_features_timeseries WHERE outcome IS NOT NULL"
        ).fetchall()
    samples = []
    for timestamp, market_id, encoded, outcome in rows:
        try:
            features = json.loads(encoded)
            if not isinstance(features, dict) or not any(key in features for key in FEATURE_NAMES):
                continue
            values = [float(features.get(key, 0.0)) for key in FEATURE_NAMES]
            label = float(outcome)
            if label not in (0.0, 1.0) or not np.isfinite(values).all() or not market_id:
                continue
            # Reuse the splitter's timestamp validation and normalization.
            from datetime import datetime, timezone
            parsed = datetime.fromisoformat(str(timestamp).replace("Z", "+00:00"))
            parsed = (parsed.replace(tzinfo=timezone.utc) if parsed.tzinfo is None else parsed).astimezone(timezone.utc)
        except (ValueError, TypeError, OverflowError):
            continue
        samples.append((parsed, values, label, {"timestamp": timestamp, "market_id": market_id}))
    samples.sort(key=lambda row: row[0])
    if len(samples) < 12:
        raise ValueError("at least twelve valid observations are required for this diagnostic")
    return (np.asarray([row[1] for row in samples]),
            np.asarray([row[2] for row in samples]),
            [row[3] for row in samples])


def evaluation_folds(metadata, count=5):
    size = len(metadata)
    width = size // (count + 1)
    if width < 2:
        raise ValueError("not enough observations for diagnostic folds")
    for fold in range(count):
        boundary = width * (fold + 1)
        stop = size if fold == count - 1 else boundary + width
        fraction = math.nextafter((stop - boundary) / stop, 0.0)
        try:
            split = chronological_holdout(metadata[:stop], fraction, embargo_seconds=900)
        except ValueError:
            continue
        yield np.asarray(split.development), np.asarray(split.holdout)


def main():
    db_path = sys.argv[1] if len(sys.argv) > 1 else "data/macbot.db"
    X, y, metadata = load_samples(db_path)
    print(f"Valid samples: {len(y)}, UP fraction: {y.mean():.3f}")
    scores = {"Logistic Regression": [], "Gradient Boosting": []}
    for train_idx, test_idx in evaluation_folds(metadata):
        if len(np.unique(y[train_idx])) != 2 or len(np.unique(y[test_idx])) != 2:
            continue
        scaler = StandardScaler().fit(X[train_idx])
        lr = LogisticRegression(max_iter=1000, C=1.0).fit(scaler.transform(X[train_idx]), y[train_idx])
        gb = GradientBoostingClassifier(n_estimators=100, max_depth=3, random_state=42).fit(X[train_idx], y[train_idx])
        scores["Logistic Regression"].append(roc_auc_score(y[test_idx], lr.predict_proba(scaler.transform(X[test_idx]))[:, 1]))
        scores["Gradient Boosting"].append(roc_auc_score(y[test_idx], gb.predict_proba(X[test_idx])[:, 1]))
    if not scores["Logistic Regression"]:
        raise ValueError("no evaluable folds remain after market/time purging and class checks")
    for name, aucs in scores.items():
        print(f"{name} AUC: {np.mean(aucs):.4f} (+/- {np.std(aucs):.4f}); folds={aucs}")
    print("Research diagnostic only; this does not demonstrate a trading edge.")


if __name__ == "__main__":
    main()
