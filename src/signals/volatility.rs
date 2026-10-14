/// Compute standard deviation of 1-minute candle returns over the last `window` candles.
/// Returns 0.0 if not enough data.
pub fn compute_recent_volatility(closes: &[f64], window: usize) -> f64 {
    if window == 0 || window >= closes.len() {
        return 0.0;
    }
    let start = closes.len() - window - 1;
    let tail = &closes[start..];
    if tail.iter().any(|p| !p.is_finite() || *p <= 0.0) {
        return 0.0;
    }
    let returns: Vec<f64> = tail.windows(2).map(|w| (w[1] - w[0]) / w[0]).collect();

    if returns.is_empty() {
        return 0.0;
    }

    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
    variance.sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_return_participates_in_the_requested_window() {
        // The two returns are 0% and 20%, so population deviation is 10%.
        assert!((compute_recent_volatility(&[100.0, 100.0, 120.0], 2) - 0.1).abs() < 1e-12);
        assert_eq!(compute_recent_volatility(&[100.0, 120.0], 1), 0.0);
    }

    #[test]
    fn invalid_prices_and_windows_do_not_produce_nan_or_panic() {
        for window in [0, usize::MAX] {
            assert_eq!(compute_recent_volatility(&[100.0, 110.0], window), 0.0);
        }
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(compute_recent_volatility(&[100.0, bad, 120.0], 2), 0.0);
        }
    }
}
