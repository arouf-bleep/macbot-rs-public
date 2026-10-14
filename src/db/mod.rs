use crate::domain::{BotMode, Direction, TradeStatus};
use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow},
    Row, SqlitePool,
};
use std::path::Path;

pub mod live;
pub mod paper;
pub mod redemption;
pub mod risk;

const DB_SCHEMA_VERSION: i64 = 8;

macro_rules! impl_sqlite_from_row {
    ($type:ty { $($field:ident),+ $(,)? }) => {
        impl<'r> sqlx::FromRow<'r, SqliteRow> for $type {
            fn from_row(row: &'r SqliteRow) -> sqlx::Result<Self> {
                Ok(Self {
                    $(
                        $field: row.try_get(stringify!($field))?,
                    )+
                })
            }
        }
};
}

pub mod maker;

/// A single trade record — SQLite schema row.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Trade {
    pub id: i64,
    pub timestamp: String,
    pub market_id: String,
    pub question: String,
    pub direction: String,
    pub entry_price: f64,
    pub amount: f64,
    pub shares: f64,
    pub expected_value: f64,
    pub reason: String,
    pub confidence: f64,
    pub status: String,
    pub mode: String,
    pub source: String,
    pub end_date: String,
    pub exit_price: Option<f64>,
    pub exit_timestamp: Option<String>,
    pub pnl: Option<f64>,
    pub exit_type: Option<String>,
    pub was_correct: Option<i64>,
    pub forced: Option<i64>,
    pub partial_closed: Option<i64>,
    pub raw_confidence: Option<f64>,
    pub order_id: Option<String>,
    pub token_id: Option<String>,
    pub fok_failed: Option<i64>,
    /// Entry fee disclosure: modeled for paper, receipt-derived for managed live.
    #[serde(default)]
    pub fee_paid: f64,
    /// 0: legacy fee-exclusive P&L; 1: settled P&L already includes cash fees.
    #[serde(default)]
    pub accounting_version: i64,
    /// JSON-encoded signal breakdown at trade open for per-signal accuracy tracking.
    pub signals_json: Option<String>,
    /// Stable fingerprint of model/calibration settings used at trade open.
    pub calibration_version: Option<String>,
    /// Runtime/deployment run identifier for per-run attribution.
    pub run_id: Option<String>,
    /// Trade strategy: "conviction" (hold to resolution) or "scalp" (auto TP/SL).
    #[serde(default = "default_strategy")]
    pub strategy: String,
}

fn default_strategy() -> String {
    "conviction".to_string()
}

/// Lightweight analysis snapshot captured each analysis cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisSnapshot {
    pub timestamp: String,
    pub run_id: String,
    pub market_id: String,
    pub question: String,
    pub signal: String,
    pub confidence: f64,
    pub net_ev: f64,
    pub p_up: Option<f64>,
    pub price: f64,
    pub liquidity: f64,
    pub regime: String,
    pub phase: String,
    pub time_remaining_minutes: f64,
    pub btc_change_pct: Option<f64>,
    pub reason: String,
    pub end_date: String,
    pub signals_json: Option<String>,
    pub calibration_version: Option<String>,
    pub source: Option<String>,
}

impl Trade {
    const ORDER_NOTIONAL_TOLERANCE_USD: f64 = 0.01;

    pub fn direction_kind(&self) -> Direction {
        Direction::parse(&self.direction)
    }
    pub fn status_kind(&self) -> TradeStatus {
        TradeStatus::parse(&self.status)
    }
    pub fn mode_kind(&self) -> BotMode {
        BotMode::parse(&self.mode)
    }
    pub fn set_direction(&mut self, d: Direction) {
        self.direction = d.as_str().to_string();
    }
    pub fn set_status(&mut self, s: TradeStatus) {
        self.status = s.as_str().to_string();
    }
    pub fn set_mode(&mut self, m: BotMode) {
        self.mode = m.as_str().to_string();
    }
    pub fn is_open(&self) -> bool {
        self.status_kind() == TradeStatus::Open
    }
    pub fn has_unsettled_exposure(&self) -> bool {
        matches!(self.status_kind(), TradeStatus::Open | TradeStatus::Closing)
    }
    pub fn is_live(&self) -> bool {
        self.mode_kind() == BotMode::Live
    }
    pub fn has_order(&self) -> bool {
        !self.order_id.as_deref().unwrap_or("").is_empty()
    }

    /// Confirmed live accounting stores net realized P&L. Paper accounting
    /// charges entry fees up front and keeps them separate from row P&L.
    pub fn net_realized_pnl(&self) -> f64 {
        self.pnl.unwrap_or(0.0)
            - if self.accounting_version == 1 {
                0.0
            } else {
                self.fee_paid
            }
    }

    pub fn entry_order_inactive(&self, matched_size: f64, avg_price: f64, status: &str) -> bool {
        if matched_size <= 1e-9 {
            return false;
        }
        let status = status.trim().to_ascii_uppercase();
        if status.contains("FILLED")
            || status.contains("MATCHED")
            || status.contains("COMPLETED")
            || status.contains("CANCEL")
            || status.contains("EXPIRED")
        {
            return true;
        }
        let fill_price = if avg_price > 0.0 {
            avg_price
        } else {
            self.entry_price.max(0.0)
        };
        (matched_size * fill_price) + Self::ORDER_NOTIONAL_TOLERANCE_USD >= self.amount.max(0.0)
    }

    pub fn locked_collateral(&self) -> f64 {
        if self.is_open() && self.is_live() && self.has_order() {
            self.amount
        } else {
            0.0
        }
    }
}

/// A persisted fill snapshot for a trade/order lifecycle.
///
/// `cumulative_size` and `avg_price` mirror exchange-reported cumulative fill
/// state for the order at observation time.
/// `delta_*` stores the derived incremental fill since the previous snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeFill {
    pub id: i64,
    pub timestamp: String,
    pub trade_id: i64,
    pub market_id: String,
    pub order_id: String,
    pub phase: String, // "entry" | "exit"
    pub intended_price: f64,
    pub avg_price: f64,
    pub cumulative_size: f64,
    pub delta_size: f64,
    pub delta_price: f64,
    pub source: String,
    pub details: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TradeFillSummary {
    pub entry_filled_size: f64,
    pub entry_vwap: Option<f64>,
    pub entry_intended_vwap: Option<f64>,
    pub entry_slippage_bps: Option<f64>, // positive = adverse
    pub exit_filled_size: f64,
    pub exit_vwap: Option<f64>,
    pub exit_intended_vwap: Option<f64>,
    pub exit_slippage_bps: Option<f64>,      // positive = adverse
    pub roundtrip_edge: Option<f64>,         // exit_vwap - entry_vwap
    pub roundtrip_slippage_bps: Option<f64>, // entry adverse + exit adverse
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TradeFillParity {
    pub max_entry_drift_bps: f64,
    pub max_exit_drift_bps: f64,
    pub entry_reference_price: Option<f64>,
    pub entry_exchange_vwap: Option<f64>,
    pub entry_drift_bps: Option<f64>,
    pub entry_status: String, // "match" | "mismatch" | "missing_reference" | "missing_exchange_fill"
    pub exit_reference_price: Option<f64>,
    pub exit_exchange_vwap: Option<f64>,
    pub exit_drift_bps: Option<f64>,
    pub exit_status: String, // "match" | "mismatch" | "missing_reference" | "missing_exchange_fill"
    pub overall_status: String, // "ok" | "mismatch" | "incomplete"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceReconstructionComponents {
    pub closed_non_live_delta: f64,
    pub open_non_live_locked: f64,
    pub open_non_live_realized: f64,
    pub open_live_locked: f64,
    pub open_non_live_count: u64,
    pub open_live_count: u64,
}

#[derive(Debug, Clone)]
pub struct NonLiveBalanceReplayRow {
    pub status: String,
    pub amount: f64,
    pub fee_paid: f64,
    pub pnl: Option<f64>,
}

impl_sqlite_from_row!(Trade {
    id,
    timestamp,
    market_id,
    question,
    direction,
    entry_price,
    amount,
    shares,
    expected_value,
    reason,
    confidence,
    status,
    mode,
    source,
    end_date,
    exit_price,
    exit_timestamp,
    pnl,
    exit_type,
    was_correct,
    forced,
    partial_closed,
    raw_confidence,
    order_id,
    token_id,
    fok_failed,
    fee_paid,
    accounting_version,
    signals_json,
    calibration_version,
    run_id,
    strategy
});

impl_sqlite_from_row!(AnalysisSnapshot {
    timestamp,
    run_id,
    market_id,
    question,
    signal,
    confidence,
    net_ev,
    p_up,
    price,
    liquidity,
    regime,
    phase,
    time_remaining_minutes,
    btc_change_pct,
    reason,
    end_date,
    signals_json,
    calibration_version,
    source
});

impl_sqlite_from_row!(TradeFill {
    id,
    timestamp,
    trade_id,
    market_id,
    order_id,
    phase,
    intended_price,
    avg_price,
    cumulative_size,
    delta_size,
    delta_price,
    source,
    details
});

impl_sqlite_from_row!(NonLiveBalanceReplayRow {
    status,
    amount,
    fee_paid,
    pnl
});

/// Open the SQLite connection pool and run schema migrations.
pub async fn open(db_path: &str) -> Result<SqlitePool> {
    // File paths are not URIs: '?' and percent escapes must remain literal.
    let options = if db_path == ":memory:" {
        "sqlite::memory:".parse::<SqliteConnectOptions>()?
    } else {
        // SQLite's URI flag still recognizes a relative `file:` prefix even
        // when the driver receives a filename. An absolute OS path cannot be
        // mistaken for that URI scheme, preserving literal ?/%/file: names.
        let path = std::path::Path::new(db_path);
        let filename = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        SqliteConnectOptions::new()
            .filename(filename)
            .create_if_missing(true)
    };
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;
    // The native library can be overridden at build time. Check the library
    // actually serving this connection before migrations enable WAL or change
    // persisted accounting state; the lockfile alone cannot prove its version.
    let runtime_version: String = sqlx::query_scalar("SELECT sqlite_version()")
        .fetch_one(&pool)
        .await?;
    if let Err(error) = validate_sqlite_runtime_version(&runtime_version) {
        pool.close().await;
        return Err(error);
    }
    migrate(&pool).await?;
    Ok(pool)
}

fn validate_sqlite_runtime_version(version: &str) -> Result<()> {
    let mut parts = version.split('.');
    let mut numeric = [0_u32; 3];
    for part in &mut numeric {
        let text = parts
            .next()
            .ok_or_else(|| anyhow!("unrecognized SQLite runtime version: {version}"))?;
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(anyhow!("unrecognized SQLite runtime version: {version}"));
        }
        *part = text
            .parse()
            .map_err(|_| anyhow!("unrecognized SQLite runtime version: {version}"))?;
    }
    if parts.next().is_some() {
        return Err(anyhow!("unrecognized SQLite runtime version: {version}"));
    }
    if numeric < [3, 51, 3] {
        return Err(anyhow!("SQLite 3.51.3 or later is required for the verified WAL-reset corruption fix; actual runtime is {version}. Rebuild with the pinned bundled dependency and remove native-library overrides."));
    }
    Ok(())
}

/// Idempotent schema setup — matches the Python bot's schema exactly.
async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query("PRAGMA journal_mode=WAL").execute(pool).await?;

    let current_version = sqlite_user_version(pool).await?;
    if current_version > DB_SCHEMA_VERSION {
        return Err(anyhow!(
            "database schema is newer than this application; refusing downgrade"
        ));
    }

    migrate_v1_core_schema(pool).await?;
    if current_version < 1 {
        set_sqlite_user_version(pool, 1).await?;
    }

    migrate_v2_trade_extensions(pool).await?;
    if current_version < 2 {
        set_sqlite_user_version(pool, 2).await?;
    }

    if current_version < 3 {
        migrate_v3_trade_accounting(pool).await?;
    }
    if current_version < 4 {
        migrate_v4_live_entry_accounting(pool).await?;
    }
    add_column_if_missing(
        pool,
        "trades",
        "accounting_version",
        "ALTER TABLE trades ADD COLUMN accounting_version INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    maker::migrate(pool).await?;
    live::migrate(pool).await?;
    paper::migrate(pool).await?;
    risk::migrate(pool).await?;
    redemption::migrate(pool).await?;

    // ── ML data collection tables (Phase 2) ─────────────────────────────────
    crate::ml::data_logger::create_ml_tables(pool).await?;

    // Record completion only after all durable control/accounting tables exist.
    set_sqlite_user_version(pool, DB_SCHEMA_VERSION).await?;

    Ok(())
}

