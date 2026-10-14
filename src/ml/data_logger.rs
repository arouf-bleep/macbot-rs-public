//! Non-blocking data logger for ML training data.
//!
//! Writes to 6 SQLite tables via an mpsc channel so the hot trading loop
//! is never blocked by I/O. Auto-prunes rows older than 30 days.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::Utc;
use sqlx::{Row, SqlitePool};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::{deep_model::DeepModelInput, model::NUM_FEATURES};

static DROPPED_EVENTS: AtomicU64 = AtomicU64::new(0);

fn record_dropped_event() -> u64 {
    DROPPED_EVENTS.fetch_add(1, Ordering::Relaxed) + 1
}

// ── Log event types ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum MlLogEvent {
    Candle1m {
        ts: String,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
        market_id: Option<String>,
        btc_price: Option<f64>,
    },
    OrderbookSnapshot {
        ts: String,
        /// JSON-encoded top 20 bid/ask levels
        levels_json: String,
        spread_bps: f64,
        imbalance: f64,
        mid_price: f64,
    },
    TickHistory {
        ts: String,
        source: String, // "binance" | "polymarket"
        price: f64,
        volume: Option<f64>,
    },
    FeaturesTimeseries {
        ts: String,
        run_id: String,
        market_id: String,
        /// JSON-encoded 14-feature breakdown
        features_json: String,
        fusion_score: f64,
        p_up: f64,
        signal: String,
        regime: String,
        phase: String,
        /// Outcome (filled in after market resolves): 1.0 = UP, 0.0 = DOWN
        outcome: Option<f64>,
        /// Seconds since each feature was last updated (14 values)
        staleness_json: Option<String>,
    },
    ModelInputSnapshot {
        ts: String,
        run_id: String,
        market_id: String,
        signal: String,
        regime_label: String,
        phase: String,
        fusion_score: f64,
        p_up: f64,
        regime_value: f64,
        features_json: String,
        scalars_json: String,
        staleness_json: String,
        candles_1m_json: String,
        candles_5m_json: String,
        candles_15m_json: String,
        orderbook_seq_json: String,
        outcome: Option<f64>,
    },
    AggressorFlow {
        ts: String,
        buy_vol: f64,
        sell_vol: f64,
        ratio: f64,
        net_flow: f64,
    },
    Liquidation {
        ts: String,
        side: String, // "LONG" | "SHORT"
        size_usd: f64,
        price: f64,
        symbol: String,
    },
    /// Prune old rows (fired by internal timer, not external callers).
    Prune,
}

// ── DataLogger ──────────────────────────────────────────────────────────────

pub struct DataLogger {
    tx: mpsc::Sender<MlLogEvent>,
}

impl DataLogger {
    /// Create a new DataLogger. Spawns a background writer task.
    pub fn new(pool: SqlitePool) -> Self {
        let (tx, rx) = mpsc::channel::<MlLogEvent>(4096);

        // Spawn the writer
        crate::task_monitor::spawn_monitored_task("ml_data_logger_writer", writer_loop(pool, rx));

        Self { tx }
    }

