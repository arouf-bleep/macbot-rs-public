//! Sleep schedule: track P&L by hour of day, skip consistently losing hours.
//!
//! Analyzes historical trade outcomes by UTC hour. If an hour has a
//! consistently negative win rate, the bot skips trading during that hour.

use std::collections::HashMap;

/// Rolling win rate and P&L tracker per UTC hour.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HourlyStats {
    pub wins: u32,
    pub losses: u32,
    pub total_pnl: f64,
}

impl HourlyStats {
    pub fn win_rate(&self) -> f64 {
        let total = (self.wins + self.losses) as f64;
        if total < 3.0 {
            return 0.5;
        } // Not enough data
        self.wins as f64 / total
    }

    pub fn avg_pnl(&self) -> f64 {
        let total = self.wins + self.losses;
        if total == 0 {
            return 0.0;
        }
        self.total_pnl / total as f64
    }
}

/// Sleep schedule: determines if the bot should trade in the current hour.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SleepSchedule {
    /// Hourly stats: hour (0-23 UTC) → stats
    pub hourly_stats: HashMap<u8, HourlyStats>,
    /// Minimum trades before applying sleep filter
    pub min_trades_for_filter: u32,
    /// Win rate below this → sleep
    pub min_win_rate_threshold: f64,
}

impl SleepSchedule {
    pub fn new() -> Self {
        SleepSchedule {
            hourly_stats: HashMap::new(),
            min_trades_for_filter: 20,
            min_win_rate_threshold: 0.45,
        }
    }

    /// Record a trade outcome for the given UTC hour.
    pub fn record_outcome(&mut self, hour: u8, won: bool, pnl: f64) {
        let stats = self.hourly_stats.entry(hour).or_default();
        if won {
            stats.wins += 1;
        } else {
            stats.losses += 1;
        }
        stats.total_pnl += pnl;
    }

    /// Check if we should trade in the given UTC hour.
    /// Returns true if trading is allowed.
    pub fn should_trade(&self, hour: u8) -> bool {
        let stats = match self.hourly_stats.get(&hour) {
            Some(s) => s,
            None => return true, // No data yet — trade
        };

        let total = stats.wins + stats.losses;
        if total < self.min_trades_for_filter {
            return true; // Not enough data — trade
        }

        let wr = stats.win_rate();
        if wr < self.min_win_rate_threshold {
            tracing::info!(
                "SleepSchedule: hour {} UTC has {:.1}% win rate ({} trades) — skipping",
                hour,
                wr * 100.0,
                total
            );
            return false;
        }

        true
    }

    /// Get a summary of all hourly stats for display.
    pub fn summary(&self) -> Vec<(u8, f64, f64, u32)> {
        let mut out: Vec<(u8, f64, f64, u32)> = self
            .hourly_stats
            .iter()
            .map(|(&h, s)| (h, s.win_rate(), s.avg_pnl(), s.wins + s.losses))
            .collect();
        out.sort_by_key(|(h, _, _, _)| *h);
        out
    }

    /// Build from historical trades. Call once at startup.
    pub fn rebuild_from_trades(&mut self, trades: &[crate::db::Trade]) {
        use chrono::Timelike;
        self.hourly_stats.clear();
        for trade in trades {
            if trade.status_kind() == crate::domain::TradeStatus::Closed {
                // Parse hour from timestamp
                if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&trade.timestamp) {
                    let hour = dt.with_timezone(&chrono::Utc).hour() as u8;
                    let pnl = trade.net_realized_pnl();
                    let won = pnl > 0.0;
                    self.record_outcome(hour, won, pnl);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SleepSchedule;
    use crate::db::Trade;

    fn closed_trade_at(timestamp: &str, pnl: f64) -> Trade {
        Trade {
            id: 1,
            timestamp: timestamp.to_string(),
            market_id: "m1".to_string(),
            question: "q".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.5,
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.01,
            reason: "test".to_string(),
            confidence: 0.6,
            status: "CLOSED".to_string(),
            mode: "simulation".to_string(),
            source: "test".to_string(),
            end_date: String::new(),
            exit_price: Some(0.6),
            exit_timestamp: Some(timestamp.to_string()),
            pnl: Some(pnl),
            exit_type: Some("tp".to_string()),
            was_correct: Some((pnl > 0.0) as i64),
            forced: Some(0),
            partial_closed: Some(0),
            raw_confidence: Some(0.6),
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: None,
            run_id: None,
            strategy: "conviction".to_string(),
        }
    }

    #[test]
    fn rebuild_from_trades_counts_closed_rows() {
        let mut schedule = SleepSchedule::new();
        schedule.rebuild_from_trades(&[
            closed_trade_at("2026-03-20T14:05:00Z", 1.0),
            closed_trade_at("2026-03-20T14:25:00Z", -0.5),
        ]);

        let stats = schedule.hourly_stats.get(&14).cloned().unwrap_or_default();
        assert_eq!(stats.wins, 1);
        assert_eq!(stats.losses, 1);
    }
}
