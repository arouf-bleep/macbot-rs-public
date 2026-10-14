#!/usr/bin/env python3
"""
Historical data backfill for deep model training.

Pulls resolved Polymarket BTC 15m/5m markets + corresponding Binance 1m candles
to build a labeled training dataset in the same SQLite schema that train.py expects.

Usage:
    python backfill_historical.py --db ./data/macbot.db --days 30 --interval 15m
    python backfill_historical.py --db ./data/macbot.db --days 14 --interval 5m
    python backfill_historical.py --db ./data/macbot.db --days 30 --interval both
"""

import argparse
import hashlib
import hmac
import json
import math
import os
import sqlite3
import time
from bisect import bisect_right
from datetime import datetime, timezone, timedelta
from pathlib import Path
from typing import Optional, List, Dict, Tuple

import requests
from sqlite_runtime import connect_for_write
from network_runtime import require_safe_tls, MAX_RESPONSE_BYTES


def _https_get(url, **kwargs):
    require_safe_tls(url)
    kwargs.setdefault("allow_redirects", False)
    # Count decoded streamed bytes before Response.json/text can allocate an
    # unbounded body; Requests' default buffers everything before returning.
    response = requests.get(url, stream=True, **kwargs)
    try:
        body = bytearray()
        for chunk in response.iter_content(chunk_size=64 * 1024):
            if len(body) + len(chunk) > MAX_RESPONSE_BYTES:
                raise RuntimeError("Historical HTTP response exceeds the 8MiB response limit")
            body.extend(chunk)
        response._content = bytes(body)
        response._content_consumed = True
        return response
    finally:
        response.close()


def _next_page_start(last_timestamp, current_start):
    """Reject missing/repeated timestamps before retaining a pagination page."""
    if last_timestamp is None or isinstance(last_timestamp, bool):
        raise ValueError("Pagination response has no valid final timestamp")
    next_start = int(last_timestamp) + 1
    if next_start <= current_start:
        raise ValueError("Pagination response does not advance the requested start time")
    return next_start

# ── Constants ────────────────────────────────────────────────────────────────

GAMMA_API = "https://gamma-api.polymarket.com"
BINANCE_API = "https://api.binance.com"
BINANCE_FUTURES_API = "https://fapi.binance.com"

LEGACY_FEATURE_KEYS = [
    "spread",
    "imbalance",
    "aggressor_ratio",
    "volatility",
    "momentum",
    "mean_reversion",
    "vwap_deviation",
    "rsi",
    "funding_rate",
    "open_interest",
    "news_sentiment",
    "social_sentiment",
    "whale_activity",
    "liquidation_pressure",
]

# Keep aligned with training/train.py FEATURE_NAMES and src/ml/model.rs.
FEATURE_NAMES = [
    "orderbook_imbalance",
    "tick_velocity",
    "vwap_confluence",
    "price_divergence",
    "spike_detector",
    "deribit_pcr",
    "rsi_macd_ha",
    "fear_greed",
    "funding_rate",
    "oi_momentum",
    "aggressor_ratio",
    "coingecko_sentiment",
    "finnhub_sentiment",
    "lunarcrush_sentiment",
    "hour_of_day",
    "day_of_week",
    "distance_from_round_1000",
    "distance_from_round_500",
    "volatility_10",
    "volatility_30",
    "window_momentum",
    "long_liq_volume",
    "short_liq_volume",
    "depth_imbalance_5",
    "polymarket_flow",
    "vpin_microstructure",
    "options_advanced",
    "whale_flow",
    "cross_market",
    "latency_arb",
    "news_sentiment",
    "btc_final_momentum",
    "kyle_lambda",
    "hawkes_flow",
]

FEATURE_INDEX = {name: idx for idx, name in enumerate(FEATURE_NAMES)}

HISTORICAL_ALWAYS_UNAVAILABLE = {
    "fear_greed",
    "coingecko_sentiment",
    "finnhub_sentiment",
    "lunarcrush_sentiment",
    "depth_imbalance_5",
    "polymarket_flow",
    "latency_arb",
}

HISTORICAL_TRAIN_DERIVED = {
    "hour_of_day",
    "day_of_week",
    "distance_from_round_1000",
    "distance_from_round_500",
    "volatility_10",
    "volatility_30",
    "window_momentum",
}

# Lookback: how many 1m candles to pull before each market window
CANDLE_LOOKBACK_MINUTES = 240  # 4 hours

# Rate limiting
GAMMA_DELAY = 0.05    # 50ms between Gamma requests
BINANCE_DELAY = 0.2   # 200ms between Binance requests


def _get_api_key() -> Optional[str]:
    return os.environ.get("BINANCE_FUTURES_API_KEY") or os.environ.get("BINANCE_API_KEY")


def _get_api_secret() -> Optional[str]:
    return os.environ.get("BINANCE_FUTURES_API_SECRET") or os.environ.get("BINANCE_API_SECRET")


def _binance_headers() -> dict:
    api_key = _get_api_key()
    return {"X-MBX-APIKEY": api_key} if api_key else {}


def _sign_params(params: Dict[str, str]) -> Dict[str, str]:
    secret = _get_api_secret()
    if not secret:
        return params
    query = "&".join(f"{k}={v}" for k, v in params.items())
    signature = hmac.new(secret.encode(), query.encode(), hashlib.sha256).hexdigest()
    params["signature"] = signature
    return params


# ── Database Setup ───────────────────────────────────────────────────────────

def init_db(db_path: str) -> sqlite3.Connection:
    """Create tables if they don't exist (same schema as data_logger.rs)."""
    conn = connect_for_write(db_path, operation="Historical backfill")
    conn.execute("PRAGMA journal_mode=WAL")

    conn.executescript("""
        CREATE TABLE IF NOT EXISTS ml_candles_1m (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            open REAL NOT NULL,
            high REAL NOT NULL,
            low REAL NOT NULL,
            close REAL NOT NULL,
            volume REAL NOT NULL,
            market_id TEXT NOT NULL DEFAULT '',
            btc_price REAL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_candles_1m_ts ON ml_candles_1m(timestamp);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_ml_candles_1m_ts_market_unique ON ml_candles_1m(timestamp, market_id);

        CREATE TABLE IF NOT EXISTS ml_features_timeseries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            run_id TEXT NOT NULL,
            market_id TEXT NOT NULL,
            features_json TEXT NOT NULL,
            fusion_score REAL NOT NULL,
            p_up REAL NOT NULL,
            signal TEXT NOT NULL,
            regime TEXT NOT NULL,
            phase TEXT NOT NULL,
            outcome REAL,
            staleness_json TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_ml_features_ts ON ml_features_timeseries(timestamp);
        CREATE INDEX IF NOT EXISTS idx_ml_features_market ON ml_features_timeseries(market_id);

        CREATE TABLE IF NOT EXISTS ml_model_inputs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            run_id TEXT NOT NULL,
            market_id TEXT NOT NULL,
            signal TEXT NOT NULL,
            regime_label TEXT NOT NULL,
            phase TEXT NOT NULL,
            fusion_score REAL NOT NULL,
            p_up REAL NOT NULL,
            regime_value REAL NOT NULL,
            features_json TEXT NOT NULL,
            scalars_json TEXT NOT NULL,
            staleness_json TEXT NOT NULL,
            candles_1m_json TEXT NOT NULL,
            candles_5m_json TEXT NOT NULL,
            candles_15m_json TEXT NOT NULL,
            orderbook_seq_json TEXT NOT NULL,
            outcome REAL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_model_inputs_ts ON ml_model_inputs(timestamp);
        CREATE INDEX IF NOT EXISTS idx_ml_model_inputs_market ON ml_model_inputs(market_id);

        CREATE TABLE IF NOT EXISTS ml_orderbook_snapshots (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            levels_json TEXT NOT NULL,
            spread_bps REAL NOT NULL,
            imbalance REAL NOT NULL,
            mid_price REAL NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_ob_snapshots_ts ON ml_orderbook_snapshots(timestamp);

        CREATE TABLE IF NOT EXISTS ml_tick_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            source TEXT NOT NULL,
            price REAL NOT NULL,
            volume REAL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_ticks_ts ON ml_tick_history(timestamp);

        CREATE TABLE IF NOT EXISTS ml_aggressor_flow (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            buy_vol REAL NOT NULL,
            sell_vol REAL NOT NULL,
            ratio REAL NOT NULL,
            net_flow REAL NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_aggressor_ts ON ml_aggressor_flow(timestamp);

        CREATE TABLE IF NOT EXISTS ml_liquidations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            side TEXT NOT NULL,
            size_usd REAL NOT NULL,
            price REAL NOT NULL,
            symbol TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_liquidations_ts ON ml_liquidations(timestamp);
        CREATE INDEX IF NOT EXISTS idx_ml_liquidations_side ON ml_liquidations(side, timestamp);

        CREATE TABLE IF NOT EXISTS ml_funding_rates (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            rate REAL NOT NULL,
            symbol TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_funding_ts ON ml_funding_rates(timestamp);

        CREATE TABLE IF NOT EXISTS ml_open_interest_hist (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            value_usd REAL NOT NULL,
            symbol TEXT NOT NULL,
            period TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_oi_ts ON ml_open_interest_hist(timestamp);
        CREATE INDEX IF NOT EXISTS idx_ml_oi_period_ts ON ml_open_interest_hist(period, timestamp);

        CREATE TABLE IF NOT EXISTS ml_long_short_ratios (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            series TEXT NOT NULL,
            ratio REAL NOT NULL,
            symbol TEXT NOT NULL,
            period TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_ml_ls_series_ts ON ml_long_short_ratios(series, timestamp);
    """)
    conn.commit()
    return conn


