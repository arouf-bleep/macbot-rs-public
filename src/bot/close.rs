//! Trade closure — full close, FOK step-down sell ladder, and partial close.

use chrono::Utc;
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use super::state::TradingBot;
use crate::clob::{
    fetch_public_book_snapshot, fetch_public_midpoint, fetch_public_order_book, LimitOrderArgs,
    OrderType, PublicBookSnapshot, Side, SimTradePriceEvidence,
};
use crate::db::{self, Trade};
use crate::domain::{BotMode, TradeStatus};
use crate::json_util::extract_levels;
use crate::metrics;

const FOK_RETRY_MAX_ATTEMPTS: u32 = 3;
const FOK_RETRY_BASE_DELAY_SECS: u64 = 10;

mod live_sale;
#[cfg(test)]
mod live_sale_tests;
mod pair_merge;
mod redemption;

/// Match the adapter's share precision without rounding a sale up. A deliberate
/// partial sale must leave another independently executable sale; it cannot
/// silently sell the whole position or strand a below-minimum remainder.
fn live_sale_size(shares: f64, fraction: Option<f64>, minimum: f64) -> anyhow::Result<f64> {
    anyhow::ensure!(
        shares.is_finite() && shares > 0.0 && shares * 100.0 <= (1_u64 << 53) as f64,
        "remaining live shares are invalid or exceed exact sizing precision"
    );
    anyhow::ensure!(
        minimum.is_finite() && minimum > 0.0,
        "venue minimum size is unavailable"
    );
    if let Some(fraction) = fraction {
        anyhow::ensure!(
            fraction.is_finite() && fraction > 0.0 && fraction < 1.0,
            "partial close fraction must be finite and strictly between 0 and 1"
        );
    }
    let requested = shares * fraction.unwrap_or(1.0);
    let size = (requested * 100.0 + 1e-9).floor() / 100.0;
    anyhow::ensure!(
        size >= minimum,
        "rounded sale size {size:.2} shares is below the venue minimum {minimum:.6}"
    );
    if fraction.is_some() {
        let remaining = shares - size;
        anyhow::ensure!(remaining >= minimum,
            "partial sale would leave {remaining:.6} shares below the venue minimum {minimum:.6}; request a smaller fraction or an explicit full close");
    }
    Ok(size)
}

#[inline]
fn token_pnl(shares: f64, entry_price: f64, exit_price: f64) -> f64 {
    shares * (exit_price - entry_price)
}

#[inline]
fn env_f64_or(key: &str, default: f64) -> f64 {
    crate::env_util::env_f64(key, default)
}

fn local_mark_snapshot(token_id: Option<&str>, price: f64, source: &str) -> PublicBookSnapshot {
    let safe_price = if price.is_finite() && price > 0.0 {
        Some(price.min(1.0))
    } else {
        None
    };
    PublicBookSnapshot::from_bbo(
        token_id.unwrap_or("unknown-token"),
        None,
        None,
        None,
        None,
        safe_price,
        source,
    )
}

fn looks_like_real_token_id(token_id: &str) -> bool {
    !token_id.trim().is_empty() && token_id.chars().all(|c| c.is_ascii_digit())
}

/// Result of the shutdown close sweep.
///
/// What a close with no price means. Until this existed, "no price" on a
/// simulated trade meant "mark it out at entry": the shutdown sweep's
/// contract, which the dashboard's Close button also hit, so a manual close
/// of a maker trade closed at its entry price, P&L zero, labelled
/// `shutdown_cleanup`. A manual close is a sale at market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CloseIntent {
    /// A strategy exit at market (take-profit, stop, reversal, retry):
    /// live orders hit the book, simulated positions walk the public bids.
    #[default]
    Market,
    /// A person asked. Priced like `Market`; the maker also stands down on
    /// that market until it rolls, because a resting bid refilled within
    /// five seconds of the last manual close and the position was back.
    Manual,
    /// A simulated resting maker ask actually crossed. Its supplied fill price
    /// is executable evidence, unlike the mark supplied by a strategy trigger.
    PassiveFill,
    /// The resolution guard verified a terminal payout; no order book sale.
    Resolution,
    /// The rebalancer redeems complementary simulated YES/NO inventory.
    /// This is a pair settlement, not a market sale or terminal resolution.
    Merge,
    /// The shutdown sweep: a simulated position is marked out at its entry
    /// so shutdown never waits on the network.
    ShutdownMark,
}

/// Only equal complementary non-live lots can be fully merged. Closing both
/// unequal legs would invent a payout for unmatched inventory.
pub(crate) fn is_exact_complementary_pair(first: &Trade, second: &Trade) -> bool {
    first.id != second.id
        && first.is_open()
        && second.is_open()
        && matches!(
            first.mode_kind(),
            BotMode::Paper | BotMode::Shadow | BotMode::Simulation
        )
        && matches!(
            second.mode_kind(),
            BotMode::Paper | BotMode::Shadow | BotMode::Simulation
        )
        && !first.market_id.is_empty()
        && first.market_id == second.market_id
        && first.shares.is_finite()
        && first.shares > 0.0
        && first.shares == second.shares
        && [first, second].iter().all(|trade| {
            let cost = trade.shares * trade.entry_price;
            trade.accounting_version == 0
                && trade.entry_price.is_finite()
                && (0.0..=1.0).contains(&trade.entry_price)
                && trade.amount.is_finite()
                && trade.amount >= 0.0
                && cost.is_finite()
                && (trade.amount - cost).abs() <= 1e-9 * cost.max(1.0)
                && trade.fee_paid.is_finite()
                && trade.fee_paid >= 0.0
                && trade.pnl.is_none_or(f64::is_finite)
        })
        && matches!(
            (first.direction_kind(), second.direction_kind()),
            (
                crate::domain::Direction::BuyYes,
                crate::domain::Direction::BuyNo
            ) | (
                crate::domain::Direction::BuyNo,
                crate::domain::Direction::BuyYes
            )
        )
}

/// Full simulated sales have FOK semantics: never extrapolate a partial book
/// fill to the rest of the position.
#[cfg(test)]
fn simulated_sell_fill(book: &Value, shares: f64) -> Option<(f64, f64)> {
    if !shares.is_finite() || shares <= 0.0 {
        return None;
    }
    let bids = extract_levels(book, "bids");
    let best_bid = bids.first()?.0;
    if best_bid <= 0.005 {
        return None;
    }
    let floor = (best_bid * 0.80).max(0.01);
    let mut remaining = shares;
    let mut proceeds = 0.0;
    for (price, qty) in bids {
        if price < floor {
            break;
        }
        let take = remaining.min(qty);
        proceeds += take * price;
        remaining -= take;
        if remaining <= 1e-9 {
            return Some((proceeds / shares, best_bid));
        }
    }
    None
}

/// Shutdown must be provable, not merely logged: a caller (and a test) needs to
/// know which positions were closed, which refused to close, and whether any
/// OPEN trade survived into the database.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ShutdownCleanupOutcome {
    /// Trades that were open in hot state when the sweep began.
    pub attempted: Vec<i64>,
    /// Trades the sweep closed successfully.
    pub closed: Vec<i64>,
    /// Trades whose close call errored, returned nothing, or left them open.
    pub failed: Vec<i64>,
    /// Trades that were still marked open by the close call itself.
    pub still_open: Vec<i64>,
    /// Trades still persisted as OPEN or CLOSING after the sweep. An unresolved
    /// exit remains exposure; a stopped process must never describe it as flat.
    pub remaining_open: Vec<i64>,
    /// Resting or unresolved orders still own the right to change exposure.
    pub remaining_orders: Vec<String>,
    pub order_cleanup_error: Option<String>,
    /// Set when persisted state could not be read at all. "I could not check"
    /// is not "there is nothing there", and must never present as clean.
    pub verification_error: Option<String>,
}

impl ShutdownCleanupOutcome {
    /// A clean shutdown verified persisted state, left nothing open, and had
    /// no failed close attempt. Unverified is never clean.
    pub fn is_clean(&self) -> bool {
        self.verification_error.is_none()
            && self.remaining_open.is_empty()
            && self.remaining_orders.is_empty()
            && self.order_cleanup_error.is_none()
            && self.failed.is_empty()
    }

    /// Whether any position survived shutdown in the database.
    pub fn leaked_open_positions(&self) -> bool {
        !self.remaining_open.is_empty()
    }

    /// Whether persisted exposure is unknown because verification failed.
    pub fn verification_failed(&self) -> bool {
        self.verification_error.is_some()
    }
}

impl TradingBot {
    pub(crate) async fn paper_execution_allowed(&self) -> bool {
        self.clob.is_none()
            && matches!(
                self.mode_kind().await,
                BotMode::Paper | BotMode::Shadow | BotMode::Simulation
            )
    }

    pub(crate) async fn refuse_mixed_mode_paper_exit(&self, trade: &Trade) -> bool {
        if trade.is_live() || self.paper_execution_allowed().await {
            return false;
        }
        self.set_degraded_no_trade(true);
        self.halt_trading("paper_live_accounting_conflict",
            "paper inventory cannot settle into a live or wallet-backed process; recover it in an isolated paper process")
            .await;
        true
    }

    pub(crate) async fn has_exact_complementary_pair(&self, first_id: i64, second_id: i64) -> bool {
        let trades = self.portfolio_state.trades.read().await;
        match (
            trades.iter().find(|trade| trade.id == first_id),
            trades.iter().find(|trade| trade.id == second_id),
        ) {
            (Some(first), Some(second)) => is_exact_complementary_pair(first, second),
            _ => false,
        }
    }

