use super::*;

pub(crate) fn fixture(
    burned: [u64; 2],
    nums: [u64; 2],
) -> (
    Value,
    Value,
    RedemptionRequest,
    Address,
    [U256; 2],
    [U256; 2],
    U256,
) {
    let request = RedemptionRequest {
        transaction_hash: format!("0x{}", "11".repeat(32)),
        condition_id: format!("0x{}", "22".repeat(32)),
        trade_ids: vec![1, 2],
    };
    let account = address("0x1111111111111111111111111111111111111111").unwrap();
    let block = json!({"number":"0x64","hash":format!("0x{}","33".repeat(32)),"timestamp":format!("0x{:x}",Utc::now().timestamp()-10)});
    let token_ids = [U256::from(123), U256::from(456)];
    let numerators = nums.map(U256::from);
    let denominator = numerators[0] + numerators[1];
    let pay = [
        burned[0] * nums[0] / nums.iter().sum::<u64>(),
        burned[1] * nums[1] / nums.iter().sum::<u64>(),
    ];
    let total = pay.iter().sum::<u64>();
    let mut logs = Vec::new();
    let adapter = address(ADAPTER).unwrap();
    let ctf = address(CTF).unwrap();
    let pusd = address(PUSD).unwrap();
    let usdce = address(USDCE).unwrap();
    let mut log = |source: &str, sig: &str, topics: Vec<B256>, data: Vec<U256>| {
        let mut t = vec![json!(event(sig))];
        t.extend(topics.into_iter().map(|v| json!(format!("{v:#x}"))));
        let bytes: Vec<u8> = data
            .into_iter()
            .flat_map(|w| w.to_be_bytes::<32>())
            .collect();
        logs.push(json!({"address":source,"topics":t,"data":format!("0x{}",hex::encode(bytes)),
            "removed":false,"transactionHash":request.transaction_hash,"blockNumber":block["number"],
            "blockHash":block["hash"],"logIndex":format!("0x{:x}",logs.len()+1)}));
    };
    let a = |address: Address| B256::from(address.into_word());
    log(
        CTF,
        "TransferBatch(address,address,address,uint256[],uint256[])",
        vec![a(adapter), a(account), a(adapter)],
        vec![
            U256::from(64),
            U256::from(160),
            U256::from(2),
            token_ids[0],
            token_ids[1],
            U256::from(2),
            U256::from(burned[0]),
            U256::from(burned[1]),
        ],
    );
    for i in 0..2 {
        if burned[i] > 0 {
            log(
                CTF,
                "TransferSingle(address,address,address,uint256,uint256)",
                vec![a(adapter), a(adapter), a(Address::ZERO)],
                vec![token_ids[i], U256::from(burned[i])],
            );
        }
    }
    if total > 0 {
        log(
            USDCE,
            "Transfer(address,address,uint256)",
            vec![a(ctf), a(adapter)],
            vec![U256::from(total)],
        );
    }
    log(
        CTF,
        "PayoutRedemption(address,address,bytes32,bytes32,uint256[],uint256)",
        vec![a(adapter), a(usdce), B256::ZERO],
        vec![
            U256::from_be_bytes(hash(&request.condition_id).unwrap().0),
            U256::from(96),
            U256::from(total),
            U256::from(2),
            U256::from(1),
            U256::from(2),
        ],
    );
    log(
        USDCE,
        "Transfer(address,address,uint256)",
        vec![a(adapter), a(pusd)],
        vec![U256::from(total)],
    );
    log(
        PUSD,
        "Transfer(address,address,uint256)",
        vec![a(Address::ZERO), a(account)],
        vec![U256::from(total)],
    );
    log(
        PUSD,
        "Wrapped(address,address,address,uint256)",
        vec![a(adapter), a(usdce), a(account)],
        vec![U256::from(total)],
    );
    let receipt = json!({"transactionHash":request.transaction_hash,"transactionIndex":"0x5","blockNumber":block["number"],
        "blockHash":block["hash"],"status":"0x1","logs":logs});
    (
        receipt,
        block,
        request,
        account,
        token_ids,
        numerators,
        denominator,
    )
}

pub(crate) fn verified_fixture(burned: [u64; 2], nums: [u64; 2]) -> VerifiedRedemption {
    let (r, b, q, a, t, n, d) = fixture(burned, nums);
    let mut proof = decode_receipt(&r, &b, &q, a, t, n, d).unwrap();
    proof.evidence =
        json!({"receipt":r,"block":b,"fixture":"synthetic offline finalized-receipt evidence"});
    proof
}

