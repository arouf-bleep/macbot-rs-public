//! Polymarket CLOB REST client.
//! EOA orders for CTF-token markets using the venue's V1/V2 contracts.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::http_client::{read_json, AUTHENTICATED_HTTP, HTTP};

use super::{
    auth::ClobAuth,
    types::{
        AssetType, BalanceAllowanceParams, LimitOrderArgs, MarketFeeParameters, OpenOrder,
        OrderInfo, OrderResponse, OrderType, Side, SignedOrder, VenueTrade,
    },
};

const CLOB_HOST: &str = "https://clob.polymarket.com";

fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub struct ClobClient {
    auth: ClobAuth,
    host: String,
    settlement_rpc: Option<String>,
}

impl ClobClient {
    #[cfg(test)]
    pub(crate) fn with_test_host(auth: ClobAuth, host: String) -> Self {
        Self {
            auth,
            host,
            settlement_rpc: None,
        }
    }
    pub fn account_address(&self) -> String {
        self.auth.address().to_ascii_lowercase()
    }
    pub fn new(auth: ClobAuth) -> Self {
        Self {
            auth,
            host: CLOB_HOST.to_string(),
            settlement_rpc: std::env::var("POLYGON_RPC_URL").ok(),
        }
    }

    pub fn with_settlement_rpc(mut self, rpc: &str) -> Result<Self> {
        super::settlement::SettlementVerifier::new(rpc)?;
        self.settlement_rpc = Some(rpc.to_string());
        Ok(self)
    }

    pub fn settlement_verifier(&self) -> Result<super::settlement::SettlementVerifier> {
        let rpc = self
            .settlement_rpc
            .as_deref()
            .ok_or_else(|| anyhow!("Polygon finalized settlement RPC is not configured"))?;
        super::settlement::SettlementVerifier::new(rpc)
    }

    pub async fn check_settlement_rpc(&self) -> Result<()> {
        self.settlement_verifier()?.validate_chain().await
    }

    // ── Signed request helper ─────────────────────────────────────────────────

    async fn signed_get(&self, path: &str) -> Result<Value> {
        self.signed_request(reqwest::Method::GET, path, None).await
    }

    async fn signed_post(&self, path: &str, body: &Value) -> Result<Value> {
        self.signed_request(reqwest::Method::POST, path, Some(body))
            .await
    }

    async fn signed_delete(&self, path: &str, body: &Value) -> Result<Value> {
        self.signed_request(reqwest::Method::DELETE, path, Some(body))
            .await
    }

