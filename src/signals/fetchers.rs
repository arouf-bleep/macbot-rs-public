//! Async HTTP fetchers for all external data sources.
//! Uses the shared HTTP client from http_client module.

use crate::http_client::JsonResponseExt;
use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::http_client::HTTP;
use crate::json_util::parse_f64;

const FETCH_CIRCUIT_FAILURE_THRESHOLD: u32 = 3;
const FETCH_CIRCUIT_OPEN_SECS: u64 = 300;

#[derive(Debug, Clone, Default)]
struct FetchCircuitState {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

static FETCH_CIRCUITS: Lazy<Mutex<HashMap<&'static str, FetchCircuitState>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

fn fetch_circuits_guard() -> MutexGuard<'static, HashMap<&'static str, FetchCircuitState>> {
    match FETCH_CIRCUITS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!("fetch circuit mutex was poisoned; recovering state");
            FETCH_CIRCUITS.clear_poison();
            poisoned.into_inner()
        }
    }
}

fn fetch_circuit_allows(source: &'static str) -> Result<()> {
    let mut guard = fetch_circuits_guard();
    let state = guard.entry(source).or_default();
    if let Some(open_until) = state.open_until {
        if Instant::now() < open_until {
            let remaining = open_until
                .saturating_duration_since(Instant::now())
                .as_secs();
            return Err(anyhow!(
                "{source} fetch circuit open for another {remaining}s"
            ));
        }
        state.open_until = None;
        state.consecutive_failures = 0;
    }
    Ok(())
}

fn fetch_circuit_record_success(source: &'static str) {
    let mut guard = fetch_circuits_guard();
    let state = guard.entry(source).or_default();
    state.consecutive_failures = 0;
    state.open_until = None;
}

fn fetch_circuit_record_failure(source: &'static str) {
    let mut guard = fetch_circuits_guard();
    let state = guard.entry(source).or_default();
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    if state.consecutive_failures >= FETCH_CIRCUIT_FAILURE_THRESHOLD {
        state.open_until = Some(Instant::now() + Duration::from_secs(FETCH_CIRCUIT_OPEN_SECS));
        tracing::warn!(
            "{source} fetch circuit opened after {} consecutive failures",
            state.consecutive_failures
        );
        state.consecutive_failures = 0;
    }
}

