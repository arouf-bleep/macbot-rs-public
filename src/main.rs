#![recursion_limit = "256"]
// The binary uses the library's modules so production and tests compile one tree.
use macbot_rs::{api, bot, config, db, market, ml, notifications, signals, tls};

use anyhow::Result;
use macbot_rs::domain::BotMode;
use macbot_rs::env_util::{env_bool, env_u64};
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{info, warn};

/// How long, after the shutdown signal, to wait for open HTTP responses to
/// finish before the position sweep runs anyway.
const HTTP_DRAIN_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

fn env_port(key: &str, default: u16) -> Result<u16> {
    let Some(raw) = std::env::var(key).ok() else {
        return Ok(default);
    };
    let port = raw
        .trim()
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("{key} must be an integer between 1 and 65535"))?;
    if port == 0 {
        anyhow::bail!("{key} must be an integer between 1 and 65535");
    }
    Ok(port)
}

async fn run_auto_db_backup(pool: &SqlitePool, backup_dir: &str) -> Result<String> {
    use chrono::Utc;

    std::fs::create_dir_all(backup_dir)?;
    let path = std::path::Path::new(backup_dir).join(format!(
        "auto-{}.sqlite",
        Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    let path_str = path.to_string_lossy().to_string();
    db::backup_database(pool, &path_str).await?;
    Ok(path_str)
}

#[tokio::main]
async fn main() -> Result<()> {
    // ── Logging ───────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("macbot=info".parse()?),
        )
        .init();

    // ── TLS backend ──────────────────────────────────────────────────────────
    // rustls 0.23 refuses to pick a crypto provider when both aws-lc-rs and
    // ring are in the dependency graph, which they are here via reqwest, sqlx
    // and tungstenite. Without this call every TLS connection panics on its
    // first handshake: the l2_ingestion, price_bus and liquidation_feed tasks
    // all died about fourteen seconds after startup, and because l2_ingestion
    // is monitored as critical the task monitor engaged the kill switch. The
    // bot halted itself on every single run and the order book was never once
    // populated.
    //
    // Installed before anything opens a socket. Failure here means a provider
    // was already installed, which is fine.
    tls::install_provider();

    // ── Load env + config ────────────────────────────────────────────────────
    if let Some(path) = std::env::var_os("MACBOT_DOTENV_PATH") {
        // An explicit installation must not discover another project's .env
        // in an ancestor directory. Preflight checks this same selected file.
        dotenvy::from_path(path).ok();
    } else {
        dotenvy::dotenv().ok();
    }
    let config_path = std::env::var("MACBOT_CONFIG").unwrap_or_else(|_| "config.json".into());
    let cfg = config::Config::load(&config_path)?;
    let backtest_only = env_bool("MACBOT_BACKTEST_ONLY", false);
    if backtest_only && cfg.mode_kind() == BotMode::Live {
        anyhow::bail!("MACBOT_BACKTEST_ONLY requires simulation or shadow configuration; live mode is refused");
    }
    // Reserve the control interface before creating state or starting actors.
    // A port/configuration failure must not leave an unattended trading run.
    let bind_all = env_bool("MACBOT_BIND_ALL", false);
    let port = env_port("MACBOT_PORT", 8000)?;
    let addr = std::net::SocketAddr::from((
        if bind_all {
            [0, 0, 0, 0]
        } else {
            [127, 0, 0, 1]
        },
        port,
    ));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let db_path = std::env::var("MACBOT_DB").unwrap_or_else(|_| "trades.db".into());
    let _instance_lock = macbot_rs::instance_lock::InstanceLock::acquire(&db_path)?;
    info!(
        "✅ Config loaded — mode={} execution_mode={}",
        cfg.mode, cfg.execution_mode
    );
    if backtest_only {
        tracing::warn!(
            "Research mode enabled (MACBOT_BACKTEST_ONLY=1): skipping live feeds, startup analysis, and trading loop/background guards"
        );
    }
    if let Ok(path) = snapshot_run_config(&cfg, &config_path) {
        info!("🧾 Config snapshot saved — {}", path);
    }

    if cfg.mode_kind() == BotMode::Live {
        tracing::warn!(
            "⚠️  Running in LIVE mode over plain HTTP! \
             Deploy behind a TLS-terminating reverse proxy (nginx, Caddy, etc.)"
        );
    }

    // ── Database ──────────────────────────────────────────────────────────────
    let pool = db::open(&db_path).await?;
    info!("✅ SQLite pool ready");
    db::quick_check(&pool).await?;
    info!("✅ SQLite quick_check ok");

    // ── Shared bot state ─────────────────────────────────────────────────────
    let bot = Arc::new(bot::state::TradingBot::new(cfg.clone(), pool.clone()).await?);
    bot.runtime_state
        .backtest_only
        .store(backtest_only, std::sync::atomic::Ordering::SeqCst);
    bot.start_execution_actor().await;
    if !backtest_only && env_bool("MACBOT_STARTUP_ANALYSIS", true) {
        let analysis = bot.analyze_market().await;
        {
            let mut guard = bot.analysis_state.last_analysis.write().await;
            *guard = analysis.clone();
        }
        bot.mark_analysis_fresh().await;
        let market_id = analysis
            .pointer("/market_data/market_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        info!("✅ Startup analysis complete — market_id={market_id}");
    }
    let startup_preflight = api::health::live_preflight_report(&cfg);
    let automatic_start_allowed =
        !backtest_only && (!startup_preflight.required || startup_preflight.ready);
    for warning in &startup_preflight.warnings {
        tracing::warn!("Live preflight warning: {warning}");
    }
    if backtest_only {
        tracing::info!("Backtest-only mode: loop auto-start disabled");
    } else if startup_preflight.required && !startup_preflight.ready {
        for error in &startup_preflight.errors {
            tracing::error!("Live preflight failed: {error}");
        }
        tracing::warn!(
            "Live preflight blocked automatic loop start; fix the reported issues and re-check /api/preflight before starting the loop"
        );
    }
    info!(
        "✅ Bot initialized — run_id={} balance=${:.2} loop_running={}",
        bot.run_id(),
        bot.balance().await,
        bot.loop_running()
    );

    let auto_backup_dir = std::env::var("MACBOT_AUTO_DB_BACKUP_DIR")
        .unwrap_or_else(|_| "artifacts/backups/auto".to_string());
    let auto_backup_on_startup = env_bool("MACBOT_AUTO_DB_BACKUP_ON_STARTUP", false);
    let auto_backup_interval_secs = env_u64("MACBOT_AUTO_DB_BACKUP_INTERVAL_SECS", 0);
    if auto_backup_on_startup {
        match run_auto_db_backup(&pool, &auto_backup_dir).await {
            Ok(path) => info!("✅ Auto DB backup created at startup — {path}"),
            Err(e) => {
                tracing::warn!("Auto DB backup at startup failed: {e}");
                notifications::notify(
                    "Auto DB Backup Failed",
                    &format!("startup backup failed: {e}"),
                    notifications::NotifyLevel::Critical,
                )
                .await;
            }
        }
    }
    if auto_backup_interval_secs > 0 {
        let pool_b = pool.clone();
        let backup_dir = auto_backup_dir.clone();
        let every_secs = auto_backup_interval_secs.max(60);
        macbot_rs::task_monitor::spawn_monitored_task("auto_db_backup", async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(every_secs));
            loop {
                interval.tick().await;
                match run_auto_db_backup(&pool_b, &backup_dir).await {
                    Ok(path) => info!("✅ Auto DB backup created — {path}"),
                    Err(e) => {
                        tracing::warn!("Auto DB backup failed: {e}");
                        notifications::notify(
                            "Auto DB Backup Failed",
                            &format!("periodic backup failed: {e}"),
                            notifications::NotifyLevel::Critical,
                        )
                        .await;
                    }
                }
            }
        });
    }

    // On live restarts, run an immediate reconcile pass before regular timers.
    if !backtest_only && bot.mode_kind().await == BotMode::Live {
        if let Err(e) = bot.full_resync_live_state().await {
            bot.set_degraded_no_trade(true);
            bot.halt_trading("startup_full_resync", e.to_string()).await;
            bot.record_reconcile_error("full_resync", e.to_string())
                .await;
            tracing::warn!("Startup live full-reconcile error: {e}");
        }
    }

    // Recovery can discover exposure or persistence faults after preflight.
    // Advertise an active loop only after that recovery has completed safely.
    if automatic_start_allowed && !bot.kill_switch_active() && !bot.degraded_no_trade_active() {
        bot.start_loop();
    }

    if !backtest_only {
        // Optional official-calendar renewal is bounded and never sits on the
        // execution actor. Missing/expired refreshed data blocks its risk gate.
        {
            let calendar_bot = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_monitored_task("macro_calendar_refresh", async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut last_attempt: Option<(String, std::time::Instant)> = None;
                loop {
                    interval.tick().await;
                    let cfg = calendar_bot.control_plane.config.read().await.clone();
                    if cfg.macro_event_guard_enabled && cfg.macro_event_auto_refresh {
                        if last_attempt.as_ref().is_some_and(|(path, when)| {
                            path == &cfg.macro_event_calendar_path
                                && when.elapsed() < std::time::Duration::from_secs(3600)
                        }) {
                            continue;
                        }
                        last_attempt = Some((
                            cfg.macro_event_calendar_path.clone(),
                            std::time::Instant::now(),
                        ));
                        if let Err(error) =
                            bot::macro_calendar::refresh_official_calendar(&cfg).await
                        {
                            tracing::warn!("Macro calendar refresh failed; guard freshness remains enforced: {error}");
                        }
                    } else {
                        last_attempt = None;
                    }
                }
            });
        }
        // ── Dedicated market L2 ingestion (maker/hybrid only) ───────────────────
        market::l2_ws::spawn_l2_ingestion(Arc::clone(&bot));

        // ── Real-time BTC/ETH/SOL mid-price bus ────────────────────────────────
        signals::price_bus::spawn_price_bus();

        // ── Binance liquidation feed (Phase 2 data collection) ────────────────
        signals::liquidation::spawn_liquidation_feed(
            Arc::clone(&bot.intelligence.liq_tracker),
            Arc::clone(&bot.intelligence.data_logger),
        );

        // ── ONNX model watcher (Phase 2 hot-reload) ──────────────────────────
        {
            let models_dir =
                std::env::var("MACBOT_MODELS_DIR").unwrap_or_else(|_| "models".to_string());
            // Ensure directory exists
            let _ = std::fs::create_dir_all(&models_dir);
            ml::model_watcher::spawn_model_watcher(
                models_dir,
                Arc::clone(&bot.intelligence.deep_model),
                Arc::clone(&bot.intelligence.ensemble_state),
            );
        }

        // ── Market filter sanity check (once on boot) ───────────────────────────
        {
            let bot_m = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_monitored_task(
                "market_filter_sanity_check",
                async move {
                    let markets = api::markets::fetch_markets_snapshot(&bot_m).await;
                    if markets.is_empty() {
                        let slug = {
                            let cfg = bot_m.control_plane.config.read().await;
                            cfg.market_filter.slug_pattern.clone()
                        };
                        tracing::warn!(
                            "Market filter returned 0 active markets (slug_pattern='{}')",
                            slug
                        );
                    }
                },
            );
        }

        // ── Resolution Guard (tokio task — fires every 5s independently) ─────────
        {
            let bot_g = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_critical_bot_task(
                "resolution_guard",
                Arc::clone(&bot_g),
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                    loop {
                        interval.tick().await;
                        if let Err(e) = bot_g.resolution_guard().await {
                            bot_g.set_degraded_no_trade(true);
                            bot_g.halt_trading("resolution_guard", e.to_string()).await;
                            bot_g
                                .record_reconcile_error("resolution_guard", e.to_string())
                                .await;
                            tracing::warn!("Resolution Guard error: {e}");
                        }
                    }
                },
            );
        }

        // ── TP/SL guard (tokio task — fires every `price_refresh_rate` seconds) ──
        // Keeps risk exits responsive even when analysis cadence is slower.
        {
            let bot_t = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_critical_bot_task(
                "tp_sl_guard",
                Arc::clone(&bot_t),
                async move {
                    loop {
                        bot_t.tp_sl_guard_tick().await;
                        let sleep_secs = {
                            let cfg = bot_t.control_plane.config.read().await;
                            cfg.price_refresh_rate.max(1)
                        };
                        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                    }
                },
            );
        }

        // ── Runtime SLO guard (tokio task — fires every 2s) ──────────────────────
        // Enforces sustained-lag/backpressure fail-closed behavior in live maker/hybrid.
        {
            let bot_s = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_critical_bot_task(
                "runtime_slo_guard",
                Arc::clone(&bot_s),
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
                    loop {
                        interval.tick().await;
                        bot_s.enforce_runtime_slos().await;
                    }
                },
            );
        }

        // ── Live order fill monitor (tokio task — fires every 30s) ───────────────
        // Polls open live GTC orders to update fill prices and catch cancellations.
        {
            let bot_f = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_critical_bot_task(
                "live_order_fill_monitor",
                Arc::clone(&bot_f),
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                    loop {
                        interval.tick().await;
                        bot_f.poll_open_live_orders().await;
                    }
                },
            );
        }

        // ── Live exchange reconciliation (tokio task — fires every 60s) ──────────
        // Syncs order drift + orphaned-trade cleanup + balance.
        {
            let bot_r = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_critical_bot_task(
                "live_exchange_reconciliation",
                Arc::clone(&bot_r),
                async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                    loop {
                        interval.tick().await;
                        if bot_r.mode_kind().await == BotMode::Live {
                            if let Err(e) = bot_r.reconcile_live_state().await {
                                bot_r.set_degraded_no_trade(true);
                                bot_r.halt_trading("live_reconcile", e.to_string()).await;
                                bot_r
                                    .record_reconcile_error("live_reconcile", e.to_string())
                                    .await;
                                tracing::warn!("Live reconcile error: {e}");
                            }
                        }
                    }
                },
            );
        }

        // ── ML archive outcome backfill (tokio task — fires every 60s) ───────────
        // Labels exact-input rows for resolved watched markets even when we never
        // traded them, so live shadow evaluation has honest outcomes.
        {
            let bot_o = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_monitored_task("archive_outcome_backfill", async move {
                let every = std::env::var("MACBOT_ARCHIVE_OUTCOME_BACKFILL_INTERVAL_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60)
                    .max(30);
                let limit = std::env::var("MACBOT_ARCHIVE_OUTCOME_BACKFILL_LIMIT")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(8)
                    .max(1);
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(every));
                loop {
                    interval.tick().await;
                    if let Err(e) = bot_o.backfill_unresolved_exact_input_outcomes(limit).await {
                        tracing::warn!("Archive outcome backfill error: {e}");
                    }
                }
            });
        }

        // ── Operational history retention (tokio task — every 6 hours) ──────────
        // Only `idempotency_cache` had any retention, so every other diagnostic
        // log grew without bound: 271,909 outbox rows against 66 trades, in a
        // 166 MB file. This never touches `trades` or `trade_fills`.
        {
            let bot_r = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_monitored_task("history_retention", async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(6 * 60 * 60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let retain = {
                        let cfg = bot_r.control_plane.config.read().await;
                        cfg.operational_retention_days
                    };
                    match db::prune_operational_history(&bot_r.db, retain).await {
                        Ok(outcome) if outcome.total() > 0 => info!(
                            "Retention: pruned {} rows older than {retain}d \
                             (outbox {}, snapshots {}, rejections {})",
                            outcome.total(),
                            outcome.order_outbox,
                            outcome.analysis_snapshots,
                            outcome.trade_rejections,
                        ),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("Retention sweep failed: {e}"),
                    }
                }
            });
        }

        // ── Full live resync (tokio task — default every 5 min) ──────────────────
        // Repairs any lingering DB-vs-memory OPEN live trade drift by stable IDs.
        {
            let bot_f = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_critical_bot_task(
                "full_live_resync",
                Arc::clone(&bot_f),
                async move {
                    let every = std::env::var("MACBOT_FULL_RESYNC_INTERVAL_SECS")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(300)
                        .max(30);
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(every));
                    loop {
                        interval.tick().await;
                        if bot_f.mode_kind().await == BotMode::Live {
                            if let Err(e) = bot_f.full_resync_live_state().await {
                                bot_f.set_degraded_no_trade(true);
                                bot_f.halt_trading("full_resync", e.to_string()).await;
                                bot_f
                                    .record_reconcile_error("full_resync", e.to_string())
                                    .await;
                                tracing::warn!("Live full resync error: {e}");
                            }
                        }
                    }
                },
            );
        }

        // ── Capital rebalancer (tokio task — fires every 30s) ───────────────────
        // Auto-merges matched YES/NO inventory in simulation-safe modes.
        {
            let bot_c = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_monitored_task("capital_rebalancer", async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if let Err(e) = bot_c.rebalance_offsetting_pairs().await {
                        tracing::warn!("Capital rebalancer error: {e}");
                    }
                }
            });
        }

        // ── Cache pruning (tokio task — fires every 120s) ─────────────────────────
        // Prevents unbounded growth in the rate-limiter DashMap and idempotency cache.
        {
            let bot_p = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_monitored_task("cache_pruning", async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(120));
                loop {
                    interval.tick().await;
                    // Prune stale rate-limiter entries (IPs that haven't sent requests recently).
                    api::prune_rate_map();
                    // Prune expired idempotency cache entries (>60s old).
                    bot_p.prune_idempotency_cache().await;
                }
            });
        }

        // ── Bot trading loop (tokio task) ─────────────────────────────────────────
        {
            let bot_l = Arc::clone(&bot);
            macbot_rs::task_monitor::spawn_critical_bot_task(
                "bot_trading_loop",
                Arc::clone(&bot_l),
                async move {
                    info!("🚀 Bot Started — BTC Up/Down 15m Markets Only");
                    info!("   mode={}", bot_l.mode().await);
                    loop {
                        let running = bot_l.loop_running();

                        if running {
                            if let Err(e) = bot_l.run_once().await {
                                tracing::error!("Bot loop error: {e}");
                            }
                        }

                        let sleep_secs = if running {
                            // Dynamic check_interval: re-read from config each tick so
                            // changes via /api/config take effect without a restart.
                            let (
                                check_interval,
                                signal_refresh_interval,
                                analysis_stale_seconds,
                                execution_mode,
                            ) = {
                                let cfg = bot_l.control_plane.config.read().await;
                                (
                                    cfg.check_interval.max(1),
                                    cfg.signal_refresh_interval.max(1),
                                    cfg.analysis_stale_seconds.max(1),
                                    cfg.execution_mode.clone(),
                                )
                            };

                            let mins_left = bot_l.time_remaining_minutes().await;
                            macbot_rs::config::analysis_refresh_secs(
                                &execution_mode,
                                check_interval,
                                signal_refresh_interval,
                                analysis_stale_seconds,
                                mins_left,
                            )
                        } else {
                            // While paused, wait for Notify (from start_loop) instead of polling
                            bot_l.runtime_state.loop_notify.notified().await;
                            continue;
                        };

                        tokio::time::sleep(std::time::Duration::from_secs(sleep_secs)).await;
                    }
                },
            );
        }
    }

    // ── Axum HTTP server with graceful shutdown ────────────────────────────────
    let router = api::router(Arc::clone(&bot));
    if bind_all {
        info!(
            "🌐 API server listening on http://{} (MACBOT_BIND_ALL=1)",
            addr
        );
    } else {
        info!(
            "🌐 API server listening on http://{} (set MACBOT_BIND_ALL=1 to expose)",
            addr
        );
    }

    // On the signal, tell every open stream to end so the drain can finish,
    // and bound the drain regardless: nothing a client does may stand between
    // the signal and the position sweep below.
    let bot_sig = Arc::clone(&bot);
    let server = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        bot_sig.runtime_state.begin_shutdown();
    });
    // `WithGracefulShutdown` is IntoFuture, not Future.
    match macbot_rs::bot::state::drain_or_deadline(
        async move { server.await },
        bot.runtime_state.shutdown_watch(),
        HTTP_DRAIN_LIMIT,
    )
    .await
    {
        Some(served) => served?,
        None => warn!(
            "HTTP drain did not finish within {HTTP_DRAIN_LIMIT:?}; \
             closing the listener and proceeding to the position sweep"
        ),
    }

    // Graceful shutdown: close all open positions at market.
    // The sweep itself lives on TradingBot so acceptance tests can prove it.
    let mut shutdown = bot.shutdown_close_open_positions().await;
    if shutdown.is_clean() {
        info!("🛑 Shutdown verified no open positions or unresolved orders");
    } else if shutdown.verification_failed() {
        // Exposure is unknown, which is worse than known-bad: nothing
        // downstream should treat this run as having ended flat.
        warn!(
            "🛑 Shutdown could not verify persisted exposure: {}",
            shutdown
                .verification_error
                .as_deref()
                .unwrap_or("unknown error")
        );
    } else if !shutdown.remaining_orders.is_empty() || shutdown.order_cleanup_error.is_some() {
        warn!(
            "🛑 Shutdown order cancellation unresolved: orders={:?} error={:?}",
            shutdown.remaining_orders, shutdown.order_cleanup_error
        );
    } else if shutdown.leaked_open_positions() {
        // The serious case: capital is still exposed with no process watching it.
        warn!(
            "🛑 Shutdown left {} OPEN position(s) persisted: {:?}",
            shutdown.remaining_open.len(),
            shutdown.remaining_open
        );
    } else {
        warn!(
            "🛑 Shutdown had {} failed close attempt(s) but no position survived",
            shutdown.failed.len()
        );
    }

    let actor_stop_error = match bot.stop_execution_actor().await {
        Ok(()) => {
            info!("🛑 Execution actor drained and stopped");
            None
        }
        Err(error) => {
            warn!("🛑 Execution actor did not stop cleanly: {error}");
            Some(error.to_string())
        }
    };

    // The writer may finish previously queued reconciliation while draining.
    // Verify its final durable state rather than relying on an earlier cache.
    bot.verify_shutdown_exposure(&mut shutdown).await;
    if !shutdown.is_clean() || actor_stop_error.is_some() {
        anyhow::bail!(
            "shutdown incomplete: remaining_open={:?} remaining_orders={:?} order_cleanup_error={:?} verification_error={:?} actor_error={:?}",
            shutdown.remaining_open,
            shutdown.remaining_orders,
            shutdown.order_cleanup_error,
            shutdown.verification_error,
            actor_stop_error
        );
    }

    info!("🛑 Shutdown complete");
    Ok(())
}

fn snapshot_run_config(cfg: &config::Config, config_path: &str) -> Result<String> {
    use chrono::Utc;
    use serde_json::json;

    let out_dir = std::env::var("MACBOT_CONFIG_SNAPSHOT_DIR")
        .unwrap_or_else(|_| "artifacts/config-snapshots".to_string());
    std::fs::create_dir_all(&out_dir)?;

    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let file_name = format!("run-{}-{}.json", ts, std::process::id());
    let path = std::path::Path::new(&out_dir).join(file_name);

    let payload = json!({
        "captured_at_utc": Utc::now().to_rfc3339(),
        "config_path": config_path,
        "mode": cfg.mode,
        "execution_mode": cfg.execution_mode,
        "config_redacted": cfg.redacted_value(),
    });
    let body = serde_json::to_string_pretty(&payload)?;
    std::fs::write(&path, body)?;

    Ok(path.to_string_lossy().to_string())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(e) => tracing::warn!("Failed to install Ctrl+C handler: {e}"),
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => tracing::warn!("Failed to install SIGTERM handler: {e}"),
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl+C, shutting down..."); }
        _ = terminate => { tracing::info!("Received SIGTERM, shutting down..."); }
    }
}
