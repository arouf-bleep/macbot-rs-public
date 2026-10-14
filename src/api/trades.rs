//! Trade-related HTTP handlers.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::bot::close::CloseIntent;
use crate::bot::idempotency::IdempotencyClaim;
use crate::bot::state::TradingBot;
use crate::db::{self, Trade};
use crate::domain::{Direction, TradeStatus};

// No losses means the ratio is undefined; JSON null avoids a false zero.
fn sum_profit_factor(pnls: impl Iterator<Item = f64>) -> Option<f64> {
    let (wins, losses) = pnls.fold((0.0, 0.0), |(wins, losses), p| {
        (wins + p.max(0.0), losses + (-p).max(0.0))
    });
    (losses > f64::EPSILON).then_some(wins / losses)
}

#[cfg(test)]
mod performance_ratio_tests {
    #[tokio::test]
    async fn close_all_reports_existing_unresolved_exit_and_keeps_its_claim() -> anyhow::Result<()>
    {
        use axum::{
            body::to_bytes,
            extract::State,
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
        };
        let pool = crate::db::open(":memory:").await?;
        let cfg = serde_json::from_str(include_str!("../../config.json"))?;
        let bot = std::sync::Arc::new(crate::bot::state::TradingBot::new(cfg, pool).await?);
        bot.portfolio_state
            .trades
            .write()
            .await
            .push(crate::db::Trade {
                id: 42,
                status: "CLOSING".into(),
                pnl: Some(99.0),
                ..Default::default()
            });
        let mut headers = HeaderMap::new();
        headers.insert("x-idempotency-key", "closing-batch".parse()?);
        let response = super::close_all(State(std::sync::Arc::clone(&bot)), headers)
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = to_bytes(response.into_body(), 4096).await?;
        let payload: serde_json::Value = serde_json::from_slice(&bytes)?;
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["closed"], 0);
        assert_eq!(payload["total_pnl"], 0.0);
        assert_eq!(payload["failed_trade_ids"], serde_json::json!([42]));
        assert!(matches!(
            bot.idempotency_claim_or_get("close_all:closing-batch")
                .await?,
            crate::bot::idempotency::IdempotencyClaim::InProgress
        ));
        bot.db.close().await;
        Ok(())
    }

    #[test]
    fn profit_factor_uses_totals_rather_than_average_trade_sizes() {
        assert_eq!(
            super::sum_profit_factor(std::iter::repeat_n(1.0, 10).chain([-5.0])),
            Some(2.0)
        );
        assert_eq!(super::sum_profit_factor([1.0, 0.0].into_iter()), None);
        assert_eq!(super::sum_profit_factor([-1.0].into_iter()), Some(0.0));
    }

    #[tokio::test]
    async fn trade_analysis_keeps_closing_exposure_without_realizing_its_result(
    ) -> anyhow::Result<()> {
        use axum::{body::to_bytes, extract::State, response::IntoResponse};
        let pool = crate::db::open(":memory:").await?;
        let cfg = serde_json::from_str(include_str!("../../config.json"))?;
        let bot = std::sync::Arc::new(crate::bot::state::TradingBot::new(cfg, pool).await?);
        crate::db::insert_trade(
            &bot.db,
            &crate::db::Trade {
                timestamp: chrono::Utc::now().to_rfc3339(),
                status: "CLOSING".into(),
                market_id: "closing-market".into(),
                direction: "BUY_YES".into(),
                entry_price: 0.5,
                amount: 1.0,
                shares: 2.0,
                pnl: Some(99.0),
                ..Default::default()
            },
        )
        .await?;
        let response = super::trade_analysis(State(std::sync::Arc::clone(&bot)))
            .await
            .into_response();
        let body = to_bytes(response.into_body(), 16_384).await?;
        let payload: serde_json::Value = serde_json::from_slice(&body)?;
        assert_eq!(payload["total_open"], 1);
        assert_eq!(payload["total_trades"], 1);
        assert_eq!(payload["total_closed"], 0);
        assert_eq!(payload["total_pnl_net"], 0.0);
        assert_eq!(payload["wins"], 0);
        bot.db.close().await;
        Ok(())
    }
}

// ── Query param types ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct TradesQuery {
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct CloseQuery {
    pub exit_price: Option<f64>,
    /// Explicit fraction of the current settled lot, strictly between 0 and 1.
    pub fraction: Option<f64>,
}

