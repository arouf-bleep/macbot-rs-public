use super::Candle;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct VwapData {
    pub vwap: f64,
    pub price_vs_vwap: f64,
    pub slope: f64,
    pub failed_reclaim: bool,
}

/// Port of signals.py:compute_vwap()
pub fn compute_vwap(candles: &[Candle]) -> VwapData {
    if candles.len() < 2 {
        return VwapData::default();
    }

    let mut cum_tp_vol = 0.0_f64;
    let mut cum_vol = 0.0_f64;
    let mut vwap_series = Vec::with_capacity(candles.len());

    for c in candles {
        let tp = (c.high + c.low + c.close) / 3.0;
        cum_tp_vol += tp * c.volume;
        cum_vol += c.volume;
        let v = if cum_vol > 0.0 {
            cum_tp_vol / cum_vol
        } else {
            tp
        };
        vwap_series.push(v);
    }

    let vwap = vwap_series.last().copied().unwrap_or(0.0);
    let price = candles.last().map(|c| c.close).unwrap_or(0.0);
    let price_vs_vwap = if vwap > 0.0 {
        (price - vwap) / vwap
    } else {
        0.0
    };

    // Slope: linear regression on last 8 VWAP values
    let window = &vwap_series[vwap_series.len().saturating_sub(8)..];
    let n = window.len() as f64;
    let x_mean = (n - 1.0) / 2.0;
    let y_mean = window.iter().sum::<f64>() / n;
    let num: f64 = window
        .iter()
        .enumerate()
        .map(|(i, &y)| (i as f64 - x_mean) * (y - y_mean))
        .sum();
    let den: f64 = window
        .iter()
        .enumerate()
        .map(|(i, _)| (i as f64 - x_mean).powi(2))
        .sum();
    let slope = if den > 0.0 { num / den } else { 0.0 };

    // Failed VWAP reclaim: price was above VWAP, dipped below, closed below
    let mut failed_reclaim = false;
    if candles.len() >= 3 {
        let n = candles.len();
        let prev_above = candles[n - 2].close > vwap_series[n - 2];
        let curr_below = price < vwap;
        failed_reclaim = prev_above && curr_below;
    }

    VwapData {
        vwap,
        price_vs_vwap,
        slope,
        failed_reclaim,
    }
}
