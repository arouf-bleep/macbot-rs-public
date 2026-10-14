use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::{Mutex, RwLock};

use super::{
    AggressorCache, CandlesCache, CoingeckoCache, FgCache, FinnhubCache, FuturesCache,
    LunarcrushCache, MlCandlesCache, PcrCache,
};

pub struct MarketState {
    pub candles_cache: Mutex<Option<CandlesCache>>,
    pub ml_candles_cache: Mutex<Option<MlCandlesCache>>,
    pub futures_cache: Mutex<Option<FuturesCache>>,
    pub pcr_cache: Mutex<Option<PcrCache>>,
    pub fg_cache: Mutex<Option<FgCache>>,
    pub aggressor_cache: Mutex<Option<AggressorCache>>,
    pub coingecko_cache: Mutex<Option<CoingeckoCache>>,
    pub finnhub_cache: Mutex<Option<FinnhubCache>>,
    pub lunarcrush_cache: Mutex<Option<LunarcrushCache>>,
    pub up_token_cache: DashMap<String, (Instant, String)>,
    pub no_token_cache: DashMap<String, (Instant, String)>,
    pub btc_start_cache: DashMap<String, (Instant, f64, String)>,
    pub live_btc_px: RwLock<(f64, Instant)>,
    pub l2_sticky_analysis_market: RwLock<Option<StickyAnalysisMarket>>,
}

/// The analysis market the L2 feed is pinned to, with the two times that
/// decide when it lets go. A market adopted moments ago is held against a
/// different proposal so the subscription does not flap; a market the
/// analysis has merely stopped naming (a failed fetch) is held for the
/// window from the last time it was named. Keeping one timestamp for both
/// meant refreshing it on every agreement, which turned "hold a fresh
/// market" into "hold every market three minutes after it rolls".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StickyAnalysisMarket {
    pub market_id: String,
    pub adopted_at: Instant,
    pub last_seen_at: Instant,
}

impl Default for MarketState {
    fn default() -> Self {
        Self {
            candles_cache: Mutex::new(None),
            ml_candles_cache: Mutex::new(None),
            futures_cache: Mutex::new(None),
            pcr_cache: Mutex::new(None),
            fg_cache: Mutex::new(None),
            aggressor_cache: Mutex::new(None),
            coingecko_cache: Mutex::new(None),
            finnhub_cache: Mutex::new(None),
            lunarcrush_cache: Mutex::new(None),
            up_token_cache: DashMap::new(),
            no_token_cache: DashMap::new(),
            btc_start_cache: DashMap::new(),
            live_btc_px: RwLock::new((0.0, Instant::now())),
            l2_sticky_analysis_market: RwLock::new(None),
        }
    }
}
