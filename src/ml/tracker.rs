use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;

/// Tracks rolling prediction accuracy and Brier score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollingAccuracyTracker {
    /// (predicted_p_up, actual_label) pairs
    predictions: VecDeque<(f64, f64)>,
    max_window: usize,
}

impl RollingAccuracyTracker {
    pub fn new(max_window: usize) -> Self {
        Self {
            predictions: VecDeque::new(),
            max_window,
        }
    }

    pub fn record(&mut self, prediction: f64, label: f64) {
        self.predictions.push_back((prediction, label));
        while self.predictions.len() > self.max_window {
            self.predictions.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.predictions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.predictions.is_empty()
    }

    /// Fraction of correct directional predictions over last `window` samples.
    pub fn accuracy(&self, window: usize) -> Option<f64> {
        let n = self.predictions.len().min(window);
        if n == 0 {
            return None;
        }
        let start = self.predictions.len() - n;
        let correct = self
            .predictions
            .iter()
            .skip(start)
            .filter(|(pred, label)| {
                let predicted_up = *pred >= 0.5;
                let actual_up = *label >= 0.5;
                predicted_up == actual_up
            })
            .count();
        Some(correct as f64 / n as f64)
    }

    /// Mean squared error over last `window` samples.
    pub fn brier_score(&self, window: usize) -> Option<f64> {
        let n = self.predictions.len().min(window);
        if n == 0 {
            return None;
        }
        let start = self.predictions.len() - n;
        let sum: f64 = self
            .predictions
            .iter()
            .skip(start)
            .map(|(pred, label)| (pred - label).powi(2))
            .sum();
        Some(sum / n as f64)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "total_samples": self.len(),
            "accuracy_20": self.accuracy(20),
            "accuracy_50": self.accuracy(50),
            "accuracy_100": self.accuracy(100),
            "accuracy_200": self.accuracy(200),
            "brier_20": self.brier_score(20),
            "brier_50": self.brier_score(50),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_accuracy() {
        let mut t = RollingAccuracyTracker::new(100);
        // 3 correct, 1 wrong
        t.record(0.7, 1.0);
        t.record(0.3, 0.0);
        t.record(0.6, 1.0);
        t.record(0.8, 0.0); // wrong
        assert!((t.accuracy(10).unwrap() - 0.75).abs() < 0.01);
    }
}
