//! Binance forced liquidation feed via WebSocket.
//!
//! Streams `forceOrder` events from Binance Futures and feeds them into
//! the DataLogger + maintains an in-memory rolling summary for real-time
//! signal consumption.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::ml::data_logger::DataLogger;

// ── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LiquidationEvent {
    pub ts: Instant,
    pub side: LiqSide,
    pub size_usd: f64,
    pub price: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiqSide {
    Long,
    Short,
}

impl LiqSide {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Long => "LONG",
            Self::Short => "SHORT",
        }
    }
}

/// Rolling liquidation tracker — maintains windowed summaries.
pub struct LiquidationTracker {
    events: VecDeque<LiquidationEvent>,
    max_events: usize,
}

impl Default for LiquidationTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl LiquidationTracker {
    pub fn new() -> Self {
        Self {
            events: VecDeque::with_capacity(1024),
            max_events: 2048,
        }
    }

    pub fn push(&mut self, event: LiquidationEvent) {
        self.events.push_back(event);
        while self.events.len() > self.max_events {
            self.events.pop_front();
        }
    }

    /// Prune events older than `max_age`.
    pub fn prune(&mut self, max_age: Duration) {
        let cutoff = Instant::now() - max_age;
        while let Some(front) = self.events.front() {
            if front.ts < cutoff {
                self.events.pop_front();
            } else {
                break;
            }
        }
    }

    /// Liquidation volume in the last `window` seconds.
    pub fn volume_in_window(&self, window_secs: u64) -> LiqWindowSummary {
        let cutoff = Instant::now() - Duration::from_secs(window_secs);
        let mut long_vol = 0.0;
        let mut short_vol = 0.0;
        let mut count = 0u32;

        for e in self.events.iter().rev() {
            if e.ts < cutoff {
                break;
            }
            match e.side {
                LiqSide::Long => long_vol += e.size_usd,
                LiqSide::Short => short_vol += e.size_usd,
            }
            count += 1;
        }

        LiqWindowSummary {
            long_vol,
            short_vol,
            total_vol: long_vol + short_vol,
            ratio: if long_vol + short_vol > 0.0 {
                long_vol / (long_vol + short_vol)
            } else {
                0.5
            },
            count,
        }
    }

    /// Spike detection: Z-score of recent liquidation volume vs rolling baseline.
    ///
    /// Buckets the 5-minute baseline into 10 × 30-second windows, computes
    /// mean and standard deviation of per-window volumes, then measures how
    /// many standard deviations the most recent 30s window sits above the mean.
    pub fn spike_z_score(&self) -> f64 {
        let recent = self.volume_in_window(30); // 30s window
        let baseline = self.volume_in_window(300); // 5 min baseline

        if baseline.count < 5 || baseline.total_vol < 100.0 {
            return 0.0; // Not enough data
        }

        // Bucket baseline events into 30-second windows for proper variance
        let now = Instant::now();
        let n_buckets: usize = 10; // 300s / 30s
        let mut buckets = vec![0.0f64; n_buckets];
        let cutoff = now.checked_sub(Duration::from_secs(300)).unwrap_or(now);
        for ev in &self.events {
            if ev.ts < cutoff {
                continue;
            }
            let age_secs = now.duration_since(ev.ts).as_secs_f64();
            let bucket_idx = ((age_secs / 30.0) as usize).min(n_buckets - 1);
            buckets[bucket_idx] += ev.size_usd;
        }

        let mean: f64 = buckets.iter().sum::<f64>() / n_buckets as f64;
        let variance: f64 =
            buckets.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n_buckets as f64;
        let std_dev = variance.sqrt().max(1.0); // floor at $1 to avoid div-by-zero

        (recent.total_vol - mean) / std_dev
    }
}

#[derive(Debug, Clone)]
pub struct LiqWindowSummary {
    pub long_vol: f64,
    pub short_vol: f64,
    pub total_vol: f64,
    /// long_vol / total_vol (0.5 = balanced)
    pub ratio: f64,
    pub count: u32,
}

/// Signal data derived from liquidation tracker.
#[derive(Debug, Clone)]
pub struct LiquidationData {
    pub direction: String, // "UP" | "DOWN" | "NEUTRAL"
    pub confidence: f64,
    pub vol_30s: f64,
    pub vol_60s: f64,
    pub vol_300s: f64,
    pub long_short_ratio: f64,
    pub spike_z: f64,
}

/// Compute liquidation signal from tracker.
pub fn compute_liquidation_signal(tracker: &LiquidationTracker) -> LiquidationData {
    let w30 = tracker.volume_in_window(30);
    let w60 = tracker.volume_in_window(60);
    let w300 = tracker.volume_in_window(300);
    let spike_z = tracker.spike_z_score();

    // Short squeeze logic: if mostly longs are getting liquidated → BTC dropping → might bounce
    // If shorts are liquidated → BTC pumping → momentum continuation
    let (direction, confidence) = if spike_z > 2.0 && w30.total_vol > 50_000.0 {
        let base_conf = 0.55 + (spike_z - 2.0).min(3.0) * 0.05;
        if w30.ratio > 0.65 {
            // Heavy long liquidations → longs getting wiped → bearish cascade
            // Contrarian bounce is secondary; primary signal is DOWN pressure
            ("DOWN".to_string(), base_conf)
        } else if w30.ratio < 0.35 {
            // Heavy short liquidations → shorts squeezed → bullish momentum
            ("UP".to_string(), base_conf)
        } else {
            ("NEUTRAL".to_string(), 0.50)
        }
    } else {
        ("NEUTRAL".to_string(), 0.50)
    };

    LiquidationData {
        direction,
        confidence: confidence.min(0.80),
        vol_30s: w30.total_vol,
        vol_60s: w60.total_vol,
        vol_300s: w300.total_vol,
        long_short_ratio: w30.ratio,
        spike_z,
    }
}

