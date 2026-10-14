//! Hot-reload watcher for ONNX model files.
//!
//! Polls a directory every 30 seconds. When a new or updated `.onnx` file
//! appears (via the `latest.onnx` symlink convention), loads it and swaps
//! the shared model behind `Arc<RwLock>` with zero downtime.

use super::ensemble::EnsembleState;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

use tracing::{info, warn};

use super::deep_model::{DeepModel, SharedDeepModel};

const POLL_INTERVAL_SECS: u64 = 30;
const MODEL_FILENAME: &str = "latest.onnx";

/// Spawn a background task that watches `models_dir` for ONNX model updates.
pub fn spawn_model_watcher(
    models_dir: String,
    shared: SharedDeepModel,
    ensemble: Arc<RwLock<EnsembleState>>,
) {
    crate::task_monitor::spawn_monitored_task("model_watcher", async move {
        let dir = PathBuf::from(&models_dir);
        let model_path = dir.join(MODEL_FILENAME);
        let mut last_mtime: Option<SystemTime> = None;

        // Try initial load
        if model_path.exists() {
            match DeepModel::load(model_path.to_str().unwrap_or("")) {
                Ok(model) => {
                    last_mtime = file_mtime(&model_path);
                    info!(
                        "Model watcher: initial load successful — version={}",
                        model.version
                    );
                    let mut guard = shared.write().await;
                    let mut state = ensemble.write().await;
                    if state.bind_model(&model.identity) {
                        state.save_to_disk();
                    }
                    *guard = Some(model);
                }
                Err(e) => {
                    warn!("Model watcher: initial load failed: {e:#}");
                }
            }
        } else {
            info!(
                "Model watcher: no model at {} — waiting for deployment",
                model_path.display()
            );
        }

        loop {
            tokio::time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;

            if !model_path.exists() {
                continue;
            }

            let current_mtime = file_mtime(&model_path);
            if current_mtime == last_mtime {
                continue;
            }

            // New model detected — load it
            info!("Model watcher: change detected at {}", model_path.display());

            match DeepModel::load(model_path.to_str().unwrap_or("")) {
                Ok(model) => {
                    last_mtime = current_mtime;
                    info!("Model watcher: hot-swapped to version={}", model.version);
                    let mut guard = shared.write().await;
                    let mut state = ensemble.write().await;
                    if state.bind_model(&model.identity) {
                        state.save_to_disk();
                    }
                    *guard = Some(model);
                }
                Err(e) => {
                    warn!("Model watcher: failed to load new model: {e:#}");
                    // Keep the old model — don't swap on failure
                }
            }
        }
    });
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}