async fn migrate_v1_core_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS trades (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp      TEXT    NOT NULL,
            market_id      TEXT    NOT NULL,
            question       TEXT    NOT NULL,
            direction      TEXT    NOT NULL,
            entry_price    REAL    NOT NULL,
            amount         REAL    NOT NULL,
            shares         REAL    NOT NULL,
            expected_value REAL    NOT NULL,
            reason         TEXT    NOT NULL,
            confidence     REAL    NOT NULL,
            status         TEXT    NOT NULL,
            mode           TEXT    NOT NULL,
            source         TEXT    NOT NULL,
            end_date       TEXT    NOT NULL DEFAULT '',
            exit_price     REAL,
            exit_timestamp TEXT,
            pnl            REAL,
            exit_type      TEXT,
            was_correct    INTEGER,
            forced         INTEGER,
            partial_closed INTEGER DEFAULT 0,
            raw_confidence REAL,
            order_id       TEXT,
            token_id       TEXT,
            fok_failed     INTEGER DEFAULT 0,
            calibration_version TEXT,
            run_id TEXT,
            strategy TEXT NOT NULL DEFAULT 'conviction'
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_status ON trades(status)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_trades_status_mode_id ON trades(status, mode, id)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_trades_closed_exit_ts ON trades(status, exit_timestamp)",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_trades_closed_ts ON trades(status, timestamp)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_trades_market_id ON trades(market_id)")
        .execute(pool)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS order_outbox (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp  TEXT NOT NULL,
            stage      TEXT NOT NULL,
            market_id  TEXT NOT NULL,
            order_id   TEXT,
            token_id   TEXT,
            action     TEXT NOT NULL,
            status     TEXT NOT NULL,
            details    TEXT
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_order_outbox_ts ON order_outbox(timestamp)")
        .execute(pool)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS idempotency_cache (
            key             TEXT PRIMARY KEY,
            payload         TEXT NOT NULL,
            created_at_unix INTEGER NOT NULL
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_idempotency_created_at ON idempotency_cache(created_at_unix)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS trade_fills (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp       TEXT NOT NULL,
            trade_id        INTEGER NOT NULL,
            market_id       TEXT NOT NULL,
            order_id        TEXT NOT NULL,
            phase           TEXT NOT NULL,
            intended_price  REAL NOT NULL,
            avg_price       REAL NOT NULL,
            cumulative_size REAL NOT NULL,
            delta_size      REAL NOT NULL,
            delta_price     REAL NOT NULL,
            source          TEXT NOT NULL,
            details         TEXT NOT NULL DEFAULT '',
            UNIQUE(trade_id, order_id, phase, cumulative_size, avg_price)
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_trade_fills_trade_id ON trade_fills(trade_id)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_trade_fills_order ON trade_fills(order_id)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_trade_fills_phase ON trade_fills(trade_id, phase)")
        .execute(pool)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS analysis_snapshots (
            id                     INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp              TEXT NOT NULL,
            run_id                 TEXT NOT NULL,
            market_id              TEXT NOT NULL,
            question               TEXT NOT NULL,
            signal                 TEXT NOT NULL,
            confidence             REAL NOT NULL,
            net_ev                 REAL NOT NULL,
            p_up                   REAL,
            price                  REAL NOT NULL,
            liquidity              REAL NOT NULL,
            regime                 TEXT NOT NULL,
            phase                  TEXT NOT NULL,
            time_remaining_minutes REAL NOT NULL,
            btc_change_pct         REAL,
            reason                 TEXT NOT NULL,
            end_date               TEXT NOT NULL,
            signals_json           TEXT,
            calibration_version    TEXT,
            source                 TEXT
        )"#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_analysis_snapshots_ts ON analysis_snapshots(timestamp)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_analysis_snapshots_market ON analysis_snapshots(market_id)",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS trade_rejections (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp        TEXT    NOT NULL,
            market_id        TEXT    NOT NULL,
            signal           TEXT    NOT NULL,
            confidence       REAL    NOT NULL,
            net_ev           REAL    NOT NULL,
            rejection_reason TEXT    NOT NULL,
            strategy         TEXT    NOT NULL DEFAULT 'conviction',
            run_id           TEXT
        )"#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_trade_rejections_ts ON trade_rejections(timestamp)",
    )
    .execute(pool)
    .await?;

    Ok(())
}

async fn migrate_v2_trade_extensions(pool: &SqlitePool) -> Result<()> {
    add_column_if_missing(
        pool,
        "trades",
        "fee_paid",
        "ALTER TABLE trades ADD COLUMN fee_paid REAL DEFAULT 0",
    )
    .await?;
    add_column_if_missing(
        pool,
        "trades",
        "signals_json",
        "ALTER TABLE trades ADD COLUMN signals_json TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "trades",
        "calibration_version",
        "ALTER TABLE trades ADD COLUMN calibration_version TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "trades",
        "run_id",
        "ALTER TABLE trades ADD COLUMN run_id TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "trades",
        "strategy",
        "ALTER TABLE trades ADD COLUMN strategy TEXT NOT NULL DEFAULT 'conviction'",
    )
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_trades_market_id ON trades(market_id)")
        .execute(pool)
        .await?;
    Ok(())
}

/// Give a trade row a durable record of what it cost, and stop the table
/// reissuing ids that fills still point at.
///
/// Two defects, both of which only bite on databases created before this
/// migration ran:
///
/// 1. `migrate_v1_core_schema` declares `id INTEGER PRIMARY KEY AUTOINCREMENT`,
///    but `CREATE TABLE IF NOT EXISTS` never upgrades a table that already
///    exists. Older files carry a plain `INTEGER PRIMARY KEY`, which is a rowid
///    alias, and SQLite reissues those after a delete. `clear_history` deletes
///    trades while leaving `trade_fills` behind, so a reissued id quietly
///    adopts a deleted trade's fills and every per-trade fill sum is wrong.
///
/// 2. A partial close rewrites `amount` and `shares` to the residual while
///    `pnl` keeps accumulating across the whole position. The row ends up
///    holding two different scopes with nothing to tell them apart, and what
///    was actually staked is gone. `entry_shares` and `entry_cost` are written
///    from entries and cannot be reduced by exits. Schema v4 also accounts
///    for live entries that fill incrementally after the row is created.
async fn migrate_v3_trade_accounting(pool: &SqlitePool) -> Result<()> {
    add_column_if_missing(
        pool,
        "trades",
        "entry_shares",
        "ALTER TABLE trades ADD COLUMN entry_shares REAL",
    )
    .await?;
    add_column_if_missing(
        pool,
        "trades",
        "entry_cost",
        "ALTER TABLE trades ADD COLUMN entry_cost REAL",
    )
    .await?;
    rebuild_trades_with_monotonic_ids(pool).await?;
    backfill_trade_entry_accounting(pool).await?;
    Ok(())
}

/// Rebuild `trades` so its id is monotonic, preserving every column the live
/// schema happens to carry.
///
/// The replacement table is derived from the stored DDL rather than a literal,
/// so columns added by any earlier migration survive without this function
/// needing to know about them.
async fn rebuild_trades_with_monotonic_ids(pool: &SqlitePool) -> Result<()> {
    const PK: &str = "INTEGER PRIMARY KEY";

    let stored_ddl: Option<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'trades'",
    )
    .fetch_optional(pool)
    .await?;
    let Some(sql) = stored_ddl else {
        return Ok(());
    };

    if sql.to_ascii_uppercase().contains("AUTOINCREMENT") {
        return Ok(());
    }

    let Some(open) = sql.find('(') else {
        return Err(anyhow!("trades DDL carries no column list: {sql}"));
    };
    let body = &sql[open..];
    // DDL is ASCII, so uppercasing preserves byte offsets.
    let Some(pk_at) = body.to_ascii_uppercase().find(PK) else {
        return Err(anyhow!("trades DDL has no {PK} to upgrade: {sql}"));
    };
    let split = pk_at + PK.len();
    let rebuilt_body = format!("{} AUTOINCREMENT{}", &body[..split], &body[split..]);

    // Dropping a table drops its indexes with it, so replay them afterwards.
    let indexes: Vec<String> = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master \
         WHERE type = 'index' AND tbl_name = 'trades' AND sql IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;

    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE trades_rebuild_v3 {rebuilt_body}"
    )))
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO trades_rebuild_v3 SELECT * FROM trades")
        .execute(&mut *tx)
        .await?;
    sqlx::query("DROP TABLE trades").execute(&mut *tx).await?;
    sqlx::query("ALTER TABLE trades_rebuild_v3 RENAME TO trades")
        .execute(&mut *tx)
        .await?;
    // Schema DDL comes from this database, not request-supplied SQL.
    for index in indexes {
        sqlx::query(sqlx::AssertSqlSafe(index))
            .execute(&mut *tx)
            .await?;
    }

    // Start the sequence past every id `trade_fills` still references, so a new
    // trade cannot inherit fills left behind by a cleared one. Orphaned fills
    // stay put — they are the only surviving record of what those trades did,
    // and the backfill below reads them.
    let high_water: i64 = sqlx::query_scalar(
        "SELECT MAX(seen) FROM ( \
             SELECT COALESCE(MAX(id), 0) AS seen FROM trades \
             UNION ALL \
             SELECT COALESCE(MAX(trade_id), 0) FROM trade_fills)",
    )
    .fetch_one(&mut *tx)
    .await?;
    // `sqlite_sequence` is declared `(name, seq)` with no unique constraint, so
    // INSERT OR REPLACE appends a second row rather than replacing the first —
    // and SQLite reads the first. Update in place, and only insert when the row
    // genuinely does not exist yet.
    let raised =
        sqlx::query("UPDATE sqlite_sequence SET seq = ? WHERE name = 'trades' AND seq < ?")
            .bind(high_water)
            .bind(high_water)
            .execute(&mut *tx)
            .await?;
    if raised.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO sqlite_sequence(name, seq) SELECT 'trades', ? \
             WHERE NOT EXISTS (SELECT 1 FROM sqlite_sequence WHERE name = 'trades')",
        )
        .bind(high_water)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    tracing::info!(
        "Schema v3: rebuilt trades with a monotonic id, sequence seeded at {high_water}"
    );
    Ok(())
}

/// Recover what each existing position actually cost.
///
/// A row that was never partially closed still holds its original figures in
/// `amount`/`shares`, so it backfills directly. A partially closed row does
/// not, and the only surviving record is `trade_fills` — which on a database
/// that reissued ids may also hold fills from deleted trades sharing the id.
/// Those are excluded by the trade's own lifetime: a fill recorded before the
/// trade opened, or after it closed, cannot belong to it.
async fn backfill_trade_entry_accounting(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        "UPDATE trades \
            SET entry_shares = shares, entry_cost = amount \
          WHERE entry_shares IS NULL AND COALESCE(partial_closed, 0) = 0",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        "UPDATE trades \
            SET entry_shares = ( \
                    SELECT SUM(f.delta_size) FROM trade_fills f \
                     WHERE f.trade_id = trades.id AND f.phase = 'entry' \
                       AND f.timestamp >= trades.timestamp \
                       AND (trades.exit_timestamp IS NULL \
                            OR f.timestamp <= trades.exit_timestamp)), \
                entry_cost = ( \
                    SELECT SUM(f.delta_size * f.delta_price) FROM trade_fills f \
                     WHERE f.trade_id = trades.id AND f.phase = 'entry' \
                       AND f.timestamp >= trades.timestamp \
                       AND (trades.exit_timestamp IS NULL \
                            OR f.timestamp <= trades.exit_timestamp)) \
          WHERE entry_shares IS NULL",
    )
    .execute(pool)
    .await?;

    // A row with no usable fills falls back to the residual. That figure is
    // wrong for a partial close, but leaving NULL would read as "no data" in
    // the places that need a number, which is the worse of the two failures.
    sqlx::query(
        "UPDATE trades \
            SET entry_shares = shares, entry_cost = amount \
          WHERE entry_shares IS NULL OR entry_cost IS NULL \
             OR entry_shares <= 0 OR entry_cost <= 0",
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Live order submission reserves a budget; it does not prove that budget
/// was spent. Recover actual entry cost from the ledger, or from the filled
/// shares for an unpartially-closed row with no ledger evidence.
async fn migrate_v4_live_entry_accounting(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        "UPDATE trades SET entry_shares = MAX(shares, 0), \
                           entry_cost = MAX(shares, 0) * entry_price \
          WHERE LOWER(mode) = 'live' AND COALESCE(partial_closed, 0) = 0",
    )
    .execute(&mut *tx)
    .await?;
    refresh_live_entry_accounting(&mut tx, None).await?;
    tx.commit().await?;
    Ok(())
}

/// Recompute from idempotent entry deltas inside the same transaction that
/// appends them. A historically incomplete ledger must not reduce entry
/// quantities already established by the trade row or earlier snapshots.
async fn refresh_live_entry_accounting(
    conn: &mut sqlx::SqliteConnection,
    trade_id: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "WITH entry_totals AS ( \
             SELECT t.id, SUM(f.delta_size) AS shares, \
                    SUM(f.delta_size * f.delta_price) AS cost \
               FROM trades t JOIN trade_fills f ON f.trade_id = t.id \
              WHERE LOWER(t.mode) = 'live' AND f.phase = 'entry' \
                AND (?1 IS NULL OR t.id = ?1) AND f.market_id = t.market_id \
                AND f.timestamp >= t.timestamp \
                AND (t.exit_timestamp IS NULL OR f.timestamp <= t.exit_timestamp) \
              GROUP BY t.id) \
         UPDATE trades \
            SET entry_shares = (SELECT shares FROM entry_totals WHERE id = trades.id), \
                entry_cost = (SELECT cost FROM entry_totals WHERE id = trades.id) \
          WHERE id IN (SELECT id FROM entry_totals \
                        WHERE shares + 1e-9 >= MAX( \
                            COALESCE(trades.entry_shares, 0), trades.shares))",
    )
    .bind(trade_id)
    .execute(conn)
    .await?;
    Ok(())
}

async fn sqlite_user_version(pool: &SqlitePool) -> Result<i64> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    Ok(version)
}

async fn set_sqlite_user_version(pool: &SqlitePool, version: i64) -> Result<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "PRAGMA user_version = {}",
        version.max(0)
    )))
    .execute(pool)
    .await?;
    Ok(())
}

/// Known tables that are valid targets for PRAGMA / DDL introspection.
const KNOWN_TABLES: &[&str] = &[
    "trades",
    "trade_fills",
    "trade_rejections",
    "analysis_snapshots",
    "order_outbox",
    "idempotency_cache",
    "meta",
    "live_orders",
    "live_matches",
    "maker_orders",
    "maker_matches",
    "maker_match_allocations",
    "maker_market_controls",
    "paper_maker_orders",
    "paper_maker_fill_events",
    "realized_pnl_events",
    "redemption_receipts",
];

fn validate_table_name(table: &str) -> Result<()> {
    if KNOWN_TABLES.contains(&table) {
        Ok(())
    } else {
        Err(anyhow!("unknown table name rejected: {table}"))
    }
}

async fn table_has_column(pool: &SqlitePool, table: &str, column: &str) -> Result<bool> {
    validate_table_name(table)?;
    let pragma = format!("PRAGMA table_info({table})");
    let rows = sqlx::query(sqlx::AssertSqlSafe(pragma))
        .fetch_all(pool)
        .await?;
    Ok(rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("name").ok())
        .any(|name| name == column))
}

async fn add_column_if_missing(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    ddl: &'static str,
) -> Result<bool> {
    if table_has_column(pool, table, column).await? {
        return Ok(false);
    }
    sqlx::query(ddl).execute(pool).await?;
    Ok(true)
}