async fn with_fetch_circuit<T, Fut>(source: &'static str, op: impl FnOnce() -> Fut) -> Result<T>
where
    Fut: Future<Output = Result<T>>,
{
    fetch_circuit_allows(source)?;
    match op().await {
        Ok(value) => {
            fetch_circuit_record_success(source);
            Ok(value)
        }
        Err(err) => {
            fetch_circuit_record_failure(source);
            Err(err)
        }
    }
}

// ── Binance ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BinanceCandle {
    pub open_time_ms: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

impl From<BinanceCandle> for crate::signals::Candle {
    fn from(b: BinanceCandle) -> Self {
        Self {
            open: b.open,
            high: b.high,
            low: b.low,
            close: b.close,
            volume: b.volume,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AggTrade {
    pub qty: f64,
    pub is_buyer_maker: bool,
}

pub async fn fetch_binance_candles(limit: usize) -> Result<Vec<BinanceCandle>> {
    fetch_binance_candles_interval(limit, "15m").await
}

pub async fn fetch_binance_candles_1m(limit: usize) -> Result<Vec<BinanceCandle>> {
    fetch_binance_candles_interval(limit, "1m").await
}

async fn fetch_binance_candles_interval(
    limit: usize,
    interval: &str,
) -> Result<Vec<BinanceCandle>> {
    let url = format!(
        "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval={interval}&limit={}",
        limit
    );
    let resp: Value = HTTP
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .bounded_json()
        .await?;
    parse_binance_candles(&resp)
}

fn parse_binance_candles(resp: &Value) -> Result<Vec<BinanceCandle>> {
    let arr = resp.as_array().ok_or_else(|| anyhow!("expected array"))?;
    let candles = arr
        .iter()
        .filter_map(|k| {
            let a = k.as_array()?;
            let candle = BinanceCandle {
                open_time_ms: a
                    .first()?
                    .as_i64()
                    .or_else(|| a.first()?.as_u64().and_then(|v| i64::try_from(v).ok()))?,
                open: parse_f64(a.get(1)?)?,
                high: parse_f64(a.get(2)?)?,
                low: parse_f64(a.get(3)?)?,
                close: parse_f64(a.get(4)?)?,
                volume: parse_f64(a.get(5)?)?,
            };
            if candle.open <= 0.0
                || candle.low <= 0.0
                || candle.close <= 0.0
                || candle.volume < 0.0
                || candle.high < candle.open.max(candle.close)
                || candle.low > candle.open.min(candle.close)
            {
                return None;
            }
            Some(candle)
        })
        .collect();
    Ok(candles)
}

pub async fn fetch_binance_agg_trades(limit: usize) -> Result<Vec<AggTrade>> {
    let url = format!(
        "https://api.binance.com/api/v3/aggTrades?symbol=BTCUSDT&limit={}",
        limit.max(1)
    );
    let resp: Value = HTTP.get(&url).send().await?.bounded_json().await?;
    let arr = resp.as_array().ok_or_else(|| anyhow!("expected array"))?;
    let trades = arr
        .iter()
        .filter_map(|t| {
            let qty = t
                .get("q")
                .and_then(parse_f64)
                .or_else(|| t.get("qty").and_then(parse_f64))
                .unwrap_or(0.0);
            let is_buyer_maker = t.get("m").and_then(|v| v.as_bool()).unwrap_or(false);
            if qty <= 0.0 {
                return None;
            }
            Some(AggTrade {
                qty,
                is_buyer_maker,
            })
        })
        .collect();
    Ok(trades)
}

#[derive(Debug, Clone, Default)]
pub struct FuturesData {
    pub funding_rate: Option<f64>,
    pub open_interest: Option<f64>,
    pub basis_pct: Option<f64>,
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
}

fn parse_futures_basis(v: &Value) -> (Option<f64>, Option<f64>, Option<f64>) {
    let mark_price = v
        .get("markPrice")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let index_price = v
        .get("indexPrice")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<f64>().ok());
    let basis_pct = match (mark_price, index_price) {
        (Some(mark), Some(index)) if index.abs() > f64::EPSILON => {
            Some((mark - index) / index * 100.0)
        }
        _ => None,
    };
    (basis_pct, mark_price, index_price)
}

pub async fn fetch_binance_futures() -> Result<FuturesData> {
    let fr_url = "https://fapi.binance.com/fapi/v1/fundingRate?symbol=BTCUSDT&limit=1";
    let oi_url = "https://fapi.binance.com/fapi/v1/openInterest?symbol=BTCUSDT";
    let premium_url = "https://fapi.binance.com/fapi/v1/premiumIndex?symbol=BTCUSDT";

    let (fr_resp, oi_resp, premium_resp): (Result<Value>, Result<Value>, Result<Value>) = tokio::join!(
        async { HTTP.get(fr_url).send().await?.bounded_json::<Value>().await },
        async { HTTP.get(oi_url).send().await?.bounded_json::<Value>().await },
        async {
            HTTP.get(premium_url)
                .send()
                .await?
                .bounded_json::<Value>()
                .await
        },
    );

    let funding_rate = fr_resp.ok().and_then(|v| {
        v.as_array()?
            .last()?
            .get("fundingRate")?
            .as_str()?
            .parse()
            .ok()
    });
    let open_interest = oi_resp
        .ok()
        .and_then(|v| v.get("openInterest")?.as_str()?.parse().ok());
    let (basis_pct, mark_price, index_price) = premium_resp
        .ok()
        .map(|v| parse_futures_basis(&v))
        .unwrap_or((None, None, None));

    Ok(FuturesData {
        funding_rate,
        open_interest,
        basis_pct,
        mark_price,
        index_price,
    })
}

// ── Binance L2 Depth ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DepthImbalance {
    /// (bid_usd - ask_usd) / (bid_usd + ask_usd) for top-N levels, range [-1, 1]
    pub imbalance_5: f64,
}

/// Fetch Binance BTCUSDT orderbook (top 20 levels) and compute depth imbalance
/// for the top-5 levels.
pub async fn fetch_binance_depth() -> Result<DepthImbalance> {
    let url = "https://api.binance.com/api/v3/depth?symbol=BTCUSDT&limit=20";
    let resp: Value = HTTP.get(url).send().await?.bounded_json().await?;

    let bids = resp
        .get("bids")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("missing bids"))?;
    let asks = resp
        .get("asks")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("missing asks"))?;

    // Compute USD depth for top-5 levels
    let mut bid_usd = 0.0_f64;
    let mut ask_usd = 0.0_f64;

    for level in bids.iter().take(5) {
        if let Some(arr) = level.as_array() {
            let price = arr
                .first()
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let qty = arr
                .get(1)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            bid_usd += price * qty;
        }
    }
    for level in asks.iter().take(5) {
        if let Some(arr) = level.as_array() {
            let price = arr
                .first()
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            let qty = arr
                .get(1)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            ask_usd += price * qty;
        }
    }

    let total = bid_usd + ask_usd;
    let imbalance_5 = if total > 0.0 {
        (bid_usd - ask_usd) / total
    } else {
        0.0
    };

    Ok(DepthImbalance { imbalance_5 })
}

