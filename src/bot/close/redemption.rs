//! Operator-requested reconciliation, never a redemption transaction sender.
use super::TradingBot;
use crate::{clob::redemption::RedemptionRequest, db, domain::BotMode};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

impl TradingBot {
    /// Only the single execution actor calls this method. Stops are prerequisites
    /// and this method never clears them, including on a harmless replay.
    pub(crate) async fn reconcile_redemption_direct(
        &self,
        request: RedemptionRequest,
    ) -> Result<Value> {
        let _controls = self
            .execution_state
            .recovery_control_gate
            .try_lock()
            .map_err(|_| anyhow::anyhow!("another recovery/control operation is active"))?;
        request.validate()?;
        ensure!(
            self.control_plane.config.read().await.mode_kind() == BotMode::Live,
            "redemption reconciliation requires live mode"
        );
        ensure!(
            !self.loop_running() && self.kill_switch_active(),
            "stop the loop and engage the kill switch before redemption reconciliation"
        );
        let proof = self
            .redemption_evidence(&request, std::time::Duration::from_secs(20))
            .await?;
        ensure!(self.control_plane.config.read().await.mode_kind()==BotMode::Live
            && !self.loop_running() && self.kill_switch_active(),
            "recovery controls changed during evidence reads; stop/freeze before replaying this reconciliation");
        let mut result = db::redemption::apply(&self.db, &proof, &request.trade_ids).await?;
        for id in &request.trade_ids {
            let trade = db::load_trade_by_id(&self.db, *id)
                .await?
                .context("reconciled trade missing from ledger")?;
            self.publish_live_trade_state(trade).await;
        }
        let balance = self
            .sync_balance_before(tokio::time::Instant::now() + std::time::Duration::from_secs(5))
            .await
            .ok()
            .flatten();
        result["available_balance"] = serde_json::json!(balance);
        result["balance_verified"] = serde_json::json!(balance.is_some());
        if balance.is_none() {
            self.set_degraded_no_trade(true);
            self.halt_trading("redemption_balance_unverified","receipt accounting committed; spendable collateral still requires a successful venue read").await;
        }
        let (kill_switch, reason) = self.kill_switch_status();
        result["kill_switch"] = serde_json::json!(kill_switch);
        result["kill_switch_reason"] = serde_json::json!(reason);
        result["loop_running"] = serde_json::json!(self.loop_running());
        Ok(result)
    }

    pub(super) async fn redemption_evidence(
        &self,
        request: &RedemptionRequest,
        deadline: std::time::Duration,
    ) -> Result<crate::clob::redemption::VerifiedRedemption> {
        let clob = self.clob.as_ref().context(
            "redemption requires the configured funding account and finalized Polygon RPC",
        )?;
        let verifier = clob.settlement_verifier()?;
        // Cancellation is confined to reads before accounting begins. Four
        // concurrent entry checks and one absolute deadline bound actor work;
        // the receipt/lot transaction itself is never cancelled by this timer.
        let read_phase = async {
            let mut proof = verifier
                .verify_redemption(request, &clob.account_address())
                .await?;
            let entries = db::redemption::entry_transactions(&self.db, &request.trade_ids).await?;
            for batch in entries.chunks(4) {
                futures_util::future::try_join_all(
                    batch
                        .iter()
                        .map(|hash| verifier.verify_entry_before_redemption(hash, &proof)),
                )
                .await?;
            }
            let orders =
                db::redemption::entry_receipt_orders(&self.db, &request.trade_ids, &entries)
                    .await?;
            for batch in orders.chunks(4) {
                futures_util::future::try_join_all(batch.iter().map(|order| async {
                    ensure!(
                        order
                            .identity
                            .maker_address
                            .eq_ignore_ascii_case(&proof.account),
                        "original entry belongs to another funding account"
                    );
                    let mut fills = Vec::new();
                    for hash in &order.transactions {
                        fills.extend(verifier.verify(hash, &order.identity).await?);
                    }
                    db::redemption::verify_entry_fills(order, &fills)
                }))
                .await?;
            }
            proof.entry_transaction_hashes = entries;
            Ok::<_, anyhow::Error>(proof)
        };
        let proof = tokio::time::timeout(deadline, read_phase).await.context(
            "redemption evidence read deadline exceeded; no redemption accounting was applied",
        )??;
        Ok(proof)
    }
}

#[cfg(test)]
#[path = "redemption/tests.rs"]
mod tests;
