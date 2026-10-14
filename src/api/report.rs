//! Ledger report — the evidence view.
//!
//! This is the one endpoint meant to be read rather than watched. It answers
//! "can this strategy be evaluated yet, and what happened so far" in a single
//! payload, with the pre-registered sample floor stated alongside the result so
//! a reader cannot mistake a promising partial window for a finding.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::bot::state::TradingBot;
use crate::db::{self, Trade};

/// Sample floor from `ops/shadow-evidence-protocol.md`. Fixed in advance on
/// purpose: a threshold that moves with the results is not a threshold.
pub const REQUIRED_TRADES: usize = 200;
pub const REQUIRED_MARKETS: usize = 60;
pub const REQUIRED_DAYS: i64 = 21;

/// Enough points to show the shape of the curve, few enough to stay a sparkline.
const MAX_CURVE_POINTS: usize = 120;
const MAX_LEDGER_ROWS: usize = 40;
const MAX_REJECTION_ROWS: usize = 8;

// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WindowProgress {
    pub closed_trades: usize,
    pub distinct_markets: usize,
    pub days_elapsed: i64,
    pub required_trades: usize,
    pub required_markets: usize,
    pub required_days: i64,
    pub first_close: Option<String>,
    pub last_close: Option<String>,
    /// True only when every floor is met. Never true on a partial window.
    pub evaluable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Totals {
    pub booked_pnl: f64,
    pub entry_fees: f64,
    pub net_pnl: f64,
    pub wins: usize,
    pub losses: usize,
    /// Percent, computed on net P&L. A trade whose fee exceeds its gain is not
    /// a win in a report headed "after fees".
    pub win_rate: Option<f64>,
    pub mean_net_per_trade: Option<f64>,
    /// Positive magnitude of the worst peak-to-trough fall in cumulative net.
    pub max_drawdown: f64,
    /// Trades with a modeled entry fee or complete receipt-managed cash-fee
    /// accounting. Zero actual fees still establish coverage for version1.
    pub trades_with_recorded_fee: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CurvePoint {
    pub t: String,
    pub cum: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LedgerRow {
    pub closed_at: Option<String>,
    pub market: String,
    pub direction: String,
    pub entry_price: f64,
    pub exit_price: Option<f64>,
    pub pnl: f64,
    pub fee: f64,
    pub net: f64,
    pub strategy: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Ledger {
    pub window: WindowProgress,
    pub totals: Totals,
    pub curve: Vec<CurvePoint>,
    pub rows: Vec<LedgerRow>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Pure computation. Kept free of axum and sqlx so it can be tested directly.
// ─────────────────────────────────────────────────────────────────────────────

fn net_of(trade: &Trade) -> f64 {
    trade.net_realized_pnl()
}

/// Closing time, falling back to the open time when a row predates the
/// `exit_timestamp` column. Ordering a ledger by open time would be wrong, but
/// an unordered row is worse than a slightly misplaced one.
fn close_key(trade: &Trade) -> &str {
    match trade.exit_timestamp.as_deref() {
        Some(ts) if !ts.trim().is_empty() => ts,
        _ => trade.timestamp.as_str(),
    }
}

fn parse_ts(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Inclusive calendar span between the first and last close. One trade is one
/// day, not zero.
fn calendar_days(first: Option<&str>, last: Option<&str>) -> i64 {
    let (Some(first), Some(last)) = (first.and_then(parse_ts), last.and_then(parse_ts)) else {
        return 0;
    };
    (last.date_naive() - first.date_naive()).num_days().max(0) + 1
}

/// Thin the curve to at most `max` points while always keeping the first and
/// last. Dropping the endpoint would misreport where the strategy actually
/// finished, which is the one number a reader takes away.
fn downsample(points: Vec<CurvePoint>, max: usize) -> Vec<CurvePoint> {
    if points.len() <= max || max < 2 {
        return points;
    }
    let last = points.len() - 1;
    let step = last as f64 / (max - 1) as f64;
    let mut out = Vec::with_capacity(max);
    let mut previous = usize::MAX;
    for i in 0..max {
        let idx = if i == max - 1 {
            last
        } else {
            (i as f64 * step).round() as usize
        };
        if idx != previous {
            out.push(points[idx].clone());
            previous = idx;
        }
    }
    out
}

pub fn build_ledger(closed: &[Trade]) -> Ledger {
    let mut sorted: Vec<&Trade> = closed.iter().collect();
    sorted.sort_by(|a, b| close_key(a).cmp(close_key(b)));

    let markets: BTreeSet<&str> = sorted.iter().map(|t| t.market_id.as_str()).collect();
    let first_close = sorted.first().map(|t| close_key(t).to_string());
    let last_close = sorted.last().map(|t| close_key(t).to_string());

    let window = WindowProgress {
        closed_trades: sorted.len(),
        distinct_markets: markets.len(),
        days_elapsed: calendar_days(first_close.as_deref(), last_close.as_deref()),
        required_trades: REQUIRED_TRADES,
        required_markets: REQUIRED_MARKETS,
        required_days: REQUIRED_DAYS,
        first_close: first_close.clone(),
        last_close: last_close.clone(),
        evaluable: false,
    };
    let window = WindowProgress {
        evaluable: window.closed_trades >= REQUIRED_TRADES
            && window.distinct_markets >= REQUIRED_MARKETS
            && window.days_elapsed >= REQUIRED_DAYS,
        ..window
    };

    let mut totals = Totals::default();
    let mut curve = Vec::with_capacity(sorted.len());
    let mut cumulative = 0.0_f64;
    let mut peak = 0.0_f64;

    for trade in &sorted {
        let net = net_of(trade);
        totals.booked_pnl += trade.pnl.unwrap_or(0.0);
        totals.entry_fees += trade.fee_paid;
        if trade.fee_paid > 0.0 || trade.accounting_version == 1 {
            totals.trades_with_recorded_fee += 1;
        }
        if net > 0.0 {
            totals.wins += 1;
        } else if net < 0.0 {
            totals.losses += 1;
        }
        cumulative += net;
        peak = peak.max(cumulative);
        totals.max_drawdown = totals.max_drawdown.max(peak - cumulative);
        curve.push(CurvePoint {
            t: close_key(trade).to_string(),
            cum: cumulative,
        });
    }

    totals.net_pnl = cumulative;
    if !sorted.is_empty() {
        let n = sorted.len() as f64;
        totals.win_rate = Some(totals.wins as f64 / n * 100.0);
        totals.mean_net_per_trade = Some(totals.net_pnl / n);
    }

    let rows: Vec<LedgerRow> = sorted
        .iter()
        .rev()
        .take(MAX_LEDGER_ROWS)
        .map(|t| LedgerRow {
            closed_at: t.exit_timestamp.clone(),
            market: t.question.clone(),
            direction: t.direction.clone(),
            entry_price: t.entry_price,
            exit_price: t.exit_price,
            pnl: t.pnl.unwrap_or(0.0),
            fee: t.fee_paid,
            net: net_of(t),
            strategy: t.strategy.clone(),
        })
        .collect();

    Ledger {
        window,
        totals,
        curve: downsample(curve, MAX_CURVE_POINTS),
        rows,
    }
}

/// Short, stable fingerprint for display. Provenance belongs in the header of
/// the report, not buried in a config tab.
fn short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())[..8].to_string()
}

/// Fingerprint of the deployed ONNX file itself. The loaded model reports only
/// its filename as a version, which cannot detect the swap the evidence
/// protocol lists as a kill criterion. Recomputed per request on purpose:
/// caching it would hide exactly the change it exists to catch.
fn model_file_hash() -> Option<String> {
    let dir = std::env::var("MACBOT_MODELS_DIR").unwrap_or_else(|_| "models".to_string());
    let bytes = std::fs::read(std::path::Path::new(&dir).join("latest.onnx")).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("{:x}", hasher.finalize())[..8].to_string())
}

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/report
// ─────────────────────────────────────────────────────────────────────────────
pub async fn ledger_report(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let closed = match db::load_closed_trades(&bot.db).await {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to load closed trades: {error}") })),
            )
                .into_response();
        }
    };

    let ledger = build_ledger(&closed);

    // A rejection summary that fails to load must not silently read as zero
    // rejections, which would be the most flattering possible error.
    let (rejections, rejection_error) =
        match db::rejection_summary(&bot.db, MAX_REJECTION_ROWS).await {
            Ok(summary) => (Some(summary), None),
            Err(error) => (
                None,
                Some(format!("failed to summarize rejections: {error}")),
            ),
        };

    let (mode, execution_mode, strategy_kind, calibration) = {
        let config = bot.control_plane.config.read().await;
        (
            config.mode_kind().as_str().to_string(),
            config.execution_mode_kind().as_str().to_string(),
            config.strategy.kind.clone(),
            config.calibration_version(),
        )
    };
    let model_loaded = bot.intelligence.deep_model.read().await.is_some();

    Json(json!({
        "generated_at": Utc::now().to_rfc3339(),
        "provenance": {
            "mode": mode,
            "execution_mode": execution_mode,
            "strategy": strategy_kind,
            "calibration_hash": short_hash(&calibration),
            "implementation_id": &crate::replay::implementation_id()[..8],
            "model_hash": model_file_hash(),
            "model_loaded": model_loaded,
        },
        "window": ledger.window,
        "totals": ledger.totals,
        "curve": ledger.curve,
        "rows": ledger.rows,
        "rejections": rejections,
        "rejection_error": rejection_error,
    }))
    .into_response()
}

