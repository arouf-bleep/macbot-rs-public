use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Duration, Timelike, Utc};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock};
use tracing::{info, warn};

use super::state::TradingBot;

pub struct BotIntelligence {
    pub ml_state: Arc<RwLock<crate::ml::MlState>>,
    pub data_logger: Arc<crate::ml::data_logger::DataLogger>,
    pub liq_tracker: Arc<Mutex<crate::signals::LiquidationTracker>>,
    pub input_builder: Arc<Mutex<crate::ml::input_builder::InputBuilder>>,
    pub deep_model: crate::ml::deep_model::SharedDeepModel,
    pub ensemble_state: Arc<RwLock<crate::ml::ensemble::EnsembleState>>,
    pub hard_negative_tracker: Arc<RwLock<crate::ml::hard_negatives::HardNegativeTracker>>,
    pub edge_tracker: Arc<Mutex<crate::ml::edge_tracker::EdgeTracker>>,
}

impl BotIntelligence {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            ml_state: Arc::new(RwLock::new(crate::ml::MlState::load_or_new())),
            data_logger: Arc::new(crate::ml::data_logger::DataLogger::new(pool)),
            liq_tracker: Arc::new(Mutex::new(crate::signals::LiquidationTracker::new())),
            input_builder: Arc::new(Mutex::new(crate::ml::input_builder::InputBuilder::new())),
            deep_model: crate::ml::deep_model::new_shared(),
            ensemble_state: Arc::new(RwLock::new(
                crate::ml::ensemble::EnsembleState::load_or_new(),
            )),
            hard_negative_tracker: Arc::new(RwLock::new(
                crate::ml::hard_negatives::HardNegativeTracker::new(),
            )),
            edge_tracker: Arc::new(Mutex::new(crate::ml::edge_tracker::EdgeTracker::new(100))),
        }
    }
}

/// Stable lot identity excludes mutable exit/accounting fields. Repeated
/// resolution callbacks and boot replay therefore consume the same outcome once.
fn resolution_identity(trade: &crate::db::Trade) -> String {
    use sha2::{Digest, Sha256};
    let key = serde_json::json!([
        trade.id,
        trade.timestamp,
        trade.run_id,
        trade.market_id,
        trade.direction
    ]);
    format!("{:x}", Sha256::digest(key.to_string().as_bytes()))
}

fn resolution_observation(trade: &crate::db::Trade) -> Option<(HashMap<String, f64>, f64)> {
    let label = match (trade.was_correct, trade.direction.as_str()) {
        (Some(1), d) if d.contains("YES") || d.contains("Yes") => 1.0,
        (Some(1), _) => 0.0,
        (Some(0), d) if d.contains("YES") || d.contains("Yes") => 0.0,
        (Some(0), _) => 1.0,
        _ => return None,
    };
    let breakdown = serde_json::from_str(trade.signals_json.as_ref()?).ok()?;
    Some((breakdown, label))
}

impl TradingBot {
    /// Replay all resolved trades through the ML model so it starts with
    /// learned weights rather than zeros after a restart.
    /// Rebuild the edge tracker from closed trades.
    ///
    /// The tracker lives in memory with a hundred-slot window and is never
    /// persisted, so every restart wiped it. On a bot that restarts often it
    /// therefore reported zero observations forever, and the one instrument
    /// that compares predicted edge against realised edge had never once been
    /// read. The database already holds everything needed, so it is rebuilt at
    /// boot rather than waiting for a hundred fresh trades.
    pub(crate) async fn edge_tracker_backfill(&self) {
        let rows = sqlx::query_as::<_, (f64, Option<f64>, f64)>(
            "SELECT expected_value, CASE WHEN accounting_version=1 THEN pnl ELSE pnl-fee_paid END, COALESCE(entry_cost, amount) FROM trades \
             WHERE status = 'CLOSED' AND pnl IS NOT NULL \
               AND COALESCE(entry_cost, amount) > 0 \
             ORDER BY exit_timestamp ASC",
        )
        .fetch_all(&self.db)
        .await
        .unwrap_or_default();

        if rows.is_empty() {
            return;
        }

        let mut tracker = self.intelligence.edge_tracker.lock().await;
        // Replaying into a tracker that already holds observations would count
        // the same trades twice and quietly skew every calibration figure
        // derived from it. Boot is the only place this should fill.
        if !tracker.is_empty() {
            return;
        }
        for (predicted_edge, pnl, amount) in &rows {
            // Same normalisation the live path uses, so a replayed observation
            // is indistinguishable from one recorded as it happened.
            tracker.record(*predicted_edge, pnl.unwrap_or(0.0), *amount);
        }

        tracing::info!(
            "Edge tracker backfill: replayed {} trades, observations={} predicted={:?} actual={:?} ratio={:?}",
            rows.len(),
            tracker.len(),
            tracker.mean_predicted_edge(),
            tracker.mean_actual_edge(),
            tracker.calibration_ratio(),
        );
    }

