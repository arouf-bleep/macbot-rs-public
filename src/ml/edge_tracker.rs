//! Realized vs implied edge tracking.
//!
//! Tracks the gap between predicted edge (`net_ev` at trade entry) and actual
//! outcome (P&L %), revealing systematic overconfidence or underconfidence in
//! the signal fusion pipeline.

use std::collections::VecDeque;

use serde_json::{json, Value};

/// Rolling window of (predicted_edge, realised_pnl, stake) observations.
///
/// The stake is kept because the means have to be weighted by it. Averaging
/// unweighted per-trade returns lets a one-cent trade count as much as a
/// four-dollar one, and on this book stakes span that whole range: doing so
/// reported a realised edge of 50% against a true return on turnover of 1.4%,
/// which inverted the sign of the calibration error.
pub struct EdgeTracker {
    observations: VecDeque<Observation>,
    window_size: usize,
}

#[derive(Debug, Clone, Copy)]
struct Observation {
    predicted_edge: f64,
    realised_pnl: f64,
    stake: f64,
}

impl EdgeTracker {
    pub fn new(window_size: usize) -> Self {
        Self {
            observations: VecDeque::with_capacity(window_size),
            window_size,
        }
    }

    /// Record an observation, trimming to the window.
    ///
    /// Takes the realised P&L and the stake rather than a precomputed ratio, so
    /// the averages below can be weighted by how much was actually at risk.
    pub fn record(&mut self, predicted_edge: f64, realised_pnl: f64, stake: f64) {
        if stake <= 0.0 {
            return;
        }
        self.observations.push_back(Observation {
            predicted_edge,
            realised_pnl,
            stake,
        });
        while self.observations.len() > self.window_size {
            self.observations.pop_front();
        }
    }

    fn total_stake(&self) -> f64 {
        self.observations.iter().map(|o| o.stake).sum()
    }

    /// Number of observations currently stored.
    pub fn len(&self) -> usize {
        self.observations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.observations.is_empty()
    }

    /// Average predicted edge across the window.
    /// Stake-weighted mean predicted edge.
    pub fn mean_predicted_edge(&self) -> Option<f64> {
        let stake = self.total_stake();
        if stake <= 0.0 {
            return None;
        }
        let weighted: f64 = self
            .observations
            .iter()
            .map(|o| o.predicted_edge * o.stake)
            .sum();
        Some(weighted / stake)
    }

    /// Realised return on turnover: total P&L over total staked.
    ///
    /// This is the figure the predicted edge has to be compared against. An
    /// unweighted mean of per-trade returns answers a different question and,
    /// on a book with stakes spanning two orders of magnitude, a misleading one.
    pub fn mean_actual_edge(&self) -> Option<f64> {
        let stake = self.total_stake();
        if stake <= 0.0 {
            return None;
        }
        let pnl: f64 = self.observations.iter().map(|o| o.realised_pnl).sum();
        Some(pnl / stake)
    }

    /// Calibration ratio: actual / predicted.
    /// - < 1.0 means overconfident (predicted more edge than realized)
    /// - > 1.0 means underconfident
    /// - ≈ 1.0 means well-calibrated
    ///
    /// Returns `None` if no observations or mean predicted edge is ~0.
    pub fn calibration_ratio(&self) -> Option<f64> {
        let predicted = self.mean_predicted_edge()?;
        let actual = self.mean_actual_edge()?;
        if predicted.abs() < 1e-12 {
            return None;
        }
        Some(actual / predicted)
    }

    /// Edge bias: predicted - actual.
    /// - Positive = overconfident (predicted edge exceeded realized)
    /// - Negative = underconfident
    pub fn edge_bias(&self) -> Option<f64> {
        let predicted = self.mean_predicted_edge()?;
        let actual = self.mean_actual_edge()?;
        Some(predicted - actual)
    }

