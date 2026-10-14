use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum OrderType {
    GTC,
    FOK,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_u8(&self) -> u8 {
        match self {
            Side::Buy => 0,
            Side::Sell => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AssetType {
    Collateral,
    Conditional,
}

/// Arguments for creating a limit order (mirrors py_clob_client.OrderArgs)
#[derive(Debug, Clone)]
pub struct LimitOrderArgs {
    pub token_id: String,
    pub price: f64,
    pub size: f64,
    pub side: Side,
}

/// A signed order ready to post to the CLOB.
#[derive(Debug, Clone)]
pub struct SignedOrder {
    /// Venue identity, computed before submission from the EIP-712 digest.
    pub(crate) order_hash: String,
    pub(crate) exchange_address: String,
    pub salt: String,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    pub fee_rate_bps: String,
    pub side: u8,
    pub signature_type: u8,
    pub signature: String,
    pub protocol: OrderProtocol,
    pub timestamp: String,
}

impl SignedOrder {
    pub fn order_id(&self) -> &str {
        &self.order_hash
    }

    pub fn exchange_address(&self) -> &str {
        &self.exchange_address
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderProtocol {
    V1,
    V2,
    /// PositionManager outcome IDs use Exchange V3, with the V2 wire schema.
    V3,
}

// The signed Solidity struct and the REST body are different contracts: REST
// expects a numeric salt, a BUY/SELL string, and camelCase field names.
impl Serialize for SignedOrder {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let salt = self
            .salt
            .parse::<u64>()
            .map_err(serde::ser::Error::custom)?;
        let side = match self.side {
            0 => "BUY",
            1 => "SELL",
            _ => return Err(serde::ser::Error::custom("invalid order side")),
        };
        let mut body = json!({
            "salt": salt,
            "maker": self.maker,
            "signer": self.signer,
            "tokenId": self.token_id,
            "makerAmount": self.maker_amount,
            "takerAmount": self.taker_amount,
            "side": side,
            "expiration": self.expiration,
            "signatureType": self.signature_type,
            "signature": self.signature,
        });
        match self.protocol {
            OrderProtocol::V1 => {
                body["taker"] = json!(self.taker);
                body["nonce"] = json!(self.nonce);
                body["feeRateBps"] = json!(self.fee_rate_bps);
            }
            OrderProtocol::V2 | OrderProtocol::V3 => {
                body["timestamp"] = json!(self.timestamp);
                // This application has no builder attribution or custom metadata.
                let zero = format!("0x{}", "00".repeat(32));
                body["metadata"] = json!(zero);
                body["builder"] = json!(zero);
            }
        }
        body.serialize(serializer)
    }
}

fn deserialize_amount<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let amount = match value {
        serde_json::Value::String(s) if s.is_empty() => Some(0.0),
        serde_json::Value::String(s) => s.parse::<f64>().ok(),
        serde_json::Value::Number(n) => n.as_f64(),
        _ => None,
    };
    amount
        .filter(|x| x.is_finite() && *x >= 0.0)
        .ok_or_else(|| {
            serde::de::Error::custom("expected a finite nonnegative number or numeric string")
        })
}

/// Response from POST /order
#[derive(Debug, Clone, Deserialize)]
pub struct OrderResponse {
    pub success: Option<bool>,
    #[serde(rename = "orderID", default)]
    pub order_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(rename = "errorMsg", default)]
    pub error_msg: String,
    #[serde(
        rename = "takingAmount",
        default,
        deserialize_with = "deserialize_amount"
    )]
    pub taking_amount: f64,
    #[serde(
        rename = "makingAmount",
        default,
        deserialize_with = "deserialize_amount"
    )]
    pub making_amount: f64,
}

/// Response from GET /order/{id}
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OrderInfo {
    #[serde(default, alias = "orderID", alias = "order_id")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_amount")]
    pub original_size: Option<f64>,
    #[serde(default, alias = "token_id")]
    pub asset_id: Option<String>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(default)]
    pub associate_trades: Vec<String>,
    #[serde(deserialize_with = "deserialize_amount")]
    pub price: f64,
    #[serde(rename = "size_matched", deserialize_with = "deserialize_amount")]
    pub size_matched: f64,
    pub status: String,
}

fn deserialize_optional_amount<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    if value.is_null() || value.as_str() == Some("") {
        return Ok(None);
    }
    let amount = value.as_f64().or_else(|| value.as_str()?.parse().ok());
    amount
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("invalid optional venue amount"))
}

/// Individual exchange match. A match is not spendable inventory until confirmed.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct VenueTrade {
    pub id: String,
    pub taker_order_id: String,
    pub market: String,
    pub asset_id: String,
    pub side: String,
    #[serde(deserialize_with = "deserialize_amount")]
    pub size: f64,
    #[serde(deserialize_with = "deserialize_amount")]
    pub price: f64,
    pub status: String,
    pub trader_side: String,
    pub maker_address: String,
    #[serde(default)]
    pub match_time: Option<String>,
    #[serde(default)]
    pub transaction_hash: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_amount")]
    pub fee_rate_bps: Option<f64>,
    #[serde(default)]
    pub maker_orders: Vec<MakerMatch>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct MarketFeeParameters {
    pub rate: f64,
    pub exponent: f64,
    pub taker_only: bool,
}

