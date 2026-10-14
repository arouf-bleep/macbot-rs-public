/// 147,567 of the production outbox's 271,909 rows are suppressions, and
/// 59,965 of those say `no_inventory` — the resting state of a flat maker,
/// rewritten every few seconds. Only changes are worth a durable row.
/// A two-tick book on a two-tick threshold must pass. In f64,
/// `(0.52 - 0.50) * 10_000` is fractionally above 200, and the comparison
/// rejected it while the log printed "200.0bps > max 200.0bps".
#[test]
fn a_book_exactly_on_the_spread_threshold_is_not_rejected_by_float_noise() {
    let mut cfg: crate::config::Config =
        serde_json::from_str(include_str!("../../config.json")).expect("config");
    cfg.maker_market_quality_max_spread_bps = 200.0;
    let noisy = (0.52_f64 - 0.50_f64) * 10_000.0;
    assert!(noisy > 200.0, "the fixture must reproduce the noise");
    let bbo = test_bbo("m", "t", Some(true), Some(0.50), Some(0.52), Some(noisy));
    assert_eq!(
        super::maker_market_quality_guard(&cfg, &bbo),
        None,
        "two ticks on a two-tick threshold"
    );
    let wide = test_bbo("m", "t", Some(true), Some(0.50), Some(0.53), Some(300.0));
    assert!(
        super::maker_market_quality_guard(&cfg, &wide).is_some(),
        "three ticks still fails"
    );
}

/// Trade 296: the book was 0.58 / 0.60 on a market whose tick is 0.01,
/// and the maker bid 0.599 for 1.695 shares. Neither the price nor the
/// size is an order the venue accepts (minimum 5 shares).
#[test]
fn quotes_sit_on_the_venue_grid_and_clear_its_minimum() {
    let mut cfg = test_config();
    cfg.maker_spread_floor_bps = 100.0;
    cfg.maker_spread_ceiling_bps = 200.0;
    let book = test_bbo(
        "m",
        "t-no",
        Some(false),
        Some(0.58),
        Some(0.60),
        Some(200.0),
    );
    let cent = VenueRules {
        tick_size: 0.01,
        min_order_size: 5.0,
    };
    // p_yes 0.306 → NO fair 0.694, well above the ask; the bid is one
    // tick under the ask, and one tick is a cent here.
    let plan = build_quote_plan(
        &cfg,
        &book,
        &cent,
        0.306,
        0.0,
        &MakerMarketContext::default(),
    )
    .expect("plan");
    assert!(
        (plan.bid - 0.59).abs() < 1e-9,
        "bid {} is not on the cent grid",
        plan.bid
    );
    assert!(
        ((plan.ask * 100.0).round() / 100.0 - plan.ask).abs() < 1e-9,
        "ask {} is not on the cent grid",
        plan.ask
    );

    // $5 at a 0.59 mid is 8.5 shares: clears the minimum.
    assert_eq!(venue_min_size_guard(&cfg, &cent, &plan), None);

    // $1 is 1.69 shares: refused, and the reason names the size that
    // would clear it at this price.
    cfg.maker_quote_notional_usd = 1.0;
    let small = build_quote_plan(
        &cfg,
        &book,
        &cent,
        0.306,
        0.0,
        &MakerMarketContext::default(),
    )
    .expect("plan");
    assert!(small.size < 5.0);
    let reason = venue_min_size_guard(&cfg, &cent, &small).expect("refused");
    assert!(reason.starts_with("venue_min_size("), "{reason}");
    assert!(reason.contains("needs >= $2.95 at 0.59"), "{reason}");

    // A market that reports no minimum does not refuse.
    let free = VenueRules {
        tick_size: 0.01,
        min_order_size: 0.0,
    };
    assert_eq!(venue_min_size_guard(&cfg, &free, &small), None);

    // On a 0.001 market the same book allows 0.599.
    let fine = VenueRules {
        tick_size: 0.001,
        min_order_size: 5.0,
    };
    let plan = build_quote_plan(
        &cfg,
        &book,
        &fine,
        0.306,
        0.0,
        &MakerMarketContext::default(),
    )
    .expect("plan");
    assert!((plan.bid - 0.599).abs() < 1e-9, "bid {}", plan.bid);
}

/// The feed tells the actor each market's rules with its target set,
/// and a tick change mid-session replaces them. Unknown markets get
/// the venue's defaults, never a finer grid.
#[test]
fn venue_rules_follow_the_feed() {
    let mut st = MakerActorState::default();
    assert_eq!(st.venue_rules("unknown"), VenueRules::default());
    assert_eq!(VenueRules::default().tick_size, 0.01);
    assert_eq!(VenueRules::default().min_order_size, 5.0);

    let fine = VenueRules {
        tick_size: 0.001,
        min_order_size: 5.0,
    };
    assert!(st.set_venue_rules("m-1", fine));
    assert!(
        !st.set_venue_rules("m-1", fine),
        "unchanged rules are not a change"
    );
    assert_eq!(st.venue_rules("m-1"), fine);
    let cent = VenueRules {
        tick_size: 0.01,
        min_order_size: 5.0,
    };
    assert!(st.set_venue_rules("m-1", cent));
    assert_eq!(
        st.snapshot()
            .pointer("/venue_rules_by_market/m-1/tick_size")
            .and_then(|v| v.as_f64()),
        Some(0.01)
    );
}

/// `spread_bps?` returned None from the guard, which reads as "no
/// problem", so a book with an empty side passed market quality.
#[test]
fn a_one_sided_book_fails_market_quality() {
    let cfg: crate::config::Config =
        serde_json::from_str(include_str!("../../config.json")).expect("config");
    let bid_only = test_bbo("m", "t", Some(true), Some(0.98), None, None);
    assert_eq!(
        super::maker_market_quality_guard(&cfg, &bid_only).as_deref(),
        Some("market_quality(one_sided_book)")
    );
    let ask_only = test_bbo("m", "t", Some(true), None, Some(0.02), None);
    assert!(super::maker_market_quality_guard(&cfg, &ask_only).is_some());
}

/// Books, models and roll state are keyed by token and were never
/// removed; the feed's target set is the one authority on what is live.
#[test]
fn retain_l2_tokens_evicts_everything_for_untargeted_tokens() {
    let mut st = MakerActorState::default();
    for (market, token) in [
        ("m-old", "t-old-yes"),
        ("m-old", "t-old-no"),
        ("m-new", "t-new"),
    ] {
        st.update_bbo(test_bbo(
            market,
            token,
            Some(true),
            Some(0.40),
            Some(0.42),
            Some(200.0),
        ));
        st.update_bbo(test_bbo(
            market,
            token,
            Some(true),
            Some(0.41),
            Some(0.43),
            Some(200.0),
        ));
        st.update_model(MakerModelState {
            market_id: market.to_string(),
            token_id: token.to_string(),
            ..MakerModelState::default()
        });
    }
    assert_eq!(st.l2_bbo_by_token.len(), 3);
    assert_eq!(st.model_by_token.len(), 3);
    assert_eq!(st.roll_by_token.len(), 3);
    assert_eq!(st.roll_mid_history_by_token.len(), 3);

    let keep: HashSet<String> = HashSet::from(["t-new".to_string()]);
    assert_eq!(st.retain_l2_tokens(&keep), 2);
    assert_eq!(
        st.l2_bbo_by_token.keys().collect::<Vec<_>>(),
        vec![&"t-new".to_string()]
    );
    assert_eq!(st.model_by_token.len(), 1);
    assert_eq!(st.roll_by_token.len(), 1);
    assert_eq!(st.roll_mid_history_by_token.len(), 1);
    assert_eq!(
        st.snapshot()
            .pointer("/l2_summary/token_count")
            .and_then(|v| v.as_u64()),
        Some(1)
    );
    // Nothing to evict is a no-op, not a panic.
    assert_eq!(st.retain_l2_tokens(&keep), 0);
    assert_eq!(st.retain_l2_tokens(&HashSet::new()), 1);
    assert!(st.l2_bbo_by_token.is_empty());
}

#[test]
fn a_suppression_is_recorded_once_until_something_changes() {
    let mut state = super::MakerActorState::default();

    assert!(state.suppression_is_new("m1", "ASK", "no_inventory"));
    assert!(
        !state.suppression_is_new("m1", "ASK", "no_inventory"),
        "the same answer a few seconds later is not a new event"
    );
    assert!(
        state.suppression_is_new("m1", "ASK", "market_quality(spread)"),
        "a different reason is a transition and must be recorded"
    );

    // The same verdict with a different live figure is not a transition:
    // reasons embed the number that tripped them, and it moves every tick.
    assert!(state.suppression_is_new("m1", "BID", "entry_ineligible(p_up 0.930 > max 0.400)"));
    assert!(
        !state.suppression_is_new("m1", "BID", "entry_ineligible(p_up 0.956 > max 0.400)"),
        "only the figure changed; the verdict did not"
    );
    assert!(
        state.suppression_is_new(
            "m1",
            "BID",
            "entry_ineligible(net_ev 0.0000 < required 0.0025)"
        ),
        "a different guard tripping is a real transition"
    );

    // Sides and markets are tracked apart.
    assert!(state.suppression_is_new("m1", "BID", "no_inventory"));
    assert!(state.suppression_is_new("m2", "ASK", "no_inventory"));

    // Quoting again resets it, so the next stop is recorded even if the
    // reason repeats.
    state.clear_suppression("m1", "ASK");
    assert!(state.suppression_is_new("m1", "ASK", "market_quality(spread)"));
}

/// These markets roll every fifteen minutes. Anything keyed by market id
/// that is not dropped with the market grows for the life of the process.
#[test]
fn suppression_memory_does_not_outlive_the_market() {
    let mut state = super::MakerActorState::default();
    state.suppression_is_new("m1", "ASK", "no_inventory");
    state.suppression_is_new("m1", "BID", "no_inventory");
    state.suppression_is_new("m2", "ASK", "no_inventory");
    assert_eq!(state.last_suppression.len(), 3);

    state.clear_market("m1");
    assert_eq!(
        state.last_suppression.len(),
        1,
        "m1's entries outlived the market that created them"
    );
    assert!(state.last_suppression.contains_key("m2|ASK"));
}
use super::*;
use proptest::prelude::*;
use serde_json::json;
use sqlx::SqlitePool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::bot::state::FuturesCache;
use crate::config::Config;
use crate::db;
use crate::market::l2_book::{TopOfBook, VenueRules};

fn test_config() -> Config {
    let mut cfg: Config =
        serde_json::from_str(include_str!("../../config.json")).expect("valid config fixture");
    cfg.mode = "simulation".to_string();
    cfg.execution_mode = "maker".to_string();
    cfg.auto_trading = true;
    cfg.min_ev = 0.0;
    cfg.edge_safety_buffer = 0.0;
    cfg.trade_gas_usd = 0.0;
    cfg.min_trade_usd = 0.05;
    cfg.max_exposure_pct = 0.0;
    cfg.live_confidence_cap = 1.0;
    cfg.min_confidence_margin = 0.0;
    cfg.strategy.params.min_confidence = 0.55;
    cfg.maker_live_quoting = false;
    // Existing engine contract tests isolate their concern from the
    // separately tested conservative paper execution assumptions.
    cfg.sim_order_latency_ms = 0;
    cfg.sim_available_depth_fraction = 1.0;
    cfg.maker_sim_queue_ahead_shares = 0.0;
    cfg.maker_sim_max_fill_fraction = 1.0;
    cfg.maker_spread_floor_bps = 20.0;
    cfg.maker_spread_ceiling_bps = 80.0;
    // Five shares is the venue minimum; a $1 quote at 0.50 is two.
    cfg.maker_quote_notional_usd = 5.0;
    cfg
}

fn test_bbo(
    market_id: &str,
    token_id: &str,
    is_yes_token: Option<bool>,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    spread_bps: Option<f64>,
) -> TopOfBook {
    TopOfBook {
        market_id: market_id.to_string(),
        token_id: token_id.to_string(),
        is_yes_token,
        best_bid,
        best_bid_size: Some(100.0),
        best_ask,
        best_ask_size: Some(100.0),
        spread_bps,
        sequence: Some(1),
        snapshot_loaded: true,
        stale: false,
        updated_at: "2026-03-09T00:00:00Z".to_string(),
    }
}

