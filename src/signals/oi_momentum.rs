#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OiMomentumData {
    pub direction: String,
    pub confidence: f64,
    pub delta_pct: f64,
}

/// Compute open-interest momentum. Direction is aligned with BTC momentum sign
/// when OI is meaningfully rising/falling; otherwise NEUTRAL.
pub fn compute_oi_momentum(
    open_interest: Option<f64>,
    prev_open_interest: Option<f64>,
    btc_momentum_pct: f64,
) -> OiMomentumData {
    let (Some(oi), Some(prev)) = (open_interest, prev_open_interest) else {
        return OiMomentumData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            delta_pct: 0.0,
        };
    };
    if oi <= 0.0 || prev <= 0.0 {
        return OiMomentumData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            delta_pct: 0.0,
        };
    }

    let delta_pct = (oi - prev) / prev * 100.0;
    let abs_delta = delta_pct.abs();
    if abs_delta < 0.15 || btc_momentum_pct.abs() < 0.05 {
        return OiMomentumData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            delta_pct,
        };
    }

    let direction = if btc_momentum_pct >= 0.0 {
        "UP"
    } else {
        "DOWN"
    };
    let mut conf = 0.52 + (abs_delta / 1.5).clamp(0.0, 0.15);
    if delta_pct < 0.0 {
        conf *= 0.85;
    }

    OiMomentumData {
        direction: direction.into(),
        confidence: conf.min(0.72),
        delta_pct,
    }
}
