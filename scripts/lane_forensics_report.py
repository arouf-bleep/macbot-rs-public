#!/usr/bin/env python3
"""Generate a lane-level forensic report from /api/backtest trade logs.

This is meant for alpha debugging after a lane collapses. It uses the per-trade
backtest payload to answer: which hours, vol buckets, confidence buckets, and
cost proxies are poisoning the lane?
"""

from __future__ import annotations

import argparse
import json
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from alpha_presets import apply_preset
from api_auth import add_api_auth_args, post_json
from backtest_trade_report_common import (
    require_resolution_edge_mode, analyze_trades, metric_text,
)


DAY_MS = 86_400_000


def csv_regimes(raw: str) -> list[str]:
    return [part.strip().upper() for part in raw.split(",") if part.strip()]


def csv_sessions(raw: str) -> list[str]:
    out: list[str] = []
    allowed = {"ASIA", "EUROPE", "US"}
    for part in raw.split(","):
        s = part.strip().upper()
        if not s:
            continue
        if s not in allowed:
            raise ValueError(f"invalid session '{s}', expected one of {sorted(allowed)}")
        out.append(s.lower())
    return out


def csv_hours(raw: str) -> list[int]:
    out: list[int] = []
    for part in raw.split(","):
        s = part.strip()
        if not s:
            continue
        hour = int(s)
        if hour < 0 or hour > 23:
            raise ValueError(f"invalid hour '{s}', expected 0-23")
        out.append(hour)
    return out


def csv_signals(raw: str) -> list[str]:
    return [part.strip().upper() for part in raw.split(",") if part.strip()]


def csv_regime_side_modes(raw: str) -> dict[str, str]:
    out: dict[str, str] = {}
    if not raw.strip():
        return out
    for part in raw.split(","):
        item = part.strip()
        if not item:
            continue
        if ":" not in item:
            raise ValueError(f"invalid regime side mode '{item}', expected REGIME:mode")
        regime, mode = item.split(":", 1)
        out[regime.strip().upper()] = mode.strip().lower()
    return out


def parse_bool_like(raw: Any) -> bool:
    if isinstance(raw, bool):
        return raw
    if isinstance(raw, str):
        value = raw.strip().lower()
        if value in {"true", "1", "yes"}:
            return True
        if value in {"false", "0", "no"}:
            return False
    raise ValueError(f"invalid boolean value '{raw}'")


def first_threshold(raw: Any) -> float | None:
    if raw is None:
        return None
    if isinstance(raw, (int, float)):
        return float(raw)
    if isinstance(raw, str):
        for part in raw.split(","):
            s = part.strip()
            if s:
                return float(s)
    return None


