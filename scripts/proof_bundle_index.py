#!/usr/bin/env python3
"""Resolve the latest shutdown-validation bundle and print a compact JSON index."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def parse_args() -> argparse.Namespace:
    default_artifacts_root = Path(__file__).resolve().parent.parent / "artifacts"
    parser = argparse.ArgumentParser(description="Resolve the latest shutdown-validation bundle")
    parser.add_argument(
        "--db",
        default=None,
        help="Optional SQLite DB path. If omitted, resolve the latest shutdown-validation artifact set.",
    )
    parser.add_argument(
        "--artifacts-root",
        default=str(default_artifacts_root),
        help="Artifacts root directory",
    )
    return parser.parse_args()


def normalize_path(raw: str | Path | None) -> str | None:
    if raw is None:
        return None
    return str(Path(raw).expanduser().resolve())


def read_json(path: Path) -> dict[str, Any] | None:
    try:
        payload = json.loads(path.read_text(encoding="utf-8"))
        return payload if isinstance(payload, dict) else None
    except Exception:
        return None


def matches_db(payload: dict[str, Any], db_path: str) -> bool:
    candidate = payload.get("db_path") or payload.get("db")
    if candidate is None:
        return False
    return normalize_path(candidate) == db_path


def newest_matching_json(
    directory: Path,
    pattern: str,
    *,
    db_path: str | None = None,
    prefer_latest_file: str | None = None,
) -> tuple[Path, dict[str, Any]] | None:
    latest_path = directory / prefer_latest_file if prefer_latest_file else None
    if db_path is None and latest_path is not None and latest_path.exists():
        payload = read_json(latest_path)
        if payload is not None:
            return latest_path, payload

    matches: list[tuple[float, Path, dict[str, Any]]] = []
    for path in directory.glob(pattern):
        if path.name.startswith("latest_"):
            continue
        payload = read_json(path)
        if payload is None:
            continue
        if db_path is not None and not matches_db(payload, db_path):
            continue
        matches.append((path.stat().st_mtime, path, payload))

    if not matches:
        if db_path is None and latest_path is not None and latest_path.exists():
            payload = read_json(latest_path)
            if payload is not None:
                return latest_path, payload
        return None

    matches.sort(key=lambda item: item[0])
    _, path, payload = matches[-1]
    return path, payload


def resolve_bundle(artifacts_root: Path, db_arg: str | None) -> dict[str, Any]:
    if db_arg is not None:
        db_path = normalize_path(db_arg)
        resolved_from = "db"
    else:
        db_path = None
        resolved_from = "latest_artifact_set"

    maker_dir = artifacts_root / "maker-soak-reports"
    sim_dir = artifacts_root / "sim-price-verification-reports"
    integrity_dir = artifacts_root / "validation-integrity-reports"
    bundle_dir = artifacts_root / "validation-bundles"

    soak_match = newest_matching_json(
        maker_dir,
        "*_maker_soak_report.json",
        db_path=db_path,
        prefer_latest_file="latest_maker_soak_report.json",
    )
    if soak_match is None:
        raise SystemExit(f"unable to resolve maker soak report in {maker_dir}")
    soak_path, soak_report = soak_match

    resolved_db = db_path or normalize_path(soak_report.get("db_path") or soak_report.get("db"))
    if resolved_db is None:
        raise SystemExit("maker soak report did not include a db path")

    sim_match = newest_matching_json(
        sim_dir,
        "*_sim_price_verification_report.json",
        db_path=resolved_db,
        prefer_latest_file="latest_sim_price_verification_report.json",
    )
    integrity_match = newest_matching_json(
        integrity_dir,
        "*_validation_db_integrity_report.json",
        db_path=resolved_db,
        prefer_latest_file="latest_validation_db_integrity_report.json",
    )
    bundle_match = newest_matching_json(
        bundle_dir,
        "*_validation_bundle.json",
        db_path=resolved_db,
        prefer_latest_file="latest_validation_bundle.json",
    )

    if sim_match is None:
        raise SystemExit(f"unable to resolve sim verification report for db {resolved_db}")
    if integrity_match is None:
        raise SystemExit(f"unable to resolve integrity report for db {resolved_db}")

    sim_path, sim_report = sim_match
    integrity_path, integrity_report = integrity_match
    bundle_path, bundle_report = bundle_match if bundle_match is not None else (None, {})

    soak_summary = soak_report.get("summary") or {}
    run_classification = soak_report.get("run_classification") or {}
    primary_blocker = soak_report.get("primary_blocker_summary") or {}
    no_trade_diagnosis = soak_report.get("no_trade_diagnosis") or {}
    run_context = soak_report.get("run_context") or {}
    maker_l2 = (run_context.get("maker_l2") or {})
    sim_summary = sim_report.get("summary") or {}
    integrity_counts = integrity_report.get("counts") or {}
    integrity_trade_counts = integrity_report.get("trade_counts") or {}
    integrity_fill_counts = integrity_report.get("fill_integrity") or {}

    derived_class = derive_terminal_class(
            soak_report=soak_report,
            sim_report=sim_report,
            integrity_report=integrity_report,
        )
    terminal_class = derived_class
    if derived_class not in {"lifecycle_bug", "market_quality_failure", "evidence_incomplete"}:
        terminal_class = str((bundle_report or {}).get("terminal_class") or derived_class)

    shutdown_summary = {
        "trade_count": int(soak_summary.get("trade_count") or 0),
        "open_trade_count": int(integrity_trade_counts.get("open_count") or 0),
        "closing_trade_count": int(integrity_trade_counts.get("closing_count") or 0),
        "cleanup_trade_count": int(integrity_trade_counts.get("cleanup_trade_count") or 0),
        "cleanup_trade_ids": integrity_trade_counts.get("cleanup_trade_ids") or [],
        "cleanup_mark_rows": int(sim_summary.get("cleanup_mark_rows") or 0),
        "execution_fill_rows": int(sim_summary.get("execution_fill_rows") or 0),
        "implausible_execution_rows": int(sim_summary.get("implausible_execution_rows") or 0),
        "plausible_execution_rows": int(sim_summary.get("plausible_execution_rows") or 0),
        "total_fill_rows": int(sim_summary.get("total_fill_rows") or 0),
        "orphan_trade_fill_count": len(integrity_fill_counts.get("orphan_trade_fill_ids") or []),
        "trade_fill_missing_order_id_count": len(
            integrity_fill_counts.get("trade_fill_missing_order_id_ids") or []
        ),
        "entry_fill_missing_outbox_count": len(
            integrity_fill_counts.get("entry_fill_missing_outbox_ids") or []
        ),
        "maker_quote_fill_opened_missing_trade_fill_count": len(
            integrity_fill_counts.get("maker_quote_fill_opened_missing_trade_fill_ids") or []
        ),
        "integrity_status": integrity_report.get("status"),
        "integrity_verdict": integrity_report.get("verdict"),
        "integrity_reason": integrity_report.get("reason"),
        "integrity_mismatch_count": int(integrity_report.get("mismatch_count") or 0),
        "integrity_failure_count": int(integrity_report.get("failure_count") or 0),
        "integrity_counts": integrity_counts,
        "peak_active_trade_count": int((bundle_report or {}).get("peak_active_trade_count") or 0),
        "stop_on_open_target_reached": bool(
            (bundle_report or {}).get("stop_on_open_target_reached")
        ),
        "run_exit_cause": (bundle_report or {}).get("run_exit_cause"),
    }
    dead_run_summary = build_dead_run_summary(
        trade_count=int(soak_summary.get("trade_count") or 0),
        run_classification=run_classification,
        primary_blocker=primary_blocker,
        no_trade_diagnosis=no_trade_diagnosis,
        maker_l2_context=maker_l2,
    )

    return {
        "bundle_kind": "shutdown_validation",
        "resolved_from": resolved_from,
        "db": resolved_db,
        "soak_report_path": str(soak_path),
        "sim_verification_report_path": str(sim_path),
        "integrity_report_path": str(integrity_path),
        "validation_bundle_path": str(bundle_path) if bundle_path is not None else None,
        "terminal_class": terminal_class,
        "run_classification": run_classification,
        "primary_blocker_summary": primary_blocker,
        "dead_run_summary": dead_run_summary,
        "shutdown_summary": shutdown_summary,
    }


def shutdown_evidence_class(sim_report: dict[str, Any], integrity_report: dict[str, Any]) -> str | None:
    """Return a blocker before any activity can be called successful."""
    if integrity_report.get("status") != "ok" or integrity_report.get("verdict") not in {None, "ok"}:
        return "lifecycle_bug"
    if integrity_report.get("mismatches"):
        return "lifecycle_bug"
    for key in ("mismatch_count", "failure_count"):
        value = integrity_report.get(key, 0)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            return "evidence_incomplete"
        if value:
            return "lifecycle_bug"
    counts = integrity_report.get("trade_counts")
    if not isinstance(counts, dict):
        return "evidence_incomplete"
    for key in ("open_count", "closing_count"):
        value = counts.get(key)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            return "evidence_incomplete"
        if value:
            return "lifecycle_bug"
    summary = sim_report.get("summary")
    if not isinstance(summary, dict):
        return "evidence_incomplete"
    for key in ("execution_fill_rows", "plausible_execution_rows", "implausible_execution_rows", "unverifiable_rows"):
        value = summary.get(key)
        if not isinstance(value, int) or isinstance(value, bool) or value < 0:
            return "evidence_incomplete"
    if summary["implausible_execution_rows"]:
        return "market_quality_failure"
    if summary["unverifiable_rows"] or summary["plausible_execution_rows"] != summary["execution_fill_rows"]:
        return "evidence_incomplete"
    return None


def derive_terminal_class(
    *,
    soak_report: dict[str, Any],
    sim_report: dict[str, Any],
    integrity_report: dict[str, Any],
) -> str:
    evidence_blocker = shutdown_evidence_class(sim_report, integrity_report)
    if evidence_blocker:
        return evidence_blocker
    integrity_status = integrity_report.get("status")
    integrity_verdict = integrity_report.get("verdict")
    integrity_mismatch_count = int(integrity_report.get("mismatch_count") or 0)
    integrity_failure_count = int(integrity_report.get("failure_count") or 0)
    if (
        integrity_status != "ok"
        or integrity_verdict not in {None, "ok"}
        or integrity_mismatch_count > 0
        or integrity_failure_count > 0
    ):
        return "lifecycle_bug"

    soak_summary = soak_report.get("summary") or {}
    trade_count = int(soak_summary.get("trade_count") or 0)
    if trade_count > 0:
        if not sim_report["summary"]["execution_fill_rows"]:
            return "evidence_incomplete"
        return "success"

    run_classification = soak_report.get("run_classification") or {}
    run_label = str(run_classification.get("label") or "").strip()
    primary_blocker = soak_report.get("primary_blocker_summary") or {}
    blocker_label = str(primary_blocker.get("label") or "").strip()
    no_trade_diagnosis = soak_report.get("no_trade_diagnosis") or {}
    no_trade_bucket = str(no_trade_diagnosis.get("bucket") or "").strip()

    if (
        run_label == "dead_window"
        or blocker_label == "no_target_markets"
        or no_trade_bucket == "dead_window"
    ):
        return "starvation/dead_window"
    if run_label == "inactive" or no_trade_bucket == "inactive":
        return "inactive/unexercised"
    if run_label in {"policy_limited"} or blocker_label in {
        "policy_suppression",
        "trade_rejections",
        "suppression_pressure",
    } or no_trade_bucket == "policy":
        return "policy_suppression"
    if run_label in {
        "market_quality_limited",
        "low_quoteability_limited",
        "one_sided_book_pressure",
        "stale_feed_limited",
    } or blocker_label in {
        "one_sided_books",
        "stale_feed",
        "stale_books",
        "snapshot_missing",
        "low_quoteability",
        "market_quality",
    } or no_trade_bucket in {"low_quoteability", "market_quality"}:
        return "market_quality_failure"

    sim_summary = sim_report.get("summary") or {}
    if int(sim_summary.get("execution_fill_rows") or 0) > 0:
        return "success"

    return "inactive/unexercised"


def build_dead_run_summary(
    *,
    trade_count: int,
    run_classification: dict[str, Any],
    primary_blocker: dict[str, Any],
    no_trade_diagnosis: dict[str, Any],
    maker_l2_context: dict[str, Any],
) -> dict[str, Any]:
    evidence = {
        "markets": int(maker_l2_context.get("market_count") or 0),
        "loaded": int(maker_l2_context.get("loaded_count") or 0),
        "two_sided": int(maker_l2_context.get("two_sided_count") or 0),
        "quoteable": int(maker_l2_context.get("quoteable_count") or 0),
        "live_quotes": int(maker_l2_context.get("live_quote_count") or 0),
        "l2_stale_reason": maker_l2_context.get("l2_stale_reason"),
    }
    if trade_count > 0:
        return {
            "bucket": "active" if trade_count > 4 else "low_activity",
            "label": str(run_classification.get("label") or "active"),
            "note": primary_blocker.get("note")
            or "Trading activity was present; the run was not a dead run.",
            "evidence": evidence,
        }
    return {
        "bucket": str(no_trade_diagnosis.get("bucket") or "inactive"),
        "label": str(
            no_trade_diagnosis.get("label")
            or primary_blocker.get("label")
            or run_classification.get("label")
            or "inactive"
        ),
        "note": str(
            no_trade_diagnosis.get("note")
            or primary_blocker.get("note")
            or "The run did not trade."
        ),
        "evidence": evidence,
    }


def main() -> int:
    args = parse_args()
    artifacts_root = Path(args.artifacts_root).expanduser().resolve()
    bundle = resolve_bundle(artifacts_root, args.db)
    print(json.dumps(bundle, separators=(",", ":"), sort_keys=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
