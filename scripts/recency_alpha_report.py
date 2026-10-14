#!/usr/bin/env python3
"""Evaluate recency-adaptive alpha candidates with explicit drift gates.

This report is aimed at "recent specialist" strategies where we want:
- strong recent-window behavior
- non-catastrophic medium-window behavior
- an explicit stress-window check before any canary thinking
"""

from __future__ import annotations

import argparse
import json
import statistics
import time
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from alpha_presets import apply_preset
from api_auth import add_api_auth_args, post_json
from backtest_trade_report_common import edge_gap_from_result, require_resolution_edge_mode


DAY_MS = 86_400_000


@dataclass(frozen=True)
class Candidate:
    entry_threshold: float
    side: str
    adaptive_stand_down: bool
    stand_down_window: int
    stand_down_margin_pp: float
    stand_down_cooldown_candles: int


@dataclass
class FoldResult:
    shift_days: int
    pnl: float
    edge_gap_pp: float
    trades: int
    win_rate: float


@dataclass
class WindowScore:
    days: int
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

    @property
    def positive_fold_ratio(self) -> float:
        return self.positive_folds / self.total_folds if self.total_folds else 0.0


@dataclass
class CandidateScore:
    candidate: Candidate
    primary_windows: list[WindowScore]
    stress_window: WindowScore | None

    @property
    def weighted_primary_mean_pnl(self) -> float:
        total_days = sum(w.days for w in self.primary_windows)
        if total_days <= 0:
            return 0.0
        return sum(w.mean_pnl * (w.days / total_days) for w in self.primary_windows)

    @property
    def weighted_primary_edge_gap_pp(self) -> float:
        total_days = sum(w.days for w in self.primary_windows)
        if total_days <= 0:
            return 0.0
        return sum(
            w.mean_edge_gap_pp * (w.days / total_days) for w in self.primary_windows
        )

    @property
    def worst_primary_mean_pnl(self) -> float:
        return min(w.mean_pnl for w in self.primary_windows)

    @property
    def best_primary_mean_pnl(self) -> float:
        return max(w.mean_pnl for w in self.primary_windows)

    @property
    def primary_pass_count(self) -> int:
        return sum(1 for w in self.primary_windows if w.mean_pnl > 0.0)

    @property
    def primary_decay_pnl(self) -> float:
        if len(self.primary_windows) < 2:
            return 0.0
        recent = self.primary_windows[0].mean_pnl
        oldest = self.primary_windows[-1].mean_pnl
        return recent - oldest

    @property
    def stress_decay_pnl(self) -> float:
        if self.stress_window is None or not self.primary_windows:
            return 0.0
        return self.primary_windows[0].mean_pnl - self.stress_window.mean_pnl


@dataclass
class Gates:
    min_primary_mean_pnl: float
    min_primary_positive_fold_ratio: float
    min_primary_mean_edge_gap_pp: float
    min_oldest_primary_positive_fold_ratio: float
    min_stress_mean_pnl: float
    min_stress_positive_fold_ratio: float
    max_primary_decay_pnl: float
    max_stress_decay_pnl: float


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
    stand_down_windows: list[int],
    stand_down_margin_pps: list[float],
    stand_down_cooldowns: list[int],
) -> list[Candidate]:
    out: list[Candidate] = []
    for threshold in entry_thresholds:
        for side in sides:
            for adaptive in adaptive_values:
                for window in stand_down_windows:
                    for margin in stand_down_margin_pps:
                        for cooldown in stand_down_cooldowns:
                            out.append(
                                Candidate(
                                    entry_threshold=float(threshold),
                                    side=side,
                                    adaptive_stand_down=bool(adaptive),
                                    stand_down_window=int(window),
                                    stand_down_margin_pp=float(margin),
                                    stand_down_cooldown_candles=int(cooldown),
                                )
                            )
    return out


