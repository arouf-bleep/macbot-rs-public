#!/usr/bin/env python3
"""Run the current recent-specialist paper-validation suite.

This wrapper keeps the recent TREND_UP + YES specialist on a fixed rail:
- daily walk-forward report
- short-horizon recency gate
- longer stress recency gate
- current-lane forensic snapshot

Exit codes:
  0 = recent-window candidate still healthy
  2 = candidate failing current paper-validation gates
  1 = runtime error
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from api_auth import add_api_auth_args, resolve_api_key, resolve_api_secret
from backtest_trade_report_common import redacted_command


ROOT = Path(__file__).resolve().parent.parent


@dataclass
class ChildReport:
    name: str
    exit_code: int
    report: dict[str, Any]
    command: list[str]


def run_child(name: str, command: list[str]) -> ChildReport:
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
        raise RuntimeError(f"{name} failed: {detail}")
    try:
        report = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"{name} returned invalid JSON: {exc}") from exc
    if not isinstance(report, dict):
        raise RuntimeError(f"{name} returned unexpected payload shape")
    return ChildReport(name=name, exit_code=proc.returncode, report=report, command=redacted_command(command))


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_recent_specialist_validation_report.json"
    md_path = out_dir / f"{ts}_recent_specialist_validation_report.md"

    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    overall = report["overall"]
    daily = report["daily_report"]
    recent = report["recent_recency"]
    long = report["long_recency"]
    lane = report["lane_forensics"]
    signal = report["signal_audit"]
    current_signal = (signal.get("shifts") or [{}])[0]
    surface = current_signal.get("surface_assessment", {}) or {}
    def correlation_text(name: str) -> str:
        value = surface.get(name)
        return f"{value:.4f}" if isinstance(value, (int, float)) else "unavailable"

    lines = [
        "# Recent Specialist Validation Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- preset: {report['preset']}",
        f"- anchor_shift_days: {report['anchor_shift_days']}",
        f"- verdict: {overall['verdict']}",
        f"- recent_window_pass: {overall['recent_window_pass']}",
        f"- long_stress_pass: {overall['long_stress_pass']}",
        f"- lane_positive_now: {overall['lane_positive_now']}",
        "",
        "## Current Lane",
        "",
        f"- trades: {lane['summary']['trades']}",
        f"- total_pnl: {lane['summary']['total_pnl']:.4f}",
        f"- win_rate: {lane['summary']['win_rate']:.2f}%",
        f"- edge_gap_pp: {lane['summary']['edge_gap_pp']:.2f}",
        "",
        "## Signal Surface",
        "",
        f"- surface_label: {surface.get('label', 'unknown')}",
        f"- warnings: {', '.join(surface.get('warnings', [])) or 'none'}",
        f"- edge_margin_corr_win: {correlation_text('edge_margin_corr_win')}",
        f"- next_return_corr_win: {correlation_text('next_return_corr_win')}",
        "",
        "## Daily Report",
        "",
        f"- status: {daily['status']}",
        f"- artifact_json: {daily['artifacts'].get('json')}",
        f"- artifact_markdown: {daily['artifacts'].get('markdown')}",
        "",
        "## Recent Recency Gate",
        "",
        f"- status: {recent['status']}",
        f"- artifact_json: {recent['artifacts'].get('json')}",
        f"- artifact_markdown: {recent['artifacts'].get('markdown')}",
        "",
        "## Long Stress Gate",
        "",
        f"- status: {long['status']}",
        f"- artifact_json: {long['artifacts'].get('json')}",
        f"- artifact_markdown: {long['artifacts'].get('markdown')}",
        "",
        "## Interpretation",
        "",
        f"- {overall['summary_line']}",
    ]
    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Recent specialist paper-validation suite")
    parser.add_argument("--preset", default="trend_up_yes_recent_specialist")
    parser.add_argument("--base-url", default="http://localhost:8000")
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    parser.add_argument("--request-timeout-secs", type=int, default=180)
    parser.add_argument("--request-interval-secs", type=float, default=1.1)
    parser.add_argument("--anchor-shift-days", type=int, default=0)
    parser.add_argument("--time-of-day-weight", action="store_true")
    parser.add_argument("--daily-days", type=int, default=14)
    parser.add_argument("--daily-min-mean-trades", type=float, default=10.0)
    parser.add_argument("--lane-days", type=int, default=7)
    add_api_auth_args(parser)
    args = parser.parse_args()
    # Child tools inherit credentials; never copy secrets into process arguments.
    os.environ["MACBOT_API_KEY"] = resolve_api_key(args.api_key)
    os.environ["MACBOT_API_HMAC_SECRET"] = resolve_api_secret(args.api_secret)

    common = [
        "--preset",
        args.preset,
        "--base-url",
        args.base_url,
        "--request-timeout-secs",
        str(args.request_timeout_secs),
        "--output-dir",
        args.output_dir,
    ]
    sweep_common = [
        *common,
        "--anchor-shift-days",
        str(args.anchor_shift_days),
        "--request-interval-secs",
        str(args.request_interval_secs),
    ]
    if args.time_of_day_weight:
        sweep_common.append("--time-of-day-weight")

    daily = run_child(
        "daily_alpha_report",
        [
            sys.executable,
            "scripts/daily_alpha_report.py",
            *sweep_common,
            "--days",
            str(args.daily_days),
            "--min-mean-trades",
            str(args.daily_min_mean_trades),
            "--fold-shifts",
            "0,7,14",
            "--top",
            "1",
        ],
    )
    recent = run_child(
        "recent_recency",
        [
            sys.executable,
            "scripts/recency_alpha_report.py",
            *sweep_common,
            "--days-list",
            "7,14,21",
            "--stress-days",
            "30",
            "--fold-shifts",
            "0,7,14",
            "--stress-fold-shifts",
            "0,7,14",
            "--top",
            "1",
        ],
    )
    long = run_child(
        "long_recency",
        [
            sys.executable,
            "scripts/recency_alpha_report.py",
            *sweep_common,
            "--days-list",
            "14,21,30",
            "--stress-days",
            "60",
            "--fold-shifts",
            "0,7,14",
            "--stress-fold-shifts",
            "0,14,28",
            "--top",
            "1",
        ],
    )
    lane = run_child(
        "lane_forensics",
        [
            sys.executable,
            "scripts/lane_forensics_report.py",
            *common,
            "--lane-label",
            f"{args.preset}_current",
            "--days",
            str(args.lane_days),
            "--shift-days",
            str(args.anchor_shift_days),
            "--filter-regimes",
            "TREND_UP",
            "--filter-signals",
            "BUY_YES",
        ],
    )
    signal = run_child(
        "signal_audit",
        [
            sys.executable,
            "scripts/signal_validity_report.py",
            "--preset",
            args.preset,
            "--base-url",
            args.base_url,
            "--days",
            str(args.lane_days),
            "--shift-days-list",
            str(args.anchor_shift_days),
            "--report-label",
            f"{args.preset}_current_signal",
            "--filter-regimes",
            "TREND_UP",
            "--filter-signals",
            "BUY_YES",
        ],
    )

    lane_summary = lane.report.get("summary", {}) or {}
    signal_surface = (
        ((signal.report.get("shifts") or [{}])[0]).get("surface_assessment", {}) or {}
    )
    recent_window_pass = recent.report.get("status") == "pass" and daily.report.get("status") == "pass"
    long_stress_pass = long.report.get("status") == "pass"
    lane_positive_now = (
        float(lane_summary.get("total_pnl", 0.0)) > 0.0
        and float(lane_summary.get("edge_gap_pp", 0.0)) > 0.0
    )

    if recent_window_pass and long_stress_pass and lane_positive_now:
        verdict = "robust_candidate"
        summary_line = "The specialist is passing both recent and longer stress gates."
        exit_code = 0
    elif recent_window_pass and lane_positive_now:
        verdict = "recent_specialist_candidate"
        summary_line = "The specialist is healthy in the recent paper window but still fails the longer stress view."
        exit_code = 0
    else:
        verdict = "failing_candidate"
        summary_line = "The specialist is not currently healthy enough in the recent paper window to trust."
        exit_code = 2
    if signal_surface.get("label") == "coarse_gate_inverted_ranking":
        summary_line += " Signal audit warns this lane is a coarse gate with an inverted edge-ranking surface."
    elif signal_surface.get("label") == "coarse_gate_only":
        summary_line += " Signal audit warns this lane behaves like a coarse gate, not a ranked score surface."

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "preset": args.preset,
        "anchor_shift_days": args.anchor_shift_days,
        "daily_report": daily.report,
        "recent_recency": recent.report,
        "long_recency": long.report,
        "lane_forensics": lane.report,
        "signal_audit": signal.report,
        "commands": {
            daily.name: daily.command,
            recent.name: recent.command,
            long.name: long.command,
            lane.name: lane.command,
            signal.name: signal.command,
        },
        "overall": {
            "verdict": verdict,
            "recent_window_pass": recent_window_pass,
            "long_stress_pass": long_stress_pass,
            "lane_positive_now": lane_positive_now,
            "signal_surface": signal_surface,
            "summary_line": summary_line,
        },
    }
    out_dir = Path(args.output_dir)
    if not out_dir.is_absolute():
        out_dir = (ROOT / out_dir).resolve()
    json_path, md_path = write_outputs(report, out_dir)
    report["artifacts"] = {
        "json": str(json_path.relative_to(ROOT)) if json_path.is_relative_to(ROOT) else str(json_path),
        "markdown": str(md_path.relative_to(ROOT)) if md_path.is_relative_to(ROOT) else str(md_path),
    }
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
