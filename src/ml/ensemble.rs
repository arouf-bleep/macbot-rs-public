//! Ensemble layer: blends LR + deep model predictions with automatic
//! shadow → active promotion/demotion and online isotonic calibration.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

use super::deep_model::DeepModelOutput;
use super::tracker::RollingAccuracyTracker;

// ── Configuration ───────────────────────────────────────────────────────────

const SHADOW_MIN_PREDICTIONS: usize = 200;
const PROMOTE_ACCURACY_THRESHOLD: f64 = 0.54;
const DEMOTE_ACCURACY_THRESHOLD: f64 = 0.52;
const DEMOTE_WINDOW: usize = 50;
const RAMP_STEPS: usize = 100;
const MAX_DEEP_WEIGHT: f64 = 0.60;
/// MC Dropout uncertainty threshold — skip trade entirely above this.
const MC_UNCERTAINTY_SKIP_THRESHOLD: f64 = 0.08;

// ── Ensemble State ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnsemblePhase {
    /// Deep model predictions are logged but ignored in trading.
    Shadow,
    /// Weight is ramping from 0 toward target.
    Ramping,
    /// Deep model is fully active at its current weight.
    Active,
}

impl EnsemblePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shadow => "shadow",
            Self::Ramping => "ramping",
            Self::Active => "active",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsembleState {
    /// Approval, calibration and pending outcomes belong only to these bytes.
    #[serde(default)]
    pub model_identity: Option<String>,
    pub phase: EnsemblePhase,
    /// Current deep model blend weight (0.0 in shadow, 0..MAX in active).
    pub deep_weight: f64,
    /// Ramp progress counter (0..RAMP_STEPS).
    pub ramp_step: usize,
    /// Deep model prediction tracker (independent of LR tracker).
    pub deep_tracker: RollingAccuracyTracker,
    /// Isotonic calibration breakpoints: sorted (raw_p, calibrated_p) pairs.
    pub isotonic_breakpoints: Vec<(f64, f64)>,
    /// Rolling calibration data: (raw_prediction, actual_label) for isotonic fitting.
    calibration_data: VecDeque<(f64, f64)>,
    /// Recent calibrated predictions + outcomes for measuring calibration quality.
    #[serde(default)]
    calibration_quality_data: VecDeque<(f64, f64)>,
    /// Last deep model prediction (for dashboard display).
    #[serde(default)]
    pub last_deep_p_up: Option<f64>,
    #[serde(default)]
    pub last_blended_p_up: Option<f64>,
    #[serde(default)]
    pub last_mc_uncertainty: Option<f64>,
    #[serde(default)]
    pub last_should_skip: Option<bool>,
    /// Pending deep predictions keyed by market_id, awaiting resolution.
    #[serde(default)]
    pub pending_predictions: HashMap<String, f64>,
}

impl Default for EnsembleState {
    fn default() -> Self {
        Self::new()
    }
}

impl EnsembleState {
    pub fn new() -> Self {
        Self {
            model_identity: None,
            phase: EnsemblePhase::Shadow,
            deep_weight: 0.0,
            ramp_step: 0,
            deep_tracker: RollingAccuracyTracker::new(500),
            isotonic_breakpoints: Vec::new(),
            calibration_data: VecDeque::new(),
            calibration_quality_data: VecDeque::new(),
            last_deep_p_up: None,
            last_blended_p_up: None,
            last_mc_uncertainty: None,
            last_should_skip: None,
            pending_predictions: HashMap::new(),
        }
    }

    /// A replacement (including legacy unbound state) always starts in shadow.
    /// Re-reading identical bytes preserves the existing approval and evidence.
    pub fn bind_model(&mut self, identity: &str) -> bool {
        if self.model_identity.as_deref() == Some(identity) {
            return false;
        }
        *self = Self::new();
        self.model_identity = Some(identity.to_string());
        true
    }

    /// Record a deep model prediction outcome for tracking.
    pub fn record_deep_prediction(&mut self, predicted_p_up: f64, actual_label: f64) {
        self.deep_tracker.record(predicted_p_up, actual_label);

        // Record calibrated prediction for quality tracking
        let calibrated = self.calibrate(predicted_p_up);
        self.record_calibration_outcome(calibrated, actual_label);

        // Also record for isotonic calibration
        self.calibration_data
            .push_back((predicted_p_up, actual_label));
        while self.calibration_data.len() > 500 {
            self.calibration_data.pop_front();
        }

        // Refit isotonic calibration every 50 new samples
        if self.calibration_data.len().is_multiple_of(50) && self.calibration_data.len() >= 30 {
            self.fit_isotonic();
        }

        // Phase transitions
        self.update_phase();
    }

