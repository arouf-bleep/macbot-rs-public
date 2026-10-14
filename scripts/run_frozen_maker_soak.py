#!/usr/bin/env python3
"""Run a maker paper-soak from a frozen config profile without patching it."""

from __future__ import annotations

import argparse
import json
import math
import uuid
import os
import shlex
import sqlite3
import subprocess
import sys
import tempfile
import time
import threading
import socket
import urllib.error
import urllib.request
from collections import deque
from contextlib import closing
from datetime import datetime, timezone
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

from run_maker_soak import paper_run_environment, read_key, bind_report_configuration, configuration_evidence, assert_configuration_unchanged
from api_auth import get_json, open_api_request
from http_limits import read_response
from proof_bundle_index import shutdown_evidence_class


class SoakStartupFailure(RuntimeError):
    def __init__(
        self,
        classification: str,
        detail: str,
        *,
        elapsed_seconds: float | None = None,
        returncode: int | None = None,
        log_excerpt: str | None = None,
    ) -> None:
        super().__init__(detail)
        self.classification = classification
        self.detail = detail
        self.elapsed_seconds = elapsed_seconds
        self.returncode = returncode
        self.log_excerpt = log_excerpt or ""


def classify_startup_failure(
    *,
    proc_returncode: int | None,
    elapsed_seconds: float | None,
    log_excerpt: str,
    timeout: bool = False,
) -> SoakStartupFailure:
    lowered = log_excerpt.lower()
    if timeout:
        return SoakStartupFailure(
            "startup_no_health_timeout",
            "frozen maker soak bot did not become healthy before the startup timeout",
            elapsed_seconds=elapsed_seconds,
            returncode=proc_returncode,
            log_excerpt=log_excerpt,
        )
    if "address already in use" in lowered or "os error 48" in lowered or "os error 98" in lowered:
        return SoakStartupFailure(
            "startup_bind_conflict",
            "frozen maker soak startup failed because the API port was already in use",
            elapsed_seconds=elapsed_seconds,
            returncode=proc_returncode,
            log_excerpt=log_excerpt,
        )
    return SoakStartupFailure(
        "startup_process_exited_before_health",
        "frozen maker soak process exited before the API became healthy",
        elapsed_seconds=elapsed_seconds,
        returncode=proc_returncode,
        log_excerpt=log_excerpt,
    )


def _tail_excerpt(lines: deque[str], limit: int = 24) -> str:
    if not lines:
        return ""
    return "\n".join(list(lines)[-max(1, limit):])


def _spawn_logged_process(
    cargo_cmd: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
) -> tuple[subprocess.Popen[str], threading.Thread, deque[str]]:
    proc = subprocess.Popen(
        cargo_cmd,
        cwd=cwd,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        bufsize=1,
    )
    log_tail: deque[str] = deque(maxlen=200)

    def _pump() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            log_tail.append(line.rstrip("\n"))
            sys.stdout.write(line)
            sys.stdout.flush()

    thread = threading.Thread(target=_pump, daemon=True)
    thread.start()
    return proc, thread, log_tail


def preflight_bind_conflict(base_url: str) -> None:
    parsed = urlparse(base_url)
    host = parsed.hostname or "127.0.0.1"
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.5)
        if sock.connect_ex((host, port)) == 0:
            raise SoakStartupFailure(
                "startup_bind_conflict",
                f"startup blocked because {host}:{port} is already accepting connections before launch",
            )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run a maker paper-soak from a frozen config file")
    parser.add_argument("--config", required=True, help="Frozen config JSON path")
    parser.add_argument("--db", default=None, help="Fresh SQLite DB path; defaults to an isolated artifact")
    parser.add_argument(
        "--duration-minutes",
        type=float,
        default=30.0,
        help="How long to leave the bot running before generating a soak report",
    )
    parser.add_argument(
        "--report-hours",
        type=float,
        default=None,
        help="Telemetry lookback window for the final maker soak report",
    )
    parser.add_argument(
        "--base-url",
        default="http://127.0.0.1:8000",
        help="Local API base URL expected from the bot",
    )
    parser.add_argument(
        "--cargo-cmd",
        default="cargo run",
        help="Command used to start the bot",
    )
    parser.add_argument(
        "--shutdown-validation",
        action="store_true",
        help="Fail loudly if the post-exit DB still has open trades or no shutdown_cleanup evidence",
    )
    parser.add_argument(
        "--shutdown-validation-self-test",
        action="store_true",
        help="Run a focused in-file smoke test for the shutdown validation checks and exit",
    )
    parser.add_argument(
        "--startup-failure-self-test",
        action="store_true",
        help="Run a focused in-file smoke test for startup failure classification and exit",
    )
    parser.add_argument(
        "--stop-on-open-trades",
        type=int,
        default=0,
        help="If >0, stop the soak early once at least this many OPEN/CLOSING trades exist in the DB",
    )
    parser.add_argument(
        "--min-run-seconds",
        type=float,
        default=0.0,
        help="Minimum elapsed runtime before --stop-on-open-trades can trigger",
    )
    return parser.parse_args()


def wait_ready(
    proc: subprocess.Popen[str],
    base_url: str,
    log_tail: deque[str],
    timeout_secs: float = 60.0,
) -> None:
    deadline = time.time() + timeout_secs
    last_error: str | None = None
    while time.time() < deadline:
        if proc.poll() is not None:
            raise classify_startup_failure(
                proc_returncode=proc.returncode,
                elapsed_seconds=max(0.0, timeout_secs - max(0.0, deadline - time.time())),
                log_excerpt=_tail_excerpt(log_tail),
            )
        try:
            req = urllib.request.Request(f"{base_url}/api/health")
            with open_api_request(req, timeout=3) as resp:
                payload = json.loads(read_response(resp, label="Health response").decode("utf-8"))
            if payload.get("status") == "ok":
                return
            last_error = f"unexpected health payload: {payload}"
        except (urllib.error.URLError, TimeoutError, ValueError) as exc:
            last_error = str(exc)
        time.sleep(1.0)
    raise classify_startup_failure(
        proc_returncode=proc.poll(),
        elapsed_seconds=timeout_secs,
        log_excerpt=_tail_excerpt(log_tail) or (last_error or "timeout"),
        timeout=True,
    )


def fetch_json(url: str, timeout_secs: float = 5.0, api_key: str | None = None) -> dict[str, Any]:
    return get_json(url, timeout=timeout_secs, api_key=api_key, read_auth=True)


