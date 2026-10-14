use super::*;
use std::time::Duration;

#[test]
fn backtest_clock_starts_after_the_feature_candle_closes_and_rejects_gaps() {
    assert_eq!(
        trade_clock(0, KLINE_15M_MS),
        Some((KLINE_15M_MS, KLINE_15M_MS * 2))
    );
    assert_eq!(trade_clock(0, KLINE_15M_MS * 2), None);
    assert_eq!(trade_clock(i64::MAX, i64::MAX), None);
}

#[test]
fn incomplete_and_invalid_candles_cannot_be_backtest_outcomes() {
    let rows = vec![
        json!([0, "100", "102", "99", "101", "20"]),
        json!([KLINE_15M_MS, "101", "103", "100", "102", "20"]),
        json!([KLINE_15M_MS * 2, "102", "NaN", "101", "103", "20"]),
        json!([KLINE_15M_MS * 3, "103", "100", "104", "105", "20"]),
    ];
    let completed = completed_kline_rows(&rows, KLINE_15M_MS + 1);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].0, 0);
    assert_eq!(parse_kline_rows(&rows).len(), 2);
}

#[test]
fn asymmetric_exit_audit_does_not_invent_resolution_break_even() {
    let mut bucket = Bucket::default();
    bucket.push(true, 1.0, 0.1, 10.0);
    assert!(bucket.as_json(false)["break_even_win_rate_pct"].is_null());
    assert!(bucket.as_json(false)["edge_gap_pp"].is_null());
    assert!(
        bucket.as_json(true)["break_even_win_rate_pct"]
            .as_f64()
            .unwrap()
            > 50.0
    );
}

#[test]
fn anti_bias_strength_is_disabled_for_one_sided_modes() {
    assert!((effective_anti_bias_strength(true, true, 0.14) - 0.14).abs() < 1e-12);
    assert_eq!(effective_anti_bias_strength(true, false, 0.14), 0.0);
    assert_eq!(effective_anti_bias_strength(false, true, 0.14), 0.0);
}

#[test]
fn classify_raw_signal_respects_asymmetric_thresholds() {
    assert_eq!(classify_raw_signal(0.61, 0.60, 0.34), Signal::BuyYes);
    assert_eq!(classify_raw_signal(-0.35, 0.60, 0.34), Signal::BuyNo);
    assert_eq!(classify_raw_signal(0.30, 0.60, 0.34), Signal::Hold);
    assert_eq!(classify_raw_signal(-0.20, 0.60, 0.34), Signal::Hold);
}

#[test]
fn side_filters_block_disabled_directions() {
    let allowlist = HashSet::new();
    let overrides = HashMap::new();
    let (sig, reason) = apply_side_and_regime_filters(
        Signal::BuyYes,
        false,
        true,
        &allowlist,
        &overrides,
        false,
        "TREND_UP",
    );
    assert_eq!(sig, Signal::Hold);
    assert_eq!(reason, Some("side_disabled_yes"));

    let (sig, reason) = apply_side_and_regime_filters(
        Signal::BuyNo,
        true,
        false,
        &allowlist,
        &overrides,
        false,
        "TREND_DOWN",
    );
    assert_eq!(sig, Signal::Hold);
    assert_eq!(reason, Some("side_disabled_no"));
}

#[test]
fn regime_alignment_blocks_mismatched_side() {
    let allowlist = HashSet::new();
    let overrides = HashMap::new();
    let (sig, reason) = apply_side_and_regime_filters(
        Signal::BuyNo,
        true,
        true,
        &allowlist,
        &overrides,
        true,
        "TREND_UP",
    );
    assert_eq!(sig, Signal::Hold);
    assert_eq!(reason, Some("regime_mismatch"));

    let (sig, reason) = apply_side_and_regime_filters(
        Signal::BuyNo,
        true,
        true,
        &allowlist,
        &overrides,
        true,
        "TREND_DOWN",
    );
    assert_eq!(sig, Signal::BuyNo);
    assert_eq!(reason, None);
}

