//! Receipt-backed FOK sales of all or a deliberate part of a live lot.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use tracing::warn;

use super::{live_sale_size, LimitOrderArgs, OrderType, Side, Trade, TradingBot};
use crate::{db, domain::TradeStatus};

impl TradingBot {
    pub(crate) async fn close_confirmed_live_position(&self, initial: &Trade) -> Option<Trade> {
        match self.close_confirmed_live_quantity(initial, None).await {
            Ok(outcome) if outcome["status"] != "pending" => {
                if outcome["status"] == "rejected" {
                    warn!(trade_id=initial.id, reason=%outcome["reason"], remaining_shares=%outcome["remaining_shares"],
                        "Live sale refused; remaining inventory was not marked closed");
                }
                serde_json::from_value(outcome["trade"].clone()).ok()
            }
            Ok(_) => None,
            Err(error) => {
                warn!(
                    trade_id = initial.id,
                    "Live close requires verified evidence: {error}"
                );
                self.set_degraded_no_trade(true);
                self.halt_trading("live_close_requires_evidence", error.to_string())
                    .await;
                None
            }
        }
    }

    pub(crate) async fn close_confirmed_live_quantity(
        &self,
        initial: &Trade,
        fraction: Option<f64>,
    ) -> Result<Value> {
        self.close_confirmed_live_quantity_for_request(initial, fraction, None)
            .await
    }

