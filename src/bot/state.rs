//! TradingBot — central shared state for the entire trading system.
//! Holds DB pool, CLOB client, trade list, balance, config, caches,
//! and all signal trackers as Arc-wrapped async primitives.

use std::time::Instant;

use std::sync::Arc;

use super::intelligence::BotIntelligence;
use crate::clob::ClobClient;
#[cfg(test)]
use crate::db::Trade;
use sqlx::SqlitePool;

mod analysis_cycle;
mod analysis_state;
mod bootstrap;
mod control_plane;
mod execution_actor;
mod execution_coordination;
mod market_state;
mod portfolio_state;
mod runtime;
mod runtime_slo;
mod runtime_state;
pub use runtime_state::{drain_or_deadline, shutdown_begun};
mod signal_trackers;
mod strategy_services;
mod tp_sl;
mod trade_lifecycle;
mod trade_loop;

use analysis_state::AnalysisState;
use control_plane::ControlPlaneState;
pub(crate) use control_plane::StreamTokenEntry;
use execution_coordination::ExecutionState;
use market_state::MarketState;
pub(crate) use market_state::StickyAnalysisMarket;
use portfolio_state::PortfolioState;
#[cfg(test)]
use runtime_slo::RuntimeSloParams;
pub(crate) use runtime_state::{ReconcileError, ReconcileSummary, RuntimeState};
use signal_trackers::SignalTrackers;
use strategy_services::StrategyServices;
#[cfg(test)]
pub(crate) use trade_lifecycle::PartialCloseIntent;
pub(crate) use trade_lifecycle::PendingSimExitEvidence;
use trade_lifecycle::TradeLifecycleState;

// ── Cache wrapper types ───────────────────────────────────────────────────────

pub struct CandlesCache {
    pub ts: Instant,
    pub candles: Vec<crate::signals::Candle>,
}

pub struct MlCandlesCache {
    pub ts: Instant,
    pub candles: Vec<crate::signals::fetchers::BinanceCandle>,
}

pub struct FuturesCache {
    pub ts: Instant,
    pub funding_rate: Option<f64>,
    pub open_interest: Option<f64>,
    pub prev_open_interest: Option<f64>,
    pub basis_pct: Option<f64>,
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
}

pub struct PcrCache {
    pub ts: Instant,
    pub put_oi: f64,
    pub call_oi: f64,
    pub provider_timestamp_ms: i64,
}

pub struct FgCache {
    pub ts: Instant,
    pub value: u32,
}

pub struct AggressorCache {
    pub ts: Instant,
    pub buy_vol: f64,
    pub sell_vol: f64,
}

pub struct CoingeckoCache {
    pub ts: Instant,
    pub up_pct: f64,
}

pub struct FinnhubCache {
    pub ts: Instant,
    pub score: f64,
}

pub struct LunarcrushCache {
    pub ts: Instant,
    pub score: f64,
    pub social_volume: Option<f64>,
}

pub struct NewsCache {
    pub ts: Instant,
    pub signal: crate::signals::NewsSignal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActorCallError {
    Unavailable,
    SendFailed,
    ResponseFailed,
    PartialClosePending,
    PairNotMergeable,
}

impl std::fmt::Display for ActorCallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "execution actor unavailable"),
            Self::SendFailed => write!(f, "execution actor send failed"),
            Self::ResponseFailed => write!(f, "execution actor response failed"),
            Self::PartialClosePending => write!(f, "partial close awaits a confirmed outcome"),
            Self::PairNotMergeable => write!(
                f,
                "pair no longer has equal complementary non-live inventory"
            ),
        }
    }
}

impl std::error::Error for ActorCallError {}

// ── TradingBot ────────────────────────────────────────────────────────────────

pub struct TradingBot {
    pub control_plane: Arc<ControlPlaneState>,
    pub portfolio_state: Arc<PortfolioState>,
    pub db: SqlitePool,
    pub clob: Option<Arc<ClobClient>>,
    pub runtime_state: Arc<RuntimeState>,
    pub signal_trackers: Arc<SignalTrackers>,

    /// Shared analysis snapshots, timestamps, and signal-weight reload markers.
    pub analysis_state: Arc<AnalysisState>,
    /// Shared market caches and token/BTC lookup state.
    pub market_state: Arc<MarketState>,

    /// Single-writer actor handle, coalesced BBO queue, and execution lock.
    pub execution_state: Arc<ExecutionState>,

    /// Cold-path ML, logging, calibration, and ensemble state.
    pub intelligence: Arc<BotIntelligence>,
    /// TP/SL caches and circuit-breaker cooldown state for open-trade lifecycle.
    pub trade_lifecycle: Arc<TradeLifecycleState>,

    /// Strategy-side schedulers, trackers, routers, and short-horizon caches.
    pub strategy_services: Arc<StrategyServices>,
}

impl TradingBot {
    fn env_u64(name: &str, default: u64) -> u64 {
        crate::env_util::env_u64(name, default)
    }

    pub(crate) fn env_f64(name: &str, default: f64) -> f64 {
        crate::env_util::env_f64(name, default)
    }

