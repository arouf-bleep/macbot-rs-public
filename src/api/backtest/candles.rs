//! Bounded, completed 15-minute Binance candles, with an in-process cache.
use super::KLINE_15M_MS;
use crate::{
    http_client::{JsonResponseExt, HTTP},
    signals::Candle,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

const BINANCE_PAGE_LIMIT: usize = 1000;
pub(super) const BACKTEST_KLINE_CACHE_TTL_SECS: u64 = 15 * 60;
const BACKTEST_KLINE_CACHE_MAX_ENTRIES: usize = 64;

#[derive(Clone)]
pub(super) struct CachedKlinesEntry {
    pub(super) stored_at: Instant,
    pub(super) rows: Arc<Vec<(i64, Candle)>>,
}

pub(super) static BACKTEST_KLINE_CACHE: LazyLock<
    dashmap::DashMap<(usize, i64), CachedKlinesEntry>,
> = LazyLock::new(dashmap::DashMap::new);

pub(super) fn parse_num_f64(v: &Value) -> Option<f64> {
    crate::json_util::parse_f64(v)
}

pub(super) fn parse_num_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().and_then(|x| i64::try_from(x).ok()))
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

pub(super) fn kline_open_time_ms(row: &Value) -> Option<i64> {
    row.as_array()
        .and_then(|a| a.first())
        .and_then(parse_num_i64)
}

pub(super) async fn fetch_historical_klines_15m(
    target_candles: usize,
    end_time_ms: i64,
) -> Result<Vec<(i64, Candle)>, String> {
    if let Some(cached) = cached_backtest_klines(target_candles, end_time_ms) {
        return Ok(cached);
    }
    let parsed = fetch_historical_pages(target_candles, end_time_ms, |start, limit| async move {
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=15m&limit={limit}&startTime={start}&endTime={end_time_ms}"
        );
        let resp = HTTP.get(&url).send().await.map_err(|e| format!("Binance fetch error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("Binance returned HTTP {}", resp.status()));
        }
        resp.bounded_json::<Vec<Value>>().await.map_err(|e| format!("invalid Binance candle payload: {e}"))
    }).await?;
    store_cached_backtest_klines(target_candles, end_time_ms, &parsed);
    Ok(parsed)
}

// A page callback permits exact horizon/pagination tests without an external request.
// The production callback above always uses the fixed public Binance endpoint.
pub(super) async fn fetch_historical_pages<F, Fut>(
    target_candles: usize,
    end_time_ms: i64,
    mut page: F,
) -> Result<Vec<(i64, Candle)>, String>
where
    F: FnMut(i64, usize) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Value>, String>>,
{
    if target_candles > super::MAX_BACKTEST_DAYS as usize * super::CANDLES_PER_DAY + 122
        || end_time_ms < 0
    {
        return Err("candle request exceeds the supported count/time bounds".into());
    }
    if target_candles == 0 {
        return Ok(Vec::new());
    }
    // Include the full buffer and possible still-forming candle, then select
    // newest completed rows. Stopping at the raw target would select a stale
    // EARLIER window rather than the requested end horizon.
    let raw_target = target_candles + 9;
    let horizon_start = end_time_ms
        .saturating_sub((target_candles as i64 + 8) * KLINE_15M_MS)
        .max(0);
    let mut start_time_ms = horizon_start;
    let mut rows: Vec<Value> = Vec::new();
    let mut seen = BTreeMap::new();
    for _ in 0..256 {
        if start_time_ms >= end_time_ms || rows.len() >= raw_target {
            break;
        }
        let limit = (raw_target - rows.len()).clamp(1, BINANCE_PAGE_LIMIT);
        let batch = page(start_time_ms, limit).await?;
        if batch.len() > limit {
            return Err("candle source returned more rows than the requested page limit".into());
        }
        if batch.is_empty() {
            break;
        }
        let batch_len = batch.len();
        let mut previous_open = None;
        for row in batch {
            let (open, candle) = parse_kline_row(&row).ok_or_else(|| {
                "candle source contains malformed timestamps or OHLCV".to_string()
            })?;
            if open % KLINE_15M_MS != 0 || open < horizon_start || open > end_time_ms {
                return Err("candle source open time is off the UTC 15-minute grid or outside the requested horizon".into());
            }
            let expected_close = open.checked_add(KLINE_15M_MS - 1);
            if row
                .as_array()
                .and_then(|a| a.get(6))
                .and_then(parse_num_i64)
                != expected_close
            {
                return Err(
                    "candle source close time does not match its 15-minute interval".into(),
                );
            }
            if previous_open.is_some_and(|previous| open < previous) {
                return Err("candle source page is not ordered by open time".into());
            }
            previous_open = Some(open);
            if let Some(prior) = seen.get(&open) {
                if !same_ohlcv(prior, &candle) {
                    return Err(format!(
                        "conflicting candle source values for open time {open}"
                    ));
                }
                // An identical overlap is safe; it is not an extra observation.
                continue;
            }
            if open < start_time_ms {
                return Err(
                    "candle source introduced an earlier, previously unseen row during pagination"
                        .into(),
                );
            }
            seen.insert(open, candle);
            rows.push(row);
        }
        let latest_open = previous_open.expect("nonempty validated candle page");
        if latest_open < start_time_ms {
            return Err("candle source pagination made no forward progress".into());
        }
        start_time_ms = latest_open.saturating_add(KLINE_15M_MS);
        if batch_len < limit {
            break;
        }
    }
    let mut parsed = completed_kline_rows(&rows, end_time_ms);
    parsed.retain(|(open, _)| *open >= horizon_start);
    if parsed.len() > target_candles {
        parsed = parsed.split_off(parsed.len() - target_candles);
    }
    Ok(parsed)
}

