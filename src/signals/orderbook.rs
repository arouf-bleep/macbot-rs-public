use std::collections::VecDeque;
use std::time::Instant;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ObData {
    pub imbalance: f64,
    pub confidence: f64,
    pub direction: String,
    pub bid_usd: f64,
    pub ask_usd: f64,
    pub total_usd: f64,
    pub bid_wall: bool,
    pub ask_wall: bool,
}

/// Port of signals.py:compute_orderbook_imbalance()
pub fn compute_orderbook_imbalance(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> ObData {
    fn usd_depth(orders: &[(f64, f64)]) -> (f64, f64) {
        let mut total = 0.0_f64;
        let mut max = 0.0_f64;
        for &(p, s) in orders {
            let val = p * s;
            total += val;
            if val > max {
                max = val;
            }
        }
        (total, max)
    }

    let (bid_usd, max_bid) = usd_depth(bids);
    let (ask_usd, max_ask) = usd_depth(asks);
    let total = bid_usd + ask_usd;

    if total < 1.0 {
        return ObData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            ..Default::default()
        };
    }

    let imbalance = (bid_usd - ask_usd) / total;
    let bid_wall = max_bid > total * 0.20;
    let ask_wall = max_ask > total * 0.20;
    let confidence = (0.55 + imbalance.abs() * 0.40).min(0.95);

    let direction = if imbalance > 0.30 {
        "UP"
    } else if imbalance < -0.30 {
        "DOWN"
    } else {
        "NEUTRAL"
    }
    .to_string();

    ObData {
        imbalance,
        confidence,
        direction,
        bid_usd,
        ask_usd,
        total_usd: total,
        bid_wall,
        ask_wall,
    }
}

// ── OBI Stability Tracker ─────────────────────────────────────────────────────

/// Detects wash-trading / spoofing by flagging large OBI swings in a short window.
/// Port of signals.py:OBIStabilityTracker
pub struct OBIStabilityTracker {
    history: VecDeque<(Instant, f64)>,
    swing_threshold: f64,
    window_secs: f64,
    cooldown_secs: f64,
    unstable_until: Option<Instant>,
}

impl OBIStabilityTracker {
    pub fn new(swing_threshold: f64, window_secs: f64, cooldown_secs: f64) -> Self {
        Self {
            history: VecDeque::with_capacity(30),
            swing_threshold,
            window_secs,
            cooldown_secs,
            unstable_until: None,
        }
    }

    /// Returns true = stable, false = flagged (spoof/wash detected).
    pub fn update(&mut self, obi: f64) -> bool {
        let now = Instant::now();
        self.history.push_back((now, obi));

        // Trim history to maxlen=30
        while self.history.len() > 30 {
            self.history.pop_front();
        }

        // Still in cooldown?
        if let Some(until) = self.unstable_until {
            if now < until {
                return false;
            }
        }

        // Check for large swing within window
        let cutoff_secs = self.window_secs;
        let recent: Vec<f64> = self
            .history
            .iter()
            .filter(|(ts, _)| ts.elapsed().as_secs_f64() <= cutoff_secs)
            .map(|(_, v)| *v)
            .collect();

        if recent.len() >= 2 {
            let max = recent.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let min = recent.iter().cloned().fold(f64::INFINITY, f64::min);
            if max - min > self.swing_threshold {
                self.unstable_until =
                    Some(now + std::time::Duration::from_secs_f64(self.cooldown_secs));
                return false;
            }
        }

        true
    }

    pub fn is_stable(&self) -> bool {
        match self.unstable_until {
            None => true,
            Some(until) => Instant::now() >= until,
        }
    }

    pub fn cooldown_remaining_secs(&self) -> f64 {
        match self.unstable_until {
            None => 0.0,
            Some(until) => {
                let now = Instant::now();
                if now >= until {
                    0.0
                } else {
                    (until - now).as_secs_f64()
                }
            }
        }
    }
}

impl Default for OBIStabilityTracker {
    fn default() -> Self {
        Self::new(0.15, 2.0, 300.0)
    }
}
