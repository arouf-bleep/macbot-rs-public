#!/usr/bin/env python3
"""Score shadow-only BUY_NO dampener candidates on labeled exact-input rows."""

from __future__ import annotations

import argparse
import json
import sqlite3
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable


ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class Candidate:
    label: str
    description: str
    block: Callable[[sqlite3.Row], bool]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Evaluate shadow-only BUY_NO guard candidates")
    parser.add_argument("--db", default="trades.db", help="SQLite DB path")
    parser.add_argument(
        "--output-dir",
        default="artifacts/trade-review-reports",
        help="Directory for JSON/Markdown artifacts",
    )
    return parser.parse_args()


def pct(part: int, total: int) -> float:
    return (part / total) * 100.0 if total else 0.0


def hour_utc(ts: str) -> int | None:
    try:
        return datetime.fromisoformat(ts.replace("Z", "+00:00")).astimezone(timezone.utc).hour
    except ValueError:
        return None


def load_rows(db_path: Path) -> list[sqlite3.Row]:
    if not db_path.is_file():
        raise FileNotFoundError(db_path)
    conn = sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True, timeout=30.0)
    conn.row_factory = sqlite3.Row
    rows = conn.execute(
        """
        SELECT
            timestamp,
            market_id,
            run_id,
            signal,
            regime_label,
            phase,
            fusion_score,
            p_up,
            outcome
        FROM ml_model_inputs
        WHERE outcome IS NOT NULL
          AND signal = 'BUY_NO'
          AND outcome IN (0,1)
          AND p_up BETWEEN 0 AND 1
          AND COALESCE(run_id,'') NOT GLOB 'reconstructed:*'
          AND COALESCE(run_id,'') NOT GLOB 'backfill_*'
        ORDER BY timestamp
        """
    ).fetchall()
    conn.close()
    return rows


def wrong(row: sqlite3.Row) -> bool:
    return float(row["outcome"]) >= 0.5


def kept_wrong_rate(rows: list[sqlite3.Row]) -> float | None:
    if not rows:
        return None
    return pct(sum(1 for row in rows if wrong(row)), len(rows))


def summarize_candidate(candidate: Candidate, rows: list[sqlite3.Row]) -> dict[str, Any]:
    blocked = [row for row in rows if candidate.block(row)]
    kept = [row for row in rows if not candidate.block(row)]
    blocked_wrong = [row for row in blocked if wrong(row)]
    blocked_right = [row for row in blocked if not wrong(row)]
    kept_wrong = [row for row in kept if wrong(row)]
    regime_counter = Counter(str(row["regime_label"] or "unknown") for row in blocked)
    phase_counter = Counter(str(row["phase"] or "unknown") for row in blocked)

    return {
        "label": candidate.label,
        "description": candidate.description,
        "blocked_rows": len(blocked),
        "blocked_pct": pct(len(blocked), len(rows)),
        "blocked_wrong_rows": len(blocked_wrong),
        "blocked_wrong_pct_of_blocked": pct(len(blocked_wrong), len(blocked)),
        "blocked_right_rows": len(blocked_right),
        "kept_rows": len(kept),
        "kept_wrong_rows": len(kept_wrong),
        "kept_wrong_rate_pct": kept_wrong_rate(kept),
        "baseline_wrong_rate_pct": kept_wrong_rate(rows),
        "wrong_rows_removed_pct": pct(len(blocked_wrong), sum(1 for row in rows if wrong(row))),
        "regimes_blocked": regime_counter,
        "phases_blocked": phase_counter,
        "sample_blocked_rows": [
            {
                "timestamp": row["timestamp"],
                "market_id": row["market_id"],
                "run_id": row["run_id"],
                "regime": row["regime_label"],
                "phase": row["phase"],
                "p_up": row["p_up"],
                "fusion_score": row["fusion_score"],
                "outcome": row["outcome"],
                "hour_utc": hour_utc(str(row["timestamp"])),
                "was_wrong": wrong(row),
            }
            for row in blocked[:5]
        ],
    }


