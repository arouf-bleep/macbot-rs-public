//! Read-only proof of one already executed standard CTF collateral-adapter
//! redemption. Unsupported contracts, mixed operations and uncertain evidence
//! are refused. No method in this module signs or submits a transaction.
use alloy::primitives::{keccak256, Address, B256, U256};
use anyhow::{anyhow, ensure, Context, Result};
use chrono::{TimeZone, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

use super::settlement::SettlementVerifier;

pub const CTF: &str = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045";
pub const ADAPTER: &str = "0xada100db00ca00073811820692005400218fce1f";
pub const PUSD: &str = "0xc011a7e12a19f7b1f670d46f03b03f3342e82dfb";
pub const USDCE: &str = "0x2791bca1f2de4661ed88a30c99a7a9449aa84174";
pub(crate) const MAX_EXACT_UNITS: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedemptionRequest {
    pub transaction_hash: String,
    pub condition_id: String,
    pub trade_ids: Vec<i64>,
}

impl RedemptionRequest {
    pub fn validate(&self) -> Result<()> {
        hash(&self.transaction_hash)?;
        ensure!(
            hash(&self.condition_id)? != B256::ZERO,
            "condition ID cannot be zero"
        );
        ensure!(
            !self.trade_ids.is_empty() && self.trade_ids.len() <= 64,
            "redemption requires between 1 and 64 local trade IDs"
        );
        let ids: HashSet<_> = self.trade_ids.iter().collect();
        ensure!(
            ids.len() == self.trade_ids.len() && self.trade_ids.iter().all(|id| *id > 0),
            "redemption trade IDs must be positive and unique"
        );
        Ok(())
    }
}

/// Constructed only by the finalized RPC verifier (or offline module tests).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct VerifiedRedemption {
    pub transaction_hash: String,
    pub condition_id: String,
    pub account: String,
    pub block_number: u64,
    pub transaction_index: u64,
    pub block_hash: String,
    pub timestamp: String,
    pub token_ids: [String; 2],
    pub burned_units: [u64; 2],
    pub payout_units: [u64; 2],
    pub total_payout_units: u64,
    pub redemption_log_index: u64,
    pub entry_transaction_hashes: Vec<String>,
    /// Full receipt and immutable contract observations form the replay baseline.
    pub evidence: Value,
}

