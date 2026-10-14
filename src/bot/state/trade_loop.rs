use chrono::{Timelike, Utc};
use serde_json::Value;
use tracing::info;

use crate::analysis::AnalysisResult as ParsedAnalysis;
use crate::bot::actor::ExecutionOrigin;
use crate::config::Config;
use crate::domain::Signal;

use super::TradingBot;

#[derive(Debug, Clone, Copy)]
struct ResolutionSniperInputs {
    time_remaining_minutes: f64,
    btc_price: f64,
    btc_start_price: f64,
    yes_price: f64,
}

impl TradingBot {
    pub(super) async fn process_trade_decisions(
        &self,
        analysis: &Value,
        cfg: &Config,
        origin: ExecutionOrigin,
    ) {
        // Every automatic strategy, including the forced expiry exception,
        // obeys the operator's automatic-entry setting.
        if !cfg.auto_trading || !self.entry_origin_allowed(origin).await {
            return;
        }
        self.maybe_execute_resolution_sniper(analysis, cfg, origin)
            .await;

        if !self.sleep_schedule_allows_trade().await {
            return;
        }

        if let Some((ml, deep)) = Self::ensemble_disagreement_pair(analysis) {
            info!(
                "Ensemble disagreement: ML p_up={:.2} vs Deep p_up={:.2} — skipping trade",
                ml, deep
            );
            return;
        }

        Self::log_latency_arb_hint(analysis);

        if self.hard_negative_blocks_trade(analysis).await {
            info!("Hard negative match: skipping trade (similar to past loss)");
            return;
        }

        let parsed = ParsedAnalysis::from_value(analysis);
        self.execute_trade_entries(analysis, cfg, &parsed, origin)
            .await;
    }

    async fn maybe_execute_resolution_sniper(
        &self,
        analysis: &Value,
        cfg: &Config,
        origin: ExecutionOrigin,
    ) {
        let Some(inputs) = Self::resolution_sniper_inputs(analysis) else {
            return;
        };

        let secs_remaining = inputs.time_remaining_minutes * 60.0;
        if let Some(decision) = self.strategy_services.resolution_sniper.should_snipe(
            secs_remaining,
            inputs.btc_price,
            inputs.btc_start_price,
            inputs.yes_price,
        ) {
            info!(
                "Resolution sniper fired: {} confidence={:.2} amount_pct={:.2} \
                 (BTC={:.0} vs strike={:.0}, YES={:.3}, {:.0}s left)",
                decision.direction,
                decision.confidence,
                decision.suggested_amount_pct,
                inputs.btc_price,
                inputs.btc_start_price,
                inputs.yes_price,
                secs_remaining,
            );

            let snipe_signal = if decision.direction == "BUY_YES" {
                Signal::BuyYes
            } else {
                Signal::BuyNo
            };
            let selected_price = if snipe_signal == Signal::BuyYes {
                inputs.yes_price
            } else {
                1.0 - inputs.yes_price
            };

            let snipe_analysis = ParsedAnalysis {
                signal: snipe_signal,
                confidence: decision.confidence,
                raw_confidence: Some(decision.confidence),
                net_ev: decision.confidence / selected_price
                    - 1.0
                    - crate::fusion::estimate_taker_fee(selected_price),
                p_up: Some(if snipe_signal == Signal::BuyYes {
                    decision.confidence
                } else {
                    1.0 - decision.confidence
                }),
                reason: format!(
                    "Resolution sniper: {} edge={:.2} BTC={:.0} strike={:.0}",
                    decision.direction,
                    decision.confidence,
                    inputs.btc_price,
                    inputs.btc_start_price,
                ),
                forced: true,
                market: ParsedAnalysis::from_value(analysis).market,
                ..ParsedAnalysis::default()
            };

            let snipe_amount = {
                let bal = self.balance().await;
                let amt = bal * decision.suggested_amount_pct;
                amt.clamp(cfg.min_stake, cfg.max_stake)
            };

            drop(
                self.execute_automatic_trade_serialized_strategy(
                    &snipe_analysis,
                    false,
                    Some(snipe_amount),
                    "resolution_sniper",
                    origin,
                )
                .await,
            );
        }
    }