def terminate_process(proc: subprocess.Popen[str]) -> None:
    if proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=30)
    except subprocess.TimeoutExpired:
        print(
            "frozen maker soak: graceful shutdown exceeded 30s; forcing SIGKILL",
            file=sys.stderr,
        )
        proc.kill()
        proc.wait(timeout=5)


def load_active_trade_count(db_path: Path) -> int:
    if not db_path.exists():
        return 0
    with closing(sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True)) as conn:
        row = conn.execute(
            """
            SELECT COUNT(*)
            FROM trades
            WHERE status IN ('OPEN', 'CLOSING')
            """
        ).fetchone()
    return int((row[0] if row else 0) or 0)


def wait_for_soak_window(
    proc: subprocess.Popen[str],
    duration_minutes: float,
    db_path: Path,
    stop_on_open_trades: int,
    min_run_seconds: float,
) -> dict[str, Any]:
    duration_secs = max(0.0, duration_minutes) * 60.0
    started_at = time.time()
    deadline = started_at + duration_secs
    peak_active_trade_count = 0
    target_reached = False
    while True:
        if proc.poll() is not None:
            raise RuntimeError(
                f"frozen maker soak process exited early with code {proc.returncode}"
            )
        elapsed = time.time() - started_at
        current_active_trade_count = load_active_trade_count(db_path)
        peak_active_trade_count = max(peak_active_trade_count, current_active_trade_count)
        if stop_on_open_trades > 0 and elapsed >= max(0.0, min_run_seconds):
            if current_active_trade_count >= stop_on_open_trades:
                target_reached = True
                break
        remaining = deadline - time.time()
        if remaining <= 0.0:
            break
        time.sleep(min(1.0, remaining))
    elapsed_seconds = max(0.0, time.time() - started_at)
    return {
        "elapsed_seconds": elapsed_seconds,
        "peak_active_trade_count": peak_active_trade_count,
        "stop_on_open_target_reached": target_reached,
        "exit_cause": "open_trade_target_reached" if target_reached else "duration_expired",
    }


def run_report(
    db_path: Path,
    report_hours: float,
    maker_state_snapshot_path: Path | None = None,
    no_arb_snapshot_path: Path | None = None,
) -> dict[str, Any]:
    root = Path(__file__).resolve().parent.parent
    cmd = [
        sys.executable,
        "scripts/maker_soak_report.py",
        "--db",
        str(db_path),
        "--hours",
        f"{report_hours:.6f}",
    ]
    if maker_state_snapshot_path is not None:
        cmd.extend(["--maker-state-snapshot", str(maker_state_snapshot_path)])
    if no_arb_snapshot_path is not None:
        cmd.extend(["--no-arb-snapshot", str(no_arb_snapshot_path)])
    try:
        proc = subprocess.run(cmd, cwd=root, check=True, capture_output=True, text=True)
        return json.loads(proc.stdout.strip())
    except subprocess.CalledProcessError as exc:
        if maker_state_snapshot_path is None and no_arb_snapshot_path is None:
            raise RuntimeError(exc.stderr or exc.stdout or str(exc)) from exc
        fallback_cmd = [
            sys.executable,
            "scripts/maker_soak_report.py",
            "--db",
            str(db_path),
            "--hours",
            f"{report_hours:.6f}",
        ]
        fallback = subprocess.run(
            fallback_cmd,
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        )
        return json.loads(fallback.stdout.strip())


def run_sim_verification_report(db_path: Path) -> dict[str, Any]:
    root = Path(__file__).resolve().parent.parent
    cmd = [
        sys.executable,
        "scripts/sim_price_verification_report.py",
        "--db",
        str(db_path),
    ]
    proc = subprocess.run(cmd, cwd=root, check=True, capture_output=True, text=True)
    return json.loads(proc.stdout.strip())


def run_validation_integrity_report(db_path: Path) -> dict[str, Any]:
    root = Path(__file__).resolve().parent.parent
    cmd = [
        sys.executable,
        "scripts/validation_db_integrity_report.py",
        "--db",
        str(db_path),
    ]
    proc = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
    stdout = proc.stdout.strip()
    if not stdout:
        raise RuntimeError(
            proc.stderr.strip() or "validation DB integrity report did not produce JSON output"
        )
    try:
        payload = json.loads(stdout)
    except json.JSONDecodeError as exc:
        raise RuntimeError(
            f"validation DB integrity report returned non-JSON output: {stdout[:400]}"
        ) from exc
    if proc.returncode not in (0, 2):
        raise RuntimeError(
            proc.stderr.strip()
            or f"validation DB integrity report exited with unexpected code {proc.returncode}"
        )
    return payload


def write_validation_bundle(payload: dict[str, Any], db_path: Path) -> dict[str, str]:
    root = Path(__file__).resolve().parent.parent
    outdir = root / "artifacts" / "validation-bundles"
    outdir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    db_stem = db_path.stem or "validation"
    json_path = outdir / f"{stamp}_{db_stem}_validation_bundle.json"
    latest_path = outdir / "latest_validation_bundle.json"
    serialized = json.dumps(payload, indent=2)
    json_path.write_text(serialized, encoding="utf-8")
    latest_path.write_text(serialized, encoding="utf-8")
    return {
        "json": str(json_path),
        "latest_json": str(latest_path),
    }


def _as_int(value: Any, default: int = 0) -> int:
    try:
        if value is None:
            return default
        return int(value)
    except (TypeError, ValueError):
        return default


def _first_label(entries: Any) -> str | None:
    if isinstance(entries, list):
        for entry in entries:
            if isinstance(entry, dict):
                label = entry.get("label")
                if label:
                    return str(label)
    return None


def _blocker_label(blocker_summary: Any, key: str, fallback: Any = None) -> str | None:
    if isinstance(blocker_summary, dict):
        value = blocker_summary.get(key)
        if isinstance(value, dict):
            label = value.get("label")
            if label:
                return str(label)
    return _first_label(fallback)