// ── LunarCrush ───────────────────────────────────────────────────────────────

fn normalize_sentiment(raw: f64) -> Option<f64> {
    if raw.is_nan() {
        return None;
    }
    // already [-1, 1]
    if raw.abs() <= 1.0 {
        return Some(raw.clamp(-1.0, 1.0));
    }
    // 1–5 scale (LunarCrush v4 average_sentiment) → [-1, 1]
    if (1.0..=5.0).contains(&raw) {
        return Some(((raw - 3.0) / 2.0).clamp(-1.0, 1.0));
    }
    // 0–100 scale → [-1, 1]
    if (0.0..=100.0).contains(&raw) {
        return Some(((raw / 100.0) * 2.0 - 1.0).clamp(-1.0, 1.0));
    }
    None
}

pub async fn fetch_lunarcrush_sentiment(api_key: &str) -> Result<(Option<f64>, Option<f64>)> {
    if api_key.trim().is_empty() {
        return Ok((None, None));
    }
    with_fetch_circuit("lunarcrush_sentiment", || async move {
        // LunarCrush v4 API — Bearer token auth, returns object not array
        let url = "https://lunarcrush.com/api4/public/coins/BTC/v1";
        let http_resp = HTTP.get(url).bearer_auth(api_key.trim()).send().await?;

        // Handle rate limiting explicitly
        if http_resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || http_resp.status().as_u16() == 429
        {
            tracing::warn!("LunarCrush: daily rate limit exceeded");
            return Ok((None, None));
        }

        let resp: Value = http_resp.bounded_json().await?;

        // Check for error in response body (LunarCrush returns 200 with error field)
        if let Some(err) = resp.get("error").and_then(|v| v.as_str()) {
            if err.to_lowercase().contains("rate limit") {
                tracing::warn!("LunarCrush: {err}");
                return Ok((None, None));
            }
        }

        let data = resp.get("data").cloned().unwrap_or(Value::Null);

        let score = data
            .get("sentiment")
            .and_then(parse_f64)
            .and_then(normalize_sentiment)
            .or_else(|| {
                data.get("average_sentiment")
                    .and_then(parse_f64)
                    .and_then(normalize_sentiment)
            })
            .or_else(|| {
                data.get("sentiment_score")
                    .and_then(parse_f64)
                    .and_then(normalize_sentiment)
            })
            .or_else(|| {
                data.get("galaxy_score")
                    .and_then(parse_f64)
                    .and_then(normalize_sentiment)
            });

        let social_volume = data
            .get("social_volume_24h")
            .and_then(parse_f64)
            .or_else(|| data.get("social_volume").and_then(parse_f64))
            .or_else(|| data.get("interactions_24h").and_then(parse_f64));

        Ok((score, social_volume))
    })
    .await
}

// ── Polymarket ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct RawOrderBook {
    pub bids: Vec<(f64, f64)>, // (price, size)
    pub asks: Vec<(f64, f64)>,
}

pub async fn fetch_polymarket_orderbook(token_id: &str) -> Result<RawOrderBook> {
    let resp: Value = HTTP
        .get("https://clob.polymarket.com/book")
        .query(&[("token_id", token_id)])
        .send()
        .await?
        .error_for_status()?
        .bounded_json()
        .await?;
    Ok(RawOrderBook {
        bids: crate::json_util::extract_levels(&resp, "bids"),
        asks: crate::json_util::extract_levels(&resp, "asks"),
    })
}

// ── Deribit ───────────────────────────────────────────────────────────────────

pub use super::deribit::PcrOpenInterest;

pub async fn fetch_deribit_pcr() -> Result<PcrOpenInterest> {
    with_fetch_circuit("deribit_pcr", || async {
        let url = "https://www.deribit.com/api/v2/public/get_book_summary_by_currency\
                   ?currency=BTC&kind=option";
        let resp: Value = HTTP
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bounded_json()
            .await?;
        super::deribit::parse_pcr_summary(&resp, chrono::Utc::now().timestamp_millis())
    })
    .await
}

// ── Fear & Greed ──────────────────────────────────────────────────────────────

pub async fn fetch_fear_greed() -> Result<u32> {
    with_fetch_circuit("fear_greed", || async {
        let url = "https://api.alternative.me/fng/?limit=1";
        let resp: Value = HTTP.get(url).send().await?.bounded_json().await?;
        let value: u32 = resp
            .pointer("/data/0/value")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(50);
        Ok(value)
    })
    .await
}

