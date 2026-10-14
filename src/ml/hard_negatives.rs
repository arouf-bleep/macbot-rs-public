#![allow(dead_code, unused_imports)]
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Track losing trades and weight them higher in online learning
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardNegativeTracker {
    /// Recent losing trade feature snapshots with 3x weight
    pub hard_negatives: VecDeque<HardNegative>,
    pub max_stored: usize,
    pub weight_multiplier: f64, // default 3.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardNegative {
    pub features: Vec<f64>,  // feature snapshot at entry
    pub predicted_prob: f64, // what we predicted
    pub actual_outcome: f64, // what actually happened (0 or 1)
    pub loss_amount: f64,    // how much we lost
    pub timestamp: u64,
}

impl Default for HardNegativeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl HardNegativeTracker {
    pub fn new() -> Self {
        Self {
            hard_negatives: VecDeque::new(),
            max_stored: 200,
            weight_multiplier: 3.0,
        }
    }

    /// Record a losing trade for hard negative mining
    pub fn record_loss(&mut self, features: Vec<f64>, predicted: f64, actual: f64, loss: f64) {
        self.hard_negatives.push_back(HardNegative {
            features,
            predicted_prob: predicted,
            actual_outcome: actual,
            loss_amount: loss,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });
        while self.hard_negatives.len() > self.max_stored {
            self.hard_negatives.pop_front();
        }
    }

    /// Get weight for a sample based on whether it matches hard negative patterns
    pub fn get_sample_weight(&self, features: &[f64]) -> f64 {
        if self.hard_negatives.is_empty() {
            return 1.0;
        }

        // Check similarity to known hard negatives
        let mut max_similarity = 0.0_f64;
        for neg in &self.hard_negatives {
            let sim = cosine_similarity(features, &neg.features);
            max_similarity = max_similarity.max(sim);
        }

        // If very similar to a past losing setup, boost weight
        if max_similarity > 0.85 {
            self.weight_multiplier
        } else {
            1.0
        }
    }

    /// Should we skip this trade? Returns true if setup looks too similar to past losses
    pub fn should_skip(&self, features: &[f64], threshold: f64) -> bool {
        if self.hard_negatives.len() < 10 {
            return false;
        }

        let recent = self.hard_negatives.iter().rev().take(20);
        let similar_count = recent
            .filter(|neg| cosine_similarity(features, &neg.features) > threshold)
            .count();

        // If >50% of recent losses look like this setup, skip
        similar_count > 10
    }
}

fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    let len = a.len().min(b.len());
    let dot: f64 = a[..len].iter().zip(&b[..len]).map(|(x, y)| x * y).sum();
    let mag_a: f64 = a[..len].iter().map(|x| x * x).sum::<f64>().sqrt();
    let mag_b: f64 = b[..len].iter().map(|x| x * x).sum::<f64>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    dot / (mag_a * mag_b)
}
