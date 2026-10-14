use crate::http_client::JsonResponseExt;
use anyhow::{anyhow, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::http_client::HTTP;
use crate::json_util::{extract_levels, parse_market_price};

const CLOB_HOST: &str = "https://clob.polymarket.com";

/// The public CLOB host, overridable like every other CLOB caller in the
/// tree so a test can serve a canned book.
fn clob_public_base() -> String {
    std::env::var("MACBOT_CLOB_BASE").unwrap_or_else(|_| CLOB_HOST.to_string())
}
const MAX_SNAPSHOT_LEVELS: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicBookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicBookSnapshot {
    pub token_id: String,
    pub captured_at: String,
    pub source: String,
    pub midpoint: Option<f64>,
    pub best_bid: Option<f64>,
    pub best_bid_size: Option<f64>,
    pub best_ask: Option<f64>,
    pub best_ask_size: Option<f64>,
    pub spread: Option<f64>,
    pub bid_depth_usd: f64,
    pub ask_depth_usd: f64,
    pub top_bids: Vec<PublicBookLevel>,
    pub top_asks: Vec<PublicBookLevel>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SimTradePriceEvidence {
    pub version: u8,
    pub phase: String,
    pub path: String,
    pub side: String,
    pub intended_price: f64,
    pub observed_price: f64,
    pub cumulative_size: f64,
    pub note: String,
    pub snapshot: PublicBookSnapshot,
}

impl PublicBookSnapshot {
    pub fn from_order_book(
        token_id: &str,
        book: &Value,
        midpoint: Option<f64>,
        source: &str,
    ) -> Self {
        let bids = extract_levels(book, "bids");
        let asks = extract_levels(book, "asks");
        let top_bids = bids
            .iter()
            .take(MAX_SNAPSHOT_LEVELS)
            .map(|(price, size)| PublicBookLevel {
                price: *price,
                size: *size,
            })
            .collect::<Vec<_>>();
        let top_asks = asks
            .iter()
            .take(MAX_SNAPSHOT_LEVELS)
            .map(|(price, size)| PublicBookLevel {
                price: *price,
                size: *size,
            })
            .collect::<Vec<_>>();
        let best_bid = top_bids.first().map(|level| level.price);
        let best_bid_size = top_bids.first().map(|level| level.size);
        let best_ask = top_asks.first().map(|level| level.price);
        let best_ask_size = top_asks.first().map(|level| level.size);
        let spread = match (best_bid, best_ask) {
            (Some(bid), Some(ask)) if ask >= bid => Some(ask - bid),
            _ => None,
        };
        let bid_depth_usd = bids.iter().map(|(price, size)| price * size).sum::<f64>();
        let ask_depth_usd = asks.iter().map(|(price, size)| price * size).sum::<f64>();

        Self {
            token_id: token_id.to_string(),
            captured_at: Utc::now().to_rfc3339(),
            source: source.to_string(),
            midpoint,
            best_bid,
            best_bid_size,
            best_ask,
            best_ask_size,
            spread,
            bid_depth_usd,
            ask_depth_usd,
            top_bids,
            top_asks,
        }
    }

    pub fn from_bbo(
        token_id: &str,
        best_bid: Option<f64>,
        best_bid_size: Option<f64>,
        best_ask: Option<f64>,
        best_ask_size: Option<f64>,
        midpoint: Option<f64>,
        source: &str,
    ) -> Self {
        let top_bids = match (best_bid, best_bid_size) {
            (Some(price), Some(size)) if price > 0.0 && size > 0.0 => {
                vec![PublicBookLevel { price, size }]
            }
            _ => Vec::new(),
        };
        let top_asks = match (best_ask, best_ask_size) {
            (Some(price), Some(size)) if price > 0.0 && size > 0.0 => {
                vec![PublicBookLevel { price, size }]
            }
            _ => Vec::new(),
        };
        let spread = match (best_bid, best_ask) {
            (Some(bid), Some(ask)) if ask >= bid => Some(ask - bid),
            _ => None,
        };

        Self {
            token_id: token_id.to_string(),
            captured_at: Utc::now().to_rfc3339(),
            source: source.to_string(),
            midpoint,
            best_bid,
            best_bid_size,
            best_ask,
            best_ask_size,
            spread,
            bid_depth_usd: top_bids.iter().map(|level| level.price * level.size).sum(),
            ask_depth_usd: top_asks.iter().map(|level| level.price * level.size).sum(),
            top_bids,
            top_asks,
        }
    }
}

impl SimTradePriceEvidence {
    // This constructor binds one simulated price observation to its captured book evidence.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        phase: &str,
        path: &str,
        side: &str,
        intended_price: f64,
        observed_price: f64,
        cumulative_size: f64,
        note: &str,
        snapshot: PublicBookSnapshot,
    ) -> Self {
        Self {
            version: 1,
            phase: phase.to_string(),
            path: path.to_string(),
            side: side.to_string(),
            intended_price,
            observed_price,
            cumulative_size,
            note: note.to_string(),
            snapshot,
        }
    }

    pub fn as_details_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            format!(
                "{{\"version\":1,\"phase\":\"{}\",\"path\":\"{}\",\"side\":\"{}\",\"intended_price\":{},\"observed_price\":{},\"cumulative_size\":{},\"note\":\"serialization_failed\"}}",
                self.phase, self.path, self.side, self.intended_price, self.observed_price, self.cumulative_size
            )
        })
    }
}

