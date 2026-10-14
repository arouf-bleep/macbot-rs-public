use std::collections::VecDeque;
use std::time::Instant;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TickVelData {
    pub velocity_60s: f64,
    pub velocity_30s: f64,
    pub acceleration: f64,
    pub direction: String,
    pub confidence: f64,
}

/// Port of signals.py:TickVelocityTracker
pub struct TickVelocityTracker {
    history: VecDeque<(Instant, f64)>,
    last_market_id: Option<String>,
}

impl Default for TickVelocityTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl TickVelocityTracker {
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(180),
            last_market_id: None,
        }
    }

    pub fn update(&mut self, prob: f64, market_id: Option<&str>) {
        if let Some(mid) = market_id {
            if self.last_market_id.as_deref() != Some(mid) {
                self.history.clear();
                self.last_market_id = Some(mid.to_string());
            }
        }
        self.history.push_back((Instant::now(), prob));
        while self.history.len() > 180 {
            self.history.pop_front();
        }
    }

    /// Return a snapshot of the history as (elapsed_secs, price) pairs for spike detection.
    pub fn history_snapshot(&self) -> Vec<(f64, f64)> {
        self.history
            .iter()
            .map(|(inst, p)| (inst.elapsed().as_secs_f64(), *p))
            .collect()
    }

    pub fn compute(&self) -> TickVelData {
        let now = Instant::now();
        let obs_60s: Vec<(f64, f64)> = self
            .history
            .iter()
            .filter(|(ts, _)| now.duration_since(*ts).as_secs_f64() <= 60.0)
            .map(|(ts, p)| (now.duration_since(*ts).as_secs_f64(), *p))
            .collect();

        if obs_60s.len() < 2 {
            return TickVelData {
                direction: "NEUTRAL".into(),
                confidence: 0.5,
                ..Default::default()
            };
        }

        // oldest → newest (safe: checked len >= 2 above)
        let first = &obs_60s[0];
        let last = &obs_60s[obs_60s.len() - 1];
        let dt_60 = (first.0 - last.0).abs().max(1.0);
        let vel_60s = (last.1 - first.1) / dt_60 * 60.0;

        let obs_30s: Vec<(f64, f64)> = self
            .history
            .iter()
            .filter(|(ts, _)| now.duration_since(*ts).as_secs_f64() <= 30.0)
            .map(|(ts, p)| (now.duration_since(*ts).as_secs_f64(), *p))
            .collect();

        let vel_30s = if obs_30s.len() >= 2 {
            let f = &obs_30s[0];
            let l = &obs_30s[obs_30s.len() - 1];
            let dt = (f.0 - l.0).abs().max(1.0);
            (l.1 - f.1) / dt * 60.0
        } else {
            0.0
        };

        // Acceleration = change in velocity (recent − older window)
        let accel = vel_30s - vel_60s;

        const THRESH_60: f64 = 0.015;
        const THRESH_30: f64 = 0.010;

        let (direction, mut base_conf) = if vel_60s.abs() < THRESH_60 && vel_30s.abs() < THRESH_30 {
            ("NEUTRAL".to_string(), 0.50)
        } else {
            let dir = if vel_60s > 0.0 { "UP" } else { "DOWN" }.to_string();
            let c = 0.55 + (vel_60s.abs() / THRESH_60.max(0.001) * 0.10).min(0.15);
            (dir, c)
        };

        if accel != 0.0 && (accel > 0.0) == (vel_60s > 0.0) {
            base_conf = (base_conf + 0.06).min(0.85);
        }
        if vel_60s != 0.0
            && vel_30s != 0.0
            && (vel_60s * vel_30s < 0.0)
            && vel_30s.abs() > THRESH_30
        {
            base_conf *= 0.80;
        }

        TickVelData {
            velocity_60s: (vel_60s * 10000.0).round() / 10000.0,
            velocity_30s: (vel_30s * 10000.0).round() / 10000.0,
            acceleration: (accel * 10000.0).round() / 10000.0,
            direction,
            confidence: base_conf.min(0.85),
        }
    }
}
