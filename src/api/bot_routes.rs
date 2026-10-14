//! Bot loop / analysis / trade execution / stats handlers.

use crate::http_client::JsonResponseExt;
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::{Component, Path as FsPath};
use std::sync::Arc;

use crate::analysis::AnalysisResult as ParsedAnalysis;
use crate::bot::idempotency::IdempotencyClaim;
use crate::bot::state::TradingBot;
use crate::domain::{BotMode, Direction, Signal};
use crate::http_client::HTTP;
use crate::metrics;

const DEFAULT_GAMMA_BASE: &str = "https://gamma-api.polymarket.com";

/// Preserve the settled Trade response, while an unresolved BUY gets an
/// explicit envelope that can be cached safely for the original request.
async fn entry_execution_payload(bot: &TradingBot, trade: &crate::db::Trade) -> (Value, bool) {
    let state = match bot.entry_confirmation_state(trade).await {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                "entry response evidence lookup failed for trade {}: {error}",
                trade.id
            );
            Some("unavailable")
        }
    };
    if let Some(state) = state {
        let reason = match state {
            "audit_uncertain" => {
                "Entry evidence needs revalidation; accounted inventory and risk stops are retained"
            }
            "unavailable" => {
                "Entry confirmation is unavailable; inspect recovery evidence before retrying"
            }
            _ => "Entry is awaiting confirmed fills; inspect recovery evidence before retrying",
        };
        let mut snapshot = json!(trade);
        snapshot["entry_execution_status"] = json!(state);
        (
            json!({"status":"pending","execution_status":state,"reason":reason,"trade":snapshot}),
            true,
        )
    } else {
        (json!(trade), false)
    }
}

fn gamma_base() -> String {
    std::env::var("MACBOT_GAMMA_BASE").unwrap_or_else(|_| DEFAULT_GAMMA_BASE.to_string())
}

fn preflight_block_message(report: &super::health::LivePreflightReport, action: &str) -> String {
    let detail = report
        .errors
        .first()
        .cloned()
        .or_else(|| report.config_error.clone())
        .unwrap_or_else(|| "see GET /api/preflight for details".to_string());
    format!("live preflight blocked {action}: {detail}. See GET /api/preflight")
}

async fn require_live_preflight_ready(
    bot: &Arc<TradingBot>,
    action: &str,
) -> Result<(), (StatusCode, String)> {
    if bot.backtest_only() {
        return Err((
            StatusCode::CONFLICT,
            format!("{action} is disabled in the research-only demo"),
        ));
    }
    let cfg = bot.control_plane.config.read().await.clone();
    let report = super::health::live_preflight_report(&cfg);
    if report.required && !report.ready {
        let message = preflight_block_message(&report, action);
        tracing::warn!("{message}");
        return Err((StatusCode::SERVICE_UNAVAILABLE, message));
    }
    Ok(())
}

// ── Query param types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct AnalyzeQuery {
    pub market_cid: Option<String>,
}

#[derive(Deserialize)]
pub struct ForceTradeQuery {
    pub direction: Option<String>,
    pub market_cid: Option<String>,
    pub amount: Option<f64>,
}

#[derive(Deserialize)]
pub struct ExecuteTradeBody {
    pub hail_mary: Option<bool>,
    pub override_amount: Option<f64>,
}

#[derive(Deserialize)]
pub struct KillSwitchBody {
    pub active: bool,
}

#[derive(Deserialize)]
pub struct ResetBalanceBody {
    pub amount: Option<f64>,
    pub force: Option<bool>,
}

#[derive(Deserialize)]
pub struct BackupDatabaseBody {
    pub path: Option<String>,
    pub overwrite: Option<bool>,
}

fn default_backup_path() -> String {
    format!(
        "artifacts/backups/manual-{}.sqlite",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    )
}

fn validate_backup_path(raw: Option<&str>) -> Result<String, (StatusCode, String)> {
    let candidate = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .unwrap_or_else(default_backup_path);
    let path = FsPath::new(&candidate);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "backup path must be a safe relative path".to_string(),
        ));
    }
    if !candidate.starts_with("artifacts/backups/") {
        return Err((
            StatusCode::BAD_REQUEST,
            "backup path must stay under artifacts/backups/".to_string(),
        ));
    }
    if path.extension().and_then(|ext| ext.to_str()) != Some("sqlite") {
        return Err((
            StatusCode::BAD_REQUEST,
            "backup path must end with .sqlite".to_string(),
        ));
    }
    Ok(candidate)
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/loop/start
// ─────────────────────────────────────────────────────────────────────────────
pub async fn start_loop(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let Ok(_recovery_control) = bot.execution_state.recovery_control_gate.try_lock() else {
        return (
            StatusCode::CONFLICT,
            Json(
                json!({"started":false,"reason":"receipt recovery is active; loop start is paused"}),
            ),
        );
    };
    if bot.backtest_only() {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "started": false,
                "reason": "loop start is disabled in the research-only demo",
            })),
        );
    }
    let cfg = bot.control_plane.config.read().await.clone();
    let report = super::health::live_preflight_report(&cfg);
    if report.required && !report.ready {
        let reason = preflight_block_message(&report, "loop start");
        tracing::warn!("{reason}");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "started": false,
                "reason": reason,
                "preflight": report,
            })),
        );
    }

    bot.start_loop();
    (StatusCode::OK, Json(json!({ "started": true })))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/loop/stop
