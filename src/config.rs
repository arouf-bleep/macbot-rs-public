use crate::domain::{BotMode, ExecutionMode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// Diagnostics worth keeping for a month; the record itself is never pruned.
fn default_operational_retention_days() -> u32 {
    30
}

fn default_analysis_stale_seconds() -> u64 {
    45
}
fn default_buy_no_trend_up_max_p_up() -> f64 {
    0.35
}
fn default_maker_buy_no_max_p_up() -> f64 {
    0.40
}
fn default_false() -> bool {
    false
}

fn default_true() -> bool {
    true
}
fn default_weight_zero() -> f64 {
    0.0
}
fn default_weight_orderbook_imbalance() -> f64 {
    0.2069
}
fn default_weight_tick_velocity() -> f64 {
    0.1724
}
fn default_weight_vwap_confluence() -> f64 {
    0.1207
}
fn default_weight_price_divergence() -> f64 {
    0.1034
}
fn default_weight_deribit_pcr() -> f64 {
    0.0690
}
fn default_weight_rsi_macd_ha() -> f64 {
    0.0517
}
fn default_weight_fear_greed() -> f64 {
    0.0259
}
fn default_weight_funding_rate() -> f64 {
    0.0086
}
fn default_weight_oi_momentum() -> f64 {
    0.0345
}
fn default_weight_aggressor_ratio() -> f64 {
    0.0517
}
fn default_weight_lunarcrush_sentiment() -> f64 {
    0.0172
}
fn default_weight_hour_of_day() -> f64 {
    0.010
}
fn default_weight_day_of_week() -> f64 {
    0.010
}
fn default_weight_distance_round_1000() -> f64 {
    0.015
}
fn default_weight_distance_round_500() -> f64 {
    0.010
}
fn default_weight_volatility_10() -> f64 {
    0.020
}
fn default_weight_volatility_30() -> f64 {
    0.020
}
fn default_weight_window_momentum() -> f64 {
    0.030
}
fn default_weight_long_liq_volume() -> f64 {
    0.020
}
fn default_weight_short_liq_volume() -> f64 {
    0.020
}
fn default_weight_depth_imbalance_5() -> f64 {
    0.012
}
fn default_weight_polymarket_flow() -> f64 {
    0.040
}
fn default_weight_vpin_microstructure() -> f64 {
    0.035
}
fn default_weight_options_advanced() -> f64 {
    0.020
}
fn default_weight_whale_flow() -> f64 {
    0.015
}
fn default_weight_cross_market() -> f64 {
    0.025
}
fn default_weight_latency_arb() -> f64 {
    0.010
}
fn default_weight_kyle_lambda() -> f64 {
    0.020
}
fn default_weight_btc_final_momentum() -> f64 {
    0.030
}
fn default_weight_hawkes_flow() -> f64 {
    0.015
}
fn default_priority_min_gwei() -> f64 {
    35.0
}
fn default_priority_max_gwei() -> f64 {
    180.0
}
fn default_priority_trigger_pct() -> f64 {
    0.03
}
fn default_priority_mult() -> f64 {
    2.0
}
fn default_merge_min_edge_usd() -> f64 {
    0.10
}
fn default_merge_gas_usd() -> f64 {
    0.03
}
fn default_merge_mismatch_pct() -> f64 {
    0.02
}
fn default_api_max_skew_seconds() -> u64 {
    30
}
fn default_idempotency_ttl_seconds() -> u64 {
    600
}
fn default_idempotency_max_entries() -> usize {
    10_000
}
fn default_execution_mode() -> String {
    "hybrid".to_string()
}
fn default_scalp_tp() -> f64 {
    0.15
} // close scalp when +15% gain on entry price
fn default_scalp_sl() -> f64 {
    0.08
} // stop loss when -8% loss on entry price
fn default_scalp_stake_pct() -> f64 {
    0.05
} // 5% of balance per scalp
fn default_scalp_min_confidence() -> f64 {
    0.65
}
fn default_tp_sl_fallback_max_age_seconds() -> u64 {
    15
}
fn default_scalp_re_entry() -> bool {
    true
}
fn default_min_trade_usd() -> f64 {
    0.25
}
fn default_trade_gas_usd() -> f64 {
    0.02
}
fn default_edge_safety_buffer() -> f64 {
    0.0015
} // 0.15%
fn default_hybrid_stand_down() -> bool {
    true
}
fn default_hybrid_stand_down_min_closed() -> usize {
    30
}
fn default_hybrid_stand_down_min_win_rate() -> f64 {
    0.52
}
fn default_maker_spread_floor_bps() -> f64 {
    30.0
}
fn default_maker_spread_ceiling_bps() -> f64 {
    250.0
}
fn default_maker_skew_per_usd_bps() -> f64 {
    0.25
}
fn default_maker_max_skew_bps() -> f64 {
    200.0
}
fn default_maker_drift_threshold_bps() -> f64 {
    15.0
}
fn default_maker_min_quote_lifetime_ms() -> u64 {
    800
}
fn default_maker_replace_cooldown_ms() -> u64 {
    250
}
fn default_maker_fill_cooldown_ms() -> u64 {
    15_000
}
fn default_sim_order_latency_ms() -> u64 {
    250
}
fn default_sim_available_depth_fraction() -> f64 {
    0.25
}
fn default_maker_sim_queue_ahead_shares() -> f64 {
    5.0
}
fn default_maker_sim_max_fill_fraction() -> f64 {
    0.25
}
fn default_maker_candidate_min_remaining_seconds() -> u64 {
    300
}
/// Polymarket quotes in one-cent ticks, and the maker measures spread as
/// `(ask - bid) * 10_000`, so a one-tick book is exactly 100 bps and a two-tick
/// book exactly 200. The old default of 160 sat between them and admitted only
/// one-tick books, rejecting most of what the venue actually shows — 15,492
/// `market_quality(spread 200.0bps > max 160.0bps)` rows in the production
/// outbox were that threshold, not a stuck feed.
pub const VENUE_TICK_BPS: f64 = 100.0;

fn default_maker_market_quality_max_spread_bps() -> f64 {
    2.0 * VENUE_TICK_BPS
}

/// A spread threshold that falls strictly between tick multiples behaves like
/// the tick below it, and says so nowhere. Returns the message to log.
pub fn spread_threshold_between_ticks(max_spread_bps: f64) -> Option<String> {
    let ticks = max_spread_bps / VENUE_TICK_BPS;
    if (ticks - ticks.round()).abs() < 1e-9 {
        return None;
    }
    Some(format!(
        "maker_market_quality_max_spread_bps={max_spread_bps:.0} is not a whole number of \
         {VENUE_TICK_BPS:.0}bps ticks; the venue cannot show a spread between {:.0} and {:.0}, \
         so this behaves as {:.0}",
        ticks.floor() * VENUE_TICK_BPS,
        ticks.ceil() * VENUE_TICK_BPS,
        ticks.floor() * VENUE_TICK_BPS,
    ))
}
fn default_maker_max_replaces_per_min() -> u32 {
    120
}
fn default_maker_quote_notional_usd() -> f64 {
    1.0
}
fn default_maker_max_inventory_notional_usd() -> f64 {
    5.0
}
fn default_maker_live_quoting() -> bool {
    true
}
fn default_maker_backwardation_guard_pct() -> f64 {
    -0.02
}
fn default_maker_backwardation_widen_bps() -> f64 {
    0.0
}
fn default_maker_backwardation_min_target_spread_bps() -> f64 {
    0.0
}
fn default_maker_buy_no_max_entry_price() -> f64 {
    0.78
}
fn default_maker_buy_no_min_edge() -> f64 {
    0.06
}
fn default_maker_roll_toxic_min_confidence() -> f64 {
    0.15
}
fn default_maker_roll_toxic_min_signal() -> f64 {
    0.05
}
fn default_maker_roll_toxic_min_sample_count() -> usize {
    6
}
fn default_live_entry_order_timeout_secs() -> u64 {
    15
}
fn default_macro_event_guard_minutes_before() -> u64 {
    15
}
fn default_macro_event_guard_minutes_after() -> u64 {
    5
}
fn default_macro_event_calendar_path() -> String {
    "ops/macro-events.json".to_string()
}
fn default_fusion_logit_scale() -> f64 {
    2.40
}
fn default_fusion_min_score() -> f64 {
    38.0
}
fn default_fusion_fee_skip_band() -> f64 {
    0.12
}
fn default_fusion_min_net_ev() -> f64 {
    0.005
}
fn default_fusion_min_weighted_abs() -> f64 {
    0.02
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeploymentProfile {
    Relaxed,
    Shadow,
    Canary,
    Live,
}

impl DeploymentProfile {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "relaxed" | "dev" | "simulation" => Some(Self::Relaxed),
            "shadow" => Some(Self::Shadow),
            "canary" => Some(Self::Canary),
            "live" | "prod" | "production" => Some(Self::Live),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub initial_balance: f64,
    pub trade_amount: f64,
    pub market_filter: MarketFilter,
    pub check_interval: u64, // seconds
    #[serde(default = "default_analysis_stale_seconds")]
    pub analysis_stale_seconds: u64,
    /// Days of `order_outbox`, `analysis_snapshots` and `trade_rejections` to
    /// keep. 0 disables the sweep. Never touches `trades` or `trade_fills`.
    #[serde(default = "default_operational_retention_days")]
    pub operational_retention_days: u32,
    pub signal_refresh_interval: u64,
    pub strategy: Strategy,
    pub mode: String, // "live" | "simulation" | "paper"
    #[serde(default = "default_execution_mode")]
    pub execution_mode: String, // "taker" | "maker" | "hybrid"
    #[serde(default)]
    pub dashboard_session_start_trade_id: u64,
    #[serde(default)]
    pub dashboard_session_start_at: String,
    pub auto_trading: bool,
    pub price_refresh_rate: u64,
    pub take_profit: bool,
    pub take_profit_threshold: f64,
    pub stop_loss: bool,
    pub stop_loss_threshold: f64,
    #[serde(default = "default_tp_sl_fallback_max_age_seconds")]
    pub tp_sl_fallback_max_age_seconds: u64,
    pub min_entry_price: f64,
    pub max_entry_price: f64,
    pub hail_mary: bool,
    pub hail_mary_stake: f64,
    pub hail_mary_max_price: f64,
    pub hail_mary_min_btc_change: f64,
    pub kelly_sizing: bool,
    pub kelly_max_mult: f64,
    pub live_confidence_cap: f64,
    pub min_liquidity: f64,
    #[serde(default)]
    pub regime_allowlist: Vec<String>,
    #[serde(default)]
    pub session_allowlist: Vec<String>,
    #[serde(default)]
    pub maker_regime_allowlist: Vec<String>,
    #[serde(default)]
    pub maker_session_allowlist: Vec<String>,
    #[serde(default)]
    pub blocked_hours_utc: Vec<u32>,
    #[serde(default)]
    pub min_confidence_margin: f64,
    #[serde(default = "default_false")]
    pub buy_no_trend_up_guard_enabled: bool,
    #[serde(default = "default_true")]
    pub buy_no_trend_up_guard_shadow_only: bool,
    #[serde(default = "default_buy_no_trend_up_max_p_up")]
    pub buy_no_trend_up_max_p_up: f64,
    #[serde(default = "default_false")]
    pub maker_buy_no_guard_enabled: bool,
    #[serde(default = "default_maker_buy_no_max_p_up")]
    pub maker_buy_no_max_p_up: f64,
    #[serde(default = "default_maker_buy_no_max_entry_price")]
    pub maker_buy_no_max_entry_price: f64,
    #[serde(default = "default_maker_buy_no_min_edge")]
    pub maker_buy_no_min_edge: f64,
    pub time_of_day_weight: bool,
    #[serde(default = "default_false")]
    pub macro_event_guard_enabled: bool,
    /// Fetch selected official calendars hourly; failures expire the guard's data.
    #[serde(default)]
    pub macro_event_auto_refresh: bool,
    #[serde(default = "default_macro_event_guard_minutes_before")]
    pub macro_event_guard_minutes_before: u64,
    #[serde(default = "default_macro_event_guard_minutes_after")]
    pub macro_event_guard_minutes_after: u64,
    #[serde(default = "default_macro_event_calendar_path")]
    pub macro_event_calendar_path: String,
    pub circuit_breaker_losses: u32,
    pub partial_take_profit: bool,
    /// Existing paper partial-taking never implicitly opts a live account in.
    #[serde(default)]
    pub live_partial_take_profit: bool,
    pub partial_tp_threshold: f64,
    pub max_open_positions: u32,

    /// Maximum aggregate exposure as a fraction of balance (0.0 = disabled).
    /// E.g., 0.60 means total open notional cannot exceed 60% of current balance.
    #[serde(default)]
    pub max_exposure_pct: f64,

    // ── Scalp strategy ──────────────────────────────────────────────────────
    #[serde(default)]
    pub scalp_enabled: bool,
    #[serde(default = "default_scalp_tp")]
    pub scalp_take_profit_threshold: f64,
    #[serde(default = "default_scalp_sl")]
    pub scalp_stop_loss_threshold: f64,
    #[serde(default = "default_scalp_stake_pct")]
    pub scalp_stake_pct: f64,
    #[serde(default = "default_scalp_min_confidence")]
    pub scalp_min_confidence: f64,
    #[serde(default = "default_scalp_re_entry")]
    pub scalp_re_entry: bool,
    /// When true, scalps use limit orders (maker) instead of taker orders.
    /// Saves ~2-4 cents per round trip in fees.
    #[serde(default)]
    pub scalp_maker_only: bool,
    pub max_slippage: f64,
    pub max_spread: f64,
    pub min_depth_usd: f64,
    pub stake_pct: f64,
    pub min_stake: f64,
    pub max_stake: f64,
    #[serde(default = "default_min_trade_usd")]
    pub min_trade_usd: f64,
    #[serde(default = "default_trade_gas_usd")]
    pub trade_gas_usd: f64,
    #[serde(default = "default_edge_safety_buffer")]
    pub edge_safety_buffer: f64,
    #[serde(default = "default_hybrid_stand_down")]
    pub hybrid_stand_down: bool,
    #[serde(default = "default_hybrid_stand_down_min_closed")]
    pub hybrid_stand_down_min_closed: usize,
    #[serde(default = "default_hybrid_stand_down_min_win_rate")]
    pub hybrid_stand_down_min_win_rate: f64,
    pub drawdown_halt_daily_pct: f64,
    pub drawdown_halt_ath_pct: f64,
    pub daily_loss_limit: f64,
    pub signal_weights: SignalWeights,
    #[serde(default)]
    pub fusion_calibration: FusionCalibration,
    pub kelly_fraction: f64,
    pub pcr_enabled: bool,
    pub fear_greed_enabled: bool,
    #[serde(default = "default_true")]
    pub funding_rate_enabled: bool,
    #[serde(default = "default_false")]
    pub oi_momentum_enabled: bool,
    #[serde(default = "default_false")]
    pub aggressor_ratio_enabled: bool,
    #[serde(default = "default_false")]
    pub lunarcrush_sentiment_enabled: bool,
    #[serde(default)]
    pub finnhub_api_key: String,
    #[serde(default)]
    pub lunarcrush_api_key: String,
    pub limit_order_enabled: bool,
    #[serde(default)]
    pub private_rpc_url: String,
    #[serde(default = "default_false")]
    pub shadow_trading: bool,
    #[serde(default = "default_false")]
    pub sim_live_execution: bool,
    #[serde(default = "default_false")]
    pub shadow_live_parity: bool,
    #[serde(default = "default_true")]
    pub analysis_snapshot_enabled: bool,
    #[serde(default)]
    pub signal_weights_path: String,
    #[serde(default = "default_false")]
    pub auth_required: bool,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub api_hmac_secret: String,
    #[serde(default = "default_api_max_skew_seconds")]
    pub api_max_skew_seconds: u64,
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
    #[serde(default = "default_idempotency_ttl_seconds")]
    pub idempotency_ttl_seconds: u64,
    #[serde(default = "default_idempotency_max_entries")]
    pub idempotency_max_entries: usize,
    #[serde(default = "default_priority_min_gwei")]
    pub eip1559_min_priority_gwei: f64,
    #[serde(default = "default_priority_max_gwei")]
    pub eip1559_max_priority_gwei: f64,
    #[serde(default = "default_priority_trigger_pct")]
    pub eip1559_spread_trigger_pct: f64,
    #[serde(default = "default_priority_mult")]
    pub eip1559_priority_multiplier: f64,
    #[serde(default = "default_false")]
    pub auto_merge_rebalancer: bool,
    #[serde(default = "default_merge_min_edge_usd")]
    pub merge_min_edge_usd: f64,
    #[serde(default = "default_merge_gas_usd")]
    pub merge_gas_usd: f64,
    #[serde(default = "default_merge_mismatch_pct")]
    pub merge_max_share_mismatch_pct: f64,
    pub min_ev: f64,
    pub expiry_guard_seconds: u64,
    #[serde(default = "default_maker_spread_floor_bps")]
    pub maker_spread_floor_bps: f64,
    #[serde(default = "default_maker_spread_ceiling_bps")]
    pub maker_spread_ceiling_bps: f64,
    #[serde(default = "default_maker_skew_per_usd_bps")]
    pub maker_skew_per_usd_bps: f64,
    #[serde(default = "default_maker_max_skew_bps")]
    pub maker_max_skew_bps: f64,
    #[serde(default = "default_maker_drift_threshold_bps")]
    pub maker_drift_threshold_bps: f64,
    #[serde(default = "default_maker_min_quote_lifetime_ms")]
    pub maker_min_quote_lifetime_ms: u64,
    #[serde(default = "default_maker_replace_cooldown_ms")]
    pub maker_replace_cooldown_ms: u64,
    #[serde(default = "default_maker_fill_cooldown_ms")]
    pub maker_fill_cooldown_ms: u64,
    #[serde(default = "default_maker_candidate_min_remaining_seconds")]
    pub maker_candidate_min_remaining_seconds: u64,
    #[serde(default = "default_maker_market_quality_max_spread_bps")]
    pub maker_market_quality_max_spread_bps: f64,
    /// Hold what the maker buys instead of selling it back on the next uptick.
    ///
    /// Off, the maker is a market maker: it bids both tokens, and the moment a
    /// bid fills it posts an ask half a spread above the *current* price and
    /// sells on the first uptick — never knowing what it paid. On a fifteen
    /// minute binary that trends to 0 or 1 by construction, that bought on the
    /// way down and sold on the bounce: 46 such trades in the production
    /// record averaged -$0.03, and the eight positions it failed to flatten
    /// before resolution averaged +$0.33. On, the maker bids only the side the
    /// model favours, never posts the flatten ask, and the position is closed
    /// by take-profit, stop-loss, a strong model reversal, or resolution —
    /// the exits that were positive in the record. The passive bid is kept:
    /// it is the cheapest way in, not the way out.
    #[serde(default)]
    pub maker_hold_positions: bool,
    /// Fill a simulated maker quote when the market merely *touches* it.
    ///
    /// Off, a resting bid fills only when the best ask trades *through* it —
    /// strictly below — because a real order sits behind everything already
    /// resting at that price, and a level that is touched and not consumed
    /// fills nobody at the back of the queue. On, the quote fills the instant
    /// the ask reaches it, for its full size, first in line. That is how every
    /// maker fill in the production record was granted, and it is the single
    /// largest optimism in the simulation. Off by default; on is for comparing
    /// against the old record, not for judging the strategy.
    #[serde(default)]
    pub maker_sim_fill_on_touch: bool,
    /// Paper order admission delay. Taker fills use a fresh book after this
    /// delay; maker opportunities before admission are discarded.
    #[serde(default = "default_sim_order_latency_ms")]
    pub sim_order_latency_ms: u64,
    /// Fraction of observed liquidity the paper simulator may consume.
    /// This is a conservative assumption, not a calibrated venue estimate.
    #[serde(default = "default_sim_available_depth_fraction")]
    pub sim_available_depth_fraction: f64,
    /// Shares assumed ahead of a new paper maker quote. Contra-side depth
    /// changes are only an opportunity proxy, never proof of actual trades.
    #[serde(default = "default_maker_sim_queue_ahead_shares")]
    pub maker_sim_queue_ahead_shares: f64,
    /// Maximum fraction of the initial maker quote filled on one distinct
    /// eligible book update. The unfilled remainder stays resting.
    #[serde(default = "default_maker_sim_max_fill_fraction")]
    pub maker_sim_max_fill_fraction: f64,
    #[serde(default = "default_maker_max_replaces_per_min")]
    pub maker_max_replaces_per_min: u32,
    #[serde(default = "default_maker_quote_notional_usd")]
    pub maker_quote_notional_usd: f64,
    #[serde(default = "default_maker_max_inventory_notional_usd")]
    pub maker_max_inventory_notional_usd: f64,
    #[serde(default = "default_maker_live_quoting")]
    pub maker_live_quoting: bool,
    #[serde(default = "default_false")]
    pub maker_backwardation_guard_enabled: bool,
    #[serde(default = "default_true")]
    pub maker_backwardation_guard_shadow_only: bool,
    #[serde(default = "default_maker_backwardation_guard_pct")]
    pub maker_backwardation_guard_pct: f64,
    #[serde(default = "default_maker_backwardation_widen_bps")]
    pub maker_backwardation_widen_bps: f64,
    #[serde(default = "default_maker_backwardation_min_target_spread_bps")]
    pub maker_backwardation_min_target_spread_bps: f64,
    #[serde(default = "default_false")]
    pub maker_roll_toxic_guard_enabled: bool,
    #[serde(default = "default_true")]
    pub maker_roll_toxic_guard_shadow_only: bool,
    #[serde(default = "default_maker_roll_toxic_min_confidence")]
    pub maker_roll_toxic_min_confidence: f64,
    #[serde(default = "default_maker_roll_toxic_min_signal")]
    pub maker_roll_toxic_min_signal: f64,
    #[serde(default = "default_maker_roll_toxic_min_sample_count")]
    pub maker_roll_toxic_min_sample_count: usize,
    #[serde(default = "default_live_entry_order_timeout_secs")]
    pub live_entry_order_timeout_secs: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct FusionCalibration {
    #[serde(default = "default_fusion_logit_scale")]
    pub logit_scale: f64,
    #[serde(default = "default_fusion_min_score")]
    pub min_score: f64,
    #[serde(default = "default_fusion_fee_skip_band")]
    pub fee_skip_band: f64,
    #[serde(default = "default_fusion_min_net_ev")]
    pub min_net_ev: f64,
    #[serde(default = "default_fusion_min_weighted_abs")]
    pub min_weighted_abs: f64,
}

impl Default for FusionCalibration {
    fn default() -> Self {
        Self {
            logit_scale: default_fusion_logit_scale(),
            min_score: default_fusion_min_score(),
            fee_skip_band: default_fusion_fee_skip_band(),
            min_net_ev: default_fusion_min_net_ev(),
            min_weighted_abs: default_fusion_min_weighted_abs(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MarketFilter {
    #[serde(rename = "type")]
    pub kind: String,
    pub slug_pattern: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Strategy {
    #[serde(rename = "type")]
    pub kind: String,
    pub params: StrategyParams,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StrategyParams {
    pub lookback_period: u32,
    pub threshold: f64,
    pub min_confidence: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SignalWeights {
    #[serde(default = "default_weight_orderbook_imbalance")]
    pub orderbook_imbalance: f64,
    #[serde(default = "default_weight_tick_velocity")]
    pub tick_velocity: f64,
    #[serde(default = "default_weight_vwap_confluence")]
    pub vwap_confluence: f64,
    #[serde(default = "default_weight_price_divergence")]
    pub price_divergence: f64,
    #[serde(default = "default_weight_zero")]
    pub spike_detector: f64,
    #[serde(default = "default_weight_deribit_pcr")]
    pub deribit_pcr: f64,
    #[serde(default = "default_weight_rsi_macd_ha")]
    pub rsi_macd_ha: f64,
    #[serde(default = "default_weight_fear_greed")]
    pub fear_greed: f64,
    #[serde(default = "default_weight_funding_rate")]
    pub funding_rate: f64,
    #[serde(default = "default_weight_oi_momentum")]
    pub oi_momentum: f64,
    #[serde(default = "default_weight_aggressor_ratio")]
    pub aggressor_ratio: f64,
    #[serde(default = "default_weight_zero")]
    pub coingecko_sentiment: f64,
    #[serde(default = "default_weight_zero")]
    pub finnhub_sentiment: f64,
    #[serde(default = "default_weight_lunarcrush_sentiment")]
    pub lunarcrush_sentiment: f64,
    #[serde(default = "default_weight_hour_of_day")]
    pub hour_of_day: f64,
    #[serde(default = "default_weight_day_of_week")]
    pub day_of_week: f64,
    #[serde(default = "default_weight_distance_round_1000")]
    pub distance_from_round_1000: f64,
    #[serde(default = "default_weight_distance_round_500")]
    pub distance_from_round_500: f64,
    #[serde(default = "default_weight_volatility_10")]
    pub volatility_10: f64,
    #[serde(default = "default_weight_volatility_30")]
    pub volatility_30: f64,
    #[serde(default = "default_weight_window_momentum")]
    pub window_momentum: f64,
    #[serde(default = "default_weight_long_liq_volume")]
    pub long_liq_volume: f64,
    #[serde(default = "default_weight_short_liq_volume")]
    pub short_liq_volume: f64,
    #[serde(default = "default_weight_depth_imbalance_5")]
    pub depth_imbalance_5: f64,
    #[serde(default = "default_weight_polymarket_flow")]
    pub polymarket_flow: f64,
    #[serde(default = "default_weight_vpin_microstructure")]
    pub vpin_microstructure: f64,
    #[serde(default = "default_weight_options_advanced")]
    pub options_advanced: f64,
    #[serde(default = "default_weight_whale_flow")]
    pub whale_flow: f64,
    #[serde(default = "default_weight_cross_market")]
    pub cross_market: f64,
    #[serde(default = "default_weight_latency_arb")]
    pub latency_arb: f64,
    #[serde(default = "default_weight_zero")]
    pub news_sentiment: f64,
    #[serde(default = "default_weight_btc_final_momentum")]
    pub btc_final_momentum: f64,
    #[serde(default = "default_weight_kyle_lambda")]
    pub kyle_lambda: f64,
    #[serde(default = "default_weight_hawkes_flow")]
    pub hawkes_flow: f64,
}

impl SignalWeights {
    pub fn ordered_pairs(&self) -> Vec<(&'static str, f64)> {
        vec![
            ("orderbook_imbalance", self.orderbook_imbalance),
            ("tick_velocity", self.tick_velocity),
            ("vwap_confluence", self.vwap_confluence),
            ("price_divergence", self.price_divergence),
            ("spike_detector", self.spike_detector),
            ("deribit_pcr", self.deribit_pcr),
            ("rsi_macd_ha", self.rsi_macd_ha),
            ("fear_greed", self.fear_greed),
            ("funding_rate", self.funding_rate),
            ("oi_momentum", self.oi_momentum),
            ("aggressor_ratio", self.aggressor_ratio),
            ("coingecko_sentiment", self.coingecko_sentiment),
            ("finnhub_sentiment", self.finnhub_sentiment),
            ("lunarcrush_sentiment", self.lunarcrush_sentiment),
            ("hour_of_day", self.hour_of_day),
            ("day_of_week", self.day_of_week),
            ("distance_from_round_1000", self.distance_from_round_1000),
            ("distance_from_round_500", self.distance_from_round_500),
            ("volatility_10", self.volatility_10),
            ("volatility_30", self.volatility_30),
            ("window_momentum", self.window_momentum),
            ("long_liq_volume", self.long_liq_volume),
            ("short_liq_volume", self.short_liq_volume),
            ("depth_imbalance_5", self.depth_imbalance_5),
            ("polymarket_flow", self.polymarket_flow),
            ("vpin_microstructure", self.vpin_microstructure),
            ("options_advanced", self.options_advanced),
            ("whale_flow", self.whale_flow),
            ("cross_market", self.cross_market),
            ("latency_arb", self.latency_arb),
            ("news_sentiment", self.news_sentiment),
            ("btc_final_momentum", self.btc_final_momentum),
            ("kyle_lambda", self.kyle_lambda),
            ("hawkes_flow", self.hawkes_flow),
        ]
    }

    pub fn as_map(&self) -> HashMap<String, f64> {
        self.ordered_pairs()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    pub fn apply_map(&mut self, map: &HashMap<String, f64>) {
        if let Some(v) = map.get("orderbook_imbalance") {
            self.orderbook_imbalance = *v;
        }
        if let Some(v) = map.get("tick_velocity") {
            self.tick_velocity = *v;
        }
        if let Some(v) = map.get("vwap_confluence") {
            self.vwap_confluence = *v;
        }
        if let Some(v) = map.get("price_divergence") {
            self.price_divergence = *v;
        }
        if let Some(v) = map.get("spike_detector") {
            self.spike_detector = *v;
        }
        if let Some(v) = map.get("deribit_pcr") {
            self.deribit_pcr = *v;
        }
        if let Some(v) = map.get("rsi_macd_ha") {
            self.rsi_macd_ha = *v;
        }
        if let Some(v) = map.get("fear_greed") {
            self.fear_greed = *v;
        }
        if let Some(v) = map.get("funding_rate") {
            self.funding_rate = *v;
        }
        if let Some(v) = map.get("oi_momentum") {
            self.oi_momentum = *v;
        }
        if let Some(v) = map.get("aggressor_ratio") {
            self.aggressor_ratio = *v;
        }
        if let Some(v) = map.get("coingecko_sentiment") {
            self.coingecko_sentiment = *v;
        }
        if let Some(v) = map.get("finnhub_sentiment") {
            self.finnhub_sentiment = *v;
        }
        if let Some(v) = map.get("lunarcrush_sentiment") {
            self.lunarcrush_sentiment = *v;
        }
        if let Some(v) = map.get("hour_of_day") {
            self.hour_of_day = *v;
        }
        if let Some(v) = map.get("day_of_week") {
            self.day_of_week = *v;
        }
        if let Some(v) = map.get("distance_from_round_1000") {
            self.distance_from_round_1000 = *v;
        }
        if let Some(v) = map.get("distance_from_round_500") {
            self.distance_from_round_500 = *v;
        }
        if let Some(v) = map.get("volatility_10") {
            self.volatility_10 = *v;
        }
        if let Some(v) = map.get("volatility_30") {
            self.volatility_30 = *v;
        }
        if let Some(v) = map.get("window_momentum") {
            self.window_momentum = *v;
        }
        if let Some(v) = map.get("long_liq_volume") {
            self.long_liq_volume = *v;
        }
        if let Some(v) = map.get("short_liq_volume") {
            self.short_liq_volume = *v;
        }
        if let Some(v) = map.get("depth_imbalance_5") {
            self.depth_imbalance_5 = *v;
        }
        if let Some(v) = map.get("polymarket_flow") {
            self.polymarket_flow = *v;
        }
        if let Some(v) = map.get("vpin_microstructure") {
            self.vpin_microstructure = *v;
        }
        if let Some(v) = map.get("options_advanced") {
            self.options_advanced = *v;
        }
        if let Some(v) = map.get("whale_flow") {
            self.whale_flow = *v;
        }
        if let Some(v) = map.get("cross_market") {
            self.cross_market = *v;
        }
        if let Some(v) = map.get("latency_arb") {
            self.latency_arb = *v;
        }
        if let Some(v) = map.get("news_sentiment") {
            self.news_sentiment = *v;
        }
        if let Some(v) = map.get("btc_final_momentum") {
            self.btc_final_momentum = *v;
        }
        if let Some(v) = map.get("kyle_lambda") {
            self.kyle_lambda = *v;
        }
        if let Some(v) = map.get("hawkes_flow") {
            self.hawkes_flow = *v;
        }
    }
}

impl Config {
    fn load_raw(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn load(path: &str) -> anyhow::Result<Self> {
        let mut cfg = Self::load_raw(path)?;
        cfg.apply_env_overrides();
        cfg.normalize_and_validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &str) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        let tmp = format!("{path}.{:032x}.tmp", rand::random::<u128>());
        let result = (|| -> anyhow::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut f = options.open(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)?;
            #[cfg(unix)]
            {
                let parent = Path::new(path)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                std::fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result
    }

    /// Persist ordinary runtime changes without copying process-only credentials.
    pub fn save_runtime(&self, path: &str) -> anyhow::Result<()> {
        self.save_runtime_with_secret_updates(path, &[])
    }

    /// Only credentials explicitly supplied in this update may override the
    /// original persisted value. Editing one provider key must not persist
    /// unrelated API credentials or an environment-only private RPC URL.
    pub fn save_runtime_with_secret_updates(
        &self,
        path: &str,
        updated_secret_fields: &[&str],
    ) -> anyhow::Result<()> {
        let mut to_save = self.clone();
        let persisted = Self::load_raw(path).ok();
        to_save.scrub_env_only_secrets_for_persistence(persisted.as_ref(), updated_secret_fields);
        to_save.save(path)
    }

    pub fn normalize_and_validate(&mut self) -> anyhow::Result<()> {
        self.validate_raw_inputs()?;
        self.normalize_runtime();
        self.validate_invariants()
    }

    /// Check runtime snapshots without silently normalizing a broken setting.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.validate_raw_inputs()?;
        self.validate_invariants()
    }

    fn validate_raw_inputs(&self) -> anyhow::Result<()> {
        if self.mode_kind() == BotMode::Unknown {
            anyhow::bail!("invalid config: unrecognized mode");
        }
        if !matches!(
            self.execution_mode.trim().to_ascii_lowercase().as_str(),
            "taker" | "maker" | "hybrid"
        ) {
            anyhow::bail!("invalid config: execution_mode must be taker, maker or hybrid");
        }
        // Config has no nullable fields. serde_json renders nonfinite floats
        // as null, including nested weights/calibration, so reject them all
        // before any max/clamp operation can conceal them or panic.
        fn finite_fields(value: &serde_json::Value, path: &str) -> anyhow::Result<()> {
            match value {
                serde_json::Value::Null => anyhow::bail!("invalid config: {path} must be finite"),
                serde_json::Value::Object(fields) => {
                    for (name, value) in fields {
                        finite_fields(value, &format!("{path}.{name}"))?;
                    }
                }
                serde_json::Value::Array(values) => {
                    for (index, value) in values.iter().enumerate() {
                        finite_fields(value, &format!("{path}[{index}]"))?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        finite_fields(&serde_json::to_value(self)?, "config")?;
        if self.maker_spread_floor_bps > 500.0 {
            anyhow::bail!("invalid config: maker_spread_floor_bps must be <= 500");
        }
        if self.initial_balance <= 0.0
            || self.trade_amount < 0.0
            || self.daily_loss_limit < 0.0
            || self.min_stake < 0.0
        {
            anyhow::bail!("invalid config: initial_balance must be positive; stake and loss amounts cannot be negative");
        }
        if !(0.0..=1.0).contains(&self.min_entry_price)
            || !(0.0..=1.0).contains(&self.max_entry_price)
        {
            anyhow::bail!("invalid config: entry prices must be within [0, 1]");
        }
        for (name, value) in [
            ("max_slippage", self.max_slippage),
            ("max_spread", self.max_spread),
            ("min_depth_usd", self.min_depth_usd),
            ("min_liquidity", self.min_liquidity),
            ("drawdown_halt_daily_pct", self.drawdown_halt_daily_pct),
            ("drawdown_halt_ath_pct", self.drawdown_halt_ath_pct),
        ] {
            if value < 0.0 {
                anyhow::bail!("invalid config: {name} cannot be negative");
            }
        }
        Ok(())
    }

    pub fn mode_kind(&self) -> BotMode {
        BotMode::parse(&self.mode)
    }

    pub fn execution_mode_kind(&self) -> ExecutionMode {
        ExecutionMode::parse(&self.execution_mode)
    }

    pub fn redacted_value(&self) -> serde_json::Value {
        let mut val = serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({}));
        if let Some(obj) = val.as_object_mut() {
            if obj
                .get("api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                obj.insert("api_key".to_string(), serde_json::json!(""));
            } else {
                obj.insert("api_key".to_string(), serde_json::json!("***"));
            }
            if obj
                .get("api_hmac_secret")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                obj.insert("api_hmac_secret".to_string(), serde_json::json!(""));
            } else {
                obj.insert("api_hmac_secret".to_string(), serde_json::json!("***"));
            }
            // private_rpc_url often contains embedded API keys (Alchemy, Infura, Flashbots, etc.)
            if obj
                .get("private_rpc_url")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                obj.insert("private_rpc_url".to_string(), serde_json::json!(""));
            } else {
                obj.insert("private_rpc_url".to_string(), serde_json::json!("***"));
            }
            if obj
                .get("finnhub_api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                obj.insert("finnhub_api_key".to_string(), serde_json::json!(""));
            } else {
                obj.insert("finnhub_api_key".to_string(), serde_json::json!("***"));
            }
            if obj
                .get("lunarcrush_api_key")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .is_empty()
            {
                obj.insert("lunarcrush_api_key".to_string(), serde_json::json!(""));
            } else {
                obj.insert("lunarcrush_api_key".to_string(), serde_json::json!("***"));
            }
        }
        val
    }

    fn apply_env_overrides(&mut self) {
        fn set_if_env(target: &mut String, key: &str) {
            if let Ok(val) = std::env::var(key) {
                if !val.trim().is_empty() {
                    *target = val;
                }
            }
        }
        set_if_env(&mut self.api_key, "MACBOT_API_KEY");
        // Both names override a persisted value. The more specific alias wins,
        // matching request authentication and persistence redaction.
        set_if_env(&mut self.api_hmac_secret, "MACBOT_API_SECRET");
        set_if_env(&mut self.api_hmac_secret, "MACBOT_API_HMAC_SECRET");
        set_if_env(&mut self.private_rpc_url, "MACBOT_PRIVATE_RPC_URL");
        set_if_env(&mut self.finnhub_api_key, "MACBOT_FINNHUB_API_KEY");
        set_if_env(&mut self.lunarcrush_api_key, "MACBOT_LUNARCRUSH_API_KEY");
        set_if_env(
            &mut self.macro_event_calendar_path,
            "MACBOT_MACRO_EVENT_CALENDAR_PATH",
        );
    }

    fn scrub_env_only_secrets_for_persistence(
        &mut self,
        persisted: Option<&Self>,
        updated_secret_fields: &[&str],
    ) {
        fn persisted_secret(
            field: &str,
            current: &mut String,
            persisted: Option<&str>,
            env_keys: &[&str],
            updated_secret_fields: &[&str],
        ) {
            if updated_secret_fields.contains(&field) {
                return;
            }
            let env_override = env_keys.iter().find_map(|key| {
                std::env::var(key)
                    .ok()
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            });
            let Some(env_override) = env_override else {
                return;
            };
            if current.trim() == env_override {
                *current = persisted.unwrap_or_default().to_string();
            }
        }

        persisted_secret(
            "api_key",
            &mut self.api_key,
            persisted.map(|cfg| cfg.api_key.as_str()),
            &["MACBOT_API_KEY"],
            updated_secret_fields,
        );
        persisted_secret(
            "api_hmac_secret",
            &mut self.api_hmac_secret,
            persisted.map(|cfg| cfg.api_hmac_secret.as_str()),
            &["MACBOT_API_HMAC_SECRET", "MACBOT_API_SECRET"],
            updated_secret_fields,
        );
        persisted_secret(
            "private_rpc_url",
            &mut self.private_rpc_url,
            persisted.map(|cfg| cfg.private_rpc_url.as_str()),
            &["MACBOT_PRIVATE_RPC_URL"],
            updated_secret_fields,
        );
        persisted_secret(
            "finnhub_api_key",
            &mut self.finnhub_api_key,
            persisted.map(|cfg| cfg.finnhub_api_key.as_str()),
            &["MACBOT_FINNHUB_API_KEY"],
            updated_secret_fields,
        );
        persisted_secret(
            "lunarcrush_api_key",
            &mut self.lunarcrush_api_key,
            persisted.map(|cfg| cfg.lunarcrush_api_key.as_str()),
            &["MACBOT_LUNARCRUSH_API_KEY"],
            updated_secret_fields,
        );
    }

    pub fn calibration_version(&self) -> String {
        let signal_weights: serde_json::Map<String, serde_json::Value> = self
            .signal_weights
            .ordered_pairs()
            .into_iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect();
        let payload = serde_json::json!({
            "signal_weights": signal_weights,
            "fusion_calibration": {
                "logit_scale": self.fusion_calibration.logit_scale,
                "min_score": self.fusion_calibration.min_score,
                "fee_skip_band": self.fusion_calibration.fee_skip_band,
                "min_net_ev": self.fusion_calibration.min_net_ev,
                "min_weighted_abs": self.fusion_calibration.min_weighted_abs
            },
            "min_ev": self.min_ev,
            "edge_safety_buffer": self.edge_safety_buffer,
            "strategy_threshold": self.strategy.params.threshold,
            "strategy_min_confidence": self.strategy.params.min_confidence
        });
        serde_json::to_string(&payload).unwrap_or_else(|_| "calibration:v1".to_string())
    }

    /// Re-base `initial_balance` — after a drawdown reset — without leaving
    /// the config invalid.
    ///
    /// `daily_loss_limit` must not exceed `initial_balance`, and a reset that
    /// lowers the balance below the limit used to persist exactly that: the
    /// file was written unvalidated, and every later config write — a change
    /// to an unrelated field included — was refused against it with an error
    /// naming two fields the operator had not touched. The limit follows the
    /// balance down; it is never raised.
    pub fn rebase_initial_balance(&mut self, new_initial: f64) {
        self.initial_balance = new_initial;
        if self.daily_loss_limit > new_initial {
            tracing::info!(
                "daily_loss_limit {:.2} lowered to {:.2} to follow the re-based initial balance",
                self.daily_loss_limit,
                new_initial
            );
            self.daily_loss_limit = new_initial;
        }
    }

    fn normalize_runtime(&mut self) {
        self.mode = self.mode_kind().as_str().to_string();
        self.execution_mode = self.execution_mode_kind().as_str().to_string();
        self.dashboard_session_start_at = self.dashboard_session_start_at.trim().to_string();
        if self.mode_kind() == BotMode::Live {
            self.auth_required = true;
        }

        self.min_trade_usd = self.min_trade_usd.max(0.01);
        self.trade_gas_usd = self.trade_gas_usd.max(0.0);
        self.edge_safety_buffer = self.edge_safety_buffer.max(0.0);
        self.hybrid_stand_down_min_closed = self.hybrid_stand_down_min_closed.max(1);
        self.hybrid_stand_down_min_win_rate = self.hybrid_stand_down_min_win_rate.clamp(0.0, 1.0);
        self.fusion_calibration.logit_scale = self.fusion_calibration.logit_scale.clamp(0.1, 10.0);
        self.fusion_calibration.min_score = self.fusion_calibration.min_score.clamp(0.0, 99.0);
        self.fusion_calibration.fee_skip_band =
            self.fusion_calibration.fee_skip_band.clamp(0.0, 0.49);
        self.fusion_calibration.min_net_ev = self.fusion_calibration.min_net_ev.clamp(0.0, 0.20);
        self.fusion_calibration.min_weighted_abs =
            self.fusion_calibration.min_weighted_abs.clamp(0.0, 1.0);
        self.macro_event_guard_minutes_before = self.macro_event_guard_minutes_before.min(240);
        self.macro_event_guard_minutes_after = self.macro_event_guard_minutes_after.min(240);
        if self.macro_event_calendar_path.trim().is_empty() {
            self.macro_event_calendar_path = default_macro_event_calendar_path();
        }
        if !self.regime_allowlist.is_empty() {
            self.regime_allowlist = self
                .regime_allowlist
                .iter()
                .map(|r| r.trim().to_ascii_uppercase())
                .filter(|r| !r.is_empty())
                .collect();
        }
        if !self.session_allowlist.is_empty() {
            self.session_allowlist = self
                .session_allowlist
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            self.session_allowlist.sort();
            self.session_allowlist.dedup();
        }
        if !self.maker_regime_allowlist.is_empty() {
            self.maker_regime_allowlist = self
                .maker_regime_allowlist
                .iter()
                .map(|r| r.trim().to_ascii_uppercase())
                .filter(|r| !r.is_empty())
                .collect();
            self.maker_regime_allowlist.sort();
            self.maker_regime_allowlist.dedup();
        }
        if !self.maker_session_allowlist.is_empty() {
            self.maker_session_allowlist = self
                .maker_session_allowlist
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            self.maker_session_allowlist.sort();
            self.maker_session_allowlist.dedup();
        }
        if !self.blocked_hours_utc.is_empty() {
            self.blocked_hours_utc = self
                .blocked_hours_utc
                .iter()
                .copied()
                .filter(|hour| *hour < 24)
                .collect();
            self.blocked_hours_utc.sort();
            self.blocked_hours_utc.dedup();
        }
        self.min_confidence_margin = self.min_confidence_margin.clamp(0.0, 0.50);
        self.buy_no_trend_up_max_p_up = self.buy_no_trend_up_max_p_up.clamp(0.0, 0.5);
        self.maker_buy_no_max_p_up = self.maker_buy_no_max_p_up.clamp(0.0, 0.5);
        self.maker_buy_no_max_entry_price = self.maker_buy_no_max_entry_price.clamp(0.01, 0.99);
        self.maker_buy_no_min_edge = self.maker_buy_no_min_edge.clamp(0.0, 0.5);
        self.tp_sl_fallback_max_age_seconds = self.tp_sl_fallback_max_age_seconds.clamp(1, 300);

        if self.maker_spread_floor_bps <= 0.0 {
            self.maker_spread_floor_bps = default_maker_spread_floor_bps();
        }
        if self.maker_spread_ceiling_bps < self.maker_spread_floor_bps {
            self.maker_spread_ceiling_bps = self.maker_spread_floor_bps;
        }
        self.maker_drift_threshold_bps = self.maker_drift_threshold_bps.max(0.5);
        self.maker_min_quote_lifetime_ms = self.maker_min_quote_lifetime_ms.max(100);
        self.maker_replace_cooldown_ms = self.maker_replace_cooldown_ms.max(50);
        self.maker_fill_cooldown_ms = self.maker_fill_cooldown_ms.max(250);
        self.maker_candidate_min_remaining_seconds = self
            .maker_candidate_min_remaining_seconds
            .max(self.expiry_guard_seconds.max(60));
        self.maker_market_quality_max_spread_bps = self
            .maker_market_quality_max_spread_bps
            .clamp(self.maker_spread_floor_bps, 500.0);
        if let Some(msg) = spread_threshold_between_ticks(self.maker_market_quality_max_spread_bps)
        {
            tracing::warn!("{msg}");
        }
        self.maker_max_replaces_per_min = self.maker_max_replaces_per_min.max(1);
        self.maker_quote_notional_usd = self.maker_quote_notional_usd.max(0.05);
        self.maker_max_inventory_notional_usd = self
            .maker_max_inventory_notional_usd
            .max(self.maker_quote_notional_usd);
        self.maker_backwardation_guard_pct = self.maker_backwardation_guard_pct.clamp(-5.0, 0.0);
        self.maker_backwardation_widen_bps = self.maker_backwardation_widen_bps.clamp(0.0, 500.0);
        self.maker_backwardation_min_target_spread_bps = self
            .maker_backwardation_min_target_spread_bps
            .clamp(0.0, self.maker_spread_ceiling_bps);
        self.maker_roll_toxic_min_confidence = self.maker_roll_toxic_min_confidence.clamp(0.0, 1.0);
        self.maker_roll_toxic_min_signal = self.maker_roll_toxic_min_signal.clamp(0.0, 1.0);
        self.maker_roll_toxic_min_sample_count = self.maker_roll_toxic_min_sample_count.max(3);
        self.live_entry_order_timeout_secs = self.live_entry_order_timeout_secs.max(1);
    }

    fn validate_invariants(&self) -> anyhow::Result<()> {
        if !(1..=300).contains(&self.api_max_skew_seconds) {
            anyhow::bail!("invalid config: api_max_skew_seconds must be within [1, 300]");
        }
        if self.sim_order_latency_ms > 60_000
            || !self.sim_available_depth_fraction.is_finite()
            || !(0.0..=1.0).contains(&self.sim_available_depth_fraction)
            || !self.maker_sim_queue_ahead_shares.is_finite()
            || !(0.0..=1_000_000.0).contains(&self.maker_sim_queue_ahead_shares)
            || !self.maker_sim_max_fill_fraction.is_finite()
            || !(0.0..=1.0).contains(&self.maker_sim_max_fill_fraction)
        {
            return Err(anyhow::anyhow!(
                "invalid paper fill assumptions: latency must be <= 60000 ms, depth/fill fractions within [0, 1], and queue shares within [0, 1000000]"
            ));
        }
        if self.min_entry_price >= self.max_entry_price {
            return Err(anyhow::anyhow!(
                "invalid config: min_entry_price ({:.4}) must be < max_entry_price ({:.4})",
                self.min_entry_price,
                self.max_entry_price
            ));
        }
        if self.min_stake > self.max_stake {
            return Err(anyhow::anyhow!(
                "invalid config: min_stake ({:.4}) must be <= max_stake ({:.4})",
                self.min_stake,
                self.max_stake
            ));
        }
        if !(0.0..=1.0).contains(&self.stake_pct) {
            return Err(anyhow::anyhow!(
                "invalid config: stake_pct ({:.4}) must be within [0, 1]",
                self.stake_pct
            ));
        }
        if !(0.0..=1.0).contains(&self.kelly_fraction) {
            return Err(anyhow::anyhow!(
                "invalid config: kelly_fraction ({:.4}) must be within [0, 1]",
                self.kelly_fraction
            ));
        }
        if !(0.0..=1.0).contains(&self.max_exposure_pct) {
            return Err(anyhow::anyhow!(
                "invalid config: max_exposure_pct ({:.4}) must be within [0, 1]",
                self.max_exposure_pct
            ));
        }
        if self.max_stake <= 0.0 || self.max_stake > 10_000.0 {
            return Err(anyhow::anyhow!(
                "invalid config: max_stake ({:.4}) must be within (0, 10000]",
                self.max_stake
            ));
        }
        if self.daily_loss_limit > self.initial_balance {
            return Err(anyhow::anyhow!(
                "invalid config: daily_loss_limit ({:.4}) must be <= initial_balance ({:.4})",
                self.daily_loss_limit,
                self.initial_balance
            ));
        }
        if self.stop_loss
            && self.take_profit
            && self.stop_loss_threshold >= self.take_profit_threshold
        {
            return Err(anyhow::anyhow!(
                "invalid config: stop_loss_threshold ({:.4}) must be < take_profit_threshold ({:.4}) when both are enabled",
                self.stop_loss_threshold,
                self.take_profit_threshold
            ));
        }
        // Live mode carries a security floor that no deployment profile may lower.
        // `MACBOT_DEPLOYMENT_PROFILE` can select a laxer profile than the mode
        // implies, so this is keyed on the mode itself rather than on the
        // profile resolved below.
        if self.mode_kind() == BotMode::Live {
            if !self.auth_required {
                return Err(anyhow::anyhow!(
                    "invalid config (live mode): auth_required must be true"
                ));
            }
            self.validate_funded_credentials("live mode")?;
        }
        self.validate_for_profile(self.deployment_profile())?;
        Ok(())
    }

    fn deployment_profile(&self) -> DeploymentProfile {
        if let Ok(raw) = std::env::var("MACBOT_DEPLOYMENT_PROFILE") {
            if let Some(p) = DeploymentProfile::parse(&raw) {
                return p;
            }
        }
        match self.mode_kind() {
            BotMode::Live => {
                let stage = std::env::var("MACBOT_ROLLOUT_STAGE")
                    .unwrap_or_else(|_| "full".to_string())
                    .to_ascii_lowercase();
                if stage == "canary" {
                    DeploymentProfile::Canary
                } else {
                    DeploymentProfile::Live
                }
            }
            BotMode::Shadow => DeploymentProfile::Shadow,
            _ => {
                if self.shadow_trading {
                    DeploymentProfile::Shadow
                } else {
                    DeploymentProfile::Relaxed
                }
            }
        }
    }

    /// Credential values that are public knowledge: they ship in `config.json`,
    /// appear in `.env.example`, or are common stand-ins. They are fine for local
    /// evaluation and must never guard a funded deployment.
    const PLACEHOLDER_CREDENTIALS: &'static [&'static str] = &[
        "local",
        "localsecret",
        "replace-with-a-random-api-key",
        "replace-with-a-long-random-hmac-secret",
        "changeme",
        "change-me",
        "password",
        "secret",
        "apikey",
        "api-key",
        "test",
        "testing",
        "example",
        "dummy",
    ];

    /// Minimum length for a credential guarding a funded deployment.
    const MIN_FUNDED_CREDENTIAL_LEN: usize = 16;

    /// Returns why `value` is unfit to guard a funded deployment, or `None` if it is fit.
    fn credential_weakness(label: &str, value: &str) -> Option<String> {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Some(format!("{label} must be configured"));
        }
        if Self::PLACEHOLDER_CREDENTIALS
            .iter()
            .any(|candidate| trimmed.eq_ignore_ascii_case(candidate))
        {
            return Some(format!(
                "{label} is a well-known placeholder value; generate a unique secret"
            ));
        }
        if trimmed.chars().count() < Self::MIN_FUNDED_CREDENTIAL_LEN {
            return Some(format!(
                "{label} must be at least {} characters",
                Self::MIN_FUNDED_CREDENTIAL_LEN
            ));
        }
        None
    }

    /// Rejects missing, placeholder, or trivially short API credentials.
    /// `context` names what is being guarded, e.g. "live mode" or "canary profile".
    fn validate_funded_credentials(&self, context: &str) -> anyhow::Result<()> {
        if let Some(reason) = Self::credential_weakness("api_key", &self.api_key)
            .or_else(|| Self::credential_weakness("api_hmac_secret", &self.api_hmac_secret))
        {
            return Err(anyhow::anyhow!("invalid config ({context}): {reason}"));
        }
        Ok(())
    }

    fn validate_for_profile(&self, profile: DeploymentProfile) -> anyhow::Result<()> {
        match profile {
            DeploymentProfile::Relaxed => Ok(()),
            DeploymentProfile::Shadow => {
                if !(self.shadow_trading || self.mode_kind() == BotMode::Shadow) {
                    return Err(anyhow::anyhow!(
                        "invalid config (shadow profile): shadow_trading must be true or mode must be 'shadow'"
                    ));
                }
                if self.mode_kind() == BotMode::Live {
                    return Err(anyhow::anyhow!(
                        "invalid config (shadow profile): mode 'live' is not allowed"
                    ));
                }
                if self.min_trade_usd < (self.trade_gas_usd * 1.5) {
                    return Err(anyhow::anyhow!(
                        "invalid config (shadow profile): min_trade_usd ({:.4}) must be >= 1.5x trade_gas_usd ({:.4})",
                        self.min_trade_usd,
                        self.trade_gas_usd
                    ));
                }
                Ok(())
            }
            DeploymentProfile::Canary => {
                if self.mode_kind() != BotMode::Live {
                    return Err(anyhow::anyhow!(
                        "invalid config (canary profile): mode must be 'live'"
                    ));
                }
                self.validate_funded_credentials("canary profile")?;
                if self.max_open_positions > 2 {
                    return Err(anyhow::anyhow!(
                        "invalid config (canary profile): max_open_positions ({}) must be <= 2",
                        self.max_open_positions
                    ));
                }
                if self.stake_pct > 0.10 {
                    return Err(anyhow::anyhow!(
                        "invalid config (canary profile): stake_pct ({:.4}) must be <= 0.10",
                        self.stake_pct
                    ));
                }
                if self.min_trade_usd < (self.trade_gas_usd * 2.0) {
                    return Err(anyhow::anyhow!(
                        "invalid config (canary profile): min_trade_usd ({:.4}) must be >= 2x trade_gas_usd ({:.4})",
                        self.min_trade_usd,
                        self.trade_gas_usd
                    ));
                }
                Ok(())
            }
            DeploymentProfile::Live => {
                if self.mode_kind() != BotMode::Live {
                    return Err(anyhow::anyhow!(
                        "invalid config (live profile): mode must be 'live'"
                    ));
                }
                if !self.auth_required {
                    return Err(anyhow::anyhow!(
                        "invalid config (live profile): auth_required must be true"
                    ));
                }
                self.validate_funded_credentials("live profile")?;
                if self.analysis_stale_seconds > 120 {
                    return Err(anyhow::anyhow!(
                        "invalid config (live profile): analysis_stale_seconds ({}) must be <= 120",
                        self.analysis_stale_seconds
                    ));
                }
                if self.daily_loss_limit <= 0.0 {
                    return Err(anyhow::anyhow!(
                        "invalid config (live profile): daily_loss_limit ({:.4}) must be > 0",
                        self.daily_loss_limit
                    ));
                }
                if self.idempotency_ttl_seconds < 60 {
                    return Err(anyhow::anyhow!(
                        "invalid config (live profile): idempotency_ttl_seconds ({}) must be >= 60",
                        self.idempotency_ttl_seconds
                    ));
                }
                if self.min_trade_usd < (self.trade_gas_usd * 2.0) {
                    return Err(anyhow::anyhow!(
                        "invalid config (live profile): min_trade_usd ({:.4}) must be >= 2x trade_gas_usd ({:.4})",
                        self.min_trade_usd,
                        self.trade_gas_usd
                    ));
                }
                Ok(())
            }
        }
    }
}

/// How long the trading loop may sleep before the next analysis cycle.
///
/// The loop must never sleep past the point where it would itself declare the
/// analysis stale. Maker and hybrid mode already clamped to the freshness
/// budget; taker mode did not, so with the shipped configuration —
/// `check_interval` 60 against `analysis_stale_seconds` 45 — it spent fifteen
/// seconds of every minute acting on analysis it had already marked too old.
///
/// `mins_left` tightens the cadence in the closing minutes of a market, but
/// never loosens it past the budget.
pub fn analysis_refresh_secs(
    execution_mode: &str,
    check_interval: u64,
    signal_refresh_interval: u64,
    analysis_stale_seconds: u64,
    mins_left: f64,
) -> u64 {
    let budget = analysis_stale_seconds.max(1);
    let base = if matches!(execution_mode, "maker" | "hybrid") {
        check_interval
            .max(1)
            .min(signal_refresh_interval.max(1))
            .min(budget)
    } else {
        check_interval.max(1).min(budget)
    };
    if mins_left < 3.0 {
        15_u64.min(base)
    } else {
        base
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_drawdown_reset_below_the_daily_limit_keeps_the_config_valid() {
        let mut cfg: Config = serde_json::from_str(include_str!("../config.json")).expect("config");
        cfg.initial_balance = 50.0;
        cfg.daily_loss_limit = 50.0;
        assert!(
            cfg.validate_invariants().is_ok(),
            "fixture must start valid"
        );

        // What Reset DD does when the balance has drifted below the limit.
        cfg.rebase_initial_balance(49.69);
        assert!(
            cfg.validate_invariants().is_ok(),
            "the reset left the config invalid, so every later save would be refused"
        );
        assert_eq!(
            cfg.daily_loss_limit, 49.69,
            "the limit follows the balance down"
        );

        // And never up: a reset above the limit leaves the limit alone.
        cfg.rebase_initial_balance(500.0);
        assert_eq!(cfg.daily_loss_limit, 49.69);
    }

    #[test]
    fn a_spread_threshold_between_ticks_is_called_out() {
        use super::spread_threshold_between_ticks as check;
        assert!(check(100.0).is_none(), "one tick is reachable");
        assert!(check(200.0).is_none(), "two ticks is reachable");
        let msg = check(160.0).expect("160 sits between one tick and two");
        assert!(msg.contains("behaves as 100"), "{msg}");
        assert!(
            check(super::default_maker_market_quality_max_spread_bps()).is_none(),
            "the shipped default must not trip its own warning"
        );
    }

    #[test]
    fn the_loop_never_sleeps_past_the_staleness_budget() {
        // The shipped configuration, in taker mode: a 60s cadence against a 45s
        // staleness budget left the analysis knowingly stale for a quarter of
        // every cycle.
        assert_eq!(super::analysis_refresh_secs("taker", 60, 30, 45, 10.0), 45);
        // Maker also folds in the signal refresh interval.
        assert_eq!(super::analysis_refresh_secs("maker", 60, 30, 45, 10.0), 30);
        // A budget longer than the cadence must not slow the loop down.
        assert_eq!(super::analysis_refresh_secs("taker", 20, 30, 45, 10.0), 20);
        // The closing-minutes speed-up tightens, and never loosens.
        assert_eq!(super::analysis_refresh_secs("taker", 60, 30, 45, 2.0), 15);
        assert_eq!(super::analysis_refresh_secs("taker", 10, 30, 45, 2.0), 10);
        // Zeroes must not produce a busy loop.
        assert_eq!(super::analysis_refresh_secs("taker", 0, 0, 0, 10.0), 1);
    }

    use super::*;

    #[test]
    fn omitted_paper_fill_settings_use_conservative_defaults() {
        let cfg: Config = serde_json::from_str(include_str!("../config.json")).unwrap();
        assert_eq!(cfg.sim_order_latency_ms, 250);
        assert_eq!(cfg.sim_available_depth_fraction, 0.25);
        assert_eq!(cfg.maker_sim_queue_ahead_shares, 5.0);
        assert_eq!(cfg.maker_sim_max_fill_fraction, 0.25);
    }

    #[test]
    fn signed_request_window_is_bounded_for_durable_replay_protection() {
        let mut cfg = fixture();
        for seconds in [0, 301, u64::MAX] {
            cfg.api_max_skew_seconds = seconds;
            assert!(cfg.validate().is_err());
            assert!(cfg.normalize_and_validate().is_err());
        }
        for seconds in [1, 30, 300] {
            cfg.api_max_skew_seconds = seconds;
            assert!(cfg.validate().is_ok());
            assert!(cfg.normalize_and_validate().is_ok());
        }
    }

    #[test]
    fn invalid_paper_fill_assumptions_are_refused_instead_of_clamped() {
        let mut cfg: Config = serde_json::from_str(include_str!("../config.json")).unwrap();
        cfg.sim_order_latency_ms = 60_001;
        assert!(cfg.validate_invariants().is_err());
        cfg.sim_order_latency_ms = 250;
        for invalid in [f64::NAN, f64::INFINITY, -0.01, 1.01] {
            cfg.sim_available_depth_fraction = invalid;
            assert!(cfg.validate_invariants().is_err());
            cfg.sim_available_depth_fraction = 0.25;
            cfg.maker_sim_max_fill_fraction = invalid;
            assert!(cfg.validate_invariants().is_err());
            cfg.maker_sim_max_fill_fraction = 0.25;
        }
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 1_000_001.0] {
            cfg.maker_sim_queue_ahead_shares = invalid;
            assert!(cfg.validate_invariants().is_err());
        }
    }

    struct EnvRestore {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            if let Some(old) = self.old.as_deref() {
                std::env::set_var(self.key, old);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn fixture() -> Config {
        serde_json::from_str(include_str!("../config.json")).expect("valid fixture")
    }

    #[test]
    fn invalid_entry_bounds_are_rejected() {
        let mut cfg = fixture();
        cfg.min_entry_price = 0.8;
        cfg.max_entry_price = 0.2;
        assert!(cfg.normalize_and_validate().is_err());
    }

    #[test]
    fn invalid_stake_bounds_are_rejected() {
        let mut cfg = fixture();
        cfg.min_stake = 2.0;
        cfg.max_stake = 1.0;
        assert!(cfg.normalize_and_validate().is_err());
    }

    #[test]
    fn invalid_tp_sl_order_is_rejected() {
        let mut cfg = fixture();
        cfg.stop_loss = true;
        cfg.take_profit = true;
        cfg.stop_loss_threshold = 0.85;
        cfg.take_profit_threshold = 0.80;
        assert!(cfg.normalize_and_validate().is_err());
    }

    #[test]
    fn fusion_calibration_is_normalized() {
        let mut cfg = fixture();
        cfg.fusion_calibration.logit_scale = -1.0;
        cfg.fusion_calibration.min_score = 1000.0;
        cfg.fusion_calibration.fee_skip_band = 5.0;
        cfg.fusion_calibration.min_net_ev = -0.4;
        cfg.fusion_calibration.min_weighted_abs = 2.0;

        cfg.normalize_and_validate()
            .expect("config should normalize");

        assert!((0.1..=10.0).contains(&cfg.fusion_calibration.logit_scale));
        assert!((0.0..=99.0).contains(&cfg.fusion_calibration.min_score));
        assert!((0.0..=0.49).contains(&cfg.fusion_calibration.fee_skip_band));
        assert!((0.0..=0.20).contains(&cfg.fusion_calibration.min_net_ev));
        assert!((0.0..=1.0).contains(&cfg.fusion_calibration.min_weighted_abs));
    }

    #[test]
    fn shadow_profile_rejects_non_shadow_mode() {
        let mut cfg = fixture();
        cfg.mode = "simulation".to_string();
        cfg.shadow_trading = false;
        assert!(cfg.validate_for_profile(DeploymentProfile::Shadow).is_err());
    }

    #[test]
    fn canary_profile_rejects_large_stake_pct() {
        let mut cfg = fixture();
        cfg.mode = "live".to_string();
        cfg.stake_pct = 0.25;
        assert!(cfg.validate_for_profile(DeploymentProfile::Canary).is_err());
    }

    #[test]
    fn live_profile_accepts_valid_live_fixture() {
        let mut cfg = fixture();
        cfg.mode = "live".to_string();
        cfg.auth_required = true;
        cfg.api_key = "3f8c1d9a4b7e2065".to_string();
        cfg.api_hmac_secret = "0c5b7e21a9d34f68be07c1a25d9f3084".to_string();
        cfg.analysis_stale_seconds = 45;
        cfg.daily_loss_limit = 25.0;
        cfg.idempotency_ttl_seconds = 600;
        cfg.min_trade_usd = 0.25;
        cfg.trade_gas_usd = 0.02;
        assert!(cfg.validate_for_profile(DeploymentProfile::Live).is_ok());
    }

    fn funded_fixture() -> Config {
        let mut cfg = fixture();
        cfg.mode = "live".to_string();
        cfg.auth_required = true;
        cfg.api_key = "3f8c1d9a4b7e2065".to_string();
        cfg.api_hmac_secret = "0c5b7e21a9d34f68be07c1a25d9f3084".to_string();
        cfg.analysis_stale_seconds = 45;
        cfg.daily_loss_limit = 25.0;
        cfg.idempotency_ttl_seconds = 600;
        cfg.min_trade_usd = 0.25;
        cfg.trade_gas_usd = 0.02;
        cfg
    }

    #[test]
    fn live_profile_rejects_shipped_placeholder_credentials() {
        // These are the values committed in config.json for local evaluation.
        let mut cfg = funded_fixture();
        cfg.api_key = "local".to_string();
        cfg.api_hmac_secret = "localsecret".to_string();
        let err = cfg
            .validate_for_profile(DeploymentProfile::Live)
            .expect_err("shipped placeholders must not guard a funded deployment");
        assert!(err.to_string().contains("placeholder"), "{err}");
    }

    #[test]
    fn live_profile_rejects_env_template_placeholders() {
        let mut cfg = funded_fixture();
        cfg.api_key = "replace-with-a-random-api-key".to_string();
        assert!(cfg.validate_for_profile(DeploymentProfile::Live).is_err());
    }

    #[test]
    fn live_profile_rejects_short_credentials() {
        let mut cfg = funded_fixture();
        cfg.api_hmac_secret = "short".to_string();
        let err = cfg
            .validate_for_profile(DeploymentProfile::Live)
            .expect_err("a five-character secret must be rejected");
        assert!(err.to_string().contains("at least"), "{err}");
    }

    #[test]
    fn live_profile_rejects_missing_credentials() {
        let mut cfg = funded_fixture();
        cfg.api_key = String::new();
        assert!(cfg.validate_for_profile(DeploymentProfile::Live).is_err());
    }

    #[test]
    fn canary_profile_rejects_placeholder_credentials() {
        let mut cfg = funded_fixture();
        cfg.max_open_positions = 2;
        cfg.stake_pct = 0.05;
        cfg.api_hmac_secret = "localsecret".to_string();
        assert!(cfg.validate_for_profile(DeploymentProfile::Canary).is_err());
    }

    #[test]
    fn placeholder_check_is_case_insensitive_and_trims() {
        assert!(Config::credential_weakness("api_key", "  LocalSecret  ").is_some());
        assert!(Config::credential_weakness("api_key", "3f8c1d9a4b7e2065").is_none());
    }

    #[test]
    fn relaxed_and_shadow_profiles_still_allow_local_placeholders() {
        // Local evaluation must keep working with the committed config.
        let mut cfg = fixture();
        cfg.api_key = "local".to_string();
        cfg.api_hmac_secret = "localsecret".to_string();
        assert!(cfg.validate_for_profile(DeploymentProfile::Relaxed).is_ok());
        cfg.shadow_trading = true;
        assert!(cfg.validate_for_profile(DeploymentProfile::Shadow).is_ok());
    }

    #[test]
    fn live_mode_floor_survives_a_relaxed_profile_override() {
        let _guard = crate::env_util::test_env_lock();
        let _profile = EnvRestore::set("MACBOT_DEPLOYMENT_PROFILE", "relaxed");
        let mut cfg = funded_fixture();
        cfg.api_key = "local".to_string();
        cfg.api_hmac_secret = "localsecret".to_string();
        let err = cfg
            .validate_invariants()
            .expect_err("a relaxed profile must not lower the live-mode floor");
        assert!(err.to_string().contains("live mode"), "{err}");
    }

    #[test]
    fn live_mode_floor_survives_a_shadow_profile_override() {
        let _guard = crate::env_util::test_env_lock();
        let _profile = EnvRestore::set("MACBOT_DEPLOYMENT_PROFILE", "shadow");
        let mut cfg = funded_fixture();
        cfg.api_hmac_secret = "localsecret".to_string();
        assert!(cfg.validate_invariants().is_err());
    }

    #[test]
    fn live_mode_floor_rejects_auth_disabled_under_an_override() {
        let _guard = crate::env_util::test_env_lock();
        let _profile = EnvRestore::set("MACBOT_DEPLOYMENT_PROFILE", "relaxed");
        let mut cfg = funded_fixture();
        cfg.auth_required = false;
        let err = cfg
            .validate_invariants()
            .expect_err("live mode must require auth regardless of profile");
        assert!(err.to_string().contains("auth_required"), "{err}");
    }

    #[test]
    fn live_mode_floor_accepts_strong_credentials_under_an_override() {
        let _guard = crate::env_util::test_env_lock();
        let _profile = EnvRestore::set("MACBOT_DEPLOYMENT_PROFILE", "relaxed");
        let cfg = funded_fixture();
        assert!(cfg.validate_invariants().is_ok());
    }

    #[test]
    fn non_live_modes_keep_working_with_shipped_placeholders() {
        let _guard = crate::env_util::test_env_lock();
        let _profile = EnvRestore::set("MACBOT_DEPLOYMENT_PROFILE", "relaxed");
        // The committed config: simulation mode with local placeholder credentials.
        let cfg = fixture();
        assert_eq!(cfg.mode_kind(), BotMode::Simulation);
        assert!(cfg.validate_invariants().is_ok());
    }

    #[test]
    fn signal_weights_fixture_exposes_full_live_surface() {
        let cfg = fixture();
        let weights = cfg.signal_weights.as_map();
        for key in [
            "orderbook_imbalance",
            "tick_velocity",
            "vwap_confluence",
            "price_divergence",
            "spike_detector",
            "deribit_pcr",
            "rsi_macd_ha",
            "fear_greed",
            "funding_rate",
            "oi_momentum",
            "aggressor_ratio",
            "coingecko_sentiment",
            "finnhub_sentiment",
            "lunarcrush_sentiment",
            "hour_of_day",
            "day_of_week",
            "distance_from_round_1000",
            "distance_from_round_500",
            "volatility_10",
            "volatility_30",
            "window_momentum",
            "long_liq_volume",
            "short_liq_volume",
            "depth_imbalance_5",
            "polymarket_flow",
            "vpin_microstructure",
            "options_advanced",
            "whale_flow",
            "cross_market",
            "latency_arb",
            "news_sentiment",
            "btc_final_momentum",
            "kyle_lambda",
            "hawkes_flow",
        ] {
            assert!(weights.contains_key(key), "missing weight key: {key}");
        }
    }

    #[test]
    fn invalid_kelly_fraction_is_rejected() {
        let mut cfg = fixture();
        cfg.kelly_fraction = 1.5;
        assert!(cfg.normalize_and_validate().is_err());
    }

    #[test]
    fn maker_allowlists_are_normalized() {
        let mut cfg = fixture();
        cfg.maker_regime_allowlist = vec![
            " squeeze ".to_string(),
            "trend_up".to_string(),
            "SQUEEZE".to_string(),
        ];
        cfg.maker_session_allowlist =
            vec![" US ".to_string(), "asia".to_string(), "us".to_string()];

        cfg.normalize_and_validate()
            .expect("config should normalize");

        assert_eq!(
            cfg.maker_regime_allowlist,
            vec!["SQUEEZE".to_string(), "TREND_UP".to_string()]
        );
        assert_eq!(
            cfg.maker_session_allowlist,
            vec!["asia".to_string(), "us".to_string()]
        );
    }

    #[test]
    fn maker_buy_no_guard_threshold_is_normalized() {
        let mut cfg = fixture();
        cfg.maker_buy_no_guard_enabled = true;
        cfg.maker_buy_no_max_p_up = 0.9;
        cfg.maker_buy_no_max_entry_price = 2.0;
        cfg.maker_buy_no_min_edge = 1.0;
        cfg.maker_candidate_min_remaining_seconds = 30;

        cfg.normalize_and_validate()
            .expect("config should normalize");

        assert!((cfg.maker_buy_no_max_p_up - 0.5).abs() < 1e-9);
        assert!((cfg.maker_buy_no_max_entry_price - 0.99).abs() < 1e-9);
        assert!((cfg.maker_buy_no_min_edge - 0.5).abs() < 1e-9);
        assert_eq!(
            cfg.maker_candidate_min_remaining_seconds,
            cfg.expiry_guard_seconds.max(60)
        );
    }

    #[test]
    fn invalid_max_exposure_pct_is_rejected() {
        let mut cfg = fixture();
        cfg.max_exposure_pct = 1.2;
        assert!(cfg.normalize_and_validate().is_err());
    }

    #[test]
    fn invalid_max_stake_is_rejected() {
        let mut cfg = fixture();
        cfg.max_stake = 20_000.0;
        assert!(cfg.normalize_and_validate().is_err());
    }

    #[test]
    fn environment_secret_overrides_nonempty_file_secret() {
        let _guard = crate::env_util::test_env_lock();
        let _alias = EnvRestore::set("MACBOT_API_HMAC_SECRET", "");
        let _secret = EnvRestore::set("MACBOT_API_SECRET", "environment-secret");
        let mut cfg = fixture();
        cfg.api_hmac_secret = "file-secret".to_string();
        cfg.apply_env_overrides();
        assert_eq!(cfg.api_hmac_secret, "environment-secret");
    }

    #[test]
    fn invalid_config_is_rejected_before_normalization_can_panic_or_hide_it() {
        for floor in [501.0, f64::INFINITY, f64::NAN] {
            let mut cfg = fixture();
            cfg.maker_spread_floor_bps = floor;
            assert!(cfg.normalize_and_validate().is_err());
        }
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            let mut cfg = fixture();
            cfg.initial_balance = invalid;
            assert!(cfg.normalize_and_validate().is_err());
        }
        let mut cfg = fixture();
        cfg.mode = "liv".to_string();
        assert!(cfg.normalize_and_validate().is_err());
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "maekr".to_string();
        assert!(cfg.normalize_and_validate().is_err());
        cfg.execution_mode = "maker".to_string();
        cfg.signal_weights.orderbook_imbalance = f64::NAN;
        assert!(cfg.normalize_and_validate().is_err());
    }

    #[test]
    fn hmac_environment_alias_has_consistent_precedence() {
        let _guard = crate::env_util::test_env_lock();
        let _alias = EnvRestore::set("MACBOT_API_HMAC_SECRET", "hmac-alias-secret");
        let _secret = EnvRestore::set("MACBOT_API_SECRET", "other-secret");
        let mut cfg = fixture();
        cfg.apply_env_overrides();
        assert_eq!(cfg.api_hmac_secret, "hmac-alias-secret");
    }

    #[test]
    fn save_runtime_keeps_env_only_secrets_off_disk() {
        let _guard = crate::env_util::test_env_lock();
        let _api_key = EnvRestore::set("MACBOT_API_KEY", "env-api-key");
        let _api_secret = EnvRestore::set("MACBOT_API_SECRET", "env-api-secret");
        let _rpc = EnvRestore::set("MACBOT_PRIVATE_RPC_URL", "https://env-rpc");

        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_config_runtime_save_{uniq}.json"));
        let seed = fixture();
        seed.save(path.to_str().expect("path"))
            .expect("seed config file");

        let mut cfg = Config::load(path.to_str().expect("path")).expect("load config");
        cfg.trade_amount = 2.5;
        cfg.save_runtime(path.to_str().expect("path"))
            .expect("save runtime");

        let persisted = std::fs::read_to_string(&path).expect("read config");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!persisted.contains("env-api-key"));
        assert!(!persisted.contains("env-api-secret"));
        assert!(!persisted.contains("https://env-rpc"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn runtime_save_only_persists_explicitly_updated_secret_fields() {
        let _guard = crate::env_util::test_env_lock();
        let _key = EnvRestore::set("MACBOT_API_KEY", "env-key");
        let _secret = EnvRestore::set("MACBOT_API_HMAC_SECRET", "env-secret");
        let _rpc = EnvRestore::set("MACBOT_PRIVATE_RPC_URL", "https://env-rpc.invalid");
        let _finnhub = EnvRestore::set("MACBOT_FINNHUB_API_KEY", "env-finnhub");
        let _lunar = EnvRestore::set("MACBOT_LUNARCRUSH_API_KEY", "env-lunar");
        let path = std::env::temp_dir().join(format!(
            "macbot-selective-secret-{:032x}.json",
            rand::random::<u128>()
        ));
        let mut seed = fixture();
        seed.api_key = "saved-key".into();
        seed.api_hmac_secret = "saved-secret".into();
        seed.private_rpc_url.clear();
        seed.finnhub_api_key.clear();
        seed.lunarcrush_api_key.clear();
        seed.save(path.to_str().unwrap()).unwrap();
        let config = Config::load(path.to_str().unwrap()).unwrap();
        // Intentionally saving the same value as its environment override is
        // supported for this field, without opting in the other four fields.
        config
            .save_runtime_with_secret_updates(path.to_str().unwrap(), &["finnhub_api_key"])
            .unwrap();
        let saved = Config::load_raw(path.to_str().unwrap()).unwrap();
        assert_eq!(saved.finnhub_api_key, "env-finnhub");
        assert_eq!(saved.api_key, "saved-key");
        assert_eq!(saved.api_hmac_secret, "saved-secret");
        assert!(saved.private_rpc_url.is_empty());
        assert!(saved.lunarcrush_api_key.is_empty());
        config
            .save_runtime_with_secret_updates(
                path.to_str().unwrap(),
                &["api_key", "api_hmac_secret"],
            )
            .unwrap();
        let saved = Config::load_raw(path.to_str().unwrap()).unwrap();
        assert_eq!(saved.api_key, "env-key");
        assert_eq!(saved.api_hmac_secret, "env-secret");
        assert!(saved.private_rpc_url.is_empty());
        assert!(saved.lunarcrush_api_key.is_empty());
        assert_eq!(
            saved.finnhub_api_key, "env-finnhub",
            "unrelated saved provider value is retained"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn runtime_save_persists_dashboard_session_boundary() {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_session_boundary_{uniq}.json"));
        let mut cfg = fixture();
        cfg.dashboard_session_start_trade_id = 321;
        cfg.dashboard_session_start_at = "2026-03-30T15:55:00Z".to_string();

        cfg.save(path.to_str().expect("path")).expect("save config");
        let loaded = Config::load(path.to_str().expect("path")).expect("load config");

        assert_eq!(loaded.dashboard_session_start_trade_id, 321);
        assert_eq!(loaded.dashboard_session_start_at, "2026-03-30T15:55:00Z");

        let _ = std::fs::remove_file(path);
    }
}
