#!/usr/bin/env python3
"""Check each closed trade against what the market actually did.

The bot's own record says a trade opened at one price and closed at another.
Everything else in this repo that checks those prices compares them to a book
snapshot the same code captured at the same instant — which is why a book read
from the wrong end passed every check for months. This asks two sources the bot
never wrote:

  * Polymarket's price history for the token that was bought, around the
    moment of entry and of exit. A fill is credible if it lies between the
    prices the token actually traded at just before and just after, with room
    for one spread and the configured slippage.
  * Gamma's published resolution for the market. An `auto_resolved` exit is
    credible only if it is 1.0 for the side that won and 0.0 for the side that
    lost, and `was_correct` agrees.

Plus the arithmetic the row itself must satisfy, and that the timestamps sit
inside the market's life.

    python3 scripts/verify_trade_truth.py --db trades.db [--limit 50] [--json]

Exit status is 1 for a MISMATCH, 2 for incomplete verification (including no
trades or unavailable evidence), and 0 only when every check is OK or DRIFT.
Entry fees are separate from row.pnl. Non-resolution exits without recorded
exit-fee evidence cannot be independently reconstructed from the trade row.
Unfilled cancellations check explicit zero accounting, not market fill prices.
Confirmed redemptions are not orderbook sales. Their stored finalized receipt
and allocation are checked for local consistency, but remain UNVERIFIABLE here
because this tool does not independently revalidate Polygon receipts. Gas and
external redemption costs are excluded from their recorded P&L.
"""
from __future__ import annotations

import argparse
import json
import math
import sqlite3
import sys
import time
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from pathlib import Path
from native_tls_runtime import require_safe_tls
from http_limits import read_response
from api_auth import open_api_request

CLOB = "https://clob.polymarket.com"
GAMMA = "https://gamma-api.polymarket.com"
BRACKET_SECS = 180          # how far either side of the fill to look for prices
TOLERANCE = 0.03            # one tick spread + max_slippage, generously
ARITHMETIC_TOLERANCE = 1e-6  # floating-point noise, not an allowance for unknown fees
RESOLUTION_GRACE = timedelta(minutes=30)
PAUSE = 0.12                # be polite to the public endpoints

UP_WORDS = {"UP", "YES"}
DOWN_WORDS = {"DOWN", "NO"}
STATUS_RANKS = {"MISMATCH": 3, "UNVERIFIABLE": 2, "DRIFT": 1, "OK": 0}


def parse_ts(raw: str) -> datetime:
    s = raw.strip().replace("Z", "+00:00")
    dt = datetime.fromisoformat(s)
    return dt if dt.tzinfo else dt.replace(tzinfo=timezone.utc)


def http_json(url: str):
    require_safe_tls(url)
    req = urllib.request.Request(url, headers={"User-Agent": "macbot-verify/1"})
    with open_api_request(req, timeout=15) as r:
        return json.loads(read_response(r, label="Public trade evidence response").decode())


def price_history(token_id: str, at: datetime) -> list[tuple[int, float]]:
    ts = int(at.timestamp())
    q = urllib.parse.urlencode({
        "market": token_id,
        "startTs": ts - BRACKET_SECS,
        "endTs": ts + BRACKET_SECS,
        "fidelity": 1,
    })
    time.sleep(PAUSE)
    try:
        h = http_json(f"{CLOB}/prices-history?{q}").get("history", [])
    except Exception:
        return []
    # The endpoint's window is approximate; filter client-side.
    points = []
    for p in h:
        try:
            observed_at, price = int(p["t"]), float(p["p"])
        except (KeyError, TypeError, ValueError, OverflowError):
            continue
        if abs(observed_at - ts) <= BRACKET_SECS and math.isfinite(price) and 0 <= price <= 1:
            points.append((observed_at, price))
    return sorted(points)


def bracket(points: list[tuple[int, float]], at: datetime) -> tuple[float, float] | None:
    """Prices just before and just after `at`, as (lo, hi)."""
    if not points:
        return None
    ts = int(at.timestamp())
    before = [p for t, p in points if t <= ts]
    after = [p for t, p in points if t > ts]
    refs = []
    if before:
        refs.append(before[-1])
    if after:
        refs.append(after[0])
    return (min(refs), max(refs))