fn env_f64_or(key: &str, default: f64) -> f64 {
    crate::env_util::env_f64(key, default)
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/trades?status=OPEN
// ─────────────────────────────────────────────────────────────────────────────
pub async fn list_trades(
    State(bot): State<Arc<TradingBot>>,
    Query(q): Query<TradesQuery>,
) -> impl IntoResponse {
    let rows = match q.status.as_deref().map(TradeStatus::parse) {
        Some(status) => db::load_trades_by_status(&bot.db, status).await,
        None => db::load_all_trades(&bot.db).await,
    };
    let filtered = match rows {
        Ok(rows) => rows,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"failed to load trades"})),
            )
                .into_response()
        }
    };
    let mut trades_out: Vec<Value> = Vec::with_capacity(filtered.len());
    for t in &filtered {
        trades_out.push(bot.trade_with_live_fields(t).await);
    }
    let count = trades_out.len();
    Json(json!({
        "trades": trades_out,
        "count":  count,
    }))
    .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/trades/:id/fills
// ─────────────────────────────────────────────────────────────────────────────
pub async fn trade_fills(
    State(bot): State<Arc<TradingBot>>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let max_entry_drift_bps = env_f64_or("MACBOT_ENTRY_PARITY_MAX_DRIFT_BPS", 35.0).max(0.0);
    let max_exit_drift_bps = env_f64_or("MACBOT_EXIT_PARITY_MAX_DRIFT_BPS", 35.0).max(0.0);

    let trade = db::load_trade_by_id(&bot.db, id)
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to load trade".to_string(),
            )
        })?
        .ok_or_else(|| (StatusCode::NOT_FOUND, format!("trade {} not found", id)))?;

    let fills = db::load_trade_fills(&bot.db, id, 500).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to load trade fills".to_string(),
        )
    })?;
    let summary = db::trade_fill_summary(&bot.db, id).await.map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to compute fill summary".to_string(),
        )
    })?;
    let parity = db::trade_fill_parity(&trade, &summary, max_entry_drift_bps, max_exit_drift_bps);
    let redemption_evidence = if trade.exit_type.as_deref() == Some("redemption_confirmed") {
        db::redemption::evidence_for_trade(&bot.db, id)
            .await
            .map_err(|error| {
                tracing::warn!("Redemption evidence unavailable for trade {id}: {error}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "redemption accounting evidence is unavailable or contradictory".to_string(),
                )
            })?
    } else {
        None
    };
    let fills_out = fills
        .into_iter()
        .map(|fill| {
            let details_json = serde_json::from_str::<serde_json::Value>(&fill.details)
                .unwrap_or(serde_json::Value::Null);
            json!({
                "id": fill.id,
                "timestamp": fill.timestamp,
                "trade_id": fill.trade_id,
                "market_id": fill.market_id,
                "order_id": fill.order_id,
                "phase": fill.phase,
                "intended_price": fill.intended_price,
                "avg_price": fill.avg_price,
                "cumulative_size": fill.cumulative_size,
                "delta_size": fill.delta_size,
                "delta_price": fill.delta_price,
                "source": fill.source,
                "details": fill.details,
                "details_json": details_json,
            })
        })
        .collect::<Vec<_>>();

    Ok(Json(json!({
        "trade_id": id,
        "trade": trade,
        "fills": fills_out,
        "summary": summary,
        "parity_check": parity,
        "redemption_evidence": redemption_evidence,
    })))
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/trades/:id/close?exit_price=X
// ─────────────────────────────────────────────────────────────────────────────
pub async fn close_trade(
    State(bot): State<Arc<TradingBot>>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Query(q): Query<CloseQuery>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    if q.fraction
        .is_some_and(|fraction| !fraction.is_finite() || fraction <= 0.0 || fraction >= 1.0)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "partial close fraction must be finite and strictly between 0 and 1".into(),
        ));
    }
    if q.exit_price
        .is_some_and(|price| !price.is_finite() || !(0.0..=1.0).contains(&price))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "exit price must be finite and between 0 and 1".into(),
        ));
    }
    let cache_key = idempotency_key(&headers).map(|k| format!("close_trade:{id}:{k}"));
    if let Some(ref key) = cache_key {
        match bot
            .idempotency_claim_or_get(key)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        {
            IdempotencyClaim::Cached(cached) => return Ok(Json(cached)),
            IdempotencyClaim::Claimed => {}
            IdempotencyClaim::InProgress => {
                return Err((
                    StatusCode::CONFLICT,
                    "idempotent close request is already in progress".to_string(),
                ));
            }
        }
    }

    if let Some(fraction) = q.fraction {
        // Deliberate live reductions require a stable operator request identity.
        // Fresh signatures/nonces on retries must retain this same identity.
        if cache_key.is_none() {
            return Err((
                StatusCode::BAD_REQUEST,
                "partial closes require x-idempotency-key; reuse it if the outcome is unknown"
                    .into(),
            ));
        }
        match bot
            .partial_close_trade_serialized_with_intent(
                id,
                q.exit_price.unwrap_or(0.5),
                fraction,
                CloseIntent::Manual,
            )
            .await
        {
            Ok(Some(payload)) => {
                if payload["status"] != "pending" {
                    if let Some(key) = cache_key {
                        bot.idempotency_put(key, payload.clone()).await;
                    }
                }
                return Ok(Json(payload));
            }
            Ok(None) => {
                // A dropped result or failed accounting can already follow a
                // side effect. Keep the claim until evidence is inspected.
                return Ok(Json(json!({"ok":false,"id":id,"status":"pending",
                    "reason":"partial close outcome unavailable; inspect the position and order evidence before retrying"})));
            }
            Err(error) => {
                if error != crate::bot::state::ActorCallError::ResponseFailed {
                    if let Some(key) = cache_key.as_deref() {
                        bot.idempotency_abort(key).await;
                    }
                }
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "partial close execution actor failure: {error}; inspect before retrying"
                    ),
                ));
            }
        }
    }

    match bot
        .close_trade_serialized_with_intent(id, q.exit_price, CloseIntent::Manual)
        .await
    {
        Ok(Some(trade)) => {
            let payload = json!({
                "ok": trade.status_kind() == TradeStatus::Closed,
                "status": if trade.status_kind() == TradeStatus::Closed { "closed" } else { "unresolved" },
                "trade": trade,
            });
            if trade.status_kind() != TradeStatus::Closed {
                // Partial fills and unresolved exits may already have executed.
                // Preserve the claim until their venue evidence is reconciled.
                Err((
                    StatusCode::CONFLICT,
                    format!("trade {} remains open after close attempt", id),
                ))
            } else {
                if let Some(key) = cache_key {
                    bot.idempotency_put(key, payload.clone()).await;
                }
                Ok(Json(payload))
            }
        }
        Ok(None) => {
            let persisted = db::load_trade_by_id(&bot.db, id).await.map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("failed to inspect trade {} after close attempt: {e}", id),
                )
            })?;
            match persisted {
                Some(trade) if trade.status_kind() == TradeStatus::Closing => Err((
                    StatusCode::CONFLICT,
                    format!(
                        "trade {} has an unresolved exit and requires venue reconciliation",
                        id
                    ),
                )),
                Some(trade) if trade.is_open() => Err((
                    StatusCode::CONFLICT,
                    format!("trade {} remains open after close attempt", id),
                )),
                Some(_) | None => {
                    if let Some(key) = cache_key.as_deref() {
                        bot.idempotency_abort(key).await;
                    }
                    Err((
                        StatusCode::NOT_FOUND,
                        format!("trade {} not found or already closed", id),
                    ))
                }
            }
        }
        Err(e) => {
            if e != crate::bot::state::ActorCallError::ResponseFailed {
                if let Some(key) = cache_key.as_deref() {
                    bot.idempotency_abort(key).await;
                }
            }
            tracing::warn!("close_trade_serialized_result failed for trade_id={id}: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "execution actor failure".to_string(),
            ))
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/trades/close-all
// ─────────────────────────────────────────────────────────────────────────────
pub async fn close_all(
    State(bot): State<Arc<TradingBot>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let cache_key = idempotency_key(&headers).map(|k| format!("close_all:{k}"));
    if let Some(ref key) = cache_key {
        match bot.idempotency_claim_or_get(key).await {
            Ok(IdempotencyClaim::Cached(cached)) => return Json(cached).into_response(),
            Ok(IdempotencyClaim::Claimed) => {}
            Ok(IdempotencyClaim::InProgress) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({
                    "error": "idempotent close-all request is already in progress"
                    })),
                )
                    .into_response();
            }
            Err(e) => {
                tracing::warn!("close_all idempotency claim failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                    "error": "idempotency claim failed"
                    })),
                )
                    .into_response();
            }
        }
    }

    let all = bot.trades().await;
    let open_ids: Vec<i64> = all
        .iter()
        .filter(|t| t.status_kind() == TradeStatus::Open)
        .map(|t| t.id)
        .collect();

    let mut closed_trades: Vec<Value> = Vec::new();
    let mut total_pnl = 0.0_f64;
    let unresolved_ids: Vec<i64> = all
        .iter()
        .filter(|t| t.status_kind() == TradeStatus::Closing)
        .map(|t| t.id)
        .collect();
    let mut failed_trade_ids = unresolved_ids.clone();
    let mut still_open_trade_ids = unresolved_ids;
    let mut missing_trade_ids: Vec<i64> = Vec::new();
    let mut inspection_failed_trade_ids: Vec<i64> = Vec::new();

    for id in open_ids {
        match bot
            .close_trade_serialized_with_intent(id, None, CloseIntent::Manual)
            .await
        {
            Ok(Some(t)) => {
                if t.status_kind() != TradeStatus::Closed {
                    failed_trade_ids.push(id);
                    still_open_trade_ids.push(id);
                } else {
                    total_pnl += t.net_realized_pnl();
                    closed_trades.push(json!(t));
                }
            }
            Ok(None) => match db::load_trade_by_id(&bot.db, id).await {
                Ok(Some(t)) if t.status_kind() != TradeStatus::Closed => {
                    failed_trade_ids.push(id);
                    still_open_trade_ids.push(id);
                }
                Ok(Some(t)) => {
                    total_pnl += t.net_realized_pnl();
                    closed_trades.push(json!(t));
                }
                Ok(None) => {
                    failed_trade_ids.push(id);
                    missing_trade_ids.push(id);
                }
                Err(e) => {
                    tracing::warn!("close_all post-close inspect failed trade_id={id}: {e}");
                    failed_trade_ids.push(id);
                    inspection_failed_trade_ids.push(id);
                }
            },
            Err(e) => {
                tracing::warn!("close_all actor failure trade_id={id}: {e}");
                failed_trade_ids.push(id);
            }
        }
    }

    let payload = json!({
        "ok": failed_trade_ids.is_empty(),
        "closed":    closed_trades.len(),
        "total_pnl": total_pnl,
        "trades":    closed_trades,
        "failed_trade_ids": failed_trade_ids,
        "still_open_trade_ids": still_open_trade_ids,
        "missing_trade_ids": missing_trade_ids,
        "inspection_failed_trade_ids": inspection_failed_trade_ids,
    });

    if failed_trade_ids.is_empty() {
        if let Some(key) = cache_key {
            bot.idempotency_put(key, payload.clone()).await;
        }
        return (StatusCode::OK, Json(payload)).into_response();
    }

    // A batch can close some trades before a later command loses its reply.
    // Keep the batch claim on partial/unknown results; never replay the batch.
    let status = if still_open_trade_ids.is_empty() {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::CONFLICT
    };

    (status, Json(payload)).into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/trades/analysis
