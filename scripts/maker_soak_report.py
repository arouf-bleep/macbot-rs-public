#!/usr/bin/env python3
"""Summarize structured maker quote telemetry from order_outbox."""

from __future__ import annotations

import argparse
import json
import math
import sqlite3
import statistics
from collections import Counter, defaultdict
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Callable


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Build a maker-soak telemetry report from order_outbox")
    parser.add_argument("--db", default="trades.db", help="SQLite database path")
    parser.add_argument("--hours", type=float, default=24.0, help="Lookback window in hours")
    parser.add_argument(
        "--maker-state-snapshot",
        default=None,
        help="Optional path to a captured /api/maker/state JSON snapshot",
    )
    parser.add_argument(
        "--no-arb-snapshot",
        default=None,
        help="Optional path to a captured /api/markets/no-arb JSON snapshot",
    )
    parser.add_argument(
        "--outdir",
        default="artifacts/maker-soak-reports",
        help="Directory for JSON/Markdown reports",
    )
    return parser.parse_args()


def mean_or_none(values: list[float | int | None]) -> float | None:
    nums = [value for v in values if (value := as_float(v)) is not None]
    return statistics.fmean(nums) if nums else None


def median_or_none(values: list[float | int | None]) -> float | None:
    nums = [value for v in values if (value := as_float(v)) is not None]
    return statistics.median(nums) if nums else None


def p90_or_none(values: list[float | int | None]) -> float | None:
    nums = sorted(value for v in values if (value := as_float(v)) is not None)
    if not nums:
        return None
    idx = max(0, min(len(nums) - 1, round(0.9 * (len(nums) - 1))))
    return nums[idx]


def basis_bucket(value: float | None) -> str:
    value = as_float(value)
    if value is None:
        return "unknown"
    if value < -0.01:
        return "backwardation"
    if value > 0.01:
        return "contango"
    return "flat"


def session_bucket(hour_utc: int) -> str:
    if 0 <= hour_utc <= 7:
        return "asia"
    if 8 <= hour_utc <= 15:
        return "europe"
    return "us"


def token_bucket(value: Any) -> str:
    if value is True:
        return "yes"
    if value is False:
        return "no"
    return "unknown"


def as_float(value: Any) -> float | None:
    try:
        if value is None:
            return None
        number = float(value)
        return number if math.isfinite(number) else None
    except (TypeError, ValueError):
        return None


def quote_age_bucket(value: float | int | None) -> str:
    age_ms = as_float(value)
    if age_ms is None or age_ms < 0:
        return "unknown"
    if age_ms < 1_000:
        return "<1s"
    if age_ms < 3_000:
        return "1-3s"
    if age_ms < 10_000:
        return "3-10s"
    return ">=10s"


def price_to_beat_bucket(value: float | None) -> str:
    value = as_float(value)
    if value is None:
        return "unknown"
    pct = abs(value)
    if pct < 0.05:
        return "<0.05%"
    if pct < 0.15:
        return "0.05-0.15%"
    if pct < 0.30:
        return "0.15-0.30%"
    return ">=0.30%"


def roll_bucket(signal: float | None, confidence: float | None) -> str:
    signal = as_float(signal)
    confidence = as_float(confidence)
    if signal is None or confidence is None:
        return "unknown"
    if confidence < 0.15 or abs(signal) < 0.05:
        return "quiet"
    if signal > 0:
        return "buy_yes"
    if signal < 0:
        return "buy_no"
    return "quiet"


def event_roll_signal(event: dict[str, Any]) -> float | None:
    value = event.get("current_roll_signal")
    if value is None:
        value = event.get("roll_signal")
    return as_float(value)


def event_roll_confidence(event: dict[str, Any]) -> float | None:
    value = event.get("current_roll_confidence")
    if value is None:
        value = event.get("roll_confidence")
    return as_float(value)


def event_reason(event: dict[str, Any]) -> str:
    return str(event.get("reason") or "unknown")


def is_roll_guard_event(event: dict[str, Any]) -> bool:
    return "roll_guard(" in event_reason(event)


def load_optional_json(path_str: str | None) -> dict[str, Any] | None:
    if not path_str:
        return None
    path = Path(path_str).expanduser().resolve()
    if not path.exists():
        return None
    return json.loads(path.read_text(encoding="utf-8"))


def event_timestamp(event: dict[str, Any]) -> datetime | None:
    raw = (
        event.get("_timestamp")
        or event.get("observed_at")
        or event.get("placed_at")
    )
    if not raw:
        return None
    try:
        parsed = datetime.fromisoformat(str(raw).replace("Z", "+00:00"))
        return (parsed.replace(tzinfo=timezone.utc) if parsed.tzinfo is None else parsed).astimezone(timezone.utc)
    except ValueError:
        return None


def load_events(db_path: Path, hours: float) -> list[dict[str, Any]]:
    cutoff = datetime.now(timezone.utc) - timedelta(hours=hours)
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        rows = conn.execute(
            """
            SELECT timestamp, market_id, order_id, token_id, action, status, details
            FROM order_outbox
            WHERE stage = 'maker_quote_telemetry'
            ORDER BY id ASC
            """
        ).fetchall()
    finally:
        conn.close()

    events: list[dict[str, Any]] = []
    for row in rows:
        ts_raw = row["timestamp"]
        try:
            ts = datetime.fromisoformat(ts_raw.replace("Z", "+00:00"))
        except Exception:
            continue
        ts = (ts.replace(tzinfo=timezone.utc) if ts.tzinfo is None else ts).astimezone(timezone.utc)
        if ts < cutoff:
            continue
        try:
            payload = json.loads(row["details"])
        except (json.JSONDecodeError, TypeError):
            continue
        if not isinstance(payload, dict):
            continue
        payload["_timestamp"] = ts.isoformat()
        payload["_outbox_action"] = row["action"]
        payload["_outbox_status"] = row["status"]
        payload["_market_id"] = row["market_id"]
        payload["_order_id"] = row["order_id"]
        payload["_token_id"] = row["token_id"]
        events.append(payload)
    return events


def load_quote_actions(db_path: Path, hours: float) -> list[dict[str, Any]]:
    cutoff = datetime.now(timezone.utc) - timedelta(hours=hours)
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        rows = conn.execute(
            """
            SELECT timestamp, market_id, order_id, token_id, action, status, details
            FROM order_outbox
            WHERE stage = 'maker_quote'
            ORDER BY id ASC
            """
        ).fetchall()
    finally:
        conn.close()

    out: list[dict[str, Any]] = []
    for row in rows:
        ts_raw = row["timestamp"]
        try:
            ts = datetime.fromisoformat(ts_raw.replace("Z", "+00:00"))
        except Exception:
            continue
        ts = (ts.replace(tzinfo=timezone.utc) if ts.tzinfo is None else ts).astimezone(timezone.utc)
        if ts < cutoff:
            continue
        out.append(
            {
                "timestamp": ts.isoformat(),
                "market_id": row["market_id"],
                "order_id": row["order_id"],
                "token_id": row["token_id"],
                "action": row["action"],
                "status": row["status"],
                "details": row["details"],
            }
        )
    return out


