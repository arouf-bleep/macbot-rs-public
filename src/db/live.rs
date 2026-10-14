//! Durable taker submission intents and confirmed settlement accounting.
//! No network request is allowed before its identity and budget are committed.

use anyhow::{anyhow, bail, ensure, Result};
use chrono::Utc;
use serde_json::json;
use sqlx::{Row, SqlitePool};

use super::Trade;
use crate::clob::settlement::{SettledOrderFill, SettlementOrderIdentity};
use crate::clob::{MarketFeeParameters, OrderInfo, OrderProtocol, Side, SignedOrder, VenueTrade};

#[derive(Debug, Clone)]
pub struct LiveOrder {
    pub order_id: String,
    pub trade_id: i64,
    pub account: String,
    pub token_id: String,
    pub phase: String,
    pub price: f64,
    pub size: f64,
    pub identity: SettlementOrderIdentity,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clob::ClobAuth;

    async fn fixture() -> (SqlitePool, i64, ClobAuth) {
        let pool = crate::db::open(":memory:").await.unwrap();
        let auth = ClobAuth::new(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "public-api-key",
            "cHVibGljLWZpeHR1cmUtc2VjcmV0",
            "public-passphrase",
        )
        .unwrap();
        let trade = Trade {
            timestamp: Utc::now().to_rfc3339(),
            market_id: "market-1".into(),
            direction: "BUY_YES".into(),
            mode: "live".into(),
            status: "OPEN".into(),
            token_id: Some("123".into()),
            amount: 3.0,
            entry_price: 0.5,
            ..Trade::default()
        };
        let id = super::super::insert_trade(&pool, &trade).await.unwrap();
        (pool, id, auth)
    }

    async fn intent(
        pool: &SqlitePool,
        id: i64,
        auth: &ClobAuth,
        phase: &str,
        salt: u64,
        price: f64,
        size: f64,
    ) -> LiveOrder {
        let (maker, taker) = if phase == "entry" {
            (size * price * 1e6, size * 1e6)
        } else {
            (size * 1e6, size * price * 1e6)
        };
        let signed = auth
            .sign_order_v2(
                "123",
                maker.round() as u128,
                taker.round() as u128,
                u8::from(phase == "exit"),
                salt,
                1717171717000,
                false,
            )
            .unwrap();
        prepare_order(
            pool,
            id,
            &auth.address(),
            &signed,
            price,
            MarketFeeParameters {
                rate: 0.07,
                exponent: 1.0,
                taker_only: true,
            },
            phase,
        )
        .await
        .unwrap();
        pending_orders(pool).await.unwrap().pop().unwrap()
    }

    fn evidence(
        order: &LiveOrder,
        qty: f64,
        price: f64,
        fee: f64,
        confirmed: bool,
    ) -> (OrderInfo, Vec<VenueTrade>, Vec<SettledOrderFill>) {
        let side = if order.phase == "entry" {
            "BUY"
        } else {
            "SELL"
        };
        let tx = format!(
            "0x{:064x}",
            order.trade_id as u64 + u64::from(order.phase == "exit") + 100
        );
        let info = OrderInfo {
            id: Some(order.order_id.clone()),
            asset_id: Some("123".into()),
            side: Some(side.into()),
            original_size: Some(order.size),
            size_matched: qty,
            price: order.price,
            status: "MATCHED".into(),
            associate_trades: vec!["match-1".into()],
        };
        let history = vec![VenueTrade {
            id: "match-1".into(),
            taker_order_id: order.order_id.clone(),
            market: "market-1".into(),
            asset_id: "123".into(),
            side: side.into(),
            size: qty,
            price,
            status: if confirmed { "CONFIRMED" } else { "MATCHED" }.into(),
            trader_side: "TAKER".into(),
            maker_address: order.account.clone(),
            fee_rate_bps: None,
            maker_orders: vec![],
            match_time: None,
            transaction_hash: Some(tx.clone()),
        }];
        let receipts = if confirmed {
            vec![SettledOrderFill {
                transaction_hash: tx,
                log_index: 1,
                block_number: 100,
                order_id: order.order_id.clone(),
                gross_shares: qty,
                gross_collateral: qty * price,
                fee_collateral: fee,
                fee_shares: 0.0,
            }]
        } else {
            vec![]
        };
        (info, history, receipts)
    }

