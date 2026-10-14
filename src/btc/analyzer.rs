//! BTC price fetcher + trend analyzer.
//! Port of btc_analyzer.py — Binance → Coinbase → CoinGecko fallback chain.

use crate::http_client::JsonResponseExt;
use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::http_client::HTTP;

#[derive(Debug, Clone, Default)]
pub struct TrendResult {
    pub direction: String, // "UP" | "DOWN" | "NEUTRAL"
    pub confidence: f64,
    pub momentum: f64, // pct change from market start
}

pub struct BTCAnalyzer;

impl BTCAnalyzer {
    /// Fetch current BTC/USD price. Tries Binance → Coinbase → CoinGecko.
    pub async fn get_price() -> Result<f64> {
        if let Some(p) = crate::signals::price_bus::shared_price_bus()
            .latest_price("BTCUSDT", 5.0)
            .await
        {
            return Ok(p);
        }
        // Binance
        if let Ok(p) = Self::price_binance().await {
            return Ok(p);
        }
        // Coinbase
        if let Ok(p) = Self::price_coinbase().await {
            return Ok(p);
        }
        // CoinGecko
        Self::price_coingecko().await
    }

    async fn price_binance() -> Result<f64> {
        let resp: Value = HTTP
            .get("https://api.binance.com/api/v3/ticker/price?symbol=BTCUSDT")
            .send()
            .await?
            .bounded_json()
            .await?;
        resp["price"]
            .as_str()
            .and_then(|s| s.parse().ok())
            .filter(|price: &f64| price.is_finite() && *price > 0.0)
            .ok_or_else(|| anyhow!("no price"))
    }

    async fn price_coinbase() -> Result<f64> {
        let resp: Value = HTTP
            .get("https://api.coinbase.com/v2/prices/BTC-USD/spot")
            .send()
            .await?
            .bounded_json()
            .await?;
        resp.pointer("/data/amount")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .filter(|price: &f64| price.is_finite() && *price > 0.0)
            .ok_or_else(|| anyhow!("no price"))
    }

    async fn price_coingecko() -> Result<f64> {
        let resp: Value = HTTP
            .get("https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd")
            .send()
            .await?
            .bounded_json()
            .await?;
        resp.pointer("/bitcoin/usd")
            .and_then(|v| v.as_f64())
            .filter(|price| price.is_finite() && *price > 0.0)
            .ok_or_else(|| anyhow!("no price"))
    }

    /// Fetch last N minutes of 1m Binance klines and analyze trend from `start_price`.
    pub async fn analyze_trend_from_start(
        start_price: f64,
        lookback_minutes: u64,
    ) -> Result<TrendResult> {
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol=BTCUSDT&interval=1m&limit={}",
            lookback_minutes.min(96)
        );
        let resp: Value = HTTP.get(&url).send().await?.bounded_json().await?;
        let arr = resp.as_array().ok_or_else(|| anyhow!("expected array"))?;

        if arr.is_empty() || !start_price.is_finite() || start_price <= 0.0 {
            return Ok(TrendResult {
                direction: "NEUTRAL".into(),
                confidence: 0.5,
                momentum: 0.0,
            });
        }

        let current_close: f64 = arr
            .last()
            .and_then(|k| k.as_array())
            .and_then(|a| a.get(4)?.as_str())
            .and_then(|s| s.parse().ok())
            .filter(|price: &f64| price.is_finite() && *price > 0.0)
            .unwrap_or(start_price);

        let momentum = (current_close - start_price) / start_price * 100.0;

        // Simple momentum → direction
        let (direction, confidence) = if momentum > 0.30 {
            ("UP", (0.55 + (momentum / 0.50).min(1.0) * 0.20).min(0.85))
        } else if momentum < -0.30 {
            (
                "DOWN",
                (0.55 + (momentum.abs() / 0.50).min(1.0) * 0.20).min(0.85),
            )
        } else {
            ("NEUTRAL", 0.50)
        };

        Ok(TrendResult {
            direction: direction.into(),
            confidence,
            momentum,
        })
    }
}
