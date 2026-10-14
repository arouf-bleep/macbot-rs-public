#!/usr/bin/env python3
"""Walk-forward sweep for alpha robustness via /api/backtest.

Ranks candidate parameter sets by rolling historical stability (mean/median/worst PnL,
positive-fold count) across rolling end-time anchors.
These folds select candidates and may overlap; they are not an untouched final holdout.

Example:
  python3 scripts/walkforward_alpha_sweep.py \
    --mode maker \
    --days 14 \
    --fold-shifts 0,5,10,15,20,25 \
    --entry-thresholds 0.34,0.36,0.38,0.40 \
    --sides both,no_only \
    --adaptive-stand-down true,false
"""

from __future__ import annotations

import argparse
import statistics
import time
from dataclasses import dataclass
from typing import Iterable

from alpha_presets import apply_preset
from api_auth import add_api_auth_args, post_json
from backtest_trade_report_common import edge_gap_from_result, require_resolution_edge_mode


DAY_MS = 86_400_000


@dataclass
class Candidate:
    entry_threshold: float
    side: str
    adaptive_stand_down: bool


@dataclass
class FoldResult:
    shift_days: int
    pnl: float
    edge_gap_pp: float
    trades: int
    win_rate: float


@dataclass
class CandidateScore:
    candidate: Candidate
    fold_results: list[FoldResult]

    @property
    def mean_pnl(self) -> float:
        return statistics.mean(r.pnl for r in self.fold_results)

    @property
    def median_pnl(self) -> float:
        return statistics.median(r.pnl for r in self.fold_results)

    @property
    def worst_pnl(self) -> float:
        return min(r.pnl for r in self.fold_results)

    @property
    def best_pnl(self) -> float:
        return max(r.pnl for r in self.fold_results)

    @property
    def mean_edge_gap_pp(self) -> float:
        return statistics.mean(r.edge_gap_pp for r in self.fold_results)

    @property
    def mean_trades(self) -> float:
        return statistics.mean(r.trades for r in self.fold_results)

    @property
    def positive_folds(self) -> int:
        return sum(1 for r in self.fold_results if r.pnl > 0)


def csv_floats(raw: str) -> list[float]:
    out: list[float] = []
    for part in raw.split(","):
        s = part.strip()
        if not s:
            continue
        out.append(float(s))
    return out


def csv_ints(raw: str) -> list[int]:
    out: list[int] = []
    for part in raw.split(","):
        s = part.strip()
        if not s:
            continue
        out.append(int(s))
    return out


def csv_sides(raw: str) -> list[str]:
    allowed = {"both", "yes_only", "no_only"}
    out = []
    for part in raw.split(","):
        s = part.strip().lower()
        if not s:
            continue
        if s not in allowed:
            raise ValueError(f"invalid side '{s}', expected one of {sorted(allowed)}")
        out.append(s)
    return out


def csv_bools(raw: str) -> list[bool]:
    lut = {
        "true": True,
        "false": False,
        "1": True,
        "0": False,
        "yes": True,
        "no": False,
    }
    out = []
    for part in raw.split(","):
        s = part.strip().lower()
        if not s:
            continue
        if s not in lut:
            raise ValueError(f"invalid boolean '{s}'")
        out.append(lut[s])
    return out


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


def side_to_flags(side: str) -> tuple[bool, bool]:
    if side == "both":
        return True, True
    if side == "yes_only":
        return True, False
    if side == "no_only":
        return False, True
    raise ValueError(f"unknown side {side}")


def iter_candidates(
    entry_thresholds: Iterable[float],
    sides: Iterable[str],
    adaptive_stand_down: Iterable[bool],
) -> Iterable[Candidate]:
    for th in entry_thresholds:
        for side in sides:
            for sd in adaptive_stand_down:
                yield Candidate(entry_threshold=th, side=side, adaptive_stand_down=sd)