    #[tokio::test]
    async fn confirmation_and_actual_receipts_are_required_and_duplicates_are_idempotent() {
        let (pool, id, auth) = fixture().await;
        let order = intent(&pool, id, &auth, "entry", 1, 0.6, 5.0).await;
        let (info, history, receipts) = evidence(&order, 5.0, 0.5, 0.05, false);
        let pending = reconcile_order(&pool, &order, &info, &history, &receipts)
            .await
            .unwrap();
        assert_eq!(pending.shares, 0.0);
        assert_eq!(pending_orders(&pool).await.unwrap().len(), 1);
        let (info, history, mut receipts) = evidence(&order, 5.0, 0.5, 0.05, true);
        receipts.push(receipts[0].clone());
        let settled = reconcile_order(&pool, &order, &info, &history, &receipts)
            .await
            .unwrap();
        assert_eq!(settled.entry_price, 0.5, "limit 0.6 is not execution VWAP");
        assert_eq!(settled.amount, 2.5);
        assert_eq!(
            settled.fee_paid, 0.05,
            "record receipt fee, not the market fee estimate"
        );
        assert_eq!(settled.shares, 5.0);
        assert!(settled.order_id.is_none());
        let again = reconcile_order(&pool, &order, &info, &history, &receipts)
            .await
            .unwrap();
        assert_eq!(again.shares, 5.0);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        assert!(pending_orders(&pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn transaction_hash_case_does_not_change_association_or_deduplication() {
        let (pool, id, auth) = fixture().await;
        let order = intent(&pool, id, &auth, "entry", 1, 0.6, 5.0).await;
        let (mut info, mut history, mut receipts) = evidence(&order, 2.0, 0.5, 0.02, true);
        info.status = "LIVE".into();
        let tx = format!("0x{:064x}", 0xabcd_u64);
        receipts[0].transaction_hash = tx.clone();
        history[0].transaction_hash = Some(format!("0x{}", tx[2..].to_ascii_uppercase()));
        let mut unrelated = receipts.clone();
        unrelated[0].transaction_hash = format!("0x{:064x}", 999);
        assert!(reconcile_order(&pool, &order, &info, &history, &unrelated)
            .await
            .is_err());
        let partial = reconcile_order(&pool, &order, &info, &history, &receipts)
            .await
            .unwrap();
        assert_eq!(partial.shares, 2.0);
        receipts[0].transaction_hash = format!("0x{}", tx[2..].to_ascii_uppercase());
        let repeated = reconcile_order(&pool, &order, &info, &history, &receipts)
            .await
            .unwrap();
        assert_eq!(repeated.shares, 2.0);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_matches")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn unmatched_orders_keep_entry_and_exit_intents_pending_until_cancellation() {
        for phase in ["entry", "exit"] {
            let (pool, id, auth) = fixture().await;
            if phase == "exit" {
                let entry = intent(&pool, id, &auth, "entry", 1, 0.6, 5.0).await;
                let (info, history, receipts) = evidence(&entry, 5.0, 0.5, 0.05, true);
                reconcile_order(&pool, &entry, &info, &history, &receipts)
                    .await
                    .unwrap();
            }
            let order = intent(&pool, id, &auth, phase, 2, 0.5, 5.0).await;
            let (mut info, _, _) = evidence(&order, 0.0, 0.5, 0.0, false);
            info.status = "UNMATCHED".into();
            info.associate_trades.clear();
            let pending = reconcile_order(&pool, &order, &info, &[], &[])
                .await
                .unwrap();
            assert_eq!(pending_orders(&pool).await.unwrap().len(), 1);
            assert_eq!(
                pending.status,
                if phase == "exit" { "CLOSING" } else { "OPEN" }
            );
            assert_eq!(pending.shares, if phase == "exit" { 5.0 } else { 0.0 });
            info.status = "CANCELED".into();
            let cancelled = reconcile_order(&pool, &order, &info, &[], &[])
                .await
                .unwrap();
            assert!(pending_orders(&pool).await.unwrap().is_empty());
            assert_eq!(
                cancelled.status,
                if phase == "exit" { "OPEN" } else { "CLOSED" }
            );
            assert_eq!(cancelled.shares, pending.shares);
        }
    }

    #[tokio::test]
    async fn uncertain_full_exit_recovers_without_resubmission_and_profit_is_net_of_actual_fees() {
        let (pool, id, auth) = fixture().await;
        let entry = intent(&pool, id, &auth, "entry", 1, 0.6, 5.0).await;
        let (i, h, r) = evidence(&entry, 5.0, 0.5, 0.05, true);
        reconcile_order(&pool, &entry, &i, &h, &r).await.unwrap();
        let exit = intent(&pool, id, &auth, "exit", 2, 0.55, 5.0).await;
        assert_eq!(
            super::super::load_trade_by_id(&pool, id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "CLOSING"
        );
        assert_eq!(
            super::super::clear_closed_trades(&pool, "OPEN")
                .await
                .unwrap(),
            0
        );
        let recovered = pending_orders(&pool).await.unwrap().pop().unwrap();
        let (i, h, r) = evidence(&recovered, 5.0, 0.6, 0.07, true);
        let closed = reconcile_order(&pool, &recovered, &i, &h, &r)
            .await
            .unwrap();
        assert_eq!(closed.status, "CLOSED");
        assert!((closed.pnl.unwrap() - 0.38).abs() < 1e-9);
        let duplicate = reconcile_order(&pool, &exit, &i, &h, &r).await.unwrap();
        assert_eq!(duplicate.pnl, closed.pnl);
        // The settlement ledger and its referenced trade survive history clear.
        assert_eq!(
            super::super::clear_closed_trades(&pool, "OPEN")
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            super::super::critical_table_counts(&pool)
                .await
                .unwrap()
                .live_matches,
            2
        );
    }

    #[tokio::test]
    async fn partial_exit_does_not_claim_flat_and_bad_evidence_rolls_back() {
        let (pool, id, auth) = fixture().await;
        let entry = intent(&pool, id, &auth, "entry", 1, 0.6, 5.0).await;
        let (i, h, r) = evidence(&entry, 5.0, 0.5, 0.05, true);
        reconcile_order(&pool, &entry, &i, &h, &r).await.unwrap();
        let exit = intent(&pool, id, &auth, "exit", 2, 0.55, 5.0).await;
        let (mut i, h, mut r) = evidence(&exit, 2.0, 0.6, 0.02, true);
        i.status = "CANCELED".into();
        let mut bad = r[0].clone();
        bad.log_index = 2;
        bad.order_id = "other".into();
        r.push(bad);
        assert!(reconcile_order(&pool, &exit, &i, &h, &r).await.is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_matches WHERE order_id=?")
            .bind(&exit.order_id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        r.pop();
        let remaining = reconcile_order(&pool, &exit, &i, &h, &r).await.unwrap();
        assert_eq!(remaining.status, "OPEN");
        assert_eq!(remaining.shares, 3.0);
        assert!((remaining.amount - 1.5).abs() < 1e-9);
        assert!((remaining.pnl.unwrap() - 0.16).abs() < 1e-9);
        let daily = super::super::risk::daily_realized_pnl(&pool, Utc::now().date_naive())
            .await
            .unwrap();
        assert!(daily.evidence_complete);
        assert!((daily.exact_today_pnl.unwrap() - 0.16).abs() < 1e-9);
        reconcile_order(&pool, &exit, &i, &h, &r).await.unwrap();
        let events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM realized_pnl_events WHERE trade_id=?")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(events, 1);
    }

    #[tokio::test]
    async fn legacy_buy_share_fee_reduces_owned_inventory_without_double_charging_cash() {
        let (pool, id, auth) = fixture().await;
        let signed = auth
            .sign_order_v1("123", 2_500_000, 5_000_000, 0, 1, 0, 0, 25, false)
            .unwrap();
        prepare_order(
            &pool,
            id,
            &auth.address(),
            &signed,
            0.5,
            MarketFeeParameters {
                rate: 0.07,
                exponent: 1.0,
                taker_only: true,
            },
            "entry",
        )
        .await
        .unwrap();
        let order = pending_orders(&pool).await.unwrap().pop().unwrap();
        let (i, h, mut r) = evidence(&order, 5.0, 0.5, 0.0, true);
        r[0].fee_shares = 0.1;
        let trade = reconcile_order(&pool, &order, &i, &h, &r).await.unwrap();
        assert!((trade.shares - 4.9).abs() < 1e-9);
        assert!((trade.amount - 2.5).abs() < 1e-9);
        assert!((trade.fee_paid - 0.05).abs() < 1e-9);
        assert!((trade.entry_price - 2.5 / 4.9).abs() < 1e-9);
    }
}

pub(super) async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS live_orders (order_id TEXT PRIMARY KEY,
        trade_id INTEGER NOT NULL, account TEXT NOT NULL, token_id TEXT NOT NULL,
        phase TEXT NOT NULL CHECK(phase IN ('entry','exit')), price REAL NOT NULL,
        size REAL NOT NULL, fee_rate REAL NOT NULL, fee_exponent REAL NOT NULL,
        taker_only INTEGER NOT NULL, exchange_address TEXT NOT NULL, protocol INTEGER NOT NULL, state TEXT NOT NULL DEFAULT 'pending',
        created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_live_orders_pending
        ON live_orders(trade_id) WHERE state='pending'",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_live_orders_terminal_audit ON live_orders(state,order_id)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS live_matches (
        order_id TEXT NOT NULL, match_id TEXT NOT NULL, size REAL NOT NULL,
        price REAL NOT NULL, fee REAL NOT NULL, fee_shares REAL NOT NULL, PRIMARY KEY(order_id, match_id))",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn prepare_order(
    pool: &SqlitePool,
    trade_id: i64,
    account: &str,
    signed: &SignedOrder,
    price: f64,
    fees: MarketFeeParameters,
    phase: &str,
) -> Result<()> {
    if !matches!(phase, "entry" | "exit") || signed.side != u8::from(phase == "exit") {
        bail!("order side does not match its accounting phase");
    }
    let size = (if phase == "entry" {
        &signed.taker_amount
    } else {
        &signed.maker_amount
    })
    .parse::<u128>()? as f64
        / 1_000_000.0;
    fees.fee(size, price).map_err(|e| anyhow!(e))?;
    if account.is_empty() || signed.order_id().is_empty() {
        bail!("missing order identity");
    }
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let audit_uncertain: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM live_orders WHERE state='audit_uncertain'")
            .fetch_one(&mut *tx)
            .await?;
    ensure!(
        audit_uncertain == 0,
        "new taker submissions require terminal evidence revalidation"
    );
    if phase == "entry" {
        let maker_pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM maker_orders WHERE state NOT IN ('TERMINAL','REJECTED')",
        )
        .fetch_one(&mut *tx)
        .await?;
        ensure!(
            maker_pending == 0,
            "taker entries wait for all maker reservations and settlement to clear"
        );
    }
    let trade: Trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
        .bind(trade_id)
        .fetch_one(&mut *tx)
        .await?;
    if !trade.is_live()
        || !trade.has_unsettled_exposure()
        || trade.token_id.as_deref() != Some(signed.token_id.as_str())
        || (phase == "entry" && trade.shares > 1e-9)
        || (phase == "exit" && (size > trade.shares + 1e-8 || trade.accounting_version != 1))
    {
        bail!("order is inconsistent with persisted live position");
    }
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO live_orders (order_id,trade_id,account,token_id,phase,
        price,size,fee_rate,fee_exponent,taker_only,exchange_address,protocol,created_at,updated_at)
        VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
    )
    .bind(signed.order_id())
    .bind(trade_id)
    .bind(account.to_ascii_lowercase())
    .bind(&signed.token_id)
    .bind(phase)
    .bind(price)
    .bind(size)
    .bind(fees.rate)
    .bind(fees.exponent)
    .bind(fees.taker_only)
    .bind(signed.exchange_address())
    .bind(match signed.protocol {
        OrderProtocol::V1 => 1,
        OrderProtocol::V2 => 2,
        OrderProtocol::V3 => 3,
    })
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    if phase == "exit" {
        sqlx::query("UPDATE trades SET status='CLOSING' WHERE id=?")
            .bind(trade_id)
            .execute(&mut *tx)
            .await?;
    } else {
        sqlx::query("UPDATE trades SET order_id=?,accounting_version=1 WHERE id=?")
            .bind(signed.order_id())
            .bind(trade_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn pending_orders(pool: &SqlitePool) -> Result<Vec<LiveOrder>> {
    let rows = sqlx::query("SELECT * FROM live_orders WHERE state IN ('pending','audit_uncertain') ORDER BY created_at,order_id")
        .fetch_all(pool)
        .await?;
    rows.iter().map(order_from_row).collect()
}

fn order_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<LiveOrder> {
    Ok(LiveOrder {
        order_id: r.try_get("order_id")?,
        trade_id: r.try_get("trade_id")?,
        account: r.try_get("account")?,
        token_id: r.try_get("token_id")?,
        phase: r.try_get("phase")?,
        price: r.try_get("price")?,
        size: r.try_get("size")?,
        identity: SettlementOrderIdentity {
            order_id: r.try_get("order_id")?,
            exchange_address: r.try_get("exchange_address")?,
            maker_address: r.try_get("account")?,
            asset_id: r.try_get("token_id")?,
            side: if r.try_get::<String, _>("phase")? == "entry" {
                Side::Buy
            } else {
                Side::Sell
            },
            protocol: match r.try_get::<i64, _>("protocol")? {
                1 => OrderProtocol::V1,
                2 => OrderProtocol::V2,
                3 => OrderProtocol::V3,
                _ => return Err(anyhow!("unknown persisted exchange protocol")),
            },
        },
    })
}

pub(crate) fn terminal_history_key(order_id: &str) -> String {
    format!("live_taker_terminal_history:{order_id}")
}

pub(crate) fn audit_reason_key(order_id: &str) -> String {
    format!("live_taker_audit_reason:{order_id}")
}

/// Canonical evidence for this taker ID, independent of page ordering and
/// unrelated trades. Record this in the same commit as terminal accounting.
pub(crate) fn canonical_terminal_history(
    order: &LiveOrder,
    history: &[VenueTrade],
) -> Result<String> {
    let side = if order.phase == "entry" {
        "BUY"
    } else {
        "SELL"
    };
    let mut rows = std::collections::BTreeMap::new();
    for matched in history
        .iter()
        .filter(|matched| matched.taker_order_id == order.order_id)
    {
        matched.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            matched.asset_id == order.token_id
                && matched.side == side
                && matched.trader_side == "TAKER"
                && matched.maker_address.eq_ignore_ascii_case(&order.account)
                && matched.size.is_finite()
                && matched.price.is_finite()
                && matched.fee_rate_bps.is_none_or(|fee| fee.is_finite())
                && (matched.is_confirmed() || matched.is_failed()),
            "terminal taker history has changed identity or unsettled/invalid evidence"
        );
        ensure!(
            (order.phase != "entry" || matched.price <= order.price + 1e-6)
                && (order.phase != "exit" || matched.price >= order.price - 1e-6),
            "terminal taker history violates the signed limit"
        );
        let evidence = json!({"id":matched.id,"market":matched.market,
            "status":if matched.is_confirmed(){"CONFIRMED"}else{"FAILED"},
            "transaction":matched.transaction_hash.as_deref().map(str::to_ascii_lowercase),
            "match_time":matched.match_time,"size":matched.size,"price":matched.price,
            "fee_rate_bps":matched.fee_rate_bps});
        if let Some(prior) = rows.insert(&matched.id, evidence.clone()) {
            ensure!(
                prior == evidence,
                "terminal taker history repeats an ID with conflicting evidence"
            );
        }
        ensure!(
            rows.len() <= 4096,
            "terminal taker history exceeds its evidence bound"
        );
    }
    let result = serde_json::to_string(&rows.into_values().collect::<Vec<_>>())?;
    ensure!(
        result.len() <= 262_144,
        "terminal taker history exceeds its byte bound"
    );
    Ok(result)
}

pub(crate) async fn audit_uncertain_count(pool: &SqlitePool) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM live_orders WHERE state='audit_uncertain'")
            .fetch_one(pool)
            .await?,
    )
}

pub(crate) async fn terminal_orders_batch(
    pool: &SqlitePool,
    after: Option<&str>,
    limit: i64,
) -> Result<Vec<LiveOrder>> {
    ensure!(
        (1..=64).contains(&limit),
        "invalid terminal taker audit batch bound"
    );
    let rows = sqlx::query(
        "SELECT * FROM live_orders WHERE state='settled' AND order_id>? ORDER BY order_id LIMIT ?",
    )
    .bind(after.unwrap_or(""))
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.iter().map(order_from_row).collect()
}

pub(crate) type TerminalMatch = (String, f64, f64, f64, f64);

pub(crate) struct TerminalAuditSnapshot {
    pub order: LiveOrder,
    pub state: String,
    pub history: Option<String>,
    pub matches: Vec<TerminalMatch>,
}

/// One local snapshot, so receipt comparisons never mix different ledger views.
pub(crate) async fn terminal_audit_snapshot(
    pool: &SqlitePool,
    order_id: &str,
) -> Result<TerminalAuditSnapshot> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query("SELECT * FROM live_orders WHERE order_id=?")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
    let order = order_from_row(&row)?;
    let state = row.try_get("state")?;
    let history = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(terminal_history_key(order_id))
        .fetch_optional(&mut *tx)
        .await?;
    let matches = sqlx::query_as("SELECT match_id,size,price,fee,fee_shares FROM live_matches WHERE order_id=? ORDER BY match_id LIMIT 4097")
        .bind(order_id).fetch_all(&mut *tx).await?;
    ensure!(
        matches.len() <= 4096,
        "terminal taker receipt evidence exceeds its bound"
    );
    ensure!(
        history
            .as_ref()
            .is_none_or(|value: &String| value.len() <= 262_144),
        "terminal taker history exceeds its byte bound"
    );
    tx.commit().await?;
    Ok(TerminalAuditSnapshot {
        order,
        state,
        history,
        matches,
    })
}

pub(crate) async fn record_verified_terminal_baseline(
    pool: &SqlitePool,
    order_id: &str,
    history: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let state: String = sqlx::query_scalar("SELECT state FROM live_orders WHERE order_id=?")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        state == "settled",
        "taker baseline requires a settled intent"
    );
    sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING")
        .bind(terminal_history_key(order_id))
        .bind(history)
        .execute(&mut *tx)
        .await?;
    let original: String = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(terminal_history_key(order_id))
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        original == history,
        "terminal taker authenticated history changed after completion"
    );
    tx.commit().await?;
    Ok(())
}