def summarize_preflight_quoteability(maker_state_snapshot: dict[str, Any] | None) -> dict[str, Any]:
    maker_snapshot = maker_state_snapshot or {}
    maker = maker_snapshot.get("maker", maker_snapshot)
    l2_summary = maker.get("l2_summary") or {}
    l2_rows = maker.get("l2_top_of_book") or []
    snapshot_available = maker_state_snapshot is not None
    loaded_count = int(l2_summary.get("snapshot_loaded_count") or sum(
        1 for row in l2_rows if row.get("snapshot_loaded")
    ))
    two_sided_count = int(l2_summary.get("two_sided_count") or sum(
        1
        for row in l2_rows
        if row.get("snapshot_loaded")
        and not row.get("stale")
        and row.get("best_bid") is not None
        and row.get("best_ask") is not None
    ))
    one_sided_count = int(l2_summary.get("one_sided_count") or sum(
        1
        for row in l2_rows
        if row.get("snapshot_loaded")
        and not row.get("stale")
        and ((row.get("best_bid") is None) != (row.get("best_ask") is None))
    ))
    stale_count = int(l2_summary.get("stale_count") or sum(
        1 for row in l2_rows if row.get("stale")
    ))
    quoteable_count = int(l2_summary.get("quoteable_count") or max(two_sided_count - stale_count, 0))
    live_quote_count = maker.get("quote_count")
    market_count = maker.get("market_count")
    stale_reason = str(maker.get("l2_stale_reason") or "").lower()

    evidence = {
        "snapshot_available": snapshot_available,
        "loaded_count": loaded_count,
        "two_sided_count": two_sided_count,
        "one_sided_count": one_sided_count,
        "stale_count": stale_count,
        "quoteable_count": quoteable_count,
        "live_quote_count": live_quote_count,
        "market_count": market_count,
        "l2_stale": maker.get("l2_stale"),
        "l2_stale_reason": maker.get("l2_stale_reason"),
    }

    if market_count in (0, None) and live_quote_count in (0, None) and stale_reason == "no target markets":
        return {
            "label": "no_target_markets",
            "bucket": "dead_window",
            "source": "preflight",
            "note": "No active markets were available before the soak began.",
            "evidence": evidence,
        }
    if not snapshot_available or loaded_count == 0:
        return {
            "label": "snapshot_missing",
            "bucket": "low_quoteability",
            "source": "preflight",
            "note": "No L2 snapshot was available before the soak began.",
            "evidence": evidence,
        }
    if one_sided_count > 0 and two_sided_count == 0:
        return {
            "label": "one_sided_books",
            "bucket": "low_quoteability",
            "source": "preflight",
            "note": "Preflight books were present but still one-sided.",
            "evidence": evidence,
        }
    if stale_count > 0 or any(
        marker in stale_reason
        for marker in ("stale", "session error", "warmup", "rearm", "backpressure")
    ):
        return {
            "label": "stale_books",
            "bucket": "low_quoteability",
            "source": "preflight",
            "note": "Preflight L2 readiness was stale before the soak began.",
            "evidence": evidence,
        }
    if quoteable_count <= 0:
        return {
            "label": "low_quoteability",
            "bucket": "low_quoteability",
            "source": "preflight",
            "note": "Preflight books were present but not clearly quoteable.",
            "evidence": evidence,
        }
    return {
        "label": "quoteable_books",
        "bucket": "quoteable",
        "source": "preflight",
        "note": "Preflight books looked quoteable.",
        "evidence": evidence,
    }