def replace_candle_range(conn: sqlite3.Connection, candles: List[dict]) -> None:
    """Store one shared BTC 1m candle stream for the covered range."""
    if not candles:
        return

    start_iso = datetime.fromtimestamp(
        candles[0]["open_time_ms"] / 1000, tz=timezone.utc
    ).isoformat()
    end_iso = datetime.fromtimestamp(
        candles[-1]["open_time_ms"] / 1000, tz=timezone.utc
    ).isoformat()
    conn.execute(
        "DELETE FROM ml_candles_1m WHERE timestamp >= ? AND timestamp <= ?",
        (start_iso, end_iso),
    )

    rows = []
    for c in candles:
        ts_iso = datetime.fromtimestamp(c["open_time_ms"] / 1000, tz=timezone.utc).isoformat()
        rows.append(
            (
                ts_iso,
                c["open"],
                c["high"],
                c["low"],
                c["close"],
                c["volume"],
                "",
                c["close"],
            )
        )

    conn.executemany(
        """INSERT INTO ml_candles_1m
           (timestamp, open, high, low, close, volume, market_id, btc_price)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)""",
        rows,
    )
    conn.commit()


# ── Polymarket API ───────────────────────────────────────────────────────────

def make_slug(interval: str, window_start_ts: int) -> str:
    """Build the Polymarket slug for a BTC updown market."""
    return f"btc-updown-{interval}-{window_start_ts}"


def fetch_market_by_slug(slug: str) -> Optional[dict]:
    """Query Gamma API for a market by slug. Returns market dict or None."""
    try:
        resp = _https_get(
            f"{GAMMA_API}/events",
            params={"slug": slug},
            timeout=10,
        )
        if resp.status_code == 200:
            data = resp.json()
            if isinstance(data, list) and len(data) > 0:
                event = data[0]
                # Event has nested markets
                markets = event.get("markets", [])
                if markets:
                    return markets[0]
                return event
        return None
    except Exception as e:
        print(f"  [WARN] Gamma API error for {slug}: {e}")
        return None


def determine_outcome(market: dict) -> Optional[float]:
    """
    Determine the market outcome from outcomePrices.
    Returns 1.0 (UP/Yes won), 0.0 (DOWN/No won), or None if unresolved.
    """
    if not isinstance(market, dict) or market.get("closed") is not True:
        return None

    outcome_prices = market.get("outcomePrices")
    if not outcome_prices:
        return None

    # outcomePrices is a string like "[\"1\",\"0\"]" or a list
    if isinstance(outcome_prices, str):
        try:
            outcome_prices = json.loads(outcome_prices)
        except json.JSONDecodeError:
            return None

    if not isinstance(outcome_prices, list) or len(outcome_prices) != 2:
        return None
    outcomes = market.get("outcomes")
    if isinstance(outcomes, str):
        try:
            outcomes = json.loads(outcomes)
        except json.JSONDecodeError:
            return None
    if not isinstance(outcomes, list) or len(outcomes) != 2:
        return None
    names = [str(name).strip().lower() for name in outcomes]
    if set(names) not in ({"up", "down"}, {"yes", "no"}):
        return None
    yes_index = next(i for i, name in enumerate(names) if name in {"up", "yes"})
    try:
        yes_price = float(outcome_prices[yes_index])
        no_price = float(outcome_prices[1 - yes_index])
    except (TypeError, ValueError):
        return None

    # Resolved: winning outcome → price ~1.0
    if yes_price == 1.0 and no_price == 0.0:
        return 1.0  # YES won → BTC went UP
    elif no_price == 1.0 and yes_price == 0.0:
        return 0.0  # NO won → BTC went DOWN
    else:
        return None  # Not cleanly resolved


# ── Binance API ──────────────────────────────────────────────────────────────

def fetch_binance_klines(
    start_ms: int, end_ms: int, symbol: str = "BTCUSDT", interval: str = "1m"
) -> List[dict]:
    """Fetch 1m spot klines from Binance. Returns list of candle dicts."""
    all_candles = []
    current_start = start_ms

    while current_start < end_ms:
        try:
            resp = _https_get(
                f"{BINANCE_API}/api/v3/klines",
                params={
                    "symbol": symbol,
                    "interval": interval,
                    "startTime": current_start,
                    "endTime": end_ms,
                    "limit": 1000,
                },
                timeout=10,
            )
            if resp.status_code != 200:
                print(f"  [WARN] Binance API error: {resp.status_code}")
                break

            klines = resp.json()
            if not klines:
                break

            next_start = int(klines[-1][6]) + 1
            if next_start <= current_start:
                raise RuntimeError("Binance candle pagination did not advance")

            for k in klines:
                all_candles.append({
                    "open_time_ms": k[0],
                    "open": float(k[1]),
                    "high": float(k[2]),
                    "low": float(k[3]),
                    "close": float(k[4]),
                    "volume": float(k[5]),
                    "close_time_ms": k[6],
                })

            # Move past last candle
            current_start = next_start
            if len(klines) < 1000:
                break

            time.sleep(BINANCE_DELAY)

        except Exception as e:
            print(f"  [WARN] Binance API error: {e}")
            break

    return all_candles


def fetch_candles_from_db(
    conn: sqlite3.Connection,
    market_id: str,
    start_ms: int,
    end_ms: int,
) -> List[dict]:
    """Fetch 1m candles already stored in DB for a given market window."""
    start_iso = datetime.fromtimestamp(start_ms / 1000, tz=timezone.utc).isoformat()
    end_iso = datetime.fromtimestamp(end_ms / 1000, tz=timezone.utc).isoformat()

    rows = conn.execute(
        """SELECT timestamp, open, high, low, close, volume
           FROM ml_candles_1m
           WHERE market_id = ? AND timestamp >= ? AND timestamp <= ?
           ORDER BY timestamp""",
        (market_id, start_iso, end_iso),
    ).fetchall()

    candles = []
    for ts_iso, open_, high, low, close, volume in rows:
        try:
            ts_ms = int(datetime.fromisoformat(ts_iso).timestamp() * 1000)
        except Exception:
            ts_ms = 0
        candles.append({
            "open_time_ms": ts_ms,
            "open": float(open_),
            "high": float(high),
            "low": float(low),
            "close": float(close),
            "volume": float(volume),
            "close_time_ms": ts_ms,
        })
    return candles


# ── Bulk Candle Cache ────────────────────────────────────────────────────────

