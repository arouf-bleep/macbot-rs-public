use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{Config, FusionCalibration};
use crate::db::{self, AnalysisSnapshot, Trade};
use crate::domain::BotMode;
use crate::metrics;

use super::TradingBot;

impl TradingBot {
    pub(super) async fn shadow_kill_reason(&self, cfg: &Config) -> Option<String> {
        if !Self::env_bool("MACBOT_SHADOW_KILL_ENABLED", false) {
            return None;
        }
        if !cfg.shadow_trading && cfg.mode_kind() != BotMode::Shadow {
            return None;
        }

        let closed_shadow = match db::load_closed_shadow_trades(&self.db).await {
            Ok(rows) => rows,
            Err(error) => {
                return Some(format!(
                    "shadow_kill: historical outcomes unavailable: {error}"
                ))
            }
        };
        let trade_count = closed_shadow.len();
        if trade_count == 0 {
            return None;
        }

        let min_trades = Self::env_u64("SHADOW_MIN_TRADES", 200) as usize;
        if trade_count < min_trades {
            return None;
        }

        let min_win_rate = Self::env_f64("SHADOW_MIN_WIN_RATE", 0.53);
        let min_expectancy = Self::env_f64("SHADOW_MIN_EXPECTANCY_USD", 0.003);
        let min_profit_factor = Self::env_f64("SHADOW_MIN_PROFIT_FACTOR", 1.03);
        let max_drawdown_pct = Self::env_f64("SHADOW_MAX_DRAWDOWN_PCT", 25.0);
        let min_recent_trades = Self::env_u64("SHADOW_MIN_RECENT_TRADES", 50) as usize;
        let min_recent_win_rate = Self::env_f64("SHADOW_MIN_RECENT_WIN_RATE", 0.50);

        let mut wins = 0usize;
        let mut total_pnl = 0.0;
        let mut gross_win = 0.0;
        let mut gross_loss_abs = 0.0;
        let mut running_pnl = 0.0;
        let mut peak_pnl = 0.0;
        let mut max_drawdown = 0.0;

        for trade in &closed_shadow {
            let pnl = trade.net_realized_pnl();
            total_pnl += pnl;
            if pnl > 0.0 {
                wins += 1;
                gross_win += pnl;
            } else if pnl < 0.0 {
                gross_loss_abs += -pnl;
            }

            running_pnl += pnl;
            if running_pnl > peak_pnl {
                peak_pnl = running_pnl;
            }
            let dd = peak_pnl - running_pnl;
            if dd > max_drawdown {
                max_drawdown = dd;
            }
        }

        let n = trade_count as f64;
        let win_rate = wins as f64 / n;
        let expectancy = total_pnl / n;
        let profit_factor = if gross_loss_abs > 0.0 {
            gross_win / gross_loss_abs
        } else if gross_win > 0.0 {
            999.0
        } else {
            0.0
        };
        let max_dd_pct = if cfg.initial_balance > 0.0 {
            max_drawdown / cfg.initial_balance * 100.0
        } else {
            0.0
        };

        if win_rate < min_win_rate {
            return Some(format!(
                "shadow_kill: win_rate {:.1}% < {:.1}% (trades={trade_count})",
                win_rate * 100.0,
                min_win_rate * 100.0
            ));
        }
        if expectancy < min_expectancy {
            return Some(format!(
                "shadow_kill: expectancy ${expectancy:.4}/trade < ${min_expectancy:.4}"
            ));
        }
        if profit_factor < min_profit_factor {
            return Some(format!(
                "shadow_kill: profit_factor {profit_factor:.3} < {min_profit_factor:.3}"
            ));
        }
        if max_dd_pct > max_drawdown_pct {
            return Some(format!(
                "shadow_kill: max_drawdown {:.1}% > {:.1}%",
                max_dd_pct, max_drawdown_pct
            ));
        }

        if min_recent_trades > 0 {
            let recent: &[Trade] = if closed_shadow.len() > min_recent_trades {
                &closed_shadow[closed_shadow.len() - min_recent_trades..]
            } else {
                &closed_shadow
            };
            if recent.len() >= min_recent_trades {
                let recent_wr = recent
                    .iter()
                    .filter(|trade| trade.net_realized_pnl() > 0.0)
                    .count() as f64
                    / recent.len() as f64;
                if recent_wr < min_recent_win_rate {
                    return Some(format!(
                        "shadow_kill: recent_win_rate {:.1}% < {:.1}% (recent_trades={})",
                        recent_wr * 100.0,
                        min_recent_win_rate * 100.0,
                        recent.len()
                    ));
                }
            }
        }

        None
    }