#[test]
fn exact_winning_losing_and_fractional_redemptions_require_real_burn_and_payout() {
    for (burned, nums, payout) in [
        ([5_000_000, 3_000_000], [1, 0], 5_000_000),
        ([0, 3_000_000], [1, 0], 0),
        ([5_000_000, 3_000_000], [1, 1], 4_000_000),
    ] {
        let proof = verified_fixture(burned, nums);
        assert_eq!(proof.burned_units, burned);
        assert_eq!(proof.total_payout_units, payout);
    }
}

#[test]
fn tampered_or_ambiguous_receipt_is_refused() {
    for case in 0..13 {
        let (mut r, mut b, q, a, t, n, d) = fixture([5_000_000, 3_000_000], [1, 0]);
        match case {
            0 => r["status"] = json!("0x0"),
            1 => r["blockHash"] = json!(format!("0x{}", "ff".repeat(32))),
            2 => r["logs"][1]["removed"] = json!(true),
            3 => {
                r["logs"].as_array_mut().unwrap().remove(1);
            }
            4 => r["logs"][0]["topics"][2] = json!(format!("0x{}", "00".repeat(32))),
            5 => r["logs"][1]["logIndex"] = r["logs"][0]["logIndex"].clone(),
            6 => r["logs"][6]["data"] = json!(format!("0x{:064x}", 6_000_000_u64)),
            7 => r["logs"][4]["topics"][2] = json!(format!("0x{}", "00".repeat(32))),
            8 => r["logs"][0]["data"] = json!("0x"),
            9 => b["timestamp"] = json!(format!("0x{:x}", Utc::now().timestamp() + 400)),
            10 => r["logs"][2]["transactionHash"] = json!(format!("0x{}", "ff".repeat(32))),
            11 => {
                r["logs"][0]["topics"][0] = json!(event(
                    "PositionsMerge(address,address,bytes32,bytes32,uint256[],uint256)"
                ))
            }
            _ => {
                let transferred = r["logs"][0]["logIndex"].clone();
                r["logs"][0]["logIndex"] = r["logs"][1]["logIndex"].clone();
                r["logs"][1]["logIndex"] = transferred;
            }
        }
        assert!(
            decode_receipt(&r, &b, &q, a, t, n, d).is_err(),
            "tampering case{case}"
        );
    }
    let (mut r, b, q, a, t, n, d) = fixture([0, 3_000_000], [1, 0]);
    r["logs"].as_array_mut().unwrap().remove(1);
    assert!(
        decode_receipt(&r, &b, &q, a, t, n, d).is_err(),
        "zero payout does not waive burn proof"
    );
}

#[test]
fn request_cannot_select_duplicate_or_invalid_lots() {
    let (_, _, mut q, _, _, _, _) = fixture([0, 3_000_000], [1, 0]);
    for ids in [vec![], vec![1, 1], vec![0], vec![1; 65]] {
        q.trade_ids = ids;
        assert!(q.validate().is_err());
    }
}

use std::sync::{Arc, Mutex};

pub(crate) struct RpcState {
    pub calls: Vec<Value>,
    receipt: Value,
    block: Value,
    tokens: [U256; 2],
    pub mode: u8,
    pub delay_entry: bool,
    pub entry_receipt: Option<Value>,
}

