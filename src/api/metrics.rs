use axum::{
    extract::State,
    http::{header, HeaderValue, StatusCode},
    response::IntoResponse,
};
use std::fmt::Write as _;
use std::sync::Arc;

use crate::bot::state::TradingBot;
use crate::db;
use crate::metrics;
use serde_json::Value;

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

fn escape_prom_label(value: &str) -> String {
    value
        .replace('\\', r"\\")
        .replace('\n', r"\n")
        .replace('"', r#"\""#)
}

fn append_maker_l2_metrics(body: &mut String, summary: &MakerL2Summary) {
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_target_market_count gauge\nmacbot_maker_l2_target_market_count {}",
        summary.target_market_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_token_count gauge\nmacbot_maker_l2_token_count {}",
        summary.token_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_snapshot_loaded_count gauge\nmacbot_maker_l2_snapshot_loaded_count {}",
        summary.snapshot_loaded_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_two_sided_count gauge\nmacbot_maker_l2_two_sided_count {}",
        summary.two_sided_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_one_sided_count gauge\nmacbot_maker_l2_one_sided_count {}",
        summary.one_sided_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_stale_count gauge\nmacbot_maker_l2_stale_count {}",
        summary.stale_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_ready_count gauge\nmacbot_maker_l2_ready_count {}",
        summary.ready_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_quoteable_count gauge\nmacbot_maker_l2_quoteable_count {}",
        summary.quoteable_count
    );
    let _ = writeln!(
        body,
        "# TYPE macbot_maker_l2_not_ready_count gauge\nmacbot_maker_l2_not_ready_count {}",
        summary.not_ready_count
    );
    let _ = writeln!(body, "# TYPE macbot_maker_l2_not_ready_reason_count gauge");
    for (reason, count) in &summary.not_ready_reasons {
        let _ = writeln!(
            body,
            "macbot_maker_l2_not_ready_reason_count{{reason=\"{}\"}} {}",
            escape_prom_label(reason),
            count
        );
    }
}

