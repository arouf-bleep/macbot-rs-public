use super::*;
use crate::bot::close::CloseIntent;
use serde_json::json;

async fn fixture(entry_fee: f64) -> anyhow::Result<(TradingBot, db::Trade, db::Trade)> {
    let pool = db::open(":memory:").await?;
    let mut config: crate::config::Config =
        serde_json::from_str(include_str!("../../../../config.json"))?;
    config.mode = "simulation".into();
    config.sim_live_execution = false;
    config.initial_balance = 100.0;
    let bot = TradingBot::new(config, pool.clone()).await?;
    bot.set_balance(100.0 - 2.0 * (4.8 + entry_fee)).await;
    let mut first = db::Trade {
        timestamp: Utc::now().to_rfc3339(),
        market_id: "atomic-paper-pair".into(),
        direction: "BUY_YES".into(),
        status: "OPEN".into(),
        mode: "simulation".into(),
        shares: 12.0,
        amount: 4.8,
        entry_price: 0.4,
        fee_paid: entry_fee,
        partial_closed: Some(0),
        ..db::Trade::default()
    };
    first.id = db::insert_trade(&pool, &first).await?;
    let mut second = first.clone();
    second.id = 0;
    second.direction = "BUY_NO".into();
    second.id = db::insert_trade(&pool, &second).await?;
    *bot.portfolio_state.trades.write().await = vec![first.clone(), second.clone()];
    Ok((bot, first, second))
}

#[tokio::test]
async fn atomic_pair_replays_cannot_pay_again_and_single_merge_is_refused() -> anyhow::Result<()> {
    let (bot, first, second) = fixture(0.0).await?;
    assert!(bot
        .close_trade_with_intent(first.id, Some(0.5), CloseIntent::Merge)
        .await
        .is_none());
    let (one, two) = bot.merge_simulated_pair_direct(first.id, second.id).await?;
    for trade in [one.unwrap(), two.unwrap()] {
        assert_eq!(trade.status, "CLOSED");
        assert_eq!(trade.exit_type.as_deref(), Some("merged"));
        assert!((trade.pnl.unwrap() - 1.2).abs() < 1e-9);
    }
    assert_eq!(bot.balance().await, 102.4);
    assert!(matches!(
        bot.merge_simulated_pair_direct(first.id, second.id).await,
        Err(ActorCallError::PairNotMergeable)
    ));
    assert_eq!(bot.balance().await, 102.4);
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM trade_fills),(SELECT COUNT(*) FROM realized_pnl_events)",
    )
    .fetch_one(&bot.db)
    .await?;
    assert_eq!(counts, (2, 2));
    let daily = db::risk::daily_realized_pnl(&bot.db, Utc::now().date_naive()).await?;
    assert!((daily.exact_today_pnl.unwrap() - 2.4).abs() < 1e-9);
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn either_leg_or_evidence_failure_rolls_back_both_lots_and_cash() -> anyhow::Result<()> {
    for table in ["trades", "trade_fills", "realized_pnl_events"] {
        for failed_leg in [0, 1] {
            let (bot, first, second) = fixture(0.0).await?;
            let failed_id = if failed_leg == 0 { first.id } else { second.id };
            // Table/operation names are fixed test constants; the identifier is
            // the freshly inserted numeric fixture ID, never external input.
            let (operation, key) = if table == "trades" {
                ("UPDATE", "id")
            } else {
                ("INSERT", "trade_id")
            };
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TRIGGER reject_pair BEFORE {operation} ON {table} WHEN NEW.{key}={failed_id} BEGIN SELECT RAISE(FAIL,'fixture pair commit refused'); END"))).execute(&bot.db).await?;
            assert!(
                bot.merge_simulated_pair_direct(first.id, second.id)
                    .await
                    .is_err(),
                "{table} leg{failed_leg}"
            );
            for original in [&first, &second] {
                let persisted = db::load_trade_by_id(&bot.db, original.id).await?.unwrap();
                assert_eq!(json!(persisted), json!(original), "{table} leg{failed_leg}");
            }
            let hot = bot.portfolio_state.trades.read().await.clone();
            assert_eq!(json!(hot), json!([&first, &second]));
            assert!((bot.balance().await - 90.4).abs() < 1e-9);
            let counts: (i64, i64) = sqlx::query_as("SELECT (SELECT COUNT(*) FROM trade_fills),(SELECT COUNT(*) FROM realized_pnl_events)").fetch_one(&bot.db).await?;
            assert_eq!(counts, (0, 0), "{table} leg{failed_leg}");
            assert!(bot.kill_switch_active());
            bot.db.close().await;
        }
    }
    Ok(())
}

