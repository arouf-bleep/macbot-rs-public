//! Regression tests for operator controls, risk admission and resting-order cleanup.
//! These tests call the actual production actor and offline venue fixture.
use super::*;

async fn automatic_entry_fixture() -> Arc<TradingBot> {
    let mut cfg: Config = serde_json::from_str(include_str!("../../config.json")).unwrap();
    cfg.mode = "simulation".into();
    cfg.execution_mode = "taker".into();
    cfg.auto_trading = true;
    cfg.initial_balance = 100.0;
    cfg.shadow_trading = false;
    cfg.shadow_live_parity = false;
    cfg.sim_live_execution = false;
    cfg.macro_event_guard_enabled = false;
    cfg.regime_allowlist.clear();
    cfg.session_allowlist.clear();
    cfg.blocked_hours_utc.clear();
    cfg.scalp_enabled = false;
    cfg.drawdown_halt_daily_pct = 0.0;
    cfg.drawdown_halt_ath_pct = 0.0;
    let bot = Arc::new(
        TradingBot::new(cfg, db::open(":memory:").await.unwrap())
            .await
            .unwrap(),
    );
    bot.start_loop();
    bot.mark_analysis_fresh().await;
    bot
}

fn automatic_analysis() -> ParsedAnalysis {
    ParsedAnalysis::from_value(&json!({
        "signal":"BUY_YES", "forced":false, "confidence":0.95,
        "raw_confidence":0.95, "p_up":0.95, "net_ev":0.8,
        "regime":"TREND_UP", "btc_change_pct":1.0,
        "market_data":{"market_id":"queued-automatic-entry", "price":0.5,
            "liquidity":100000.0,
            "end_date":(Utc::now()+chrono::Duration::minutes(15)).to_rfc3339()}
    }))
}

async fn dispatch_entry(bot: &Arc<TradingBot>, origin: ExecutionOrigin) -> Option<Trade> {
    let (resp, receiver) = oneshot::channel();
    let (outbox, _events) = mpsc::channel(32);
    let mut maker = MakerActorState::default();
    process_execution_command(
        ExecutionCommand::ExecuteTrade {
            origin,
            analysis: Box::new(automatic_analysis()),
            hail_mary: false,
            override_amount: Some(5.0),
            strategy: "conviction".into(),
            resp,
        },
        bot,
        &mut maker,
        &outbox,
    )
    .await;
    receiver.await.unwrap()
}

#[tokio::test]
async fn adversarial_queued_automatic_entry_respects_loop_stop_at_dispatch() {
    // Positive control: exactly the same non-forced actor route and risk gates
    // can open a trade while the loop is running.
    let running = automatic_entry_fixture().await;
    assert!(
        dispatch_entry(&running, running.automatic_entry_origin())
            .await
            .is_some(),
        "positive control must admit"
    );
    running.db.close().await;

    let stopped = automatic_entry_fixture().await;
    let queued_origin = stopped.automatic_entry_origin();
    stopped.stop_loop();
    assert!(!stopped.loop_running());
    assert!(
        !stopped.kill_switch_active(),
        "loop stop is a deliberate pause, not a fault"
    );
    let cash = stopped.balance().await;
    let opened = dispatch_entry(&stopped, queued_origin).await;
    assert!(
        opened.is_none(),
        "a previously queued AUTOMATIC command opened a new trade after stop_loop"
    );
    assert_eq!(stopped.balance().await, cash);
    assert!(db::load_open_trades(&stopped.db).await.unwrap().is_empty());
    stopped.db.close().await;
}

#[tokio::test]
async fn adversarial_queued_automatic_entry_respects_current_auto_trading_off() {
    let running = automatic_entry_fixture().await;
    assert!(
        dispatch_entry(&running, running.automatic_entry_origin())
            .await
            .is_some(),
        "positive control must admit"
    );
    running.db.close().await;
    let disabled = automatic_entry_fixture().await;
    let queued_origin = disabled.automatic_entry_origin();
    disabled.control_plane.config.write().await.auto_trading = false;
    let opened = dispatch_entry(&disabled, queued_origin).await;
    assert!(
        opened.is_none(),
        "a previously queued AUTOMATIC command ignored current auto_trading=false"
    );
    disabled.db.close().await;
}

