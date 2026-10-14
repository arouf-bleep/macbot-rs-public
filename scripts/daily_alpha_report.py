#!/usr/bin/env python3
"""Generate a daily walk-forward alpha report with hard pass/fail criteria.

This script runs a parameter sweep through /api/backtest, computes walk-forward
robustness stats for each candidate, emits top candidates, and evaluates the
best candidate against explicit kill criteria.

Exit codes:
  0 = PASS (alpha gate satisfied)
  2 = FAIL (alpha gate not satisfied)
  1 = runtime/parse error
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import time
from dataclasses import dataclass, asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

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

    @property
    def total_folds(self) -> int:
        return len(self.fold_results)


@dataclass
class Gates:
    min_mean_pnl: float
    min_median_pnl: float
    min_worst_pnl: float
    min_positive_fold_ratio: float
    min_mean_edge_gap_pp: float
    min_mean_trades: float

    @classmethod
    def from_env_and_args(cls, args: argparse.Namespace) -> "Gates":
        return cls(
            min_mean_pnl=float(os.getenv("ALPHA_MIN_MEAN_PNL", str(args.min_mean_pnl))),
            min_median_pnl=float(
                os.getenv("ALPHA_MIN_MEDIAN_PNL", str(args.min_median_pnl))
            ),
            min_worst_pnl=float(os.getenv("ALPHA_MIN_WORST_PNL", str(args.min_worst_pnl))),
            min_positive_fold_ratio=float(
                os.getenv(
                    "ALPHA_MIN_POSITIVE_FOLD_RATIO",
                    str(args.min_positive_fold_ratio),
                )
            ),
            min_mean_edge_gap_pp=float(
                os.getenv("ALPHA_MIN_MEAN_EDGE_GAP_PP", str(args.min_mean_edge_gap_pp))
            ),
            min_mean_trades=float(os.getenv("ALPHA_MIN_MEAN_TRADES", str(args.min_mean_trades))),
        )


def csv_floats(raw: str) -> list[float]:
    out: list[float] = []
    for part in raw.split(","):
        s = part.strip()
        if s:
            out.append(float(s))
    return out


def csv_ints(raw: str) -> list[int]:
    out: list[int] = []
    for part in raw.split(","):
        s = part.strip()
        if s:
            out.append(int(s))
    return out


def csv_sides(raw: str) -> list[str]:
    allowed = {"both", "yes_only", "no_only"}
    out: list[str] = []
    for part in raw.split(","):
        s = part.strip().lower()
        if not s:
            continue
        if s not in allowed:
            raise ValueError(f"invalid side '{s}', expected one of {sorted(allowed)}")
        out.append(s)
    return out


def csv_bools(raw: str) -> list[bool]:
    lut = {"true": True, "false": False, "1": True, "0": False, "yes": True, "no": False}
    out: list[bool] = []
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
    raise ValueError(f"unknown side '{side}'")


def iter_candidates(
    entry_thresholds: list[float],
    sides: list[str],
    adaptive_values: list[bool],
) -> list[Candidate]:
    out: list[Candidate] = []
    for th in entry_thresholds:
        for side in sides:
            for adaptive in adaptive_values:
                out.append(
                    Candidate(
                        entry_threshold=float(th),
                        side=side,
                        adaptive_stand_down=bool(adaptive),
                    )
                )
    return out


def rank_key(score: CandidateScore) -> tuple[float, float, float, float, int]:
    return (
        score.mean_pnl,
        score.median_pnl,
        score.worst_pnl,
        score.mean_edge_gap_pp,
        score.positive_folds,
    )


def evaluate_gates(score: CandidateScore, gates: Gates) -> tuple[str, dict[str, bool]]:
    ratio = (
        score.positive_folds / score.total_folds if score.total_folds > 0 else 0.0
    )
    checks = {
        "trade_evidence_present": bool(score.fold_results) and all(r.trades > 0 for r in score.fold_results),
        "min_mean_pnl": score.mean_pnl >= gates.min_mean_pnl,
        "min_median_pnl": score.median_pnl >= gates.min_median_pnl,
        "min_worst_pnl": score.worst_pnl >= gates.min_worst_pnl,
        "min_positive_fold_ratio": ratio >= gates.min_positive_fold_ratio,
        "min_mean_edge_gap_pp": score.mean_edge_gap_pp >= gates.min_mean_edge_gap_pp,
        "min_mean_trades": score.mean_trades >= gates.min_mean_trades,
    }
    status = "pass" if all(checks.values()) else "fail"
    return status, checks


def candidate_to_json(score: CandidateScore) -> dict[str, Any]:
    return {
        "candidate": asdict(score.candidate),
        "stats": {
            "mean_pnl": score.mean_pnl,
            "median_pnl": score.median_pnl,
            "worst_pnl": score.worst_pnl,
            "best_pnl": score.best_pnl,
            "mean_edge_gap_pp": score.mean_edge_gap_pp,
            "mean_trades": score.mean_trades,
            "positive_folds": score.positive_folds,
            "total_folds": score.total_folds,
        },
        "folds": [asdict(fr) for fr in score.fold_results],
    }


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_alpha_report.json"
    md_path = out_dir / f"{ts}_alpha_report.md"

    with json_path.open("w", encoding="utf-8") as f:
        json.dump(report, f, indent=2)

    top = report.get("top_candidates", [])
    lines = [
        "# Daily Alpha Report",
        "",
        f"- status: **{report.get('status', 'unknown').upper()}**",
        f"- generated_at_utc: {report.get('generated_at_utc')}",
        f"- api_base: {report.get('api_base')}",
        "",
        "## Gate Checks",
    ]
    checks = report.get("checks", {})
    for k, v in checks.items():
        lines.append(f"- {k}: {'PASS' if v else 'FAIL'}")

    lines.extend(["", "## Top Candidates", "", "| rank | th | side | stand_down | mean | median | worst | pos_folds | mean_gap_pp | mean_trades |", "| ---: | ---: | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |"])
    for i, row in enumerate(top, start=1):
        c = row["candidate"]
        s = row["stats"]
        lines.append(
            f"| {i} | {c['entry_threshold']:.3f} | {c['side']} | {str(c['adaptive_stand_down']).lower()} | "
            f"{s['mean_pnl']:.4f} | {s['median_pnl']:.4f} | {s['worst_pnl']:.4f} | "
            f"{s['positive_folds']}/{s['total_folds']} | {s['mean_edge_gap_pp']:.2f} | {s['mean_trades']:.1f} |"
        )

    with md_path.open("w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")

    return json_path, md_path


def main() -> int:
    p = argparse.ArgumentParser(description="Daily alpha robustness report")
    p.add_argument("--preset", default="", help="named sweep preset (e.g. trend_up_yes_specialist)")
    p.add_argument("--base-url", default="http://localhost:8000")
    p.add_argument("--output-dir", default="artifacts/alpha-reports")
    p.add_argument("--top", type=int, default=10)
    p.add_argument("--request-timeout-secs", type=int, default=120)
    p.add_argument("--anchor-shift-days", type=int, default=0)

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
    p.add_argument("--high-volatility-threshold", type=float, default=0.004)
    p.add_argument("--time-of-day-weight", action="store_true")
    p.add_argument("--require-regime-alignment", action="store_true")
    p.add_argument("--regime-allowlist", default="")
    p.add_argument("--session-allowlist", default="")
    p.add_argument("--blocked-hours-utc", default="")
    p.add_argument("--regime-side-modes", default="")
    p.add_argument("--request-interval-secs", type=float, default=1.1)

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

    p.add_argument("--entry-thresholds", default="0.34,0.36,0.38,0.40")
    p.add_argument("--sides", default="both,no_only")
    p.add_argument("--adaptive-stand-down", default="true,false")
    p.add_argument("--fold-shifts", default="0,5,10,15,20,25")

    p.add_argument("--min-mean-pnl", type=float, default=0.0)
    p.add_argument("--min-median-pnl", type=float, default=0.0)
    p.add_argument("--min-worst-pnl", type=float, default=-1.5)
    p.add_argument("--min-positive-fold-ratio", type=float, default=0.60)
    p.add_argument("--min-mean-edge-gap-pp", type=float, default=0.0)
    p.add_argument("--min-mean-trades", type=float, default=30.0)
    add_api_auth_args(p)

    defaults = p.parse_args([])
    args = p.parse_args()
    apply_preset(args, defaults)
    require_resolution_edge_mode(args.exit_mode)
    gates = Gates.from_env_and_args(args)

    try:
        entry_thresholds = csv_floats(args.entry_thresholds)
        sides = csv_sides(args.sides)
        adaptive_values = csv_bools(args.adaptive_stand_down)
        fold_shifts = csv_ints(args.fold_shifts)
        regime_allowlist = csv_regimes(args.regime_allowlist)
        session_allowlist = csv_sessions(args.session_allowlist)
        blocked_hours_utc = csv_hours(args.blocked_hours_utc)
        regime_side_modes = csv_regime_side_modes(args.regime_side_modes)
    except ValueError as exc:
        print(json.dumps({"status": "error", "error": str(exc)}, indent=2))
        return 1

    if not entry_thresholds or not sides or not adaptive_values or not fold_shifts:
        print(
            json.dumps(
                {
                    "status": "error",
                    "error": "one or more sweep dimensions are empty",
                },
                indent=2,
            )
        )
        return 1
    if args.days <= 0 or args.anchor_shift_days < 0 or any(shift < 0 for shift in fold_shifts):
        raise SystemExit("days must be positive and shifts must be nonnegative")

    api_url = args.base_url.rstrip("/") + "/api/backtest"
    now_ms = int(time.time() * 1000) - args.anchor_shift_days * DAY_MS
    candidates = iter_candidates(entry_thresholds, sides, adaptive_values)

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
        "time_of_day_weight": args.time_of_day_weight,
        "take_profit_pct": args.take_profit_pct,
        "stop_loss_pct": args.stop_loss_pct,
    }

    scores: list[CandidateScore] = []
    try:
        for cand in candidates:
            allow_yes, allow_no = side_to_flags(cand.side)
            folds: list[FoldResult] = []
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
                    api_url,
                    payload,
                    timeout=args.request_timeout_secs,
                    api_key=args.api_key,
                    api_secret=args.api_secret,
                    require_auth=True,
                )
                if args.request_interval_secs > 0:
                    time.sleep(args.request_interval_secs)
                folds.append(
                    FoldResult(
                        shift_days=shift,
                        pnl=float(out.get("total_pnl", 0.0)),
                        edge_gap_pp=edge_gap_from_result(out),
                        trades=int(out.get("total_trades", 0)),
                        win_rate=float(out.get("win_rate", 0.0)),
                    )
                )
            scores.append(CandidateScore(candidate=cand, fold_results=folds))
    except (RuntimeError, ValueError) as exc:
        print(json.dumps({"status": "error", "error": str(exc)}, indent=2))
        return 1

    scores.sort(key=rank_key, reverse=True)
    if not scores:
        print(json.dumps({"status": "error", "error": "no scores generated"}, indent=2))
        return 1

    best = scores[0]
    status, checks = evaluate_gates(best, gates)

    report = {
        "methodology": "historical candidate selection; folds may overlap; no untouched final holdout",
        "status": status,
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "api_base": args.base_url.rstrip("/"),
        "mode": args.mode,
        "exit_mode": args.exit_mode,
        "take_profit_pct": args.take_profit_pct,
        "stop_loss_pct": args.stop_loss_pct,
        "anchor_shift_days": args.anchor_shift_days,
        "require_regime_alignment": args.require_regime_alignment,
        "regime_allowlist": regime_allowlist,
        "regime_side_modes": regime_side_modes,
        "fold_shifts_days": fold_shifts,
        "gates": asdict(gates),
        "checks": checks,
        "best_candidate": candidate_to_json(best),
        "top_candidates": [
            candidate_to_json(s) for s in scores[: max(1, args.top)]
        ],
    }

    out_dir = Path(args.output_dir)
    json_path, md_path = write_outputs(report, out_dir)
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}

    print(json.dumps(report, indent=2))
    return 0 if status == "pass" else 2


if __name__ == "__main__":
    raise SystemExit(main())
