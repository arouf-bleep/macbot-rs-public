//! Roll-style microstructure detection from short-horizon mid-price changes.
//!
//! In uninformed markets, prices tend to bounce between bid and ask, creating
//! negative serial covariance in successive price changes. When informed flow
//! arrives and keeps hitting one side, that covariance becomes less negative or
//! turns positive. We use Polymarket L2 mid-price changes as a lightweight
//! proxy for that arrival detection.

#[derive(Debug, Clone, Default)]
pub struct RollMicrostructureData {
    /// Serial covariance of successive mid-price changes, in bps^2.
    pub serial_covariance_bps2: f64,
    /// Normalized persistence score in [-1, 1]. Positive means directional
    /// persistence; negative means bid/ask bounce dominates.
    pub persistence: f64,
    /// Signed directional signal in [-1, 1]. Positive implies BUY_YES flow,
    /// negative implies BUY_NO flow.
    pub signal: f64,
    /// Confidence [0, 1] that informed directional flow is present.
    pub confidence: f64,
    /// Average of the most recent deltas, in bps.
    pub recent_delta_bps: f64,
    /// Number of deltas used in the estimate.
    pub sample_count: usize,
}

pub fn compute_roll_microstructure(mids: &[f64]) -> RollMicrostructureData {
    if mids.len() < 4 {
        return RollMicrostructureData::default();
    }

    let deltas: Vec<f64> = mids
        .windows(2)
        .filter_map(|pair| {
            let prev = pair[0];
            let next = pair[1];
            if prev > 0.0 && next > 0.0 {
                Some((next - prev) / prev * 10_000.0)
            } else {
                None
            }
        })
        .collect();

    if deltas.len() < 3 {
        return RollMicrostructureData::default();
    }

    let x = &deltas[1..];
    let y = &deltas[..deltas.len() - 1];
    let mean_x = x.iter().sum::<f64>() / x.len() as f64;
    let mean_y = y.iter().sum::<f64>() / y.len() as f64;
    let serial_covariance_bps2 = x
        .iter()
        .zip(y.iter())
        .map(|(a, b)| (a - mean_x) * (b - mean_y))
        .sum::<f64>()
        / x.len() as f64;

    let delta_scale = deltas.iter().map(|d| d * d).sum::<f64>() / deltas.len() as f64;
    let persistence = if delta_scale > 1e-12 {
        (serial_covariance_bps2 / delta_scale).clamp(-1.0, 1.0)
    } else {
        0.0
    };

    let recent_slice = &deltas[deltas.len().saturating_sub(3)..];
    let recent_delta_bps = recent_slice.iter().sum::<f64>() / recent_slice.len() as f64;
    let directional_bias = if recent_delta_bps.abs() > 1e-9 {
        recent_delta_bps.signum()
    } else {
        0.0
    };

    let sample_factor = ((deltas.len() as f64 - 2.0) / 10.0).clamp(0.0, 1.0);
    let positive_persistence = persistence.max(0.0);
    let confidence = (positive_persistence.abs() * sample_factor).clamp(0.0, 1.0);
    let signal = (positive_persistence * directional_bias).clamp(-1.0, 1.0);

    RollMicrostructureData {
        serial_covariance_bps2,
        persistence,
        signal,
        confidence,
        recent_delta_bps,
        sample_count: deltas.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upward_persistence_produces_positive_signal() {
        let mids = [0.50, 0.505, 0.511, 0.518, 0.526, 0.535];
        let data = compute_roll_microstructure(&mids);
        assert!(data.persistence > 0.0);
        assert!(data.signal > 0.0);
        assert!(data.confidence > 0.0);
    }

    #[test]
    fn downward_persistence_produces_negative_signal() {
        let mids = [0.55, 0.548, 0.545, 0.541, 0.536, 0.53];
        let data = compute_roll_microstructure(&mids);
        assert!(data.persistence > 0.0);
        assert!(data.signal < 0.0);
        assert!(data.confidence > 0.0);
    }

    #[test]
    fn alternating_bounce_stays_low_confidence() {
        let mids = [0.50, 0.505, 0.50, 0.505, 0.50, 0.505];
        let data = compute_roll_microstructure(&mids);
        assert!(data.persistence < 0.0);
        assert_eq!(data.signal, 0.0);
        assert_eq!(data.confidence, 0.0);
    }
}