fn same_ohlcv(a: &Candle, b: &Candle) -> bool {
    [a.open, a.high, a.low, a.close, a.volume] == [b.open, b.high, b.low, b.close, b.volume]
}

fn parse_kline_row(row: &Value) -> Option<(i64, Candle)> {
    let arr = row.as_array()?;
    let open_time_ms = kline_open_time_ms(row)?;
    let candle = Candle {
        open: arr.get(1).and_then(parse_num_f64)?,
        high: arr.get(2).and_then(parse_num_f64)?,
        low: arr.get(3).and_then(parse_num_f64)?,
        close: arr.get(4).and_then(parse_num_f64)?,
        volume: arr.get(5).and_then(parse_num_f64)?,
    };
    if open_time_ms < 0
        || chrono::DateTime::from_timestamp_millis(open_time_ms).is_none()
        || [candle.open, candle.high, candle.low, candle.close]
            .iter()
            .any(|p| !(1e-8..=1e12).contains(p))
        || !(0.0..=1e18).contains(&candle.volume)
        || candle.low > candle.high
        || candle.low > candle.open.min(candle.close)
        || candle.high < candle.open.max(candle.close)
    {
        return None;
    }
    Some((open_time_ms, candle))
}

pub(super) fn parse_kline_rows(rows: &[Value]) -> Vec<(i64, Candle)> {
    let mut parsed: Vec<(i64, Candle)> = rows.iter().filter_map(parse_kline_row).collect();
    parsed.sort_by_key(|(ts, _)| *ts);
    parsed.dedup_by_key(|(ts, _)| *ts);
    parsed
}

pub(super) fn completed_kline_rows(rows: &[Value], end_time_ms: i64) -> Vec<(i64, Candle)> {
    let mut parsed = parse_kline_rows(rows);
    parsed.retain(|(open, _)| {
        open.checked_add(KLINE_15M_MS)
            .is_some_and(|close| close <= end_time_ms)
    });
    parsed
}

pub(super) fn trade_clock(feature_open: i64, outcome_open: i64) -> Option<(i64, i64)> {
    let entry = feature_open.checked_add(KLINE_15M_MS)?;
    if entry != outcome_open {
        return None;
    }
    Some((entry, outcome_open.checked_add(KLINE_15M_MS)?))
}

pub(super) fn cached_backtest_klines(
    target_candles: usize,
    end_time_ms: i64,
) -> Option<Vec<(i64, Candle)>> {
    let key = (target_candles, end_time_ms);
    let entry = BACKTEST_KLINE_CACHE.get(&key)?;
    if entry.stored_at.elapsed() > Duration::from_secs(BACKTEST_KLINE_CACHE_TTL_SECS) {
        drop(entry);
        BACKTEST_KLINE_CACHE.remove(&key);
        return None;
    }
    Some(entry.rows.as_ref().clone())
}

pub(super) fn store_cached_backtest_klines(
    target_candles: usize,
    end_time_ms: i64,
    rows: &[(i64, Candle)],
) {
    BACKTEST_KLINE_CACHE.insert(
        (target_candles, end_time_ms),
        CachedKlinesEntry {
            stored_at: Instant::now(),
            rows: Arc::new(rows.to_vec()),
        },
    );
    prune_backtest_kline_cache();
}

pub(super) fn prune_backtest_kline_cache() {
    let ttl = Duration::from_secs(BACKTEST_KLINE_CACHE_TTL_SECS);
    BACKTEST_KLINE_CACHE.retain(|_, entry| entry.stored_at.elapsed() <= ttl);
    while BACKTEST_KLINE_CACHE.len() > BACKTEST_KLINE_CACHE_MAX_ENTRIES {
        let oldest_key = BACKTEST_KLINE_CACHE
            .iter()
            .min_by_key(|entry| entry.stored_at)
            .map(|entry| *entry.key());
        match oldest_key {
            Some(key) => {
                BACKTEST_KLINE_CACHE.remove(&key);
            }
            None => break,
        }
    }
}

#[cfg(test)]
mod tests;
