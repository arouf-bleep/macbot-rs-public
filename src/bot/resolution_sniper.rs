//! Resolution oracle / last-30-second sniper: trade aggressively in the
//! final seconds before market resolution using the Binance price feed as
//! a strong signal about where the market will resolve.

/// Decision output from the resolution sniper.
#[derive(Debug, Clone)]
pub struct SnipeDecision {
    /// `"BUY_YES"` or `"BUY_NO"`.
    pub direction: &'static str,
    /// Confidence in the snipe [0.0, 0.95].
    pub confidence: f64,
    /// Suggested position size as a fraction of balance.
    pub suggested_amount_pct: f64,
}

/// Trades aggressively in the final seconds before market resolution.
///
/// The Binance BTC price feed gives a strong signal about where the market
/// will resolve. If BTC is clearly above/below the strike and the YES/NO
/// token is mispriced, this is a high-edge opportunity.
#[derive(Debug, Clone)]
pub struct ResolutionSniper {
    /// Activate when this many seconds remain before resolution.
    pub activation_seconds: u64,
    /// Minimum edge (in token price terms) required to trade (e.g. 0.10 = 10 cents).
    pub min_edge: f64,
    /// Maximum fraction of balance to deploy.
    pub max_position_pct: f64,
    /// Weight given to recent BTC momentum in the signal.
    pub btc_momentum_weight: f64,
    /// Minimum BTC-vs-strike separation required before we trust the late signal.
    pub min_btc_edge_pct: f64,
}

impl ResolutionSniper {
    pub fn new() -> Self {
        Self {
            activation_seconds: 30,
            min_edge: 0.10,
            max_position_pct: 0.20,
            btc_momentum_weight: 0.5,
            min_btc_edge_pct: 0.0015,
        }
    }

    /// Check whether we should snipe the resolution.
    ///
    /// Returns `Some(SnipeDecision)` if:
    /// - We are within `activation_seconds` of resolution
    /// - BTC price vs the strike creates enough edge (> `min_edge`)
    ///
    /// The edge is measured as: if BTC is above the strike, the YES token
    /// should be worth ~1.0, so edge = `1.0 - yes_token_price`.
    /// Conversely if BTC is below, edge = `yes_token_price` (NO wins).
    pub fn should_snipe(
        &self,
        seconds_remaining: f64,
        btc_price: f64,
        price_to_beat: f64,
        yes_token_price: f64,
    ) -> Option<SnipeDecision> {
        if !seconds_remaining.is_finite()
            || seconds_remaining <= 0.0
            || self.activation_seconds == 0
            || seconds_remaining > self.activation_seconds as f64
            || !btc_price.is_finite()
            || btc_price <= 0.0
            || !price_to_beat.is_finite()
            || price_to_beat <= 0.0
            || !yes_token_price.is_finite()
            || yes_token_price <= 0.0
            || yes_token_price >= 1.0
        {
            return None;
        }

        // Determine which side should win based on BTC price vs strike
        let btc_edge = (btc_price - price_to_beat) / price_to_beat;
        let abs_btc_edge = btc_edge.abs();
        if abs_btc_edge < self.min_btc_edge_pct {
            return None;
        }

        // If BTC is above strike, YES should resolve to 1.0 → edge = 1 - current price
        // If BTC is below strike, NO should resolve to 1.0 → edge = current YES price
        let token_edge = if btc_edge > 0.0 {
            1.0 - yes_token_price
        } else {
            yes_token_price
        }
        .clamp(0.0, 1.0);

        if token_edge < self.min_edge {
            return None;
        }

        let time_urgency =
            (1.0 - (seconds_remaining / self.activation_seconds as f64)).clamp(0.0, 1.0);
        let btc_conviction = ((abs_btc_edge - self.min_btc_edge_pct) / 0.01).clamp(0.0, 1.0);
        let conviction = (self.btc_momentum_weight * btc_conviction
            + (1.0 - self.btc_momentum_weight) * token_edge)
            .clamp(0.0, 1.0);
        let confidence =
            (0.45 * token_edge + 0.35 * conviction + 0.20 * time_urgency).clamp(0.0, 0.95);
        let size_scalar = (0.50 + 0.50 * time_urgency) * (0.55 + 0.45 * conviction);
        let suggested_amount_pct =
            (token_edge * size_scalar * 1.5).clamp(0.0, self.max_position_pct);

        Some(SnipeDecision {
            direction: if btc_edge > 0.0 { "BUY_YES" } else { "BUY_NO" },
            confidence,
            suggested_amount_pct,
        })
    }
}

impl Default for ResolutionSniper {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_snipe_too_early() {
        let sniper = ResolutionSniper::new();
        let result = sniper.should_snipe(120.0, 105_000.0, 100_000.0, 0.50);
        assert!(result.is_none());
    }

    #[test]
    fn snipe_buy_yes_btc_above() {
        let sniper = ResolutionSniper::new();
        // BTC clearly above strike, YES token still cheap
        let decision = sniper
            .should_snipe(15.0, 105_000.0, 100_000.0, 0.60)
            .expect("should snipe");
        assert_eq!(decision.direction, "BUY_YES");
        assert!(decision.confidence > 0.40);
    }

    #[test]
    fn snipe_buy_no_btc_below() {
        let sniper = ResolutionSniper::new();
        // BTC clearly below strike, YES token still expensive
        let decision = sniper
            .should_snipe(10.0, 95_000.0, 100_000.0, 0.60)
            .expect("should snipe");
        assert_eq!(decision.direction, "BUY_NO");
        assert!(decision.confidence > 0.50);
    }

    #[test]
    fn no_snipe_insufficient_edge() {
        let sniper = ResolutionSniper::new();
        // BTC slightly above strike, YES token already priced in
        let result = sniper.should_snipe(10.0, 100_100.0, 100_000.0, 0.95);
        assert!(result.is_none());
    }

    #[test]
    fn position_pct_capped() {
        let sniper = ResolutionSniper::new();
        // Huge edge → should cap at max_position_pct
        let decision = sniper
            .should_snipe(5.0, 110_000.0, 100_000.0, 0.10)
            .expect("should snipe");
        assert!(decision.suggested_amount_pct <= sniper.max_position_pct);
    }

    #[test]
    fn no_snipe_when_btc_is_too_close_to_strike() {
        let sniper = ResolutionSniper::new();
        let result = sniper.should_snipe(8.0, 100_050.0, 100_000.0, 0.70);
        assert!(result.is_none());
    }

    #[test]
    fn closer_to_resolution_increases_confidence_and_size() {
        let sniper = ResolutionSniper::new();
        let earlier = sniper
            .should_snipe(25.0, 105_000.0, 100_000.0, 0.60)
            .expect("should snipe earlier");
        let later = sniper
            .should_snipe(5.0, 105_000.0, 100_000.0, 0.60)
            .expect("should snipe later");

        assert!(later.confidence > earlier.confidence);
        assert!(later.suggested_amount_pct >= earlier.suggested_amount_pct);
    }
}
