//! Inputs for the explicitly synthetic TA experiment.
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BacktestRequest {
    /// Explicit experiment type. Omitted on existing clients.
    #[serde(default = "default_mode")]
    pub mode: String,
    /// Fixed synthetic token purchase prices, not observed historical quotes.
    /// The two sides are independent assumptions; neither is derived from the other.
    #[serde(default = "default_entry_price")]
    pub entry_price_yes: f64,
    #[serde(default = "default_entry_price")]
    pub entry_price_no: f64,
    /// Number of historical days to test (max 180).
    #[serde(default = "default_days")]
    pub days: u32,
    /// Starting virtual balance in USD.
    #[serde(default = "default_balance")]
    pub initial_balance: f64,
    /// Fixed gas cost per trade in USD.
    #[serde(default = "default_gas_usd")]
    pub gas_usd: f64,
    /// Liquidity proxy for slippage estimation in USD.
    #[serde(default = "default_liquidity_proxy_usd")]
    pub liquidity_proxy_usd: f64,
    /// Hard cap on position size as % of current balance.
    #[serde(default = "default_max_trade_pct")]
    pub max_trade_pct: f64,
    /// TA lookback / warmup window.
    #[serde(default = "default_window")]
    pub window: usize,
    /// Absolute score threshold to open a position.
    #[serde(default = "default_entry_threshold")]
    pub entry_threshold: f64,
    /// Minimum economic trade size in USD.
    #[serde(default = "default_min_trade_usd")]
    pub min_trade_usd: f64,
    /// Additional edge buffer (fractional, e.g. 0.0015 = 0.15%).
    #[serde(default = "default_edge_safety_buffer")]
    pub edge_safety_buffer: f64,
    /// Optional regime-specific threshold for BULL/TREND_UP.
    #[serde(default)]
    pub bull_entry_threshold: Option<f64>,
    /// Optional regime-specific threshold for BEAR/TREND_DOWN.
    #[serde(default)]
    pub bear_entry_threshold: Option<f64>,
    /// Optional regime-specific threshold for CHOP.
    #[serde(default)]
    pub chop_entry_threshold: Option<f64>,
    /// Optional absolute score threshold for BUY_YES.
    #[serde(default)]
    pub buy_yes_threshold: Option<f64>,
    /// Optional absolute score threshold for BUY_NO.
    #[serde(default)]
    pub buy_no_threshold: Option<f64>,
    /// Enable/disable BUY_YES side.
    #[serde(default = "default_true")]
    pub allow_buy_yes: bool,
    /// Enable/disable BUY_NO side.
    #[serde(default = "default_true")]
    pub allow_buy_no: bool,
    /// Optional allowlist of regimes to trade. Empty = all regimes allowed.
    #[serde(default)]
    pub regime_allowlist: Vec<String>,
    /// Optional allowlist of UTC trading sessions (`asia`, `europe`, `us`).
    /// Empty = all sessions allowed.
    #[serde(default)]
    pub session_allowlist: Vec<String>,
    /// Optional UTC entry-hour blocklist (`0`-`23`).
    #[serde(default)]
    pub blocked_hours_utc: Vec<u32>,
    /// Optional per-regime side policy overrides.
    /// Values: `both`, `yes_only`, `no_only`, `none`.
    #[serde(default)]
    pub regime_side_modes: HashMap<String, String>,
    /// If true, only allow BUY_YES in up regimes and BUY_NO in down regimes.
    #[serde(default)]
    pub require_regime_alignment: bool,
    /// Anti-bias strength for repeated same-side trades (0 disables).
    #[serde(default = "default_anti_bias_strength")]
    pub anti_bias_strength: f64,
    /// Optional threshold override when rolling volatility is high.
    #[serde(default)]
    pub high_vol_entry_threshold: Option<f64>,
    /// Rolling volatility threshold for applying `high_vol_entry_threshold`.
    #[serde(default = "default_high_volatility_threshold")]
    pub high_volatility_threshold: f64,
    /// Optional minimum margin above the chosen threshold before entering.
    #[serde(default)]
    pub min_confidence_margin: f64,
    /// Apply session-aware weighting to the simplified backtest score.
    #[serde(default)]
    pub time_of_day_weight: bool,
    /// Enable adaptive stand-down when recent hit-rate falls below break-even.
    #[serde(default)]
    pub adaptive_stand_down: bool,
    /// Number of recent trades used for stand-down health checks.
    #[serde(default = "default_stand_down_window")]
    pub stand_down_window: usize,
    /// Required buffer above break-even win-rate (percentage points).
    #[serde(default = "default_stand_down_margin_pp")]
    pub stand_down_margin_pp: f64,
    /// Cooldown duration in candles once stand-down triggers.
    #[serde(default = "default_stand_down_cooldown_candles")]
    pub stand_down_cooldown_candles: usize,
    /// Optional backtest end time in Unix milliseconds (UTC).
    /// When omitted, the backtest ends at "now".
    #[serde(default)]
    pub end_time_ms: Option<i64>,
    /// Execution cost model used in backtest economics (`taker` or `maker`).
    /// Strategy logic remains directional; this selects fee/slippage assumptions.
    #[serde(default)]
    pub execution_mode: Option<String>,
    /// Effective maker fee fraction used when `execution_mode=maker`.
    /// Example: -0.0001 means a 1bp rebate, 0.0 means zero fee.
    #[serde(default)]
    pub maker_fee_pct: Option<f64>,
    /// Multiplicative factor applied to LMSR slippage when `execution_mode=maker`.
    /// Example: 0.2 means maker slippage is 20% of taker slippage.
    #[serde(default)]
    pub maker_slippage_factor: Option<f64>,
    /// Gas used when `execution_mode=maker` (defaults to 0.0).
    #[serde(default)]
    pub maker_gas_usd: Option<f64>,
    /// Trade exit model (`next_candle` or `taker_tp_sl`).
    #[serde(default)]
    pub exit_mode: Option<String>,
    /// Take-profit percentage on the binary entry price (0.12 = +12%).
    #[serde(default)]
    pub take_profit_pct: Option<f64>,
    /// Stop-loss percentage on the binary entry price (0.08 = -8%).
    #[serde(default)]
    pub stop_loss_pct: Option<f64>,
}

