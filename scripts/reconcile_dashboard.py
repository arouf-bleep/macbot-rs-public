#!/usr/bin/env python3
"""Every figure the dashboard shows, checked against where it comes from.

The page has three sources for most numbers — the SSE stream, the REST
endpoints it polls, and the database underneath both — and they have
disagreed before without anything noticing. This takes all three as close to
one instant as the process allows and lays each figure out side by side:

    figure          stream      rest        database    verdict

A figure is OK when its required sources are present and every available
source agrees within tolerance. Non-live balance is reconstructed from the
database; live balance requires an authoritative wallet reference, not the
non-live ledger formula. Fees compare closed-trade entry fees only.

    python3 scripts/reconcile_dashboard.py --db trades.db [--base http://127.0.0.1:8000] [--json]

Needs the bot running with MACBOT_API_KEY / MACBOT_API_SECRET (or --api-key /
--api-secret). Exits 1 on disagreement or invalid data, 2 on incomplete
verification, and 0 on agreement. No wallet synchronization is requested.
"""
from __future__ import annotations

import argparse
import json
import math
import sqlite3
import sys
import time
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from api_auth import add_api_auth_args, get_json, resolve_api_key, open_api_request  # noqa: E402
from http_limits import MAX_RESPONSE_BYTES  # noqa: E402

MONEY_TOL = 0.011      # a cent, plus float noise
PCT_TOL = 0.11         # a tenth of a percent
COUNT_TOL = 0.0


def one_stream_payload(base: str, api_key: str, timeout: int = 8) -> dict:
    """The first full payload from /api/stream."""
    req = urllib.request.Request(f"{base}/api/stream", headers={"X-API-Key": api_key})
    deadline = time.monotonic() + timeout
    received = 0
    with open_api_request(req, timeout=timeout) as r:
        while time.monotonic() <= deadline:
            raw = r.readline(MAX_RESPONSE_BYTES + 1)
            if not raw:
                break
            received += len(raw)
            if len(raw) > MAX_RESPONSE_BYTES or received > MAX_RESPONSE_BYTES:
                raise RuntimeError("Dashboard stream exceeds the8MiB frame/read limit")
            line = raw.decode().strip()
            if line.startswith("data:"):
                d = json.loads(line[5:])
                if d.get("kind") in ("full", "fast"):
                    return d
    return {}


def db_truth(path: str, capital_origin=None) -> dict:
    con = sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    # These aggregates must describe one database snapshot even during trading.
    con.execute("BEGIN")
    q = lambda sql, *a: con.execute(sql, a).fetchone()[0]
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    columns={r[1] for r in con.execute("PRAGMA table_info(trades)")}
    net = "pnl - CASE WHEN accounting_version=1 THEN 0 ELSE COALESCE(fee_paid,0) END" if "accounting_version" in columns else "pnl-COALESCE(fee_paid,0)"
    closed = q("SELECT COUNT(*) FROM trades WHERE status='CLOSED'")
    wins = q(f"SELECT COUNT(*) FROM trades WHERE status='CLOSED' AND ({net}) > 0")
    daily = daily_realized_truth(con, today, columns, capital_origin)
    result = {
        "closed_trades": closed,
        "open_trades": q("SELECT COUNT(*) FROM trades WHERE status IN ('OPEN','CLOSING')"),
        "wins": wins,
        "win_rate_pct": (wins / closed * 100.0) if closed else 0.0,
        # Paper pnl keeps entry fees separate. Confirmed live pnl already
        # includes both actual entry and exit fees from settlement receipts.
        "total_pnl_net": q(f"SELECT COALESCE(SUM({net}),0) FROM trades WHERE status='CLOSED'"),
        "today_pnl": daily["exact_today_pnl"],
        "daily_pnl_evidence": daily,
        "closed_entry_fees": q("SELECT COALESCE(SUM(fee_paid),0) FROM trades WHERE status='CLOSED'"),
        # An independent reconstruction of the non-live cash ledger. Open
        # positions reserve remaining cost plus entry fee; partial exits may
        # already have realised pnl while the remainder is still open.
        "closed_non_live_delta": q("SELECT COALESCE(SUM(pnl - COALESCE(fee_paid,0)),0) FROM trades WHERE status='CLOSED' AND mode!='live'"),
        "open_non_live_locked": q("SELECT COALESCE(SUM(amount + COALESCE(fee_paid,0)),0) FROM trades WHERE status IN ('OPEN','CLOSING') AND mode!='live'"),
        "open_non_live_realized": q("SELECT COALESCE(SUM(pnl),0) FROM trades WHERE status IN ('OPEN','CLOSING') AND mode!='live'"),
        "missing_closed_pnl": q("SELECT COUNT(*) FROM trades WHERE status='CLOSED' AND pnl IS NULL"),
        "missing_accounting_fields": q("SELECT COUNT(*) FROM trades WHERE status IN ('OPEN','CLOSING','CLOSED') AND (fee_paid IS NULL OR mode IS NULL OR (status IN ('OPEN','CLOSING') AND amount IS NULL))"),
    }
    con.close()
    return result


