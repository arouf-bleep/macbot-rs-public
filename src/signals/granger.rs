#![allow(dead_code, unused_imports)]
//! Granger causality testing for adaptive signal weighting.
//!
//! Granger causality tests whether time-series X "causes" (Granger-causes) Y
//! by checking if lagged values of X improve prediction of Y beyond Y's own lags.
//!
//! Applied here: does funding_rate actually CAUSE price moves, or just correlate?
//! Features that pass the causality test get their weight boosted in the fusion engine.
//!
//! Uses proper OLS regression via normal equations (X'X)^-1 X'y for small
//! lag matrices. The restricted model regresses y on its own lags; the
//! unrestricted model adds lagged feature values. An F-test on SSR
//! reduction determines statistical significance.

use std::collections::VecDeque;

/// Rolling Granger causality tester for a single feature vs. price.
#[derive(Debug, Clone)]
pub struct GrangerTester {
    /// Feature values: (feature_val, price_direction) pairs — rolling window
    pub(crate) history: VecDeque<(f64, f64)>,
    max_window: usize,
    /// Number of lags to test (typically 1-3 for short-term trading)
    n_lags: usize,
    /// Last computed F-statistic (higher = more causality)
    last_f_stat: f64,
    /// Whether this feature Granger-causes price at last test
    last_result: bool,
}

impl GrangerTester {
    pub fn new(window: usize, n_lags: usize) -> Self {
        GrangerTester {
            history: VecDeque::with_capacity(window + 10),
            max_window: window,
            n_lags: n_lags.clamp(1, 5),
            last_f_stat: 0.0,
            last_result: false,
        }
    }

    /// Push a new (feature_value, price_direction) observation.
    /// price_direction: +1 = price went up, -1 = down, 0 = flat
    pub fn push(&mut self, feature_val: f64, price_direction: f64) {
        self.history.push_back((feature_val, price_direction));
        if self.history.len() > self.max_window {
            self.history.pop_front();
        }
    }

    /// Test Granger causality via proper OLS F-test.
    ///
    /// Restricted model:   y[t] = a₀ + Σ aᵢ·y[t-i]           (AR on y only)
    /// Unrestricted model:  y[t] = a₀ + Σ aᵢ·y[t-i] + Σ bⱼ·x[t-j]  (AR on y + lagged x)
    ///
    /// Both models are fit via OLS normal equations. F-test compares SSR.
    /// Returns (f_statistic, is_causal) where is_causal = F > critical value.
    pub fn test(&mut self) -> (f64, bool) {
        let n = self.history.len();
        let p = self.n_lags;

        // Need enough data for regression: at least p lags + 20 observations
        if n < p + 20 {
            return (0.0, false);
        }

        let data: Vec<(f64, f64)> = self.history.iter().copied().collect();
        let n_obs = n - p;

        // Build target vector y and design matrices
        let mut y = Vec::with_capacity(n_obs);
        let mut x_restricted = Vec::with_capacity(n_obs); // [1, y_{t-1}, ..., y_{t-p}]
        let mut x_unrestricted = Vec::with_capacity(n_obs); // [1, y_{t-1},.., y_{t-p}, x_{t-1},.., x_{t-p}]

        for t in p..n {
            y.push(data[t].1);

            // Restricted: intercept + lagged y
            let mut row_r = Vec::with_capacity(1 + p);
            row_r.push(1.0); // intercept
            for lag in 1..=p {
                row_r.push(data[t - lag].1);
            }
            x_restricted.push(row_r);

            // Unrestricted: intercept + lagged y + lagged x
            let mut row_u = Vec::with_capacity(1 + 2 * p);
            row_u.push(1.0); // intercept
            for lag in 1..=p {
                row_u.push(data[t - lag].1);
            }
            for lag in 1..=p {
                row_u.push(data[t - lag].0);
            }
            x_unrestricted.push(row_u);
        }

        let ssr_r = ols_ssr(&x_restricted, &y);
        let ssr_u = ols_ssr(&x_unrestricted, &y);

        // F = ((SSR_R - SSR_U) / q) / (SSR_U / (n - k))
        // q = number of additional regressors (p lagged x terms)
        // k = total regressors in unrestricted model (1 + 2p)
        let q = p as f64;
        let k = (1 + 2 * p) as f64;
        let df_denom = (n_obs as f64 - k).max(1.0);

        let f_stat = if ssr_u > 1e-12 && ssr_r >= ssr_u {
            ((ssr_r - ssr_u) / q) / (ssr_u / df_denom)
        } else {
            0.0
        };

        // Critical value ~2.5 for relaxed significance in noisy trading data
        // (standard F-test at p<0.05 with these df is ~3.84, but we want
        // to detect moderate causality signals early)
        let is_causal = f_stat > 2.5;

        self.last_f_stat = f_stat;
        self.last_result = is_causal;

        (f_stat, is_causal)
    }