    /// Blend LR and deep model predictions.
    ///
    /// Returns `(blended_p_up, should_skip)`.
    /// `should_skip` is true when MC uncertainty is too high.
    pub fn blend(&self, lr_p_up: f64, deep_output: &DeepModelOutput) -> (f64, bool) {
        if self.model_identity.as_deref() != Some(deep_output.model_identity.as_str()) {
            return (lr_p_up, false);
        }
        let w = self.deep_weight;
        if w < 0.001 {
            return (lr_p_up, false);
        }
        // Check MC Dropout uncertainty — skip trade entirely if too high
        if let Some(unc) = deep_output.mc_uncertainty {
            if unc > MC_UNCERTAINTY_SKIP_THRESHOLD {
                return (lr_p_up, true);
            }
        }

        let deep_p = self.calibrate(deep_output.p_up);
        let blended = (1.0 - w) * lr_p_up + w * deep_p;
        (blended.clamp(0.001, 0.999), false)
    }

    /// Apply isotonic calibration to a raw deep model p_up.
    pub fn calibrate(&self, raw_p: f64) -> f64 {
        if self.isotonic_breakpoints.is_empty() {
            return raw_p;
        }

        // Binary search for the interpolation interval
        let bps = &self.isotonic_breakpoints;
        if raw_p <= bps[0].0 {
            return bps[0].1;
        }
        if raw_p >= bps[bps.len() - 1].0 {
            return bps[bps.len() - 1].1;
        }

        for i in 0..bps.len() - 1 {
            if raw_p >= bps[i].0 && raw_p < bps[i + 1].0 {
                // Linear interpolation
                let frac = (raw_p - bps[i].0) / (bps[i + 1].0 - bps[i].0);
                return bps[i].1 + frac * (bps[i + 1].1 - bps[i].1);
            }
        }
        raw_p
    }

    /// Record a calibrated prediction outcome for quality tracking.
    /// Call this after calibration has been applied and the outcome is known.
    pub fn record_calibration_outcome(&mut self, calibrated_p: f64, actual_label: f64) {
        self.calibration_quality_data
            .push_back((calibrated_p, actual_label));
        while self.calibration_quality_data.len() > 100 {
            self.calibration_quality_data.pop_front();
        }
    }

    /// Mean absolute calibration error over the last 100 predictions.
    /// Returns None if no calibration quality data is available.
    /// Lower is better — 0.0 means perfectly calibrated.
    pub fn calibration_quality(&self) -> Option<f64> {
        if self.calibration_quality_data.is_empty() {
            return None;
        }
        let sum: f64 = self
            .calibration_quality_data
            .iter()
            .map(|(pred, actual)| (pred - actual).abs())
            .sum();
        Some(sum / self.calibration_quality_data.len() as f64)
    }

    /// Update ensemble phase based on current tracking.
    fn update_phase(&mut self) {
        match self.phase {
            EnsemblePhase::Shadow => {
                if self.deep_tracker.len() >= SHADOW_MIN_PREDICTIONS {
                    let acc = self
                        .deep_tracker
                        .accuracy(SHADOW_MIN_PREDICTIONS)
                        .unwrap_or(0.0);
                    if acc >= PROMOTE_ACCURACY_THRESHOLD {
                        tracing::info!(
                            "Ensemble: promoting deep model from shadow (accuracy={:.1}%)",
                            acc * 100.0
                        );
                        self.phase = EnsemblePhase::Ramping;
                        self.ramp_step = 0;
                    }
                }
            }
            EnsemblePhase::Ramping => {
                self.ramp_step += 1;
                self.deep_weight = MAX_DEEP_WEIGHT * (self.ramp_step as f64 / RAMP_STEPS as f64);
                self.deep_weight = self.deep_weight.min(MAX_DEEP_WEIGHT);

                if self.ramp_step >= RAMP_STEPS {
                    self.phase = EnsemblePhase::Active;
                    tracing::info!(
                        "Ensemble: deep model fully active (weight={:.2})",
                        self.deep_weight
                    );
                }

                // Check for demotion even during ramp
                self.check_demotion();
            }
            EnsemblePhase::Active => {
                self.check_demotion();
            }
        }
    }