#[tokio::test]
async fn partial_realizations_and_remaining_entry_fees_are_allocated_once() -> anyhow::Result<()> {
    let (bot, first, second) = fixture(1.2).await?;
    for original in [&first, &second] {
        let partial = bot
            .partial_close_trade_with_intent(original.id, 0.5, 0.25, CloseIntent::PassiveFill)
            .await
            .unwrap();
        assert_eq!(partial["remaining_shares"], 9.0);
    }
    let before = bot.balance().await;
    assert!((before - 91.0).abs() < 1e-9);
    let (one, two) = bot.merge_simulated_pair_direct(first.id, second.id).await?;
    for closed in [one.unwrap(), two.unwrap()] {
        assert_eq!(closed.shares, 9.0);
        assert_eq!(closed.partial_closed, Some(1));
        assert!((closed.pnl.unwrap() - 1.2).abs() < 1e-9);
        assert!(closed.net_realized_pnl().abs() < 1e-9);
    }
    assert_eq!(bot.balance().await, before + 9.0);
    let daily = db::risk::daily_realized_pnl(&bot.db, Utc::now().date_naive()).await?;
    assert!(daily.evidence_complete);
    assert!(daily.exact_today_pnl.unwrap().abs() < 1e-9);
    let restarted = TradingBot::new(
        bot.control_plane.config.read().await.clone(),
        bot.db.clone(),
    )
    .await?;
    assert!(
        (restarted.balance().await - bot.balance().await).abs() < 1e-9,
        "the committed paper ledger reconstructs the same cash after restart"
    );
    assert!(matches!(
        restarted
            .merge_simulated_pair_direct(first.id, second.id)
            .await,
        Err(ActorCallError::PairNotMergeable)
    ));
    assert!((restarted.balance().await - 100.0).abs() < 1e-9);
    for id in [first.id, second.id] {
        let (events, realized_shares, net): (i64, f64, f64) = sqlx::query_as("SELECT COUNT(*),SUM(realized_shares),SUM(net_pnl) FROM realized_pnl_events WHERE trade_id=?").bind(id).fetch_one(&bot.db).await?;
        assert_eq!(events, 2);
        assert_eq!(realized_shares, 12.0);
        assert!(net.abs() < 1e-9);
    }
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn paper_exits_cannot_credit_live_or_wallet_backed_cash() -> anyhow::Result<()> {
    for wallet_backed in [false, true] {
        let (mut bot, first, second) = fixture(0.0).await?;
        if wallet_backed {
            // Public inert fixture identity; this test performs no network call.
            let auth = crate::clob::ClobAuth::new(
                "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "public-api-key",
                "cHVibGljLWZpeHR1cmUtc2VjcmV0",
                "public-passphrase",
            )?;
            bot.clob = Some(std::sync::Arc::new(
                crate::clob::ClobClient::with_test_host(auth, "http://127.0.0.1:9".into()),
            ));
        } else {
            bot.control_plane.config.write().await.mode = "live".into();
        }
        let before = bot.balance().await;
        for intent in [
            CloseIntent::Market,
            CloseIntent::Manual,
            CloseIntent::PassiveFill,
            CloseIntent::Resolution,
            CloseIntent::ShutdownMark,
            CloseIntent::Merge,
        ] {
            assert!(bot
                .close_trade_with_intent(first.id, Some(0.5), intent)
                .await
                .is_none());
        }
        for intent in [CloseIntent::Market, CloseIntent::Manual] {
            let result = bot
                .partial_close_trade_with_intent(first.id, 0.5, 0.5, intent)
                .await
                .unwrap();
            assert_eq!(result["status"], "rejected");
            assert!(result["reason"].as_str().unwrap().contains("wallet-backed"));
        }
        assert!(matches!(
            bot.merge_simulated_pair_direct(first.id, second.id).await,
            Err(ActorCallError::PairNotMergeable)
        ));
        bot.control_plane.config.write().await.auto_merge_rebalancer = true;
        assert_eq!(bot.rebalance_offsetting_pairs().await?, 0);
        if !wallet_backed {
            let bot = std::sync::Arc::new(bot);
            bot.start_execution_actor().await;
            assert!(bot
                .close_trade_serialized_with_intent(first.id, Some(0.5), CloseIntent::Manual)
                .await?
                .is_none());
            let partial = bot
                .partial_close_trade_serialized_with_intent(first.id, 0.5, 0.5, CloseIntent::Manual)
                .await?
                .unwrap();
            assert_eq!(partial["status"], "rejected");
            assert!(bot
                .trade_lifecycle
                .partial_close_intents
                .read()
                .await
                .is_empty());
            assert!(
                db::maker::stood_down_markets(&bot.db).await?.is_empty(),
                "mode refusal precedes maker cancellation/stand-down changes"
            );
            bot.stop_execution_actor().await?;
            verify_unmodified(&bot, &first, &second, before).await?;
            bot.db.close().await;
        } else {
            verify_unmodified(&bot, &first, &second, before).await?;
            bot.db.close().await;
        }
    }
    Ok(())
}

async fn verify_unmodified(
    bot: &TradingBot,
    first: &db::Trade,
    second: &db::Trade,
    balance: f64,
) -> anyhow::Result<()> {
    for original in [first, second] {
        assert_eq!(
            json!(db::load_trade_by_id(&bot.db, original.id).await?.unwrap()),
            json!(original)
        );
    }
    assert_eq!(
        json!(*bot.portfolio_state.trades.read().await),
        json!([first, second])
    );
    assert_eq!(bot.balance().await, balance);
    assert!(bot
        .trade_lifecycle
        .closing_in_progress
        .read()
        .await
        .is_empty());
    assert!(bot
        .trade_lifecycle
        .pending_sim_exit_evidence
        .read()
        .await
        .is_empty());
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM trade_fills),(SELECT COUNT(*) FROM realized_pnl_events)",
    )
    .fetch_one(&bot.db)
    .await?;
    assert_eq!(counts, (0, 0));
    Ok(())
}

