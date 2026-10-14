#!/usr/bin/env python3
"""Summarize lifecycle integrity for a validation/control SQLite DB."""

from __future__ import annotations

import argparse
import json
import sqlite3
from contextlib import closing
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parent.parent


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build a validation DB lifecycle integrity report"
    )
    parser.add_argument("--db", required=True, help="SQLite DB path")
    parser.add_argument(
        "--output-dir",
        default="artifacts/validation-integrity-reports",
        help="Directory for report JSON/Markdown artifacts",
    )
    parser.add_argument(
        "--top-limit",
        type=int,
        default=10,
        help="How many top reasons to keep in report summaries",
    )
    return parser.parse_args()


def table_exists(conn: sqlite3.Connection, table: str) -> bool:
    row = conn.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        (table,),
    ).fetchone()
    return row is not None


def require_tables(conn: sqlite3.Connection, tables: list[str]) -> list[str]:
    missing = [table for table in tables if not table_exists(conn, table)]
    return missing


def top_counts(counter: Counter[str], limit: int) -> list[dict[str, Any]]:
    total = sum(counter.values())
    if total <= 0:
        return []
    rows: list[dict[str, Any]] = []
    for label, count in counter.most_common(limit):
        rows.append(
            {
                "label": label,
                "count": int(count),
                "share_pct": round((count / total) * 100.0, 1),
            }
        )
    return rows


def load_trade_rejections(conn: sqlite3.Connection, limit: int) -> dict[str, Any]:
    total = int(conn.execute("SELECT COUNT(*) FROM trade_rejections").fetchone()[0] or 0)
    if total == 0:
        return {
            "total_count": 0,
            "reason_top": [],
            "strategy_top": [],
            "reason_counter": Counter(),
        }
    reasons = Counter(
        row[0] or "unknown"
        for row in conn.execute(
            "SELECT rejection_reason FROM trade_rejections ORDER BY timestamp DESC, id DESC"
        ).fetchall()
    )
    strategies = Counter(
        row[0] or "unknown"
        for row in conn.execute(
            "SELECT strategy FROM trade_rejections ORDER BY timestamp DESC, id DESC"
        ).fetchall()
    )
    return {
        "total_count": total,
        "reason_top": top_counts(reasons, limit),
        "strategy_top": top_counts(strategies, limit),
        "reason_counter": reasons,
    }


def load_order_outbox_counts(conn: sqlite3.Connection, limit: int) -> dict[str, Any]:
    stage_action_counter: Counter[str] = Counter()
    action_counter: Counter[str] = Counter()
    suppress_reasons: Counter[str] = Counter()
    cancel_reasons: Counter[str] = Counter()

    for row in conn.execute(
        "SELECT stage, action, details FROM order_outbox ORDER BY id"
    ).fetchall():
        stage = row[0] or "unknown"
        action = row[1] or "unknown"
        details = row[2] or ""
        stage_action_counter[f"{stage}/{action}"] += 1
        action_counter[action] += 1
        if action == "suppress":
            reason = parse_reason(details) or "unknown"
            suppress_reasons[reason] += 1
        elif action == "cancel":
            reason = parse_reason(details) or details or "unknown"
            cancel_reasons[reason] += 1

    return {
        "total_count": int(sum(action_counter.values())),
        "action_top": top_counts(action_counter, limit),
        "stage_action_top": top_counts(stage_action_counter, limit),
        "suppress_reason_top": top_counts(suppress_reasons, limit),
        "cancel_reason_top": top_counts(cancel_reasons, limit),
        "action_counter": action_counter,
        "suppress_reason_counter": suppress_reasons,
        "cancel_reason_counter": cancel_reasons,
    }


def parse_reason(details: str) -> str | None:
    if details.lstrip().startswith("{"):
        try:
            payload = json.loads(details)
        except json.JSONDecodeError:
            payload = None
        if isinstance(payload, dict):
            reason = payload.get("reason")
            if reason is not None:
                reason_str = str(reason).strip()
                return reason_str or None
    marker = "reason="
    idx = details.find(marker)
    if idx < 0:
        return None
    tail = details[idx + len(marker) :]
    reason = tail.strip()
    return reason or None


