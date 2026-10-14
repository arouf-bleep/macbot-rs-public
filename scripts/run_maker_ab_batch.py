#!/usr/bin/env python3
"""Run repeated maker A/B cycles and summarize the candidate across windows."""

from __future__ import annotations

import argparse
import json
import math
import hashlib
import subprocess
import sys
import time
from collections import Counter
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any

from run_maker_soak import config_fingerprint


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run a batch of maker A/B cycles")
    parser.add_argument("--config", default="config.json", help="Base config JSON path")
    parser.add_argument("--candidate-config", help="Exact frozen candidate config to A/B test")
    parser.add_argument("--expected-candidate-fingerprint", help="Configuration identity captured by the selecting gate")
    parser.add_argument(
        "--outdir",
        default="artifacts/maker-ab-batches",
        help="Directory for batch summary artifacts",
    )
    parser.add_argument(
        "--cycles",
        type=int,
        default=3,
        help="Default number of A/B cycles to run when no adaptive stop is configured",
    )
    parser.add_argument(
        "--existing-cycle-json",
        nargs="*",
        default=None,
        help="Optional existing maker_ab_cycle JSON artifacts to summarize instead of running new cycles",
    )
    parser.add_argument(
        "--sleep-seconds",
        type=float,
        default=2.0,
        help="Pause between cycles",
    )
    parser.add_argument(
        "--target-usable-cycles",
        type=int,
        default=None,
        help="Stop once this many usable cycles are collected",
    )
    parser.add_argument(
        "--max-total-cycles",
        type=int,
        default=None,
        help="Hard cap on total cycle attempts when targeting usable cycles",
    )
    parser.add_argument(
        "--max-sparse-streak",
        type=int,
        default=None,
        help="Stop early after this many sparse cycles in a row",
    )
    parser.add_argument(
        "--min-leg-events",
        type=int,
        default=50,
        help="Minimum baseline and candidate event count for a cycle to count as usable batch evidence",
    )
    parser.add_argument(
        "--duration-minutes",
        type=float,
        default=20.0,
        help="Minutes per leg (baseline and candidate)",
    )
    parser.add_argument(
        "--report-hours",
        type=float,
        default=1.0,
        help="Lookback window passed through to maker_soak_report",
    )
    parser.add_argument(
        "--enable-macro-guard",
        action="store_true",
        help="Enable the macro-event guard for both legs",
    )
    parser.add_argument(
        "--session-allowlist",
        default="ASIA,US",
        help="Comma-separated runtime session allowlist",
    )
    parser.add_argument(
        "--blocked-hours-utc",
        default="1,3,4,6,16,17,18,19,20",
        help="Comma-separated blocked UTC entry hours",
    )
    parser.add_argument(
        "--min-confidence-margin",
        type=float,
        default=0.03,
        help="Minimum runtime confidence margin above the mode floor",
    )
    parser.add_argument(
        "--candidate-mode",
        choices=["guard", "widen", "roll_guard"],
        default="widen",
        help="Candidate leg behavior: backwardation guard, spread widening, or side-aware Roll toxicity suppression",
    )
    parser.add_argument(
        "--maker-backwardation-guard-pct",
        type=float,
        default=-0.02,
        help="Backwardation threshold for the candidate leg",
    )
    parser.add_argument(
        "--maker-backwardation-widen-bps",
        type=float,
        default=40.0,
        help="Additional spread widening in bps for the candidate leg when running in widen mode",
    )
    parser.add_argument(
        "--maker-backwardation-min-target-spread-bps",
        type=float,
        default=0.0,
        help="Minimum target spread in bps enforced when backwardation widening is active",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-confidence",
        type=float,
        default=0.15,
        help="Minimum Roll confidence when candidate_mode=roll_guard",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-signal",
        type=float,
        default=0.05,
        help="Minimum absolute Roll signal when candidate_mode=roll_guard",
    )
    parser.add_argument(
        "--maker-roll-toxic-min-sample-count",
        type=int,
        default=6,
        help="Minimum Roll sample count when candidate_mode=roll_guard",
    )
    parser.add_argument(
        "--cargo-cmd",
        default="cargo run",
        help="Command used to start the bot",
    )
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:8000",
        help="Local API base URL expected from the bot",
    )
    parser.add_argument(
        "--wait-until-eligible-seconds",
        type=float,
        default=0.0,
        help="If the current UTC hour is blocked or outside the session allowlist, wait up to this many seconds for the next eligible start",
    )
    return parser.parse_args()


