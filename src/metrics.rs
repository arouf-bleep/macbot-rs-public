use std::sync::atomic::{AtomicU64, Ordering};

use once_cell::sync::Lazy;

#[derive(Default)]
pub struct Metrics {
    pub analysis_cycles: AtomicU64,
    pub analysis_snapshot_persist_fail: AtomicU64,
    pub shadow_kill_switch_trips: AtomicU64,
    pub trade_entry_block_kill_switch: AtomicU64,
    pub trade_entry_block_drawdown: AtomicU64,
    pub trade_entry_block_alpha_freeze: AtomicU64,
    pub trade_entry_block_degraded: AtomicU64,
    pub trade_entry_block_circuit_breaker: AtomicU64,
    pub trade_entry_block_l2_stale: AtomicU64,
    pub api_trade_requests: AtomicU64,
    pub api_trade_success: AtomicU64,
    pub api_trade_fail: AtomicU64,
    pub api_trade_filtered: AtomicU64,
    pub api_trade_idem_hits: AtomicU64,
    pub api_force_requests: AtomicU64,
    pub api_force_success: AtomicU64,
    pub api_force_fail: AtomicU64,
    pub api_force_idem_hits: AtomicU64,
    pub trades_opened: AtomicU64,
    pub trades_closed: AtomicU64,
    pub partial_closes: AtomicU64,
    pub capital_rebalances: AtomicU64,
    pub db_insert_fail: AtomicU64,
    pub clob_order_reject: AtomicU64,
    pub outbox_enqueued: AtomicU64,
    pub outbox_dropped: AtomicU64,
    pub outbox_write_fail: AtomicU64,
    pub actor_cmd_drop: AtomicU64,
    pub actor_cmd_drop_high: AtomicU64,
    pub actor_cmd_drop_low: AtomicU64,
    pub reconcile_full_runs: AtomicU64,
    pub reconcile_drift_events: AtomicU64,
    pub slo_guard_trips: AtomicU64,
    pub degraded_mode_transitions: AtomicU64,
    pub balance_drift_warns: AtomicU64,
    pub actor_lane_high_depth: AtomicU64,
    pub actor_lane_medium_depth: AtomicU64,
    pub actor_lane_low_depth: AtomicU64,
}

pub static METRICS: Lazy<Metrics> = Lazy::new(Metrics::default);

#[derive(Debug, Clone)]
pub struct MetricsSnapshot {
    pub analysis_cycles: u64,
    pub analysis_snapshot_persist_fail: u64,
    pub shadow_kill_switch_trips: u64,
    pub trade_entry_block_kill_switch: u64,
    pub trade_entry_block_drawdown: u64,
    pub trade_entry_block_alpha_freeze: u64,
    pub trade_entry_block_degraded: u64,
    pub trade_entry_block_circuit_breaker: u64,
    pub trade_entry_block_l2_stale: u64,
    pub api_trade_requests: u64,
    pub api_trade_success: u64,
    pub api_trade_fail: u64,
    pub api_trade_filtered: u64,
    pub api_trade_idem_hits: u64,
    pub api_force_requests: u64,
    pub api_force_success: u64,
    pub api_force_fail: u64,
    pub api_force_idem_hits: u64,
    pub trades_opened: u64,
    pub trades_closed: u64,
    pub partial_closes: u64,
    pub capital_rebalances: u64,
    pub db_insert_fail: u64,
    pub clob_order_reject: u64,
    pub outbox_enqueued: u64,
    pub outbox_dropped: u64,
    pub outbox_write_fail: u64,
    pub actor_cmd_drop: u64,
    pub actor_cmd_drop_high: u64,
    pub actor_cmd_drop_low: u64,
    pub reconcile_full_runs: u64,
    pub reconcile_drift_events: u64,
    pub slo_guard_trips: u64,
    pub degraded_mode_transitions: u64,
    pub balance_drift_warns: u64,
    pub actor_lane_high_depth: u64,
    pub actor_lane_medium_depth: u64,
    pub actor_lane_low_depth: u64,
}

