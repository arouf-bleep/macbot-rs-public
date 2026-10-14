#![allow(dead_code, unused_imports)]
use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// Isotonic regression calibrator
/// Maps raw model probabilities to calibrated probabilities based on historical accuracy
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsotonicCalibrator {
    /// Sorted (raw_probability, actual_win_rate) pairs
    pub calibration_points: Vec<(f64, f64)>,
    /// Number of bins for calibration
    pub num_bins: usize,
    /// Minimum samples per bin before calibration is active
    pub min_samples_per_bin: usize,
    /// Raw observations: (predicted_prob, actual_outcome)
    pub observations: VecDeque<(f64, f64)>,
    pub max_observations: usize,
}

impl Default for IsotonicCalibrator {
    fn default() -> Self {
        Self::new()
    }
}

impl IsotonicCalibrator {
    pub fn new() -> Self {
        Self {
            calibration_points: Vec::new(),
            num_bins: 20,
            min_samples_per_bin: 5,
            observations: VecDeque::new(),
            max_observations: 2000,
        }
    }

    /// Add an observation (predicted probability, actual outcome 0/1)
    pub fn add_observation(&mut self, predicted: f64, actual: f64) {
        if !predicted.is_finite()
            || !(0.0..=1.0).contains(&predicted)
            || !matches!(actual, 0.0 | 1.0)
        {
            return;
        }
        self.observations.push_back((predicted, actual));
        if self.observations.len() > self.max_observations {
            self.observations.pop_front();
        }
        // Rebuild calibration every 50 observations
        if self.observations.len().is_multiple_of(50) {
            self.fit();
        }
    }

    /// Fit isotonic regression on accumulated observations
    pub fn fit(&mut self) {
        if self.observations.len() < self.num_bins * self.min_samples_per_bin {
            return; // not enough data
        }

        self.calibration_points =
            fit_monotonic(&self.observations.iter().copied().collect::<Vec<_>>());
    }

    /// Calibrate a raw probability
    pub fn calibrate(&self, raw_prob: f64) -> f64 {
        if self.calibration_points.is_empty() {
            return raw_prob;
        }

        // Binary search for the right interval
        let points = &self.calibration_points;
        if raw_prob <= points[0].0 {
            return points[0].1;
        }
        if let Some((last_x, last_y)) = points.last().copied() {
            if raw_prob >= last_x {
                return last_y;
            }
        }

        for i in 0..points.len() - 1 {
            if raw_prob >= points[i].0 && raw_prob <= points[i + 1].0 {
                // Linear interpolation
                let frac = (raw_prob - points[i].0) / (points[i + 1].0 - points[i].0);
                return points[i].1 + frac * (points[i + 1].1 - points[i].1);
            }
        }
        raw_prob
    }

    /// Compute Expected Calibration Error
    pub fn ece(&self) -> f64 {
        if self.calibration_points.is_empty() {
            return 1.0;
        }
        self.calibration_points
            .iter()
            .map(|(pred, actual)| (pred - actual).abs())
            .sum::<f64>()
            / self.calibration_points.len() as f64
    }

    pub fn is_ready(&self) -> bool {
        !self.calibration_points.is_empty()
    }
}

/// Pool Adjacent Violators Algorithm for isotonic regression
pub(crate) fn fit_monotonic(points: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut sorted: Vec<_> = points
        .iter()
        .copied()
        .filter(|(x, y)| {
            x.is_finite() && y.is_finite() && (0.0..=1.0).contains(x) && (0.0..=1.0).contains(y)
        })
        .collect();
    // Tie outcomes are aggregated before PAV, never split into ordered bins.
    sorted.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1)));
    let mut groups: Vec<(f64, f64, usize)> = Vec::new();
    for (x, y) in sorted {
        if let Some(last) = groups.last_mut().filter(|last| last.0 == x) {
            last.1 += y;
            last.2 += 1;
        } else {
            groups.push((x, y, 1));
        }
    }
    let mut result: Vec<(f64, f64, usize)> = Vec::new();
    for (x, total_y, count) in groups {
        result.push((x, total_y / count as f64, count));

        while result.len() >= 2 {
            let n = result.len();
            if result[n - 1].1 < result[n - 2].1 {
                // Pool: merge last two blocks
                let Some(last) = result.pop() else {
                    break;
                };
                let Some(prev) = result.last_mut() else {
                    result.push(last);
                    break;
                };
                let total = prev.2 + last.2;
                prev.0 = (prev.0 * prev.2 as f64 + last.0 * last.2 as f64) / total as f64;
                prev.1 = (prev.1 * prev.2 as f64 + last.1 * last.2 as f64) / total as f64;
                prev.2 = total;
            } else {
                break;
            }
        }
    }

    result.into_iter().map(|(x, y, _)| (x, y)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_single_block_fit_calibrates_to_constant() {
        let mut calibrator = IsotonicCalibrator::new();
        for index in 0..100 {
            calibrator.add_observation(
                0.01 + index as f64 * 0.0098,
                if index < 50 { 1.0 } else { 0.0 },
            );
        }
        assert_eq!(calibrator.calibration_points.len(), 1);
        assert!((calibrator.calibrate(0.95) - 0.5).abs() < 1e-12);
        assert!(calibrator.is_ready());
    }

    #[test]
    fn equal_predictions_are_order_independent_and_weighted() {
        let first = vec![(0.3, 1.0), (0.3, 0.0), (0.3, 0.0), (0.8, 1.0)];
        let mut reversed = first.clone();
        reversed.reverse();
        assert_eq!(fit_monotonic(&first), fit_monotonic(&reversed));
        assert_eq!(fit_monotonic(&first), vec![(0.3, 1.0 / 3.0), (0.8, 1.0)]);
        let pooled = fit_monotonic(&[(0.3, 1.0), (0.3, 1.0), (0.3, 1.0), (0.8, 0.0)]);
        assert_eq!(pooled.len(), 1);
        assert!((pooled[0].0 - 0.425).abs() < 1e-12);
        assert_eq!(pooled[0].1, 0.75);
    }

    #[test]
    fn invalid_observations_do_not_enter_calibration() {
        let mut calibrator = IsotonicCalibrator::new();
        for pair in [
            (f64::NAN, 1.0),
            (2.0, 1.0),
            (0.5, f64::INFINITY),
            (0.5, 0.2),
        ] {
            calibrator.add_observation(pair.0, pair.1);
        }
        assert!(calibrator.observations.is_empty());
        assert!(!calibrator.is_ready());
    }
}
