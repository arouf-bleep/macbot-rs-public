#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FundingData {
    pub direction: String,
    pub confidence: f64,
    pub rate: Option<f64>,
}

/// Port of signals.py:compute_funding_signal() — contrarian
pub fn compute_funding_signal(
    funding_rate: Option<f64>,
    _open_interest: Option<f64>,
) -> FundingData {
    match funding_rate {
        None => FundingData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            rate: None,
        },
        Some(r) => {
            if r > 0.0003 {
                FundingData {
                    direction: "DOWN".into(),
                    confidence: 0.60,
                    rate: Some(r),
                }
            } else if r < -0.0003 {
                FundingData {
                    direction: "UP".into(),
                    confidence: 0.60,
                    rate: Some(r),
                }
            } else {
                FundingData {
                    direction: "NEUTRAL".into(),
                    confidence: 0.5,
                    rate: Some(r),
                }
            }
        }
    }
}
