//! Adaptive signal fusion engine.
//! Combines microstructure, momentum and macro signals into a calibrated
//! posterior probability and applies EV-aware execution gates.

pub(crate) mod replay;

use std::collections::HashMap;

use crate::config::FusionCalibration;
use crate::domain::Signal;
use crate::signals::lmsr::estimate_lmsr_slippage;
use crate::signals::{
    AggressorData, DivergenceData, FearGreedData, FundingData, HaData, LunarcrushSentimentData,
    MacdData, ObData, OiMomentumData, PcrData, PhaseData, RsiData, TickVelData, VwapData,
};

// ── Polymarket Crypto Fee Schedule (Jan 2026) ────────────────────────────────
// fee = price * feeRate * (price * (1 - price))^exponent
// Crypto markets: feeRate = 0.25, exponent = 2
// Maker rebate: 20% of taker fees collected (makers pay nothing)
const CRYPTO_FEE_RATE: f64 = 0.25;
const CRYPTO_FEE_EXPONENT: f64 = 2.0;
const MAKER_REBATE_PCT: f64 = 0.20;

// ── Default signal weights (sum = 1.0 after normalization) ────────────────
pub fn default_weights() -> HashMap<String, f64> {
    let mut m = HashMap::new();
    m.insert("orderbook_imbalance".into(), 0.2069);
    m.insert("tick_velocity".into(), 0.1724);
    m.insert("vwap_confluence".into(), 0.1207);
    m.insert("price_divergence".into(), 0.1034);
    m.insert("deribit_pcr".into(), 0.0690);
    m.insert("rsi_macd_ha".into(), 0.0517);
    m.insert("fear_greed".into(), 0.0259);
    m.insert("funding_rate".into(), 0.0086);
    m.insert("oi_momentum".into(), 0.0345);
    m.insert("aggressor_ratio".into(), 0.0517);
    m.insert("lunarcrush_sentiment".into(), 0.0172);
    // New contextual/enrichment features (start with low weights)
    m.insert("hour_of_day".into(), 0.010);
    m.insert("day_of_week".into(), 0.010);
    m.insert("distance_from_round_1000".into(), 0.015);
    m.insert("distance_from_round_500".into(), 0.010);
    m.insert("volatility_10".into(), 0.020);
    m.insert("volatility_30".into(), 0.020);
    m.insert("window_momentum".into(), 0.030);
    m.insert("long_liq_volume".into(), 0.020);
    m.insert("short_liq_volume".into(), 0.020);
    m.insert("depth_imbalance_5".into(), 0.012); // reduced: correlated with orderbook_imbalance
    m.insert("polymarket_flow".into(), 0.040); // Strong alpha from Polymarket order flow
    m.insert("vpin_microstructure".into(), 0.035); // Experimental VPIN-style feature
    m.insert("options_advanced".into(), 0.020); // Options skew/max pain
    m.insert("whale_flow".into(), 0.015); // Whale alert
    m.insert("cross_market".into(), 0.025); // Experimental ETH/SOL correlation feature
    m.insert("latency_arb".into(), 0.010); // Binance→Polymarket lag (degraded: REST-only, ~10s cadence)
    m.insert("news_sentiment".into(), 0.0); // explicitly tracked, but default-off until proven
    m.insert("kyle_lambda".into(), 0.020); // Kyle's Lambda price impact
    m.insert("btc_final_momentum".into(), 0.030); // BTC final-minute linear extrapolation (strongest LATE phase)
    m.insert("hawkes_flow".into(), 0.015); // Hawkes-style order-flow clustering
    m
}