//   Comprehensive statistics computed from the trade history.
// ─────────────────────────────────────────────────────────────────────────────
pub async fn trade_analysis(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let rows = match db::load_all_trades(&bot.db).await {
        Ok(rows) => rows,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"failed to load trades for analysis"})),
            )
                .into_response();
        }
    };
    let closed: Vec<&Trade> = rows
        .iter()
        .filter(|trade| trade.status_kind() == TradeStatus::Closed)
        .collect();
    let open: Vec<&Trade> = rows
        .iter()
        .filter(|trade| trade.has_unsettled_exposure())
        .collect();

    let total_closed = closed.len();
    let total_open = open.len();
    let total_trades = total_closed + total_open;

    // Win/Loss — defined as PnL > 0
    let win_count: usize = closed.iter().filter(|t| t.net_realized_pnl() > 0.0).count();
    let loss_count: usize = closed
        .iter()
        .filter(|t| t.net_realized_pnl() <= 0.0)
        .count();
    let win_rate = if total_closed > 0 {
        win_count as f64 / total_closed as f64 * 100.0
    } else {
        0.0
    };

    // PnL aggregates (gross vs net)
    let total_pnl: f64 = closed.iter().map(|t| t.pnl.unwrap_or(0.0)).sum();
    let total_pnl_net: f64 = closed.iter().map(|t| t.net_realized_pnl()).sum();

    let avg_win: f64 = {
        let wins: Vec<f64> = closed
            .iter()
            .map(|t| t.pnl.unwrap_or(0.0))
            .filter(|p| *p > 0.0)
            .collect();
        if wins.is_empty() {
            0.0
        } else {
            wins.iter().sum::<f64>() / wins.len() as f64
        }
    };
    let avg_win_net: f64 = {
        let wins: Vec<f64> = closed
            .iter()
            .map(|t| t.net_realized_pnl())
            .filter(|p| *p > 0.0)
            .collect();
        if wins.is_empty() {
            0.0
        } else {
            wins.iter().sum::<f64>() / wins.len() as f64
        }
    };

    let avg_loss: f64 = {
        let losses: Vec<f64> = closed
            .iter()
            .map(|t| t.pnl.unwrap_or(0.0))
            .filter(|p| *p <= 0.0)
            .collect();
        if losses.is_empty() {
            0.0
        } else {
            losses.iter().sum::<f64>() / losses.len() as f64
        }
    };
    let avg_loss_net: f64 = {
        let losses: Vec<f64> = closed
            .iter()
            .map(|t| t.net_realized_pnl())
            .filter(|p| *p <= 0.0)
            .collect();
        if losses.is_empty() {
            0.0
        } else {
            losses.iter().sum::<f64>() / losses.len() as f64
        }
    };

    let profit_factor = sum_profit_factor(closed.iter().map(|t| t.pnl.unwrap_or(0.0)));
    let profit_factor_net = sum_profit_factor(closed.iter().map(|t| t.net_realized_pnl()));

    // By direction
    let yes_trades: Vec<&Trade> = closed
        .iter()
        .filter(|t| t.direction_kind() == Direction::BuyYes)
        .copied()
        .collect();
    let no_trades: Vec<&Trade> = closed
        .iter()
        .filter(|t| t.direction_kind() == Direction::BuyNo)
        .copied()
        .collect();
    let by_direction = json!({
        "BUY_YES": {
            "count": yes_trades.len(),
            "wins":  yes_trades.iter().filter(|t| t.net_realized_pnl() > 0.0).count(),
            "pnl":   yes_trades.iter().map(|t| t.pnl.unwrap_or(0.0)).sum::<f64>(),
        },
        "BUY_NO": {
            "count": no_trades.len(),
            "wins":  no_trades.iter().filter(|t| t.net_realized_pnl() > 0.0).count(),
            "pnl":   no_trades.iter().map(|t| t.pnl.unwrap_or(0.0)).sum::<f64>(),
        },
    });

    // By exit type
    let mut by_exit: std::collections::HashMap<String, (usize, f64)> =
        std::collections::HashMap::new();
    for t in &closed {
        let et = t.exit_type.clone().unwrap_or_else(|| "unknown".into());
        let entry = by_exit.entry(et).or_insert((0, 0.0));
        entry.0 += 1;
        entry.1 += t.pnl.unwrap_or(0.0);
    }
    let by_exit_json: serde_json::Map<String, Value> = by_exit
        .into_iter()
        .map(|(k, (count, pnl))| (k, json!({"count": count, "pnl": pnl})))
        .collect();

    // By source
    let mut by_source: std::collections::HashMap<String, (usize, usize, f64)> =
        std::collections::HashMap::new();
    for t in &closed {
        let src = t.source.clone();
        let entry = by_source.entry(src).or_insert((0, 0, 0.0));
        entry.0 += 1;
        if t.net_realized_pnl() > 0.0 {
            entry.1 += 1;
        }
        entry.2 += t.pnl.unwrap_or(0.0);
    }
    let by_source_json: serde_json::Map<String, Value> = by_source
        .into_iter()
        .map(|(k, (count, wins, pnl))| (k, json!({"count": count, "wins": wins, "pnl": pnl})))
        .collect();

    // Confidence stats
    let avg_conf: f64 = if !closed.is_empty() {
        closed.iter().map(|t| t.confidence).sum::<f64>() / closed.len() as f64
    } else {
        0.0
    };

    let winning_confs: Vec<f64> = closed
        .iter()
        .filter(|t| t.net_realized_pnl() > 0.0)
        .map(|t| t.confidence)
        .collect();
    let avg_winning_conf = if !winning_confs.is_empty() {
        winning_confs.iter().sum::<f64>() / winning_confs.len() as f64
    } else {
        0.0
    };

    // Cumulative PnL series for chart
    let mut sorted_closed = closed.clone();
    sorted_closed.sort_by(|a, b| closed_at(a).cmp(closed_at(b)));
    let mut cumulative = 0.0_f64;
    let pnl_series: Vec<Value> = sorted_closed
        .iter()
        .map(|t| {
            cumulative += t.pnl.unwrap_or(0.0);
            json!({"ts": closed_at(t), "pnl": cumulative})
        })
        .collect();
    let mut cumulative_net = 0.0_f64;
    let pnl_series_net: Vec<Value> = sorted_closed
        .iter()
        .map(|t| {
            cumulative_net += t.net_realized_pnl();
            json!({"ts": closed_at(t), "pnl": cumulative_net})
        })
        .collect();

    // Recent 10 closed trades (newest first)
    let mut recent = sorted_closed.clone();
    recent.sort_by(|a, b| closed_at(b).cmp(closed_at(a)));
    let recent_10: Vec<Value> = recent.iter().take(10).map(|t| json!(*t)).collect();

    // Max drawdown and Sharpe
    let max_drawdown = compute_max_drawdown(&sorted_closed);
    let sharpe = compute_sharpe(&sorted_closed);
    let net_pnls: Vec<f64> = sorted_closed.iter().map(|t| t.net_realized_pnl()).collect();
    let max_drawdown_net = compute_max_drawdown_from_pnls(&net_pnls);
    let sharpe_net = compute_sharpe_from_pnls(&net_pnls);

    Json(json!({
        "total_trades":           total_trades,
        "total_closed":           total_closed,
        "total_open":             total_open,
        "wins":                   win_count,
        "losses":                 loss_count,
        "win_rate":               win_rate,
        "total_pnl":              total_pnl,
        "total_pnl_net":          total_pnl_net,
        "avg_win":                avg_win,
        "avg_win_net":            avg_win_net,
        "avg_loss":               avg_loss,
        "avg_loss_net":           avg_loss_net,
        "profit_factor":          profit_factor,
        "profit_factor_net":      profit_factor_net,
        "max_drawdown":           max_drawdown,
        "max_drawdown_net":       max_drawdown_net,
        "sharpe_ratio":           sharpe,
        "sharpe_ratio_net":       sharpe_net,
        "sharpe_basis": "unannualized_per_trade_dollar_pnl",
        "avg_confidence":         avg_conf,
        "avg_winning_confidence": avg_winning_conf,
        "by_direction":           by_direction,
        "by_exit_type":           Value::Object(by_exit_json),
        "by_source":              Value::Object(by_source_json),
        "pnl_series":             pnl_series,
        "pnl_series_net":         pnl_series_net,
        "recent_10":              recent_10,
    }))
    .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/signal-stats
