use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::bot::state::TradingBot;
use crate::db;
use crate::domain::BotMode;

use super::realtime;

const DEFAULT_RECENT_TRADE_LIMIT: usize = 12;
const MAX_RECENT_TRADE_LIMIT: usize = 50;
const BALANCE_TOLERANCE_USD: f64 = 0.01;
const PNL_TOLERANCE_USD: f64 = 0.01;
const NOTIONAL_TOLERANCE_USD: f64 = 0.01;

#[derive(Debug, Clone, Default)]
struct MakerL2Summary {
    target_market_count: u64,
    token_count: u64,
    snapshot_loaded_count: u64,
    two_sided_count: u64,
    one_sided_count: u64,
    stale_count: u64,
    ready_count: u64,
    quoteable_count: u64,
    not_ready_count: u64,
    not_ready_reasons: Vec<(String, u64)>,
}

#[derive(Debug, Deserialize, Default)]
pub struct IntegrityReportQuery {
    pub sync_authoritative: Option<bool>,
    pub recent_limit: Option<usize>,
}

/// Fees make the identity inexact. The widest gap across the production book's
/// thirty-eight single-exit closed trades is 5.1 cents, so this sits just above
/// it: tight enough to catch an accounting break, loose enough not to flag fees.
const TRADE_PNL_TOLERANCE_USD: f64 = 0.06;

fn abs_diff(a: f64, b: f64) -> f64 {
    (a - b).abs()
}

fn push_mismatch(mismatches: &mut Vec<Value>, check: &str, detail: &str) {
    mismatches.push(json!({
        "check": check,
        "detail": detail,
    }));
}

