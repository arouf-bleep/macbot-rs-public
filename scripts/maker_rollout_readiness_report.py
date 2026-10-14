#!/usr/bin/env python3
"""Summarize whether the frozen maker paper-rollout candidate is aligned with current evidence."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from run_maker_soak import config_fingerprint


PROMOTABLE_VERDICTS = {
    "promising_guard_candidate",
    "promising_repeatable_candidate",
}

FRONT_RUNNER_ELIGIBLE_VERDICTS = {
    "mixed_when_engaged",
    "directional_improvement_when_engaged",
    "promising_but_noncomparable",
    "promising_guard_candidate",
    "promising_repeatable_candidate",
}

MEANINGFUL_BATCH_VERDICTS = FRONT_RUNNER_ELIGIBLE_VERDICTS | {
    "directional_regression_when_engaged",
    "guard_starved_candidate",
    "guard_regression",
    "repeatable_regression",
    "repeatable_regression_when_engaged",
    "starved_candidate",
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build a maker rollout readiness report")
    parser.add_argument(
        "--baseline-config",
        default="ops/profiles/maker_paper_baseline.json",
        help="Frozen baseline config path",
    )
    parser.add_argument(
        "--candidate-config",
        default="ops/profiles/maker_paper_widen20.json",
        help="Frozen candidate config path",
    )
    parser.add_argument(
        "--rollout-state",
        default="ops/profiles/maker_rollout_state.json",
        help="Rollout state JSON path describing the active candidate, if any",
    )
    parser.add_argument(
        "--batch-dir",
        default="artifacts/maker-ab-batches",
        help="Directory containing maker A/B batch artifacts",
    )
    parser.add_argument(
        "--outdir",
        default="artifacts/readiness-reports",
        help="Directory for readiness JSON/Markdown artifacts",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(path.read_text(encoding="utf-8"))


def load_optional_json(path: Path) -> dict[str, Any] | None:
    if not path.exists():
        return None
    return load_json(path)


def widen_candidate_label(widen_bps: float, floor_bps: float) -> str:
    label = f"widen_{int(round(widen_bps))}bps"
    if floor_bps > 0.0:
        label += f"_floor{int(round(floor_bps))}bps"
    return label


def roll_guard_candidate_label(confidence: float, signal: float, samples: int) -> str:
    return f"roll_guard_c{int(round(confidence * 100))}_s{int(round(signal * 100))}_n{samples}"


def profile_label(cfg: dict[str, Any]) -> str:
    widen = float(cfg.get("maker_backwardation_widen_bps") or 0.0)
    floor = float(cfg.get("maker_backwardation_min_target_spread_bps") or 0.0)
    if widen > 0:
        return widen_candidate_label(widen, floor)
    if bool(cfg.get("maker_roll_toxic_guard_enabled")):
        return roll_guard_candidate_label(
            float(cfg.get("maker_roll_toxic_min_confidence") or 0.15),
            float(cfg.get("maker_roll_toxic_min_signal") or 0.05),
            int(cfg.get("maker_roll_toxic_min_sample_count") or 6),
        )
    return "baseline"


def summarize_profile(path: Path, cfg: dict[str, Any]) -> dict[str, Any]:
    sessions = sorted({str(s).lower() for s in cfg.get("session_allowlist") or []})
    regimes = sorted({str(r).upper() for r in cfg.get("regime_allowlist") or []})
    blocked = sorted(int(h) for h in (cfg.get("blocked_hours_utc") or []))
    issues: list[str] = []
    if str(cfg.get("mode")).lower() != "simulation":
        issues.append("mode must stay simulation for the frozen paper profile")
    if str(cfg.get("execution_mode")).lower() != "maker":
        issues.append("execution_mode must be maker")
    if not bool(cfg.get("sim_live_execution")):
        issues.append("sim_live_execution must be true")
    if bool(cfg.get("shadow_trading")):
        issues.append("shadow_trading must be false")
    if not bool(cfg.get("maker_live_quoting")):
        issues.append("maker_live_quoting must be true")
    if not bool(cfg.get("macro_event_guard_enabled")):
        issues.append("macro_event_guard_enabled should stay enabled")
    if regimes != ["TREND_UP"]:
        issues.append("regime_allowlist should be frozen to TREND_UP")
    if sessions != ["asia", "us"]:
        issues.append("session_allowlist should be frozen to asia/us")
    if blocked != [1, 3, 4, 6, 16, 17, 18, 19, 20]:
        issues.append("blocked_hours_utc drifted from the current maker research rail")
    if abs(float(cfg.get("min_confidence_margin") or 0.0) - 0.03) > 1e-9:
        issues.append("min_confidence_margin should stay at 0.03")
    return {
        "path": str(path),
        "label": profile_label(cfg),
        "config_fingerprint": config_fingerprint(cfg),
        "maker_backwardation_widen_bps": float(cfg.get("maker_backwardation_widen_bps") or 0.0),
        "maker_backwardation_min_target_spread_bps": float(
            cfg.get("maker_backwardation_min_target_spread_bps") or 0.0
        ),
        "issues": issues,
        "session_allowlist": sessions,
        "regime_allowlist": regimes,
        "blocked_hours_utc": blocked,
        "macro_event_guard_enabled": bool(cfg.get("macro_event_guard_enabled")),
    }


def summarize_batch(path: Path, obj: dict[str, Any]) -> dict[str, Any]:
    counts = obj.get("counts") or {}
    aggs = obj.get("aggregates") or {}
    args = obj.get("args") or {}
    run_control = obj.get("run_control") or {}
    widen = float(args.get("maker_backwardation_widen_bps") or 0.0)
    floor = float(args.get("maker_backwardation_min_target_spread_bps") or 0.0)
    roll_conf = float(args.get("maker_roll_toxic_min_confidence") or 0.15)
    roll_signal = float(args.get("maker_roll_toxic_min_signal") or 0.05)
    roll_samples = int(args.get("maker_roll_toxic_min_sample_count") or 6)
    return {
        "path": str(path),
        "generated_at": obj.get("generated_at"),
        "label": widen_candidate_label(widen, floor)
        if (args.get("candidate_mode") == "widen" and widen > 0.0)
        else roll_guard_candidate_label(roll_conf, roll_signal, roll_samples)
        if args.get("candidate_mode") == "roll_guard"
        else str(args.get("candidate_mode") or "unknown"),
        "verdict": obj.get("verdict"),
        "configuration_bound": obj.get("configuration_bound") is True,
        "treatment_mode": obj.get("treatment_mode"),
        "candidate_config_fingerprint": obj.get("candidate_config_fingerprint"),
        "candidate_mode": args.get("candidate_mode"),
        "maker_backwardation_widen_bps": widen,
        "maker_backwardation_min_target_spread_bps": floor,
        "usable_cycles": counts.get("usable_cycles"),
        "engaged_comparable_windows": counts.get("engaged_comparable_windows"),
        "engaged_improved_adverse_move": counts.get("engaged_improved_adverse_move"),
        "engaged_regressed_adverse_move": counts.get("engaged_regressed_adverse_move"),
        "avg_adverse_move_delta_bps": aggs.get("avg_engaged_adverse_move_delta_bps", aggs.get("avg_adverse_move_delta_bps")),
        "avg_p90_adverse_delta_bps": aggs.get("avg_p90_adverse_delta_bps"),
        "avg_places_delta_pct": aggs.get("avg_places_delta_pct"),
        "stop_reason": run_control.get("stop_reason"),
        "next_eligible_delay_seconds": run_control.get("next_eligible_delay_seconds"),
    }


def front_runner_cost(item: dict[str, Any]) -> float:
    adverse = float(item["avg_adverse_move_delta_bps"]) if item.get("avg_adverse_move_delta_bps") is not None else 10000.0
    p90 = float(item.get("avg_p90_adverse_delta_bps") or 0.0)
    places_tax = abs(float(item["avg_places_delta_pct"])) if item.get("avg_places_delta_pct") is not None else 1000.0
    return adverse + (0.20 * places_tax) + (0.05 * p90)


def select_front_runner(
    items: list[dict[str, Any]], preferred_label: str | None = None,
    preferred_fingerprint: str | None = None,
) -> dict[str, Any] | None:
    candidates = [item for item in items if item.get("verdict") in FRONT_RUNNER_ELIGIBLE_VERDICTS]
    if not candidates:
        return None
    if preferred_label:
        preferred = [item for item in candidates if item.get("label") == preferred_label]
        if not preferred:
            return None
        candidates = preferred
    if preferred_fingerprint:
        candidates = [item for item in candidates if item.get("configuration_bound") is True
                      and item.get("candidate_config_fingerprint") == preferred_fingerprint
                      and item.get("treatment_mode") in {"widen", "guard", "roll_guard"}
                      and item.get("treatment_mode") == item.get("candidate_mode")]
        if not candidates:
            return None
    candidates.sort(
        key=lambda item: (
            front_runner_cost(item),
            -int(item.get("engaged_comparable_windows") or 0),
            -(
                int(item.get("engaged_improved_adverse_move") or 0)
                - int(item.get("engaged_regressed_adverse_move") or 0)
            ),
        )
    )
    return candidates[0]


def latest_batches_by_configuration(batch_dir: Path) -> list[dict[str, Any]]:
    latest: dict[tuple[str, str | None], dict[str, Any]] = {}
    for path in sorted(batch_dir.glob("*.json")):
        obj = load_json(path)
        if "verdict" not in obj:
            continue
        summary = summarize_batch(path, obj)
        identity = (summary["label"], summary.get("candidate_config_fingerprint"))
        previous = latest.get(identity)
        if previous is None or summary.get("generated_at", "") > previous.get("generated_at", ""):
            latest[identity] = summary
    return sorted(latest.values(), key=lambda item: item.get("generated_at", ""), reverse=True)


def render_markdown(report: dict[str, Any]) -> str:
    active_candidate = report.get("active_candidate_profile")
    lines = [
        "# Maker Rollout Readiness Report",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- Verdict: `{report['verdict']}`",
        f"- Front-runner: `{(report.get('front_runner') or {}).get('label')}`",
        f"- Active candidate profile: `{(active_candidate or {}).get('label')}`",
        "",
        "## Blockers",
        "",
    ]
    blockers = report.get("blockers") or []
    if blockers:
        for blocker in blockers:
            lines.append(f"- {blocker}")
    else:
        lines.append("- None")

    lines.extend(["", "## Candidate Evidence", ""])
    front = report.get("front_runner") or {}
    if front:
        lines.append(f"- Verdict: `{front.get('verdict')}`")
        lines.append(f"- Avg adverse delta: `{front.get('avg_adverse_move_delta_bps')}`")
        lines.append(f"- Avg p90 adverse delta: `{front.get('avg_p90_adverse_delta_bps')}`")
        lines.append(f"- Avg places delta: `{front.get('avg_places_delta_pct')}`")
        lines.append(f"- Engaged comparable windows: `{front.get('engaged_comparable_windows')}`")
    else:
        lines.append("- No front-runner batch available")

    lines.extend(["", "## Profiles", ""])
    for key in ("baseline_profile", "active_candidate_profile"):
        profile = report[key]
        if profile is None:
            lines.append(f"- `{key}` => `None`")
            continue
        lines.append(f"- `{key}` => `{profile['path']}` ({profile['label']})")
        if profile["issues"]:
            for issue in profile["issues"]:
                lines.append(f"  - issue: {issue}")
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    baseline_path = (root / args.baseline_config).resolve()
    candidate_path = (root / args.candidate_config).resolve()
    rollout_state_path = (root / args.rollout_state).resolve()
    batch_dir = (root / args.batch_dir).resolve()
    outdir = (root / args.outdir).resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    baseline_profile = summarize_profile(baseline_path, load_json(baseline_path))
    rollout_state = load_optional_json(rollout_state_path) or {}
    active_candidate_rel = rollout_state.get("active_candidate_profile")
    if active_candidate_rel:
        candidate_path = (root / str(active_candidate_rel)).resolve()
    active_candidate_profile = (
        summarize_profile(candidate_path, load_json(candidate_path))
        if active_candidate_rel
        else None
    )

    latest_batches = latest_batches_by_configuration(batch_dir)
    latest_meaningful_batches = sorted(
        [item for item in latest_batches if item.get("verdict") in MEANINGFUL_BATCH_VERDICTS],
        key=lambda item: item.get("generated_at", ""),
        reverse=True,
    )
    overall_front = select_front_runner(latest_meaningful_batches)
    front = (
        select_front_runner(latest_meaningful_batches, active_candidate_profile["label"], active_candidate_profile["config_fingerprint"])
        if active_candidate_profile is not None
        else None
    )

    blockers: list[str] = []
    blockers.extend(f"baseline profile: {issue}" for issue in baseline_profile["issues"])
    if active_candidate_profile is None:
        blockers.append(
            rollout_state.get("reason")
            or "No active maker rollout candidate is selected; baseline remains control-only."
        )
    else:
        blockers.extend(
            f"candidate profile: {issue}" for issue in active_candidate_profile["issues"]
        )
    if active_candidate_profile is not None and front is None:
        blockers.append(
            f"No eligible maker A/B evidence is available yet for the frozen candidate `{active_candidate_profile['label']}`."
        )
    if active_candidate_profile is not None and front is not None:
        if front.get("verdict") not in PROMOTABLE_VERDICTS:
            blockers.append(
                f"Frozen candidate verdict is still `{front.get('verdict')}`, so repeatable evidence is not strong enough to freeze a rollout config."
            )
        if float(front.get("avg_places_delta_pct") or 0.0) <= -25.0:
            blockers.append(
                f"Quote-coverage tax is still high at {front.get('avg_places_delta_pct'):.2f}%."
            )
        if int(front.get("engaged_comparable_windows") or 0) < 5:
            blockers.append(
                f"Only {front.get('engaged_comparable_windows') or 0} engaged comparable windows are available; need more repeated evidence."
            )

    if active_candidate_profile is None:
        verdict = "baseline_only_no_candidate"
    else:
        verdict = "paper_rollout_candidate_ready" if not blockers else "blocked_waiting_for_repeatable_evidence"
    report = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "verdict": verdict,
        "baseline_profile": baseline_profile,
        "active_candidate_profile": active_candidate_profile,
        "front_runner": front,
        "reference_front_runner": overall_front,
        "latest_batches": latest_batches,
        "latest_meaningful_batches": latest_meaningful_batches,
        "rollout_state": rollout_state or None,
        "blockers": blockers,
    }

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_maker_rollout_readiness_report.json"
    md_path = outdir / f"{stamp}_maker_rollout_readiness_report.md"
    latest_json_path = outdir / "latest_maker_rollout_readiness_report.json"
    latest_md_path = outdir / "latest_maker_rollout_readiness_report.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    md_path.write_text(render_markdown(report), encoding="utf-8")
    latest_json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    latest_md_path.write_text(render_markdown(report), encoding="utf-8")
    print(json.dumps({"json": str(json_path), "md": str(md_path), "verdict": verdict}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
