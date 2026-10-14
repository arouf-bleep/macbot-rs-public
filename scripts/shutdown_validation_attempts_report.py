#!/usr/bin/env python3
"""Summarize recent shutdown-validation attempts and classify them."""

from __future__ import annotations

import argparse
import json
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from proof_bundle_index import shutdown_evidence_class


ROOT = Path(__file__).resolve().parent.parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Summarize recent shutdown-validation attempts"
    )
    parser.add_argument(
        "--maker-report-dir",
        default="artifacts/maker-soak-reports",
        help="Directory containing maker soak reports",
    )
    parser.add_argument(
        "--sim-report-dir",
        default="artifacts/sim-price-verification-reports",
        help="Directory containing sim verification reports",
    )
    parser.add_argument(
        "--integrity-report-dir",
        default="artifacts/validation-integrity-reports",
        help="Directory containing lifecycle integrity reports",
    )
    parser.add_argument(
        "--validation-bundle-dir",
        default="artifacts/validation-bundles",
        help="Directory containing final validation bundle JSON artifacts",
    )
    parser.add_argument(
        "--output-dir",
        default="artifacts/shutdown-validation-attempts",
        help="Directory for JSON/Markdown summary artifacts",
    )
    parser.add_argument(
        "--limit",
        type=int,
        default=12,
        help="Maximum number of recent shutdown-validation attempts to include",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict[str, Any] | None:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        return None


def report_timestamp(path: Path, payload: dict[str, Any] | None) -> float:
    if isinstance(payload, dict):
        raw = payload.get("generated_at") or payload.get("generated_at_utc")
        if raw:
            try:
                return datetime.fromisoformat(str(raw).replace("Z", "+00:00")).timestamp()
            except ValueError:
                pass
    try:
        return path.stat().st_mtime
    except OSError:
        return 0.0


def db_name(payload: dict[str, Any] | None) -> str | None:
    if not isinstance(payload, dict):
        return None
    raw = payload.get("db_path") or payload.get("db")
    if not raw:
        return None
    return str((ROOT / Path(str(raw)).expanduser()).resolve())


def latest_by_db(paths: list[Path]) -> dict[str, tuple[Path, dict[str, Any]]]:
    latest: dict[str, tuple[Path, dict[str, Any]]] = {}
    for path in paths:
        payload = load_json(path)
        if not isinstance(payload, dict):
            continue
        key = db_name(payload)
        if not key or "shutdown_validation" not in key:
            continue
        current = latest.get(key)
        if current is None or report_timestamp(path, payload) >= report_timestamp(current[0], current[1]):
            latest[key] = (path, payload)
    return latest


def load_reports(report_dir: Path, suffix: str) -> dict[str, tuple[Path, dict[str, Any]]]:
    paths = sorted(report_dir.glob(f"*{suffix}"))
    by_db = latest_by_db(paths)
    return by_db


def lookup_by_db(
    report_map: dict[str, tuple[Path, dict[str, Any]]],
    key: str | None,
) -> tuple[Path, dict[str, Any]] | None:
    if not key:
        return None
    return report_map.get(key)


def compact_blocker(payload: dict[str, Any] | None) -> dict[str, Any] | None:
    if not isinstance(payload, dict):
        return None
    out: dict[str, Any] = {}
    for key in ("label", "source", "note"):
        value = payload.get(key)
        if value is not None:
            out[key] = value
    evidence = payload.get("evidence")
    if evidence is not None:
        out["evidence"] = evidence
    return out or None


def classify_attempt(
    maker_report: dict[str, Any],
    sim_report: dict[str, Any] | None,
    integrity_report: dict[str, Any] | None,
    validation_bundle: dict[str, Any] | None,
) -> dict[str, Any]:
    summary = maker_report.get("summary") or {}
    run_classification = maker_report.get("run_classification") or {}
    primary_blocker = maker_report.get("primary_blocker_summary") or {}
    run_context = maker_report.get("run_context") or {}
    maker_l2 = run_context.get("maker_l2") or {}
    maker_inventory = run_context.get("maker_inventory") or {}

    trade_count = int(summary.get("trade_count") or 0)
    open_trade_count = int(summary.get("open_trade_count") or 0)
    closed_trade_count = int(summary.get("closed_trade_count") or 0)
    fill_count = int(summary.get("fill_count") or 0)
    cleanup_trade_count = 0
    integrity_status = None
    integrity_verdict = None
    integrity_reason = None
    if isinstance(integrity_report, dict):
        integrity_status = integrity_report.get("status")
        integrity_verdict = integrity_report.get("verdict")
        integrity_reason = integrity_report.get("reason")
        cleanup_trade_count = int(
            ((integrity_report.get("counts") or {}).get("trades") or {}).get("cleanup") or 0
        )
    peak_active_trade_count = int((validation_bundle or {}).get("peak_active_trade_count") or 0)
    stop_on_open_target_reached = bool(
        (validation_bundle or {}).get("stop_on_open_target_reached")
    )
    run_exit_cause = str((validation_bundle or {}).get("run_exit_cause") or "")

    sim_summary = (sim_report or {}).get("summary") or {}
    sim_plausible = int(sim_summary.get("plausible_execution_rows") or 0)
    sim_implausible = int(sim_summary.get("implausible_execution_rows") or 0)
    sim_cleanup = int(sim_summary.get("cleanup_mark_rows") or 0)
    sim_total = int(sim_summary.get("total_fill_rows") or 0)

    run_label = str(run_classification.get("label") or "")
    blocker_label = str(primary_blocker.get("label") or "")
    l2_reason = str(maker_l2.get("l2_stale_reason") or "")
    market_count = int(maker_l2.get("market_count") or 0)
    quoteable_count = int(maker_l2.get("quoteable_count") or 0)

    if (
        shutdown_evidence_class(sim_report or {}, integrity_report or {}) is None
        and sim_plausible > 0
        and integrity_status == "ok"
        and integrity_verdict == "ok"
        and cleanup_trade_count > 0
        and trade_count > 0
        and open_trade_count == 0
        and sim_implausible == 0
    ):
        proof_detail = (
            "multi_open_cleanup"
            if stop_on_open_target_reached or peak_active_trade_count >= 2
            else "single_open_cleanup"
        )
        return {
            "class": "proof",
            "detail": proof_detail,
            "reason": "integrity passed and shutdown cleanup evidence exists",
        }

    if (
        run_label == "dead_window"
        or blocker_label == "no_target_markets"
        or (trade_count == 0 and market_count == 0 and quoteable_count == 0)
        or "no target markets" in l2_reason.lower()
    ):
        return {
            "class": "starvation",
            "detail": blocker_label or run_label or "dead_window",
            "reason": "no active market path exercised the shutdown rail",
        }

    if (
        sim_cleanup > 0
        and trade_count > 0
        and open_trade_count == 0
        and sim_implausible == 0
        and integrity_status != "ok"
    ):
        detail = (
            "multi_open_cleanup_without_integrity"
            if stop_on_open_target_reached or peak_active_trade_count >= 2
            else "cleanup_evidence_only_without_integrity"
        )
        return {
            "class": "other",
            "detail": detail,
            "reason": "shutdown cleanup evidence exists, but the modern integrity proof bundle is missing",
        }

    if trade_count > 0 and open_trade_count == 0 and cleanup_trade_count > 0 and sim_implausible == 0:
        return {
            "class": "other",
            "detail": "cleanup_evidence_only",
            "reason": "shutdown cleanup evidence exists, but the multi-open target was not explicitly recorded",
        }

    return {
        "class": "other",
        "detail": blocker_label or run_label or run_exit_cause or "unclassified",
        "reason": "the attempt exercised plumbing but did not prove shutdown cleanup or dead-window starvation",
    }


def summarize_attempts(
    maker_report_dir: Path,
    sim_report_dir: Path,
    integrity_report_dir: Path,
    validation_bundle_dir: Path,
    limit: int,
) -> dict[str, Any]:
    maker_reports = load_reports(maker_report_dir, "_maker_soak_report.json")
    sim_reports = load_reports(sim_report_dir, "_sim_price_verification_report.json")
    integrity_reports = load_reports(integrity_report_dir, "_validation_db_integrity_report.json")
    validation_bundles = load_reports(validation_bundle_dir, "_validation_bundle.json")

    attempts: list[dict[str, Any]] = []
    for db_key, (maker_path, maker_payload) in maker_reports.items():
        summary = maker_payload.get("summary") or {}
        run_classification = maker_payload.get("run_classification") or {}
        primary_blocker = maker_payload.get("primary_blocker_summary") or {}
        if "shutdown_validation" not in str(maker_payload.get("db_path") or maker_payload.get("db") or ""):
            continue

        sim_match = lookup_by_db(sim_reports, db_key)
        integrity_match = lookup_by_db(integrity_reports, db_key)
        bundle_match = lookup_by_db(validation_bundles, db_key)
        sim_payload = sim_match[1] if sim_match else None
        integrity_payload = integrity_match[1] if integrity_match else None
        bundle_payload = bundle_match[1] if bundle_match else None
        peak_active_trade_count = int((bundle_payload or {}).get("peak_active_trade_count") or 0)
        stop_on_open_target_reached = bool(
            (bundle_payload or {}).get("stop_on_open_target_reached")
        )
        run_exit_cause = (bundle_payload or {}).get("run_exit_cause")
        classification = classify_attempt(
            maker_payload,
            sim_payload,
            integrity_payload,
            bundle_payload,
        )

        attempts.append(
            {
                "db": str(maker_payload.get("db_path") or maker_payload.get("db") or ""),
                "maker_report": str(maker_path),
                "sim_report": str(sim_match[0]) if sim_match else None,
                "integrity_report": str(integrity_match[0]) if integrity_match else None,
                "validation_bundle": str(bundle_match[0]) if bundle_match else None,
                "generated_at": maker_payload.get("generated_at"),
                "trade_count": int(summary.get("trade_count") or 0),
                "open_trade_count": int(summary.get("open_trade_count") or 0),
                "closed_trade_count": int(summary.get("closed_trade_count") or 0),
                "fill_count": int(summary.get("fill_count") or 0),
                "peak_active_trade_count": peak_active_trade_count,
                "stop_on_open_target_reached": stop_on_open_target_reached,
                "run_exit_cause": run_exit_cause,
                "run_classification": run_classification,
                "primary_blocker_summary": compact_blocker(primary_blocker),
                "integrity_status": (integrity_payload or {}).get("status"),
                "integrity_verdict": (integrity_payload or {}).get("verdict"),
                "integrity_reason": (integrity_payload or {}).get("reason"),
                "classification": classification,
            }
        )

    attempts.sort(key=lambda item: str(item.get("generated_at") or ""), reverse=True)
    attempts = attempts[: max(0, limit)]

    counts = Counter(item["classification"]["class"] for item in attempts)
    detail_counts = Counter(
        f"{item['classification']['class']}:{item['classification'].get('detail')}"
        for item in attempts
    )
    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "limit": limit,
        "attempt_count": len(attempts),
        "classification_counts": dict(counts),
        "detail_counts_top": [
            {"label": label, "count": count}
            for label, count in detail_counts.most_common(10)
        ],
        "attempts": attempts,
    }