    async fn reload_signal_weights_if_needed(&self) {
        let path = {
            let cfg = self.control_plane.config.read().await;
            cfg.signal_weights_path.trim().to_string()
        };
        if path.is_empty() {
            *self.analysis_state.signal_weights_file.write().await = None;
            return;
        }

        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => return,
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(_) => return,
        };

        let mut last_seen = self.analysis_state.signal_weights_file.write().await;
        if last_seen
            .as_ref()
            .is_some_and(|(previous_path, previous_time)| {
                previous_path == &path && *previous_time == modified
            })
        {
            return;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) => {
                warn!("signal weights reload: could not read {path}: {err}");
                return;
            }
        };
        let payload: Value = match serde_json::from_str(&text) {
            Ok(payload) => payload,
            Err(err) => {
                warn!("signal weights reload: invalid JSON in {path}: {err}");
                return;
            }
        };

        let mut current_cfg = self.control_plane.config.write().await;
        // A settings update may select another file while this reload waits
        // for the write lock. Never publish the previously selected payload.
        if current_cfg.signal_weights_path.trim() != path {
            return;
        }
        let mut cfg = current_cfg.clone();
        let mut changed = false;

        if let Some(weights_obj) = payload.get("signal_weights").and_then(Value::as_object) {
            let weights: HashMap<String, f64> = weights_obj
                .iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect();
            if !weights.is_empty() {
                cfg.signal_weights.apply_map(&weights);
                changed = true;
            }
        } else if let Some(payload_obj) = payload.as_object() {
            let weights: HashMap<String, f64> = payload_obj
                .iter()
                .filter_map(|(k, v)| v.as_f64().map(|f| (k.clone(), f)))
                .collect();
            if !weights.is_empty() {
                cfg.signal_weights.apply_map(&weights);
                changed = true;
            }
        }

        if let Some(cal) = payload.get("fusion_calibration") {
            let mut next = cfg.fusion_calibration;
            if let Some(v) = cal.get("logit_scale").and_then(Value::as_f64) {
                next.logit_scale = v;
            }
            if let Some(v) = cal.get("min_score").and_then(Value::as_f64) {
                next.min_score = v;
            }
            if let Some(v) = cal.get("fee_skip_band").and_then(Value::as_f64) {
                next.fee_skip_band = v;
            }
            if let Some(v) = cal.get("min_net_ev").and_then(Value::as_f64) {
                next.min_net_ev = v;
            }
            if let Some(v) = cal.get("min_weighted_abs").and_then(Value::as_f64) {
                next.min_weighted_abs = v;
            }
            if next.logit_scale != cfg.fusion_calibration.logit_scale
                || next.min_score != cfg.fusion_calibration.min_score
                || next.fee_skip_band != cfg.fusion_calibration.fee_skip_band
                || next.min_net_ev != cfg.fusion_calibration.min_net_ev
                || next.min_weighted_abs != cfg.fusion_calibration.min_weighted_abs
            {
                cfg.fusion_calibration = FusionCalibration {
                    logit_scale: next.logit_scale,
                    min_score: next.min_score,
                    fee_skip_band: next.fee_skip_band,
                    min_net_ev: next.min_net_ev,
                    min_weighted_abs: next.min_weighted_abs,
                };
                changed = true;
            }
        }

        if changed {
            if let Err(error) = cfg.validate() {
                warn!("signal weights reload rejected invalid settings from {path}: {error}");
                return;
            }
            *current_cfg = cfg;
            info!("reloaded signal weights from {path}");
        }
        *last_seen = Some((path, modified));
    }

    pub async fn run_once(&self) -> Result<()> {
        // This identity belongs to the whole cycle, including analysis and
        // decision awaits. A later restart cannot revive pre-Stop work.
        let entry_origin = self.automatic_entry_origin();
        metrics::inc_analysis_cycle();
        self.enforce_runtime_slos().await;
        self.reload_signal_weights_if_needed().await;

        let analysis = self.analyze_market().await;
        {
            let mut last = self.analysis_state.last_analysis.write().await;
            *last = analysis.clone();
        }
        self.mark_analysis_fresh().await;

        let cfg = self.control_plane.config.read().await.clone();
        self.persist_analysis_snapshot(&analysis, &cfg).await;

        if let Some(reason) = self.shadow_kill_reason(&cfg).await {
            if !self.kill_switch_active() {
                metrics::inc_shadow_kill_switch_trip();
                warn!("Shadow kill switch triggered: {reason}");
                self.halt_trading("shadow_kill_criteria", reason.clone())
                    .await;
                crate::notifications::notify_kill_switch(&reason).await;
            }
        }

        self.check_tp_sl(&cfg, &analysis).await;

        if self.kill_switch_active() {
            metrics::inc_trade_entry_block_kill_switch();
            return Ok(());
        }
        if self.is_drawdown_halted().await {
            metrics::inc_trade_entry_block_drawdown();
            return Ok(());
        }
        if Self::env_bool("MACBOT_FREEZE_ALPHA", false) {
            metrics::inc_trade_entry_block_alpha_freeze();
            return Ok(());
        }
        if self.degraded_no_trade_active() {
            metrics::inc_trade_entry_block_degraded();
            return Ok(());
        }

        let mut pause_guard = self
            .trade_lifecycle
            .circuit_breaker_pause_until
            .write()
            .await;
        if let Some(until) = *pause_guard {
            if Utc::now() < until {
                metrics::inc_trade_entry_block_circuit_breaker();
                return Ok(());
            }
            info!("Circuit breaker cooldown expired — resuming new entries");
            *pause_guard = None;
        }
        drop(pause_guard);

        if cfg.mode_kind() == BotMode::Live && self.l2_feed_stale() {
            metrics::inc_trade_entry_block_l2_stale();
            return Ok(());
        }

        self.process_trade_decisions(&analysis, &cfg, entry_origin)
            .await;
        Ok(())
    }

    async fn persist_analysis_snapshot(&self, analysis: &Value, cfg: &Config) {
        if !cfg.analysis_snapshot_enabled {
            return;
        }

        let market = analysis.get("market_data").unwrap_or(&Value::Null);
        let market_id = market
            .get("market_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if market_id.is_empty() {
            return;
        }

        let signals_json = analysis
            .get("signals")
            .and_then(|signals| serde_json::to_string(signals).ok());

        let snapshot = AnalysisSnapshot {
            timestamp: Utc::now().to_rfc3339(),
            run_id: self.control_plane.run_id.clone(),
            market_id,
            question: market
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            signal: analysis
                .get("signal")
                .and_then(Value::as_str)
                .unwrap_or("HOLD")
                .to_string(),
            confidence: analysis
                .get("confidence")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            net_ev: analysis
                .get("net_ev")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            p_up: analysis.get("p_up").and_then(Value::as_f64),
            price: market.get("price").and_then(Value::as_f64).unwrap_or(0.0),
            liquidity: market
                .get("liquidity")
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
            regime: analysis
                .get("regime")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            phase: analysis
                .get("phase")
                .and_then(Value::as_str)
                .or_else(|| analysis.pointer("/extra/phase").and_then(Value::as_str))
                .unwrap_or("")
                .to_string(),
            time_remaining_minutes: analysis
                .get("time_remaining_minutes")
                .and_then(Value::as_f64)
                .or_else(|| {
                    analysis
                        .pointer("/extra/time_remaining_minutes")
                        .and_then(Value::as_f64)
                })
                .unwrap_or(0.0),
            btc_change_pct: analysis.get("btc_change_pct").and_then(Value::as_f64),
            reason: analysis
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            end_date: market
                .get("end_date")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            signals_json,
            calibration_version: Some(cfg.calibration_version()),
            source: market
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| {
                    analysis
                        .get("data_source")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
        };

        if let Err(err) = db::insert_analysis_snapshot(&self.db, &snapshot).await {
            metrics::inc_analysis_snapshot_persist_fail();
            warn!(
                "analysis snapshot persist failed for market {}: {}",
                snapshot.market_id, err
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Trade;

    struct EnvRestore {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            if let Some(old) = self.old.as_deref() {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn shadow_trade(id: i64, pnl: f64) -> Trade {
        Trade {
            id,
            timestamp: "2026-03-27T00:00:00Z".to_string(),
            market_id: format!("shadow-{id:04}"),
            question: "shadow-kill-test".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.5,
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.0,
            reason: "shadow-test".to_string(),
            confidence: 0.6,
            status: "CLOSED".to_string(),
            mode: "shadow".to_string(),
            source: "shadow".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: Some(0.5),
            exit_timestamp: Some("2026-03-27T01:00:00Z".to_string()),
            pnl: Some(pnl),
            exit_type: Some("resolution".to_string()),
            was_correct: None,
            forced: Some(0),
            partial_closed: Some(0),
            raw_confidence: Some(0.6),
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: Some("test-cal-v1".to_string()),
            run_id: Some("shadow-kill-test".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    async fn new_shadow_bot() -> anyhow::Result<(TradingBot, crate::config::Config, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_shadow_cycle_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.shadow_trading = true;

        let bot = TradingBot::new(cfg.clone(), pool).await?;
        Ok((bot, cfg, path_str))
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn shadow_kill_reason_ignores_small_samples() -> anyhow::Result<()> {
        let _guard = crate::env_util::test_env_lock();
        let _restore = EnvRestore::set("MACBOT_SHADOW_KILL_ENABLED", "1");

        let (bot, cfg, path_str) = new_shadow_bot().await?;
        for i in 0..199_i64 {
            let pnl = if i % 5 < 3 { 0.03 } else { -0.015 };
            let _ = crate::db::insert_trade(&bot.db, &shadow_trade(i + 1, pnl)).await?;
        }

        assert_eq!(bot.shadow_kill_reason(&cfg).await, None);

        drop(bot);
        let _ = std::fs::remove_file(&path_str);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn shadow_kill_reason_trips_on_low_win_rate() -> anyhow::Result<()> {
        let _guard = crate::env_util::test_env_lock();
        let _restore = EnvRestore::set("MACBOT_SHADOW_KILL_ENABLED", "1");

        let (bot, cfg, path_str) = new_shadow_bot().await?;
        for i in 0..200_i64 {
            let pnl = if i < 90 { 0.02 } else { -0.02 };
            let _ = crate::db::insert_trade(&bot.db, &shadow_trade(i + 1, pnl)).await?;
        }

        let reason = bot.shadow_kill_reason(&cfg).await;
        assert!(reason.as_deref().is_some_and(|r| r.contains("win_rate")));

        drop(bot);
        let _ = std::fs::remove_file(&path_str);
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

        let (bot, cfg, path_str) = new_shadow_bot().await?;
        for i in 0..200_i64 {
            let pnl = if i % 5 < 3 { 0.03 } else { -0.015 };
            let _ = crate::db::insert_trade(&bot.db, &shadow_trade(i + 1, pnl)).await?;
        }

        assert_eq!(bot.shadow_kill_reason(&cfg).await, None);

        drop(bot);
        let _ = std::fs::remove_file(&path_str);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn reload_signal_weights_applies_live_updates() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let weights_path = std::env::temp_dir().join(format!("macbot_weights_reload_{uniq}.json"));
        let db_path = std::env::temp_dir().join(format!("macbot_weights_reload_{uniq}.sqlite"));

        let payload = serde_json::json!({
            "signal_weights": {
                "orderbook_imbalance": 0.3333,
                "tick_velocity": 0.2222
            },
            "fusion_calibration": {
                "logit_scale": 3.10,
                "min_score": 41.0,
                "fee_skip_band": 0.07,
                "min_net_ev": 0.02,
                "min_weighted_abs": 0.27
            }
        });
        std::fs::write(&weights_path, serde_json::to_string_pretty(&payload)?)?;

        let db_path_str = db_path.to_string_lossy().to_string();
        let pool = crate::db::open(&db_path_str).await?;
        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.signal_weights_path = String::new();

        let bot = TradingBot::new(cfg, pool).await?;

        {
            let mut guard = bot.control_plane.config.write().await;
            guard.signal_weights_path = weights_path.to_string_lossy().to_string();
            guard.signal_weights.orderbook_imbalance = 0.1111;
            guard.signal_weights.tick_velocity = 0.1111;
            guard.fusion_calibration.logit_scale = 1.0;
            guard.fusion_calibration.min_score = 10.0;
            guard.fusion_calibration.fee_skip_band = 0.01;
            guard.fusion_calibration.min_net_ev = 0.001;
            guard.fusion_calibration.min_weighted_abs = 0.01;
        }

        bot.reload_signal_weights_if_needed().await;

        let cfg = bot.control_plane.config.read().await.clone();
        assert!((cfg.signal_weights.orderbook_imbalance - 0.3333).abs() < 1e-9);
        assert!((cfg.signal_weights.tick_velocity - 0.2222).abs() < 1e-9);
        assert!((cfg.fusion_calibration.logit_scale - 3.10).abs() < 1e-9);
        assert!((cfg.fusion_calibration.min_score - 41.0).abs() < 1e-9);
        assert!((cfg.fusion_calibration.fee_skip_band - 0.07).abs() < 1e-9);
        assert!((cfg.fusion_calibration.min_net_ev - 0.02).abs() < 1e-9);
        assert!((cfg.fusion_calibration.min_weighted_abs - 0.27).abs() < 1e-9);

        drop(bot);
        let _ = std::fs::remove_file(&weights_path);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{}-shm", db_path_str));
        let _ = std::fs::remove_file(format!("{}-wal", db_path_str));
        Ok(())
    }
}

#[cfg(test)]
mod adversarial_weight_path_tests {
    use super::*;
    use std::fs::{File, FileTimes};
    use std::time::{Duration, SystemTime};

    #[tokio::test]
    async fn weights_reload_applies_older_revision_and_disabled_path_clears_identity(
    ) -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "macbot-weight-revision-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root)?;
        let path = root.join("weights.json");
        let now = SystemTime::now();
        std::fs::write(&path, r#"{"signal_weights":{"orderbook_imbalance":0.3}}"#)?;
        File::options()
            .write(true)
            .open(&path)?
            .set_times(FileTimes::new().set_modified(now))?;
        let mut config: Config = serde_json::from_str(include_str!("../../../config.json"))?;
        config.mode = "simulation".into();
        config.signal_weights_path = path.to_string_lossy().into_owned();
        let bot = TradingBot::new(config, db::open(":memory:").await?).await?;
        bot.reload_signal_weights_if_needed().await;
        std::fs::write(&path, r#"{"signal_weights":{"orderbook_imbalance":0.7}}"#)?;
        File::options()
            .write(true)
            .open(&path)?
            .set_times(FileTimes::new().set_modified(now - Duration::from_secs(120)))?;
        bot.reload_signal_weights_if_needed().await;
        assert_eq!(
            bot.control_plane
                .config
                .read()
                .await
                .signal_weights
                .orderbook_imbalance,
            0.7
        );
        bot.control_plane
            .config
            .write()
            .await
            .signal_weights_path
            .clear();
        bot.reload_signal_weights_if_needed().await;
        assert!(bot
            .analysis_state
            .signal_weights_file
            .read()
            .await
            .is_none());
        // Re-enabling even the same file must reread it after an intentional
        // disable, rather than treating the old cache as selected identity.
        {
            let mut cfg = bot.control_plane.config.write().await;
            cfg.signal_weights_path = path.to_string_lossy().into_owned();
            cfg.signal_weights.orderbook_imbalance = 0.2;
        }
        bot.reload_signal_weights_if_needed().await;
        assert_eq!(
            bot.control_plane
                .config
                .read()
                .await
                .signal_weights
                .orderbook_imbalance,
            0.7
        );
        bot.db.close().await;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn weights_reload_does_not_publish_a_path_replaced_while_waiting() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!(
            "macbot-weight-selection-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root)?;
        let first = root.join("first.json");
        let second = root.join("second.json");
        std::fs::write(&first, r#"{"signal_weights":{"orderbook_imbalance":0.3}}"#)?;
        std::fs::write(&second, r#"{"signal_weights":{"orderbook_imbalance":0.7}}"#)?;
        let mut config: Config = serde_json::from_str(include_str!("../../../config.json"))?;
        config.mode = "simulation".into();
        config.signal_weights_path = first.to_string_lossy().into_owned();
        config.signal_weights.orderbook_imbalance = 0.2;
        let bot = TradingBot::new(config, db::open(":memory:").await?).await?;
        let cache_lease = bot.analysis_state.signal_weights_file.write().await;
        let mut old_selection = Box::pin(bot.reload_signal_weights_if_needed());
        // The future reads the first path, then necessarily waits for this
        // held cache lock. A settings write selects the second before release.
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut old_selection)
                .await
                .is_err()
        );
        bot.control_plane.config.write().await.signal_weights_path =
            second.to_string_lossy().into_owned();
        drop(cache_lease);
        old_selection.await;
        assert_eq!(
            bot.control_plane
                .config
                .read()
                .await
                .signal_weights
                .orderbook_imbalance,
            0.2,
            "superseded first payload must not be published"
        );
        bot.reload_signal_weights_if_needed().await;
        assert_eq!(
            bot.control_plane
                .config
                .read()
                .await
                .signal_weights
                .orderbook_imbalance,
            0.7
        );
        bot.db.close().await;
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn adversarial_switch_to_older_weights_file_applies_selected_file() -> anyhow::Result<()>
    {
        let root = std::env::temp_dir().join(format!(
            "macbot-adversarial-weights-{:032x}",
            rand::random::<u128>()
        ));
        std::fs::create_dir(&root)?;
        let first = root.join("recent.json");
        let second = root.join("older.json");
        std::fs::write(&first, r#"{"signal_weights":{"orderbook_imbalance":0.3}}"#)?;
        std::fs::write(&second, r#"{"signal_weights":{"orderbook_imbalance":0.7}}"#)?;
        let now = SystemTime::now();
        File::options()
            .write(true)
            .open(&first)?
            .set_times(FileTimes::new().set_modified(now))?;
        File::options()
            .write(true)
            .open(&second)?
            .set_times(FileTimes::new().set_modified(now - Duration::from_secs(120)))?;
        let mut config: Config = serde_json::from_str(include_str!("../../../config.json"))?;
        config.mode = "simulation".into();
        config.signal_weights_path = first.to_string_lossy().into_owned();
        let bot = TradingBot::new(config, db::open(":memory:").await?).await?;
        bot.reload_signal_weights_if_needed().await;
        assert_eq!(
            bot.control_plane
                .config
                .read()
                .await
                .signal_weights
                .orderbook_imbalance,
            0.3,
            "positive control must load the first file"
        );
        // This is an ordinary API-editable path. File age does not establish
        // that content from a completely different path has been loaded.
        bot.control_plane.config.write().await.signal_weights_path =
            second.to_string_lossy().into_owned();
        bot.reload_signal_weights_if_needed().await;
        let actual = bot
            .control_plane
            .config
            .read()
            .await
            .signal_weights
            .orderbook_imbalance;
        bot.db.close().await;
        std::fs::remove_dir_all(&root)?;
        assert_eq!(
            actual, 0.7,
            "selected older weights file was ignored; previous file's 0.3 remained active"
        );
        Ok(())
    }
}