pub async fn quick_check(pool: &SqlitePool) -> Result<()> {
    let rows: Vec<String> = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_all(pool)
        .await?;
    if rows.iter().all(|row| row.eq_ignore_ascii_case("ok")) {
        Ok(())
    } else {
        Err(anyhow!("sqlite quick_check failed: {}", rows.join(", ")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CriticalTableCounts {
    pub meta: i64,
    pub trades: i64,
    pub order_outbox: i64,
    pub trade_fills: i64,
    pub analysis_snapshots: i64,
    pub idempotency_cache: i64,
    pub live_orders: i64,
    pub live_matches: i64,
    pub maker_orders: i64,
    pub maker_matches: i64,
    pub maker_match_allocations: i64,
    pub maker_market_controls: i64,
    pub paper_maker_orders: i64,
    pub paper_maker_fill_events: i64,
    pub realized_pnl_events: i64,
    pub redemption_receipts: i64,
}

pub async fn critical_table_counts(pool: &SqlitePool) -> Result<CriticalTableCounts> {
    async fn count(pool: &SqlitePool, table: &str) -> Result<i64> {
        validate_table_name(table)?;
        let query = format!("SELECT COUNT(*) FROM {table}");
        Ok(sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(query))
            .fetch_one(pool)
            .await?)
    }

    Ok(CriticalTableCounts {
        meta: count(pool, "meta").await?,
        trades: count(pool, "trades").await?,
        order_outbox: count(pool, "order_outbox").await?,
        trade_fills: count(pool, "trade_fills").await?,
        analysis_snapshots: count(pool, "analysis_snapshots").await?,
        idempotency_cache: count(pool, "idempotency_cache").await?,
        live_orders: count(pool, "live_orders").await?,
        live_matches: count(pool, "live_matches").await?,
        maker_orders: count(pool, "maker_orders").await?,
        maker_matches: count(pool, "maker_matches").await?,
        maker_match_allocations: count(pool, "maker_match_allocations").await?,
        maker_market_controls: count(pool, "maker_market_controls").await?,
        paper_maker_orders: count(pool, "paper_maker_orders").await?,
        paper_maker_fill_events: count(pool, "paper_maker_fill_events").await?,
        realized_pnl_events: count(pool, "realized_pnl_events").await?,
        redemption_receipts: count(pool, "redemption_receipts").await?,
    })
}

pub async fn backup_database(pool: &SqlitePool, backup_path: &str) -> Result<()> {
    let backup_path = backup_path.trim();
    if backup_path.is_empty() {
        return Err(anyhow!("backup path must not be empty"));
    }

    let backup = Path::new(backup_path);
    if backup.is_absolute()
        || backup.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(anyhow!(
            "backup path must be a safe relative path under artifacts/backups/"
        ));
    }
    if !backup.starts_with("artifacts/backups") {
        return Err(anyhow!("backup path must stay under artifacts/backups/"));
    }
    if backup.extension().and_then(|ext| ext.to_str()) != Some("sqlite") {
        return Err(anyhow!("backup path must end with .sqlite"));
    }
    if backup.exists() {
        return Err(anyhow!("backup path already exists: {}", backup.display()));
    }
    if let Some(parent) = backup.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
            let backup_root = std::fs::canonicalize("artifacts/backups")?;
            let parent_canon = std::fs::canonicalize(parent)?;
            if !parent_canon.starts_with(&backup_root) {
                return Err(anyhow!(
                    "backup path must resolve inside artifacts/backups/"
                ));
            }
        }
    }

    // Keep backup names within the supported relative-path character set.
    // The VACUUM destination is bound separately as data, never SQL text.
    if backup_path
        .chars()
        .any(|c| !c.is_alphanumeric() && !matches!(c, '_' | '-' | '.' | '/'))
    {
        return Err(anyhow!(
            "backup path contains disallowed characters: {backup_path}"
        ));
    }
    if pool.options().get_max_connections() < 2 {
        return Err(anyhow!(
            "verified online backup requires at least two database connections"
        ));
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        if Path::new(&format!("{backup_path}{suffix}")).exists() {
            return Err(anyhow!("backup destination has active SQLite sidecars"));
        }
    }
    // Build privately and publish only after verification. Creating the empty
    // staging file ourselves also prevents a backup from inheriting public
    // default permissions on Unix.
    let stage = backup.parent().unwrap_or(Path::new(".")).join(format!(
        ".macbot-backup-{:032x}.sqlite",
        rand::random::<u128>()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    drop(options.open(&stage)?);
    let result = async {
    // Reserve the source writer while a second connection copies it. VACUUM
    // INTO is a consistent read snapshot and does not need a source write.
    // This keeps verification counts stable under concurrent actor writes;
    // checkpointing alone did not provide that guarantee.
    let reservation = pool.begin_with("BEGIN IMMEDIATE").await?;
    quick_check(pool).await?;
    let source_counts = critical_table_counts(pool).await?;
    sqlx::query("VACUUM INTO ?")
        .bind(stage.to_str().ok_or_else(|| anyhow!("backup path must be UTF-8"))?)
        .execute(pool)
        .await?;

    let backup_pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(SqliteConnectOptions::new().filename(&stage).read_only(true))
        .await?;
    quick_check(&backup_pool).await?;

    let backup_version = sqlite_user_version(&backup_pool).await?;
    if backup_version != DB_SCHEMA_VERSION {
        return Err(anyhow!(
            "backup schema version mismatch: expected {}, got {}",
            DB_SCHEMA_VERSION,
            backup_version
        ));
    }

    let backup_counts = critical_table_counts(&backup_pool).await?;
    if backup_counts != source_counts {
        return Err(anyhow!(
            "backup critical table counts mismatch: source={source_counts:?} backup={backup_counts:?}"
        ));
    }

    backup_pool.close().await;
    reservation.rollback().await?;
    std::fs::File::open(&stage)?.sync_all()?;
    // hard_link is an atomic no-replace publication, even if another caller
    // creates the requested destination after the initial existence check.
    std::fs::hard_link(&stage, backup)?;
    #[cfg(unix)]
    std::fs::File::open(backup.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    Ok::<(), anyhow::Error>(())
    }.await;
    let _ = std::fs::remove_file(&stage);
    result
}

// ── CRUD helpers ──────────────────────────────────────────────────────────────

pub async fn insert_trade(pool: &SqlitePool, t: &Trade) -> Result<i64> {
    let entry_cost = if t.is_live() {
        t.shares * t.entry_price
    } else {
        t.amount
    };
    if t.id > 0 {
        sqlx::query(
            r#"INSERT INTO trades
               (id,timestamp,market_id,question,direction,entry_price,amount,shares,
                expected_value,reason,confidence,status,mode,source,end_date,
                exit_price,exit_timestamp,pnl,exit_type,was_correct,forced,
                partial_closed,raw_confidence,order_id,token_id,fok_failed,
                fee_paid,accounting_version,signals_json,calibration_version,run_id,strategy,
                entry_shares,entry_cost)
               VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
        )
        .bind(t.id)
        .bind(&t.timestamp)
        .bind(&t.market_id)
        .bind(&t.question)
        .bind(&t.direction)
        .bind(t.entry_price)
        .bind(t.amount)
        .bind(t.shares)
        .bind(t.expected_value)
        .bind(&t.reason)
        .bind(t.confidence)
        .bind(&t.status)
        .bind(&t.mode)
        .bind(&t.source)
        .bind(&t.end_date)
        .bind(t.exit_price)
        .bind(&t.exit_timestamp)
        .bind(t.pnl)
        .bind(&t.exit_type)
        .bind(t.was_correct)
        .bind(t.forced)
        .bind(t.partial_closed)
        .bind(t.raw_confidence)
        .bind(&t.order_id)
        .bind(&t.token_id)
        .bind(t.fok_failed)
        .bind(t.fee_paid)
        .bind(t.accounting_version)
        .bind(&t.signals_json)
        .bind(&t.calibration_version)
        .bind(&t.run_id)
        .bind(&t.strategy)
        // Residual updates do not touch these columns. Further live entry
        // fills update them atomically with the fill ledger.
        .bind(t.shares)
        .bind(entry_cost)
        .execute(pool)
        .await?;
        return Ok(t.id);
    }

    let res = sqlx::query(
        r#"INSERT INTO trades
           (timestamp,market_id,question,direction,entry_price,amount,shares,
            expected_value,reason,confidence,status,mode,source,end_date,
            exit_price,exit_timestamp,pnl,exit_type,was_correct,forced,
            partial_closed,raw_confidence,order_id,token_id,fok_failed,
            fee_paid,accounting_version,signals_json,calibration_version,run_id,strategy,
            entry_shares,entry_cost)
           VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)"#,
    )
    .bind(&t.timestamp)
    .bind(&t.market_id)
    .bind(&t.question)
    .bind(&t.direction)
    .bind(t.entry_price)
    .bind(t.amount)
    .bind(t.shares)
    .bind(t.expected_value)
    .bind(&t.reason)
    .bind(t.confidence)
    .bind(&t.status)
    .bind(&t.mode)
    .bind(&t.source)
    .bind(&t.end_date)
    .bind(t.exit_price)
    .bind(&t.exit_timestamp)
    .bind(t.pnl)
    .bind(&t.exit_type)
    .bind(t.was_correct)
    .bind(t.forced)
    .bind(t.partial_closed)
    .bind(t.raw_confidence)
    .bind(&t.order_id)
    .bind(&t.token_id)
    .bind(t.fok_failed)
    .bind(t.fee_paid)
    .bind(t.accounting_version)
    .bind(&t.signals_json)
    .bind(&t.calibration_version)
    .bind(&t.run_id)
    .bind(&t.strategy)
    // See the explicit-id branch above: these two are the entry record.
    .bind(t.shares)
    .bind(entry_cost)
    .execute(pool)
    .await?;

    Ok(res.last_insert_rowid())
}

pub async fn insert_analysis_snapshot(pool: &SqlitePool, s: &AnalysisSnapshot) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO analysis_snapshots (
            timestamp, run_id, market_id, question, signal, confidence, net_ev, p_up,
            price, liquidity, regime, phase, time_remaining_minutes, btc_change_pct,
            reason, end_date, signals_json, calibration_version, source
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(&s.timestamp)
    .bind(&s.run_id)
    .bind(&s.market_id)
    .bind(&s.question)
    .bind(&s.signal)
    .bind(s.confidence)
    .bind(s.net_ev)
    .bind(s.p_up)
    .bind(s.price)
    .bind(s.liquidity)
    .bind(&s.regime)
    .bind(&s.phase)
    .bind(s.time_remaining_minutes)
    .bind(s.btc_change_pct)
    .bind(&s.reason)
    .bind(&s.end_date)
    .bind(&s.signals_json)
    .bind(&s.calibration_version)
    .bind(&s.source)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_trade(pool: &SqlitePool, t: &Trade) -> Result<()> {
    // A venue fill can be observed even when its ledger append fails. Keep
    // that observed cumulative entry before the final poll clears order_id.
    // Residual exits cannot shrink the entry, and paper entries stay fixed.
    let can_update_entry = t.is_live() && t.partial_closed.unwrap_or(0) == 0;
    sqlx::query(
        r#"UPDATE trades SET
            timestamp=?,market_id=?,question=?,direction=?,entry_price=?,
            amount=?,shares=?,expected_value=?,reason=?,confidence=?,
            status=?,mode=?,source=?,end_date=?,exit_price=?,exit_timestamp=?,
            pnl=?,exit_type=?,was_correct=?,forced=?,partial_closed=?,
            raw_confidence=?,order_id=?,token_id=?,fok_failed=?,
            fee_paid=?,accounting_version=?,signals_json=?,calibration_version=?,run_id=?,
            entry_cost=CASE WHEN ? AND ? > COALESCE(entry_shares,0) + 1e-9
                            THEN ? ELSE entry_cost END,
            entry_shares=CASE WHEN ? AND ? > COALESCE(entry_shares,0) + 1e-9
                              THEN ? ELSE entry_shares END
           WHERE id=?"#,
    )
    .bind(&t.timestamp)
    .bind(&t.market_id)
    .bind(&t.question)
    .bind(&t.direction)
    .bind(t.entry_price)
    .bind(t.amount)
    .bind(t.shares)
    .bind(t.expected_value)
    .bind(&t.reason)
    .bind(t.confidence)
    .bind(&t.status)
    .bind(&t.mode)
    .bind(&t.source)
    .bind(&t.end_date)
    .bind(t.exit_price)
    .bind(&t.exit_timestamp)
    .bind(t.pnl)
    .bind(&t.exit_type)
    .bind(t.was_correct)
    .bind(t.forced)
    .bind(t.partial_closed)
    .bind(t.raw_confidence)
    .bind(&t.order_id)
    .bind(&t.token_id)
    .bind(t.fok_failed)
    .bind(t.fee_paid)
    .bind(t.accounting_version)
    .bind(&t.signals_json)
    .bind(&t.calibration_version)
    .bind(&t.run_id)
    .bind(can_update_entry)
    .bind(t.shares)
    .bind(t.shares * t.entry_price)
    .bind(can_update_entry)
    .bind(t.shares)
    .bind(t.shares)
    .bind(t.id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn load_all_trades(pool: &SqlitePool) -> Result<Vec<Trade>> {
    let rows = sqlx::query_as::<_, Trade>("SELECT * FROM trades ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn load_trades_by_status(pool: &SqlitePool, status: TradeStatus) -> Result<Vec<Trade>> {
    let rows = sqlx::query_as::<_, Trade>("SELECT * FROM trades WHERE status = ? ORDER BY id")
        .bind(status.as_str())
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn load_trade_by_id(pool: &SqlitePool, trade_id: i64) -> Result<Option<Trade>> {
    let row = sqlx::query_as::<_, Trade>("SELECT * FROM trades WHERE id = ? LIMIT 1")
        .bind(trade_id)
        .fetch_optional(pool)
        .await?;
    Ok(row)
}

pub async fn load_recent_trades(pool: &SqlitePool, limit: usize) -> Result<Vec<Trade>> {
    let lim = limit.max(1) as i64;
    let mut rows = sqlx::query_as::<_, Trade>("SELECT * FROM trades ORDER BY id DESC LIMIT ?")
        .bind(lim)
        .fetch_all(pool)
        .await?;
    rows.sort_by_key(|t| t.id);
    Ok(rows)
}

pub async fn load_recent_closed_trades(pool: &SqlitePool, limit: usize) -> Result<Vec<Trade>> {
    let lim = limit.max(1) as i64;
    let mut rows = sqlx::query_as::<_, Trade>(
        "SELECT * FROM trades WHERE status = 'CLOSED' ORDER BY id DESC LIMIT ?",
    )
    .bind(lim)
    .fetch_all(pool)
    .await?;
    rows.sort_by_key(|t| t.id);
    Ok(rows)
}

pub async fn load_closed_trades(pool: &SqlitePool) -> Result<Vec<Trade>> {
    let rows = sqlx::query_as::<_, Trade>("SELECT * FROM trades WHERE status='CLOSED' ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn load_closed_shadow_trades(pool: &SqlitePool) -> Result<Vec<Trade>> {
    let rows = sqlx::query_as::<_, Trade>(
        "SELECT * FROM trades WHERE status='CLOSED' AND (mode='shadow' OR lower(source)='shadow') ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn load_open_trades(pool: &SqlitePool) -> Result<Vec<Trade>> {
    let rows = sqlx::query_as::<_, Trade>("SELECT * FROM trades WHERE status='OPEN' ORDER BY id")
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn load_open_live_trades(pool: &SqlitePool) -> Result<Vec<Trade>> {
    let rows = sqlx::query_as::<_, Trade>(
        "SELECT * FROM trades WHERE status='OPEN' AND mode='live' ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn load_non_live_balance_replay_rows(
    pool: &SqlitePool,
) -> Result<Vec<NonLiveBalanceReplayRow>> {
    let rows = sqlx::query_as::<_, NonLiveBalanceReplayRow>(
        "SELECT status, amount, fee_paid, pnl FROM trades WHERE mode!='live' ORDER BY id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn count_open_trades(pool: &SqlitePool) -> Result<u64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades WHERE status = 'OPEN'")
        .fetch_one(pool)
        .await?;
    Ok(n.max(0) as u64)
}

pub async fn count_closed_trades(pool: &SqlitePool) -> Result<u64> {
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades WHERE status = 'CLOSED'")
        .fetch_one(pool)
        .await?;
    Ok(n.max(0) as u64)
}

pub async fn get_idempotency_entry(pool: &SqlitePool, key: &str) -> Result<Option<(String, i64)>> {
    let row: Option<(String, i64)> = sqlx::query_as(
        "SELECT payload, created_at_unix FROM idempotency_cache WHERE key = ? LIMIT 1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn upsert_idempotency_entry(
    pool: &SqlitePool,
    key: &str,
    payload: &str,
    created_at_unix: i64,
) -> Result<()> {
    sqlx::query(
        r#"INSERT INTO idempotency_cache (key, payload, created_at_unix)
           VALUES (?, ?, ?)
           ON CONFLICT(key) DO UPDATE SET
             payload = excluded.payload,
             created_at_unix = excluded.created_at_unix"#,
    )
    .bind(key)
    .bind(payload)
    .bind(created_at_unix)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn insert_idempotency_entry_if_absent(
    pool: &SqlitePool,
    key: &str,
    payload: &str,
    created_at_unix: i64,
) -> Result<bool> {
    let res = sqlx::query(
        r#"INSERT INTO idempotency_cache (key, payload, created_at_unix)
           VALUES (?, ?, ?)
           ON CONFLICT(key) DO NOTHING"#,
    )
    .bind(key)
    .bind(payload)
    .bind(created_at_unix)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn delete_idempotency_entry(pool: &SqlitePool, key: &str) -> Result<()> {
    sqlx::query("DELETE FROM idempotency_cache WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn prune_idempotency_entries(
    pool: &SqlitePool,
    min_created_at_unix: i64,
    max_entries: usize,
) -> Result<()> {
    // An unfinished claim can represent an order whose response was lost.
    // Time and cache pressure are not evidence that it is safe to replay it.
    // Preserve malformed records too: uncertainty must fail closed.
    const COMPLETED: &str = "CASE WHEN json_valid(payload) THEN COALESCE(json_extract(payload, '$._macbot_state'), '') <> 'pending' ELSE 0 END";
    let mut tx = pool.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "DELETE FROM idempotency_cache WHERE created_at_unix < ? AND ({COMPLETED})"
    )))
    .bind(min_created_at_unix)
    .execute(&mut *tx)
    .await?;

    let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM idempotency_cache WHERE ({COMPLETED})"
    )))
    .fetch_one(&mut *tx)
    .await?;
    let max_entries = i64::try_from(max_entries.max(1)).unwrap_or(i64::MAX);
    let excess = (n - max_entries).max(0);
    if excess > 0 {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            r#"DELETE FROM idempotency_cache
               WHERE key IN (
                 SELECT key
                 FROM idempotency_cache
                 WHERE ({COMPLETED})
                 ORDER BY created_at_unix ASC, key ASC
                 LIMIT ?
               )"#
        )))
        .bind(excess)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn append_order_outbox(
    pool: &SqlitePool,
    stage: &str,
    market_id: &str,
    order_id: Option<&str>,
    token_id: Option<&str>,
    action: &str,
    status: &str,
    details: &str,
) -> Result<i64> {
    let ts = Utc::now().to_rfc3339();
    let res = sqlx::query(
        r#"INSERT INTO order_outbox
           (timestamp,stage,market_id,order_id,token_id,action,status,details)
           VALUES (?,?,?,?,?,?,?,?)"#,
    )
    .bind(ts)
    .bind(stage)
    .bind(market_id)
    .bind(order_id)
    .bind(token_id)
    .bind(action)
    .bind(status)
    .bind(details)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

/// One rejection reason and how often it fired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectionReasonCount {
    pub reason: String,
    pub count: i64,
}

/// Why the bot did not trade, aggregated.
///
/// These rows were written from the start and never read back: there was no
/// query and no route, so the single largest category of bot behaviour was
/// invisible. A strategy that declines most markets is a different object from
/// one that trades them, and the reasons are the evidence for which it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectionSummary {
    pub total: i64,
    pub distinct_reasons: i64,
    pub reasons: Vec<RejectionReasonCount>,
}

/// Aggregate rejection reasons, most frequent first.
///
/// Reasons embed specifics ("only 105s remaining < 120s minimum"), so raw
/// grouping yields hundreds of near-duplicates. The leading clause before the
/// first colon is the stable category, which is what an operator wants counted.
/// Reduce a stored rejection reason to the category a reader can count.
///
/// Reasons embed the specific threshold that fired ("only 105s remaining < 120s
/// minimum", "token price 0.990 outside entry range [0.1, 0.92]"), so grouping
/// the raw text yields hundreds of rows that each occur once and hide the
/// pattern entirely. Two reductions handle every shape the bot writes: the
/// leading clause before a colon, and the removal of the numbers themselves.
fn rejection_category(reason: &str) -> String {
    let head = match reason.split_once(':') {
        Some((prefix, _)) if !prefix.trim().is_empty() => prefix.trim(),
        _ => reason.trim(),
    };

    // Drop bracketed ranges wholesale, then any remaining numeric literal.
    let mut without_brackets = String::with_capacity(head.len());
    let mut depth = 0_usize;
    for ch in head.chars() {
        match ch {
            '[' | '(' => depth += 1,
            ']' | ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => without_brackets.push(ch),
            _ => {}
        }
    }

    let stripped: Vec<&str> = without_brackets
        .split_whitespace()
        .filter(|word| {
            // A word is a literal when it carries a digit. Words like "120s" and
            // "0.92" vary per rejection; words like "conviction" do not.
            !word.chars().any(|c| c.is_ascii_digit())
        })
        .collect();

    let category = stripped.join(" ");
    let trimmed = category.trim_matches(|c: char| c.is_whitespace() || c == ',' || c == '.');
    if trimmed.is_empty() {
        // Nothing but numbers. Keep the original rather than emit a blank row.
        reason.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

pub async fn rejection_summary(pool: &SqlitePool, limit: usize) -> Result<RejectionSummary> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        r#"SELECT rejection_reason, COUNT(*) AS n
           FROM trade_rejections
           GROUP BY rejection_reason"#,
    )
    .fetch_all(pool)
    .await?;

    let mut grouped: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    let mut total = 0_i64;
    for (reason, n) in rows {
        total += n;
        *grouped.entry(rejection_category(&reason)).or_insert(0) += n;
    }

    let distinct_reasons = grouped.len() as i64;
    let mut reasons: Vec<RejectionReasonCount> = grouped
        .into_iter()
        .map(|(reason, count)| RejectionReasonCount { reason, count })
        .collect();
    // Deterministic order: count first, then reason, so equal counts do not
    // shuffle between requests.
    reasons.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    reasons.truncate(limit);

    Ok(RejectionSummary {
        total,
        distinct_reasons,
        reasons,
    })
}

/// One rejection as recorded, with its full reason text. Distinct from
/// [`RejectionReasonCount`], which aggregates: a decision feed needs the
/// specific threshold that fired, not the category.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RejectionRow {
    pub timestamp: String,
    pub market_id: String,
    pub signal: String,
    pub confidence: f64,
    pub net_ev: f64,
    pub rejection_reason: String,
    pub strategy: String,
}

pub async fn recent_rejections(pool: &SqlitePool, limit: usize) -> Result<Vec<RejectionRow>> {
    let rows: Vec<(String, String, String, f64, f64, String, String)> = sqlx::query_as(
        r#"SELECT timestamp, market_id, signal, confidence, net_ev, rejection_reason, strategy
           FROM trade_rejections
           ORDER BY timestamp DESC, id DESC
           LIMIT ?"#,
    )
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(timestamp, market_id, signal, confidence, net_ev, rejection_reason, strategy)| {
                RejectionRow {
                    timestamp,
                    market_id,
                    signal,
                    confidence,
                    net_ev,
                    rejection_reason,
                    strategy,
                }
            },
        )
        .collect())
}

// These fields mirror one durable rejection record and its run provenance.
#[allow(clippy::too_many_arguments)]
pub async fn append_trade_rejection(
    pool: &SqlitePool,
    market_id: &str,
    signal: &str,
    confidence: f64,
    net_ev: f64,
    rejection_reason: &str,
    strategy: &str,
    run_id: Option<&str>,
) -> Result<()> {
    let ts = Utc::now().to_rfc3339();
    sqlx::query(
        r#"INSERT INTO trade_rejections
           (timestamp,market_id,signal,confidence,net_ev,rejection_reason,strategy,run_id)
           VALUES (?,?,?,?,?,?,?,?)"#,
    )
    .bind(ts)
    .bind(market_id)
    .bind(signal)
    .bind(confidence)
    .bind(net_ev)
    .bind(rejection_reason)
    .bind(strategy)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn append_trade_fill_snapshot(
    pool: &SqlitePool,
    trade_id: i64,
    market_id: &str,
    order_id: &str,
    phase: &str,
    intended_price: f64,
    avg_price: f64,
    cumulative_size: f64,
    source: &str,
    details: &str,
) -> Result<Option<i64>> {
    if trade_id <= 0
        || order_id.trim().is_empty()
        || !intended_price.is_finite()
        || !avg_price.is_finite()
        || !cumulative_size.is_finite()
        || intended_price <= 0.0
        || intended_price > 1.0
        || avg_price <= 0.0
        || avg_price > 1.0
        || cumulative_size <= 0.0
    {
        return Ok(None);
    }

    // Serialize the read/delta/write sequence across poll/reconcile callers.
    // A deferred transaction can read the same prior snapshot twice.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let prev: Option<(f64, f64)> = sqlx::query_as(
        r#"SELECT cumulative_size, avg_price
           FROM trade_fills
           WHERE trade_id = ? AND order_id = ? AND phase = ?
           ORDER BY id DESC
           LIMIT 1"#,
    )
    .bind(trade_id)
    .bind(order_id)
    .bind(phase)
    .fetch_optional(&mut *tx)
    .await?;

    let (prev_size, prev_avg) = prev.unwrap_or((0.0, avg_price));
    let delta_size = (cumulative_size - prev_size).max(0.0);
    if delta_size <= 1e-9 {
        tx.commit().await?;
        return Ok(None);
    }
    let prev_notional = prev_size * prev_avg;
    let curr_notional = cumulative_size * avg_price;
    if !curr_notional.is_finite()
        || !prev_notional.is_finite()
        || curr_notional + 1e-9 < prev_notional
    {
        return Err(anyhow!(
            "cumulative fill notional cannot decrease or overflow"
        ));
    }
    let delta_notional = (curr_notional - prev_notional).max(0.0);
    let delta_price = if delta_size > 1e-9 {
        delta_notional / delta_size
    } else {
        avg_price
    };
    if !delta_price.is_finite() || delta_price <= 0.0 || delta_price > 1.0 + 1e-9 {
        return Err(anyhow!(
            "incremental binary fill price must be within (0, 1]"
        ));
    }

    let ts = Utc::now().to_rfc3339();
    let res = sqlx::query(
        r#"INSERT OR IGNORE INTO trade_fills
           (timestamp, trade_id, market_id, order_id, phase, intended_price,
            avg_price, cumulative_size, delta_size, delta_price, source, details)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(ts)
    .bind(trade_id)
    .bind(market_id)
    .bind(order_id)
    .bind(phase)
    .bind(intended_price)
    .bind(avg_price)
    .bind(cumulative_size)
    .bind(delta_size)
    .bind(delta_price)
    .bind(source)
    .bind(details)
    .execute(&mut *tx)
    .await?;

    if phase == "entry" && res.rows_affected() > 0 {
        refresh_live_entry_accounting(&mut tx, Some(trade_id)).await?;
    }
    tx.commit().await?;
    if res.rows_affected() == 0 {
        Ok(None)
    } else {
        Ok(Some(res.last_insert_rowid()))
    }
}

/// How far the stored trade rows can be trusted to add up.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TradeRecordConsistency {
    /// Closed trades the identity can be applied to.
    pub checked: i64,
    /// Of those, how many do not add up.
    pub mismatched: i64,
    /// Ids of the offenders, capped for reporting.
    pub mismatched_ids: Vec<i64>,
    /// Closed trades whose pnl cannot be verified from the row at all.
    pub unverifiable: i64,
    /// Closed trades with no usable entry cost, which breaks return-on-turnover.
    pub missing_entry_cost: i64,
}

/// Check that closed trades add up, and say plainly how many cannot be checked.
///
/// For a trade closed in one go the identity `pnl == shares * (exit - entry)`
/// holds up to fees, and nothing was asserting it: trade 57 in the production
/// database reads as a winning BUY_YES that exited at 0.000, and every field in
/// it is individually correct. The row could not be read, and no check noticed.
///
/// A partially closed trade cannot be checked this way — `shares` and
/// `exit_price` describe the residual while `pnl` describes the whole position
/// — and `trade_fills` does not close the gap, because the exit side is not
/// recorded as a fill for most trades. Those are counted as unverifiable rather
/// than silently passed or noisily failed. Saying "26 of these cannot be
/// verified" is the useful output; pretending otherwise is not.
pub async fn trade_record_consistency(
    pool: &SqlitePool,
    tolerance: f64,
) -> Result<TradeRecordConsistency> {
    // Receipt-managed P&L is net of CASH fees. V1 BUY share fees are already
    // represented by net shares and their effective entry price.
    let expected_rows = "WITH expected AS (SELECT t.*,
        shares * (exit_price - entry_price) - CASE WHEN accounting_version=1 THEN
        COALESCE((SELECT SUM(m.fee) FROM live_matches m JOIN live_orders o
                  ON o.order_id=m.order_id WHERE o.trade_id=t.id),0.0) +
        COALESCE((SELECT SUM(a.fee_usd) FROM maker_match_allocations a WHERE a.trade_id=t.id),0.0)
        ELSE 0.0 END AS implied_pnl FROM trades t
        WHERE status='CLOSED' AND pnl IS NOT NULL AND exit_price IS NOT NULL
          AND COALESCE(partial_closed,0)=0)";
    let (checked, mismatched): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{expected_rows} SELECT COUNT(*),COALESCE(SUM(ABS(pnl-implied_pnl)>?),0) FROM expected"
    )))
    .bind(tolerance)
    .fetch_one(pool)
    .await?;
    let mismatched_ids:Vec<i64>=sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "{expected_rows} SELECT id FROM expected WHERE ABS(pnl-implied_pnl)>? ORDER BY id DESC LIMIT 25")))
        .bind(tolerance).fetch_all(pool).await?;

    let unverifiable: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM trades \
          WHERE status = 'CLOSED' AND pnl IS NOT NULL \
            AND COALESCE(partial_closed, 0) = 1",
    )
    .fetch_one(pool)
    .await?;

    // A row with no stake at all is degenerate but self-consistent. The real
    // inconsistency is a row that staked something and has no record of what.
    let missing_entry_cost: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM trades \
          WHERE status = 'CLOSED' AND amount > 0 \
            AND (entry_cost IS NULL OR entry_cost <= 0)",
    )
    .fetch_one(pool)
    .await?;

    Ok(TradeRecordConsistency {
        checked,
        mismatched,
        mismatched_ids,
        unverifiable,
        missing_entry_cost,
    })
}