pub fn snapshot() -> MetricsSnapshot {
    MetricsSnapshot {
        analysis_cycles: METRICS.analysis_cycles.load(Ordering::Relaxed),
        analysis_snapshot_persist_fail: METRICS
            .analysis_snapshot_persist_fail
            .load(Ordering::Relaxed),
        shadow_kill_switch_trips: METRICS.shadow_kill_switch_trips.load(Ordering::Relaxed),
        trade_entry_block_kill_switch: METRICS
            .trade_entry_block_kill_switch
            .load(Ordering::Relaxed),
        trade_entry_block_drawdown: METRICS.trade_entry_block_drawdown.load(Ordering::Relaxed),
        trade_entry_block_alpha_freeze: METRICS
            .trade_entry_block_alpha_freeze
            .load(Ordering::Relaxed),
        trade_entry_block_degraded: METRICS.trade_entry_block_degraded.load(Ordering::Relaxed),
        trade_entry_block_circuit_breaker: METRICS
            .trade_entry_block_circuit_breaker
            .load(Ordering::Relaxed),
        trade_entry_block_l2_stale: METRICS.trade_entry_block_l2_stale.load(Ordering::Relaxed),
        api_trade_requests: METRICS.api_trade_requests.load(Ordering::Relaxed),
        api_trade_success: METRICS.api_trade_success.load(Ordering::Relaxed),
        api_trade_fail: METRICS.api_trade_fail.load(Ordering::Relaxed),
        api_trade_filtered: METRICS.api_trade_filtered.load(Ordering::Relaxed),
        api_trade_idem_hits: METRICS.api_trade_idem_hits.load(Ordering::Relaxed),
        api_force_requests: METRICS.api_force_requests.load(Ordering::Relaxed),
        api_force_success: METRICS.api_force_success.load(Ordering::Relaxed),
        api_force_fail: METRICS.api_force_fail.load(Ordering::Relaxed),
        api_force_idem_hits: METRICS.api_force_idem_hits.load(Ordering::Relaxed),
        trades_opened: METRICS.trades_opened.load(Ordering::Relaxed),
        trades_closed: METRICS.trades_closed.load(Ordering::Relaxed),
        partial_closes: METRICS.partial_closes.load(Ordering::Relaxed),
        capital_rebalances: METRICS.capital_rebalances.load(Ordering::Relaxed),
        db_insert_fail: METRICS.db_insert_fail.load(Ordering::Relaxed),
        clob_order_reject: METRICS.clob_order_reject.load(Ordering::Relaxed),
        outbox_enqueued: METRICS.outbox_enqueued.load(Ordering::Relaxed),
        outbox_dropped: METRICS.outbox_dropped.load(Ordering::Relaxed),
        outbox_write_fail: METRICS.outbox_write_fail.load(Ordering::Relaxed),
        actor_cmd_drop: METRICS.actor_cmd_drop.load(Ordering::Relaxed),
        actor_cmd_drop_high: METRICS.actor_cmd_drop_high.load(Ordering::Relaxed),
        actor_cmd_drop_low: METRICS.actor_cmd_drop_low.load(Ordering::Relaxed),
        reconcile_full_runs: METRICS.reconcile_full_runs.load(Ordering::Relaxed),
        reconcile_drift_events: METRICS.reconcile_drift_events.load(Ordering::Relaxed),
        slo_guard_trips: METRICS.slo_guard_trips.load(Ordering::Relaxed),
        degraded_mode_transitions: METRICS.degraded_mode_transitions.load(Ordering::Relaxed),
        balance_drift_warns: METRICS.balance_drift_warns.load(Ordering::Relaxed),
        actor_lane_high_depth: METRICS.actor_lane_high_depth.load(Ordering::Relaxed),
        actor_lane_medium_depth: METRICS.actor_lane_medium_depth.load(Ordering::Relaxed),
        actor_lane_low_depth: METRICS.actor_lane_low_depth.load(Ordering::Relaxed),
    }
}

#[inline]
fn inc(a: &AtomicU64) {
    a.fetch_add(1, Ordering::Relaxed);
}

