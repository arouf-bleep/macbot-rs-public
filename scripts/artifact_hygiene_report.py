#!/usr/bin/env python3
"""Summarize shutdown/proof artifacts for truth-first hygiene.

This helper does not delete anything. It groups recent shutdown-validation
artifacts into:
- keep-worthy proof/control evidence
- stale or dead-window repeat attempts
- everything else that should stay readable but not drive decisions
"""

from __future__ import annotations

import argparse
import json
import subprocess
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build a truth-first artifact hygiene report")
    parser.add_argument(
        "--attempts-report",
        default="artifacts/shutdown-validation-attempts/latest_shutdown_validation_attempts_report.json",
        help="Latest shutdown-validation attempts JSON",
    )
    parser.add_argument(
        "--proof-bundle",
        default="artifacts/shutdown-validation-attempts/latest_shutdown_validation_proof_bundle.json",
        help="Latest shutdown-validation proof bundle JSON",
    )
    parser.add_argument(
        "--output-dir",
        default="artifacts/artifact-hygiene-reports",
        help="Directory for JSON/Markdown artifacts",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=12,
        help="Maximum recent attempts to include in the report",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any] | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def normalize_path(raw: str | None) -> str | None:
    if raw is None:
        return None
    return str(Path(raw).expanduser().resolve())


def compact_attempt(attempt: dict[str, Any]) -> dict[str, Any]:
    classification = attempt.get("classification") or {}
    return {
        "db": attempt.get("db"),
        "maker_report": attempt.get("maker_report"),
        "sim_report": attempt.get("sim_report"),
        "integrity_report": attempt.get("integrity_report"),
        "validation_bundle": attempt.get("validation_bundle"),
        "generated_at": attempt.get("generated_at"),
        "trade_count": int(attempt.get("trade_count") or 0),
        "open_trade_count": int(attempt.get("open_trade_count") or 0),
        "closed_trade_count": int(attempt.get("closed_trade_count") or 0),
        "fill_count": int(attempt.get("fill_count") or 0),
        "peak_active_trade_count": int(attempt.get("peak_active_trade_count") or 0),
        "stop_on_open_target_reached": bool(attempt.get("stop_on_open_target_reached")),
        "run_exit_cause": attempt.get("run_exit_cause"),
        "run_classification": attempt.get("run_classification") or {},
        "primary_blocker_summary": attempt.get("primary_blocker_summary") or {},
        "preflight_quoteability": attempt.get("preflight_quoteability") or {},
        "integrity_status": attempt.get("integrity_status"),
        "integrity_verdict": attempt.get("integrity_verdict"),
        "integrity_reason": attempt.get("integrity_reason"),
        "classification": classification,
    }


