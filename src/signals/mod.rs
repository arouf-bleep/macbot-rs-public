pub mod aggressor;
pub mod btc_final_momentum;
pub mod counterfactual;
pub mod cross_market;
pub mod deribit;
pub mod divergence;
pub mod fear_greed;
pub mod fetchers;
pub mod funding;
pub mod granger;
pub mod hawkes;
pub mod heiken_ashi;
pub mod kyle_lambda;
pub mod liquidation;
pub mod lmsr;
pub mod lunarcrush;
pub mod macd;
pub mod microstructure;
pub mod news_nlp;
pub mod oi_momentum;
pub mod options_advanced;
pub mod orderbook;
pub mod phase;
pub mod polymarket_flow;
pub mod price_bus;
pub mod regime;
pub mod roll;
pub mod round_number;
pub mod rsi;
pub mod temporal;
pub mod tick_velocity;
pub mod volatility;
pub mod vwap;
pub mod whale;

pub use aggressor::{compute_aggressor_ratio, AggressorData};
#[allow(unused_imports)]
pub use btc_final_momentum::{compute_btc_final_momentum, BtcFinalMomentumData};
#[allow(unused_imports)]
pub use counterfactual::{ScenarioAction, ScenarioTree};
#[allow(unused_imports)]
pub use cross_market::{
    compute_cross_market_signal, fetch_cross_market_resolutions, CrossMarketData,
};
pub use deribit::{compute_deribit_pcr, PcrData};
pub use divergence::{compute_price_divergence, DivergenceData};
pub use fear_greed::{compute_fear_greed, FearGreedData};
pub use funding::{compute_funding_signal, FundingData};
#[allow(unused_imports)]
pub use granger::{GrangerManager, GrangerWeights};
#[allow(unused_imports)]
pub use hawkes::{compute_hawkes_flow, HawkesFlowData};
pub use heiken_ashi::{compute_ha, HaData};
#[allow(unused_imports)]
pub use kyle_lambda::{compute_kyle_signal, KyleLambdaTracker};
#[allow(unused_imports)]
pub use liquidation::{compute_liquidation_signal, LiquidationData, LiquidationTracker};
pub use lunarcrush::{compute_lunarcrush_sentiment, LunarcrushSentimentData};
pub use macd::{compute_macd, MacdData};
#[allow(unused_imports)]
pub use microstructure::{compute_microstructure, compute_vpin, MicrostructureData};
#[allow(unused_imports)]
pub use news_nlp::{compute_news_signal, fetch_crypto_news, EventType, NewsClassifier, NewsSignal};
pub use oi_momentum::{compute_oi_momentum, OiMomentumData};
#[allow(unused_imports)]
pub use options_advanced::{compute_options_advanced, OptionsAdvancedData};
pub use orderbook::{compute_orderbook_imbalance, OBIStabilityTracker, ObData};
pub use phase::{compute_phase_decay, PhaseData};
#[allow(unused_imports)]
pub use polymarket_flow::{compute_polymarket_flow, PolymarketFlowData};
#[allow(unused_imports)]
pub use price_bus::{shared_price_bus, spawn_price_bus, PriceBus};
pub use regime::compute_regime;
#[allow(unused_imports)]
pub use roll::{compute_roll_microstructure, RollMicrostructureData};
pub use round_number::compute_distance_from_round;
pub use rsi::{compute_rsi, RsiData};
pub use temporal::{compute_day_of_week, compute_hour_of_day};
pub use tick_velocity::{TickVelData, TickVelocityTracker};
pub use volatility::compute_recent_volatility;
pub use vwap::{compute_vwap, VwapData};
#[allow(unused_imports)]
pub use whale::{compute_whale_signal, WhaleData};

/// A single OHLCV candle (matches Binance kline format).
#[derive(Debug, Clone)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}
