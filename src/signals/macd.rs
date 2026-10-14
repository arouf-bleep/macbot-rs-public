use super::Candle;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MacdData {
    pub macd: f64,
    pub signal: f64,
    pub hist: f64,
    pub hist_delta: f64,
}

fn ema(values: &[f64], period: usize) -> Vec<f64> {
    if values.is_empty() {
        return vec![];
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(values.len());
    out.push(values[0]);
    for &v in &values[1..] {
        let prev = out.last().copied().unwrap_or(values[0]);
        out.push(v * k + prev * (1.0 - k));
    }
    out
}

/// Port of signals.py:compute_macd() — fast=12, slow=26, signal=9
pub fn compute_macd(candles: &[Candle], fast: usize, slow: usize, sig: usize) -> MacdData {
    if candles.len() < slow + sig {
        return MacdData::default();
    }
    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let fast_ema = ema(&closes, fast);
    let slow_ema = ema(&closes, slow);
    let macd_line: Vec<f64> = fast_ema
        .iter()
        .zip(slow_ema.iter())
        .map(|(f, s)| f - s)
        .collect();
    let sig_line = ema(&macd_line, sig);
    let hist: Vec<f64> = macd_line
        .iter()
        .zip(sig_line.iter())
        .map(|(m, s)| m - s)
        .collect();

    let last_hist = *hist.last().unwrap_or(&0.0);
    let prev_hist = hist
        .get(hist.len().saturating_sub(2))
        .copied()
        .unwrap_or(0.0);

    MacdData {
        macd: *macd_line.last().unwrap_or(&0.0),
        signal: *sig_line.last().unwrap_or(&0.0),
        hist: last_hist,
        hist_delta: last_hist - prev_hist,
    }
}
