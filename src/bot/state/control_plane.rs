use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::RwLock;

use crate::bot::idempotency::IdempotencyEntry;
use crate::config::Config;

pub struct StreamTokenEntry {
    pub ts: Instant,
    pub client_ip: Option<String>,
    pub auth_fingerprint: [u8; 32],
}

pub struct ControlPlaneState {
    pub run_id: String,
    pub config_path: String,
    pub config: RwLock<Config>,
    pub idempotency_cache: DashMap<String, IdempotencyEntry>,
    pub stream_tokens: DashMap<String, StreamTokenEntry>,
}

impl ControlPlaneState {
    pub fn new(run_id: String, config: Config) -> Self {
        Self {
            run_id,
            config_path: std::env::var("MACBOT_CONFIG").unwrap_or_else(|_| "config.json".into()),
            config: RwLock::new(config),
            idempotency_cache: DashMap::new(),
            stream_tokens: DashMap::new(),
        }
    }
}
