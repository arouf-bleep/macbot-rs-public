//! Resolution guard and performance reporting.
//!
//! resolution_guard() — retains live exposure without redemption evidence;
//!                       auto-resolves paper/sim trades via Gamma API.
//! get_performance_report() — comprehensive closed-trade statistics.

use crate::http_client::JsonResponseExt;
use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};
use tracing::info;

use super::analyze::parse_utc;
use super::state::TradingBot;
use crate::db::{self, Trade};
use crate::domain::{BotMode, Direction};
use crate::http_client::HTTP;

const DEFAULT_GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const DEFAULT_CLOB_BASE: &str = "https://clob.polymarket.com";

fn gamma_base() -> String {
    std::env::var("MACBOT_GAMMA_BASE").unwrap_or_else(|_| DEFAULT_GAMMA_BASE.to_string())
}

fn clob_base() -> String {
    std::env::var("MACBOT_CLOB_BASE").unwrap_or_else(|_| DEFAULT_CLOB_BASE.to_string())
}

fn classify_binary_outcome_label(s: &str) -> Option<&'static str> {
    let l = s.trim().to_lowercase();
    if matches!(l.as_str(), "yes" | "up") {
        Some("UP")
    } else if matches!(l.as_str(), "no" | "down") {
        Some("DOWN")
    } else {
        None
    }
}

fn outcome_from_tokens_payload(resp: &Value) -> Option<(&'static str, bool)> {
    let tokens = resp.get("tokens").and_then(|v| v.as_array())?;
    if tokens.is_empty() {
        return None;
    }

    let mut up_price: Option<f64> = None;
    let mut down_price: Option<f64> = None;
    let mut up_winner = false;
    let mut down_winner = false;

    for t in tokens {
        let outcome = t
            .get("outcome")
            .and_then(|v| v.as_str())
            .and_then(classify_binary_outcome_label)?;
        let price = t.get("price").map(parse_guard_price).unwrap_or(0.5);
        let winner = t.get("winner").and_then(|v| v.as_bool()).unwrap_or(false);

        if outcome == "UP" {
            up_price = Some(price);
            up_winner |= winner;
        } else {
            down_price = Some(price);
            down_winner |= winner;
        }
    }

    if up_winner && down_winner {
        return None;
    }
    if up_winner {
        return Some(("UP", true));
    }
    if down_winner {
        return Some(("DOWN", true));
    }

    let up = up_price.unwrap_or(0.5);
    let down = down_price.unwrap_or(0.5);
    if !valid_binary_prices(up, down) {
        return None;
    }
    let settled_strict = up >= 0.999 || down >= 0.999 || up <= 0.001 || down <= 0.001;

    if up >= 0.98 || down <= 0.02 {
        Some(("UP", settled_strict))
    } else if down >= 0.98 || up <= 0.02 {
        Some(("DOWN", settled_strict))
    } else {
        None
    }
}

