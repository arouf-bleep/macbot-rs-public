//! Read-only, rotating verification of immutable terminal taker evidence.
//! Network reads run outside the writer; only the actor persists audit stops.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Result};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::task::JoinHandle;

use super::ExecutionCommand;
use crate::bot::state::TradingBot;
use crate::clob::settlement::SettledOrderFill;
use crate::clob::{ClobClient, OrderInfo, VenueTrade};
use crate::db::{self, live::TerminalAuditSnapshot};

const BATCH_SIZE: i64 = 8;
const AUDIT_INTERVAL: Duration = Duration::from_secs(30);
const AUDIT_DEADLINE: Duration = Duration::from_secs(20);

struct AuditOutcome {
    order_id: String,
    evidence: Result<String, &'static str>,
}

#[derive(Default)]
pub(super) struct TakerAuditState {
    last_started: Option<Instant>,
    cursor: Option<String>,
    worker: Option<JoinHandle<Vec<AuditOutcome>>>,
    selected: Vec<String>,
    verified_checks: u64,
    last_failure: Option<&'static str>,
}

impl Drop for TakerAuditState {
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            worker.abort();
        }
    }
}

impl TakerAuditState {
    pub(super) fn snapshot(&self) -> Value {
        json!({"running":self.worker.is_some(),"batch_limit":BATCH_SIZE,
            "interval_seconds":AUDIT_INTERVAL.as_secs(),"deadline_seconds":AUDIT_DEADLINE.as_secs(),
            "last_order_id":self.cursor,"verified_checks":self.verified_checks,
            "last_failure":self.last_failure})
    }

    pub(super) fn request_next_check(&mut self) {
        // An operator's newer verification must not be overwritten by an
        // older in-flight observation when the actor polls its result.
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
        self.selected.clear();
        self.last_started = None;
    }
}

