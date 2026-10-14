use serde::{Deserialize, Serialize};

pub const NUM_FEATURES: usize = 34;

/// Previous feature count for migration from 24→34.
pub const PREV_NUM_FEATURES: usize = 24;

pub const FEATURE_NAMES: [&str; NUM_FEATURES] = [
    // 0–23: original features
    "orderbook_imbalance",
    "tick_velocity",
    "vwap_confluence",
    "price_divergence",
    "spike_detector",
    "deribit_pcr",
    "rsi_macd_ha",
    "fear_greed",
    "funding_rate",
    "oi_momentum",
    "aggressor_ratio",
    "coingecko_sentiment",
    "finnhub_sentiment",
    "lunarcrush_sentiment",
    "hour_of_day",
    "day_of_week",
    "distance_from_round_1000",
    "distance_from_round_500",
    "volatility_10",
    "volatility_30",
    "window_momentum",
    "long_liq_volume",
    "short_liq_volume",
    "depth_imbalance_5",
    // 24–33: features added in Sessions 5–16 (previously fusion-only, now tracked by LR/ONNX)
    "polymarket_flow",     // 24
    "vpin_microstructure", // 25
    "options_advanced",    // 26
    "whale_flow",          // 27
    "cross_market",        // 28
    "latency_arb",         // 29
    "news_sentiment",      // 30
    "btc_final_momentum",  // 31
    "kyle_lambda",         // 32
    "hawkes_flow",         // 33
];

