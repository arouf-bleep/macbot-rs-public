use anyhow::{anyhow, bail, ensure, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::json_util::parse_f64;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PcrData {
    pub direction: String,
    pub confidence: f64,
    pub pcr: f64,
}

/// Contrarian PCR heuristic. Missing/invalid denominators are unavailable, not neutral.
pub fn compute_deribit_pcr(put_oi: f64, call_oi: f64) -> Option<PcrData> {
    if !put_oi.is_finite() || put_oi < 0.0 || !call_oi.is_finite() || call_oi <= 0.0 {
        return None;
    }
    let pcr = put_oi / call_oi;
    if !pcr.is_finite() {
        return None;
    }
    let (direction, confidence) = if pcr > 1.2 {
        ("UP", (0.55 + (pcr - 1.2) * 0.10).min(0.75))
    } else if pcr < 0.7 {
        ("DOWN", (0.55 + (0.7 - pcr) * 0.10).min(0.75))
    } else {
        ("NEUTRAL", 0.5)
    };
    Some(PcrData {
        direction: direction.into(),
        confidence,
        pcr,
    })
}

#[derive(Debug, Clone, Default)]
pub struct PcrOpenInterest {
    pub put_oi: f64,
    pub call_oi: f64,
    pub provider_timestamp_ms: i64,
}

/// PCR's five-minute cache is measured from provider UTC time, not retrieval.
pub fn pcr_source_age_secs(timestamp_ms: i64, now_ms: i64) -> Option<u64> {
    (timestamp_ms > 0
        && now_ms.saturating_sub(timestamp_ms) <= 300_000
        && timestamp_ms.saturating_sub(now_ms) <= FUTURE_TOLERANCE_MS)
        .then(|| now_ms.saturating_sub(timestamp_ms).max(0) as u64 / 1000)
}

const DERIBIT_API: &str = "https://www.deribit.com/api/v2/public";
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);
const METADATA_TTL: Duration = Duration::from_secs(600);
const FETCH_DEADLINE: Duration = Duration::from_secs(8);
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);
const MAX_DATA_AGE_MS: i64 = 120_000;
const FUTURE_TOLERANCE_MS: i64 = 5_000;
const MIN_EXPIRY_MS: i64 = 86_400_000;
const MAX_EXPIRY_MS: i64 = 30 * MIN_EXPIRY_MS;
const MAX_INSTRUMENTS: usize = 4096;
const MAX_EXPIRY_INSTRUMENTS: usize = 256;
const MAX_TICKERS: usize = 12;

/// Scoped to a single expiry, not the whole options market. Public open interest
/// cannot identify dealer long/short inventory, so signed GEX is not reported.
#[derive(Debug, Clone)]
pub struct DeribitAdvancedData {
    /// Near-25-delta put/call IV skew; each measured delta is within 0.05 of target.
    pub skew_25d: f64,
    /// Strike minimizing OI-weighted USD intrinsic value, a research proxy.
    pub max_pain_price: f64,
    pub expiry_timestamp_ms: i64,
    pub oldest_source_timestamp_ms: i64,
    pub instruments_in_expiry: usize,
    pub sampled_tickers: usize,
}

