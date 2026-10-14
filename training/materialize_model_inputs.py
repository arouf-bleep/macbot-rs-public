#!/usr/bin/env python3
"""Materialize reconstructed research inputs from historical feature archives.

These are not recordings of the tensors used by a running model. Missing
orderbooks and live context cannot be recovered from historical candles.
"""

from __future__ import annotations

import argparse
import ast
import datetime
import json
import re
import sqlite3
from pathlib import Path

import numpy as np

from regime_contract import regime_value_from_label
from sqlite_runtime import connect_for_write


ROOT = Path(__file__).resolve().parents[1]
RUST_MODEL = ROOT / "src" / "ml" / "model.rs"
NUM_FEATURES = 34
NUM_CONTEXT = 6


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


FEATURE_NAMES = load_rust_feature_names(RUST_MODEL)


def _aggregate_candles(candles: np.ndarray, period: int) -> np.ndarray:
    n = len(candles)
    if n == 0:
        return np.zeros((0, 5), dtype=np.float32)
    assert n % period == 0, f"candles len {n} must divide by period {period}"
    bars = []
    for start in range(0, n, period):
        chunk = candles[start:start + period]
        chunk = chunk[~np.all(chunk == 0, axis=1)]
        if not len(chunk):
            bars.append([0.0] * 5)
            continue
        bars.append(
            [
                float(chunk[0, 0]),
                float(np.max(chunk[:, 1])),
                float(np.min(chunk[:, 2])),
                float(chunk[-1, 3]),
                float(np.sum(chunk[:, 4])),
            ]
        )
    return np.array(bars, dtype=np.float32)