def load_trade_counts(db_path: Path, hours: float | None = None) -> dict[str, int]:
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    try:
        cutoff = (datetime.now(timezone.utc) - timedelta(hours=hours)).isoformat() if hours is not None else None
        total_trades = int(conn.execute("SELECT COUNT(*) FROM trades WHERE ? IS NULL OR julianday(timestamp) >= julianday(?)", (cutoff, cutoff)).fetchone()[0] or 0)
        open_trades = int(
            conn.execute("SELECT COUNT(*) FROM trades WHERE status IN ('OPEN','CLOSING')").fetchone()[0] or 0
        )
        closed_trades = int(
            conn.execute("SELECT COUNT(*) FROM trades WHERE status = 'CLOSED' AND (? IS NULL OR julianday(timestamp) >= julianday(?))", (cutoff, cutoff)).fetchone()[0] or 0
        )
        fill_count = int(conn.execute("SELECT COUNT(*) FROM trade_fills WHERE ? IS NULL OR julianday(timestamp) >= julianday(?)", (cutoff, cutoff)).fetchone()[0] or 0)
    finally:
        conn.close()
    return {
        "trade_count": total_trades,
        "open_trade_count": open_trades,
        "closed_trade_count": closed_trades,
        "fill_count": fill_count,
    }


def quote_action_reason(details: Any) -> str:
    text = str(details or "").strip()
    if "reason=" in text:
        text = text.split("reason=", 1)[1].strip()
    return text or "unknown"


def policy_reason_bucket(reason: str) -> str:
    if reason.startswith("market_quality("):
        return "market_quality"
    if "maker BUY_NO" in reason:
        return "maker_buy_no"
    if reason.startswith("entry_ineligible("):
        return "entry_ineligible"
    if reason.startswith("inventory_cap("):
        return "inventory_cap"
    if reason == "no_inventory":
        return "no_inventory"
    if reason.startswith("roll_guard("):
        return "roll_guard"
    return reason or "unknown"


def trade_rejection_reason_bucket(reason: str) -> str:
    text = (reason or "").lower()
    if "analysis stale" in text:
        return "analysis_stale"
    if "market quality" in text or "market_quality" in text:
        return "market_quality"
    if "maker buy_no" in text or "buy_no" in text:
        return "maker_buy_no"
    if "entry_ineligible" in text:
        return "entry_ineligible"
    if "inventory_cap" in text or "inventory cap" in text:
        return "inventory_cap"
    if "roll_guard" in text:
        return "roll_guard"
    if "confidence" in text:
        return "confidence_floor"
    if "duplicate" in text:
        return "duplicate_entry"
    if "no_inventory" in text:
        return "no_inventory"
    return "other"


def top_counter_items(counter: Counter[str], limit: int = 5) -> list[dict[str, Any]]:
    total = sum(counter.values())
    if total <= 0:
        return []
    items: list[dict[str, Any]] = []
    for label, count in counter.most_common(limit):
        items.append(
            {
                "label": label,
                "count": int(count),
                "share_pct": round((count / total) * 100.0, 1),
            }
        )
    return items


def top_label(counter: Counter[str]) -> dict[str, Any] | None:
    items = top_counter_items(counter, 1)
    return items[0] if items else None


def summarize_quoteability_state(maker_l2: dict[str, Any]) -> dict[str, Any]:
    snapshot_available = bool(maker_l2.get("snapshot_available"))
    loaded_count = int(maker_l2.get("loaded_count") or 0)
    two_sided_count = int(maker_l2.get("two_sided_count") or 0)
    one_sided_count = int(maker_l2.get("one_sided_count") or 0)
    stale_count = int(maker_l2.get("stale_count") or 0)
    quoteable_count = int(maker_l2.get("quoteable_count") or 0)
    live_quote_count = maker_l2.get("live_quote_count")
    market_count = maker_l2.get("market_count")
    stale_reason = str(maker_l2.get("l2_stale_reason") or "").lower()

    evidence = {
        "snapshot_available": snapshot_available,
        "loaded_count": loaded_count,
        "two_sided_count": two_sided_count,
        "one_sided_count": one_sided_count,
        "stale_count": stale_count,
        "quoteable_count": quoteable_count,
        "live_quote_count": live_quote_count,
        "market_count": market_count,
        "l2_stale": maker_l2.get("l2_stale"),
        "l2_stale_reason": maker_l2.get("l2_stale_reason"),
    }

    if market_count in (0, None) and live_quote_count in (0, None) and stale_reason == "no target markets":
        return {
            "label": "no_target_markets",
            "bucket": "dead_window",
            "source": "run_context",
            "note": "No active markets were available, so the run never had quoteable targets.",
            "evidence": evidence,
        }
    if not snapshot_available or loaded_count == 0:
        return {
            "label": "snapshot_missing",
            "bucket": "low_quoteability",
            "source": "run_context",
            "note": "No L2 snapshot was captured, so quoteability could not be established.",
            "evidence": evidence,
        }
    if one_sided_count > 0 and two_sided_count == 0:
        return {
            "label": "one_sided_books",
            "bucket": "low_quoteability",
            "source": "run_context",
            "note": "Books were loaded, but they remained one-sided and never became safely quoteable.",
            "evidence": evidence,
        }
    if maker_l2.get("l2_stale") is True or stale_count > 0 or any(
        marker in stale_reason
        for marker in ("stale", "session error", "warmup", "rearm", "backpressure")
    ):
        return {
            "label": "stale_books",
            "bucket": "low_quoteability",
            "source": "run_context",
            "note": "L2 readiness stayed stale long enough that the run never reached dependable quoteability.",
            "evidence": evidence,
        }
    if quoteable_count <= 0:
        return {
            "label": "low_quoteability",
            "bucket": "low_quoteability",
            "source": "run_context",
            "note": "Books were present, but quoteability never crossed the useful threshold.",
            "evidence": evidence,
        }
    return {
        "label": "quoteable_books",
        "bucket": "quoteable",
        "source": "run_context",
        "note": "L2 books were available and quoteable.",
        "evidence": evidence,
    }