fn test_top_of_book(
    market_id: &str,
    token_id: &str,
    is_yes_token: Option<bool>,
    best_bid: Option<f64>,
    best_ask: Option<f64>,
    snapshot_loaded: bool,
    stale: bool,
) -> TopOfBook {
    TopOfBook {
        market_id: market_id.to_string(),
        token_id: token_id.to_string(),
        is_yes_token,
        best_bid,
        best_bid_size: best_bid.map(|_| 100.0),
        best_ask,
        best_ask_size: best_ask.map(|_| 100.0),
        spread_bps: match (best_bid, best_ask) {
            (Some(bid), Some(ask)) if ask >= bid => Some((ask - bid) * 10_000.0),
            _ => None,
        },
        sequence: Some(1),
        snapshot_loaded,
        stale,
        updated_at: "2026-03-09T00:00:00Z".to_string(),
    }
}

#[test]
fn execution_mode_normalization_is_safe() {
    assert_eq!(normalized_execution_mode("taker"), "taker");
    assert_eq!(normalized_execution_mode("maker"), "maker");
    assert_eq!(normalized_execution_mode("hybrid"), "hybrid");
    assert_eq!(normalized_execution_mode("unknown"), "hybrid");
}

#[test]
fn maker_state_upsert_remove_and_clear_market() {
    let mut st = MakerActorState::default();

    let q1 = st
        .upsert_quote(
            "oid-1".to_string(),
            "mkt-1".to_string(),
            "tok-1".to_string(),
            QuoteSide::Bid,
            0.51,
            10.0,
        )
        .expect("quote should be accepted");
    assert_eq!(q1.sequence, 1);
    assert_eq!(st.quotes.len(), 1);

    let q2 = st
        .upsert_quote(
            "oid-2".to_string(),
            "mkt-1".to_string(),
            "tok-2".to_string(),
            QuoteSide::Ask,
            0.53,
            9.0,
        )
        .expect("quote should be accepted");
    assert_eq!(q2.sequence, 2);
    assert_eq!(st.quotes.len(), 2);

    assert!(st.remove_quote("oid-1"));
    assert_eq!(st.quotes.len(), 1);

    let cleared = st.clear_market("mkt-1");
    assert_eq!(cleared, 1);
    assert!(st.quotes.is_empty());
}

#[test]
fn maker_state_replaces_existing_quote_for_same_market_token_and_side() {
    let mut st = MakerActorState::default();

    let _ = st
        .upsert_quote(
            "oid-old".to_string(),
            "mkt-1".to_string(),
            "tok-1".to_string(),
            QuoteSide::Bid,
            0.50,
            10.0,
        )
        .expect("first quote");

    let replacement = st
        .upsert_quote(
            "oid-new".to_string(),
            "mkt-1".to_string(),
            "tok-1".to_string(),
            QuoteSide::Bid,
            0.51,
            11.0,
        )
        .expect("replacement quote");

    assert_eq!(
        st.quotes.len(),
        1,
        "only one quote should remain for the side"
    );
    assert!(!st.quotes.contains_key("oid-old"));
    assert!(st.quotes.contains_key("oid-new"));
    assert_eq!(replacement.sequence, 2);

    let current = st
        .quote_for("mkt-1", "tok-1", &QuoteSide::Bid)
        .expect("current quote");
    assert_eq!(current.order_id, "oid-new");
    assert!((current.price - 0.51).abs() < 1e-9);

    let market_orders = st.by_market.get("mkt-1").expect("market set");
    assert_eq!(market_orders.len(), 1);
    assert!(market_orders.contains("oid-new"));
}

#[test]
fn quote_plan_respects_skew_and_spread_bounds() {
    let mut cfg = test_config();
    cfg.maker_skew_per_usd_bps = 0.25;
    cfg.maker_max_skew_bps = 200.0;

    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(150.0),
    );
    let plan = build_quote_plan(
        &cfg,
        &bbo,
        &VenueRules::default(),
        0.50,
        1_000.0,
        &MakerMarketContext::default(),
    )
    .expect("quote plan");

    assert!((plan.skew_bps - 200.0).abs() < 1e-9);
    assert!((plan.target_spread_bps - cfg.maker_spread_ceiling_bps).abs() < 1e-9);
    assert!((plan.fair_value - 0.48).abs() < 1e-9);
    assert!(plan.bid < plan.ask);
    assert!(plan.ask >= 0.491); // at least one tick above observed best bid
    assert!(plan.bid <= 0.509); // at least one tick below observed best ask
}

#[test]
fn quote_plan_handles_one_sided_and_rejects_empty_book() {
    let cfg = test_config();

    let one_sided = test_bbo("mkt-1", "tok-x", None, Some(0.52), None, None);
    let plan = build_quote_plan(
        &cfg,
        &one_sided,
        &VenueRules::default(),
        0.50,
        0.0,
        &MakerMarketContext::default(),
    )
    .expect("one sided book still quotable");
    assert!(plan.bid < plan.ask);

    let empty = test_bbo("mkt-1", "tok-y", None, None, None, None);
    assert!(build_quote_plan(
        &cfg,
        &empty,
        &VenueRules::default(),
        0.50,
        0.0,
        &MakerMarketContext::default()
    )
    .is_none());
}

#[tokio::test]
async fn maker_entry_blocks_market_mismatch_without_claiming_stale_analysis() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_maker_mismatch_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg.clone(), pool).await.expect("bot");

    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-other",
                "price": 0.53,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.53,
            "confidence": 0.70,
            "net_ev": 0.02
        });
    }
    bot.mark_analysis_fresh().await;

    let block = evaluate_maker_entry(&bot, &cfg, "mkt-target", Direction::BuyYes, 0.51, 1.0, None)
        .await
        .expect_err("expected market mismatch");

    assert_eq!(block.reason, "no fresh analysis for market mkt-target");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn maker_entry_blocks_true_analysis_stale_with_clear_reason() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.analysis_stale_seconds = 1;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_maker_stale_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg.clone(), pool).await.expect("bot");

    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-target",
                "price": 0.53,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.53,
            "confidence": 0.70,
            "net_ev": 0.02
        });
    }
    {
        let mut last_at = bot.analysis_state.last_analysis_at.write().await;
        *last_at = Instant::now() - Duration::from_secs(2);
    }

    let block = evaluate_maker_entry(&bot, &cfg, "mkt-target", Direction::BuyYes, 0.51, 1.0, None)
        .await
        .expect_err("expected stale analysis");

    assert_eq!(block.reason, "analysis stale");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[test]
fn quote_plan_widens_under_backwardation_when_enabled() {
    let mut cfg = test_config();
    cfg.maker_backwardation_guard_pct = -0.02;
    cfg.maker_backwardation_widen_bps = 40.0;
    cfg.maker_spread_floor_bps = 20.0;
    cfg.maker_spread_ceiling_bps = 200.0;

    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.495),
        Some(0.505),
        Some(100.0),
    );

    // A 40 bps widening is under one cent; on a 0.01 grid it rounds
    // away. The venue quotes some markets at 0.001, and that is where a
    // sub-cent widening is visible.
    let fine = VenueRules {
        tick_size: 0.001,
        min_order_size: 5.0,
    };
    let normal = build_quote_plan(&cfg, &bbo, &fine, 0.50, 0.0, &MakerMarketContext::default())
        .expect("normal quote plan");
    let toxic_ctx = MakerMarketContext {
        basis_pct: Some(-0.03),
        ..MakerMarketContext::default()
    };
    let widened =
        build_quote_plan(&cfg, &bbo, &fine, 0.50, 0.0, &toxic_ctx).expect("widened quote plan");

    assert!((normal.target_spread_bps - 100.0).abs() < 1e-9);
    assert!((widened.target_spread_bps - 140.0).abs() < 1e-9);
    assert!(widened.bid < normal.bid);
    assert!(widened.ask > normal.ask);
}

#[test]
fn quote_plan_honors_backwardation_min_target_spread_floor() {
    let mut cfg = test_config();
    cfg.maker_backwardation_guard_pct = -0.02;
    cfg.maker_backwardation_widen_bps = 40.0;
    cfg.maker_backwardation_min_target_spread_bps = 120.0;
    cfg.maker_spread_floor_bps = 20.0;
    cfg.maker_spread_ceiling_bps = 200.0;

    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.499),
        Some(0.501),
        Some(20.0),
    );

    let toxic_ctx = MakerMarketContext {
        basis_pct: Some(-0.03),
        ..MakerMarketContext::default()
    };
    let widened = build_quote_plan(&cfg, &bbo, &VenueRules::default(), 0.50, 0.0, &toxic_ctx)
        .expect("widened quote plan");

    assert!((widened.target_spread_bps - 120.0).abs() < 1e-9);
}

#[test]
fn roll_guard_maps_toxic_side_by_token_direction() {
    assert!(roll_signal_is_toxic_for_side(
        Some(true),
        &QuoteSide::Ask,
        0.30
    ));
    assert!(roll_signal_is_toxic_for_side(
        Some(true),
        &QuoteSide::Bid,
        -0.30
    ));
    assert!(roll_signal_is_toxic_for_side(
        Some(false),
        &QuoteSide::Bid,
        0.30
    ));
    assert!(roll_signal_is_toxic_for_side(
        Some(false),
        &QuoteSide::Ask,
        -0.30
    ));
    assert!(!roll_signal_is_toxic_for_side(
        Some(true),
        &QuoteSide::Bid,
        0.30
    ));
    assert!(!roll_signal_is_toxic_for_side(
        Some(false),
        &QuoteSide::Ask,
        0.30
    ));
}

#[test]
fn replace_limits_enforce_lifetime_cooldown_and_rate_cap() {
    let mut cfg = test_config();
    cfg.maker_min_quote_lifetime_ms = 1_000;
    cfg.maker_replace_cooldown_ms = 500;
    cfg.maker_max_replaces_per_min = 2;

    let mut st = MakerActorState::default();
    let q = st
        .upsert_quote(
            "oid-1".to_string(),
            "mkt-1".to_string(),
            "tok-1".to_string(),
            QuoteSide::Bid,
            0.51,
            1.0,
        )
        .expect("quote inserted");

    let now = Instant::now();
    assert_eq!(
        st.check_replace_limits("mkt-1", "tok-1", &QuoteSide::Bid, &q.order_id, now, &cfg),
        ReplaceCheck::TooSoon
    );

    // Age quote enough to clear min-lifetime gate.
    st.quote_birth
        .insert(q.order_id.clone(), now - Duration::from_secs(2));
    assert_eq!(
        st.check_replace_limits("mkt-1", "tok-1", &QuoteSide::Bid, &q.order_id, now, &cfg),
        ReplaceCheck::ReplaceAllowed
    );

    st.mark_replace("mkt-1", "tok-1", &QuoteSide::Bid, now);
    assert_eq!(
        st.check_replace_limits(
            "mkt-1",
            "tok-1",
            &QuoteSide::Bid,
            &q.order_id,
            now + Duration::from_millis(100),
            &cfg
        ),
        ReplaceCheck::Cooldown
    );

    // Force rate-limit path (clear cooldown by removing side timestamp).
    st.last_replace_by_key.clear();
    st.replace_events.clear();
    st.replace_events
        .push_back(now + Duration::from_millis(200));
    st.replace_events
        .push_back(now + Duration::from_millis(300));
    assert_eq!(
        st.check_replace_limits(
            "mkt-1",
            "tok-1",
            &QuoteSide::Bid,
            &q.order_id,
            now + Duration::from_secs(1),
            &cfg
        ),
        ReplaceCheck::RateLimited
    );
}

proptest! {
    #[test]
    fn quote_plan_property_invariants_hold(
        p_yes in 0.0f64..1.0,
        inventory_notional in -10_000.0f64..10_000.0,
        best_bid in 0.01f64..0.98,
        spread in 0.0015f64..0.15,
        observed_spread_bps in 1.0f64..800.0,
        is_yes in any::<bool>(),
    ) {
        let mut cfg = test_config();
        cfg.maker_spread_floor_bps = 5.0;
        cfg.maker_spread_ceiling_bps = 120.0;
        cfg.maker_quote_notional_usd = 0.25;

        let best_ask = (best_bid + spread).clamp(best_bid + 0.001, 0.999);
        prop_assume!(best_ask > best_bid);

        let bbo = test_bbo(
            "mkt-prop",
            "tok-prop",
            Some(is_yes),
            Some(best_bid),
            Some(best_ask),
            Some(observed_spread_bps),
        );

        if let Some(plan) =
            build_quote_plan(&cfg, &bbo, &VenueRules::default(), p_yes, inventory_notional, &MakerMarketContext::default())
        {
            prop_assert!(plan.bid.is_finite() && plan.ask.is_finite());
            prop_assert!(plan.fair_value.is_finite());
            prop_assert!(plan.size.is_finite());
            prop_assert!(plan.ask > plan.bid);

            prop_assert!(plan.bid >= MAKER_MIN_PRICE - 1e-12);
            prop_assert!(plan.ask <= MAKER_MAX_PRICE + 1e-12);
            prop_assert!(plan.fair_value >= MAKER_MIN_PRICE - 1e-12);
            prop_assert!(plan.fair_value <= MAKER_MAX_PRICE + 1e-12);
            prop_assert!(plan.size > 0.0);

            prop_assert!(plan.target_spread_bps >= cfg.maker_spread_floor_bps - 1e-9);
            prop_assert!(plan.target_spread_bps <= cfg.maker_spread_ceiling_bps + 1e-9);
            prop_assert!(plan.skew_bps >= -cfg.maker_max_skew_bps - 1e-9);
            prop_assert!(plan.skew_bps <= cfg.maker_max_skew_bps + 1e-9);
        }
    }
}