// ── Altcoin Correlation (#14) ────────────────────────────────────────────────

/// Fetch latest price change for ETH and SOL from Binance, compute correlation
/// with BTC's recent move. Divergence = potential reversal signal.
#[derive(Debug, Clone)]
pub struct AltcoinCorrelation {
    pub eth_change_pct: f64,
    pub sol_change_pct: f64,
    /// How aligned alts are with BTC: 1.0 = perfect correlation, -1.0 = divergence
    pub correlation_score: f64,
}

pub async fn fetch_altcoin_correlation(btc_change_pct: f64) -> Result<AltcoinCorrelation> {
    let live_bus = crate::signals::price_bus::shared_price_bus();
    let btc_live_change = live_bus.price_change_pct("BTCUSDT", 45.0, 25.0).await;
    let eth_live_change = live_bus.price_change_pct("ETHUSDT", 45.0, 25.0).await;
    let sol_live_change = live_bus.price_change_pct("SOLUSDT", 45.0, 25.0).await;

    if let (Some(eth_change), Some(sol_change)) = (eth_live_change, sol_live_change) {
        let btc_change = btc_live_change.unwrap_or(btc_change_pct);
        let eth_agreement = direction_agreement(btc_change, eth_change);
        let sol_agreement = direction_agreement(btc_change, sol_change);
        return Ok(AltcoinCorrelation {
            eth_change_pct: eth_change,
            sol_change_pct: sol_change,
            correlation_score: (eth_agreement + sol_agreement) / 2.0,
        });
    }

    let eth_url = "https://api.binance.com/api/v3/klines?symbol=ETHUSDT&interval=15m&limit=2";
    let sol_url = "https://api.binance.com/api/v3/klines?symbol=SOLUSDT&interval=15m&limit=2";

    let (eth_resp, sol_resp) = tokio::join!(
        async {
            HTTP.get(eth_url)
                .send()
                .await?
                .bounded_json::<Value>()
                .await
        },
        async {
            HTTP.get(sol_url)
                .send()
                .await?
                .bounded_json::<Value>()
                .await
        }
    );

    let parse_change = |resp: Result<Value, _>| -> Option<f64> {
        resp.ok()
            .and_then(|v| v.as_array().cloned())
            .and_then(|arr| {
                if arr.len() >= 2 {
                    let prev_close = arr[0].as_array()?.get(4)?.as_str()?.parse::<f64>().ok()?;
                    let curr_close = arr[1].as_array()?.get(4)?.as_str()?.parse::<f64>().ok()?;
                    if !prev_close.is_finite()
                        || !curr_close.is_finite()
                        || prev_close <= 0.0
                        || curr_close <= 0.0
                    {
                        return None;
                    }
                    Some((curr_close - prev_close) / prev_close * 100.0)
                } else {
                    None
                }
            })
    };

    let eth_change =
        parse_change(eth_resp).ok_or_else(|| anyhow!("ETH price change unavailable"))?;
    let sol_change =
        parse_change(sol_resp).ok_or_else(|| anyhow!("SOL price change unavailable"))?;

    // Correlation score: average of sign agreement with BTC
    let eth_agreement = direction_agreement(btc_change_pct, eth_change);
    let sol_agreement = direction_agreement(btc_change_pct, sol_change);
    let correlation_score = (eth_agreement + sol_agreement) / 2.0;

    Ok(AltcoinCorrelation {
        eth_change_pct: eth_change,
        sol_change_pct: sol_change,
        correlation_score,
    })
}

fn direction_agreement(first: f64, second: f64) -> f64 {
    if !first.is_finite() || !second.is_finite() || first == 0.0 || second == 0.0 {
        0.0
    } else if first.signum() == second.signum() {
        1.0
    } else {
        -1.0
    }
}

// ── CryptoPanic News Sentiment (#14) ─────────────────────────────────────

