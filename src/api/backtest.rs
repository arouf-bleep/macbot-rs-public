//! Synthetic TA experiments and captured shared-decision replay.
//! Neither mode reconstructs historical venue execution or establishes live profitability.

mod candles;
mod coverage;
mod exit;
mod metrics;
mod policy;
mod replay;
mod request;
#[cfg(test)]
mod tests;

use crate::{
    bot::state::TradingBot,
    domain::Signal,
    fusion::estimate_taker_fee,
    signals::{
        compute_ha, compute_macd, compute_regime, compute_rsi, compute_vwap,
        lmsr::estimate_lmsr_slippage, Candle,
    },
};
use axum::{
    extract::{rejection::JsonRejection, State},
    Json,
};
use candles::*;
use chrono::{Datelike, TimeZone, Timelike, Utc};
use coverage::*;
use exit::*;
use metrics::*;
use policy::*;
use request::BacktestRequest;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Instant,
};
use tracing::{info, warn};

const CANDLES_PER_DAY: usize = 96;
const MAX_BACKTEST_DAYS: u32 = 180;
const KLINE_15M_MS: i64 = 15 * 60 * 1000;
const RESULT_CONTRACT_VERSION: u32 = 2;

#[derive(Default, Clone, Copy)]
struct Bucket {
    trades: usize,
    wins: usize,
    pnl: f64,
    cost: f64,
    stake: f64,
}

impl Bucket {
    fn push(&mut self, won: bool, pnl: f64, cost: f64, stake: f64) {
        self.trades += 1;
        if won {
            self.wins += 1;
        }
        self.pnl += pnl;
        self.cost += cost;
        self.stake += stake;
    }
    fn as_json(self, resolution_model: bool) -> Value {
        let win_rate = sample_average(self.wins as f64 * 100.0, self.trades);
        let avg_cost_pct =
            (self.trades > 0 && self.stake > 0.0).then(|| self.cost / self.stake * 100.0);
        let break_even_win_rate_pct = avg_cost_pct
            .filter(|_| resolution_model)
            .map(|cost| 50.0 + cost / 2.0);
        let edge_gap_pp = win_rate
            .zip(break_even_win_rate_pct)
            .map(|(win, rate)| win - rate);
        let expectancy_per_trade = sample_average(self.pnl, self.trades);
        json!({
            "trades": self.trades,
            "wins": self.wins,
            "win_rate": win_rate,
            "total_pnl": self.pnl,
            "avg_cost_pct": avg_cost_pct,
            "break_even_win_rate_pct": break_even_win_rate_pct,
            "edge_gap_pp": edge_gap_pp,
            "expectancy_per_trade": expectancy_per_trade,
        })
    }
}

// ── Handler ───────────────────────────────────────────────────────────────────

