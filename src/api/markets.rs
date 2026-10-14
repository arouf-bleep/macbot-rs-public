//! Market data handlers — proxies Polymarket Gamma + CLOB APIs.

use crate::http_client::JsonResponseExt;
use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Utc};
use chrono_tz::America::New_York;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::bot::state::TradingBot;
use crate::http_client::HTTP;

// ── Constants ─────────────────────────────────────────────────────────────────

const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const CLOB_BASE: &str = "https://clob.polymarket.com";
const DEFAULT_TARGET_WINDOW_MINS: f64 = 15.0;
const DEFAULT_WINDOW_TOLERANCE_MINS: f64 = 1.0;
const NO_ARB_VIOLATION_THRESHOLD_CENTS: f64 = 1.0;

fn env_f64(key: &str, default: f64) -> f64 {
    crate::env_util::env_f64(key, default)
}

fn target_window_mins() -> f64 {
    env_f64("MACBOT_TARGET_WINDOW_MINS", DEFAULT_TARGET_WINDOW_MINS).clamp(1.0, 1440.0)
}

fn window_tolerance_mins() -> f64 {
    env_f64(
        "MACBOT_WINDOW_TOLERANCE_MINS",
        DEFAULT_WINDOW_TOLERANCE_MINS,
    )
    .max(0.1)
}

fn parse_btc_price_from_text(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut i = 0usize;
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
                if let Ok(price) = s.parse::<f64>() {
                    if price > 5_000.0 && price < 10_000_000.0 {
                        return Some(price);
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

fn is_btc_related_market(slug: &str, title: &str, question: &str) -> bool {
    let haystack = format!("{slug} {title} {question}").to_lowercase();
    haystack.contains("btc") || haystack.contains("bitcoin")
}

#[derive(Debug, Clone, Serialize)]
struct NoArbMarketEntry {
    market_id: String,
    question: String,
    end_date: String,
    strike_price: f64,
    yes_price: f64,
    no_price: f64,
    volume: f64,
    liquidity: f64,
}

#[derive(Debug, Clone, Serialize)]
struct NoArbViolation {
    lower_market_id: String,
    higher_market_id: String,
    lower_strike_price: f64,
    higher_strike_price: f64,
    lower_yes_price: f64,
    higher_yes_price: f64,
    violation_cents: f64,
}

#[derive(Debug, Clone, Serialize)]
struct NoArbGroup {
    end_date: String,
    market_count: usize,
    max_violation_cents: f64,
    clean: bool,
    ladder: Vec<NoArbMarketEntry>,
    violations: Vec<NoArbViolation>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NoArbSnapshot {
    status: String,
    threshold_cents: f64,
    market_count: usize,
    group_count: usize,
    comparable_group_count: usize,
    violation_count: usize,
    max_violation_cents: f64,
    worst_group: Option<NoArbGroup>,
    groups: Vec<NoArbGroup>,
}

impl NoArbSnapshot {
    fn insufficient() -> Self {
        Self {
            status: "insufficient_data".to_string(),
            threshold_cents: NO_ARB_VIOLATION_THRESHOLD_CENTS,
            market_count: 0,
            group_count: 0,
            comparable_group_count: 0,
            violation_count: 0,
            max_violation_cents: 0.0,
            worst_group: None,
            groups: Vec::new(),
        }
    }
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
            // Bare 'm' or 'h' must NOT be the start of a longer word (e.g. "march",
            // "may") — otherwise "1815-march" in a Polymarket slug gets misread as
            // 1815 minutes.
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
        .or_else(|| market_duration_minutes(start_date, end_date))
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
    // Special-case: treat "updown" as "up or down" for human-readable titles.
    if lp.contains("updown") && combined.contains("up or down") {
        if lp.contains("btc") {
            return combined.contains("btc") || combined.contains("bitcoin");
        }
        return true;
    }
    false
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/markets
// ─────────────────────────────────────────────────────────────────────────────
fn gamma_event_rows(raw: Value) -> anyhow::Result<Vec<Value>> {
    raw.as_array()
        .cloned()
        .or_else(|| raw.get("data").and_then(Value::as_array).cloned())
        .ok_or_else(|| anyhow::anyhow!("Gamma events response is not an array"))
}

async fn fetch_gamma_events(url: &str) -> anyhow::Result<Vec<Value>> {
    let response = HTTP.get(url).send().await?.error_for_status()?;
    gamma_event_rows(response.bounded_json::<Value>().await?)
}

fn is_above_expiry_contract(question: &str) -> bool {
    let words: Vec<_> = question
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .collect();
    words.iter().any(|word| word == "above")
        && !words.iter().any(|word| {
            matches!(
                word.as_str(),
                "below" | "ever" | "hit" | "reach" | "highest" | "between"
            )
        })
}

pub async fn list_markets(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let markets = try_fetch_markets_snapshot(&bot).await;
    match markets {
        Ok(markets) => (axum::http::StatusCode::OK, Json(json!({"markets":markets}))),
        Err(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"market discovery unavailable"})),
        ),
    }
}

pub async fn no_arb_snapshot(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let snapshot = fetch_no_arb_snapshot(&bot).await;
    if snapshot.is_null() {
        (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"no-arb discovery unavailable"})),
        )
    } else {
        (axum::http::StatusCode::OK, Json(snapshot))
    }
}

