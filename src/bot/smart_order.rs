//! Smart order routing: try limit orders first, fall back to market orders,
//! and split large orders into iceberg chunks to reduce market impact.

/// Order type decision from the smart router.
#[derive(Debug, Clone)]
pub enum OrderType {
    Market,
    Limit { offset_from_mid: f64 },
}

/// Routes orders intelligently based on spread width, book depth, and order size.
#[derive(Debug, Clone)]
pub struct SmartOrderRouter {
    /// How long (seconds) to wait for a limit order to fill before falling back.
    pub max_wait_secs: u64,
    /// If spread < this threshold, prefer a limit order.
    pub spread_threshold: f64,
    /// If order size (USD) exceeds this, split into iceberg chunks.
    pub iceberg_threshold: f64,
    /// Size of each iceberg chunk (USD).
    pub chunk_size: f64,
}

impl SmartOrderRouter {
    pub fn new() -> Self {
        Self {
            max_wait_secs: 10,
            spread_threshold: 0.03,
            iceberg_threshold: 50.0,
            chunk_size: 10.0,
        }
    }

    /// Decide whether to use a limit or market order based on current spread and depth.
    ///
    /// When the spread is tight (< `spread_threshold`), places a limit order
    /// one tick inside the spread to capture a better fill price.
    /// Otherwise falls back to a market order for guaranteed execution.
    pub fn route_order(&self, spread: f64, _depth: f64, _amount: f64) -> OrderType {
        if spread < self.spread_threshold {
            // Place limit 1 tick inside the spread
            OrderType::Limit {
                offset_from_mid: spread / 2.0 - 0.005,
            }
        } else {
            OrderType::Market
        }
    }

    /// Split a large order into equally-sized iceberg chunks.
    ///
    /// Orders below `iceberg_threshold` are returned as-is.
    /// Larger orders are divided into `chunk_size` pieces (evenly distributed).
    pub fn iceberg_split(&self, total_amount: f64) -> Vec<f64> {
        if !total_amount.is_finite() || total_amount <= 0.0 {
            return Vec::new();
        }
        if total_amount <= self.iceberg_threshold {
            return vec![total_amount];
        }
        if !total_amount.is_finite()
            || total_amount <= 0.0
            || !self.chunk_size.is_finite()
            || self.chunk_size <= 0.0
        {
            return Vec::new();
        }
        // This is an advisory allocation plan. Bound memory even for a huge
        // operator amount; no executor submits the individual chunks.
        let n = ((total_amount / self.chunk_size).ceil() as usize).clamp(1, 1000);
        let chunk = total_amount / n as f64;
        vec![chunk; n]
    }
}

impl Default for SmartOrderRouter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tight_spread_routes_limit() {
        let router = SmartOrderRouter::new();
        let order = router.route_order(0.01, 100.0, 5.0);
        assert!(matches!(order, OrderType::Limit { .. }));
    }

    #[test]
    fn wide_spread_routes_market() {
        let router = SmartOrderRouter::new();
        let order = router.route_order(0.05, 100.0, 5.0);
        assert!(matches!(order, OrderType::Market));
    }

    #[test]
    fn small_order_no_split() {
        let router = SmartOrderRouter::new();
        let chunks = router.iceberg_split(20.0);
        assert_eq!(chunks.len(), 1);
        assert!((chunks[0] - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn large_order_splits_evenly() {
        let router = SmartOrderRouter::new();
        let chunks = router.iceberg_split(100.0);
        assert_eq!(chunks.len(), 10);
        let total: f64 = chunks.iter().sum();
        assert!((total - 100.0).abs() < 1e-9);
    }
}
