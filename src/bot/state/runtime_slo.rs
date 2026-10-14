use std::time::Instant;

use tracing::{info, warn};

use crate::domain::{BotMode, ExecutionMode};
use crate::metrics;

use super::TradingBot;

#[derive(Debug, Default)]
pub(super) struct SloGuardState {
    pub(super) l2_breach_since: Option<Instant>,
    pub(super) l2_trip_latched: bool,
    pub(super) backpressure_trip_latched: bool,
    pub(super) l2_trip_count: u32,
    pub(super) backpressure_trip_count: u32,
    pub(super) healthy_since: Option<Instant>,
    pub(super) last_actor_cmd_drop: u64,
    pub(super) last_outbox_drop: u64,
    pub(super) counters_primed: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RuntimeSloParams {
    pub(super) enforce: bool,
    pub(super) max_l2_age_ms: u64,
    pub(super) l2_breach_hold_ms: u64,
    pub(super) kill_on_l2_breach: bool,
    pub(super) kill_after_l2_trip_count: u32,
    pub(super) kill_after_backpressure_trip_count: u32,
    pub(super) recovery_hold_ms: u64,
    pub(super) max_actor_cmd_drop_delta: u64,
    pub(super) max_outbox_drop_delta: u64,
}

impl TradingBot {
    pub(super) fn runtime_slo_params_from_env() -> RuntimeSloParams {
        RuntimeSloParams {
            enforce: Self::env_bool("MACBOT_SLO_ENFORCE", true),
            max_l2_age_ms: Self::env_u64("MACBOT_SLO_MAX_L2_AGE_MS", 3_000).max(100),
            l2_breach_hold_ms: Self::env_u64("MACBOT_SLO_L2_BREACH_HOLD_MS", 15_000).max(100),
            kill_on_l2_breach: Self::env_bool("MACBOT_SLO_KILL_ON_L2_BREACH", true),
            kill_after_l2_trip_count: Self::env_u64("MACBOT_SLO_KILL_AFTER_L2_TRIPS", 3) as u32,
            kill_after_backpressure_trip_count: Self::env_u64(
                "MACBOT_SLO_KILL_AFTER_BACKPRESSURE_TRIPS",
                2,
            ) as u32,
            recovery_hold_ms: Self::env_u64("MACBOT_SLO_RECOVERY_HOLD_MS", 20_000).max(500),
            max_actor_cmd_drop_delta: Self::env_u64("MACBOT_SLO_MAX_ACTOR_CMD_DROPS_DELTA", 120),
            max_outbox_drop_delta: Self::env_u64("MACBOT_SLO_MAX_OUTBOX_DROPS_DELTA", 60),
        }
    }

    pub async fn enforce_runtime_slos(&self) {
        let params = Self::runtime_slo_params_from_env();
        self.enforce_runtime_slos_with(params).await;
    }

