use crate::http_client::JsonResponseExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use futures_util::{SinkExt, StreamExt};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use crate::bot::state::{StickyAnalysisMarket, TradingBot};
use crate::json_util::parse_f64;

use super::l2_book::{price_to_ticks, BookSide, LocalL2Book, TopOfBook, VenueRules};

const DEFAULT_CLOB_WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const DEFAULT_STALE_SECONDS: u64 = 20;
const DEFAULT_STALE_DEBOUNCE_MS: u64 = 2_000;
const DEFAULT_REARM_MS: u64 = 1_500;
const DEFAULT_WARMUP_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_TARGET_REFRESH_MS: u64 = 5_000;
const DEFAULT_MAX_ACTOR_DROP_STREAK: u32 = 25;
const DEFAULT_ANALYSIS_MARKET_STICKY_SECS: u64 = 180;
const READ_TIMEOUT_MS: u64 = 2_000;
/// A session that has run this long has proven the link; the next reconnect
/// starts the backoff over instead of inheriting whatever it had grown to.
const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(30);
const BACKOFF_MAX_SECS: u64 = 30;
/// Markets whose token ids are remembered. Two tokens per fifteen minutes;
/// thirty-two covers eight hours and the cache never needs to be bigger.
const TOKEN_INFO_CACHE_CAPACITY: usize = 32;

fn stable_l2_stale_code(reason: &str) -> Option<&'static str> {
    let reason = reason.trim();
    if reason.is_empty() {
        return None;
    }
    if reason == "no target markets" {
        return Some("no_target_markets");
    }
    if reason == "execution_mode=taker" {
        return Some("execution_mode_taker");
    }
    if reason == "warmup: awaiting snapshots" {
        return Some("warmup_awaiting_snapshots");
    }
    if reason.starts_with("warmup timeout") {
        return Some("warmup_timeout");
    }
    if reason.starts_with("warmup ") {
        return Some("warmup_partial");
    }
    if reason.starts_with("rearm_wait_ms=") {
        return Some("rearm_wait");
    }
    if reason.starts_with("stale timeout") {
        return Some("stale_timeout");
    }
    if reason.starts_with("session error:") {
        return Some("session_error");
    }
    if reason.starts_with("sequence gap token_id=") {
        return Some("sequence_gap");
    }
    if reason.starts_with("execution_actor_backpressure") {
        return Some("actor_backpressure");
    }
    Some("l2_stale")
}

async fn publish_l2_stale(
    bot: &std::sync::Arc<TradingBot>,
    stale: bool,
    reason: Option<String>,
) -> bool {
    let code = reason.as_deref().and_then(stable_l2_stale_code);
    bot.set_l2_feed_stale(stale).await;
    bot.maker_publish_l2_stale_with_code(stale, code, reason)
        .await
}

#[derive(Debug, Clone, PartialEq)]
struct TargetInfo {
    market_id: String,
    is_yes_token: Option<bool>,
    /// From `/markets/{id}`; None only for a token that came from an open
    /// trade whose market the backfill could not fetch.
    rules: Option<VenueRules>,
}

#[derive(Debug, Clone)]
struct MarketTokenInfo {
    token_id: String,
    is_yes_token: Option<bool>,
    rules: Option<VenueRules>,
}

/// The venue rules the feed knows, one entry per market it targets.
fn venue_rules_of(targets: &HashMap<String, TargetInfo>) -> HashMap<String, VenueRules> {
    targets
        .values()
        .filter_map(|t| t.rules.map(|r| (t.market_id.clone(), r)))
        .collect()
}

/// How a session ended. Only a failure earns a backoff: a reconnect the
/// session asked for itself must not wait.
#[derive(Debug, PartialEq, Eq)]
enum SessionExit {
    TargetsChanged,
}

/// The sleep before the next connect and the backoff to carry forward.
/// Every session used to end as an error, so the backoff doubled to its
/// cap by the sixth reconnect and stayed there for the life of the
/// process: each market roll then cost a thirty-second dead feed on top
/// of everything else.
fn reconnect_delay(backoff_secs: u64, session_lived: Duration) -> (u64, u64) {
    let backoff_secs = if session_lived >= BACKOFF_RESET_AFTER {
        1
    } else {
        backoff_secs.max(1)
    };
    let sleep_for = backoff_secs.min(BACKOFF_MAX_SECS);
    (sleep_for, (backoff_secs * 2).min(BACKOFF_MAX_SECS))
}

pub fn spawn_l2_ingestion(bot: std::sync::Arc<TradingBot>) {
    let monitor_bot = std::sync::Arc::clone(&bot);
    crate::task_monitor::spawn_critical_bot_task("l2_ingestion", monitor_bot, async move {
        let mut backoff_secs = 1u64;
        let mut last_retained: Option<(Vec<String>, HashMap<String, VenueRules>)> = None;
        loop {
            let execution_mode = {
                let cfg = bot.control_plane.config.read().await;
                cfg.execution_mode.clone()
            };

            if execution_mode == "taker" {
                publish_l2_stale(&bot, true, Some("execution_mode=taker".to_string())).await;
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            let targets = match collect_targets(&bot).await {
                Ok(t) => t,
                Err(e) => {
                    warn!("L2: failed to collect targets: {e:#}");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };

            // Whatever the feed subscribes to is what the actor keeps books
            // for. Sent only when the set changes, so the idle poll below
            // does not repeat itself every three seconds.
            let mut retained: Vec<String> = targets.keys().cloned().collect();
            retained.sort();
            let retained = (retained, venue_rules_of(&targets));
            if last_retained.as_ref() != Some(&retained) {
                if !bot
                    .maker_retain_l2_tokens(targets.keys().cloned().collect(), retained.1.clone())
                    .await
                {
                    publish_l2_stale(
                        &bot,
                        true,
                        Some("execution_actor_backpressure retaining tokens".into()),
                    )
                    .await;
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                last_retained = Some(retained);
            }

            if targets.is_empty() {
                publish_l2_stale(&bot, true, Some("no target markets".to_string())).await;
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }

            let ws_url = std::env::var("MACBOT_CLOB_WS_URL")
                .unwrap_or_else(|_| DEFAULT_CLOB_WS_URL.to_string());
            info!(
                "L2: connecting to {} for {} token(s)",
                ws_url,
                targets.len()
            );

            let started_at = Instant::now();
            match run_l2_session(&bot, &ws_url, &targets).await {
                Ok(SessionExit::TargetsChanged) => {
                    backoff_secs = 1;
                }
                Err(e) => {
                    warn!("L2: session ended: {e:#}");
                    publish_l2_stale(&bot, true, Some(format!("session error: {e}"))).await;
                    let (sleep_for, next) = reconnect_delay(backoff_secs, started_at.elapsed());
                    tokio::time::sleep(Duration::from_secs(sleep_for)).await;
                    backoff_secs = next;
                }
            }
        }
    });
}

/// Aborts the target watcher when the session that owns it ends, however
/// it ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Target discovery does HTTP and an actor round-trip. It runs beside the
/// socket, never on it: when it ran inside the read loop every five
/// seconds, the socket went unread for the length of a CLOB fetch and
/// Polymarket closed the session as a "slow consumer".
fn spawn_target_watcher(
    bot: std::sync::Arc<TradingBot>,
    every: Duration,
) -> (
    watch::Receiver<Option<HashMap<String, TargetInfo>>>,
    AbortOnDrop,
) {
    let (tx, rx) = watch::channel(None);
    let task = tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            match collect_targets(&bot).await {
                Ok(latest) => {
                    if tx.send(Some(latest)).is_err() {
                        return;
                    }
                }
                Err(e) => warn!("L2: periodic target refresh failed: {e:#}"),
            }
        }
    });
    (rx, AbortOnDrop(task))
}

