#!/usr/bin/env python3
"""Replay recent-specialist validation across shifted recent anchors.

This is a paper-soak proxy. Instead of trusting one "today", it reruns the
recent specialist validation wrapper across a sequence of shifted anchor days
and scores how often each preset still looks healthy.
"""

from __future__ import annotations

import argparse
import json
import os
import statistics
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from api_auth import add_api_auth_args, resolve_api_key, resolve_api_secret
from backtest_trade_report_common import redacted_command


ROOT = Path(__file__).resolve().parent.parent


@dataclass
class ShiftValidation:
    anchor_shift_days: int
    exit_code: int
    report: dict[str, Any]
    command: list[str]


def csv_ints(raw: str) -> list[int]:
    out: list[int] = []
    for part in raw.split(","):
        value = part.strip()
        if value:
            out.append(int(value))
    return out


def csv_strings(raw: str) -> list[str]:
    return [part.strip() for part in raw.split(",") if part.strip()]


def run_validation(command: list[str]) -> tuple[int, dict[str, Any]]:
    proc = subprocess.run(
        command,
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    stdout = proc.stdout.strip()
    if proc.returncode not in (0, 2):
        detail = proc.stderr.strip() or stdout or f"exit code {proc.returncode}"
        raise RuntimeError(detail)
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"invalid JSON: {exc}") from exc
    if not isinstance(payload, dict):
        raise RuntimeError("unexpected payload shape")
    return proc.returncode, payload


def summarize_runs(preset: str, runs: list[ShiftValidation]) -> dict[str, Any]:
    verdict_counts = Counter(str(run.report.get("overall", {}).get("verdict", "unknown")) for run in runs)
    recent_recency_passes = sum(str(run.report.get("recent_recency", {}).get("status")) == "pass" for run in runs)
    recent_window_passes = sum(bool(run.report.get("overall", {}).get("recent_window_pass")) for run in runs)
    long_stress_passes = sum(bool(run.report.get("overall", {}).get("long_stress_pass")) for run in runs)
    lane_positive_passes = sum(bool(run.report.get("overall", {}).get("lane_positive_now")) for run in runs)
    daily_passes = sum(str(run.report.get("daily_report", {}).get("status")) == "pass" for run in runs)
    lane_pnls = [
        float(run.report.get("lane_forensics", {}).get("summary", {}).get("total_pnl", 0.0) or 0.0)
        for run in runs
    ]
    lane_trades = [
        int(run.report.get("lane_forensics", {}).get("summary", {}).get("trades", 0) or 0)
        for run in runs
    ]
    if not runs or not any(run.anchor_shift_days == 0 for run in runs):
        raise ValueError("current anchor 0 is required; historical shifts cannot stand in for today")
    current = next(run for run in runs if run.anchor_shift_days == 0)
    run_rows = []
    for run in runs:
        overall = run.report.get("overall", {}) or {}
        lane_summary = run.report.get("lane_forensics", {}).get("summary", {}) or {}
        run_rows.append(
            {
                "anchor_shift_days": run.anchor_shift_days,
                "verdict": overall.get("verdict"),
                "daily_pass": str(run.report.get("daily_report", {}).get("status")) == "pass",
                "recent_recency_pass": str(run.report.get("recent_recency", {}).get("status")) == "pass",
                "recent_window_pass": bool(overall.get("recent_window_pass")),
                "long_stress_pass": bool(overall.get("long_stress_pass")),
                "lane_positive_now": bool(overall.get("lane_positive_now")),
                "lane_total_pnl": float(lane_summary.get("total_pnl", 0.0) or 0.0),
                "lane_trades": int(lane_summary.get("trades", 0) or 0),
                "artifact_json": run.report.get("artifacts", {}).get("json"),
            }
        )
    run_rows.sort(key=lambda row: row["anchor_shift_days"])

    summary = {
        "preset": preset,
        "anchors_tested": len(runs),
        "verdict_counts": dict(verdict_counts),
        "recent_recency_pass_count": recent_recency_passes,
        "recent_recency_pass_ratio": recent_recency_passes / len(runs) if runs else 0.0,
        "recent_window_pass_count": recent_window_passes,
        "recent_window_pass_ratio": recent_window_passes / len(runs) if runs else 0.0,
        "long_stress_pass_count": long_stress_passes,
        "long_stress_pass_ratio": long_stress_passes / len(runs) if runs else 0.0,
        "lane_positive_count": lane_positive_passes,
        "lane_positive_ratio": lane_positive_passes / len(runs) if runs else 0.0,
        "daily_pass_count": daily_passes,
        "daily_pass_ratio": daily_passes / len(runs) if runs else 0.0,
        "mean_lane_pnl": statistics.mean(lane_pnls) if lane_pnls else 0.0,
        "median_lane_pnl": statistics.median(lane_pnls) if lane_pnls else 0.0,
        "mean_lane_trades": statistics.mean(lane_trades) if lane_trades else 0.0,
        "current_anchor": {
            "anchor_shift_days": current.anchor_shift_days,
            "verdict": current.report.get("overall", {}).get("verdict"),
            "daily_pass": str(current.report.get("daily_report", {}).get("status")) == "pass",
            "recent_recency_pass": str(current.report.get("recent_recency", {}).get("status")) == "pass",
            "recent_window_pass": bool(current.report.get("overall", {}).get("recent_window_pass")),
            "long_stress_pass": bool(current.report.get("overall", {}).get("long_stress_pass")),
            "lane_positive_now": bool(current.report.get("overall", {}).get("lane_positive_now")),
            "artifact_json": current.report.get("artifacts", {}).get("json"),
        },
        "runs": run_rows,
    }
    return summary


