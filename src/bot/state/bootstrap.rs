use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use sqlx::SqlitePool;
use tracing::{info, warn};
use zeroize::Zeroizing;

use super::super::intelligence::BotIntelligence;
use super::analysis_state::AnalysisState;
use super::control_plane::ControlPlaneState;
use super::execution_coordination::ExecutionState;
use super::market_state::MarketState;
use super::portfolio_state::PortfolioState;
use super::runtime_state::RuntimeState;
use super::signal_trackers::SignalTrackers;
use super::strategy_services::StrategyServices;
use super::trade_lifecycle::TradeLifecycleState;
use super::TradingBot;
use crate::clob::{ClobAuth, ClobClient};
use crate::config::Config;
use crate::db;
use crate::domain::{BotMode, TradeStatus};

impl TradingBot {
    fn build_run_id() -> String {
        if let Ok(raw) = std::env::var("MACBOT_RUN_ID") {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let mut s: String = trimmed
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                    .take(120)
                    .collect();
                if !s.is_empty() {
                    return s;
                }
                s = trimmed.chars().take(120).collect();
                if !s.is_empty() {
                    return s;
                }
            }
        }
        format!(
            "run-{}-{}",
            Utc::now().format("%Y%m%dT%H%M%SZ"),
            std::process::id()
        )
    }

    async fn repair_open_live_trades_from_fill_ledger(
        pool: &SqlitePool,
        open_trades: &mut [crate::db::Trade],
    ) -> usize {
        let mut repaired = 0usize;

        for trade in open_trades.iter_mut().filter(|t| {
            t.is_open()
                && t.is_live()
                && t.order_id.as_deref().is_some_and(|v| !v.trim().is_empty())
        }) {
            let managed: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM live_orders WHERE trade_id=?")
                    .bind(trade.id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(1);
            if managed > 0 {
                continue;
            }
            let Ok(summary) = db::trade_fill_summary(pool, trade.id).await else {
                continue;
            };
            if summary.entry_filled_size <= 1e-9 {
                continue;
            }

            let mut changed = false;
            if (trade.shares - summary.entry_filled_size).abs() > 1e-9 {
                trade.shares = summary.entry_filled_size;
                changed = true;
            }
            if let Some(vwap) = summary.entry_vwap.filter(|v| *v > 0.0) {
                if (trade.entry_price - vwap).abs() > 1e-9 {
                    trade.entry_price = vwap;
                    changed = true;
                }
            }
            if trade.entry_order_inactive(
                summary.entry_filled_size,
                summary.entry_vwap.unwrap_or(trade.entry_price),
                "",
            ) && trade.order_id.is_some()
            {
                trade.order_id = None;
                changed = true;
            }

            if !changed {
                continue;
            }

            match db::update_trade(pool, trade).await {
                Ok(()) => {
                    repaired += 1;
                    let details = format!(
                        "trade_id={} replayed_from_fill_ledger entry_price={:.4} shares={:.6} active_order={}",
                        trade.id,
                        trade.entry_price,
                        trade.shares,
                        trade.order_id.as_deref().unwrap_or("none")
                    );
                    if let Err(e) = db::append_order_outbox(
                        pool,
                        "restart_fill_replay",
                        &trade.market_id,
                        trade.order_id.as_deref(),
                        trade.token_id.as_deref(),
                        "repair_trade_from_fill_ledger",
                        "ok",
                        &details,
                    )
                    .await
                    {
                        warn!(
                            "Failed writing restart fill replay audit for trade {}: {e}",
                            trade.id
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed replaying fill ledger into open live trade {}: {e}",
                        trade.id
                    );
                }
            }
        }

        repaired
    }

    /// Construct a new TradingBot.  Loads all trades from DB, initialises
    /// the CLOB client when mode == "live", and computes the starting balance.
    pub async fn new(config: Config, pool: SqlitePool) -> Result<Self> {
        // Load OPEN trades and all unresolved exits into hot memory. CLOSED rows remain
        // in SQLite and are queried on-demand for analytics/risk reporting.
        let mut open_trades = db::load_open_trades(&pool).await?;
        let unresolved_closes = db::load_trades_by_status(&pool, TradeStatus::Closing).await?;
        let unresolved_ids: Vec<_> = unresolved_closes.iter().map(|t| t.id).collect();
        let pending_live = db::live::pending_orders(&pool).await?;
        let pending_maker = db::maker::pending_orders(&pool).await?;
        let replayed =
            Self::repair_open_live_trades_from_fill_ledger(&pool, &mut open_trades).await;
        if replayed > 0 {
            info!(
                "Restart fill replay: repaired {} open live trade(s) from trade_fills",
                replayed
            );
        }
        open_trades.extend(unresolved_closes);

        // Balance = initial + closed non-live realised delta - open non-live locked collateral.
        let closed_non_live_delta = Self::closed_non_live_balance_delta(&pool).await?;
        let balance = Self::compute_balance_from_trades(
            &open_trades,
            config.initial_balance + closed_non_live_delta,
        );

        // CLOB client — only in live mode
        let clob: Option<Arc<ClobClient>> = if config.mode_kind() == BotMode::Live {
            match Self::build_clob_client(&config) {
                Ok(c) => {
                    info!("CLOB client initialised");
                    Some(Arc::new(c))
                }
                Err(e) => {
                    warn!("CLOB client init failed ({e}), running without live trading");
                    None
                }
            }
        } else {
            None
        };

        let bot = Self {
            control_plane: Arc::new(ControlPlaneState::new(Self::build_run_id(), config)),
            portfolio_state: Arc::new(PortfolioState::new(open_trades, balance)),
            db: pool.clone(),
            clob,
            runtime_state: Arc::new(RuntimeState::default()),
            signal_trackers: Arc::new(SignalTrackers::default()),
            analysis_state: Arc::new(AnalysisState::default()),
            market_state: Arc::new(MarketState::default()),
            execution_state: Arc::new(ExecutionState::default()),
            intelligence: Arc::new(BotIntelligence::new(pool.clone())),
            trade_lifecycle: Arc::new(TradeLifecycleState::default()),
            strategy_services: Arc::new(StrategyServices::new(&pool).await),
        };
        if !pending_live.is_empty() || !pending_maker.is_empty() {
            bot.set_degraded_no_trade(true);
            bot.halt_trading(
                "startup_live_settlement_pending",
                "durable live orders require confirmed settlement recovery before trading",
            )
            .await;
        }
        if !unresolved_ids.is_empty() {
            bot.trade_lifecycle
                .closing_in_progress
                .write()
                .await
                .extend(unresolved_ids.iter().copied());
            bot.set_degraded_no_trade(true);
            bot.halt_trading(
                "startup_unresolved_exit",
                format!("Persisted exits require reconciliation: trade_ids={unresolved_ids:?}"),
            )
            .await;
        }

        // Backfill ML model from historical resolved trades
        bot.ml_backfill().await;
        // Same reasoning: rebuild from persisted trades so the calibration
        // figures mean something on a freshly started process.
        bot.edge_tracker_backfill().await;

        Ok(bot)
    }

    fn build_clob_client(config: &Config) -> Result<ClobClient> {
        let private_key = Zeroizing::new(
            std::env::var("PRIVATE_KEY").map_err(|_| anyhow!("PRIVATE_KEY env var not set"))?,
        );
        let api_key = Zeroizing::new(
            std::env::var("POLYMARKET_API_KEY")
                .map_err(|_| anyhow!("POLYMARKET_API_KEY env var not set"))?,
        );
        let api_secret = Zeroizing::new(
            std::env::var("POLYMARKET_SECRET")
                .map_err(|_| anyhow!("POLYMARKET_SECRET env var not set"))?,
        );
        let passphrase = Zeroizing::new(
            std::env::var("POLYMARKET_PASSPHRASE")
                .map_err(|_| anyhow!("POLYMARKET_PASSPHRASE env var not set"))?,
        );

        let signature_type = std::env::var("POLYMARKET_SIGNATURE_TYPE")
            .unwrap_or_else(|_| "0".into())
            .parse::<u8>()
            .map_err(|_| anyhow!("POLYMARKET_SIGNATURE_TYPE must be 0, 1, 2 or 3"))?;
        let funder = std::env::var("POLYMARKET_FUNDER")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let auth = if signature_type == 0 && funder.is_none() {
            ClobAuth::new(&private_key, &api_key, &api_secret, &passphrase)?
        } else {
            ClobAuth::new_wallet(
                &private_key,
                &api_key,
                &api_secret,
                &passphrase,
                signature_type,
                funder.as_deref(),
            )?
        };
        let client = ClobClient::new(auth);
        if config.private_rpc_url.trim().is_empty() {
            Ok(client)
        } else {
            client.with_settlement_rpc(&config.private_rpc_url)
        }
    }
}
