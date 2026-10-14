//! Assembles `DeepModelInput` tensors from in-memory ring buffers and
//! current fusion breakdown for inference.
//!
//! Handles multi-resolution candle building, orderbook sequence assembly,
//! liquidation flow embedding, feature staleness tracking, and scalar
//! context packing.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use super::deep_model::DeepModelInput;
use super::model::NUM_FEATURES;
use super::FEATURE_NAMES;

// ── Ring buffer types ───────────────────────────────────────────────────────

/// Single 1-min OHLCV bar.
#[derive(Debug, Clone, Copy)]
pub struct MiniCandle {
    pub open: f32,
    pub high: f32,
    pub low: f32,
    pub close: f32,
    pub volume: f32,
    pub ts: Instant,
}

/// Single L2 orderbook snapshot (top 10 levels each side + flow).
#[derive(Debug, Clone)]
pub struct ObSnapshot {
    /// [bid_price_0, bid_size_0, ..., bid_price_9, bid_size_9,
    ///  ask_price_0, ask_size_0, ..., ask_price_9, ask_size_9] = 40 values
    pub levels: Vec<f32>,
    /// [buy_flow, sell_flow] = 2 values
    pub flow: [f32; 2],
    /// [liq_long_vol, liq_short_vol, liq_net] = 3 values
    pub liquidation: [f32; 3],
    pub ts: Instant,
}

/// Feature staleness entry: when each signal was last refreshed.
#[derive(Debug, Clone)]
pub struct FeatureStaleness {
    pub last_updated: [Option<Instant>; NUM_FEATURES],
}

impl Default for FeatureStaleness {
    fn default() -> Self {
        Self::new()
    }
}

impl FeatureStaleness {
    pub fn new() -> Self {
        Self {
            last_updated: [None; NUM_FEATURES],
        }
    }

    pub fn mark_fresh(&mut self, idx: usize) {
        if idx < NUM_FEATURES {
            self.last_updated[idx] = Some(Instant::now());
        }
    }

    pub fn mark_all_fresh(&mut self) {
        let now = Instant::now();
        for slot in self.last_updated.iter_mut() {
            *slot = Some(now);
        }
    }

    pub fn record_availability(&mut self, idx: usize, available: bool, age_secs: u64) {
        if idx < NUM_FEATURES {
            self.last_updated[idx] = if available {
                Instant::now().checked_sub(std::time::Duration::from_secs(age_secs))
            } else {
                None
            };
        }
    }

    /// Return staleness as normalized seconds (capped at 300s, divided by 300).
    pub fn as_normalized(&self) -> Vec<f32> {
        let now = Instant::now();
        self.last_updated
            .iter()
            .map(|opt| {
                opt.map(|t| (now.duration_since(t).as_secs_f32() / 300.0).min(1.0))
                    .unwrap_or(1.0) // Unknown = maximally stale
            })
            .collect()
    }
}

// ── Input Builder ───────────────────────────────────────────────────────────

/// Accumulates streaming data and builds model input tensors on demand.
pub struct InputBuilder {
    /// 1-minute candles (newest at back)
    pub candles_1m: VecDeque<MiniCandle>,
    /// L2 orderbook snapshots at 5s intervals (newest at back)
    pub ob_snapshots: VecDeque<ObSnapshot>,
    orderbook_market_id: Option<String>,
    /// Feature staleness tracker
    pub staleness: FeatureStaleness,
    /// Cross-exchange price discovery lag estimate (seconds)
    pub price_discovery_lag: f32,
    /// Time remaining in current market window (fraction 0..1)
    pub time_remaining_frac: f32,
    /// Current balance as fraction of initial
    pub balance_frac: f32,
    /// Number of open positions
    pub open_positions: f32,
    /// Current BTC price (for normalization)
    pub btc_price: f32,
    /// Current Polymarket contract price
    pub contract_price: f32,
}

impl Default for InputBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl InputBuilder {
    pub fn new() -> Self {
        Self {
            candles_1m: VecDeque::with_capacity(256),
            ob_snapshots: VecDeque::with_capacity(16),
            orderbook_market_id: None,
            staleness: FeatureStaleness::new(),
            price_discovery_lag: 0.0,
            time_remaining_frac: 0.5,
            balance_frac: 1.0,
            open_positions: 0.0,
            btc_price: 0.0,
            contract_price: 0.5,
        }
    }

