use serde_json::{json, Value};

use crate::{
    domain::Signal,
    json_util::{parse_f64, parse_market_price},
};

#[derive(Debug, Clone)]
pub struct AnalysisMarketData {
    pub market_id: String,
    pub question: String,
    pub price: f64,
    pub end_date: String,
    pub liquidity: f64,
}

impl Default for AnalysisMarketData {
    fn default() -> Self {
        Self {
            market_id: String::new(),
            question: String::new(),
            price: 0.5,
            end_date: String::new(),
            liquidity: 0.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalysisResult {
    pub signal: Signal,
    pub confidence: f64,
    pub raw_confidence: Option<f64>,
    pub net_ev: f64,
    pub volatility: Option<f64>,
    pub reason: String,
    pub regime: String,
    pub forced: bool,
    pub p_up: Option<f64>,
    pub btc_change_pct: Option<f64>,
    pub funding_rate: Option<f64>,
    pub market: AnalysisMarketData,
    pub signals: Option<Value>,
    pub extra: Option<Value>,
}

impl Default for AnalysisResult {
    fn default() -> Self {
        Self {
            signal: Signal::Hold,
            confidence: 0.0,
            raw_confidence: None,
            net_ev: 0.0,
            volatility: None,
            reason: String::new(),
            regime: String::new(),
            forced: false,
            p_up: None,
            btc_change_pct: None,
            funding_rate: None,
            market: AnalysisMarketData::default(),
            signals: None,
            extra: None,
        }
    }
}

impl AnalysisResult {
    pub fn from_value(v: &Value) -> Self {
        if v.is_null() {
            return Self::default();
        }

        let market = v.get("market_data").unwrap_or(&Value::Null);
        let signals = v.get("signals").filter(|s| !s.is_null()).cloned();

        Self {
            signal: Signal::parse(v.get("signal").and_then(|x| x.as_str()).unwrap_or("HOLD")),
            confidence: v.get("confidence").and_then(parse_f64).unwrap_or(0.0),
            raw_confidence: v.get("raw_confidence").and_then(parse_f64),
            net_ev: v.get("net_ev").and_then(parse_f64).unwrap_or(0.0),
            volatility: v.get("volatility").and_then(parse_f64),
            reason: v
                .get("reason")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            regime: v
                .get("regime")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            forced: v.get("forced").and_then(|x| x.as_bool()).unwrap_or(false),
            p_up: v.get("p_up").and_then(parse_f64),
            btc_change_pct: v.get("btc_change_pct").and_then(parse_f64),
            funding_rate: v.get("funding_rate").and_then(parse_f64),
            market: AnalysisMarketData {
                market_id: market
                    .get("market_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                question: market
                    .get("question")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                price: market
                    .get("price")
                    .and_then(parse_market_price)
                    .unwrap_or(0.5),
                end_date: market
                    .get("end_date")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                liquidity: market.get("liquidity").and_then(parse_f64).unwrap_or(0.0),
            },
            signals,
            extra: v.get("extra").filter(|e| !e.is_null()).cloned(),
        }
    }

    pub fn to_value(&self) -> Value {
        let mut out = json!({
            "signal": self.signal.as_str(),
            "reason": self.reason,
            "confidence": self.confidence,
            "raw_confidence": self.raw_confidence,
            "net_ev": self.net_ev,
            "regime": self.regime,
            "forced": self.forced,
            "p_up": self.p_up,
            "btc_change_pct": self.btc_change_pct,
            "funding_rate": self.funding_rate,
            "volatility": self.volatility,
            "market_data": {
                "market_id": self.market.market_id,
                "question": self.market.question,
                "price": self.market.price,
                "end_date": self.market.end_date,
                "liquidity": self.market.liquidity,
            },
            "signals": self.signals.clone().unwrap_or(Value::Null),
        });

        if let Some(extra) = &self.extra {
            if let (Some(dst), Some(src)) = (out.as_object_mut(), extra.as_object()) {
                for (k, v) in src {
                    if k == "market_data" {
                        if let (Some(dst_md), Some(src_md)) = (
                            dst.get_mut("market_data").and_then(Value::as_object_mut),
                            v.as_object(),
                        ) {
                            for (mk, mv) in src_md {
                                dst_md.insert(mk.clone(), mv.clone());
                            }
                            continue;
                        }
                    }
                    dst.insert(k.clone(), v.clone());
                }
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AnalysisMarketData, AnalysisResult};
    use crate::domain::Signal;

    #[test]
    fn parses_analysis_with_defaults() {
        let parsed = AnalysisResult::from_value(&json!({
            "signal": "BUY_NO",
            "confidence": "0.73",
            "market_data": { "market_id": "m1", "price": "0.52" }
        }));
        assert_eq!(parsed.signal, Signal::BuyNo);
        assert_eq!(parsed.market.market_id, "m1");
        assert!((parsed.market.price - 0.52).abs() < 1e-12);
        assert_eq!(parsed.market.question, "");
        assert_eq!(parsed.market.end_date, "");
    }

    #[test]
    fn null_value_returns_hold_defaults() {
        let parsed = AnalysisResult::from_value(&serde_json::Value::Null);
        assert_eq!(parsed.signal, Signal::Hold);
        assert!((parsed.market.price - 0.5).abs() < 1e-12);
    }

    #[test]
    fn out_of_range_market_price_falls_back_to_default() {
        let parsed = AnalysisResult::from_value(&json!({
            "signal": "BUY_YES",
            "market_data": { "market_id": "m1", "price": "1.25" }
        }));
        assert!((parsed.market.price - 0.5).abs() < 1e-12);
    }

    #[test]
    fn to_value_merges_market_data_extra_fields() {
        let parsed = AnalysisResult {
            signal: Signal::BuyYes,
            confidence: 0.66,
            reason: "ok".to_string(),
            market: AnalysisMarketData {
                market_id: "m1".to_string(),
                question: "q".to_string(),
                price: 0.51,
                end_date: "2026-01-01T00:00:00Z".to_string(),
                liquidity: 123.0,
            },
            extra: Some(json!({
                "market_data": {
                    "volume": 42.0,
                    "source": "gamma"
                },
                "minute_13_rule": false
            })),
            ..AnalysisResult::default()
        };

        let v = parsed.to_value();
        assert_eq!(v.get("signal").and_then(|x| x.as_str()), Some("BUY_YES"));
        assert_eq!(
            v.pointer("/market_data/market_id").and_then(|x| x.as_str()),
            Some("m1")
        );
        assert_eq!(
            v.pointer("/market_data/volume").and_then(|x| x.as_f64()),
            Some(42.0)
        );
        assert_eq!(
            v.pointer("/market_data/source").and_then(|x| x.as_str()),
            Some("gamma")
        );
    }
}
