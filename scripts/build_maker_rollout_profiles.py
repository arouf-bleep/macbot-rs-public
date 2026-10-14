#!/usr/bin/env python3
"""Materialize frozen maker paper-rollout profiles from the checked-in base config."""

from __future__ import annotations

import argparse
import json
import os
import tempfile
from pathlib import Path
from typing import Any


COMMON_OVERRIDES: dict[str, Any] = {
    "mode": "simulation",
    "execution_mode": "maker",
    "auto_trading": True,
    "auto_refresh": True,
    "sim_live_execution": True,
    "shadow_trading": False,
    "time_of_day_weight": False,
    "maker_live_quoting": True,
    "maker_buy_no_guard_enabled": True,
    "maker_buy_no_max_p_up": 0.40,
    "maker_buy_no_max_entry_price": 0.78,
    "maker_buy_no_min_edge": 0.06,
    "maker_candidate_min_remaining_seconds": 300,
    "maker_market_quality_max_spread_bps": 160.0,
    "maker_backwardation_guard_enabled": False,
    "maker_backwardation_guard_shadow_only": True,
    "maker_backwardation_guard_pct": -0.02,
    "maker_roll_toxic_guard_enabled": False,
    "maker_roll_toxic_guard_shadow_only": True,
    "maker_roll_toxic_min_confidence": 0.15,
    "maker_roll_toxic_min_signal": 0.05,
    "maker_roll_toxic_min_sample_count": 6,
    "macro_event_guard_enabled": True,
    "regime_allowlist": ["TREND_UP"],
    "session_allowlist": ["asia", "us"],
    "maker_regime_allowlist": [],
    "maker_session_allowlist": [],
    "blocked_hours_utc": [1, 3, 4, 6, 16, 17, 18, 19, 20],
    "min_confidence_margin": 0.03,
}


def build_profile(base_cfg: dict[str, Any], **overrides: Any) -> dict[str, Any]:
    profile = {**base_cfg, **COMMON_OVERRIDES, **overrides}
    # Generated configs are portable research artifacts. Runtime credentials
    # must come from environment overrides, never copied private base settings.
    for field in ("api_key", "api_hmac_secret", "private_rpc_url", "finnhub_api_key", "lunarcrush_api_key"):
        if field in profile:
            profile[field] = ""
    return profile


def write_profile(path: Path, cfg: dict[str, Any]) -> None:
    # Publish privately and atomically, including over an existing public file.
    descriptor, temporary = tempfile.mkstemp(prefix=".profile-", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            json.dump(cfg, stream, indent=2, allow_nan=False)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def widen_candidate_label(widen_bps: float, floor_bps: float) -> str:
    label = f"widen_{int(round(widen_bps))}bps"
    if floor_bps > 0.0:
        label += f"_floor{int(round(floor_bps))}bps"
    return label


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build frozen maker paper-rollout config profiles"
    )
    parser.add_argument("--base-config", default="config.json", help="Base config JSON path")
    parser.add_argument(
        "--outdir",
        default="ops/profiles",
        help="Directory for generated frozen rollout configs",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any]:
    cfg = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(cfg, dict):
        raise ValueError("base config must be a JSON object")
    return cfg


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    base_path = (root / args.base_config).resolve()
    outdir = (root / args.outdir).resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    base_cfg = load_json(base_path)
    profiles: dict[str, dict[str, Any]] = {
        "maker_paper_baseline.json": build_profile(
            base_cfg,
            maker_backwardation_widen_bps=0.0,
            maker_backwardation_min_target_spread_bps=0.0,
        ),
        "maker_paper_validation_control.json": build_profile(
            base_cfg,
            blocked_hours_utc=[],
            maker_backwardation_widen_bps=0.0,
            maker_backwardation_min_target_spread_bps=0.0,
        ),
        "maker_paper_validation_shutdown.json": build_profile(
            base_cfg,
            blocked_hours_utc=[],
            market_filter={"type": "basic", "slug_pattern": "updown"},
            maker_backwardation_widen_bps=0.0,
            maker_backwardation_min_target_spread_bps=0.0,
            take_profit=False,
            stop_loss=False,
            min_confidence_margin=0.0,
            maker_buy_no_min_edge=0.01,
            maker_candidate_min_remaining_seconds=180,
            maker_market_quality_max_spread_bps=300.0,
            maker_fill_cooldown_ms=1000,
            maker_quote_notional_usd=3.0,
            maker_max_inventory_notional_usd=30.0,
            max_open_positions=4,
        ),
        "maker_paper_widen20.json": build_profile(
            base_cfg,
            maker_backwardation_widen_bps=20.0,
            maker_backwardation_min_target_spread_bps=0.0,
        ),
        "maker_paper_widen40_floor120.json": build_profile(
            base_cfg,
            maker_backwardation_widen_bps=40.0,
            maker_backwardation_min_target_spread_bps=120.0,
        ),
        "maker_paper_rollguard_c15_s5_n6.json": build_profile(
            base_cfg,
            maker_backwardation_widen_bps=0.0,
            maker_backwardation_min_target_spread_bps=0.0,
            maker_roll_toxic_guard_enabled=True,
            maker_roll_toxic_guard_shadow_only=False,
            maker_roll_toxic_min_confidence=0.15,
            maker_roll_toxic_min_signal=0.05,
            maker_roll_toxic_min_sample_count=6,
        ),
    }

    emitted: list[str] = []
    for name, cfg in profiles.items():
        path = outdir / name
        write_profile(path, cfg)
        emitted.append(str(path))

    print(json.dumps({"profiles": emitted}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