#[test]
fn regime_allowlist_blocks_excluded_regime() {
    let allowlist = HashSet::from([String::from("TREND_UP")]);
    let overrides = HashMap::new();
    let (sig, reason) = apply_side_and_regime_filters(
        Signal::BuyYes,
        true,
        true,
        &allowlist,
        &overrides,
        false,
        "RANGE",
    );
    assert_eq!(sig, Signal::Hold);
    assert_eq!(reason, Some("regime_blocked"));
}

#[test]
fn session_bucket_matches_forensics_convention() {
    assert_eq!(session_bucket(0), "asia");
    assert_eq!(session_bucket(7), "asia");
    assert_eq!(session_bucket(8), "europe");
    assert_eq!(session_bucket(15), "europe");
    assert_eq!(session_bucket(16), "us");
    assert_eq!(session_bucket(23), "us");
}

#[test]
fn time_of_day_score_weights_match_session_profile() {
    let asia = backtest_time_of_day_weights(2);
    let europe = backtest_time_of_day_weights(12);
    let us = backtest_time_of_day_weights(18);

    assert!(asia.technical < europe.technical);
    assert!(europe.technical < us.technical);
    assert!(asia.regime < europe.regime);
    assert!(europe.regime < us.regime);
}

#[test]
fn regime_side_mode_override_blocks_disallowed_side() {
    let allowlist = HashSet::new();
    let overrides = HashMap::from([(String::from("TREND_UP"), SideMode::YesOnly)]);
    let (sig, reason) = apply_side_and_regime_filters(
        Signal::BuyNo,
        true,
        true,
        &allowlist,
        &overrides,
        false,
        "TREND_UP",
    );
    assert_eq!(sig, Signal::Hold);
    assert_eq!(reason, Some("side_disabled_no"));
}

#[test]
fn stand_down_triggers_when_recent_hit_rate_falls_below_break_even_plus_margin() {
    let mut health = VecDeque::new();
    for _ in 0..3 {
        health.push_back(true);
    }
    for _ in 0..7 {
        health.push_back(false);
    }
    assert!(should_trigger_stand_down(&health, 10, 51.5, 0.5));
    assert!(!should_trigger_stand_down(&health, 10, 20.0, 0.0));
}

#[test]
fn cached_backtest_klines_roundtrip_and_prune() {
    let rows = vec![(
        1_700_000_000_000,
        Candle {
            open: 1.0,
            high: 2.0,
            low: 0.5,
            close: 1.5,
            volume: 42.0,
        },
    )];

    store_cached_backtest_klines(10, 1_700_000_000_000, &rows);
    let cached = cached_backtest_klines(10, 1_700_000_000_000).unwrap_or_default();
    assert_eq!(cached.len(), 1);
    assert!((cached[0].1.close - 1.5).abs() < f64::EPSILON);

    BACKTEST_KLINE_CACHE.insert(
        (11, 1_700_000_000_001),
        CachedKlinesEntry {
            stored_at: Instant::now() - Duration::from_secs(BACKTEST_KLINE_CACHE_TTL_SECS + 1),
            rows: Arc::new(rows.clone()),
        },
    );
    prune_backtest_kline_cache();
    assert!(cached_backtest_klines(11, 1_700_000_000_001).is_none());
}

#[test]
fn parse_exit_mode_accepts_expected_values() {
    assert_eq!(parse_exit_mode("next_candle"), Some(ExitMode::NextCandle));
    assert_eq!(parse_exit_mode("resolution"), Some(ExitMode::NextCandle));
    assert_eq!(parse_exit_mode("taker_tp_sl"), Some(ExitMode::TakerTpSl));
    assert_eq!(parse_exit_mode("tp_sl"), Some(ExitMode::TakerTpSl));
    assert_eq!(parse_exit_mode("unknown"), None);
}