async fn flat_live_bid_fixture() -> (
    Arc<TradingBot>,
    MakerActorState,
    String,
    std::path::PathBuf,
    maker_live_tests::MockMakerVenue,
) {
    let (bot, original, path, venue) = maker_live_tests::fixture().await;
    // The reusable venue fixture starts with an inventory lot for sale tests.
    // Remove that fixture-only lot before accepting a new resting BID so the
    // shutdown case is genuinely flat and does not depend on a failed sale.
    sqlx::query("DELETE FROM trades WHERE id=?")
        .bind(original.id)
        .execute(&bot.db)
        .await
        .unwrap();
    bot.portfolio_state.trades.write().await.clear();
    {
        let mut cfg = bot.control_plane.config.write().await;
        cfg.execution_mode = "maker".into();
        cfg.auto_trading = true;
        cfg.macro_event_guard_enabled = false;
        cfg.min_trade_usd = 1.0;
        cfg.max_open_positions = 5;
        cfg.max_exposure_pct = 0.0;
    }
    *bot.analysis_state.last_analysis.write().await = json!({
        "signal":"BUY_YES", "p_up":0.95, "confidence":0.95,
        "net_ev":0.8, "regime":"TREND_UP",
        "market_data":{"market_id":"exit-market","price":0.5,"liquidity":100000.0,
            "end_date":(Utc::now()+chrono::Duration::minutes(15)).to_rfc3339()}
    });
    bot.mark_analysis_fresh().await;
    bot.start_loop();
    bot.mark_l2_feed_event().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Bid,
        0.4,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .expect("offline venue must accept a resting BID");
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(venue.state.lock().unwrap().status, "LIVE");
    let persisted = db::maker::pending_orders(&bot.db).await.unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].state, "LIVE");
    assert_eq!(persisted[0].side, "BUY");
    assert!(db::load_open_trades(&bot.db).await.unwrap().is_empty());
    let mut state = MakerActorState::default();
    state.last_live_reconcile = Some(Instant::now());
    state.live_accounting_ready = true;
    state.upsert_quote(
        order.clone(),
        "exit-market".into(),
        "123456789".into(),
        QuoteSide::Bid,
        0.4,
        5.0,
    );
    (Arc::new(bot), state, order, path, venue)
}

fn current_book() -> TopOfBook {
    TopOfBook {
        market_id: "exit-market".into(),
        token_id: "123456789".into(),
        is_yes_token: Some(true),
        best_bid: Some(0.49),
        best_bid_size: Some(20.0),
        best_ask: Some(0.61),
        best_ask_size: Some(20.0),
        spread_bps: Some(1200.0),
        sequence: Some(1),
        snapshot_loaded: true,
        stale: false,
        updated_at: Utc::now().to_rfc3339(),
    }
}

