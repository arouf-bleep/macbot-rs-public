"""
Backfill outcome labels in ML archive tables from the Polymarket CLOB API.

Each market_id is a condition_id. The API returns token winners for resolved markets.
If the "Up" token won, outcome = 1.0; if "Down" won, outcome = 0.0.

Usage:
  python3 backfill_outcomes.py --db ./trades.db
"""

import argparse
import json
import sqlite3
import time
import urllib.request
import urllib.error
from pathlib import Path

from sqlite_runtime import connect_for_write
from network_runtime import require_safe_tls, read_response, open_api_request


CLOB_API = "https://clob.polymarket.com/markets"


def fetch_market(condition_id: str):
    """Fetch market data from Polymarket CLOB API."""
    url = f"{CLOB_API}/{condition_id}"
    require_safe_tls(url)
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "macbot-backfill/1.0"})
        with open_api_request(req, timeout=10) as resp:
            return json.loads(read_response(resp, label="Outcome backfill response"))
    except (urllib.error.URLError, urllib.error.HTTPError, json.JSONDecodeError) as e:
        print(f"  Error fetching {condition_id[:16]}...: {e}")
        return None


def determine_outcome(market: dict):
    """Determine outcome from market data. Returns 1.0 (UP), 0.0 (DOWN), or None (unresolved)."""
    if not isinstance(market, dict) or market.get("closed") is not True:
        return None  # Still open

    tokens = market.get("tokens", [])
    if not isinstance(tokens, list):
        return None
    winners = [token for token in tokens if isinstance(token, dict) and token.get("winner") is True]
    if len(winners) != 1:
        return None
    for token in winners:
        outcome_str = str(token.get("outcome") or "").lower()
        winner = token.get("winner", False)

        if winner is True:
            if outcome_str.strip() in {"up", "yes"}:
                return 1.0
            elif outcome_str.strip() in {"down", "no"}:
                return 0.0

    return None  # Closed but can't determine winner


def main():
    parser = argparse.ArgumentParser(description="Backfill outcomes from Polymarket API")
    parser.add_argument("--db", required=True, help="Path to SQLite database")
    parser.add_argument("--dry-run", action="store_true", help="Don't write to DB")
    parser.add_argument("--delay", type=float, default=0.2, help="Delay between API calls (seconds)")
    args = parser.parse_args()
    require_safe_tls(CLOB_API)

    conn = (
        sqlite3.connect(Path(args.db).expanduser().resolve().as_uri() + "?mode=ro", uri=True)
        if args.dry_run
        else connect_for_write(args.db, operation="Outcome backfill")
    )

    # Get all market_ids missing outcomes in either archive table
    rows = conn.execute(
        """
        SELECT DISTINCT market_id
        FROM (
            SELECT market_id FROM ml_features_timeseries WHERE outcome IS NULL
            UNION
            SELECT market_id FROM ml_model_inputs WHERE outcome IS NULL
        )
        """
    ).fetchall()

    market_ids = [r[0] for r in rows]
    print(f"Found {len(market_ids)} markets with missing outcomes")

    updated = 0
    skipped = 0
    errors = 0

    for i, mid in enumerate(market_ids):
        # Count rows for this market
        feature_count = conn.execute(
            "SELECT COUNT(*) FROM ml_features_timeseries WHERE market_id = ? AND outcome IS NULL",
            (mid,),
        ).fetchone()[0]
        model_input_count = conn.execute(
            "SELECT COUNT(*) FROM ml_model_inputs WHERE market_id = ? AND outcome IS NULL",
            (mid,),
        ).fetchone()[0]

        market = fetch_market(mid)
        if market is None:
            errors += 1
            continue

        outcome = determine_outcome(market)
        if outcome is None:
            skipped += 1
            if (i + 1) % 50 == 0:
                print(f"  [{i+1}/{len(market_ids)}] {mid[:16]}... still open, skipping")
            continue

        label = "UP" if outcome == 1.0 else "DOWN"
        if not args.dry_run:
            conn.execute(
                "UPDATE ml_features_timeseries SET outcome = ? WHERE market_id = ? AND outcome IS NULL",
                (outcome, mid),
            )
            conn.execute(
                "UPDATE ml_model_inputs SET outcome = ? WHERE market_id = ? AND outcome IS NULL",
                (outcome, mid),
            )
            conn.commit()

        updated += 1
        if updated % 20 == 0 or updated <= 5:
            print(
                f"  [{i+1}/{len(market_ids)}] {mid[:16]}... → {label} "
                f"(features={feature_count}, model_inputs={model_input_count})"
            )

        time.sleep(args.delay)

    print(f"\nDone!")
    print(f"  Updated: {updated} markets")
    print(f"  Skipped (still open): {skipped}")
    print(f"  Errors: {errors}")

    # Show final counts
    total_features = conn.execute("SELECT COUNT(*) FROM ml_features_timeseries").fetchone()[0]
    with_outcome_features = conn.execute(
        "SELECT COUNT(*) FROM ml_features_timeseries WHERE outcome IS NOT NULL"
    ).fetchone()[0]
    total_model_inputs = conn.execute("SELECT COUNT(*) FROM ml_model_inputs").fetchone()[0]
    with_outcome_model_inputs = conn.execute(
        "SELECT COUNT(*) FROM ml_model_inputs WHERE outcome IS NOT NULL"
    ).fetchone()[0]
    print(
        f"  Feature rows: {total_features}, with outcome: {with_outcome_features} "
        f"({100*with_outcome_features/max(total_features,1):.1f}%)"
    )
    print(
        f"  Model-input rows: {total_model_inputs}, with outcome: {with_outcome_model_inputs} "
        f"({100*with_outcome_model_inputs/max(total_model_inputs,1):.1f}%)"
    )

    conn.close()


if __name__ == "__main__":
    main()