impl DeribitAdvancedData {
    pub fn source_age_secs(&self) -> u64 {
        chrono::Utc::now()
            .timestamp_millis()
            .saturating_sub(self.oldest_source_timestamp_ms)
            .max(0) as u64
            / 1000
    }
    pub fn is_fresh(&self) -> bool {
        self.is_fresh_at(chrono::Utc::now().timestamp_millis())
    }
    fn is_fresh_at(&self, now_ms: i64) -> bool {
        source_timestamp_is_fresh(self.oldest_source_timestamp_ms, now_ms)
            && self.expiry_timestamp_ms.saturating_sub(now_ms) >= MIN_EXPIRY_MS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionSide {
    Call,
    Put,
}

#[derive(Debug, Clone)]
struct Instrument {
    name: String,
    strike: f64,
    expiry_ms: i64,
    side: OptionSide,
    is_open: bool,
}

#[derive(Debug, Clone)]
struct Summary {
    instrument: Instrument,
    oi_btc: f64,
    iv_pct: f64,
    underlying_price: f64,
    timestamp_ms: i64,
}

#[derive(Debug)]
struct Ticker {
    instrument: Instrument,
    delta: f64,
    iv_pct: f64,
    timestamp_ms: i64,
}

#[derive(Default)]
struct OptionsFeedCache {
    metadata: Option<(Instant, Vec<Instrument>)>,
    data: Option<(Instant, DeribitAdvancedData)>,
    last_attempt: Option<Instant>,
}

static OPTIONS_CACHE: Lazy<tokio::sync::Mutex<OptionsFeedCache>> =
    Lazy::new(|| tokio::sync::Mutex::new(OptionsFeedCache::default()));

fn result_array(response: &Value) -> Result<&Vec<Value>> {
    ensure!(
        response.get("error").is_none(),
        "Deribit returned an RPC error"
    );
    response
        .get("result")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Deribit response has no result array"))
}

fn source_timestamp_is_fresh(timestamp_ms: i64, now_ms: i64) -> bool {
    timestamp_ms > 0
        && now_ms.saturating_sub(timestamp_ms) <= MAX_DATA_AGE_MS
        && timestamp_ms.saturating_sub(now_ms) <= FUTURE_TOLERANCE_MS
}

pub(super) fn parse_pcr_summary(response: &Value, now_ms: i64) -> Result<PcrOpenInterest> {
    let items = result_array(response)?;
    ensure!(
        !items.is_empty() && items.len() <= MAX_INSTRUMENTS,
        "Deribit PCR chain is empty or oversized"
    );
    let (mut puts, mut calls) = (0.0, 0.0);
    let mut names = HashSet::new();
    let (mut n_puts, mut n_calls) = (0usize, 0usize);
    let mut provider_timestamp_ms = i64::MAX;
    for item in items {
        let name = item
            .get("instrument_name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Deribit PCR instrument name missing"))?;
        ensure!(
            name.starts_with("BTC-") && names.insert(name),
            "Deribit PCR instrument identity invalid or duplicated"
        );
        let oi = item
            .get("open_interest")
            .and_then(parse_f64)
            .filter(|v| *v >= 0.0)
            .ok_or_else(|| anyhow!("Deribit PCR open interest missing or invalid"))?;
        let timestamp_ms = item
            .get("creation_timestamp")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("Deribit PCR source timestamp missing"))?;
        ensure!(
            source_timestamp_is_fresh(timestamp_ms, now_ms),
            "Deribit PCR summary is stale or future-dated"
        );
        provider_timestamp_ms = provider_timestamp_ms.min(timestamp_ms);
        // Deribit options amount/OI is BTC, not USD and not a contract count.
        if name.ends_with("-P") {
            puts += oi;
            n_puts += 1;
        } else if name.ends_with("-C") {
            calls += oi;
            n_calls += 1;
        } else {
            bail!("Deribit PCR option side invalid");
        }
    }
    ensure!(
        n_puts > 0 && n_calls > 0 && compute_deribit_pcr(puts, calls).is_some(),
        "Deribit PCR requires both option sides and positive call open interest"
    );
    Ok(PcrOpenInterest {
        put_oi: puts,
        call_oi: calls,
        provider_timestamp_ms,
    })
}

fn parse_instruments(response: &Value, now_ms: i64) -> Result<Vec<Instrument>> {
    let items = result_array(response)?;
    ensure!(
        !items.is_empty() && items.len() <= MAX_INSTRUMENTS,
        "Deribit instrument chain is empty or oversized"
    );
    let mut instruments = Vec::new();
    let mut names = HashSet::new();
    for item in items {
        ensure!(
            item.get("kind").and_then(Value::as_str) == Some("option")
                && item.get("base_currency").and_then(Value::as_str) == Some("BTC")
                && item.get("settlement_currency").and_then(Value::as_str) == Some("BTC"),
            "Deribit instrument currency/kind unsupported"
        );
        let expiry_ms = item
            .get("expiration_timestamp")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("Deribit expiry missing"))?;
        if expiry_ms <= now_ms || item.get("is_active").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let name = item
            .get("instrument_name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Deribit instrument name missing"))?;
        ensure!(
            name.starts_with("BTC-") && names.insert(name.to_owned()),
            "Deribit instrument identity invalid or duplicated"
        );
        let strike = item
            .get("strike")
            .and_then(parse_f64)
            .filter(|v| *v > 0.0)
            .ok_or_else(|| anyhow!("Deribit strike invalid"))?;
        let side = match item.get("option_type").and_then(Value::as_str) {
            Some("call") => OptionSide::Call,
            Some("put") => OptionSide::Put,
            _ => bail!("Deribit option type missing"),
        };
        ensure!(
            name.ends_with(if side == OptionSide::Call { "-C" } else { "-P" }),
            "Deribit option identity and side disagree"
        );
        instruments.push(Instrument {
            name: name.into(),
            strike,
            expiry_ms,
            side,
            is_open: item.get("state").and_then(Value::as_str) == Some("open"),
        });
    }
    ensure!(
        !instruments.is_empty(),
        "Deribit has no active supported options"
    );
    Ok(instruments)
}

fn select_expiry(instruments: &[Instrument], now_ms: i64) -> Result<Vec<Instrument>> {
    let expiry = instruments
        .iter()
        .filter(|i| (MIN_EXPIRY_MS..=MAX_EXPIRY_MS).contains(&i.expiry_ms.saturating_sub(now_ms)))
        .map(|i| i.expiry_ms)
        .min()
        .ok_or_else(|| anyhow!("Deribit has no supported expiry between 1 and 30 days"))?;
    let chain: Vec<_> = instruments
        .iter()
        .filter(|i| i.expiry_ms == expiry)
        .cloned()
        .collect();
    ensure!(
        chain.len() <= MAX_EXPIRY_INSTRUMENTS
            && chain.iter().all(|i| i.is_open)
            && chain.iter().any(|i| i.side == OptionSide::Call)
            && chain.iter().any(|i| i.side == OptionSide::Put),
        "Deribit selected expiry is oversized, not open, or missing an option side"
    );
    Ok(chain)
}

