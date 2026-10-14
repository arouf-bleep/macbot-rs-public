//! Durable paper maker admissions and atomic per-order partial entry lots.
//! A small fill is permitted only against a previously admitted full quote.
use anyhow::{anyhow, bail, ensure, Result};
use chrono::Utc;
use sqlx::{Sqlite, SqlitePool, Transaction};

use super::Trade;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS paper_maker_orders (
            order_id TEXT PRIMARY KEY, run_id TEXT NOT NULL,
            market_id TEXT NOT NULL, token_id TEXT NOT NULL,
            limit_price REAL NOT NULL, size REAL NOT NULL,
            filled_shares REAL NOT NULL DEFAULT 0,
            filled_cost REAL NOT NULL DEFAULT 0,
            trade_id INTEGER, template TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'OPEN', created_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS paper_maker_fill_events (
            order_id TEXT NOT NULL, observation_id TEXT NOT NULL,
            shares REAL NOT NULL, price REAL NOT NULL,
            PRIMARY KEY(order_id, observation_id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS paper_maker_orders_status ON paper_maker_orders(status,run_id)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Reserve the full quote's remaining budget and one position slot. Already
/// filled lots count in trades, so a partial order never consumes two slots.
// The atomic database admission checks exact order identity and all paper risk bounds.
#[allow(clippy::too_many_arguments)]
pub async fn admit_order(
    pool: &SqlitePool,
    template: &Trade,
    size: f64,
    available_balance: f64,
    min_trade_usd: f64,
    max_positions: u32,
    max_inventory_notional: f64,
    max_exposure_pct: f64,
) -> Result<()> {
    let order_id = template.order_id.as_deref().unwrap_or("");
    let token_id = template.token_id.as_deref().unwrap_or("");
    let run_id = template.run_id.as_deref().unwrap_or("");
    let price = template.entry_price;
    ensure!(
        order_id.starts_with("sim-mkr-")
            && !template.is_live()
            && template.strategy == "maker"
            && !run_id.is_empty()
            && !token_id.is_empty()
            && !template.market_id.is_empty()
            && matches!(template.direction.as_str(), "BUY_YES" | "BUY_NO")
            && price.is_finite()
            && price > 0.0
            && price < 1.0
            && size.is_finite()
            && size > 0.0
            && available_balance.is_finite()
            && available_balance >= 0.0
            && min_trade_usd.is_finite()
            && min_trade_usd >= 0.0
            && max_inventory_notional.is_finite()
            && max_inventory_notional >= 0.0
            && max_exposure_pct.is_finite()
            && (0.0..=1.0).contains(&max_exposure_pct),
        "invalid paper quote admission"
    );
    let notional = price * size;
    ensure!(
        notional.is_finite() && notional + 1e-9 >= min_trade_usd,
        "paper quote below minimum budget"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing: Option<(String, String, String, f64, f64, String, String)> = sqlx::query_as(
        "SELECT run_id, market_id, token_id, limit_price, size, status, template FROM paper_maker_orders WHERE order_id=?",
    )
    .bind(order_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((old_run, old_market, old_token, old_price, old_size, status, old_template)) =
        existing
    {
        let original: Trade = serde_json::from_str(&old_template)?;
        ensure!(
            old_run == run_id
                && old_market == template.market_id
                && old_token == token_id
                && original.direction == template.direction
                && original.mode == template.mode
                && old_price == price
                && old_size == size
                && status == "OPEN",
            "paper order identity reused"
        );
        tx.commit().await?;
        return Ok(());
    }
    let reserved: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(MAX(size-filled_shares,0)*limit_price),0.0) FROM paper_maker_orders WHERE status='OPEN'",
    )
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        available_balance + 1e-9 >= reserved + notional,
        "paper quote exceeds unreserved balance"
    );
    let open_positions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM trades WHERE status IN ('OPEN','CLOSING') AND strategy!='scalp'",
    )
    .fetch_one(&mut *tx)
    .await?;
    let pending_positions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM paper_maker_orders WHERE status='OPEN' AND trade_id IS NULL",
    )
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        max_positions == 0 || open_positions + pending_positions < i64::from(max_positions),
        "paper quote exceeds reserved position slots"
    );
    let market_exposure: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount),0.0) FROM trades WHERE status IN ('OPEN','CLOSING') AND market_id=?",
    )
    .bind(&template.market_id)
    .fetch_one(&mut *tx)
    .await?;
    let market_reserved: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(MAX(size-filled_shares,0)*limit_price),0.0) FROM paper_maker_orders WHERE status='OPEN' AND market_id=?",
    )
    .bind(&template.market_id)
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        max_inventory_notional <= 0.0
            || market_exposure + market_reserved + notional <= max_inventory_notional + 1e-9,
        "paper quote exceeds reserved market inventory cap"
    );
    let open_exposure: f64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount),0.0) FROM trades WHERE status IN ('OPEN','CLOSING')",
    )
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        max_exposure_pct <= 0.0
            || open_exposure + reserved + notional <= available_balance * max_exposure_pct + 1e-9,
        "paper quote exceeds reserved exposure cap"
    );
    sqlx::query(
        "INSERT INTO paper_maker_orders(order_id,run_id,market_id,token_id,limit_price,size,template,created_at) VALUES(?,?,?,?,?,?,?,?)",
    )
    .bind(order_id)
    .bind(run_id)
    .bind(&template.market_id)
    .bind(token_id)
    .bind(price)
    .bind(size)
    .bind(serde_json::to_string(template)?)
    .bind(Utc::now().to_rfc3339())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn cancel_order(pool: &SqlitePool, order_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE paper_maker_orders SET status='CANCELLED' WHERE order_id=? AND status='OPEN'",
    )
    .bind(order_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn continuing_lot(
    pool: &SqlitePool,
    order_id: &str,
    market_id: &str,
    direction: &str,
    price: f64,
    remaining_budget: f64,
) -> Result<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT o.trade_id FROM paper_maker_orders o JOIN trades t ON t.id=o.trade_id
         WHERE o.order_id=? AND o.market_id=? AND t.direction=? AND t.mode!='live'
         AND o.status='OPEN' AND t.status='OPEN' AND COALESCE(t.partial_closed,0)=0
         AND ABS(o.limit_price-?)<1e-9 AND (o.size-o.filled_shares)*o.limit_price+1e-9>=?",
    )
    .bind(order_id)
    .bind(market_id)
    .bind(direction)
    .bind(price)
    .bind(remaining_budget)
    .fetch_optional(pool)
    .await?)
}