/// Fetch recent crypto news headlines from CryptoPanic (free, no auth)
/// and score them with keyword-based sentiment analysis.
/// Returns a score in [-1, 1]. Caller should cache for 5 minutes.
pub async fn fetch_crypto_news_sentiment() -> Result<Option<f64>> {
    with_fetch_circuit("crypto_news_sentiment", || async {
        let url = "https://cryptopanic.com/api/free/v1/posts/?currencies=BTC&kind=news";
        let resp: Value = HTTP.get(url).send().await?.bounded_json().await?;

        let results = match resp.get("results").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => return Ok(None),
        };
        if results.is_empty() {
            return Ok(None);
        }

        // Filter to headlines from the last 15 minutes
        let now = chrono::Utc::now();
        let cutoff = now - chrono::Duration::minutes(15);

        let recent: Vec<&Value> = results
            .iter()
            .filter(|item| {
                item.get("published_at")
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt >= cutoff)
                    .unwrap_or(false)
            })
            .collect();

        // Fall back to all results if none are recent enough
        let headlines: Vec<&Value> = if recent.is_empty() {
            results.iter().take(20).collect()
        } else {
            recent
        };

        const BULL: &[&str] = &[
            "surge",
            "rally",
            "bullish",
            "etf",
            "adoption",
            "buy",
            "soar",
            "breakout",
            "pump",
            "accumulate",
            "institutional",
        ];
        const BEAR: &[&str] = &[
            "crash",
            "dump",
            "bearish",
            "hack",
            "ban",
            "sell",
            "plunge",
            "liquidat",
            "fear",
            "panic",
            "investigation",
        ];

        let mut bull_count = 0i32;
        let mut bear_count = 0i32;

        for item in &headlines {
            let title = item.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let lower = title.to_ascii_lowercase();
            for kw in BULL {
                if lower.contains(kw) {
                    bull_count += 1;
                    break;
                }
            }
            for kw in BEAR {
                if lower.contains(kw) {
                    bear_count += 1;
                    break;
                }
            }
        }

        let total = (bull_count + bear_count).max(1) as f64;
        let score = (bull_count as f64 - bear_count as f64) / total;
        Ok(Some(score.clamp(-1.0, 1.0)))
    })
    .await
}

// ── Cross-Exchange Price (#30) ───────────────────────────────────────────────

/// Fetch BTC price from Coinbase for cross-exchange divergence detection.
pub async fn fetch_coinbase_btc_price() -> Result<f64> {
    with_fetch_circuit("coinbase_btc_price", || async {
        let url = "https://api.coinbase.com/v2/prices/BTC-USD/spot";
        let resp: Value = HTTP
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bounded_json()
            .await?;
        let price = resp
            .pointer("/data/amount")
            .and_then(parse_f64)
            .filter(|price| *price > 0.0)
            .ok_or_else(|| anyhow!("Coinbase: no price"))?;
        Ok(price)
    })
    .await
}

// ── Polymarket Trades (#1) ─────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct PolymarketTradeData {
    pub yes_volume: f64,
    pub no_volume: f64,
    pub price_start: f64,
    pub price_now: f64,
    pub large_fill_detected: bool,
    pub elapsed_mins: f64,
}

/// Read public, market-scoped taker trades. BUY/SELL is transaction direction;
/// it is not the market outcome. A YES purchase or NO sale is bullish flow.
pub async fn fetch_polymarket_flow(
    market_id: &str,
    yes_token_id: &str,
) -> Result<PolymarketTradeData> {
    anyhow::ensure!(
        market_id.len() == 66
            && market_id.starts_with("0x")
            && market_id[2..].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "public trade flow requires a condition ID"
    );
    let resp: Value = HTTP
        .get("https://data-api.polymarket.com/trades")
        .query(&[
            ("market", market_id),
            ("limit", "100"),
            ("takerOnly", "true"),
        ])
        .send()
        .await?
        .error_for_status()?
        .bounded_json()
        .await?;
    parse_polymarket_flow(
        &resp,
        market_id,
        yes_token_id,
        chrono::Utc::now().timestamp(),
    )
}

fn parse_polymarket_flow(
    resp: &Value,
    market_id: &str,
    yes_token_id: &str,
    now: i64,
) -> Result<PolymarketTradeData> {
    let trades = resp
        .as_array()
        .ok_or_else(|| anyhow!("public trades response is not an array"))?;
    let mut data = PolymarketTradeData {
        elapsed_mins: 5.0,
        ..Default::default()
    };
    let mut oldest: Option<(i64, f64)> = None;
    let mut newest: Option<(i64, f64)> = None;
    for trade in trades {
        if trade.get("conditionId").and_then(Value::as_str) != Some(market_id) {
            continue;
        }
        let Some(ts) = trade.get("timestamp").and_then(Value::as_i64) else {
            continue;
        };
        if ts < now.saturating_sub(300) || ts > now {
            continue;
        }
        let Some(size) = trade
            .get("size")
            .and_then(parse_f64)
            .filter(|size| *size > 0.0)
        else {
            continue;
        };
        let Some(price) = trade
            .get("price")
            .and_then(parse_f64)
            .filter(|price| *price > 0.0 && *price <= 1.0)
        else {
            continue;
        };
        let Some(asset) = trade
            .get("asset")
            .and_then(Value::as_str)
            .filter(|asset| !asset.is_empty())
        else {
            continue;
        };
        let side = trade.get("side").and_then(Value::as_str).unwrap_or("");
        if !matches!(side, "BUY" | "SELL") {
            continue;
        }
        let is_yes = asset == yes_token_id;
        if is_yes == (side == "BUY") {
            data.yes_volume += size;
        } else {
            data.no_volume += size;
        }
        data.large_fill_detected |= size * price > 500.0;
        let yes_price = if is_yes { price } else { 1.0 - price };
        if oldest.is_none_or(|(time, _)| ts < time) {
            oldest = Some((ts, yes_price));
        }
        if newest.is_none_or(|(time, _)| ts > time) {
            newest = Some((ts, yes_price));
        }
    }
    data.price_start = oldest.map_or(0.0, |(_, price)| price);
    data.price_now = newest.map_or(0.0, |(_, price)| price);
    Ok(data)
}