    pub(crate) async fn close_confirmed_live_quantity_for_request(
        &self,
        initial: &Trade,
        fraction: Option<f64>,
        request_id: Option<u64>,
    ) -> Result<Value> {
        let clob = self
            .clob
            .as_ref()
            .ok_or_else(|| anyhow!("live close requires venue credentials"))?;
        clob.check_settlement_rpc().await?;
        // Remember an already submitted intent before recovery. If recovery
        // finishes a prior partial sale, this invocation reports that result;
        // it never uses the reopened remainder to submit another sale.
        let recovering = db::live::pending_orders(&self.db)
            .await?
            .into_iter()
            .find(|order| order.trade_id == initial.id);
        if !self.recover_live_orders_or_halt().await {
            return Err(anyhow!(
                "live order recovery requires more verified evidence; do not retry the sale"
            ));
        }
        let trade = db::load_trade_by_id(&self.db, initial.id)
            .await?
            .ok_or_else(|| anyhow!("live position no longer exists"))?;
        anyhow::ensure!(trade.is_live(), "persisted position is not a live lot");
        if let Some(order) = recovering {
            if order.phase != "exit" {
                return self
                    .live_sale_outcome(
                        trade,
                        0.0,
                        Some(&order.order_id),
                        None,
                        "recovered an entry BUY, not a sale; refresh confirmed inventory before requesting a new sale",
                    )
                    .await;
            }
            return self
                .live_sale_outcome(
                    trade,
                    order.size,
                    Some(&order.order_id),
                    None,
                    "recovered the existing order; no additional sale was submitted",
                )
                .await;
        }
        if !trade.is_open() || trade.has_order() {
            return self
                .live_sale_outcome(
                    trade,
                    0.0,
                    None,
                    None,
                    "position is not an available settled open lot",
                )
                .await;
        }
        anyhow::ensure!(
            trade.accounting_version == 1,
            "historical live inventory requires verified entry cost and fees before a sale"
        );
        anyhow::ensure!(
            (trade.shares - initial.shares).abs() <= 1e-8,
            "persisted position quantity changed; refresh before requesting a new sale"
        );
        let token = trade
            .token_id
            .as_deref()
            .ok_or_else(|| anyhow!("live lot has no asset identity"))?;
        let book = clob.get_order_book(token).await?;
        let number = |key: &str| {
            book.get(key)
                .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
                .filter(|v| v.is_finite() && *v > 0.0)
                .ok_or_else(|| anyhow!("venue book has no valid {key}"))
        };
        let tick = number("tick_size")?;
        let minimum = number("min_order_size")?;
        let size = match live_sale_size(trade.shares, fraction, minimum) {
            Ok(size) => size,
            Err(error) => {
                let requested =
                    (trade.shares * fraction.unwrap_or(1.0) * 100.0 + 1e-9).floor() / 100.0;
                return self
                    .live_sale_outcome(trade, requested, None, Some(minimum), &error.to_string())
                    .await;
            }
        };
        let best_bid = crate::json_util::extract_levels(&book, "bids")
            .first()
            .map(|level| level.0)
            .ok_or_else(|| anyhow!("venue book has no executable bid"))?;
        let floor = ((best_bid * 0.8 / tick).ceil() * tick).max(tick);
        let fees = clob.market_fee_parameters(token).await?;
        clob.update_balance_allowance(&crate::clob::BalanceAllowanceParams {
            asset_type: crate::clob::AssetType::Conditional,
            token_id: Some(token.to_string()),
        })
        .await?;
        let mut price = (best_bid / tick + 1e-9).floor() * tick;
        while price + 1e-9 >= floor {
            let signed = match clob
                .create_order(&LimitOrderArgs {
                    token_id: token.into(),
                    price,
                    size,
                    side: Side::Sell,
                })
                .await
            {
                Ok(signed) => signed,
                Err(error) => {
                    return self
                        .live_sale_outcome(
                            trade,
                            size,
                            None,
                            Some(minimum),
                            &format!("venue sizing or signing rules refused this sale: {error}"),
                        )
                        .await
                }
            };
            let signed_size = signed.maker_amount.parse::<u128>()? as f64 / 1_000_000.0;
            anyhow::ensure!(
                (signed_size - size).abs() <= 1e-8 && signed_size <= trade.shares + 1e-8,
                "signed sale quantity differs from the validated requested quantity"
            );
            let order_id = signed.order_id().to_string();
            // Commit the exact signed identity/size and CLOSING state before POST.
            db::live::prepare_order(
                &self.db,
                trade.id,
                &clob.account_address(),
                &signed,
                price,
                fees,
                "exit",
            )
            .await?;
            if let Some(request_id) = request_id.filter(|_| fraction.is_some()) {
                if let Some(intent) = self
                    .trade_lifecycle
                    .partial_close_intents
                    .write()
                    .await
                    .get_mut(&trade.id)
                {
                    if intent.request_id == request_id {
                        intent.live_order_id = Some(order_id.clone());
                    }
                }
            }
            if let Some(closing) = db::load_trade_by_id(&self.db, trade.id).await? {
                self.publish_live_trade_state(closing).await;
            }
            match clob.post_order(&signed, OrderType::FOK).await {
                Ok(response) if response.success == Some(false) => {
                    // Only an explicit venue rejection permits a fresh price.
                    let reopened = db::live::reject_order(&self.db, &order_id).await?;
                    self.publish_live_trade_state(reopened).await;
                }
                outcome => {
                    if !matches!(&outcome, Ok(response) if response.order_id == order_id) {
                        self.set_degraded_no_trade(true);
                        self.halt_trading("live_exit_submission_uncertain",
                            "exit identity is saved; venue confirmation is required before another sale").await;
                    }
                    let reconciled = self.recover_live_orders_or_halt().await;
                    let updated =
                        db::load_trade_by_id(&self.db, trade.id)
                            .await?
                            .ok_or_else(|| {
                                anyhow!("submitted live lot cannot be read; do not retry")
                            })?;
                    if updated.status_kind() != TradeStatus::Closing && reconciled {
                        self.sync_balance().await;
                    }
                    return self
                        .live_sale_outcome(
                            updated,
                            size,
                            Some(&order_id),
                            Some(minimum),
                            if reconciled {
                                "venue and finalized wallet receipts reconciled"
                            } else {
                                "settlement evidence is incomplete; do not retry the sale"
                            },
                        )
                        .await;
                }
            }
            price = ((price / tick).round() - 1.0) * tick;
        }
        let updated = db::load_trade_by_id(&self.db, trade.id)
            .await?
            .ok_or_else(|| anyhow!("rejected live lot cannot be read"))?;
        self.live_sale_outcome(
            updated,
            size,
            None,
            Some(minimum),
            "venue rejected every FOK price within the sale floor; no shares were sold",
        )
        .await
    }