impl SettlementVerifier {
    /// Verify cumulative entries before this redemption, including transactions
    /// within the same block. No venue timestamp substitutes for chain order.
    pub(crate) async fn verify_entry_before_redemption(
        &self,
        entry_hash: &str,
        proof: &VerifiedRedemption,
    ) -> Result<()> {
        let expected = hash(entry_hash)?;
        ensure!(
            expected != hash(&proof.transaction_hash)?,
            "mixed entry/redemption transaction is unsupported"
        );
        let receipt = self
            .rpc("eth_getTransactionReceipt", json!([entry_hash]))
            .await?;
        ensure!(
            hash(string(&receipt, "transactionHash")?)? == expected
                && quantity(field(&receipt, "status")?)? == 1,
            "original entry receipt is missing or unsuccessful"
        );
        let number = quantity(field(&receipt, "blockNumber")?)?;
        let index = quantity(field(&receipt, "transactionIndex")?)?;
        ensure!(
            number > 0
                && (number < proof.block_number
                    || (number == proof.block_number && index < proof.transaction_index)),
            "entry fill occurred after the redeemed inventory was burned"
        );
        let block = self
            .rpc(
                "eth_getBlockByNumber",
                json!([format!("0x{number:x}"), false]),
            )
            .await?;
        ensure!(
            quantity(field(&block, "number")?)? == number
                && hash(string(&receipt, "blockHash")?)? == hash(string(&block, "hash")?)?,
            "entry receipt is no longer canonical"
        );
        let seconds = quantity(field(&block, "timestamp")?)?;
        let redeemed = chrono::DateTime::parse_from_rfc3339(&proof.timestamp)?.timestamp();
        ensure!(
            seconds >= 1_577_836_800 && seconds <= u64::try_from(redeemed)?,
            "entry execution time follows redemption"
        );
        Ok(())
    }
    pub(crate) async fn verify_redemption(
        &self,
        request: &RedemptionRequest,
        account: &str,
    ) -> Result<VerifiedRedemption> {
        request.validate()?;
        let account = address(account)?;
        ensure!(
            account != Address::ZERO,
            "redemption account cannot be zero"
        );
        let (chain, receipt, finalized) = tokio::try_join!(
            self.rpc("eth_chainId", json!([])),
            self.rpc(
                "eth_getTransactionReceipt",
                json!([request.transaction_hash])
            ),
            self.rpc("eth_getBlockByNumber", json!(["finalized", false])),
        )?;
        ensure!(
            quantity(&chain)? == 137,
            "redemption RPC is not Polygon mainnet"
        );
        let finalized_number = quantity(field(&finalized, "number")?)?;
        hash(string(&finalized, "hash")?)?;
        let block_number = quantity(field(&receipt, "blockNumber")?)?;
        ensure!(
            block_number > 0 && block_number <= finalized_number,
            "redemption transaction is not finalized"
        );
        let block = self
            .rpc(
                "eth_getBlockByNumber",
                json!([format!("0x{block_number:x}"), false]),
            )
            .await?;
        ensure!(
            quantity(field(&block, "number")?)? == block_number,
            "redemption block number mismatch"
        );
        let block_hash = hash(string(&block, "hash")?)?;
        ensure!(
            hash(string(&receipt, "blockHash")?)? == block_hash,
            "redemption receipt is no longer canonical"
        );
        // EIP-1898 binds every read to the same canonical block, rather than
        // silently observing a different state between calls or after a reorg.
        let block_ref = json!({"blockHash":format!("{block_hash:#x}"),"requireCanonical":true});
        let call = |target: &'static str, signature: &'static str, args: Vec<U256>| {
            let block_ref = block_ref.clone();
            async move {
                self.rpc(
                    "eth_call",
                    json!([{"to":target,"data":call_data(signature,&args)},block_ref]),
                )
                .await
                .and_then(|v| one_word(&v))
            }
        };
        let condition = U256::from_be_bytes(hash(&request.condition_id)?.0);
        let (ctf, collateral, usdce, slots, denominator, n0, n1) = tokio::try_join!(
            call(ADAPTER, "CONDITIONAL_TOKENS()", vec![]),
            call(ADAPTER, "COLLATERAL_TOKEN()", vec![]),
            call(ADAPTER, "USDCE()", vec![]),
            call(CTF, "getOutcomeSlotCount(bytes32)", vec![condition]),
            call(CTF, "payoutDenominator(bytes32)", vec![condition]),
            call(
                CTF,
                "payoutNumerators(bytes32,uint256)",
                vec![condition, U256::ZERO]
            ),
            call(
                CTF,
                "payoutNumerators(bytes32,uint256)",
                vec![condition, U256::from(1)]
            ),
        )?;
        ensure!(
            ctf == address_word(CTF)?
                && collateral == address_word(PUSD)?
                && usdce == address_word(USDCE)?,
            "unsupported collateral-adapter deployment"
        );
        ensure!(
            slots == U256::from(2)
                && denominator > U256::ZERO
                && n0.checked_add(n1) == Some(denominator),
            "condition is not a resolved binary CTF condition"
        );
        let (c0, c1) = tokio::try_join!(
            call(
                CTF,
                "getCollectionId(bytes32,bytes32,uint256)",
                vec![U256::ZERO, condition, U256::from(1)]
            ),
            call(
                CTF,
                "getCollectionId(bytes32,bytes32,uint256)",
                vec![U256::ZERO, condition, U256::from(2)]
            ),
        )?;
        let (t0, t1) = tokio::try_join!(
            call(CTF, "getPositionId(address,bytes32)", vec![usdce, c0]),
            call(CTF, "getPositionId(address,bytes32)", vec![usdce, c1]),
        )?;
        let mut proof = decode_receipt(
            &receipt,
            &block,
            request,
            account,
            [t0, t1],
            [n0, n1],
            denominator,
        )?;
        proof.evidence = json!({"receipt":receipt,"block":{
            "number":block_number,"hash":format!("{block_hash:#x}"),"timestamp":proof.timestamp},
            "contracts":{"adapter":ADAPTER,"ctf":CTF,"collateral":PUSD,"underlying":USDCE},
            "token_ids":proof.token_ids,"payout_numerators":[n0.to_string(),n1.to_string()],
            "payout_denominator":denominator.to_string()});
        Ok(proof)
    }
}

