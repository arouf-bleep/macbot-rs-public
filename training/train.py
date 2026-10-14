"""
train.py — Full training pipeline for MultiHeadBTCPredictor.

Pipeline stages:
  1. Load data from SQLite (6 ML tables)
  2. Build tensors (candles, orderbook sequences, scalars, staleness, regime)
  3. Reserve a chronological, market-disjoint holdout
  4. Development-only diagnostics and Optuna walk-forward selection
  5. Optional orderbook pretraining on the final training prefix
  6. Final training and one holdout evaluation against baselines
  7. Versioned ONNX/report export; runtime publication is opt-in

Usage:
  python train.py --db ../../data/macbot.db --out ../../models/
"""

from __future__ import annotations

import argparse
import datetime
from datetime import timedelta as target_horizon
import json
import math
import os
import sqlite3
from pathlib import Path

import numpy as np
import optuna
import torch
import torch.nn as nn
import torch.nn.functional as F
from sklearn.metrics import brier_score_loss, roc_auc_score
from torch.utils.data import DataLoader, Dataset, Subset
from tqdm import tqdm

from evaluation_split import chronological_holdout
from evaluation_identity import (
    build_evaluation_identity, dataset_identity, source_snapshot,
    verify_source_snapshot, verified_model_bytes,
)
from regime_contract import regime_value_from_label
from model import (
    MultiHeadBTCPredictor,
    NUM_CONTEXT_FEATURES,
    NUM_FUSION_FEATURES,
    NUM_SCALAR_FEATURES,
    NUM_STALENESS_FEATURES,
    OrderbookTransformerBranch,
    export_onnx,
)

# Keep this list aligned with src/ml/model.rs FEATURE_NAMES
FEATURE_NAMES = [
    # 0–23: original features
    "orderbook_imbalance",
    "tick_velocity",
    "vwap_confluence",
    "price_divergence",
    "spike_detector",
    "deribit_pcr",
    "rsi_macd_ha",
    "fear_greed",
    "funding_rate",
    "oi_momentum",
    "aggressor_ratio",
    "coingecko_sentiment",
    "finnhub_sentiment",
    "lunarcrush_sentiment",
    "hour_of_day",
    "day_of_week",
    "distance_from_round_1000",
    "distance_from_round_500",
    "volatility_10",
    "volatility_30",
    "window_momentum",
    "long_liq_volume",
    "short_liq_volume",
    "depth_imbalance_5",
    # 24–33: features added in Sessions 5–16
    "polymarket_flow",
    "vpin_microstructure",
    "options_advanced",
    "whale_flow",
    "cross_market",
    "latency_arb",
    "news_sentiment",
    "btc_final_momentum",
    "kyle_lambda",
    "hawkes_flow",
]
NUM_FEATURES = len(FEATURE_NAMES)  # 34
assert NUM_FEATURES == NUM_FUSION_FEATURES, "training FEATURE_NAMES must match model.py feature contract"


# ── Dataset ──────────────────────────────────────────────────────────────────

def utc_timestamp(value):
    timestamp = datetime.datetime.fromisoformat(str(value).replace("Z", "+00:00"))
    if timestamp.tzinfo is None:
        timestamp = timestamp.replace(tzinfo=datetime.timezone.utc)
    return timestamp.astimezone(datetime.timezone.utc)