def utc_day(value):
    if not isinstance(value, str):
        return None
    try:
        stamp = datetime.fromisoformat(value.replace("Z", "+00:00"))
        return stamp.astimezone(timezone.utc).date().isoformat() if stamp.tzinfo else None
    except ValueError:
        return None


def daily_realized_truth(con, today, columns, capital_origin=None):
    """Independent snapshot reconstruction of the runtime realization basis.

    Quantity and P&L coverage prove all sales are allocated, including zero-net
    history. Undated cross-day partials make the exact daily figure unavailable.
    """
    if capital_origin not in (None, "live", "non_live"):
        raise ValueError("unknown capital origin")
    live = None if capital_origin is None else int(capital_origin == "live")
    result = {"day_utc": today, "capital_origin": capital_origin, "dated_today_pnl": 0.0, "exact_today_pnl": None,
              "conservative_unallocated_loss": 0.0, "evidence_complete": True,
              "unallocated_trade_ids": []}
    has_events = con.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='realized_pnl_events'").fetchone()
    totals = {}
    if has_events:
        events_sql = "SELECT trade_id,timestamp,net_pnl,realized_shares FROM realized_pnl_events" if live is None else "SELECT e.trade_id,e.timestamp,e.net_pnl,e.realized_shares FROM realized_pnl_events e LEFT JOIN trades t ON t.id=e.trade_id WHERE (LOWER(TRIM(t.mode))='live')=? OR t.id IS NULL"
        for event in con.execute(events_sql, () if live is None else (live,)):
            day = utc_day(event["timestamp"])
            pnl, shares = float(event["net_pnl"]), float(event["realized_shares"])
            if day is None or not math.isfinite(pnl) or not math.isfinite(shares) or shares <= 0:
                raise ValueError("invalid realized event evidence")
            total = totals.setdefault(event["trade_id"], {"pnl":0.0,"shares":0.0,"dated":0.0,"first":day,"last":day,"has_day":False})
            total["pnl"] += pnl; total["shares"] += shares
            total["first"] = min(total["first"],day); total["last"] = max(total["last"],day)
            if day == today:
                total["dated"] += pnl; total["has_day"] = True
    trades_sql = "SELECT * FROM trades WHERE status IN ('OPEN','CLOSING','CLOSED')"
    if live is not None:
        trades_sql += " AND (LOWER(TRIM(mode))='live')=?"
    for raw in con.execute(trades_sql, () if live is None else (live,)):
        row = dict(raw); trade_id = row.get("id")
        total = totals.pop(trade_id, {"pnl":0.0,"shares":0.0,"dated":0.0,"first":None,"last":None,"has_day":False})
        pnl, fee = float(row.get("pnl") or 0), float(row.get("fee_paid") or 0)
        remaining = float(row.get("shares") or 0)
        if not all(math.isfinite(v) for v in (pnl,fee,remaining)) or min(fee,remaining) < 0:
            raise ValueError("invalid realized lot amounts")
        closed, partial = row["status"] == "CLOSED", bool(row.get("partial_closed"))
        if not closed and not partial and pnl == 0 and total["shares"] == 0:
            continue
        created, exited = utc_day(row.get("timestamp")), utc_day(row.get("exit_timestamp"))
        if (created and total["first"] and total["first"] < created) or (closed and exited and total["last"] and total["last"] > exited):
            raise ValueError("realized event outside lot lifecycle")
        original = row.get("entry_shares")
        original = float(original) if original is not None else None
        if original is not None and (not math.isfinite(original) or original <= 0):
            original = None
        if not closed and original is not None and remaining > original + 1e-7*max(original,1):
            raise ValueError("remaining quantity exceeds original lot")
        realized_shares = (original if closed else max(original-remaining,0)) if original is not None else None
        realized_fee = (0 if row.get("accounting_version") == 1 else fee if closed or original is None
                        else fee * realized_shares / original)
        target = pnl-realized_fee; residual = target-total["pnl"]
        if realized_shares is not None and total["shares"] > realized_shares + 1e-7*max(realized_shares,1):
            raise ValueError("realized event quantity exceeds lot")
        result["dated_today_pnl"] += total["dated"]
        complete = realized_shares is not None and abs(realized_shares-total["shares"]) <= 1e-7*max(realized_shares,1)
        if complete and abs(residual) > 1e-7*max(abs(target),abs(total["pnl"]),1):
            raise ValueError("complete realized event quantities disagree with lot P&L")
        if complete and abs(residual) <= 1e-7*max(abs(target),abs(total["pnl"]),1):
            continue
        single_close = closed and not partial and total["shares"] == 0
        single_day = created is not None and (created == exited if closed else created == today)
        if (single_close or single_day) and (exited if closed else created):
            if (exited if closed else created) == today:
                result["dated_today_pnl"] += residual
            continue
        if (closed and exited and exited < today and not total["has_day"]) or (created and created > today and not total["has_day"]):
            continue
        result["evidence_complete"] = False
        result["unallocated_trade_ids"].append(trade_id)
        result["conservative_unallocated_loss"] += max(-residual,0)
    if totals or not all(math.isfinite(result[k]) for k in ("dated_today_pnl","conservative_unallocated_loss")):
        raise ValueError("invalid or orphaned realized P&L event")
    if result["evidence_complete"]:
        result["exact_today_pnl"] = result["dated_today_pnl"]
    return result