// ── Math helpers ────────────────────────────────────────────────────────────
#[inline]
fn clamp_unit(v: f64) -> f64 {
    if v.is_finite() {
        v.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[inline]
fn logit(p: f64) -> f64 {
    let p = p.clamp(0.001, 0.999);
    (p / (1.0 - p)).ln()
}

#[inline]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

#[inline]
fn conf_to_strength(confidence: f64) -> f64 {
    ((confidence.clamp(0.5, 0.99) - 0.5) * 2.0).clamp(0.0, 1.0)
}

#[inline]
fn signed_vote(direction: &str, confidence: f64) -> f64 {
    let sign = match direction {
        "UP" => 1.0,
        "DOWN" => -1.0,
        _ => 0.0,
    };
    sign * conf_to_strength(confidence)
}

#[derive(Debug, Clone, Copy, Default)]
struct Feature {
    signed: f64,      // [-1, +1]
    reliability: f64, // [0, +inf), clamped at use sites
}

impl Feature {
    fn new(signed: f64, reliability: f64) -> Self {
        if !signed.is_finite() || !reliability.is_finite() {
            return Self::default();
        }
        Self {
            signed: clamp_unit(signed),
            reliability,
        }
    }

    fn effective(self) -> f64 {
        let r = self.reliability.clamp(0.0, 1.0);
        clamp_unit(self.signed * r)
    }
}

#[derive(Debug, Clone, Copy)]
struct PhaseProfile {
    micro: f64,
    momentum: f64,
    macro_: f64,
}

fn phase_profile(phase: &str) -> PhaseProfile {
    match phase {
        "EARLY" => PhaseProfile {
            micro: 1.00,
            momentum: 0.92,
            macro_: 0.80,
        },
        "LATE" => PhaseProfile {
            micro: 1.10,
            momentum: 0.80,
            macro_: 0.75,
        },
        _ => PhaseProfile {
            micro: 0.95,
            momentum: 1.00,
            macro_: 1.00,
        },
    }
}

fn phase_signal_multiplier(phase: &str, signal_name: &str) -> f64 {
    match phase {
        "EARLY" => match signal_name {
            // Settlement-specific signals should contribute very little when the
            // market is still far from resolution.
            "btc_final_momentum" => 0.15,
            "window_momentum" => 0.70,
            "hawkes_flow" => 0.80,
            "kyle_lambda" => 0.90,
            // Cross-market and latency dislocations are often more actionable
            // earlier, before the local market fully reprices.
            "cross_market" => 1.12,
            "latency_arb" => 1.08,
            // Broader macro/context can still matter early in the window.
            "fear_greed"
            | "funding_rate"
            | "options_advanced"
            | "whale_flow"
            | "lunarcrush_sentiment"
            | "news_sentiment" => 1.05,
            _ => 1.0,
        },
        "LATE" => match signal_name {
            // Near resolution, prefer fast microstructure/late-window signals.
            "btc_final_momentum" => 1.35,
            "window_momentum" => 1.15,
            "hawkes_flow" => 1.20,
            "kyle_lambda" => 1.15,
            "polymarket_flow" => 1.15,
            "orderbook_imbalance" => 1.08,
            "aggressor_ratio" => 1.08,
            // Macro/context should decay as the settlement clock dominates.
            "cross_market" => 0.82,
            "latency_arb" => 0.92,
            "fear_greed"
            | "funding_rate"
            | "options_advanced"
            | "whale_flow"
            | "lunarcrush_sentiment"
            | "news_sentiment"
            | "hour_of_day"
            | "day_of_week"
            | "distance_from_round_1000"
            | "distance_from_round_500" => 0.70,
            _ => 1.0,
        },
        _ => 1.0,
    }
}

#[inline]
fn feature_for_phase(signal_name: &str, phase: &str, signed: f64, reliability: f64) -> Feature {
    Feature::new(
        signed,
        reliability * phase_signal_multiplier(phase, signal_name),
    )
}

fn normalize_weights(
    input: Option<HashMap<String, f64>>,
    obi_stable: bool,
) -> HashMap<String, f64> {
    let mut merged = default_weights();

    if let Some(custom) = input {
        for (k, v) in custom {
            if merged.contains_key(&k) && v.is_finite() {
                merged.insert(k, v.max(0.0));
            }
        }
    }

    if !obi_stable {
        if let Some(w) = merged.get_mut("orderbook_imbalance") {
            *w *= 0.35;
        }
    }

    let mut sum: f64 = merged.values().copied().sum();
    if !sum.is_finite() || sum <= f64::EPSILON {
        tracing::warn!("signal weight sum is zero — falling back to default weights");
        merged = default_weights();
        sum = merged.values().copied().sum();
    }

    if sum > f64::EPSILON {
        for v in merged.values_mut() {
            *v /= sum;
        }
    }

    // Post-normalization sanity check
    let final_sum: f64 = merged.values().sum();
    if (final_sum - 1.0).abs() > 0.001 {
        tracing::warn!(
            "signal weights not properly normalized: sum={final_sum:.6} — falling back to defaults"
        );
        merged = default_weights();
        let s: f64 = merged.values().copied().sum();
        for v in merged.values_mut() {
            *v /= s;
        }
    }
    merged
}

pub fn apply_time_of_day_weighting(weights: &mut HashMap<String, f64>, hour_utc: u32) {
    let defaults = default_weights();
    let ensure_scaled = |map: &mut HashMap<String, f64>, name: &str, mult: f64| {
        let base = map
            .get(name)
            .copied()
            .or_else(|| defaults.get(name).copied())
            .unwrap_or(0.0);
        map.insert(name.to_string(), base * mult);
    };

    let apply_many = |map: &mut HashMap<String, f64>, names: &[&str], mult: f64| {
        for name in names {
            ensure_scaled(map, name, mult);
        }
    };

    let micro_names = [
        "orderbook_imbalance",
        "tick_velocity",
        "aggressor_ratio",
        "depth_imbalance_5",
        "polymarket_flow",
        "vpin_microstructure",
        "latency_arb",
        "kyle_lambda",
        "hawkes_flow",
    ];
    let macro_names = [
        "news_sentiment",
        "fear_greed",
        "funding_rate",
        "oi_momentum",
        "options_advanced",
        "whale_flow",
        "cross_market",
        "deribit_pcr",
    ];
    let momentum_names = [
        "vwap_confluence",
        "price_divergence",
        "rsi_macd_ha",
        "window_momentum",
        "btc_final_momentum",
    ];

    match hour_utc {
        0..=7 => {
            apply_many(weights, &micro_names, 0.82);
            apply_many(weights, &macro_names, 0.90);
            apply_many(weights, &momentum_names, 0.92);
        }
        8..=15 => {
            apply_many(weights, &micro_names, 1.00);
            apply_many(weights, &macro_names, 1.02);
            apply_many(weights, &momentum_names, 1.00);
        }
        _ => {
            apply_many(weights, &micro_names, 1.18);
            apply_many(weights, &macro_names, 1.10);
            apply_many(weights, &momentum_names, 1.06);
        }
    }
}

/// Polymarket crypto taker fee: `feeRate * (p * (1 - p))^exponent`
/// Returns fee as a fraction of notional (e.g. 0.0156 = 1.56%).
/// At p=0.50 → ~1.56%, at p=0.80 → ~0.64%, at p=0.95 → ~0.06%.
pub fn estimate_taker_fee(prob: f64) -> f64 {
    let p = if prob.is_finite() {
        prob.clamp(0.01, 0.99)
    } else {
        0.5
    };
    let base = p * (1.0 - p); // peaks at 0.25 when p=0.50
    let fee = CRYPTO_FEE_RATE * base.powf(CRYPTO_FEE_EXPONENT);
    // Floor prevents underflow to zero at price extremes (p near 0.01/0.99)
    // where base^exponent can lose all precision.
    fee.max(0.0001) // minimum 0.01% fee
}

/// Maker fee: zero on Polymarket (makers receive rebates instead).
/// Returns the rebate as a negative fee (credit).
pub fn estimate_maker_fee(prob: f64) -> f64 {
    let taker = estimate_taker_fee(prob);
    -(taker * MAKER_REBATE_PCT) // negative = credit
}

/// Expected return per dollar of entry notional, before gas. Fee and impact
/// estimates are fractions of the same notional, as required by risk gates.
pub fn entry_economics(
    p_up: f64,
    market_price: f64,
    buy_yes: bool,
    amount: f64,
    liquidity: f64,
) -> Option<(f64, f64, f64)> {
    if !p_up.is_finite()
        || !(0.0..=1.0).contains(&p_up)
        || !market_price.is_finite()
        || market_price <= 0.0
        || market_price >= 1.0
        || !amount.is_finite()
        || amount <= 0.0
        || !liquidity.is_finite()
        || liquidity < 0.0
    {
        return None;
    }
    let (probability, price) = if buy_yes {
        (p_up, market_price)
    } else {
        (1.0 - p_up, 1.0 - market_price)
    };
    let fee = estimate_taker_fee(price);
    let impact = estimate_lmsr_slippage(amount, price, liquidity.max(1.0));
    let net = probability / price - 1.0 - fee - impact;
    net.is_finite().then_some((net, fee, impact))
}

// ── Fusion result ────────────────────────────────────────────────────────────
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FusionResult {
    pub signal: Signal,
    pub score: f64,
    pub confidence: f64,
    pub reason: String,
    pub net_ev: f64,
    pub taker_fee: f64,
    pub lmsr_slippage: f64,
    pub p_up: f64,
    /// Per-signal effective signed strength in [-1, +1].
    pub breakdown: HashMap<String, f64>,
    /// Final normalized weights that actually participated in live fusion.
    pub normalized_weights: HashMap<String, f64>,
    /// Weight * signed strength for each active fusion feature.
    pub weighted_contributions: HashMap<String, f64>,
}

impl FusionResult {
    fn hold(reason: &str, market_price: f64, normalized_weights: HashMap<String, f64>) -> Self {
        let zero_map: HashMap<String, f64> = normalized_weights
            .keys()
            .cloned()
            .map(|key| (key, 0.0))
            .collect();
        Self {
            signal: Signal::Hold,
            score: 0.0,
            confidence: 0.0,
            reason: reason.into(),
            net_ev: 0.0,
            taker_fee: estimate_taker_fee(market_price),
            lmsr_slippage: 0.0,
            p_up: if market_price.is_finite() {
                market_price.clamp(0.001, 0.999)
            } else {
                0.5
            },
            breakdown: zero_map.clone(),
            normalized_weights,
            weighted_contributions: zero_map,
        }
    }
}

// ── Enrichment data for new features ─────────────────────────────────────────
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EnrichmentData {
    pub hour_of_day: f64,         // [0, 1] normalized hour
    pub day_of_week: f64,         // [0, 1] normalized day
    pub distance_round_1000: f64, // [0, 1] distance from nearest $1000
    pub distance_round_500: f64,  // [0, 1] distance from nearest $500
    pub volatility_10: f64,       // stddev of last 10 1m returns
    pub volatility_30: f64,       // stddev of last 30 1m returns
    pub window_momentum: f64,     // price change since window open (pct)
    pub long_liq_volume: f64,     // long liquidation USD (last 5 min)
    pub short_liq_volume: f64,    // short liquidation USD (last 5 min)
    pub depth_imbalance_5: f64,   // Binance L2 top-5 imbalance [-1, 1]
    // New Phase 2 signals
    pub polymarket_flow_signal: f64, // [-1, 1] Polymarket order flow
    pub polymarket_flow_confidence: f64, // [0, 1]
    pub vpin_signal: f64,            // [-1, 1] VPIN microstructure
    pub vpin_confidence: f64,        // [0, 1]
    pub options_signal: f64,         // [-1, 1] Deribit advanced
    pub options_confidence: f64,     // [0, 1]
    pub whale_signal: f64,           // [-1, 1] whale/on-chain
    pub whale_confidence: f64,       // [0, 1]
    // Granger causality weight multipliers (applied to relevant signals)
    pub granger_funding_rate: f64, // 0.65..1.15
    pub granger_orderbook_imbalance: f64,
    pub granger_aggressor_ratio: f64,
    pub granger_vpin: f64,
    pub granger_tick_velocity: f64,
    // Cross-market and latency arb (advisory → now integrated)
    pub cross_market_signal: f64, // [-1, 1] ETH/SOL correlation heuristic
    pub cross_market_confidence: f64, // [0, 1]
    pub latency_arb_signal: f64,  // [-1, 1] Binance→Polymarket lag
    pub latency_arb_confidence: f64, // [0, 1]
    // Staleness flags — true means the signal failed to fetch this cycle
    pub stale_options: bool,
    pub stale_whale: bool,
    pub stale_cross_market: bool,
    pub stale_polymarket_flow: bool,
    // News NLP signal
    pub news_sentiment: f64,     // [-1, 1] headline sentiment
    pub news_event_type: String, // event classification (e.g. "Normal", "EtfFlow")
    // Raw VPIN value [0, 1] for circuit breaker (NOT the transformed signal)
    pub vpin_raw: f64,
    // BTC final-minute momentum (linear extrapolation near window close)
    pub btc_final_momentum: f64, // [-1, 1] extrapolated price change vs ref
    pub btc_final_momentum_confidence: f64, // [0, 1]
    // Kyle's Lambda: price impact of signed order flow
    pub kyle_lambda: f64, // [-1, 1] signal (negative = high impact -> caution)
    pub kyle_lambda_confidence: f64, // [0, 1] scales with R²
    pub kyle_lambda_raw: f64, // raw lambda coefficient
    // Hawkes-style order-flow clustering
    pub hawkes_flow: f64,       // [-1, 1] side-specific clustered flow pressure
    pub hawkes_confidence: f64, // [0, 1]
}

impl Default for EnrichmentData {
    fn default() -> Self {
        Self {
            hour_of_day: 0.0,
            day_of_week: 0.0,
            distance_round_1000: 0.0,
            distance_round_500: 0.0,
            volatility_10: 0.0,
            volatility_30: 0.0,
            window_momentum: 0.0,
            long_liq_volume: 0.0,
            short_liq_volume: 0.0,
            depth_imbalance_5: 0.0,
            polymarket_flow_signal: 0.0,
            polymarket_flow_confidence: 0.0,
            vpin_signal: 0.0,
            vpin_confidence: 0.0,
            options_signal: 0.0,
            options_confidence: 0.0,
            whale_signal: 0.0,
            whale_confidence: 0.0,
            // Granger weights default to 1.0 (neutral — no boost or penalty)
            granger_funding_rate: 1.0,
            granger_orderbook_imbalance: 1.0,
            granger_aggressor_ratio: 1.0,
            granger_vpin: 1.0,
            granger_tick_velocity: 1.0,
            // Cross-market and latency arb default to 0 (no signal)
            cross_market_signal: 0.0,
            cross_market_confidence: 0.0,
            latency_arb_signal: 0.0,
            latency_arb_confidence: 0.0,
            stale_options: false,
            stale_whale: false,
            stale_cross_market: false,
            stale_polymarket_flow: false,
            news_sentiment: 0.0,
            news_event_type: "Normal".to_string(),
            vpin_raw: 0.0,
            btc_final_momentum: 0.0,
            btc_final_momentum_confidence: 0.0,
            kyle_lambda: 0.0,
            kyle_lambda_confidence: 0.0,
            kyle_lambda_raw: 0.0,
            hawkes_flow: 0.0,
            hawkes_confidence: 0.0,
        }
    }
}

// ── Main fusion function ─────────────────────────────────────────────────────
#[allow(clippy::too_many_arguments)]
pub fn fuse_signals(
    orderbook: &ObData,
    tick_vel: &TickVelData,
    vwap: &VwapData,
    rsi: &RsiData,
    macd: &MacdData,
    ha: &HaData,
    divergence: &DivergenceData,
    funding: &FundingData,
    regime: &str,
    phase: &PhaseData,
    market_price: f64,
    pcr: Option<&PcrData>,
    fear_greed: Option<&FearGreedData>,
    oi_momentum: Option<&OiMomentumData>,
    aggressor: Option<&AggressorData>,
    lunarcrush: Option<&LunarcrushSentimentData>,
    calibration: Option<&FusionCalibration>,
    weights: Option<HashMap<String, f64>>,
    liquidity: f64,
    trade_amount: f64,
    obi_stable: bool,
    enrichment: &EnrichmentData,
) -> FusionResult {
    let captured = crate::replay::capture_enabled().then(|| {
        serde_json::json!({
            "orderbook": orderbook,
            "tick_vel": tick_vel,
            "vwap": vwap,
            "rsi": rsi,
            "macd": macd,
            "ha": ha,
            "divergence": divergence,
            "funding": funding,
            "regime": regime,
            "phase": phase,
            "market_price": market_price,
            "pcr": pcr,
            "fear_greed": fear_greed,
            "oi_momentum": oi_momentum,
            "aggressor": aggressor,
            "lunarcrush": lunarcrush,
            "calibration": calibration,
            "weights": weights,
            "liquidity": liquidity,
            "trade_amount": trade_amount,
            "obi_stable": obi_stable,
            "enrichment": enrichment,
        })
    });
    let result = fuse_signals_core(FusionInputs {
        orderbook,
        tick_vel,
        vwap,
        rsi,
        macd,
        ha,
        divergence,
        funding,
        regime,
        phase,
        market_price,
        pcr,
        fear_greed,
        oi_momentum,
        aggressor,
        lunarcrush,
        calibration,
        weights,
        liquidity,
        trade_amount,
        obi_stable,
        enrichment,
    });
    if let Some(input) = captured {
        crate::replay::capture("fusion", || input, &result);
    }
    result
}

/// Named inputs keep live and replay calculations aligned without positional arguments.
struct FusionInputs<'a> {
    orderbook: &'a ObData,
    tick_vel: &'a TickVelData,
    vwap: &'a VwapData,
    rsi: &'a RsiData,
    macd: &'a MacdData,
    ha: &'a HaData,
    divergence: &'a DivergenceData,
    funding: &'a FundingData,
    regime: &'a str,
    phase: &'a PhaseData,
    market_price: f64,
    pcr: Option<&'a PcrData>,
    fear_greed: Option<&'a FearGreedData>,
    oi_momentum: Option<&'a OiMomentumData>,
    aggressor: Option<&'a AggressorData>,
    lunarcrush: Option<&'a LunarcrushSentimentData>,
    calibration: Option<&'a FusionCalibration>,
    weights: Option<HashMap<String, f64>>,
    liquidity: f64,
    trade_amount: f64,
    obi_stable: bool,
    enrichment: &'a EnrichmentData,
}

fn fuse_signals_core(inputs: FusionInputs<'_>) -> FusionResult {
    let FusionInputs {
        orderbook,
        tick_vel,
        vwap,
        rsi,
        macd,
        ha,
        divergence,
        funding,
        regime,
        phase,
        market_price,
        pcr,
        fear_greed,
        oi_momentum,
        aggressor,
        lunarcrush,
        calibration,
        weights,
        liquidity,
        trade_amount,
        obi_stable,
        enrichment,
    } = inputs;
    let calib = calibration.copied().unwrap_or_default();
    let w = normalize_weights(weights, obi_stable);
    if !market_price.is_finite()
        || market_price <= 0.0
        || market_price >= 1.0
        || !liquidity.is_finite()
        || liquidity < 0.0
        || !trade_amount.is_finite()
        || trade_amount <= 0.0
    {
        return FusionResult::hold("Invalid market price or execution inputs", market_price, w);
    }
    // Gate 0: range/chop suppression.
    if regime == "CHOP" {
        return FusionResult::hold("CHOP regime — signal quality too low", market_price, w);
    }

    // Gate 0b: VPIN circuit breaker — block entry when informed flow detected.
    if enrichment.vpin_raw > 0.70 {
        return FusionResult::hold(
            &format!(
                "VPIN circuit breaker: informed flow detected (VPIN={:.2})",
                enrichment.vpin_raw
            ),
            market_price,
            w,
        );
    }

    let mkt_p = market_price;
    let profile = phase_profile(&phase.phase);

    let mut features: HashMap<String, Feature> = HashMap::new();

    // Granger causality multipliers — scale reliability of signals with
    // evidence of actual predictive power (vs mere correlation).
    let g_obi = enrichment.granger_orderbook_imbalance;
    let g_tv = enrichment.granger_tick_velocity;
    let g_fr = enrichment.granger_funding_rate;
    let g_agg = enrichment.granger_aggressor_ratio;
    let g_vpin = enrichment.granger_vpin;

    // 1) Orderbook imbalance
    let ob_base = (orderbook.imbalance / 0.35).clamp(-1.0, 1.0);
    let ob_vote = signed_vote(&orderbook.direction, orderbook.confidence);
    let wall_adj = (if orderbook.bid_wall { 0.08 } else { 0.0 })
        - (if orderbook.ask_wall { 0.08 } else { 0.0 });
    let ob_feature = 0.65 * ob_base + 0.25 * ob_vote + wall_adj;
    features.insert(
        "orderbook_imbalance".into(),
        feature_for_phase(
            "orderbook_imbalance",
            &phase.phase,
            ob_feature,
            profile.micro * if obi_stable { 1.0 } else { 0.35 } * g_obi,
        ),
    );

    // 2) Tick velocity
    let vel_component = 0.55 * (tick_vel.velocity_60s / 0.025).tanh()
        + 0.35 * (tick_vel.velocity_30s / 0.020).tanh();
    let accel_component = 0.20 * (tick_vel.acceleration / 0.015).tanh();
    let tick_vote = 0.20 * signed_vote(&tick_vel.direction, tick_vel.confidence);
    features.insert(
        "tick_velocity".into(),
        feature_for_phase(
            "tick_velocity",
            &phase.phase,
            vel_component + accel_component + tick_vote,
            profile.micro
                * if tick_vel.direction == "NEUTRAL" {
                    0.65
                } else {
                    0.95
                }
                * g_tv,
        ),
    );

    // 3) VWAP confluence
    let slope_pct = if vwap.vwap > 0.0 {
        vwap.slope / vwap.vwap
    } else {
        0.0
    };
    let reclaim_penalty = if vwap.failed_reclaim { -0.35 } else { 0.0 };
    features.insert(
        "vwap_confluence".into(),
        feature_for_phase(
            "vwap_confluence",
            &phase.phase,
            vwap.price_vs_vwap * 8.0 + slope_pct * 1200.0 + reclaim_penalty,
            profile.micro * if vwap.failed_reclaim { 0.90 } else { 0.75 },
        ),
    );

    // 4) Price divergence
    let div_vote = signed_vote(&divergence.direction, divergence.confidence);
    let div_strength = divergence.strength.clamp(0.0, 1.0);
    features.insert(
        "price_divergence".into(),
        feature_for_phase(
            "price_divergence",
            &phase.phase,
            div_vote * (0.55 + 0.45 * div_strength),
            profile.momentum * (0.75 + 0.25 * div_strength),
        ),
    );

    // 5) Deribit PCR
    let pcr_feature = if let Some(p) = pcr {
        let pcr_edge = ((p.pcr - 1.0) / 0.45).clamp(-1.0, 1.0);
        let vote = signed_vote(&p.direction, p.confidence);
        feature_for_phase(
            "deribit_pcr",
            &phase.phase,
            0.70 * pcr_edge + 0.30 * vote,
            profile.macro_ * 0.70,
        )
    } else {
        Feature::new(0.0, 0.0)
    };
    features.insert("deribit_pcr".into(), pcr_feature);

    // 7) RSI/MACD/HA composite
    let rsi_comp = ((rsi.rsi - 50.0) / 20.0).clamp(-1.0, 1.0);
    let rsi_slope_comp = (rsi.slope / 4.0).clamp(-1.0, 1.0);
    let macd_comp = 0.70 * (macd.hist / 45.0).tanh() + 0.30 * (macd.hist_delta / 8.0).tanh();
    let ha_comp = (ha.consecutive as f64 / 4.0).clamp(-1.0, 1.0);
    features.insert(
        "rsi_macd_ha".into(),
        feature_for_phase(
            "rsi_macd_ha",
            &phase.phase,
            0.35 * rsi_comp + 0.10 * rsi_slope_comp + 0.40 * macd_comp + 0.15 * ha_comp,
            profile.momentum * 0.62,
        ),
    );

    // 8) Fear & Greed
    let fg_feature = if let Some(fg) = fear_greed {
        let contrarian = ((50.0 - fg.value as f64) / 30.0).clamp(-1.0, 1.0);
        let vote = signed_vote(&fg.direction, fg.confidence);
        feature_for_phase(
            "fear_greed",
            &phase.phase,
            0.75 * contrarian + 0.25 * vote,
            profile.macro_ * 0.65,
        )
    } else {
        Feature::new(0.0, 0.0)
    };
    features.insert("fear_greed".into(), fg_feature);

    // 9) Funding rate
    let funding_feature = if let Some(rate) = funding.rate {
        let contrarian = (-rate / 0.0012).clamp(-1.0, 1.0);
        let vote = signed_vote(&funding.direction, funding.confidence);
        let rel = if rate.abs() < 0.00005 { 0.40 } else { 0.65 };
        feature_for_phase(
            "funding_rate",
            &phase.phase,
            0.70 * contrarian + 0.30 * vote,
            profile.macro_ * rel * g_fr,
        )
    } else {
        Feature::new(0.0, 0.0)
    };
    features.insert("funding_rate".into(), funding_feature);

    // 10) OI momentum (aligns with BTC momentum direction)
    let oi_feature = if let Some(oi) = oi_momentum {
        let delta_strength = (oi.delta_pct.abs() / 1.0).clamp(0.0, 1.0);
        let vote = signed_vote(&oi.direction, oi.confidence);
        let rel = if oi.delta_pct >= 0.0 { 0.70 } else { 0.50 };
        feature_for_phase(
            "oi_momentum",
            &phase.phase,
            vote * (0.55 + 0.45 * delta_strength),
            profile.momentum * rel,
        )
    } else {
        Feature::new(0.0, 0.0)
    };
    features.insert("oi_momentum".into(), oi_feature);

    // 11) Aggressor ratio (buy vs sell initiated volume)
    let aggressor_feature = if let Some(ag) = aggressor {
        let pressure = ((ag.ratio - 0.5) / 0.15).clamp(-1.0, 1.0);
        let vote = signed_vote(&ag.direction, ag.confidence);
        feature_for_phase(
            "aggressor_ratio",
            &phase.phase,
            0.70 * pressure + 0.30 * vote,
            profile.micro * 0.90 * g_agg,
        )
    } else {
        Feature::new(0.0, 0.0)
    };
    features.insert("aggressor_ratio".into(), aggressor_feature);

    // 12) LunarCrush social sentiment
    let lunarcrush_feature = if let Some(lc) = lunarcrush {
        let strength = lc.score.abs().clamp(0.0, 1.0);
        let vote = signed_vote(&lc.direction, lc.confidence);
        feature_for_phase(
            "lunarcrush_sentiment",
            &phase.phase,
            vote * (0.50 + 0.50 * strength),
            profile.macro_ * 0.60,
        )
    } else {
        Feature::new(0.0, 0.0)
    };
    features.insert("lunarcrush_sentiment".into(), lunarcrush_feature);

    // 15) Hour of day — contextual, not directional. Encode as signed for fusion.
    // Peak trading hours (13-21 UTC / US session) get positive, off-hours negative.
    let hour_signed = (enrichment.hour_of_day - 0.5) * 2.0; // center around 0
    features.insert(
        "hour_of_day".into(),
        feature_for_phase("hour_of_day", &phase.phase, hour_signed, 0.5),
    );

    // 16) Day of week — weekends tend to be lower volume/different behavior
    let day_signed = (enrichment.day_of_week - 0.5) * 2.0;
    features.insert(
        "day_of_week".into(),
        feature_for_phase("day_of_week", &phase.phase, day_signed, 0.5),
    );

    // 17) Distance from $1000 round number — near round = more resistance/support
    // Low distance (near round) → strong signal, high distance → weak
    let round_1k = (1.0 - enrichment.distance_round_1000) * 2.0 - 1.0;
    features.insert(
        "distance_from_round_1000".into(),
        feature_for_phase("distance_from_round_1000", &phase.phase, round_1k, 0.6),
    );

    // 18) Distance from $500 round number
    let round_500 = (1.0 - enrichment.distance_round_500) * 2.0 - 1.0;
    features.insert(
        "distance_from_round_500".into(),
        feature_for_phase("distance_from_round_500", &phase.phase, round_500, 0.5),
    );

    // 19) Short-term volatility (10 candles) — high vol → uncertain
    let vol10 = (enrichment.volatility_10 / 0.002).clamp(0.0, 1.0); // normalize
    features.insert(
        "volatility_10".into(),
        feature_for_phase("volatility_10", &phase.phase, vol10 * 2.0 - 1.0, 0.7),
    );

    // 20) Medium-term volatility (30 candles)
    let vol30 = (enrichment.volatility_30 / 0.003).clamp(0.0, 1.0);
    features.insert(
        "volatility_30".into(),
        feature_for_phase("volatility_30", &phase.phase, vol30 * 2.0 - 1.0, 0.7),
    );

    // 21) Window momentum — how much BTC moved since window open
    let mom = (enrichment.window_momentum / 0.5).clamp(-1.0, 1.0); // ±0.5% = full scale
    features.insert(
        "window_momentum".into(),
        feature_for_phase("window_momentum", &phase.phase, mom, profile.momentum * 0.8),
    );

    // 22) Long liquidation volume (log-scaled)
    let long_liq = if enrichment.long_liq_volume > 100.0 {
        (enrichment.long_liq_volume.ln() / 16.0).clamp(0.0, 1.0) // ln(10M)~16
    } else {
        0.0
    };
    // Heavy long liquidations are bearish (longs getting rekt → price dropping)
    features.insert(
        "long_liq_volume".into(),
        feature_for_phase(
            "long_liq_volume",
            &phase.phase,
            -long_liq,
            profile.micro * 0.7,
        ),
    );

    // 23) Short liquidation volume (log-scaled)
    let short_liq = if enrichment.short_liq_volume > 100.0 {
        (enrichment.short_liq_volume.ln() / 16.0).clamp(0.0, 1.0)
    } else {
        0.0
    };
    // Heavy short liquidations are bullish (shorts getting rekt → price pumping)
    features.insert(
        "short_liq_volume".into(),
        feature_for_phase(
            "short_liq_volume",
            &phase.phase,
            short_liq,
            profile.micro * 0.7,
        ),
    );

    // 24) Binance L2 depth imbalance (top-5 levels)
    features.insert(
        "depth_imbalance_5".into(),
        feature_for_phase(
            "depth_imbalance_5",
            &phase.phase,
            enrichment.depth_imbalance_5,
            profile.micro * 0.75,
        ),
    );

    // Staleness gate: if a signal failed to fetch, zero its reliability
    // so it contributes nothing to the posterior (rather than garbage-in).
    let stale_gate = |stale: bool, reliability: f64| -> f64 {
        if stale {
            0.0
        } else {
            reliability
        }
    };

    // 25) Polymarket native flow signal (if provided)
    features.insert(
        "polymarket_flow".into(),
        feature_for_phase(
            "polymarket_flow",
            &phase.phase,
            enrichment.polymarket_flow_signal,
            stale_gate(
                enrichment.stale_polymarket_flow,
                profile.micro * enrichment.polymarket_flow_confidence.clamp(0.0, 1.0),
            ),
        ),
    );

    // 26) VPIN microstructure signal
    features.insert(
        "vpin_microstructure".into(),
        feature_for_phase(
            "vpin_microstructure",
            &phase.phase,
            enrichment.vpin_signal,
            profile.micro * enrichment.vpin_confidence.clamp(0.0, 1.0) * g_vpin,
        ),
    );

    // 27) Options advanced signal (25d skew + max pain)
    features.insert(
        "options_advanced".into(),
        feature_for_phase(
            "options_advanced",
            &phase.phase,
            enrichment.options_signal,
            stale_gate(
                enrichment.stale_options,
                profile.macro_ * enrichment.options_confidence.clamp(0.0, 1.0),
            ),
        ),
    );

    // 28) Whale/on-chain flow signal
    features.insert(
        "whale_flow".into(),
        feature_for_phase(
            "whale_flow",
            &phase.phase,
            enrichment.whale_signal,
            stale_gate(
                enrichment.stale_whale,
                profile.macro_ * enrichment.whale_confidence.clamp(0.0, 1.0),
            ),
        ),
    );

    // 29) Cross-market correlation heuristic; predictive value is unproven
    features.insert(
        "cross_market".into(),
        feature_for_phase(
            "cross_market",
            &phase.phase,
            enrichment.cross_market_signal,
            stale_gate(
                enrichment.stale_cross_market,
                profile.micro * enrichment.cross_market_confidence.clamp(0.0, 1.0),
            ),
        ),
    );

    // 30) Latency arbitrage (Binance leads Polymarket repricing)
    features.insert(
        "latency_arb".into(),
        feature_for_phase(
            "latency_arb",
            &phase.phase,
            enrichment.latency_arb_signal,
            profile.micro * enrichment.latency_arb_confidence.clamp(0.0, 1.0),
        ),
    );

    // 31) News NLP sentiment
    features.insert(
        "news_sentiment".into(),
        feature_for_phase(
            "news_sentiment",
            &phase.phase,
            enrichment.news_sentiment.clamp(-1.0, 1.0),
            profile.macro_ * 0.55,
        ),
    );

    // 32) BTC final-minute momentum (linear extrapolation of last few candles)
    // Uses "momentum" category → profile.momentum, but strongest in LATE phase
    // because confidence already encodes time-proximity weighting.
    features.insert(
        "btc_final_momentum".into(),
        feature_for_phase(
            "btc_final_momentum",
            &phase.phase,
            enrichment.btc_final_momentum,
            profile.momentum * enrichment.btc_final_momentum_confidence.clamp(0.0, 1.0),
        ),
    );

    // 33) Kyle's Lambda — price impact signal (category: micro)
    features.insert(
        "kyle_lambda".into(),
        feature_for_phase(
            "kyle_lambda",
            &phase.phase,
            enrichment.kyle_lambda,
            profile.micro * enrichment.kyle_lambda_confidence.clamp(0.0, 1.0),
        ),
    );

    // 34) Hawkes-style clustered order flow (category: micro)
    features.insert(
        "hawkes_flow".into(),
        feature_for_phase(
            "hawkes_flow",
            &phase.phase,
            enrichment.hawkes_flow,
            profile.micro * enrichment.hawkes_confidence.clamp(0.0, 1.0),
        ),
    );

    // Weighted aggregation + dashboard breakdown.
    let mut weighted_sum = 0.0_f64;
    let mut weighted_abs = 0.0_f64;
    let mut breakdown: HashMap<String, f64> = HashMap::new();
    let mut weighted_contributions: HashMap<String, f64> = HashMap::new();

    for (name, weight) in &w {
        let eff = features.get(name).copied().unwrap_or_default().effective();
        breakdown.insert(name.clone(), eff);
        let contrib = weight * eff;
        weighted_contributions.insert(name.clone(), contrib);
        if *weight <= 0.0 || eff == 0.0 {
            continue;
        }
        weighted_sum += contrib;
        weighted_abs += weight * eff.abs();
    }

    let build_result = |signal: Signal,
                        score: f64,
                        confidence: f64,
                        reason: String,
                        net_ev: f64,
                        taker_fee: f64,
                        lmsr_slippage: f64,
                        p_up: f64| FusionResult {
        signal,
        score,
        confidence,
        reason,
        net_ev,
        taker_fee,
        lmsr_slippage,
        p_up,
        breakdown: breakdown.clone(),
        normalized_weights: w.clone(),
        weighted_contributions: weighted_contributions.clone(),
    };

    if weighted_abs < calib.min_weighted_abs {
        return build_result(
            Signal::Hold,
            0.0,
            0.0,
            "Insufficient directional signal strength".into(),
            0.0,
            estimate_taker_fee(mkt_p),
            0.0,
            mkt_p,
        );
    }

    let prior = logit(mkt_p);
    let posterior_logit = prior + weighted_sum * calib.logit_scale;
    let p_up = sigmoid(posterior_logit).clamp(0.001, 0.999);
    let directional_prob = p_up.max(1.0 - p_up);
    let agreement = if weighted_abs > 0.0 {
        (weighted_sum.abs() / weighted_abs).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let fusion_conf =
        (0.50 + (directional_prob - 0.50) * 1.25 + agreement * 0.12 + weighted_abs.min(1.0) * 0.08)
            .clamp(0.50, 0.97);

    let score = (20.0
        + (directional_prob - 0.50) * 140.0
        + agreement * 30.0
        + weighted_abs.min(1.0) * 35.0)
        .clamp(0.0, 99.0)
        .round();

    let fee_at_mkt = estimate_taker_fee(mkt_p);

    // Gate 1: minimum model quality score.
    if score < calib.min_score {
        return build_result(
            Signal::Hold,
            score,
            fusion_conf,
            format!("score {score:.0} < {:.0}", calib.min_score),
            0.0,
            fee_at_mkt,
            0.0,
            p_up,
        );
    }

    // Gate 2: phase confidence threshold.
    if fusion_conf < phase.conf_threshold {
        return build_result(
            Signal::Hold,
            score,
            fusion_conf,
            format!(
                "conf {:.0}% < phase threshold {:.0}%",
                fusion_conf * 100.0,
                phase.conf_threshold * 100.0
            ),
            0.0,
            fee_at_mkt,
            0.0,
            p_up,
        );
    }

    // Side selection is driven by calibrated mispricing from market prior.
    let yes_edge = p_up - mkt_p;
    let dominant_up = yes_edge >= 0.0;
    let raw_edge = yes_edge.abs();

    // Gate 3: minimum directional edge (phase-aware floor).
    let min_raw_edge = (phase.edge_threshold * 0.45).max(0.015);
    if raw_edge < min_raw_edge {
        return build_result(
            Signal::Hold,
            score,
            fusion_conf,
            format!(
                "raw_edge {:.2}% < phase min {:.2}%",
                raw_edge * 100.0,
                min_raw_edge * 100.0
            ),
            0.0,
            fee_at_mkt,
            0.0,
            p_up,
        );
    }

    // Gate 4: net EV after dynamic fee + slippage.
    let (net_ev, taker_fee, lmsr_slip) =
        entry_economics(p_up, mkt_p, dominant_up, trade_amount, liquidity)
            .expect("fusion inputs were validated before economic evaluation");
    if net_ev < calib.min_net_ev {
        return build_result(
            Signal::Hold,
            score,
            fusion_conf,
            format!(
                "net_ev {:.3}% < min {:.1}% (fee={:.3}% slip={:.3}%)",
                net_ev * 100.0,
                calib.min_net_ev * 100.0,
                taker_fee * 100.0,
                lmsr_slip * 100.0
            ),
            net_ev,
            taker_fee,
            lmsr_slip,
            p_up,
        );
    }

    // Gate 5: center fee-zone filter.
    let entry_price = if dominant_up { mkt_p } else { 1.0 - mkt_p };
    if (mkt_p - 0.5).abs() < calib.fee_skip_band && raw_edge / entry_price < taker_fee * 1.5 {
        return build_result(
            Signal::Hold,
            score,
            fusion_conf,
            format!(
                "fee zone: price={:.2} within ±{:.0}% of 0.50, edge too thin",
                mkt_p,
                calib.fee_skip_band * 100.0
            ),
            net_ev,
            taker_fee,
            lmsr_slip,
            p_up,
        );
    }

    let signal = if dominant_up {
        Signal::BuyYes
    } else {
        Signal::BuyNo
    };
    let reason = format!(
        "{} | p_up={:.1}% score={:.0} edge={:.2}% net_ev={:.2}% fee={:.2}% agree={:.0}% phase={}",
        signal.as_str(),
        p_up * 100.0,
        score,
        raw_edge * 100.0,
        net_ev * 100.0,
        taker_fee * 100.0,
        agreement * 100.0,
        phase.phase
    );

    build_result(
        signal,
        score,
        fusion_conf,
        reason,
        net_ev,
        taker_fee,
        lmsr_slip,
        p_up,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permissive_calibration() -> FusionCalibration {
        FusionCalibration {
            min_score: 0.0,
            fee_skip_band: 0.0,
            min_net_ev: 0.0,
            min_weighted_abs: 0.0,
            ..FusionCalibration::default()
        }
    }

    fn isolated_weight(target: &str) -> HashMap<String, f64> {
        let mut weights = default_weights();
        for value in weights.values_mut() {
            *value = 0.0;
        }
        weights.insert(target.to_string(), 1.0);
        weights
    }

    fn phase_named(phase: &str) -> PhaseData {
        match phase {
            "EARLY" => PhaseData {
                phase: "EARLY".into(),
                edge_threshold: 0.05,
                conf_threshold: 0.55,
            },
            "LATE" => PhaseData {
                phase: "LATE".into(),
                edge_threshold: 0.20,
                conf_threshold: 0.65,
            },
            _ => PhaseData::default(),
        }
    }

    fn fuse_for_phase_test(
        phase: &str,
        weights: HashMap<String, f64>,
        enrichment: &EnrichmentData,
    ) -> FusionResult {
        let phase = phase_named(phase);
        fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "TREND_UP",
            &phase,
            0.50,
            None,
            None,
            None,
            None,
            None,
            Some(&permissive_calibration()),
            Some(weights),
            1_000_000.0,
            1.0,
            true,
            enrichment,
        )
    }

    #[test]
    fn taker_fee_peaks_at_mid() {
        let lo = estimate_taker_fee(0.10);
        let hi = estimate_taker_fee(0.90);
        let mid = estimate_taker_fee(0.50);
        assert!(mid > lo);
        assert!(mid > hi);
    }

    #[test]
    fn economics_use_selected_token_and_return_per_dollar_not_probability_points() {
        let (net, fee, impact) = entry_economics(0.7, 0.8, false, 10.0, 1000.0).unwrap();
        assert!((net - (0.3 / 0.2 - 1.0 - fee - impact)).abs() < 1e-12);
        assert!(net > 0.4);
        let (wrong_side, _, _) = entry_economics(0.7, 0.8, true, 10.0, 1000.0).unwrap();
        assert!(wrong_side < 0.0);
        for price in [0.0, 1.0, f64::NAN, f64::INFINITY] {
            assert!(entry_economics(0.7, price, true, 10.0, 1000.0).is_none());
        }
        assert!(entry_economics(f64::NAN, 0.5, true, 10.0, 1000.0).is_none());
    }

    #[test]
    fn malformed_features_and_weights_cannot_poison_fusion() {
        let feature = Feature::new(f64::NAN, 1.0);
        assert_eq!(feature.effective(), 0.0);
        let weights = normalize_weights(
            Some(HashMap::from([(
                "orderbook_imbalance".into(),
                f64::INFINITY,
            )])),
            true,
        );
        assert!(weights.values().all(|value| value.is_finite()));
        assert!((weights.values().sum::<f64>() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn weight_normalization_is_stable() {
        let mut raw = HashMap::new();
        raw.insert("orderbook_imbalance".to_string(), 2.0);
        raw.insert("tick_velocity".to_string(), 2.0);
        let w = normalize_weights(Some(raw), true);
        let sum: f64 = w.values().sum();
        assert!((sum - 1.0).abs() < 1e-9);
    }

    #[test]
    fn time_of_day_weighting_boosts_microstructure_during_us_hours() {
        let mut weights = HashMap::new();
        weights.insert("orderbook_imbalance".to_string(), 1.0);
        weights.insert("fear_greed".to_string(), 1.0);
        weights.insert("vwap_confluence".to_string(), 1.0);

        apply_time_of_day_weighting(&mut weights, 18);

        assert!((weights["orderbook_imbalance"] - 1.18).abs() < 1e-9);
        assert!((weights["fear_greed"] - 1.10).abs() < 1e-9);
        assert!((weights["vwap_confluence"] - 1.06).abs() < 1e-9);
    }

    #[test]
    fn time_of_day_weighting_seeds_missing_defaults_before_scaling() {
        let mut weights = HashMap::new();
        weights.insert("fear_greed".to_string(), 0.5);

        apply_time_of_day_weighting(&mut weights, 2);

        assert!((weights["fear_greed"] - 0.45).abs() < 1e-9);
        assert!((weights["tick_velocity"] - (0.1724 * 0.82)).abs() < 1e-9);
        assert!((weights["vwap_confluence"] - (0.1207 * 0.92)).abs() < 1e-9);
    }

    #[test]
    fn fusion_result_exposes_normalized_weights_and_contributions() {
        let mut enrichment = EnrichmentData::default();
        enrichment.cross_market_signal = 0.65;
        enrichment.cross_market_confidence = 1.0;

        let out = fuse_for_phase_test("MID", isolated_weight("cross_market"), &enrichment);

        assert!(out.normalized_weights.contains_key("cross_market"));
        assert!(out.weighted_contributions.contains_key("cross_market"));
        assert!(out.weighted_contributions["cross_market"] > 0.0);
        assert_eq!(out.weighted_contributions["news_sentiment"], 0.0);
    }

    #[test]
    fn chop_regime_forces_hold() {
        let out = fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "CHOP",
            &PhaseData::default(),
            0.5,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            5_000.0,
            5.0,
            true,
            &EnrichmentData::default(),
        );
        assert_eq!(out.signal, Signal::Hold);
    }

    #[test]
    fn btc_final_momentum_moves_fusion_score_when_weighted_in() {
        let weights = isolated_weight("btc_final_momentum");

        let baseline = fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "TREND_UP",
            &PhaseData::default(),
            0.20,
            None,
            None,
            None,
            None,
            None,
            Some(&permissive_calibration()),
            Some(weights.clone()),
            1_000_000.0,
            1.0,
            true,
            &EnrichmentData::default(),
        );

        let mut enrichment = EnrichmentData::default();
        enrichment.btc_final_momentum = 0.90;
        enrichment.btc_final_momentum_confidence = 1.0;

        let enriched = fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "TREND_UP",
            &PhaseData::default(),
            0.20,
            None,
            None,
            None,
            None,
            None,
            Some(&permissive_calibration()),
            Some(weights),
            1_000_000.0,
            1.0,
            true,
            &enrichment,
        );

        assert_eq!(baseline.breakdown["btc_final_momentum"], 0.0);
        assert!(enriched.breakdown["btc_final_momentum"] > 0.0);
        assert!(enriched.p_up > baseline.p_up);
        assert!(enriched.score > baseline.score);
        assert_ne!(enriched.signal, Signal::Hold);
    }

    #[test]
    fn kyle_lambda_moves_fusion_score_when_weighted_in() {
        let weights = isolated_weight("kyle_lambda");

        let baseline = fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "TREND_UP",
            &PhaseData::default(),
            0.20,
            None,
            None,
            None,
            None,
            None,
            Some(&permissive_calibration()),
            Some(weights.clone()),
            1_000_000.0,
            1.0,
            true,
            &EnrichmentData::default(),
        );

        let mut enrichment = EnrichmentData::default();
        enrichment.kyle_lambda = 0.85;
        enrichment.kyle_lambda_confidence = 1.0;

        let enriched = fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "TREND_UP",
            &PhaseData::default(),
            0.20,
            None,
            None,
            None,
            None,
            None,
            Some(&permissive_calibration()),
            Some(weights),
            1_000_000.0,
            1.0,
            true,
            &enrichment,
        );

        assert_eq!(baseline.breakdown["kyle_lambda"], 0.0);
        assert!(enriched.breakdown["kyle_lambda"] > 0.0);
        assert!(enriched.p_up > baseline.p_up);
        assert!(enriched.score > baseline.score);
        assert_ne!(enriched.signal, Signal::Hold);
    }

    #[test]
    fn hawkes_flow_moves_fusion_score_when_weighted_in() {
        let weights = isolated_weight("hawkes_flow");

        let baseline = fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "TREND_UP",
            &PhaseData::default(),
            0.20,
            None,
            None,
            None,
            None,
            None,
            Some(&permissive_calibration()),
            Some(weights.clone()),
            1_000_000.0,
            1.0,
            true,
            &EnrichmentData::default(),
        );

        let mut enrichment = EnrichmentData::default();
        enrichment.hawkes_flow = 0.75;
        enrichment.hawkes_confidence = 1.0;

        let enriched = fuse_signals(
            &ObData::default(),
            &TickVelData::default(),
            &VwapData::default(),
            &RsiData::default(),
            &MacdData::default(),
            &HaData::default(),
            &DivergenceData::default(),
            &FundingData::default(),
            "TREND_UP",
            &PhaseData::default(),
            0.20,
            None,
            None,
            None,
            None,
            None,
            Some(&permissive_calibration()),
            Some(weights),
            1_000_000.0,
            1.0,
            true,
            &enrichment,
        );

        assert_eq!(baseline.breakdown["hawkes_flow"], 0.0);
        assert!(enriched.breakdown["hawkes_flow"] > 0.0);
        assert!(enriched.p_up > baseline.p_up);
        assert!(enriched.score > baseline.score);
        assert_ne!(enriched.signal, Signal::Hold);
    }

    #[test]
    fn btc_final_momentum_is_phase_weighted_toward_late_window() {
        let mut enrichment = EnrichmentData::default();
        enrichment.btc_final_momentum = 0.80;
        enrichment.btc_final_momentum_confidence = 0.70;

        let early =
            fuse_for_phase_test("EARLY", isolated_weight("btc_final_momentum"), &enrichment);
        let mid = fuse_for_phase_test("MID", isolated_weight("btc_final_momentum"), &enrichment);
        let late = fuse_for_phase_test("LATE", isolated_weight("btc_final_momentum"), &enrichment);

        assert!(early.breakdown["btc_final_momentum"] < mid.breakdown["btc_final_momentum"]);
        assert!(mid.breakdown["btc_final_momentum"] < late.breakdown["btc_final_momentum"]);
        assert!(early.p_up < mid.p_up);
        assert!(mid.p_up < late.p_up);
    }

    #[test]
    fn cross_market_signal_decays_as_market_moves_late() {
        let mut enrichment = EnrichmentData::default();
        enrichment.cross_market_signal = 0.80;
        enrichment.cross_market_confidence = 0.85;

        let early = fuse_for_phase_test("EARLY", isolated_weight("cross_market"), &enrichment);
        let mid = fuse_for_phase_test("MID", isolated_weight("cross_market"), &enrichment);
        let late = fuse_for_phase_test("LATE", isolated_weight("cross_market"), &enrichment);

        assert!(late.breakdown["cross_market"] < mid.breakdown["cross_market"]);
        assert!(mid.breakdown["cross_market"] < early.breakdown["cross_market"]);
        assert!(late.p_up < mid.p_up);
        assert!(mid.p_up < early.p_up);
    }
}
