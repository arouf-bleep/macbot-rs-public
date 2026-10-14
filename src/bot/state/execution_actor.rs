use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::analysis::AnalysisResult as ParsedAnalysis;
use crate::bot::close::CloseIntent;
use crate::db;
use crate::market::l2_book::{TopOfBook, VenueRules};
use crate::metrics;

use super::{ActorCallError, TradingBot};

/// Maximum time to wait for the actor to respond to a high-priority command
/// (trade execution, close). If exceeded, the actor is assumed hung and the
/// kill switch is activated.
const ACTOR_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const ACTOR_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorSendError {
    Closed,
    TimedOut,
}

async fn send_command_with_timeout<T>(
    tx: &mpsc::Sender<T>,
    cmd: T,
) -> std::result::Result<(), ActorSendError> {
    match tokio::time::timeout(ACTOR_SEND_TIMEOUT, tx.send(cmd)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(ActorSendError::Closed),
        Err(_) => Err(ActorSendError::TimedOut),
    }
}

impl TradingBot {
    pub(crate) async fn cancel_maker_orders_for_shutdown_serialized(&self) -> anyhow::Result<()> {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            anyhow::bail!("execution actor unavailable for shutdown order cleanup");
        };
        let (resp, receiver) = oneshot::channel();
        send_command_with_timeout(
            &tx,
            super::super::actor::ExecutionCommand::CancelMakerOrdersForShutdown { resp },
        )
        .await
        .map_err(|error| anyhow::anyhow!("shutdown cancellation dispatch failed: {error:?}"))?;
        // One cancellation read budget plus one taker-recovery read budget.
        // A lost reply means unknown outcome, never permission to release reservations.
        match tokio::time::timeout(Duration::from_secs(45), receiver).await {
            Ok(Ok(result)) => result.map_err(anyhow::Error::msg),
            _ => anyhow::bail!(
                "shutdown order cleanup response unavailable; inspect durable intents"
            ),
        }
    }

    pub(crate) async fn finish_partial_close_request(
        &self,
        trade_id: i64,
        request_id: Option<u64>,
        result: &Option<Value>,
    ) {
        let Some(request_id) = request_id else { return };
        let known = result.as_ref().is_some_and(|value| {
            value["status"] == "settled" && value["ok"] == true
                || value["status"] == "rejected" && value["ok"] == false
        });
        let mut intents = self.trade_lifecycle.partial_close_intents.write().await;
        if let Some(intent) = intents.get_mut(&trade_id) {
            if intent.request_id == request_id {
                if known {
                    intents.remove(&trade_id);
                } else {
                    intent.outcome_unknown = true;
                }
            }
        }
    }

    async fn forget_undispatched_partial_close(&self, trade_id: i64, request_id: u64) {
        let mut intents = self.trade_lifecycle.partial_close_intents.write().await;
        if intents
            .get(&trade_id)
            .is_some_and(|intent| intent.request_id == request_id)
        {
            intents.remove(&trade_id);
        }
    }

    pub async fn reconcile_redemption_serialized(
        &self,
        request: crate::clob::redemption::RedemptionRequest,
    ) -> anyhow::Result<Value> {
        request.validate()?;
        let (resp, receiver) = oneshot::channel();
        self.send_reset_command(super::super::actor::ExecutionCommand::ReconcileRedemption {
            request,
            resp,
        })
        .await?;
        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            _ => {
                self.halt_trading("redemption_reconciliation_outcome_unknown","inspect the receipt ledger before retrying; replay the same transaction and exact trade IDs only").await;
                anyhow::bail!("redemption reconciliation outcome unknown; replay only the same transaction and exact trade IDs")
            }
        }
    }
    pub async fn reset_paper_balance_serialized(
        &self,
        amount: Option<f64>,
        force: bool,
    ) -> anyhow::Result<Value> {
        let (resp, receiver) = oneshot::channel();
        self.send_reset_command(super::super::actor::ExecutionCommand::ResetPaperBalance {
            amount,
            force,
            resp,
        })
        .await?;
        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result.map_err(anyhow::Error::msg),
            _ => {
                self.halt_trading("execution_actor", "paper balance reset outcome unknown")
                    .await;
                anyhow::bail!("paper balance reset outcome unknown; inspect state before retrying")
            }
        }
    }

    pub async fn reset_drawdown_cash_peak(&self) -> anyhow::Result<f64> {
        let (resp, receiver) = oneshot::channel();
        self.send_reset_command(
            super::super::actor::ExecutionCommand::ResetDrawdownCashPeak { resp },
        )
        .await?;
        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result.map_err(anyhow::Error::msg),
            _ => {
                self.halt_trading("execution_actor", "cash peak reset outcome unknown")
                    .await;
                anyhow::bail!("cash peak reset outcome unknown; inspect state before retrying")
            }
        }
    }

    async fn send_reset_command(
        &self,
        command: super::super::actor::ExecutionCommand,
    ) -> anyhow::Result<()> {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            anyhow::bail!("execution actor unavailable")
        };
        send_command_with_timeout(&tx, command)
            .await
            .map_err(|error| anyhow::anyhow!("execution actor reset dispatch failed: {error:?}"))
    }
    pub(super) async fn partial_close_trade_serialized(
        &self,
        trade_id: i64,
        exit_price: f64,
        fraction: f64,
    ) -> Result<Option<Value>, ActorCallError> {
        self.partial_close_trade_serialized_with_intent(
            trade_id,
            exit_price,
            fraction,
            CloseIntent::Market,
        )
        .await
    }

    pub async fn partial_close_trade_serialized_with_intent(
        &self,
        trade_id: i64,
        exit_price: f64,
        fraction: f64,
        intent: CloseIntent,
    ) -> Result<Option<Value>, ActorCallError> {
        let request_id = {
            let mut intents = self.trade_lifecycle.partial_close_intents.write().await;
            if intents.contains_key(&trade_id) {
                return Ok(Some(json!({"ok":false,"id":trade_id,"status":"pending",
                    "reason":"an earlier partial sale is still in flight or unresolved; inspect its evidence before another sale"})));
            }
            let Ok(previous) = self.trade_lifecycle.partial_close_sequence.fetch_update(
                Ordering::SeqCst,
                Ordering::SeqCst,
                |value| value.checked_add(1),
            ) else {
                return Ok(Some(json!({"ok":false,"id":trade_id,"status":"pending",
                    "reason":"partial request identity exhausted; stop and restart before requesting another sale"})));
            };
            let request_id = previous + 1;
            intents.insert(
                trade_id,
                super::trade_lifecycle::PartialCloseIntent {
                    request_id,
                    outcome_unknown: false,
                    live_order_id: None,
                },
            );
            request_id
        };
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            self.forget_undispatched_partial_close(trade_id, request_id)
                .await;
            self.halt_trading("execution_actor", "unavailable for partial close")
                .await;
            return Err(ActorCallError::Unavailable);
        };
        let (resp, receiver) = oneshot::channel();
        if send_command_with_timeout(
            &tx,
            super::super::actor::ExecutionCommand::PartialCloseTrade {
                trade_id,
                exit_price,
                fraction,
                intent,
                request_id: Some(request_id),
                resp,
            },
        )
        .await
        .is_err()
        {
            self.forget_undispatched_partial_close(trade_id, request_id)
                .await;
            self.halt_trading("execution_actor", "send failed for partial close")
                .await;
            return Err(ActorCallError::SendFailed);
        }
        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, receiver).await {
            Ok(Ok(result)) => {
                self.finish_partial_close_request(trade_id, Some(request_id), &result)
                    .await;
                Ok(result)
            }
            _ => {
                self.finish_partial_close_request(trade_id, Some(request_id), &None)
                    .await;
                self.halt_trading("execution_actor", "partial close response unknown")
                    .await;
                Err(ActorCallError::ResponseFailed)
            }
        }
    }
    #[cfg(test)]
    pub async fn execute_trade_serialized(
        &self,
        analysis: &serde_json::Value,
        hail_mary: bool,
        override_amount: Option<f64>,
    ) -> Option<crate::db::Trade> {
        self.execute_trade_serialized_result(analysis, hail_mary, override_amount)
            .await
            .ok()
            .flatten()
    }

    pub async fn execute_trade_serialized_parsed(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
    ) -> Option<crate::db::Trade> {
        self.execute_trade_serialized_strategy(analysis, hail_mary, override_amount, "conviction")
            .await
            .ok()
            .flatten()
    }

    pub async fn execute_scalp_serialized(
        &self,
        analysis: &ParsedAnalysis,
        override_amount: Option<f64>,
    ) -> Option<crate::db::Trade> {
        self.execute_trade_serialized_strategy(analysis, false, override_amount, "scalp")
            .await
            .ok()
            .flatten()
    }

    pub async fn execute_trade_serialized_result(
        &self,
        analysis: &serde_json::Value,
        hail_mary: bool,
        override_amount: Option<f64>,
    ) -> std::result::Result<Option<crate::db::Trade>, ActorCallError> {
        let parsed = ParsedAnalysis::from_value(analysis);
        self.execute_trade_serialized_strategy(&parsed, hail_mary, override_amount, "conviction")
            .await
    }

    pub async fn execute_trade_serialized_strategy(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
    ) -> std::result::Result<Option<crate::db::Trade>, ActorCallError> {
        self.execute_trade_serialized_for_origin(
            analysis,
            hail_mary,
            override_amount,
            strategy,
            super::super::actor::ExecutionOrigin::Manual,
        )
        .await
    }

    pub(crate) async fn execute_automatic_trade_serialized_strategy(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: super::super::actor::ExecutionOrigin,
    ) -> std::result::Result<Option<crate::db::Trade>, ActorCallError> {
        if !self.entry_origin_allowed(origin).await {
            return Ok(None);
        }
        self.execute_trade_serialized_for_origin(
            analysis,
            hail_mary,
            override_amount,
            strategy,
            origin,
        )
        .await
    }

    async fn execute_trade_serialized_for_origin(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: super::super::actor::ExecutionOrigin,
    ) -> std::result::Result<Option<crate::db::Trade>, ActorCallError> {
        let tx = match self.execution_state.execution_tx.read().await.clone() {
            Some(tx) => tx,
            None => {
                warn!("Execution actor unavailable for execute_trade; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "unavailable for execute_trade; activating kill switch",
                )
                .await;
                return Err(ActorCallError::Unavailable);
            }
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = super::super::actor::ExecutionCommand::ExecuteTrade {
            origin,
            analysis: Box::new(analysis.clone()),
            hail_mary,
            override_amount,
            strategy: strategy.to_string(),
            resp: resp_tx,
        };

        match send_command_with_timeout(&tx, cmd).await {
            Ok(()) => {}
            Err(ActorSendError::Closed) => {
                warn!("Execution actor send failed for execute_trade; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "send failed for execute_trade; activating kill switch",
                )
                .await;
                return Err(ActorCallError::SendFailed);
            }
            Err(ActorSendError::TimedOut) => {
                warn!(
                    "Execution actor send timed out ({}s) for execute_trade; activating kill switch",
                    ACTOR_SEND_TIMEOUT.as_secs()
                );
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "send timed out for execute_trade; activating kill switch",
                )
                .await;
                return Err(ActorCallError::SendFailed);
            }
        }

        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => {
                warn!("Execution actor response dropped for execute_trade; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "response dropped for execute_trade; activating kill switch",
                )
                .await;
                Err(ActorCallError::ResponseFailed)
            }
            Err(_) => {
                warn!(
                    "Execution actor timed out ({}s) for execute_trade; activating kill switch",
                    ACTOR_RESPONSE_TIMEOUT.as_secs()
                );
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "timed out for execute_trade; activating kill switch",
                )
                .await;
                Err(ActorCallError::ResponseFailed)
            }
        }
    }

    pub async fn close_trade_serialized(
        &self,
        trade_id: i64,
        exit_price: Option<f64>,
    ) -> Option<crate::db::Trade> {
        self.close_trade_serialized_result(trade_id, exit_price)
            .await
            .ok()
            .flatten()
    }

    pub async fn close_trade_serialized_result(
        &self,
        trade_id: i64,
        exit_price: Option<f64>,
    ) -> std::result::Result<Option<crate::db::Trade>, ActorCallError> {
        self.close_trade_serialized_with_intent(trade_id, exit_price, CloseIntent::Market)
            .await
    }

    pub async fn close_trade_serialized_with_intent(
        &self,
        trade_id: i64,
        exit_price: Option<f64>,
        intent: CloseIntent,
    ) -> std::result::Result<Option<crate::db::Trade>, ActorCallError> {
        let tx = match self.execution_state.execution_tx.read().await.clone() {
            Some(tx) => tx,
            None => {
                warn!("Execution actor unavailable for close_trade; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "unavailable for close_trade; activating kill switch",
                )
                .await;
                return Err(ActorCallError::Unavailable);
            }
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = super::super::actor::ExecutionCommand::CloseTrade {
            trade_id,
            exit_price,
            intent,
            resp: resp_tx,
        };
        // Hold the admission read lease through queueing, so a partial intent
        // cannot be inserted between the automatic exit check and its send.
        let partial_guard = if matches!(intent, CloseIntent::Market | CloseIntent::Resolution) {
            Some(self.trade_lifecycle.partial_close_intents.read().await)
        } else {
            None
        };
        if partial_guard
            .as_ref()
            .is_some_and(|intents| intents.contains_key(&trade_id))
        {
            return Ok(None);
        }
        let dispatched = send_command_with_timeout(&tx, cmd).await;
        drop(partial_guard);
        match dispatched {
            Ok(()) => {}
            Err(ActorSendError::Closed) => {
                warn!("Execution actor send failed for close_trade; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "send failed for close_trade; activating kill switch",
                )
                .await;
                return Err(ActorCallError::SendFailed);
            }
            Err(ActorSendError::TimedOut) => {
                warn!(
                    "Execution actor send timed out ({}s) for close_trade; activating kill switch",
                    ACTOR_SEND_TIMEOUT.as_secs()
                );
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "send timed out for close_trade; activating kill switch",
                )
                .await;
                return Err(ActorCallError::SendFailed);
            }
        }

        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => {
                warn!("Execution actor response dropped for close_trade; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "response dropped for close_trade; activating kill switch",
                )
                .await;
                Err(ActorCallError::ResponseFailed)
            }
            Err(_) => {
                warn!(
                    "Execution actor timed out ({}s) for close_trade; activating kill switch",
                    ACTOR_RESPONSE_TIMEOUT.as_secs()
                );
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "timed out for close_trade; activating kill switch",
                )
                .await;
                Err(ActorCallError::ResponseFailed)
            }
        }
    }

    pub async fn close_pair_serialized(
        &self,
        first_trade_id: i64,
        second_trade_id: i64,
        exit_price: Option<f64>,
    ) -> (Option<crate::db::Trade>, Option<crate::db::Trade>) {
        self.close_pair_serialized_result(first_trade_id, second_trade_id, exit_price)
            .await
            .unwrap_or((None, None))
    }

    pub async fn close_pair_serialized_result(
        &self,
        first_trade_id: i64,
        second_trade_id: i64,
        exit_price: Option<f64>,
    ) -> std::result::Result<(Option<crate::db::Trade>, Option<crate::db::Trade>), ActorCallError>
    {
        let tx = match self.execution_state.execution_tx.read().await.clone() {
            Some(tx) => tx,
            None => {
                warn!("Execution actor unavailable for close_pair; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "unavailable for close_pair; activating kill switch",
                )
                .await;
                return Err(ActorCallError::Unavailable);
            }
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = super::super::actor::ExecutionCommand::ClosePair {
            first_trade_id,
            second_trade_id,
            exit_price,
            resp: resp_tx,
        };

        // Protect both legs until the pair command is admitted. The actor
        // repeats this check because an older queued command may meet a newer
        // partial request before it starts executing.
        let partial_guard = self.trade_lifecycle.partial_close_intents.read().await;
        if partial_guard.contains_key(&first_trade_id)
            || partial_guard.contains_key(&second_trade_id)
        {
            return Err(ActorCallError::PartialClosePending);
        }
        if exit_price != Some(0.5)
            || !self.paper_execution_allowed().await
            || !self
                .has_exact_complementary_pair(first_trade_id, second_trade_id)
                .await
        {
            return Err(ActorCallError::PairNotMergeable);
        }
        let dispatched = send_command_with_timeout(&tx, cmd).await;
        drop(partial_guard);
        match dispatched {
            Ok(()) => {}
            Err(ActorSendError::Closed) => {
                warn!("Execution actor send failed for close_pair; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "send failed for close_pair; activating kill switch",
                )
                .await;
                return Err(ActorCallError::SendFailed);
            }
            Err(ActorSendError::TimedOut) => {
                warn!(
                    "Execution actor send timed out ({}s) for close_pair; activating kill switch",
                    ACTOR_SEND_TIMEOUT.as_secs()
                );
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "send timed out for close_pair; activating kill switch",
                )
                .await;
                return Err(ActorCallError::SendFailed);
            }
        }

        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => {
                warn!("Execution actor response dropped for close_pair; activating kill switch");
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "response dropped for close_pair; activating kill switch",
                )
                .await;
                Err(ActorCallError::ResponseFailed)
            }
            Err(_) => {
                warn!(
                    "Execution actor timed out ({}s) for close_pair; activating kill switch",
                    ACTOR_RESPONSE_TIMEOUT.as_secs()
                );
                metrics::inc_actor_cmd_drop_high();
                self.halt_trading(
                    "execution_actor",
                    "timed out for close_pair; activating kill switch",
                )
                .await;
                Err(ActorCallError::ResponseFailed)
            }
        }
    }

    pub async fn maker_state_snapshot(&self) -> Value {
        let tx = match self.execution_state.execution_tx.read().await.clone() {
            Some(tx) => tx,
            None => return json!({ "error": "execution actor unavailable" }),
        };

        let (resp_tx, resp_rx) = oneshot::channel();
        if let Err(err) = send_command_with_timeout(
            &tx,
            super::super::actor::ExecutionCommand::SnapshotMakerState { resp: resp_tx },
        )
        .await
        {
            return json!({
                "error": match err {
                    ActorSendError::Closed => "execution actor send failed",
                    ActorSendError::TimedOut => "execution actor send timed out",
                }
            });
        }

        match tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => json!({ "error": "execution actor response failed" }),
            Err(_) => json!({ "error": "execution actor response timed out" }),
        }
    }

    pub async fn maker_publish_bbo(&self, bbo: TopOfBook) -> bool {
        {
            let mut g = self.execution_state.pending_bbo.lock().await;
            g.insert(bbo.token_id.clone(), bbo);
        }

        if self
            .execution_state
            .bbo_flush_enqueued
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return true;
        }

        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            self.execution_state
                .bbo_flush_enqueued
                .store(false, Ordering::SeqCst);
            metrics::inc_actor_cmd_drop_low();
            return false;
        };
        match tx.try_send(super::super::actor::ExecutionCommand::FlushBestBidAsk) {
            Ok(()) => true,
            Err(_) => {
                self.execution_state
                    .bbo_flush_enqueued
                    .store(false, Ordering::SeqCst);
                metrics::inc_actor_cmd_drop_low();
                false
            }
        }
    }

    pub async fn maker_publish_l2_stale(&self, stale: bool, reason: Option<String>) -> bool {
        self.maker_publish_l2_stale_with_code(stale, None, reason)
            .await
    }

    pub async fn maker_publish_l2_stale_with_code(
        &self,
        stale: bool,
        code: Option<&str>,
        reason: Option<String>,
    ) -> bool {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            metrics::inc_actor_cmd_drop_high();
            return false;
        };
        match tx.try_send(super::super::actor::ExecutionCommand::SetL2Stale {
            stale,
            code: code.map(ToString::to_string),
            reason,
        }) {
            Ok(()) => true,
            Err(_) => {
                metrics::inc_actor_cmd_drop_high();
                false
            }
        }
    }

    /// Tell the actor which tokens the L2 feed is subscribed to so it can
    /// drop the books of markets that have rolled. Non-blocking: a dropped
    /// command only delays the eviction to the next (re)connect.
    pub async fn maker_retain_l2_tokens(
        &self,
        token_ids: HashSet<String>,
        venue_rules: std::collections::HashMap<String, VenueRules>,
    ) -> bool {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            metrics::inc_actor_cmd_drop_high();
            return false;
        };
        match tx.try_send(super::super::actor::ExecutionCommand::RetainL2Tokens {
            token_ids,
            venue_rules,
        }) {
            Ok(()) => true,
            Err(_) => {
                metrics::inc_actor_cmd_drop_high();
                false
            }
        }
    }

    /// A market's venue rules changed under a live session (the tick moves
    /// as a market nears resolution).
    pub async fn maker_set_venue_rules(&self, market_id: String, rules: VenueRules) -> bool {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            metrics::inc_actor_cmd_drop_high();
            return false;
        };
        match tx.try_send(super::super::actor::ExecutionCommand::SetVenueRules { market_id, rules })
        {
            Ok(()) => true,
            Err(_) => {
                metrics::inc_actor_cmd_drop_high();
                false
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn append_outbox_audit(
        &self,
        stage: &str,
        market_id: &str,
        order_id: Option<&str>,
        token_id: Option<&str>,
        action: &str,
        status: &str,
        details: &str,
    ) {
        if let Some(tx) = self.execution_state.execution_tx.read().await.clone() {
            let cmd = super::super::actor::ExecutionCommand::AppendOutboxAudit {
                stage: stage.to_string(),
                market_id: market_id.to_string(),
                order_id: order_id.map(ToString::to_string),
                token_id: token_id.map(ToString::to_string),
                action: action.to_string(),
                status: status.to_string(),
                details: details.to_string(),
            };
            match tx.try_send(cmd) {
                Ok(()) => return,
                Err(_) => metrics::inc_actor_cmd_drop_low(),
            }
        }

        let pool = self.db.clone();
        let stage = stage.to_string();
        let market_id = market_id.to_string();
        let order_id = order_id.map(ToString::to_string);
        let token_id = token_id.map(ToString::to_string);
        let action = action.to_string();
        let status = status.to_string();
        let details = details.to_string();
        tokio::spawn(async move {
            if let Err(e) = db::append_order_outbox(
                &pool,
                &stage,
                &market_id,
                order_id.as_deref(),
                token_id.as_deref(),
                &action,
                &status,
                &details,
            )
            .await
            {
                metrics::inc_outbox_write_fail();
                warn!("Fallback outbox write failed: {e}");
            } else {
                metrics::inc_outbox_enqueued();
            }
        });
    }

    pub async fn maker_upsert_quote_state(
        &self,
        order_id: String,
        market_id: String,
        token_id: String,
        side: super::super::actor::QuoteSide,
        price: f64,
        size: f64,
    ) -> Option<super::super::actor::QuoteState> {
        let tx = self.execution_state.execution_tx.read().await.clone()?;
        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = super::super::actor::ExecutionCommand::UpsertQuoteState {
            order_id,
            market_id,
            token_id,
            side,
            price,
            size,
            resp: resp_tx,
        };
        if send_command_with_timeout(&tx, cmd).await.is_err() {
            return None;
        }
        tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx)
            .await
            .ok()
            .and_then(|resp| resp.ok())
            .flatten()
    }

    pub async fn maker_remove_quote_state(&self, order_id: String, reason: Option<String>) -> bool {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            return false;
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = super::super::actor::ExecutionCommand::RemoveQuoteState {
            order_id,
            reason,
            resp: resp_tx,
        };
        if send_command_with_timeout(&tx, cmd).await.is_err() {
            return false;
        }
        tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx)
            .await
            .ok()
            .and_then(|resp| resp.ok())
            .unwrap_or(false)
    }

    pub async fn maker_clear_market_quotes(
        &self,
        market_id: String,
        reason: Option<String>,
    ) -> usize {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            return 0;
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = super::super::actor::ExecutionCommand::ClearMarketQuotes {
            market_id,
            reason,
            resp: resp_tx,
        };
        if send_command_with_timeout(&tx, cmd).await.is_err() {
            return 0;
        }
        tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx)
            .await
            .ok()
            .and_then(|resp| resp.ok())
            .unwrap_or(0)
    }

    pub async fn maker_update_inventory_state(
        &self,
        market_id: String,
        net_yes_shares: f64,
        net_no_shares: f64,
        net_notional_usd: f64,
    ) -> super::super::actor::InventoryState {
        let Some(tx) = self.execution_state.execution_tx.read().await.clone() else {
            return super::super::actor::InventoryState::default();
        };
        let (resp_tx, resp_rx) = oneshot::channel();
        let cmd = super::super::actor::ExecutionCommand::UpdateInventoryState {
            market_id,
            net_yes_shares,
            net_no_shares,
            net_notional_usd,
            resp: resp_tx,
        };
        if send_command_with_timeout(&tx, cmd).await.is_err() {
            return super::super::actor::InventoryState::default();
        }
        tokio::time::timeout(ACTOR_RESPONSE_TIMEOUT, resp_rx)
            .await
            .ok()
            .and_then(|resp| resp.ok())
            .unwrap_or_default()
    }
}
