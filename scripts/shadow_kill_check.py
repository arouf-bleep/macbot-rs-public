#!/usr/bin/env python3
"""
Shadow-mode kill criteria checker.

Reads trade history from the running API and enforces hard pass/fail thresholds.
Exit codes:
  0 = configured historical criteria passed (no live-operation approval)
  2 = FAIL (kill criteria triggered)
  1 = runtime/parse error
"""

from __future__ import annotations

import json
import math
import os
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, Dict, List

from api_auth import get_json, resolve_api_key


@dataclass
class Thresholds:
    min_trades: int = 200
    min_win_rate: float = 0.53
    min_expectancy_usd: float = 0.003
    min_profit_factor: float = 1.03
    max_drawdown_pct: float = 25.0
    min_recent_trades: int = 50
    min_recent_win_rate: float = 0.50

    def __post_init__(self):
        if self.min_trades < 1 or self.min_recent_trades < 1:
            raise ValueError("trade sample thresholds must be positive")
        values = [self.min_win_rate, self.min_expectancy_usd, self.min_profit_factor, self.max_drawdown_pct, self.min_recent_win_rate]
        if not all(math.isfinite(value) for value in values):
            raise ValueError("thresholds must be finite")
        if not 0 <= self.min_win_rate <= 1 or not 0 <= self.min_recent_win_rate <= 1 or self.max_drawdown_pct < 0 or self.min_profit_factor < 0:
            raise ValueError("invalid probability, drawdown or profit-factor threshold")

    @classmethod
    def from_env(cls) -> "Thresholds":
        return cls(
            min_trades=int(os.getenv("SHADOW_MIN_TRADES", "200")),
            min_win_rate=float(os.getenv("SHADOW_MIN_WIN_RATE", "0.53")),
            min_expectancy_usd=float(os.getenv("SHADOW_MIN_EXPECTANCY_USD", "0.003")),
            min_profit_factor=float(os.getenv("SHADOW_MIN_PROFIT_FACTOR", "1.03")),
            max_drawdown_pct=float(os.getenv("SHADOW_MAX_DRAWDOWN_PCT", "25.0")),
            min_recent_trades=int(os.getenv("SHADOW_MIN_RECENT_TRADES", "50")),
            min_recent_win_rate=float(os.getenv("SHADOW_MIN_RECENT_WIN_RATE", "0.50")),
        )


def fetch_json(url: str) -> Dict[str, Any]:
    return get_json(url, timeout=10, read_auth=bool(resolve_api_key(None)))


def parse_ts_key(trade: Dict[str, Any]) -> float:
    raw = trade.get("exit_timestamp") or trade.get("timestamp")
    if not isinstance(raw, str) or not raw:
        raise ValueError("closed shadow trade has no timestamp")
    parsed = datetime.fromisoformat(raw.replace("Z", "+00:00"))
    parsed = parsed.replace(tzinfo=timezone.utc) if parsed.tzinfo is None else parsed
    return parsed.timestamp()


def as_float(v: Any, default: float = 0.0) -> float:
    try:
        value = float(v)
        return value if math.isfinite(value) else default
    except (TypeError, ValueError):
        return default