def parse_int_field(details: str, field: str) -> int | None:
    if details.lstrip().startswith("{"):
        try:
            payload = json.loads(details)
            value = payload.get(field) if isinstance(payload, dict) else None
            if isinstance(value, bool):
                return int(value)
            if isinstance(value, int):
                return value
        except json.JSONDecodeError:
            pass
    marker = f"{field}="
    idx = details.find(marker)
    if idx < 0:
        return None
    tail = details[idx + len(marker) :]
    end = len(tail)
    for sep in (" ", "|", ",", "\n", "\t"):
        sep_idx = tail.find(sep)
        if sep_idx >= 0:
            end = min(end, sep_idx)
    raw = tail[:end].strip()
    if not raw:
        return None
    try:
        return int(raw)
    except ValueError:
        return None


def load_fill_integrity(conn: sqlite3.Connection) -> dict[str, Any]:
    rows = conn.execute(
        """
        SELECT tf.id, tf.trade_id, tf.order_id, tf.phase, tf.source,
               t.id AS trade_exists
        FROM trade_fills AS tf
        LEFT JOIN trades AS t
          ON t.id = tf.trade_id
        ORDER BY tf.id
        """
    ).fetchall()

    orphan_trade_fill_ids = [int(r[0]) for r in rows if r[5] is None]
    trade_fill_missing_order_id_ids = [
        int(r[0]) for r in rows if r[2] is None
    ]
    cleanup_rows = [
        r for r in rows if r[3] == "mark_exit" and r[4] == "shutdown_cleanup"
    ]
    non_shutdown_mark_exit_ids = [
        int(r[0])
        for r in rows
        if r[3] == "mark_exit" and r[4] != "shutdown_cleanup"
    ]
    entry_rows = [
        {
            "id": int(r[0]),
            "trade_id": int(r[1]),
            "order_id": r[2],
            "phase": r[3] or "unknown",
            "source": r[4] or "unknown",
        }
        for r in rows
        if r[3] == "entry"
    ]
    entry_order_ids = {row["order_id"] for row in entry_rows if row["order_id"]}

    outbox_fill_rows = conn.execute(
        """
        SELECT id, order_id, details
        FROM order_outbox
        WHERE stage = 'maker_quote' AND action = 'fill'
        ORDER BY id
        """
    ).fetchall()
    outbox_fill_opened_rows = [
        {
            "id": int(r[0]),
            "order_id": r[1],
            "details": r[2] or "",
        }
        for r in outbox_fill_rows
        if parse_int_field(r[2] or "", "opened") == 1
    ]
    outbox_fill_opened_order_ids = {
        row["order_id"] for row in outbox_fill_opened_rows if row["order_id"]
    }

    maker_entry_fill_missing_outbox_ids = [
        row["id"] for row in entry_rows if row["source"] == "maker_sim" and row["order_id"] not in outbox_fill_opened_order_ids
    ]
    maker_entry_outbox_missing_trade_fill_ids = [
        row["id"] for row in outbox_fill_opened_rows if row["order_id"] not in entry_order_ids
    ]

    phase_counter = Counter((r[3] or "unknown") for r in rows)
    source_counter = Counter((r[4] or "unknown") for r in rows)
    cleanup_trade_ids = sorted({int(r[1]) for r in cleanup_rows})

    return {
        "total_count": len(rows),
        "phase_top": top_counts(phase_counter, 10),
        "source_top": top_counts(source_counter, 10),
        "orphan_trade_fill_ids": orphan_trade_fill_ids,
        "trade_fill_missing_order_id_ids": trade_fill_missing_order_id_ids,
        "entry_count": len(entry_rows),
        "entry_trade_fill_ids": [row["id"] for row in entry_rows],
        "entry_fill_missing_outbox_ids": maker_entry_fill_missing_outbox_ids,
        "entry_fill_outbox_match_count": len(entry_rows) - len(maker_entry_fill_missing_outbox_ids),
        "maker_quote_fill_opened_count": len(outbox_fill_opened_rows),
        "maker_quote_fill_opened_ids": [row["id"] for row in outbox_fill_opened_rows],
        "maker_quote_fill_opened_missing_trade_fill_ids": maker_entry_outbox_missing_trade_fill_ids,
        "cleanup_trade_ids": cleanup_trade_ids,
        "non_shutdown_mark_exit_ids": non_shutdown_mark_exit_ids,
    }


