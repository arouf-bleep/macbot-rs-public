//! Bounded original entry identities and receipt amounts, read as one SQL snapshot.
use super::*;
use crate::clob::{settlement::SettledOrderFill, OrderProtocol, Side};
use std::collections::{BTreeMap, HashMap};

pub(crate) struct EntryReceiptOrder {
    pub identity: SettlementOrderIdentity,
    pub transactions: Vec<String>,
    matches: BTreeMap<String, RecordedFill>,
}

struct RecordedFill {
    shares: f64,
    collateral: f64,
    fee_collateral: f64,
    fee_shares: f64,
}

/// Full own-order receipt sets are compared even when a maker receipt also
/// allocated inventory to another lot. Never mistake an allocation for the
/// complete on-chain event, or silently truncate oversized evidence.
/// An audit-uncertain terminal intent may still have its amounts inspected:
/// atomic apply refuses new credit in that state, while an unchanged prior
/// redemption can be replayed without changing accounting or releasing stops.
pub(crate) async fn entry_receipt_orders(
    pool: &SqlitePool,
    trade_ids: &[i64],
    transactions: &[String],
) -> Result<Vec<EntryReceiptOrder>> {
    ensure!(
        !trade_ids.is_empty() && trade_ids.len() <= 64,
        "invalid entry selection"
    );
    ensure!(
        !transactions.is_empty() && transactions.len() <= 128,
        "invalid entry transaction set"
    );
    let selection = serde_json::to_string(trade_ids)?;
    let mut tx = pool.begin().await?;
    let takers = sqlx::query("SELECT order_id,account,token_id,exchange_address,protocol,state FROM live_orders WHERE phase='entry' AND trade_id IN (SELECT value FROM json_each(?)) ORDER BY order_id LIMIT 129")
        .bind(&selection).fetch_all(&mut *tx).await?;
    let makers = sqlx::query("SELECT DISTINCT o.order_id,o.token_id,o.side,o.state,o.settlement_json FROM maker_orders o JOIN maker_match_allocations a ON a.order_id=o.order_id WHERE a.phase='entry' AND a.trade_id IN (SELECT value FROM json_each(?)) ORDER BY o.order_id LIMIT 129")
        .bind(&selection).fetch_all(&mut *tx).await?;
    ensure!(
        takers.len() + makers.len() <= 128,
        "oversized entry order evidence"
    );
    let mut orders = Vec::new();
    let mut count = 0;
    let mut covered = BTreeSet::new();
    for (maker, row) in takers
        .into_iter()
        .map(|r| (false, r))
        .chain(makers.into_iter().map(|r| (true, r)))
    {
        let order_id: String = row.try_get("order_id")?;
        let token_id: String = row.try_get("token_id")?;
        let identity = if maker {
            ensure!(
                matches!(
                    row.try_get::<String, _>("state")?.as_str(),
                    "TERMINAL" | "AUDIT_UNCERTAIN"
                ) && row.try_get::<String, _>("side")? == "BUY",
                "maker entry order is not finalized"
            );
            serde_json::from_str::<SettlementOrderIdentity>(
                &row.try_get::<String, _>("settlement_json")?,
            )?
        } else {
            ensure!(
                matches!(
                    row.try_get::<String, _>("state")?.as_str(),
                    "settled" | "audit_uncertain"
                ),
                "taker entry order is not settled"
            );
            let protocol = match row.try_get::<i64, _>("protocol")? {
                1 => OrderProtocol::V1,
                2 => OrderProtocol::V2,
                3 => OrderProtocol::V3,
                _ => anyhow::bail!("unknown original entry protocol"),
            };
            SettlementOrderIdentity {
                order_id: order_id.clone(),
                exchange_address: row.try_get("exchange_address")?,
                maker_address: row.try_get("account")?,
                asset_id: token_id.clone(),
                side: Side::Buy,
                protocol,
            }
        };
        ensure!(
            identity.order_id == order_id
                && identity.asset_id == token_id
                && identity.side == Side::Buy,
            "original entry identity contradicts its ledger"
        );
        // Both queries have the same explicit shape; maker fees are collateral.
        let matches = if maker {
            sqlx::query("SELECT match_id,size,price,fee_usd AS fee,0.0 AS fee_shares FROM maker_matches WHERE order_id=? AND side='BUY' AND token_id=? ORDER BY match_id LIMIT 4097")
                .bind(&order_id).bind(&token_id).fetch_all(&mut *tx).await?
        } else {
            sqlx::query("SELECT match_id,size,price,fee,fee_shares FROM live_matches WHERE order_id=? ORDER BY match_id LIMIT 4097")
                .bind(&order_id).fetch_all(&mut *tx).await?
        };
        count += matches.len();
        ensure!(count <= 4096, "oversized original entry fill evidence");
        let mut recorded = BTreeMap::new();
        let mut own_transactions = BTreeSet::new();
        for row in matches {
            let id: String = row.try_get("match_id")?;
            let hash = receipt_transaction(&id)?;
            if !transactions.contains(&hash) {
                continue;
            }
            let index: u64 = id.rsplit_once(':').unwrap().1.parse()?;
            let shares: f64 = row.try_get("size")?;
            let price: f64 = row.try_get("price")?;
            let fee_collateral: f64 = row.try_get("fee")?;
            let fee_shares: f64 = row.try_get("fee_shares")?;
            let collateral = shares * price;
            ensure!(
                [shares, price, collateral, fee_collateral, fee_shares]
                    .iter()
                    .all(|v| v.is_finite())
                    && shares > 0.0
                    && price > 0.0
                    && price <= 1.0
                    && fee_collateral >= 0.0
                    && fee_shares >= 0.0
                    && fee_shares < shares,
                "invalid recorded original entry amounts"
            );
            ensure!(
                recorded
                    .insert(
                        format!("{hash}:{index}"),
                        RecordedFill {
                            shares,
                            collateral,
                            fee_collateral,
                            fee_shares
                        }
                    )
                    .is_none(),
                "duplicate normalized original entry receipt"
            );
            own_transactions.insert(hash.clone());
            covered.insert(hash);
        }
        ensure!(
            !recorded.is_empty(),
            "original entry order lacks selected receipt evidence"
        );
        orders.push(EntryReceiptOrder {
            identity,
            transactions: own_transactions.into_iter().collect(),
            matches: recorded,
        });
    }
    ensure!(
        !orders.is_empty() && transactions.iter().all(|h| covered.contains(h)),
        "incomplete original entry order evidence"
    );
    tx.commit().await?;
    Ok(orders)
}