    /// Total number of events dropped because the channel was full.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dropped_events_total() -> u64 {
        DROPPED_EVENTS.load(Ordering::Relaxed)
    }

    /// Send a log event. Non-blocking - drops are counted and sampled in logs if buffer full.
    pub fn log(&self, event: MlLogEvent) {
        if let Err(e) = self.tx.try_send(event) {
            match e {
                mpsc::error::TrySendError::Full(_) => {
                    let dropped = record_dropped_event();
                    if dropped == 1 || dropped.is_multiple_of(1000) {
                        warn!("DataLogger: dropped {dropped} events (channel full)");
                    }
                }
                mpsc::error::TrySendError::Closed(_) => {
                    warn!("DataLogger: channel closed");
                }
            }
        }
    }

    /// Convenience: log a 1-min candle.
    pub fn log_candle(&self, open: f64, high: f64, low: f64, close: f64, volume: f64) {
        self.log(MlLogEvent::Candle1m {
            ts: Utc::now().to_rfc3339(),
            open,
            high,
            low,
            close,
            volume,
            market_id: None,
            btc_price: None,
        });
    }

    // These fields mirror a timestamped candle evidence record.
    #[allow(clippy::too_many_arguments)]
    pub fn log_candle_at(
        &self,
        ts: String,
        open: f64,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
        market_id: Option<&str>,
        btc_price: Option<f64>,
    ) {
        self.log(MlLogEvent::Candle1m {
            ts,
            open,
            high,
            low,
            close,
            volume,
            market_id: market_id.map(str::to_string),
            btc_price,
        });
    }

    /// Convenience: log a liquidation event.
    pub fn log_liquidation(&self, side: &str, size_usd: f64, price: f64, symbol: &str) {
        self.log(MlLogEvent::Liquidation {
            ts: Utc::now().to_rfc3339(),
            side: side.to_string(),
            size_usd,
            price,
            symbol: symbol.to_string(),
        });
    }

    /// Convenience: log an orderbook snapshot.
    pub fn log_orderbook(
        &self,
        levels_json: String,
        spread_bps: f64,
        imbalance: f64,
        mid_price: f64,
    ) {
        self.log(MlLogEvent::OrderbookSnapshot {
            ts: Utc::now().to_rfc3339(),
            levels_json,
            spread_bps,
            imbalance,
            mid_price,
        });
    }

    /// Convenience: log feature timeseries.
    // These fields bind the feature evidence to its decision and freshness record.
    #[allow(clippy::too_many_arguments)]
    pub fn log_features(
        &self,
        run_id: &str,
        market_id: &str,
        features: &HashMap<String, f64>,
        fusion_score: f64,
        p_up: f64,
        signal: &str,
        regime: &str,
        phase: &str,
        staleness: Option<&[f64; NUM_FEATURES]>,
    ) {
        let features_json = serde_json::to_string(features).unwrap_or_else(|_| "{}".to_string());
        let staleness_json = staleness
            .map(|s| serde_json::to_string(s.as_slice()).unwrap_or_else(|_| "[]".to_string()));
        self.log(MlLogEvent::FeaturesTimeseries {
            ts: Utc::now().to_rfc3339(),
            run_id: run_id.to_string(),
            market_id: market_id.to_string(),
            features_json,
            fusion_score,
            p_up,
            signal: signal.to_string(),
            regime: regime.to_string(),
            phase: phase.to_string(),
            outcome: None,
            staleness_json,
        });
    }

    // These fields bind the native model input to its exact decision evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn log_model_input(
        &self,
        run_id: &str,
        market_id: &str,
        features: &HashMap<String, f64>,
        fusion_score: f64,
        p_up: f64,
        signal: &str,
        regime: &str,
        phase: &str,
        input: &DeepModelInput,
    ) {
        let features_json = serde_json::to_string(features).unwrap_or_else(|_| "{}".to_string());
        let scalars_json =
            serde_json::to_string(&input.scalars).unwrap_or_else(|_| "[]".to_string());
        let staleness_json =
            serde_json::to_string(&input.staleness).unwrap_or_else(|_| "[]".to_string());
        let candles_1m_json =
            serde_json::to_string(&input.candles_1m).unwrap_or_else(|_| "[]".to_string());
        let candles_5m_json =
            serde_json::to_string(&input.candles_5m).unwrap_or_else(|_| "[]".to_string());
        let candles_15m_json =
            serde_json::to_string(&input.candles_15m).unwrap_or_else(|_| "[]".to_string());
        let orderbook_seq_json =
            serde_json::to_string(&input.orderbook_seq).unwrap_or_else(|_| "[]".to_string());

        self.log(MlLogEvent::ModelInputSnapshot {
            ts: Utc::now().to_rfc3339(),
            run_id: run_id.to_string(),
            market_id: market_id.to_string(),
            signal: signal.to_string(),
            regime_label: regime.to_string(),
            phase: phase.to_string(),
            fusion_score,
            p_up,
            regime_value: input.regime as f64,
            features_json,
            scalars_json,
            staleness_json,
            candles_1m_json,
            candles_5m_json,
            candles_15m_json,
            orderbook_seq_json,
            outcome: None,
        });
    }

    /// Convenience: log aggressor flow.
    pub fn log_aggressor(&self, buy_vol: f64, sell_vol: f64) {
        let total = buy_vol + sell_vol;
        let ratio = if total > 0.0 { buy_vol / total } else { 0.5 };
        self.log(MlLogEvent::AggressorFlow {
            ts: Utc::now().to_rfc3339(),
            buy_vol,
            sell_vol,
            ratio,
            net_flow: buy_vol - sell_vol,
        });
    }

    /// Convenience: log a tick.
    pub fn log_tick(&self, source: &str, price: f64, volume: Option<f64>) {
        self.log(MlLogEvent::TickHistory {
            ts: Utc::now().to_rfc3339(),
            source: source.to_string(),
            price,
            volume,
        });
    }

    #[cfg(test)]
    pub(crate) fn with_sender(tx: mpsc::Sender<MlLogEvent>) -> Self {
        Self { tx }
    }

    #[cfg(test)]
    pub(crate) fn reset_dropped_events_for_tests() {
        DROPPED_EVENTS.store(0, Ordering::Relaxed);
    }
}

