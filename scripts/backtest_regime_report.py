#!/usr/bin/env python3
"""Run /api/backtest and print a compact per-regime attribution report.

Examples:
  python3 scripts/backtest_regime_report.py
  python3 scripts/backtest_regime_report.py --mode maker --days 14 --entry-threshold 0.45
"""

from __future__ import annotations

import argparse
import json
import math
import sys

from api_auth import add_api_auth_args, post_json


def metric(v: object, digits: int = 2, suffix: str = "") -> str:
    if v is None or isinstance(v, bool):
        return "unavailable"
    try:
        value = float(v)
    except (TypeError, ValueError, OverflowError):
        return "unavailable"
    if not math.isfinite(value):
        return "unavailable"
    return f"{value:.{digits}f}{suffix}"


def pct(v: object) -> str:
    return metric(v, suffix="%")


def render_report(out: dict) -> str:
    edge = out.get("edge_audit", {}) or {}
    by_regime = edge.get("by_regime", {}) or {}
    lines = [
        "Backtest Summary",
        f"  mode={out.get('execution_mode')} days={out.get('days')} trades={out.get('total_trades')}",
        f"  final_balance={metric(out.get('final_balance'), 4)} total_pnl={metric(out.get('total_pnl'), 4)}",
        f"  win_rate={pct(out.get('win_rate'))} "
        f"be_wr={pct(edge.get('break_even_win_rate_pct'))} "
        f"edge_gap={metric(edge.get('edge_gap_pp'), suffix='pp')}",
        "",
        "Per-Regime Attribution",
        "regime,trades,win_rate,be_wr,edge_gap_pp,expectancy,total_pnl,avg_cost_pct",
    ]
    for regime, row in sorted(by_regime.items()):
        values = [
            regime, metric(row.get("trades"), 0), metric(row.get("win_rate")),
            metric(row.get("break_even_win_rate_pct")), metric(row.get("edge_gap_pp")),
            metric(row.get("expectancy_per_trade"), 4), metric(row.get("total_pnl"), 4),
            metric(row.get("avg_cost_pct")),
        ]
        lines.append(",".join(values))
    return "\n".join(lines)


def csv_regimes(raw: str) -> list[str]:
    out: list[str] = []
    for part in raw.split(","):
        s = part.strip().upper()
        if s:
            out.append(s)
    return out


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


def csv_regime_side_modes(raw: str) -> dict[str, str]:
    out: dict[str, str] = {}
    if not raw.strip():
        return out
    allowed = {"both", "yes_only", "no_only", "none"}
    for part in raw.split(","):
        item = part.strip()
        if not item:
            continue
        if ":" not in item:
            raise ValueError(
                f"invalid regime side mode '{item}', expected REGIME:mode"
            )
        regime, mode = item.split(":", 1)
        regime_key = regime.strip().upper()
        mode_value = mode.strip().lower()
        if not regime_key:
            raise ValueError(f"invalid regime side mode '{item}', empty regime")
        if mode_value not in allowed:
            raise ValueError(
                f"invalid regime side mode '{item}', expected one of {sorted(allowed)}"
            )
        out[regime_key] = mode_value
    return out


def main() -> int:
    p = argparse.ArgumentParser(description="Backtest regime attribution report")
    p.add_argument("--base-url", default="http://localhost:8000")
    p.add_argument("--days", type=int, default=14)
    p.add_argument("--initial-balance", type=float, default=10.0)
    p.add_argument("--entry-threshold", type=float, default=0.30)
    p.add_argument("--max-trade-pct", type=float, default=0.05)
    p.add_argument("--min-trade-usd", type=float, default=0.25)
    p.add_argument("--gas-usd", type=float, default=0.02)
    p.add_argument("--liquidity-proxy-usd", type=float, default=10000.0)
    p.add_argument("--mode", choices=("taker", "maker"), default="taker")
    p.add_argument("--maker-fee-pct", type=float, default=0.0)
    p.add_argument("--maker-slippage-factor", type=float, default=0.2)
    p.add_argument("--maker-gas-usd", type=float, default=0.0)
    p.add_argument("--high-vol-entry-threshold", type=float, default=0.44)
    p.add_argument("--min-confidence-margin", type=float, default=0.0)
    p.add_argument("--high-volatility-threshold", type=float, default=0.004)
    p.add_argument("--require-regime-alignment", action="store_true")
    p.add_argument("--regime-allowlist", default="")
    p.add_argument("--session-allowlist", default="")
    p.add_argument("--blocked-hours-utc", default="")
    p.add_argument("--regime-side-modes", default="")
    p.add_argument("--request-timeout-secs", type=int, default=60)
    add_api_auth_args(p)
    args = p.parse_args()

    payload = {
        "days": args.days,
        "initial_balance": args.initial_balance,
        "entry_threshold": args.entry_threshold,
        "max_trade_pct": args.max_trade_pct,
        "min_trade_usd": args.min_trade_usd,
        "gas_usd": args.gas_usd,
        "liquidity_proxy_usd": args.liquidity_proxy_usd,
        "high_vol_entry_threshold": args.high_vol_entry_threshold,
        "min_confidence_margin": args.min_confidence_margin,
        "high_volatility_threshold": args.high_volatility_threshold,
        "require_regime_alignment": args.require_regime_alignment,
        "regime_allowlist": csv_regimes(args.regime_allowlist),
        "session_allowlist": csv_sessions(args.session_allowlist),
        "blocked_hours_utc": csv_hours(args.blocked_hours_utc),
        "regime_side_modes": csv_regime_side_modes(args.regime_side_modes),
        "execution_mode": args.mode,
        "maker_fee_pct": args.maker_fee_pct,
        "maker_slippage_factor": args.maker_slippage_factor,
        "maker_gas_usd": args.maker_gas_usd,
    }

    url = args.base_url.rstrip("/") + "/api/backtest"
    try:
        out = post_json(
            url,
            payload,
            timeout=args.request_timeout_secs,
            api_key=args.api_key,
            api_secret=args.api_secret,
            require_auth=True,
        )
    except RuntimeError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    if "error" in out:
        print(json.dumps(out, indent=2))
        return 2

    print(render_report(out))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
