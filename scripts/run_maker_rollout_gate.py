#!/usr/bin/env python3
"""Autonomous maker paper-rollout gate runner.

This script keeps the current maker paper candidate moving without manual babysitting:

1. bind the selected frozen maker paper profile
2. regenerate the maker rollout readiness report
3. if still blocked, run another frozen-candidate maker A/B evidence batch
4. refresh readiness again
5. repeat until ready or the round budget is exhausted
"""

from __future__ import annotations

import argparse
import json
import math
import os
import subprocess
import sys
import time
import tempfile
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from run_maker_soak import config_fingerprint


def widen_candidate_label(widen_bps: float, floor_bps: float) -> str:
    label = f"widen_{int(round(widen_bps))}bps"
    if floor_bps > 0.0:
        label += f"_floor{int(round(floor_bps))}bps"
    return label


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run the maker rollout gate autonomously")
    parser.add_argument("--config", default="config.json", help="Base config JSON path")
    parser.add_argument(
        "--outdir",
        default="artifacts/readiness-reports",
        help="Directory for rollout gate artifacts",
    )
    parser.add_argument(
        "--rollout-state",
        default="ops/profiles/maker_rollout_state.json",
        help="Rollout state JSON path describing the active candidate, if any",
    )
    parser.add_argument(
        "--rounds",
        type=int,
        default=3,
        help="Maximum rollout-gate rounds to run",
    )
    parser.add_argument(
        "--batch-cycles",
        type=int,
        default=3,
        help="Fallback cycle count per evidence batch",
    )
    parser.add_argument(
        "--target-usable-cycles",
        type=int,
        default=3,
        help="Target usable cycles per evidence batch",
    )
    parser.add_argument(
        "--max-total-cycles",
        type=int,
        default=7,
        help="Hard cap on total cycle attempts per evidence batch",
    )
    parser.add_argument(
        "--max-sparse-streak",
        type=int,
        default=2,
        help="Stop an evidence batch after this many sparse cycles in a row",
    )
    parser.add_argument(
        "--min-leg-events",
        type=int,
        default=50,
        help="Minimum baseline/candidate event count for a usable cycle",
    )
    parser.add_argument(
        "--duration-minutes",
        type=float,
        default=0.5,
        help="Minutes per leg for each maker A/B cycle",
    )
    parser.add_argument(
        "--report-hours",
        type=float,
        default=1.0,
        help="Telemetry lookback window for each maker soak report",
    )
    parser.add_argument(
        "--wait-until-eligible-seconds",
        type=float,
        default=14400.0,
        help="Maximum time an evidence batch may wait for the next eligible start window",
    )
    parser.add_argument(
        "--maker-backwardation-widen-bps",
        type=float,
        default=20.0,
        help="Current paper candidate widening amount in bps",
    )
    parser.add_argument(
        "--maker-backwardation-min-target-spread-bps",
        type=float,
        default=0.0,
        help="Minimum target spread in bps enforced when backwardation widening is active",
    )
    parser.add_argument(
        "--candidate-label",
        default=None,
        help="Expected frozen candidate label; defaults to one derived from the widening and floor settings",
    )
    parser.add_argument(
        "--cargo-cmd",
        default="cargo run",
        help="Command used by the batch/soak runners to start the bot",
    )
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:8000",
        help="Local API base URL expected from the bot",
    )
    parser.add_argument(
        "--ready-soak-config",
        default="ops/profiles/maker_paper_widen20.json",
        help="Frozen maker paper profile to soak automatically once the gate turns ready",
    )
    parser.add_argument(
        "--ready-soak-db",
        default=None,
        help="Fresh SQLite DB path for the automatic frozen paper soak; defaults to an isolated artifact",
    )
    parser.add_argument(
        "--ready-soak-duration-minutes",
        type=float,
        default=30.0,
        help="Duration for the automatic frozen paper soak once the gate turns ready",
    )
    parser.add_argument(
        "--ready-soak-report-hours",
        type=float,
        default=1.0,
        help="Telemetry lookback window for the automatic frozen paper soak report",
    )
    return parser.parse_args()


def run_json(cmd: list[str], cwd: Path) -> dict[str, Any]:
    proc = subprocess.run(cmd, cwd=cwd, check=True, capture_output=True, text=True)
    raw = proc.stdout.strip()
    if not raw:
        raise RuntimeError(f"command produced no JSON: {' '.join(cmd)}")
    try:
        return json.loads(raw)
    except json.JSONDecodeError:
        lines = [line.strip() for line in raw.splitlines() if line.strip()]
        return json.loads(lines[-1])


