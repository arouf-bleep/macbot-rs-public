//! Durable risk accounting, partitioned by the capital that funds the orders.
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Sqlite, SqlitePool, Transaction};
use std::collections::HashMap;

/// Live wallet capital and the shared non-live ledger must never offset each
/// other's losses. Paper, shadow and simulation all draw from non-live cash.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapitalOrigin {
    Live,
    NonLive,
}

impl CapitalOrigin {
    pub fn for_mode(mode: crate::domain::BotMode) -> Self {
        if mode == crate::domain::BotMode::Live {
            Self::Live
        } else {
            Self::NonLive
        }
    }

    fn live_flag(self) -> i64 {
        i64::from(self == Self::Live)
    }
}

/// Apply the capital boundary before LIMIT, so simulated wins cannot hide the
/// live wallet's loss streak even when the recent aggregate window is full.
pub async fn recent_closed_trades_for_origin(
    pool: &SqlitePool,
    limit: usize,
    origin: CapitalOrigin,
) -> Result<Vec<super::Trade>> {
    let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
    let mut rows = sqlx::query_as::<_, super::Trade>(
        "SELECT * FROM trades WHERE status='CLOSED' AND (LOWER(TRIM(mode))='live')=? ORDER BY id DESC LIMIT ?",
    )
    .bind(origin.live_flag())
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.reverse();
    Ok(rows)
}

/// Realized accounting events use UTC recognition dates. Unknown legacy
/// cross-day allocations are disclosed separately and never authorize entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DailyRealizedPnl {
    pub day_utc: String,
    /// None denotes an explicitly requested aggregate research report.
    pub capital_origin: Option<CapitalOrigin>,
    pub dated_today_pnl: f64,
    pub exact_today_pnl: Option<f64>,
    pub conservative_unallocated_loss: f64,
    pub evidence_complete: bool,
    pub unallocated_trade_ids: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CashDrawdownPeak {
    pub reset_at: String,
    pub reset_balance: f64,
    pub peak_balance: f64,
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::query("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS realized_pnl_events (
        trade_id INTEGER NOT NULL, event_key TEXT NOT NULL,
        timestamp TEXT NOT NULL, net_pnl REAL NOT NULL,
        realized_shares REAL NOT NULL,
        PRIMARY KEY(trade_id,event_key)
    )",
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS realized_pnl_events_timestamp ON realized_pnl_events(timestamp)")
        .execute(pool).await?;
    Ok(())
}

/// Insert within the sale's accounting transaction, so risk can never observe
/// the changed lot without its dated realized delta. Identical receipt replays
/// are harmless; changed evidence for an existing identity is rejected.
pub async fn record_realized_delta(
    tx: &mut Transaction<'_, Sqlite>,
    trade_id: i64,
    event_key: &str,
    timestamp: &str,
    net_pnl: f64,
    realized_shares: f64,
) -> Result<()> {
    ensure!(
        trade_id > 0 && !event_key.trim().is_empty(),
        "invalid realized event identity"
    );
    ensure!(
        net_pnl.is_finite() && realized_shares.is_finite() && realized_shares > 0.0,
        "invalid realized event amounts"
    );
    let timestamp = DateTime::parse_from_rfc3339(timestamp)
        .context("invalid realized event timestamp")?
        .with_timezone(&Utc)
        .to_rfc3339();
    let prior: Option<(String, f64, f64)> = sqlx::query_as(
        "SELECT timestamp,net_pnl,realized_shares FROM realized_pnl_events WHERE trade_id=? AND event_key=?")
        .bind(trade_id).bind(event_key).fetch_optional(&mut **tx).await?;
    if let Some((old_time, old_pnl, old_shares)) = prior {
        ensure!(
            old_time == timestamp && old_pnl == net_pnl && old_shares == realized_shares,
            "realized event identity has contradictory evidence"
        );
        return Ok(());
    }
    sqlx::query("INSERT INTO realized_pnl_events(trade_id,event_key,timestamp,net_pnl,realized_shares) VALUES(?,?,?,?,?)")
        .bind(trade_id).bind(event_key).bind(timestamp).bind(net_pnl).bind(realized_shares)
        .execute(&mut **tx).await?;
    Ok(())
}

#[derive(Default)]
struct RealizedTotals {
    net_pnl: f64,
    shares: f64,
    dated: f64,
    has_day_event: bool,
    first_day: Option<NaiveDate>,
    last_day: Option<NaiveDate>,
}