class BTCDataset(Dataset):
    """
    Loads aligned windows from the ML SQLite tables or exact archived model inputs.

    Each sample is a prediction window with candles filtered by market_id:
      - 60 × 1m candles → [60, 5]
      - 24 × 5m candles (aggregated from 1m) → [24, 5]
      - 16 × 15m candles (aggregated from 1m) → [16, 5]
      - 12 × orderbook snapshots → [12, 45]
      - NUM_SCALAR_FEATURES scalars
      - NUM_STALENESS_FEATURES staleness values
      - 1 regime float
      - Label: resolved p_up (0/1); no observed forward auxiliary targets
    """

    def __init__(self, db_path: str, max_days: int = 0, source: str = "auto"):
        self.samples = []
        self.meta = []
        self.has_orderbook = False
        self.source = source
        self.source_kind = "features"
        self._load(db_path, max_days=max_days, source=source)

    def _resolve_source(self, conn: sqlite3.Connection, source: str) -> str:
        if source == "features":
            return "features"
        if source == "exact_inputs":
            return "exact_inputs"
        try:
            count = conn.execute(
                "SELECT COUNT(*) FROM ml_model_inputs WHERE outcome IS NOT NULL"
            ).fetchone()[0]
        except sqlite3.OperationalError:
            count = 0
        return "exact_inputs" if count > 0 else "features"

    def _load_exact_inputs(self, conn: sqlite3.Connection, db_path: str, max_days: int = 0):
        if max_days > 0:
            cutoff = (datetime.datetime.utcnow() - datetime.timedelta(days=max_days)).isoformat()
            rows = conn.execute(
                """
                SELECT timestamp, market_id, signal, regime_label, regime_value,
                       scalars_json, staleness_json, candles_1m_json, candles_5m_json,
                       candles_15m_json, orderbook_seq_json, outcome, fusion_score, p_up, run_id
                FROM ml_model_inputs
                WHERE outcome IS NOT NULL AND timestamp >= ?
                ORDER BY timestamp, market_id, signal
                """,
                (cutoff,),
            ).fetchall()
            print(f"  Filtered exact archive to last {max_days} days (cutoff={cutoff})")
        else:
            rows = conn.execute(
                """
                SELECT timestamp, market_id, signal, regime_label, regime_value,
                       scalars_json, staleness_json, candles_1m_json, candles_5m_json,
                       candles_15m_json, orderbook_seq_json, outcome, fusion_score, p_up, run_id
                FROM ml_model_inputs
                WHERE outcome IS NOT NULL
                ORDER BY timestamp, market_id, signal
                """
            ).fetchall()

        skipped = 0
        for row in rows:
            try:
                scalars = np.array(json.loads(row[5]), dtype=np.float32)
                staleness = np.array(json.loads(row[6]), dtype=np.float32)
                c1m = np.array(json.loads(row[7]), dtype=np.float32).reshape(60, 5)
                c5m = np.array(json.loads(row[8]), dtype=np.float32).reshape(24, 5)
                c15m = np.array(json.loads(row[9]), dtype=np.float32).reshape(16, 5)
                ob_seq = np.array(json.loads(row[10]), dtype=np.float32).reshape(12, 45)
                label_p_up = float(row[11])
                regime_value = float(row[4])
            except (ValueError, TypeError, json.JSONDecodeError):
                skipped += 1
                continue

            if (scalars.shape != (NUM_SCALAR_FEATURES,)
                    or staleness.shape != (NUM_STALENESS_FEATURES,)
                    or label_p_up not in (0.0, 1.0) or not math.isfinite(regime_value)
                    or not all(np.isfinite(values).all()
                               for values in (scalars, staleness, c1m, c5m, c15m, ob_seq))):
                skipped += 1
                continue

            self.has_orderbook = self.has_orderbook or bool(np.any(np.abs(ob_seq) > 1e-12))
            source_kind = ("reconstructed_inputs" if str(row[14]).startswith(("reconstructed:", "backfill_"))
                           else "exact_inputs")
            # Only the resolved binary outcome is supervised. Exact input
            # snapshots do not contain future move/volatility/time observations.
            self.samples.append({
                "candles_1m": c1m,
                "candles_5m": c5m,
                "candles_15m": c15m,
                "orderbook_seq": ob_seq,
                "scalars": scalars,
                "staleness": staleness,
                "regime": np.array([regime_value], dtype=np.float32),
                "label_p_up": np.float32(label_p_up),
            })
            self.meta.append(
                {
                    "timestamp": row[0],
                    "market_id": row[1],
                    "signal": row[2],
                    "regime_label": row[3],
                    "outcome": label_p_up,
                    "fusion_score": float(row[12]) if row[12] is not None else None,
                    "archived_p_up": float(row[13]) if row[13] is not None else None,
                    "source_kind": source_kind,
                }
            )

        kinds = {metadata["source_kind"] for metadata in self.meta}
        if "reconstructed_inputs" in kinds:
            self.source_kind = "mixed_inputs" if len(kinds) > 1 else "reconstructed_inputs"
        if skipped > 0:
            print(f"  Skipped {skipped} malformed exact-input rows")
        print(f"Loaded {len(self.samples)} labeled exact-input samples from {db_path}")

    def _load(self, db_path: str, max_days: int = 0, source: str = "auto"):
        conn = sqlite3.connect(Path(db_path).expanduser().resolve().as_uri() + "?mode=ro", uri=True)
        self.source_kind = self._resolve_source(conn, source)
        print(f"  Data source: {self.source_kind}")
        if self.source_kind == "exact_inputs":
            self._load_exact_inputs(conn, db_path, max_days=max_days)
            conn.close()
            if self.samples:
                labels = np.array([s["label_p_up"] for s in self.samples])
                up_pct = labels.mean() * 100
                print(f"\n── Data Sanity Checks ──")
                print(f"  Label distribution: {up_pct:.1f}% UP / {100-up_pct:.1f}% DOWN")
                all_scalars = np.array([s["scalars"] for s in self.samples])
                variances = all_scalars.var(axis=0)
                zero_var = np.sum(variances < 1e-10)
                print(f"  Scalar features: {len(variances)} total, {zero_var} zero-variance")
                c1m_sample = self.samples[0]["candles_1m"]
                print(f"  Sample candle range: close [{c1m_sample[:, 3].min():.2f} - {c1m_sample[:, 3].max():.2f}]")
                if not self.has_orderbook:
                    print(f"  Orderbook data: NONE (branch will be gated)")
            return

        # ── Load features timeseries (for labels + scalars) ──────────────
        if max_days > 0:
            from datetime import datetime, timedelta
            cutoff = (datetime.utcnow() - timedelta(days=max_days)).isoformat()
            features_raw = conn.execute(
                "SELECT timestamp, features_json, fusion_score, p_up, signal, "
                "regime, outcome, staleness_json, market_id "
                "FROM ml_features_timeseries WHERE outcome IS NOT NULL "
                "AND timestamp >= ? ORDER BY timestamp",
                (cutoff,),
            ).fetchall()
            print(f"  Filtered to last {max_days} days (cutoff={cutoff})")
        else:
            features_raw = conn.execute(
                "SELECT timestamp, features_json, fusion_score, p_up, signal, "
                "regime, outcome, staleness_json, market_id "
                "FROM ml_features_timeseries WHERE outcome IS NOT NULL "
                "ORDER BY timestamp"
            ).fetchall()

        # ── Check if any orderbook data exists ─────────────────────────
        ob_count = conn.execute("SELECT COUNT(*) FROM ml_orderbook_snapshots").fetchone()[0]
        self.has_orderbook = ob_count > 0

        # ── Load ALL candles once (they're BTC candles, shared across markets) ──
        # Historical backfills can legitimately reuse the same BTC minute across
        # many market windows, so dedupe by timestamp here before alignment.
        all_candles_raw = conn.execute(
            "SELECT timestamp, MIN(open), MAX(high), MIN(low), MIN(close), MIN(volume) "
            "FROM ml_candles_1m GROUP BY timestamp ORDER BY timestamp"
        ).fetchall()
        all_candles_raw.sort(key=lambda row: utc_timestamp(row[0]))
        all_candle_ts = [utc_timestamp(r[0]) + target_horizon(minutes=1) for r in all_candles_raw]
        all_candle_data = np.array([(r[1], r[2], r[3], r[4], r[5]) for r in all_candles_raw], dtype=np.float32)
        print(f"  Loaded {len(all_candle_data)} BTC 1m candles")

        import bisect

        # ── Build samples ─────────────────────────────────────────────
        skipped = 0
        for feat_row in features_raw:
            feat_ts = feat_row[0]
            features_json_str = feat_row[1]
            regime_str = feat_row[5]
            outcome = float(feat_row[6])
            staleness_json = feat_row[7]
            market_id = feat_row[8]

            # Find candles up to this feature's timestamp
            idx = bisect.bisect_right(all_candle_ts, utc_timestamp(feat_ts))
            if idx < 60:
                skipped += 1
                continue

            candles_raw = all_candle_data[max(0, idx - 240):idx]  # up to 240 for 15m aggregation

            if len(candles_raw) < 60:
                skipped += 1
                continue

            candle_data = np.array(candles_raw, dtype=np.float32)  # [N, 5]
            n_candles = len(candle_data)

            # 60 × 1m candles (last 60)
            c1m = candle_data[-60:]  # [60, 5]

            # Aggregate to 5m: need 120 1m bars for 24 × 5m
            if n_candles >= 120:
                c5m_raw = candle_data[-120:]
            else:
                c5m_raw = candle_data
                pad = np.zeros((120 - len(c5m_raw), 5), dtype=np.float32)
                c5m_raw = np.concatenate([pad, c5m_raw])
            c5m = _aggregate_candles(c5m_raw, 5)  # [24, 5]

            # Aggregate to 15m: need 240 1m bars for 16 × 15m
            if n_candles >= 240:
                c15m_raw = candle_data[-240:]
            else:
                c15m_raw = candle_data
                pad = np.zeros((240 - len(c15m_raw), 5), dtype=np.float32)
                c15m_raw = np.concatenate([pad, c15m_raw])
            c15m = _aggregate_candles(c15m_raw, 15)  # [16, 5]

            # Orderbook: zeros (no historical data)
            ob_seq = np.zeros((12, 45), dtype=np.float32)

            # Parse scalars from features_json
            try:
                feat_dict = json.loads(features_json_str)
            except (json.JSONDecodeError, TypeError):
                skipped += 1
                continue
            if not isinstance(feat_dict, dict):
                skipped += 1
                continue

            # All fusion features in runtime order
            feature_map = {}
            for k, v in feat_dict.items():
                try:
                    feature_map[k] = float(v)
                except (TypeError, ValueError):
                    continue

            # 6 context vars
            context_keys = [
                "time_remaining", "balance", "positions",
                "contract_price", "price_discovery_lag", "btc_price",
            ]
            btc_price_fallback = float(candle_data[-1, 3])
            context_defaults = {"btc_price": btc_price_fallback}
            try:
                context_feats = [float(feat_dict.get(k, context_defaults.get(k, 0.0))) for k in context_keys]
            except (TypeError, ValueError):
                skipped += 1
                continue

            # ── Backfill 10 enrichment features from existing data ──────
            # 1. hour_of_day (normalized 0-1)
            try:
                from datetime import datetime as _dt
                ts_dt = _dt.fromisoformat(feat_ts.replace("Z", "+00:00") if feat_ts.endswith("Z") else feat_ts)
                hour_of_day = ts_dt.hour / 23.0
                day_of_week = ts_dt.weekday() / 6.0
            except Exception:
                hour_of_day = 0.0
                day_of_week = 0.0

            # 2. distance from round numbers (using BTC price from candles)
            btc_price = btc_price_fallback  # last close
            rem_1000 = btc_price % 1000.0
            half_1000 = 500.0
            dist_1000 = (half_1000 - abs(rem_1000 - half_1000)) / half_1000
            rem_500 = btc_price % 500.0
            half_500 = 250.0
            dist_500 = (half_500 - abs(rem_500 - half_500)) / half_500

            # 3. volatility_10 and volatility_30 from 1m candle closes
            closes = candle_data[:, 3]
            def _vol(c, w):
                if len(c) < w + 1:
                    return 0.0
                rets = np.diff(c[-w-1:]) / (c[-w-1:-1] + 1e-10)
                return float(np.std(rets)) if len(rets) > 0 else 0.0
            vol_10 = _vol(closes, 10)
            vol_30 = _vol(closes, 30)

            # 4. window_momentum (price change since window open, normalized)
            if n_candles >= 2:
                window_mom = (float(candle_data[-1, 3]) - float(candle_data[0, 0])) / (float(candle_data[0, 0]) + 1e-10)
                window_mom = max(-1.0, min(1.0, window_mom / 0.005))  # ±0.5% scale
            else:
                window_mom = 0.0

            # 5. liquidation volumes (not available historically)
            long_liq = 0.0
            short_liq = 0.0

            # 6. depth_imbalance_5 (not available historically)
            depth_imb = 0.0

            feature_map.update({
                "hour_of_day": hour_of_day,
                "day_of_week": day_of_week,
                "distance_from_round_1000": dist_1000,
                "distance_from_round_500": dist_500,
                "volatility_10": vol_10,
                "volatility_30": vol_30,
                "window_momentum": window_mom,
                "long_liq_volume": long_liq,
                "short_liq_volume": short_liq,
                "depth_imbalance_5": depth_imb,
            })

            fusion_feats = [float(feature_map.get(name, 0.0)) for name in FEATURE_NAMES]
            scalars = np.array(fusion_feats + context_feats, dtype=np.float32)  # [NUM_SCALAR_FEATURES]

            # Staleness (NUM_STALENESS_FEATURES features)
            if staleness_json:
                try:
                    stale_base = np.array(json.loads(staleness_json), dtype=np.float32)
                except (json.JSONDecodeError, TypeError):
                    stale_base = np.zeros(NUM_FEATURES, dtype=np.float32)
            else:
                stale_base = np.zeros(NUM_FEATURES, dtype=np.float32)
            if len(stale_base) < NUM_FEATURES:
                stale_base = np.pad(stale_base, (0, NUM_FEATURES - len(stale_base)))
            elif len(stale_base) > NUM_FEATURES:
                stale_base = stale_base[:NUM_FEATURES]
            staleness = stale_base  # [NUM_STALENESS_FEATURES]
            if (outcome not in (0.0, 1.0) or not np.isfinite(scalars).all()
                    or not np.isfinite(candle_data).all() or not np.isfinite(staleness).all()):
                skipped += 1
                continue

            # Regime encoding stays pinned to the shared Rust/Python contract.
            regime = np.array([regime_value_from_label(regime_str)], dtype=np.float32)

            # ── Labels ───────────────────────────────────────────────────
            label_p_up = outcome

            # Reconstructed rows likewise have no observed forward auxiliary targets.
            self.samples.append({
                "candles_1m": c1m,        # [60, 5]
                "candles_5m": c5m,        # [24, 5]
                "candles_15m": c15m,      # [16, 5]
                "orderbook_seq": ob_seq,  # [12, 45]
                "scalars": scalars,       # [NUM_SCALAR_FEATURES]
                "staleness": staleness,   # [NUM_STALENESS_FEATURES]
                "regime": regime,          # [1]
                "label_p_up": np.float32(label_p_up),
            })
            self.meta.append(
                {
                    "timestamp": feat_ts,
                    "market_id": market_id,
                    "regime_label": regime_str,
                    "outcome": float(label_p_up),
                }
            )

        conn.close()

        if skipped > 0:
            print(f"  Skipped {skipped} samples with < 60 candles")
        print(f"Loaded {len(self.samples)} labeled samples from {db_path}")

        # ── Sanity checks ────────────────────────────────────────────────
        if self.samples:
            labels = np.array([s["label_p_up"] for s in self.samples])
            up_pct = labels.mean() * 100
            print(f"\n── Data Sanity Checks ──")
            print(f"  Label distribution: {up_pct:.1f}% UP / {100-up_pct:.1f}% DOWN")

            # Check feature variance
            all_scalars = np.array([s["scalars"] for s in self.samples])
            variances = all_scalars.var(axis=0)
            zero_var = np.sum(variances < 1e-10)
            print(f"  Scalar features: {len(variances)} total, {zero_var} zero-variance")

            # Check candle variance (first sample)
            c1m_sample = self.samples[0]["candles_1m"]
            print(f"  Sample candle range: close [{c1m_sample[:, 3].min():.2f} - {c1m_sample[:, 3].max():.2f}]")

            if not self.has_orderbook:
                print(f"  Orderbook data: NONE (branch will be gated)")

    def subset(self, indices) -> "BTCDataset":
        result = BTCDataset.__new__(BTCDataset)
        result.samples = [self.samples[i] for i in indices]
        result.meta = [self.meta[i] for i in indices]
        result.source = self.source
        result.source_kind = self.source_kind
        result.has_orderbook = any(
            np.any(np.abs(s["orderbook_seq"]) > 1e-12) for s in result.samples
        )
        return result

    def __len__(self):
        return len(self.samples)

    def __getitem__(self, idx):
        s = self.samples[idx]
        return {k: torch.from_numpy(v) if isinstance(v, np.ndarray) else torch.tensor(v)
                for k, v in s.items()}


