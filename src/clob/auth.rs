//! Polymarket CLOB authentication.
//! L1: EIP-712 ClobAuth wallet attestation.
//! L2: HMAC-SHA256 of (timestamp + method + path + body) with API secret.
//! Order signing: EIP-712 typed data matching CTF Exchange contract on Polygon.

use alloy::{
    primitives::{keccak256, Address, B256, U256},
    signers::{local::PrivateKeySigner, SignerSync},
    sol,
    sol_types::{Eip712Domain, SolStruct},
};
use anyhow::{anyhow, Result};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD},
    Engine,
};
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use zeroize::{Zeroize, Zeroizing};

use super::types::{OrderProtocol, SignedOrder};

type HmacSha256 = Hmac<Sha256>;

// ── EIP-712 Order struct (matches py_clob_client exactly) ────────────────────
sol! {
    #[derive(Debug)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        address taker;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint256 expiration;
        uint256 nonce;
        uint256 feeRateBps;
        uint8 side;
        uint8 signatureType;
    }
}

/// Polymarket CTF Exchange domain on Polygon mainnet (chain_id = 137).
/// Domain separator matches the deployed contract — do NOT change.
const CTF_EXCHANGE: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const CHAIN_ID: u64 = 137;
const DOMAIN_NAME: &str = "Polymarket CTF Exchange";
const DOMAIN_VER: &str = "1";

const NEG_RISK_EXCHANGE: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";
const CTF_EXCHANGE_V2: &str = "0xE111180000d2663C0091e4f400237545B87B996B";
const NEG_RISK_EXCHANGE_V2: &str = "0xe2222d279d744050d28e00520010520000310F59";
const EXCHANGE_V3: &str = "0xe3333700cA9d93003F00f0F71f8515005F6c00Aa";

// The deposit-wallet wrapper commits to this exact SDK type string.
const MODERN_ORDER_TYPE: &str = "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";
const DEPOSIT_SIGN_TYPE: &str = "TypedDataSign(Order contents,string name,string version,uint256 chainId,address verifyingContract,bytes32 salt)Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";

#[cfg(test)]
mod auth_message {
    alloy::sol! {
        struct ClobAuth {
            address address;
            string timestamp;
            uint256 nonce;
            string message;
        }
    }
}

mod v2 {
    alloy::sol! {
        struct Order {
            uint256 salt;
            address maker;
            address signer;
            uint256 tokenId;
            uint256 makerAmount;
            uint256 takerAmount;
            uint8 side;
            uint8 signatureType;
            uint256 timestamp;
            bytes32 metadata;
            bytes32 builder;
        }
    }
}

pub struct ClobAuth {
    pub signer: PrivateKeySigner,
    pub api_key: String,
    pub api_secret: String,
    pub passphrase: String,
    maker_addr: Address,
    maker_hex: String,
    signer_hex: String,
    signature_type: u8,
    token_u256_cache: DashMap<String, U256>,
}

impl Drop for ClobAuth {
    fn drop(&mut self) {
        self.api_secret.zeroize();
        self.passphrase.zeroize();
        self.api_key.zeroize();
    }
}

impl ClobAuth {
    pub fn new(
        private_key: &str,
        api_key: &str,
        api_secret: &str,
        passphrase: &str,
    ) -> Result<Self> {
        Self::new_wallet(private_key, api_key, api_secret, passphrase, 0, None)
    }

    /// Configure the wallet that holds collateral and positions separately
    /// from the EOA that authenticates private requests and signs orders.
    pub fn new_wallet(
        private_key: &str,
        api_key: &str,
        api_secret: &str,
        passphrase: &str,
        signature_type: u8,
        funder: Option<&str>,
    ) -> Result<Self> {
        let signer = private_key
            .parse::<PrivateKeySigner>()
            .map_err(|e| anyhow!("Invalid private key: {e}"))?;
        if signature_type > 3 {
            return Err(anyhow!("unsupported CLOB wallet signature type"));
        }
        let maker_addr = match funder {
            Some(value) => value
                .parse::<Address>()
                .map_err(|_| anyhow!("Invalid CLOB funding wallet address"))?,
            None if signature_type == 0 => signer.address(),
            None => {
                return Err(anyhow!(
                    "CLOB smart wallet requires a funding wallet address"
                ))
            }
        };
        if maker_addr.is_zero() {
            return Err(anyhow!("CLOB funding wallet address cannot be zero"));
        }
        if signature_type == 0 && maker_addr != signer.address() {
            return Err(anyhow!("EOA funding wallet must match its signer"));
        }
        let maker_hex = format!("{:?}", maker_addr);
        let signer_hex = format!("{:?}", signer.address());
        if api_key.trim().is_empty() || passphrase.trim().is_empty() {
            return Err(anyhow!("Missing CLOB API key or passphrase"));
        }
        let auth = Self {
            signer,
            api_key: api_key.to_string(),
            api_secret: api_secret.to_string(),
            passphrase: passphrase.to_string(),
            maker_addr,
            maker_hex,
            signer_hex,
            signature_type,
            token_u256_cache: DashMap::new(),
        };
        auth.sign_l2(0, "GET", "/", "")?;
        Ok(auth)
    }

