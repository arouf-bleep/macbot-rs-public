#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct FearGreedData {
    pub direction: String,
    pub confidence: f64,
    pub value: u32,
}

/// Port of signals.py:compute_fear_greed() — contrarian macro signal
pub fn compute_fear_greed(value: u32) -> FearGreedData {
    if value < 25 {
        // Extreme fear → contrarian UP
        let conf = 0.55 + (25 - value) as f64 * 0.004;
        FearGreedData {
            direction: "UP".into(),
            confidence: conf.min(0.65),
            value,
        }
    } else if value > 75 {
        // Extreme greed → contrarian DOWN
        let conf = 0.55 + (value - 75) as f64 * 0.004;
        FearGreedData {
            direction: "DOWN".into(),
            confidence: conf.min(0.65),
            value,
        }
    } else {
        FearGreedData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            value,
        }
    }
}