# ── Curriculum Learning ───────────────────────────────────────────────────────

class CurriculumSampler:
    """
    Curriculum learning: sort training samples by difficulty.

    Easy = model was confident AND correct on these samples.
    Hard = model was uncertain OR wrong.

    Start training with easy examples, gradually introduce hard ones.
    This helps the model build fundamentals before learning edge cases.
    """

    def __init__(self, dataset: BTCDataset):
        self.dataset = dataset
        self.n = len(dataset)
        # Initial difficulty = uniform (0.5 for all)
        self.difficulties = np.ones(self.n) * 0.5

    def update_difficulties(self, model: 'MultiHeadBTCPredictor', device: str):
        """Compute per-sample difficulty using current model predictions."""
        model.eval()
        with torch.no_grad():
            for i, sample in enumerate(self.dataset.samples):
                batch = {k: torch.from_numpy(v).unsqueeze(0).to(device)
                         if isinstance(v, np.ndarray) else torch.tensor(v).unsqueeze(0).to(device)
                         for k, v in sample.items()}

                p_up, _, _, _, _ = model(
                    batch["candles_1m"], batch["candles_5m"], batch["candles_15m"],
                    batch["orderbook_seq"], batch["scalars"], batch["staleness"],
                    batch["regime"],
                )
                pred = p_up.squeeze().item()
                label = float(sample["label_p_up"])

                # Difficulty = how wrong the model is
                error = abs(pred - label)
                # Confident and wrong = hardest (difficulty = 1.0)
                # Uncertain = medium difficulty (difficulty = 0.5)
                # Confident and correct = easy (difficulty = 0.0)
                confidence = abs(pred - 0.5) * 2  # [0, 1]
                wrong = (pred > 0.5) != (label > 0.5)

                if wrong:
                    self.difficulties[i] = 0.5 + confidence * 0.5  # [0.5, 1.0]
                else:
                    self.difficulties[i] = 0.5 - confidence * 0.5  # [0.0, 0.5]

    def get_curriculum_indices(self, epoch: int, total_epochs: int) -> list:
        """
        Return sample indices for this epoch based on curriculum phase.

        Phase 1 (first 30%): Easy examples only (difficulty < 0.4)
        Phase 2 (middle 40%): Easy + Medium (difficulty < 0.7)
        Phase 3 (last 30%): All examples
        """
        progress = epoch / max(total_epochs - 1, 1)

        if progress < 0.30:
            # Easy phase: use bottom 40% by difficulty
            threshold = np.percentile(self.difficulties, 40)
            indices = [i for i, d in enumerate(self.difficulties) if d <= threshold]
        elif progress < 0.70:
            # Medium phase: use bottom 70% by difficulty
            threshold = np.percentile(self.difficulties, 70)
            indices = [i for i, d in enumerate(self.difficulties) if d <= threshold]
        else:
            # Full phase: all examples
            indices = list(range(self.n))

        if len(indices) < 10:
            indices = list(range(self.n))  # Fallback to all

        return indices