_gamma_cache: dict[str, dict | None] = {}


def gamma_market(condition_id: str) -> dict | None:
    if condition_id in _gamma_cache:
        return _gamma_cache[condition_id]
    time.sleep(PAUSE)
    try:
        ms = http_json(f"{GAMMA}/markets?condition_ids={condition_id}&closed=true")
        m = ms[0] if ms else None
    except Exception:
        m = None
    _gamma_cache[condition_id] = m
    return m


def resolution(m: dict) -> str | None:
    """'UP', 'DOWN', or None if the market has not resolved cleanly."""
    if not m or str(m.get("umaResolutionStatus", "")).lower() != "resolved":
        return None
    winner = str(m.get("winner") or "").upper()
    outcomes = m.get("outcomes")
    prices = m.get("outcomePrices")
    try:
        if isinstance(outcomes, str):
            outcomes = json.loads(outcomes)
        if isinstance(prices, str):
            prices = json.loads(prices)
        if outcomes is None and prices is None:
            return "UP" if winner in UP_WORDS else "DOWN" if winner in DOWN_WORDS else None
        if not isinstance(outcomes, list) or not isinstance(prices, list) or len(outcomes) != 2 or len(prices) != 2:
            return None
        values = [float(price) for price in prices]
    except (ValueError, TypeError, OverflowError):
        return None
    labels = [str(label).upper() for label in outcomes]
    if not (any(label in UP_WORDS for label in labels) and any(label in DOWN_WORDS for label in labels)):
        return None
    if not all(math.isfinite(value) and 0 <= value <= 1 for value in values):
        return None
    idx = 0 if values[0] > values[1] else 1
    if values[idx] < 0.99 or values[1 - idx] > 0.01:
        return None
    label = str(outcomes[idx]).upper()
    declared = "UP" if winner in UP_WORDS else "DOWN" if winner in DOWN_WORDS else None
    if label in UP_WORDS:
        return "UP" if declared in (None, "UP") else None
    if label in DOWN_WORDS:
        return "DOWN" if declared in (None, "DOWN") else None
    return None


def judge_price(observed: float, br: tuple[float, float] | None) -> tuple[str, float | None]:
    if br is None:
        return "UNVERIFIABLE", None
    lo, hi = br
    if lo <= observed <= hi:
        return "OK", 0.0
    drift = observed - hi if observed > hi else observed - lo
    if abs(drift) <= TOLERANCE:
        return "DRIFT", round(drift, 4)
    return "MISMATCH", round(drift, 4)


def finite_number(value) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value)


def pnl_check(row: dict) -> dict:
    """Check the row's declared accounting version.

    Legacy resolution P&L excludes entry fees. Receipt-managed P&L is already
    net of its recorded cash fees. Legacy market exits without a fee ledger
    remain unverifiable; a current fee schedule cannot reconstruct history.
    """
    for name in ("pnl", "shares", "entry_price", "exit_price"):
        if row.get(name) is None:
            return {"status": "UNVERIFIABLE", "reason": f"missing {name}"}
        if not finite_number(row[name]):
            return {"status": "MISMATCH", "reason": f"invalid {name}"}
    if row["shares"] <= 0 or not 0 <= row["entry_price"] <= 1 or not 0 <= row["exit_price"] <= 1:
        return {"status": "MISMATCH", "reason": "invalid shares or token price"}
    if row.get("partial_closed"):
        return {"status": "UNVERIFIABLE", "reason": "partial close: the row holds the residual"}
    cash_fees=0.0
    if row.get("accounting_version",0)==1:
        cash_fees=row.get("actual_cash_fees")
        if not finite_number(cash_fees) or cash_fees<0:
            return {"status":"UNVERIFIABLE","reason":"missing receipt-derived cash fees"}
    elif row.get("exit_type") not in ("auto_resolved", "resolution"):
        return {"status": "UNVERIFIABLE", "reason": "exit fees are not separately recorded; cannot reconstruct pnl"}
    implied = row["shares"] * (row["exit_price"] - row["entry_price"]) - cash_fees
    gap = row["pnl"] - implied
    return {"status": "OK" if abs(gap) <= ARITHMETIC_TOLERANCE else "MISMATCH",
            "booked": round(row["pnl"], 6), "implied": round(implied, 6), "gap": round(gap, 6)}