def summarize_primary_blocker(report: dict[str, Any], summary: dict[str, Any]) -> dict[str, Any]:
    blocker_summary = summary.get("blocker_summary") or {}
    run_context = report.get("run_context") or {}
    maker_l2 = run_context.get("maker_l2") or {}
    quoteability_state = maker_l2.get("quoteability_state") or summarize_quoteability_state(maker_l2)

    top_quote_suppress = blocker_summary.get("top_quote_suppress") or {}
    top_trade_rejection = blocker_summary.get("top_trade_rejection") or {}
    top_quote_cancel = blocker_summary.get("top_quote_cancel") or {}
    top_decision_reason = blocker_summary.get("top_decision_reason") or {}

    trade_count = int(summary.get("trade_count") or 0)
    event_count = int(report.get("event_count") or 0)
    quote_suppress_count = int(summary.get("quote_suppress_count") or 0)
    market_quality_count = int(summary.get("market_quality_suppress_count") or 0)
    maker_buy_no_count = int(summary.get("maker_buy_no_suppress_count") or 0)
    entry_ineligible_count = int(summary.get("entry_ineligible_suppress_count") or 0)
    recent_rejection_count = int(summary.get("trade_rejection_count_recent") or 0)
    market_count = maker_l2.get("market_count")
    live_quote_count = maker_l2.get("live_quote_count")
    stale_reason = maker_l2.get("l2_stale_reason")
    snapshot_available = bool(maker_l2.get("snapshot_available"))
    loaded_count = int(maker_l2.get("loaded_count") or 0)
    two_sided_count = int(maker_l2.get("two_sided_count") or 0)
    one_sided_count = int(maker_l2.get("one_sided_count") or 0)
    stale_count = int(maker_l2.get("stale_count") or 0)

    def blocker_payload(
        label: str,
        source: str,
        evidence: dict[str, Any] | None = None,
        note: str | None = None,
    ) -> dict[str, Any]:
        payload = {
            "label": label,
            "source": source,
            "evidence": evidence,
        }
        if note is not None:
            payload["note"] = note
        return payload

    if (
        trade_count == 0
        and market_count in (0, None)
        and live_quote_count in (0, None)
        and stale_reason == "no target markets"
    ):
        return blocker_payload(
            label="no_target_markets",
            source="run_context",
            note="No active markets were available, so the run never exercised the quote path.",
            evidence={
                "event_count": event_count,
                "market_count": market_count,
                "live_quote_count": live_quote_count,
                "l2_stale_reason": stale_reason,
            },
        )

    if trade_count == 0:
        if market_quality_count > 0 and market_quality_count >= max(entry_ineligible_count, maker_buy_no_count, 1):
            return blocker_payload(
                label="market_quality",
                source="quote_suppress",
                evidence=top_quote_suppress or top_quote_cancel,
                note="Market-quality suppressions dominated the zero-trade run.",
            )
        if entry_ineligible_count > 0 or maker_buy_no_count > 0:
            dominant = top_quote_suppress or top_trade_rejection or top_decision_reason
            return blocker_payload(
                label="policy_suppression",
                source="quote_suppress",
                evidence=dominant,
                note="Entry policy suppressions dominated the zero-trade run.",
            )
        if quoteability_state.get("label") == "one_sided_books":
            return blocker_payload(
                label="one_sided_books",
                source="run_context",
                note=quoteability_state.get("note")
                or "Books were loaded but remained one-sided, so the run never had quoteable two-sided markets.",
                evidence=quoteability_state.get("evidence"),
            )
        if quoteability_state.get("label") == "stale_books":
            return blocker_payload(
                label="stale_books",
                source="run_context",
                note=quoteability_state.get("note")
                or "Feed/session readiness stayed stale long enough that the run never reached quoteable books.",
                evidence=quoteability_state.get("evidence"),
            )
        if quoteability_state.get("label") == "snapshot_missing":
            return blocker_payload(
                label="snapshot_missing",
                source="run_context",
                note=quoteability_state.get("note")
                or "No L2 snapshot was captured, so quoteability could not be established.",
                evidence=quoteability_state.get("evidence"),
            )
        if recent_rejection_count > 0:
            return blocker_payload(
                label="trade_rejections",
                source="trade_rejection",
                evidence=top_trade_rejection,
                note="Recent trade rejections were present even though no trades landed.",
            )
        if quote_suppress_count > 0:
            dominant = top_quote_suppress or top_quote_cancel or top_decision_reason
            return blocker_payload(
                label="suppression_pressure",
                source="quote_suppress",
                evidence=dominant,
            note="The run suppressed heavily, but no single blocker bucket dominated clearly.",
        )
        return blocker_payload(
            label="inactive",
            source="summary",
            note="The run produced no trades and no meaningful suppressions or rejections.",
            evidence={
                "trade_count": trade_count,
                "event_count": event_count,
            },
        )

    if trade_count <= 4:
        dominant_label = None
        dominant_source = "summary"
        dominant_evidence: dict[str, Any] | None = None
        for label, source, evidence in (
            (top_quote_suppress.get("label"), "quote_suppress", top_quote_suppress),
            (top_trade_rejection.get("label"), "trade_rejection", top_trade_rejection),
            (top_quote_cancel.get("label"), "quote_cancel", top_quote_cancel),
            (top_decision_reason.get("label"), "decision_reason", top_decision_reason),
        ):
            if label:
                dominant_label = str(label)
                dominant_source = source
                dominant_evidence = evidence
                break
        return blocker_payload(
            label=dominant_label or "low_activity",
            source=dominant_source,
            evidence=dominant_evidence or {
                "trade_count": trade_count,
                "event_count": event_count,
                "quote_suppress_count": quote_suppress_count,
            },
            note="The run traded, but sample size is still sparse enough that it should be treated as low-activity plumbing evidence.",
        )

    return blocker_payload(
        label="active",
        source="summary",
        evidence={
            "trade_count": trade_count,
            "event_count": event_count,
            "quote_suppress_count": quote_suppress_count,
        },
        note="Trading activity was present and no single plumbing blocker dominated the run.",
    )


def summarize_run_classification(report: dict[str, Any], summary: dict[str, Any]) -> dict[str, Any]:
    primary_blocker = summarize_primary_blocker(report, summary)
    trade_count = int(summary.get("trade_count") or 0)
    event_count = int(report.get("event_count") or 0)
    quote_suppress_count = int(summary.get("quote_suppress_count") or 0)
    market_quality_count = int(summary.get("market_quality_suppress_count") or 0)
    entry_ineligible_count = int(summary.get("entry_ineligible_suppress_count") or 0)
    maker_buy_no_count = int(summary.get("maker_buy_no_suppress_count") or 0)
    recent_rejection_count = int(summary.get("trade_rejection_count_recent") or 0)

    if primary_blocker["label"] == "no_target_markets":
        label = "dead_window"
        note = primary_blocker.get("note")
    elif primary_blocker["label"] in {"one_sided_books", "stale_books", "snapshot_missing", "low_quoteability"}:
        label = "low_quoteability_limited"
        note = primary_blocker.get("note")
    elif primary_blocker["label"] == "market_quality":
        label = "market_quality_limited"
        note = primary_blocker.get("note")
    elif primary_blocker["label"] in {"policy_suppression", "trade_rejections", "suppression_pressure"}:
        label = "policy_limited"
        note = primary_blocker.get("note")
    elif trade_count == 0:
        label = "inactive"
        note = primary_blocker.get("note")
    elif trade_count <= 4:
        label = "low_activity"
        note = primary_blocker.get("note")
    else:
        label = "active"
        note = primary_blocker.get("note")

    return {
        "label": label,
        "note": note,
        "trade_count": trade_count,
        "event_count": event_count,
        "quote_suppress_count": quote_suppress_count,
        "market_quality_suppress_count": market_quality_count,
        "entry_ineligible_suppress_count": entry_ineligible_count,
        "maker_buy_no_suppress_count": maker_buy_no_count,
        "trade_rejection_count_recent": recent_rejection_count,
    }