def load_json(path: str | Path) -> dict[str, Any]:
    path = Path(path)
    return json.loads(path.read_text(encoding="utf-8"))


def load_optional_json(path: str | Path) -> dict[str, Any] | None:
    path = Path(path)
    if not path.exists():
        return None
    return load_json(path)


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Maker Rollout Gate",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- Verdict: `{report['verdict']}`",
        f"- Stop reason: `{report['stop_reason']}`",
        f"- Candidate label: `{report['candidate_label']}`",
        "",
        "## Rounds",
        "",
    ]
    for round_item in report["rounds"]:
        lines.append(
            f"- Round `{round_item['round_index']}`: before=`{round_item['readiness_before']['verdict']}` "
            f"batch=`{(round_item.get('batch') or {}).get('verdict')}` "
            f"after=`{(round_item.get('readiness_after') or {}).get('verdict')}`"
        )
    lines.extend(["", "## Final Blockers", ""])
    blockers = (report.get("final_readiness") or {}).get("blockers") or []
    if blockers:
        for blocker in blockers:
            lines.append(f"- {blocker}")
    else:
        lines.append("- None")
    return "\n".join(lines) + "\n"


def write_runtime_status(outdir: Path, status: dict[str, Any]) -> None:
    json_path = outdir / "latest_maker_rollout_gate_status.json"
    md_path = outdir / "latest_maker_rollout_gate_status.md"
    json_path.write_text(json.dumps(status, indent=2), encoding="utf-8")
    md_lines = [
        "# Maker Rollout Gate Runtime Status",
        "",
        f"- Updated: `{status.get('updated_at')}`",
        f"- Active: `{status.get('active')}`",
        f"- Phase: `{status.get('phase')}`",
        f"- Current round: `{status.get('current_round')}`",
        f"- Candidate: `{status.get('candidate_label')}`",
        f"- Last readiness verdict: `{status.get('last_readiness_verdict')}`",
        f"- Last batch verdict: `{status.get('last_batch_verdict')}`",
        f"- Next eligible delay seconds: `{status.get('next_eligible_delay_seconds')}`",
        f"- Stop reason: `{status.get('stop_reason')}`",
    ]
    blockers = status.get("last_blockers") or []
    md_lines.extend(["", "## Blockers", ""])
    if blockers:
        for blocker in blockers:
            md_lines.append(f"- {blocker}")
    else:
        md_lines.append("- None")
    md_path.write_text("\n".join(md_lines) + "\n", encoding="utf-8")


def run_ready_soak(args: argparse.Namespace, root: Path) -> dict[str, Any]:
    cmd = [
        sys.executable,
        "scripts/run_frozen_maker_soak.py",
        "--config",
        args.ready_soak_config,
        "--duration-minutes",
        str(args.ready_soak_duration_minutes),
        "--report-hours",
        str(args.ready_soak_report_hours),
        "--cargo-cmd",
        args.cargo_cmd,
        "--base-url",
        args.base_url,
    ]
    if args.ready_soak_db:
        cmd.extend(["--db", args.ready_soak_db])
    selected = getattr(args, "_selected_config", None)
    if selected is None:
        raise ValueError("frozen soak requires a configuration bound at selection")
    with tempfile.TemporaryDirectory(prefix="macbot_selected_rollout_") as directory:
        path = Path(directory) / "selected.json"
        with path.open("x", encoding="utf-8") as stream:
            os.chmod(path, 0o600)
            stream.write(json.dumps(selected, allow_nan=False))
        cmd[cmd.index("--config") + 1] = str(path)
        result = run_json(cmd, root)
    if result.get("config_fingerprint") != args._candidate_fingerprint:
        raise ValueError("frozen soak did not execute the configuration selected by this gate")
    return result


