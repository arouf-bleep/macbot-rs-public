use super::Candle;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HaData {
    pub direction: String, // "UP" | "DOWN" | "NEUTRAL"
    pub consecutive: i32,
}

/// Port of signals.py:compute_ha()
pub fn compute_ha(candles: &[Candle]) -> HaData {
    if candles.len() < 3 {
        return HaData {
            direction: "NEUTRAL".into(),
            consecutive: 0,
        };
    }

    // Build HA candles
    let mut ha_opens = vec![0.0_f64; candles.len()];
    let mut ha_closes = vec![0.0_f64; candles.len()];

    ha_closes[0] = (candles[0].open + candles[0].high + candles[0].low + candles[0].close) / 4.0;
    ha_opens[0] = (candles[0].open + candles[0].close) / 2.0;

    for i in 1..candles.len() {
        ha_closes[i] =
            (candles[i].open + candles[i].high + candles[i].low + candles[i].close) / 4.0;
        ha_opens[i] = (ha_opens[i - 1] + ha_closes[i - 1]) / 2.0;
    }

    // Count consecutive bullish/bearish candles from the end
    let mut consecutive = 0_i32;
    let last_bull = ha_closes.last().unwrap_or(&0.0) > ha_opens.last().unwrap_or(&0.0);

    for i in (0..candles.len()).rev() {
        let bull = ha_closes[i] > ha_opens[i];
        if bull == last_bull {
            consecutive += 1;
        } else {
            break;
        }
    }

    let direction = if last_bull { "UP" } else { "DOWN" }.to_string();
    let consecutive = if last_bull { consecutive } else { -consecutive };

    HaData {
        direction,
        consecutive,
    }
}