#[tokio::test]
async fn config_origin_cannot_change_during_blocked_pair_commit_or_cash_publication(
) -> anyhow::Result<()> {
    let (bot, first, second) = fixture(0.0).await?;
    let bot = std::sync::Arc::new(bot);
    // Hold the SQL writer while merge reaches its accounting transaction.
    let held_writer = bot.db.begin_with("BEGIN IMMEDIATE").await?;
    let merge_bot = bot.clone();
    let merge = tokio::spawn(async move {
        merge_bot
            .merge_simulated_pair_direct(first.id, second.id)
            .await
    });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    while bot.control_plane.config.try_write().is_ok() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "pair merge must retain its accounting-origin lease while SQL is blocked"
        );
        tokio::task::yield_now().await;
    }
    assert!(
        !merge.is_finished(),
        "the SQL writer should still block settlement"
    );
    let write_bot = bot.clone();
    let writer = tokio::spawn(async move {
        let mut cfg = write_bot.control_plane.config.write().await;
        // The mode writer must observe coherent cash only after publication.
        let cash = write_bot.balance().await;
        cfg.mode = "live".into();
        cash
    });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !writer.is_finished(),
        "mode must not change ahead of pair cash publication"
    );
    held_writer.rollback().await?;
    let result = merge.await??;
    assert_eq!(result.0.unwrap().status, "CLOSED");
    assert_eq!(result.1.unwrap().status, "CLOSED");
    assert!((writer.await? - 102.4).abs() < 1e-9);
    assert!((bot.balance().await - 102.4).abs() < 1e-9);
    assert_eq!(bot.control_plane.config.read().await.mode, "live");
    bot.db.close().await;
    Ok(())
}
