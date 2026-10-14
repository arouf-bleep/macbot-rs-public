//! Exact order accounting from finalized Polygon transaction receipts.
//!
//! V1 OrderFilled fees are taken from the received asset (BUY shares,
//! SELL collateral). V2/V3 always charge collateral: BUY adds the fee to
//! spending, SELL deducts it from proceeds. Never infer these amounts from
//! a limit price, a mutable fee schedule, or an acceptance response.

use alloy::primitives::{keccak256, Address, B256, U256};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

use super::types::{OrderProtocol, Side, SignedOrder};

const LEGACY_EVENT: &str =
    "OrderFilled(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)";
const MODERN_EVENT: &str =
    "OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)";
const MAX_EXACT_MICRO_UNITS: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SettlementOrderIdentity {
    pub order_id: String,
    pub exchange_address: String,
    pub maker_address: String,
    pub asset_id: String,
    pub side: Side,
    pub protocol: OrderProtocol,
}

impl From<&SignedOrder> for SettlementOrderIdentity {
    fn from(order: &SignedOrder) -> Self {
        Self {
            order_id: order.order_id().into(),
            exchange_address: order.exchange_address().into(),
            maker_address: order.maker.clone(),
            asset_id: order.token_id.clone(),
            side: if order.side == 0 {
                Side::Buy
            } else {
                Side::Sell
            },
            protocol: order.protocol,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SettledOrderFill {
    /// Durable deduplication identity is transaction_hash + log_index.
    pub transaction_hash: String,
    pub log_index: u64,
    pub block_number: u64,
    pub order_id: String,
    pub gross_shares: f64,
    pub gross_collateral: f64,
    pub fee_collateral: f64,
    pub fee_shares: f64,
}

pub struct SettlementVerifier {
    rpc_url: reqwest::Url,
    http: reqwest::Client,
}

impl SettlementVerifier {
    /// The operator must configure a trusted Polygon RPC. There is no
    /// implicit public provider and no fallback from finalized evidence.
    pub fn new(rpc_url: &str) -> Result<Self> {
        let rpc_url = reqwest::Url::parse(rpc_url)
            .map_err(|_| anyhow!("invalid Polygon settlement RPC URL"))?;
        let loopback = matches!(
            rpc_url.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        );
        if rpc_url.scheme() != "https" && !(rpc_url.scheme() == "http" && loopback) {
            return Err(anyhow!("Polygon settlement RPC must use HTTPS"));
        }
        if rpc_url.host_str().is_none() || rpc_url.fragment().is_some() {
            return Err(anyhow!("invalid Polygon settlement RPC URL"));
        }
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { rpc_url, http })
    }

    pub(super) async fn rpc(&self, method: &str, params: Value) -> Result<Value> {
        let response = self
            .http
            .post(self.rpc_url.clone())
            .json(&json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params}))
            .send()
            .await
            .map_err(|_| anyhow!("Polygon settlement RPC unavailable"))?
            .error_for_status()
            .map_err(|_| anyhow!("Polygon settlement RPC HTTP failure"))?;
        let response = crate::http_client::read_json::<Value>(response)
            .await
            .map_err(|_| anyhow!("invalid or oversized Polygon settlement RPC JSON"))?;
        if response.get("error").is_some()
            || response.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || response.get("id") != Some(&json!(1))
        {
            return Err(anyhow!(
                "Polygon settlement RPC returned an error or invalid envelope"
            ));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("Polygon settlement RPC omitted result"))
    }

    /// Preflight the configured provider before an order can be submitted.
    /// Providers without finalized-block support are not sufficient evidence.
    pub async fn validate_chain(&self) -> Result<()> {
        let (chain, finalized) = tokio::try_join!(
            self.rpc("eth_chainId", json!([])),
            self.rpc("eth_getBlockByNumber", json!(["finalized", false])),
        )?;
        if hex_u64(&chain)? != 137 {
            return Err(anyhow!("settlement RPC is not Polygon mainnet"));
        }
        if hex_u64(required(&finalized, "number")?)? == 0 {
            return Err(anyhow!("settlement RPC lacks a finalized block"));
        }
        parse_hash(required_str(&finalized, "hash")?)?;
        Ok(())
    }