def prefetch_all_candles(
    start_ms: int, end_ms: int, symbol: str = "BTCUSDT"
) -> List[dict]:
    """
    Download ALL 1m candles for the entire date range in one batch.
    Returns sorted list of candle dicts keyed by open_time_ms.
    This avoids fetching the same candles thousands of times per-market.
    """
    print(f"\n── Bulk downloading 1m candles ({symbol}) ──")
    total_minutes = (end_ms - start_ms) // 60_000
    print(f"  Range: {total_minutes:,} minutes (~{total_minutes // 1440} days)")
    print(f"  Estimated API calls: ~{(total_minutes // 1000) + 1}")

    all_candles = []
    current_start = start_ms
    call_count = 0
    consecutive_failures = 0

    while current_start < end_ms:
        try:
            resp = _https_get(
                f"{BINANCE_API}/api/v3/klines",
                params={
                    "symbol": symbol,
                    "interval": "1m",
                    "startTime": current_start,
                    "endTime": end_ms,
                    "limit": 1000,
                },
                timeout=10,
            )
            call_count += 1

            if resp.status_code == 429:
                consecutive_failures += 1
                if consecutive_failures >= 3:
                    raise RuntimeError("Binance repeatedly rate-limited the download")
                print(f"  [RATE LIMIT] Waiting 60s... (call #{call_count})")
                time.sleep(60)
                continue

            if resp.status_code != 200:
                print(f"  [WARN] Binance API error: {resp.status_code}")
                break

            klines = resp.json()
            if not klines:
                break
            next_start = int(klines[-1][6]) + 1
            if next_start <= current_start:
                raise RuntimeError("Binance candle pagination did not advance")

            for k in klines:
                all_candles.append({
                    "open_time_ms": k[0],
                    "open": float(k[1]),
                    "high": float(k[2]),
                    "low": float(k[3]),
                    "close": float(k[4]),
                    "volume": float(k[5]),
                    "close_time_ms": k[6],
                })

            current_start = next_start
            consecutive_failures = 0
            if len(klines) < 1000:
                break

            # Progress update every 50 calls
            if call_count % 50 == 0:
                pct = (current_start - start_ms) / (end_ms - start_ms) * 100
                print(f"  Progress: {pct:.1f}% ({len(all_candles):,} candles, {call_count} API calls)")

            time.sleep(BINANCE_DELAY)

        except Exception as e:
            print(f"  [WARN] Binance API error: {e}")
            consecutive_failures += 1
            if consecutive_failures >= 3:
                raise RuntimeError("Binance candle download failed repeatedly") from e
            time.sleep(2)
            continue

    # Sort by timestamp
    all_candles.sort(key=lambda c: c["open_time_ms"])

    print(f"  Bulk download complete: {len(all_candles):,} candles in {call_count} API calls")
    return all_candles


def slice_candles_from_cache(
    candle_cache: List[dict], start_ms: int, end_ms: int
) -> List[dict]:
    """
    Extract candles for a specific time window from the pre-fetched cache.
    Uses binary search for efficiency.
    """
    if not candle_cache:
        return []

    # Binary search for start position
    lo, hi = 0, len(candle_cache)
    while lo < hi:
        mid = (lo + hi) // 2
        if candle_cache[mid]["open_time_ms"] < start_ms:
            lo = mid + 1
        else:
            hi = mid

    # Collect candles in range
    result = []
    for i in range(lo, len(candle_cache)):
        c = candle_cache[i]
        if c["open_time_ms"] > end_ms:
            break
        result.append(c)

    return result


def fetch_binance_futures_klines(
    start_ms: int, end_ms: int, symbol: str = "BTCUSDT", interval: str = "1m"
) -> List[list]:
    """Fetch 1m futures klines from Binance. Returns raw kline rows."""
    all_klines = []
    current_start = start_ms

    while current_start < end_ms:
        try:
            resp = _https_get(
                f"{BINANCE_FUTURES_API}/fapi/v1/klines",
                params={
                    "symbol": symbol,
                    "interval": interval,
                    "startTime": current_start,
                    "endTime": end_ms,
                    "limit": 1000,
                },
                headers=_binance_headers(),
                allow_redirects=False,
                timeout=10,
            )
            if resp.status_code != 200:
                print(f"  [WARN] Binance Futures kline error: {resp.status_code}")
                break

            klines = resp.json()
            if not klines:
                break

            next_start = _next_page_start(klines[-1][6], current_start)
            all_klines.extend(klines)
            current_start = next_start
            if len(klines) < 1000:
                break

            time.sleep(BINANCE_DELAY)

        except Exception as e:
            print(f"  [WARN] Binance Futures kline error: {e}")
            break

    return all_klines


def fetch_binance_funding_rates(
    start_ms: int, end_ms: int, symbol: str = "BTCUSDT"
) -> List[dict]:
    """Fetch funding rate history from Binance Futures."""
    all_rates = []
    current_start = start_ms

    while current_start < end_ms:
        try:
            resp = _https_get(
                f"{BINANCE_FUTURES_API}/fapi/v1/fundingRate",
                params={
                    "symbol": symbol,
                    "startTime": current_start,
                    "endTime": end_ms,
                    "limit": 1000,
                },
                headers=_binance_headers(),
                allow_redirects=False,
                timeout=10,
            )
            if resp.status_code != 200:
                print(f"  [WARN] Binance funding error: {resp.status_code}")
                break

            data = resp.json()
            if not data:
                break

            next_start = _next_page_start(data[-1].get("fundingTime"), current_start)
            all_rates.extend(data)
            current_start = next_start

            if len(data) < 1000:
                break

            time.sleep(BINANCE_DELAY)

        except Exception as e:
            print(f"  [WARN] Binance funding error: {e}")
            break

    return all_rates


def fetch_binance_open_interest_hist(
    start_ms: int,
    end_ms: int,
    symbol: str = "BTCUSDT",
    period: str = "5m",
) -> List[dict]:
    """Fetch open interest history from Binance Futures."""
    all_rows = []
    endpoint = f"{BINANCE_FUTURES_API}/futures/data/openInterestHist"
    limit = 500
    period_ms = _period_to_ms(period)
    # Binance's MARKET_DATA history endpoints can return 400 on wide 5m windows,
    # especially for older ranges without an API key. Cap chunks at 24h so month
    # backfills degrade gracefully instead of dropping the earliest context.
    chunk_ms = min(max(period_ms * max(limit - 50, 1), period_ms), 24 * 60 * 60 * 1000)

    for chunk_start, chunk_end in _iter_chunk_ranges(start_ms, end_ms, chunk_ms):
        current_start = chunk_start
        while current_start < chunk_end:
            try:
                resp = _https_get(
                    endpoint,
                    params={
                        "symbol": symbol,
                        "period": period,
                        "startTime": current_start,
                        "endTime": chunk_end,
                        "limit": limit,
                    },
                    headers=_binance_headers(),
                    allow_redirects=False,
                    timeout=10,
                )
                if resp.status_code != 200:
                    print(
                        f"  [WARN] Binance OI error: {resp.status_code} "
                        f"(chunk={datetime.fromtimestamp(chunk_start / 1000, tz=timezone.utc).isoformat()} "
                        f"→ {datetime.fromtimestamp(chunk_end / 1000, tz=timezone.utc).isoformat()})"
                    )
                    break

                data = resp.json()
                if not data:
                    break

                next_start = _next_page_start(data[-1].get("timestamp", data[-1].get("time")), current_start)
                all_rows.extend(data)
                current_start = next_start

                if len(data) < limit:
                    break

                time.sleep(BINANCE_DELAY)

            except Exception as e:
                print(f"  [WARN] Binance OI error: {e}")
                break

    return _dedupe_rows_by_timestamp(all_rows)


def fetch_binance_futures_data_series(
    endpoint: str,
    start_ms: int,
    end_ms: int,
    symbol: str = "BTCUSDT",
    period: str = "5m",
    limit: int = 500,
) -> List[dict]:
    """Fetch paginated futures/data series from Binance."""
    all_rows = []
    url = f"{BINANCE_FUTURES_API}/futures/data/{endpoint}"
    period_ms = _period_to_ms(period)
    chunk_ms = min(max(period_ms * max(limit - 50, 1), period_ms), 24 * 60 * 60 * 1000)

    for chunk_start, chunk_end in _iter_chunk_ranges(start_ms, end_ms, chunk_ms):
        current_start = chunk_start
        while current_start < chunk_end:
            try:
                resp = _https_get(
                    url,
                    params={
                        "symbol": symbol,
                        "period": period,
                        "startTime": current_start,
                        "endTime": chunk_end,
                        "limit": limit,
                    },
                    headers=_binance_headers(),
                    allow_redirects=False,
                    timeout=10,
                )
                if resp.status_code != 200:
                    print(
                        f"  [WARN] Binance futures/data {endpoint} error: {resp.status_code} "
                        f"(chunk={datetime.fromtimestamp(chunk_start / 1000, tz=timezone.utc).isoformat()} "
                        f"→ {datetime.fromtimestamp(chunk_end / 1000, tz=timezone.utc).isoformat()})"
                    )
                    break

                data = resp.json()
                if not data:
                    break

                next_start = _next_page_start(data[-1].get("timestamp", data[-1].get("time")), current_start)
                all_rows.extend(data)
                current_start = next_start

                if len(data) < limit:
                    break

                time.sleep(BINANCE_DELAY)
            except Exception as e:
                print(f"  [WARN] Binance futures/data {endpoint} error: {e}")
                break

    return _dedupe_rows_by_timestamp(all_rows)