#[test]
fn taker_tp_sl_same_candle_uses_conservative_stop_loss() {
    let next = Candle {
        open: 100.0,
        high: 105.0,
        low: 95.0,
        close: 102.0,
        volume: 1_000.0,
    };
    let exit = simulate_binary_trade_exit(
        Signal::BuyYes,
        ExitMode::TakerTpSl,
        0.50,
        100.0,
        &next,
        0.02,
        10.0,
        10_000.0,
        0.0,
        0.0,
        0.02,
        0.12,
        0.08,
    );
    assert_eq!(exit.exit_type, "stop_loss_same_candle");
    assert!((exit.exit_price - 0.46).abs() < 1e-9);
    assert!(exit.exit_fee_cost > 0.0);
    assert_eq!(exit.exit_time_offset_ms, KLINE_15M_MS / 2);
}

#[test]
fn taker_tp_sl_take_profit_can_exit_early() {
    let next = Candle {
        open: 100.0,
        high: 103.0,
        low: 100.5,
        close: 102.5,
        volume: 1_000.0,
    };
    let exit = simulate_binary_trade_exit(
        Signal::BuyYes,
        ExitMode::TakerTpSl,
        0.50,
        100.0,
        &next,
        0.01,
        10.0,
        10_000.0,
        0.0,
        0.0,
        0.02,
        0.12,
        0.08,
    );
    assert_eq!(exit.exit_type, "take_profit");
    assert!(exit.exit_price > 0.50);
    assert!(exit.trade_won);
    assert!(exit.total_cost > 0.02);
}

#[test]
fn synthetic_request_keeps_defaults_and_rejects_ambiguous_or_unsafe_assumptions() {
    let mut req: BacktestRequest = serde_json::from_value(json!({})).unwrap();
    assert_eq!(req.entry_price_yes, 0.5);
    assert_eq!(req.entry_price_no, 0.5);
    assert!(req.validate().is_ok());
    req.gas_usd = f64::NAN;
    assert!(req.validate().unwrap_err().contains("gas_usd"));
    req.gas_usd = 0.02;
    req.maker_fee_pct = Some(f64::INFINITY);
    assert!(req.validate().unwrap_err().contains("maker_fee_pct"));
    req.maker_fee_pct = None;
    for price in [0.0, 1.0, -0.5, f64::NAN, f64::INFINITY] {
        req.entry_price_yes = price;
        assert!(req.validate().is_err());
    }
    req.entry_price_yes = 0.3;
    req.exit_mode = Some("taker_tp_sl".into());
    assert!(req.validate().unwrap_err().contains("anchored at 0.50"));
    req.exit_mode = Some("next_candle".into());
    req.adaptive_stand_down = true;
    assert!(req.validate().unwrap_err().contains("asymmetric-price"));
    req.adaptive_stand_down = false;
    assert!(req.validate().is_ok());
    assert!(serde_json::from_value::<BacktestRequest>(json!({"entry_prcie_yes":0.3})).is_err());
    assert!(serde_json::from_str::<BacktestRequest>("{\"gas_usd\":1e999}").is_err());
    req.session_allowlist = vec!["europe_typo".into()];
    assert!(req.validate().unwrap_err().contains("session"));
    req.session_allowlist.clear();
    req.blocked_hours_utc = vec![24];
    assert!(req.validate().unwrap_err().contains("23"));
    req.blocked_hours_utc.clear();
    req.regime_side_modes = HashMap::from([
        ("BULL".into(), "yes_only".into()),
        ("bull".into(), "no_only".into()),
    ]);
    assert!(req.validate().unwrap_err().contains("duplicate"));
}

#[test]
fn asymmetric_binary_purchases_have_correct_shares_payout_and_stake_return_units() {
    for (signal, price, close) in [(Signal::BuyYes, 0.25, 101.0), (Signal::BuyNo, 0.6, 99.0)] {
        let candle = Candle {
            open: 100.0,
            high: 102.0,
            low: 98.0,
            close,
            volume: 100.0,
        };
        let exit = simulate_binary_trade_exit(
            signal,
            ExitMode::NextCandle,
            price,
            100.0,
            &candle,
            0.01,
            12.0,
            5000.0,
            0.02,
            0.01,
            0.5,
            0.12,
            0.08,
        );
        assert_eq!(exit.exit_price, 1.0);
        let expected = 12.0 / price - 12.0;
        assert!((exit.gross_pnl - expected).abs() < 1e-12);
        assert!((exit.total_cost - 0.86).abs() < 1e-12);
        assert!(exit.trade_won);
    }
    assert!((expected_resolution_return(0.6, 0.5).unwrap() - 0.2).abs() < 1e-12);
    assert!(expected_resolution_return(0.6, 0.9).unwrap() < 0.0);
    assert!(expected_resolution_return(0.6, 0.25).unwrap() > 1.0);
    assert!(expected_resolution_return(f64::NAN, 0.5).is_none());
    assert!(expected_resolution_return(0.6, 0.0).is_none());
}