def render_markdown(report: dict[str, Any]) -> str:
    lines = [
        "# Shutdown Validation Attempts",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- Attempts included: `{report['attempt_count']}`",
        "",
        "## Classification Counts",
    ]
    if report.get("classification_counts"):
        for label, count in sorted(report["classification_counts"].items()):
            lines.append(f"- `{label}`: `{count}`")
    else:
        lines.append("- None")
    lines.extend(["", "## Recent Attempts"])
    attempts = report.get("attempts") or []
    if not attempts:
        lines.append("- None")
    else:
        for attempt in attempts:
            cls = attempt.get("classification") or {}
            lines.extend(
                [
                    f"- `{Path(str(attempt.get('db') or '')).name}`",
                    f"  - class: `{cls.get('class')}`",
                    f"  - detail: `{cls.get('detail')}`",
                    f"  - reason: `{cls.get('reason')}`",
                    f"  - trades/open/closed: `{attempt.get('trade_count')}` / `{attempt.get('open_trade_count')}` / `{attempt.get('closed_trade_count')}`",
                    f"  - fill_count: `{attempt.get('fill_count')}`",
                    f"  - integrity: `{attempt.get('integrity_status')}` / `{attempt.get('integrity_verdict')}` / `{attempt.get('integrity_reason')}`",
                    f"  - run_classification: `{(attempt.get('run_classification') or {}).get('label')}`",
                    f"  - primary_blocker: `{(attempt.get('primary_blocker_summary') or {}).get('label')}`",
                ]
            )
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    maker_report_dir = (ROOT / args.maker_report_dir).resolve()
    sim_report_dir = (ROOT / args.sim_report_dir).resolve()
    integrity_report_dir = (ROOT / args.integrity_report_dir).resolve()
    validation_bundle_dir = (ROOT / args.validation_bundle_dir).resolve()
    output_dir = (ROOT / args.output_dir).resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    report = summarize_attempts(
        maker_report_dir,
        sim_report_dir,
        integrity_report_dir,
        validation_bundle_dir,
        args.limit,
    )

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = output_dir / f"{stamp}_shutdown_validation_attempts_report.json"
    md_path = output_dir / f"{stamp}_shutdown_validation_attempts_report.md"
    latest_json = output_dir / "latest_shutdown_validation_attempts_report.json"
    latest_md = output_dir / "latest_shutdown_validation_attempts_report.md"

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