    fn env_bool(name: &str, default: bool) -> bool {
        crate::env_util::env_bool(name, default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::idempotency::IdempotencyEntry;
    use chrono::Utc;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    struct EnvRestore {
        key: &'static str,
        prev: Option<String>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            if let Some(prev) = self.prev.take() {
                std::env::set_var(self.key, prev);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

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

    fn shadow_closed_trade(id: i64, pnl: f64) -> Trade {
        let mut t = trade(id, "simulation", "CLOSED", 1.0, 0.0, Some(pnl));
        t.source = "shadow".to_string();
        t
    }

    #[test]
    fn compute_balance_reconstructs_fees_and_partials() {
        let trades = vec![
            // Closed paper trade: +pnl minus entry fee.
            trade(1, "simulation", "CLOSED", 0.0, 2.0, Some(10.0)),
            // Open partially-closed trade: locked amount + fee, plus realised pnl on the open row.
            trade(2, "simulation", "OPEN", 20.0, 1.0, Some(3.0)),
            // Live trades must not affect paper/sim reconstruction.
            trade(3, "live", "CLOSED", 0.0, 100.0, Some(999.0)),
        ];

        let bal = TradingBot::compute_balance_from_trades(&trades, 100.0);

        // 100 + (10 - 2) - (20 + 1) + 3 = 90
        assert!((bal - 90.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn new_loads_open_trades_only_and_restores_balance_from_db() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_open_only_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 10.0;

        let closed = trade(1, "simulation", "CLOSED", 0.0, 0.10, Some(2.00));
        let open = trade(2, "simulation", "OPEN", 1.00, 0.05, Some(0.20));
        let _ = crate::db::insert_trade(&pool, &closed).await?;
        let _ = crate::db::insert_trade(&pool, &open).await?;

        let bot = TradingBot::new(cfg, pool).await?;
        let in_mem = bot.portfolio_state.trades.read().await.clone();
        assert_eq!(in_mem.len(), 1, "only OPEN trades should be in hot memory");
        assert_eq!(in_mem[0].status_kind(), crate::domain::TradeStatus::Open);

        // expected balance:
        // initial (10.00)
        // + closed realised delta (2.00 - 0.10 = 1.90)
        // - open locked collateral/fees (1.00 + 0.05)
        // + open realised partial pnl (+0.20)
        // => 11.05
        let bal = bot.balance().await;
        assert!((bal - 11.05).abs() < 1e-9, "balance mismatch: {bal}");

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn persisted_open_rebuilds_cleanly_after_restart() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("macbot_persisted_open_restart_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 25.0;

        let bot = TradingBot::new(cfg.clone(), pool.clone()).await?;
        let pre_crash_balance = bot.balance().await;
        assert!((pre_crash_balance - 25.0).abs() < 1e-9);

        let persisted = trade(11, "simulation", "OPEN", 5.0, 0.25, Some(0.0));
        let trade_id = crate::db::insert_trade(&bot.db, &persisted).await?;
        assert!(
            bot.portfolio_state
                .trades
                .read()
                .await
                .iter()
                .all(|t| t.id != trade_id),
            "hot state should still reflect the pre-crash empty portfolio"
        );
        assert!((bot.balance().await - pre_crash_balance).abs() < 1e-9);

        drop(bot);

        let restarted = TradingBot::new(cfg, pool).await?;
        let expected_balance = TradingBot::compute_balance_from_trades(
            &[crate::db::load_trade_by_id(&restarted.db, trade_id)
                .await?
                .expect("persisted open trade")],
            25.0,
        );
        assert!((restarted.balance().await - expected_balance).abs() < 1e-9);
        let restored = restarted
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == trade_id)
            .cloned()
            .expect("restarted open trade should be loaded");
        assert!(restored.is_open());
        assert_eq!(restored.strategy, "conviction");

        drop(restarted);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn persisted_maker_passive_fill_rebuilds_cleanly_after_restart() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("macbot_maker_persisted_open_restart_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 25.0;

        let bot = TradingBot::new(cfg.clone(), pool.clone()).await?;
        let pre_crash_balance = bot.balance().await;

        let mut persisted = trade(12, "simulation", "OPEN", 3.0, 0.0, Some(0.0));
        persisted.order_id = Some("maker-order-restart".to_string());
        persisted.token_id = Some("123456".to_string());
        persisted.strategy = "maker".to_string();
        persisted.reason = "maker passive fill crossed_book".to_string();
        let trade_id = crate::db::insert_trade(&bot.db, &persisted).await?;
        assert!(
            bot.portfolio_state
                .trades
                .read()
                .await
                .iter()
                .all(|t| t.id != trade_id),
            "hot state should still be stale before restart"
        );
        assert!((bot.balance().await - pre_crash_balance).abs() < 1e-9);

        drop(bot);

        let restarted = TradingBot::new(cfg, pool).await?;
        let persisted = crate::db::load_trade_by_id(&restarted.db, trade_id)
            .await?
            .expect("persisted maker trade");
        let expected_balance = TradingBot::compute_balance_from_trades(&[persisted.clone()], 25.0);
        assert!((restarted.balance().await - expected_balance).abs() < 1e-9);
        let restored = restarted
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == trade_id)
            .cloned()
            .expect("restarted maker trade should be loaded");
        assert!(restored.is_open());
        assert_eq!(restored.strategy, "maker");
        assert_eq!(restored.order_id.as_deref(), Some("maker-order-restart"));

        drop(restarted);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn new_replays_fill_ledger_into_open_live_trade_on_restart() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_live_fill_replay_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 10.0;

        let mut live_open = trade(7, "live", "OPEN", 5.0, 0.0, None);
        live_open.order_id = Some("oid-restart-fill".to_string());
        live_open.token_id = Some("tok-restart-fill".to_string());
        live_open.entry_price = 0.45;
        live_open.shares = 10.0;
        let trade_id = crate::db::insert_trade(&pool, &live_open).await?;

        let _ = crate::db::append_trade_fill_snapshot(
            &pool,
            trade_id,
            &live_open.market_id,
            live_open.order_id.as_deref().unwrap_or_default(),
            "entry",
            0.50,
            0.62,
            2.0,
            "test",
            "partial fill snapshot 1",
        )
        .await?;
        let _ = crate::db::append_trade_fill_snapshot(
            &pool,
            trade_id,
            &live_open.market_id,
            live_open.order_id.as_deref().unwrap_or_default(),
            "entry",
            0.50,
            0.58,
            5.0,
            "test",
            "partial fill snapshot 2",
        )
        .await?;

        let bot = TradingBot::new(cfg, pool.clone()).await?;
        let restored = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == trade_id)
            .cloned()
            .expect("replayed trade should be loaded");
        assert!(
            (restored.shares - 5.0).abs() < 1e-9,
            "expected shares repaired from fill ledger"
        );
        assert!(
            (restored.entry_price - 0.58).abs() < 1e-9,
            "expected entry price repaired from fill ledger"
        );

        let persisted = crate::db::load_trade_by_id(&pool, trade_id)
            .await?
            .expect("persisted repaired trade");
        assert!((persisted.shares - 5.0).abs() < 1e-9);
        assert!((persisted.entry_price - 0.58).abs() < 1e-9);

        let outbox: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT stage, action, details FROM order_outbox WHERE order_id = ? ORDER BY id",
        )
        .bind("oid-restart-fill")
        .fetch_all(&pool)
        .await?;
        assert!(
            outbox.iter().any(|(stage, action, details)| {
                stage == "restart_fill_replay"
                    && action == "repair_trade_from_fill_ledger"
                    && details.contains("trade_id=")
            }),
            "expected restart fill replay audit row"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn new_clears_order_id_for_fully_filled_live_trade_on_restart() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_live_fill_complete_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 10.0;

        let mut live_open = trade(8, "live", "OPEN", 5.0, 0.0, None);
        live_open.order_id = Some("oid-restart-complete".to_string());
        live_open.token_id = Some("tok-restart-complete".to_string());
        live_open.entry_price = 0.50;
        live_open.shares = 0.0;
        let trade_id = crate::db::insert_trade(&pool, &live_open).await?;

        let _ = crate::db::append_trade_fill_snapshot(
            &pool,
            trade_id,
            &live_open.market_id,
            live_open.order_id.as_deref().unwrap_or_default(),
            "entry",
            0.50,
            0.50,
            10.0,
            "test",
            "full fill snapshot",
        )
        .await?;

        let bot = TradingBot::new(cfg, pool.clone()).await?;
        let restored = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == trade_id)
            .cloned()
            .expect("restored trade");
        assert_eq!(restored.order_id, None);
        assert!((restored.shares - 10.0).abs() < 1e-9);

        let persisted = crate::db::load_trade_by_id(&pool, trade_id)
            .await?
            .expect("persisted trade");
        assert_eq!(persisted.order_id, None);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn accounting_truth_snapshot_matches_reconstructed_balance() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_accounting_snapshot_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 10.0;

        let closed = trade(1, "simulation", "CLOSED", 0.0, 0.10, Some(2.00));
        let open = trade(2, "simulation", "OPEN", 1.00, 0.05, Some(0.20));
        let _ = crate::db::insert_trade(&pool, &closed).await?;
        let _ = crate::db::insert_trade(&pool, &open).await?;

        let bot = TradingBot::new(cfg, pool).await?;
        let snapshot = bot.accounting_truth_snapshot(false).await;

        assert_eq!(snapshot["mode"], "simulation");
        assert!((snapshot["current_balance"].as_f64().unwrap_or_default() - 11.05).abs() < 1e-9);
        assert!(
            (snapshot["derived_restart_balance"]
                .as_f64()
                .unwrap_or_default()
                - 11.05)
                .abs()
                < 1e-9
        );
        assert!(
            (snapshot["derived_hot_balance"].as_f64().unwrap_or_default() - 11.05).abs() < 1e-9
        );
        assert_eq!(snapshot["open_hot_count"].as_u64(), Some(1));
        assert_eq!(
            snapshot["components"]["closed_non_live_delta"]
                .as_f64()
                .unwrap_or_default(),
            1.9
        );
        assert_eq!(
            snapshot["components"]["open_non_live_locked"]
                .as_f64()
                .unwrap_or_default(),
            1.05
        );
        assert_eq!(
            snapshot["components"]["open_non_live_realized"]
                .as_f64()
                .unwrap_or_default(),
            0.2
        );
        assert_eq!(
            snapshot["authoritative_live_balance"],
            serde_json::Value::Null
        );
        assert_eq!(
            snapshot["drift_vs_authoritative_usd"],
            serde_json::Value::Null
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn serialized_execute_close_cycle_updates_balance_consistently() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_cycle_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.auto_trading = true;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.8;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool).await?);
        bot.start_execution_actor().await;

        let initial = bot.balance().await;
        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.95,
            "raw_confidence": 0.95,
            "net_ev": 0.20,
            "forced": true,
            "reason": "integration-test",
            "regime": "BULL",
            "p_up": 0.71,
            "btc_change_pct": 1.2,
            "market_data": {
                "market_id": "mkt-int-1",
                "question": "test-market",
                "price": 0.55,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 20000.0
            },
            "signals": {"orderbook_imbalance": 0.6}
        });

        let opened = bot
            .execute_trade_serialized(&analysis, false, Some(1.0))
            .await
            .expect("trade should open");
        assert!(opened.is_open());

        let bal_after_open = bot.balance().await;
        let expected_open = initial - opened.amount - opened.fee_paid;
        assert!((bal_after_open - expected_open).abs() < 1e-9);

        let closed = bot
            .close_trade_serialized(opened.id, Some(0.80))
            .await
            .expect("trade should close");
        assert_eq!(closed.status_kind(), crate::domain::TradeStatus::Closed);
        assert!(closed.pnl.unwrap_or(0.0) > 0.0);

        let final_bal = bot.balance().await;
        // Paper/sim accounting model:
        // final = initial + closed.pnl - entry_fee
        let expected_final = initial + closed.pnl.unwrap_or(0.0) - closed.fee_paid;
        assert!((final_bal - expected_final).abs() < 1e-9);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn ml_on_resolution_records_edge_tracker_observation() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_edge_tracker_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.auto_trading = true;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.8;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        bot.start_execution_actor().await;

        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.95,
            "raw_confidence": 0.95,
            "net_ev": 0.20,
            "forced": true,
            "reason": "edge-tracker-test",
            "regime": "BULL",
            "p_up": 0.71,
            "btc_change_pct": 1.2,
            "market_data": {
                "market_id": "mkt-edge-1",
                "question": "edge-tracker-market",
                "price": 0.55,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 20000.0
            },
            "signals": {"orderbook_imbalance": 0.6}
        });

