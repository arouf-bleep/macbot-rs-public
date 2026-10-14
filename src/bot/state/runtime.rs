use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tracing::info;

use crate::db::Trade;
use crate::domain::BotMode;
use crate::market::l2_book::TopOfBook;
use crate::metrics;

use super::{ReconcileError, ReconcileSummary, TradingBot};

impl TradingBot {
    // ── Public accessors ──────────────────────────────────────────────────

    pub async fn balance(&self) -> f64 {
        *self.portfolio_state.balance.read().await
    }

    pub async fn set_balance(&self, v: f64) {
        *self.portfolio_state.balance.write().await = v;
    }

    pub async fn last_reconcile_summary(&self) -> Option<ReconcileSummary> {
        self.runtime_state
            .last_reconcile_summary
            .read()
            .await
            .clone()
    }

    pub async fn last_reconcile_error(&self) -> Option<ReconcileError> {
        self.runtime_state.last_reconcile_error.read().await.clone()
    }

    pub async fn record_reconcile_summary(&self, summary: ReconcileSummary) {
        *self.runtime_state.last_reconcile_summary.write().await = Some(summary);
        *self.runtime_state.last_reconcile_error.write().await = None;
    }

    pub async fn record_reconcile_error(&self, phase: &str, error: impl Into<String>) {
        let error = error.into();

        // Nearly every code path that halts trading records an error here on
        // the very next line, so the cause of the halt already exists; it was
        // simply going somewhere the halt could not see. Adopting it when the
        // switch is engaged and no cause is set yet covers every one of those
        // paths without editing each call site, and without overwriting a more
        // specific cause recorded by halt_trading.
        {
            let mut status = self.kill_switch_state();
            if status.active && status.cause.is_none() {
                status.cause = Some(super::runtime_state::KillSwitchCause {
                    source: phase.to_string(),
                    detail: error.clone(),
                    at: Utc::now().to_rfc3339(),
                });
            }
        }

        *self.runtime_state.last_reconcile_error.write().await = Some(ReconcileError {
            phase: phase.to_string(),
            timestamp: Utc::now().to_rfc3339(),
            error,
        });
    }

    pub async fn trades(&self) -> Vec<Trade> {
        self.portfolio_state.trades.read().await.clone()
    }

    pub async fn mode(&self) -> String {
        self.control_plane.config.read().await.mode.clone()
    }

    pub fn run_id(&self) -> &str {
        &self.control_plane.run_id
    }

    pub async fn mode_kind(&self) -> BotMode {
        self.control_plane.config.read().await.mode_kind()
    }

    /// Remove closed trades from the in-memory hot list.
    /// They remain in SQLite for analytics — this only frees working memory
    /// and prevents O(n) growth of linear scans in the trading hot path.
    pub async fn evict_closed_trades(&self) {
        let mut trades = self.portfolio_state.trades.write().await;
        let before = trades.len();
        let closed_ids: Vec<i64> = trades
            .iter()
            .filter(|t| t.status_kind() == crate::domain::TradeStatus::Closed)
            .map(|t| t.id)
            .collect();
        // CLOSING still owns exposure until authenticated settlement completes.
        trades.retain(|t| t.status_kind() != crate::domain::TradeStatus::Closed);
        let evicted = before - trades.len();
        if evicted > 0 {
            info!(
                "Evicted {evicted} closed trade(s) from hot memory ({before} → {})",
                trades.len()
            );
            let mut hw = self.trade_lifecycle.scalp_high_water.write().await;
            for id in &closed_ids {
                hw.remove(id);
            }
            let mut tp = self.trade_lifecycle.last_trade_prices.write().await;
            for id in &closed_ids {
                tp.remove(id);
            }
        }
    }

    // ── Loop control ──────────────────────────────────────────────────────

    pub fn loop_running(&self) -> bool {
        self.runtime_state.loop_running.load(Ordering::SeqCst)
    }

    pub fn backtest_only(&self) -> bool {
        self.runtime_state.backtest_only.load(Ordering::SeqCst)
    }

    pub fn start_loop(&self) {
        self.runtime_state
            .loop_running
            .store(true, Ordering::SeqCst);
        self.runtime_state.loop_notify.notify_one();
    }