// ── Background writer ───────────────────────────────────────────────────────

async fn writer_loop(pool: SqlitePool, mut rx: mpsc::Receiver<MlLogEvent>) {
    // Spawn auto-prune timer
    let prune_pool = pool.clone();
    crate::task_monitor::spawn_monitored_task("ml_data_logger_prune", async move {
        let prune_interval = Duration::from_secs(3600);
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + prune_interval, prune_interval);
        loop {
            interval.tick().await;
            if let Err(e) = prune_old_rows(&prune_pool).await {
                warn!("DataLogger: prune error: {e}");
            }
        }
    });

    while let Some(event) = rx.recv().await {
        let result: anyhow::Result<()> = match event {
            MlLogEvent::Candle1m {
                ts,
                open,
                high,
                low,
                close,
                volume,
                market_id,
                btc_price,
            } => {
                sqlx::query(
                    "INSERT OR IGNORE INTO ml_candles_1m (timestamp, open, high, low, close, volume, market_id, btc_price) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&ts).bind(open).bind(high).bind(low).bind(close).bind(volume)
                .bind(market_id.as_deref().unwrap_or(""))
                .bind(btc_price)
                .execute(&pool).await.map(|_| ()).map_err(Into::into)
            }
            MlLogEvent::OrderbookSnapshot {
                ts,
                levels_json,
                spread_bps,
                imbalance,
                mid_price,
            } => {
                sqlx::query(
                    "INSERT INTO ml_orderbook_snapshots (timestamp, levels_json, spread_bps, imbalance, mid_price) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&ts).bind(&levels_json).bind(spread_bps).bind(imbalance).bind(mid_price)
                .execute(&pool).await.map(|_| ()).map_err(Into::into)
            }
            MlLogEvent::TickHistory {
                ts,
                source,
                price,
                volume,
            } => {
                sqlx::query(
                    "INSERT INTO ml_tick_history (timestamp, source, price, volume) VALUES (?, ?, ?, ?)",
                )
                .bind(&ts).bind(&source).bind(price).bind(volume)
                .execute(&pool).await.map(|_| ()).map_err(Into::into)
            }
            MlLogEvent::FeaturesTimeseries {
                ts,
                run_id,
                market_id,
                features_json,
                fusion_score,
                p_up,
                signal,
                regime,
                phase,
                outcome,
                staleness_json,
            } => {
                sqlx::query(
                    "INSERT INTO ml_features_timeseries (timestamp, run_id, market_id, features_json, fusion_score, p_up, signal, regime, phase, outcome, staleness_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&ts).bind(&run_id).bind(&market_id).bind(&features_json)
                .bind(fusion_score).bind(p_up).bind(&signal).bind(&regime).bind(&phase)
                .bind(outcome).bind(staleness_json.as_deref())
                .execute(&pool).await.map(|_| ()).map_err(Into::into)
            }
            MlLogEvent::AggressorFlow {
                ts,
                buy_vol,
                sell_vol,
                ratio,
                net_flow,
            } => {
                sqlx::query(
                    "INSERT INTO ml_aggressor_flow (timestamp, buy_vol, sell_vol, ratio, net_flow) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&ts).bind(buy_vol).bind(sell_vol).bind(ratio).bind(net_flow)
                .execute(&pool).await.map(|_| ()).map_err(Into::into)
            }
            MlLogEvent::ModelInputSnapshot {
                ts,
                run_id,
                market_id,
                signal,
                regime_label,
                phase,
                fusion_score,
                p_up,
                regime_value,
                features_json,
                scalars_json,
                staleness_json,
                candles_1m_json,
                candles_5m_json,
                candles_15m_json,
                orderbook_seq_json,
                outcome,
            } => {
                sqlx::query(
                    "INSERT INTO ml_model_inputs (timestamp, run_id, market_id, signal, regime_label, phase, fusion_score, p_up, regime_value, features_json, scalars_json, staleness_json, candles_1m_json, candles_5m_json, candles_15m_json, orderbook_seq_json, outcome) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&ts)
                .bind(&run_id)
                .bind(&market_id)
                .bind(&signal)
                .bind(&regime_label)
                .bind(&phase)
                .bind(fusion_score)
                .bind(p_up)
                .bind(regime_value)
                .bind(&features_json)
                .bind(&scalars_json)
                .bind(&staleness_json)
                .bind(&candles_1m_json)
                .bind(&candles_5m_json)
                .bind(&candles_15m_json)
                .bind(&orderbook_seq_json)
                .bind(outcome)
                .execute(&pool)
                .await
                .map(|_| ())
                .map_err(Into::into)
            }
            MlLogEvent::Liquidation {
                ts,
                side,
                size_usd,
                price,
                symbol,
            } => {
                sqlx::query(
                    "INSERT INTO ml_liquidations (timestamp, side, size_usd, price, symbol) VALUES (?, ?, ?, ?, ?)",
                )
                .bind(&ts).bind(&side).bind(size_usd).bind(price).bind(&symbol)
                .execute(&pool).await.map(|_| ()).map_err(Into::into)
            }
            MlLogEvent::Prune => prune_old_rows(&pool).await,
        };

        if let Err(e) = result {
            warn!("DataLogger: write error: {e}");
        }
    }
}

async fn prune_old_rows(pool: &SqlitePool) -> anyhow::Result<()> {
    let cutoff = (Utc::now() - chrono::Duration::days(30)).to_rfc3339();

    let tables = [
        "ml_candles_1m",
        "ml_orderbook_snapshots",
        "ml_tick_history",
        "ml_features_timeseries",
        "ml_model_inputs",
        "ml_aggressor_flow",
        "ml_liquidations",
    ];

    let mut total_deleted = 0i64;
    for table in &tables {
        // `tables` is the fixed internal collection-table list above.
        let result = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE timestamp < ?"
        )))
        .bind(&cutoff)
        .execute(pool)
        .await?;
        total_deleted += result.rows_affected() as i64;
    }

    if total_deleted > 0 {
        info!("DataLogger: pruned {total_deleted} rows older than 30 days");
    }
    Ok(())
}

async fn table_has_column(pool: &SqlitePool, table: &str, column: &str) -> anyhow::Result<bool> {
    let pragma = format!("PRAGMA table_info(\"{}\")", table.replace('"', "\"\""));
    let rows = sqlx::query(sqlx::AssertSqlSafe(pragma))
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .any(|row| row.get::<String, _>("name").eq_ignore_ascii_case(column)))
}