def main() -> int:
    api_base = os.getenv("MACBOT_API_BASE", "http://localhost:8000").rstrip("/")
    th = Thresholds.from_env()

    try:
        trades_payload = fetch_json(f"{api_base}/api/trades")
        config_payload = fetch_json(f"{api_base}/api/config")
    except (urllib.error.URLError, RuntimeError) as e:
        print(json.dumps({"status": "error", "error": f"api_unreachable: {e}"}, indent=2))
        return 1
    except json.JSONDecodeError as e:
        print(json.dumps({"status": "error", "error": f"invalid_json: {e}"}, indent=2))
        return 1

    trades = trades_payload.get("trades", [])
    if not isinstance(trades, list):
        print(json.dumps({"status": "error", "error": "api_trades_shape_invalid"}, indent=2))
        return 1

    initial_balance = as_float(config_payload.get("initial_balance"), 0.0)

    closed_shadow: List[Dict[str, Any]] = [
        t for t in trades
        if isinstance(t, dict) and str(t.get("status", "")).upper() == "CLOSED"
        and (
            str(t.get("source", "")).lower() == "shadow"
            or str(t.get("mode", "")).lower() == "shadow"
        )
    ]
    if any(not math.isfinite(as_float(t.get("pnl"), math.nan)) for t in closed_shadow):
        print(json.dumps({"status": "error", "error": "shadow_trade_pnl_missing_or_invalid"}))
        return 1

    try:
        closed_shadow.sort(key=parse_ts_key)
    except (ValueError, OverflowError, OSError):
        print(json.dumps({"status": "error", "error": "shadow_trade_timestamp_missing_or_invalid"}))
        return 1
    n = len(closed_shadow)

    wins = sum(1 for t in closed_shadow if as_float(t.get("pnl")) > 0.0)
    losses = n - wins
    total_pnl = sum(as_float(t.get("pnl")) for t in closed_shadow)
    gross_win = sum(as_float(t.get("pnl")) for t in closed_shadow if as_float(t.get("pnl")) > 0.0)
    gross_loss_abs = -sum(as_float(t.get("pnl")) for t in closed_shadow if as_float(t.get("pnl")) < 0.0)

    win_rate = (wins / n) if n > 0 else 0.0
    expectancy = (total_pnl / n) if n > 0 else 0.0
    profit_factor = (gross_win / gross_loss_abs) if gross_loss_abs > 0 else (999.0 if gross_win > 0 else 0.0)

    # Max drawdown from cumulative realized PnL.
    running = 0.0
    peak = 0.0
    max_dd = 0.0
    for t in closed_shadow:
        running += as_float(t.get("pnl"))
        if running > peak:
            peak = running
        dd = peak - running
        if dd > max_dd:
            max_dd = dd
    balance_available = math.isfinite(initial_balance) and initial_balance > 0
    max_dd_pct = (max_dd / initial_balance * 100.0) if balance_available else None

    recent = closed_shadow[-th.min_recent_trades:] if th.min_recent_trades > 0 else []
    recent_n = len(recent)
    recent_wr = (
        sum(1 for t in recent if as_float(t.get("pnl")) > 0.0) / recent_n
        if recent_n > 0
        else 0.0
    )

    checks = {
        "min_trades": n >= th.min_trades,
        "min_win_rate": win_rate >= th.min_win_rate,
        "min_expectancy_usd": expectancy >= th.min_expectancy_usd,
        "min_profit_factor": profit_factor >= th.min_profit_factor,
        "drawdown_denominator_available": balance_available,
        "max_drawdown_pct": max_dd_pct is not None and max_dd_pct <= th.max_drawdown_pct,
        "min_recent_win_rate": (recent_n >= th.min_recent_trades and recent_wr >= th.min_recent_win_rate),
    }

    status = "pass" if all(checks.values()) else "fail"
    report = {
        "status": status,
        "api_base": api_base,
        "thresholds": th.__dict__,
        "metrics": {
            "closed_shadow_trades": n,
            "wins": wins,
            "losses": losses,
            "win_rate": round(win_rate, 6),
            "expectancy_usd_per_trade": round(expectancy, 6),
            "profit_factor": round(profit_factor, 6),
            "total_pnl_usd": round(total_pnl, 6),
            "max_drawdown_usd": round(max_dd, 6),
            "max_drawdown_pct": round(max_dd_pct, 6) if max_dd_pct is not None else None,
            "recent_trades": recent_n,
            "recent_win_rate": round(recent_wr, 6),
            "initial_balance": round(initial_balance, 6),
        },
        "checks": checks,
    }
    print(json.dumps(report, indent=2))

    return 0 if status == "pass" else 2


if __name__ == "__main__":
    sys.exit(main())
