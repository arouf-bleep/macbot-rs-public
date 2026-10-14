#!/usr/bin/env python3
"""Run a compact validation wrapper around the exact-archive shadow filter rail."""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent


@dataclass
class ChildReport:
    exit_code: int
    report: dict[str, Any]
    command: list[str]


def run_child(command: list[str]) -> ChildReport:
    proc = subprocess.run(
        command,
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    stdout = proc.stdout.strip()
    if proc.returncode != 0:
        detail = proc.stderr.strip() or stdout or f"exit code {proc.returncode}"
        raise RuntimeError(f"shadow filter report failed: {detail}")
    try:
        payload = stdout
        lines = stdout.splitlines()
        for idx, line in enumerate(lines):
            if line.lstrip().startswith("{"):
                payload = "\n".join(lines[idx:])
                break
        report = json.loads(payload)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"shadow filter report returned invalid JSON: {exc}") from exc
    if not isinstance(report, dict):
        raise RuntimeError("shadow filter report returned a non-object payload")
    return ChildReport(exit_code=proc.returncode, report=report, command=command)


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_deep_model_shadow_validation_report.json"
    md_path = out_dir / f"{ts}_deep_model_shadow_validation_report.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    overall = report["overall"]
    child = report["shadow_filter"]
    lines = [
        "# Deep Model Shadow Validation Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- preset: {report['preset']}",
        f"- model: {report['model']}",
        f"- threshold: {report['threshold']:.3f}",
        f"- verdict: {overall['verdict']}",
        f"- current_window_improved: {overall['current_window_improved']}",
        f"- improved_fraction: {overall['improved_fraction']:.2%}",
        f"- recent_positive_windows: {overall['recent_positive_windows']}",
        f"- stress_positive_windows: {overall['stress_positive_windows']}",
        "",
        "## Child Artifact",
        "",
        f"- json: {child['artifacts'].get('json')}",
        f"- markdown: {child['artifacts'].get('markdown')}",
        "",
        "## Interpretation",
        "",
        f"- {overall['summary_line']}",
    ]
    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Deep-model shadow validation wrapper")
    parser.add_argument("--db", required=True)
    parser.add_argument("--model", required=True)
    parser.add_argument("--head", choices=("p_up", "calibration"), default="p_up")
    parser.add_argument("--preset", default="trend_up_yes_specialist")
    parser.add_argument("--threshold", type=float, default=0.56)
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    args = parser.parse_args()

    command = [
        sys.executable,
        "scripts/deep_model_shadow_filter_report.py",
        "--db",
        args.db,
        "--model",
        args.model,
        "--head",
        args.head,
        "--preset",
        args.preset,
        "--threshold",
        str(args.threshold),
        "--days-list",
        "7,14,21,30",
        "--shift-days-list",
        "0,7,14,21",
        "--report-label",
        f"{args.preset}_shadow_filter_validation_child",
    ]
    child = run_child(command)
    window_reports = child.report.get("window_reports", [])

    current = next(
        (row for row in window_reports if row.get("days") == 7 and row.get("shift_days") == 0),
        None,
    )
    recent_rows = [row for row in window_reports if row.get("days") == 7]
    stress_rows = [row for row in window_reports if row.get("days") in {14, 21, 30}]
    recent_positive = sum(1 for row in recent_rows if row.get("improved"))
    stress_positive = sum(1 for row in stress_rows if row.get("improved"))
    improved_fraction = float(child.report.get("overall", {}).get("improved_fraction", 0.0))
    current_window_improved = bool(child.report.get("overall", {}).get("current_window_improved"))

    if current_window_improved and improved_fraction >= 0.5 and stress_positive >= 4:
        verdict = "shadow_recent_candidate"
        summary_line = (
            "The shadow model is improving the broader specialist lane in the current window "
            "and is holding up across a meaningful share of shifted stress windows."
        )
        exit_code = 0
    elif improved_fraction >= 0.5 or recent_positive >= 2:
        verdict = "shadow_mixed_candidate"
        summary_line = (
            "The shadow model helps on a meaningful share of windows, but the current window "
            "or the broader stress mix is still too inconsistent to trust as a live gate."
        )
        exit_code = 0
    else:
        verdict = "shadow_failing_candidate"
        summary_line = (
            "The shadow model is not improving enough windows on the broader specialist lane "
            "to justify treating it as a trustworthy gate yet."
        )
        exit_code = 2

    report = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "db": args.db,
        "model": args.model,
        "head": args.head,
        "preset": args.preset,
        "threshold": args.threshold,
        "shadow_filter": child.report,
        "commands": {"shadow_filter": child.command},
        "overall": {
            "verdict": verdict,
            "current_window_improved": current_window_improved,
            "current_window_lift_pp": float(current.get("win_rate_lift_pp", 0.0)) if current else 0.0,
            "current_window_coverage": float(current.get("coverage", 0.0)) if current else 0.0,
            "recent_positive_windows": recent_positive,
            "stress_positive_windows": stress_positive,
            "improved_fraction": improved_fraction,
            "summary_line": summary_line,
        },
    }
    out_dir = Path(args.output_dir)
    if not out_dir.is_absolute():
        out_dir = (ROOT / out_dir).resolve()
    json_path, md_path = write_outputs(report, out_dir)
    report["artifacts"] = {
        "json": str(json_path),
        "markdown": str(md_path),
    }
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