fn validate_summary_metadata(response: &Value, metadata: &[Instrument]) -> Result<()> {
    let items = result_array(response)?;
    ensure!(
        items.len() <= MAX_INSTRUMENTS,
        "Deribit summary chain oversized"
    );
    let known: HashSet<_> = metadata.iter().map(|i| i.name.as_str()).collect();
    ensure!(
        items.iter().all(|item| item
            .get("instrument_name")
            .and_then(Value::as_str)
            .is_some_and(|name| known.contains(name))),
        "Deribit summary contains new or unsupported instruments; metadata refresh required"
    );
    Ok(())
}

fn parse_summaries(response: &Value, chain: &[Instrument], now_ms: i64) -> Result<Vec<Summary>> {
    let items = result_array(response)?;
    ensure!(
        items.len() <= MAX_INSTRUMENTS,
        "Deribit summary chain oversized"
    );
    let mut by_name = HashMap::new();
    for item in items {
        let name = item
            .get("instrument_name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Deribit summary instrument missing"))?;
        ensure!(
            by_name.insert(name, item).is_none(),
            "Deribit summary instrument duplicated"
        );
    }
    let mut summaries = Vec::with_capacity(chain.len());
    for instrument in chain {
        let item = by_name
            .get(instrument.name.as_str())
            .ok_or_else(|| anyhow!("Deribit expiry summary is incomplete"))?;
        let oi_btc = item
            .get("open_interest")
            .and_then(parse_f64)
            .filter(|v| *v >= 0.0)
            .ok_or_else(|| anyhow!("Deribit open interest missing or invalid"))?;
        let iv_pct = item
            .get("mark_iv")
            .and_then(parse_f64)
            .filter(|v| *v > 0.0 && *v <= 1000.0)
            .ok_or_else(|| anyhow!("Deribit mark IV missing or outside supported range"))?;
        let underlying_price = item
            .get("underlying_price")
            .and_then(parse_f64)
            .filter(|v| *v > 0.0)
            .ok_or_else(|| anyhow!("Deribit underlying price missing"))?;
        let timestamp_ms = item
            .get("creation_timestamp")
            .and_then(Value::as_i64)
            .ok_or_else(|| anyhow!("Deribit summary timestamp missing"))?;
        ensure!(
            source_timestamp_is_fresh(timestamp_ms, now_ms),
            "Deribit expiry summary stale or future-dated"
        );
        summaries.push(Summary {
            instrument: instrument.clone(),
            oi_btc,
            iv_pct,
            underlying_price,
            timestamp_ms,
        });
    }
    ensure!(
        summaries.iter().map(|s| s.oi_btc).sum::<f64>().is_finite(),
        "Deribit expiry open interest overflow"
    );
    ensure!(
        summaries
            .iter()
            .any(|s| s.instrument.side == OptionSide::Call && s.oi_btc > 0.0)
            && summaries
                .iter()
                .any(|s| s.instrument.side == OptionSide::Put && s.oi_btc > 0.0),
        "Deribit expiry has no positive open interest on both sides"
    );
    let spot = summaries[0].underlying_price;
    ensure!(
        summaries
            .iter()
            .all(|s| (s.underlying_price / spot - 1.0).abs() <= 0.01),
        "Deribit underlying prices disagree within expiry"
    );
    Ok(summaries)
}

/// A Black-Scholes strike estimate only ranks candidate requests. No estimated
/// Greeks enter the returned analytics; measured ticker deltas must pass below.
fn select_ticker_candidates(summaries: &[Summary], now_ms: i64) -> Vec<Instrument> {
    let mut selected = Vec::new();
    let years = (summaries[0].instrument.expiry_ms - now_ms) as f64 / (365.25 * 86_400_000.0);
    for (side, delta_quantile) in [
        (OptionSide::Call, 0.67448975),
        (OptionSide::Put, -0.67448975),
        (OptionSide::Call, 0.0),
        (OptionSide::Put, 0.0),
    ] {
        let mut ranked: Vec<_> = summaries
            .iter()
            .filter(|s| s.instrument.side == side && s.oi_btc > 0.0)
            .map(|s| {
                let sigma = s.iv_pct / 100.0;
                let target_log_strike = s.underlying_price.ln()
                    + 0.5 * sigma * sigma * years
                    + delta_quantile * sigma * years.sqrt();
                (
                    (s.instrument.strike.ln() - target_log_strike).abs(),
                    &s.instrument,
                )
            })
            .collect();
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.name.cmp(&b.1.name)));
        for (_, instrument) in ranked.into_iter().take(3) {
            if !selected
                .iter()
                .any(|i: &Instrument| i.name == instrument.name)
            {
                selected.push(instrument.clone());
            }
        }
    }
    selected
}