def run_json(cmd: list[str], cwd: Path) -> dict[str, Any]:
    proc = subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True)
    lines = [line.strip() for line in proc.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"command produced no JSON: {' '.join(cmd)}")
    return json.loads(lines[-1])


def load_json(path: str | Path) -> dict[str, Any]:
    path = Path(path)
    return json.loads(path.read_text(encoding="utf-8"))


def parse_session_allowlist(raw: str) -> set[str]:
    return {item.strip().lower() for item in raw.split(",") if item.strip()}


def parse_blocked_hours(raw: str) -> set[int]:
    blocked: set[int] = set()
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        try:
            hour = int(item)
        except ValueError as exc:
            raise ValueError("blocked hours must be integers from 0 to 23") from exc
        if not 0 <= hour <= 23:
            raise ValueError("blocked hours must be integers from 0 to 23")
        blocked.add(hour)
    return blocked


def session_bucket(hour_utc: int) -> str:
    if 0 <= hour_utc <= 7:
        return "asia"
    if 8 <= hour_utc <= 15:
        return "europe"
    return "us"


def start_window_status(args: argparse.Namespace, now: datetime | None = None) -> dict[str, Any]:
    now = now or datetime.now(timezone.utc)
    hour = now.hour
    session = session_bucket(hour)
    blocked_hours = parse_blocked_hours(args.blocked_hours_utc)
    allowed_sessions = parse_session_allowlist(args.session_allowlist)
    hour_allowed = hour not in blocked_hours
    session_allowed = not allowed_sessions or session in allowed_sessions
    return {
        "now_utc": now.isoformat(),
        "hour_utc": hour,
        "session": session,
        "hour_allowed": hour_allowed,
        "session_allowed": session_allowed,
        "eligible": hour_allowed and session_allowed,
    }


def next_eligible_delay_seconds(args: argparse.Namespace, now: datetime | None = None) -> float | None:
    now = now or datetime.now(timezone.utc)
    anchor = now.replace(minute=0, second=0, microsecond=0)
    for step in range(1, 49):
        candidate = anchor + timedelta(hours=step)
        if start_window_status(args, candidate)["eligible"]:
            return max(0.0, (candidate - now).total_seconds())
    return None


def mean_or_none(values: list[float | int | None]) -> float | None:
    vals = [float(v) for v in values if isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)]
    if not vals:
        return None
    return sum(vals) / len(vals)


def median_or_none(values: list[float | int | None]) -> float | None:
    vals = sorted(float(v) for v in values if isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v))
    if not vals:
        return None
    mid = len(vals) // 2
    if len(vals) % 2 == 1:
        return vals[mid]
    return (vals[mid - 1] + vals[mid]) / 2.0


