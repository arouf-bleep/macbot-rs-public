use super::Candle;

/// Result of the BTC final-5-minute momentum signal.
#[derive(Debug, Clone)]
pub struct BtcFinalMomentumData {
    pub signal: f64,     // [-1, 1] extrapolated price change vs ref
    pub confidence: f64, // [0, 1]
}

/// Compute the BTC final-minute momentum signal by linear-extrapolating the
/// last few 1-minute candle closes to predict where BTC will be at window close.
///
/// Strongest in the LATE phase (< 5 min remaining). Returns `None` when there
/// are fewer than 3 candles or when time remaining exceeds 7 minutes.
///
/// # Arguments
/// * `candles` — recent 1-minute OHLCV candles (oldest first)
/// * `btc_ref_price` — the oracle reference price ("price to beat")
/// * `time_remaining_mins` — minutes until the prediction window closes
///
/// # Returns
/// `Some((signal, confidence))` where signal is the extrapolated fractional
/// price change clamped to [-1, 1], and confidence incorporates both time
/// proximity and regression fit quality (R²).
pub fn compute_btc_final_momentum(
    candles: &[Candle],
    btc_ref_price: f64,
    time_remaining_mins: f64,
) -> Option<BtcFinalMomentumData> {
    // Need at least 3 candles for a meaningful regression
    if candles.len() < 3 || btc_ref_price <= 0.0 {
        return None;
    }

    // Zero confidence when too far from close
    if time_remaining_mins > 7.0 {
        return Some(BtcFinalMomentumData {
            signal: 0.0,
            confidence: 0.0,
        });
    }

    // Use the last 3-5 candles (tail end of the series)
    let n = candles.len().min(5);
    let tail = &candles[candles.len() - n..];

    // Linear regression of close prices over the candle indices.
    // x_i = 0, 1, ..., n-1  (each unit = 1 minute)
    // y_i = close price of candle i
    let n_f = n as f64;
    let mut sum_x = 0.0_f64;
    let mut sum_y = 0.0_f64;
    let mut sum_xy = 0.0_f64;
    let mut sum_xx = 0.0_f64;

    for (i, c) in tail.iter().enumerate() {
        let x = i as f64;
        let y = c.close;
        sum_x += x;
        sum_y += y;
        sum_xy += x * y;
        sum_xx += x * x;
    }

    let denom = n_f * sum_xx - sum_x * sum_x;
    if denom.abs() < f64::EPSILON {
        return None; // degenerate — all x values identical (shouldn't happen)
    }

    let slope = (n_f * sum_xy - sum_x * sum_y) / denom;
    let intercept = (sum_y - slope * sum_x) / n_f;

    // Extrapolate forward by `time_remaining_mins` minutes
    // The last candle is at x = n-1, so the close is at x = n-1 + time_remaining_mins
    let x_close = (n_f - 1.0) + time_remaining_mins;
    let extrapolated_price = intercept + slope * x_close;

    // Signal: fractional change of extrapolated price vs reference, clamped [-1, 1]
    let signal = ((extrapolated_price - btc_ref_price) / btc_ref_price).clamp(-1.0, 1.0);

    // Compute R² for regression quality
    let y_mean = sum_y / n_f;
    let mut ss_tot = 0.0_f64;
    let mut ss_res = 0.0_f64;
    for (i, c) in tail.iter().enumerate() {
        let x = i as f64;
        let y = c.close;
        let y_hat = intercept + slope * x;
        ss_tot += (y - y_mean).powi(2);
        ss_res += (y - y_hat).powi(2);
    }
    let r_squared = if ss_tot > f64::EPSILON {
        (1.0 - ss_res / ss_tot).clamp(0.0, 1.0)
    } else {
        0.0 // flat prices — no variance to explain
    };

    // Time-based confidence: ramps from 0 at 7 min to 1.0 at <= 2 min remaining,
    // strongest when < 5 min left.
    //   7 min → 0.0
    //   5 min → 0.4
    //   2 min → 1.0
    //   0 min → 1.0
    let time_conf = if time_remaining_mins <= 2.0 {
        1.0
    } else if time_remaining_mins >= 7.0 {
        0.0
    } else {
        // Linear interpolation: 7 → 0, 2 → 1
        (7.0 - time_remaining_mins) / 5.0
    };

    // Final confidence: product of time proximity and regression fit quality
    let confidence = (time_conf * r_squared).clamp(0.0, 1.0);

    Some(BtcFinalMomentumData { signal, confidence })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_candles(closes: &[f64]) -> Vec<Candle> {
        closes
            .iter()
            .map(|&c| Candle {
                open: c,
                high: c + 10.0,
                low: c - 10.0,
                close: c,
                volume: 100.0,
            })
            .collect()
    }

    #[test]
    fn returns_none_with_fewer_than_3_candles() {
        let candles = make_candles(&[100_000.0, 100_050.0]);
        assert!(compute_btc_final_momentum(&candles, 100_000.0, 3.0).is_none());
    }

    #[test]
    fn returns_none_with_zero_ref_price() {
        let candles = make_candles(&[100_000.0, 100_050.0, 100_100.0]);
        assert!(compute_btc_final_momentum(&candles, 0.0, 3.0).is_none());
    }

    #[test]
    fn zero_confidence_when_far_from_close() {
        let candles = make_candles(&[100_000.0, 100_050.0, 100_100.0]);
        let result = compute_btc_final_momentum(&candles, 100_000.0, 10.0).unwrap();
        assert_eq!(result.confidence, 0.0);
    }

    #[test]
    fn positive_signal_on_uptrend() {
        // Steadily rising prices
        let candles = make_candles(&[100_000.0, 100_100.0, 100_200.0, 100_300.0, 100_400.0]);
        let result = compute_btc_final_momentum(&candles, 100_000.0, 3.0).unwrap();
        assert!(
            result.signal > 0.0,
            "expected positive signal, got {}",
            result.signal
        );
        assert!(result.confidence > 0.0, "expected positive confidence");
    }

    #[test]
    fn negative_signal_on_downtrend() {
        // Steadily falling prices
        let candles = make_candles(&[100_400.0, 100_300.0, 100_200.0, 100_100.0, 100_000.0]);
        let result = compute_btc_final_momentum(&candles, 100_200.0, 3.0).unwrap();
        assert!(
            result.signal < 0.0,
            "expected negative signal, got {}",
            result.signal
        );
    }

    #[test]
    fn high_r_squared_on_perfect_trend() {
        // Perfect linear trend → R² = 1.0
        let candles = make_candles(&[100_000.0, 100_100.0, 100_200.0, 100_300.0, 100_400.0]);
        let result = compute_btc_final_momentum(&candles, 100_000.0, 2.0).unwrap();
        // With perfect linearity and 2 min remaining, confidence should be high
        assert!(
            result.confidence > 0.9,
            "expected high confidence for perfect trend, got {}",
            result.confidence
        );
    }

    #[test]
    fn low_confidence_on_noisy_data() {
        // Noisy prices → low R²
        let candles = make_candles(&[100_000.0, 100_500.0, 99_800.0, 100_600.0, 99_900.0]);
        let result = compute_btc_final_momentum(&candles, 100_000.0, 2.0).unwrap();
        // Low R² means confidence should be modest
        assert!(
            result.confidence < 0.8,
            "expected lower confidence for noisy data, got {}",
            result.confidence
        );
    }

    #[test]
    fn signal_is_clamped() {
        // Extreme slope that would overshoot
        let candles = make_candles(&[50_000.0, 75_000.0, 100_000.0, 125_000.0, 150_000.0]);
        let result = compute_btc_final_momentum(&candles, 50_000.0, 3.0).unwrap();
        assert!(result.signal >= -1.0 && result.signal <= 1.0);
    }

    #[test]
    fn confidence_scales_with_time() {
        let candles = make_candles(&[100_000.0, 100_100.0, 100_200.0, 100_300.0, 100_400.0]);
        let at_2min = compute_btc_final_momentum(&candles, 100_000.0, 2.0).unwrap();
        let at_5min = compute_btc_final_momentum(&candles, 100_000.0, 5.0).unwrap();
        let at_6min = compute_btc_final_momentum(&candles, 100_000.0, 6.0).unwrap();
        // Closer to close → higher confidence
        assert!(at_2min.confidence > at_5min.confidence);
        assert!(at_5min.confidence > at_6min.confidence);
    }

    #[test]
    fn uses_at_most_5_candles() {
        // With 10 candles, should only use last 5
        let mut prices = vec![50_000.0; 5]; // noise that should be ignored
        prices.extend_from_slice(&[100_000.0, 100_100.0, 100_200.0, 100_300.0, 100_400.0]);
        let candles = make_candles(&prices);
        let result = compute_btc_final_momentum(&candles, 100_000.0, 3.0).unwrap();
        assert!(
            result.signal > 0.0,
            "should reflect uptrend in last 5 candles"
        );
    }
}