    pub async fn verify(
        &self,
        transaction_hash: &str,
        identity: &SettlementOrderIdentity,
    ) -> Result<Vec<SettledOrderFill>> {
        parse_hash(transaction_hash)?;
        let (chain, receipt, finalized) = tokio::try_join!(
            self.rpc("eth_chainId", json!([])),
            self.rpc("eth_getTransactionReceipt", json!([transaction_hash])),
            self.rpc("eth_getBlockByNumber", json!(["finalized", false])),
        )?;
        let chain_id = hex_u64(&chain)?;
        if chain_id != 137 {
            return Err(anyhow!("settlement RPC is not Polygon mainnet"));
        }
        let finalized_block = hex_u64(required(&finalized, "number")?)?;
        parse_hash(required_str(&finalized, "hash")?)?;
        let receipt_block = required(&receipt, "blockNumber")?;
        let canonical = self
            .rpc("eth_getBlockByNumber", json!([receipt_block, false]))
            .await?;
        let canonical_number = hex_u64(required(&canonical, "number")?)?;
        if canonical_number != hex_u64(receipt_block)? {
            return Err(anyhow!("settlement block number mismatch"));
        }
        let canonical_hash = required(&canonical, "hash")?
            .as_str()
            .ok_or_else(|| anyhow!("missing canonical settlement block hash"))?;
        decode_finalized_receipt(
            &receipt,
            identity,
            transaction_hash,
            chain_id,
            finalized_block,
            canonical_hash,
        )
    }
}