/// Read both lots and their events in one SQLite snapshot. Fully dated events
/// cover remaining-open positions too. A legacy aggregate is attributable only
/// for a single close, or a lifecycle wholly within one UTC day. Cross-day
/// partial aggregates cannot prove today's loss, even when their net is zero.
pub async fn daily_realized_pnl(pool: &SqlitePool, day: NaiveDate) -> Result<DailyRealizedPnl> {
    daily_realized_pnl_scoped(pool, day, None).await
}

pub async fn daily_realized_pnl_for_origin(
    pool: &SqlitePool,
    day: NaiveDate,
    origin: CapitalOrigin,
) -> Result<DailyRealizedPnl> {
    daily_realized_pnl_scoped(pool, day, Some(origin)).await
}

async fn daily_realized_pnl_scoped(
    pool: &SqlitePool,
    day: NaiveDate,
    origin: Option<CapitalOrigin>,
) -> Result<DailyRealizedPnl> {
    let mut tx = pool.begin().await?;
    let live = origin.map(CapitalOrigin::live_flag);
    let events: Vec<(i64, String, f64, f64)> = sqlx::query_as(
        "SELECT e.trade_id,e.timestamp,e.net_pnl,e.realized_shares
         FROM realized_pnl_events e LEFT JOIN trades t ON t.id=e.trade_id
         WHERE (? IS NULL OR (LOWER(TRIM(t.mode))='live')=? OR t.id IS NULL)",
    )
    .bind(live)
    .bind(live)
    .fetch_all(&mut *tx)
    .await?;
    let mut totals: HashMap<i64, RealizedTotals> = HashMap::new();
    for (trade_id, timestamp, pnl, shares) in events {
        ensure!(
            pnl.is_finite() && shares.is_finite() && shares > 0.0,
            "invalid realized event amounts for trade {trade_id}"
        );
        let event_day = DateTime::parse_from_rfc3339(&timestamp)
            .context("invalid realized event time")?
            .with_timezone(&Utc)
            .date_naive();
        let total = totals.entry(trade_id).or_default();
        total.net_pnl += pnl;
        total.shares += shares;
        total.first_day = Some(total.first_day.map_or(event_day, |d| d.min(event_day)));
        total.last_day = Some(total.last_day.map_or(event_day, |d| d.max(event_day)));
        if event_day == day {
            total.dated += pnl;
            total.has_day_event = true;
        }
        ensure!(
            total.net_pnl.is_finite() && total.shares.is_finite() && total.dated.is_finite(),
            "realized event sum overflow"
        );
    }
    #[allow(clippy::type_complexity)]
    let trades: Vec<(i64, String, String, f64, Option<f64>, Option<f64>, f64, i64, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT id,timestamp,status,shares,entry_shares,pnl,fee_paid,accounting_version,partial_closed,exit_timestamp FROM trades
         WHERE status IN ('OPEN','CLOSING','CLOSED') AND (? IS NULL OR (LOWER(TRIM(mode))='live')=?)")
        .bind(live).bind(live)
        .fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let mut result = DailyRealizedPnl {
        day_utc: day.to_string(),
        capital_origin: origin,
        dated_today_pnl: 0.0,
        exact_today_pnl: None,
        conservative_unallocated_loss: 0.0,
        evidence_complete: true,
        unallocated_trade_ids: Vec::new(),
    };
    for (id, created, status, remaining, original, pnl, fee, version, partial, exited) in trades {
        let total = totals.remove(&id).unwrap_or_default();
        let pnl = pnl.unwrap_or(0.0);
        ensure!(
            pnl.is_finite()
                && fee.is_finite()
                && fee >= 0.0
                && remaining.is_finite()
                && remaining >= 0.0,
            "invalid realized lot amounts for trade {id}"
        );
        let closed = status == "CLOSED";
        let partial = partial.unwrap_or(0) != 0;
        if !closed && !partial && pnl == 0.0 && total.shares == 0.0 {
            continue;
        }
        let created_day = DateTime::parse_from_rfc3339(&created)
            .ok()
            .map(|t| t.with_timezone(&Utc).date_naive());
        let exited_day = exited
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc).date_naive());
        ensure!(
            !matches!((created_day,total.first_day),(Some(created),Some(event)) if event < created),
            "realized event precedes lot creation for trade {id}"
        );
        ensure!(
            !closed
                || !matches!((exited_day,total.last_day),(Some(exited),Some(event)) if event > exited),
            "realized event follows lot closure for trade {id}"
        );
        let original = original.filter(|s| s.is_finite() && *s > 0.0);
        ensure!(
            closed || !original.is_some_and(|s| remaining > s + 1e-7 * s.max(1.0)),
            "remaining quantity exceeds original lot for trade {id}"
        );
        let realized_shares = original.map(|s| if closed { s } else { (s - remaining).max(0.0) });
        let realized_fee = if version == 1 {
            0.0
        } else if closed {
            fee
        } else if let (Some(shares), Some(original)) = (realized_shares, original) {
            fee * shares / original
        } else {
            fee
        };
        let target = pnl - realized_fee;
        ensure!(
            target.is_finite(),
            "realized lot P&L overflow for trade {id}"
        );
        let residual = target - total.net_pnl;
        let tolerance = 1e-7 * target.abs().max(total.net_pnl.abs()).max(1.0);
        let quantity_complete =
            realized_shares.is_some_and(|s| (s - total.shares).abs() <= 1e-7 * s.max(1.0));
        ensure!(
            !quantity_complete || residual.abs() <= tolerance,
            "complete realized event quantities disagree with lot P&L for trade {id}"
        );
        if let Some(s) = realized_shares {
            ensure!(
                total.shares <= s + 1e-7 * s.max(1.0),
                "realized event quantity exceeds lot for trade {id}"
            );
        }
        result.dated_today_pnl += total.dated;
        if quantity_complete && residual.abs() <= tolerance {
            continue;
        }
        // A non-partial legacy closed row represents one dated realization.
        let single_close = closed && !partial && total.shares == 0.0;
        let single_day = created_day.is_some()
            && if closed {
                created_day == exited_day
            } else {
                created_day == Some(day)
            };
        if (single_close || single_day)
            && (if closed {
                exited_day.is_some()
            } else {
                created_day.is_some()
            })
        {
            let realized_day = if closed { exited_day } else { created_day };
            if realized_day == Some(day) {
                result.dated_today_pnl += residual;
            }
            continue;
        }
        // A position closed before the requested day cannot have subsequently
        // realized a sale today. Events are still included on their own dates.
        if closed && exited_day.is_some_and(|d| d < day) && !total.has_day_event {
            continue;
        }
        if created_day.is_some_and(|d| d > day) && !total.has_day_event {
            continue;
        }
        result.evidence_complete = false;
        result.unallocated_trade_ids.push(id);
        result.conservative_unallocated_loss += (-residual).max(0.0);
    }
    ensure!(totals.is_empty(), "orphaned realized P&L event");
    ensure!(
        result.dated_today_pnl.is_finite() && result.conservative_unallocated_loss.is_finite(),
        "daily realized P&L overflow"
    );
    result.unallocated_trade_ids.sort_unstable();
    result.exact_today_pnl = result.evidence_complete.then_some(result.dated_today_pnl);
    Ok(result)
}

