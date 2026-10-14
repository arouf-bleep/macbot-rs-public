//! Operational acceptance with open positions.
//!
//! The project's own limitations doc records that the multi-open shutdown proof
//! was never completed: the live attempts landed in dead windows with zero
//! target markets and zero trades, so nothing was ever open to prove anything
//! about. These tests construct the open positions deterministically instead,
//! so the lifecycle is exercised on every run rather than when a market happens
//! to cooperate.
//!
//! Covered: multiple simultaneous open positions, partial fills replayed from
//! the fill ledger, restart recovery, the shutdown sweep, and accounting
//! verification afterwards.
//!
//! Not covered, and deliberately so: `reconcile_live_state` returns early
//! outside live mode, so exchange-truth reconciliation cannot be exercised
//! here. That still requires a funded live run.

use std::sync::Arc;

const CRASH_SEED_DB_ENV: &str = "MACBOT_ACCEPTANCE_CRASH_SEED_DB";

use macbot_rs::bot::state::TradingBot;
use macbot_rs::config::Config;
use macbot_rs::db;
use serde_json::json;

struct Harness {
    path: std::path::PathBuf,
    path_str: String,
}

impl Harness {
    fn new(label: &str) -> anyhow::Result<Self> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_accept_{label}_{uniq}.sqlite"));
        Ok(Self {
            path_str: path.to_string_lossy().to_string(),
            path,
        })
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(format!("{}-shm", self.path_str));
        let _ = std::fs::remove_file(format!("{}-wal", self.path_str));
    }
}

fn acceptance_config() -> anyhow::Result<Config> {
    let mut cfg: Config = serde_json::from_str(include_str!("../config.json"))?;
    cfg.mode = "simulation".to_string();
    cfg.execution_mode = "taker".to_string();
    cfg.shadow_trading = false;
    cfg.sim_live_execution = false;
    cfg.auto_trading = true;
    cfg.min_entry_price = 0.2;
    cfg.max_entry_price = 0.9;
    cfg.min_ev = 0.0;
    cfg.edge_safety_buffer = 0.0;
    cfg.trade_gas_usd = 0.0;
    // Several positions open at once is the entire point of these tests.
    cfg.max_open_positions = 8;
    cfg.max_exposure_pct = 0.95;
    Ok(cfg)
}

async fn boot(harness: &Harness) -> anyhow::Result<Arc<TradingBot>> {
    let pool = db::open(&harness.path_str).await?;
    let bot = Arc::new(TradingBot::new(acceptance_config()?, pool).await?);
    bot.start_execution_actor().await;
    Ok(bot)
}

fn buy_yes(market_id: &str, price: f64) -> serde_json::Value {
    json!({
        "signal": "BUY_YES",
        "confidence": 0.95,
        "raw_confidence": 0.95,
        "net_ev": 0.20,
        "forced": true,
        "reason": "operational-acceptance",
        "regime": "BULL",
        "p_up": 0.71,
        "btc_change_pct": 1.2,
        "market_data": {
            "market_id": market_id,
            "question": format!("acceptance-{market_id}"),
            "price": price,
            "end_date": "2099-01-01T00:00:00Z",
            "liquidity": 20000.0
        },
        "signals": {"orderbook_imbalance": 0.6}
    })
}

async fn open_positions(bot: &Arc<TradingBot>, label: &str, count: usize) -> Vec<i64> {
    let mut ids = Vec::new();
    for index in 0..count {
        let analysis = buy_yes(
            &format!("mkt-{label}-{index}"),
            0.40 + (index as f64) * 0.05,
        );
        let opened = bot
            .execute_trade_serialized_result(&analysis, false, Some(1.0))
            .await
            .expect("actor call should succeed")
            .expect("trade should open");
        assert!(opened.is_open(), "trade {} should be open", opened.id);
        ids.push(opened.id);
    }
    ids.sort_unstable();
    ids
}

async fn open_ids_in_db(bot: &Arc<TradingBot>) -> anyhow::Result<Vec<i64>> {
    let mut ids: Vec<i64> = db::load_open_trades(&bot.db)
        .await?
        .iter()
        .map(|t| t.id)
        .collect();
    ids.sort_unstable();
    Ok(ids)
}