async fn offline_backtest_bot() -> anyhow::Result<Arc<TradingBot>> {
    let pool = crate::db::open(":memory:").await?;
    let mut cfg: crate::config::Config =
        serde_json::from_str(include_str!("../../../config.json"))?;
    cfg.mode = "simulation".into();
    cfg.min_entry_price = 0.01;
    cfg.max_entry_price = 0.99;
    cfg.min_stake = 0.25;
    cfg.max_stake = 100.0;
    cfg.stake_pct = 0.0;
    cfg.trade_amount = 5.0;
    Ok(Arc::new(TradingBot::new(cfg, pool).await?))
}

fn offline_request(up: bool, price_yes: f64, price_no: f64) -> Value {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let end = 1_577_836_800_000
        + NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) * 1000 * KLINE_15M_MS;
    let count = CANDLES_PER_DAY + 40 + 2;
    let rows: Vec<_> = (0..count)
        .map(|i| {
            let open = 1000.0 * f64::powi(if up { 1.003 } else { 0.997 }, i as i32);
            let close = open * if up { 1.003 } else { 0.997 };
            (
                end - (count - i) as i64 * KLINE_15M_MS,
                Candle {
                    open,
                    close,
                    high: open.max(close) * 1.0001,
                    low: open.min(close) * 0.9999,
                    volume: 100.0,
                },
            )
        })
        .collect();
    store_cached_backtest_klines(count, end, &rows);
    json!({"days":1,"window":40,"end_time_ms":end,"entry_threshold":0.15,"entry_price_yes":price_yes,"entry_price_no":price_no,"anti_bias_strength":0.0,"execution_mode":"maker","maker_slippage_factor":0.0,"maker_fee_pct":0.0,"maker_gas_usd":0.0})
}

