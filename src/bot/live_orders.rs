//! Recovery of durable taker intents from confirmed account matches.

use anyhow::{anyhow, ensure, Context, Result};
use std::future::Future;
use std::time::Duration;
use tracing::warn;

use super::state::TradingBot;
use crate::{db, domain::TradeStatus};

const RECOVERY_READ_BUDGET: Duration = Duration::from_secs(20);

/// One deadline covers the complete exchange-read phase. Keep transactions
/// and hot-state publication outside this cancellation boundary: a completed
/// receipt commit must still be published even if the next read cannot finish.
pub(super) async fn before_recovery_read_deadline<T>(
    deadline: tokio::time::Instant,
    operation: &'static str,
    read: impl Future<Output = Result<T>>,
) -> Result<T> {
    ensure!(
        tokio::time::Instant::now() < deadline,
        "{operation} read deadline exceeded; durable execution intents retained"
    );
    tokio::time::timeout_at(deadline, read)
        .await
        .with_context(|| {
            format!("{operation} read deadline exceeded; durable execution intents retained")
        })?
}

impl TradingBot {
    pub(crate) async fn publish_live_trade_state(&self, trade: db::Trade) {
        let id = trade.id;
        let mut trades = self.portfolio_state.trades.write().await;
        if let Some(current) = trades.iter_mut().find(|t| t.id == id) {
            *current = trade.clone();
        } else if trade.has_unsettled_exposure() {
            trades.push(trade.clone());
        }
        drop(trades);
        if trade.status_kind() != TradeStatus::Closing {
            self.trade_lifecycle
                .closing_in_progress
                .write()
                .await
                .remove(&id);
        }
    }

    /// Read-only exchange recovery. A timeout, missing order or incomplete page
    /// cannot justify replaying a POST or assuming that a position has closed.
    pub(crate) async fn reconcile_pending_live_orders(&self) -> Result<()> {
        self.reconcile_pending_live_orders_with_read_budget(RECOVERY_READ_BUDGET)
            .await
    }

    async fn reconcile_pending_live_orders_with_read_budget(&self, budget: Duration) -> Result<()> {
        ensure!(
            db::live::audit_uncertain_count(&self.db).await? == 0,
            "terminal taker evidence is audit-uncertain; signed operator recheck is required"
        );
        let orders = db::live::pending_orders(&self.db).await?;
        if orders.is_empty() {
            return Ok(());
        }
        let clob = self
            .clob
            .as_ref()
            .ok_or_else(|| anyhow!("live intents require exchange credentials"))?;
        let deadline = tokio::time::Instant::now() + budget;
        let history =
            before_recovery_read_deadline(deadline, "taker history", clob.list_trades()).await?;
        let verifier = clob.settlement_verifier()?;
        for order in orders {
            let partial_request_id = self
                .trade_lifecycle
                .partial_close_intents
                .read()
                .await
                .get(&order.trade_id)
                .filter(|intent| {
                    order.phase == "exit"
                        && intent.live_order_id.as_deref() == Some(order.order_id.as_str())
                })
                .map(|intent| intent.request_id);
            if !order.account.eq_ignore_ascii_case(&clob.account_address()) {
                return Err(anyhow!(
                    "pending live order belongs to a different funding wallet"
                ));
            }
            let info = before_recovery_read_deadline(
                deadline,
                "taker order status",
                clob.get_order(&order.order_id),
            )
            .await?;
            let txs: std::collections::HashSet<_> = history
                .iter()
                .filter(|m| m.taker_order_id == order.order_id && m.is_confirmed())
                .map(|m| {
                    m.transaction_hash.as_deref().ok_or_else(|| {
                        anyhow!("confirmed match is missing its settlement transaction")
                    })
                })
                .collect::<Result<_>>()?;
            let mut fills = Vec::new();
            for tx in txs {
                fills.extend(
                    before_recovery_read_deadline(
                        deadline,
                        "taker settlement receipt",
                        verifier.verify(tx, &order.identity),
                    )
                    .await?,
                );
            }
            let updated =
                db::live::reconcile_order(&self.db, &order, &info, &history, &fills).await?;
            self.publish_live_trade_state(updated).await;
            if partial_request_id.is_some() {
                let state: String =
                    sqlx::query_scalar("SELECT state FROM live_orders WHERE order_id=?")
                        .bind(&order.order_id)
                        .fetch_one(&self.db)
                        .await?;
                let outcome = match state.as_str() {
                    "settled" => Some(serde_json::json!({"ok":true,"status":"settled"})),
                    "rejected" => Some(serde_json::json!({"ok":false,"status":"rejected"})),
                    _ => None,
                };
                self.finish_partial_close_request(order.trade_id, partial_request_id, &outcome)
                    .await;
            }
        }
        Ok(())
    }

    pub(crate) async fn recover_live_orders_or_halt(&self) -> bool {
        self.finish_live_recovery(self.reconcile_pending_live_orders().await)
            .await
    }

    #[cfg(test)]
    async fn recover_live_orders_or_halt_with_read_budget(&self, budget: Duration) -> bool {
        self.finish_live_recovery(
            self.reconcile_pending_live_orders_with_read_budget(budget)
                .await,
        )
        .await
    }

    async fn finish_live_recovery(&self, result: Result<()>) -> bool {
        match result {
            Ok(()) => true,
            Err(error) => {
                warn!("Live order recovery needs more exchange evidence: {error}");
                self.set_degraded_no_trade(true);
                self.halt_trading("live_order_reconciliation_pending", error.to_string())
                    .await;
                self.record_reconcile_error("live_order_reconciliation_pending", error.to_string())
                    .await;
                false
            }
        }
    }
}

#[cfg(test)]
#[path = "live_orders_tests.rs"]
mod tests;