def summarize_no_trade_diagnosis(report: dict[str, Any], summary: dict[str, Any]) -> dict[str, Any]:
    run_classification = report.get("run_classification") or {}
    primary_blocker = report.get("primary_blocker_summary") or {}
    run_context = report.get("run_context") or {}
    maker_l2 = run_context.get("maker_l2") or {}
    quoteability_state = maker_l2.get("quoteability_state") or summarize_quoteability_state(maker_l2)

    trade_count = int(summary.get("trade_count") or 0)
    event_count = int(report.get("event_count") or 0)
    quote_suppress_count = int(summary.get("quote_suppress_count") or 0)
    market_quality_count = int(summary.get("market_quality_suppress_count") or 0)
    maker_buy_no_count = int(summary.get("maker_buy_no_suppress_count") or 0)
    entry_ineligible_count = int(summary.get("entry_ineligible_suppress_count") or 0)
    trade_rejection_recent = int(summary.get("trade_rejection_count_recent") or 0)

    evidence = {
        "trade_count": trade_count,
        "event_count": event_count,
        "market_count": maker_l2.get("market_count"),
        "loaded_count": maker_l2.get("loaded_count"),
        "two_sided_count": maker_l2.get("two_sided_count"),
        "one_sided_count": maker_l2.get("one_sided_count"),
        "stale_count": maker_l2.get("stale_count"),
        "quoteable_count": maker_l2.get("quoteable_count"),
        "l2_stale": maker_l2.get("l2_stale"),
        "l2_stale_reason": maker_l2.get("l2_stale_reason"),
        "quote_suppress_count": quote_suppress_count,
        "market_quality_suppress_count": market_quality_count,
        "maker_buy_no_suppress_count": maker_buy_no_count,
        "entry_ineligible_suppress_count": entry_ineligible_count,
        "trade_rejection_count_recent": trade_rejection_recent,
        "primary_blocker_label": primary_blocker.get("label"),
        "run_classification_label": run_classification.get("label"),
        "quoteability_state": quoteability_state,
    }

    if trade_count == 0:
        label = str(primary_blocker.get("label") or run_classification.get("label") or "inactive")
        if label == "no_target_markets":
            bucket = "dead_window"
            note = quoteability_state.get("note") or "No active markets were available, so the run never exercised the quote path."
            source = "run_context"
        elif label in {"one_sided_books", "stale_books", "snapshot_missing", "low_quoteability"}:
            bucket = "low_quoteability"
            note = quoteability_state.get("note") or primary_blocker.get("note") or "Feed/session readiness kept the books from becoming safely quoteable."
            source = "run_context"
        elif label == "market_quality":
            bucket = "market_quality"
            note = primary_blocker.get("note") or "Market-quality suppressions dominated the zero-trade run."
            source = "quote_suppress"
        elif label in {"policy_suppression", "trade_rejections", "suppression_pressure"}:
            bucket = "policy"
            note = primary_blocker.get("note") or "Policy gates or inventory constraints suppressed quoting."
            source = primary_blocker.get("source") or "quote_suppress"
        else:
            bucket = "inactive"
            note = primary_blocker.get("note") or "The run produced no trades and no dominant blocker surfaced."
            source = primary_blocker.get("source") or "summary"
        return {
            "bucket": bucket,
            "label": label,
            "source": source,
            "note": note,
            "evidence": evidence,
        }

    if trade_count <= 4:
        label = str(run_classification.get("label") or primary_blocker.get("label") or "low_activity")
        bucket = "low_activity"
        note = primary_blocker.get("note") or "The run traded, but sample size is still sparse enough to treat it as plumbing evidence."
        return {
            "bucket": bucket,
            "label": label,
            "source": primary_blocker.get("source") or "summary",
            "note": note,
            "evidence": evidence,
        }

    return {
        "bucket": "active",
        "label": str(run_classification.get("label") or "active"),
        "source": primary_blocker.get("source") or "summary",
        "note": primary_blocker.get("note") or "Trading activity was present and no single plumbing blocker dominated the run.",
        "evidence": evidence,
    }


def summarize_group(events: list[dict[str, Any]]) -> dict[str, Any]:
    place_like = [e for e in events if e.get("event") in {"place", "guard_skip", "guard_shadow"}]
    cancel_events = [e for e in events if e.get("event") == "cancel"]
    widen_events = [e for e in place_like if e.get("basis_widen_applied")]
    roll_guard_skip_events = [e for e in events if e.get("event") == "guard_skip" and is_roll_guard_event(e)]
    roll_guard_shadow_events = [e for e in events if e.get("event") == "guard_shadow" and is_roll_guard_event(e)]
    return {
        "count": len(events),
        "place_count": sum(1 for e in events if e.get("event") == "place"),
        "cancel_count": len(cancel_events),
        "guard_skip_count": sum(1 for e in events if e.get("event") == "guard_skip"),
        "guard_shadow_count": sum(1 for e in events if e.get("event") == "guard_shadow"),
        "roll_guard_skip_count": len(roll_guard_skip_events),
        "roll_guard_shadow_count": len(roll_guard_shadow_events),
        "basis_widen_count": len(widen_events),
        "avg_basis_widen_bps": mean_or_none([e.get("basis_widen_bps") for e in widen_events]),
        "avg_basis_pct": mean_or_none([e.get("basis_pct") for e in place_like]),
        "avg_price_to_beat_distance_pct": mean_or_none(
            [e.get("price_to_beat_distance_pct") for e in place_like]
        ),
        "avg_spread_bps": mean_or_none([e.get("placed_spread_bps") for e in place_like]),
        "avg_quote_age_ms": mean_or_none([e.get("quote_age_ms") for e in cancel_events]),
        "median_quote_age_ms": median_or_none([e.get("quote_age_ms") for e in cancel_events]),
        "avg_adverse_move_bps": mean_or_none([e.get("adverse_move_bps") for e in cancel_events]),
        "p90_adverse_move_bps": p90_or_none([e.get("adverse_move_bps") for e in cancel_events]),
        "avg_signed_quote_move_bps": mean_or_none(
            [e.get("signed_quote_move_bps") for e in cancel_events]
        ),
        "avg_roll_signal": mean_or_none([event_roll_signal(e) for e in events]),
        "avg_roll_confidence": mean_or_none([event_roll_confidence(e) for e in events]),
    }


def grouped_summary(
    events: list[dict[str, Any]],
    key_fn: Callable[[dict[str, Any]], str],
) -> dict[str, dict[str, Any]]:
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for event in events:
        grouped[key_fn(event)].append(event)
    return {
        key: summarize_group(group_events)
        for key, group_events in sorted(grouped.items(), key=lambda item: item[0])
    }


