#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct DivergenceData {
    pub direction: String,
    pub confidence: f64,
    pub strength: f64,
}

/// Port of signals.py:compute_price_divergence()
pub fn compute_price_divergence(btc_momentum_pct: f64, poly_prob: f64) -> DivergenceData {
    // Extreme fade
    if poly_prob > 0.68 {
        return DivergenceData {
            direction: "DOWN".into(),
            confidence: 0.62,
            strength: poly_prob - 0.68,
        };
    }
    if poly_prob < 0.32 {
        return DivergenceData {
            direction: "UP".into(),
            confidence: 0.62,
            strength: 0.32 - poly_prob,
        };
    }

    // Momentum mispricing: BTC moved but Polymarket near 50%
    if btc_momentum_pct.abs() >= 0.30 && (poly_prob - 0.5).abs() < 0.10 {
        let strength = (btc_momentum_pct.abs() / 0.50).min(1.0);
        let direction = if btc_momentum_pct > 0.0 { "UP" } else { "DOWN" }.to_string();
        let confidence = 0.55 + strength * 0.15;
        return DivergenceData {
            direction,
            confidence,
            strength,
        };
    }

    DivergenceData {
        direction: "NEUTRAL".into(),
        confidence: 0.5,
        strength: 0.0,
    }
}
