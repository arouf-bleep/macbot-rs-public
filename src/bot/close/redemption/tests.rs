//! Complete actor/RPC fixtures are synthetic and never submit funds.
use super::*;
use crate::{
    clob::redemption::tests::mock_rpc,
    db::redemption::tests::{cleanup, fixture},
};
use std::sync::Arc;

async fn actor_fixture() -> (
    Arc<TradingBot>,
    RedemptionRequest,
    std::path::PathBuf,
    Arc<std::sync::Mutex<crate::clob::redemption::tests::RpcState>>,
    tokio::task::JoinHandle<()>,
) {
    let (pool, path, proof, ids) = fixture().await;
    let (client, mut request, state, task) = mock_rpc().await;
    request.trade_ids = ids.clone();
    sqlx::query("UPDATE live_orders SET account=?")
        .bind(client.account_address())
        .execute(&pool)
        .await
        .unwrap();
    use alloy::primitives::{keccak256, Address, B256, U256};
    let mut receipt = proof.evidence["receipt"].clone();
    let entry_hash = format!("0x{}", "44".repeat(32));
    receipt["transactionHash"] = serde_json::json!(entry_hash);
    receipt["transactionIndex"] = serde_json::json!("0x4");
    let account: Address = client.account_address().parse().unwrap();
    let mut logs = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        let order_id = format!("0x{id:064x}");
        sqlx::query("UPDATE live_matches SET order_id=? WHERE order_id=?")
            .bind(&order_id)
            .bind(format!("entry-{id}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE live_orders SET order_id=? WHERE order_id=?")
            .bind(&order_id)
            .bind(format!("entry-{id}"))
            .execute(&pool)
            .await
            .unwrap();
        let size = if index == 0 { 5_000_000 } else { 3_000_000 };
        let words = [
            U256::ZERO,
            U256::from(if index == 0 { 123 } else { 456 }),
            U256::from(size * 4 / 10),
            U256::from(size),
            U256::from(size / 100),
            U256::ZERO,
            U256::ZERO,
        ];
        let bytes: Vec<u8> = words
            .into_iter()
            .flat_map(|word| word.to_be_bytes::<32>())
            .collect();
        logs.push(serde_json::json!({"address":"0xe111180000d2663c0091e4f400237545b87b996b",
            "topics":[format!("{:#x}",keccak256("OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)")),order_id,format!("{:#x}",B256::from(account.into_word())),format!("{:#x}",B256::ZERO)],
            "data":format!("0x{}",hex::encode(bytes)),"removed":false,"transactionHash":entry_hash,
            "blockNumber":receipt["blockNumber"],"blockHash":receipt["blockHash"],"logIndex":format!("0x{id:x}")}));
    }
    receipt["logs"] = serde_json::json!(logs);
    state.lock().unwrap().entry_receipt = Some(receipt);
    let mut cfg: crate::config::Config =
        serde_json::from_str(include_str!("../../../../config.json")).unwrap();
    cfg.mode = "live".into();
    cfg.api_key = "redemption-read-fixture".into();
    cfg.api_hmac_secret = "redemption-signing-fixture".into();
    cfg.allowed_ips.clear();
    let mut bot = TradingBot::new(cfg, pool).await.unwrap();
    bot.clob = Some(Arc::new(client));
    let bot = Arc::new(bot);
    bot.freeze_entries(
        "operator_redemption_fixture",
        "offline recovery safety test",
    )
    .await;
    bot.start_execution_actor().await;
    (bot, request, path, state, task)
}