# ── Synthetic Data Augmentation (#13) ────────────────────────────────────────

def augment_sample(sample: dict, noise_scale: float = 0.01) -> list:
    """Perturb fusion measurements without inventing outcomes or market context.

    Mirroring a candle history does not establish the opposite market outcome;
    independent OHLC jitter can also create impossible bars. Keep candles,
    context, and labels intact and add only small feature measurement noise.
    """
    augmented = []
    for _ in range(2):
        copy = {key: value.copy() if isinstance(value, np.ndarray) else value
                for key, value in sample.items()}
        copy["scalars"][:NUM_FUSION_FEATURES] += (
            np.random.randn(NUM_FUSION_FEATURES).astype(np.float32) * noise_scale
        )
        augmented.append(copy)
    return augmented


def create_augmented_dataset(dataset: BTCDataset, augment_factor: int = 2) -> BTCDataset:
    """
    Create augmented dataset by synthetically generating new samples.

    augment_factor=2 means 2 augmented samples per real sample → 3x total size.
    """
    print(f"\n── Synthetic Data Augmentation (factor={augment_factor}x) ──")
    original_n = len(dataset)

    new_samples = list(dataset.samples)  # Start with originals

    for i, sample in enumerate(dataset.samples):
        augmented = augment_sample(sample)
        new_samples.extend(augmented[:augment_factor])

    # Create a new dataset with augmented samples
    augmented_dataset = BTCDataset.__new__(BTCDataset)
    augmented_dataset.samples = new_samples
    augmented_dataset.has_orderbook = dataset.has_orderbook

    print(f"  Original samples: {original_n}")
    print(f"  Augmented samples: {len(new_samples)} ({len(new_samples)/original_n:.1f}x)")
    return augmented_dataset


# ── Probability Surface via MC Dropout (#17) ─────────────────────────────────

def compute_probability_surface(
    model: 'MultiHeadBTCPredictor',
    batch: dict,
    n_samples: int = 30,
    device: str = "cpu",
) -> dict:
    """
    Instead of p_up = 0.73 (point estimate), compute a full distribution:

    Returns:
    - mean: central estimate
    - std: uncertainty
    - p5, p25, p75, p95: distribution percentiles
    - high_conviction: True if std < 0.05 (tight distribution)
    - should_skip: True if std > 0.10 (fat tails = skip trade)

    Uses MC Dropout: enable dropout at inference time, run N forward passes.
    """
    # Enable dropout for MC sampling
    model.train()  # dropout active in train mode

    predictions = []
    with torch.no_grad():
        for _ in range(n_samples):
            p_up, _, _, _, calib = model(
                batch["candles_1m"].to(device),
                batch["candles_5m"].to(device),
                batch["candles_15m"].to(device),
                batch["orderbook_seq"].to(device),
                batch["scalars"].to(device),
                batch["staleness"].to(device),
                batch["regime"].to(device),
            )
            predictions.append(p_up.squeeze(-1).cpu().numpy())

    model.eval()  # Restore eval mode

    predictions = np.array(predictions)  # [n_samples, batch_size]

    mean = predictions.mean(axis=0)
    std = predictions.std(axis=0)
    p5 = np.percentile(predictions, 5, axis=0)
    p25 = np.percentile(predictions, 25, axis=0)
    p75 = np.percentile(predictions, 75, axis=0)
    p95 = np.percentile(predictions, 95, axis=0)

    return {
        "mean": mean,
        "std": std,
        "p5": p5,
        "p25": p25,
        "p75": p75,
        "p95": p95,
        "high_conviction": std < 0.05,
        "should_skip": std > 0.10,
    }


def _freeze_noisy_branches(model: MultiHeadBTCPredictor):
    """Freeze CNN and Transformer branches so only MLP + heads learn."""
    for param in model.cnn.parameters():
        param.requires_grad = False
    for param in model.transformer.parameters():
        param.requires_grad = False
    trainable = sum(p.numel() for p in model.parameters() if p.requires_grad)
    total = sum(p.numel() for p in model.parameters())
    print(f"  Frozen CNN + Transformer. Trainable: {trainable:,} / {total:,} params")


def _aggregate_candles(candles: np.ndarray, period: int) -> np.ndarray:
    """Aggregate 1m candles into N-minute candles. Input: [N*period, 5], Output: [N, 5]."""
    n = len(candles) // period
    result = np.zeros((n, 5), dtype=np.float32)
    for i in range(n):
        chunk = candles[i * period: (i + 1) * period]
        chunk = chunk[~np.all(chunk == 0, axis=1)]
        if not len(chunk):
            continue
        result[i, 0] = chunk[0, 0]          # open
        result[i, 1] = chunk[:, 1].max()     # high
        result[i, 2] = chunk[:, 2].min()     # low
        result[i, 3] = chunk[-1, 3]          # close
        result[i, 4] = chunk[:, 4].sum()     # volume
    return result


