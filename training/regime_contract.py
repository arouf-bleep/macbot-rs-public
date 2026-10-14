from __future__ import annotations

"""Shared regime-value contract for Rust runtime and Python training/export.

Rust runtime still feeds ordinal floats into the deep model. We keep the
string->float mapping here so training, export, and contract checks all agree
with `src/ml/input_builder.rs`.
"""

NUM_REGIME_BUCKETS = 4

REGIME_MEAN_REVERT_MAX = 0.3
REGIME_TREND_MIN = 1.0
REGIME_HIGH_VOL_MIN = 2.0
DEFAULT_REGIME_FLOAT = 0.5

REGIME_FLOATS = {
    "TREND_UP": 1.0,
    "TREND_DOWN": -1.0,
    "CHOP": 0.0,
    "CHOPPY": 0.0,
    "RANGE": 0.1,
    "BREAKOUT": 1.5,
    "SQUEEZE": 0.3,
    "TRENDING": 1.0,
    "HIGH_VOL": 2.0,
}


def regime_value_from_label(label: str | None) -> float:
    if not label:
        return DEFAULT_REGIME_FLOAT
    return REGIME_FLOATS.get(label.upper(), DEFAULT_REGIME_FLOAT)