/// Decode only the order's own OrderFilled logs from one finalized receipt.
/// A missing fill is an error, never evidence that an order did not execute.
pub fn decode_finalized_receipt(
    receipt: &Value,
    identity: &SettlementOrderIdentity,
    transaction_hash: &str,
    chain_id: u64,
    finalized_block: u64,
    canonical_block_hash: &str,
) -> Result<Vec<SettledOrderFill>> {
    if chain_id != 137 {
        return Err(anyhow!("settlement receipt is not Polygon mainnet"));
    }
    let expected_tx = parse_hash(transaction_hash)?;
    if parse_hash(required_str(receipt, "transactionHash")?)? != expected_tx {
        return Err(anyhow!("settlement transaction hash mismatch"));
    }
    if hex_u64(required(receipt, "status")?)? != 1 {
        return Err(anyhow!(
            "settlement transaction reverted or lacks successful status"
        ));
    }
    let block_number = hex_u64(required(receipt, "blockNumber")?)?;
    if block_number > finalized_block {
        return Err(anyhow!("settlement transaction is not finalized"));
    }
    let block_hash = parse_hash(required_str(receipt, "blockHash")?)?;
    if block_hash != parse_hash(canonical_block_hash)? {
        return Err(anyhow!("settlement block is no longer canonical"));
    }
    let exchange: Address = identity
        .exchange_address
        .parse()
        .map_err(|_| anyhow!("invalid settlement exchange address"))?;
    validate_exchange(exchange, identity.protocol)?;
    let maker: Address = identity
        .maker_address
        .parse()
        .map_err(|_| anyhow!("invalid settlement maker address"))?;
    if maker.is_zero() {
        return Err(anyhow!("settlement maker address cannot be zero"));
    }
    let order_hash = parse_hash(&identity.order_id)?;
    let asset: U256 = identity
        .asset_id
        .parse()
        .map_err(|_| anyhow!("invalid settlement asset ID"))?;
    if asset == U256::ZERO {
        return Err(anyhow!("settlement asset ID cannot be zero"));
    }
    let expected_topic = keccak256(if identity.protocol == OrderProtocol::V1 {
        LEGACY_EVENT
    } else {
        MODERN_EVENT
    });
    let logs = required(receipt, "logs")?
        .as_array()
        .ok_or_else(|| anyhow!("missing settlement logs"))?;
    let mut seen = HashMap::<u64, Value>::new();
    let mut fills = Vec::new();
    for log in logs {
        // Foreign events are unrelated to this order. A matching own event
        // must satisfy every identity and numeric field before accounting.
        let Some(topics) = log.get("topics").and_then(Value::as_array) else {
            continue;
        };
        if topics
            .first()
            .and_then(Value::as_str)
            .and_then(|s| parse_hash(s).ok())
            != Some(expected_topic)
            || topics
                .get(1)
                .and_then(Value::as_str)
                .and_then(|s| parse_hash(s).ok())
                != Some(order_hash)
        {
            continue;
        }
        let log_exchange: Address = required_str(log, "address")?
            .parse()
            .map_err(|_| anyhow!("invalid settlement log address"))?;
        if log_exchange != exchange {
            continue;
        }
        if topics.len() != 4 {
            return Err(anyhow!("invalid OrderFilled topics"));
        }
        let maker_word = parse_hash(
            topics[2]
                .as_str()
                .ok_or_else(|| anyhow!("invalid maker topic"))?,
        )?;
        let mut expected_maker = [0_u8; 32];
        expected_maker[12..].copy_from_slice(maker.as_slice());
        if maker_word.as_slice() != expected_maker {
            return Err(anyhow!("settlement fill belongs to another maker"));
        }
        parse_hash(
            topics[3]
                .as_str()
                .ok_or_else(|| anyhow!("invalid taker topic"))?,
        )?;
        if log.get("removed").and_then(Value::as_bool) == Some(true) {
            return Err(anyhow!("settlement fill log was removed"));
        }
        if parse_hash(required_str(log, "transactionHash")?)? != expected_tx
            || parse_hash(required_str(log, "blockHash")?)? != block_hash
            || hex_u64(required(log, "blockNumber")?)? != block_number
        {
            return Err(anyhow!("settlement log transaction or block mismatch"));
        }
        let log_index = hex_u64(required(log, "logIndex")?)?;
        if let Some(previous) = seen.get(&log_index) {
            if previous != log {
                return Err(anyhow!(
                    "conflicting settlement logs with the same identity"
                ));
            }
            continue;
        }
        seen.insert(log_index, log.clone());
        let data = required_str(log, "data")?
            .strip_prefix("0x")
            .ok_or_else(|| anyhow!("invalid settlement event data"))?;
        let bytes = hex::decode(data).map_err(|_| anyhow!("invalid settlement event hex"))?;
        let expected_len = if identity.protocol == OrderProtocol::V1 {
            5 * 32
        } else {
            7 * 32
        };
        if bytes.len() != expected_len {
            return Err(anyhow!("invalid settlement event data length"));
        }
        let words: Vec<U256> = bytes.chunks_exact(32).map(U256::from_be_slice).collect();
        let is_buy = identity.side == Side::Buy;
        if identity.protocol == OrderProtocol::V1 {
            let (maker_asset, taker_asset) = if is_buy {
                (U256::ZERO, asset)
            } else {
                (asset, U256::ZERO)
            };
            if words[0] != maker_asset || words[1] != taker_asset {
                return Err(anyhow!("settlement fill asset or side mismatch"));
            }
        } else if words[0] != U256::from(if is_buy { 0 } else { 1 }) || words[1] != asset {
            return Err(anyhow!("settlement fill asset or side mismatch"));
        }
        let (making, taking, fee) = (units(words[2])?, units(words[3])?, units(words[4])?);
        let (gross_collateral, gross_shares) = if is_buy {
            (making, taking)
        } else {
            (taking, making)
        };
        if gross_shares <= 0.0 || gross_collateral <= 0.0 || gross_collateral > gross_shares {
            return Err(anyhow!("invalid settlement fill quantity or price"));
        }
        let fee_shares = if identity.protocol == OrderProtocol::V1 && is_buy {
            fee
        } else {
            0.0
        };
        let fee_collateral = if fee_shares > 0.0 { 0.0 } else { fee };
        if fee_shares >= gross_shares || (!is_buy && fee_collateral > gross_collateral) {
            return Err(anyhow!("settlement fee exceeds proceeds"));
        }
        fills.push(SettledOrderFill {
            transaction_hash: format!("{expected_tx:#x}"),
            log_index,
            block_number,
            order_id: format!("{order_hash:#x}"),
            gross_shares,
            gross_collateral,
            fee_collateral,
            fee_shares,
        });
    }
    if fills.is_empty() {
        return Err(anyhow!(
            "no finalized OrderFilled evidence for the requested order"
        ));
    }
    fills.sort_by_key(|fill| fill.log_index);
    Ok(fills)
}