    pub(super) async fn enforce_runtime_slos_with(&self, params: RuntimeSloParams) {
        if !params.enforce || self.kill_switch_active() {
            return;
        }

        let cfg = self.control_plane.config.read().await.clone();
        if cfg.mode_kind() != BotMode::Live {
            return;
        }
        if !matches!(
            cfg.execution_mode_kind(),
            ExecutionMode::Maker | ExecutionMode::Hybrid
        ) {
            return;
        }

        let l2_age_ms = self.l2_feed_age_ms().await;
        let l2_bad = self.l2_feed_stale()
            || l2_age_ms
                .map(|age| age > params.max_l2_age_ms)
                .unwrap_or(true);
        let snapshot = metrics::snapshot();

        let mut trip_l2 = false;
        let mut trip_backpressure = false;
        let mut clear_degraded = false;
        let (l2_trip_count, bp_trip_count, actor_delta, outbox_delta) = {
            let mut state = self.runtime_state.slo_guard_state.lock().await;
            let mut actor_delta = 0u64;
            let mut outbox_delta = 0u64;

            if l2_bad {
                let since = state.l2_breach_since.get_or_insert_with(Instant::now);
                if since.elapsed().as_millis() as u64 >= params.l2_breach_hold_ms
                    && !state.l2_trip_latched
                {
                    trip_l2 = true;
                    state.l2_trip_latched = true;
                    state.l2_trip_count = state.l2_trip_count.saturating_add(1);
                }
            } else {
                state.l2_breach_since = None;
                state.l2_trip_latched = false;
            }

            if state.counters_primed {
                actor_delta = snapshot
                    .actor_cmd_drop
                    .saturating_sub(state.last_actor_cmd_drop);
                outbox_delta = snapshot
                    .outbox_dropped
                    .saturating_sub(state.last_outbox_drop);
                if (actor_delta > params.max_actor_cmd_drop_delta
                    || outbox_delta > params.max_outbox_drop_delta)
                    && !state.backpressure_trip_latched
                {
                    trip_backpressure = true;
                    state.backpressure_trip_latched = true;
                    state.backpressure_trip_count = state.backpressure_trip_count.saturating_add(1);
                } else if actor_delta == 0 && outbox_delta == 0 {
                    state.backpressure_trip_latched = false;
                }
            } else {
                state.counters_primed = true;
            }

            if !l2_bad && actor_delta == 0 && outbox_delta == 0 {
                let since = state.healthy_since.get_or_insert_with(Instant::now);
                if since.elapsed().as_millis() as u64 >= params.recovery_hold_ms {
                    if self.degraded_no_trade_active() {
                        clear_degraded = true;
                    }
                    state.l2_trip_count = 0;
                    state.backpressure_trip_count = 0;
                }
            } else {
                state.healthy_since = None;
            }
            let l2_trip_count = state.l2_trip_count;
            let bp_trip_count = state.backpressure_trip_count;
            state.last_actor_cmd_drop = snapshot.actor_cmd_drop;
            state.last_outbox_drop = snapshot.outbox_dropped;
            (l2_trip_count, bp_trip_count, actor_delta, outbox_delta)
        };

        if trip_l2 {
            let reason = format!(
                "runtime_slo_l2 age_ms={:?} max={} stale={}",
                l2_age_ms,
                params.max_l2_age_ms,
                self.l2_feed_stale()
            );
            self.set_l2_feed_stale(true).await;
            let _ = self
                .maker_publish_l2_stale(true, Some(reason.clone()))
                .await;
            self.set_degraded_no_trade(true);
            if params.kill_on_l2_breach && l2_trip_count >= params.kill_after_l2_trip_count {
                warn!("{reason} — activating kill switch");
                self.halt_trading("slo_l2_breach", reason.clone()).await;
            } else {
                warn!(
                    "{reason} — forcing degraded no-trade (l2_trip_count={} kill_after={})",
                    l2_trip_count, params.kill_after_l2_trip_count
                );
            }
            metrics::inc_slo_guard_trips();
        }

        if trip_backpressure {
            self.set_degraded_no_trade(true);
            if bp_trip_count >= params.kill_after_backpressure_trip_count {
                warn!(
                    "runtime_slo_backpressure actor_cmd_drop_delta={} outbox_drop_delta={} trip_count={} — activating kill switch",
                    actor_delta, outbox_delta, bp_trip_count
                );
                self.halt_trading(
                    "slo_backpressure",
                    format!(
                        "actor_cmd_drop_delta={actor_delta} outbox_drop_delta={outbox_delta} trip_count={bp_trip_count}"
                    ),
                )
                .await;
            } else {
                warn!(
                    "runtime_slo_backpressure actor_cmd_drop_delta={} outbox_drop_delta={} trip_count={} — degraded no-trade",
                    actor_delta, outbox_delta, bp_trip_count
                );
            }
            metrics::inc_slo_guard_trips();
        }

        if clear_degraded {
            info!(
                "runtime_slo_recovery stable for {}ms — clearing degraded no-trade mode",
                params.recovery_hold_ms
            );
            self.set_degraded_no_trade(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RuntimeSloParams, TradingBot};
    use crate::config::Config;
    use crate::db;
    use crate::metrics;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    async fn live_bot(
        name: &str,
        execution_mode: &str,
    ) -> anyhow::Result<(Arc<TradingBot>, std::path::PathBuf, String)> {
        let uniq = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_runtime_slo_{name}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = execution_mode.to_string();
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        Ok((bot, path, path_str))
    }

    fn cleanup_db(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    #[tokio::test]
    async fn runtime_slo_recovery_clears_degraded_after_healthy_hold() -> anyhow::Result<()> {
        let (bot, path, path_str) = live_bot("recovery", "maker").await?;

        bot.mark_l2_feed_event().await;
        bot.set_degraded_no_trade(true);
        {
            let snapshot = metrics::snapshot();
            let mut state = bot.runtime_state.slo_guard_state.lock().await;
            state.counters_primed = true;
            state.last_actor_cmd_drop = snapshot.actor_cmd_drop;
            state.last_outbox_drop = snapshot.outbox_dropped;
            state.healthy_since = Some(Instant::now() - Duration::from_millis(25));
            state.l2_trip_count = 2;
            state.backpressure_trip_count = 1;
        }

        bot.enforce_runtime_slos_with(RuntimeSloParams {
            enforce: true,
            max_l2_age_ms: 60_000,
            l2_breach_hold_ms: 1_000,
            kill_on_l2_breach: true,
            kill_after_l2_trip_count: 3,
            kill_after_backpressure_trip_count: 3,
            recovery_hold_ms: 10,
            max_actor_cmd_drop_delta: u64::MAX,
            max_outbox_drop_delta: u64::MAX,
        })
        .await;

        assert!(!bot.degraded_no_trade_active());
        assert!(!bot.kill_switch_active());
        let state = bot.runtime_state.slo_guard_state.lock().await;
        assert_eq!(state.l2_trip_count, 0);
        assert_eq!(state.backpressure_trip_count, 0);

        drop(state);
        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }
}
