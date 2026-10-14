//! Actor-only, atomic settlement of exactly matched complementary paper lots.
use super::{is_exact_complementary_pair, local_mark_snapshot, TradingBot};
use crate::{
    bot::state::ActorCallError, clob::SimTradePriceEvidence, db, domain::BotMode, metrics,
};
use chrono::Utc;

impl TradingBot {
    pub(crate) async fn merge_simulated_pair_direct(
        &self,
        first_id: i64,
        second_id: i64,
    ) -> Result<(Option<db::Trade>, Option<db::Trade>), ActorCallError> {
        if !self.paper_execution_allowed().await {
            return Err(ActorCallError::PairNotMergeable);
        }
        let (first, second) = {
            let trades = self.portfolio_state.trades.read().await;
            let Some(first) = trades.iter().find(|trade| trade.id == first_id) else {
                return Err(ActorCallError::PairNotMergeable);
            };
            let Some(second) = trades.iter().find(|trade| trade.id == second_id) else {
                return Err(ActorCallError::PairNotMergeable);
            };
            if !is_exact_complementary_pair(first, second) {
                return Err(ActorCallError::PairNotMergeable);
            }
            (first.clone(), second.clone())
        };
        let balance = self.balance().await;
        let payout = first.shares;
        if !balance.is_finite() || !(balance + payout).is_finite() || balance < 0.0 {
            return Err(ActorCallError::PairNotMergeable);
        }
        let now = Utc::now().to_rfc3339();
        let mut closed = [first.clone(), second.clone()];
        for trade in &mut closed {
            let pnl = trade.pnl.unwrap_or(0.0) + trade.shares * (0.5 - trade.entry_price);
            if !pnl.is_finite() || !trade.fee_paid.is_finite() || trade.fee_paid < 0.0 {
                return Err(ActorCallError::PairNotMergeable);
            }
            trade.status = "CLOSED".into();
            trade.pnl = Some(pnl);
            trade.exit_price = Some(0.5);
            trade.exit_timestamp = Some(now.clone());
            trade.exit_type = Some("merged".into());
            trade.was_correct = Some(i64::from(trade.net_realized_pnl() > 0.0));
            trade.fok_failed = Some(0);
        }
        let ids = [first.id, second.id];
        let order_ids = ids.map(|id| format!("pair-merge-{first_id}-{second_id}-{id}"));
        let details = [&first, &second].map(|trade| {
            SimTradePriceEvidence::new(
                "mark_exit",
                "pair_merge",
                "sell",
                0.5,
                0.5,
                trade.shares,
                "atomic complementary simulated pair payout; no exchange execution",
                local_mark_snapshot(trade.token_id.as_deref(), 0.5, "pair_merge"),
            )
            .as_details_json()
        });
        let evidence = [0, 1].map(|index| db::paper::PaperExecutionEvidence {
            order_id: &order_ids[index],
            phase: "mark_exit",
            intended_price: 0.5,
            avg_price: 0.5,
            shares: payout,
            source: "pair_merge",
            details: &details[index],
        });
        // Keep the verified accounting origin stable through both durable
        // settlement and cash publication, as on other paper cash writers.
        let origin = self.control_plane.config.read().await;
        if self.clob.is_some()
            || !matches!(
                origin.mode_kind(),
                BotMode::Paper | BotMode::Shadow | BotMode::Simulation
            )
        {
            return Err(ActorCallError::PairNotMergeable);
        }
        if let Err(error) = db::paper::apply_pair_merge(
            &self.db,
            &first,
            &closed[0],
            &evidence[0],
            &second,
            &closed[1],
            &evidence[1],
        )
        .await
        {
            self.set_degraded_no_trade(true);
            self.halt_trading("paper_pair_merge_persistence", error.to_string())
                .await;
            return Err(ActorCallError::ResponseFailed);
        }
        // No cash or hot lot is published before both durable legs and their
        // evidence commit. The execution actor is the single cash writer.
        {
            let mut trades = self.portfolio_state.trades.write().await;
            for trade in &closed {
                if let Some(current) = trades.iter_mut().find(|current| current.id == trade.id) {
                    *current = trade.clone();
                }
            }
        }
        self.set_balance(balance + payout).await;
        for id in ids {
            self.trade_lifecycle
                .closing_in_progress
                .write()
                .await
                .remove(&id);
            metrics::inc_trades_closed();
        }
        self.evict_closed_trades().await;
        drop(origin);
        Ok((Some(closed[0].clone()), Some(closed[1].clone())))
    }
}

#[cfg(test)]
mod tests;
