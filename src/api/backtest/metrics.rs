//! A rate or per-sample diagnostic requires actual finite observations.
pub(super) fn sample_average(sum: f64, count: usize) -> Option<f64> {
    if count == 0 || !sum.is_finite() {
        return None;
    }
    let average = sum / count as f64;
    average.is_finite().then_some(average)
}

pub(super) fn per_trade_sharpe(pnls: &[f64]) -> Option<f64> {
    if pnls.len() < 2 || pnls.iter().any(|pnl| !pnl.is_finite()) {
        return None;
    }
    let mean = sample_average(pnls.iter().sum(), pnls.len())?;
    let variance =
        pnls.iter().map(|pnl| (pnl - mean).powi(2)).sum::<f64>() / (pnls.len() - 1) as f64;
    let std = variance.sqrt();
    if !std.is_finite() || std <= f64::EPSILON {
        return None;
    }
    let diagnostic = mean / std;
    diagnostic.is_finite().then_some(diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_metrics_require_observations_but_preserve_observed_zero() {
        assert_eq!(sample_average(0.0, 0), None);
        assert_eq!(sample_average(0.0, 5), Some(0.0));
        assert_eq!(sample_average(f64::INFINITY, 2), None);
        assert_eq!(sample_average(9.0, 3), Some(3.0));
    }

    #[test]
    fn sharpe_requires_a_finite_nonconstant_multi_trade_sample() {
        for sample in [
            &[][..],
            &[1.0][..],
            &[1.0, 1.0][..],
            &[0.0, 0.0][..],
            &[1.0, f64::NAN][..],
        ] {
            assert_eq!(per_trade_sharpe(sample), None);
        }
        assert_eq!(per_trade_sharpe(&[-1.0, 1.0]), Some(0.0));
        assert!((per_trade_sharpe(&[1.0, 3.0]).unwrap() - 2.0f64.sqrt()).abs() < 1e-12);
    }
}
