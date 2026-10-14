//! Single-expiry options research heuristics: measured IV skew and max-pain proxy.
//! Public open interest cannot identify dealer long/short positions. Signed gamma
//! exposure is unavailable and does not contribute an invented directional bias.

#[derive(Debug, Clone)]
pub struct OptionsAdvancedData {
    /// (near-25-delta put IV - near-25-delta call IV) / same-expiry near-ATM IV.
    pub skew_25d: f64,
    /// Strike minimizing OI-weighted USD intrinsic value for one expiry.
    /// A research proxy, not a prediction that prices must move to this level.
    pub max_pain_price: f64,
    pub signal: f64,
    pub confidence: f64,
}

/// Equal-weight, bounded skew/max-pain heuristic. No profitability is implied.
/// Unavailable/invalid input remains unavailable, rather than measured neutral.
pub fn compute_options_advanced(
    skew_25d: f64,
    current_price: f64,
    max_pain: f64,
) -> Option<OptionsAdvancedData> {
    if !skew_25d.is_finite()
        || !current_price.is_finite()
        || current_price <= 0.0
        || !max_pain.is_finite()
        || max_pain <= 0.0
    {
        return None;
    }
    let skew_signal = (-skew_25d / 0.20).clamp(-1.0, 1.0);
    let deviation = (max_pain - current_price) / current_price;
    if !deviation.is_finite() {
        return None;
    }
    let max_pain_signal = (deviation / 0.02).clamp(-1.0, 1.0);
    let signal = (0.5 * skew_signal + 0.5 * max_pain_signal).clamp(-1.0, 1.0);
    let confidence = (0.5 * (skew_25d.abs() / 0.10).clamp(0.0, 1.0)
        + 0.5 * (deviation.abs() / 0.02).clamp(0.0, 1.0))
    .clamp(0.0, 1.0);
    Some(OptionsAdvancedData {
        skew_25d,
        max_pain_price: max_pain,
        signal,
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_non_finite_inputs_are_unavailable() {
        for (skew, spot, pain) in [
            (f64::NAN, 100.0, 100.0),
            (0.0, 0.0, 100.0),
            (0.0, 100.0, 0.0),
            (0.0, f64::INFINITY, 100.0),
            (0.0, 100.0, f64::INFINITY),
        ] {
            assert!(compute_options_advanced(skew, spot, pain).is_none());
        }
    }
    #[test]
    fn measured_neutral_has_no_invented_gamma_bias() {
        let measured = compute_options_advanced(0.0, 100.0, 100.0).unwrap();
        assert_eq!(measured.signal, 0.0);
        assert_eq!(measured.confidence, 0.0);
    }
    #[test]
    fn directional_heuristics_are_symmetric_and_bounded() {
        let up = compute_options_advanced(-0.2, 100.0, 102.0).unwrap();
        let down = compute_options_advanced(0.2, 100.0, 98.0).unwrap();
        assert_eq!(up.signal, 1.0);
        assert_eq!(down.signal, -1.0);
        assert_eq!(up.confidence, 1.0);
        assert_eq!(down.confidence, 1.0);
    }
}