pub async fn expire_unrestored_quotes(pool: &SqlitePool) -> Result<()> {
    // Paper quotes exist only in actor memory. Reused run labels must not
    // leave invisible budget reservations behind after a process restart.
    sqlx::query("UPDATE paper_maker_orders SET status='CANCELLED' WHERE status='OPEN'")
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug)]
pub struct AppliedPaperFill {
    pub trade: Trade,
    pub debit: f64,
    pub applied_shares: f64,
    pub new_position: bool,
}

/// The order, event identity, position and cumulative fill snapshot change in
/// one SQLite transaction. Replaying the same observation debits nothing.
pub async fn apply_entry_fill(
    pool: &SqlitePool,
    order_id: &str,
    observation_id: &str,
    shares: f64,
    price: f64,
    available_balance: f64,
    details: &str,
) -> Result<AppliedPaperFill> {
    ensure!(
        !observation_id.is_empty()
            && shares.is_finite()
            && shares > 0.0
            && price.is_finite()
            && price > 0.0
            && price < 1.0
            && available_balance.is_finite()
            && available_balance >= 0.0,
        "invalid paper fill"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (limit_price, size, old_shares, old_cost, trade_id, template_json, status):
        (f64,f64,f64,f64,Option<i64>,String,String) = sqlx::query_as(
        "SELECT limit_price,size,filled_shares,filled_cost,trade_id,template,status FROM paper_maker_orders WHERE order_id=?",
    ).bind(order_id).fetch_optional(&mut *tx).await?.ok_or_else(||anyhow!("paper fill has no admitted order"))?;
    let duplicate: Option<(f64, f64)> = sqlx::query_as(
        "SELECT shares,price FROM paper_maker_fill_events WHERE order_id=? AND observation_id=?",
    )
    .bind(order_id)
    .bind(observation_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((old_qty, old_price)) = duplicate {
        ensure!(
            old_qty == shares && old_price == price,
            "paper observation changed after accounting"
        );
        let trade: Trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
            .bind(trade_id.ok_or_else(|| anyhow!("paper duplicate lacks trade identity"))?)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(AppliedPaperFill {
            trade,
            debit: 0.0,
            applied_shares: 0.0,
            new_position: false,
        });
    }
    ensure!(status == "OPEN", "paper order is no longer resting");
    ensure!(
        price <= limit_price + 1e-9 && old_shares + shares <= size + 1e-9,
        "paper fill exceeds admitted order"
    );
    let debit = shares * price;
    ensure!(
        debit.is_finite() && available_balance + 1e-9 >= debit,
        "paper fill exceeds available balance"
    );
    let mut trade = if let Some(id) = trade_id {
        let trade: Trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        // Cancel the unfilled buy remainder once its lot is sold; it may not
        // resurrect a closed lot or rewrite a realised partial-exit cost basis.
        if !trade.is_open() || trade.partial_closed.unwrap_or(0) != 0 {
            sqlx::query("UPDATE paper_maker_orders SET status='CANCELLED' WHERE order_id=?")
                .bind(order_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            bail!("paper lot has begun exiting; entry remainder cancelled");
        }
        trade
    } else {
        serde_json::from_str::<Trade>(&template_json)?
    };
    ensure!(
        !trade.is_live()
            && trade.strategy == "maker"
            && trade.order_id.as_deref() == Some(order_id),
        "paper order template mismatch"
    );
    let total_shares = old_shares + shares;
    let total_cost = old_cost + debit;
    trade.shares = total_shares;
    trade.amount = total_cost;
    trade.entry_price = total_cost / total_shares;
    trade.status = "OPEN".into();
    let new_position = trade_id.is_none();
    if new_position {
        trade.id = insert_paper_lot(&mut tx, &trade).await?;
    } else {
        sqlx::query("UPDATE trades SET shares=?,amount=?,entry_price=?,entry_shares=?,entry_cost=? WHERE id=? AND status='OPEN'")
            .bind(total_shares).bind(total_cost).bind(trade.entry_price).bind(total_shares).bind(total_cost).bind(trade.id).execute(&mut *tx).await?;
    }
    sqlx::query(
        "INSERT INTO paper_maker_fill_events(order_id,observation_id,shares,price) VALUES(?,?,?,?)",
    )
    .bind(order_id)
    .bind(observation_id)
    .bind(shares)
    .bind(price)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO trade_fills(timestamp,trade_id,market_id,order_id,phase,intended_price,avg_price,cumulative_size,delta_size,delta_price,source,details) VALUES(?,?,?,?,'entry',?,?,?,?,?,'maker_sim',?)")
        .bind(Utc::now().to_rfc3339()).bind(trade.id).bind(&trade.market_id).bind(order_id)
        .bind(limit_price).bind(trade.entry_price).bind(total_shares).bind(shares).bind(price).bind(details).execute(&mut *tx).await?;
    sqlx::query("UPDATE paper_maker_orders SET filled_shares=?,filled_cost=?,trade_id=?,status=? WHERE order_id=?")
        .bind(total_shares).bind(total_cost).bind(trade.id)
        .bind(if total_shares + 1e-9 >= size {"FILLED"} else {"OPEN"}).bind(order_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(AppliedPaperFill {
        trade,
        debit,
        applied_shares: shares,
        new_position,
    })
}

async fn insert_paper_lot(tx: &mut Transaction<'_, Sqlite>, trade: &Trade) -> Result<i64> {
    let result = sqlx::query("INSERT INTO trades(timestamp,market_id,question,direction,entry_price,amount,shares,expected_value,reason,confidence,status,mode,source,end_date,partial_closed,raw_confidence,order_id,token_id,fok_failed,fee_paid,signals_json,calibration_version,run_id,strategy,entry_shares,entry_cost) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
        .bind(&trade.timestamp).bind(&trade.market_id).bind(&trade.question).bind(&trade.direction)
        .bind(trade.entry_price).bind(trade.amount).bind(trade.shares).bind(trade.expected_value)
        .bind(&trade.reason).bind(trade.confidence).bind(&trade.status).bind(&trade.mode).bind(&trade.source)
        .bind(&trade.end_date).bind(trade.partial_closed).bind(trade.raw_confidence).bind(&trade.order_id)
        .bind(&trade.token_id).bind(trade.fok_failed).bind(trade.fee_paid).bind(&trade.signals_json)
        .bind(&trade.calibration_version).bind(&trade.run_id).bind(&trade.strategy).bind(trade.shares)
        .bind(trade.amount).execute(&mut **tx).await?;
    Ok(result.last_insert_rowid())
}

/// Execution evidence committed with a simulated lot transition. A zero
/// price is valid for a losing resolution and is not an absent fill price.
pub struct PaperExecutionEvidence<'a> {
    pub order_id: &'a str,
    pub phase: &'a str,
    pub intended_price: f64,
    pub avg_price: f64,
    pub shares: f64,
    pub source: &'a str,
    pub details: &'a str,
}

async fn insert_execution_evidence(
    tx: &mut Transaction<'_, Sqlite>,
    trade: &Trade,
    timestamp: &str,
    evidence: &PaperExecutionEvidence<'_>,
) -> Result<()> {
    ensure!(
        !evidence.order_id.trim().is_empty()
            && matches!(
                evidence.phase,
                "entry" | "exit" | "partial_exit" | "mark_exit"
            )
            && evidence.shares.is_finite()
            && evidence.shares > 1e-9
            && evidence.intended_price.is_finite()
            && (0.0..=1.0).contains(&evidence.intended_price)
            && evidence.avg_price.is_finite()
            && (0.0..=1.0).contains(&evidence.avg_price),
        "invalid paper execution evidence"
    );
    sqlx::query("INSERT INTO trade_fills(timestamp,trade_id,market_id,order_id,phase,intended_price,avg_price,cumulative_size,delta_size,delta_price,source,details) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")
        .bind(timestamp).bind(trade.id).bind(&trade.market_id).bind(evidence.order_id)
        .bind(evidence.phase).bind(evidence.intended_price).bind(evidence.avg_price)
        .bind(evidence.shares).bind(evidence.shares).bind(evidence.avg_price)
        .bind(evidence.source).bind(evidence.details).execute(&mut **tx).await?;
    Ok(())
}

/// An orderbook-based simulated entry and its execution evidence either both
/// exist or neither does. Cash is published by the actor after this commit.
pub async fn insert_taker_entry(
    pool: &SqlitePool,
    template: &Trade,
    evidence: &PaperExecutionEvidence<'_>,
) -> Result<Trade> {
    ensure!(
        template.id == 0
            && template.is_open()
            && !template.is_live()
            && template.accounting_version == 0
            && template.pnl.is_none()
            && template.exit_price.is_none()
            && template.entry_price.is_finite()
            && template.entry_price > 0.0
            && template.entry_price < 1.0
            && template.amount.is_finite()
            && template.amount > 0.0
            && template.fee_paid.is_finite()
            && template.fee_paid >= 0.0
            && template.shares.is_finite()
            && (template.shares - evidence.shares).abs() <= 1e-9
            && (template.entry_price - evidence.avg_price).abs() <= 1e-9
            && template.order_id.as_deref() == Some(evidence.order_id)
            && evidence.phase == "entry",
        "invalid simulated taker entry"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let mut trade = template.clone();
    trade.id = insert_paper_lot(&mut tx, &trade).await?;
    insert_execution_evidence(&mut tx, &trade, &trade.timestamp, evidence).await?;
    tx.commit().await?;
    Ok(trade)
}

/// Preserve the accounting lot and exit evidence together, including explicit
/// non-exchange marks and resolution payouts. Never publish a closed lot whose
/// evidence failed to persist.
pub async fn apply_full_exit(
    pool: &SqlitePool,
    original: &Trade,
    closed: &Trade,
    evidence: &PaperExecutionEvidence<'_>,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    apply_full_exit_in_transaction(&mut tx, original, closed, evidence).await?;
    tx.commit().await?;
    Ok(())
}

/// Both complementary paper legs, execution marks and UTC realization events
/// share one commit. A failure on either leg cannot create a single-leg payout.
pub async fn apply_pair_merge(
    pool: &SqlitePool,
    first: &Trade,
    first_closed: &Trade,
    first_evidence: &PaperExecutionEvidence<'_>,
    second: &Trade,
    second_closed: &Trade,
    second_evidence: &PaperExecutionEvidence<'_>,
) -> Result<()> {
    ensure!(
        first.id != second.id
            && !first.market_id.is_empty()
            && first.market_id == second.market_id
            && matches!(
                first.mode_kind(),
                crate::domain::BotMode::Paper
                    | crate::domain::BotMode::Shadow
                    | crate::domain::BotMode::Simulation
            )
            && matches!(
                second.mode_kind(),
                crate::domain::BotMode::Paper
                    | crate::domain::BotMode::Shadow
                    | crate::domain::BotMode::Simulation
            )
            && first.shares.is_finite()
            && first.shares > 1e-9
            && first.shares == second.shares
            && matches!(
                (first.direction_kind(), second.direction_kind()),
                (
                    crate::domain::Direction::BuyYes,
                    crate::domain::Direction::BuyNo
                ) | (
                    crate::domain::Direction::BuyNo,
                    crate::domain::Direction::BuyYes
                )
            )
            && first_closed.exit_timestamp == second_closed.exit_timestamp,
        "pair merge requires equal complementary paper inventory"
    );
    for (original, closed, evidence) in [
        (first, first_closed, first_evidence),
        (second, second_closed, second_evidence),
    ] {
        ensure!(
            original.entry_price.is_finite()
                && (0.0..=1.0).contains(&original.entry_price)
                && original.amount.is_finite()
                && original.amount >= 0.0
                && original.accounting_version == 0
                && (original.amount - original.shares * original.entry_price).abs()
                    <= 1e-9 * (original.shares * original.entry_price).max(1.0)
                && original.pnl.is_none_or(f64::is_finite)
                && closed.pnl
                    == Some(
                        original.pnl.unwrap_or(0.0)
                            + original.shares * (0.5 - original.entry_price)
                    )
                && closed.exit_price == Some(0.5)
                && closed.exit_type.as_deref() == Some("merged")
                && evidence.phase == "mark_exit"
                && evidence.source == "pair_merge"
                && evidence.intended_price == 0.5
                && evidence.avg_price == 0.5,
            "invalid simulated pair payout accounting"
        );
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    apply_full_exit_in_transaction(&mut tx, first, first_closed, first_evidence).await?;
    apply_full_exit_in_transaction(&mut tx, second, second_closed, second_evidence).await?;
    tx.commit().await?;
    Ok(())
}

async fn apply_full_exit_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    original: &Trade,
    closed: &Trade,
    evidence: &PaperExecutionEvidence<'_>,
) -> Result<()> {
    ensure!(
        original.id > 0
            && original.is_open()
            && !original.is_live()
            && closed.id == original.id
            && closed.status == "CLOSED"
            && !closed.is_live()
            && original.shares.is_finite()
            && original.shares > 1e-9
            && closed.shares == original.shares
            && closed.amount == original.amount
            && closed.entry_price == original.entry_price
            && closed.pnl.is_some_and(|pnl| pnl.is_finite())
            && closed.exit_timestamp.is_some()
            && closed.exit_type.is_some()
            && closed.exit_price == Some(evidence.avg_price)
            && (original.shares - evidence.shares).abs() <= 1e-9
            && matches!(evidence.phase, "exit" | "partial_exit" | "mark_exit"),
        "invalid paper full exit"
    );
    let persisted: Trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
        .bind(original.id)
        .fetch_one(&mut **tx)
        .await?;
    ensure!(
        serde_json::to_value(&persisted)? == serde_json::to_value(original)?,
        "paper lot changed before its full exit"
    );
    sqlx::query("UPDATE trades SET status='CLOSED',token_id=?,exit_price=?,exit_timestamp=?,pnl=?,exit_type=?,was_correct=?,fok_failed=? WHERE id=?")
        .bind(&closed.token_id).bind(closed.exit_price).bind(&closed.exit_timestamp)
        .bind(closed.pnl).bind(&closed.exit_type).bind(closed.was_correct)
        .bind(closed.fok_failed).bind(closed.id).execute(&mut **tx).await?;
    insert_execution_evidence(
        tx,
        closed,
        closed.exit_timestamp.as_deref().unwrap_or_default(),
        evidence,
    )
    .await?;
    record_paper_realization(
        tx,
        original,
        closed,
        evidence.order_id,
        closed.exit_timestamp.as_deref().unwrap_or_default(),
        evidence.shares,
    )
    .await?;
    Ok(())
}

/// A paper sale changes its remaining lot and records its execution in the
/// same commit. Callers publish cash and hot state only after this succeeds.
#[allow(clippy::too_many_arguments)]
pub async fn apply_partial_exit(
    pool: &SqlitePool,
    original: &Trade,
    updated: &Trade,
    order_id: &str,
    intended_price: f64,
    avg_price: f64,
    closed_size: f64,
    source: &str,
    details: &str,
) -> Result<()> {
    ensure!(
        original.id > 0
            && updated.id == original.id
            && original.is_open()
            && !original.is_live()
            && updated.is_open()
            && !updated.is_live()
            && !order_id.trim().is_empty()
            && closed_size.is_finite()
            && closed_size > 1e-9
            && intended_price.is_finite()
            && (0.0..=1.0).contains(&intended_price)
            && avg_price.is_finite()
            && (0.0..=1.0).contains(&avg_price)
            && original.shares.is_finite()
            && updated.shares.is_finite()
            && updated.shares > 0.0
            && updated.shares < original.shares
            && (original.shares - updated.shares - closed_size).abs() <= 1e-9
            && updated.amount.is_finite()
            && updated.amount >= 0.0
            && updated.pnl.is_some_and(|pnl| pnl.is_finite())
            && updated.partial_closed == Some(1),
        "invalid paper partial exit"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let persisted: Trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
        .bind(original.id)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        serde_json::to_value(&persisted)? == serde_json::to_value(original)?,
        "paper lot changed before its partial exit"
    );
    sqlx::query("UPDATE trades SET shares=?,amount=?,partial_closed=1,pnl=? WHERE id=?")
        .bind(updated.shares)
        .bind(updated.amount)
        .bind(updated.pnl)
        .bind(updated.id)
        .execute(&mut *tx)
        .await?;
    let timestamp = Utc::now().to_rfc3339();
    insert_execution_evidence(
        &mut tx,
        original,
        &timestamp,
        &PaperExecutionEvidence {
            order_id,
            phase: "partial_exit",
            intended_price,
            avg_price,
            shares: closed_size,
            source,
            details,
        },
    )
    .await?;
    record_paper_realization(
        &mut tx,
        original,
        updated,
        order_id,
        &timestamp,
        closed_size,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn record_paper_realization(
    tx: &mut Transaction<'_, Sqlite>,
    original: &Trade,
    updated: &Trade,
    order_id: &str,
    timestamp: &str,
    sold_shares: f64,
) -> Result<()> {
    let entry_shares: Option<f64> =
        sqlx::query_scalar("SELECT entry_shares FROM trades WHERE id=?")
            .bind(original.id)
            .fetch_one(&mut **tx)
            .await?;
    let entry_fee = if original.accounting_version == 1 || original.fee_paid == 0.0 {
        0.0
    } else {
        let quantity = entry_shares
            .filter(|q| q.is_finite() && *q > 0.0)
            .ok_or_else(|| {
                anyhow!("paper entry quantity unavailable for realized fee allocation")
            })?;
        ensure!(
            sold_shares <= quantity + 1e-9,
            "paper sale exceeds original entry quantity"
        );
        original.fee_paid * sold_shares / quantity
    };
    let net_pnl = updated.pnl.unwrap_or(0.0) - original.pnl.unwrap_or(0.0) - entry_fee;
    let key = serde_json::to_string(&["paper", order_id])?;
    super::risk::record_realized_delta(tx, original.id, &key, timestamp, net_pnl, sold_shares).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn partial_and_full_sales_record_net_daily_deltas_atomically_without_double_counting(
    ) -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        let mut original = template("sim-taker-daily-loss");
        original.strategy = "conviction".into();
        original.shares = 10.0;
        original.amount = 5.0;
        original.fee_paid = 1.0;
        original.id = super::super::insert_trade(&pool, &original).await?;
        let mut partial = original.clone();
        partial.shares = 6.0;
        partial.amount = 3.0;
        partial.pnl = Some(-1.0);
        partial.partial_closed = Some(1);
        sqlx::query("CREATE TRIGGER reject_realized_event BEFORE INSERT ON realized_pnl_events BEGIN SELECT RAISE(ABORT,'fixture risk ledger failure'); END").execute(&pool).await?;
        assert!(apply_partial_exit(
            &pool, &original, &partial, "sale-one", 0.25, 0.25, 4.0, "fixture", "{}"
        )
        .await
        .is_err());
        let unchanged = super::super::load_trade_by_id(&pool, original.id)
            .await?
            .unwrap();
        assert_eq!(unchanged.shares, 10.0);
        assert_eq!(unchanged.pnl, None);
        assert_eq!(
            super::super::load_trade_fills(&pool, original.id, 10)
                .await?
                .len(),
            0
        );
        sqlx::query("DROP TRIGGER reject_realized_event")
            .execute(&pool)
            .await?;
        apply_partial_exit(
            &pool, &original, &partial, "sale-one", 0.25, 0.25, 4.0, "fixture", "{}",
        )
        .await?;
        let day = Utc::now().date_naive();
        let snapshot = super::super::risk::daily_realized_pnl(&pool, day).await?;
        assert!((snapshot.exact_today_pnl.unwrap() + 1.4).abs() < 1e-9);
        let mut closed = partial.clone();
        closed.status = "CLOSED".into();
        closed.exit_price = Some(0.4);
        closed.exit_timestamp = Some(Utc::now().to_rfc3339());
        closed.exit_type = Some("manual".into());
        closed.pnl = Some(-1.6);
        apply_full_exit(
            &pool,
            &partial,
            &closed,
            &PaperExecutionEvidence {
                order_id: "sale-two",
                phase: "exit",
                intended_price: 0.4,
                avg_price: 0.4,
                shares: 6.0,
                source: "fixture",
                details: "{}",
            },
        )
        .await?;
        let snapshot = super::super::risk::daily_realized_pnl(&pool, day).await?;
        assert!((snapshot.exact_today_pnl.unwrap() + 2.6).abs() < 1e-9);
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM realized_pnl_events WHERE trade_id=?")
                .bind(original.id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 2);
        Ok(())
    }

    fn template(id: &str) -> Trade {
        Trade {
            timestamp: Utc::now().to_rfc3339(),
            market_id: "paper-market".into(),
            token_id: Some("paper-token".into()),
            direction: "BUY_YES".into(),
            entry_price: 0.5,
            status: "OPEN".into(),
            mode: "simulation".into(),
            strategy: "maker".into(),
            order_id: Some(id.into()),
            run_id: Some("run-1".into()),
            partial_closed: Some(0),
            ..Trade::default()
        }
    }

    #[tokio::test]
    async fn taker_entry_evidence_failure_rolls_back_lot_and_preserves_arrival_timestamp_and_fee() {
        let pool = super::super::open(":memory:").await.unwrap();
        let trade = Trade {
            amount: 2.5,
            shares: 5.0,
            fee_paid: 0.1,
            strategy: "conviction".into(),
            ..template("sim-taker-atomic")
        };
        let evidence = PaperExecutionEvidence {
            order_id: "sim-taker-atomic",
            phase: "entry",
            intended_price: 0.51,
            avg_price: 0.5,
            shares: 5.0,
            source: "fixture-arrival",
            details: "{}",
        };
        sqlx::query("CREATE TRIGGER reject_taker_entry BEFORE INSERT ON trade_fills BEGIN SELECT RAISE(ABORT,'fixture entry failure'); END")
            .execute(&pool).await.unwrap();
        assert!(insert_taker_entry(&pool, &trade, &evidence).await.is_err());
        assert!(super::super::load_open_trades(&pool)
            .await
            .unwrap()
            .is_empty());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trade_fills")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        sqlx::query("DROP TRIGGER reject_taker_entry")
            .execute(&pool)
            .await
            .unwrap();
        let inserted = insert_taker_entry(&pool, &trade, &evidence).await.unwrap();
        assert_eq!(inserted.fee_paid, 0.1);
        let persisted = super::super::load_trade_by_id(&pool, inserted.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.timestamp, trade.timestamp);
        assert_eq!(persisted.fee_paid, 0.1);
        let fills = super::super::load_trade_fills(&pool, inserted.id, 10)
            .await
            .unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].timestamp, trade.timestamp);
        assert_eq!(fills[0].intended_price, 0.51);
        assert_eq!(fills[0].avg_price, 0.5);
        assert_eq!(fills[0].delta_size, 5.0);
    }

    #[tokio::test]
    async fn admitted_small_tranches_use_one_lot_and_replay_without_another_debit() {
        let pool = super::super::open(":memory:").await.unwrap();
        let seed = template("sim-mkr-small");
        admit_order(&pool, &seed, 10.0, 20.0, 1.0, 1, 10.0, 0.0)
            .await
            .unwrap();
        let first = apply_entry_fill(&pool, "sim-mkr-small", "revision-1", 0.5, 0.5, 20.0, "{}")
            .await
            .unwrap();
        assert!(first.new_position);
        assert_eq!(first.debit, 0.25);
        let second = apply_entry_fill(&pool, "sim-mkr-small", "revision-2", 1.0, 0.4, 19.75, "{}")
            .await
            .unwrap();
        assert!(!second.new_position);
        assert_eq!(first.trade.id, second.trade.id);
        assert_eq!(second.trade.shares, 1.5);
        assert!((second.trade.amount - 0.65).abs() < 1e-9);
        assert!((second.trade.entry_price - 0.65 / 1.5).abs() < 1e-9);
        let duplicate =
            apply_entry_fill(&pool, "sim-mkr-small", "revision-2", 1.0, 0.4, 19.35, "{}")
                .await
                .unwrap();
        assert_eq!(duplicate.debit, 0.0);
        assert_eq!(duplicate.applied_shares, 0.0);
        assert_eq!(
            super::super::load_open_trades(&pool).await.unwrap().len(),
            1
        );
        let fills = super::super::load_trade_fills(&pool, first.trade.id, 20)
            .await
            .unwrap();
        assert_eq!(fills.len(), 2);
        assert!(
            (super::super::trade_entry_cost(&pool, first.trade.id)
                .await
                .unwrap()
                .unwrap()
                - 0.65)
                .abs()
                < 1e-9
        );
    }

    #[tokio::test]
    async fn reservations_enforce_cash_slots_and_market_caps_before_any_fill() {
        let pool = super::super::open(":memory:").await.unwrap();
        admit_order(
            &pool,
            &template("sim-mkr-one"),
            10.0,
            10.0,
            1.0,
            1,
            10.0,
            0.0,
        )
        .await
        .unwrap();
        assert!(admit_order(
            &pool,
            &template("sim-mkr-two"),
            10.0,
            10.0,
            1.0,
            1,
            10.0,
            0.0
        )
        .await
        .is_err());
        assert!(admit_order(
            &pool,
            &template("sim-mkr-two"),
            12.0,
            10.0,
            1.0,
            0,
            20.0,
            0.0
        )
        .await
        .is_err());
        assert!(admit_order(
            &pool,
            &template("sim-mkr-two"),
            10.0,
            20.0,
            1.0,
            0,
            9.0,
            0.0
        )
        .await
        .is_err());
        assert!(admit_order(
            &pool,
            &template("sim-mkr-two"),
            10.0,
            20.0,
            1.0,
            0,
            20.0,
            0.4
        )
        .await
        .is_err());
        cancel_order(&pool, "sim-mkr-one").await.unwrap();
        admit_order(
            &pool,
            &template("sim-mkr-two"),
            10.0,
            10.0,
            1.0,
            1,
            10.0,
            0.0,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn missing_admission_oversize_cancelled_and_exited_lots_refuse_new_fills() {
        let pool = super::super::open(":memory:").await.unwrap();
        assert!(
            apply_entry_fill(&pool, "sim-mkr-no-admission", "one", 1.0, 0.5, 20.0, "{}")
                .await
                .is_err()
        );
        let seed = template("sim-mkr-bounds");
        admit_order(&pool, &seed, 5.0, 20.0, 1.0, 2, 20.0, 0.0)
            .await
            .unwrap();
        assert!(
            apply_entry_fill(&pool, "sim-mkr-bounds", "big", 6.0, 0.5, 20.0, "{}")
                .await
                .is_err()
        );
        let filled = apply_entry_fill(&pool, "sim-mkr-bounds", "one", 1.0, 0.5, 20.0, "{}")
            .await
            .unwrap();
        sqlx::query("UPDATE trades SET partial_closed=1 WHERE id=?")
            .bind(filled.trade.id)
            .execute(&pool)
            .await
            .unwrap();
        assert!(
            apply_entry_fill(&pool, "sim-mkr-bounds", "two", 1.0, 0.5, 19.5, "{}")
                .await
                .is_err()
        );
        assert!(
            apply_entry_fill(&pool, "sim-mkr-bounds", "three", 1.0, 0.5, 19.5, "{}")
                .await
                .is_err()
        );
        assert_eq!(
            super::super::load_open_trades(&pool).await.unwrap()[0].shares,
            1.0
        );
    }

    #[tokio::test]
    async fn fill_snapshot_failure_rolls_back_lot_event_and_balance_debit() {
        let pool = super::super::open(":memory:").await.unwrap();
        admit_order(
            &pool,
            &template("sim-mkr-atomic"),
            10.0,
            20.0,
            1.0,
            1,
            20.0,
            0.0,
        )
        .await
        .unwrap();
        sqlx::query("CREATE TRIGGER reject_paper_fill BEFORE INSERT ON trade_fills BEGIN SELECT RAISE(ABORT,'fixture failure'); END").execute(&pool).await.unwrap();
        assert!(
            apply_entry_fill(&pool, "sim-mkr-atomic", "one", 1.0, 0.5, 20.0, "{}")
                .await
                .is_err()
        );
        assert!(super::super::load_open_trades(&pool)
            .await
            .unwrap()
            .is_empty());
        let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_maker_fill_events")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(events, 0);
        sqlx::query("DROP TRIGGER reject_paper_fill")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            apply_entry_fill(&pool, "sim-mkr-atomic", "one", 1.0, 0.5, 20.0, "{}")
                .await
                .unwrap()
                .debit,
            0.5
        );
    }

    #[tokio::test]
    async fn restart_expires_old_quotes_but_retains_accounted_positions_and_event_identity() {
        let pool = super::super::open(":memory:").await.unwrap();
        admit_order(
            &pool,
            &template("sim-mkr-old-run"),
            10.0,
            20.0,
            1.0,
            2,
            20.0,
            0.0,
        )
        .await
        .unwrap();
        let first = apply_entry_fill(&pool, "sim-mkr-old-run", "one", 1.0, 0.5, 20.0, "{}")
            .await
            .unwrap();
        expire_unrestored_quotes(&pool).await.unwrap();
        let duplicate = apply_entry_fill(&pool, "sim-mkr-old-run", "one", 1.0, 0.5, 19.5, "{}")
            .await
            .unwrap();
        assert_eq!(duplicate.debit, 0.0);
        assert_eq!(duplicate.trade.id, first.trade.id);
        assert!(
            apply_entry_fill(&pool, "sim-mkr-old-run", "two", 1.0, 0.5, 19.5, "{}")
                .await
                .is_err()
        );
        assert_eq!(
            super::super::load_open_trades(&pool).await.unwrap()[0].shares,
            1.0
        );
    }
}