def fetch_binance_force_orders(
    start_ms: int, end_ms: int, symbol: str = "BTCUSDT"
) -> List[dict]:
    """Fetch liquidation (force order) history from Binance Futures."""
    all_rows = []
    current_start = start_ms
    endpoints = [
        f"{BINANCE_FUTURES_API}/fapi/v1/forceOrders",
        f"{BINANCE_FUTURES_API}/futures/data/forceOrders",
    ]

    for endpoint in endpoints:
        all_rows = []
        current_start = start_ms
        while current_start < end_ms:
            try:
                resp = _https_get(
                endpoint,
                params=_sign_params({
                    "symbol": symbol,
                    "startTime": current_start,
                    "endTime": end_ms,
                    "limit": 1000,
                    "timestamp": int(time.time() * 1000),
                    "recvWindow": 5000,
                }),
                headers=_binance_headers(),
                allow_redirects=False,
                timeout=10,
            )
                if resp.status_code != 200:
                    print(f"  [WARN] Binance liquidation error ({endpoint}): {resp.status_code}")
                    all_rows = []
                    break

                data = resp.json()
                if not data:
                    break

                next_start = _next_page_start(data[-1].get("time", data[-1].get("T")), current_start)
                all_rows.extend(data)
                current_start = next_start

                if len(data) < 1000:
                    break

                time.sleep(BINANCE_DELAY)

            except Exception as e:
                print(f"  [WARN] Binance liquidation error ({endpoint}): {e}")
                all_rows = []
                break

        if all_rows:
            return all_rows

    return all_rows


# ── Feature Computation ──────────────────────────────────────────────────────

def _latest_before(ts_list: List[int], val_list: List[float], target_ms: int) -> float:
    if not ts_list:
        return 0.0
    idx = bisect_right(ts_list, target_ms) - 1
    if idx < 0:
        return 0.0
    return val_list[idx]


def _ema(values: List[float], span: int) -> List[float]:
    """Exponential moving average."""
    if not values:
        return []
    alpha = 2.0 / (span + 1)
    result = [values[0]]
    for v in values[1:]:
        result.append(alpha * v + (1 - alpha) * result[-1])
    return result


def _clamp(value: float, lo: float, hi: float) -> float:
    return max(lo, min(hi, value))


def _safe_log_ratio(value: float) -> float:
    if value <= 0.0:
        return 0.0
    return _clamp(math.log(max(value, 1e-6)), -2.0, 2.0)


def _period_to_ms(period: str) -> int:
    units = {
        "m": 60_000,
        "h": 3_600_000,
        "d": 86_400_000,
    }
    if not period or len(period) < 2:
        raise ValueError(f"invalid period: {period!r}")
    unit = period[-1].lower()
    if unit not in units:
        raise ValueError(f"unsupported period unit in {period!r}")
    count = int(period[:-1])
    if count <= 0:
        raise ValueError("period must be positive")
    return count * units[unit]


def _iter_chunk_ranges(start_ms: int, end_ms: int, chunk_ms: int) -> List[Tuple[int, int]]:
    if chunk_ms <= 0:
        raise ValueError("chunk size must be positive")
    ranges = []
    current = start_ms
    while current < end_ms:
        chunk_end = min(end_ms, current + chunk_ms)
        ranges.append((current, chunk_end))
        current = chunk_end + 1
    return ranges


def _dedupe_rows_by_timestamp(rows: List[dict]) -> List[dict]:
    deduped: Dict[int, dict] = {}
    for row in rows:
        try:
            ts = int(row.get("timestamp", row.get("time", 0)))
        except Exception:
            continue
        if ts:
            deduped[ts] = row
    return [deduped[ts] for ts in sorted(deduped)]


def _has_observation_before(ts_list: List[int], target_ms: int) -> bool:
    return bool(ts_list) and bisect_right(ts_list, target_ms) > 0


def _historical_runtime_features(candles: List[dict], ts_sec: int) -> dict:
    closes = [c["close"] for c in candles]
    btc_price = closes[-1] if closes else 0.0
    ts_dt = datetime.fromtimestamp(ts_sec, tz=timezone.utc)

    def _vol(window: int) -> float:
        if len(closes) < window + 1:
            return 0.0
        rets = [
            (closes[i] - closes[i - 1]) / max(closes[i - 1], 1e-10)
            for i in range(len(closes) - window, len(closes))
        ]
        if not rets:
            return 0.0
        mean = sum(rets) / len(rets)
        return float(math.sqrt(sum((r - mean) ** 2 for r in rets) / len(rets)))

    rem_1000 = btc_price % 1000.0
    rem_500 = btc_price % 500.0
    dist_1000 = (500.0 - abs(rem_1000 - 500.0)) / 500.0 if btc_price else 0.0
    dist_500 = (250.0 - abs(rem_500 - 250.0)) / 250.0 if btc_price else 0.0

    if len(candles) >= 2:
        first_open = float(candles[0]["open"])
        window_momentum = (btc_price - first_open) / max(first_open, 1e-10)
        window_momentum = _clamp(window_momentum / 0.005, -1.0, 1.0)
    else:
        window_momentum = 0.0

    return {
        "hour_of_day": ts_dt.hour / 23.0 if 23 else 0.0,
        "day_of_week": ts_dt.weekday() / 6.0 if 6 else 0.0,
        "distance_from_round_1000": dist_1000,
        "distance_from_round_500": dist_500,
        "volatility_10": _vol(10),
        "volatility_30": _vol(30),
        "window_momentum": window_momentum,
    }


def _historical_oi_momentum(context: Optional[dict], target_ms: int, lookback_ms: int) -> Optional[float]:
    if context is None:
        return None
    oi_ts = context.get("oi_ts", [])
    oi_vals = context.get("oi_vals", [])
    if not (_has_observation_before(oi_ts, target_ms) and _has_observation_before(oi_ts, target_ms - lookback_ms)):
        return None
    current = _latest_before(oi_ts, oi_vals, target_ms)
    prior = _latest_before(oi_ts, oi_vals, target_ms - lookback_ms)
    if current <= 0.0 or prior <= 0.0:
        return None
    return _clamp((current - prior) / max(abs(prior), 1e-10), -2.0, 2.0)


def _historical_feature_availability(
    context: Optional[dict],
    ts_sec: int,
) -> Dict[str, bool]:
    availability = {name: True for name in FEATURE_NAMES}
    for name in HISTORICAL_ALWAYS_UNAVAILABLE:
        availability[name] = False
    for name in HISTORICAL_TRAIN_DERIVED:
        availability[name] = True

    target_ms = ts_sec * 1000
    if context is None:
        for name in ("funding_rate", "oi_momentum", "deribit_pcr", "options_advanced", "cross_market"):
            availability[name] = False
        availability["long_liq_volume"] = False
        availability["short_liq_volume"] = False
        return availability

    funding_ts = context.get("funding_ts", [])
    global_ls_ts = context.get("global_ls_ts", [])
    top_account_ts = context.get("top_account_ts", [])
    top_position_ts = context.get("top_position_ts", [])

    availability["funding_rate"] = _has_observation_before(funding_ts, target_ms)
    availability["oi_momentum"] = _historical_oi_momentum(
        context,
        target_ms,
        context.get("ratio_period_ms", 300_000) * 12,
    ) is not None
    availability["deribit_pcr"] = _has_observation_before(global_ls_ts, target_ms)
    availability["options_advanced"] = _has_observation_before(global_ls_ts, target_ms) and _has_observation_before(
        top_account_ts, target_ms
    )
    availability["cross_market"] = _has_observation_before(global_ls_ts, target_ms) and _has_observation_before(
        top_position_ts, target_ms
    )

    liq_enabled = bool(context.get("liquidations_requested"))
    availability["long_liq_volume"] = liq_enabled
    availability["short_liq_volume"] = liq_enabled
    return availability


def _historical_staleness_vector(context: Optional[dict], ts_sec: int) -> List[float]:
    availability = _historical_feature_availability(context, ts_sec)
    staleness = [0.0] * len(FEATURE_NAMES)
    for name, idx in FEATURE_INDEX.items():
        staleness[idx] = 0.0 if availability.get(name, False) else 1.0
    return staleness