    fn resolution_sniper_inputs(analysis: &Value) -> Option<ResolutionSniperInputs> {
        let time_remaining_minutes = analysis
            .pointer("/extra/time_remaining_minutes")
            .or_else(|| analysis.get("time_remaining_minutes"))
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::MAX);

        if !time_remaining_minutes.is_finite()
            || time_remaining_minutes <= 0.0
            || time_remaining_minutes >= 0.5
        {
            return None;
        }

        let btc_price = analysis
            .pointer("/extra/btc_price")
            .or_else(|| analysis.get("btc_price"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let btc_start_price = analysis
            .pointer("/extra/btc_start_price")
            .or_else(|| analysis.get("btc_start_price"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if !btc_price.is_finite()
            || !btc_start_price.is_finite()
            || btc_price <= 0.0
            || btc_start_price <= 0.0
        {
            return None;
        }

        let yes_price = analysis
            .pointer("/market_data/price")
            .and_then(|v| v.as_f64())?;
        if !yes_price.is_finite() || !(0.0..1.0).contains(&yes_price) || yes_price == 0.0 {
            return None;
        }

        Some(ResolutionSniperInputs {
            time_remaining_minutes,
            btc_price,
            btc_start_price,
            yes_price,
        })
    }

    async fn sleep_schedule_allows_trade(&self) -> bool {
        let hour = Utc::now().hour() as u8;
        let schedule = self.strategy_services.sleep_schedule.read().await;
        if !schedule.should_trade(hour) {
            info!(
                "Sleep schedule: hour {} UTC has poor P&L history — skipping trade",
                hour
            );
            return false;
        }
        true
    }

    fn ensemble_disagreement_pair(analysis: &Value) -> Option<(f64, f64)> {
        let phase = analysis
            .get("deep_model")
            .and_then(|d| d.get("ensemble_phase"))
            .and_then(|v| v.as_str())
            .unwrap_or("shadow");
        if phase == "shadow" {
            return None;
        }

        let ml = analysis
            .get("ml")
            .and_then(|m| m.get("p_up"))
            .and_then(|v| v.as_f64())?;
        let deep = analysis
            .get("deep_model")
            .and_then(|d| d.get("deep_p_up"))
            .and_then(|v| v.as_f64())?;

        let ml_bullish = ml > 0.6;
        let deep_bullish = deep > 0.6;
        let ml_bearish = ml < 0.4;
        let deep_bearish = deep < 0.4;

        if (ml_bullish && deep_bearish) || (ml_bearish && deep_bullish) {
            Some((ml, deep))
        } else {
            None
        }
    }

    fn latency_arb_hint(analysis: &Value) -> Option<(f64, f64)> {
        analysis
            .get("extra")
            .and_then(|e| e.get("latency_arb"))
            .and_then(|a| {
                let detected = a.get("arb_detected").and_then(|v| v.as_bool())?;
                let dir = a.get("expected_direction").and_then(|v| v.as_f64())?;
                let conf = a.get("confidence").and_then(|v| v.as_f64())?;
                if detected && conf > 0.3 {
                    Some((dir, conf))
                } else {
                    None
                }
            })
    }

    fn log_latency_arb_hint(analysis: &Value) {
        if let Some((dir, conf)) = Self::latency_arb_hint(analysis) {
            info!(
                "Latency arb: Binance→Polymarket lag detected (dir={:+.0} conf={:.2}) — advisory signal",
                dir, conf
            );
        }
    }

    async fn hard_negative_blocks_trade(&self, analysis: &Value) -> bool {
        let signals_map: Option<std::collections::HashMap<String, f64>> = analysis
            .get("signals")
            .and_then(|v| serde_json::from_value(v.clone()).ok());
        if let Some(ref breakdown) = signals_map {
            let features = crate::ml::extract_features(breakdown);
            let tracker = self.intelligence.hard_negative_tracker.read().await;
            tracker.should_skip(&features, 0.90)
        } else {
            false
        }
    }

    async fn execute_trade_entries(
        &self,
        _analysis: &Value,
        cfg: &Config,
        parsed: &ParsedAnalysis,
        origin: ExecutionOrigin,
    ) {
        if !cfg.auto_trading || parsed.signal == Signal::Hold {
            return;
        }

        // No stake override. Entry sizing is decided by the configured stake
        // policy and the risk limits below it, not scaled afterwards.
        drop(
            self.execute_automatic_trade_serialized_strategy(
                parsed,
                false,
                None,
                "conviction",
                origin,
            )
            .await,
        );

        if cfg.scalp_enabled && parsed.confidence >= cfg.scalp_min_confidence {
            let has_existing_conviction = {
                let trades = self.portfolio_state.trades.read().await;
                trades.iter().any(|t| {
                    t.is_open() && t.strategy != "scalp" && t.market_id == parsed.market.market_id
                })
            };
            if has_existing_conviction {
                let scalp_amount = {
                    let bal = self.balance().await;
                    let amt = bal * cfg.scalp_stake_pct;
                    amt.clamp(cfg.min_stake, cfg.max_stake)
                };
                drop(
                    self.execute_automatic_trade_serialized_strategy(
                        parsed,
                        false,
                        Some(scalp_amount),
                        "scalp",
                        origin,
                    )
                    .await,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::TradingBot;

    #[tokio::test]
    async fn disabled_automatic_trading_also_blocks_expiry_sniper() {
        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../../config.json")).unwrap();
        cfg.mode = "simulation".into();
        cfg.auto_trading = false;
        let pool = crate::db::open(":memory:").await.unwrap();
        let bot = TradingBot::new(cfg.clone(), pool).await.unwrap();
        let analysis = json!({
            "extra": {"time_remaining_minutes":0.1,"btc_price":101000.0,"btc_start_price":100000.0},
            "market_data":{"market_id":"sniper-stop","price":0.1,"end_date":"2099-01-01T00:00:00Z"}
        });
        assert!(bot
            .strategy_services
            .resolution_sniper
            .should_snipe(6.0, 101000.0, 100000.0, 0.1)
            .is_some());
        bot.process_trade_decisions(&analysis, &cfg, bot.automatic_entry_origin())
            .await;
        assert!(
            !bot.kill_switch_active(),
            "disabled entries must never reach an unavailable actor"
        );
        assert!(crate::db::load_all_trades(&bot.db)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn in_flight_automatic_decision_cannot_inherit_a_restarted_or_reenabled_epoch() {
        use std::future::Future;
        use std::sync::Arc;
        use std::task::Poll;

        for disable_instead_of_stop in [false, true] {
            let mut cfg: crate::config::Config =
                serde_json::from_str(include_str!("../../../config.json")).unwrap();
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
            cfg.kelly_sizing = false;
            cfg.stake_pct = 0.0;
            cfg.min_stake = 5.0;
            cfg.max_stake = 5.0;
            let bot = Arc::new(
                TradingBot::new(cfg.clone(), crate::db::open(":memory:").await.unwrap())
                    .await
                    .unwrap(),
            );
            let analysis = json!({
                "signal":"BUY_YES", "forced":false, "confidence":0.95,
                "raw_confidence":0.95, "p_up":0.95, "net_ev":0.8,
                "regime":"TREND_UP", "btc_change_pct":1.0,
                "signals":{"momentum":0.8},
                "market_data":{"market_id":"awaited-automatic-decision", "price":0.5,
                    "liquidity":100000.0,
                    "end_date":(chrono::Utc::now()+chrono::Duration::minutes(15)).to_rfc3339()}
            });
            *bot.analysis_state.last_analysis.write().await = analysis.clone();
            bot.start_loop();
            bot.mark_analysis_fresh().await;
            bot.start_execution_actor().await;

            // Poll the actual decision pipeline until its hard-negative read
            // blocks. There is no replacement evaluator or timing-only sleep.
            let gate = bot.intelligence.hard_negative_tracker.write().await;
            let old_origin = bot.automatic_entry_origin();
            let decision = bot.process_trade_decisions(&analysis, &cfg, old_origin);
            tokio::pin!(decision);
            std::future::poll_fn(|cx| {
                assert!(decision.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            if disable_instead_of_stop {
                {
                    let mut current = bot.control_plane.config.write().await;
                    bot.invalidate_automatic_entries();
                    current.auto_trading = false;
                }
                bot.control_plane.config.write().await.auto_trading = true;
            } else {
                bot.stop_loop();
                bot.start_loop();
            }
            drop(gate);
            decision.await;
            assert!(
                crate::db::load_open_trades(&bot.db)
                    .await
                    .unwrap()
                    .is_empty(),
                "an old decision inherited the new automatic-entry epoch"
            );
            assert_eq!(bot.balance().await, 100.0);
            assert!(!bot.kill_switch_active());

            // The same decision with a genuinely new cycle identity still
            // enters, proving that the negative path is not an unrelated gate.
            bot.process_trade_decisions(&analysis, &cfg, bot.automatic_entry_origin())
                .await;
            assert_eq!(
                crate::db::load_open_trades(&bot.db).await.unwrap().len(),
                1,
                "a fresh automatic decision must remain usable after restart/reenable"
            );
            assert!(bot.balance().await < 100.0);
            bot.stop_execution_actor().await.unwrap();
            bot.db.close().await;
        }
    }

    #[test]
    fn resolution_sniper_inputs_accepts_extra_fields() {
        let analysis = json!({
            "extra": {
                "time_remaining_minutes": 0.25,
                "btc_price": 85321.0,
                "btc_start_price": 85200.0
            },
            "market_data": {
                "price": 0.61
            }
        });

        let inputs = TradingBot::resolution_sniper_inputs(&analysis)
            .expect("extra payload should produce sniper inputs");
        assert!((inputs.time_remaining_minutes - 0.25).abs() < f64::EPSILON);
        assert!((inputs.btc_price - 85321.0).abs() < f64::EPSILON);
        assert!((inputs.btc_start_price - 85200.0).abs() < f64::EPSILON);
        assert!((inputs.yes_price - 0.61).abs() < f64::EPSILON);
    }

    #[test]
    fn resolution_sniper_inputs_require_last_half_minute_and_prices() {
        let late_but_missing_prices = json!({
            "time_remaining_minutes": 0.2,
            "market_data": {
                "price": 0.42
            }
        });
        assert!(TradingBot::resolution_sniper_inputs(&late_but_missing_prices).is_none());

        let too_early = json!({
            "extra": {
                "time_remaining_minutes": 0.75,
                "btc_price": 85321.0,
                "btc_start_price": 85200.0
            }
        });
        assert!(TradingBot::resolution_sniper_inputs(&too_early).is_none());
    }

    #[test]
    fn ensemble_disagreement_pair_detects_conflict() {
        let analysis = json!({
            "ml": {
                "p_up": 0.81
            },
            "deep_model": {
                "ensemble_phase": "active",
                "deep_p_up": 0.19
            }
        });

        let pair = TradingBot::ensemble_disagreement_pair(&analysis)
            .expect("active disagreement should be detected");
        assert!((pair.0 - 0.81).abs() < f64::EPSILON);
        assert!((pair.1 - 0.19).abs() < f64::EPSILON);
    }

    #[test]
    fn ensemble_disagreement_pair_ignores_shadow_phase() {
        let analysis = json!({
            "ml": {
                "p_up": 0.81
            },
            "deep_model": {
                "ensemble_phase": "shadow",
                "deep_p_up": 0.19
            }
        });

        assert!(TradingBot::ensemble_disagreement_pair(&analysis).is_none());
    }
}
