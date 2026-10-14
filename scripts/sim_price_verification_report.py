#!/usr/bin/env python3
"""Verify simulated trade prices against captured CLOB book evidence."""

from __future__ import annotations

import argparse
import json
import math
import sqlite3
from collections import Counter
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


PRICE_TOLERANCE = 0.0015


@dataclass
class FillVerdict:
    fill_id: int
    trade_id: int
    token_id: str | None
    phase: str
    source: str
    path: str
    classification: str
    status: str
    reason: str
    intended_price: float | None
    observed_price: float | None
    midpoint: float | None
    best_bid: float | None
    best_ask: float | None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Verify simulated trade prices against captured book evidence"
    )
    parser.add_argument("--db", required=True, help="SQLite DB path")
    parser.add_argument(
        "--outdir",
        default="artifacts/sim-price-verification-reports",
        help="Directory for JSON/Markdown artifacts",
    )
    return parser.parse_args()


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
    return [
        {
            "label": label,
            "count": int(count),
            "share_pct": round((count / total) * 100.0, 1),
        }
        for label, count in counter.most_common(limit)
    ]


def load_rows(db_path: Path) -> tuple[list[sqlite3.Row], list[sqlite3.Row]]:
    conn = sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        conn.execute("BEGIN")
        fills = conn.execute(
            """
            SELECT
              f.id,
              f.trade_id,
              f.phase,
              f.source,
              f.details,
              t.token_id,
              t.market_id
            FROM trade_fills f
            JOIN trades t ON t.id = f.trade_id
            ORDER BY f.id ASC
            """
        ).fetchall()
        trades = conn.execute(
            """
            SELECT id, token_id, market_id, direction, status, exit_type, entry_price, exit_price
            FROM trades
            ORDER BY id ASC
            """
        ).fetchall()
    finally:
        conn.close()
    return fills, trades


def parse_json(raw: str) -> dict[str, Any] | None:
    try:
        value = json.loads(raw)
    except json.JSONDecodeError:
        return None
    return value if isinstance(value, dict) else None


def load_trade_rejection_stats(db_path: Path) -> dict[str, Any]:
    conn = sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True)
    conn.row_factory = sqlite3.Row
    try:
        total = int(conn.execute("SELECT COUNT(*) FROM trade_rejections").fetchone()[0] or 0)
        rows = conn.execute(
            """
            SELECT rejection_reason, strategy
            FROM trade_rejections
            ORDER BY id ASC
            """
        ).fetchall()
    except sqlite3.OperationalError:
        return {
            "total_count": 0,
            "reason_counts": {},
            "bucket_counts": {},
            "strategy_counts": {},
            "reason_top": [],
            "bucket_top": [],
            "strategy_top": [],
        }
    finally:
        conn.close()

    reasons = [str(row["rejection_reason"] or "") for row in rows]
    strategies = [str(row["strategy"] or "unknown") for row in rows]
    reason_counts = Counter(reasons)
    bucket_counts = Counter(trade_rejection_reason_bucket(reason) for reason in reasons)
    strategy_counts = Counter(strategies)
    return {
        "total_count": total,
        "reason_counts": dict(reason_counts),
        "bucket_counts": dict(bucket_counts),
        "strategy_counts": dict(strategy_counts),
        "reason_top": top_counter_items(reason_counts, 5),
        "bucket_top": top_counter_items(bucket_counts, 5),
        "strategy_top": top_counter_items(strategy_counts, 5),
    }