pub async fn fetch_public_order_book(token_id: &str) -> Result<Value> {
    let url = format!("{}/book?token_id={token_id}", clob_public_base());
    let response = HTTP.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "public CLOB book request failed: {}",
            response.status()
        ));
    }
    response.bounded_json::<Value>().await
}

pub async fn fetch_public_midpoint(token_id: &str) -> Result<f64> {
    let url = format!("{}/midpoint?token_id={token_id}", clob_public_base());
    let response = HTTP.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "public CLOB midpoint request failed: {}",
            response.status()
        ));
    }
    let value: Value = response.bounded_json().await?;
    value
        .get("mid")
        .and_then(parse_market_price)
        .ok_or_else(|| anyhow!("missing or invalid midpoint for token_id={token_id}"))
}

pub async fn fetch_public_book_snapshot(token_id: &str) -> Result<PublicBookSnapshot> {
    let book = fetch_public_order_book(token_id).await?;
    let midpoint = fetch_public_midpoint(token_id).await.ok();
    Ok(PublicBookSnapshot::from_order_book(
        token_id,
        &book,
        midpoint,
        "clob_rest_book",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn snapshot_from_order_book_keeps_top_levels() {
        let book = json!({
            "bids": [
                {"price": "0.48", "size": "10"},
                {"price": "0.47", "size": "20"}
            ],
            "asks": [
                {"price": "0.52", "size": "12"},
                {"price": "0.53", "size": "18"}
            ]
        });
        let snapshot = PublicBookSnapshot::from_order_book("tok", &book, Some(0.50), "test");
        assert_eq!(snapshot.best_bid, Some(0.48));
        assert_eq!(snapshot.best_ask, Some(0.52));
        assert!(snapshot
            .spread
            .is_some_and(|spread| (spread - 0.04).abs() < 1e-9));
        assert_eq!(snapshot.top_bids.len(), 2);
        assert_eq!(snapshot.top_asks.len(), 2);
    }

    #[test]
    fn evidence_serializes_to_json() {
        let snapshot = PublicBookSnapshot::from_bbo(
            "tok",
            Some(0.49),
            Some(100.0),
            Some(0.51),
            Some(120.0),
            Some(0.50),
            "clob_ws_bbo",
        );
        let details = SimTradePriceEvidence::new(
            "entry",
            "maker_passive",
            "buy",
            0.49,
            0.49,
            100.0,
            "maker passive fill",
            snapshot,
        )
        .as_details_json();
        let parsed: Value = serde_json::from_str(&details).expect("valid json");
        assert_eq!(parsed.get("phase").and_then(Value::as_str), Some("entry"));
        assert_eq!(
            parsed
                .get("snapshot")
                .and_then(|v| v.get("source"))
                .and_then(Value::as_str),
            Some("clob_ws_bbo")
        );
    }
}
