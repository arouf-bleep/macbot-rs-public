#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct LunarcrushSentimentData {
    pub direction: String,
    pub confidence: f64,
    pub score: f64,
    pub social_volume: Option<f64>,
}

/// LunarCrush sentiment score [-1, 1]. Positive = bullish.
pub fn compute_lunarcrush_sentiment(
    score: Option<f64>,
    social_volume: Option<f64>,
) -> LunarcrushSentimentData {
    let Some(score) = score else {
        return LunarcrushSentimentData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            score: 0.0,
            social_volume,
        };
    };
    let score = score.clamp(-1.0, 1.0);
    let strength = score.abs();
    if strength < 0.15 {
        return LunarcrushSentimentData {
            direction: "NEUTRAL".into(),
            confidence: 0.5,
            score,
            social_volume,
        };
    }
    let direction = if score >= 0.0 { "UP" } else { "DOWN" };
    let mut conf = 0.52 + strength * 0.16;
    if let Some(vol) = social_volume {
        if vol > 0.0 {
            conf *= 1.0 + (vol.ln() / 15.0).clamp(0.0, 0.12);
        }
    }
    LunarcrushSentimentData {
        direction: direction.into(),
        confidence: conf.min(0.72),
        score,
        social_volume,
    }
}
