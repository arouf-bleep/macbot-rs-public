#!/usr/bin/env python3
"""Evaluate the promoted shadow model on resolved live exact-input rows."""

from __future__ import annotations

import argparse
import json
import sqlite3
import subprocess
import sys
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent


@dataclass
class ChildReport:
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
        raise RuntimeError(f"shadow filter child failed: {detail}")
    payload = stdout
    lines = stdout.splitlines()
    for idx, line in enumerate(lines):
        if line.lstrip().startswith("{"):
            payload = "\n".join(lines[idx:])
            break
    return ChildReport(report=json.loads(payload), command=command)


def load_archive_stats(db_path: Path) -> dict[str, Any]:
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row

    has_table = conn.execute(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='ml_model_inputs'"
    ).fetchone()[0]
    if not has_table:
        conn.close()
        return {"status": "missing_table", "table": "ml_model_inputs"}

    counts = conn.execute(
        """
        SELECT
            COUNT(*) AS total_rows,
            SUM(CASE WHEN outcome IS NOT NULL THEN 1 ELSE 0 END) AS labeled_rows,
            SUM(CASE WHEN outcome IS NULL THEN 1 ELSE 0 END) AS unresolved_rows,
            COUNT(DISTINCT market_id) AS total_markets,
            COUNT(DISTINCT CASE WHEN outcome IS NOT NULL THEN market_id END) AS labeled_markets,
            COUNT(DISTINCT CASE WHEN outcome IS NULL THEN market_id END) AS unresolved_markets,
            MIN(timestamp) AS first_timestamp,
            MAX(timestamp) AS last_timestamp,
            MAX(CASE WHEN outcome IS NOT NULL THEN timestamp END) AS last_labeled_timestamp
        FROM ml_model_inputs
        WHERE run_id NOT LIKE 'reconstructed:%' AND run_id NOT LIKE 'backfill_%' AND trim(run_id) <> ''
        """
    ).fetchone()

    unresolved = [
        {
            "market_id": row["market_id"],
            "rows": int(row["row_count"]),
            "last_timestamp": row["last_timestamp"],
        }
        for row in conn.execute(
            """
            SELECT market_id, COUNT(*) AS row_count, MAX(timestamp) AS last_timestamp
            FROM ml_model_inputs
            WHERE outcome IS NULL AND run_id NOT LIKE 'reconstructed:%' AND run_id NOT LIKE 'backfill_%' AND trim(run_id) <> ''
            GROUP BY market_id
            ORDER BY last_timestamp DESC
            LIMIT 10
            """
        ).fetchall()
    ]

    conn.close()
    total_rows = int(counts["total_rows"] or 0)
    labeled_rows = int(counts["labeled_rows"] or 0)
    unresolved_rows = int(counts["unresolved_rows"] or 0)
    return {
        "status": "ok",
        "total_rows": total_rows,
        "labeled_rows": labeled_rows,
        "labeled_pct": (labeled_rows / total_rows) * 100.0 if total_rows else 0.0,
        "unresolved_rows": unresolved_rows,
        "unresolved_pct": (unresolved_rows / total_rows) * 100.0 if total_rows else 0.0,
        "total_markets": int(counts["total_markets"] or 0),
        "labeled_markets": int(counts["labeled_markets"] or 0),
        "unresolved_markets": int(counts["unresolved_markets"] or 0),
        "first_timestamp": counts["first_timestamp"],
        "last_timestamp": counts["last_timestamp"],
        "last_labeled_timestamp": counts["last_labeled_timestamp"],
        "recent_unresolved_markets": unresolved,
    }


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    ts = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = out_dir / f"{ts}_live_shadow_report.json"
    md_path = out_dir / f"{ts}_live_shadow_report.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")

    archive = report["archive"]
    overall = report["overall"]
    lines = [
        "# Live Shadow Report",
        "",
        f"- generated_at_utc: {report['generated_at_utc']}",
        f"- db: `{report['db']}`",
        f"- model: `{report['model']}`",
        f"- preset: `{report['preset']}`",
        f"- threshold: `{report['threshold']:.3f}`",
        f"- verdict: `{overall['verdict']}`",
        "",
        "## Archive Coverage",
        "",
        f"- total_rows: `{archive.get('total_rows', 0)}`",
        f"- labeled_rows: `{archive.get('labeled_rows', 0)}` (`{archive.get('labeled_pct', 0.0):.1f}%`)",
        f"- unresolved_rows: `{archive.get('unresolved_rows', 0)}` (`{archive.get('unresolved_pct', 0.0):.1f}%`)",
        f"- labeled_markets: `{archive.get('labeled_markets', 0)}` / `{archive.get('total_markets', 0)}`",
    ]

    child = report.get("shadow_filter")
    if child:
        current = child["window_reports"][0]
        lines.extend(
            [
                "",
                "## Live Window",
                "",
                f"- baseline_trades: `{current['baseline']['trades']}`",
                f"- baseline_win_rate: `{current['baseline']['win_rate']:.2f}%`",
                f"- filtered_trades: `{current['filtered']['trades']}`",
                f"- filtered_win_rate: `{current['filtered']['win_rate']:.2f}%`",
                f"- win_rate_lift_pp: `{current['win_rate_lift_pp']:.2f}`",
                f"- coverage: `{current['coverage']:.2%}`",
                f"- improved: `{current['improved']}`",
            ]
        )

    lines.extend(
        [
            "",
            "## Interpretation",
            "",
            f"- {overall['summary_line']}",
        ]
    )
    md_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return json_path, md_path


