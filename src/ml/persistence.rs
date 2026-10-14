use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io::Write;
use std::path::Path;

use super::model::{OnlineLogisticRegression, NUM_FEATURES, PREV_NUM_FEATURES};
use super::normalizer::FeatureNormalizer;
use super::tracker::RollingAccuracyTracker;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MlSnapshot {
    pub model: OnlineLogisticRegression,
    pub normalizer: FeatureNormalizer,
    pub tracker: RollingAccuracyTracker,
    pub version: u32,
    pub last_updated: String,
    /// Missing on legacy snapshots: their already learned rows cannot be identified.
    #[serde(default)]
    pub learned_outcomes: Option<BTreeSet<String>>,
    #[serde(default)]
    pub calibrator: super::calibration::IsotonicCalibrator,
}

const CURRENT_VERSION: u32 = 4;

/// Feature count from v1 format (original 14 features).
const V1_NUM_FEATURES: usize = 14;

pub fn default_path() -> String {
    std::env::var("MACBOT_ML_WEIGHTS_PATH").unwrap_or_else(|_| "ml_weights.json".to_string())
}

/// Atomic save: write to .tmp then rename.
pub fn save(snapshot: &MlSnapshot, path: &str) -> anyhow::Result<()> {
    let tmp = format!("{path}.tmp");
    let json = serde_json::to_string_pretty(snapshot)?;
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(json.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    let parent = Path::new(path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Load ML state from disk. Returns None if file missing.
/// Handles migration from older feature counts:
///   v1 (14 features) → v3 (34 features)
///   v2 (24 features) → v3 (34 features)
pub fn load(path: &str) -> Option<MlSnapshot> {
    if !Path::new(path).exists() {
        return None;
    }
    let data = std::fs::read_to_string(path).ok()?;

    // Try direct deserialization first (current 34-feature format)
    if let Ok(snap) = serde_json::from_str::<MlSnapshot>(&data) {
        // Check if the weights array actually has the right length
        if snap.model.weights.len() == NUM_FEATURES {
            return Some(snap);
        }
    }

    // Parse as raw JSON and attempt migration
    let mut raw: serde_json::Value = serde_json::from_str(&data).ok()?;

    // Detect source format by inspecting weights array length
    let weights_len = raw
        .pointer("/model/weights")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    let from_size = if weights_len == V1_NUM_FEATURES {
        V1_NUM_FEATURES
    } else if weights_len == PREV_NUM_FEATURES {
        PREV_NUM_FEATURES
    } else {
        tracing::warn!(
            "ML weights have unexpected size {weights_len}, attempting migration to {NUM_FEATURES}"
        );
        weights_len
    };

    tracing::info!(
        "Attempting migration from {from_size}-feature to {NUM_FEATURES}-feature format"
    );

    for field_path in &[
        &["model", "weights"][..],
        &["normalizer", "means"][..],
        &["normalizer", "m2"][..],
    ] {
        migrate_array(&mut raw, field_path, from_size, NUM_FEATURES);
    }

    // Update version
    if let Some(obj) = raw.as_object_mut() {
        obj.insert(
            "version".to_string(),
            serde_json::Value::from(CURRENT_VERSION),
        );
    }

    let migrated: MlSnapshot = serde_json::from_value(raw).ok()?;
    tracing::info!("Successfully migrated ML weights from {from_size} to {NUM_FEATURES} features");
    Some(migrated)
}

/// Extend a JSON array at a nested path to `new_len` by appending zeros.
/// Works for any source length that is smaller than `new_len`.
fn migrate_array(root: &mut serde_json::Value, path: &[&str], _old_len: usize, new_len: usize) {
    let mut current = root as &mut serde_json::Value;
    for &key in path {
        current = match current.get_mut(key) {
            Some(v) => v,
            None => return,
        };
    }
    if let Some(arr) = current.as_array_mut() {
        while arr.len() < new_len {
            arr.push(serde_json::Value::from(0.0));
        }
    }
}

/// Create a snapshot from current state.
pub fn to_snapshot(
    model: &OnlineLogisticRegression,
    normalizer: &FeatureNormalizer,
    tracker: &RollingAccuracyTracker,
    learned_outcomes: &BTreeSet<String>,
    calibrator: &super::calibration::IsotonicCalibrator,
) -> MlSnapshot {
    MlSnapshot {
        model: model.clone(),
        normalizer: normalizer.clone(),
        tracker: tracker.clone(),
        version: CURRENT_VERSION,
        last_updated: chrono::Utc::now().to_rfc3339(),
        learned_outcomes: Some(learned_outcomes.clone()),
        calibrator: calibrator.clone(),
    }
}