#[tokio::test]
// This current-thread test holds the process environment lease through async fixture work.
#[allow(clippy::await_holding_lock)]
async fn serialized_redemption_replays_without_submission_and_exports_local_evidence() {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    let _environment = crate::env_util::test_env_lock();
    let _api_key =
        crate::bot::close::tests::EnvRestore::set("MACBOT_API_KEY", "redemption-read-fixture");
    let _api_secret = crate::bot::close::tests::EnvRestore::set(
        "MACBOT_API_HMAC_SECRET",
        "redemption-signing-fixture",
    );
    let (bot, request, path, state, task) = actor_fixture().await;
    let result = bot
        .reconcile_redemption_serialized(request.clone())
        .await
        .unwrap();
    assert_eq!(result["status"], "reconciled");
    assert_eq!(result["balance_verified"], true);
    assert_eq!(result["available_balance"], 18.0);
    assert_eq!(result["kill_switch"], true);
    assert_eq!(result["loop_running"], false);
    assert_eq!(
        bot.reconcile_redemption_serialized(request.clone())
            .await
            .unwrap()["replayed"],
        true
    );
    assert!(state.lock().unwrap().calls.iter().all(|c| matches!(
        c["method"].as_str(),
        Some("eth_call" | "eth_getTransactionReceipt" | "eth_getBlockByNumber" | "eth_chainId")
    )));
    let router = crate::api::router(bot.clone());
    let no_auth = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/recovery/redemptions/reconcile")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&request).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
    // The actual signed API path repeats the immutable proof without adding
    // another realization or issuing any chain transaction.
    use hmac::{Hmac, Mac};
    let body = serde_json::to_string(&request).unwrap();
    let path_api = "/api/recovery/redemptions/reconcile";
    let timestamp = chrono::Utc::now().timestamp().to_string();
    let nonce = hex::encode(rand::random::<[u8; 32]>());
    let canonical = serde_json::to_string(&[
        timestamp.as_str(),
        nonce.as_str(),
        "POST",
        path_api,
        "",
        body.as_str(),
    ])
    .unwrap();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"redemption-signing-fixture").unwrap();
    mac.update(canonical.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());
    let replay = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(path_api)
                .header("content-type", "application/json")
                .header("x-api-key", "redemption-read-fixture")
                .header("x-api-ts", timestamp)
                .header("x-api-nonce", nonce)
                .header("x-api-signature", signature)
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    let payload: Value =
        serde_json::from_slice(&to_bytes(replay.into_body(), 2_000_000).await.unwrap()).unwrap();
    assert_eq!(payload["replayed"], true);
    for path in [
        format!("/api/recovery/orders/{}", request.transaction_hash),
        format!("/api/trades/{}/fills", request.trade_ids[0]),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("x-api-key", "redemption-read-fixture")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 2_000_000).await.unwrap();
        let payload: Value = serde_json::from_slice(&bytes).unwrap();
        if path.ends_with("/fills") {
            assert_eq!(
                payload["redemption_evidence"]["status"],
                "recorded_finalized_receipt"
            );
            assert_eq!(
                payload["parity_check"]["exit_status"],
                "redemption_requires_receipt"
            );
        } else {
            assert_eq!(payload["schema_version"], 8);
            assert_eq!(
                payload["evidence"]["redemption_receipts"]["rows"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                payload["evidence"]["trades"]["rows"]
                    .as_array()
                    .unwrap()
                    .len(),
                2
            );
        }
    }
    bot.stop_execution_actor().await.unwrap();
    task.abort();
    cleanup(bot.db.clone(), path).await;
}