//   Per-signal accuracy based on closed trades that have signals_json stored.
// ─────────────────────────────────────────────────────────────────────────────
pub async fn signal_stats(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let closed_trades = match db::load_closed_trades(&bot.db).await {
        Ok(rows) => rows,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error":"failed to load trades for signal stats"})),
            )
                .into_response();
        }
    };

    #[derive(Default)]
    struct Acc {
        trades: usize,
        correct: usize,
        total_value: f64,
    }

    let mut per_signal: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();

    for t in &closed_trades {
        let was_correct = match t.was_correct {
            Some(1) => true,
            Some(0) => false,
            _ => continue,
        };

        let signals_str = match &t.signals_json {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };

        // signals_json is {"signal_name": signed_value} where + = voted UP/YES
        let signal_map: std::collections::HashMap<String, f64> =
            match serde_json::from_str(signals_str) {
                Ok(m) => m,
                Err(_) => continue,
            };

        // True market outcome: was BTC actually UP?
        // BTC UP  ↔ was_correct && direction == BUY_YES
        //         OR !was_correct && direction == BUY_NO
        let outcome_up = match t.direction_kind() {
            Direction::BuyYes => was_correct,
            Direction::BuyNo => !was_correct,
            Direction::Unknown => continue,
        };

        for (name, &value) in &signal_map {
            // A neutral signal abstained; treating it as DOWN invents a vote.
            if !value.is_finite() || value == 0.0 {
                continue;
            }
            let signal_voted_up = value > 0.0;
            let signal_correct = signal_voted_up == outcome_up;

            let entry = per_signal.entry(name.clone()).or_default();
            entry.trades += 1;
            entry.total_value += value.abs();
            if signal_correct {
                entry.correct += 1;
            }
        }
    }

    let result: serde_json::Map<String, Value> = per_signal
        .into_iter()
        .map(|(name, acc)| {
            let accuracy = if acc.trades > 0 {
                acc.correct as f64 / acc.trades as f64
            } else {
                0.0
            };
            let avg_value = if acc.trades > 0 {
                acc.total_value / acc.trades as f64
            } else {
                0.0
            };
            (
                name,
                json!({
                    "trades":    acc.trades,
                    "correct":   acc.correct,
                    "accuracy":  accuracy,
                    "avg_value": avg_value,
                }),
            )
        })
        .collect();

    let total_with_signals = closed_trades
        .iter()
        .filter(|t| t.signals_json.as_deref().unwrap_or("").len() > 2)
        .count();

    Json(json!({
        "total_closed_with_signals": total_with_signals,
        "signals": Value::Object(result),
    }))
    .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/attribution