def cancelled_order_checks(row: dict, opened: datetime, closed: datetime | None) -> dict:
    """A cancelled, unfilled order is a closed row, not a market execution.

    Its entry_price/amount can still describe the requested limit and budget.
    Only explicit zero fill quantities, cost, fees and pnl establish that the
    row claims no execution; absent accounting fields are not treated as zero.
    """
    checks = {}
    for field in ("shares", "entry_shares", "entry_cost", "fee_paid", "pnl"):
        value = row.get(field)
        if value is None:
            checks[field] = {"status": "UNVERIFIABLE", "reason": f"missing {field} on cancelled order"}
        elif not finite_number(value) or value != 0:
            checks[field] = {"status": "MISMATCH", "reason": f"unfilled cancellation requires zero {field}"}
        else:
            checks[field] = {"status": "OK", "recorded": value}
    if row.get("partial_closed") or row.get("exit_price") is not None:
        checks["execution"] = {"status": "MISMATCH", "reason": "unfilled cancellation records a partial close or exit price"}
    if closed is None:
        checks["timing"] = {"status": "UNVERIFIABLE", "reason": "no cancellation timestamp recorded"}
    elif closed < opened:
        checks["timing"] = {"status": "MISMATCH", "reason": "cancelled before order opened"}
    else:
        checks["timing"] = {"status": "OK"}
    return checks


def load_redemption_evidence(con: sqlite3.Connection, row: dict, tables: set[str]) -> list[dict]:
    """Follow the dedicated fill's transaction ID, never a Gamma price guess."""
    if not {"trade_fills", "redemption_receipts"} <= tables:
        return []
    fills = con.execute("""SELECT timestamp,avg_price,delta_size,source,details
        FROM trade_fills WHERE trade_id=? AND phase='redemption' LIMIT 2""", (row["id"],))
    evidence = []
    for fill in fills:
        fill = dict(fill)
        try:
            details = json.loads(fill["details"])
            transaction = details.get("transaction_hash") if isinstance(details, dict) else None
        except (TypeError, ValueError):
            transaction = None
        receipt = con.execute("SELECT * FROM redemption_receipts WHERE transaction_hash=?",
                              (transaction,)).fetchone() if transaction else None
        evidence.append({"fill": fill, "receipt": dict(receipt) if receipt else None})
    return evidence