// ─────────────────────────────────────────────────────────────────────────────
// Decision feed — fills and rejections in one stream.
//
// The bot declines roughly 96% of the markets it looks at. A feed that shows
// only the fills describes a different machine from the one that is running.
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Decision {
    pub at: String,
    pub market: String,
    /// "traded" or "declined". The view colours on this, so it stays a closed set.
    pub kind: String,
    pub call: String,
    pub confidence: Option<f64>,
    /// For a fill: how it ended, or that it is still open. For a rejection: the
    /// full reason text including the threshold that fired.
    pub outcome: String,
    pub pnl: Option<f64>,
    pub strategy: String,
}

fn market_label(question: &str, market_id: &str) -> String {
    let q = question.trim();
    if q.is_empty() {
        // Falling back to a 66-character hex id is useless in a table; the tail
        // is at least enough to tell two rows apart.
        let id = market_id.trim();
        return if id.len() > 10 {
            let tail: String = id
                .chars()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("…{tail}")
        } else {
            id.to_string()
        };
    }
    q.to_string()
}

fn side_label(direction: &str) -> String {
    let d = direction.trim().to_uppercase();
    d.strip_prefix("BUY_").unwrap_or(&d).to_string()
}

pub fn merge_decisions(
    trades: &[Trade],
    rejections: &[db::RejectionRow],
    limit: usize,
) -> Vec<Decision> {
    let mut out: Vec<Decision> = Vec::with_capacity(trades.len() + rejections.len());

    for t in trades {
        let status = t.status_kind();
        let pnl = if status == crate::domain::TradeStatus::Closed {
            t.pnl.map(|_| net_of(t))
        } else {
            None
        };
        let outcome = if status == crate::domain::TradeStatus::Open {
            format!("Open at {:.2}", t.entry_price)
        } else if status == crate::domain::TradeStatus::Closing {
            "Exit unresolved".into()
        } else {
            match pnl {
                Some(p) if p > 0.0 => format!("Won +{:.2}", p),
                Some(p) if p < 0.0 => format!("Lost {:.2}", p),
                Some(_) => "Flat".to_string(),
                None => t
                    .exit_type
                    .clone()
                    .unwrap_or_else(|| t.status.to_lowercase()),
            }
        };
        out.push(Decision {
            // A fill is placed at the moment it was decided, not when it closed,
            // so the feed reads as a decision log rather than a settlement log.
            at: t.timestamp.clone(),
            market: market_label(&t.question, &t.market_id),
            kind: "traded".to_string(),
            call: format!("Buy {}", side_label(&t.direction)),
            confidence: Some(t.confidence),
            outcome,
            pnl,
            strategy: t.strategy.clone(),
        });
    }

    for r in rejections {
        out.push(Decision {
            at: r.timestamp.clone(),
            market: market_label("", &r.market_id),
            kind: "declined".to_string(),
            call: format!("Skip {}", side_label(&r.signal)),
            confidence: Some(r.confidence),
            outcome: r.rejection_reason.clone(),
            pnl: None,
            strategy: r.strategy.clone(),
        });
    }

    out.sort_by(|a, b| b.at.cmp(&a.at));
    out.truncate(limit);
    out
}