/// Every originally accounted event must still be present with the exact
/// gross amounts and actual fees. Fresh unknown events are equally unsafe.
pub(crate) fn verify_entry_fills(
    order: &EntryReceiptOrder,
    fills: &[SettledOrderFill],
) -> Result<()> {
    let mut observed = HashMap::new();
    for fill in fills {
        ensure!(
            fill.order_id.eq_ignore_ascii_case(&order.identity.order_id),
            "unrelated original entry fill"
        );
        let key = format!(
            "{}:{}",
            fill.transaction_hash.to_ascii_lowercase(),
            fill.log_index
        );
        ensure!(
            observed.insert(key.clone(), ()).is_none(),
            "duplicate current original entry receipt"
        );
        let old = order
            .matches
            .get(&key)
            .context("new original entry receipt contradicts accounted basis")?;
        ensure!(
            [
                fill.gross_shares,
                fill.gross_collateral,
                fill.fee_collateral,
                fill.fee_shares
            ]
            .iter()
            .all(|v| v.is_finite())
                && (old.shares - fill.gross_shares).abs() <= 1e-7
                && (old.collateral - fill.gross_collateral).abs() <= 1e-7
                && (old.fee_collateral - fill.fee_collateral).abs() <= 1e-7
                && (old.fee_shares - fill.fee_shares).abs() <= 1e-7,
            "original entry receipt changed quantity, cost or actual fees"
        );
    }
    ensure!(
        observed.len() == order.matches.len()
            && order.matches.keys().all(|id| observed.contains_key(id)),
        "original entry receipt is missing; basis is not freshly verified"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_fees_and_complete_receipt_identity_are_part_of_original_basis() {
        let hash = format!("0x{}", "44".repeat(32));
        let order = EntryReceiptOrder {
            identity: SettlementOrderIdentity {
                order_id: format!("0x{}", "55".repeat(32)),
                exchange_address: "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".into(),
                maker_address: "0x1111111111111111111111111111111111111111".into(),
                asset_id: "123".into(),
                side: Side::Buy,
                protocol: OrderProtocol::V1,
            },
            transactions: vec![hash.clone()],
            matches: BTreeMap::from([(
                format!("{hash}:1"),
                RecordedFill {
                    shares: 5.0,
                    collateral: 2.0,
                    fee_collateral: 0.0,
                    fee_shares: 0.05,
                },
            )]),
        };
        let fill = SettledOrderFill {
            transaction_hash: hash,
            log_index: 1,
            block_number: 100,
            order_id: order.identity.order_id.clone(),
            gross_shares: 5.0,
            gross_collateral: 2.0,
            fee_collateral: 0.0,
            fee_shares: 0.05,
        };
        verify_entry_fills(&order, std::slice::from_ref(&fill)).unwrap();
        assert!(verify_entry_fills(&order, &[]).is_err());
        assert!(verify_entry_fills(&order, &[fill.clone(), fill.clone()]).is_err());
        for case in 0..5 {
            let mut changed = fill.clone();
            match case {
                0 => changed.fee_shares += 0.000001,
                1 => changed.fee_collateral = 0.05,
                2 => changed.log_index = 2,
                3 => changed.gross_collateral = f64::NAN,
                _ => changed.order_id = format!("0x{}", "66".repeat(32)),
            }
            assert!(verify_entry_fills(&order, &[changed]).is_err());
        }
    }

    #[tokio::test]
    async fn equivalent_transaction_log_spellings_cannot_double_original_basis() {
        let (pool, path, proof, ids) = super::super::tests::fixture().await;
        sqlx::query("INSERT INTO live_matches(order_id,match_id,size,price,fee,fee_shares) VALUES(?,?,5,0.4,0.05,0)")
            .bind(format!("entry-{}",ids[0])).bind(format!("{}:0{}",proof.entry_transaction_hashes[0],ids[0])).execute(&pool).await.unwrap();
        let error = entry_receipt_orders(&pool, &ids, &proof.entry_transaction_hashes)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("duplicate normalized"));
        super::super::tests::cleanup(pool, path).await;
    }
}