fn value_as_f64(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

fn value_as_u64(v: Option<&Value>) -> Option<u64> {
    v.and_then(Value::as_u64)
}

fn value_as_bool(v: Option<&Value>) -> Option<bool> {
    v.and_then(Value::as_bool)
}

fn maker_l2_summary(maker_snapshot: &Value) -> MakerL2Summary {
    let Some(l2) = maker_snapshot.get("l2_summary") else {
        return MakerL2Summary::default();
    };
    let mut summary = MakerL2Summary {
        target_market_count: l2
            .get("target_market_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        token_count: l2.get("token_count").and_then(Value::as_u64).unwrap_or(0),
        snapshot_loaded_count: l2
            .get("snapshot_loaded_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        two_sided_count: l2
            .get("two_sided_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        one_sided_count: l2
            .get("one_sided_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        stale_count: l2.get("stale_count").and_then(Value::as_u64).unwrap_or(0),
        ready_count: l2.get("ready_count").and_then(Value::as_u64).unwrap_or(0),
        quoteable_count: l2
            .get("quoteable_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        not_ready_count: l2
            .get("not_ready_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        ..MakerL2Summary::default()
    };
    if let Some(reasons) = l2.get("not_ready_reasons").and_then(Value::as_array) {
        summary.not_ready_reasons = reasons
            .iter()
            .filter_map(|reason| {
                Some((
                    reason.get("label")?.as_str()?.to_string(),
                    reason.get("count")?.as_u64()?,
                ))
            })
            .collect();
    }
    summary
}

fn maker_l2_diagnosis(summary: &MakerL2Summary) -> Value {
    let top_reason = summary
        .not_ready_reasons
        .first()
        .map(|(label, count)| json!({"label": label, "count": count}));
    if summary.target_market_count == 0 {
        return json!({
            "classification": "no_target_markets",
            "detail": "maker snapshot has no target markets to quote",
            "top_reason": top_reason,
        });
    }
    if summary.ready_count == 0 {
        return json!({
            "classification": "no_ready_l2",
            "detail": format!(
                "maker has {} target markets but no ready L2 snapshots",
                summary.target_market_count
            ),
            "top_reason": top_reason,
        });
    }
    json!({
        "classification": "ready",
        "detail": format!(
            "maker has {} ready L2 snapshots across {} target markets",
            summary.ready_count,
            summary.target_market_count
        ),
        "top_reason": top_reason,
    })
}

fn recent_trade_ids(rows: &[Value], limit: usize) -> Vec<i64> {
    rows.iter()
        .filter_map(|row| row.get("id").and_then(Value::as_i64))
        .take(limit)
        .collect()
}

pub(crate) async fn dashboard_integrity_report(
    bot: &Arc<TradingBot>,
    sync_authoritative: bool,
    recent_limit: usize,
) -> Value {
    if sync_authoritative {
        return json!({"ok": false, "error": "read-only integrity checks cannot resync balances; use signed POST /api/resync/full"});
    }
    let recent_limit = recent_limit.clamp(1, MAX_RECENT_TRADE_LIMIT);
    let fast = realtime::fast_snapshot(bot).await;
    let slow = realtime::slow_snapshot_cached(bot).await;
    let accounting = bot.accounting_truth_snapshot(sync_authoritative).await;
    let stats = bot.get_performance_report().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let mode = cfg.mode_kind();
    let current_balance = bot.balance().await;
    let loop_running = bot.loop_running();
    let daily_evidence = match db::risk::daily_realized_pnl_for_origin(
        &bot.db,
        chrono::Utc::now().date_naive(),
        db::risk::CapitalOrigin::for_mode(mode),
    )
    .await
    {
        Ok(evidence) => evidence,
        Err(error) => {
            return json!({"ok":false,"incomplete":true,"error":format!("daily realized evidence unavailable: {error}")})
        }
    };
    let daily_pnl = daily_evidence.exact_today_pnl;
    let hot_trades = bot.trades().await;
    let hot_open_count = hot_trades
        .iter()
        .filter(|t| t.has_unsettled_exposure())
        .count();
    let hot_open_notional_usd: f64 = hot_trades
        .iter()
        .filter(|t| t.has_unsettled_exposure())
        .map(|t| t.amount)
        .sum();
    let (db_open_trades, db_recent_trades) = match tokio::try_join!(
        async {
            sqlx::query_as::<_, db::Trade>(
                "SELECT * FROM trades WHERE status IN ('OPEN','CLOSING') ORDER BY id",
            )
            .fetch_all(&bot.db)
            .await
            .map_err(anyhow::Error::from)
        },
        db::load_recent_trades(&bot.db, recent_limit)
    ) {
        Ok(rows) => rows,
        Err(error) => {
            return json!({"ok": false, "incomplete": true, "error": format!("integrity evidence unavailable: {error}")})
        }
    };
    let maker_snapshot = bot.maker_state_snapshot().await;
    let maker_l2 = maker_l2_summary(&maker_snapshot);
    let last_reconcile_summary = bot.last_reconcile_summary().await;
    let last_reconcile_error = bot.last_reconcile_error().await;

    let fast_balance = value_as_f64(fast.get("balance")).unwrap_or_default();
    let fast_daily_pnl = value_as_f64(fast.get("daily_pnl"));
    let fast_mode = fast.get("mode").and_then(Value::as_str).unwrap_or_default();
    let fast_loop_running = value_as_bool(fast.get("loop_running")).unwrap_or(false);
    let fast_open_positions = value_as_u64(fast.pointer("/execution/open_positions"))
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or_default();
    let fast_open_notional_usd =
        value_as_f64(fast.pointer("/execution/open_notional_usd")).unwrap_or_default();
    let fast_last_trade_id = fast
        .pointer("/execution/last_trade/id")
        .and_then(Value::as_i64);
    let fast_maker_quote_count = value_as_u64(fast.pointer("/maker/quote_count")).unwrap_or(0);
    let direct_maker_quote_count = value_as_u64(maker_snapshot.get("quote_count")).unwrap_or(0);
    let stats_current_balance = stats
        .get("current_balance")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let stats_today_pnl = stats.get("today_pnl").and_then(Value::as_f64);
    let stats_open_trades = stats
        .get("open_trades")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or_default();
    let accounting_current_balance = accounting
        .get("current_balance")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let accounting_derived_hot_balance = accounting
        .get("derived_hot_balance")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let accounting_open_hot_count = accounting
        .get("open_hot_count")
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or_default();
    let slow_trade_ids = slow
        .get("trades")
        .and_then(Value::as_array)
        .map(|rows| recent_trade_ids(rows, recent_limit))
        .unwrap_or_default();
    let mut db_recent_ids: Vec<i64> = db_recent_trades.iter().map(|t| t.id).collect();
    db_recent_ids.sort_unstable_by(|a, b| b.cmp(a));
    let db_last_trade_id = db_recent_ids.first().copied();

    let mut mismatches = Vec::new();
    let mut incomplete_checks = Vec::new();
    if !daily_evidence.evidence_complete {
        incomplete_checks.push("daily_realized_allocation_incomplete");
    }
    if slow.get("trades").and_then(Value::as_array).is_none() {
        incomplete_checks.push("slow_recent_trade_ids_vs_db");
    }
    if stats.get("error").is_some() || accounting.get("error").is_some() {
        incomplete_checks.push("accounting_evidence_unavailable");
    }

    // When the execution actor is mid-mutation (DB written but balance/hot
    // trades not yet updated), cross-source comparisons race and produce
    // false-positive drift reports.  Flag the condition and skip the checks
    // that are known to be affected.
    let actor_in_flight = bot.actor_mutating();

    if !actor_in_flight && abs_diff(fast_balance, current_balance) > BALANCE_TOLERANCE_USD {
        push_mismatch(
            &mut mismatches,
            "fast_balance_vs_runtime",
            &format!(
                "fast snapshot balance {:.4} differed from runtime balance {:.4}",
                fast_balance, current_balance
            ),
        );
    }
    if !actor_in_flight && abs_diff(stats_current_balance, current_balance) > BALANCE_TOLERANCE_USD
    {
        push_mismatch(
            &mut mismatches,
            "stats_balance_vs_runtime",
            &format!(
                "stats current_balance {:.4} differed from runtime balance {:.4}",
                stats_current_balance, current_balance
            ),
        );
    }
    if !actor_in_flight
        && abs_diff(accounting_current_balance, current_balance) > BALANCE_TOLERANCE_USD
    {
        push_mismatch(
            &mut mismatches,
            "accounting_balance_vs_runtime",
            &format!(
                "accounting current_balance {:.4} differed from runtime balance {:.4}",
                accounting_current_balance, current_balance
            ),
        );
    }
    if !actor_in_flight
        && mode != BotMode::Live
        && abs_diff(accounting_derived_hot_balance, current_balance) > BALANCE_TOLERANCE_USD
    {
        push_mismatch(
            &mut mismatches,
            "runtime_balance_vs_derived_hot_balance",
            &format!(
                "non-live runtime balance {:.4} drifted from derived_hot_balance {:.4}",
                current_balance, accounting_derived_hot_balance
            ),
        );
    }
    for (check, observed, evidence) in [
        (
            "fast_daily_pnl_vs_runtime",
            fast_daily_pnl,
            fast.get("daily_pnl_evidence"),
        ),
        (
            "stats_today_pnl_vs_runtime",
            stats_today_pnl,
            stats.get("daily_pnl_evidence"),
        ),
    ] {
        let same_basis = evidence
            .and_then(|v| v.get("day_utc"))
            .and_then(Value::as_str)
            == Some(daily_evidence.day_utc.as_str());
        match (observed, daily_pnl) {
            (Some(observed), Some(expected)) if same_basis => {
                if !actor_in_flight && abs_diff(observed, expected) > PNL_TOLERANCE_USD {
                    push_mismatch(&mut mismatches, check, &format!("reported realized P&L {observed:.4} differed from dated ledger P&L {expected:.4}"));
                }
            }
            _ => incomplete_checks.push(check),
        }
    }
    if fast_mode != cfg.mode {
        push_mismatch(
            &mut mismatches,
            "fast_mode_vs_runtime",
            &format!(
                "fast mode '{fast_mode}' differed from runtime mode '{}'",
                cfg.mode
            ),
        );
    }
    if fast_loop_running != loop_running {
        push_mismatch(
            &mut mismatches,
            "fast_loop_running_vs_runtime",
            &format!(
                "fast loop_running={} differed from runtime loop_running={}",
                fast_loop_running, loop_running
            ),
        );
    }
    if !actor_in_flight && fast_open_positions != hot_open_count {
        push_mismatch(
            &mut mismatches,
            "fast_open_positions_vs_hot_state",
            &format!(
                "fast open_positions={} differed from hot open count={}",
                fast_open_positions, hot_open_count
            ),
        );
    }
    if !actor_in_flight && stats_open_trades != hot_open_count {
        push_mismatch(
            &mut mismatches,
            "stats_open_trades_vs_hot_state",
            &format!(
                "stats open_trades={} differed from hot open count={}",
                stats_open_trades, hot_open_count
            ),
        );
    }
    if !actor_in_flight && accounting_open_hot_count != hot_open_count {
        push_mismatch(
            &mut mismatches,
            "accounting_open_hot_count_vs_hot_state",
            &format!(
                "accounting open_hot_count={} differed from hot open count={}",
                accounting_open_hot_count, hot_open_count
            ),
        );
    }
    if !actor_in_flight && db_open_trades.len() != hot_open_count {
        push_mismatch(
            &mut mismatches,
            "db_open_trades_vs_hot_state",
            &format!(
                "db open trade count={} differed from hot open count={}",
                db_open_trades.len(),
                hot_open_count
            ),
        );
    }
    if !actor_in_flight
        && abs_diff(fast_open_notional_usd, hot_open_notional_usd) > NOTIONAL_TOLERANCE_USD
    {
        push_mismatch(
            &mut mismatches,
            "fast_open_notional_vs_hot_state",
            &format!(
                "fast open_notional_usd {:.4} differed from hot open notional {:.4}",
                fast_open_notional_usd, hot_open_notional_usd
            ),
        );
    }
    if fast_last_trade_id != db_last_trade_id {
        push_mismatch(
            &mut mismatches,
            "fast_last_trade_vs_db",
            &format!(
                "fast last_trade_id {:?} differed from db last_trade_id {:?}",
                fast_last_trade_id, db_last_trade_id
            ),
        );
    }
    if slow.get("trades").and_then(Value::as_array).is_some() && slow_trade_ids != db_recent_ids {
        push_mismatch(
            &mut mismatches,
            "slow_recent_trade_ids_vs_db",
            &format!(
                "slow trade ids {:?} differed from db ids {:?}",
                slow_trade_ids, db_recent_ids
            ),
        );
    }
    if fast_maker_quote_count != direct_maker_quote_count {
        push_mismatch(
            &mut mismatches,
            "fast_maker_quote_count_vs_direct",
            &format!(
                "fast maker quote_count {} differed from direct maker quote_count {}",
                fast_maker_quote_count, direct_maker_quote_count
            ),
        );
    }

    // A closed, single-exit trade must satisfy pnl == shares * (exit - entry)
    // up to fees. Nothing asserted it before, which is how a winning trade that
    // records an exit price of 0.000 went unnoticed.
    let record = match crate::db::trade_record_consistency(&bot.db, TRADE_PNL_TOLERANCE_USD).await {
        Ok(record) => record,
        Err(error) => {
            return json!({"ok": false, "incomplete": true, "error": format!("trade record verification unavailable: {error}")})
        }
    };
    if record.mismatched > 0 {
        push_mismatch(
            &mut mismatches,
            "trade_record_pnl",
            &format!(
                "{} of {} single-exit closed trades do not satisfy \
                 pnl == shares * (exit - entry) within ${:.2}: ids {:?}",
                record.mismatched, record.checked, TRADE_PNL_TOLERANCE_USD, record.mismatched_ids
            ),
        );
    }
    if record.missing_entry_cost > 0 {
        push_mismatch(
            &mut mismatches,
            "trade_entry_cost",
            &format!(
                "{} closed trades carry no usable entry cost, so their return on \
                 turnover cannot be computed",
                record.missing_entry_cost
            ),
        );
    }

    json!({
        "ok": mismatches.is_empty() && incomplete_checks.is_empty(),
        "incomplete": !incomplete_checks.is_empty(),
        "incomplete_checks": incomplete_checks,
        "actor_in_flight": actor_in_flight,
        "trade_record": record,
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "sync_authoritative": sync_authoritative,
        "recent_limit": recent_limit,
        "checks": {
            "balance": {
                "runtime_balance": current_balance,
                "fast_balance": fast_balance,
                "stats_current_balance": stats_current_balance,
                "accounting_current_balance": accounting_current_balance,
                "accounting_derived_hot_balance": accounting_derived_hot_balance,
                "mode_uses_derived_hot_guard": mode != BotMode::Live,
                "derived_hot_drift_usd": current_balance - accounting_derived_hot_balance,
            },
            "daily_pnl": {
                "runtime_daily_pnl": daily_pnl,
                "evidence": daily_evidence,
                "basis": "UTC realization event day; partial sales included while the lot remains open",
                "fast_daily_pnl": fast_daily_pnl,
                "stats_today_pnl": stats_today_pnl,
            },
            "mode_and_loop": {
                "runtime_mode": cfg.mode,
                "fast_mode": fast_mode,
                "runtime_loop_running": loop_running,
                "fast_loop_running": fast_loop_running,
            },
            "open_positions": {
                "hot_open_count": hot_open_count,
                "db_open_count": db_open_trades.len(),
                "stats_open_trades": stats_open_trades,
                "accounting_open_hot_count": accounting_open_hot_count,
                "fast_open_positions": fast_open_positions,
                "hot_open_notional_usd": hot_open_notional_usd,
                "fast_open_notional_usd": fast_open_notional_usd,
            },
            "recent_trades": {
                "fast_last_trade_id": fast_last_trade_id,
                "db_last_trade_id": db_last_trade_id,
                "slow_trade_ids": slow_trade_ids,
                "db_trade_ids": db_recent_ids,
            },
            "maker": {
                "fast_quote_count": fast_maker_quote_count,
                "direct_quote_count": direct_maker_quote_count,
                "l2_summary": {
                    "target_market_count": maker_l2.target_market_count,
                    "token_count": maker_l2.token_count,
                    "snapshot_loaded_count": maker_l2.snapshot_loaded_count,
                    "two_sided_count": maker_l2.two_sided_count,
                    "one_sided_count": maker_l2.one_sided_count,
                    "stale_count": maker_l2.stale_count,
                    "ready_count": maker_l2.ready_count,
                    "quoteable_count": maker_l2.quoteable_count,
                    "not_ready_count": maker_l2.not_ready_count,
                    "not_ready_reasons": maker_l2
                        .not_ready_reasons
                        .iter()
                        .map(|(label, count)| json!({"label": label, "count": count}))
                        .collect::<Vec<_>>(),
                },
                "l2_diagnosis": maker_l2_diagnosis(&maker_l2),
            },
            "reconcile": {
                "last_reconcile_summary": last_reconcile_summary,
                "last_reconcile_error": last_reconcile_error,
            },
        },
        "mismatches": mismatches,
    })
}

pub async fn integrity_report(
    State(bot): State<Arc<TradingBot>>,
    Query(q): Query<IntegrityReportQuery>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let sync_authoritative = q.sync_authoritative.unwrap_or(false);
    if sync_authoritative {
        return Err((
            StatusCode::BAD_REQUEST,
            "use signed POST /api/resync/full to resync balances".into(),
        ));
    }
    let recent_limit = q.recent_limit.unwrap_or(DEFAULT_RECENT_TRADE_LIMIT);
    Ok(Json(
        dashboard_integrity_report(&bot, false, recent_limit).await,
    ))
}

#[cfg(test)]
mod tests {
    use super::dashboard_integrity_report;
    use crate::bot::state::TradingBot;
    use crate::db::Trade;
    use crate::market::l2_book::TopOfBook;
    use serde_json::Value;
    use std::sync::Arc;

    fn trade(id: i64, status: &str, amount: f64, pnl: Option<f64>) -> Trade {
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
            mode: "simulation".to_string(),
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
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: Some("test-cal-v1".to_string()),
            run_id: Some("test-run".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    async fn test_bot(
        label: &str,
    ) -> anyhow::Result<(Arc<TradingBot>, std::path::PathBuf, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_integrity_{label}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 25.0;

        let _ = crate::db::insert_trade(&pool, &trade(1, "CLOSED", 0.0, Some(2.5))).await?;
        let _ = crate::db::insert_trade(&pool, &trade(2, "OPEN", 3.0, None)).await?;

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        Ok((bot, path, path_str))
    }

    fn cleanup_sqlite(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    fn tob(
        market_id: &str,
        token_id: &str,
        best_bid: Option<f64>,
        best_ask: Option<f64>,
        snapshot_loaded: bool,
        stale: bool,
    ) -> TopOfBook {
        TopOfBook {
            market_id: market_id.to_string(),
            token_id: token_id.to_string(),
            is_yes_token: Some(true),
            best_bid,
            best_bid_size: best_bid.map(|_| 100.0),
            best_ask,
            best_ask_size: best_ask.map(|_| 100.0),
            spread_bps: match (best_bid, best_ask) {
                (Some(bid), Some(ask)) if ask >= bid => Some((ask - bid) * 10_000.0),
                _ => None,
            },
            sequence: Some(1),
            snapshot_loaded,
            stale,
            updated_at: "2026-03-30T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn unresolved_closing_position_is_counted_as_exposure_everywhere() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("closing").await?;
        sqlx::query("UPDATE trades SET status='CLOSING' WHERE id=2")
            .execute(&bot.db)
            .await?;
        bot.portfolio_state
            .trades
            .write()
            .await
            .iter_mut()
            .find(|t| t.id == 2)
            .unwrap()
            .status = "CLOSING".into();
        let report = dashboard_integrity_report(&bot, false, 5).await;
        for field in [
            "hot_open_count",
            "fast_open_positions",
            "db_open_count",
            "accounting_open_hot_count",
        ] {
            assert_eq!(
                report["checks"]["open_positions"][field], 1,
                "{field}: {report}"
            );
        }
        let mismatches = report["mismatches"].as_array().unwrap();
        assert!(
            !mismatches
                .iter()
                .any(|m| m["check"].as_str().unwrap_or("").contains("open")),
            "{report}"
        );
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn failed_database_evidence_cannot_report_clean_integrity() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("unavailable").await?;
        bot.db.close().await;
        let report = dashboard_integrity_report(&bot, false, 5).await;
        assert_eq!(report["ok"], false);
        assert_eq!(report["incomplete"], true);
        assert!(report["error"].as_str().unwrap().contains("unavailable"));
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integrity_report_is_clean_when_views_match() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("clean").await?;

        let report = dashboard_integrity_report(&bot, false, 5).await;
        let mismatches = report
            .get("mismatches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let non_cache_mismatches: Vec<_> = mismatches
            .iter()
            .filter(|m| {
                let check = m.get("check").and_then(Value::as_str).unwrap_or("");
                check != "slow_recent_trade_ids_vs_db" && check != "fast_last_trade_vs_db"
            })
            .collect();
        assert!(
            non_cache_mismatches.is_empty(),
            "{}",
            serde_json::to_string_pretty(&report)?
        );
        assert_eq!(
            report
                .pointer("/checks/open_positions/hot_open_count")
                .and_then(Value::as_u64),
            Some(1)
        );

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integrity_report_flags_non_live_balance_drift() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("drift").await?;
        *bot.portfolio_state.balance.write().await = 999.0;

        let report = dashboard_integrity_report(&bot, false, 5).await;
        assert_eq!(report.get("ok").and_then(Value::as_bool), Some(false));
        let mismatches = report
            .get("mismatches")
            .and_then(Value::as_array)
            .map(|rows| rows.to_vec())
            .unwrap_or_default();
        assert!(mismatches.iter().any(|m: &Value| {
            m.get("check").and_then(Value::as_str) == Some("runtime_balance_vs_derived_hot_balance")
        }));

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integrity_report_exposes_maker_l2_diagnosis_and_summary() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("maker_l2").await?;
        {
            let mut cfg = bot.control_plane.config.write().await;
            cfg.execution_mode = "maker".to_string();
            cfg.maker_live_quoting = true;
            cfg.auto_trading = true;
            cfg.maker_market_quality_max_spread_bps = 250.0;
        }
        bot.start_loop();
        bot.start_execution_actor().await;

        let quote = bot
            .maker_upsert_quote_state(
                "oid-1".to_string(),
                "mkt-1".to_string(),
                "tok-1".to_string(),
                crate::bot::actor::QuoteSide::Bid,
                0.50,
                1.0,
            )
            .await;
        assert!(quote.is_some(), "quote state should be inserted");
        let _ = bot
            .maker_publish_bbo(tob(
                "mkt-1",
                "tok-ready",
                Some(0.49),
                Some(0.51),
                true,
                false,
            ))
            .await;
        let _ = bot
            .maker_publish_bbo(tob(
                "mkt-1",
                "tok-stale",
                Some(0.48),
                Some(0.52),
                true,
                true,
            ))
            .await;
        let _ = bot
            .maker_publish_bbo(tob(
                "mkt-1",
                "tok-missing",
                Some(0.47),
                Some(0.53),
                false,
                false,
            ))
            .await;
        let _ = bot
            .maker_publish_bbo(tob("mkt-1", "tok-one-sided", Some(0.46), None, true, false))
            .await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        loop {
            let snapshot = bot.maker_state_snapshot().await;
            let quote_count = snapshot
                .get("quote_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let target_market_count = snapshot
                .pointer("/l2_summary/target_market_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let token_count = snapshot
                .pointer("/l2_summary/token_count")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            if quote_count == 1 && target_market_count == 1 && token_count == 4 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "maker snapshot never stabilized: {}",
                serde_json::to_string_pretty(&snapshot)?
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let report = dashboard_integrity_report(&bot, false, 5).await;
        let maker = report
            .get("checks")
            .and_then(|v| v.get("maker"))
            .expect("maker checks");
        let l2 = maker.get("l2_summary").expect("l2 summary");

        assert_eq!(
            l2.get("target_market_count").and_then(Value::as_u64),
            Some(1)
        );
        assert_eq!(l2.get("token_count").and_then(Value::as_u64), Some(4));
        assert_eq!(
            l2.get("snapshot_loaded_count").and_then(Value::as_u64),
            Some(3)
        );
        assert_eq!(l2.get("two_sided_count").and_then(Value::as_u64), Some(3));
        assert_eq!(l2.get("one_sided_count").and_then(Value::as_u64), Some(1));
        assert_eq!(l2.get("stale_count").and_then(Value::as_u64), Some(1));
        assert_eq!(l2.get("ready_count").and_then(Value::as_u64), Some(1));
        assert_eq!(l2.get("quoteable_count").and_then(Value::as_u64), Some(1));
        assert_eq!(l2.get("not_ready_count").and_then(Value::as_u64), Some(3));

        let diagnosis = maker.get("l2_diagnosis").expect("diagnosis");
        assert_eq!(
            diagnosis.get("classification").and_then(Value::as_str),
            Some("ready")
        );
        assert!(diagnosis.get("detail").and_then(Value::as_str).is_some());
        assert!(diagnosis.get("top_reason").is_some());

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integrity_report_marks_no_target_markets() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("no_targets").await?;

        let report = dashboard_integrity_report(&bot, false, 5).await;
        let maker = report
            .get("checks")
            .and_then(|v| v.get("maker"))
            .expect("maker checks");
        let l2 = maker.get("l2_summary").expect("l2 summary");
        assert_eq!(
            l2.get("target_market_count").and_then(Value::as_u64),
            Some(0)
        );
        assert_eq!(l2.get("ready_count").and_then(Value::as_u64), Some(0));
        let diagnosis = maker.get("l2_diagnosis").expect("diagnosis");
        assert_eq!(
            diagnosis.get("classification").and_then(Value::as_str),
            Some("no_target_markets")
        );

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }
}
