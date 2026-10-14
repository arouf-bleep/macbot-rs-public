use chrono::Utc;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::warn;

use crate::bot::state::TradingBot;
use crate::db;
use crate::domain::BotMode;
use crate::signals::price_bus::shared_price_bus;

use super::markets::{
    fetch_no_arb_snapshot, fetch_recent_outcomes_snapshot, try_fetch_markets_snapshot,
};

struct SlowSnapshotCache {
    bot: Weak<TradingBot>,
    ts: Instant,
    payload: Value,
    /// Bumped on every rebuild so callers can tell one entry from the next.
    generation: u64,
}

static SLOW_CACHE: Lazy<RwLock<Option<SlowSnapshotCache>>> = Lazy::new(|| RwLock::new(None));
const SLOW_TTL: Duration = Duration::from_secs(1);
const PRICE_BUS_FRESH_AGE_MS: u64 = 5_000;

/// Closed-trade statistics and dated realized P&L, cached for a moment.
///
/// The stream ticks five times a second and every tick rebuilt this report,
/// which loads every closed trade and recomputes the totals. That is five full
/// scans per second per connected browser to produce a figure that only changes
/// when a sale is realized. The TTL bounds the dashboard reporting delay and
/// long enough that the scan happens once per second rather than five times.
struct PerfCache {
    bot: Weak<TradingBot>,
    ts: Instant,
    payload: Value,
}

static PERF_CACHE: Lazy<RwLock<Option<PerfCache>>> = Lazy::new(|| RwLock::new(None));
const PERF_TTL: Duration = Duration::from_millis(1_000);

async fn performance_report_cached(bot: &Arc<TradingBot>) -> Value {
    let bot_ref = Arc::downgrade(bot);
    if let Some(hit) = PERF_CACHE.read().await.as_ref() {
        if hit.bot.ptr_eq(&bot_ref) && hit.ts.elapsed() < PERF_TTL {
            return hit.payload.clone();
        }
    }
    let fresh = bot.get_performance_report().await;
    *PERF_CACHE.write().await = Some(PerfCache {
        bot: bot_ref,
        ts: Instant::now(),
        payload: fresh.clone(),
    });
    fresh
}

