#![allow(dead_code, unused_imports)]
use crate::http_client::{JsonResponseExt, HTTP};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsSignal {
    pub headline_sentiment: f64, // -1.0 to 1.0
    pub event_type: EventType,
    pub urgency: f64,        // 0.0 to 1.0
    pub source_count: usize, // number of sources reporting same event
    pub headlines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum EventType {
    Normal,
    EtfFlow,           // ETF inflow/outflow
    FedSpeaker,        // Fed/central bank mention
    ExchangeHack,      // Exchange security incident
    WhaleMovement,     // Large transfer
    Regulatory,        // Government/regulation news
    TechnicalBreakout, // Technical analysis event
}

/// Keyword-based NLP classifier for crypto news
pub struct NewsClassifier {
    bullish_keywords: Vec<(&'static str, f64)>,
    bearish_keywords: Vec<(&'static str, f64)>,
    event_keywords: Vec<(&'static str, EventType)>,
}

impl Default for NewsClassifier {
    fn default() -> Self {
        Self::new()
    }
}

impl NewsClassifier {
    pub fn new() -> Self {
        Self {
            bullish_keywords: vec![
                ("etf inflow", 0.8),
                ("etf approved", 0.9),
                ("institutional buy", 0.7),
                ("adoption", 0.5),
                ("bullish", 0.6),
                ("all-time high", 0.7),
                ("accumulation", 0.5),
                ("buy signal", 0.6),
                ("breakout", 0.5),
                ("upgrade", 0.4),
                ("partnership", 0.4),
                ("integration", 0.3),
                ("halving", 0.5),
                ("scarcity", 0.4),
                ("demand", 0.3),
            ],
            bearish_keywords: vec![
                ("etf outflow", -0.8),
                ("sec lawsuit", -0.7),
                ("hack", -0.9),
                ("exploit", -0.8),
                ("crash", -0.7),
                ("bearish", -0.6),
                ("sell-off", -0.7),
                ("liquidation", -0.5),
                ("ban", -0.8),
                ("regulation", -0.4),
                ("crackdown", -0.7),
                ("mt gox", -0.6),
                ("dump", -0.6),
                ("ponzi", -0.5),
                ("fraud", -0.7),
                ("rate hike", -0.5),
                ("hawkish", -0.4),
            ],
            event_keywords: vec![
                ("etf", EventType::EtfFlow),
                ("fed ", EventType::FedSpeaker),
                ("fomc", EventType::FedSpeaker),
                ("powell", EventType::FedSpeaker),
                ("rate", EventType::FedSpeaker),
                ("hack", EventType::ExchangeHack),
                ("exploit", EventType::ExchangeHack),
                ("whale", EventType::WhaleMovement),
                ("transfer", EventType::WhaleMovement),
                ("sec ", EventType::Regulatory),
                ("regulation", EventType::Regulatory),
                ("ban", EventType::Regulatory),
                ("law", EventType::Regulatory),
                ("breakout", EventType::TechnicalBreakout),
            ],
        }
    }

    /// Classify a headline
    pub fn classify(&self, headline: &str) -> (f64, EventType) {
        let lower = headline.to_lowercase();
        let mut sentiment = 0.0_f64;
        let mut event = EventType::Normal;

        for (keyword, weight) in &self.bullish_keywords {
            if lower.contains(keyword) {
                sentiment += weight;
            }
        }
        for (keyword, weight) in &self.bearish_keywords {
            if lower.contains(keyword) {
                sentiment += weight;
            }
        }
        for (keyword, evt) in &self.event_keywords {
            if lower.contains(keyword) {
                event = evt.clone();
                break;
            }
        }

        (sentiment.clamp(-1.0, 1.0), event)
    }
}

/// Fetch crypto news from CryptoPanic API (free tier)
pub async fn fetch_crypto_news(api_key: &str) -> anyhow::Result<Vec<(String, String)>> {
    let resp = HTTP
        .get("https://cryptopanic.com/api/free/v1/posts/")
        .query(&[
            ("auth_token", api_key),
            ("currencies", "BTC"),
            ("kind", "news"),
        ])
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(reqwest::Error::without_url)?
        .bounded_json::<serde_json::Value>()
        .await?;

    let mut headlines = Vec::new();
    if let Some(results) = resp.get("results").and_then(|r| r.as_array()) {
        for item in results.iter().take(20) {
            let title = item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or("")
                .to_string();
            let source = item
                .get("source")
                .and_then(|s| s.get("title"))
                .and_then(|t| t.as_str())
                .unwrap_or("unknown")
                .to_string();
            if !title.is_empty() {
                headlines.push((title, source));
            }
        }
    }

    Ok(headlines)
}

/// Compute aggregate news signal from recent headlines
pub fn compute_news_signal(
    headlines: &[(String, String)],
    classifier: &NewsClassifier,
) -> NewsSignal {
    if headlines.is_empty() {
        return NewsSignal {
            headline_sentiment: 0.0,
            event_type: EventType::Normal,
            urgency: 0.0,
            source_count: 0,
            headlines: Vec::new(),
        };
    }

    let mut total_sentiment = 0.0;
    let mut max_urgency = 0.0_f64;
    let mut primary_event = EventType::Normal;
    let mut event_count = 0;

    for (headline, _source) in headlines {
        let (sent, evt) = classifier.classify(headline);
        total_sentiment += sent;
        let abs_sent = sent.abs();
        if abs_sent > max_urgency {
            max_urgency = abs_sent;
        }
        if evt != EventType::Normal {
            primary_event = evt;
            event_count += 1;
        }
    }

    NewsSignal {
        headline_sentiment: (total_sentiment / headlines.len() as f64).clamp(-1.0, 1.0),
        event_type: primary_event,
        urgency: max_urgency,
        source_count: event_count,
        headlines: headlines.iter().map(|(h, _)| h.clone()).take(5).collect(),
    }
}