fn key(mode: &str) -> Result<String> {
    ensure!(
        matches!(mode, "live" | "paper" | "shadow" | "simulation"),
        "invalid cash drawdown mode"
    );
    Ok(format!("drawdown_cash_peak:{mode}"))
}

fn validate(peak: &CashDrawdownPeak) -> Result<()> {
    ensure!(
        peak.reset_balance.is_finite()
            && peak.reset_balance >= 0.0
            && peak.peak_balance.is_finite()
            && peak.peak_balance >= peak.reset_balance,
        "invalid persisted cash drawdown peak"
    );
    DateTime::parse_from_rfc3339(&peak.reset_at)
        .context("invalid persisted drawdown reset time")?;
    Ok(())
}

pub async fn reset_cash_peak(
    pool: &SqlitePool,
    mode: &str,
    balance: f64,
) -> Result<CashDrawdownPeak> {
    ensure!(
        balance.is_finite() && balance >= 0.0,
        "invalid cash drawdown reset balance"
    );
    let key = key(mode)?;
    let peak = CashDrawdownPeak {
        reset_at: Utc::now().to_rfc3339(),
        reset_balance: balance,
        peak_balance: balance,
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(key).bind(serde_json::to_string(&peak)?).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(peak)
}

/// Return no peak until the operator has explicitly reset this mode. Once
/// present, atomically preserve or advance it; corruption never resets risk.
pub async fn observe_cash_peak(
    pool: &SqlitePool,
    mode: &str,
    balance: f64,
) -> Result<Option<CashDrawdownPeak>> {
    ensure!(
        balance.is_finite() && balance >= 0.0,
        "invalid observed cash balance"
    );
    let key = key(mode)?;
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
        .bind(&key)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(raw) = raw else {
        tx.commit().await?;
        return Ok(None);
    };
    let mut peak: CashDrawdownPeak =
        serde_json::from_str(&raw).context("invalid persisted cash drawdown data")?;
    validate(&peak)?;
    if balance > peak.peak_balance {
        peak.peak_balance = balance;
        sqlx::query("UPDATE meta SET value=? WHERE key=?")
            .bind(serde_json::to_string(&peak)?)
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Some(peak))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn lot(
        pool: &SqlitePool,
        created: &str,
        status: &str,
        shares: f64,
        pnl: f64,
        partial: i64,
        exited: Option<&str>,
    ) -> Result<i64> {
        let trade = super::super::Trade {
            timestamp: created.into(),
            status: status.into(),
            shares,
            amount: shares * 0.5,
            entry_price: 0.5,
            mode: "simulation".into(),
            partial_closed: Some(partial),
            pnl: Some(pnl),
            accounting_version: 1,
            exit_timestamp: exited.map(str::to_string),
            ..Default::default()
        };
        let id = super::super::insert_trade(pool, &trade).await?;
        sqlx::query("UPDATE trades SET entry_shares=10,entry_cost=5 WHERE id=?")
            .bind(id)
            .execute(pool)
            .await?;
        Ok(id)
    }

    #[tokio::test]
    async fn open_partial_losses_use_event_day_not_closure_or_prior_profit() -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        let id = lot(&pool, "2026-10-09T08:00:00Z", "OPEN", 2.0, -2.0, 1, None).await?;
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        record_realized_delta(&mut tx, id, "gain", "2026-10-10T22:00:00Z", 4.0, 4.0).await?;
        record_realized_delta(&mut tx, id, "loss", "2026-10-11T01:00:00+01:00", -6.0, 4.0).await?;
        tx.commit().await?;
        let yesterday =
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 10).unwrap()).await?;
        assert_eq!(yesterday.exact_today_pnl, Some(4.0));
        let today =
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 11).unwrap()).await?;
        assert_eq!(today.exact_today_pnl, Some(-6.0));
        assert!(today.evidence_complete);
        let mut tx = pool.begin().await?;
        record_realized_delta(&mut tx, id, "loss", "2026-10-11T00:00:00Z", -6.0, 4.0).await?;
        assert!(
            record_realized_delta(&mut tx, id, "loss", "2026-10-11T00:00:00Z", -7.0, 4.0)
                .await
                .is_err()
        );
        tx.rollback().await?;
        assert_eq!(
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
                .await?
                .exact_today_pnl,
            Some(-6.0)
        );
        Ok(())
    }

    #[tokio::test]
    async fn live_daily_losses_cannot_be_offset_by_non_live_profit() -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        let day = NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        let live = lot(
            &pool,
            "2026-10-11T01:00:00Z",
            "CLOSED",
            10.0,
            -60.0,
            0,
            Some("2026-10-11T02:00:00Z"),
        )
        .await?;
        sqlx::query("UPDATE trades SET mode='live' WHERE id=?")
            .bind(live)
            .execute(&pool)
            .await?;
        lot(
            &pool,
            "2026-10-11T01:00:00Z",
            "CLOSED",
            10.0,
            100.0,
            0,
            Some("2026-10-11T02:00:00Z"),
        )
        .await?;
        let live = daily_realized_pnl_for_origin(&pool, day, CapitalOrigin::Live).await?;
        let non_live = daily_realized_pnl_for_origin(&pool, day, CapitalOrigin::NonLive).await?;
        assert_eq!(live.exact_today_pnl, Some(-60.0));
        assert_eq!(live.capital_origin, Some(CapitalOrigin::Live));
        assert_eq!(non_live.exact_today_pnl, Some(100.0));
        let recent_live = recent_closed_trades_for_origin(&pool, 1, CapitalOrigin::Live).await?;
        assert_eq!(recent_live.len(), 1);
        assert_eq!(recent_live[0].pnl, Some(-60.0));
        assert_eq!(
            daily_realized_pnl(&pool, day).await?.exact_today_pnl,
            Some(40.0)
        );
        Ok(())
    }

    #[tokio::test]
    async fn partial_events_and_missing_dates_are_scoped_to_their_capital() -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        let day = NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        let live = lot(&pool, "2026-10-10T08:00:00Z", "OPEN", 5.0, -2.0, 1, None).await?;
        sqlx::query("UPDATE trades SET mode=' LIVE ' WHERE id=?")
            .bind(live)
            .execute(&pool)
            .await?;
        let mut tx = pool.begin().await?;
        record_realized_delta(
            &mut tx,
            live,
            "live-sale",
            "2026-10-11T01:00:00Z",
            -2.0,
            5.0,
        )
        .await?;
        tx.commit().await?;
        let shadow = lot(&pool, "2026-10-10T08:00:00Z", "OPEN", 5.0, 30.0, 1, None).await?;
        sqlx::query("UPDATE trades SET mode='shadow' WHERE id=?")
            .bind(shadow)
            .execute(&pool)
            .await?;
        let live = daily_realized_pnl_for_origin(&pool, day, CapitalOrigin::Live).await?;
        assert_eq!(live.exact_today_pnl, Some(-2.0));
        assert!(live.evidence_complete);
        let non_live = daily_realized_pnl_for_origin(&pool, day, CapitalOrigin::NonLive).await?;
        assert!(!non_live.evidence_complete);
        assert_eq!(non_live.unallocated_trade_ids, vec![shadow]);
        Ok(())
    }

    #[tokio::test]
    async fn undated_cross_day_partials_never_claim_exact_even_if_net_is_zero_or_positive(
    ) -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        let gain = lot(&pool, "2026-10-10T08:00:00Z", "OPEN", 5.0, 3.0, 1, None).await?;
        let zero = lot(&pool, "2026-10-10T08:00:00Z", "OPEN", 5.0, 0.0, 1, None).await?;
        let loss = lot(&pool, "2026-10-10T08:00:00Z", "OPEN", 5.0, -2.0, 1, None).await?;
        let today =
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 11).unwrap()).await?;
        assert_eq!(today.unallocated_trade_ids, vec![gain, zero, loss]);
        assert_eq!(today.exact_today_pnl, None);
        assert_eq!(today.dated_today_pnl, 0.0);
        assert_eq!(today.conservative_unallocated_loss, 2.0);
        assert!(!today.evidence_complete);
        Ok(())
    }

    #[tokio::test]
    async fn legacy_single_close_and_same_day_partial_have_provable_dates() -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        lot(
            &pool,
            "2026-10-10T08:00:00Z",
            "CLOSED",
            10.0,
            -4.0,
            0,
            Some("2026-10-11T00:00:00Z"),
        )
        .await?;
        lot(&pool, "2026-10-11T08:00:00Z", "OPEN", 5.0, -2.0, 1, None).await?;
        lot(
            &pool,
            "2026-10-09T08:00:00Z",
            "CLOSED",
            5.0,
            -100.0,
            1,
            Some("2026-10-10T09:00:00Z"),
        )
        .await?;
        assert_eq!(
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
                .await?
                .exact_today_pnl,
            Some(-6.0)
        );
        Ok(())
    }

    #[tokio::test]
    async fn complete_event_quantities_cannot_hide_changed_accounting_pnl() -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        let id = lot(&pool, "2026-10-11T00:00:00Z", "OPEN", 5.0, -1.0, 1, None).await?;
        let mut tx = pool.begin().await?;
        record_realized_delta(&mut tx, id, "sale", "2026-10-11T08:00:00Z", -2.0, 5.0).await?;
        tx.commit().await?;
        assert!(
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_close_date_and_closed_pool_never_fabricate_realization_day() -> Result<()> {
        let pool = super::super::open(":memory:").await?;
        lot(&pool, "2026-10-11T08:00:00Z", "CLOSED", 10.0, -4.0, 0, None).await?;
        assert_eq!(
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
                .await?
                .exact_today_pnl,
            None
        );
        pool.close().await;
        assert!(
            daily_realized_pnl(&pool, NaiveDate::from_ymd_opt(2026, 10, 11).unwrap())
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn reset_preserves_accounting_origin_and_peak_survives_reopen() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "macbot_risk_peak_{}.sqlite",
            Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let name = path.to_string_lossy().to_string();
        let pool = crate::db::open(&name).await?;
        assert!(observe_cash_peak(&pool, "simulation", 100.0)
            .await?
            .is_none());
        reset_cash_peak(&pool, "simulation", 80.0).await?;
        assert_eq!(
            observe_cash_peak(&pool, "simulation", 95.0)
                .await?
                .unwrap()
                .peak_balance,
            95.0
        );
        assert_eq!(
            observe_cash_peak(&pool, "simulation", 60.0)
                .await?
                .unwrap()
                .peak_balance,
            95.0
        );
        assert!(observe_cash_peak(&pool, "live", 500.0).await?.is_none());
        pool.close().await;
        let pool = crate::db::open(&name).await?;
        assert_eq!(
            observe_cash_peak(&pool, "simulation", 65.0)
                .await?
                .unwrap()
                .peak_balance,
            95.0
        );
        sqlx::query("UPDATE meta SET value='corrupt' WHERE key='drawdown_cash_peak:simulation'")
            .execute(&pool)
            .await?;
        assert!(observe_cash_peak(&pool, "simulation", 65.0).await.is_err());
        pool.close().await;
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{name}-wal"));
        let _ = std::fs::remove_file(format!("{name}-shm"));
        Ok(())
    }
}