// ─────────────────────────────────────────────────────────────────────────────
pub async fn stop_loop(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    bot.stop_loop();
    Json(json!({ "stopped": true }))
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/loop/status
// ─────────────────────────────────────────────────────────────────────────────
pub async fn loop_status(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    Json(json!({
        "running": bot.loop_running(),
        "backtest_only": bot.backtest_only(),
        "mode":    bot.mode().await,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/analysis — read the cache without starting an analysis cycle
// ─────────────────────────────────────────────────────────────────────────────
/// Attach the server's own view of how old this analysis is.
///
/// Callers draw every field of this payload as if it were current. It can be a
/// whole analysis interval old, so the age travels with the data rather than
/// being left for the reader to guess.
fn with_freshness(mut analysis: Value, freshness: Value) -> Value {
    if analysis.is_null() {
        analysis = json!({});
    }
    if let Some(obj) = analysis.as_object_mut() {
        obj.insert("analysis_freshness".to_string(), freshness);
    }
    analysis
}

pub async fn get_analysis(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let cached = {
        let guard = bot.analysis_state.last_analysis.read().await;
        guard.clone()
    };

    // Polling the dashboard must never initiate external provider requests or
    // make an absent analysis look fresh. POST /api/analyze owns that action.
    let freshness = bot.analysis_freshness().await;
    Json(with_freshness(cached, freshness))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/analyze — trigger fresh analysis for the active market
// ─────────────────────────────────────────────────────────────────────────────
pub async fn trigger_analysis(
    State(bot): State<Arc<TradingBot>>,
    Query(q): Query<AnalyzeQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if bot.backtest_only() {
        return Err((
            StatusCode::CONFLICT,
            "analysis is disabled in the research-only demo; use the historical backtest API"
                .into(),
        ));
    }
    if q.market_cid.is_some() {
        return Err((StatusCode::BAD_REQUEST,
            "market_cid selection is unsupported for analysis; the active market is selected by the feed".into()));
    }

    let analysis = bot.analyze_market().await;
    {
        let mut guard = bot.analysis_state.last_analysis.write().await;
        *guard = analysis.clone();
    }
    bot.mark_analysis_fresh().await;
    let freshness = bot.analysis_freshness().await;
    Ok(Json(with_freshness(analysis, freshness)))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/trade  — execute from cached analysis
// ─────────────────────────────────────────────────────────────────────────────
pub async fn execute_trade_handler(
    State(bot): State<Arc<TradingBot>>,
    headers: HeaderMap,
    body: Option<Json<ExecuteTradeBody>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    metrics::inc_api_trade_request();
    let (hail_mary, override_amount) = body
        .map(|Json(b)| (b.hail_mary.unwrap_or(false), b.override_amount))
        .unwrap_or((false, None));

    let idem_key = idempotency_key(&headers).map(|key| {
        format!(
            "trade:hail_mary={}:override_amount={}:{}",
            hail_mary,
            override_amount
                .map(|v| format!("{v:.8}"))
                .unwrap_or_else(|| "none".to_string()),
            key
        )
    });
    let mut claimed_idempotency = false;
    if let Some(ref key) = idem_key {
        match bot
            .idempotency_claim_or_get(key)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            IdempotencyClaim::Cached(cached) => {
                metrics::inc_api_trade_idem_hit();
                return Ok(Json(cached));
            }
            IdempotencyClaim::Claimed => claimed_idempotency = true,
            IdempotencyClaim::InProgress => {
                return Err((
                    StatusCode::CONFLICT,
                    "idempotent trade request is already in progress".into(),
                ));
            }
        }
    }

    if let Err(err) = require_live_preflight_ready(&bot, "trade execution").await {
        if claimed_idempotency {
            if let Some(key) = idem_key.as_deref() {
                bot.idempotency_abort(key).await;
            }
        }
        return Err(err);
    }

    // Use cached analysis
    let analysis = {
        let guard = bot.analysis_state.last_analysis.read().await;
        guard.clone()
    };

    if analysis.is_null() {
        if claimed_idempotency {
            if let Some(key) = idem_key.as_deref() {
                bot.idempotency_abort(key).await;
            }
        }
        metrics::inc_api_trade_fail();
        return Err((
            StatusCode::BAD_REQUEST,
            "no analysis available — call POST /api/analyze first".into(),
        ));
    }

    let parsed = ParsedAnalysis::from_value(&analysis);
    let signal = parsed.signal;

    if signal == Signal::Hold && !hail_mary {
        if claimed_idempotency {
            if let Some(key) = idem_key.as_deref() {
                bot.idempotency_abort(key).await;
            }
        }
        metrics::inc_api_trade_fail();
        return Err((
            StatusCode::BAD_REQUEST,
            "current signal is HOLD — use /api/trade/force to override the signal".into(),
        ));
    }

    match bot
        .execute_trade_serialized_result(&analysis, hail_mary, override_amount)
        .await
    {
        Ok(Some(trade)) => {
            let (payload, pending) = entry_execution_payload(&bot, &trade).await;
            if let Some(key) = idem_key {
                bot.idempotency_put(key, payload.clone()).await;
            }
            if pending {
                metrics::inc_api_trade_filtered();
            } else {
                metrics::inc_api_trade_success();
            }
            Ok(Json(payload))
        }
        Ok(None) => {
            let payload = json!({
                "status": "not_executed",
                "reason": "trade request did not result in an opened trade"
            });
            if let Some(key) = idem_key {
                bot.idempotency_put(key, payload.clone()).await;
            }
            metrics::inc_api_trade_filtered();
            Ok(Json(payload))
        }
        Err(e) => {
            // A dropped/timed-out response does not prove the dispatched order
            // failed. Keep its durable claim so a retry cannot submit it again.
            if e != crate::bot::state::ActorCallError::ResponseFailed {
                if let Some(key) = idem_key.as_deref() {
                    bot.idempotency_abort(key).await;
                }
            }
            metrics::inc_api_trade_fail();
            tracing::warn!("execute_trade_serialized_result failed: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "execution actor failure".into(),
            ))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/trade/force?direction=BUY_YES&market_cid=X&amount=Y
//   Override the signal using fresh same-market posterior evidence and normal risk checks.
// ─────────────────────────────────────────────────────────────────────────────
fn forced_analysis_with_evidence(
    mut cached: Value,
    direction: Direction,
    market_id: &str,
    entry_price: f64,
    question: String,
    end_date: String,
    stale: bool,
) -> Option<Value> {
    let evidence = ParsedAnalysis::from_value(&cached);
    let p_up = evidence.p_up?;
    let confidence = cached
        .get("confidence")
        .and_then(crate::json_util::parse_f64)?;
    if stale
        || evidence.market.market_id != market_id
        || !p_up.is_finite()
        || !(0.0..=1.0).contains(&p_up)
        || !(0.0..=1.0).contains(&confidence)
    {
        return None;
    }
    cached["signal"] = json!(direction.as_str());
    cached["forced"] = json!(true);
    cached["reason"] = json!("Manual force using fresh same-market model evidence");
    // The executor computes EV from this posterior and its actual order price.
    cached["net_ev"] = Value::Null;
    cached["market_data"]["market_id"] = json!(market_id);
    cached["market_data"]["question"] = json!(question);
    cached["market_data"]["price"] = json!(entry_price);
    cached["market_data"]["end_date"] = json!(end_date);
    Some(cached)
}

pub async fn force_trade(
    State(bot): State<Arc<TradingBot>>,
    headers: HeaderMap,
    Query(q): Query<ForceTradeQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    metrics::inc_api_force_request();
    let idem_key = idempotency_key(&headers).map(|key| {
        format!(
            "force_trade:direction={}:market_cid={}:amount={}:{}",
            q.direction.as_deref().unwrap_or("BUY_YES"),
            q.market_cid.as_deref().unwrap_or("cached"),
            q.amount
                .map(|v| format!("{v:.8}"))
                .unwrap_or_else(|| "none".to_string()),
            key
        )
    });
    let mut claimed_idempotency = false;
    if let Some(ref key) = idem_key {
        match bot
            .idempotency_claim_or_get(key)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            IdempotencyClaim::Cached(cached) => {
                metrics::inc_api_force_idem_hit();
                return Ok(Json(cached));
            }
            IdempotencyClaim::Claimed => claimed_idempotency = true,
            IdempotencyClaim::InProgress => {
                return Err((
                    StatusCode::CONFLICT,
                    "idempotent force-trade request is already in progress".into(),
                ));
            }
        }
    }

    if let Err(err) = require_live_preflight_ready(&bot, "force trade").await {
        if claimed_idempotency {
            if let Some(key) = idem_key.as_deref() {
                bot.idempotency_abort(key).await;
            }
        }
        return Err(err);
    }

    let direction = Direction::parse(q.direction.as_deref().unwrap_or("BUY_YES"));
    if direction == Direction::Unknown {
        if claimed_idempotency {
            if let Some(key) = idem_key.as_deref() {
                bot.idempotency_abort(key).await;
            }
        }
        metrics::inc_api_force_fail();
        return Err((
            StatusCode::BAD_REQUEST,
            "direction must be BUY_YES or BUY_NO".into(),
        ));
    }

    // Get config values for reasonable defaults
    let (trade_amount, min_entry, max_entry) = {
        let cfg = bot.control_plane.config.read().await;
        (cfg.trade_amount, cfg.min_entry_price, cfg.max_entry_price)
    };

    let override_amount = q.amount.or(Some(trade_amount));

    // Resolve market_cid — either from query param or last cached analysis.
    // Also carry end_date so forced trades can auto-resolve.
    let (resolved_cid, mut question, mut end_date) = if let Some(ref cid) = q.market_cid {
        let (mkt_q, mkt_end) = fetch_market_meta(cid).await;
        (cid.clone(), mkt_q, mkt_end)
    } else {
        // Read market_id + question + end_date from last analysis.
        let analysis = bot.analysis_state.last_analysis.read().await;
        let parsed = ParsedAnalysis::from_value(&analysis);
        let mid = parsed.market.market_id;
        let q_str = if parsed.market.question.is_empty() {
            "BTC 15m forced trade".to_string()
        } else {
            parsed.market.question
        };
        let end = parsed.market.end_date;
        (mid, q_str, end)
    };

    if resolved_cid.is_empty() {
        if claimed_idempotency {
            if let Some(key) = idem_key.as_deref() {
                bot.idempotency_abort(key).await;
            }
        }
        metrics::inc_api_force_fail();
        return Err((
            StatusCode::BAD_REQUEST,
            "no market_cid provided and no cached analysis available".into(),
        ));
    }

    // Backfill end_date from market metadata when cached analysis did not have it.
    if end_date.trim().is_empty() || question.trim().is_empty() {
        let (fetched_q, fetched_end) = fetch_market_meta(&resolved_cid).await;
        let (q2, end2) = backfill_market_meta(question, end_date, fetched_q, fetched_end);
        question = q2;
        end_date = end2;
    }

    // Always fetch a fresh live price — never use cached/stale analysis price
    let Some(entry_price) = fetch_fresh_market_price(&bot, &resolved_cid, direction).await else {
        if let Some(key) = idem_key.as_deref() {
            bot.idempotency_abort(key).await;
        }
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no current two-sided CLOB book is available for this market and side".into(),
        ));
    };
    if entry_price < min_entry || entry_price > max_entry {
        if let Some(key) = idem_key.as_deref() {
            bot.idempotency_abort(key).await;
        }
        return Err((
            StatusCode::BAD_REQUEST,
            "current market price is outside configured entry bounds".into(),
        ));
    }
    let market_id = resolved_cid;

    // Force overrides the signal decision, but cannot invent model evidence.
    let cached = bot.analysis_state.last_analysis.read().await.clone();
    let mut forced_analysis = match forced_analysis_with_evidence(
        cached,
        direction,
        &market_id,
        entry_price,
        question,
        end_date,
        bot.is_analysis_stale().await,
    ) {
        Some(analysis) => analysis,
        None => {
            if let Some(key) = idem_key.as_deref() {
                bot.idempotency_abort(key).await;
            }
            return Err((StatusCode::SERVICE_UNAVAILABLE,
                "force requires fresh same-market analysis with a valid model probability; refresh analysis first".into()));
        }
    };
    forced_analysis["force_requested_at"] = json!(Utc::now().to_rfc3339());

    // The analysis above already says BUY, so the signal gate is bypassed by
    // construction. Passing `hail_mary = true` here did nothing for that and
    // everything else wrong: it swapped the stake for `hail_mary_stake` and put
    // the entry behind the hail-mary rules — token under `hail_mary_max_price`
    // (5 cents) and BTC moved at least `hail_mary_min_btc_change` (2%) — so an
    // operator "force" could only ever open on a near-worthless token after a
    // big move. A force overrides the signal and nothing else; every other
    // risk rule still applies.
    match bot
        .execute_trade_serialized_result(&forced_analysis, false, override_amount)
        .await
    {
        Ok(Some(trade)) => {
            let (payload, pending) = entry_execution_payload(&bot, &trade).await;
            if let Some(key) = idem_key {
                bot.idempotency_put(key, payload.clone()).await;
            }
            if pending {
                metrics::inc_api_force_fail();
            } else {
                metrics::inc_api_force_success();
            }
            Ok(Json(payload))
        }
        Ok(None) => {
            let payload = json!({
                "status": "not_executed",
                "reason": "force trade did not result in an opened trade"
            });
            if let Some(key) = idem_key {
                bot.idempotency_put(key, payload.clone()).await;
            }
            metrics::inc_api_force_fail();
            Ok(Json(payload))
        }
        Err(e) => {
            // A dropped/timed-out response does not prove the dispatched order
            // failed. Keep its durable claim so a retry cannot submit it again.
            if e != crate::bot::state::ActorCallError::ResponseFailed {
                if let Some(key) = idem_key.as_deref() {
                    bot.idempotency_abort(key).await;
                }
            }
            metrics::inc_api_force_fail();
            tracing::warn!("force execute_trade_serialized_result failed: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "execution actor failure".into(),
            ))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/risk/status
// ─────────────────────────────────────────────────────────────────────────────
pub async fn risk_status(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let analysis_age_secs = bot.analysis_age_secs().await;
    let analysis_stale = bot.is_analysis_stale().await;
    let (kill_switch, kill_switch_cause) = bot.kill_switch_status();
    let degraded_no_trade = bot.degraded_no_trade_active();
    let l2_feed_stale = bot.l2_feed_stale();
    let l2_feed_age_ms = bot.l2_feed_age_ms().await;
    let l2_age_slo_ms = std::env::var("MACBOT_SLO_MAX_L2_AGE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(3_000);
    let l2_age_slo_breached = l2_feed_age_ms
        .map(|age| age > l2_age_slo_ms)
        .unwrap_or(true);
    let slo_enforce = std::env::var("MACBOT_SLO_ENFORCE")
        .ok()
        .map(|v| {
            let x = v.trim().to_ascii_lowercase();
            matches!(x.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(true);
    let slo_l2_breach_hold_ms = std::env::var("MACBOT_SLO_L2_BREACH_HOLD_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(15_000);
    let slo_kill_on_l2_breach = std::env::var("MACBOT_SLO_KILL_ON_L2_BREACH")
        .ok()
        .map(|v| {
            let x = v.trim().to_ascii_lowercase();
            matches!(x.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(true);
    let slo_kill_after_l2_trips = std::env::var("MACBOT_SLO_KILL_AFTER_L2_TRIPS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3);
    let slo_kill_after_backpressure_trips =
        std::env::var("MACBOT_SLO_KILL_AFTER_BACKPRESSURE_TRIPS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(2);
    let slo_recovery_hold_ms = std::env::var("MACBOT_SLO_RECOVERY_HOLD_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20_000);
    let slo_max_actor_cmd_drop_delta = std::env::var("MACBOT_SLO_MAX_ACTOR_CMD_DROPS_DELTA")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    let slo_max_outbox_drop_delta = std::env::var("MACBOT_SLO_MAX_OUTBOX_DROPS_DELTA")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    let full_resync_interval_secs = std::env::var("MACBOT_FULL_RESYNC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    let reconcile_max_live_drift = std::env::var("MACBOT_RECONCILE_MAX_LIVE_DRIFT")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let metric_snapshot = metrics::snapshot();
    let last_reconcile_summary = bot.last_reconcile_summary().await;
    let last_reconcile_error = bot.last_reconcile_error().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let macro_guard = crate::bot::macro_calendar::macro_guard_status(&cfg, Utc::now());
    let mut out = Map::new();
    out.insert("kill_switch".to_string(), json!(kill_switch));
    out.insert("current_run_id".to_string(), json!(bot.run_id()));
    out.insert("degraded_no_trade".to_string(), json!(degraded_no_trade));
    out.insert(
        "analysis_cycles_total".to_string(),
        json!(metric_snapshot.analysis_cycles),
    );
    out.insert(
        "analysis_snapshot_persist_fail_total".to_string(),
        json!(metric_snapshot.analysis_snapshot_persist_fail),
    );
    out.insert(
        "shadow_kill_switch_trips_total".to_string(),
        json!(metric_snapshot.shadow_kill_switch_trips),
    );
    out.insert(
        "execution_mode".to_string(),
        json!(cfg.execution_mode.clone()),
    );
    out.insert("analysis_age_secs".to_string(), json!(analysis_age_secs));
    out.insert("analysis_stale".to_string(), json!(analysis_stale));
    out.insert("l2_feed_stale".to_string(), json!(l2_feed_stale));
    out.insert("l2_feed_age_ms".to_string(), json!(l2_feed_age_ms));
    out.insert("l2_age_slo_ms".to_string(), json!(l2_age_slo_ms));
    out.insert(
        "l2_age_slo_breached".to_string(),
        json!(l2_age_slo_breached),
    );
    out.insert(
        "actor_cmd_drop_total".to_string(),
        json!(metric_snapshot.actor_cmd_drop),
    );
    out.insert(
        "outbox_dropped_total".to_string(),
        json!(metric_snapshot.outbox_dropped),
    );
    out.insert(
        "outbox_write_fail_total".to_string(),
        json!(metric_snapshot.outbox_write_fail),
    );
    out.insert(
        "slo_guard_trips_total".to_string(),
        json!(metric_snapshot.slo_guard_trips),
    );
    out.insert(
        "reconcile_full_runs_total".to_string(),
        json!(metric_snapshot.reconcile_full_runs),
    );
    out.insert(
        "reconcile_drift_events_total".to_string(),
        json!(metric_snapshot.reconcile_drift_events),
    );
    out.insert(
        "last_reconcile_summary".to_string(),
        json!(last_reconcile_summary),
    );
    out.insert(
        "last_reconcile_error".to_string(),
        json!(last_reconcile_error),
    );
    // Why trading stopped. A halted bot with no stated cause sends the operator
    // to the logs, which may have rotated. Not every halt path records one yet,
    // so this is null rather than invented when the cause is unknown.
    out.insert("kill_switch_reason".to_string(), json!(kill_switch_cause));
    out.insert(
        "trade_entry_block_kill_switch_total".to_string(),
        json!(metric_snapshot.trade_entry_block_kill_switch),
    );
    out.insert(
        "trade_entry_block_drawdown_total".to_string(),
        json!(metric_snapshot.trade_entry_block_drawdown),
    );
    out.insert(
        "trade_entry_block_alpha_freeze_total".to_string(),
        json!(metric_snapshot.trade_entry_block_alpha_freeze),
    );
    out.insert(
        "trade_entry_block_degraded_total".to_string(),
        json!(metric_snapshot.trade_entry_block_degraded),
    );
    out.insert(
        "trade_entry_block_circuit_breaker_total".to_string(),
        json!(metric_snapshot.trade_entry_block_circuit_breaker),
    );
    out.insert(
        "trade_entry_block_l2_stale_total".to_string(),
        json!(metric_snapshot.trade_entry_block_l2_stale),
    );
    out.insert("slo_enforce".to_string(), json!(slo_enforce));
    out.insert(
        "slo_l2_breach_hold_ms".to_string(),
        json!(slo_l2_breach_hold_ms),
    );
    out.insert(
        "slo_kill_on_l2_breach".to_string(),
        json!(slo_kill_on_l2_breach),
    );
    out.insert(
        "slo_kill_after_l2_trips".to_string(),
        json!(slo_kill_after_l2_trips),
    );
    out.insert(
        "slo_kill_after_backpressure_trips".to_string(),
        json!(slo_kill_after_backpressure_trips),
    );
    out.insert(
        "slo_recovery_hold_ms".to_string(),
        json!(slo_recovery_hold_ms),
    );
    out.insert(
        "slo_max_actor_cmd_drop_delta".to_string(),
        json!(slo_max_actor_cmd_drop_delta),
    );
    out.insert(
        "slo_max_outbox_drop_delta".to_string(),
        json!(slo_max_outbox_drop_delta),
    );
    out.insert(
        "actor_cmd_drop_high_total".to_string(),
        json!(metric_snapshot.actor_cmd_drop_high),
    );
    out.insert(
        "actor_cmd_drop_low_total".to_string(),
        json!(metric_snapshot.actor_cmd_drop_low),
    );
    out.insert(
        "actor_lane_high_depth".to_string(),
        json!(metric_snapshot.actor_lane_high_depth),
    );
    out.insert(
        "actor_lane_medium_depth".to_string(),
        json!(metric_snapshot.actor_lane_medium_depth),
    );
    out.insert(
        "actor_lane_low_depth".to_string(),
        json!(metric_snapshot.actor_lane_low_depth),
    );
    out.insert(
        "degraded_mode_transitions_total".to_string(),
        json!(metric_snapshot.degraded_mode_transitions),
    );
    out.insert(
        "balance_drift_warns_total".to_string(),
        json!(metric_snapshot.balance_drift_warns),
    );
    out.insert(
        "last_balance_drift_usd".to_string(),
        json!(bot.last_balance_drift_usd().await),
    );
    out.insert(
        "rollout_stage".to_string(),
        json!(std::env::var("MACBOT_ROLLOUT_STAGE").unwrap_or_else(|_| "full".to_string())),
    );
    out.insert(
        "freeze_alpha".to_string(),
        json!(std::env::var("MACBOT_FREEZE_ALPHA").ok()),
    );
    out.insert(
        "require_shadow_pass".to_string(),
        json!(std::env::var("MACBOT_REQUIRE_SHADOW_PASS").ok()),
    );
    out.insert(
        "shadow_pass".to_string(),
        json!(std::env::var("MACBOT_SHADOW_PASS").ok()),
    );
    out.insert(
        "full_resync_interval_secs".to_string(),
        json!(full_resync_interval_secs),
    );
    out.insert(
        "reconcile_max_live_drift".to_string(),
        json!(reconcile_max_live_drift),
    );
    out.insert(
        "analysis_stale_threshold_secs".to_string(),
        json!(cfg.analysis_stale_seconds),
    );
    out.insert(
        "max_open_positions".to_string(),
        json!(cfg.max_open_positions),
    );
    out.insert("max_slippage".to_string(), json!(cfg.max_slippage));
    out.insert(
        "auth_required".to_string(),
        json!(
            cfg.auth_required
                || cfg.mode_kind() == BotMode::Live
                || !cfg.private_rpc_url.is_empty()
        ),
    );
    out.insert(
        "allowed_ips_count".to_string(),
        json!(cfg.allowed_ips.len()),
    );
    out.insert(
        "idempotency_ttl_seconds".to_string(),
        json!(cfg.idempotency_ttl_seconds),
    );
    out.insert(
        "idempotency_max_entries".to_string(),
        json!(cfg.idempotency_max_entries),
    );
    out.insert("shadow_trading".to_string(), json!(cfg.shadow_trading));
    out.insert("macro_guard".to_string(), json!(macro_guard));
    out.insert(
        "eip1559_min_priority_gwei".to_string(),
        json!(cfg.eip1559_min_priority_gwei),
    );
    out.insert(
        "eip1559_max_priority_gwei".to_string(),
        json!(cfg.eip1559_max_priority_gwei),
    );
    out.insert(
        "eip1559_spread_trigger_pct".to_string(),
        json!(cfg.eip1559_spread_trigger_pct),
    );
    out.insert(
        "eip1559_priority_multiplier".to_string(),
        json!(cfg.eip1559_priority_multiplier),
    );
    out.insert(
        "auto_merge_rebalancer".to_string(),
        json!(cfg.auto_merge_rebalancer),
    );
    out.insert(
        "merge_min_edge_usd".to_string(),
        json!(cfg.merge_min_edge_usd),
    );
    out.insert("merge_gas_usd".to_string(), json!(cfg.merge_gas_usd));
    out.insert(
        "merge_max_share_mismatch_pct".to_string(),
        json!(cfg.merge_max_share_mismatch_pct),
    );
    Json(Value::Object(out))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/risk/kill-switch
// ─────────────────────────────────────────────────────────────────────────────
pub async fn set_kill_switch(
    State(bot): State<Arc<TradingBot>>,
    Json(body): Json<KillSwitchBody>,
) -> impl IntoResponse {
    let _recovery_control = if body.active {
        None
    } else {
        match bot.execution_state.recovery_control_gate.try_lock() {
            Ok(guard) => Some(guard),
            Err(_) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({"error":"receipt recovery is active; entries must remain frozen"})),
                )
            }
        }
    };
    if body.active {
        bot.halt_trading("operator", "engaged from the dashboard")
            .await;
    } else {
        bot.set_kill_switch(false);
    }
    let (kill_switch, kill_switch_reason) = bot.kill_switch_status();
    (
        StatusCode::OK,
        Json(json!({
            "kill_switch": kill_switch,
            "kill_switch_reason": kill_switch_reason,
        })),
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/resync/full
//   Trigger exchange+db authoritative live full resync immediately.
// ─────────────────────────────────────────────────────────────────────────────
pub async fn full_resync(
    State(bot): State<Arc<TradingBot>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let mode = bot.mode().await;
    if let Err(e) = bot.full_resync_live_state().await {
        bot.record_reconcile_error("full_resync", e.to_string())
            .await;
        tracing::warn!("Full resync request failed: {e}");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            "full resync failed".to_string(),
        ));
    }
    let metric_snapshot = metrics::snapshot();
    Ok(Json(json!({
        "ok": true,
        "mode": mode,
        "kill_switch": bot.kill_switch_active(),
        "last_reconcile_summary": bot.last_reconcile_summary().await,
        "reconcile_full_runs_total": metric_snapshot.reconcile_full_runs,
        "reconcile_drift_events_total": metric_snapshot.reconcile_drift_events
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/balance/reset  — reset runtime balance (non-live only)
// body: { "amount": 10.0 }
// ─────────────────────────────────────────────────────────────────────────────
pub async fn reset_balance(
    State(bot): State<Arc<TradingBot>>,
    Json(body): Json<ResetBalanceBody>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let payload = bot
        .reset_paper_balance_serialized(body.amount, body.force.unwrap_or(false))
        .await
        .map_err(|error| {
            let message = error.to_string();
            let status =
                if message.contains("execution actor") || message.contains("outcome unknown") {
                    StatusCode::SERVICE_UNAVAILABLE
                } else if message == "balance reset blocked in live mode"
                    || message == "amount must be finite and nonnegative"
                {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
            (status, message)
        })?;
    Ok(Json(payload))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/drawdown/reset — reset the durable available-cash peak only
// ─────────────────────────────────────────────────────────────────────────────
pub async fn reset_drawdown(
    State(bot): State<Arc<TradingBot>>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let peak_balance = bot.reset_drawdown_cash_peak().await.map_err(|error| {
        (
            if error.to_string().contains("execution actor")
                || error.to_string().contains("outcome unknown")
            {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            },
            format!("drawdown reset failed: {error}"),
        )
    })?;
    tracing::info!("Drawdown available-cash peak reset to {peak_balance:.2}");
    Ok(Json(json!({
        "ok": true,
        "drawdown_basis": "available_cash",
        "peak_balance": peak_balance,
        "daily_losses_reset": false,
        "kill_switch_cleared": false,
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/db/backup  — verified SQLite backup under artifacts/backups/
// ─────────────────────────────────────────────────────────────────────────────
pub async fn backup_database(
    State(bot): State<Arc<TradingBot>>,
    Json(body): Json<BackupDatabaseBody>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let path = validate_backup_path(body.path.as_deref())?;
    if body.overwrite.unwrap_or(false) {
        return Err((
            StatusCode::BAD_REQUEST,
            "API backups require a new path; overwrite is refused to preserve previous evidence"
                .into(),
        ));
    }
    let overwrite = false;

    crate::db::backup_database(&bot.db, &path)
        .await
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    Ok(Json(json!({
        "ok": true,
        "path": path,
        "overwrite": overwrite,
        "utc": Utc::now().to_rfc3339(),
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/ml/state  — ML model weights, accuracy, feature importance
// ─────────────────────────────────────────────────────────────────────────────
pub async fn ml_state(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let ml = bot.intelligence.ml_state.read().await;
    let active = crate::ml::should_use_ml(&ml);
    let importance = ml.model.feature_importance();

    Json(json!({
        "active": active,
        "samples_seen": ml.model.samples_seen,
        "learning_rate": ml.model.lr(),
        "bias": ml.model.bias,
        "weights": crate::ml::FEATURE_NAMES.iter()
            .zip(ml.model.weights.iter())
            .map(|(n, w)| json!({"name": n, "weight": w}))
            .collect::<Vec<_>>(),
        "feature_importance": importance.iter().take(5)
            .map(|(n, w)| json!({"name": n, "weight": w}))
            .collect::<Vec<_>>(),
        "accuracy": ml.tracker.to_json(),
        "normalizer": {
            "count": ml.normalizer.count,
        },
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/stats  — performance report + daily loss limit info
// ─────────────────────────────────────────────────────────────────────────────
pub async fn get_stats(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let report = bot.get_performance_report().await;

    let balance = bot.balance().await;
    let mode = bot.mode().await;
    let running = bot.loop_running();

    let (daily_loss_limit, initial_balance) = {
        let cfg = bot.control_plane.config.read().await;
        (cfg.daily_loss_limit, cfg.initial_balance)
    };

    // Partial sales belong to their UTC realization day even while a lot stays
    // open. Undated legacy aggregates block entry and remain an unknown total.
    let daily_pnl = report.get("today_pnl").and_then(Value::as_f64);
    let daily_pnl_evidence = report
        .get("daily_pnl_evidence")
        .cloned()
        .unwrap_or(Value::Null);
    let daily_evidence_complete = daily_pnl_evidence
        .get("evidence_complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let daily_loss_admission_blocked =
        !daily_evidence_complete || daily_pnl.is_none_or(|pnl| pnl <= -daily_loss_limit.abs());
    let daily_limit_remaining = daily_pnl.map(|pnl| daily_loss_limit + pnl);
    let circuit_breaker_active = daily_pnl.map(|pnl| pnl <= -daily_loss_limit.abs());

    // Circuit breaker cooldown info
    let cb_pause = {
        let pause = bot.trade_lifecycle.circuit_breaker_pause_until.read().await;
        pause
            .map(|until| {
                let now = chrono::Utc::now();
                if until > now {
                    serde_json::json!({
                        "active": true,
                        "until": until.to_rfc3339(),
                        "minutes_left": (until - now).num_minutes()
                    })
                } else {
                    serde_json::json!({"active": false})
                }
            })
            .unwrap_or(serde_json::json!({"active": false}))
    };

    Json(json!({
        "performance":             report,
        "balance":                 balance,
        "mode":                    mode,
        "loop_running":            running,
        "backtest_only":           bot.backtest_only(),
        "daily_pnl":               daily_pnl,
        "daily_pnl_evidence":      daily_pnl_evidence,
        "daily_loss_admission_blocked": daily_loss_admission_blocked,
        "daily_loss_limit":        daily_loss_limit,
        "daily_limit_remaining":   daily_limit_remaining,
        "circuit_breaker_active":  circuit_breaker_active,
        "circuit_breaker_cooldown": cb_pause,
        "initial_balance":         initial_balance,
    }))
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/btc-chart  — last 21 1-minute Binance candles + market context
// ─────────────────────────────────────────────────────────────────────────────
pub async fn btc_chart_data(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    if bot.backtest_only() {
        return Json(json!({
            "candles": [], "current_price": null, "start_price": null,
            "market_question": "", "market_end": "",
        }));
    }
    // Fetch the last 21 1-minute candles from Binance (21 so we always have a
    // complete "20 closed + 1 live" window without any partial-candle gaps).
    let url = "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&limit=21";
    let raw: Value = match HTTP.get(url).send().await {
        Ok(r) if r.status().is_success() => r.bounded_json().await.unwrap_or(Value::Array(vec![])),
        _ => Value::Array(vec![]),
    };

    let mut candles: Vec<Value> = vec![];
    if let Some(arr) = raw.as_array() {
        for c in arr {
            if let Some(row) = c.as_array() {
                let ts_ms = row.first().and_then(|v| v.as_u64()).unwrap_or(0);
                let open = row
                    .get(1)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let high = row
                    .get(2)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let low = row
                    .get(3)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                let close = row
                    .get(4)
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<f64>().ok())
                    .unwrap_or(0.0);
                if close > 0.0 {
                    candles.push(json!({
                        "ts":    ts_ms / 1000,
                        "open":  open,
                        "high":  high,
                        "low":   low,
                        "close": close,
                    }));
                }
            }
        }
    }

    // History is not a current quote. The stream's bounded cache read supplies
    // current price; absence stays absent when the price feed is down.
    let current_price = super::realtime::live_btc_price(&bot).await;
    let analysis = bot.analysis_state.last_analysis.read().await;
    let parsed = ParsedAnalysis::from_value(&analysis);
    let start_price = analysis
        .get("btc_start_price")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let market_question = parsed.market.question;
    let market_end = parsed.market.end_date;

    Json(json!({
        "candles":          candles,
        "current_price":    current_price,
        "start_price":      start_price,
        "market_question":  market_question,
        "market_end":       market_end,
    }))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// Resolve the requested outcome token, then use that token's current book.
/// A NO-token price is already a NO price: complementing it invents a different
/// quote. Metadata and fallback constants cannot substitute for a tradable book.
async fn fetch_fresh_market_price(
    bot: &Arc<TradingBot>,
    cid: &str,
    direction: Direction,
) -> Option<f64> {
    let signal = match direction {
        Direction::BuyYes => Signal::BuyYes,
        Direction::BuyNo => Signal::BuyNo,
        Direction::Unknown => return None,
    };
    let token = bot.get_clob_token_id(cid, signal).await?;
    let book = crate::clob::fetch_public_book_snapshot(&token).await.ok()?;
    match (book.best_bid, book.best_ask) {
        (Some(bid), Some(ask))
            if bid.is_finite() && ask.is_finite() && bid > 0.0 && ask < 1.0 && ask >= bid =>
        {
            Some((bid + ask) / 2.0)
        }
        _ => None,
    }
}

async fn fetch_market_meta(cid: &str) -> (String, String) {
    let url = format!("{}/markets", gamma_base());
    if let Ok(r) = HTTP.get(&url).query(&[("condition_ids", cid)]).send().await {
        if r.status().is_success() {
            if let Ok(raw) = r.bounded_json::<Value>().await {
                let v = raw
                    .as_array()
                    .and_then(|rows| rows.first())
                    .or_else(|| {
                        raw.get("data")
                            .and_then(Value::as_array)
                            .and_then(|rows| rows.first())
                    })
                    .unwrap_or(&raw);
                let question = v
                    .get("question")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| format!("BTC market {}", cid));
                let end_date = v
                    .get("endDate")
                    .or_else(|| v.get("end_date"))
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string)
                    .unwrap_or_default();
                return (question, end_date);
            }
        }
    }
    (format!("BTC market {}", cid), String::new())
}

fn backfill_market_meta(
    question: String,
    end_date: String,
    fetched_question: String,
    fetched_end_date: String,
) -> (String, String) {
    let q = if question.trim().is_empty() {
        fetched_question
    } else {
        question
    };
    let end = if end_date.trim().is_empty() {
        fetched_end_date
    } else {
        end_date
    };
    (q, end)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn receipt_recovery_excludes_unfreeze_and_config_but_allows_emergency_stop(
    ) -> anyhow::Result<()> {
        use super::*;
        let pool = crate::db::open(":memory:").await?;
        let cfg: crate::config::Config = serde_json::from_str(include_str!("../../config.json"))?;
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        let lease = bot.execution_state.recovery_control_gate.lock().await;
        bot.set_kill_switch(true);
        let start = start_loop(State(Arc::clone(&bot))).await.into_response();
        assert_eq!(start.status(), StatusCode::CONFLICT);
        assert!(!bot.loop_running());
        let clear = set_kill_switch(
            State(Arc::clone(&bot)),
            Json(KillSwitchBody { active: false }),
        )
        .await
        .into_response();
        assert_eq!(clear.status(), StatusCode::CONFLICT);
        assert!(bot.kill_switch_active());
        let patch = super::super::config_api::update_config(
            State(Arc::clone(&bot)),
            Json(json!({"execution_mode":"hybrid"})),
        )
        .await;
        assert_eq!(
            patch
                .err()
                .expect("configuration changed during recovery")
                .0,
            StatusCode::CONFLICT
        );
        let stopped = stop_loop(State(Arc::clone(&bot))).await.into_response();
        assert_eq!(stopped.status(), StatusCode::OK);
        let freeze = set_kill_switch(
            State(Arc::clone(&bot)),
            Json(KillSwitchBody { active: true }),
        )
        .await
        .into_response();
        assert_eq!(freeze.status(), StatusCode::OK);
        assert!(bot.kill_switch_active());
        drop(lease);
        let clear = set_kill_switch(
            State(Arc::clone(&bot)),
            Json(KillSwitchBody { active: false }),
        )
        .await
        .into_response();
        assert_eq!(clear.status(), StatusCode::OK);
        assert!(!bot.kill_switch_active());
        Ok(())
    }

    #[tokio::test]
    async fn partial_realization_is_dated_or_disclosed_as_unknown_across_all_dashboard_reads(
    ) -> anyhow::Result<()> {
        for dated in [false, true] {
            let pool = crate::db::open(":memory:").await?;
            let mut cfg: crate::config::Config =
                serde_json::from_str(include_str!("../../config.json"))?;
            cfg.mode = "simulation".into();
            let now = chrono::Utc::now();
            let trade = crate::db::Trade {
                timestamp: (now - chrono::Duration::days(1)).to_rfc3339(),
                market_id: "partial-daily".into(),
                direction: "BUY_YES".into(),
                entry_price: 0.5,
                amount: 1.0,
                shares: 2.0,
                status: "OPEN".into(),
                mode: "simulation".into(),
                pnl: Some(-0.5),
                partial_closed: Some(1),
                accounting_version: 1,
                ..Default::default()
            };
            let id = crate::db::insert_trade(&pool, &trade).await?;
            sqlx::query("UPDATE trades SET entry_shares=4.0, entry_cost=2.0 WHERE id=?")
                .bind(id)
                .execute(&pool)
                .await?;
            if dated {
                let mut tx = pool.begin().await?;
                crate::db::risk::record_realized_delta(
                    &mut tx,
                    id,
                    "partial-sale",
                    &now.to_rfc3339(),
                    -0.5,
                    2.0,
                )
                .await?;
                tx.commit().await?;
            }
            let bot = Arc::new(TradingBot::new(cfg, pool).await?);
            bot.runtime_state
                .backtest_only
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let response = get_stats(State(Arc::clone(&bot))).await.into_response();
            let body = axum::body::to_bytes(response.into_body(), 1_000_000).await?;
            let report: Value = serde_json::from_slice(&body)?;
            assert_eq!(report["daily_pnl_evidence"]["evidence_complete"], dated);
            assert_eq!(report["daily_loss_admission_blocked"], !dated);
            if dated {
                assert_eq!(report["daily_pnl"], -0.5);
                assert_eq!(report["performance"]["total_pnl_net"], 0.0);
            } else {
                assert!(report["daily_pnl"].is_null());
                assert!(report["circuit_breaker_active"].is_null());
                assert!(report["daily_limit_remaining"].is_null());
                assert_eq!(
                    report["daily_pnl_evidence"]["conservative_unallocated_loss"],
                    0.5
                );
            }
            let fast = super::super::realtime::fast_snapshot(&bot).await;
            assert_eq!(fast["daily_pnl"], report["daily_pnl"]);
            assert_eq!(fast["daily_pnl_evidence"], report["daily_pnl_evidence"]);
            let integrity =
                super::super::integrity::dashboard_integrity_report(&bot, false, 5).await;
            assert_eq!(
                integrity["checks"]["daily_pnl"]["runtime_daily_pnl"],
                report["daily_pnl"]
            );
            assert_eq!(
                integrity["checks"]["daily_pnl"]["evidence"]["evidence_complete"],
                dated
            );
            assert_eq!(
                integrity["incomplete_checks"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|check| check == "daily_realized_allocation_incomplete"),
                !dated
            );
            if dated {
                bot.control_plane.config.write().await.daily_loss_limit = 0.5;
                let response = get_stats(State(Arc::clone(&bot))).await.into_response();
                let body = axum::body::to_bytes(response.into_body(), 1_000_000).await?;
                let limited: Value = serde_json::from_slice(&body)?;
                assert_eq!(limited["daily_loss_admission_blocked"], true);
                assert_eq!(limited["circuit_breaker_active"], true);
                assert_eq!(limited["daily_limit_remaining"], 0.0);
            }
            bot.db.close().await;
        }
        Ok(())
    }

    #[test]
    fn force_preserves_real_probability_and_refuses_missing_or_stale_evidence() {
        let evidence = serde_json::json!({"p_up":0.3,"confidence":0.42,"regime":"CHOP","ts":"original",
            "market_data":{"market_id":"m","price":0.8}});
        let forced = super::forced_analysis_with_evidence(
            evidence.clone(),
            crate::domain::Direction::BuyNo,
            "m",
            0.7,
            "question".into(),
            "end".into(),
            false,
        )
        .unwrap();
        assert_eq!(forced["p_up"], 0.3);
        assert_eq!(forced["confidence"], 0.42);
        assert_eq!(forced["ts"], "original");
        assert!(forced["net_ev"].is_null());
        for (value, market, stale) in [
            (evidence.clone(), "other", false),
            (evidence.clone(), "m", true),
            (
                serde_json::json!({"market_data":{"market_id":"m"}}),
                "m",
                false,
            ),
        ] {
            assert!(super::forced_analysis_with_evidence(
                value,
                crate::domain::Direction::BuyYes,
                market,
                0.5,
                "question".into(),
                "end".into(),
                stale
            )
            .is_none());
        }
    }
    use super::*;
    use crate::bot::state::TradingBot;
    use axum::{extract::State, http::HeaderMap, response::IntoResponse};
    use std::sync::Arc;

    async fn invalid_live_bot(
        label: &str,
    ) -> anyhow::Result<(Arc<TradingBot>, std::path::PathBuf, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_api_{label}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.auth_required = false;
        cfg.api_key.clear();
        cfg.api_hmac_secret.clear();

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        Ok((bot, path, path_str))
    }

    fn cleanup_sqlite(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    #[test]
    fn backfill_market_meta_prefers_existing_values() {
        let (q, end) = backfill_market_meta(
            "Cached Question".to_string(),
            "2026-03-08T10:00:00Z".to_string(),
            "Fetched Question".to_string(),
            "2026-03-08T10:15:00Z".to_string(),
        );
        assert_eq!(q, "Cached Question");
        assert_eq!(end, "2026-03-08T10:00:00Z");
    }

    #[test]
    fn backfill_market_meta_fills_missing_end_date() {
        let (q, end) = backfill_market_meta(
            "Cached Question".to_string(),
            "".to_string(),
            "Fetched Question".to_string(),
            "2026-03-08T10:15:00Z".to_string(),
        );
        assert_eq!(q, "Cached Question");
        assert_eq!(end, "2026-03-08T10:15:00Z");
    }

    #[test]
    fn backfill_market_meta_fills_missing_question() {
        let (q, end) = backfill_market_meta(
            "".to_string(),
            "2026-03-08T10:00:00Z".to_string(),
            "Fetched Question".to_string(),
            "2026-03-08T10:15:00Z".to_string(),
        );
        assert_eq!(q, "Fetched Question");
        assert_eq!(end, "2026-03-08T10:00:00Z");
    }

    #[test]
    fn validate_backup_path_rejects_escape_and_absolute_paths() {
        let abs = validate_backup_path(Some("/tmp/out.sqlite"));
        assert!(abs.is_err());

        let escape = validate_backup_path(Some("../out.sqlite"));
        assert!(escape.is_err());

        let outside = validate_backup_path(Some("tmp/out.sqlite"));
        assert!(outside.is_err());
    }

    #[test]
    fn validate_backup_path_accepts_artifacts_backup_target() {
        let path = validate_backup_path(Some("artifacts/backups/manual.sqlite"))
            .expect("backup path should be accepted");
        assert_eq!(path, "artifacts/backups/manual.sqlite");
    }

    #[tokio::test]
    async fn absent_analysis_read_is_immediate_and_does_not_run_analysis() -> anyhow::Result<()> {
        let pool = crate::db::open(":memory:").await?;
        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".into();
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        let response = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            get_analysis(State(Arc::clone(&bot))),
        )
        .await?
        .into_response();
        let body = axum::body::to_bytes(response.into_body(), 4096).await?;
        let absent: Value = serde_json::from_slice(&body)?;
        assert_eq!(absent["analysis_freshness"]["never_run"], true);
        assert!(absent["analysis_freshness"]["age_secs"].is_null());
        assert!(bot.analysis_state.last_analysis.read().await.is_null());

        *bot.analysis_state.last_analysis.write().await = json!({"signal": "hold"});
        let response = get_analysis(State(Arc::clone(&bot))).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), 4096).await?;
        let cached: Value = serde_json::from_slice(&body)?;
        assert_eq!(cached["signal"], "hold");
        assert_eq!(cached["analysis_freshness"]["never_run"], false);
        bot.runtime_state
            .backtest_only
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let response = start_loop(State(Arc::clone(&bot))).await.into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(!bot.loop_running());
        let response = trigger_analysis(
            State(Arc::clone(&bot)),
            Query(AnalyzeQuery { market_cid: None }),
        )
        .await;
        assert_eq!(response.err().unwrap().0, StatusCode::CONFLICT);
        assert_eq!(
            require_live_preflight_ready(&bot, "trade")
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        bot.db.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn full_resync_endpoint_is_safe_in_simulation_mode() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_api_full_resync_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        let resp = full_resync(State(bot)).await;
        assert!(resp.is_ok());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn backup_database_endpoint_creates_verified_backup() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let db_path = std::env::temp_dir().join(format!("macbot_api_backup_{uniq}.sqlite"));
        let db_path_str = db_path.to_string_lossy().to_string();
        let pool = crate::db::open(&db_path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);

        let rel_path = format!("artifacts/backups/test-backup-{uniq}.sqlite");
        let _ = std::fs::remove_file(&rel_path);
        let response = backup_database(
            State(Arc::clone(&bot)),
            Json(BackupDatabaseBody {
                path: Some(rel_path.clone()),
                overwrite: Some(false),
            }),
        )
        .await;
        assert!(response.is_ok());
        assert!(FsPath::new(&rel_path).exists());
        let original = std::fs::read(&rel_path)?;
        let refused = backup_database(
            State(Arc::clone(&bot)),
            Json(BackupDatabaseBody {
                path: Some(rel_path.clone()),
                overwrite: Some(true),
            }),
        )
        .await;
        assert_eq!(refused.err().unwrap().0, StatusCode::BAD_REQUEST);
        assert_eq!(std::fs::read(&rel_path)?, original);
        let selection = super::trigger_analysis(
            State(Arc::clone(&bot)),
            Query(super::AnalyzeQuery {
                market_cid: Some("unsupported-selection".into()),
            }),
        )
        .await;
        assert_eq!(selection.err().unwrap().0, StatusCode::BAD_REQUEST);

        let _ = std::fs::remove_file(&rel_path);
        cleanup_sqlite(&db_path, &db_path_str);
        Ok(())
    }

    #[tokio::test]
    async fn start_loop_blocks_when_live_preflight_is_red() -> anyhow::Result<()> {
        let (bot, path, path_str) = invalid_live_bot("start_loop_block").await?;

        let response = start_loop(State(Arc::clone(&bot))).await.into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!bot.loop_running());

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn execute_trade_blocks_when_live_preflight_is_red() -> anyhow::Result<()> {
        let (bot, path, path_str) = invalid_live_bot("execute_block").await?;

        let err = match execute_trade_handler(State(bot), HeaderMap::new(), None).await {
            Ok(_) => panic!("preflight should block manual trade execution"),
            Err(err) => err,
        };

        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.contains("live preflight blocked trade execution"));

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn force_trade_blocks_when_live_preflight_is_red() -> anyhow::Result<()> {
        let (bot, path, path_str) = invalid_live_bot("force_block").await?;

        let err = match force_trade(
            State(bot),
            HeaderMap::new(),
            Query(ForceTradeQuery {
                direction: Some("BUY_YES".to_string()),
                market_cid: Some("test-market".to_string()),
                amount: Some(1.0),
            }),
        )
        .await
        {
            Ok(_) => panic!("preflight should block forced trade execution"),
            Err(err) => err,
        };

        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.contains("live preflight blocked force trade"));

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn cached_idempotent_trade_response_survives_preflight_block() -> anyhow::Result<()> {
        let (bot, path, path_str) = invalid_live_bot("idem_replay").await?;
        let header_key = "test-idempotency-key".to_string();
        let scoped_key = format!("trade:hail_mary=false:override_amount=none:{header_key}");
        bot.idempotency_put(scoped_key, json!({ "cached": true }))
            .await;

        let mut headers = HeaderMap::new();
        headers.insert("x-idempotency-key", header_key.parse()?);

        let response = execute_trade_handler(State(bot), headers, None)
            .await
            .expect("cached idempotent response should win")
            .into_response();

        assert_eq!(response.status(), StatusCode::OK);

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }
}

#[cfg(test)]
mod adversarial_entry_response_tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::connect_info::ConnectInfo,
        http::Request,
    };
    use hmac::{Hmac, Mac};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    struct RestoreEnv(Vec<(&'static str, Option<String>)>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (name, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    fn request() -> Request<Body> {
        let ts = Utc::now().timestamp().to_string();
        let nonce = hex::encode(rand::random::<[u8; 32]>());
        let body = r#"{"override_amount":5}"#;
        let canonical = crate::api::auth::request_canonical(
            &ts,
            &nonce,
            "POST",
            "/api/trade",
            "public-audit-entry",
            body,
        );
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"public-audit-hmac").unwrap();
        mac.update(canonical.as_bytes());
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/trade")
            .header("content-type", "application/json")
            .header("x-api-key", "public-audit-key")
            .header("x-idempotency-key", "public-audit-entry")
            .header("x-api-ts", ts)
            .header("x-api-nonce", nonce)
            .header("x-api-signature", hex::encode(mac.finalize().into_bytes()))
            .body(Body::from(body))
            .unwrap();
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 42092))));
        request
    }

    #[tokio::test]
    async fn entry_response_classifies_durable_buy_without_relabelling_rejected_sell(
    ) -> anyhow::Result<()> {
        let (bot, mut trade, path) = crate::bot::close::tests::maker_exit_fixture(5.0).await;
        let settled_paper = entry_execution_payload(&bot, &trade).await;
        assert!(!settled_paper.1);
        assert_eq!(
            settled_paper.0,
            json!(trade),
            "settled API response stays compatible"
        );
        trade.mode = "live".into();
        trade.order_id = Some("public-fixture-order".into());
        sqlx::query("INSERT INTO live_orders(order_id,trade_id,account,token_id,phase,price,size,fee_rate,fee_exponent,taker_only,exchange_address,protocol,state,created_at,updated_at) VALUES(?,?,'fixture-account','fixture-token','entry',0.5,5,0,1,1,'fixture-exchange',2,'pending',?,?)")
            .bind(trade.order_id.as_deref().unwrap()).bind(trade.id)
            .bind(Utc::now().to_rfc3339()).bind(Utc::now().to_rfc3339()).execute(&bot.db).await?;
        let pending = entry_execution_payload(&bot, &trade).await;
        assert!(
            pending.1,
            "partially confirmed BUY still requires its terminal proof"
        );
        assert_eq!(pending.0["status"], "pending");
        let displayed = bot.trade_with_live_fields(&trade).await;
        assert_eq!(displayed["entry_execution_status"], "pending");
        assert!(displayed["unrealized_pnl"].is_null());
        sqlx::query("UPDATE live_orders SET phase='exit',state='rejected' WHERE trade_id=?")
            .bind(trade.id)
            .execute(&bot.db)
            .await?;
        let rejected_sell = entry_execution_payload(&bot, &trade).await;
        assert!(
            !rejected_sell.1,
            "a retained rejected SELL identity is not a pending BUY"
        );
        assert_eq!(rejected_sell.0, json!(trade));
        trade.status = "CLOSING".into();
        assert_eq!(bot.entry_confirmation_state(&trade).await?, None);
        trade.status = "OPEN".into();
        trade.order_id = None;
        sqlx::query(
            "UPDATE live_orders SET phase='entry',state='audit_uncertain' WHERE trade_id=?",
        )
        .bind(trade.id)
        .execute(&bot.db)
        .await?;
        let uncertain = entry_execution_payload(&bot, &trade).await;
        assert!(uncertain.1);
        assert_eq!(uncertain.0["execution_status"], "audit_uncertain");
        assert_eq!(
            uncertain.0["trade"]["shares"], trade.shares,
            "retained accounted shares are not rewritten"
        );
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
        Ok(())
    }

    // Actual signed API -> actual actor -> durable BUY -> local mock venue
    // accepts before dropping its response. Never contacts an exchange.
    #[test]
    fn adversarial_unconfirmed_live_entry_is_reported_pending_not_executed() -> anyhow::Result<()> {
        let _lease = crate::env_util::test_env_lock();
        let entries = [
            ("MACBOT_API_KEY", ""),
            ("MACBOT_API_HMAC_SECRET", ""),
            ("MACBOT_API_SECRET", ""),
            ("MACBOT_PRIVATE_RPC_URL", ""),
            ("PRIVATE_KEY", "public-audit-wallet-present"),
            ("POLYMARKET_API_KEY", "public-audit-present"),
            ("POLYMARKET_SECRET", "public-audit-present"),
            ("POLYMARKET_PASSPHRASE", "public-audit-present"),
            ("POLYGON_RPC_URL", "https://public-audit-rpc.invalid"),
            (
                "MACBOT_DOTENV_PATH",
                "/private/tmp/macbot-public-adversarial-no-dotenv",
            ),
            ("MACBOT_DEV_CORS", "0"),
            ("MACBOT_ROLLOUT_STAGE", "full"),
        ];
        let restore = RestoreEnv(
            entries
                .iter()
                .map(|(name, _)| (*name, std::env::var(name).ok()))
                .collect(),
        );
        for (name, value) in entries {
            std::env::set_var(name, value);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let observed = runtime.block_on(async {
            let (bot, _, path, venue) = crate::bot::actor::maker_live_tests::fixture().await;
            bot.set_l2_feed_stale(false).await;
            bot.set_balance(100.0).await;
            {
                let mut cfg = bot.control_plane.config.write().await;
                cfg.execution_mode = "taker".into();
                cfg.initial_balance = 100.0;
                cfg.min_trade_usd = 1.0;
                cfg.trade_amount = 5.0;
                cfg.max_open_positions = 5;
                cfg.macro_event_guard_enabled = false;
                cfg.regime_allowlist.clear();
                cfg.session_allowlist.clear();
                cfg.blocked_hours_utc.clear();
                cfg.auth_required = true;
                cfg.api_key = "public-audit-key".into();
                cfg.api_hmac_secret = "public-audit-hmac".into();
            }
            {
                let mut state = venue.state.lock().unwrap();
                state.tight_deep_book = true;
                state.drop_post_reply = true;
            }
            bot.market_state.up_token_cache.insert(
                "entry-market".into(), (std::time::Instant::now(),"123456789".into()),
            );
            *bot.analysis_state.last_analysis.write().await = json!({
                "signal":"BUY_YES", "forced":false,
                "confidence":0.99,"raw_confidence":0.99,"p_up":0.99,"net_ev":0.9,"regime":"TREND_UP",
                "market_data":{"market_id":"entry-market","price":0.495,"liquidity":100000,
                    "question":"Offline unconfirmed API entry", "end_date":(Utc::now()+chrono::Duration::minutes(30)).to_rfc3339()}
            });
            bot.mark_analysis_fresh().await;
            let bot = Arc::new(bot);
            bot.start_execution_actor().await;
            let response = crate::api::router(Arc::clone(&bot)).oneshot(request()).await?;
            let http_status = response.status();
            let payload: Value = serde_json::from_slice(&to_bytes(response.into_body(), 64_000).await?)?;
            let pending = crate::db::live::pending_orders(&bot.db).await?;
            let post_count = venue.state.lock().unwrap().posts;
            let evidence_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_matches")
                .fetch_one(&bot.db).await?;
            let repeat = crate::api::router(Arc::clone(&bot)).oneshot(request()).await?;
            let repeat_payload: Value = serde_json::from_slice(&to_bytes(repeat.into_body(), 64_000).await?)?;
            let repeat_post_count = venue.state.lock().unwrap().posts;
            let observed = json!({"http_status":http_status.as_u16(),"payload":payload,
                "pending_entry_count":pending.iter().filter(|order|order.phase=="entry").count(),
                "venue_posts":post_count,"confirmed_receipt_fill_count":evidence_count,
                "repeat_payload":repeat_payload,"repeat_venue_posts":repeat_post_count,
                "kill_switch":bot.kill_switch_active(),"degraded":bot.degraded_no_trade_active()});
            bot.stop_execution_actor().await?;
            crate::bot::close::tests::cleanup_exit_fixture(&bot,&path).await;
            anyhow::Ok(observed)
        })?;
        drop(restore);
        println!(
            "offline API pending-entry evidence: {}",
            json!({
                "http_status":observed["http_status"],
                "status":observed["payload"]["status"],
                "shares":observed["payload"]["trade"]["shares"],
                "order_identity_retained":observed["payload"]["trade"]["order_id"].is_string(),
                "pending_entries":observed["pending_entry_count"],
                "posts":observed["venue_posts"],
                "receipt_fills":observed["confirmed_receipt_fill_count"],
                "repeat_is_cached":observed["repeat_payload"]==observed["payload"],
                "repeat_posts":observed["repeat_venue_posts"],
                "kill_switch":observed["kill_switch"],"degraded":observed["degraded"],
            })
        );
        assert_eq!(
            observed["venue_posts"], 1,
            "fixture must actually submit one durable local-venue BUY"
        );
        assert_eq!(
            observed["repeat_venue_posts"], 1,
            "repeat is cached, not a second submission"
        );
        assert_eq!(
            observed["pending_entry_count"], 1,
            "accepted entry must still lack settlement"
        );
        assert_eq!(
            observed["confirmed_receipt_fill_count"], 0,
            "no actual owned inventory was proved"
        );
        assert_eq!(
            observed["payload"]["trade"]["shares"].as_f64(),
            Some(0.0),
            "reserved budget cannot fabricate confirmed inventory"
        );
        assert_eq!(
            observed["payload"]["trade"]["entry_execution_status"],
            "pending"
        );
        assert_eq!(
            observed["repeat_payload"], observed["payload"],
            "cached pending result remains stable for the original request"
        );
        assert_eq!(
            observed["payload"]["status"], "pending",
            "an unconfirmed durable BUY cannot be returned as an executed OPEN trade"
        );
        Ok(())
    }
}