    /// Push a new 1-minute candle. Auto-caps at 240 entries.
    pub fn push_candle(&mut self, candle: MiniCandle) {
        self.candles_1m.push_back(candle);
        while self.candles_1m.len() > 240 {
            self.candles_1m.pop_front();
        }
    }

    /// Replace the full 1m candle window with an exact recent slice.
    pub fn replace_candles<I>(&mut self, candles: I)
    where
        I: IntoIterator<Item = MiniCandle>,
    {
        let mut rebuilt = VecDeque::with_capacity(256);
        for candle in candles {
            rebuilt.push_back(candle);
            if rebuilt.len() > 240 {
                rebuilt.pop_front();
            }
        }
        self.candles_1m = rebuilt;
    }

    /// Push a new orderbook snapshot. Auto-caps at 16 entries.
    pub fn push_ob_snapshot(&mut self, snap: ObSnapshot) {
        self.ob_snapshots.push_back(snap);
        while self.ob_snapshots.len() > 16 {
            self.ob_snapshots.pop_front();
        }
    }

    /// Do not carry another contract's orderbook into the current prediction.
    pub fn set_orderbook_market(&mut self, market_id: &str) {
        if self.orderbook_market_id.as_deref() != Some(market_id) {
            self.ob_snapshots.clear();
            self.orderbook_market_id = Some(market_id.to_owned());
        }
    }

    /// Runtime analysis may run faster than the five-second sequence cadence.
    pub fn record_orderbook_snapshot(&mut self, snap: ObSnapshot) {
        if snap
            .levels
            .iter()
            .chain(snap.flow.iter())
            .chain(snap.liquidation.iter())
            .any(|x| !x.is_finite())
        {
            return;
        }
        if self.ob_snapshots.back().is_some_and(|last| {
            snap.ts.saturating_duration_since(last.ts) < std::time::Duration::from_secs(5)
        }) {
            return;
        }
        self.push_ob_snapshot(snap);
    }

    /// Build the full input tensor from current state + fusion breakdown.
    ///
    /// `breakdown`: the fusion breakdown HashMap (up to NUM_FEATURES signals)
    /// `regime`: current market regime string ("TRENDING", "CHOP", etc.)
    pub fn build(&self, breakdown: &HashMap<String, f64>, regime: &str) -> DeepModelInput {
        DeepModelInput {
            candles_1m: self.build_candles_1m(),
            candles_5m: self.build_candles_5m(),
            candles_15m: self.build_candles_15m(),
            orderbook_seq: self.build_orderbook_seq(),
            scalars: self.build_scalars(breakdown),
            staleness: self.staleness.as_normalized(),
            regime: regime_to_float(regime),
        }
    }

    // ── Candle builders ─────────────────────────────────────────────────────

    /// Last 60 1-min candles as [60, 5] flattened. Zero-padded if insufficient.
    fn build_candles_1m(&self) -> Vec<f32> {
        build_candle_tensor(&self.candles_1m, 60, 1)
    }

    /// Aggregate 1m candles into 5m bars: last 24 bars = 2 hours. [24, 5]
    fn build_candles_5m(&self) -> Vec<f32> {
        build_candle_tensor(&self.candles_1m, 24, 5)
    }

    /// Aggregate 1m candles into 15m bars: last 16 bars = 4 hours. [16, 5]
    fn build_candles_15m(&self) -> Vec<f32> {
        build_candle_tensor(&self.candles_1m, 16, 15)
    }

    // ── Orderbook builder ───────────────────────────────────────────────────