fn parse_ticker(
    response: &Value,
    instrument: &Instrument,
    now_ms: i64,
    spot: f64,
) -> Result<Ticker> {
    ensure!(response.get("error").is_none(), "Deribit ticker RPC error");
    let item = response
        .get("result")
        .ok_or_else(|| anyhow!("Deribit ticker missing result"))?;
    ensure!(
        item.get("instrument_name").and_then(Value::as_str) == Some(instrument.name.as_str())
            && item.get("state").and_then(Value::as_str) == Some("open"),
        "Deribit ticker identity/state invalid"
    );
    let timestamp_ms = item
        .get("timestamp")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("Deribit ticker timestamp missing"))?;
    ensure!(
        source_timestamp_is_fresh(timestamp_ms, now_ms),
        "Deribit ticker stale or future-dated"
    );
    let delta = item
        .pointer("/greeks/delta")
        .and_then(parse_f64)
        .ok_or_else(|| anyhow!("Deribit ticker delta missing"))?;
    ensure!(
        if instrument.side == OptionSide::Call {
            (0.0..=1.0).contains(&delta)
        } else {
            (-1.0..=0.0).contains(&delta)
        },
        "Deribit ticker delta sign/range invalid"
    );
    let iv_pct = item
        .get("mark_iv")
        .and_then(parse_f64)
        .filter(|v| *v > 0.0 && *v <= 1000.0)
        .ok_or_else(|| anyhow!("Deribit ticker mark IV missing or unsupported"))?;
    let underlying = item
        .get("underlying_price")
        .and_then(parse_f64)
        .filter(|v| *v > 0.0)
        .ok_or_else(|| anyhow!("Deribit ticker underlying missing"))?;
    ensure!(
        (underlying / spot - 1.0).abs() <= 0.01,
        "Deribit ticker underlying diverged from summary"
    );
    Ok(Ticker {
        instrument: instrument.clone(),
        delta,
        iv_pct,
        timestamp_ms,
    })
}

fn nearest_measured_iv(tickers: &[Ticker], side: OptionSide, target: f64) -> Result<f64> {
    tickers
        .iter()
        .filter(|t| t.instrument.side == side && (t.delta.abs() - target).abs() <= 0.05)
        .min_by(|a, b| {
            (a.delta.abs() - target)
                .abs()
                .total_cmp(&(b.delta.abs() - target).abs())
                .then_with(|| a.instrument.name.cmp(&b.instrument.name))
        })
        .map(|t| t.iv_pct)
        .ok_or_else(|| {
            anyhow!("Deribit sampled tickers do not contain required measured delta region")
        })
}

fn compute_advanced(
    summaries: &[Summary],
    tickers: &[Ticker],
    now_ms: i64,
) -> Result<DeribitAdvancedData> {
    let call_iv = nearest_measured_iv(tickers, OptionSide::Call, 0.25)?;
    let put_iv = nearest_measured_iv(tickers, OptionSide::Put, 0.25)?;
    let atm_iv = (nearest_measured_iv(tickers, OptionSide::Call, 0.5)?
        + nearest_measured_iv(tickers, OptionSide::Put, 0.5)?)
        / 2.0;
    let skew_25d = (put_iv - call_iv) / atm_iv;
    let spot = summaries[0].underlying_price;
    let mut strikes: Vec<_> = summaries.iter().map(|s| s.instrument.strike).collect();
    strikes.sort_by(f64::total_cmp);
    strikes.dedup();
    let payouts: Vec<_> = strikes
        .into_iter()
        .map(|price| {
            // OI is already base-currency BTC; multiplying contract_size again
            // would incorrectly change the weights. This is USD intrinsic, not
            // the inverse option's coin-denominated settlement or dealer inventory.
            let pain: f64 = summaries
                .iter()
                .map(|s| match s.instrument.side {
                    OptionSide::Call => (price - s.instrument.strike).max(0.0) * s.oi_btc,
                    OptionSide::Put => (s.instrument.strike - price).max(0.0) * s.oi_btc,
                })
                .sum();
            (pain, (price - spot).abs(), price)
        })
        .collect();
    ensure!(
        payouts.iter().all(|p| p.0.is_finite()),
        "Deribit intrinsic payout overflow"
    );
    let max_pain_price = payouts
        .into_iter()
        .min_by(|a, b| {
            a.0.total_cmp(&b.0)
                .then_with(|| a.1.total_cmp(&b.1))
                .then_with(|| a.2.total_cmp(&b.2))
        })
        .map(|(_, _, p)| p)
        .ok_or_else(|| anyhow!("Deribit no expiry strikes"))?;
    ensure!(
        skew_25d.is_finite() && max_pain_price.is_finite(),
        "Deribit analytics non-finite"
    );
    let oldest_source_timestamp_ms = summaries
        .iter()
        .map(|s| s.timestamp_ms)
        .chain(tickers.iter().map(|t| t.timestamp_ms))
        .min()
        .ok_or_else(|| anyhow!("Deribit no source timestamp"))?;
    let data = DeribitAdvancedData {
        skew_25d,
        max_pain_price,
        expiry_timestamp_ms: summaries[0].instrument.expiry_ms,
        oldest_source_timestamp_ms,
        instruments_in_expiry: summaries.len(),
        sampled_tickers: tickers.len(),
    };
    ensure!(
        data.is_fresh_at(now_ms),
        "Deribit options expired or became stale during collection"
    );
    Ok(data)
}