def classify_terminal_outcome(
    report: dict[str, Any] | None,
    verification_report: dict[str, Any] | None,
    shutdown_validation: dict[str, Any] | None,
    integrity_report: dict[str, Any] | None,
    post_run_error: Exception | None,
    preflight_quoteability: dict[str, Any] | None = None,
) -> dict[str, Any]:
    summary = report.get("summary") if isinstance(report, dict) else {}
    summary = summary if isinstance(summary, dict) else {}
    blocker_summary = summary.get("blocker_summary")
    blocker_summary = blocker_summary if isinstance(blocker_summary, dict) else {}

    event_count = _as_int(report.get("event_count") if isinstance(report, dict) else None)
    place_event_count = _as_int(
        report.get("place_event_count") if isinstance(report, dict) else None
    )
    cancel_event_count = _as_int(
        report.get("cancel_event_count") if isinstance(report, dict) else None
    )
    guard_shadow_event_count = _as_int(
        report.get("guard_shadow_event_count") if isinstance(report, dict) else None
    )
    guard_skip_event_count = _as_int(
        report.get("guard_skip_event_count") if isinstance(report, dict) else None
    )
    trade_count = _as_int(summary.get("trade_count"))
    fill_count = _as_int(summary.get("fill_count"))
    open_trade_count = _as_int(summary.get("open_trade_count"))
    closing_trade_count = _as_int(summary.get("closing_trade_count"))
    quote_suppress_count = _as_int(summary.get("quote_suppress_count"))
    trade_rejection_count = _as_int(summary.get("trade_rejection_count_total"))
    market_quality_suppress_count = _as_int(summary.get("market_quality_suppress_count"))
    entry_ineligible_suppress_count = _as_int(summary.get("entry_ineligible_suppress_count"))
    maker_buy_no_suppress_count = _as_int(summary.get("maker_buy_no_suppress_count"))

    quote_suppress_reason_counts = (
        report.get("quote_suppress_reason_counts")
        if isinstance(report, dict)
        else None
    )
    quote_cancel_reason_counts = (
        report.get("quote_cancel_reason_counts")
        if isinstance(report, dict)
        else None
    )

    top_quote_suppress = _blocker_label(
        blocker_summary,
        "top_quote_suppress",
        summary.get("quote_suppress_subreason_top"),
    )
    top_quote_cancel = _blocker_label(
        blocker_summary,
        "top_quote_cancel",
        summary.get("quote_cancel_subreason_top"),
    )
    top_trade_rejection = _blocker_label(
        blocker_summary,
        "top_trade_rejection",
        summary.get("trade_rejection_reason_top"),
    )
    top_decision_reason = _blocker_label(
        blocker_summary,
        "top_decision_reason",
        summary.get("decision_reason_top"),
    )

    verification_summary = (
        verification_report.get("summary") if isinstance(verification_report, dict) else {}
    )
    verification_summary = (
        verification_summary if isinstance(verification_summary, dict) else {}
    )
    implausible_execution_rows = _as_int(
        verification_summary.get("implausible_execution_rows")
    )
    cleanup_mark_rows = _as_int(verification_summary.get("cleanup_mark_rows"))
    execution_fill_rows = _as_int(verification_summary.get("execution_fill_rows"))
    total_fill_rows = _as_int(verification_summary.get("total_fill_rows"))

    integrity_status = (
        integrity_report.get("status") if isinstance(integrity_report, dict) else None
    )
    integrity_mismatch_count = _as_int(
        integrity_report.get("mismatch_count") if isinstance(integrity_report, dict) else None
    )
    integrity_failure_count = _as_int(
        integrity_report.get("failure_count") if isinstance(integrity_report, dict) else None
    )
    shutdown_active_trade_count = _as_int(
        shutdown_validation.get("active_trade_count")
        if isinstance(shutdown_validation, dict)
        else None
    )
    shutdown_closing_trade_count = _as_int(
        shutdown_validation.get("closing_trade_count")
        if isinstance(shutdown_validation, dict)
        else None
    )

    lifecycle_bug = False
    lifecycle_reason: str | None = None
    if post_run_error is not None:
        lifecycle_bug = True
        lifecycle_reason = str(post_run_error)
    elif integrity_report is not None and (
        integrity_status != "ok"
        or integrity_mismatch_count > 0
        or integrity_failure_count > 0
    ):
        lifecycle_bug = True
        lifecycle_reason = (
            f"integrity report status={integrity_status!r} mismatches={integrity_mismatch_count}"
        )
    elif isinstance(shutdown_validation, dict) and (
        shutdown_active_trade_count > 0
        or shutdown_closing_trade_count > 0
        or (
            shutdown_validation.get("shutdown_cleanup_required") is True
            and _as_int(shutdown_validation.get("shutdown_cleanup_trade_count")) == 0
        )
    ):
        lifecycle_bug = True
        lifecycle_reason = (
            "shutdown validation found stranded OPEN/CLOSING trades or missing cleanup evidence"
        )
    elif trade_count > 0 and implausible_execution_rows > 0:
        lifecycle_bug = True
        lifecycle_reason = (
            f"verification report found {implausible_execution_rows} implausible execution rows"
        )

    if lifecycle_bug:
        reason = lifecycle_reason or "post-run lifecycle validation failed"
        return {
            "class": "lifecycle_bug",
            "reason": reason,
            "evidence": {
                "trade_count": trade_count,
                "fill_count": fill_count,
                "event_count": event_count,
                "place_event_count": place_event_count,
                "cancel_event_count": cancel_event_count,
                "guard_shadow_event_count": guard_shadow_event_count,
                "guard_skip_event_count": guard_skip_event_count,
                "quote_suppress_count": quote_suppress_count,
                "trade_rejection_count": trade_rejection_count,
                "top_quote_suppress": top_quote_suppress,
                "top_quote_cancel": top_quote_cancel,
                "top_trade_rejection": top_trade_rejection,
                "top_decision_reason": top_decision_reason,
                "verification_implausible_execution_rows": implausible_execution_rows,
                "verification_cleanup_mark_rows": cleanup_mark_rows,
                "verification_execution_fill_rows": execution_fill_rows,
                "verification_total_fill_rows": total_fill_rows,
                "integrity_status": integrity_status,
                "integrity_mismatch_count": integrity_mismatch_count,
                "shutdown_active_trade_count": shutdown_active_trade_count,
                "shutdown_closing_trade_count": shutdown_closing_trade_count,
            },
        }

    evidence_blocker = shutdown_evidence_class(verification_report or {}, integrity_report or {})
    if evidence_blocker:
        return {"class": evidence_blocker, "reason": "missing or contradictory shutdown/fill evidence", "evidence": {}}
    if trade_count > 0 and execution_fill_rows == 0:
        return {"class": "evidence_incomplete", "reason": "trades have no verifiable execution fills", "evidence": {}}

    if trade_count > 0:
        return {
            "class": "success",
            "reason": "run completed with one or more trades and no lifecycle mismatches",
            "evidence": {
                "trade_count": trade_count,
                "fill_count": fill_count,
                "event_count": event_count,
                "place_event_count": place_event_count,
                "cancel_event_count": cancel_event_count,
                "guard_shadow_event_count": guard_shadow_event_count,
                "guard_skip_event_count": guard_skip_event_count,
                "quote_suppress_count": quote_suppress_count,
                "trade_rejection_count": trade_rejection_count,
                "top_quote_suppress": top_quote_suppress,
                "top_quote_cancel": top_quote_cancel,
                "top_trade_rejection": top_trade_rejection,
                "top_decision_reason": top_decision_reason,
                "verification_implausible_execution_rows": implausible_execution_rows,
                "verification_cleanup_mark_rows": cleanup_mark_rows,
                "verification_execution_fill_rows": execution_fill_rows,
                "verification_total_fill_rows": total_fill_rows,
                "integrity_status": integrity_status,
                "integrity_mismatch_count": integrity_mismatch_count,
                "shutdown_active_trade_count": shutdown_active_trade_count,
                "shutdown_closing_trade_count": shutdown_closing_trade_count,
            },
        }

    quote_suppress_counts = quote_suppress_reason_counts if isinstance(quote_suppress_reason_counts, dict) else {}
    quote_cancel_counts = quote_cancel_reason_counts if isinstance(quote_cancel_reason_counts, dict) else {}

    if (
        event_count == 0
        and place_event_count == 0
        and cancel_event_count == 0
        and guard_shadow_event_count == 0
        and guard_skip_event_count == 0
        and quote_suppress_count == 0
        and trade_rejection_count == 0
    ):
        preflight_label = str((preflight_quoteability or {}).get("label") or "")
        preflight_note = str((preflight_quoteability or {}).get("note") or "")
        if preflight_label == "no_target_markets":
            return {
                "class": "starvation/dead_window",
                "reason": preflight_note or "preflight found no target markets before the soak began",
                "evidence": {
                    "trade_count": trade_count,
                    "fill_count": fill_count,
                    "event_count": event_count,
                    "place_event_count": place_event_count,
                    "cancel_event_count": cancel_event_count,
                    "guard_shadow_event_count": guard_shadow_event_count,
                    "guard_skip_event_count": guard_skip_event_count,
                    "quote_suppress_count": quote_suppress_count,
                    "trade_rejection_count": trade_rejection_count,
                    "quote_suppress_reason_counts": quote_suppress_counts,
                    "quote_cancel_reason_counts": quote_cancel_counts,
                    "integrity_status": integrity_status,
                    "integrity_mismatch_count": integrity_mismatch_count,
                    "preflight_quoteability": preflight_quoteability,
                },
            }
        if preflight_label in {"one_sided_books", "stale_books", "snapshot_missing", "low_quoteability"}:
            return {
                "class": "low_quoteability",
                "reason": preflight_note or "preflight quoteability was too weak to justify a quoteable soak window",
                "evidence": {
                    "trade_count": trade_count,
                    "fill_count": fill_count,
                    "event_count": event_count,
                    "place_event_count": place_event_count,
                    "cancel_event_count": cancel_event_count,
                    "guard_shadow_event_count": guard_shadow_event_count,
                    "guard_skip_event_count": guard_skip_event_count,
                    "quote_suppress_count": quote_suppress_count,
                    "trade_rejection_count": trade_rejection_count,
                    "quote_suppress_reason_counts": quote_suppress_counts,
                    "quote_cancel_reason_counts": quote_cancel_counts,
                    "integrity_status": integrity_status,
                    "integrity_mismatch_count": integrity_mismatch_count,
                    "preflight_quoteability": preflight_quoteability,
                },
            }
        return {
            "class": "starvation/dead_window",
            "reason": "no events, quotes, suppressions, or rejections were recorded during the run",
            "evidence": {
                "trade_count": trade_count,
                "fill_count": fill_count,
                "event_count": event_count,
                "place_event_count": place_event_count,
                "cancel_event_count": cancel_event_count,
                "guard_shadow_event_count": guard_shadow_event_count,
                "guard_skip_event_count": guard_skip_event_count,
                "quote_suppress_count": quote_suppress_count,
                "trade_rejection_count": trade_rejection_count,
                "quote_suppress_reason_counts": quote_suppress_counts,
                "quote_cancel_reason_counts": quote_cancel_counts,
                "integrity_status": integrity_status,
                "integrity_mismatch_count": integrity_mismatch_count,
                "preflight_quoteability": preflight_quoteability,
            },
        }

    market_quality_score = market_quality_suppress_count
    top_suppress = (top_quote_suppress or "").lower()
    top_cancel = (top_quote_cancel or "").lower()
    if "market_quality" in top_suppress or "market_quality" in top_cancel:
        market_quality_score = max(market_quality_score, quote_suppress_count, trade_rejection_count)

    policy_score = max(
        quote_suppress_count,
        entry_ineligible_suppress_count,
        maker_buy_no_suppress_count,
    )
    policy_like = any(
        label in (top_suppress or "")
        for label in ("entry_ineligible", "maker_buy_no", "no_inventory")
    ) or any(
        label in (top_cancel or "")
        for label in ("entry_ineligible", "maker_buy_no", "no_inventory")
    )

    if "market_quality" in top_suppress or "market_quality" in top_cancel:
        return {
            "class": "market_quality_failure",
            "reason": "market quality suppressions/cancels dominated the run",
            "evidence": {
                "trade_count": trade_count,
                "fill_count": fill_count,
                "event_count": event_count,
                "place_event_count": place_event_count,
                "cancel_event_count": cancel_event_count,
                "guard_shadow_event_count": guard_shadow_event_count,
                "guard_skip_event_count": guard_skip_event_count,
                "quote_suppress_count": quote_suppress_count,
                "trade_rejection_count": trade_rejection_count,
                "market_quality_suppress_count": market_quality_suppress_count,
                "top_quote_suppress": top_quote_suppress,
                "top_quote_cancel": top_quote_cancel,
                "top_trade_rejection": top_trade_rejection,
                "quote_suppress_reason_counts": quote_suppress_counts,
                "quote_cancel_reason_counts": quote_cancel_counts,
                "integrity_status": integrity_status,
                "integrity_mismatch_count": integrity_mismatch_count,
            },
        }

    if market_quality_score > 0 and market_quality_score >= policy_score:
        return {
            "class": "market_quality_failure",
            "reason": "market quality suppressions/cancels dominated the run",
            "evidence": {
                "trade_count": trade_count,
                "fill_count": fill_count,
                "event_count": event_count,
                "place_event_count": place_event_count,
                "cancel_event_count": cancel_event_count,
                "guard_shadow_event_count": guard_shadow_event_count,
                "guard_skip_event_count": guard_skip_event_count,
                "quote_suppress_count": quote_suppress_count,
                "trade_rejection_count": trade_rejection_count,
                "market_quality_suppress_count": market_quality_suppress_count,
                "top_quote_suppress": top_quote_suppress,
                "top_quote_cancel": top_quote_cancel,
                "top_trade_rejection": top_trade_rejection,
                "quote_suppress_reason_counts": quote_suppress_counts,
                "quote_cancel_reason_counts": quote_cancel_counts,
                "integrity_status": integrity_status,
                "integrity_mismatch_count": integrity_mismatch_count,
            },
        }

    if policy_like or policy_score > 0 or quote_suppress_count > 0 or trade_rejection_count > 0:
        return {
            "class": "policy_suppression",
            "reason": "policy gates or inventory constraints suppressed quoting/trading",
            "evidence": {
                "trade_count": trade_count,
                "fill_count": fill_count,
                "event_count": event_count,
                "place_event_count": place_event_count,
                "cancel_event_count": cancel_event_count,
                "guard_shadow_event_count": guard_shadow_event_count,
                "guard_skip_event_count": guard_skip_event_count,
                "quote_suppress_count": quote_suppress_count,
                "trade_rejection_count": trade_rejection_count,
                "entry_ineligible_suppress_count": entry_ineligible_suppress_count,
                "maker_buy_no_suppress_count": maker_buy_no_suppress_count,
                "top_quote_suppress": top_quote_suppress,
                "top_quote_cancel": top_quote_cancel,
                "top_trade_rejection": top_trade_rejection,
                "top_decision_reason": top_decision_reason,
                "quote_suppress_reason_counts": quote_suppress_counts,
                "quote_cancel_reason_counts": quote_cancel_counts,
                "blocker_summary": blocker_summary,
                "integrity_status": integrity_status,
                "integrity_mismatch_count": integrity_mismatch_count,
            },
        }

    return {
        "class": "starvation/dead_window",
        "reason": "the run produced no trades and no actionable suppressor dominated the metrics",
        "evidence": {
            "trade_count": trade_count,
            "fill_count": fill_count,
            "event_count": event_count,
            "place_event_count": place_event_count,
            "cancel_event_count": cancel_event_count,
            "guard_shadow_event_count": guard_shadow_event_count,
            "guard_skip_event_count": guard_skip_event_count,
            "quote_suppress_count": quote_suppress_count,
            "trade_rejection_count": trade_rejection_count,
            "quote_suppress_reason_counts": quote_suppress_counts,
            "quote_cancel_reason_counts": quote_cancel_counts,
            "integrity_status": integrity_status,
            "integrity_mismatch_count": integrity_mismatch_count,
        },
    }