pub async fn fast_snapshot(bot: &Arc<TradingBot>) -> Value {
    let build_start = Instant::now();
    let analysis = {
        let guard = bot.analysis_state.last_analysis.read().await;
        guard.clone()
    };
    // The stream ticks far faster than the analysis cycle, so a subscriber sees
    // the same analysis many times over. Its age travels with it.
    let analysis_freshness = bot.analysis_freshness().await;

    let live_btc_price = live_btc_price(bot).await;
    let stats = performance_report_cached(bot).await;
    let daily_pnl = stats.get("today_pnl").and_then(|v| v.as_f64());
    let daily_pnl_evidence = stats
        .get("daily_pnl_evidence")
        .cloned()
        .unwrap_or(Value::Null);
    let balance = bot.balance().await;
    let loop_running = bot.loop_running();
    let mode = bot.mode().await;
    let (execution_mode, maker_hold_positions, macro_guard) = {
        let cfg = bot.control_plane.config.read().await.clone();
        (
            cfg.execution_mode.clone(),
            cfg.maker_hold_positions,
            crate::bot::macro_calendar::macro_guard_status(&cfg, Utc::now()),
        )
    };
    let risk = risk_snapshot(bot).await;
    let execution = execution_snapshot(bot).await;

    let deep_model = {
        let model_guard = bot.intelligence.deep_model.read().await;
        let model_info = match model_guard.as_ref() {
            Some(m) => json!({
                "loaded": true,
                "version": m.version,
                "loaded_ago_secs": m.loaded_at.elapsed().as_secs(),
            }),
            None => json!({ "loaded": false }),
        };
        drop(model_guard);

        let ensemble_guard = bot.intelligence.ensemble_state.read().await;
        let ensemble_info = ensemble_guard.to_json();
        let last_pred = json!({
            "deep_p_up": ensemble_guard.last_deep_p_up,
            "blended_p_up": ensemble_guard.last_blended_p_up,
            "mc_uncertainty": ensemble_guard.last_mc_uncertainty,
            "should_skip": ensemble_guard.last_should_skip,
        });
        drop(ensemble_guard);

        json!({
            "model": model_info,
            "ensemble": ensemble_info,
            "last_prediction": last_pred,
        })
    };

    let maker = bot.maker_state_snapshot().await;
    let l2_feed_stale = bot.l2_feed_stale();
    let l2_feed_age_ms = bot.l2_feed_age_ms().await;
    let feed_truth = compact_feed_truth(&maker, l2_feed_stale, l2_feed_age_ms);
    let no_trade_truth = compact_no_trade_truth(&maker, l2_feed_stale);
    let price_bus = price_bus_snapshot().await;

    json!({
        "analysis": analysis,
        "analysis_freshness": analysis_freshness,
        "live_btc_price": live_btc_price,
        "stats": stats,
        "daily_pnl": daily_pnl,
        "daily_pnl_evidence": daily_pnl_evidence,
        "loop_running": loop_running,
        "backtest_only": bot.backtest_only(),
        "balance": balance,
        "mode": mode,
        "execution_mode": execution_mode,
        // Whether the maker holds what it buys or sells it back on the next
        // uptick is the single largest behavioural switch the bot has, and the
        // page had no way to show which one was running.
        "maker_hold_positions": maker_hold_positions,
        "l2_feed_stale": l2_feed_stale,
        "l2_feed_age_ms": l2_feed_age_ms,
        "feed_truth": feed_truth,
        "no_trade_truth": no_trade_truth,
        "risk": risk,
        "execution": execution,
        "macro_guard": macro_guard,
        "deep_model_status": deep_model,
        "maker": maker,
        "price_bus": price_bus,
        "ts": Utc::now().to_rfc3339(),
        "server_build_ms": build_start.elapsed().as_millis() as u64,
    })
}

fn compact_feed_truth(snapshot: &Value, l2_feed_stale: bool, l2_feed_age_ms: Option<u64>) -> Value {
    let l2_summary = snapshot.get("l2_summary").unwrap_or(&Value::Null);
    json!({
        "l2_feed_stale": l2_feed_stale,
        "l2_feed_age_ms": l2_feed_age_ms,
        "market_count": snapshot.get("market_count").and_then(Value::as_u64),
        "quote_count": snapshot.get("quote_count").and_then(Value::as_u64),
        "target_market_count": l2_summary.get("target_market_count").and_then(Value::as_u64),
        "token_count": l2_summary.get("token_count").and_then(Value::as_u64),
        "snapshot_loaded_count": l2_summary.get("snapshot_loaded_count").and_then(Value::as_u64),
        "two_sided_count": l2_summary.get("two_sided_count").and_then(Value::as_u64),
        "one_sided_count": l2_summary.get("one_sided_count").and_then(Value::as_u64),
        "stale_count": l2_summary.get("stale_count").and_then(Value::as_u64),
        "quoteable_count": l2_summary.get("quoteable_count").and_then(Value::as_u64),
        "primary_not_ready_reason": l2_summary.get("primary_not_ready_reason").and_then(Value::as_str),
        "not_ready_reasons": l2_summary.get("not_ready_reasons").cloned(),
        "l2_stale_code": normalized_l2_stale_code(snapshot),
        "l2_stale_reason": snapshot.get("l2_stale_reason").and_then(Value::as_str),
    })
}