async fn request_json(
    client: &reqwest::Client,
    base: &str,
    method: &str,
    parameters: &[(&str, &str)],
    timeout: Duration,
) -> Result<Value> {
    let response = client
        .get(format!("{base}/{method}"))
        .query(parameters)
        .timeout(timeout)
        .send()
        .await?
        .error_for_status()?;
    crate::http_client::read_json_limited(response, 4 * 1024 * 1024).await
}

async fn fetch_with_cache(
    cache: &tokio::sync::Mutex<OptionsFeedCache>,
    client: &reqwest::Client,
    base: &str,
    deadline: Duration,
    request_timeout: Duration,
) -> Result<DeribitAdvancedData> {
    tokio::time::timeout(deadline, async {
        let mut cache = cache.lock().await;
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let Some((fetched, data)) = &cache.data {
            if fetched.elapsed() < REFRESH_INTERVAL && data.is_fresh_at(now_ms) {
                return Ok(data.clone());
            }
        }
        cache.data = None;
        if cache
            .last_attempt
            .is_some_and(|attempt| attempt.elapsed() < REFRESH_INTERVAL)
        {
            bail!("Deribit options unavailable; refresh is rate limited");
        }
        cache.last_attempt = Some(Instant::now());
        let metadata_fresh = cache
            .metadata
            .as_ref()
            .is_some_and(|(ts, _)| ts.elapsed() < METADATA_TTL);
        if !metadata_fresh {
            let response = request_json(
                client,
                base,
                "get_instruments",
                &[
                    ("currency", "BTC"),
                    ("kind", "option"),
                    ("expired", "false"),
                ],
                request_timeout,
            )
            .await?;
            let instruments = parse_instruments(&response, chrono::Utc::now().timestamp_millis())?;
            cache.metadata = Some((Instant::now(), instruments));
        }
        let chain = select_expiry(
            &cache.metadata.as_ref().expect("metadata populated").1,
            now_ms,
        )?;
        let response = request_json(
            client,
            base,
            "get_book_summary_by_currency",
            &[("currency", "BTC"), ("kind", "option")],
            request_timeout,
        )
        .await?;
        if let Err(error) = validate_summary_metadata(
            &response,
            &cache.metadata.as_ref().expect("metadata populated").1,
        ) {
            cache.metadata = None;
            return Err(error);
        }
        let summaries = parse_summaries(&response, &chain, chrono::Utc::now().timestamp_millis())?;
        let candidates =
            select_ticker_candidates(&summaries, chrono::Utc::now().timestamp_millis());
        ensure!(
            !candidates.is_empty() && candidates.len() <= MAX_TICKERS,
            "Deribit ticker request limit exceeded"
        );
        let spot = summaries[0].underlying_price;
        let tickers: Vec<Ticker> = stream::iter(candidates)
            .map(|instrument| async move {
                let response = request_json(
                    client,
                    base,
                    "ticker",
                    &[("instrument_name", instrument.name.as_str())],
                    request_timeout,
                )
                .await?;
                parse_ticker(
                    &response,
                    &instrument,
                    chrono::Utc::now().timestamp_millis(),
                    spot,
                )
            })
            .buffer_unordered(3)
            .try_collect()
            .await?;
        let data = compute_advanced(&summaries, &tickers, chrono::Utc::now().timestamp_millis())?;
        cache.data = Some((Instant::now(), data.clone()));
        Ok(data)
    })
    .await
    .map_err(|_| anyhow!("Deribit options collection exceeded its time limit"))?
}