def compute_features_from_candles(
    candles: List[dict],
    context: Optional[dict] = None,
    ts_sec: Optional[int] = None,
    interval: str = "15m",
) -> dict:
    """
    Compute legacy historical features from raw candles + optional Binance Futures
    context. These are projected into the current 34-feature contract later.
    """
    if len(candles) < 20:
        return {name: 0.0 for name in LEGACY_FEATURE_KEYS}

    closes = [c["close"] for c in candles]
    volumes = [c["volume"] for c in candles]
    highs = [c["high"] for c in candles]
    lows = [c["low"] for c in candles]
    opens = [c["open"] for c in candles]

    last_price = closes[-1]
    n = len(closes)
    returns = [(closes[i] - closes[i-1]) / closes[i-1] for i in range(1, n)]

    # ── RSI (14-period, Wilder smoothing) ──
    gains = [max(0, r) for r in returns[-14:]]
    losses = [max(0, -r) for r in returns[-14:]]
    avg_gain = sum(gains) / 14 if gains else 0
    avg_loss = sum(losses) / 14 if losses else 0.001
    if avg_loss > 0:
        rsi = 100 - 100 / (1 + avg_gain / avg_loss)
    else:
        rsi = 100.0 if avg_gain > 0 else 50.0

    # ── Momentum (multi-scale) ──
    mom_20 = (closes[-1] - closes[-20]) / closes[-20] if n >= 20 else 0.0
    mom_60 = (closes[-1] - closes[-60]) / closes[-60] if n >= 60 else mom_20

    # ── Volatility (realized vol, annualized-ish for scale) ──
    recent_returns = returns[-20:] if len(returns) >= 20 else returns
    if recent_returns:
        mean_ret = sum(recent_returns) / len(recent_returns)
        vol = (sum((r - mean_ret)**2 for r in recent_returns) / len(recent_returns)) ** 0.5
    else:
        vol = 0.0

    # ── VWAP deviation ──
    total_vwap_num = sum(c["close"] * c["volume"] for c in candles[-60:])
    total_vwap_den = sum(c["volume"] for c in candles[-60:])
    vwap = total_vwap_num / max(total_vwap_den, 1e-10)
    vwap_dev = (last_price - vwap) / vwap

    # ── Mean reversion (SMA20 deviation) ──
    sma20 = sum(closes[-20:]) / 20 if n >= 20 else closes[-1]
    mean_rev = (sma20 - last_price) / last_price

    # ── Aggressor ratio from candle body direction ──
    up_candles = sum(1 for c in candles[-10:] if c["close"] > c["open"])
    aggressor = up_candles / 10.0

    # ── Bollinger Band %B (replaces zero "spread" slot with real signal) ──
    if n >= 20:
        sma = sum(closes[-20:]) / 20
        bb_std = (sum((c - sma)**2 for c in closes[-20:]) / 20) ** 0.5
        upper = sma + 2 * bb_std
        lower = sma - 2 * bb_std
        bb_pctb = (last_price - lower) / max(upper - lower, 1e-10)
        bb_pctb = max(-1.0, min(2.0, bb_pctb))  # Clip outliers
    else:
        bb_pctb = 0.5

    # ── Volume imbalance (buy vs sell volume proxy) ──
    recent_candles = candles[-20:]
    buy_vol = sum(c["volume"] * (c["close"] - c["low"]) / max(c["high"] - c["low"], 1e-10)
                  for c in recent_candles)
    sell_vol = sum(c["volume"] * (c["high"] - c["close"]) / max(c["high"] - c["low"], 1e-10)
                   for c in recent_candles)
    total_vol = buy_vol + sell_vol
    vol_imbalance = (buy_vol - sell_vol) / max(total_vol, 1e-10)

    # ── MACD histogram (12/26/9 EMA on closes) ──
    if n >= 26:
        ema12 = _ema(closes, 12)
        ema26 = _ema(closes, 26)
        macd_line = [a - b for a, b in zip(ema12, ema26)]
        signal_line = _ema(macd_line, 9)
        macd_hist = macd_line[-1] - signal_line[-1]
        # Normalize by price
        macd_norm = macd_hist / last_price * 1000  # Scale to ~[-2, 2]
    else:
        macd_norm = 0.0

    # ── OBV slope (On-Balance Volume trend) ──
    if n >= 20:
        obv = [0.0]
        for i in range(1, n):
            if closes[i] > closes[i-1]:
                obv.append(obv[-1] + volumes[i])
            elif closes[i] < closes[i-1]:
                obv.append(obv[-1] - volumes[i])
            else:
                obv.append(obv[-1])
        # OBV slope over last 20 bars, normalized
        obv_recent = obv[-20:]
        obv_slope = (obv_recent[-1] - obv_recent[0]) / max(abs(obv_recent[0]), 1e-10)
        obv_slope = max(-2.0, min(2.0, obv_slope))
    else:
        obv_slope = 0.0

    # ── ATR ratio (current ATR vs longer-term ATR for vol regime) ──
    if n >= 60:
        tr_vals = [max(highs[i] - lows[i],
                       abs(highs[i] - closes[i-1]),
                       abs(lows[i] - closes[i-1]))
                   for i in range(1, n)]
        atr_14 = sum(tr_vals[-14:]) / 14
        atr_60 = sum(tr_vals[-60:]) / 60
        atr_ratio = atr_14 / max(atr_60, 1e-10) - 1.0  # 0 = normal, >0 = expanding vol
        atr_ratio = max(-2.0, min(2.0, atr_ratio))
    else:
        atr_ratio = 0.0

    # ── Volume momentum (recent vs avg volume) ──
    if n >= 60:
        vol_recent = sum(volumes[-10:]) / 10
        vol_avg = sum(volumes[-60:]) / 60
        vol_momentum = vol_recent / max(vol_avg, 1e-10) - 1.0  # 0 = normal
        vol_momentum = max(-2.0, min(2.0, vol_momentum))
    else:
        vol_momentum = 0.0

    # ── Context lookups (Binance Futures data when available) ──
    funding_rate = 0.0
    if context is not None and ts_sec is not None:
        target_ms = ts_sec * 1000
        # A historical kline's full taker flow is known only when it closes.
        minute_ms = (ts_sec // 60 - 1) * 60_000

        # Aggressor from futures taker flow (overrides candle proxy)
        flow = context.get("flow_by_minute", {}).get(minute_ms)
        if flow:
            buy_v, sell_v, ratio, net_flow = flow
            total = buy_v + sell_v
            aggressor = ratio
            vol_imbalance = (net_flow / max(total, 1e-10)) if total > 0 else vol_imbalance

        funding_rate = _latest_before(
            context.get("funding_ts", []),
            context.get("funding_rates", []),
            target_ms,
        )

        # Long/short ratios → macd_norm slot enhancement
        global_ls = _latest_before(
            context.get("global_ls_ts", []),
            context.get("global_ls_ratio", []),
            target_ms,
        )
        if global_ls:
            # log(L/S ratio): >0 = longs dominate, <0 = shorts dominate
            macd_norm = math.log(max(global_ls, 1e-6))

    features = {
        "spread": bb_pctb,                    # Bollinger %B position [0..1]
        "imbalance": vol_imbalance,            # Volume buy/sell imbalance [-1..1]
        "aggressor_ratio": aggressor,          # Bullish candle ratio [0..1]
        "volatility": vol * 10000,             # Realized vol in bps (better scale)
        "momentum": mom_20 * 10000,            # 20-bar momentum in bps
        "mean_reversion": mean_rev * 10000,    # Mean reversion in bps
        "vwap_deviation": vwap_dev * 10000,    # VWAP dev in bps
        "rsi": (rsi - 50) / 50,               # RSI normalized [-1..1]
        "funding_rate": funding_rate * 10000,  # Funding in bps (better scale)
        "open_interest": atr_ratio,            # ATR expansion ratio [-2..2]
        "news_sentiment": macd_norm,           # MACD histogram or L/S ratio
        "social_sentiment": obv_slope,         # OBV trend [-2..2]
        "whale_activity": vol_momentum,        # Volume surge ratio [-2..2]
        "liquidation_pressure": mom_60 * 10000,  # 60-bar momentum in bps
    }
    return features


def project_features_to_contract(
    legacy: dict,
    candles: List[dict],
    context: Optional[dict] = None,
    ts_sec: Optional[int] = None,
) -> dict:
    """
    Map historical candle/context signals into the current 34-feature runtime
    contract. Features we cannot reconstruct historically remain neutral.
    """
    closes = [c["close"] for c in candles]
    n = len(closes)
    returns = [(closes[i] - closes[i - 1]) / max(closes[i - 1], 1e-10) for i in range(1, n)]

    last_ret_bps = returns[-1] * 10000 if returns else 0.0
    mom_5_bps = (
        (closes[-1] - closes[-5]) / max(closes[-5], 1e-10) * 10000 if n >= 5 else last_ret_bps
    )
    mom_15_bps = (
        (closes[-1] - closes[-15]) / max(closes[-15], 1e-10) * 10000 if n >= 15 else mom_5_bps
    )
    mom_60_bps = legacy.get("liquidation_pressure", 0.0)

    global_ls_log = 0.0
    top_account_log = 0.0
    top_position_log = 0.0
    funding_bps = legacy.get("funding_rate", 0.0)
    oi_momentum = legacy.get("open_interest", 0.0)

    if context is not None and ts_sec is not None:
        target_ms = ts_sec * 1000
        lookback_ms = context.get("ratio_period_ms", 300_000) * 12
        global_ls_log = _safe_log_ratio(
            _latest_before(
                context.get("global_ls_ts", []),
                context.get("global_ls_ratio", []),
                target_ms,
            )
        )
        top_account_log = _safe_log_ratio(
            _latest_before(
                context.get("top_account_ts", []),
                context.get("top_account_ratio", []),
                target_ms,
            )
        )
        top_position_log = _safe_log_ratio(
            _latest_before(
                context.get("top_position_ts", []),
                context.get("top_position_ratio", []),
                target_ms,
            )
        )
        oi_value = _historical_oi_momentum(context, target_ms, lookback_ms)
        if oi_value is not None:
            oi_momentum = oi_value

    rsi_macd_ha = _clamp(
        (legacy.get("rsi", 0.0) + _clamp(legacy.get("news_sentiment", 0.0), -2.0, 2.0)) / 2.0,
        -2.0,
        2.0,
    )
    vpin_micro = _clamp(
        legacy.get("imbalance", 0.0) * legacy.get("whale_activity", 0.0),
        -2.0,
        2.0,
    )
    whale_flow = legacy.get("whale_activity", 0.0)
    cross_market = _clamp((global_ls_log + top_position_log) / 2.0, -2.0, 2.0)
    options_advanced = _clamp(top_account_log - global_ls_log, -2.0, 2.0)
    kyle_lambda = _clamp(
        abs(legacy.get("volatility", 0.0)) * abs(legacy.get("imbalance", 0.0)) / 100.0,
        0.0,
        2.0,
    )
    hawkes_flow = _clamp(
        legacy.get("aggressor_ratio", 0.0) * legacy.get("whale_activity", 0.0),
        -2.0,
        2.0,
    )

    projected = {name: 0.0 for name in FEATURE_NAMES}
    projected.update(
        {
            "orderbook_imbalance": legacy.get("imbalance", 0.0),
            "tick_velocity": last_ret_bps,
            "vwap_confluence": -abs(legacy.get("vwap_deviation", 0.0)),
            "price_divergence": legacy.get("mean_reversion", 0.0),
            "spike_detector": abs(legacy.get("volatility", 0.0)),
            "deribit_pcr": -global_ls_log,
            "rsi_macd_ha": rsi_macd_ha,
            "fear_greed": 0.0,
            "funding_rate": funding_bps,
            "oi_momentum": oi_momentum,
            "aggressor_ratio": legacy.get("aggressor_ratio", 0.0),
            "coingecko_sentiment": 0.0,
            "finnhub_sentiment": 0.0,
            "lunarcrush_sentiment": 0.0,
            "polymarket_flow": 0.0,
            "vpin_microstructure": vpin_micro,
            "options_advanced": options_advanced,
            "whale_flow": whale_flow,
            "cross_market": cross_market,
            "latency_arb": 0.0,
            "news_sentiment": _clamp(legacy.get("news_sentiment", 0.0), -2.0, 2.0),
            "btc_final_momentum": mom_15_bps if abs(mom_15_bps) > abs(mom_5_bps) else mom_5_bps,
            "kyle_lambda": kyle_lambda,
            "hawkes_flow": hawkes_flow if hawkes_flow != 0.0 else _clamp(mom_60_bps / 10000.0, -2.0, 2.0),
        }
    )
    return projected


def estimate_regime(candles: List[dict]) -> str:
    """Estimate market regime from recent candles using ADX-like metric."""
    if len(candles) < 30:
        return "chop"

    closes = [c["close"] for c in candles[-60:]]
    highs = [c["high"] for c in candles[-60:]]
    lows = [c["low"] for c in candles[-60:]]
    n = len(closes)
    returns = [(closes[i] - closes[i-1]) / closes[i-1] for i in range(1, n)]

    if not returns:
        return "chop"

    # Volatility
    mean_ret = sum(returns) / len(returns)
    vol = (sum((r - mean_ret)**2 for r in returns) / len(returns)) ** 0.5

    # Directional movement (ADX-like)
    plus_dm = []
    minus_dm = []
    tr_vals = []
    for i in range(1, n):
        up_move = highs[i] - highs[i-1]
        down_move = lows[i-1] - lows[i]
        plus_dm.append(max(up_move, 0) if up_move > down_move else 0)
        minus_dm.append(max(down_move, 0) if down_move > up_move else 0)
        tr_vals.append(max(highs[i] - lows[i],
                           abs(highs[i] - closes[i-1]),
                           abs(lows[i] - closes[i-1])))

    if len(tr_vals) < 14:
        return "chop"

    # Smoothed 14-period averages
    atr = sum(tr_vals[-14:]) / 14
    avg_plus = sum(plus_dm[-14:]) / 14
    avg_minus = sum(minus_dm[-14:]) / 14

    plus_di = avg_plus / max(atr, 1e-10)
    minus_di = avg_minus / max(atr, 1e-10)
    dx = abs(plus_di - minus_di) / max(plus_di + minus_di, 1e-10)

    if vol > 0.002:  # ~0.2% per minute = high vol
        return "high_vol"
    elif dx > 0.25:  # Strong directional movement
        return "trending"
    else:
        return "chop"


# ── Main Backfill Logic ─────────────────────────────────────────────────────

def backfill_interval(
    conn: sqlite3.Connection,
    interval: str,
    start_ts: int,
    end_ts: int,
    run_id: str,
    context: Optional[dict] = None,
    refresh_existing: bool = False,
    candle_cache: Optional[List[dict]] = None,
):
    """Backfill markets for a given interval (5m or 15m)."""
    step = 900 if interval == "15m" else 300
    total = (end_ts - start_ts) // step
    found = 0
    skipped = 0
    errors = 0
    refreshed = 0

    print(f"\n{'='*60}")
    print(f"Backfilling {interval} markets: {total} windows to check")
    print(f"  From: {datetime.fromtimestamp(start_ts, tz=timezone.utc).isoformat()}")
    print(f"  To:   {datetime.fromtimestamp(end_ts, tz=timezone.utc).isoformat()}")
    print(f"{'='*60}\n")

    current_ts = start_ts
    while current_ts < end_ts:
        slug = make_slug(interval, current_ts)
        window_start = datetime.fromtimestamp(current_ts, tz=timezone.utc)

        # Check if we already have this market
        existing = conn.execute(
            "SELECT COUNT(*) FROM ml_features_timeseries WHERE market_id = ?",
            (slug,),
        ).fetchone()[0]

        if existing > 0 and not refresh_existing:
            current_ts += step
            skipped += 1
            continue

        # Determine outcome
        outcome = None
        if existing > 0 and refresh_existing:
            row = conn.execute(
                "SELECT outcome FROM ml_features_timeseries WHERE market_id = ? LIMIT 1",
                (slug,),
            ).fetchone()
            if row is not None:
                outcome = row[0]
        else:
            # Query Polymarket
            market = fetch_market_by_slug(slug)
            time.sleep(GAMMA_DELAY)

            if market is None:
                current_ts += step
                continue

            outcome = determine_outcome(market)

        if outcome is None:
            current_ts += step
            continue

        # We have a resolved market! Pull candle window (prefer DB when refreshing)
        lookback_start_ms = (current_ts - CANDLE_LOOKBACK_MINUTES * 60) * 1000
        # Build the feature snapshot from information available strictly before
        # the market opens; do not leak any candle from the traded window itself.
        window_end_ms = current_ts * 1000 - 1

        if existing > 0 and refresh_existing:
            candles = fetch_candles_from_db(conn, slug, lookback_start_ms, window_end_ms)
            if len(candles) < 60 and candle_cache:
                candles = slice_candles_from_cache(candle_cache, lookback_start_ms, window_end_ms)
            elif len(candles) < 60:
                candles = fetch_binance_klines(lookback_start_ms, window_end_ms)
        elif candle_cache:
            candles = slice_candles_from_cache(candle_cache, lookback_start_ms, window_end_ms)
        else:
            candles = fetch_binance_klines(lookback_start_ms, window_end_ms)

        if len(candles) < 60:
            print(f"  [{window_start.strftime('%Y-%m-%d %H:%M')}] {slug}: "
                  f"only {len(candles)} candles, skipping")
            current_ts += step
            errors += 1
            continue

        # Compute features from candles
        legacy_features = compute_features_from_candles(
            candles, context=context, ts_sec=current_ts, interval=interval
        )
        features = project_features_to_contract(
            legacy_features, candles, context=context, ts_sec=current_ts
        )
        regime = estimate_regime(candles)

        # Add runtime-like features that we can compute honestly at market open.
        features.update(_historical_runtime_features(candles, current_ts))

        # Add context features that we can compute historically.
        features["time_remaining"] = 1.0  # Full window remaining at start
        features["balance"] = 1.0  # Neutral starting balance fraction
        features["positions"] = 0.0
        features["btc_price"] = candles[-1]["close"] / 100_000.0  # Normalized
        features["contract_price"] = 0.5  # Unknown, neutral default
        features["price_discovery_lag"] = 0.0

        # Compute a simple fusion score from features
        fusion_vals = [v for k, v in features.items()
                       if k not in ("time_remaining", "btc_price", "contract_price",
                                    "balance", "positions", "price_discovery_lag")]
        fusion_score = sum(fusion_vals) / max(len(fusion_vals), 1)

        # Insert or update feature record with known outcome
        ts_iso = window_start.isoformat()
        staleness = _historical_staleness_vector(context, current_ts)

        if existing > 0 and refresh_existing:
            conn.execute(
                """UPDATE ml_features_timeseries
                   SET features_json = ?, fusion_score = ?, regime = ?, staleness_json = ?
                   WHERE market_id = ?""",
                (
                    json.dumps(features), fusion_score, regime,
                    json.dumps(staleness), slug,
                ),
            )
        else:
            conn.execute(
                """INSERT INTO ml_features_timeseries
                   (timestamp, run_id, market_id, features_json, fusion_score,
                    p_up, signal, regime, phase, outcome, staleness_json)
                   VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)""",
                (
                    ts_iso, run_id, slug,
                    json.dumps(features), fusion_score,
                    0.5,  # No pre-existing prediction
                    "NEUTRAL",  # No live signal
                    regime, "backfill",
                    outcome,
                    json.dumps(staleness),
                ),
            )

        conn.commit()
        if existing > 0 and refresh_existing:
            print(f"  [{window_start.strftime('%Y-%m-%d %H:%M')}] {slug}: "
                  f"REFRESHED | {len(candles)} candles | regime={regime}")
            refreshed += 1
        else:
            found += 1
            outcome_str = "UP" if outcome == 1.0 else "DOWN"
            print(f"  [{window_start.strftime('%Y-%m-%d %H:%M')}] {slug}: "
                  f"{outcome_str} | {len(candles)} candles | regime={regime}")

        current_ts += step

    print(f"\n── {interval} Summary ──")
    print(f"  Found & stored:  {found}")
    print(f"  Already existed: {skipped}")
    if refresh_existing:
        print(f"  Refreshed:       {refreshed}")
    print(f"  Errors/skipped:  {errors}")
    print(f"  Not found:       {total - found - skipped - errors - refreshed}")

    return found


def build_context(
    conn: sqlite3.Connection,
    start_ts: int,
    end_ts: int,
    symbol: str = "BTCUSDT",
    ratio_period: str = "5m",
    fetch_liquidations: bool = False,
) -> dict:
    """Prefetch flow/funding/OI/liquidations and store in DB + context maps."""
    print("\n── Prefetching Binance Futures context (flow, funding, OI, ratios) ──")
    start_ms = start_ts * 1000
    end_ms = end_ts * 1000
    if not _get_api_key():
        print("  [WARN] BINANCE_FUTURES_API_KEY not set; some MARKET_DATA endpoints may return empty.")

    # Aggressor flow from 1m futures klines (taker buy/sell)
    flow_by_minute: Dict[int, Tuple[float, float, float, float]] = {}
    futures_klines = fetch_binance_futures_klines(start_ms, end_ms, symbol=symbol, interval="1m")
    for k in futures_klines:
        try:
            open_time = int(k[0])
            quote_vol = float(k[7])
            taker_buy_quote = float(k[10])
            buy_vol = taker_buy_quote
            sell_vol = max(0.0, quote_vol - taker_buy_quote)
            total = buy_vol + sell_vol
            ratio = buy_vol / total if total > 0 else 0.5
            net_flow = buy_vol - sell_vol
            flow_by_minute[open_time] = (buy_vol, sell_vol, ratio, net_flow)
        except Exception:
            continue

    # Replace flow rows in range
    start_iso = datetime.fromtimestamp(start_ts, tz=timezone.utc).isoformat()
    end_iso = datetime.fromtimestamp(end_ts, tz=timezone.utc).isoformat()
    conn.execute(
        "DELETE FROM ml_aggressor_flow WHERE timestamp >= ? AND timestamp <= ?",
        (start_iso, end_iso),
    )
    if flow_by_minute:
        flow_rows = []
        for ts_ms, (buy_vol, sell_vol, ratio, net_flow) in flow_by_minute.items():
            ts_iso = datetime.fromtimestamp(ts_ms / 1000, tz=timezone.utc).isoformat()
            flow_rows.append((ts_iso, buy_vol, sell_vol, ratio, net_flow))
        conn.executemany(
            "INSERT INTO ml_aggressor_flow (timestamp, buy_vol, sell_vol, ratio, net_flow) "
            "VALUES (?, ?, ?, ?, ?)",
            flow_rows,
        )
        conn.commit()
        print(f"  Aggressor flow rows inserted: {len(flow_rows)}")
    else:
        print("  [WARN] No aggressor flow data fetched.")

    # Funding rates
    funding_rows = fetch_binance_funding_rates(start_ms, end_ms, symbol=symbol)
    funding_ts = []
    funding_rates = []
    for row in funding_rows:
        try:
            funding_ts.append(int(row["fundingTime"]))
            funding_rates.append(float(row["fundingRate"]))
        except Exception:
            continue
    if funding_ts:
        print(f"  Funding points fetched: {len(funding_ts)}")
    else:
        print("  [WARN] No funding data fetched.")
    conn.execute(
        "DELETE FROM ml_funding_rates WHERE timestamp >= ? AND timestamp <= ? AND symbol = ?",
        (start_iso, end_iso, symbol),
    )
    if funding_ts:
        funding_rows_db = [
            (
                datetime.fromtimestamp(ts / 1000, tz=timezone.utc).isoformat(),
                rate,
                symbol,
            )
            for ts, rate in zip(funding_ts, funding_rates)
        ]
        conn.executemany(
            "INSERT INTO ml_funding_rates (timestamp, rate, symbol) VALUES (?, ?, ?)",
            funding_rows_db,
        )
        conn.commit()

    # Open interest history (last ~30 days)
    oi_rows = fetch_binance_open_interest_hist(start_ms, end_ms, symbol=symbol, period=ratio_period)
    oi_ts = []
    oi_vals = []
    for row in oi_rows:
        try:
            ts = int(row.get("timestamp", row.get("time", 0)))
            if ts:
                oi_ts.append(ts)
                oi_vals.append(float(row.get("sumOpenInterestValue", row.get("sumOpenInterest", 0.0))))
        except Exception:
            continue
    if oi_ts:
        print(f"  Open interest points fetched: {len(oi_ts)}")
    else:
        print("  [WARN] No open interest data fetched (check API key or endpoint access).")
    conn.execute(
        "DELETE FROM ml_open_interest_hist WHERE timestamp >= ? AND timestamp <= ? AND symbol = ? AND period = ?",
        (start_iso, end_iso, symbol, ratio_period),
    )
    if oi_ts:
        oi_rows_db = [
            (
                datetime.fromtimestamp(ts / 1000, tz=timezone.utc).isoformat(),
                value,
                symbol,
                ratio_period,
            )
            for ts, value in zip(oi_ts, oi_vals)
        ]
        conn.executemany(
            "INSERT INTO ml_open_interest_hist (timestamp, value_usd, symbol, period) VALUES (?, ?, ?, ?)",
            oi_rows_db,
        )
        conn.commit()

    # Long/short ratios (global + top traders)
    global_ls_rows = fetch_binance_futures_data_series(
        "globalLongShortAccountRatio", start_ms, end_ms, symbol=symbol, period=ratio_period,
    )
    top_account_rows = fetch_binance_futures_data_series(
        "topLongShortAccountRatio", start_ms, end_ms, symbol=symbol, period=ratio_period,
    )
    top_position_rows = fetch_binance_futures_data_series(
        "topLongShortPositionRatio", start_ms, end_ms, symbol=symbol, period=ratio_period,
    )

    global_ls_ts, global_ls_ratio = [], []
    for row in global_ls_rows:
        try:
            global_ls_ts.append(int(row.get("timestamp", 0)))
            global_ls_ratio.append(float(row.get("longShortRatio", 0.0)))
        except Exception:
            continue

    top_account_ts, top_account_ratio = [], []
    for row in top_account_rows:
        try:
            top_account_ts.append(int(row.get("timestamp", 0)))
            top_account_ratio.append(float(row.get("longShortRatio", 0.0)))
        except Exception:
            continue

    top_position_ts, top_position_ratio = [], []
    for row in top_position_rows:
        try:
            top_position_ts.append(int(row.get("timestamp", 0)))
            top_position_ratio.append(float(row.get("longShortRatio", 0.0)))
        except Exception:
            continue

    if global_ls_ts:
        print(f"  Global long/short points fetched: {len(global_ls_ts)}")
    else:
        print("  [WARN] No global long/short data fetched.")

    if top_account_ts:
        print(f"  Top account long/short points fetched: {len(top_account_ts)}")
    else:
        print("  [WARN] No top account long/short data fetched.")

    if top_position_ts:
        print(f"  Top position long/short points fetched: {len(top_position_ts)}")
    else:
        print("  [WARN] No top position long/short data fetched.")
    conn.execute(
        "DELETE FROM ml_long_short_ratios WHERE timestamp >= ? AND timestamp <= ? AND symbol = ? AND period = ?",
        (start_iso, end_iso, symbol, ratio_period),
    )
    ratio_rows_db = []
    for series_name, ts_values, ratio_values in [
        ("global_account", global_ls_ts, global_ls_ratio),
        ("top_account", top_account_ts, top_account_ratio),
        ("top_position", top_position_ts, top_position_ratio),
    ]:
        for ts, value in zip(ts_values, ratio_values):
            ratio_rows_db.append(
                (
                    datetime.fromtimestamp(ts / 1000, tz=timezone.utc).isoformat(),
                    series_name,
                    value,
                    symbol,
                    ratio_period,
                )
            )
    if ratio_rows_db:
        conn.executemany(
            "INSERT INTO ml_long_short_ratios (timestamp, series, ratio, symbol, period) VALUES (?, ?, ?, ?, ?)",
            ratio_rows_db,
        )
        conn.commit()

    # Liquidations
    liq_by_minute: Dict[int, Tuple[float, float, float, float]] = {}
    if fetch_liquidations:
        liq_rows = fetch_binance_force_orders(start_ms, end_ms, symbol=symbol)
        # Replace liquidation rows in range
        conn.execute(
            "DELETE FROM ml_liquidations WHERE timestamp >= ? AND timestamp <= ?",
            (start_iso, end_iso),
        )
        if liq_rows:
            insert_rows = []
            for row in liq_rows:
                try:
                    ts_ms = int(row.get("time", row.get("T", 0)))
                    if not ts_ms:
                        continue
                    side = row.get("side", "")
                    price = float(row.get("price", row.get("avgPrice", 0.0)))
                    qty = float(row.get("executedQty", row.get("origQty", row.get("quantity", 0.0))))
                    size_usd = price * qty if price and qty else 0.0
                    symbol_val = row.get("symbol", symbol)

                    ts_iso = datetime.fromtimestamp(ts_ms / 1000, tz=timezone.utc).isoformat()
                    insert_rows.append((ts_iso, side, size_usd, price, symbol_val))

                    minute_ms = (ts_ms // 60000) * 60000
                    buy_usd, sell_usd, _, _ = liq_by_minute.get(minute_ms, (0.0, 0.0, 0.0, 0.0))
                    if side.upper() == "BUY":
                        buy_usd += size_usd
                    elif side.upper() == "SELL":
                        sell_usd += size_usd
                    net_usd = buy_usd - sell_usd
                    total_usd = buy_usd + sell_usd
                    liq_by_minute[minute_ms] = (buy_usd, sell_usd, net_usd, total_usd)

                except Exception:
                    continue

            if insert_rows:
                conn.executemany(
                    "INSERT INTO ml_liquidations (timestamp, side, size_usd, price, symbol) "
                    "VALUES (?, ?, ?, ?, ?)",
                    insert_rows,
                )
                conn.commit()
                print(f"  Liquidation rows inserted: {len(insert_rows)}")
        else:
            print("  [WARN] No liquidation data fetched.")

    return {
        "flow_by_minute": flow_by_minute,
        "liq_by_minute": liq_by_minute,
        "liquidations_requested": fetch_liquidations,
        "funding_ts": funding_ts,
        "funding_rates": funding_rates,
        "oi_ts": oi_ts,
        "oi_vals": oi_vals,
        "global_ls_ts": global_ls_ts,
        "global_ls_ratio": global_ls_ratio,
        "top_account_ts": top_account_ts,
        "top_account_ratio": top_account_ratio,
        "top_position_ts": top_position_ts,
        "top_position_ratio": top_position_ratio,
        "ratio_period_ms": _period_to_ms(ratio_period),
    }


def main():
    parser = argparse.ArgumentParser(description="Backfill historical Polymarket + Binance data")
    parser.add_argument("--db", default="./data/macbot.db", help="SQLite database path")
    parser.add_argument("--days", type=int, default=30, help="How many days back to pull")
    parser.add_argument("--interval", default="both", choices=["5m", "15m", "both"],
                        help="Market interval to backfill")
    parser.add_argument("--start-date", default=None,
                        help="Start date (YYYY-MM-DD). Overrides --days.")
    parser.add_argument("--skip-context", action="store_true",
                        help="Skip fetching Binance Futures context (flow/funding/OI/liqs).")
    parser.add_argument("--refresh-existing", action="store_true",
                        help="Recompute features for markets already in the DB (no candle reinserts).")
    parser.add_argument("--ratio-period", default="5m",
                        help="Period for futures ratios/OI (e.g., 5m,15m,30m,1h,2h,4h,6h,12h,1d).")
    parser.add_argument("--fetch-liquidations", action="store_true",
                        help="Attempt to fetch liquidation history (requires API key/secret; user data only).")
    args = parser.parse_args()
    # Refuse an unsafe interpreter before creating a DB or converting a TLS
    # refusal inside collector retry loops into a successful empty backfill.
    require_safe_tls(GAMMA_API)

    # Ensure data directory exists
    db_path = Path(args.db)
    db_path.parent.mkdir(parents=True, exist_ok=True)

    conn = init_db(str(db_path))
    run_id = f"backfill_{datetime.now(timezone.utc).strftime('%Y%m%d_%H%M%S')}"

    now = datetime.now(timezone.utc)
    if args.start_date:
        start_dt = datetime.strptime(args.start_date, "%Y-%m-%d").replace(tzinfo=timezone.utc)
    else:
        start_dt = now - timedelta(days=args.days)

    # Round to interval boundaries
    start_ts = int(start_dt.timestamp())
    end_ts = int(now.timestamp()) - 3600  # Stop 1 hour ago (avoid in-progress markets)

    total_found = 0
    context = None
    if not args.skip_context:
        context = build_context(
            conn, start_ts, end_ts, symbol="BTCUSDT",
            ratio_period=args.ratio_period,
            fetch_liquidations=args.fetch_liquidations,
        )

    # Bulk pre-fetch ALL 1m candles for the entire range (Option 2: one download, slice locally)
    # Include lookback buffer so the earliest markets have enough candle history
    candle_cache_start_ms = (start_ts - CANDLE_LOOKBACK_MINUTES * 60) * 1000
    candle_cache_end_ms = end_ts * 1000
    candle_cache = None
    if not args.refresh_existing:
        # Only bulk-fetch when adding new markets (refresh uses DB candles)
        candle_cache = prefetch_all_candles(candle_cache_start_ms, candle_cache_end_ms)
    else:
        # For refresh, still try bulk fetch as fallback for incomplete DB windows
        candle_cache = prefetch_all_candles(candle_cache_start_ms, candle_cache_end_ms)
    if candle_cache:
        replace_candle_range(conn, candle_cache)

    if args.interval in ("15m", "both"):
        # Align to 15m boundary
        aligned_start = (start_ts // 900) * 900
        total_found += backfill_interval(
            conn, "15m", aligned_start, end_ts, run_id,
            context=context, refresh_existing=args.refresh_existing,
            candle_cache=candle_cache,
        )

    if args.interval in ("5m", "both"):
        # Align to 5m boundary
        aligned_start = (start_ts // 300) * 300
        total_found += backfill_interval(
            conn, "5m", aligned_start, end_ts, run_id,
            context=context, refresh_existing=args.refresh_existing,
            candle_cache=candle_cache,
        )

    # Print final stats
    candle_count = conn.execute("SELECT COUNT(*) FROM ml_candles_1m").fetchone()[0]
    feature_count = conn.execute(
        "SELECT COUNT(*) FROM ml_features_timeseries WHERE outcome IS NOT NULL"
    ).fetchone()[0]

    print(f"\n{'='*60}")
    print(f"BACKFILL COMPLETE")
    print(f"  New markets found this run: {total_found}")
    print(f"  Total candles in DB:        {candle_count}")
    print(f"  Total labeled samples in DB: {feature_count}")
    print(f"  Database: {db_path.resolve()}")
    print(f"{'='*60}")

    conn.close()


if __name__ == "__main__":
    main()