fn normalized_l2_stale_code(snapshot: &Value) -> Option<&str> {
    snapshot
        .get("l2_stale_code")
        .and_then(Value::as_str)
        .or_else(|| {
            let reason = snapshot.get("l2_stale_reason").and_then(Value::as_str)?;
            if reason == "no target markets" {
                Some("no_target_markets")
            } else if reason == "execution_mode=taker" {
                Some("execution_mode_taker")
            } else if reason.starts_with("warmup: awaiting snapshots") {
                Some("warmup_awaiting_snapshots")
            } else if reason.starts_with("warmup timeout") {
                Some("warmup_timeout")
            } else if reason.starts_with("warmup ") {
                Some("warmup_partial")
            } else if reason.starts_with("rearm_wait_ms=") {
                Some("rearm_wait")
            } else if reason.starts_with("stale timeout") {
                Some("stale_timeout")
            } else if reason.starts_with("session error:") {
                Some("session_error")
            } else if reason.starts_with("execution_actor_backpressure") {
                Some("actor_backpressure")
            } else {
                Some("l2_stale")
            }
        })
}

fn derive_no_trade_label(snapshot: &Value, l2_feed_stale: bool) -> String {
    let l2_summary = snapshot.get("l2_summary").unwrap_or(&Value::Null);
    let target_market_count = l2_summary
        .get("target_market_count")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            snapshot
                .get("market_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        });
    let token_count = l2_summary
        .get("token_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let snapshot_loaded_count = l2_summary
        .get("snapshot_loaded_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let two_sided_count = l2_summary
        .get("two_sided_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let one_sided_count = l2_summary
        .get("one_sided_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let stale_count = l2_summary
        .get("stale_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let primary_not_ready_reason = l2_summary
        .get("primary_not_ready_reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let l2_stale_code = normalized_l2_stale_code(snapshot).unwrap_or("");
    let reason = snapshot
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("");

    if l2_stale_code == "execution_mode_taker" {
        return "execution_mode_taker".to_string();
    }
    if target_market_count == 0 {
        return "no_target_markets".to_string();
    }
    if l2_feed_stale || snapshot.get("l2_stale").and_then(Value::as_bool) == Some(true) {
        return "global_l2_feed_stale".to_string();
    }
    if primary_not_ready_reason == "snapshot_missing"
        || (snapshot_loaded_count == 0 && token_count > 0)
    {
        return "no_snapshot".to_string();
    }
    if primary_not_ready_reason == "one_sided_book" || (one_sided_count > 0 && two_sided_count == 0)
    {
        return "one_sided_books".to_string();
    }
    if primary_not_ready_reason == "token_bbo_stale"
        || (stale_count > 0 && two_sided_count == 0)
        || l2_stale_code == "stale_timeout"
    {
        return "token_bbo_stale".to_string();
    }
    if primary_not_ready_reason.starts_with("l2_stale(") || !l2_stale_code.is_empty() {
        return "global_l2_feed_stale".to_string();
    }
    if reason.contains("no_quote_plan") {
        return "no_quote_plan".to_string();
    }
    if reason.contains("basis_guard") {
        return "basis_guard".to_string();
    }
    if reason.contains("entry_ineligible") {
        return "entry_ineligible".to_string();
    }
    if reason.contains("maker_buy_no") {
        return "maker_buy_no".to_string();
    }
    "maker_idle".to_string()
}

fn compact_no_trade_truth(snapshot: &Value, l2_feed_stale: bool) -> Value {
    let l2_summary = snapshot.get("l2_summary").unwrap_or(&Value::Null);
    let label = derive_no_trade_label(snapshot, l2_feed_stale);
    json!({
        "label": label,
        "decision": snapshot.get("decision").and_then(Value::as_str),
        "bid_action": snapshot.get("bid_action").and_then(Value::as_str),
        "bid_reason": snapshot.get("bid_reason").and_then(Value::as_str),
        "ask_action": snapshot.get("ask_action").and_then(Value::as_str),
        "ask_reason": snapshot.get("ask_reason").and_then(Value::as_str),
        "target_market_count": l2_summary.get("target_market_count").and_then(Value::as_u64),
        "primary_not_ready_reason": l2_summary.get("primary_not_ready_reason").and_then(Value::as_str),
        "not_ready_reasons": l2_summary.get("not_ready_reasons").cloned(),
        "l2_stale_code": normalized_l2_stale_code(snapshot),
        "l2_stale_reason": snapshot.get("l2_stale_reason").and_then(Value::as_str),
        "market_count": snapshot.get("market_count").and_then(Value::as_u64),
        "quote_count": snapshot.get("quote_count").and_then(Value::as_u64),
        "snapshot_loaded_count": l2_summary.get("snapshot_loaded_count").and_then(Value::as_u64),
        "two_sided_count": l2_summary.get("two_sided_count").and_then(Value::as_u64),
        "one_sided_count": l2_summary.get("one_sided_count").and_then(Value::as_u64),
        "stale_count": l2_summary.get("stale_count").and_then(Value::as_u64),
        "quoteable_count": l2_summary.get("quoteable_count").and_then(Value::as_u64),
        "market_id_count": snapshot
            .get("inventory_by_market")
            .and_then(Value::as_object)
            .map(|obj| obj.len()),
    })
}

/// Guards against several callers all deciding to refresh at once.
static SLOW_REFRESHING: AtomicBool = AtomicBool::new(false);
static SLOW_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The slow half of the snapshot, served from cache and refreshed behind it.
///
/// This used to refresh inline: when the entry aged past its TTL the caller
/// waited for four network round trips to Polymarket. That caller is the stream
/// tick, so a 200ms tick spent whole seconds inside this function, the interval
/// fell behind, and the whole loop managed roughly one pass every two seconds.
///
/// Returning the existing entry immediately and refreshing in the background
/// keeps the tick at microseconds. The data is at most one TTL older than it
/// would have been, which nothing on screen can perceive, and the tick rate
/// stops being governed by Polymarket's latency.
/// When the cached slow half was last rebuilt.
///
/// The stream uses this to avoid resending eight kilobytes of markets and
/// trades alongside a Bitcoin price that moved by a cent.
pub async fn slow_snapshot_generation(bot: &Arc<TradingBot>) -> u64 {
    let bot_ref = Arc::downgrade(bot);
    let observed = SLOW_CACHE
        .read()
        .await
        .as_ref()
        .filter(|cached| cached.bot.ptr_eq(&bot_ref))
        .map(|cached| (cached.generation, cached.ts.elapsed()));

    match observed {
        // The stream polls the generation on every fast tick. Refreshing from
        // this observation keeps the slow half moving even when no caller
        // needs to clone or send the current payload.
        Some((generation, age)) => {
            observe_slow_generation(generation, age, || spawn_slow_refresh(bot))
        }
        None => 0,
    }
}

fn observe_slow_generation(generation: u64, age: Duration, request_refresh: impl FnOnce()) -> u64 {
    if age >= SLOW_TTL {
        request_refresh();
    }
    generation
}

pub async fn slow_snapshot_cached(bot: &Arc<TradingBot>) -> Value {
    let bot_ref = Arc::downgrade(bot);
    let cached = {
        let guard = SLOW_CACHE.read().await;
        guard
            .as_ref()
            .filter(|c| c.bot.ptr_eq(&bot_ref))
            .map(|c| (c.payload.clone(), c.ts.elapsed()))
    };

    match cached {
        Some((payload, age)) => {
            if age >= SLOW_TTL {
                spawn_slow_refresh(bot);
            }
            payload
        }
        // Initial discovery may wait on the venue. Keep the first frame and
        // heartbeat independent of it, with absent fields while it loads.
        None => {
            spawn_slow_refresh(bot);
            json!({"markets": null, "no_arb": null, "outcomes": null,
                "trades": null, "snapshot_ts": null, "slow_loading": true})
        }
    }
}

fn spawn_slow_refresh(bot: &Arc<TradingBot>) {
    // One refresh at a time. Without this every tick past the TTL would launch
    // its own, and they would pile up faster than they complete.
    if SLOW_REFRESHING.swap(true, Ordering::SeqCst) {
        return;
    }
    struct RefreshGuard;
    impl Drop for RefreshGuard {
        fn drop(&mut self) {
            SLOW_REFRESHING.store(false, Ordering::SeqCst);
        }
    }
    // Move the guard into the task so cancellation before its first poll also
    // releases the single-flight claim.
    let refresh_guard = RefreshGuard;
    let bot = Arc::clone(bot);
    tokio::spawn(async move {
        let _refresh_guard = refresh_guard;
        let payload = match tokio::time::timeout(Duration::from_secs(25), slow_snapshot(&bot)).await
        {
            Ok(payload) => payload,
            Err(_) => json!({"markets": null, "no_arb": null, "outcomes": null,
                "trades": null, "snapshot_ts": null, "slow_error": "venue snapshot timed out"}),
        };
        *SLOW_CACHE.write().await = Some(SlowSnapshotCache {
            bot: Arc::downgrade(&bot),
            ts: Instant::now(),
            payload,
            generation: SLOW_GENERATION.fetch_add(1, Ordering::SeqCst) + 1,
        });
    });
}

async fn slow_snapshot(bot: &Arc<TradingBot>) -> Value {
    if bot.backtest_only() {
        let (trades, trades_error) = recent_trades_snapshot(bot).await;
        return json!({
            "markets": [], "no_arb": null, "outcomes": [],
            "trades": trades, "trades_error": trades_error,
            "snapshot_ts": Utc::now().to_rfc3339(),
        });
    }
    let (markets, no_arb, outcomes, trades) = tokio::join!(
        try_fetch_markets_snapshot(bot),
        fetch_no_arb_snapshot(bot),
        fetch_recent_outcomes_snapshot(bot, 16),
        recent_trades_snapshot(bot),
    );

    let (trades, trades_error) = trades;
    let discovery_error = [
        markets.as_ref().err().map(ToString::to_string),
        outcomes.as_ref().err().map(ToString::to_string),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    json!({
        "markets": markets.ok(),
        "no_arb": no_arb,
        "outcomes": outcomes.ok(),
        "slow_error": if discovery_error.is_empty() { None } else { Some(discovery_error.join("; ")) },
        "trades": trades,
        "trades_error": trades_error,
        "snapshot_ts": Utc::now().to_rfc3339(),
    })
}

async fn recent_trades_snapshot(bot: &Arc<TradingBot>) -> (Option<Vec<Value>>, Option<String>) {
    let mut trades = match db::load_recent_trades(&bot.db, 200).await {
        Ok(trades) => trades,
        Err(e) => {
            warn!("realtime snapshot failed to load recent trades: {e}");
            return (None, Some("trade history unavailable".into()));
        }
    };
    trades.sort_by(|a, b| b.id.cmp(&a.id));
    let mut out = Vec::new();
    for t in trades.into_iter().take(200) {
        out.push(bot.trade_with_live_fields(&t).await);
    }
    (Some(out), None)
}

pub(super) async fn live_btc_price(bot: &Arc<TradingBot>) -> Option<f64> {
    if let Some(price) = shared_price_bus().latest_price("BTCUSDT", 5.0).await {
        return Some(price);
    }
    let cached = bot.market_state.live_btc_px.read().await;
    // Streaming a status snapshot must not make a provider request. REST
    // fallback analysis may populate this cache; expiry remains visible.
    (cached.0.is_finite() && cached.0 > 0.0 && cached.1.elapsed() < Duration::from_secs(2))
        .then_some(cached.0)
}

async fn price_bus_snapshot() -> Value {
    let bus = shared_price_bus();
    let btc = bus.symbol_snapshot("BTCUSDT").await;
    let eth = bus.symbol_snapshot("ETHUSDT").await;
    let sol = bus.symbol_snapshot("SOLUSDT").await;

    let symbols = [btc, eth, sol];
    let live_count = symbols
        .iter()
        .filter(|snapshot| {
            snapshot
                .map(|row| row.age_ms <= PRICE_BUS_FRESH_AGE_MS)
                .unwrap_or(false)
        })
        .count();

    let status = if live_count == 0 {
        "offline"
    } else if live_count == symbols.len() {
        "live"
    } else {
        "degraded"
    };

    let symbol_json = |snapshot: Option<crate::signals::price_bus::PriceBusSnapshot>| match snapshot
    {
        Some(row) => json!({
            "price": row.price,
            "age_ms": row.age_ms,
            "live": row.age_ms <= PRICE_BUS_FRESH_AGE_MS,
        }),
        None => json!({
            "price": Value::Null,
            "age_ms": Value::Null,
            "live": false,
        }),
    };

    json!({
        "source": "binance_ws",
        "status": status,
        "fresh_within_ms": PRICE_BUS_FRESH_AGE_MS,
        "symbols_live": live_count,
        "symbols_total": symbols.len(),
        "btc": symbol_json(btc),
        "eth": symbol_json(eth),
        "sol": symbol_json(sol),
    })
}

async fn risk_snapshot(bot: &Arc<TradingBot>) -> Value {
    let analysis_age_secs = bot.analysis_age_secs().await;
    let analysis_stale = bot.is_analysis_stale().await;
    let (kill_switch, kill_switch_cause) = bot.kill_switch_status();
    let l2_feed_stale = bot.l2_feed_stale();
    let l2_feed_age_ms = bot.l2_feed_age_ms().await;
    let cfg = bot.control_plane.config.read().await;
    let macro_guard = crate::bot::macro_calendar::macro_guard_status(&cfg, Utc::now());

    json!({
        "kill_switch": kill_switch,
        // The stream is what the page reads most of the time, so the cause has
        // to travel on it too, not only on the REST risk endpoint.
        "kill_switch_reason": kill_switch_cause,
        "analysis_age_secs": analysis_age_secs,
        "analysis_stale": analysis_stale,
        "l2_feed_stale": l2_feed_stale,
        "l2_feed_age_ms": l2_feed_age_ms,
        "analysis_stale_threshold_secs": cfg.analysis_stale_seconds,
        "execution_mode": cfg.execution_mode.clone(),
        "max_open_positions": cfg.max_open_positions,
        "max_slippage": cfg.max_slippage,
        "macro_guard": macro_guard,
        "auth_required": cfg.auth_required
            || cfg.mode_kind() == BotMode::Live
            || !cfg.private_rpc_url.is_empty(),
        "shadow_trading": cfg.shadow_trading,
        "degraded_no_trade": bot.degraded_no_trade_active(),
    })
}

async fn execution_snapshot(bot: &Arc<TradingBot>) -> Value {
    let trades = bot.trades().await;
    let open_positions = trades.iter().filter(|t| t.has_unsettled_exposure()).count();
    let open_notional_usd: f64 = trades
        .iter()
        .filter(|t| t.has_unsettled_exposure())
        .map(|t| t.amount)
        .sum();

    let last_trade = db::load_recent_trades(&bot.db, 1)
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next())
        .map(|t| {
            json!({
                "id": t.id,
                "status": t.status,
                "direction": t.direction,
                "ts": t.timestamp,
                "pnl": t.pnl,
                "mode": t.mode,
            })
        })
        .unwrap_or(Value::Null);

    json!({
        "open_positions": open_positions,
        "open_notional_usd": open_notional_usd,
        "last_trade": last_trade,
    })
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn initial_stream_snapshot_returns_absent_fields_while_discovery_runs(
    ) -> anyhow::Result<()> {
        let pool = crate::db::open(":memory:").await?;
        let config = serde_json::from_str(include_str!("../../config.json"))?;
        let bot = std::sync::Arc::new(crate::bot::state::TradingBot::new(config, pool).await?);
        bot.runtime_state
            .backtest_only
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let payload = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            super::slow_snapshot_cached(&bot),
        )
        .await?;
        assert_eq!(payload["slow_loading"], true);
        assert!(payload["trades"].is_null());
        assert!(payload["snapshot_ts"].is_null());
        bot.db.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn history_query_failure_is_unknown_rather_than_an_empty_history() -> anyhow::Result<()> {
        let pool = crate::db::open(":memory:").await?;
        let config = serde_json::from_str(include_str!("../../config.json"))?;
        let bot = std::sync::Arc::new(crate::bot::state::TradingBot::new(config, pool).await?);
        bot.db.close().await;
        let (trades, error) = super::recent_trades_snapshot(&bot).await;
        assert!(trades.is_none());
        assert_eq!(error.as_deref(), Some("trade history unavailable"));
        Ok(())
    }

    #[tokio::test]
    async fn stream_price_is_a_bounded_read_and_expires_without_refetching() -> anyhow::Result<()> {
        let pool = crate::db::open(":memory:").await?;
        let config: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        let bot = std::sync::Arc::new(crate::bot::state::TradingBot::new(config, pool).await?);
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_millis(250),
                super::live_btc_price(&bot)
            )
            .await?,
            None
        );
        *bot.market_state.live_btc_px.write().await = (100.0, std::time::Instant::now());
        assert_eq!(super::live_btc_price(&bot).await, Some(100.0));
        *bot.market_state.live_btc_px.write().await = (
            100.0,
            std::time::Instant::now() - std::time::Duration::from_secs(3),
        );
        assert_eq!(super::live_btc_price(&bot).await, None);
        assert_eq!(bot.market_state.live_btc_px.read().await.0, 100.0);
        bot.runtime_state
            .backtest_only
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let snapshot = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            super::slow_snapshot(&bot),
        )
        .await?;
        assert!(snapshot["markets"].as_array().unwrap().is_empty());
        assert!(snapshot["no_arb"].is_null());
        bot.db.close().await;
        Ok(())
    }

    use super::{
        compact_feed_truth, compact_no_trade_truth, derive_no_trade_label, observe_slow_generation,
        SLOW_TTL,
    };
    use serde_json::json;
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn observing_an_expired_slow_generation_requests_its_refresh() {
        let refreshes = Cell::new(0);

        let generation = observe_slow_generation(7, SLOW_TTL - Duration::from_millis(1), || {
            refreshes.set(refreshes.get() + 1)
        });
        assert_eq!(generation, 7);
        assert_eq!(refreshes.get(), 0, "a fresh cache must not refresh");

        let generation =
            observe_slow_generation(7, SLOW_TTL, || refreshes.set(refreshes.get() + 1));
        assert_eq!(generation, 7);
        assert_eq!(refreshes.get(), 1, "an expired cache must refresh");
    }

    #[test]
    fn no_trade_label_prefers_no_target_markets_when_market_surface_is_empty() {
        let snapshot = json!({
            "market_count": 0,
            "quote_count": 0,
            "l2_summary": {
                "target_market_count": 0,
                "token_count": 0,
                "snapshot_loaded_count": 0,
                "two_sided_count": 0,
                "one_sided_count": 0,
                "stale_count": 0,
                "quoteable_count": 0
            },
            "l2_stale_reason": "no target markets",
            "decision": "no_quote_plan"
        });

        assert_eq!(derive_no_trade_label(&snapshot, false), "no_target_markets");
        let truth = compact_no_trade_truth(&snapshot, false);
        assert_eq!(
            truth.get("label").and_then(|v| v.as_str()),
            Some("no_target_markets")
        );
        assert_eq!(truth.get("market_count").and_then(|v| v.as_u64()), Some(0));
    }

    #[test]
    fn no_trade_label_prefers_taker_mode_over_empty_market_surface() {
        let snapshot = json!({
            "market_count": 0,
            "quote_count": 0,
            "l2_stale_code": "execution_mode_taker",
            "l2_stale_reason": "execution_mode=taker",
            "l2_summary": {
                "target_market_count": 0,
                "token_count": 0,
                "snapshot_loaded_count": 0,
                "two_sided_count": 0,
                "one_sided_count": 0,
                "stale_count": 0,
                "quoteable_count": 0
            },
            "decision": "no_quote_plan"
        });

        assert_eq!(
            derive_no_trade_label(&snapshot, true),
            "execution_mode_taker"
        );
        let truth = compact_no_trade_truth(&snapshot, true);
        assert_eq!(
            truth.get("label").and_then(|v| v.as_str()),
            Some("execution_mode_taker")
        );
        assert_eq!(
            truth.get("l2_stale_code").and_then(|v| v.as_str()),
            Some("execution_mode_taker")
        );
    }

    #[test]
    fn feed_truth_compacts_l2_counts() {
        let snapshot = json!({
            "market_count": 3,
            "quote_count": 6,
            "l2_summary": {
                "target_market_count": 3,
                "token_count": 4,
                "snapshot_loaded_count": 4,
                "two_sided_count": 2,
                "one_sided_count": 1,
                "stale_count": 1,
                "quoteable_count": 1,
                "primary_not_ready_reason": "token_bbo_stale",
                "not_ready_reasons": [{"label": "token_bbo_stale", "count": 1}]
            },
            "l2_stale_code": "stale_timeout",
            "l2_stale_reason": "token_bbo_stale"
        });
        let truth = compact_feed_truth(&snapshot, true, Some(1234));
        assert_eq!(
            truth.get("l2_feed_stale").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            truth.get("snapshot_loaded_count").and_then(|v| v.as_u64()),
            Some(4)
        );
        assert_eq!(
            truth.get("quoteable_count").and_then(|v| v.as_u64()),
            Some(1)
        );
        assert_eq!(
            truth.get("target_market_count").and_then(|v| v.as_u64()),
            Some(3)
        );
        assert_eq!(
            truth
                .get("primary_not_ready_reason")
                .and_then(|v| v.as_str()),
            Some("token_bbo_stale")
        );
        assert_eq!(
            truth.get("l2_stale_code").and_then(|v| v.as_str()),
            Some("stale_timeout")
        );
        assert_eq!(
            truth.get("l2_stale_reason").and_then(|v| v.as_str()),
            Some("token_bbo_stale")
        );
    }

    #[test]
    fn no_trade_label_distinguishes_snapshot_and_one_sided_books() {
        let no_snapshot = json!({
            "market_count": 2,
            "quote_count": 0,
            "l2_summary": {
                "target_market_count": 2,
                "token_count": 2,
                "snapshot_loaded_count": 0,
                "two_sided_count": 0,
                "one_sided_count": 0,
                "stale_count": 0,
                "quoteable_count": 0,
                "primary_not_ready_reason": "snapshot_missing",
                "not_ready_reasons": [{"label": "snapshot_missing", "count": 2}]
            },
            "decision": "no_quote_plan"
        });
        assert_eq!(derive_no_trade_label(&no_snapshot, false), "no_snapshot");

        let one_sided = json!({
            "market_count": 2,
            "quote_count": 0,
            "l2_summary": {
                "target_market_count": 2,
                "token_count": 2,
                "snapshot_loaded_count": 2,
                "two_sided_count": 0,
                "one_sided_count": 2,
                "stale_count": 0,
                "quoteable_count": 0,
                "primary_not_ready_reason": "one_sided_book",
                "not_ready_reasons": [{"label": "one_sided_book", "count": 2}]
            },
            "decision": "no_quote_plan"
        });
        assert_eq!(derive_no_trade_label(&one_sided, false), "one_sided_books");
    }
}
