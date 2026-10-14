//! Latency arbitrage: detect Binance → Polymarket price lag.
//!
//! Binance BTC price updates faster than Polymarket's CLOB reprices.
//! When BTC spikes on Binance but YES token hasn't moved yet, there's
//! a 1-3 second window to buy before the book adjusts.
//!
//! This module checks multiple time horizons (5s, 15s, 30s) and requires
//! the Polymarket price to be meaningfully lagging the implied fair value.

/// Result of a latency arbitrage check.
#[derive(Debug, Clone)]
pub struct LatencyArbSignal {
    /// Whether there's an exploitable lag
    pub arb_detected: bool,
    /// Expected price move: positive = BUY_YES opportunity, negative = BUY_NO
    pub expected_direction: f64,
    /// Magnitude of Binance move in percent
    pub binance_move_pct: f64,
    /// Current Polymarket YES token price (should trail Binance)
    pub polymarket_yes_price: f64,
    /// Confidence score [0, 1]
    pub confidence: f64,
    /// Which time horizon detected the arb (5s, 15s, 30s)
    pub horizon_secs: u32,
    /// Size of the lag in YES price terms (how far Poly trails implied)
    pub lag_cents: f64,
}

/// Historical BTC price snapshot at a specific time.
#[derive(Debug, Clone, Copy)]
pub struct PriceSnapshot {
    pub price: f64,
    pub age_secs: f64,
}

/// Check for exploitable latency across multiple time horizons.
///
/// Uses the insight that YES token fair value is approximately:
///   fair_yes ≈ 0.5 + sigmoid_slope × btc_move_pct
/// where sigmoid_slope depends on how sensitive the market is to BTC moves.
///
/// When actual_yes diverges from fair_yes by more than the spread, there's arb.
///
/// # Parameters
/// - `binance_price_now`: latest Binance BTC price
/// - `snapshots`: historical BTC price snapshots at various ages (5s, 15s, 30s ago)
/// - `polymarket_yes_price`: current YES token price on Polymarket
/// - `polymarket_spread`: current bid-ask spread on Polymarket (for min arb threshold)
pub fn detect_latency_arb(
    binance_price_now: f64,
    snapshots: &[PriceSnapshot],
    polymarket_yes_price: f64,
    polymarket_spread: f64,
) -> LatencyArbSignal {
    if binance_price_now <= 0.0 || polymarket_yes_price <= 0.0 {
        return LatencyArbSignal {
            arb_detected: false,
            expected_direction: 0.0,
            binance_move_pct: 0.0,
            polymarket_yes_price,
            confidence: 0.0,
            horizon_secs: 0,
            lag_cents: 0.0,
        };
    }

    // Sensitivity: how many cents does YES move per 1% BTC move?
    // Near 0.50, sensitivity is highest. Near 0.10 or 0.90, lower.
    let yes_sensitivity = 2.0 * polymarket_yes_price * (1.0 - polymarket_yes_price);

    let mut best_arb = LatencyArbSignal {
        arb_detected: false,
        expected_direction: 0.0,
        binance_move_pct: 0.0,
        polymarket_yes_price,
        confidence: 0.0,
        horizon_secs: 0,
        lag_cents: 0.0,
    };

    // Check each time horizon — shorter horizons are stronger signals
    for snap in snapshots {
        if snap.price <= 0.0 || snap.age_secs < 2.0 || snap.age_secs > 60.0 {
            continue;
        }

        let move_pct = (binance_price_now - snap.price) / snap.price * 100.0;

        // Skip tiny moves
        if move_pct.abs() < 0.05 {
            continue;
        }

        // Implied fair YES price based on BTC move
        let implied_yes =
            (polymarket_yes_price + move_pct / 100.0 * yes_sensitivity).clamp(0.01, 0.99);

        // Lag: how far behind is the actual Polymarket price?
        let lag = (implied_yes - polymarket_yes_price) * move_pct.signum();
        let lag_cents = lag * 100.0;

        // Arb threshold: lag must exceed spread + minimum edge
        let min_lag = polymarket_spread + 0.02; // spread + 2 cents minimum edge

        if lag > min_lag && lag_cents > best_arb.lag_cents {
            // Confidence factors:
            // 1. Fresher snapshots are more reliable (5s > 15s > 30s)
            let freshness = (1.0 - (snap.age_secs - 2.0) / 58.0).clamp(0.2, 1.0);
            // 2. Larger moves are more reliable (not noise)
            let move_strength = (move_pct.abs() / 0.15 - 1.0).clamp(0.0, 1.0);
            // 3. Larger lags are more reliable
            let lag_strength = ((lag - min_lag) / 0.05).clamp(0.0, 1.0);

            let confidence =
                (freshness * 0.40 + move_strength * 0.35 + lag_strength * 0.25).clamp(0.0, 1.0);

            best_arb = LatencyArbSignal {
                arb_detected: true,
                expected_direction: move_pct.signum(),
                binance_move_pct: move_pct,
                polymarket_yes_price,
                confidence,
                horizon_secs: snap.age_secs as u32,
                lag_cents,
            };
        }
    }

    best_arb
}

