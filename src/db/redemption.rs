//! Atomic local accounting for a receipt verified by the read-only adapter.
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use std::collections::BTreeSet;

use super::Trade;
use crate::clob::{
    redemption::{VerifiedRedemption, MAX_EXACT_UNITS},
    settlement::SettlementOrderIdentity,
};

#[path = "redemption/entry.rs"]
mod entry;
pub(crate) use entry::{entry_receipt_orders, verify_entry_fills};

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS redemption_receipts (
        transaction_hash TEXT PRIMARY KEY, account TEXT NOT NULL, condition_id TEXT NOT NULL,
        block_number INTEGER NOT NULL, block_hash TEXT NOT NULL, timestamp TEXT NOT NULL,
        proof_json TEXT NOT NULL, trade_ids_json TEXT NOT NULL, result_json TEXT NOT NULL)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Local immutable evidence, explicitly separate from a fresh chain recheck.
pub async fn evidence_for_trade(pool: &SqlitePool, trade_id: i64) -> Result<Option<Value>> {
    let mut tx = pool.begin().await?;
    let trade: Trade = sqlx::query_as("SELECT * FROM trades WHERE id=?")
        .bind(trade_id)
        .fetch_one(&mut *tx)
        .await?;
    let rows:Vec<(String,String)>=sqlx::query_as("SELECT proof_json,result_json FROM redemption_receipts r WHERE EXISTS(SELECT 1 FROM json_each(r.trade_ids_json) j WHERE j.value=?) LIMIT 2")
        .bind(trade_id).fetch_all(&mut *tx).await?;
    if rows.is_empty() {
        tx.commit().await?;
        return Ok(None);
    }
    ensure!(
        rows.len() == 1,
        "multiple redemption receipts reference one managed lot"
    );
    let proof: VerifiedRedemption = serde_json::from_str(&rows[0].0)?;
    let result: Value = serde_json::from_str(&rows[0].1)?;
    let outcomes = result["trades"]
        .as_array()
        .context("missing redemption lot outcomes")?;
    let outcome = outcomes
        .iter()
        .find(|v| v["id"].as_i64() == Some(trade_id))
        .context("missing redeemed trade outcome")?;
    let shares = outcome["shares_redeemed"]
        .as_f64()
        .context("missing redeemed quantity")?;
    let cash = outcome["collateral_received"]
        .as_f64()
        .context("missing redeemed collateral")?;
    let pnl = outcome["total_pnl"]
        .as_f64()
        .context("missing redeemed profit")?;
    let delta = outcome["realized_pnl_delta"]
        .as_f64()
        .context("missing redeemed profit delta")?;
    ensure!(
        [shares, cash, pnl, delta].iter().all(|v| v.is_finite())
            && shares > 0.0
            && cash >= 0.0
            && trade.status == "CLOSED"
            && trade.is_live()
            && trade.exit_type.as_deref() == Some("redemption_confirmed")
            && trade.exit_timestamp.as_deref() == Some(proof.timestamp.as_str())
            && (trade.shares - shares).abs() <= 1e-7
            && trade.pnl.is_some_and(|v| (v - pnl).abs() <= 1e-7)
            && trade
                .exit_price
                .is_some_and(|v| (v - cash / shares).abs() <= 1e-7),
        "redeemed lot contradicts its immutable accounting outcome"
    );
    let event_key = format!(
        "redemption/{}:{}",
        proof.transaction_hash, proof.redemption_log_index
    );
    let event:(String,f64,f64)=sqlx::query_as("SELECT timestamp,net_pnl,realized_shares FROM realized_pnl_events WHERE trade_id=? AND event_key=?")
        .bind(trade_id).bind(event_key).fetch_one(&mut *tx).await?;
    ensure!(
        event.0 == proof.timestamp
            && (event.1 - delta).abs() <= 1e-7
            && (event.2 - shares).abs() <= 1e-7,
        "redemption dated event contradicts its receipt outcome"
    );
    tx.commit().await?;
    Ok(Some(
        json!({"status":"recorded_finalized_receipt","fresh_chain_recheck":false,
        "transaction_hash":proof.transaction_hash,"condition_id":proof.condition_id,"account":proof.account,
        "block_hash":proof.block_hash,"block_number":proof.block_number,"timestamp":proof.timestamp,
        "trade":outcome,"pnl_basis":result["pnl_basis"],"proof":proof.evidence}),
    ))
}