    async fn signed_request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let response = self
            .signed_request_builder(method, path, body, now_ts())?
            .send()
            .await?;
        if response.status().is_redirection() {
            return Err(anyhow!("authenticated venue request refused a redirect"));
        }
        let resp: Value = read_json(response.error_for_status()?).await?;
        Ok(resp)
    }

    fn signed_request_builder(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        ts: u64,
    ) -> Result<reqwest::RequestBuilder> {
        let body_str = body
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_default();
        // The venue signs the route, excluding query parameters.
        let route = path.split('?').next().unwrap_or(path);
        let hdrs = self
            .auth
            .headers(ts, 0, method.as_str(), route, &body_str)?;
        let mut req = AUTHENTICATED_HTTP.request(method, format!("{}{path}", self.host));
        if body.is_some() {
            // Send precisely the bytes included in the HMAC.
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body_str);
        }
        for (k, v) in &hdrs {
            let mut value = reqwest::header::HeaderValue::from_str(v)?;
            // Custom venue credential names do not receive Authorization's
            // automatic sensitivity flag. Preserve their wire bytes while
            // keeping them out of request Debug output and compression caches.
            value.set_sensitive(true);
            req = req.header(k, value);
        }
        Ok(req)
    }

    async fn public_get(&self, path: &str) -> Result<Value> {
        read_json(
            HTTP.get(format!("{}{path}", self.host))
                .send()
                .await?
                .error_for_status()?,
        )
        .await
    }

    // ── Public methods ────────────────────────────────────────────────────────

    /// GET /book?token_id=... → raw orderbook Value
    pub async fn get_order_book(&self, token_id: &str) -> Result<Value> {
        validate_identifier(token_id)?;
        self.public_get(&format!("/book?token_id={token_id}")).await
    }

    pub async fn market_fee_parameters(&self, token_id: &str) -> Result<MarketFeeParameters> {
        let book = self.get_order_book(token_id).await?;
        let market = book
            .get("market")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("book is missing market identity for fee lookup"))?;
        validate_identifier(market)?;
        let info = self.public_get(&format!("/clob-markets/{market}")).await?;
        if !info
            .get("t")
            .and_then(Value::as_array)
            .is_some_and(|tokens| {
                tokens
                    .iter()
                    .any(|row| row.get("t").and_then(Value::as_str) == Some(token_id))
            })
        {
            return Err(anyhow!(
                "fee parameters do not identify the requested token"
            ));
        }
        let fees = info
            .get("fd")
            .ok_or_else(|| anyhow!("missing venue fee details"))?;
        let result = MarketFeeParameters {
            rate: fees
                .get("r")
                .and_then(Value::as_f64)
                .ok_or_else(|| anyhow!("missing venue fee rate"))?,
            exponent: fees
                .get("e")
                .and_then(Value::as_f64)
                .ok_or_else(|| anyhow!("missing venue fee exponent"))?,
            taker_only: fees
                .get("to")
                .and_then(Value::as_bool)
                .ok_or_else(|| anyhow!("missing maker fee policy"))?,
        };
        result.fee(1.0, 0.5).map_err(|e| anyhow!(e))?;
        Ok(result)
    }

    pub async fn maker_fee_is_zero(&self, token_id: &str) -> Result<bool> {
        let fees = self.market_fee_parameters(token_id).await?;
        Ok(fees.taker_only || fees.rate == 0.0)
    }

    /// Resolve venue rules and exchange version before locally signing an order.
    pub async fn create_order(&self, args: &LimitOrderArgs) -> Result<SignedOrder> {
        validate_identifier(&args.token_id)?;
        if !args.price.is_finite() || !args.size.is_finite() || args.size <= 0.0 {
            return Err(anyhow!(
                "order price and size must be finite and size must be positive"
            ));
        }
        let (version, book) = tokio::try_join!(
            self.public_get("/version"),
            self.get_order_book(&args.token_id)
        )?;
        let version = version
            .get("version")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("missing exchange version"))?;
        let tick = parse_positive_number(book.get("tick_size"))?;
        let minimum = parse_positive_number(book.get("min_order_size"))?;
        let neg_risk = book
            .get("neg_risk")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("missing market negative-risk flag"))?;
        let position_market = match book.get("version") {
            None => false,
            Some(Value::String(version)) if version == "v1" => false,
            Some(Value::String(version)) if version == "v2" => true,
            Some(other) => return Err(anyhow!("unknown market asset version {other}")),
        };
        if position_market
            && book.get("asset_id").and_then(Value::as_str) != Some(args.token_id.as_str())
        {
            return Err(anyhow!(
                "position market book does not confirm the requested asset"
            ));
        }
        let (maker_amount, taker_amount) = order_amounts(args, tick, minimum)?;
        use rand::Rng;
        // REST salt is numeric; remain exact in JavaScript JSON consumers.
        let salt = rand::thread_rng().gen::<u64>() & ((1_u64 << 53) - 1);
        if position_market {
            let ts = u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
            return self.auth.sign_order_v3(
                &args.token_id,
                maker_amount,
                taker_amount,
                args.side.as_u8(),
                salt,
                ts,
            );
        }
        match version {
            1 => {
                let fees = self
                    .public_get(&format!("/fee-rate?token_id={}", args.token_id))
                    .await?;
                let fee = fees
                    .get("base_fee")
                    .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
                    .ok_or_else(|| anyhow!("missing legacy fee rate"))?;
                self.auth.sign_order_v1(
                    &args.token_id,
                    maker_amount,
                    taker_amount,
                    args.side.as_u8(),
                    salt,
                    0,
                    0,
                    fee,
                    neg_risk,
                )
            }
            2 => {
                let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
                let ts = u64::try_from(ts).map_err(|_| anyhow!("order timestamp overflow"))?;
                self.auth.sign_order_v2(
                    &args.token_id,
                    maker_amount,
                    taker_amount,
                    args.side.as_u8(),
                    salt,
                    ts,
                    neg_risk,
                )
            }
            _ => Err(anyhow!("unsupported exchange version {version}")),
        }
    }

    /// POST /order — submit a signed order. Returns OrderResponse.
    pub async fn post_order(
        &self,
        signed: &SignedOrder,
        order_type: OrderType,
    ) -> Result<OrderResponse> {
        self.post_order_with_options(signed, order_type, false)
            .await
    }

    pub async fn post_order_with_options(
        &self,
        signed: &SignedOrder,
        order_type: OrderType,
        post_only: bool,
    ) -> Result<OrderResponse> {
        if post_only && order_type == OrderType::FOK {
            return Err(anyhow!("post-only is incompatible with FOK"));
        }
        let type_str = match order_type {
            OrderType::GTC => "GTC",
            OrderType::FOK => "FOK",
        };
        let body = json!({
            "order": signed,
            "owner": self.auth.api_key,
            "orderType": type_str,
            "postOnly": post_only,
            "deferExec": false,
        });
        let resp = self.signed_post("/order", &body).await?;
        let result: OrderResponse = serde_json::from_value(resp)?;
        if result.success == Some(false) {
            if result.making_amount > 0.0 || result.taking_amount > 0.0 {
                return Err(anyhow!("rejected order response also claims a fill"));
            }
        } else if result.success != Some(true)
            || result.order_id.trim().is_empty()
            || result.status.trim().is_empty()
        {
            return Err(anyhow!(
                "order submission response does not confirm acceptance or rejection"
            ));
        }
        Ok(result)
    }

    /// GET /data/order/{order_id} — fetch fill status.
    pub async fn get_order(&self, order_id: &str) -> Result<OrderInfo> {
        validate_identifier(order_id)?;
        let path = format!("/data/order/{order_id}");
        let resp = self.signed_get(&path).await?;
        let info: OrderInfo =
            serde_json::from_value(resp).map_err(|e| anyhow!("get_order parse error: {e}"))?;
        if info.price <= 0.0 || info.price > 1.0 || info.status.trim().is_empty() {
            return Err(anyhow!("invalid order status response"));
        }
        Ok(info)
    }

    /// DELETE /order with an orderID body — cancel an open order.
    /// Returns the list of cancelled order IDs.
    pub async fn cancel_order(&self, order_id: &str) -> Result<Vec<String>> {
        validate_identifier(order_id)?;
        let resp = self
            .signed_delete("/order", &json!({"orderID": order_id}))
            .await?;
        let canceled: Vec<String> = resp
            .get("canceled")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !canceled.iter().any(|id| id == order_id) {
            return Err(anyhow!(
                "venue did not confirm cancellation of requested order"
            ));
        }
        Ok(canceled)
    }

    /// Read the authenticated funding wallet's cached balance and allowance.
    /// Balances are integer collateral/token base units, not dollar values.
    pub async fn get_balance_allowance(&self, params: &BalanceAllowanceParams) -> Result<Value> {
        let asset_type = match params.asset_type {
            AssetType::Collateral => "COLLATERAL",
            AssetType::Conditional => "CONDITIONAL",
        };
        let mut path = format!(
            "/balance-allowance?asset_type={asset_type}&signature_type={}",
            self.auth.signature_type()
        );
        if let Some(tid) = &params.token_id {
            validate_identifier(tid)?;
            path.push_str(&format!("&token_id={tid}"));
        }
        self.signed_get(&path).await
    }

    /// Refresh the venue's cached balance/allowance; this does not grant onchain approval.
    pub async fn update_balance_allowance(&self, params: &BalanceAllowanceParams) -> Result<Value> {
        let asset_type = match params.asset_type {
            AssetType::Collateral => "COLLATERAL",
            AssetType::Conditional => "CONDITIONAL",
        };
        let mut path = format!(
            "/balance-allowance/update?asset_type={asset_type}&signature_type={}",
            self.auth.signature_type()
        );
        if let Some(tid) = &params.token_id {
            validate_identifier(tid)?;
            path.push_str(&format!("&token_id={tid}"));
        }
        self.signed_get(&path).await
    }

    /// GET open orders for this account.
    /// A failed or truncated page must never masquerade as a complete inventory.
    pub async fn list_open_orders(&self) -> Result<Vec<OpenOrder>> {
        const MAX_PAGES: usize = 20;
        let mut all = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut seen_cursors = HashSet::new();
        let mut cursor: Option<String> = Some("MA==".into());
        for _ in 0..MAX_PAGES {
            let path = with_cursor("/data/orders", cursor.as_deref());
            let resp = self.signed_get(&path).await?;
            let entries = resp
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("invalid open-orders response"))?;
            let next = resp
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|c| !c.trim().is_empty())
                .ok_or_else(|| anyhow!("open-orders response is missing a pagination cursor"))?;
            let rows = parse_open_orders_payload(&resp);
            if rows.len() != entries.len() {
                return Err(anyhow!(
                    "open-orders response contains an unidentified order"
                ));
            }
            for row in rows {
                if seen_ids.insert(row.order_id.clone()) {
                    all.push(row);
                }
            }
            match next {
                "LTE=" => return Ok(all),
                next => {
                    let next = next.to_string();
                    if !seen_cursors.insert(next.clone()) {
                        return Err(anyhow!("open-orders pagination repeated a cursor"));
                    }
                    cursor = Some(next);
                }
            }
        }
        Err(anyhow!("open-orders pagination exceeded {MAX_PAGES} pages"))
    }

    /// Authenticated account matches. Every page must succeed; partial history
    /// is never treated as proof that an accepted order did not execute.
    pub async fn list_trades(&self) -> Result<Vec<VenueTrade>> {
        let mut cursor = "MA==".to_string();
        let mut cursors = HashSet::new();
        let mut all = std::collections::BTreeMap::<String, VenueTrade>::new();
        for _ in 0..100 {
            let base = format!("/data/trades?maker_address={}", self.account_address());
            let path = with_cursor(&base, Some(&cursor));
            let response = self.signed_get(&path).await?;
            let rows = response
                .get("data")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("invalid account-trades response"))?;
            let next = response
                .get("next_cursor")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("account-trades pagination is missing"))?;
            for row in rows {
                let trade: VenueTrade = serde_json::from_value(row.clone())?;
                trade.validate().map_err(|e| anyhow!(e))?;
                if let Some(prior) = all.get(&trade.id) {
                    if prior != &trade {
                        return Err(anyhow!(
                            "account trade changed during pagination; retry a consistent inventory"
                        ));
                    }
                } else {
                    all.insert(trade.id.clone(), trade);
                }
            }
            if next == "LTE=" {
                return Ok(all.into_values().collect());
            }
            if !cursors.insert(next.to_string()) {
                return Err(anyhow!("account-trades pagination repeated a cursor"));
            }
            cursor = next.to_string();
        }
        Err(anyhow!("account-trades pagination exceeded 100 pages"))
    }
}