impl BacktestRequest {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.mode != "synthetic_ta" {
            return Err("mode must be synthetic_ta".into());
        }
        let scalars = [
            ("initial_balance", self.initial_balance),
            ("gas_usd", self.gas_usd),
            ("liquidity_proxy_usd", self.liquidity_proxy_usd),
            ("max_trade_pct", self.max_trade_pct),
            ("entry_threshold", self.entry_threshold),
            ("min_trade_usd", self.min_trade_usd),
            ("edge_safety_buffer", self.edge_safety_buffer),
            ("anti_bias_strength", self.anti_bias_strength),
            ("high_volatility_threshold", self.high_volatility_threshold),
            ("min_confidence_margin", self.min_confidence_margin),
            ("stand_down_margin_pp", self.stand_down_margin_pp),
        ];
        let optional = [
            ("bull_entry_threshold", self.bull_entry_threshold),
            ("bear_entry_threshold", self.bear_entry_threshold),
            ("chop_entry_threshold", self.chop_entry_threshold),
            ("buy_yes_threshold", self.buy_yes_threshold),
            ("buy_no_threshold", self.buy_no_threshold),
            ("high_vol_entry_threshold", self.high_vol_entry_threshold),
            ("maker_fee_pct", self.maker_fee_pct),
            ("maker_slippage_factor", self.maker_slippage_factor),
            ("maker_gas_usd", self.maker_gas_usd),
            ("take_profit_pct", self.take_profit_pct),
            ("stop_loss_pct", self.stop_loss_pct),
        ];
        for (name, value) in scalars.into_iter().chain(
            optional
                .into_iter()
                .filter_map(|(name, value)| value.map(|value| (name, value))),
        ) {
            if !value.is_finite() || value.abs() > 1e12 {
                return Err(format!(
                    "{name} must be finite and within the supported numeric range"
                ));
            }
        }
        if self.initial_balance > 1e9
            || self.gas_usd > 1e6
            || self.maker_gas_usd.is_some_and(|v| v > 1e6)
            || self.min_trade_usd > 1e9
        {
            return Err(
                "initial_balance/min_trade_usd must be <=1e9; gas costs must be <=1e6".into(),
            );
        }
        for (name, price) in [
            ("entry_price_yes", self.entry_price_yes),
            ("entry_price_no", self.entry_price_no),
        ] {
            if !price.is_finite() || !(0.001..=0.999).contains(&price) {
                return Err(format!(
                    "{name} must be a finite fixed synthetic price between 0.001 and 0.999"
                ));
            }
        }
        let symmetric = self.entry_price_yes == 0.5 && self.entry_price_no == 0.5;
        if !symmetric
            && super::exit::parse_exit_mode(self.exit_mode.as_deref().unwrap_or("next_candle"))
                == Some(super::exit::ExitMode::TakerTpSl)
        {
            return Err("taker_tp_sl supports only entry_price_yes=entry_price_no=0.50; its candle proxy is anchored at 0.50 and has no historical token path".into());
        }
        if !symmetric && self.adaptive_stand_down {
            return Err("adaptive_stand_down requires both fixed entry prices to be 0.50; asymmetric-price win-rate break-even is not modeled".into());
        }
        if self.regime_allowlist.len() > 128
            || self.session_allowlist.len() > 3
            || self.blocked_hours_utc.len() > 24
            || self.regime_side_modes.len() > 128
        {
            return Err("backtest filter lists exceed the supported bounds".into());
        }
        for text in self
            .regime_allowlist
            .iter()
            .chain(&self.session_allowlist)
            .chain(self.regime_side_modes.keys())
            .chain(self.regime_side_modes.values())
        {
            if text.len() > 64 {
                return Err("backtest filter names must be at most 64 bytes".into());
            }
        }
        if self.session_allowlist.iter().any(|s| {
            !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "asia" | "europe" | "us"
            )
        }) {
            return Err("session_allowlist entries must be asia, europe or us".into());
        }
        if self.blocked_hours_utc.iter().any(|hour| *hour > 23) {
            return Err("blocked_hours_utc entries must be between 0 and 23".into());
        }
        let mut regimes = std::collections::HashSet::new();
        for regime in self.regime_side_modes.keys() {
            let key = regime.trim().to_ascii_uppercase();
            if key.is_empty() || !regimes.insert(key) {
                return Err(
                    "regime_side_modes contains an empty or duplicate normalized regime".into(),
                );
            }
        }
        if let Some(end) = self.end_time_ms {
            if end < 0
                || chrono::DateTime::from_timestamp_millis(end).is_none()
                || end > chrono::Utc::now().timestamp_millis()
            {
                return Err("end_time_ms must be a valid nonnegative UTC millisecond timestamp no later than now".into());
            }
        }
        Ok(())
    }
}

fn default_mode() -> String {
    "synthetic_ta".into()
}
fn default_entry_price() -> f64 {
    0.50
}

fn default_days() -> u32 {
    7
}
fn default_balance() -> f64 {
    100.0
}
fn default_gas_usd() -> f64 {
    0.02
}
fn default_liquidity_proxy_usd() -> f64 {
    5_000.0
}
fn default_max_trade_pct() -> f64 {
    0.05
}
fn default_window() -> usize {
    50
}
fn default_entry_threshold() -> f64 {
    0.25
}
fn default_min_trade_usd() -> f64 {
    0.25
}
fn default_edge_safety_buffer() -> f64 {
    0.0015
}
fn default_high_volatility_threshold() -> f64 {
    0.004
}
fn default_anti_bias_strength() -> f64 {
    0.14
}
fn default_stand_down_window() -> usize {
    20
}
fn default_stand_down_margin_pp() -> f64 {
    0.5
}
fn default_stand_down_cooldown_candles() -> usize {
    24
}
fn default_true() -> bool {
    true
}