def load_trade_counts(conn: sqlite3.Connection) -> dict[str, Any]:
    total = int(conn.execute("SELECT COUNT(*) FROM trades").fetchone()[0] or 0)
    status_rows = conn.execute(
        """
        SELECT status, COUNT(*) AS c
        FROM trades
        GROUP BY status
        ORDER BY c DESC, status
        """
    ).fetchall()
    status_counter = Counter({row[0] or "unknown": int(row[1] or 0) for row in status_rows})
    active_rows = conn.execute(
        """
        SELECT id, market_id, token_id, direction, status, exit_type
        FROM trades
        WHERE status IN ('OPEN', 'CLOSING')
        ORDER BY id
        """
    ).fetchall()
    cleanup_rows = conn.execute(
        """
        SELECT id, market_id, token_id, direction, status, exit_type
        FROM trades
        WHERE status = 'CLOSED' AND exit_type = 'shutdown_cleanup'
        ORDER BY id
        """
    ).fetchall()
    cleanup_trade_ids = [int(row[0]) for row in cleanup_rows]
    cleanup_trade_entry_missing = []
    if cleanup_trade_ids:
        placeholders = ",".join("?" for _ in cleanup_trade_ids)
        cleanup_trade_entry_missing = [
            int(row[0])
            for row in conn.execute(
                f"""
                SELECT tf.trade_id
                FROM trade_fills AS tf
                LEFT JOIN order_outbox AS oo
                  ON oo.order_id = tf.order_id
                 AND oo.stage = 'maker_quote'
                 AND oo.action = 'fill'
                WHERE tf.trade_id IN ({placeholders})
                  AND tf.phase = 'entry'
                  AND tf.source = 'maker_sim'
                  AND oo.order_id IS NULL
                GROUP BY tf.trade_id
                ORDER BY tf.trade_id
                """,
                cleanup_trade_ids,
            ).fetchall()
        ]
    return {
        "total_count": total,
        "status_top": top_counts(status_counter, 10),
        "active_count": len(active_rows),
        "open_count": len([row for row in active_rows if row[4] == "OPEN"]),
        "closing_count": len([row for row in active_rows if row[4] == "CLOSING"]),
        "active_rows": [
            {
                "id": int(row[0]),
                "market_id": row[1],
                "token_id": row[2],
                "direction": row[3],
                "status": row[4],
                "exit_type": row[5],
            }
            for row in active_rows
        ],
        "cleanup_trade_count": len(cleanup_rows),
        "cleanup_trade_ids": cleanup_trade_ids,
        "cleanup_trade_statuses": [
            {
                "id": int(row[0]),
                "status": row[4],
                "exit_type": row[5],
            }
            for row in cleanup_rows
        ],
        "cleanup_trade_entry_missing_ids": cleanup_trade_entry_missing,
    }


def classify_report(
    missing_tables: list[str],
    trade_counts: dict[str, Any],
    fill_counts: dict[str, Any],
    outbox_counts: dict[str, Any],
    trade_rejections: dict[str, Any],
    mismatches: list[dict[str, Any]],
) -> tuple[str, str]:
    if missing_tables:
        return "invalid", "missing_tables"
    if mismatches:
        return "mismatch", str(mismatches[0].get("check") or "unknown")
    if (trade_counts.get("active_count") or 0) == 0:
        return "ok", "clean"
    return "mismatch", "active_trades_remain"