fn micro_units(shares: f64) -> Result<u64> {
    ensure!(
        shares.is_finite() && shares >= 0.0,
        "invalid local redemption quantity"
    );
    let units = shares * 1_000_000.0;
    ensure!(
        units <= MAX_EXACT_UNITS as f64 && (units - units.round()).abs() <= 1e-4,
        "local quantity cannot be represented by exact six-decimal token units"
    );
    Ok(units.round() as u64)
}

fn receipt_transaction(match_id: &str) -> Result<String> {
    let (hash, index) = match_id
        .rsplit_once(':')
        .context("entry evidence lacks a finalized transaction/log identity")?;
    ensure!(
        hash.starts_with("0x")
            && hash.len() == 66
            && hash[2..].bytes().all(|b| b.is_ascii_hexdigit())
            && !index.is_empty()
            && index.bytes().all(|b| b.is_ascii_digit())
            && index.parse::<u64>().is_ok(),
        "entry evidence lacks a finalized transaction/log identity"
    );
    Ok(hash.to_ascii_lowercase())
}

pub(crate) async fn entry_transactions(
    pool: &SqlitePool,
    trade_ids: &[i64],
) -> Result<Vec<String>> {
    ensure!(
        !trade_ids.is_empty() && trade_ids.len() <= 64,
        "invalid redemption trade selection"
    );
    let mut tx = pool.begin().await?;
    let mut hashes = BTreeSet::new();
    for id in trade_ids {
        let matches:Vec<String>=sqlx::query_scalar("SELECT m.match_id FROM live_matches m JOIN live_orders o ON o.order_id=m.order_id WHERE o.trade_id=? AND o.phase='entry' UNION SELECT a.match_id FROM maker_match_allocations a WHERE a.trade_id=? AND a.phase='entry' LIMIT 257")
            .bind(id).bind(id).fetch_all(&mut *tx).await?;
        ensure!(
            !matches.is_empty() && matches.len() <= 256,
            "missing or oversized original entry receipt history"
        );
        for id in matches {
            hashes.insert(receipt_transaction(&id)?);
        }
    }
    ensure!(
        hashes.len() <= 128,
        "too many entry transactions for bounded redemption reconciliation"
    );
    tx.commit().await?;
    Ok(hashes.into_iter().collect())
}