def ensure_model_input_schema(conn: sqlite3.Connection) -> None:
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS ml_model_inputs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            run_id TEXT NOT NULL,
            market_id TEXT NOT NULL,
            signal TEXT NOT NULL,
            regime_label TEXT NOT NULL,
            phase TEXT NOT NULL,
            fusion_score REAL NOT NULL,
            p_up REAL NOT NULL,
            regime_value REAL NOT NULL,
            features_json TEXT NOT NULL,
            scalars_json TEXT NOT NULL,
            staleness_json TEXT NOT NULL,
            candles_1m_json TEXT NOT NULL,
            candles_5m_json TEXT NOT NULL,
            candles_15m_json TEXT NOT NULL,
            orderbook_seq_json TEXT NOT NULL,
            outcome REAL
        )
        """
    )
    conn.execute("CREATE INDEX IF NOT EXISTS idx_ml_model_inputs_ts ON ml_model_inputs(timestamp)")
    conn.execute("CREATE INDEX IF NOT EXISTS idx_ml_model_inputs_market ON ml_model_inputs(market_id)")
    cols = [row[1] for row in conn.execute("PRAGMA table_info(ml_model_inputs)").fetchall()]
    if "outcome" not in cols:
        conn.execute("ALTER TABLE ml_model_inputs ADD COLUMN outcome REAL")
    conn.commit()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", required=True, help="Path to historical SQLite DB")
    parser.add_argument("--days", type=int, default=0, help="Only materialize the last N days")
    parser.add_argument("--limit", type=int, default=0, help="Optional maximum row count")
    parser.add_argument("--clear-existing", action="store_true", help="Delete reconstructed research inputs first; preserve runtime recordings")
    args = parser.parse_args()

    conn = connect_for_write(args.db, operation="Model-input materialization")
    ensure_model_input_schema(conn)
    if args.clear_existing:
        conn.execute("DELETE FROM ml_model_inputs WHERE run_id LIKE 'reconstructed:%'")
        conn.commit()

    if args.days > 0:
        cutoff = (datetime.datetime.utcnow() - datetime.timedelta(days=args.days)).isoformat()
        feature_rows = conn.execute(
            """
            SELECT timestamp, run_id, market_id, features_json, fusion_score, p_up,
                   signal, regime, phase, outcome, staleness_json
            FROM ml_features_timeseries
            WHERE outcome IS NOT NULL AND timestamp >= ?
            ORDER BY timestamp
            """,
            (cutoff,),
        ).fetchall()
        print(f"Filtered to last {args.days} days (cutoff={cutoff})")
    else:
        feature_rows = conn.execute(
            """
            SELECT timestamp, run_id, market_id, features_json, fusion_score, p_up,
                   signal, regime, phase, outcome, staleness_json
            FROM ml_features_timeseries
            WHERE outcome IS NOT NULL
            ORDER BY timestamp
            """
        ).fetchall()

    all_candles_raw = conn.execute(
        """
        SELECT timestamp, MIN(open), MAX(high), MIN(low), MIN(close), MIN(volume)
        FROM ml_candles_1m
        GROUP BY timestamp
        ORDER BY timestamp
        """
    ).fetchall()
    def utc_timestamp(value):
        timestamp = datetime.datetime.fromisoformat(str(value).replace("Z", "+00:00"))
        if timestamp.tzinfo is None:
            timestamp = timestamp.replace(tzinfo=datetime.timezone.utc)
        return timestamp.astimezone(datetime.timezone.utc)
    all_candles_raw.sort(key=lambda row: utc_timestamp(row[0]))
    # Historical rows contain the final close, known only after the minute ends.
    all_candle_ts = [utc_timestamp(r[0]) + datetime.timedelta(minutes=1) for r in all_candles_raw]
    all_candle_data = np.array([(r[1], r[2], r[3], r[4], r[5]) for r in all_candles_raw], dtype=np.float32)
    print(f"Loaded {len(all_candle_data)} BTC 1m candles")

    import bisect

    inserted = 0
    skipped = 0
    for row in feature_rows:
        if args.limit and inserted >= args.limit:
            break

        feat_ts, run_id, market_id, features_json_str, fusion_score, p_up, signal, regime_str, phase, outcome, staleness_json = row
        if conn.execute("SELECT 1 FROM ml_model_inputs WHERE timestamp=? AND run_id=? AND market_id=? AND signal=? LIMIT 1",
                        (feat_ts, f"reconstructed:{run_id}", market_id, signal)).fetchone():
            skipped += 1
            continue
        idx = bisect.bisect_right(all_candle_ts, utc_timestamp(feat_ts))
        if idx < 60:
            skipped += 1
            continue

        candle_data = np.array(all_candle_data[max(0, idx - 240):idx], dtype=np.float32)
        n_candles = len(candle_data)
        if n_candles < 60:
            skipped += 1
            continue

        c1m = candle_data[-60:]
        c5m_raw = candle_data[-120:] if n_candles >= 120 else np.concatenate(
            [np.zeros((120 - n_candles, 5), dtype=np.float32), candle_data]
        )
        c15m_raw = candle_data[-240:] if n_candles >= 240 else np.concatenate(
            [np.zeros((240 - n_candles, 5), dtype=np.float32), candle_data]
        )
        c5m = _aggregate_candles(c5m_raw, 5)
        c15m = _aggregate_candles(c15m_raw, 15)
        ob_seq = np.zeros((12, 45), dtype=np.float32)

        try:
            feat_dict = json.loads(features_json_str)
        except (json.JSONDecodeError, TypeError):
            skipped += 1
            continue
        if not isinstance(feat_dict, dict):
            skipped += 1
            continue

        feature_map = {}
        for key, value in feat_dict.items():
            try:
                feature_map[key] = float(value)
            except (TypeError, ValueError):
                continue

        btc_price = float(candle_data[-1, 3])
        context_keys = [
            "time_remaining",
            "balance",
            "positions",
            "contract_price",
            "price_discovery_lag",
            "btc_price",
        ]
        context_defaults = {"btc_price": btc_price}
        try:
            context_feats = [float(feat_dict.get(k, context_defaults.get(k, 0.0))) for k in context_keys]
            fusion_score = float(fusion_score)
            p_up = float(p_up)
        except (TypeError, ValueError, OverflowError):
            skipped += 1
            continue
        if not np.isfinite(fusion_score) or not np.isfinite(p_up) or not 0.0 <= p_up <= 1.0:
            skipped += 1
            continue

        try:
            ts_dt = datetime.datetime.fromisoformat(feat_ts.replace("Z", "+00:00") if feat_ts.endswith("Z") else feat_ts)
            hour_of_day = ts_dt.hour / 23.0
            day_of_week = ts_dt.weekday() / 6.0
        except Exception:
            hour_of_day = 0.0
            day_of_week = 0.0

        rem_1000 = btc_price % 1000.0
        rem_500 = btc_price % 500.0
        dist_1000 = (500.0 - abs(rem_1000 - 500.0)) / 500.0
        dist_500 = (250.0 - abs(rem_500 - 250.0)) / 250.0

        closes = candle_data[:, 3]

        def _vol(c: np.ndarray, window: int) -> float:
            if len(c) < window + 1:
                return 0.0
            rets = np.diff(c[-window - 1:]) / (c[-window - 1:-1] + 1e-10)
            return float(np.std(rets)) if len(rets) > 0 else 0.0

        vol_10 = _vol(closes, 10)
        vol_30 = _vol(closes, 30)
        window_mom = (
            max(-1.0, min(1.0, ((float(candle_data[-1, 3]) - float(candle_data[0, 0])) / (float(candle_data[0, 0]) + 1e-10)) / 0.005))
            if n_candles >= 2
            else 0.0
        )

        feature_map.update(
            {
                "hour_of_day": hour_of_day,
                "day_of_week": day_of_week,
                "distance_from_round_1000": dist_1000,
                "distance_from_round_500": dist_500,
                "volatility_10": vol_10,
                "volatility_30": vol_30,
                "window_momentum": window_mom,
                "long_liq_volume": 0.0,
                "short_liq_volume": 0.0,
                "depth_imbalance_5": 0.0,
            }
        )

        scalars = np.array([float(feature_map.get(name, 0.0)) for name in FEATURE_NAMES] + context_feats, dtype=np.float32)
        if staleness_json:
            try:
                staleness = np.array(json.loads(staleness_json), dtype=np.float32)
            except (json.JSONDecodeError, TypeError, ValueError, OverflowError):
                skipped += 1
                continue
        else:
            staleness = np.zeros(NUM_FEATURES, dtype=np.float32)
        if staleness.ndim != 1:
            skipped += 1
            continue
        if len(staleness) < NUM_FEATURES:
            staleness = np.pad(staleness, (0, NUM_FEATURES - len(staleness)))
        elif len(staleness) > NUM_FEATURES:
            staleness = staleness[:NUM_FEATURES]

        regime_label = str(regime_str).upper()
        if outcome not in (0.0, 1.0) or not all(np.isfinite(tensor).all() for tensor in (scalars, staleness, c1m, c5m, c15m)):
            skipped += 1
            continue

        conn.execute(
            """
            INSERT INTO ml_model_inputs (
                timestamp, run_id, market_id, signal, regime_label, phase,
                fusion_score, p_up, regime_value, features_json, scalars_json,
                staleness_json, candles_1m_json, candles_5m_json, candles_15m_json,
                orderbook_seq_json, outcome
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            """,
            (
                feat_ts,
                f"reconstructed:{run_id}",
                market_id,
                signal,
                regime_label,
                phase,
                float(fusion_score),
                float(p_up),
                float(regime_value_from_label(regime_label)),
                json.dumps(feature_map, sort_keys=True),
                json.dumps(scalars.tolist()),
                json.dumps(staleness.tolist()),
                json.dumps(c1m.flatten().tolist()),
                json.dumps(c5m.flatten().tolist()),
                json.dumps(c15m.flatten().tolist()),
                json.dumps(ob_seq.flatten().tolist()),
                float(outcome),
            ),
        )
        inserted += 1
        if inserted % 500 == 0:
            conn.commit()
            print(f"  Materialized {inserted} reconstructed research rows...")

    conn.commit()
    print(f"Inserted {inserted} reconstructed research rows into ml_model_inputs")
    if skipped:
        print(f"Skipped {skipped} existing, malformed or insufficient-history rows")
    conn.close()


if __name__ == "__main__":
    main()
