pub mod calibration;
pub mod data_logger;
pub mod deep_model;
pub mod edge_tracker;
pub mod ensemble;
pub mod hard_negatives;
pub mod input_builder;
pub mod model;
pub mod model_watcher;
pub mod normalizer;
pub mod persistence;
pub mod tracker;

use std::collections::{BTreeSet, HashMap};

#[allow(unused_imports)]
pub use calibration::IsotonicCalibrator;
#[allow(unused_imports)]
pub use hard_negatives::HardNegativeTracker;
pub use model::{OnlineLogisticRegression, FEATURE_NAMES, NUM_FEATURES};
pub use normalizer::FeatureNormalizer;
pub use tracker::RollingAccuracyTracker;

/// Combined ML state: model + normalizer + accuracy tracker.
#[derive(Clone)]
pub struct MlState {
    pub model: OnlineLogisticRegression,
    pub normalizer: FeatureNormalizer,
    pub tracker: RollingAccuracyTracker,
    pub learned_outcomes: BTreeSet<String>,
    pub calibrator: IsotonicCalibrator,
    pub snapshot_path: String,
}

impl Default for MlState {
    fn default() -> Self {
        Self::new()
    }
}

impl MlState {
    pub fn new() -> Self {
        Self {
            model: OnlineLogisticRegression::new(),
            normalizer: FeatureNormalizer::new(),
            tracker: RollingAccuracyTracker::new(200),
            learned_outcomes: BTreeSet::new(),
            calibrator: IsotonicCalibrator::new(),
            snapshot_path: persistence::default_path(),
        }
    }

    /// Load from disk or create fresh.
    pub fn load_or_new() -> Self {
        let path = persistence::default_path();
        if let Some(snap) = persistence::load(&path) {
            tracing::info!(
                "ML state loaded from {path}: {} samples, lr={:.4}",
                snap.model.samples_seen,
                snap.model.lr()
            );
            Self::from_snapshot(snap)
        } else {
            tracing::info!("ML state: fresh initialization (no {path} found)");
            Self::new()
        }
    }

    pub fn from_snapshot(snap: persistence::MlSnapshot) -> Self {
        let Some(learned_outcomes) = snap.learned_outcomes else {
            tracing::warn!("Legacy online-model snapshot has no learned-outcome identities; rebuilding from retained durable resolutions");
            return Self::new();
        };
        Self {
            model: snap.model,
            normalizer: snap.normalizer,
            tracker: snap.tracker,
            learned_outcomes,
            calibrator: snap.calibrator,
            snapshot_path: persistence::default_path(),
        }
    }

    /// Save current state to disk.
    pub fn save(&self) {
        let path = &self.snapshot_path;
        let snap = persistence::to_snapshot(
            &self.model,
            &self.normalizer,
            &self.tracker,
            &self.learned_outcomes,
            &self.calibrator,
        );
        if let Err(e) = persistence::save(&snap, path) {
            tracing::warn!("Failed to save ML state: {e}");
        }
    }
}

/// Extract feature effective values from the fusion breakdown HashMap
/// into a fixed-order array matching FEATURE_NAMES.
pub fn extract_features(breakdown: &HashMap<String, f64>) -> [f64; NUM_FEATURES] {
    let mut features = [0.0; NUM_FEATURES];
    for (i, name) in FEATURE_NAMES.iter().enumerate() {
        features[i] = breakdown.get(*name).copied().unwrap_or(0.0);
    }
    features
}

/// Predict p_up using ML model (normalizes features first).
pub fn ml_predict(state: &MlState, breakdown: &HashMap<String, f64>) -> f64 {
    let raw = extract_features(breakdown);
    let normed = state.normalizer.normalize(&raw);
    state.calibrator.calibrate(state.model.predict(&normed))
}