//   Closed-trade attribution by calibration version + source + mode + run_id.
// ─────────────────────────────────────────────────────────────────────────────
/// Why the bot did not trade.
///
/// Rejections were recorded from the beginning and never surfaced anywhere, so
/// the largest category of the bot's behaviour could not be inspected without
/// opening the database by hand. A run that declines almost every market is a
/// different object from one that trades them, and this is the evidence for
/// which one happened.
pub async fn rejections(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    match db::rejection_summary(&bot.db, 20).await {
        Ok(summary) => Json(json!({
            "total": summary.total,
            "distinct_reasons": summary.distinct_reasons,
            "reasons": summary.reasons,
        }))
        .into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to summarize rejections: {error}") })),
        )
            .into_response(),
    }
}

pub async fn attribution(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let closed_rows = match db::load_closed_trades(&bot.db).await {
        Ok(rows) => rows,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "failed to load trades for attribution" })),
            )
                .into_response();
        }
    };

    #[derive(Default)]
    struct Acc {
        trades: usize,
        wins: usize,
        pnl: f64,
    }

    let closed: Vec<&Trade> = closed_rows.iter().collect();

    let mut by_cal: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();
    let mut by_source: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();
    let mut by_run_id: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();
    let mut by_mode: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();

    for t in &closed {
        let pnl = t.net_realized_pnl();
        let won = pnl > 0.0;

        let cal_key = t
            .calibration_version
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();
        let cal = by_cal.entry(cal_key).or_default();
        cal.trades += 1;
        cal.pnl += pnl;
        if won {
            cal.wins += 1;
        }

        let src = by_source.entry(t.source.clone()).or_default();
        src.trades += 1;
        src.pnl += pnl;
        if won {
            src.wins += 1;
        }

        let mode = by_mode.entry(t.mode.clone()).or_default();
        mode.trades += 1;
        mode.pnl += pnl;
        if won {
            mode.wins += 1;
        }

        let run = by_run_id
            .entry(
                t.run_id
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("unknown")
                    .to_string(),
            )
            .or_default();
        run.trades += 1;
        run.pnl += pnl;
        if won {
            run.wins += 1;
        }
    }

    let mut cal_rows: Vec<Value> = by_cal
        .into_iter()
        .map(|(k, v)| {
            let wr = if v.trades > 0 {
                v.wins as f64 / v.trades as f64
            } else {
                0.0
            };
            let avg = if v.trades > 0 {
                v.pnl / v.trades as f64
            } else {
                0.0
            };
            json!({
                "calibration_version": k,
                "trades": v.trades,
                "wins": v.wins,
                "win_rate": wr,
                "total_pnl": v.pnl,
                "avg_pnl": avg
            })
        })
        .collect();
    cal_rows.sort_by(|a, b| {
        let a_t = a.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        let b_t = b.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        b_t.cmp(&a_t)
    });

    let mut source_rows: Vec<Value> = by_source
        .into_iter()
        .map(|(k, v)| {
            let wr = if v.trades > 0 {
                v.wins as f64 / v.trades as f64
            } else {
                0.0
            };
            let avg = if v.trades > 0 {
                v.pnl / v.trades as f64
            } else {
                0.0
            };
            json!({
                "source": k,
                "trades": v.trades,
                "wins": v.wins,
                "win_rate": wr,
                "total_pnl": v.pnl,
                "avg_pnl": avg
            })
        })
        .collect();
    source_rows.sort_by(|a, b| {
        let a_t = a.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        let b_t = b.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        b_t.cmp(&a_t)
    });

    let mut mode_rows: Vec<Value> = by_mode
        .into_iter()
        .map(|(k, v)| {
            let wr = if v.trades > 0 {
                v.wins as f64 / v.trades as f64
            } else {
                0.0
            };
            let avg = if v.trades > 0 {
                v.pnl / v.trades as f64
            } else {
                0.0
            };
            json!({
                "mode": k,
                "trades": v.trades,
                "wins": v.wins,
                "win_rate": wr,
                "total_pnl": v.pnl,
                "avg_pnl": avg
            })
        })
        .collect();
    mode_rows.sort_by(|a, b| {
        let a_t = a.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        let b_t = b.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        b_t.cmp(&a_t)
    });

    let mut run_rows: Vec<Value> = by_run_id
        .into_iter()
        .map(|(k, v)| {
            let wr = if v.trades > 0 {
                v.wins as f64 / v.trades as f64
            } else {
                0.0
            };
            let avg = if v.trades > 0 {
                v.pnl / v.trades as f64
            } else {
                0.0
            };
            json!({
                "run_id": k,
                "trades": v.trades,
                "wins": v.wins,
                "win_rate": wr,
                "total_pnl": v.pnl,
                "avg_pnl": avg
            })
        })
        .collect();
    run_rows.sort_by(|a, b| {
        let a_t = a.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        let b_t = b.get("trades").and_then(|v| v.as_u64()).unwrap_or(0);
        b_t.cmp(&a_t)
    });

    let current_calibration = bot.control_plane.config.read().await.calibration_version();
    let current_run_id = bot.run_id().to_string();

    Json(json!({
        "closed_trades": closed.len(),
        "current_calibration_version": current_calibration,
        "current_run_id": current_run_id,
        "by_calibration": cal_rows,
        "by_source": source_rows,
        "by_mode": mode_rows,
        "by_run_id": run_rows
    }))
    .into_response()
}

