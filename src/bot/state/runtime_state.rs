use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Instant;

use serde::Serialize;
use tokio::sync::{Mutex, Notify, RwLock};

use super::runtime_slo::SloGuardState;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReconcileSummary {
    pub phase: String,
    pub completed_at: String,
    pub drift_count: usize,
    pub repaired_count: usize,
    pub orphan_local_closes: usize,
    pub repaired_local_trades: usize,
    pub restored_local_trades: usize,
    pub closed_local_ghosts: usize,
    pub missing_on_exchange: usize,
    pub orphan_on_exchange: usize,
    pub allowed_drift: usize,
    pub kill_switch_active: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReconcileError {
    pub phase: String,
    pub timestamp: String,
    pub error: String,
}

/// A recorded cause for the kill switch being engaged.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KillSwitchCause {
    /// Short stable identifier for the code path that halted trading.
    pub source: String,
    /// Human-readable detail, usually the underlying error.
    pub detail: String,
    pub at: String,
}

#[derive(Debug, Clone, Default)]
pub(super) struct KillSwitchStatus {
    pub(super) active: bool,
    pub(super) cause: Option<KillSwitchCause>,
}

pub struct RuntimeState {
    /// Set at process startup; the feed-free demo cannot start an analysis or
    /// execution loop through dashboard controls.
    pub backtest_only: AtomicBool,
    pub loop_running: AtomicBool,
    /// Stop/disable invalidates automatic work already queued or validating.
    pub automatic_entry_generation: AtomicU64,
    /// The switch and its cause share one lock so readers can never observe a
    /// flag from one transition and a cause from another.
    pub(super) kill_switch: std::sync::Mutex<KillSwitchStatus>,
    pub degraded_no_trade: AtomicBool,
    pub l2_feed_stale: AtomicBool,
    pub l2_last_event_at: RwLock<Option<Instant>>,
    pub loop_notify: Notify,
    pub(super) slo_guard_state: Mutex<SloGuardState>,
    pub last_balance_drift_usd: RwLock<Option<f64>>,
    pub last_reconcile_summary: RwLock<Option<ReconcileSummary>>,
    pub last_reconcile_error: RwLock<Option<ReconcileError>>,
    /// True while the execution actor is processing a trade mutation
    /// (execute, close, close-pair). The integrity checker reads this to
    /// suppress false-positive drift reports caused by non-atomic updates.
    pub actor_mutating: AtomicBool,
    /// Flips to true once the process has been told to stop. The SSE and
    /// WebSocket streams watch it and end themselves, because an open stream
    /// is an in-flight response that graceful shutdown would otherwise wait on
    /// forever — and the position sweep runs after that wait.
    pub(super) shutdown: tokio::sync::watch::Sender<bool>,
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self {
            backtest_only: AtomicBool::new(false),
            loop_running: AtomicBool::new(false),
            automatic_entry_generation: AtomicU64::new(0),
            kill_switch: std::sync::Mutex::new(KillSwitchStatus::default()),
            degraded_no_trade: AtomicBool::new(false),
            l2_feed_stale: AtomicBool::new(true),
            l2_last_event_at: RwLock::new(None),
            loop_notify: Notify::new(),
            slo_guard_state: Mutex::new(SloGuardState::default()),
            last_balance_drift_usd: RwLock::new(None),
            last_reconcile_summary: RwLock::new(None),
            last_reconcile_error: RwLock::new(None),
            actor_mutating: AtomicBool::new(false),
            shutdown: tokio::sync::watch::channel(false).0,
        }
    }
}

impl RuntimeState {
    /// Tell every long-lived response to finish.
    pub fn begin_shutdown(&self) {
        self.shutdown.send_replace(true);
    }

    /// A receiver that resolves once shutdown has begun. Cheap to clone; one
    /// per stream.
    pub fn shutdown_watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }
}

/// Resolves once the watch reports shutdown, or immediately if it already has.
pub async fn shutdown_begun(mut rx: tokio::sync::watch::Receiver<bool>) {
    while !*rx.borrow_and_update() {
        if rx.changed().await.is_err() {
            // The sender is gone: the process is going away with it.
            return;
        }
    }
}

/// Drive `server` to completion, but never past `limit` after shutdown begins.
///
/// Graceful shutdown waits for every in-flight response, and the streams end
/// themselves on the signal — but a client that ignores that, or a response
/// this code has not thought of, must not be able to stand between the signal
/// and the position sweep. `None` means the deadline won, and the server
/// future was dropped, which closes the listener.
pub async fn drain_or_deadline<F, T>(
    server: F,
    rx: tokio::sync::watch::Receiver<bool>,
    limit: std::time::Duration,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    tokio::select! {
        out = server => Some(out),
        () = async {
            shutdown_begun(rx).await;
            tokio::time::sleep(limit).await;
        } => None,
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn shutdown_begun_resolves_when_the_switch_flips_and_not_before() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let waiter = tokio::spawn(shutdown_begun(rx));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!waiter.is_finished(), "resolved before shutdown began");
        tx.send_replace(true);
        tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("did not resolve after shutdown began")
            .expect("join");
    }

    #[tokio::test]
    async fn shutdown_begun_resolves_immediately_if_already_begun() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send_replace(true);
        tokio::time::timeout(Duration::from_millis(50), shutdown_begun(rx))
            .await
            .expect("a late subscriber must not wait for a change that already happened");
    }

    /// The production failure: an SSE response never ends, graceful shutdown
    /// waits for it, and the position sweep after it never runs — while the
    /// bot's own tasks keep trading. The drain must have a deadline.
    #[tokio::test]
    async fn a_response_that_never_ends_cannot_hold_shutdown_open() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        let never_drains = std::future::pending::<()>();
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            tx.send_replace(true);
        });
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            drain_or_deadline(never_drains, rx, Duration::from_millis(50)),
        )
        .await
        .expect("the deadline never fired; shutdown would hang forever");
        assert_eq!(out, None, "a never-ending drain must lose to the deadline");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn a_server_that_drains_promptly_is_not_cut_off() {
        let (tx, rx) = tokio::sync::watch::channel(false);
        tx.send_replace(true);
        let out = drain_or_deadline(async { 42 }, rx, Duration::from_secs(5)).await;
        assert_eq!(out, Some(42));
    }
}