#[tokio::test]
async fn api_prices_propagate_independently_without_fabricated_book_paths_or_winrate_edge(
) -> anyhow::Result<()> {
    let bot = offline_backtest_bot().await?;
    for up in [true, false] {
        let result = run_backtest(
            State(bot.clone()),
            Ok(Json(offline_request(up, 0.25, 0.55))),
        )
        .await
        .0;
        assert!(result["error"].is_null(), "{result}");
        assert_eq!(result["mode"], "synthetic_ta");
        assert_eq!(result["result_contract_version"], 2);
        assert_eq!(result["price_assumptions"]["yes"], 0.25);
        assert_eq!(result["price_assumptions"]["no"], 0.55);
        let trades = result["trades"].as_array().unwrap();
        assert!(
            !trades.is_empty(),
            "trend fixture should exercise a trade: {result}"
        );
        let signal = if up { "BUY_YES" } else { "BUY_NO" };
        let price = if up { 0.25 } else { 0.55 };
        for trade in trades {
            assert_eq!(trade["signal"], signal, "{trade}");
            assert_eq!(trade["entry_price"], price);
            assert!(trade["path_low_price"].is_null());
            assert!(trade["path_high_price"].is_null());
            let stake = trade["stake"].as_f64().unwrap();
            assert!((trade["gross_pnl"].as_f64().unwrap() - (stake / price - stake)).abs() < 1e-9);
            let q = trade["heuristic_directional_probability"].as_f64().unwrap();
            assert!(
                (trade["expected_raw_edge"].as_f64().unwrap() - (q / price - 1.0)).abs() < 1e-12
            );
        }
        assert!(result["edge_audit"]["break_even_win_rate_pct"].is_null());
        assert!(result["edge_audit"]["edge_gap_pp"].is_null());
        assert!(result["edge_audit"]["by_signal"][signal]["break_even_win_rate_pct"].is_null());
        assert!(result["edge_audit"]["stand_down_recommended"].is_null());
        assert!(
            (result["final_balance"].as_f64().unwrap()
                - 100.0
                - result["total_pnl"].as_f64().unwrap())
            .abs()
                < 1e-9
        );
    }
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn high_synthetic_price_blocks_heuristic_entries_and_default_shape_remains_supported(
) -> anyhow::Result<()> {
    let bot = offline_backtest_bot().await?;
    let high = run_backtest(
        State(bot.clone()),
        Ok(Json(offline_request(true, 0.9, 0.6))),
    )
    .await
    .0;
    assert_eq!(
        high["total_trades"], 0,
        "the heuristic q<=.85 cannot justify a .90 purchase: {high}"
    );
    assert!(high["edge_audit"]["skipped"]["edge_gate"].as_u64().unwrap() > 0);
    let mut default = offline_request(true, 0.5, 0.5);
    default.as_object_mut().unwrap().remove("entry_price_yes");
    default.as_object_mut().unwrap().remove("entry_price_no");
    let result = run_backtest(State(bot.clone()), Ok(Json(default))).await.0;
    assert!(result["total_trades"].as_u64().unwrap() > 0, "{result}");
    assert_eq!(result["price_assumptions"]["yes"], 0.5);
    assert!(result["edge_audit"]["break_even_win_rate_pct"].is_number());
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn research_modes_and_validation_never_mutate_the_execution_portfolio() -> anyhow::Result<()>
{
    let bot = offline_backtest_bot().await?;
    bot.control_plane.config.write().await.mode = "live".into();
    bot.freeze_entries(
        "backtest-readonly-fixture",
        "Verify research leaves deliberately frozen entries unchanged",
    )
    .await;
    let cfg_before = serde_json::to_value(&*bot.control_plane.config.read().await)?;
    let balance = bot.balance().await;
    let replay = json!({"mode":"decision_replay","required_stages":["simulated_buy_fill"],"records":[{"schema_version":1,"id":"readonly","captured_at":"2026-09-14T00:00:00Z","origin":"synthetic_fixture","implementation_id":crate::replay::implementation_id(),"stage":"simulated_buy_fill","input":{"book":{"asks":[{"price":0.4,"size":5.0}]},"limit_price":0.5,"size":2.0},"expected":[0.4,2.0]}]});
    let out = run_backtest(State(bot.clone()), Ok(Json(replay))).await.0;
    assert_eq!(out["report"]["matches_required_stages"], true, "{out}");
    let out = run_backtest(
        State(bot.clone()),
        Ok(Json(offline_request(true, 0.25, 0.6))),
    )
    .await
    .0;
    assert!(out["total_trades"].as_u64().unwrap() > 0);
    for bad in [
        json!({"mode":"unknown"}),
        json!({"mode":null}),
        json!({"entry_price_yes":0.0}),
        json!({"entry_price_no":0.3,"exit_mode":"taker_tp_sl"}),
    ] {
        assert!(run_backtest(State(bot.clone()), Ok(Json(bad))).await.0["error"].is_string());
    }
    assert_eq!(
        serde_json::to_value(&*bot.control_plane.config.read().await)?,
        cfg_before
    );
    assert_eq!(bot.balance().await, balance);
    assert!(bot.kill_switch_active());
    assert!(!bot.loop_running());
    assert!(bot.trades().await.is_empty());
    let counts:(i64,i64,i64)=sqlx::query_as("SELECT (SELECT COUNT(*) FROM trades),(SELECT COUNT(*) FROM trade_fills),(SELECT COUNT(*) FROM realized_pnl_events)").fetch_one(&bot.db).await?;
    assert_eq!(counts, (0, 0, 0));
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn hypothetical_entry_costs_cannot_exceed_the_virtual_cash_balance() -> anyhow::Result<()> {
    let bot = offline_backtest_bot().await?;
    let mut request = offline_request(true, 0.01, 0.6);
    request["maker_gas_usd"] = json!(200.0);
    let out = run_backtest(State(bot.clone()), Ok(Json(request))).await.0;
    assert_eq!(out["total_trades"], 0, "{out}");
    assert_eq!(out["final_balance"], 100.0);
    assert_eq!(out["total_pnl"], 0.0);
    assert!(
        out["edge_audit"]["skipped"]["entry_cost_exceeds_balance"]
            .as_u64()
            .unwrap()
            > 0
    );
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn pagination_returns_the_newest_completed_horizon_after_fetching_the_buffer() {
    let target = 1600usize;
    let aligned_end = 1_700_000_000_000 / KLINE_15M_MS * KLINE_15M_MS;
    for end in [aligned_end, aligned_end + KLINE_15M_MS / 2] {
        let source: Vec<Value> = (0..target + 20)
            .map(|i| {
                let open = aligned_end - (target + 19 - i) as i64 * KLINE_15M_MS;
                json!([
                    open,
                    "100",
                    "102",
                    "99",
                    "101",
                    "20",
                    open + KLINE_15M_MS - 1,
                ])
            })
            .collect();
        let mut requests = Vec::new();
        let parsed = fetch_historical_pages(target, end, |start, limit| {
            requests.push((start, limit));
            let batch = source
                .iter()
                .filter(|row| {
                    kline_open_time_ms(row).is_some_and(|open| open >= start && open <= end)
                })
                .take(limit)
                .cloned()
                .collect();
            std::future::ready(Ok(batch))
        })
        .await
        .unwrap();
        assert_eq!(
            requests.len(),
            2,
            "must page through buffer/end, not stop at earliest target"
        );
        assert_eq!(parsed.len(), target);
        assert_eq!(parsed.last().unwrap().0, aligned_end - KLINE_15M_MS);
        assert_eq!(
            parsed.first().unwrap().0,
            aligned_end - target as i64 * KLINE_15M_MS
        );
        assert!(parsed.iter().all(|(open, _)| open + KLINE_15M_MS <= end));
    }
}

#[tokio::test]
async fn candle_pagination_bounds_and_repeated_pages_stop_without_inventing_a_full_window() {
    let end = 1_700_000_000_000 / KLINE_15M_MS * KLINE_15M_MS;
    let repeated = json!([
        end - 10 * KLINE_15M_MS,
        "100",
        "102",
        "99",
        "101",
        "20",
        end - 9 * KLINE_15M_MS - 1
    ]);
    let mut calls = 0;
    let parsed = fetch_historical_pages(1600, end, |_, limit| {
        calls += 1;
        std::future::ready(Ok(vec![repeated.clone(); limit]))
    })
    .await
    .unwrap_err();
    assert_eq!(calls, 2);
    assert!(parsed.contains("no forward progress"));
    assert!(
        fetch_historical_pages(usize::MAX, end, |_, _| std::future::ready(Ok(vec![])))
            .await
            .is_err()
    );
    let mut called = false;
    assert!(fetch_historical_pages(0, end, |_, _| {
        called = true;
        std::future::ready(Ok(vec![]))
    })
    .await
    .unwrap()
    .is_empty());
    assert!(!called);
}

#[tokio::test]
async fn conflicting_duplicate_candles_are_refused_instead_of_selecting_the_first_row() {
    let end = KLINE_15M_MS * 20;
    let first = json!([
        KLINE_15M_MS * 10,
        "100",
        "103",
        "99",
        "101",
        "20",
        KLINE_15M_MS * 11 - 1
    ]);
    let mut changed = first.clone();
    changed[4] = json!("102");
    let outcome = fetch_historical_pages(4, end, |_, _| {
        std::future::ready(Ok(vec![first.clone(), changed.clone()]))
    })
    .await;
    assert!(
        outcome.is_err(),
        "conflicting source rows were silently selected: {outcome:?}"
    );
}

#[tokio::test]
async fn synthetic_ta_never_trades_across_a_missing_feature_candle() -> anyhow::Result<()> {
    let bot = offline_backtest_bot().await?;
    let request = offline_request(true, 0.25, 0.55);
    let end = request["end_time_ms"].as_i64().unwrap();
    let count = CANDLES_PER_DAY + 40 + 2;
    let mut rows = cached_backtest_klines(count, end).unwrap();
    let missing_open = rows.remove(70).0;
    store_cached_backtest_klines(count, end, &rows);
    let result = run_backtest(State(bot.clone()), Ok(Json(request))).await.0;
    assert!(result["error"].is_null(), "{result}");
    assert_eq!(result["ta_signal_model"]["version"], 2);
    assert_eq!(
        result["ta_signal_model"]["legacy_results_comparable"],
        false
    );
    let trades = result["trades"].as_array().unwrap();
    assert!(
        !trades.is_empty(),
        "valid contiguous parts should remain usable"
    );
    assert_eq!(result["data_coverage"]["gap_count"], 1);
    assert_eq!(result["data_coverage"]["missing_intervals"], 1);
    assert_eq!(result["data_coverage"]["requested_window_complete"], false);
    assert_eq!(
        result["data_coverage"]["reaches_requested_end_horizon"],
        true
    );
    assert!(result["data_warning"]
        .as_str()
        .unwrap()
        .contains("missing intervals"));
    assert_eq!(
        result["candles_analyzed"],
        result["data_coverage"]["eligible_contiguous_windows"]
    );
    assert_eq!(
        result["edge_audit"]["skipped"]["missing_feature_or_outcome_candle"],
        result["data_coverage"]["rejected_noncontiguous_windows"]
    );
    for trade in trades {
        let entry = trade["entry_time_ms"].as_i64().unwrap();
        assert!(
            missing_open < entry - 40 * KLINE_15M_MS || missing_open > entry,
            "trade used a feature/outcome window with a missing candle: {trade}"
        );
    }
    bot.db.close().await;
    Ok(())
}

#[tokio::test]
async fn no_sample_metrics_stay_unavailable_instead_of_zero_percent() -> anyhow::Result<()> {
    let bot = offline_backtest_bot().await?;
    let out = run_backtest(
        State(bot.clone()),
        Ok(Json(offline_request(true, 0.9, 0.6))),
    )
    .await
    .0;
    assert_eq!(out["total_trades"], 0);
    assert!(
        out["win_rate"].is_null(),
        "no observations cannot establish a zero-percent hit rate: {out}"
    );
    assert!(out["sharpe_ratio"].is_null());
    assert!(out["edge_audit"]["expectancy_per_trade"].is_null());
    assert!(out["edge_audit"]["avg_stake_usd"].is_null());
    assert_eq!(out["total_pnl"], 0.0);
    assert_eq!(out["final_balance"], 100.0);
    bot.db.close().await;
    Ok(())
}

#[test]
fn classified_trend_names_reach_the_existing_weighted_directional_score() {
    let make_trend = |direction: f64| {
        (0..40)
            .map(|index| {
                let price = 50_000.0 + direction * index as f64 * 50.0;
                Candle {
                    open: price,
                    high: price + 20.0,
                    low: price - 20.0,
                    close: price,
                    volume: 100.0,
                }
            })
            .collect::<Vec<_>>()
    };
    let up = compute_regime(&make_trend(1.0), 0.01);
    let down = compute_regime(&make_trend(-1.0), -0.01);
    assert_eq!(up, "TREND_UP");
    assert_eq!(down, "TREND_DOWN");
    for hour in [0, 8, 16] {
        let weight = backtest_time_of_day_weights(hour).regime;
        assert_eq!(weighted_regime_score(&up, weight), 0.08 * weight);
        assert_eq!(weighted_regime_score(&down, weight), -0.08 * weight);
        assert_eq!(weighted_regime_score("BULL", weight), 0.08 * weight);
        assert_eq!(weighted_regime_score("BEAR", weight), -0.08 * weight);
        for neutral in ["CHOP", "RANGE", "BREAKOUT", "SQUEEZE", "unknown"] {
            assert_eq!(weighted_regime_score(neutral, weight), 0.0);
        }
    }
}