fn validate_identifier(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(anyhow!("invalid venue identifier"));
    }
    Ok(())
}

fn parse_positive_number(value: Option<&Value>) -> Result<f64> {
    value
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
        .filter(|x| x.is_finite() && *x > 0.0)
        .ok_or_else(|| anyhow!("missing or invalid market rule"))
}

fn order_amounts(args: &LimitOrderArgs, tick: f64, minimum: f64) -> Result<(u128, u128)> {
    if !args.price.is_finite()
        || !args.size.is_finite()
        || args.size <= 0.0
        || args.size * 100.0 > (1u64 << 53) as f64
    {
        return Err(anyhow!("invalid order price or size"));
    }
    let tick_units = (tick * 10_000.0).round();
    if !tick.is_finite()
        || !minimum.is_finite()
        || minimum <= 0.0
        || ![1.0, 10.0, 25.0, 50.0, 100.0, 1000.0].contains(&tick_units)
        || (tick * 10_000.0 - tick_units).abs() > 1e-8
    {
        return Err(anyhow!("unsupported venue tick or minimum size"));
    }
    let price_units = (args.price * 10_000.0).round();
    if args.price < tick
        || args.price > 1.0 - tick
        || (args.price * 10_000.0 - price_units).abs() > 1e-8
        || !(price_units as u64).is_multiple_of(tick_units as u64)
    {
        return Err(anyhow!("order price is outside the venue tick grid"));
    }
    // Venue SDKs floor shares to two decimals. Integer multiplication then
    // preserves exact collateral precision without floating-point casts.
    // Correct binary-float noise at whole cents (5.01 * 100 can be 500.999...).
    // The tolerance is far below the venue's six-decimal token precision.
    let share_cents = (args.size * 100.0 + 1e-9).floor() as u128;
    if share_cents == 0 || (share_cents as f64) / 100.0 < minimum {
        return Err(anyhow!("rounded order size is below the venue minimum"));
    }
    let shares = share_cents * 10_000;
    let collateral = share_cents * (price_units as u128);
    match args.side {
        Side::Buy => Ok((collateral, shares)),
        Side::Sell => Ok((shares, collateral)),
    }
}

