#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AggressorData {
    pub direction: String,
    pub confidence: f64,
    pub ratio: f64,
    pub buy_vol: f64,
    pub sell_vol: f64,
}

/// Aggressor ratio from recent Binance aggTrades.
/// Ratio > 0.5 means buy-initiated volume dominates.
pub fn compute_aggressor_ratio(buy_vol: f64, sell_vol: f64) -> AggressorData {
    let total = buy_vol + sell_vol;
    if total <= 0.0 {
        return AggressorData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            ratio: 0.5,
            buy_vol,
            sell_vol,
        };
    }

    let ratio = (buy_vol / total).clamp(0.0, 1.0);
    let pressure = ratio - 0.5; // [-0.5, 0.5]
    let strength = (pressure.abs() / 0.15).clamp(0.0, 1.0);

    if strength < 0.20 {
        AggressorData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            ratio,
            buy_vol,
            sell_vol,
        }
    } else {
        let direction = if pressure >= 0.0 { "UP" } else { "DOWN" };
        let conf = (0.52 + strength * 0.18).min(0.72);
        AggressorData {
            direction: direction.into(),
            confidence: conf,
            ratio,
            buy_vol,
            sell_vol,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compute_aggressor_ratio;

    #[test]
    fn zero_or_negative_total_volume_returns_neutral_defaults() {
        let neutral = compute_aggressor_ratio(0.0, 0.0);
        assert_eq!(neutral.direction, "NEUTRAL");
        assert_eq!(neutral.confidence, 0.5);
        assert_eq!(neutral.ratio, 0.5);
        assert_eq!(neutral.buy_vol, 0.0);
        assert_eq!(neutral.sell_vol, 0.0);

        let negative_total = compute_aggressor_ratio(-0.25, 0.10);
        assert_eq!(negative_total.direction, "NEUTRAL");
        assert_eq!(negative_total.confidence, 0.5);
        assert_eq!(negative_total.ratio, 0.5);
        assert_eq!(negative_total.buy_vol, -0.25);
        assert_eq!(negative_total.sell_vol, 0.10);
    }

    #[test]
    fn small_imbalance_stays_neutral_inside_deadband() {
        let data = compute_aggressor_ratio(51.0, 49.0);
        assert_eq!(data.direction, "NEUTRAL");
        assert_eq!(data.confidence, 0.5);
        assert!((data.ratio - 0.51).abs() < 1e-12);
    }

    #[test]
    fn crossing_deadband_sets_direction_and_confidence() {
        let up = compute_aggressor_ratio(54.0, 46.0);
        assert_eq!(up.direction, "UP");
        assert!((up.ratio - 0.54).abs() < 1e-12);
        assert!((up.confidence - 0.568).abs() < 1e-12);

        let down = compute_aggressor_ratio(46.0, 54.0);
        assert_eq!(down.direction, "DOWN");
        assert!((down.ratio - 0.46).abs() < 1e-12);
        assert!((down.confidence - 0.568).abs() < 1e-12);
    }

    #[test]
    fn ratio_clamps_when_inputs_produce_out_of_range_values() {
        let data = compute_aggressor_ratio(5.0, -1.0);
        assert_eq!(data.direction, "UP");
        assert_eq!(data.ratio, 1.0);
        assert!((data.confidence - 0.70).abs() < 1e-12);
    }
}