def write_outputs(report: dict[str, Any], out_dir: Path, stem: str) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_{stem}.json"
    md_path = out_dir / f"{ts}_{stem}.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    lines = [
        "# Lane Forensics Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- api_base: {report['api_base']}",
        f"- lane_label: {report['lane_label']}",
        f"- trades: {report['summary']['trades']}",
        f"- total_pnl: {metric_text(report['summary']['total_pnl'], 4)}",
        f"- win_rate: {metric_text(report['summary']['win_rate'], 2)}%",
        f"- edge_gap_pp: {metric_text(report['summary']['edge_gap_pp'], 2)}",
        "",
        "## Limitations",
        "",
        "- This report uses candle-based backtest logs. It does not have true historical orderbook spread/depth or intramarket time-to-expiry.",
        "- Cost buckets use the backtest cost proxy (`total_cost / stake`) rather than real spread snapshots.",
    ]
    for section in ("by_session", "by_hour_utc", "by_volatility_bucket", "by_confidence_margin_bucket", "by_edge_margin_bucket"):
        rows = report.get(section, [])
        lines.extend(["", f"## {section}", ""])
        lines.append("| bucket | trades | win_rate | be_wr | edge_gap_pp | total_pnl | avg_pnl | avg_cost_pct |")
        lines.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
        for row in rows:
            lines.append(
                f"| {row['bucket']} | {row['trades']} | {metric_text(row['win_rate'], 2)} | "
                f"{metric_text(row['break_even_win_rate_pct'], 2)} | {metric_text(row['edge_gap_pp'], 2)} | "
                f"{metric_text(row['total_pnl'], 4)} | {metric_text(row['avg_pnl'], 4)} | {metric_text(row['avg_cost_pct'], 2)} |"
            )
    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Lane forensics report from /api/backtest")
    parser.add_argument("--preset", default="", help="named sweep preset")
    parser.add_argument("--base-url", default="http://localhost:8000")
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    parser.add_argument("--lane-label", default="lane")
    parser.add_argument("--days", type=int, default=7)
    parser.add_argument("--shift-days", type=int, default=0)
    parser.add_argument("--request-timeout-secs", type=int, default=120)

    parser.add_argument("--initial-balance", type=float, default=10.0)
    parser.add_argument("--window", type=int, default=60)
    parser.add_argument("--max-trade-pct", type=float, default=0.05)
    parser.add_argument("--min-trade-usd", type=float, default=0.25)
    parser.add_argument("--gas-usd", type=float, default=0.02)
    parser.add_argument("--liquidity-proxy-usd", type=float, default=10000.0)
    parser.add_argument("--anti-bias-strength", type=float, default=0.10)
    parser.add_argument("--high-vol-entry-threshold", type=float, default=0.44)
    parser.add_argument("--high-volatility-threshold", type=float, default=0.004)
    parser.add_argument("--min-confidence-margin", type=float, default=0.0)
    parser.add_argument("--time-of-day-weight", action="store_true")
    parser.add_argument("--mode", choices=("taker", "maker"), default="maker")
    parser.add_argument("--maker-fee-pct", type=float, default=0.0)
    parser.add_argument("--maker-slippage-factor", type=float, default=0.2)
    parser.add_argument("--maker-gas-usd", type=float, default=0.0)
    parser.add_argument(
        "--exit-mode",
        choices=("next_candle", "taker_tp_sl"),
        default="next_candle",
    )
    parser.add_argument("--take-profit-pct", type=float, default=0.12)
    parser.add_argument("--stop-loss-pct", type=float, default=0.08)

    parser.add_argument("--entry-threshold", type=float, default=0.36)
    parser.add_argument("--allow-buy-yes", action="store_true")
    parser.add_argument("--allow-buy-no", action="store_true")
    parser.add_argument("--adaptive-stand-down", action="store_true")
    parser.add_argument("--stand-down-window", type=int, default=8)
    parser.add_argument("--stand-down-margin-pp", type=float, default=1.5)
    parser.add_argument("--stand-down-cooldown-candles", type=int, default=48)
    parser.add_argument("--require-regime-alignment", action="store_true")
    parser.add_argument("--regime-allowlist", default="")
    parser.add_argument("--session-allowlist", default="")
    parser.add_argument("--blocked-hours-utc", default="")
    parser.add_argument("--regime-side-modes", default="")
    parser.add_argument("--filter-regimes", default="")
    parser.add_argument("--filter-signals", default="")
    add_api_auth_args(parser)

    defaults = parser.parse_args([])
    args = parser.parse_args()
    apply_preset(args, defaults)
    require_resolution_edge_mode(args.exit_mode)

    if args.preset and args.entry_threshold == defaults.entry_threshold:
        preset_threshold = first_threshold(getattr(args, "entry_thresholds", None))
        if preset_threshold is not None:
            args.entry_threshold = preset_threshold

    args.adaptive_stand_down = parse_bool_like(args.adaptive_stand_down)

    if args.preset:
        if args.allow_buy_yes == defaults.allow_buy_yes and args.allow_buy_no == defaults.allow_buy_no:
            args.allow_buy_yes = "yes_only" in getattr(args, "sides", "") or getattr(args, "sides", "") == "both"
            args.allow_buy_no = "no_only" in getattr(args, "sides", "") or getattr(args, "sides", "") == "both"

    regime_allowlist = csv_regimes(args.regime_allowlist)
    session_allowlist = csv_sessions(args.session_allowlist)
    blocked_hours_utc = csv_hours(args.blocked_hours_utc)
    regime_side_modes = csv_regime_side_modes(args.regime_side_modes)
    filter_regimes = set(csv_regimes(args.filter_regimes))
    filter_signals = set(csv_signals(args.filter_signals))

    payload = {
        "days": args.days,
        "initial_balance": args.initial_balance,
        "window": args.window,
        "entry_threshold": args.entry_threshold,
        "max_trade_pct": args.max_trade_pct,
        "min_trade_usd": args.min_trade_usd,
        "gas_usd": args.gas_usd,
        "liquidity_proxy_usd": args.liquidity_proxy_usd,
        "anti_bias_strength": args.anti_bias_strength,
        "high_vol_entry_threshold": args.high_vol_entry_threshold,
        "high_volatility_threshold": args.high_volatility_threshold,
        "min_confidence_margin": args.min_confidence_margin,
        "time_of_day_weight": args.time_of_day_weight,
        "allow_buy_yes": args.allow_buy_yes,
        "allow_buy_no": args.allow_buy_no,
        "adaptive_stand_down": args.adaptive_stand_down,
        "stand_down_window": args.stand_down_window,
        "stand_down_margin_pp": args.stand_down_margin_pp,
        "stand_down_cooldown_candles": args.stand_down_cooldown_candles,
        "require_regime_alignment": args.require_regime_alignment,
        "regime_allowlist": regime_allowlist,
        "session_allowlist": session_allowlist,
        "blocked_hours_utc": blocked_hours_utc,
        "regime_side_modes": regime_side_modes,
        "execution_mode": args.mode,
        "maker_fee_pct": args.maker_fee_pct,
        "maker_slippage_factor": args.maker_slippage_factor,
        "maker_gas_usd": args.maker_gas_usd,
        "exit_mode": args.exit_mode,
        "take_profit_pct": args.take_profit_pct,
        "stop_loss_pct": args.stop_loss_pct,
    }
    if args.shift_days:
        payload["end_time_ms"] = int(time.time() * 1000) - args.shift_days * DAY_MS
    out = post_json(
        args.base_url.rstrip("/") + "/api/backtest",
        payload,
        timeout=args.request_timeout_secs,
        api_key=args.api_key,
        api_secret=args.api_secret,
        require_auth=True,
    )
    trades = out.get("trades", []) or []
    filtered: list[dict[str, Any]] = []
    for trade in trades:
        if filter_regimes and str(trade.get("regime", "")).upper() not in filter_regimes:
            continue
        if filter_signals and str(trade.get("signal", "")).upper() not in filter_signals:
            continue
        filtered.append(trade)

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "api_base": args.base_url.rstrip("/"),
        "lane_label": args.lane_label,
        "config": {
            "days": args.days,
            "shift_days": args.shift_days,
            "end_time_ms": payload.get("end_time_ms"),
            "entry_threshold": args.entry_threshold,
            "execution_mode": args.mode,
            "exit_mode": args.exit_mode,
            "take_profit_pct": args.take_profit_pct,
            "stop_loss_pct": args.stop_loss_pct,
            "allow_buy_yes": args.allow_buy_yes,
            "allow_buy_no": args.allow_buy_no,
            "adaptive_stand_down": args.adaptive_stand_down,
            "stand_down_window": args.stand_down_window,
            "stand_down_margin_pp": args.stand_down_margin_pp,
            "stand_down_cooldown_candles": args.stand_down_cooldown_candles,
            "require_regime_alignment": args.require_regime_alignment,
            "regime_allowlist": regime_allowlist,
            "session_allowlist": session_allowlist,
            "blocked_hours_utc": blocked_hours_utc,
            "regime_side_modes": regime_side_modes,
            "filter_regimes": sorted(filter_regimes),
            "filter_signals": sorted(filter_signals),
        },
        **{key: value for key, value in analyze_trades(
            filtered, high_vol_threshold=args.high_volatility_threshold).items()
           if key in {"summary", "by_session", "by_hour_utc", "by_volatility_bucket",
                      "by_confidence_margin_bucket", "by_edge_margin_bucket", "worst_trades",
                      "best_trades", "ranking_unavailable_trade_count"}},
    }

    out_dir = Path(args.output_dir)
    stem = f"{args.lane_label}_lane_forensics"
    json_path, md_path = write_outputs(report, out_dir, stem)
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