        let opened = bot
            .execute_trade_serialized(&analysis, false, Some(1.0))
            .await
            .expect("trade should open");
        let mut closed = bot
            .close_trade_serialized(opened.id, Some(0.80))
            .await
            .expect("trade should close");
        closed.was_correct = Some(1);
        closed.exit_type = Some("resolution".into());
        if closed.signals_json.is_none() {
            closed.signals_json = Some(r#"{"orderbook_imbalance":0.6}"#.to_string());
        }
        let archived_at = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO ml_model_inputs (timestamp, run_id, market_id, signal, regime_label, phase, fusion_score, p_up, regime_value, features_json, scalars_json, staleness_json, candles_1m_json, candles_5m_json, candles_15m_json, orderbook_seq_json, outcome) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&archived_at)
        .bind("run-test")
        .bind(&closed.market_id)
        .bind("BUY_YES")
        .bind("TREND_UP")
        .bind("shadow")
        .bind(0.20_f64)
        .bind(0.71_f64)
        .bind(1.0_f64)
        .bind(r#"{"orderbook_imbalance":0.6}"#)
        .bind("[0.6]")
        .bind("[0.0]")
        .bind("[]")
        .bind("[]")
        .bind("[]")
        .bind("[]")
        .bind(Option::<f64>::None)
        .execute(&bot.db)
        .await?;
        bot.ml_on_resolution(&closed).await;

        let edge_tracker = bot.intelligence.edge_tracker.lock().await.to_json();
        let mean_predicted = edge_tracker["mean_predicted_edge"].as_f64().unwrap_or(-1.0);
        let mean_actual = edge_tracker["mean_actual_edge"].as_f64().unwrap_or(-1.0);
        let expected_actual = closed.net_realized_pnl() / closed.amount;

        assert_eq!(edge_tracker["observations"], 1);
        assert!((mean_predicted - opened.expected_value).abs() < 1e-9);
        assert!(
            (mean_predicted - 0.20).abs() > 1e-3,
            "learning uses actual-price expected return, not the stale analysis estimate"
        );
        assert!((mean_actual - expected_actual).abs() < 1e-9);
        let archived_outcome: Option<f64> =
            sqlx::query_scalar("SELECT outcome FROM ml_model_inputs WHERE market_id = ? LIMIT 1")
                .bind(&closed.market_id)
                .fetch_one(&bot.db)
                .await?;
        assert_eq!(archived_outcome, Some(1.0));

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn serialized_execute_partial_then_full_close_cycle_is_consistent() -> anyhow::Result<()>
    {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_partial_cycle_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.auto_trading = true;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.8;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool).await?);
        bot.start_execution_actor().await;

