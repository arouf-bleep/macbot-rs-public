//! Market analysis — fetches all signals concurrently, fuses them, and
//! returns a rich JSON snapshot consumed by the trading loop and API.

use crate::http_client::JsonResponseExt;
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::America::New_York;
use serde_json::{json, Value};
use tracing::warn;

use crate::http_client::HTTP;

use crate::analysis::{AnalysisMarketData, AnalysisResult as ParsedAnalysis};
use crate::btc::BTCAnalyzer;
use crate::domain::Signal;
use crate::fusion::{apply_time_of_day_weighting, fuse_signals, EnrichmentData, FusionResult};
use crate::signals::{
    self, compute_aggressor_ratio, compute_btc_final_momentum, compute_day_of_week,
    compute_deribit_pcr, compute_distance_from_round, compute_fear_greed, compute_funding_signal,
    compute_ha, compute_hawkes_flow, compute_hour_of_day, compute_lunarcrush_sentiment,
    compute_macd, compute_microstructure, compute_oi_momentum, compute_options_advanced,
    compute_orderbook_imbalance, compute_phase_decay, compute_polymarket_flow,
    compute_price_divergence, compute_recent_volatility, compute_regime, compute_rsi, compute_vwap,
    compute_whale_signal,
    fetchers::{
        fetch_binance_candles, fetch_binance_candles_1m, fetch_binance_depth,
        fetch_binance_futures, fetch_deribit_pcr, fetch_fear_greed,
    },
    Candle,
};

use super::state::{CandlesCache, FgCache, FuturesCache, MlCandlesCache, PcrCache, TradingBot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalFreshness {
    Fresh,
    Fallback,
    Missing,
}

impl SignalFreshness {
    fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Fallback => "fallback",
            Self::Missing => "missing",
        }
    }
}

#[derive(Debug, Clone)]
struct CachedSignal<T> {
    value: T,
    freshness: SignalFreshness,
    age_secs: Option<u64>,
    detail: Option<String>,
}

impl<T> CachedSignal<T> {
    fn fresh(value: T, age_secs: Option<u64>, detail: impl Into<String>) -> Self {
        Self {
            value,
            freshness: SignalFreshness::Fresh,
            age_secs,
            detail: Some(detail.into()),
        }
    }

    fn fallback(value: T, age_secs: Option<u64>, detail: impl Into<String>) -> Self {
        Self {
            value,
            freshness: SignalFreshness::Fallback,
            age_secs,
            detail: Some(detail.into()),
        }
    }

    fn missing(value: T, detail: impl Into<String>) -> Self {
        Self {
            value,
            freshness: SignalFreshness::Missing,
            age_secs: None,
            detail: Some(detail.into()),
        }
    }

    fn status_json(&self) -> serde_json::Value {
        serde_json::json!({
            "freshness": self.freshness.as_str(),
            "age_secs": self.age_secs,
            "detail": self.detail,
        })
    }
}

fn refresh_pcr_freshness(
    signal: &mut CachedSignal<crate::signals::fetchers::PcrOpenInterest>,
    now_ms: i64,
) {
    let timestamp = signal.value.provider_timestamp_ms;
    if timestamp > 0 {
        signal.age_secs = Some(now_ms.saturating_sub(timestamp).max(0) as u64 / 1000);
    }
    if signal.freshness == SignalFreshness::Fresh
        && crate::signals::deribit::pcr_source_age_secs(timestamp, now_ms).is_none()
    {
        signal.freshness = SignalFreshness::Fallback;
        signal.detail = Some("PCR provider time became stale or invalid during analysis".into());
    }
}

// ── Gamma API market data ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MarketData {
    pub market_id: String,
    pub question: String,
    pub price: f64,
    pub volume: f64,
    pub liquidity: f64,
    pub start_date: String,
    pub end_date: String,
    pub window_mins: f64,
    pub source: String,
    /// BTC reference price parsed from the market description — the exact
    /// "price to beat" set by Polymarket's oracle when the market opened.
    pub btc_ref_price: Option<f64>,
}

fn posterior_signal_from_p_up(p_up: f64, market_price: f64) -> Signal {
    if p_up > market_price {
        Signal::BuyYes
    } else if p_up < market_price {
        Signal::BuyNo
    } else {
        Signal::Hold
    }
}

fn realign_signal_with_posterior(
    result: &mut FusionResult,
    market_price: f64,
    stage: &str,
) -> bool {
    if result.signal == Signal::Hold {
        return false;
    }
    let aligned = posterior_signal_from_p_up(result.p_up, market_price);
    if aligned == result.signal {
        return false;
    }

    let old_signal = result.signal;
    result.signal = aligned;
    if result.reason.is_empty() {
        result.reason = format!(
            "posterior realigned after {stage}: {} -> {}",
            old_signal.as_str(),
            result.signal.as_str()
        );
    } else if !result.reason.contains("posterior realigned") {
        result.reason = format!(
            "{} | posterior realigned after {}: {} -> {}",
            result.reason,
            stage,
            old_signal.as_str(),
            result.signal.as_str()
        );
    }
    true
}

fn refresh_posterior_economics(
    result: &mut FusionResult,
    market_price: f64,
    amount: f64,
    liquidity: f64,
    minimum: f64,
) {
    if result.signal == Signal::Hold {
        return;
    }
    let economics = crate::fusion::entry_economics(
        result.p_up,
        market_price,
        result.signal == Signal::BuyYes,
        amount,
        liquidity,
    );
    if let Some((net, fee, impact)) = economics {
        result.net_ev = net;
        result.taker_fee = fee;
        result.lmsr_slippage = impact;
        if net >= minimum {
            return;
        }
        result.reason = format!(
            "Posterior entry return {:.3}% < minimum {:.3}% after costs",
            net * 100.0,
            minimum * 100.0
        );
    } else {
        result.net_ev = 0.0;
        result.reason = "Invalid posterior entry economics — waiting".into();
    }
    result.signal = Signal::Hold;
}

fn historical_candle_open(value: &Value, start_ms: i64) -> Option<f64> {
    let row = value.as_array()?.first()?.as_array()?;
    if row.first()?.as_i64()? != start_ms {
        return None;
    }
    crate::json_util::parse_f64(row.get(1)?).filter(|p| *p > 0.0)
}

/// Extract a BTC-range dollar price from free-form text.
/// Matches patterns like "$67,219.42" or "$67219.42".
fn parse_btc_price_from_text(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            i += 1;
            let start = i;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit() || bytes[i] == b',' || bytes[i] == b'.')
            {
                i += 1;
            }
            if i > start {
                let s: String = text[start..i].chars().filter(|c| *c != ',').collect();
                if let Ok(p) = s.parse::<f64>() {
                    // Sanity-check: reasonable BTC price range
                    if p > 5_000.0 && p < 10_000_000.0 {
                        return Some(p);
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const CLOB_BASE: &str = "https://clob.polymarket.com";

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse an ISO-8601 string into a UTC DateTime.  Returns None on failure.
pub(crate) fn parse_utc(s: &str) -> Option<DateTime<Utc>> {
    // Try RFC3339 first, then naive with Z appended
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
                .map(|ndt| Utc.from_utc_datetime(&ndt))
        })
        .ok()
}

// ── Market discovery ──────────────────────────────────────────────────────────

/// Parse a numeric JSON field that may be stored as a string or a number.
/// Non-finite values (NaN, ±Infinity) are treated as missing and return 0.0.
fn parse_f64_field(v: &Value) -> f64 {
    crate::json_util::parse_f64(v).unwrap_or(0.0)
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(default)
}

fn target_window_mins() -> f64 {
    env_f64("MACBOT_TARGET_WINDOW_MINS", 15.0)
}

fn window_tolerance_mins() -> f64 {
    env_f64("MACBOT_WINDOW_TOLERANCE_MINS", 1.0).max(0.1)
}

fn extract_window_from_text(text: &str) -> Option<f64> {
    let lower = text.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let num_str = &lower[start..i];
            let num: f64 = num_str.parse().ok()?;
            let mut j = i;
            while j < bytes.len() && matches!(bytes[j], b' ' | b'-' | b'_' | b'/') {
                j += 1;
            }
            let rest = &lower[j..];
            // "min..." covers min/mins/minute/minutes.
            // Bare 'm' or 'h' must NOT be the start of a longer word (e.g. "march",
            // "may") — otherwise "1815-march" in a Polymarket slug like
            // "btc-up-or-down-1800-1815-march-11" gets misread as 1815 minutes,
            // causing |1815 - 15| = 1800 > tolerance → market skipped every time.
            let next_alpha = |s: &str| {
                s.as_bytes()
                    .get(1)
                    .copied()
                    .map(|b| b.is_ascii_alphabetic())
                    .unwrap_or(false)
            };
            let is_minutes =
                rest.starts_with("min") || (rest.starts_with('m') && !next_alpha(rest));
            let is_hours = rest.starts_with("hr")
                || rest.starts_with("hour")
                || (rest.starts_with('h') && !next_alpha(rest));
            if is_minutes {
                return Some(num);
            }
            if is_hours {
                return Some(num * 60.0);
            }
        } else {
            i += 1;
        }
    }
    None
}

fn infer_window_mins(slug: &str, question: &str, start_date: &str, end_date: &str) -> Option<f64> {
    extract_window_from_text(slug)
        .or_else(|| extract_window_from_text(question))
        .or_else(|| {
            parse_utc(start_date)
                .and_then(|s| parse_utc(end_date).map(|e| (e - s).num_seconds() as f64 / 60.0))
        })
}

fn matches_slug_or_title(slug_pattern: &str, slug: &str, title: &str, question: &str) -> bool {
    if slug.contains(slug_pattern) {
        return true;
    }
    let lp = slug_pattern.to_lowercase();
    let combined = format!("{title} {question}").to_lowercase();
    if combined.contains(&lp) {
        return true;
    }
    if lp.contains("updown") && combined.contains("up or down") {
        if lp.contains("btc") {
            return combined.contains("btc") || combined.contains("bitcoin");
        }
        return true;
    }
    false
}

fn maker_candidate_confidence_floor(cfg: &crate::config::Config) -> f64 {
    let base_min_conf = crate::bot::risk::mode_min_confidence(cfg);
    if cfg.execution_mode.eq_ignore_ascii_case("maker") {
        (base_min_conf + cfg.min_confidence_margin).min(0.995)
    } else {
        base_min_conf
    }
}

fn maker_candidate_confidence_guard(
    cfg: &crate::config::Config,
    result: &mut FusionResult,
) -> Option<String> {
    if !cfg.execution_mode.eq_ignore_ascii_case("maker") || result.signal == Signal::Hold {
        return None;
    }

    let floor = maker_candidate_confidence_floor(cfg);
    if result.confidence + 1e-9 < floor {
        let reason = format!(
            "maker candidate confidence {:.0}% < floor {:.0}%",
            result.confidence * 100.0,
            floor * 100.0
        );
        result.signal = Signal::Hold;
        if result.reason.is_empty() {
            result.reason = reason.clone();
        } else if !result.reason.contains("maker candidate confidence") {
            result.reason = format!("{} | {}", result.reason, reason);
        }
        return Some(reason);
    }

    None
}

fn parse_epoch_from_slug(slug: &str) -> Option<i64> {
    let last = slug.rsplit('-').next()?;
    if last.len() < 9 || last.len() > 11 {
        return None;
    }
    if !last.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    last.parse::<i64>().ok()
}

fn month_number(s: &str) -> Option<u32> {
    match s {
        "january" => Some(1),
        "february" => Some(2),
        "march" => Some(3),
        "april" => Some(4),
        "may" => Some(5),
        "june" => Some(6),
        "july" => Some(7),
        "august" => Some(8),
        "september" => Some(9),
        "october" => Some(10),
        "november" => Some(11),
        "december" => Some(12),
        _ => None,
    }
}

fn parse_time_tokens(s: &str) -> Vec<NaiveTime> {
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut hour: u32 = 0;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                hour = hour
                    .saturating_mul(10)
                    .saturating_add((bytes[i] - b'0') as u32);
                i += 1;
            }
            let mut minute: u32 = 0;
            if i < bytes.len() && bytes[i] == b':' {
                i += 1;
                if i + 1 < bytes.len() && bytes[i].is_ascii_digit() && bytes[i + 1].is_ascii_digit()
                {
                    minute = ((bytes[i] - b'0') as u32) * 10 + (bytes[i + 1] - b'0') as u32;
                    i += 2;
                }
            }
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let suffix = s.get(i..i + 2.min(s.len().saturating_sub(i))).unwrap_or("");
            let mut h24 = hour;
            if suffix == "am" {
                if h24 == 12 {
                    h24 = 0;
                }
            } else if suffix == "pm" {
                if h24 < 12 {
                    h24 += 12;
                }
            } else {
                i = start + 1;
                continue;
            }
            i += 2;
            if let Some(t) = NaiveTime::from_hms_opt(h24, minute, 0) {
                out.push(t);
            }
        } else {
            i += 1;
        }
    }
    out
}

fn parse_question_times(question: &str) -> Option<(NaiveTime, NaiveTime)> {
    let lower = question.to_lowercase();
    let times = parse_time_tokens(&lower);
    if times.len() < 2 {
        return None;
    }
    Some((times[0], times[1]))
}

fn parse_question_window_with_date(
    question: &str,
    date: NaiveDate,
) -> Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    let (t0, t1) = parse_question_times(question)?;
    let start_naive = NaiveDateTime::new(date, t0);
    let end_naive = NaiveDateTime::new(date, t1);
    let start_local = New_York
        .from_local_datetime(&start_naive)
        .single()
        .or_else(|| New_York.from_local_datetime(&start_naive).earliest())?;
    let mut end_local = New_York
        .from_local_datetime(&end_naive)
        .single()
        .or_else(|| New_York.from_local_datetime(&end_naive).earliest())?;
    if end_local < start_local {
        end_local += chrono::Duration::days(1);
    }
    Some((
        start_local.with_timezone(&Utc),
        end_local.with_timezone(&Utc),
    ))
}

fn parse_question_window_utc(
    question: &str,
    now: &chrono::DateTime<Utc>,
) -> Option<(chrono::DateTime<Utc>, chrono::DateTime<Utc>)> {
    let lower = question.to_lowercase();
    let mut month_pos: Option<(usize, u32)> = None;
    for name in [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ] {
        if let Some(idx) = lower.find(name) {
            month_pos = Some((idx, month_number(name)?));
            break;
        }
    }
    let (idx, month) = month_pos?;
    let rest = &lower[idx + 1..];
    let mut day: Option<u32> = None;
    let mut i = 0usize;
    while i < rest.len() {
        if rest.as_bytes()[i].is_ascii_digit() {
            let start = i;
            while i < rest.len() && rest.as_bytes()[i].is_ascii_digit() {
                i += 1;
            }
            day = rest[start..i].parse::<u32>().ok();
            break;
        }
        i += 1;
    }
    let day = day?;
    let (t0, t1) = parse_question_times(question)?;
    let mut year = now.year();
    if month < now.month() && (now.month() - month) > 6 {
        year += 1;
    }
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let start_naive = NaiveDateTime::new(date, t0);
    let end_naive = NaiveDateTime::new(date, t1);
    let start_local = New_York
        .from_local_datetime(&start_naive)
        .single()
        .or_else(|| New_York.from_local_datetime(&start_naive).earliest())?;
    let mut end_local = New_York
        .from_local_datetime(&end_naive)
        .single()
        .or_else(|| New_York.from_local_datetime(&end_naive).earliest())?;
    if end_local < start_local {
        end_local += chrono::Duration::days(1);
    }
    Some((
        start_local.with_timezone(&Utc),
        end_local.with_timezone(&Utc),
    ))
}