pub(crate) async fn mock_rpc() -> (
    crate::clob::ClobClient,
    RedemptionRequest,
    Arc<Mutex<RpcState>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{
        extract::State,
        routing::{get, post},
        Json, Router,
    };
    let auth = crate::clob::ClobAuth::new(
        "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "public-fixture-key",
        "cHVibGljLWZpeHR1cmUtc2VjcmV0",
        "public-fixture-passphrase",
    )
    .unwrap();
    let account = address(&auth.address()).unwrap();
    let (mut receipt, block, request, original, tokens, _, _) =
        fixture([5_000_000, 3_000_000], [1, 0]);
    let old = format!("{:#x}", original.into_word());
    let new = format!("{:#x}", account.into_word());
    for log in receipt["logs"].as_array_mut().unwrap() {
        for topic in log["topics"].as_array_mut().unwrap() {
            if topic.as_str() == Some(&old) {
                *topic = json!(new);
            }
        }
    }
    let state = Arc::new(Mutex::new(RpcState {
        calls: vec![],
        receipt,
        block,
        tokens,
        mode: 0,
        delay_entry: false,
        entry_receipt: None,
    }));
    async fn rpc(
        State(state): State<Arc<Mutex<RpcState>>>,
        Json(input): Json<Value>,
    ) -> Json<Value> {
        let (result, delay) = {
            let mut st = state.lock().unwrap();
            st.calls.push(input.clone());
            let method = input["method"].as_str().unwrap();
            let entry = method == "eth_getTransactionReceipt"
                && input["params"][0] != st.receipt["transactionHash"];
            let result = match method {
                "eth_chainId" => json!(if st.mode == 1 { "0x1" } else { "0x89" }),
                "eth_getTransactionReceipt" => {
                    let mut receipt = if entry {
                        st.entry_receipt
                            .clone()
                            .unwrap_or_else(|| st.receipt.clone())
                    } else {
                        st.receipt.clone()
                    };
                    if entry {
                        receipt["transactionHash"] = input["params"][0].clone();
                        receipt["transactionIndex"] =
                            json!(if st.mode == 5 { "0x6" } else { "0x4" });
                        if st.mode == 6 {
                            receipt["blockNumber"] = json!("0x65");
                        }
                        if st.mode == 7 {
                            receipt["blockHash"] = json!(format!("0x{}", "ff".repeat(32)));
                        }
                    }
                    receipt
                }
                "eth_getBlockByNumber" => {
                    let mut block = st.block.clone();
                    if input["params"][0] == "finalized" && st.mode == 2 {
                        block["number"] = json!("0x63");
                    }
                    if input["params"][0] != "finalized" && st.mode == 3 {
                        block["hash"] = json!(format!("0x{}", "ff".repeat(32)));
                    }
                    block
                }
                "eth_call" => {
                    assert_eq!(
                        input["params"][1],
                        json!({"blockHash":st.block["hash"],"requireCanonical":true})
                    );
                    let data = input["params"][0]["data"].as_str().unwrap();
                    let is = |sig: &str| data.starts_with(&call_data(sig, &[]));
                    let v = if is("CONDITIONAL_TOKENS()") {
                        if st.mode == 4 {
                            U256::ZERO
                        } else {
                            address_word(CTF).unwrap()
                        }
                    } else if is("COLLATERAL_TOKEN()") {
                        address_word(PUSD).unwrap()
                    } else if is("USDCE()") {
                        address_word(USDCE).unwrap()
                    } else if is("getOutcomeSlotCount(bytes32)") {
                        U256::from(2)
                    } else if is("payoutDenominator(bytes32)") {
                        U256::from(1)
                    } else if is("payoutNumerators(bytes32,uint256)") {
                        if data.ends_with(&format!("{:064x}", 0)) {
                            U256::from(1)
                        } else {
                            U256::ZERO
                        }
                    } else if is("getCollectionId(bytes32,bytes32,uint256)") {
                        if data.ends_with(&format!("{:064x}", 1)) {
                            U256::from(100)
                        } else {
                            U256::from(200)
                        }
                    } else if is("getPositionId(address,bytes32)") {
                        if data.ends_with(&format!("{:064x}", 100)) {
                            st.tokens[0]
                        } else {
                            st.tokens[1]
                        }
                    } else {
                        panic!("unexpected call")
                    };
                    json!(format!("0x{v:064x}"))
                }
                other => panic!("no non-read chain method permitted: {other}"),
            };
            (result, entry && st.delay_entry)
        };
        if delay {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
        Json(json!({"jsonrpc":"2.0","id":1,"result":result}))
    }
    async fn balance() -> Json<Value> {
        Json(json!({"balance":"18000000","allowances":{"fixture":"100000000"}}))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let shared = state.clone();
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/rpc", post(rpc))
                .route("/balance-allowance", get(balance))
                .route("/balance-allowance/update", get(balance))
                .with_state(shared),
        )
        .await
        .unwrap()
    });
    let client = crate::clob::ClobClient::with_test_host(auth, url.clone())
        .with_settlement_rpc(&format!("{url}/rpc"))
        .unwrap();
    (client, request, state, task)
}

#[tokio::test]
async fn rpc_reads_are_block_hash_bound_and_never_submit_a_transaction() {
    let (client, request, state, task) = mock_rpc().await;
    let verifier = client.settlement_verifier().unwrap();
    let proof = verifier
        .verify_redemption(&request, &client.account_address())
        .await
        .unwrap();
    assert_eq!(proof.total_payout_units, 5_000_000);
    assert_eq!(
        state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|c| c["method"] == "eth_call")
            .count(),
        11
    );
    for mode in 1..=4 {
        state.lock().unwrap().mode = mode;
        assert!(
            verifier
                .verify_redemption(&request, &client.account_address())
                .await
                .is_err(),
            "mode{mode}"
        );
    }
    state.lock().unwrap().mode = 0;
    let entry = format!("0x{}", "44".repeat(32));
    verifier
        .verify_entry_before_redemption(&entry, &proof)
        .await
        .unwrap();
    for mode in 5..=7 {
        state.lock().unwrap().mode = mode;
        assert!(
            verifier
                .verify_entry_before_redemption(&entry, &proof)
                .await
                .is_err(),
            "entrymode{mode}"
        );
    }
    task.abort();
}
