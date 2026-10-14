//! Authenticated evidence export and conservative terminal-order rechecks.
use crate::bot::state::TradingBot;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde_json::{json, Value};
use sqlx::{Column, Row, TypeInfo, ValueRef};
use std::sync::Arc;

type ApiError = (StatusCode, String);

fn row_json(row: sqlx::sqlite::SqliteRow) -> anyhow::Result<Value> {
    let mut object = serde_json::Map::new();
    for (index, column) in row.columns().iter().enumerate() {
        let raw = row.try_get_raw(index)?;
        let value = if raw.is_null() {
            Value::Null
        } else {
            match raw.type_info().name() {
                "INTEGER" => json!(row.try_get::<i64, _>(index)?),
                "REAL" => json!(row.try_get::<f64, _>(index)?),
                "TEXT" => json!(row.try_get::<String, _>(index)?),
                name => anyhow::bail!("unexpected recovery evidence type: {name}"),
            }
        };
        object.insert(column.name().into(), value);
    }
    Ok(Value::Object(object))
}

/// All queries share one snapshot. The export is bounded and explicitly
/// identifies truncation; it contains ledger evidence, never config secrets.
async fn export(bot: &TradingBot, order: Option<&str>) -> anyhow::Result<Value> {
    let mut tx = bot.db.begin().await?;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    let mut evidence = serde_json::Map::new();
    let tables = if order.is_some() {
        vec![
            ("live_orders", "order_id=?"), ("live_matches", "order_id=?"),
            ("maker_orders", "order_id=?"), ("maker_matches", "order_id=?"),
            ("maker_match_allocations", "order_id=?"),
            ("trade_fills", "order_id=? OR (phase='redemption' AND json_valid(details) AND json_extract(details,'$.transaction_hash')=?)"),
            ("order_outbox", "order_id=?"),
            ("redemption_receipts", "transaction_hash=?"),
            ("realized_pnl_events", "trade_id IN (SELECT trade_id FROM live_orders WHERE order_id=?) OR trade_id IN (SELECT trade_id FROM maker_match_allocations WHERE order_id=?) OR trade_id IN (SELECT j.value FROM redemption_receipts r,json_each(r.trade_ids_json) j WHERE r.transaction_hash=?)"),
            ("trades", "id IN (SELECT trade_id FROM live_orders WHERE order_id=?) OR id IN (SELECT trade_id FROM maker_match_allocations WHERE order_id=?) OR market_id IN (SELECT market_id FROM maker_orders WHERE order_id=?) OR id IN (SELECT j.value FROM redemption_receipts r,json_each(r.trade_ids_json) j WHERE r.transaction_hash=?)"),
        ]
    } else {
        vec![
            ("meta", "key LIKE 'drawdown_%' OR key LIKE 'live_taker_terminal_history:%' OR key LIKE 'live_taker_audit_reason:%'"),
            ("realized_pnl_events", "1=1"),
            ("redemption_receipts", "1=1"),
            ("live_orders", "state IN ('pending','audit_uncertain')"),
            ("maker_orders", "state NOT IN ('TERMINAL','REJECTED')"),
            ("maker_market_controls", "1=1"),
            ("trades", "mode='live' AND status IN ('OPEN','CLOSING')"),
            ("paper_maker_orders", "status='OPEN'"),
        ]
    };
    for (table, condition) in tables {
        let projection = if table == "redemption_receipts" && order.is_none() {
            // Global exports remain bounded even when raw receipt proofs are large.
            "transaction_hash,account,condition_id,block_number,block_hash,timestamp,trade_ids_json,result_json"
        } else {
            "*"
        };
        let sql = format!("SELECT {projection} FROM {table} WHERE {condition} LIMIT 1001");
        // Identifiers/conditions are the fixed pairs above; the request's order
        // ID is always bound separately and never interpolated into the SQL.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
        if let Some(id) = order {
            query = query.bind(id);
            if table == "realized_pnl_events" {
                query = query.bind(id).bind(id);
            } else if table == "trades" {
                query = query.bind(id).bind(id).bind(id);
            } else if table == "trade_fills" {
                query = query.bind(id);
            }
        }
        let mut rows = query.fetch_all(&mut *tx).await?;
        let truncated = rows.len() > 1000;
        rows.truncate(1000);
        evidence.insert(
            table.into(),
            json!({"truncated":truncated,
            "rows":rows.into_iter().map(row_json).collect::<anyhow::Result<Vec<_>>>()?}),
        );
    }
    if let Some(order_id) = order {
        let rows = sqlx::query("SELECT * FROM meta WHERE key IN (?,?)")
            .bind(format!("live_taker_terminal_history:{order_id}"))
            .bind(format!("live_taker_audit_reason:{order_id}"))
            .fetch_all(&mut *tx)
            .await?;
        evidence.insert(
            "meta".into(),
            json!({"truncated":false,
            "rows":rows.into_iter().map(row_json).collect::<anyhow::Result<Vec<_>>>()?}),
        );
    }
    tx.commit().await?;
    let (stop, reason) = bot.kill_switch_status();
    Ok(
        json!({"schema_version":version,"captured_at":chrono::Utc::now().to_rfc3339(),
        "order_id":order,"kill_switch":stop,"kill_switch_reason":reason,
        "evidence":evidence,"instructions":"ops/recovery-runbook.md",
        "realization_basis":"UTC realization event day: live taker settlement recognition time, maker fill time, external redemption finalized block time; undated legacy partial amounts remain unallocated.",
        "scope":"Persisted local evidence only; this does not verify current venue or wallet state."}),
    )
}