# ── Orderbook contrastive pretraining ──────────────────────────────────

class ContrastiveOrderbookDataset(Dataset):
    """Generates augmented pairs from orderbook sequences for SimCLR."""

    def __init__(self, dataset: BTCDataset):
        self.sequences = [s["orderbook_seq"] for s in dataset.samples]

    def __len__(self):
        return len(self.sequences)

    def __getitem__(self, idx):
        seq = self.sequences[idx].copy()
        # Augmentation 1: Gaussian noise
        aug1 = seq + np.random.randn(*seq.shape).astype(np.float32) * 0.01
        # Augmentation 2: Random feature masking (10% of features zeroed)
        aug2 = seq.copy()
        mask = np.random.rand(*seq.shape) < 0.1
        aug2[mask] = 0.0
        return torch.from_numpy(aug1), torch.from_numpy(aug2)


def nt_xent_loss(z1: torch.Tensor, z2: torch.Tensor, temperature: float = 0.1) -> torch.Tensor:
    """Normalized temperature-scaled cross-entropy loss (NT-Xent)."""
    batch_size = z1.size(0)
    z = torch.cat([z1, z2], dim=0)  # [2B, D]
    z = F.normalize(z, dim=1)
    sim = torch.mm(z, z.t()) / temperature  # [2B, 2B]

    # Mask out self-similarity
    mask = torch.eye(2 * batch_size, device=z.device).bool()
    sim.masked_fill_(mask, -1e9)

    # Positive pairs: (i, i+B) and (i+B, i)
    labels = torch.cat([
        torch.arange(batch_size, 2 * batch_size),
        torch.arange(batch_size),
    ]).to(z.device)

    return F.cross_entropy(sim, labels)


def pretrain_orderbook_encoder(
    dataset: BTCDataset,
    n_epochs: int = 20,
    lr: float = 1e-3,
    batch_size: int = 64,
    device: str = "cpu",
) -> OrderbookTransformerBranch:
    """SimCLR-style pretraining for the orderbook Transformer branch."""
    print("\n── Contrastive Pretraining (Orderbook Encoder) ──")

    contrast_ds = ContrastiveOrderbookDataset(dataset)
    loader = DataLoader(contrast_ds, batch_size=batch_size, shuffle=True, drop_last=True)

    encoder = OrderbookTransformerBranch(out_dim=64).to(device)
    # Projection head for contrastive learning
    proj_head = nn.Sequential(
        nn.Linear(64, 64),
        nn.ReLU(),
        nn.Linear(64, 32),
    ).to(device)

    optimizer = torch.optim.Adam(
        list(encoder.parameters()) + list(proj_head.parameters()),
        lr=lr, weight_decay=1e-5,
    )

    for epoch in range(n_epochs):
        total_loss = 0.0
        n_batches = 0
        for aug1, aug2 in loader:
            aug1, aug2 = aug1.to(device), aug2.to(device)
            z1 = proj_head(encoder(aug1))
            z2 = proj_head(encoder(aug2))
            loss = nt_xent_loss(z1, z2)
            optimizer.zero_grad()
            loss.backward()
            optimizer.step()
            total_loss += loss.item()
            n_batches += 1

        avg = total_loss / max(n_batches, 1)
        if (epoch + 1) % 5 == 0 or epoch == 0:
            print(f"  Epoch {epoch+1}/{n_epochs}: NT-Xent loss = {avg:.4f}")

    print("  Contrastive pretraining complete.")
    return encoder


# ── Expected Calibration Error (#10) ──────────────────────────────────────

def expected_calibration_error(
    probs: torch.Tensor,
    labels: torch.Tensor,
    n_bins: int = 10,
) -> torch.Tensor:
    """Differentiable ECE loss for calibration head training.

    Computes the expected calibration error: weighted average of
    |accuracy - confidence| across probability bins.

    probs:  [B] predicted probabilities
    labels: [B] binary labels (0 or 1)
    """
    if n_bins < 1 or probs.numel() == 0 or probs.shape != labels.shape:
        raise ValueError("calibration needs matching nonempty probabilities and labels and positive bins")
    bin_boundaries = torch.linspace(0, 1, n_bins + 1, device=probs.device)
    ece = torch.zeros(1, device=probs.device)
    total = probs.size(0)

    for i in range(n_bins):
        lo, hi = bin_boundaries[i], bin_boundaries[i + 1]
        mask = (probs >= lo) & ((probs <= hi) if i == n_bins - 1 else (probs < hi))
        count = mask.sum().float()
        if count > 0:
            avg_conf = probs[mask].mean()
            avg_acc = labels[mask].float().mean()
            ece = ece + (count / total) * torch.abs(avg_acc - avg_conf)

    return ece.squeeze()


# ── Adversarial Validation ───────────────────────────────────────────────────

def adversarial_validation(
    train_scalars: np.ndarray,
    test_scalars: np.ndarray,
    threshold: float = 0.75,
) -> float:
    """
    Train a classifier to distinguish train vs test samples.
    If AUC > threshold, there's significant distribution shift.
    Returns the AUC score.
    """
    from sklearn.ensemble import GradientBoostingClassifier
    from sklearn.model_selection import cross_val_score

    X = np.concatenate([train_scalars, test_scalars])
    y = np.concatenate([np.zeros(len(train_scalars)), np.ones(len(test_scalars))])

    clf = GradientBoostingClassifier(n_estimators=50, max_depth=3, random_state=42)
    scores = cross_val_score(clf, X, y, cv=3, scoring="roc_auc")
    auc = scores.mean()

    print(f"\n── Adversarial Validation ──")
    print(f"  Train/Test distinguishability AUC: {auc:.3f}")
    if auc > threshold:
        print(f"  ⚠ Distribution shift detected (AUC > {threshold})!")
        print(f"  Consider re-sampling or adding domain adaptation.")
    else:
        print(f"  ✓ No significant distribution shift.")
    return auc


# ── Walk-Forward Cross-Validation ────────────────────────────────────────────

def development_validation_indices(dataset, boundary, stop=None):
    """Purge market overlap and a full target horizon at every selection split."""
    stop = len(dataset) if stop is None else stop
    if not 0 < boundary < stop <= len(dataset) or len(dataset.meta) != len(dataset):
        raise ValueError("selection split needs aligned metadata and nonempty partitions")
    # Bias below the exact fraction to avoid ceil rounding adding a row.
    fraction = math.nextafter((stop - boundary) / stop, 0.0)
    split = chronological_holdout(dataset.meta[:stop], fraction, embargo_seconds=900)
    return list(split.development), list(split.holdout)

