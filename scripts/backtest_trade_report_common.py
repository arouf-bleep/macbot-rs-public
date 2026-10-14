#!/usr/bin/env python3
"""Shared helpers for backtest trade forensic reports."""

from __future__ import annotations

import time
import math
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Any, Callable

from alpha_presets import apply_preset
from api_auth import post_json


DAY_MS = 86_400_000


def require_resolution_edge_mode(exit_mode: str) -> None:
    if exit_mode != "next_candle":
        raise ValueError(
            "These edge reports require next_candle resolution returns; "
            "TP/SL has asymmetric payoffs and no supported break-even win-rate model"
        )


def edge_gap_from_result(result: dict[str, Any]) -> float:
    edge = result.get("edge_audit")
    value = edge.get("edge_gap_pp") if isinstance(edge, dict) else None
    if isinstance(value, bool) or not isinstance(value, (float, int)) or not math.isfinite(value):
        raise ValueError("backtest edge_gap_pp is unavailable or invalid; cannot rank this evidence")
    return float(value)


def redacted_command(command: list[str]) -> list[str]:
    out: list[str] = []
    hide_next = False
    for arg in command:
        if hide_next:
            out.append("[redacted]")
            hide_next = False
        elif arg in {"--api-key", "--api-secret"}:
            out.append(arg)
            hide_next = True
        elif arg.startswith(("--api-key=", "--api-secret=")):
            out.append(arg.split("=", 1)[0] + "=[redacted]")
        else:
            out.append(arg)
    return out

NUMERIC_FIELDS: list[tuple[str, str]] = [
    ("score", "score"),
    ("confidence_margin", "confidence_margin"),
    ("edge_margin_pp", "edge_margin_pp"),
    ("rsi", "rsi"),
    ("macd_hist", "macd_hist"),
    ("macd_hist_delta", "macd_hist_delta"),
    ("momentum_pct", "momentum_pct"),
    ("volatility", "volatility"),
    ("next_return_pct", "next_return_pct"),
    ("holding_minutes", "holding_minutes"),
    ("cost_pct", "cost_pct"),
    ("realized_return_pct", "realized_return_pct"),
    ("path_upside_pct", "path_upside_pct"),
    ("path_downside_pct", "path_downside_pct"),
]


@dataclass
class BucketStats:
    trades: int = 0
    wins: int = 0
    totals: dict[str, float] = field(default_factory=dict)
    observations: dict[str, int] = field(default_factory=dict)

    def push(self, trade: dict[str, Any]) -> None:
        self.trades += 1
        if isinstance(trade.get("won"), bool):
            self.wins += int(trade["won"])
            self.observations["won"] = self.observations.get("won", 0) + 1
        for key in ("pnl", "cost_pct", "confidence_margin", "edge_margin_pp", "volatility"):
            value = optional_numeric_value(trade, key)
            if value is not None:
                self.totals[key] = self.totals.get(key, 0.0) + value
                self.observations[key] = self.observations.get(key, 0) + 1

    def complete_mean(self, key: str) -> float | None:
        if not self.trades or self.observations.get(key, 0) != self.trades:
            return None
        value = self.totals[key] / self.trades
        return value if math.isfinite(value) else None

    def as_json(self, label: str) -> dict[str, Any]:
        win_rate = (self.wins / self.trades * 100.0
                    if self.trades and self.observations.get("won", 0) == self.trades else None)
        avg_cost_pct = self.complete_mean("cost_pct")
        # This report is restricted to the symmetric next-candle resolution proxy.
        be_wr = 50.0 + avg_cost_pct * 0.5 if avg_cost_pct is not None else None
        return {
            "bucket": label,
            "trades": self.trades,
            "wins": self.wins,
            "win_rate": win_rate,
            "break_even_win_rate_pct": be_wr,
            "edge_gap_pp": win_rate - be_wr if win_rate is not None and be_wr is not None else None,
            "total_pnl": (self.totals.get("pnl", 0.0)
                          if self.observations.get("pnl", 0) == self.trades
                          and math.isfinite(self.totals.get("pnl", 0.0)) else None),
            "avg_pnl": self.complete_mean("pnl"),
            "avg_cost_pct": avg_cost_pct,
            "avg_confidence_margin": self.complete_mean("confidence_margin"),
            "avg_edge_margin_pp": self.complete_mean("edge_margin_pp"),
            "avg_volatility": self.complete_mean("volatility"),
            "metric_observations": {key: self.observations.get(key, 0) for key in
                                    ("won", "pnl", "cost_pct", "confidence_margin", "edge_margin_pp", "volatility")},
        }