#[test]
fn duplicate_flush_ticks_coalesce_without_losing_progress() {
    let before = crate::metrics::snapshot().actor_cmd_drop_low;
    let mut high: VecDeque<ExecutionCommand> = VecDeque::new();
    let mut close: VecDeque<ExecutionCommand> = VecDeque::new();
    let mut medium: VecDeque<ExecutionCommand> = VecDeque::new();
    let mut low: VecDeque<ExecutionCommand> = VecDeque::new();

    let target = LANE_LOW_CAPACITY;
    for _ in 0..target {
        try_push_lane(
            ExecutionCommand::FlushBestBidAsk,
            &mut high,
            &mut close,
            &mut medium,
            &mut low,
        );
    }
    let baseline_len = low.len();
    try_push_lane(
        ExecutionCommand::FlushBestBidAsk,
        &mut high,
        &mut close,
        &mut medium,
        &mut low,
    );

    assert_eq!(
        baseline_len, 1,
        "BBO notifications coalesce without losing the only flush"
    );
    assert_eq!(low.len(), baseline_len);
    let after = crate::metrics::snapshot().actor_cmd_drop_low;
    assert!(after >= before.saturating_add(1));
}

#[test]
fn audit_and_priority_pressure_preserve_the_only_book_flush() {
    let mut high = VecDeque::new();
    let mut close = VecDeque::new();
    let mut medium = VecDeque::new();
    let mut low = VecDeque::new();
    for _ in 0..LANE_LOW_CAPACITY {
        low.push_back(ExecutionCommand::AppendOutboxAudit {
            stage: "test".into(),
            market_id: "m".into(),
            order_id: None,
            token_id: None,
            action: "test".into(),
            status: "ok".into(),
            details: "{}".into(),
        });
    }
    try_push_lane(
        ExecutionCommand::FlushBestBidAsk,
        &mut high,
        &mut close,
        &mut medium,
        &mut low,
    );
    assert_eq!(low.len(), LANE_LOW_CAPACITY);
    assert_eq!(
        low.iter()
            .filter(|cmd| matches!(cmd, ExecutionCommand::FlushBestBidAsk))
            .count(),
        1
    );
    for _ in 0..(LANE_HIGH_CAPACITY + LANE_LOW_CAPACITY + 1) {
        try_push_lane(
            ExecutionCommand::SetL2Stale {
                stale: true,
                code: None,
                reason: None,
            },
            &mut high,
            &mut close,
            &mut medium,
            &mut low,
        );
    }
    assert_eq!(low.len(), 1);
    assert!(matches!(
        low.front(),
        Some(ExecutionCommand::FlushBestBidAsk)
    ));
}

#[tokio::test]
async fn process_maker_bbo_event_places_quotes_when_ready() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_place_telemetry_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.53,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.53,
            "confidence": 0.70,
            "net_ev": 0.02
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
        .is_some());
    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Ask)
        .is_none());
    let model = maker_state
        .model_by_token
        .get("tok-yes")
        .expect("model snapshot");
    assert!(model.book_two_sided);
    assert_eq!(model.bid_action, "placed");
    assert_eq!(model.ask_action, "suppressed");
    assert_eq!(model.ask_reason.as_deref(), Some("no_inventory"));
    assert!(model.decision.contains("placed") || model.decision.contains("hold"));

    sleep(Duration::from_millis(60)).await;
    let telemetry_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote_telemetry' AND action='place'",
    )
    .fetch_one(&bot.db)
    .await
    .expect("telemetry count");
    assert_eq!(
        telemetry_count, 1,
        "expected place telemetry for the entry side only"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_suppresses_bid_when_entry_is_ineligible() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_bid_ineligible_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.53,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.53,
            "confidence": 0.10,
            "net_ev": 0.02
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
        .is_none());
    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Ask)
        .is_none());
    let model = maker_state
        .model_by_token
        .get("tok-yes")
        .expect("model snapshot");
    assert_eq!(model.bid_action, "suppressed");
    assert!(model
        .bid_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("entry_ineligible")));
    assert_eq!(model.ask_action, "suppressed");
    assert_eq!(model.ask_reason.as_deref(), Some("no_inventory"));
    assert!(model.decision.contains("entry_ineligible"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_suppresses_buy_no_bid_when_p_up_too_high() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_buy_no_guard_enabled = true;
    cfg.maker_buy_no_max_p_up = 0.40;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_buy_no_guard_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.38,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.62,
            "confidence": 0.90,
            "net_ev": 0.08
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-no",
        Some(false),
        Some(0.21),
        Some(0.22),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-no", &QuoteSide::Bid)
        .is_none());
    assert!(maker_state
        .quote_for("mkt-1", "tok-no", &QuoteSide::Ask)
        .is_none());
    let model = maker_state
        .model_by_token
        .get("tok-no")
        .expect("model snapshot");
    assert_eq!(model.bid_action, "suppressed");
    assert!(model
        .bid_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("maker BUY_NO guard")));
    assert_eq!(model.ask_action, "suppressed");
    assert_eq!(model.ask_reason.as_deref(), Some("no_inventory"));
    assert!(model.decision.contains("entry_ineligible"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_suppresses_buy_no_bid_when_price_too_high() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_buy_no_guard_enabled = true;
    cfg.maker_buy_no_max_p_up = 0.40;
    cfg.maker_buy_no_max_entry_price = 0.78;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_buy_no_price_cap_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.18,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.14,
            "confidence": 0.90,
            "net_ev": 0.08
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-no",
        Some(false),
        Some(0.79),
        Some(0.81),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;

    let model = maker_state
        .model_by_token
        .get("tok-no")
        .expect("model snapshot");
    assert_eq!(model.bid_action, "suppressed");
    assert!(model
        .bid_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("maker BUY_NO price")));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_suppresses_buy_no_bid_when_edge_too_thin() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_buy_no_guard_enabled = true;
    cfg.maker_buy_no_max_p_up = 0.45;
    cfg.maker_buy_no_max_entry_price = 0.90;
    cfg.maker_buy_no_min_edge = 0.06;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_buy_no_edge_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.37,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.37,
            "confidence": 0.90,
            "net_ev": 0.08
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-no",
        Some(false),
        Some(0.67),
        Some(0.69),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;

    let model = maker_state
        .model_by_token
        .get("tok-no")
        .expect("model snapshot");
    assert_eq!(model.bid_action, "suppressed");
    assert!(model
        .bid_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("maker BUY_NO edge")));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_crossed_bid_opens_maker_trade() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_min_quote_lifetime_ms = 0;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_fill_open_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "maker fill open",
                "price": 0.50,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.50,
            "confidence": 0.62,
            "net_ev": 0.01
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());

    let first_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    maker_state.update_bbo(first_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &first_bbo, &outbox_tx).await;

    // On the venue's 0.01 grid the bid rests at 0.49, the best bid, so
    // only an ask through 0.49 fills it; 0.495 used to, against a bid at
    // the sub-tick 0.509.
    let crossed_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.48),
        Some(0.485),
        Some(50.0),
    );
    maker_state.update_bbo(crossed_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed_bbo, &outbox_tx).await;
    sleep(Duration::from_millis(80)).await;

    let open_trades = db::load_open_trades(&bot.db).await.expect("open trades");
    assert_eq!(open_trades.len(), 1, "expected a maker trade to open");
    let opened = &open_trades[0];
    assert_eq!(opened.direction, "BUY_YES");
    assert_eq!(opened.strategy, "maker");
    assert_eq!(opened.market_id, "mkt-1");
    assert_eq!(opened.question, "maker fill open");
    let fills = db::load_trade_fills(&bot.db, opened.id, 10)
        .await
        .expect("entry fills");
    let entry_fill = fills
        .iter()
        .find(|f| f.phase == "entry" && f.source == "maker_sim")
        .expect("maker entry fill");
    let details: serde_json::Value =
        serde_json::from_str(&entry_fill.details).expect("json maker entry verification");
    assert_eq!(
        details.get("path").and_then(serde_json::Value::as_str),
        Some("maker_passive")
    );
    assert_eq!(
        details
            .get("snapshot")
            .and_then(|v| v.get("source"))
            .and_then(serde_json::Value::as_str),
        Some("clob_ws_bbo")
    );

    let fill_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote' AND action='fill'",
    )
    .fetch_one(&bot.db)
    .await
    .expect("fill outbox count");
    assert_eq!(fill_count, 1, "expected one simulated maker fill audit");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

/// The shopkeeper's exit, with hold mode on: the bid fills, the position
/// opens, and then nothing — no ask is posted, and a book that would have
/// crossed one leaves the position exactly where it was.
#[tokio::test]
async fn in_hold_mode_a_filled_bid_is_held_not_flattened() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_min_quote_lifetime_ms = 0;
    cfg.maker_hold_positions = true;
    cfg.maker_buy_no_max_p_up = 0.40;
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_hold_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "hold mode",
                "price": 0.50,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.65,
            "confidence": 0.62,
            "net_ev": 0.01
        });
    }
    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());

    let first_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    maker_state.update_bbo(first_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &first_bbo, &outbox_tx).await;
    assert!(
        maker_state
            .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
            .is_some(),
        "the model favours YES at p_up 0.65, so the YES bid must be posted"
    );

    let crossed_bid_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.495),
        Some(50.0),
    );
    maker_state.update_bbo(crossed_bid_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed_bid_bbo, &outbox_tx).await;
    sleep(Duration::from_millis(100)).await;
    let open = db::load_open_trades(&bot.db).await.expect("open");
    assert_eq!(open.len(), 1, "the crossed bid must still open a position");

    // The shopkeeper would now have an ask resting. The collector does not.
    assert!(
        maker_state
            .quote_for("mkt-1", "tok-yes", &QuoteSide::Ask)
            .is_none(),
        "hold mode posted a flatten ask"
    );
    let model = maker_state.model_by_token.get("tok-yes").expect("model");
    assert_eq!(model.ask_action, "suppressed");
    assert!(
        model
            .ask_reason
            .as_deref()
            .unwrap_or("")
            .contains("hold_mode"),
        "ask reason {:?}",
        model.ask_reason
    );

    // A book that would have crossed the shopkeeper's ask.
    let rally = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.60),
        Some(0.61),
        Some(100.0),
    );
    maker_state.update_bbo(rally.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &rally, &outbox_tx).await;
    sleep(Duration::from_millis(100)).await;
    let still_open = db::load_open_trades(&bot.db).await.expect("open");
    assert_eq!(still_open.len(), 1, "the rally flattened a held position");
    assert!(db::load_closed_trades(&bot.db)
        .await
        .expect("closed")
        .is_empty());

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