#[tokio::test]
async fn slow_read_preflight_is_cancelled_without_mutating_lots_or_receipt_ledger() {
    let (bot, request, path, state, task) = actor_fixture().await;
    state.lock().unwrap().delay_entry = true;
    let before = std::time::Instant::now();
    let error = bot
        .redemption_evidence(&request, std::time::Duration::from_millis(50))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("deadline"));
    assert!(before.elapsed() < std::time::Duration::from_secs(2));
    for id in request.trade_ids {
        assert!(db::load_trade_by_id(&bot.db, id)
            .await
            .unwrap()
            .unwrap()
            .is_open());
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redemption_receipts")
        .fetch_one(&bot.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
    bot.stop_execution_actor().await.unwrap();
    task.abort();
    cleanup(bot.db.clone(), path).await;
}

#[tokio::test]
async fn entry_receipt_without_its_orderfilled_logs_cannot_establish_redemption_basis() {
    let (bot, request, path, state, task) = actor_fixture().await;
    if let Some(receipt) = &mut state.lock().unwrap().entry_receipt {
        receipt["logs"] = serde_json::json!([]);
    }
    let evidence = bot
        .redemption_evidence(&request, std::time::Duration::from_secs(2))
        .await;
    let receipts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redemption_receipts")
        .fetch_one(&bot.db)
        .await
        .unwrap();
    assert_eq!(receipts, 0);
    for id in &request.trade_ids {
        assert!(db::load_trade_by_id(&bot.db, *id)
            .await
            .unwrap()
            .unwrap()
            .is_open());
    }
    bot.stop_execution_actor().await.unwrap();
    task.abort();
    cleanup(bot.db.clone(), path).await;
    assert!(
        evidence.is_err(),
        "a successful entry envelope is not proof of original fill quantity/cost/fees"
    );
}

fn change_entry_word(receipt: &mut Value, word: usize, units: u64) {
    let data = receipt["logs"][0]["data"].as_str().unwrap();
    let mut bytes = hex::decode(&data[2..]).unwrap();
    bytes[word * 32..(word + 1) * 32]
        .copy_from_slice(&alloy::primitives::U256::from(units).to_be_bytes::<32>());
    receipt["logs"][0]["data"] = serde_json::json!(format!("0x{}", hex::encode(bytes)));
}

#[tokio::test]
async fn changed_or_incomplete_original_entry_receipts_cannot_credit_redemption() {
    for case in 0..6 {
        let (bot, request, path, state, task) = actor_fixture().await;
        let before = bot.balance().await;
        {
            let mut rpc = state.lock().unwrap();
            let receipt = rpc.entry_receipt.as_mut().unwrap();
            match case {
                0 => change_entry_word(receipt, 3, 5_000_001),
                1 => change_entry_word(receipt, 2, 2_000_001),
                2 => change_entry_word(receipt, 4, 50_001),
                3 => change_entry_word(receipt, 1, 999),
                4 => {
                    receipt["logs"].as_array_mut().unwrap().remove(0);
                }
                _ => {
                    let mut extra = receipt["logs"][0].clone();
                    extra["logIndex"] = serde_json::json!("0x99");
                    receipt["logs"].as_array_mut().unwrap().push(extra);
                }
            }
        }
        let result = bot.reconcile_redemption_serialized(request.clone()).await;
        assert!(
            result.is_err(),
            "changed entry evidence accepted in case {case}"
        );
        assert_eq!(bot.balance().await, before);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM redemption_receipts")
            .fetch_one(&bot.db)
            .await
            .unwrap();
        assert_eq!(count, 0);
        for id in &request.trade_ids {
            let trade = db::load_trade_by_id(&bot.db, *id).await.unwrap().unwrap();
            assert!(trade.is_open());
            assert_eq!(trade.pnl, None);
        }
        assert!(bot.kill_switch_active());
        assert!(state.lock().unwrap().calls.iter().all(|c| matches!(
            c["method"].as_str(),
            Some("eth_call" | "eth_getTransactionReceipt" | "eth_getBlockByNumber" | "eth_chainId")
        )));
        bot.stop_execution_actor().await.unwrap();
        task.abort();
        cleanup(bot.db.clone(), path).await;
    }
}

#[tokio::test]
async fn maker_entry_checks_complete_receipt_and_credits_only_selected_lot_basis() {
    let (bot, request, path, state, task) = actor_fixture().await;
    let id = request.trade_ids[0];
    let order_id = format!("0x{id:064x}");
    let lot = db::load_trade_by_id(&bot.db, id).await.unwrap().unwrap();
    let identity = crate::clob::settlement::SettlementOrderIdentity {
        order_id: order_id.clone(),
        exchange_address: "0xe111180000d2663c0091e4f400237545b87b996b".into(),
        maker_address: bot.clob.as_ref().unwrap().account_address(),
        asset_id: "123".into(),
        side: crate::clob::Side::Buy,
        protocol: crate::clob::OrderProtocol::V2,
    };
    sqlx::query("DELETE FROM live_matches WHERE order_id=?")
        .bind(&order_id)
        .execute(&bot.db)
        .await
        .unwrap();
    sqlx::query("DELETE FROM live_orders WHERE order_id=?")
        .bind(&order_id)
        .execute(&bot.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO maker_orders(order_id,market_id,token_id,side,price,size,confirmed_size,state,template_json,settlement_json,created_at,updated_at) VALUES(?,?,'123','BUY',0.4,6,6,'TERMINAL',?,?,?,?)")
        .bind(&order_id).bind(&lot.market_id).bind(serde_json::to_string(&lot).unwrap()).bind(serde_json::to_string(&identity).unwrap()).bind(&lot.timestamp).bind(&lot.timestamp).execute(&bot.db).await.unwrap();
    let match_id = format!("0x{}:{id}", "44".repeat(32));
    sqlx::query("INSERT INTO maker_matches(match_id,order_id,token_id,side,size,price,fee_usd,timestamp) VALUES(?,?,'123','BUY',6,0.4,0.06,?)")
        .bind(&match_id).bind(&order_id).bind(&lot.timestamp).execute(&bot.db).await.unwrap();
    sqlx::query("INSERT INTO maker_match_allocations(match_id,order_id,trade_id,phase,size,notional,fee_usd) VALUES(?,?,?,'entry',5,2,0.05)")
        .bind(&match_id).bind(&order_id).bind(id).execute(&bot.db).await.unwrap();
    {
        let mut rpc = state.lock().unwrap();
        let receipt = rpc.entry_receipt.as_mut().unwrap();
        change_entry_word(receipt, 2, 2_400_000);
        change_entry_word(receipt, 3, 6_000_000);
        change_entry_word(receipt, 4, 60_000);
    }
    let result = bot
        .reconcile_redemption_serialized(request.clone())
        .await
        .unwrap();
    assert!((result["realized_pnl_delta"].as_f64().unwrap() - 1.72).abs() < 1e-9);
    assert_eq!(
        bot.reconcile_redemption_serialized(request).await.unwrap()["replayed"],
        true
    );
    bot.stop_execution_actor().await.unwrap();
    task.abort();
    cleanup(bot.db.clone(), path).await;
}

#[tokio::test]
async fn uncertain_entry_blocks_new_credit_but_verified_replay_never_clears_a_stop() {
    let (bot, request, path, _, task) = actor_fixture().await;
    let cash = bot.balance().await;
    sqlx::query("UPDATE live_orders SET state='audit_uncertain'")
        .execute(&bot.db)
        .await
        .unwrap();
    assert!(bot
        .reconcile_redemption_serialized(request.clone())
        .await
        .unwrap_err()
        .to_string()
        .contains("audit-uncertain"));
    assert_eq!(bot.balance().await, cash);
    assert!(db::load_trade_by_id(&bot.db, request.trade_ids[0])
        .await
        .unwrap()
        .unwrap()
        .is_open());
    sqlx::query("UPDATE live_orders SET state='settled'")
        .execute(&bot.db)
        .await
        .unwrap();
    bot.reconcile_redemption_serialized(request.clone())
        .await
        .unwrap();
    sqlx::query("UPDATE live_orders SET state='audit_uncertain'")
        .execute(&bot.db)
        .await
        .unwrap();
    let cash = bot.balance().await;
    let result = bot.reconcile_redemption_serialized(request).await.unwrap();
    assert_eq!(result["replayed"], true);
    assert_eq!(bot.balance().await, cash);
    assert_eq!(db::live::audit_uncertain_count(&bot.db).await.unwrap(), 2);
    assert!(bot.kill_switch_active());
    bot.stop_execution_actor().await.unwrap();
    task.abort();
    cleanup(bot.db.clone(), path).await;
}