def compact_counts(
    trade_counts: dict[str, Any],
    fill_counts: dict[str, Any],
    outbox_counts: dict[str, Any],
    trade_rejections: dict[str, Any],
) -> dict[str, Any]:
    return {
        "trades": {
            "total": int(trade_counts.get("total_count") or 0),
            "active": int(trade_counts.get("active_count") or 0),
            "open": int(trade_counts.get("open_count") or 0),
            "closing": int(trade_counts.get("closing_count") or 0),
            "cleanup": int(trade_counts.get("cleanup_trade_count") or 0),
        },
        "fills": {
            "total": int(fill_counts.get("total_count") or 0),
            "entry": int(fill_counts.get("entry_count") or 0),
            "orphan": len(fill_counts.get("orphan_trade_fill_ids") or []),
            "missing_order_id": len(fill_counts.get("trade_fill_missing_order_id_ids") or []),
            "cleanup": len(fill_counts.get("cleanup_trade_ids") or []),
        },
        "outbox": {
            "total": int(outbox_counts.get("total_count") or 0),
            "suppress": int((outbox_counts.get("action_counter") or {}).get("suppress") or 0),
            "place": int((outbox_counts.get("action_counter") or {}).get("place") or 0),
            "cancel": int((outbox_counts.get("action_counter") or {}).get("cancel") or 0),
            "fill": int((outbox_counts.get("action_counter") or {}).get("fill") or 0),
        },
        "rejections": {
            "total": int(trade_rejections.get("total_count") or 0),
        },
    }


def build_report(db_path: Path, top_limit: int) -> dict[str, Any]:
    with closing(sqlite3.connect(db_path.resolve().as_uri() + "?mode=ro", uri=True)) as conn:
        conn.row_factory = sqlite3.Row
        conn.execute("BEGIN")
        missing_tables = require_tables(
            conn,
            ["trades", "trade_fills", "order_outbox", "trade_rejections"],
        )
        trade_counts = load_trade_counts(conn) if not missing_tables else {}
        fill_counts = (
            load_fill_integrity(conn)
            if not missing_tables
            else {}
        )
        outbox_counts = (
            load_order_outbox_counts(conn, top_limit)
            if not missing_tables
            else {}
        )
        trade_rejections = (
            load_trade_rejections(conn, top_limit)
            if not missing_tables
            else {"total_count": 0, "reason_top": [], "strategy_top": [], "reason_counter": Counter()}
        )

    mismatches: list[dict[str, Any]] = []
    if missing_tables:
        mismatches.append(
            {
                "check": "missing_tables",
                "count": len(missing_tables),
                "details": missing_tables,
            }
        )
        verdict, reason = classify_report(missing_tables, trade_counts, fill_counts, outbox_counts, trade_rejections, mismatches)
        return {
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "db": str(db_path),
            "ok": False,
            "status": "mismatch",
            "verdict": verdict,
            "reason": reason,
            "counts": compact_counts(trade_counts, fill_counts, outbox_counts, trade_rejections),
            "missing_tables": missing_tables,
            "trade_counts": trade_counts,
            "fill_integrity": fill_counts,
            "outbox_counts": outbox_counts,
            "trade_rejections": trade_rejections,
            "mismatches": mismatches,
        }

    if trade_counts["active_count"] > 0:
        mismatches.append(
            {
                "check": "active_trades_remain",
                "count": trade_counts["active_count"],
                "details": trade_counts["active_rows"],
            }
        )
    if fill_counts["orphan_trade_fill_ids"]:
        mismatches.append(
            {
                "check": "orphan_trade_fills",
                "count": len(fill_counts["orphan_trade_fill_ids"]),
                "details": fill_counts["orphan_trade_fill_ids"],
            }
        )
    if fill_counts["trade_fill_missing_order_id_ids"]:
        mismatches.append(
            {
                "check": "trade_fill_missing_order_id",
                "count": len(fill_counts["trade_fill_missing_order_id_ids"]),
                "details": fill_counts["trade_fill_missing_order_id_ids"],
            }
        )
    if fill_counts["entry_fill_missing_outbox_ids"]:
        mismatches.append(
            {
                "check": "maker_entry_trade_fills_missing_outbox_fill",
                "count": len(fill_counts["entry_fill_missing_outbox_ids"]),
                "details": fill_counts["entry_fill_missing_outbox_ids"],
            }
        )
    if fill_counts["maker_quote_fill_opened_missing_trade_fill_ids"]:
        mismatches.append(
            {
                "check": "maker_quote_opened_outbox_missing_trade_fill",
                "count": len(fill_counts["maker_quote_fill_opened_missing_trade_fill_ids"]),
                "details": fill_counts["maker_quote_fill_opened_missing_trade_fill_ids"],
            }
        )
    if fill_counts["non_shutdown_mark_exit_ids"]:
        mismatches.append(
            {
                "check": "mark_exit_source_mismatch",
                "count": len(fill_counts["non_shutdown_mark_exit_ids"]),
                "details": fill_counts["non_shutdown_mark_exit_ids"],
            }
        )
    if trade_counts["cleanup_trade_ids"] and trade_counts["cleanup_trade_entry_missing_ids"]:
        mismatches.append(
            {
                "check": "cleanup_entry_outbox_missing",
                "count": len(trade_counts["cleanup_trade_entry_missing_ids"]),
                "details": trade_counts["cleanup_trade_entry_missing_ids"],
            }
        )

    verdict, reason = classify_report(missing_tables, trade_counts, fill_counts, outbox_counts, trade_rejections, mismatches)
    summary = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "db": str(db_path),
        "ok": not mismatches,
        "status": "ok" if not mismatches else "mismatch",
        "verdict": verdict,
        "reason": reason,
        "counts": compact_counts(trade_counts, fill_counts, outbox_counts, trade_rejections),
        "trade_counts": trade_counts,
        "fill_integrity": fill_counts,
        "outbox_counts": outbox_counts,
        "trade_rejections": trade_rejections,
        "mismatches": mismatches,
    }
    return summary


