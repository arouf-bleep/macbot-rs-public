#!/usr/bin/env python3
"""Shared alpha research presets for repeatable sweeps/reports."""

from __future__ import annotations


TREND_UP_YES_SPECIALIST = {
    "mode": "maker",
    "entry_thresholds": "0.370",
    "sides": "yes_only",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 2.0,
    "stand_down_cooldown_candles": 48,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_UP",
    "regime_side_modes": "",
}

TREND_UP_YES_RECENT_SPECIALIST = {
    "mode": "maker",
    "entry_thresholds": "0.380",
    "sides": "yes_only",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 1.5,
    "stand_down_cooldown_candles": 48,
    "min_confidence_margin": 0.03,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_UP",
    "session_allowlist": "ASIA,US",
    "blocked_hours_utc": "4,6",
    "regime_side_modes": "TREND_UP:yes_only",
}

TREND_UP_YES_HOUR_FILTERED_SPECIALIST = {
    "mode": "maker",
    "entry_thresholds": "0.380",
    "sides": "yes_only",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 1.5,
    "stand_down_cooldown_candles": 48,
    "min_confidence_margin": 0.03,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_UP",
    "session_allowlist": "ASIA,US",
    "blocked_hours_utc": "1,3,4,6,16,17,18,19,20",
    "regime_side_modes": "TREND_UP:yes_only",
}

TREND_UP_YES_TOD_HOUR_FILTERED_SPECIALIST = {
    **TREND_UP_YES_HOUR_FILTERED_SPECIALIST,
    "time_of_day_weight": True,
}

TREND_DIRECTIONAL_SPECIALIST = {
    "mode": "maker",
    "entry_thresholds": "0.380",
    "sides": "both",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 2.0,
    "stand_down_cooldown_candles": 48,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_UP,TREND_DOWN",
    "regime_side_modes": "TREND_UP:yes_only,TREND_DOWN:no_only",
}

TAKER_RESOLUTION_BASELINE = {
    "mode": "taker",
    "exit_mode": "next_candle",
    "take_profit_pct": 0.12,
    "stop_loss_pct": 0.08,
    "entry_thresholds": "0.34,0.36,0.38",
    "sides": "both,no_only",
    "adaptive_stand_down": "true,false",
}

TAKER_TP_SL_PROBE = {
    "mode": "taker",
    "exit_mode": "taker_tp_sl",
    "take_profit_pct": 0.12,
    "stop_loss_pct": 0.08,
    "entry_thresholds": "0.34,0.36,0.38",
    "sides": "both,yes_only,no_only",
    "adaptive_stand_down": "true,false",
}

TREND_UP_YES_TAKER_TP_SL = {
    "mode": "taker",
    "exit_mode": "taker_tp_sl",
    "take_profit_pct": 0.12,
    "stop_loss_pct": 0.08,
    "entry_thresholds": "0.36,0.38",
    "sides": "yes_only",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 1.5,
    "stand_down_cooldown_candles": 48,
    "min_confidence_margin": 0.03,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_UP",
    "regime_side_modes": "TREND_UP:yes_only",
}

TREND_UP_YES_TAKER_RESOLUTION = {
    "mode": "taker",
    "exit_mode": "next_candle",
    "take_profit_pct": 0.12,
    "stop_loss_pct": 0.08,
    "entry_thresholds": "0.36,0.38",
    "sides": "yes_only",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 1.5,
    "stand_down_cooldown_candles": 48,
    "min_confidence_margin": 0.03,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_UP",
    "regime_side_modes": "TREND_UP:yes_only",
}

TREND_UP_YES_TAKER_HOUR_FILTERED = {
    "mode": "taker",
    "exit_mode": "next_candle",
    "take_profit_pct": 0.12,
    "stop_loss_pct": 0.08,
    "entry_thresholds": "0.36,0.38",
    "sides": "yes_only",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 1.5,
    "stand_down_cooldown_candles": 48,
    "min_confidence_margin": 0.03,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_UP",
    "blocked_hours_utc": "1,3,10,11,12",
    "regime_side_modes": "TREND_UP:yes_only",
}

TREND_DOWN_NO_TAKER_RESOLUTION = {
    "mode": "taker",
    "exit_mode": "next_candle",
    "take_profit_pct": 0.12,
    "stop_loss_pct": 0.08,
    "entry_thresholds": "0.36,0.38",
    "sides": "no_only",
    "adaptive_stand_down": "true",
    "stand_down_window": 8,
    "stand_down_margin_pp": 1.5,
    "stand_down_cooldown_candles": 48,
    "min_confidence_margin": 0.03,
    "require_regime_alignment": True,
    "regime_allowlist": "TREND_DOWN",
    "regime_side_modes": "TREND_DOWN:no_only",
}


PRESETS = {
    "trend_up_yes_specialist": TREND_UP_YES_SPECIALIST,
    "trend_up_yes_recent_specialist": TREND_UP_YES_RECENT_SPECIALIST,
    "trend_up_yes_hour_filtered_specialist": TREND_UP_YES_HOUR_FILTERED_SPECIALIST,
    "trend_up_yes_tod_hour_filtered_specialist": TREND_UP_YES_TOD_HOUR_FILTERED_SPECIALIST,
    "trend_directional_specialist": TREND_DIRECTIONAL_SPECIALIST,
    "taker_resolution_baseline": TAKER_RESOLUTION_BASELINE,
    "taker_tp_sl_probe": TAKER_TP_SL_PROBE,
    "trend_up_yes_taker_tp_sl": TREND_UP_YES_TAKER_TP_SL,
    "trend_up_yes_taker_resolution": TREND_UP_YES_TAKER_RESOLUTION,
    "trend_up_yes_taker_hour_filtered": TREND_UP_YES_TAKER_HOUR_FILTERED,
    "trend_down_no_taker_resolution": TREND_DOWN_NO_TAKER_RESOLUTION,
}


def apply_preset(args, defaults=None) -> None:
    preset = getattr(args, "preset", "").strip().lower()
    if not preset:
        return
    values = PRESETS.get(preset)
    if values is None:
        valid = ", ".join(sorted(PRESETS))
        raise ValueError(f"unknown preset '{args.preset}', expected one of: {valid}")
    for key, value in values.items():
        if defaults is not None and hasattr(defaults, key) and hasattr(args, key):
            if getattr(args, key) != getattr(defaults, key):
                continue
        setattr(args, key, value)