    pub fn address(&self) -> String {
        self.maker_hex.clone()
    }

    pub fn signer_address(&self) -> String {
        self.signer_hex.clone()
    }

    pub fn signature_type(&self) -> u8 {
        self.signature_type
    }

    /// L1 wallet attestation used for venue API-key creation/derivation.
    #[cfg(test)]
    fn sign_l1(&self, timestamp: u64, nonce: u64) -> Result<String> {
        let msg = auth_message::ClobAuth {
            address: self.signer.address(),
            timestamp: timestamp.to_string(),
            nonce: U256::from(nonce),
            message: "This message attests that I control the given wallet".to_string(),
        };
        let domain = alloy::sol_types::eip712_domain!(
            name: "ClobAuthDomain",
            version: "1",
            chain_id: CHAIN_ID,
        );
        let sig = self
            .signer
            .sign_hash_sync(&msg.eip712_signing_hash(&domain))
            .map_err(|e| anyhow!("L1 sign failed: {e}"))?;
        Ok(format!("0x{}", hex::encode(sig.as_bytes())))
    }

    /// L2 signs the request using the decoded API secret and URL-safe base64.
    pub fn sign_l2(&self, timestamp: u64, method: &str, path: &str, body: &str) -> Result<String> {
        let msg = format!("{}{}{}{}", timestamp, method, path, body);
        let decoded = Zeroizing::new(
            URL_SAFE
                .decode(&self.api_secret)
                .or_else(|_| URL_SAFE_NO_PAD.decode(&self.api_secret))
                .or_else(|_| STANDARD.decode(&self.api_secret))
                .map_err(|_| anyhow!("CLOB API secret is not valid base64"))?,
        );
        if decoded.is_empty() {
            return Err(anyhow!("CLOB API secret is empty"));
        }
        let mut mac =
            HmacSha256::new_from_slice(&decoded).map_err(|e| anyhow!("HMAC init: {e}"))?;
        mac.update(msg.as_bytes());
        Ok(URL_SAFE.encode(mac.finalize().into_bytes()))
    }