def rank_key(summary: dict[str, Any]) -> tuple[float, float, float, float, float, float]:
    current = summary["current_anchor"]
    return (
        float(current["recent_recency_pass"]),
        float(current["long_stress_pass"]),
        float(current["lane_positive_now"]),
        summary["recent_recency_pass_ratio"],
        summary["long_stress_pass_ratio"],
        summary["lane_positive_ratio"],
        summary["recent_window_pass_ratio"],
        summary["daily_pass_ratio"],
        summary["mean_lane_pnl"],
    )


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_recent_specialist_soak_report.json"
    md_path = out_dir / f"{ts}_recent_specialist_soak_report.md"

    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    lines = [
        "# Recent Specialist Soak Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- presets: {', '.join(report['presets'])}",
        f"- anchor_shifts_days: {report['anchor_shifts_days']}",
        f"- status: {report['status']}",
        f"- best_preset: {report['best_preset']}",
        "",
        "## Summary",
        "",
        f"- {report['summary_line']}",
    ]

    for summary in report["preset_summaries"]:
        current = summary["current_anchor"]
        lines.extend(
            [
                "",
                f"## {summary['preset']}",
                "",
                f"- recent_recency_pass_ratio: {summary['recent_recency_pass_ratio']:.2f}",
                f"- recent_window_pass_ratio: {summary['recent_window_pass_ratio']:.2f}",
                f"- long_stress_pass_ratio: {summary['long_stress_pass_ratio']:.2f}",
                f"- lane_positive_ratio: {summary['lane_positive_ratio']:.2f}",
                f"- daily_pass_ratio: {summary['daily_pass_ratio']:.2f}",
                f"- mean_lane_pnl: {summary['mean_lane_pnl']:.4f}",
                f"- mean_lane_trades: {summary['mean_lane_trades']:.2f}",
                f"- current_anchor_verdict: {current['verdict']}",
                f"- current_anchor_artifact: {current['artifact_json']}",
            ]
        )

    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Replay recent specialist validation across shifted anchors")
    parser.add_argument(
        "--presets",
        default="trend_up_yes_recent_specialist,trend_up_yes_hour_filtered_specialist",
    )
    parser.add_argument("--anchor-shifts", default="0,1,2,3,4,5,6")
    parser.add_argument("--base-url", default="http://localhost:8000")
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    parser.add_argument("--request-timeout-secs", type=int, default=180)
    parser.add_argument("--request-interval-secs", type=float, default=1.3)
    parser.add_argument("--time-of-day-weight", action="store_true")
    parser.add_argument("--daily-days", type=int, default=14)
    parser.add_argument("--daily-min-mean-trades", type=float, default=10.0)
    parser.add_argument("--lane-days", type=int, default=7)
    add_api_auth_args(parser)
    args = parser.parse_args()
    # Child tools inherit credentials; never copy secrets into process arguments.
    os.environ["MACBOT_API_KEY"] = resolve_api_key(args.api_key)
    os.environ["MACBOT_API_HMAC_SECRET"] = resolve_api_secret(args.api_secret)

    presets = list(dict.fromkeys(csv_strings(args.presets)))
    anchor_shifts = list(dict.fromkeys(csv_ints(args.anchor_shifts)))
    if not presets:
        raise SystemExit("no presets provided")
    if not anchor_shifts or 0 not in anchor_shifts or any(shift < 0 for shift in anchor_shifts):
        raise SystemExit("anchor shifts must be nonnegative and include 0 for the current anchor")

    preset_summaries: list[dict[str, Any]] = []
    commands: dict[str, list[list[str]]] = {}
    for preset in presets:
        runs: list[ShiftValidation] = []
        preset_commands: list[list[str]] = []
        for shift in anchor_shifts:
            command = [
                sys.executable,
                "scripts/recent_specialist_validation_report.py",
                "--preset",
                preset,
                "--base-url",
                args.base_url,
                "--output-dir",
                args.output_dir,
                "--request-timeout-secs",
                str(args.request_timeout_secs),
                "--request-interval-secs",
                str(args.request_interval_secs),
                "--anchor-shift-days",
                str(shift),
                "--daily-days",
                str(args.daily_days),
                "--daily-min-mean-trades",
                str(args.daily_min_mean_trades),
                "--lane-days",
                str(args.lane_days),
            ]
            if args.time_of_day_weight:
                command.append("--time-of-day-weight")
            exit_code, report = run_validation(command)
            runs.append(
                ShiftValidation(
                    anchor_shift_days=shift,
                    exit_code=exit_code,
                    report=report,
                    command=redacted_command(command),
                )
            )
            preset_commands.append(redacted_command(command))
        preset_summaries.append(summarize_runs(preset, runs))
        commands[preset] = preset_commands

    preset_summaries.sort(key=rank_key, reverse=True)
    best = preset_summaries[0]
    best_current = best["current_anchor"]
    if (
        best_current["recent_recency_pass"]
        and best["recent_recency_pass_ratio"] >= 0.67
        and best["long_stress_pass_ratio"] >= 0.67
        and best["lane_positive_ratio"] >= 0.67
        and best["daily_pass_ratio"] >= 0.50
    ):
        status = "pass"
        summary_line = (
            f"{best['preset']} is the strongest recent specialist soak candidate with "
            f"recent-recency pass ratio {best['recent_recency_pass_ratio']:.2f}, "
            f"long-stress pass ratio {best['long_stress_pass_ratio']:.2f}, and "
            f"daily pass ratio {best['daily_pass_ratio']:.2f}."
        )
    elif (
        best_current["recent_recency_pass"]
        and best["recent_recency_pass_ratio"] >= 0.67
        and best["long_stress_pass_ratio"] >= 0.67
        and best["lane_positive_ratio"] >= 0.67
    ):
        status = "sparse_candidate"
        summary_line = (
            f"{best['preset']} looks like a sparse but stable specialist: "
            f"recent-recency pass ratio {best['recent_recency_pass_ratio']:.2f}, "
            f"long-stress pass ratio {best['long_stress_pass_ratio']:.2f}, "
            f"lane-positive ratio {best['lane_positive_ratio']:.2f}, but daily pass ratio only "
            f"{best['daily_pass_ratio']:.2f}."
        )
    else:
        status = "fail"
        summary_line = (
            f"{best['preset']} is still not stable enough: recent-recency pass ratio "
            f"{best['recent_recency_pass_ratio']:.2f}, long-stress pass ratio "
            f"{best['long_stress_pass_ratio']:.2f}, daily pass ratio {best['daily_pass_ratio']:.2f}."
        )
    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "presets": presets,
        "anchor_shifts_days": anchor_shifts,
        "status": status,
        "best_preset": best["preset"],
        "summary_line": summary_line,
        "preset_summaries": preset_summaries,
        "commands": commands,
    }
    out_dir = Path(args.output_dir)
    json_path, md_path = write_outputs(report, out_dir)
    report["artifacts"] = {"json": str(json_path), "markdown": str(md_path)}

    print(json.dumps(report, indent=2))
    return 0 if status in {"pass", "sparse_candidate"} else 2


if __name__ == "__main__":
    raise SystemExit(main())