    async fn live_sale_outcome(
        &self,
        trade: Trade,
        requested: f64,
        order_id: Option<&str>,
        minimum: Option<f64>,
        reason: &str,
    ) -> Result<Value> {
        let pending = trade.status_kind() == TradeStatus::Closing;
        let confirmed = if let Some(order_id) = order_id {
            sqlx::query_scalar::<_, f64>(
                "SELECT COALESCE(SUM(m.size),0.0) FROM live_matches m
                 JOIN live_orders o ON o.order_id=m.order_id
                 WHERE m.order_id=? AND o.trade_id=? AND o.phase='exit'",
            )
            .bind(order_id)
            .bind(trade.id)
            .fetch_one(&self.db)
            .await?
        } else {
            0.0
        };
        let pnl = if !pending && confirmed > 0.0 {
            let key = format!("live/{}", order_id.unwrap());
            sqlx::query_scalar::<_, f64>(
                "SELECT net_pnl FROM realized_pnl_events WHERE event_key=? AND trade_id=?",
            )
            .bind(key)
            .bind(trade.id)
            .fetch_optional(&self.db)
            .await?
        } else {
            None
        };
        anyhow::ensure!(
            pending || confirmed == 0.0 || pnl.is_some(),
            "confirmed sale is missing its realized accounting event; do not retry"
        );
        let remaining = if trade.status_kind() == TradeStatus::Closed {
            0.0
        } else {
            trade.shares
        };
        let sellable = (remaining * 100.0 + 1e-9).floor() / 100.0;
        let dust = minimum.map(|minimum| remaining > 1e-6 && sellable < minimum);
        self.publish_live_trade_state(trade.clone()).await;
        Ok(json!({
            "ok": !pending && confirmed > 0.0,
            "id": trade.id, "status": if pending { "pending" } else if confirmed > 0.0 { "settled" } else { "rejected" },
            "partial": remaining > 1e-6, "requested_shares": requested, "confirmed_shares": confirmed,
            "remaining_shares": remaining, "remaining_amount": if remaining > 0.0 { trade.amount } else { 0.0 },
            "remaining_sellable_shares": sellable, "minimum_order_shares": minimum,
            "dust_remaining": dust, "order_id": order_id, "pnl": pnl,
            "exit_price": if !pending && confirmed > 0.0 { trade.exit_price } else { None },
            "reason": reason, "trade": trade,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::live_sale_size;

    #[test]
    fn partial_sizes_preserve_an_executable_remainder_and_never_round_up() {
        assert_eq!(live_sale_size(12.01, Some(0.5), 5.0).unwrap(), 6.0);
        assert_eq!(live_sale_size(10.02, Some(0.5), 5.0).unwrap(), 5.01);
        assert_eq!(live_sale_size(10.0, Some(0.5), 5.0).unwrap(), 5.0);
        assert!(live_sale_size(9.99, Some(0.5), 5.0)
            .unwrap_err()
            .to_string()
            .contains("below the venue minimum"));
        assert!(live_sale_size(10.0, Some(0.6), 5.0)
            .unwrap_err()
            .to_string()
            .contains("would leave"));
        assert_eq!(
            live_sale_size(5.00999, None, 5.0).unwrap(),
            5.0,
            "a full sale may leave real sub-cent inventory, which accounting must retain"
        );
    }

    #[test]
    fn invalid_quantities_cannot_reach_signing() {
        for size in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1e15] {
            assert!(live_sale_size(size, None, 5.0).is_err());
        }
        for fraction in [f64::NAN, f64::INFINITY, -0.1, 0.0, 1.0, 1.1] {
            assert!(live_sale_size(12.0, Some(fraction), 5.0).is_err());
        }
        for minimum in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            assert!(live_sale_size(12.0, Some(0.5), minimum).is_err());
        }
    }
}