def walk_forward_cv(
    dataset: BTCDataset,
    hparams: dict,
    n_splits: int = 5,
    pretrained_encoder: OrderbookTransformerBranch | None = None,
    device: str = "cpu",
    freeze_branches: bool = False,
) -> list[dict]:
    """
    Time-series walk-forward CV. Each fold uses all prior data for training
    and the next chunk for validation.
    """
    n = len(dataset)
    fold_size = n // (n_splits + 1)
    results = []

    for fold in range(n_splits):
        train_end = fold_size * (fold + 2)
        val_start = train_end
        val_end = min(val_start + fold_size, n)

        if val_end <= val_start:
            break

        train_idx, val_idx = development_validation_indices(dataset, train_end, val_end)

        train_loader = DataLoader(
            Subset(dataset, train_idx),
            batch_size=hparams.get("batch_size", 64),
            shuffle=True,
        )
        val_loader = DataLoader(
            Subset(dataset, val_idx),
            batch_size=hparams.get("batch_size", 64),
        )

        model = MultiHeadBTCPredictor(disable_cnn_tf=freeze_branches).to(device)

        # Load pretrained orderbook encoder weights if available
        if pretrained_encoder is not None and not freeze_branches:
            model.transformer.load_state_dict(pretrained_encoder.state_dict())

        metrics = _train_one_fold(model, train_loader, val_loader, hparams, device)
        metrics["fold"] = fold
        results.append(metrics)
        print(f"  Fold {fold}: Brier={metrics['brier']:.4f}, AUC={metrics['auc']:.4f}")

    return results


def _train_one_fold(
    model: MultiHeadBTCPredictor,
    train_loader: DataLoader,
    val_loader: DataLoader,
    hparams: dict,
    device: str,
    curriculum_sampler=None,
) -> dict:
    """Train model for one CV fold and return validation metrics.

    Loss = main_bce + 0.3 * magnitude_mse + 0.2 * volatility_mse
           + ttm_mse + 0.1 * calibration_ece  (#9, #10)
    """
    optimizer = torch.optim.AdamW(
        filter(lambda p: p.requires_grad, model.parameters()),
        lr=hparams.get("lr", 1e-3),
        weight_decay=hparams.get("weight_decay", 1e-4),
    )
    scheduler = torch.optim.lr_scheduler.CosineAnnealingLR(
        optimizer, T_max=hparams.get("epochs", 30),
    )

    n_epochs = hparams.get("epochs", 30)

    for epoch in range(n_epochs):
        model.train()
        for batch in train_loader:
            batch = {k: v.to(device) for k, v in batch.items()}

            p_up, mag, vol, ttm, calib = model(
                batch["candles_1m"], batch["candles_5m"], batch["candles_15m"],
                batch["orderbook_seq"], batch["scalars"], batch["staleness"],
                batch["regime"],
            )

            loss = primary_training_loss(p_up, calib, batch["label_p_up"])

            optimizer.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
            optimizer.step()

        scheduler.step()

    # Validation
    model.eval()
    all_preds, all_labels = [], []
    with torch.no_grad():
        for batch in val_loader:
            batch = {k: v.to(device) for k, v in batch.items()}
            p_up, _, _, _, _ = model(
                batch["candles_1m"], batch["candles_5m"], batch["candles_15m"],
                batch["orderbook_seq"], batch["scalars"], batch["staleness"],
                batch["regime"],
            )
            all_preds.extend(p_up.squeeze(-1).cpu().numpy().tolist())
            all_labels.extend(batch["label_p_up"].cpu().numpy().tolist())

    preds = np.array(all_preds)
    labels = np.array(all_labels)

    brier = brier_score_loss(labels, preds) if len(labels) > 0 else 1.0
    try:
        auc = roc_auc_score(labels, preds) if len(set(labels)) > 1 else 0.5
    except ValueError:
        auc = 0.5

    accuracy = float(np.mean((preds > 0.5) == (labels > 0.5))) if len(labels) > 0 else 0.0

    return {"brier": brier, "auc": auc, "accuracy": accuracy}


# ── Optuna HPO ───────────────────────────────────────────────────────────────

def optuna_hpo(
    dataset: BTCDataset,
    n_trials: int = 100,
    pretrained_encoder: OrderbookTransformerBranch | None = None,
    device: str = "cpu",
    freeze_branches: bool = False,
) -> dict:
    """Bayesian hyperparameter optimization with Optuna."""
    print("\n── Optuna Hyperparameter Optimization ──")

    def objective(trial: optuna.Trial) -> float:
        hparams = {
            "lr": trial.suggest_float("lr", 1e-5, 1e-2, log=True),
            "weight_decay": trial.suggest_float("weight_decay", 1e-6, 1e-2, log=True),
            "batch_size": trial.suggest_categorical("batch_size", [32, 64, 128]),
            "epochs": trial.suggest_int("epochs", 15, 50),
        }

        results = walk_forward_cv(
            dataset, hparams, n_splits=3,
            pretrained_encoder=pretrained_encoder, device=device,
            freeze_branches=freeze_branches,
        )

        if not results:
            raise ValueError("not enough observations for a purged validation fold")
        mean_brier = np.mean([r["brier"] for r in results])
        return mean_brier  # Minimize Brier score

    study = optuna.create_study(
        direction="minimize",
        sampler=optuna.samplers.TPESampler(seed=42),
        pruner=optuna.pruners.MedianPruner(n_warmup_steps=10),
    )
    study.optimize(objective, n_trials=n_trials, show_progress_bar=True)

    best = study.best_params
    print(f"\n  Best hparams: {best}")
    print(f"  Best Brier score: {study.best_value:.4f}")
    return best


# ── Final Training ───────────────────────────────────────────────────────────