def classify_fill(row: sqlite3.Row) -> FillVerdict:
    details = parse_json(row["details"] or "")
    path = str((details or {}).get("path") or "")
    phase = str(row["phase"] or "")
    source = str(row["source"] or "")
    token_id = row["token_id"]

    if not details:
        return FillVerdict(
            fill_id=int(row["id"]),
            trade_id=int(row["trade_id"]),
            token_id=token_id,
            phase=phase,
            source=source,
            path=path or "unknown",
            classification="legacy_or_unstructured",
            status="unverifiable",
            reason="fill details are not structured JSON",
            intended_price=None,
            observed_price=None,
            midpoint=None,
            best_bid=None,
            best_ask=None,
        )

    snapshot = details.get("snapshot") or {}
    if not isinstance(snapshot, dict):
        snapshot = {}
    intended = as_float(details.get("intended_price"))
    observed = as_float(details.get("observed_price"))
    midpoint = as_float(snapshot.get("midpoint"))
    best_bid = as_float(snapshot.get("best_bid"))
    best_ask = as_float(snapshot.get("best_ask"))

    if phase == "mark_exit" or path == "shutdown_cleanup":
        if path == "resolution":
            classification, reason = "resolution_payout", "explicit resolution payout, not an orderbook fill"
        elif path == "pair_merge":
            classification, reason = "pair_payout", "complementary inventory payout, not an orderbook fill"
        elif path == "shutdown_cleanup":
            classification, reason = "cleanup_mark", "forced cleanup mark, not an execution fill"
        else:
            classification, reason = "accounting_mark", "explicit accounting mark, not an execution fill"
        return FillVerdict(
            fill_id=int(row["id"]),
            trade_id=int(row["trade_id"]),
            token_id=token_id,
            phase=phase,
            source=source,
            path=path or "shutdown_cleanup",
            classification=classification,
            status="n/a",
            reason=reason,
            intended_price=intended,
            observed_price=observed,
            midpoint=midpoint,
            best_bid=best_bid,
            best_ask=best_ask,
        )

    if intended is None or observed is None:
        return FillVerdict(
            fill_id=int(row["id"]),
            trade_id=int(row["trade_id"]),
            token_id=token_id,
            phase=phase,
            source=source,
            path=path or "unknown",
            classification="execution_fill",
            status="unverifiable",
            reason="missing intended or observed price",
            intended_price=intended,
            observed_price=observed,
            midpoint=midpoint,
            best_bid=best_bid,
            best_ask=best_ask,
        )

    if snapshot.get("token_id") and snapshot["token_id"] != token_id:
        return verdict(row, token_id, path, "execution_fill", False,
                       "captured book belongs to another token", intended, observed, midpoint, best_bid, best_ask)
    if best_bid is not None and best_ask is not None and best_bid > best_ask:
        return verdict(row, token_id, path, "execution_fill", False,
                       "captured book is crossed", intended, observed, midpoint, best_bid, best_ask)

    if path == "maker_passive":
        ok = best_ask is not None and best_ask <= observed + PRICE_TOLERANCE
        return verdict(
            row, token_id, path, "execution_fill", ok,
            "captured ask crossed maker bid" if ok else "captured ask did not cross maker bid",
            intended, observed, midpoint, best_bid, best_ask,
        )

    if path == "maker_flatten":
        ok = best_bid is not None and observed <= best_bid + PRICE_TOLERANCE
        return verdict(
            row, token_id, path, "execution_fill", ok,
            "captured bid crossed maker ask" if ok else "captured bid did not cross maker ask",
            intended, observed, midpoint, best_bid, best_ask,
        )

    if path in {"sim_taker_orderbook", "manual_orderbook"}:
        if details.get("side") == "buy":
            ok = (
                observed <= intended + PRICE_TOLERANCE
                and best_ask is not None and observed + PRICE_TOLERANCE >= best_ask
            )
            reason = (
                "buy fill stayed inside limit and above top ask"
                if ok
                else "buy fill inconsistent with limit/top ask"
            )
        elif details.get("side") == "sell":
            ok = (
                observed <= intended + PRICE_TOLERANCE
                and best_bid is not None and observed <= best_bid + PRICE_TOLERANCE
            )
            reason = (
                "sell fill stayed inside bid-side book"
                if ok
                else "sell fill inconsistent with top bid"
            )
        else:
            ok, reason = False, "execution fill has no valid side"
        return verdict(
            row, token_id, path, "execution_fill", ok, reason,
            intended, observed, midpoint, best_bid, best_ask,
        )

    if path == "sim_taker_midpoint_fallback":
        ok = midpoint is not None and abs(observed - midpoint) <= PRICE_TOLERANCE
        return verdict(
            row, token_id, path, "execution_fill", ok,
            "observed price matches midpoint fallback"
            if ok
            else "observed price diverges from midpoint fallback",
            intended, observed, midpoint, best_bid, best_ask,
        )

    if path == "partial_close":
        ok = (
            (best_bid is not None and observed <= best_bid + PRICE_TOLERANCE)
            or (midpoint is not None and abs(observed - midpoint) <= PRICE_TOLERANCE)
        )
        return verdict(
            row, token_id, path, "execution_fill", ok,
            "partial close aligns with bid/mid snapshot"
            if ok
            else "partial close price not supported by snapshot",
            intended, observed, midpoint, best_bid, best_ask,
        )

    return FillVerdict(
        fill_id=int(row["id"]),
        trade_id=int(row["trade_id"]),
        token_id=token_id,
        phase=phase,
        source=source,
        path=path or "unknown",
        classification="execution_fill",
        status="unverifiable",
        reason="unsupported or unknown verification path",
        intended_price=intended,
        observed_price=observed,
        midpoint=midpoint,
        best_bid=best_bid,
        best_ask=best_ask,
    )