    /// Last 12 OB snapshots as [12, 45] flattened.
    /// Each snapshot: 40 (levels) + 2 (flow) + 3 (liquidation) = 45
    fn build_orderbook_seq(&self) -> Vec<f32> {
        let target = 12;
        let width = 45;
        let mut out = vec![0.0f32; target * width];
        let n = self.ob_snapshots.len().min(target);
        let start = self.ob_snapshots.len().saturating_sub(target);

        for (i, snap) in self.ob_snapshots.iter().skip(start).enumerate() {
            let offset = (target - n + i) * width;
            let levels_len = snap.levels.len().min(40);
            out[offset..offset + levels_len].copy_from_slice(&snap.levels[..levels_len]);
            out[offset + 40] = snap.flow[0];
            out[offset + 41] = snap.flow[1];
            out[offset + 42] = snap.liquidation[0];
            out[offset + 43] = snap.liquidation[1];
            out[offset + 44] = snap.liquidation[2];
        }
        out
    }

    // ── Scalar builder ──────────────────────────────────────────────────────

    /// NUM_FEATURES fusion features + 6 context vars = scalars
    fn build_scalars(&self, breakdown: &HashMap<String, f64>) -> Vec<f32> {
        let mut scalars = Vec::with_capacity(NUM_FEATURES + 6);

        // All fusion features in canonical order
        for name in FEATURE_NAMES.iter() {
            scalars.push(breakdown.get(*name).copied().unwrap_or(0.0) as f32);
        }

        // 6 context variables
        scalars.push(self.time_remaining_frac);
        scalars.push(self.balance_frac);
        scalars.push(self.open_positions);
        scalars.push(self.contract_price);
        scalars.push(self.price_discovery_lag);
        scalars.push(self.btc_price / 100_000.0); // Normalize to ~1.0 range

        scalars
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Build a candle tensor from 1m candles with aggregation factor.
fn build_candle_tensor(
    candles: &VecDeque<MiniCandle>,
    target_bars: usize,
    agg_factor: usize,
) -> Vec<f32> {
    let width = 5;
    let mut out = vec![0.0; target_bars * width];
    let needed = target_bars * agg_factor;
    let available = candles.len().min(needed);
    let padding = needed - available;
    let source_start = candles.len() - available;
    for bar in 0..target_bars {
        let end = (bar + 1) * agg_factor;
        if end <= padding {
            continue;
        }
        let start = (bar * agg_factor).max(padding);
        let mut chunk = candles
            .iter()
            .skip(source_start + start - padding)
            .take(end - start);
        let Some(first) = chunk.next() else {
            continue;
        };
        let (mut high, mut low, mut close, mut volume) =
            (first.high, first.low, first.close, first.volume);
        for candle in chunk {
            high = high.max(candle.high);
            low = low.min(candle.low);
            close = candle.close;
            volume += candle.volume;
        }
        out[bar * width..(bar + 1) * width]
            .copy_from_slice(&[first.open, high, low, close, volume]);
    }
    out
}

fn regime_to_float(regime: &str) -> f32 {
    // Maps regime classifier output to ordinal float for deep model input.
    // Ordering: strong-bear … neutral … strong-bull, with vol extremes at edges.
    match regime.to_uppercase().as_str() {
        // Current regime classifier labels (regime.rs)
        "TREND_UP" => 1.0,
        "TREND_DOWN" => -1.0,
        "CHOP" | "CHOPPY" => 0.0,
        "RANGE" => 0.1,    // slightly above neutral — mean-reverting
        "BREAKOUT" => 1.5, // directional expansion
        "SQUEEZE" => 0.3,  // compressed vol, pre-breakout
        // Legacy labels (kept for backwards compat)
        "TRENDING" => 1.0,
        "HIGH_VOL" => 2.0,
        _ => 0.5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_refresh_invalidates_even_a_recent_observation() {
        let mut freshness = FeatureStaleness::new();
        freshness.record_availability(24, true, 0);
        assert!(freshness.as_normalized()[24] < 0.01);
        freshness.record_availability(24, false, 0);
        assert_eq!(freshness.as_normalized()[24], 1.0);
        freshness.record_availability(5, true, 150);
        assert!((freshness.as_normalized()[5] - 0.5).abs() < 0.01);
    }

    #[test]
    fn cold_candle_windows_keep_newest_bars_at_the_end() {
        let candles: VecDeque<_> = (1..=7).map(|value| mini(value as f32)).collect();
        let result = build_candle_tensor(&candles, 3, 5);
        assert_eq!(&result[..5], &[0.0; 5]);
        assert_eq!(&result[5..10], &[1.0, 3.0, 0.0, 2.5, 20.0]);
        assert_eq!(&result[10..], &[3.0, 8.0, 2.0, 7.5, 50.0]);
        let minute = build_candle_tensor(&candles, 10, 1);
        assert_eq!(&minute[..15], &[0.0; 15]);
        assert_eq!(minute[48], 7.5);
    }

    #[test]
    fn runtime_orderbook_sequence_has_cadence_and_resets_at_market_roll() {
        let mut builder = InputBuilder::new();
        builder.set_orderbook_market("first");
        let now = Instant::now();
        let snapshot = ObSnapshot {
            levels: vec![0.4, 10.0],
            flow: [1.0, 2.0],
            liquidation: [0.0; 3],
            ts: now,
        };
        builder.record_orderbook_snapshot(snapshot.clone());
        builder.record_orderbook_snapshot(snapshot.clone());
        assert_eq!(builder.ob_snapshots.len(), 1);
        assert_eq!(&builder.build_orderbook_seq()[495..497], &[0.4, 10.0]);
        let mut later = snapshot.clone();
        later.ts = now + std::time::Duration::from_secs(5);
        builder.record_orderbook_snapshot(later);
        assert_eq!(builder.ob_snapshots.len(), 2);
        builder.set_orderbook_market("second");
        assert!(builder
            .build_orderbook_seq()
            .iter()
            .all(|value| *value == 0.0));
        let mut invalid = snapshot;
        invalid.levels[0] = f32::NAN;
        builder.record_orderbook_snapshot(invalid);
        assert!(builder.ob_snapshots.is_empty());
    }

    fn mini(open: f32) -> MiniCandle {
        MiniCandle {
            open,
            high: open + 1.0,
            low: open - 1.0,
            close: open + 0.5,
            volume: 10.0,
            ts: Instant::now(),
        }
    }

    #[test]
    fn test_staleness_all_fresh() {
        let mut s = FeatureStaleness::new();
        s.mark_all_fresh();
        let norm = s.as_normalized();
        assert_eq!(norm.len(), NUM_FEATURES);
        for v in &norm {
            assert!(*v < 0.01, "freshly marked should be near 0");
        }
    }

    #[test]
    fn test_staleness_unknown_is_max() {
        let s = FeatureStaleness::new();
        let norm = s.as_normalized();
        for v in &norm {
            assert!((*v - 1.0).abs() < 0.001, "unknown should be 1.0");
        }
    }

    #[test]
    fn test_build_candle_1m_zero_padded() {
        let builder = InputBuilder::new();
        let tensor = builder.build_candles_1m();
        assert_eq!(tensor.len(), 60 * 5);
        assert!(tensor.iter().all(|v| *v == 0.0));
    }

    #[test]
    fn replace_candles_keeps_exact_recent_window() {
        let mut builder = InputBuilder::new();
        let candles = (0..300).map(|i| mini(i as f32)).collect::<Vec<_>>();

        builder.replace_candles(candles);

        assert_eq!(builder.candles_1m.len(), 240);
        assert_eq!(builder.candles_1m.front().map(|c| c.open), Some(60.0));
        assert_eq!(builder.candles_1m.back().map(|c| c.open), Some(299.0));
    }

    #[test]
    fn test_regime_to_float() {
        // Current regime classifier labels
        assert_eq!(regime_to_float("TREND_UP"), 1.0);
        assert_eq!(regime_to_float("TREND_DOWN"), -1.0);
        assert_eq!(regime_to_float("CHOP"), 0.0);
        assert_eq!(regime_to_float("RANGE"), 0.1);
        assert_eq!(regime_to_float("BREAKOUT"), 1.5);
        assert_eq!(regime_to_float("SQUEEZE"), 0.3);
        // Legacy labels
        assert_eq!(regime_to_float("TRENDING"), 1.0);
        assert_eq!(regime_to_float("HIGH_VOL"), 2.0);
        assert_eq!(regime_to_float("OTHER"), 0.5);
    }
}