def summarize_snapshot_context(
    maker_state_snapshot: dict[str, Any] | None,
    no_arb_snapshot: dict[str, Any] | None,
) -> dict[str, Any]:
    maker_snapshot = maker_state_snapshot or {}
    maker = maker_snapshot.get("maker", maker_snapshot)
    roll_rows = maker.get("roll_microstructure") or []
    strongest_roll = roll_rows[0] if roll_rows else None
    l2_summary = maker.get("l2_summary") or {}
    l2_rows = maker.get("l2_top_of_book") or []
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
    stale_count = int(l2_summary.get("stale_count") or sum(
        1 for row in l2_rows if row.get("stale")
    ))
    one_sided_count = int(l2_summary.get("one_sided_count") or sum(
        1
        for row in l2_rows
        if row.get("snapshot_loaded")
        and not row.get("stale")
        and ((row.get("best_bid") is None) != (row.get("best_ask") is None))
    ))
    declared_quoteable = l2_summary.get("quoteable_count")
    quoteable_count = int(declared_quoteable) if declared_quoteable is not None else two_sided_count

    return {
        "maker_roll": {
            "entry_count": len(roll_rows),
            "strongest_signal": strongest_roll.get("signal") if strongest_roll else None,
            "strongest_confidence": strongest_roll.get("confidence") if strongest_roll else None,
            "strongest_persistence": strongest_roll.get("persistence") if strongest_roll else None,
            "strongest_token_id": strongest_roll.get("token_id") if strongest_roll else None,
        },
        "maker_l2": {
            "snapshot_available": maker_state_snapshot is not None,
            "entry_count": len(l2_rows),
            "loaded_count": loaded_count,
            "two_sided_count": two_sided_count,
            "one_sided_count": one_sided_count,
            "stale_count": stale_count,
            "quoteable_count": quoteable_count,
            "live_quote_count": maker.get("quote_count"),
            "market_count": maker.get("market_count"),
            "l2_stale": maker.get("l2_stale"),
            "l2_stale_reason": maker.get("l2_stale_reason"),
            "quoteability_state": summarize_quoteability_state(
                {
                    "snapshot_available": maker_state_snapshot is not None,
                    "loaded_count": loaded_count,
                    "two_sided_count": two_sided_count,
                    "one_sided_count": one_sided_count,
                    "stale_count": stale_count,
                    "quoteable_count": quoteable_count,
                    "live_quote_count": maker.get("quote_count"),
                    "market_count": maker.get("market_count"),
                    "l2_stale": maker.get("l2_stale"),
                    "l2_stale_reason": maker.get("l2_stale_reason"),
                }
            ),
        },
        "maker_inventory": {
            "market_count": len(maker.get("inventory_by_market") or {}),
            "markets_with_position": sum(
                1
                for inv in (maker.get("inventory_by_market") or {}).values()
                if any(
                    abs(v) > 1e-9
                    for v in [
                        as_float(inv.get("net_notional_usd")) if isinstance(inv, dict) else None,
                        as_float(inv.get("net_yes_shares")) if isinstance(inv, dict) else None,
                        as_float(inv.get("net_no_shares")) if isinstance(inv, dict) else None,
                    ]
                    if v is not None
                )
            ),
        },
        "no_arb": {
            "status": (no_arb_snapshot or {}).get("status"),
            "group_count": (no_arb_snapshot or {}).get("group_count"),
            "comparable_group_count": (no_arb_snapshot or {}).get("comparable_group_count"),
            "violation_count": (no_arb_snapshot or {}).get("violation_count"),
            "max_violation_cents": (no_arb_snapshot or {}).get("max_violation_cents"),
            "worst_group_end_date": ((no_arb_snapshot or {}).get("worst_group") or {}).get("end_date"),
            "worst_group_market_count": ((no_arb_snapshot or {}).get("worst_group") or {}).get("market_count"),
        },
    }