def render_markdown(report: dict[str, Any]) -> str:
    trade_counts = report.get("trade_counts") or {}
    fill_counts = report.get("fill_integrity") or {}
    outbox_counts = report.get("outbox_counts") or {}
    trade_rejections = report.get("trade_rejections") or {}
    counts = report.get("counts") or {}
    mismatches = report.get("mismatches") or []
    trade_counts_compact = counts.get("trades") or {}
    fill_counts_compact = counts.get("fills") or {}
    outbox_counts_compact = counts.get("outbox") or {}
    rejection_counts_compact = counts.get("rejections") or {}

    lines = [
        "# Validation DB Integrity Report",
        "",
        f"- Generated: `{report.get('generated_at')}`",
        f"- DB: `{report.get('db')}`",
        f"- OK: `{report.get('ok')}`",
        f"- Verdict: `{report.get('verdict')}`",
        f"- Reason: `{report.get('reason')}`",
        "",
        "## Counts",
        "",
        f"- Trades total/open/closing/cleanup: `{trade_counts_compact.get('total')}` / `{trade_counts_compact.get('open')}` / `{trade_counts_compact.get('closing')}` / `{trade_counts_compact.get('cleanup')}`",
        f"- Fills total/entry/orphan/missing-order-id/cleanup: `{fill_counts_compact.get('total')}` / `{fill_counts_compact.get('entry')}` / `{fill_counts_compact.get('orphan')}` / `{fill_counts_compact.get('missing_order_id')}` / `{fill_counts_compact.get('cleanup')}`",
        f"- Outbox total/place/cancel/fill/suppress: `{outbox_counts_compact.get('total')}` / `{outbox_counts_compact.get('place')}` / `{outbox_counts_compact.get('cancel')}` / `{outbox_counts_compact.get('fill')}` / `{outbox_counts_compact.get('suppress')}`",
        f"- Rejections total: `{rejection_counts_compact.get('total')}`",
        "",
        "## Trades",
        "",
        f"- Total: `{trade_counts.get('total_count')}`",
        f"- Open: `{trade_counts.get('open_count')}`",
        f"- Closing: `{trade_counts.get('closing_count')}`",
        f"- Active total: `{trade_counts.get('active_count')}`",
        f"- Shutdown cleanup trades: `{trade_counts.get('cleanup_trade_count')}`",
        "",
        "## Fills",
        "",
        f"- Total: `{fill_counts.get('total_count')}`",
        f"- Entry fills: `{fill_counts.get('entry_count')}`",
        f"- Entry fills matched to outbox open rows: `{fill_counts.get('entry_fill_outbox_match_count')}`",
        f"- Opened outbox fill rows: `{fill_counts.get('maker_quote_fill_opened_count')}`",
        f"- Orphan fill ids: `{fill_counts.get('orphan_trade_fill_ids')}`",
        f"- Missing order ids: `{fill_counts.get('trade_fill_missing_order_id_ids')}`",
        f"- Entry fills missing outbox open row: `{fill_counts.get('entry_fill_missing_outbox_ids')}`",
        f"- Opened outbox rows missing entry fill: `{fill_counts.get('maker_quote_fill_opened_missing_trade_fill_ids')}`",
        f"- Non-shutdown mark-exit ids: `{fill_counts.get('non_shutdown_mark_exit_ids')}`",
        "",
        "## Outbox",
        "",
        f"- Total: `{outbox_counts.get('total_count')}`",
        f"- Top actions: `{outbox_counts.get('action_top')}`",
        f"- Top stage/actions: `{outbox_counts.get('stage_action_top')}`",
        f"- Top suppress reasons: `{outbox_counts.get('suppress_reason_top')}`",
        f"- Top cancel reasons: `{outbox_counts.get('cancel_reason_top')}`",
        "",
        "## Trade Rejections",
        "",
        f"- Total: `{trade_rejections.get('total_count')}`",
        f"- Top reasons: `{trade_rejections.get('reason_top')}`",
        f"- Top strategies: `{trade_rejections.get('strategy_top')}`",
        "",
        "## Mismatches",
        "",
    ]
    if mismatches:
        for item in mismatches:
            lines.append(f"- `{item.get('check')}`: {item.get('count')} -> {item.get('details')}")
    else:
        lines.append("- none")
    return "\n".join(lines) + "\n"