    pub(crate) async fn ml_backfill(&self) {
        let rows = match sqlx::query_as::<_, crate::db::Trade>(
            "SELECT * FROM trades WHERE signals_json IS NOT NULL AND was_correct IS NOT NULL \
             AND status='CLOSED' AND exit_type IN ('auto_resolved','resolution') \
             ORDER BY exit_timestamp ASC, id ASC",
        )
        .fetch_all(&self.db)
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                warn!("ML backfill could not read durable resolutions: {error}");
                return;
            }
        };
        // Label completion is independently idempotent. A saved LR identity
        // must not prevent repairing an archive write interrupted after learning.
        for trade in &rows {
            if let Some((_, label)) = resolution_observation(trade) {
                self.complete_resolution_labels(&trade.market_id, label)
                    .await;
                self.score_deep_prediction_for_market(&trade.market_id, label == 1.0)
                    .await;
            }
        }
        let mut ml = self.intelligence.ml_state.write().await;
        let mut count = 0;
        for trade in rows {
            let Some((breakdown, label)) = resolution_observation(&trade) else {
                continue;
            };
            if crate::ml::learn_resolution(&mut ml, resolution_identity(&trade), &breakdown, label)
                .is_some()
            {
                count += 1;
            }
        }
        if count > 0 {
            ml.save();
            info!(
                "ML catch-up: learned {count} unseen durable resolutions, samples={}",
                ml.model.samples_seen
            );
        }
    }

    /// Backfill unresolved exact-archive outcomes for watched markets, even if
    /// we never traded them. This closes the truth loop for live shadow evals.
    pub async fn backfill_unresolved_exact_input_outcomes(
        &self,
        limit: usize,
    ) -> anyhow::Result<usize> {
        let min_age_secs = std::env::var("MACBOT_ARCHIVE_OUTCOME_BACKFILL_MIN_AGE_SECS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(20 * 60)
            .max(60);
        let cutoff = Utc::now() - Duration::seconds(min_age_secs);

        let rows = sqlx::query_as::<_, (String, String, i64)>(
            r#"
            SELECT market_id, MAX(timestamp) AS last_timestamp, COUNT(*) AS unlabeled_rows
            FROM ml_model_inputs
            WHERE outcome IS NULL
            GROUP BY market_id
            ORDER BY last_timestamp ASC
            LIMIT ?
            "#,
        )
        .bind(limit.max(1) as i64)
        .fetch_all(&self.db)
        .await?;

        let mut updated_markets = 0usize;
        let mut updated_model_rows = 0u64;
        let mut updated_feature_rows = 0u64;

        for (market_id, last_timestamp, _unlabeled_rows) in rows {
            let Ok(last_seen) = DateTime::parse_from_rfc3339(&last_timestamp) else {
                continue;
            };
            if last_seen.with_timezone(&Utc) > cutoff {
                continue;
            }

            let Some(outcome) = crate::bot::guard::fetch_binary_market_outcome(&market_id).await
            else {
                continue;
            };

            let model_res = sqlx::query(
                "UPDATE ml_model_inputs SET outcome = ? WHERE market_id = ? AND outcome IS NULL",
            )
            .bind(outcome)
            .bind(&market_id)
            .execute(&self.db)
            .await?;
            let feature_res = sqlx::query(
                "UPDATE ml_features_timeseries SET outcome = ? WHERE market_id = ? AND outcome IS NULL",
            )
            .bind(outcome)
            .bind(&market_id)
            .execute(&self.db)
            .await?;

            let model_rows = model_res.rows_affected();
            let feature_rows = feature_res.rows_affected();
            if model_rows == 0 && feature_rows == 0 {
                continue;
            }

            updated_markets += 1;
            updated_model_rows += model_rows;
            updated_feature_rows += feature_rows;
        }

        if updated_markets > 0 {
            info!(
                "Archive outcome backfill: labeled {updated_markets} market(s), model_rows={}, feature_rows={}",
                updated_model_rows,
                updated_feature_rows
            );
        }

        Ok(updated_markets)
    }

    /// Called after a trade resolves — trains the ML model with the outcome.
    pub async fn ml_on_resolution(&self, trade: &crate::db::Trade) {
        if trade.status_kind() != crate::domain::TradeStatus::Closed
            || !matches!(
                trade.exit_type.as_deref(),
                Some("auto_resolved" | "resolution")
            )
        {
            return;
        }
        let Some((breakdown, label)) = resolution_observation(trade) else {
            return;
        };
        self.complete_resolution_labels(&trade.market_id, label)
            .await;
        self.score_deep_prediction_for_market(&trade.market_id, label == 1.0)
            .await;
        let mut ml = self.intelligence.ml_state.write().await;
        let Some((predicted_before_update, raw)) =
            crate::ml::learn_resolution(&mut ml, resolution_identity(trade), &breakdown, label)
        else {
            return;
        };
        ml.save();
        drop(ml);

        {
            let won = trade.was_correct == Some(1);
            let pnl = trade.net_realized_pnl();
            if !won {
                let features = raw.to_vec();
                let predicted = predicted_before_update;
                let mut tracker = self.intelligence.hard_negative_tracker.write().await;
                tracker.record_loss(features, predicted, label, pnl.abs());
                tracing::debug!(
                    "Hard negative recorded: market={} loss={:.2} total={}",
                    trade.market_id.chars().take(12).collect::<String>(),
                    pnl,
                    tracker.hard_negatives.len()
                );
            }
        }

        {
            let exit_hour = trade
                .exit_timestamp
                .as_deref()
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|dt| dt.with_timezone(&Utc).hour() as u8)
                .unwrap_or_else(|| Utc::now().hour() as u8);
            let pnl = trade.net_realized_pnl();
            let won = pnl > 0.0;
            self.strategy_services
                .sleep_schedule
                .write()
                .await
                .record_outcome(exit_hour, won, pnl);
        }

        {
            let predicted_edge = trade.expected_value;
            // `trade.amount` is the residual after any partial close, which
            // would weight a fully-staked position as though it were pennies.
            let stake = crate::db::trade_entry_cost(&self.db, trade.id)
                .await
                .ok()
                .flatten()
                .filter(|cost| *cost > 0.0)
                .unwrap_or(trade.amount);
            let mut et = self.intelligence.edge_tracker.lock().await;
            et.record(predicted_edge, trade.net_realized_pnl(), stake);
            tracing::debug!(
                "Edge tracker: predicted={:.3} pnl={:.4} stake={:.4} n={} realised={:?} ratio={:?}",
                predicted_edge,
                trade.pnl.unwrap_or(0.0),
                stake,
                et.len(),
                et.mean_actual_edge(),
                et.calibration_ratio()
            );
        }
    }

    /// Retryable archive completion is separate from exactly-once LR training.
    async fn complete_resolution_labels(&self, market_id: &str, label: f64) {
        let outcome_i = label as i64;
        if let Err(e) = sqlx::query(
            "UPDATE ml_features_timeseries SET outcome = ? WHERE market_id = ? AND outcome IS NULL",
        )
        .bind(outcome_i)
        .bind(market_id)
        .execute(&self.db)
        .await
        {
            warn!("Failed to backfill ml_features_timeseries outcome: {e}");
        }
        if let Err(e) = sqlx::query(
            "UPDATE ml_model_inputs SET outcome = ? WHERE market_id = ? AND outcome IS NULL",
        )
        .bind(outcome_i)
        .bind(market_id)
        .execute(&self.db)
        .await
        {
            warn!("Failed to backfill ml_model_inputs outcome: {e}");
        }
    }

    /// Score any pending deep model prediction for a market, regardless of trade state.
    /// `yes_won` = true if YES outcome, false if NO outcome.
    pub async fn score_deep_prediction_for_market(&self, market_id: &str, yes_won: bool) {
        let label = if yes_won { 1.0_f64 } else { 0.0 };
        let mut ensemble = self.intelligence.ensemble_state.write().await;
        if let Some(predicted_p_up) = ensemble.pending_predictions.remove(market_id) {
            ensemble.record_deep_prediction(predicted_p_up, label);
            ensemble.save_to_disk();
            info!(
                "Deep model scored (market resolve): market={} pred={:.3} actual={} samples={}",
                market_id.chars().take(12).collect::<String>(),
                predicted_p_up,
                label,
                ensemble.deep_tracker.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {

    /// A partial close rewrites `trades.amount` down to the residual while the
    /// pnl keeps the whole position's result, so weighting by `amount` treats a
    /// dollar position as though it were a dime and reports a tenfold return.
    /// Twenty-eight of the sixty-six real trades are in this shape.
    #[tokio::test]
    async fn a_partially_closed_trade_is_weighted_by_what_it_cost_not_what_is_left(
    ) -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_edge_partial_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let trade = crate::db::Trade {
            id: 1,
            timestamp: "2026-03-01T00:00:00+00:00".to_string(),
            market_id: "m1".to_string(),
            status: "CLOSED".to_string(),
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.10,
            pnl: Some(0.10),
            exit_timestamp: Some("2026-03-01T01:00:00+00:00".to_string()),
            ..Default::default()
        };
        crate::db::insert_trade(&pool, &trade).await?;

        // What partial_close does to the row afterwards.
        let closed = crate::db::Trade {
            amount: 0.1,
            shares: 0.2,
            partial_closed: Some(1),
            ..trade.clone()
        };
        crate::db::update_trade(&pool, &closed).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        let bot = std::sync::Arc::new(crate::bot::state::TradingBot::new(cfg, pool).await?);
        bot.edge_tracker_backfill().await;

        let tracker = bot.intelligence.edge_tracker.lock().await.to_json();
        let realised = tracker["mean_actual_edge"].as_f64().expect("realised edge");
        assert!(
            (realised - 0.10).abs() < 1e-9,
            "realised edge {realised} — a $0.10 gain on a $1.00 stake is 10%, not \
             the 100% you get by dividing by the residual"
        );
        let ratio = tracker["calibration_ratio"].as_f64().expect("ratio");
        assert!((ratio - 1.0).abs() < 1e-9, "calibration ratio {ratio}");

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn edge_tracker_survives_a_restart_by_replaying_closed_trades() -> anyhow::Result<()> {
        // The tracker is in-memory with no persistence, so before this every
        // restart reset it to zero observations. On a bot that restarts often
        // it never accumulated anything, and the calibration figures on the
        // dashboard were permanently blank.
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_edge_backfill_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        // Two closed trades: one that beat its predicted edge, one that missed.
        for (id, expected, pnl) in [(1_i64, 0.10_f64, 0.50_f64), (2, 0.10, -0.10)] {
            let trade = crate::db::Trade {
                id,
                timestamp: "2026-03-01T00:00:00+00:00".to_string(),
                market_id: format!("m{id}"),
                status: "CLOSED".to_string(),
                amount: 1.0,
                expected_value: expected,
                pnl: Some(pnl),
                exit_timestamp: Some("2026-03-01T01:00:00+00:00".to_string()),
                ..Default::default()
            };
            crate::db::insert_trade(&pool, &trade).await?;
        }

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        let bot = std::sync::Arc::new(crate::bot::state::TradingBot::new(cfg, pool).await?);

        // Constructing the bot is the restart: boot fills the tracker, and
        // calling the backfill again must not double count.
        bot.edge_tracker_backfill().await;

        let tracker = bot.intelligence.edge_tracker.lock().await.to_json();
        assert_eq!(
            tracker["observations"], 2,
            "both closed trades should be replayed into the tracker"
        );
        let predicted = tracker["mean_predicted_edge"].as_f64().unwrap_or(-1.0);
        let actual = tracker["mean_actual_edge"].as_f64().unwrap_or(-1.0);
        assert!(
            (predicted - 0.10).abs() < 1e-9,
            "predicted edge was {predicted}"
        );
        assert!((actual - 0.20).abs() < 1e-9, "actual edge was {actual}");
        assert!(
            tracker["calibration_ratio"].as_f64().is_some(),
            "a populated tracker must be able to state a calibration ratio"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    use super::*;
    use crate::db::Trade;

    fn learning_trade(id: i64) -> Trade {
        Trade {
            id,
            timestamp: "2026-03-27T00:00:00Z".to_string(),
            market_id: "mkt-learning-1".to_string(),
            question: "learning-path-test".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.5,
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.15,
            reason: "learning-path-test".to_string(),
            confidence: 0.9,
            status: "CLOSED".to_string(),
            mode: "simulation".to_string(),
            source: "bot".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: Some(0.7),
            exit_timestamp: Some("2026-03-27T01:00:00Z".to_string()),
            pnl: Some(0.20),
            exit_type: Some("resolution".to_string()),
            was_correct: Some(1),
            forced: Some(0),
            partial_closed: Some(0),
            raw_confidence: Some(0.9),
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: Some(r#"{"orderbook_imbalance":0.6,"tick_velocity":0.2}"#.to_string()),
            calibration_version: Some("test-cal-v1".to_string()),
            run_id: Some("learning-path-test".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    async fn new_learning_bot() -> anyhow::Result<(TradingBot, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_learning_intel_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();

        let bot = TradingBot::new(cfg, pool).await?;
        let mut fresh = crate::ml::MlState::new();
        fresh.snapshot_path = format!("{path_str}.ml.json");
        *bot.intelligence.ml_state.write().await = fresh;
        Ok((bot, path_str))
    }

    /// The live resolution path, as distinct from the boot backfill. By the time
    /// a partially closed position resolves, the `Trade` handed to
    /// `ml_on_resolution` carries the residual in `amount`, so the stake has to
    /// be read back from the row rather than taken from the struct.
    #[tokio::test]
    async fn resolving_a_partially_closed_trade_weights_it_by_the_entry_cost() -> anyhow::Result<()>
    {
        let (bot, path_str) = new_learning_bot().await?;

        let opened = Trade {
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.10,
            pnl: None,
            status: "OPEN".to_string(),
            ..learning_trade(1)
        };
        crate::db::insert_trade(&bot.db, &opened).await?;

        // Two thirds closed early: the row keeps the residual, the pnl keeps
        // the whole position's result.
        let resolved = Trade {
            amount: 0.1,
            shares: 0.2,
            partial_closed: Some(1),
            pnl: Some(0.10),
            status: "CLOSED".to_string(),
            ..opened.clone()
        };
        crate::db::update_trade(&bot.db, &resolved).await?;

        bot.ml_on_resolution(&resolved).await;

        let tracker = bot.intelligence.edge_tracker.lock().await.to_json();
        assert_eq!(tracker["observations"], 1);
        let realised = tracker["mean_actual_edge"].as_f64().expect("realised edge");
        assert!(
            (realised - 0.10).abs() < 1e-9,
            "realised edge {realised}: a $0.10 gain on the $1.00 this position \
             actually cost is 10%, not the 100% the residual implies"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path_str);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn ml_on_resolution_records_edge_tracker_and_backfills_outcomes() -> anyhow::Result<()> {
        let (bot, path_str) = new_learning_bot().await?;
        let trade = learning_trade(1);
        let archived_at = Utc::now().to_rfc3339();

        sqlx::query(
            "INSERT INTO ml_features_timeseries (timestamp, run_id, market_id, features_json, fusion_score, p_up, signal, regime, phase, outcome, staleness_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&archived_at)
        .bind("learning-path-test")
        .bind(&trade.market_id)
        .bind(r#"{"orderbook_imbalance":0.6,"tick_velocity":0.2}"#)
        .bind(0.15_f64)
        .bind(0.72_f64)
        .bind("BUY_YES")
        .bind("TREND_UP")
        .bind("shadow")
        .bind(Option::<i64>::None)
        .bind(r#"[0.0,0.0]"#)
        .execute(&bot.db)
        .await?;
        sqlx::query(
            "INSERT INTO ml_model_inputs (timestamp, run_id, market_id, signal, regime_label, phase, fusion_score, p_up, regime_value, features_json, scalars_json, staleness_json, candles_1m_json, candles_5m_json, candles_15m_json, orderbook_seq_json, outcome) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&archived_at)
        .bind("learning-path-test")
        .bind(&trade.market_id)
        .bind("BUY_YES")
        .bind("TREND_UP")
        .bind("shadow")
        .bind(0.15_f64)
        .bind(0.72_f64)
        .bind(1.0_f64)
        .bind(r#"{"orderbook_imbalance":0.6,"tick_velocity":0.2}"#)
        .bind("[0.6,0.2]")
        .bind("[0.0,0.0]")
        .bind("[]")
        .bind("[]")
        .bind("[]")
        .bind("[]")
        .bind(Option::<i64>::None)
        .execute(&bot.db)
        .await?;

        bot.ml_on_resolution(&trade).await;

        let edge_tracker = bot.intelligence.edge_tracker.lock().await.to_json();
        assert_eq!(edge_tracker["observations"], 1);
        assert!(
            (edge_tracker["mean_predicted_edge"]
                .as_f64()
                .unwrap_or_default()
                - 0.15)
                .abs()
                < 1e-9
        );
        assert!(
            (edge_tracker["mean_actual_edge"]
                .as_f64()
                .unwrap_or_default()
                - 0.20)
                .abs()
                < 1e-9
        );

        let model_outcome: Option<f64> =
            sqlx::query_scalar("SELECT outcome FROM ml_model_inputs WHERE market_id = ? LIMIT 1")
                .bind(&trade.market_id)
                .fetch_one(&bot.db)
                .await?;
        let feature_outcome: Option<f64> = sqlx::query_scalar(
            "SELECT outcome FROM ml_features_timeseries WHERE market_id = ? LIMIT 1",
        )
        .bind(&trade.market_id)
        .fetch_one(&bot.db)
        .await?;
        assert_eq!(model_outcome, Some(1.0));
        assert_eq!(feature_outcome, Some(1.0));

        drop(bot);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        let _ = std::fs::remove_file(path_str);
        Ok(())
    }
    #[tokio::test]
    async fn saved_learning_state_catches_unseen_durable_resolution_once() -> anyhow::Result<()> {
        let (bot, path) = new_learning_bot().await?;
        let first = learning_trade(901);
        let second = Trade {
            market_id: "second-resolution".into(),
            exit_timestamp: Some("2026-03-27T02:00:00Z".into()),
            ..learning_trade(902)
        };
        crate::db::insert_trade(&bot.db, &first).await?;
        // Actual callback saves the first learned identity with the weights.
        bot.ml_on_resolution(&first).await;
        let snapshot_path = bot.intelligence.ml_state.read().await.snapshot_path.clone();
        let snapshot = crate::ml::persistence::load(&snapshot_path).expect("saved state");
        assert_eq!(snapshot.model.samples_seen, 1);
        crate::db::insert_trade(&bot.db, &second).await?;
        // Restart from that older on-disk snapshot, then catch up the newer row.
        let mut restored = crate::ml::MlState::from_snapshot(snapshot);
        restored.snapshot_path = snapshot_path.clone();
        *bot.intelligence.ml_state.write().await = restored;
        bot.ml_backfill().await;
        bot.ml_backfill().await;
        bot.ml_on_resolution(&second).await;
        let final_snapshot =
            crate::ml::persistence::load(&snapshot_path).expect("caught-up saved state");
        assert_eq!(final_snapshot.model.samples_seen, 2);
        assert_eq!(final_snapshot.learned_outcomes.as_ref().unwrap().len(), 2);
        assert_eq!(final_snapshot.calibrator.observations.len(), 2);
        bot.db.close().await;
        let _ = std::fs::remove_file(snapshot_path);
        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[tokio::test]
    async fn failed_learning_save_replays_from_older_snapshot_without_double_learning(
    ) -> anyhow::Result<()> {
        let (bot, path) = new_learning_bot().await?;
        let first = learning_trade(903);
        let second = Trade {
            market_id: "later-outcome".into(),
            ..learning_trade(904)
        };
        crate::db::insert_trade(&bot.db, &first).await?;
        bot.ml_on_resolution(&first).await;
        let saved_path = bot.intelligence.ml_state.read().await.snapshot_path.clone();
        let older = crate::ml::persistence::load(&saved_path).unwrap();
        crate::db::insert_trade(&bot.db, &second).await?;
        // A missing parent makes the actual snapshot write fail.
        bot.intelligence.ml_state.write().await.snapshot_path =
            format!("{path}.missing/state.json");
        bot.ml_on_resolution(&second).await;
        assert_eq!(bot.intelligence.ml_state.read().await.model.samples_seen, 2);
        let mut restored = crate::ml::MlState::from_snapshot(older);
        restored.snapshot_path = saved_path.clone();
        *bot.intelligence.ml_state.write().await = restored;
        bot.ml_backfill().await;
        assert_eq!(bot.intelligence.ml_state.read().await.model.samples_seen, 2);
        bot.ml_backfill().await;
        assert_eq!(bot.intelligence.ml_state.read().await.model.samples_seen, 2);
        bot.db.close().await;
        let _ = std::fs::remove_file(saved_path);
        let _ = std::fs::remove_file(path);
        Ok(())
    }
    #[tokio::test]
    async fn consumed_learning_identity_does_not_block_archive_label_repair() -> anyhow::Result<()>
    {
        for use_backfill in [false, true] {
            let (bot, path) = new_learning_bot().await?;
            let trade = learning_trade(905);
            crate::db::insert_trade(&bot.db, &trade).await?;
            sqlx::query("INSERT INTO ml_features_timeseries(timestamp,run_id,market_id,features_json,fusion_score,p_up,signal,regime,phase,staleness_json) VALUES(?,?,?,'{}',0.1,0.7,'BUY_YES','TREND_UP','shadow','[]')")
                .bind(&trade.timestamp).bind("label-repair").bind(&trade.market_id).execute(&bot.db).await?;
            sqlx::query("INSERT INTO ml_model_inputs(timestamp,run_id,market_id,signal,regime_label,phase,fusion_score,p_up,regime_value,features_json,scalars_json,staleness_json,candles_1m_json,candles_5m_json,candles_15m_json,orderbook_seq_json) VALUES(?,?,?,'BUY_YES','TREND_UP','shadow',0.1,0.7,1.0,'{}','[]','[]','[]','[]','[]','[]')")
                .bind(&trade.timestamp).bind("label-repair").bind(&trade.market_id).execute(&bot.db).await?;
            sqlx::query("CREATE TRIGGER refuse_labels BEFORE UPDATE ON ml_model_inputs BEGIN SELECT RAISE(ABORT,'injected label failure'); END")
                .execute(&bot.db).await?;
            bot.ml_on_resolution(&trade).await;
            let saved = bot.intelligence.ml_state.read().await.snapshot_path.clone();
            assert_eq!(
                crate::ml::persistence::load(&saved)
                    .unwrap()
                    .model
                    .samples_seen,
                1
            );
            let outcome: Option<f64> =
                sqlx::query_scalar("SELECT outcome FROM ml_model_inputs WHERE market_id=?")
                    .bind(&trade.market_id)
                    .fetch_one(&bot.db)
                    .await?;
            assert!(outcome.is_none());
            sqlx::query("DROP TRIGGER refuse_labels")
                .execute(&bot.db)
                .await?;
            if use_backfill {
                bot.ml_backfill().await;
            } else {
                bot.ml_on_resolution(&trade).await;
            }
            let outcome: Option<f64> =
                sqlx::query_scalar("SELECT outcome FROM ml_model_inputs WHERE market_id=?")
                    .bind(&trade.market_id)
                    .fetch_one(&bot.db)
                    .await?;
            assert_eq!(outcome, Some(1.0));
            assert_eq!(bot.intelligence.ml_state.read().await.model.samples_seen, 1);
            assert_eq!(
                bot.intelligence
                    .ml_state
                    .read()
                    .await
                    .calibrator
                    .observations
                    .len(),
                1
            );
            assert_eq!(bot.intelligence.edge_tracker.lock().await.len(), 1);
            bot.db.close().await;
            let _ = std::fs::remove_file(saved);
            let _ = std::fs::remove_file(path);
        }
        Ok(())
    }
}