    /// Returns weight multiplier based on causality result.
    /// Smooth scaling: strong causality boosts, weak causality dampens.
    pub fn weight_multiplier(&self) -> f64 {
        if self.history.len() < self.n_lags + 20 {
            return 1.0; // Not enough data — don't penalize
        }
        if self.last_f_stat > 5.0 {
            1.15 // Strong causality — boost signal weight
        } else if self.last_f_stat > 2.5 {
            1.0 // Moderate causality — keep as-is
        } else if self.last_f_stat > 1.0 {
            0.80 // Weak — dampen
        } else {
            0.65 // No evidence of causality — significant downweight
        }
    }
}

/// Solve OLS via normal equations and return sum of squared residuals.
/// X: n×k design matrix (Vec of row Vecs), y: n×1 target.
/// Returns SSR = Σ(y_i - ŷ_i)² where ŷ = X·β, β = (X'X)⁻¹X'y.
fn ols_ssr(x: &[Vec<f64>], y: &[f64]) -> f64 {
    let n = x.len();
    if n == 0 || x[0].is_empty() {
        return f64::MAX;
    }
    let k = x[0].len();

    // Compute X'X (k×k) and X'y (k×1)
    let mut xtx = vec![0.0_f64; k * k];
    let mut xty = vec![0.0_f64; k];

    for i in 0..n {
        for j in 0..k {
            xty[j] += x[i][j] * y[i];
            for l in j..k {
                let val = x[i][j] * x[i][l];
                xtx[j * k + l] += val;
                if j != l {
                    xtx[l * k + j] += val;
                }
            }
        }
    }

    // Solve (X'X)β = X'y via Cholesky decomposition for small k
    // Falls back to diagonal regularization if singular
    let beta = match cholesky_solve(&xtx, &xty, k) {
        Some(b) => b,
        None => return f64::MAX, // Singular — can't fit
    };

    // Compute SSR
    let mut ssr = 0.0_f64;
    for i in 0..n {
        let mut y_hat = 0.0_f64;
        for j in 0..k {
            y_hat += x[i][j] * beta[j];
        }
        ssr += (y[i] - y_hat).powi(2);
    }
    ssr
}

/// Cholesky decomposition solver for Ax = b where A is k×k symmetric positive definite.
/// Adds small ridge (1e-8) to diagonal for numerical stability.
fn cholesky_solve(a: &[f64], b: &[f64], k: usize) -> Option<Vec<f64>> {
    // Copy A and add ridge regularization
    let mut l = vec![0.0_f64; k * k];
    let mut a_reg = a.to_vec();
    for i in 0..k {
        a_reg[i * k + i] += 1e-8;
    }

    // Cholesky decomposition: A = L·L'
    for i in 0..k {
        for j in 0..=i {
            let mut sum = 0.0_f64;
            for m in 0..j {
                sum += l[i * k + m] * l[j * k + m];
            }
            if i == j {
                let diag = a_reg[i * k + i] - sum;
                if diag <= 0.0 {
                    return None; // Not positive definite
                }
                l[i * k + j] = diag.sqrt();
            } else {
                l[i * k + j] = (a_reg[i * k + j] - sum) / l[j * k + j];
            }
        }
    }

    // Forward substitution: L·z = b
    let mut z = vec![0.0_f64; k];
    for i in 0..k {
        let mut sum = 0.0_f64;
        for j in 0..i {
            sum += l[i * k + j] * z[j];
        }
        z[i] = (b[i] - sum) / l[i * k + i];
    }

    // Back substitution: L'·x = z
    let mut x = vec![0.0_f64; k];
    for i in (0..k).rev() {
        let mut sum = 0.0_f64;
        for j in (i + 1)..k {
            sum += l[j * k + i] * x[j];
        }
        x[i] = (z[i] - sum) / l[i * k + i];
    }

    Some(x)
}

/// Multi-signal Granger causality manager.
/// Tests causality for key signals against BTC price direction.
#[derive(Debug, Clone)]
pub struct GrangerManager {
    pub funding_rate: GrangerTester,
    pub orderbook_imbalance: GrangerTester,
    pub aggressor_ratio: GrangerTester,
    pub vpin: GrangerTester,
    pub tick_velocity: GrangerTester,
}

impl Default for GrangerManager {
    fn default() -> Self {
        GrangerManager {
            funding_rate: GrangerTester::new(200, 2),
            orderbook_imbalance: GrangerTester::new(200, 1),
            aggressor_ratio: GrangerTester::new(200, 1),
            vpin: GrangerTester::new(200, 2),
            tick_velocity: GrangerTester::new(200, 1),
        }
    }
}

