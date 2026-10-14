use super::*;
use crate::bot::actor::maker_live_tests::fixture;
use crate::clob::{LimitOrderArgs, OrderType, Side};

#[tokio::test]
async fn stalled_taker_history_releases_the_writer_without_replaying_or_releasing_a_sale() {
    let (bot, original, path, venue) = fixture().await;
    venue.state.lock().unwrap().drop_post_reply = true;
    assert!(bot.close_confirmed_live_position(&original).await.is_none());
    let pending = db::live::pending_orders(&bot.db).await.unwrap().remove(0);
    let before = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    let cash = bot.balance().await;
    venue.state.lock().unwrap().delay_history_ms = 500;
    let started = tokio::time::Instant::now();
    assert!(
        !bot.recover_live_orders_or_halt_with_read_budget(Duration::from_millis(30))
            .await
    );
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(bot.kill_switch_active());
    assert!(bot.degraded_no_trade_active());
    assert!(bot
        .kill_switch_status()
        .1
        .unwrap()
        .detail
        .contains("deadline"));
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(bot.balance().await, cash);
    assert_eq!(
        serde_json::json!(db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()),
        serde_json::json!(before)
    );
    assert_eq!(
        db::live::pending_orders(&bot.db).await.unwrap()[0].order_id,
        pending.order_id
    );
    assert!(db::load_trade_fills(&bot.db, original.id, 100)
        .await
        .unwrap()
        .is_empty());
    venue.state.lock().unwrap().delay_history_ms = 0;
    venue.fill("after-timeout", 5.0, 0.55, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    assert!(bot.recover_live_orders_or_halt().await);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    let settled = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(settled.status, "CLOSED");
    assert_eq!(settled.exit_price, Some(0.55));
    assert_eq!(
        db::load_trade_fills(&bot.db, original.id, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn taker_read_timeout_preserves_committed_partial_entry_and_restart_deduplicates_receipts() {
    let (bot, original, path, venue) = fixture().await;
    sqlx::query("UPDATE trades SET shares=0,entry_shares=0,entry_cost=0,amount=2.5,fee_paid=0,order_id=NULL WHERE id=?")
        .bind(original.id).execute(&bot.db).await.unwrap();
    let initial = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    *bot.portfolio_state.trades.write().await = vec![initial];
    let clob = bot.clob.as_ref().unwrap();
    let signed = clob
        .create_order(&LimitOrderArgs {
            token_id: "123456789".into(),
            price: 0.5,
            size: 5.0,
            side: Side::Buy,
        })
        .await
        .unwrap();
    let fees = clob.market_fee_parameters("123456789").await.unwrap();
    db::live::prepare_order(
        &bot.db,
        original.id,
        &clob.account_address(),
        &signed,
        0.5,
        fees,
        "entry",
    )
    .await
    .unwrap();
    clob.post_order(&signed, OrderType::FOK).await.unwrap();
    venue.fill("partial-entry", 2.0, 0.49, "CONFIRMED");
    // A confirmed subset on a still nonterminal venue order remains reserved.
    assert!(bot.recover_live_orders_or_halt().await);
    let partial = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(partial.shares, 2.0);
    assert_eq!(
        db::trade_entry_cost(&bot.db, original.id).await.unwrap(),
        Some(0.98)
    );
    assert!(partial.has_order());
    let cash = bot.balance().await;
    venue.state.lock().unwrap().delay_history_ms = 500;
    assert!(
        !bot.recover_live_orders_or_halt_with_read_budget(Duration::from_millis(30))
            .await
    );
    assert_eq!(
        serde_json::json!(db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()),
        serde_json::json!(partial)
    );
    assert_eq!(bot.balance().await, cash);
    // Reopen the fixture ledger as a restart, preserving the exact intent and
    // first fill before accepting a later complete receipt set.
    let reopened = db::open(path.to_str().unwrap()).await.unwrap();
    assert_eq!(db::live::pending_orders(&reopened).await.unwrap().len(), 1);
    assert_eq!(
        db::load_trade_by_id(&reopened, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        2.0
    );
    venue.state.lock().unwrap().delay_history_ms = 0;
    venue.fill("entry-remainder", 3.0, 0.5, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    let order = db::live::pending_orders(&reopened).await.unwrap().remove(0);
    let info = clob.get_order(&order.order_id).await.unwrap();
    let history = clob.list_trades().await.unwrap();
    let verifier = clob.settlement_verifier().unwrap();
    let mut receipts = Vec::new();
    for matched in &history {
        receipts.extend(
            verifier
                .verify(
                    matched.transaction_hash.as_deref().unwrap(),
                    &order.identity,
                )
                .await
                .unwrap(),
        );
    }
    for _ in 0..2 {
        let settled = db::live::reconcile_order(&reopened, &order, &info, &history, &receipts)
            .await
            .unwrap();
        assert_eq!(settled.shares, 5.0);
        assert!(
            (db::trade_entry_cost(&reopened, original.id)
                .await
                .unwrap()
                .unwrap()
                - 2.48)
                .abs()
                < 1e-9
        );
        assert!(!settled.has_order());
    }
    assert_eq!(
        db::load_trade_fills(&reopened, original.id, 100)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    reopened.close().await;
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn an_expired_recovery_budget_never_starts_another_provider_read() {
    let called = std::sync::atomic::AtomicBool::new(false);
    let read = async {
        called.store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    };
    let error = before_recovery_read_deadline(tokio::time::Instant::now(), "fixture", read)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("deadline"));
    assert!(!called.load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test]
async fn successive_recovery_reads_share_one_budget_instead_of_resetting_the_timeout() {
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_millis(80);
    before_recovery_read_deadline(deadline, "first fixture", async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    })
    .await
    .unwrap();
    let error = before_recovery_read_deadline(deadline, "second fixture", async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("second fixture"));
    assert!(started.elapsed() < Duration::from_millis(150));
}