def csv_regimes(raw: str) -> list[str]:
    return [part.strip().upper() for part in raw.split(",") if part.strip()]


def csv_signals(raw: str) -> list[str]:
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


def csv_ints(raw: str) -> list[int]:
    return [int(part.strip()) for part in raw.split(",") if part.strip()]


def csv_strings(raw: str) -> list[str]:
    return [part.strip() for part in raw.split(",") if part.strip()]


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


def resolve_args(args: Any, defaults: Any) -> None:
    apply_preset(args, defaults)
    args.adaptive_stand_down = parse_bool_like(args.adaptive_stand_down)
    if args.preset and args.entry_threshold == defaults.entry_threshold:
        preset_threshold = first_threshold(getattr(args, "entry_thresholds", None))
        if preset_threshold is not None:
            args.entry_threshold = preset_threshold
    if args.preset:
        if args.allow_buy_yes == defaults.allow_buy_yes and args.allow_buy_no == defaults.allow_buy_no:
            sides = getattr(args, "sides", "")
            args.allow_buy_yes = "yes_only" in sides or sides == "both"
            args.allow_buy_no = "no_only" in sides or sides == "both"


def add_common_backtest_args(parser: Any) -> None:
    parser.add_argument("--preset", default="", help="named sweep preset")
    parser.add_argument("--base-url", default="http://localhost:8000")
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


def build_payload(args: Any, *, days: int, shift_days: int) -> dict[str, Any]:
    require_resolution_edge_mode(args.exit_mode)
    payload = {
        "days": days,
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
        "allow_buy_yes": args.allow_buy_yes,
        "allow_buy_no": args.allow_buy_no,
        "adaptive_stand_down": args.adaptive_stand_down,
        "stand_down_window": args.stand_down_window,
        "stand_down_margin_pp": args.stand_down_margin_pp,
        "stand_down_cooldown_candles": args.stand_down_cooldown_candles,
        "require_regime_alignment": args.require_regime_alignment,
        "regime_allowlist": csv_regimes(args.regime_allowlist),
        "session_allowlist": csv_sessions(args.session_allowlist),
        "blocked_hours_utc": csv_hours(args.blocked_hours_utc),
        "regime_side_modes": csv_regime_side_modes(args.regime_side_modes),
        "execution_mode": args.mode,
        "maker_fee_pct": args.maker_fee_pct,
        "maker_slippage_factor": args.maker_slippage_factor,
        "maker_gas_usd": args.maker_gas_usd,
        "exit_mode": args.exit_mode,
        "take_profit_pct": args.take_profit_pct,
        "stop_loss_pct": args.stop_loss_pct,
    }
    if shift_days:
        payload["end_time_ms"] = int(time.time() * 1000) - shift_days * DAY_MS
    return payload


def run_backtest(args: Any, *, days: int, shift_days: int) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    payload = build_payload(args, days=days, shift_days=shift_days)
    out = post_json(
        args.base_url.rstrip("/") + "/api/backtest",
        payload,
        timeout=args.request_timeout_secs,
        api_key=args.api_key,
        api_secret=args.api_secret,
        require_auth=True,
    )
    trades = out.get("trades", []) or []
    filtered = filter_trades(
        trades,
        filter_regimes=set(csv_regimes(args.filter_regimes)),
        filter_signals=set(csv_signals(args.filter_signals)),
    )
    return payload, filtered