fn validate_exchange(exchange: Address, protocol: OrderProtocol) -> Result<()> {
    let allowed: &[&str] = match protocol {
        OrderProtocol::V1 => &[
            "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E",
            "0xC5d563A36AE78145C45a50134d48A1215220f80a",
        ],
        OrderProtocol::V2 => &[
            "0xE111180000d2663C0091e4f400237545B87B996B",
            "0xe2222d279d744050d28e00520010520000310F59",
        ],
        OrderProtocol::V3 => &["0xe3333700cA9d93003F00f0F71f8515005F6c00Aa"],
    };
    if !allowed
        .iter()
        .any(|value| value.parse::<Address>().ok() == Some(exchange))
    {
        return Err(anyhow!("unsupported settlement exchange or protocol"));
    }
    Ok(())
}

fn parse_hash(value: &str) -> Result<B256> {
    if !value.starts_with("0x") || value.len() != 66 {
        return Err(anyhow!("invalid settlement hash"));
    }
    value
        .parse()
        .map_err(|_| anyhow!("invalid settlement hash"))
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| anyhow!("missing settlement field {key}"))
}

fn required_str<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    required(value, key)?
        .as_str()
        .ok_or_else(|| anyhow!("invalid settlement field {key}"))
}

fn hex_u64(value: &Value) -> Result<u64> {
    let value = value
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("invalid settlement hex quantity"))?;
    u64::from_str_radix(value, 16).map_err(|_| anyhow!("invalid settlement hex quantity"))
}