pub async fn prometheus_metrics(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let s = metrics::snapshot();
    let (open_trades, closed_trades) = match (
        db::count_open_trades(&bot.db).await,
        db::count_closed_trades(&bot.db).await,
    ) {
        (Ok(o), Ok(c)) => (o, c),
        _ => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::http::HeaderMap::new(),
                "metrics trade counts unavailable".to_string(),
            )
        }
    };
    let loop_running = if bot.loop_running() { 1_u64 } else { 0_u64 };
    let kill_switch = if bot.kill_switch_active() {
        1_u64
    } else {
        0_u64
    };
    let degraded_no_trade = if bot.degraded_no_trade_active() {
        1_u64
    } else {
        0_u64
    };
    let analysis_age_secs = bot.analysis_age_secs().await;
    let balance = bot.balance().await;
    let last_balance_drift = bot.last_balance_drift_usd().await;
    let last_balance_drift_usd = last_balance_drift.unwrap_or(0.0);
    let last_balance_drift_known = if last_balance_drift.is_some() {
        1_u64
    } else {
        0_u64
    };
    let l2_feed_stale = if bot.l2_feed_stale() { 1_u64 } else { 0_u64 };
    let l2_feed_age_ms = bot.l2_feed_age_ms().await.unwrap_or(0);
    let maker_snapshot = bot.maker_state_snapshot().await;
    let maker_quote_count = maker_snapshot
        .get("quote_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let maker_replace_events_last_min = maker_snapshot
        .get("replace_events_last_min")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let maker_l2 = maker_l2_summary(&maker_snapshot);

    let body = format!(
        concat!(
            "# TYPE macbot_analysis_cycles_total counter\n",
            "macbot_analysis_cycles_total {}\n",
            "# TYPE macbot_analysis_snapshot_persist_fail_total counter\n",
            "macbot_analysis_snapshot_persist_fail_total {}\n",
            "# TYPE macbot_shadow_kill_switch_trips_total counter\n",
            "macbot_shadow_kill_switch_trips_total {}\n",
            "# TYPE macbot_trade_entry_block_kill_switch_total counter\n",
            "macbot_trade_entry_block_kill_switch_total {}\n",
            "# TYPE macbot_trade_entry_block_drawdown_total counter\n",
            "macbot_trade_entry_block_drawdown_total {}\n",
            "# TYPE macbot_trade_entry_block_alpha_freeze_total counter\n",
            "macbot_trade_entry_block_alpha_freeze_total {}\n",
            "# TYPE macbot_trade_entry_block_degraded_total counter\n",
            "macbot_trade_entry_block_degraded_total {}\n",
            "# TYPE macbot_trade_entry_block_circuit_breaker_total counter\n",
            "macbot_trade_entry_block_circuit_breaker_total {}\n",
            "# TYPE macbot_trade_entry_block_l2_stale_total counter\n",
            "macbot_trade_entry_block_l2_stale_total {}\n",
            "# TYPE macbot_api_trade_requests_total counter\n",
            "macbot_api_trade_requests_total {}\n",
            "# TYPE macbot_api_trade_success_total counter\n",
            "macbot_api_trade_success_total {}\n",
            "# TYPE macbot_api_trade_fail_total counter\n",
            "macbot_api_trade_fail_total {}\n",
            "# TYPE macbot_api_trade_filtered_total counter\n",
            "macbot_api_trade_filtered_total {}\n",
            "# TYPE macbot_api_trade_idempotent_hits_total counter\n",
            "macbot_api_trade_idempotent_hits_total {}\n",
            "# TYPE macbot_api_force_trade_requests_total counter\n",
            "macbot_api_force_trade_requests_total {}\n",
            "# TYPE macbot_api_force_trade_success_total counter\n",
            "macbot_api_force_trade_success_total {}\n",
            "# TYPE macbot_api_force_trade_fail_total counter\n",
            "macbot_api_force_trade_fail_total {}\n",
            "# TYPE macbot_api_force_trade_idempotent_hits_total counter\n",
            "macbot_api_force_trade_idempotent_hits_total {}\n",
            "# TYPE macbot_trades_opened_total counter\n",
            "macbot_trades_opened_total {}\n",
            "# TYPE macbot_trades_closed_total counter\n",
            "macbot_trades_closed_total {}\n",
            "# TYPE macbot_partial_closes_total counter\n",
            "macbot_partial_closes_total {}\n",
            "# TYPE macbot_capital_rebalances_total counter\n",
            "macbot_capital_rebalances_total {}\n",
            "# TYPE macbot_db_insert_fail_total counter\n",
            "macbot_db_insert_fail_total {}\n",
            "# TYPE macbot_clob_order_reject_total counter\n",
            "macbot_clob_order_reject_total {}\n",
            "# TYPE macbot_outbox_enqueued_total counter\n",
            "macbot_outbox_enqueued_total {}\n",
            "# TYPE macbot_outbox_dropped_total counter\n",
            "macbot_outbox_dropped_total {}\n",
            "# TYPE macbot_outbox_write_fail_total counter\n",
            "macbot_outbox_write_fail_total {}\n",
            "# TYPE macbot_actor_cmd_drop_total counter\n",
            "macbot_actor_cmd_drop_total {}\n",
            "# TYPE macbot_actor_cmd_drop_high_total counter\n",
            "macbot_actor_cmd_drop_high_total {}\n",
            "# TYPE macbot_actor_cmd_drop_low_total counter\n",
            "macbot_actor_cmd_drop_low_total {}\n",
            "# TYPE macbot_actor_lane_high_depth gauge\n",
            "macbot_actor_lane_high_depth {}\n",
            "# TYPE macbot_actor_lane_medium_depth gauge\n",
            "macbot_actor_lane_medium_depth {}\n",
            "# TYPE macbot_actor_lane_low_depth gauge\n",
            "macbot_actor_lane_low_depth {}\n",
            "# TYPE macbot_reconcile_full_runs_total counter\n",
            "macbot_reconcile_full_runs_total {}\n",
            "# TYPE macbot_reconcile_drift_events_total counter\n",
            "macbot_reconcile_drift_events_total {}\n",
            "# TYPE macbot_slo_guard_trips_total counter\n",
            "macbot_slo_guard_trips_total {}\n",
            "# TYPE macbot_degraded_mode_transitions_total counter\n",
            "macbot_degraded_mode_transitions_total {}\n",
            "# TYPE macbot_balance_drift_warns_total counter\n",
            "macbot_balance_drift_warns_total {}\n",
            "# TYPE macbot_open_trades gauge\n",
            "macbot_open_trades {}\n",
            "# TYPE macbot_closed_trades gauge\n",
            "macbot_closed_trades {}\n",
            "# TYPE macbot_loop_running gauge\n",
            "macbot_loop_running {}\n",
            "# TYPE macbot_kill_switch_active gauge\n",
            "macbot_kill_switch_active {}\n",
            "# TYPE macbot_degraded_no_trade_active gauge\n",
            "macbot_degraded_no_trade_active {}\n",
            "# TYPE macbot_analysis_age_seconds gauge\n",
            "macbot_analysis_age_seconds {}\n",
            "# TYPE macbot_balance_usd gauge\n",
            "macbot_balance_usd {:.6}\n",
            "# TYPE macbot_last_balance_drift_usd gauge\n",
            "macbot_last_balance_drift_usd {:.6}\n",
            "# TYPE macbot_last_balance_drift_known gauge\n",
            "macbot_last_balance_drift_known {}\n",
            "# TYPE macbot_l2_feed_stale gauge\n",
            "macbot_l2_feed_stale {}\n",
            "# TYPE macbot_l2_feed_age_ms gauge\n",
            "macbot_l2_feed_age_ms {}\n",
            "# TYPE macbot_maker_quote_count gauge\n",
            "macbot_maker_quote_count {}\n",
            "# TYPE macbot_maker_replace_events_last_min gauge\n",
            "macbot_maker_replace_events_last_min {}\n"
        ),
        s.analysis_cycles,
        s.analysis_snapshot_persist_fail,
        s.shadow_kill_switch_trips,
        s.trade_entry_block_kill_switch,
        s.trade_entry_block_drawdown,
        s.trade_entry_block_alpha_freeze,
        s.trade_entry_block_degraded,
        s.trade_entry_block_circuit_breaker,
        s.trade_entry_block_l2_stale,
        s.api_trade_requests,
        s.api_trade_success,
        s.api_trade_fail,
        s.api_trade_filtered,
        s.api_trade_idem_hits,
        s.api_force_requests,
        s.api_force_success,
        s.api_force_fail,
        s.api_force_idem_hits,
        s.trades_opened,
        s.trades_closed,
        s.partial_closes,
        s.capital_rebalances,
        s.db_insert_fail,
        s.clob_order_reject,
        s.outbox_enqueued,
        s.outbox_dropped,
        s.outbox_write_fail,
        s.actor_cmd_drop,
        s.actor_cmd_drop_high,
        s.actor_cmd_drop_low,
        s.actor_lane_high_depth,
        s.actor_lane_medium_depth,
        s.actor_lane_low_depth,
        s.reconcile_full_runs,
        s.reconcile_drift_events,
        s.slo_guard_trips,
        s.degraded_mode_transitions,
        s.balance_drift_warns,
        open_trades,
        closed_trades,
        loop_running,
        kill_switch,
        degraded_no_trade,
        analysis_age_secs,
        balance,
        last_balance_drift_usd,
        last_balance_drift_known,
        l2_feed_stale,
        l2_feed_age_ms,
        maker_quote_count,
        maker_replace_events_last_min,
    );
    let mut body = body;
    append_maker_l2_metrics(&mut body, &maker_l2);

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4"),
    );
    (StatusCode::OK, headers, body)
}