def assert_shutdown_validation(db_path: Path) -> dict[str, Any]:
    with closing(sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True)) as conn:
        conn.row_factory = sqlite3.Row
        total_trade_count = conn.execute(
            """
            SELECT COUNT(*) AS c
            FROM trades
            """
        ).fetchone()["c"]
        active_trades = conn.execute(
            """
            SELECT id, market_id, token_id, direction, status
            FROM trades
            WHERE status IN ('OPEN', 'CLOSING')
            ORDER BY id
            """
        ).fetchall()
        cleanup_rows = conn.execute(
            """
            SELECT trade_id, COUNT(*) AS fill_count
            FROM trade_fills
            WHERE phase = 'mark_exit' AND source = 'shutdown_cleanup'
            GROUP BY trade_id
            ORDER BY trade_id
            """
        ).fetchall()
        cleanup_trade_ids = [int(row["trade_id"]) for row in cleanup_rows]
        cleanup_trade_rows = []
        cleanup_entry_outbox_missing = []
        if cleanup_trade_ids:
            placeholders = ",".join("?" for _ in cleanup_trade_ids)
            cleanup_trade_rows = conn.execute(
                f"""
                SELECT id, status, exit_type, entry_price, exit_price
                FROM trades
                WHERE id IN ({placeholders})
                ORDER BY id
                """,
                cleanup_trade_ids,
            ).fetchall()
            cleanup_entry_outbox_missing = conn.execute(
                f"""
                SELECT tf.trade_id, tf.order_id
                FROM trade_fills AS tf
                LEFT JOIN order_outbox AS oo
                  ON oo.order_id = tf.order_id
                 AND oo.action = 'fill'
                WHERE tf.phase = 'entry'
                  AND tf.source = 'maker_sim'
                  AND tf.trade_id IN ({placeholders})
                  AND oo.order_id IS NULL
                ORDER BY tf.trade_id
                """,
                cleanup_trade_ids,
            ).fetchall()
        orphan_fills = conn.execute(
            """
            SELECT tf.trade_id, COUNT(*) AS fill_count
            FROM trade_fills AS tf
            LEFT JOIN trades AS t ON t.id = tf.trade_id
            WHERE t.id IS NULL
            GROUP BY tf.trade_id
            ORDER BY tf.trade_id
            """
        ).fetchall()

    cleanup_trade_count = len(cleanup_rows)
    cleanup_fill_count = sum(int(row["fill_count"] or 0) for row in cleanup_rows)
    active_trade_count = len(active_trades)
    orphan_fill_count = sum(int(row["fill_count"] or 0) for row in orphan_fills)
    cleanup_trade_map = {int(row["id"]): row for row in cleanup_trade_rows}
    summary = {
        "trade_count": int(total_trade_count or 0),
        "active_trade_count": active_trade_count,
        "open_trade_count": len([row for row in active_trades if row["status"] == "OPEN"]),
        "closing_trade_count": len([row for row in active_trades if row["status"] == "CLOSING"]),
        "active_trades": [
            {key: row[key] for key in row.keys()}
            for row in active_trades
        ],
        "shutdown_cleanup_trade_count": cleanup_trade_count,
        "shutdown_cleanup_fill_count": cleanup_fill_count,
        "shutdown_cleanup_trade_ids": cleanup_trade_ids,
        "shutdown_cleanup_trade_statuses": {
            str(trade_id): {
                "status": cleanup_trade_map[trade_id]["status"],
                "exit_type": cleanup_trade_map[trade_id]["exit_type"],
                "entry_price": cleanup_trade_map[trade_id]["entry_price"],
                "exit_price": cleanup_trade_map[trade_id]["exit_price"],
            }
            for trade_id in cleanup_trade_map
        },
        "shutdown_cleanup_entry_outbox_missing_trade_ids": [
            int(row["trade_id"]) for row in cleanup_entry_outbox_missing
        ],
        "shutdown_cleanup_required": active_trade_count > 0,
        "orphan_trade_fill_count": orphan_fill_count,
        "orphan_trade_fill_trade_ids": [int(row["trade_id"]) for row in orphan_fills],
    }

    failures: list[str] = []
    if active_trades:
        details = "; ".join(
            f"#{row['id']} {row['market_id']} {row['direction']} status={row['status']}"
            for row in active_trades
        )
        failures.append(f"active trades remain after shutdown: {details}")
    if any(row["status"] == "CLOSING" for row in active_trades):
        details = "; ".join(
            f"#{row['id']} {row['market_id']} {row['direction']} status={row['status']}"
            for row in active_trades
            if row["status"] == "CLOSING"
        )
        failures.append(f"stranded CLOSING trades remain after shutdown: {details}")
    if active_trades and cleanup_fill_count == 0:
        failures.append(
            "shutdown cleanup evidence missing: no trade_fills rows with phase='mark_exit' and source='shutdown_cleanup'"
        )
    if cleanup_trade_rows:
        mismatched_cleanup_trades = [
            row
            for row in cleanup_trade_rows
            if row["status"] != "CLOSED" or row["exit_type"] != "shutdown_cleanup"
        ]
        if mismatched_cleanup_trades:
            details = "; ".join(
                f"#{row['id']} status={row['status']} exit_type={row['exit_type']}"
                for row in mismatched_cleanup_trades
            )
            failures.append(
                "shutdown cleanup trades are not closed with shutdown_cleanup exit_type: "
                f"{details}"
            )
        if cleanup_entry_outbox_missing:
            details = ", ".join(
                f"trade_id={row['trade_id']} order_id={row['order_id']}"
                for row in cleanup_entry_outbox_missing
            )
            failures.append(
                "shutdown cleanup trades are missing matching maker entry outbox fill rows: "
                f"{details}"
            )
    if orphan_fill_count > 0:
        details = ", ".join(str(trade_id) for trade_id in summary["orphan_trade_fill_trade_ids"])
        failures.append(f"orphan trade_fills rows reference missing trades: trade_id(s) {details}")
    if failures:
        raise RuntimeError("shutdown validation failed:\n- " + "\n- ".join(failures))

    return summary