/// Create the 6 ML data collection tables. Called during DB migration.
pub async fn create_ml_tables(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS ml_candles_1m (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            open      REAL NOT NULL,
            high      REAL NOT NULL,
            low       REAL NOT NULL,
            close     REAL NOT NULL,
            volume    REAL NOT NULL,
            market_id TEXT NOT NULL DEFAULT '',
            btc_price REAL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_ml_candles_1m_ts ON ml_candles_1m(timestamp)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_ml_candles_1m_ts_market_unique ON ml_candles_1m(timestamp, market_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS ml_orderbook_snapshots (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp   TEXT NOT NULL,
            levels_json TEXT NOT NULL,
            spread_bps  REAL NOT NULL,
            imbalance   REAL NOT NULL,
            mid_price   REAL NOT NULL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ml_ob_snapshots_ts ON ml_orderbook_snapshots(timestamp)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS ml_tick_history (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            source    TEXT NOT NULL,
            price     REAL NOT NULL,
            volume    REAL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_ml_ticks_ts ON ml_tick_history(timestamp)")
        .execute(pool)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS ml_features_timeseries (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp      TEXT NOT NULL,
            run_id         TEXT NOT NULL,
            market_id      TEXT NOT NULL,
            features_json  TEXT NOT NULL,
            fusion_score   REAL NOT NULL,
            p_up           REAL NOT NULL,
            signal         TEXT NOT NULL,
            regime         TEXT NOT NULL,
            phase          TEXT NOT NULL,
            outcome        REAL,
            staleness_json TEXT
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ml_features_ts ON ml_features_timeseries(timestamp)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ml_features_market ON ml_features_timeseries(market_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS ml_model_inputs (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp        TEXT NOT NULL,
            run_id           TEXT NOT NULL,
            market_id        TEXT NOT NULL,
            signal           TEXT NOT NULL,
            regime_label     TEXT NOT NULL,
            phase            TEXT NOT NULL,
            fusion_score     REAL NOT NULL,
            p_up             REAL NOT NULL,
            regime_value     REAL NOT NULL,
            features_json    TEXT NOT NULL,
            scalars_json     TEXT NOT NULL,
            staleness_json   TEXT NOT NULL,
            candles_1m_json  TEXT NOT NULL,
            candles_5m_json  TEXT NOT NULL,
            candles_15m_json TEXT NOT NULL,
            orderbook_seq_json TEXT NOT NULL,
            outcome          REAL
        )"#,
    )
    .execute(pool)
    .await?;
    if !table_has_column(pool, "ml_model_inputs", "outcome").await? {
        sqlx::query("ALTER TABLE ml_model_inputs ADD COLUMN outcome REAL")
            .execute(pool)
            .await?;
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_ml_model_inputs_ts ON ml_model_inputs(timestamp)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ml_model_inputs_market ON ml_model_inputs(market_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS ml_aggressor_flow (
            id        INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            buy_vol   REAL NOT NULL,
            sell_vol  REAL NOT NULL,
            ratio     REAL NOT NULL,
            net_flow  REAL NOT NULL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_ml_aggressor_ts ON ml_aggressor_flow(timestamp)")
        .execute(pool)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS ml_liquidations (
            id       INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            side      TEXT NOT NULL,
            size_usd  REAL NOT NULL,
            price     REAL NOT NULL,
            symbol    TEXT NOT NULL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_ml_liquidations_ts ON ml_liquidations(timestamp)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_ml_liquidations_side ON ml_liquidations(side, timestamp)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use sqlx::Row;
    use tokio::sync::mpsc;

    use super::*;

    fn temp_sqlite_path(prefix: &str) -> anyhow::Result<(std::path::PathBuf, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("{prefix}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        Ok((path, path_str))
    }

    #[tokio::test]
    async fn create_ml_tables_supports_exact_model_input_archive() -> anyhow::Result<()> {
        let (path, path_str) = temp_sqlite_path("macbot_ml_model_inputs")?;
        let pool = crate::db::open(&path_str).await?;
        let logger = DataLogger::new(pool.clone());

        let mut features = HashMap::new();
        features.insert("orderbook_imbalance".to_string(), 0.12);
        features.insert("tick_velocity".to_string(), -0.07);
        let input = DeepModelInput {
            candles_1m: vec![1.0; 60 * 5],
            candles_5m: vec![2.0; 24 * 5],
            candles_15m: vec![3.0; 16 * 5],
            orderbook_seq: vec![4.0; 12 * 45],
            scalars: vec![5.0; NUM_FEATURES + 6],
            staleness: vec![0.25; NUM_FEATURES],
            regime: 2.0,
        };

        logger.log_model_input(
            "run-test", "mkt-test", &features, 0.44, 0.61, "BUY_YES", "TRENDING", "active", &input,
        );

        tokio::time::sleep(Duration::from_millis(75)).await;

        let row = sqlx::query(
            "SELECT regime_value, scalars_json, staleness_json, candles_1m_json, features_json, outcome FROM ml_model_inputs LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;

        let regime_value: f64 = row.get(0);
        assert_eq!(regime_value, 2.0);
        let scalars: Vec<f32> = serde_json::from_str(&row.get::<String, _>(1))?;
        let staleness: Vec<f32> = serde_json::from_str(&row.get::<String, _>(2))?;
        let candles_1m: Vec<f32> = serde_json::from_str(&row.get::<String, _>(3))?;
        let stored_features: HashMap<String, f64> = serde_json::from_str(&row.get::<String, _>(4))?;
        let outcome: Option<f64> = row.get(5);

        assert_eq!(scalars.len(), NUM_FEATURES + 6);
        assert_eq!(staleness.len(), NUM_FEATURES);
        assert_eq!(candles_1m.len(), 60 * 5);
        assert_eq!(
            stored_features.get("orderbook_imbalance").copied(),
            Some(0.12)
        );
        assert_eq!(outcome, None);

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn create_ml_tables_adds_outcome_column_to_legacy_model_input_archive(
    ) -> anyhow::Result<()> {
        let (path, path_str) = temp_sqlite_path("macbot_ml_model_inputs_legacy")?;
        let pool = crate::db::open(&path_str).await?;
        sqlx::query("DROP TABLE IF EXISTS ml_model_inputs")
            .execute(&pool)
            .await?;
        sqlx::query(
            r#"CREATE TABLE ml_model_inputs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp TEXT NOT NULL,
                run_id TEXT NOT NULL,
                market_id TEXT NOT NULL,
                signal TEXT NOT NULL,
                regime_label TEXT NOT NULL,
                phase TEXT NOT NULL,
                fusion_score REAL NOT NULL,
                p_up REAL NOT NULL,
                regime_value REAL NOT NULL,
                features_json TEXT NOT NULL,
                scalars_json TEXT NOT NULL,
                staleness_json TEXT NOT NULL,
                candles_1m_json TEXT NOT NULL,
                candles_5m_json TEXT NOT NULL,
                candles_15m_json TEXT NOT NULL,
                orderbook_seq_json TEXT NOT NULL
            )"#,
        )
        .execute(&pool)
        .await?;

        create_ml_tables(&pool).await?;

        assert!(table_has_column(&pool, "ml_model_inputs", "outcome").await?);

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn candle_logging_ignores_duplicate_timestamp_market_pairs() -> anyhow::Result<()> {
        let (path, path_str) = temp_sqlite_path("macbot_ml_candle_dedupe")?;
        let pool = crate::db::open(&path_str).await?;
        let logger = DataLogger::new(pool.clone());

        let timestamp = Utc::now().to_rfc3339();
        for _ in 0..2 {
            logger.log_candle_at(
                timestamp.clone(),
                1.0,
                2.0,
                0.5,
                1.5,
                100.0,
                Some(""),
                Some(84_000.0),
            );
        }

        tokio::time::sleep(Duration::from_millis(75)).await;
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ml_candles_1m")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 1);

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn dropped_events_are_counted_when_channel_is_full() -> anyhow::Result<()> {
        DataLogger::reset_dropped_events_for_tests();
        let (tx, _rx) = mpsc::channel::<MlLogEvent>(1);
        let logger = DataLogger::with_sender(tx);

        logger.log(MlLogEvent::Prune);
        assert_eq!(DataLogger::dropped_events_total(), 0);

        logger.log(MlLogEvent::Prune);
        assert_eq!(DataLogger::dropped_events_total(), 1);

        DataLogger::reset_dropped_events_for_tests();
        Ok(())
    }
}