def filter_trades(
    trades: list[dict[str, Any]],
    *,
    filter_regimes: set[str],
    filter_signals: set[str],
) -> list[dict[str, Any]]:
    filtered: list[dict[str, Any]] = []
    for trade in trades:
        if filter_regimes and str(trade.get("regime", "")).upper() not in filter_regimes:
            continue
        if filter_signals and str(trade.get("signal", "")).upper() not in filter_signals:
            continue
        filtered.append(trade)
    return filtered


def avg(values: list[float]) -> float:
    return math.fsum(value / len(values) for value in values) if values else 0.0


def median(values: list[float]) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return ordered[mid - 1] / 2.0 + ordered[mid] / 2.0


def optional_numeric_value(trade: dict[str, Any], key: str) -> float | None:
    required = {
        "cost_pct": ("stake", "total_cost"),
        "realized_return_pct": ("stake", "pnl"),
        "path_upside_pct": ("entry_price", "path_high_price"),
        "path_downside_pct": ("entry_price", "path_low_price"),
    }.get(key, (key,))
    try:
        if any(trade.get(name) is None or isinstance(trade[name], bool) for name in required):
            return None
        values = [float(trade[name]) for name in required]
        if not all(math.isfinite(value) for value in values):
            return None
        if key in {"cost_pct", "realized_return_pct", "path_upside_pct", "path_downside_pct"}:
            denominator, numerator = values
            if denominator <= 0:
                return None
            if key == "path_upside_pct":
                numerator -= denominator
            elif key == "path_downside_pct":
                numerator = denominator - numerator
            value = numerator / denominator * 100.0
        else:
            value = values[0]
        return value if math.isfinite(value) else None
    except (ValueError, TypeError, OverflowError):
        return None


def numeric_value(trade: dict[str, Any], key: str) -> float:
    value = optional_numeric_value(trade, key)
    if value is None:
        raise ValueError(f"{key} is unavailable or invalid")
    return value


def metric_text(value: Any, decimals: int = 2) -> str:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value):
        return "unavailable"
    return f"{value:.{decimals}f}"


def summary_metric_delta(left: dict[str, Any], right: dict[str, Any], key: str) -> float | None:
    lhs, rhs = optional_numeric_value(left, key), optional_numeric_value(right, key)
    value = lhs - rhs if lhs is not None and rhs is not None else None
    return value if value is not None and math.isfinite(value) else None


def summary_from_trades(trades: list[dict[str, Any]]) -> dict[str, Any]:
    stats = BucketStats()
    for trade in trades:
        stats.push(trade)
    row = stats.as_json("all")
    return {key: row[key] for key in ("trades", "wins", "win_rate", "break_even_win_rate_pct",
                                    "edge_gap_pp", "total_pnl", "avg_pnl", "avg_cost_pct", "metric_observations")}


def bucket_sort_key(row: dict[str, Any]) -> tuple[float, float, int]:
    # Unknown observations cannot rank as a fabricated flat result.
    pnl = optional_numeric_value(row, "total_pnl")
    edge = optional_numeric_value(row, "edge_gap_pp")
    return (pnl if pnl is not None else float("-inf"),
            edge if edge is not None else float("-inf"), row["trades"])