/// How many rows each retention sweep removed.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PruneOutcome {
    pub order_outbox: u64,
    pub analysis_snapshots: u64,
    pub trade_rejections: u64,
}

impl PruneOutcome {
    pub fn total(&self) -> u64 {
        self.order_outbox + self.analysis_snapshots + self.trade_rejections
    }
}

/// Drop operational history older than `retain_days`.
///
/// Only `idempotency_cache` had any retention at all, so every other log grew
/// without bound: 271,909 outbox rows and 10,748 analysis snapshots against 66
/// trades, in a 166 MB file on a disk at 98% capacity. These three tables are
/// diagnostics — nothing reads them back beyond a recent window — unlike
/// `trades` and `trade_fills`, which are the record itself and are never
/// touched here.
///
/// `retain_days` of 0 disables the sweep rather than deleting everything: an
/// unset or zeroed config must not be a data-loss event.
pub async fn prune_operational_history(
    pool: &SqlitePool,
    retain_days: u32,
) -> Result<PruneOutcome> {
    if retain_days == 0 {
        return Ok(PruneOutcome::default());
    }
    // Built with chrono rather than SQLite's `datetime()`, which renders
    // `YYYY-MM-DD HH:MM:SS` and would never compare equal to the stored
    // RFC3339 text — a mismatch that silently matches no rows at all.
    let cutoff = (Utc::now() - chrono::Duration::days(i64::from(retain_days))).to_rfc3339();

    let mut outcome = PruneOutcome::default();
    let mut tx = pool.begin().await?;
    for (table, field) in [
        ("order_outbox", &mut outcome.order_outbox),
        ("analysis_snapshots", &mut outcome.analysis_snapshots),
        ("trade_rejections", &mut outcome.trade_rejections),
    ] {
        *field = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE timestamp < ?"
        )))
        .bind(&cutoff)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(outcome)
}

/// Delete every trade that is no longer open, together with its fills.
///
/// Deleting the trade alone leaves its fills pointing at an id that no longer
/// exists. Before schema v3 that id was then reissued to a new trade, which
/// silently adopted the dead trade's fills; v3 stops the reissue, and this
/// stops the orphan being created. Both are needed — an orphan still breaks
/// any join that trusts `trade_id`, and still grows the table forever.
///
/// Returns the number of trades removed.
pub async fn clear_closed_trades(pool: &SqlitePool, _open_status: &str) -> Result<u64> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "DELETE FROM trade_fills WHERE trade_id IN \
         (SELECT id FROM trades WHERE status = 'CLOSED'
            AND id NOT IN (SELECT trade_id FROM live_orders)
            AND id NOT IN (SELECT trade_id FROM maker_match_allocations)
            AND id NOT IN (SELECT trade_id FROM realized_pnl_events)
            AND id NOT IN (SELECT trade_id FROM paper_maker_orders WHERE trade_id IS NOT NULL))",
    )
    .execute(&mut *tx)
    .await?;
    let removed = sqlx::query(
        "DELETE FROM trades WHERE status = 'CLOSED'
            AND id NOT IN (SELECT trade_id FROM live_orders)
            AND id NOT IN (SELECT trade_id FROM maker_match_allocations)
            AND id NOT IN (SELECT trade_id FROM realized_pnl_events)
            AND id NOT IN (SELECT trade_id FROM paper_maker_orders WHERE trade_id IS NOT NULL)",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(removed)
}

/// What the position actually cost, as recorded at entry.
///
/// `trades.amount` is rewritten to the residual by every partial close, so it
/// is the wrong denominator for anything measuring return on turnover: a
/// position that staked a dollar and was two-thirds closed reports fifteen
/// cents. `entry_cost` follows entry fills and is never reduced by exits. Falls back to
/// `amount` only for rows written before the column existed and with no fills
/// to recover from.
pub async fn trade_entry_cost(pool: &SqlitePool, trade_id: i64) -> Result<Option<f64>> {
    let cost: Option<f64> =
        sqlx::query_scalar("SELECT COALESCE(entry_cost, amount) FROM trades WHERE id = ?")
            .bind(trade_id)
            .fetch_optional(pool)
            .await?
            .flatten();
    Ok(cost)
}

