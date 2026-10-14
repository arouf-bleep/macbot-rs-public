use super::Candle;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RsiData {
    pub rsi: f64,
    pub slope: f64,
}

/// Port of signals.py:compute_rsi() — SMA-based RSI, period=14
pub fn compute_rsi(candles: &[Candle], period: usize) -> RsiData {
    let period = period.max(2);
    if period >= candles.len() || candles.iter().any(|c| !c.close.is_finite()) {
        return RsiData {
            rsi: 50.0,
            slope: 0.0,
        };
    }

    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let deltas: Vec<f64> = closes.windows(2).map(|w| w[1] - w[0]).collect();

    let mut gains: Vec<f64> = Vec::with_capacity(deltas.len());
    let mut losses: Vec<f64> = Vec::with_capacity(deltas.len());
    for &d in &deltas {
        gains.push(d.max(0.0));
        losses.push((-d).max(0.0));
    }

    let mut rsi_series = Vec::new();
    for i in (period - 1)..gains.len() {
        let avg_gain = gains[i + 1 - period..=i].iter().sum::<f64>() / period as f64;
        let avg_loss = losses[i + 1 - period..=i].iter().sum::<f64>() / period as f64;
        let rsi = if avg_loss > 0.0 {
            100.0 - 100.0 / (1.0 + avg_gain / avg_loss)
        } else if avg_gain > 0.0 {
            100.0
        } else {
            50.0
        };
        rsi_series.push(rsi);
    }

    if rsi_series.is_empty() {
        return RsiData {
            rsi: 50.0,
            slope: 0.0,
        };
    }

    let rsi = rsi_series.last().copied().unwrap_or(50.0);
    let slope = if rsi_series.len() >= 3 {
        rsi_series[rsi_series.len() - 1] - rsi_series[rsi_series.len() - 3]
    } else {
        0.0
    };

    RsiData { rsi, slope }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candles(closes: impl IntoIterator<Item = f64>) -> Vec<Candle> {
        closes
            .into_iter()
            .map(|close| Candle {
                open: close,
                high: close,
                low: close,
                close,
                volume: 1.0,
            })
            .collect()
    }

    #[test]
    fn flat_prices_are_neutral_and_one_way_moves_reach_the_endpoint() {
        assert_eq!(compute_rsi(&candles([100.0; 16]), 14).rsi, 50.0);
        assert_eq!(
            compute_rsi(&candles((100..116).map(f64::from)), 14).rsi,
            100.0
        );
        assert_eq!(
            compute_rsi(&candles((100..116).rev().map(f64::from)), 14).rsi,
            0.0
        );
    }

    #[test]
    fn invalid_input_is_neutral() {
        assert_eq!(compute_rsi(&candles([100.0; 16]), usize::MAX).rsi, 50.0);
        assert_eq!(compute_rsi(&candles([f64::NAN; 16]), 14).rsi, 50.0);
    }
}