// ── Blockchair Whale Alert (#3) ──────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct WhaleTransferData {
    pub inflow_usd: f64,  // BTC transfers TO exchanges (sell pressure)
    pub outflow_usd: f64, // BTC transfers FROM exchanges (accumulation)
}

/// Inspect a small sample of pending large transfers against static wallet
/// labels. This is a research heuristic, not comprehensive exchange flow.
pub async fn fetch_whale_transfers() -> Result<WhaleTransferData> {
    with_fetch_circuit("whale_transfers", || async {
        let (inflow_usd, outflow_usd) = fetch_mempool_large_txns().await?;
        Ok(WhaleTransferData {
            inflow_usd,
            outflow_usd,
        })
    })
    .await
}

/// Static heuristic exchange wallet labels. Match complete addresses only;
/// ownership and coverage are not independently guaranteed by this software.
/// Sources: Arkham Intelligence, WalletExplorer, Bitfinex GitHub, OKLink.
const EXCHANGE_IDENTIFIERS: &[&str] = &[
    // ── Binance ──
    "bc1qm34lsc65zpw79lxes69zkqmk6ee3ewf0j77s3", // Binance hot wallet (~107K BTC)
    "34xp4vRoCGJym3xR7yCVPFHoCNxv4Twseo",        // Binance cold wallet (~248K BTC)
    "bc1qjasf9z3h7w3jspkhtgatgpyvvzgpa2wwd2lr0", // Binance deposit aggregation
    "3M219KR5vEneNb47ewrPfWyb5jQ2DjxRP6",        // Binance cold
    "1NDyJtNTjmwk5xPNhjgAMu4HDHigtobu1s",        // Binance-linked
    // ── Bitfinex (from github.com/bitfinexcom/pub/blob/main/wallets.txt) ──
    "1Kr6QSydW9bFQG1mXiPNNu6WpJGmUa9i1g", // Bitfinex hot wallet
    "3JZq4atUahhuA9rLhXLMhhTo133J9rF97j", // Bitfinex cold wallet
    "bc1qgdjqv0av3q56jvd82tkdjpy7gdp9ut8tlqmgrpmv24sq90ecnvqqjwvw97", // Bitfinex cold
    // ── Coinbase ──
    "bc1q4c8n5t00jmj8temxdgcc3t32nkg2wjwz24lywv", // Coinbase prime
    "3FHNBLobJnbCTFTVakh5TXmEneyf5PT61B",         // Coinbase cold
    "bc1qx9t2l3pyny2spqpqlye8svce70nppwtaxwdrp4", // Coinbase institutional
    // ── Kraken ──
    "1FzWLkAahHooV3kzTgyx6qsXoRDrBv5CJR",         // Kraken hot
    "bc1qr4dl5wa7kl8yu792dceg9z5knl2gkn220lk7a9", // Kraken cold
    // ── OKX ──
    "bc1q2s3rjwvam9dt2ftt4sqxqjf3twav0gdx0k0q2etjz8kf9nnrjyeqgc4j2m", // OKX cold
    "3LtrsjtyLsE6pYhav4oSqLFxBHVNYSmKWK",                             // OKX hot
];