async fn open_ids_in_hot_state(bot: &Arc<TradingBot>) -> Vec<i64> {
    let mut ids: Vec<i64> = bot
        .portfolio_state
        .trades
        .read()
        .await
        .iter()
        .filter(|t| t.is_open())
        .map(|t| t.id)
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn shutdown_closes_every_open_position_and_leaves_none_persisted() -> anyhow::Result<()> {
    let harness = Harness::new("multi_open_shutdown")?;
    let bot = boot(&harness).await?;

    let opened = open_positions(&bot, "shutdown", 4).await;
    assert_eq!(
        open_ids_in_db(&bot).await?,
        opened,
        "all four should persist"
    );
    assert_eq!(open_ids_in_hot_state(&bot).await, opened);

    // Queue a low-priority audit behind the actor. stop_execution_actor must
    // not return until both the command and the outbox writer have drained it.
    bot.append_outbox_audit(
        "shutdown_acceptance",
        "mkt-audit-drain",
        None,
        None,
        "prove_drain",
        "ok",
        "queued before shutdown",
    )
    .await;

    let outcome = bot.shutdown_close_open_positions().await;

    assert_eq!(
        outcome.attempted, opened,
        "sweep should attempt every open position"
    );
    assert_eq!(
        outcome.closed, opened,
        "sweep should close every open position"
    );
    assert!(
        outcome.failed.is_empty(),
        "no close should fail: {:?}",
        outcome.failed
    );
    assert!(
        !outcome.leaked_open_positions(),
        "shutdown leaked open positions: {:?}",
        outcome.remaining_open
    );
    assert!(
        outcome.is_clean(),
        "shutdown outcome should be clean: {outcome:?}"
    );

    assert!(
        open_ids_in_db(&bot).await?.is_empty(),
        "database must hold no OPEN trades"
    );
    assert!(
        open_ids_in_hot_state(&bot).await.is_empty(),
        "hot state must hold no open trades"
    );

    // Shutdown engages the entry gate before sweeping. Even while the actor is
    // still available to finish closes, it cannot create exposure behind us.
    let rejected = bot
        .execute_trade_serialized_result(&buy_yes("mkt-after-shutdown", 0.50), false, Some(1.0))
        .await
        .expect("the draining actor should answer");
    assert!(rejected.is_none(), "shutdown must block new exposure");
    assert!(open_ids_in_db(&bot).await?.is_empty());
    bot.stop_execution_actor().await?;
    let drained_audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM order_outbox WHERE stage = 'shutdown_acceptance' AND action = 'prove_drain'",
    )
    .fetch_one(&bot.db)
    .await?;
    assert_eq!(drained_audits, 1, "queued audit must persist before join");

    // Every attempted position must have a terminal status and a recorded P&L.
    for id in opened {
        let persisted = db::load_trade_by_id(&bot.db, id)
            .await?
            .expect("closed trade should persist");
        assert_eq!(persisted.status, "CLOSED", "trade {id} should be CLOSED");
        assert!(
            persisted.exit_price.is_some(),
            "trade {id} needs an exit price"
        );
        assert!(persisted.pnl.is_some(), "trade {id} needs a recorded P&L");
    }
    Ok(())
}

#[tokio::test]
async fn shutdown_on_an_empty_book_is_a_clean_no_op() -> anyhow::Result<()> {
    let harness = Harness::new("empty_shutdown")?;
    let bot = boot(&harness).await?;

    let outcome = bot.shutdown_close_open_positions().await;
    assert!(outcome.attempted.is_empty());
    assert!(outcome.is_clean());
    assert!(!outcome.leaked_open_positions());
    Ok(())
}

#[tokio::test]
async fn orderly_restart_releases_old_actor_and_recovers_state() -> anyhow::Result<()> {
    let harness = Harness::new("restart_recovery")?;

    let (opened, balance_before_restart) = {
        let bot = boot(&harness).await?;
        let opened = open_positions(&bot, "restart", 3).await;
        let balance = bot.balance().await;
        let weak = Arc::downgrade(&bot);
        bot.stop_execution_actor().await?;
        bot.db.close().await;
        drop(bot);
        tokio::task::yield_now().await;
        assert!(
            weak.upgrade().is_none(),
            "the stopped execution actor must release the old bot"
        );
        (opened, balance)
    };

    // Restart against the same database.
    let bot = boot(&harness).await?;

    assert_eq!(
        open_ids_in_hot_state(&bot).await,
        opened,
        "every open position should be rebuilt into hot state after restart"
    );
    assert_eq!(open_ids_in_db(&bot).await?, opened);
    assert!(
        (bot.balance().await - balance_before_restart).abs() < 1e-9,
        "balance should survive restart: {} vs {}",
        bot.balance().await,
        balance_before_restart
    );

    // A restarted process must still be able to shut down cleanly.
    let outcome = bot.shutdown_close_open_positions().await;
    assert_eq!(outcome.closed, opened);
    assert!(outcome.is_clean(), "{outcome:?}");
    assert!(open_ids_in_db(&bot).await?.is_empty());
    bot.stop_execution_actor().await?;
    Ok(())
}