    /// Invalidate queued automatic entries without blocking explicit manual actions.
    /// Saturation permanently refuses automatic admission rather than reusing an epoch.
    pub fn invalidate_automatic_entries(&self) {
        let _ = self.runtime_state.automatic_entry_generation.fetch_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |epoch| Some(epoch.saturating_add(1)),
        );
    }

    pub(crate) fn automatic_entry_origin(&self) -> super::super::actor::ExecutionOrigin {
        super::super::actor::ExecutionOrigin::Automatic {
            generation: self
                .runtime_state
                .automatic_entry_generation
                .load(Ordering::SeqCst),
        }
    }

    pub(crate) fn entry_origin_allowed_in_config(
        &self,
        origin: super::super::actor::ExecutionOrigin,
        cfg: &crate::config::Config,
    ) -> bool {
        match origin {
            super::super::actor::ExecutionOrigin::Manual => true,
            super::super::actor::ExecutionOrigin::Automatic { generation } => {
                generation != u64::MAX
                    && generation
                        == self
                            .runtime_state
                            .automatic_entry_generation
                            .load(Ordering::SeqCst)
                    && self.loop_running()
                    && cfg.auto_trading
                    && !self.entries_frozen()
            }
        }
    }

    pub(crate) async fn entry_origin_allowed(
        &self,
        origin: super::super::actor::ExecutionOrigin,
    ) -> bool {
        let cfg = self.control_plane.config.read().await;
        self.entry_origin_allowed_in_config(origin, &cfg)
    }

    pub fn stop_loop(&self) {
        self.runtime_state
            .loop_running
            .store(false, Ordering::SeqCst);
        self.invalidate_automatic_entries();
    }

    pub fn actor_mutating(&self) -> bool {
        self.runtime_state.actor_mutating.load(Ordering::Acquire)
    }

    pub fn kill_switch_active(&self) -> bool {
        self.kill_switch_status().0
    }

    /// Read the switch and its cause from one coherent state transition.
    pub fn kill_switch_status(&self) -> (bool, Option<super::runtime_state::KillSwitchCause>) {
        let status = self.kill_switch_state();
        (status.active, status.cause.clone())
    }

    /// Engage or clear the kill switch without recording a cause.
    ///
    /// Prefer `halt_trading`, which records one. This exists for the operator
    /// endpoint, where the cause is simply that a person asked.
    pub fn set_kill_switch(&self, active: bool) {
        let mut status = self.kill_switch_state();
        status.active = active;
        if !active {
            status.cause = None;
        }
    }

    /// Halt trading and record why.
    ///
    /// Every automatic halt goes through here. An operator who finds a stopped
    /// bot can then read the cause instead of reconstructing it from logs that
    /// may have rotated.
    pub async fn halt_trading(&self, source: &str, detail: impl Into<String>) {
        let detail = detail.into();
        tracing::error!("Halting trading [{source}]: {detail}");
        self.record_halt(source, detail);
    }

    /// Freeze new entries deliberately, recording why.
    ///
    /// The shutdown sweep engages the same switch so nothing opens fresh
    /// exposure behind it. That is designed behaviour on every clean run, not a
    /// fault, so it must not log as an error or trip checks that look for one.
    /// A reader who finds the switch engaged still learns the reason.
    pub async fn freeze_entries(&self, source: &str, detail: impl Into<String>) {
        let detail = detail.into();
        tracing::info!("Freezing new entries [{source}]: {detail}");
        self.record_halt(source, detail);
    }

    fn record_halt(&self, source: &str, detail: String) {
        let mut status = self.kill_switch_state();
        status.active = true;
        status.cause = Some(super::runtime_state::KillSwitchCause {
            source: source.to_string(),
            detail,
            at: chrono::Utc::now().to_rfc3339(),
        });
    }

    fn kill_switch_state(
        &self,
    ) -> std::sync::MutexGuard<'_, super::runtime_state::KillSwitchStatus> {
        self.runtime_state
            .kill_switch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn degraded_no_trade_active(&self) -> bool {
        self.runtime_state.degraded_no_trade.load(Ordering::SeqCst)
    }

    pub(crate) fn entries_frozen(&self) -> bool {
        self.kill_switch_active()
            || self.degraded_no_trade_active()
            || *self.runtime_state.shutdown.borrow()
    }

    pub fn set_degraded_no_trade(&self, active: bool) {
        let was = self
            .runtime_state
            .degraded_no_trade
            .swap(active, Ordering::SeqCst);
        if was != active {
            metrics::inc_degraded_mode_transitions();
        }
    }

    pub fn l2_feed_stale(&self) -> bool {
        self.runtime_state.l2_feed_stale.load(Ordering::SeqCst)
    }

    pub async fn l2_feed_age_ms(&self) -> Option<u64> {
        let last = *self.runtime_state.l2_last_event_at.read().await;
        last.map(|ts| ts.elapsed().as_millis() as u64)
    }

    pub async fn set_l2_feed_stale(&self, stale: bool) {
        self.runtime_state
            .l2_feed_stale
            .store(stale, Ordering::SeqCst);
    }

    pub async fn mark_l2_feed_event(&self) {
        self.runtime_state
            .l2_feed_stale
            .store(false, Ordering::SeqCst);
        *self.runtime_state.l2_last_event_at.write().await = Some(Instant::now());
    }

    pub async fn drain_pending_bbo_updates(&self) -> Vec<TopOfBook> {
        let mut g = self.execution_state.pending_bbo.lock().await;
        let out = g.values().cloned().collect::<Vec<_>>();
        g.clear();
        self.execution_state
            .bbo_flush_enqueued
            .store(false, Ordering::SeqCst);
        out
    }

    pub async fn mark_analysis_fresh(&self) {
        *self.analysis_state.last_analysis_at.write().await = Instant::now();
    }

    pub async fn start_execution_actor(self: &Arc<Self>) {
        let mut task_slot = self.execution_state.execution_task.lock().await;
        if task_slot.is_some() {
            return;
        }
        let handle = super::super::actor::spawn_execution_actor(Arc::clone(self));
        *self.execution_state.execution_tx.write().await = Some(handle.sender);
        *task_slot = Some(handle.task);
    }

    /// Stop accepting execution commands, drain commands already accepted by
    /// the actor, and wait until the actor releases its state and database
    /// handles. This gives restart tests and orderly shutdown a real ownership
    /// boundary instead of relying on `drop(Arc<TradingBot>)`.
    pub async fn stop_execution_actor(&self) -> anyhow::Result<()> {
        let mut task_slot = self.execution_state.execution_task.lock().await;
        // Removing the sole long-lived sender closes the channel after any
        // in-flight caller releases its short-lived clone. The actor continues
        // processing the channel and its priority lanes before exiting.
        self.execution_state.execution_tx.write().await.take();

        let Some(mut task) = task_slot.take() else {
            return Ok(());
        };

        match tokio::time::timeout(std::time::Duration::from_secs(10), &mut task).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(message))) => {
                self.set_degraded_no_trade(true);
                self.set_kill_switch(true);
                self.record_reconcile_error("execution_actor_shutdown", &message)
                    .await;
                anyhow::bail!(message)
            }
            Ok(Err(error)) => {
                self.set_degraded_no_trade(true);
                self.set_kill_switch(true);
                let message = format!("execution actor join failed: {error}");
                self.record_reconcile_error("execution_actor_shutdown", &message)
                    .await;
                anyhow::bail!(message)
            }
            Err(_) => {
                task.abort();
                let _ = task.await;
                self.set_degraded_no_trade(true);
                self.set_kill_switch(true);
                let message = "execution actor did not drain within 10 seconds";
                self.record_reconcile_error("execution_actor_shutdown", message)
                    .await;
                anyhow::bail!(message)
            }
        }
    }

    /// Type-safe accessor for the `time_remaining_minutes` field from the
    /// last analysis JSON. Returns `f64::MAX` when no analysis is available
    /// or the field is missing, so callers can safely compare against thresholds.
    pub async fn time_remaining_minutes(&self) -> f64 {
        self.analysis_state
            .last_analysis
            .read()
            .await
            .get("time_remaining_minutes")
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::MAX)
    }

    pub async fn analysis_age_secs(&self) -> u64 {
        self.analysis_state
            .last_analysis_at
            .read()
            .await
            .elapsed()
            .as_secs()
    }

    pub async fn is_analysis_stale(&self) -> bool {
        let max_age = {
            let cfg = self.control_plane.config.read().await;
            cfg.analysis_stale_seconds.max(1)
        };
        self.analysis_age_secs().await > max_age
    }

    /// How old the cached analysis is, and whether the server considers that
    /// too old, computed here rather than from a timestamp the client would
    /// have to diff against its own clock.
    ///
    /// Every value derived from one analysis cycle shares this age. Without it
    /// a figure computed a minute ago is drawn exactly like one computed a
    /// moment ago, which is indistinguishable from the dashboard being broken.
    pub async fn analysis_freshness(&self) -> serde_json::Value {
        let max_age = {
            let cfg = self.control_plane.config.read().await;
            cfg.analysis_stale_seconds.max(1)
        };
        let age = self.analysis_age_secs().await;
        let has_analysis = !self.analysis_state.last_analysis.read().await.is_null();
        serde_json::json!({
            "age_secs": if has_analysis { Some(age) } else { None },
            "max_age_secs": max_age,
            "stale": has_analysis && age > max_age,
            "never_run": !has_analysis,
            "interval_secs": self.control_plane.config.read().await.check_interval,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::TradingBot;
    use crate::config::Config;
    use crate::db;
    use crate::market::l2_book::TopOfBook;
    use serde_json::json;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    async fn test_bot(name: &str) -> anyhow::Result<(Arc<TradingBot>, std::path::PathBuf, String)> {
        let uniq = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_runtime_{name}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../../config.json"))?;
        cfg.mode = "simulation".to_string();
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        Ok((bot, path, path_str))
    }

    #[tokio::test]
    async fn closed_eviction_retains_uncertain_closing_exposure_and_its_prices(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("closing_eviction").await?;
        *bot.portfolio_state.trades.write().await = vec![
            db::Trade {
                id: 1,
                status: "CLOSING".into(),
                shares: 5.0,
                amount: 2.0,
                ..Default::default()
            },
            db::Trade {
                id: 2,
                status: "CLOSED".into(),
                ..Default::default()
            },
        ];
        for id in [1, 2] {
            bot.trade_lifecycle
                .scalp_high_water
                .write()
                .await
                .insert(id, 0.6);
            bot.trade_lifecycle.last_trade_prices.write().await.insert(
                id,
                super::super::trade_lifecycle::CachedTradePrice {
                    price: 0.6,
                    observed_at: Instant::now(),
                },
            );
        }
        bot.evict_closed_trades().await;
        assert_eq!(
            bot.trades()
                .await
                .iter()
                .map(|trade| trade.id)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert!(bot
            .trade_lifecycle
            .last_trade_prices
            .read()
            .await
            .contains_key(&1));
        assert!(!bot
            .trade_lifecycle
            .last_trade_prices
            .read()
            .await
            .contains_key(&2));
        assert!(bot
            .trade_lifecycle
            .scalp_high_water
            .read()
            .await
            .contains_key(&1));
        assert!(!bot
            .trade_lifecycle
            .scalp_high_water
            .read()
            .await
            .contains_key(&2));
        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn a_reconcile_error_becomes_the_halt_cause_when_none_is_set() -> anyhow::Result<()> {
        // Seventeen code paths halt trading and then record a reconcile error
        // on the next line. Adopting that error is what lets those paths say
        // why they stopped without editing every one of them.
        let (bot, path, path_str) = test_bot("halt_cause_adopt").await?;

        bot.set_kill_switch(true);
        assert!(
            bot.kill_switch_status().1.is_none(),
            "no cause before one is recorded"
        );

        bot.record_reconcile_error("live_reconcile", "repair failed for trade 7")
            .await;

        let cause = bot
            .kill_switch_status()
            .1
            .expect("the reconcile error should become the cause");
        assert_eq!(cause.source, "live_reconcile");
        assert_eq!(cause.detail, "repair failed for trade 7");

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn adoption_never_overwrites_a_more_specific_cause() -> anyhow::Result<()> {
        // halt_trading names the precise reason. A reconcile error recorded
        // afterwards is usually the same event described more loosely, and
        // must not replace it.
        let (bot, path, path_str) = test_bot("halt_cause_keep").await?;

        bot.halt_trading("slo_l2_breach", "l2 stale for 30s").await;
        bot.record_reconcile_error("live_reconcile", "something vaguer")
            .await;

        let cause = bot.kill_switch_status().1.expect("a cause was set");
        assert_eq!(
            cause.source, "slo_l2_breach",
            "the specific cause must survive"
        );
        assert_eq!(cause.detail, "l2 stale for 30s");

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn a_reconcile_error_while_trading_is_permitted_sets_no_halt_cause() -> anyhow::Result<()>
    {
        // Errors are recorded during normal operation too. One of those must
        // not invent a halt reason for a bot that is running fine.
        let (bot, path, path_str) = test_bot("halt_cause_none").await?;

        bot.record_reconcile_error("live_reconcile", "transient blip")
            .await;

        assert!(!bot.kill_switch_active());
        assert!(
            bot.kill_switch_status().1.is_none(),
            "a reconcile error must not imply a halt that never happened"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn a_deliberate_freeze_is_not_reported_as_a_fault() -> anyhow::Result<()> {
        // The shutdown sweep engages the same switch on every clean run. It
        // records a cause like any halt, but it is not an error and must not be
        // logged or counted as one, or every clean shutdown looks like a
        // failure.
        let (bot, path, path_str) = test_bot("freeze_not_fault").await?;

        bot.freeze_entries("shutdown_sweep", "entries frozen while closing")
            .await;

        assert!(bot.kill_switch_active(), "the switch is still engaged");
        let cause = bot
            .kill_switch_status()
            .1
            .expect("a freeze still records why");
        assert_eq!(cause.source, "shutdown_sweep");

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn clearing_the_switch_clears_the_cause() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("halt_cause_clear").await?;

        bot.halt_trading("operator", "engaged from the dashboard")
            .await;
        assert!(bot.kill_switch_status().1.is_some());

        bot.set_kill_switch(false);
        assert!(
            bot.kill_switch_status().1.is_none(),
            "a cleared switch must not keep reporting why it was once engaged"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_halt_and_clear_publish_a_coherent_snapshot() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("halt_clear_race").await?;

        for attempt in 0..1_000 {
            bot.set_kill_switch(false);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let halt_bot = Arc::clone(&bot);
            let halt_barrier = Arc::clone(&barrier);
            let clear_bot = Arc::clone(&bot);

            std::thread::scope(|scope| {
                scope.spawn(move || {
                    halt_barrier.wait();
                    halt_bot.record_halt("race_test", format!("attempt {attempt}"));
                });
                scope.spawn(move || {
                    barrier.wait();
                    clear_bot.set_kill_switch(false);
                });
            });

            let (active, cause) = bot.kill_switch_status();
            assert_eq!(
                active,
                cause.is_some(),
                "attempt {attempt} exposed a switch/cause mismatch"
            );
        }

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    fn cleanup_db(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    fn bbo(token_id: &str, best_bid: f64, seq: u64) -> TopOfBook {
        TopOfBook {
            market_id: "mkt-test".to_string(),
            token_id: token_id.to_string(),
            is_yes_token: Some(true),
            best_bid: Some(best_bid),
            best_bid_size: Some(10.0),
            best_ask: Some(best_bid + 0.01),
            best_ask_size: Some(12.0),
            spread_bps: Some(100.0),
            sequence: Some(seq),
            snapshot_loaded: true,
            stale: false,
            updated_at: "2026-03-20T00:00:00Z".to_string(),
        }
    }

    #[tokio::test]
    async fn loop_control_and_safety_flags_round_trip() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("loop_flags").await?;

        assert!(!bot.loop_running());
        assert!(!bot.kill_switch_active());
        assert!(!bot.degraded_no_trade_active());

        let notified = bot.runtime_state.loop_notify.notified();
        bot.start_loop();
        tokio::time::timeout(Duration::from_millis(50), notified).await?;
        assert!(bot.loop_running());

        bot.set_kill_switch(true);
        bot.set_degraded_no_trade(true);
        assert!(bot.kill_switch_active());
        assert!(bot.degraded_no_trade_active());

        bot.stop_loop();
        bot.set_kill_switch(false);
        bot.set_degraded_no_trade(false);
        assert!(!bot.loop_running());
        assert!(!bot.kill_switch_active());
        assert!(!bot.degraded_no_trade_active());

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn mark_l2_feed_event_clears_stale_and_refreshes_age() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("l2_refresh").await?;

        bot.set_l2_feed_stale(true).await;
        assert!(bot.l2_feed_stale());
        assert!(bot.l2_feed_age_ms().await.is_none());

        bot.mark_l2_feed_event().await;

        assert!(!bot.l2_feed_stale());
        let age_ms = bot.l2_feed_age_ms().await.unwrap_or(u64::MAX);
        assert!(age_ms < 250, "expected fresh l2 age, got {age_ms}ms");

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn drain_pending_bbo_updates_coalesces_and_clears_flush_flag() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("bbo_drain").await?;

        {
            let mut pending = bot.execution_state.pending_bbo.lock().await;
            pending.insert("tok-a".to_string(), bbo("tok-a", 0.40, 1));
            pending.insert("tok-a".to_string(), bbo("tok-a", 0.55, 2));
            pending.insert("tok-b".to_string(), bbo("tok-b", 0.25, 3));
        }
        bot.execution_state
            .bbo_flush_enqueued
            .store(true, Ordering::SeqCst);

        let drained = bot.drain_pending_bbo_updates().await;
        assert_eq!(drained.len(), 2);
        assert!(drained.iter().any(|bbo| {
            bbo.token_id == "tok-a" && bbo.best_bid == Some(0.55) && bbo.sequence == Some(2)
        }));
        assert!(drained.iter().any(|bbo| bbo.token_id == "tok-b"));
        assert!(!bot
            .execution_state
            .bbo_flush_enqueued
            .load(Ordering::SeqCst));
        assert!(bot.drain_pending_bbo_updates().await.is_empty());

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn analysis_freshness_and_time_remaining_accessors_are_consistent() -> anyhow::Result<()>
    {
        let (bot, path, path_str) = test_bot("analysis_age").await?;

        {
            let mut cfg = bot.control_plane.config.write().await;
            cfg.analysis_stale_seconds = 1;
        }
        *bot.analysis_state.last_analysis_at.write().await =
            Instant::now() - Duration::from_secs(3);

        assert_eq!(bot.time_remaining_minutes().await, f64::MAX);
        assert!(bot.analysis_age_secs().await >= 3);
        assert!(bot.is_analysis_stale().await);

        *bot.analysis_state.last_analysis.write().await = json!({
            "time_remaining_minutes": 12.5,
        });
        bot.mark_analysis_fresh().await;

        assert_eq!(bot.time_remaining_minutes().await, 12.5);
        assert!(bot.analysis_age_secs().await <= 1);
        assert!(!bot.is_analysis_stale().await);

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn reconcile_summary_overwrites_error_and_stores_latest_state() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("reconcile_summary").await?;

        bot.record_reconcile_error("full_resync", "db unavailable")
            .await;
        let err = bot
            .last_reconcile_error()
            .await
            .expect("reconcile error should be recorded");
        assert_eq!(err.phase, "full_resync");
        assert!(err.error.contains("db unavailable"));

        bot.record_reconcile_summary(crate::bot::state::ReconcileSummary {
            phase: "full_resync".to_string(),
            completed_at: "2026-03-20T00:00:00Z".to_string(),
            drift_count: 2,
            repaired_count: 2,
            orphan_local_closes: 1,
            repaired_local_trades: 1,
            restored_local_trades: 1,
            closed_local_ghosts: 0,
            missing_on_exchange: 0,
            orphan_on_exchange: 0,
            allowed_drift: 1,
            kill_switch_active: true,
        })
        .await;

        assert!(bot.last_reconcile_error().await.is_none());
        let summary = bot
            .last_reconcile_summary()
            .await
            .expect("reconcile summary should be recorded");
        assert_eq!(summary.phase, "full_resync");
        assert_eq!(summary.repaired_count, 2);
        assert_eq!(summary.orphan_local_closes, 1);
        assert!(summary.kill_switch_active);

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }
}