pub fn inc_api_trade_request() {
    inc(&METRICS.api_trade_requests);
}
pub fn inc_analysis_cycle() {
    inc(&METRICS.analysis_cycles);
}
pub fn inc_analysis_snapshot_persist_fail() {
    inc(&METRICS.analysis_snapshot_persist_fail);
}
pub fn inc_shadow_kill_switch_trip() {
    inc(&METRICS.shadow_kill_switch_trips);
}
pub fn inc_trade_entry_block_kill_switch() {
    inc(&METRICS.trade_entry_block_kill_switch);
}
pub fn inc_trade_entry_block_drawdown() {
    inc(&METRICS.trade_entry_block_drawdown);
}
pub fn inc_trade_entry_block_alpha_freeze() {
    inc(&METRICS.trade_entry_block_alpha_freeze);
}
pub fn inc_trade_entry_block_degraded() {
    inc(&METRICS.trade_entry_block_degraded);
}
pub fn inc_trade_entry_block_circuit_breaker() {
    inc(&METRICS.trade_entry_block_circuit_breaker);
}
pub fn inc_trade_entry_block_l2_stale() {
    inc(&METRICS.trade_entry_block_l2_stale);
}
pub fn inc_api_trade_success() {
    inc(&METRICS.api_trade_success);
}
pub fn inc_api_trade_fail() {
    inc(&METRICS.api_trade_fail);
}
pub fn inc_api_trade_filtered() {
    inc(&METRICS.api_trade_filtered);
}
pub fn inc_api_trade_idem_hit() {
    inc(&METRICS.api_trade_idem_hits);
}
pub fn inc_api_force_request() {
    inc(&METRICS.api_force_requests);
}
pub fn inc_api_force_success() {
    inc(&METRICS.api_force_success);
}
pub fn inc_api_force_fail() {
    inc(&METRICS.api_force_fail);
}
pub fn inc_api_force_idem_hit() {
    inc(&METRICS.api_force_idem_hits);
}
pub fn inc_trades_opened() {
    inc(&METRICS.trades_opened);
}
pub fn inc_trades_closed() {
    inc(&METRICS.trades_closed);
}
pub fn inc_partial_closes() {
    inc(&METRICS.partial_closes);
}
pub fn inc_capital_rebalances() {
    inc(&METRICS.capital_rebalances);
}
pub fn inc_db_insert_fail() {
    inc(&METRICS.db_insert_fail);
}
pub fn inc_clob_order_reject() {
    inc(&METRICS.clob_order_reject);
}
pub fn inc_outbox_enqueued() {
    inc(&METRICS.outbox_enqueued);
}
pub fn inc_outbox_dropped() {
    inc(&METRICS.outbox_dropped);
}
pub fn inc_outbox_write_fail() {
    inc(&METRICS.outbox_write_fail);
}
pub fn inc_actor_cmd_drop_high() {
    inc(&METRICS.actor_cmd_drop);
    inc(&METRICS.actor_cmd_drop_high);
}
pub fn inc_actor_cmd_drop_low() {
    inc(&METRICS.actor_cmd_drop);
    inc(&METRICS.actor_cmd_drop_low);
}
pub fn inc_reconcile_full_runs() {
    inc(&METRICS.reconcile_full_runs);
}
pub fn inc_reconcile_drift_events() {
    inc(&METRICS.reconcile_drift_events);
}
pub fn inc_slo_guard_trips() {
    inc(&METRICS.slo_guard_trips);
}
pub fn inc_degraded_mode_transitions() {
    inc(&METRICS.degraded_mode_transitions);
}
pub fn inc_balance_drift_warns() {
    inc(&METRICS.balance_drift_warns);
}

pub fn set_actor_lane_depths(high: usize, medium: usize, low: usize) {
    METRICS
        .actor_lane_high_depth
        .store(high as u64, Ordering::Relaxed);
    METRICS
        .actor_lane_medium_depth
        .store(medium as u64, Ordering::Relaxed);
    METRICS
        .actor_lane_low_depth
        .store(low as u64, Ordering::Relaxed);
}