/// Trade 288 was closed by hand at 18:18:42; the resting bid refilled
/// at 18:18:47 and the position was back. A manual close is a person
/// overriding the strategy on that market, so the maker stands down
/// there until the market rolls, and not one second longer.
#[tokio::test]
// This current-thread test holds the process environment lease through async fixture work.
#[allow(clippy::await_holding_lock)]
async fn a_manual_close_stands_the_maker_down_until_the_market_rolls() {
    let _guard = crate::env_util::test_env_lock();
    let base = crate::bot::close::tests::serve_one_book(json!({
        "bids": [{"price":"0.55", "size":"100"}],
        "asks": [{"price":"0.57", "size":"100"}]
    }))
    .await;
    let _env = crate::bot::close::tests::EnvRestore::set("MACBOT_CLOB_BASE", &base);

    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_min_quote_lifetime_ms = 0;
    // The post-fill cooldown would hide the stand-down; it is not what
    // is under test.
    cfg.maker_fill_cooldown_ms = 0;
    cfg.maker_hold_positions = true;
    cfg.maker_buy_no_max_p_up = 0.40;
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_manual_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = Arc::new(TradingBot::new(cfg, pool).await.expect("bot"));
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "manual close",
                "price": 0.50,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.65,
            "confidence": 0.62,
            "net_ev": 0.01
        });
    }
    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());

    let quiet = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    let crossed = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.495),
        Some(50.0),
    );
    maker_state.update_bbo(quiet.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &quiet, &outbox_tx).await;
    maker_state.update_bbo(crossed.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed, &outbox_tx).await;
    sleep(Duration::from_millis(100)).await;
    let open = db::load_open_trades(&bot.db).await.expect("open");
    assert_eq!(open.len(), 1, "the crossed bid opens a position");
    let trade_id = open[0].id;

    // The supplied mark is only a trigger; the sale uses our stub book.
    // Manual intent also stands down the market.
    let (resp_tx, resp_rx) = oneshot::channel();
    process_execution_command(
        ExecutionCommand::CloseTrade {
            trade_id,
            exit_price: Some(0.55),
            intent: CloseIntent::Manual,
            resp: resp_tx,
        },
        &bot,
        &mut maker_state,
        &outbox_tx,
    )
    .await;
    let closed = resp_rx.await.expect("resp").expect("closed");
    assert!(!closed.is_open());
    assert!(maker_state.hands_off_markets.contains("mkt-1"));
    assert!(
        maker_state.quotes.values().all(|q| q.market_id != "mkt-1"),
        "a manual close cancels the market's resting quotes"
    );

    // The book crosses again. Nothing happens.
    maker_state.update_bbo(quiet.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &quiet, &outbox_tx).await;
    maker_state.update_bbo(crossed.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed, &outbox_tx).await;
    sleep(Duration::from_millis(100)).await;
    assert!(
        db::load_open_trades(&bot.db)
            .await
            .expect("open")
            .is_empty(),
        "the maker re-entered a market a person had just closed"
    );
    let model = maker_state.model_by_token.get("tok-yes").expect("model");
    assert_eq!(model.decision, "manual_close");

    // A roll creates a new market identity. Clearing book/quote caches
    // must not unlock the old market that the operator stopped.
    maker_state.clear_market("mkt-1");
    assert!(maker_state.hands_off_markets.contains("mkt-1"));
    let mut quiet = quiet.clone();
    let mut crossed = crossed.clone();
    quiet.market_id = "mkt-rolled".into();
    crossed.market_id = "mkt-rolled".into();
    bot.analysis_state.last_analysis.write().await["market_data"]["market_id"] =
        json!("mkt-rolled");
    maker_state.update_bbo(quiet.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &quiet, &outbox_tx).await;
    maker_state.update_bbo(crossed.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed, &outbox_tx).await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        db::load_open_trades(&bot.db).await.expect("open").len(),
        1,
        "after the roll the maker quotes again"
    );

    // A strategy close does not stand the maker down.
    let open = db::load_open_trades(&bot.db).await.expect("open");
    let (resp_tx, resp_rx) = oneshot::channel();
    process_execution_command(
        ExecutionCommand::CloseTrade {
            trade_id: open[0].id,
            exit_price: Some(0.55),
            intent: CloseIntent::Market,
            resp: resp_tx,
        },
        &bot,
        &mut maker_state,
        &outbox_tx,
    )
    .await;
    assert!(!resp_rx.await.expect("resp").expect("closed").is_open());
    assert!(!maker_state.hands_off_markets.contains("mkt-rolled"));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