def validate_selected_readiness(args: argparse.Namespace, readiness: dict[str, Any]) -> None:
    expected = getattr(args, "_candidate_fingerprint", None)
    if expected is None:
        return
    profile = readiness.get("active_candidate_profile") or {}
    if profile.get("config_fingerprint") != expected:
        raise ValueError("selected profile changed while the rollout gate was running")
    if readiness.get("verdict") == "paper_rollout_candidate_ready":
        front = readiness.get("front_runner") or {}
        if (front.get("configuration_bound") is not True or front.get("candidate_config_fingerprint") != expected
                or front.get("treatment_mode") != "widen"):
            raise ValueError("readiness evidence is not bound to this selected configuration")


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent
    outdir = (root / args.outdir).resolve()
    rollout_state_path = (root / args.rollout_state).resolve()
    outdir.mkdir(parents=True, exist_ok=True)
    rollout_state = load_optional_json(rollout_state_path) or {}
    candidate_label = (
        args.candidate_label
        or rollout_state.get("active_candidate_label")
        or (
            widen_candidate_label(
                args.maker_backwardation_widen_bps,
                args.maker_backwardation_min_target_spread_bps,
            )
            if rollout_state.get("active_candidate_profile")
            else None
        )
    )

    rounds: list[dict[str, Any]] = []
    idle_waits: list[dict[str, Any]] = []
    stop_reason = "max_rounds_exhausted"
    final_readiness: dict[str, Any] | None = None
    paper_soak: dict[str, Any] | None = None
    runtime_status: dict[str, Any] = {
        "updated_at": datetime.now(timezone.utc).isoformat(),
        "active": True,
        "phase": "initializing",
        "current_round": 0,
        "candidate_label": candidate_label,
        "last_readiness_verdict": None,
        "last_batch_verdict": None,
        "last_blockers": [],
        "next_eligible_delay_seconds": None,
        "stop_reason": None,
    }
    write_runtime_status(outdir, runtime_status)

    if rollout_state.get("active_candidate_profile"):
        args.ready_soak_config = str((root / str(rollout_state["active_candidate_profile"])).resolve())
        selected = load_json((root / args.ready_soak_config).resolve())
        widen = float(selected.get("maker_backwardation_widen_bps") or 0.0)
        floor = float(selected.get("maker_backwardation_min_target_spread_bps") or 0.0)
        if not math.isfinite(widen) or not math.isfinite(floor) or widen <= 0.0 or selected.get("maker_roll_toxic_guard_enabled"):
            raise ValueError("this rollout runner requires an active widening candidate")
        actual_label = widen_candidate_label(widen, floor)
        if args.candidate_label and args.candidate_label != actual_label:
            raise ValueError("requested candidate label does not match the selected profile")
        candidate_label = actual_label
        args.maker_backwardation_widen_bps = widen
        args.maker_backwardation_min_target_spread_bps = floor
        args._selected_config = selected
        args._candidate_fingerprint = config_fingerprint(selected)
    if not rollout_state.get("active_candidate_profile"):
        stop_reason = "no_active_candidate"
        final_readiness = run_json([sys.executable, "scripts/maker_rollout_readiness_report.py", "--rollout-state", str(rollout_state_path)], root)
        final_readiness_obj = load_json(final_readiness["json"])
        report = {
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "candidate_label": candidate_label,
            "maker_backwardation_widen_bps": args.maker_backwardation_widen_bps,
            "maker_backwardation_min_target_spread_bps": args.maker_backwardation_min_target_spread_bps,
            "verdict": final_readiness_obj.get("verdict") or "baseline_only_no_candidate",
            "stop_reason": stop_reason,
            "rounds": [],
            "idle_waits": [],
            "final_readiness": final_readiness_obj,
            "paper_soak": None,
        }
        stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
        json_path = outdir / f"{stamp}_maker_rollout_gate.json"
        md_path = outdir / f"{stamp}_maker_rollout_gate.md"
        latest_json_path = outdir / "latest_maker_rollout_gate.json"
        latest_md_path = outdir / "latest_maker_rollout_gate.md"
        json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
        md_path.write_text(render_markdown(report), encoding="utf-8")
        latest_json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
        latest_md_path.write_text(render_markdown(report), encoding="utf-8")
        runtime_status.update(
            {
                "updated_at": datetime.now(timezone.utc).isoformat(),
                "active": False,
                "phase": "completed",
                "candidate_label": None,
                "last_readiness_verdict": final_readiness_obj.get("verdict"),
                "last_readiness_path": final_readiness["json"],
                "last_blockers": final_readiness_obj.get("blockers") or [],
                "stop_reason": stop_reason,
                "final_verdict": final_readiness_obj.get("verdict"),
            }
        )
        write_runtime_status(outdir, runtime_status)
        print(
            json.dumps(
                {
                    "json": str(json_path),
                    "md": str(md_path),
                    "verdict": report["verdict"],
                    "stop_reason": stop_reason,
                }
            )
        )
        return 0

    completed_rounds = 0
    try:
        while completed_rounds < args.rounds:
            round_index = completed_rounds + 1
            runtime_status.update(
                {
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                    "phase": "refresh_readiness_before",
                    "current_round": round_index,
                }
            )
            write_runtime_status(outdir, runtime_status)
            runtime_status.update(
                {
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                    "phase": "refresh_readiness_before",
                }
            )
            write_runtime_status(outdir, runtime_status)
            readiness_meta = run_json([sys.executable, "scripts/maker_rollout_readiness_report.py", "--rollout-state", str(rollout_state_path)], root)
            readiness_before = load_json(readiness_meta["json"])
            validate_selected_readiness(args, readiness_before)
            runtime_status.update(
                {
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                    "last_readiness_verdict": readiness_before.get("verdict"),
                    "last_readiness_path": readiness_meta["json"],
                    "last_blockers": readiness_before.get("blockers") or [],
                }
            )
            write_runtime_status(outdir, runtime_status)

            round_record: dict[str, Any] = {
                "round_index": round_index,
                "readiness_before": {
                    "path": readiness_meta["json"],
                    "verdict": readiness_before.get("verdict"),
                    "front_runner": (readiness_before.get("front_runner") or {}).get("label"),
                    "blocker_count": len(readiness_before.get("blockers") or []),
                },
            }

            if readiness_before.get("verdict") == "paper_rollout_candidate_ready":
                final_readiness = readiness_before
                stop_reason = "ready_before_batch"
                runtime_status.update(
                    {
                        "updated_at": datetime.now(timezone.utc).isoformat(),
                        "phase": "running_frozen_soak",
                    }
                )
                write_runtime_status(outdir, runtime_status)
                paper_soak = run_ready_soak(args, root)
                rounds.append(round_record)
                break

            runtime_status.update(
                {
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                    "phase": "running_batch",
                    "current_round": round_index,
                }
            )
            write_runtime_status(outdir, runtime_status)
            batch_cmd = [
                sys.executable,
                "scripts/run_maker_ab_batch.py",
                "--config",
                args.config,
                "--candidate-config",
                args.ready_soak_config,
                "--expected-candidate-fingerprint",
                args._candidate_fingerprint,
                "--cycles",
                str(args.batch_cycles),
                "--target-usable-cycles",
                str(args.target_usable_cycles),
                "--max-total-cycles",
                str(args.max_total_cycles),
                "--max-sparse-streak",
                str(args.max_sparse_streak),
                "--min-leg-events",
                str(args.min_leg_events),
                "--duration-minutes",
                str(args.duration_minutes),
                "--report-hours",
                str(args.report_hours),
                "--enable-macro-guard",
                "--candidate-mode",
                "widen",
                "--maker-backwardation-widen-bps",
                str(args.maker_backwardation_widen_bps),
                "--maker-backwardation-min-target-spread-bps",
                str(args.maker_backwardation_min_target_spread_bps),
                "--wait-until-eligible-seconds",
                str(args.wait_until_eligible_seconds),
                "--cargo-cmd",
                args.cargo_cmd,
                "--base-url",
                args.base_url,
            ]
            batch_meta = run_json(batch_cmd, root)
            batch_report = load_json(batch_meta["json"])
            runtime_status.update(
                {
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                    "last_batch_verdict": batch_report.get("verdict"),
                    "last_batch_path": batch_meta["json"],
                    "next_eligible_delay_seconds": (batch_report.get("run_control") or {}).get(
                        "next_eligible_delay_seconds"
                    ),
                }
            )
            write_runtime_status(outdir, runtime_status)
            round_record["batch"] = {
                "path": batch_meta["json"],
                "verdict": batch_report.get("verdict"),
                "widen_bps": (batch_report.get("args") or {}).get("maker_backwardation_widen_bps"),
                "min_target_spread_bps": (batch_report.get("args") or {}).get(
                    "maker_backwardation_min_target_spread_bps"
                ),
                "usable_cycles": (batch_report.get("counts") or {}).get("usable_cycles"),
                "engaged_comparable_windows": (batch_report.get("counts") or {}).get(
                    "engaged_comparable_windows"
                ),
                "avg_adverse_move_delta_bps": (batch_report.get("aggregates") or {}).get(
                    "avg_engaged_adverse_move_delta_bps",
                    (batch_report.get("aggregates") or {}).get("avg_adverse_move_delta_bps"),
                ),
                "avg_places_delta_pct": (batch_report.get("aggregates") or {}).get(
                    "avg_places_delta_pct"
                ),
                "stop_reason": (batch_report.get("run_control") or {}).get("stop_reason"),
            }

            if batch_report.get("verdict") == "awaiting_eligible_start":
                wait_seconds = float(
                    (batch_report.get("run_control") or {}).get("next_eligible_delay_seconds") or 0.0
                )
                idle_waits.append(
                    {
                        "round_index": round_index,
                        "batch_path": batch_meta["json"],
                        "wait_seconds": wait_seconds,
                        "stop_reason": (batch_report.get("run_control") or {}).get("stop_reason"),
                    }
                )
                if wait_seconds <= 0.0:
                    stop_reason = "no_eligible_start_found"
                    rounds.append(round_record)
                    break
                runtime_status.update(
                    {
                        "updated_at": datetime.now(timezone.utc).isoformat(),
                        "phase": "awaiting_eligible_start",
                        "next_eligible_delay_seconds": wait_seconds,
                    }
                )
                write_runtime_status(outdir, runtime_status)
                if wait_seconds > max(0.0, args.wait_until_eligible_seconds):
                    stop_reason = "eligible_start_outside_wait_budget"
                    rounds.append(round_record)
                    break
                time.sleep(wait_seconds)
                completed_rounds += 1
                continue

            runtime_status.update(
                {
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                    "phase": "refresh_readiness_after",
                    "next_eligible_delay_seconds": None,
                }
            )
            write_runtime_status(outdir, runtime_status)
            readiness_meta_after = run_json(
                [sys.executable, "scripts/maker_rollout_readiness_report.py", "--rollout-state", str(rollout_state_path)], root
            )
            readiness_after = load_json(readiness_meta_after["json"])
            validate_selected_readiness(args, readiness_after)
            runtime_status.update(
                {
                    "updated_at": datetime.now(timezone.utc).isoformat(),
                    "last_readiness_verdict": readiness_after.get("verdict"),
                    "last_readiness_path": readiness_meta_after["json"],
                    "last_blockers": readiness_after.get("blockers") or [],
                }
            )
            write_runtime_status(outdir, runtime_status)
            round_record["readiness_after"] = {
                "path": readiness_meta_after["json"],
                "verdict": readiness_after.get("verdict"),
                "front_runner": (readiness_after.get("front_runner") or {}).get("label"),
                "blocker_count": len(readiness_after.get("blockers") or []),
            }
            rounds.append(round_record)
            completed_rounds += 1
            final_readiness = readiness_after

            if readiness_after.get("verdict") == "paper_rollout_candidate_ready":
                stop_reason = "ready_after_batch"
                runtime_status.update(
                    {
                        "updated_at": datetime.now(timezone.utc).isoformat(),
                        "phase": "running_frozen_soak",
                    }
                )
                write_runtime_status(outdir, runtime_status)
                paper_soak = run_ready_soak(args, root)
                break
    except KeyboardInterrupt:
        stop_reason = "interrupted"

    if final_readiness is None:
        runtime_status.update(
            {
                "updated_at": datetime.now(timezone.utc).isoformat(),
                "phase": "refresh_readiness_final",
            }
        )
        write_runtime_status(outdir, runtime_status)
        readiness_meta = run_json([sys.executable, "scripts/maker_rollout_readiness_report.py", "--rollout-state", str(rollout_state_path)], root)
        final_readiness = load_json(readiness_meta["json"])
        validate_selected_readiness(args, final_readiness)
        runtime_status.update(
            {
                "updated_at": datetime.now(timezone.utc).isoformat(),
                "last_readiness_verdict": final_readiness.get("verdict"),
                "last_readiness_path": readiness_meta["json"],
                "last_blockers": final_readiness.get("blockers") or [],
            }
        )
        write_runtime_status(outdir, runtime_status)

    verdict = final_readiness.get("verdict") or "unknown"
    report = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "candidate_label": candidate_label,
        "maker_backwardation_widen_bps": args.maker_backwardation_widen_bps,
        "maker_backwardation_min_target_spread_bps": args.maker_backwardation_min_target_spread_bps,
        "verdict": verdict,
        "stop_reason": stop_reason,
        "rounds": rounds,
        "idle_waits": idle_waits,
        "final_readiness": final_readiness,
        "paper_soak": paper_soak,
    }

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_maker_rollout_gate.json"
    md_path = outdir / f"{stamp}_maker_rollout_gate.md"
    latest_json_path = outdir / "latest_maker_rollout_gate.json"
    latest_md_path = outdir / "latest_maker_rollout_gate.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    md_path.write_text(render_markdown(report), encoding="utf-8")
    latest_json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    latest_md_path.write_text(render_markdown(report), encoding="utf-8")

    runtime_status.update(
        {
            "updated_at": datetime.now(timezone.utc).isoformat(),
            "active": False,
            "phase": "completed",
            "stop_reason": stop_reason,
            "final_verdict": verdict,
            "paper_soak_report": (paper_soak or {}).get("report", {}).get("json"),
        }
    )
    write_runtime_status(outdir, runtime_status)

    print(json.dumps({"json": str(json_path), "md": str(md_path), "verdict": verdict, "stop_reason": stop_reason}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