pub async fn status(State(bot): State<Arc<TradingBot>>) -> Result<Json<Value>, ApiError> {
    export(&bot, None).await.map(Json).map_err(|error| {
        tracing::error!("Recovery export failed: {error}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "recovery evidence unavailable".into(),
        )
    })
}

/// The caller supplies identifiers, never quantities, prices or a payout.
/// Receipt identity itself is the durable idempotency key inside the actor.
pub async fn reconcile_redemption(
    State(bot): State<Arc<TradingBot>>,
    Json(request): Json<crate::clob::redemption::RedemptionRequest>,
) -> Result<Json<Value>, ApiError> {
    request
        .validate()
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    bot.reconcile_redemption_serialized(request)
        .await
        .map(Json)
        .map_err(|error| {
            tracing::warn!("Redemption reconciliation refused: {error}");
            (
                StatusCode::CONFLICT,
                format!("redemption reconciliation requires complete immutable evidence: {error}"),
            )
        })
}

pub async fn order_evidence(
    State(bot): State<Arc<TradingBot>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    export(&bot, Some(&id)).await.map(Json).map_err(|error| {
        tracing::error!("Order recovery export failed: {error}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "order evidence unavailable".into(),
        )
    })
}

pub async fn recheck(
    State(bot): State<Arc<TradingBot>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let is_taker: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM live_orders WHERE order_id=?)")
            .bind(&id)
            .fetch_one(&bot.db)
            .await
            .map_err(|error| {
                tracing::warn!("Terminal order lookup refused: {error}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "order identity unavailable".into(),
                )
            })?;
    let checked = if is_taker {
        bot.recheck_terminal_taker_order_serialized(id).await
    } else {
        bot.recheck_terminal_maker_order_serialized(id).await
    };
    checked.map_err(|error| {
        tracing::warn!("Terminal order recheck refused: {error}");
        (StatusCode::CONFLICT, error.to_string())
    })?;
    Ok(Json(
        json!({"verified":true,"kill_switch":bot.kill_switch_active(),
        "degraded_no_trade":bot.degraded_no_trade_active(),
        "message":"Original evidence revalidated. Risk stops retained; full resync and operator review still required."}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[tokio::test]
    async fn taker_audit_evidence_is_scoped_by_exact_bound_identity() {
        let pool = db::open(":memory:").await.unwrap();
        let cfg = serde_json::from_str(include_str!("../../config.json")).unwrap();
        let bot = TradingBot::new(cfg, pool).await.unwrap();
        for (key, value) in [
            (
                "live_taker_terminal_history:order-1",
                "immutable receipt baseline",
            ),
            ("live_taker_audit_reason:order-1", "provider unavailable"),
            ("live_taker_audit_reason:order-2", "other order"),
            (
                "private_wallet_note",
                "do not expose this unrelated metadata",
            ),
        ] {
            sqlx::query("INSERT INTO meta(key,value) VALUES(?,?)")
                .bind(key)
                .bind(value)
                .execute(&bot.db)
                .await
                .unwrap();
        }
        let payload = export(&bot, Some("order-1")).await.unwrap();
        let rows = payload["evidence"]["meta"]["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows
            .iter()
            .all(|row| row["key"].as_str().unwrap().ends_with(":order-1")));
        let injected = export(&bot, Some("order-1' OR 1=1 --")).await.unwrap();
        assert!(injected["evidence"]["meta"]["rows"]
            .as_array()
            .unwrap()
            .is_empty());
        let global = export(&bot, None).await.unwrap();
        assert!(!global.to_string().contains("do not expose"));
        bot.db.close().await;
    }

    #[tokio::test]
    async fn evidence_export_identifies_blockers_without_changing_or_leaking_config() {
        let pool = db::open(":memory:").await.unwrap();
        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.api_key = "private-api-key-do-not-export".into();
        let bot = TradingBot::new(cfg, pool).await.unwrap();
        db::maker::persist_manual_stand_down(&bot.db, "stopped-market")
            .await
            .unwrap();
        let trade = db::Trade {
            timestamp: chrono::Utc::now().to_rfc3339(),
            status: "OPEN".into(),
            mode: "simulation".into(),
            shares: 2.0,
            entry_price: 0.5,
            amount: 1.0,
            pnl: Some(-0.25),
            partial_closed: Some(1),
            accounting_version: 1,
            ..Default::default()
        };
        let id = db::insert_trade(&bot.db, &trade).await.unwrap();
        sqlx::query("UPDATE trades SET entry_shares=3.0, entry_cost=1.5 WHERE id=?")
            .bind(id)
            .execute(&bot.db)
            .await
            .unwrap();
        let mut tx = bot.db.begin().await.unwrap();
        db::risk::record_realized_delta(
            &mut tx,
            id,
            "recovery-partial-sale",
            &trade.timestamp,
            -0.25,
            1.0,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let before = db::critical_table_counts(&bot.db).await.unwrap();
        let report = export(&bot, None).await.unwrap();
        assert_eq!(
            report["evidence"]["maker_market_controls"]["rows"][0]["market_id"],
            "stopped-market"
        );
        assert!(!report.to_string().contains("private-api-key-do-not-export"));
        assert_eq!(
            report["evidence"]["realized_pnl_events"]["rows"][0]["net_pnl"],
            -0.25
        );
        assert_eq!(
            report["evidence"]["realized_pnl_events"]["rows"][0]["realized_shares"],
            1.0
        );
        assert_eq!(
            report["evidence"]["realized_pnl_events"]["truncated"],
            false
        );
        assert_eq!(before, db::critical_table_counts(&bot.db).await.unwrap());
        // Bound the export visibly instead of implying completeness.
        for n in 0..1001 {
            db::maker::persist_manual_stand_down(&bot.db, &format!("extra-market-{n}"))
                .await
                .unwrap();
        }
        let bounded = export(&bot, None).await.unwrap();
        assert!(bounded["evidence"]["maker_market_controls"]["truncated"]
            .as_bool()
            .unwrap());
        assert_eq!(
            bounded["evidence"]["maker_market_controls"]["rows"]
                .as_array()
                .unwrap()
                .len(),
            1000
        );

        assert!(!report["evidence"]["maker_market_controls"]["truncated"]
            .as_bool()
            .unwrap());
        let order = export(&bot, Some("unknown-order")).await.unwrap();
        assert_eq!(order["evidence"]["maker_orders"]["rows"], json!([]));
        assert_eq!(order["evidence"]["realized_pnl_events"]["rows"], json!([]));
    }
}