fn with_cursor(base_path: &str, cursor: Option<&str>) -> String {
    match cursor {
        Some(c) if !c.trim().is_empty() => {
            let sep = if base_path.contains('?') { "&" } else { "?" };
            let encoded: String = c
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("{base_path}{sep}next_cursor={encoded}")
        }
        _ => base_path.to_string(),
    }
}

#[cfg(test)]
fn extract_next_cursor(resp: &Value) -> Option<String> {
    let primary = resp
        .get("next_cursor")
        .or_else(|| resp.get("nextCursor"))
        .or_else(|| resp.get("cursor"))
        .or_else(|| resp.get("next"))
        .and_then(|v| v.as_str())
        .map(ToString::to_string);
    let fallback = resp.get("meta").and_then(|meta| {
        meta.get("next_cursor")
            .or_else(|| meta.get("nextCursor"))
            .or_else(|| meta.get("cursor"))
            .or_else(|| meta.get("next"))
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
    });
    primary
        .or(fallback)
        .filter(|x| !x.trim().is_empty())
        .and_then(|x| {
            if x == "null" || x == "NONE" || x == "none" || x == "LTE=" {
                None
            } else {
                Some(x)
            }
        })
}

fn parse_open_orders_payload(resp: &Value) -> Vec<OpenOrder> {
    let arr = if let Some(a) = resp.as_array() {
        Some(a.clone())
    } else if let Some(a) = resp.get("orders").and_then(|v| v.as_array()) {
        Some(a.clone())
    } else if let Some(a) = resp.get("data").and_then(|v| v.as_array()) {
        Some(a.clone())
    } else {
        resp.get("results").and_then(|v| v.as_array()).cloned()
    };

    let mut out = Vec::new();
    for item in arr.unwrap_or_default() {
        let order =
            serde_json::from_value::<OpenOrder>(item.clone()).unwrap_or_else(|_| OpenOrder {
                order_id: item
                    .get("orderID")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("order_id").and_then(|v| v.as_str()))
                    .or_else(|| item.get("id").and_then(|v| v.as_str()))
                    .unwrap_or_default()
                    .to_string(),
                market_id: item
                    .get("market_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("market").and_then(|v| v.as_str()))
                    .or_else(|| item.get("marketId").and_then(|v| v.as_str()))
                    .unwrap_or_default()
                    .to_string(),
                token_id: item
                    .get("token_id")
                    .and_then(|v| v.as_str())
                    .or_else(|| item.get("tokenId").and_then(|v| v.as_str()))
                    .or_else(|| item.get("asset_id").and_then(|v| v.as_str()))
                    .unwrap_or_default()
                    .to_string(),
                status: item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
            });

        if !order.order_id.trim().is_empty() {
            out.push(order);
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn built_venue_requests_preserve_signatures_but_redact_credential_debug_output() {
        let auth = ClobAuth::new(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "public-api-key",
            "cHVibGljLWZpeHR1cmUtc2VjcmV0",
            "public-passphrase",
        )
        .unwrap();
        let client = ClobClient::new(auth);
        let body = json!({"signed":"body"});
        let expected = client
            .auth
            .headers(1_725_000_123, 0, "POST", "/order", r#"{"signed":"body"}"#)
            .unwrap();
        let builder = client
            .signed_request_builder(
                reqwest::Method::POST,
                "/order?public=fixture",
                Some(&body),
                1_725_000_123,
            )
            .unwrap();
        let builder_debug = format!("{builder:?}");
        let request = builder.build().unwrap();
        let request_debug = format!("{request:?}");
        for (name, value) in &expected {
            let actual = &request.headers()[name];
            assert_eq!(actual.as_bytes(), value.as_bytes());
            assert!(
                actual.is_sensitive(),
                "venue header {name} was not redacted"
            );
            assert!(!builder_debug.contains(value));
            assert!(!request_debug.contains(value));
        }
        assert_eq!(
            request.body().unwrap().as_bytes().unwrap(),
            br#"{"signed":"body"}"#
        );
        assert_eq!(request.url().query(), Some("public=fixture"));
        assert_eq!(request.method(), reqwest::Method::POST);
    }

    #[tokio::test]
    async fn authenticated_redirects_never_forward_credentials_or_signed_bodies() {
        use axum::{
            body::to_bytes,
            extract::{Request, State},
            http::{HeaderValue, StatusCode},
            response::IntoResponse,
            Json, Router,
        };
        use std::sync::{atomic::AtomicUsize, atomic::Ordering, Arc};

        async fn destination(State(count): State<Arc<AtomicUsize>>) -> Json<Value> {
            count.fetch_add(1, Ordering::SeqCst);
            Json(json!({"unexpected":"redirect followed"}))
        }
        #[derive(Clone)]
        struct RedirectState {
            location: String,
            received: Arc<AtomicUsize>,
        }
        async fn redirect(
            State(state): State<RedirectState>,
            req: Request,
        ) -> axum::response::Response {
            state.received.fetch_add(1, Ordering::SeqCst);
            assert_eq!(req.headers()["POLY_API_KEY"], "public-api-key");
            assert_eq!(req.headers()["POLY_PASSPHRASE"], "public-passphrase");
            assert!(req.headers().contains_key("POLY_SIGNATURE"));
            if req.uri().path() == "/leak" {
                return Json(json!({"unexpected":"same-origin redirect followed"})).into_response();
            }
            let status = req.uri().path()[1..].parse::<u16>().unwrap();
            let has_body = req.method() != reqwest::Method::GET;
            let bytes = to_bytes(req.into_body(), 32_768).await.unwrap();
            assert_eq!(
                bytes.as_ref(),
                if has_body {
                    br#"{"signed":"body"}"#.as_slice()
                } else {
                    b""
                }
            );
            let mut response = StatusCode::from_u16(status).unwrap().into_response();
            response.headers_mut().insert(
                reqwest::header::LOCATION,
                HeaderValue::from_str(&state.location).unwrap(),
            );
            response
        }

        let forwarded = Arc::new(AtomicUsize::new(0));
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target_router = Router::new()
            .fallback(destination)
            .with_state(forwarded.clone());
        let target_task = tokio::spawn(async move {
            axum::serve(target_listener, target_router).await.unwrap();
        });
        let received = Arc::new(AtomicUsize::new(0));
        for cross_origin in [true, false] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let location = if cross_origin {
                format!("http://{target_address}/leak")
            } else {
                "/leak".to_string()
            };
            let router = Router::new().fallback(redirect).with_state(RedirectState {
                location,
                received: received.clone(),
            });
            let task = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let auth = ClobAuth::new(
                "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "public-api-key",
                "cHVibGljLWZpeHR1cmUtc2VjcmV0",
                "public-passphrase",
            )
            .unwrap();
            let client = ClobClient::with_test_host(auth, format!("http://{address}"));
            for status in [301, 302, 307, 308] {
                for method in [
                    reqwest::Method::GET,
                    reqwest::Method::POST,
                    reqwest::Method::DELETE,
                ] {
                    let body = json!({"signed":"body"});
                    let payload = (method != reqwest::Method::GET).then_some(&body);
                    let before = received.load(Ordering::SeqCst);
                    let error = client
                        .signed_request(method, &format!("/{status}"), payload)
                        .await
                        .unwrap_err();
                    assert!(error.to_string().contains("refused a redirect"));
                    assert_eq!(received.load(Ordering::SeqCst), before + 1);
                    assert_eq!(forwarded.load(Ordering::SeqCst), 0);
                }
            }
            task.abort();
        }
        target_task.abort();
    }

    type CapturedRequests = std::sync::Arc<std::sync::Mutex<Vec<(String, String, Value)>>>;

    pub(crate) struct MockVenue {
        requests: CapturedRequests,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for MockVenue {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl MockVenue {
        pub(crate) fn requests(&self) -> Vec<(String, String, Value)> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[derive(Clone)]
    struct MockState {
        behavior: u8,
        requests: CapturedRequests,
    }

    // This server verifies the bytes on the wire independently of ClobAuth.
    // All keys and credentials below are public fixtures; it never contacts a venue.
    pub(crate) async fn mock_venue(behavior: u8) -> (ClobClient, MockVenue) {
        use axum::{
            body::to_bytes,
            extract::{Request, State},
            http::StatusCode,
            response::IntoResponse,
            Json, Router,
        };
        async fn handle(State(state): State<MockState>, req: Request) -> axum::response::Response {
            use base64::{engine::general_purpose::URL_SAFE, Engine};
            use hmac::{Hmac, Mac};
            use sha2::Sha256;
            let method = req.method().to_string();
            let uri = req.uri().to_string();
            let path = req.uri().path().to_string();
            let headers = req.headers().clone();
            let bytes = to_bytes(req.into_body(), 32_768).await.unwrap();
            let body: Value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            state
                .requests
                .lock()
                .unwrap()
                .push((method.clone(), uri.clone(), body.clone()));
            if ![
                "/book",
                "/version",
                "/fee-rate",
                "/midpoint",
                "/clob-markets/market-1",
            ]
            .contains(&path.as_str())
            {
                let ts = headers
                    .get("POLY_TIMESTAMP")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let mut mac = Hmac::<Sha256>::new_from_slice(b"public-fixture-secret").unwrap();
                mac.update(format!("{ts}{method}{path}").as_bytes());
                mac.update(&bytes);
                let expected = URL_SAFE.encode(mac.finalize().into_bytes());
                let valid = headers.get("POLY_SIGNATURE").and_then(|v| v.to_str().ok())
                    == Some(expected.as_str())
                    && headers.get("POLY_API_KEY").and_then(|v| v.to_str().ok())
                        == Some("public-api-key")
                    && headers.get("POLY_PASSPHRASE").and_then(|v| v.to_str().ok())
                        == Some("public-passphrase")
                    && !headers.contains_key("POLY_SECRET")
                    && !headers.contains_key("POLY_NONCE");
                if !valid {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error":"bad L2 contract"})),
                    )
                        .into_response();
                }
            }
            let result = match (method.as_str(), path.as_str()) {
                ("GET", "/version") => {
                    json!({"version": match state.behavior { 8 => 1, 9 => 99, _ => 2 }})
                }
                ("GET", "/midpoint") => json!({"mid":"0.20"}),
                ("GET", "/fee-rate") => json!({"base_fee":25}),
                ("GET", "/book") if state.behavior == 10 => json!({"tick_size":"0.01"}),
                ("GET", "/book") => {
                    json!({"market":"market-1", "asset_id":"123456789",
                    "version":match state.behavior {16=>json!("v2"),17=>json!(2),18=>Value::Null,19=>json!("v9"),_=>json!("v1")},
                    "tick_size":"0.01", "min_order_size":"5", "neg_risk":state.behavior == 8,
                    "bids":[{"price":"0.01","size":"100"},{"price":"0.53","size":"100"}],
                    "asks":[{"price":"0.54","size":"100"},{"price":"0.99","size":"100"}]})
                }
                ("GET", "/clob-markets/market-1") => json!({"t":[{"t":"123456789"}],
                    "fd":{"r":0.07,"e":1,"to":true}}),
                ("GET", "/data/trades") => {
                    if state.behavior == 14 {
                        json!({"data":[],"next_cursor":"repeat"})
                    } else if state.behavior == 15 {
                        json!({"error":"malformed"})
                    } else {
                        json!({"data":[{"id":"match-1","taker_order_id":"order-1",
                        "market":"market-1","asset_id":"123456789","side":"BUY",
                        "size":"5","price":"0.5","status":"TRADE_STATUS_CONFIRMED",
                        "trader_side":"MAKER","maker_address":"public-wallet",
                        "fee_rate_bps":"","maker_orders":[{"order_id":"maker-1",
                        "maker_address":"public-wallet","asset_id":"123456789",
                        "side":"SELL","matched_amount":"5","price":"0.5",
                        "fee_rate_bps":""}]}],"next_cursor":"LTE="})
                    }
                }
                ("POST", "/order") if state.behavior == 7 => {
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error":"unknown submission result"})),
                    )
                        .into_response()
                }
                ("POST", "/order") if state.behavior == 12 => json!({"error":"invalid payload"}),
                ("POST", "/order") => {
                    assert_eq!(body["owner"], "public-api-key");
                    assert!(body["order"]["salt"].as_u64().is_some());
                    assert!(body["order"]["tokenId"].as_str().is_some());
                    assert!(body["order"].get("token_id").is_none());
                    assert!(body["order"]["signature"]
                        .as_str()
                        .unwrap()
                        .starts_with("0x"));
                    if state.behavior == 8 {
                        assert_eq!(body["order"]["feeRateBps"], "25");
                        assert!(body["order"].get("timestamp").is_none());
                    } else {
                        assert!(
                            body["order"]["timestamp"]
                                .as_str()
                                .unwrap()
                                .parse::<u64>()
                                .unwrap()
                                > 0
                        );
                        assert!(body["order"].get("feeRateBps").is_none());
                    }
                    if state.behavior == 6 {
                        json!({"success":true,"orderID":"venue-order","status":"matched","makingAmount":"","takingAmount":""})
                    } else {
                        let maker = body["order"]["makerAmount"]
                            .as_str()
                            .unwrap()
                            .parse::<f64>()
                            .unwrap()
                            / 1_000_000.0;
                        let taker = body["order"]["takerAmount"]
                            .as_str()
                            .unwrap()
                            .parse::<f64>()
                            .unwrap()
                            / 1_000_000.0;
                        json!({"success":true,"orderID":"venue-order","status":"matched","makingAmount":maker.to_string(),"takingAmount":taker.to_string()})
                    }
                }
                ("GET", "/data/order/venue-order") => {
                    json!({"price":"0.53","size_matched":"5","status":"MATCHED"})
                }
                ("DELETE", "/order") => {
                    assert_eq!(body, json!({"orderID":"venue-order"}));
                    if state.behavior == 4 {
                        json!({"canceled":[],"not_canceled":{"venue-order":"still live"}})
                    } else {
                        json!({"canceled":["venue-order"]})
                    }
                }
                ("GET", "/balance-allowance/update") => {
                    assert!(
                        uri.contains("asset_type=CONDITIONAL&signature_type=0&token_id=123456789")
                    );
                    json!({})
                }
                ("GET", "/data/orders") => {
                    let second = !uri.ends_with("next_cursor=MA%3D%3D");
                    if second {
                        assert!(uri.ends_with("next_cursor=a%2Bb%2F%3D"));
                    }
                    if second && state.behavior == 1 {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"error":"page failed"})),
                        )
                            .into_response();
                    }
                    if state.behavior == 11 {
                        json!({"data":[{"id":"first"}]})
                    } else if state.behavior == 3 {
                        json!({"error":"not an order list"})
                    } else {
                        json!({"data":[{"id":if second {"second"} else {"first"},"market":"market-1","asset_id":"123456789","status":"LIVE"}],
                        "next_cursor":if !second || state.behavior == 2 {"a+b/="} else {"LTE="}})
                    }
                }
                _ => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error":"wrong endpoint"})),
                    )
                        .into_response()
                }
            };
            Json(result).into_response()
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = Router::new().fallback(handle).with_state(MockState {
            behavior,
            requests: requests.clone(),
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let auth = ClobAuth::new(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "public-api-key",
            "cHVibGljLWZpeHR1cmUtc2VjcmV0",
            "public-passphrase",
        )
        .unwrap();
        (
            ClobClient {
                auth,
                host,
                settlement_rpc: None,
            },
            MockVenue { requests, task },
        )
    }

    #[tokio::test]
    async fn account_trade_and_fee_contracts_reject_incomplete_history() {
        let (client, server) = mock_venue(13).await;
        let matches = client.list_trades().await.unwrap();
        assert_eq!(matches.len(), 1);
        assert!(matches[0].is_confirmed());
        assert_eq!(matches[0].maker_orders[0].fee_rate_bps, None);
        assert!(server
            .requests()
            .iter()
            .any(|(_, uri, _)| uri.contains("maker_address=")));
        let fees = client.market_fee_parameters("123456789").await.unwrap();
        assert!(client.maker_fee_is_zero("123456789").await.unwrap());
        assert_eq!(fees.fee(100.0, 0.5).unwrap(), 1.75);
        for behavior in [14, 15] {
            let (client, _server) = mock_venue(behavior).await;
            assert!(client.list_trades().await.is_err());
        }
        let (client, _server) = mock_venue(16).await;
        let signed = client
            .create_order(&LimitOrderArgs {
                token_id: "123456789".into(),
                price: 0.53,
                size: 5.0,
                side: Side::Buy,
            })
            .await
            .unwrap();
        assert_eq!(signed.protocol, super::super::OrderProtocol::V3);
        for behavior in [17, 18, 19] {
            let (client, _server) = mock_venue(behavior).await;
            assert!(client
                .create_order(&LimitOrderArgs {
                    token_id: "123456789".into(),
                    price: 0.53,
                    size: 5.0,
                    side: Side::Buy
                })
                .await
                .is_err());
        }
    }

    #[tokio::test]
    async fn venue_http_contract_covers_auth_schema_cancellation_and_pagination() {
        for behavior in [0, 8] {
            let (client, server) = mock_venue(behavior).await;
            for side in [Side::Buy, Side::Sell] {
                let signed = client
                    .create_order(&LimitOrderArgs {
                        token_id: "123456789".into(),
                        price: 0.53,
                        size: 5.0,
                        side,
                    })
                    .await
                    .unwrap();
                let response = client
                    .post_order_with_options(&signed, OrderType::GTC, true)
                    .await
                    .unwrap();
                assert_eq!(response.order_id, "venue-order");
                let wire = server
                    .requests()
                    .into_iter()
                    .rev()
                    .find(|r| r.0 == "POST")
                    .unwrap()
                    .2;
                assert_eq!(wire["postOnly"], true);
                assert_eq!(wire["deferExec"], false);
                assert_eq!(
                    wire["order"]["side"],
                    if side == Side::Buy { "BUY" } else { "SELL" }
                );
                if side == Side::Sell {
                    assert_eq!(response.making_amount, 5.0);
                    assert_eq!(response.taking_amount, 2.65);
                }
                assert!(client
                    .post_order_with_options(&signed, OrderType::FOK, true)
                    .await
                    .is_err());
            }
            let order = client.get_order("venue-order").await.unwrap();
            assert_eq!(order.size_matched, 5.0);
            assert_eq!(
                client.cancel_order("venue-order").await.unwrap(),
                vec!["venue-order"]
            );
            client
                .update_balance_allowance(&BalanceAllowanceParams {
                    asset_type: AssetType::Conditional,
                    token_id: Some("123456789".into()),
                })
                .await
                .unwrap();
            assert_eq!(client.list_open_orders().await.unwrap().len(), 2);
        }
    }

    #[tokio::test]
    async fn incomplete_venue_results_do_not_masquerade_as_success() {
        for behavior in [1, 2, 3, 11] {
            let (client, _server) = mock_venue(behavior).await;
            assert!(client.list_open_orders().await.is_err());
        }
        let (client, _server) = mock_venue(4).await;
        assert!(client.cancel_order("venue-order").await.is_err());
        for behavior in [9, 10] {
            let (client, server) = mock_venue(behavior).await;
            assert!(client
                .create_order(&LimitOrderArgs {
                    token_id: "123456789".into(),
                    price: 0.53,
                    size: 5.0,
                    side: Side::Buy
                })
                .await
                .is_err());
            assert!(!server.requests().iter().any(|r| r.0 == "POST"));
        }
        let (client, _server) = mock_venue(12).await;
        let signed = client
            .create_order(&LimitOrderArgs {
                token_id: "123456789".into(),
                price: 0.53,
                size: 5.0,
                side: Side::Buy,
            })
            .await
            .unwrap();
        assert!(client.post_order(&signed, OrderType::GTC).await.is_err());
    }

    #[test]
    fn parse_open_orders_handles_common_variants() {
        let resp = json!({
            "orders": [
                {"orderID": "a1", "market": "m1", "tokenId": "t1", "status": "OPEN"},
                {"order_id": "a2", "market_id": "m2", "token_id": "t2", "status": "open"},
                {"id": "a3", "marketId": "m3", "asset_id": "t3"}
            ]
        });
        let rows = parse_open_orders_payload(&resp);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].order_id, "a1");
        assert_eq!(rows[1].order_id, "a2");
        assert_eq!(rows[2].order_id, "a3");
    }

    #[test]
    fn next_cursor_parser_handles_root_and_meta_fields() {
        let a = json!({"next_cursor":"abc"});
        let b = json!({"meta":{"nextCursor":"def"}});
        let c = json!({"next":"null"});
        assert_eq!(extract_next_cursor(&a).as_deref(), Some("abc"));
        assert_eq!(extract_next_cursor(&b).as_deref(), Some("def"));
        assert!(extract_next_cursor(&c).is_none());
    }

    #[test]
    fn with_cursor_appends_query_safely() {
        assert_eq!(with_cursor("/orders", Some("x")), "/orders?next_cursor=x");
        assert_eq!(
            with_cursor("/orders?status=open", Some("x")),
            "/orders?status=open&next_cursor=x"
        );
        assert_eq!(
            with_cursor("/orders", Some("a+b/=")),
            "/orders?next_cursor=a%2Bb%2F%3D"
        );
        assert!(extract_next_cursor(&json!({"next_cursor":"LTE="})).is_none());
    }

    #[test]
    fn buy_and_sell_amounts_match_official_sdk_sizing() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/clob_contract_vectors.json"
        ))
        .unwrap();
        for case in fixture["sizing"].as_array().unwrap() {
            let args = LimitOrderArgs {
                token_id: "123456789".into(),
                price: case["price"].as_f64().unwrap(),
                size: case["size"].as_f64().unwrap(),
                side: if case["side"] == "BUY" {
                    Side::Buy
                } else {
                    Side::Sell
                },
            };
            let amounts = order_amounts(&args, case["tick"].as_f64().unwrap(), 5.0).unwrap();
            assert_eq!(amounts.0.to_string(), case["maker"].as_str().unwrap());
            assert_eq!(amounts.1.to_string(), case["taker"].as_str().unwrap());
        }
    }

    #[test]
    fn whole_cent_share_sizes_do_not_lose_a_cent_to_float_noise() {
        for cents in [501_u128, 803, 1701, 909] {
            let args = LimitOrderArgs {
                token_id: "123456789".into(),
                price: 0.53,
                size: cents as f64 / 100.0,
                side: Side::Sell,
            };
            let (shares, collateral) = order_amounts(&args, 0.01, 5.0).unwrap();
            assert_eq!(shares, cents * 10_000);
            assert_eq!(collateral, cents * 5300);
        }
    }

    #[test]
    fn invalid_or_undersized_orders_are_not_signed() {
        let mut args = LimitOrderArgs {
            token_id: "123".into(),
            price: 0.51,
            size: 5.0,
            side: Side::Buy,
        };
        for price in [f64::NAN, f64::INFINITY, -0.1, 0.0, 1.0, 0.515] {
            args.price = price;
            assert!(order_amounts(&args, 0.01, 5.0).is_err());
        }
        args.price = 0.51;
        for size in [f64::NAN, f64::INFINITY, -1.0, 0.0, 4.999] {
            args.size = size;
            assert!(order_amounts(&args, 0.01, 5.0).is_err());
        }
        args.size = 5.0;
        assert!(order_amounts(&args, f64::NAN, 5.0).is_err());
        assert!(order_amounts(&args, 0.01, f64::NAN).is_err());
        assert!(validate_identifier("x&token_id=other").is_err());
        assert!(validate_identifier("../other").is_err());
    }
}
