#![allow(dead_code, unused_imports)]
//! Cross-market signals: ETH/SOL Polymarket resolutions as BTC leading indicators.
//!
//! ETH Up/Down and SOL Up/Down markets on Polymarket are correlated with BTC.
//! If ETH Up/Down just resolved YES and BTC hasn't moved yet, that's a leading indicator.
//! We scrape recent resolutions and look for cross-asset momentum signals.

use crate::http_client::JsonResponseExt;
use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::http_client::HTTP;

/// Recent cross-market resolution signal.
#[derive(Debug, Clone)]
pub struct CrossMarketSignal {
    /// Asset that resolved ("ETH", "SOL", etc.)
    pub asset: String,
    /// Whether the UP outcome won (true = up, false = down)
    pub resolved_up: bool,
    /// Time since resolution in seconds (0 = just now)
    pub secs_since_resolution: f64,
    /// How much the signal leads BTC (estimated seconds)
    pub lead_time_secs: f64,
}

/// Summary of cross-market leading indicators.
#[derive(Debug, Clone, Default)]
pub struct CrossMarketData {
    pub signals: Vec<CrossMarketSignal>,
    /// Net bullish signal: +1 = strong bullish, -1 = strong bearish, 0 = mixed/none
    pub net_signal: f64,
    /// Confidence [0, 1]
    pub confidence: f64,
}

/// Compute cross-market signal from resolved ETH/SOL Polymarket markets.
///
/// Logic:
/// 1. Fetch recent ETH Up/Down and SOL Up/Down resolutions from Gamma API
/// 2. If ETH/SOL went UP but BTC hasn't moved yet → BUY_YES (BTC likely to follow)
/// 3. If ETH/SOL went DOWN but BTC hasn't recovered → BUY_NO
///
/// Only uses resolutions within the last 2 minutes (fresh signal only).
pub fn compute_cross_market_signal(signals: &[CrossMarketSignal]) -> CrossMarketData {
    if signals.is_empty() {
        return CrossMarketData::default();
    }

    let now_threshold_secs = 120.0; // Only count signals from last 2 minutes

    let mut bull_weight = 0.0_f64;
    let mut bear_weight = 0.0_f64;

    for sig in signals {
        if sig.secs_since_resolution > now_threshold_secs {
            continue;
        }

        // Weight decays with time: freshest signal = full weight
        let time_weight = (1.0 - sig.secs_since_resolution / now_threshold_secs).max(0.0);

        // ETH has higher correlation with BTC than SOL
        let asset_weight = match sig.asset.as_str() {
            "ETH" => 1.0,
            "SOL" => 0.7,
            _ => 0.5,
        };

        let weight = time_weight * asset_weight;

        if sig.resolved_up {
            bull_weight += weight;
        } else {
            bear_weight += weight;
        }
    }

    let total = bull_weight + bear_weight;
    if total < 0.1 {
        return CrossMarketData {
            signals: signals.to_vec(),
            net_signal: 0.0,
            confidence: 0.0,
        };
    }

    let net_signal = (bull_weight - bear_weight) / total;
    let confidence = (total / 2.0).clamp(0.0, 1.0); // Max confidence at 2 signals

    CrossMarketData {
        signals: signals.to_vec(),
        net_signal,
        confidence,
    }
}

/// Fetch recent ETH and SOL Up/Down market resolutions from Polymarket Gamma API.
/// Returns a list of CrossMarketSignal events from the last 5 minutes.
pub async fn fetch_cross_market_resolutions() -> Result<Vec<CrossMarketSignal>> {
    let now = chrono::Utc::now();
    let cutoff = (now - chrono::Duration::minutes(5)).to_rfc3339();

    // Fetch recently resolved ETH Up/Down markets
    // These are correlated with BTC and often lead it by 1-2 minutes
    let url = format!(
        "https://gamma-api.polymarket.com/events?limit=50&order=endDate&ascending=false&closed=true&end_date_min={}",
        cutoff
    );

    let resp: Value = HTTP
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("cross-market fetch: {e}"))?
        .bounded_json()
        .await
        .unwrap_or(Value::Null);

    let events = resp
        .as_array()
        .cloned()
        .or_else(|| resp.get("data").and_then(|v| v.as_array()).cloned())
        .unwrap_or_default();

    let mut signals = Vec::new();

    for event in &events {
        let slug = event.get("slug").and_then(|v| v.as_str()).unwrap_or("");
        let title = event.get("title").and_then(|v| v.as_str()).unwrap_or("");

        // Detect ETH and SOL up/down markets
        let asset = if slug.contains("eth")
            && (slug.contains("updown") || slug.contains("up-or-down"))
        {
            "ETH"
        } else if slug.contains("sol") && (slug.contains("updown") || slug.contains("up-or-down")) {
            "SOL"
        } else if title.to_lowercase().contains("eth")
            && title.to_lowercase().contains("up or down")
        {
            "ETH"
        } else if title.to_lowercase().contains("sol")
            && title.to_lowercase().contains("up or down")
        {
            "SOL"
        } else {
            continue;
        };

        // Get the resolution — look at markets within the event
        let sub_markets = event
            .get("markets")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for market in &sub_markets {
            let winner = market.get("winner").and_then(|v| v.as_str()).unwrap_or("");

            if winner.is_empty() {
                continue;
            }

            let resolved_up =
                winner.to_uppercase().contains("UP") || winner.to_uppercase().contains("YES");

            // Parse end_date to compute secs_since_resolution
            let end_date = market.get("endDate").and_then(|v| v.as_str()).unwrap_or("");

            let secs_since = if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(end_date) {
                let age = now - dt.with_timezone(&chrono::Utc);
                age.num_seconds().max(0) as f64
            } else {
                120.0 // Unknown age — treat as old
            };

            if secs_since <= 300.0 {
                // Only include resolutions from last 5 minutes
                signals.push(CrossMarketSignal {
                    asset: asset.to_string(),
                    resolved_up,
                    secs_since_resolution: secs_since,
                    lead_time_secs: if asset == "ETH" { 45.0 } else { 60.0 },
                });
            }
        }
    }

    Ok(signals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_market_signal_bullish() {
        let signals = vec![
            CrossMarketSignal {
                asset: "ETH".to_string(),
                resolved_up: true,
                secs_since_resolution: 30.0,
                lead_time_secs: 45.0,
            },
            CrossMarketSignal {
                asset: "SOL".to_string(),
                resolved_up: true,
                secs_since_resolution: 60.0,
                lead_time_secs: 60.0,
            },
        ];

        let data = compute_cross_market_signal(&signals);
        assert!(data.net_signal > 0.0, "Both resolved UP → bullish signal");
        assert!(data.confidence > 0.0);
    }

    #[test]
    fn cross_market_signal_stale() {
        let signals = vec![CrossMarketSignal {
            asset: "ETH".to_string(),
            resolved_up: true,
            secs_since_resolution: 200.0, // Too old
            lead_time_secs: 45.0,
        }];

        let data = compute_cross_market_signal(&signals);
        assert_eq!(data.net_signal, 0.0, "Stale signal should be ignored");
    }
}