/// All lot mutations, the full immutable receipt baseline, and dated risk
/// events commit together. The same transaction can never credit cash twice.
pub(crate) async fn apply(
    pool: &SqlitePool,
    proof: &VerifiedRedemption,
    trade_ids: &[i64],
) -> Result<Value> {
    let mut ids = trade_ids.to_vec();
    ids.sort_unstable();
    ensure!(
        !ids.is_empty()
            && ids.len() <= 64
            && ids.iter().all(|id| *id > 0)
            && ids.windows(2).all(|w| w[0] != w[1]),
        "invalid redemption trade selection"
    );
    let proof_json = serde_json::to_string(proof)?;
    let ids_json = serde_json::to_string(&ids)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let prior:Option<(String,String,String)>=sqlx::query_as(
        "SELECT proof_json,trade_ids_json,result_json FROM redemption_receipts WHERE transaction_hash=?")
        .bind(&proof.transaction_hash).fetch_optional(&mut *tx).await?;
    if let Some((prior_proof, prior_ids, result)) = prior {
        ensure!(
            prior_proof == proof_json && prior_ids == ids_json,
            "redemption replay has contradictory immutable evidence or trade selection"
        );
        tx.commit().await?;
        let mut result: Value = serde_json::from_str(&result)?;
        result["replayed"] = json!(true);
        return Ok(result);
    }
    let unresolved: i64 = sqlx::query_scalar(
        "SELECT
        (SELECT COUNT(*) FROM live_orders WHERE state NOT IN ('settled','rejected'))+
        (SELECT COUNT(*) FROM maker_orders WHERE state NOT IN ('TERMINAL','REJECTED'))",
    )
    .fetch_one(&mut *tx)
    .await?;
    ensure!(
        unresolved == 0,
        "unresolved or audit-uncertain orders must be reconciled before redemption"
    );
    let lots:Vec<Trade>=sqlx::query_as("SELECT * FROM trades WHERE mode='live' AND status IN ('OPEN','CLOSING') AND token_id IN (?,?) ORDER BY id LIMIT 65")
        .bind(&proof.token_ids[0]).bind(&proof.token_ids[1]).fetch_all(&mut *tx).await?;
    let selected: Vec<i64> = lots.iter().map(|t| t.id).collect();
    ensure!(
        selected == ids,
        "selected trades must include every managed live lot of the redeemed tokens"
    );
    let mut units = [0_u64; 2];
    for lot in &lots {
        ensure!(
            lot.is_open() && lot.order_id.is_none() && lot.accounting_version == 1,
            "redemption needs an open lot with complete entry accounting"
        );
        let side = proof
            .token_ids
            .iter()
            .position(|id| Some(id.as_str()) == lot.token_id.as_deref())
            .context("unknown redemption token")?;
        units[side] = units[side]
            .checked_add(micro_units(lot.shares)?)
            .context("redemption quantity overflow")?;
    }
    ensure!(units==proof.burned_units,"finalized burn does not exactly cover managed remaining inventory; mixed external holdings are unsupported");
    let realized_time = DateTime::parse_from_rfc3339(&proof.timestamp)?.with_timezone(&Utc);
    let mut outcomes = Vec::new();
    let mut net_total = 0.0;
    for lot in lots {
        let side = proof
            .token_ids
            .iter()
            .position(|id| Some(id.as_str()) == lot.token_id.as_deref())
            .unwrap();
        ensure!(
            DateTime::parse_from_rfc3339(&lot.timestamp)?.with_timezone(&Utc) <= realized_time,
            "redemption predates managed entry"
        );
        let prior_times: Vec<String> = sqlx::query_scalar(
            "SELECT timestamp FROM realized_pnl_events WHERE trade_id=? LIMIT 257",
        )
        .bind(lot.id)
        .fetch_all(&mut *tx)
        .await?;
        ensure!(
            prior_times.len() <= 256,
            "oversized prior realization history"
        );
        for prior_time in prior_times {
            ensure!(
                DateTime::parse_from_rfc3339(&prior_time)?.with_timezone(&Utc) <= realized_time,
                "redemption predates an already accounted partial sale"
            );
        }
        let (entry_size, entry_cost): (f64, f64) =
            sqlx::query_as("SELECT entry_shares,entry_cost FROM trades WHERE id=?")
                .bind(lot.id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            entry_size.is_finite()
                && entry_size > 0.0
                && entry_cost.is_finite()
                && entry_cost > 0.0
                && lot.shares <= entry_size + 1e-8
                && lot.amount.is_finite()
                && (lot.amount - entry_cost * lot.shares / entry_size).abs() <= 1e-7,
            "remaining lot cost is not supported by original entry accounting"
        );
        let (prior_shares,prior_pnl):(f64,f64)=sqlx::query_as("SELECT COALESCE(SUM(realized_shares),0.0),COALESCE(SUM(net_pnl),0.0) FROM realized_pnl_events WHERE trade_id=?")
            .bind(lot.id).fetch_one(&mut *tx).await?;
        ensure!(
            prior_shares.is_finite()
                && prior_pnl.is_finite()
                && (prior_shares - (entry_size - lot.shares)).abs() <= 1e-7
                && (prior_pnl - lot.pnl.unwrap_or(0.0)).abs() <= 1e-7,
            "prior partial accounting lacks exact dated quantity and profit evidence"
        );
        let taker_entries=sqlx::query("SELECT o.account,o.token_id,o.state,m.match_id,m.size,m.price,m.fee,m.fee_shares FROM live_orders o JOIN live_matches m ON m.order_id=o.order_id WHERE o.trade_id=? AND o.phase='entry' LIMIT 257")
            .bind(lot.id).fetch_all(&mut *tx).await?;
        ensure!(taker_entries.len() <= 256, "oversized taker entry history");
        let mut proven_size = 0.0;
        let mut proven_cost = 0.0;
        let mut cash_fee = 0.0;
        for row in taker_entries {
            ensure!(
                proof
                    .entry_transaction_hashes
                    .contains(&receipt_transaction(
                        &row.try_get::<String, _>("match_id")?
                    )?),
                "entry receipt was not verified to precede redemption"
            );
            let account: String = row.try_get("account")?;
            let token: String = row.try_get("token_id")?;
            let state: String = row.try_get("state")?;
            let size: f64 = row.try_get("size")?;
            let price: f64 = row.try_get("price")?;
            let fee: f64 = row.try_get("fee")?;
            let share_fee: f64 = row.try_get("fee_shares")?;
            ensure!(
                account.eq_ignore_ascii_case(&proof.account)
                    && token == proof.token_ids[side]
                    && state == "settled"
                    && [size, price, fee, share_fee].iter().all(|v| v.is_finite())
                    && size > 0.0
                    && price > 0.0
                    && price < 1.0
                    && fee >= 0.0
                    && share_fee >= 0.0
                    && share_fee < size,
                "entry receipt ledger is inconsistent with redemption account"
            );
            proven_size += size - share_fee;
            proven_cost += size * price;
            cash_fee += fee;
        }
        let maker_entries=sqlx::query("SELECT o.order_id,o.state,o.side,o.token_id,o.settlement_json,a.match_id,m.timestamp,a.size,a.notional,a.fee_usd FROM maker_orders o JOIN maker_match_allocations a ON a.order_id=o.order_id JOIN maker_matches m ON m.order_id=a.order_id AND m.match_id=a.match_id WHERE a.trade_id=? AND a.phase='entry' LIMIT 257")
            .bind(lot.id).fetch_all(&mut *tx).await?;
        ensure!(maker_entries.len() <= 256, "oversized maker entry history");
        for row in maker_entries {
            ensure!(
                proof
                    .entry_transaction_hashes
                    .contains(&receipt_transaction(
                        &row.try_get::<String, _>("match_id")?
                    )?),
                "maker entry receipt was not verified to precede redemption"
            );
            ensure!(
                DateTime::parse_from_rfc3339(&row.try_get::<String, _>("timestamp")?)?
                    .with_timezone(&Utc)
                    <= realized_time,
                "maker entry timestamp follows redemption"
            );
            let identity: SettlementOrderIdentity =
                serde_json::from_str(&row.try_get::<String, _>("settlement_json")?)?;
            let token: String = row.try_get("token_id")?;
            let state: String = row.try_get("state")?;
            let order_side: String = row.try_get("side")?;
            let order_id: String = row.try_get("order_id")?;
            let size: f64 = row.try_get("size")?;
            let cost: f64 = row.try_get("notional")?;
            let fee: f64 = row.try_get("fee_usd")?;
            ensure!(
                identity.order_id == order_id
                    && identity.maker_address.eq_ignore_ascii_case(&proof.account)
                    && identity.asset_id == proof.token_ids[side]
                    && token == proof.token_ids[side]
                    && order_side == "BUY"
                    && identity.side == crate::clob::Side::Buy
                    && state == "TERMINAL"
                    && [size, cost, fee].iter().all(|v| v.is_finite())
                    && size > 0.0
                    && cost > 0.0
                    && fee >= 0.0,
                "maker entry evidence is inconsistent with redemption account"
            );
            proven_size += size;
            proven_cost += cost;
            cash_fee += fee;
        }
        ensure!(
            [proven_size, proven_cost, cash_fee]
                .iter()
                .all(|v| v.is_finite())
                && (proven_size - entry_size).abs() <= 1e-7
                && (proven_cost - entry_cost).abs() <= 1e-7,
            "legacy or incomplete entry receipts cannot establish redemption profit"
        );
        let quantity = micro_units(lot.shares)?;
        let payout = proof.payout_units[side] as f64 / 1_000_000.0 * quantity as f64
            / proof.burned_units[side] as f64;
        let allocated_cost = (entry_cost + cash_fee) * lot.shares / entry_size;
        let delta = payout - allocated_cost;
        let total = lot.pnl.unwrap_or(0.0) + delta;
        ensure!(
            [payout, allocated_cost, delta, total]
                .iter()
                .all(|v| v.is_finite()),
            "redemption accounting overflow"
        );
        let price = payout / lot.shares;
        let correct = if price == 1.0 {
            Some(1_i64)
        } else if price == 0.0 {
            Some(0_i64)
        } else {
            None
        };
        sqlx::query("UPDATE trades SET status='CLOSED',pnl=?,exit_price=?,exit_timestamp=?,exit_type='redemption_confirmed',was_correct=?,fok_failed=0,order_id=NULL WHERE id=? AND status='OPEN'")
            .bind(total).bind(price).bind(&proof.timestamp).bind(correct).bind(lot.id).execute(&mut *tx).await?;
        let event_key = format!(
            "redemption/{}:{}",
            proof.transaction_hash, proof.redemption_log_index
        );
        super::risk::record_realized_delta(
            &mut tx,
            lot.id,
            &event_key,
            &proof.timestamp,
            delta,
            lot.shares,
        )
        .await?;
        let details = serde_json::to_string(
            &json!({"transaction_hash":proof.transaction_hash,"block_hash":proof.block_hash,
            "block_number":proof.block_number,"token_id":proof.token_ids[side],"burned_units":quantity,
            "collateral_received":payout,"allocation_basis":"proportional within the same outcome-token burn",
            "proof":"finalized standard CTF collateral-adapter burn and pUSD payout; no orderbook sale"}),
        )?;
        sqlx::query("INSERT INTO trade_fills(timestamp,trade_id,market_id,order_id,phase,intended_price,avg_price,cumulative_size,delta_size,delta_price,source,details) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(&proof.timestamp).bind(lot.id).bind(&lot.market_id).bind(&event_key).bind("redemption")
            .bind(price).bind(price).bind(lot.shares).bind(lot.shares).bind(price)
            .bind("finalized_redemption").bind(details).execute(&mut *tx).await?;
        net_total += delta;
        outcomes.push(
            json!({"id":lot.id,"shares_redeemed":lot.shares,"collateral_received":payout,
            "realized_pnl_delta":delta,"total_pnl":total,"status":"CLOSED"}),
        );
    }
    let result = json!({"ok":true,"status":"reconciled","transaction_hash":proof.transaction_hash,
        "condition_id":proof.condition_id,"account":proof.account,"timestamp":proof.timestamp,
        "collateral_received":proof.total_payout_units as f64/1_000_000.0,"realized_pnl_delta":net_total,
        "trades":outcomes,"replayed":false,"transaction_submitted":false,
        "kill_switch_cleared":false,"realization_basis":"UTC finalized block timestamp",
        "pnl_basis":"received pUSD minus recorded entry cost and entry cash fees; gas and external redemption costs are excluded"});
    sqlx::query("INSERT INTO redemption_receipts(transaction_hash,account,condition_id,block_number,block_hash,timestamp,proof_json,trade_ids_json,result_json) VALUES(?,?,?,?,?,?,?,?,?)")
        .bind(&proof.transaction_hash).bind(&proof.account).bind(&proof.condition_id).bind(i64::try_from(proof.block_number)?)
        .bind(&proof.block_hash).bind(&proof.timestamp).bind(proof_json).bind(ids_json).bind(serde_json::to_string(&result)?)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result)
}

#[cfg(test)]
#[path = "redemption/tests.rs"]
pub(crate) mod tests;