// ── WebSocket feed ──────────────────────────────────────────────────────────

const BINANCE_FORCE_ORDER_URL: &str = "wss://fstream.binance.com/ws/btcusdt@forceOrder";

/// Spawn a background task that streams Binance liquidation events.
pub fn spawn_liquidation_feed(
    tracker: Arc<Mutex<LiquidationTracker>>,
    data_logger: Arc<DataLogger>,
) {
    crate::task_monitor::spawn_monitored_task("liquidation_feed", async move {
        let mut backoff_secs = 1u64;

        loop {
            info!("Liquidation feed: connecting to Binance forceOrder stream");

            match crate::tls::connect_feed(BINANCE_FORCE_ORDER_URL).await {
                Ok((ws_stream, _)) => {
                    backoff_secs = 1; // Reset backoff on successful connect
                    let (_write, mut read) = ws_stream.split();

                    loop {
                        let msg = tokio::time::timeout(Duration::from_secs(60), read.next()).await;

                        match msg {
                            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(text)))) => {
                                if let Some(event) = parse_force_order(&text) {
                                    // Log to DataLogger for persistence
                                    data_logger.log_liquidation(
                                        event.side.as_str(),
                                        event.size_usd,
                                        event.price,
                                        "BTCUSDT",
                                    );

                                    // Push to in-memory tracker
                                    let mut t = tracker.lock().await;
                                    t.push(event);
                                    t.prune(Duration::from_secs(600)); // Keep 10 min
                                }
                            }
                            Ok(Some(Ok(_))) => {} // Ping/pong/binary — ignore
                            Ok(Some(Err(e))) => {
                                warn!("Liquidation feed: WS error: {e}");
                                break;
                            }
                            Ok(None) => {
                                warn!("Liquidation feed: stream ended");
                                break;
                            }
                            Err(_) => {
                                // Timeout — no liquidations in 60s is normal during calm markets
                                continue;
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Liquidation feed: connect failed: {e}");
                }
            }

            let sleep = backoff_secs.min(30);
            tokio::time::sleep(Duration::from_secs(sleep)).await;
            backoff_secs = (backoff_secs * 2).min(30);
        }
    });
}

/// Parse a Binance forceOrder WebSocket message.
///
/// Example payload:
/// ```json
/// {
///   "e": "forceOrder",
///   "E": 1710000000000,
///   "o": {
///     "s": "BTCUSDT",
///     "S": "SELL",        // SELL = long liquidation, BUY = short liquidation
///     "o": "LIMIT",
///     "f": "IOC",
///     "q": "0.001",       // quantity in BTC
///     "p": "65000.00",    // price
///     "ap": "64950.00",   // average fill price
///     "X": "FILLED",
///     "l": "0.001",       // last fill qty
///     "z": "0.001",       // cumulative filled qty
///     "T": 1710000000000
///   }
/// }
/// ```
fn parse_force_order(text: &str) -> Option<LiquidationEvent> {
    let v: Value = serde_json::from_str(text).ok()?;
    let order = v.get("o")?;

    let trade_side = order.get("S")?.as_str()?;
    let qty = order
        .get("z")
        .or_else(|| order.get("q"))
        .and_then(crate::json_util::parse_f64)?;
    let price = order
        .get("ap")
        .or_else(|| order.get("p"))
        .and_then(crate::json_util::parse_f64)?;
    if qty <= 0.0 || price <= 0.0 {
        return None;
    }
    let size_usd = qty * price;
    if !size_usd.is_finite() {
        return None;
    }

    // Binance: SELL order = long position being liquidated
    //          BUY order = short position being liquidated
    let side = match trade_side {
        "SELL" => LiqSide::Long,
        "BUY" => LiqSide::Short,
        _ => return None,
    };

    Some(LiquidationEvent {
        ts: Instant::now(),
        side,
        size_usd,
        price,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_force_order() {
        let json = r#"{
            "e": "forceOrder",
            "E": 1710000000000,
            "o": {
                "s": "BTCUSDT",
                "S": "SELL",
                "o": "LIMIT",
                "f": "IOC",
                "q": "0.100",
                "p": "65000.00",
                "ap": "64950.00",
                "X": "FILLED",
                "l": "0.100",
                "z": "0.100",
                "T": 1710000000000
            }
        }"#;

        let event = parse_force_order(json).expect("should parse");
        assert_eq!(event.side, LiqSide::Long);
        assert!((event.size_usd - 6495.0).abs() < 0.01);
        assert!((event.price - 64950.0).abs() < 0.01);
    }

    #[test]
    fn test_tracker_window() {
        let mut tracker = LiquidationTracker::new();
        tracker.push(LiquidationEvent {
            ts: Instant::now(),
            side: LiqSide::Long,
            size_usd: 10000.0,
            price: 65000.0,
        });
        tracker.push(LiquidationEvent {
            ts: Instant::now(),
            side: LiqSide::Short,
            size_usd: 5000.0,
            price: 65100.0,
        });

        let summary = tracker.volume_in_window(30);
        assert_eq!(summary.count, 2);
        assert!((summary.total_vol - 15000.0).abs() < 0.01);
        assert!(summary.ratio > 0.6); // More longs than shorts
    }
}