    pub(crate) async fn require_reconciled_live_exits(&self) -> anyhow::Result<()> {
        if !self.recover_live_orders_or_halt().await {
            anyhow::bail!("live order recovery requires more verified evidence");
        }
        if !db::live::pending_orders(&self.db).await?.is_empty() {
            self.set_degraded_no_trade(true);
            self.halt_trading(
                "live_order_settlement_pending",
                "live order settlement is still pending; entries remain frozen",
            )
            .await;
            anyhow::bail!("live order settlement is still pending; entries remain frozen");
        }
        let detail = match db::load_trades_by_status(&self.db, TradeStatus::Closing).await {
            Ok(rows) => {
                let ids: Vec<_> = rows.iter().filter(|t| t.is_live()).map(|t| t.id).collect();
                if ids.is_empty() {
                    return Ok(());
                }
                format!("Unresolved live exits require venue reconciliation: trade_ids={ids:?}")
            }
            Err(err) => format!("Cannot verify unresolved live exits: {err}"),
        };
        self.set_degraded_no_trade(true);
        self.halt_trading("live_exit_requires_reconciliation", detail.clone())
            .await;
        anyhow::bail!(detail)
    }

    /// Close every open position at market, then verify none survived.
    ///
    /// This is the shutdown contract. It was previously inline in `main.rs`,
    /// which made the multi-open shutdown proof impossible to run without
    /// launching the binary against live markets. It returns an outcome so both
    /// the binary and the acceptance tests can assert on the same result.
    ///
    /// Failing to close is not treated as success: a surviving OPEN row records
    /// a reconcile error under the `shutdown_cleanup` phase.
    pub async fn shutdown_close_open_positions(&self) -> ShutdownCleanupOutcome {
        // Freeze entry creation before taking the initial snapshot. Commands
        // already accepted by the actor remain ordered, but execute paths see
        // the kill switch and cannot open fresh exposure behind the sweep.
        self.stop_loop();
        self.freeze_entries(
            "shutdown_sweep",
            "entries frozen while open positions are closed",
        )
        .await;

        // A flat portfolio can still have a resting BID. Cancel and reconcile
        // through the writer before snapshotting positions, including fills that
        // raced cancellation. No local flag stands in for a venue acknowledgement.
        let order_cleanup_error = if self.execution_state.execution_tx.read().await.is_some() {
            self.cancel_maker_orders_for_shutdown_serialized()
                .await
                .err()
                .map(|error| error.to_string())
        } else {
            None
        };
        let mut outcome = ShutdownCleanupOutcome {
            order_cleanup_error,
            attempted: self
                .portfolio_state
                .trades
                .read()
                .await
                .iter()
                .filter(|t| t.is_open())
                .map(|t| t.id)
                .collect(),
            ..Default::default()
        };
        outcome.attempted.sort_unstable();

        // Deliberately no early return on an empty hot state. Hot state is a
        // cache; the database is the authority. A position that is OPEN on disk
        // but missing from memory is exactly the case worth catching, and
        // skipping the check would report it as a clean shutdown.
        if !outcome.attempted.is_empty() {
            info!(
                "🛑 Graceful shutdown: closing {} open position(s)...",
                outcome.attempted.len()
            );
        }

        for trade_id in outcome.attempted.clone() {
            match self
                .close_trade_serialized_with_intent(trade_id, None, CloseIntent::ShutdownMark)
                .await
            {
                Ok(Some(closed)) => {
                    if closed.status_kind() != TradeStatus::Closed {
                        warn!(
                            "  Shutdown close left trade {} OPEN (exit_type={:?})",
                            closed.id, closed.exit_type
                        );
                        outcome.failed.push(closed.id);
                        outcome.still_open.push(closed.id);
                    } else {
                        info!(
                            "  Closed trade {} @ {:.4} P&L=${:.2}",
                            closed.id,
                            closed.exit_price.unwrap_or(0.0),
                            closed.pnl.unwrap_or(0.0)
                        );
                        outcome.closed.push(closed.id);
                    }
                }
                Ok(None) => {
                    warn!("  Graceful shutdown close returned no trade for trade_id={trade_id}");
                    outcome.failed.push(trade_id);
                }
                Err(e) => {
                    warn!("  Graceful shutdown close failed for trade_id={trade_id}: {e}");
                    outcome.failed.push(trade_id);
                }
            }
        }

        self.verify_shutdown_exposure(&mut outcome).await;

        if outcome.verification_failed() {
            // Nothing further can be asserted about exposure.
        } else if !outcome.remaining_open.is_empty() {
            warn!(
                "🛑 Shutdown completed with {} OPEN trade(s) still persisted: {:?}",
                outcome.remaining_open.len(),
                outcome.remaining_open
            );
            self.record_reconcile_error(
                "shutdown_cleanup",
                format!(
                    "shutdown left OPEN trades persisted failed_trade_ids={:?} still_open_trade_ids={:?} remaining_open_ids={:?}",
                    outcome.failed, outcome.still_open, outcome.remaining_open
                ),
            )
            .await;
        } else if !outcome.failed.is_empty() {
            warn!(
                "🛑 Shutdown close attempts had failures but no OPEN trades remain: {:?}",
                outcome.failed
            );
        }

        outcome
    }

    pub async fn verify_shutdown_exposure(&self, outcome: &mut ShutdownCleanupOutcome) {
        match sqlx::query_as::<_, Trade>(
            "SELECT * FROM trades WHERE status IN ('OPEN', 'CLOSING') ORDER BY id",
        )
        .fetch_all(&self.db)
        .await
        {
            Ok(rows) => outcome.remaining_open = rows.iter().map(|trade| trade.id).collect(),
            Err(error) => {
                let message = format!("could not read persisted trades: {error}");
                outcome.verification_error = Some(message.clone());
                self.record_reconcile_error("shutdown_verification", message)
                    .await;
            }
        }
        let orders = sqlx::query_scalar::<_, String>(
            "SELECT 'maker:'||order_id FROM maker_orders WHERE state NOT IN ('TERMINAL','REJECTED')
             UNION ALL SELECT 'taker:'||order_id FROM live_orders WHERE state IN ('pending','audit_uncertain')
             UNION ALL SELECT 'paper:'||order_id FROM paper_maker_orders WHERE status='OPEN' ORDER BY 1"
        ).fetch_all(&self.db).await;
        match orders {
            Ok(orders) => {
                outcome.remaining_orders = orders;
                if !outcome.remaining_orders.is_empty() {
                    self.record_reconcile_error(
                        "shutdown_order_cleanup",
                        format!("unresolved orders: {:?}", outcome.remaining_orders),
                    )
                    .await;
                }
            }
            Err(error) => {
                let message = format!("could not read persisted orders: {error}");
                outcome.verification_error = Some(message.clone());
                self.record_reconcile_error("shutdown_verification", message)
                    .await;
            }
        }
    }