const MAX_DECISIONS: usize = 30;

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/decisions
// ─────────────────────────────────────────────────────────────────────────────
pub async fn decisions(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    // Pull more of each than are shown, because the merge discards whichever
    // side is older. Taking exactly MAX_DECISIONS of each would truncate the
    // busier stream and silently bias the feed toward the quieter one.
    let fetch = MAX_DECISIONS * 2;

    let trades = match db::load_recent_trades(&bot.db, fetch).await {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to load recent trades: {error}") })),
            )
                .into_response();
        }
    };
    let rejections = match db::recent_rejections(&bot.db, fetch).await {
        Ok(rows) => rows,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to load recent rejections: {error}") })),
            )
                .into_response();
        }
    };

    Json(json!({
        "generated_at": Utc::now().to_rfc3339(),
        "decisions": merge_decisions(&trades, &rejections, MAX_DECISIONS),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(id: i64, market: &str, ts: &str, pnl: f64, fee: f64) -> Trade {
        Trade {
            id,
            timestamp: ts.to_string(),
            market_id: market.to_string(),
            question: format!("Bitcoin Up or Down {market}"),
            direction: "BUY_YES".to_string(),
            entry_price: 0.55,
            amount: 5.0,
            shares: 9.0,
            status: "CLOSED".to_string(),
            mode: "shadow".to_string(),
            exit_price: Some(1.0),
            exit_timestamp: Some(ts.to_string()),
            pnl: Some(pnl),
            fee_paid: fee,
            strategy: "conviction".to_string(),
            ..Default::default()
        }
    }

    fn rejection(ts: &str, reason: &str) -> db::RejectionRow {
        db::RejectionRow {
            timestamp: ts.to_string(),
            market_id: "0xabcdef0123456789abcdef".to_string(),
            signal: "BUY_NO".to_string(),
            confidence: 0.75,
            net_ev: 0.12,
            rejection_reason: reason.to_string(),
            strategy: "conviction".to_string(),
        }
    }

    #[test]
    fn the_feed_interleaves_fills_and_rejections_by_time() {
        let decisions = merge_decisions(
            &[
                closed(1, "m1", "2026-03-01T10:00:00+00:00", 0.4, 0.0),
                closed(2, "m2", "2026-03-01T12:00:00+00:00", -0.2, 0.0),
            ],
            &[
                rejection("2026-03-01T11:00:00+00:00", "thin book"),
                rejection("2026-03-01T13:00:00+00:00", "too late"),
            ],
            10,
        );
        let order: Vec<&str> = decisions.iter().map(|d| d.kind.as_str()).collect();
        assert_eq!(
            order,
            ["declined", "traded", "declined", "traded"],
            "newest first, both kinds in one stream"
        );
    }

    #[test]
    fn decisions_use_net_profit_and_never_label_unresolved_exits_as_wins() {
        let trade = closed(1, "m1", "2026-03-01T00:00:00Z", 0.05, 0.10);
        let decisions = merge_decisions(std::slice::from_ref(&trade), &[], 10);
        assert_eq!(decisions[0].outcome, "Lost -0.05");
        assert_eq!(decisions[0].pnl, Some(-0.05));
        let mut unresolved = trade;
        unresolved.status = "CLOSING".into();
        let decisions = merge_decisions(&[unresolved], &[], 10);
        assert_eq!(decisions[0].outcome, "Exit unresolved");
        assert_eq!(decisions[0].pnl, None);
        assert_eq!(
            market_label("", "😀😀😀😀😀😀😀😀😀😀😀"),
            "…😀😀😀😀😀😀😀😀"
        );
    }

    #[test]
    fn a_rejection_keeps_the_threshold_that_fired() {
        // The category is for counting. The feed answers "why not this one",
        // which needs the specific number.
        let decisions = merge_decisions(
            &[],
            &[rejection(
                "2026-03-01T11:00:00+00:00",
                "conviction entry blocked: only 105s remaining < 120s minimum",
            )],
            10,
        );
        assert_eq!(
            decisions[0].outcome,
            "conviction entry blocked: only 105s remaining < 120s minimum"
        );
    }

    #[test]
    fn an_open_trade_reads_as_open_not_as_a_flat_result() {
        let mut open = closed(1, "m1", "2026-03-01T10:00:00+00:00", 0.0, 0.0);
        open.status = "OPEN".to_string();
        open.pnl = None;
        open.exit_price = None;
        open.entry_price = 0.58;
        let decisions = merge_decisions(&[open], &[], 10);
        assert_eq!(decisions[0].outcome, "Open at 0.58");
        assert_eq!(decisions[0].pnl, None);
    }

    #[test]
    fn a_decision_without_a_question_falls_back_to_the_market_id_tail() {
        let decisions = merge_decisions(&[], &[rejection("2026-03-01T11:00:00+00:00", "x")], 10);
        assert_eq!(decisions[0].market, "…89abcdef");
    }

    #[test]
    fn the_feed_is_capped_without_dropping_the_newest() {
        let trades: Vec<Trade> = (0..40i64)
            .map(|i| {
                closed(
                    i,
                    "m1",
                    &format!("2026-03-01T10:{:02}:00+00:00", i),
                    0.01,
                    0.0,
                )
            })
            .collect();
        let decisions = merge_decisions(&trades, &[], 10);
        assert_eq!(decisions.len(), 10);
        assert_eq!(decisions[0].at, "2026-03-01T10:39:00+00:00");
    }

    #[test]
    fn a_fill_is_dated_by_its_decision_not_its_settlement() {
        // Ordering fills by exit time would put a long-held position after
        // decisions that were actually made later.
        let mut t = closed(1, "m1", "2026-03-01T10:00:00+00:00", 0.4, 0.0);
        t.timestamp = "2026-03-01T09:00:00+00:00".to_string();
        t.exit_timestamp = Some("2026-03-01T10:00:00+00:00".to_string());
        let decisions = merge_decisions(&[t], &[rejection("2026-03-01T09:30:00+00:00", "x")], 10);
        assert_eq!(decisions[0].kind, "declined");
        assert_eq!(decisions[1].at, "2026-03-01T09:00:00+00:00");
    }

    #[test]
    fn net_is_gross_less_fees_not_gross() {
        let ledger = build_ledger(&[
            closed(1, "m1", "2026-03-01T00:00:00+00:00", 1.00, 0.10),
            closed(2, "m2", "2026-03-02T00:00:00+00:00", 0.50, 0.10),
        ]);
        assert_eq!(ledger.totals.booked_pnl, 1.50);
        assert_eq!(ledger.totals.entry_fees, 0.20);
        assert!((ledger.totals.net_pnl - 1.30).abs() < 1e-9);
    }

    #[test]
    fn a_trade_whose_fee_exceeds_its_gain_counts_as_a_loss() {
        // The headline figure is "net after fees", so the win count must agree
        // with it. Counting this as a win is how a losing strategy shows a
        // winning win rate.
        let ledger = build_ledger(&[closed(1, "m1", "2026-03-01T00:00:00+00:00", 0.05, 0.10)]);
        assert_eq!(ledger.totals.wins, 0);
        assert_eq!(ledger.totals.losses, 1);
        assert_eq!(ledger.totals.win_rate, Some(0.0));
    }

    #[test]
    fn fee_coverage_is_reported_so_a_zero_fee_sample_cannot_pass_as_net() {
        // The retained shadow sample is mostly maker fills, which record no
        // local fee. A reader must be able to see that "after fees" covers only
        // part of the window.
        let mut maker = closed(1, "m1", "2026-03-01T00:00:00+00:00", 0.50, 0.0);
        maker.strategy = "maker".to_string();
        let ledger = build_ledger(&[
            maker,
            closed(2, "m2", "2026-03-02T00:00:00+00:00", 0.50, 0.02),
        ]);
        assert_eq!(ledger.window.closed_trades, 2);
        assert_eq!(
            ledger.totals.trades_with_recorded_fee, 1,
            "only the fee-bearing trade counts toward coverage"
        );
    }

    #[test]
    fn a_partial_window_is_never_evaluable() {
        let trades: Vec<Trade> = (0..REQUIRED_TRADES as i64)
            .map(|i| {
                closed(
                    i,
                    &format!("m{i}"),
                    &format!("2026-03-{:02}T00:00:00+00:00", (i % 5) + 1),
                    0.01,
                    0.0,
                )
            })
            .collect();
        let ledger = build_ledger(&trades);
        assert_eq!(ledger.window.closed_trades, REQUIRED_TRADES);
        assert!(ledger.window.distinct_markets >= REQUIRED_MARKETS);
        assert_eq!(ledger.window.days_elapsed, 5, "five distinct calendar days");
        assert!(
            !ledger.window.evaluable,
            "trade and market floors met but the 21-day floor is not"
        );
    }

    #[test]
    fn every_floor_met_makes_the_window_evaluable() {
        let trades: Vec<Trade> = (0..REQUIRED_TRADES as i64)
            .map(|i| {
                let day = (i % 25) + 1;
                closed(
                    i,
                    &format!("m{i}"),
                    &format!("2026-03-{day:02}T00:00:00+00:00"),
                    0.01,
                    0.0,
                )
            })
            .collect();
        let ledger = build_ledger(&trades);
        assert_eq!(ledger.window.days_elapsed, 25);
        assert!(ledger.window.evaluable);
    }

    #[test]
    fn one_trade_spans_one_calendar_day_not_zero() {
        let ledger = build_ledger(&[closed(1, "m1", "2026-03-01T12:00:00+00:00", 0.1, 0.0)]);
        assert_eq!(ledger.window.days_elapsed, 1);
    }

    #[test]
    fn an_empty_window_reports_zero_rather_than_dividing_by_it() {
        let ledger = build_ledger(&[]);
        assert_eq!(ledger.window.closed_trades, 0);
        assert_eq!(ledger.window.days_elapsed, 0);
        assert_eq!(ledger.totals.win_rate, None);
        assert_eq!(ledger.totals.mean_net_per_trade, None);
        assert!(ledger.curve.is_empty());
        assert!(!ledger.window.evaluable);
    }

    #[test]
    fn drawdown_measures_peak_to_trough_not_final_loss() {
        // Up 1.00, down to 0.20, back to 0.60: the worst fall is 0.80 even
        // though the window ends in profit.
        let ledger = build_ledger(&[
            closed(1, "m1", "2026-03-01T00:00:00+00:00", 1.00, 0.0),
            closed(2, "m2", "2026-03-02T00:00:00+00:00", -0.80, 0.0),
            closed(3, "m3", "2026-03-03T00:00:00+00:00", 0.40, 0.0),
        ]);
        assert!((ledger.totals.max_drawdown - 0.80).abs() < 1e-9);
        assert!((ledger.totals.net_pnl - 0.60).abs() < 1e-9);
    }

    #[test]
    fn the_curve_is_ordered_by_close_time_not_insertion_order() {
        let ledger = build_ledger(&[
            closed(1, "m1", "2026-03-03T00:00:00+00:00", 1.00, 0.0),
            closed(2, "m2", "2026-03-01T00:00:00+00:00", -0.50, 0.0),
        ]);
        assert_eq!(ledger.curve[0].t, "2026-03-01T00:00:00+00:00");
        assert!((ledger.curve[0].cum + 0.50).abs() < 1e-9);
        assert!((ledger.curve[1].cum - 0.50).abs() < 1e-9);
    }

    #[test]
    fn downsampling_keeps_the_endpoint() {
        let trades: Vec<Trade> = (0..500i64)
            .map(|i| {
                closed(
                    i,
                    "m1",
                    &format!("2026-03-01T00:00:{:02}+00:00", i % 60),
                    0.01,
                    0.0,
                )
            })
            .collect();
        let ledger = build_ledger(&trades);
        assert!(ledger.curve.len() <= MAX_CURVE_POINTS);
        let final_cum = ledger.curve.last().expect("curve is not empty").cum;
        assert!(
            (final_cum - ledger.totals.net_pnl).abs() < 1e-9,
            "the last plotted point must equal the reported net"
        );
    }

    #[test]
    fn rows_are_newest_first_and_capped() {
        let trades: Vec<Trade> = (0..60i64)
            .map(|i| {
                closed(
                    i,
                    "m1",
                    &format!("2026-03-01T00:{:02}:00+00:00", i),
                    0.01,
                    0.0,
                )
            })
            .collect();
        let ledger = build_ledger(&trades);
        assert_eq!(ledger.rows.len(), MAX_LEDGER_ROWS);
        assert_eq!(
            ledger.rows[0].closed_at.as_deref(),
            Some("2026-03-01T00:59:00+00:00")
        );
    }

    #[test]
    fn a_row_without_an_exit_timestamp_still_sorts_by_its_open_time() {
        let mut orphan = closed(1, "m1", "2026-03-02T00:00:00+00:00", 0.10, 0.0);
        orphan.exit_timestamp = None;
        let ledger = build_ledger(&[
            orphan,
            closed(2, "m2", "2026-03-05T00:00:00+00:00", 0.20, 0.0),
        ]);
        assert_eq!(ledger.curve[0].t, "2026-03-02T00:00:00+00:00");
        assert_eq!(ledger.window.days_elapsed, 4);
    }
}