fn floor_start_dt(dt: chrono::DateTime<Utc>, window_mins: f64) -> Option<chrono::DateTime<Utc>> {
    let window = window_mins.round() as i64;
    if window <= 0 {
        return None;
    }
    let local = dt.with_timezone(&New_York);
    let total_mins = (local.hour() as i64) * 60 + (local.minute() as i64);
    let floored = (total_mins / window) * window;
    let hour = (floored / 60) as u32;
    let minute = (floored % 60) as u32;
    let date = local.date_naive();
    let naive = NaiveDateTime::new(date, NaiveTime::from_hms_opt(hour, minute, 0)?);
    let mut floored_local = New_York
        .from_local_datetime(&naive)
        .single()
        .or_else(|| New_York.from_local_datetime(&naive).earliest())?;
    if floored_local > local {
        floored_local -= chrono::Duration::minutes(window);
    }
    Some(floored_local.with_timezone(&Utc))
}

fn normalize_question_label(question: &str, start_dt: &chrono::DateTime<Utc>) -> String {
    let Some((t0, t1)) = parse_question_times(question) else {
        return question.to_string();
    };
    let prefix = question.split(" - ").next().unwrap_or(question).trim();
    let start_local = start_dt.with_timezone(&New_York);
    let date_str = start_local.format("%B %-d").to_string();
    let t0s = t0.format("%-I:%M%p").to_string();
    let t1s = t1.format("%-I:%M%p").to_string();
    format!("{prefix} - {date_str}, {t0s}-{t1s} ET")
}