pub async fn run_backtest(
    State(bot): State<Arc<TradingBot>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Json<Value> {
    let value = match body {
        Ok(body) => body.0,
        Err(err) => return request_error(format!("invalid backtest request: {}", err.body_text())),
    };
    match value.get("mode") {
        None | Some(Value::String(_)) => {}
        _ => return request_error("mode must be synthetic_ta or decision_replay"),
    };
    match value
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("synthetic_ta")
    {
        "decision_replay" => replay::run(value).await,
        "synthetic_ta" => {
            let req = match serde_json::from_value::<BacktestRequest>(value) {
                Ok(req) => req,
                Err(error) => {
                    return request_error(format!("invalid synthetic backtest request: {error}"))
                }
            };
            if let Err(error) = req.validate() {
                return request_error(error);
            }
            run_synthetic_backtest(bot, req).await
        }
        _ => request_error("mode must be synthetic_ta or decision_replay"),
    }
}

fn request_error(error: impl Into<String>) -> Json<Value> {
    Json(json!({"error": error.into(), "result_contract_version": RESULT_CONTRACT_VERSION}))
}

async fn run_synthetic_backtest(bot: Arc<TradingBot>, req: BacktestRequest) -> Json<Value> {
    let entry_price_yes = req.entry_price_yes;
    let entry_price_no = req.entry_price_no;
    let symmetric_prices = entry_price_yes == 0.5 && entry_price_no == 0.5;
    let days = req.days.clamp(1, MAX_BACKTEST_DAYS);
    let init = req.initial_balance.max(1.0);
    let gas_usd = req.gas_usd.max(0.0);
    let liquidity_proxy_usd = req.liquidity_proxy_usd.max(100.0);
    let max_trade_pct = req.max_trade_pct.clamp(0.01, 0.25);
    let warmup = req.window.clamp(40, 120);
    let entry_threshold = req.entry_threshold.clamp(0.15, 0.50);
    let min_trade_usd = req.min_trade_usd.max(0.01);
    let edge_safety_buffer = req.edge_safety_buffer.max(0.0);
    let bull_entry_threshold = req.bull_entry_threshold.map(|v| v.clamp(0.15, 0.50));
    let bear_entry_threshold = req.bear_entry_threshold.map(|v| v.clamp(0.15, 0.50));
    let chop_entry_threshold = req.chop_entry_threshold.map(|v| v.clamp(0.15, 0.50));
    let buy_yes_threshold = req.buy_yes_threshold.map(|v| v.clamp(0.15, 0.80));
    let buy_no_threshold = req.buy_no_threshold.map(|v| v.clamp(0.15, 0.80));
    let allow_buy_yes = req.allow_buy_yes;
    let allow_buy_no = req.allow_buy_no;
    let regime_allowlist: HashSet<String> = req
        .regime_allowlist
        .iter()
        .map(|s| normalize_regime_key(s))
        .filter(|s| !s.is_empty())
        .collect();
    let session_allowlist: HashSet<String> = req
        .session_allowlist
        .iter()
        .map(|s| normalize_session_key(s))
        .filter(|s| matches!(s.as_str(), "asia" | "europe" | "us"))
        .collect();
    let blocked_hours_utc: HashSet<u32> = req
        .blocked_hours_utc
        .iter()
        .copied()
        .filter(|hour| *hour < 24)
        .collect();
    let mut regime_side_modes: HashMap<String, SideMode> = HashMap::new();
    for (regime, mode_raw) in &req.regime_side_modes {
        let regime_key = normalize_regime_key(regime);
        if regime_key.is_empty() {
            continue;
        }
        let Some(mode) = parse_side_mode(mode_raw) else {
            return Json(json!({
                "error": format!(
                    "invalid regime_side_modes value for {regime}: {mode_raw} (expected both|yes_only|no_only|none)"
                )
            }));
        };
        regime_side_modes.insert(regime_key, mode);
    }
    let require_regime_alignment = req.require_regime_alignment;
    let anti_bias_strength = req.anti_bias_strength.clamp(0.0, 0.50);
    let high_vol_entry_threshold = req.high_vol_entry_threshold.map(|v| v.clamp(0.15, 0.50));
    let high_volatility_threshold = req.high_volatility_threshold.max(0.0);
    let min_confidence_margin = req.min_confidence_margin.clamp(0.0, 0.50);
    let time_of_day_weight = req.time_of_day_weight;
    let adaptive_stand_down = req.adaptive_stand_down;
    let stand_down_window = req.stand_down_window.clamp(5, 200);
    let stand_down_margin_pp = req.stand_down_margin_pp.clamp(-5.0, 20.0);
    let stand_down_cooldown_candles = req.stand_down_cooldown_candles.clamp(1, 384);
    let execution_mode_raw = req
        .execution_mode
        .as_deref()
        .unwrap_or("taker")
        .trim()
        .to_ascii_lowercase();
    let is_maker_cost_mode = execution_mode_raw == "maker";
    let execution_cost_mode = if is_maker_cost_mode { "maker" } else { "taker" };
    let maker_fee_pct = req.maker_fee_pct.unwrap_or(0.0).clamp(-0.02, 0.02);
    let maker_slippage_factor = req.maker_slippage_factor.unwrap_or(0.20).clamp(0.0, 1.0);
    let maker_gas_usd = req.maker_gas_usd.unwrap_or(0.0).max(0.0);
    let exit_mode = match parse_exit_mode(req.exit_mode.as_deref().unwrap_or("next_candle")) {
        Some(mode) => mode,
        None => {
            return Json(json!({
                "error": format!(
                    "invalid exit_mode '{}' (expected next_candle|taker_tp_sl)",
                    req.exit_mode.as_deref().unwrap_or("")
                )
            }));
        }
    };
    let take_profit_pct = req.take_profit_pct.unwrap_or(0.12).clamp(0.0, 1.0);
    let stop_loss_pct = req.stop_loss_pct.unwrap_or(0.08).clamp(0.0, 1.0);
    if exit_mode == ExitMode::TakerTpSl && is_maker_cost_mode {
        return Json(json!({
            "error": "exit_mode=taker_tp_sl requires execution_mode=taker"
        }));
    }
    if adaptive_stand_down && exit_mode == ExitMode::TakerTpSl {
        return Json(
            json!({"error":"adaptive_stand_down requires resolution exits; TP/SL win-rate break-even is not modeled"}),
        );
    }
    if !matches!(execution_mode_raw.as_str(), "maker" | "taker") {
        return Json(json!({"error":"execution_mode must be maker or taker"}));
    }
    let end_time_ms = req
        .end_time_ms
        .unwrap_or_else(|| Utc::now().timestamp_millis());

    // Read config for trade sizing
    let cfg = bot.control_plane.config.read().await.clone();

    // ── Fetch historical 15m candles (paginated) ─────────────────────────────
    let requested_candles = days as usize * CANDLES_PER_DAY + warmup + 2;
    let started_at = Instant::now();
    info!(
        "Backtest start: days={} warmup={} candles={} end_time_ms={} execution_mode={}",
        days, warmup, requested_candles, end_time_ms, execution_cost_mode
    );
    let parsed = match fetch_historical_klines_15m(requested_candles, end_time_ms).await {
        Ok(v) => v,
        Err(e) => {
            warn!("Backtest: {e}");
            return Json(json!({"error": e}));
        }
    };
    info!(
        "Backtest fetched {} candles in {:.2?}",
        parsed.len(),
        started_at.elapsed()
    );

    if parsed.len() < warmup + 2 {
        return Json(json!({
            "error": format!(
                "Not enough candle data from Binance (got {}, need at least {})",
                parsed.len(),
                warmup + 2
            )
        }));
    }

    let data_start_ms = parsed.first().map(|(ts, _)| *ts);
    let data_end_ms = parsed.last().map(|(ts, _)| *ts);
    let candle_times_ms: Vec<i64> = parsed.iter().map(|(ts, _)| *ts).collect();
    let candles: Vec<Candle> = parsed.into_iter().map(|(_, c)| c).collect();
    let coverage = candle_coverage(&candle_times_ms, requested_candles, end_time_ms, warmup);
    let analyzed_candles = coverage.eligible_contiguous_windows;
    let effective_days_covered = analyzed_candles as f64 / CANDLES_PER_DAY as f64;
    let requested_days = req.days;
    let data_start_utc = data_start_ms
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .map(|dt| dt.to_rfc3339());
    let data_end_utc = data_end_ms
        .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
        .map(|dt| dt.to_rfc3339());
    let data_warning = if !coverage.requested_window_complete {
        Some(format!(
            "Incomplete candle coverage: fetched {} of {} completed candles; {} missing intervals in {} gaps; requested end horizon reached={}; {} candidate windows rejected",
            candles.len(),
            requested_candles,
            coverage.missing_intervals,
            coverage.gap_count,
            coverage.reaches_requested_end_horizon,
            coverage.rejected_noncontiguous_windows,
        ))
    } else {
        None
    };

    // ── Simulate trades ───────────────────────────────────────────────────────
    // We need a sufficiently deep lookback before trading starts.
    let min_px = cfg.min_entry_price;
    let max_px = cfg.max_entry_price;
    let effective_min_stake = cfg.min_stake.min((init * 0.005).max(0.05));
    let min_economic_stake = effective_min_stake.max(min_trade_usd);

    let mut balance = init;
    let mut trades_out: Vec<Value> = Vec::new();
    let mut pnl_series: Vec<Value> = Vec::new();
    let mut wins = 0usize;
    let mut total = 0usize;
    let mut cumulative_pnl = 0.0f64;
    let mut recent_dirs: VecDeque<i8> = VecDeque::with_capacity(24);
    let mut recent_health: VecDeque<bool> = VecDeque::with_capacity(stand_down_window + 1);
    let mut stand_down_cooldown_left = 0usize;
    let mut skipped: HashMap<String, usize> = HashMap::new();
    let mut by_signal: HashMap<String, Bucket> = HashMap::new();
    let mut by_regime: HashMap<String, Bucket> = HashMap::new();
    let mut sum_stake = 0.0f64;
    let mut sum_fee_frac = 0.0f64;
    let mut sum_gas_frac = 0.0f64;
    let mut sum_total_cost_frac = 0.0f64;

    let inc_skip = |m: &mut HashMap<String, usize>, key: &str| {
        *m.entry(key.to_string()).or_insert(0) += 1;
    };

    // Every feature window needs the following completed candle as its outcome.
    let n = candles.len().saturating_sub(1);

    for i in warmup..n {
        if i % 96 == 0 {
            tokio::task::yield_now().await;
        }
        // Validate all feature intervals and the outcome before stateful policy
        // counters, indicators or simulated trades consume this observation.
        let Some((entry_time_ms, resolution_time_ms)) =
            feature_outcome_clock(&candle_times_ms, i, warmup)
        else {
            inc_skip(&mut skipped, "missing_feature_or_outcome_candle");
            continue;
        };
        if adaptive_stand_down && stand_down_cooldown_left > 0 {
            stand_down_cooldown_left -= 1;
            inc_skip(&mut skipped, "stand_down_cooldown");
            continue;
        }

        // Trailing window for TA
        let window = &candles[i.saturating_sub(warmup - 1)..=i];
        let next = &candles[i + 1]; // outcome candle

        // Compute signals
        let vwap = compute_vwap(window);
        let rsi = compute_rsi(window, 14);
        let macd = compute_macd(window, 12, 26, 9);
        let ha = compute_ha(window);
        let regime = compute_regime(window, vwap.slope);
        let entry_dt = Utc.timestamp_millis_opt(entry_time_ms).single();
        let entry_hour_utc = entry_dt.as_ref().map(|dt| dt.hour()).unwrap_or(0);
        let entry_weekday_utc = entry_dt
            .as_ref()
            .map(|dt| dt.weekday().to_string())
            .unwrap_or_else(|| String::from("Unknown"));
        let entry_session = session_bucket(entry_hour_utc).to_string();
        let tod_weights = if time_of_day_weight {
            backtest_time_of_day_weights(entry_hour_utc)
        } else {
            TimeOfDayScoreWeights {
                technical: 1.0,
                regime: 1.0,
            }
        };

        // ── Signal generation (simplified TA fusion) ──────────────────────────
        let mut technical_score = 0.0f64;

        // RSI signal
        if rsi.rsi < 30.0 {
            technical_score += 0.18;
        }
        // oversold → UP
        else if rsi.rsi > 70.0 {
            technical_score -= 0.18;
        } // overbought → DOWN

        // MACD signal
        if macd.hist > 0.0 {
            technical_score += 0.12;
        } else if macd.hist < 0.0 {
            technical_score -= 0.12;
        }
        if macd.hist_delta > 0.0 {
            technical_score += 0.05;
        } else if macd.hist_delta < 0.0 {
            technical_score -= 0.05;
        }

        // HA signal
        match ha.consecutive {
            n if n >= 2 => technical_score += 0.12,
            n if n <= -2 => technical_score -= 0.12,
            _ => {}
        }

        // VWAP signal
        if candles[i].close > vwap.vwap * 1.0002 {
            technical_score += 0.08;
        } else if candles[i].close < vwap.vwap * 0.9998 {
            technical_score -= 0.08;
        }

        // Short-horizon momentum from the trailing window.
        let momentum_pct = if let Some(first) = window.first() {
            if first.close > 0.0 {
                (candles[i].close / first.close - 1.0) * 100.0
            } else {
                0.0
            }
        } else {
            0.0
        };
        if momentum_pct > 0.35 {
            technical_score += 0.10;
        } else if momentum_pct < -0.35 {
            technical_score -= 0.10;
        }

        let mut score = technical_score * tod_weights.technical
            + weighted_regime_score(regime.as_str(), tod_weights.regime);

        // Anti-bias correction: penalise piling into the same side repeatedly.
        if !recent_dirs.is_empty() {
            let skew =
                recent_dirs.iter().map(|d| *d as f64).sum::<f64>() / recent_dirs.len() as f64;
            let effective_anti_bias =
                effective_anti_bias_strength(allow_buy_yes, allow_buy_no, anti_bias_strength);
            score -= skew * effective_anti_bias;
        }

        let mut entry_threshold_used = match regime.as_str() {
            "BULL" | "TREND_UP" => bull_entry_threshold.unwrap_or(entry_threshold),
            "BEAR" | "TREND_DOWN" => bear_entry_threshold.unwrap_or(entry_threshold),
            "CHOP" => chop_entry_threshold.unwrap_or((entry_threshold + 0.03).min(0.50)),
            _ => entry_threshold,
        };
        let rolling_vol = window_volatility(window);
        if rolling_vol >= high_volatility_threshold {
            entry_threshold_used =
                high_vol_entry_threshold.unwrap_or((entry_threshold_used + 0.02).min(0.50));
        }
        let buy_yes_threshold_used = buy_yes_threshold.unwrap_or(entry_threshold_used);
        let buy_no_threshold_used = buy_no_threshold.unwrap_or(entry_threshold_used);
        let raw_signal = classify_raw_signal(score, buy_yes_threshold_used, buy_no_threshold_used);
        let side_threshold_used = if raw_signal == Signal::BuyNo {
            buy_no_threshold_used
        } else {
            buy_yes_threshold_used
        };
        let confidence_margin = (score.abs() - side_threshold_used).max(0.0);

        if raw_signal != Signal::Hold {
            recent_dirs.push_back(if raw_signal == Signal::BuyYes { 1 } else { -1 });
            if recent_dirs.len() > 24 {
                recent_dirs.pop_front();
            }
        }

        let (signal, skip_reason) = apply_side_and_regime_filters(
            raw_signal,
            allow_buy_yes,
            allow_buy_no,
            &regime_allowlist,
            &regime_side_modes,
            require_regime_alignment,
            regime.as_str(),
        );
        if let Some(reason) = skip_reason {
            inc_skip(&mut skipped, reason);
        }

        if signal == Signal::Hold {
            inc_skip(&mut skipped, "hold");
            continue;
        }
        if !session_allowlist.is_empty() && !session_allowlist.contains(entry_session.as_str()) {
            inc_skip(&mut skipped, "session_allowlist");
            continue;
        }
        if blocked_hours_utc.contains(&entry_hour_utc) {
            inc_skip(&mut skipped, "blocked_hour_utc");
            continue;
        }
        if confidence_margin < min_confidence_margin {
            inc_skip(&mut skipped, "confidence_margin");
            continue;
        }

        // Entry price filter
        let entry_price = if signal == Signal::BuyNo {
            entry_price_no
        } else {
            entry_price_yes
        };
        if entry_price < min_px || entry_price > max_px {
            inc_skip(&mut skipped, "entry_price_filter");
            continue;
        }

        // Stake (use config stake_pct or trade_amount) with hard risk cap.
        let desired = if cfg.stake_pct > 0.0 {
            balance * cfg.stake_pct
        } else {
            cfg.trade_amount
        };
        let trade_amt = desired
            .clamp(effective_min_stake, cfg.max_stake)
            .min(balance * max_trade_pct)
            .min(balance);

        if balance < trade_amt || trade_amt < effective_min_stake || trade_amt < 0.01 {
            inc_skip(&mut skipped, "balance_or_min_stake");
            continue;
        }
        if trade_amt < min_economic_stake {
            inc_skip(&mut skipped, "min_trade_usd");
            continue;
        }

        // Both the hypothetical return and costs use dollars per stake dollar.
        // The score-to-probability mapping is a heuristic, not a calibrated forecast.
        let taker_fee_pct = estimate_taker_fee(entry_price);
        let base_lmsr_slippage_pct =
            estimate_lmsr_slippage(trade_amt, entry_price, liquidity_proxy_usd);
        let effective_fee_pct = if is_maker_cost_mode {
            maker_fee_pct
        } else {
            taker_fee_pct
        };
        let effective_lmsr_slippage_pct = if is_maker_cost_mode {
            base_lmsr_slippage_pct * maker_slippage_factor
        } else {
            base_lmsr_slippage_pct
        };
        let effective_gas_usd = if is_maker_cost_mode {
            maker_gas_usd
        } else {
            gas_usd
        };
        let gas_pct = if trade_amt > 0.0 {
            effective_gas_usd / trade_amt
        } else {
            0.0
        };
        let directional_prob = (0.5 + score.abs().min(1.0) * 0.35).clamp(0.50, 0.99);
        let expected_raw_edge = expected_resolution_return(directional_prob, entry_price)
            .expect("validated synthetic price and bounded heuristic probability");
        let required_edge =
            effective_fee_pct + effective_lmsr_slippage_pct + gas_pct + edge_safety_buffer;
        let entry_debit =
            trade_amt * (1.0 + effective_fee_pct + effective_lmsr_slippage_pct) + effective_gas_usd;
        if !entry_debit.is_finite() || entry_debit > balance {
            inc_skip(&mut skipped, "entry_cost_exceeds_balance");
            continue;
        }
        if expected_raw_edge <= required_edge {
            inc_skip(&mut skipped, "edge_gate");
            continue;
        }

        let exit = simulate_binary_trade_exit(
            signal,
            exit_mode,
            entry_price,
            candles[i].close,
            next,
            rolling_vol,
            trade_amt,
            liquidity_proxy_usd,
            effective_fee_pct,
            effective_lmsr_slippage_pct,
            effective_gas_usd,
            take_profit_pct,
            stop_loss_pct,
        );
        let pnl = exit.gross_pnl - exit.total_cost;
        let exit_time_ms = (entry_time_ms + exit.exit_time_offset_ms).min(resolution_time_ms);
        let resolution_dt = Utc.timestamp_millis_opt(resolution_time_ms).single();
        let exit_dt = Utc.timestamp_millis_opt(exit_time_ms).single();
        let entry_minute_of_day_utc = entry_dt
            .as_ref()
            .map(|dt| dt.hour() * 60 + dt.minute())
            .unwrap_or(0);
        let holding_minutes = ((exit_time_ms - entry_time_ms).max(0) / 60_000) as i64;
        let edge_margin_pp = (expected_raw_edge - required_edge) * 100.0;
        let next_return_pct = if candles[i].close > 0.0 {
            (next.close / candles[i].close - 1.0) * 100.0
        } else {
            0.0
        };

        balance += pnl;
        balance = balance.max(0.0);
        cumulative_pnl += pnl;

        total += 1;
        if exit.trade_won {
            wins += 1;
        }
        if adaptive_stand_down {
            recent_health.push_back(exit.trade_won);
            if recent_health.len() > stand_down_window {
                recent_health.pop_front();
            }
            if recent_health.len() == stand_down_window {
                let trade_break_even_wr = 50.0 + (exit.total_cost / trade_amt) * 50.0;
                if should_trigger_stand_down(
                    &recent_health,
                    stand_down_window,
                    trade_break_even_wr,
                    stand_down_margin_pp,
                ) {
                    stand_down_cooldown_left = stand_down_cooldown_candles;
                    inc_skip(&mut skipped, "stand_down_triggered");
                }
            }
        }

        sum_stake += trade_amt;
        sum_fee_frac += exit.fee_cost / trade_amt;
        sum_gas_frac += exit.gas_cost / trade_amt;
        sum_total_cost_frac += exit.total_cost / trade_amt;
        by_signal
            .entry(signal.as_str().to_string())
            .or_default()
            .push(exit.trade_won, pnl, exit.total_cost, trade_amt);
        by_regime.entry(regime.clone()).or_default().push(
            exit.trade_won,
            pnl,
            exit.total_cost,
            trade_amt,
        );

        pnl_series.push(json!({ "i": i, "pnl": cumulative_pnl, "balance": balance }));
        trades_out.push(json!({
            "candle":      i,
            "entry_time_ms": entry_time_ms,
            "entry_time_utc": entry_dt.as_ref().map(|dt| dt.to_rfc3339()),
            "exit_time_ms": exit_time_ms,
            "exit_time_utc": exit_dt.as_ref().map(|dt| dt.to_rfc3339()),
            "resolution_time_ms": resolution_time_ms,
            "resolution_time_utc": resolution_dt.as_ref().map(|dt| dt.to_rfc3339()),
            "entry_hour_utc": entry_hour_utc,
            "entry_weekday_utc": entry_weekday_utc,
            "entry_minute_of_day_utc": entry_minute_of_day_utc,
            "entry_session_utc": entry_session,
            "holding_minutes": holding_minutes,
            "regime":      regime,
            "volatility":  rolling_vol,
            "entry_threshold_used": entry_threshold_used,
            "side_threshold_used": side_threshold_used,
            "signal":      signal.as_str(),
            "entry_price": entry_price,
            "stake":       trade_amt,
            "shares": trade_amt / entry_price,
            "heuristic_directional_probability": directional_prob,
            "edge_units": "expected_return_per_stake_dollar",
            "expected_raw_edge": expected_raw_edge,
            "required_edge": required_edge,
            "gross_pnl":   exit.gross_pnl,
            "fee_cost":    exit.fee_cost,
            "gas_usd":     exit.gas_cost,
            "total_cost":  exit.total_cost,
            "taker_fee_pct": taker_fee_pct,
            "effective_fee_pct": effective_fee_pct,
            "lmsr_slippage_pct": effective_lmsr_slippage_pct,
            "base_lmsr_slippage_pct": base_lmsr_slippage_pct,
            "execution_cost_mode": execution_cost_mode,
            "exit_mode": exit_mode.as_str(),
            "take_profit_pct": take_profit_pct,
            "stop_loss_pct": stop_loss_pct,
            "exit_type": exit.exit_type,
            "exit_price": exit.exit_price,
            "resolution_price": exit.resolution_price,
            "exit_fee_cost": exit.exit_fee_cost,
            "exit_fee_pct": exit.exit_fee_pct,
            "exit_slippage_pct": exit.exit_slippage_pct,
            "path_low_price": symmetric_prices.then_some(exit.path_low),
            "path_high_price": symmetric_prices.then_some(exit.path_high),
            "resolved_won": exit.resolved_won,
            "confidence_margin": confidence_margin,
            "edge_margin_pp": edge_margin_pp,
            "next_return_pct": next_return_pct,
            "pnl":         pnl,
            "won":         exit.trade_won,
            "rsi":         rsi.rsi,
            "macd_hist":   macd.hist,
            "macd_hist_delta": macd.hist_delta,
            "momentum_pct": momentum_pct,
            "score":       score,
        }));
    }

    // ── Aggregate stats ───────────────────────────────────────────────────────
    let win_rate = sample_average(wins as f64 * 100.0, total);

    // Max drawdown
    let mut peak = init;
    let mut max_dd = 0.0f64;
    let mut running_bal = init;
    for t in &trades_out {
        running_bal += t.get("pnl").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if running_bal > peak {
            peak = running_bal;
        }
        let dd = peak - running_bal;
        if dd > max_dd {
            max_dd = dd;
        }
    }

    // Diagnostic mean/std of per-trade dollar P&L; no annualization assumption.
    let pnls: Vec<f64> = trades_out
        .iter()
        .filter_map(|t| t.get("pnl").and_then(|v| v.as_f64()))
        .collect();
    let sharpe = per_trade_sharpe(&pnls);
    let avg_stake_usd = sample_average(sum_stake, total);
    let avg_fee_pct = sample_average(sum_fee_frac * 100.0, total);
    let avg_gas_pct = sample_average(sum_gas_frac * 100.0, total);
    let avg_total_cost_pct = sample_average(sum_total_cost_frac * 100.0, total);
    let symmetric_resolution_audit = symmetric_prices && exit_mode == ExitMode::NextCandle;
    let break_even_win_rate_pct = if total > 0 && symmetric_resolution_audit {
        Some(50.0 + (sum_total_cost_frac / total as f64) * 50.0)
    } else {
        None
    };
    let edge_gap_pp = win_rate
        .zip(break_even_win_rate_pct)
        .map(|(win, rate)| win - rate);
    let expectancy_per_trade = sample_average(cumulative_pnl, total);
    let stand_down_recommended = edge_gap_pp.map(|gap| total == 0 || gap <= 0.0);

    let by_signal_json: serde_json::Map<String, Value> = by_signal
        .into_iter()
        .map(|(k, v)| (k, v.as_json(symmetric_resolution_audit)))
        .collect();
    let by_regime_json: serde_json::Map<String, Value> = by_regime
        .into_iter()
        .map(|(k, v)| (k, v.as_json(symmetric_resolution_audit)))
        .collect();
    let skipped_json: serde_json::Map<String, Value> =
        skipped.into_iter().map(|(k, v)| (k, json!(v))).collect();
    let regime_allowlist_json: Vec<Value> =
        regime_allowlist.into_iter().map(Value::String).collect();
    let session_allowlist_json: Vec<Value> =
        session_allowlist.into_iter().map(Value::String).collect();
    let blocked_hours_utc_json: Vec<Value> =
        blocked_hours_utc.into_iter().map(Value::from).collect();
    let regime_side_modes_json: serde_json::Map<String, Value> = regime_side_modes
        .into_iter()
        .map(|(k, mode)| (k, json!(mode.as_str())))
        .collect();

    info!(
        "Backtest complete: trades={} pnl={:.4} elapsed={:.2?}",
        total,
        cumulative_pnl,
        started_at.elapsed()
    );

    Json(json!({
        "result_contract_version": RESULT_CONTRACT_VERSION,
        "mode": "synthetic_ta",
        "scope": "simplified_ta_candles_with_hypothetical_binary_purchases",
        "ta_signal_model": {
            "version": 2,
            "directional_regime_names": ["TREND_UP", "TREND_DOWN"],
            "directional_regime_contribution": 0.08,
            "legacy_directional_aliases": ["BULL", "BEAR"],
            "legacy_results_comparable": false,
        },
        "price_assumptions": {
            "type": "fixed_independent_synthetic_purchase_prices",
            "yes": entry_price_yes,
            "no": entry_price_no,
            "historical_venue_quotes": false,
        },
        "sizing_config": {
            "stake_pct": cfg.stake_pct, "trade_amount": cfg.trade_amount,
            "min_stake": cfg.min_stake, "max_stake": cfg.max_stake,
            "min_entry_price": cfg.min_entry_price, "max_entry_price": cfg.max_entry_price,
        },
        "entry_gate": {
            "version": 2,
            "units": "expected_return_per_stake_dollar",
            "formula": "heuristic_directional_probability / entry_price - 1 > fees + slippage + gas / stake + buffer",
            "probability_source": "uncalibrated_ta_score_mapping",
            "legacy_results_comparable": false,
        },
        "requested_days":   requested_days,
        "days":             days,
        "candles_fetched":  candles.len(),
        "candles_analyzed": analyzed_candles,
        "effective_days_covered": effective_days_covered,
        "data_coverage": coverage,
        "data_start_utc":   data_start_utc,
        "data_end_utc":     data_end_utc,
        "data_warning":     data_warning,
        "window":           warmup,
        "entry_threshold":  entry_threshold,
        "regime_thresholds": {
            "bull": bull_entry_threshold,
            "bear": bear_entry_threshold,
            "chop": chop_entry_threshold,
            "buy_yes": buy_yes_threshold,
            "buy_no": buy_no_threshold,
            "allow_buy_yes": allow_buy_yes,
            "allow_buy_no": allow_buy_no,
            "regime_allowlist": regime_allowlist_json,
            "session_allowlist": session_allowlist_json,
            "blocked_hours_utc": blocked_hours_utc_json,
            "regime_side_modes": Value::Object(regime_side_modes_json),
            "require_regime_alignment": require_regime_alignment,
            "anti_bias_strength": anti_bias_strength,
            "adaptive_stand_down": adaptive_stand_down,
            "stand_down_window": stand_down_window,
            "stand_down_margin_pp": stand_down_margin_pp,
            "stand_down_cooldown_candles": stand_down_cooldown_candles,
            "high_vol": high_vol_entry_threshold,
            "high_volatility_threshold": high_volatility_threshold,
            "min_confidence_margin": min_confidence_margin,
            "time_of_day_weight": time_of_day_weight,
        },
        "gas_usd":          gas_usd,
        "execution_mode":   execution_cost_mode,
        "exit_mode": exit_mode.as_str(),
        "take_profit_pct": take_profit_pct,
        "stop_loss_pct": stop_loss_pct,
        "maker_fee_pct": maker_fee_pct,
        "maker_slippage_factor": maker_slippage_factor,
        "maker_gas_usd": maker_gas_usd,
        "liquidity_proxy_usd": liquidity_proxy_usd,
        "max_trade_pct":    max_trade_pct,
        "min_trade_usd":    min_trade_usd,
        "edge_safety_buffer": edge_safety_buffer,
        "total_trades":     total,
        "wins":             wins,
        "losses":           total - wins,
        "win_rate":         win_rate,
        "initial_balance":  init,
        "final_balance":    balance,
        "total_pnl":        cumulative_pnl,
        "max_drawdown":     max_dd,
        "sharpe_ratio":     sharpe,
        "sharpe_basis": "unannualized_per_trade_dollar_pnl",
        "synthetic_execution": true,
        "simulation_limitations": [
            "Fixed synthetic token purchase assumptions, not historical venue quotes",
            "Simplified TA score and heuristic probability are not the live fusion/ML strategy or a calibrated forecast",
            "No venue liquidity, maker queue, latency, partial fills, receipt or portfolio execution replay in synthetic_ta mode",
            "Next-candle BTC direction defines the hypothetical binary payout; it is not verified venue settlement",
            "TP/SL path is an uncalibrated candle-extreme proxy, supported only for 0.50 entries",
            "Win-rate break-even summaries are available only for symmetric 0.50 resolution experiments; varying stakes/costs make these descriptive averages, not exact portfolio break-even",
            "Entry gate version 2 corrects probability/return cost units; old backtest trade selection is not numerically comparable",
            "Missing candle intervals are not reconstructed; feature/outcome windows crossing a gap are excluded",
            "TA signal model version 2 connects classifier TREND_UP/TREND_DOWN labels to the unchanged directional weight; older synthetic trade selection is not numerically comparable"
        ],
        "edge_audit": {
            "win_rate_break_even_scope": if symmetric_resolution_audit { "descriptive_symmetric_0.50_resolution_average" } else { "unavailable_for_asymmetric_prices_or_proxy_exits" },
            "avg_stake_usd": avg_stake_usd,
            "avg_fee_pct": avg_fee_pct,
            "avg_gas_pct": avg_gas_pct,
            "avg_total_cost_pct": avg_total_cost_pct,
            "break_even_win_rate_pct": break_even_win_rate_pct,
            "edge_gap_pp": edge_gap_pp,
            "expectancy_per_trade": expectancy_per_trade,
            "stand_down_recommended": stand_down_recommended,
            "by_signal": Value::Object(by_signal_json),
            "by_regime": Value::Object(by_regime_json),
            "skipped": Value::Object(skipped_json),
        },
        "pnl_series":       pnl_series,
        "trades":           trades_out,
    }))
}