def rank_key(score: CandidateScore) -> tuple[float, float, float, float, int]:
    return (
        score.mean_pnl,
        score.median_pnl,
        score.worst_pnl,
        score.mean_edge_gap_pp,
        score.positive_folds,
    )


def main() -> int:
    p = argparse.ArgumentParser(description="Walk-forward alpha robustness sweep")
    p.add_argument("--preset", default="", help="named sweep preset (e.g. trend_up_yes_specialist)")
    p.add_argument("--base-url", default="http://localhost:8000")
    p.add_argument("--days", type=int, default=14)
    p.add_argument("--initial-balance", type=float, default=10.0)
    p.add_argument("--window", type=int, default=60)
    p.add_argument("--max-trade-pct", type=float, default=0.05)
    p.add_argument("--min-trade-usd", type=float, default=0.25)
    p.add_argument("--gas-usd", type=float, default=0.02)
    p.add_argument("--liquidity-proxy-usd", type=float, default=10000.0)
    p.add_argument("--anti-bias-strength", type=float, default=0.10)
    p.add_argument("--high-vol-entry-threshold", type=float, default=0.44)
    p.add_argument("--min-confidence-margin", type=float, default=0.0)
    p.add_argument("--time-of-day-weight", action="store_true")

    p.add_argument("--mode", choices=("taker", "maker"), default="maker")
    p.add_argument("--maker-fee-pct", type=float, default=0.0)
    p.add_argument("--maker-slippage-factor", type=float, default=0.2)
    p.add_argument("--maker-gas-usd", type=float, default=0.0)
    p.add_argument(
        "--exit-mode",
        choices=("next_candle", "taker_tp_sl"),
        default="next_candle",
    )
    p.add_argument("--take-profit-pct", type=float, default=0.12)
    p.add_argument("--stop-loss-pct", type=float, default=0.08)

    p.add_argument("--stand-down-window", type=int, default=12)
    p.add_argument("--stand-down-margin-pp", type=float, default=0.0)
    p.add_argument("--stand-down-cooldown-candles", type=int, default=8)
    p.add_argument("--high-volatility-threshold", type=float, default=0.004)
    p.add_argument("--require-regime-alignment", action="store_true")
    p.add_argument("--regime-allowlist", default="")
    p.add_argument("--session-allowlist", default="")
    p.add_argument("--blocked-hours-utc", default="")
    p.add_argument("--regime-side-modes", default="")
    p.add_argument("--request-interval-secs", type=float, default=1.1)

    p.add_argument("--entry-thresholds", default="0.34,0.36,0.38,0.40")
    p.add_argument("--sides", default="both,no_only")
    p.add_argument("--adaptive-stand-down", default="true,false")
    p.add_argument("--fold-shifts", default="0,5,10,15,20,25")
    p.add_argument("--top", type=int, default=10)
    p.add_argument("--request-timeout-secs", type=int, default=120)
    add_api_auth_args(p)

    defaults = p.parse_args([])
    args = p.parse_args()
    apply_preset(args, defaults)
    require_resolution_edge_mode(args.exit_mode)

    entry_thresholds = csv_floats(args.entry_thresholds)
    sides = csv_sides(args.sides)
    adaptive_values = csv_bools(args.adaptive_stand_down)
    fold_shifts = csv_ints(args.fold_shifts)
    regime_allowlist = csv_regimes(args.regime_allowlist)
    session_allowlist = csv_sessions(args.session_allowlist)
    blocked_hours_utc = csv_hours(args.blocked_hours_utc)
    regime_side_modes = csv_regime_side_modes(args.regime_side_modes)

    if not entry_thresholds:
        raise SystemExit("no entry thresholds provided")
    if not sides:
        raise SystemExit("no sides provided")
    if not adaptive_values:
        raise SystemExit("no adaptive_stand_down values provided")
    if not fold_shifts:
        raise SystemExit("no fold shifts provided")
    if args.days <= 0 or any(shift < 0 for shift in fold_shifts):
        raise SystemExit("days must be positive and fold shifts must be nonnegative")

    now_ms = int(time.time() * 1000)
    url = args.base_url.rstrip("/") + "/api/backtest"

    base_payload = {
        "days": args.days,
        "initial_balance": args.initial_balance,
        "window": args.window,
        "max_trade_pct": args.max_trade_pct,
        "min_trade_usd": args.min_trade_usd,
        "gas_usd": args.gas_usd,
        "liquidity_proxy_usd": args.liquidity_proxy_usd,
        "anti_bias_strength": args.anti_bias_strength,
        "high_vol_entry_threshold": args.high_vol_entry_threshold,
        "min_confidence_margin": args.min_confidence_margin,
        "high_volatility_threshold": args.high_volatility_threshold,
        "time_of_day_weight": args.time_of_day_weight,
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

    scores: list[CandidateScore] = []
    for cand in iter_candidates(entry_thresholds, sides, adaptive_values):
        allow_yes, allow_no = side_to_flags(cand.side)
        fold_results: list[FoldResult] = []
        for shift in fold_shifts:
            payload = dict(base_payload)
            payload.update(
                {
                    "entry_threshold": cand.entry_threshold,
                    "adaptive_stand_down": cand.adaptive_stand_down,
                    "allow_buy_yes": allow_yes,
                    "allow_buy_no": allow_no,
                    "end_time_ms": now_ms - shift * DAY_MS,
                }
            )
            out = post_json(
                url,
                payload,
                timeout=args.request_timeout_secs,
                api_key=args.api_key,
                api_secret=args.api_secret,
                require_auth=True,
            )
            if args.request_interval_secs > 0:
                time.sleep(args.request_interval_secs)
            fold_results.append(
                FoldResult(
                    shift_days=shift,
                    pnl=float(out.get("total_pnl", 0.0)),
                    edge_gap_pp=edge_gap_from_result(out),
                    trades=int(out.get("total_trades", 0)),
                    win_rate=float(out.get("win_rate", 0.0)),
                )
            )
        scores.append(CandidateScore(candidate=cand, fold_results=fold_results))

    scores.sort(key=rank_key, reverse=True)

    print("Walk-Forward Alpha Sweep")
    print("Historical candidate selection; folds may overlap. No untouched final holdout is measured.")
    print(
        f"mode={args.mode} folds={fold_shifts} candidates={len(scores)} days={args.days} "
        f"balance={args.initial_balance} exit_mode={args.exit_mode} "
        f"tp={args.take_profit_pct:.3f} sl={args.stop_loss_pct:.3f}"
    )
    print(
        "rank,entry_threshold,side,adaptive_stand_down,mean_pnl,median_pnl,worst_pnl,best_pnl,"
        "positive_folds,mean_edge_gap_pp,mean_trades"
    )

    top = max(1, args.top)
    for i, score in enumerate(scores[:top], start=1):
        c = score.candidate
        print(
            f"{i},{c.entry_threshold:.3f},{c.side},{str(c.adaptive_stand_down).lower()},"
            f"{score.mean_pnl:.4f},{score.median_pnl:.4f},{score.worst_pnl:.4f},"
            f"{score.best_pnl:.4f},{score.positive_folds}/{len(score.fold_results)},"
            f"{score.mean_edge_gap_pp:.2f},{score.mean_trades:.1f}"
        )

    if scores:
        print("\nTop candidate fold breakdown")
        top_score = scores[0]
        c = top_score.candidate
        print(
            f"entry_threshold={c.entry_threshold:.3f} side={c.side} "
            f"adaptive_stand_down={str(c.adaptive_stand_down).lower()}"
        )
        print("shift_days,pnl,edge_gap_pp,trades,win_rate")
        for fr in top_score.fold_results:
            print(
                f"{fr.shift_days},{fr.pnl:.4f},{fr.edge_gap_pp:.2f},{fr.trades},{fr.win_rate:.2f}"
            )

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