/// Serde helper: serialize/deserialize `[f64; NUM_FEATURES]` via Vec<f64>.
/// Needed because serde's built-in array support caps at 32 elements.
pub mod serde_feature_array {
    use super::NUM_FEATURES;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(arr: &[f64; NUM_FEATURES], ser: S) -> Result<S::Ok, S::Error> {
        arr.as_slice().serialize(ser)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<[f64; NUM_FEATURES], D::Error> {
        let v: Vec<f64> = Vec::deserialize(de)?;
        v.try_into().map_err(|v: Vec<f64>| {
            serde::de::Error::custom(format!(
                "expected {} elements, got {}",
                NUM_FEATURES,
                v.len()
            ))
        })
    }
}

#[inline]
fn sigmoid(x: f64) -> f64 {
    1.0 / (1.0 + (-x).exp())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnlineLogisticRegression {
    #[serde(with = "serde_feature_array")]
    pub weights: [f64; NUM_FEATURES],
    pub bias: f64,
    pub lr_initial: f64,
    pub lr_decay: f64,
    pub samples_seen: u64,
    /// Tracks when we last applied weight decay (every 200 samples).
    #[serde(default)]
    pub last_decay_at: u64,
}

impl Default for OnlineLogisticRegression {
    fn default() -> Self {
        Self::new()
    }
}

impl OnlineLogisticRegression {
    pub fn new() -> Self {
        Self {
            weights: [0.0; NUM_FEATURES],
            bias: 0.0,
            lr_initial: 0.1,
            lr_decay: 0.002,
            samples_seen: 0,
            last_decay_at: 0,
        }
    }

    /// Current learning rate (decays with samples seen).
    pub fn lr(&self) -> f64 {
        self.lr_initial / (1.0 + self.lr_decay * self.samples_seen as f64)
    }

    /// Predict p_up from normalized features.
    pub fn predict(&self, features: &[f64; NUM_FEATURES]) -> f64 {
        if features.iter().any(|value| !value.is_finite()) {
            return 0.5;
        }
        let z: f64 = self
            .weights
            .iter()
            .zip(features.iter())
            .map(|(w, x)| w * x)
            .sum::<f64>()
            + self.bias;
        sigmoid(z)
    }

    /// SGD update with L2 weight decay: label = 1.0 (UP) or 0.0 (DOWN).
    /// Weight decay gently shrinks old weights toward zero, so the model
    /// naturally forgets stale regime data without manual retraining.
    pub fn update(&mut self, features: &[f64; NUM_FEATURES], label: f64) {
        if !matches!(label, 0.0 | 1.0) || features.iter().any(|value| !value.is_finite()) {
            return;
        }
        let prediction = self.predict(features);
        let error = prediction - label;
        let lr = self.lr();
        let weight_decay = 0.0001; // L2 regularization — old weights decay toward zero

        for (model_weight, feature) in self.weights.iter_mut().zip(features.iter()) {
            *model_weight -= lr * (error * feature + weight_decay * *model_weight);
        }
        self.bias -= lr * error;
        self.samples_seen += 1;
    }

    /// Update with a specific sample weight.
    /// Use weight=3.0 for hard negatives (cases where the model was confidently wrong).
    pub fn update_with_weight(&mut self, features: &[f64; NUM_FEATURES], label: f64, weight: f64) {
        if !matches!(label, 0.0 | 1.0)
            || !weight.is_finite()
            || features.iter().any(|value| !value.is_finite())
        {
            return;
        }
        let p = self.predict(features);
        let error = label - p;
        let lr = self.lr();
        let effective_lr = lr * weight.clamp(0.1, 5.0);
        let weight_decay = 0.0001;

        let feat_norm = features.iter().map(|x| x * x).sum::<f64>().sqrt().max(1.0);
        let grad_scale = effective_lr * error / feat_norm;

        for (model_weight, feature) in self.weights.iter_mut().zip(features.iter()) {
            *model_weight += grad_scale * feature - effective_lr * weight_decay * *model_weight;
        }
        self.bias += effective_lr * error;
        self.samples_seen += 1;
    }

    /// Check if this was a hard negative (model was confidently wrong).
    /// Returns weight to use: 3.0 for hard negatives, 1.0 otherwise.
    pub fn hard_negative_weight(&self, features: &[f64; NUM_FEATURES], actual_label: f64) -> f64 {
        let predicted = self.predict(features);
        let predicted_direction = if predicted > 0.5 { 1.0 } else { 0.0 };
        let was_wrong = (predicted_direction - actual_label).abs() > 0.5;
        let was_confident = (predicted - 0.5).abs() > 0.20; // > 70% or < 30% confidence

        if was_wrong && was_confident {
            3.0 // Hard negative: confident and wrong — learn 3x harder
        } else {
            1.0
        }
    }

    /// Feature importance sorted by absolute weight descending.
    pub fn feature_importance(&self) -> Vec<(String, f64)> {
        let mut pairs: Vec<(String, f64)> = FEATURE_NAMES
            .iter()
            .zip(self.weights.iter())
            .map(|(n, w)| (n.to_string(), *w))
            .collect();
        pairs.sort_by(|a, b| {
            b.1.abs()
                .partial_cmp(&a.1.abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        pairs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_observations_do_not_poison_model_weights() {
        let mut model = OnlineLogisticRegression::new();
        let mut features = [0.0; NUM_FEATURES];
        features[0] = f64::NAN;
        model.update(&features, 1.0);
        assert_eq!(model.samples_seen, 0);
        assert_eq!(model.predict(&features), 0.5);
        features[0] = 1.0;
        model.update(&features, f64::NAN);
        model.update_with_weight(&features, 1.0, f64::INFINITY);
        assert_eq!(model.samples_seen, 0);
        assert!(model.weights.iter().all(|weight| weight.is_finite()));
    }

    #[test]
    fn test_predict_zero_weights() {
        let model = OnlineLogisticRegression::new();
        let features = [0.0; NUM_FEATURES];
        let p = model.predict(&features);
        assert!((p - 0.5).abs() < 1e-10, "zero weights should predict 0.5");
    }

    #[test]
    fn test_update_moves_prediction() {
        let mut model = OnlineLogisticRegression::new();
        let mut features = [0.0; NUM_FEATURES];
        features[0] = 1.0;

        // Train on UP label several times
        for _ in 0..20 {
            model.update(&features, 1.0);
        }
        let p = model.predict(&features);
        assert!(
            p > 0.6,
            "after training on UP, should predict >0.6, got {p}"
        );
    }

    #[test]
    fn test_lr_decay() {
        let mut model = OnlineLogisticRegression::new();
        let lr0 = model.lr();
        model.samples_seen = 100;
        let lr1 = model.lr();
        assert!(lr1 < lr0, "learning rate should decay");
    }
}
