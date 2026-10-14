#!/usr/bin/env python3
"""Summarize recent validation/control artifacts for retention and hygiene."""

from __future__ import annotations

import argparse
import json
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Summarize validation/control artifact hygiene")
    parser.add_argument(
        "--bundle-dir",
        default="artifacts/validation-control-bundles",
        help="Directory containing validation control bundle artifacts",
    )
    parser.add_argument(
        "--maker-report-dir",
        default="artifacts/maker-soak-reports",
        help="Directory containing maker soak reports",
    )
    parser.add_argument(
        "--output-dir",
        default="artifacts/validation-artifact-hygiene",
        help="Directory for JSON/Markdown summary artifacts",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=12,
        help="How many recent bundles to keep in the hygiene summary",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any] | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def report_timestamp(path: Path, payload: dict[str, Any] | None) -> float:
    if isinstance(payload, dict):
        raw = payload.get("generated_at")
        if raw:
            try:
                return datetime.fromisoformat(str(raw).replace("Z", "+00:00")).timestamp()
            except ValueError:
                pass
    try:
        return path.stat().st_mtime
    except OSError:
        return 0.0


def classify_bundle(payload: dict[str, Any]) -> str:
    terminal = str(payload.get("terminal_class") or "")
    run_label = str((payload.get("run_classification") or {}).get("label") or "")
    blocker_label = str((payload.get("primary_blocker_summary") or {}).get("label") or "")
    no_trade_bucket = str((payload.get("no_trade_diagnosis") or {}).get("bucket") or "")
    no_trade_label = str((payload.get("no_trade_diagnosis") or {}).get("label") or "")
    if terminal == "success":
        return "proof"
    if terminal == "lifecycle_bug":
        return "lifecycle_bug"
    if (
        run_label == "dead_window"
        or blocker_label == "no_target_markets"
        or no_trade_bucket == "dead_window"
        or no_trade_label == "no_target_markets"
    ):
        return "starvation"
    if (
        run_label in {"policy_limited", "market_quality_limited", "stale_feed_limited", "one_sided_book_pressure"}
        or no_trade_bucket == "low_quoteability"
        or no_trade_label in {"one_sided_books", "stale_books", "snapshot_missing", "low_quoteability"}
        or terminal == "low_quoteability"
    ):
        return "low_activity"
    if run_label == "active":
        return "active"
    return "other"


def summarize_bundles(bundle_dir: Path, maker_report_dir: Path, limit: int) -> dict[str, Any]:
    if limit < 0:
        raise ValueError("limit must be nonnegative")
    bundle_paths = sorted((p for p in bundle_dir.glob("*_validation_control_bundle.json") if not p.name.startswith("latest_")),
                          key=lambda p: report_timestamp(p, load_json(p)))
    maker_paths = sorted((p for p in maker_report_dir.glob("*_maker_soak_report.json") if not p.name.startswith("latest_")),
                         key=lambda p: report_timestamp(p, load_json(p)))
    recent_bundle_paths = bundle_paths[-limit:] if limit else []
    recent_maker_paths = maker_paths[-limit:] if limit else []

    bundle_items: list[dict[str, Any]] = []
    for path in recent_bundle_paths:
        payload = load_json(path)
        if not isinstance(payload, dict):
            continue
        bundle_items.append(
            {
                "path": str(path),
                "db": payload.get("db"),
                "terminal_class": payload.get("terminal_class"),
                "run_classification": (payload.get("run_classification") or {}).get("label"),
                "primary_blocker": (payload.get("primary_blocker_summary") or {}).get("label"),
                "classification": classify_bundle(payload),
                "generated_at": payload.get("generated_at"),
            }
        )

    maker_counts = Counter()
    for path in recent_maker_paths:
        payload = load_json(path)
        if not isinstance(payload, dict):
            continue
        maker_counts[str((payload.get("run_classification") or {}).get("label") or "unknown")] += 1

    classification_counts = Counter(item["classification"] for item in bundle_items)
    db_counts = Counter(str(item.get("db") or "unknown") for item in bundle_items)
    repeated_db_paths = [item["db"] for item in bundle_items if db_counts[str(item.get("db") or "unknown")] > 1]

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "bundle_dir": str(bundle_dir),
        "maker_report_dir": str(maker_report_dir),
        "limit": int(limit),
        "recent_bundle_count": len(bundle_items),
        "classification_counts": dict(classification_counts),
        "maker_run_classification_counts": dict(maker_counts),
        "top_db_reuse": [
            {"db": db, "count": count}
            for db, count in db_counts.most_common(10)
        ],
        "repeated_db_paths": sorted({db for db in repeated_db_paths if db}),
        "bundles": bundle_items,
        "prune_candidates": [
            item for item in bundle_items if item["classification"] in {"starvation", "low_activity"}
        ],
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Validation Artifact Hygiene",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- Bundle dir: `{report['bundle_dir']}`",
        f"- Recent bundles considered: `{report['recent_bundle_count']}`",
        "",
        "## Classification Counts",
    ]
    counts = report.get("classification_counts") or {}
    if counts:
        for label, count in sorted(counts.items()):
            lines.append(f"- `{label}`: `{count}`")
    else:
        lines.append("- None")
    lines.extend(["", "## Top DB Reuse"])
    top_db_reuse = report.get("top_db_reuse") or []
    if top_db_reuse:
        for item in top_db_reuse:
            lines.append(f"- `{item['db']}`: `{item['count']}`")
    else:
        lines.append("- None")
    lines.extend(["", "## Prune Candidates"])
    prune_candidates = report.get("prune_candidates") or []
    if prune_candidates:
        for item in prune_candidates:
            lines.append(
                f"- `{Path(str(item.get('path') or '')).name}` => `{item.get('classification')}` / `{item.get('run_classification')}`"
            )
    else:
        lines.append("- None")
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    bundle_dir = (ROOT / args.bundle_dir).resolve()
    maker_report_dir = (ROOT / args.maker_report_dir).resolve()
    output_dir = (ROOT / args.output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    report = summarize_bundles(bundle_dir, maker_report_dir, args.limit)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = output_dir / f"{stamp}_validation_artifact_hygiene_report.json"
    md_path = output_dir / f"{stamp}_validation_artifact_hygiene_report.md"
    latest_json = output_dir / "latest_validation_artifact_hygiene_report.json"
    latest_md = output_dir / "latest_validation_artifact_hygiene_report.md"
    json_text = json.dumps(report, indent=2)
    md_text = render_markdown(report)
    json_path.write_text(json_text, encoding="utf-8")
    md_path.write_text(md_text, encoding="utf-8")
    latest_json.write_text(json_text, encoding="utf-8")
    latest_md.write_text(md_text, encoding="utf-8")
    print(json.dumps({"json": str(json_path), "md": str(md_path), "recent_bundle_count": report["recent_bundle_count"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