/// Child-process entry point for the crash recovery test below. The test is
/// ignored during an ordinary suite run and invoked explicitly in a separate
/// process with a unique database path.
#[test]
#[ignore = "invoked as a subprocess by crash_restart_recovers_persisted_state"]
fn subprocess_crash_seed_helper() {
    let path = std::env::var(CRASH_SEED_DB_ENV).expect("crash-seed database path");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("child runtime");
    runtime.block_on(async move {
        let pool = db::open(&path).await.expect("open crash-seed database");
        let bot = Arc::new(
            TradingBot::new(acceptance_config().expect("config"), pool)
                .await
                .expect("construct child bot"),
        );
        bot.start_execution_actor().await;
        let opened = open_positions(&bot, "process-crash", 3).await;
        assert_eq!(open_ids_in_db(&bot).await.expect("persisted ids"), opened);
        // Intentionally skip actor shutdown, pool closure and destructors. The
        // parent can only continue after this process has ceased to exist.
        std::process::exit(0);
    });
}

#[tokio::test]
async fn crash_restart_recovers_persisted_state() -> anyhow::Result<()> {
    let harness = Harness::new("process_crash_restart")?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "subprocess_crash_seed_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(CRASH_SEED_DB_ENV, &harness.path_str)
        .status()?;
    assert!(status.success(), "crash-seed child failed: {status}");

    // The child process is gone. Reopening SQLite exercises WAL recovery and
    // constructs an entirely new bot and execution actor.
    let bot = boot(&harness).await?;
    let persisted = db::load_open_trades(&bot.db).await?;
    assert_eq!(persisted.len(), 3, "all crash-time positions must recover");
    assert_eq!(
        open_ids_in_hot_state(&bot).await,
        open_ids_in_db(&bot).await?
    );

    let config = acceptance_config()?;
    let expected_balance = config.initial_balance
        - persisted
            .iter()
            .map(|trade| trade.amount + trade.fee_paid)
            .sum::<f64>();
    assert!(
        (bot.balance().await - expected_balance).abs() < 1e-9,
        "restart balance {} should match persisted ledger {expected_balance}",
        bot.balance().await
    );

    let outcome = bot.shutdown_close_open_positions().await;
    assert!(outcome.is_clean(), "{outcome:?}");
    assert!(open_ids_in_db(&bot).await?.is_empty());
    bot.stop_execution_actor().await?;
    Ok(())
}

