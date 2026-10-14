//! Offline venue and finalized receipt regressions; no external network/funds.
use super::{CloseIntent, TradingBot};
use crate::{
    bot::actor::maker_live_tests::{fixture, MockMakerVenue},
    db,
};

async fn expanded_fixture(
    shares: f64,
) -> (TradingBot, db::Trade, std::path::PathBuf, MockMakerVenue) {
    let (bot, mut trade, path, venue) = fixture().await;
    trade.shares = shares;
    trade.amount = shares * trade.entry_price;
    db::update_trade(&bot.db, &trade).await.unwrap();
    sqlx::query("UPDATE trades SET entry_shares=?,entry_cost=? WHERE id=?")
        .bind(shares)
        .bind(trade.amount)
        .bind(trade.id)
        .execute(&bot.db)
        .await
        .unwrap();
    *bot.portfolio_state.trades.write().await = vec![trade.clone()];
    (bot, trade, path, venue)
}

#[tokio::test]
async fn recovered_entry_buy_cannot_satisfy_a_partial_sale_request() {
    use crate::clob::{LimitOrderArgs, OrderType, Side};
    let (bot, mut trade, path, venue) = fixture().await;
    // A pending entry starts with reserved budget and no proved inventory.
    // The shared sale fixture normally represents a previously settled lot.
    trade.shares = 0.0;
    db::update_trade(&bot.db, &trade).await.unwrap();
    sqlx::query("UPDATE trades SET entry_shares=0,entry_cost=0 WHERE id=?")
        .bind(trade.id)
        .execute(&bot.db)
        .await
        .unwrap();
    *bot.portfolio_state.trades.write().await = vec![trade.clone()];
    let clob = bot.clob.as_ref().unwrap();
    let token = trade.token_id.as_ref().unwrap();
    let signed = clob
        .create_order(&LimitOrderArgs {
            token_id: token.clone(),
            price: 0.4,
            size: 5.0,
            side: Side::Buy,
        })
        .await
        .unwrap();
    let fees = clob.market_fee_parameters(token).await.unwrap();
    db::live::prepare_order(
        &bot.db,
        trade.id,
        &clob.account_address(),
        &signed,
        0.4,
        fees,
        "entry",
    )
    .await
    .unwrap();
    let pending = db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending.shares, 0.0);
    clob.post_order(&signed, OrderType::FOK).await.unwrap();
    venue.fill("confirmed-buy-only", 5.0, 0.4, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    let result = bot
        .close_confirmed_live_quantity(&pending, Some(0.5))
        .await
        .unwrap();
    assert_eq!(result["status"], "rejected");
    assert_eq!(result["ok"], false);
    assert_eq!(result["confirmed_shares"], 0.0);
    assert_eq!(result["remaining_shares"], 5.0);
    assert!(result["pnl"].is_null());
    assert!(result["exit_price"].is_null());
    assert!(result["reason"].as_str().unwrap().contains("entry BUY"));
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    let stored = db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, "OPEN");
    assert_eq!(stored.shares, 5.0);
    assert_eq!(stored.partial_closed, Some(0));
    let realized: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM realized_pnl_events WHERE trade_id=?")
            .bind(trade.id)
            .fetch_one(&bot.db)
            .await
            .unwrap();
    assert_eq!(realized, 0);
    super::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn requested_partial_quantity_is_durable_and_final_receipt_drives_net_accounting() {
    let (bot, trade, path, venue) = expanded_fixture(12.0).await;
    venue.state.lock().unwrap().drop_post_reply = true;
    let initial = bot
        .partial_close_trade_with_intent(trade.id, 0.99, 0.5, CloseIntent::Market)
        .await
        .unwrap();
    assert_eq!(initial["status"], "pending");
    assert!(initial["pnl"].is_null());
    let durable = db::live::pending_orders(&bot.db)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(durable.size, 6.0);
    assert_eq!(
        venue.state.lock().unwrap().size,
        6.0,
        "mock POST also verifies intent was committed first"
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    venue.fill("deliberate-six", 6.0, 0.55, "CONFIRMED");
    {
        let mut state = venue.state.lock().unwrap();
        state.status = "MATCHED".into();
        // A real SELL receipt's collateral fee is the fifth ABI data word.
        let receipt = state.receipts.values_mut().next().unwrap();
        let data = receipt["logs"][0]["data"].as_str().unwrap();
        let mut bytes = hex::decode(&data[2..]).unwrap();
        bytes[128..160]
            .copy_from_slice(&alloy::primitives::U256::from(30_000_u64).to_be_bytes::<32>());
        receipt["logs"][0]["data"] = serde_json::json!(format!("0x{}", hex::encode(bytes)));
    }
    let recovered = bot
        .partial_close_trade_with_intent(trade.id, 0.99, 0.5, CloseIntent::Market)
        .await
        .unwrap();
    assert_eq!(recovered["status"], "settled");
    assert_eq!(recovered["confirmed_shares"], 6.0);
    assert_eq!(recovered["remaining_shares"], 6.0);
    assert!(
        (recovered["exit_price"].as_f64().unwrap() - 0.55).abs() < 1e-12,
        "a caller's trigger price cannot become execution truth"
    );
    assert!((recovered["pnl"].as_f64().unwrap() - 0.87).abs() < 1e-9);
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "recovery must not sell reopened remainder"
    );
    let persisted = db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.status, "OPEN");
    assert_eq!(persisted.partial_closed, Some(1));
    assert_eq!(persisted.shares, 6.0);
    assert!((persisted.amount - 2.4).abs() < 1e-9);
    let entry = db::trade_entry_cost(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    assert!((entry - 4.8).abs() < 1e-9);
    let daily = db::risk::daily_realized_pnl(&bot.db, chrono::Utc::now().date_naive())
        .await
        .unwrap();
    assert!((daily.exact_today_pnl.unwrap() - 0.87).abs() < 1e-9);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM realized_pnl_events WHERE trade_id=?")
            .bind(trade.id)
            .fetch_one(&bot.db)
            .await
            .unwrap();
    assert_eq!(count, 1);
    super::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn unconfirmed_partial_sale_survives_restart_without_a_second_post() {
    let (bot, trade, path, venue) = expanded_fixture(12.0).await;
    let outcome = bot.partial_close_trade(trade.id, 0.9, 0.5).await.unwrap();
    assert_eq!(outcome["status"], "pending");
    venue.fill("still-matched", 6.0, 0.55, "MATCHED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    let mut restarted = TradingBot::new(
        bot.control_plane.config.read().await.clone(),
        bot.db.clone(),
    )
    .await
    .unwrap();
    restarted.clob = bot.clob.clone();
    let result = restarted
        .partial_close_trade(trade.id, 0.9, 0.5)
        .await
        .unwrap();
    assert_eq!(result["status"], "pending");
    let stored = db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, "CLOSING");
    assert_eq!(stored.shares, 12.0);
    assert!(stored.pnl.is_none());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    venue.confirm("still-matched");
    let recovered = restarted
        .partial_close_trade(trade.id, 0.9, 0.5)
        .await
        .unwrap();
    assert_eq!(recovered["status"], "settled");
    assert_eq!(recovered["remaining_shares"], 6.0);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    drop(restarted);
    super::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn below_minimum_sale_or_remainder_is_explicitly_refused_without_submission() {
    for (shares, fraction) in [(5.0, 0.5), (10.0, 0.6)] {
        let (bot, trade, path, venue) = expanded_fixture(shares).await;
        let cash = bot.balance().await;
        let result = bot
            .partial_close_trade(trade.id, 0.9, fraction)
            .await
            .unwrap();
        assert_eq!(result["status"], "rejected");
        assert_eq!(result["ok"], false);
        assert!(result["reason"].as_str().unwrap().contains("minimum"));
        assert_eq!(venue.state.lock().unwrap().posts, 0);
        assert!(db::live::pending_orders(&bot.db).await.unwrap().is_empty());
        let stored = db::load_trade_by_id(&bot.db, trade.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, "OPEN");
        assert_eq!(stored.shares, shares);
        assert_eq!(stored.pnl, None);
        assert_eq!(bot.balance().await, cash);
        super::tests::cleanup_exit_fixture(&bot, &path).await;
    }
}

#[tokio::test]
async fn full_sale_keeps_real_precision_dust_open_and_reports_it() {
    let (bot, trade, path, venue) = expanded_fixture(5.00999).await;
    let pending = bot
        .close_confirmed_live_quantity(&trade, None)
        .await
        .unwrap();
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["requested_shares"], 5.0);
    venue.fill("five-share-sale", 5.0, 0.55, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    assert!(bot.recover_live_orders_or_halt().await);
    let remaining = db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remaining.status, "OPEN");
    assert!((remaining.shares - 0.00999).abs() < 1e-9);
    let result = bot
        .close_confirmed_live_quantity(&remaining, None)
        .await
        .unwrap();
    assert_eq!(result["status"], "rejected");
    assert_eq!(result["dust_remaining"], true);
    assert!(result["reason"].as_str().unwrap().contains("minimum"));
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "unexecutable dust is retained instead of fabricated sold"
    );
    super::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn manual_partial_actor_command_persists_stand_down_before_submitting() {
    let (bot, trade, path, venue) = expanded_fixture(12.0).await;
    let bot = std::sync::Arc::new(bot);
    bot.start_execution_actor().await;
    let result = bot
        .partial_close_trade_serialized_with_intent(trade.id, 0.99, 0.5, CloseIntent::Manual)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "pending");
    assert!(db::maker::market_stood_down(&bot.db, &trade.market_id)
        .await
        .unwrap());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(venue.state.lock().unwrap().size, 6.0);
    bot.stop_execution_actor().await.unwrap();
    super::tests::cleanup_exit_fixture(&bot, &path).await;

    let (bot, trade, path, venue) = expanded_fixture(12.0).await;
    sqlx::query("CREATE TRIGGER fail_manual_stop BEFORE INSERT ON maker_market_controls BEGIN SELECT RAISE(ABORT,'injected manual stop persistence failure'); END")
        .execute(&bot.db).await.unwrap();
    let bot = std::sync::Arc::new(bot);
    bot.start_execution_actor().await;
    let result = bot
        .partial_close_trade_serialized_with_intent(trade.id, 0.99, 0.5, CloseIntent::Manual)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["status"], "pending");
    assert_eq!(
        venue.state.lock().unwrap().posts,
        0,
        "a failed durable stand-down must stop the sale before POST"
    );
    assert!(db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap()
        .is_open());
    assert!(bot.kill_switch_active());
    bot.stop_execution_actor().await.unwrap();
    super::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn partial_actor_hold_releases_only_after_its_bound_exit_receipt_is_final() {
    let (bot, trade, path, venue) = expanded_fixture(12.0).await;
    let bot = std::sync::Arc::new(bot);
    bot.start_execution_actor().await;
    let pending = bot
        .partial_close_trade_serialized_with_intent(trade.id, 0.99, 0.5, CloseIntent::Manual)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pending["status"], "pending");
    let closing = db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    let visible = bot.trade_with_live_fields(&closing).await;
    assert_eq!(visible["partial_close_state"], "unknown");
    assert!(visible["unrealized_pnl"].is_null());
    bot.stop_execution_actor().await.unwrap();
    let order = db::live::pending_orders(&bot.db)
        .await
        .unwrap()
        .pop()
        .unwrap();
    let intent = bot.trade_lifecycle.partial_close_intents.read().await[&trade.id].clone();
    assert_eq!(
        intent.live_order_id.as_deref(),
        Some(order.order_id.as_str())
    );
    assert!(intent.outcome_unknown);
    venue.fill("held-six", 6.0, 0.55, "MATCHED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    bot.reconcile_pending_live_orders().await.unwrap();
    assert_eq!(
        db::load_trade_by_id(&bot.db, trade.id)
            .await
            .unwrap()
            .unwrap()
            .status,
        "CLOSING"
    );
    assert!(bot
        .trade_lifecycle
        .partial_close_intents
        .read()
        .await
        .contains_key(&trade.id));
    venue.confirm("held-six");
    bot.reconcile_pending_live_orders().await.unwrap();
    assert!(!bot
        .trade_lifecycle
        .partial_close_intents
        .read()
        .await
        .contains_key(&trade.id));
    let remaining = db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap();
    assert!(remaining.is_open());
    assert_eq!(remaining.shares, 6.0);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    super::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn pre_submit_recovery_cannot_release_an_unbound_new_partial_generation() {
    use crate::bot::state::PartialCloseIntent;
    let (bot, trade, path, venue) = expanded_fixture(12.0).await;
    let pending = bot.partial_close_trade(trade.id, 0.99, 0.5).await.unwrap();
    assert_eq!(pending["status"], "pending");
    bot.trade_lifecycle
        .partial_close_intents
        .write()
        .await
        .insert(
            trade.id,
            PartialCloseIntent {
                request_id: 44,
                outcome_unknown: false,
                live_order_id: None,
            },
        );
    venue.fill("old-six", 6.0, 0.55, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    bot.reconcile_pending_live_orders().await.unwrap();
    assert_eq!(
        bot.trade_lifecycle.partial_close_intents.read().await[&trade.id].request_id,
        44
    );
    assert!(
        bot.trade_lifecycle.partial_close_intents.read().await[&trade.id]
            .live_order_id
            .is_none()
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    super::tests::cleanup_exit_fixture(&bot, &path).await;
}