fn address(s: &str) -> Result<Address> {
    s.parse().map_err(|_| anyhow!("invalid redemption address"))
}
fn hash(s: &str) -> Result<B256> {
    ensure!(
        s.len() == 66 && s.starts_with("0x"),
        "invalid redemption hash"
    );
    s.parse().map_err(|_| anyhow!("invalid redemption hash"))
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a Value> {
    v.get(key)
        .ok_or_else(|| anyhow!("missing redemption {key}"))
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    field(v, key)?
        .as_str()
        .ok_or_else(|| anyhow!("invalid redemption {key}"))
}
fn quantity(v: &Value) -> Result<u64> {
    let s = v
        .as_str()
        .ok_or_else(|| anyhow!("invalid redemption quantity"))?;
    let s = s
        .strip_prefix("0x")
        .ok_or_else(|| anyhow!("invalid redemption quantity"))?;
    ensure!(
        !s.is_empty() && s.len() <= 16,
        "invalid redemption quantity"
    );
    Ok(u64::from_str_radix(s, 16)?)
}
fn words(v: &Value) -> Result<Vec<U256>> {
    let s = v
        .as_str()
        .ok_or_else(|| anyhow!("invalid redemption ABI data"))?;
    let b = hex::decode(
        s.strip_prefix("0x")
            .ok_or_else(|| anyhow!("invalid redemption ABI data"))?,
    )?;
    ensure!(
        b.len() <= 512 && b.len() % 32 == 0,
        "unsupported redemption ABI length"
    );
    Ok(b.chunks_exact(32).map(|c| U256::from_be_slice(c)).collect())
}
fn one_word(v: &Value) -> Result<U256> {
    let w = words(v)?;
    ensure!(w.len() == 1, "invalid redemption contract observation");
    Ok(w[0])
}
fn address_word(s: &str) -> Result<U256> {
    Ok(U256::from_be_slice(address(s)?.as_slice()))
}
fn call_data(sig: &str, args: &[U256]) -> String {
    let selector = keccak256(sig.as_bytes());
    let mut b = selector[..4].to_vec();
    for a in args {
        b.extend(a.to_be_bytes::<32>());
    }
    format!("0x{}", hex::encode(b))
}
fn units(v: U256) -> Result<u64> {
    ensure!(
        v <= U256::from(MAX_EXACT_UNITS),
        "redemption exceeds exact accounting range"
    );
    Ok(v.to::<u64>())
}
fn topic_address(v: &Value) -> Result<Address> {
    let h = hash(
        v.as_str()
            .ok_or_else(|| anyhow!("invalid redemption topic"))?,
    )?;
    ensure!(
        h[..12].iter().all(|b| *b == 0),
        "invalid address topic padding"
    );
    Ok(Address::from_slice(&h[12..]))
}
fn topics(log: &Value) -> Result<Vec<Value>> {
    let t = field(log, "topics")?
        .as_array()
        .ok_or_else(|| anyhow!("invalid redemption topics"))?;
    ensure!(
        !t.is_empty() && t.len() <= 4,
        "unsupported redemption topics"
    );
    for v in t {
        hash(
            v.as_str()
                .ok_or_else(|| anyhow!("invalid redemption topic"))?,
        )?;
    }
    Ok(t.clone())
}
fn event(sig: &str) -> String {
    format!("{:#x}", keccak256(sig.as_bytes()))
}

/// Offline decoder receives only already block-bound contract observations.
fn decode_receipt(
    receipt: &Value,
    block: &Value,
    request: &RedemptionRequest,
    account: Address,
    token_ids: [U256; 2],
    numerators: [U256; 2],
    denominator: U256,
) -> Result<VerifiedRedemption> {
    ensure!(
        hash(string(receipt, "transactionHash")?)? == hash(&request.transaction_hash)?,
        "redemption transaction mismatch"
    );
    ensure!(
        quantity(field(receipt, "status")?)? == 1,
        "redemption transaction failed"
    );
    let block_number = quantity(field(receipt, "blockNumber")?)?;
    let transaction_index = quantity(field(receipt, "transactionIndex")?)?;
    ensure!(
        quantity(field(block, "number")?)? == block_number
            && hash(string(receipt, "blockHash")?)? == hash(string(block, "hash")?)?,
        "redemption block is not canonical"
    );
    let seconds = quantity(field(block, "timestamp")?)?;
    ensure!(
        seconds >= 1_577_836_800 && seconds <= Utc::now().timestamp() as u64 + 300,
        "implausible redemption timestamp"
    );
    let timestamp = Utc
        .timestamp_opt(i64::try_from(seconds)?, 0)
        .single()
        .context("invalid redemption time")?
        .to_rfc3339();
    ensure!(
        token_ids[0] != token_ids[1] && token_ids.iter().all(|t| *t > U256::ZERO),
        "invalid outcome token IDs"
    );
    ensure!(
        denominator > U256::ZERO && numerators[0].checked_add(numerators[1]) == Some(denominator),
        "invalid condition payout"
    );
    let logs = field(receipt, "logs")?
        .as_array()
        .ok_or_else(|| anyhow!("invalid redemption logs"))?;
    ensure!(
        !logs.is_empty() && logs.len() <= 128,
        "unsupported redemption log count"
    );
    let adapter = address(ADAPTER)?;
    let ctf = address(CTF)?;
    let pusd = address(PUSD)?;
    let usdce = address(USDCE)?;
    let zero = Address::ZERO;
    let mut indexes = HashSet::new();
    let mut transfer = None;
    let mut burned = [0_u64; 2];
    let mut burn_seen = [false; 2];
    let mut payout = None;
    let mut incoming = None;
    let mut outgoing = None;
    let mut mint = None;
    let mut wrapped = None;
    let mut last_burn = 0_u64;
    let mut first_burn = u64::MAX;
    for log in logs {
        ensure!(
            log.get("removed").and_then(Value::as_bool) == Some(false),
            "removed or incomplete redemption log"
        );
        ensure!(
            hash(string(log, "transactionHash")?)? == hash(&request.transaction_hash)?
                && quantity(field(log, "blockNumber")?)? == block_number
                && hash(string(log, "blockHash")?)? == hash(string(block, "hash")?)?,
            "redemption log identity mismatch"
        );
        let index = quantity(field(log, "logIndex")?)?;
        ensure!(indexes.insert(index), "duplicate redemption log index");
        let source = address(string(log, "address")?)?;
        let t = topics(log)?;
        let sig = t[0].as_str().unwrap().to_ascii_lowercase();
        let data = field(log, "data")?;
        if source == ctf {
            let w = words(data)?;
            if sig == event("TransferBatch(address,address,address,uint256[],uint256[])") {
                ensure!(
                    transfer.is_none()
                        && t.len() == 4
                        && topic_address(&t[1])? == adapter
                        && topic_address(&t[2])? == account
                        && topic_address(&t[3])? == adapter,
                    "mixed or unsupported outcome-token transfer"
                );
                ensure!(
                    w.len() == 8
                        && w[0] == U256::from(64)
                        && w[1] == U256::from(160)
                        && w[2] == U256::from(2)
                        && w[5] == U256::from(2)
                        && [w[3], w[4]] == token_ids,
                    "invalid standard binary transfer batch"
                );
                transfer = Some((index, [units(w[6])?, units(w[7])?]));
            } else if sig == event("TransferSingle(address,address,address,uint256,uint256)") {
                ensure!(
                    t.len() == 4
                        && w.len() == 2
                        && topic_address(&t[1])? == adapter
                        && topic_address(&t[2])? == adapter
                        && topic_address(&t[3])? == zero,
                    "unsupported outcome-token burn"
                );
                let side = token_ids
                    .iter()
                    .position(|id| *id == w[0])
                    .ok_or_else(|| anyhow!("burned token belongs to another condition"))?;
                ensure!(
                    !burn_seen[side] && w[1] > U256::ZERO,
                    "duplicate or zero token burn"
                );
                burn_seen[side] = true;
                burned[side] = units(w[1])?;
                last_burn = last_burn.max(index);
                first_burn = first_burn.min(index);
            } else if sig
                == event("PayoutRedemption(address,address,bytes32,bytes32,uint256[],uint256)")
            {
                ensure!(
                    payout.is_none()
                        && t.len() == 4
                        && topic_address(&t[1])? == adapter
                        && topic_address(&t[2])? == usdce
                        && hash(t[3].as_str().unwrap())? == B256::ZERO,
                    "mixed or unsupported redemption event"
                );
                ensure!(
                    w.len() == 6
                        && w[0] == U256::from_be_bytes(hash(&request.condition_id)?.0)
                        && w[1] == U256::from(96)
                        && w[3] == U256::from(2)
                        && w[4] == U256::from(1)
                        && w[5] == U256::from(2),
                    "unsupported redemption partition"
                );
                payout = Some((index, units(w[2])?));
            } else {
                anyhow::bail!("unsupported mixed CTF operation in redemption receipt");
            }
        } else if (source == usdce || source == pusd)
            && sig == event("Transfer(address,address,uint256)")
        {
            ensure!(t.len() == 3, "invalid collateral transfer topics");
            let from = topic_address(&t[1])?;
            let to = topic_address(&t[2])?;
            let amount = units(one_word(data)?)?;
            if source == usdce && (from == adapter || to == adapter) {
                if from == ctf && to == adapter {
                    ensure!(incoming.is_none(), "duplicate collateral payout");
                    incoming = Some((index, amount));
                } else if from == adapter && to == pusd {
                    ensure!(outgoing.is_none(), "duplicate collateral wrapping transfer");
                    outgoing = Some((index, amount));
                } else {
                    anyhow::bail!("unsupported mixed adapter collateral transfer");
                }
            } else if source == pusd && (from == account || to == account) {
                ensure!(
                    from == zero && to == account && mint.is_none(),
                    "unsupported mixed account collateral transfer"
                );
                mint = Some((index, amount));
            }
        } else if source == pusd && sig == event("Wrapped(address,address,address,uint256)") {
            ensure!(
                wrapped.is_none()
                    && t.len() == 4
                    && topic_address(&t[1])? == adapter
                    && topic_address(&t[2])? == usdce
                    && topic_address(&t[3])? == account,
                "unsupported mixed collateral wrap"
            );
            wrapped = Some((index, units(one_word(data)?)?));
        }
    }
    let (transfer_index, transferred) =
        transfer.context("missing account outcome-token transfer")?;
    ensure!(
        transferred == burned && burned.iter().any(|v| *v > 0),
        "token burn does not exactly cover account transfer"
    );
    let (payout_index, total) = payout.context("missing CTF payout redemption event")?;
    let mut allocated = [0_u64; 2];
    for i in 0..2 {
        allocated[i] = units(
            U256::from(burned[i])
                .checked_mul(numerators[i])
                .context("payout multiplication overflow")?
                / denominator,
        )?;
    }
    ensure!(
        allocated[0].checked_add(allocated[1]) == Some(total),
        "payout disagrees with resolved burned quantities"
    );
    let (out_index, out) = outgoing.context("missing adapter collateral wrapping transfer")?;
    let (mint_index, minted) = mint.context("missing actual account collateral mint")?;
    let (wrap_index, amount) = wrapped.context("missing collateral wrap event")?;
    ensure!(
        out == total && minted == total && amount == total,
        "payout is not the exact collateral received by this account"
    );
    if total > 0 {
        let (in_index, received) = incoming.context("missing CTF collateral payout transfer")?;
        ensure!(
            received == total && in_index > last_burn && in_index < payout_index,
            "CTF collateral payout ordering mismatch"
        );
    } else {
        ensure!(
            incoming.is_none(),
            "unexpected collateral payout for losing tokens"
        );
    }
    ensure!(
        transfer_index < first_burn
            && last_burn < payout_index
            && payout_index < out_index
            && out_index < mint_index
            && mint_index < wrap_index,
        "redemption event ordering mismatch"
    );
    Ok(VerifiedRedemption {
        transaction_hash: format!("{:#x}", hash(&request.transaction_hash)?),
        condition_id: format!("{:#x}", hash(&request.condition_id)?),
        account: format!("{account:#x}"),
        block_number,
        transaction_index,
        block_hash: format!("{:#x}", hash(string(block, "hash")?)?),
        timestamp,
        token_ids: token_ids.map(|t| t.to_string()),
        burned_units: burned,
        payout_units: allocated,
        total_payout_units: total,
        redemption_log_index: payout_index,
        entry_transaction_hashes: vec![],
        evidence: Value::Null,
    })
}

#[cfg(test)]
#[path = "redemption/tests.rs"]
pub(crate) mod tests;