def numeric_profile(trades: list[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    profile: dict[str, dict[str, Any]] = {}
    for key, label in NUMERIC_FIELDS:
        values = [value for trade in trades if (value := optional_numeric_value(trade, key)) is not None]
        profile[label] = {
            "count": len(values),
            "missing_count": len(trades) - len(values),
            "mean": avg(values) if values else None,
            "median": median(values) if values else None,
            "min": min(values) if values else None,
            "max": max(values) if values else None,
        }
    return profile


def winner_loser_profile(trades: list[dict[str, Any]]) -> dict[str, Any]:
    winners = [trade for trade in trades if trade.get("won") is True]
    losers = [trade for trade in trades if trade.get("won") is False]
    winner_profile = numeric_profile(winners)
    loser_profile = numeric_profile(losers)
    deltas: dict[str, float | None] = {}
    for _, label in NUMERIC_FIELDS:
        left, right = winner_profile[label]["mean"], loser_profile[label]["mean"]
        deltas[label] = left - right if left is not None and right is not None else None
    return {
        "winner_count": len(winners),
        "loser_count": len(losers),
        "unknown_outcome_count": len(trades) - len(winners) - len(losers),
        "winner_means": {label: row["mean"] for label, row in winner_profile.items()},
        "loser_means": {label: row["mean"] for label, row in loser_profile.items()},
        "winner_minus_loser_mean": deltas,
    }


def bucket_session(hour: int) -> str:
    if 0 <= hour <= 7:
        return "asia"
    if 8 <= hour <= 15:
        return "europe"
    return "us"


def bucket_volatility(vol: float, high_vol_threshold: float) -> str:
    if vol < high_vol_threshold * 0.5:
        return "low"
    if vol < high_vol_threshold:
        return "medium"
    return "high"


def bucket_confidence_margin(margin: float) -> str:
    if margin < 0.03:
        return "thin"
    if margin < 0.08:
        return "medium"
    return "thick"


def bucket_edge_margin(edge_margin_pp: float) -> str:
    if edge_margin_pp < 1.0:
        return "<1pp"
    if edge_margin_pp < 3.0:
        return "1-3pp"
    return "3pp+"


def summarize_buckets(
    trades: list[dict[str, Any]],
    bucket_fn: Callable[[dict[str, Any]], str],
) -> list[dict[str, Any]]:
    buckets: dict[str, BucketStats] = defaultdict(BucketStats)
    for trade in trades:
        buckets[bucket_fn(trade)].push(trade)
    rows = [stats.as_json(label) for label, stats in buckets.items()]
    rows.sort(key=bucket_sort_key)
    return rows


def observed_bucket(trade: dict[str, Any], key: str, classify: Callable[[float], str]) -> str:
    value = optional_numeric_value(trade, key)
    return classify(value) if value is not None else "unavailable"


def observed_hour_bucket(trade: dict[str, Any], *, session: bool) -> str:
    hour = optional_numeric_value(trade, "entry_hour_utc")
    if hour is None or not hour.is_integer() or not 0 <= hour <= 23:
        return "unavailable"
    return bucket_session(int(hour)) if session else f"{int(hour):02d}"


def analyze_trades(trades: list[dict[str, Any]], *, high_vol_threshold: float) -> dict[str, Any]:
    ranked = [trade for trade in trades if optional_numeric_value(trade, "pnl") is not None]
    ordered = sorted(ranked, key=lambda trade: numeric_value(trade, "pnl"))
    return {
        "summary": summary_from_trades(trades),
        "numeric_profile": numeric_profile(trades),
        "winner_loser_profile": winner_loser_profile(trades),
        "by_regime": summarize_buckets(trades, lambda t: str(t.get("regime", "UNKNOWN")).upper()),
        "by_signal": summarize_buckets(trades, lambda t: str(t.get("signal", "UNKNOWN")).upper()),
        "by_session": summarize_buckets(trades, lambda t: observed_hour_bucket(t, session=True)),
        "by_hour_utc": summarize_buckets(trades, lambda t: observed_hour_bucket(t, session=False)),
        "by_volatility_bucket": summarize_buckets(trades, lambda t: observed_bucket(
            t, "volatility", lambda value: bucket_volatility(value, high_vol_threshold))),
        "by_confidence_margin_bucket": summarize_buckets(trades, lambda t: observed_bucket(
            t, "confidence_margin", bucket_confidence_margin)),
        "by_edge_margin_bucket": summarize_buckets(trades, lambda t: observed_bucket(
            t, "edge_margin_pp", bucket_edge_margin)),
        "ranking_unavailable_trade_count": len(trades) - len(ranked),
        "worst_trades": ordered[:5],
        "best_trades": list(reversed(ordered))[:5],
    }


def top_bucket_rows(rows: list[dict[str, Any]], *, limit: int = 5, reverse: bool = False) -> list[dict[str, Any]]:
    ordered = sorted(
        [row for row in rows if optional_numeric_value(row, "total_pnl") is not None],
        key=bucket_sort_key,
        reverse=reverse,
    )
    return ordered[:limit]
