//! Hawkes-style order-flow clustering signal.
//!
//! This is a lightweight discrete approximation rather than a full fitted
//! continuous-time Hawkes process. It uses recent signed trade flow to estimate:
//! - side-specific excitation intensity
//! - clustering / branching strength
//! - a directional signal from the imbalance of those intensities

#[derive(Debug, Clone, Default)]
pub struct HawkesFlowData {
    /// Decayed excitation intensity for buy-initiated flow.
    pub buy_intensity: f64,
    /// Decayed excitation intensity for sell-initiated flow.
    pub sell_intensity: f64,
    /// Approximate branching ratio in [0, 0.99].
    pub branching_ratio: f64,
    /// Degree of same-side clustering in [0, 1].
    pub cluster_ratio: f64,
    /// Directional signal in [-1, 1].
    pub signal: f64,
    /// Confidence in [0, 1].
    pub confidence: f64,
}

/// Compute a Hawkes-style flow signal from recent Binance agg trades.
///
/// Input tuples are `(qty, is_buyer_maker)` where `false` means buy-initiated
/// flow and `true` means sell-initiated flow.
pub fn compute_hawkes_flow(trades: &[(f64, bool)]) -> HawkesFlowData {
    if trades.len() < 10 {
        return HawkesFlowData::default();
    }

    let decay_factor = (-0.35_f64).exp();
    let excitation_scale = 0.85_f64;

    let mut buy_excitation = 0.0_f64;
    let mut sell_excitation = 0.0_f64;
    let mut same_side_pairs = 0.0_f64;
    let mut pair_count = 0.0_f64;
    let mut burst_weight = 0.0_f64;
    let mut total_weight = 0.0_f64;
    let mut prev_side: Option<bool> = None;

    for &(qty, is_buyer_maker) in trades {
        let weight = qty.max(0.0).sqrt();
        if weight <= f64::EPSILON {
            continue;
        }

        let is_buy = !is_buyer_maker;
        buy_excitation *= decay_factor;
        sell_excitation *= decay_factor;

        if is_buy {
            buy_excitation += weight * excitation_scale;
        } else {
            sell_excitation += weight * excitation_scale;
        }

        total_weight += weight;

        if let Some(prev) = prev_side {
            pair_count += 1.0;
            if prev == is_buy {
                same_side_pairs += 1.0;
                burst_weight += weight;
            }
        }
        prev_side = Some(is_buy);
    }

    if total_weight <= f64::EPSILON {
        return HawkesFlowData::default();
    }

    let total_intensity = buy_excitation + sell_excitation;
    let intensity_imbalance = if total_intensity > f64::EPSILON {
        (buy_excitation - sell_excitation) / total_intensity
    } else {
        0.0
    };

    let cluster_ratio = if pair_count > 0.0 {
        (((same_side_pairs / pair_count) - 0.5) / 0.5).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let burst_ratio = (burst_weight / total_weight).clamp(0.0, 1.0);
    let branching_ratio = (0.55 * cluster_ratio + 0.45 * burst_ratio).clamp(0.0, 0.99);

    let signal = (intensity_imbalance * (0.35 + 0.65 * branching_ratio)).clamp(-1.0, 1.0);
    let confidence = (intensity_imbalance.abs() * 0.55 + branching_ratio * 0.45).clamp(0.0, 1.0);

    HawkesFlowData {
        buy_intensity: buy_excitation,
        sell_intensity: sell_excitation,
        branching_ratio,
        cluster_ratio,
        signal,
        confidence,
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_hawkes_flow, HawkesFlowData};

    fn buy_cluster(len: usize) -> Vec<(f64, bool)> {
        (0..len).map(|_| (25.0, false)).collect()
    }

    fn sell_cluster(len: usize) -> Vec<(f64, bool)> {
        (0..len).map(|_| (25.0, true)).collect()
    }

    fn alternating(len: usize) -> Vec<(f64, bool)> {
        (0..len).map(|i| (25.0, i % 2 == 0)).collect()
    }

    #[test]
    fn too_few_trades_returns_default() {
        let out = compute_hawkes_flow(&buy_cluster(4));
        assert_eq!(out.signal, HawkesFlowData::default().signal);
        assert_eq!(out.confidence, 0.0);
    }

    #[test]
    fn clustered_buy_flow_is_positive() {
        let out = compute_hawkes_flow(&buy_cluster(30));
        assert!(out.buy_intensity > out.sell_intensity);
        assert!(out.branching_ratio > 0.0);
        assert!(out.signal > 0.0);
        assert!(out.confidence > 0.0);
    }

    #[test]
    fn clustered_sell_flow_is_negative() {
        let out = compute_hawkes_flow(&sell_cluster(30));
        assert!(out.sell_intensity > out.buy_intensity);
        assert!(out.branching_ratio > 0.0);
        assert!(out.signal < 0.0);
        assert!(out.confidence > 0.0);
    }

    #[test]
    fn alternating_flow_is_less_confident_than_clustered_flow() {
        let clustered = compute_hawkes_flow(&buy_cluster(30));
        let alternating = compute_hawkes_flow(&alternating(30));
        assert!(alternating.cluster_ratio < clustered.cluster_ratio);
        assert!(alternating.branching_ratio < clustered.branching_ratio);
        assert!(alternating.confidence < clustered.confidence);
    }
}