def verdict(
    row: sqlite3.Row,
    token_id: str | None,
    path: str,
    classification: str,
    ok: bool,
    reason: str,
    intended: float | None,
    observed: float | None,
    midpoint: float | None,
    best_bid: float | None,
    best_ask: float | None,
) -> FillVerdict:
    return FillVerdict(
        fill_id=int(row["id"]),
        trade_id=int(row["trade_id"]),
        token_id=token_id,
        phase=str(row["phase"] or ""),
        source=str(row["source"] or ""),
        path=path,
        classification=classification,
        status="plausible" if ok else "implausible",
        reason=reason,
        intended_price=intended,
        observed_price=observed,
        midpoint=midpoint,
        best_bid=best_bid,
        best_ask=best_ask,
    )


def as_float(value: Any) -> float | None:
    if value is None:
        return None
    try:
        out = float(value)
    except (TypeError, ValueError):
        return None
    return out if not isinstance(value, bool) and math.isfinite(out) and 0 <= out <= 1 else None


def churn_summary(trades: list[sqlite3.Row]) -> dict[str, Any]:
    token_counts: Counter[str] = Counter()
    max_streak = 0
    streak_token: str | None = None
    current_token: str | None = None
    current_streak = 0

    for row in trades:
        token = row["token_id"] or "unknown"
        token_counts[token] += 1
        if token == current_token:
            current_streak += 1
        else:
            current_token = token
            current_streak = 1
        if current_streak > max_streak:
            max_streak = current_streak
            streak_token = token

    total_trades = len(trades)
    top_tokens = [
        {
            "token_id": token,
            "trade_count": count,
            "share_pct": (count / total_trades * 100.0) if total_trades else None,
        }
        for token, count in token_counts.most_common(5)
    ]
    return {
        "total_trades": total_trades,
        "unique_tokens": len(token_counts),
        "top_tokens": top_tokens,
        "max_consecutive_same_token_trades": max_streak,
        "max_streak_token_id": streak_token,
    }


def build_report(db_path: Path) -> dict[str, Any]:
    fill_rows, trade_rows = load_rows(db_path)
    verdicts = [classify_fill(row) for row in fill_rows]
    trade_rejections = load_trade_rejection_stats(db_path)

    by_status = Counter(v.status for v in verdicts)
    by_path = Counter(v.path for v in verdicts)
    by_classification = Counter(v.classification for v in verdicts)

    implausible_samples = [
        {
            "fill_id": v.fill_id,
            "trade_id": v.trade_id,
            "token_id": v.token_id,
            "phase": v.phase,
            "path": v.path,
            "reason": v.reason,
            "intended_price": v.intended_price,
            "observed_price": v.observed_price,
            "midpoint": v.midpoint,
            "best_bid": v.best_bid,
            "best_ask": v.best_ask,
        }
        for v in verdicts
        if v.status == "implausible"
    ][:10]

    cleanup_count = sum(1 for v in verdicts if v.classification == "cleanup_mark")
    execution_count = sum(1 for v in verdicts if v.classification == "execution_fill")

    return {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "db": str(db_path),
        "summary": {
            "total_fill_rows": len(verdicts),
            "execution_fill_rows": execution_count,
            "cleanup_mark_rows": cleanup_count,
            "plausible_execution_rows": by_status.get("plausible", 0),
            "implausible_execution_rows": by_status.get("implausible", 0),
            "unverifiable_rows": by_status.get("unverifiable", 0),
            "n_a_rows": by_status.get("n/a", 0),
            "trade_rejection_count": trade_rejections["total_count"],
            "trade_rejection_reason_top": trade_rejections["reason_top"],
            "trade_rejection_bucket_top": trade_rejections["bucket_top"],
            "trade_rejection_strategy_top": trade_rejections["strategy_top"],
            "blocker_summary": {
                "top_trade_rejection": trade_rejections["reason_top"][0]
                if trade_rejections["reason_top"]
                else None,
                "top_trade_rejection_bucket": trade_rejections["bucket_top"][0]
                if trade_rejections["bucket_top"]
                else None,
            },
        },
        "counts_by_path": dict(by_path),
        "counts_by_classification": dict(by_classification),
        "churn": churn_summary(trade_rows),
        "implausible_samples": implausible_samples,
        "verdicts": [v.__dict__ for v in verdicts],
    }