/// Require the complete currently verified receipt set, not just an old SQL
/// sum that happens to equal the order's reported filled quantity.
pub(super) fn verify_terminal_evidence(
    snapshot: &TerminalAuditSnapshot,
    info: &OrderInfo,
    history: &[VenueTrade],
    settlements: &[SettledOrderFill],
) -> Result<String> {
    let order = &snapshot.order;
    ensure!(
        matches!(snapshot.state.as_str(), "settled" | "audit_uncertain"),
        "taker intent is not terminal"
    );
    let canonical = db::live::canonical_terminal_history(order, history)?;
    if let Some(original) = &snapshot.history {
        ensure!(
            *original == canonical,
            "original terminal taker history changed"
        );
    }
    let side = if order.phase == "entry" {
        "BUY"
    } else {
        "SELL"
    };
    ensure!(
        info.id.as_deref() == Some(order.order_id.as_str())
            && info.asset_id.as_deref() == Some(order.token_id.as_str())
            && info.side.as_deref() == Some(side)
            && info
                .original_size
                .is_some_and(|size| size.is_finite() && (size - order.size).abs() <= 1e-6)
            && info.price.is_finite()
            && (info.price - order.price).abs() <= 1e-6
            && info.size_matched.is_finite()
            && info.size_matched >= 0.0
            && info.size_matched <= order.size + 1e-6,
        "terminal taker identity or signed size/price changed"
    );
    ensure!(
        matches!(
            info.status.as_str(),
            "MATCHED" | "FILLED" | "CANCELED" | "CANCELLED" | "EXPIRED"
        ),
        "previously completed taker order is no longer terminal"
    );
    let relevant: Vec<_> = history
        .iter()
        .filter(|m| m.taker_order_id == order.order_id)
        .collect();
    let history_ids: HashSet<_> = relevant.iter().map(|m| m.id.as_str()).collect();
    ensure!(
        info.associate_trades
            .iter()
            .all(|id| history_ids.contains(id.as_str())),
        "terminal order has an associated trade missing from its authenticated history"
    );
    let transactions: HashSet<_> = relevant
        .iter()
        .filter(|m| m.is_confirmed())
        .map(|m| {
            m.transaction_hash
                .as_deref()
                .filter(|tx| !tx.is_empty())
                .map(str::to_ascii_lowercase)
                .ok_or_else(|| anyhow!("confirmed terminal match lacks a transaction"))
        })
        .collect::<Result<_>>()?;
    let recorded: HashMap<_, _> = snapshot
        .matches
        .iter()
        .map(|row| (row.0.as_str(), row))
        .collect();
    ensure!(
        recorded.len() == snapshot.matches.len(),
        "duplicate recorded terminal receipt identity"
    );
    let mut observed = HashSet::new();
    let mut quantity = 0.0;
    for fill in settlements {
        ensure!(
            fill.order_id == order.order_id
                && transactions.contains(&fill.transaction_hash.to_ascii_lowercase())
                && fill.gross_shares.is_finite()
                && fill.gross_shares > 0.0
                && fill.gross_collateral.is_finite()
                && fill.gross_collateral > 0.0
                && fill.fee_collateral.is_finite()
                && fill.fee_collateral >= 0.0
                && fill.fee_shares.is_finite()
                && fill.fee_shares >= 0.0
                && fill.fee_shares < fill.gross_shares,
            "invalid or unrelated terminal receipt"
        );
        let identity = format!(
            "{}:{}",
            fill.transaction_hash.to_ascii_lowercase(),
            fill.log_index
        );
        ensure!(
            observed.insert(identity.clone()),
            "duplicate current terminal receipt"
        );
        let old = recorded
            .get(identity.as_str())
            .ok_or_else(|| anyhow!("new receipt after terminal taker accounting"))?;
        let price = fill.gross_collateral / fill.gross_shares;
        ensure!(
            (old.1 - fill.gross_shares).abs() <= 1e-6
                && (old.2 - price).abs() <= 1e-6
                && (old.3 - fill.fee_collateral).abs() <= 1e-6
                && (old.4 - fill.fee_shares).abs() <= 1e-6,
            "terminal taker receipt changed quantity, price or actual fees"
        );
        quantity += fill.gross_shares;
    }
    ensure!(
        observed.len() == recorded.len() && recorded.keys().all(|id| observed.contains(*id)),
        "previously accounted terminal receipt is no longer verifiable"
    );
    ensure!(
        quantity.is_finite() && (quantity - info.size_matched).abs() <= 1e-6,
        "current finalized receipt quantity disagrees with terminal matched size"
    );
    Ok(canonical)
}

async fn observe_one(
    pool: &SqlitePool,
    clob: &ClobClient,
    order: &db::live::LiveOrder,
    history: &[VenueTrade],
) -> Result<String, &'static str> {
    let snapshot = db::live::terminal_audit_snapshot(pool, &order.order_id)
        .await
        .map_err(|_| "observation_unavailable")?;
    if !order.account.eq_ignore_ascii_case(&clob.account_address()) {
        return Err("observation_unavailable");
    }
    let info = clob
        .get_order(&order.order_id)
        .await
        .map_err(|_| "observation_unavailable")?;
    let verifier = clob
        .settlement_verifier()
        .map_err(|_| "observation_unavailable")?;
    let transactions: HashSet<_> = history
        .iter()
        .filter(|m| m.taker_order_id == order.order_id && m.is_confirmed())
        .map(|m| {
            m.transaction_hash
                .as_deref()
                .filter(|tx| !tx.is_empty())
                .map(str::to_ascii_lowercase)
                .ok_or("evidence_changed")
        })
        .collect::<Result<_, _>>()?;
    let mut fills = Vec::new();
    for transaction in transactions {
        fills.extend(
            verifier
                .verify(&transaction, &order.identity)
                .await
                .map_err(|_| "observation_unavailable")?,
        );
    }
    verify_terminal_evidence(&snapshot, &info, history, &fills).map_err(|_| "evidence_changed")
}