    // Named venue fields retain the SDK signing contract and compatibility fixtures.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_order_v1(
        &self,
        token_id: &str,
        maker_amount: u128,
        taker_amount: u128,
        side: u8,
        salt: u64,
        expiration: u64,
        nonce: u64,
        fee_rate_bps: u64,
        neg_risk: bool,
    ) -> Result<SignedOrder> {
        validate_order_amounts(maker_amount, taker_amount, side)?;
        if self.signature_type == 3 {
            return Err(anyhow!(
                "Deposit Wallet signatures require Exchange V2 or V3"
            ));
        }
        let maker_addr: Address = self.maker_addr;
        let taker_addr: Address = Address::ZERO;
        let exchange: Address = (if neg_risk {
            NEG_RISK_EXCHANGE
        } else {
            CTF_EXCHANGE
        })
        .parse()
        .map_err(|e| anyhow!("Invalid exchange address: {e}"))?;

        let order = Order {
            salt: U256::from(salt),
            maker: maker_addr,
            signer: self.signer.address(),
            taker: taker_addr,
            tokenId: self.token_id_u256(token_id)?,
            makerAmount: U256::from(maker_amount),
            takerAmount: U256::from(taker_amount),
            expiration: U256::from(expiration),
            nonce: U256::from(nonce),
            feeRateBps: U256::from(fee_rate_bps),
            side,
            signatureType: self.signature_type,
        };

        // EIP-712 domain
        let domain = alloy::sol_types::eip712_domain!(
            name: DOMAIN_NAME,
            version: DOMAIN_VER,
            chain_id: CHAIN_ID,
            verifying_contract: exchange,
        );

        let hash = order.eip712_signing_hash(&domain);
        let sig = self
            .signer
            .sign_hash_sync(&hash)
            .map_err(|e| anyhow!("Order sign failed: {e}"))?;

        Ok(SignedOrder {
            order_hash: format!("{hash:#x}"),
            exchange_address: format!("{exchange:?}"),
            salt: salt.to_string(),
            maker: self.maker_hex.clone(),
            signer: self.signer_hex.clone(),
            taker: format!("{:?}", taker_addr),
            token_id: token_id.to_string(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: expiration.to_string(),
            nonce: nonce.to_string(),
            fee_rate_bps: fee_rate_bps.to_string(),
            side,
            signature_type: self.signature_type,
            signature: format!("0x{}", hex::encode(sig.as_bytes())),
            protocol: OrderProtocol::V1,
            timestamp: String::new(),
        })
    }

    // Named venue fields retain the SDK signing contract and compatibility fixtures.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_order_v2(
        &self,
        token_id: &str,
        maker_amount: u128,
        taker_amount: u128,
        side: u8,
        salt: u64,
        timestamp_ms: u64,
        neg_risk: bool,
    ) -> Result<SignedOrder> {
        self.sign_order_modern(
            token_id,
            maker_amount,
            taker_amount,
            side,
            salt,
            timestamp_ms,
            OrderProtocol::V2,
            neg_risk,
        )
    }

    /// Only call this for an explicitly identified PositionManager outcome.
    /// Exchange V3 shares the V2 wire schema, but not its signing domain.
    pub fn sign_order_v3(
        &self,
        position_id: &str,
        maker_amount: u128,
        taker_amount: u128,
        side: u8,
        salt: u64,
        timestamp_ms: u64,
    ) -> Result<SignedOrder> {
        self.sign_order_modern(
            position_id,
            maker_amount,
            taker_amount,
            side,
            salt,
            timestamp_ms,
            OrderProtocol::V3,
            false,
        )
    }

    // Named venue fields retain the SDK signing contract and compatibility fixtures.
    #[allow(clippy::too_many_arguments)]
    fn sign_order_modern(
        &self,
        token_id: &str,
        maker_amount: u128,
        taker_amount: u128,
        side: u8,
        salt: u64,
        timestamp_ms: u64,
        protocol: OrderProtocol,
        neg_risk: bool,
    ) -> Result<SignedOrder> {
        validate_order_amounts(maker_amount, taker_amount, side)?;
        let order_signer = if self.signature_type == 3 {
            self.maker_addr
        } else {
            self.signer.address()
        };
        let order = v2::Order {
            salt: U256::from(salt),
            maker: self.maker_addr,
            signer: order_signer,
            tokenId: self.token_id_u256(token_id)?,
            makerAmount: U256::from(maker_amount),
            takerAmount: U256::from(taker_amount),
            side,
            signatureType: self.signature_type,
            timestamp: U256::from(timestamp_ms),
            metadata: Default::default(),
            builder: Default::default(),
        };
        let exchange: Address = (if protocol == OrderProtocol::V3 {
            EXCHANGE_V3
        } else if neg_risk {
            NEG_RISK_EXCHANGE_V2
        } else {
            CTF_EXCHANGE_V2
        })
        .parse()
        .map_err(|e| anyhow!("Invalid exchange address: {e}"))?;
        let domain = alloy::sol_types::eip712_domain!(
            name: DOMAIN_NAME,
            version: if protocol == OrderProtocol::V3 { "3" } else { "2" },
            chain_id: CHAIN_ID,
            verifying_contract: exchange,
        );
        let hash = order.eip712_signing_hash(&domain);
        let signature = if self.signature_type == 3 {
            self.deposit_wallet_signature(&order, &domain)?
        } else {
            let sig = self
                .signer
                .sign_hash_sync(&hash)
                .map_err(|e| anyhow!("Order sign failed: {e}"))?;
            format!("0x{}", hex::encode(sig.as_bytes()))
        };
        Ok(SignedOrder {
            order_hash: format!("{hash:#x}"),
            exchange_address: format!("{exchange:?}"),
            salt: salt.to_string(),
            maker: self.maker_hex.clone(),
            signer: format!("{order_signer:?}"),
            taker: String::new(),
            token_id: token_id.to_string(),
            maker_amount: maker_amount.to_string(),
            taker_amount: taker_amount.to_string(),
            expiration: "0".to_string(),
            nonce: String::new(),
            fee_rate_bps: String::new(),
            side,
            signature_type: self.signature_type,
            signature,
            protocol,
            timestamp: timestamp_ms.to_string(),
        })
    }

    fn deposit_wallet_signature(&self, order: &v2::Order, domain: &Eip712Domain) -> Result<String> {
        let contents_hash = order.eip712_hash_struct();
        let app_domain_separator = domain.separator();
        // ABI encoding of seven static fields in TypedDataSign. The final
        // zero word is DepositWallet's domain salt, not the order salt.
        let mut wrapped = Vec::with_capacity(7 * 32);
        wrapped.extend_from_slice(keccak256(DEPOSIT_SIGN_TYPE).as_slice());
        wrapped.extend_from_slice(contents_hash.as_slice());
        wrapped.extend_from_slice(keccak256("DepositWallet").as_slice());
        wrapped.extend_from_slice(keccak256("1").as_slice());
        wrapped.extend_from_slice(&U256::from(CHAIN_ID).to_be_bytes::<32>());
        let mut address_word = [0_u8; 32];
        address_word[12..].copy_from_slice(self.maker_addr.as_slice());
        wrapped.extend_from_slice(&address_word);
        wrapped.extend_from_slice(B256::ZERO.as_slice());

        let mut digest_bytes = Vec::with_capacity(66);
        digest_bytes.extend_from_slice(&[0x19, 0x01]);
        digest_bytes.extend_from_slice(app_domain_separator.as_slice());
        digest_bytes.extend_from_slice(keccak256(wrapped).as_slice());
        let sig = self
            .signer
            .sign_hash_sync(&keccak256(digest_bytes))
            .map_err(|e| anyhow!("Deposit Wallet order sign failed: {e}"))?;
        let mut result = Vec::with_capacity(131 + MODERN_ORDER_TYPE.len());
        result.extend_from_slice(&sig.as_bytes());
        result.extend_from_slice(app_domain_separator.as_slice());
        result.extend_from_slice(contents_hash.as_slice());
        result.extend_from_slice(MODERN_ORDER_TYPE.as_bytes());
        result.extend_from_slice(&(MODERN_ORDER_TYPE.len() as u16).to_be_bytes());
        Ok(format!("0x{}", hex::encode(result)))
    }

    #[inline]
    fn token_id_u256(&self, token_id: &str) -> Result<U256> {
        if let Some(v) = self.token_u256_cache.get(token_id) {
            return Ok(*v);
        }
        let parsed = token_id
            .parse::<U256>()
            .map_err(|e| anyhow!("Invalid token_id '{token_id}': {e}"))?;
        self.token_u256_cache.insert(token_id.to_string(), parsed);
        Ok(parsed)
    }

    /// Build L2 headers. Wallet attestations are not request HMAC signatures.
    pub fn headers(
        &self,
        timestamp: u64,
        _nonce: u64,
        method: &str,
        path: &str,
        body: &str,
    ) -> Result<std::collections::HashMap<String, String>> {
        let mut h = std::collections::HashMap::new();
        h.insert("POLY_ADDRESS".into(), self.signer_address());
        h.insert(
            "POLY_SIGNATURE".into(),
            self.sign_l2(timestamp, method, path, body)?,
        );
        h.insert("POLY_TIMESTAMP".into(), timestamp.to_string());
        h.insert("POLY_API_KEY".into(), self.api_key.clone());
        h.insert("POLY_PASSPHRASE".into(), self.passphrase.clone());
        Ok(h)
    }
}

