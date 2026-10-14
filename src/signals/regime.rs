#![allow(dead_code, unused_imports)]
use super::Candle;

/// Multi-indicator regime classifier.
///
/// Combines 5 indicators into a voting system:
/// 1. VWAP cross frequency (chop detection)
/// 2. VWAP slope (trend direction)
/// 3. ADX-proxy (trend strength via directional movement)
/// 4. Bollinger Band width (volatility expansion/contraction)
/// 5. Volume profile (confirmation via volume trend)
///
/// Returns: "CHOP" | "TREND_UP" | "TREND_DOWN" | "RANGE" | "BREAKOUT" | "SQUEEZE"
pub fn compute_regime(candles: &[Candle], vwap_slope: f64) -> String {
    if candles.len() < 14 {
        return "CHOP".to_string();
    }

    let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
    let highs: Vec<f64> = candles.iter().map(|c| c.high).collect();
    let lows: Vec<f64> = candles.iter().map(|c| c.low).collect();
    let vols: Vec<f64> = candles.iter().map(|c| c.volume).collect();
    let n = closes.len();

    // ── Indicator 1: VWAP cross frequency (chop detector) ─────────────────
    let mean_price = closes.iter().sum::<f64>() / n as f64;
    let mut crosses = 0u32;
    for i in 1..n {
        if (closes[i - 1] > mean_price) != (closes[i] > mean_price) {
            crosses += 1;
        }
    }
    let cross_rate = crosses as f64 / n as f64;

    // ── Indicator 2: ADX proxy via Average True Range directional movement ──
    // Simplified ADX: measure the consistency of directional moves
    let mut plus_dm_sum = 0.0_f64;
    let mut minus_dm_sum = 0.0_f64;
    let mut atr_sum = 0.0_f64;
    let lookback = n.min(14);

    for i in (n - lookback)..n {
        if i == 0 {
            continue;
        }
        let up_move = highs[i] - highs[i - 1];
        let down_move = lows[i - 1] - lows[i];

        if up_move > down_move && up_move > 0.0 {
            plus_dm_sum += up_move;
        }
        if down_move > up_move && down_move > 0.0 {
            minus_dm_sum += down_move;
        }

        // True range
        let tr = (highs[i] - lows[i])
            .max((highs[i] - closes[i - 1]).abs())
            .max((lows[i] - closes[i - 1]).abs());
        atr_sum += tr;
    }

    let atr = if lookback > 1 {
        atr_sum / (lookback - 1) as f64
    } else {
        1.0
    };

    // Directional index: how one-sided is the movement?
    let di_plus = if atr > 0.0 { plus_dm_sum / atr } else { 0.0 };
    let di_minus = if atr > 0.0 { minus_dm_sum / atr } else { 0.0 };
    let di_total = di_plus + di_minus;
    let dx = if di_total > 0.0 {
        ((di_plus - di_minus).abs() / di_total).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // ── Indicator 3: Bollinger Band width (volatility state) ──────────────
    let bb_period = n.min(20);
    let recent_closes = &closes[n - bb_period..];
    let bb_mean = recent_closes.iter().sum::<f64>() / bb_period as f64;
    let bb_var = recent_closes
        .iter()
        .map(|c| (c - bb_mean).powi(2))
        .sum::<f64>()
        / bb_period as f64;
    let bb_std = bb_var.sqrt();
    let bb_width = if bb_mean > 0.0 {
        (2.0 * bb_std) / bb_mean // Normalized BB width (%)
    } else {
        0.0
    };

    // Historical BB width for comparison (squeeze detection)
    let hist_period = n.min(60);
    let hist_closes = &closes[n.saturating_sub(hist_period)..];
    let hist_mean = hist_closes.iter().sum::<f64>() / hist_closes.len() as f64;
    let hist_var = hist_closes
        .iter()
        .map(|c| (c - hist_mean).powi(2))
        .sum::<f64>()
        / hist_closes.len() as f64;
    let hist_bb_width = if hist_mean > 0.0 {
        (2.0 * hist_var.sqrt()) / hist_mean
    } else {
        bb_width
    };

    let bb_ratio = if hist_bb_width > 0.0 {
        bb_width / hist_bb_width
    } else {
        1.0
    };

    // ── Indicator 4: Volume trend (confirmation) ─────────────────────────
    let vol_half = vols.len() / 2;
    let early_vol: f64 = vols[..vol_half].iter().sum::<f64>() / vol_half.max(1) as f64;
    let later_vol: f64 =
        vols[vol_half..].iter().sum::<f64>() / (vols.len() - vol_half).max(1) as f64;
    let vol_expansion = if early_vol > 0.0 {
        (later_vol - early_vol) / early_vol
    } else {
        0.0
    };

    // ── Indicator 5: Higher-highs / lower-lows consistency ──────────────
    let hh_ll_window = n.min(10);
    let mut higher_highs = 0u32;
    let mut lower_lows = 0u32;
    for i in (n - hh_ll_window + 1)..n {
        if highs[i] > highs[i - 1] {
            higher_highs += 1;
        }
        if lows[i] < lows[i - 1] {
            lower_lows += 1;
        }
    }
    let hh_rate = higher_highs as f64 / (hh_ll_window - 1) as f64;
    let ll_rate = lower_lows as f64 / (hh_ll_window - 1) as f64;

    // ── Regime classification via scoring ────────────────────────────────

    // Squeeze: BB width is < 60% of historical average (compression)
    if bb_ratio < 0.60 && dx < 0.30 {
        return "SQUEEZE".to_string();
    }

    // Breakout: BB expanding rapidly + volume surge + directional
    if bb_ratio > 1.5 && vol_expansion > 0.30 && dx > 0.40 {
        return "BREAKOUT".to_string();
    }

    // Chop: high cross rate + low directional index + no HH/LL pattern
    let chop_score = cross_rate * 0.35
        + (1.0 - dx) * 0.30
        + (1.0 - (hh_rate - ll_rate).abs()) * 0.20
        + (1.0 - vwap_slope.abs() / 0.01).clamp(0.0, 1.0) * 0.15;

    if chop_score > 0.70 {
        return "CHOP".to_string();
    }

    // Trend detection: combine VWAP slope, DX, and HH/LL
    let trend_up_score = (vwap_slope / 0.005).clamp(0.0, 1.0) * 0.35
        + dx * 0.25
        + hh_rate * 0.25
        + (di_plus / di_total.max(0.01)).clamp(0.0, 1.0) * 0.15;

    let trend_down_score = (-vwap_slope / 0.005).clamp(0.0, 1.0) * 0.35
        + dx * 0.25
        + ll_rate * 0.25
        + (di_minus / di_total.max(0.01)).clamp(0.0, 1.0) * 0.15;

    if trend_up_score > 0.55 && trend_up_score > trend_down_score {
        return "TREND_UP".to_string();
    }

    if trend_down_score > 0.55 && trend_down_score > trend_up_score {
        return "TREND_DOWN".to_string();
    }

    "RANGE".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_candles(prices: &[f64], volumes: &[f64]) -> Vec<Candle> {
        prices
            .iter()
            .zip(volumes.iter())
            .map(|(&p, &v)| Candle {
                open: p - 10.0,
                high: p + 20.0,
                low: p - 20.0,
                close: p,
                volume: v,
            })
            .collect()
    }

    #[test]
    fn detects_trend_up() {
        // Steadily increasing prices
        let prices: Vec<f64> = (0..30).map(|i| 50000.0 + i as f64 * 50.0).collect();
        let volumes = vec![100.0; 30];
        let candles = make_candles(&prices, &volumes);
        let regime = compute_regime(&candles, 0.01);
        assert!(
            regime == "TREND_UP" || regime == "BREAKOUT",
            "Expected TREND_UP or BREAKOUT, got {regime}"
        );
    }

    #[test]
    fn detects_chop() {
        // Oscillating prices
        let prices: Vec<f64> = (0..30)
            .map(|i| 50000.0 + if i % 2 == 0 { 50.0 } else { -50.0 })
            .collect();
        let volumes = vec![100.0; 30];
        let candles = make_candles(&prices, &volumes);
        let regime = compute_regime(&candles, 0.0001);
        assert!(
            regime == "CHOP" || regime == "RANGE",
            "Expected CHOP or RANGE, got {regime}"
        );
    }

    #[test]
    fn too_few_candles() {
        let candles = make_candles(&[50000.0; 5], &[100.0; 5]);
        assert_eq!(compute_regime(&candles, 0.0), "CHOP");
    }
}