def main() -> int:
    parser = argparse.ArgumentParser(description="Evaluate live resolved exact rows as a shadow lane")
    parser.add_argument("--db", default="trades.db")
    parser.add_argument("--model", default="models/latest.onnx")
    parser.add_argument("--head", choices=("p_up", "calibration"), default="p_up")
    parser.add_argument("--preset", default="trend_up_yes_specialist")
    parser.add_argument("--threshold", type=float, default=0.56)
    parser.add_argument("--max-days", type=int, default=14)
    parser.add_argument("--min-labeled-rows", type=int, default=10)
    parser.add_argument("--output-dir", default="artifacts/alpha-reports")
    args = parser.parse_args()

    db_path = Path(args.db)
    if not db_path.is_absolute():
        db_path = (ROOT / db_path).resolve()
    model_path = Path(args.model)
    if not model_path.is_absolute():
        model_path = (ROOT / model_path).resolve()

    archive = load_archive_stats(db_path)
    report: dict[str, Any] = {
        "generated_at_utc": datetime.now(timezone.utc).isoformat(),
        "db": str(db_path),
        "model": str(model_path),
        "head": args.head,
        "preset": args.preset,
        "threshold": args.threshold,
        "max_days": args.max_days,
        "archive": archive,
    }

    if archive.get("status") != "ok":
        report["overall"] = {
            "verdict": "missing_exact_archive",
            "summary_line": "The live DB does not contain an ml_model_inputs archive yet.",
        }
    elif int(archive.get("labeled_rows", 0)) == 0:
        report["overall"] = {
            "verdict": "waiting_for_resolutions",
            "summary_line": "The exact archive has not accumulated any resolved live rows yet, so there is nothing honest to score.",
        }
    else:
        child_cmd = [
            sys.executable,
            "scripts/deep_model_shadow_filter_report.py",
            "--db",
            str(db_path),
            "--model",
            str(model_path),
            "--head",
            args.head,
            "--preset",
            args.preset,
            "--threshold",
            str(args.threshold),
            "--days-list",
            str(args.max_days),
            "--shift-days-list",
            "0",
            "--report-label",
            f"live_shadow_child_{args.preset}",
        ]
        child = run_child(child_cmd)
        current = child.report["window_reports"][0]
        labeled_rows = int(archive.get("labeled_rows", 0))
        filtered_trades = int(current["filtered"]["trades"])
        improved = bool(current["improved"])
        lift_pp = float(current["win_rate_lift_pp"])

        baseline_trades = int(current["baseline"]["trades"])
        if baseline_trades == 0:
            verdict = "no_matching_live_rows"
            summary_line = (
                "The live exact archive has resolved rows, but none of them match this shadow lane yet, "
                "so there is nothing honest to compare."
            )
            exit_code = 0
        elif baseline_trades < args.min_labeled_rows:
            verdict = "insufficient_live_labels"
            summary_line = (
                "The live exact archive is working, but there are not enough resolved rows yet "
                "to trust the shadow comparison."
            )
            exit_code = 0
        elif improved and filtered_trades >= 3:
            verdict = "live_shadow_promising"
            summary_line = (
                "On the resolved live exact rows we have so far, the shadow filter is improving "
                "the broader specialist lane enough to keep tracking seriously."
            )
            exit_code = 0
        elif filtered_trades == 0:
            verdict = "live_shadow_no_coverage"
            summary_line = (
                "The model threshold is too selective on the current live archive and is not "
                "selecting enough trades to judge."
            )
            exit_code = 0
        elif lift_pp >= 0.0:
            verdict = "live_shadow_mixed"
            summary_line = (
                "The shadow filter is not clearly harmful on the resolved live rows so far, but "
                "it is not yet improving enough with enough coverage to promote."
            )
            exit_code = 0
        else:
            verdict = "live_shadow_not_helping"
            summary_line = (
                "On the resolved live exact rows collected so far, the shadow filter is reducing "
                "coverage without improving realized directional quality."
            )
            exit_code = 2

        report["shadow_filter"] = child.report
        report["commands"] = {"shadow_filter": child.command}
        report["overall"] = {
            "verdict": verdict,
            "baseline_trades": baseline_trades,
            "filtered_trades": filtered_trades,
            "baseline_win_rate": float(current["baseline"]["win_rate"]),
            "filtered_win_rate": float(current["filtered"]["win_rate"]),
            "win_rate_lift_pp": lift_pp,
            "coverage": float(current["coverage"]),
            "summary_line": summary_line,
        }
        report["exit_code"] = exit_code

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
    return int(report.get("exit_code", 0))


if __name__ == "__main__":
    raise SystemExit(main())
