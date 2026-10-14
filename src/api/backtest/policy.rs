//! Simplified TA selection rules; these are not the live fusion scheduler.
use crate::{domain::Signal, signals::Candle};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy)]
pub(super) struct TimeOfDayScoreWeights {
    pub(super) technical: f64,
    pub(super) regime: f64,
}

pub(super) fn backtest_time_of_day_weights(hour_utc: u32) -> TimeOfDayScoreWeights {
    match hour_utc {
        0..=7 => TimeOfDayScoreWeights {
            technical: 0.92,
            regime: 0.90,
        },
        8..=15 => TimeOfDayScoreWeights {
            technical: 1.00,
            regime: 1.02,
        },
        _ => TimeOfDayScoreWeights {
            technical: 1.06,
            regime: 1.10,
        },
    }
}

pub(super) fn weighted_regime_score(regime: &str, weight: f64) -> f64 {
    let contribution = match regime {
        "BULL" | "TREND_UP" => 0.08,
        "BEAR" | "TREND_DOWN" => -0.08,
        _ => 0.0,
    };
    contribution * weight
}

pub(super) fn window_volatility(window: &[Candle]) -> f64 {
    if window.len() < 3 {
        return 0.0;
    }
    let mut rets: Vec<f64> = Vec::with_capacity(window.len() - 1);
    for pair in window.windows(2) {
        let prev = pair[0].close;
        let curr = pair[1].close;
        if prev > 0.0 {
            rets.push(curr / prev - 1.0);
        }
    }
    if rets.len() < 2 {
        return 0.0;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (rets.len() as f64 - 1.0);
    var.sqrt()
}

pub(super) fn effective_anti_bias_strength(
    allow_buy_yes: bool,
    allow_buy_no: bool,
    anti_bias_strength: f64,
) -> f64 {
    if allow_buy_yes && allow_buy_no {
        anti_bias_strength
    } else {
        0.0
    }
}

pub(super) fn classify_raw_signal(
    score: f64,
    buy_yes_threshold: f64,
    buy_no_threshold: f64,
) -> Signal {
    if score > buy_yes_threshold {
        Signal::BuyYes
    } else if score < -buy_no_threshold {
        Signal::BuyNo
    } else {
        Signal::Hold
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SideMode {
    Both,
    YesOnly,
    NoOnly,
    None,
}

impl SideMode {
    pub(super) fn allow_buy_yes(self) -> bool {
        matches!(self, SideMode::Both | SideMode::YesOnly)
    }

    pub(super) fn allow_buy_no(self) -> bool {
        matches!(self, SideMode::Both | SideMode::NoOnly)
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            SideMode::Both => "both",
            SideMode::YesOnly => "yes_only",
            SideMode::NoOnly => "no_only",
            SideMode::None => "none",
        }
    }
}

pub(super) fn parse_side_mode(raw: &str) -> Option<SideMode> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "both" => Some(SideMode::Both),
        "yes_only" => Some(SideMode::YesOnly),
        "no_only" => Some(SideMode::NoOnly),
        "none" => Some(SideMode::None),
        _ => None,
    }
}

pub(super) fn normalize_regime_key(regime: &str) -> String {
    regime.trim().to_ascii_uppercase()
}

pub(super) fn normalize_session_key(session: &str) -> String {
    session.trim().to_ascii_lowercase()
}

pub(super) fn session_bucket(hour_utc: u32) -> &'static str {
    match hour_utc {
        0..=7 => "asia",
        8..=15 => "europe",
        _ => "us",
    }
}

pub(super) fn apply_side_and_regime_filters(
    raw_signal: Signal,
    allow_buy_yes: bool,
    allow_buy_no: bool,
    regime_allowlist: &HashSet<String>,
    regime_side_modes: &HashMap<String, SideMode>,
    require_regime_alignment: bool,
    regime: &str,
) -> (Signal, Option<&'static str>) {
    let mut signal = raw_signal;
    let regime_key = normalize_regime_key(regime);
    if !regime_allowlist.is_empty() && !regime_allowlist.contains(&regime_key) {
        return (Signal::Hold, Some("regime_blocked"));
    }

    let (effective_allow_buy_yes, effective_allow_buy_no) = regime_side_modes
        .get(&regime_key)
        .copied()
        .map(|mode| (mode.allow_buy_yes(), mode.allow_buy_no()))
        .unwrap_or((allow_buy_yes, allow_buy_no));

    if signal == Signal::BuyYes && !effective_allow_buy_yes {
        return (Signal::Hold, Some("side_disabled_yes"));
    }
    if signal == Signal::BuyNo && !effective_allow_buy_no {
        return (Signal::Hold, Some("side_disabled_no"));
    }
    if signal != Signal::Hold && require_regime_alignment {
        let aligned = match regime_key.as_str() {
            "BULL" | "TREND_UP" => signal == Signal::BuyYes,
            "BEAR" | "TREND_DOWN" => signal == Signal::BuyNo,
            _ => false,
        };
        if !aligned {
            signal = Signal::Hold;
            return (signal, Some("regime_mismatch"));
        }
    }
    (signal, None)
}

pub(super) fn should_trigger_stand_down(
    recent_health: &VecDeque<bool>,
    stand_down_window: usize,
    trade_break_even_wr: f64,
    stand_down_margin_pp: f64,
) -> bool {
    if recent_health.len() < stand_down_window {
        return false;
    }
    let health_wins = recent_health.iter().filter(|w| **w).count();
    let health_wr = health_wins as f64 / stand_down_window as f64 * 100.0;
    health_wr < trade_break_even_wr + stand_down_margin_pp
}
