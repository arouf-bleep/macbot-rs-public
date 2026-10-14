//! Live exchange/local reconciliation helpers.
//!
//! This is a safety layer that periodically checks for orphaned local trades
//! and reconciles order status drift.

use anyhow::{anyhow, Result};
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use tracing::{info, warn};

use crate::db;
use crate::domain::{BotMode, Direction, TradeStatus};
use crate::fusion;
use crate::metrics;

use super::state::{ReconcileSummary, TradingBot};

#[derive(Debug, Clone, Copy, Default)]
pub struct LiveReconcileOutcome {
    pub orphan_local_closes: usize,
}

#[inline]
fn compute_merge_net_edge(merge_shares: f64, yes_entry: f64, no_entry: f64, gas_usd: f64) -> f64 {
    let exit_fee_rate = fusion::estimate_taker_fee(0.5);
    let yes_leg = merge_shares * (0.5 - yes_entry) - (exit_fee_rate * (merge_shares * 0.5));
    let no_leg = merge_shares * (0.5 - no_entry) - (exit_fee_rate * (merge_shares * 0.5));
    yes_leg + no_leg - gas_usd.max(0.0)
}

fn compute_order_set_drift(
    local_open_ids: &HashSet<String>,
    exchange_open_ids: &HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let mut missing_on_exchange: Vec<String> = local_open_ids
        .difference(exchange_open_ids)
        .cloned()
        .collect();
    let mut orphan_on_exchange: Vec<String> = exchange_open_ids
        .difference(local_open_ids)
        .cloned()
        .collect();
    missing_on_exchange.sort();
    orphan_on_exchange.sort();
    (missing_on_exchange, orphan_on_exchange)
}

// Durable local-state audit fields: market, order, token, action and details.
type LocalRepairAudit = (String, Option<String>, Option<String>, String, String);