def load_trade_rejection_stats(db_path: Path, hours: float) -> dict[str, Any]:
    cutoff = datetime.now(timezone.utc) - timedelta(hours=hours)
    cutoff_str = cutoff.isoformat()
    conn = sqlite3.connect(Path(db_path).resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        total = int(conn.execute("SELECT COUNT(*) FROM trade_rejections").fetchone()[0] or 0)
        recent_rows = conn.execute(
            """
            SELECT rejection_reason, strategy
            FROM trade_rejections
            WHERE julianday(timestamp) >= julianday(?)
            ORDER BY id ASC
            """,
            (cutoff_str,),
        ).fetchall()
    except sqlite3.OperationalError:
        return {
            "total_count": 0,
            "recent_count": 0,
            "recent_reason_counts": {},
            "recent_bucket_counts": {},
            "recent_strategy_counts": {},
            "recent_reason_top": [],
            "recent_bucket_top": [],
            "recent_strategy_top": [],
        }
    finally:
        conn.close()

    recent_reasons = [str(row["rejection_reason"] or "") for row in recent_rows]
    recent_strategies = [str(row["strategy"] or "unknown") for row in recent_rows]
    recent_bucket_counts = Counter(trade_rejection_reason_bucket(reason) for reason in recent_reasons)
    recent_reason_counts = Counter(recent_reasons)
    recent_strategy_counts = Counter(recent_strategies)
    return {
        "total_count": total,
        "recent_count": len(recent_rows),
        "recent_reason_counts": dict(recent_reason_counts),
        "recent_bucket_counts": dict(recent_bucket_counts),
        "recent_strategy_counts": dict(recent_strategy_counts),
        "recent_reason_top": top_counter_items(recent_reason_counts, 5),
        "recent_bucket_top": top_counter_items(recent_bucket_counts, 5),
        "recent_strategy_top": top_counter_items(recent_strategy_counts, 5),
    }


def summarize(
    events: list[dict[str, Any]],
    quote_actions: list[dict[str, Any]],
    db_path: Path,
    hours: float,
    maker_state_snapshot: dict[str, Any] | None = None,
    no_arb_snapshot: dict[str, Any] | None = None,
) -> dict[str, Any]:
    trade_counts = load_trade_counts(db_path, hours)
    trade_rejections = load_trade_rejection_stats(db_path, hours)
    run_context = summarize_snapshot_context(maker_state_snapshot, no_arb_snapshot)
    place_events = [e for e in events if e.get("event") == "place"]
    cancel_events = [e for e in events if e.get("event") == "cancel"]
    guard_skip_events = [e for e in events if e.get("event") == "guard_skip"]
    guard_shadow_events = [e for e in events if e.get("event") == "guard_shadow"]
    roll_guard_skip_events = [e for e in guard_skip_events if is_roll_guard_event(e)]
    roll_guard_shadow_events = [e for e in guard_shadow_events if is_roll_guard_event(e)]
    decision_events = place_events + guard_skip_events + guard_shadow_events
    suppress_actions = [row for row in quote_actions if row.get("action") == "suppress"]
    quote_cancel_actions = [row for row in quote_actions if row.get("action") == "cancel"]
    suppress_reasons = Counter(quote_action_reason(row.get("details")) for row in suppress_actions)
    suppress_reason_counts = Counter(
        policy_reason_bucket(quote_action_reason(row.get("details"))) for row in suppress_actions
    )
    quote_cancel_reason_counts = Counter(
        policy_reason_bucket(quote_action_reason(row.get("details")))
        for row in quote_cancel_actions
    )
    quote_cancel_raw_reasons = Counter(quote_action_reason(row.get("details")) for row in quote_cancel_actions)
    decision_reason_counts = Counter(event_reason(e) for e in decision_events)
    top_trade_rejection = trade_rejections["recent_reason_top"][0] if trade_rejections["recent_reason_top"] else None

    side_counts = Counter(str(e.get("side", "unknown")).lower() for e in events)
    reason_counts = Counter(str(e.get("reason", "none")) for e in cancel_events)
    event_counts = Counter(str(e.get("event", "unknown")) for e in events)

    basis_stats = grouped_summary(cancel_events, lambda e: basis_bucket(e.get("basis_pct")))
    side_stats = grouped_summary(events, lambda e: str(e.get("side", "unknown")).lower())
    token_stats = grouped_summary(events, lambda e: token_bucket(e.get("is_yes_token")))
    session_stats = grouped_summary(
        cancel_events,
        lambda e: session_bucket((event_timestamp(e) or datetime.now(timezone.utc)).hour),
    )
    hour_stats = grouped_summary(
        cancel_events,
        lambda e: f"{(event_timestamp(e) or datetime.now(timezone.utc)).hour:02d}",
    )
    price_to_beat_stats = grouped_summary(
        events,
        lambda e: price_to_beat_bucket(e.get("price_to_beat_distance_pct")),
    )
    quote_age_stats = grouped_summary(
        cancel_events,
        lambda e: quote_age_bucket(e.get("quote_age_ms")),
    )
    roll_stats = grouped_summary(
        events,
        lambda e: roll_bucket(event_roll_signal(e), event_roll_confidence(e)),
    )

    run_classification = summarize_run_classification(
        {
            "event_count": len(events),
            "run_context": run_context,
        },
        {
            "trade_count": trade_counts["trade_count"],
            "open_trade_count": trade_counts["open_trade_count"],
            "closed_trade_count": trade_counts["closed_trade_count"],
            "fill_count": trade_counts["fill_count"],
            "trade_rejection_count_total": trade_rejections["total_count"],
            "trade_rejection_count_recent": trade_rejections["recent_count"],
            "quote_suppress_count": len(suppress_actions),
            "market_quality_suppress_count": suppress_reason_counts.get("market_quality", 0),
            "maker_buy_no_suppress_count": suppress_reason_counts.get("maker_buy_no", 0),
            "entry_ineligible_suppress_count": suppress_reason_counts.get("entry_ineligible", 0),
            "blocker_summary": {
                "top_quote_suppress": top_label(suppress_reasons),
                "top_trade_rejection": top_trade_rejection,
                "top_quote_cancel": top_label(quote_cancel_raw_reasons),
                "top_decision_reason": top_label(decision_reason_counts),
            },
        },
    )
    primary_blocker = summarize_primary_blocker(
        {
            "event_count": len(events),
            "run_context": run_context,
        },
        {
            "trade_count": trade_counts["trade_count"],
            "open_trade_count": trade_counts["open_trade_count"],
            "closed_trade_count": trade_counts["closed_trade_count"],
            "fill_count": trade_counts["fill_count"],
            "trade_rejection_count_total": trade_rejections["total_count"],
            "trade_rejection_count_recent": trade_rejections["recent_count"],
            "quote_suppress_count": len(suppress_actions),
            "market_quality_suppress_count": suppress_reason_counts.get("market_quality", 0),
            "maker_buy_no_suppress_count": suppress_reason_counts.get("maker_buy_no", 0),
            "entry_ineligible_suppress_count": suppress_reason_counts.get("entry_ineligible", 0),
            "blocker_summary": {
                "top_quote_suppress": top_label(suppress_reasons),
                "top_trade_rejection": top_trade_rejection,
                "top_quote_cancel": top_label(quote_cancel_raw_reasons),
                "top_decision_reason": top_label(decision_reason_counts),
            },
        },
    )
    no_trade_diagnosis = summarize_no_trade_diagnosis(
        {
            "event_count": len(events),
            "run_context": run_context,
            "run_classification": run_classification,
            "primary_blocker_summary": primary_blocker,
        },
        {
            "trade_count": trade_counts["trade_count"],
            "open_trade_count": trade_counts["open_trade_count"],
            "closed_trade_count": trade_counts["closed_trade_count"],
            "fill_count": trade_counts["fill_count"],
            "trade_rejection_count_total": trade_rejections["total_count"],
            "trade_rejection_count_recent": trade_rejections["recent_count"],
            "quote_suppress_count": len(suppress_actions),
            "market_quality_suppress_count": suppress_reason_counts.get("market_quality", 0),
            "maker_buy_no_suppress_count": suppress_reason_counts.get("maker_buy_no", 0),
            "entry_ineligible_suppress_count": suppress_reason_counts.get("entry_ineligible", 0),
            "blocker_summary": {
                "top_quote_suppress": top_label(suppress_reasons),
                "top_trade_rejection": top_trade_rejection,
                "top_quote_cancel": top_label(quote_cancel_raw_reasons),
                "top_decision_reason": top_label(decision_reason_counts),
            },
        },
    )

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "db_path": str(db_path),
        "lookback_hours": hours,
        "event_count": len(events),
        "place_event_count": len(place_events),
        "cancel_event_count": len(cancel_events),
        "guard_skip_event_count": len(guard_skip_events),
        "guard_shadow_event_count": len(guard_shadow_events),
        "side_counts": dict(side_counts),
        "event_counts": dict(event_counts),
        "cancel_reason_counts": dict(reason_counts),
        "quote_suppress_reason_counts": dict(suppress_reason_counts),
        "quote_cancel_reason_counts": dict(quote_cancel_reason_counts),
        "summary": {
            "trade_count": trade_counts["trade_count"],
            "open_trade_count": trade_counts["open_trade_count"],
            "closed_trade_count": trade_counts["closed_trade_count"],
            "fill_count": trade_counts["fill_count"],
            "trade_rejection_count_total": trade_rejections["total_count"],
            "trade_rejection_count_recent": trade_rejections["recent_count"],
            "roll_guard_skip_count_at_decision": len(roll_guard_skip_events),
            "roll_guard_shadow_count_at_decision": len(roll_guard_shadow_events),
            "quote_suppress_count": len(suppress_actions),
            "market_quality_suppress_count": suppress_reason_counts.get("market_quality", 0),
            "maker_buy_no_suppress_count": suppress_reason_counts.get("maker_buy_no", 0),
            "entry_ineligible_suppress_count": suppress_reason_counts.get("entry_ineligible", 0),
            "trade_rejection_reason_top": trade_rejections["recent_reason_top"],
            "trade_rejection_bucket_top": trade_rejections["recent_bucket_top"],
            "trade_rejection_strategy_top": trade_rejections["recent_strategy_top"],
            "quote_suppress_subreason_top": top_counter_items(suppress_reasons, 5),
            "quote_cancel_subreason_top": top_counter_items(quote_cancel_raw_reasons, 5),
            "decision_reason_top": top_counter_items(decision_reason_counts, 5),
            "blocker_summary": {
                "top_quote_suppress": top_label(suppress_reasons),
                "top_trade_rejection": top_trade_rejection,
                "top_quote_cancel": top_label(quote_cancel_raw_reasons),
                "top_decision_reason": top_label(decision_reason_counts),
            },
            "avg_basis_pct_at_place": mean_or_none([e.get("basis_pct") for e in place_events]),
            "avg_basis_pct_at_decision": mean_or_none([e.get("basis_pct") for e in decision_events]),
            "avg_basis_pct_at_guard_skip": mean_or_none([e.get("basis_pct") for e in guard_skip_events]),
            "basis_widen_count_at_place": sum(1 for e in place_events if e.get("basis_widen_applied")),
            "basis_widen_count_at_decision": sum(
                1 for e in decision_events if e.get("basis_widen_applied")
            ),
            "avg_basis_widen_bps_at_decision": mean_or_none(
                [e.get("basis_widen_bps") for e in decision_events if e.get("basis_widen_applied")]
            ),
            "non_null_basis_place_count": sum(1 for e in place_events if e.get("basis_pct") is not None),
            "non_null_basis_decision_count": sum(
                1 for e in decision_events if e.get("basis_pct") is not None
            ),
            "avg_price_to_beat_distance_pct_at_place": mean_or_none(
                [e.get("price_to_beat_distance_pct") for e in place_events]
            ),
            "avg_price_to_beat_distance_pct_at_decision": mean_or_none(
                [e.get("price_to_beat_distance_pct") for e in decision_events]
            ),
            "avg_target_spread_bps_at_place": mean_or_none(
                [e.get("target_spread_bps") for e in place_events]
            ),
            "avg_target_spread_bps_at_decision": mean_or_none(
                [e.get("target_spread_bps") for e in decision_events]
            ),
            "avg_observed_market_spread_bps_at_place": mean_or_none(
                [e.get("placed_spread_bps") for e in place_events]
            ),
            # Backward-compatible alias for older consumers; this is observed market spread, not our quote width.
            "avg_spread_bps_at_place": mean_or_none([e.get("placed_spread_bps") for e in place_events]),
            "non_null_observed_market_spread_place_count": sum(
                1 for e in place_events if e.get("placed_spread_bps") is not None
            ),
            "avg_quote_age_ms": mean_or_none([e.get("quote_age_ms") for e in cancel_events]),
            "median_quote_age_ms": median_or_none([e.get("quote_age_ms") for e in cancel_events]),
            "avg_adverse_move_bps": mean_or_none([e.get("adverse_move_bps") for e in cancel_events]),
            "p90_adverse_move_bps": p90_or_none([e.get("adverse_move_bps") for e in cancel_events]),
            "avg_signed_quote_move_bps": mean_or_none(
                [e.get("signed_quote_move_bps") for e in cancel_events]
            ),
            "avg_basis_delta_pct": mean_or_none([e.get("basis_delta_pct") for e in cancel_events]),
            "avg_btc_move_bps": mean_or_none([e.get("btc_move_bps") for e in cancel_events]),
            "avg_roll_signal_at_decision": mean_or_none([event_roll_signal(e) for e in decision_events]),
            "avg_roll_confidence_at_decision": mean_or_none(
                [event_roll_confidence(e) for e in decision_events]
            ),
            "avg_roll_signal_on_cancel": mean_or_none([event_roll_signal(e) for e in cancel_events]),
            "avg_roll_confidence_on_cancel": mean_or_none(
                [event_roll_confidence(e) for e in cancel_events]
            ),
            "trade_per_place_rate": (
                trade_counts["trade_count"] / len(place_events) if place_events else None
            ),
            "trade_per_event_rate": (trade_counts["trade_count"] / len(events)) if events else None,
            "fill_per_trade_rate": (
                trade_counts["fill_count"] / trade_counts["trade_count"]
                if trade_counts["trade_count"]
                else None
            ),
        },
        "run_classification": run_classification,
        "no_trade_diagnosis": no_trade_diagnosis,
        "primary_blocker_summary": primary_blocker,
        "run_context": run_context,
        "basis_buckets": basis_stats,
        "side_buckets": side_stats,
        "token_buckets": token_stats,
        "session_buckets": session_stats,
        "hour_buckets": hour_stats,
        "price_to_beat_buckets": price_to_beat_stats,
        "quote_age_buckets": quote_age_stats,
        "roll_buckets": roll_stats,
        "recent_events": events[-12:],
    }