fn units(amount: U256) -> Result<f64> {
    if amount > U256::from(MAX_EXACT_MICRO_UNITS) {
        return Err(anyhow!("settlement amount exceeds accounting precision"));
    }
    Ok(amount.to::<u64>() as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(protocol: OrderProtocol, side: Side) -> (SettlementOrderIdentity, Value, String) {
        let identity = SettlementOrderIdentity {
            order_id: format!("0x{}", "11".repeat(32)),
            exchange_address: match protocol {
                OrderProtocol::V1 => "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E",
                OrderProtocol::V2 => "0xE111180000d2663C0091e4f400237545B87B996B",
                OrderProtocol::V3 => "0xe3333700cA9d93003F00f0F71f8515005F6c00Aa",
            }
            .into(),
            maker_address: "0x1111111111111111111111111111111111111111".into(),
            asset_id: "123456789".into(),
            side,
            protocol,
        };
        let tx = format!("0x{}", "22".repeat(32));
        let block_hash = format!("0x{}", "33".repeat(32));
        let asset: U256 = identity.asset_id.parse().unwrap();
        let is_buy = side == Side::Buy;
        let mut values = if protocol == OrderProtocol::V1 {
            if is_buy {
                vec![U256::ZERO, asset]
            } else {
                vec![asset, U256::ZERO]
            }
        } else {
            vec![U256::from(if is_buy { 0 } else { 1 }), asset]
        };
        values.extend(if is_buy {
            [U256::from(2_500_000), U256::from(5_000_000)]
        } else {
            [U256::from(5_000_000), U256::from(2_500_000)]
        });
        values.push(U256::from(50_000));
        if protocol != OrderProtocol::V1 {
            values.extend([U256::ZERO, U256::ZERO]);
        }
        let data: Vec<u8> = values.iter().flat_map(|n| n.to_be_bytes::<32>()).collect();
        let mut maker_word = [0_u8; 32];
        maker_word[12..].copy_from_slice(
            identity
                .maker_address
                .parse::<Address>()
                .unwrap()
                .as_slice(),
        );
        let event = if protocol == OrderProtocol::V1 {
            LEGACY_EVENT
        } else {
            MODERN_EVENT
        };
        let log = json!({"address":identity.exchange_address,"topics":[format!("{:#x}",keccak256(event)),identity.order_id,format!("0x{}",hex::encode(maker_word)),format!("{:#x}",B256::ZERO)],
            "data":format!("0x{}",hex::encode(data)),"transactionHash":tx,"blockHash":block_hash,"blockNumber":"0x64","logIndex":"0x2","removed":false});
        (
            identity,
            json!({"transactionHash":tx,"blockHash":block_hash,"blockNumber":"0x64","status":"0x1","logs":[log]}),
            tx,
        )
    }

    fn decode(
        id: &SettlementOrderIdentity,
        receipt: &Value,
        tx: &str,
    ) -> Result<Vec<SettledOrderFill>> {
        decode_finalized_receipt(receipt, id, tx, 137, 101, &format!("0x{}", "33".repeat(32)))
    }

    #[test]
    fn exact_fee_units_and_directions_for_every_protocol_and_side() {
        for protocol in [OrderProtocol::V1, OrderProtocol::V2, OrderProtocol::V3] {
            for side in [Side::Buy, Side::Sell] {
                let (id, receipt, tx) = fixture(protocol, side);
                let fill = decode(&id, &receipt, &tx).unwrap().remove(0);
                assert_eq!(fill.gross_shares, 5.0);
                assert_eq!(fill.gross_collateral, 2.5);
                let fee_in_shares = protocol == OrderProtocol::V1 && side == Side::Buy;
                assert_eq!(fill.fee_shares, if fee_in_shares { 0.05 } else { 0.0 });
                assert_eq!(fill.fee_collateral, if fee_in_shares { 0.0 } else { 0.05 });
            }
        }
    }

    #[test]
    fn unrelated_and_duplicate_logs_never_double_count() {
        let (id, mut receipt, tx) = fixture(OrderProtocol::V2, Side::Buy);
        let own = receipt["logs"][0].clone();
        let mut foreign = own.clone();
        foreign["topics"][1] = json!(format!("0x{}", "44".repeat(32)));
        let mut orders_matched = own.clone();
        orders_matched["topics"][0] = json!(format!(
            "{:#x}",
            keccak256("OrdersMatched(bytes32,address,uint8,uint256,uint256,uint256)")
        ));
        receipt["logs"] = json!([own.clone(), foreign, orders_matched, own]);
        assert_eq!(decode(&id, &receipt, &tx).unwrap().len(), 1);
        let mut second = receipt["logs"][0].clone();
        second["logIndex"] = json!("0x3");
        receipt["logs"].as_array_mut().unwrap().push(second);
        assert_eq!(decode(&id, &receipt, &tx).unwrap().len(), 2);
        receipt["logs"][4]["data"] = json!("0xdeadbeef");
        receipt["logs"][4]["logIndex"] = json!("0x2");
        assert!(decode(&id, &receipt, &tx).is_err());
    }

    #[test]
    fn provisional_reverted_reorged_and_wrong_identity_receipts_fail_closed() {
        let (id, receipt, tx) = fixture(OrderProtocol::V2, Side::Buy);
        assert!(decode(&id, &Value::Null, &tx).is_err());
        assert!(decode_finalized_receipt(
            &receipt,
            &id,
            &tx,
            1,
            101,
            &format!("0x{}", "33".repeat(32))
        )
        .is_err());
        assert!(decode_finalized_receipt(
            &receipt,
            &id,
            &tx,
            137,
            99,
            &format!("0x{}", "33".repeat(32))
        )
        .is_err());
        assert!(decode_finalized_receipt(
            &receipt,
            &id,
            &tx,
            137,
            101,
            &format!("0x{}", "99".repeat(32))
        )
        .is_err());
        for (field, replacement) in [
            ("status", json!("0x0")),
            ("transactionHash", json!(format!("0x{}", "88".repeat(32)))),
            ("logs", json!([])),
        ] {
            let mut bad = receipt.clone();
            bad[field] = replacement;
            assert!(decode(&id, &bad, &tx).is_err());
        }
        for (field, replacement) in [
            ("removed", json!(true)),
            ("data", json!("0x00")),
            ("blockNumber", json!("0x65")),
            (
                "address",
                json!("0x1111111111111111111111111111111111111111"),
            ),
        ] {
            let mut bad = receipt.clone();
            bad["logs"][0][field] = replacement;
            assert!(decode(&id, &bad, &tx).is_err());
        }
        let mut wrong_asset = id.clone();
        wrong_asset.asset_id = "123456788".into();
        assert!(decode(&wrong_asset, &receipt, &tx).is_err());
        let mut wrong_side = id.clone();
        wrong_side.side = Side::Sell;
        assert!(decode(&wrong_side, &receipt, &tx).is_err());
        let mut wrong_maker = id.clone();
        wrong_maker.maker_address = "0x2222222222222222222222222222222222222222".into();
        assert!(decode(&wrong_maker, &receipt, &tx).is_err());
        let mut wrong_hash = id.clone();
        wrong_hash.order_id = format!("0x{}", "55".repeat(32));
        assert!(decode(&wrong_hash, &receipt, &tx).is_err());
    }

    #[test]
    fn rpc_urls_and_quantity_precision_are_strict() {
        assert!(SettlementVerifier::new("").is_err());
        assert!(SettlementVerifier::new("http://example.com").is_err());
        assert!(SettlementVerifier::new("https://example.com/#fragment").is_err());
        assert!(SettlementVerifier::new("https://example.com").is_ok());
        assert!(SettlementVerifier::new("http://127.0.0.1:9999").is_ok());
        assert!(units(U256::from(MAX_EXACT_MICRO_UNITS) + U256::from(1)).is_err());
    }

    #[tokio::test]
    async fn rpc_preflight_and_receipt_checks_reject_wrong_chain_missing_finality_and_reorg() {
        use axum::{extract::State, routing::post, Json, Router};

        #[derive(Clone)]
        struct RpcState {
            receipt: Value,
            behavior: u8,
        }
        async fn serve(State(state): State<RpcState>, Json(request): Json<Value>) -> Json<Value> {
            let method = request["method"].as_str().unwrap();
            let hash = state.receipt["blockHash"].clone();
            let result = match method {
                "eth_chainId" => json!(if state.behavior == 1 { "0x1" } else { "0x89" }),
                "eth_getTransactionReceipt" => state.receipt.clone(),
                "eth_getBlockByNumber" if request["params"][0] == "finalized" => {
                    if state.behavior == 2 {
                        return Json(
                            json!({"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unsupported finalized block"}}),
                        );
                    }
                    json!({"number":if state.behavior == 3 { "0x63" } else { "0x65" },"hash":hash})
                }
                "eth_getBlockByNumber" => {
                    json!({"number":"0x64","hash":if state.behavior == 4 { json!(format!("0x{}","99".repeat(32))) } else { hash }})
                }
                _ => panic!("unexpected RPC method"),
            };
            Json(json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
        }

        for behavior in 0..=4 {
            let (identity, receipt, tx) = fixture(OrderProtocol::V3, Side::Sell);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = Router::new()
                .route("/", post(serve))
                .with_state(RpcState { receipt, behavior });
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let verifier = SettlementVerifier::new(&format!("http://{address}")).unwrap();
            assert_eq!(
                verifier.validate_chain().await.is_ok(),
                behavior != 1 && behavior != 2
            );
            assert_eq!(verifier.verify(&tx, &identity).await.is_ok(), behavior == 0);
            server.abort();
        }
    }
}