/// A collector without a view does not buy, and never buys against its
/// view. The margin is the BUY_NO guard's, applied to both sides.
#[tokio::test]
async fn in_hold_mode_the_bid_follows_the_model_or_stays_away() {
    for (p_up, token, is_yes, expect_bid, why) in [
        (0.65, "tok-yes", true, true, "YES favoured at 0.65"),
        (0.50, "tok-yes", true, false, "no view at 0.50: no YES bid"),
        (0.50, "tok-no", false, false, "no view at 0.50: no NO bid"),
        (0.65, "tok-no", false, false, "YES favoured: no NO bid"),
        (0.30, "tok-no", false, true, "NO favoured at 0.30"),
        (0.30, "tok-yes", true, false, "NO favoured: no YES bid"),
    ] {
        let mut cfg = test_config();
        cfg.execution_mode = "maker".to_string();
        cfg.maker_hold_positions = true;
        cfg.maker_buy_no_max_p_up = 0.40;
        cfg.maker_buy_no_guard_enabled = false;
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_actor_hold_side_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await.expect("sqlite pool");
        let bot = TradingBot::new(cfg, pool).await.expect("bot");
        bot.start_loop();
        bot.set_l2_feed_stale(false).await;
        {
            let mut last = bot.analysis_state.last_analysis.write().await;
            *last = json!({
                "market_data": { "market_id": "mkt-1", "price": 0.50, "end_date": "2099-01-01T00:00:00Z" },
                "p_up": p_up, "confidence": 0.62, "net_ev": 0.01
            });
        }
        let mut maker_state = MakerActorState::default();
        let outbox_tx = spawn_outbox_writer(bot.db.clone());
        let bbo = test_bbo(
            "mkt-1",
            token,
            Some(is_yes),
            Some(0.49),
            Some(0.51),
            Some(100.0),
        );
        maker_state.update_bbo(bbo.clone());
        process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;
        let has_bid = maker_state
            .quote_for("mkt-1", token, &QuoteSide::Bid)
            .is_some();
        assert_eq!(has_bid, expect_bid, "{why}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }
}

#[test]
fn cancellation_requires_the_requested_order_acknowledgement() {
    assert!(!cancellation_confirmed("wanted", &[]));
    assert!(!cancellation_confirmed("wanted", &["other".into()]));
    assert!(cancellation_confirmed("wanted", &["wanted".into()]));
}

#[tokio::test]
// This current-thread test holds the process environment lease through async fixture work.
#[allow(clippy::await_holding_lock)]
async fn pair_merge_settles_complementary_maker_inventory_without_book_or_fee() {
    let _guard = crate::env_util::test_env_lock();
    let base = crate::bot::close::tests::serve_one_book(json!({"bids": [], "asks": []})).await;
    let _env = crate::bot::close::tests::EnvRestore::set("MACBOT_CLOB_BASE", &base);
    let (bot, yes, path) = crate::bot::close::tests::maker_exit_fixture(2.0).await;
    let mut no = yes.clone();
    no.id = 0;
    no.direction = "BUY_NO".into();
    no.token_id = Some("test-no-token".into());
    no.id = db::insert_trade(&bot.db, &no).await.unwrap();
    *bot.portfolio_state.trades.write().await = vec![yes.clone(), no.clone()];
    let bot = Arc::new(bot);
    let (outbox_tx, _outbox_rx) = mpsc::channel(16);
    let mut maker = MakerActorState::default();
    for held_id in [yes.id, no.id] {
        bot.trade_lifecycle
            .partial_close_intents
            .write()
            .await
            .insert(
                held_id,
                crate::bot::state::PartialCloseIntent {
                    request_id: 9,
                    outcome_unknown: true,
                    live_order_id: None,
                },
            );
        let before_balance = bot.balance().await;
        let held = if held_id == yes.id { &yes } else { &no };
        let snapshot = PublicBookSnapshot::from_bbo(
            held.token_id.as_deref().unwrap(),
            Some(0.8),
            Some(100.0),
            Some(0.9),
            Some(100.0),
            Some(0.85),
            "synthetic-hold-regression",
        );
        for maximum in [held.shares, held.shares / 2.0] {
            assert_eq!(
                close_against_existing_inventory(
                    &bot,
                    &held.market_id,
                    held.direction_kind(),
                    0.8,
                    maximum,
                    &snapshot,
                )
                .await,
                (0.0, 0, 0),
                "synthetic flattening must respect the held lot"
            );
        }
        let (resp, rx) = oneshot::channel();
        process_execution_command(
            ExecutionCommand::ClosePair {
                first_trade_id: yes.id,
                second_trade_id: no.id,
                exit_price: Some(0.5),
                resp,
            },
            &bot,
            &mut maker,
            &outbox_tx,
        )
        .await;
        assert!(matches!(
            rx.await.unwrap(),
            Err(crate::bot::state::ActorCallError::PartialClosePending)
        ));
        assert_eq!(bot.balance().await, before_balance);
        assert!(db::load_trade_by_id(&bot.db, yes.id)
            .await
            .unwrap()
            .unwrap()
            .is_open());
        assert!(db::load_trade_by_id(&bot.db, no.id)
            .await
            .unwrap()
            .unwrap()
            .is_open());
        bot.trade_lifecycle
            .partial_close_intents
            .write()
            .await
            .remove(&held_id);
    }
    // A pair may change after it was snapshotted/admitted. Dequeue must
    // validate current inventory before either leg's accounting changes.
    for mutation in [
        "unequal",
        "same_side",
        "other_market",
        "live",
        "closing",
        "nonfinite",
        "unknown_mode",
        "wrong_payout",
        "inconsistent_cost",
        "receipt_accounting",
    ] {
        let mut changed = no.clone();
        match mutation {
            "unequal" => changed.shares = 1.9,
            "same_side" => changed.direction = yes.direction.clone(),
            "other_market" => changed.market_id = "different-market".into(),
            "live" => changed.mode = "live".into(),
            "closing" => changed.status = "CLOSING".into(),
            "nonfinite" => changed.shares = f64::NAN,
            "unknown_mode" => changed.mode = "unknown-mode".into(),
            "wrong_payout" => {}
            "inconsistent_cost" => changed.amount += 1.0,
            "receipt_accounting" => changed.accounting_version = 1,
            _ => unreachable!(),
        }
        *bot.portfolio_state.trades.write().await = vec![yes.clone(), changed];
        let before_balance = bot.balance().await;
        let (resp, rx) = oneshot::channel();
        process_execution_command(
            ExecutionCommand::ClosePair {
                first_trade_id: yes.id,
                second_trade_id: no.id,
                exit_price: Some(if mutation == "wrong_payout" { 0.9 } else { 0.5 }),
                resp,
            },
            &bot,
            &mut maker,
            &outbox_tx,
        )
        .await;
        assert!(
            matches!(
                rx.await.unwrap(),
                Err(crate::bot::state::ActorCallError::PairNotMergeable)
            ),
            "{mutation}"
        );
        assert_eq!(bot.balance().await, before_balance, "{mutation}");
        for original in [&yes, &no] {
            let persisted = db::load_trade_by_id(&bot.db, original.id)
                .await
                .unwrap()
                .unwrap();
            assert!(persisted.is_open(), "{mutation}");
            assert_eq!(persisted.shares, original.shares, "{mutation}");
            assert_eq!(persisted.pnl, original.pnl, "{mutation}");
        }
    }
    *bot.portfolio_state.trades.write().await = vec![yes.clone(), no.clone()];
    let (resp, rx) = oneshot::channel();
    process_execution_command(
        ExecutionCommand::ClosePair {
            first_trade_id: yes.id,
            second_trade_id: no.id,
            exit_price: Some(0.5),
            resp,
        },
        &bot,
        &mut maker,
        &outbox_tx,
    )
    .await;
    let (first, second) = rx.await.unwrap().unwrap();
    for closed in [first.unwrap(), second.unwrap()] {
        assert_eq!(closed.status, "CLOSED");
        assert_eq!(closed.exit_price, Some(0.5));
        assert_eq!(closed.exit_type.as_deref(), Some("merged"));
        assert!(closed.pnl.unwrap().abs() < 1e-9);
        let persisted = db::load_trade_by_id(&bot.db, closed.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.status, "CLOSED");
        assert_eq!(persisted.exit_type.as_deref(), Some("merged"));
    }
    assert!((bot.balance().await - 22.0).abs() < 1e-9);
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn simulated_maker_admission_and_fills_never_mutate_wallet_backed_cash() {
    for wallet_backed in [false, true] {
        let mut cfg = test_config();
        cfg.min_trade_usd = 1.0;
        cfg.max_open_positions = 0;
        cfg.maker_min_quote_lifetime_ms = 0;
        let pool = db::open(":memory:").await.unwrap();
        let mut bot = TradingBot::new(cfg.clone(), pool).await.unwrap();
        *bot.analysis_state.last_analysis.write().await = json!({
            "p_up":0.7,"confidence":0.75,
            "market_data":{"market_id":"maker-origin","price":0.5}
        });
        let order_id = "sim-mkr-origin-existing";
        // A legitimately admitted quote may survive a later mode change.
        admit_simulated_maker_order(
            &bot,
            &cfg,
            "maker-origin",
            "origin-token",
            Some(true),
            0.5,
            5.0,
            order_id,
        )
        .await
        .unwrap();
        if wallet_backed {
            // Inert public fixture; none of these guards performs network I/O.
            let auth = crate::clob::ClobAuth::new(
                "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "public-api-key",
                "cHVibGljLWZpeHR1cmUtc2VjcmV0",
                "public-passphrase",
            )
            .unwrap();
            bot.clob = Some(Arc::new(crate::clob::ClobClient::with_test_host(
                auth,
                "http://127.0.0.1:9".into(),
            )));
        } else {
            bot.control_plane.config.write().await.mode = "live".into();
        }
        // A stale non-live snapshot cannot override the process origin.
        cfg.shadow_trading = true;
        let before_balance = bot.balance().await;
        let mut maker = MakerActorState::default();
        maker
            .upsert_quote(
                order_id.into(),
                "maker-origin".into(),
                "origin-token".into(),
                QuoteSide::Bid,
                0.5,
                5.0,
            )
            .unwrap();
        let bbo = test_bbo(
            "maker-origin",
            "origin-token",
            Some(true),
            Some(0.4),
            Some(0.49),
            Some(100.0),
        );
        let (outbox_tx, _outbox_rx) = mpsc::channel(16);
        assert!(admit_simulated_maker_order(
            &bot,
            &cfg,
            "maker-origin",
            "origin-token",
            Some(true),
            0.5,
            5.0,
            "sim-mkr-origin-new",
        )
        .await
        .is_err());
        for side in [QuoteSide::Bid, QuoteSide::Ask] {
            assert!(place_quote(
                &bot,
                "maker-origin",
                "origin-token",
                &side,
                0.5,
                5.0,
                &cfg,
                Some(true),
                2,
                &outbox_tx,
            )
            .await
            .is_none());
        }
        assert!(open_simulated_maker_trade(
            &bot,
            &cfg,
            order_id,
            "blocked-observation",
            0.5,
            2.0,
            "{}",
        )
        .await
        .is_none());
        let quote = maker.quotes[order_id].clone();
        assert!(
            !simulate_passive_fill_for_quote(
                &bot,
                &mut maker,
                &cfg,
                quote,
                &bbo,
                &MakerMarketContext::default(),
                &outbox_tx,
            )
            .await
        );
        assert_eq!(
            simulate_passive_maker_fills(
                &bot,
                &mut maker,
                &cfg,
                &bbo,
                &MakerMarketContext::default(),
                &outbox_tx,
            )
            .await,
            0
        );
        assert!(maker.paper_fill_state.is_empty());
        assert_eq!(maker.quotes[order_id].size, 5.0);
        assert!(bot.portfolio_state.trades.read().await.is_empty());
        assert_eq!(bot.balance().await, before_balance);
        let counts: (i64, i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM paper_maker_orders),(SELECT COUNT(*) FROM paper_maker_fill_events),(SELECT COUNT(*) FROM trades),(SELECT COUNT(*) FROM trade_fills)",
            ).fetch_one(&bot.db).await.unwrap();
        assert_eq!(counts, (1, 0, 0, 0));
        let admitted: (String, f64, f64, Option<i64>) = sqlx::query_as(
                "SELECT status,filled_shares,filled_cost,trade_id FROM paper_maker_orders WHERE order_id=?",
            ).bind(order_id).fetch_one(&bot.db).await.unwrap();
        assert_eq!(admitted, ("OPEN".into(), 0.0, 0.0, None));
        // The exact same admitted order still fills normally in a genuine
        // non-wallet paper process, proving the origin gate is targeted.
        bot.clob = None;
        bot.control_plane.config.write().await.mode = cfg.mode.clone();
        let applied = open_simulated_maker_trade(
            &bot,
            &cfg,
            order_id,
            "valid-paper-observation",
            0.5,
            2.0,
            "{}",
        )
        .await
        .unwrap();
        assert_eq!(applied.applied_shares, 2.0);
        assert_eq!(applied.debit, 1.0);
        assert_eq!(bot.balance().await, before_balance - 1.0);
        assert_eq!(db::load_open_trades(&bot.db).await.unwrap().len(), 1);
        bot.db.close().await;
    }
}

#[tokio::test]
async fn live_mode_never_creates_synthetic_maker_quotes_or_fills() {
    let (bot, _, path) = crate::bot::close::tests::maker_exit_fixture(5.0).await;
    let (outbox_tx, _rx) = mpsc::channel(32);
    let mut maker = MakerActorState::default();
    maker.upsert_quote(
        "sim-mkr-existing".into(),
        "exit-market".into(),
        "test-token".into(),
        QuoteSide::Bid,
        0.6,
        5.0,
    );
    let bbo = test_bbo(
        "exit-market",
        "test-token",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    let before = bot.portfolio_state.trades.read().await.len();
    for quoting in [false, true] {
        let mut cfg = bot.control_plane.config.read().await.clone();
        cfg.mode = "live".into();
        cfg.maker_live_quoting = quoting;
        assert!(place_quote(
            &bot,
            "exit-market",
            "test-token",
            &QuoteSide::Bid,
            0.5,
            5.0,
            &cfg,
            Some(true),
            1,
            &outbox_tx
        )
        .await
        .is_none());
        assert_eq!(
            simulate_passive_maker_fills(
                &bot,
                &mut maker,
                &cfg,
                &bbo,
                &MakerMarketContext::default(),
                &outbox_tx
            )
            .await,
            0
        );
        assert_eq!(bot.portfolio_state.trades.read().await.len(), before);
    }
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn cancellation_provenance_survives_live_quoting_config_changes() {
    let (bot, _, path) = crate::bot::close::tests::maker_exit_fixture(2.0).await;
    assert!(bot.clob.is_none());
    // The exit fixture deliberately holds only $20 cash against a $50
    // initial budget. This test isolates quote provenance, not drawdown.
    {
        let mut cfg = bot.control_plane.config.write().await;
        cfg.drawdown_halt_daily_pct = 0.0;
        cfg.drawdown_halt_ath_pct = 0.0;
    }
    *bot.analysis_state.last_analysis.write().await = json!({
        "p_up":0.7,"confidence":0.75,"net_ev":0.4,
        "market_data":{"market_id":"exit-market","price":0.5}
    });
    let (outbox_tx, _outbox_rx) = mpsc::channel(32);
    let cfg = bot.control_plane.config.read().await.clone();
    let simulated = place_quote(
        &bot,
        "exit-market",
        "test-token",
        &QuoteSide::Bid,
        0.5,
        2.0,
        &cfg,
        Some(true),
        1,
        &outbox_tx,
    )
    .await
    .unwrap();
    for (mode, enabled) in [("live", true), ("live", false), ("simulation", false)] {
        {
            let mut cfg = bot.control_plane.config.write().await;
            cfg.mode = mode.into();
            cfg.maker_live_quoting = enabled;
        }
        assert!(
            !cancel_quote(
                &bot,
                "venue-order",
                "exit-market",
                "test-token",
                "manual_close",
                &outbox_tx,
            )
            .await,
            "a config toggle must never acknowledge a venue order"
        );
        assert!(
            cancel_quote(
                &bot,
                &simulated,
                "exit-market",
                "test-token",
                "manual_close",
                &outbox_tx,
            )
            .await,
            "a synthetic quote stays synthetic after a config toggle"
        );
    }
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn manual_cleanup_retries_at_most_once_per_market_per_second() {
    let (bot, _, path) = crate::bot::close::tests::maker_exit_fixture(2.0).await;
    let (outbox_tx, mut outbox_rx) = mpsc::channel(16);
    let mut maker = MakerActorState::default();
    maker.upsert_quote(
        "venue-order".into(),
        "exit-market".into(),
        "test-token".into(),
        QuoteSide::Bid,
        0.5,
        2.0,
    );
    maker.stand_down("exit-market");
    // Initial attempt is immediate. No CLOB client means no acknowledgement.
    retry_manual_quote_cleanup(&bot, &mut maker, &outbox_tx).await;
    assert_eq!(outbox_rx.try_recv().unwrap().status, "error");
    for _ in 0..20 {
        retry_manual_quote_cleanup(&bot, &mut maker, &outbox_tx).await;
    }
    assert!(
        outbox_rx.try_recv().is_err(),
        "book bursts must share the timer's throttle"
    );
    assert!(maker.quotes.contains_key("venue-order"));
    maker.manual_cancel_last_attempt.insert(
        "exit-market".into(),
        Instant::now() - Duration::from_secs(1),
    );
    retry_manual_quote_cleanup(&bot, &mut maker, &outbox_tx).await;
    assert_eq!(outbox_rx.try_recv().unwrap().status, "error");
    assert!(outbox_rx.try_recv().is_err());
    assert!(maker.quotes.contains_key("venue-order"));
    maker.clear_market("exit-market");
    assert!(!maker.manual_cancel_last_attempt.contains_key("exit-market"));
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn manual_cleanup_retains_unconfirmed_quotes_until_retry_ack() {
    let (bot, _, path) = crate::bot::close::tests::maker_exit_fixture(2.0).await;
    let (outbox_tx, _outbox_rx) = mpsc::channel(16);
    let mut maker = MakerActorState::default();
    maker.upsert_quote(
        "resting".into(),
        "exit-market".into(),
        "test-token".into(),
        QuoteSide::Bid,
        0.50,
        2.0,
    );
    maker.stand_down("exit-market");
    let cancelled = cancel_quotes_in_with(
        &bot,
        &mut maker,
        Some("exit-market"),
        "manual_close",
        &outbox_tx,
        |_| std::future::ready(false),
    )
    .await;
    assert_eq!(cancelled, 0);
    assert!(maker.quotes.contains_key("resting"));
    maker.retain_l2_tokens(&HashSet::new());
    assert!(
        maker.hands_off_markets.contains("exit-market"),
        "feed eviction must not forget outstanding cleanup"
    );
    let cancelled = cancel_quotes_in_with(
        &bot,
        &mut maker,
        Some("exit-market"),
        "manual_close",
        &outbox_tx,
        |_| std::future::ready(true),
    )
    .await;
    assert_eq!(cancelled, 1);
    assert!(maker.quotes.is_empty());
    assert!(
        maker.hands_off_markets.contains("exit-market"),
        "acknowledgement does not rearm entries"
    );
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
// This current-thread test holds the process environment lease through async fixture work.
#[allow(clippy::await_holding_lock)]
async fn manual_request_stands_down_even_when_the_sale_cannot_fill() {
    let _guard = crate::env_util::test_env_lock();
    let base = crate::bot::close::tests::serve_one_book(json!({
        "bids": [{"price":"0.60", "size":"2"}],
        "asks": [{"price":"0.62", "size":"100"}]
    }))
    .await;
    let _env = crate::bot::close::tests::EnvRestore::set("MACBOT_CLOB_BASE", &base);
    let (bot, trade, path) = crate::bot::close::tests::maker_exit_fixture(100.0).await;
    let bot = Arc::new(bot);
    let (outbox_tx, _outbox_rx) = mpsc::channel(16);
    let mut maker = MakerActorState::default();
    maker.upsert_quote(
        "sim-mkr-resting".into(),
        trade.market_id.clone(),
        "test-token".into(),
        QuoteSide::Bid,
        0.50,
        2.0,
    );
    let (resp, rx) = oneshot::channel();
    process_execution_command(
        ExecutionCommand::CloseTrade {
            trade_id: trade.id,
            exit_price: None,
            intent: CloseIntent::Manual,
            resp,
        },
        &bot,
        &mut maker,
        &outbox_tx,
    )
    .await;
    let refused = rx
        .await
        .unwrap()
        .expect("existing trade returns still-open semantics");
    assert!(refused.is_open());
    assert_eq!(refused.shares, 100.0);
    assert!(maker.hands_off_markets.contains(&trade.market_id));
    assert!(maker.quotes.is_empty());
    assert!(db::load_trade_by_id(&bot.db, trade.id)
        .await
        .unwrap()
        .unwrap()
        .is_open());
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn hands_off_book_updates_retry_pending_cancellations_without_quoting() {
    let (bot, trade, path) = crate::bot::close::tests::maker_exit_fixture(2.0).await;
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    let (outbox_tx, _outbox_rx) = mpsc::channel(16);
    let mut maker = MakerActorState::default();
    maker.upsert_quote(
        "resting".into(),
        trade.market_id.clone(),
        "test-token".into(),
        QuoteSide::Bid,
        0.50,
        2.0,
    );
    maker.stand_down(&trade.market_id);
    let bbo = test_bbo(
        &trade.market_id,
        "test-token",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(200.0),
    );
    maker.update_bbo(bbo.clone());
    // No CLOB client: live cancellation is unavailable, so the quote must
    // stay tracked while the market stays entry-blocked. No network call.
    {
        let mut cfg = bot.control_plane.config.write().await;
        cfg.mode = "live".into();
        cfg.maker_live_quoting = true;
        cfg.auto_trading = true;
    }
    process_maker_bbo_event(&bot, &mut maker, &bbo, &outbox_tx).await;
    assert!(maker.quotes.contains_key("resting"));
    assert_eq!(
        maker.model_by_token["test-token"].decision,
        "manual_close_cancel_pending"
    );
    // Inject the eventual venue acknowledgement through the same cleanup
    // worker: only this acknowledgement may remove the resting order.
    cancel_quotes_in_with(
        &bot,
        &mut maker,
        Some(&trade.market_id),
        "manual_close",
        &outbox_tx,
        |_| std::future::ready(true),
    )
    .await;
    process_maker_bbo_event(&bot, &mut maker, &bbo, &outbox_tx).await;
    assert!(maker.quotes.is_empty());
    assert_eq!(maker.model_by_token["test-token"].decision, "manual_close");
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn paper_bid_small_partial_tranches_share_one_admitted_position_slot() {
    let mut cfg = test_config();
    cfg.min_trade_usd = 1.0;
    cfg.max_open_positions = 1;
    cfg.maker_min_quote_lifetime_ms = 0;
    cfg.maker_sim_max_fill_fraction = 0.10;
    let pool = db::open(":memory:").await.unwrap();
    let bot = TradingBot::new(cfg.clone(), pool).await.unwrap();
    *bot.analysis_state.last_analysis.write().await = json!({
        "market_data": {
            "market_id":"partial-entry", "question":"partial entry",
            "price":0.50, "liquidity":10000.0, "end_date":"2099-01-01T00:00:00Z"
        },
        "p_up":0.60, "confidence":0.62, "net_ev":0.01
    });
    bot.mark_analysis_fresh().await;
    let mut maker = MakerActorState::default();
    let order_id = "sim-mkr-small-bid";
    admit_simulated_maker_order(
        &bot,
        &cfg,
        "partial-entry",
        "partial-token",
        Some(true),
        0.50,
        5.0,
        order_id,
    )
    .await
    .unwrap();
    maker
        .upsert_quote(
            order_id.into(),
            "partial-entry".into(),
            "partial-token".into(),
            QuoteSide::Bid,
            0.50,
            5.0,
        )
        .unwrap();
    let (outbox_tx, _outbox_rx) = mpsc::channel(32);
    for sequence in 1..=9 {
        let mut bbo = test_bbo(
            "partial-entry",
            "partial-token",
            Some(true),
            Some(0.40),
            Some(0.49),
            Some(100.0),
        );
        bbo.sequence = Some(sequence);
        bbo.updated_at = format!("2026-03-09T00:00:{sequence:02}Z");
        bbo.best_ask_size = Some(100.0 + sequence as f64);
        let quote = maker.quotes[order_id].clone();
        assert!(
            simulate_passive_fill_for_quote(
                &bot,
                &mut maker,
                &cfg,
                quote,
                &bbo,
                &MakerMarketContext::default(),
                &outbox_tx
            )
            .await
        );
    }
    let trades = db::load_open_trades(&bot.db).await.unwrap();
    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].shares, 4.5);
    assert_eq!(trades[0].amount, 2.25);
    assert_eq!(maker.quotes[order_id].size, 0.5);
    assert!((bot.balance().await - (cfg.initial_balance - 2.25)).abs() < 1e-9);
    assert_eq!(
        db::load_trade_fills(&bot.db, trades[0].id, 20)
            .await
            .unwrap()
            .len(),
        9
    );
    // A final small remainder belongs to the admitted lot, while an
    // unadmitted quote still fails the normal economic/position guards.
    assert!(evaluate_maker_entry(
        &bot,
        &cfg,
        "partial-entry",
        Direction::BuyYes,
        0.5,
        0.25,
        Some(order_id)
    )
    .await
    .is_ok());
    assert!(evaluate_maker_entry(
        &bot,
        &cfg,
        "partial-entry",
        Direction::BuyYes,
        0.5,
        0.25,
        None
    )
    .await
    .is_err());
    assert!(admit_simulated_maker_order(
        &bot,
        &cfg,
        "partial-entry",
        "partial-token",
        Some(true),
        0.50,
        5.0,
        "sim-mkr-another-position"
    )
    .await
    .is_err());
    bot.db.close().await;
}

#[tokio::test]
async fn paper_fill_respects_placement_provenance_after_a_mode_change() {
    let (bot, trade, path) = crate::bot::close::tests::maker_exit_fixture(20.0).await;
    let mut cfg = test_config();
    cfg.maker_hold_positions = false;
    cfg.maker_min_quote_lifetime_ms = 0;
    let (outbox_tx, _outbox_rx) = mpsc::channel(32);
    let mut maker = MakerActorState::default();
    let bbo = test_bbo(
        &trade.market_id,
        "test-token",
        Some(true),
        Some(0.61),
        Some(0.62),
        Some(100.0),
    );
    for (mode, order_id) in [
        ("simulation", "venue-order"),
        ("live", "sim-mkr-provenance"),
    ] {
        cfg.mode = mode.into();
        let quote = maker
            .upsert_quote(
                order_id.into(),
                trade.market_id.clone(),
                "test-token".into(),
                QuoteSide::Ask,
                0.60,
                20.0,
            )
            .unwrap();
        assert!(
            !simulate_passive_fill_for_quote(
                &bot,
                &mut maker,
                &cfg,
                quote,
                &bbo,
                &MakerMarketContext::default(),
                &outbox_tx,
            )
            .await
        );
        assert_eq!(db::load_open_trades(&bot.db).await.unwrap()[0].shares, 20.0);
        assert!(maker.paper_fill_state.is_empty());
    }
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[tokio::test]
async fn paper_maker_partial_fill_retains_remainder_and_cannot_reuse_a_snapshot() {
    let (bot, trade, path) = crate::bot::close::tests::maker_exit_fixture(20.0).await;
    let mut cfg = test_config();
    cfg.maker_hold_positions = false;
    cfg.maker_min_quote_lifetime_ms = 0;
    cfg.sim_order_latency_ms = 0;
    cfg.sim_available_depth_fraction = 0.25;
    cfg.maker_sim_queue_ahead_shares = 5.0;
    cfg.maker_sim_max_fill_fraction = 0.25;
    let (outbox_tx, _outbox_rx) = mpsc::channel(32);
    let mut maker = MakerActorState::default();
    let quote = maker
        .upsert_quote(
            "sim-mkr-partial".into(),
            trade.market_id.clone(),
            "test-token".into(),
            QuoteSide::Ask,
            0.60,
            20.0,
        )
        .unwrap();
    let mut bbo = test_bbo(
        &trade.market_id,
        "test-token",
        Some(true),
        Some(0.61),
        Some(0.62),
        Some(100.0),
    );
    let context = MakerMarketContext::default();
    assert!(
        simulate_passive_fill_for_quote(
            &bot,
            &mut maker,
            &cfg,
            quote.clone(),
            &bbo,
            &context,
            &outbox_tx
        )
        .await
    );
    let remainder = maker.quotes[&quote.order_id].clone();
    assert_eq!(remainder.size, 15.0);
    assert_eq!(remainder.status, "PARTIALLY_FILLED");
    assert_eq!(db::load_open_trades(&bot.db).await.unwrap()[0].shares, 15.0);
    assert!(
        !simulate_passive_fill_for_quote(
            &bot,
            &mut maker,
            &cfg,
            remainder.clone(),
            &bbo,
            &context,
            &outbox_tx
        )
        .await
    );
    bbo.sequence = Some(2);
    bbo.updated_at = "2026-03-09T00:00:01Z".into();
    assert!(
        !simulate_passive_fill_for_quote(
            &bot,
            &mut maker,
            &cfg,
            remainder.clone(),
            &bbo,
            &context,
            &outbox_tx
        )
        .await
    );
    bbo.sequence = Some(3);
    bbo.updated_at = "2026-03-09T00:00:02Z".into();
    bbo.best_bid_size = Some(120.0);
    assert!(
        simulate_passive_fill_for_quote(
            &bot, &mut maker, &cfg, remainder, &bbo, &context, &outbox_tx
        )
        .await
    );
    assert_eq!(maker.quotes[&quote.order_id].size, 10.0);
    assert_eq!(db::load_open_trades(&bot.db).await.unwrap()[0].shares, 10.0);
    assert!(maker.remove_quote(&quote.order_id));
    assert!(!maker.paper_fill_state.contains_key(&quote.order_id));
    crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
}

#[test]
fn a_maker_quote_fills_on_trade_through_not_on_touch() {
    let bid_quote = QuoteState {
        order_id: "b".into(),
        market_id: "m".into(),
        token_id: "t".into(),
        side: QuoteSide::Bid,
        price: 0.50,
        size: 1.0,
        sequence: 1,
        status: "OPEN".into(),
        updated_at: String::new(),
    };
    let touched = test_bbo("m", "t", Some(true), Some(0.49), Some(0.50), Some(100.0));
    let through = test_bbo("m", "t", Some(true), Some(0.48), Some(0.49), Some(100.0));
    let untouched = test_bbo("m", "t", Some(true), Some(0.50), Some(0.51), Some(100.0));

    assert!(
        !quote_crosses_top_of_book(&bid_quote, &touched, false),
        "touch must not fill"
    );
    assert!(
        quote_crosses_top_of_book(&bid_quote, &through, false),
        "through must fill"
    );
    assert!(!quote_crosses_top_of_book(&bid_quote, &untouched, false));
    // The optimistic rule, kept for comparison against the old record.
    assert!(quote_crosses_top_of_book(&bid_quote, &touched, true));

    let ask_quote = QuoteState {
        side: QuoteSide::Ask,
        price: 0.52,
        ..bid_quote.clone()
    };
    let ask_touched = test_bbo("m", "t", Some(true), Some(0.52), Some(0.53), Some(100.0));
    let ask_through = test_bbo("m", "t", Some(true), Some(0.53), Some(0.54), Some(100.0));
    assert!(!quote_crosses_top_of_book(&ask_quote, &ask_touched, false));
    assert!(quote_crosses_top_of_book(&ask_quote, &ask_through, false));
}

#[tokio::test]
async fn process_maker_bbo_event_crossed_ask_closes_existing_inventory() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_min_quote_lifetime_ms = 0;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_fill_close_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "maker fill close",
                "price": 0.50,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.50,
            "confidence": 0.62,
            "net_ev": 0.01
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());

    let first_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    maker_state.update_bbo(first_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &first_bbo, &outbox_tx).await;

    let crossed_bid_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.46),
        Some(0.47),
        Some(50.0),
    );
    maker_state.update_bbo(crossed_bid_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed_bid_bbo, &outbox_tx).await;

    let ask_quote = maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Ask)
        .cloned()
        .expect("ask quote after bid fill");
    let crossed_ask_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some((ask_quote.price + 0.02).min(0.98)),
        Some((ask_quote.price + 0.03).min(0.99)),
        Some(100.0),
    );
    maker_state.update_bbo(crossed_ask_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed_ask_bbo, &outbox_tx).await;
    sleep(Duration::from_millis(100)).await;

    let open_trades = db::load_open_trades(&bot.db).await.expect("open trades");
    assert!(
        open_trades.is_empty(),
        "expected crossed maker ask to flatten existing inventory, found {open_trades:?}"
    );
    let closed_trades = db::load_closed_trades(&bot.db)
        .await
        .expect("closed trades");
    assert_eq!(closed_trades.len(), 1, "expected closed maker trade");
    assert_eq!(closed_trades[0].direction, "BUY_YES");
    assert_eq!(closed_trades[0].exit_type.as_deref(), Some("maker_flatten"));

    let fills = db::load_trade_fills(&bot.db, closed_trades[0].id, 10)
        .await
        .expect("trade fills");
    assert!(
        fills
            .iter()
            .any(|f| f.phase == "exit" && f.source == "maker_flatten"),
        "expected maker flatten exit snapshot"
    );
    let exit_fill = fills
        .iter()
        .find(|f| f.phase == "exit" && f.source == "maker_flatten")
        .expect("maker flatten fill");
    let details: serde_json::Value =
        serde_json::from_str(&exit_fill.details).expect("json verification details");
    assert_eq!(
        details.get("path").and_then(serde_json::Value::as_str),
        Some("maker_flatten")
    );
    assert!(
        details
            .get("snapshot")
            .and_then(|v| v.get("source"))
            .and_then(serde_json::Value::as_str)
            .is_some(),
        "expected verification snapshot source"
    );

    let fill_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote' AND action='fill'",
    )
    .fetch_one(&bot.db)
    .await
    .expect("fill outbox count");
    assert!(
        fill_count >= 2,
        "expected both the opening and closing simulated maker fills to be audited"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_crossed_ask_without_inventory_does_not_open_complement() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_min_quote_lifetime_ms = 0;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_fill_no_complement_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "maker no complement",
                "price": 0.50,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.50,
            "confidence": 0.62,
            "net_ev": 0.01
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());

    let first_bbo = test_bbo(
        "mkt-1",
        "tok-no",
        Some(false),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    maker_state.update_bbo(first_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &first_bbo, &outbox_tx).await;

    let crossed_ask_bbo = test_bbo(
        "mkt-1",
        "tok-no",
        Some(false),
        Some(0.99),
        Some(0.99),
        Some(100.0),
    );
    assert!(maker_state
        .quote_for("mkt-1", "tok-no", &QuoteSide::Ask)
        .is_none());
    maker_state.update_bbo(crossed_ask_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed_ask_bbo, &outbox_tx).await;
    sleep(Duration::from_millis(80)).await;

    let trades_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trades")
        .fetch_one(&bot.db)
        .await
        .expect("trade count");
    assert_eq!(
        trades_total, 0,
        "ask-side maker fill without matching inventory should stay conservative"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_waits_for_min_quote_lifetime_before_fill() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_min_quote_lifetime_ms = 1_000;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_fill_min_lifetime_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "maker fill min lifetime",
                "price": 0.50,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.50,
            "confidence": 0.62,
            "net_ev": 0.01
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());

    let first_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    maker_state.update_bbo(first_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &first_bbo, &outbox_tx).await;

    let crossed_bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.48),
        Some(0.485),
        Some(50.0),
    );
    maker_state.update_bbo(crossed_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &crossed_bbo, &outbox_tx).await;

    let open_trades = db::load_open_trades(&bot.db).await.expect("open trades");
    assert!(
        open_trades.is_empty(),
        "quote should not fill before min lifetime"
    );

    let bid_quote = maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
        .cloned()
        .expect("resting bid quote");
    maker_state.quote_birth.insert(
        bid_quote.order_id.clone(),
        Instant::now() - Duration::from_millis(1_500),
    );

    // Admission cannot retroactively consume the same early crossing.
    // A new observation with replenished contra depth is a new opportunity.
    let mut admitted_bbo = crossed_bbo.clone();
    admitted_bbo.sequence = Some(2);
    admitted_bbo.updated_at = "2026-03-09T00:00:01Z".into();
    admitted_bbo.best_ask_size = Some(200.0);
    maker_state.update_bbo(admitted_bbo.clone());
    process_maker_bbo_event(&bot, &mut maker_state, &admitted_bbo, &outbox_tx).await;
    sleep(Duration::from_millis(80)).await;

    let open_trades = db::load_open_trades(&bot.db).await.expect("open trades");
    assert_eq!(open_trades.len(), 1, "expected fill after quote aged");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_basis_guard_skips_when_enabled() {
    let mut cfg = test_config();
    cfg.maker_backwardation_guard_enabled = true;
    cfg.maker_backwardation_guard_shadow_only = false;
    cfg.maker_backwardation_guard_pct = -0.02;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_basis_guard_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.54,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.54,
            "confidence": 0.70,
            "net_ev": 0.02,
            "price_to_beat_distance_pct": 0.12
        });
    }
    {
        let mut futures = bot.market_state.futures_cache.lock().await;
        *futures = Some(FuturesCache {
            ts: Instant::now(),
            funding_rate: None,
            open_interest: None,
            prev_open_interest: None,
            basis_pct: Some(-0.05),
            mark_price: Some(100.0),
            index_price: Some(100.05),
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;
    sleep(Duration::from_millis(60)).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
        .is_none());
    let telemetry_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote_telemetry' AND action='guard_skip'",
        )
        .fetch_one(&bot.db)
        .await
        .expect("guard telemetry count");
    assert_eq!(telemetry_count, 2, "expected bid/ask guard skip telemetry");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_market_quality_gate_skips_wide_books() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();
    cfg.maker_market_quality_max_spread_bps = 120.0;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_market_quality_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.54,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.54,
            "confidence": 0.70,
            "net_ev": 0.02
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(180.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
        .is_none());
    let model = maker_state
        .model_by_token
        .get("tok-yes")
        .expect("model snapshot");
    assert_eq!(model.bid_action, "suppressed");
    assert!(model
        .bid_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("market_quality(")));
    assert_eq!(model.ask_action, "suppressed");
    assert!(model
        .ask_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("market_quality(")));

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_basis_guard_shadow_keeps_quotes() {
    let mut cfg = test_config();
    cfg.maker_backwardation_guard_enabled = true;
    cfg.maker_backwardation_guard_shadow_only = true;
    cfg.maker_backwardation_guard_pct = -0.02;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_basis_guard_shadow_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.54,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.54,
            "confidence": 0.70,
            "net_ev": 0.02,
            "price_to_beat_distance_pct": 0.12
        });
    }
    {
        let mut futures = bot.market_state.futures_cache.lock().await;
        *futures = Some(FuturesCache {
            ts: Instant::now(),
            funding_rate: None,
            open_interest: None,
            prev_open_interest: None,
            basis_pct: Some(-0.05),
            mark_price: Some(100.0),
            index_price: Some(100.05),
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;
    sleep(Duration::from_millis(60)).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
        .is_some());
    let model = maker_state
        .model_by_token
        .get("tok-yes")
        .expect("model snapshot");
    assert_eq!(model.bid_action, "placed");
    assert_eq!(model.ask_action, "suppressed");
    assert_eq!(model.ask_reason.as_deref(), Some("no_inventory"));
    let shadow_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote_telemetry' AND action='guard_shadow'",
        )
        .fetch_one(&bot.db)
        .await
        .expect("guard shadow telemetry count");
    assert_eq!(shadow_count, 2, "expected bid/ask shadow telemetry");

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_roll_guard_skips_toxic_side_only() {
    let mut cfg = test_config();
    cfg.maker_roll_toxic_guard_enabled = true;
    cfg.maker_roll_toxic_guard_shadow_only = false;
    cfg.maker_roll_toxic_min_confidence = 0.10;
    cfg.maker_roll_toxic_min_signal = 0.05;
    cfg.maker_roll_toxic_min_sample_count = 4;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_roll_guard_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.54,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.54,
            "confidence": 0.70,
            "net_ev": 0.02,
            "price_to_beat_distance_pct": 0.12
        });
    }

    let mut maker_state = MakerActorState::default();
    maker_state.update_inventory("mkt-1".to_string(), 1.0, 0.0, 0.54);
    maker_state.roll_by_token.insert(
        "tok-yes".to_string(),
        RollMicrostructureState {
            market_id: "mkt-1".to_string(),
            token_id: "tok-yes".to_string(),
            is_yes_token: Some(true),
            signal: 0.30,
            confidence: 0.25,
            persistence: 0.40,
            serial_covariance_bps2: 14.0,
            recent_delta_bps: 22.0,
            sample_count: 6,
            updated_at: Utc::now().to_rfc3339(),
        },
    );
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;
    sleep(Duration::from_millis(60)).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Ask)
        .is_none());
    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Bid)
        .is_some());
    let model = maker_state
        .model_by_token
        .get("tok-yes")
        .expect("model snapshot");
    assert_eq!(model.bid_action, "placed");
    assert_eq!(model.ask_action, "suppressed");
    assert!(model
        .ask_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("roll_guard(")));
    let telemetry_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote_telemetry' AND action='guard_skip'",
        )
        .fetch_one(&bot.db)
        .await
        .expect("guard telemetry count");
    assert_eq!(
        telemetry_count, 1,
        "expected toxic-side-only guard skip telemetry"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn process_maker_bbo_event_roll_guard_shadow_keeps_toxic_side() {
    let mut cfg = test_config();
    cfg.maker_roll_toxic_guard_enabled = true;
    cfg.maker_roll_toxic_guard_shadow_only = true;
    cfg.maker_roll_toxic_min_confidence = 0.10;
    cfg.maker_roll_toxic_min_signal = 0.05;
    cfg.maker_roll_toxic_min_sample_count = 4;

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_roll_guard_shadow_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.54,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.54,
            "confidence": 0.70,
            "net_ev": 0.02,
            "price_to_beat_distance_pct": 0.12
        });
    }

    let mut maker_state = MakerActorState::default();
    maker_state.update_inventory("mkt-1".to_string(), 1.0, 0.0, 0.54);
    maker_state.roll_by_token.insert(
        "tok-yes".to_string(),
        RollMicrostructureState {
            market_id: "mkt-1".to_string(),
            token_id: "tok-yes".to_string(),
            is_yes_token: Some(true),
            signal: 0.30,
            confidence: 0.25,
            persistence: 0.40,
            serial_covariance_bps2: 14.0,
            recent_delta_bps: 22.0,
            sample_count: 6,
            updated_at: Utc::now().to_rfc3339(),
        },
    );
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;
    sleep(Duration::from_millis(60)).await;

    assert!(maker_state
        .quote_for("mkt-1", "tok-yes", &QuoteSide::Ask)
        .is_some());
    let shadow_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote_telemetry' AND action='guard_shadow'",
        )
        .fetch_one(&bot.db)
        .await
        .expect("guard shadow telemetry count");
    assert_eq!(
        shadow_count, 1,
        "expected toxic-side-only roll guard shadow telemetry"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn cancel_all_quotes_emits_cancel_telemetry() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();

    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_cancel_telemetry_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await.expect("sqlite pool");
    let bot = TradingBot::new(cfg, pool).await.expect("bot");
    bot.start_loop();
    bot.set_l2_feed_stale(false).await;
    {
        let mut last = bot.analysis_state.last_analysis.write().await;
        *last = json!({
            "market_data": {
                "market_id": "mkt-1",
                "price": 0.54,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.54,
            "confidence": 0.70,
            "net_ev": 0.02
        });
    }

    let mut maker_state = MakerActorState::default();
    let outbox_tx = spawn_outbox_writer(bot.db.clone());
    let bbo = test_bbo(
        "mkt-1",
        "tok-yes",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    process_maker_bbo_event(&bot, &mut maker_state, &bbo, &outbox_tx).await;
    sleep(Duration::from_millis(60)).await;

    let removed = cancel_all_quotes(&bot, &mut maker_state, "test_cleanup", &outbox_tx).await;
    assert_eq!(removed, 1);
    sleep(Duration::from_millis(60)).await;

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT details FROM order_outbox WHERE stage='maker_quote_telemetry' AND action='cancel'",
    )
    .fetch_all(&bot.db)
    .await
    .expect("cancel telemetry rows");
    assert_eq!(
        rows.len(),
        1,
        "expected cancel telemetry for the quoted side"
    );
    assert!(
        rows.iter()
            .all(|(details,)| details.contains("\"reason\":\"test_cleanup\"")),
        "expected cancel telemetry to preserve cleanup reason"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
}

#[tokio::test]
async fn stale_command_fail_closes_existing_quotes() {
    let mut cfg = test_config();
    cfg.execution_mode = "maker".to_string();

    let pool = db::open(":memory:").await.expect("sqlite memory pool");
    let bot = Arc::new(TradingBot::new(cfg, pool).await.expect("bot"));
    bot.start_execution_actor().await;

    let up = bot
        .maker_upsert_quote_state(
            "sim-mkr-stale-1".to_string(),
            "mkt-1".to_string(),
            "tok-1".to_string(),
            QuoteSide::Bid,
            0.50,
            1.0,
        )
        .await;
    assert!(up.is_some());

    let before = bot.maker_state_snapshot().await;
    assert_eq!(before.get("quote_count").and_then(|v| v.as_u64()), Some(1));

    let sent = bot
        .maker_publish_l2_stale_with_code(true, Some("test_stale"), Some("test_stale".to_string()))
        .await;
    assert!(sent);

    // Wait briefly for actor command processing.
    sleep(Duration::from_millis(60)).await;

    let after = bot.maker_state_snapshot().await;
    assert_eq!(after.get("quote_count").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(after.get("l2_stale").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        after.get("l2_stale_reason").and_then(|v| v.as_str()),
        Some("test_stale")
    );
}

#[test]
fn maker_state_snapshot_summarizes_l2_readiness_and_not_ready_reasons() {
    let mut st = MakerActorState::default();
    st.by_market
        .entry("mkt-a".to_string())
        .or_default()
        .insert("oid-a".to_string());
    st.by_market
        .entry("mkt-b".to_string())
        .or_default()
        .insert("oid-b".to_string());

    st.l2_bbo_by_token.insert(
        "tok-ready".to_string(),
        test_top_of_book(
            "mkt-a",
            "tok-ready",
            Some(true),
            Some(0.49),
            Some(0.51),
            true,
            false,
        ),
    );
    st.l2_bbo_by_token.insert(
        "tok-stale".to_string(),
        test_top_of_book(
            "mkt-a",
            "tok-stale",
            Some(false),
            Some(0.48),
            Some(0.52),
            true,
            true,
        ),
    );
    st.l2_bbo_by_token.insert(
        "tok-missing".to_string(),
        test_top_of_book(
            "mkt-b",
            "tok-missing",
            Some(true),
            Some(0.47),
            Some(0.53),
            false,
            false,
        ),
    );
    st.l2_bbo_by_token.insert(
        "tok-one-sided".to_string(),
        test_top_of_book(
            "mkt-b",
            "tok-one-sided",
            Some(true),
            Some(0.46),
            None,
            true,
            false,
        ),
    );

    let snap = st.snapshot();
    let l2 = snap.get("l2_summary").expect("l2 summary");

    assert_eq!(
        l2.get("target_market_count").and_then(|v| v.as_u64()),
        Some(2)
    );
    assert_eq!(l2.get("token_count").and_then(|v| v.as_u64()), Some(4));
    assert_eq!(
        l2.get("snapshot_loaded_count").and_then(|v| v.as_u64()),
        Some(3)
    );
    assert_eq!(l2.get("two_sided_count").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(l2.get("one_sided_count").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(l2.get("stale_count").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(l2.get("ready_count").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(l2.get("quoteable_count").and_then(|v| v.as_u64()), Some(1));
    assert_eq!(l2.get("not_ready_count").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(
        l2.get("primary_not_ready_reason").and_then(|v| v.as_str()),
        Some("one_sided_book")
    );

    let reasons = l2
        .get("not_ready_reasons")
        .and_then(|v| v.as_array())
        .expect("not ready reasons");
    let mut reason_map = std::collections::HashMap::new();
    for reason in reasons {
        let label = reason.get("label").and_then(|v| v.as_str()).unwrap_or("");
        let count = reason.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        reason_map.insert(label.to_string(), count);
    }
    assert_eq!(
        reason_map.get("token_bbo_stale").copied(),
        Some(1),
        "stale token should be counted"
    );
    assert_eq!(
        reason_map.get("snapshot_missing").copied(),
        Some(1),
        "missing snapshot should be counted"
    );
    assert_eq!(
        reason_map.get("one_sided_book").copied(),
        Some(1),
        "one-sided book should be counted"
    );
}

#[test]
fn maker_state_snapshot_uses_global_l2_stale_reason_for_all_tokens() {
    let mut st = MakerActorState::default();
    st.l2_stale = true;
    st.l2_stale_reason = Some("no target markets".to_string());
    st.l2_bbo_by_token.insert(
        "tok-a".to_string(),
        test_top_of_book(
            "mkt-a",
            "tok-a",
            Some(true),
            Some(0.49),
            Some(0.51),
            true,
            false,
        ),
    );
    st.l2_bbo_by_token.insert(
        "tok-b".to_string(),
        test_top_of_book(
            "mkt-b",
            "tok-b",
            Some(false),
            Some(0.48),
            Some(0.52),
            true,
            false,
        ),
    );

    let snap = st.snapshot();
    assert_eq!(
        snap.get("l2_stale_code").and_then(|v| v.as_str()),
        Some("no_target_markets")
    );
    let l2 = snap.get("l2_summary").expect("l2 summary");
    assert_eq!(l2.get("ready_count").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(l2.get("quoteable_count").and_then(|v| v.as_u64()), Some(0));
    assert_eq!(l2.get("not_ready_count").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(
        l2.get("primary_not_ready_reason").and_then(|v| v.as_str()),
        Some("l2_stale(no target markets)")
    );

    let reasons = l2
        .get("not_ready_reasons")
        .and_then(|v| v.as_array())
        .expect("not ready reasons");
    assert_eq!(reasons.len(), 1);
    assert_eq!(
        reasons[0].get("label").and_then(|v| v.as_str()),
        Some("l2_stale(no target markets)")
    );
    assert_eq!(reasons[0].get("count").and_then(|v| v.as_u64()), Some(2));
}

#[tokio::test]
async fn outbox_writer_failure_increments_metric() -> anyhow::Result<()> {
    let pool = SqlitePool::connect("sqlite::memory:").await?;
    // Force writer failures by closing the pool before background insert attempts.
    pool.close().await;
    let tx = spawn_outbox_writer(pool.clone());
    let before = crate::metrics::snapshot().outbox_write_fail;

    outbox_emit(
        &tx,
        &pool,
        OutboxEvent {
            stage: "maker_quote".to_string(),
            market_id: "mkt-test".to_string(),
            order_id: Some("oid-test".to_string()),
            token_id: Some("tok-test".to_string()),
            action: "place".to_string(),
            status: "ok".to_string(),
            details: "failure-injection".to_string(),
            priority: OutboxPriority::Critical,
        },
    );

    sleep(Duration::from_millis(80)).await;
    let after = crate::metrics::snapshot().outbox_write_fail;
    assert!(after >= before.saturating_add(1));
    Ok(())
}

#[tokio::test]
async fn cancel_quote_without_durable_intent_records_error_audit() -> anyhow::Result<()> {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_cancel_missing_clob_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await?;

    let mut cfg = test_config();
    cfg.mode = "live".to_string();
    cfg.maker_live_quoting = true;
    let bot = TradingBot::new(cfg.clone(), pool.clone()).await?;
    assert!(bot.clob.is_none(), "test expects missing live clob client");

    let outbox_tx = spawn_outbox_writer(pool.clone());
    let ok = cancel_quote(&bot, "oid-missing", "mkt-1", "tok-1", "replace", &outbox_tx).await;
    assert!(!ok);

    sleep(Duration::from_millis(60)).await;
    let row: (String, String, String) = sqlx::query_as(
            "SELECT action, status, details FROM order_outbox WHERE order_id='oid-missing' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.0, "cancel");
    assert_eq!(row.1, "error");
    assert!(row.2.contains("intent is absent"));
    assert!(bot.kill_switch_active());

    drop(bot);
    drop(pool);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
    Ok(())
}

#[tokio::test]
async fn place_quote_without_clob_records_error_audit() -> anyhow::Result<()> {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_place_missing_clob_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await?;

    let mut cfg = test_config();
    cfg.mode = "live".to_string();
    cfg.maker_live_quoting = true;
    cfg.shadow_trading = false;
    let bot = TradingBot::new(cfg.clone(), pool.clone()).await?;
    assert!(bot.clob.is_none(), "test expects missing live clob client");

    let outbox_tx = spawn_outbox_writer(pool.clone());
    let order_id = place_quote(
        &bot,
        "mkt-1",
        "tok-1",
        &QuoteSide::Bid,
        0.50,
        1.0,
        &cfg,
        Some(true),
        1,
        &outbox_tx,
    )
    .await;
    assert!(order_id.is_none());

    sleep(Duration::from_millis(60)).await;
    let row: (String, String, String) = sqlx::query_as(
            "SELECT action, status, details FROM order_outbox WHERE stage='maker_quote' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;
    assert_eq!(row.0, "place");
    assert_eq!(row.1, "error");
    assert!(row.2.contains("requires exchange credentials"));

    drop(bot);
    drop(pool);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
    Ok(())
}

#[tokio::test]
async fn replace_path_keeps_existing_quote_when_cancel_fails() -> anyhow::Result<()> {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_replace_fail_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await?;

    let mut cfg = test_config();
    cfg.mode = "live".to_string();
    cfg.maker_live_quoting = true;
    cfg.maker_min_quote_lifetime_ms = 0;
    cfg.maker_replace_cooldown_ms = 0;
    let bot = TradingBot::new(cfg.clone(), pool.clone()).await?;
    assert!(bot.clob.is_none(), "test expects missing live clob client");

    let mut maker_state = MakerActorState::default();
    let existing = maker_state
        .upsert_quote(
            "oid-existing".to_string(),
            "mkt-1".to_string(),
            "tok-1".to_string(),
            QuoteSide::Bid,
            0.50,
            1.0,
        )
        .expect("existing quote");
    maker_state.quote_birth.insert(
        existing.order_id.clone(),
        Instant::now() - Duration::from_secs(10),
    );
    let bbo = test_bbo(
        "mkt-1",
        "tok-1",
        Some(true),
        Some(0.49),
        Some(0.51),
        Some(100.0),
    );
    maker_state.update_bbo(bbo.clone());
    let plan = build_quote_plan(
        &cfg,
        &bbo,
        &VenueRules::default(),
        0.55,
        0.0,
        &MakerMarketContext::default(),
    )
    .expect("quote plan");
    let market_ctx = MakerMarketContext::default();

    let outbox_tx = spawn_outbox_writer(pool.clone());
    let mut decisions: Vec<String> = Vec::new();
    sync_quote_side(
        &bot,
        &mut maker_state,
        &cfg,
        "mkt-1",
        "tok-1",
        &QuoteSide::Bid,
        0.55,
        1.0,
        &bbo,
        &plan,
        &market_ctx,
        &mut decisions,
        &outbox_tx,
        ExecutionOrigin::Manual,
    )
    .await;

    let current = maker_state
        .quote_for("mkt-1", "tok-1", &QuoteSide::Bid)
        .expect("existing quote should remain");
    assert_eq!(current.order_id, "oid-existing");
    assert!(decisions.iter().any(|d| d.contains("cancel_failed")));

    drop(bot);
    drop(pool);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
    Ok(())
}

#[tokio::test]
async fn critical_outbox_event_direct_writes_when_queue_is_full() -> anyhow::Result<()> {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_outbox_full_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await?;

    let (tx, _rx) = mpsc::channel::<OutboxEvent>(1);
    tx.try_send(OutboxEvent {
        stage: "filler".to_string(),
        market_id: "mkt-fill".to_string(),
        order_id: Some("oid-fill".to_string()),
        token_id: Some("tok-fill".to_string()),
        action: "noop".to_string(),
        status: "ok".to_string(),
        details: "fill queue".to_string(),
        priority: OutboxPriority::BestEffort,
    })
    .expect("queue filler");

    let before = crate::metrics::snapshot().outbox_enqueued;
    outbox_emit(
        &tx,
        &pool,
        OutboxEvent {
            stage: "reconcile_test".to_string(),
            market_id: "mkt-test".to_string(),
            order_id: Some("oid-critical".to_string()),
            token_id: Some("tok-test".to_string()),
            action: "cancel_order".to_string(),
            status: "error".to_string(),
            details: "critical fallback".to_string(),
            priority: OutboxPriority::Critical,
        },
    );

    sleep(Duration::from_millis(80)).await;

    let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_outbox WHERE stage='reconcile_test' AND order_id='oid-critical'",
        )
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        count, 1,
        "critical event should persist via direct-write fallback"
    );
    let after = crate::metrics::snapshot().outbox_enqueued;
    assert!(after >= before.saturating_add(1));

    pool.close().await;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
    Ok(())
}

#[tokio::test]
async fn best_effort_outbox_event_drops_when_queue_is_full() -> anyhow::Result<()> {
    let uniq = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = std::env::temp_dir().join(format!("macbot_actor_outbox_drop_{uniq}.sqlite"));
    let path_str = path.to_string_lossy().to_string();
    let pool = db::open(&path_str).await?;

    let (tx, _rx) = mpsc::channel::<OutboxEvent>(1);
    tx.try_send(OutboxEvent {
        stage: "filler".to_string(),
        market_id: "mkt-fill".to_string(),
        order_id: Some("oid-fill".to_string()),
        token_id: Some("tok-fill".to_string()),
        action: "noop".to_string(),
        status: "ok".to_string(),
        details: "fill queue".to_string(),
        priority: OutboxPriority::BestEffort,
    })
    .expect("queue filler");

    let before = crate::metrics::snapshot().outbox_dropped;
    outbox_emit(
        &tx,
        &pool,
        OutboxEvent {
            stage: "maker_quote".to_string(),
            market_id: "mkt-test".to_string(),
            order_id: Some("oid-best-effort".to_string()),
            token_id: Some("tok-test".to_string()),
            action: "place".to_string(),
            status: "ok".to_string(),
            details: "best effort fallback".to_string(),
            priority: OutboxPriority::BestEffort,
        },
    );

    sleep(Duration::from_millis(40)).await;

    let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM order_outbox WHERE stage='maker_quote' AND order_id='oid-best-effort'",
        )
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        count, 0,
        "best-effort event should be dropped when queue is full"
    );
    let after = crate::metrics::snapshot().outbox_dropped;
    assert!(after >= before.saturating_add(1));

    pool.close().await;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{path_str}-shm"));
    let _ = std::fs::remove_file(format!("{path_str}-wal"));
    Ok(())
}
