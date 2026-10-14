#![allow(dead_code, unused_imports)]
//! Counterfactual reasoning: pre-compute scenario trees before trade entry.
//!
//! "If BTC drops $100 in the next 3 minutes, what happens to our position?"
//! Pre-compute bull/bear/chop scenarios and have exit plans ready BEFORE entry.
//! This is proactive risk management, not reactive.
//!
//! Scenario magnitudes are derived from current realized volatility, not
//! fixed percentages. A 0.3% move at VIX 12 vs VIX 40 means very different
//! things — the scenario tree adapts accordingly.

/// A single scenario with pre-computed outcome.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: &'static str,
    /// Price move in this scenario (percent)
    pub btc_move_pct: f64,
    /// Expected YES token price after this move
    pub expected_yes_price: f64,
    /// Expected P&L on a YES position (before fees)
    pub yes_pnl_pct: f64,
    /// Expected P&L on a NO position (before fees)
    pub no_pnl_pct: f64,
    /// Probability of this scenario given current regime
    pub probability: f64,
}

/// Pre-computed scenario tree for a potential trade.
#[derive(Debug, Clone)]
pub struct ScenarioTree {
    pub scenarios: Vec<Scenario>,
    /// Expected value of a YES trade across all scenarios
    pub yes_ev: f64,
    /// Expected value of a NO trade across all scenarios
    pub no_ev: f64,
    /// Maximum loss in worst-case scenario (for YES position)
    pub max_loss_yes_pct: f64,
    /// Recommended action based on scenario analysis
    pub recommended_action: ScenarioAction,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScenarioAction {
    /// Strong YES signal confirmed across scenarios
    BuyYes,
    /// Strong NO signal confirmed across scenarios
    BuyNo,
    /// Too risky — hold
    Hold,
}

impl ScenarioTree {
    /// Build volatility-adaptive scenario tree for current market conditions.
    ///
    /// # Parameters
    /// - `yes_price`: current YES token price
    /// - `btc_momentum_pct`: current BTC momentum (price change since window open)
    /// - `regime`: current market regime
    /// - `time_remaining_mins`: minutes remaining in the 15m window
    /// - `confidence`: current signal confidence [0, 1]
    /// - `realized_vol`: realized volatility of 1-min returns (e.g. 0.0015 = 0.15%)
    pub fn build(
        yes_price: f64,
        btc_momentum_pct: f64,
        regime: &str,
        time_remaining_mins: f64,
        confidence: f64,
        realized_vol: f64,
    ) -> Self {
        // Scale moves by realized volatility and time remaining.
        // Expected move ≈ vol × sqrt(time) for random walk baseline.
        let time_scale = (time_remaining_mins / 15.0).clamp(0.05, 1.0).sqrt();

        // Use realized vol to set scenario magnitudes.
        // If vol is 0.15% per minute, a 1-sigma move over 5 min ≈ 0.15 * sqrt(5) ≈ 0.34%
        // Use 1σ for base, 2σ for extreme, 0.5σ for mild
        let vol_scaled = realized_vol.max(0.0005) * 100.0 * time_scale; // Convert to % and scale by time

        // Regime-specific scenario probabilities
        let (bull_prob, bear_prob, chop_prob, tail_prob) =
            regime_probabilities(regime, btc_momentum_pct, confidence);

        // Logistic sensitivity: how much YES price moves per % BTC move
        let sensitivity = 2.0 * yes_price * (1.0 - yes_price);

        // 5 scenarios: strong bull, mild bull, chop, mild bear, strong bear
        let scenarios_def: [(
            &str,
            f64, // move multiplier (× vol_scaled)
            f64, // probability
        ); 5] = [
            (
                "strong_bull",
                2.0,
                tail_prob * if btc_momentum_pct > 0.0 { 1.3 } else { 0.7 },
            ),
            ("mild_bull", 1.0, bull_prob),
            ("chop", 0.0, chop_prob),
            ("mild_bear", -1.0, bear_prob),
            (
                "strong_bear",
                -2.0,
                tail_prob * if btc_momentum_pct < 0.0 { 1.3 } else { 0.7 },
            ),
        ];

        let mut scenarios = Vec::with_capacity(5);
        let mut prob_sum = 0.0_f64;

        for &(name, mult, raw_prob) in &scenarios_def {
            prob_sum += raw_prob;
            let btc_move = mult * vol_scaled;
            let yes_after = (yes_price + btc_move / 100.0 * sensitivity).clamp(0.01, 0.99);
            let no_after = 1.0 - yes_after;
            let no_now = 1.0 - yes_price;

            let yes_pnl = if yes_price > 0.0 {
                (yes_after - yes_price) / yes_price
            } else {
                0.0
            };
            let no_pnl = if no_now > 0.0 {
                (no_after - no_now) / no_now
            } else {
                0.0
            };

            scenarios.push(Scenario {
                name: match name {
                    "strong_bull" => "strong_bull",
                    "mild_bull" => "mild_bull",
                    "chop" => "chop",
                    "mild_bear" => "mild_bear",
                    "strong_bear" => "strong_bear",
                    _ => "unknown",
                },
                btc_move_pct: btc_move,
                expected_yes_price: yes_after,
                yes_pnl_pct: yes_pnl,
                no_pnl_pct: no_pnl,
                probability: raw_prob,
            });
        }

        // Normalize probabilities to sum to 1.0
        if prob_sum > 0.0 {
            for s in &mut scenarios {
                s.probability /= prob_sum;
            }
        }

        // Compute expected values
        let yes_ev: f64 = scenarios
            .iter()
            .map(|s| s.probability * s.yes_pnl_pct)
            .sum();
        let no_ev: f64 = scenarios.iter().map(|s| s.probability * s.no_pnl_pct).sum();

        let max_loss_yes_pct = scenarios
            .iter()
            .map(|s| s.yes_pnl_pct)
            .fold(f64::INFINITY, f64::min);

        // Recommendation: require positive EV AND favorable risk/reward.
        // Scale threshold by:
        //   1. Market sensitivity — at extreme prices (0.05 or 0.95), price moves
        //      are ~10x smaller than at 0.50, so a fixed threshold blocks everything.
        //   2. Time remaining — shorter windows produce proportionally smaller EVs
        //      because BTC moves scale with sqrt(time). The scenarios already use
        //      time_scale for magnitudes, so the threshold must shrink to match.
        let sensitivity_at_50 = 0.5; // max sensitivity: 2 * 0.5 * 0.5
        let sensitivity_ratio = (sensitivity / sensitivity_at_50).clamp(0.05, 1.0);
        // Base threshold 0.01% — these 15-min BTC binary markets produce
        // EVs in the 0.01-0.10% range because BTC only moves ~0.3% in 15 min
        // and YES price near 0.50 has near-zero sensitivity.
        // Old value 0.1% blocked virtually all valid signals.
        let ev_threshold = 0.0001 * sensitivity_ratio * time_scale;
        let recommended_action =
            if yes_ev > ev_threshold && yes_ev > no_ev && max_loss_yes_pct > -0.30 {
                ScenarioAction::BuyYes
            } else if no_ev > ev_threshold && no_ev > yes_ev {
                let max_loss_no = scenarios
                    .iter()
                    .map(|s| s.no_pnl_pct)
                    .fold(f64::INFINITY, f64::min);
                if max_loss_no > -0.30 {
                    ScenarioAction::BuyNo
                } else {
                    ScenarioAction::Hold
                }
            } else {
                ScenarioAction::Hold
            };

        ScenarioTree {
            scenarios,
            yes_ev,
            no_ev,
            max_loss_yes_pct,
            recommended_action,
        }
    }