def build_report(attempts_report: dict[str, Any], proof_bundle: dict[str, Any] | None, limit: int) -> dict[str, Any]:
    attempts = attempts_report.get("attempts") or []
    recent_attempts = attempts[: max(0, limit)]

    proof_items: list[dict[str, Any]] = []
    repeat_items: list[dict[str, Any]] = []
    low_quoteability_items: list[dict[str, Any]] = []
    other_items: list[dict[str, Any]] = []

    for attempt in recent_attempts:
        classification = attempt.get("classification") or {}
        klass = str(classification.get("class") or "")
        detail = str(classification.get("detail") or "")
        compact = compact_attempt(attempt)
        if klass == "proof":
            compact["hygiene_class"] = "keep"
            compact["hygiene_reason"] = "shutdown proof evidence"
            proof_items.append(compact)
        elif klass == "starvation" and detail in {"dead_window", "no_target_markets"}:
            compact["hygiene_class"] = "repeat_candidate"
            compact["hygiene_reason"] = "dead-window repeat"
            repeat_items.append(compact)
        elif klass == "low_quoteability" or detail in {"one_sided_books", "stale_books", "snapshot_missing", "low_quoteability"}:
            compact["hygiene_class"] = "keep_for_context"
            compact["hygiene_reason"] = "low-quoteability context"
            low_quoteability_items.append(compact)
        else:
            compact["hygiene_class"] = "keep_for_context"
            compact["hygiene_reason"] = "operator context"
            other_items.append(compact)

    repeat_groups = Counter(item["classification"].get("detail") or "unknown" for item in repeat_items)
    proof_groups = Counter(
        "multi_open" if item.get("peak_active_trade_count", 0) >= 2 else "single_open"
        for item in proof_items
    )

    keep_paths = []
    for item in proof_items:
        keep_paths.extend(
            [
                item.get("maker_report"),
                item.get("sim_report"),
                item.get("integrity_report"),
                item.get("validation_bundle"),
            ]
        )
    keep_paths = [path for path in dict.fromkeys(p for p in keep_paths if p)]

    latest_bundle = proof_bundle or {}
    latest_bundle_path = None
    if not latest_bundle and proof_items:
        latest_proof_db = proof_items[0].get("db")
        if latest_proof_db:
            try:
                proc = subprocess.run(
                    ["python3", str(ROOT / "scripts" / "proof_bundle_index.py"), "--db", str(latest_proof_db)],
                    cwd=ROOT,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                latest_bundle = json.loads(proc.stdout.strip() or "{}")
                latest_bundle_path = latest_bundle.get("validation_bundle_path")
            except Exception:
                latest_bundle = {}
    latest_bundle_summary = {
        "terminal_class": latest_bundle.get("terminal_class"),
        "run_classification": latest_bundle.get("run_classification") or {},
        "primary_blocker_summary": latest_bundle.get("primary_blocker_summary") or {},
        "shutdown_summary": latest_bundle.get("shutdown_summary") or {},
    }

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "attempt_count": int(attempts_report.get("attempt_count") or len(attempts)),
        "included_attempt_count": len(recent_attempts),
        "proof_count": len(proof_items),
        "repeat_candidate_count": len(repeat_items),
        "low_quoteability_count": len(low_quoteability_items),
        "other_count": len(other_items),
        "repeat_detail_top": [
            {"label": label, "count": count}
            for label, count in repeat_groups.most_common(10)
        ],
        "proof_detail_top": [
            {"label": label, "count": count}
            for label, count in proof_groups.most_common(10)
        ],
        "keep_paths": keep_paths,
        "proof_items": proof_items,
        "repeat_items": repeat_items,
        "low_quoteability_items": low_quoteability_items,
        "other_items": other_items,
        "proof_bundle_path": latest_bundle_path or (proof_bundle or {}).get("validation_bundle_path"),
        "latest_bundle_summary": latest_bundle_summary,
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Artifact Hygiene Report",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- Total attempts: `{report['attempt_count']}`",
        f"- Attempts included: `{report['included_attempt_count']}`",
        f"- Proof items: `{report['proof_count']}`",
        f"- Repeat candidates: `{report['repeat_candidate_count']}`",
        f"- Low-quoteability items: `{report['low_quoteability_count']}`",
        f"- Other items: `{report['other_count']}`",
        f"- Proof bundle path: `{report.get('proof_bundle_path')}`",
        "",
        "## Keep Paths",
    ]
    keep_paths = report.get("keep_paths") or []
    if keep_paths:
        for path in keep_paths:
            lines.append(f"- `{path}`")
    else:
        lines.append("- None")

    lines.extend(["", "## Repeat Candidates"])
    repeat_items = report.get("repeat_items") or []
    if repeat_items:
        for item in repeat_items:
            cls = item.get("classification") or {}
            lines.extend(
                [
                    f"- `{Path(str(item.get('db') or '')).name}`",
                    f"  - class: `{cls.get('class')}`",
                    f"  - detail: `{cls.get('detail')}`",
                    f"  - reason: `{item.get('hygiene_reason')}`",
                    f"  - run_exit_cause: `{item.get('run_exit_cause')}`",
                ]
            )
    else:
        lines.append("- None")

    lines.extend(["", "## Low Quoteability Context"])
    low_quoteability_items = report.get("low_quoteability_items") or []
    if low_quoteability_items:
        for item in low_quoteability_items:
            cls = item.get("classification") or {}
            lines.extend(
                [
                    f"- `{Path(str(item.get('db') or '')).name}`",
                    f"  - class: `{cls.get('class')}`",
                    f"  - detail: `{cls.get('detail')}`",
                    f"  - reason: `{item.get('hygiene_reason')}`",
                    f"  - preflight_quoteability: `{(item.get('preflight_quoteability') or {}).get('label')}`",
                ]
            )
    else:
        lines.append("- None")

    lines.extend(["", "## Proof Items"])
    proof_items = report.get("proof_items") or []
    if proof_items:
        for item in proof_items:
            lines.extend(
                [
                    f"- `{Path(str(item.get('db') or '')).name}`",
                    f"  - open_trade_count: `{item.get('open_trade_count')}`",
                    f"  - peak_active_trade_count: `{item.get('peak_active_trade_count')}`",
                    f"  - stop_on_open_target_reached: `{item.get('stop_on_open_target_reached')}`",
                    f"  - integrity_verdict: `{item.get('integrity_verdict')}`",
                ]
            )
    else:
        lines.append("- None")

    lines.extend(["", "## Latest Bundle Summary"])
    bundle = report.get("latest_bundle_summary") or {}
    shutdown = bundle.get("shutdown_summary") or {}
    lines.extend(
        [
            f"- terminal_class: `{bundle.get('terminal_class')}`",
            f"- run_classification: `{(bundle.get('run_classification') or {}).get('label')}`",
            f"- primary_blocker: `{(bundle.get('primary_blocker_summary') or {}).get('label')}`",
            f"- cleanup_trade_count: `{shutdown.get('cleanup_trade_count')}`",
            f"- integrity_verdict: `{shutdown.get('integrity_verdict')}`",
        ]
    )
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    attempts_path = (ROOT / args.attempts_report).resolve()
    proof_bundle_path = (ROOT / args.proof_bundle).resolve()
    output_dir = (ROOT / args.output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    attempts_report = load_json(attempts_path)
    if attempts_report is None:
        raise SystemExit(f"unable to load attempts report: {attempts_path}")
    proof_bundle = load_json(proof_bundle_path)
    report = build_report(attempts_report, proof_bundle, args.limit)

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = output_dir / f"{stamp}_artifact_hygiene_report.json"
    md_path = output_dir / f"{stamp}_artifact_hygiene_report.md"
    latest_json = output_dir / "latest_artifact_hygiene_report.json"
    latest_md = output_dir / "latest_artifact_hygiene_report.md"

    json_text = json.dumps(report, indent=2)
    md_text = render_markdown(report)
    json_path.write_text(json_text, encoding="utf-8")
    md_path.write_text(md_text, encoding="utf-8")
    latest_json.write_text(json_text, encoding="utf-8")
    latest_md.write_text(md_text, encoding="utf-8")

    print(json_text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