#[cfg(test)]
mod tests {
    use super::{append_maker_l2_metrics, maker_l2_summary};
    use serde_json::json;

    #[test]
    fn maker_l2_metrics_render_counts_and_reason_labels() {
        let snapshot = json!({
            "l2_summary": {
                "target_market_count": 2,
                "token_count": 4,
                "snapshot_loaded_count": 3,
                "two_sided_count": 3,
                "one_sided_count": 1,
                "stale_count": 1,
                "ready_count": 1,
                "quoteable_count": 1,
                "not_ready_count": 3,
                "not_ready_reasons": [
                    {"label": "token_bbo_stale", "count": 1},
                    {"label": "snapshot_missing", "count": 1},
                    {"label": "one_sided_book", "count": 1}
                ]
            }
        });

        let summary = maker_l2_summary(&snapshot);
        let mut body = String::new();
        append_maker_l2_metrics(&mut body, &summary);

        assert!(body.contains("macbot_maker_l2_target_market_count 2"));
        assert!(body.contains("macbot_maker_l2_snapshot_loaded_count 3"));
        assert!(body.contains("macbot_maker_l2_two_sided_count 3"));
        assert!(body.contains("macbot_maker_l2_one_sided_count 1"));
        assert!(body.contains("macbot_maker_l2_stale_count 1"));
        assert!(body.contains("macbot_maker_l2_ready_count 1"));
        assert!(body.contains("macbot_maker_l2_not_ready_count 3"));
        assert!(
            body.contains("macbot_maker_l2_not_ready_reason_count{reason=\"token_bbo_stale\"} 1")
        );
        assert!(
            body.contains("macbot_maker_l2_not_ready_reason_count{reason=\"snapshot_missing\"} 1")
        );
        assert!(
            body.contains("macbot_maker_l2_not_ready_reason_count{reason=\"one_sided_book\"} 1")
        );
    }
}
