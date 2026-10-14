//! Kyle's Lambda estimation for price impact measurement.
//!
//! Kyle's Lambda is the regression coefficient of price changes on signed
//! order flow: `price_change = lambda * signed_volume + epsilon`.
//!
//! High lambda indicates informed traders are active (price moves a lot
//! per unit of net order flow), signaling caution. Low lambda indicates
//! normal liquidity conditions.

use std::collections::VecDeque;

/// Rolling window estimator for Kyle's Lambda via OLS regression.
#[derive(Debug, Clone)]
pub struct KyleLambdaTracker {
    /// Rolling window of (signed_volume, price_change) observations.
    observations: VecDeque<(f64, f64)>,
    window_size: usize,
    lambda: Option<f64>,
    r_squared: Option<f64>,
}

impl KyleLambdaTracker {
    pub fn new(window_size: usize) -> Self {
        Self {
            observations: VecDeque::with_capacity(window_size + 10),
            window_size,
            lambda: None,
            r_squared: None,
        }
    }

    /// Push a new (signed_volume, price_change) observation and trim to window.
    pub fn update(&mut self, signed_volume: f64, price_change: f64) {
        self.observations.push_back((signed_volume, price_change));
        while self.observations.len() > self.window_size {
            self.observations.pop_front();
        }
    }

    /// Estimate Kyle's Lambda via OLS: `price_change = lambda * signed_volume + epsilon`.
    ///
    /// Returns `Some((lambda, r_squared))` if at least 30 observations are available,
    /// otherwise `None`.
    ///
    /// OLS formulas:
    /// - `lambda = Cov(x, y) / Var(x)`
    /// - `r_squared = Cor(x, y)^2`
    pub fn estimate(&mut self) -> Option<(f64, f64)> {
        let n = self.observations.len();
        if n < 30 {
            self.lambda = None;
            self.r_squared = None;
            return None;
        }

        let n_f = n as f64;

        // Compute means
        let (sum_x, sum_y) = self
            .observations
            .iter()
            .fold((0.0, 0.0), |(sx, sy), &(x, y)| (sx + x, sy + y));
        let mean_x = sum_x / n_f;
        let mean_y = sum_y / n_f;

        // Compute Cov(x,y), Var(x), Var(y)
        let (mut cov_xy, mut var_x, mut var_y) = (0.0_f64, 0.0_f64, 0.0_f64);
        for &(x, y) in &self.observations {
            let dx = x - mean_x;
            let dy = y - mean_y;
            cov_xy += dx * dy;
            var_x += dx * dx;
            var_y += dy * dy;
        }
        cov_xy /= n_f;
        var_x /= n_f;
        var_y /= n_f;

        // Guard against degenerate data (zero variance in x)
        if var_x < 1e-20 {
            self.lambda = None;
            self.r_squared = None;
            return None;
        }

        let lambda = cov_xy / var_x;

        // r_squared = Cor(x,y)^2 = Cov(x,y)^2 / (Var(x) * Var(y))
        let r_squared = if var_y > 1e-20 {
            (cov_xy * cov_xy) / (var_x * var_y)
        } else {
            0.0
        };

        self.lambda = Some(lambda);
        self.r_squared = Some(r_squared.clamp(0.0, 1.0));

        Some((lambda, r_squared.clamp(0.0, 1.0)))
    }

    /// Current lambda estimate (if available).
    #[cfg(test)]
    pub fn lambda(&self) -> Option<f64> {
        self.lambda
    }

    /// Current R-squared estimate (if available).
    #[cfg(test)]
    pub fn r_squared(&self) -> Option<f64> {
        self.r_squared
    }
}

