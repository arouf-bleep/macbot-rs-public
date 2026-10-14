use crate::http_client::JsonResponseExt;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::http_client::HTTP;

#[derive(Debug, Clone)]
pub struct PrivateRpcClient {
    url: String,
}

impl PrivateRpcClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn is_configured(&self) -> bool {
        !self.url.trim().is_empty()
    }

    pub async fn send_raw_transaction(&self, raw_tx: &str) -> Result<String> {
        if raw_tx.trim().is_empty() {
            return Err(anyhow!("raw_tx cannot be empty"));
        }
        let out = self
            .rpc_call("eth_sendRawTransaction", json!([raw_tx]))
            .await?;
        out.as_str()
            .map(ToString::to_string)
            .ok_or_else(|| anyhow!("eth_sendRawTransaction returned non-string result"))
    }

    pub async fn max_priority_fee_per_gas(&self) -> Result<u128> {
        let out = self.rpc_call("eth_maxPriorityFeePerGas", json!([])).await?;
        let hex = out
            .as_str()
            .ok_or_else(|| anyhow!("eth_maxPriorityFeePerGas returned non-string result"))?;
        parse_hex_u128(hex)
    }

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value> {
        if !self.is_configured() {
            return Err(anyhow!("private RPC URL is not configured"));
        }
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1_u64,
        });

        let resp: Value = HTTP
            .post(self.url())
            .json(&body)
            .send()
            .await?
            .bounded_json()
            .await?;
        if let Some(err) = resp.get("error") {
            return Err(anyhow!("RPC {method} error: {err}"));
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("RPC {method} response missing result"))
    }
}

pub fn gwei_to_wei(v: f64) -> u128 {
    let scaled = (v.max(0.0) * 1_000_000_000.0).round();
    if scaled.is_finite() && scaled >= 0.0 {
        scaled as u128
    } else {
        0
    }
}

pub fn wei_to_gwei(v: u128) -> f64 {
    v as f64 / 1_000_000_000.0
}

pub fn bounded_priority_fee_wei(
    baseline_wei: u128,
    spread_pct: f64,
    trigger_pct: f64,
    multiplier: f64,
    min_priority_gwei: f64,
    max_priority_gwei: f64,
) -> u128 {
    let min_wei = gwei_to_wei(min_priority_gwei);
    let max_wei = gwei_to_wei(max_priority_gwei.max(min_priority_gwei));
    let trigger = trigger_pct.max(0.0001);
    let spread = spread_pct.max(0.0);
    let mut scaled = baseline_wei.max(min_wei);

    if spread > trigger {
        let excess = spread - trigger;
        let scale = 1.0 + (excess / trigger) * multiplier.max(0.0);
        let candidate = (baseline_wei as f64 * scale).round();
        if candidate.is_finite() && candidate > 0.0 {
            scaled = candidate as u128;
        }
    }

    scaled.clamp(min_wei, max_wei)
}

pub fn parse_hex_u128(hex: &str) -> Result<u128> {
    let h = hex.trim().trim_start_matches("0x");
    if h.is_empty() {
        return Ok(0);
    }
    u128::from_str_radix(h, 16).map_err(|e| anyhow!("invalid hex u128: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{bounded_priority_fee_wei, gwei_to_wei, parse_hex_u128};

    #[test]
    fn bounded_priority_fee_respects_bounds() {
        let out = bounded_priority_fee_wei(gwei_to_wei(10.0), 0.0, 0.03, 2.0, 35.0, 180.0);
        assert_eq!(out, gwei_to_wei(35.0));
    }

    #[test]
    fn bounded_priority_fee_scales_and_caps() {
        let out = bounded_priority_fee_wei(gwei_to_wei(100.0), 0.12, 0.03, 2.0, 35.0, 180.0);
        assert!(out <= gwei_to_wei(180.0));
        assert!(out >= gwei_to_wei(100.0));
    }

    #[test]
    fn parse_hex_u128_accepts_prefixed_and_plain() {
        assert_eq!(parse_hex_u128("0x0a").unwrap_or_default(), 10);
        assert_eq!(parse_hex_u128("0a").unwrap_or_default(), 10);
    }
}