async fn audit_batch(
    pool: SqlitePool,
    clob: Option<Arc<ClobClient>>,
    orders: Vec<db::live::LiveOrder>,
    duration: Duration,
) -> Vec<AuditOutcome> {
    let deadline = tokio::time::Instant::now() + duration;
    let Some(clob) = clob else {
        return orders
            .into_iter()
            .map(|order| AuditOutcome {
                order_id: order.order_id,
                evidence: Err("observation_unavailable"),
            })
            .collect();
    };
    let history = match tokio::time::timeout_at(deadline, clob.list_trades()).await {
        Ok(Ok(history)) => history,
        _ => {
            return orders
                .into_iter()
                .map(|order| AuditOutcome {
                    order_id: order.order_id,
                    evidence: Err("observation_unavailable"),
                })
                .collect()
        }
    };
    let mut outcomes = Vec::with_capacity(orders.len());
    for order in orders {
        let evidence =
            tokio::time::timeout_at(deadline, observe_one(&pool, &clob, &order, &history))
                .await
                .unwrap_or(Err("observation_unavailable"));
        outcomes.push(AuditOutcome {
            order_id: order.order_id,
            evidence,
        });
    }
    outcomes
}

async fn stop_for_uncertainty(bot: &TradingBot, category: &str) {
    bot.set_degraded_no_trade(true);
    let detail = format!("Terminal taker evidence requires signed operator revalidation ({category}); accounted inventory was retained");
    bot.halt_trading("taker_terminal_evidence_audit", detail.clone())
        .await;
    bot.record_reconcile_error("taker_terminal_evidence_audit", detail)
        .await;
}

pub(super) async fn maintain(bot: &TradingBot, state: &mut TakerAuditState) {
    if state
        .worker
        .as_ref()
        .is_some_and(|worker| worker.is_finished())
    {
        let outcomes = match state.worker.take().expect("finished audit worker").await {
            Ok(outcomes) => outcomes,
            Err(_) => state
                .selected
                .iter()
                .map(|id| AuditOutcome {
                    order_id: id.clone(),
                    evidence: Err("observation_unavailable"),
                })
                .collect(),
        };
        state.selected.clear();
        for outcome in outcomes {
            let result = match outcome.evidence {
                Ok(history) => db::live::record_verified_terminal_baseline(
                    &bot.db,
                    &outcome.order_id,
                    &history,
                )
                .await
                .map_err(|_| "observation_unavailable"),
                Err(category) => Err(category),
            };
            match result {
                Ok(()) => state.verified_checks = state.verified_checks.saturating_add(1),
                Err(category) => {
                    state.last_failure = Some(category);
                    if db::live::mark_audit_uncertain(&bot.db, &outcome.order_id, category)
                        .await
                        .is_err()
                    {
                        state.last_failure = Some("audit_stop_persistence_failed");
                    }
                    stop_for_uncertainty(bot, state.last_failure.unwrap_or(category)).await;
                }
            }
        }
    }
    match db::live::audit_uncertain_count(&bot.db).await {
        Ok(count) if count > 0 => {
            // Preserve the visible stop after restart or an attempted generic
            // kill-switch reset. Only explicit baseline revalidation clears it.
            if !bot.kill_switch_active() || !bot.degraded_no_trade_active() {
                stop_for_uncertainty(bot, "persisted_audit_uncertainty").await;
            }
        }
        Err(_) => {
            stop_for_uncertainty(bot, "audit_ledger_unavailable").await;
            return;
        }
        _ => {}
    }
    if state.worker.is_some()
        || state
            .last_started
            .is_some_and(|last| last.elapsed() < AUDIT_INTERVAL)
    {
        return;
    }
    state.last_started = Some(Instant::now());
    let orders = async {
        let mut orders =
            db::live::terminal_orders_batch(&bot.db, state.cursor.as_deref(), BATCH_SIZE).await?;
        if orders.is_empty() && state.cursor.is_some() {
            orders = db::live::terminal_orders_batch(&bot.db, None, BATCH_SIZE).await?;
        }
        Ok::<_, anyhow::Error>(orders)
    }
    .await;
    let orders = match orders {
        Ok(orders) => orders,
        Err(_) => {
            stop_for_uncertainty(bot, "audit_ledger_unavailable").await;
            return;
        }
    };
    if let Some(last) = orders.last() {
        state.cursor = Some(last.order_id.clone());
    } else {
        return;
    }
    state.selected = orders.iter().map(|order| order.order_id.clone()).collect();
    state.worker = Some(tokio::spawn(audit_batch(
        bot.db.clone(),
        bot.clob.clone(),
        orders,
        AUDIT_DEADLINE,
    )));
}