def _train_one_run(
    dataset: BTCDataset,
    hparams: dict,
    seed: int,
    pretrained_encoder: OrderbookTransformerBranch | None = None,
    device: str = "cpu",
    freeze_branches: bool = False,
) -> tuple[MultiHeadBTCPredictor, float, float]:
    """Train one full run with a given seed. Returns (model, best_brier, best_auc).

    Integrates:
      - #9  Multi-task weighted loss
      - #10 Calibration ECE loss
      - #12 Curriculum learning (easy → medium → hard)
      - #13 Synthetic data augmentation (training set only)
    """
    torch.manual_seed(seed)
    np.random.seed(seed)

    n = len(dataset)
    train_end = int(n * 0.85)
    train_indices, val_indices = development_validation_indices(dataset, train_end)

    # #13 Create augmented training subset
    train_subset_ds = BTCDataset.__new__(BTCDataset)
    train_subset_ds.samples = [dataset.samples[i] for i in train_indices]
    train_subset_ds.has_orderbook = dataset.has_orderbook
    augmented_train = create_augmented_dataset(train_subset_ds, augment_factor=2)

    val_loader = DataLoader(
        Subset(dataset, val_indices),
        batch_size=hparams.get("batch_size", 64),
    )

    model = MultiHeadBTCPredictor(disable_cnn_tf=freeze_branches).to(device)
    if pretrained_encoder is not None and not freeze_branches:
        model.transformer.load_state_dict(pretrained_encoder.state_dict())

    optimizer = torch.optim.AdamW(
        filter(lambda p: p.requires_grad, model.parameters()),
        lr=hparams.get("lr", 1e-3),
        weight_decay=hparams.get("weight_decay", 1e-4),
    )
    n_epochs = hparams.get("epochs", 30)
    if n_epochs < 1:
        raise ValueError("training epochs must be positive")
    scheduler = torch.optim.lr_scheduler.CosineAnnealingLR(optimizer, T_max=n_epochs)

    # #12 Curriculum learning sampler
    curriculum = CurriculumSampler(augmented_train)

    best_brier = 1.0
    best_auc = 0.5
    best_state = None

    for epoch in range(n_epochs):
        # #12 Curriculum: update difficulties and get indices for this epoch
        if epoch > 0 and epoch % 5 == 0:
            curriculum.update_difficulties(model, device)

        curriculum_indices = curriculum.get_curriculum_indices(epoch, n_epochs)
        train_loader = DataLoader(
            Subset(augmented_train, curriculum_indices),
            batch_size=hparams.get("batch_size", 64),
            shuffle=True,
        )

        model.train()
        epoch_loss = 0.0
        n_batches = 0

        for batch in train_loader:
            batch = {k: v.to(device) for k, v in batch.items()}

            p_up, mag, vol, ttm, calib = model(
                batch["candles_1m"], batch["candles_5m"], batch["candles_15m"],
                batch["orderbook_seq"], batch["scalars"], batch["staleness"],
                batch["regime"],
            )

            loss = primary_training_loss(p_up, calib, batch["label_p_up"])

            optimizer.zero_grad()
            loss.backward()
            torch.nn.utils.clip_grad_norm_(model.parameters(), 1.0)
            optimizer.step()

            epoch_loss += loss.item()
            n_batches += 1

        scheduler.step()

        # Validation
        model.eval()
        all_preds, all_labels = [], []
        with torch.no_grad():
            for batch in val_loader:
                batch = {k: v.to(device) for k, v in batch.items()}
                p_up, _, _, _, _ = model(
                    batch["candles_1m"], batch["candles_5m"], batch["candles_15m"],
                    batch["orderbook_seq"], batch["scalars"], batch["staleness"],
                    batch["regime"],
                )
                all_preds.extend(p_up.squeeze(-1).cpu().numpy().tolist())
                all_labels.extend(batch["label_p_up"].cpu().numpy().tolist())

        preds = np.array(all_preds)
        labels = np.array(all_labels)
        brier = brier_score_loss(labels, preds) if len(labels) > 0 else 1.0
        try:
            auc = roc_auc_score(labels, preds) if len(set(labels)) > 1 else 0.5
        except ValueError:
            auc = 0.5
        accuracy = float(np.mean((preds > 0.5) == (labels > 0.5))) if len(labels) > 0 else 0.0

        avg_loss = epoch_loss / max(n_batches, 1)
        if (epoch + 1) % 5 == 0 or epoch == 0:
            phase = "easy" if epoch / max(n_epochs-1, 1) < 0.30 else ("medium" if epoch / max(n_epochs-1, 1) < 0.70 else "full")
            print(f"  Epoch {epoch+1}/{n_epochs} [{phase}]: loss={avg_loss:.4f}, "
                  f"brier={brier:.4f}, auc={auc:.3f}, acc={accuracy:.3f}")

        if brier < best_brier:
            best_brier = brier
            best_auc = auc
            best_state = {k: v.clone() for k, v in model.state_dict().items()}

    if best_state is not None:
        model.load_state_dict(best_state)

    return model, best_brier, best_auc


def train_final(
    dataset: BTCDataset,
    hparams: dict,
    pretrained_encoder: OrderbookTransformerBranch | None = None,
    device: str = "cpu",
    freeze_branches: bool = False,
    n_restarts: int = 5,
) -> MultiHeadBTCPredictor:
    """Train final model with multiple random restarts to avoid collapse."""
    if n_restarts < 1:
        raise ValueError("training restarts must be positive")
    print(f"\n── Final Training ({n_restarts} random restarts) ──")

    best_model = None
    best_brier = 1.0
    best_auc = 0.5

    for restart in range(n_restarts):
        seed = 42 + restart * 137
        print(f"\n  Restart {restart+1}/{n_restarts} (seed={seed}):")
        model, brier, auc = _train_one_run(
            dataset, hparams, seed=seed,
            pretrained_encoder=pretrained_encoder, device=device,
            freeze_branches=freeze_branches,
        )
        print(f"  → Restart {restart+1}: Brier={brier:.4f}, AUC={auc:.3f}")

        if best_model is None or brier < best_brier:
            best_brier = brier
            best_auc = auc
            best_model = model
            print(f"  ★ New best! Brier={best_brier:.4f}, AUC={best_auc:.3f}")

    print(f"\n  Final best: Brier={best_brier:.4f}, AUC={best_auc:.3f}")
    return best_model


# ── Main ─────────────────────────────────────────────────────────────────────

def split_evaluation_data(dataset, fraction=0.2, embargo_seconds=900):
    if len(dataset.meta) != len(dataset.samples):
        raise ValueError("every sample must have evaluation metadata")
    split = chronological_holdout(dataset.meta, fraction, embargo_seconds)
    development = dataset.subset(split.development)
    holdout = dataset.subset(split.holdout)
    if len(development) < 50 or len(holdout) < 20:
        raise ValueError("need at least 50 development and 20 holdout samples after purging")
    return development, holdout, len(split.purged)


def evaluate_holdout(model, development, holdout, device="cpu"):
    """Evaluate once after selection; the baseline uses development labels only."""
    model.eval()
    predictions, labels = [], []
    with torch.no_grad():
        for batch in DataLoader(holdout, batch_size=64):
            batch = {key: value.to(device) for key, value in batch.items()}
            p_up, _, _, _, _ = model(
                batch["candles_1m"], batch["candles_5m"], batch["candles_15m"],
                batch["orderbook_seq"], batch["scalars"], batch["staleness"],
                batch["regime"],
            )
            predictions.extend(p_up.squeeze(-1).cpu().numpy().tolist())
            labels.extend(batch["label_p_up"].cpu().numpy().tolist())
    baseline_probability = float(np.mean([float(s["label_p_up"]) for s in development.samples]))
    brier = float(brier_score_loss(labels, predictions))
    baseline_brier = float(brier_score_loss(labels, [baseline_probability] * len(labels)))
    return {
        "sample_count": len(labels),
        "brier": brier,
        "auc": float(roc_auc_score(labels, predictions)) if len(set(labels)) > 1 else None,
        "baseline_probability": baseline_probability,
        "baseline_brier": baseline_brier,
        "coin_flip_brier": float(brier_score_loss(labels, [0.5] * len(labels))),
        "brier_improvement_over_baseline": baseline_brier - brier,
    }


def primary_training_loss(p_up, calibrated_p_up, labels):
    """Train the observed binary outcome only; no fabricated auxiliary targets."""
    return (F.binary_cross_entropy(p_up.squeeze(-1), labels)
            + 0.1 * expected_calibration_error(calibrated_p_up.squeeze(-1), labels))


