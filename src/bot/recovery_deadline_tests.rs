use super::maker_live_tests::fixture;
use super::*;

#[tokio::test]
async fn maker_recovery_deadline_preserves_resting_reservations_and_freezes_controls() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let quote = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    let mut state = MakerActorState::default();
    state.upsert_quote(
        quote.clone(),
        "exit-market".into(),
        "123456789".into(),
        QuoteSide::Ask,
        0.6,
        5.0,
    );
    let cash = bot.balance().await;
    venue.state.lock().unwrap().delay_history_ms = 500;
    let started = tokio::time::Instant::now();
    let error =
        reconcile_maker_orders_with_read_budget(&bot, &mut state, Duration::from_millis(30))
            .await
            .unwrap_err();
    assert!(error.to_string().contains("deadline"));
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(!state.live_accounting_ready);
    assert!(bot.kill_switch_active());
    assert!(bot.degraded_no_trade_active());
    assert!(bot
        .kill_switch_status()
        .1
        .unwrap()
        .detail
        .contains("deadline"));
    assert_eq!(state.quotes.len(), 1);
    let pending = db::maker::pending_orders(&bot.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].order_id, quote);
    assert_eq!(pending[0].confirmed_size, 0.0);
    assert_eq!(
        db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        5.0
    );
    assert!(db::load_trade_fills(&bot.db, original.id, 100)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(bot.balance().await, cash);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn maker_recovery_timeout_retains_partial_receipt_accounting_across_restart() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let quote = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    venue.fill("maker-first", 2.0, 0.6, "CONFIRMED");
    let mut state = MakerActorState::default();
    reconcile_maker_orders(&bot, &mut state).await.unwrap();
    let partial = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(partial.shares, 3.0);
    assert!((partial.pnl.unwrap() - 0.4).abs() < 1e-9);
    assert_eq!(
        db::maker::recorded_matches(&bot.db, &quote)
            .await
            .unwrap()
            .len(),
        1
    );
    let cash = bot.balance().await;
    venue.state.lock().unwrap().delay_history_ms = 500;
    assert!(
        reconcile_maker_orders_with_read_budget(&bot, &mut state, Duration::from_millis(30))
            .await
            .is_err()
    );
    assert_eq!(
        json!(db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()),
        json!(partial)
    );
    assert_eq!(bot.balance().await, cash);
    let reopened = db::open(path.to_str().unwrap()).await.unwrap();
    assert_eq!(
        db::maker::pending_orders(&reopened).await.unwrap()[0].confirmed_size,
        2.0
    );
    assert_eq!(
        db::load_trade_by_id(&reopened, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        3.0
    );
    reopened.close().await;
    venue.state.lock().unwrap().delay_history_ms = 0;
    // Replaying the same durable receipt after reopening is a no-op. Only new
    // evidence may sell the remaining shares, never a replacement submission.
    let mut restarted_state = MakerActorState::default();
    reconcile_maker_orders(&bot, &mut restarted_state)
        .await
        .unwrap();
    assert_eq!(
        db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        3.0
    );
    venue.fill("maker-last", 3.0, 0.6, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    for _ in 0..2 {
        reconcile_maker_orders(&bot, &mut restarted_state)
            .await
            .unwrap();
        let closed = db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(closed.status, "CLOSED");
        assert!((closed.pnl.unwrap() - 1.0).abs() < 1e-9);
    }
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert_eq!(
        db::maker::recorded_matches(&bot.db, &quote)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        db::load_trade_fills(&bot.db, original.id, 100)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}