def write_outputs(report: dict[str, Any], out_dir: Path) -> tuple[Path, Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%d_%H%M%S_%f")
    json_path = out_dir / f"{stamp}_validation_db_integrity_report.json"
    md_path = out_dir / f"{stamp}_validation_db_integrity_report.md"
    latest_json = out_dir / "latest_validation_db_integrity_report.json"
    latest_md = out_dir / "latest_validation_db_integrity_report.md"

    body = json.dumps(report, indent=2)
    markdown = render_markdown(report)
    json_path.write_text(body, encoding="utf-8")
    md_path.write_text(markdown, encoding="utf-8")
    latest_json.write_text(body, encoding="utf-8")
    latest_md.write_text(markdown, encoding="utf-8")
    return json_path, md_path


def main() -> int:
    args = parse_args()
    db_path = Path(args.db).expanduser().resolve()
    out_dir = (ROOT / args.output_dir).resolve()
    report = build_report(db_path, args.top_limit)
    json_path, md_path = write_outputs(report, out_dir)

    print(
        json.dumps(
            {
                "status": report.get("status") or ("ok" if report.get("ok") else "mismatch"),
                "verdict": report.get("verdict"),
                "reason": report.get("reason"),
                "mismatch_count": len(report.get("mismatches") or []),
                "artifacts": {"json": str(json_path), "markdown": str(md_path)},
                "report": report,
            },
            indent=2,
        )
    )
    return 0 if report.get("ok") else 2


if __name__ == "__main__":
    raise SystemExit(main())