pub(super) async fn recheck(bot: &TradingBot, order_id: &str) -> Result<()> {
    ensure!(
        bot.kill_switch_active(),
        "operator recheck requires an engaged kill switch"
    );
    let snapshot = db::live::terminal_audit_snapshot(&bot.db, order_id).await?;
    ensure!(
        snapshot.state == "audit_uncertain",
        "taker order is not audit-uncertain"
    );
    ensure!(
        snapshot.history.is_some(),
        "original taker terminal baseline is unavailable; manual reconciliation is required"
    );
    let order = snapshot.order;
    let mut outcomes = audit_batch(
        bot.db.clone(),
        bot.clob.clone(),
        vec![order],
        AUDIT_DEADLINE,
    )
    .await;
    let history = outcomes
        .pop()
        .ok_or_else(|| anyhow!("taker audit did not return evidence"))?
        .evidence
        .map_err(|category| anyhow!("taker recheck failed: {category}"))?;
    db::live::restore_verified_terminal_order(&bot.db, order_id, &history).await
}

impl TradingBot {
    pub async fn recheck_terminal_taker_order_serialized(&self, order_id: String) -> Result<()> {
        let sender = self
            .execution_state
            .execution_tx
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("execution actor unavailable for operator recheck"))?;
        let (response, receiver) = tokio::sync::oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            sender.send(ExecutionCommand::RecheckTerminalTakerOrder {
                order_id,
                resp: response,
            }),
        )
        .await
        .map_err(|_| anyhow!("operator recheck send timed out"))?
        .map_err(|_| anyhow!("operator recheck actor stopped"))?;
        tokio::time::timeout(Duration::from_secs(30), receiver)
            .await
            .map_err(|_| anyhow!("operator recheck timed out; inspect persisted state"))?
            .map_err(|_| anyhow!("operator recheck response dropped; inspect persisted state"))?
            .map_err(anyhow::Error::msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::actor::maker_live_tests::{fixture, MockMakerVenue};
    use crate::db::Trade;

    async fn settled_fixture() -> (
        TradingBot,
        Trade,
        std::path::PathBuf,
        MockMakerVenue,
        String,
    ) {
        let (bot, original, path, venue) = fixture().await;
        venue.state.lock().unwrap().drop_post_reply = true;
        assert!(bot.close_confirmed_live_position(&original).await.is_none());
        let order = db::live::pending_orders(&bot.db).await.unwrap().remove(0);
        venue.fill("settled-taker-sale", 5.0, 0.55, "CONFIRMED");
        venue.state.lock().unwrap().status = "MATCHED".into();
        assert!(bot.recover_live_orders_or_halt().await);
        let closed = db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(closed.status, "CLOSED");
        (bot, closed, path, venue, order.order_id)
    }

    async fn finish_check(bot: &TradingBot, state: &mut TakerAuditState) {
        maintain(bot, state).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        while state.worker.is_some() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "offline audit worker stalled"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
            maintain(bot, state).await;
        }
    }

    #[tokio::test]
    async fn recurring_taker_check_accepts_unchanged_finalized_evidence_without_reapplying_cash() {
        let (bot, closed, path, venue, id) = settled_fixture().await;
        let snapshot = db::live::terminal_audit_snapshot(&bot.db, &id)
            .await
            .unwrap();
        assert!(
            snapshot.history.is_some(),
            "baseline must be committed with terminal accounting"
        );
        let cash = bot.balance().await;
        let fills = db::load_trade_fills(&bot.db, closed.id, 100)
            .await
            .unwrap()
            .len();
        let mut state = TakerAuditState::default();
        for _ in 0..2 {
            state.request_next_check();
            finish_check(&bot, &mut state).await;
        }
        assert_eq!(state.verified_checks, 2);
        assert_eq!(bot.balance().await, cash);
        assert_eq!(
            db::load_trade_fills(&bot.db, closed.id, 100)
                .await
                .unwrap()
                .len(),
            fills
        );
        assert_eq!(venue.state.lock().unwrap().posts, 1);
        assert_eq!(db::live::audit_uncertain_count(&bot.db).await.unwrap(), 0);
        assert_eq!(
            db::load_trade_by_id(&bot.db, closed.id)
                .await
                .unwrap()
                .unwrap()
                .pnl,
            closed.pnl
        );
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn late_taker_history_change_is_durable_and_cannot_be_cleared_by_generic_recovery() {
        let (bot, closed, path, venue, id) = settled_fixture().await;
        let baseline = db::live::terminal_audit_snapshot(&bot.db, &id)
            .await
            .unwrap()
            .history;
        let cash = bot.balance().await;
        venue.state.lock().unwrap().matches[0]["fee_rate_bps"] = json!("1001");
        let mut state = TakerAuditState::default();
        finish_check(&bot, &mut state).await;
        assert!(bot.kill_switch_active());
        assert!(bot.degraded_no_trade_active());
        assert_eq!(state.last_failure, Some("evidence_changed"));
        assert_eq!(db::live::audit_uncertain_count(&bot.db).await.unwrap(), 1);
        assert_eq!(
            db::live::terminal_audit_snapshot(&bot.db, &id)
                .await
                .unwrap()
                .history,
            baseline
        );
        assert!(!bot.recover_live_orders_or_halt().await);
        assert!(
            recheck(&bot, &id).await.is_err(),
            "operator cannot waive changed evidence"
        );
        assert_eq!(venue.state.lock().unwrap().posts, 1);
        assert_eq!(bot.balance().await, cash);
        let pool = db::open(path.to_str().unwrap()).await.unwrap();
        assert_eq!(db::live::audit_uncertain_count(&pool).await.unwrap(), 1);
        assert_eq!(
            db::live::pending_orders(&pool).await.unwrap()[0].order_id,
            id
        );
        let persisted = db::load_trade_by_id(&pool, closed.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.pnl, closed.pnl);
        assert_eq!(persisted.shares, closed.shares);
        pool.close().await;
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn changed_terminal_taker_receipt_fee_halts_without_rewriting_the_lot() {
        let (bot, closed, path, venue, id) = settled_fixture().await;
        let cash = bot.balance().await;
        {
            let mut observed = venue.state.lock().unwrap();
            let receipt = observed.receipts.values_mut().next().unwrap();
            let mut data = receipt["logs"][0]["data"].as_str().unwrap().to_string();
            data.replace_range(2 + 4 * 64..2 + 5 * 64, &format!("{:064x}", 1000));
            receipt["logs"][0]["data"] = json!(data);
        }
        let mut state = TakerAuditState::default();
        finish_check(&bot, &mut state).await;
        assert_eq!(state.last_failure, Some("evidence_changed"));
        assert!(bot.kill_switch_active());
        assert!(recheck(&bot, &id).await.is_err());
        assert_eq!(
            db::load_trade_by_id(&bot.db, closed.id)
                .await
                .unwrap()
                .unwrap()
                .pnl,
            closed.pnl
        );
        assert_eq!(bot.balance().await, cash);
        assert_eq!(venue.state.lock().unwrap().posts, 1);
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn unavailable_terminal_receipt_requires_explicit_recheck_and_keeps_risk_stops() {
        let (bot, closed, path, venue, id) = settled_fixture().await;
        let cash = bot.balance().await;
        venue.state.lock().unwrap().nonfinalized = true;
        let mut state = TakerAuditState::default();
        finish_check(&bot, &mut state).await;
        assert_eq!(state.last_failure, Some("observation_unavailable"));
        assert_eq!(db::live::audit_uncertain_count(&bot.db).await.unwrap(), 1);
        venue.state.lock().unwrap().nonfinalized = false;
        assert!(
            !bot.recover_live_orders_or_halt().await,
            "generic reconciliation cannot auto-clear the uncertainty"
        );
        recheck(&bot, &id).await.unwrap();
        assert_eq!(db::live::audit_uncertain_count(&bot.db).await.unwrap(), 0);
        assert!(bot.kill_switch_active());
        assert!(bot.degraded_no_trade_active());
        assert_eq!(bot.balance().await, cash);
        assert_eq!(
            db::load_trade_by_id(&bot.db, closed.id)
                .await
                .unwrap()
                .unwrap()
                .pnl,
            closed.pnl
        );
        assert_eq!(venue.state.lock().unwrap().posts, 1);
        let reason: Option<String> = sqlx::query_scalar("SELECT value FROM meta WHERE key=?")
            .bind(db::live::audit_reason_key(&id))
            .fetch_optional(&bot.db)
            .await
            .unwrap();
        assert!(reason.is_none());
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn a_cached_sql_quantity_cannot_replace_missing_current_receipt_evidence() {
        let (bot, _, path, _venue, id) = settled_fixture().await;
        let snapshot = db::live::terminal_audit_snapshot(&bot.db, &id)
            .await
            .unwrap();
        let clob = bot.clob.as_ref().unwrap();
        let info = clob.get_order(&id).await.unwrap();
        let history = clob.list_trades().await.unwrap();
        assert!(!snapshot.matches.is_empty());
        assert!(verify_terminal_evidence(&snapshot, &info, &history, &[]).is_err());
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn taker_audit_network_wait_is_off_writer_and_has_one_batch_deadline() {
        let (bot, _, path, venue, _) = settled_fixture().await;
        venue.state.lock().unwrap().delay_history_ms = 500;
        let mut state = TakerAuditState::default();
        let started = tokio::time::Instant::now();
        maintain(&bot, &mut state).await;
        assert!(started.elapsed() < Duration::from_millis(100));
        assert!(
            state.worker.is_some(),
            "network verification should continue outside the actor"
        );
        state.request_next_check();
        let orders = db::live::terminal_orders_batch(&bot.db, None, BATCH_SIZE)
            .await
            .unwrap();
        let outcomes = audit_batch(
            bot.db.clone(),
            bot.clob.clone(),
            orders,
            Duration::from_millis(20),
        )
        .await;
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].evidence, Err("observation_unavailable"));
        assert_eq!(venue.state.lock().unwrap().posts, 1);
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }

    #[tokio::test]
    async fn terminal_taker_rotation_uses_permanent_ids_and_skips_uncertain_intents() {
        let (bot, _, path, _venue, id) = settled_fixture().await;
        for number in 0..12 {
            sqlx::query("INSERT INTO live_orders SELECT ?,trade_id,account,token_id,phase,price,size,fee_rate,fee_exponent,taker_only,exchange_address,protocol,'settled',created_at,updated_at FROM live_orders WHERE order_id=?")
                .bind(format!("rotation-{number:02}")).bind(&id).execute(&bot.db).await.unwrap();
        }
        let first = db::live::terminal_orders_batch(&bot.db, None, BATCH_SIZE)
            .await
            .unwrap();
        assert_eq!(first.len(), 8);
        let second = db::live::terminal_orders_batch(
            &bot.db,
            Some(&first.last().unwrap().order_id),
            BATCH_SIZE,
        )
        .await
        .unwrap();
        assert_eq!(second.len(), 5);
        assert!(first
            .iter()
            .all(|a| second.iter().all(|b| a.order_id != b.order_id)));
        assert!(db::live::terminal_orders_batch(
            &bot.db,
            Some(&second.last().unwrap().order_id),
            BATCH_SIZE
        )
        .await
        .unwrap()
        .is_empty());
        db::live::mark_audit_uncertain(&bot.db, &first[0].order_id, "evidence_changed")
            .await
            .unwrap();
        let after = db::live::terminal_orders_batch(&bot.db, None, BATCH_SIZE)
            .await
            .unwrap();
        assert!(after
            .iter()
            .all(|order| order.order_id != first[0].order_id));
        assert!(db::live::terminal_orders_batch(&bot.db, None, 65)
            .await
            .is_err());
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }
}