def pct(v, *, fraction: bool):
    """Normalize the declared source unit, never guess from the value."""
    if v is None:
        return None
    try:
        return float(v) * (100.0 if fraction else 1.0)
    except (ValueError, TypeError):
        return v  # The comparison reports this as INVALID rather than dropping it.


def compare_figure(name, tolerance, sources, required=(), reason=None) -> dict:
    present = {key: value for key, value in sources.items() if value is not None}
    missing = [key for key in required if sources.get(key) is None]
    if tolerance is None:
        disagree = len({str(value) for value in present.values()}) > 1
        invalid = False
    else:
        try:
            values = [float(value) for value in present.values()]
            invalid = not all(math.isfinite(value) for value in values)
            disagree = bool(values) and max(values) - min(values) > tolerance
        except (TypeError, ValueError, OverflowError):
            invalid, disagree = True, False
    if invalid:
        verdict = "INVALID"
    elif disagree:
        verdict = "DISAGREE"
    elif missing or len(present) < 2 or reason:
        verdict = "UNVERIFIABLE"
    else:
        verdict = "OK"
    result = {"figure": name, "verdict": verdict, "sources": sources}
    if missing:
        result["missing_sources"] = missing
    if reason:
        result["reason"] = reason
    return result


def take_and_compare(a, key, auth) -> list[dict]:
    # ── take every source as close together as we can ────────────────────────
    stream = one_stream_payload(a.base, key)
    rest_stats_env = get_json(f"{a.base}/api/stats", **auth)
    # /api/stats nests the performance figures; the stream flattens them.
    rest_stats = rest_stats_env.get("performance") or rest_stats_env
    rest_analysis = get_json(f"{a.base}/api/trades/analysis", **auth)
    rest_truth = get_json(f"{a.base}/api/accounting/truth", **auth)
    rest_trades = get_json(f"{a.base}/api/trades", **auth)
    rest_loop = get_json(f"{a.base}/api/loop/status", **auth)
    rest_cfg = get_json(f"{a.base}/api/config", **auth)
    selected_mode = str(rest_cfg.get("mode") or "").strip().lower()
    capital_origin = "live" if selected_mode == "live" else "non_live" if selected_mode in ("paper", "shadow", "simulation", "sim") else None
    db = db_truth(a.db, capital_origin)

    ss = stream.get("stats") or {}
    rt = rest_trades if isinstance(rest_trades, list) else rest_trades.get("trades")
    rest_open = sum(1 for t in rt if t.get("status") in ("OPEN", "CLOSING")) if isinstance(rt, list) else None
    rest_closed = sum(1 for t in rt if t.get("status") == "CLOSED") if isinstance(rt, list) else None
    truth_bal = rest_truth.get("current_balance") if isinstance(rest_truth, dict) else None
    if truth_bal is None and isinstance(rest_truth, dict):
        truth_bal = (rest_truth.get("balance") or {}).get("current")

    mode = str(rest_cfg.get("mode") or "").strip().lower()
    balance_sources = {"stream": stream.get("balance"), "stream/stats": ss.get("current_balance"),
                       "rest": rest_stats.get("current_balance"), "rest/env": rest_stats_env.get("balance"),
                       "accounting": truth_bal}
    balance_required = ["stream", "rest", "accounting"]
    balance_reason = None
    if mode in ("paper", "shadow", "simulation", "sim"):
        initial = rest_cfg.get("initial_balance")
        derived = None
        if initial is not None:
            try:
                derived = (float(initial) + db["closed_non_live_delta"]
                           - db["open_non_live_locked"] + db["open_non_live_realized"])
            except (ValueError, TypeError):
                derived = float("nan")
        balance_sources.update({"db": derived, "accounting/derived": rest_truth.get("derived_restart_balance")})
        balance_required += ["db", "accounting/derived"]
    elif mode == "live":
        # A wallet can have deposits, withdrawals and external trades. The
        # paper restart formula cannot establish its collateral balance.
        balance_sources["authoritative"] = rest_truth.get("authoritative_live_balance")
        balance_required.append("authoritative")
        if balance_sources["authoritative"] is None:
            balance_reason = "live balance has no authoritative wallet reference; cached balances alone cannot verify it"
    else:
        balance_reason = "unknown bot mode; cannot select the balance accounting contract"

    # figure: (tolerance, {source: value})
    figures = {
        "closed trades":   (COUNT_TOL, {"stream": ss.get("closed_trades"), "rest": rest_stats.get("closed_trades"),
                                        "rest/trades": rest_closed, "analysis": rest_analysis.get("total_closed"), "db": db["closed_trades"]}),
        "open positions":  (COUNT_TOL, {"stream": ss.get("open_trades"), "rest": rest_stats.get("open_trades"),
                                        "rest/trades": rest_open, "analysis": rest_analysis.get("total_open"), "db": db["open_trades"]}),
        "wins":            (COUNT_TOL, {"stream": ss.get("wins"), "rest": rest_stats.get("wins"),
                                        "analysis": rest_analysis.get("wins"), "db": db["wins"]}),
        "win rate %":      (PCT_TOL,   {"stream": pct(ss.get("win_rate"), fraction=True), "rest": pct(rest_stats.get("win_rate"), fraction=True),
                                        "analysis": pct(rest_analysis.get("win_rate"), fraction=False), "db": db["win_rate_pct"]}),
        "total pnl (net)": (MONEY_TOL, {"stream": ss.get("total_pnl_net"), "rest": rest_stats.get("total_pnl_net"),
                                        "analysis": rest_analysis.get("total_pnl_net"), "db": db["total_pnl_net"]}),
        "today pnl":       (MONEY_TOL, {"stream": stream.get("daily_pnl"), "stream/stats": ss.get("today_pnl"),
                                        "rest": rest_stats.get("today_pnl"), "rest/env": rest_stats_env.get("daily_pnl"), "db": db["today_pnl"]}),
        "balance":         (MONEY_TOL, balance_sources),
        "closed entry fees": (MONEY_TOL, {"stream": ss.get("total_entry_fees"), "rest": rest_stats.get("total_entry_fees"), "db": db["closed_entry_fees"]}),
        "loop running":    (COUNT_TOL, {"stream": stream.get("loop_running"), "rest": rest_loop.get("running")}),
        "execution mode":  (None,      {"stream": stream.get("execution_mode"), "rest": rest_cfg.get("execution_mode")}),
        "bot mode":        (None,      {"stream": stream.get("mode"), "rest": rest_cfg.get("mode"), "loop": rest_loop.get("mode")}),
    }

    required = {
        "closed trades": ("stream", "rest", "rest/trades", "analysis", "db"),
        "open positions": ("rest/trades", "analysis", "db"),
        "wins": ("stream", "rest", "analysis", "db"),
        "win rate %": ("stream", "rest", "analysis", "db"),
        "total pnl (net)": ("stream", "rest", "analysis", "db"),
        "today pnl": ("stream", "rest", "db"),
        "balance": balance_required,
        "closed entry fees": ("stream", "rest", "db"),
        "loop running": ("stream", "rest"),
        "execution mode": ("stream", "rest"),
        "bot mode": ("stream", "rest", "loop"),
    }
    results = [compare_figure(name, tol, sources, required[name],
                             balance_reason if name == "balance" else
                             "undated partial-sale history; exact daily P&L unavailable" if name == "today pnl" and not db["daily_pnl_evidence"]["evidence_complete"] else None)
               for name, (tol, sources) in figures.items()]
    missing_rows = db["missing_closed_pnl"] + db["missing_accounting_fields"]
    if missing_rows:
        results.append({"figure": "database evidence", "verdict": "INVALID", "sources": {},
                        "reason": "missing mandatory pnl, fee, mode, or open-position amount in the ledger"})
    return results


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", required=True)
    ap.add_argument("--base", default="http://127.0.0.1:8000")
    ap.add_argument("--json", action="store_true")
    add_api_auth_args(ap)
    a = ap.parse_args()
    key = resolve_api_key(a.api_key)
    auth = dict(api_key=a.api_key, api_secret=a.api_secret, read_auth=True)

    results = take_and_compare(a, key, auth)
    if any(r["verdict"] == "DISAGREE" for r in results):
        # The bot trades while this runs. A trade closing between two reads
        # looks like a disagreement and is not one; a second take settles it.
        time.sleep(1.5)
        results = take_and_compare(a, key, auth)
    disagree = [r for r in results if r["verdict"] in ("DISAGREE", "INVALID")]
    incomplete = [r for r in results if r["verdict"] == "UNVERIFIABLE"]
    if a.json:
        print(json.dumps({"taken_at": datetime.now(timezone.utc).isoformat(), "figures": results}, indent=2, default=str))
    else:
        cols = sorted({k for r in results for k in r["sources"]})
        fmt = lambda v: "·" if v is None else (f"{v:.2f}" if isinstance(v, float) else str(v))
        print(f"{'figure':<16} " + " ".join(f"{c:>12}" for c in cols) + "   verdict")
        for r in results:
            print(f"{r['figure']:<16} " + " ".join(f"{fmt(r['sources'].get(c)):>12}" for c in cols) + f"   {r['verdict']}")
            if r.get("reason") or r.get("missing_sources"):
                print("  " + r.get("reason", "missing required sources: " + ", ".join(r.get("missing_sources", []))))
        print(f"\ndashboard reconciliation: {len(results)} figures, {len(disagree)} disagree/invalid, {len(incomplete)} incomplete")
    return reconciliation_exit_code(results)


def reconciliation_exit_code(results: list[dict]) -> int:
    if any(result["verdict"] in ("DISAGREE", "INVALID") for result in results):
        return 1
    if not results or any(result["verdict"] != "OK" for result in results):
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