pub async fn fetch_no_arb_snapshot(_bot: &Arc<TradingBot>) -> Value {
    let url = format!(
        "{}/events?limit=300&order=endDate&ascending=true&active=true",
        GAMMA_BASE
    );

    let events = match fetch_gamma_events(&url).await {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(%error, "no-arb discovery unavailable");
            return Value::Null;
        }
    };

    let mut grouped: HashMap<i64, Vec<NoArbMarketEntry>> = HashMap::new();

    for event in &events {
        let slug = event.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let title = event.get("title").and_then(|v| v.as_str()).unwrap_or("");
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

        let ev_liquidity = event
            .get("liquidity")
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0.0);
        let ev_volume = event
            .get("volume")
            .and_then(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0.0);

        let Some(markets) = event.get("markets").and_then(|v| v.as_array()) else {
            continue;
        };

        for market in markets {
            let market_closed = market
                .get("closed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let market_active = market
                .get("active")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            if market_closed || !market_active {
                continue;
            }

            let question = market
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or(title);
            if !is_btc_related_market(slug, title, question) || !is_above_expiry_contract(question)
            {
                continue;
            }

            let description = market
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Some(strike_price) = parse_btc_price_from_text(description)
                .or_else(|| parse_btc_price_from_text(question))
            else {
                continue;
            };

            let end_date = market
                .get("endDate")
                .or_else(|| event.get("endDate"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Some(end_dt) = parse_utc_dt(end_date) else {
                continue;
            };

            let outcomes = market
                .get("outcomes")
                .and_then(|v| {
                    v.as_array()
                        .cloned()
                        .or_else(|| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
                })
                .unwrap_or_default();
            let out_prices = parse_outcome_prices(market);
            let Some((yes_price, no_price)) = extract_yes_no_prices(&outcomes, &out_prices, market)
            else {
                continue;
            };
            if !(0.0..=1.0).contains(&yes_price) || !(0.0..=1.0).contains(&no_price) {
                continue;
            }

            let market_id = market
                .get("conditionId")
                .or_else(|| market.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if market_id.is_empty() {
                continue;
            }

            let liquidity = if ev_liquidity > 0.0 {
                ev_liquidity
            } else {
                market
                    .get("liquidity")
                    .and_then(|v| {
                        v.as_f64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .unwrap_or(0.0)
            };
            let volume = if ev_volume > 0.0 {
                ev_volume
            } else {
                market
                    .get("volume")
                    .and_then(|v| {
                        v.as_f64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .unwrap_or(0.0)
            };

            grouped
                .entry(end_dt.timestamp())
                .or_default()
                .push(NoArbMarketEntry {
                    market_id,
                    question: question.to_string(),
                    end_date: end_dt.to_rfc3339(),
                    strike_price,
                    yes_price,
                    no_price,
                    volume,
                    liquidity,
                });
        }
    }

    let mut snapshot = json!(summarize_no_arb_groups(grouped));
    snapshot["assumptions"] = json!("Above-at-expiry questions with matching end dates; Gamma quotes are diagnostic, not executable arbitrage evidence");
    snapshot
}

fn summarize_no_arb_groups(grouped: HashMap<i64, Vec<NoArbMarketEntry>>) -> NoArbSnapshot {
    let mut groups: Vec<NoArbGroup> = grouped
        .into_values()
        .filter_map(|mut ladder| {
            ladder.sort_by(|a, b| {
                a.strike_price
                    .partial_cmp(&b.strike_price)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.market_id.cmp(&b.market_id))
            });
            if ladder.len() < 2 {
                return None;
            }

            let mut violations = Vec::new();
            for lower_idx in 0..ladder.len() {
                for higher_idx in (lower_idx + 1)..ladder.len() {
                    let lower = &ladder[lower_idx];
                    let higher = &ladder[higher_idx];
                    if (higher.strike_price - lower.strike_price).abs() < f64::EPSILON {
                        continue;
                    }
                    let violation_cents = (higher.yes_price - lower.yes_price) * 100.0;
                    if violation_cents > NO_ARB_VIOLATION_THRESHOLD_CENTS {
                        violations.push(NoArbViolation {
                            lower_market_id: lower.market_id.clone(),
                            higher_market_id: higher.market_id.clone(),
                            lower_strike_price: lower.strike_price,
                            higher_strike_price: higher.strike_price,
                            lower_yes_price: lower.yes_price,
                            higher_yes_price: higher.yes_price,
                            violation_cents,
                        });
                    }
                }
            }

            let max_violation_cents = violations
                .iter()
                .map(|v| v.violation_cents)
                .fold(0.0_f64, f64::max);

            Some(NoArbGroup {
                end_date: ladder
                    .first()
                    .map(|row| row.end_date.clone())
                    .unwrap_or_default(),
                market_count: ladder.len(),
                max_violation_cents,
                clean: violations.is_empty(),
                ladder,
                violations,
            })
        })
        .collect();

    groups.sort_by(|a, b| {
        b.max_violation_cents
            .partial_cmp(&a.max_violation_cents)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.end_date.cmp(&b.end_date))
    });

    if groups.is_empty() {
        return NoArbSnapshot::insufficient();
    }

    let market_count = groups.iter().map(|g| g.market_count).sum();
    let violation_count = groups.iter().map(|g| g.violations.len()).sum();
    let max_violation_cents = groups
        .iter()
        .map(|g| g.max_violation_cents)
        .fold(0.0_f64, f64::max);
    let worst_group = groups
        .iter()
        .max_by(|a, b| {
            a.max_violation_cents
                .partial_cmp(&b.max_violation_cents)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned();

    NoArbSnapshot {
        status: if violation_count == 0 {
            "clean".to_string()
        } else {
            "violations".to_string()
        },
        threshold_cents: NO_ARB_VIOLATION_THRESHOLD_CENTS,
        market_count,
        group_count: groups.len(),
        comparable_group_count: groups.len(),
        violation_count,
        max_violation_cents,
        worst_group,
        groups,
    }
}

pub async fn fetch_markets_snapshot(bot: &Arc<TradingBot>) -> Vec<Value> {
    try_fetch_markets_snapshot(bot)
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "market discovery unavailable");
            Vec::new()
        })
}

pub(crate) async fn try_fetch_markets_snapshot(
    bot: &Arc<TradingBot>,
) -> anyhow::Result<Vec<Value>> {
    let slug_pattern = {
        let cfg = bot.control_plane.config.read().await;
        cfg.market_filter.slug_pattern.clone()
    };
    let now = Utc::now();

    // Use a broad active-events query; filter at the market level instead of
    // relying on event endDate, which is not aligned to short-interval markets.
    let url = format!(
        "{}/events?limit=200&order=startDate&ascending=false&active=true",
        GAMMA_BASE
    );

    let mut events = fetch_gamma_events(&url).await?;

    // ── Targeted fetch: current 5m + 15m window slugs ─────────────────────
    {
        let now_et = now.with_timezone(&New_York);
        for target_window in [5u32, 15u32] {
            let floored_min = (now_et.minute() / target_window) * target_window;
            if let Some(floored_et) = now_et
                .with_minute(floored_min)
                .and_then(|dt| dt.with_second(0))
                .and_then(|dt| dt.with_nanosecond(0))
            {
                let epoch = floored_et.with_timezone(&Utc).timestamp();
                for ep in [epoch, epoch - (target_window as i64) * 60] {
                    let target_slug = format!("{slug_pattern}-{target_window}m-{ep}");
                    let slug_url = format!("{GAMMA_BASE}/events?slug={target_slug}");
                    if let Ok(r) = HTTP.get(&slug_url).send().await {
                        if r.status().is_success() {
                            if let Ok(v) = r.bounded_json::<Value>().await {
                                let slug_events: Vec<Value> = v
                                    .as_array()
                                    .cloned()
                                    .or_else(|| v.get("data").and_then(|a| a.as_array()).cloned())
                                    .unwrap_or_default();
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
                        }
                    }
                }
            }
        } // for target_window
    }

    let mut markets: Vec<Value> = vec![];

    for event in &events {
        // Slug filter — driven by config.market_filter.slug_pattern
        let slug = event.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let title = event.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !matches_slug_or_title(&slug_pattern, slug, title, "") {
            continue;
        }

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

        if let Some(mkt_arr) = event.get("markets").and_then(|v| v.as_array()) {
            for m in mkt_arr {
                let market_id = m
                    .get("conditionId")
                    .or_else(|| m.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if market_id.is_empty() {
                    continue;
                }

                let question_raw = m
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or(
                        event
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("BTC Up/Down"),
                    )
                    .to_string();

                let start_date_str = m
                    .get("startDate")
                    .or_else(|| event.get("startDate"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let end_date_str = m
                    .get("endDate")
                    .or_else(|| event.get("endDate"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let market_slug = m.get("slug").and_then(|v| v.as_str()).unwrap_or(slug);
                if !matches_slug_or_title(&slug_pattern, market_slug, title, &question_raw) {
                    continue;
                }

                let target = target_window_mins();
                let tol = window_tolerance_mins();
                let window_mins = if let Some(window) =
                    infer_window_mins(market_slug, &question_raw, start_date_str, end_date_str)
                {
                    if (window - target).abs() > tol {
                        continue;
                    }
                    window
                } else {
                    // If we cannot infer the window, skip to avoid mixed durations.
                    continue;
                };

                let start_dt = match select_start_dt(
                    &now,
                    start_date_str,
                    market_slug,
                    window_mins,
                    &question_raw,
                    end_date_str,
                ) {
                    Some(dt) => dt,
                    None => continue,
                };
                let end_dt = start_dt + chrono::Duration::minutes(window_mins.round() as i64);
                let display_question = normalize_question_label(&question_raw, &start_dt);
                let mins_left = ((end_dt - now).num_seconds() as f64 / 60.0).max(0.0);
                let elapsed_mins = (now - start_dt).num_seconds() as f64 / 60.0;
                let computed_end = end_dt.to_rfc3339();
                if mins_left <= 0.0 || mins_left > window_mins + 1.0 || elapsed_mins < -0.25 {
                    continue;
                }

                // outcomePrices — same dual-parse as analyze_market()
                let outcomes = m
                    .get("outcomes")
                    .and_then(|v| {
                        v.as_array()
                            .cloned()
                            .or_else(|| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
                    })
                    .unwrap_or_default();
                let out_prices = parse_outcome_prices(m);

                let Some((mut yes_price, mut no_price)) =
                    extract_yes_no_prices(&outcomes, &out_prices, m)
                else {
                    continue;
                };
                if let Some((yes_token, no_token)) = extract_yes_no_token_ids(&outcomes, m) {
                    if let Some(p) = fetch_token_price_clob(&yes_token).await {
                        yes_price = p;
                    }
                    if let Some(p) = fetch_token_price_clob(&no_token).await {
                        no_price = p;
                    }
                }
                let best_bid = m.get("bestBid").and_then(parse_price);
                let best_ask = m.get("bestAsk").and_then(parse_price);
                let spread_bps = best_bid
                    .zip(best_ask)
                    .filter(|(bid, ask)| ask >= bid)
                    .map(|(bid, ask)| (ask - bid) * 10_000.0);

                let volume = event
                    .get("volume")
                    .and_then(|v| {
                        v.as_f64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .or_else(|| m.get("volume").and_then(|v| v.as_f64()))
                    .unwrap_or(0.0);

                let liquidity = event
                    .get("liquidity")
                    .and_then(|v| {
                        v.as_f64()
                            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    })
                    .or_else(|| m.get("liquidity").and_then(|v| v.as_f64()))
                    .unwrap_or(0.0);

                markets.push(json!({
                    "id":           market_id,
                    "market_id":    market_id,
                    "question":     display_question,
                    "end_date":     computed_end,
                    "mins_left":    mins_left,
                    "elapsed_mins": elapsed_mins,
                    "current_price": yes_price,
                    "yes_price":    yes_price,
                    "no_price":     no_price,
                    "best_bid":     best_bid,
                    "best_ask":     best_ask,
                    "spread_bps":   spread_bps,
                    "volume":       volume,
                    "liquidity":    liquidity,
                }));
            }
        }
    }

    Ok(markets)
}

/// Parse outcomePrices — handles both raw JSON array and JSON-encoded string.
fn parse_outcome_prices(m: &Value) -> Vec<Value> {
    if let Some(v) = m.get("outcomePrices") {
        if let Some(arr) = v.as_array() {
            return arr.clone();
        }
        if let Some(s) = v.as_str() {
            if let Ok(parsed) = serde_json::from_str::<Vec<Value>>(s) {
                return parsed;
            }
        }
    }
    vec![]
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/markets/:cid/price
// ─────────────────────────────────────────────────────────────────────────────
pub async fn market_price(
    State(_bot): State<Arc<TradingBot>>,
    Path(cid): Path<String>,
) -> impl IntoResponse {
    // `cid` identifies a binary condition, not either outcome token. Resolve
    // both tokens and read each token's own book; unknown remains absent.
    let yes_token = _bot
        .get_clob_token_id(&cid, crate::domain::Signal::BuyYes)
        .await;
    let no_token = _bot
        .get_clob_token_id(&cid, crate::domain::Signal::BuyNo)
        .await;
    let book_mid = |book: crate::clob::PublicBookSnapshot| {
        book.best_bid
            .zip(book.best_ask)
            .filter(|(bid, ask)| {
                bid.is_finite() && ask.is_finite() && *bid > 0.0 && *ask < 1.0 && ask >= bid
            })
            .map(|(bid, ask)| (bid + ask) / 2.0)
    };
    let yes = if let Some(token) = yes_token {
        crate::clob::fetch_public_book_snapshot(&token)
            .await
            .ok()
            .and_then(book_mid)
    } else {
        None
    };
    let no = if let Some(token) = no_token {
        crate::clob::fetch_public_book_snapshot(&token)
            .await
            .ok()
            .and_then(book_mid)
    } else {
        None
    };
    Json(json!({"up_price": yes, "down_price": no,
        "live": yes.is_some() && no.is_some(), "source": "clob_book", "available": yes.is_some() || no.is_some()}))
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/outcomes?limit=16
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct OutcomesQuery {
    pub limit: Option<usize>,
}

pub async fn recent_outcomes(
    State(bot): State<Arc<TradingBot>>,
    Query(q): Query<OutcomesQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(16).clamp(1, 200);
    let outcomes = fetch_recent_outcomes_snapshot(&bot, limit).await;
    match outcomes {
        Ok(outcomes) => (
            axum::http::StatusCode::OK,
            Json(json!({"outcomes":outcomes})),
        ),
        Err(_) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"outcome discovery unavailable"})),
        ),
    }
}

pub async fn fetch_recent_outcomes_snapshot(
    bot: &Arc<TradingBot>,
    limit: usize,
) -> anyhow::Result<Vec<Value>> {
    let slug_pattern = {
        let cfg = bot.control_plane.config.read().await;
        cfg.market_filter.slug_pattern.clone()
    };

    // Fetch recent events (open or closed) and detect settled sub-markets.
    let url = format!(
        "{}/events?limit=300&order=endDate&ascending=false",
        GAMMA_BASE
    );

    let events = fetch_gamma_events(&url).await?;

    let mut outcomes: Vec<Value> = vec![];

    'outer: for event in &events {
        // Filter by slug pattern from config
        let slug = event.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let title = event.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if !matches_slug_or_title(&slug_pattern, slug, title, "") {
            continue;
        }

        // Event-level winner — Gamma sometimes sets this at the event, not market level
        let ev_winner = event.get("winner").and_then(|v| v.as_str()).unwrap_or("");
        let event_closed = event.get("closed").and_then(Value::as_bool) == Some(true);
        let ev_title = event.get("title").and_then(|v| v.as_str()).unwrap_or("");

        if let Some(mkt_arr) = event.get("markets").and_then(|v| v.as_array()) {
            for m in mkt_arr {
                if outcomes.len() >= limit {
                    break 'outer;
                }

                let question = m
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or(ev_title);

                let out_arr = parse_outcome_prices(m);

                // Check sub-market first, then fall back to event-level winner
                let settled_outcome = determine_settled_outcome(&out_arr, m).or_else(|| {
                    if !event_closed || m.get("closed").and_then(Value::as_bool) != Some(true) {
                        return None;
                    }
                    normalize_winner(ev_winner).map(str::to_string)
                });

                let Some(settled_outcome) = settled_outcome else {
                    continue;
                };

                let start_date = m
                    .get("startDate")
                    .or_else(|| event.get("startDate"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                let end_date = m
                    .get("endDate")
                    .or_else(|| event.get("endDate"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                let market_slug = m.get("slug").and_then(|v| v.as_str()).unwrap_or(slug);
                if !matches_slug_or_title(&slug_pattern, market_slug, ev_title, question) {
                    continue;
                }

                let target = target_window_mins();
                let tol = window_tolerance_mins();
                if let Some(window) =
                    infer_window_mins(market_slug, question, start_date, end_date.as_str())
                {
                    if (window - target).abs() > tol {
                        continue;
                    }
                } else {
                    continue;
                }

                outcomes.push(json!({
                    "outcome":   settled_outcome,
                    "end_date":  end_date,
                    "question":  question,
                }));
            }
        }
    }

    Ok(outcomes)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_utc_dt(s: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
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
                let Some(next_hour) = hour
                    .checked_mul(10)
                    .and_then(|v| v.checked_add((bytes[i] - b'0') as u32))
                else {
                    return Vec::new();
                };
                hour = next_hour;
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

fn select_start_dt(
    now: &chrono::DateTime<Utc>,
    start_date_str: &str,
    slug: &str,
    window_mins: f64,
    question: &str,
    end_date_str: &str,
) -> Option<chrono::DateTime<Utc>> {
    let wm = chrono::Duration::minutes(window_mins.round() as i64);

    // ── Priority 1: slug epoch (exact start time embedded in slug) ──────
    if let Some(ts) = parse_epoch_from_slug(slug).and_then(|ts| Utc.timestamp_opt(ts, 0).single()) {
        let end_dt = ts + wm;
        if end_dt > *now {
            return Some(ts);
        }
    }

    // ── Priority 2: endDate − window (Polymarket endDate is always correct) ──
    if let Some(end_dt) = parse_utc_dt(end_date_str) {
        let start = end_dt - wm;
        if end_dt > *now && (*now - start).num_minutes().abs() <= (window_mins * 2.0) as i64 {
            return Some(start);
        }
    }

    // ── Priority 3: parse date + times from question text ───────────────
    if let Some((qs, qe)) = parse_question_window_utc(question, now) {
        if qe > *now {
            return Some(qs);
        }
    }

    // ── Priority 4: parse times from question using API startDate's date ─
    let dt = parse_utc_dt(start_date_str);
    if let Some(dt) = dt {
        let base_date = dt.with_timezone(&New_York).date_naive();
        if let Some((qs, qe)) = parse_question_window_with_date(question, base_date) {
            if qe > *now {
                return Some(qs);
            }
        }
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

    // ── Priority 6: snap "now" to the nearest window boundary ───────────
    if let Some(floored) = floor_start_dt(*now, window_mins) {
        let end_dt = floored + wm;
        if end_dt > *now {
            return Some(floored);
        }
    }

    None
}

fn market_duration_minutes(start_date_str: &str, end_date_str: &str) -> Option<f64> {
    if start_date_str.is_empty() || end_date_str.is_empty() {
        return None;
    }
    let start = chrono::DateTime::parse_from_rfc3339(start_date_str).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(end_date_str).ok()?;
    let diff = end.with_timezone(&Utc) - start.with_timezone(&Utc);
    Some(diff.num_seconds() as f64 / 60.0)
}

fn parse_outcomes_list(m: &Value) -> Vec<String> {
    m.get("outcomes")
        .and_then(|v| {
            v.as_array()
                .cloned()
                .or_else(|| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
        })
        .unwrap_or_default()
        .iter()
        .filter_map(|v| v.as_str().map(|s| s.to_string()))
        .collect()
}

fn parse_token_ids(m: &Value) -> Vec<String> {
    m.get("clobTokenIds")
        .and_then(|v| {
            v.as_array()
                .cloned()
                .or_else(|| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
        })
        .unwrap_or_default()
        .iter()
        .filter_map(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .collect()
}

fn extract_yes_no_token_ids(outcomes: &[Value], market: &Value) -> Option<(String, String)> {
    let out_strings = if outcomes.is_empty() {
        parse_outcomes_list(market)
    } else {
        outcomes
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect()
    };
    let token_ids = parse_token_ids(market);
    if token_ids.len() < 2 {
        return None;
    }
    let yes_idx = out_strings
        .iter()
        .position(|o| {
            let s = o.to_lowercase();
            s == "yes" || s == "up"
        })
        .unwrap_or(0);
    let no_idx = if yes_idx == 0 { 1 } else { 0 };
    let yes_token = token_ids.get(yes_idx).cloned()?;
    let no_token = token_ids.get(no_idx).cloned()?;
    Some((yes_token, no_token))
}

async fn fetch_token_price_clob(token_id: &str) -> Option<f64> {
    let url = format!("{CLOB_BASE}/price?token_id={token_id}&side=BUY");
    let r = HTTP.get(&url).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    let v: Value = r.bounded_json().await.ok()?;
    let p = v.get("price").and_then(|x| {
        x.as_f64()
            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
    })?;
    if (0.0..=1.0).contains(&p) {
        Some(p)
    } else {
        None
    }
}

fn extract_yes_no_prices(
    outcomes: &[Value],
    out_prices: &[Value],
    market: &Value,
) -> Option<(f64, f64)> {
    let labels = if outcomes.is_empty() {
        parse_outcomes_list(market)
    } else {
        outcomes
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    };
    if labels.len() != 2 || out_prices.len() != 2 {
        return None;
    }
    let yes_idx = labels
        .iter()
        .position(|label| matches!(label.to_ascii_lowercase().as_str(), "yes" | "up"))?;
    let no_idx = labels
        .iter()
        .position(|label| matches!(label.to_ascii_lowercase().as_str(), "no" | "down"))?;
    if yes_idx == no_idx {
        return None;
    }
    Some((
        parse_price(out_prices.get(yes_idx)?)?,
        parse_price(out_prices.get(no_idx)?)?,
    ))
}

fn parse_price(v: &Value) -> Option<f64> {
    crate::json_util::parse_f64(v).filter(|price| (0.0..=1.0).contains(price))
}

fn normalize_winner(winner: &str) -> Option<&'static str> {
    match winner.trim().to_ascii_lowercase().as_str() {
        "yes" | "up" => Some("UP"),
        "no" | "down" => Some("DOWN"),
        _ => None,
    }
}

fn determine_settled_outcome(out_prices: &[Value], market: &Value) -> Option<String> {
    // A price near one is still a market opinion until resolution. Only
    // explicit closed/resolved data can contribute to the outcome streak.
    let resolved = market.get("closed").and_then(Value::as_bool) == Some(true)
        || market.get("umaResolutionStatus").and_then(Value::as_str) == Some("resolved");
    if !resolved {
        return None;
    }
    if let Some(winner) = market
        .get("winner")
        .and_then(Value::as_str)
        .and_then(normalize_winner)
    {
        return Some(winner.into());
    }
    let (yes, no) = extract_yes_no_prices(&[], out_prices, market)?;
    if yes >= 1.0 - 1e-9 && no <= 1e-9 {
        Some("UP".into())
    } else if no >= 1.0 - 1e-9 && yes <= 1e-9 {
        Some("DOWN".into())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn discovery_failure_is_distinct_from_a_successful_empty_scan() {
        assert!(super::gamma_event_rows(serde_json::json!([]))
            .unwrap()
            .is_empty());
        assert!(super::gamma_event_rows(serde_json::json!({"data":[]}))
            .unwrap()
            .is_empty());
        for invalid in [
            serde_json::json!(null),
            serde_json::json!({"error":"provider failed"}),
            serde_json::json!({"data":null}),
        ] {
            assert!(super::gamma_event_rows(invalid).is_err());
        }
    }

    #[test]
    fn strike_ladders_never_mix_below_or_path_dependent_contracts() {
        assert!(super::is_above_expiry_contract(
            "Will Bitcoin be above $90,000 on October 11?"
        ));
        for question in [
            "Will Bitcoin be below $90,000?",
            "Will Bitcoin ever reach above $90,000?",
            "Will Bitcoin be between $80,000 and above $90,000?",
            "Bitcoin up or down?",
        ] {
            assert!(!super::is_above_expiry_contract(question));
        }
    }
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn missing_or_malformed_prices_are_never_even_odds() {
        assert_eq!(parse_price(&json!("not a price")), None);
        assert_eq!(parse_price(&json!("NaN")), None);
        assert_eq!(extract_yes_no_prices(&[], &[], &json!({})), None);
        assert_eq!(
            extract_yes_no_prices(
                &[json!("Other"), json!("No"), json!("Yes")],
                &[json!(0.5), json!(0.5)],
                &json!({})
            ),
            None
        );
    }

    #[test]
    fn outcome_streak_requires_resolution_and_respects_reversed_string_labels() {
        let live = json!({"closed": false, "outcomes":"[\"Yes\",\"No\"]"});
        assert_eq!(
            determine_settled_outcome(&[json!(0.99), json!(0.01)], &live),
            None
        );
        let resolved = json!({"closed": true, "outcomes":"[\"No\",\"Yes\"]"});
        assert_eq!(
            determine_settled_outcome(&[json!(0), json!(1)], &resolved),
            Some("UP".into())
        );
        assert_eq!(
            determine_settled_outcome(&[json!(0.02), json!(0.98)], &resolved),
            None
        );
        assert_eq!(normalize_winner("unknown"), None);
        assert!(parse_time_tokens("999999999999999999999pm").is_empty());
    }

    #[test]
    fn parse_outcome_prices_accepts_json_string() {
        let market = json!({
            "outcomePrices": "[\"0.61\",\"0.39\"]"
        });

        let out = parse_outcome_prices(&market);
        assert_eq!(out.len(), 2);
        assert!((parse_price(&out[0]).unwrap() - 0.61).abs() < 1e-9);
        assert!((parse_price(&out[1]).unwrap() - 0.39).abs() < 1e-9);
    }

    #[test]
    fn extract_yes_no_prices_works_with_string_parsed_outcomes() {
        let market = json!({
            "outcomes": ["Yes", "No"],
            "outcomePrices": "[\"0.73\",\"0.27\"]"
        });

        let outcomes = market
            .get("outcomes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let out_prices = parse_outcome_prices(&market);
        let (yes, no) = extract_yes_no_prices(&outcomes, &out_prices, &market).unwrap();

        assert!((yes - 0.73).abs() < 1e-9);
        assert!((no - 0.27).abs() < 1e-9);
    }

    #[test]
    fn parse_btc_price_from_text_extracts_strike() {
        let question = "Will BTC be above $92,500 at 4:00 PM ET?";
        let strike = parse_btc_price_from_text(question).unwrap();
        assert!((strike - 92_500.0).abs() < 1e-9);
    }

    #[test]
    fn summarize_no_arb_groups_flags_monotonicity_violation() {
        let mut grouped = HashMap::new();
        grouped.insert(
            1,
            vec![
                NoArbMarketEntry {
                    market_id: "low".into(),
                    question: "BTC above $90k".into(),
                    end_date: "2026-03-21T16:00:00Z".into(),
                    strike_price: 90_000.0,
                    yes_price: 0.42,
                    no_price: 0.58,
                    volume: 1_000.0,
                    liquidity: 1_500.0,
                },
                NoArbMarketEntry {
                    market_id: "high".into(),
                    question: "BTC above $95k".into(),
                    end_date: "2026-03-21T16:00:00Z".into(),
                    strike_price: 95_000.0,
                    yes_price: 0.47,
                    no_price: 0.53,
                    volume: 900.0,
                    liquidity: 1_400.0,
                },
            ],
        );

        let snapshot = summarize_no_arb_groups(grouped);
        assert_eq!(snapshot.status, "violations");
        assert_eq!(snapshot.violation_count, 1);
        assert!(snapshot.max_violation_cents > 4.9);
    }

    #[test]
    fn summarize_no_arb_groups_accepts_clean_ladder() {
        let mut grouped = HashMap::new();
        grouped.insert(
            1,
            vec![
                NoArbMarketEntry {
                    market_id: "low".into(),
                    question: "BTC above $90k".into(),
                    end_date: "2026-03-21T16:00:00Z".into(),
                    strike_price: 90_000.0,
                    yes_price: 0.61,
                    no_price: 0.39,
                    volume: 1_000.0,
                    liquidity: 1_500.0,
                },
                NoArbMarketEntry {
                    market_id: "high".into(),
                    question: "BTC above $95k".into(),
                    end_date: "2026-03-21T16:00:00Z".into(),
                    strike_price: 95_000.0,
                    yes_price: 0.43,
                    no_price: 0.57,
                    volume: 900.0,
                    liquidity: 1_400.0,
                },
            ],
        );

        let snapshot = summarize_no_arb_groups(grouped);
        assert_eq!(snapshot.status, "clean");
        assert_eq!(snapshot.violation_count, 0);
        assert_eq!(snapshot.max_violation_cents, 0.0);
    }
}