def candidate_to_json(score: CandidateScore) -> dict[str, Any]:
    def window_to_json(window: WindowScore) -> dict[str, Any]:
        return {
            "days": window.days,
            "stats": {
                "mean_pnl": window.mean_pnl,
                "median_pnl": window.median_pnl,
                "worst_pnl": window.worst_pnl,
                "best_pnl": window.best_pnl,
                "mean_edge_gap_pp": window.mean_edge_gap_pp,
                "mean_trades": window.mean_trades,
                "positive_folds": window.positive_folds,
                "total_folds": window.total_folds,
                "positive_fold_ratio": window.positive_fold_ratio,
            },
            "folds": [asdict(fr) for fr in window.fold_results],
        }

    payload = {
        "candidate": asdict(score.candidate),
        "primary_windows": [window_to_json(w) for w in score.primary_windows],
        "composite": {
            "weighted_primary_mean_pnl": score.weighted_primary_mean_pnl,
            "weighted_primary_edge_gap_pp": score.weighted_primary_edge_gap_pp,
            "worst_primary_mean_pnl": score.worst_primary_mean_pnl,
            "best_primary_mean_pnl": score.best_primary_mean_pnl,
            "primary_pass_count": score.primary_pass_count,
            "primary_decay_pnl": score.primary_decay_pnl,
            "stress_decay_pnl": score.stress_decay_pnl,
        },
    }
    if score.stress_window is not None:
        payload["stress_window"] = window_to_json(score.stress_window)
    return payload


def evaluate_gates(score: CandidateScore, gates: Gates) -> tuple[str, dict[str, bool]]:
    primary_means_ok = all(w.mean_pnl >= gates.min_primary_mean_pnl for w in score.primary_windows)
    primary_ratio_ok = all(
        w.positive_fold_ratio >= gates.min_primary_positive_fold_ratio for w in score.primary_windows
    )
    primary_gap_ok = all(
        w.mean_edge_gap_pp >= gates.min_primary_mean_edge_gap_pp for w in score.primary_windows
    )
    oldest_ratio_ok = True
    if score.primary_windows:
        oldest_ratio_ok = (
            score.primary_windows[-1].positive_fold_ratio
            >= gates.min_oldest_primary_positive_fold_ratio
        )
    stress_mean_ok = True
    stress_ratio_ok = True
    if score.stress_window is not None:
        stress_mean_ok = score.stress_window.mean_pnl >= gates.min_stress_mean_pnl
        stress_ratio_ok = (
            score.stress_window.positive_fold_ratio >= gates.min_stress_positive_fold_ratio
        )
    checks = {
        "trade_evidence_present": bool(score.primary_windows)
        and all(w.fold_results and all(r.trades > 0 for r in w.fold_results) for w in score.primary_windows)
        and (score.stress_window is None or bool(score.stress_window.fold_results)
             and all(r.trades > 0 for r in score.stress_window.fold_results)),
        "primary_mean_pnl": primary_means_ok,
        "primary_positive_fold_ratio": primary_ratio_ok,
        "primary_mean_edge_gap_pp": primary_gap_ok,
        "oldest_primary_positive_fold_ratio": oldest_ratio_ok,
        "primary_decay_pnl": score.primary_decay_pnl <= gates.max_primary_decay_pnl,
        "stress_mean_pnl": stress_mean_ok,
        "stress_positive_fold_ratio": stress_ratio_ok,
        "stress_decay_pnl": score.stress_decay_pnl <= gates.max_stress_decay_pnl,
    }
    status = "pass" if all(checks.values()) else "fail"
    return status, checks


