//! Polymarket-native order flow features.
//! Tracks volume velocity, price velocity, large orders, and YES/NO ratio.

#[derive(Debug, Clone, Default)]
pub struct PolymarketFlowData {
    /// Volume velocity: contracts/min over last 5 minutes
    pub volume_velocity: f64,
    /// Price velocity: price change rate (cents/min)
    pub price_velocity: f64,
    /// True if any recent fill was > $500 (large order detected)
    pub large_order_detected: bool,
    /// YES volume / (YES + NO volume) over last 5 min, range [0, 1]
    pub yes_no_ratio: f64,
    /// Raw signal in [-1, 1]: positive = bullish flow, negative = bearish flow
    pub signal: f64,
    /// Confidence [0, 1]
    pub confidence: f64,
}

/// Compute Polymarket-native flow signal from raw trade data.
///
/// # Parameters
/// - `yes_volume`: total YES token volume (contracts) in last 5 min
/// - `no_volume`: total NO token volume (contracts) in last 5 min
/// - `price_start`: YES token price 5 min ago (e.g. 0.50)
/// - `price_now`: YES token price now (e.g. 0.52)
/// - `large_fill_detected`: whether any fill > $500 was seen
/// - `elapsed_mins`: elapsed minutes (to compute velocities)
pub fn compute_polymarket_flow(
    yes_volume: f64,
    no_volume: f64,
    price_start: f64,
    price_now: f64,
    large_fill_detected: bool,
    elapsed_mins: f64,
) -> PolymarketFlowData {
    let elapsed = elapsed_mins.max(0.1);
    let total_volume = yes_volume + no_volume;

    let volume_velocity = total_volume / elapsed;
    let price_velocity = if elapsed > 0.0 {
        (price_now - price_start) / elapsed * 100.0 // cents/min
    } else {
        0.0
    };

    let yes_no_ratio = if total_volume > 0.0 {
        yes_volume / total_volume
    } else {
        0.5
    };

    // Signal: YES ratio > 0.5 is bullish, large orders amplify signal
    let ratio_signal = ((yes_no_ratio - 0.5) * 4.0).clamp(-1.0, 1.0);
    let velocity_signal = (price_velocity / 5.0).clamp(-1.0, 1.0); // ±5 cents/min = full scale
    let large_order_boost = if large_fill_detected { 0.15 } else { 0.0 };

    let raw_signal = 0.5 * ratio_signal + 0.3 * velocity_signal + large_order_boost;
    let signal = raw_signal.clamp(-1.0, 1.0);

    // Confidence scales with volume velocity (more volume = more reliable)
    let confidence = (volume_velocity / 1000.0).clamp(0.0, 1.0);

    PolymarketFlowData {
        volume_velocity,
        price_velocity,
        large_order_detected: large_fill_detected,
        yes_no_ratio,
        signal,
        confidence,
    }
}