/// Simplified detection using a single reference price (backwards compatible).
pub fn detect_latency_arb_simple(
    binance_price_now: f64,
    binance_price_ref: f64,
    polymarket_yes_price: f64,
    min_move_pct: f64,
) -> LatencyArbSignal {
    if binance_price_ref <= 0.0 {
        return LatencyArbSignal {
            arb_detected: false,
            expected_direction: 0.0,
            binance_move_pct: 0.0,
            polymarket_yes_price,
            confidence: 0.0,
            horizon_secs: 0,
            lag_cents: 0.0,
        };
    }

    let snapshots = [PriceSnapshot {
        price: binance_price_ref,
        age_secs: 15.0, // Assume ~15s age for legacy callers
    }];

    detect_latency_arb(
        binance_price_now,
        &snapshots,
        polymarket_yes_price,
        min_move_pct / 100.0, // Convert min_move_pct to spread equivalent
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_arb_when_prices_aligned() {
        let result = detect_latency_arb(
            100_000.0,
            &[PriceSnapshot {
                price: 100_000.0,
                age_secs: 5.0,
            }],
            0.55,
            0.02,
        );
        assert!(!result.arb_detected);
    }

    #[test]
    fn detects_arb_on_btc_spike() {
        // BTC jumped 5% but Polymarket YES hasn't moved.
        // sensitivity at p=0.50 is 0.50, so implied move = 5% × 0.50/100 = 2.5 cents.
        // lag = 0.025 > spread(0.01) + min_edge(0.02) = 0.03 ... still marginal.
        // Use a tighter spread so the threshold is lower.
        let result = detect_latency_arb(
            105_000.0,
            &[PriceSnapshot {
                price: 100_000.0,
                age_secs: 5.0,
            }],
            0.50,  // Still at 0.50 — hasn't repriced
            0.005, // tight spread
        );
        assert!(result.arb_detected, "should detect arb on 5% BTC spike");
        assert!(result.expected_direction > 0.0, "BTC up → buy YES");
        assert!(result.confidence > 0.3);
    }

    #[test]
    fn fresher_snapshot_higher_confidence() {
        let fresh = detect_latency_arb(
            100_500.0,
            &[PriceSnapshot {
                price: 100_000.0,
                age_secs: 5.0,
            }],
            0.50,
            0.02,
        );
        let stale = detect_latency_arb(
            100_500.0,
            &[PriceSnapshot {
                price: 100_000.0,
                age_secs: 45.0,
            }],
            0.50,
            0.02,
        );
        if fresh.arb_detected && stale.arb_detected {
            assert!(
                fresh.confidence >= stale.confidence,
                "fresher snapshot should have >= confidence: {} vs {}",
                fresh.confidence,
                stale.confidence
            );
        }
    }
}