impl MarketFeeParameters {
    pub fn fee(&self, shares: f64, price: f64) -> Result<f64, &'static str> {
        if !self.rate.is_finite()
            || !(0.0..=1.0).contains(&self.rate)
            || !self.exponent.is_finite()
            || !(0.0..=10.0).contains(&self.exponent)
            || !shares.is_finite()
            || shares <= 0.0
            || !price.is_finite()
            || price <= 0.0
            || price > 1.0
        {
            return Err("invalid venue fee parameters or fill");
        }
        let fee = shares * self.rate * (price * (1.0 - price)).powf(self.exponent);
        Ok((fee * 100_000.0).round() / 100_000.0)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct MakerMatch {
    pub order_id: String,
    pub maker_address: String,
    pub asset_id: String,
    pub side: String,
    #[serde(deserialize_with = "deserialize_amount")]
    pub matched_amount: f64,
    #[serde(deserialize_with = "deserialize_amount")]
    pub price: f64,
    #[serde(default, deserialize_with = "deserialize_optional_amount")]
    pub fee_rate_bps: Option<f64>,
}

impl VenueTrade {
    pub fn is_confirmed(&self) -> bool {
        matches!(self.status.as_str(), "CONFIRMED" | "TRADE_STATUS_CONFIRMED")
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.status.as_str(), "FAILED" | "TRADE_STATUS_FAILED")
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.trim().is_empty()
            || self.taker_order_id.trim().is_empty()
            || self.asset_id.trim().is_empty()
            || self.market.trim().is_empty()
            || self.maker_address.trim().is_empty()
            || self.size <= 0.0
            || self.price <= 0.0
            || self.price > 1.0
            || !matches!(self.side.as_str(), "BUY" | "SELL")
            || !matches!(self.trader_side.as_str(), "TAKER" | "MAKER")
            || !matches!(
                self.status
                    .strip_prefix("TRADE_STATUS_")
                    .unwrap_or(&self.status),
                "MATCHED" | "MINED" | "CONFIRMED" | "RETRYING" | "FAILED"
            )
            || self.maker_orders.iter().any(|leg| {
                leg.order_id.trim().is_empty()
                    || leg.asset_id.trim().is_empty()
                    || leg.maker_address.trim().is_empty()
                    || !matches!(leg.side.as_str(), "BUY" | "SELL")
                    || leg.matched_amount <= 0.0
                    || leg.price <= 0.0
                    || leg.price > 1.0
            })
        {
            return Err("incomplete or invalid venue trade evidence");
        }
        Ok(())
    }
}

/// Open-order row from list-open-orders endpoint.
/// Parsing is intentionally tolerant to account for upstream naming variants.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct OpenOrder {
    #[serde(default, rename = "orderID", alias = "order_id", alias = "id")]
    pub order_id: String,
    #[serde(default, alias = "market", alias = "marketId")]
    pub market_id: String,
    #[serde(default, alias = "tokenId", alias = "asset_id")]
    pub token_id: String,
    #[serde(default)]
    pub status: String,
}

/// Balance/allowance update params
#[derive(Debug, Clone)]
pub struct BalanceAllowanceParams {
    pub asset_type: AssetType,
    pub token_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn venue_numeric_strings_and_unfilled_amounts_are_parsed() {
        let response: OrderResponse = serde_json::from_value(json!({
            "success": true, "orderID": "order-1", "takingAmount": "2.5", "makingAmount": "1.25"
        }))
        .unwrap();
        assert_eq!(response.taking_amount, 2.5);
        assert_eq!(response.making_amount, 1.25);
        let unfilled: OrderResponse = serde_json::from_value(json!({
            "takingAmount": "", "makingAmount": ""
        }))
        .unwrap();
        assert_eq!(unfilled.taking_amount, 0.0);
        let order: OrderInfo = serde_json::from_value(json!({
            "price": "0.51", "size_matched": "5.25", "status": "LIVE"
        }))
        .unwrap();
        assert_eq!(order.price, 0.51);
        assert_eq!(order.size_matched, 5.25);
    }

    #[test]
    fn invalid_venue_numbers_and_error_payloads_are_rejected() {
        for value in [
            json!("NaN"),
            json!("inf"),
            json!("-1"),
            json!(null),
            json!(true),
        ] {
            assert!(
                serde_json::from_value::<OrderResponse>(json!({"takingAmount": value})).is_err()
            );
        }
        assert!(serde_json::from_value::<OrderInfo>(json!({"error": "unauthorized"})).is_err());
    }
}