def main():
    parser = argparse.ArgumentParser(description="Train MultiHeadBTCPredictor")
    parser.add_argument("--db", required=True, help="Path to SQLite database")
    parser.add_argument("--out", default="./models", help="Output directory for ONNX models")
    parser.add_argument("--trials", type=int, default=100, help="Optuna trials")
    parser.add_argument("--skip-pretrain", action="store_true", help="Skip contrastive pretraining")
    parser.add_argument("--skip-hpo", action="store_true", help="Skip Optuna HPO, use defaults")
    parser.add_argument("--device", default="cpu", help="Device (cpu/cuda/mps)")
    parser.add_argument("--days", type=int, default=0, help="Only use last N days of data (0=all)")
    parser.add_argument("--holdout-fraction", type=float, default=0.2,
                        help="Newest fraction reserved before model selection (default: 0.2)")
    parser.add_argument("--embargo-seconds", type=int, default=900,
                        help="Gap before holdout; must cover the prediction horizon (default: 900)")
    parser.add_argument("--epochs", type=int, default=30, help="Epochs when --skip-hpo is used")
    parser.add_argument("--restarts", type=int, default=5, help="Final model selection restarts")
    parser.add_argument("--publish-latest", action="store_true",
                        help="Explicitly replace latest.onnx after export; does not establish live readiness")
    parser.add_argument(
        "--data-source",
        choices=["auto", "features", "exact_inputs"],
        default="auto",
        help="Dataset source: reconstructed features path, exact archived model inputs, or auto-detect",
    )
    args = parser.parse_args()
    if args.epochs < 1 or args.restarts < 1:
        parser.error("epochs and restarts must be positive")
    if args.embargo_seconds < 900:
        parser.error("embargo must cover the fifteen-minute target horizon (at least 900 seconds)")

    os.makedirs(args.out, exist_ok=True)
    device = args.device
    source_root = Path(__file__).resolve().parent.parent
    sources = source_snapshot(source_root)

    # 1. Load data
    print(f"Loading dataset...{f' (last {args.days} days)' if args.days > 0 else ''}")
    dataset = BTCDataset(args.db, max_days=args.days, source=args.data_source)
    if args.publish_latest and dataset.source_kind in {"features", "reconstructed_inputs", "mixed_inputs"}:
        parser.error("reconstructed inputs are research data; archive runtime inputs before publishing a runtime model")
    loaded_data_identity = dataset_identity(dataset)
    try:
        dataset, holdout, purged_count = split_evaluation_data(
            dataset, args.holdout_fraction, args.embargo_seconds,
        )
    except (ValueError, KeyError) as exc:
        parser.error(str(exc))
    data_identity = {
        "scope": "ordered base tensors and metadata, before random augmentation",
        "loaded_before_split": loaded_data_identity,
        "development": dataset_identity(dataset),
        "holdout": dataset_identity(holdout),
        "purged_count": purged_count,
    }
    print(f"Evaluation split: {len(dataset)} development, {len(holdout)} holdout, "
          f"{purged_count} purged. Holdout is excluded from all selection stages.")

    # 2. Adversarial validation (train/test split check)
    n = len(dataset)
    split = int(n * 0.7)
    train_scalars = np.array([s["scalars"] for s in dataset.samples[:split]])
    test_scalars = np.array([s["scalars"] for s in dataset.samples[split:]])
    adversarial_validation(train_scalars, test_scalars)

    # Model selection uses development data only. Orderbook pretraining is
    # deferred until after HPO so a shared encoder cannot see CV validation folds.
    pretrained_encoder = None

    # 4. Auto-freeze temporal-attention/Transformer when no orderbook data
    freeze = not dataset.has_orderbook
    if freeze:
        print("\n── Freezing Temporal Attention + Transformer branches (no orderbook data) ──")
        print("  Only MLP (scalar features) + task heads will be trained.")

    # 5. Optuna HPO
    if args.skip_hpo:
        best_hparams = {"lr": 5e-4, "weight_decay": 1e-4, "batch_size": 64, "epochs": args.epochs}
        print(f"\nSkipping HPO, using defaults: {best_hparams}")
    else:
        best_hparams = optuna_hpo(
            dataset, n_trials=args.trials,
            pretrained_encoder=pretrained_encoder, device=device,
            freeze_branches=freeze,
        )

    # Only pretrain on the same prefix used by final supervised training.
    if not args.skip_pretrain and dataset.has_orderbook and len(dataset) >= 100:
        pretrain_indices, _ = development_validation_indices(dataset, int(len(dataset) * 0.85))
        pretrained_encoder = pretrain_orderbook_encoder(
            dataset.subset(pretrain_indices), n_epochs=20, device=device,
        )

    # 6. Final training
    model = train_final(
        dataset, best_hparams,
        pretrained_encoder=pretrained_encoder, device=device,
        freeze_branches=freeze, n_restarts=args.restarts,
    )

    holdout_metrics = evaluate_holdout(model, dataset, holdout, device)

    # 7. Export ONNX
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    version = f"v002_{now}_holdout_brier{holdout_metrics['brier']:.3f}"
    onnx_path = os.path.join(args.out, f"{version}.onnx")
    export_onnx(model, onnx_path)

    verify_source_snapshot(source_root, sources)
    if (dataset_identity(dataset) != data_identity["development"]
            or dataset_identity(holdout) != data_identity["holdout"]):
        raise ValueError("evaluation base samples changed during training; rerun before publishing")
    identity = build_evaluation_identity(
        model=Path(onnx_path), model_version=version, sources=sources,
        data=data_identity, arguments=args, hyperparameters=best_hparams,
        metrics=holdout_metrics,
    )
    report = {
        "evaluation_identity": identity,
        "model_version": version, "source_kind": dataset.source_kind,
        "supervised_targets": ["resolved_p_up"],
        "unsupported_auxiliary_targets": ["magnitude", "volatility", "time_to_move"],
        "auxiliary_output_contract": "Zero compatibility placeholders, unavailable as forecasts",
        "development_count": len(dataset), "holdout_count": len(holdout),
        "purged_count": purged_count, "embargo_seconds": args.embargo_seconds,
        "holdout_start": holdout.meta[0]["timestamp"],
        "holdout_end": holdout.meta[-1]["timestamp"],
        "development_market_count": len({m["market_id"] for m in dataset.meta}),
        "holdout_market_count": len({m["market_id"] for m in holdout.meta}),
        "hyperparameters": best_hparams, "restarts": args.restarts,
        "holdout_metrics": holdout_metrics,
        "caveat": "A reused holdout is not independent evidence; minimum sample counts only ensure the pipeline can run.",
    }
    report_path = os.path.join(args.out, f"{version}.json")
    Path(report_path).write_text(json.dumps(report, indent=2), encoding="utf-8")

    if args.publish_latest:
        # Replace atomically so the runtime watcher cannot read a partial model.
        latest_path = Path(args.out) / "latest.onnx"
        temporary = Path(args.out) / f".{version}.latest.tmp"
        temporary.write_bytes(verified_model_bytes(Path(onnx_path), identity["model"]))
        temporary.replace(latest_path)
        print(f"Published {latest_path}")

    print(f"\n{'='*60}")
    print(f"  Training complete!")
    print(f"  Model version: {version}")
    print(f"  Holdout Brier score: {holdout_metrics['brier']:.4f}")
    print(f"  Development-prevalence baseline Brier: {holdout_metrics['baseline_brier']:.4f}")
    print(f"  Parameters: {model.count_parameters():,}")
    print(f"  ONNX exported: {onnx_path}")
    print(f"  Evaluation report: {report_path}")
    print(f"{'='*60}")


if __name__ == "__main__":
    main()
