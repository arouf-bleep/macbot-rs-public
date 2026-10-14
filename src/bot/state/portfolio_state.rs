use tokio::sync::RwLock;

use crate::db::Trade;

pub struct PortfolioState {
    pub trades: RwLock<Vec<Trade>>,
    pub balance: RwLock<f64>,
}

impl PortfolioState {
    pub fn new(trades: Vec<Trade>, balance: f64) -> Self {
        Self {
            trades: RwLock::new(trades),
            balance: RwLock::new(balance),
        }
    }
}