def build_candidates() -> list[Candidate]:
    return [
        Candidate(
            label="block_all_trend_up_buy_no",
            description="Block all BUY_NO trades in TREND_UP.",
            block=lambda row: str(row["regime_label"] or "") == "TREND_UP",
        ),
        Candidate(
            label="block_trend_up_buy_no_unless_strong",
            description="Block BUY_NO in TREND_UP unless p_up <= 0.35.",
            block=lambda row: str(row["regime_label"] or "") == "TREND_UP"
            and float(row["p_up"]) > 0.35,
        ),
        Candidate(
            label="block_squeeze_early_mid_buy_no_unless_strong",
            description="Block BUY_NO in SQUEEZE EARLY/MID unless p_up <= 0.40.",
            block=lambda row: str(row["regime_label"] or "") == "SQUEEZE"
            and str(row["phase"] or "") in {"EARLY", "MID"}
            and float(row["p_up"]) > 0.40,
        ),
        Candidate(
            label="combined_trend_up_and_squeeze_guard",
            description="Block BUY_NO in TREND_UP unless p_up <= 0.35, plus SQUEEZE EARLY/MID unless p_up <= 0.40.",
            block=lambda row: (
                str(row["regime_label"] or "") == "TREND_UP"
                and float(row["p_up"]) > 0.35
            )
            or (
                str(row["regime_label"] or "") == "SQUEEZE"
                and str(row["phase"] or "") in {"EARLY", "MID"}
                and float(row["p_up"]) > 0.40
            ),
        ),
    ]


def percent_text(value: float | None) -> str:
    return "unavailable" if value is None else f"{value:.1f}%"


def markdown_report(report: dict[str, Any]) -> str:
    lines = [
        "# BUY_NO Shadow Guard Report",
        "",
        f"- DB: `{report['db']}`",
        f"- BUY_NO labeled rows: `{report['buy_no_rows']}`",
        f"- Baseline wrong rate: `{percent_text(report['baseline_wrong_rate_pct'])}`",
        "",
        "## Candidates",
        "",
    ]
    for candidate in report["candidates"]:
        lines.extend(
            [
                f"### {candidate['label']}",
                "",
                candidate["description"],
                "",
                f"- blocked: `{candidate['blocked_rows']}` (`{candidate['blocked_pct']:.1f}%`)",
                f"- blocked wrong: `{candidate['blocked_wrong_rows']}` (`{candidate['blocked_wrong_pct_of_blocked']:.1f}%` of blocked)",
                f"- kept rows: `{candidate['kept_rows']}`",
                f"- kept wrong rate: `{percent_text(candidate['kept_wrong_rate_pct'])}`",
                f"- wrong rows removed: `{candidate['wrong_rows_removed_pct']:.1f}%`",
                "",
            ]
        )
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    db_path = (ROOT / args.db).resolve()
    rows = load_rows(db_path)

    report: dict[str, Any] = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "db": str(db_path),
        "buy_no_rows": len(rows),
        "baseline_wrong_rate_pct": kept_wrong_rate(rows),
        "candidates": [summarize_candidate(candidate, rows) for candidate in build_candidates()],
    }

    outdir = (ROOT / args.output_dir).resolve()
    outdir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_buy_no_shadow_guard_report.json"
    md_path = outdir / f"{stamp}_buy_no_shadow_guard_report.md"
    latest_json = outdir / "latest_buy_no_shadow_guard_report.json"
    latest_md = outdir / "latest_buy_no_shadow_guard_report.md"

    json_blob = json.dumps(report, indent=2, default=lambda value: dict(value))
    md_blob = markdown_report(report)

    json_path.write_text(json_blob + "\n", encoding="utf-8")
    md_path.write_text(md_blob, encoding="utf-8")
    latest_json.write_text(json_blob + "\n", encoding="utf-8")
    latest_md.write_text(md_blob, encoding="utf-8")

    print(json.dumps({"json": str(json_path), "md": str(md_path)}, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