/// Official methods and units: https://docs.deribit.com/api-reference/market-data/public-ticker
/// and https://docs.deribit.com/articles/options-data-collection-best-practices.
/// At most one metadata call, one summary call and 12 ticker calls per 60s;
/// the metadata is reused for 10min. Total wall time is bounded, including cache waits.
pub(super) async fn fetch_advanced_options() -> Result<DeribitAdvancedData> {
    fetch_with_cache(
        &OPTIONS_CACHE,
        &crate::http_client::HTTP,
        DERIBIT_API,
        FETCH_DEADLINE,
        REQUEST_DEADLINE,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    const NOW: i64 = 1_800_000_000_000;

    fn fixture(now_ms: i64) -> (Value, Value, HashMap<String, Value>) {
        let mut instruments = Vec::new();
        let mut summaries = Vec::new();
        let mut tickers = HashMap::new();
        for (strike, side, delta, iv) in [
            (90_000.0, "put", -0.25, 60.0),
            (100_000.0, "put", -0.5, 50.0),
            (100_000.0, "call", 0.5, 50.0),
            (110_000.0, "call", 0.25, 40.0),
        ] {
            let suffix = if side == "call" { "C" } else { "P" };
            let name = format!("BTC-TEST-{}-{suffix}", strike as i64);
            instruments.push(json!({"instrument_name": name, "kind":"option", "base_currency":"BTC", "settlement_currency":"BTC", "state":"open", "is_active":true, "expiration_timestamp": now_ms + 7 * MIN_EXPIRY_MS, "strike":strike, "option_type":side, "contract_size":0.1}));
            summaries.push(json!({"instrument_name":name, "open_interest":2.0,"mark_iv":iv,"underlying_price":100_000.0,"creation_timestamp":now_ms}));
            tickers.insert(name.clone(), json!({"result":{"instrument_name":name,"state":"open","timestamp":now_ms,"greeks":{"delta":delta},"mark_iv":iv,"underlying_price":100_000.0}}));
        }
        (
            json!({"result":instruments}),
            json!({"result":summaries}),
            tickers,
        )
    }

    fn parsed_fixture() -> (Vec<Summary>, Vec<Ticker>) {
        let (instruments, summaries, tickers) = fixture(NOW);
        let chain = select_expiry(&parse_instruments(&instruments, NOW).unwrap(), NOW).unwrap();
        let summaries = parse_summaries(&summaries, &chain, NOW).unwrap();
        let tickers = chain
            .iter()
            .map(|i| parse_ticker(&tickers[&i.name], i, NOW, 100_000.0).unwrap())
            .collect();
        (summaries, tickers)
    }

    #[test]
    fn measured_same_expiry_skew_and_btc_oi_proxy_are_computed() {
        let (summaries, tickers) = parsed_fixture();
        let data = compute_advanced(&summaries, &tickers, NOW).unwrap();
        assert!((data.skew_25d - 0.4).abs() < 1e-12);
        assert_eq!(data.max_pain_price, 100_000.0);
        assert_eq!(data.instruments_in_expiry, 4);
        assert_eq!(data.sampled_tickers, 4);
        assert!(data.is_fresh_at(NOW));
        assert!(!data.is_fresh_at(NOW + MAX_DATA_AGE_MS + 1));
        // A contract_size of 0.1 is metadata, not another OI multiplier.
        assert_eq!(summaries.iter().map(|s| s.oi_btc).sum::<f64>(), 8.0);
    }

    #[test]
    fn missing_invalid_and_duplicate_summary_values_never_become_zero() {
        let (instruments, valid, _) = fixture(NOW);
        let chain = select_expiry(&parse_instruments(&instruments, NOW).unwrap(), NOW).unwrap();
        for field in [
            "open_interest",
            "mark_iv",
            "underlying_price",
            "creation_timestamp",
        ] {
            let mut response = valid.clone();
            response["result"][0].as_object_mut().unwrap().remove(field);
            assert!(parse_summaries(&response, &chain, NOW).is_err(), "{field}");
        }
        for (field, value) in [
            ("open_interest", json!(-1)),
            ("mark_iv", json!("NaN")),
            ("underlying_price", json!(0)),
            ("creation_timestamp", json!(NOW - MAX_DATA_AGE_MS - 1)),
            ("creation_timestamp", json!(NOW + FUTURE_TOLERANCE_MS + 1)),
        ] {
            let mut response = valid.clone();
            response["result"][0][field] = value;
            assert!(parse_summaries(&response, &chain, NOW).is_err(), "{field}");
        }
        let mut incomplete = valid.clone();
        incomplete["result"].as_array_mut().unwrap().pop();
        assert!(parse_summaries(&incomplete, &chain, NOW).is_err());
        let mut duplicate = valid.clone();
        let item = duplicate["result"][0].clone();
        duplicate["result"].as_array_mut().unwrap().push(item);
        assert!(parse_summaries(&duplicate, &chain, NOW).is_err());
        assert!(parse_summaries(
            &json!({"error":{"code":10028},"result":valid["result"]}),
            &chain,
            NOW
        )
        .is_err());
    }

    #[test]
    fn newly_listed_summary_instrument_requires_complete_metadata_refresh() {
        let (metadata, mut summary, _) = fixture(NOW);
        let metadata = parse_instruments(&metadata, NOW).unwrap();
        assert!(validate_summary_metadata(&summary, &metadata).is_ok());
        let mut added = summary["result"][0].clone();
        added["instrument_name"] = json!("BTC-TEST-91000-P");
        summary["result"].as_array_mut().unwrap().push(added);
        assert!(validate_summary_metadata(&summary, &metadata).is_err());
    }

    #[test]
    fn overflowing_oi_and_intrinsic_values_remain_unavailable() {
        let (metadata, mut summary, _) = fixture(NOW);
        let chain = select_expiry(&parse_instruments(&metadata, NOW).unwrap(), NOW).unwrap();
        for item in summary["result"].as_array_mut().unwrap() {
            item["open_interest"] = json!(f64::MAX);
        }
        assert!(parse_summaries(&summary, &chain, NOW).is_err());
        let (mut summaries, tickers) = parsed_fixture();
        summaries[1].oi_btc = f64::MAX / 2.0;
        assert!(compute_advanced(&summaries, &tickers, NOW).is_err());
    }

    #[test]
    fn ticker_identity_greeks_state_and_provider_time_are_required() {
        let (metadata, _, tickers) = fixture(NOW);
        let instrument = &parse_instruments(&metadata, NOW).unwrap()[0];
        let valid = &tickers[&instrument.name];
        for field in ["greeks", "timestamp", "mark_iv", "underlying_price"] {
            let mut response = valid.clone();
            response["result"].as_object_mut().unwrap().remove(field);
            assert!(
                parse_ticker(&response, instrument, NOW, 100_000.0).is_err(),
                "{field}"
            );
        }
        for (field, value) in [
            ("instrument_name", json!("BTC-WRONG-P")),
            ("state", json!("halted")),
            ("timestamp", json!(NOW - MAX_DATA_AGE_MS - 1)),
            ("timestamp", json!(NOW + FUTURE_TOLERANCE_MS + 1)),
            ("underlying_price", json!(120_000.0)),
        ] {
            let mut response = valid.clone();
            response["result"][field] = value;
            assert!(
                parse_ticker(&response, instrument, NOW, 100_000.0).is_err(),
                "{field}"
            );
        }
        let mut wrong_delta = valid.clone();
        wrong_delta["result"]["greeks"]["delta"] = json!(0.25);
        assert!(parse_ticker(&wrong_delta, instrument, NOW, 100_000.0).is_err());
    }

    #[test]
    fn expiry_and_currency_selection_never_mix_chains_or_settlement_states() {
        let (mut metadata, _, _) = fixture(NOW);
        let mut near = metadata["result"][0].clone();
        near["instrument_name"] = json!("BTC-NEAR-90000-P");
        near["expiration_timestamp"] = json!(NOW + 1_800_000);
        metadata["result"].as_array_mut().unwrap().push(near);
        let mut halted = metadata["result"][0].clone();
        halted["instrument_name"] = json!("BTC-HALT-90000-P");
        halted["state"] = json!("halted");
        halted["expiration_timestamp"] = json!(NOW + 35 * MIN_EXPIRY_MS);
        metadata["result"].as_array_mut().unwrap().push(halted);
        let instruments = parse_instruments(&metadata, NOW).unwrap();
        let chain = select_expiry(&instruments, NOW).unwrap();
        assert_eq!(chain.len(), 4);
        assert!(chain.iter().all(|i| i.expiry_ms == NOW + 7 * MIN_EXPIRY_MS));
        assert!(select_expiry(&instruments, NOW + 8 * MIN_EXPIRY_MS).is_err());
        metadata["result"][0]["state"] = json!("halted");
        assert!(select_expiry(&parse_instruments(&metadata, NOW).unwrap(), NOW).is_err());
        metadata["result"][0]["settlement_currency"] = json!("USDC");
        assert!(parse_instruments(&metadata, NOW).is_err());
    }

    #[test]
    fn incomplete_delta_regions_and_stale_collection_remain_unavailable() {
        let (summaries, mut tickers) = parsed_fixture();
        tickers.retain(|t| !(t.instrument.side == OptionSide::Put && t.delta == -0.25));
        assert!(compute_advanced(&summaries, &tickers, NOW).is_err());
        let (summaries, tickers) = parsed_fixture();
        assert!(compute_advanced(&summaries, &tickers, NOW + MAX_DATA_AGE_MS + 1).is_err());
    }

    #[test]
    fn candidate_requests_are_bounded_and_use_only_one_expiry() {
        let (mut summaries, _) = parsed_fixture();
        let template = summaries[0].clone();
        for n in 0..200 {
            let mut item = template.clone();
            item.instrument.name = format!("BTC-TEST-{}-P", 90_001 + n);
            item.instrument.strike = 90_001.0 + n as f64;
            summaries.push(item);
        }
        let candidates = select_ticker_candidates(&summaries, NOW);
        assert!(candidates.len() <= MAX_TICKERS);
        assert_eq!(
            candidates
                .iter()
                .map(|i| &i.name)
                .collect::<HashSet<_>>()
                .len(),
            candidates.len()
        );
    }

    #[test]
    fn pcr_invalid_inputs_and_missing_feed_never_manufacture_neutral() {
        for (puts, calls) in [
            (0.0, 0.0),
            (-1.0, 1.0),
            (1.0, -1.0),
            (f64::NAN, 1.0),
            (1.0, f64::INFINITY),
            (f64::MAX, f64::MIN_POSITIVE),
        ] {
            assert!(compute_deribit_pcr(puts, calls).is_none());
        }
        assert_eq!(
            compute_deribit_pcr(10.0, 10.0).unwrap().direction,
            "NEUTRAL"
        );
        let (_, valid, _) = fixture(NOW);
        let pcr = parse_pcr_summary(&valid, NOW).unwrap();
        assert_eq!(
            (pcr.put_oi, pcr.call_oi, pcr.provider_timestamp_ms),
            (4.0, 4.0, NOW)
        );
        assert_eq!(pcr_source_age_secs(NOW, NOW + 300_000), Some(300));
        assert_eq!(pcr_source_age_secs(NOW, NOW + 300_001), None);
        assert_eq!(
            pcr_source_age_secs(NOW + FUTURE_TOLERANCE_MS + 1, NOW),
            None
        );
        assert!(parse_pcr_summary(&json!({"result":[]}), NOW).is_err());
        let mut missing = valid.clone();
        missing["result"][0]
            .as_object_mut()
            .unwrap()
            .remove("open_interest");
        assert!(parse_pcr_summary(&missing, NOW).is_err());
        assert!(parse_pcr_summary(&valid, NOW + MAX_DATA_AGE_MS + 1).is_err());
    }

    #[derive(Clone)]
    struct ServerFixture {
        metadata: Value,
        summaries: Value,
        tickers: HashMap<String, Value>,
        requests: Arc<AtomicUsize>,
        delay: Duration,
    }

    async fn fixture_server(
        delay: Duration,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        use axum::{
            extract::{Path, Query, State},
            Json, Router,
        };
        async fn serve(
            State(f): State<ServerFixture>,
            Path(method): Path<String>,
            Query(q): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            f.requests.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(f.delay).await;
            Json(match method.as_str() {
                "get_instruments" => f.metadata,
                "get_book_summary_by_currency" => f.summaries,
                "ticker" => f.tickers[&q["instrument_name"]].clone(),
                _ => panic!("unexpected provider request"),
            })
        }
        let (metadata, summaries, tickers) = fixture(chrono::Utc::now().timestamp_millis());
        let requests = Arc::new(AtomicUsize::new(0));
        let f = ServerFixture {
            metadata,
            summaries,
            tickers,
            requests: requests.clone(),
            delay,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/:method", axum::routing::get(serve))
                    .with_state(f),
            )
            .await
            .unwrap()
        });
        (format!("http://{address}"), requests, task)
    }

    #[tokio::test]
    async fn real_http_fixture_is_cached_and_expired_source_is_not_reused() {
        let (base, requests, server) = fixture_server(Duration::ZERO).await;
        let cache = tokio::sync::Mutex::new(OptionsFeedCache::default());
        let first = fetch_with_cache(
            &cache,
            &crate::http_client::HTTP,
            &base,
            Duration::from_secs(2),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 6);
        let second = fetch_with_cache(
            &cache,
            &crate::http_client::HTTP,
            &base,
            Duration::from_secs(2),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(second.skew_25d, first.skew_25d);
        assert_eq!(requests.load(Ordering::SeqCst), 6);
        cache
            .lock()
            .await
            .data
            .as_mut()
            .unwrap()
            .1
            .oldest_source_timestamp_ms -= MAX_DATA_AGE_MS + 1;
        assert!(fetch_with_cache(
            &cache,
            &crate::http_client::HTTP,
            &base,
            Duration::from_secs(2),
            Duration::from_secs(1)
        )
        .await
        .is_err());
        assert!(cache.lock().await.data.is_none());
        assert_eq!(
            requests.load(Ordering::SeqCst),
            6,
            "stale cache must not trigger an unbounded immediate refresh"
        );
        // Simulate the refresh interval elapsing without waiting in real time.
        cache.lock().await.last_attempt = Some(Instant::now() - REFRESH_INTERVAL);
        fetch_with_cache(
            &cache,
            &crate::http_client::HTTP,
            &base,
            Duration::from_secs(2),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            11,
            "metadata should be reused"
        );
        server.abort();
    }

    #[tokio::test]
    async fn collection_and_each_request_have_bounded_time_and_no_fallback() {
        let (base, requests, server) = fixture_server(Duration::from_secs(1)).await;
        let cache = tokio::sync::Mutex::new(OptionsFeedCache::default());
        let start = Instant::now();
        let err = fetch_with_cache(
            &cache,
            &crate::http_client::HTTP,
            &base,
            Duration::from_millis(40),
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("time limit"));
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(cache.lock().await.data.is_none());
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(fetch_with_cache(
            &cache,
            &crate::http_client::HTTP,
            &base,
            Duration::from_millis(40),
            Duration::from_secs(2)
        )
        .await
        .is_err());
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "failed requests should be rate limited"
        );
        let other_cache = tokio::sync::Mutex::new(OptionsFeedCache::default());
        let err = fetch_with_cache(
            &other_cache,
            &crate::http_client::HTTP,
            &base,
            Duration::from_secs(1),
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();
        assert!(err.downcast_ref::<reqwest::Error>().unwrap().is_timeout());
        assert!(other_cache.lock().await.data.is_none());
        server.abort();
    }
}
