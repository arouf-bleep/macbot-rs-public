use std::time::Instant;

use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock};

use crate::bot::sleep_schedule::SleepSchedule;
use crate::signals::{GrangerManager, KyleLambdaTracker};

use super::NewsCache;

pub struct StrategyServices {
    pub sleep_schedule: RwLock<SleepSchedule>,
    pub btc_px_30s_ago: RwLock<(f64, Instant)>,
    pub granger: Mutex<GrangerManager>,
    pub kyle_tracker: Mutex<KyleLambdaTracker>,
    pub resolution_sniper: super::super::resolution_sniper::ResolutionSniper,
    pub smart_order_router: super::super::smart_order::SmartOrderRouter,
    pub news_cache: Mutex<Option<NewsCache>>,
}

impl StrategyServices {
    pub async fn new(_pool: &SqlitePool) -> Self {
        Self {
            sleep_schedule: RwLock::new(SleepSchedule::new()),
            btc_px_30s_ago: RwLock::new((0.0, Instant::now())),
            granger: Mutex::new(GrangerManager::default()),
            kyle_tracker: Mutex::new(KyleLambdaTracker::new(200)),
            resolution_sniper: super::super::resolution_sniper::ResolutionSniper::new(),
            smart_order_router: super::super::smart_order::SmartOrderRouter::new(),
            news_cache: Mutex::new(None),
        }
    }
}