fn closed_at(trade: &Trade) -> &str {
    trade
        .exit_timestamp
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or(&trade.timestamp)
}

// ── Stat helpers ──────────────────────────────────────────────────────────────

/// Maximum peak-to-trough drawdown over the sorted closed-trade sequence.
fn compute_max_drawdown(sorted: &[&Trade]) -> f64 {
    let mut peak = 0.0_f64;
    let mut cum = 0.0_f64;
    let mut max_dd = 0.0_f64;
    for t in sorted {
        cum += t.pnl.unwrap_or(0.0);
        peak = peak.max(cum);
        max_dd = max_dd.max(peak - cum);
    }
    max_dd
}

/// Unannualized mean/std of per-trade dollar P&L; no time/return assumptions.
fn compute_sharpe(sorted: &[&Trade]) -> f64 {
    if sorted.len() < 2 {
        return 0.0;
    }
    let pnls: Vec<f64> = sorted.iter().map(|t| t.pnl.unwrap_or(0.0)).collect();
    let n = pnls.len() as f64;
    let mean = pnls.iter().sum::<f64>() / n;
    let var = pnls.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std = var.sqrt();
    if std < f64::EPSILON {
        0.0
    } else {
        mean / std
    }
}

fn compute_max_drawdown_from_pnls(pnls: &[f64]) -> f64 {
    let mut peak = 0.0_f64;
    let mut cum = 0.0_f64;
    let mut max_dd = 0.0_f64;
    for p in pnls {
        cum += *p;
        peak = peak.max(cum);
        max_dd = max_dd.max(peak - cum);
    }
    max_dd
}