/// A stop is durable but does not rewrite any accounted lot, match or fill.
pub(crate) async fn mark_audit_uncertain(
    pool: &SqlitePool,
    order_id: &str,
    category: &str,
) -> Result<()> {
    ensure!(
        matches!(category, "evidence_changed" | "observation_unavailable"),
        "invalid taker audit category"
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let changed = sqlx::query("UPDATE live_orders SET state='audit_uncertain',updated_at=? WHERE order_id=? AND state IN ('settled','audit_uncertain')")
        .bind(Utc::now().to_rfc3339()).bind(order_id).execute(&mut *tx).await?;
    ensure!(
        changed.rows_affected() == 1,
        "terminal taker audit intent is absent or no longer settled"
    );
    let reason = json!({"category":category,"order_id":order_id,"at":Utc::now().to_rfc3339(),
        "detail":"Original terminal history and finalized receipts require operator revalidation; accounted inventory retained."}).to_string();
    sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING")
        .bind(audit_reason_key(order_id))
        .bind(reason)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn restore_verified_terminal_order(
    pool: &SqlitePool,
    order_id: &str,
    baseline: &str,
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let original: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(terminal_history_key(order_id))
        .fetch_optional(&mut *tx)
        .await?;
    ensure!(
        original.as_deref() == Some(baseline),
        "verified original taker baseline is required"
    );
    let changed = sqlx::query("UPDATE live_orders SET state='settled',updated_at=? WHERE order_id=? AND state='audit_uncertain'")
        .bind(Utc::now().to_rfc3339()).bind(order_id).execute(&mut *tx).await?;
    ensure!(
        changed.rows_affected() == 1,
        "taker order is not audit-uncertain"
    );
    sqlx::query("DELETE FROM meta WHERE key=?")
        .bind(audit_reason_key(order_id))
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO order_outbox(timestamp,stage,market_id,order_id,token_id,action,status,details) SELECT ?,'operator_recovery',trades.market_id,live_orders.order_id,live_orders.token_id,'taker_terminal_recheck','verified','Original terminal history and receipts revalidated; inventory and risk stops retained' FROM live_orders JOIN trades ON trades.id=live_orders.trade_id WHERE live_orders.order_id=?")
        .bind(Utc::now().to_rfc3339()).bind(order_id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

/// Explicit refusal is proof of no fill only before any match is recorded.
pub async fn reject_order(pool: &SqlitePool, order_id: &str) -> Result<Trade> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let (id, phase, state): (i64, String, String) =
        sqlx::query_as("SELECT trade_id,phase,state FROM live_orders WHERE order_id=?")
            .bind(order_id)
            .fetch_one(&mut *tx)
            .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_matches WHERE order_id=?")
        .bind(order_id)
        .fetch_one(&mut *tx)
        .await?;
    if state != "pending" || count != 0 {
        bail!("cannot reject an accounted or settled order");
    }
    if phase == "entry" {
        sqlx::query(
            "UPDATE trades SET status='CLOSED',amount=0,shares=0,entry_cost=0,
            entry_shares=0,order_id=NULL,pnl=0,exit_type='rejected',exit_timestamp=? WHERE id=?",
        )
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query("UPDATE trades SET status='OPEN' WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE live_orders SET state='rejected',updated_at=? WHERE order_id=?")
        .bind(Utc::now().to_rfc3339())
        .bind(order_id)
        .execute(&mut *tx)
        .await?;
    let trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(trade)
}

/// Atomically consume confirmed exchange matches exactly once. Incomplete
/// settlement leaves the intent pending and cannot unlock another submission.
pub async fn reconcile_order(
    pool: &SqlitePool,
    order: &LiveOrder,
    info: &OrderInfo,
    history: &[VenueTrade],
    settlements: &[SettledOrderFill],
) -> Result<Trade> {
    if info.id.as_deref() != Some(order.order_id.as_str())
        || info.asset_id.as_deref() != Some(order.token_id.as_str())
        || info.side.as_deref()
            != Some(if order.phase == "entry" {
                "BUY"
            } else {
                "SELL"
            })
        || info
            .original_size
            .is_none_or(|n| (n - order.size).abs() > 1e-6)
        || info.size_matched > order.size + 1e-6
    {
        bail!("exchange order identity/size does not match durable intent");
    }
    let relevant: Vec<_> = history
        .iter()
        .filter(|t| t.taker_order_id == order.order_id)
        .collect();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let state: String = sqlx::query_scalar("SELECT state FROM live_orders WHERE order_id=?")
        .bind(&order.order_id)
        .fetch_one(&mut *tx)
        .await?;
    ensure!(
        state != "audit_uncertain",
        "terminal taker evidence requires explicit operator revalidation"
    );
    let expected_side = if order.phase == "entry" {
        "BUY"
    } else {
        "SELL"
    };
    for m in &relevant {
        m.validate().map_err(|e| anyhow!(e))?;
        if m.asset_id != order.token_id
            || m.side != expected_side
            || m.trader_side != "TAKER"
            || !m.maker_address.eq_ignore_ascii_case(&order.account)
            || (order.phase == "entry" && m.price > order.price + 1e-6)
            || (order.phase == "exit" && m.price < order.price - 1e-6)
        {
            bail!("match does not belong to the intended account/asset/side/limit");
        }
    }
    let confirmed_transactions: std::collections::HashSet<_> = relevant
        .iter()
        .filter(|m| m.is_confirmed())
        .filter_map(|m| m.transaction_hash.as_deref())
        .map(str::to_ascii_lowercase)
        .collect();
    for fill in settlements {
        if fill.order_id != order.order_id
            || !confirmed_transactions.contains(&fill.transaction_hash.to_ascii_lowercase())
            || !fill.gross_shares.is_finite()
            || fill.gross_shares <= 0.0
            || !fill.gross_collateral.is_finite()
            || fill.gross_collateral <= 0.0
            || !fill.fee_collateral.is_finite()
            || fill.fee_collateral < 0.0
            || !fill.fee_shares.is_finite()
            || fill.fee_shares < 0.0
            || fill.fee_shares >= fill.gross_shares
        {
            bail!("invalid or unassociated finalized settlement evidence");
        }
        let price = fill.gross_collateral / fill.gross_shares;
        if !price.is_finite()
            || price > 1.0
            || (order.phase == "entry" && price > order.price + 1e-6)
            || (order.phase == "exit" && price < order.price - 1e-6)
        {
            bail!("settled execution violates signed limit");
        }
        let evidence_id = format!(
            "{}:{}",
            fill.transaction_hash.to_ascii_lowercase(),
            fill.log_index
        );
        let prior: Option<(f64, f64, f64, f64)> = sqlx::query_as(
            "SELECT size,price,fee,fee_shares FROM live_matches WHERE order_id=? AND match_id=?",
        )
        .bind(&order.order_id)
        .bind(&evidence_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(prior) = prior {
            if prior
                != (
                    fill.gross_shares,
                    price,
                    fill.fee_collateral,
                    fill.fee_shares,
                )
            {
                bail!("finalized settlement changed after accounting");
            }
        } else {
            if state != "pending" {
                bail!("new settlement evidence appeared after terminal accounting");
            }
            sqlx::query("INSERT INTO live_matches(order_id,match_id,size,price,fee,fee_shares) VALUES(?,?,?,?,?,?)")
                .bind(&order.order_id).bind(evidence_id).bind(fill.gross_shares).bind(price)
                .bind(fill.fee_collateral).bind(fill.fee_shares).execute(&mut *tx).await?;
        }
    }
    let (qty, gross, fee, fee_shares): (f64, f64, f64, f64) = sqlx::query_as(
        "SELECT COALESCE(SUM(size),0.0),COALESCE(SUM(size*price),0.0),COALESCE(SUM(fee),0.0),
         COALESCE(SUM(fee_shares),0.0) FROM live_matches WHERE order_id=?",
    )
    .bind(&order.order_id)
    .fetch_one(&mut *tx)
    .await?;
    let net_qty = qty - fee_shares;
    if qty > order.size + 1e-6 || qty > info.size_matched + 1e-6 {
        bail!("confirmed quantity exceeds venue/order totals");
    }
    let terminal = matches!(
        info.status.to_ascii_uppercase().as_str(),
        "MATCHED" | "FILLED" | "CANCELED" | "CANCELLED" | "EXPIRED"
    );
    let complete = terminal
        && (qty - info.size_matched).abs() <= 1e-6
        && relevant.iter().all(|t| t.is_confirmed() || t.is_failed())
        && info
            .associate_trades
            .iter()
            .all(|id| relevant.iter().any(|t| &t.id == id));
    if state != "pending" {
        if !complete {
            bail!("terminal order evidence no longer agrees with recorded settlement");
        }
        let history = canonical_terminal_history(order, history)?;
        let original: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
            .bind(terminal_history_key(&order.order_id))
            .fetch_optional(&mut *tx)
            .await?;
        ensure!(
            original.is_none_or(|original| original == history),
            "terminal taker authenticated history changed after completion"
        );
        let trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
            .bind(order.trade_id)
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(trade);
    }
    if order.phase == "entry" {
        if qty > 0.0 {
            sqlx::query(
                "UPDATE trades SET entry_price=?,shares=?,entry_shares=?,entry_cost=?,
                fee_paid=?,amount=?,accounting_version=1,order_id=CASE WHEN ? THEN NULL ELSE order_id END WHERE id=?",
            )
            .bind(gross / net_qty)
            .bind(net_qty)
            .bind(net_qty)
            .bind(gross)
            .bind(fee + fee_shares* gross/qty)
            .bind(if complete {
                gross
            } else {
                order.size * order.price
            })
            .bind(complete)
            .bind(order.trade_id)
            .execute(&mut *tx)
            .await?;
        } else if complete {
            sqlx::query("UPDATE trades SET status='CLOSED',amount=0,shares=0,entry_cost=0,
                entry_shares=0,order_id=NULL,pnl=0,exit_type='unfilled',exit_timestamp=? WHERE id=?")
                .bind(Utc::now().to_rfc3339()).bind(order.trade_id).execute(&mut *tx).await?;
        }
    } else if complete {
        let recognition_time = Utc::now().to_rfc3339();
        let trade: Trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
            .bind(order.trade_id)
            .fetch_one(&mut *tx)
            .await?;
        if qty > trade.shares + 1e-6 {
            bail!("exit exceeds remaining local inventory");
        }
        let (entry_qty, entry_cost): (f64, f64) =
            sqlx::query_as("SELECT entry_shares,entry_cost FROM trades WHERE id=?")
                .bind(order.trade_id)
                .fetch_one(&mut *tx)
                .await?;
        if entry_qty <= 0.0 {
            bail!("missing live entry cost");
        }
        let remaining = (trade.shares - qty).max(0.0);
        let entry_cash_fee:f64=sqlx::query_scalar("SELECT COALESCE((SELECT SUM(m.fee) FROM live_matches m JOIN live_orders o ON o.order_id=m.order_id
            WHERE o.trade_id=? AND o.phase='entry'),0.0) +
            COALESCE((SELECT SUM(fee_usd) FROM maker_match_allocations WHERE trade_id=? AND phase='entry'),0.0)")
            .bind(order.trade_id).bind(order.trade_id).fetch_one(&mut *tx).await?;
        let pnl = trade.pnl.unwrap_or(0.0) + gross
            - entry_cost * qty / entry_qty
            - fee
            - entry_cash_fee * qty / entry_qty;
        let closed = remaining <= 1e-6;
        sqlx::query(
            "UPDATE trades SET status=?,shares=?,amount=?,pnl=?,exit_price=?,
            exit_timestamp=?,exit_type=?,was_correct=?,partial_closed=?,accounting_version=1,order_id=NULL WHERE id=?",
        )
        .bind(if closed { "CLOSED" } else { "OPEN" })
        .bind(if closed { trade.shares } else { remaining })
        .bind(if closed {
            trade.amount
        } else {
            entry_cost * remaining / entry_qty
        })
        .bind(pnl)
        .bind(if qty > 0.0 { Some(gross / qty) } else { None })
        .bind(if closed {
            Some(recognition_time.clone())
        } else {
            None
        })
        .bind(if closed {
            Some("venue_confirmed")
        } else {
            None
        })
        .bind(if closed {
            Some(i64::from(pnl > 0.0))
        } else {
            None
        })
        .bind(i64::from(!closed || trade.partial_closed == Some(1)))
        .bind(order.trade_id)
        .execute(&mut *tx)
        .await?;
        if qty > 0.0 {
            super::risk::record_realized_delta(
                &mut tx,
                order.trade_id,
                &format!("live/{}", order.order_id),
                &recognition_time,
                pnl - trade.pnl.unwrap_or(0.0),
                qty,
            )
            .await?;
        }
    }
    if complete {
        let history = canonical_terminal_history(order, history)?;
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING")
            .bind(terminal_history_key(&order.order_id))
            .bind(&history)
            .execute(&mut *tx)
            .await?;
        let original: String = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
            .bind(terminal_history_key(&order.order_id))
            .fetch_one(&mut *tx)
            .await?;
        ensure!(
            original == history,
            "terminal taker baseline conflicts with accounted evidence"
        );
        sqlx::query("UPDATE live_orders SET state='settled',updated_at=? WHERE order_id=?")
            .bind(Utc::now().to_rfc3339())
            .bind(&order.order_id)
            .execute(&mut *tx)
            .await?;
    }
    // The public fill ledger gets cumulative VWAP evidence in the SAME commit.
    if qty > 0.0 {
        let prev: f64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(cumulative_size),0.0) FROM trade_fills
            WHERE trade_id=? AND order_id=? AND phase=?",
        )
        .bind(order.trade_id)
        .bind(&order.order_id)
        .bind(&order.phase)
        .fetch_one(&mut *tx)
        .await?;
        if qty > prev + 1e-9 {
            let prior_gross: f64 = sqlx::query_scalar(
                "SELECT COALESCE(SUM(delta_size*delta_price),0.0)
                FROM trade_fills WHERE trade_id=? AND order_id=? AND phase=?",
            )
            .bind(order.trade_id)
            .bind(&order.order_id)
            .bind(&order.phase)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO trade_fills(timestamp,trade_id,market_id,order_id,phase,
                intended_price,avg_price,cumulative_size,delta_size,delta_price,source,details)
                SELECT ?,id,market_id,?,?,?,?,?,?,?,'confirmed_venue_matches',? FROM trades WHERE id=?")
                .bind(Utc::now().to_rfc3339()).bind(&order.order_id).bind(&order.phase)
                .bind(order.price).bind(gross/qty).bind(qty).bind(qty-prev)
                .bind((gross-prior_gross)/(qty-prev))
                .bind(format!("finalized exchange receipts; actual collateral fee={fee:.5}, share fee={fee_shares:.6}"))
                .bind(order.trade_id).execute(&mut *tx).await?;
        }
    }
    let trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
        .bind(order.trade_id)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(trade)
}
