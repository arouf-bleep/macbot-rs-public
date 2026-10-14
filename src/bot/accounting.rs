use chrono::Utc;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tracing::warn;

use crate::db::{self, Trade};
use crate::domain::{BotMode, TradeStatus};
use crate::metrics;

use super::state::TradingBot;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn trade(
        id: i64,
        mode: &str,
        status: &str,
        amount: f64,
        fee_paid: f64,
        pnl: Option<f64>,
    ) -> Trade {
        Trade {
            id,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            market_id: "m".to_string(),
            question: "q".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.5,
            amount,
            shares: 2.0,
            expected_value: 0.0,
            reason: "".to_string(),
            confidence: 0.6,
            status: status.to_string(),
            mode: mode.to_string(),
            source: "bot".to_string(),
            end_date: "".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: None,
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid,
            accounting_version: 0,
            signals_json: None,
            calibration_version: Some("test-cal-v1".to_string()),
            run_id: Some("test-run".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    async fn test_db(
        name: &str,
    ) -> anyhow::Result<(sqlx::SqlitePool, std::path::PathBuf, String, Config)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_accounting_{name}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 100.0;

        Ok((pool, path, path_str, cfg))
    }

    fn cleanup_db(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    #[test]
    fn compute_balance_ignores_live_and_applies_non_live_rules() {
        let trades = vec![
            trade(1, "simulation", "CLOSED", 0.0, 2.0, Some(10.0)),
            trade(2, "simulation", "OPEN", 20.0, 1.0, Some(3.0)),
            trade(3, "live", "CLOSED", 100.0, 50.0, Some(999.0)),
            trade(4, "simulation", "CLOSED", 0.0, 0.5, Some(-1.0)),
        ];

        let bal = TradingBot::compute_balance_from_trades(&trades, 100.0);

        // 100 + (10 - 2) - (20 + 1) + 3 + (-1 - 0.5) = 88.5
        assert!((bal - 88.5).abs() < 1e-9);
    }

    #[test]
    fn closing_paper_inventory_keeps_its_cash_locked() {
        let rows = vec![trade(1, "simulation", "CLOSING", 20.0, 1.0, Some(3.0))];
        assert_eq!(TradingBot::compute_balance_from_trades(&rows, 100.0), 82.0);
    }

    #[tokio::test]
    async fn closed_balance_query_failure_is_not_a_zero_pnl_history() {
        let pool = db::open(":memory:").await.unwrap();
        pool.close().await;
        assert!(TradingBot::closed_non_live_balance_delta(&pool)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn restart_retains_closing_paper_inventory_and_blocks_entries() -> anyhow::Result<()> {
        let (pool, path, path_str, cfg) = test_db("closing-paper").await?;
        let closing = trade(1, "simulation", "CLOSING", 20.0, 1.0, Some(3.0));
        db::insert_trade(&pool, &closing).await?;
        let bot = TradingBot::new(cfg, pool).await?;
        assert_eq!(bot.balance().await, 82.0);
        assert_eq!(bot.trades().await[0].status, "CLOSING");
        assert!(bot.entries_frozen());
        let snapshot = bot.accounting_truth_snapshot(false).await;
        assert_eq!(snapshot["derived_hot_balance"], 82.0);
        assert_eq!(snapshot["derived_restart_balance"], 82.0);
        bot.db.close().await;
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn closed_non_live_balance_delta_matches_persisted_rows() -> anyhow::Result<()> {
        let (pool, path, path_str, cfg) = test_db("delta").await?;

        let rows = vec![
            trade(1, "simulation", "CLOSED", 0.0, 2.0, Some(12.0)),
            trade(2, "paper", "CLOSED", 0.0, 1.5, Some(7.5)),
            trade(3, "live", "CLOSED", 100.0, 50.0, Some(999.0)),
            trade(4, "simulation", "OPEN", 20.0, 1.0, Some(3.0)),
        ];
        for row in &rows {
            db::insert_trade(&pool, row).await?;
        }

        let bot = TradingBot::new(cfg, pool).await?;
        let delta = TradingBot::closed_non_live_balance_delta(&bot.db).await?;
        assert!((delta - 16.0).abs() < 1e-9);

        let snapshot = bot.accounting_truth_snapshot(false).await;
        assert_eq!(snapshot["mode"], "simulation");
        assert!((snapshot["current_balance"].as_f64().unwrap_or_default() - 98.0).abs() < 1e-9);
        assert!(
            (snapshot["derived_restart_balance"]
                .as_f64()
                .unwrap_or_default()
                - 98.0)
                .abs()
                < 1e-9
        );
        assert!((snapshot["derived_hot_balance"].as_f64().unwrap_or_default() - 98.0).abs() < 1e-9);
        assert_eq!(snapshot["open_hot_count"].as_u64(), Some(1));
        assert!(
            (snapshot["components"]["closed_non_live_delta"]
                .as_f64()
                .unwrap_or_default()
                - 16.0)
                .abs()
                < 1e-9
        );
        assert!(
            (snapshot["components"]["open_non_live_locked"]
                .as_f64()
                .unwrap_or_default()
                - 21.0)
                .abs()
                < 1e-9
        );
        assert!(
            (snapshot["components"]["open_non_live_realized"]
                .as_f64()
                .unwrap_or_default()
                - 3.0)
                .abs()
                < 1e-9
        );
        assert_eq!(
            snapshot["components"]["open_live_locked"]
                .as_f64()
                .unwrap_or_default(),
            0.0
        );

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn reset_plan_absorbs_closed_pnl_and_preserves_locked_collateral() -> anyhow::Result<()> {
        let (pool, path, path_str, cfg) = test_db("reset_plan").await?;
        db::insert_trade(
            &pool,
            &trade(1, "simulation", "CLOSED", 0.0, 2.0, Some(12.0)),
        )
        .await?;
        db::insert_trade(&pool, &trade(2, "simulation", "OPEN", 20.0, 1.0, Some(3.0))).await?;
        let bot = TradingBot::new(cfg.clone(), pool).await?;
        let balance_before = bot.balance().await;
        let mut candidate = cfg;
        candidate.initial_balance = 150.0;
        let planned_balance = bot.prepare_initial_balance_update(&mut candidate).await?;
        assert_eq!(candidate.initial_balance, 140.0); // 150 requested - 10 closed net PnL.
        assert_eq!(planned_balance, 132.0); // 150 - 21 locked + 3 partially realised.
        assert_eq!(bot.balance().await, balance_before);
        assert_eq!(bot.control_plane.config.read().await.initial_balance, 100.0);
        bot.db.close().await;
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn unavailable_accounting_history_is_not_reported_as_zero() -> anyhow::Result<()> {
        let (pool, path, path_str, cfg) = test_db("missing_history").await?;
        let bot = TradingBot::new(cfg, pool).await?;
        bot.db.close().await;
        let snapshot = bot.accounting_truth_snapshot(false).await;
        assert!(snapshot["components"].is_null());
        assert!(snapshot["derived_restart_balance"].is_null());
        assert!(snapshot["derived_hot_balance"].is_null());
        assert!(snapshot["open_live_locked_db"].is_null());
        assert!(snapshot["reconstruction_error"].as_str().is_some());
        assert_eq!(snapshot["current_balance"].as_f64(), Some(100.0));
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn pending_close_remains_exposure_in_accounting_counts() -> anyhow::Result<()> {
        let (pool, path, path_str, cfg) = test_db("closing_exposure").await?;
        let mut row = trade(1, "live", "CLOSING", 20.0, 0.0, None);
        row.order_id = Some("pending-sale".into());
        db::insert_trade(&pool, &row).await?;
        let bot = TradingBot::new(cfg, pool).await?;
        let snapshot = bot.accounting_truth_snapshot(false).await;
        assert_eq!(snapshot["open_hot_count"].as_u64(), Some(1));
        assert_eq!(snapshot["open_live_locked_hot"].as_f64(), Some(20.0));
        assert_eq!(snapshot["open_live_locked_db"].as_f64(), Some(20.0));
        bot.db.close().await;
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn paper_balance_reset_rejects_live_and_boot_wallet_origin_without_writes(
    ) -> anyhow::Result<()> {
        for (mode, wallet_backed) in [("live", false), ("simulation", true), ("unknown", false)] {
            let (pool, path, path_str, cfg) = test_db("reset-origin").await?;
            let mut bot = TradingBot::new(cfg.clone(), pool).await?;
            let config_path = path.with_extension("json");
            cfg.save_runtime(config_path.to_str().unwrap())?;
            std::sync::Arc::get_mut(&mut bot.control_plane)
                .expect("fixture has no shared control-plane handle")
                .config_path = config_path.to_string_lossy().into_owned();
            let persisted_before = std::fs::read(&config_path)?;
            bot.control_plane.config.write().await.mode = mode.into();
            if wallet_backed {
                let auth = crate::clob::ClobAuth::new(
                    "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "public-api-key",
                    "cHVibGljLWZpeHR1cmUtc2VjcmV0",
                    "public-passphrase",
                )?;
                bot.clob = Some(std::sync::Arc::new(
                    crate::clob::ClobClient::with_test_host(auth, "http://127.0.0.1:9".into()),
                ));
            }
            let cash = bot.balance().await;
            let config_before = serde_json::to_value(&*bot.control_plane.config.read().await)?;
            for force in [false, true] {
                assert!(bot
                    .reset_paper_balance_direct(Some(150.0), force)
                    .await
                    .is_err());
            }
            assert_eq!(bot.balance().await, cash);
            assert_eq!(
                serde_json::to_value(&*bot.control_plane.config.read().await)?,
                config_before
            );
            assert_eq!(std::fs::read(&config_path)?, persisted_before);
            assert!(bot.trades().await.is_empty());
            let counts: (i64, i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM trades),(SELECT COUNT(*) FROM trade_fills),(SELECT COUNT(*) FROM live_orders),(SELECT COUNT(*) FROM realized_pnl_events)",
            ).fetch_one(&bot.db).await?;
            assert_eq!(counts, (0, 0, 0, 0));
            assert!(bot.kill_switch_active());
            assert_eq!(
                bot.kill_switch_status().1.unwrap().source,
                "paper_live_accounting_conflict"
            );
            bot.db.close().await;
            cleanup_db(&path, &path_str);
            std::fs::remove_file(config_path)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn isolated_paper_balance_reset_still_persists_and_survives_restart() -> anyhow::Result<()>
    {
        for mode in ["paper", "shadow", "simulation"] {
            let (pool, path, path_str, mut cfg) = test_db("reset-valid-origin").await?;
            cfg.mode = mode.into();
            let mut bot = TradingBot::new(cfg, pool).await?;
            let config_path = path.with_extension("json");
            std::sync::Arc::get_mut(&mut bot.control_plane)
                .expect("fixture has no shared control-plane handle")
                .config_path = config_path.to_string_lossy().into_owned();
            let result = bot.reset_paper_balance_direct(Some(150.0), false).await?;
            assert_eq!(result["new_balance"], 150.0);
            assert_eq!(bot.balance().await, 150.0);
            let persisted: Config = serde_json::from_slice(&std::fs::read(&config_path)?)?;
            assert_eq!(persisted.initial_balance, 150.0);
            let restarted = TradingBot::new(persisted, bot.db.clone()).await?;
            assert_eq!(restarted.balance().await, 150.0);
            assert!(!bot.kill_switch_active());
            bot.db.close().await;
            cleanup_db(&path, &path_str);
            std::fs::remove_file(config_path)?;
        }
        Ok(())
    }
}

impl TradingBot {
    /// Paper cash belongs only to a process that never acquired a live client.
    /// The client is a boot-time identity; changing config cannot turn its cash
    /// into a simulation balance.
    pub(crate) async fn ensure_paper_execution_origin(&self, operation: &str) -> bool {
        if self.paper_execution_allowed().await {
            return true;
        }
        self.reject_paper_execution_origin(operation).await;
        false
    }

    pub(crate) async fn reject_paper_execution_origin(&self, operation: &str) {
        self.set_degraded_no_trade(true);
        self.halt_trading(
            "paper_live_accounting_conflict",
            format!("{operation} requires an isolated non-live process without wallet-backed cash"),
        )
        .await;
    }

    /// Execution-actor-only reset: keep ledger inspection, capital origin and
    /// available-cash publication ordered with entries and exits.
    pub(crate) async fn reset_paper_balance_direct(
        &self,
        amount: Option<f64>,
        force: bool,
    ) -> anyhow::Result<Value> {
        anyhow::ensure!(
            self.ensure_paper_execution_origin("paper balance reset")
                .await,
            "balance reset blocked in live or wallet-backed mode"
        );
        let mut config = self.control_plane.config.write().await;
        // Do not reenter paper_execution_allowed while holding its config lock.
        // Keep this write lease through persistence and cash publication so a
        // concurrent config change cannot switch the origin during the reset.
        if self.clob.is_some()
            || !matches!(
                config.mode_kind(),
                BotMode::Paper | BotMode::Shadow | BotMode::Simulation
            )
        {
            self.reject_paper_execution_origin("paper balance reset")
                .await;
            anyhow::bail!("balance reset blocked in live or wallet-backed mode");
        }
        let requested = amount.unwrap_or(config.initial_balance);
        anyhow::ensure!(
            requested.is_finite() && requested >= 0.0,
            "amount must be finite and nonnegative"
        );
        let components = db::balance_reconstruction_components(&self.db).await?;
        let locked = components.open_non_live_locked;
        let open_trades = self.portfolio_state.trades.read().await.len();
        let mut candidate = config.clone();
        candidate.initial_balance = if force {
            requested + locked - components.open_non_live_realized
        } else {
            requested
        };
        let new_balance = self.prepare_initial_balance_update(&mut candidate).await?;
        candidate.normalize_and_validate()?;
        candidate.save_runtime(&self.control_plane.config_path)?;
        *config = candidate;
        self.set_balance(new_balance).await;
        Ok(
            json!({"ok":true,"requested_balance":requested,"locked_open_amount":locked,
            "new_balance":new_balance,"open_trades":open_trades,"force":force}),
        )
    }
    /// Reconstruct logical paper/simulation balance from closed-trade PnL.
    /// Deducts both trade amount and entry fee_paid for open positions so
    /// the balance is accurate after a restart.
    pub(crate) fn compute_balance_from_trades(trades: &[Trade], initial: f64) -> f64 {
        let mut bal = initial;
        for t in trades {
            if t.mode_kind() != BotMode::Live {
                if t.has_unsettled_exposure() {
                    // Locked collateral + entry fees paid upfront
                    bal -= t.amount + t.fee_paid;
                    // Partial closes realise PnL while the position is still OPEN.
                    bal += t.pnl.unwrap_or(0.0);
                } else if t.status_kind() == TradeStatus::Closed {
                    if let Some(pnl) = t.pnl {
                        // Closed-trade PnL excludes entry fees in paper mode.
                        bal += pnl - t.fee_paid;
                    }
                }
            }
        }
        bal.max(0.0)
    }

    pub(crate) async fn closed_non_live_balance_delta(pool: &SqlitePool) -> anyhow::Result<f64> {
        let delta = sqlx::query_scalar::<_, f64>(
            r#"SELECT COALESCE(SUM(COALESCE(pnl, 0.0) - COALESCE(fee_paid, 0.0)), 0.0)
               FROM trades
               WHERE status = 'CLOSED' AND mode != 'live'"#,
        )
        .fetch_one(pool)
        .await?;
        anyhow::ensure!(delta.is_finite(), "nonfinite closed paper balance history");
        Ok(delta)
    }

    pub async fn last_balance_drift_usd(&self) -> Option<f64> {
        *self.runtime_state.last_balance_drift_usd.read().await
    }

    pub async fn accounting_truth_snapshot(&self, sync_authoritative: bool) -> Value {
        let cfg = self.control_plane.config.read().await.clone();
        let mode = cfg.mode_kind();
        let reconstruction = db::balance_reconstruction_components(&self.db).await;
        let reconstruction_error = reconstruction.as_ref().err().map(ToString::to_string);
        let comps = reconstruction.ok();
        let current_balance = self.balance().await;
        let open_hot = self.portfolio_state.trades.read().await.clone();

        let derived_restart_balance = comps.as_ref().map(|c| {
            cfg.initial_balance + c.closed_non_live_delta - c.open_non_live_locked
                + c.open_non_live_realized
        });
        let derived_hot_balance = comps.as_ref().map(|c| {
            Self::compute_balance_from_trades(
                &open_hot,
                cfg.initial_balance + c.closed_non_live_delta,
            )
        });

        let mut authoritative_live_balance = None;
        if sync_authoritative && mode == BotMode::Live {
            authoritative_live_balance = self.sync_balance().await;
        }

        let drift_vs_authoritative = authoritative_live_balance.map(|auth| current_balance - auth);
        if let Some(drift) = drift_vs_authoritative {
            *self.runtime_state.last_balance_drift_usd.write().await = Some(drift);
            let warn_threshold = Self::env_f64("MACBOT_BALANCE_DRIFT_WARN_USD", 0.05).abs();
            if drift.abs() >= warn_threshold {
                metrics::inc_balance_drift_warns();
                warn!(
                    "Accounting drift detected: current=${:.4} authoritative=${:.4} drift=${:+.4}",
                    current_balance,
                    authoritative_live_balance.unwrap_or(0.0),
                    drift
                );
            }
        }

        json!({
            "mode": cfg.mode,
            "execution_mode": cfg.execution_mode,
            "initial_balance": cfg.initial_balance,
            "current_balance": current_balance,
            "derived_restart_balance": derived_restart_balance,
            "derived_hot_balance": derived_hot_balance,
            "components": comps,
            "reconstruction_error": reconstruction_error,
            "open_hot_count": open_hot.iter().filter(|t| t.has_unsettled_exposure()).count(),
            "open_live_locked_hot": open_hot
                .iter()
                .filter(|t| t.has_unsettled_exposure() && t.is_live())
                .map(|t| t.amount)
                .sum::<f64>(),
            "open_live_locked_db": comps.as_ref().map(|c| c.open_live_locked),
            "authoritative_live_balance": authoritative_live_balance,
            "drift_vs_authoritative_usd": drift_vs_authoritative,
            "degraded_no_trade": self.degraded_no_trade_active(),
            "kill_switch": self.kill_switch_active(),
            "generated_at": Utc::now().to_rfc3339(),
        })
    }

    /// Prepare a reset without changing shared state. The caller validates and
    /// persists this candidate before publishing the config and balance.
    pub(crate) async fn prepare_initial_balance_update(
        &self,
        candidate: &mut crate::config::Config,
    ) -> anyhow::Result<f64> {
        let desired = candidate.initial_balance;
        let closed_delta = db::balance_reconstruction_components(&self.db)
            .await?
            .closed_non_live_delta;
        candidate.initial_balance = desired - closed_delta;
        let open_trades = self.portfolio_state.trades.read().await.clone();
        Ok(Self::compute_balance_from_trades(&open_trades, desired))
    }
}