fn outcome_from_prices_payload(resp: &Value) -> Option<(&'static str, bool)> {
    let out_prices: Vec<Value> = resp
        .get("outcomePrices")
        .and_then(|v| {
            v.as_array()
                .cloned()
                .or_else(|| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
        })
        .unwrap_or_default();
    if out_prices.len() < 2 {
        return None;
    }

    let outcomes = resp
        .get("outcomes")
        .and_then(|v| {
            v.as_array()
                .cloned()
                .or_else(|| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
        })
        .unwrap_or_default();
    let yes_is_idx0 = outcomes
        .first()
        .and_then(|v| v.as_str())
        .map(|s| {
            let l = s.to_lowercase();
            !(l == "no" || l == "down")
        })
        .unwrap_or(true);

    let p0 = parse_guard_price(&out_prices[0]);
    let p1 = parse_guard_price(&out_prices[1]);
    let (yes_p, no_p) = if yes_is_idx0 { (p0, p1) } else { (p1, p0) };
    if !valid_binary_prices(yes_p, no_p) {
        return None;
    }

    let settled_strict = yes_p >= 0.999 || no_p >= 0.999 || yes_p <= 0.001 || no_p <= 0.001;
    if yes_p >= 0.999 || no_p <= 0.001 {
        Some(("UP", settled_strict))
    } else if no_p >= 0.999 || yes_p <= 0.001 {
        Some(("DOWN", settled_strict))
    } else if yes_p >= 0.98 {
        Some(("UP", settled_strict))
    } else if no_p >= 0.98 || yes_p <= 0.02 {
        Some(("DOWN", settled_strict))
    } else {
        None
    }
}

fn valid_binary_prices(yes: f64, no: f64) -> bool {
    yes.is_finite()
        && no.is_finite()
        && (0.0..=1.0).contains(&yes)
        && (0.0..=1.0).contains(&no)
        && (yes + no - 1.0).abs() <= 0.02
}

async fn fetch_market_resolution_payload(market_id: &str) -> Option<Value> {
    let gamma_base = gamma_base();
    let clob_base = clob_base();
    let condition_id = market_id.starts_with("0x") && market_id.len() == 66;
    for url in [
        if condition_id {
            format!("{gamma_base}/markets?condition_ids={market_id}")
        } else {
            format!("{gamma_base}/markets/{market_id}")
        },
        format!("{clob_base}/markets/{market_id}"),
    ] {
        if let Ok(r) = HTTP.get(&url).send().await {
            if r.status().is_success() {
                if let Ok(mut v) = r.bounded_json::<Value>().await {
                    if v.is_array() {
                        let Some(market) = v.as_array().and_then(|markets| {
                            markets.iter().find(|market| {
                                market
                                    .get("conditionId")
                                    .and_then(Value::as_str)
                                    .is_some_and(|id| id.eq_ignore_ascii_case(market_id))
                            })
                        }) else {
                            continue;
                        };
                        v = market.clone();
                    }
                    if settled_binary_outcome_from_payload(&v).is_some() {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn settled_binary_outcome_from_payload(resp: &Value) -> Option<f64> {
    let closed = resp
        .get("closed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let outcome_from_winner: Option<&'static str> = resp
        .get("winner")
        .and_then(|v| v.as_str())
        .filter(|w| !w.is_empty())
        .and_then(classify_binary_outcome_label);

    let (outcome, settled_from_prices) = if let Some(outcome) = outcome_from_winner {
        (outcome, true)
    } else if let Some((outcome, settled)) = outcome_from_tokens_payload(resp) {
        (outcome, settled)
    } else {
        let (outcome, settled) = outcome_from_prices_payload(resp)?;
        (outcome, settled)
    };

    if !closed && !settled_from_prices {
        return None;
    }

    Some(if outcome == "UP" { 1.0 } else { 0.0 })
}

pub(crate) async fn fetch_binary_market_outcome(market_id: &str) -> Option<f64> {
    let resp = fetch_market_resolution_payload(market_id).await?;
    settled_binary_outcome_from_payload(&resp)
}

/// Net-of-fee aggregates for the performance report.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NetSummary {
    pub total_fees: f64,
    pub total_pnl_net: f64,
    /// Positive magnitude of the worst peak-to-trough fall in cumulative net.
    pub max_drawdown_net: f64,
}

/// Gross alone is not enough for a payload whose consumer labels the figure
/// "P&L". The dashboard header reads this report and the trade-analysis report
/// into the same element, and without these fields the two disagreed by the
/// entire fee bill depending on which call landed last.
pub fn net_summary(closed: &[Trade]) -> NetSummary {
    let mut summary = NetSummary::default();
    let mut peak = 0.0_f64;
    let mut cumulative = 0.0_f64;

    for trade in closed {
        let net = trade.net_realized_pnl();
        summary.total_fees += trade.fee_paid;
        cumulative += net;
        peak = peak.max(cumulative);
        summary.max_drawdown_net = summary.max_drawdown_net.max(peak - cumulative);
    }
    summary.total_pnl_net = cumulative;
    summary
}

impl TradingBot {
    // ── Resolution guard ──────────────────────────────────────────────────────

    /// Paper positions can be simulated at a resolved outcome. Funded positions
    /// require sale/redemption receipt evidence before their inventory or cash
    /// can be settled. Gamma's winning outcome alone is not a wallet payout.
    pub async fn resolution_guard(&self) -> Result<()> {
        self.resolve_paper_trades().await;
        let guard_secs = self.control_plane.config.read().await.expiry_guard_seconds as i64;
        let now = Utc::now();
        let unsettled: Vec<Trade> = self
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|trade| trade.is_live() && trade.has_unsettled_exposure())
            .cloned()
            .collect();
        for trade in unsettled {
            let Some(end) = parse_utc(&trade.end_date) else {
                continue;
            };
            let remaining = (end - now).num_seconds();
            if remaining > guard_secs {
                continue;
            }
            // Retain the exact position, existing realized P&L and entry fees.
            // Pending matched quantities are recovered only by the durable
            // receipt accounting path, never from an order's limit price.
            let detail = format!(
                "trade {} market={} has {} shares and reaches expiry in {}s; \
                 sale/redemption transaction evidence is required before booking paid collateral",
                trade.id, trade.market_id, trade.shares, remaining
            );
            self.set_degraded_no_trade(true);
            self.halt_trading("live_redemption_requires_evidence", detail.clone())
                .await;
            self.record_reconcile_error("live_redemption_requires_evidence", detail)
                .await;
        }
        Ok(())
    }

    // ── Paper/sim auto-resolution ─────────────────────────────────────────────

    /// Check all open paper/sim trades whose end_date has passed.
    /// For each, query Gamma to determine if the market resolved UP or DOWN,
    /// then close the trade at 1.0 (win) or 0.0 (loss) accordingly.
    async fn resolve_paper_trades(&self) {
        let now = Utc::now();

        let expired: Vec<Trade> = self
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|t| t.is_open() && !t.is_live() && !t.end_date.is_empty())
            .cloned()
            .collect();

        for trade in expired {
            let end_dt = match parse_utc(&trade.end_date) {
                Some(dt) => dt,
                None => continue,
            };

            if now < end_dt {
                continue; // not expired yet
            }

            // Query Gamma for resolution outcome
            let exit_price = match self
                .fetch_paper_resolution(&trade.market_id, &trade.direction)
                .await
            {
                Some(p) => p,
                None => continue, // market not resolved yet — retry next tick
            };

            info!(
                "Paper resolution: trade {} market={} {} → exit={:.1}",
                trade.id, trade.market_id, trade.direction, exit_price
            );

            if let Ok(Some(closed)) = self
                .close_trade_serialized_with_intent(
                    trade.id,
                    Some(exit_price),
                    super::close::CloseIntent::Resolution,
                )
                .await
            {
                if closed.status_kind() == crate::domain::TradeStatus::Closed {
                    self.set_trade_exit_type(closed.id, "auto_resolved").await;
                    self.ml_on_resolution(&closed).await;
                }
            }
        }

        // Score any pending deep model predictions for markets that resolved
        // but had no open trades (e.g., trade was closed via TP/SL/manual earlier)
        let pending_market_ids: Vec<String> = {
            let ensemble = self.intelligence.ensemble_state.read().await;
            ensemble.pending_predictions.keys().cloned().collect()
        };
        for market_id in pending_market_ids {
            // Check if this market's end_date has passed by querying resolution
            // Use BUY_YES direction to determine if YES won (exit=1.0) or NO won (exit=0.0)
            if let Some(exit_price) = self.fetch_paper_resolution(&market_id, "BUY_YES").await {
                let yes_won = exit_price > 0.5;
                self.score_deep_prediction_for_market(&market_id, yes_won)
                    .await;
            }
        }
    }

    /// Query market resolution data (Gamma first, CLOB fallback) and return
    /// 1.0 (won) or 0.0 (lost) for the given trade direction.
    /// Returns None when resolution is not confidently settled yet.
    async fn fetch_paper_resolution(&self, market_id: &str, direction: &str) -> Option<f64> {
        let outcome_up = fetch_binary_market_outcome(market_id).await?;
        let dir = Direction::parse(direction);
        let won = (outcome_up >= 0.5 && dir == Direction::BuyYes)
            || (outcome_up < 0.5 && dir == Direction::BuyNo);
        Some(if won { 1.0 } else { 0.0 })
    }

    // ── Performance report ────────────────────────────────────────────────────

    /// Build a comprehensive performance JSON snapshot.
    pub async fn get_performance_report(&self) -> Value {
        let cfg = self.control_plane.config.read().await.clone();
        let balance = self.balance().await;
        let closed_rows = match db::load_closed_trades(&self.db).await {
            Ok(rows) => rows,
            Err(_) => return json!({ "error": "failed to load trades" }),
        };

        let closed: Vec<&Trade> = closed_rows.iter().collect();

        let n = closed.len();
        let wins = closed.iter().filter(|t| t.net_realized_pnl() > 0.0).count();
        let losses = n - wins;
        let win_rate = if n > 0 { wins as f64 / n as f64 } else { 0.0 };

        let total_pnl: f64 = closed.iter().filter_map(|t| t.pnl).sum();
        let avg_pnl = if n > 0 { total_pnl / n as f64 } else { 0.0 };

        // Gross alone is not enough for a consumer that labels its figure "P&L".
        // The dashboard reads this payload and the trade-analysis payload into
        // the same header, and the two disagreed by the whole fee bill.
        let NetSummary {
            total_fees,
            total_pnl_net,
            max_drawdown_net,
        } = net_summary(&closed_rows);

        let initial = cfg.initial_balance;
        let roi = if initial > 0.0 {
            (balance - initial) / initial * 100.0
        } else {
            0.0
        };

        // Per-trade dollar P&L has no established annual observation frequency.
        let pnl_series: Vec<f64> = closed.iter().map(|t| t.net_realized_pnl()).collect();
        let sharpe = compute_sharpe(&pnl_series, 1.0);

        // ── Max drawdown ──────────────────────────────────────────────────────
        let max_drawdown_pct = compute_max_drawdown(&pnl_series, initial);

        let (today_pnl, daily_pnl_evidence) = match db::risk::daily_realized_pnl_for_origin(
            &self.db,
            Utc::now().date_naive(),
            db::risk::CapitalOrigin::for_mode(cfg.mode_kind()),
        )
        .await
        {
            Ok(snapshot) => (snapshot.exact_today_pnl, json!(snapshot)),
            Err(error) => (
                None,
                json!({"evidence_complete":false,"error":error.to_string()}),
            ),
        };

        let live_closed: Vec<&&Trade> = closed
            .iter()
            .filter(|t| t.mode_kind() == BotMode::Live)
            .collect();
        let lw = live_closed
            .iter()
            .filter(|t| t.net_realized_pnl() > 0.0)
            .count();
        let live_win_rate = if live_closed.is_empty() {
            0.0
        } else {
            lw as f64 / live_closed.len() as f64
        };

        let brier_50 = compute_brier_score(&closed_rows, 50);
        let brier_200 = compute_brier_score(&closed_rows, 200);

        json!({
            "closed_trades":    n,
            "wins":             wins,
            "losses":           losses,
            "win_rate":         win_rate,
            "total_pnl":        total_pnl,
            "total_entry_fees": total_fees,
            "total_pnl_net":    total_pnl_net,
            "max_drawdown_net": max_drawdown_net,
            "avg_pnl":          avg_pnl,
            "current_balance":  balance,
            "initial_balance":  initial,
            "roi_pct":          roi,
            "sharpe_ratio":     sharpe,
            "sharpe_basis":     "unannualized_per_trade_dollar_pnl",
            "max_drawdown_pct": max_drawdown_pct,
            "today_pnl":        today_pnl,
            "daily_pnl_evidence": daily_pnl_evidence,
            "live_win_rate":    live_win_rate,
            "brier_50":         brier_50,
            "brier_200":        brier_200,
            "open_trades":      self.portfolio_state.trades.read().await.iter().filter(|t| t.has_unsettled_exposure()).count(),
            "edge_tracker":     self.intelligence.edge_tracker.lock().await.to_json(),
        })
    }
}

// ── Stat helpers ──────────────────────────────────────────────────────────────

/// Annualised Sharpe ratio.  `annualise_factor` = √(trades_per_year).
fn compute_sharpe(pnl_series: &[f64], annualise_factor: f64) -> f64 {
    let n = pnl_series.len();
    if n < 2 {
        return 0.0;
    }

    let mean = pnl_series.iter().sum::<f64>() / n as f64;
    let var = pnl_series.iter().map(|&x| (x - mean).powi(2)).sum::<f64>() / (n as f64 - 1.0);
    let std = var.sqrt();

    if !std.is_finite() || std < f64::EPSILON {
        return 0.0;
    }

    (mean / std) * annualise_factor.sqrt()
}

fn compute_brier_score(trades: &[Trade], max_n: usize) -> Option<f64> {
    let mut scored = Vec::new();
    for t in trades.iter().rev().take(max_n) {
        // Early-exit profitability is not the market's binary outcome.
        if !matches!(t.exit_type.as_deref(), Some("auto_resolved" | "resolution")) {
            continue;
        }
        let was_correct = match t.was_correct {
            Some(1) => true,
            Some(0) => false,
            _ => continue,
        };
        let conf = t.raw_confidence.unwrap_or(t.confidence);
        if !conf.is_finite() {
            continue;
        }
        let conf = conf.clamp(0.0, 1.0);
        let outcome_up = matches!(
            (t.direction_kind(), was_correct),
            (Direction::BuyYes, true) | (Direction::BuyNo, false)
        );
        let p_up = if t.direction_kind() == Direction::BuyYes {
            conf
        } else {
            1.0 - conf
        };
        let y = if outcome_up { 1.0 } else { 0.0 };
        scored.push((p_up - y).powi(2));
    }
    if scored.is_empty() {
        return None;
    }
    Some(scored.iter().sum::<f64>() / scored.len() as f64)
}

/// Maximum peak-to-trough drawdown as a percentage of `initial_balance`.
fn compute_max_drawdown(pnl_series: &[f64], initial: f64) -> f64 {
    if initial <= 0.0 || pnl_series.is_empty() {
        return 0.0;
    }

    let mut equity = initial;
    let mut peak = initial;
    let mut max_dd = 0.0_f64;

    for &pnl in pnl_series {
        equity += pnl;
        if equity > peak {
            peak = equity;
        }
        let dd = (peak - equity) / peak * 100.0;
        if dd > max_dd {
            max_dd = dd;
        }
    }

    max_dd
}

fn parse_guard_price(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.5),
        Value::String(s) => s.parse().unwrap_or(0.5),
        _ => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn live_expiry_retains_inventory_and_partial_profit_without_claiming_paid_redemption() {
        let pool = db::open(":memory:").await.unwrap();
        let trade = Trade {
            timestamp: Utc::now().to_rfc3339(),
            market_id: "expired-live-market".into(),
            direction: "BUY_YES".into(),
            status: "OPEN".into(),
            mode: "live".into(),
            strategy: "maker".into(),
            token_id: Some("123".into()),
            end_date: (Utc::now() - chrono::Duration::minutes(5)).to_rfc3339(),
            shares: 3.0,
            amount: 1.2,
            entry_price: 0.4,
            pnl: Some(0.38),
            partial_closed: Some(1),
            fee_paid: 0.03,
            accounting_version: 1,
            ..Trade::default()
        };
        let id = db::insert_trade(&pool, &trade).await.unwrap();
        let mut config: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json")).unwrap();
        config.mode = "simulation".into();
        let bot = TradingBot::new(config, pool).await.unwrap();
        bot.resolution_guard().await.unwrap();
        let persisted = db::load_trade_by_id(&bot.db, id).await.unwrap().unwrap();
        assert!(persisted.is_open());
        assert_eq!(persisted.shares, 3.0);
        assert_eq!(persisted.amount, 1.2);
        assert_eq!(persisted.pnl, Some(0.38));
        assert_eq!(persisted.fee_paid, 0.03);
        assert!(persisted.exit_price.is_none());
        assert!(persisted.exit_timestamp.is_none());
        assert!(db::load_trade_fills(&bot.db, id, 10)
            .await
            .unwrap()
            .is_empty());
        assert!(bot.degraded_no_trade_active());
        assert_eq!(
            bot.kill_switch_status().1.unwrap().source,
            "live_redemption_requires_evidence"
        );
        // A repeat/restart cannot manufacture a zero/one exit or discard the
        // partial sale already realized before expiration.
        bot.resolution_guard().await.unwrap();
        let restarted = TradingBot::new(
            bot.control_plane.config.read().await.clone(),
            bot.db.clone(),
        )
        .await
        .unwrap();
        restarted.resolution_guard().await.unwrap();
        assert_eq!(
            db::load_trade_by_id(&restarted.db, id)
                .await
                .unwrap()
                .unwrap()
                .pnl,
            Some(0.38)
        );
        assert!(restarted.kill_switch_active());
    }

    fn closed_trade(pnl: f64, fee: f64) -> Trade {
        Trade {
            status: "CLOSED".to_string(),
            pnl: Some(pnl),
            fee_paid: fee,
            ..Default::default()
        }
    }

    #[test]
    fn calibration_ignores_early_exit_profitability_labels() {
        let mut trade = closed_trade(1.0, 0.0);
        trade.direction = "BUY_YES".into();
        trade.confidence = 0.9;
        trade.was_correct = Some(1);
        trade.exit_type = Some("manual".into());
        assert_eq!(compute_brier_score(&[trade.clone()], 50), None);
        trade.exit_type = Some("resolution".into());
        assert!((compute_brier_score(&[trade], 50).unwrap() - 0.01).abs() < 1e-9);
    }

    #[test]
    fn net_summary_subtracts_fees_from_both_pnl_and_drawdown() {
        // Up 1.00 net, down to 0.10, back to 0.50: the worst fall is 0.90 even
        // though the run ends in profit, and every leg is charged its fee.
        let summary = net_summary(&[
            closed_trade(1.10, 0.10),
            closed_trade(-0.80, 0.10),
            closed_trade(0.50, 0.10),
        ]);
        assert!((summary.total_fees - 0.30).abs() < 1e-9);
        assert!((summary.total_pnl_net - 0.50).abs() < 1e-9);
        assert!((summary.max_drawdown_net - 0.90).abs() < 1e-9);
    }

    #[test]
    fn net_summary_of_nothing_is_zero_not_a_division_by_zero() {
        assert_eq!(net_summary(&[]), NetSummary::default());
    }

    #[test]
    fn net_summary_never_reports_a_negative_drawdown() {
        // A run that only goes up has no drawdown, and a negative magnitude
        // would render as a gain in a field labelled "max drawdown".
        let summary = net_summary(&[closed_trade(0.5, 0.0), closed_trade(0.5, 0.0)]);
        assert_eq!(summary.max_drawdown_net, 0.0);
    }

    #[test]
    fn classify_binary_outcome_label_maps_yes_no_and_up_down() {
        assert_eq!(classify_binary_outcome_label("YES"), Some("UP"));
        assert_eq!(classify_binary_outcome_label("Up"), Some("UP"));
        assert_eq!(classify_binary_outcome_label("no"), Some("DOWN"));
        assert_eq!(classify_binary_outcome_label("DOWN"), Some("DOWN"));
        assert_eq!(classify_binary_outcome_label("maybe"), None);
        assert_eq!(classify_binary_outcome_label("not up"), None);
    }

    #[test]
    fn reversed_string_outcomes_and_contradictory_resolution_are_handled_honestly() {
        assert_eq!(
            settled_binary_outcome_from_payload(
                &json!({"closed":true,"outcomes":"[\"No\",\"Yes\"]","outcomePrices":"[\"1\",\"0\"]"})
            ),
            Some(0.0)
        );
        for payload in [
            json!({"closed":true,"outcomePrices":[1,1]}),
            json!({"closed":true,"outcomePrices":[0,0]}),
            json!({"closed":true,"tokens":[{"outcome":"Yes","winner":true},{"outcome":"No","winner":true}]}),
        ] {
            assert_eq!(settled_binary_outcome_from_payload(&payload), None);
        }
    }

    #[test]
    fn outcome_from_tokens_payload_prefers_explicit_winner() {
        let v = json!({
            "tokens": [
                { "outcome": "Up", "price": 0.0, "winner": false },
                { "outcome": "Down", "price": 1.0, "winner": true }
            ]
        });
        assert_eq!(outcome_from_tokens_payload(&v), Some(("DOWN", true)));
    }

    #[test]
    fn outcome_from_tokens_payload_uses_price_when_winner_missing() {
        let v = json!({
            "tokens": [
                { "outcome": "Yes", "price": "0.99" },
                { "outcome": "No",  "price": "0.01" }
            ]
        });
        assert_eq!(outcome_from_tokens_payload(&v), Some(("UP", false)));
    }

    #[test]
    fn outcome_from_tokens_payload_returns_none_if_unsettled() {
        let v = json!({
            "tokens": [
                { "outcome": "Up", "price": 0.54 },
                { "outcome": "Down", "price": 0.46 }
            ]
        });
        assert_eq!(outcome_from_tokens_payload(&v), None);
    }

    #[test]
    fn settled_binary_outcome_from_payload_prefers_token_winner() {
        let v = json!({
            "closed": true,
            "tokens": [
                { "outcome": "Yes", "price": 0.0, "winner": false },
                { "outcome": "No", "price": 1.0, "winner": true }
            ]
        });
        assert_eq!(settled_binary_outcome_from_payload(&v), Some(0.0));
    }

    #[test]
    fn settled_binary_outcome_from_payload_accepts_strict_price_even_if_not_closed() {
        let v = json!({
            "closed": false,
            "outcomes": ["Yes", "No"],
            "outcomePrices": ["1.0", "0.0"]
        });
        assert_eq!(settled_binary_outcome_from_payload(&v), Some(1.0));
    }

    #[test]
    fn settled_binary_outcome_from_payload_rejects_soft_price_if_market_open() {
        let v = json!({
            "closed": false,
            "outcomes": ["Yes", "No"],
            "outcomePrices": ["0.98", "0.02"]
        });
        assert_eq!(settled_binary_outcome_from_payload(&v), None);
    }

    #[test]
    fn compute_sharpe_returns_zero_for_flat_pnl_series() {
        let pnl_series = [1.0, 1.0, 1.0, 1.0];
        assert_eq!(compute_sharpe(&pnl_series, 252.0), 0.0);
    }

    #[test]
    fn compute_sharpe_returns_zero_for_non_finite_variance() {
        let pnl_series = [f64::INFINITY, 1.0];
        assert_eq!(compute_sharpe(&pnl_series, 252.0), 0.0);
    }
}