fn debug_markets_enabled() -> bool {
    std::env::var("MACBOT_DEBUG_MARKETS")
        .ok()
        .map(|v| {
            let x = v.trim().to_ascii_lowercase();
            matches!(x.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

fn select_start_dt(
    now: &chrono::DateTime<Utc>,
    start_date_str: &str,
    slug: &str,
    window_mins: f64,
    question: &str,
    end_date_str: &str,
) -> Option<chrono::DateTime<Utc>> {
    if !window_mins.is_finite() || !(1.0..=1440.0).contains(&window_mins) {
        return None;
    }
    let wm = chrono::Duration::minutes(window_mins.round() as i64);

    // ── Priority 1: slug epoch (exact start time embedded in slug) ──────
    // Slugs like "btc-updown-15m-1773625500" contain the Unix epoch of the
    // real market-window start.  This is the most reliable source.
    if let Some(ts) = parse_epoch_from_slug(slug).and_then(|ts| Utc.timestamp_opt(ts, 0).single()) {
        let end_dt = ts + wm;
        return (end_dt > *now).then_some(ts);
    }

    // ── Priority 2: endDate − window (Polymarket endDate is always correct) ──
    if let Some(end_dt) = parse_utc(end_date_str) {
        if end_dt <= *now {
            return None;
        }
        let start = end_dt - wm;
        if end_dt > *now && (*now - start).num_minutes().abs() <= (window_mins * 2.0) as i64 {
            return Some(start);
        }
    }

    // ── Priority 3: parse date + times from question text ───────────────
    // "Bitcoin Up or Down - March 15, 10:00PM-10:15PM ET" → extracts month,
    // day, start-time, end-time directly from the question.
    if let Some((qs, qe)) = parse_question_window_utc(question, now) {
        if qe > *now {
            return Some(qs);
        }
    }

    // ── Priority 4: parse times from question using API startDate's date ─
    let dt = parse_utc(start_date_str);
    if let Some(dt) = dt {
        let base_date = dt.with_timezone(&New_York).date_naive();
        if let Some((qs, qe)) = parse_question_window_with_date(question, base_date) {
            if qe > *now {
                return Some(qs);
            }
        }
        // Also try with tomorrow's date (API startDate can be a day behind)
        let next_date = base_date + chrono::Duration::days(1);
        if let Some((qs, qe)) = parse_question_window_with_date(question, next_date) {
            if qe > *now {
                return Some(qs);
            }
        }
    }

    // ── Priority 5: floor API startDate to window boundary ──────────────
    if let Some(dt) = dt {
        if let Some(floored) = floor_start_dt(dt, window_mins) {
            let end_dt = floored + wm;
            if end_dt > *now {
                return Some(floored);
            }
        }
    }

    None
}

/// Fetch the events for one exact slug, with the same three attempts the
/// broad list gets.
///
/// Returns the events (possibly none) or the last error.
async fn fetch_events_by_slug(slug: &str) -> std::result::Result<Vec<Value>, String> {
    let url = format!("{GAMMA_BASE}/events?slug={slug}");
    let mut last_err = String::new();
    for attempt in 1..=3usize {
        match HTTP.get(&url).send().await {
            Ok(r) => {
                let status = r.status();
                if status.is_success() {
                    match r.bounded_json::<Value>().await {
                        Ok(v) => {
                            return Ok(v
                                .as_array()
                                .cloned()
                                .or_else(|| v.get("data").and_then(|a| a.as_array()).cloned())
                                .unwrap_or_default());
                        }
                        Err(e) => last_err = format!("JSON parse error: {e}"),
                    }
                } else {
                    last_err = format!("HTTP {status}");
                }
            }
            Err(e) => last_err = format!("network: {e}"),
        }
        if attempt < 3 {
            tokio::time::sleep(tokio::time::Duration::from_millis(400)).await;
        }
    }
    Err(last_err)
}

/// The maker does not take a market with fewer than
/// `maker_candidate_min_remaining_seconds` to go. For the last five minutes
/// of every window the analysis reported "No active BTC Up/Down 15m market
/// found" — fifteen warnings a window for a market that was there — and the
/// dashboard showed no market at all. Name the market and the rule instead;
/// the signal is still Hold and nothing quotes it.
fn late_market_analysis(
    all_markets: &[MarketData],
    now: DateTime<Utc>,
    min_remaining_secs: f64,
) -> Option<Value> {
    if min_remaining_secs <= 0.0 {
        return None;
    }
    let mut best: Option<(&MarketData, DateTime<Utc>, DateTime<Utc>, f64)> = None;
    for m in all_markets {
        let Some(start_dt) = parse_utc(&m.start_date).or_else(|| {
            parse_utc(&m.end_date)
                .map(|end| end - chrono::Duration::minutes(m.window_mins.round() as i64))
        }) else {
            continue;
        };
        let end_dt = start_dt + chrono::Duration::minutes(m.window_mins.round() as i64);
        let secs_remaining = (end_dt - now).num_seconds() as f64;
        let elapsed_mins = (now - start_dt).num_seconds() as f64 / 60.0;
        if secs_remaining <= 0.0 || secs_remaining > min_remaining_secs || elapsed_mins < -0.25 {
            continue;
        }
        if best.is_none_or(|(_, _, _, left)| secs_remaining > left) {
            best = Some((m, start_dt, end_dt, secs_remaining));
        }
    }
    let (market, start_dt, end_dt, secs_remaining) = best?;
    Some(
        ParsedAnalysis {
            signal: Signal::Hold,
            confidence: 0.0,
            reason: format!(
                "Market has {:.0}s left, under the maker minimum of {:.0}s — waiting for the next window",
                secs_remaining, min_remaining_secs
            ),
            market: AnalysisMarketData {
                market_id: market.market_id.clone(),
                question: market.question.clone(),
                price: market.price,
                end_date: end_dt.to_rfc3339(),
                liquidity: market.liquidity,
            },
            extra: Some(json!({
                "minute_13_rule": false,
                "time_remaining_minutes": secs_remaining / 60.0,
                "waiting_for_window": true,
                "too_late_for_maker": true,
                "market_data": {
                    "start_date": start_dt.to_rfc3339(),
                    "elapsed_mins": ((now - start_dt).num_seconds() as f64 / 60.0),
                }
            })),
            ..ParsedAnalysis::default()
        }
        .to_value(),
    )
}

/// Fetch active BTC Up/Down 15-minute markets from the Gamma API.
/// Mirrors the Python get_polymarket_markets() exactly:
///   - queries events with end_date window [now, now+1h]
///   - filters slug containing `slug_pattern` (from config.market_filter.slug_pattern)
///   - checks active/closed at the EVENT level (not market level)
///   - parses outcomePrices as a JSON-encoded string
///   - falls back to event-level liquidity/volume
pub async fn get_polymarket_markets(slug_pattern: &str) -> Vec<MarketData> {
    let now = Utc::now();
    let debug = debug_markets_enabled();
    let mut debug_emitted = 0usize;
    let mut total_markets = 0usize;
    let mut selected_markets = 0usize;
    // Use a broad active-events query; filter by window at the market level.
    let url = format!("{GAMMA_BASE}/events?limit=200&order=startDate&ascending=false&active=true");

    // Retry up to 3 times with 1s delay — Gamma occasionally returns 429 or drops
    // connections, which previously caused a silent empty result and "no market found".
    let resp: Value = {
        let mut last_err = String::new();
        let mut result: Option<Value> = None;
        for attempt in 1..=3usize {
            match HTTP.get(&url).send().await {
                Ok(r) => {
                    let status = r.status();
                    if status.is_success() {
                        match r.bounded_json::<Value>().await {
                            Ok(v) => {
                                result = Some(v);
                                break;
                            }
                            Err(e) => {
                                last_err = format!("JSON parse error: {e}");
                                warn!("Gamma market JSON parse error (attempt {attempt}/3): {e}");
                            }
                        }
                    } else {
                        last_err = format!("HTTP {status}");
                        warn!("Gamma market fetch HTTP {status} (attempt {attempt}/3)");
                    }
                }
                Err(e) => {
                    last_err = format!("network: {e}");
                    warn!("Gamma market fetch error (attempt {attempt}/3): {e}");
                }
            }
            if attempt < 3 {
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        }
        match result {
            Some(v) => v,
            None => {
                warn!("Gamma market fetch failed after 3 attempts: {last_err}");
                json!([])
            }
        }
    };

    // Gamma returns either a JSON array or { "data": [...] }
    let mut events: Vec<Value> = resp
        .as_array()
        .cloned()
        .or_else(|| resp.get("data").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default();

    // ── Targeted fetch: compute current 15m window slug and fetch directly ──
    // The broad query (limit=200, sorted by startDate desc) often misses the
    // currently-active market because hundreds of newer future-window events
    // push it off the first page.  We compute the expected slug from the
    // current ET time, floored to the nearest 15-minute boundary, and fetch
    // that specific event by slug.
    {
        let now_et = now.with_timezone(&New_York);
        let target = target_window_mins();
        // Fetch both 5m and 15m windows for more ML training data
        for target_window in [5u32, 15u32] {
            let floored_min = (now_et.minute() / target_window) * target_window;
            if let Some(floored_et) = now_et
                .with_minute(floored_min)
                .and_then(|dt| dt.with_second(0))
                .and_then(|dt| dt.with_nanosecond(0))
            {
                let epoch = floored_et.with_timezone(&Utc).timestamp();
                // Try both the current window and the previous window (in case we're
                // right at the boundary and the current one hasn't been created yet).
                for ep in [epoch, epoch - (target_window as i64) * 60] {
                    let target_slug = format!("{slug_pattern}-{target_window}m-{ep}");
                    // This lookup is the only reliable way to find the live
                    // window: the broad list is capped at a hundred and is
                    // filled with tomorrow's pre-created markets, sorted to
                    // the top by creation time. It used to be a bare
                    // `if let Ok(r)` with no retry and no log, so when it
                    // failed the bot reported "no active market" while
                    // Gamma had one, and nothing recorded why.
                    let is_live_window = ep == epoch && f64::from(target_window) == target;
                    match fetch_events_by_slug(&target_slug).await {
                        Ok(slug_events) => {
                            if is_live_window && slug_events.is_empty() {
                                warn!("Gamma direct lookup for {target_slug} returned no event");
                            }
                            let existing_slugs: std::collections::HashSet<String> = events
                                .iter()
                                .filter_map(|e| {
                                    e.get("slug")
                                        .and_then(|s| s.as_str())
                                        .map(|s| s.to_string())
                                })
                                .collect();
                            for se in slug_events {
                                let s = se.get("slug").and_then(|s| s.as_str()).unwrap_or("");
                                if !existing_slugs.contains(s) {
                                    events.push(se);
                                }
                            }
                        }
                        Err(e) => {
                            if is_live_window {
                                warn!("Gamma direct lookup for {target_slug} failed: {e}");
                            } else {
                                tracing::debug!(
                                    "Gamma direct lookup for {target_slug} failed: {e}"
                                );
                            }
                        }
                    }
                }
            }
        } // for target_window
    }

    let mut markets: Vec<MarketData> = Vec::new();

    for event in &events {
        // ── Filter by slug at event level (matches Python) ────────────────
        let slug = event.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let title = event.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !matches_slug_or_title(slug_pattern, slug, title, "") {
            continue;
        }

        // ── Active/closed check at EVENT level ────────────────────────────
        let ev_closed = event
            .get("closed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let ev_active = event
            .get("active")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if ev_closed || !ev_active {
            continue;
        }

        // Event-level fallback fields
        let ev_liq = event.get("liquidity").map(parse_f64_field).unwrap_or(0.0);
        let ev_vol = event.get("volume").map(parse_f64_field).unwrap_or(0.0);
        let ev_end = event
            .get("endDate")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let ev_title = event
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("BTC Up or Down")
            .to_string();

        let sub_markets = event
            .get("markets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for m in &sub_markets {
            total_markets += 1;
            if m.get("closed").and_then(Value::as_bool) == Some(true)
                || m.get("active").and_then(Value::as_bool) == Some(false)
            {
                continue;
            }
            // Market ID: conditionId first, then id
            let market_id = m
                .get("conditionId")
                .or_else(|| m.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if market_id.is_empty() {
                continue;
            }

            // outcomePrices is stored as a JSON-encoded STRING "[\"0.5\",\"0.5\"]"
            let yes_price: f64 = m
                .get("outcomePrices")
                .and_then(|v| {
                    // Try as raw JSON array first
                    if let Some(arr) = v.as_array() {
                        return arr.first().and_then(|x| {
                            x.as_str()
                                .and_then(|s| s.parse().ok())
                                .or_else(|| x.as_f64())
                        });
                    }
                    // Otherwise parse the embedded JSON string
                    if let Some(s) = v.as_str() {
                        if let Ok(parsed) = serde_json::from_str::<Vec<Value>>(s) {
                            return parsed.first().and_then(|x| {
                                x.as_str()
                                    .and_then(|s2| s2.parse().ok())
                                    .or_else(|| x.as_f64())
                            });
                        }
                    }
                    None
                })
                .unwrap_or(0.5);

            let start_date = m
                .get("startDate")
                .or_else(|| event.get("startDate"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let end_date = m
                .get("endDate")
                .and_then(|v| v.as_str())
                .unwrap_or(&ev_end)
                .to_string();
            let question_raw = m
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or(&ev_title)
                .to_string();

            let market_slug = m.get("slug").and_then(|v| v.as_str()).unwrap_or(slug);
            if !matches_slug_or_title(slug_pattern, market_slug, title, &question_raw) {
                if debug && debug_emitted < 10 {
                    tracing::info!(
                        "DEBUG market skip slug: slug={} title={} question={}",
                        market_slug,
                        title,
                        question_raw
                    );
                    debug_emitted += 1;
                }
                continue;
            }

            let target = target_window_mins();
            let tol = window_tolerance_mins();
            let window = if let Some(window) =
                infer_window_mins(market_slug, &question_raw, &start_date, &end_date)
            {
                if (window - target).abs() > tol {
                    if debug && debug_emitted < 10 {
                        tracing::info!(
                            "DEBUG market skip window: slug={} window={} target={} tol={}",
                            market_slug,
                            window,
                            target,
                            tol
                        );
                        debug_emitted += 1;
                    }
                    continue;
                }
                window
            } else {
                if debug && debug_emitted < 10 {
                    tracing::info!(
                        "DEBUG market skip no-window: slug={} question={}",
                        market_slug,
                        question_raw
                    );
                    debug_emitted += 1;
                }
                continue;
            };

            let computed_start = select_start_dt(
                &now,
                &start_date,
                market_slug,
                window,
                &question_raw,
                &end_date,
            );

            let start_dt = match computed_start {
                Some(dt) => dt,
                None => {
                    if debug && debug_emitted < 10 {
                        tracing::info!(
                            "DEBUG market skip no-start: slug={} startDate={} slug_epoch={:?}",
                            market_slug,
                            start_date,
                            parse_epoch_from_slug(market_slug)
                        );
                        debug_emitted += 1;
                    }
                    continue;
                }
            };

            let end_dt = start_dt + chrono::Duration::minutes(window.round() as i64);
            let mins_left = (end_dt - now).num_seconds() as f64 / 60.0;
            let elapsed_mins = (now - start_dt).num_seconds() as f64 / 60.0;
            if mins_left <= 0.0 || mins_left > window + 1.0 || elapsed_mins < -0.25 {
                if debug && debug_emitted < 10 {
                    tracing::info!(
                        "DEBUG market skip timing: slug={} elapsed={:.2} mins_left={:.2} window={}",
                        market_slug,
                        elapsed_mins,
                        mins_left,
                        window
                    );
                    debug_emitted += 1;
                }
                continue;
            }

            let computed_end = end_dt.to_rfc3339();
            let display_question = normalize_question_label(&question_raw, &start_dt);

            // Liquidity/volume: event level first, then market level (matches Python)
            let liquidity = if ev_liq > 0.0 {
                ev_liq
            } else {
                m.get("liquidity").map(parse_f64_field).unwrap_or(0.0)
            };
            let volume = if ev_vol > 0.0 {
                ev_vol
            } else {
                m.get("volume").map(parse_f64_field).unwrap_or(0.0)
            };

            // Try to extract the exact BTC reference price from the
            // sub-market description or question (set by Polymarket oracle).
            let description = m.get("description").and_then(|v| v.as_str()).unwrap_or("");
            let btc_ref_price = parse_btc_price_from_text(description)
                .or_else(|| parse_btc_price_from_text(&question_raw));

            markets.push(MarketData {
                market_id,
                question: display_question,
                price: yes_price,
                volume,
                liquidity,
                start_date: start_dt.to_rfc3339(),
                end_date: computed_end,
                window_mins: window,
                source: "gamma".to_string(),
                btc_ref_price,
            });
            selected_markets += 1;
        }
    }

    // Sort by end_date ascending — earliest expiry first
    markets.sort_by(|a, b| a.start_date.cmp(&b.start_date));
    if debug {
        tracing::info!(
            "DEBUG markets summary: total={} selected={} slug_pattern={}",
            total_markets,
            selected_markets,
            slug_pattern
        );
    }
    markets
}

// ── UP token cache helper ─────────────────────────────────────────────────────

impl TradingBot {
    /// Return the YES/UP token_id for a given market, using a 60-second cache.
    pub async fn get_up_token_cached(&self, market_id: &str) -> Option<String> {
        const TTL: Duration = Duration::from_secs(60);

        // Check cache
        if let Some(entry) = self.market_state.up_token_cache.get(market_id) {
            let (fetched_at, token_id) = entry.value().clone();
            if fetched_at.elapsed() < TTL {
                return Some(token_id);
            }
        }

        // Fetch from CLOB
        let url = format!("{CLOB_BASE}/markets/{market_id}");
        let resp: Value = match HTTP.get(&url).send().await {
            Ok(r) => match r.bounded_json().await {
                Ok(v) => v,
                Err(e) => {
                    warn!("up_token parse error: {e}");
                    return None;
                }
            },
            Err(e) => {
                warn!("up_token fetch error: {e}");
                return None;
            }
        };

        // The tokens array has objects with "outcome" field
        let token_id = resp
            .get("tokens")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|t| {
                        let outcome = t.get("outcome").and_then(|v| v.as_str()).unwrap_or("");
                        outcome.to_uppercase().contains("YES")
                            || outcome.to_uppercase().contains("UP")
                    })
                    .and_then(|t| t.get("token_id").and_then(|v| v.as_str()))
                    .map(|s| s.to_string())
            });

        if let Some(ref tid) = token_id {
            self.market_state
                .up_token_cache
                .insert(market_id.to_string(), (Instant::now(), tid.clone()));
        }
        token_id
    }

    // ── Main analyze entry point ──────────────────────────────────────────────

    /// Full market analysis pass — returns a JSON Value with every signal field.
    pub async fn analyze_market(&self) -> Value {
        // ── Step 1: Get market list ───────────────────────────────────────────
        let (slug_pattern, maker_min_remaining_secs) = {
            let cfg = self.control_plane.config.read().await;
            let min_remaining = if cfg.execution_mode.eq_ignore_ascii_case("maker") {
                cfg.maker_candidate_min_remaining_seconds as f64
            } else {
                0.0
            };
            (cfg.market_filter.slug_pattern.clone(), min_remaining)
        };
        let all_markets = get_polymarket_markets(&slug_pattern).await;
        let now = Utc::now();

        // Apply Python's elapsed-minute filter: only markets 0..14 min into
        // their 15-min window, with no liquidity/volume floor (already API-filtered).
        // Among those, pick the one closest to minute-6 (confidence peak).
        struct Candidate<'a> {
            market: &'a MarketData,
            elapsed: f64,
            start_dt: chrono::DateTime<Utc>,
            end_dt: chrono::DateTime<Utc>,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        if all_markets.is_empty() {
            warn!("analyze_market: all_markets is EMPTY after filtering");
        }
        for m in &all_markets {
            let start_dt = parse_utc(&m.start_date).or_else(|| {
                parse_utc(&m.end_date)
                    .map(|end| end - chrono::Duration::minutes(m.window_mins.round() as i64))
            });
            let start_dt = match start_dt {
                Some(dt) => dt,
                None => {
                    tracing::debug!("candidate skip: no start_dt for {}", m.market_id);
                    continue;
                }
            };
            let end_dt = start_dt + chrono::Duration::minutes(m.window_mins.round() as i64);
            let secs_remaining = (end_dt - now).num_seconds().max(0) as f64;
            let elapsed_mins = (now - start_dt).num_seconds() as f64 / 60.0;
            let min_elapsed = 0.5;
            let max_elapsed = (m.window_mins - 0.1).max(min_elapsed);
            if (min_elapsed..max_elapsed).contains(&elapsed_mins)
                && end_dt > now
                && secs_remaining > maker_min_remaining_secs
            {
                candidates.push(Candidate {
                    market: m,
                    elapsed: elapsed_mins,
                    start_dt,
                    end_dt,
                });
            } else {
                tracing::debug!(
                    "candidate skip: id={} elapsed={:.1} window={:.0} end_dt={} now={} question={}",
                    m.market_id,
                    elapsed_mins,
                    m.window_mins,
                    end_dt,
                    now,
                    m.question
                );
            }
        }

        // Sort by proximity to minute-6 (Gaussian confidence peak)
        candidates.sort_by(|a, b| {
            let target = a.market.window_mins * 0.4;
            (a.elapsed - target)
                .abs()
                .partial_cmp(&(b.elapsed - (b.market.window_mins * 0.4)).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let (market, start_dt, end_dt) = match candidates.into_iter().next() {
            Some(c) => (c.market.clone(), c.start_dt, c.end_dt),
            None => {
                let mut fallback: Option<(
                    MarketData,
                    chrono::DateTime<Utc>,
                    chrono::DateTime<Utc>,
                    f64,
                )> = None;
                for m in &all_markets {
                    let start_dt = parse_utc(&m.start_date).or_else(|| {
                        parse_utc(&m.end_date).map(|end| {
                            end - chrono::Duration::minutes(m.window_mins.round() as i64)
                        })
                    });
                    let start_dt = match start_dt {
                        Some(dt) => dt,
                        None => continue,
                    };
                    let end_dt = start_dt + chrono::Duration::minutes(m.window_mins.round() as i64);
                    let mins_left = (end_dt - now).num_seconds() as f64 / 60.0;
                    let secs_remaining = (end_dt - now).num_seconds().max(0) as f64;
                    let elapsed_mins = (now - start_dt).num_seconds() as f64 / 60.0;
                    if end_dt <= now
                        || mins_left > m.window_mins + 1.0
                        || elapsed_mins < -0.25
                        || secs_remaining <= maker_min_remaining_secs
                    {
                        continue;
                    }
                    let target = m.window_mins * 0.4;
                    let score = (elapsed_mins - target).abs();
                    match fallback {
                        None => fallback = Some((m.clone(), start_dt, end_dt, score)),
                        Some((_, _, _, best_score)) if score < best_score => {
                            fallback = Some((m.clone(), start_dt, end_dt, score))
                        }
                        _ => {}
                    }
                }
                if let Some((market, start_dt, end_dt, _)) = fallback {
                    let secs_remaining = (end_dt - now).num_seconds().max(0) as f64;
                    let time_remaining_minutes = secs_remaining / 60.0;
                    // This is the first half-minute of a window (or a market
                    // outside the 0.5–14.9 minute candidate band), not a
                    // missing market: it is right there in `market`. The old
                    // text said "no active market" over a market the page was
                    // showing.
                    let elapsed_secs = (now - start_dt).num_seconds();
                    let reason = if elapsed_secs < 30 {
                        format!(
                            "Market opened {elapsed_secs}s ago — waiting for the first 30s of trading"
                        )
                    } else {
                        format!(
                            "Market is outside the candidate band ({:.1} min elapsed) — waiting",
                            elapsed_secs as f64 / 60.0
                        )
                    };
                    tracing::info!("{reason} (market {})", market.market_id);
                    return ParsedAnalysis {
                        signal: Signal::Hold,
                        confidence: 0.0,
                        reason,
                        market: AnalysisMarketData {
                            market_id: market.market_id.clone(),
                            question: market.question.clone(),
                            price: market.price,
                            end_date: end_dt.to_rfc3339(),
                            liquidity: market.liquidity,
                        },
                        extra: Some(json!({
                            "minute_13_rule": false,
                            "time_remaining_minutes": time_remaining_minutes,
                            "waiting_for_window": true,
                            "market_data": {
                                "start_date": start_dt.to_rfc3339(),
                                "elapsed_mins": ((now - start_dt).num_seconds() as f64 / 60.0),
                            }
                        })),
                        ..ParsedAnalysis::default()
                    }
                    .to_value();
                }
                if let Some(late) =
                    late_market_analysis(&all_markets, now, maker_min_remaining_secs)
                {
                    tracing::info!(
                        "{}",
                        late.get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("late market")
                    );
                    return late;
                }
                warn!("No active BTC Up/Down 15m market found");
                return ParsedAnalysis {
                    reason: "No active BTC Up/Down 15-minute market in current window — waiting"
                        .to_string(),
                    extra: Some(json!({
                        "minute_13_rule": false,
                        "time_remaining_minutes": 0.0,
                        "waiting_for_window": true,
                        "market_data": Value::Null
                    })),
                    ..ParsedAnalysis::default()
                }
                .to_value();
            }
        };
        let mut market = market;
        let liquidity = market.liquidity;

        // ── Step 1b: Override Gamma price with live CLOB price ──────────────────
        // Gamma outcomePrices is often stale for 15-min markets (stays ~0.50 the
        // entire window). Fetch the real YES token price from CLOB to get an
        // accurate market price for analysis and trade entry.
        let up_token = self.get_up_token_cached(&market.market_id).await;
        if let Some(ref token_id) = up_token {
            let url = format!("{CLOB_BASE}/price?token_id={token_id}&side=BUY");
            if let Ok(r) = HTTP.get(&url).send().await {
                if r.status().is_success() {
                    if let Ok(v) = r.bounded_json::<serde_json::Value>().await {
                        if let Some(p) = v.get("price").and_then(|x| {
                            x.as_f64()
                                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
                        }) {
                            if p > 0.0 && p < 1.0 {
                                let gamma_price = market.price;
                                market.price = p;
                                if (p - gamma_price).abs() > 0.05 {
                                    tracing::info!(
                                        "CLOB price override: gamma={:.3} → clob={:.3} (Δ={:.3})",
                                        gamma_price,
                                        p,
                                        p - gamma_price
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        let market = &market;
        let market_price = market.price;

        // ── Step 2: Parse end_date → time_remaining ───────────────────────────
        let secs_remaining = (end_dt - now).num_seconds().max(0) as f64;
        let time_remaining_minutes = secs_remaining / 60.0;

        // ── Step 3: Minute-13 Rule (≤2 min remaining → momentum-only signal) ──
        if time_remaining_minutes <= 2.0 {
            return self
                .minute_13_signal(market, market_price, time_remaining_minutes)
                .await
                .to_value();
        }

        // ── Step 4: Market start time for BTC comparison ──────────────────────
        let market_start = start_dt;

        // ── Step 6: Fetch candles (5-min cache) ───────────────────────────────
        let candles = self.get_cached_candles().await;

        // Compute TA signals from candles
        let vwap = compute_vwap(&candles);
        let rsi = compute_rsi(&candles, 14);
        let macd = compute_macd(&candles, 12, 26, 9);
        let ha = compute_ha(&candles);
        let regime = compute_regime(&candles, vwap.slope).to_string();

        // ── Step 7: Snapshot config for gating ───────────────────────────────
        let cfg_snap = self.control_plane.config.read().await.clone();

        // ── Step 8: Fetch futures (5-min cache) ───────────────────────────────
        let (
            funding_rate,
            open_interest,
            prev_open_interest,
            futures_basis_pct,
            futures_mark_price,
            futures_index_price,
        ) = if cfg_snap.funding_rate_enabled || cfg_snap.oi_momentum_enabled {
            self.get_cached_futures().await
        } else {
            (None, None, None, None, None, None)
        };
        let funding = if cfg_snap.funding_rate_enabled {
            compute_funding_signal(funding_rate, open_interest)
        } else {
            crate::signals::FundingData::default()
        };

        // ── Step 9: Fetch orderbook + compute OBI ─────────────────────────────
        let (ob_data, obi_stable, orderbook_available, raw_orderbook) =
            if let Some(ref tid) = up_token {
                match signals::fetchers::fetch_polymarket_orderbook(tid).await {
                    Ok(raw) => {
                        let ob = compute_orderbook_imbalance(&raw.bids, &raw.asks);
                        let stable = {
                            let mut tracker = self.signal_trackers.obi_tracker.lock().await;
                            tracker.update(ob.imbalance)
                        };
                        // Update tick velocity with current market price
                        {
                            let mut tv = self.signal_trackers.tick_vel.lock().await;
                            tv.update(market_price, Some(&market.market_id));
                        }
                        let available = !raw.bids.is_empty() || !raw.asks.is_empty();
                        (ob, stable, available, Some(raw))
                    }
                    Err(e) => {
                        warn!("Orderbook fetch failed: {e}");
                        (signals::ObData::default(), true, false, None)
                    }
                }
            } else {
                (signals::ObData::default(), true, false, None)
            };

        // ── Step 10: Tick velocity ─────────────────────────────────────────────
        let tick_vel_data = {
            let tv = self.signal_trackers.tick_vel.lock().await;
            tv.compute()
        };

        // ── Step 10b: Aggressor ratio (short-horizon order flow) ─────────────
        let aggressor_data: Option<crate::signals::AggressorData> =
            if cfg_snap.aggressor_ratio_enabled {
                let (buy_vol, sell_vol) = self.get_cached_aggressor().await;
                Some(compute_aggressor_ratio(buy_vol, sell_vol))
            } else {
                None
            };

        // ── Step 11: PCR (5-min cache, if enabled) ────────────────────────────
        let mut pcr_signal: Option<CachedSignal<crate::signals::fetchers::PcrOpenInterest>> =
            if cfg_snap.pcr_enabled {
                Some(self.get_cached_pcr().await)
            } else {
                None
            };
        let pcr_data: Option<crate::signals::PcrData> = pcr_signal
            .as_ref()
            .filter(|signal| signal.freshness == SignalFreshness::Fresh)
            .and_then(|signal| compute_deribit_pcr(signal.value.put_oi, signal.value.call_oi));

        // ── Step 12: Fear & Greed (1-hr cache, if enabled) ────────────────────
        let fg_signal: Option<CachedSignal<u32>> = if cfg_snap.fear_greed_enabled {
            Some(self.get_cached_fg().await)
        } else {
            None
        };
        let fg_data: Option<crate::signals::FearGreedData> = fg_signal
            .as_ref()
            .map(|signal| compute_fear_greed(signal.value));

        // ── Step 12b: Macro sentiment signals ────────────────────────────────
        // coingecko_sentiment, finnhub_sentiment, spike_detector pruned
        // (negative correlation). Types removed from codebase.

        let lunarcrush_signal: Option<CachedSignal<(Option<f64>, Option<f64>)>> =
            if cfg_snap.lunarcrush_sentiment_enabled {
                Some(
                    self.get_cached_lunarcrush(&cfg_snap.lunarcrush_api_key)
                        .await,
                )
            } else {
                None
            };
        let lunarcrush_data: Option<crate::signals::LunarcrushSentimentData> = lunarcrush_signal
            .as_ref()
            .map(|signal| compute_lunarcrush_sentiment(signal.value.0, signal.value.1));

        // ── Step 13: Spike detector — pruned (negative correlation) ─────────

        // ── Step 14: BTC analysis from market start ───────────────────────────
        // Priority: 1) cached value for this market (fetched earlier this cycle)
        //           2) oracle price parsed from Gamma market description
        //           3) Binance klines open price at market_start
        // Evict stale btc_start_cache entries (older than 20 minutes)
        let stale_keys: Vec<String> = self
            .market_state
            .btc_start_cache
            .iter()
            .filter(|e| e.value().0.elapsed().as_secs() > 1200)
            .map(|e| e.key().clone())
            .collect();
        for key in stale_keys {
            self.market_state.btc_start_cache.remove(&key);
        }

        let (btc_start_price, btc_start_source) =
            if let Some(cached) = self.market_state.btc_start_cache.get(&market.market_id) {
                (cached.1, cached.2.clone())
            } else {
                let (p, src) = if let Some(ref_price) = market.btc_ref_price {
                    // Oracle price from Polymarket description — most accurate
                    (ref_price, "oracle".to_string())
                } else {
                    // Fallback: Binance candle at market_start.
                    // Try both the exact candle and 1 min prior (oracle may
                    // snapshot BTC price slightly before the window opens).
                    let exact = self.get_btc_price_at_start(market_start).await;
                    let prior = self
                        .get_btc_price_at_start(market_start - chrono::Duration::seconds(60))
                        .await;
                    // Use exact candle by default; if the two differ significantly
                    // the prior candle is likely closer to the oracle snapshot.
                    // In practice this matters most for 15-min windows where even
                    // $20 off changes the BTC-change% signal meaningfully.
                    if exact > 0.0 {
                        (exact, format!("binance@{}", market_start.format("%H:%M")))
                    } else if prior > 0.0 {
                        (
                            prior,
                            format!("binance@{}-1m", market_start.format("%H:%M")),
                        )
                    } else {
                        (0.0, "none".to_string())
                    }
                };
                if p > 0.0 {
                    tracing::info!(
                        "BTC price-to-beat for {}: ${:.2} (source={})",
                        market.market_id.chars().take(12).collect::<String>(),
                        p,
                        src,
                    );
                    self.market_state
                        .btc_start_cache
                        .insert(market.market_id.clone(), (Instant::now(), p, src.clone()));
                }
                (p, src)
            };
        let btc_current = BTCAnalyzer::get_price().await.unwrap_or(0.0);
        let price_to_beat_distance_pct = if btc_start_price > 0.0 && btc_current > 0.0 {
            Some((btc_current - btc_start_price) / btc_start_price * 100.0)
        } else {
            None
        };
        let btc_momentum = if btc_start_price > 0.0 && btc_current > 0.0 {
            (btc_current - btc_start_price) / btc_start_price * 100.0
        } else {
            0.0
        };
        let (btc_prediction, btc_confidence) = self.btc_trend(btc_momentum);

        // ── Step 14c: Altcoin correlation + cross-exchange price ────────────
        let (altcoin_corr, coinbase_divergence) = {
            let btc_pct = btc_momentum;
            let (corr_res, cb_res) = tokio::join!(
                crate::signals::fetchers::fetch_altcoin_correlation(btc_pct),
                crate::signals::fetchers::fetch_coinbase_btc_price()
            );
            let corr = corr_res.ok();
            let cb_div = cb_res.ok().map(|cb_price| {
                if btc_current > 0.0 {
                    (cb_price - btc_current) / btc_current * 100.0
                } else {
                    0.0
                }
            });
            (corr, cb_div)
        };

        // ── Step 14c.1: Feed BTC/ETH returns to correlation guard ────────────
        if let Some(ref corr_data) = altcoin_corr {
            let btc_ret = btc_momentum; // % change
            let eth_ret = corr_data.eth_change_pct;
            let mut tracker = self.signal_trackers.correlation_tracker.lock().await;
            tracker.push(btc_ret, eth_ret);
        }

        // ── Step 14d: Phase 2 new signals (concurrent fetch) ─────────────────
        let (polymarket_flow_result, whale_result, options_adv_result, cross_market_signals) = {
            let up_tok = up_token.clone();
            tokio::join!(
                async {
                    if let Some(ref tid) = up_tok {
                        signals::fetchers::fetch_polymarket_flow(&market.market_id, tid)
                            .await
                            .ok()
                    } else {
                        None
                    }
                },
                signals::fetchers::fetch_whale_transfers(),
                signals::fetchers::fetch_deribit_advanced(),
                crate::signals::fetch_cross_market_resolutions(),
            )
        };
        let cross_market_stale = cross_market_signals.is_err();
        let cross_market_data = {
            let sigs = cross_market_signals.unwrap_or_default();
            crate::signals::compute_cross_market_signal(&sigs)
        };

        // ── Latency arb: compare live BTC vs Polymarket YES token ─────────────
        let latency_arb = {
            let mut snapshots = Vec::new();
            if let Some((price, age_secs)) = crate::signals::price_bus::shared_price_bus()
                .snapshot_near("BTCUSDT", 30.0, 15.0)
                .await
            {
                snapshots.push(crate::bot::latency_arb::PriceSnapshot { price, age_secs });
            } else {
                let (btc_30s, btc_30s_ts) = *self.strategy_services.btc_px_30s_ago.read().await;
                let age = btc_30s_ts.elapsed().as_secs_f64();
                if btc_30s > 0.0 && (5.0..=60.0).contains(&age) {
                    snapshots.push(crate::bot::latency_arb::PriceSnapshot {
                        price: btc_30s,
                        age_secs: age,
                    });
                }
            }
            crate::bot::latency_arb::detect_latency_arb(
                btc_current,
                &snapshots,
                market_price,
                0.02, // polymarket spread estimate
            )
        };
        // Update btc_px_30s_ago if the stored snapshot is older than 10s (tighter for arb)
        {
            let age = self
                .strategy_services
                .btc_px_30s_ago
                .read()
                .await
                .1
                .elapsed()
                .as_secs_f64();
            if age >= 10.0 && btc_current > 0.0 {
                *self.strategy_services.btc_px_30s_ago.write().await =
                    (btc_current, std::time::Instant::now());
            }
        };

        // ── Step 14e: Compute microstructure VPIN from Binance trades ─────────
        let agg_trade_pairs = match signals::fetchers::fetch_binance_agg_trades(500).await {
            Ok(trades) => Some(
                trades
                    .iter()
                    .map(|t| (t.qty, t.is_buyer_maker))
                    .collect::<Vec<_>>(),
            ),
            Err(_) => None,
        };
        let microstructure_data = agg_trade_pairs.as_ref().map(|trade_pairs| {
            compute_microstructure(
                trade_pairs,
                trade_pairs,
                btc_current,
                vwap.vwap,
                50.0, // bucket size in BTC contracts
            )
        });
        let hawkes_flow_data = agg_trade_pairs
            .as_ref()
            .map(|trade_pairs| compute_hawkes_flow(trade_pairs));

        // ── Step 14f: Compute Polymarket flow data ─────────────────────────────
        let polymarket_flow_data = polymarket_flow_result.as_ref().map(|flow_raw| {
            compute_polymarket_flow(
                flow_raw.yes_volume,
                flow_raw.no_volume,
                flow_raw.price_start,
                flow_raw.price_now,
                flow_raw.large_fill_detected,
                flow_raw.elapsed_mins,
            )
        });

        // ── Step 14g: Whale data ───────────────────────────────────────────────
        let whale_data = if let Ok(ref w) = whale_result {
            Some(compute_whale_signal(w.inflow_usd, w.outflow_usd))
        } else {
            None
        };

        // ── Step 14h: Options advanced ────────────────────────────────────────
        let options_adv_data = if let Some(opt) = options_adv_result
            .as_ref()
            .ok()
            .filter(|opt| opt.is_fresh())
        {
            compute_options_advanced(opt.skew_25d, btc_current, opt.max_pain_price)
        } else {
            None
        };

        // ── Step 14b: OI momentum (aligns with BTC momentum) ─────────────────
        let oi_momentum_data: Option<crate::signals::OiMomentumData> =
            if cfg_snap.oi_momentum_enabled {
                Some(compute_oi_momentum(
                    open_interest,
                    prev_open_interest,
                    btc_momentum,
                ))
            } else {
                None
            };

        // ── Step 15: Price divergence ─────────────────────────────────────────
        let divergence = compute_price_divergence(btc_momentum, market_price);

        // ── Step 16: Phase decay ──────────────────────────────────────────────
        let phase = compute_phase_decay(time_remaining_minutes);

        // ── Step 16b: Granger causality — record observations & get weights ───
        let price_direction = if btc_momentum > 0.05 {
            1.0
        } else if btc_momentum < -0.05 {
            -1.0
        } else {
            0.0
        };
        let granger_weights = {
            let mut granger = self.strategy_services.granger.lock().await;
            granger.record(
                funding.rate.unwrap_or(0.0),
                ob_data.imbalance,
                aggressor_data.as_ref().map(|a| a.ratio).unwrap_or(0.5),
                microstructure_data.as_ref().map(|d| d.vpin).unwrap_or(0.5),
                tick_vel_data.velocity_60s,
                price_direction,
            );
            granger.weight_multipliers()
        };

        // ── Step 16b.2: Kyle's Lambda — price impact estimation ───────────
        let (kyle_signal, kyle_confidence, kyle_raw) = {
            let signed_volume = aggressor_data
                .as_ref()
                .map(|a| a.buy_vol - a.sell_vol)
                .unwrap_or(0.0);
            let price_change = btc_momentum / 100.0; // convert pct to fraction
            let mut tracker = self.strategy_services.kyle_tracker.lock().await;
            tracker.update(signed_volume, price_change);
            match tracker.estimate() {
                Some((lambda, r_sq)) => {
                    let (sig, conf) =
                        crate::signals::kyle_lambda::compute_kyle_signal(lambda, r_sq);
                    (sig, conf, lambda)
                }
                None => (0.0, 0.0, 0.0),
            }
        };

        // ── Step 16c: News NLP signal (rate-limited to every 5 minutes) ───
        let (news_signal, news_age_secs) = {
            let mut cache = self.strategy_services.news_cache.lock().await;
            let should_fetch = match cache.as_ref() {
                Some(c) => c.ts.elapsed() > Duration::from_secs(300),
                None => true,
            };
            if should_fetch {
                let api_key = std::env::var("CRYPTOPANIC_API_KEY").unwrap_or_default();
                if !api_key.is_empty() {
                    match crate::signals::fetch_crypto_news(&api_key).await {
                        Ok(headlines) => {
                            let classifier = crate::signals::NewsClassifier::new();
                            let sig = crate::signals::compute_news_signal(&headlines, &classifier);
                            tracing::info!(
                                "News signal: sentiment={:.3} event={:?} headlines={}",
                                sig.headline_sentiment,
                                sig.event_type,
                                sig.headlines.len()
                            );
                            *cache = Some(super::state::NewsCache {
                                ts: Instant::now(),
                                signal: sig,
                            });
                        }
                        Err(e) => {
                            warn!("News fetch failed: {e}");
                        }
                    }
                }
            }
            let fresh = cache
                .as_ref()
                .filter(|c| c.ts.elapsed() < Duration::from_secs(300));
            (
                fresh
                    .map(|c| c.signal.clone())
                    .unwrap_or_else(|| crate::signals::NewsSignal {
                        headline_sentiment: 0.0,
                        event_type: crate::signals::news_nlp::EventType::Normal,
                        urgency: 0.0,
                        source_count: 0,
                        headlines: Vec::new(),
                    }),
                fresh.map(|c| c.ts.elapsed().as_secs()),
            )
        };

        // ── Step 16d: Compute enrichment features ──────────────────────────
        let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
        let liq_summary = {
            let tracker = self.intelligence.liq_tracker.lock().await;
            tracker.volume_in_window(300) // 5-minute window
        };
        let depth_data = fetch_binance_depth().await.ok();
        let depth_imbalance = depth_data.as_ref().map_or(0.0, |d| d.imbalance_5);

        // BTC final-minute momentum: linear extrapolation of recent candles
        let btc_final_mom = if btc_start_price > 0.0 {
            compute_btc_final_momentum(&candles, btc_start_price, time_remaining_minutes)
        } else {
            None
        };

        if let Some(signal) = pcr_signal.as_mut() {
            refresh_pcr_freshness(signal, Utc::now().timestamp_millis());
        }
        let pcr_data = pcr_data.filter(|_| {
            pcr_signal
                .as_ref()
                .is_some_and(|signal| signal.freshness == SignalFreshness::Fresh)
        });
        // Other optional fetches can take time; check provider UTC freshness
        // again at the point the options values enter fusion/model features.
        let options_adv_data = options_adv_data
            .filter(|_| options_adv_result.as_ref().is_ok_and(|opt| opt.is_fresh()));
        let enrichment = EnrichmentData {
            hour_of_day: compute_hour_of_day(),
            day_of_week: compute_day_of_week(),
            distance_round_1000: compute_distance_from_round(btc_current, 1000.0),
            distance_round_500: compute_distance_from_round(btc_current, 500.0),
            volatility_10: compute_recent_volatility(&closes, 10),
            volatility_30: compute_recent_volatility(&closes, 30),
            window_momentum: btc_momentum,
            long_liq_volume: liq_summary.long_vol,
            short_liq_volume: liq_summary.short_vol,
            depth_imbalance_5: depth_imbalance,
            // Phase 2 signals
            polymarket_flow_signal: polymarket_flow_data
                .as_ref()
                .map(|d| d.signal)
                .unwrap_or(0.0),
            polymarket_flow_confidence: polymarket_flow_data
                .as_ref()
                .map(|d| d.confidence)
                .unwrap_or(0.0),
            vpin_signal: microstructure_data
                .as_ref()
                .map(|d| d.signal)
                .unwrap_or(0.0),
            vpin_confidence: microstructure_data
                .as_ref()
                .map(|d| d.confidence)
                .unwrap_or(0.0),
            options_signal: options_adv_data.as_ref().map(|d| d.signal).unwrap_or(0.0),
            options_confidence: options_adv_data
                .as_ref()
                .map(|d| d.confidence)
                .unwrap_or(0.0),
            whale_signal: whale_data.as_ref().map(|d| d.signal).unwrap_or(0.0),
            whale_confidence: whale_data.as_ref().map(|d| d.confidence).unwrap_or(0.0),
            // Granger causality adaptive weights
            granger_funding_rate: granger_weights.funding_rate,
            granger_orderbook_imbalance: granger_weights.orderbook_imbalance,
            granger_aggressor_ratio: granger_weights.aggressor_ratio,
            granger_vpin: granger_weights.vpin,
            granger_tick_velocity: granger_weights.tick_velocity,
            // Cross-market and latency arb (now integrated into fusion)
            cross_market_signal: cross_market_data.net_signal,
            cross_market_confidence: cross_market_data.confidence,
            latency_arb_signal: latency_arb.expected_direction
                * if latency_arb.arb_detected { 1.0 } else { 0.0 },
            latency_arb_confidence: latency_arb.confidence,
            // Staleness flags — mark signals that failed to fetch this cycle
            stale_options: options_adv_data.is_none(),
            stale_whale: whale_data.is_none(),
            stale_cross_market: cross_market_stale,
            stale_polymarket_flow: polymarket_flow_data.is_none(),
            // News NLP
            news_sentiment: news_signal.headline_sentiment,
            news_event_type: format!("{:?}", news_signal.event_type),
            // Raw VPIN for circuit breaker (0.0 when unavailable → breaker inactive)
            vpin_raw: microstructure_data.as_ref().map(|d| d.vpin).unwrap_or(0.0),
            // BTC final-minute momentum
            btc_final_momentum: btc_final_mom.as_ref().map(|d| d.signal).unwrap_or(0.0),
            btc_final_momentum_confidence: btc_final_mom
                .as_ref()
                .map(|d| d.confidence)
                .unwrap_or(0.0),
            // Kyle's Lambda price impact
            kyle_lambda: kyle_signal,
            kyle_lambda_confidence: kyle_confidence,
            kyle_lambda_raw: kyle_raw,
            // Hawkes-style clustered order flow
            hawkes_flow: hawkes_flow_data.as_ref().map(|d| d.signal).unwrap_or(0.0),
            hawkes_confidence: hawkes_flow_data
                .as_ref()
                .map(|d| d.confidence)
                .unwrap_or(0.0),
        };

        let signal_freshness_json = json!({
            "pcr": pcr_signal.as_ref().map(|s| s.status_json()),
            "fear_greed": fg_signal.as_ref().map(|s| s.status_json()),
            "lunarcrush": lunarcrush_signal.as_ref().map(|s| s.status_json()),
            "options_advanced": match &options_adv_result {
                Ok(opt) => json!({
                    "freshness": if options_adv_data.is_some() { "fresh" } else { "missing" },
                    "age_secs": opt.source_age_secs(),
                    "expiry_timestamp_ms": opt.expiry_timestamp_ms,
                    "instruments_in_expiry": opt.instruments_in_expiry,
                    "sampled_tickers": opt.sampled_tickers,
                    "scope": "single_expiry_skew_and_usd_intrinsic_max_pain_proxy",
                    "signed_gamma_exposure": null,
                    "gamma_detail": "Public OI does not reveal dealer long/short inventory",
                }),
                Err(error) => json!({"freshness": "missing", "age_secs": null, "detail": error.to_string()}),
            },
        });

        // ── Step 17: Fuse signals ─────────────────────────────────────────────
        let configured_weights = cfg_snap.signal_weights.as_map();
        let mut weights = configured_weights.clone();
        if cfg_snap.time_of_day_weight {
            apply_time_of_day_weighting(&mut weights, Utc::now().hour());
        }
        let trade_amount = cfg_snap.trade_amount;
        // Mode-aware min confidence: simulation trades more freely to generate
        // data for ML training and agent rebalancing. Live stays conservative.
        let base_min_conf = cfg_snap.strategy.params.min_confidence;
        let min_conf = match cfg_snap.mode.as_str() {
            "simulation" => base_min_conf.min(0.62),
            "shadow" | "canary" => base_min_conf.min(0.68),
            _ => base_min_conf, // live: use configured value (0.72)
        };
        let live_conf_cap = cfg_snap.live_confidence_cap;

        let mut result = fuse_signals(
            &ob_data,
            &tick_vel_data,
            &vwap,
            &rsi,
            &macd,
            &ha,
            &divergence,
            &funding,
            &regime,
            &phase,
            market_price,
            pcr_data.as_ref().filter(|_| {
                pcr_signal
                    .as_ref()
                    .is_some_and(|s| s.freshness == SignalFreshness::Fresh)
            }),
            fg_data.as_ref().filter(|_| {
                fg_signal
                    .as_ref()
                    .is_some_and(|s| s.freshness == SignalFreshness::Fresh)
            }),
            oi_momentum_data.as_ref(),
            aggressor_data.as_ref(),
            lunarcrush_data.as_ref().filter(|_| {
                lunarcrush_signal
                    .as_ref()
                    .is_some_and(|s| s.freshness == SignalFreshness::Fresh)
            }),
            Some(&cfg_snap.fusion_calibration),
            Some(weights),
            liquidity,
            trade_amount,
            obi_stable,
            &enrichment,
        );
        let breakdown_json = json!(result.breakdown.clone());
        let normalized_weights_json = json!(result.normalized_weights.clone());
        let weighted_contributions_json = json!(result.weighted_contributions.clone());
        let configured_weights_json = json!(configured_weights.clone());

        // ── Step 17a: Data logging for ML training ──────────────────────────
        {
            // Log aggressor flow
            if let Some(ref agg) = aggressor_data {
                self.intelligence
                    .data_logger
                    .log_aggressor(agg.buy_vol, agg.sell_vol);
            }
        }

        // ── Step 17b: Deep model inference + ensemble blending ──────────────
        let model_candles_1m = self.get_cached_model_candles_1m().await;
        for c in model_candles_1m
            .iter()
            .skip(model_candles_1m.len().saturating_sub(5))
        {
            let ts = Utc
                .timestamp_millis_opt(c.open_time_ms)
                .single()
                .unwrap_or_else(Utc::now)
                .to_rfc3339();
            self.intelligence.data_logger.log_candle_at(
                ts,
                c.open,
                c.high,
                c.low,
                c.close,
                c.volume,
                Some(""),
                Some(c.close),
            );
        }

        let model_input = {
            let mut builder = self.intelligence.input_builder.lock().await;
            builder.set_orderbook_market(&market.market_id);
            if let Some(raw) = raw_orderbook.as_ref().filter(|_| orderbook_available) {
                let mut levels = vec![0.0; 40];
                for (side, offset) in [(&raw.bids, 0), (&raw.asks, 20)] {
                    for (i, (price, size)) in side.iter().take(10).enumerate() {
                        levels[offset + i * 2] = *price as f32;
                        levels[offset + i * 2 + 1] = *size as f32;
                    }
                }
                builder.record_orderbook_snapshot(crate::ml::input_builder::ObSnapshot {
                    levels,
                    flow: aggressor_data
                        .as_ref()
                        .map_or([0.0, 0.0], |a| [a.buy_vol as f32, a.sell_vol as f32]),
                    liquidation: [
                        liq_summary.long_vol as f32,
                        liq_summary.short_vol as f32,
                        (liq_summary.long_vol - liq_summary.short_vol) as f32,
                    ],
                    ts: Instant::now(),
                });
            }

            builder.replace_candles(model_candles_1m.iter().map(|c| {
                crate::ml::input_builder::MiniCandle {
                    open: c.open as f32,
                    high: c.high as f32,
                    low: c.low as f32,
                    close: c.close as f32,
                    volume: c.volume as f32,
                    ts: Instant::now(),
                }
            }));

            // Update context vars
            builder.time_remaining_frac = (time_remaining_minutes / 15.0).clamp(0.0, 1.0) as f32;
            builder.btc_price = btc_current as f32;
            builder.contract_price = market_price as f32;

            // Set balance and position context
            let bal = self.balance().await;
            let initial_bal = {
                let cfg = self.control_plane.config.read().await;
                cfg.initial_balance.max(1.0)
            };
            builder.balance_frac = (bal / initial_bal) as f32;
            let open_count = {
                let trades = self.portfolio_state.trades.read().await;
                trades.iter().filter(|t| t.is_open()).count()
            };
            builder.open_positions = open_count as f32;

            // Record source availability rather than the fact that analysis ran.
            // 0=orderbook_imbalance, 1=tick_velocity, 2=vwap_confluence,
            // 3=price_divergence, 4=spike_detector, 6=rsi_macd_ha, 10=aggressor_ratio
            // 14=hour_of_day, 15=day_of_week, 16=dist_round_1000, 17=dist_round_500
            // 18=volatility_10, 19=volatility_30, 20=window_momentum
            // 21=long_liq_volume, 22=short_liq_volume, 23=depth_imbalance_5
            // 24=polymarket_flow, 25=vpin_microstructure, 26=options_advanced
            // 27=whale_flow, 28=cross_market, 29=latency_arb
            // 30=news_sentiment, 31=btc_final_momentum, 32=kyle_lambda, 33=hawkes_flow
            let candles_age = self
                .market_state
                .candles_cache
                .lock()
                .await
                .as_ref()
                .map_or(0, |c| c.ts.elapsed().as_secs());
            for (idx, available, age) in [
                (0, orderbook_available, 0),
                (1, orderbook_available, 0),
                (2, !candles.is_empty(), candles_age),
                (3, btc_current > 0.0 && btc_start_price > 0.0, 0),
                (6, candles.len() > 26, candles_age),
                (8, funding_rate.is_some(), 0),
                (
                    9,
                    open_interest.is_some() && prev_open_interest.is_some(),
                    0,
                ),
                (
                    10,
                    aggressor_data
                        .as_ref()
                        .is_some_and(|a| a.buy_vol + a.sell_vol > 0.0),
                    0,
                ),
                (14, true, 0),
                (15, true, 0),
                (16, btc_current > 0.0, 0),
                (17, btc_current > 0.0, 0),
                (18, candles.len() > 10, candles_age),
                (19, candles.len() > 30, candles_age),
                (20, btc_current > 0.0 && btc_start_price > 0.0, 0),
                (23, depth_data.is_some(), 0),
                (24, !enrichment.stale_polymarket_flow, 0),
                (25, microstructure_data.is_some(), 0),
                (
                    26,
                    !enrichment.stale_options,
                    options_adv_result
                        .as_ref()
                        .map_or(0, |opt| opt.source_age_secs()),
                ),
                (27, !enrichment.stale_whale, 0),
                (28, !enrichment.stale_cross_market, 0),
                (29, latency_arb.confidence > 0.0, 0),
                (30, news_age_secs.is_some(), news_age_secs.unwrap_or(0)),
                (31, btc_final_mom.is_some(), candles_age),
                (32, kyle_confidence > 0.0, 0),
                (33, hawkes_flow_data.is_some(), 0),
            ] {
                builder.staleness.record_availability(idx, available, age);
            }
            for (idx, signal) in [
                (5, pcr_signal.as_ref().map(|s| (s.freshness, s.age_secs))),
                (7, fg_signal.as_ref().map(|s| (s.freshness, s.age_secs))),
                (
                    13,
                    lunarcrush_signal
                        .as_ref()
                        .map(|s| (s.freshness, s.age_secs)),
                ),
            ] {
                let (available, age) = signal.map_or((false, 0), |(state, age)| {
                    (state == SignalFreshness::Fresh, age.unwrap_or(0))
                });
                builder.staleness.record_availability(idx, available, age);
            }

            builder.build(&result.breakdown, &regime)
        };

        let mut staleness_arr = [0.0f64; crate::ml::NUM_FEATURES];
        for (i, value) in model_input
            .staleness
            .iter()
            .take(crate::ml::NUM_FEATURES)
            .enumerate()
        {
            staleness_arr[i] = *value as f64;
        }

        let deep_model_output = {
            // Run inference if model is loaded
            let model_guard = self.intelligence.deep_model.read().await;
            if let Some(ref _model) = *model_guard {
                drop(model_guard);

                let mut model_guard = self.intelligence.deep_model.write().await;
                if let Some(ref mut model) = *model_guard {
                    match model.predict(&model_input) {
                        Ok(output) => Some(output),
                        Err(e) => {
                            tracing::warn!("Deep model inference failed: {e}");
                            None
                        }
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };

        // Keep the loaded generation stable until this forecast is bound and
        // applied. A replacement cannot inherit or accept an old forecast.
        let model_generation_guard = self.intelligence.deep_model.read().await;
        let deep_model_output = deep_model_output.filter(|output| {
            model_generation_guard
                .as_ref()
                .is_some_and(|model| model.identity == output.model_identity)
        });
        let mut deep_ml_info = serde_json::json!(null);
        if let Some(ref deep_out) = deep_model_output {
            let mut ensemble = self.intelligence.ensemble_state.write().await;
            if ensemble.bind_model(&deep_out.model_identity) {
                ensemble.save_to_disk();
            }
            let (blended_p_up, should_skip) = ensemble.blend(result.p_up, deep_out);
            ensemble.last_deep_p_up = Some(deep_out.p_up);
            ensemble.last_blended_p_up = Some(blended_p_up);
            ensemble.last_mc_uncertainty = deep_out.mc_uncertainty;
            ensemble.last_should_skip = Some(should_skip);
            ensemble
                .pending_predictions
                .insert(market.market_id.clone(), deep_out.p_up);
            deep_ml_info = serde_json::json!({
                "deep_p_up": deep_out.p_up,
                "deep_magnitude": null,
                "deep_volatility": null,
                "deep_time_to_move": null,
                "auxiliary_predictions_available": false,
                "auxiliary_unavailable_reason": "Forward auxiliary targets are not recorded; compatibility outputs are not forecasts",
                "mc_uncertainty": deep_out.mc_uncertainty,
                "model_version": deep_out.model_version,
                "model_identity": deep_out.model_identity,
                "ensemble_phase": ensemble.phase.as_str(),
                "deep_weight": ensemble.deep_weight,
                "blended_p_up": blended_p_up,
                "should_skip": should_skip,
            });
            if should_skip {
                result.signal = Signal::Hold;
                result.reason = "MC Dropout uncertainty too high — skipping".to_string();
            } else if ensemble.deep_weight > 0.001 {
                result.p_up = blended_p_up;
                realign_signal_with_posterior(&mut result, market_price, "ensemble_blend");
            }
        }
        drop(model_generation_guard);
        // The online LR calibrator is fitted to LR pre-update forecasts only.
        // Fusion/deep probabilities are calibrated by their own evidence, never
        // by that unrelated LR mapping (ml_predict applies it to LR metadata).

        refresh_posterior_economics(
            &mut result,
            market_price,
            trade_amount,
            liquidity,
            cfg_snap.fusion_calibration.min_net_ev,
        );

        // ── Step 18: BTC confirmation bias ──────────────────────────────────
        if result.signal != Signal::Hold {
            let signal_up = result.signal == Signal::BuyYes;
            let agrees =
                (signal_up && btc_prediction == "UP") || (!signal_up && btc_prediction == "DOWN");

            if agrees {
                result.confidence = (result.confidence * 1.05).min(live_conf_cap.max(0.92));
            } else if btc_confidence > 0.55 {
                // Soft penalty: BTC direction is one of 24 signals, not a veto.
                // 7% haircut is enough to flag disagreement without nuking
                // confidence below threshold every time.
                result.confidence *= 0.93;
            }

            // Re-check minimum confidence after adjustment
            if result.confidence < min_conf {
                result.signal = Signal::Hold;
                result.reason = format!(
                    "Post-BTC-adj confidence {:.0}% < min {:.0}%",
                    result.confidence * 100.0,
                    min_conf * 100.0
                );
            }
        }

        // ── Step 18a: Counterfactual scenario tree ────────────────────────────
        let scenario_tree = crate::signals::ScenarioTree::build(
            market_price,
            btc_momentum,
            &regime,
            time_remaining_minutes,
            result.confidence,
            enrichment.volatility_10, // Use realized 10-candle volatility
        );
        // If counterfactual analysis recommends Hold (no positive EV), downgrade signal.
        // However, when fusion confidence is strong (>78%), the counterfactual
        // softens to a confidence penalty rather than a full veto — the 24-signal
        // fusion engine has strong conviction and the scenario tree's tiny EV
        // calculation (especially near 50/50 prices) shouldn't completely override it.
        if result.signal != Signal::Hold {
            use crate::signals::ScenarioAction;
            let tree_action = scenario_tree.recommended_action;
            let signal_is_yes = result.signal == Signal::BuyYes;
            let tree_agrees = (signal_is_yes && tree_action == ScenarioAction::BuyYes)
                || (!signal_is_yes && tree_action == ScenarioAction::BuyNo);
            if tree_action == ScenarioAction::Hold {
                if result.confidence >= 0.78 {
                    // High confidence: penalize but don't veto
                    result.confidence *= 0.88;
                    tracing::info!(
                        "Counterfactual soft-veto: {} — high confidence ({:.0}%) overrides full block",
                        scenario_tree.summary(), result.confidence * 100.0
                    );
                    result.reason =
                        format!("CF soft-veto (conf penalized): {}", scenario_tree.summary());
                } else {
                    tracing::info!(
                        "Counterfactual: {} — scenario tree recommends Hold (yes_ev={:.1}% no_ev={:.1}%)",
                        scenario_tree.summary(), scenario_tree.yes_ev * 100.0, scenario_tree.no_ev * 100.0
                    );
                    result.signal = Signal::Hold;
                    result.reason = format!("Counterfactual: {}", scenario_tree.summary());
                }
            } else if !tree_agrees {
                // Mismatch: downweight confidence by 10%
                result.confidence *= 0.90;
                tracing::debug!(
                    "Counterfactual mismatch: signal downweighted to {:.0}%",
                    result.confidence * 100.0
                );
            }
        }

        if let Some(reason) = maker_candidate_confidence_guard(&cfg_snap, &mut result) {
            tracing::info!("Maker candidate stand-down: {}", reason);
        }

        // Apply this after every probability/confidence overlay. Missing core
        // observations must not become an entry through later ML blending.
        if !btc_current.is_finite()
            || btc_current <= 0.0
            || !btc_start_price.is_finite()
            || btc_start_price <= 0.0
            || candles.len() < 30
            || !orderbook_available
            || !result.p_up.is_finite()
            || !result.confidence.is_finite()
            || !result.net_ev.is_finite()
        {
            result.signal = Signal::Hold;
            result.confidence = 0.0;
            result.net_ev = 0.0;
            result.reason =
                "Required BTC price, reference, candles or orderbook unavailable — waiting"
                    .to_string();
        }

        self.intelligence.data_logger.log_features(
            &self.control_plane.run_id,
            &market.market_id,
            &result.breakdown,
            result.score,
            result.p_up,
            result.signal.as_str(),
            &regime,
            &phase.phase,
            Some(&staleness_arr),
        );
        self.intelligence.data_logger.log_model_input(
            &self.control_plane.run_id,
            &market.market_id,
            &result.breakdown,
            result.score,
            result.p_up,
            result.signal.as_str(),
            &regime,
            &phase.phase,
            &model_input,
        );

        // ── Step 18: Build typed output + API JSON projection ─────────────────
        ParsedAnalysis {
            signal: result.signal,
            confidence: result.confidence,
            raw_confidence: Some(result.confidence),
            net_ev: result.net_ev,
            volatility: Some(self.estimate_volatility(&candles)),
            reason: result.reason,
            regime,
            forced: false,
            p_up: Some(result.p_up),
            btc_change_pct: Some(btc_momentum),
            funding_rate,
            market: AnalysisMarketData {
                market_id: market.market_id.clone(),
                question: market.question.clone(),
                price: market_price,
                end_date: market.end_date.clone(),
                liquidity,
            },
            signals: Some(breakdown_json),
            extra: Some({
                // ML shadow prediction
                let ml_info = {
                    let ml = self.intelligence.ml_state.read().await;
                    let ml_p_up = crate::ml::ml_predict(&ml, &result.breakdown);
                    let ml_active = crate::ml::should_use_ml(&ml);
                    let ml_signal = if ml_p_up >= 0.5 { "BUY_YES" } else { "BUY_NO" };
                    tracing::debug!(
                        "ML shadow: p_up={ml_p_up:.3} ({ml_signal}) vs fusion p_up={:.3} | samples={} active={ml_active}",
                        result.p_up, ml.model.samples_seen,
                    );
                    json!({
                        "p_up": ml_p_up,
                        "signal": ml_signal,
                        "active": ml_active,
                        "samples_seen": ml.model.samples_seen,
                        "accuracy_20": ml.tracker.accuracy(20),
                        "accuracy_50": ml.tracker.accuracy(50),
                    })
                };
                // Pre-build sub-objects to reduce json! macro recursion depth
                let altcoin_json = altcoin_corr.as_ref().map(|c| json!({
                    "eth_change_pct": c.eth_change_pct,
                    "sol_change_pct": c.sol_change_pct,
                    "correlation_score": c.correlation_score,
                }));
                let cross_market_json = json!({
                    "net_signal": cross_market_data.net_signal,
                    "confidence": cross_market_data.confidence,
                    "signal_count": cross_market_data.signals.len(),
                });
                let latency_arb_json = json!({
                    "arb_detected": latency_arb.arb_detected,
                    "binance_move_pct": latency_arb.binance_move_pct,
                    "expected_direction": latency_arb.expected_direction,
                    "confidence": latency_arb.confidence,
                });
                let counterfactual_json = json!({
                    "yes_ev_pct": scenario_tree.yes_ev * 100.0,
                    "no_ev_pct": scenario_tree.no_ev * 100.0,
                    "max_loss_pct": scenario_tree.max_loss_yes_pct * 100.0,
                    "recommended_action": format!("{:?}", scenario_tree.recommended_action),
                    "has_catastrophic_risk": scenario_tree.has_catastrophic_risk(),
                    "scenarios": scenario_tree.scenarios.iter().map(|s| json!({
                        "name": s.name,
                        "btc_move_pct": s.btc_move_pct,
                        "yes_price": s.expected_yes_price,
                        "yes_pnl_pct": s.yes_pnl_pct,
                        "no_pnl_pct": s.no_pnl_pct,
                        "probability": s.probability,
                    })).collect::<Vec<_>>(),
                });
                let granger_json = json!({
                    "funding_rate": granger_weights.funding_rate,
                    "orderbook_imbalance": granger_weights.orderbook_imbalance,
                    "aggressor_ratio": granger_weights.aggressor_ratio,
                    "vpin": granger_weights.vpin,
                    "tick_velocity": granger_weights.tick_velocity,
                });
                let vpin_json = microstructure_data.as_ref().map(|d| json!({
                    "vpin": d.vpin,
                    "trade_imbalance": d.trade_imbalance_30s,
                    "vwap_deviation": d.vwap_deviation,
                    "signal": d.signal,
                    "confidence": d.confidence,
                }));
                let hawkes_json = hawkes_flow_data.as_ref().map(|d| json!({
                    "buy_intensity": d.buy_intensity,
                    "sell_intensity": d.sell_intensity,
                    "branching_ratio": d.branching_ratio,
                    "cluster_ratio": d.cluster_ratio,
                    "signal": d.signal,
                    "confidence": d.confidence,
                }));
                let cross_market_signals_json: Vec<Value> = cross_market_data.signals.iter().map(|s| json!({
                    "asset": s.asset,
                    "resolved_up": s.resolved_up,
                    "secs_ago": s.secs_since_resolution,
                })).collect();
                let market_data_json = json!({
                    "volume": market.volume,
                    "source": market.source,
                });
                json!({
                "fusion_score": result.score,
                "fusion_debug": {
                    "configured_weights": configured_weights_json,
                    "normalized_weights": normalized_weights_json,
                    "weighted_contributions": weighted_contributions_json,
                    "time_of_day_weighting_enabled": cfg_snap.time_of_day_weight,
                },
                "taker_fee": result.taker_fee,
                "lmsr_slippage": result.lmsr_slippage,
                "time_remaining_minutes": time_remaining_minutes,
                "minute_13_rule": false,
                "phase": phase.phase,
                "btc_price": btc_current,
                "btc_start_price": btc_start_price,
                "btc_start_source": btc_start_source,
                "price_to_beat_distance_pct": price_to_beat_distance_pct,
                "market_start_utc": market_start.to_rfc3339(),
                "btc_prediction": btc_prediction,
                "btc_confidence": btc_confidence,
                "vwap": vwap.vwap,
                "vwap_slope": vwap.slope,
                "rsi": rsi.rsi,
                "macd_hist": macd.hist,
                "ha_consecutive": ha.consecutive,
                "ob_imbalance": ob_data.imbalance,
                "obi_stable": obi_stable,
                "tick_velocity_60s": tick_vel_data.velocity_60s,
                "tick_velocity_30s": tick_vel_data.velocity_30s,
                "tick_acceleration": tick_vel_data.acceleration,
                "funding_rate": funding.rate,
                "open_interest": open_interest,
                "futures_basis_pct": futures_basis_pct,
                "futures_mark_price": futures_mark_price,
                "futures_index_price": futures_index_price,
                "oi_delta_pct": oi_momentum_data.as_ref().map(|o| o.delta_pct),
                "aggressor_ratio": aggressor_data.as_ref().map(|a| a.ratio),
                "aggressor_buy_vol": aggressor_data.as_ref().map(|a| a.buy_vol),
                "aggressor_sell_vol": aggressor_data.as_ref().map(|a| a.sell_vol),
                "pcr": pcr_data.as_ref().map(|p| p.pcr),
                "fear_greed_value": fg_data.as_ref().map(|f| f.value),
                "signal_freshness": signal_freshness_json,
                "coingecko_sentiment_up_pct": serde_json::Value::Null,
                "finnhub_sentiment_score": serde_json::Value::Null,
                "lunarcrush_sentiment_score": lunarcrush_data.as_ref().map(|l| l.score),
                "lunarcrush_social_volume": lunarcrush_data.as_ref().and_then(|l| l.social_volume),
                "altcoin_correlation": altcoin_json,
                "coinbase_divergence_pct": coinbase_divergence,
                "cross_market": cross_market_json,
                "cross_market_signals": cross_market_signals_json,
                "latency_arb": latency_arb_json,
                "data_source": "binance+polymarket",
                "market_data": market_data_json,
                "ml": ml_info,
                "deep_model": deep_ml_info,
                "counterfactual": counterfactual_json,
                "granger": granger_json,
                "vpin": vpin_json,
                "hawkes_flow": hawkes_json,
                "cross_market_net_signal": cross_market_data.net_signal,
                "latency_arb_detected": latency_arb.arb_detected,
                "news_sentiment": news_signal.headline_sentiment,
                "news_event_type": format!("{:?}", news_signal.event_type),
            })
            }),
        }
        .to_value()
    }

    #[cfg(test)]
    pub(crate) async fn analyze_market_from_seed(
        &self,
        all_markets: Vec<MarketData>,
        now: DateTime<Utc>,
    ) -> Value {
        let maker_min_remaining_secs = {
            let cfg = self.control_plane.config.read().await;
            if cfg.execution_mode.eq_ignore_ascii_case("maker") {
                cfg.maker_candidate_min_remaining_seconds as f64
            } else {
                0.0
            }
        };
        let mut candidates: Vec<(MarketData, chrono::DateTime<Utc>, f64, f64)> = Vec::new();
        let late = late_market_analysis(&all_markets, now, maker_min_remaining_secs);
        for m in all_markets {
            let start_dt = parse_utc(&m.start_date).or_else(|| {
                parse_utc(&m.end_date)
                    .map(|end| end - chrono::Duration::minutes(m.window_mins.round() as i64))
            });
            let start_dt = match start_dt {
                Some(dt) => dt,
                None => continue,
            };
            let end_dt = start_dt + chrono::Duration::minutes(m.window_mins.round() as i64);
            let secs_remaining = (end_dt - now).num_seconds().max(0) as f64;
            let elapsed_mins = (now - start_dt).num_seconds() as f64 / 60.0;
            let min_elapsed = 0.5;
            let max_elapsed = (m.window_mins - 0.1).max(min_elapsed);
            if (min_elapsed..max_elapsed).contains(&elapsed_mins)
                && secs_remaining > maker_min_remaining_secs
            {
                let window_mins = m.window_mins;
                candidates.push((m, end_dt, elapsed_mins, window_mins));
            }
        }

        candidates.sort_by(|a, b| {
            let target_a = a.3 * 0.4;
            let target_b = b.3 * 0.4;
            (a.2 - target_a)
                .abs()
                .partial_cmp(&(b.2 - target_b).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let (market, end_dt, _, _) = match candidates.into_iter().next() {
            Some(c) => c,
            None => {
                if let Some(late) = late {
                    return late;
                }
                return ParsedAnalysis {
                    reason: "No active BTC Up/Down 15-minute market in current window — waiting"
                        .to_string(),
                    extra: Some(json!({
                        "minute_13_rule": false,
                        "time_remaining_minutes": 0.0,
                        "waiting_for_window": true,
                        "market_data": Value::Null
                    })),
                    ..ParsedAnalysis::default()
                }
                .to_value();
            }
        };

        let secs_remaining = (end_dt - now).num_seconds().max(0) as f64;
        let time_remaining_minutes = secs_remaining / 60.0;
        if time_remaining_minutes <= 2.0 {
            return self
                .minute_13_signal(&market, market.price, time_remaining_minutes)
                .await
                .to_value();
        }

        json!({
            "signal": "HOLD",
            "reason": "Seeded test path only covers minute-13 deterministic branch",
            "confidence": 0.0,
            "market_data": {
                "market_id": market.market_id,
                "question": market.question,
                "price": market.price,
                "end_date": market.end_date,
            },
            "minute_13_rule": false,
            "time_remaining_minutes": time_remaining_minutes,
        })
    }

    // ── Minute-13 momentum-only signal ───────────────────────────────────────

    async fn minute_13_signal(
        &self,
        market: &MarketData,
        market_price: f64,
        time_remaining: f64,
    ) -> ParsedAnalysis {
        // The market price alone supplies no probability edge over that price.
        // Preserve the observed YES probability; do not invent profit by measuring
        // its distance from 0.50 or by reversing the NO probability.
        let signal = Signal::Hold;
        let reason = "Late-window market price has no independent probability edge — waiting";
        let raw_edge = 0.0;
        let taker_fee = crate::fusion::estimate_taker_fee(market_price);
        let net_ev = -taker_fee;
        let p_up = market_price.clamp(0.0, 1.0);

        let market_start = parse_utc(&market.start_date).or_else(|| {
            parse_utc(&market.end_date)
                .map(|end| end - chrono::Duration::minutes(market.window_mins.round() as i64))
        });
        let (btc_start_price, btc_start_source, btc_current, price_to_beat_distance_pct) =
            if let Some(market_start) = market_start {
                let (start_price, start_source) = if let Some(cached) =
                    self.market_state.btc_start_cache.get(&market.market_id)
                {
                    (cached.1, cached.2.clone())
                } else {
                    let (p, src) = if let Some(ref_price) = market.btc_ref_price {
                        (ref_price, "oracle".to_string())
                    } else {
                        let exact = self.get_btc_price_at_start(market_start).await;
                        let prior = self
                            .get_btc_price_at_start(market_start - chrono::Duration::seconds(60))
                            .await;
                        if exact > 0.0 {
                            (exact, format!("binance@{}", market_start.format("%H:%M")))
                        } else if prior > 0.0 {
                            (
                                prior,
                                format!("binance@{}-1m", market_start.format("%H:%M")),
                            )
                        } else {
                            (0.0, "none".to_string())
                        }
                    };
                    if p > 0.0 {
                        self.market_state
                            .btc_start_cache
                            .insert(market.market_id.clone(), (Instant::now(), p, src.clone()));
                    }
                    (p, src)
                };
                let current = BTCAnalyzer::get_price().await.unwrap_or(0.0);
                let distance_pct = if start_price > 0.0 && current > 0.0 {
                    Some((current - start_price) / start_price * 100.0)
                } else {
                    None
                };
                (start_price, start_source, current, distance_pct)
            } else {
                (0.0, "none".to_string(), 0.0, None)
            };
        let (_, _, _, futures_basis_pct, futures_mark_price, futures_index_price) =
            self.get_cached_futures().await;

        ParsedAnalysis {
            signal,
            confidence: if signal == Signal::Hold { 0.0 } else { 0.75 },
            raw_confidence: Some(if signal == Signal::Hold { 0.0 } else { 0.75 }),
            net_ev,
            p_up: Some(p_up),
            reason: reason.to_string(),
            market: AnalysisMarketData {
                market_id: market.market_id.clone(),
                question: market.question.clone(),
                price: market_price,
                end_date: market.end_date.clone(),
                liquidity: market.liquidity,
            },
            extra: Some(json!({
                "fusion_score": 0.0,
                "minute_13_rule": true,
                "time_remaining_minutes": time_remaining,
                "phase": "LATE",
                "data_source": "price_momentum",
                "net_ev": net_ev,
                "raw_edge": raw_edge,
                "taker_fee": taker_fee,
                "btc_price": btc_current,
                "btc_start_price": if btc_start_price > 0.0 { Some(btc_start_price) } else { None::<f64> },
                "btc_start_source": btc_start_source,
                "price_to_beat_distance_pct": price_to_beat_distance_pct,
                "market_start_utc": market_start.map(|dt| dt.to_rfc3339()),
                "futures_basis_pct": futures_basis_pct,
                "futures_mark_price": futures_mark_price,
                "futures_index_price": futures_index_price,
            })),
            ..ParsedAnalysis::default()
        }
    }

    // ── Cache helpers ─────────────────────────────────────────────────────────

    pub(crate) async fn get_cached_candles(&self) -> Vec<Candle> {
        const TTL: Duration = Duration::from_secs(300); // 5 min

        // Fast path: return cached value without doing network I/O under lock.
        {
            let guard = self.market_state.candles_cache.lock().await;
            if let Some(ref c) = *guard {
                if c.ts.elapsed() < TTL {
                    return c.candles.clone();
                }
            }
        }

        // Fetch fresh outside lock to avoid head-of-line blocking.
        let fresh = match fetch_binance_candles(100).await {
            Ok(raw) => raw
                .into_iter()
                .map(|b| Candle {
                    open: b.open,
                    high: b.high,
                    low: b.low,
                    close: b.close,
                    volume: b.volume,
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                warn!("Candles fetch error: {e}");
                vec![]
            }
        };

        // Double-check after fetch: if another task refreshed meanwhile, use it.
        let mut guard = self.market_state.candles_cache.lock().await;
        if let Some(ref c) = *guard {
            if c.ts.elapsed() < TTL {
                return c.candles.clone();
            }
        }
        *guard = Some(CandlesCache {
            ts: Instant::now(),
            candles: fresh.clone(),
        });
        fresh
    }

    pub(crate) async fn get_cached_model_candles_1m(
        &self,
    ) -> Vec<crate::signals::fetchers::BinanceCandle> {
        const TTL: Duration = Duration::from_secs(45);

        {
            let guard = self.market_state.ml_candles_cache.lock().await;
            if let Some(ref c) = *guard {
                if c.ts.elapsed() < TTL {
                    return c.candles.clone();
                }
            }
        }

        let fresh = match fetch_binance_candles_1m(240).await {
            Ok(raw) => raw,
            Err(e) => {
                warn!("1m model candles fetch error: {e}");
                vec![]
            }
        };

        let mut guard = self.market_state.ml_candles_cache.lock().await;
        *guard = Some(MlCandlesCache {
            ts: Instant::now(),
            candles: fresh.clone(),
        });
        fresh
    }

    pub(crate) async fn get_cached_futures(
        &self,
    ) -> (
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
    ) {
        const TTL: Duration = Duration::from_secs(300);

        {
            let guard = self.market_state.futures_cache.lock().await;
            if let Some(ref c) = *guard {
                if c.ts.elapsed() < TTL {
                    return (
                        c.funding_rate,
                        c.open_interest,
                        c.prev_open_interest,
                        c.basis_pct,
                        c.mark_price,
                        c.index_price,
                    );
                }
            }
        }

        let (fr, oi, basis_pct, mark_price, index_price) = match fetch_binance_futures().await {
            Ok(f) => (
                f.funding_rate,
                f.open_interest,
                f.basis_pct,
                f.mark_price,
                f.index_price,
            ),
            Err(e) => {
                warn!("Futures fetch error: {e}");
                (None, None, None, None, None)
            }
        };

        let mut guard = self.market_state.futures_cache.lock().await;
        if let Some(ref c) = *guard {
            if c.ts.elapsed() < TTL {
                return (
                    c.funding_rate,
                    c.open_interest,
                    c.prev_open_interest,
                    c.basis_pct,
                    c.mark_price,
                    c.index_price,
                );
            }
        }
        let prev = guard.as_ref().and_then(|c| c.open_interest);
        *guard = Some(FuturesCache {
            ts: Instant::now(),
            funding_rate: fr,
            open_interest: oi,
            prev_open_interest: prev,
            basis_pct,
            mark_price,
            index_price,
        });
        (fr, oi, prev, basis_pct, mark_price, index_price)
    }

    async fn get_cached_pcr(&self) -> CachedSignal<crate::signals::fetchers::PcrOpenInterest> {
        const TTL: Duration = Duration::from_secs(300);

        {
            let guard = self.market_state.pcr_cache.lock().await;
            if let Some(ref c) = *guard {
                if let Some(age) = crate::signals::deribit::pcr_source_age_secs(
                    c.provider_timestamp_ms,
                    Utc::now().timestamp_millis(),
                )
                .filter(|_| c.ts.elapsed() < TTL)
                {
                    return CachedSignal::fresh(
                        crate::signals::fetchers::PcrOpenInterest {
                            put_oi: c.put_oi,
                            call_oi: c.call_oi,
                            provider_timestamp_ms: c.provider_timestamp_ms,
                        },
                        Some(age),
                        "cache_hit",
                    );
                }
            }
        }

        let summary = match fetch_deribit_pcr().await {
            Ok(v) => v,
            Err(e) => {
                warn!("PCR fetch error: {e}");
                let guard = self.market_state.pcr_cache.lock().await;
                if let Some(ref c) = *guard {
                    let age = Utc::now()
                        .timestamp_millis()
                        .saturating_sub(c.provider_timestamp_ms)
                        .max(0) as u64
                        / 1000;
                    return CachedSignal::fallback(
                        crate::signals::fetchers::PcrOpenInterest {
                            put_oi: c.put_oi,
                            call_oi: c.call_oi,
                            provider_timestamp_ms: c.provider_timestamp_ms,
                        },
                        Some(age),
                        format!("fetch_error_fallback: {e}"),
                    );
                }
                return CachedSignal::missing(
                    crate::signals::fetchers::PcrOpenInterest::default(),
                    format!("fetch_error_missing: {e}"),
                );
            }
        };

        let mut guard = self.market_state.pcr_cache.lock().await;
        if let Some(ref c) = *guard {
            if let Some(age) = crate::signals::deribit::pcr_source_age_secs(
                c.provider_timestamp_ms,
                Utc::now().timestamp_millis(),
            )
            .filter(|_| c.ts.elapsed() < TTL)
            {
                return CachedSignal::fresh(
                    crate::signals::fetchers::PcrOpenInterest {
                        put_oi: c.put_oi,
                        call_oi: c.call_oi,
                        provider_timestamp_ms: c.provider_timestamp_ms,
                    },
                    Some(age),
                    "cache_race_hit",
                );
            }
        }
        let Some(age) = crate::signals::deribit::pcr_source_age_secs(
            summary.provider_timestamp_ms,
            Utc::now().timestamp_millis(),
        ) else {
            return CachedSignal::missing(
                crate::signals::fetchers::PcrOpenInterest::default(),
                "PCR source became stale during collection",
            );
        };
        *guard = Some(PcrCache {
            ts: Instant::now(),
            put_oi: summary.put_oi,
            call_oi: summary.call_oi,
            provider_timestamp_ms: summary.provider_timestamp_ms,
        });
        CachedSignal::fresh(summary, Some(age), "live_fetch")
    }

    async fn get_cached_fg(&self) -> CachedSignal<u32> {
        const TTL: Duration = Duration::from_secs(3600); // 1 hr

        {
            let guard = self.market_state.fg_cache.lock().await;
            if let Some(ref c) = *guard {
                if c.ts.elapsed() < TTL {
                    return CachedSignal::fresh(
                        c.value,
                        Some(c.ts.elapsed().as_secs()),
                        "cache_hit",
                    );
                }
            }
        }

        let value = match fetch_fear_greed().await {
            Ok(v) => v,
            Err(e) => {
                warn!("Fear&Greed fetch error: {e}");
                let guard = self.market_state.fg_cache.lock().await;
                if let Some(ref c) = *guard {
                    return CachedSignal::fallback(
                        c.value,
                        Some(c.ts.elapsed().as_secs()),
                        format!("fetch_error_fallback: {e}"),
                    );
                }
                return CachedSignal::missing(50, format!("fetch_error_missing: {e}"));
            }
        };

        let mut guard = self.market_state.fg_cache.lock().await;
        if let Some(ref c) = *guard {
            if c.ts.elapsed() < TTL {
                return CachedSignal::fresh(
                    c.value,
                    Some(c.ts.elapsed().as_secs()),
                    "cache_race_hit",
                );
            }
        }
        *guard = Some(FgCache {
            ts: Instant::now(),
            value,
        });
        CachedSignal::fresh(value, Some(0), "live_fetch")
    }

    pub(crate) async fn get_cached_aggressor(&self) -> (f64, f64) {
        const TTL: Duration = Duration::from_secs(15);

        {
            let guard = self.market_state.aggressor_cache.lock().await;
            if let Some(ref c) = *guard {
                if c.ts.elapsed() < TTL {
                    return (c.buy_vol, c.sell_vol);
                }
            }
        }

        let (buy_vol, sell_vol) = match signals::fetchers::fetch_binance_agg_trades(200).await {
            Ok(trades) => {
                let mut buy = 0.0;
                let mut sell = 0.0;
                for t in trades {
                    if t.is_buyer_maker {
                        sell += t.qty;
                    } else {
                        buy += t.qty;
                    }
                }
                (buy, sell)
            }
            Err(e) => {
                warn!("Aggressor fetch error: {e}");
                (0.0, 0.0)
            }
        };

        let mut guard = self.market_state.aggressor_cache.lock().await;
        if let Some(ref c) = *guard {
            if c.ts.elapsed() < TTL {
                return (c.buy_vol, c.sell_vol);
            }
        }
        *guard = Some(crate::bot::state::AggressorCache {
            ts: Instant::now(),
            buy_vol,
            sell_vol,
        });
        (buy_vol, sell_vol)
    }

    async fn get_cached_lunarcrush(
        &self,
        api_key: &str,
    ) -> CachedSignal<(Option<f64>, Option<f64>)> {
        const TTL: Duration = Duration::from_secs(600);

        {
            let guard = self.market_state.lunarcrush_cache.lock().await;
            if let Some(ref c) = *guard {
                if c.ts.elapsed() < TTL {
                    return CachedSignal::fresh(
                        (Some(c.score), c.social_volume),
                        Some(c.ts.elapsed().as_secs()),
                        "cache_hit",
                    );
                }
            }
        }

        let (score, social_volume) = match signals::fetchers::fetch_lunarcrush_sentiment(api_key)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                warn!("LunarCrush sentiment fetch error: {e}");
                let guard = self.market_state.lunarcrush_cache.lock().await;
                if let Some(ref c) = *guard {
                    return CachedSignal::fallback(
                        (Some(c.score), c.social_volume),
                        Some(c.ts.elapsed().as_secs()),
                        format!("fetch_error_fallback: {e}"),
                    );
                }
                return CachedSignal::missing((None, None), format!("fetch_error_missing: {e}"));
            }
        };

        if let Some(score) = score {
            let mut guard = self.market_state.lunarcrush_cache.lock().await;
            if let Some(ref c) = *guard {
                if c.ts.elapsed() < TTL {
                    return CachedSignal::fresh(
                        (Some(c.score), c.social_volume),
                        Some(c.ts.elapsed().as_secs()),
                        "cache_race_hit",
                    );
                }
            }
            *guard = Some(crate::bot::state::LunarcrushCache {
                ts: Instant::now(),
                score,
                social_volume,
            });
            return CachedSignal::fresh((Some(score), social_volume), Some(0), "live_fetch");
        }

        let guard = self.market_state.lunarcrush_cache.lock().await;
        if let Some(ref c) = *guard {
            return CachedSignal::fallback(
                (Some(c.score), c.social_volume),
                Some(c.ts.elapsed().as_secs()),
                "empty_fetch_fallback_cache",
            );
        }
        CachedSignal::missing((score, social_volume), "empty_fetch_missing")
    }

    // ── BTC helpers ───────────────────────────────────────────────────────────

    async fn get_btc_price_at_start(&self, market_start: DateTime<Utc>) -> f64 {
        // Fetch the 1-minute candle that opened at market_start from Binance.
        let start_ms = market_start.timestamp_millis();
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&limit=2&startTime={}",
            start_ms
        );
        if let Ok(r) = HTTP.get(&url).send().await {
            if r.status().is_success() {
                if let Ok(v) = r.bounded_json::<Value>().await {
                    if let Some(p) = historical_candle_open(&v, start_ms) {
                        return p;
                    }
                }
            }
        }
        // A current quote cannot recover the price at the window's start.
        0.0
    }

    fn btc_trend(&self, momentum_pct: f64) -> (String, f64) {
        if momentum_pct > 0.30 {
            let conf = (0.55 + (momentum_pct / 0.50).min(1.0) * 0.20).min(0.85);
            ("UP".to_string(), conf)
        } else if momentum_pct < -0.30 {
            let conf = (0.55 + (momentum_pct.abs() / 0.50).min(1.0) * 0.20).min(0.85);
            ("DOWN".to_string(), conf)
        } else {
            ("NEUTRAL".to_string(), 0.50)
        }
    }

    fn estimate_volatility(&self, candles: &[Candle]) -> f64 {
        if candles.len() < 2 {
            return 0.0;
        }
        let returns: Vec<f64> = candles
            .windows(2)
            .filter_map(|w| {
                if w[0].close > 0.0 {
                    Some(((w[1].close / w[0].close).ln()).powi(2))
                } else {
                    None
                }
            })
            .collect();
        if returns.is_empty() {
            return 0.0;
        }
        let variance = returns.iter().sum::<f64>() / returns.len() as f64;
        variance.sqrt()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::Utc;

    use super::{
        historical_candle_open, maker_candidate_confidence_guard, realign_signal_with_posterior,
        refresh_pcr_freshness, refresh_posterior_economics, CachedSignal, MarketData,
        SignalFreshness, TradingBot,
    };
    use crate::domain::Signal;
    use crate::fusion::FusionResult;

    #[test]
    fn posterior_realignment_flips_stale_buy_no_to_buy_yes() {
        let mut result = FusionResult {
            signal: Signal::BuyNo,
            score: 60.0,
            confidence: 0.74,
            reason: "test".to_string(),
            net_ev: 0.01,
            taker_fee: 0.0,
            lmsr_slippage: 0.0,
            p_up: 0.61,
            breakdown: HashMap::new(),
            normalized_weights: HashMap::new(),
            weighted_contributions: HashMap::new(),
        };

        assert!(realign_signal_with_posterior(&mut result, 0.5, "unit_test"));
        assert_eq!(result.signal, Signal::BuyYes);
        assert!(result.reason.contains("posterior realigned"));
    }

    #[test]
    fn posterior_realignment_leaves_hold_untouched() {
        let mut result = FusionResult {
            signal: Signal::Hold,
            score: 0.0,
            confidence: 0.0,
            reason: "hold".to_string(),
            net_ev: 0.0,
            taker_fee: 0.0,
            lmsr_slippage: 0.0,
            p_up: 0.72,
            breakdown: HashMap::new(),
            normalized_weights: HashMap::new(),
            weighted_contributions: HashMap::new(),
        };

        assert!(!realign_signal_with_posterior(
            &mut result,
            0.5,
            "unit_test"
        ));
        assert_eq!(result.signal, Signal::Hold);
    }

    #[test]
    fn posterior_side_compares_probability_with_entry_price_and_refreshes_economics() {
        let mut result = FusionResult {
            signal: Signal::BuyYes,
            p_up: 0.7,
            net_ev: 0.9,
            confidence: 0.74,
            score: 60.0,
            reason: "test".into(),
            taker_fee: 0.0,
            lmsr_slippage: 0.0,
            breakdown: HashMap::new(),
            normalized_weights: HashMap::new(),
            weighted_contributions: HashMap::new(),
        };
        assert!(realign_signal_with_posterior(&mut result, 0.8, "unit_test"));
        assert_eq!(result.signal, Signal::BuyNo);
        refresh_posterior_economics(&mut result, 0.8, 10.0, 1000.0, 0.05);
        assert!(result.net_ev > 0.4 && result.net_ev < 0.5);
        result.p_up = 0.799;
        refresh_posterior_economics(&mut result, 0.8, 10.0, 1000.0, 0.05);
        assert_eq!(result.signal, Signal::Hold);
        assert!(result.net_ev < 0.05);
    }

    #[test]
    fn a_later_candle_cannot_replace_a_missing_window_start() {
        assert_eq!(
            historical_candle_open(&serde_json::json!([[60000, "65000"]]), 60000),
            Some(65000.0)
        );
        assert_eq!(
            historical_candle_open(&serde_json::json!([[120000, "65000"]]), 60000),
            None
        );
        assert_eq!(
            historical_candle_open(&serde_json::json!([[60000, "NaN"]]), 60000),
            None
        );
        assert_eq!(
            historical_candle_open(&serde_json::json!([[60000]]), 60000),
            None
        );
    }

    #[tokio::test]
    async fn maker_seeded_analysis_waits_when_only_near_expiry_market_exists() -> anyhow::Result<()>
    {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_maker_seed_wait_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "maker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = true;
        cfg.auto_trading = true;
        cfg.maker_candidate_min_remaining_seconds = 300;

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        let now = Utc::now();
        let market = MarketData {
            market_id: "mkt-maker-near-expiry".to_string(),
            question: "BTC Up or Down seeded maker".to_string(),
            price: 0.74,
            volume: 0.0,
            liquidity: 10_000.0,
            start_date: (now - chrono::Duration::seconds(810)).to_rfc3339(),
            end_date: (now + chrono::Duration::seconds(90)).to_rfc3339(),
            window_mins: 15.0,
            source: "seed-test".to_string(),
            btc_ref_price: None,
        };

        let analysis = bot.analyze_market_from_seed(vec![market], now).await;
        // The market is there and the maker is choosing not to take it: say
        // so, and keep the market on the analysis so the page can show it.
        assert_eq!(
            analysis.get("reason").and_then(|v| v.as_str()),
            Some("Market has 90s left, under the maker minimum of 300s — waiting for the next window")
        );
        assert_eq!(
            analysis.get("waiting_for_window").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            analysis.get("too_late_for_maker").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            analysis.get("signal").and_then(|v| v.as_str()),
            Some("HOLD")
        );
        assert_eq!(
            analysis
                .pointer("/market_data/market_id")
                .and_then(|v| v.as_str()),
            Some("mkt-maker-near-expiry")
        );
        assert!(
            (analysis
                .get("time_remaining_minutes")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                - 1.5)
                .abs()
                < 0.05
        );

        Ok(())
    }

    #[tokio::test]
    async fn analyze_seed_execute_partial_full_cycle_is_consistent() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_analyze_cycle_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.auto_trading = true;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.9;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        bot.start_execution_actor().await;

        let initial = bot.balance().await;
        let now = Utc::now();
        let market = MarketData {
            market_id: "mkt-seeded-1".to_string(),
            question: "BTC Up or Down seeded".to_string(),
            price: 0.74,
            volume: 0.0,
            liquidity: 10_000.0,
            start_date: (now - chrono::Duration::seconds(810)).to_rfc3339(),
            end_date: (now + chrono::Duration::seconds(90)).to_rfc3339(),
            window_mins: 15.0,
            source: "seed-test".to_string(),
            btc_ref_price: None,
        };

        let mut analysis = bot.analyze_market_from_seed(vec![market], now).await;
        assert_eq!(
            analysis.get("minute_13_rule").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            analysis.get("signal").and_then(|v| v.as_str()),
            Some("HOLD")
        );
        assert_eq!(analysis.get("p_up").and_then(|v| v.as_f64()), Some(0.74));
        assert!(analysis.get("net_ev").and_then(|v| v.as_f64()).unwrap() < 0.0);
        // The accounting cycle below uses an explicitly forced synthetic order,
        // rather than claiming that a price-only late signal establishes an edge.
        if let Some(obj) = analysis.as_object_mut() {
            obj.insert("forced".to_string(), serde_json::Value::Bool(true));
            obj.insert("signal".to_string(), serde_json::json!("BUY_YES"));
            obj.insert("confidence".to_string(), serde_json::json!(0.9));
            obj.insert("p_up".to_string(), serde_json::json!(0.9));
            obj.insert("net_ev".to_string(), serde_json::json!(0.1));
        }

        let opened = bot
            .execute_trade_serialized_result(&analysis, false, Some(1.0))
            .await?
            .expect("trade should open from seeded analysis");
        assert!(opened.is_open());

        let partial = bot
            .partial_close_trade(opened.id, 0.80, 0.5)
            .await
            .expect("partial close should succeed");
        let remaining_amount = partial
            .get("remaining_amount")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!((remaining_amount - opened.amount * 0.5).abs() < 1e-9);

        let closed = bot
            .close_trade_serialized(opened.id, Some(0.90))
            .await
            .expect("full close should succeed");
        assert_eq!(closed.status_kind(), crate::domain::TradeStatus::Closed);

        let final_bal = bot.balance().await;
        let expected_final = initial + closed.pnl.unwrap_or(0.0) - closed.fee_paid;
        assert!((final_bal - expected_final).abs() < 1e-9);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[test]
    fn maker_candidate_confidence_guard_holds_low_confidence_maker_signal() {
        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "maker".to_string();
        cfg.min_confidence_margin = 0.03;

        let mut result = crate::fusion::FusionResult {
            signal: Signal::BuyYes,
            score: 1.0,
            confidence: 0.64,
            reason: "fusion ok".to_string(),
            net_ev: 0.02,
            taker_fee: 0.0,
            lmsr_slippage: 0.0,
            p_up: 0.64,
            breakdown: std::collections::HashMap::new(),
            normalized_weights: std::collections::HashMap::new(),
            weighted_contributions: std::collections::HashMap::new(),
        };

        let reason = maker_candidate_confidence_guard(&cfg, &mut result)
            .expect("maker guard should block sub-floor confidence");
        assert_eq!(result.signal, Signal::Hold);
        assert!(reason.contains("maker candidate confidence"));
        assert!(result.reason.contains("maker candidate confidence"));
    }

    #[test]
    fn pcr_provider_can_age_out_during_other_fetches_without_becoming_neutral() {
        let now_ms = 1_800_000_000_000;
        let mut signal = CachedSignal::fresh(
            crate::signals::fetchers::PcrOpenInterest {
                put_oi: 2.0,
                call_oi: 1.0,
                provider_timestamp_ms: now_ms - 299_000,
            },
            Some(299),
            "live_fetch",
        );
        refresh_pcr_freshness(&mut signal, now_ms + 2_000);
        assert_eq!(signal.freshness, SignalFreshness::Fallback);
        assert_eq!(signal.age_secs, Some(301));
        let unavailable = (signal.freshness == SignalFreshness::Fresh)
            .then(|| crate::signals::compute_deribit_pcr(signal.value.put_oi, signal.value.call_oi))
            .flatten();
        assert!(unavailable.is_none());
        let mut missing = CachedSignal::missing(
            crate::signals::fetchers::PcrOpenInterest::default(),
            "fetch_error",
        );
        refresh_pcr_freshness(&mut missing, now_ms);
        assert_eq!(missing.freshness, SignalFreshness::Missing);
        assert_eq!(missing.age_secs, None);
    }

    #[test]
    fn cached_signal_status_json_distinguishes_fresh_fallback_and_missing() {
        let fresh = CachedSignal::fresh((1.0, 2.0), Some(0), "live_fetch");
        let fallback = CachedSignal::fallback((3.0, 4.0), Some(17), "fetch_error_fallback");
        let missing = CachedSignal::missing((0.0, 0.0), "fetch_error_missing");

        assert_eq!(fresh.status_json()["freshness"], "fresh");
        assert_eq!(fallback.status_json()["freshness"], "fallback");
        assert_eq!(missing.status_json()["freshness"], "missing");
        assert_eq!(fresh.status_json()["age_secs"], 0);
        assert_eq!(fallback.status_json()["age_secs"], 17);
        assert!(missing.status_json()["age_secs"].is_null());
    }
}
