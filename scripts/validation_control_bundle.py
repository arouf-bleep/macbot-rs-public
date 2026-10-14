#!/usr/bin/env python3
"""Resolve the latest validation/control bundle and print a compact JSON index."""

from __future__ import annotations

import argparse
import json
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from proof_bundle_index import shutdown_evidence_class


def parse_args() -> argparse.Namespace:
    default_artifacts_root = Path(__file__).resolve().parent.parent / "artifacts"
    parser = argparse.ArgumentParser(
        description="Resolve the latest validation/control artifact bundle"
    )
    parser.add_argument(
        "--db",
        default=None,
        help="Optional SQLite DB path. If omitted, resolve the latest validation/control artifact set.",
    )
    parser.add_argument(
        "--artifacts-root",
        default=str(default_artifacts_root),
        help="Artifacts root directory",
    )
    parser.add_argument(
        "--output-dir",
        default=str(default_artifacts_root / "validation-control-bundles"),
        help="Directory for the resolved bundle JSON/Markdown artifacts",
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


def normalize_integrity_report(payload: dict[str, Any]) -> dict[str, Any]:
    status = payload.get("status")
    verdict = payload.get("verdict")
    reason = payload.get("reason")
    mismatches = payload.get("mismatches") or []
    if status is None:
        status = "ok" if payload.get("ok") else ("mismatch" if mismatches else None)
    if verdict is None:
        verdict = "ok" if payload.get("ok") else ("mismatch" if mismatches else None)
    if reason is None:
        if payload.get("ok") and not mismatches:
            reason = "clean"
        elif mismatches:
            first = mismatches[0] if isinstance(mismatches, list) and mismatches else {}
            reason = str(first.get("check") or "mismatch")
    mismatch_count = payload.get("mismatch_count")
    if mismatch_count is None:
        mismatch_count = len(mismatches) if isinstance(mismatches, list) else 0
    failure_count = payload.get("failure_count")
    if failure_count is None:
        failure_count = int(mismatch_count or 0)
    counts = payload.get("counts")
    if counts is None:
        counts = {}
    return {
        **payload,
        "status": status,
        "verdict": verdict,
        "reason": reason,
        "mismatch_count": int(mismatch_count or 0),
        "failure_count": int(failure_count or 0),
        "counts": counts,
    }


def compact_maker_l2_context(run_context: dict[str, Any]) -> dict[str, Any]:
    maker_l2 = (run_context or {}).get("maker_l2") or {}
    return {
        "snapshot_available": maker_l2.get("snapshot_available"),
        "entry_count": maker_l2.get("entry_count"),
        "market_count": maker_l2.get("market_count"),
        "live_quote_count": maker_l2.get("live_quote_count"),
        "loaded_count": maker_l2.get("loaded_count"),
        "two_sided_count": maker_l2.get("two_sided_count"),
        "one_sided_count": maker_l2.get("one_sided_count"),
        "stale_count": maker_l2.get("stale_count"),
        "quoteable_count": maker_l2.get("quoteable_count"),
        "l2_stale": maker_l2.get("l2_stale"),
        "l2_stale_reason": maker_l2.get("l2_stale_reason"),
    }


def derive_terminal_class(
    *,
    maker_report: dict[str, Any],
    sim_report: dict[str, Any],
    integrity_report: dict[str, Any],
) -> str:
    blocker = shutdown_evidence_class(sim_report, integrity_report)
    if blocker:
        return blocker
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

    maker_summary = maker_report.get("summary") or {}
    trade_count = int(maker_summary.get("trade_count") or 0)
    if trade_count > 0:
        if not sim_report["summary"]["execution_fill_rows"]:
            return "evidence_incomplete"
        if trade_count <= 4:
            return "low_activity"
        return "success"

    run_classification = maker_report.get("run_classification") or {}
    run_label = str(run_classification.get("label") or "").strip()
    primary_blocker = maker_report.get("primary_blocker_summary") or {}
    blocker_label = str(primary_blocker.get("label") or "").strip()
    no_trade_diagnosis = maker_report.get("no_trade_diagnosis") or {}
    no_trade_bucket = str(no_trade_diagnosis.get("bucket") or "").strip()

    if (
        run_label == "dead_window"
        or blocker_label == "no_target_markets"
        or no_trade_bucket == "dead_window"
    ):
        return "starvation/dead_window"
    if run_label == "inactive" or no_trade_bucket == "inactive":
        return "inactive/unexercised"
    if run_label == "policy_limited" or blocker_label in {
        "policy_suppression",
        "trade_rejections",
        "suppression_pressure",
    } or no_trade_bucket == "policy":
        return "policy_suppression"
    if run_label in {"market_quality_limited", "low_quoteability_limited"} or blocker_label in {
        "one_sided_books",
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


def build_bundle(artifacts_root: Path, db_arg: str | None) -> dict[str, Any]:
    if db_arg is not None:
        db_path = normalize_path(db_arg)
        resolved_from = "db"
    else:
        db_path = None
        resolved_from = "latest_artifact_set"

    maker_dir = artifacts_root / "maker-soak-reports"
    sim_dir = artifacts_root / "sim-price-verification-reports"
    integrity_dir = artifacts_root / "validation-integrity-reports"
    validation_dir = artifacts_root / "validation-bundles"

    maker_match = newest_matching_json(
        maker_dir,
        "*_maker_soak_report.json",
        db_path=db_path,
        prefer_latest_file="latest_maker_soak_report.json",
    )
    if maker_match is None:
        raise SystemExit(f"unable to resolve maker soak report in {maker_dir}")
    maker_path, maker_report = maker_match

    resolved_db = db_path or normalize_path(maker_report.get("db_path") or maker_report.get("db"))
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
    validation_match = newest_matching_json(
        validation_dir,
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
    integrity_report = normalize_integrity_report(integrity_report)
    validation_path, validation_report = (
        validation_match if validation_match is not None else (None, {})
    )

    maker_summary = maker_report.get("summary") or {}
    run_classification = maker_report.get("run_classification") or {}
    primary_blocker = maker_report.get("primary_blocker_summary") or {}
    no_trade_diagnosis = maker_report.get("no_trade_diagnosis") or {}
    run_context = maker_report.get("run_context") or {}
    maker_l2_context = compact_maker_l2_context(run_context)
    sim_summary = sim_report.get("summary") or {}
    integrity_counts = integrity_report.get("counts") or {}
    integrity_trade_counts = integrity_report.get("trade_counts") or {}
    integrity_fill_counts = integrity_report.get("fill_integrity") or {}

    terminal_class = derive_terminal_class(
        maker_report=maker_report,
        sim_report=sim_report,
        integrity_report=integrity_report,
    )

    blocker_label = (
        primary_blocker.get("label")
        or no_trade_diagnosis.get("label")
        or no_trade_diagnosis.get("bucket")
    )
    dead_run_summary = build_dead_run_summary(
        trade_count=int(maker_summary.get("trade_count") or 0),
        run_classification=run_classification,
        primary_blocker=primary_blocker,
        no_trade_diagnosis=no_trade_diagnosis,
        maker_l2_context=maker_l2_context,
    )

    summary = {
        "trade_count": int(maker_summary.get("trade_count") or 0),
        "open_trade_count": int(integrity_trade_counts.get("open_count") or 0),
        "closing_trade_count": int(integrity_trade_counts.get("closing_count") or 0),
        "fill_count": int(maker_summary.get("fill_count") or 0),
        "cleanup_trade_count": int(integrity_trade_counts.get("cleanup_trade_count") or 0),
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
        "terminal_class": terminal_class,
        "blocker_label": blocker_label,
        "maker_l2_market_count": int(maker_l2_context.get("market_count") or 0),
        "maker_l2_live_quote_count": int(maker_l2_context.get("live_quote_count") or 0),
        "maker_l2_loaded_count": int(maker_l2_context.get("loaded_count") or 0),
        "maker_l2_two_sided_count": int(maker_l2_context.get("two_sided_count") or 0),
        "maker_l2_one_sided_count": int(maker_l2_context.get("one_sided_count") or 0),
        "maker_l2_stale_count": int(maker_l2_context.get("stale_count") or 0),
        "maker_l2_quoteable_count": int(maker_l2_context.get("quoteable_count") or 0),
        "maker_l2_stale": maker_l2_context.get("l2_stale"),
        "maker_l2_stale_reason": maker_l2_context.get("l2_stale_reason"),
    }

    return {
        "bundle_kind": "validation_control",
        "resolved_from": resolved_from,
        "db": resolved_db,
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "terminal_class": terminal_class,
        "run_classification": run_classification,
        "run_classification_label": run_classification.get("label"),
        "blocker_label": blocker_label,
        "integrity_verdict": summary["integrity_verdict"],
        "integrity_reason": summary["integrity_reason"],
        "trade_count": summary["trade_count"],
        "fill_count": summary["fill_count"],
        "open_trade_count": summary["open_trade_count"],
        "closing_trade_count": summary["closing_trade_count"],
        "maker_soak_report_path": str(maker_path),
        "sim_verification_report_path": str(sim_path),
        "integrity_report_path": str(integrity_path),
        "validation_bundle_path": str(validation_path) if validation_path is not None else None,
        "primary_blocker_summary": primary_blocker,
        "no_trade_diagnosis": no_trade_diagnosis,
        "dead_run_summary": dead_run_summary,
        "maker_l2_context": maker_l2_context,
        "summary": summary,
    }


def render_markdown(bundle: dict[str, Any]) -> str:
    summary = bundle.get("summary") or {}
    run_classification = bundle.get("run_classification") or {}
    primary_blocker = bundle.get("primary_blocker_summary") or {}
    no_trade_diagnosis = bundle.get("no_trade_diagnosis") or {}
    dead_run_summary = bundle.get("dead_run_summary") or {}
    maker_l2_context = bundle.get("maker_l2_context") or {}
    return "\n".join(
        [
            "# Validation Control Bundle",
            "",
            f"- DB: `{bundle.get('db')}`",
            f"- Terminal class: `{bundle.get('terminal_class')}`",
            f"- Run classification: `{run_classification.get('label')}`",
            f"- Blocker label: `{bundle.get('blocker_label')}`",
            f"- Primary blocker: `{primary_blocker.get('label')}`",
            f"- No-trade diagnosis: `{no_trade_diagnosis.get('bucket')}` / `{no_trade_diagnosis.get('label')}`",
            f"- Dead-run summary: `{dead_run_summary.get('bucket')}` / `{dead_run_summary.get('label')}`",
            f"- Dead-run note: `{dead_run_summary.get('note')}`",
            (
                "- Maker L2 context: "
                f"`markets={maker_l2_context.get('market_count')}` "
                f"`loaded={maker_l2_context.get('loaded_count')}` "
                f"`two_sided={maker_l2_context.get('two_sided_count')}` "
                f"`one_sided={maker_l2_context.get('one_sided_count')}` "
                f"`stale={maker_l2_context.get('stale_count')}` "
                f"`quoteable={maker_l2_context.get('quoteable_count')}` "
                f"`live_quotes={maker_l2_context.get('live_quote_count')}` "
                f"`l2_stale_reason={maker_l2_context.get('l2_stale_reason')}`"
            ),
            "",
            "## Summary",
            f"- Trades / open / closing: `{summary.get('trade_count')}` / `{summary.get('open_trade_count')}` / `{summary.get('closing_trade_count')}`",
            f"- Fills / cleanup marks: `{summary.get('fill_count')}` / `{summary.get('cleanup_mark_rows')}`",
            f"- Execution fills plausible / implausible: `{summary.get('plausible_execution_rows')}` / `{summary.get('implausible_execution_rows')}`",
            f"- Integrity verdict: `{summary.get('integrity_verdict')}`",
            f"- Integrity reason: `{summary.get('integrity_reason')}`",
        ]
    ) + "\n"


def main() -> int:
    args = parse_args()
    artifacts_root = Path(args.artifacts_root).expanduser().resolve()
    output_dir = Path(args.output_dir).expanduser().resolve()
    output_dir.mkdir(parents=True, exist_ok=True)

    bundle = build_bundle(artifacts_root, args.db)

    stamp = Path(args.db).name if args.db else datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = output_dir / f"{stamp}_validation_control_bundle.json"
    md_path = output_dir / f"{stamp}_validation_control_bundle.md"
    latest_json = output_dir / "latest_validation_control_bundle.json"
    latest_md = output_dir / "latest_validation_control_bundle.md"
    json_text = json.dumps(bundle, indent=2)
    md_text = render_markdown(bundle)
    json_path.write_text(json_text, encoding="utf-8")
    md_path.write_text(md_text, encoding="utf-8")
    latest_json.write_text(json_text, encoding="utf-8")
    latest_md.write_text(md_text, encoding="utf-8")
    print(json.dumps({"json": str(json_path), "md": str(md_path), "db": bundle["db"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
