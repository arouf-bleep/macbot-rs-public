use std::time::{Instant, SystemTime};

use serde_json::Value;
use tokio::sync::RwLock;

pub struct AnalysisState {
    pub last_analysis: RwLock<Value>,
    pub last_analysis_at: RwLock<Instant>,
    pub signal_weights_file: RwLock<Option<(String, SystemTime)>>,
}

impl Default for AnalysisState {
    fn default() -> Self {
        Self {
            last_analysis: RwLock::new(Value::Null),
            last_analysis_at: RwLock::new(Instant::now()),
            signal_weights_file: RwLock::new(None),
        }
    }
}