def _run_shutdown_validation_self_test() -> None:
    with tempfile.TemporaryDirectory(prefix="macbot_shutdown_validation_selftest_") as tmpdir:
        db_path = Path(tmpdir) / "selftest.sqlite"
        with sqlite3.connect(str(db_path)) as conn:
            conn.execute(
                """
                CREATE TABLE trades (
                    id INTEGER PRIMARY KEY,
                    market_id TEXT NOT NULL,
                    token_id TEXT NOT NULL,
                    direction TEXT NOT NULL,
                    entry_price REAL,
                    exit_price REAL,
                    status TEXT NOT NULL,
                    exit_type TEXT
                )
                """
            )
            conn.execute(
                """
                CREATE TABLE trade_fills (
                    id INTEGER PRIMARY KEY,
                    trade_id INTEGER NOT NULL,
                    order_id TEXT,
                    phase TEXT NOT NULL,
                    source TEXT NOT NULL
                )
                """
            )
            conn.execute(
                """
                CREATE TABLE order_outbox (
                    id INTEGER PRIMARY KEY,
                    timestamp TEXT NOT NULL,
                    stage TEXT NOT NULL,
                    market_id TEXT NOT NULL,
                    order_id TEXT,
                    token_id TEXT,
                    action TEXT NOT NULL,
                    status TEXT NOT NULL,
                    details TEXT
                )
                """
            )
            conn.execute(
                """
                CREATE TABLE trade_rejections (
                    id INTEGER PRIMARY KEY,
                    timestamp TEXT NOT NULL,
                    trade_id INTEGER,
                    strategy TEXT,
                    rejection_reason TEXT
                )
                """
            )
            conn.executemany(
                "INSERT INTO trades (id, market_id, token_id, direction, entry_price, exit_price, status, exit_type) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                [
                    (1, "m1", "t1", "BUY_YES", 0.25, 0.25, "CLOSED", "shutdown_cleanup"),
                    (2, "m2", "t2", "BUY_NO", 0.31, None, "CLOSING", None),
                ],
            )
            conn.executemany(
                "INSERT INTO trade_fills (id, trade_id, order_id, phase, source) VALUES (?, ?, ?, ?, ?)",
                [
                    (1, 1, "entry-1", "entry", "maker_sim"),
                    (2, 1, "mark-exit-1", "mark_exit", "shutdown_cleanup"),
                    (3, 999, "orphan-1", "entry", "maker_sim"),
                ],
            )
            conn.executemany(
                """
                INSERT INTO order_outbox (
                    id, timestamp, stage, market_id, order_id, token_id, action, status, details
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                [
                    (
                        1,
                        "2026-03-26T00:00:00Z",
                        "maker_quote",
                        "m1",
                        "entry-1",
                        "t1",
                        "fill",
                        "ok",
                        "opened=1 reason=entry",
                    ),
                ],
            )
            conn.commit()

        try:
            assert_shutdown_validation(db_path)
        except RuntimeError as exc:
            message = str(exc)
            if "stranded CLOSING trades remain after shutdown" not in message:
                raise
            if "orphan trade_fills rows reference missing trades" not in message:
                raise
        else:
            raise RuntimeError("shutdown validation self-test did not fail on stranded CLOSING and orphan fills")

        with sqlite3.connect(str(db_path)) as conn:
            conn.execute("UPDATE trades SET status = 'CLOSED' WHERE id = 2")
            conn.execute("DELETE FROM trade_fills WHERE trade_id = 999")
            conn.commit()

        summary = assert_shutdown_validation(db_path)
        if summary["active_trade_count"] != 0 or summary["orphan_trade_fill_count"] != 0:
            raise RuntimeError(f"shutdown validation self-test summary unexpected: {summary}")
        if summary["shutdown_cleanup_entry_outbox_missing_trade_ids"]:
            raise RuntimeError(
                "shutdown validation self-test unexpectedly reported missing entry outbox rows: "
                f"{summary['shutdown_cleanup_entry_outbox_missing_trade_ids']}"
            )
        integrity_report = run_validation_integrity_report(db_path)
        if integrity_report.get("status") != "ok" or integrity_report.get("mismatch_count") != 0:
            raise RuntimeError(f"validation integrity self-test unexpected report: {integrity_report}")


def _run_startup_failure_self_test() -> None:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        sock.bind(("127.0.0.1", 0))
        sock.listen(1)
        port = sock.getsockname()[1]
        try:
            preflight_bind_conflict(f"http://127.0.0.1:{port}")
        except SoakStartupFailure as exc:
            if exc.classification != "startup_bind_conflict":
                raise RuntimeError(f"unexpected preflight bind classification: {exc.classification}")
        else:
            raise RuntimeError("preflight bind conflict self-test did not raise")

    bind = classify_startup_failure(
        proc_returncode=1,
        elapsed_seconds=0.4,
        log_excerpt="Error: Address already in use (os error 48)",
    )
    if bind.classification != "startup_bind_conflict":
        raise RuntimeError(f"unexpected bind classification: {bind.classification}")

    exited = classify_startup_failure(
        proc_returncode=1,
        elapsed_seconds=0.2,
        log_excerpt="startup crashed before health",
    )
    if exited.classification != "startup_process_exited_before_health":
        raise RuntimeError(f"unexpected exit classification: {exited.classification}")

    timeout = classify_startup_failure(
        proc_returncode=None,
        elapsed_seconds=60.0,
        log_excerpt="",
        timeout=True,
    )
    if timeout.classification != "startup_no_health_timeout":
        raise RuntimeError(f"unexpected timeout classification: {timeout.classification}")


def main() -> int:
    args = parse_args()
    args.base_url = args.base_url.rstrip("/")
    if args.shutdown_validation_self_test:
        _run_shutdown_validation_self_test()
        print(json.dumps({"shutdown_validation_self_test": "ok"}))
        return 0
    if args.startup_failure_self_test:
        _run_startup_failure_self_test()
        print(json.dumps({"startup_failure_self_test": "ok"}))
        return 0
    root = Path(__file__).resolve().parent.parent
    config_path = Path(args.config).expanduser().resolve()
    db_path = Path(args.db).expanduser().resolve() if args.db else root / "artifacts/maker-soak-dbs" / f"frozen-{uuid.uuid4().hex}.sqlite"
    frozen_cfg = json.loads(config_path.read_text())
    if not math.isfinite(args.duration_minutes) or args.duration_minutes < 0:
        raise ValueError("duration must be finite and nonnegative")
    cargo_cmd = shlex.split(args.cargo_cmd)
    if not cargo_cmd:
        raise SystemExit("empty --cargo-cmd")
    report_hours = (
        args.report_hours
        if args.report_hours is not None
        else max(0.25, (max(0.0, args.duration_minutes) / 60.0) + 0.10)
    )

    if not math.isfinite(report_hours) or report_hours <= 0 or not math.isfinite(args.min_run_seconds) or args.min_run_seconds < 0:
        raise ValueError("report hours must be positive; minimum run seconds must be finite and nonnegative")

    with tempfile.TemporaryDirectory(prefix="macbot_frozen_maker_soak_") as tmpdir:
        frozen_snapshot_path = Path(tmpdir) / "frozen_config.json"
        with frozen_snapshot_path.open("x", encoding="utf-8") as stream:
            os.chmod(frozen_snapshot_path, 0o600)
            stream.write(json.dumps(frozen_cfg, allow_nan=False))
        maker_state_snapshot_path = Path(tmpdir) / "maker_state_snapshot.json"
        no_arb_snapshot_path = Path(tmpdir) / "no_arb_snapshot.json"

        env = paper_run_environment(frozen_cfg, frozen_snapshot_path, db_path, args.base_url)
        env.setdefault("MACBOT_L2_ANALYSIS_MARKET_STICKY_SECS", "180")

        actual_elapsed_minutes: float | None = None
        startup_failure: SoakStartupFailure | None = None
        preflight_quoteability: dict[str, Any] | None = None
        try:
            preflight_bind_conflict(args.base_url)
            proc, pump_thread, log_tail = _spawn_logged_process(cargo_cmd, cwd=root, env=env)
            wait_ready(proc, args.base_url, log_tail)
            try:
                maker_state_snapshot = fetch_json(f"{args.base_url}/api/maker/state", api_key=read_key(frozen_cfg))
                no_arb_snapshot = fetch_json(f"{args.base_url}/api/markets/no-arb", api_key=read_key(frozen_cfg))
                preflight_quoteability = summarize_preflight_quoteability(maker_state_snapshot)
                maker_state_snapshot_path.write_text(
                    json.dumps(maker_state_snapshot, indent=2),
                    encoding="utf-8",
                )
                no_arb_snapshot_path.write_text(
                    json.dumps(no_arb_snapshot, indent=2),
                    encoding="utf-8",
                )
            except Exception:
                preflight_quoteability = None
            soak_window = wait_for_soak_window(
                proc,
                args.duration_minutes,
                db_path,
                int(args.stop_on_open_trades or 0),
                float(args.min_run_seconds or 0.0),
            )
            actual_elapsed_minutes = soak_window["elapsed_seconds"] / 60.0
            try:
                maker_state_snapshot_path.write_text(
                json.dumps(fetch_json(f"{args.base_url}/api/maker/state", api_key=read_key(frozen_cfg)), indent=2),
                encoding="utf-8",
            )
            except Exception:
                maker_state_snapshot_path = None
            try:
                no_arb_snapshot_path.write_text(
                    json.dumps(fetch_json(f"{args.base_url}/api/markets/no-arb", api_key=read_key(frozen_cfg)), indent=2),
                    encoding="utf-8",
                )
            except Exception:
                no_arb_snapshot_path = None
        except SoakStartupFailure as exc:
            startup_failure = exc
        finally:
            if "proc" in locals():
                terminate_process(proc)
            if "pump_thread" in locals():
                pump_thread.join(timeout=2.0)

        if startup_failure is not None:
            payload = {
                "status": "startup_failure",
                "failure_class": startup_failure.classification,
                "failure_detail": startup_failure.detail,
                "elapsed_seconds": startup_failure.elapsed_seconds,
                "returncode": startup_failure.returncode,
                "config": str(config_path),
                "db": str(db_path),
            }
            if startup_failure.log_excerpt:
                payload["log_excerpt"] = startup_failure.log_excerpt
            print(json.dumps(payload, indent=2))
            return 1

        report = None
        verification_report = None
        shutdown_validation = None
        integrity_report = None
        terminal_classification = None
        post_run_error: Exception | None = None
        try:
            assert_configuration_unchanged(frozen_snapshot_path, frozen_cfg)
            report = run_report(
                db_path,
                report_hours,
                maker_state_snapshot_path,
                no_arb_snapshot_path,
            )
            report_meta = bind_report_configuration(report, frozen_cfg)
            report = json.loads(Path(report_meta["json"]).read_text())
            report["json"] = report_meta["json"]
            report["md"] = report_meta.get("md")
            verification_report = run_sim_verification_report(db_path)
            shutdown_validation = (
                assert_shutdown_validation(db_path) if args.shutdown_validation else None
            )
            integrity_report = run_validation_integrity_report(db_path)
            if integrity_report.get("status") != "ok":
                post_run_error = RuntimeError(
                    "validation DB integrity report found mismatches"
                )
            terminal_classification = classify_terminal_outcome(
                report,
                verification_report,
                shutdown_validation,
                integrity_report,
                post_run_error,
                preflight_quoteability,
            )
        except Exception as exc:
            post_run_error = exc
            terminal_classification = classify_terminal_outcome(
                report,
                verification_report,
                shutdown_validation,
                integrity_report,
                post_run_error,
                preflight_quoteability,
            )

    payload = {
        "config": str(config_path),
        "db": str(db_path),
        "requested_duration_minutes": float(args.duration_minutes),
        "actual_elapsed_minutes": actual_elapsed_minutes,
        "stop_on_open_trades": int(args.stop_on_open_trades or 0),
        "min_run_seconds": float(args.min_run_seconds or 0.0),
        "stop_on_open_target_reached": soak_window["stop_on_open_target_reached"] if "soak_window" in locals() else None,
        "peak_active_trade_count": soak_window["peak_active_trade_count"] if "soak_window" in locals() else None,
        "run_exit_cause": soak_window["exit_cause"] if "soak_window" in locals() else None,
        "preflight_quoteability": preflight_quoteability,
        "report": report,
        "verification_report": verification_report,
        "shutdown_validation": shutdown_validation,
        "integrity_report": integrity_report,
        "terminal_classification": terminal_classification,
        **configuration_evidence(frozen_cfg),
    }
    if terminal_classification is not None:
        payload["terminal_class"] = terminal_classification.get("class")
    if post_run_error is not None:
        payload["status"] = "post_run_failure"
        payload["failure_detail"] = str(post_run_error)
        payload["validation_bundle_artifacts"] = write_validation_bundle(payload, db_path)
        print(json.dumps(payload, indent=2))
        return 1

    payload["status"] = "ok"
    payload["validation_bundle_artifacts"] = write_validation_bundle(payload, db_path)
    print(json.dumps(payload, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