    fn check_demotion(&mut self) {
        if self.deep_tracker.len() >= DEMOTE_WINDOW {
            let recent_acc = self.deep_tracker.accuracy(DEMOTE_WINDOW).unwrap_or(0.0);
            if recent_acc < DEMOTE_ACCURACY_THRESHOLD {
                tracing::warn!(
                    "Ensemble: demoting deep model (recent_acc={:.1}%, window={})",
                    recent_acc * 100.0,
                    DEMOTE_WINDOW
                );
                self.phase = EnsemblePhase::Shadow;
                self.deep_weight = 0.0;
                self.ramp_step = 0;
            }
        }
    }

    /// Fit isotonic regression on accumulated calibration data.
    ///
    /// Pool Adjacent Violators (PAV) algorithm — produces a monotonically
    /// non-decreasing mapping from raw predictions to calibrated probabilities.
    fn fit_isotonic(&mut self) {
        let data: Vec<(f64, f64)> = self.calibration_data.iter().copied().collect();
        if data.len() < 10 {
            return;
        }

        self.isotonic_breakpoints = super::calibration::fit_monotonic(&data);
    }

    /// Summary JSON for the dashboard.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "phase": self.phase.as_str(),
            "model_identity": self.model_identity,
            "deep_weight": self.deep_weight,
            "ramp_step": self.ramp_step,
            "deep_accuracy_50": self.deep_tracker.accuracy(50),
            "deep_accuracy_200": self.deep_tracker.accuracy(200),
            "deep_brier_50": self.deep_tracker.brier_score(50),
            "deep_total_samples": self.deep_tracker.len(),
            "calibration_breakpoints": self.isotonic_breakpoints.len(),
            "last_deep_p_up": self.last_deep_p_up,
            "last_blended_p_up": self.last_blended_p_up,
            "last_mc_uncertainty": self.last_mc_uncertainty,
            "last_should_skip": self.last_should_skip,
        })
    }

    // ── Persistence ─────────────────────────────────────────────────────────

    pub fn default_path() -> String {
        std::env::var("MACBOT_ENSEMBLE_PATH").unwrap_or_else(|_| "ensemble_state.json".to_string())
    }

    /// Atomic save: write to .tmp then rename.
    pub fn save_to_disk(&self) {
        let path = Self::default_path();
        if let Err(e) = Self::save_to_disk_at(Path::new(&path), self) {
            tracing::warn!("Failed to save ensemble state to {path}: {e}");
        }
    }

    fn save_to_disk_at(path: &Path, state: &Self) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(state).context("serialize ensemble state")?;
        let tmp = PathBuf::from(format!("{}.tmp", path.display()));

        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)
                .with_context(|| format!("open temp ensemble file {}", tmp.display()))?;
            file.write_all(json.as_bytes())
                .with_context(|| format!("write temp ensemble file {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("sync temp ensemble file {}", tmp.display()))?;
        }

        std::fs::rename(&tmp, path).with_context(|| {
            format!(
                "rename ensemble state from {} to {}",
                tmp.display(),
                path.display()
            )
        })?;

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                let dir = OpenOptions::new()
                    .read(true)
                    .open(parent)
                    .with_context(|| format!("open ensemble parent dir {}", parent.display()))?;
                dir.sync_all()
                    .with_context(|| format!("sync ensemble parent dir {}", parent.display()))?;
            }
        }

        Ok(())
    }

    /// Load from disk, fallback to fresh state.
    pub fn load_or_new() -> Self {
        let path = Self::default_path();
        if !Path::new(&path).exists() {
            return Self::new();
        }
        match std::fs::read_to_string(&path) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                tracing::warn!("Failed to parse ensemble state from {path}: {e}");
                Self::new()
            }),
            Err(_) => Self::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(prefix: &str) -> PathBuf {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}_{uniq}.json"))
    }

    #[test]
    fn test_shadow_to_active() {
        let mut state = EnsembleState::new();
        assert_eq!(state.phase, EnsemblePhase::Shadow);
        assert_eq!(state.deep_weight, 0.0);

        // Simulate 200 correct predictions
        for _ in 0..200 {
            state.record_deep_prediction(0.7, 1.0);
        }

        // Should have promoted to ramping
        assert!(
            state.phase == EnsemblePhase::Ramping || state.phase == EnsemblePhase::Active,
            "should promote after 200 correct predictions"
        );
    }

    #[test]
    fn test_blend_shadow() {
        let state = EnsembleState::new();
        let deep = DeepModelOutput {
            p_up: 0.9,
            magnitude: 0.01,
            volatility: 0.02,
            time_to_move: 0.5,
            mc_uncertainty: None,
            model_version: "test".to_string(),
            model_identity: "test-bytes".to_string(),
        };
        let (blended, skip) = state.blend(0.6, &deep);
        assert_eq!(blended, 0.6); // Shadow = ignore deep
        assert!(!skip);
    }

    #[test]
    fn test_mc_uncertainty_skip() {
        let mut state = EnsembleState::new();
        state.bind_model("test-bytes");
        state.deep_weight = 0.5;
        state.phase = EnsemblePhase::Active;
        let deep = DeepModelOutput {
            p_up: 0.8,
            magnitude: 0.01,
            volatility: 0.05,
            time_to_move: 0.5,
            mc_uncertainty: Some(0.12), // Above threshold
            model_version: "test".to_string(),
            model_identity: "test-bytes".to_string(),
        };
        let (_, skip) = state.blend(0.6, &deep);
        assert!(skip, "should skip when MC uncertainty too high");
    }

    #[test]
    fn test_isotonic_calibration() {
        let mut state = EnsembleState::new();

        // Add calibration data: model predicts 0.3 but actual is 0.5
        for _ in 0..30 {
            state.calibration_data.push_back((0.3, 0.5));
            state.calibration_data.push_back((0.7, 0.8));
        }
        state.fit_isotonic();

        // 0.3 should calibrate closer to 0.5
        let cal = state.calibrate(0.3);
        assert!(
            cal > 0.35,
            "isotonic should push 0.3 toward actual rate ~0.5, got {cal}"
        );
    }

    #[test]
    fn model_replacement_clears_approval_calibration_and_pending_forecasts() {
        let mut state = EnsembleState::new();
        state.bind_model("first-content-sha");
        state.phase = EnsemblePhase::Active;
        state.deep_weight = 0.6;
        state.deep_tracker.record(0.9, 1.0);
        state.isotonic_breakpoints = vec![(0.9, 0.5)];
        state.pending_predictions.insert("market".into(), 0.9);
        assert!(!state.bind_model("first-content-sha"));
        assert_eq!(state.deep_weight, 0.6);
        assert!(state.bind_model("replacement-content-sha"));
        assert_eq!(state.phase, EnsemblePhase::Shadow);
        assert_eq!(state.deep_weight, 0.0);
        assert_eq!(state.deep_tracker.len(), 0);
        assert!(state.isotonic_breakpoints.is_empty());
        assert!(state.pending_predictions.is_empty());
    }

    #[test]
    fn ensemble_ties_are_aggregated_before_monotonic_fitting() {
        let mut first = EnsembleState::new();
        let mut reversed = EnsembleState::new();
        let observations: Vec<_> = (0..20)
            .map(|i| (0.7, if i % 2 == 0 { 1.0 } else { 0.0 }))
            .collect();
        first.calibration_data.extend(observations.iter().copied());
        reversed
            .calibration_data
            .extend(observations.iter().rev().copied());
        first.fit_isotonic();
        reversed.fit_isotonic();
        assert_eq!(first.isotonic_breakpoints, reversed.isotonic_breakpoints);
        assert_eq!(first.isotonic_breakpoints, vec![(0.7, 0.5)]);
        assert_eq!(first.calibrate(0.7), 0.5);
    }

    #[test]
    fn save_to_disk_writes_round_trip_state() {
        let path = temp_path("macbot_ensemble_roundtrip");
        let mut state = EnsembleState::new();
        state.phase = EnsemblePhase::Active;
        state.deep_weight = 0.42;
        state.last_deep_p_up = Some(0.77);

        EnsembleState::save_to_disk_at(&path, &state).expect("save should succeed");

        let loaded = std::fs::read_to_string(&path).expect("saved file should exist");
        let restored: EnsembleState =
            serde_json::from_str(&loaded).expect("saved JSON should parse");
        assert_eq!(restored.phase, EnsemblePhase::Active);
        assert_eq!(restored.deep_weight, 0.42);
        assert_eq!(restored.last_deep_p_up, Some(0.77));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_to_disk_reports_rename_failure() {
        let parent = std::env::temp_dir().join(format!(
            "macbot_ensemble_bad_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&parent).expect("create temp dir");
        let path = parent.join("ensemble_state.json");
        std::fs::create_dir_all(&path).expect("create conflicting target dir");

        let state = EnsembleState::new();
        let err = EnsembleState::save_to_disk_at(&path, &state).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rename ensemble state"));

        let _ = std::fs::remove_dir_all(&path);
        let _ = std::fs::remove_dir_all(&parent);
    }
}