/// Compute a fusion-compatible signal from Kyle's Lambda.
///
/// Returns `(signal, confidence)` where:
/// - `signal` in `[-1, 1]`: high lambda (> 0.002) = informed traders active = caution (negative),
///   low lambda = normal liquidity = neutral (near 0).
/// - `confidence` in `[0, 1]`: scales with R-squared (high R² = reliable estimate).
pub fn compute_kyle_signal(lambda: f64, r_squared: f64) -> (f64, f64) {
    // Threshold-based signal:
    // lambda > 0.002 → informed flow, signal caution (negative)
    // lambda in [0.0005, 0.002] → mild caution
    // lambda < 0.0005 → normal liquidity, neutral
    //
    // We use absolute lambda since negative lambda (price moves opposite to flow)
    // is also noteworthy.
    let abs_lambda = lambda.abs();

    let signal = if abs_lambda > 0.005 {
        -1.0 // very high impact → strong caution
    } else if abs_lambda > 0.002 {
        // Scale linearly from -0.4 to -1.0 over [0.002, 0.005]
        -0.4 - 0.6 * ((abs_lambda - 0.002) / 0.003).clamp(0.0, 1.0)
    } else if abs_lambda > 0.0005 {
        // Scale linearly from 0.0 to -0.4 over [0.0005, 0.002]
        -0.4 * ((abs_lambda - 0.0005) / 0.0015).clamp(0.0, 1.0)
    } else {
        0.0 // normal liquidity, neutral
    };

    // Confidence scales with R² — if the regression fits well, we trust the lambda.
    // R² < 0.05 → very low confidence; R² > 0.3 → high confidence
    let confidence = (r_squared / 0.3).clamp(0.0, 1.0);

    (signal.clamp(-1.0, 1.0), confidence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insufficient_observations() {
        let mut tracker = KyleLambdaTracker::new(200);
        for i in 0..29 {
            tracker.update(i as f64, i as f64 * 0.001);
        }
        assert!(tracker.estimate().is_none());
    }

    #[test]
    fn test_synthetic_linear_relationship() {
        // signed_volume clearly predicts price_change: price_change = 0.003 * signed_volume
        let mut tracker = KyleLambdaTracker::new(200);
        let true_lambda = 0.003;
        for i in 0..100 {
            let signed_vol = (i as f64 - 50.0) * 10.0; // range: -500 to 490
            let price_change = true_lambda * signed_vol;
            tracker.update(signed_vol, price_change);
        }
        let (lambda, r_sq) = tracker.estimate().expect("should have enough data");
        assert!(
            (lambda - true_lambda).abs() < 1e-8,
            "lambda should be ~{true_lambda}, got {lambda}"
        );
        assert!(
            r_sq > 0.99,
            "R² should be ~1.0 for perfect linear data, got {r_sq}"
        );
        assert_eq!(tracker.lambda(), Some(lambda));
        assert_eq!(tracker.r_squared(), Some(r_sq));
    }

    #[test]
    fn test_noisy_data_low_r_squared() {
        // Random-ish data with no real relationship → low R²
        let mut tracker = KyleLambdaTracker::new(200);
        let mut x = 0.7_f64;
        for _ in 0..100 {
            // Pseudo-random via LCG
            x = (x * 17.0 + 0.3) % 3.0 - 1.5;
            let signed_vol = x * 100.0;
            // Price change is unrelated noise
            let y = ((x * 31.0 + 0.7) % 2.0 - 1.0) * 0.01;
            tracker.update(signed_vol, y);
        }
        let (_, r_sq) = tracker.estimate().expect("should have enough data");
        // With pseudo-random data, R² should be low (though not guaranteed to be 0
        // due to accidental correlations in a simple LCG)
        assert!(
            r_sq < 0.5,
            "R² should be low for unrelated data, got {r_sq}"
        );
    }

    #[test]
    fn test_window_trimming() {
        let mut tracker = KyleLambdaTracker::new(50);
        for i in 0..100 {
            tracker.update(i as f64, i as f64 * 0.001);
        }
        assert_eq!(tracker.observations.len(), 50);
    }

    #[test]
    fn test_compute_kyle_signal_high_lambda() {
        // Very high lambda → strong caution signal
        let (signal, _) = compute_kyle_signal(0.01, 0.5);
        assert!(
            signal < -0.9,
            "high lambda should produce strong negative signal, got {signal}"
        );
    }

    #[test]
    fn test_compute_kyle_signal_low_lambda() {
        // Very low lambda → neutral
        let (signal, _) = compute_kyle_signal(0.0001, 0.5);
        assert!(
            signal.abs() < 0.01,
            "low lambda should produce neutral signal, got {signal}"
        );
    }

    #[test]
    fn test_compute_kyle_signal_confidence_scales_with_r_squared() {
        let (_, conf_low) = compute_kyle_signal(0.003, 0.02);
        let (_, conf_high) = compute_kyle_signal(0.003, 0.5);
        assert!(
            conf_high > conf_low,
            "higher R² should give higher confidence: low={conf_low}, high={conf_high}"
        );
    }

    #[test]
    fn test_zero_variance_returns_none() {
        let mut tracker = KyleLambdaTracker::new(200);
        // All same signed_volume → zero variance in x
        for _ in 0..50 {
            tracker.update(1.0, 0.001);
        }
        assert!(tracker.estimate().is_none());
    }
}
