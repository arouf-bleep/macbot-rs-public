//! Binary resolution arithmetic and an explicitly uncalibrated candle TP/SL proxy.
use super::KLINE_15M_MS;
use crate::{
    domain::Signal,
    fusion::estimate_taker_fee,
    signals::{lmsr::estimate_lmsr_slippage, Candle},
};

/// Expected gross resolution return per stake dollar under a declared probability.
/// This arithmetic does not establish that a score-derived probability is calibrated.
pub(super) fn expected_resolution_return(probability: f64, price: f64) -> Option<f64> {
    if !probability.is_finite()
        || !(0.0..=1.0).contains(&probability)
        || !price.is_finite()
        || !(0.0..1.0).contains(&price)
        || price <= 0.0
    {
        return None;
    }
    Some(probability / price - 1.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExitMode {
    NextCandle,
    TakerTpSl,
}

impl ExitMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            ExitMode::NextCandle => "next_candle",
            ExitMode::TakerTpSl => "taker_tp_sl",
        }
    }
}

pub(super) fn parse_exit_mode(raw: &str) -> Option<ExitMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "next_candle" | "resolution" => Some(ExitMode::NextCandle),
        "taker_tp_sl" | "tp_sl" => Some(ExitMode::TakerTpSl),
        _ => None,
    }
}

#[derive(Debug, Clone)]
pub(super) struct SimulatedExit {
    pub(super) exit_price: f64,
    pub(super) resolution_price: f64,
    pub(super) gross_pnl: f64,
    pub(super) total_cost: f64,
    pub(super) fee_cost: f64,
    pub(super) gas_cost: f64,
    pub(super) exit_type: &'static str,
    pub(super) trade_won: bool,
    pub(super) exit_fee_cost: f64,
    pub(super) exit_fee_pct: f64,
    pub(super) exit_slippage_pct: f64,
    pub(super) path_low: f64,
    pub(super) path_high: f64,
    pub(super) exit_time_offset_ms: i64,
    pub(super) resolved_won: bool,
}

pub(super) fn binary_price_scale(strike_price: f64, rolling_vol: f64) -> f64 {
    let rel_scale = rolling_vol.max(0.0015);
    (strike_price.abs() * rel_scale).max(1.0)
}

pub(super) fn proxy_yes_binary_price(strike_price: f64, underlying_price: f64, scale: f64) -> f64 {
    let z = ((underlying_price - strike_price) / scale).clamp(-8.0, 8.0);
    1.0 / (1.0 + (-z).exp())
}

pub(super) fn binary_path_for_signal(
    signal: Signal,
    strike_price: f64,
    next: &Candle,
    rolling_vol: f64,
) -> (f64, f64, f64, bool) {
    let scale = binary_price_scale(strike_price, rolling_vol);
    let yes_low = proxy_yes_binary_price(strike_price, next.low, scale);
    let yes_high = proxy_yes_binary_price(strike_price, next.high, scale);
    let btc_went_up = next.close > strike_price;
    let resolution_yes_price = if btc_went_up { 1.0 } else { 0.0 };

    match signal {
        Signal::BuyYes => (yes_low, yes_high, resolution_yes_price, btc_went_up),
        Signal::BuyNo => (
            1.0 - yes_high,
            1.0 - yes_low,
            1.0 - resolution_yes_price,
            !btc_went_up,
        ),
        Signal::Hold => (0.5, 0.5, 0.5, false),
    }
}

// Preserve the explicit synthetic exit contract; payoff inputs are reviewed together.
#[allow(clippy::too_many_arguments)]
pub(super) fn simulate_binary_trade_exit(
    signal: Signal,
    exit_mode: ExitMode,
    entry_price: f64,
    strike_price: f64,
    next: &Candle,
    rolling_vol: f64,
    trade_amt: f64,
    liquidity_proxy_usd: f64,
    entry_fee_pct: f64,
    entry_slippage_pct: f64,
    entry_gas_usd: f64,
    take_profit_pct: f64,
    stop_loss_pct: f64,
) -> SimulatedExit {
    let shares = trade_amt / entry_price;
    let entry_fee_cost = trade_amt * (entry_fee_pct + entry_slippage_pct);
    let (path_low, path_high, resolution_price, resolved_won) =
        binary_path_for_signal(signal, strike_price, next, rolling_vol);

    let mut exit_price = resolution_price;
    let mut exit_type = "resolution";
    let mut exit_fee_cost = 0.0;
    let mut exit_fee_pct = 0.0;
    let mut exit_slippage_pct = 0.0;
    let mut exit_time_offset_ms = KLINE_15M_MS;

    if exit_mode == ExitMode::TakerTpSl {
        let take_profit_price = (entry_price * (1.0 + take_profit_pct)).clamp(0.0, 1.0);
        let stop_loss_price = (entry_price * (1.0 - stop_loss_pct)).clamp(0.0, 1.0);
        let take_profit_hit = path_high >= take_profit_price;
        let stop_loss_hit = path_low <= stop_loss_price;

        if take_profit_hit && stop_loss_hit {
            exit_price = stop_loss_price;
            exit_type = "stop_loss_same_candle";
            exit_time_offset_ms = KLINE_15M_MS / 2;
        } else if stop_loss_hit {
            exit_price = stop_loss_price;
            exit_type = "stop_loss";
            exit_time_offset_ms = KLINE_15M_MS / 2;
        } else if take_profit_hit {
            exit_price = take_profit_price;
            exit_type = "take_profit";
            exit_time_offset_ms = KLINE_15M_MS / 2;
        }

        if exit_type != "resolution" {
            let exit_notional = shares * exit_price;
            exit_fee_pct = estimate_taker_fee(exit_price);
            exit_slippage_pct =
                estimate_lmsr_slippage(exit_notional, exit_price, liquidity_proxy_usd);
            exit_fee_cost = exit_notional * (exit_fee_pct + exit_slippage_pct);
        }
    }

    let gross_pnl = shares * (exit_price - entry_price);
    let gas_cost = if exit_type == "resolution" {
        entry_gas_usd
    } else {
        entry_gas_usd * 2.0
    };
    let fee_cost = entry_fee_cost + exit_fee_cost;
    let total_cost = fee_cost + gas_cost;
    let net_pnl = gross_pnl - total_cost;

    SimulatedExit {
        exit_price,
        resolution_price,
        gross_pnl,
        total_cost,
        fee_cost,
        gas_cost,
        exit_type,
        trade_won: net_pnl > 0.0,
        exit_fee_cost,
        exit_fee_pct,
        exit_slippage_pct,
        path_low,
        path_high,
        exit_time_offset_ms,
        resolved_won,
    }
}