    async fn restore_hot_trade_after_close_abort(&self, trade: &Trade) {
        {
            let mut trades = self.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|t| t.id == trade.id) {
                *t = trade.clone();
            }
        }
        self.trade_lifecycle
            .closing_in_progress
            .write()
            .await
            .remove(&trade.id);
    }

    // ── Full close ────────────────────────────────────────────────────────────

    /// Close an open trade by ID at market.  Handles live FOK sell, PnL
    /// calculation, DB update, and balance reconciliation.
    pub async fn close_trade_with_intent(
        &self,
        trade_id: i64,
        exit_price: Option<f64>,
        intent: CloseIntent,
    ) -> Option<Trade> {
        // A merge is an atomic two-lot settlement, never a standalone mark.
        if intent == CloseIntent::Merge {
            return None;
        }
        // ── Find trade ────────────────────────────────────────────────────────
        let initial_trade = {
            let trades = self.portfolio_state.trades.read().await;
            trades.iter().find(|t| t.id == trade_id).cloned()?
        };

        if self.refuse_mixed_mode_paper_exit(&initial_trade).await {
            return None;
        }
        if !initial_trade.is_open() {
            warn!(
                "close_trade: trade {} is not open (status={})",
                trade_id, initial_trade.status
            );
            return None;
        }

        // Atomic guard: prevent concurrent close attempts for the same trade.
        // If another task is already closing this trade, bail out immediately.
        {
            let mut closing = self.trade_lifecycle.closing_in_progress.write().await;
            if !closing.insert(trade_id) {
                warn!("close_trade: trade {trade_id} is already being closed by another task");
                return None;
            }
        }

        let sim_live = {
            let cfg = self.control_plane.config.read().await;
            cfg.sim_live_execution && cfg.mode_kind() != BotMode::Live
        };
        let simulated_maker_trade = initial_trade.strategy == "maker" && !initial_trade.is_live();
        let pending_sim_exit_evidence = self
            .trade_lifecycle
            .pending_sim_exit_evidence
            .write()
            .await
            .remove(&trade_id);

        // Mark CLOSING in memory (lightweight, avoids re-entry)
        {
            let mut trades = self.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|t| t.id == trade_id) {
                t.set_status(TradeStatus::Closing);
            }
        }

        let mut trade = initial_trade.clone();
        trade.set_status(TradeStatus::Closing);

        // ── Live mode: FOK sell ───────────────────────────────────────────────
        let mut actual_exit_price = exit_price;
        let mut exit_fill_snapshot: Option<(
            String,
            &'static str,
            f64,
            f64,
            f64,
            &'static str,
            String,
        )> = None;
        let cleanup_mark_close =
            !trade.is_live() && exit_price.is_none() && intent == CloseIntent::ShutdownMark;
        // A simulated position closed with no price named: sell it into the
        // public bids like any taker would, whatever strategy opened it.
        let aggressive_exit = matches!(intent, CloseIntent::Market | CloseIntent::Manual);
        let simulated_market_close = !trade.is_live()
            && aggressive_exit
            && (simulated_maker_trade
                || sim_live
                || exit_price.is_none()
                || intent == CloseIntent::Manual);
        let manual_market_close = simulated_market_close && intent == CloseIntent::Manual;
        let is_resolution_exit = intent == CloseIntent::Resolution;

        if trade.is_live() {
            let outcome = self.close_confirmed_live_position(&initial_trade).await;
            if outcome.is_none() {
                if let Ok(Some(persisted)) = db::load_trade_by_id(&self.db, trade_id).await {
                    self.publish_live_trade_state(persisted).await;
                }
            }
            return outcome;
        } else if simulated_market_close {
            let mut token_id = trade.token_id.clone();
            if token_id.as_deref().unwrap_or("").is_empty() {
                let signal = match trade.direction_kind() {
                    crate::domain::Direction::BuyYes => crate::domain::Signal::BuyYes,
                    crate::domain::Direction::BuyNo => crate::domain::Signal::BuyNo,
                    _ => crate::domain::Signal::Hold,
                };
                token_id = self.get_clob_token_id(&trade.market_id, signal).await;
            }
            let Some(tid) = token_id.as_ref() else {
                warn!("SimLive close: missing token_id for trade {trade_id}");
                self.restore_hot_trade_after_close_abort(&initial_trade)
                    .await;
                return None;
            };
            if trade.token_id.as_deref().unwrap_or("").is_empty() {
                // Persist this only with the completed close. The temporary
                // CLOSING state must not escape if the book cannot fill us.
                trade.token_id = Some(tid.clone());
            }
            let sim_config = self.control_plane.config.read().await.clone();
            if !self
                .wait_paper_order_latency(sim_config.sim_order_latency_ms)
                .await
            {
                self.restore_hot_trade_after_close_abort(&initial_trade)
                    .await;
                return None;
            }
            let book = match fetch_public_order_book(tid).await {
                Ok(b) => b,
                Err(_) => {
                    warn!("SimLive close: order book fetch failed for {tid}");
                    self.restore_hot_trade_after_close_abort(&initial_trade)
                        .await;
                    return None;
                }
            };
            let bids = extract_levels(&book, "bids");
            let asks = extract_levels(&book, "asks");
            let best_bid = bids.first().map(|x| x.0).unwrap_or(0.50);
            let best_ask = asks.first().map(|x| x.0).unwrap_or(0.50);
            let spread = best_ask - best_bid;
            let total_usd: f64 = bids.iter().chain(asks.iter()).map(|(p, s)| p * s).sum();

            // Fetch midpoint from API (always available even on thin books)
            // One snapshot, one truth: the book already fetched is the source of its
            // own midpoint. `/midpoint` is a second call at a second instant, and on a
            // fast market the two disagree; see `book_mid`. It remains the fallback
            // for a one-sided book, where a midpoint against a defaulted 0.50 is not one.
            let mid = match crate::json_util::book_mid(&book) {
                Some(m) => m,
                None => fetch_public_midpoint(tid)
                    .await
                    .unwrap_or_else(|_| (best_bid + best_ask) / 2.0),
            };
            let book_snapshot =
                PublicBookSnapshot::from_order_book(tid, &book, Some(mid), "clob_rest_book");

            let thin_book = spread > 0.05 || total_usd < 50.0;
            if thin_book {
                // Noted, not shortcut: a thin book used to exit at the
                // midpoint, half a spread better than any seller gets, and
                // precisely when the spread is widest. The bids are walked
                // like any other book; a sale is worth what the bids pay.
                info!(
                    "SimLive close: thin book (spread={spread:.4}, depth=${total_usd:.2}) for trade {trade_id}; walking the bids"
                );
            }

            let Some((fill_price, intended_exit_price)) = super::simulation::taker_arrival_sell(
                &book,
                trade.shares,
                sim_config.sim_available_depth_fraction,
            ) else {
                warn!("SimLive close: insufficient executable bids for all shares of trade {trade_id}");
                self.restore_hot_trade_after_close_abort(&initial_trade)
                    .await;
                return Some(initial_trade.clone());
            };

            actual_exit_price = Some(fill_price);
            let fill_details = SimTradePriceEvidence::new(
                "exit",
                if manual_market_close {
                    "manual_orderbook"
                } else {
                    "sim_taker_orderbook"
                },
                "sell",
                intended_exit_price,
                fill_price,
                trade.shares,
                if manual_market_close {
                    "manual close, walked the bids"
                } else {
                    "simulated orderbook exit"
                },
                book_snapshot,
            )
            .as_details_json();
            exit_fill_snapshot = Some((
                format!("sim-close-{trade_id}"),
                "exit",
                intended_exit_price,
                fill_price,
                trade.shares,
                if manual_market_close {
                    "close_trade_manual"
                } else {
                    "close_trade_sim_live"
                },
                fill_details,
            ));
        }

        if intent == CloseIntent::PassiveFill {
            if let Some(evidence) = pending_sim_exit_evidence.as_ref() {
                exit_fill_snapshot = Some((
                    evidence.order_id.clone(),
                    if evidence.phase == "partial_exit" {
                        "partial_exit"
                    } else if evidence.phase == "mark_exit" {
                        "mark_exit"
                    } else {
                        "exit"
                    },
                    evidence.intended_price,
                    evidence.avg_price,
                    evidence.shares,
                    if evidence.source == "shutdown_cleanup" {
                        "shutdown_cleanup"
                    } else if evidence.source == "maker_flatten" {
                        "maker_flatten"
                    } else {
                        "sim"
                    },
                    evidence.details.clone(),
                ));
            }
        }

        if simulated_maker_trade
            && (intent == CloseIntent::PassiveFill || cleanup_mark_close)
            && exit_fill_snapshot.is_none()
        {
            let maker_snapshot = if cleanup_mark_close {
                local_mark_snapshot(
                    trade.token_id.as_deref(),
                    actual_exit_price.unwrap_or(trade.entry_price),
                    "shutdown_cleanup",
                )
            } else {
                match trade.token_id.as_deref() {
                    Some(token_id) if looks_like_real_token_id(token_id) => {
                        fetch_public_book_snapshot(token_id)
                            .await
                            .ok()
                            .unwrap_or_else(|| {
                                local_mark_snapshot(
                                    Some(token_id),
                                    actual_exit_price.unwrap_or(trade.entry_price),
                                    "local_mark_fallback",
                                )
                            })
                    }
                    Some(token_id) if !token_id.trim().is_empty() => local_mark_snapshot(
                        Some(token_id),
                        actual_exit_price.unwrap_or(trade.entry_price),
                        "local_mark_fallback",
                    ),
                    _ => local_mark_snapshot(
                        None,
                        actual_exit_price.unwrap_or(trade.entry_price),
                        "local_mark_fallback",
                    ),
                }
            };
            if cleanup_mark_close {
                let ep = actual_exit_price.unwrap_or(trade.entry_price);
                let details = SimTradePriceEvidence::new(
                    "mark_exit",
                    "shutdown_cleanup",
                    "sell",
                    ep,
                    ep,
                    trade.shares,
                    "forced simulation cleanup mark",
                    maker_snapshot.clone(),
                )
                .as_details_json();
                exit_fill_snapshot = Some((
                    format!("mark-exit-{trade_id}"),
                    "mark_exit",
                    ep,
                    ep,
                    trade.shares,
                    "shutdown_cleanup",
                    details,
                ));
            } else if exit_fill_snapshot.is_none() {
                let ep = actual_exit_price.unwrap_or(trade.entry_price);
                let details = SimTradePriceEvidence::new(
                    "exit",
                    "maker_flatten",
                    "sell",
                    ep,
                    ep,
                    trade.shares,
                    "simulated maker flatten exit",
                    maker_snapshot.clone(),
                )
                .as_details_json();
                exit_fill_snapshot = Some((
                    format!("maker-exit-{trade_id}"),
                    "exit",
                    ep,
                    ep,
                    trade.shares,
                    "maker_flatten",
                    details,
                ));
            }
        }

        // ── Compute PnL ───────────────────────────────────────────────────────
        // ep is always the EXIT TOKEN price:
        //   BUY_YES → YES token (entry=0.60 → win ep=1.0 → PnL = shares × 0.40)
        //   BUY_NO  → NO  token (entry=0.40 → win ep=1.0 → PnL = shares × 0.60)
        // The same formula works for both because entry_price is stored as the
        // held token's price in execute.rs, not always the YES price.
        let ep = actual_exit_price.unwrap_or(trade.entry_price);
        let mut pnl_remaining = token_pnl(trade.shares, trade.entry_price, ep);

        // For paper/sim: deduct exit taker fee on early sells (not resolution).
        // Entry taker fee was already charged upfront from balance at open.
        // Only aggressive sales have a taker fee; explicit settlement does not.
        if !trade.is_live() && aggressive_exit && !is_resolution_exit {
            let exit_fee = crate::fusion::estimate_taker_fee(ep);
            let exit_value = trade.shares * ep;
            pnl_remaining -= exit_fee * exit_value;
        }

        // Partial closes may already have realised PnL on this open trade.
        let realised_so_far = if trade.is_live() {
            0.0
        } else {
            trade.pnl.unwrap_or(0.0)
        };
        let total_pnl = realised_so_far + pnl_remaining;
        // The market label is independent of profits taken before settlement.
        let was_correct: i64 = if is_resolution_exit {
            i64::from(ep > 0.5)
        } else {
            i64::from(total_pnl - trade.fee_paid > 0.0)
        };

        // ── Build closed trade ────────────────────────────────────────────────
        let now = Utc::now().to_rfc3339();
        trade.set_status(TradeStatus::Closed);
        trade.exit_price = Some(ep);
        trade.exit_timestamp = Some(now);
        trade.pnl = Some(total_pnl);
        let inferred_exit_type = if cleanup_mark_close {
            "shutdown_cleanup"
        } else if intent == CloseIntent::PassiveFill {
            "maker_flatten"
        } else if is_resolution_exit {
            "auto_resolved"
        } else if intent == CloseIntent::Merge {
            "merged"
        } else {
            "manual"
        };
        trade.exit_type = Some(inferred_exit_type.to_string());
        trade.was_correct = Some(was_correct);
        trade.fok_failed = Some(0); // FOK failures return early above

        // A resolution or explicit accounting mark is evidence too, but never
        // pretend it was an executable orderbook sale.
        if exit_fill_snapshot.is_none() {
            let path = if is_resolution_exit {
                "resolution"
            } else if intent == CloseIntent::Merge {
                "pair_merge"
            } else if cleanup_mark_close {
                "shutdown_cleanup"
            } else {
                "accounting_mark"
            };
            let details = SimTradePriceEvidence::new(
                "mark_exit",
                path,
                "sell",
                ep,
                ep,
                trade.shares,
                "explicit simulated accounting payout or mark; no exchange execution",
                local_mark_snapshot(trade.token_id.as_deref(), ep, path),
            )
            .as_details_json();
            exit_fill_snapshot = Some((
                format!("mark-exit-{trade_id}"),
                "mark_exit",
                ep,
                ep,
                trade.shares,
                path,
                details,
            ));
        }
        let (order_id, phase, intended_px, avg_px, cum_sz, source, details) =
            exit_fill_snapshot.as_ref()?;
        let evidence = db::paper::PaperExecutionEvidence {
            order_id,
            phase,
            intended_price: *intended_px,
            avg_price: *avg_px,
            shares: *cum_sz,
            source,
            details,
        };
        if let Err(error) =
            db::paper::apply_full_exit(&self.db, &initial_trade, &trade, &evidence).await
        {
            self.set_degraded_no_trade(true);
            self.halt_trading("paper_full_exit_persistence", &error.to_string())
                .await;
            warn!("full exit lot and evidence were not committed for trade {trade_id}: {error}");
            self.restore_hot_trade_after_close_abort(&initial_trade)
                .await;
            return None;
        }

        // Runtime entry/exit fill-parity verification against persisted exchange
        // fill snapshots. Emits a warning if the stored trade prices drift too
        // far from exchange VWAP evidence.
        if trade.is_live() {
            let max_entry_drift_bps =
                env_f64_or("MACBOT_ENTRY_PARITY_MAX_DRIFT_BPS", 35.0).max(0.0);
            let max_exit_drift_bps = env_f64_or("MACBOT_EXIT_PARITY_MAX_DRIFT_BPS", 35.0).max(0.0);
            if let Ok(summary) = db::trade_fill_summary(&self.db, trade.id).await {
                let parity = db::trade_fill_parity(
                    &trade,
                    &summary,
                    max_entry_drift_bps,
                    max_exit_drift_bps,
                );
                if parity.overall_status == "mismatch" {
                    warn!(
                        "Fill parity mismatch trade_id={} entry_drift_bps={:?} exit_drift_bps={:?} entry_status={} exit_status={}",
                        trade.id,
                        parity.entry_drift_bps,
                        parity.exit_drift_bps,
                        parity.entry_status,
                        parity.exit_status
                    );
                }
            }
        }

        // ── Update in-memory list ─────────────────────────────────────────────
        {
            let mut trades = self.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|t| t.id == trade_id) {
                *t = trade.clone();
            }
        }

        // ── Balance reconciliation ────────────────────────────────────────────
        if trade.is_live() {
            self.sync_balance().await;
        } else {
            // Paper/sim: return collateral + PnL
            let bal = self.balance().await;
            let new_bal = (bal + trade.amount + pnl_remaining).max(0.0);
            self.set_balance(new_bal).await;
        }

        info!(
            "Trade {} closed @ {ep:.4} PnL={total_pnl:+.2} ({})",
            trade_id,
            if was_correct == 1 {
                "WIN"
            } else if total_pnl == 0.0 {
                "FLAT"
            } else {
                "LOSS"
            }
        );
        metrics::inc_trades_closed();

        // Notify on large losses
        if total_pnl < -2.0 {
            let tid = trade_id;
            let pnl = total_pnl;
            tokio::spawn(async move {
                crate::notifications::notify_large_loss(tid, pnl).await;
            });
        }

        // Evict closed trades from hot memory so the Vec doesn't grow unbounded.
        self.evict_closed_trades().await;

        // Release the closing guard so future close attempts (e.g. retry) are allowed.
        self.trade_lifecycle
            .closing_in_progress
            .write()
            .await
            .remove(&trade_id);

        Some(trade)
    }

    // ── FOK close retry ────────────────────────────────────────────────────────

    /// Spawn a background task that retries closing a trade after FOK failure.
    /// Uses exponential backoff with up to `FOK_RETRY_MAX_ATTEMPTS` retries.
    /// Each retry goes through the full serialized close path (actor channel).
    pub fn spawn_fok_close_retry(
        bot: std::sync::Arc<Self>,
        trade_id: i64,
        exit_price: Option<f64>,
        intent: CloseIntent,
    ) {
        if !bot
            .trade_lifecycle
            .fok_retry_in_progress
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(trade_id)
        {
            return;
        }
        struct RetryGuard(std::sync::Arc<TradingBot>, i64);
        impl Drop for RetryGuard {
            fn drop(&mut self) {
                self.0
                    .trade_lifecycle
                    .fok_retry_in_progress
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&self.1);
            }
        }
        let guard = RetryGuard(std::sync::Arc::clone(&bot), trade_id);
        crate::task_monitor::spawn_monitored_task("fok_close_retry", async move {
            let _guard = guard;
            Self::retry_fok_close(
                bot,
                trade_id,
                exit_price,
                intent,
                Duration::from_secs(FOK_RETRY_BASE_DELAY_SECS),
            )
            .await;
        });
    }

    async fn retry_fok_close(
        bot: std::sync::Arc<Self>,
        trade_id: i64,
        exit_price: Option<f64>,
        intent: CloseIntent,
        base_delay: Duration,
    ) {
        for attempt in 1..=FOK_RETRY_MAX_ATTEMPTS {
            let delay = base_delay * 2u32.pow(attempt - 1);
            info!(
                "FOK retry: trade {trade_id} attempt {attempt}/{FOK_RETRY_MAX_ATTEMPTS} in {}s",
                delay.as_secs()
            );
            sleep(delay).await;

            // Check if trade is still open
            let still_open = {
                let trades = bot.portfolio_state.trades.read().await;
                trades.iter().any(|t| t.id == trade_id && t.is_open())
            };
            if !still_open {
                info!("FOK retry: trade {trade_id} already closed, stopping retries");
                return;
            }

            // Attempt close through the serialized actor path
            match bot
                .close_trade_serialized_with_intent(trade_id, exit_price, intent)
                .await
            {
                Ok(Some(t)) if t.status_kind() == TradeStatus::Closed => {
                    info!(
                        "FOK retry: trade {trade_id} closed on attempt {attempt} @ {:.4}",
                        t.exit_price.unwrap_or(0.0)
                    );
                    return;
                }
                Ok(_) => {
                    warn!("FOK retry: trade {trade_id} attempt {attempt} did not close");
                }
                Err(e) => {
                    warn!("FOK retry: trade {trade_id} attempt {attempt} actor error: {e:?}");
                }
            }
        }
        warn!(
            "FOK retry: trade {trade_id} exhausted {FOK_RETRY_MAX_ATTEMPTS} retries — \
                 manual intervention required"
        );
        crate::notifications::notify_fok_retry_exhausted(trade_id).await;
    }

    // ── Partial close ─────────────────────────────────────────────────────────

    /// Reduce a trade by `fraction` (0.0..1.0).
    /// Returns JSON with id, pnl, remaining_shares, partial=true.
    pub async fn partial_close_trade(
        &self,
        trade_id: i64,
        exit_price: f64,
        fraction: f64,
    ) -> Option<Value> {
        self.partial_close_trade_with_intent(trade_id, exit_price, fraction, CloseIntent::Market)
            .await
    }

    pub async fn partial_close_trade_with_intent(
        &self,
        trade_id: i64,
        exit_price: f64,
        fraction: f64,
        intent: CloseIntent,
    ) -> Option<Value> {
        self.partial_close_trade_with_request(trade_id, exit_price, fraction, intent, None)
            .await
    }

    pub(crate) async fn partial_close_trade_with_request(
        &self,
        trade_id: i64,
        exit_price: f64,
        fraction: f64,
        intent: CloseIntent,
        request_id: Option<u64>,
    ) -> Option<Value> {
        if !fraction.is_finite()
            || !exit_price.is_finite()
            || !(0.0..=1.0).contains(&exit_price)
            || fraction <= 0.0
            || fraction >= 1.0
        {
            return None;
        }

        let trade_snapshot = {
            let trades = self.portfolio_state.trades.read().await;
            trades.iter().find(|t| t.id == trade_id).cloned()?
        };

        if self.refuse_mixed_mode_paper_exit(&trade_snapshot).await {
            return Some(json!({"ok":false,"id":trade_id,"status":"rejected",
                "reason":"paper inventory cannot settle into live or wallet-backed cash; use an isolated paper process",
                "remaining_shares":trade_snapshot.shares,"partial":true,"pnl":null}));
        }
        if !(trade_snapshot.is_open()
            || trade_snapshot.is_live() && trade_snapshot.status_kind() == TradeStatus::Closing)
        {
            return None;
        }

        if trade_snapshot.is_live() {
            if !matches!(intent, CloseIntent::Market | CloseIntent::Manual) {
                return Some(json!({"ok":false,"id":trade_id,"status":"rejected",
                    "reason":"live partial closes require a receipt-backed market sale",
                    "remaining_shares":trade_snapshot.shares,"pnl":null,"partial":true}));
            }
            return match self
                .close_confirmed_live_quantity_for_request(
                    &trade_snapshot,
                    Some(fraction),
                    request_id,
                )
                .await
            {
                Ok(outcome) => Some(outcome),
                Err(error) => {
                    self.set_degraded_no_trade(true);
                    self.halt_trading("live_partial_close_requires_evidence", error.to_string())
                        .await;
                    let persisted = db::load_trade_by_id(&self.db, trade_id)
                        .await
                        .ok()
                        .flatten();
                    if let Some(trade) = persisted.as_ref() {
                        self.publish_live_trade_state(trade.clone()).await;
                    }
                    Some(json!({"ok":false,"id":trade_id,"status":"pending",
                        "reason":error.to_string(),"remaining_shares":persisted.as_ref().map(|trade|trade.shares),
                        "pnl":null,"partial":true}))
                }
            };
        }

        let closed_shares = trade_snapshot.shares * fraction;
        let closed_amount = trade_snapshot.amount * fraction;
        let remaining_shares = trade_snapshot.shares - closed_shares;
        if !closed_shares.is_finite()
            || closed_shares <= 1e-9
            || !closed_amount.is_finite()
            || closed_amount < 0.0
            || !remaining_shares.is_finite()
            || remaining_shares <= 0.0
            || remaining_shares >= trade_snapshot.shares
        {
            return None;
        }
        let aggressive_exit = matches!(intent, CloseIntent::Market | CloseIntent::Manual);
        let sim_config = self.control_plane.config.read().await.clone();
        let executed_book = if aggressive_exit
            && (trade_snapshot.strategy == "maker"
                || sim_config.sim_live_execution
                || intent == CloseIntent::Manual)
        {
            let token = trade_snapshot.token_id.as_deref()?;
            if !self
                .wait_paper_order_latency(sim_config.sim_order_latency_ms)
                .await
            {
                return None;
            }
            let book = fetch_public_order_book(token).await.ok()?;
            let (price, intended) = super::simulation::taker_arrival_sell(
                &book,
                closed_shares,
                sim_config.sim_available_depth_fraction,
            )?;
            Some((
                price,
                intended,
                PublicBookSnapshot::from_order_book(
                    token,
                    &book,
                    crate::json_util::book_mid(&book),
                    "clob_rest_book",
                ),
            ))
        } else {
            None
        };
        let exit_price = executed_book
            .as_ref()
            .map(|(price, _, _)| *price)
            .unwrap_or(exit_price);

        // Unified token-price PnL:
        // pnl = closed_shares * (exit_token_price - entry_token_price)
        // Works for both YES and NO tokens because entry_price is the held token price.
        let mut pnl = token_pnl(closed_shares, trade_snapshot.entry_price, exit_price);

        // Paper/sim early exits pay taker fee on exit notional.
        if aggressive_exit {
            let exit_fee = crate::fusion::estimate_taker_fee(exit_price);
            let exit_value = closed_shares * exit_price;
            pnl -= exit_fee * exit_value;
        }

        // Derive remaining_amount from remaining_shares to maintain the
        // shares/amount ratio invariant across multiple partial closes.
        let remaining_amount = remaining_shares * trade_snapshot.entry_price;

        // Update trade in-place
        let mut updated = trade_snapshot.clone();
        updated.shares = remaining_shares;
        updated.amount = remaining_amount;
        updated.partial_closed = Some(1);
        // Accumulate PnL in the record for reporting
        updated.pnl = Some(updated.pnl.unwrap_or(0.0) + pnl);

        let pending_evidence = self
            .trade_lifecycle
            .pending_sim_exit_evidence
            .write()
            .await
            .remove(&trade_id);
        let (order_id, source, fill_details, intended_px, avg_px, closed_size) =
            if let Some((price, intended, snapshot)) = executed_book {
                (
                    format!(
                        "partial-market-{trade_id}-{:016x}",
                        remaining_shares.to_bits()
                    ),
                    "close_trade_sim_live".to_string(),
                    SimTradePriceEvidence::new(
                        "partial_exit",
                        "sim_taker_orderbook",
                        "sell",
                        intended,
                        price,
                        closed_shares,
                        "partial sale, walked the bids",
                        snapshot,
                    )
                    .as_details_json(),
                    intended,
                    price,
                    closed_shares,
                )
            } else if let Some(evidence) = pending_evidence {
                (
                    evidence.order_id,
                    evidence.source,
                    evidence.details,
                    evidence.intended_price,
                    evidence.avg_price,
                    evidence.shares,
                )
            } else {
                let snapshot = match trade_snapshot.token_id.as_deref() {
                    Some(token_id) if looks_like_real_token_id(token_id) => {
                        fetch_public_book_snapshot(token_id)
                            .await
                            .ok()
                            .unwrap_or_else(|| {
                                local_mark_snapshot(
                                    Some(token_id),
                                    exit_price,
                                    "local_mark_fallback",
                                )
                            })
                    }
                    Some(token_id) if !token_id.trim().is_empty() => {
                        local_mark_snapshot(Some(token_id), exit_price, "local_mark_fallback")
                    }
                    _ => local_mark_snapshot(None, exit_price, "local_mark_fallback"),
                };
                (
                    // Separate executions are not cumulative snapshots of the same
                    // order. Reusing a 50pct ID lost every subsequent half-close.
                    format!("partial-{trade_id}-{:016x}", remaining_shares.to_bits()),
                    "sim".to_string(),
                    SimTradePriceEvidence::new(
                        "partial_exit",
                        "partial_close",
                        "sell",
                        exit_price,
                        exit_price,
                        closed_shares,
                        &format!("partial {:.0}% close, pnl={:+.4}", fraction * 100.0, pnl),
                        snapshot,
                    )
                    .as_details_json(),
                    exit_price,
                    exit_price,
                    closed_shares,
                )
            };
        if let Err(error) = db::paper::apply_partial_exit(
            &self.db,
            &trade_snapshot,
            &updated,
            &order_id,
            intended_px,
            avg_px,
            closed_size,
            &source,
            &fill_details,
        )
        .await
        {
            self.set_degraded_no_trade(true);
            self.halt_trading("paper_partial_exit_persistence", &error.to_string())
                .await;
            warn!("partial exit lot and fill were not committed for trade {trade_id}: {error}");
            return None;
        }
        {
            let mut trades = self.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|t| t.id == trade_id) {
                *t = updated;
            }
        }
        let bal = self.balance().await;
        self.set_balance(bal + closed_amount + pnl).await;
        metrics::inc_partial_closes();
        info!(
            "Partial close {:.0}% of trade {} @ {:.4} pnl={:+.2}",
            fraction * 100.0,
            trade_id,
            exit_price,
            pnl
        );

        Some(json!({
            "ok": true,
            "status": "settled",
            "id":               trade_id,
            "pnl":              pnl,
            "remaining_shares": remaining_shares,
            "remaining_amount": remaining_amount,
            "partial":          true,
        }))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{token_pnl, CloseIntent};

    #[tokio::test]
    async fn repeated_partial_sales_preserve_every_fill_and_its_weighted_price() {
        let (bot, trade, path) = maker_exit_fixture(8.0).await;
        for (price, fraction) in [(0.60, 0.5), (0.70, 0.5)] {
            assert!(bot
                .partial_close_trade_with_intent(
                    trade.id,
                    price,
                    fraction,
                    CloseIntent::PassiveFill
                )
                .await
                .is_some());
        }
        let summary = crate::db::trade_fill_summary(&bot.db, trade.id)
            .await
            .unwrap();
        assert_eq!(summary.exit_filled_size, 6.0);
        assert!((summary.exit_vwap.unwrap() - (4.0 * 0.60 + 2.0 * 0.70) / 6.0).abs() < 1e-9);
        assert_eq!(
            crate::db::load_trade_fills(&bot.db, trade.id, 20)
                .await
                .unwrap()
                .len(),
            2
        );
        for fraction in [0.0, 1.0, 1e-20, f64::NAN, f64::INFINITY] {
            assert!(bot
                .partial_close_trade_with_intent(trade.id, 0.60, fraction, CloseIntent::PassiveFill)
                .await
                .is_none());
        }
        let persisted = crate::db::load_trade_by_id(&bot.db, trade.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.shares, 2.0);
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn failed_partial_fill_insert_rolls_back_lot_cash_and_hot_state() {
        let (bot, trade, path) = maker_exit_fixture(8.0).await;
        let cash = bot.balance().await;
        sqlx::query("CREATE TRIGGER fail_partial_fill BEFORE INSERT ON trade_fills WHEN NEW.phase='partial_exit' BEGIN SELECT RAISE(ABORT,'simulated fill persistence failure'); END")
            .execute(&bot.db).await.unwrap();
        assert!(bot
            .partial_close_trade_with_intent(trade.id, 0.6, 0.5, CloseIntent::PassiveFill)
            .await
            .is_none());
        let persisted = crate::db::load_trade_by_id(&bot.db, trade.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.shares, trade.shares);
        assert_eq!(persisted.amount, trade.amount);
        assert_eq!(persisted.pnl, trade.pnl);
        assert_eq!(persisted.partial_closed, trade.partial_closed);
        let hot = bot.portfolio_state.trades.read().await;
        assert_eq!(
            hot.iter().find(|t| t.id == trade.id).unwrap().shares,
            trade.shares
        );
        drop(hot);
        assert_eq!(bot.balance().await, cash);
        assert!(crate::db::load_trade_fills(&bot.db, trade.id, 20)
            .await
            .unwrap()
            .is_empty());
        assert!(bot.kill_switch_active());
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn failed_full_exit_evidence_rolls_back_zero_payout_lot_cash_and_hot_state() {
        let (bot, trade, path) = maker_exit_fixture(8.0).await;
        let cash = bot.balance().await;
        sqlx::query("CREATE TRIGGER reject_full_exit BEFORE INSERT ON trade_fills BEGIN SELECT RAISE(ABORT,'fixture full-exit failure'); END")
            .execute(&bot.db).await.unwrap();
        assert!(bot
            .close_trade_with_intent(trade.id, Some(0.0), CloseIntent::Resolution)
            .await
            .is_none());
        let persisted = db::load_trade_by_id(&bot.db, trade.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(&persisted).unwrap(),
            serde_json::to_value(&trade).unwrap()
        );
        assert_eq!(bot.portfolio_state.trades.read().await[0].status, "OPEN");
        assert_eq!(bot.balance().await, cash);
        assert!(db::load_trade_fills(&bot.db, trade.id, 20)
            .await
            .unwrap()
            .is_empty());
        assert!(bot.kill_switch_active());
        assert!(!bot
            .trade_lifecycle
            .closing_in_progress
            .read()
            .await
            .contains(&trade.id));
        sqlx::query("DROP TRIGGER reject_full_exit")
            .execute(&bot.db)
            .await
            .unwrap();
        let closed = bot
            .close_trade_with_intent(trade.id, Some(0.0), CloseIntent::Resolution)
            .await
            .unwrap();
        assert_eq!(closed.exit_price, Some(0.0));
        let fills = db::load_trade_fills(&bot.db, trade.id, 20).await.unwrap();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].phase, "mark_exit");
        assert_eq!(fills[0].avg_price, 0.0);
        assert_eq!(fills[0].source, "resolution");
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn shutdown_does_not_report_unresolved_closing_exposure_as_clean() {
        let (bot, mut trade, path) = maker_exit_fixture(2.0).await;
        trade.status = "CLOSING".into();
        crate::db::update_trade(&bot.db, &trade).await.unwrap();
        bot.portfolio_state.trades.write().await.clear();
        let result = bot.shutdown_close_open_positions().await;
        assert!(!result.is_clean());
        assert_eq!(result.remaining_open, vec![trade.id]);
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn profitable_partial_sales_do_not_change_the_resolved_market_label() {
        let (bot, trade, path) = maker_exit_fixture(8.0).await;
        assert!(bot
            .partial_close_trade_with_intent(trade.id, 0.90, 0.75, CloseIntent::PassiveFill)
            .await
            .is_some());
        let closed = bot
            .close_trade_with_intent(trade.id, Some(0.0), CloseIntent::Resolution)
            .await
            .unwrap();
        assert!(closed.pnl.unwrap() > 0.0);
        assert_eq!(
            closed.was_correct,
            Some(0),
            "the held outcome lost despite profitable earlier sales"
        );
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn startup_refuses_an_unreadable_position_database() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        assert!(super::TradingBot::new(fixture(), pool.clone())
            .await
            .is_err());
        pool.close().await;
    }

    #[tokio::test]
    async fn live_close_without_finalized_rpc_cannot_trust_an_order_acknowledgement() {
        let (client, server) = crate::clob::client::tests::mock_venue(0).await;
        let (mut bot, mut trade, path) = maker_exit_fixture(5.0).await;
        trade.mode = "live".into();
        trade.accounting_version = 1;
        trade.token_id = Some("123456789".into());
        db::update_trade(&bot.db, &trade).await.unwrap();
        *bot.portfolio_state.trades.write().await = vec![trade.clone()];
        bot.clob = Some(std::sync::Arc::new(client));
        assert!(bot
            .close_trade_with_intent(trade.id, None, CloseIntent::Manual)
            .await
            .is_none());
        assert!(bot.kill_switch_active());
        let stored = db::load_trade_by_id(&bot.db, trade.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, "OPEN");
        assert!(stored.exit_price.is_none());
        assert_eq!(
            server.requests().iter().filter(|r| r.0 == "POST").count(),
            0,
            "funded submissions require a working settlement verifier before POST"
        );
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn unsettled_live_exit_blocks_resubmission_and_keeps_exposure_across_restart() {
        for (behavior, shares, expected_posts) in [(6, 5.0, 0)] {
            let (client, server) = crate::clob::client::tests::mock_venue(behavior).await;
            let (mut bot, mut trade, path) = maker_exit_fixture(shares).await;
            trade.mode = "live".into();
            trade.accounting_version = 1;
            trade.token_id = Some("123456789".into());
            crate::db::update_trade(&bot.db, &trade).await.unwrap();
            *bot.portfolio_state.trades.write().await = vec![trade.clone()];
            // A durable intent survives the ambiguous boundary between a
            // committed intent and receiving the venue's POST reply.
            let signed = client
                .create_order(&crate::clob::LimitOrderArgs {
                    token_id: "123456789".into(),
                    price: 0.53,
                    size: shares,
                    side: crate::clob::Side::Sell,
                })
                .await
                .unwrap();
            db::live::prepare_order(
                &bot.db,
                trade.id,
                &client.account_address(),
                &signed,
                0.53,
                crate::clob::MarketFeeParameters {
                    rate: 0.0,
                    exponent: 1.0,
                    taker_only: true,
                },
                "exit",
            )
            .await
            .unwrap();
            bot.clob = Some(std::sync::Arc::new(client));
            assert!(bot
                .close_trade_with_intent(trade.id, None, CloseIntent::Manual)
                .await
                .is_none());
            assert!(bot.kill_switch_active());
            let stored = crate::db::load_trade_by_id(&bot.db, trade.id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.status, "CLOSING");
            assert_eq!(stored.shares, shares);
            assert!(stored.exit_price.is_none());
            let components = crate::db::balance_reconstruction_components(&bot.db)
                .await
                .unwrap();
            assert_eq!(components.open_live_count, 1);
            assert!((components.open_live_locked - trade.amount).abs() < 1e-9);
            use tower::ServiceExt;
            let api_bot = std::sync::Arc::new(
                super::TradingBot::new(
                    bot.control_plane.config.read().await.clone(),
                    bot.db.clone(),
                )
                .await
                .unwrap(),
            );
            api_bot.control_plane.config.write().await.api_key = "closing-read-test".into();
            let response = crate::api::router(api_bot)
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/trades")
                        .header("x-api-key", "closing-read-test")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 32_768)
                .await
                .unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert!(payload["trades"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["id"] == trade.id && t["status"] == "CLOSING"));
            assert!(bot
                .close_trade_with_intent(trade.id, None, CloseIntent::Manual)
                .await
                .is_none());
            assert_eq!(
                server.requests().iter().filter(|r| r.0 == "POST").count(),
                expected_posts
            );
            let cfg = bot.control_plane.config.read().await.clone();
            let restarted = super::TradingBot::new(cfg, bot.db.clone()).await.unwrap();
            assert!(restarted.kill_switch_active());
            assert!(restarted
                .portfolio_state
                .trades
                .read()
                .await
                .iter()
                .any(|t| t.id == trade.id && t.status == "CLOSING"));
            restarted.set_kill_switch(false);
            assert!(restarted.require_reconciled_live_exits().await.is_err());
            assert!(restarted.kill_switch_active());
            drop(restarted);
            cleanup_exit_fixture(&bot, &path).await;
        }
    }
    use crate::bot::state::TradingBot;
    use crate::config::Config;
    use crate::db::{self, Trade};
    use chrono::Utc;

    #[test]
    fn token_pnl_formula_is_direction_agnostic() {
        // Winning BUY_NO path: NO token bought at 0.40, settles at 1.00.
        let pnl = token_pnl(100.0, 0.40, 1.00);
        assert!((pnl - 60.0).abs() < 1e-9);
    }

    #[test]
    fn token_pnl_handles_loss() {
        let pnl = token_pnl(100.0, 0.40, 0.00);
        assert!((pnl + 40.0).abs() < 1e-9);
    }

    fn fixture() -> Config {
        serde_json::from_str(include_str!("../../config.json")).expect("valid fixture")
    }

    #[tokio::test]
    async fn maker_cleanup_close_is_labeled_and_snapshotted() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_close_cleanup_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;
        let bot = TradingBot::new(fixture(), pool.clone()).await?;

        let trade = Trade {
            id: 0,
            timestamp: Utc::now().to_rfc3339(),
            market_id: "mkt-1".to_string(),
            question: "maker cleanup".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.01,
            amount: 1.0,
            shares: 100.0,
            expected_value: 0.01,
            reason: "maker passive fill".to_string(),
            confidence: 0.5,
            status: "OPEN".to_string(),
            mode: "simulation".to_string(),
            source: "bot".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: Some(0.5),
            order_id: Some("maker-order-cleanup".to_string()),
            token_id: Some("tok-yes".to_string()),
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: None,
            run_id: Some("test-run".to_string()),
            strategy: "maker".to_string(),
        };
        let trade_id = db::insert_trade(&pool, &trade).await?;
        let inserted = db::load_open_trades(&pool)
            .await?
            .into_iter()
            .find(|t| t.id == trade_id)
            .expect("inserted trade");
        bot.portfolio_state.trades.write().await.push(inserted);

        let closed = bot
            .close_trade_with_intent(trade_id, None, CloseIntent::ShutdownMark)
            .await
            .expect("closed trade");
        assert_eq!(closed.exit_type.as_deref(), Some("shutdown_cleanup"));

        let fills = db::load_trade_fills(&pool, trade_id, 10).await?;
        assert!(
            fills
                .iter()
                .any(|f| f.phase == "mark_exit" && f.source == "shutdown_cleanup"),
            "expected shutdown cleanup mark snapshot"
        );
        let cleanup_fill = fills
            .iter()
            .find(|f| f.phase == "mark_exit" && f.source == "shutdown_cleanup")
            .expect("cleanup fill");
        let details: serde_json::Value =
            serde_json::from_str(&cleanup_fill.details).expect("json cleanup verification");
        assert_eq!(
            details.get("path").and_then(serde_json::Value::as_str),
            Some("shutdown_cleanup")
        );

        pool.close().await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    /// A one-request HTTP stub standing in for the public CLOB: answers
    /// every GET with the same book. Returns the base URL to point
    /// `MACBOT_CLOB_BASE` at.
    pub(crate) async fn serve_one_book(book: serde_json::Value) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind stub");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let body = book.to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        base
    }

    pub(crate) struct EnvRestore {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvRestore {
        pub(crate) fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.old.as_deref() {
                Some(old) => std::env::set_var(self.key, old),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// The Close button sends no price. That used to fall into the shutdown
    /// sweep's branch: a maker position closed at its own entry, P&L zero,
    /// `exit_type = shutdown_cleanup`. A manual close sells into the bids.
    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn manual_close_of_a_maker_position_sells_into_the_bids() -> anyhow::Result<()> {
        let _guard = crate::env_util::test_env_lock();
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_close_manual_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;
        let mut cfg = fixture();
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "maker".to_string();
        cfg.sim_available_depth_fraction = 0.25;
        let bot = TradingBot::new(cfg, pool.clone()).await?;

        // Haircut depth leaves 1.25 shares at 0.60. Selling all 2 shares
        // therefore also consumes 0.75 shares at 0.55, for VWAP 0.58125.
        let base = serve_one_book(serde_json::json!({
            "bids": [{"price": "0.55", "size": "100"}, {"price": "0.60", "size": "5"}],
            "asks": [{"price": "0.62", "size": "50"}, {"price": "0.70", "size": "10"}],
        }))
        .await;
        let _env = EnvRestore::set("MACBOT_CLOB_BASE", &base);

        let trade = Trade {
            id: 0,
            timestamp: Utc::now().to_rfc3339(),
            market_id: "mkt-manual".to_string(),
            question: "manual close".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.50,
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.01,
            reason: "maker passive fill".to_string(),
            confidence: 0.5,
            status: "OPEN".to_string(),
            mode: "simulation".to_string(),
            source: "bot".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: Some(0.5),
            order_id: Some("maker-order-manual".to_string()),
            token_id: Some("1234567890".to_string()),
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: None,
            run_id: Some("test-run".to_string()),
            strategy: "maker".to_string(),
        };
        let trade_id = db::insert_trade(&pool, &trade).await?;
        let inserted = db::load_open_trades(&pool)
            .await?
            .into_iter()
            .find(|t| t.id == trade_id)
            .expect("inserted trade");
        bot.portfolio_state.trades.write().await.push(inserted);

        let closed = bot
            .close_trade_with_intent(trade_id, None, CloseIntent::Manual)
            .await
            .expect("closed trade");
        assert_eq!(closed.exit_type.as_deref(), Some("manual"));
        let expected_price = (1.25 * 0.60 + 0.75 * 0.55) / 2.0;
        assert!(
            (closed.exit_price.expect("exit price") - expected_price).abs() < 1e-9,
            "the whole position must sell at the haircutted book VWAP"
        );
        let expected_gross_pnl = 2.0 * (expected_price - 0.50);
        let fee = crate::fusion::estimate_taker_fee(expected_price) * (2.0 * expected_price);
        let pnl = closed.pnl.expect("pnl");
        assert!(
            (pnl - (expected_gross_pnl - fee)).abs() < 1e-9,
            "pnl {pnl} should be {expected_gross_pnl} less fee {fee}"
        );
        assert_eq!(closed.was_correct, Some(1));

        let fills = db::load_trade_fills(&pool, trade_id, 10).await?;
        let exit = fills
            .iter()
            .find(|f| f.phase == "exit")
            .expect("exit fill recorded");
        assert_eq!(exit.source, "close_trade_manual");
        assert!((exit.avg_price - expected_price).abs() < 1e-9);
        assert!((exit.cumulative_size - 2.0).abs() < 1e-9);
        let details: serde_json::Value = serde_json::from_str(&exit.details)?;
        assert_eq!(
            details.get("path").and_then(serde_json::Value::as_str),
            Some("manual_orderbook")
        );
        assert!(
            !fills.iter().any(|f| f.source == "shutdown_cleanup"),
            "a manual close is not a shutdown mark"
        );

        pool.close().await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    pub(crate) async fn maker_exit_fixture(shares: f64) -> (TradingBot, Trade, std::path::PathBuf) {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let sequence = rand::random::<u64>();
        let path =
            std::env::temp_dir().join(format!("macbot_exit_regression_{unique}_{sequence}.sqlite"));
        let pool = db::open(path.to_str().unwrap()).await.unwrap();
        let mut cfg = fixture();
        cfg.mode = "simulation".into();
        cfg.execution_mode = "maker".into();
        cfg.maker_hold_positions = true;
        cfg.sim_order_latency_ms = 0;
        cfg.sim_available_depth_fraction = 1.0;
        let bot = TradingBot::new(cfg, pool.clone()).await.unwrap();
        let mut trade = Trade {
            timestamp: Utc::now().to_rfc3339(),
            market_id: "exit-market".into(),
            direction: "BUY_YES".into(),
            entry_price: 0.50,
            amount: shares * 0.50,
            shares,
            status: "OPEN".into(),
            mode: "simulation".into(),
            strategy: "maker".into(),
            token_id: Some("test-token".into()),
            end_date: "2099-01-01T00:00:00Z".into(),
            partial_closed: Some(0),
            ..Trade::default()
        };
        trade.id = db::insert_trade(&pool, &trade).await.unwrap();
        bot.portfolio_state.trades.write().await.push(trade.clone());
        bot.set_balance(20.0).await;
        (bot, trade, path)
    }

    pub(crate) async fn cleanup_exit_fixture(bot: &TradingBot, path: &std::path::Path) {
        bot.db.close().await;
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path.display()));
        let _ = std::fs::remove_file(format!("{}-shm", path.display()));
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn maker_market_exit_uses_bids_and_fees_instead_of_supplied_mark() {
        let _guard = crate::env_util::test_env_lock();
        let base = serve_one_book(serde_json::json!({
            "bids": [{"price":"0.60", "size":"100"}],
            "asks": [{"price":"0.62", "size":"100"}]
        }))
        .await;
        let _env = EnvRestore::set("MACBOT_CLOB_BASE", &base);
        for intent in [CloseIntent::Market, CloseIntent::Manual] {
            let (bot, trade, path) = maker_exit_fixture(2.0).await;
            let closed = bot
                .close_trade_with_intent(trade.id, Some(0.999), intent)
                .await
                .unwrap();
            assert_eq!(
                closed.exit_price,
                Some(0.60),
                "a trigger mark is not a fill or resolution"
            );
            let expected = 0.20 - crate::fusion::estimate_taker_fee(0.60) * 1.20;
            assert!((closed.pnl.unwrap() - expected).abs() < 1e-9);
            let fills = db::load_trade_fills(&bot.db, trade.id, 10).await.unwrap();
            assert_eq!(fills.len(), 1);
            assert!((fills[0].avg_price - 0.60).abs() < 1e-9);
            cleanup_exit_fixture(&bot, &path).await;
        }
    }

    #[tokio::test]
    async fn passive_maker_fill_and_resolution_keep_their_supplied_price_without_taker_fees() {
        for (intent, price) in [
            (CloseIntent::PassiveFill, 0.75),
            (CloseIntent::Resolution, 1.0),
        ] {
            let (bot, trade, path) = maker_exit_fixture(2.0).await;
            let closed = bot
                .close_trade_with_intent(trade.id, Some(price), intent)
                .await
                .unwrap();
            assert_eq!(closed.exit_price, Some(price));
            assert!((closed.pnl.unwrap() - 2.0 * (price - 0.5)).abs() < 1e-9);
            cleanup_exit_fixture(&bot, &path).await;
        }
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn simulated_full_close_refuses_partial_depth_without_mutation() {
        let _guard = crate::env_util::test_env_lock();
        let base = serve_one_book(serde_json::json!({
            "bids": [{"price":"0.60", "size":"2"}],
            "asks": [{"price":"0.62", "size":"100"}]
        }))
        .await;
        let _env = EnvRestore::set("MACBOT_CLOB_BASE", &base);
        for missing_token in [false, true] {
            let (bot, mut trade, path) = maker_exit_fixture(100.0).await;
            if missing_token {
                trade.token_id = None;
                db::update_trade(&bot.db, &trade).await.unwrap();
                *bot.portfolio_state
                    .trades
                    .write()
                    .await
                    .iter_mut()
                    .find(|t| t.id == trade.id)
                    .unwrap() = trade.clone();
                // Exercise token derivation without making any metadata request.
                bot.market_state.up_token_cache.insert(
                    trade.market_id.clone(),
                    (std::time::Instant::now(), "test-token".into()),
                );
            }
            for _ in 0..2 {
                let refused = bot
                    .close_trade_with_intent(trade.id, None, CloseIntent::Manual)
                    .await
                    .expect("existing trade must report still_open, not not-found");
                assert!(refused.is_open());
                assert_eq!(refused.shares, 100.0);
                assert_eq!(refused.token_id, trade.token_id);
                assert_eq!(refused.fok_failed, trade.fok_failed);
                let persisted = db::load_trade_by_id(&bot.db, trade.id)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(persisted.is_open());
                assert_eq!(persisted.token_id, trade.token_id);
                assert_eq!(persisted.shares, 100.0);
                assert_eq!(persisted.amount, trade.amount);
                assert_eq!(persisted.pnl, None);
                assert_eq!(bot.balance().await, 20.0);
                assert!(!bot
                    .trade_lifecycle
                    .closing_in_progress
                    .read()
                    .await
                    .contains(&trade.id));
                assert!(db::load_trade_fills(&bot.db, trade.id, 10)
                    .await
                    .unwrap()
                    .is_empty());
            }
            assert!(bot.partial_close_trade(trade.id, 0.99, 0.5).await.is_none());
            assert_eq!(bot.balance().await, 20.0);
            cleanup_exit_fixture(&bot, &path).await;
        }
    }

    #[test]
    fn simulated_sale_walks_multiple_levels_and_requires_all_requested_shares() {
        let book = serde_json::json!({"bids": [
            {"price":"0.40", "size":"100"},
            {"price":"0.55", "size":"3"},
            {"price":"0.60", "size":"2"}
        ]});
        let (price, intended) = super::simulated_sell_fill(&book, 5.0).unwrap();
        assert!((price - 0.57).abs() < 1e-9);
        assert_eq!(intended, 0.60);
        assert!(
            super::simulated_sell_fill(&book, 6.0).is_none(),
            "depth below floor cannot complete sale"
        );
        assert!(super::simulated_sell_fill(&serde_json::json!({"bids":[]}), 1.0).is_none());
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn maker_partial_market_exit_uses_bids_and_taker_fee() {
        let _guard = crate::env_util::test_env_lock();
        let base = serve_one_book(serde_json::json!({
            "bids": [{"price":"0.60", "size":"100"}],
            "asks": [{"price":"0.62", "size":"100"}]
        }))
        .await;
        let _env = EnvRestore::set("MACBOT_CLOB_BASE", &base);
        for maker in [true, false] {
            let (bot, mut trade, path) = maker_exit_fixture(4.0).await;
            if !maker {
                bot.control_plane.config.write().await.sim_live_execution = true;
                trade.strategy = "conviction".into();
                sqlx::query("UPDATE trades SET strategy='conviction' WHERE id=?")
                    .bind(trade.id)
                    .execute(&bot.db)
                    .await
                    .unwrap();
                *bot.portfolio_state.trades.write().await = vec![trade.clone()];
            }
            let partial = bot.partial_close_trade(trade.id, 0.99, 0.5).await.unwrap();
            let expected = 0.20 - crate::fusion::estimate_taker_fee(0.60) * 1.20;
            assert!((partial["pnl"].as_f64().unwrap() - expected).abs() < 1e-9);
            assert_eq!(partial["remaining_shares"], 2.0);
            let fills = db::load_trade_fills(&bot.db, trade.id, 10).await.unwrap();
            assert_eq!(fills[0].avg_price, 0.60);
            cleanup_exit_fixture(&bot, &path).await;
        }
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn deliberate_partial_sale_of_paper_taker_inventory_uses_the_book_not_the_hint() {
        let _guard = crate::env_util::test_env_lock();
        let base = serve_one_book(serde_json::json!({
            "bids":[{"price":"0.60","size":"100"}],
            "asks":[{"price":"0.62","size":"100"}]
        }))
        .await;
        let _env = EnvRestore::set("MACBOT_CLOB_BASE", &base);
        let (bot, mut trade, path) = maker_exit_fixture(4.0).await;
        trade.strategy = "conviction".into();
        db::update_trade(&bot.db, &trade).await.unwrap();
        sqlx::query("UPDATE trades SET strategy='conviction' WHERE id=?")
            .bind(trade.id)
            .execute(&bot.db)
            .await
            .unwrap();
        *bot.portfolio_state.trades.write().await = vec![trade.clone()];
        bot.control_plane.config.write().await.sim_live_execution = false;
        let outcome = bot
            .partial_close_trade_with_intent(trade.id, 0.99, 0.5, CloseIntent::Manual)
            .await
            .unwrap();
        assert_eq!(outcome["status"], "settled");
        let fills = db::load_trade_fills(&bot.db, trade.id, 10).await.unwrap();
        assert_eq!(fills[0].avg_price, 0.6);
        assert_eq!(fills[0].delta_size, 2.0);
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn fok_retry_preserves_manual_intent_after_failure() {
        use crate::bot::actor::ExecutionCommand;
        let (bot, trade, path) = maker_exit_fixture(2.0).await;
        let bot = std::sync::Arc::new(bot);
        let (tx, mut rx) = tokio::sync::mpsc::channel(2);
        *bot.execution_state.execution_tx.write().await = Some(tx);
        let worker = tokio::spawn(TradingBot::retry_fok_close(
            bot.clone(),
            trade.id,
            Some(0.80),
            CloseIntent::Manual,
            std::time::Duration::ZERO,
        ));
        for attempt in 0..2 {
            let command = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            match command {
                ExecutionCommand::CloseTrade {
                    trade_id,
                    exit_price,
                    intent,
                    resp,
                } => {
                    assert_eq!(trade_id, trade.id);
                    assert_eq!(exit_price, Some(0.80));
                    assert_eq!(intent, CloseIntent::Manual);
                    let mut response = trade.clone();
                    response.status = if attempt == 0 { "OPEN" } else { "CLOSED" }.into();
                    response.fok_failed = Some(i64::from(attempt == 0));
                    resp.send(Some(response)).unwrap();
                }
                _ => panic!("expected close command"),
            }
        }
        worker.await.unwrap();
        assert!(rx.try_recv().is_err(), "stop retries after confirmed close");
        cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn partial_close_does_not_mutate_hot_state_when_persistence_fails() -> anyhow::Result<()>
    {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_partial_fail_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg = fixture();
        cfg.mode = "simulation".to_string();
        let bot = TradingBot::new(cfg, pool.clone()).await?;

        let trade = Trade {
            id: 0,
            timestamp: Utc::now().to_rfc3339(),
            market_id: "mkt-partial-fail".to_string(),
            question: "partial close failure".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.50,
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.10,
            reason: "test".to_string(),
            confidence: 0.8,
            status: "OPEN".to_string(),
            mode: "simulation".to_string(),
            source: "bot".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: Some(0.0),
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: Some(0.8),
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid: 0.02,
            accounting_version: 0,
            signals_json: None,
            calibration_version: Some("test-cal".to_string()),
            run_id: Some("test-run".to_string()),
            strategy: "conviction".to_string(),
        };
        let trade_id = db::insert_trade(&pool, &trade).await?;
        let inserted = db::load_trade_by_id(&pool, trade_id)
            .await?
            .expect("inserted trade");
        bot.portfolio_state
            .trades
            .write()
            .await
            .push(inserted.clone());
        bot.set_balance(7.5).await;

        pool.close().await;

        let res = bot.partial_close_trade(trade_id, 0.80, 0.5).await;
        assert!(res.is_none(), "partial close should fail when DB is closed");

        let hot_trade = bot
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .find(|t| t.id == trade_id)
            .cloned()
            .expect("hot trade");
        assert!((hot_trade.amount - inserted.amount).abs() < 1e-9);
        assert!((hot_trade.shares - inserted.shares).abs() < 1e-9);
        assert_eq!(hot_trade.partial_closed, inserted.partial_closed);
        assert_eq!(hot_trade.pnl, inserted.pnl);
        assert!((bot.balance().await - 7.5).abs() < 1e-9);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }
}
