//! Stateful offline venue tests for maker submission, settlement, and restart.

use super::*;
use crate::clob::{ClobAuth, ClobClient};

#[derive(Default)]
pub(crate) struct VenueState {
    pub(crate) order_id: String,
    pub(crate) side: String,
    pub(crate) size: f64,
    pub(crate) price: f64,
    pub(crate) matched_size: f64,
    pub(crate) status: String,
    pub(crate) matches: Vec<Value>,
    pub(crate) receipts: HashMap<String, Value>,
    pub(crate) identity: Option<crate::clob::settlement::SettlementOrderIdentity>,
    pub(crate) posts: usize,
    pub(crate) cancels: usize,
    pub(crate) cancel_failures_remaining: usize,
    pub(crate) maker_fees_charged: bool,
    pub(crate) wrong_token: bool,
    pub(crate) drop_post_reply: bool,
    pub(crate) reject_post: bool,
    pub(crate) nonfinalized: bool,
    pub(crate) taker_order: bool,
    pub(crate) expect_manual_stop_before_cancel: bool,
    pub(crate) delay_history_ms: u64,
    pub(crate) delay_balance_ms: u64,
    pub(crate) balance_reads: usize,
    pub(crate) tight_deep_book: bool,
}

#[derive(Clone)]
struct ServerState {
    state: Arc<std::sync::Mutex<VenueState>>,
    pool: SqlitePool,
}