fn validate_order_amounts(maker: u128, taker: u128, side: u8) -> Result<()> {
    if maker == 0 || taker == 0 || side > 1 {
        return Err(anyhow!(
            "order amounts must be positive and side must be BUY or SELL"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn reference() -> Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/clob_contract_vectors.json"
        ))
        .unwrap()
    }

    fn auth() -> ClobAuth {
        ClobAuth::new(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "api-key-123",
            "cHVibGljLWZpeHR1cmUtc2VjcmV0",
            "passphrase-789",
        )
        .expect("test auth should construct")
    }

    #[test]
    fn headers_and_signatures_are_consistent() {
        let auth = auth();
        let address = auth.address();
        let timestamp = 1_717_171_717;
        let nonce = 42;
        let method = "POST";
        let path = "/orders";
        let body = r#"{"market":"mkt-1","size":2}"#;

        let headers = auth
            .headers(timestamp, nonce, method, path, body)
            .expect("header generation should succeed");

        assert_eq!(headers.get("POLY_ADDRESS"), Some(&address));
        assert_eq!(
            headers.get("POLY_SIGNATURE"),
            Some(&reference()["hmac_signature"].as_str().unwrap().to_string())
        );
        assert_eq!(
            headers.get("POLY_SIGNATURE"),
            Some(
                &auth
                    .sign_l2(timestamp, method, path, body)
                    .expect("l2 signature")
            )
        );
        assert_eq!(headers.get("POLY_TIMESTAMP"), Some(&timestamp.to_string()));
        assert!(!headers.contains_key("POLY_NONCE"));
        assert!(!headers.contains_key("POLY_SECRET"));
        assert_eq!(headers.get("POLY_API_KEY"), Some(&auth.api_key));
        assert_eq!(headers.get("POLY_PASSPHRASE"), Some(&auth.passphrase));

        let expected_l1 = format!("0x{}", reference()["l1_signature"].as_str().unwrap());
        assert_eq!(auth.sign_l1(timestamp, nonce).unwrap(), expected_l1);

        let body_alt = r#"{"market":"mkt-2","size":2}"#;
        assert_ne!(
            auth.sign_l2(timestamp, method, path, body)
                .expect("l2 signature"),
            auth.sign_l2(timestamp, method, path, body_alt)
                .expect("l2 signature")
        );
    }

    #[test]
    fn signatures_and_wire_fields_match_official_sdk_vectors() {
        let auth = auth();
        let fixture = reference();
        for case in fixture["orders"].as_array().unwrap() {
            let maker = case["maker"].as_str().unwrap().parse().unwrap();
            let taker = case["taker"].as_str().unwrap().parse().unwrap();
            let side = case["side"].as_u64().unwrap() as u8;
            let neg_risk = case["neg_risk"].as_bool().unwrap();
            let order = if case["protocol"] == 1 {
                auth.sign_order_v1("123456789", maker, taker, side, 7, 9999, 11, 25, neg_risk)
            } else {
                auth.sign_order_v2("123456789", maker, taker, side, 7, 1717171717000, neg_risk)
            }
            .unwrap();
            assert_eq!(
                order.signature.strip_prefix("0x").unwrap(),
                case["signature"].as_str().unwrap()
            );
            let wire = serde_json::to_value(&order).unwrap();
            let mut keys: Vec<_> = wire.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(serde_json::to_value(keys).unwrap(), case["wire_keys"]);
            assert_eq!(wire["salt"], 7);
            assert_eq!(wire["side"], if side == 0 { "BUY" } else { "SELL" });
            assert_eq!(wire["makerAmount"], case["maker"]);
            assert_eq!(wire["takerAmount"], case["taker"]);
        }
    }

    #[test]
    fn wallet_signatures_and_order_ids_match_official_sdk() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/clob_wallet_vectors.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let auth = ClobAuth::new_wallet(
                "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "api-key-123",
                "cHVibGljLWZpeHR1cmUtc2VjcmV0",
                "passphrase-789",
                case["signature_type"].as_u64().unwrap() as u8,
                Some(case["funder"].as_str().unwrap()),
            )
            .unwrap();
            let maker = case["maker"].as_str().unwrap().parse().unwrap();
            let taker = case["taker"].as_str().unwrap().parse().unwrap();
            let side = case["side"].as_u64().unwrap() as u8;
            let neg_risk = case["neg_risk"].as_bool().unwrap();
            let order = match case["protocol"].as_u64().unwrap() {
                1 => auth.sign_order_v1("123456789", maker, taker, side, 7, 9999, 11, 25, neg_risk),
                2 => {
                    auth.sign_order_v2("123456789", maker, taker, side, 7, 1717171717000, neg_risk)
                }
                3 => auth.sign_order_v3("123456789", maker, taker, side, 7, 1717171717000),
                _ => unreachable!(),
            }
            .unwrap();
            assert_eq!(
                order.signature.strip_prefix("0x").unwrap(),
                case["signature"]
            );
            assert_eq!(
                order.order_id().strip_prefix("0x").unwrap(),
                case["order_hash"]
            );
            assert_eq!(
                order.exchange_address().to_ascii_lowercase(),
                case["exchange"].as_str().unwrap().to_ascii_lowercase()
            );
            assert_eq!(
                order.signer.to_ascii_lowercase(),
                case["signer"].as_str().unwrap().to_ascii_lowercase()
            );
            assert_eq!(
                order.maker.to_ascii_lowercase(),
                case["funder"].as_str().unwrap().to_ascii_lowercase()
            );
            let headers = auth
                .headers(1717171717, 0, "GET", "/data/trades", "")
                .unwrap();
            assert_eq!(
                headers["POLY_ADDRESS"].to_ascii_lowercase(),
                fixture["signer_address"]
                    .as_str()
                    .unwrap()
                    .to_ascii_lowercase()
            );
            assert_eq!(
                auth.address().to_ascii_lowercase(),
                case["funder"].as_str().unwrap().to_ascii_lowercase()
            );
            assert_eq!(
                auth.signature_type(),
                case["signature_type"].as_u64().unwrap() as u8
            );
            let wire = serde_json::to_value(&order).unwrap();
            assert!(wire.get("order_hash").is_none());
            assert!(wire.get("positionId").is_none());
            assert_eq!(wire["tokenId"], "123456789");
        }
    }

    #[test]
    fn invalid_wallet_configuration_fails_closed() {
        let create = |kind, funder| {
            ClobAuth::new_wallet(
                "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "api-key-123",
                "cHVibGljLWZpeHR1cmUtc2VjcmV0",
                "passphrase-789",
                kind,
                funder,
            )
        };
        for kind in 1..=3 {
            assert!(create(kind, None).is_err());
        }
        assert!(create(4, None).is_err());
        assert!(create(0, Some("0x1111111111111111111111111111111111111111")).is_err());
        assert!(create(1, Some("0x0000000000000000000000000000000000000000")).is_err());
        assert!(create(1, Some("not an address")).is_err());
        let deposit = create(3, Some("0x1111111111111111111111111111111111111111")).unwrap();
        assert!(deposit
            .sign_order_v1("123", 1, 1, 0, 7, 0, 0, 0, false)
            .is_err());
    }

    #[test]
    fn invalid_secrets_and_zero_orders_fail_closed() {
        let mut auth = auth();
        for secret in ["", "not valid base64!"] {
            auth.api_secret = secret.to_string();
            assert!(auth.headers(1717171717, 0, "POST", "/order", "{}").is_err());
        }
        assert!(auth
            .sign_order_v2("123", 0, 1, 0, 7, 1717171717000, false)
            .is_err());
        assert!(auth
            .sign_order_v2("123", 1, 1, 2, 7, 1717171717000, false)
            .is_err());
    }

    #[test]
    fn sign_order_is_deterministic_and_validates_token_id() {
        let auth = auth();
        let first = auth
            .sign_order_v1(
                "123456789",
                1_000_000,
                2_000_000,
                0,
                7,
                9_999,
                11,
                25,
                false,
            )
            .expect("valid order should sign");
        let second = auth
            .sign_order_v1(
                "123456789",
                1_000_000,
                2_000_000,
                0,
                7,
                9_999,
                11,
                25,
                false,
            )
            .expect("valid order should sign");

        assert_eq!(first.signature, second.signature);
        assert_eq!(first.salt, second.salt);
        assert_eq!(first.maker, second.maker);
        assert_eq!(first.signer, second.signer);
        assert_eq!(first.taker, second.taker);
        assert_eq!(first.token_id, second.token_id);
        let address = auth.address();
        assert_eq!(first.maker, address);
        assert_eq!(first.signer, address);
        assert_eq!(first.taker, format!("{:?}", Address::ZERO));
        assert_eq!(first.token_id, "123456789");
        assert_eq!(first.maker_amount, "1000000");
        assert_eq!(first.taker_amount, "2000000");
        assert_eq!(first.expiration, "9999");
        assert_eq!(first.nonce, "11");
        assert_eq!(first.fee_rate_bps, "25");
        assert_eq!(first.side, 0);
        assert_eq!(first.signature_type, 0);
        assert!(first.signature.starts_with("0x"));

        assert!(auth
            .sign_order_v1(
                "not-a-token",
                1_000_000,
                2_000_000,
                0,
                7,
                9_999,
                11,
                25,
                false
            )
            .is_err());
    }
}