impl TradingBot {
    pub async fn recheck_terminal_maker_order_serialized(&self, order_id: String) -> Result<()> {
        let sender = self
            .execution_state
            .execution_tx
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("execution actor unavailable for operator recheck"))?;
        let (response, receiver) = tokio::sync::oneshot::channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sender.send(super::actor::ExecutionCommand::RecheckTerminalMakerOrder {
                order_id,
                resp: response,
            }),
        )
        .await
        .map_err(|_| anyhow!("operator recheck send timed out"))?
        .map_err(|_| anyhow!("operator recheck actor stopped"))?;
        tokio::time::timeout(std::time::Duration::from_secs(30), receiver)
            .await
            .map_err(|_| anyhow!("operator recheck timed out; inspect persisted state"))?
            .map_err(|_| anyhow!("operator recheck response dropped; inspect persisted state"))?
            .map_err(|error| anyhow!(error))
    }

    /// Run maker venue accounting in the same writer as quote and close
    /// transitions. No REST/reconcile task mutates maker inventory directly.
    pub async fn reconcile_maker_orders_serialized(&self) -> Result<()> {
        if db::maker::pending_orders(&self.db).await?.is_empty() {
            return Ok(());
        }
        let sender = self
            .execution_state
            .execution_tx
            .read()
            .await
            .clone()
            .ok_or_else(|| {
                anyhow!("execution actor unavailable for durable maker reconciliation")
            })?;
        let (response, receiver) = tokio::sync::oneshot::channel();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sender.send(super::actor::ExecutionCommand::ReconcileMakerOrders { resp: response }),
        )
        .await
        .map_err(|_| anyhow!("maker reconcile command send timed out"))?
        .map_err(|_| anyhow!("maker reconcile actor stopped"))?;
        tokio::time::timeout(std::time::Duration::from_secs(30), receiver)
            .await
            .map_err(|_| anyhow!("maker reconcile response timed out"))?
            .map_err(|_| anyhow!("maker reconcile response dropped"))?
            .map_err(|error| anyhow!(error))
    }

    /// Reconcile local live-trade state with exchange order state.
    /// - Runs fill monitor refresh
    /// - Closes orphaned local live OPEN trades that lack both order state and filled shares
    /// - Syncs wallet balance
    pub async fn reconcile_live_state(&self) -> Result<LiveReconcileOutcome> {
        self.reconcile_live_state_serialized(false).await
    }

    async fn reconcile_live_state_serialized(&self, full: bool) -> Result<LiveReconcileOutcome> {
        if self.mode_kind().await != BotMode::Live {
            return Ok(LiveReconcileOutcome::default());
        }
        if let Some(sender) = self.execution_state.execution_tx.read().await.clone() {
            let (response, receiver) = tokio::sync::oneshot::channel();
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sender.send(super::actor::ExecutionCommand::ReconcileLiveState {
                    full,
                    resp: response,
                }),
            )
            .await
            .map_err(|_| anyhow!("live reconcile send timed out"))?
            .map_err(|_| anyhow!("live reconcile actor stopped"))?;
            return tokio::time::timeout(std::time::Duration::from_secs(30), receiver)
                .await
                .map_err(|_| anyhow!("live reconcile timed out; inspect persisted state"))?
                .map_err(|_| anyhow!("live reconcile response dropped; inspect persisted state"))?
                .map_err(|error| anyhow!(error));
        }
        // Startup and isolated library tests have no concurrent execution writer.
        self.reconcile_maker_orders_serialized().await?;
        let outcome = self.reconcile_live_state_inner().await?;
        if full {
            self.full_resync_live_state_inner(outcome).await?;
        }
        Ok(outcome)
    }

    pub(super) async fn reconcile_live_state_inner(&self) -> Result<LiveReconcileOutcome> {
        if self.mode_kind().await != BotMode::Live {
            return Ok(LiveReconcileOutcome::default());
        }
        self.require_reconciled_live_exits().await?;

        // Refresh statuses and fill prices from exchange.
        self.poll_open_live_orders().await;

        // Orphan guard: local live OPEN trade with neither an active order nor
        // a filled position should not persist.
        let orphan_ids: Vec<i64> = {
            let trades = self.portfolio_state.trades.read().await;
            trades
                .iter()
                .filter(|t| t.is_open() && t.is_live() && !t.has_order() && t.shares <= 1e-9)
                .map(|t| t.id)
                .collect()
        };

        let mut orphan_updates = Vec::new();
        let mut repair_failure: Option<String> = None;
        for id in orphan_ids {
            warn!("Reconcile: closing orphaned live OPEN trade {}", id);
            let closed = {
                let trades = self.portfolio_state.trades.read().await;
                trades.iter().find(|t| t.id == id).map(|t| {
                    let mut c = t.clone();
                    c.set_status(TradeStatus::Closed);
                    c.exit_type = Some("reconcile_orphaned".to_string());
                    c.exit_timestamp = Some(Utc::now().to_rfc3339());
                    c.exit_price = Some(c.entry_price);
                    c.pnl = Some(0.0);
                    c
                })
            };
            if let Some(closed) = closed {
                if db::update_trade(&self.db, &closed).await.is_ok() {
                    let mut trades = self.portfolio_state.trades.write().await;
                    if let Some(t) = trades.iter_mut().find(|t| t.id == id) {
                        *t = closed.clone();
                    }
                    metrics::inc_trades_closed();
                    orphan_updates.push(id);
                    self.append_outbox_audit(
                        "reconcile_orphaned_local_trade",
                        &closed.market_id,
                        closed.order_id.as_deref(),
                        closed.token_id.as_deref(),
                        "close_local_trade",
                        "repaired",
                        &format!("trade_id={} exit_type=reconcile_orphaned", closed.id),
                    )
                    .await;
                } else {
                    let err = format!("failed persisting orphan close for trade {id}");
                    warn!("Reconcile: {err}");
                    repair_failure = Some(err);
                    break;
                }
            }
        }
        if let Some(err) = repair_failure {
            self.set_degraded_no_trade(true);
            self.set_kill_switch(true);
            self.record_reconcile_error("live_reconcile", err.clone())
                .await;
            return Err(anyhow!(err));
        }
        if !orphan_updates.is_empty() {
            info!(
                "Reconcile: closed {} orphaned live trades",
                orphan_updates.len()
            );
            // Evict closed trades from hot memory after orphan cleanup.
            self.evict_closed_trades().await;
        }

        if self.clob.is_some() && self.sync_balance().await.is_none() {
            let err = "balance sync returned no value after orphan cleanup".to_string();
            warn!("Reconcile: {err}");
            self.set_degraded_no_trade(true);
            self.set_kill_switch(true);
            self.record_reconcile_error("live_reconcile", err.clone())
                .await;
            return Err(anyhow!(err));
        } else if self.clob.is_none() {
            warn!("Reconcile: skipping live balance sync verification because clob is unavailable");
        }
        self.record_reconcile_summary(ReconcileSummary {
            phase: "live_reconcile".to_string(),
            completed_at: Utc::now().to_rfc3339(),
            drift_count: orphan_updates.len(),
            repaired_count: orphan_updates.len(),
            orphan_local_closes: orphan_updates.len(),
            repaired_local_trades: 0,
            restored_local_trades: 0,
            closed_local_ghosts: 0,
            missing_on_exchange: 0,
            orphan_on_exchange: 0,
            allowed_drift: 0,
            kill_switch_active: self.kill_switch_active(),
        })
        .await;
        info!("Reconcile: completed live state sync");
        Ok(LiveReconcileOutcome {
            orphan_local_closes: orphan_updates.len(),
        })
    }

    /// Deep live reconcile pass used for restart safety and periodic full drift
    /// repair. This includes a normal reconcile cycle + DB-vs-memory OPEN live
    /// trade set repair by stable trade IDs.
    pub async fn full_resync_live_state(&self) -> Result<()> {
        self.reconcile_live_state_serialized(true).await.map(|_| ())
    }

    pub(super) async fn full_resync_live_state_inner(
        &self,
        live_outcome: LiveReconcileOutcome,
    ) -> Result<()> {
        if self.mode_kind().await != BotMode::Live {
            return Ok(());
        }
        metrics::inc_reconcile_full_runs();

        let db_open_live = db::load_open_live_trades(&self.db).await?;
        let db_open_live_count = db_open_live.len();
        let mut local_open_by_order: HashMap<String, crate::db::Trade> = HashMap::new();
        for t in &db_open_live {
            if t.has_order() {
                if let Some(oid) = t.order_id.as_deref().filter(|x| !x.trim().is_empty()) {
                    local_open_by_order.insert(oid.to_string(), t.clone());
                }
            }
        }
        // A resting maker quote is a durable order intent before it has a
        // filled position. It belongs in the local order set even with no
        // trade row yet; otherwise every valid maker quote is an orphan.
        for maker in db::maker::pending_orders(&self.db).await? {
            let mut template: crate::db::Trade = serde_json::from_str(&maker.template_json)?;
            template.order_id = Some(maker.order_id.clone());
            local_open_by_order.insert(maker.order_id, template);
        }
        let mut db_by_id: HashMap<i64, crate::db::Trade> =
            db_open_live.into_iter().map(|t| (t.id, t)).collect();
        let local_open_live_snapshot: Vec<crate::db::Trade> = {
            let trades = self.portfolio_state.trades.read().await;
            trades
                .iter()
                .filter(|t| t.is_open() && t.is_live())
                .cloned()
                .collect()
        };

        let mut drift_count = 0usize;
        let mut repaired_count = 0usize;
        let mut repaired_local_trades = 0usize;
        let mut restored_local_trades = 0usize;
        let mut closed_local_ghosts = 0usize;
        let mut missing_on_exchange_count = 0usize;
        let mut orphan_on_exchange_count = 0usize;
        let mut local_repair_audits: Vec<LocalRepairAudit> = Vec::new();
        let cancel_exchange_orphans = std::env::var("MACBOT_RECONCILE_CANCEL_EXCHANGE_ORPHANS")
            .ok()
            .map(|v| {
                let x = v.trim().to_ascii_lowercase();
                matches!(x.as_str(), "1" | "true" | "yes" | "on")
            })
            .unwrap_or(false);

        // Exchange-authoritative open-order view for full resync.
        if let Some(clob) = self.clob.as_ref() {
            match clob.list_open_orders().await {
                Ok(open_orders) => {
                    let mut exchange_open_ids: HashSet<String> = HashSet::new();
                    let mut exchange_by_id: HashMap<String, crate::clob::OpenOrder> =
                        HashMap::new();
                    for o in open_orders {
                        if !o.order_id.trim().is_empty() {
                            exchange_open_ids.insert(o.order_id.clone());
                            exchange_by_id.insert(o.order_id.clone(), o);
                        }
                    }

                    let local_open_ids: HashSet<String> =
                        local_open_by_order.keys().cloned().collect();
                    let (missing_on_exchange, orphan_on_exchange) =
                        compute_order_set_drift(&local_open_ids, &exchange_open_ids);
                    missing_on_exchange_count = missing_on_exchange.len();
                    orphan_on_exchange_count = orphan_on_exchange.len();

                    for oid in &missing_on_exchange {
                        drift_count += 1;
                        if let Some(t) = local_open_by_order.get(oid) {
                            warn!(
                                "Full reconcile drift: local OPEN order missing on exchange (trade_id={} order_id={})",
                                t.id, oid
                            );
                            self.append_outbox_audit(
                                "reconcile_exchange_missing",
                                &t.market_id,
                                Some(oid.as_str()),
                                t.token_id.as_deref(),
                                "verify_open_order",
                                "drift",
                                "local OPEN order_id not present in exchange open-order set",
                            )
                            .await;
                        }
                    }

                    for oid in &orphan_on_exchange {
                        drift_count += 1;
                        let ex = exchange_by_id.get(oid);
                        let market_id = ex.map(|x| x.market_id.as_str()).unwrap_or("");
                        let token_id = ex.and_then(|x| {
                            if x.token_id.trim().is_empty() {
                                None
                            } else {
                                Some(x.token_id.as_str())
                            }
                        });
                        warn!(
                            "Full reconcile drift: exchange OPEN order has no local OPEN trade (order_id={} market={})",
                            oid, market_id
                        );
                        self.append_outbox_audit(
                            "reconcile_exchange_orphan",
                            market_id,
                            Some(oid.as_str()),
                            token_id,
                            "verify_open_order",
                            "drift",
                            "exchange OPEN order_id has no local OPEN live trade",
                        )
                        .await;

                        if cancel_exchange_orphans {
                            match clob.cancel_order(oid).await {
                                Ok(cancelled) => {
                                    self.append_outbox_audit(
                                        "reconcile_exchange_orphan_cancel",
                                        market_id,
                                        Some(oid.as_str()),
                                        token_id,
                                        "cancel_order",
                                        "ok",
                                        &format!("cancelled={cancelled:?}"),
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    self.append_outbox_audit(
                                        "reconcile_exchange_orphan_cancel",
                                        market_id,
                                        Some(oid.as_str()),
                                        token_id,
                                        "cancel_order",
                                        "error",
                                        &e.to_string(),
                                    )
                                    .await;
                                    warn!(
                                        "Full reconcile orphan cancel failed (order_id={}): {e}",
                                        oid
                                    );
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Full reconcile: list_open_orders failed: {e}");
                    return Err(anyhow!("full reconcile list_open_orders failed: {e}"));
                }
            }
        }

        // Build authoritative settlement prices for local ghost OPEN trades
        // (present in memory but absent from DB OPEN set). Prefer exchange fill
        // snapshots / order status over assumed local entry prices.
        let mut ghost_exit_px: HashMap<i64, f64> = HashMap::new();
        for t in &local_open_live_snapshot {
            if db_by_id.contains_key(&t.id) {
                continue;
            }
            let mut px = t.entry_price;
            if let Ok(summary) = db::trade_fill_summary(&self.db, t.id).await {
                if let Some(v) = summary.exit_vwap.or(summary.entry_vwap) {
                    if v > 0.0 {
                        px = v;
                    }
                }
            }
            if let (Some(clob), Some(oid)) = (self.clob.as_ref(), t.order_id.as_deref()) {
                if !oid.trim().is_empty() {
                    if let Ok(info) = clob.get_order(oid).await {
                        if info.size_matched > 0.0 && info.price > 0.0 {
                            px = info.price;
                        }
                    }
                }
            }
            ghost_exit_px.insert(t.id, px);
        }

        {
            let mut trades = self.portfolio_state.trades.write().await;

            for t in trades.iter_mut().filter(|t| t.is_open() && t.is_live()) {
                if let Some(db_t) = db_by_id.remove(&t.id) {
                    let changed = serde_json::to_value(&*t)? != serde_json::to_value(&db_t)?;
                    if changed {
                        drift_count += 1;
                        repaired_count += 1;
                        repaired_local_trades += 1;
                        local_repair_audits.push((
                            db_t.market_id.clone(),
                            db_t.order_id.clone(),
                            db_t.token_id.clone(),
                            "repair_local_trade".to_string(),
                            format!(
                                "trade_id={} reconciled_from_db entry_price={:.4} status={} mode={}",
                                db_t.id, db_t.entry_price, db_t.status, db_t.mode
                            ),
                        ));
                        *t = db_t;
                    }
                } else {
                    drift_count += 1;
                    repaired_count += 1;
                    closed_local_ghosts += 1;
                    local_repair_audits.push((
                        t.market_id.clone(),
                        t.order_id.clone(),
                        t.token_id.clone(),
                        "close_local_trade".to_string(),
                        format!(
                            "trade_id={} missing_from_db_open_set exit_type=reconcile_missing_db_live_open",
                            t.id
                        ),
                    ));
                    t.set_status(TradeStatus::Closed);
                    t.exit_type = Some("reconcile_missing_db_live_open".to_string());
                    t.exit_timestamp = Some(Utc::now().to_rfc3339());
                    t.exit_price = Some(*ghost_exit_px.get(&t.id).unwrap_or(&t.entry_price));
                    t.pnl = Some(0.0);
                }
            }

            for (_, missing) in db_by_id {
                drift_count += 1;
                repaired_count += 1;
                restored_local_trades += 1;
                local_repair_audits.push((
                    missing.market_id.clone(),
                    missing.order_id.clone(),
                    missing.token_id.clone(),
                    "restore_local_trade".to_string(),
                    format!(
                        "trade_id={} restored_from_db_open_set entry_price={:.4} status={}",
                        missing.id, missing.entry_price, missing.status
                    ),
                ));
                trades.push(missing);
            }
            trades.sort_by_key(|t| t.id);
        }

        // Evict closed/ghost trades from hot memory after full reconcile repairs.
        self.evict_closed_trades().await;

        let max_allowed_drift = std::env::var("MACBOT_RECONCILE_MAX_LIVE_DRIFT")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);

        if drift_count > 0 {
            metrics::inc_reconcile_drift_events();
            for (market_id, order_id, token_id, action, details) in local_repair_audits {
                self.append_outbox_audit(
                    "reconcile_local_state",
                    &market_id,
                    order_id.as_deref(),
                    token_id.as_deref(),
                    &action,
                    "repaired",
                    &details,
                )
                .await;
            }
            if drift_count > max_allowed_drift {
                warn!(
                    "Full reconcile drift={} > allowed={} — activating kill switch",
                    drift_count, max_allowed_drift
                );
                self.halt_trading(
                    "reconcile_drift",
                    format!("drift={drift_count} exceeds allowed={max_allowed_drift}"),
                )
                .await;
            }
        }

        self.record_reconcile_summary(ReconcileSummary {
            phase: "full_resync".to_string(),
            completed_at: Utc::now().to_rfc3339(),
            drift_count,
            repaired_count,
            orphan_local_closes: live_outcome.orphan_local_closes,
            repaired_local_trades,
            restored_local_trades,
            closed_local_ghosts,
            missing_on_exchange: missing_on_exchange_count,
            orphan_on_exchange: orphan_on_exchange_count,
            allowed_drift: max_allowed_drift,
            kill_switch_active: self.kill_switch_active(),
        })
        .await;

        info!(
            "Full reconcile: repaired={} drift={} open_live_db={}",
            repaired_count, drift_count, db_open_live_count
        );
        Ok(())
    }

    /// Capital rebalancer for matched YES+NO inventory on the same market.
    /// Only exactly matched non-live pairs settle; both legs commit atomically.
    pub async fn rebalance_offsetting_pairs(&self) -> Result<usize> {
        let cfg = self.control_plane.config.read().await.clone();
        if !cfg.auto_merge_rebalancer || !self.paper_execution_allowed().await {
            return Ok(0);
        }

        let open_non_live = {
            let trades = self.portfolio_state.trades.read().await;
            trades
                .iter()
                .filter(|t| t.is_open() && !t.is_live())
                .cloned()
                .collect::<Vec<_>>()
        };

        let mut yes_by_market: HashMap<String, Vec<crate::db::Trade>> = HashMap::new();
        let mut no_by_market: HashMap<String, Vec<crate::db::Trade>> = HashMap::new();
        for t in open_non_live {
            if t.direction_kind() == Direction::BuyYes {
                yes_by_market
                    .entry(t.market_id.clone())
                    .or_default()
                    .push(t);
            } else if t.direction_kind() == Direction::BuyNo {
                no_by_market.entry(t.market_id.clone()).or_default().push(t);
            }
        }

        let mut close_pairs: Vec<(i64, i64, f64)> = Vec::new();
        for (market_id, mut yes_trades) in yes_by_market {
            let Some(mut no_trades) = no_by_market.remove(&market_id) else {
                continue;
            };
            yes_trades.sort_by_key(|t| t.id);
            no_trades.sort_by_key(|t| t.id);
            if yes_trades.is_empty() || no_trades.is_empty() {
                continue;
            }

            let yes = &yes_trades[0];
            let no = &no_trades[0];
            if !super::close::is_exact_complementary_pair(yes, no) {
                continue;
            }

            let merge_shares = yes.shares.min(no.shares);
            let net_edge = compute_merge_net_edge(
                merge_shares,
                yes.entry_price,
                no.entry_price,
                cfg.merge_gas_usd,
            );
            if net_edge < cfg.merge_min_edge_usd {
                continue;
            }
            close_pairs.push((yes.id, no.id, net_edge));
        }

        let mut rebalanced = 0usize;
        for (yes_id, no_id, net_edge) in close_pairs {
            {
                let intents = self.trade_lifecycle.partial_close_intents.read().await;
                if intents.contains_key(&yes_id) || intents.contains_key(&no_id) {
                    continue;
                }
            }
            let (yes_snap, no_snap) = {
                let trades = self.portfolio_state.trades.read().await;
                let yes = trades.iter().find(|t| t.id == yes_id).cloned();
                let no = trades.iter().find(|t| t.id == no_id).cloned();
                (yes, no)
            };
            if yes_snap.as_ref().is_none() || no_snap.as_ref().is_none() {
                warn!(
                    "Rebalancer: missing snapshot for pair yes_id={} no_id={}",
                    yes_id, no_id
                );
                continue;
            }
            let (yes_closed, no_closed) = match self
                .close_pair_serialized_result(yes_id, no_id, Some(0.5))
                .await
            {
                Ok(result) => result,
                Err(
                    super::state::ActorCallError::PartialClosePending
                    | super::state::ActorCallError::PairNotMergeable,
                ) => continue,
                Err(error) => {
                    warn!(%error, yes_id, no_id, "Rebalancer: pair command did not confirm");
                    (None, None)
                }
            };
            if yes_closed.is_some() && no_closed.is_some() {
                rebalanced += 1;
                metrics::inc_capital_rebalances();
                info!(
                    "Rebalancer: merged YES/NO pair (yes_id={}, no_id={}) net_edge=${:.3}",
                    yes_id, no_id, net_edge
                );
            } else {
                warn!(
                    "Rebalancer: partial/failed merge for pair yes_id={} no_id={} — activating kill switch",
                    yes_id, no_id,
                );
                self.halt_trading(
                    "rebalancer_partial_merge",
                    format!("partial or failed merge yes_id={yes_id} no_id={no_id}"),
                )
                .await;
            }
        }

        Ok(rebalanced)
    }
}

#[cfg(test)]
mod tests {
    use super::compute_merge_net_edge;
    use super::compute_order_set_drift;
    use super::TradingBot;
    use crate::config::Config;
    use crate::db::{self, Trade};
    use crate::domain::TradeStatus;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::time::sleep;

    struct EnvVarRestore {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvVarRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvVarRestore {
        fn drop(&mut self) {
            if let Some(old) = self.old.as_deref() {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn merge_edge_accounts_for_exit_fees_and_gas() {
        let net = compute_merge_net_edge(100.0, 0.46, 0.46, 0.05);
        assert!(net < 8.0);
        assert!(net > 0.0);
    }

    #[test]
    fn merge_edge_turns_negative_when_prices_sum_too_high() {
        let net = compute_merge_net_edge(100.0, 0.53, 0.52, 0.02);
        assert!(net < 0.0);
    }

    #[test]
    fn order_set_drift_is_computed_stably() {
        let local = ["a".to_string(), "b".to_string(), "c".to_string()]
            .into_iter()
            .collect();
        let exchange = ["b".to_string(), "c".to_string(), "d".to_string()]
            .into_iter()
            .collect();
        let (missing_on_exchange, orphan_on_exchange) = compute_order_set_drift(&local, &exchange);
        assert_eq!(missing_on_exchange, vec!["a".to_string()]);
        assert_eq!(orphan_on_exchange, vec!["d".to_string()]);
    }

    fn live_open_trade(id: i64, order_id: Option<&str>) -> Trade {
        Trade {
            id,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            market_id: "mkt-reconcile".to_string(),
            question: "reconcile".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.52,
            amount: 1.0,
            shares: 1.0,
            expected_value: 0.0,
            reason: "test".to_string(),
            confidence: 0.8,
            status: "OPEN".to_string(),
            mode: "live".to_string(),
            source: "bot".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: Some(0.8),
            order_id: order_id.map(|x| x.to_string()),
            token_id: Some("tok-reconcile".to_string()),
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: Some("test-cal-v1".to_string()),
            run_id: Some("test-run".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    #[tokio::test]
    async fn rebalancer_defers_either_held_leg_without_fault_or_accounting() -> anyhow::Result<()> {
        let pool = db::open(":memory:").await?;
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".into();
        cfg.auto_merge_rebalancer = true;
        cfg.merge_min_edge_usd = 0.0;
        cfg.merge_gas_usd = 0.0;
        let bot = TradingBot::new(cfg, pool.clone()).await?;
        let mut yes = live_open_trade(42, None);
        yes.mode = "simulation".into();
        yes.entry_price = 0.4;
        yes.shares = 12.0;
        yes.amount = 4.8;
        let mut no = yes.clone();
        no.id = 43;
        no.direction = "BUY_NO".into();
        *bot.portfolio_state.trades.write().await = vec![yes, no];
        let original_balance = bot.balance().await;
        for held_id in [42, 43] {
            bot.trade_lifecycle
                .partial_close_intents
                .write()
                .await
                .insert(
                    held_id,
                    crate::bot::state::PartialCloseIntent {
                        request_id: 9,
                        outcome_unknown: true,
                        live_order_id: None,
                    },
                );
            assert_eq!(bot.rebalance_offsetting_pairs().await?, 0);
            assert!(
                !bot.kill_switch_active(),
                "an intentional hold is not a failed merge"
            );
            assert_eq!(bot.balance().await, original_balance);
            assert!(bot
                .portfolio_state
                .trades
                .read()
                .await
                .iter()
                .all(|trade| trade.is_open() && trade.shares == 12.0));
            bot.trade_lifecycle
                .partial_close_intents
                .write()
                .await
                .remove(&held_id);
        }
        // The old mismatch tolerance admitted this 100/95 pair and paid 97.5
        // instead of retaining the unmatched 5 YES. No partial merge is claimed.
        {
            let mut config = bot.control_plane.config.write().await;
            config.merge_max_share_mismatch_pct = 0.10;
        }
        {
            let mut trades = bot.portfolio_state.trades.write().await;
            trades[0].shares = 100.0;
            trades[1].shares = 95.0;
        }
        assert_eq!(bot.rebalance_offsetting_pairs().await?, 0);
        assert!(!bot.kill_switch_active());
        assert_eq!(bot.balance().await, original_balance);
        let trades = bot.portfolio_state.trades.read().await;
        assert_eq!((trades[0].shares, trades[1].shares), (100.0, 95.0));
        assert!(trades
            .iter()
            .all(|trade| trade.is_open() && trade.pnl.is_none()));
        drop(trades);
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn full_resync_repairs_live_open_drift_and_kills_on_contract_breach() -> anyhow::Result<()>
    {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_full_resync_test_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let t1 = live_open_trade(1, Some("oid-1"));
        let mut t2 = live_open_trade(2, Some("oid-2"));
        t2.entry_price = 0.61;
        db::insert_trade(&pool, &t1).await?;
        db::insert_trade(&pool, &t2).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool).await?);

        // Inject in-memory drift:
        // - mutate only inventory/fees on t1, keeping identity and entry price
        // - remove t2
        // - add ghost live OPEN not present in DB
        {
            let mut trades = bot.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|x| x.id == 1) {
                t.shares = 0.1;
                t.amount = 0.052;
                t.fee_paid = 8.0;
            }
            trades.retain(|t| t.id != 2);
            let ghost = live_open_trade(999, Some("oid-ghost"));
            trades.push(ghost);
        }

        bot.full_resync_live_state().await?;

        let trades = bot.portfolio_state.trades.read().await.clone();
        assert!(
            trades.iter().any(|t| t.id == 2 && t.is_open()),
            "missing DB-open live trade should be restored into memory"
        );
        assert!(
            trades.iter().any(|t| t.id == 1
                && t.shares == t1.shares
                && t.amount == t1.amount
                && t.fee_paid == t1.fee_paid),
            "existing live-open trade should be repaired from DB source of truth"
        );
        // Ghost trade was force-closed and then evicted from hot memory by
        // evict_closed_trades() — it must no longer appear in the in-memory list.
        assert!(
            !trades.iter().any(|t| t.id == 999),
            "ghost local live-open trade should be evicted from memory after force-close"
        );
        assert!(
            bot.kill_switch_active(),
            "full reconcile drift should fail-closed by default"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn full_resync_no_drift_keeps_kill_switch_off() -> anyhow::Result<()> {
        let _env_guard = crate::env_util::test_env_lock();
        let _restore = EnvVarRestore::set("MACBOT_RECONCILE_MAX_LIVE_DRIFT", "0");
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_full_resync_clean_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        // This test concerns local inventory drift, not an unreconciled
        // legacy standing order. Settled inventory has no active order ID.
        let t1 = live_open_trade(1, None);
        db::insert_trade(&pool, &t1).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let mut bot = TradingBot::new(cfg, pool).await?;
        bot.clob = None;
        let bot = Arc::new(bot);
        assert!(!bot.kill_switch_active());
        bot.full_resync_live_state().await?;
        assert!(
            !bot.kill_switch_active(),
            "clean full resync should not activate kill switch"
        );
        let summary = bot.last_reconcile_summary().await.unwrap();
        assert_eq!(summary.drift_count, 0);
        assert_eq!(summary.allowed_drift, 0);
        assert!(!summary.kill_switch_active);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn full_resync_respects_allowed_drift_budget() -> anyhow::Result<()> {
        let _env_guard = crate::env_util::test_env_lock();
        let _restore = EnvVarRestore::set("MACBOT_RECONCILE_MAX_LIVE_DRIFT", "1");

        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_full_resync_budget_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let t1 = live_open_trade(1, None);
        db::insert_trade(&pool, &t1).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let mut bot = TradingBot::new(cfg, pool).await?;
        bot.clob = None;
        let bot = Arc::new(bot);
        {
            let mut trades = bot.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|x| x.id == 1) {
                t.entry_price = 0.10;
            }
        }

        bot.full_resync_live_state().await?;

        assert!(
            !bot.kill_switch_active(),
            "single repaired mismatch should stay within the configured drift budget"
        );
        let summary = bot.last_reconcile_summary().await.unwrap();
        assert_eq!(summary.drift_count, 1);
        assert_eq!(summary.repaired_local_trades, 1);
        assert_eq!(summary.allowed_drift, 1);
        assert!(!summary.kill_switch_active);
        let trades = bot.portfolio_state.trades.read().await;
        assert!(
            trades
                .iter()
                .any(|t| t.id == 1 && (t.entry_price - 0.52).abs() < 1e-9),
            "full resync should still repair the mismatched trade from DB"
        );

        drop(trades);
        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn local_drift_budget_cannot_authorize_unreconciled_legacy_orders() -> anyhow::Result<()>
    {
        let _env_guard = crate::env_util::test_env_lock();
        let _restore = EnvVarRestore::set("MACBOT_RECONCILE_MAX_LIVE_DRIFT", "100");
        let pool = db::open(":memory:").await?;
        let trade = live_open_trade(1, Some("legacy-oid"));
        db::insert_trade(&pool, &trade).await?;
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".into();
        let mut bot = TradingBot::new(cfg, pool).await?;
        bot.clob = None;
        bot.full_resync_live_state().await?;
        assert!(bot.degraded_no_trade_active());
        assert_eq!(
            bot.kill_switch_status().1.unwrap().source,
            "legacy_live_order_requires_migration"
        );
        let unchanged = db::load_trade_by_id(&bot.db, 1).await?.unwrap();
        assert!(unchanged.is_open());
        assert_eq!(unchanged.order_id.as_deref(), Some("legacy-oid"));
        assert_eq!(unchanged.shares, trade.shares);
        let summary = bot.last_reconcile_summary().await.unwrap();
        assert_eq!(summary.allowed_drift, 100);
        assert_eq!(summary.drift_count, 0);
        assert!(summary.kill_switch_active);
        Ok(())
    }

    /// Wait for the outbox to reach an expected audit count.
    ///
    /// `append_outbox_audit` hands the write to the execution actor and returns
    /// immediately, so the row lands on another task. A fixed sleep races that
    /// and fails on a loaded CI runner while passing on an idle laptop, which
    /// is the worst kind of test. Polling to a generous ceiling is deterministic
    /// where it matters and still fails if the write never happens.
    async fn await_outbox_audits(
        pool: &sqlx::SqlitePool,
        stage: &str,
        expected: i64,
        budget: Duration,
    ) -> i64 {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM order_outbox WHERE stage = ?")
                    .bind(stage)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
            if count >= expected || std::time::Instant::now() >= deadline {
                return count;
            }
            sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn full_resync_writes_outbox_audits_for_local_repairs() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_full_resync_outbox_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let t1 = live_open_trade(1, Some("oid-1"));
        let t2 = live_open_trade(2, Some("oid-2"));
        db::insert_trade(&pool, &t1).await?;
        db::insert_trade(&pool, &t2).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool.clone()).await?);
        {
            let mut trades = bot.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|x| x.id == 1) {
                t.entry_price = 0.10;
            }
            trades.retain(|t| t.id != 2);
            trades.push(live_open_trade(999, Some("oid-ghost")));
        }

        bot.full_resync_live_state().await?;
        await_outbox_audits(&pool, "reconcile_local_state", 3, Duration::from_secs(10)).await;

        let rows: Vec<(String, String, String)> =
            sqlx::query_as("SELECT stage, action, details FROM order_outbox ORDER BY id")
                .fetch_all(&pool)
                .await?;

        assert!(
            rows.iter().any(|(stage, action, details)| {
                stage == "reconcile_local_state"
                    && action == "repair_local_trade"
                    && details.contains("trade_id=1")
            }),
            "expected repair audit for mismatched local trade"
        );
        assert!(
            rows.iter().any(|(stage, action, details)| {
                stage == "reconcile_local_state"
                    && action == "restore_local_trade"
                    && details.contains("trade_id=2")
            }),
            "expected restore audit for DB-open trade reintroduced into memory"
        );
        assert!(
            rows.iter().any(|(stage, action, details)| {
                stage == "reconcile_local_state"
                    && action == "close_local_trade"
                    && details.contains("trade_id=999")
            }),
            "expected close audit for ghost local trade missing from DB open set"
        );

        let summary = bot
            .last_reconcile_summary()
            .await
            .expect("full resync should record a runtime summary");
        assert_eq!(summary.phase, "full_resync");
        assert_eq!(summary.repaired_local_trades, 1);
        assert_eq!(summary.restored_local_trades, 1);
        assert_eq!(summary.closed_local_ghosts, 1);
        assert_eq!(summary.repaired_count, 3);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn full_resync_is_idempotent_after_initial_repair() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_full_resync_idem_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let t1 = live_open_trade(1, Some("oid-1"));
        let t2 = live_open_trade(2, Some("oid-2"));
        db::insert_trade(&pool, &t1).await?;
        db::insert_trade(&pool, &t2).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool.clone()).await?);
        {
            let mut trades = bot.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|x| x.id == 1) {
                t.entry_price = 0.10;
            }
            trades.retain(|t| t.id != 2);
            trades.push(live_open_trade(999, Some("oid-ghost")));
        }

        bot.full_resync_live_state().await?;
        let first_count =
            await_outbox_audits(&pool, "reconcile_local_state", 3, Duration::from_secs(10)).await;
        assert_eq!(
            first_count, 3,
            "expected one repair/restore/close audit each"
        );

        bot.full_resync_live_state().await?;
        // A duplicate would push the count past first_count, so wait for one
        // more than expected and assert it never arrives.
        let second_count = await_outbox_audits(
            &pool,
            "reconcile_local_state",
            first_count + 1,
            // Proving a duplicate never arrives means waiting out a grace
            // period rather than waiting for a row, so this budget is short:
            // the same write path already landed three rows above.
            Duration::from_millis(1500),
        )
        .await;
        assert_eq!(
            second_count, first_count,
            "idempotent full resync should not emit duplicate local repair audits"
        );

        let summary = bot
            .last_reconcile_summary()
            .await
            .expect("second full resync should leave a summary");
        assert_eq!(summary.phase, "full_resync");
        assert_eq!(summary.drift_count, 0);
        assert_eq!(summary.repaired_count, 0);
        assert_eq!(summary.repaired_local_trades, 0);
        assert_eq!(summary.restored_local_trades, 0);
        assert_eq!(summary.closed_local_ghosts, 0);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn reconcile_live_state_closes_orphaned_live_trade_without_order_id() -> anyhow::Result<()>
    {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_reconcile_orphan_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut orphan = live_open_trade(7, None);
        orphan.shares = 0.0;
        db::insert_trade(&pool, &orphan).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool.clone()).await?);
        assert!(bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .any(|t| t.id == 7 && t.is_open()));

        let outcome = bot.reconcile_live_state().await?;
        assert_eq!(outcome.orphan_local_closes, 1);

        let in_mem = bot.portfolio_state.trades.read().await.clone();
        assert!(
            !in_mem.iter().any(|t| t.id == 7),
            "closed orphan should be evicted from hot memory"
        );

        let persisted = db::load_all_trades(&pool)
            .await?
            .into_iter()
            .find(|t| t.id == 7)
            .expect("persisted orphan trade");
        assert_eq!(persisted.status_kind(), TradeStatus::Closed);
        assert_eq!(persisted.exit_type.as_deref(), Some("reconcile_orphaned"));
        assert_eq!(persisted.pnl, Some(0.0));
        assert_eq!(persisted.exit_price, Some(orphan.entry_price));
        assert!(persisted.exit_timestamp.is_some());

        let summary = bot
            .last_reconcile_summary()
            .await
            .expect("live reconcile should record a runtime summary");
        assert_eq!(summary.phase, "live_reconcile");
        assert_eq!(summary.orphan_local_closes, 1);
        assert_eq!(summary.repaired_count, 1);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn reconcile_live_state_keeps_filled_live_trade_without_order_id() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_reconcile_filled_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut filled = live_open_trade(9, None);
        filled.shares = 3.0;
        db::insert_trade(&pool, &filled).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "live".to_string();
        cfg.execution_mode = "maker".to_string();

        let bot = Arc::new(TradingBot::new(cfg, pool.clone()).await?);
        let outcome = bot.reconcile_live_state().await?;
        assert_eq!(outcome.orphan_local_closes, 0);
        assert!(
            bot.portfolio_state
                .trades
                .read()
                .await
                .iter()
                .any(|t| t.id == 9 && t.is_open()),
            "filled position without active order should stay open"
        );

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }
}