fn compute_sharpe_from_pnls(pnls: &[f64]) -> f64 {
    if pnls.len() < 2 {
        return 0.0;
    }
    let n = pnls.len() as f64;
    let mean = pnls.iter().sum::<f64>() / n;
    let var = pnls.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let std = var.sqrt();
    if std < f64::EPSILON {
        0.0
    } else {
        mean / std
    }
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod deliberate_partial_api_tests {
    use super::*;
    use crate::bot::{actor::ExecutionCommand, idempotency::IdempotencyClaim};

    async fn bot() -> Arc<TradingBot> {
        let pool = db::open(":memory:").await.unwrap();
        let cfg = serde_json::from_str(include_str!("../../config.json")).unwrap();
        Arc::new(TradingBot::new(cfg, pool).await.unwrap())
    }

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-idempotency-key", "deliberate-half".parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn invalid_fractions_and_missing_identity_are_refused_before_actor_dispatch() {
        let bot = bot().await;
        for fraction in [f64::NAN, f64::INFINITY, 0.0, 1.0, -0.5] {
            let response = close_trade(
                State(bot.clone()),
                headers(),
                Path(7),
                Query(CloseQuery {
                    exit_price: None,
                    fraction: Some(fraction),
                }),
            )
            .await;
            assert!(matches!(response, Err((StatusCode::BAD_REQUEST, _))));
        }
        let response = close_trade(
            State(bot.clone()),
            HeaderMap::new(),
            Path(7),
            Query(CloseQuery {
                exit_price: None,
                fraction: Some(0.5),
            }),
        )
        .await;
        assert!(matches!(response, Err((StatusCode::BAD_REQUEST, _))));
        assert!(
            matches!(
                bot.idempotency_claim_or_get("close_trade:7:deliberate-half")
                    .await
                    .unwrap(),
                IdempotencyClaim::Claimed
            ),
            "validation must not consume the request identity"
        );
        bot.db.close().await;
    }

    #[tokio::test]
    async fn settled_partial_result_is_cached_and_retry_cannot_dispatch_a_second_sale() {
        let bot = bot().await;
        let (tx, mut receiver) = tokio::sync::mpsc::channel(2);
        *bot.execution_state.execution_tx.write().await = Some(tx);
        let worker = tokio::spawn(async move {
            let Some(ExecutionCommand::PartialCloseTrade {
                fraction,
                intent,
                resp,
                ..
            }) = receiver.recv().await
            else {
                panic!("expected partial command")
            };
            assert_eq!(fraction, 0.5);
            assert_eq!(intent, CloseIntent::Manual);
            resp.send(Some(
                json!({"ok":true,"status":"settled","remaining_shares":6.0,"pnl":0.87}),
            ))
            .unwrap();
            receiver
        });
        let response = close_trade(
            State(bot.clone()),
            headers(),
            Path(7),
            Query(CloseQuery {
                exit_price: None,
                fraction: Some(0.5),
            }),
        )
        .await
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let mut receiver = worker.await.unwrap();
        let response = close_trade(
            State(bot.clone()),
            headers(),
            Path(7),
            Query(CloseQuery {
                exit_price: None,
                fraction: Some(0.5),
            }),
        )
        .await
        .unwrap()
        .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["pnl"], 0.87);
        assert!(
            receiver.try_recv().is_err(),
            "same identity must not sell the remainder again"
        );
        bot.db.close().await;
    }

    #[tokio::test]
    async fn pending_partial_result_keeps_the_durable_claim() {
        let bot = bot().await;
        let (tx, mut receiver) = tokio::sync::mpsc::channel(2);
        *bot.execution_state.execution_tx.write().await = Some(tx);
        let worker = tokio::spawn(async move {
            let Some(ExecutionCommand::PartialCloseTrade { resp, .. }) = receiver.recv().await
            else {
                panic!("expected partial command")
            };
            resp.send(Some(
                json!({"ok":false,"status":"pending","reason":"wallet receipt incomplete"}),
            ))
            .unwrap();
        });
        let response = close_trade(
            State(bot.clone()),
            headers(),
            Path(7),
            Query(CloseQuery {
                exit_price: None,
                fraction: Some(0.5),
            }),
        )
        .await
        .unwrap()
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        worker.await.unwrap();
        assert!(matches!(
            bot.idempotency_claim_or_get("close_trade:7:deliberate-half")
                .await
                .unwrap(),
            IdempotencyClaim::InProgress
        ));
        bot.db.close().await;
    }
}