pub async fn load_trade_fills(
    pool: &SqlitePool,
    trade_id: i64,
    limit: usize,
) -> Result<Vec<TradeFill>> {
    let rows = sqlx::query_as::<_, TradeFill>(
        r#"SELECT *
           FROM trade_fills
           WHERE trade_id = ?
           ORDER BY id DESC
           LIMIT ?"#,
    )
    .bind(trade_id)
    .bind(limit as i64)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn trade_fill_summary(pool: &SqlitePool, trade_id: i64) -> Result<TradeFillSummary> {
    let entry: (f64, f64, f64) = sqlx::query_as(
        r#"SELECT
               COALESCE(SUM(delta_size), 0.0) as qty,
               COALESCE(SUM(delta_size * delta_price), 0.0) as notional,
               COALESCE(SUM(delta_size * intended_price), 0.0) as intended_notional
           FROM trade_fills
           WHERE trade_id = ? AND phase = 'entry'"#,
    )
    .bind(trade_id)
    .fetch_one(pool)
    .await?;

    let exit: (f64, f64, f64) = sqlx::query_as(
        r#"SELECT
               COALESCE(SUM(delta_size), 0.0) as qty,
               COALESCE(SUM(delta_size * delta_price), 0.0) as notional,
               COALESCE(SUM(delta_size * intended_price), 0.0) as intended_notional
           FROM trade_fills
           WHERE trade_id = ? AND phase IN ('exit', 'partial_exit')"#,
    )
    .bind(trade_id)
    .fetch_one(pool)
    .await?;

    let (entry_qty, entry_notional, entry_intended_notional) = entry;
    let (exit_qty, exit_notional, exit_intended_notional) = exit;

    let entry_vwap = if entry_qty > 0.0 {
        Some(entry_notional / entry_qty)
    } else {
        None
    };
    let entry_intended_vwap = if entry_qty > 0.0 {
        Some(entry_intended_notional / entry_qty)
    } else {
        None
    };
    let exit_vwap = if exit_qty > 0.0 {
        Some(exit_notional / exit_qty)
    } else {
        None
    };
    let exit_intended_vwap = if exit_qty > 0.0 {
        Some(exit_intended_notional / exit_qty)
    } else {
        None
    };

    // Positive means adverse:
    // - Entry buys: paying more than intended is adverse.
    // - Exit sells: receiving less than intended is adverse.
    let entry_slippage_bps = match (entry_vwap, entry_intended_vwap) {
        (Some(actual), Some(intended)) if intended > 0.0 => {
            Some((actual - intended) / intended * 10_000.0)
        }
        _ => None,
    };
    let exit_slippage_bps = match (exit_vwap, exit_intended_vwap) {
        (Some(actual), Some(intended)) if intended > 0.0 => {
            Some((intended - actual) / intended * 10_000.0)
        }
        _ => None,
    };

    let roundtrip_edge = match (entry_vwap, exit_vwap) {
        (Some(e), Some(x)) => Some(x - e),
        _ => None,
    };
    let roundtrip_slippage_bps = match (entry_slippage_bps, exit_slippage_bps) {
        (Some(e), Some(x)) => Some(e + x),
        _ => None,
    };

    Ok(TradeFillSummary {
        entry_filled_size: entry_qty,
        entry_vwap,
        entry_intended_vwap,
        entry_slippage_bps,
        exit_filled_size: exit_qty,
        exit_vwap,
        exit_intended_vwap,
        exit_slippage_bps,
        roundtrip_edge,
        roundtrip_slippage_bps,
    })
}

fn leg_parity_status(
    reference_price: Option<f64>,
    exchange_vwap: Option<f64>,
    max_drift_bps: f64,
) -> (Option<f64>, String) {
    let Some(reference) = reference_price else {
        return (None, "missing_reference".to_string());
    };
    if reference <= 0.0 {
        return (None, "missing_reference".to_string());
    }
    let Some(actual) = exchange_vwap else {
        return (None, "missing_exchange_fill".to_string());
    };
    if actual <= 0.0 {
        return (None, "missing_exchange_fill".to_string());
    }
    let drift_bps = ((actual - reference).abs() / reference) * 10_000.0;
    let status = if drift_bps <= max_drift_bps {
        "match"
    } else {
        "mismatch"
    };
    (Some(drift_bps), status.to_string())
}

pub fn trade_fill_parity(
    trade: &Trade,
    summary: &TradeFillSummary,
    max_entry_drift_bps: f64,
    max_exit_drift_bps: f64,
) -> TradeFillParity {
    let max_entry = max_entry_drift_bps.max(0.0);
    let max_exit = max_exit_drift_bps.max(0.0);

    // V1 share fees make the position's effective cost differ from gross fill
    // VWAP. Partial exits likewise keep only the last sale price in the row.
    // Those are different scopes, not evidence of faulty execution.
    let entry_reference_price =
        if trade.entry_price > 0.0 && !(trade.is_live() && trade.fee_paid > 0.0) {
            Some(trade.entry_price)
        } else {
            None
        };
    let exit_reference_price = if trade.partial_closed == Some(1) {
        None
    } else {
        trade.exit_price
    };

    let (entry_drift_bps, entry_status) =
        leg_parity_status(entry_reference_price, summary.entry_vwap, max_entry);

    // A redemption is a collateral receipt, not a book sale. This pure helper
    // has no receipt proof: the authenticated API verifies it separately.
    // Only an explicit resolution can substitute a payout for exchange fills.
    // An ordinary sale near either endpoint still needs execution evidence.
    let (exit_drift_bps, exit_status) =
        if trade.exit_type.as_deref() == Some("redemption_confirmed") {
            (None, "redemption_requires_receipt".to_string())
        } else {
            match exit_reference_price {
                Some(p)
                    if trade.exit_type.as_deref() == Some("auto_resolved")
                        && (p <= 0.001 || p >= 0.999) =>
                {
                    (None, "resolved".to_string())
                }
                Some(p) => leg_parity_status(Some(p), summary.exit_vwap, max_exit),
                None => (None, "missing_reference".to_string()),
            }
        };

    let is_open = trade.is_open();
    let overall_status = if entry_status == "mismatch" || (!is_open && exit_status == "mismatch") {
        "mismatch".to_string()
    } else if entry_status == "match"
        && (is_open || exit_status == "match" || exit_status == "resolved")
    {
        "ok".to_string()
    } else {
        "incomplete".to_string()
    };

    TradeFillParity {
        max_entry_drift_bps: max_entry,
        max_exit_drift_bps: max_exit,
        entry_reference_price,
        entry_exchange_vwap: summary.entry_vwap,
        entry_drift_bps,
        entry_status,
        exit_reference_price,
        exit_exchange_vwap: summary.exit_vwap,
        exit_drift_bps,
        exit_status,
        overall_status,
    }
}

pub async fn balance_reconstruction_components(
    pool: &SqlitePool,
) -> Result<BalanceReconstructionComponents> {
    let (
        closed_non_live_delta,
        open_non_live_locked,
        open_non_live_realized,
        open_live_locked,
        open_non_live_count,
        open_live_count,
    ): (f64, f64, f64, f64, i64, i64) = sqlx::query_as(
        r#"SELECT
             COALESCE(SUM(CASE WHEN status='CLOSED' AND mode!='live'
                               THEN COALESCE(pnl,0.0)-COALESCE(fee_paid,0.0) ELSE 0.0 END),0.0),
             COALESCE(SUM(CASE WHEN status IN ('OPEN','CLOSING') AND mode!='live'
                               THEN COALESCE(amount,0.0)+COALESCE(fee_paid,0.0) ELSE 0.0 END),0.0),
             COALESCE(SUM(CASE WHEN status IN ('OPEN','CLOSING') AND mode!='live'
                               THEN COALESCE(pnl,0.0) ELSE 0.0 END),0.0),
             COALESCE(SUM(CASE WHEN status IN ('OPEN','CLOSING') AND mode='live'
                               THEN COALESCE(amount,0.0) ELSE 0.0 END),0.0),
             COALESCE(SUM(CASE WHEN status IN ('OPEN','CLOSING') AND mode!='live' THEN 1 ELSE 0 END),0),
             COALESCE(SUM(CASE WHEN status IN ('OPEN','CLOSING') AND mode='live' THEN 1 ELSE 0 END),0)
           FROM trades"#,
    )
    .fetch_one(pool)
    .await?;

    if [
        closed_non_live_delta,
        open_non_live_locked,
        open_non_live_realized,
        open_live_locked,
    ]
    .iter()
    .any(|amount| !amount.is_finite())
    {
        return Err(anyhow!(
            "balance reconstruction has nonfinite accounting values"
        ));
    }
    Ok(BalanceReconstructionComponents {
        closed_non_live_delta,
        open_non_live_locked,
        open_non_live_realized,
        open_live_locked,
        open_non_live_count: open_non_live_count.max(0) as u64,
        open_live_count: open_live_count.max(0) as u64,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn sqlite_native_gate_rejects_old_and_unrecognized_versions() {
        for version in [
            "3.46.0",
            "3.50.7",
            "3.51.2",
            "3.9.99",
            "",
            "3.51",
            "3.51.3.1",
            "3.51.3-preview",
            "+3.51.3",
            "3.9999999999999999999999.3",
        ] {
            assert!(
                super::validate_sqlite_runtime_version(version).is_err(),
                "accepted {version}"
            );
        }
        for version in ["3.51.3", "3.51.4", "3.53.0", "3.54.0", "4.0.0"] {
            assert!(
                super::validate_sqlite_runtime_version(version).is_ok(),
                "rejected {version}"
            );
        }
    }

    #[tokio::test]
    async fn actual_sqlite_connection_uses_the_pinned_fixed_native_runtime() -> anyhow::Result<()> {
        let pool = super::open(":memory:").await?;
        let version: String = sqlx::query_scalar("SELECT sqlite_version()")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            version, "3.51.3",
            "native artifact differs from the pinned bundled source"
        );
        super::validate_sqlite_runtime_version(&version)?;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn bundled_sqlite_preserves_atomic_balances_during_wal_checkpoints() -> anyhow::Result<()>
    {
        // Exercise the application's multi-connection WAL shape. This checks
        // the replacement's integration and transaction invariants; a finite
        // stress run cannot prove the upstream rare race is absent.
        let directory =
            std::env::temp_dir().join(format!("macbot_wal_checkpoint_{}", rand::random::<u64>()));
        std::fs::create_dir(&directory)?;
        let path = directory.join("wal-checkpoint.sqlite");
        let pool = super::open(path.to_str().unwrap()).await?;
        let journal: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&pool)
            .await?;
        assert_eq!(journal.to_lowercase(), "wal");
        sqlx::query(
            "CREATE TABLE checkpoint_balance (id INTEGER PRIMARY KEY, cash INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await?;
        sqlx::query("INSERT INTO checkpoint_balance VALUES (1, 1000), (2, 1000)")
            .execute(&pool)
            .await?;
        sqlx::query(
            "CREATE TABLE checkpoint_transfers (id INTEGER PRIMARY KEY, amount INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await?;
        let mut writers = Vec::new();
        for _ in 0..4 {
            let pool = pool.clone();
            writers.push(tokio::spawn(async move {
                for _ in 0..32 {
                    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                    sqlx::query("UPDATE checkpoint_balance SET cash=cash-1 WHERE id=1")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("UPDATE checkpoint_balance SET cash=cash+1 WHERE id=2")
                        .execute(&mut *tx)
                        .await?;
                    sqlx::query("INSERT INTO checkpoint_transfers(amount) VALUES(1)")
                        .execute(&mut *tx)
                        .await?;
                    tx.commit().await?;
                    tokio::task::yield_now().await;
                }
                anyhow::Ok(())
            }));
        }
        let checkpoint_pool = pool.clone();
        let checkpoint = tokio::spawn(async move {
            for _ in 0..64 {
                // A busy checkpoint is a valid outcome while other connections
                // hold WAL readers/writers; it must never corrupt their ledger.
                let (busy, _, _): (i64, i64, i64) =
                    sqlx::query_as("PRAGMA wal_checkpoint(PASSIVE)")
                        .fetch_one(&checkpoint_pool)
                        .await?;
                assert!(matches!(busy, 0 | 1));
                tokio::task::yield_now().await;
            }
            anyhow::Ok(())
        });
        for writer in writers {
            writer.await??;
        }
        checkpoint.await??;
        let amounts: Vec<i64> =
            sqlx::query_scalar("SELECT cash FROM checkpoint_balance ORDER BY id")
                .fetch_all(&pool)
                .await?;
        assert_eq!(amounts, [872, 1128]);
        let transfers: (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), SUM(amount) FROM checkpoint_transfers")
                .fetch_one(&pool)
                .await?;
        assert_eq!(transfers, (128, 128));
        super::quick_check(&pool).await?;
        pool.close().await;
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[tokio::test]
    async fn rejection_summary_groups_by_category_not_by_exact_text() -> Result<()> {
        // Reasons embed specifics, so raw grouping produces hundreds of
        // near-duplicates that hide the actual pattern.
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_rejsum_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        for reason in [
            "conviction entry blocked: only 105s remaining < 120s minimum",
            "conviction entry blocked: only 89s remaining < 120s minimum",
            "conviction entry blocked: only 12s remaining < 120s minimum",
            "SimLive thin book: spread 0.9800 > max 0.0500",
            "SimLive thin book: spread 0.4200 > max 0.0500",
            "token price 0.990 outside entry range [0.1, 0.92]",
            "token price 0.940 outside entry range [0.1, 0.92]",
        ] {
            append_trade_rejection(
                &pool,
                "mkt",
                "BUY_YES",
                0.7,
                0.01,
                reason,
                "conviction",
                None,
            )
            .await?;
        }

        let summary = rejection_summary(&pool, 20).await?;
        assert_eq!(summary.total, 7, "every rejection must be counted");
        assert_eq!(
            summary.distinct_reasons, 3,
            "seven distinct texts collapse to three categories"
        );
        assert_eq!(summary.reasons[0].reason, "conviction entry blocked");
        assert_eq!(summary.reasons[0].count, 3, "most frequent category first");
        assert_eq!(summary.reasons[1].reason, "SimLive thin book");
        assert_eq!(summary.reasons[1].count, 2);

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[test]
    fn rejection_category_collapses_embedded_thresholds() {
        // Every shape the bot actually writes, taken from the production table.
        assert_eq!(
            rejection_category("conviction entry blocked: only 105s remaining < 120s minimum"),
            "conviction entry blocked"
        );
        assert_eq!(
            rejection_category("SimLive thin book: spread 0.9800 > max 0.0500"),
            "SimLive thin book"
        );
        assert_eq!(
            rejection_category("token price 0.990 outside entry range [0.1, 0.92]"),
            "token price outside entry range",
            "a reason with no colon must still lose its numbers"
        );
        assert_eq!(
            rejection_category("token price 0.940 outside entry range [0.1, 0.92]"),
            rejection_category("token price 0.991 outside entry range [0.1, 0.92]"),
            "the same rule with different numbers is one category"
        );
        assert_eq!(rejection_category("Scalp skipped"), "Scalp skipped");
    }

    #[test]
    fn rejection_category_never_returns_a_blank_row() {
        // A reason made only of numbers must keep its original text rather than
        // collapse every such row into one nameless bucket.
        assert_eq!(rejection_category("0.99 0.92"), "0.99 0.92");
        assert_eq!(rejection_category("   "), "");
    }

    #[tokio::test]
    async fn rejection_summary_is_empty_rather_than_failing_on_a_fresh_database() -> Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_rejempty_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let summary = rejection_summary(&pool, 20).await?;
        assert_eq!(summary.total, 0);
        assert_eq!(summary.distinct_reasons, 0);
        assert!(summary.reasons.is_empty());

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    use super::*;

    fn sample_trade(question: &str) -> Trade {
        Trade {
            id: 0,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            market_id: "mkt-1".to_string(),
            question: question.to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.5,
            amount: 10.0,
            shares: 20.0,
            expected_value: 0.01,
            reason: "test".to_string(),
            confidence: 0.7,
            status: "OPEN".to_string(),
            mode: "simulation".to_string(),
            source: "test".to_string(),
            end_date: "".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: None,
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: Some("test-cal-v1".to_string()),
            run_id: Some("test-run".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    #[tokio::test]
    async fn open_records_current_schema_version() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_schema_version_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();

        let pool = open(&path_str).await?;
        assert_eq!(sqlite_user_version(&pool).await?, DB_SCHEMA_VERSION);

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn quick_check_passes_on_fresh_db() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_quick_check_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();

        let pool = open(&path_str).await?;
        quick_check(&pool).await?;

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn backup_database_roundtrips_critical_tables_and_schema_version() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_backup_src_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let backup =
            Path::new("artifacts/backups").join(format!("macbot_backup_dst_{uniq}.sqlite"));
        let backup_str = backup.to_string_lossy().to_string();

        let pool = open(&path_str).await?;
        let trade_id = insert_trade(&pool, &sample_trade("backup")).await?;
        risk::reset_cash_peak(&pool, "simulation", 80.0).await?;
        risk::observe_cash_peak(&pool, "simulation", 95.0).await?;
        let mut realization = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE trades SET shares=19,amount=9.5,pnl=-0.25,partial_closed=1 WHERE id=?")
            .bind(trade_id)
            .execute(&mut *realization)
            .await?;
        risk::record_realized_delta(
            &mut realization,
            trade_id,
            "backup/partial",
            "2026-01-01T00:00:00Z",
            -0.25,
            1.0,
        )
        .await?;
        realization.commit().await?;
        append_order_outbox(
            &pool,
            "backup_test",
            "mkt-1",
            Some("ord-1"),
            Some("tok-1"),
            "persist_trade",
            "ok",
            "backup verification",
        )
        .await?;
        let _ = append_trade_fill_snapshot(
            &pool,
            trade_id,
            "mkt-1",
            "ord-1",
            "entry",
            0.50,
            0.51,
            4.0,
            "test",
            "backup fill",
        )
        .await?;
        insert_analysis_snapshot(
            &pool,
            &AnalysisSnapshot {
                timestamp: "2026-01-01T00:00:00Z".to_string(),
                run_id: "backup-run".to_string(),
                market_id: "mkt-1".to_string(),
                question: "backup analysis".to_string(),
                signal: "BUY_YES".to_string(),
                confidence: 0.77,
                net_ev: 0.05,
                p_up: Some(0.61),
                price: 0.52,
                liquidity: 10_000.0,
                regime: "TREND_UP".to_string(),
                phase: "active".to_string(),
                time_remaining_minutes: 7.5,
                btc_change_pct: Some(0.8),
                reason: "backup".to_string(),
                end_date: "2026-01-01T00:15:00Z".to_string(),
                signals_json: Some(r#"{"fusion":1}"#.to_string()),
                calibration_version: Some("test-cal-v1".to_string()),
                source: Some("test".to_string()),
            },
        )
        .await?;
        upsert_idempotency_entry(&pool, "backup-k1", r#"{"ok":true}"#, 1_234).await?;

        // Every durable execution table must be nonempty in the restore
        // fixture. Pending intents, partial matches and manual controls must
        // survive a copy, not merely the schema of empty tables.
        sqlx::query("INSERT INTO live_orders VALUES('live-pending',?,'account','token','exit',0.5,5,0.07,1,1,'exchange',2,'pending','created','updated')")
            .bind(trade_id).execute(&pool).await?;
        sqlx::query("INSERT INTO live_matches VALUES('live-pending','receipt:1',2,0.5,0.01,0)")
            .execute(&pool)
            .await?;
        sqlx::query("INSERT INTO maker_orders(order_id,market_id,token_id,side,price,size,confirmed_size,state,template_json,settlement_json,created_at,updated_at,terminal_history_json)
            VALUES('maker-uncertain','mkt-1','token','BUY',0.5,5,2,'AUDIT_UNCERTAIN','{}','{}','created','updated','[]')")
            .execute(&pool).await?;
        sqlx::query("INSERT INTO maker_matches VALUES('receipt:2','maker-uncertain','token','BUY',2,0.5,0,'time')")
            .execute(&pool).await?;
        sqlx::query("INSERT INTO maker_match_allocations VALUES('receipt:2','maker-uncertain',?,'entry',2,1,0)")
            .bind(trade_id).execute(&pool).await?;
        maker::persist_manual_stand_down(&pool, "stopped-market").await?;
        sqlx::query("INSERT INTO paper_maker_orders VALUES('sim-mkr-backup','run','mkt-1','token',0.5,5,2,1,?,'{}','OPEN','time')")
            .bind(trade_id).execute(&pool).await?;
        sqlx::query(
            "INSERT INTO paper_maker_fill_events VALUES('sim-mkr-backup','observation',2,0.5)",
        )
        .execute(&pool)
        .await?;

        let source_counts = critical_table_counts(&pool).await?;

        backup_database(&pool, &backup_str).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&backup)?.permissions().mode() & 0o777,
                0o600
            );
        }

        let backup_pool = open(&backup_str).await?;
        assert_eq!(sqlite_user_version(&backup_pool).await?, DB_SCHEMA_VERSION);
        assert_eq!(critical_table_counts(&backup_pool).await?, source_counts);
        assert_eq!(source_counts.realized_pnl_events, 1);
        let restored_delta: (f64, f64) = sqlx::query_as(
            "SELECT net_pnl, realized_shares FROM realized_pnl_events WHERE trade_id=? AND event_key='backup/partial'",
        ).bind(trade_id).fetch_one(&backup_pool).await?;
        assert_eq!(restored_delta, (-0.25, 1.0));
        assert_eq!(
            risk::observe_cash_peak(&backup_pool, "simulation", 85.0)
                .await?
                .unwrap()
                .peak_balance,
            95.0
        );
        let restored = load_trade_by_id(&backup_pool, trade_id)
            .await?
            .expect("backup trade");
        assert_eq!(restored.question, "backup");
        assert!(maker::market_stood_down(&backup_pool, "stopped-market").await?);
        let intent = maker::pending_orders(&backup_pool).await?.pop().unwrap();
        assert_eq!(intent.state, "AUDIT_UNCERTAIN");
        assert_eq!(intent.confirmed_size, 2.0);
        assert!((maker::reserved_buy_collateral(&backup_pool).await? - 1.5).abs() < 1e-9);
        let pending: String =
            sqlx::query_scalar("SELECT state FROM live_orders WHERE order_id='live-pending'")
                .fetch_one(&backup_pool)
                .await?;
        assert_eq!(pending, "pending");
        let paper: (f64, f64) = sqlx::query_as(
            "SELECT size,filled_shares FROM paper_maker_orders WHERE order_id='sim-mkr-backup'",
        )
        .fetch_one(&backup_pool)
        .await?;
        assert_eq!(paper, (5.0, 2.0));
        quick_check(&backup_pool).await?;

        drop(backup_pool);
        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        let _ = std::fs::remove_file(&backup);
        let _ = std::fs::remove_file(format!("{backup_str}-shm"));
        let _ = std::fs::remove_file(format!("{backup_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn backup_database_rejects_paths_outside_backup_root() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_backup_src_reject_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let invalid_backup =
            std::env::temp_dir().join(format!("macbot_backup_dst_reject_{uniq}.sqlite"));
        let invalid_backup_str = invalid_backup.to_string_lossy().to_string();

        let err = backup_database(&pool, &invalid_backup_str).await;
        assert!(err.is_err());

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn backup_validation_failure_does_not_publish_an_unverified_file() -> Result<()> {
        let pool = open(":memory:").await?;
        set_sqlite_user_version(&pool, DB_SCHEMA_VERSION - 1).await?;
        let backup = format!(
            "artifacts/backups/invalid-schema-{:032x}.sqlite",
            rand::random::<u128>()
        );
        assert!(backup_database(&pool, &backup).await.is_err());
        assert!(!Path::new(&backup).exists());
        Ok(())
    }

    #[tokio::test]
    async fn migrate_upgrades_legacy_trades_schema_without_swallowing_alter_errors(
    ) -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_legacy_schema_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let url = format!("sqlite://{}?mode=rwc", path_str);

        let legacy_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        sqlx::query(
            r#"CREATE TABLE trades (
                id             INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp      TEXT    NOT NULL,
                market_id      TEXT    NOT NULL,
                question       TEXT    NOT NULL,
                direction      TEXT    NOT NULL,
                entry_price    REAL    NOT NULL,
                amount         REAL    NOT NULL,
                shares         REAL    NOT NULL,
                expected_value REAL    NOT NULL,
                reason         TEXT    NOT NULL,
                confidence     REAL    NOT NULL,
                status         TEXT    NOT NULL,
                mode           TEXT    NOT NULL,
                source         TEXT    NOT NULL,
                end_date       TEXT    NOT NULL DEFAULT '',
                exit_price     REAL,
                exit_timestamp TEXT,
                pnl            REAL,
                exit_type      TEXT,
                was_correct    INTEGER,
                forced         INTEGER,
                partial_closed INTEGER DEFAULT 0,
                raw_confidence REAL,
                order_id       TEXT,
                token_id       TEXT,
                fok_failed     INTEGER DEFAULT 0
            )"#,
        )
        .execute(&legacy_pool)
        .await?;
        set_sqlite_user_version(&legacy_pool, 1).await?;
        drop(legacy_pool);

        let pool = open(&path_str).await?;
        assert!(table_has_column(&pool, "trades", "fee_paid").await?);
        assert!(table_has_column(&pool, "trades", "signals_json").await?);
        assert!(table_has_column(&pool, "trades", "calibration_version").await?);
        assert!(table_has_column(&pool, "trades", "run_id").await?);
        assert!(table_has_column(&pool, "trades", "strategy").await?);
        assert_eq!(sqlite_user_version(&pool).await?, DB_SCHEMA_VERSION);

        let id = insert_trade(&pool, &sample_trade("legacy-upgraded")).await?;
        let restored = load_trade_by_id(&pool, id).await?.expect("restored trade");
        assert_eq!(restored.strategy, "conviction");
        assert_eq!(restored.fee_paid, 0.0);

        drop(pool);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    /// A legacy file declares `trades.id` as a plain rowid alias, which SQLite
    /// reissues after a delete. `clear_history` deletes trades and leaves
    /// `trade_fills` behind, so without this the next trade adopts a deleted
    /// one's fills.
    #[tokio::test]
    async fn a_cleared_legacy_database_stops_handing_out_ids_that_fills_still_claim(
    ) -> anyhow::Result<()> {
        // The shape production is actually in: history has been cleared more than
        // once, so `trade_fills` references ids far above anything still in
        // `trades` (66 surviving trades against fills for trade_id up to 214).
        // AUTOINCREMENT alone does not help here — it only remembers the highest
        // id *this* table issued, which the orphans have already overtaken.
        let (path_str, path) = legacy_rowid_database(&[(1, "one"), (2, "two")], &[9]).await?;

        let pool = open(&path_str).await?;
        let surviving: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades")
            .fetch_one(&pool)
            .await?;
        assert_eq!(surviving, 2, "the rebuild lost or invented a row");

        // Simulate clear-history, which removes trades and nothing else.
        sqlx::query("DELETE FROM trades").execute(&pool).await?;
        let orphans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trade_fills")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            orphans, 1,
            "the fill outlives the trade, as it does in production"
        );

        let fresh = insert_trade(&pool, &sample_trade("after-clear")).await?;
        assert!(
            fresh > 9,
            "new trade took id {fresh}, which an orphaned fill already claims — \
             every per-trade fill sum for it would include a deleted trade's fills"
        );

        drop(pool);
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[tokio::test]
    async fn the_rebuild_keeps_the_rows_and_the_indexes() -> anyhow::Result<()> {
        let (path_str, path) = legacy_rowid_database(&[(1, "one"), (2, "two")], &[]).await?;

        let pool = open(&path_str).await?;

        let ddl: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'trades'",
        )
        .fetch_one(&pool)
        .await?;
        assert!(ddl.to_ascii_uppercase().contains("AUTOINCREMENT"), "{ddl}");

        let questions: Vec<String> = sqlx::query_scalar("SELECT question FROM trades ORDER BY id")
            .fetch_all(&pool)
            .await?;
        assert_eq!(questions, vec!["one".to_string(), "two".to_string()]);

        // Dropping a table drops its indexes; they have to come back.
        let index: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master \
             WHERE type = 'index' AND tbl_name = 'trades' AND name = 'idx_legacy_probe'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(
            index, 1,
            "the rebuild dropped an index and did not replay it"
        );

        drop(pool);
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    /// The whole point of the two columns: a partial close rewrites `amount`
    /// and `shares` to the residual, and the entry figures have to survive it.
    #[tokio::test]
    async fn a_partial_close_cannot_rewrite_what_the_position_cost() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_entry_cost_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let id = insert_trade(&pool, &sample_trade("partial")).await?;
        let (shares0, cost0): (f64, f64) =
            sqlx::query_as("SELECT entry_shares, entry_cost FROM trades WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!((shares0, cost0), (20.0, 10.0));

        // What partial_close does: shrink the residual, accumulate the pnl.
        let mut partial = sample_trade("partial");
        partial.id = id;
        partial.shares = 5.0;
        partial.amount = 2.5;
        partial.partial_closed = Some(1);
        partial.pnl = Some(3.0);
        update_trade(&pool, &partial).await?;

        let (shares1, cost1, residual): (f64, f64, f64) =
            sqlx::query_as("SELECT entry_shares, entry_cost, shares FROM trades WHERE id = ?")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(residual, 5.0, "the residual should still shrink");
        assert_eq!(
            (shares1, cost1),
            (20.0, 10.0),
            "the entry figures were rewritten by a partial close"
        );

        drop(pool);
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    async fn entry_accounting_test_db(
        name: &str,
    ) -> Result<(SqlitePool, std::path::PathBuf, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_entry_{name}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        Ok((open(&path_str).await?, path, path_str))
    }

    async fn entry_totals(pool: &SqlitePool, id: i64) -> Result<(f64, f64)> {
        Ok(
            sqlx::query_as("SELECT entry_shares, entry_cost FROM trades WHERE id = ?")
                .bind(id)
                .fetch_one(pool)
                .await?,
        )
    }

    #[tokio::test]
    async fn live_entry_accounting_tracks_fills_not_reserved_budget_or_exits() -> Result<()> {
        let (pool, path, path_str) = entry_accounting_test_db("fills").await?;
        let mut trade = sample_trade("delayed live fill");
        trade.mode = "live".into();
        trade.shares = 0.0;
        trade.id = insert_trade(&pool, &trade).await?;
        assert_eq!(entry_totals(&pool, trade.id).await?, (0.0, 0.0));

        for (size, avg) in [(2.0, 0.5), (4.0, 0.55), (4.0, 0.55), (2.0, 0.5)] {
            append_trade_fill_snapshot(
                &pool,
                trade.id,
                "mkt-1",
                "entry-1",
                "entry",
                0.5,
                avg,
                size,
                "test",
                "cumulative entry fill",
            )
            .await?;
        }
        // An ordinary row update can carry an older entry mark. Equal-share
        // updates must preserve the more precise fill-ledger cost.
        trade.shares = 4.0;
        trade.entry_price = 0.5;
        update_trade(&pool, &trade).await?;
        let (_, cost) = entry_totals(&pool, trade.id).await?;
        assert!((cost - 2.2).abs() < 1e-9);
        // A replacement entry order is independently cumulative.
        append_trade_fill_snapshot(
            &pool,
            trade.id,
            "mkt-1",
            "entry-2",
            "entry",
            0.6,
            0.6,
            2.0,
            "test",
            "replacement order",
        )
        .await?;
        let (shares, cost) = entry_totals(&pool, trade.id).await?;
        assert_eq!(shares, 6.0);
        assert!(
            (cost - 3.4).abs() < 1e-9,
            "actual cost {cost}, not $10 reserved"
        );

        trade.shares = 1.0;
        trade.amount = 0.55;
        trade.partial_closed = Some(1);
        trade.pnl = Some(0.4);
        update_trade(&pool, &trade).await?;
        append_trade_fill_snapshot(
            &pool,
            trade.id,
            "mkt-1",
            "exit-1",
            "exit",
            0.7,
            0.7,
            5.0,
            "test",
            "partial exit",
        )
        .await?;
        assert_eq!(entry_totals(&pool, trade.id).await?, (shares, cost));
        pool.close().await;
        let reopened = open(&path_str).await?;
        assert_eq!(entry_totals(&reopened, trade.id).await?, (shares, cost));
        reopened.close().await;
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[tokio::test]
    async fn live_entry_accounting_is_atomic_with_fill_append() -> Result<()> {
        let (pool, path, path_str) = entry_accounting_test_db("atomic").await?;
        let mut trade = sample_trade("entry persistence failure");
        trade.mode = "live".into();
        trade.shares = 0.0;
        let id = insert_trade(&pool, &trade).await?;
        sqlx::query(
            "CREATE TRIGGER reject_entry_cost BEFORE UPDATE OF entry_cost ON trades \
                     BEGIN SELECT RAISE(ABORT, 'test entry accounting failure'); END",
        )
        .execute(&pool)
        .await?;
        let result = append_trade_fill_snapshot(
            &pool, id, "mkt-1", "entry", "entry", 0.5, 0.5, 4.0, "test", "",
        )
        .await;
        assert!(result.is_err());
        assert!(load_trade_fills(&pool, id, 10).await?.is_empty());
        assert_eq!(entry_totals(&pool, id).await?, (0.0, 0.0));
        pool.close().await;
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[tokio::test]
    async fn live_entry_accounting_serializes_concurrent_fill_snapshots() -> Result<()> {
        let (pool, path, path_str) = entry_accounting_test_db("concurrent").await?;
        let mut trade = sample_trade("concurrent fill polls");
        trade.mode = "live".into();
        trade.shares = 0.0;
        let id = insert_trade(&pool, &trade).await?;
        let (first, second) = tokio::join!(
            append_trade_fill_snapshot(
                &pool, id, "mkt-1", "entry", "entry", 0.5, 0.5, 2.0, "test", ""
            ),
            append_trade_fill_snapshot(
                &pool, id, "mkt-1", "entry", "entry", 0.5, 0.55, 4.0, "test", ""
            ),
        );
        first?;
        second?;
        let (shares, cost) = entry_totals(&pool, id).await?;
        assert_eq!(shares, 4.0);
        assert!((cost - 2.2).abs() < 1e-9);
        assert_eq!(
            trade_fill_summary(&pool, id).await?.entry_filled_size,
            shares
        );
        pool.close().await;
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[tokio::test]
    async fn live_entry_accounting_preserves_observed_fill_when_ledger_append_fails() -> Result<()>
    {
        let (pool, path, path_str) = entry_accounting_test_db("missing_ledger").await?;
        let mut trade = sample_trade("terminal fill with incomplete ledger");
        trade.mode = "live".into();
        trade.shares = 0.0;
        trade.id = insert_trade(&pool, &trade).await?;
        append_trade_fill_snapshot(
            &pool, trade.id, "mkt-1", "entry", "entry", 0.5, 0.5, 2.0, "test", "",
        )
        .await?;
        sqlx::query(
            "CREATE TRIGGER reject_fill BEFORE INSERT ON trade_fills \
                    BEGIN SELECT RAISE(ABORT, 'test fill append failure'); END",
        )
        .execute(&pool)
        .await?;
        assert!(append_trade_fill_snapshot(
            &pool, trade.id, "mkt-1", "entry", "entry", 0.5, 0.55, 4.0, "test", "",
        )
        .await
        .is_err());
        trade.shares = 4.0;
        trade.entry_price = 0.55;
        trade.amount = 2.2;
        trade.order_id = None;
        update_trade(&pool, &trade).await?;
        assert_eq!(entry_totals(&pool, trade.id).await?, (4.0, 2.2));

        // The migration must not let the older two-share ledger overwrite
        // the four shares independently observed on the trade row.
        sqlx::query("UPDATE trades SET entry_shares=0, entry_cost=10 WHERE id=?")
            .bind(trade.id)
            .execute(&pool)
            .await?;
        set_sqlite_user_version(&pool, 3).await?;
        pool.close().await;
        let upgraded = open(&path_str).await?;
        assert_eq!(entry_totals(&upgraded, trade.id).await?, (4.0, 2.2));
        upgraded.close().await;
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[tokio::test]
    async fn live_entry_accounting_migration_repairs_v3_and_preserves_non_live() -> Result<()> {
        let (pool, path, path_str) = entry_accounting_test_db("upgrade").await?;
        let mut live = sample_trade("old partial live position");
        live.mode = "live".into();
        live.shares = 0.0;
        live.id = insert_trade(&pool, &live).await?;
        for (size, avg) in [(2.0, 0.5), (4.0, 0.55)] {
            append_trade_fill_snapshot(
                &pool, live.id, "mkt-1", "entry", "entry", 0.5, avg, size, "test", "",
            )
            .await?;
        }
        live.shares = 1.0;
        live.amount = 0.55;
        live.partial_closed = Some(1);
        update_trade(&pool, &live).await?;
        sqlx::query("UPDATE trades SET entry_shares = 0, entry_cost = 10 WHERE id = ?")
            .bind(live.id)
            .execute(&pool)
            .await?;
        let pending = Trade {
            shares: 0.0,
            partial_closed: Some(0),
            id: 0,
            ..live.clone()
        };
        let pending_id = insert_trade(&pool, &pending).await?;
        sqlx::query("UPDATE trades SET entry_cost = 10 WHERE id = ?")
            .bind(pending_id)
            .execute(&pool)
            .await?;
        let sim_id = insert_trade(&pool, &sample_trade("unmodified simulation")).await?;
        set_sqlite_user_version(&pool, 3).await?;
        pool.close().await;

        let upgraded = open(&path_str).await?;
        assert_eq!(sqlite_user_version(&upgraded).await?, DB_SCHEMA_VERSION);
        let (shares, cost) = entry_totals(&upgraded, live.id).await?;
        assert_eq!(shares, 4.0);
        assert!((cost - 2.2).abs() < 1e-9, "incremental VWAP cost is {cost}");
        assert_eq!(entry_totals(&upgraded, pending_id).await?, (0.0, 0.0));
        assert_eq!(entry_totals(&upgraded, sim_id).await?, (20.0, 10.0));
        upgraded.close().await;
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    /// On a database that reissued ids, a trade's id may also be claimed by
    /// fills from a deleted trade. The backfill has to tell them apart, and the
    /// only thing that separates them is when they happened.
    #[tokio::test]
    async fn the_backfill_ignores_fills_from_outside_the_trades_own_lifetime() -> anyhow::Result<()>
    {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_window_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let url = format!("sqlite://{path_str}?mode=rwc");

        let legacy = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        legacy_schema(&legacy).await?;
        // A partially closed trade whose amount/shares are already the residual.
        sqlx::query(
            "INSERT INTO trades (id,timestamp,market_id,question,direction,entry_price, \
                 amount,shares,expected_value,reason,confidence,status,mode,source, \
                 end_date,exit_timestamp,partial_closed) \
             VALUES (7,'2026-04-03T20:00:00Z','m','q','BUY_YES',0.369,0.41,1.11,0.05, \
                 'r',0.7,'CLOSED','simulation','test','','2026-04-03T20:30:00Z',1)",
        )
        .execute(&legacy)
        .await?;
        for (ts, size, price) in [
            // A deleted trade's fills, sharing the reissued id 7.
            ("2026-03-16T20:01:02Z", 11.105, 0.225),
            // This trade's own entry.
            ("2026-04-03T20:00:01Z", 2.74, 0.369),
        ] {
            sqlx::query(
                "INSERT INTO trade_fills (timestamp,trade_id,market_id,order_id,phase, \
                     intended_price,avg_price,cumulative_size,delta_size,delta_price,source) \
                 VALUES (?,7,'m','o','entry',?,?,?,?,?,'test')",
            )
            .bind(ts)
            .bind(price)
            .bind(price)
            .bind(size)
            .bind(size)
            .bind(price)
            .execute(&legacy)
            .await?;
        }
        set_sqlite_user_version(&legacy, 2).await?;
        drop(legacy);

        let pool = open(&path_str).await?;
        let (entry_shares, entry_cost): (f64, f64) =
            sqlx::query_as("SELECT entry_shares, entry_cost FROM trades WHERE id = 7")
                .fetch_one(&pool)
                .await?;

        assert!(
            (entry_shares - 2.74).abs() < 1e-9,
            "expected only this trade's own entry fill, got {entry_shares}"
        );
        assert!(
            (entry_cost - 2.74 * 0.369).abs() < 1e-9,
            "entry cost {entry_cost} picked up a foreign fill"
        );

        drop(pool);
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    /// Run the real migration against a real database file, for hand-checking a
    /// production copy. Ignored by default because it needs a file that only
    /// exists on an operator's machine:
    ///
    ///     MACBOT_MIGRATION_PROBE_DB=/path/to/copy.db \
    ///         cargo test --lib migration_probe -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "needs MACBOT_MIGRATION_PROBE_DB pointing at a copy of a real database"]
    async fn migration_probe() -> anyhow::Result<()> {
        let Ok(path) = std::env::var("MACBOT_MIGRATION_PROBE_DB") else {
            return Ok(());
        };
        let pool = open(&path).await?;
        quick_check(&pool).await?;

        let ddl: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'trades'",
        )
        .fetch_one(&pool)
        .await?;
        let seq: Option<i64> =
            sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = 'trades'")
                .fetch_optional(&pool)
                .await?;
        let seq_rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_sequence WHERE name = 'trades'")
                .fetch_one(&pool)
                .await?;
        let trades: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades")
            .fetch_one(&pool)
            .await?;
        let indexes: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND tbl_name='trades'",
        )
        .fetch_one(&pool)
        .await?;
        let missing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM trades WHERE entry_shares IS NULL OR entry_cost IS NULL",
        )
        .fetch_one(&pool)
        .await?;

        println!(
            "autoincrement : {}",
            ddl.to_ascii_uppercase().contains("AUTOINCREMENT")
        );
        println!("sqlite_sequence rows for trades: {seq_rows} (seq = {seq:?})");
        println!("trades        : {trades}");
        println!("indexes       : {indexes}");
        println!("entry_* nulls : {missing}");
        Ok(())
    }

    #[tokio::test]
    async fn clearing_history_takes_the_fills_with_it_and_leaves_open_trades_alone(
    ) -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_clear_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let mut closed = sample_trade("closed");
        closed.status = "CLOSED".to_string();
        let closed_id = insert_trade(&pool, &closed).await?;
        let open_id = insert_trade(&pool, &sample_trade("still-open")).await?;
        let mut closing = sample_trade("unresolved-sale");
        closing.status = "CLOSING".to_string();
        let closing_id = insert_trade(&pool, &closing).await?;

        let mut realized = sample_trade("realized-risk-evidence");
        realized.status = "CLOSED".to_string();
        realized.pnl = Some(-1.0);
        let realized_id = insert_trade(&pool, &realized).await?;
        let mut realization = pool.begin_with("BEGIN IMMEDIATE").await?;
        risk::record_realized_delta(
            &mut realization,
            realized_id,
            "retained/risk",
            "2026-01-01T00:00:01Z",
            -1.0,
            20.0,
        )
        .await?;
        realization.commit().await?;

        for trade_id in [closed_id, open_id, closing_id, realized_id] {
            sqlx::query(
                "INSERT INTO trade_fills (timestamp,trade_id,market_id,order_id,phase, \
                     intended_price,avg_price,cumulative_size,delta_size,delta_price,source) \
                 VALUES ('2026-01-01T00:00:01Z',?,'m','o','entry',0.5,0.5,20.0,20.0,0.5,'t')",
            )
            .bind(trade_id)
            .execute(&pool)
            .await?;
        }

        let removed = clear_closed_trades(&pool, "OPEN").await?;
        assert_eq!(removed, 1);

        let orphaned: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM trade_fills WHERE trade_id = ?")
                .bind(closed_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            orphaned, 0,
            "the cleared trade's fills outlived it, and a reissued id would adopt them"
        );

        let kept: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trade_fills WHERE trade_id = ?")
            .bind(open_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(kept, 1, "an open trade lost its fills");

        let surviving: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades")
            .fetch_one(&pool)
            .await?;
        assert_eq!(surviving, 3);
        assert!(load_trade_by_id(&pool, realized_id).await?.is_some());
        let risk_events: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM realized_pnl_events WHERE trade_id=?")
                .bind(realized_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            risk_events, 1,
            "history cleanup cannot erase realized risk evidence"
        );
        let retained_fills: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM trade_fills WHERE trade_id=?")
                .bind(realized_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(retained_fills, 1);
        assert!(load_trade_by_id(&pool, closing_id).await?.is_some());
        let pending_fills: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM trade_fills WHERE trade_id = ?")
                .bind(closing_id)
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            pending_fills, 1,
            "unresolved sale evidence must survive history clearing"
        );

        drop(pool);
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[tokio::test]
    async fn a_trade_whose_pnl_contradicts_its_own_prices_is_reported() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_consistency_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        // Adds up: the fixture holds 20 shares entered at 0.5, so exiting at
        // 0.8 is a $6.00 gain.
        let mut good = sample_trade("adds-up");
        good.id = 1;
        good.status = "CLOSED".to_string();
        good.exit_price = Some(0.8);
        good.pnl = Some(6.00);
        insert_trade(&pool, &good).await?;

        // Does not: the shape of trade 57, a winner recorded as exiting at zero.
        let mut broken = sample_trade("contradicts-itself");
        broken.id = 2;
        broken.status = "CLOSED".to_string();
        broken.exit_price = Some(0.0);
        broken.pnl = Some(0.99);
        broken.was_correct = Some(1);
        insert_trade(&pool, &broken).await?;

        // Partially closed: the identity cannot apply, so it must not be
        // counted as either passing or failing.
        let mut partial = sample_trade("partially-closed");
        partial.id = 3;
        partial.status = "CLOSED".to_string();
        partial.exit_price = Some(0.0);
        partial.pnl = Some(0.99);
        partial.partial_closed = Some(1);
        insert_trade(&pool, &partial).await?;

        let report = trade_record_consistency(&pool, 0.06).await?;
        assert_eq!(report.checked, 2, "the partial close should not be checked");
        assert_eq!(report.mismatched, 1);
        assert_eq!(report.mismatched_ids, vec![2]);
        assert_eq!(report.unverifiable, 1);
        assert_eq!(report.missing_entry_cost, 0);

        // A stake with no recorded cost is the real inconsistency...
        sqlx::query("UPDATE trades SET entry_cost = NULL WHERE id = 1")
            .execute(&pool)
            .await?;
        assert_eq!(
            trade_record_consistency(&pool, 0.06)
                .await?
                .missing_entry_cost,
            1
        );

        // ...whereas a row that never staked anything is degenerate but
        // self-consistent, and must not be reported as a break.
        sqlx::query("UPDATE trades SET entry_cost = NULL, amount = 0 WHERE id = 1")
            .execute(&pool)
            .await?;
        assert_eq!(
            trade_record_consistency(&pool, 0.06)
                .await?
                .missing_entry_cost,
            0
        );

        drop(pool);
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[tokio::test]
    async fn accounting_version_preserves_legacy_fees_and_checks_actual_cash_fees() -> Result<()> {
        let pool = open(":memory:").await?;
        let mut legacy = sample_trade("legacy live fee semantics");
        legacy.mode = "live".into();
        legacy.status = "CLOSED".into();
        legacy.exit_price = Some(0.8);
        legacy.pnl = Some(6.0);
        legacy.fee_paid = 0.1;
        let legacy_id = insert_trade(&pool, &legacy).await?;
        let legacy = load_trade_by_id(&pool, legacy_id).await?.unwrap();
        assert!((legacy.net_realized_pnl() - 5.9).abs() < 1e-9);
        let mut managed = legacy.clone();
        managed.id = 0;
        managed.accounting_version = 1;
        managed.pnl = Some(5.7);
        let managed_id = insert_trade(&pool, &managed).await?;
        sqlx::query(
            "INSERT INTO maker_match_allocations
            (match_id,order_id,trade_id,phase,size,notional,fee_usd)
            VALUES('test-fee','test-order',?,'entry',20,10,0.3)",
        )
        .bind(managed_id)
        .execute(&pool)
        .await?;
        let managed = load_trade_by_id(&pool, managed_id).await?.unwrap();
        assert!((managed.net_realized_pnl() - 5.7).abs() < 1e-9);
        let checked = trade_record_consistency(&pool, 0.001).await?;
        assert_eq!(checked.checked, 2);
        assert_eq!(checked.mismatched, 0);
        sqlx::query("UPDATE trades SET pnl=pnl+0.01 WHERE id=?")
            .bind(managed_id)
            .execute(&pool)
            .await?;
        assert_eq!(
            trade_record_consistency(&pool, 0.001).await?.mismatched_ids,
            vec![managed_id]
        );
        // Repeat migration must not rewrite historical P&L or its marker.
        migrate(&pool).await?;
        assert!(
            (load_trade_by_id(&pool, legacy_id)
                .await?
                .unwrap()
                .net_realized_pnl()
                - 5.9)
                .abs()
                < 1e-9
        );
        Ok(())
    }

    #[tokio::test]
    async fn retention_drops_old_diagnostics_and_never_the_record() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_prune_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let old = (Utc::now() - chrono::Duration::days(40)).to_rfc3339();
        let recent = (Utc::now() - chrono::Duration::days(2)).to_rfc3339();
        for ts in [&old, &recent] {
            sqlx::query(
                "INSERT INTO order_outbox (timestamp,stage,market_id,action,status) \
                 VALUES (?,'maker_quote','m','suppress','observed')",
            )
            .bind(ts)
            .execute(&pool)
            .await?;
            sqlx::query(
                "INSERT INTO trade_rejections \
                     (timestamp,market_id,signal,confidence,net_ev,rejection_reason) \
                 VALUES (?,'m','BUY_YES',0.7,0.01,'thin book')",
            )
            .bind(ts)
            .execute(&pool)
            .await?;
        }
        let trade_id = insert_trade(&pool, &sample_trade("keep-me")).await?;

        let outcome = prune_operational_history(&pool, 30).await?;
        assert_eq!(
            outcome.order_outbox, 1,
            "the 40-day-old outbox row should go"
        );
        assert_eq!(outcome.trade_rejections, 1);

        let outbox_left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM order_outbox")
            .fetch_one(&pool)
            .await?;
        assert_eq!(outbox_left, 1, "the recent row must survive");

        // The record itself is never a retention target.
        assert!(load_trade_by_id(&pool, trade_id).await?.is_some());

        // Zero must disable the sweep, not empty the tables.
        let disabled = prune_operational_history(&pool, 0).await?;
        assert_eq!(disabled.total(), 0);
        let still_there: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM order_outbox")
            .fetch_one(&pool)
            .await?;
        assert_eq!(still_there, 1);

        drop(pool);
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    fn cleanup_sqlite(path_str: &str, path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    /// The v1 schema as it shipped before `AUTOINCREMENT` was declared: a plain
    /// rowid alias, which SQLite reissues after a delete.
    async fn legacy_schema(pool: &SqlitePool) -> anyhow::Result<()> {
        sqlx::query(
            r#"CREATE TABLE trades (
                id             INTEGER PRIMARY KEY,
                timestamp      TEXT    NOT NULL,
                market_id      TEXT    NOT NULL,
                question       TEXT    NOT NULL,
                direction      TEXT    NOT NULL,
                entry_price    REAL    NOT NULL,
                amount         REAL    NOT NULL,
                shares         REAL    NOT NULL,
                expected_value REAL    NOT NULL,
                reason         TEXT    NOT NULL,
                confidence     REAL    NOT NULL,
                status         TEXT    NOT NULL,
                mode           TEXT    NOT NULL,
                source         TEXT    NOT NULL,
                end_date       TEXT    NOT NULL DEFAULT '',
                exit_price     REAL,
                exit_timestamp TEXT,
                pnl            REAL,
                exit_type      TEXT,
                was_correct    INTEGER,
                forced         INTEGER,
                partial_closed INTEGER DEFAULT 0,
                raw_confidence REAL,
                order_id       TEXT,
                token_id       TEXT,
                fok_failed     INTEGER DEFAULT 0
            )"#,
        )
        .execute(pool)
        .await?;
        sqlx::query("CREATE INDEX idx_legacy_probe ON trades(market_id)")
            .execute(pool)
            .await?;
        sqlx::query(
            r#"CREATE TABLE trade_fills (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp       TEXT NOT NULL,
                trade_id        INTEGER NOT NULL,
                market_id       TEXT NOT NULL,
                order_id        TEXT NOT NULL,
                phase           TEXT NOT NULL,
                intended_price  REAL NOT NULL,
                avg_price       REAL NOT NULL,
                cumulative_size REAL NOT NULL,
                delta_size      REAL NOT NULL,
                delta_price     REAL NOT NULL,
                source          TEXT NOT NULL,
                details         TEXT NOT NULL DEFAULT ''
            )"#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn legacy_rowid_database(
        trades: &[(i64, &str)],
        fills_for: &[i64],
    ) -> anyhow::Result<(String, std::path::PathBuf)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_legacy_rowid_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let url = format!("sqlite://{path_str}?mode=rwc");

        let legacy = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;
        legacy_schema(&legacy).await?;
        for (id, question) in trades {
            sqlx::query(
                "INSERT INTO trades (id,timestamp,market_id,question,direction,entry_price, \
                     amount,shares,expected_value,reason,confidence,status,mode,source,end_date) \
                 VALUES (?,'2026-01-01T00:00:00Z','m',?,'BUY_YES',0.5,10.0,20.0,0.01,'r', \
                     0.7,'OPEN','simulation','test','')",
            )
            .bind(id)
            .bind(question)
            .execute(&legacy)
            .await?;
        }
        for trade_id in fills_for {
            sqlx::query(
                "INSERT INTO trade_fills (timestamp,trade_id,market_id,order_id,phase, \
                     intended_price,avg_price,cumulative_size,delta_size,delta_price,source) \
                 VALUES ('2026-01-01T00:00:01Z',?,'m','o','entry',0.5,0.5,20.0,20.0,0.5,'test')",
            )
            .bind(trade_id)
            .execute(&legacy)
            .await?;
        }
        set_sqlite_user_version(&legacy, 2).await?;
        drop(legacy);
        Ok((path_str, path))
    }

    #[tokio::test]
    async fn insert_trade_assigns_monotonic_ids() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_db_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();

        let pool = open(&path_str).await?;

        let id1 = insert_trade(&pool, &sample_trade("t1")).await?;
        let id2 = insert_trade(&pool, &sample_trade("t2")).await?;

        assert!(id1 > 0);
        assert_eq!(id2, id1 + 1);

        drop(pool);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[tokio::test]
    async fn calibration_version_roundtrips() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_cal_roundtrip_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let mut t = sample_trade("cal");
        t.calibration_version = Some("cal-v1".to_string());
        let id = insert_trade(&pool, &t).await?;
        let rows = load_all_trades(&pool).await?;
        let found = rows.into_iter().find(|x| x.id == id).expect("trade exists");
        assert_eq!(found.calibration_version.as_deref(), Some("cal-v1"));

        drop(pool);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[tokio::test]
    async fn run_id_roundtrips() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_runid_roundtrip_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let mut t = sample_trade("run");
        t.run_id = Some("run-20260309T070000Z-1234".to_string());
        let id = insert_trade(&pool, &t).await?;
        let rows = load_all_trades(&pool).await?;
        let found = rows.into_iter().find(|x| x.id == id).expect("trade exists");
        assert_eq!(found.run_id.as_deref(), Some("run-20260309T070000Z-1234"));

        drop(pool);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[tokio::test]
    async fn fill_summary_uses_incremental_vwap_math() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_fill_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let tid = insert_trade(&pool, &sample_trade("fill")).await?;

        // Entry order fills in two cumulative snapshots:
        // 1.0 @ 0.50 then 2.0 cumulative @ 0.55 -> second delta is 1.0 @ 0.60
        let _ = append_trade_fill_snapshot(
            &pool,
            tid,
            "mkt-1",
            "ord-entry",
            "entry",
            0.50,
            0.50,
            1.0,
            "test",
            "",
        )
        .await?;
        let _ = append_trade_fill_snapshot(
            &pool,
            tid,
            "mkt-1",
            "ord-entry",
            "entry",
            0.50,
            0.55,
            2.0,
            "test",
            "",
        )
        .await?;

        // Exit one-shot
        let _ = append_trade_fill_snapshot(
            &pool, tid, "mkt-1", "ord-exit", "exit", 0.65, 0.64, 2.0, "test", "",
        )
        .await?;

        let s = trade_fill_summary(&pool, tid).await?;
        assert!((s.entry_filled_size - 2.0).abs() < 1e-9);
        assert!((s.exit_filled_size - 2.0).abs() < 1e-9);
        assert!((s.entry_vwap.unwrap_or(0.0) - 0.55).abs() < 1e-9);
        assert!((s.exit_vwap.unwrap_or(0.0) - 0.64).abs() < 1e-9);
        assert!(s.entry_slippage_bps.unwrap_or(0.0) > 0.0);
        assert!(s.exit_slippage_bps.unwrap_or(0.0) > 0.0);

        drop(pool);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_fill_snapshots_do_not_double_count() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_fill_dup_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        let tid = insert_trade(&pool, &sample_trade("fill-dup")).await?;

        let first = append_trade_fill_snapshot(
            &pool,
            tid,
            "mkt-1",
            "ord-entry",
            "entry",
            0.50,
            0.50,
            1.0,
            "test",
            "first",
        )
        .await?;
        let duplicate = append_trade_fill_snapshot(
            &pool,
            tid,
            "mkt-1",
            "ord-entry",
            "entry",
            0.50,
            0.50,
            1.0,
            "test",
            "duplicate replay",
        )
        .await?;

        assert!(first.is_some());
        assert!(
            duplicate.is_none(),
            "duplicate cumulative snapshot should be ignored"
        );

        let fills = load_trade_fills(&pool, tid, 10).await?;
        assert_eq!(fills.len(), 1);

        let summary = trade_fill_summary(&pool, tid).await?;
        assert!((summary.entry_filled_size - 1.0).abs() < 1e-9);
        assert!((summary.entry_vwap.unwrap_or(0.0) - 0.50).abs() < 1e-9);

        drop(pool);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn trade_fill_parity_marks_open_trade_ok_on_entry_match() {
        let mut t = sample_trade("parity-open");
        t.set_status(TradeStatus::Open);
        t.entry_price = 0.50;
        t.exit_price = None;
        let summary = TradeFillSummary {
            entry_filled_size: 2.0,
            entry_vwap: Some(0.5006),
            entry_intended_vwap: Some(0.50),
            entry_slippage_bps: Some(12.0),
            exit_filled_size: 0.0,
            exit_vwap: None,
            exit_intended_vwap: None,
            exit_slippage_bps: None,
            roundtrip_edge: None,
            roundtrip_slippage_bps: None,
        };
        let parity = trade_fill_parity(&t, &summary, 20.0, 20.0);
        assert_eq!(parity.entry_status, "match");
        assert_eq!(parity.exit_status, "missing_reference");
        assert_eq!(parity.overall_status, "ok");
    }

    #[test]
    fn trade_fill_parity_marks_closed_trade_mismatch_on_exit_drift() {
        let mut t = sample_trade("parity-closed");
        t.set_status(TradeStatus::Closed);
        t.entry_price = 0.41;
        t.exit_price = Some(0.62);
        let summary = TradeFillSummary {
            entry_filled_size: 2.0,
            entry_vwap: Some(0.4102),
            entry_intended_vwap: Some(0.41),
            entry_slippage_bps: Some(4.87),
            exit_filled_size: 2.0,
            exit_vwap: Some(0.605),
            exit_intended_vwap: Some(0.62),
            exit_slippage_bps: Some(241.9),
            roundtrip_edge: Some(0.1948),
            roundtrip_slippage_bps: Some(246.77),
        };
        let parity = trade_fill_parity(&t, &summary, 30.0, 30.0);
        assert_eq!(parity.entry_status, "match");
        assert_eq!(parity.exit_status, "mismatch");
        assert_eq!(parity.overall_status, "mismatch");
        assert!(parity.exit_drift_bps.unwrap_or(0.0) > 30.0);
    }

    #[test]
    fn trade_fill_parity_marks_incomplete_when_exchange_fill_missing() {
        let mut t = sample_trade("parity-incomplete");
        t.set_status(TradeStatus::Closed);
        t.entry_price = 0.50;
        t.exit_price = Some(1.0);
        t.exit_type = Some("auto_resolved".into());
        let summary = TradeFillSummary {
            entry_filled_size: 0.0,
            entry_vwap: None,
            entry_intended_vwap: None,
            entry_slippage_bps: None,
            exit_filled_size: 0.0,
            exit_vwap: None,
            exit_intended_vwap: None,
            exit_slippage_bps: None,
            roundtrip_edge: None,
            roundtrip_slippage_bps: None,
        };
        let parity = trade_fill_parity(&t, &summary, 30.0, 30.0);
        assert_eq!(parity.entry_status, "missing_exchange_fill");
        assert_eq!(parity.exit_status, "resolved");
        assert_eq!(parity.overall_status, "incomplete");
    }

    #[test]
    fn manual_extreme_price_needs_fills_and_partial_exit_has_no_false_reference() {
        let mut t = sample_trade("manual-parity");
        t.set_status(TradeStatus::Closed);
        t.exit_type = Some("manual".into());
        t.exit_price = Some(0.999);
        let mut summary = TradeFillSummary {
            entry_vwap: Some(t.entry_price),
            ..Default::default()
        };
        let parity = trade_fill_parity(&t, &summary, 30.0, 30.0);
        assert_eq!(parity.exit_status, "missing_exchange_fill");
        assert_eq!(parity.overall_status, "incomplete");
        t.partial_closed = Some(1);
        summary.exit_vwap = Some(0.65);
        let parity = trade_fill_parity(&t, &summary, 30.0, 30.0);
        assert_eq!(parity.exit_status, "missing_reference");
        assert_eq!(parity.overall_status, "incomplete");
        t.mode = "live".into();
        t.fee_paid = 0.1;
        assert_eq!(
            trade_fill_parity(&t, &summary, 30.0, 30.0).entry_status,
            "missing_reference"
        );
    }

    #[test]
    fn redemption_parity_never_infers_receipt_from_book_price() {
        let mut t = sample_trade("redemption-parity");
        t.set_status(TradeStatus::Closed);
        t.exit_type = Some("redemption_confirmed".into());
        t.exit_price = Some(1.0);
        let summary = TradeFillSummary {
            entry_vwap: Some(t.entry_price),
            exit_vwap: Some(1.0),
            ..Default::default()
        };
        let parity = trade_fill_parity(&t, &summary, 30.0, 30.0);
        assert_eq!(parity.exit_status, "redemption_requires_receipt");
        assert_eq!(parity.overall_status, "incomplete");
        assert_eq!(parity.exit_drift_bps, None);
    }

    #[tokio::test]
    async fn cumulative_fill_cannot_create_negative_execution_notional() -> Result<()> {
        let pool = open(":memory:").await?;
        let tid = insert_trade(&pool, &sample_trade("contradictory fill")).await?;
        append_trade_fill_snapshot(
            &pool, tid, "mkt-1", "order", "entry", 0.5, 0.5, 2.0, "test", "",
        )
        .await?;
        assert!(append_trade_fill_snapshot(
            &pool, tid, "mkt-1", "order", "entry", 0.5, 0.2, 3.0, "test", ""
        )
        .await
        .is_err());
        assert!(append_trade_fill_snapshot(
            &pool, tid, "mkt-1", "order", "entry", 0.5, 0.9, 3.0, "test", ""
        )
        .await
        .is_err());
        assert_eq!(load_trade_fills(&pool, tid, 10).await?.len(), 1);
        assert!((trade_fill_summary(&pool, tid).await?.entry_vwap.unwrap() - 0.5).abs() < 1e-9);
        Ok(())
    }

    #[tokio::test]
    async fn database_path_is_a_literal_filename_not_a_connection_uri() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "macbot-literal-{:032x}?mode=ro%23.sqlite",
            rand::random::<u128>()
        ));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;
        let id = insert_trade(&pool, &sample_trade("literal filename")).await?;
        pool.close().await;
        assert!(path.is_file());
        let reopened = open(&path_str).await?;
        assert!(load_trade_by_id(&reopened, id).await?.is_some());
        reopened.close().await;
        cleanup_sqlite(&path_str, &path);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relative_file_scheme_is_a_literal_filename_not_an_in_memory_uri() -> Result<()> {
        // ':' is a legal Unix filename character; Windows refuses it at the
        // filesystem layer. Changing the process cwd would race other tests,
        // so use a unique relative name and remove it after closing the pool.
        let filename = format!(
            "file:macbot-literal-{:032x}?mode=memory&cache=shared.sqlite",
            rand::random::<u128>()
        );
        let path = std::env::current_dir()?.join(&filename);
        let pool = open(&filename).await?;
        let id = insert_trade(&pool, &sample_trade("file prefix literal")).await?;
        pool.close().await;
        assert!(path.is_file(), "file: path was treated as an in-memory URI");
        let reopened = open(&filename).await?;
        assert!(load_trade_by_id(&reopened, id).await?.is_some());
        reopened.close().await;
        cleanup_sqlite(path.to_str().unwrap(), &path);
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_roundtrip_and_prune_work() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idem_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        upsert_idempotency_entry(&pool, "k1", r#"{"a":1}"#, 1_000).await?;
        upsert_idempotency_entry(&pool, "k2", r#"{"a":2}"#, 2_000).await?;
        upsert_idempotency_entry(&pool, "k3", r#"{"a":3}"#, 3_000).await?;

        let got = get_idempotency_entry(&pool, "k2").await?;
        assert!(got.is_some());
        let (payload, ts) = got.unwrap_or_default();
        assert_eq!(payload, r#"{"a":2}"#);
        assert_eq!(ts, 2_000);

        prune_idempotency_entries(&pool, 1_500, 1).await?;
        let k1 = get_idempotency_entry(&pool, "k1").await?;
        assert!(k1.is_none());
        let k2 = get_idempotency_entry(&pool, "k2").await?;
        let k3 = get_idempotency_entry(&pool, "k3").await?;
        // max_entries=1 should keep only newest
        assert!(k2.is_none());
        assert!(k3.is_some());

        delete_idempotency_entry(&pool, "k3").await?;
        assert!(get_idempotency_entry(&pool, "k3").await?.is_none());

        drop(pool);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[tokio::test]
    async fn insert_idempotency_entry_if_absent_only_claims_once() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idem_claim_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = open(&path_str).await?;

        assert!(
            insert_idempotency_entry_if_absent(&pool, "k1", r#"{"pending":true}"#, 1_000).await?
        );
        assert!(
            !insert_idempotency_entry_if_absent(&pool, "k1", r#"{"pending":true}"#, 2_000).await?
        );
        assert_eq!(
            get_idempotency_entry(&pool, "k1").await?,
            Some((r#"{"pending":true}"#.to_string(), 1_000))
        );

        drop(pool);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[tokio::test]
    async fn reconstruction_retains_closing_paper_exposure_and_rejects_nonfinite_sums() -> Result<()>
    {
        let pool = open(":memory:").await?;
        let mut t = sample_trade("closing-paper-balance");
        t.status = "CLOSING".into();
        t.mode = "simulation".into();
        t.amount = 2.0;
        t.fee_paid = 0.1;
        t.pnl = Some(1.0);
        let id = insert_trade(&pool, &t).await?;
        let components = balance_reconstruction_components(&pool).await?;
        assert!((components.open_non_live_locked - 2.1).abs() < 1e-9);
        assert_eq!(components.open_non_live_realized, 1.0);
        assert_eq!(components.open_non_live_count, 1);
        sqlx::query("UPDATE trades SET amount=? WHERE id=?")
            .bind(f64::INFINITY)
            .bind(id)
            .execute(&pool)
            .await?;
        assert!(balance_reconstruction_components(&pool).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_prune_preserves_uncertain_claims() -> anyhow::Result<()> {
        let pool = open(":memory:").await?;
        upsert_idempotency_entry(&pool, "old-pending", r#"{"_macbot_state":"pending"}"#, 1).await?;
        upsert_idempotency_entry(
            &pool,
            "recent-pending",
            r#"{"_macbot_state":"pending"}"#,
            2_000,
        )
        .await?;
        upsert_idempotency_entry(&pool, "older-result", r#"{"ok":true}"#, 2_000).await?;
        upsert_idempotency_entry(&pool, "newest-result", r#"{"ok":true}"#, 3_000).await?;
        prune_idempotency_entries(&pool, 1_500, 1).await?;
        assert!(get_idempotency_entry(&pool, "old-pending").await?.is_some());
        assert!(get_idempotency_entry(&pool, "recent-pending")
            .await?
            .is_some());
        assert!(get_idempotency_entry(&pool, "older-result")
            .await?
            .is_none());
        assert!(get_idempotency_entry(&pool, "newest-result")
            .await?
            .is_some());
        Ok(())
    }
}