def redemption_exit_check(row: dict, closed: datetime | None) -> dict:
    """Check local allocation only; an old local proof is not fresh chain truth."""
    evidence = row.get("redemption_evidence") or []
    if not evidence:
        return {"status": "UNVERIFIABLE", "reason": "missing finalized redemption receipt evidence"}
    if len(evidence) != 1:
        return {"status": "MISMATCH", "reason": "multiple final redemption fills for one lot"}
    if evidence[0].get("receipt") is None:
        return {"status": "UNVERIFIABLE", "reason": "missing finalized redemption receipt evidence"}
    try:
        fill, receipt = evidence[0]["fill"], evidence[0]["receipt"]
        details = json.loads(fill["details"])
        proof = json.loads(receipt["proof_json"])
        ids = json.loads(receipt["trade_ids_json"])
        result = json.loads(receipt["result_json"])
        token_side = proof["token_ids"].index(row["token_id"])
        selected = [trade for trade in result["trades"] if trade["id"] == row["id"]]
        if len(selected) != 1 or row["id"] not in ids or len(proof["token_ids"]) != 2:
            raise ValueError("receipt selection does not identify this lot")
        allocation = selected[0]
        if len(proof["burned_units"]) != 2 or len(proof["payout_units"]) != 2:
            raise ValueError("receipt is not a two-outcome token redemption")
        burned, payout = proof["burned_units"][token_side], proof["payout_units"][token_side]
        if not all(isinstance(value, int) and not isinstance(value, bool) and 0 <= value <= 2**53
                   for value in (*proof["burned_units"], *proof["payout_units"], proof["total_payout_units"])):
            raise ValueError("invalid exact redemption units")
        if burned <= 0 or sum(proof["payout_units"]) != proof["total_payout_units"]:
            raise ValueError("invalid redemption burn or total payout")
        values = (row["shares"], row["exit_price"], row["pnl"], fill["delta_size"], fill["avg_price"],
                  details["collateral_received"], allocation["shares_redeemed"],
                  allocation["collateral_received"], allocation["total_pnl"])
        if not all(finite_number(value) for value in values) or row["shares"] <= 0:
            raise ValueError("invalid redemption lot quantities or payout")
        units = row["shares"] * 1_000_000
        expected_payout = payout / 1_000_000 * units / burned
        expected_price = expected_payout / row["shares"]
        if abs(units - round(units)) > 1e-4 or units > burned:
            raise ValueError("lot does not fit exact redeemed token quantity")
        if not 0 <= expected_price <= 1 or any(abs(left - right) > ARITHMETIC_TOLERANCE for left, right in (
                (fill["delta_size"], row["shares"]), (allocation["shares_redeemed"], row["shares"]),
                (fill["avg_price"], expected_price), (row["exit_price"], expected_price),
                (details["collateral_received"], expected_payout),
                (allocation["collateral_received"], expected_payout),
                (allocation["total_pnl"], row["pnl"]))):
            raise ValueError("redemption allocation disagrees with stored burn/payout or lot")
        if fill["source"] != "finalized_redemption" or allocation["status"] != "CLOSED":
            raise ValueError("redemption fill lacks finalized provenance")
        if any(receipt[key] != proof[key] for key in (
                "transaction_hash", "account", "condition_id", "block_number", "block_hash", "timestamp")):
            raise ValueError("receipt metadata disagrees with its immutable proof")
        if details["transaction_hash"] != proof["transaction_hash"] or details["token_id"] != row["token_id"]:
            raise ValueError("redemption fill identifies a different receipt or token")
        if closed is None or parse_ts(receipt["timestamp"]) != closed or parse_ts(fill["timestamp"]) != closed:
            raise ValueError("redemption timestamp disagrees with finalized receipt")
        if not isinstance(proof.get("evidence"), dict) or not proof["evidence"]:
            raise ValueError("receipt has no immutable raw chain evidence")
    except (KeyError, IndexError, TypeError, ValueError, OverflowError, AttributeError) as error:
        return {"status": "MISMATCH", "reason": f"contradictory local redemption evidence: {error}"}
    return {"status": "UNVERIFIABLE", "local_consistency": "OK",
            "reason": "stored finalized redemption allocation agrees; Polygon receipt was not independently revalidated",
            "transaction_hash": receipt["transaction_hash"], "collateral_received": expected_payout,
            "pnl_excludes": "gas and external redemption costs", "execution_basis": "token burn and collateral payout; no orderbook sale"}