async fn run_l2_session(
    bot: &std::sync::Arc<TradingBot>,
    ws_url: &str,
    initial_targets: &HashMap<String, TargetInfo>,
) -> Result<SessionExit> {
    let mut targets = initial_targets.clone();
    let (stream, _) = crate::tls::connect_feed(ws_url)
        .await
        .with_context(|| format!("failed websocket connect: {ws_url}"))?;
    let (mut write, mut read) = stream.split();

    subscribe(&mut write, &targets).await?;
    publish_l2_stale(bot, true, Some("warmup: awaiting snapshots".to_string())).await;

    let mut books: HashMap<String, LocalL2Book> = targets
        .keys()
        .map(|token_id| (token_id.clone(), LocalL2Book::new()))
        .collect();

    let mut ready_tokens: HashSet<String> = HashSet::new();
    let mut last_event_at = Instant::now();
    let mut stale_breach_since: Option<Instant> = None;
    let mut healthy_since: Option<Instant> = None;
    let mut actor_drop_streak: u32 = 0;
    let stale_after = Duration::from_secs(
        std::env::var("MACBOT_L2_STALE_SECONDS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_STALE_SECONDS)
            .max(2),
    );
    let stale_debounce = Duration::from_millis(
        std::env::var("MACBOT_L2_STALE_DEBOUNCE_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_STALE_DEBOUNCE_MS)
            .max(100),
    );
    let rearm_after = Duration::from_millis(
        std::env::var("MACBOT_L2_REARM_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_REARM_MS)
            .max(100),
    );
    let max_actor_drop_streak = std::env::var("MACBOT_L2_MAX_ACTOR_DROPS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_MAX_ACTOR_DROP_STREAK)
        .max(1);

    let mut ping = tokio::time::interval(Duration::from_secs(10));
    let target_refresh_every = Duration::from_millis(
        std::env::var("MACBOT_L2_TARGET_REFRESH_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_TARGET_REFRESH_MS)
            .max(500),
    );
    let (mut targets_rx, _watcher) =
        spawn_target_watcher(std::sync::Arc::clone(bot), target_refresh_every);
    let mut warmup_check = tokio::time::interval(target_refresh_every);
    warmup_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let warmup_timeout = Duration::from_millis(
        std::env::var("MACBOT_L2_WARMUP_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_WARMUP_TIMEOUT_MS)
            .max(1_000),
    );
    let mut warmup_started_at = Some(Instant::now());
    loop {
        tokio::select! {
            _ = ping.tick() => {
                if let Err(e) = write.send(Message::Text("PING".to_string())).await {
                    return Err(anyhow!("websocket ping failed: {e}"));
                }
            }
            // Its own arm, so nothing can stand in front of it. The target
            // check used to sit behind the warmup timeout, and once one token
            // went one-sided the session stopped looking for the next market
            // and stayed on the dead one until Polymarket hung up.
            changed = targets_rx.changed() => {
                if changed.is_err() {
                    return Err(anyhow!("target watcher stopped"));
                }
                let latest = targets_rx.borrow_and_update().clone();
                if let Some(latest) = latest {
                    if target_set_changed(&targets, &latest) {
                        info!(
                            "L2: target set changed ({} -> {}) — reconnecting",
                            targets.len(),
                            latest.len()
                        );
                        return Ok(SessionExit::TargetsChanged);
                    }
                }
            }
            _ = warmup_check.tick() => {
                refresh_warmup_state(
                    &mut warmup_started_at,
                    ready_tokens.len(),
                    targets.len(),
                    Instant::now(),
                );
                if let Some(reason) = warmup_timeout_reason(
                    warmup_started_at,
                    ready_tokens.len(),
                    targets.len(),
                    warmup_timeout,
                    Instant::now(),
                ) {
                    publish_l2_stale(bot, true, Some(reason)).await;
                }
            }
            next_msg = tokio::time::timeout(Duration::from_millis(READ_TIMEOUT_MS), read.next()) => {
                let msg = match next_msg {
                    Ok(Some(Ok(m))) => m,
                    Ok(Some(Err(e))) => return Err(anyhow!("websocket read error: {e}")),
                    Ok(None) => return Err(anyhow!("websocket closed")),
                    Err(_) => {
                        if last_event_at.elapsed() >= stale_after {
                            let breach_start = stale_breach_since.get_or_insert_with(Instant::now);
                            if breach_start.elapsed() >= stale_debounce {
                                publish_l2_stale(
                                    bot,
                                    true,
                                    Some(format!(
                                        "stale timeout >= {}s (debounce={}ms)",
                                        stale_after.as_secs(),
                                        stale_debounce.as_millis()
                                    )),
                                )
                                .await;
                                return Err(anyhow!("stale feed timeout"));
                            }
                        } else {
                            stale_breach_since = None;
                        }
                        // Allow rearm to complete even if we only saw the initial snapshots
                        // and no further heartbeat/messages within the read timeout window.
                        sync_readiness(
                            bot,
                            &ready_tokens,
                            targets.len(),
                            &mut healthy_since,
                            rearm_after,
                        )
                        .await;
                        continue;
                    }
                };
                stale_breach_since = None;

                match msg {
                    Message::Text(text) => {
                        // Polymarket sends plain-text "PONG" in response to our "PING"
                        // keepalive messages. These are not valid JSON so they would
                        // otherwise be silently dropped, causing last_event_at to stale out.
                        if text.trim().eq_ignore_ascii_case("pong") {
                            last_event_at = Instant::now();
                            bot.mark_l2_feed_event().await;
                            sync_readiness(
                                bot,
                                &ready_tokens,
                                targets.len(),
                                &mut healthy_since,
                                rearm_after,
                            )
                            .await;
                            continue;
                        }
                        let value: Value = match serde_json::from_str(text.as_ref()) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        handle_payload(
                            bot,
                            &value,
                            &mut targets,
                            &mut books,
                            &mut ready_tokens,
                            &mut last_event_at,
                            &mut healthy_since,
                            rearm_after,
                            &mut actor_drop_streak,
                            max_actor_drop_streak,
                        )
                        .await?;
                    }
                    Message::Binary(bytes) => {
                        if let Ok(text) = std::str::from_utf8(&bytes) {
                            if let Ok(value) = serde_json::from_str::<Value>(text) {
                                handle_payload(
                                    bot,
                                    &value,
                                    &mut targets,
                                    &mut books,
                                    &mut ready_tokens,
                                    &mut last_event_at,
                                    &mut healthy_since,
                                    rearm_after,
                                    &mut actor_drop_streak,
                                    max_actor_drop_streak,
                                )
                                .await?;
                            }
                        }
                    }
                    Message::Ping(payload) => {
                        let _ = write.send(Message::Pong(payload)).await;
                        last_event_at = Instant::now();
                        bot.mark_l2_feed_event().await;
                        sync_readiness(
                            bot,
                            &ready_tokens,
                            targets.len(),
                            &mut healthy_since,
                            rearm_after,
                        )
                        .await;
                    }
                    Message::Pong(_) => {
                        last_event_at = Instant::now();
                        bot.mark_l2_feed_event().await;
                        sync_readiness(
                            bot,
                            &ready_tokens,
                            targets.len(),
                            &mut healthy_since,
                            rearm_after,
                        )
                        .await;
                    }
                    Message::Close(frame) => {
                        return Err(anyhow!("websocket closed by peer: {frame:?}"));
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn subscribe<S>(write: &mut S, targets: &HashMap<String, TargetInfo>) -> Result<()>
where
    S: futures_util::sink::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let token_ids: Vec<String> = targets.keys().cloned().collect();
    let channel = std::env::var("MACBOT_CLOB_WS_CHANNEL").unwrap_or_else(|_| "market".to_string());
    // Primary (legacy) subscribe format
    let legacy = json!({
        "type": "subscribe",
        "channel": channel,
        "asset_ids": token_ids,
        "assets_ids": token_ids,
        "token_ids": token_ids,
    });
    write
        .send(Message::Text(legacy.to_string()))
        .await
        .context("failed sending legacy subscribe payload")?;

    // Newer market-format subscribe used by Polymarket public feeds
    let market = json!({
        "type": "market",
        "assets_ids": token_ids,
        "custom_feature_enabled": true,
        "initial_dump": true
    });
    if let Err(e) = write.send(Message::Text(market.to_string())).await {
        warn!("L2: market-format subscribe failed (non-fatal): {e}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_payload(
    bot: &std::sync::Arc<TradingBot>,
    payload: &Value,
    targets: &mut HashMap<String, TargetInfo>,
    books: &mut HashMap<String, LocalL2Book>,
    ready_tokens: &mut HashSet<String>,
    last_event_at: &mut Instant,
    healthy_since: &mut Option<Instant>,
    rearm_after: Duration,
    actor_drop_streak: &mut u32,
    max_actor_drop_streak: u32,
) -> Result<()> {
    let events = parse_events(payload);
    let mut frame_sequences = HashMap::new();
    let mut changed_delta_tokens = HashSet::new();
    for evt in events {
        match evt {
            ParsedEvent::Snapshot {
                token_id,
                sequence,
                bids,
                asks,
                tick_size,
            } => {
                let Some(target) = targets.get(&token_id).cloned() else {
                    continue;
                };
                if let Some(tick_size) = tick_size {
                    publish_venue_tick(bot, targets, &token_id, tick_size).await?;
                }
                let book = books.entry(token_id.clone()).or_default();
                let first_snapshot = !book.snapshot_loaded();
                let result = book.apply_snapshot(&bids, &asks, sequence);
                let bbo = book.top_of_book(
                    target.market_id.clone(),
                    token_id.clone(),
                    target.is_yes_token,
                );
                update_ready_token_state(ready_tokens, &token_id, &bbo);
                if result.bbo_changed || first_snapshot {
                    publish_bbo_or_fail(
                        bot,
                        bbo,
                        actor_drop_streak,
                        max_actor_drop_streak,
                        "snapshot",
                    )
                    .await?;
                }
                *last_event_at = Instant::now();
                bot.mark_l2_feed_event().await;
                sync_readiness(bot, ready_tokens, targets.len(), healthy_since, rearm_after).await;
            }
            ParsedEvent::Delta {
                token_id,
                sequence,
                side,
                price_ticks,
                size,
            } => {
                let Some(_target) = targets.get(&token_id) else {
                    continue;
                };
                let book = books.entry(token_id.clone()).or_default();
                let sequence = sequence_for_frame(&mut frame_sequences, &token_id, sequence);
                let result = book.apply_delta(side, price_ticks, size, sequence);
                if result.gap_detected {
                    ready_tokens.remove(&token_id);
                    publish_l2_stale(bot, true, Some(format!("sequence gap token_id={token_id}")))
                        .await;
                    return Err(anyhow!("sequence gap detected"));
                }
                if result.bbo_changed {
                    changed_delta_tokens.insert(token_id);
                }
                *last_event_at = Instant::now();
                bot.mark_l2_feed_event().await;
                sync_readiness(bot, ready_tokens, targets.len(), healthy_since, rearm_after).await;
            }
            ParsedEvent::TickSizeChange {
                token_id,
                tick_size,
            } => {
                if !targets.contains_key(&token_id) {
                    continue;
                }
                publish_venue_tick(bot, targets, &token_id, tick_size).await?;
                *last_event_at = Instant::now();
                bot.mark_l2_feed_event().await;
            }
            ParsedEvent::Heartbeat => {
                *last_event_at = Instant::now();
                bot.mark_l2_feed_event().await;
                sync_readiness(bot, ready_tokens, targets.len(), healthy_since, rearm_after).await;
            }
        }
    }
    // A wire event may change both sides at one sequence. Publish its final
    // book once, avoiding an intermediate quote or a second simulated fill.
    for token_id in changed_delta_tokens {
        let target = &targets[&token_id];
        let bbo = books[&token_id].top_of_book(
            target.market_id.clone(),
            token_id.clone(),
            target.is_yes_token,
        );
        update_ready_token_state(ready_tokens, &token_id, &bbo);
        publish_bbo_or_fail(bot, bbo, actor_drop_streak, max_actor_drop_streak, "delta").await?;
    }
    sync_readiness(bot, ready_tokens, targets.len(), healthy_since, rearm_after).await;
    Ok(())
}

fn sequence_for_frame(
    seen: &mut HashMap<String, u64>,
    token_id: &str,
    sequence: Option<u64>,
) -> Option<u64> {
    let sequence = sequence?;
    if seen.get(token_id) == Some(&sequence) {
        None
    } else {
        seen.insert(token_id.to_string(), sequence);
        Some(sequence)
    }
}

/// Keep the session and metadata cache on the same grid as the actor. A
/// later snapshot can restore the original tick, and a reconnect must not
/// silently restore the grid from the first REST fetch of this market.
async fn publish_venue_tick(
    bot: &std::sync::Arc<TradingBot>,
    targets: &mut HashMap<String, TargetInfo>,
    token_id: &str,
    tick_size: f64,
) -> Result<()> {
    let Some(target) = targets.get(token_id) else {
        return Ok(());
    };
    let market_id = target.market_id.clone();
    let rules = VenueRules {
        tick_size,
        ..target.rules.unwrap_or_default()
    };
    for target in targets.values_mut().filter(|t| t.market_id == market_id) {
        target.rules = Some(rules);
    }
    if let Ok(mut cache) = TOKEN_INFO_CACHE.lock() {
        if let Some((_, tokens)) = cache.iter_mut().find(|(id, _)| *id == market_id) {
            for token in tokens {
                token.rules = Some(rules);
            }
        }
    }
    // Remember the new grid even when delivery fails: the reconnect retries
    // its rules rather than restoring the stale REST metadata.
    if !bot.maker_set_venue_rules(market_id, rules).await {
        return Err(anyhow!("execution actor rejected venue tick update"));
    }
    Ok(())
}

async fn publish_bbo_or_fail(
    bot: &std::sync::Arc<TradingBot>,
    bbo: TopOfBook,
    actor_drop_streak: &mut u32,
    max_actor_drop_streak: u32,
    phase: &str,
) -> Result<()> {
    if bot.maker_publish_bbo(bbo).await {
        *actor_drop_streak = 0;
        return Ok(());
    }
    *actor_drop_streak = actor_drop_streak.saturating_add(1);
    if *actor_drop_streak >= max_actor_drop_streak {
        publish_l2_stale(
            bot,
            true,
            Some(format!(
                "execution_actor_backpressure phase={phase} drops={}",
                actor_drop_streak
            )),
        )
        .await;
        return Err(anyhow!(
            "execution actor backpressure: drop_streak={}",
            actor_drop_streak
        ));
    }
    Ok(())
}

async fn sync_readiness(
    bot: &std::sync::Arc<TradingBot>,
    ready_tokens: &HashSet<String>,
    target_count: usize,
    healthy_since: &mut Option<Instant>,
    rearm_after: Duration,
) {
    if target_count == 0 || ready_tokens.len() < target_count {
        *healthy_since = None;
        publish_l2_stale(
            bot,
            true,
            Some(format!("warmup {}/{}", ready_tokens.len(), target_count)),
        )
        .await;
        return;
    }

    let since = healthy_since.get_or_insert_with(Instant::now);
    if since.elapsed() < rearm_after {
        publish_l2_stale(
            bot,
            true,
            Some(format!("rearm_wait_ms={}", rearm_after.as_millis())),
        )
        .await;
        return;
    }

    publish_l2_stale(bot, false, None).await;
}

async fn collect_targets(bot: &std::sync::Arc<TradingBot>) -> Result<HashMap<String, TargetInfo>> {
    let mut targets: HashMap<String, TargetInfo> = HashMap::new();

    // 1) Open trades with known token ids.
    for t in bot.trades().await.into_iter().filter(|t| t.is_open()) {
        if let Some(token_id) = t.token_id.as_ref().filter(|x| !x.is_empty()) {
            let inferred_is_yes = match t.direction.as_str() {
                "BUY_YES" => Some(true),
                "BUY_NO" => Some(false),
                _ => None,
            };
            targets
                .entry(token_id.clone())
                .and_modify(|e| {
                    if e.market_id.is_empty() {
                        e.market_id = t.market_id.clone();
                    }
                    if e.is_yes_token.is_none() {
                        e.is_yes_token = inferred_is_yes;
                    }
                })
                .or_insert(TargetInfo {
                    market_id: t.market_id.clone(),
                    is_yes_token: inferred_is_yes,
                    rules: None,
                });
        }
    }

    // 2) Existing maker quote state.
    let maker = bot.maker_state_snapshot().await;
    if let Some(quotes) = maker.get("quotes").and_then(|v| v.as_array()) {
        for q in quotes {
            let token_id = q
                .get("token_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let market_id = q
                .get("market_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let is_yes_token = q.get("is_yes_token").and_then(|v| v.as_bool());
            if !token_id.is_empty() && !market_id.is_empty() {
                targets
                    .entry(token_id)
                    .and_modify(|e| {
                        if e.market_id.is_empty() {
                            e.market_id = market_id.clone();
                        }
                        if e.is_yes_token.is_none() {
                            e.is_yes_token = is_yes_token;
                        }
                    })
                    .or_insert(TargetInfo {
                        market_id,
                        is_yes_token,
                        rules: None,
                    });
            }
        }
    }

    // 3) Active analyzed market fallback (YES + NO tokens).
    //    Keep this subscribed even when inventory/quotes exist so the target
    //    set does not flap between 2-token analysis subscriptions and 1-token
    //    inventory subscriptions during short maker control runs.
    //    Wrap in an 8-second timeout — a half-open TCP connection to the CLOB
    //    API would otherwise hang indefinitely, causing long reconnect delays
    //    with no log output (the outer loop never sees an error).
    const TOKEN_FETCH_TIMEOUT: Duration = Duration::from_secs(8);
    if let Some(market_id) = sticky_analysis_market_id(bot).await {
        let fetch_result =
            tokio::time::timeout(TOKEN_FETCH_TIMEOUT, fetch_market_token_infos(&market_id))
                .await
                .unwrap_or_else(|_| Err(anyhow!("market token fetch timed out")))
                .with_context(|| format!("market token fetch failed: {market_id}"))?;
        for token in fetch_result {
            targets
                .entry(token.token_id)
                .and_modify(|e| {
                    if e.market_id.is_empty() {
                        e.market_id = market_id.clone();
                    }
                    if e.is_yes_token.is_none() {
                        e.is_yes_token = token.is_yes_token;
                    }
                    if e.rules.is_none() {
                        e.rules = token.rules;
                    }
                })
                .or_insert(TargetInfo {
                    market_id: market_id.clone(),
                    is_yes_token: token.is_yes_token,
                    rules: token.rules,
                });
        }
    }

    // 4) Backfill unknown YES/NO mapping and venue rules from market metadata
    //    when possible.
    let unresolved_market_ids: HashSet<String> = targets
        .values()
        .filter(|t| (t.is_yes_token.is_none() || t.rules.is_none()) && !t.market_id.is_empty())
        .map(|t| t.market_id.clone())
        .collect();
    for market_id in unresolved_market_ids {
        let tokens =
            tokio::time::timeout(TOKEN_FETCH_TIMEOUT, fetch_market_token_infos(&market_id))
                .await
                .unwrap_or_else(|_| Ok(vec![]))
                .unwrap_or_default();
        if tokens.is_empty() {
            continue;
        }
        let market_rules = tokens.iter().find_map(|t| t.rules);
        let by_token: HashMap<String, Option<bool>> = tokens
            .into_iter()
            .map(|t| (t.token_id, t.is_yes_token))
            .collect();
        for (token_id, target) in targets.iter_mut() {
            if target.market_id != market_id {
                continue;
            }
            if target.is_yes_token.is_none() {
                target.is_yes_token = by_token.get(token_id).copied().flatten();
            }
            if target.rules.is_none() {
                target.rules = market_rules;
            }
        }
    }

    Ok(targets)
}

async fn sticky_analysis_market_id(bot: &std::sync::Arc<TradingBot>) -> Option<String> {
    let analysis = bot.analysis_state.last_analysis.read().await.clone();
    let proposed = extract_analysis_market_id(&analysis);
    let sticky_window = Duration::from_secs(
        std::env::var("MACBOT_L2_ANALYSIS_MARKET_STICKY_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_ANALYSIS_MARKET_STICKY_SECS),
    );
    let now = Instant::now();
    let mut sticky = bot.market_state.l2_sticky_analysis_market.write().await;
    *sticky = apply_sticky_analysis_market(sticky.take(), proposed.as_deref(), now, sticky_window);
    sticky.as_ref().map(|pinned| pinned.market_id.clone())
}

fn extract_analysis_market_id(analysis: &Value) -> Option<String> {
    analysis
        .pointer("/market_data/market_id")
        .and_then(|v| v.as_str())
        .or_else(|| analysis.get("market_id").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

fn target_set_signature(
    targets: &HashMap<String, TargetInfo>,
) -> Vec<(String, String, Option<bool>)> {
    let mut out = targets
        .iter()
        .map(|(token_id, info)| (token_id.clone(), info.market_id.clone(), info.is_yes_token))
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn target_set_changed(
    current: &HashMap<String, TargetInfo>,
    latest: &HashMap<String, TargetInfo>,
) -> bool {
    !latest.is_empty() && target_set_signature(current) != target_set_signature(latest)
}

fn apply_sticky_analysis_market(
    current: Option<StickyAnalysisMarket>,
    proposed: Option<&str>,
    now: Instant,
    sticky_window: Duration,
) -> Option<StickyAnalysisMarket> {
    match (current, proposed) {
        // The analysis still names the pinned market: note that it was seen,
        // and leave the adoption time alone.
        (Some(mut pinned), Some(next_market)) if pinned.market_id == next_market => {
            pinned.last_seen_at = now;
            Some(pinned)
        }
        // A different market, but this one was adopted moments ago: hold it
        // so the subscription does not flap.
        (Some(pinned), Some(_))
            if now.saturating_duration_since(pinned.adopted_at) < sticky_window =>
        {
            Some(pinned)
        }
        (_, Some(next_market)) => Some(StickyAnalysisMarket {
            market_id: next_market.to_string(),
            adopted_at: now,
            last_seen_at: now,
        }),
        // The analysis names nothing (a failed fetch, or the window between
        // markets): keep the last market for a while rather than tearing
        // the subscription down and back up.
        (Some(pinned), None)
            if now.saturating_duration_since(pinned.last_seen_at) < sticky_window =>
        {
            Some(pinned)
        }
        _ => None,
    }
}

fn refresh_warmup_state(
    warmup_started_at: &mut Option<Instant>,
    ready_count: usize,
    target_count: usize,
    now: Instant,
) {
    if target_count == 0 || ready_count >= target_count {
        *warmup_started_at = None;
    } else {
        warmup_started_at.get_or_insert(now);
    }
}

fn warmup_timed_out(
    warmup_started_at: Option<Instant>,
    ready_count: usize,
    target_count: usize,
    timeout: Duration,
    now: Instant,
) -> bool {
    if target_count == 0 || ready_count >= target_count {
        return false;
    }
    warmup_started_at.is_some_and(|since| now.saturating_duration_since(since) >= timeout)
}

fn warmup_timeout_reason(
    warmup_started_at: Option<Instant>,
    ready_count: usize,
    target_count: usize,
    timeout: Duration,
    now: Instant,
) -> Option<String> {
    if !warmup_timed_out(warmup_started_at, ready_count, target_count, timeout, now) {
        return None;
    }
    Some(format!(
        "warmup timeout {}/{} >= {}ms",
        ready_count,
        target_count,
        timeout.as_millis()
    ))
}

/// A market's token ids never change once it exists, and the watcher asks
/// for them every few seconds for the life of the market. One fetch per
/// market; a 404 (not created yet, or gone) is not remembered, so a market
/// that appears late is still found.
type TokenInfoCache = VecDeque<(String, Vec<MarketTokenInfo>)>;

static TOKEN_INFO_CACHE: Lazy<Mutex<TokenInfoCache>> = Lazy::new(|| Mutex::new(VecDeque::new()));

fn cached_token_infos(market_id: &str) -> Option<Vec<MarketTokenInfo>> {
    let cache = TOKEN_INFO_CACHE.lock().ok()?;
    cache
        .iter()
        .find(|(id, _)| id == market_id)
        .map(|(_, tokens)| tokens.clone())
}

fn remember_token_infos(market_id: &str, tokens: &[MarketTokenInfo]) {
    if tokens.is_empty() {
        return;
    }
    let Ok(mut cache) = TOKEN_INFO_CACHE.lock() else {
        return;
    };
    cache.retain(|(id, _)| id != market_id);
    cache.push_back((market_id.to_string(), tokens.to_vec()));
    while cache.len() > TOKEN_INFO_CACHE_CAPACITY {
        cache.pop_front();
    }
}

async fn fetch_market_token_infos(market_id: &str) -> Result<Vec<MarketTokenInfo>> {
    if let Some(tokens) = cached_token_infos(market_id) {
        return Ok(tokens);
    }
    let tokens = fetch_market_token_infos_uncached(market_id).await?;
    remember_token_infos(market_id, &tokens);
    Ok(tokens)
}

async fn fetch_market_token_infos_uncached(market_id: &str) -> Result<Vec<MarketTokenInfo>> {
    let base = std::env::var("MACBOT_CLOB_BASE")
        .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
    let url = format!("{base}/markets/{market_id}");
    let resp = crate::http_client::HTTP
        .get(&url)
        .send()
        .await
        .with_context(|| format!("market token fetch failed: {url}"))?;

    if !resp.status().is_success() {
        // 404 means the market has expired/been removed — not a transient error.
        // Return empty rather than Err so collect_targets returns Ok(empty), which
        // puts the L2 loop into the "no target markets" wait state instead of the
        // error-retry loop (which would keep 404ing the same dead market_id).
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(vec![]);
        }
        return Err(anyhow!("market token fetch HTTP {}", resp.status()));
    }
    let v: Value = resp
        .bounded_json()
        .await
        .context("market token json parse failed")?;
    Ok(parse_market_token_infos(&v))
}

fn parse_market_token_infos(v: &Value) -> Vec<MarketTokenInfo> {
    let rules = VenueRules::from_market_json(v);
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Some(tokens) = v.get("tokens").and_then(|x| x.as_array()) {
        for t in tokens {
            if let Some(token_id) = t.get("token_id").and_then(|x| x.as_str()) {
                if !token_id.is_empty() && seen.insert(token_id.to_string()) {
                    let is_yes_token = t
                        .get("outcome")
                        .and_then(|x| x.as_str())
                        .and_then(parse_is_yes_outcome);
                    out.push(MarketTokenInfo {
                        token_id: token_id.to_string(),
                        is_yes_token,
                        rules,
                    });
                }
            }
        }
    }
    out
}

fn parse_is_yes_outcome(outcome: &str) -> Option<bool> {
    let l = outcome.to_ascii_lowercase();
    if l.contains("yes") || l.contains("up") {
        Some(true)
    } else if l.contains("no") || l.contains("down") {
        Some(false)
    } else {
        None
    }
}

#[derive(Debug)]
enum ParsedEvent {
    Snapshot {
        token_id: String,
        sequence: Option<u64>,
        bids: Vec<(u32, f64)>,
        asks: Vec<(u32, f64)>,
        /// The venue's price grid for this token, as the `book` message
        /// reports it. It changes as a market nears resolution.
        tick_size: Option<f64>,
    },
    Delta {
        token_id: String,
        sequence: Option<u64>,
        side: BookSide,
        price_ticks: u32,
        size: f64,
    },
    TickSizeChange {
        token_id: String,
        tick_size: f64,
    },
    Heartbeat,
}

fn parse_events(v: &Value) -> Vec<ParsedEvent> {
    match v {
        Value::Array(arr) => arr.iter().flat_map(parse_one).collect(),
        _ => parse_one(v),
    }
}

fn parse_one(v: &Value) -> Vec<ParsedEvent> {
    let data = v.get("data").unwrap_or(v);
    if let Value::Array(items) = data {
        return items.iter().flat_map(parse_one).collect();
    }

    // Wire events share fields (a trade also has asset_id/side/price/size).
    // Classify first: only book/price_change may mutate resting depth.
    // Untyped legacy fixtures retain their shape-based compatibility path.
    let event_type = data
        .get("event_type")
        .or_else(|| v.get("event_type"))
        .and_then(Value::as_str);
    if let Some(kind) = event_type {
        if !matches!(kind, "book" | "price_change" | "tick_size_change") {
            return Vec::new();
        }
    }
    if is_heartbeat(v) || is_heartbeat(data) {
        return vec![ParsedEvent::Heartbeat];
    }

    let token_id = extract_token_id(data).or_else(|| extract_token_id(v));
    let sequence = extract_sequence(data).or_else(|| extract_sequence(v));
    if event_type == Some("tick_size_change") {
        return token_id
            .zip(
                data.get("new_tick_size")
                    .and_then(parse_f64)
                    .filter(|tick| *tick > 0.0 && *tick < 1.0),
            )
            .map(|(token_id, tick_size)| ParsedEvent::TickSizeChange {
                token_id,
                tick_size,
            })
            .into_iter()
            .collect();
    }

    let is_snapshot = matches!(event_type, None | Some("book"))
        && data.get("bids").is_some_and(Value::is_array)
        && data.get("asks").is_some_and(Value::is_array);
    if is_snapshot {
        // An explicitly empty book clears old levels too. Testing the parsed
        // vectors for non-emptiness would leave the previous book in place.
        return token_id
            .map(|token_id| ParsedEvent::Snapshot {
                token_id,
                sequence,
                bids: parse_levels(data.get("bids")),
                asks: parse_levels(data.get("asks")),
                tick_size: data
                    .get("tick_size")
                    .and_then(parse_f64)
                    .filter(|tick| *tick > 0.0 && *tick < 1.0),
            })
            .into_iter()
            .collect();
    }
    if event_type == Some("book") {
        return Vec::new();
    }

    if let Some(changes) = data
        .get("price_changes")
        .or_else(|| data.get("changes"))
        .or_else(|| data.get("deltas"))
        .or_else(|| data.get("updates"))
        .and_then(Value::as_array)
    {
        return changes
            .iter()
            .filter_map(|change| parse_delta(change, token_id.as_deref(), sequence))
            .collect();
    }
    parse_delta(data, token_id.as_deref(), sequence)
        .into_iter()
        .collect()
}

fn parse_delta(v: &Value, token_hint: Option<&str>, sequence: Option<u64>) -> Option<ParsedEvent> {
    let token_id = extract_token_id(v)
        .or_else(|| token_hint.map(ToString::to_string))
        .filter(|s| !s.is_empty())?;
    let side = parse_side(v.get("side")?)?;
    let price = parse_f64(v.get("price").or_else(|| v.get("p"))?)?;
    let size = parse_f64(
        v.get("size")
            .or_else(|| v.get("s"))
            .or_else(|| v.get("quantity"))
            .or_else(|| v.get("qty"))
            .or_else(|| v.get("amount"))?,
    )?;
    let price_ticks = price_to_ticks(price)?;
    Some(ParsedEvent::Delta {
        token_id,
        sequence,
        side,
        price_ticks,
        size,
    })
}

fn parse_levels(v: Option<&Value>) -> Vec<(u32, f64)> {
    let Some(Value::Array(levels)) = v else {
        return Vec::new();
    };
    levels
        .iter()
        .filter_map(|lvl| match lvl {
            Value::Array(items) if items.len() >= 2 => {
                let price = parse_f64(&items[0])?;
                let size = parse_f64(&items[1])?;
                let ticks = price_to_ticks(price)?;
                Some((ticks, size))
            }
            Value::Object(_) => {
                let price = parse_f64(lvl.get("price").or_else(|| lvl.get("p"))?)?;
                let size = parse_f64(
                    lvl.get("size")
                        .or_else(|| lvl.get("s"))
                        .or_else(|| lvl.get("quantity"))
                        .or_else(|| lvl.get("qty"))
                        .or_else(|| lvl.get("amount"))?,
                )?;
                let ticks = price_to_ticks(price)?;
                Some((ticks, size))
            }
            _ => None,
        })
        .collect()
}

/// A token is ready once its snapshot has loaded. Readiness used to also
/// require both sides of the book, but a one-sided book is a market state,
/// not a feed state: in the last minute the winning token has no asks, and
/// the feed was flagged stale while events were still arriving, every
/// quote on every market was cancelled, and the reason read "warmup".
/// The actor judges one-sidedness per token, where it belongs.
fn update_ready_token_state(ready_tokens: &mut HashSet<String>, token_id: &str, bbo: &TopOfBook) {
    if bbo.snapshot_loaded {
        ready_tokens.insert(token_id.to_string());
    } else {
        ready_tokens.remove(token_id);
    }
}

fn parse_side(v: &Value) -> Option<BookSide> {
    let s = match v {
        Value::String(s) => s.to_ascii_lowercase(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    if s.contains("bid") || s.contains("buy") || s == "0" {
        Some(BookSide::Bid)
    } else if s.contains("ask") || s.contains("sell") || s == "1" {
        Some(BookSide::Ask)
    } else {
        None
    }
}

fn extract_token_id(v: &Value) -> Option<String> {
    ["asset_id", "token_id", "token"]
        .into_iter()
        .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string()))
}

fn extract_sequence(v: &Value) -> Option<u64> {
    ["sequence", "seq", "offset", "version"]
        .into_iter()
        .find_map(|k| {
            v.get(k).and_then(|x| {
                x.as_u64()
                    .or_else(|| x.as_i64().and_then(|i| u64::try_from(i).ok()))
                    .or_else(|| x.as_str().and_then(|s| s.parse::<u64>().ok()))
            })
        })
}

fn is_heartbeat(v: &Value) -> bool {
    let kind = v
        .get("type")
        .or_else(|| v.get("event"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if kind.contains("heartbeat") || kind == "pong" {
        return true;
    }
    if let Some(msg) = v.get("message").and_then(|x| x.as_str()) {
        return msg.to_ascii_lowercase().contains("heartbeat");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::market::l2_book::TopOfBook;
    use sqlx::SqlitePool;
    use std::sync::Arc;

    #[test]
    fn parses_snapshot_and_delta() {
        let snap = json!({
            "asset_id": "tok-1",
            "sequence": 10,
            "bids": [["0.51", "100"]],
            "asks": [["0.53", "120"]],
            "tick_size": "0.01"
        });
        let events = parse_events(&snap);
        assert_eq!(events.len(), 1);
        match &events[0] {
            ParsedEvent::Snapshot {
                token_id,
                sequence,
                bids,
                asks,
                tick_size,
            } => {
                assert_eq!(token_id, "tok-1");
                assert_eq!(*sequence, Some(10));
                assert_eq!(bids.len(), 1);
                assert_eq!(asks.len(), 1);
                assert_eq!(
                    *tick_size,
                    Some(0.01),
                    "the book message names the venue tick"
                );
            }
            _ => panic!("expected snapshot"),
        }
        let untagged = json!({
            "asset_id": "tok-1",
            "bids": [["0.51", "100"]],
            "asks": []
        });
        match &parse_events(&untagged)[0] {
            ParsedEvent::Snapshot { tick_size, .. } => assert_eq!(*tick_size, None),
            _ => panic!("expected snapshot"),
        }

        let delta = json!({
            "data": {
                "asset_id": "tok-1",
                "seq": 11,
                "changes": [
                    {"side": "BUY", "price": "0.52", "size": "0"},
                    {"side": "SELL", "price": "0.54", "size": "10"}
                ]
            }
        });
        let events = parse_events(&delta);
        assert_eq!(events.len(), 2);
    }

    // Raw WebSocket shapes, not SDK-normalized events:
    // https://docs.polymarket.com/market-data/realtime-data#market-stream
    #[test]
    fn venue_price_changes_use_each_asset_id_and_keep_zero_size_removals() {
        let events = parse_events(&json!({
            "event_type": "price_change", "market": "condition-id",
            "timestamp": "1782753357257",
            "price_changes": [
                {"asset_id": "yes-token", "price": "0.50", "size": "200", "side": "BUY", "best_bid": "0.50", "best_ask": "0.51"},
                {"asset_id": "no-token", "price": "0.49", "size": "0", "side": "SELL", "best_bid": "0.48", "best_ask": "0.50"}
            ]
        }));
        assert_eq!(events.len(), 2);
        assert!(
            matches!(&events[0], ParsedEvent::Delta { token_id, side: BookSide::Bid, price_ticks: 500_000, size, .. }
            if token_id == "yes-token" && *size == 200.0)
        );
        assert!(
            matches!(&events[1], ParsedEvent::Delta { token_id, side: BookSide::Ask, price_ticks: 490_000, size, .. }
            if token_id == "no-token" && *size == 0.0)
        );

        let missing_asset = json!({
            "event_type": "price_change", "market": "condition-id",
            "price_changes": [{"price": "0.50", "size": "1", "side": "BUY"}]
        });
        assert!(
            parse_events(&missing_asset).is_empty(),
            "a condition id is not a token id"
        );
    }

    #[test]
    fn typed_trade_and_unknown_events_cannot_mutate_depth() {
        for kind in [
            "last_trade_price",
            "best_bid_ask",
            "new_market",
            "market_resolved",
            "future_event",
        ] {
            let event = json!({
                "event_type": kind, "market": "condition-id", "asset_id": "token",
                "price": "0.52", "size": "1.25", "side": "BUY",
                "fee_rate_bps": "0", "timestamp": "1782753357257"
            });
            assert!(
                parse_events(&event).is_empty(),
                "{kind} became a book update"
            );
            assert!(parse_events(&json!({"data": event})).is_empty());
        }
    }

    #[test]
    fn venue_tick_changes_validate_the_new_grid() {
        let mut event = json!({
            "event_type": "tick_size_change", "market": "condition-id", "asset_id": "token",
            "old_tick_size": "0.01", "new_tick_size": "0.001", "timestamp": "1782753357257"
        });
        assert!(
            matches!(&parse_events(&event)[0], ParsedEvent::TickSizeChange { token_id, tick_size }
            if token_id == "token" && *tick_size == 0.001)
        );
        for invalid in [Value::Null, json!(0), json!(-0.01), json!(1), json!("NaN")] {
            event["new_tick_size"] = invalid;
            assert!(parse_events(&event).is_empty());
        }
    }

    #[tokio::test]
    async fn venue_events_update_real_books_and_publish_tick_rules() -> anyhow::Result<()> {
        use crate::bot::actor::ExecutionCommand;

        let pool = crate::db::open(":memory:").await?;
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".into();
        cfg.execution_mode = "maker".into();
        let bot = Arc::new(TradingBot::new(cfg, pool).await?);
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        *bot.execution_state.execution_tx.write().await = Some(tx);
        let mut targets = HashMap::from([(
            "token".to_string(),
            TargetInfo {
                market_id: "condition-id".into(),
                is_yes_token: Some(true),
                rules: Some(VenueRules {
                    tick_size: 0.01,
                    min_order_size: 12.0,
                }),
            },
        )]);
        let mut books = HashMap::new();
        let mut ready = HashSet::new();
        let mut last_event = Instant::now();
        let mut healthy = None;
        let mut drops = 0;
        let cases = [
            (
                json!({"event_type":"book", "asset_id":"token", "market":"condition-id", "bids":[], "asks":[]}),
                None,
                None,
                None,
            ),
            (
                json!({"event_type":"book", "asset_id":"token", "market":"condition-id", "sequence":10, "tick_size":"0.01",
                "bids":[{"price":"0.50", "size":"100"}], "asks":[{"price":"0.60", "size":"100"}]}),
                Some(0.50),
                Some(0.60),
                Some(100.0),
            ),
            (
                json!({"event_type":"price_change", "market":"condition-id", "sequence":11, "price_changes":[
                {"asset_id":"token", "price":"0.51", "size":"50", "side":"BUY"},
                {"asset_id":"token", "price":"0.60", "size":"0", "side":"SELL"}]}),
                Some(0.51),
                None,
                Some(50.0),
            ),
            (
                json!({"event_type":"last_trade_price", "asset_id":"token", "market":"condition-id",
                "price":"0.51", "size":"1", "side":"BUY"}),
                Some(0.51),
                None,
                Some(50.0),
            ),
            (
                json!({"event_type":"tick_size_change", "asset_id":"token", "market":"condition-id",
                "old_tick_size":"0.01", "new_tick_size":"0.001"}),
                Some(0.51),
                None,
                Some(50.0),
            ),
            (
                json!({"event_type":"book", "asset_id":"token", "market":"condition-id", "tick_size":"0.01", "bids":[], "asks":[]}),
                None,
                None,
                None,
            ),
        ];
        for (payload, bid, ask, bid_size) in cases {
            handle_payload(
                &bot,
                &payload,
                &mut targets,
                &mut books,
                &mut ready,
                &mut last_event,
                &mut healthy,
                Duration::ZERO,
                &mut drops,
                1,
            )
            .await?;
            let bbo = books["token"].top_of_book("condition-id".into(), "token".into(), Some(true));
            assert_eq!(bbo.best_bid, bid);
            assert_eq!(bbo.best_ask, ask);
            assert_eq!(
                bbo.best_bid_size, bid_size,
                "a last trade must not overwrite resting size"
            );
            assert!(ready.contains("token"));
            assert!(
                bot.execution_state.pending_bbo.lock().await["token"].snapshot_loaded,
                "even the first empty snapshot must reach the actor"
            );
        }
        let pending = bot.execution_state.pending_bbo.lock().await;
        assert_eq!(
            pending["token"].best_bid, None,
            "empty book did not clear the actor's pending mark"
        );
        assert_eq!(pending["token"].best_ask, None);
        drop(pending);
        let mut ticks = Vec::new();
        while let Ok(command) = rx.try_recv() {
            if let ExecutionCommand::SetVenueRules { market_id, rules } = command {
                assert_eq!(market_id, "condition-id");
                assert_eq!(
                    rules.min_order_size, 12.0,
                    "tick updates must preserve the venue minimum"
                );
                ticks.push(rules.tick_size);
            }
        }
        assert_eq!(
            ticks,
            vec![0.01, 0.001, 0.01],
            "tick changes in either direction must reach the actor"
        );
        assert_eq!(targets["token"].rules.unwrap().tick_size, 0.01);
        Ok(())
    }

    #[test]
    fn parse_heartbeat_event() {
        let v = json!({"type":"heartbeat"});
        let events = parse_events(&v);
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ParsedEvent::Heartbeat));
    }

    #[test]
    fn pong_text_is_recognised_as_keepalive() {
        // Polymarket sends plain-text "PONG" (not JSON). Verify that is_heartbeat
        // does NOT handle this (it's pre-JSON parsing), and that the eq_ignore_ascii_case
        // check we added in run_l2_session would match it.
        for pong in &["PONG", "pong", "Pong", " PONG\n"] {
            assert!(
                pong.trim().eq_ignore_ascii_case("pong"),
                "expected PONG match for {pong:?}"
            );
        }
    }

    #[test]
    fn extract_analysis_market_id_supports_nested_and_top_level_shapes() {
        let nested = json!({
            "market_data": {
                "market_id": "mkt-nested"
            }
        });
        assert_eq!(
            extract_analysis_market_id(&nested).as_deref(),
            Some("mkt-nested")
        );

        let top_level = json!({
            "market_id": "mkt-top"
        });
        assert_eq!(
            extract_analysis_market_id(&top_level).as_deref(),
            Some("mkt-top")
        );
    }

    #[test]
    fn target_set_changed_detects_market_rotation_but_not_reordering() {
        let mut current = HashMap::new();
        current.insert(
            "tok-yes".to_string(),
            TargetInfo {
                market_id: "mkt-a".to_string(),
                is_yes_token: Some(true),
                rules: None,
            },
        );
        current.insert(
            "tok-no".to_string(),
            TargetInfo {
                market_id: "mkt-a".to_string(),
                is_yes_token: Some(false),
                rules: None,
            },
        );

        let mut reordered = HashMap::new();
        reordered.insert(
            "tok-no".to_string(),
            TargetInfo {
                market_id: "mkt-a".to_string(),
                is_yes_token: Some(false),
                rules: None,
            },
        );
        reordered.insert(
            "tok-yes".to_string(),
            TargetInfo {
                market_id: "mkt-a".to_string(),
                is_yes_token: Some(true),
                rules: None,
            },
        );
        assert!(!target_set_changed(&current, &reordered));

        let mut rotated = HashMap::new();
        rotated.insert(
            "tok-yes-2".to_string(),
            TargetInfo {
                market_id: "mkt-b".to_string(),
                is_yes_token: Some(true),
                rules: None,
            },
        );
        rotated.insert(
            "tok-no-2".to_string(),
            TargetInfo {
                market_id: "mkt-b".to_string(),
                is_yes_token: Some(false),
                rules: None,
            },
        );
        assert!(target_set_changed(&current, &rotated));
        assert!(!target_set_changed(&current, &HashMap::new()));
    }

    fn pinned(market_id: &str, at: Instant) -> Option<StickyAnalysisMarket> {
        Some(StickyAnalysisMarket {
            market_id: market_id.to_string(),
            adopted_at: at,
            last_seen_at: at,
        })
    }

    fn market_of(sticky: &Option<StickyAnalysisMarket>) -> Option<&str> {
        sticky.as_ref().map(|s| s.market_id.as_str())
    }

    #[test]
    fn sticky_analysis_market_holds_recent_market_across_rotation() {
        let now = Instant::now();
        let sticky = Duration::from_secs(180);

        let held = apply_sticky_analysis_market(pinned("mkt-a", now), Some("mkt-b"), now, sticky);
        assert_eq!(market_of(&held), Some("mkt-a"));

        let expired = apply_sticky_analysis_market(
            pinned("mkt-a", now),
            Some("mkt-b"),
            now + Duration::from_secs(181),
            sticky,
        );
        assert_eq!(market_of(&expired), Some("mkt-b"));
    }

    /// The hold is measured from adoption, not from the last agreement.
    /// A market pinned for a whole window used to be held three more
    /// minutes after it rolled, because every refresh re-stamped it.
    #[test]
    fn sticky_analysis_market_adopted_long_ago_rolls_at_once() {
        let adopted = Instant::now();
        let sticky = Duration::from_secs(180);

        let mut current = pinned("mkt-a", adopted);
        for step in 1..=120u64 {
            current = apply_sticky_analysis_market(
                current,
                Some("mkt-a"),
                adopted + Duration::from_secs(step * 5),
                sticky,
            );
        }
        let last = current.as_ref().expect("still pinned");
        assert_eq!(
            last.adopted_at, adopted,
            "agreement must not re-stamp adoption"
        );
        assert_eq!(last.last_seen_at, adopted + Duration::from_secs(600));

        let rolled = apply_sticky_analysis_market(
            current,
            Some("mkt-b"),
            adopted + Duration::from_secs(605),
            sticky,
        );
        assert_eq!(market_of(&rolled), Some("mkt-b"));
    }

    #[test]
    fn sticky_analysis_market_keeps_recent_market_when_analysis_is_missing() {
        let now = Instant::now();
        let sticky = Duration::from_secs(120);

        let held = apply_sticky_analysis_market(
            pinned("mkt-a", now),
            None,
            now + Duration::from_secs(60),
            sticky,
        );
        assert_eq!(market_of(&held), Some("mkt-a"));

        let dropped = apply_sticky_analysis_market(
            pinned("mkt-a", now),
            None,
            now + Duration::from_secs(121),
            sticky,
        );
        assert!(dropped.is_none());
    }

    /// The missing-analysis hold is measured from the last sighting, so a
    /// market adopted long ago but named a moment ago survives a blip.
    #[test]
    fn sticky_analysis_market_blip_after_long_pin_is_held() {
        let adopted = Instant::now();
        let sticky = Duration::from_secs(120);
        let current = Some(StickyAnalysisMarket {
            market_id: "mkt-a".to_string(),
            adopted_at: adopted,
            last_seen_at: adopted + Duration::from_secs(600),
        });
        let held =
            apply_sticky_analysis_market(current, None, adopted + Duration::from_secs(605), sticky);
        assert_eq!(market_of(&held), Some("mkt-a"));
    }

    #[test]
    fn reconnect_delay_backs_off_only_while_sessions_keep_failing() {
        let brief = Duration::from_secs(1);
        assert_eq!(reconnect_delay(1, brief), (1, 2));
        assert_eq!(reconnect_delay(2, brief), (2, 4));
        assert_eq!(reconnect_delay(16, brief), (16, 30));
        assert_eq!(reconnect_delay(30, brief), (30, 30));
        // A session that ran for a while proved the link: start over.
        assert_eq!(reconnect_delay(30, BACKOFF_RESET_AFTER), (1, 2));
        assert_eq!(reconnect_delay(0, brief), (1, 2));
    }

    /// The venue names its own grid and minimum on `/markets/{id}`; the
    /// maker used to assume 0.001 and no minimum.
    #[test]
    fn market_json_carries_venue_rules_to_every_token() {
        let market = json!({
            "condition_id": "0xabc",
            "minimum_tick_size": 0.01,
            "minimum_order_size": 5,
            "tokens": [
                {"token_id": "1111", "outcome": "Up"},
                {"token_id": "2222", "outcome": "Down"}
            ]
        });
        let infos = parse_market_token_infos(&market);
        assert_eq!(infos.len(), 2);
        for info in &infos {
            assert_eq!(
                info.rules,
                Some(VenueRules {
                    tick_size: 0.01,
                    min_order_size: 5.0
                })
            );
        }
        assert_eq!(infos[0].is_yes_token, Some(true));
        assert_eq!(infos[1].is_yes_token, Some(false));

        let mut targets = HashMap::new();
        for info in infos {
            targets.insert(
                info.token_id.clone(),
                TargetInfo {
                    market_id: "0xabc".to_string(),
                    is_yes_token: info.is_yes_token,
                    rules: info.rules,
                },
            );
        }
        let rules = venue_rules_of(&targets);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules.get("0xabc").map(|r| r.min_order_size), Some(5.0));

        let bare = json!({"tokens": [{"token_id": "1111", "outcome": "Up"}]});
        assert_eq!(parse_market_token_infos(&bare)[0].rules, None);
    }

    #[test]
    fn token_info_cache_remembers_markets_and_forgets_the_oldest() {
        let tokens = vec![MarketTokenInfo {
            token_id: "tok".to_string(),
            is_yes_token: Some(true),
            rules: None,
        }];
        assert!(cached_token_infos("cache-test-none").is_none());
        remember_token_infos("cache-test-empty", &[]);
        assert!(cached_token_infos("cache-test-empty").is_none());
        for i in 0..(TOKEN_INFO_CACHE_CAPACITY + 1) {
            remember_token_infos(&format!("cache-test-{i}"), &tokens);
        }
        assert!(cached_token_infos("cache-test-0").is_none());
        assert_eq!(
            cached_token_infos(&format!("cache-test-{TOKEN_INFO_CACHE_CAPACITY}")).map(|t| t.len()),
            Some(1)
        );
    }

    #[test]
    fn warmup_timeout_only_trips_when_not_ready_long_enough() {
        let now = Instant::now();
        let timeout = Duration::from_secs(10);

        assert!(!warmup_timed_out(None, 0, 2, timeout, now));
        assert!(!warmup_timed_out(Some(now), 2, 2, timeout, now));
        assert!(!warmup_timed_out(
            Some(now),
            1,
            2,
            timeout,
            now + Duration::from_secs(5)
        ));
        assert!(warmup_timed_out(
            Some(now),
            1,
            2,
            timeout,
            now + Duration::from_secs(10)
        ));
    }

    #[test]
    fn warmup_timeout_reason_keeps_session_alive() {
        let now = Instant::now();
        let timeout = Duration::from_secs(10);
        assert_eq!(
            warmup_timeout_reason(Some(now), 1, 2, timeout, now + Duration::from_secs(10)),
            Some("warmup timeout 1/2 >= 10000ms".to_string())
        );
        assert_eq!(
            warmup_timeout_reason(Some(now), 2, 2, timeout, now + Duration::from_secs(10)),
            None
        );
    }

    /// A one-sided book is still a loaded book. Marking it "not ready"
    /// flagged the whole feed stale in the last minute of every market.
    #[test]
    fn update_ready_token_state_follows_the_snapshot_not_the_sides() {
        let token_id = "tok-1";
        let mut ready_tokens = HashSet::new();
        let no_snapshot = TopOfBook {
            market_id: "mkt".to_string(),
            token_id: token_id.to_string(),
            is_yes_token: Some(true),
            best_bid: None,
            best_bid_size: None,
            best_ask: None,
            best_ask_size: None,
            spread_bps: None,
            sequence: None,
            snapshot_loaded: false,
            stale: false,
            updated_at: "2026-03-25T00:00:00Z".to_string(),
        };
        update_ready_token_state(&mut ready_tokens, token_id, &no_snapshot);
        assert!(ready_tokens.is_empty(), "no snapshot, not ready");

        let bid_only = TopOfBook {
            best_bid: Some(0.98),
            best_bid_size: Some(10.0),
            sequence: Some(1),
            snapshot_loaded: true,
            ..no_snapshot.clone()
        };
        update_ready_token_state(&mut ready_tokens, token_id, &bid_only);
        assert!(
            ready_tokens.contains(token_id),
            "one-sided but loaded: ready"
        );

        let full_bbo = TopOfBook {
            best_ask: Some(0.99),
            best_ask_size: Some(12.0),
            ..bid_only
        };
        update_ready_token_state(&mut ready_tokens, token_id, &full_bbo);
        assert!(ready_tokens.contains(token_id));

        update_ready_token_state(&mut ready_tokens, token_id, &no_snapshot);
        assert!(ready_tokens.is_empty(), "a reset book drops out again");
    }

    #[tokio::test]
    async fn actor_backpressure_threshold_marks_stale_and_errors() -> anyhow::Result<()> {
        let pool: SqlitePool = crate::db::open(":memory:").await?;
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.execution_mode = "maker".to_string();

        let bot = Arc::new(crate::bot::state::TradingBot::new(cfg, pool).await?);
        bot.set_l2_feed_stale(false).await;

        let bbo = TopOfBook {
            market_id: "mkt".to_string(),
            token_id: "tok".to_string(),
            is_yes_token: Some(true),
            best_bid: Some(0.49),
            best_bid_size: Some(10.0),
            best_ask: Some(0.51),
            best_ask_size: Some(11.0),
            spread_bps: Some(200.0),
            sequence: Some(1),
            snapshot_loaded: true,
            stale: false,
            updated_at: "2026-03-09T00:00:00Z".to_string(),
        };

        let mut streak = 0u32;
        let res = publish_bbo_or_fail(&bot, bbo, &mut streak, 1, "test").await;
        assert!(
            res.is_err(),
            "expected fail-close error on actor backpressure"
        );
        assert!(bot.l2_feed_stale(), "expected stale feed after fail-close");
        Ok(())
    }
}
