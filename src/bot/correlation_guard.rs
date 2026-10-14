//! Correlation regime switching.
//!
//! When BTC-ETH correlation breaks down (usually during black swan events),
//! switch to defensive mode: smaller positions, tighter stops, skip low-confidence signals.

use std::collections::VecDeque;
use std::time::Instant;

/// Rolling correlation tracker for BTC and ETH price changes.
#[derive(Debug, Clone, Default)]
pub struct CorrelationTracker {
    /// Recent (btc_ret, eth_ret) pairs — rolling window
    history: VecDeque<(f64, f64)>,
    max_window: usize,
    /// Last computed correlation
    last_correlation: f64,
    last_updated: Option<Instant>,
}

impl CorrelationTracker {
    pub fn new(window: usize) -> Self {
        CorrelationTracker {
            history: VecDeque::with_capacity(window + 10),
            max_window: window,
            last_correlation: 1.0, // Start assuming correlated
            last_updated: None,
        }
    }

    /// Add a new observation.
    pub fn push(&mut self, btc_ret: f64, eth_ret: f64) {
        self.history.push_back((btc_ret, eth_ret));
        if self.history.len() > self.max_window {
            self.history.pop_front();
        }
        self.last_updated = Some(Instant::now());
        self.last_correlation = self.compute();
    }

    /// Compute Pearson correlation from current window.
    pub fn compute(&self) -> f64 {
        let n = self.history.len();
        if n < 5 {
            return 1.0; // Not enough data — assume correlated
        }

        let btc: Vec<f64> = self.history.iter().map(|(b, _)| *b).collect();
        let eth: Vec<f64> = self.history.iter().map(|(_, e)| *e).collect();

        let mean_btc = btc.iter().sum::<f64>() / n as f64;
        let mean_eth = eth.iter().sum::<f64>() / n as f64;

        let mut cov = 0.0_f64;
        let mut var_btc = 0.0_f64;
        let mut var_eth = 0.0_f64;

        for i in 0..n {
            let db = btc[i] - mean_btc;
            let de = eth[i] - mean_eth;
            cov += db * de;
            var_btc += db * db;
            var_eth += de * de;
        }

        let denom = (var_btc * var_eth).sqrt();
        if denom < 1e-10 {
            return 1.0;
        }

        (cov / denom).clamp(-1.0, 1.0)
    }

    /// Returns current correlation (from last update).
    pub fn correlation(&self) -> f64 {
        self.last_correlation
    }

    /// Returns true if we're in a decorrelated (black swan) regime.
    /// Threshold: correlation drops below 0.50 (normally > 0.80).
    pub fn is_decorrelated(&self) -> bool {
        self.history.len() >= 10 && self.last_correlation < 0.50
    }

    /// Position size multiplier for current correlation regime.
    /// Returns 1.0 if correlated, 0.5 if decorrelated (defensive mode).
    pub fn position_size_multiplier(&self) -> f64 {
        if self.is_decorrelated() {
            tracing::warn!(
                "CorrelationGuard: BTC-ETH correlation={:.2} — defensive mode (50% size)",
                self.last_correlation
            );
            0.5
        } else {
            1.0
        }
    }
}