def render_markdown(report: dict[str, Any]) -> str:
    summary = report["summary"]
    churn = report["churn"]
    lines = [
        "# Sim Price Verification Report",
        "",
        f"- Generated: `{report['generated_at']}`",
        f"- DB: `{report['db']}`",
        f"- Total fill rows: `{summary['total_fill_rows']}`",
        f"- Execution fill rows: `{summary['execution_fill_rows']}`",
        f"- Cleanup mark rows: `{summary['cleanup_mark_rows']}`",
        f"- Plausible execution rows: `{summary['plausible_execution_rows']}`",
        f"- Implausible execution rows: `{summary['implausible_execution_rows']}`",
        f"- Unverifiable rows: `{summary['unverifiable_rows']}`",
        f"- Trade rejections: `{summary['trade_rejection_count']}`",
        "",
        "## Blockers",
        f"- Top trade rejection: `{(summary['blocker_summary']['top_trade_rejection'] if summary.get('blocker_summary') else None)}`",
        f"- Top trade rejection bucket: `{(summary['blocker_summary']['top_trade_rejection_bucket'] if summary.get('blocker_summary') else None)}`",
        "",
        "## Top Rejections",
    ]
    if summary["trade_rejection_reason_top"]:
        lines.extend(
            [
                f"- `{row['label']}`: `{row['count']}` ({row['share_pct']}%)"
                for row in summary["trade_rejection_reason_top"]
            ]
        )
    else:
        lines.append("- None")

    lines.extend(
        [
            "",
            "## Churn",
            f"- Total trades: `{churn['total_trades']}`",
            f"- Unique tokens: `{churn['unique_tokens']}`",
            f"- Max consecutive same-token trades: `{churn['max_consecutive_same_token_trades']}`",
            f"- Max streak token: `{churn['max_streak_token_id']}`",
            "",
            "## Top Tokens",
        ]
    )
    if churn["top_tokens"]:
        lines.extend(
            [
                f"- `{row['token_id']}`: `{row['trade_count']}` trades"
                + (f" ({row['share_pct']:.1f}%)" if row["share_pct"] is not None else "")
                for row in churn["top_tokens"]
            ]
        )
    else:
        lines.append("- None")

    lines.extend(["", "## Implausible Samples"])
    if report["implausible_samples"]:
        for sample in report["implausible_samples"]:
            lines.append(
                "- "
                + f"fill `{sample['fill_id']}` trade `{sample['trade_id']}` "
                + f"path=`{sample['path']}` obs=`{sample['observed_price']}` "
                + f"bid=`{sample['best_bid']}` ask=`{sample['best_ask']}` "
                + f"mid=`{sample['midpoint']}` reason=`{sample['reason']}`"
            )
    else:
        lines.append("- None")
    return "\n".join(lines) + "\n"


def main() -> int:
    args = parse_args()
    db_path = Path(args.db).expanduser().resolve()
    outdir = Path(args.outdir).expanduser().resolve()
    outdir.mkdir(parents=True, exist_ok=True)

    report = build_report(db_path)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S")
    json_path = outdir / f"{stamp}_sim_price_verification_report.json"
    md_path = outdir / f"{stamp}_sim_price_verification_report.md"
    latest_json = outdir / "latest_sim_price_verification_report.json"
    latest_md = outdir / "latest_sim_price_verification_report.md"

    json_blob = json.dumps(report, indent=2)
    md_blob = render_markdown(report)

    json_path.write_text(json_blob + "\n", encoding="utf-8")
    md_path.write_text(md_blob, encoding="utf-8")
    latest_json.write_text(json_blob + "\n", encoding="utf-8")
    latest_md.write_text(md_blob, encoding="utf-8")
    print(json_blob)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