def rank_key(item: tuple[CandidateScore, str, dict[str, bool]]) -> tuple[int, float, float, float, float]:
    score, status, _ = item
    return (
        1 if status == "pass" else 0,
        score.weighted_primary_mean_pnl,
        score.primary_windows[0].mean_pnl if score.primary_windows else 0.0,
        score.stress_window.mean_pnl if score.stress_window is not None else 0.0,
        score.weighted_primary_edge_gap_pp,
    )


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_recency_alpha_report.json"
    md_path = out_dir / f"{ts}_recency_alpha_report.md"
    with json_path.open("w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2)

    lines = [
        "# Recency Alpha Report",
        "",
        f"- status: **{report.get('status', 'unknown').upper()}**",
        f"- generated_at_utc: {report.get('generated_at_utc')}",
        f"- api_base: {report.get('api_base')}",
        f"- preset: {report.get('preset', '') or '(custom)'}",
        "",
        "## Gate Checks",
    ]
    for key, value in report.get("checks", {}).items():
        lines.append(f"- {key}: {'PASS' if value else 'FAIL'}")

    best = report.get("best_candidate", {})
    if best:
        lines.extend(["", "## Best Candidate", ""])
        cand = best["candidate"]
        lines.append(
            f"- entry_threshold={cand['entry_threshold']:.3f}, side={cand['side']}, "
            f"adaptive_stand_down={str(cand['adaptive_stand_down']).lower()}, "
            f"stand_down_window={cand['stand_down_window']}, "
            f"stand_down_margin_pp={cand['stand_down_margin_pp']:.2f}, "
            f"cooldown={cand['stand_down_cooldown_candles']}"
        )
        lines.append("")
        lines.append("| window_days | mean | median | worst | pos_folds | mean_gap_pp | mean_trades |")
        lines.append("| ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
        for window in best.get("primary_windows", []):
            stats = window["stats"]
            lines.append(
                f"| {window['days']} | {stats['mean_pnl']:.4f} | {stats['median_pnl']:.4f} | "
                f"{stats['worst_pnl']:.4f} | {stats['positive_folds']}/{stats['total_folds']} | "
                f"{stats['mean_edge_gap_pp']:.2f} | {stats['mean_trades']:.1f} |"
            )
        stress = best.get("stress_window")
        if stress:
            stats = stress["stats"]
            lines.extend(
                [
                    "",
                    "### Stress Window",
                    "",
                    f"- days={stress['days']} mean_pnl={stats['mean_pnl']:.4f} "
                    f"positive_folds={stats['positive_folds']}/{stats['total_folds']} "
                    f"mean_edge_gap_pp={stats['mean_edge_gap_pp']:.2f}",
                ]
            )

    with md_path.open("w", encoding="utf-8") as handle:
        handle.write("\n".join(lines) + "\n")
    return json_path, md_path


def run_window(
    api_url: str,
    now_ms: int,
    base_payload: dict[str, Any],
    candidate: Candidate,
    days: int,
    fold_shifts: list[int],
    *,
    timeout: int,
    api_key: str | None,
    api_secret: str | None,
    request_interval_secs: float,
) -> WindowScore:
    require_resolution_edge_mode(base_payload.get("exit_mode", "next_candle"))
    if days <= 0 or not fold_shifts or any(shift < 0 for shift in fold_shifts):
        raise ValueError("days must be positive and fold shifts must be nonempty and nonnegative")
    allow_yes, allow_no = side_to_flags(candidate.side)
    payload_base = dict(base_payload)
    payload_base.update(
        {
            "days": days,
            "entry_threshold": candidate.entry_threshold,
            "adaptive_stand_down": candidate.adaptive_stand_down,
            "allow_buy_yes": allow_yes,
            "allow_buy_no": allow_no,
            "stand_down_window": candidate.stand_down_window,
            "stand_down_margin_pp": candidate.stand_down_margin_pp,
            "stand_down_cooldown_candles": candidate.stand_down_cooldown_candles,
        }
    )
    folds: list[FoldResult] = []
    for shift in fold_shifts:
        payload = dict(payload_base)
        payload["end_time_ms"] = now_ms - shift * DAY_MS
        out = post_json(
            api_url,
            payload,
            timeout=timeout,
            api_key=api_key,
            api_secret=api_secret,
            require_auth=True,
        )
        if request_interval_secs > 0:
            time.sleep(request_interval_secs)
        folds.append(
            FoldResult(
                shift_days=shift,
                pnl=float(out.get("total_pnl", 0.0)),
                edge_gap_pp=edge_gap_from_result(out),
                trades=int(out.get("total_trades", 0)),
                win_rate=float(out.get("win_rate", 0.0)),
            )
        )
    return WindowScore(days=days, fold_results=folds)


def main() -> int:
    parser = argparse.ArgumentParser(description="Recency-adaptive alpha report")
    parser.add_argument(
        "--preset",
        default="trend_directional_specialist",
        help="named sweep preset (default: trend_directional_specialist)",
    )
    parser.add_argument("--base-url", default="http://localhost:8000")
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    parser.add_argument("--request-timeout-secs", type=int, default=180)
    parser.add_argument("--request-interval-secs", type=float, default=1.2)
    parser.add_argument("--anchor-shift-days", type=int, default=0)

    parser.add_argument("--days-list", default="14,21,30")
    parser.add_argument("--stress-days", type=int, default=60)
    parser.add_argument("--fold-shifts", default="0,7,14,21")
    parser.add_argument("--stress-fold-shifts", default="0,14,28,42,56")

    parser.add_argument("--initial-balance", type=float, default=10.0)
    parser.add_argument("--window", type=int, default=60)
    parser.add_argument("--max-trade-pct", type=float, default=0.05)
    parser.add_argument("--min-trade-usd", type=float, default=0.25)
    parser.add_argument("--gas-usd", type=float, default=0.02)
    parser.add_argument("--liquidity-proxy-usd", type=float, default=10000.0)
    parser.add_argument("--anti-bias-strength", type=float, default=0.10)
    parser.add_argument("--high-vol-entry-threshold", type=float, default=0.44)
    parser.add_argument("--min-confidence-margin", type=float, default=0.0)
    parser.add_argument("--high-volatility-threshold", type=float, default=0.004)
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

    parser.add_argument("--entry-thresholds", default="0.34,0.36,0.38,0.40")
    parser.add_argument("--sides", default="both")
    parser.add_argument("--adaptive-stand-down", default="true")
    parser.add_argument("--stand-down-windows", default="8")
    parser.add_argument("--stand-down-margin-pps", default="1.5,2.0")
    parser.add_argument("--stand-down-cooldowns", default="48")
    parser.add_argument("--require-regime-alignment", action="store_true")
    parser.add_argument("--regime-allowlist", default="")
    parser.add_argument("--session-allowlist", default="")
    parser.add_argument("--blocked-hours-utc", default="")
    parser.add_argument("--regime-side-modes", default="")

    parser.add_argument("--min-primary-mean-pnl", type=float, default=0.0)
    parser.add_argument("--min-primary-positive-fold-ratio", type=float, default=0.50)
    parser.add_argument("--min-primary-mean-edge-gap-pp", type=float, default=0.0)
    parser.add_argument("--min-oldest-primary-positive-fold-ratio", type=float, default=0.25)
    parser.add_argument("--min-stress-mean-pnl", type=float, default=0.0)
    parser.add_argument("--min-stress-positive-fold-ratio", type=float, default=0.20)
    parser.add_argument("--max-primary-decay-pnl", type=float, default=3.0)
    parser.add_argument("--max-stress-decay-pnl", type=float, default=6.0)

    parser.add_argument("--top", type=int, default=10)
    add_api_auth_args(parser)
    defaults = parser.parse_args([])
    args = parser.parse_args()
    apply_preset(args, defaults)
    require_resolution_edge_mode(args.exit_mode)
    if args.anchor_shift_days < 0:
        raise SystemExit("anchor shift must be nonnegative")

    # Reuse the existing singular preset fields from the other alpha scripts.
    if hasattr(args, "stand_down_window"):
        args.stand_down_windows = str(args.stand_down_window)
    if hasattr(args, "stand_down_margin_pp"):
        args.stand_down_margin_pps = str(args.stand_down_margin_pp)
    if hasattr(args, "stand_down_cooldown_candles"):
        args.stand_down_cooldowns = str(args.stand_down_cooldown_candles)

    days_list = sorted(csv_ints(args.days_list))
    fold_shifts = csv_ints(args.fold_shifts)
    stress_fold_shifts = csv_ints(args.stress_fold_shifts)
    entry_thresholds = csv_floats(args.entry_thresholds)
    sides = csv_sides(args.sides)
    adaptive_values = csv_bools(args.adaptive_stand_down)
    stand_down_windows = csv_ints(args.stand_down_windows)
    stand_down_margin_pps = csv_floats(args.stand_down_margin_pps)
    stand_down_cooldowns = csv_ints(args.stand_down_cooldowns)
    regime_allowlist = csv_regimes(args.regime_allowlist)
    session_allowlist = csv_sessions(args.session_allowlist)
    blocked_hours_utc = csv_hours(args.blocked_hours_utc)
    regime_side_modes = csv_regime_side_modes(args.regime_side_modes)

    if not days_list:
        raise SystemExit("no primary windows provided")
    if not fold_shifts:
        raise SystemExit("no fold shifts provided")
    if args.stress_days > 0 and not stress_fold_shifts:
        raise SystemExit("stress window requested but no stress fold shifts provided")

    candidates = iter_candidates(
        entry_thresholds,
        sides,
        adaptive_values,
        stand_down_windows,
        stand_down_margin_pps,
        stand_down_cooldowns,
    )
    if not candidates:
        raise SystemExit("no candidates generated")

    gates = Gates(
        min_primary_mean_pnl=args.min_primary_mean_pnl,
        min_primary_positive_fold_ratio=args.min_primary_positive_fold_ratio,
        min_primary_mean_edge_gap_pp=args.min_primary_mean_edge_gap_pp,
        min_oldest_primary_positive_fold_ratio=args.min_oldest_primary_positive_fold_ratio,
        min_stress_mean_pnl=args.min_stress_mean_pnl,
        min_stress_positive_fold_ratio=args.min_stress_positive_fold_ratio,
        max_primary_decay_pnl=args.max_primary_decay_pnl,
        max_stress_decay_pnl=args.max_stress_decay_pnl,
    )

    api_url = args.base_url.rstrip("/") + "/api/backtest"
    now_ms = int(time.time() * 1000) - args.anchor_shift_days * DAY_MS
    base_payload = {
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

    ranked: list[tuple[CandidateScore, str, dict[str, bool]]] = []
    for candidate in candidates:
        primary_windows = [
            run_window(
                api_url,
                now_ms,
                base_payload,
                candidate,
                days,
                fold_shifts,
                timeout=args.request_timeout_secs,
                api_key=args.api_key,
                api_secret=args.api_secret,
                request_interval_secs=args.request_interval_secs,
            )
            for days in days_list
        ]
        stress_window = None
        if args.stress_days > 0:
            stress_window = run_window(
                api_url,
                now_ms,
                base_payload,
                candidate,
                args.stress_days,
                stress_fold_shifts,
                timeout=args.request_timeout_secs,
                api_key=args.api_key,
                api_secret=args.api_secret,
                request_interval_secs=args.request_interval_secs,
            )
        score = CandidateScore(
            candidate=candidate,
            primary_windows=primary_windows,
            stress_window=stress_window,
        )
        status, checks = evaluate_gates(score, gates)
        ranked.append((score, status, checks))

    ranked.sort(key=rank_key, reverse=True)
    best_score, best_status, best_checks = ranked[0]
    top_payload = []
    for score, status, checks in ranked[: max(1, args.top)]:
        row = candidate_to_json(score)
        row["status"] = status
        row["checks"] = checks
        top_payload.append(row)

    report = {
        "methodology": "historical candidate selection; overlapping windows are not independent; no untouched final holdout",
        "status": best_status,
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "api_base": args.base_url.rstrip("/"),
        "preset": args.preset,
        "mode": args.mode,
        "exit_mode": args.exit_mode,
        "time_of_day_weight": args.time_of_day_weight,
        "take_profit_pct": args.take_profit_pct,
        "stop_loss_pct": args.stop_loss_pct,
        "anchor_shift_days": args.anchor_shift_days,
        "days_list": days_list,
        "stress_days": args.stress_days,
        "fold_shifts": fold_shifts,
        "stress_fold_shifts": stress_fold_shifts,
        "require_regime_alignment": args.require_regime_alignment,
        "regime_allowlist": regime_allowlist,
        "session_allowlist": session_allowlist,
        "blocked_hours_utc": blocked_hours_utc,
        "regime_side_modes": regime_side_modes,
        "gates": asdict(gates),
        "checks": best_checks,
        "best_candidate": candidate_to_json(best_score),
        "top_candidates": top_payload,
    }
    out_dir = Path(args.output_dir)
    json_path, md_path = write_outputs(report, out_dir)
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}

    print(json.dumps(report, indent=2))
    return 0 if best_status == "pass" else 2


if __name__ == "__main__":
    raise SystemExit(main())