def verify_trade(row: sqlite3.Row) -> dict:
    row = dict(row)
    out = {"id": row["id"], "direction": row["direction"], "exit_type": row["exit_type"], "checks": {}}
    checks = out["checks"]
    if row.get("timestamp") in (None, ""):
        checks["timing"] = {"status": "UNVERIFIABLE", "reason": "missing entry timestamp"}
        out["verdict"] = "UNVERIFIABLE"
        return out
    try:
        opened = parse_ts(row["timestamp"])
        closed = parse_ts(row["exit_timestamp"]) if row.get("exit_timestamp") else None
    except (ValueError, TypeError, AttributeError, OverflowError):
        checks["timing"] = {"status": "MISMATCH", "reason": "invalid entry or exit timestamp"}
        out["verdict"] = "MISMATCH"
        return out
    if row["exit_type"] == "cancelled":
        out["checks"] = cancelled_order_checks(row, opened, closed)
        out["verification_scope"] = "unfilled cancellation accounting; no market execution claimed"
        out["verdict"] = max((check["status"] for check in out["checks"].values()), key=STATUS_RANKS.get)
        return out
    token = row["token_id"] or ""
    partial = bool(row["partial_closed"])

    # ── entry against the market ─────────────────────────────────────────────
    if row["entry_price"] is None:
        checks["entry"] = {"status": "UNVERIFIABLE", "reason": "no entry price recorded"}
    elif not finite_number(row["entry_price"]) or not 0 <= row["entry_price"] <= 1:
        checks["entry"] = {"status": "MISMATCH", "reason": "invalid entry price"}
    elif token:
        br = bracket(price_history(token, opened), opened)
        status, drift = judge_price(row["entry_price"], br)
        checks["entry"] = {"status": status, "price": row["entry_price"], "market": br, "drift": drift}
    else:
        checks["entry"] = {"status": "UNVERIFIABLE", "reason": "no token_id on the row"}

    # ── exit against the market or the resolution ───────────────────────────
    if row["exit_type"] == "redemption_confirmed":
        checks["exit"] = redemption_exit_check(row, closed)
        out["verification_scope"] = "entry market history and local redemption consistency; fresh chain proof unavailable"
        out["pnl_excludes"] = "gas and external redemption costs"
    elif row["exit_price"] is None or closed is None:
        checks["exit"] = {"status": "UNVERIFIABLE", "reason": "no exit recorded"}
    elif not finite_number(row["exit_price"]) or not 0 <= row["exit_price"] <= 1:
        checks["exit"] = {"status": "MISMATCH", "reason": "invalid exit price"}
    elif row["direction"] not in ("BUY_YES", "BUY_NO"):
        checks["exit"] = {"status": "MISMATCH", "reason": "unknown trade direction"}
    elif row["exit_type"] in ("auto_resolved", "resolution"):
        m = gamma_market(row["market_id"])
        res = resolution(m) if m else None
        if res is None:
            checks["exit"] = {"status": "UNVERIFIABLE", "reason": "market not resolved on Gamma"}
        else:
            bought_up = row["direction"] == "BUY_YES"
            token_won = (res == "UP") == bought_up
            expected = 1.0 if token_won else 0.0
            ok = abs(row["exit_price"] - expected) < 1e-9
            c = {"status": "OK" if ok else "MISMATCH", "price": row["exit_price"],
                 "resolution": res, "expected": expected}
            # was_correct is the whole position's verdict; for a partial close
            # it reflects the realised pnl, not the residual's resolution.
            if not partial and row["was_correct"] is not None:
                if bool(row["was_correct"]) != token_won:
                    c["status"] = "MISMATCH"
                    c["was_correct_disagrees"] = True
            checks["exit"] = c
    elif token:
        br = bracket(price_history(token, closed), closed)
        status, drift = judge_price(row["exit_price"], br)
        checks["exit"] = {"status": status, "price": row["exit_price"], "market": br, "drift": drift}
    else:
        checks["exit"] = {"status": "UNVERIFIABLE", "reason": "no token_id on the row"}

    # ── the row's own arithmetic ─────────────────────────────────────────────
    checks["pnl"] = pnl_check(row)
    entry_values = [row.get(name) for name in ("entry_cost", "entry_shares", "entry_price")]
    if any(value is None for value in entry_values):
        checks["entry_cost"] = {"status": "UNVERIFIABLE", "reason": "missing original entry cost, shares, or price"}
    elif not all(finite_number(value) for value in entry_values) or row["entry_shares"] <= 0 or row["entry_cost"] <= 0:
        checks["entry_cost"] = {"status": "MISMATCH", "reason": "invalid original entry cost or shares"}
    else:
        implied_cost = row["entry_price"] * row["entry_shares"]
        gap = row["entry_cost"] - implied_cost
        checks["entry_cost"] = {"status": "OK" if abs(gap) <= ARITHMETIC_TOLERANCE else "MISMATCH",
                                "recorded": round(row["entry_cost"], 4), "implied": round(implied_cost, 4)}

    # ── timestamps inside the market's life ─────────────────────────────────
    t = {"status": "OK"}
    if closed is not None and closed < opened:
        t = {"status": "MISMATCH", "reason": "closed before it opened"}
    else:
        m = None if row["exit_type"] == "redemption_confirmed" else gamma_market(row["market_id"])
        end_raw = (m or {}).get("endDate") or row["end_date"]
        if end_raw:
            try:
                end = parse_ts(end_raw)
                if opened > end:
                    t = {"status": "MISMATCH", "reason": f"opened after the market ended ({end.isoformat()})"}
                elif row["exit_type"] != "redemption_confirmed" and closed is not None and closed > end + RESOLUTION_GRACE:
                    t = {"status": "MISMATCH", "reason": f"closed {closed - end} after the market ended"}
            except ValueError:
                t = {"status": "UNVERIFIABLE", "reason": f"unparseable end date {end_raw!r}"}
        else:
            t = {"status": "UNVERIFIABLE", "reason": "no end date"}
    checks["timing"] = t

    out["verdict"] = max((c["status"] for c in checks.values()), key=STATUS_RANKS.get)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", required=True)
    ap.add_argument("--limit", type=int, default=50, help="most recent closed trades to check")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()

    con = sqlite3.connect(Path(a.db).resolve().as_uri() + "?mode=ro", uri=True)
    con.row_factory = sqlite3.Row
    rows=[dict(r) for r in con.execute(
        "SELECT * FROM trades WHERE status='CLOSED' ORDER BY id DESC LIMIT ?",(a.limit,))]
    tables={r[0] for r in con.execute("SELECT name FROM sqlite_master WHERE type='table'")}
    for row in rows:
        if row.get("exit_type") == "redemption_confirmed":
            row["redemption_evidence"] = load_redemption_evidence(con, row, tables)
        if row.get("accounting_version",0)!=1:
            continue
        if {"live_matches","live_orders","maker_match_allocations"} <= tables:
            row["actual_cash_fees"]=con.execute("""SELECT
                COALESCE((SELECT SUM(m.fee) FROM live_matches m JOIN live_orders o ON o.order_id=m.order_id WHERE o.trade_id=?),0.0)+
                COALESCE((SELECT SUM(fee_usd) FROM maker_match_allocations WHERE trade_id=?),0.0)""",(row["id"],row["id"])).fetchone()[0]
    con.close()

    results = [verify_trade(r) for r in rows]
    summary = {"checked": len(results)}
    for v in ("OK", "DRIFT", "UNVERIFIABLE", "MISMATCH"):
        summary[v.lower()] = sum(1 for r in results if r["verdict"] == v)
    by_check: dict[str, dict[str, int]] = {}
    for r in results:
        for name, c in r["checks"].items():
            by_check.setdefault(name, {}).setdefault(c["status"], 0)
            by_check[name][c["status"]] += 1
    summary["by_check"] = by_check

    if a.json:
        print(json.dumps({"summary": summary, "trades": results}, indent=2))
    else:
        for r in results:
            if r["verdict"] == "OK":
                continue
            bits = [f"{k}={c['status']}" + (f"({c.get('drift') or c.get('gap') or c.get('reason','')})" if c["status"] != "OK" else "")
                    for k, c in r["checks"].items() if c["status"] != "OK"]
            print(f"  #{r['id']:<4} {r['direction']:<8} {str(r['exit_type']):<14} {r['verdict']:<12} {'  '.join(bits)}")
        print(f"trade truth: {summary['checked']} checked — "
              f"{summary['ok']} ok, {summary['drift']} drift, {summary['unverifiable']} unverifiable, "
              f"{summary['mismatch']} MISMATCH")
        for name, counts in by_check.items():
            print(f"  {name:<11} " + "  ".join(f"{k.lower()}={v}" for k, v in sorted(counts.items())))
    return verification_exit_code(results)


def verification_exit_code(results: list[dict]) -> int:
    statuses = {check["status"] for result in results for check in result["checks"].values()}
    if "MISMATCH" in statuses:
        return 1
    if not results or "UNVERIFIABLE" in statuses:
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