/// Fetch recent large BTC transactions from Blockchair.
/// Returns (exchange_inflow_usd, exchange_outflow_usd).
/// Fetch pending large transactions from mempool.space.
/// Unconfirmed transactions may be replaced or never confirmed.
/// Returns (exchange_inflow_usd, exchange_outflow_usd).
///
/// Two-step approach:
/// 1. `/api/mempool/recent` → get txids + values (no addresses in this endpoint)
/// 2. `/api/tx/{txid}` → get full vout with addresses for large txns only
async fn fetch_mempool_large_txns() -> Result<(f64, f64)> {
    // Step 1: Get recent mempool txids and values
    let url = "https://mempool.space/api/mempool/recent";
    let resp: Value = HTTP
        .get(url)
        .send()
        .await
        .map_err(|e| anyhow!("mempool recent: {e}"))?
        .error_for_status()?
        .bounded_json()
        .await?;

    let txns = resp
        .as_array()
        .ok_or_else(|| anyhow!("mempool recent: expected transaction array"))?;

    // Fetch BTC price for USD conversion
    let btc_price_resp: Value = HTTP
        .get("https://mempool.space/api/v1/prices")
        .send()
        .await
        .map_err(|e| anyhow!("mempool prices: {e}"))?
        .error_for_status()?
        .bounded_json()
        .await?;
    let btc_price = btc_price_resp.get("USD").and_then(parse_f64).unwrap_or(0.0);

    anyhow::ensure!(btc_price > 0.0, "mempool prices: no valid USD price");

    // Collect large txids (>10 BTC) — limit to 5 detail fetches to avoid rate-limiting
    let mut large_txids = Vec::new();
    for txn in txns {
        let value_sat = txn.get("value").and_then(parse_f64).unwrap_or(0.0);
        let value_btc = value_sat / 100_000_000.0;
        if value_btc >= 10.0 {
            if let Some(txid) = txn.get("txid").and_then(|v| v.as_str()) {
                large_txids.push((txid.to_string(), value_sat));
                if large_txids.len() >= 5 {
                    break;
                }
            }
        }
    }

    if large_txids.is_empty() {
        return Ok((0.0, 0.0));
    }

    // Step 2: Fetch full tx details to get output addresses
    let mut inflow = 0.0_f64;
    let mut outflow = 0.0_f64;

    let detail_futs: Vec<_> = large_txids
        .iter()
        .map(|(txid, _)| {
            let url = format!("https://mempool.space/api/tx/{txid}");
            async move {
                HTTP.get(&url)
                    .send()
                    .await
                    .ok()?
                    .error_for_status()
                    .ok()?
                    .bounded_json::<Value>()
                    .await
                    .ok()
            }
        })
        .collect();

    let details = futures_util::future::join_all(detail_futs).await;
    anyhow::ensure!(
        details.iter().any(Option::is_some),
        "mempool: all transaction detail requests failed"
    );

    for detail_opt in details {
        let Some(detail) = detail_opt else {
            continue;
        };

        let exchange_net_sat = labelled_exchange_net_satoshis(&detail);
        let usd_value = exchange_net_sat * btc_price / 100_000_000.0;
        inflow += usd_value.max(0.0);
        outflow += (-usd_value).max(0.0);
    }

    Ok((inflow, outflow))
}

fn labelled_exchange_net_satoshis(detail: &Value) -> f64 {
    let labelled_value = |output: &Value| -> f64 {
        let address = output
            .get("scriptpubkey_address")
            .and_then(Value::as_str)
            .unwrap_or("");
        if EXCHANGE_IDENTIFIERS.contains(&address) {
            output
                .get("value")
                .and_then(parse_f64)
                .unwrap_or(0.0)
                .max(0.0)
        } else {
            0.0
        }
    };
    let outputs: f64 = detail
        .get("vout")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(labelled_value)
        .sum();
    let inputs: f64 = detail
        .get("vin")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|input| input.get("prevout"))
        .map(labelled_value)
        .sum();
    outputs - inputs
}

// ── Deribit Advanced Options (#4) ──────────────────────────────────────────

pub use super::deribit::DeribitAdvancedData;

/// Cached, single-expiry skew and max-pain proxy from validated real tickers.
/// Public OI does not reveal dealer positions, so signed GEX is unavailable.
pub async fn fetch_deribit_advanced() -> Result<DeribitAdvancedData> {
    with_fetch_circuit("deribit_advanced", super::deribit::fetch_advanced_options).await
}