    /// Backwards-compatible builder that estimates volatility from momentum.
    pub fn build_compat(
        yes_price: f64,
        btc_momentum_pct: f64,
        regime: &str,
        time_remaining_mins: f64,
        confidence: f64,
    ) -> Self {
        // Estimate vol from momentum magnitude — higher momentum = higher vol environment
        let estimated_vol = btc_momentum_pct.abs() / 100.0 * 0.5 + 0.001;
        Self::build(
            yes_price,
            btc_momentum_pct,
            regime,
            time_remaining_mins,
            confidence,
            estimated_vol,
        )
    }

    /// Check if any single scenario has a catastrophic outcome (>50% loss).
    pub fn has_catastrophic_risk(&self) -> bool {
        self.scenarios
            .iter()
            .any(|s| s.yes_pnl_pct < -0.50 && s.probability > 0.05)
    }

    /// Get a brief summary string for logging.
    pub fn summary(&self) -> String {
        let vol_range: String = if self.scenarios.len() >= 2 {
            let min_move = self
                .scenarios
                .iter()
                .map(|s| s.btc_move_pct)
                .fold(f64::INFINITY, f64::min);
            let max_move = self
                .scenarios
                .iter()
                .map(|s| s.btc_move_pct)
                .fold(f64::NEG_INFINITY, f64::max);
            format!(" range=[{:.2}%,{:.2}%]", min_move, max_move)
        } else {
            String::new()
        };
        format!(
            "yes_ev={:.1}% no_ev={:.1}% max_loss={:.1}% action={:?}{}",
            self.yes_ev * 100.0,
            self.no_ev * 100.0,
            self.max_loss_yes_pct * 100.0,
            self.recommended_action,
            vol_range,
        )
    }
}

/// Compute regime-specific scenario probabilities.
/// Returns (bull_prob, bear_prob, chop_prob, tail_prob).
fn regime_probabilities(regime: &str, btc_momentum: f64, confidence: f64) -> (f64, f64, f64, f64) {
    let momentum_bias = (btc_momentum / 0.5).clamp(-1.0, 1.0);

    match regime.to_uppercase().as_str() {
        "TREND_UP" | "BREAKOUT" => {
            // Strong trend: momentum persists, thin tails in trend direction
            let bull = (0.40 + momentum_bias * 0.15 + confidence * 0.05).clamp(0.20, 0.55);
            let bear = (0.20 - momentum_bias * 0.10).clamp(0.10, 0.35);
            let tail = 0.10; // Tail events in breakouts
            let chop = (1.0 - bull - bear - 2.0 * tail).clamp(0.05, 0.35);
            (bull, bear, chop, tail)
        }
        "TREND_DOWN" => {
            let bear = (0.40 - momentum_bias * 0.15 + confidence * 0.05).clamp(0.20, 0.55);
            let bull = (0.20 + momentum_bias * 0.10).clamp(0.10, 0.35);
            let tail = 0.10;
            let chop = (1.0 - bull - bear - 2.0 * tail).clamp(0.05, 0.35);
            (bull, bear, chop, tail)
        }
        "CHOP" | "RANGE" => {
            // Mean-reverting: fade extremes, fat tails rare
            let bull = (0.30 - momentum_bias * 0.10).clamp(0.15, 0.40);
            let bear = (0.30 + momentum_bias * 0.10).clamp(0.15, 0.40);
            let tail = 0.05;
            let chop = (1.0 - bull - bear - 2.0 * tail).clamp(0.15, 0.45);
            (bull, bear, chop, tail)
        }
        "SQUEEZE" => {
            // Compressed vol: equal directional probability, high tail risk
            let tail: f64 = 0.15; // Squeeze breakouts produce fat tails
            let bull: f64 = 0.25;
            let bear: f64 = 0.25;
            let chop: f64 = (1.0 - bull - bear - 2.0 * tail).clamp(0.05, 0.30);
            (bull, bear, chop, tail)
        }
        _ => {
            let bull: f64 = (0.30 + momentum_bias * 0.08 + confidence * 0.03).clamp(0.20, 0.45);
            let bear: f64 = (0.30 - momentum_bias * 0.08).clamp(0.15, 0.40);
            let tail: f64 = 0.08;
            let chop: f64 = (1.0 - bull - bear - 2.0 * tail).clamp(0.05, 0.30);
            (bull, bear, chop, tail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_tree_probs_sum_to_one() {
        let tree = ScenarioTree::build(0.55, 0.20, "TREND_UP", 8.0, 0.72, 0.0015);
        let total_prob: f64 = tree.scenarios.iter().map(|s| s.probability).sum();
        assert!(
            (total_prob - 1.0).abs() < 0.01,
            "probabilities should sum to ~1.0, got {total_prob}"
        );
    }

    #[test]
    fn high_vol_produces_wider_scenarios() {
        let low_vol = ScenarioTree::build(0.55, 0.10, "RANGE", 10.0, 0.60, 0.0005);
        let high_vol = ScenarioTree::build(0.55, 0.10, "RANGE", 10.0, 0.60, 0.003);

        let low_range = low_vol
            .scenarios
            .iter()
            .map(|s| s.btc_move_pct.abs())
            .fold(0.0_f64, f64::max);
        let high_range = high_vol
            .scenarios
            .iter()
            .map(|s| s.btc_move_pct.abs())
            .fold(0.0_f64, f64::max);

        assert!(
            high_range > low_range * 2.0,
            "high vol scenarios should be much wider: low={low_range:.3} high={high_range:.3}"
        );
    }

    #[test]
    fn squeeze_has_fat_tails() {
        let tree = ScenarioTree::build(0.50, 0.0, "SQUEEZE", 10.0, 0.50, 0.001);
        let tail_prob: f64 = tree
            .scenarios
            .iter()
            .filter(|s| s.name.starts_with("strong"))
            .map(|s| s.probability)
            .sum();
        assert!(
            tail_prob > 0.15,
            "squeeze should have meaningful tail probability: {tail_prob:.3}"
        );
    }

    #[test]
    fn compat_builder_works() {
        let tree = ScenarioTree::build_compat(0.55, 0.30, "TREND_UP", 6.0, 0.75);
        assert!(!tree.scenarios.is_empty());
        assert!(tree.yes_ev.is_finite());
    }
}