/// Learn a durable resolution once. Weights, normalization, calibration and
/// consumed identities are saved together by the caller.
pub fn learn_resolution(
    state: &mut MlState,
    identity: String,
    breakdown: &HashMap<String, f64>,
    label: f64,
) -> Option<(f64, [f64; NUM_FEATURES])> {
    let raw = extract_features(breakdown);
    if state.learned_outcomes.contains(&identity)
        || !matches!(label, 0.0 | 1.0)
        || raw.iter().any(|v| !v.is_finite())
    {
        return None;
    }
    // This is the same predictor/normalizer used at inference, before either updates.
    let before = state.normalizer.normalize(&raw);
    if before.iter().any(|v| !v.is_finite()) {
        return None;
    }
    let predicted = state.model.predict(&before);
    let mut normalizer = state.normalizer.clone();
    normalizer.update(&raw);
    let normed = normalizer.normalize(&raw);
    if normed.iter().any(|v| !v.is_finite()) {
        return None;
    }
    state.normalizer = normalizer;
    let weight = state.model.hard_negative_weight(&normed, label);
    state.model.update_with_weight(&normed, label, weight);
    state.tracker.record(predicted, label);
    state.calibrator.add_observation(predicted, label);
    state.learned_outcomes.insert(identity);
    Some((predicted, raw))
}

/// Update ML model with a new labeled sample.
/// label: 1.0 = market resolved UP, 0.0 = market resolved DOWN.
pub fn ml_update(state: &mut MlState, breakdown: &HashMap<String, f64>, label: f64) {
    let raw = extract_features(breakdown);
    if !matches!(label, 0.0 | 1.0) || raw.iter().any(|value| !value.is_finite()) {
        tracing::warn!("Rejected invalid online-model training observation");
        return;
    }
    state.normalizer.update(&raw);
    let normed = state.normalizer.normalize(&raw);
    let prediction = state.model.predict(&normed);
    state.model.update(&normed, label);
    state.tracker.record(prediction, label);

    tracing::info!(
        "ML update: label={label} pred={prediction:.3} samples={} lr={:.4} acc_20={:.1}%",
        state.model.samples_seen,
        state.model.lr(),
        state.tracker.accuracy(20).unwrap_or(0.0) * 100.0,
    );
}

/// Whether the ML model has enough training data to be trusted.
pub fn should_use_ml(state: &MlState) -> bool {
    state.model.samples_seen >= 50
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learning_observation_uses_inference_normalizer_before_update() {
        let mut state = MlState::new();
        for i in 0..10 {
            state.normalizer.update(&[i as f64; NUM_FEATURES]);
        }
        state.model.weights[0] = 0.8;
        let breakdown = HashMap::from([(FEATURE_NAMES[0].to_string(), 20.0)]);
        let predicted = ml_predict(&state, &breakdown);
        let (observed, _) =
            learn_resolution(&mut state, "outcome-a".into(), &breakdown, 1.0).unwrap();
        assert_eq!(observed, predicted);
        assert_eq!(
            state.calibrator.observations.back(),
            Some(&(predicted, 1.0))
        );
        let samples = state.model.samples_seen;
        assert!(learn_resolution(&mut state, "outcome-a".into(), &breakdown, 1.0).is_none());
        assert_eq!(state.model.samples_seen, samples);
    }

    #[test]
    fn legacy_snapshot_rebuilds_instead_of_guessing_consumed_rows() {
        let mut state = MlState::new();
        let breakdown = HashMap::from([(FEATURE_NAMES[0].to_string(), 0.5)]);
        ml_update(&mut state, &breakdown, 1.0);
        let mut snapshot = persistence::to_snapshot(
            &state.model,
            &state.normalizer,
            &state.tracker,
            &state.learned_outcomes,
            &state.calibrator,
        );
        snapshot.learned_outcomes = None;
        let rebuilt = MlState::from_snapshot(snapshot);
        assert_eq!(rebuilt.model.samples_seen, 0);
        assert!(rebuilt.learned_outcomes.is_empty());
    }

    #[test]
    fn lr_calibration_changes_lr_prediction_without_calibrating_fusion() {
        let mut state = MlState::new();
        state.calibrator.calibration_points = vec![(0.5, 0.2)];
        assert_eq!(ml_predict(&state, &HashMap::new()), 0.2);
        let fusion = 0.8;
        let ensemble = ensemble::EnsembleState::new();
        let deep = deep_model::DeepModelOutput {
            p_up: 0.9,
            magnitude: 0.0,
            volatility: 0.0,
            time_to_move: 0.0,
            mc_uncertainty: None,
            model_version: "fixture".into(),
            model_identity: "fixture-bytes".into(),
        };
        assert_eq!(ensemble.blend(fusion, &deep), (fusion, false));
    }
}
