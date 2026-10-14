//! Durable maker order intents and exactly-once settled match accounting.
//!
//! An order intent is committed before its HTTP submission. Venue match IDs,
//! rather than polling timestamps, identify fills. A match, its lot changes,
//! and its fill snapshots are committed together in one immediate transaction.

use anyhow::{anyhow, ensure, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{sqlite::SqliteRow, Row, SqlitePool};

use super::Trade;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerOrder {
    pub order_id: String,
    pub market_id: String,
    pub token_id: String,
    pub side: String,
    pub price: f64,
    pub size: f64,
    pub confirmed_size: f64,
    pub state: String,
    pub template_json: String,
    pub settlement_json: String,
    pub created_at: String,
    pub updated_at: String,
}

impl_sqlite_from_row!(MakerOrder {
    order_id,
    market_id,
    token_id,
    side,
    price,
    size,
    confirmed_size,
    state,
    template_json,
    settlement_json,
    created_at,
    updated_at
});

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerMatch {
    pub match_id: String,
    pub order_id: String,
    pub token_id: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
    /// Actual fee charged to this maker leg, in collateral units. This is
    /// distinct from fee_rate_bps, which describes the market's taker fee.
    pub fee_usd: f64,
    pub timestamp: String,
}

impl_sqlite_from_row!(MakerMatch {
    match_id,
    order_id,
    token_id,
    side,
    size,
    price,
    fee_usd,
    timestamp
});

#[derive(Debug, Clone, Copy)]
pub struct MakerBuyBudget {
    pub available_balance: f64,
    pub min_trade_usd: f64,
    pub max_positions: u32,
    pub max_inventory_notional: f64,
    pub max_exposure_pct: f64,
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS maker_orders (
            order_id TEXT PRIMARY KEY, market_id TEXT NOT NULL,
            token_id TEXT NOT NULL, side TEXT NOT NULL CHECK(side IN ('BUY','SELL')),
            price REAL NOT NULL, size REAL NOT NULL, confirmed_size REAL NOT NULL DEFAULT 0,
            state TEXT NOT NULL, template_json TEXT NOT NULL, settlement_json TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL, updated_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    super::add_column_if_missing(
        pool,
        "maker_orders",
        "settlement_json",
        "ALTER TABLE maker_orders ADD COLUMN settlement_json TEXT NOT NULL DEFAULT ''",
    )
    .await?;
    super::add_column_if_missing(
        pool,
        "maker_orders",
        "terminal_history_json",
        "ALTER TABLE maker_orders ADD COLUMN terminal_history_json TEXT NOT NULL DEFAULT ''",
    )
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS maker_market_controls (
        market_id TEXT PRIMARY KEY,created_at TEXT NOT NULL)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS maker_matches (
            match_id TEXT NOT NULL, order_id TEXT NOT NULL,
            token_id TEXT NOT NULL, side TEXT NOT NULL, size REAL NOT NULL,
            price REAL NOT NULL, fee_usd REAL NOT NULL, timestamp TEXT NOT NULL,
            PRIMARY KEY(match_id, order_id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS maker_match_allocations (
            match_id TEXT NOT NULL, order_id TEXT NOT NULL, trade_id INTEGER NOT NULL,
            phase TEXT NOT NULL, size REAL NOT NULL, notional REAL NOT NULL,
            fee_usd REAL NOT NULL,
            PRIMARY KEY(match_id, order_id, trade_id, phase)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS maker_order_state ON maker_orders(state)")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn persist_manual_stand_down(pool: &SqlitePool, market_id: &str) -> Result<()> {
    ensure!(
        !market_id.trim().is_empty(),
        "manual stand-down requires a market identity"
    );
    sqlx::query(
        "INSERT INTO maker_market_controls(market_id,created_at) VALUES(?,?)
        ON CONFLICT(market_id) DO NOTHING",
    )
    .bind(market_id)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn stood_down_markets(pool: &SqlitePool) -> Result<Vec<String>> {
    Ok(
        sqlx::query_scalar("SELECT market_id FROM maker_market_controls ORDER BY market_id")
            .fetch_all(pool)
            .await?,
    )
}

pub async fn market_stood_down(pool: &SqlitePool, market_id: &str) -> Result<bool> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM maker_market_controls WHERE market_id=?")
            .bind(market_id)
            .fetch_one(pool)
            .await?;
    Ok(count > 0)
}

pub async fn terminal_orders_batch(
    pool: &SqlitePool,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<MakerOrder>> {
    ensure!(
        (1..=32).contains(&limit),
        "terminal maker audit batch limit is invalid"
    );
    Ok(sqlx::query_as(
        "SELECT * FROM maker_orders WHERE state IN ('TERMINAL','REJECTED')
        AND order_id>? ORDER BY order_id LIMIT ?",
    )
    .bind(after.unwrap_or(""))
    .bind(limit)
    .fetch_all(pool)
    .await?)
}

pub async fn terminal_history(pool: &SqlitePool, order_id: &str) -> Result<String> {
    Ok(
        sqlx::query_scalar("SELECT terminal_history_json FROM maker_orders WHERE order_id=?")
            .bind(order_id)
            .fetch_one(pool)
            .await?,
    )
}

pub async fn finalize_order(pool: &SqlitePool, order_id: &str, history: &str) -> Result<()> {
    let _: serde_json::Value = serde_json::from_str(history)?;
    let changed = sqlx::query(
        "UPDATE maker_orders SET state='TERMINAL',terminal_history_json=?,updated_at=?
        WHERE order_id=? AND state NOT IN ('TERMINAL','REJECTED','AUDIT_UNCERTAIN')",
    )
    .bind(history)
    .bind(Utc::now().to_rfc3339())
    .bind(order_id)
    .execute(pool)
    .await?
    .rows_affected();
    ensure!(
        changed == 1,
        "cannot finalize absent or already completed maker intent"
    );
    Ok(())
}

pub async fn record_verified_terminal_baseline(
    pool: &SqlitePool,
    order_id: &str,
    history: &str,
) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE maker_orders SET terminal_history_json=?
        WHERE order_id=? AND state IN ('TERMINAL','REJECTED') AND terminal_history_json=''",
    )
    .bind(history)
    .bind(order_id)
    .execute(pool)
    .await?
    .rows_affected();
    ensure!(
        changed == 1,
        "terminal maker baseline is absent or already recorded"
    );
    Ok(())
}

pub async fn mark_audit_uncertain(pool: &SqlitePool, order_id: &str) -> Result<()> {
    let changed = sqlx::query(
        "UPDATE maker_orders SET state='AUDIT_UNCERTAIN',updated_at=?
        WHERE order_id=? AND state IN ('TERMINAL','REJECTED','AUDIT_UNCERTAIN')",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(order_id)
    .execute(pool)
    .await?
    .rows_affected();
    ensure!(
        changed == 1,
        "maker terminal audit intent is absent or no longer terminal"
    );
    Ok(())
}

pub async fn recorded_matches(pool: &SqlitePool, order_id: &str) -> Result<Vec<MakerMatch>> {
    Ok(
        sqlx::query_as("SELECT * FROM maker_matches WHERE order_id=? ORDER BY match_id")
            .bind(order_id)
            .fetch_all(pool)
            .await?,
    )
}

/// The caller has revalidated original terminal history and every finalized
/// receipt in the execution actor. Compare the baseline again in the write
/// transaction and keep an audit record; never alter lots or release a stop.
pub(crate) async fn restore_verified_terminal_order(
    pool: &SqlitePool,
    order_id: &str,
    baseline: &str,
) -> Result<()> {
    ensure!(
        !baseline.is_empty(),
        "a verified terminal baseline is required"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let changed = sqlx::query(
        "UPDATE maker_orders SET state='TERMINAL',updated_at=?
        WHERE order_id=? AND state='AUDIT_UNCERTAIN' AND terminal_history_json=?",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(order_id)
    .bind(baseline)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    ensure!(
        changed == 1,
        "uncertain intent or verified baseline changed during recheck"
    );
    sqlx::query(
        "INSERT INTO order_outbox(timestamp,action,market_id,order_id,stage,status,details)
        SELECT ?,'operator_recovery',market_id,order_id,'maker_terminal_recheck','verified',
        'Original terminal history and finalized receipts revalidated; risk stops retained'
        FROM maker_orders WHERE order_id=?",
    )
    .bind(Utc::now().to_rfc3339())
    .bind(order_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

fn validate_order(order: &MakerOrder) -> Result<()> {
    ensure!(
        !order.order_id.trim().is_empty()
            && !order.market_id.trim().is_empty()
            && !order.token_id.trim().is_empty()
            && matches!(order.side.as_str(), "BUY" | "SELL")
            && order.price.is_finite()
            && order.price > 0.0
            && order.price < 1.0
            && order.size.is_finite()
            && order.size > 0.0
            && order.confirmed_size == 0.0
            && order.state == "SUBMITTING",
        "invalid maker intent"
    );
    let template: Trade = serde_json::from_str(&order.template_json)?;
    let identity: crate::clob::settlement::SettlementOrderIdentity =
        serde_json::from_str(&order.settlement_json)?;
    ensure!(
        identity.order_id == order.order_id
            && identity.asset_id == order.token_id
            && identity.side.as_u8() == if order.side == "BUY" { 0 } else { 1 },
        "maker intent settlement identity differs from the signed order"
    );
    ensure!(
        template.is_live()
            && template.market_id == order.market_id
            && template.token_id.as_deref() == Some(order.token_id.as_str())
            && matches!(template.direction.as_str(), "BUY_YES" | "BUY_NO"),
        "maker intent template does not identify its live token"
    );
    Ok(())
}

async fn insert_order(connection: &mut sqlx::SqliteConnection, order: &MakerOrder) -> Result<()> {
    sqlx::query(
        "INSERT INTO maker_orders (order_id,market_id,token_id,side,price,size,
         confirmed_size,state,template_json,settlement_json,created_at,updated_at)
         VALUES (?,?,?,?,?,?,0,'SUBMITTING',?,?,?,?)",
    )
    .bind(&order.order_id)
    .bind(&order.market_id)
    .bind(&order.token_id)
    .bind(&order.side)
    .bind(order.price)
    .bind(order.size)
    .bind(&order.template_json)
    .bind(&order.settlement_json)
    .bind(&order.created_at)
    .bind(&order.updated_at)
    .execute(connection)
    .await?;
    Ok(())
}

pub async fn register_order(pool: &SqlitePool, order: &MakerOrder) -> Result<()> {
    validate_order(order)?;
    insert_order(&mut *pool.acquire().await?, order).await
}

/// Reserve the complete signed entry budget before its POST. An untouched
/// quote occupies a position slot; its partial fills replace that reservation
/// with the same position slot rather than counting it twice.
pub async fn admit_buy_order(
    pool: &SqlitePool,
    order: &MakerOrder,
    budget: MakerBuyBudget,
) -> Result<()> {
    validate_order(order)?;
    ensure!(
        order.side == "BUY"
            && budget.available_balance.is_finite()
            && budget.available_balance >= 0.0
            && budget.min_trade_usd.is_finite()
            && budget.min_trade_usd >= 0.0
            && budget.max_inventory_notional.is_finite()
            && budget.max_inventory_notional >= 0.0
            && budget.max_exposure_pct.is_finite()
            && (0.0..=1.0).contains(&budget.max_exposure_pct),
        "invalid live maker entry budget"
    );
    let notional = order.price * order.size;
    ensure!(
        notional.is_finite() && notional + 1e-9 >= budget.min_trade_usd,
        "live maker quote is below minimum trade budget"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let result:Result<()> = async {
        let unresolved_takers:i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_orders WHERE state IN ('pending','audit_uncertain')")
            .fetch_one(&mut *tx).await?;
        ensure!(unresolved_takers == 0,"maker entry waits for unresolved taker settlement");
        let reserved:f64 = sqlx::query_scalar("SELECT COALESCE(SUM(MAX(0,size-confirmed_size)*price),0.0)
            FROM maker_orders WHERE side='BUY' AND state NOT IN ('TERMINAL','REJECTED')")
            .fetch_one(&mut *tx).await?;
        ensure!(budget.available_balance + 1e-9 >= reserved + notional,
            "live maker quote exceeds unreserved collateral");
        let positions:i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT COALESCE((SELECT 'maker:'||a.order_id FROM maker_match_allocations a
              WHERE a.trade_id=t.id AND a.phase='entry' LIMIT 1),'trade:'||t.id))
             FROM trades t WHERE t.mode='live' AND t.status IN ('OPEN','CLOSING') AND t.strategy!='scalp'")
            .fetch_one(&mut *tx).await?;
        let reserved_positions:i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM maker_orders o WHERE o.side='BUY' AND o.state NOT IN ('TERMINAL','REJECTED')
             AND o.size-o.confirmed_size>0.000001 AND NOT EXISTS (
              SELECT 1 FROM maker_match_allocations a JOIN trades t ON t.id=a.trade_id
              WHERE a.order_id=o.order_id AND a.phase='entry' AND t.status IN ('OPEN','CLOSING'))")
            .fetch_one(&mut *tx).await?;
        ensure!(budget.max_positions == 0 || positions + reserved_positions < i64::from(budget.max_positions),
            "live maker quote exceeds reserved position slots");
        let market_exposure:f64 = sqlx::query_scalar("SELECT COALESCE(SUM(amount),0.0) FROM trades
            WHERE mode='live' AND status IN ('OPEN','CLOSING') AND market_id=?")
            .bind(&order.market_id).fetch_one(&mut *tx).await?;
        let market_reserved:f64 = sqlx::query_scalar("SELECT COALESCE(SUM(MAX(0,size-confirmed_size)*price),0.0)
            FROM maker_orders WHERE side='BUY' AND state NOT IN ('TERMINAL','REJECTED') AND market_id=?")
            .bind(&order.market_id).fetch_one(&mut *tx).await?;
        ensure!(budget.max_inventory_notional <= 0.0 || market_exposure + market_reserved + notional <= budget.max_inventory_notional + 1e-9,
            "live maker quote exceeds reserved market inventory cap");
        let exposure:f64 = sqlx::query_scalar("SELECT COALESCE(SUM(amount),0.0) FROM trades WHERE mode='live' AND status IN ('OPEN','CLOSING')")
            .fetch_one(&mut *tx).await?;
        ensure!(budget.max_exposure_pct <= 0.0 || exposure + reserved + notional <= budget.available_balance * budget.max_exposure_pct + 1e-9,
            "live maker quote exceeds reserved exposure cap");
        insert_order(&mut tx,order).await
    }.await;
    match result {
        Ok(()) => {
            tx.commit().await?;
            Ok(())
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

pub async fn pending_orders(pool: &SqlitePool) -> Result<Vec<MakerOrder>> {
    Ok(sqlx::query_as::<_, MakerOrder>(
        "SELECT * FROM maker_orders WHERE state NOT IN ('TERMINAL','REJECTED') ORDER BY created_at,order_id",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn update_state(pool: &SqlitePool, order_id: &str, state: &str) -> Result<()> {
    ensure!(
        matches!(
            state,
            "LIVE" | "SUBMITTING" | "CANCEL_REQUESTED" | "UNKNOWN" | "TERMINAL" | "REJECTED"
        ),
        "invalid maker order state"
    );
    let changed = sqlx::query(
        "UPDATE maker_orders SET state=?,updated_at=? WHERE order_id=?
        AND (state NOT IN ('TERMINAL','REJECTED','AUDIT_UNCERTAIN') OR state=?)",
    )
    .bind(state)
    .bind(Utc::now().to_rfc3339())
    .bind(order_id)
    .bind(state)
    .execute(pool)
    .await?
    .rows_affected();
    ensure!(
        changed == 1,
        "maker order intent is absent or its terminal state conflicts: {order_id}"
    );
    Ok(())
}

pub async fn reserved_buy_collateral(pool: &SqlitePool) -> Result<f64> {
    let value: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(MAX(0,size-confirmed_size)*price),0.0)
         FROM maker_orders WHERE side='BUY' AND state NOT IN ('TERMINAL','REJECTED')",
    )
    .fetch_one(pool)
    .await?;
    ensure!(
        value.is_finite() && value >= 0.0,
        "invalid maker reservation"
    );
    Ok(value)
}

/// Apply only a confirmed venue match. The caller must first establish maker
/// ownership and settlement status from the authenticated venue response.
/// Duplicate evidence is accepted only when every immutable economic field
/// matches. Changed evidence is an accounting fault, never a second fill.
pub async fn apply_confirmed_match(pool: &SqlitePool, fill: &MakerMatch) -> Result<Vec<Trade>> {
    ensure!(
        !fill.match_id.trim().is_empty()
            && !fill.order_id.trim().is_empty()
            && !fill.token_id.trim().is_empty()
            && matches!(fill.side.as_str(), "BUY" | "SELL")
            && fill.size.is_finite()
            && fill.size > 0.0
            && fill.price.is_finite()
            && fill.price > 0.0
            && fill.price < 1.0
            && fill.fee_usd.is_finite()
            && fill.fee_usd >= 0.0
            && chrono::DateTime::parse_from_rfc3339(&fill.timestamp).is_ok(),
        "invalid confirmed maker match"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let result: Result<Vec<Trade>> = async {
    let order: MakerOrder = sqlx::query_as("SELECT * FROM maker_orders WHERE order_id=?")
        .bind(&fill.order_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| anyhow!("confirmed maker match lacks a durable order intent"))?;
    ensure!(
        order.token_id == fill.token_id && order.side == fill.side,
        "confirmed maker match token or side differs from its order"
    );
    let prior = sqlx::query(
        "SELECT token_id,side,size,price,fee_usd,timestamp FROM maker_matches WHERE match_id=? AND order_id=?",
    )
    .bind(&fill.match_id)
    .bind(&fill.order_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(prior) = prior {
        ensure!(
            prior.get::<String, _>("token_id") == fill.token_id
                && prior.get::<String, _>("side") == fill.side
                && (prior.get::<f64, _>("size") - fill.size).abs() < 1e-9
                && (prior.get::<f64, _>("price") - fill.price).abs() < 1e-9
                && (prior.get::<f64, _>("fee_usd") - fill.fee_usd).abs() < 1e-9
                && chrono::DateTime::parse_from_rfc3339(&prior.get::<String, _>("timestamp"))?
                    == chrono::DateTime::parse_from_rfc3339(&fill.timestamp)?,
            "venue changed already accounted maker match {}",
            fill.match_id
        );
        return Ok(Vec::new());
    }
    ensure!(
        order.confirmed_size + fill.size <= order.size + 1e-6,
        "maker match exceeds its signed order size"
    );
    ensure!(
        (fill.side == "BUY" && fill.price <= order.price + 1e-9)
            || (fill.side == "SELL" && fill.price + 1e-9 >= order.price),
        "maker execution price violates its signed limit"
    );
    sqlx::query(
        "INSERT INTO maker_matches (match_id,order_id,token_id,side,size,price,fee_usd,timestamp)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(&fill.match_id)
    .bind(&fill.order_id)
    .bind(&fill.token_id)
    .bind(&fill.side)
    .bind(fill.size)
    .bind(fill.price)
    .bind(fill.fee_usd)
    .bind(&fill.timestamp)
    .execute(&mut *tx)
    .await?;

    let mut changed = Vec::new();
    if fill.side == "BUY" {
        // Reuse this order's open lot. A late confirmed entry after that lot
        // was sold opens a new lot; it never rewrites an earlier sale's P&L.
        let existing: Option<Trade> = sqlx::query_as(
            "SELECT t.* FROM trades t JOIN maker_match_allocations a ON a.trade_id=t.id
             WHERE a.order_id=? AND a.phase='entry' AND t.status='OPEN' AND COALESCE(t.partial_closed,0)=0
             ORDER BY t.id DESC LIMIT 1",
        )
        .bind(&fill.order_id)
        .fetch_optional(&mut *tx)
        .await?;
        let notional = fill.size * fill.price;
        let mut trade = if let Some(mut trade) = existing {
            let residual_cost = trade.shares * trade.entry_price;
            trade.shares += fill.size;
            trade.amount = residual_cost + notional;
            trade.entry_price = trade.amount / trade.shares;
            trade.fee_paid += fill.fee_usd;
            trade.accounting_version = 1;
            sqlx::query(
                "UPDATE trades SET shares=?,amount=?,entry_price=?,fee_paid=?,
                 accounting_version=1,entry_shares=entry_shares+?,entry_cost=entry_cost+? WHERE id=? AND status='OPEN'",
            )
            .bind(trade.shares)
            .bind(trade.amount)
            .bind(trade.entry_price)
            .bind(trade.fee_paid)
            .bind(fill.size)
            .bind(notional)
            .bind(trade.id)
            .execute(&mut *tx)
            .await?;
            trade
        } else {
            let mut trade: Trade = serde_json::from_str(&order.template_json)?;
            trade.id = 0;
            trade.timestamp = fill.timestamp.clone();
            trade.entry_price = fill.price;
            trade.amount = notional;
            trade.shares = fill.size;
            trade.status = "OPEN".into();
            trade.order_id = None;
            trade.fee_paid = fill.fee_usd;
            trade.accounting_version = 1;
            trade.partial_closed = Some(0);
            trade.pnl = None;
            let inserted = sqlx::query(
                "INSERT INTO trades (timestamp,market_id,question,direction,entry_price,amount,shares,
                 expected_value,reason,confidence,status,mode,source,end_date,partial_closed,
                 raw_confidence,token_id,fok_failed,fee_paid,signals_json,calibration_version,run_id,
                 strategy,entry_shares,entry_cost,accounting_version)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,1)",
            )
            .bind(&trade.timestamp)
            .bind(&trade.market_id)
            .bind(&trade.question)
            .bind(&trade.direction)
            .bind(trade.entry_price)
            .bind(trade.amount)
            .bind(trade.shares)
            .bind(trade.expected_value)
            .bind(&trade.reason)
            .bind(trade.confidence)
            .bind(&trade.status)
            .bind(&trade.mode)
            .bind(&trade.source)
            .bind(&trade.end_date)
            .bind(trade.partial_closed)
            .bind(trade.raw_confidence)
            .bind(&trade.token_id)
            .bind(trade.fok_failed)
            .bind(trade.fee_paid)
            .bind(&trade.signals_json)
            .bind(&trade.calibration_version)
            .bind(&trade.run_id)
            .bind("maker")
            .bind(fill.size)
            .bind(notional)
            .execute(&mut *tx)
            .await?;
            trade.id = inserted.last_insert_rowid();
            trade.strategy = "maker".into();
            trade
        };
        allocate(
            &mut tx,
            fill,
            &mut trade,
            "entry",
            fill.size,
            notional,
            fill.fee_usd,
            order.price,
        )
        .await?;
        changed.push(trade);
    } else {
        let lots: Vec<Trade> = sqlx::query_as(
            "SELECT * FROM trades WHERE status='OPEN' AND mode='live' AND market_id=?
             AND token_id=? AND shares>0 ORDER BY id",
        )
        .bind(&order.market_id)
        .bind(&fill.token_id)
        .fetch_all(&mut *tx)
        .await?;
        ensure!(
            lots.iter().all(|trade| trade.accounting_version == 1),
            "maker sale cannot account legacy inventory without verified entry accounting"
        );
        let available: f64 = lots.iter().map(|t| t.shares).sum();
        ensure!(
            available + 1e-6 >= fill.size,
            "confirmed maker sale exceeds recorded available inventory"
        );
        let mut remaining = fill.size;
        for mut trade in lots {
            if remaining <= 1e-9 {
                break;
            }
            let size = remaining.min(trade.shares);
            let proceeds = size * fill.price;
            let fee = fill.fee_usd * size / fill.size;
            let (entry_size,entry_cost):(f64,f64) = sqlx::query_as("SELECT entry_shares,entry_cost FROM trades WHERE id=?")
                .bind(trade.id).fetch_one(&mut *tx).await?;
            ensure!(entry_size > 0.0 && entry_cost.is_finite() && entry_cost >= 0.0, "maker sale lacks a trustworthy entry cost");
            let entry_cash_fee:f64 = sqlx::query_scalar(
                "SELECT COALESCE((SELECT SUM(fee_usd) FROM maker_match_allocations WHERE trade_id=? AND phase='entry'),0.0)
                    + COALESCE((SELECT SUM(m.fee) FROM live_matches m JOIN live_orders o ON o.order_id=m.order_id
                         WHERE o.trade_id=? AND o.phase='entry'),0.0)")
                .bind(trade.id).bind(trade.id).fetch_one(&mut *tx).await?;
            let profit = proceeds - size * (entry_cost + entry_cash_fee) / entry_size - fee;
            trade.shares = (trade.shares - size).max(0.0);
            trade.amount = trade.shares * entry_cost / entry_size;
            trade.pnl = Some(trade.pnl.unwrap_or(0.0) + profit);
            trade.exit_price = Some(fill.price);
            trade.exit_timestamp = Some(fill.timestamp.clone());
            trade.exit_type = Some("maker".into());
            trade.partial_closed = Some(1);
            trade.accounting_version = 1;
            if trade.shares <= 1e-9 {
                trade.status = "CLOSED".into();
                trade.was_correct = Some(i64::from(trade.pnl.unwrap_or(0.0) > 0.0));
            }
            sqlx::query(
                "UPDATE trades SET shares=?,amount=?,pnl=?,exit_price=?,exit_timestamp=?,
                 exit_type='maker',partial_closed=1,status=?,was_correct=?,accounting_version=1 WHERE id=? AND status='OPEN'",
            )
            .bind(trade.shares)
            .bind(trade.amount)
            .bind(trade.pnl)
            .bind(trade.exit_price)
            .bind(&trade.exit_timestamp)
            .bind(&trade.status)
            .bind(trade.was_correct)
            .bind(trade.id)
            .execute(&mut *tx)
            .await?;
            allocate(
                &mut tx,
                fill,
                &mut trade,
                "exit",
                size,
                proceeds,
                fee,
                order.price,
            )
            .await?;
            super::risk::record_realized_delta(
                &mut tx,
                trade.id,
                &format!("maker/{}/{}", fill.order_id, fill.match_id),
                &fill.timestamp,
                profit,
                size,
            )
            .await?;
            changed.push(trade);
            remaining -= size;
        }
    }
    sqlx::query(
        "UPDATE maker_orders SET confirmed_size=confirmed_size+?,updated_at=? WHERE order_id=?",
    )
    .bind(fill.size)
    .bind(Utc::now().to_rfc3339())
    .bind(&fill.order_id)
    .execute(&mut *tx)
    .await?;
    Ok(changed)
    }.await;
    match result {
        Ok(changed) => {
            tx.commit().await?;
            Ok(changed)
        }
        Err(error) => {
            // Await rollback before returning. Drop's deferred rollback can
            // race a caller's immediate retry on another pooled connection.
            tx.rollback().await?;
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn allocate(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    fill: &MakerMatch,
    trade: &mut Trade,
    phase: &str,
    size: f64,
    notional: f64,
    fee: f64,
    intended_price: f64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO maker_match_allocations (match_id,order_id,trade_id,phase,size,notional,fee_usd)
         VALUES (?,?,?,?,?,?,?)",
    )
    .bind(&fill.match_id)
    .bind(&fill.order_id)
    .bind(trade.id)
    .bind(phase)
    .bind(size)
    .bind(notional)
    .bind(fee)
    .execute(&mut **tx)
    .await?;
    let (cumulative, cost): (f64, f64) = sqlx::query_as(
        "SELECT SUM(size),SUM(notional) FROM maker_match_allocations WHERE order_id=? AND trade_id=? AND phase=?",
    )
    .bind(&fill.order_id)
    .bind(trade.id)
    .bind(phase)
    .fetch_one(&mut **tx)
    .await?;
    let details = serde_json::to_string(fill)?;
    sqlx::query(
        "INSERT INTO trade_fills (timestamp,trade_id,market_id,order_id,phase,intended_price,
         avg_price,cumulative_size,delta_size,delta_price,source,details)
         VALUES (?,?,?,?,?,?,?,?,?,?,'maker_confirmed',?)",
    )
    .bind(&fill.timestamp)
    .bind(trade.id)
    .bind(&trade.market_id)
    .bind(&fill.order_id)
    .bind(phase)
    .bind(intended_price)
    .bind(cost / cumulative)
    .bind(cumulative)
    .bind(size)
    .bind(fill.price)
    .bind(details)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(id: &str, side: &str, price: f64, size: f64) -> MakerOrder {
        let template = Trade {
            market_id: "market".into(),
            token_id: Some("token".into()),
            direction: "BUY_YES".into(),
            mode: "live".into(),
            question: "maker accounting fixture".into(),
            reason: "confirmed maker match".into(),
            source: "bot".into(),
            status: "OPEN".into(),
            strategy: "maker".into(),
            ..Trade::default()
        };
        MakerOrder {
            order_id: id.into(),
            market_id: "market".into(),
            token_id: "token".into(),
            side: side.into(),
            price,
            size,
            confirmed_size: 0.0,
            state: "SUBMITTING".into(),
            template_json: serde_json::to_string(&template).unwrap(),
            settlement_json: serde_json::to_string(
                &crate::clob::settlement::SettlementOrderIdentity {
                    order_id: id.into(),
                    asset_id: "token".into(),
                    maker_address: "0x1111111111111111111111111111111111111111".into(),
                    exchange_address: "0xE111180000d2663C0091e4f400237545B87B996B".into(),
                    side: if side == "BUY" {
                        crate::clob::Side::Buy
                    } else {
                        crate::clob::Side::Sell
                    },
                    protocol: crate::clob::OrderProtocol::V2,
                },
            )
            .unwrap(),
            created_at: "2026-10-10T12:00:00Z".into(),
            updated_at: "2026-10-10T12:00:00Z".into(),
        }
    }

    fn matched(id: &str, order_id: &str, side: &str, size: f64, price: f64) -> MakerMatch {
        MakerMatch {
            match_id: id.into(),
            order_id: order_id.into(),
            token_id: "token".into(),
            side: side.into(),
            size,
            price,
            fee_usd: 0.0,
            timestamp: "2026-10-10T12:01:00Z".into(),
        }
    }

    async fn test_pool() -> Result<(SqlitePool, std::path::PathBuf)> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);
        // Wall-clock readings can be identical on parallel macOS threads.
        // Exclusively create each path so another fixture's cleanup can never
        // unlink a live database or turn its restart into a fresh empty DB.
        let path = loop {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let candidate = std::env::temp_dir().join(format!(
                "macbot-maker-ledger-{}-{sequence}.sqlite",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => {
                    drop(file);
                    break candidate;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let pool = super::super::open(path.to_str().unwrap()).await?;
        Ok((pool, path))
    }

    async fn cleanup(pool: SqlitePool, path: std::path::PathBuf) {
        pool.close().await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[tokio::test]
    async fn settled_partial_matches_use_actual_vwap_and_survive_restart() -> Result<()> {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("buy", "BUY", 0.51, 10.0)).await?;
        assert_eq!(reserved_buy_collateral(&pool).await?, 5.1);
        let first = matched("match-1", "buy", "BUY", 3.0, 0.49);
        let trade = apply_confirmed_match(&pool, &first).await?.remove(0);
        assert!((trade.amount - 1.47).abs() < 1e-9);
        pool.close().await;
        let pool = super::super::open(path.to_str().unwrap()).await?;
        assert!(apply_confirmed_match(&pool, &first).await?.is_empty());
        let second = matched("match-2", "buy", "BUY", 7.0, 0.50);
        let trade = apply_confirmed_match(&pool, &second).await?.remove(0);
        assert_eq!(trade.shares, 10.0);
        assert!((trade.entry_price - 0.497).abs() < 1e-9);
        assert!(
            (super::super::trade_entry_cost(&pool, trade.id)
                .await?
                .unwrap()
                - 4.97)
                .abs()
                < 1e-9
        );
        assert_eq!(reserved_buy_collateral(&pool).await?, 0.0);
        let fills = super::super::load_trade_fills(&pool, trade.id, 10).await?;
        assert_eq!(fills.len(), 2);
        assert!((fills[0].avg_price - 0.497).abs() < 1e-9);
        let mut changed = first.clone();
        changed.price = 0.48;
        assert!(apply_confirmed_match(&pool, &changed).await.is_err());
        let mut changed_day = first.clone();
        changed_day.timestamp = "2026-10-11T12:01:00Z".into();
        assert!(apply_confirmed_match(&pool, &changed_day).await.is_err());
        let mut same_instant = first.clone();
        same_instant.timestamp = "2026-10-10T14:01:00+02:00".into();
        assert!(apply_confirmed_match(&pool, &same_instant)
            .await?
            .is_empty());
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_duplicate_match_changes_inventory_once() -> Result<()> {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("buy", "BUY", 0.5, 10.0)).await?;
        let fill = matched("same-match", "buy", "BUY", 5.0, 0.5);
        let (first, second) = tokio::join!(
            apply_confirmed_match(&pool, &fill),
            apply_confirmed_match(&pool, &fill)
        );
        assert_eq!(first?.len() + second?.len(), 1);
        let trades = super::super::load_all_trades(&pool).await?;
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].shares, 5.0);
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn maker_sales_partially_close_lots_and_account_each_match_once() -> Result<()> {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("buy", "BUY", 0.4, 10.0)).await?;
        let original = apply_confirmed_match(&pool, &matched("entry", "buy", "BUY", 10.0, 0.4))
            .await?
            .remove(0);
        register_order(&pool, &intent("sell", "SELL", 0.6, 10.0)).await?;
        let partial = matched("sell-1", "sell", "SELL", 3.0, 0.61);
        let trade = apply_confirmed_match(&pool, &partial).await?.remove(0);
        assert_eq!(trade.id, original.id);
        assert_eq!(trade.shares, 7.0);
        assert_eq!(trade.status, "OPEN");
        assert!((trade.pnl.unwrap() - 0.63).abs() < 1e-9);
        let complete = matched("sell-2", "sell", "SELL", 7.0, 0.6);
        let trade = apply_confirmed_match(&pool, &complete).await?.remove(0);
        assert_eq!(trade.shares, 0.0);
        assert_eq!(trade.status, "CLOSED");
        assert!((trade.pnl.unwrap() - 2.03).abs() < 1e-9);
        assert!(apply_confirmed_match(&pool, &complete).await?.is_empty());
        assert!(
            (super::super::trade_entry_cost(&pool, trade.id)
                .await?
                .unwrap()
                - 4.0)
                .abs()
                < 1e-9
        );
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn unsupported_or_inconsistent_match_rolls_back_receipt_and_lot_changes() -> Result<()> {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("sell", "SELL", 0.6, 5.0)).await?;
        let sale = matched("unowned-sale", "sell", "SELL", 5.0, 0.6);
        assert!(apply_confirmed_match(&pool, &sale).await.is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM maker_matches")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 0);
        assert_eq!(pending_orders(&pool).await?[0].confirmed_size, 0.0);
        register_order(&pool, &intent("buy", "BUY", 0.5, 5.0)).await?;
        assert!(
            apply_confirmed_match(&pool, &matched("overfill", "buy", "BUY", 6.0, 0.5))
                .await
                .is_err()
        );
        assert!(
            apply_confirmed_match(&pool, &matched("bad-limit", "buy", "BUY", 5.0, 0.6))
                .await
                .is_err()
        );
        assert!(super::super::load_all_trades(&pool).await?.is_empty());
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn maker_realized_profit_allocates_actual_entry_and_exit_cash_fees_once() -> Result<()> {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("buy-fees", "BUY", 0.5, 10.0)).await?;
        let mut entry = matched("entry-fees", "buy-fees", "BUY", 10.0, 0.5);
        entry.fee_usd = 0.1;
        let original = apply_confirmed_match(&pool, &entry).await?.remove(0);
        register_order(&pool, &intent("sell-fees", "SELL", 0.6, 10.0)).await?;
        let mut first = matched("first-sale-fees", "sell-fees", "SELL", 4.0, 0.6);
        first.fee_usd = 0.02;
        let partial = apply_confirmed_match(&pool, &first).await?.remove(0);
        assert!((partial.pnl.unwrap() - 0.34).abs() < 1e-9);
        let partial_day = super::super::risk::daily_realized_pnl(
            &pool,
            chrono::NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
        )
        .await?;
        assert!(partial_day.evidence_complete);
        assert!((partial_day.exact_today_pnl.unwrap() - 0.34).abs() < 1e-9);
        let mut last = matched("last-sale-fees", "sell-fees", "SELL", 6.0, 0.6);
        last.fee_usd = 0.03;
        let closed = apply_confirmed_match(&pool, &last).await?.remove(0);
        assert_eq!(closed.id, original.id);
        assert!((closed.pnl.unwrap() - 0.85).abs() < 1e-9);
        assert!((closed.fee_paid - 0.1).abs() < 1e-9);
        assert_eq!(closed.accounting_version, 1);
        assert!((closed.net_realized_pnl() - 0.85).abs() < 1e-9);
        let day = super::super::risk::daily_realized_pnl(
            &pool,
            chrono::NaiveDate::from_ymd_opt(2026, 10, 10).unwrap(),
        )
        .await?;
        assert!(day.evidence_complete);
        assert!((day.exact_today_pnl.unwrap() - 0.85).abs() < 1e-9);
        assert!(apply_confirmed_match(&pool, &last).await?.is_empty());
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn realized_event_failure_rolls_back_maker_receipt_and_sale() -> Result<()> {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("buy-risk", "BUY", 0.5, 10.0)).await?;
        let original = apply_confirmed_match(
            &pool,
            &matched("buy-risk-fill", "buy-risk", "BUY", 10.0, 0.5),
        )
        .await?
        .remove(0);
        register_order(&pool, &intent("sell-risk", "SELL", 0.6, 5.0)).await?;
        sqlx::query("CREATE TRIGGER reject_realization BEFORE INSERT ON realized_pnl_events BEGIN SELECT RAISE(ABORT, 'injected risk evidence failure'); END").execute(&pool).await?;
        let sale = matched("sell-risk-fill", "sell-risk", "SELL", 5.0, 0.6);
        assert!(apply_confirmed_match(&pool, &sale).await.is_err());
        let unchanged = super::super::load_trade_by_id(&pool, original.id)
            .await?
            .unwrap();
        assert_eq!(unchanged.shares, original.shares);
        assert_eq!(unchanged.pnl, original.pnl);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM maker_matches WHERE match_id='sell-risk-fill'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER reject_realization")
            .execute(&pool)
            .await?;
        apply_confirmed_match(&pool, &sale).await?;
        apply_confirmed_match(&pool, &sale).await?;
        let events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM realized_pnl_events WHERE trade_id=?")
                .bind(original.id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(events, 1);
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn late_buy_after_partial_sale_opens_a_new_lot_without_repricing_prior_profit(
    ) -> Result<()> {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("buy-late", "BUY", 0.5, 10.0)).await?;
        let original =
            apply_confirmed_match(&pool, &matched("entry-first", "buy-late", "BUY", 5.0, 0.5))
                .await?
                .remove(0);
        register_order(&pool, &intent("sell-part", "SELL", 0.6, 2.0)).await?;
        let partial =
            apply_confirmed_match(&pool, &matched("sale-part", "sell-part", "SELL", 2.0, 0.6))
                .await?
                .remove(0);
        assert_eq!(partial.id, original.id);
        assert!((partial.pnl.unwrap() - 0.2).abs() < 1e-9);
        let late = matched("entry-late", "buy-late", "BUY", 5.0, 0.4);
        let new_lot = apply_confirmed_match(&pool, &late).await?.remove(0);
        assert_ne!(new_lot.id, original.id);
        let prior = super::super::load_trade_by_id(&pool, original.id)
            .await?
            .unwrap();
        assert_eq!(prior.shares, 3.0);
        assert_eq!(prior.entry_price, 0.5);
        assert!((prior.pnl.unwrap() - 0.2).abs() < 1e-9);
        assert_eq!(new_lot.shares, 5.0);
        assert_eq!(new_lot.entry_price, 0.4);
        assert!(apply_confirmed_match(&pool, &late).await?.is_empty());
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn maker_sale_refuses_legacy_inventory_without_changing_old_fee_semantics() -> Result<()>
    {
        let (pool, path) = test_pool().await?;
        register_order(&pool, &intent("buy-legacy", "BUY", 0.5, 5.0)).await?;
        let original =
            apply_confirmed_match(&pool, &matched("old-entry", "buy-legacy", "BUY", 5.0, 0.5))
                .await?
                .remove(0);
        sqlx::query("UPDATE trades SET accounting_version=0,fee_paid=0.12,pnl=0.2 WHERE id=?")
            .bind(original.id)
            .execute(&pool)
            .await?;
        register_order(&pool, &intent("sell-legacy", "SELL", 0.6, 2.0)).await?;
        assert!(apply_confirmed_match(
            &pool,
            &matched("legacy-sale", "sell-legacy", "SELL", 2.0, 0.6)
        )
        .await
        .is_err());
        let unchanged = super::super::load_trade_by_id(&pool, original.id)
            .await?
            .unwrap();
        assert_eq!(unchanged.shares, 5.0);
        assert_eq!(unchanged.accounting_version, 0);
        assert!((unchanged.net_realized_pnl() - 0.08).abs() < 1e-9);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM maker_matches WHERE match_id='legacy-sale'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 0);
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn terminal_maker_order_cannot_be_resurrected_or_reused() -> Result<()> {
        let (pool, path) = test_pool().await?;
        let order = intent("terminal", "BUY", 0.5, 5.0);
        register_order(&pool, &order).await?;
        update_state(&pool, &order.order_id, "TERMINAL").await?;
        update_state(&pool, &order.order_id, "TERMINAL").await?;
        assert!(update_state(&pool, &order.order_id, "LIVE").await.is_err());
        assert!(register_order(&pool, &order).await.is_err());
        assert!(pending_orders(&pool).await?.is_empty());
        assert_eq!(reserved_buy_collateral(&pool).await?, 0.0);
        cleanup(pool, path).await;
        Ok(())
    }

    fn budget() -> MakerBuyBudget {
        MakerBuyBudget {
            available_balance: 100.0,
            min_trade_usd: 1.0,
            max_positions: 2,
            max_inventory_notional: 0.0,
            max_exposure_pct: 1.0,
        }
    }

    #[tokio::test]
    async fn terminal_taker_uncertainty_blocks_maker_admission_without_reserving_cash() -> Result<()>
    {
        let (pool, path) = test_pool().await?;
        sqlx::query("INSERT INTO live_orders(order_id,trade_id,account,token_id,phase,price,size,fee_rate,fee_exponent,taker_only,exchange_address,protocol,state,created_at,updated_at) VALUES('uncertain-taker',1,'account','token','entry',0.5,5,0,1,1,'exchange',2,'audit_uncertain','2026-10-10T12:00:00Z','2026-10-10T12:00:00Z')")
            .execute(&pool).await?;
        let next = intent("maker-after-uncertain-taker", "BUY", 0.5, 10.0);
        assert!(admit_buy_order(&pool, &next, budget()).await.is_err());
        assert!(pending_orders(&pool).await?.is_empty());
        assert_eq!(reserved_buy_collateral(&pool).await?, 0.0);
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn untouched_maker_quotes_reserve_full_cash_positions_and_exposure_atomically(
    ) -> Result<()> {
        let (pool, path) = test_pool().await?;
        let first = intent("first-budget", "BUY", 0.5, 10.0);
        let next = intent("next-budget", "BUY", 0.5, 10.0);
        admit_buy_order(&pool, &first, budget()).await?;
        for constrained in [
            MakerBuyBudget {
                available_balance: 9.0,
                ..budget()
            },
            MakerBuyBudget {
                max_positions: 1,
                ..budget()
            },
            MakerBuyBudget {
                max_inventory_notional: 7.0,
                ..budget()
            },
            MakerBuyBudget {
                max_exposure_pct: 0.06,
                ..budget()
            },
            MakerBuyBudget {
                min_trade_usd: 6.0,
                ..budget()
            },
        ] {
            assert!(admit_buy_order(&pool, &next, constrained).await.is_err());
            assert_eq!(pending_orders(&pool).await?.len(), 1);
            assert_eq!(reserved_buy_collateral(&pool).await?, 5.0);
        }
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn partial_maker_quote_and_its_filled_lots_share_one_position_slot() -> Result<()> {
        let (pool, path) = test_pool().await?;
        let first = intent("partial-budget", "BUY", 0.5, 10.0);
        admit_buy_order(&pool, &first, budget()).await?;
        let trade = apply_confirmed_match(
            &pool,
            &matched("partial-budget-fill", "partial-budget", "BUY", 3.0, 0.5),
        )
        .await?
        .remove(0);
        sqlx::query("UPDATE trades SET status='CLOSING' WHERE id=?")
            .bind(trade.id)
            .execute(&pool)
            .await?;
        let next = intent("next-partial-budget", "BUY", 0.5, 10.0);
        assert!(admit_buy_order(
            &pool,
            &next,
            MakerBuyBudget {
                max_positions: 1,
                ..budget()
            }
        )
        .await
        .is_err());
        // One filled/closing lot plus its unfilled reservation is one slot,
        // so the second allowed slot remains available.
        admit_buy_order(&pool, &next, budget()).await?;
        assert_eq!(pending_orders(&pool).await?.len(), 2);
        assert_eq!(reserved_buy_collateral(&pool).await?, 8.5);
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_maker_admissions_cannot_both_claim_the_last_position_slot() -> Result<()> {
        let (pool, path) = test_pool().await?;
        let first = intent("concurrent-first", "BUY", 0.5, 10.0);
        let second = intent("concurrent-second", "BUY", 0.5, 10.0);
        let constrained = MakerBuyBudget {
            max_positions: 1,
            ..budget()
        };
        let (one, two) = tokio::join!(
            admit_buy_order(&pool, &first, constrained),
            admit_buy_order(&pool, &second, constrained)
        );
        assert_eq!(usize::from(one.is_ok()) + usize::from(two.is_ok()), 1);
        assert_eq!(pending_orders(&pool).await?.len(), 1);
        assert_eq!(reserved_buy_collateral(&pool).await?, 5.0);
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn manual_market_stop_is_durable_idempotent_and_does_not_block_a_new_market() -> Result<()>
    {
        let (pool, path) = test_pool().await?;
        persist_manual_stand_down(&pool, "market-stopped").await?;
        persist_manual_stand_down(&pool, "market-stopped").await?;
        pool.close().await;
        let pool = super::super::open(path.to_str().unwrap()).await?;
        assert_eq!(stood_down_markets(&pool).await?, vec!["market-stopped"]);
        assert!(market_stood_down(&pool, "market-stopped").await?);
        assert!(!market_stood_down(&pool, "new-market-after-roll").await?);
        cleanup(pool, path).await;
        Ok(())
    }

    #[tokio::test]
    async fn terminal_audit_batches_are_bounded_and_uncertainty_is_durable() -> Result<()> {
        let (pool, path) = test_pool().await?;
        for i in 0..10 {
            let order = intent(&format!("terminal-{i:02}"), "BUY", 0.5, 5.0);
            register_order(&pool, &order).await?;
            finalize_order(&pool, &order.order_id, "[]").await?;
        }
        let first = terminal_orders_batch(&pool, None, 8).await?;
        assert_eq!(first.len(), 8);
        let last = terminal_orders_batch(&pool, Some(&first.last().unwrap().order_id), 8).await?;
        assert_eq!(last.len(), 2);
        mark_audit_uncertain(&pool, &first[0].order_id).await?;
        assert_eq!(pending_orders(&pool).await?[0].state, "AUDIT_UNCERTAIN");
        assert!(update_state(&pool, &first[0].order_id, "LIVE")
            .await
            .is_err());
        pool.close().await;
        let pool = super::super::open(path.to_str().unwrap()).await?;
        assert_eq!(pending_orders(&pool).await?[0].state, "AUDIT_UNCERTAIN");
        assert!(finalize_order(&pool, &first[0].order_id, "[]")
            .await
            .is_err());
        cleanup(pool, path).await;
        Ok(())
    }
}