        let initial = bot.balance().await;
        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.95,
            "raw_confidence": 0.95,
            "net_ev": 0.20,
            "forced": true,
            "reason": "integration-test-partial",
            "regime": "BULL",
            "p_up": 0.71,
            "btc_change_pct": 1.2,
            "market_data": {
                "market_id": "mkt-int-partial-1",
                "question": "test-market-partial",
                "price": 0.55,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 20000.0
            },
            "signals": {"orderbook_imbalance": 0.6}
        });
        let parsed = crate::analysis::AnalysisResult::from_value(&analysis);

        let opened = bot
            .execute_trade_serialized_parsed(&parsed, false, Some(1.0))
            .await
            .expect("trade should open");
        assert!(opened.is_open());

        let partial = bot
            .partial_close_trade(opened.id, 0.75, 0.5)
            .await
            .expect("partial close should succeed");
        let remaining_amount = partial
            .get("remaining_amount")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        assert!((remaining_amount - (opened.amount * 0.5)).abs() < 1e-9);

        let trade_after_partial = {
            let trades = bot.portfolio_state.trades.read().await;
            trades
                .iter()
                .find(|t| t.id == opened.id)
                .cloned()
                .expect("trade should exist after partial")
        };
        assert!(trade_after_partial.is_open());
        assert_eq!(trade_after_partial.partial_closed, Some(1));

        let closed = bot
            .close_trade_serialized(opened.id, Some(0.85))
            .await
            .expect("trade should close after partial");
        assert_eq!(closed.status_kind(), crate::domain::TradeStatus::Closed);

        let final_bal = bot.balance().await;
        // Accounting model with partial + full:
        // final = initial + total_pnl - entry_fee
        // where total_pnl includes realised partial pnl + remaining close pnl.
        let expected_final = initial + closed.pnl.unwrap_or(0.0) - closed.fee_paid;
        assert!((final_bal - expected_final).abs() < 1e-9);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn paper_trade_rejects_when_amount_plus_fees_exceeds_balance() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_fee_guard_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.auto_trading = true;
        cfg.kelly_sizing = false;
        cfg.initial_balance = 1.0;
        cfg.trade_amount = 1.0;
        cfg.min_trade_usd = 0.25;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.8;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool).await?);
        bot.start_execution_actor().await;
        let initial = bot.balance().await;

        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.95,
            "raw_confidence": 0.95,
            "net_ev": 0.20,
            "reason": "fee-guard-test",
            "regime": "BULL",
            "p_up": 0.70,
            "btc_change_pct": 1.0,
            "market_data": {
                "market_id": "mkt-int-fee",
                "question": "fee-guard-market",
                "price": 0.50,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 20000.0
            },
            "signals": {"orderbook_imbalance": 0.6}
        });

        let opened = bot.execute_trade_serialized(&analysis, false, None).await;
        assert!(
            opened.is_none(),
            "trade should be rejected by amount+fees guard"
        );
        assert!((bot.balance().await - initial).abs() < 1e-9);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn shadow_kill_reason_trips_on_low_win_rate() -> anyhow::Result<()> {
        // Enable the shadow kill check (gated behind env var)
        let _guard = crate::env_util::test_env_lock();
        let _restore = EnvRestore::set("MACBOT_SHADOW_KILL_ENABLED", "1");
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_shadow_kill_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.shadow_trading = true;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool).await?);
        let mut sample = Vec::new();
        for i in 0..200_i64 {
            // 45% wins, 55% losses.
            let pnl = if i < 90 { 0.02 } else { -0.02 };
            sample.push(shadow_closed_trade(i + 1, pnl));
        }
        for t in sample {
            let _ = crate::db::insert_trade(&bot.db, &t).await?;
        }

        let reason = bot.shadow_kill_reason(&cfg).await;
        assert!(reason.is_some(), "expected shadow kill reason");
        assert!(
            reason.unwrap_or_default().contains("win_rate"),
            "expected win-rate trigger"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn shadow_kill_reason_allows_healthy_profile() -> anyhow::Result<()> {
        let _guard = crate::env_util::test_env_lock();
        let _restore = EnvRestore::set("MACBOT_SHADOW_KILL_ENABLED", "1");
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_shadow_ok_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.shadow_trading = true;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool).await?);
        let mut sample = Vec::new();
        for i in 0..200_i64 {
            // 60% wins, positive expectancy, PF > 1.03.
            let pnl = if i % 5 < 3 { 0.03 } else { -0.015 };
            sample.push(shadow_closed_trade(i + 1, pnl));
        }
        for t in sample {
            let _ = crate::db::insert_trade(&bot.db, &t).await?;
        }

        let reason = bot.shadow_kill_reason(&cfg).await;
        assert!(
            reason.is_none(),
            "unexpected shadow kill reason: {:?}",
            reason
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_slo_l2_breach_fail_closes_live_maker() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_runtime_slo_l2_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        bot.set_l2_feed_stale(true).await;
        assert!(!bot.kill_switch_active());

        bot.enforce_runtime_slos_with(RuntimeSloParams {
            enforce: true,
            max_l2_age_ms: 10,
            l2_breach_hold_ms: 0,
            kill_on_l2_breach: true,
            kill_after_l2_trip_count: 1,
            kill_after_backpressure_trip_count: u32::MAX,
            recovery_hold_ms: 0,
            max_actor_cmd_drop_delta: u64::MAX,
            max_outbox_drop_delta: u64::MAX,
        })
        .await;

        assert!(bot.kill_switch_active());

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn runtime_slo_backpressure_delta_trips_kill_switch() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_runtime_slo_bp_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "hybrid".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        bot.mark_l2_feed_event().await;

        // Prime counter baseline (first pass should not trip on historical totals).
        bot.enforce_runtime_slos_with(RuntimeSloParams {
            enforce: true,
            max_l2_age_ms: 60_000,
            l2_breach_hold_ms: 60_000,
            kill_on_l2_breach: false,
            kill_after_l2_trip_count: u32::MAX,
            kill_after_backpressure_trip_count: 1,
            recovery_hold_ms: 0,
            max_actor_cmd_drop_delta: u64::MAX,
            max_outbox_drop_delta: u64::MAX,
        })
        .await;
        assert!(!bot.kill_switch_active());

        crate::metrics::inc_actor_cmd_drop_low();
        bot.enforce_runtime_slos_with(RuntimeSloParams {
            enforce: true,
            max_l2_age_ms: 60_000,
            l2_breach_hold_ms: 60_000,
            kill_on_l2_breach: false,
            kill_after_l2_trip_count: u32::MAX,
            kill_after_backpressure_trip_count: 1,
            recovery_hold_ms: 0,
            max_actor_cmd_drop_delta: 0,
            max_outbox_drop_delta: u64::MAX,
        })
        .await;

        assert!(bot.kill_switch_active());

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn actor_result_methods_report_transport_failures() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_actor_errs_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);

        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.9,
            "raw_confidence": 0.9,
            "net_ev": 0.1,
            "reason": "test",
            "market_data": {
                "market_id": "mkt",
                "question": "q",
                "price": 0.5,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 10000.0
            }
        });

        let r = bot
            .execute_trade_serialized_result(&analysis, false, Some(1.0))
            .await;
        assert!(matches!(r, Err(ActorCallError::Unavailable)));

        let c = bot.close_trade_serialized_result(1, None).await;
        assert!(matches!(c, Err(ActorCallError::Unavailable)));

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_persists_across_restart() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idem_restart_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();

        let bot1 = Arc::new(TradingBot::new(cfg.clone(), pool.clone()).await?);
        bot1.idempotency_put("idem-k".to_string(), json!({"ok": true}))
            .await;
        drop(bot1);

        let bot2 = Arc::new(TradingBot::new(cfg, pool).await?);
        let got = bot2.idempotency_get("idem-k").await;
        assert_eq!(got, Some(json!({"ok": true})));

        drop(bot2);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_claim_serializes_in_progress_and_cached_states() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idem_claim_states_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool.clone()).await?);
        assert!(matches!(
            bot.idempotency_claim_or_get("idem-claim").await?,
            crate::bot::idempotency::IdempotencyClaim::Claimed
        ));
        assert!(matches!(
            bot.idempotency_claim_or_get("idem-claim").await?,
            crate::bot::idempotency::IdempotencyClaim::InProgress
        ));

        bot.idempotency_put("idem-claim".to_string(), json!({"ok": true}))
            .await;
        let got = bot.idempotency_claim_or_get("idem-claim").await?;
        match got {
            crate::bot::idempotency::IdempotencyClaim::Cached(payload) => {
                assert_eq!(payload, json!({"ok": true}));
            }
            _ => panic!("expected cached idempotency payload after finalize"),
        }

        bot.idempotency_abort("idem-claim").await;
        assert!(matches!(
            bot.idempotency_claim_or_get("idem-claim").await?,
            crate::bot::idempotency::IdempotencyClaim::Claimed
        ));

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_get_retains_malformed_db_payload_as_unknown_execution(
    ) -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idem_bad_payload_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();

        let now_unix = Utc::now().timestamp();
        crate::db::upsert_idempotency_entry(&pool, "idem-bad", "{not-json", now_unix).await?;

        let bot = Arc::new(TradingBot::new(cfg, pool.clone()).await?);
        let got = bot.idempotency_get("idem-bad").await;
        assert!(got.is_none());
        assert!(crate::db::get_idempotency_entry(&pool, "idem-bad")
            .await?
            .is_some());
        assert!(matches!(
            bot.idempotency_claim_or_get("idem-bad").await?,
            crate::bot::idempotency::IdempotencyClaim::InProgress
        ));
        assert!(!bot.control_plane.idempotency_cache.contains_key("idem-bad"));

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_get_deletes_expired_db_payload() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idem_expired_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.idempotency_ttl_seconds = 1;

        let expired_unix = Utc::now().timestamp() - 60;
        crate::db::upsert_idempotency_entry(&pool, "idem-expired", r#"{"ok":true}"#, expired_unix)
            .await?;

        let bot = Arc::new(TradingBot::new(cfg, pool.clone()).await?);
        let got = bot.idempotency_get("idem-expired").await;
        assert!(got.is_none());
        assert!(crate::db::get_idempotency_entry(&pool, "idem-expired")
            .await?
            .is_none());
        assert!(!bot
            .control_plane
            .idempotency_cache
            .contains_key("idem-expired"));

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn prune_idempotency_cache_evicts_oldest_entries_first() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idem_prune_order_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.idempotency_ttl_seconds = 3_600;
        cfg.idempotency_max_entries = 2;

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        let now = Instant::now();
        bot.control_plane.idempotency_cache.insert(
            "oldest".to_string(),
            IdempotencyEntry {
                ts: now - Duration::from_secs(30),
                payload: json!({"seq": 1}),
            },
        );
        bot.control_plane.idempotency_cache.insert(
            "middle".to_string(),
            IdempotencyEntry {
                ts: now - Duration::from_secs(20),
                payload: json!({"seq": 2}),
            },
        );
        bot.control_plane.idempotency_cache.insert(
            "newest".to_string(),
            IdempotencyEntry {
                ts: now - Duration::from_secs(10),
                payload: json!({"seq": 3}),
            },
        );

        bot.prune_idempotency_cache().await;

        assert!(!bot.control_plane.idempotency_cache.contains_key("oldest"));
        assert!(bot.control_plane.idempotency_cache.contains_key("middle"));
        assert!(bot.control_plane.idempotency_cache.contains_key("newest"));

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn outbox_audit_fallback_writes_when_actor_not_started() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_outbox_fallback_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        bot.append_outbox_audit(
            "test_stage",
            "mkt-test",
            Some("oid-test"),
            Some("tok-test"),
            "test_action",
            "ok",
            "fallback path",
        )
        .await;

        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_outbox WHERE stage='test_stage' AND order_id='oid-test'",
        )
        .fetch_one(&bot.db)
        .await
        .unwrap_or(0);
        assert!(count >= 1, "expected fallback outbox write");

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn partial_close_state_rebuilds_cleanly_after_restart() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_partial_restart_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.auto_trading = true;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.8;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool.clone()).await?);
        bot.start_execution_actor().await;

        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.91,
            "raw_confidence": 0.91,
            "net_ev": 0.18,
            "forced": true,
            "reason": "restart-partial",
            "regime": "BULL",
            "p_up": 0.69,
            "btc_change_pct": 0.9,
            "market_data": {
                "market_id": "mkt-partial-restart-1",
                "question": "partial-restart-test",
                "price": 0.52,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 15000.0
            },
            "signals": {"orderbook_imbalance": 0.55}
        });
        let parsed = crate::analysis::AnalysisResult::from_value(&analysis);

        let opened = bot
            .execute_trade_serialized_parsed(&parsed, false, Some(1.0))
            .await
            .expect("trade should open");
        let partial = bot
            .partial_close_trade(opened.id, 0.80, 0.4)
            .await
            .expect("partial close should succeed");

        let remaining_amount = partial
            .get("remaining_amount")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let balance_after_partial = bot.balance().await;

        drop(bot);

        let restarted = TradingBot::new(cfg, pool).await?;
        let restarted_balance = restarted.balance().await;
        assert!(
            (restarted_balance - balance_after_partial).abs() < 1e-9,
            "restart balance drifted: before={balance_after_partial} after={restarted_balance}"
        );

        let restored_trade = restarted
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == opened.id)
            .cloned()
            .expect("partially closed trade should be restored");
        assert!(restored_trade.is_open());
        assert_eq!(restored_trade.partial_closed, Some(1));
        assert!((restored_trade.amount - remaining_amount).abs() < 1e-9);
        assert!(restored_trade.pnl.unwrap_or(0.0) > 0.0);

        drop(restarted);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn persisted_partial_close_rebuilds_cleanly_from_stale_hot_state() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("macbot_partial_stale_hot_restart_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.auto_trading = true;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.8;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool.clone()).await?);
        bot.start_execution_actor().await;

        let initial_balance = bot.balance().await;
        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.93,
            "raw_confidence": 0.93,
            "net_ev": 0.19,
            "forced": true,
            "reason": "restart-partial-stale-hot",
            "regime": "BULL",
            "p_up": 0.70,
            "btc_change_pct": 1.0,
            "market_data": {
                "market_id": "mkt-partial-stale-hot-1",
                "question": "partial-stale-hot-test",
                "price": 0.52,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 15000.0
            },
            "signals": {"orderbook_imbalance": 0.56}
        });
        let parsed = crate::analysis::AnalysisResult::from_value(&analysis);

        let opened = bot
            .execute_trade_serialized_parsed(&parsed, false, Some(1.0))
            .await
            .expect("trade should open");
        let balance_after_open = bot.balance().await;

        let mut persisted = crate::db::load_trade_by_id(&bot.db, opened.id)
            .await?
            .expect("persisted open trade");
        let fraction = 0.4;
        let closed_shares = persisted.shares * fraction;
        let remaining_shares = persisted.shares - closed_shares;
        let remaining_amount = remaining_shares * persisted.entry_price;
        let exit_price = 0.80;
        let exit_fee = crate::fusion::estimate_taker_fee(exit_price);
        let exit_value = closed_shares * exit_price;
        let partial_pnl =
            (closed_shares * (exit_price - persisted.entry_price)) - (exit_fee * exit_value);
        persisted.shares = remaining_shares;
        persisted.amount = remaining_amount;
        persisted.partial_closed = Some(1);
        persisted.pnl = Some(partial_pnl);
        crate::db::update_trade(&bot.db, &persisted).await?;

        let hot_trade = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == opened.id)
            .cloned()
            .expect("hot trade should still reflect pre-crash state");
        assert!((hot_trade.amount - opened.amount).abs() < 1e-9);
        assert!((bot.balance().await - balance_after_open).abs() < 1e-9);

        drop(bot);

        let restarted = TradingBot::new(cfg, pool).await?;
        let expected_balance =
            TradingBot::compute_balance_from_trades(&[persisted.clone()], initial_balance);
        assert!((restarted.balance().await - expected_balance).abs() < 1e-9);
        let restored = restarted
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == opened.id)
            .cloned()
            .expect("partially closed trade should be restored");
        assert!(restored.is_open());
        assert_eq!(restored.partial_closed, Some(1));
        assert!((restored.amount - remaining_amount).abs() < 1e-9);
        assert!((restored.shares - remaining_shares).abs() < 1e-9);
        assert!((restored.pnl.unwrap_or_default() - partial_pnl).abs() < 1e-9);

        drop(restarted);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn persisted_close_rebuilds_cleanly_after_restart() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_close_restart_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.auto_trading = true;
        cfg.min_entry_price = 0.2;
        cfg.max_entry_price = 0.8;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.trade_gas_usd = 0.0;

        let bot = Arc::new(TradingBot::new(cfg.clone(), pool.clone()).await?);
        bot.start_execution_actor().await;

        let initial_balance = bot.balance().await;
        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.92,
            "raw_confidence": 0.92,
            "net_ev": 0.19,
            "forced": true,
            "reason": "restart-close-window",
            "regime": "BULL",
            "p_up": 0.70,
            "btc_change_pct": 1.1,
            "market_data": {
                "market_id": "mkt-close-restart-1",
                "question": "close-restart-test",
                "price": 0.55,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 15000.0
            },
            "signals": {"orderbook_imbalance": 0.57}
        });
        let parsed = crate::analysis::AnalysisResult::from_value(&analysis);

        let opened = bot
            .execute_trade_serialized_parsed(&parsed, false, Some(1.0))
            .await
            .expect("trade should open");
        let balance_after_open = bot.balance().await;

        let mut persisted_closed = crate::db::load_trade_by_id(&bot.db, opened.id)
            .await?
            .expect("persisted open trade");
        let exit_price = 0.80;
        let exit_value = persisted_closed.shares * exit_price;
        let exit_fee = crate::fusion::estimate_taker_fee(exit_price) * exit_value;
        let persisted_pnl =
            (persisted_closed.shares * (exit_price - persisted_closed.entry_price)) - exit_fee;
        persisted_closed.set_status(crate::domain::TradeStatus::Closed);
        persisted_closed.exit_price = Some(exit_price);
        persisted_closed.exit_timestamp = Some(chrono::Utc::now().to_rfc3339());
        persisted_closed.pnl = Some(persisted_pnl);
        persisted_closed.exit_type = Some("manual".to_string());
        persisted_closed.was_correct = Some(1);
        crate::db::update_trade(&bot.db, &persisted_closed).await?;

        let hot_trade = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == opened.id)
            .cloned()
            .expect("hot trade should still exist before restart");
        assert!(
            hot_trade.is_open(),
            "hot state should still reflect the pre-crash open trade"
        );
        assert!(
            (bot.balance().await - balance_after_open).abs() < 1e-9,
            "hot balance should still reflect the pre-crash open state"
        );

        drop(bot);

        let restarted = TradingBot::new(cfg, pool).await?;
        let restarted_balance = restarted.balance().await;
        let expected_balance = initial_balance + persisted_pnl - persisted_closed.fee_paid;
        assert!(
            (restarted_balance - expected_balance).abs() < 1e-9,
            "restart close drifted: expected={expected_balance} after={restarted_balance}"
        );
        assert!(
            restarted.portfolio_state.trades.read().await.is_empty(),
            "persisted closed trade should not remain in hot open-trade memory after restart"
        );

        drop(restarted);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    // ── Integration: open → close → DB + integrity reconciliation ────────

    /// Helper: create a bot configured for paper simulation integration tests.
    async fn integration_bot(
        label: &str,
    ) -> anyhow::Result<(Arc<TradingBot>, std::path::PathBuf, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_integ_{label}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
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

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        bot.start_execution_actor().await;
        Ok((bot, path, path_str))
    }

    fn cleanup_sqlite(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    fn buy_yes_analysis(market_id: &str, price: f64) -> serde_json::Value {
        json!({
            "signal": "BUY_YES",
            "confidence": 0.95,
            "raw_confidence": 0.95,
            "net_ev": 0.20,
            "forced": true,
            "reason": "integration-test",
            "regime": "BULL",
            "p_up": 0.71,
            "btc_change_pct": 1.2,
            "market_data": {
                "market_id": market_id,
                "question": format!("test-market-{market_id}"),
                "price": price,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 20000.0
            },
            "signals": {"orderbook_imbalance": 0.6}
        })
    }

    #[tokio::test]
    async fn integration_open_close_db_matches_hot_state() -> anyhow::Result<()> {
        let (bot, path, path_str) = integration_bot("db_match").await?;
        let initial = bot.balance().await;

        // Open a trade
        let analysis = buy_yes_analysis("mkt-db-match", 0.55);
        let opened = bot
            .execute_trade_serialized(&analysis, false, Some(1.0))
            .await
            .expect("trade should open");
        assert!(opened.is_open());

        // Verify DB and hot state agree on open count
        let hot_open = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|t| t.is_open())
            .count();
        let db_open = crate::db::load_open_trades(&bot.db).await?.len();
        assert_eq!(hot_open, db_open, "hot vs DB open count after open");

        // Close the trade
        let closed = bot
            .close_trade_serialized(opened.id, Some(0.80))
            .await
            .expect("trade should close");
        assert_eq!(closed.status_kind(), crate::domain::TradeStatus::Closed);

        // Verify no open trades remain in hot state or DB
        let hot_open_after = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|t| t.is_open())
            .count();
        let db_open_after = crate::db::load_open_trades(&bot.db).await?.len();
        assert_eq!(hot_open_after, 0);
        assert_eq!(db_open_after, 0);

        // Verify DB trade is CLOSED with correct PnL
        let db_trade = crate::db::load_trade_by_id(&bot.db, opened.id)
            .await?
            .expect("closed trade should be in DB");
        assert_eq!(db_trade.status, "CLOSED");
        assert!(db_trade.pnl.is_some());
        assert!(db_trade.exit_price.is_some());
        assert!(db_trade.exit_timestamp.is_some());

        // Balance reconciliation
        let final_bal = bot.balance().await;
        let expected = initial + closed.pnl.unwrap_or(0.0) - closed.fee_paid;
        assert!(
            (final_bal - expected).abs() < 1e-9,
            "balance mismatch: got {final_bal}, expected {expected}"
        );

        drop(bot);
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integration_integrity_report_clean_after_lifecycle() -> anyhow::Result<()> {
        let (bot, path, path_str) = integration_bot("integrity_clean").await?;

        // Open and close a trade
        let analysis = buy_yes_analysis("mkt-integrity", 0.55);
        let opened = bot
            .execute_trade_serialized(&analysis, false, Some(1.0))
            .await
            .expect("trade should open");
        let _closed = bot
            .close_trade_serialized(opened.id, Some(0.80))
            .await
            .expect("trade should close");

        // Run integrity report — verify balance/position checks are clean.
        // The slow_recent_trade_ids_vs_db check uses a global cache that can
        // race between parallel tests, so we check individual check groups
        // rather than asserting report.ok == true.
        let report = crate::api::dashboard_integrity_report(&bot, false, 5).await;
        let mismatches = report
            .get("mismatches")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let balance_or_position_mismatches: Vec<_> = mismatches
            .iter()
            .filter(|m| {
                let check = m
                    .get("check")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                // Exclude cache-sensitive checks that race in parallel tests
                check != "slow_recent_trade_ids_vs_db" && check != "fast_last_trade_vs_db"
            })
            .collect();
        assert!(
            balance_or_position_mismatches.is_empty(),
            "unexpected integrity mismatches after lifecycle:\n{:?}",
            balance_or_position_mismatches
        );

        // Verify accounting snapshot agrees
        let accounting = bot.accounting_truth_snapshot(false).await;
        let current = accounting["current_balance"].as_f64().unwrap_or(0.0);
        let derived = accounting["derived_hot_balance"].as_f64().unwrap_or(0.0);
        assert!(
            (current - derived).abs() < 0.01,
            "accounting current={current} vs derived_hot={derived}"
        );

        drop(bot);
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    /// In maker mode the actor drops the automatic taker path — the signal is
    /// expressed through quotes instead. A forced trade is an operator asking
    /// for a taker entry now, and used to be dropped by the same gate, with
    /// the route reporting only "did not result in an opened trade".
    #[tokio::test]
    async fn a_forced_trade_is_not_dropped_by_the_maker_mode_gate() -> anyhow::Result<()> {
        let (bot, path, path_str) = integration_bot("forced_in_maker").await?;
        {
            let mut cfg = bot.control_plane.config.write().await;
            cfg.execution_mode = "maker".to_string();
        }

        // The automatic path in maker mode is still dropped.
        let mut automatic = buy_yes_analysis("mkt-forced-a", 0.50);
        automatic["forced"] = json!(false);
        let dropped = bot
            .execute_trade_serialized(&automatic, false, Some(1.0))
            .await;
        assert!(
            dropped.is_none(),
            "maker mode must still drop the automatic taker path"
        );

        // A force goes through.
        let forced = buy_yes_analysis("mkt-forced-b", 0.50);
        let opened = bot
            .execute_trade_serialized(&forced, false, Some(1.0))
            .await
            .expect("a forced trade in maker mode must open");
        assert_eq!(opened.direction, "BUY_YES");
        assert!(opened.is_open());

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn integration_multi_market_open_close_reconcile() -> anyhow::Result<()> {
        let (bot, path, path_str) = integration_bot("multi_market").await?;
        let initial = bot.balance().await;

        // Open trades on two different markets
        let a1 = buy_yes_analysis("mkt-multi-a", 0.50);
        let a2 = buy_yes_analysis("mkt-multi-b", 0.60);

        let t1 = bot
            .execute_trade_serialized(&a1, false, Some(1.0))
            .await
            .expect("trade 1 should open");
        let t2 = bot
            .execute_trade_serialized(&a2, false, Some(1.0))
            .await
            .expect("trade 2 should open");

        // Verify both are open
        let hot_open = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|t| t.is_open())
            .count();
        assert_eq!(hot_open, 2);

        // Close first at a profit, second at a loss
        let c1 = bot
            .close_trade_serialized(t1.id, Some(0.75))
            .await
            .expect("trade 1 close");
        assert!(c1.pnl.unwrap_or(0.0) > 0.0);

        let c2 = bot
            .close_trade_serialized(t2.id, Some(0.40))
            .await
            .expect("trade 2 close");
        assert!(c2.pnl.unwrap_or(0.0) < 0.0);

        // Final balance = initial + sum(pnl) - sum(fees)
        let final_bal = bot.balance().await;
        let total_pnl = c1.pnl.unwrap_or(0.0) + c2.pnl.unwrap_or(0.0);
        let total_fees = c1.fee_paid + c2.fee_paid;
        let expected = initial + total_pnl - total_fees;
        assert!(
            (final_bal - expected).abs() < 1e-9,
            "multi-market balance mismatch: got {final_bal}, expected {expected}"
        );

        // DB should have exactly 2 closed trades, 0 open
        let db_open = crate::db::load_open_trades(&bot.db).await?.len();
        assert_eq!(db_open, 0);

        // Integrity report: check balance/position consistency
        let report = crate::api::dashboard_integrity_report(&bot, false, 5).await;
        let mismatches = report
            .get("mismatches")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let balance_or_position_mismatches: Vec<_> = mismatches
            .iter()
            .filter(|m| {
                let check = m
                    .get("check")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                check != "slow_recent_trade_ids_vs_db" && check != "fast_last_trade_vs_db"
            })
            .collect();
        assert!(
            balance_or_position_mismatches.is_empty(),
            "unexpected integrity mismatches after multi-market cycle:\n{:?}",
            balance_or_position_mismatches
        );

        drop(bot);
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integration_rejection_persisted_to_db() -> anyhow::Result<()> {
        let (bot, path, path_str) = integration_bot("rejection_db").await?;

        // Create a trade with a price outside the allowed entry range (max_entry_price=0.9).
        // The guardrails should reject it and the rejection should be persisted.
        let analysis = json!({
            "signal": "BUY_YES",
            "confidence": 0.95,
            "raw_confidence": 0.95,
            "net_ev": 0.20,
            "forced": true,
            "reason": "rejection-test",
            "regime": "BULL",
            "p_up": 0.71,
            "btc_change_pct": 1.2,
            "market_data": {
                "market_id": "mkt-rej-price",
                "question": "test-reject",
                "price": 0.95,
                "end_date": "2099-01-01T00:00:00Z",
                "liquidity": 20000.0
            },
            "signals": {"orderbook_imbalance": 0.6}
        });
        let result = bot
            .execute_trade_serialized(&analysis, false, Some(1.0))
            .await;
        assert!(result.is_none(), "trade should be rejected by price guard");

        // Give fire-and-forget spawn a moment to persist
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Check that the rejection was recorded
        let rejection_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM trade_rejections WHERE market_id = 'mkt-rej-price'",
        )
        .fetch_one(&bot.db)
        .await?;
        assert!(
            rejection_count >= 1,
            "expected at least 1 trade rejection row, got {rejection_count}"
        );

        drop(bot);
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integration_duplicate_rejection_persisted_to_db() -> anyhow::Result<()> {
        let (bot, path, path_str) = integration_bot("rejection_dup_db").await?;

        let analysis = buy_yes_analysis("mkt-rej-dup", 0.55);
        let _opened = bot
            .execute_trade_serialized(&analysis, false, Some(1.0))
            .await
            .expect("first trade should open");

        let result = bot
            .execute_trade_serialized(&analysis, false, Some(1.0))
            .await;
        assert!(result.is_none(), "duplicate trade should be rejected");

        tokio::time::sleep(Duration::from_millis(50)).await;

        let rejection_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM trade_rejections WHERE market_id = 'mkt-rej-dup'",
        )
        .fetch_one(&bot.db)
        .await?;
        assert!(
            rejection_count >= 1,
            "expected at least 1 trade rejection row, got {rejection_count}"
        );

        drop(bot);
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }
}
