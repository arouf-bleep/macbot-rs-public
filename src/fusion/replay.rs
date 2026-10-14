//! Exact inputs to the production fusion calculation (before model blending).
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FusionInput {
    pub orderbook: ObData,
    pub tick_vel: TickVelData,
    pub vwap: VwapData,
    pub rsi: RsiData,
    pub macd: MacdData,
    pub ha: HaData,
    pub divergence: DivergenceData,
    pub funding: FundingData,
    pub regime: String,
    pub phase: PhaseData,
    pub market_price: f64,
    pub pcr: Option<PcrData>,
    pub fear_greed: Option<FearGreedData>,
    pub oi_momentum: Option<OiMomentumData>,
    pub aggressor: Option<AggressorData>,
    pub lunarcrush: Option<LunarcrushSentimentData>,
    pub calibration: Option<FusionCalibration>,
    pub weights: Option<HashMap<String, f64>>,
    pub liquidity: f64,
    pub trade_amount: f64,
    pub obi_stable: bool,
    pub enrichment: EnrichmentData,
}

impl FusionInput {
    pub(crate) fn evaluate(&self) -> FusionResult {
        fuse_signals_core(FusionInputs {
            orderbook: &self.orderbook,
            tick_vel: &self.tick_vel,
            vwap: &self.vwap,
            rsi: &self.rsi,
            macd: &self.macd,
            ha: &self.ha,
            divergence: &self.divergence,
            funding: &self.funding,
            regime: &self.regime,
            phase: &self.phase,
            market_price: self.market_price,
            pcr: self.pcr.as_ref(),
            fear_greed: self.fear_greed.as_ref(),
            oi_momentum: self.oi_momentum.as_ref(),
            aggressor: self.aggressor.as_ref(),
            lunarcrush: self.lunarcrush.as_ref(),
            calibration: self.calibration.as_ref(),
            weights: self.weights.clone(),
            liquidity: self.liquidity,
            trade_amount: self.trade_amount,
            obi_stable: self.obi_stable,
            enrichment: &self.enrichment,
        })
    }
}