    /// Serialize metrics for API / dashboard consumption.
    pub fn to_json(&self) -> Value {
        json!({
            "observations": self.observations.len(),
            "window_size": self.window_size,
            "mean_predicted_edge": self.mean_predicted_edge(),
            "mean_actual_edge": self.mean_actual_edge(),
            "calibration_ratio": self.calibration_ratio(),
            "edge_bias": self.edge_bias(),
        })
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_penny_trade_does_not_count_as_much_as_a_large_one() {
        // The real book had stakes from 1.5 cents to just over four dollars.
        // Averaging per-trade returns unweighted reported a realised edge of
        // 50% against a true return on turnover of 1.4%, which inverted the
        // sign of the calibration error and made an overconfident model look
        // underconfident.
        let mut tracker = EdgeTracker::new(100);
        // One large trade that lost a little.
        tracker.record(0.05, -0.04, 4.00);
        // One tiny trade that doubled.
        tracker.record(0.05, 0.02, 0.02);

        let realised = tracker.mean_actual_edge().expect("two observations");
        // Weighted: (-0.04 + 0.02) / 4.02 is slightly negative.
        assert!(
            realised < 0.0,
            "the large losing trade must dominate; got {realised}"
        );
        // Unweighted this would be (-0.01 + 1.00) / 2 = +0.495, wildly positive.
        assert!(
            realised > -0.02,
            "and it should not overshoot either; got {realised}"
        );
    }

    #[test]
    fn realised_edge_is_return_on_turnover() {
        let mut tracker = EdgeTracker::new(100);
        tracker.record(0.05, 1.0, 50.0);
        tracker.record(0.05, -0.5, 50.0);
        // 0.5 profit on 100 staked.
        let realised = tracker.mean_actual_edge().expect("observations");
        assert!((realised - 0.005).abs() < 1e-12, "got {realised}");
    }

    #[test]
    fn a_zero_stake_observation_is_refused() {
        // Dividing by it would poison every figure derived from the window.
        let mut tracker = EdgeTracker::new(100);
        tracker.record(0.05, 1.0, 0.0);
        assert!(tracker.is_empty());
        assert_eq!(tracker.mean_actual_edge(), None);
    }

    #[test]
    fn predicted_edge_is_weighted_by_stake_too() {
        // Otherwise the two halves of the ratio are computed on different
        // bases and the ratio means nothing.
        let mut tracker = EdgeTracker::new(100);
        tracker.record(0.10, 0.0, 90.0);
        tracker.record(1.00, 0.0, 10.0);
        let predicted = tracker.mean_predicted_edge().expect("observations");
        // (0.10*90 + 1.00*10) / 100 = 0.19, not the unweighted 0.55.
        assert!((predicted - 0.19).abs() < 1e-12, "got {predicted}");
    }

    use super::*;

    #[test]
    fn perfectly_calibrated() {
        let mut tracker = EdgeTracker::new(100);
        // Predicted edge matches actual P&L perfectly
        for i in 0..50 {
            let edge = 0.05 + (i as f64) * 0.001;
            tracker.record(edge, edge, 1.0);
        }
        assert_eq!(tracker.len(), 50);

        let ratio = tracker.calibration_ratio().unwrap();
        assert!(
            (ratio - 1.0).abs() < 1e-9,
            "calibration_ratio should be ~1.0 for perfect calibration, got {ratio}"
        );

        let bias = tracker.edge_bias().unwrap();
        assert!(
            bias.abs() < 1e-9,
            "edge_bias should be ~0.0 for perfect calibration, got {bias}"
        );
    }

    #[test]
    fn overconfident_predictions() {
        let mut tracker = EdgeTracker::new(100);
        // Predict 10% edge but only realize 5%
        for _ in 0..40 {
            tracker.record(0.10, 0.05, 1.0);
        }
        assert_eq!(tracker.len(), 40);

        let ratio = tracker.calibration_ratio().unwrap();
        assert!(
            (ratio - 0.5).abs() < 1e-9,
            "calibration_ratio should be 0.5 for 2x overconfidence, got {ratio}"
        );

        let bias = tracker.edge_bias().unwrap();
        assert!(
            (bias - 0.05).abs() < 1e-9,
            "edge_bias should be 0.05, got {bias}"
        );
    }

    #[test]
    fn underconfident_predictions() {
        let mut tracker = EdgeTracker::new(100);
        // Predict 3% edge but realize 6%
        for _ in 0..30 {
            tracker.record(0.03, 0.06, 1.0);
        }

        let ratio = tracker.calibration_ratio().unwrap();
        assert!(
            (ratio - 2.0).abs() < 1e-9,
            "calibration_ratio should be 2.0 for underconfidence, got {ratio}"
        );

        let bias = tracker.edge_bias().unwrap();
        assert!(
            (bias - (-0.03)).abs() < 1e-9,
            "edge_bias should be -0.03, got {bias}"
        );
    }

    #[test]
    fn window_eviction() {
        let mut tracker = EdgeTracker::new(5);
        for i in 0..10 {
            tracker.record(i as f64, i as f64, 1.0);
        }
        assert_eq!(tracker.len(), 5);
        // Only last 5 observations (5,6,7,8,9)
        let mean = tracker.mean_predicted_edge().unwrap();
        assert!(
            (mean - 7.0).abs() < 1e-9,
            "mean should be 7.0 after eviction, got {mean}"
        );
    }

    #[test]
    fn empty_tracker() {
        let tracker = EdgeTracker::new(100);
        assert_eq!(tracker.len(), 0);
        assert!(tracker.mean_predicted_edge().is_none());
        assert!(tracker.mean_actual_edge().is_none());
        assert!(tracker.calibration_ratio().is_none());
        assert!(tracker.edge_bias().is_none());

        let j = tracker.to_json();
        assert_eq!(j["observations"], 0);
        assert!(j["calibration_ratio"].is_null());
    }

    #[test]
    fn zero_predicted_edge() {
        let mut tracker = EdgeTracker::new(100);
        tracker.record(0.0, 0.05, 1.0);
        // calibration_ratio should be None when predicted is ~0
        assert!(tracker.calibration_ratio().is_none());
        // edge_bias is still valid
        let bias = tracker.edge_bias().unwrap();
        assert!((bias - (-0.05)).abs() < 1e-9);
    }

    #[test]
    fn to_json_populated() {
        let mut tracker = EdgeTracker::new(100);
        tracker.record(0.08, 0.06, 1.0);
        tracker.record(0.12, 0.04, 1.0);

        let j = tracker.to_json();
        assert_eq!(j["observations"], 2);
        assert_eq!(j["window_size"], 100);
        // mean predicted = (0.08 + 0.12) / 2 = 0.10
        let mp = j["mean_predicted_edge"].as_f64().unwrap();
        assert!((mp - 0.10).abs() < 1e-9);
        // mean actual = (0.06 + 0.04) / 2 = 0.05
        let ma = j["mean_actual_edge"].as_f64().unwrap();
        assert!((ma - 0.05).abs() < 1e-9);
        // ratio = 0.05 / 0.10 = 0.5
        let ratio = j["calibration_ratio"].as_f64().unwrap();
        assert!((ratio - 0.5).abs() < 1e-9);
    }
}