#[cfg(test)]
mod tests {
    use super::{
        direction_agreement, fetch_circuit_allows, fetch_circuit_record_failure,
        fetch_circuit_record_success, fetch_circuits_guard, labelled_exchange_net_satoshis,
        parse_binance_candles, parse_futures_basis, parse_polymarket_flow,
    };
    use serde_json::json;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn malformed_candles_are_skipped_without_panicking_or_nan() {
        let payload = json!([
            [],
            [1, "100"],
            [1, "NaN", "101", "99", "100", "1"],
            [1, "100", "101", "99", "100", "1"],
            [1, "100", "99", "99", "100", "1"]
        ]);
        let candles = parse_binance_candles(&payload).unwrap();
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].close, 100.0);
    }

    #[test]
    fn no_price_change_is_neutral_not_direction_agreement() {
        assert_eq!(direction_agreement(1.0, 0.0), 0.0);
        assert_eq!(direction_agreement(0.0, 1.0), 0.0);
        assert_eq!(direction_agreement(1.0, -1.0), -1.0);
        assert_eq!(direction_agreement(-1.0, -1.0), 1.0);
    }

    #[test]
    fn public_flow_respects_transaction_side_market_and_time_order() {
        let payload = json!([
            {"conditionId":"market", "asset":"yes", "side":"SELL", "timestamp":990, "size":3, "price":0.6},
            {"conditionId":"market", "asset":"no", "side":"SELL", "timestamp":900, "size":5, "price":0.7},
            {"conditionId":"other", "asset":"yes", "side":"BUY", "timestamp":995, "size":1000, "price":0.9},
            {"conditionId":"market", "asset":"yes", "side":"BUY", "timestamp":1001, "size":1000, "price":0.9}
        ]);
        let data = parse_polymarket_flow(&payload, "market", "yes", 1000).unwrap();
        assert_eq!(data.yes_volume, 5.0);
        assert_eq!(data.no_volume, 3.0);
        assert!((data.price_start - 0.3).abs() < 1e-9);
        assert_eq!(data.price_now, 0.6);
        assert!(!data.large_fill_detected);
        assert!(
            parse_polymarket_flow(&json!({"error":"unauthorized"}), "market", "yes", 1000).is_err()
        );
    }

    #[test]
    fn unknown_whale_addresses_are_neutral_and_exchange_change_is_net() {
        let address = super::EXCHANGE_IDENTIFIERS[0];
        assert_eq!(
            labelled_exchange_net_satoshis(&json!({
                "vout":[{"scriptpubkey_address":"bc1qa-random-wallet", "value":1000000000}]
            })),
            0.0
        );
        assert_eq!(
            labelled_exchange_net_satoshis(&json!({
                "vin":[{"prevout":{"scriptpubkey_address":address, "value":1000}}],
                "vout":[{"scriptpubkey_address":address, "value":800},
                        {"scriptpubkey_address":"unknown", "value":190}]
            })),
            -200.0
        );
    }

    #[test]
    fn parse_futures_basis_extracts_pct_and_prices() {
        let payload = json!({
            "markPrice": "90500.0",
            "indexPrice": "90000.0"
        });
        let (basis_pct, mark_price, index_price) = parse_futures_basis(&payload);
        assert_eq!(mark_price, Some(90500.0));
        assert_eq!(index_price, Some(90000.0));
        assert!((basis_pct.unwrap_or_default() - 0.5555555555).abs() < 1e-6);
    }

    #[test]
    fn parse_futures_basis_handles_missing_prices() {
        let payload = json!({});
        let (basis_pct, mark_price, index_price) = parse_futures_basis(&payload);
        assert_eq!(basis_pct, None);
        assert_eq!(mark_price, None);
        assert_eq!(index_price, None);
    }

    #[test]
    fn fetch_circuit_opens_after_three_failures() {
        let source = "test_source";
        fetch_circuits_guard().remove(source);
        assert!(fetch_circuit_allows(source).is_ok());
        fetch_circuit_record_failure(source);
        fetch_circuit_record_failure(source);
        assert!(fetch_circuit_allows(source).is_ok());
        fetch_circuit_record_failure(source);
        assert!(fetch_circuit_allows(source).is_err());
        fetch_circuits_guard().remove(source);
    }

    #[test]
    fn fetch_circuit_success_resets_failure_state() {
        let source = "reset_source";
        fetch_circuits_guard().remove(source);
        fetch_circuit_record_failure(source);
        fetch_circuit_record_failure(source);
        fetch_circuit_record_success(source);
        assert!(fetch_circuit_allows(source).is_ok());
        fetch_circuits_guard().remove(source);
    }

    #[test]
    fn fetch_circuit_recovers_from_poison() {
        let source = "poison_source";
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = fetch_circuits_guard();
            panic!("intentional poison");
        }));

        assert!(fetch_circuit_allows(source).is_ok());
        fetch_circuit_record_failure(source);
        fetch_circuit_record_failure(source);
        fetch_circuit_record_failure(source);
        assert!(fetch_circuit_allows(source).is_err());
        fetch_circuits_guard().remove(source);
    }
}