def summarize_batch(cycles: list[dict[str, Any]], args: argparse.Namespace) -> dict[str, Any]:
    paths = [Path(item["compare"]["json"]).resolve() for item in cycles]
    hashes = [hashlib.sha256(path.read_bytes()).hexdigest() for path in paths]
    if len(set(paths)) != len(paths) or len(set(hashes)) != len(hashes):
        raise ValueError("repeated comparison evidence cannot count as independent cycles")
    compare_reports = [load_json(path) for path in paths]
    candidate_fingerprints = {item.get("candidate_config_fingerprint") for item in compare_reports}
    baseline_fingerprints = {item.get("baseline_config_fingerprint") for item in compare_reports}
    configuration_bound = (bool(compare_reports) and all(item.get("configuration_bound") is True for item in compare_reports)
                           and all(item.get("treatment_mode") == args.candidate_mode for item in compare_reports)
                           and len(candidate_fingerprints) == len(baseline_fingerprints) == 1
                           and None not in candidate_fingerprints and None not in baseline_fingerprints)
    expected_fingerprint = getattr(args, "expected_candidate_fingerprint", None)
    if expected_fingerprint and candidate_fingerprints != {expected_fingerprint}:
        configuration_bound = False
    usable_pairs: list[tuple[dict[str, Any], dict[str, Any]]] = []
    sparse_cycles = 0
    for cycle, compare in zip(cycles, compare_reports):
        if (
            cycle["baseline"]["events"] >= args.min_leg_events
            and cycle["candidate"]["events"] >= args.min_leg_events
        ):
            usable_pairs.append((cycle, compare))
        else:
            sparse_cycles += 1

    usable_cycles = [cycle for cycle, _ in usable_pairs]
    usable_reports = [compare for _, compare in usable_pairs]
    comparable = [r for r in usable_reports if r.get("comparable_window")]
    adequate = [r for r in usable_reports if r.get("sample_adequate")]
    adequate_comparable = [r for r in comparable if r.get("sample_adequate")]
    starved = [r for r in compare_reports if r.get("coverage_starved")]
    def engagement_count(report: dict[str, Any]) -> float:
        metrics = report.get("metrics") or {}
        if args.candidate_mode == "widen":
            return float(metrics.get("basis_widen_count_at_decision", {}).get("candidate") or 0)
        if args.candidate_mode == "roll_guard":
            return float(metrics.get("roll_guard_skip_count_at_decision", {}).get("candidate") or 0)
        return float(metrics.get("guard_skips", {}).get("candidate") or 0)

    engaged = [r for r in usable_reports if engagement_count(r) > 0]
    engaged_comparable = [r for r in comparable if r in engaged]
    engaged_adequate_comparable = [r for r in adequate_comparable if r in engaged]

    adverse_deltas = [r["metrics"]["avg_adverse_move_bps"]["delta"] for r in adequate_comparable]
    p90_deltas = [r["metrics"]["p90_adverse_move_bps"]["delta"] for r in comparable]
    places_deltas = [r["metrics"]["places"]["delta_pct"] for r in comparable]
    engagement_counts = [engagement_count(r) for r in compare_reports]

    improved = sum(1 for d in adverse_deltas if d is not None and d < 0)
    regressed = sum(1 for d in adverse_deltas if d is not None and d > 0)
    engaged_adverse_deltas = [
        r["metrics"]["avg_adverse_move_bps"]["delta"] for r in engaged_adequate_comparable
    ]
    engaged_improved = sum(1 for d in engaged_adverse_deltas if d is not None and d < 0)
    engaged_regressed = sum(1 for d in engaged_adverse_deltas if d is not None and d > 0)

    verdict = "insufficient_batch"
    if starved and len(starved) == len(compare_reports):
        verdict = "starved_candidate"
    elif usable_cycles and not adequate_comparable:
        verdict = "inadequate_sample_batch"
    elif args.candidate_mode in {"widen", "roll_guard"}:
        if len(engaged_adequate_comparable) == 0:
            verdict = f"{args.candidate_mode}_inactive"
        elif len(engaged_adequate_comparable) >= 2:
            if engaged_improved >= 2 and engaged_regressed == 0:
                verdict = "promising_repeatable_candidate"
            elif engaged_regressed >= 2 and engaged_improved == 0:
                verdict = "repeatable_regression_when_engaged"
            else:
                verdict = "mixed_when_engaged"
        else:
            if engaged_regressed >= 1 and engaged_improved == 0:
                verdict = "directional_regression_when_engaged"
            elif engaged_improved >= 1 and engaged_regressed == 0:
                verdict = "directional_improvement_when_engaged"
            else:
                verdict = "directional_but_thin"
    elif len(adequate_comparable) >= 3:
        if improved >= 2 and regressed == 0:
            verdict = "promising_repeatable_candidate"
        elif regressed >= 2 and improved == 0:
            verdict = "repeatable_regression"
        else:
            verdict = "mixed_repeatability"
    elif len(adequate_comparable) >= 1:
        verdict = "directional_but_thin"
    if cycles and not configuration_bound:
        verdict = "configuration_unverified"

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "cycles_run": len(cycles),
        "verdict": verdict,
        "configuration_bound": configuration_bound,
        "treatment_mode": args.candidate_mode if configuration_bound else None,
        "candidate_config_fingerprint": next(iter(candidate_fingerprints)) if configuration_bound else None,
        "baseline_config_fingerprint": next(iter(baseline_fingerprints)) if configuration_bound else None,
        "args": {
            "cycles": args.cycles,
            "target_usable_cycles": args.target_usable_cycles,
            "max_total_cycles": args.max_total_cycles,
            "max_sparse_streak": args.max_sparse_streak,
            "min_leg_events": args.min_leg_events,
            "duration_minutes": args.duration_minutes,
            "report_hours": args.report_hours,
            "enable_macro_guard": args.enable_macro_guard,
            "session_allowlist": args.session_allowlist,
            "blocked_hours_utc": args.blocked_hours_utc,
            "min_confidence_margin": args.min_confidence_margin,
            "candidate_mode": args.candidate_mode,
            "maker_backwardation_guard_pct": args.maker_backwardation_guard_pct,
            "maker_backwardation_widen_bps": args.maker_backwardation_widen_bps,
            "maker_backwardation_min_target_spread_bps": args.maker_backwardation_min_target_spread_bps,
            "maker_roll_toxic_min_confidence": args.maker_roll_toxic_min_confidence,
            "maker_roll_toxic_min_signal": args.maker_roll_toxic_min_signal,
            "maker_roll_toxic_min_sample_count": args.maker_roll_toxic_min_sample_count,
        },
        "counts": {
            "usable_cycles": len(usable_cycles),
            "sparse_cycles": sparse_cycles,
            "comparable_windows": len(comparable),
            "sample_adequate": len(adequate),
            "adequate_comparable_windows": len(adequate_comparable),
            "coverage_starved": len(starved),
            "improved_adverse_move": improved,
            "regressed_adverse_move": regressed,
            "engaged_windows": len(engaged),
            "engaged_comparable_windows": len(engaged_comparable),
            "engaged_adequate_comparable_windows": len(engaged_adequate_comparable),
            "engaged_improved_adverse_move": engaged_improved,
            "engaged_regressed_adverse_move": engaged_regressed,
            "verdicts": dict(Counter(r["verdict"] for r in compare_reports)),
        },
        "aggregates": {
            "avg_adverse_move_delta_bps": mean_or_none(adverse_deltas),
            "median_adverse_move_delta_bps": median_or_none(adverse_deltas),
            "avg_p90_adverse_delta_bps": mean_or_none(p90_deltas),
            "avg_places_delta_pct": mean_or_none(places_deltas),
            "avg_candidate_engagement_count": mean_or_none(engagement_counts),
            "avg_widen_count": mean_or_none(engagement_counts) if args.candidate_mode == "widen" else None,
            "avg_engaged_adverse_move_delta_bps": mean_or_none(engaged_adverse_deltas),
        },
        "cycles": cycles,
        "cycle_paths": [item["cycle_path"] for item in cycles],
        "compare_paths": [item["compare"]["json"] for item in cycles],
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Maker A/B Batch",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- Verdict: `{report['verdict']}`",
        f"- Cycles run: `{report['cycles_run']}`",
        f"- Stop reason: `{(report.get('run_control') or {}).get('stop_reason')}`",
        f"- Usable cycles: `{report['counts']['usable_cycles']}`",
        f"- Sparse cycles: `{report['counts']['sparse_cycles']}`",
        f"- Comparable windows: `{report['counts']['comparable_windows']}`",
        f"- Engaged windows: `{report['counts']['engaged_windows']}`",
        f"- Engaged comparable windows: `{report['counts']['engaged_comparable_windows']}`",
        f"- Sample adequate windows: `{report['counts']['sample_adequate']}`",
        f"- Adequate comparable windows: `{report['counts']['adequate_comparable_windows']}`",
        f"- Coverage-starved windows: `{report['counts']['coverage_starved']}`",
        f"- Improved adverse-move windows: `{report['counts']['improved_adverse_move']}`",
        f"- Regressed adverse-move windows: `{report['counts']['regressed_adverse_move']}`",
        f"- Engaged adequate comparable windows: `{report['counts']['engaged_adequate_comparable_windows']}`",
        f"- Engaged improved adverse-move windows: `{report['counts']['engaged_improved_adverse_move']}`",
        f"- Engaged regressed adverse-move windows: `{report['counts']['engaged_regressed_adverse_move']}`",
        f"- Avg adverse delta (bps): `{report['aggregates']['avg_adverse_move_delta_bps']}`",
        f"- Avg engaged adverse delta (bps): `{report['aggregates']['avg_engaged_adverse_move_delta_bps']}`",
        f"- Median adverse delta (bps): `{report['aggregates']['median_adverse_move_delta_bps']}`",
        f"- Avg p90 adverse delta (bps): `{report['aggregates']['avg_p90_adverse_delta_bps']}`",
        f"- Avg places delta (%): `{report['aggregates']['avg_places_delta_pct']}`",
        f"- Avg candidate engagement count: `{report['aggregates'].get('avg_candidate_engagement_count')}`",
        f"- Avg candidate widen count: `{report['aggregates']['avg_widen_count']}`",
    ]
    start_window = report.get("start_window")
    if start_window:
        lines.extend(
            [
                f"- Start hour UTC: `{start_window['hour_utc']}`",
                f"- Start session: `{start_window['session']}`",
                f"- Start eligible: `{start_window['eligible']}`",
            ]
        )
    lines.extend(["", "## Cycle Verdicts", ""])
    for verdict, count in sorted(report["counts"]["verdicts"].items()):
        lines.append(f"- `{verdict}`: `{count}`")

    lines.extend(["", "## Cycles", ""])
    for idx, cycle in enumerate(report["cycles"], start=1):
        lines.append(
            f"- Cycle `{idx}`: verdict=`{cycle['compare']['verdict']}` "
            f"baseline_events=`{cycle['baseline']['events']}` "
            f"candidate_events=`{cycle['candidate']['events']}` "
            f"compare=`{cycle['compare']['json']}` "
            f"cycle=`{cycle['cycle_path']}`"
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    if args.cycles <= 0 or args.min_leg_events <= 0:
        raise ValueError("cycles and minimum leg events must be positive")
    root = Path(__file__).resolve().parent.parent
    if args.candidate_config:
        candidate = load_json((root / args.candidate_config).resolve())
        fingerprint = config_fingerprint(candidate)
        if args.expected_candidate_fingerprint and fingerprint != args.expected_candidate_fingerprint:
            raise ValueError("selected candidate configuration changed before the A/B batch")
        args.expected_candidate_fingerprint = fingerprint
        args.session_allowlist = ",".join(candidate.get("session_allowlist") or [])
        args.blocked_hours_utc = ",".join(str(hour) for hour in candidate.get("blocked_hours_utc") or [])
        args.min_confidence_margin = float(candidate.get("min_confidence_margin") or 0.0)
        args.enable_macro_guard = bool(candidate.get("macro_event_guard_enabled"))
        args.maker_backwardation_widen_bps = float(candidate.get("maker_backwardation_widen_bps") or 0.0)
        args.maker_backwardation_min_target_spread_bps = float(candidate.get("maker_backwardation_min_target_spread_bps") or 0.0)
    outdir = (root / args.outdir).resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    common = [
        sys.executable,
        "scripts/run_maker_ab_cycle.py",
        "--config",
        args.config,
        "--duration-minutes",
        str(args.duration_minutes),
        "--report-hours",
        str(args.report_hours),
        "--session-allowlist",
        args.session_allowlist,
        "--blocked-hours-utc",
        args.blocked_hours_utc,
        "--min-confidence-margin",
        str(args.min_confidence_margin),
        "--candidate-mode",
        args.candidate_mode,
        "--maker-backwardation-guard-pct",
        str(args.maker_backwardation_guard_pct),
        "--maker-backwardation-widen-bps",
        str(args.maker_backwardation_widen_bps),
        "--maker-backwardation-min-target-spread-bps",
        str(args.maker_backwardation_min_target_spread_bps),
        "--maker-roll-toxic-min-confidence",
        str(args.maker_roll_toxic_min_confidence),
        "--maker-roll-toxic-min-signal",
        str(args.maker_roll_toxic_min_signal),
        "--maker-roll-toxic-min-sample-count",
        str(args.maker_roll_toxic_min_sample_count),
        "--cargo-cmd",
        args.cargo_cmd,
        "--base-url",
        args.base_url,
    ]
    if args.enable_macro_guard:
        common.append("--enable-macro-guard")
    if args.candidate_config:
        common.extend(["--candidate-config", args.candidate_config, "--expected-candidate-fingerprint", args.expected_candidate_fingerprint])

    cycles: list[dict[str, Any]] = []
    failure: dict[str, Any] | None = None
    start_window = start_window_status(args) if not args.existing_cycle_json else None
    if args.existing_cycle_json:
        for raw_path in args.existing_cycle_json:
            path = Path(raw_path).expanduser().resolve()
            cycle = load_json(path)
            cycle["cycle_path"] = str(path)
            cycles.append(cycle)
    else:
        if start_window and not start_window["eligible"]:
            wait_limit = max(0.0, args.wait_until_eligible_seconds)
            delay = next_eligible_delay_seconds(args)
            if delay is None or delay > wait_limit:
                report = {
                    "generated_at": datetime.now(timezone.utc).isoformat(),
                    "cycles_run": 0,
                    "verdict": "awaiting_eligible_start",
                    "args": {
                        "cycles": args.cycles,
                        "target_usable_cycles": args.target_usable_cycles,
                        "max_total_cycles": args.max_total_cycles,
                        "max_sparse_streak": args.max_sparse_streak,
                        "min_leg_events": args.min_leg_events,
                        "duration_minutes": args.duration_minutes,
                        "report_hours": args.report_hours,
                        "enable_macro_guard": args.enable_macro_guard,
                        "session_allowlist": args.session_allowlist,
                        "blocked_hours_utc": args.blocked_hours_utc,
                        "min_confidence_margin": args.min_confidence_margin,
                        "candidate_mode": args.candidate_mode,
                        "maker_backwardation_guard_pct": args.maker_backwardation_guard_pct,
                        "maker_backwardation_widen_bps": args.maker_backwardation_widen_bps,
                        "maker_backwardation_min_target_spread_bps": args.maker_backwardation_min_target_spread_bps,
                        "maker_roll_toxic_min_confidence": args.maker_roll_toxic_min_confidence,
                        "maker_roll_toxic_min_signal": args.maker_roll_toxic_min_signal,
                        "maker_roll_toxic_min_sample_count": args.maker_roll_toxic_min_sample_count,
                    },
                    "counts": {
                        "usable_cycles": 0,
                        "sparse_cycles": 0,
                        "comparable_windows": 0,
                        "sample_adequate": 0,
                        "adequate_comparable_windows": 0,
                        "engaged_adequate_comparable_windows": 0,
                        "coverage_starved": 0,
                        "improved_adverse_move": 0,
                        "regressed_adverse_move": 0,
                        "engaged_windows": 0,
                        "engaged_comparable_windows": 0,
                        "engaged_improved_adverse_move": 0,
                        "engaged_regressed_adverse_move": 0,
                        "verdicts": {},
                    },
                    "aggregates": {
                        "avg_adverse_move_delta_bps": None,
                        "median_adverse_move_delta_bps": None,
                        "avg_p90_adverse_delta_bps": None,
                        "avg_places_delta_pct": None,
                        "avg_candidate_engagement_count": None,
                        "avg_widen_count": None,
                        "avg_engaged_adverse_move_delta_bps": None,
                    },
                    "cycles": [],
                    "cycle_paths": [],
                    "compare_paths": [],
                    "failure": None,
                    "run_control": {
                        "target_usable_cycles": args.target_usable_cycles or args.cycles,
                        "max_total_cycles": args.max_total_cycles or max(args.cycles, args.target_usable_cycles or args.cycles),
                        "max_sparse_streak": args.max_sparse_streak,
                        "stop_reason": "start_window_ineligible",
                        "next_eligible_delay_seconds": delay,
                    },
                    "start_window": start_window,
                }
                stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
                json_path = outdir / f"{stamp}_maker_ab_batch.json"
                md_path = outdir / f"{stamp}_maker_ab_batch.md"
                json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
                md_path.write_text(render_markdown(report), encoding="utf-8")
                print(
                    json.dumps(
                        {
                            "json": str(json_path),
                            "md": str(md_path),
                            "verdict": report["verdict"],
                            "cycles_run": 0,
                        }
                    )
                )
                return 0
            time.sleep(delay)
            start_window = start_window_status(args)

        usable_cycles = 0
        sparse_streak = 0
        target_usable = args.target_usable_cycles or args.cycles
        max_total = args.max_total_cycles or max(args.cycles, target_usable)
        stop_reason: str | None = None
        for idx in range(max_total):
            try:
                result = run_json(common, root)
                cycle = load_json(result["cycle"])
                cycle["cycle_path"] = result["cycle"]
                cycles.append(cycle)
                is_usable = (
                    cycle["baseline"]["events"] >= args.min_leg_events
                    and cycle["candidate"]["events"] >= args.min_leg_events
                )
                if is_usable:
                    usable_cycles += 1
                    sparse_streak = 0
                else:
                    sparse_streak += 1
                if usable_cycles >= target_usable:
                    stop_reason = f"target_usable_cycles_reached({usable_cycles})"
                    break
                if args.max_sparse_streak is not None and sparse_streak >= args.max_sparse_streak:
                    stop_reason = f"max_sparse_streak_reached({sparse_streak})"
                    break
            except subprocess.CalledProcessError as exc:
                failure = {
                    "cycle_index": idx + 1,
                    "returncode": exc.returncode,
                    "cmd": exc.cmd,
                    "stdout": exc.stdout,
                    "stderr": exc.stderr,
                }
                break
            if idx < max_total - 1 and args.sleep_seconds > 0:
                time.sleep(args.sleep_seconds)
        args._derived_stop_reason = stop_reason
        args._derived_target_usable = target_usable
        args._derived_max_total = max_total

    report = summarize_batch(cycles, args)
    report["failure"] = failure
    if failure is not None:
        report["verdict"] = "batch_failed"
    report["run_control"] = {
        "target_usable_cycles": getattr(args, "_derived_target_usable", args.target_usable_cycles or args.cycles),
        "max_total_cycles": getattr(args, "_derived_max_total", args.max_total_cycles or args.cycles),
        "max_sparse_streak": args.max_sparse_streak,
        "stop_reason": getattr(args, "_derived_stop_reason", None),
    }
    report["start_window"] = start_window
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_maker_ab_batch.json"
    md_path = outdir / f"{stamp}_maker_ab_batch.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    md_path.write_text(render_markdown(report), encoding="utf-8")

    print(
        json.dumps(
            {
                "json": str(json_path),
                "md": str(md_path),
                "verdict": report["verdict"],
                "cycles_run": report["cycles_run"],
            }
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
