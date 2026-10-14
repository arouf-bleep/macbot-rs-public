use super::*;
use crate::{clob::redemption::tests::verified_fixture, db};

pub(crate) async fn fixture() -> (SqlitePool, std::path::PathBuf, VerifiedRedemption, Vec<i64>) {
    let path = std::env::temp_dir().join(format!(
        "macbot_redemption_{}.sqlite",
        rand::random::<u64>()
    ));
    let pool = db::open(path.to_str().unwrap()).await.unwrap();
    let mut proof = verified_fixture([5_000_000, 3_000_000], [1, 0]);
    proof.entry_transaction_hashes = vec![format!("0x{}", "44".repeat(32))];
    let mut ids = Vec::new();
    for (i, size) in [5.0, 3.0].into_iter().enumerate() {
        let trade = Trade {
            timestamp: (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339(),
            market_id: proof.condition_id.clone(),
            direction: if i == 0 { "BUY_YES" } else { "BUY_NO" }.into(),
            entry_price: 0.4,
            amount: size * 0.4,
            shares: size,
            status: "OPEN".into(),
            mode: "live".into(),
            accounting_version: 1,
            token_id: Some(proof.token_ids[i].clone()),
            fee_paid: size * 0.01,
            partial_closed: Some(0),
            ..Trade::default()
        };
        let id = db::insert_trade(&pool, &trade).await.unwrap();
        ids.push(id);
        sqlx::query("INSERT INTO live_orders(order_id,trade_id,account,token_id,phase,price,size,fee_rate,fee_exponent,taker_only,exchange_address,protocol,state,created_at,updated_at) VALUES(?,?,?,?,'entry',0.4,?,0,1,1,'0xe111180000d2663c0091e4f400237545b87b996b',2,'settled',?,?)")
            .bind(format!("entry-{id}")).bind(id).bind(&proof.account).bind(&proof.token_ids[i]).bind(size)
            .bind(&trade.timestamp).bind(&trade.timestamp).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO live_matches(order_id,match_id,size,price,fee,fee_shares) VALUES(?,?,?,0.4,?,0)")
            .bind(format!("entry-{id}")).bind(format!("{}:{id}",proof.entry_transaction_hashes[0])).bind(size).bind(size*0.01)
            .execute(&pool).await.unwrap();
    }
    (pool, path, proof, ids)
}
pub(crate) async fn cleanup(pool: SqlitePool, path: std::path::PathBuf) {
    pool.close().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[tokio::test]
async fn finalized_redemption_accounts_both_sides_fees_dates_and_replays_once() {
    let (pool, path, proof, ids) = fixture().await;
    let result = apply(&pool, &proof, &ids).await.unwrap();
    assert!((result["realized_pnl_delta"].as_f64().unwrap() - 1.72).abs() < 1e-9);
    assert_eq!(result["collateral_received"], 5.0);
    assert_eq!(result["transaction_submitted"], false);
    for id in &ids {
        assert_eq!(
            db::load_trade_by_id(&pool, *id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "CLOSED"
        );
    }
    let recognition_day = DateTime::parse_from_rfc3339(&proof.timestamp)
        .unwrap()
        .with_timezone(&Utc)
        .date_naive();
    let pnl = db::risk::daily_realized_pnl(&pool, recognition_day)
        .await
        .unwrap();
    assert!((pnl.exact_today_pnl.unwrap() - 1.72).abs() < 1e-9);
    assert_eq!(apply(&pool, &proof, &ids).await.unwrap()["replayed"], true);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redemption_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let events: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM realized_pnl_events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(events, 2);
    let mut changed = proof.clone();
    changed.evidence["changed"] = json!(true);
    assert!(apply(&pool, &changed, &ids)
        .await
        .unwrap_err()
        .to_string()
        .contains("contradictory"));
    // Restore-critical inventory retains the exact receipt and accounting rows.
    let backup = std::path::PathBuf::from(format!(
        "artifacts/backups/redemption_{}.sqlite",
        rand::random::<u64>()
    ));
    db::backup_database(&pool, backup.to_str().unwrap())
        .await
        .unwrap();
    let copy = db::open(backup.to_str().unwrap()).await.unwrap();
    assert_eq!(
        db::critical_table_counts(&copy)
            .await
            .unwrap()
            .redemption_receipts,
        1
    );
    assert_eq!(apply(&copy, &proof, &ids).await.unwrap()["replayed"], true);
    cleanup(copy, backup).await;
    cleanup(pool, path).await;
}

#[tokio::test]
async fn losing_only_burn_has_zero_payout_but_real_realized_loss() {
    let (pool, path, _, ids) = fixture().await;
    sqlx::query("DELETE FROM trades WHERE id=?")
        .bind(ids[0])
        .execute(&pool)
        .await
        .unwrap();
    let mut proof = verified_fixture([0, 3_000_000], [1, 0]);
    proof.entry_transaction_hashes = vec![format!("0x{}", "44".repeat(32))];
    let result = apply(&pool, &proof, &ids[1..]).await.unwrap();
    assert_eq!(result["collateral_received"], 0.0);
    assert!((result["realized_pnl_delta"].as_f64().unwrap() + 1.23).abs() < 1e-9);
    assert_eq!(
        db::load_trade_by_id(&pool, ids[1])
            .await
            .unwrap()
            .unwrap()
            .was_correct,
        Some(0)
    );
    cleanup(pool, path).await;
}

#[tokio::test]
async fn incomplete_or_mixed_managed_evidence_cannot_close_any_lot() {
    for case in 0..8 {
        let (pool, path, mut proof, mut ids) = fixture().await;
        match case {
            0 => proof.burned_units[0] += 1,
            1 => {
                ids.pop();
            }
            2 => {
                sqlx::query("UPDATE live_orders SET state='pending' WHERE trade_id=?")
                    .bind(ids[0])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            3 => {
                sqlx::query("UPDATE live_orders SET state='audit_uncertain' WHERE trade_id=?")
                    .bind(ids[0])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            4 => {
                sqlx::query("DELETE FROM live_matches WHERE order_id=?")
                    .bind(format!("entry-{}", ids[0]))
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            5 => {
                sqlx::query("UPDATE trades SET accounting_version=0 WHERE id=?")
                    .bind(ids[0])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            6 => {
                sqlx::query("UPDATE trades SET pnl=1,partial_closed=1 WHERE id=?")
                    .bind(ids[0])
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            _ => {
                proof.account = "0x2222222222222222222222222222222222222222".into();
            }
        }
        assert!(apply(&pool, &proof, &ids).await.is_err(), "case{case}");
        let closed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades WHERE status='CLOSED'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(closed, 0);
        let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redemption_receipts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(receipts, 0);
        cleanup(pool, path).await;
    }
}

#[tokio::test]
async fn failed_event_insert_rolls_back_lots_and_receipt_identity() {
    let (pool, path, proof, ids) = fixture().await;
    sqlx::query("CREATE TRIGGER fail_redemption_event BEFORE INSERT ON realized_pnl_events BEGIN SELECT RAISE(ABORT,'injected realization failure'); END").execute(&pool).await.unwrap();
    assert!(apply(&pool, &proof, &ids).await.is_err());
    for id in &ids {
        let lot = db::load_trade_by_id(&pool, *id).await.unwrap().unwrap();
        assert!(lot.is_open());
        assert!(lot.pnl.is_none());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redemption_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER fail_redemption_event")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(apply(&pool, &proof, &ids).await.unwrap()["replayed"], false);
    cleanup(pool, path).await;
}

#[tokio::test]
async fn competing_replays_cannot_account_the_same_burn_twice() {
    let (pool, path, proof, ids) = fixture().await;
    let (a, b) = tokio::join!(apply(&pool, &proof, &ids), apply(&pool, &proof, &ids));
    let a = a.unwrap();
    let b = b.unwrap();
    assert_ne!(a["replayed"], b["replayed"]);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redemption_receipts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    cleanup(pool, path).await;
}

#[tokio::test]
async fn evidence_export_refuses_a_changed_lot_and_requires_verified_entry_identities() {
    let (pool, path, mut proof, ids) = fixture().await;
    assert_eq!(
        entry_transactions(&pool, &ids).await.unwrap(),
        proof.entry_transaction_hashes
    );
    let verified = proof.entry_transaction_hashes.clone();
    proof.entry_transaction_hashes.clear();
    assert!(apply(&pool, &proof, &ids)
        .await
        .unwrap_err()
        .to_string()
        .contains("not verified"));
    proof.entry_transaction_hashes = verified;
    apply(&pool, &proof, &ids).await.unwrap();
    let evidence = evidence_for_trade(&pool, ids[0]).await.unwrap().unwrap();
    assert_eq!(evidence["fresh_chain_recheck"], false);
    assert_eq!(evidence["transaction_hash"], proof.transaction_hash);
    sqlx::query("UPDATE trades SET pnl=pnl+1 WHERE id=?")
        .bind(ids[0])
        .execute(&pool)
        .await
        .unwrap();
    assert!(evidence_for_trade(&pool, ids[0]).await.is_err());
    cleanup(pool, path).await;
}

#[tokio::test]
async fn invalid_or_later_dated_partial_event_never_passes_a_text_max_timestamp() {
    for timestamp in ["not-a-date", "2099-01-01T00:00:00+13:00"] {
        let (pool, path, proof, ids) = fixture().await;
        sqlx::query("INSERT INTO realized_pnl_events VALUES(?, 'bad/time', ?,0,1)")
            .bind(ids[0])
            .bind(timestamp)
            .execute(&pool)
            .await
            .unwrap();
        assert!(apply(&pool, &proof, &ids).await.is_err());
        assert!(db::load_trade_by_id(&pool, ids[0])
            .await
            .unwrap()
            .unwrap()
            .is_open());
        cleanup(pool, path).await;
    }
}

#[tokio::test]
async fn maker_entry_time_after_redemption_cannot_be_hidden_by_an_earlier_lot_time() {
    let (pool, path, proof, ids) = fixture().await;
    let order = "maker-entry-fixture";
    let match_id = format!("{}:10", proof.entry_transaction_hashes[0]);
    let identity = SettlementOrderIdentity {
        order_id: order.into(),
        exchange_address: "0xe111180000d2663c0091e4f400237545b87b996b".into(),
        maker_address: proof.account.clone(),
        asset_id: proof.token_ids[0].clone(),
        side: crate::clob::Side::Buy,
        protocol: crate::clob::OrderProtocol::V2,
    };
    sqlx::query("DELETE FROM live_matches WHERE order_id=?")
        .bind(format!("entry-{}", ids[0]))
        .execute(&pool)
        .await
        .unwrap();
    let before = (DateTime::parse_from_rfc3339(&proof.timestamp).unwrap()
        - chrono::Duration::minutes(1))
    .to_rfc3339();
    let after = (DateTime::parse_from_rfc3339(&proof.timestamp).unwrap()
        + chrono::Duration::minutes(1))
    .to_rfc3339();
    sqlx::query("INSERT INTO maker_orders(order_id,market_id,token_id,side,price,size,confirmed_size,state,template_json,settlement_json,created_at,updated_at) VALUES(?,?,?,'BUY',0.4,5,5,'TERMINAL','{}',?,?,?)")
        .bind(order).bind(&proof.condition_id).bind(&proof.token_ids[0])
        .bind(serde_json::to_string(&identity).unwrap()).bind(&before).bind(&before).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO maker_matches(match_id,order_id,token_id,side,size,price,fee_usd,timestamp) VALUES(?,?,?,'BUY',5,0.4,0.05,?)")
        .bind(&match_id).bind(order).bind(&proof.token_ids[0]).bind(&after).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO maker_match_allocations(match_id,order_id,trade_id,phase,size,notional,fee_usd) VALUES(?,?,?,'entry',5,2,0.05)")
        .bind(&match_id).bind(order).bind(ids[0]).execute(&pool).await.unwrap();
    assert!(apply(&pool, &proof, &ids)
        .await
        .unwrap_err()
        .to_string()
        .contains("timestamp follows"));
    assert!(db::load_trade_by_id(&pool, ids[0])
        .await
        .unwrap()
        .unwrap()
        .is_open());
    sqlx::query("UPDATE maker_matches SET timestamp=? WHERE order_id=?")
        .bind(before)
        .bind(order)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        apply(&pool, &proof, &ids).await.unwrap()["status"],
        "reconciled"
    );
    cleanup(pool, path).await;
}

#[tokio::test]
async fn prior_partial_profit_and_entry_fees_are_not_charged_twice_on_redemption() {
    let (pool, path, _, ids) = fixture().await;
    let mut proof = verified_fixture([3_000_000, 3_000_000], [1, 0]);
    proof.entry_transaction_hashes = vec![format!("0x{}", "44".repeat(32))];
    sqlx::query("UPDATE trades SET shares=3,amount=1.2,pnl=-0.22,partial_closed=1 WHERE id=?")
        .bind(ids[0])
        .execute(&pool)
        .await
        .unwrap();
    // Distinct accounting identities may share one recognition second. Keep
    // this aggregate-fee fixture on one UTC day, even at exactly midnight.
    let earlier = proof.timestamp.clone();
    sqlx::query("INSERT INTO realized_pnl_events(trade_id,event_key,timestamp,net_pnl,realized_shares) VALUES(?,'partial-fixture',?,-0.22,2)")
        .bind(ids[0]).bind(earlier).execute(&pool).await.unwrap();
    let result = apply(&pool, &proof, &ids).await.unwrap();
    assert!((result["realized_pnl_delta"].as_f64().unwrap() - 0.54).abs() < 1e-9);
    let lot = db::load_trade_by_id(&pool, ids[0]).await.unwrap().unwrap();
    assert!((lot.pnl.unwrap() - 1.55).abs() < 1e-9);
    let recognition_day = DateTime::parse_from_rfc3339(&proof.timestamp)
        .unwrap()
        .with_timezone(&Utc)
        .date_naive();
    assert!(
        (db::risk::daily_realized_pnl(&pool, recognition_day)
            .await
            .unwrap()
            .exact_today_pnl
            .unwrap()
            - 0.32)
            .abs()
            < 1e-9
    );
    cleanup(pool, path).await;
}

#[tokio::test]
async fn oversized_local_entry_history_is_refused_before_receipt_accounting() {
    let (pool, path, proof, ids) = fixture().await;
    for index in 10..266 {
        sqlx::query("INSERT INTO live_matches(order_id,match_id,size,price,fee,fee_shares) VALUES(?,?,1,0.4,0,0)")
            .bind(format!("entry-{}",ids[0])).bind(format!("{}:{index}",proof.entry_transaction_hashes[0])).execute(&pool).await.unwrap();
    }
    assert!(entry_transactions(&pool, &ids)
        .await
        .unwrap_err()
        .to_string()
        .contains("oversized"));
    assert!(apply(&pool, &proof, &ids)
        .await
        .unwrap_err()
        .to_string()
        .contains("oversized"));
    assert!(db::load_trade_by_id(&pool, ids[0])
        .await
        .unwrap()
        .unwrap()
        .is_open());
    cleanup(pool, path).await;
}