#[tokio::test]
async fn adversarial_maker_to_taker_switch_cancels_accepted_resting_bid() {
    let (bot, mut state, order, path, venue) = flat_live_bid_fixture().await;
    bot.control_plane.config.write().await.execution_mode = "taker".into();
    let (outbox, _events) = mpsc::channel(32);
    let book = current_book();
    state.update_bbo(book.clone());
    process_maker_bbo_event(&bot, &mut state, &book, &outbox).await;
    assert!(
        !state.quotes.contains_key(&order),
        "maker→taker accepted transition left an existing resting BID in actor state"
    );
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert_eq!(venue.state.lock().unwrap().status, "CANCELED");
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn adversarial_flat_shutdown_cannot_report_clean_with_accepted_resting_bid() {
    let (bot, _state, order, path, venue) = flat_live_bid_fixture().await;
    bot.start_execution_actor().await;
    let snapshot = bot.maker_state_snapshot().await;
    assert!(
        snapshot["quotes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|quote| quote["order_id"] == order),
        "actual actor must restore the accepted durable quote before shutdown"
    );
    // Mirror main's actual order: shutdown watch, HTTP drain (none in this
    // fixture), position sweep, execution actor drain. No book arrives during
    // this ordinary idle interval; no cancellation has failed.
    bot.runtime_state.begin_shutdown();
    let outcome = bot.shutdown_close_open_positions().await;
    bot.stop_execution_actor()
        .await
        .expect("actual actor must drain normally");
    let pending = db::maker::pending_orders(&bot.db).await.unwrap();
    // Accept either an authoritative cancellation or an explicitly non-clean
    // result. A successful-flat assertion while the venue can still fill is
    // the defect; this test does not require a cancellation failure/stale feed.
    assert!(
        pending.is_empty() && outcome.is_clean(),
        "clean shutdown reported while accepted maker BID {order} remains {:?}; venue status={}",
        pending
            .iter()
            .map(|entry| (&entry.order_id, &entry.state))
            .collect::<Vec<_>>(),
        venue.state.lock().unwrap().status
    );
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn execution_controls_stop_restart_and_disable_enable_never_revive_queued_generation() {
    for disable in [false, true] {
        let bot = automatic_entry_fixture().await;
        let old = bot.automatic_entry_origin();
        if disable {
            let mut cfg = bot.control_plane.config.write().await;
            bot.invalidate_automatic_entries();
            cfg.auto_trading = false;
            cfg.auto_trading = true;
        } else {
            bot.stop_loop();
            bot.start_loop();
        }
        assert!(dispatch_entry(&bot, old).await.is_none());
        assert!(db::load_open_trades(&bot.db).await.unwrap().is_empty());
        assert!(
            dispatch_entry(&bot, bot.automatic_entry_origin())
                .await
                .is_some(),
            "newly admitted work must remain usable"
        );
        bot.db.close().await;
    }
}

#[tokio::test]
async fn execution_controls_manual_nonforced_entry_survives_deliberate_stop_and_disable() {
    let bot = automatic_entry_fixture().await;
    bot.stop_loop();
    bot.control_plane.config.write().await.auto_trading = false;
    assert!(
        dispatch_entry(&bot, ExecutionOrigin::Manual)
            .await
            .is_some(),
        "manual origin is independent of analysis.forced"
    );
    bot.db.close().await;
}

#[tokio::test]
async fn execution_controls_manual_entry_respects_percentage_loss_while_loop_is_paused() {
    let bot = automatic_entry_fixture().await;
    record_percentage_loss(&bot, "simulation").await;
    bot.stop_loop();
    bot.control_plane.config.write().await.auto_trading = false;
    let cfg = bot.control_plane.config.read().await.clone();
    assert_eq!(cfg.daily_loss_limit, 100.0);
    assert_eq!(cfg.drawdown_halt_daily_pct, 10.0);
    assert!((bot.get_today_pnl_for_mode(BotMode::Simulation).await + 15.0).abs() < 1e-9);
    let cash = bot.balance().await;
    assert!(
        dispatch_entry(&bot, ExecutionOrigin::Manual)
            .await
            .is_none(),
        "manual control cannot bypass the 10% loss limit after a 15% loss"
    );
    assert_eq!(bot.balance().await, cash);
    assert!(db::load_open_trades(&bot.db).await.unwrap().is_empty());
    assert_eq!(db::load_all_trades(&bot.db).await.unwrap().len(), 1);
    assert!(
        !bot.kill_switch_active(),
        "a financial admission refusal is not a fault"
    );

    // Keep the same loss history, stopped loop and disabled automatic setting.
    // Removing only the percentage cap admits the same deliberate manual trade.
    {
        let mut cfg = bot.control_plane.config.write().await;
        cfg.drawdown_halt_daily_pct = 0.0;
        cfg.drawdown_halt_ath_pct = 0.0;
    }
    assert!(
        dispatch_entry(&bot, ExecutionOrigin::Manual)
            .await
            .is_some(),
        "manual pause exception remains valid when financial admission permits it"
    );
    assert_eq!(db::load_open_trades(&bot.db).await.unwrap().len(), 1);
    assert!(bot.balance().await < cash);
    assert!(!bot.loop_running());
    assert!(!bot.control_plane.config.read().await.auto_trading);
    bot.db.close().await;
}

#[tokio::test]
async fn execution_controls_final_backend_gate_rejects_invalidated_work_after_lock_wait() {
    let bot = automatic_entry_fixture().await;
    let origin = bot.automatic_entry_origin();
    let guard = bot.execution_state.live_execution_lock.lock().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let notice = entered.clone();
    let worker = bot.clone();
    let task = tokio::spawn(async move {
        notice.notify_one();
        worker
            .execute_trade_strategy_for_origin(
                &automatic_analysis(),
                false,
                Some(5.0),
                "conviction",
                origin,
            )
            .await
    });
    entered.notified().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        !task.is_finished(),
        "backend must be parked behind the actual execution lock"
    );
    bot.stop_loop();
    bot.start_loop();
    drop(guard);
    assert!(task.await.unwrap().is_none());
    assert_eq!(bot.balance().await, 100.0);
    assert!(db::load_open_trades(&bot.db).await.unwrap().is_empty());
    bot.db.close().await;
}

#[tokio::test]
async fn execution_controls_idle_maintenance_retries_failed_stale_cancel_without_feed_change() {
    let (bot, mut state, order, path, venue) = flat_live_bid_fixture().await;
    venue.state.lock().unwrap().cancel_failures_remaining = 1;
    bot.set_l2_feed_stale(true).await;
    let (outbox, _events) = mpsc::channel(32);
    process_execution_command(
        ExecutionCommand::SetL2Stale {
            stale: true,
            code: Some("fixture".into()),
            reason: Some("no new book".into()),
        },
        &bot,
        &mut state,
        &outbox,
    )
    .await;
    assert_eq!(venue.state.lock().unwrap().cancels, 1);
    assert!(state.quotes.contains_key(&order));
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "CANCEL_REQUESTED"
    );
    assert!(bot.kill_switch_active());
    // This is the exact timer maintenance function, with no SetL2Stale/BBO
    // transition between the failed DELETE and its independent retry.
    maintain_quote_lifecycle(&bot, &mut state, &outbox).await;
    assert_eq!(venue.state.lock().unwrap().cancels, 2);
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "cleanup cannot resubmit an entry"
    );
    assert!(state.quotes.is_empty());
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(db::load_open_trades(&bot.db).await.unwrap().is_empty());
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn execution_controls_shutdown_failure_retains_reservation_and_is_not_clean() {
    let (bot, _state, order, path, venue) = flat_live_bid_fixture().await;
    venue.state.lock().unwrap().cancel_failures_remaining = 100;
    bot.start_execution_actor().await;
    bot.runtime_state.begin_shutdown();
    let outcome = bot.shutdown_close_open_positions().await;
    assert!(!outcome.is_clean());
    assert!(outcome
        .remaining_orders
        .iter()
        .any(|id| id == &format!("maker:{order}")));
    assert!(outcome.order_cleanup_error.is_some());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(venue.state.lock().unwrap().status, "LIVE");
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "CANCEL_REQUESTED"
    );
    bot.stop_execution_actor().await.unwrap();
    bot.verify_shutdown_exposure(&mut outcome.clone()).await;
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

async fn record_percentage_loss(bot: &TradingBot, mode: &str) {
    let now = Utc::now().to_rfc3339();
    let loss = Trade {
        timestamp: now.clone(),
        exit_timestamp: Some(now),
        market_id: "prior-percentage-loss".into(),
        direction: "BUY_YES".into(),
        entry_price: 0.5,
        amount: 20.0,
        shares: 0.0,
        exit_price: Some(0.125),
        pnl: Some(-15.0),
        status: "CLOSED".into(),
        mode: mode.into(),
        strategy: "maker".into(),
        fee_paid: 0.0,
        accounting_version: 0,
        partial_closed: Some(0),
        ..Trade::default()
    };
    let id = db::insert_trade(&bot.db, &loss).await.unwrap();
    sqlx::query("UPDATE trades SET entry_cost=20.0,entry_shares=40.0 WHERE id=?")
        .bind(id)
        .execute(&bot.db)
        .await
        .unwrap();
    let mut cfg = bot.control_plane.config.write().await;
    cfg.initial_balance = 100.0;
    cfg.daily_loss_limit = 100.0;
    cfg.drawdown_halt_daily_pct = 10.0;
    cfg.drawdown_halt_ath_pct = 0.0;
}

#[tokio::test]
async fn execution_controls_percentage_drawdown_blocks_live_and_paper_maker_admission_and_paper_fill(
) {
    let bot = automatic_entry_fixture().await;
    {
        let mut cfg = bot.control_plane.config.write().await;
        cfg.execution_mode = "maker".into();
        cfg.min_trade_usd = 1.0;
        cfg.max_exposure_pct = 0.0;
    }
    *bot.analysis_state.last_analysis.write().await = json!({
        "signal":"BUY_YES", "forced":false, "confidence":0.95, "raw_confidence":0.95,
        "p_up":0.95, "net_ev":0.8, "regime":"TREND_UP", "btc_change_pct":1.0,
        "market_data":{"market_id":"queued-automatic-entry","price":0.5,"liquidity":100000.0,
            "end_date":(Utc::now()+chrono::Duration::minutes(15)).to_rfc3339()}
    });
    let cfg = bot.control_plane.config.read().await.clone();
    let order = "sim-mkr-dd-fixture";
    admit_simulated_maker_order(
        &bot,
        &cfg,
        "queued-automatic-entry",
        "yes-token",
        Some(true),
        0.4,
        5.0,
        order,
    )
    .await
    .unwrap();
    record_percentage_loss(&bot, "simulation").await;
    assert!(
        -bot.get_today_pnl().await < bot.control_plane.config.read().await.daily_loss_limit,
        "absolute daily gate must remain below its threshold, isolating percentage DD"
    );
    assert!(bot.is_drawdown_halted().await);
    let cfg = bot.control_plane.config.read().await.clone();
    assert!(evaluate_maker_entry(
        &bot,
        &cfg,
        "queued-automatic-entry",
        Direction::BuyYes,
        0.4,
        2.0,
        None
    )
    .await
    .is_err());
    assert!(admit_simulated_maker_order(
        &bot,
        &cfg,
        "queued-automatic-entry",
        "yes-token",
        Some(true),
        0.4,
        5.0,
        "sim-mkr-new"
    )
    .await
    .is_err());
    let cash = bot.balance().await;
    assert!(
        open_simulated_maker_trade(&bot, &cfg, order, "new-observation", 0.4, 5.0, "fixture")
            .await
            .is_none()
    );
    assert_eq!(bot.balance().await, cash);
    let fills: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_maker_fill_events")
        .fetch_one(&bot.db)
        .await
        .unwrap();
    assert_eq!(fills, 0);
    assert!(db::load_open_trades(&bot.db).await.unwrap().is_empty());
    bot.db.close().await;

    let (live, original, path, venue) = maker_live_tests::fixture().await;
    sqlx::query("DELETE FROM trades WHERE id=?")
        .bind(original.id)
        .execute(&live.db)
        .await
        .unwrap();
    live.portfolio_state.trades.write().await.clear();
    record_percentage_loss(&live, "live").await;
    let cfg = live.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    assert!(place_quote(
        &live,
        "exit-market",
        "123456789",
        &QuoteSide::Bid,
        0.4,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox
    )
    .await
    .is_none());
    assert_eq!(venue.state.lock().unwrap().posts, 0);
    assert!(db::maker::pending_orders(&live.db)
        .await
        .unwrap()
        .is_empty());
    crate::bot::close::tests::cleanup_exit_fixture(&live, &path).await;
}

#[tokio::test]
async fn execution_controls_maker_generation_rechecked_after_authenticated_balance_wait() {
    let (bot, mut state, order, path, venue) = flat_live_bid_fixture().await;
    let (outbox, _events) = mpsc::channel(32);
    assert!(
        cancel_quote(
            &bot,
            &order,
            "exit-market",
            "123456789",
            "fixture removal",
            &outbox
        )
        .await
    );
    state.remove_quote(&order);
    let reads_before = venue.state.lock().unwrap().balance_reads;
    venue.state.lock().unwrap().delay_balance_ms = 200;
    let cfg = bot.control_plane.config.read().await.clone();
    let origin = bot.automatic_entry_origin();
    let worker = bot.clone();
    let task = tokio::spawn(async move {
        let (outbox, _events) = mpsc::channel(32);
        place_quote_for_origin(
            &worker,
            "exit-market",
            "123456789",
            &QuoteSide::Bid,
            0.4,
            5.0,
            &cfg,
            Some(true),
            2,
            &outbox,
            origin,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while venue.state.lock().unwrap().balance_reads == reads_before {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !task.is_finished(),
        "the actual authenticated balance read must still be pending"
    );
    bot.stop_loop();
    bot.start_loop();
    assert!(task.await.unwrap().is_none());
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "old quote is canceled; queued replacement cannot POST after Stop/restart"
    );
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn execution_controls_percentage_drawdown_cancels_standing_bid_without_liquidating_holdings()
{
    let (bot, mut state, _order, path, venue) = flat_live_bid_fixture().await;
    record_percentage_loss(&bot, "live").await;
    // Preserve an unrelated exiting quote in actor memory: percentage entry
    // protection targets BUYs rather than deleting normal exit policies.
    state.upsert_quote(
        "sim-mkr-existing-ask".into(),
        "held-market".into(),
        "held-token".into(),
        QuoteSide::Ask,
        0.6,
        5.0,
    );
    let (outbox, _events) = mpsc::channel(32);
    maintain_quote_lifecycle(&bot, &mut state, &outbox).await;
    assert_eq!(venue.state.lock().unwrap().cancels, 1);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(state.quotes.contains_key("sim-mkr-existing-ask"));
    assert_eq!(state.quotes.len(), 1);
    assert!(
        !bot.kill_switch_active(),
        "verified percentage cancellation is a deliberate entry restriction"
    );
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn execution_controls_actor_timer_retries_failed_cancel_while_feed_remains_idle() {
    let (bot, _state, order, path, venue) = flat_live_bid_fixture().await;
    bot.start_execution_actor().await;
    let restored = bot.maker_state_snapshot().await;
    assert!(restored["quotes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|quote| quote["order_id"] == order));
    venue.state.lock().unwrap().cancel_failures_remaining = 1;
    // Only the actor command changes the stale state; the timer cannot consume
    // an externally changed flag before that first cancellation command.
    assert!(
        bot.maker_publish_l2_stale(true, Some("fixture idle feed".into()))
            .await
    );
    // The snapshot is a medium-lane barrier behind the high-priority stale
    // command. Idle maintenance may already have completed the retry before
    // this barrier arrives; that is valid progress, not a test failure. The
    // direct maintenance test separately proves reservation retention after
    // the first failed DELETE.
    let _ = bot.maker_state_snapshot().await;
    assert!((1..=2).contains(&venue.state.lock().unwrap().cancels));
    tokio::time::timeout(Duration::from_secs(4), async {
        while !db::maker::pending_orders(&bot.db).await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actor's timer must retry without another stale command or BBO");
    let settled = bot.maker_state_snapshot().await;
    assert!(!settled["quotes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|quote| quote["order_id"] == order));
    assert_eq!(venue.state.lock().unwrap().cancels, 2);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(db::load_open_trades(&bot.db).await.unwrap().is_empty());
    bot.stop_execution_actor().await.unwrap();
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn execution_controls_drawdown_cancellation_still_books_prior_confirmed_live_fill() {
    let (bot, mut state, order, path, venue) = flat_live_bid_fixture().await;
    record_percentage_loss(&bot, "live").await;
    venue.fill("prior-accepted-buy", 5.0, 0.4, "CONFIRMED");
    let (outbox, _events) = mpsc::channel(32);
    maintain_quote_lifecycle(&bot, &mut state, &outbox).await;
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(!state.quotes.contains_key(&order));
    let trades = db::load_open_trades(&bot.db).await.unwrap();
    assert_eq!(
        trades.len(),
        1,
        "risk admission cannot erase an actual receipt-backed buy"
    );
    assert!(trades[0].is_live());
    assert!((trades[0].shares - 5.0).abs() < 1e-9);
    assert!((trades[0].amount - 2.0).abs() < 1e-9);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn execution_controls_cancel_read_deadline_retains_intent_until_a_later_verified_retry() {
    let (bot, mut state, order, path, venue) = flat_live_bid_fixture().await;
    venue.state.lock().unwrap().delay_history_ms = 250;
    let (outbox, _events) = mpsc::channel(32);
    let start = Instant::now();
    assert!(
        !cancel_quote_before(
            &bot,
            &order,
            "exit-market",
            "123456789",
            "fixture bounded cancellation",
            &outbox,
            tokio::time::Instant::now() + Duration::from_millis(100)
        )
        .await
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(venue.state.lock().unwrap().status, "CANCELED");
    assert!(state.quotes.contains_key(&order));
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "CANCEL_REQUESTED",
        "an acknowledgement without complete fill history cannot release inventory reservations"
    );
    venue.state.lock().unwrap().delay_history_ms = 0;
    maintain_quote_lifecycle(&bot, &mut state, &outbox).await;
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(state.quotes.is_empty());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}
