#![allow(dead_code, unused_imports)]
//! Tick-level microstructure signals: VPIN, trade imbalance, VWAP deviation.
//!
//! VPIN = Volume-synchronized Probability of Informed Trading.
//! High VPIN (> 0.5) means informed traders are dominating — expect moves.

#[derive(Debug, Clone, Default)]
pub struct MicrostructureData {
    /// VPIN in [0, 1] — high = informed trading = volatile/directional move expected
    pub vpin: f64,
    /// Buy-sell imbalance in last 30s: positive = buy pressure, range [-1, 1]
    pub trade_imbalance_30s: f64,
    /// VWAP deviation: (price - vwap) / vwap, range roughly [-0.01, 0.01]
    pub vwap_deviation: f64,
    /// Combined signal in [-1, 1]
    pub signal: f64,
    /// Confidence [0, 1]
    pub confidence: f64,
}

/// Compute VPIN from a series of (volume, is_buy) trade buckets.
///
/// Classic VPIN: divide cumulative volume into equal buckets V,
/// within each bucket classify buy vs sell volume, compute |V_buy - V_sell| / V.
/// Then VPIN = rolling average over last 50 buckets.
///
/// Here we use a simplified streaming version:
/// - `trades`: Vec of (volume, is_buyer_maker) — false = buy initiated
/// - `bucket_size`: target volume per bucket (default 100.0 BTC-contracts)
pub fn compute_vpin(trades: &[(f64, bool)], bucket_size: f64) -> f64 {
    if trades.is_empty() {
        return 0.0;
    }
    let bucket_size = bucket_size.max(1.0);

    let mut vpin_buckets: Vec<f64> = Vec::new();
    let mut current_buy = 0.0_f64;
    let mut current_total = 0.0_f64;

    for &(vol, is_maker) in trades {
        // is_buyer_maker = true means market sell (maker was buyer, taker was seller)
        // So: sell volume if is_buyer_maker, buy volume if !is_buyer_maker
        if !is_maker {
            current_buy += vol;
        }
        current_total += vol;

        if current_total >= bucket_size {
            let imbalance = (current_buy - (current_total - current_buy)).abs() / current_total;
            vpin_buckets.push(imbalance.clamp(0.0, 1.0));
            current_buy = 0.0;
            current_total = 0.0;
        }
    }

    if vpin_buckets.is_empty() {
        // Not enough volume for a full bucket — estimate from partial bucket
        if current_total > 0.0 {
            return ((current_buy - (current_total - current_buy)).abs() / current_total)
                .clamp(0.0, 1.0);
        }
        return 0.0;
    }

    // Rolling average over last 50 buckets
    let n = vpin_buckets.len().min(50);
    vpin_buckets[vpin_buckets.len() - n..].iter().sum::<f64>() / n as f64
}

/// Compute full microstructure signal from recent trade data.
///
/// # Parameters
/// - `trades_30s`: (volume, is_buyer_maker) trades in last 30 seconds
/// - `trades_all`: all recent trades for VPIN computation
/// - `current_price`: latest BTC price
/// - `vwap`: volume-weighted average price
/// - `bucket_size`: VPIN bucket size in BTC contracts
pub fn compute_microstructure(
    trades_30s: &[(f64, bool)],
    trades_all: &[(f64, bool)],
    current_price: f64,
    vwap: f64,
    bucket_size: f64,
) -> MicrostructureData {
    // VPIN
    let vpin = compute_vpin(trades_all, bucket_size);

    // 30s trade imbalance
    let (buy_30s, total_30s) = trades_30s
        .iter()
        .fold((0.0, 0.0), |(b, t), &(vol, is_maker)| {
            if !is_maker {
                (b + vol, t + vol)
            } else {
                (b, t + vol)
            }
        });
    let trade_imbalance_30s = if total_30s > 0.0 {
        ((buy_30s - (total_30s - buy_30s)) / total_30s).clamp(-1.0, 1.0)
    } else {
        0.0
    };

    // VWAP deviation
    let vwap_deviation = if vwap > 0.0 {
        (current_price - vwap) / vwap
    } else {
        0.0
    };

    // Signal: high VPIN = expect directional move (but which way? use imbalance for direction)
    let vpin_strength = ((vpin - 0.3) / 0.4).clamp(0.0, 1.0); // VPIN > 0.7 = max signal
    let direction_signal = trade_imbalance_30s; // [-1, 1]
    let vwap_signal = (vwap_deviation / 0.001).clamp(-1.0, 1.0); // ±0.1% = full scale

    let signal = (vpin_strength * direction_signal * 0.6 + vwap_signal * 0.4).clamp(-1.0, 1.0);
    let confidence = vpin_strength * 0.7 + (trade_imbalance_30s.abs() * 0.3);

    MicrostructureData {
        vpin,
        trade_imbalance_30s,
        vwap_deviation,
        signal,
        confidence: confidence.clamp(0.0, 1.0),
    }
}