def render_summary_lines(items: dict[str, dict[str, Any]]) -> list[str]:
    if not items:
        return ["- None yet"]
    lines: list[str] = []
    for key, value in items.items():
        lines.append(
            f"- `{key}`: count=`{value['count']}` place=`{value['place_count']}` "
            f"cancel=`{value['cancel_count']}` guard_skip=`{value['guard_skip_count']}` "
            f"guard_shadow=`{value['guard_shadow_count']}` widen=`{value['basis_widen_count']}` "
            f"avg_age_ms=`{value['avg_quote_age_ms']}` "
            f"avg_adverse_bps=`{value['avg_adverse_move_bps']}`"
        )
    return lines


def render_markdown(report: dict[str, Any]) -> str:
    summary = report["summary"]
    run_classification = report.get("run_classification") or {}
    primary_blocker = report.get("primary_blocker_summary") or {}
    no_trade_diagnosis = report.get("no_trade_diagnosis") or {}
    lines = [
        "# Maker Soak Report",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- DB: `{report['db_path']}`",
        f"- Lookback: `{report['lookback_hours']}` hours",
        f"- Events: `{report['event_count']}`",
        f"- Places: `{report['place_event_count']}`",
        f"- Cancels: `{report['cancel_event_count']}`",
        f"- Guard skips: `{report['guard_skip_event_count']}`",
        f"- Guard shadow events: `{report['guard_shadow_event_count']}`",
        "",
        "## Summary",
        f"- Trade rejections total / recent: `{summary['trade_rejection_count_total']}` / `{summary['trade_rejection_count_recent']}`",
        f"- Roll-guard skips at decision: `{summary['roll_guard_skip_count_at_decision']}`",
        f"- Roll-guard shadows at decision: `{summary['roll_guard_shadow_count_at_decision']}`",
        f"- Trades: `{summary['trade_count']}`",
        f"- Open trades: `{summary['open_trade_count']}`",
        f"- Closed trades: `{summary['closed_trade_count']}`",
        f"- Fill rows: `{summary['fill_count']}`",
        f"- Quote suppressions: `{summary['quote_suppress_count']}`",
        f"- Market-quality suppressions: `{summary['market_quality_suppress_count']}`",
        f"- Maker BUY_NO suppressions: `{summary['maker_buy_no_suppress_count']}`",
        f"- Entry-ineligible suppressions: `{summary['entry_ineligible_suppress_count']}`",
        f"- Trade per place rate: `{summary['trade_per_place_rate']}`",
        f"- Trade per event rate: `{summary['trade_per_event_rate']}`",
        f"- Fill per trade rate: `{summary['fill_per_trade_rate']}`",
        "",
        "## Run Classification",
        f"- Classification: `{run_classification.get('label')}`",
        f"- Note: `{run_classification.get('note')}`",
        "",
        "## Primary Blocker Summary",
        f"- Label: `{primary_blocker.get('label')}`",
        f"- Source: `{primary_blocker.get('source')}`",
        f"- Evidence: `{primary_blocker.get('evidence')}`",
        f"- Note: `{primary_blocker.get('note')}`",
        "",
        "## No-Trade Diagnosis",
        f"- Bucket: `{no_trade_diagnosis.get('bucket')}`",
        f"- Label: `{no_trade_diagnosis.get('label')}`",
        f"- Source: `{no_trade_diagnosis.get('source')}`",
        f"- Note: `{no_trade_diagnosis.get('note')}`",
        f"- Evidence: `{no_trade_diagnosis.get('evidence')}`",
        "",
        "## Blockers",
    ]
    blocker_summary = summary.get("blocker_summary") or {}
    lines.extend(
        [
            f"- Top trade rejection: `{blocker_summary.get('top_trade_rejection')}`",
            f"- Top quote suppress: `{blocker_summary.get('top_quote_suppress')}`",
            f"- Top quote cancel: `{blocker_summary.get('top_quote_cancel')}`",
            f"- Top decision reason: `{blocker_summary.get('top_decision_reason')}`",
            "",
            "## Top Rejections",
        ]
    )
    if summary["trade_rejection_reason_top"]:
        lines.extend(
            [
                f"- `{item['label']}`: `{item['count']}` ({item['share_pct']}%)"
                for item in summary["trade_rejection_reason_top"]
            ]
        )
    else:
        lines.append("- None yet")

    lines.extend(["", "## Top Quote Suppressions"])
    if summary["quote_suppress_subreason_top"]:
        lines.extend(
            [
                f"- `{item['label']}`: `{item['count']}` ({item['share_pct']}%)"
                for item in summary["quote_suppress_subreason_top"]
            ]
        )
    else:
        lines.append("- None yet")

    lines.extend(["", "## Top Decision Reasons"])
    if summary["decision_reason_top"]:
        lines.extend(
            [
                f"- `{item['label']}`: `{item['count']}` ({item['share_pct']}%)"
                for item in summary["decision_reason_top"]
            ]
        )
    else:
        lines.append("- None yet")

    lines.extend(["", "## Top Quote Cancel Reasons"])
    if summary["quote_cancel_subreason_top"]:
        lines.extend(
            [
                f"- `{item['label']}`: `{item['count']}` ({item['share_pct']}%)"
                for item in summary["quote_cancel_subreason_top"]
            ]
        )
    else:
        lines.append("- None yet")

    lines.extend(
        [
            "",
            "## Run Context",
            f"- No-arb status: `{report['run_context']['no_arb']['status']}`",
            f"- No-arb max violation (cents): `{report['run_context']['no_arb']['max_violation_cents']}`",
            f"- No-arb comparable groups: `{report['run_context']['no_arb']['comparable_group_count']}`",
            f"- Strongest Roll signal: `{report['run_context']['maker_roll']['strongest_signal']}`",
            f"- Strongest Roll confidence: `{report['run_context']['maker_roll']['strongest_confidence']}`",
            f"- Roll entries captured: `{report['run_context']['maker_roll']['entry_count']}`",
            f"- L2 snapshot available: `{report['run_context']['maker_l2']['snapshot_available']}`",
            f"- L2 books loaded / two-sided / one-sided / stale / quoteable: `{report['run_context']['maker_l2']['loaded_count']}` / `{report['run_context']['maker_l2']['two_sided_count']}` / `{report['run_context']['maker_l2']['one_sided_count']}` / `{report['run_context']['maker_l2']['stale_count']}` / `{report['run_context']['maker_l2']['quoteable_count']}`",
            f"- Maker quote count / market count: `{report['run_context']['maker_l2']['live_quote_count']}` / `{report['run_context']['maker_l2']['market_count']}`",
            f"- L2 stale reason: `{report['run_context']['maker_l2']['l2_stale_reason']}`",
            f"- Quoteability state: `{report['run_context']['maker_l2']['quoteability_state'].get('label')}` (`{report['run_context']['maker_l2']['quoteability_state'].get('bucket')}`)",
            f"- Quoteability note: `{report['run_context']['maker_l2']['quoteability_state'].get('note')}`",
            f"- Maker inventory markets with position: `{report['run_context']['maker_inventory']['markets_with_position']}`",
            "",
            "## Event Counts",
        ]
    )
    if report["event_counts"]:
        for key, value in sorted(report["event_counts"].items()):
            lines.append(f"- `{key}`: `{value}`")
    else:
        lines.append("- None yet")

    lines.extend(["", "## Cancel Reasons"])
    if report["cancel_reason_counts"]:
        for key, value in sorted(report["cancel_reason_counts"].items()):
            lines.append(f"- `{key}`: `{value}`")
    else:
        lines.append("- None yet")

    lines.extend(["", "## Quote Suppress Reasons"])
    if report["quote_suppress_reason_counts"]:
        for key, value in sorted(report["quote_suppress_reason_counts"].items()):
            lines.append(f"- `{key}`: `{value}`")
    else:
        lines.append("- None yet")

    lines.extend(["", "## Quote Cancel Reasons"])
    if report["quote_cancel_reason_counts"]:
        for key, value in sorted(report["quote_cancel_reason_counts"].items()):
            lines.append(f"- `{key}`: `{value}`")
    else:
        lines.append("- None yet")

    lines.extend(["", "## Basis Buckets"])
    lines.extend(render_summary_lines(report["basis_buckets"]))
    lines.extend(["", "## Side Buckets"])
    lines.extend(render_summary_lines(report["side_buckets"]))
    lines.extend(["", "## Token Buckets"])
    lines.extend(render_summary_lines(report["token_buckets"]))
    lines.extend(["", "## Session Buckets"])
    lines.extend(render_summary_lines(report["session_buckets"]))
    lines.extend(["", "## Hour Buckets"])
    lines.extend(render_summary_lines(report["hour_buckets"]))
    lines.extend(["", "## Price-vs-Beat Buckets"])
    lines.extend(render_summary_lines(report["price_to_beat_buckets"]))
    lines.extend(["", "## Quote Age Buckets"])
    lines.extend(render_summary_lines(report["quote_age_buckets"]))
    lines.extend(["", "## Roll Buckets"])
    lines.extend(render_summary_lines(report["roll_buckets"]))
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    if not math.isfinite(args.hours) or args.hours <= 0:
        raise SystemExit("lookback hours must be finite and positive")
    db_path = Path(args.db).expanduser().resolve()
    outdir = Path(args.outdir).expanduser().resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    events = load_events(db_path, args.hours)
    quote_actions = load_quote_actions(db_path, args.hours)
    maker_state_snapshot = load_optional_json(args.maker_state_snapshot)
    no_arb_snapshot = load_optional_json(args.no_arb_snapshot)
    report = summarize(
        events,
        quote_actions,
        db_path,
        args.hours,
        maker_state_snapshot,
        no_arb_snapshot,
    )

    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_maker_soak_report.json"
    md_path = outdir / f"{stamp}_maker_soak_report.md"
    latest_json = outdir / "latest_maker_soak_report.json"
    latest_md = outdir / "latest_maker_soak_report.md"
    json_path.write_text(json.dumps(report, indent=2), encoding="utf-8")
    md_path.write_text(render_markdown(report), encoding="utf-8")
    latest_json.write_text(json.dumps(report, indent=2), encoding="utf-8")
    latest_md.write_text(render_markdown(report), encoding="utf-8")

    print(json.dumps({"json": str(json_path), "md": str(md_path), "events": report["event_count"]}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