pub(crate) struct MockMakerVenue {
    pub(crate) state: Arc<std::sync::Mutex<VenueState>>,
    pub(crate) account: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockMakerVenue {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockMakerVenue {
    pub(crate) fn fill(&self, id: &str, size: f64, price: f64, status: &str) {
        use alloy::primitives::{keccak256, Address, U256};
        let mut venue = self.state.lock().unwrap();
        venue.matched_size += size;
        let transaction = format!("0x{:064x}", venue.matches.len() + 1);
        let fill = json!({
            "id":id,"taker_order_id":if venue.taker_order {venue.order_id.clone()} else {"taker-order".into()},"market":"condition",
            "asset_id":"123456789","side":if venue.taker_order {venue.side.clone()} else if venue.side=="BUY" { "SELL".into() } else { "BUY".into() },
            "size":size.to_string(),"price":price.to_string(),"status":status,
            "match_time":"1791633660","transaction_hash":transaction,"trader_side":if venue.taker_order {"TAKER"} else {"MAKER"},"maker_address":self.account,
            "fee_rate_bps":"1000","maker_orders":if venue.taker_order {json!([])} else {json!([{
                "order_id":venue.order_id,"maker_address":self.account,"asset_id":"123456789",
                "side":venue.side,"matched_amount":size.to_string(),"price":price.to_string(),
                "fee_rate_bps":"1000"
            }])}
        });
        let identity = venue.identity.as_ref().unwrap();
        let shares = U256::from((size * 1_000_000.0).round() as u64);
        let collateral = U256::from((size * price * 1_000_000.0).round() as u64);
        let buy = venue.side == "BUY";
        let values = [
            U256::from(if buy { 0 } else { 1 }),
            U256::from(123456789),
            if buy { collateral } else { shares },
            if buy { shares } else { collateral },
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
        ];
        let data: Vec<u8> = values
            .iter()
            .flat_map(|value| value.to_be_bytes::<32>())
            .collect();
        let mut maker = [0u8; 32];
        maker[12..].copy_from_slice(self.account.parse::<Address>().unwrap().as_slice());
        let block_hash = format!("0x{}", "33".repeat(32));
        let receipt = json!({"transactionHash":transaction,"status":"0x1","blockNumber":"0x123","blockHash":block_hash,
            "logs":[{"address":identity.exchange_address,"transactionHash":transaction,"blockNumber":"0x123",
                "blockHash":block_hash,"logIndex":"0x0","removed":false,
                "topics":[format!("{:#x}",keccak256("OrderFilled(bytes32,address,address,uint8,uint256,uint256,uint256,uint256,bytes32,bytes32)")),
                    venue.order_id, format!("0x{}",hex::encode(maker)),format!("0x{}","00".repeat(32))],
                "data":format!("0x{}",hex::encode(data))}]});
        venue.receipts.insert(transaction, receipt);
        venue.matches.push(fill);
    }

    pub(crate) fn confirm(&self, id: &str) {
        let mut venue = self.state.lock().unwrap();
        venue
            .matches
            .iter_mut()
            .find(|fill| fill["id"] == id)
            .unwrap()["status"] = json!("CONFIRMED");
    }
}

async fn server(pool: SqlitePool) -> (ClobClient, MockMakerVenue) {
    use axum::{
        body::to_bytes,
        extract::{Request, State},
        http::StatusCode,
        response::IntoResponse,
        Json, Router,
    };
    async fn respond(
        State(server): State<ServerState>,
        request: Request,
    ) -> axum::response::Response {
        use base64::{engine::general_purpose::URL_SAFE, Engine};
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        let route = request.uri().path().to_string();
        let method = request.method().to_string();
        let headers = request.headers().clone();
        let body = to_bytes(request.into_body(), 16384).await.unwrap();
        let json_body: Value = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        if !matches!(
            route.as_str(),
            "/version" | "/book" | "/clob-markets/condition" | "/rpc"
        ) {
            let timestamp = headers.get("POLY_TIMESTAMP").unwrap().to_str().unwrap();
            let mut hmac = Hmac::<Sha256>::new_from_slice(b"public-fixture-secret").unwrap();
            hmac.update(format!("{timestamp}{method}{route}").as_bytes());
            hmac.update(&body);
            assert_eq!(
                headers.get("POLY_SIGNATURE").unwrap().to_str().unwrap(),
                URL_SAFE.encode(hmac.finalize().into_bytes())
            );
            assert_eq!(headers.get("POLY_API_KEY").unwrap(), "public-api-key");
        }
        if method == "POST" && route == "/order" {
            // The server checks that the process committed the intent before
            // sending the body. A crash after this point remains recoverable.
            let taker_order = json_body["orderType"] == "FOK";
            let intent: db::maker::MakerOrder = if taker_order {
                let live = db::live::pending_orders(&server.pool)
                    .await
                    .unwrap()
                    .pop()
                    .unwrap();
                let closing = db::load_trade_by_id(&server.pool, live.trade_id)
                    .await
                    .unwrap()
                    .unwrap();
                if live.phase == "exit" {
                    assert_eq!(closing.status, "CLOSING");
                } else {
                    assert_eq!(live.phase, "entry");
                    assert_eq!(closing.status, "OPEN");
                    assert_eq!(
                        closing.shares, 0.0,
                        "submission cannot invent entry inventory"
                    );
                }
                db::maker::MakerOrder {
                    order_id: live.order_id,
                    market_id: closing.market_id,
                    token_id: live.token_id,
                    side: if live.phase == "exit" { "SELL" } else { "BUY" }.into(),
                    price: live.price,
                    size: live.size,
                    confirmed_size: 0.0,
                    state: "SUBMITTING".into(),
                    template_json: String::new(),
                    settlement_json: serde_json::to_string(&live.identity).unwrap(),
                    created_at: String::new(),
                    updated_at: String::new(),
                }
            } else {
                sqlx::query_as("SELECT * FROM maker_orders WHERE state='SUBMITTING' ORDER BY created_at DESC LIMIT 1")
                    .fetch_one(&server.pool).await.unwrap()
            };
            assert_eq!(json_body["owner"], "public-api-key");
            assert_eq!(json_body["postOnly"], !taker_order);
            assert_eq!(
                json_body["orderType"],
                if taker_order { "FOK" } else { "GTC" }
            );
            assert_eq!(json_body["order"]["side"], intent.side);
            let mut state = server.state.lock().unwrap();
            state.order_id = intent.order_id.clone();
            state.side = intent.side;
            state.size = intent.size;
            state.price = intent.price;
            state.status = "LIVE".into();
            state.identity = Some(serde_json::from_str(&intent.settlement_json).unwrap());
            state.posts += 1;
            state.taker_order = taker_order;
            if state.reject_post {
                state.status = "REJECTED".into();
                return Json(json!({"success":false,"errorMsg":"post-only quote would cross"}))
                    .into_response();
            }
            if state.drop_post_reply {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"error":"accepted before transport failure"})),
                )
                    .into_response();
            }
            return Json(json!({"success":true,"orderID":intent.order_id,"status":"live","makingAmount":"","takingAmount":""})).into_response();
        }
        if method == "DELETE" && route == "/order" {
            {
                let mut state = server.state.lock().unwrap();
                state.cancels += 1;
                if state.cancel_failures_remaining > 0 {
                    state.cancel_failures_remaining -= 1;
                    return (
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error":"fixture cancellation temporarily unavailable"})),
                    )
                        .into_response();
                }
            }
            let expect_stop = server
                .state
                .lock()
                .unwrap()
                .expect_manual_stop_before_cancel;
            if expect_stop {
                assert!(
                    db::maker::market_stood_down(&server.pool, "exit-market")
                        .await
                        .unwrap(),
                    "manual stop must be committed before cancelling the venue order"
                );
            }
        }
        if method == "GET" && route == "/balance-allowance" {
            let delay = {
                let mut state = server.state.lock().unwrap();
                state.balance_reads += 1;
                state.delay_balance_ms
            };
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        if method == "GET" && route == "/data/trades" {
            let delay = server.state.lock().unwrap().delay_history_ms;
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }
        let mut state = server.state.lock().unwrap();
        if method == "POST" && route == "/rpc" {
            let result = match json_body["method"].as_str().unwrap() {
                "eth_chainId" => json!("0x89"),
                "eth_getBlockByNumber" => {
                    json!({"number":if state.nonfinalized && json_body["params"][0] == "finalized" {"0x122"} else {"0x123"},"hash":format!("0x{}","33".repeat(32))})
                }
                "eth_getTransactionReceipt" => state
                    .receipts
                    .get(json_body["params"][0].as_str().unwrap())
                    .cloned()
                    .unwrap_or(Value::Null),
                _ => panic!("unexpected settlement RPC method"),
            };
            return Json(json!({"jsonrpc":"2.0","id":1,"result":result})).into_response();
        }
        let body = match (method.as_str(), route.as_str()) {
            ("GET", "/version") => json!({"version":2}),
            ("GET", "/book") => {
                json!({"market":"condition","tick_size":"0.01","min_order_size":"5","neg_risk":false,
                "bids":[{"price":"0.49","size":if state.tight_deep_book {"10000"} else {"20"}}],
                "asks":[{"price":if state.tight_deep_book {"0.50"} else {"0.61"},"size":if state.tight_deep_book {"10000"} else {"20"}}]})
            }
            ("GET", "/clob-markets/condition") => {
                json!({"t":[{"t":"123456789"}],"fd":{"r":0.07,"e":1,"to":!state.maker_fees_charged}})
            }
            ("GET", "/data/trades") => json!({"data":state.matches,"next_cursor":"LTE="}),
            ("GET", "/balance-allowance/update") => json!({}),
            ("GET", "/balance-allowance") => json!({"balance":"100000000","allowances":{}}),
            ("GET", path) if path.starts_with("/data/order/") => {
                assert_eq!(path.trim_start_matches("/data/order/"), state.order_id);
                json!({"id":state.order_id,"asset_id":if state.wrong_token { "unexpected-token" } else { "123456789" },
                    "side":state.side,"original_size":state.size.to_string(),"size_matched":state.matched_size.to_string(),
                    "price":state.price.to_string(),"status":state.status,
                    "associate_trades":state.matches.iter().map(|fill|fill["id"].clone()).collect::<Vec<_>>()})
            }
            ("DELETE", "/order") => {
                assert_eq!(json_body["orderID"], state.order_id);
                state.status = "CANCELED".into();
                json!({"canceled":[state.order_id]})
            }
            _ => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error":"unexpected request"})),
                )
                    .into_response()
            }
        };
        Json(body).into_response()
    }
    let auth = ClobAuth::new(
        "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "public-api-key",
        "cHVibGljLWZpeHR1cmUtc2VjcmV0",
        "public-passphrase",
    )
    .unwrap();
    let account = auth.address().to_ascii_lowercase();
    let state = Arc::new(std::sync::Mutex::new(VenueState::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let host = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().fallback(respond).with_state(ServerState {
        state: state.clone(),
        pool,
    });
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let rpc_url = format!("{host}/rpc");
    (
        ClobClient::with_test_host(auth, host)
            .with_settlement_rpc(&rpc_url)
            .unwrap(),
        MockMakerVenue {
            state,
            account,
            task,
        },
    )
}

pub(crate) async fn fixture() -> (TradingBot, Trade, std::path::PathBuf, MockMakerVenue) {
    let (mut bot, mut trade, path) = crate::bot::close::tests::maker_exit_fixture(5.0).await;
    trade.mode = "live".into();
    trade.token_id = Some("123456789".into());
    trade.entry_price = 0.4;
    trade.amount = 2.0;
    trade.order_id = None;
    trade.accounting_version = 1;
    db::update_trade(&bot.db, &trade).await.unwrap();
    sqlx::query("UPDATE trades SET entry_cost=2.0,entry_shares=5.0 WHERE id=?")
        .bind(trade.id)
        .execute(&bot.db)
        .await
        .unwrap();
    *bot.portfolio_state.trades.write().await = vec![trade.clone()];
    let (client, venue) = server(bot.db.clone()).await;
    bot.clob = Some(Arc::new(client));
    {
        let mut cfg = bot.control_plane.config.write().await;
        cfg.mode = "live".into();
        cfg.maker_live_quoting = true;
        cfg.shadow_trading = false;
    }
    (bot, trade, path, venue)
}

#[tokio::test]
async fn maker_sale_posts_after_intent_and_waits_for_confirmation_before_cancel_replacement() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    let mut actor = MakerActorState::default();
    actor.upsert_quote(
        order.clone(),
        "exit-market".into(),
        "123456789".into(),
        QuoteSide::Ask,
        0.6,
        5.0,
    );
    venue.fill("first", 2.0, 0.61, "MATCHED");
    reconcile_maker_orders(&bot, &mut actor).await.unwrap();
    assert!(!actor.live_accounting_ready);
    assert_eq!(
        db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        5.0
    );
    assert!(!cancel_quote(&bot, &order, "exit-market", "123456789", "replace", &outbox).await);
    assert_eq!(db::maker::pending_orders(&bot.db).await.unwrap().len(), 1);
    venue.confirm("first");
    reconcile_maker_orders(&bot, &mut actor).await.unwrap();
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(!actor.quotes.contains_key(&order));
    let remaining = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remaining.shares, 3.0);
    assert!((remaining.pnl.unwrap() - 0.42).abs() < 1e-9);
    reconcile_maker_orders(&bot, &mut actor).await.unwrap();
    assert_eq!(
        db::load_trade_fills(&bot.db, original.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn manual_close_stands_down_before_waiting_for_an_unsettled_maker_match() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    let bot = Arc::new(bot);
    let mut actor = MakerActorState::default();
    actor.upsert_quote(
        order,
        "exit-market".into(),
        "123456789".into(),
        QuoteSide::Ask,
        0.6,
        5.0,
    );
    venue.fill("still-settling", 2.0, 0.61, "MATCHED");
    venue.state.lock().unwrap().expect_manual_stop_before_cancel = true;
    let (resp, rx) = oneshot::channel();
    process_execution_command(
        ExecutionCommand::CloseTrade {
            trade_id: original.id,
            exit_price: None,
            intent: CloseIntent::Manual,
            resp,
        },
        &bot,
        &mut actor,
        &outbox,
    )
    .await;
    let unchanged = rx.await.unwrap().unwrap();
    assert!(actor.hands_off_markets.contains("exit-market"));
    assert_eq!(unchanged.shares, 5.0);
    assert!(unchanged.is_open());
    assert!(bot.kill_switch_active());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(db::maker::pending_orders(&bot.db).await.unwrap().len(), 1);
    let mut restarted = MakerActorState::default();
    restore_maker_controls(&bot, &mut restarted).await.unwrap();
    restarted.retain_l2_tokens(&HashSet::new());
    restarted.clear_market("exit-market");
    assert!(restarted.hands_off_markets.contains("exit-market"));
    assert!(place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        2,
        &outbox
    )
    .await
    .is_none());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn lost_maker_post_response_recovers_identity_without_reposting() {
    let (bot, _, path, venue) = fixture().await;
    venue.state.lock().unwrap().drop_post_reply = true;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    assert!(place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox
    )
    .await
    .is_none());
    assert!(bot.kill_switch_active());
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "UNKNOWN"
    );
    let mut restored = MakerActorState::default();
    reconcile_maker_orders(&bot, &mut restored).await.unwrap();
    assert_eq!(restored.quotes.len(), 1);
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "LIVE"
    );
    reconcile_maker_orders(&bot, &mut restored).await.unwrap();
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn explicit_maker_rejection_does_not_leave_an_unknown_intent_or_halt() {
    let (bot, original, path, venue) = fixture().await;
    venue.state.lock().unwrap().reject_post = true;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    assert!(place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox
    )
    .await
    .is_none());
    assert!(!bot.kill_switch_active());
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert_eq!(
        db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        5.0
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn maker_fee_and_order_identity_mismatches_block_without_invented_fills() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    venue.state.lock().unwrap().maker_fees_charged = true;
    assert!(place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox
    )
    .await
    .is_none());
    assert_eq!(venue.state.lock().unwrap().posts, 0);
    venue.state.lock().unwrap().maker_fees_charged = false;
    place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        2,
        &outbox,
    )
    .await
    .unwrap();
    venue.state.lock().unwrap().wrong_token = true;
    let mut actor = MakerActorState::default();
    assert!(reconcile_maker_orders(&bot, &mut actor).await.is_err());
    assert!(!actor.live_accounting_ready);
    assert_eq!(
        db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        5.0
    );
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn maker_confirmation_requires_finalized_chain_evidence_and_recovers_later() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    venue.fill("finality", 2.0, 0.61, "CONFIRMED");
    venue.state.lock().unwrap().nonfinalized = true;
    let mut state = MakerActorState::default();
    assert!(reconcile_maker_orders(&bot, &mut state).await.is_err());
    assert_eq!(
        db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        5.0
    );
    assert!(db::load_trade_fills(&bot.db, original.id, 10)
        .await
        .unwrap()
        .is_empty());
    venue.state.lock().unwrap().nonfinalized = false;
    reconcile_maker_orders(&bot, &mut state).await.unwrap();
    assert_eq!(
        db::load_trade_by_id(&bot.db, original.id)
            .await
            .unwrap()
            .unwrap()
            .shares,
        3.0
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn maker_buy_partial_matches_restore_remaining_quote_and_actual_cost_after_restart() {
    let (bot, mut template, path, venue) = fixture().await;
    let clob = bot.clob.as_ref().unwrap();
    let signed = clob
        .create_order(&LimitOrderArgs {
            token_id: "123456789".into(),
            price: 0.5,
            size: 5.0,
            side: Side::Buy,
        })
        .await
        .unwrap();
    let now = Utc::now().to_rfc3339();
    template.reason = "confirmed maker entry".into();
    db::maker::register_order(
        &bot.db,
        &db::maker::MakerOrder {
            order_id: signed.order_id().into(),
            market_id: "exit-market".into(),
            token_id: "123456789".into(),
            side: "BUY".into(),
            price: 0.5,
            size: 5.0,
            confirmed_size: 0.0,
            state: "SUBMITTING".into(),
            template_json: serde_json::to_string(&template).unwrap(),
            settlement_json: serde_json::to_string(
                &crate::clob::settlement::SettlementOrderIdentity::from(&signed),
            )
            .unwrap(),
            created_at: now.clone(),
            updated_at: now,
        },
    )
    .await
    .unwrap();
    clob.post_order_with_options(&signed, OrderType::GTC, true)
        .await
        .unwrap();
    db::maker::update_state(&bot.db, signed.order_id(), "LIVE")
        .await
        .unwrap();
    venue.fill("buy-first", 2.0, 0.49, "CONFIRMED");
    let mut before_restart = MakerActorState::default();
    reconcile_maker_orders(&bot, &mut before_restart)
        .await
        .unwrap();
    let active = before_restart
        .quote_for("exit-market", "123456789", &QuoteSide::Bid)
        .unwrap();
    assert_eq!(active.size, 3.0);
    // Restore an actual verified backup into a new process state. The mock
    // venue retains the original order; replay must account each receipt once.
    db::maker::persist_manual_stand_down(&bot.db, "manual-stop")
        .await
        .unwrap();
    let backup = format!(
        "artifacts/backups/maker-restore-{}.sqlite",
        path.file_stem().unwrap().to_string_lossy()
    );
    db::backup_database(&bot.db, &backup).await.unwrap();
    let restored_path = path.with_extension("restored.sqlite");
    std::fs::copy(&backup, &restored_path).unwrap();
    std::fs::remove_file(&backup).unwrap();
    let mut cfg = bot.control_plane.config.read().await.clone();
    cfg.mode = "simulation".into();
    let restored_pool = db::open(restored_path.to_str().unwrap()).await.unwrap();
    let mut recovered = TradingBot::new(cfg, restored_pool).await.unwrap();
    recovered.clob = bot.clob.clone();
    recovered.control_plane.config.write().await.mode = "live".into();
    recovered
        .freeze_entries("restore_drill", "verify before resuming")
        .await;
    let mut after_restart = MakerActorState::default();
    restore_maker_controls(&recovered, &mut after_restart)
        .await
        .unwrap();
    assert!(after_restart.hands_off_markets.contains("manual-stop"));
    reconcile_maker_orders(&recovered, &mut after_restart)
        .await
        .unwrap();
    assert_eq!(after_restart.quotes.len(), 1);
    assert_eq!(db::load_all_trades(&recovered.db).await.unwrap().len(), 2);
    venue.fill("buy-last", 3.0, 0.5, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    reconcile_maker_orders(&recovered, &mut after_restart)
        .await
        .unwrap();
    assert!(after_restart.quotes.is_empty());
    assert!(db::maker::pending_orders(&recovered.db)
        .await
        .unwrap()
        .is_empty());
    let acquired = db::load_all_trades(&recovered.db)
        .await
        .unwrap()
        .into_iter()
        .find(|trade| trade.id != template.id)
        .unwrap();
    assert_eq!(acquired.shares, 5.0);
    assert!((acquired.entry_price - 0.496).abs() < 1e-9);
    assert!(
        (db::trade_entry_cost(&recovered.db, acquired.id)
            .await
            .unwrap()
            .unwrap()
            - 2.48)
            .abs()
            < 1e-9
    );
    assert_eq!(
        db::load_trade_fills(&recovered.db, acquired.id, 10)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert!(recovered.kill_switch_active());
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "restore must not resubmit the order"
    );
    let received = db::maker::recorded_matches(&recovered.db, signed.order_id())
        .await
        .unwrap();
    assert_eq!(received.len(), 2, "two receipt events, each recorded once");
    crate::bot::close::tests::cleanup_exit_fixture(&recovered, &restored_path).await;
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn lost_fok_close_response_recovers_finalized_sale_without_selling_twice() {
    let (bot, original, path, venue) = fixture().await;
    venue.state.lock().unwrap().drop_post_reply = true;
    assert!(bot.close_confirmed_live_position(&original).await.is_none());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    let closing = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(closing.status, "CLOSING");
    assert_eq!(closing.shares, 5.0);
    assert_eq!(db::live::pending_orders(&bot.db).await.unwrap().len(), 1);
    let durable_order = db::live::pending_orders(&bot.db).await.unwrap().remove(0);
    assert!(bot.close_confirmed_live_position(&original).await.is_none());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    venue.fill("actual-fok-sale", 5.0, 0.55, "CONFIRMED");
    venue.state.lock().unwrap().status = "MATCHED".into();
    let recovered = bot.close_confirmed_live_position(&original).await.unwrap();
    assert_eq!(recovered.status, "CLOSED");
    assert_eq!(recovered.exit_price, Some(0.55));
    assert!((recovered.pnl.unwrap() - 0.75).abs() < 1e-9);
    assert_eq!(recovered.accounting_version, 1);
    assert!(db::live::pending_orders(&bot.db).await.unwrap().is_empty());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert_eq!(
        db::load_trade_fills(&bot.db, original.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let duplicated = bot.close_confirmed_live_position(&original).await.unwrap();
    assert_eq!(duplicated.pnl, recovered.pnl);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    let clob = bot.clob.as_ref().unwrap();
    let info = clob.get_order(&durable_order.order_id).await.unwrap();
    let history = clob.list_trades().await.unwrap();
    let transaction = history[0].transaction_hash.as_deref().unwrap();
    let settlements = clob
        .settlement_verifier()
        .unwrap()
        .verify(transaction, &durable_order.identity)
        .await
        .unwrap();
    let cash = bot.balance().await;
    let unchanged =
        db::live::reconcile_order(&bot.db, &durable_order, &info, &history, &settlements)
            .await
            .unwrap();
    assert_eq!(unchanged.shares, recovered.shares);
    assert_eq!(unchanged.pnl, recovered.pnl);
    assert_eq!(bot.balance().await, cash);
    for changed_fee in [true, false] {
        let mut changed = settlements.clone();
        if changed_fee {
            changed[0].fee_collateral += 0.01;
        } else {
            changed[0].gross_collateral += 0.01;
        }
        assert!(
            db::live::reconcile_order(&bot.db, &durable_order, &info, &history, &changed)
                .await
                .is_err()
        );
    }
    let mut extra = settlements.clone();
    let mut new_receipt = settlements[0].clone();
    new_receipt.log_index += 1;
    extra.push(new_receipt);
    assert!(
        db::live::reconcile_order(&bot.db, &durable_order, &info, &history, &extra)
            .await
            .is_err()
    );
    let persisted = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.pnl, recovered.pnl);
    assert_eq!(persisted.shares, recovered.shares);
    assert_eq!(bot.balance().await, cash);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn late_terminal_maker_match_halts_without_inventing_inventory_and_survives_restart() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    assert!(cancel_quote(&bot, &order, "exit-market", "123456789", "replace", &outbox).await);
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    venue.fill("appeared-after-cancellation", 2.0, 0.61, "MATCHED");
    let mut actor = MakerActorState::default();
    maintain_live_maker_state(&bot, &mut actor).await;
    assert!(bot.kill_switch_active());
    assert!(bot.degraded_no_trade_active());
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "AUDIT_UNCERTAIN"
    );
    let unchanged = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.shares, 5.0);
    assert!(db::load_trade_fills(&bot.db, original.id, 10)
        .await
        .unwrap()
        .is_empty());
    venue.confirm("appeared-after-cancellation");
    let mut restarted = MakerActorState::default();
    assert!(
        reconcile_maker_orders(&bot, &mut restarted).await.is_err(),
        "a restart cannot silently resolve evidence requiring operator review"
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn terminal_maker_audit_accepts_unchanged_history_then_detects_a_changed_receipt_fee() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    venue.fill("settled-sale", 2.0, 0.61, "CONFIRMED");
    assert!(cancel_quote(&bot, &order, "exit-market", "123456789", "replace", &outbox).await);
    let before = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    let mut actor = MakerActorState::default();
    audit_terminal_maker_orders(&bot, &mut actor).await.unwrap();
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    {
        let mut state = venue.state.lock().unwrap();
        let receipt = state.receipts.values_mut().next().unwrap();
        let mut data = receipt["logs"][0]["data"].as_str().unwrap().to_string();
        data.replace_range(2 + 4 * 64..2 + 5 * 64, &format!("{:064x}", 1000));
        receipt["logs"][0]["data"] = json!(data);
    }
    actor.last_terminal_audit = None;
    maintain_live_maker_state(&bot, &mut actor).await;
    assert!(bot.kill_switch_active());
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "AUDIT_UNCERTAIN"
    );
    assert!(
        recheck_terminal_maker_order(&bot, &order).await.is_err(),
        "operator recheck cannot waive a changed finalized fee"
    );
    let after = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.shares, before.shares);
    assert_eq!(after.pnl, before.pnl);
    assert_eq!(
        db::load_trade_fills(&bot.db, original.id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn operator_recheck_requires_original_evidence_and_retains_stops() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let id = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    assert!(cancel_quote(&bot, &id, "exit-market", "123456789", "operator", &outbox).await);
    db::maker::mark_audit_uncertain(&bot.db, &id).await.unwrap();
    assert!(
        recheck_terminal_maker_order(&bot, &id).await.is_err(),
        "operator must freeze entries"
    );
    bot.freeze_entries("operator_recovery", "reviewing original evidence")
        .await;
    bot.set_degraded_no_trade(true);
    recheck_terminal_maker_order(&bot, &id).await.unwrap();
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(bot.kill_switch_active());
    assert!(bot.degraded_no_trade_active());
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM order_outbox WHERE order_id=? AND stage='maker_terminal_recheck'",
    )
    .bind(&id)
    .fetch_one(&bot.db)
    .await
    .unwrap();
    assert_eq!(audits, 1);
    assert!(
        recheck_terminal_maker_order(&bot, &id).await.is_err(),
        "repeat request cannot rewrite terminal state"
    );
    venue.fill("late-evidence", 2.0, 0.61, "CONFIRMED");
    db::maker::mark_audit_uncertain(&bot.db, &id).await.unwrap();
    assert!(
        recheck_terminal_maker_order(&bot, &id).await.is_err(),
        "changed history must not be waived"
    );
    assert_eq!(
        db::maker::pending_orders(&bot.db).await.unwrap()[0].state,
        "AUDIT_UNCERTAIN"
    );
    let after = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.shares, original.shares);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn maker_millisecond_timestamp_cannot_hide_a_realized_loss_in_a_future_day() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    venue.fill("bad-millisecond-time", 2.0, 0.61, "CONFIRMED");
    venue.state.lock().unwrap().matches[0]["match_time"] =
        json!(Utc::now().timestamp_millis().to_string());
    let mut actor = MakerActorState::default();
    maintain_live_maker_state(&bot, &mut actor).await;
    assert!(bot.kill_switch_active());
    assert!(bot.degraded_no_trade_active());
    assert!(db::maker::recorded_matches(&bot.db, &order)
        .await
        .unwrap()
        .is_empty());
    let unchanged = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.shares, original.shares);
    assert_eq!(unchanged.pnl, original.pnl);
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn maker_transaction_day_is_complete_and_independent_of_history_page_order() {
    let (bot, _, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    venue.fill("same-tx-one", 1.0, 0.61, "CONFIRMED");
    venue.fill("same-tx-two", 1.0, 0.61, "CONFIRMED");
    let mut history = bot.clob.as_ref().unwrap().list_trades().await.unwrap();
    let transaction = history[0].transaction_hash.clone().unwrap();
    history[1].transaction_hash = Some(transaction.clone());
    history[0].match_time = Some("2026-10-10T12:01:00+00:00".into());
    history[1].match_time = Some("2026-10-10T12:00:00+00:00".into());
    let now = chrono::DateTime::parse_from_rfc3339("2026-10-11T12:00:00+00:00")
        .unwrap()
        .with_timezone(&Utc);
    let first = confirmed_maker_transaction_time(&history, &order, &transaction, now).unwrap();
    history.reverse();
    assert_eq!(
        confirmed_maker_transaction_time(&history, &order, &transaction, now).unwrap(),
        first
    );
    assert_eq!(first, "2026-10-10T12:00:00+00:00");
    history[1].match_time = Some("2026-10-09T23:59:59+00:00".into());
    assert!(confirmed_maker_transaction_time(&history, &order, &transaction, now).is_err());
    history[1].match_time = Some("2026-10-11T12:05:01+00:00".into());
    assert!(confirmed_maker_transaction_time(&history, &order, &transaction, now).is_err());
    history[1].match_time = None;
    assert!(confirmed_maker_transaction_time(&history, &order, &transaction, now).is_err());
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn hybrid_handoff_refuses_unconfirmed_maker_activity_without_a_taker_post() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    let mut state = MakerActorState::default();
    state.upsert_quote(
        order.clone(),
        "exit-market".into(),
        "123456789".into(),
        QuoteSide::Ask,
        0.6,
        5.0,
    );
    venue.fill("matched-during-handoff", 2.0, 0.61, "MATCHED");
    bot.control_plane.config.write().await.execution_mode = "hybrid".into();
    assert!(prepare_live_hybrid_handoff(&bot, &mut state, &outbox)
        .await
        .is_err());
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    assert!(!db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    let unchanged = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.shares, original.shares);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn hybrid_handoff_settles_maker_receipts_before_releasing_inventory_and_cash() {
    let (bot, original, path, venue) = fixture().await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    let mut state = MakerActorState::default();
    state.upsert_quote(
        order.clone(),
        "exit-market".into(),
        "123456789".into(),
        QuoteSide::Ask,
        0.6,
        5.0,
    );
    venue.fill("confirmed-during-handoff", 2.0, 0.61, "CONFIRMED");
    bot.control_plane.config.write().await.execution_mode = "hybrid".into();
    prepare_live_hybrid_handoff(&bot, &mut state, &outbox)
        .await
        .unwrap();
    assert!(state.quotes.is_empty());
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    assert!(db::live::pending_orders(&bot.db).await.unwrap().is_empty());
    let remaining = db::load_trade_by_id(&bot.db, original.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(remaining.shares, 3.0);
    assert!((remaining.pnl.unwrap() - 0.42).abs() < 1e-9);
    assert_eq!(
        bot.balance().await,
        100.0,
        "authenticated balance is observed after settled inventory publication"
    );
    assert_eq!(venue.state.lock().unwrap().posts, 1);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn hybrid_actor_handoff_is_required_before_the_single_durable_taker_post() {
    let (bot, _, path, venue) = fixture().await;
    bot.set_l2_feed_stale(false).await;
    let cfg = bot.control_plane.config.read().await.clone();
    let (outbox, _events) = mpsc::channel(32);
    let order = place_quote(
        &bot,
        "exit-market",
        "123456789",
        &QuoteSide::Ask,
        0.6,
        5.0,
        &cfg,
        Some(true),
        1,
        &outbox,
    )
    .await
    .unwrap();
    let bot = Arc::new(bot);
    let mut state = MakerActorState::default();
    state.upsert_quote(
        order,
        "exit-market".into(),
        "123456789".into(),
        QuoteSide::Ask,
        0.6,
        5.0,
    );
    {
        let mut cfg = bot.control_plane.config.write().await;
        cfg.execution_mode = "hybrid".into();
        cfg.min_trade_usd = 1.0;
        cfg.trade_amount = 5.0;
        cfg.max_open_positions = 5;
        cfg.macro_event_guard_enabled = false;
    }
    {
        let mut v = venue.state.lock().unwrap();
        v.tight_deep_book = true;
        v.drop_post_reply = true;
    }
    bot.market_state
        .up_token_cache
        .insert("entry-market".into(), (Instant::now(), "123456789".into()));
    let analysis = ParsedAnalysis::from_value(&json!({"signal":"BUY_YES","forced":true,
        "confidence":0.99,"raw_confidence":0.99,"p_up":0.99,"net_ev":0.9,"regime":"BULL",
        "market_data":{"market_id":"entry-market","price":0.495,"liquidity":100000,
            "question":"Offline hybrid admission","end_date":(Utc::now()+chrono::Duration::minutes(30)).to_rfc3339()}}));
    assert_eq!(analysis.market.market_id, "entry-market");
    assert!(bot
        .execute_trade_strategy(&analysis, false, Some(5.0), "conviction")
        .await
        .is_none());
    assert_eq!(
        venue.state.lock().unwrap().posts,
        1,
        "public direct hybrid execution must remain refused"
    );
    let (resp, receiver) = oneshot::channel();
    process_execution_command(
        ExecutionCommand::ExecuteTrade {
            origin: ExecutionOrigin::Manual,
            analysis: Box::new(analysis),
            hail_mary: false,
            override_amount: Some(5.0),
            strategy: "conviction".into(),
            resp,
        },
        &bot,
        &mut state,
        &outbox,
    )
    .await;
    let _ = receiver.await.unwrap();
    let rejections: Vec<String> =
        sqlx::query_scalar("SELECT rejection_reason FROM trade_rejections ORDER BY id")
            .fetch_all(&bot.db)
            .await
            .unwrap();
    assert_eq!(
        venue.state.lock().unwrap().posts,
        2,
        "only the settled actor handoff may submit the one taker entry; rejections={rejections:?}, kill={}, degraded={}",
        bot.kill_switch_active(), bot.degraded_no_trade_active()
    );
    assert!(db::maker::pending_orders(&bot.db).await.unwrap().is_empty());
    let pending = db::live::pending_orders(&bot.db).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].phase, "entry");
    assert!(
        db::load_trade_by_id(&bot.db, pending[0].trade_id)
            .await
            .unwrap()
            .unwrap()
            .shares
            == 0.0
    );
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}