/// Crosses a real child-process termination, backs up the recovered WAL state,
/// and boots from a new restored file. No funded venue is involved.
#[tokio::test]
async fn crash_backup_restore_preserves_positions_controls_and_idempotency() -> anyhow::Result<()> {
    let source = Harness::new("backup_crash_source")?;
    let restored = Harness::new("backup_crash_restore")?;
    let status = std::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "subprocess_crash_seed_helper",
            "--ignored",
            "--nocapture",
        ])
        .env(CRASH_SEED_DB_ENV, &source.path_str)
        .status()?;
    assert!(status.success());
    let pool = db::open(&source.path_str).await?;
    let original = db::load_open_trades(&pool).await?;
    assert_eq!(original.len(), 3);
    db::maker::persist_manual_stand_down(&pool, "stopped-after-crash").await?;
    db::upsert_idempotency_entry(&pool, "previous-request", r#"{"accepted":true}"#, 123).await?;
    let backup_path = format!(
        "artifacts/backups/crash-restore-{}.sqlite",
        source.path.file_stem().unwrap().to_string_lossy()
    );
    let counts = db::critical_table_counts(&pool).await?;
    db::backup_database(&pool, &backup_path).await?;
    pool.close().await;
    std::fs::copy(&backup_path, &restored.path)?;
    std::fs::remove_file(&backup_path)?;
    let bot = boot(&restored).await?;
    assert_eq!(db::critical_table_counts(&bot.db).await?, counts);
    assert!(db::maker::market_stood_down(&bot.db, "stopped-after-crash").await?);
    let cached: String =
        sqlx::query_scalar("SELECT payload FROM idempotency_cache WHERE key='previous-request'")
            .fetch_one(&bot.db)
            .await?;
    assert_eq!(cached, r#"{"accepted":true}"#);
    let ids: Vec<_> = original.iter().map(|trade| trade.id).collect();
    assert_eq!(open_ids_in_hot_state(&bot).await, ids);
    let first = bot.shutdown_close_open_positions().await;
    assert!(first.is_clean(), "{first:?}");
    let after = db::critical_table_counts(&bot.db).await?;
    let second = bot.shutdown_close_open_positions().await;
    assert!(second.is_clean());
    let repeated = db::critical_table_counts(&bot.db).await?;
    assert_eq!(repeated.trades, after.trades);
    assert_eq!(
        repeated.trade_fills, after.trade_fills,
        "repeat shutdown cannot account twice"
    );
    assert_eq!(repeated.idempotency_cache, after.idempotency_cache);
    assert_eq!(repeated.maker_market_controls, after.maker_market_controls);
    bot.stop_execution_actor().await?;
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn partial_fills_are_replayed_from_the_ledger_across_restart() -> anyhow::Result<()> {
    let harness = Harness::new("partial_fill_restart")?;
    let pool = db::open(&harness.path_str).await?;

    // A live trade whose recorded size disagrees with its fill ledger. The
    // ledger is the authority; restart must repair the trade to match it.
    let mut live = db::Trade {
        entry_price: 0.45,
        shares: 10.0,
        amount: 4.5,
        order_id: Some("oid-acceptance-partial".to_string()),
        token_id: Some("tok-acceptance-partial".to_string()),
        ..sample_open_live_trade()
    };
    live.market_id = "mkt-partial-fill".to_string();
    let trade_id = db::insert_trade(&pool, &live).await?;

    for (cumulative, avg_price, note) in
        [(2.0, 0.62, "first partial"), (5.0, 0.58, "second partial")]
    {
        db::append_trade_fill_snapshot(
            &pool,
            trade_id,
            &live.market_id,
            live.order_id.as_deref().unwrap_or_default(),
            "entry",
            0.50,
            avg_price,
            cumulative,
            "acceptance",
            note,
        )
        .await?;
    }
    drop(pool);

    let bot = boot(&harness).await?;
    let restored = bot
        .portfolio_state
        .trades
        .read()
        .await
        .iter()
        .find(|t| t.id == trade_id)
        .cloned()
        .expect("partially filled trade should be rebuilt");

    assert!(
        (restored.shares - 5.0).abs() < 1e-9,
        "shares should be repaired to the ledger's cumulative size, got {}",
        restored.shares
    );
    assert!(
        (restored.entry_price - 0.58).abs() < 1e-9,
        "entry price should be repaired to the ledger's average, got {}",
        restored.entry_price
    );

    // The repair must be durable, not only in memory.
    let persisted = db::load_trade_by_id(&bot.db, trade_id)
        .await?
        .expect("repaired trade should persist");
    assert!((persisted.shares - 5.0).abs() < 1e-9);
    assert!((persisted.entry_price - 0.58).abs() < 1e-9);

    // Shutting down a live position with no exchange route must fail loudly.
    // Silently marking it closed would invent a fill that never happened and
    // lose the position, so the correct behaviour is to leave it open, record
    // the leak, and stop trading.
    let outcome = bot.shutdown_close_open_positions().await;
    assert_eq!(outcome.attempted, vec![trade_id]);
    assert!(
        outcome.closed.is_empty(),
        "an unreachable live position must not be reported as closed"
    );
    assert_eq!(outcome.failed, vec![trade_id]);
    assert!(
        outcome.leaked_open_positions(),
        "the surviving position must be reported, not hidden"
    );
    assert!(!outcome.is_clean());
    assert_eq!(
        open_ids_in_db(&bot).await?,
        vec![trade_id],
        "the position must still be persisted as OPEN, not silently dropped"
    );

    // The leak must be visible to an operator, not only in logs.
    let recorded = bot
        .last_reconcile_error()
        .await
        .expect("a failed shutdown must record a reconcile error");
    assert!(
        recorded.phase == "shutdown_cleanup"
            || recorded.phase == "close_trade_live_exit_unavailable",
        "unexpected reconcile phase: {}",
        recorded.phase
    );
    assert!(
        bot.kill_switch_active(),
        "a position that cannot be exited must engage the kill switch"
    );
    Ok(())
}

#[tokio::test]
async fn accounting_is_consistent_after_a_multi_position_shutdown() -> anyhow::Result<()> {
    let harness = Harness::new("accounting")?;
    let bot = boot(&harness).await?;

    let starting_balance = bot.balance().await;
    let opened = open_positions(&bot, "accounting", 3).await;
    let outcome = bot.shutdown_close_open_positions().await;
    assert!(outcome.is_clean(), "{outcome:?}");

    // Balance must equal the starting balance plus realised P&L, less fees.
    let mut realised = 0.0;
    let mut fees = 0.0;
    for id in &opened {
        let trade = db::load_trade_by_id(&bot.db, *id)
            .await?
            .expect("closed trade should persist");
        realised += trade.pnl.unwrap_or(0.0);
        fees += trade.fee_paid;
    }
    let expected = starting_balance + realised - fees;
    let actual = bot.balance().await;
    assert!(
        (actual - expected).abs() < 1e-6,
        "balance {actual} should equal start {starting_balance} + pnl {realised} - fees {fees} = {expected}"
    );

    // Hot state and the database must agree on what remains open.
    assert_eq!(
        open_ids_in_hot_state(&bot).await,
        open_ids_in_db(&bot).await?
    );
    assert!(open_ids_in_db(&bot).await?.is_empty());

    // No trade may be left in a non-terminal status.
    for id in &opened {
        let trade = db::load_trade_by_id(&bot.db, *id)
            .await?
            .expect("closed trade should persist");
        assert_eq!(
            trade.status, "CLOSED",
            "trade {id} left in status {}",
            trade.status
        );
    }
    Ok(())
}

#[tokio::test]
async fn shutdown_checks_the_database_even_when_hot_state_is_empty() -> anyhow::Result<()> {
    // The failure this guards: a position OPEN on disk but absent from memory.
    // An earlier version returned early on an empty hot state and reported a
    // perfectly clean shutdown while the position was still persisted.
    let harness = Harness::new("db_only_exposure")?;
    let pool = db::open(&harness.path_str).await?;
    let mut orphan = sample_open_live_trade();
    orphan.market_id = "mkt-db-only".to_string();
    orphan.mode = "simulation".to_string();
    let trade_id = db::insert_trade(&pool, &orphan).await?;

    // Remove it from hot state only, leaving the database row untouched.
    let bot = boot(&harness).await?;
    bot.portfolio_state.trades.write().await.clear();
    assert!(
        open_ids_in_hot_state(&bot).await.is_empty(),
        "precondition: hot state must be empty"
    );
    assert_eq!(
        open_ids_in_db(&bot).await?,
        vec![trade_id],
        "precondition: the database must still hold the position"
    );

    let outcome = bot.shutdown_close_open_positions().await;

    assert!(
        outcome.attempted.is_empty(),
        "nothing was open in hot state, so nothing should be attempted"
    );
    assert_eq!(
        outcome.remaining_open,
        vec![trade_id],
        "the persisted position must be detected despite empty hot state"
    );
    assert!(outcome.leaked_open_positions());
    assert!(
        !outcome.is_clean(),
        "a shutdown leaving persisted exposure must never report clean"
    );
    Ok(())
}

#[tokio::test]
async fn shutdown_reports_verification_failure_rather_than_clean() -> anyhow::Result<()> {
    // If persisted state cannot be read, exposure is unknown. Unknown must not
    // present as flat: an earlier version turned a read error into an empty
    // result, which is indistinguishable from "nothing is open".
    let harness = Harness::new("unreadable_pool")?;
    let bot = boot(&harness).await?;
    let opened = open_positions(&bot, "unreadable", 2).await;
    assert_eq!(open_ids_in_db(&bot).await?, opened);

    // Close the pool so every subsequent query fails.
    bot.db.close().await;

    let outcome = bot.shutdown_close_open_positions().await;

    assert!(
        outcome.verification_failed(),
        "an unreadable database must be reported as a verification failure"
    );
    assert!(
        !outcome.is_clean(),
        "unverified exposure must never report clean: {outcome:?}"
    );
    let verification_error = outcome.verification_error.as_deref().unwrap_or("");
    assert!(
        verification_error.contains("persisted trades")
            || verification_error.contains("persisted orders"),
        "the error should say what could not be read: {:?}",
        outcome.verification_error
    );
    Ok(())
}

/// A minimal OPEN live trade row for ledger-repair tests.
fn sample_open_live_trade() -> db::Trade {
    db::Trade {
        id: 0,
        timestamp: chrono::Utc::now().to_rfc3339(),
        market_id: "mkt-acceptance".to_string(),
        question: "acceptance".to_string(),
        direction: "BUY_YES".to_string(),
        entry_price: 0.5,
        amount: 5.0,
        shares: 10.0,
        expected_value: 0.1,
        reason: "acceptance".to_string(),
        confidence: 0.9,
        status: "OPEN".to_string(),
        mode: "live".to_string(),
        source: "acceptance".to_string(),
        end_date: "2099-01-01T00:00:00Z".to_string(),
        ..Default::default()
    }
}
