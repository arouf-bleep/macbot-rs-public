use std::collections::{HashMap, HashSet};
use std::time::Instant;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy)]
pub struct CachedTradePrice {
    pub price: f64,
    pub observed_at: Instant,
}

#[derive(Debug, Clone)]
pub struct PendingSimExitEvidence {
    pub order_id: String,
    pub phase: String,
    pub intended_price: f64,
    pub avg_price: f64,
    pub shares: f64,
    pub source: String,
    pub details: String,
}

#[derive(Debug, Clone)]
pub struct PartialCloseIntent {
    pub request_id: u64,
    pub outcome_unknown: bool,
    pub live_order_id: Option<String>,
}

pub struct TradeLifecycleState {
    pub last_trade_prices: RwLock<HashMap<i64, CachedTradePrice>>,
    pub scalp_high_water: RwLock<HashMap<i64, f64>>,
    pub circuit_breaker_pause_until: RwLock<Option<DateTime<Utc>>>,
    /// Tracks trade IDs that are currently in the close pipeline to prevent
    /// concurrent close attempts (TOCTOU race on CLOSING status toggle).
    pub closing_in_progress: RwLock<HashSet<i64>>,
    /// One bounded retry worker per trade; a failed retry must not spawn
    /// another worker when it passes through the execution actor.
    pub fok_retry_in_progress: std::sync::Mutex<HashSet<i64>>,
    pub pending_sim_exit_evidence: RwLock<HashMap<i64, PendingSimExitEvidence>>,
    /// Protects an unresolved partial sale from another automatic guard exit.
    /// The actor releases only the exact request after a known completion.
    pub partial_close_intents: RwLock<HashMap<i64, PartialCloseIntent>>,
    pub partial_close_sequence: std::sync::atomic::AtomicU64,
}

impl Default for TradeLifecycleState {
    fn default() -> Self {
        Self {
            last_trade_prices: RwLock::new(HashMap::new()),
            scalp_high_water: RwLock::new(HashMap::new()),
            circuit_breaker_pause_until: RwLock::new(None),
            closing_in_progress: RwLock::new(HashSet::new()),
            fok_retry_in_progress: std::sync::Mutex::new(HashSet::new()),
            pending_sim_exit_evidence: RwLock::new(HashMap::new()),
            partial_close_intents: RwLock::new(HashMap::new()),
            partial_close_sequence: std::sync::atomic::AtomicU64::new(0),
        }
    }
}
