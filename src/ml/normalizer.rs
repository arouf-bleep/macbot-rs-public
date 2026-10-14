use serde::{Deserialize, Serialize};

use super::model::serde_feature_array;
use super::model::NUM_FEATURES;

/// Running z-score normalizer using Welford's online algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureNormalizer {
    #[serde(with = "serde_feature_array")]
    pub means: [f64; NUM_FEATURES],
    #[serde(with = "serde_feature_array")]
    pub m2: [f64; NUM_FEATURES],
    pub count: u64,
}

impl Default for FeatureNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

impl FeatureNormalizer {
    pub fn new() -> Self {
        Self {
            means: [0.0; NUM_FEATURES],
            m2: [0.0; NUM_FEATURES],
            count: 0,
        }
    }

    /// Update running statistics with a new raw feature vector.
    pub fn update(&mut self, raw: &[f64; NUM_FEATURES]) {
        if raw.iter().any(|value| !value.is_finite()) {
            return;
        }
        self.count += 1;
        let n = self.count as f64;
        for ((mean, m2), raw_value) in self
            .means
            .iter_mut()
            .zip(self.m2.iter_mut())
            .zip(raw.iter())
        {
            let delta = raw_value - *mean;
            *mean += delta / n;
            let delta2 = raw_value - *mean;
            *m2 += delta * delta2;
        }
    }

    /// Compute standard deviations from running m2.
    pub fn std_devs(&self) -> [f64; NUM_FEATURES] {
        let mut stds = [0.0; NUM_FEATURES];
        if self.count < 2 {
            return stds;
        }
        for (std, m2) in stds.iter_mut().zip(self.m2.iter()) {
            *std = (m2 / (self.count - 1) as f64).sqrt();
        }
        stds
    }

    /// Normalize raw features to z-scores.
    /// Returns raw values if fewer than 5 samples seen.
    pub fn normalize(&self, raw: &[f64; NUM_FEATURES]) -> [f64; NUM_FEATURES] {
        if self.count < 5 {
            return *raw;
        }
        let stds = self.std_devs();
        let mut normed = [0.0; NUM_FEATURES];
        for (((normalized, raw_value), mean), std) in normed
            .iter_mut()
            .zip(raw.iter())
            .zip(self.means.iter())
            .zip(stds.iter())
        {
            *normalized = (raw_value - mean) / std.max(0.01);
        }
        normed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_identity_early() {
        let norm = FeatureNormalizer::new();
        let raw = [0.5; NUM_FEATURES];
        let out = norm.normalize(&raw);
        assert_eq!(out, raw, "should return raw when count < 5");
    }

    #[test]
    fn test_normalize_after_samples() {
        let mut norm = FeatureNormalizer::new();
        for i in 0..10 {
            let mut raw = [0.0; NUM_FEATURES];
            raw[0] = i as f64;
            norm.update(&raw);
        }
        // mean of 0..9 = 4.5, std ≈ 3.03
        let mut test = [0.0; NUM_FEATURES];
        test[0] = 4.5;
        let out = norm.normalize(&test);
        assert!(out[0].abs() < 0.01, "mean value should normalize near 0");
    }
}