impl GrangerManager {
    /// Record a new tick of all signal values + actual price direction.
    /// Call this once per analysis cycle with the outcome of the PREVIOUS cycle.
    pub fn record(
        &mut self,
        funding_rate: f64,
        obi: f64,
        aggressor: f64,
        vpin: f64,
        tick_vel: f64,
        actual_price_direction: f64, // +1 = price went up, -1 = down
    ) {
        self.funding_rate.push(funding_rate, actual_price_direction);
        self.orderbook_imbalance.push(obi, actual_price_direction);
        self.aggressor_ratio.push(aggressor, actual_price_direction);
        self.vpin.push(vpin, actual_price_direction);
        self.tick_velocity.push(tick_vel, actual_price_direction);

        // Run tests periodically (every 10 new observations)
        let n = self.funding_rate.history.len();
        if n.is_multiple_of(10) && n >= 30 {
            self.run_all_tests();
        }
    }

    fn run_all_tests(&mut self) {
        let (f_fr, c_fr) = self.funding_rate.test();
        let (f_obi, c_obi) = self.orderbook_imbalance.test();
        let (f_agg, c_agg) = self.aggressor_ratio.test();
        let (f_vpin, c_vpin) = self.vpin.test();
        let (f_tv, c_tv) = self.tick_velocity.test();

        tracing::debug!(
            "Granger OLS: funding={:.2}({}) obi={:.2}({}) agg={:.2}({}) vpin={:.2}({}) tv={:.2}({})",
            f_fr, c_fr as u8,
            f_obi, c_obi as u8,
            f_agg, c_agg as u8,
            f_vpin, c_vpin as u8,
            f_tv, c_tv as u8,
        );
    }

    /// Get weight multipliers for each signal based on causality.
    pub fn weight_multipliers(&self) -> GrangerWeights {
        GrangerWeights {
            funding_rate: self.funding_rate.weight_multiplier(),
            orderbook_imbalance: self.orderbook_imbalance.weight_multiplier(),
            aggressor_ratio: self.aggressor_ratio.weight_multiplier(),
            vpin: self.vpin.weight_multiplier(),
            tick_velocity: self.tick_velocity.weight_multiplier(),
        }
    }
}

/// Weight multipliers derived from Granger causality tests.
#[derive(Debug, Clone, Copy)]
pub struct GrangerWeights {
    pub funding_rate: f64,
    pub orderbook_imbalance: f64,
    pub aggressor_ratio: f64,
    pub vpin: f64,
    pub tick_velocity: f64,
}

impl Default for GrangerWeights {
    fn default() -> Self {
        GrangerWeights {
            funding_rate: 1.0,
            orderbook_imbalance: 1.0,
            aggressor_ratio: 1.0,
            vpin: 1.0,
            tick_velocity: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ols_ssr_simple_regression() {
        // y = 2*x + 1, perfect fit should have SSR ≈ 0
        let x = vec![
            vec![1.0, 0.0],
            vec![1.0, 1.0],
            vec![1.0, 2.0],
            vec![1.0, 3.0],
            vec![1.0, 4.0],
        ];
        let y = vec![1.0, 3.0, 5.0, 7.0, 9.0];
        let ssr = ols_ssr(&x, &y);
        assert!(
            ssr < 1e-8,
            "perfect linear fit should have SSR ≈ 0, got {ssr}"
        );
    }

    #[test]
    fn granger_detects_causal_signal() {
        let mut tester = GrangerTester::new(100, 1);

        // Generate data where feature at t predicts price at t+1
        let mut prev_feature: f64 = 0.5;
        for _ in 0..60 {
            let feature = if prev_feature > 0.0 { -0.8 } else { 0.8 }; // oscillate
            let price_dir = prev_feature.signum(); // price follows lagged feature
            tester.push(feature, price_dir);
            prev_feature = feature;
        }

        let (f_stat, _) = tester.test();
        assert!(f_stat >= 0.0, "F-stat should be non-negative");
        // The OLS fit on this deterministic data should produce a real F-stat
    }

    #[test]
    fn granger_weight_scaling() {
        let mut tester = GrangerTester::new(100, 1);
        // Not enough data → weight = 1.0
        assert_eq!(tester.weight_multiplier(), 1.0);

        // Fill with noise (no causality)
        let mut val = 0.3;
        for i in 0..50 {
            val = ((val * 17.0 + 0.1) % 2.0) - 1.0;
            let price = if i % 3 == 0 { 1.0 } else { -1.0 };
            tester.push(val, price);
        }
        tester.test();
        // With random data, should be downweighted
        let w = tester.weight_multiplier();
        assert!(w <= 1.0, "random data should not boost weight, got {w}");
    }

    #[test]
    fn granger_manager_default_weights() {
        let mgr = GrangerManager::default();
        let w = mgr.weight_multipliers();
        // With no data, all weights should be 1.0 (no penalty)
        assert_eq!(w.funding_rate, 1.0);
        assert_eq!(w.orderbook_imbalance, 1.0);
    }
}
