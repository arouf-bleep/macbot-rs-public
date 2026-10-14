use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure};
use chrono::{Timelike, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

use crate::analysis::AnalysisResult as ParsedAnalysis;
use crate::clob::{LimitOrderArgs, OrderType, PublicBookSnapshot, Side, SimTradePriceEvidence};
use crate::config::Config;
use crate::db;
use crate::db::Trade;
use crate::domain::{BotMode, Direction, Signal};
use crate::market::l2_book::{TopOfBook, VenueRules};
use crate::metrics;
use crate::signals::compute_roll_microstructure;

#[path = "taker_audit.rs"]
mod taker_audit;

#[cfg(test)]
#[path = "recovery_deadline_tests.rs"]
mod recovery_deadline_tests;

use super::close::CloseIntent;
use super::risk::{
    evaluate_entry_coordination, evaluate_trade_guardrails, EntryCoordinationProposal,
    EntryCoordinationSnapshot, ProposedTrade,
};
use super::state::{PendingSimExitEvidence, TradingBot};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum QuoteSide {
    Bid,
    Ask,
}

impl QuoteSide {
    fn label(&self) -> &'static str {
        match self {
            QuoteSide::Bid => "BID",
            QuoteSide::Ask => "ASK",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuoteState {
    pub order_id: String,
    pub market_id: String,
    pub token_id: String,
    pub side: QuoteSide,
    pub price: f64,
    pub size: f64,
    pub sequence: u64,
    pub status: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct InventoryState {
    pub net_yes_shares: f64,
    pub net_no_shares: f64,
    pub net_notional_usd: f64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct MakerModelState {
    pub market_id: String,
    pub token_id: String,
    pub is_yes_token: Option<bool>,
    pub fair_value: f64,
    pub p_yes: f64,
    pub skew_bps: f64,
    pub target_spread_bps: f64,
    pub target_bid: f64,
    pub target_ask: f64,
    pub observed_bid: Option<f64>,
    pub observed_ask: Option<f64>,
    pub snapshot_loaded: bool,
    pub book_two_sided: bool,
    pub basis_pct: Option<f64>,
    pub price_to_beat_distance_pct: Option<f64>,
    pub inventory_notional_usd: f64,
    pub bid_action: String,
    pub bid_reason: Option<String>,
    pub ask_action: String,
    pub ask_reason: Option<String>,
    pub decision: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
struct MakerSideOutcome {
    action: String,
    reason: Option<String>,
}

impl Default for MakerSideOutcome {
    fn default() -> Self {
        Self {
            action: "idle".to_string(),
            reason: None,
        }
    }
}

impl MakerSideOutcome {
    fn new(action: &str, reason: Option<String>) -> Self {
        Self {
            action: action.to_string(),
            reason,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RollMicrostructureState {
    pub market_id: String,
    pub token_id: String,
    pub is_yes_token: Option<bool>,
    pub signal: f64,
    pub confidence: f64,
    pub persistence: f64,
    pub serial_covariance_bps2: f64,
    pub recent_delta_bps: f64,
    pub sample_count: usize,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct QuoteTelemetryContext {
    pub order_id: String,
    pub market_id: String,
    pub token_id: String,
    pub is_yes_token: Option<bool>,
    pub side: QuoteSide,
    pub quote_price: f64,
    pub quote_size: f64,
    pub fair_value: f64,
    pub p_yes: f64,
    pub inventory_notional_usd: f64,
    pub target_spread_bps: f64,
    pub basis_widen_applied: bool,
    pub basis_widen_bps: f64,
    pub placed_bid: Option<f64>,
    pub placed_bid_size: Option<f64>,
    pub placed_ask: Option<f64>,
    pub placed_ask_size: Option<f64>,
    pub placed_mid: Option<f64>,
    pub placed_spread_bps: Option<f64>,
    pub basis_pct: Option<f64>,
    pub mark_price: Option<f64>,
    pub index_price: Option<f64>,
    pub btc_spot_price: Option<f64>,
    pub price_to_beat_distance_pct: Option<f64>,
    pub roll_signal: Option<f64>,
    pub roll_confidence: Option<f64>,
    pub roll_persistence: Option<f64>,
    pub roll_recent_delta_bps: Option<f64>,
    pub roll_sample_count: Option<usize>,
    pub placed_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct MakerQuoteTelemetryEvent {
    pub event: String,
    pub reason: Option<String>,
    pub order_id: String,
    pub market_id: String,
    pub token_id: String,
    pub is_yes_token: Option<bool>,
    pub side: QuoteSide,
    pub quote_price: f64,
    pub quote_size: f64,
    pub quote_age_ms: Option<u64>,
    pub fair_value: f64,
    pub p_yes: f64,
    pub inventory_notional_usd: f64,
    pub target_spread_bps: f64,
    pub basis_widen_applied: bool,
    pub basis_widen_bps: f64,
    pub placed_mid: Option<f64>,
    pub current_mid: Option<f64>,
    pub mid_move_bps: Option<f64>,
    pub signed_quote_move_bps: Option<f64>,
    pub adverse_move_bps: Option<f64>,
    pub placed_spread_bps: Option<f64>,
    pub current_spread_bps: Option<f64>,
    pub placed_bid: Option<f64>,
    pub current_bid: Option<f64>,
    pub placed_ask: Option<f64>,
    pub current_ask: Option<f64>,
    pub placed_bid_size: Option<f64>,
    pub current_bid_size: Option<f64>,
    pub placed_ask_size: Option<f64>,
    pub current_ask_size: Option<f64>,
    pub basis_pct: Option<f64>,
    pub current_basis_pct: Option<f64>,
    pub basis_delta_pct: Option<f64>,
    pub mark_price: Option<f64>,
    pub current_mark_price: Option<f64>,
    pub index_price: Option<f64>,
    pub current_index_price: Option<f64>,
    pub btc_spot_price: Option<f64>,
    pub current_btc_spot_price: Option<f64>,
    pub btc_move_bps: Option<f64>,
    pub price_to_beat_distance_pct: Option<f64>,
    pub current_price_to_beat_distance_pct: Option<f64>,
    pub roll_signal: Option<f64>,
    pub current_roll_signal: Option<f64>,
    pub roll_confidence: Option<f64>,
    pub current_roll_confidence: Option<f64>,
    pub roll_persistence: Option<f64>,
    pub current_roll_persistence: Option<f64>,
    pub roll_recent_delta_bps: Option<f64>,
    pub current_roll_recent_delta_bps: Option<f64>,
    pub roll_sample_count: Option<usize>,
    pub current_roll_sample_count: Option<usize>,
    pub observed_at: String,
}

#[derive(Debug, Clone, Default)]
struct MakerMarketContext {
    basis_pct: Option<f64>,
    mark_price: Option<f64>,
    index_price: Option<f64>,
    btc_spot_price: Option<f64>,
    price_to_beat_distance_pct: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReplaceCheck {
    ReplaceAllowed,
    TooSoon,
    Cooldown,
    RateLimited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandPriority {
    High,
    Close,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutboxPriority {
    Critical,
    BestEffort,
}

#[derive(Default)]
struct MakerActorState {
    sequence: u64,
    quotes: HashMap<String, QuoteState>, // order_id -> quote
    by_market: HashMap<String, HashSet<String>>, // market_id -> order_ids
    by_token_side: HashMap<String, String>, // market|token|side -> order_id
    inventory: HashMap<String, InventoryState>, // market_id -> inventory
    l2_bbo_by_token: HashMap<String, TopOfBook>, // token_id -> last top-of-book
    /// Markets a person closed by hand. No quotes there until the market
    /// rolls; the strategy re-entered five seconds after the last one.
    hands_off_markets: HashSet<String>,
    /// Shared by book events and the timer so failed cancels cannot retry at
    /// feed speed. A new manual stand-down has no delay before its first try.
    manual_cancel_last_attempt: HashMap<String, Instant>,
    lifecycle_cancel_last_attempt: Option<Instant>,
    /// What each targeted market will accept: its price grid and smallest
    /// order. Unknown markets get the venue's defaults, never something
    /// finer.
    venue_rules_by_market: HashMap<String, VenueRules>,
    quote_birth: HashMap<String, Instant>, // order_id -> insertion instant
    paper_fill_state: HashMap<String, super::simulation::MakerPaperState>,
    quote_context: HashMap<String, QuoteTelemetryContext>, // order_id -> placement context
    last_replace_by_key: HashMap<String, Instant>,         // market|token|side -> instant
    replace_events: VecDeque<Instant>,                     // global replace timestamps
    model_by_token: HashMap<String, MakerModelState>,      // token_id -> latest model
    roll_by_token: HashMap<String, RollMicrostructureState>, // token_id -> latest roll state
    roll_mid_history_by_token: HashMap<String, VecDeque<f64>>, // token_id -> recent mid history
    recent_quote_telemetry: VecDeque<MakerQuoteTelemetryEvent>,
    post_fill_cooldown_until: HashMap<String, Instant>, // market|token|side -> cooldown expiry
    l2_stale: bool,
    l2_stale_code: Option<String>,
    l2_stale_reason: Option<String>,
    last_event_ts: Option<String>,
    /// market|side -> the suppression reason last written to the outbox.
    last_suppression: HashMap<String, String>,
    last_live_reconcile: Option<Instant>,
    last_terminal_audit: Option<Instant>,
    terminal_audit_cursor: Option<String>,
    taker_audit: taker_audit::TakerAuditState,
    live_accounting_ready: bool,
}

impl MakerActorState {
    /// True when this suppression is a change, rather than the same answer the
    /// loop reached a few seconds ago.
    ///
    /// The maker loop re-evaluates continuously and re-derives the same
    /// verdict, so writing a durable row each time recorded the resting state
    /// rather than an event. The production outbox holds 271,909 rows against
    /// 66 trades; 147,567 are suppressions, and 59,965 of those are the single
    /// reason `no_inventory` — which is simply what "flat" looks like.
    /// Recording transitions keeps what you would actually debug with — when a
    /// side stopped quoting and why, and when the reason changed — and drops
    /// the repetition.
    fn suppression_is_new(&mut self, market_id: &str, side: &str, reason: &str) -> bool {
        let key = format!("{market_id}|{side}");
        let category = suppression_category(reason);
        if self
            .last_suppression
            .get(&key)
            .is_some_and(|prev| *prev == category)
        {
            return false;
        }
        self.last_suppression.insert(key, category);
        true
    }

    /// A side that is quoting again is no longer suppressed, so the next
    /// suppression is a fresh transition even if the reason repeats.
    fn clear_suppression(&mut self, market_id: &str, side: &str) {
        self.last_suppression.remove(&format!("{market_id}|{side}"));
    }

    fn next_sequence(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }

    fn mark_event(&mut self) {
        self.last_event_ts = Some(Utc::now().to_rfc3339());
    }

    fn token_side_key(market_id: &str, token_id: &str, side: &QuoteSide) -> String {
        format!("{market_id}|{token_id}|{}", side.label())
    }

    fn quote_for(&self, market_id: &str, token_id: &str, side: &QuoteSide) -> Option<&QuoteState> {
        let key = Self::token_side_key(market_id, token_id, side);
        let order_id = self.by_token_side.get(&key)?;
        self.quotes.get(order_id)
    }

    fn set_post_fill_cooldown(
        &mut self,
        market_id: &str,
        token_id: &str,
        side: &QuoteSide,
        now: Instant,
        cooldown_ms: u64,
    ) {
        let key = Self::token_side_key(market_id, token_id, side);
        self.post_fill_cooldown_until
            .insert(key, now + Duration::from_millis(cooldown_ms.max(1)));
        self.mark_event();
    }

    fn fill_cooldown_remaining(
        &mut self,
        market_id: &str,
        token_id: &str,
        side: &QuoteSide,
        now: Instant,
    ) -> Option<Duration> {
        let key = Self::token_side_key(market_id, token_id, side);
        let until = self.post_fill_cooldown_until.get(&key).copied()?;
        if now >= until {
            self.post_fill_cooldown_until.remove(&key);
            return None;
        }
        Some(until.saturating_duration_since(now))
    }

    fn quote_age(&self, order_id: &str, now: Instant) -> Option<Duration> {
        self.quote_birth
            .get(order_id)
            .map(|born| now.saturating_duration_since(*born))
    }

    fn update_model(&mut self, model: MakerModelState) {
        self.model_by_token.insert(model.token_id.clone(), model);
        self.mark_event();
    }

    fn set_quote_context(&mut self, context: QuoteTelemetryContext) {
        self.quote_context.insert(context.order_id.clone(), context);
        self.mark_event();
    }

    fn quote_context(&self, order_id: &str) -> Option<&QuoteTelemetryContext> {
        self.quote_context.get(order_id)
    }

    fn record_quote_telemetry(&mut self, event: MakerQuoteTelemetryEvent) {
        self.recent_quote_telemetry.push_front(event);
        while self.recent_quote_telemetry.len() > 200 {
            self.recent_quote_telemetry.pop_back();
        }
        self.mark_event();
    }

    fn inventory_notional_usd(&self, market_id: &str) -> f64 {
        self.inventory
            .get(market_id)
            .map(|x| x.net_notional_usd)
            .unwrap_or(0.0)
    }

    fn upsert_quote(
        &mut self,
        order_id: String,
        market_id: String,
        token_id: String,
        side: QuoteSide,
        price: f64,
        size: f64,
    ) -> Option<QuoteState> {
        if order_id.trim().is_empty() || market_id.trim().is_empty() || token_id.trim().is_empty() {
            return None;
        }
        if price <= 0.0 || size <= 0.0 {
            return None;
        }
        let side_key = Self::token_side_key(&market_id, &token_id, &side);

        // If this side already had another quote id, evict it from in-memory state.
        if let Some(prev_order_id) = self.by_token_side.get(&side_key).cloned() {
            if prev_order_id != order_id {
                let _ = self.remove_quote(&prev_order_id);
            }
        }

        let seq = self.next_sequence();
        let quote = QuoteState {
            order_id: order_id.clone(),
            market_id: market_id.clone(),
            token_id: token_id.clone(),
            side: side.clone(),
            price,
            size,
            sequence: seq,
            status: "OPEN".to_string(),
            updated_at: Utc::now().to_rfc3339(),
        };
        self.clear_suppression(&market_id, side.label());
        self.quotes.insert(order_id.clone(), quote.clone());
        self.by_market
            .entry(market_id)
            .or_default()
            .insert(order_id);
        self.by_token_side.insert(side_key, quote.order_id.clone());
        self.quote_birth
            .entry(quote.order_id.clone())
            .or_insert_with(Instant::now);
        self.mark_event();
        Some(quote)
    }

    fn remove_quote(&mut self, order_id: &str) -> bool {
        let Some(removed) = self.quotes.remove(order_id) else {
            return false;
        };
        if let Some(order_ids) = self.by_market.get_mut(&removed.market_id) {
            order_ids.remove(order_id);
            if order_ids.is_empty() {
                self.by_market.remove(&removed.market_id);
            }
        }
        let side_key = Self::token_side_key(&removed.market_id, &removed.token_id, &removed.side);
        self.by_token_side.remove(&side_key);
        self.quote_birth.remove(order_id);
        self.paper_fill_state.remove(order_id);
        self.quote_context.remove(order_id);
        self.mark_event();
        true
    }

    fn clear_market(&mut self, market_id: &str) -> usize {
        // These markets roll every fifteen minutes, so anything keyed by market
        // has to be dropped with it or it accumulates for the life of the
        // process.
        self.last_suppression
            .retain(|key, _| !key.starts_with(&format!("{market_id}|")));
        self.manual_cancel_last_attempt.remove(market_id);
        let Some(order_ids) = self.by_market.remove(market_id) else {
            return 0;
        };
        let mut removed = 0usize;
        for oid in order_ids {
            if self.remove_quote(&oid) {
                removed += 1;
            }
        }
        removed
    }

    /// Drop every per-token record for tokens the L2 feed no longer
    /// subscribes to. Books, quote models and roll state are keyed by token
    /// and were never removed, so after seven markets the snapshot carried
    /// fourteen books, half of them for resolved markets, counted as
    /// "one-sided" and sent on every stream tick.
    fn retain_l2_tokens(&mut self, keep: &HashSet<String>) -> usize {
        let before = self.l2_bbo_by_token.len();
        self.l2_bbo_by_token
            .retain(|token_id, _| keep.contains(token_id));
        self.model_by_token
            .retain(|token_id, _| keep.contains(token_id));
        self.roll_by_token
            .retain(|token_id, _| keep.contains(token_id));
        self.roll_mid_history_by_token
            .retain(|token_id, _| keep.contains(token_id));
        // A restored stop must survive subscription warmup and book eviction.
        // The next market has a new identity, so an old stop cannot block it.
        self.manual_cancel_last_attempt
            .retain(|market_id, _| self.hands_off_markets.contains(market_id));
        before.saturating_sub(self.l2_bbo_by_token.len())
    }

    fn stand_down(&mut self, market_id: &str) -> bool {
        self.hands_off_markets.insert(market_id.to_string())
    }

    fn manual_cancel_due(&mut self, market_id: &str, now: Instant) -> bool {
        if self
            .manual_cancel_last_attempt
            .get(market_id)
            .is_some_and(|last| now.saturating_duration_since(*last) < Duration::from_secs(1))
        {
            return false;
        }
        self.manual_cancel_last_attempt
            .insert(market_id.to_string(), now);
        true
    }

    fn venue_rules(&self, market_id: &str) -> VenueRules {
        self.venue_rules_by_market
            .get(market_id)
            .copied()
            .unwrap_or_default()
    }

    fn set_venue_rules(&mut self, market_id: &str, rules: VenueRules) -> bool {
        let changed = self.venue_rules_by_market.get(market_id) != Some(&rules);
        self.venue_rules_by_market
            .insert(market_id.to_string(), rules);
        changed
    }

    fn update_inventory(
        &mut self,
        market_id: String,
        net_yes_shares: f64,
        net_no_shares: f64,
        net_notional_usd: f64,
    ) -> InventoryState {
        let state = InventoryState {
            net_yes_shares,
            net_no_shares,
            net_notional_usd,
            updated_at: Utc::now().to_rfc3339(),
        };
        self.inventory.insert(market_id, state.clone());
        self.mark_event();
        state
    }

    fn update_bbo(&mut self, bbo: TopOfBook) {
        if let (Some(best_bid), Some(best_ask)) = (bbo.best_bid, bbo.best_ask) {
            if best_bid > 0.0 && best_ask >= best_bid {
                let mid = (best_bid + best_ask) / 2.0;
                let history = self
                    .roll_mid_history_by_token
                    .entry(bbo.token_id.clone())
                    .or_default();
                let changed = history
                    .back()
                    .copied()
                    .map(|prev| (prev - mid).abs() > f64::EPSILON)
                    .unwrap_or(true);
                if changed {
                    history.push_back(mid);
                    while history.len() > 32 {
                        history.pop_front();
                    }
                    let mids: Vec<f64> = history.iter().copied().collect();
                    let roll = compute_roll_microstructure(&mids);
                    self.roll_by_token.insert(
                        bbo.token_id.clone(),
                        RollMicrostructureState {
                            market_id: bbo.market_id.clone(),
                            token_id: bbo.token_id.clone(),
                            is_yes_token: bbo.is_yes_token,
                            signal: roll.signal,
                            confidence: roll.confidence,
                            persistence: roll.persistence,
                            serial_covariance_bps2: roll.serial_covariance_bps2,
                            recent_delta_bps: roll.recent_delta_bps,
                            sample_count: roll.sample_count,
                            updated_at: Utc::now().to_rfc3339(),
                        },
                    );
                }
            }
        }
        self.l2_bbo_by_token.insert(bbo.token_id.clone(), bbo);
        self.mark_event();
    }

    fn set_l2_stale(&mut self, stale: bool, code: Option<String>, reason: Option<String>) {
        self.l2_stale = stale;
        self.l2_stale_code = code;
        self.l2_stale_reason = reason;
        self.mark_event();
    }

    fn prune_replace_window(&mut self, now: Instant) {
        while let Some(ts) = self.replace_events.front().copied() {
            if now.saturating_duration_since(ts) > Duration::from_secs(60) {
                self.replace_events.pop_front();
            } else {
                break;
            }
        }
    }

    fn check_replace_limits(
        &mut self,
        market_id: &str,
        token_id: &str,
        side: &QuoteSide,
        order_id: &str,
        now: Instant,
        cfg: &Config,
    ) -> ReplaceCheck {
        if self
            .quote_age(order_id, now)
            .is_some_and(|age| age < Duration::from_millis(cfg.maker_min_quote_lifetime_ms))
        {
            return ReplaceCheck::TooSoon;
        }

        let key = Self::token_side_key(market_id, token_id, side);
        if self.last_replace_by_key.get(&key).is_some_and(|last| {
            now.saturating_duration_since(*last)
                < Duration::from_millis(cfg.maker_replace_cooldown_ms)
        }) {
            return ReplaceCheck::Cooldown;
        }

        self.prune_replace_window(now);
        if self.replace_events.len() >= cfg.maker_max_replaces_per_min as usize {
            return ReplaceCheck::RateLimited;
        }
        ReplaceCheck::ReplaceAllowed
    }

    fn mark_replace(&mut self, market_id: &str, token_id: &str, side: &QuoteSide, now: Instant) {
        let key = Self::token_side_key(market_id, token_id, side);
        self.last_replace_by_key.insert(key, now);
        self.replace_events.push_back(now);
        self.prune_replace_window(now);
    }

    fn snapshot(&self) -> Value {
        let mut quotes: Vec<&QuoteState> = self.quotes.values().collect();
        quotes.sort_by(|a, b| b.sequence.cmp(&a.sequence));
        let top_quotes: Vec<Value> = quotes.into_iter().take(200).map(|q| json!(q)).collect();

        let inventory_by_market: serde_json::Map<String, Value> = self
            .inventory
            .iter()
            .map(|(market_id, inv)| (market_id.clone(), json!(inv)))
            .collect();

        let mut bbo: Vec<&TopOfBook> = self.l2_bbo_by_token.values().collect();
        bbo.sort_by(|a, b| a.token_id.cmp(&b.token_id));
        let l2_token_count = bbo.len();
        let l2_target_market_count = self.by_market.len();
        let l2_snapshot_loaded_count = bbo.iter().filter(|x| x.snapshot_loaded).count();
        let l2_two_sided_count = bbo.iter().filter(|x| book_two_sided(x)).count();
        let l2_one_sided_count = bbo
            .iter()
            .filter(|x| x.snapshot_loaded && !x.stale && !book_two_sided(x))
            .count();
        let l2_stale_count = bbo.iter().filter(|x| x.stale).count();
        let mut l2_not_ready_reason_counts: HashMap<String, usize> = HashMap::new();
        let mut l2_ready_count = 0usize;
        for token_bbo in &bbo {
            if let Some(reason) = maker_snapshot_l2_not_ready_reason(
                self.l2_stale,
                self.l2_stale_code.as_deref(),
                self.l2_stale_reason.as_deref(),
                token_bbo,
            ) {
                *l2_not_ready_reason_counts.entry(reason).or_insert(0) += 1;
            } else {
                l2_ready_count += 1;
            }
        }
        let l2_not_ready_count = l2_not_ready_reason_counts.values().sum::<usize>();
        let mut l2_not_ready_reasons: Vec<(String, usize)> =
            l2_not_ready_reason_counts.into_iter().collect();
        l2_not_ready_reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let l2_primary_not_ready_reason =
            l2_not_ready_reasons.first().map(|(label, _)| label.clone());
        let l2_not_ready_reasons: Vec<Value> = l2_not_ready_reasons
            .into_iter()
            .take(5)
            .map(|(label, count)| json!({"label": label, "count": count}))
            .collect();
        let l2_top_of_book: Vec<Value> = bbo.into_iter().map(|x| json!(x)).collect();

        let mut models: Vec<&MakerModelState> = self.model_by_token.values().collect();
        models.sort_by(|a, b| a.token_id.cmp(&b.token_id));
        let quote_model: Vec<Value> = models.into_iter().map(|m| json!(m)).collect();
        let mut roll_models: Vec<&RollMicrostructureState> = self.roll_by_token.values().collect();
        roll_models.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.token_id.cmp(&b.token_id))
        });
        let roll_microstructure: Vec<Value> =
            roll_models.into_iter().take(50).map(|m| json!(m)).collect();
        let recent_quote_telemetry: Vec<Value> = self
            .recent_quote_telemetry
            .iter()
            .take(50)
            .map(|evt| json!(evt))
            .collect();

        json!({
            "sequence": self.sequence,
            "taker_terminal_audit": self.taker_audit.snapshot(),
            "quote_count": self.quotes.len(),
            "market_count": self.by_market.len(),
            "quotes": top_quotes,
            "inventory_by_market": inventory_by_market,
            "l2_top_of_book": l2_top_of_book,
            "l2_summary": {
                "target_market_count": l2_target_market_count,
                "token_count": l2_token_count,
                "snapshot_loaded_count": l2_snapshot_loaded_count,
                "two_sided_count": l2_two_sided_count,
                "one_sided_count": l2_one_sided_count,
                "stale_count": l2_stale_count,
                "ready_count": l2_ready_count,
                "quoteable_count": l2_ready_count,
                "not_ready_count": l2_not_ready_count,
                "primary_not_ready_reason": l2_primary_not_ready_reason,
                "not_ready_reasons": l2_not_ready_reasons,
            },
            "quote_model": quote_model,
            "roll_microstructure": roll_microstructure,
            "recent_quote_telemetry": recent_quote_telemetry,
            "hands_off_markets": self.hands_off_markets.iter().collect::<Vec<_>>(),
            "venue_rules_by_market": self.venue_rules_by_market,
            "l2_stale": self.l2_stale,
            "l2_stale_code": self
                .l2_stale_code
                .as_deref()
                .or_else(|| normalize_l2_stale_code(self.l2_stale_reason.as_deref())),
            "l2_stale_reason": self.l2_stale_reason,
            "replace_events_last_min": self.replace_events.len(),
            "last_event_ts": self.last_event_ts,
        })
    }
}

type PairCloseResult =
    std::result::Result<(Option<Trade>, Option<Trade>), crate::bot::state::ActorCallError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionOrigin {
    Manual,
    Automatic { generation: u64 },
}

pub enum ExecutionCommand {
    CancelMakerOrdersForShutdown {
        resp: oneshot::Sender<Result<(), String>>,
    },
    ResetPaperBalance {
        amount: Option<f64>,
        force: bool,
        resp: oneshot::Sender<Result<Value, String>>,
    },
    ResetDrawdownCashPeak {
        resp: oneshot::Sender<Result<f64, String>>,
    },
    ReconcileRedemption {
        request: crate::clob::redemption::RedemptionRequest,
        resp: oneshot::Sender<anyhow::Result<Value>>,
    },
    RecheckTerminalTakerOrder {
        order_id: String,
        resp: oneshot::Sender<Result<(), String>>,
    },
    RecheckTerminalMakerOrder {
        order_id: String,
        resp: oneshot::Sender<Result<(), String>>,
    },
    ReconcileMakerOrders {
        resp: oneshot::Sender<Result<(), String>>,
    },
    ReconcileLiveState {
        full: bool,
        resp: oneshot::Sender<Result<super::reconcile::LiveReconcileOutcome, String>>,
    },
    ExecuteTrade {
        origin: ExecutionOrigin,
        analysis: Box<ParsedAnalysis>,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: String,
        resp: oneshot::Sender<Option<Trade>>,
    },
    CloseTrade {
        trade_id: i64,
        exit_price: Option<f64>,
        intent: CloseIntent,
        resp: oneshot::Sender<Option<Trade>>,
    },
    PartialCloseTrade {
        trade_id: i64,
        exit_price: f64,
        fraction: f64,
        intent: CloseIntent,
        request_id: Option<u64>,
        resp: oneshot::Sender<Option<Value>>,
    },
    ClosePair {
        first_trade_id: i64,
        second_trade_id: i64,
        exit_price: Option<f64>,
        resp: oneshot::Sender<PairCloseResult>,
    },
    UpsertQuoteState {
        order_id: String,
        market_id: String,
        token_id: String,
        side: QuoteSide,
        price: f64,
        size: f64,
        resp: oneshot::Sender<Option<QuoteState>>,
    },
    RemoveQuoteState {
        order_id: String,
        reason: Option<String>,
        resp: oneshot::Sender<bool>,
    },
    ClearMarketQuotes {
        market_id: String,
        reason: Option<String>,
        resp: oneshot::Sender<usize>,
    },
    UpdateInventoryState {
        market_id: String,
        net_yes_shares: f64,
        net_no_shares: f64,
        net_notional_usd: f64,
        resp: oneshot::Sender<InventoryState>,
    },
    SnapshotMakerState {
        resp: oneshot::Sender<Value>,
    },
    FlushBestBidAsk,
    SetL2Stale {
        stale: bool,
        code: Option<String>,
        reason: Option<String>,
    },
    /// The L2 feed names the tokens it is subscribed to; everything else is
    /// a book for a market that has rolled, and is dropped.
    RetainL2Tokens {
        token_ids: HashSet<String>,
        venue_rules: HashMap<String, VenueRules>,
    },
    /// The venue's tick or minimum for one market changed mid-session.
    SetVenueRules {
        market_id: String,
        rules: VenueRules,
    },
    AppendOutboxAudit {
        stage: String,
        market_id: String,
        order_id: Option<String>,
        token_id: Option<String>,
        action: String,
        status: String,
        details: String,
    },
}

const OUTBOX_QUEUE_CAPACITY: usize = 4_096;
const LANE_HIGH_CAPACITY: usize = 256;
const LANE_MEDIUM_CAPACITY: usize = 384;
const LANE_LOW_CAPACITY: usize = 256;

#[derive(Debug, Clone)]
struct OutboxEvent {
    stage: String,
    market_id: String,
    order_id: Option<String>,
    token_id: Option<String>,
    action: String,
    status: String,
    details: String,
    priority: OutboxPriority,
}

#[cfg(test)]
fn spawn_outbox_writer(pool: SqlitePool) -> mpsc::Sender<OutboxEvent> {
    spawn_outbox_writer_handle(pool).0
}

fn spawn_outbox_writer_handle(
    pool: SqlitePool,
) -> (
    mpsc::Sender<OutboxEvent>,
    tokio::task::JoinHandle<Result<(), String>>,
) {
    let (tx, mut rx) = mpsc::channel::<OutboxEvent>(OUTBOX_QUEUE_CAPACITY);
    let task = tokio::spawn(async move {
        let mut first_error = None;
        while let Some(evt) = rx.recv().await {
            if let Err(error) = db::append_order_outbox(
                &pool,
                &evt.stage,
                &evt.market_id,
                evt.order_id.as_deref(),
                evt.token_id.as_deref(),
                &evt.action,
                &evt.status,
                &evt.details,
            )
            .await
            {
                metrics::inc_outbox_write_fail();
                warn!("Outbox writer failed: {error}");
                first_error.get_or_insert_with(|| error.to_string());
            }
        }
        first_error.map_or(Ok(()), Err)
    });
    (tx, task)
}

fn outbox_emit(tx: &mpsc::Sender<OutboxEvent>, pool: &SqlitePool, evt: OutboxEvent) {
    match tx.try_send(evt) {
        Ok(()) => metrics::inc_outbox_enqueued(),
        Err(e) => {
            let evt = e.into_inner();
            match evt.priority {
                OutboxPriority::Critical => {
                    // Hard-fail safe: persist critical audits directly when queue is congested.
                    let pool = pool.clone();
                    tokio::spawn(async move {
                        match db::append_order_outbox(
                            &pool,
                            &evt.stage,
                            &evt.market_id,
                            evt.order_id.as_deref(),
                            evt.token_id.as_deref(),
                            &evt.action,
                            &evt.status,
                            &evt.details,
                        )
                        .await
                        {
                            Ok(_) => metrics::inc_outbox_enqueued(),
                            Err(err) => {
                                metrics::inc_outbox_write_fail();
                                warn!("Outbox critical direct-write failed: {err}");
                            }
                        }
                    });
                }
                OutboxPriority::BestEffort => {
                    metrics::inc_outbox_dropped();
                    warn!("Outbox queue full/closed: dropping best-effort audit event");
                }
            }
        }
    }
}

fn command_priority(cmd: &ExecutionCommand) -> CommandPriority {
    match cmd {
        // Risk/cancel lane: always highest priority.
        ExecutionCommand::CancelMakerOrdersForShutdown { .. }
        | ExecutionCommand::SetL2Stale { .. }
        | ExecutionCommand::ResetPaperBalance { .. }
        | ExecutionCommand::ResetDrawdownCashPeak { .. }
        | ExecutionCommand::RecheckTerminalMakerOrder { .. }
        | ExecutionCommand::RecheckTerminalTakerOrder { .. }
        | ExecutionCommand::ReconcileRedemption { .. }
        | ExecutionCommand::ReconcileMakerOrders { .. }
        | ExecutionCommand::ReconcileLiveState { .. }
        | ExecutionCommand::RetainL2Tokens { .. }
        | ExecutionCommand::SetVenueRules { .. }
        | ExecutionCommand::RemoveQuoteState { .. }
        | ExecutionCommand::ClearMarketQuotes { .. } => CommandPriority::High,
        // Close lane: exits outrank new placements.
        ExecutionCommand::CloseTrade { .. }
        | ExecutionCommand::PartialCloseTrade { .. }
        | ExecutionCommand::ClosePair { .. } => CommandPriority::Close,
        // Place/replace lane.
        ExecutionCommand::ExecuteTrade { .. }
        | ExecutionCommand::UpsertQuoteState { .. }
        | ExecutionCommand::UpdateInventoryState { .. }
        | ExecutionCommand::SnapshotMakerState { .. } => CommandPriority::Medium,
        // Telemetry lane (shed first under pressure).
        ExecutionCommand::FlushBestBidAsk | ExecutionCommand::AppendOutboxAudit { .. } => {
            CommandPriority::Low
        }
    }
}

fn infer_outbox_priority(stage: &str, action: &str, status: &str) -> OutboxPriority {
    if status.eq_ignore_ascii_case("error")
        || action.contains("cancel")
        || stage.starts_with("reconcile_")
    {
        OutboxPriority::Critical
    } else {
        OutboxPriority::BestEffort
    }
}

fn try_push_lane(
    cmd: ExecutionCommand,
    high: &mut VecDeque<ExecutionCommand>,
    close: &mut VecDeque<ExecutionCommand>,
    medium: &mut VecDeque<ExecutionCommand>,
    low: &mut VecDeque<ExecutionCommand>,
) {
    // One coalesced BBO notification owns the flush flag. Dropping the only
    // notification leaves that flag set forever and stops all future books.
    if matches!(cmd, ExecutionCommand::FlushBestBidAsk)
        && low
            .iter()
            .any(|queued| matches!(queued, ExecutionCommand::FlushBestBidAsk))
    {
        metrics::inc_actor_cmd_drop_low();
        return;
    }
    match command_priority(&cmd) {
        CommandPriority::High => {
            if high.len() >= LANE_HIGH_CAPACITY && !low.is_empty() {
                shed_low_audit(low);
            }
            high.push_back(cmd);
        }
        CommandPriority::Close => {
            let total = close.len() + medium.len();
            if total >= LANE_MEDIUM_CAPACITY && !low.is_empty() {
                shed_low_audit(low);
            }
            close.push_back(cmd);
        }
        CommandPriority::Medium => {
            let total = close.len() + medium.len();
            if total >= LANE_MEDIUM_CAPACITY && !low.is_empty() {
                shed_low_audit(low);
            }
            medium.push_back(cmd);
        }
        CommandPriority::Low => {
            if low.len() >= LANE_LOW_CAPACITY {
                if matches!(cmd, ExecutionCommand::FlushBestBidAsk) {
                    shed_low_audit(low);
                } else {
                    metrics::inc_actor_cmd_drop_low();
                    return;
                }
            }
            low.push_back(cmd);
        }
    }
}

fn shed_low_audit(low: &mut VecDeque<ExecutionCommand>) {
    if let Some(index) = low
        .iter()
        .position(|queued| !matches!(queued, ExecutionCommand::FlushBestBidAsk))
    {
        low.remove(index);
        metrics::inc_actor_cmd_drop_low();
    }
}

/// A suppression reason with its live numbers blanked, so two ticks of the
/// same verdict compare equal.
///
/// Reasons carry the figure that tripped them — `p_up 0.930 > max 0.400`,
/// `spread 200.0bps > max 160.0bps` — and the figure moves every tick. Compared
/// raw, every tick looked like a transition and the dedupe wrote 98 rows in
/// three minutes for one market. Digits are replaced rather than stripped so
/// `p_up # > max #` still reads as what it was.
fn suppression_category(reason: &str) -> String {
    let mut out = String::with_capacity(reason.len());
    let mut in_number = false;
    for ch in reason.chars() {
        if ch.is_ascii_digit() || (in_number && ch == '.') {
            if !in_number {
                out.push('#');
                in_number = true;
            }
        } else {
            in_number = false;
            out.push(ch);
        }
    }
    out
}

fn normalized_execution_mode(raw: &str) -> &str {
    match raw {
        "taker" | "maker" | "hybrid" => raw,
        _ => "hybrid",
    }
}

async fn recent_closed_win_rate(bot: &TradingBot, sample: usize) -> Option<f64> {
    let trades = db::load_recent_closed_trades(&bot.db, sample).await.ok()?;
    let seen = trades.len();
    let wins = trades.iter().filter(|t| t.net_realized_pnl() > 0.0).count();
    if seen == 0 {
        None
    } else {
        Some(wins as f64 / seen as f64)
    }
}

/// Returns `(allowed, reason)` — reason is empty when allowed.
async fn should_take_in_hybrid(bot: &TradingBot, analysis: &ParsedAnalysis) -> (bool, String) {
    if bot.degraded_no_trade_active() {
        return (false, "degraded/no-trade active".into());
    }
    let cfg = bot.control_plane.config.read().await.clone();
    let conf = analysis.confidence;
    let net_ev = analysis.net_ev;
    let signal = analysis.signal;
    let regime = if analysis.regime.is_empty() {
        "UNKNOWN"
    } else {
        analysis.regime.as_str()
    };
    let volatility = analysis.volatility.unwrap_or(0.0);
    let expected_amount = cfg.trade_amount.max(cfg.min_trade_usd).max(0.01);
    let gas_pct = (cfg.trade_gas_usd / expected_amount).max(0.0);

    if signal == Signal::Hold {
        return (false, "signal=Hold".into());
    }

    // Runtime stand-down: if recent closed-trade quality is poor, suppress taker crosses.
    if cfg.hybrid_stand_down {
        if let Some(wr) = recent_closed_win_rate(bot, cfg.hybrid_stand_down_min_closed).await {
            if wr < cfg.hybrid_stand_down_min_win_rate {
                return (
                    false,
                    format!(
                        "stand-down (win_rate={:.1}% < {:.1}%)",
                        wr * 100.0,
                        cfg.hybrid_stand_down_min_win_rate * 100.0
                    ),
                );
            }
        }
    }

    // Taker confidence gate — use config threshold directly.
    let mut conf_gate = cfg.strategy.params.min_confidence;
    let mut ev_gate = cfg.min_ev + cfg.edge_safety_buffer + gas_pct + 0.0015;

    // Regime-aware gates.
    match regime {
        "CHOP" => {
            conf_gate = (conf_gate + 0.08).min(0.995);
            ev_gate += 0.0025;
        }
        "BULL" | "BEAR" | "TREND_UP" | "TREND_DOWN" => {
            conf_gate = (conf_gate - 0.02).max(0.55);
        }
        _ => {}
    }

    // High-volatility periods require extra margin above costs.
    if volatility > 0.006 {
        ev_gate += 0.0015;
    }

    if conf < conf_gate || net_ev < ev_gate {
        return (
            false,
            format!(
                "conf={:.3} (need {:.3}), ev={:.4} (need {:.4}), regime={}, signal={}",
                conf,
                conf_gate,
                net_ev,
                ev_gate,
                regime,
                signal.as_str()
            ),
        );
    }

    (true, String::new())
}

const MAKER_MIN_PRICE: f64 = 0.01;
const MAKER_MAX_PRICE: f64 = 0.99;

#[derive(Debug, Clone)]
struct QuotePlan {
    fair_value: f64,
    p_yes: f64,
    skew_bps: f64,
    target_spread_bps: f64,
    basis_widen_applied: bool,
    basis_widen_bps: f64,
    bid: f64,
    ask: f64,
    size: f64,
    observed_bid: Option<f64>,
    observed_ask: Option<f64>,
    inventory_notional_usd: f64,
}

#[derive(Debug, Clone)]
struct MakerBasisGuard {
    reason: String,
    shadow_only: bool,
}

#[derive(Debug, Clone)]
struct MakerRollGuard {
    reason: String,
    shadow_only: bool,
}

fn signed_quote_move_bps(
    side: &QuoteSide,
    placed_mid: Option<f64>,
    current_mid: Option<f64>,
) -> Option<f64> {
    let placed = placed_mid?;
    let current = current_mid?;
    if placed <= 0.0 || current <= 0.0 {
        return None;
    }
    let move_bps = (current - placed) * 10_000.0;
    Some(match side {
        QuoteSide::Bid => move_bps,
        QuoteSide::Ask => -move_bps,
    })
}

fn maker_basis_guard(cfg: &Config, market_ctx: &MakerMarketContext) -> Option<MakerBasisGuard> {
    if !cfg.maker_backwardation_guard_enabled {
        return None;
    }
    let basis_pct = market_ctx.basis_pct?;
    if basis_pct > cfg.maker_backwardation_guard_pct {
        return None;
    }
    Some(MakerBasisGuard {
        reason: format!(
            "backwardation_guard({basis_pct:.4} <= {:.4})",
            cfg.maker_backwardation_guard_pct
        ),
        shadow_only: cfg.maker_backwardation_guard_shadow_only,
    })
}

fn roll_signal_is_toxic_for_side(
    is_yes_token: Option<bool>,
    side: &QuoteSide,
    roll_signal: f64,
) -> bool {
    let positive_yes_flow = roll_signal > 0.0;
    match (is_yes_token, side) {
        (Some(true), QuoteSide::Ask) => positive_yes_flow,
        (Some(true), QuoteSide::Bid) => !positive_yes_flow,
        (Some(false), QuoteSide::Bid) => positive_yes_flow,
        (Some(false), QuoteSide::Ask) => !positive_yes_flow,
        _ => false,
    }
}

fn maker_roll_guard(
    cfg: &Config,
    is_yes_token: Option<bool>,
    side: &QuoteSide,
    roll_ctx: Option<&RollMicrostructureState>,
) -> Option<MakerRollGuard> {
    if !cfg.maker_roll_toxic_guard_enabled {
        return None;
    }
    let roll = roll_ctx?;
    if roll.confidence < cfg.maker_roll_toxic_min_confidence
        || roll.signal.abs() < cfg.maker_roll_toxic_min_signal
        || roll.sample_count < cfg.maker_roll_toxic_min_sample_count
        || roll.persistence <= 0.0
        || !roll_signal_is_toxic_for_side(is_yes_token, side, roll.signal)
    {
        return None;
    }
    Some(MakerRollGuard {
        reason: format!(
            "roll_guard(side={} signal={:.3} conf={:.3} persistence={:.3} samples={})",
            side.label(),
            roll.signal,
            roll.confidence,
            roll.persistence,
            roll.sample_count
        ),
        shadow_only: cfg.maker_roll_toxic_guard_shadow_only,
    })
}

fn adverse_move_bps(
    side: &QuoteSide,
    placed_mid: Option<f64>,
    current_mid: Option<f64>,
) -> Option<f64> {
    signed_quote_move_bps(side, placed_mid, current_mid).map(|signed| (-signed).max(0.0))
}

fn clamp_price(px: f64) -> f64 {
    px.clamp(MAKER_MIN_PRICE, MAKER_MAX_PRICE)
}

fn round_to_tick(px: f64, rules: &VenueRules) -> f64 {
    clamp_price(rules.round_to_tick(px))
}

fn mid_from_bbo(bbo: &TopOfBook) -> Option<f64> {
    match (bbo.best_bid, bbo.best_ask) {
        (Some(bid), Some(ask)) if ask >= bid && ask > 0.0 => Some((bid + ask) / 2.0),
        (Some(bid), None) if bid > 0.0 => Some(bid),
        (None, Some(ask)) if ask > 0.0 => Some(ask),
        _ => None,
    }
}

fn book_two_sided(bbo: &TopOfBook) -> bool {
    bbo.best_bid.is_some() && bbo.best_ask.is_some()
}

fn maker_l2_not_ready_reason(
    maker_state: &MakerActorState,
    bot_feed_stale: bool,
    bbo: &TopOfBook,
) -> String {
    if maker_state.l2_stale {
        if let Some(code) = maker_state.l2_stale_code.as_ref().filter(|s| !s.is_empty()) {
            return format!("l2_stale({code})");
        }
        if let Some(reason) = maker_state
            .l2_stale_reason
            .as_ref()
            .filter(|s| !s.is_empty())
        {
            return format!("l2_stale({reason})");
        }
        return "l2_stale".to_string();
    }
    if bot_feed_stale {
        return "l2_feed_stale".to_string();
    }
    if bbo.stale {
        return "token_bbo_stale".to_string();
    }
    if !bbo.snapshot_loaded {
        return "snapshot_missing".to_string();
    }
    if !book_two_sided(bbo) {
        return "one_sided_book".to_string();
    }
    "l2_not_ready".to_string()
}

fn maker_snapshot_l2_not_ready_reason(
    l2_stale: bool,
    l2_stale_code: Option<&str>,
    l2_stale_reason: Option<&str>,
    bbo: &TopOfBook,
) -> Option<String> {
    if l2_stale {
        if let Some(code) = l2_stale_code.filter(|s| !s.is_empty()) {
            return Some(format!("l2_stale({code})"));
        }
        if let Some(reason) = l2_stale_reason.filter(|s| !s.is_empty()) {
            return Some(format!("l2_stale({reason})"));
        }
        return Some("l2_stale".to_string());
    }
    if bbo.stale {
        return Some("token_bbo_stale".to_string());
    }
    if !bbo.snapshot_loaded {
        return Some("snapshot_missing".to_string());
    }
    if !book_two_sided(bbo) {
        return Some("one_sided_book".to_string());
    }
    None
}

fn normalize_l2_stale_code(reason: Option<&str>) -> Option<&'static str> {
    let reason = reason?.trim();
    if reason.is_empty() {
        return None;
    }
    if reason == "no target markets" {
        return Some("no_target_markets");
    }
    if reason == "execution_mode=taker" {
        return Some("execution_mode_taker");
    }
    if reason.starts_with("warmup: awaiting snapshots") {
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
    if reason.starts_with("execution_actor_backpressure") {
        return Some("actor_backpressure");
    }
    Some("l2_stale")
}

fn maker_bid_edge(direction: Direction, p_yes: f64, entry_price: f64) -> Option<f64> {
    match direction {
        Direction::BuyYes => Some(p_yes - entry_price),
        Direction::BuyNo => Some((1.0 - p_yes) - entry_price),
        Direction::Unknown => None,
    }
}

/// An order under the venue's minimum is not placed, in simulation as in
/// life. The reason says what quote size would clear it, because that is
/// the operator's decision, not the bot's.
fn venue_min_size_guard(cfg: &Config, rules: &VenueRules, plan: &QuotePlan) -> Option<String> {
    if rules.min_order_size <= 0.0 || plan.size + 1e-9 >= rules.min_order_size {
        return None;
    }
    // Size is notional over the observed mid, so the notional that clears
    // the minimum is the minimum times that same mid.
    let size_ref = if plan.size > 0.0 {
        cfg.maker_quote_notional_usd.max(0.05) / plan.size
    } else {
        0.01
    };
    Some(format!(
        "venue_min_size({:.2} shares < min {:.0}; quote size ${:.2} needs >= ${:.2} at {:.2})",
        plan.size,
        rules.min_order_size,
        cfg.maker_quote_notional_usd,
        rules.min_order_size * size_ref,
        size_ref
    ))
}

fn maker_market_quality_guard(cfg: &Config, bbo: &TopOfBook) -> Option<String> {
    // `(0.52 - 0.50) * 10_000` is 200.00000000000003 in f64, and a book sitting
    // exactly on the threshold was rejected by that noise while the log printed
    // "200.0bps > max 200.0bps". The venue quotes in whole ticks, so the spread
    // is a whole number of bps and rounding it loses nothing.
    // No spread means one side of the book is empty. `?` here used to
    // return None, which reads as "quality fine", and the plan went on to
    // quote against a side that did not exist.
    let Some(spread_bps) = bbo.spread_bps else {
        return Some("market_quality(one_sided_book)".to_string());
    };
    let spread_bps = spread_bps.round();
    if spread_bps > cfg.maker_market_quality_max_spread_bps {
        return Some(format!(
            "market_quality(spread {:.1}bps > max {:.1}bps)",
            spread_bps, cfg.maker_market_quality_max_spread_bps
        ));
    }
    None
}

fn market_yes_mid(state: &MakerActorState, market_id: &str) -> Option<f64> {
    state
        .l2_bbo_by_token
        .values()
        .filter(|b| b.market_id == market_id)
        .find_map(|b| {
            let mid = mid_from_bbo(b)?;
            match b.is_yes_token {
                Some(true) => Some(mid),
                Some(false) => Some(1.0 - mid),
                None => None,
            }
        })
}

async fn market_inventory_notional(
    bot: &TradingBot,
    state: &MakerActorState,
    market_id: &str,
) -> f64 {
    let tracked = state.inventory_notional_usd(market_id);
    if tracked.abs() > 0.0 {
        return tracked;
    }

    let trades = bot.portfolio_state.trades.read().await;
    trades
        .iter()
        .filter(|t| t.is_open() && t.market_id == market_id)
        .fold(0.0, |acc, t| {
            if t.direction_kind() == Direction::BuyYes {
                acc + t.amount
            } else if t.direction_kind() == Direction::BuyNo {
                acc - t.amount
            } else {
                acc
            }
        })
}

async fn market_gross_open_notional_usd(bot: &TradingBot, market_id: &str) -> f64 {
    let trades = bot.portfolio_state.trades.read().await;
    trades
        .iter()
        .filter(|t| t.is_open() && t.market_id == market_id)
        .map(|t| t.amount.max(0.0))
        .sum()
}

async fn market_token_inventory_shares(
    bot: &TradingBot,
    state: &MakerActorState,
    market_id: &str,
    is_yes_token: Option<bool>,
) -> f64 {
    if let Some(inventory) = state.inventory.get(market_id) {
        return match is_yes_token {
            Some(true) => inventory.net_yes_shares.max(0.0),
            Some(false) => inventory.net_no_shares.max(0.0),
            None => 0.0,
        };
    }

    let trades = bot.portfolio_state.trades.read().await;
    trades
        .iter()
        .filter(|t| t.is_open() && t.market_id == market_id)
        .filter_map(|t| match (is_yes_token, t.direction_kind()) {
            (Some(true), Direction::BuyYes) => Some(t.shares.max(0.0)),
            (Some(false), Direction::BuyNo) => Some(t.shares.max(0.0)),
            _ => None,
        })
        .sum()
}

#[derive(Debug, Clone)]
struct MakerEntryBlock {
    reason: String,
}

async fn evaluate_maker_entry(
    bot: &TradingBot,
    cfg: &Config,
    market_id: &str,
    direction: Direction,
    entry_price: f64,
    amount: f64,
    continuing_paper_order: Option<&str>,
) -> Result<(), MakerEntryBlock> {
    if bot.is_drawdown_halted().await {
        return Err(MakerEntryBlock {
            reason: "percentage drawdown halt".into(),
        });
    }
    let analysis_value = bot.analysis_state.last_analysis.read().await.clone();
    let analysis = ParsedAnalysis::from_value(&analysis_value);
    let signal = match direction {
        Direction::BuyYes => Signal::BuyYes,
        Direction::BuyNo => Signal::BuyNo,
        Direction::Unknown => Signal::Hold,
    };
    let analysis_matches_market = analysis.market.market_id == market_id;
    let analysis_stale = bot.is_analysis_stale().await;
    if !analysis_matches_market {
        return Err(MakerEntryBlock {
            reason: format!("no fresh analysis for market {market_id}"),
        });
    }
    if analysis_stale {
        return Err(MakerEntryBlock {
            reason: "analysis stale".to_string(),
        });
    }

    // A collector bids only the side its model favours, by the same margin
    // the BUY_NO guard already uses, applied symmetrically: NO needs
    // p_up <= max_p_up, YES needs p_up >= 1 - max_p_up. Without a view it
    // does not buy. A market maker bids both sides; that is the mode this
    // replaces.
    if cfg.maker_hold_positions {
        let p_up = analysis.p_up.unwrap_or(0.5);
        let margin = cfg.maker_buy_no_max_p_up;
        let favoured = match direction {
            Direction::BuyYes => p_up >= 1.0 - margin,
            Direction::BuyNo => p_up <= margin,
            Direction::Unknown => false,
        };
        if !favoured {
            return Err(MakerEntryBlock {
                reason: format!(
                    "hold_mode: model does not favour {} (p_up {:.3}, needs {} {:.2})",
                    signal.as_str(),
                    p_up,
                    if direction == Direction::BuyYes {
                        ">="
                    } else {
                        "<="
                    },
                    if direction == Direction::BuyYes {
                        1.0 - margin
                    } else {
                        margin
                    },
                ),
            });
        }
    }

    if cfg.maker_buy_no_guard_enabled && direction == Direction::BuyNo {
        let p_up = analysis
            .p_up
            .unwrap_or(analysis.market.price)
            .clamp(0.0, 1.0);
        if p_up > cfg.maker_buy_no_max_p_up {
            return Err(MakerEntryBlock {
                reason: format!(
                    "maker BUY_NO guard: p_up {:.3} > max {:.3}",
                    p_up, cfg.maker_buy_no_max_p_up
                ),
            });
        }
        if entry_price > cfg.maker_buy_no_max_entry_price {
            return Err(MakerEntryBlock {
                reason: format!(
                    "maker BUY_NO price {:.3} > max {:.3}",
                    entry_price, cfg.maker_buy_no_max_entry_price
                ),
            });
        }
        let edge = maker_bid_edge(direction, p_up, entry_price).unwrap_or_default();
        if edge + 1e-9 < cfg.maker_buy_no_min_edge {
            return Err(MakerEntryBlock {
                reason: format!(
                    "maker BUY_NO edge {:.3} < min {:.3}",
                    edge, cfg.maker_buy_no_min_edge
                ),
            });
        }
    }

    let continuing_trade_id = if let Some(order_id) = continuing_paper_order {
        match db::paper::continuing_lot(
            &bot.db,
            order_id,
            market_id,
            direction.as_str(),
            entry_price,
            amount,
        )
        .await
        {
            Ok(id) => id,
            Err(error) => {
                return Err(MakerEntryBlock {
                    reason: format!("paper remainder validation failed: {error}"),
                })
            }
        }
    } else {
        None
    };
    let open_trades = bot.portfolio_state.trades.read().await;
    let entry_snapshot = EntryCoordinationSnapshot {
        has_same_market_strategy_position: open_trades.iter().any(|t| {
            Some(t.id) != continuing_trade_id
                && t.market_id == market_id
                && t.is_open()
                && t.strategy == "maker"
        }),
        open_scalps: open_trades
            .iter()
            .filter(|t| t.is_open() && t.strategy == "scalp")
            .count() as u32,
        open_conviction: open_trades
            .iter()
            .filter(|t| Some(t.id) != continuing_trade_id && t.is_open() && t.strategy != "scalp")
            .count() as u32,
    };
    drop(open_trades);

    let entry_coordination = EntryCoordinationProposal {
        signal,
        strategy: "maker".to_string(),
        market_id: market_id.to_string(),
        market_yes_price: analysis.market.price,
        token_price: entry_price,
        p_up: analysis.p_up,
        liquidity: analysis.market.liquidity,
    };
    if let Err(reject) = evaluate_entry_coordination(cfg, &entry_snapshot, &entry_coordination) {
        return Err(MakerEntryBlock {
            reason: reject.to_string(),
        });
    }

    let time_remaining_secs = crate::bot::analyze::parse_utc(&analysis.market.end_date)
        .map(|end_dt| end_dt.signed_duration_since(Utc::now()).num_milliseconds() as f64 / 1000.0);
    let risk_snapshot = bot
        .trade_risk_snapshot(cfg, cfg.mode_kind() == BotMode::Live)
        .await;
    // Live maker admission separately requires venue-confirmed zero maker
    // fees. Compare the actual quote price to the selected posterior too.
    let quote_net_ev = super::execute::entry_return_at_price(&analysis, signal, entry_price, 0.0)
        .ok_or_else(|| MakerEntryBlock {
        reason: "maker quote has no valid outcome probability".into(),
    })?;
    let risk_proposal = ProposedTrade {
        signal,
        strategy: "maker".to_string(),
        confidence: analysis.confidence,
        funding_rate: analysis.funding_rate,
        regime: analysis.regime.to_ascii_uppercase(),
        token_price: entry_price,
        liquidity: analysis.market.liquidity,
        entry_hour_utc: Utc::now().hour(),
        time_remaining_secs,
        btc_change_pct: analysis.btc_change_pct,
        net_ev: quote_net_ev,
        forced: analysis.forced,
        hail_mary: false,
        requested_amount: amount,
    };
    let mut entry_cfg = cfg.clone();
    if continuing_trade_id.is_some() {
        // The initial whole quote met the economic minimum. A remaining
        // tranche is the same admitted order and the same position slot.
        entry_cfg.min_trade_usd = entry_cfg.min_trade_usd.min(amount);
    }
    let decision = match evaluate_trade_guardrails(&entry_cfg, &risk_snapshot, &risk_proposal) {
        Ok(decision) => decision,
        Err(reject) => {
            return Err(MakerEntryBlock {
                reason: reject.to_string(),
            });
        }
    };
    if decision.approved_amount + 1e-9 < amount {
        return Err(MakerEntryBlock {
            reason: format!(
                "approved amount {:.4} below crossed quote {:.4}",
                decision.approved_amount, amount
            ),
        });
    }

    Ok(())
}

async fn fair_yes_probability(
    bot: &TradingBot,
    market_id: &str,
    yes_mid_fallback: Option<f64>,
) -> (f64, &'static str) {
    let analysis = bot.analysis_state.last_analysis.read().await.clone();
    let parsed = ParsedAnalysis::from_value(&analysis);
    if parsed.market.market_id == market_id {
        if let Some(p_up) = parsed.p_up {
            return (
                p_up.clamp(MAKER_MIN_PRICE, MAKER_MAX_PRICE),
                "analysis.p_up",
            );
        }
        let mkt_px = parsed.market.price;
        if mkt_px > 0.0 && mkt_px <= 1.0 {
            return (
                mkt_px.clamp(MAKER_MIN_PRICE, MAKER_MAX_PRICE),
                "analysis.market_price",
            );
        }
    }
    if let Some(mid) = yes_mid_fallback {
        return (mid.clamp(MAKER_MIN_PRICE, MAKER_MAX_PRICE), "l2_mid");
    }
    (0.5, "neutral")
}

async fn current_maker_market_context(bot: &TradingBot) -> MakerMarketContext {
    let futures_cache = bot.market_state.futures_cache.lock().await;
    let (basis_pct, mark_price, index_price) = futures_cache
        .as_ref()
        .filter(|cache| cache.ts.elapsed() < Duration::from_secs(60))
        .map(|cache| (cache.basis_pct, cache.mark_price, cache.index_price))
        .unwrap_or((None, None, None));
    drop(futures_cache);

    let btc_spot_price = crate::signals::price_bus::shared_price_bus()
        .latest_price("BTCUSDT", 5.0)
        .await
        .or_else(|| {
            bot.market_state
                .live_btc_px
                .try_read()
                .ok()
                .and_then(|cached| {
                    if cached.0 > 0.0 && cached.1.elapsed() < Duration::from_secs(30) {
                        Some(cached.0)
                    } else {
                        None
                    }
                })
        });
    let price_to_beat_distance_pct = bot
        .analysis_state
        .last_analysis
        .read()
        .await
        .get("price_to_beat_distance_pct")
        .and_then(|value| value.as_f64());

    MakerMarketContext {
        basis_pct,
        mark_price,
        index_price,
        btc_spot_price,
        price_to_beat_distance_pct,
    }
}

/// Whether a resting quote would have filled against this top of book.
///
/// `on_touch` grants the fill when the far side reaches the quote's price —
/// first in line, full size. Otherwise the far side must trade *through* the
/// price: a level that is touched and not consumed fills nobody at the back of
/// the queue, which is where a new order sits. See
/// `Config::maker_sim_fill_on_touch`.
#[cfg(test)]
fn quote_crosses_top_of_book(quote: &QuoteState, bbo: &TopOfBook, on_touch: bool) -> bool {
    match quote.side {
        QuoteSide::Bid => bbo.best_ask.is_some_and(|ask| {
            if on_touch {
                ask <= quote.price + f64::EPSILON
            } else {
                ask < quote.price - f64::EPSILON
            }
        }),
        QuoteSide::Ask => bbo.best_bid.is_some_and(|bid| {
            if on_touch {
                bid + f64::EPSILON >= quote.price
            } else {
                bid > quote.price + f64::EPSILON
            }
        }),
    }
}

fn direct_fill_direction(is_yes_token: Option<bool>) -> Option<Direction> {
    match is_yes_token {
        Some(true) => Some(Direction::BuyYes),
        Some(false) => Some(Direction::BuyNo),
        None => None,
    }
}

async fn refresh_market_inventory_from_portfolio(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    market_id: &str,
) -> InventoryState {
    let trades = bot.portfolio_state.trades.read().await;
    let mut net_yes_shares = 0.0;
    let mut net_no_shares = 0.0;
    let mut net_notional_usd = 0.0;

    for trade in trades
        .iter()
        .filter(|t| t.is_open() && t.market_id == market_id)
    {
        match trade.direction_kind() {
            Direction::BuyYes => {
                net_yes_shares += trade.shares.max(0.0);
                net_notional_usd += trade.amount.max(0.0);
            }
            Direction::BuyNo => {
                net_no_shares += trade.shares.max(0.0);
                net_notional_usd -= trade.amount.max(0.0);
            }
            Direction::Unknown => {}
        }
    }
    drop(trades);

    maker_state.update_inventory(
        market_id.to_string(),
        net_yes_shares,
        net_no_shares,
        net_notional_usd,
    )
}

// Admission binds quote identity, exact quantity and paper risk limits together.
#[allow(clippy::too_many_arguments)]
async fn admit_simulated_maker_order(
    bot: &TradingBot,
    cfg: &Config,
    market_id: &str,
    token_id: &str,
    is_yes_token: Option<bool>,
    price: f64,
    size: f64,
    order_id: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg.mode_kind() != BotMode::Live && bot.paper_execution_allowed().await,
        "paper admission requires an isolated non-live process without wallet-backed cash"
    );
    anyhow::ensure!(!bot.is_drawdown_halted().await, "percentage drawdown halt");
    let direction = direct_fill_direction(is_yes_token)
        .ok_or_else(|| anyhow::anyhow!("paper quote lacks token direction"))?;
    let analysis_value = bot.analysis_state.last_analysis.read().await.clone();
    let analysis = ParsedAnalysis::from_value(&analysis_value);
    let shadow = cfg.shadow_trading || cfg.mode_kind() == BotMode::Shadow;
    let quote_expected_return = super::execute::entry_return_at_price(
        &analysis,
        Signal::parse(direction.as_str()),
        price,
        0.0,
    )
    .ok_or_else(|| anyhow!("paper quote lacks a valid outcome probability"))?;
    let template = Trade {
        timestamp: Utc::now().to_rfc3339(),
        market_id: market_id.into(),
        question: if analysis.market.question.is_empty() {
            market_id.into()
        } else {
            analysis.market.question.clone()
        },
        direction: direction.as_str().into(),
        entry_price: price,
        expected_value: quote_expected_return,
        reason: "maker passive fill BID depth_proxy".into(),
        confidence: analysis.confidence,
        status: "OPEN".into(),
        mode: if shadow {
            "shadow".into()
        } else {
            cfg.mode.clone()
        },
        source: if shadow {
            "shadow".into()
        } else {
            "bot".into()
        },
        end_date: analysis.market.end_date.clone(),
        partial_closed: Some(0),
        raw_confidence: analysis.raw_confidence,
        order_id: Some(order_id.into()),
        token_id: Some(token_id.into()),
        fok_failed: Some(0),
        signals_json: analysis.signals.as_ref().map(|value| value.to_string()),
        calibration_version: analysis_value
            .get("calibration_version")
            .and_then(Value::as_str)
            .map(str::to_owned),
        run_id: Some(bot.run_id().into()),
        strategy: "maker".into(),
        ..Trade::default()
    };
    let origin = bot.control_plane.config.read().await;
    anyhow::ensure!(
        bot.clob.is_none()
            && matches!(
                origin.mode_kind(),
                BotMode::Paper | BotMode::Shadow | BotMode::Simulation
            ),
        "paper admission requires an isolated non-live process without wallet-backed cash"
    );
    let admitted = db::paper::admit_order(
        &bot.db,
        &template,
        size,
        bot.balance().await,
        cfg.min_trade_usd,
        cfg.max_open_positions,
        cfg.maker_max_inventory_notional_usd,
        cfg.max_exposure_pct,
    )
    .await;
    drop(origin);
    admitted
}

async fn open_simulated_maker_trade(
    bot: &TradingBot,
    cfg: &Config,
    order_id: &str,
    observation_id: &str,
    entry_price: f64,
    shares: f64,
    fill_details: &str,
) -> Option<db::paper::AppliedPaperFill> {
    if cfg.mode_kind() == BotMode::Live
        || !order_id.starts_with("sim-mkr-")
        || !bot.paper_execution_allowed().await
    {
        return None;
    }
    if bot.is_drawdown_halted().await {
        if let Err(error) = db::paper::cancel_order(&bot.db, order_id).await {
            bot.set_degraded_no_trade(true);
            bot.halt_trading("paper_maker_cancel_persistence", error.to_string())
                .await;
        }
        return None;
    }
    // Keep the current accounting origin stable through durable accounting
    // and hot cash publication, including a concurrent runtime config change.
    let origin = bot.control_plane.config.read().await;
    if bot.clob.is_some()
        || !matches!(
            origin.mode_kind(),
            BotMode::Paper | BotMode::Shadow | BotMode::Simulation
        )
    {
        return None;
    }
    let applied = match db::paper::apply_entry_fill(
        &bot.db,
        order_id,
        observation_id,
        shares,
        entry_price,
        bot.balance().await,
        fill_details,
    )
    .await
    {
        Ok(applied) => applied,
        Err(error) => {
            metrics::inc_db_insert_fail();
            warn!(%error, order_id, "paper maker fill accounting refused; no balance debit");
            return None;
        }
    };
    if applied.debit > 0.0 {
        bot.set_balance((bot.balance().await - applied.debit).max(0.0))
            .await;
    }
    let mut portfolio = bot.portfolio_state.trades.write().await;
    if let Some(existing) = portfolio
        .iter_mut()
        .find(|trade| trade.id == applied.trade.id)
    {
        *existing = applied.trade.clone();
    } else {
        portfolio.push(applied.trade.clone());
    }
    drop(portfolio);
    drop(origin);
    if applied.new_position {
        metrics::inc_trades_opened();
    }
    Some(applied)
}

async fn close_against_existing_inventory(
    bot: &TradingBot,
    market_id: &str,
    direction: Direction,
    exit_price: f64,
    max_shares: f64,
    fill_snapshot: &PublicBookSnapshot,
) -> (f64, usize, usize) {
    if !exit_price.is_finite()
        || exit_price <= 0.0
        || exit_price >= 1.0
        || !max_shares.is_finite()
        || max_shares <= 0.0
    {
        return (0.0, 0, 0);
    }

    let mut candidates: Vec<Trade> = bot
        .portfolio_state
        .trades
        .read()
        .await
        .iter()
        .filter(|t| t.is_open() && t.market_id == market_id && t.direction_kind() == direction)
        .cloned()
        .collect();
    candidates.sort_by_key(|t| t.id);

    let mut consumed = 0.0;
    let mut closed_full = 0usize;
    let mut closed_partial = 0usize;

    for trade in candidates {
        if bot
            .trade_lifecycle
            .partial_close_intents
            .read()
            .await
            .contains_key(&trade.id)
        {
            continue;
        }
        let remaining = max_shares - consumed;
        if remaining <= 1e-9 {
            break;
        }

        let take = remaining.min(trade.shares.max(0.0));
        if take <= 1e-9 {
            continue;
        }

        if (take - trade.shares).abs() <= 1e-9 || take + 1e-9 >= trade.shares {
            let details = SimTradePriceEvidence::new(
                "exit",
                "maker_flatten",
                "sell",
                exit_price,
                exit_price,
                trade.shares,
                "simulated maker flatten exit",
                fill_snapshot.clone(),
            )
            .as_details_json();
            bot.trade_lifecycle
                .pending_sim_exit_evidence
                .write()
                .await
                .insert(
                    trade.id,
                    PendingSimExitEvidence {
                        order_id: format!("maker-exit-{}", trade.id),
                        phase: "exit".to_string(),
                        intended_price: exit_price,
                        avg_price: exit_price,
                        shares: trade.shares,
                        source: "maker_flatten".to_string(),
                        details,
                    },
                );
            if bot
                .close_trade_with_intent(trade.id, Some(exit_price), CloseIntent::PassiveFill)
                .await
                .is_some()
            {
                consumed += trade.shares;
                closed_full += 1;
            } else {
                bot.trade_lifecycle
                    .pending_sim_exit_evidence
                    .write()
                    .await
                    .remove(&trade.id);
            }
        } else {
            bot.trade_lifecycle
                .pending_sim_exit_evidence
                .write()
                .await
                .insert(
                    trade.id,
                    PendingSimExitEvidence {
                        order_id: format!("partial-{}-{:016x}", trade.id, trade.shares.to_bits()),
                        phase: "partial_exit".to_string(),
                        intended_price: exit_price,
                        avg_price: exit_price,
                        shares: take,
                        source: "partial_close".to_string(),
                        details: SimTradePriceEvidence::new(
                            "partial_exit",
                            "partial_close",
                            "sell",
                            exit_price,
                            exit_price,
                            take,
                            &format!(
                                "partial {:.0}% close via maker ask cross",
                                (take / trade.shares) * 100.0
                            ),
                            fill_snapshot.clone(),
                        )
                        .as_details_json(),
                    },
                );
            if bot
                .partial_close_trade_with_intent(
                    trade.id,
                    exit_price,
                    take / trade.shares,
                    CloseIntent::PassiveFill,
                )
                .await
                .is_some()
            {
                consumed += take;
                closed_partial += 1;
            } else {
                bot.trade_lifecycle
                    .pending_sim_exit_evidence
                    .write()
                    .await
                    .remove(&trade.id);
            }
        }
    }

    (consumed, closed_full, closed_partial)
}

async fn simulate_passive_fill_for_quote(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    cfg: &Config,
    quote: QuoteState,
    bbo: &TopOfBook,
    market_ctx: &MakerMarketContext,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> bool {
    // Placement provenance survives config changes. A previously submitted
    // venue order must never acquire an invented fill when mode is toggled.
    if cfg.mode_kind() == BotMode::Live
        || !quote.order_id.starts_with("sim-mkr-")
        || !bot.paper_execution_allowed().await
    {
        return false;
    }
    if quote.side == QuoteSide::Bid && bot.is_drawdown_halted().await {
        if cancel_quote(
            bot,
            &quote.order_id,
            &quote.market_id,
            &quote.token_id,
            "percentage drawdown halt",
            outbox_tx,
        )
        .await
        {
            maker_state.remove_quote(&quote.order_id);
        }
        return false;
    }
    let now = Instant::now();
    let Some(quote_age) = maker_state.quote_age(&quote.order_id, now) else {
        return false;
    };
    let Ok(observed_at) = chrono::DateTime::parse_from_rfc3339(&bbo.updated_at) else {
        return false;
    };
    let mut settings = super::simulation::PaperFillSettings::from(cfg);
    settings.latency_ms = settings.latency_ms.max(cfg.maker_min_quote_lifetime_ms);
    let state = maker_state
        .paper_fill_state
        .get(&quote.order_id)
        .cloned()
        .unwrap_or_else(|| super::simulation::MakerPaperState::new(quote.size, settings));
    let (side, contra_price, contra_size) = match quote.side {
        QuoteSide::Bid => (
            super::simulation::PaperSide::Buy,
            bbo.best_ask,
            bbo.best_ask_size,
        ),
        QuoteSide::Ask => (
            super::simulation::PaperSide::Sell,
            bbo.best_bid,
            bbo.best_bid_size,
        ),
    };
    let step = super::simulation::capture_maker_fill_step(&super::simulation::MakerPaperInput {
        settings,
        state,
        side,
        quote_price: quote.price,
        remaining_shares: quote.size,
        age_ms: quote_age.as_millis().min(u128::from(u64::MAX)) as u64,
        sequence: bbo.sequence,
        observed_at_us: observed_at.timestamp_micros(),
        contra_price,
        contra_size,
        usable_book: bbo.snapshot_loaded && !bbo.stale,
    });
    let fill_shares = step.fill_shares;
    maker_state
        .paper_fill_state
        .insert(quote.order_id.clone(), step.state);
    if fill_shares <= 1e-9 {
        return false;
    }

    let mut opened = 0usize;
    let mut closed_full = 0usize;
    let mut closed_partial = 0usize;
    let mut consumed_shares = 0.0;

    match quote.side {
        QuoteSide::Bid => {
            if direct_fill_direction(bbo.is_yes_token).is_none() {
                let _ = maker_state.remove_quote(&quote.order_id);
                return false;
            }
            let snapshot = PublicBookSnapshot::from_bbo(
                &quote.token_id,
                bbo.best_bid,
                bbo.best_bid_size,
                bbo.best_ask,
                bbo.best_ask_size,
                mid_from_bbo(bbo),
                "clob_ws_bbo",
            );
            let fill_reason = format!("maker passive fill {} depth_proxy", quote.side.label());
            let fill_details = SimTradePriceEvidence::new(
                "entry",
                "maker_passive",
                "buy",
                quote.price,
                quote.price,
                fill_shares,
                &fill_reason,
                snapshot,
            )
            .as_details_json();
            let observation_id = format!("{}:{:?}", bbo.updated_at, bbo.sequence);
            if let Some(applied) = open_simulated_maker_trade(
                bot,
                cfg,
                &quote.order_id,
                &observation_id,
                quote.price,
                fill_shares,
                &fill_details,
            )
            .await
            {
                opened += usize::from(applied.new_position);
                consumed_shares = applied.applied_shares;
            }
            maker_state.set_post_fill_cooldown(
                &quote.market_id,
                &quote.token_id,
                &QuoteSide::Bid,
                now,
                cfg.maker_fill_cooldown_ms,
            );
        }
        QuoteSide::Ask => {
            // An ask that was resting before hold mode was switched on must
            // not become the flatten it was never meant to be.
            if cfg.maker_hold_positions {
                let _ = maker_state.remove_quote(&quote.order_id);
                return false;
            }
            let Some(close_direction) = direct_fill_direction(bbo.is_yes_token) else {
                let _ = maker_state.remove_quote(&quote.order_id);
                return false;
            };
            let snapshot = PublicBookSnapshot::from_bbo(
                &quote.token_id,
                bbo.best_bid,
                bbo.best_bid_size,
                bbo.best_ask,
                bbo.best_ask_size,
                mid_from_bbo(bbo),
                "clob_ws_bbo",
            );
            let (consumed, full, partial) = close_against_existing_inventory(
                bot,
                &quote.market_id,
                close_direction,
                quote.price,
                fill_shares,
                &snapshot,
            )
            .await;
            closed_full += full;
            closed_partial += partial;
            consumed_shares = consumed;
            if full > 0 || partial > 0 {
                maker_state.set_post_fill_cooldown(
                    &quote.market_id,
                    &quote.token_id,
                    &QuoteSide::Ask,
                    now,
                    cfg.maker_fill_cooldown_ms,
                );
            }
        }
    }

    if consumed_shares <= 1e-9 {
        return false;
    }
    let remaining = (quote.size - consumed_shares).max(0.0);
    if let Some(context) = maker_state.quote_context(&quote.order_id).cloned() {
        emit_quote_telemetry(
            bot,
            maker_state,
            outbox_tx,
            if remaining > 1e-9 {
                "partial_fill"
            } else {
                "fill"
            },
            Some("depth_proxy"),
            &context,
            Some(bbo),
            market_ctx,
            Some(quote_age.as_millis().min(u128::from(u64::MAX)) as u64),
        );
    }
    if remaining <= 1e-9 {
        let _ = maker_state.remove_quote(&quote.order_id);
    } else if let Some(resting) = maker_state.quotes.get_mut(&quote.order_id) {
        resting.size = remaining;
        resting.status = "PARTIALLY_FILLED".into();
        resting.updated_at = Utc::now().to_rfc3339();
        maker_state.mark_event();
    }
    let inventory =
        refresh_market_inventory_from_portfolio(bot, maker_state, &quote.market_id).await;
    outbox_emit(
        outbox_tx,
        &bot.db,
        OutboxEvent {
            stage: "maker_quote".to_string(),
            market_id: quote.market_id.clone(),
            order_id: Some(quote.order_id.clone()),
            token_id: Some(quote.token_id.clone()),
            action: "fill".to_string(),
            status: "ok".to_string(),
            details: format!(
                "side={} quote_px={:.4} shares={:.6} remaining={:.6} depth_fraction={:.4} max_fill_fraction={:.4} opened={} closed_full={} closed_partial={} inventory_yes={:.6} inventory_no={:.6} inventory_notional={:.4}",
                quote.side.label(),
                quote.price,
                consumed_shares,
                remaining,
                cfg.sim_available_depth_fraction,
                cfg.maker_sim_max_fill_fraction,
                opened,
                closed_full,
                closed_partial,
                inventory.net_yes_shares,
                inventory.net_no_shares,
                inventory.net_notional_usd,
            ),
            priority: OutboxPriority::Critical,
        },
    );
    true
}

async fn simulate_passive_maker_fills(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    cfg: &Config,
    bbo: &TopOfBook,
    market_ctx: &MakerMarketContext,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> usize {
    if cfg.mode_kind() == BotMode::Live || !bot.paper_execution_allowed().await {
        return 0;
    }

    let quotes: Vec<QuoteState> = maker_state
        .quotes
        .values()
        .filter(|q| {
            q.market_id == bbo.market_id
                && q.token_id == bbo.token_id
                && q.order_id.starts_with("sim-mkr-")
        })
        .cloned()
        .collect();

    let mut filled = 0usize;
    for quote in quotes {
        if simulate_passive_fill_for_quote(bot, maker_state, cfg, quote, bbo, market_ctx, outbox_tx)
            .await
        {
            filled += 1;
        }
    }
    filled
}

// Telemetry records the same quote, book and model snapshots used for the decision.
#[allow(clippy::too_many_arguments)]
fn build_quote_telemetry_context(
    order_id: String,
    market_id: &str,
    token_id: &str,
    side: &QuoteSide,
    desired_price: f64,
    desired_size: f64,
    bbo: &TopOfBook,
    plan: &QuotePlan,
    market_ctx: &MakerMarketContext,
    roll_ctx: Option<&RollMicrostructureState>,
) -> QuoteTelemetryContext {
    QuoteTelemetryContext {
        order_id,
        market_id: market_id.to_string(),
        token_id: token_id.to_string(),
        is_yes_token: bbo.is_yes_token,
        side: side.clone(),
        quote_price: desired_price,
        quote_size: desired_size,
        fair_value: plan.fair_value,
        p_yes: plan.p_yes,
        inventory_notional_usd: plan.inventory_notional_usd,
        target_spread_bps: plan.target_spread_bps,
        basis_widen_applied: plan.basis_widen_applied,
        basis_widen_bps: plan.basis_widen_bps,
        placed_bid: bbo.best_bid,
        placed_bid_size: bbo.best_bid_size,
        placed_ask: bbo.best_ask,
        placed_ask_size: bbo.best_ask_size,
        placed_mid: mid_from_bbo(bbo),
        placed_spread_bps: bbo.spread_bps,
        basis_pct: market_ctx.basis_pct,
        mark_price: market_ctx.mark_price,
        index_price: market_ctx.index_price,
        btc_spot_price: market_ctx.btc_spot_price,
        price_to_beat_distance_pct: market_ctx.price_to_beat_distance_pct,
        roll_signal: roll_ctx.map(|r| r.signal),
        roll_confidence: roll_ctx.map(|r| r.confidence),
        roll_persistence: roll_ctx.map(|r| r.persistence),
        roll_recent_delta_bps: roll_ctx.map(|r| r.recent_delta_bps),
        roll_sample_count: roll_ctx.map(|r| r.sample_count),
        placed_at: Utc::now().to_rfc3339(),
    }
}

// The emission binds quote identity and current market observations without state mutation.
#[allow(clippy::too_many_arguments)]
fn emit_quote_telemetry(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
    event: &str,
    reason: Option<&str>,
    context: &QuoteTelemetryContext,
    current_bbo: Option<&TopOfBook>,
    current_market_ctx: &MakerMarketContext,
    quote_age_ms: Option<u64>,
) {
    let current_mid = current_bbo.and_then(mid_from_bbo);
    let current_roll = maker_state.roll_by_token.get(&context.token_id).cloned();
    let event_payload = MakerQuoteTelemetryEvent {
        event: event.to_string(),
        reason: reason.map(str::to_string),
        order_id: context.order_id.clone(),
        market_id: context.market_id.clone(),
        token_id: context.token_id.clone(),
        is_yes_token: context.is_yes_token,
        side: context.side.clone(),
        quote_price: context.quote_price,
        quote_size: context.quote_size,
        quote_age_ms,
        fair_value: context.fair_value,
        p_yes: context.p_yes,
        inventory_notional_usd: context.inventory_notional_usd,
        target_spread_bps: context.target_spread_bps,
        basis_widen_applied: context.basis_widen_applied,
        basis_widen_bps: context.basis_widen_bps,
        placed_mid: context.placed_mid,
        current_mid,
        mid_move_bps: match (context.placed_mid, current_mid) {
            (Some(placed), Some(current)) if placed > 0.0 && current > 0.0 => {
                Some((current - placed) * 10_000.0)
            }
            _ => None,
        },
        signed_quote_move_bps: signed_quote_move_bps(
            &context.side,
            context.placed_mid,
            current_mid,
        ),
        adverse_move_bps: adverse_move_bps(&context.side, context.placed_mid, current_mid),
        placed_spread_bps: context.placed_spread_bps,
        current_spread_bps: current_bbo.and_then(|b| b.spread_bps),
        placed_bid: context.placed_bid,
        current_bid: current_bbo.and_then(|b| b.best_bid),
        placed_ask: context.placed_ask,
        current_ask: current_bbo.and_then(|b| b.best_ask),
        placed_bid_size: context.placed_bid_size,
        current_bid_size: current_bbo.and_then(|b| b.best_bid_size),
        placed_ask_size: context.placed_ask_size,
        current_ask_size: current_bbo.and_then(|b| b.best_ask_size),
        basis_pct: context.basis_pct,
        current_basis_pct: current_market_ctx.basis_pct,
        basis_delta_pct: match (context.basis_pct, current_market_ctx.basis_pct) {
            (Some(placed), Some(current)) => Some(current - placed),
            _ => None,
        },
        mark_price: context.mark_price,
        current_mark_price: current_market_ctx.mark_price,
        index_price: context.index_price,
        current_index_price: current_market_ctx.index_price,
        btc_spot_price: context.btc_spot_price,
        current_btc_spot_price: current_market_ctx.btc_spot_price,
        btc_move_bps: match (context.btc_spot_price, current_market_ctx.btc_spot_price) {
            (Some(placed), Some(current)) if placed > 0.0 && current > 0.0 => {
                Some((current - placed) / placed * 10_000.0)
            }
            _ => None,
        },
        price_to_beat_distance_pct: context.price_to_beat_distance_pct,
        current_price_to_beat_distance_pct: current_market_ctx.price_to_beat_distance_pct,
        roll_signal: context.roll_signal,
        current_roll_signal: current_roll.as_ref().map(|r| r.signal),
        roll_confidence: context.roll_confidence,
        current_roll_confidence: current_roll.as_ref().map(|r| r.confidence),
        roll_persistence: context.roll_persistence,
        current_roll_persistence: current_roll.as_ref().map(|r| r.persistence),
        roll_recent_delta_bps: context.roll_recent_delta_bps,
        current_roll_recent_delta_bps: current_roll.as_ref().map(|r| r.recent_delta_bps),
        roll_sample_count: context.roll_sample_count,
        current_roll_sample_count: current_roll.as_ref().map(|r| r.sample_count),
        observed_at: Utc::now().to_rfc3339(),
    };
    maker_state.record_quote_telemetry(event_payload.clone());
    let details = serde_json::to_string(&event_payload)
        .unwrap_or_else(|_| "{\"event\":\"encode_error\"}".to_string());
    outbox_emit(
        outbox_tx,
        &bot.db,
        OutboxEvent {
            stage: "maker_quote_telemetry".to_string(),
            market_id: context.market_id.clone(),
            order_id: Some(context.order_id.clone()),
            token_id: Some(context.token_id.clone()),
            action: event.to_string(),
            status: "observed".to_string(),
            details,
            priority: OutboxPriority::BestEffort,
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn emit_guard_telemetry(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
    event: &str,
    reason: &str,
    market_id: &str,
    token_id: &str,
    side: &QuoteSide,
    desired_price: f64,
    desired_size: f64,
    bbo: &TopOfBook,
    plan: &QuotePlan,
    market_ctx: &MakerMarketContext,
    roll_ctx: Option<&RollMicrostructureState>,
) {
    let context = QuoteTelemetryContext {
        order_id: format!(
            "guard-{}-{}-{}",
            market_id.chars().take(8).collect::<String>(),
            side.label().to_ascii_lowercase(),
            Utc::now().timestamp_millis()
        ),
        market_id: market_id.to_string(),
        token_id: token_id.to_string(),
        is_yes_token: bbo.is_yes_token,
        side: side.clone(),
        quote_price: desired_price,
        quote_size: desired_size,
        fair_value: plan.fair_value,
        p_yes: plan.p_yes,
        inventory_notional_usd: plan.inventory_notional_usd,
        target_spread_bps: plan.target_spread_bps,
        basis_widen_applied: plan.basis_widen_applied,
        basis_widen_bps: plan.basis_widen_bps,
        placed_bid: bbo.best_bid,
        placed_bid_size: bbo.best_bid_size,
        placed_ask: bbo.best_ask,
        placed_ask_size: bbo.best_ask_size,
        placed_mid: mid_from_bbo(bbo),
        placed_spread_bps: bbo.spread_bps,
        basis_pct: market_ctx.basis_pct,
        mark_price: market_ctx.mark_price,
        index_price: market_ctx.index_price,
        btc_spot_price: market_ctx.btc_spot_price,
        price_to_beat_distance_pct: market_ctx.price_to_beat_distance_pct,
        roll_signal: roll_ctx.map(|r| r.signal),
        roll_confidence: roll_ctx.map(|r| r.confidence),
        roll_persistence: roll_ctx.map(|r| r.persistence),
        roll_recent_delta_bps: roll_ctx.map(|r| r.recent_delta_bps),
        roll_sample_count: roll_ctx.map(|r| r.sample_count),
        placed_at: Utc::now().to_rfc3339(),
    };
    emit_quote_telemetry(
        bot,
        maker_state,
        outbox_tx,
        event,
        Some(reason),
        &context,
        Some(bbo),
        market_ctx,
        Some(0),
    );
}

fn build_quote_plan(
    cfg: &Config,
    bbo: &TopOfBook,
    rules: &VenueRules,
    p_yes: f64,
    inventory_notional: f64,
    market_ctx: &MakerMarketContext,
) -> Option<QuotePlan> {
    let observed_mid = mid_from_bbo(bbo)?;
    let tick = rules.tick_size.max(0.001);
    let token_sign = match bbo.is_yes_token {
        Some(true) => 1.0,
        Some(false) => -1.0,
        None => 0.0,
    };

    let fair_token_raw = match bbo.is_yes_token {
        Some(true) => p_yes,
        Some(false) => 1.0 - p_yes,
        None => observed_mid,
    };
    let skew_bps = (inventory_notional * cfg.maker_skew_per_usd_bps)
        .clamp(-cfg.maker_max_skew_bps, cfg.maker_max_skew_bps);
    let fair_token = clamp_price(fair_token_raw - token_sign * (skew_bps / 10_000.0));

    let observed_spread = bbo.spread_bps.unwrap_or(cfg.maker_spread_floor_bps);
    let mut target_spread_bps =
        observed_spread.clamp(cfg.maker_spread_floor_bps, cfg.maker_spread_ceiling_bps);
    let basis_widen_applied = cfg.maker_backwardation_widen_bps > 0.0
        && market_ctx
            .basis_pct
            .is_some_and(|basis_pct| basis_pct <= cfg.maker_backwardation_guard_pct);
    if basis_widen_applied {
        target_spread_bps = (target_spread_bps + cfg.maker_backwardation_widen_bps)
            .clamp(cfg.maker_spread_floor_bps, cfg.maker_spread_ceiling_bps);
        if cfg.maker_backwardation_min_target_spread_bps > 0.0 {
            target_spread_bps = target_spread_bps.max(
                cfg.maker_backwardation_min_target_spread_bps
                    .clamp(cfg.maker_spread_floor_bps, cfg.maker_spread_ceiling_bps),
            );
        }
    }
    let half_spread = target_spread_bps / 20_000.0;

    let mut bid = fair_token - half_spread;
    let mut ask = fair_token + half_spread;

    if let Some(best_ask) = bbo.best_ask {
        bid = bid.min(best_ask - tick);
    }
    if let Some(best_bid) = bbo.best_bid {
        ask = ask.max(best_bid + tick);
    }

    bid = round_to_tick(clamp_price(bid), rules);
    ask = round_to_tick(clamp_price(ask), rules);
    if ask <= bid {
        let anchor = round_to_tick(observed_mid, rules);
        bid = round_to_tick(clamp_price(anchor - tick), rules);
        ask = round_to_tick(clamp_price(anchor + tick), rules);
        if ask <= bid {
            return None;
        }
    }

    let notional = cfg.maker_quote_notional_usd.max(0.05);
    let size_ref = observed_mid.max(MAKER_MIN_PRICE);
    let size = (notional / size_ref).max(0.000_001);

    Some(QuotePlan {
        fair_value: fair_token,
        p_yes,
        skew_bps,
        target_spread_bps,
        basis_widen_applied,
        basis_widen_bps: if basis_widen_applied {
            cfg.maker_backwardation_widen_bps
        } else {
            0.0
        },
        bid,
        ask,
        size,
        observed_bid: bbo.best_bid,
        observed_ask: bbo.best_ask,
        inventory_notional_usd: inventory_notional,
    })
}

fn cancellation_confirmed(order_id: &str, cancelled: &[String]) -> bool {
    cancelled.iter().any(|id| id == order_id)
}

fn maker_order_terminal(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "CANCELED" | "CANCELLED" | "EXPIRED" | "MATCHED" | "FILLED" | "COMPLETED"
    )
}

async fn restore_maker_controls(
    bot: &TradingBot,
    state: &mut MakerActorState,
) -> anyhow::Result<()> {
    state
        .hands_off_markets
        .extend(db::maker::stood_down_markets(&bot.db).await?);
    Ok(())
}

/// Keep the evidence for this specific maker leg, without unrelated orders
/// or page ordering affecting its immutable terminal baseline.
fn terminal_maker_history(
    order: &db::maker::MakerOrder,
    history: &[crate::clob::VenueTrade],
) -> anyhow::Result<String> {
    let identity: crate::clob::settlement::SettlementOrderIdentity =
        serde_json::from_str(&order.settlement_json)?;
    let mut evidence = Vec::new();
    for matched in history {
        for leg in matched
            .maker_orders
            .iter()
            .filter(|leg| leg.order_id == order.order_id)
        {
            ensure!(
                leg.maker_address
                    .eq_ignore_ascii_case(&identity.maker_address)
                    && leg.asset_id == order.token_id
                    && leg.side == order.side,
                "terminal maker history changed its account/token/side identity"
            );
            ensure!(
                matched.is_confirmed() || matched.is_failed(),
                "terminal maker order has a newly unsettled match"
            );
            evidence.push(json!({"id":matched.id,"status":if matched.is_confirmed(){"CONFIRMED"}else{"FAILED"},
                "transaction":matched.transaction_hash.as_deref().map(str::to_ascii_lowercase),
                "match_time":matched.match_time,"size":leg.matched_amount,"price":leg.price,
                "fee_rate_bps":leg.fee_rate_bps}));
        }
    }
    evidence.sort_by_key(Value::to_string);
    Ok(serde_json::to_string(&evidence)?)
}

async fn audit_one_terminal_maker_order(
    bot: &TradingBot,
    order: &db::maker::MakerOrder,
    history: &[crate::clob::VenueTrade],
) -> anyhow::Result<()> {
    let clob = bot
        .clob
        .as_ref()
        .ok_or_else(|| anyhow!("terminal maker evidence requires exchange credentials"))?;
    let identity: crate::clob::settlement::SettlementOrderIdentity =
        serde_json::from_str(&order.settlement_json)?;
    ensure!(
        identity
            .maker_address
            .eq_ignore_ascii_case(&clob.account_address()),
        "terminal maker intent belongs to another funding account"
    );
    let current = terminal_maker_history(order, history)?;
    let prior = db::maker::terminal_history(&bot.db, &order.order_id).await?;
    ensure!(
        prior.is_empty() || prior == current,
        "terminal maker authenticated history changed after completion"
    );
    let recorded = db::maker::recorded_matches(&bot.db, &order.order_id).await?;
    if order.state == "REJECTED" {
        ensure!(
            current == "[]" && recorded.is_empty() && order.confirmed_size == 0.0,
            "rejected maker order subsequently acquired execution evidence"
        );
    } else {
        let observed = clob.get_order(&order.order_id).await?;
        validate_maker_order_observation(order, &observed)?;
        ensure!(
            maker_order_terminal(&observed.status)
                && (observed.size_matched - order.confirmed_size).abs() <= 1e-6,
            "terminal maker order reports new activity or matched quantity"
        );
        ensure!(
            observed
                .associate_trades
                .iter()
                .all(|id| history.iter().any(|matched| &matched.id == id
                    && (matched.is_confirmed() || matched.is_failed())
                    && matched
                        .maker_orders
                        .iter()
                        .any(|leg| leg.order_id == order.order_id))),
            "terminal maker order has incomplete associated trade history"
        );
        let verifier = clob.settlement_verifier()?;
        let mut transactions = HashSet::new();
        let mut seen = HashSet::new();
        for matched in history.iter().filter(|matched| {
            matched.is_confirmed()
                && matched
                    .maker_orders
                    .iter()
                    .any(|leg| leg.order_id == order.order_id)
        }) {
            let transaction = matched
                .transaction_hash
                .as_deref()
                .filter(|hash| !hash.is_empty())
                .ok_or_else(|| {
                    anyhow!("terminal confirmed maker trade is missing its receipt identity")
                })?;
            if !transactions.insert(transaction.to_ascii_lowercase()) {
                continue;
            }
            let timestamp = confirmed_maker_transaction_time(
                history,
                &order.order_id,
                transaction,
                Utc::now(),
            )?;
            for event in verifier.verify(transaction, &identity).await? {
                let key = format!("{}:{}", event.transaction_hash, event.log_index);
                let original = recorded
                    .iter()
                    .find(|fill| fill.match_id == key)
                    .ok_or_else(|| {
                        anyhow!("terminal maker order acquired an unrecorded finalized receipt")
                    })?;
                ensure!(
                    event.fee_shares == 0.0
                        && chrono::DateTime::parse_from_rfc3339(&original.timestamp)
                            .ok()
                            .is_some_and(|time| time.with_timezone(&Utc).to_rfc3339() == timestamp)
                        && original.token_id == order.token_id
                        && original.side == order.side
                        && (original.size - event.gross_shares).abs() <= 1e-9
                        && (original.price - event.gross_collateral / event.gross_shares).abs()
                            <= 1e-9
                        && (original.fee_usd - event.fee_collateral).abs() <= 1e-9,
                    "terminal maker finalized receipt differs from its recorded inventory/cost/fee"
                );
                seen.insert(key);
            }
        }
        ensure!(
            seen.len() == recorded.len(),
            "terminal maker recorded receipts disappeared from authenticated history"
        );
    }
    if prior.is_empty() {
        db::maker::record_verified_terminal_baseline(&bot.db, &order.order_id, &current).await?;
    }
    Ok(())
}

/// Audit a bounded rotating batch even when every quote is terminal. This
/// detects late evidence; it never invents a fill or silently repairs a lot.
async fn audit_terminal_maker_orders(
    bot: &TradingBot,
    state: &mut MakerActorState,
) -> anyhow::Result<()> {
    let mut orders =
        db::maker::terminal_orders_batch(&bot.db, state.terminal_audit_cursor.as_deref(), 8)
            .await?;
    if orders.is_empty() && state.terminal_audit_cursor.is_some() {
        state.terminal_audit_cursor = None;
        orders = db::maker::terminal_orders_batch(&bot.db, None, 8).await?;
    }
    if orders.is_empty() {
        return Ok(());
    }
    let clob = bot
        .clob
        .as_ref()
        .ok_or_else(|| anyhow!("terminal maker audit requires exchange credentials"))?;
    let history = clob.list_trades().await?;
    for order in orders {
        if let Err(error) = audit_one_terminal_maker_order(bot, &order, &history).await {
            db::maker::mark_audit_uncertain(&bot.db, &order.order_id).await?;
            return Err(anyhow!(
                "maker order {} needs operator reconciliation: {error}",
                order.order_id
            ));
        }
        state.terminal_audit_cursor = Some(order.order_id);
    }
    Ok(())
}

/// Resolve only a temporary observation failure. New/changed history or
/// receipts cannot be waived, and this never changes inventory or risk stops.
async fn recheck_terminal_maker_order(bot: &TradingBot, order_id: &str) -> anyhow::Result<()> {
    ensure!(
        bot.kill_switch_active(),
        "freeze entries before an operator recheck"
    );
    let mut order: db::maker::MakerOrder =
        sqlx::query_as("SELECT * FROM maker_orders WHERE order_id=?")
            .bind(order_id)
            .fetch_one(&bot.db)
            .await?;
    ensure!(
        order.state == "AUDIT_UNCERTAIN",
        "order is not audit-uncertain"
    );
    let baseline = db::maker::terminal_history(&bot.db, order_id).await?;
    ensure!(
        !baseline.is_empty(),
        "no verified terminal baseline; manual reconciliation is required"
    );
    let clob = bot
        .clob
        .as_ref()
        .ok_or_else(|| anyhow!("exchange credentials are required"))?;
    let history = clob.list_trades().await?;
    // Treat the candidate as terminal for verification, never for accounting.
    // Rejected submissions without a terminal venue order remain unresolved.
    order.state = "TERMINAL".into();
    audit_one_terminal_maker_order(bot, &order, &history).await?;
    db::maker::restore_verified_terminal_order(&bot.db, order_id, &baseline).await
}

fn validate_maker_order_observation(
    order: &db::maker::MakerOrder,
    observed: &crate::clob::OrderInfo,
) -> anyhow::Result<()> {
    ensure!(
        observed.id.as_deref() == Some(order.order_id.as_str()),
        "maker order status has a different or absent order ID"
    );
    ensure!(
        observed.asset_id.as_deref() == Some(order.token_id.as_str()),
        "maker order status has a different or absent token ID"
    );
    ensure!(
        observed.side.as_deref() == Some(order.side.as_str()),
        "maker order status has a different or absent side"
    );
    ensure!(
        observed
            .original_size
            .is_some_and(|size| (size - order.size).abs() <= 1e-6),
        "maker order status has a different or absent original size"
    );
    ensure!(
        (observed.price - order.price).abs() <= 1e-9,
        "maker order limit changed"
    );
    ensure!(
        observed.size_matched <= order.size + 1e-6,
        "maker order reports excess matched shares"
    );
    ensure!(
        observed.size_matched + 1e-6 >= order.confirmed_size,
        "maker order matched quantity went below confirmed inventory"
    );
    Ok(())
}

/// A receipt can cover several authenticated matches. Require one credible
/// UTC day across that transaction and pick its earliest time deterministically.
fn confirmed_maker_transaction_time(
    history: &[crate::clob::VenueTrade],
    order_id: &str,
    transaction: &str,
    now: chrono::DateTime<Utc>,
) -> anyhow::Result<String> {
    let mut times = Vec::new();
    for matched in history.iter().filter(|matched| {
        matched.is_confirmed()
            && matched
                .transaction_hash
                .as_deref()
                .is_some_and(|tx| tx.eq_ignore_ascii_case(transaction))
            && matched
                .maker_orders
                .iter()
                .any(|leg| leg.order_id == order_id)
    }) {
        let value = matched
            .match_time
            .as_deref()
            .ok_or_else(|| anyhow!("confirmed maker match lacks its execution time"))?;
        let time = value
            .parse::<i64>()
            .ok()
            .and_then(|seconds| chrono::DateTime::<Utc>::from_timestamp(seconds, 0))
            .or_else(|| {
                chrono::DateTime::parse_from_rfc3339(value)
                    .ok()
                    .map(|time| time.with_timezone(&Utc))
            })
            .ok_or_else(|| anyhow!("confirmed maker match has an invalid execution time"))?;
        ensure!(
            time.timestamp() >= 1_577_836_800 && time <= now + chrono::Duration::minutes(5),
            "confirmed maker execution time is outside a plausible UTC range"
        );
        if let Some(first) = times.first() {
            ensure!(
                chrono::DateTime::<Utc>::date_naive(first) == time.date_naive(),
                "confirmed maker transaction has contradictory UTC-day evidence"
            );
        }
        times.push(time);
    }
    times
        .into_iter()
        .min()
        .map(|time| time.to_rfc3339())
        .ok_or_else(|| anyhow!("confirmed maker transaction has no dated match evidence"))
}

async fn reconcile_maker_orders(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
) -> anyhow::Result<()> {
    reconcile_maker_orders_with_read_budget(bot, maker_state, Duration::from_secs(20)).await
}

async fn reconcile_maker_orders_with_read_budget(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    budget: Duration,
) -> anyhow::Result<()> {
    reconcile_maker_orders_before(bot, maker_state, tokio::time::Instant::now() + budget).await
}

async fn reconcile_maker_orders_before(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    let result = reconcile_maker_orders_with_read_budget_inner(bot, maker_state, deadline).await;
    if let Err(error) = &result {
        maker_state.live_accounting_ready = false;
        bot.set_degraded_no_trade(true);
        bot.halt_trading("maker_fill_reconciliation", error.to_string())
            .await;
    }
    result
}

async fn reconcile_maker_orders_with_read_budget_inner(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    deadline: tokio::time::Instant,
) -> anyhow::Result<()> {
    maker_state.live_accounting_ready = false;
    let orders = db::maker::pending_orders(&bot.db).await?;
    ensure!(
        !orders.iter().any(|order| order.state == "AUDIT_UNCERTAIN"),
        "late or changed terminal maker evidence requires operator reconciliation"
    );
    if orders.is_empty() {
        maker_state.last_live_reconcile = Some(Instant::now());
        maker_state.live_accounting_ready = true;
        return Ok(());
    }
    let clob = bot.clob.as_ref().ok_or_else(|| {
        anyhow!("durable live maker intents require exchange credentials for reconciliation")
    })?;
    let matches = super::live_orders::before_recovery_read_deadline(
        deadline,
        "maker history",
        clob.list_trades(),
    )
    .await?;
    let verifier = clob.settlement_verifier()?;
    let account = clob.account_address();
    let mut ready = true;
    let mut inventory_changed = false;
    for order in orders {
        let identity: crate::clob::settlement::SettlementOrderIdentity =
            serde_json::from_str(&order.settlement_json)?;
        ensure!(
            identity.maker_address.eq_ignore_ascii_case(&account),
            "durable maker intent belongs to another funding wallet"
        );
        let mut seen_transactions = HashSet::new();
        for matched in &matches {
            if !matched.is_confirmed() {
                continue;
            }
            for leg in matched
                .maker_orders
                .iter()
                .filter(|leg| leg.order_id == order.order_id)
            {
                ensure!(
                    leg.maker_address.eq_ignore_ascii_case(&account),
                    "maker match belongs to another account"
                );
                ensure!(
                    leg.asset_id == order.token_id && leg.side == order.side,
                    "maker match token or side differs from its durable intent"
                );
                let transaction = matched
                    .transaction_hash
                    .as_deref()
                    .filter(|hash| !hash.is_empty())
                    .ok_or_else(|| {
                        anyhow!("confirmed maker match is missing settlement transaction identity")
                    })?;
                if !seen_transactions.insert(transaction.to_ascii_lowercase()) {
                    continue;
                }
                let settled = super::live_orders::before_recovery_read_deadline(
                    deadline,
                    "maker settlement receipt",
                    verifier.verify(transaction, &identity),
                )
                .await?;
                for event in settled {
                    ensure!(
                        event.fee_shares == 0.0,
                        "maker received-asset fee is unsupported; cannot invent net inventory"
                    );
                    let timestamp = confirmed_maker_transaction_time(
                        &matches,
                        &order.order_id,
                        transaction,
                        Utc::now(),
                    )?;
                    let fill = db::maker::MakerMatch {
                        match_id: format!("{}:{}", event.transaction_hash, event.log_index),
                        order_id: event.order_id,
                        token_id: identity.asset_id.clone(),
                        side: order.side.clone(),
                        size: event.gross_shares,
                        price: event.gross_collateral / event.gross_shares,
                        fee_usd: event.fee_collateral,
                        timestamp,
                    };
                    let changes = db::maker::apply_confirmed_match(&bot.db, &fill).await?;
                    if !changes.is_empty() {
                        inventory_changed = true;
                        let mut trades = bot.portfolio_state.trades.write().await;
                        for trade in changes {
                            if let Some(current) = trades.iter_mut().find(|t| t.id == trade.id) {
                                if current.is_open() && !trade.is_open() {
                                    metrics::inc_trades_closed();
                                }
                                *current = trade;
                            } else {
                                metrics::inc_trades_opened();
                                trades.push(trade);
                            }
                        }
                    }
                }
            }
        }
        let observed = super::live_orders::before_recovery_read_deadline(
            deadline,
            "maker order status",
            clob.get_order(&order.order_id),
        )
        .await?;
        // Reload the confirmed total after atomically applying new matches.
        let current: db::maker::MakerOrder =
            sqlx::query_as("SELECT * FROM maker_orders WHERE order_id=?")
                .bind(&order.order_id)
                .fetch_one(&bot.db)
                .await?;
        validate_maker_order_observation(&current, &observed)?;
        let all_settled = (observed.size_matched - current.confirmed_size).abs() <= 1e-6
            && observed.associate_trades.iter().all(|id| {
                matches.iter().any(|matched| {
                    &matched.id == id
                        && (matched.is_confirmed() || matched.is_failed())
                        && matched.maker_orders.iter().any(|leg| {
                            leg.order_id == order.order_id
                                && leg.maker_address.eq_ignore_ascii_case(&account)
                        })
                })
            });
        if maker_order_terminal(&observed.status) && all_settled {
            db::maker::finalize_order(
                &bot.db,
                &order.order_id,
                &terminal_maker_history(&order, &matches)?,
            )
            .await?;
            maker_state.remove_quote(&order.order_id);
        } else {
            if !all_settled || order.state == "UNKNOWN" || order.state == "SUBMITTING" {
                ready = false;
            }
            let state =
                if order.state == "CANCEL_REQUESTED" || maker_order_terminal(&observed.status) {
                    "CANCEL_REQUESTED"
                } else {
                    "LIVE"
                };
            db::maker::update_state(&bot.db, &order.order_id, state).await?;
            maker_state.upsert_quote(
                order.order_id.clone(),
                order.market_id.clone(),
                order.token_id.clone(),
                if order.side == "BUY" {
                    QuoteSide::Bid
                } else {
                    QuoteSide::Ask
                },
                order.price,
                (order.size - current.confirmed_size).max(0.0),
            );
        }
        refresh_market_inventory_from_portfolio(bot, maker_state, &order.market_id).await;
    }
    if inventory_changed {
        let balance = bot.sync_balance_before(deadline).await?;
        ensure!(
            balance.is_some(),
            "settled maker inventory was recorded but collateral balance refresh failed"
        );
    }
    maker_state.last_live_reconcile = Some(Instant::now());
    maker_state.live_accounting_ready = ready;
    Ok(())
}

async fn maintain_live_maker_state(bot: &TradingBot, state: &mut MakerActorState) {
    taker_audit::maintain(bot, &mut state.taker_audit).await;
    if state
        .last_terminal_audit
        .is_none_or(|last| last.elapsed() >= Duration::from_secs(30))
    {
        state.last_terminal_audit = Some(Instant::now());
        if let Err(error) = tokio::time::timeout(
            Duration::from_secs(20),
            audit_terminal_maker_orders(bot, state),
        )
        .await
        .map_err(|_| anyhow!("terminal maker evidence audit timed out"))
        .and_then(|result| result)
        {
            state.live_accounting_ready = false;
            bot.set_degraded_no_trade(true);
            bot.halt_trading("maker_terminal_evidence_audit", error.to_string())
                .await;
        }
    }
    if state
        .last_live_reconcile
        .is_some_and(|last| last.elapsed() < Duration::from_secs(2))
    {
        return;
    }
    bot.runtime_state
        .actor_mutating
        .store(true, std::sync::atomic::Ordering::Release);
    let result = reconcile_maker_orders(bot, state).await;
    bot.runtime_state
        .actor_mutating
        .store(false, std::sync::atomic::Ordering::Release);
    if let Err(error) = result {
        // Back off failed reads too; a bad endpoint must not run at feed speed.
        state.last_live_reconcile = Some(Instant::now());
        bot.set_degraded_no_trade(true);
        bot.halt_trading("maker_fill_reconciliation", error.to_string())
            .await;
    }
}

async fn cancel_quote(
    bot: &TradingBot,
    order_id: &str,
    market_id: &str,
    token_id: &str,
    reason: &str,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> bool {
    cancel_quote_before(
        bot,
        order_id,
        market_id,
        token_id,
        reason,
        outbox_tx,
        tokio::time::Instant::now() + Duration::from_secs(20),
    )
    .await
}

// Identity, durable intent, and cancellation audit share the same exchange operation.
#[allow(clippy::too_many_arguments)]
async fn cancel_quote_before(
    bot: &TradingBot,
    order_id: &str,
    market_id: &str,
    token_id: &str,
    reason: &str,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
    deadline: tokio::time::Instant,
) -> bool {
    // Placement provenance survives config changes. Disabling live quoting
    // cannot turn a previously submitted order into a simulated cancellation.
    let requires_venue_ack = !order_id.starts_with("sim-mkr-");
    let mut ok = true;
    let mut detail = reason.to_string();
    if !requires_venue_ack {
        if let Err(error) = db::paper::cancel_order(&bot.db, order_id).await {
            warn!("paper maker cancellation persistence failed for {order_id}: {error}");
            bot.set_degraded_no_trade(true);
            bot.halt_trading("paper_maker_cancel_persistence", error.to_string())
                .await;
            return false;
        }
    }
    if requires_venue_ack {
        if let Err(error) = db::maker::update_state(&bot.db, order_id, "CANCEL_REQUESTED").await {
            warn!("cannot durably request maker cancellation for {order_id}: {error}");
            bot.set_degraded_no_trade(true);
            bot.halt_trading("maker_cancel_persistence", error.to_string())
                .await;
            outbox_emit(
                outbox_tx,
                &bot.db,
                OutboxEvent {
                    stage: "maker_quote".into(),
                    market_id: market_id.into(),
                    order_id: Some(order_id.into()),
                    token_id: Some(token_id.into()),
                    action: "cancel".into(),
                    status: "error".into(),
                    details: format!("{reason}: durable cancellation refused: {error}"),
                    priority: OutboxPriority::Critical,
                },
            );
            return false;
        }
        let Some(clob) = bot.clob.as_ref() else {
            outbox_emit(
                outbox_tx,
                &bot.db,
                OutboxEvent {
                    stage: "maker_quote".to_string(),
                    market_id: market_id.to_string(),
                    order_id: Some(order_id.to_string()),
                    token_id: Some(token_id.to_string()),
                    action: "cancel".to_string(),
                    status: "error".to_string(),
                    details: format!("{reason}: clob unavailable"),
                    priority: OutboxPriority::Critical,
                },
            );
            return false;
        };
        match super::live_orders::before_recovery_read_deadline(
            deadline,
            "maker cancellation",
            clob.cancel_order(order_id),
        )
        .await
        {
            Ok(cancelled) if cancellation_confirmed(order_id, &cancelled) => {
                // Cancellation acknowledges the remaining quote, not earlier
                // matches. Those must settle and be booked before replacement.
                let mut recovered = MakerActorState::default();
                if let Err(error) =
                    reconcile_maker_orders_before(bot, &mut recovered, deadline).await
                {
                    ok = false;
                    detail = format!("{reason}: cancellation fill reconciliation failed: {error}");
                } else {
                    match db::maker::pending_orders(&bot.db).await {
                        Ok(orders) if !orders.iter().any(|order| order.order_id == order_id) => {}
                        Ok(_) => {
                            ok = false;
                            detail = format!(
                                "{reason}: cancellation acknowledged; matches are still settling"
                            );
                        }
                        Err(error) => {
                            ok = false;
                            detail =
                                format!("{reason}: cancellation persistence check failed: {error}");
                        }
                    }
                }
            }
            Ok(_) => {
                ok = false;
                detail = format!("{reason}: venue did not confirm cancellation of {order_id}");
            }
            Err(e) => {
                ok = false;
                detail = format!("{reason}: {e}");
            }
        }
    }

    outbox_emit(
        outbox_tx,
        &bot.db,
        OutboxEvent {
            stage: "maker_quote".to_string(),
            market_id: market_id.to_string(),
            order_id: Some(order_id.to_string()),
            token_id: Some(token_id.to_string()),
            action: "cancel".to_string(),
            status: if ok { "ok" } else { "error" }.to_string(),
            details: detail,
            priority: OutboxPriority::Critical,
        },
    );
    ok
}

async fn maker_origin_allowed(
    bot: &TradingBot,
    origin: ExecutionOrigin,
    expected_mode: BotMode,
) -> bool {
    let cfg = bot.control_plane.config.read().await;
    bot.entry_origin_allowed_in_config(origin, &cfg)
        && match origin {
            ExecutionOrigin::Manual => true,
            ExecutionOrigin::Automatic { .. } => {
                cfg.mode_kind() == expected_mode
                    && matches!(
                        normalized_execution_mode(&cfg.execution_mode),
                        "maker" | "hybrid"
                    )
                    && (cfg.mode_kind() != BotMode::Live
                        || (cfg.maker_live_quoting && !cfg.shadow_trading))
            }
        }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn place_quote(
    bot: &TradingBot,
    market_id: &str,
    token_id: &str,
    side: &QuoteSide,
    price: f64,
    size: f64,
    cfg: &Config,
    is_yes_token: Option<bool>,
    synthetic_seq: u64,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> Option<String> {
    place_quote_for_origin(
        bot,
        market_id,
        token_id,
        side,
        price,
        size,
        cfg,
        is_yes_token,
        synthetic_seq,
        outbox_tx,
        ExecutionOrigin::Manual,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn place_quote_for_origin(
    bot: &TradingBot,
    market_id: &str,
    token_id: &str,
    side: &QuoteSide,
    price: f64,
    size: f64,
    cfg: &Config,
    is_yes_token: Option<bool>,
    synthetic_seq: u64,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
    origin: ExecutionOrigin,
) -> Option<String> {
    if !maker_origin_allowed(bot, origin, cfg.mode_kind()).await {
        return None;
    }
    if bot.entries_frozen() {
        return None;
    }
    match db::maker::market_stood_down(&bot.db, market_id).await {
        Ok(false) => {}
        Ok(true) => return None,
        Err(error) => {
            bot.set_degraded_no_trade(true);
            bot.halt_trading("maker_manual_stop_read", error.to_string())
                .await;
            return None;
        }
    }
    if cfg.mode_kind() == BotMode::Live {
        if !cfg.maker_live_quoting || cfg.shadow_trading {
            return None;
        }
        let result = async {
            ensure!(
                !db::maker::pending_orders(&bot.db)
                    .await?
                    .iter()
                    .any(|order| order.state != "LIVE"),
                "maker placement waits for unresolved submission/cancellation settlement"
            );
            ensure!(
                db::live::pending_orders(&bot.db).await?.is_empty(),
                "maker placement waits for unresolved taker settlement"
            );
            let clob = bot
                .clob
                .as_ref()
                .ok_or_else(|| anyhow!("live maker requires exchange credentials"))?;
            let direction = direct_fill_direction(is_yes_token)
                .ok_or_else(|| anyhow!("live maker requires a known token side"))?;
            // Fee-bearing maker metadata supplies no signed immutable fee
            // cap: modern exchange maxFeeRate=0 disables its ceiling. Keep
            // unknown fee policies refused instead of guessing a reserve.
            // https://github.com/Polymarket/ctf-exchange-v2/blob/main/src/exchange/mixins/Fees.sol
            ensure!(
                clob.maker_fee_is_zero(token_id).await?,
                "market does not guarantee zero maker fees"
            );
            clob.check_settlement_rpc().await?;
            let size = (size * 100.0 + 1e-9).floor() / 100.0;
            if *side == QuoteSide::Bid {
                if bot.sync_balance().await.is_none() {
                    bot.set_degraded_no_trade(true);
                    bot.halt_trading(
                        "maker_balance_sync_before_entry",
                        "live maker entry requires a fresh authenticated collateral balance",
                    )
                    .await;
                    return Err(anyhow!("live maker collateral balance refresh failed"));
                }
                evaluate_maker_entry(bot, cfg, market_id, direction, price, size * price, None)
                    .await
                    .map_err(|block| anyhow!(block.reason))?;
                let reserved = db::maker::reserved_buy_collateral(&bot.db).await?;
                ensure!(
                    bot.balance().await + 1e-9 >= reserved + size * price,
                    "insufficient unreserved collateral for maker quote"
                );
            } else {
                ensure!(
                    !bot.portfolio_state.trades.read().await.iter().any(|trade| {
                        trade.is_open()
                            && trade.is_live()
                            && trade.market_id == market_id
                            && trade.token_id.as_deref() == Some(token_id)
                            && trade.accounting_version != 1
                    }),
                    "maker sale requires verified entry accounting for existing inventory"
                );
                let inventory: f64 = bot
                    .portfolio_state
                    .trades
                    .read()
                    .await
                    .iter()
                    .filter(|t| {
                        t.is_open()
                            && t.is_live()
                            && t.market_id == market_id
                            && t.token_id.as_deref() == Some(token_id)
                    })
                    .map(|t| t.shares)
                    .sum();
                let reserved: f64 = db::maker::pending_orders(&bot.db)
                    .await?
                    .iter()
                    .filter(|order| order.token_id == token_id && order.side == "SELL")
                    .map(|order| (order.size - order.confirmed_size).max(0.0))
                    .sum();
                ensure!(
                    inventory + 1e-6 >= size + reserved,
                    "insufficient unreserved inventory for maker quote"
                );
            }
            let signed = clob
                .create_order(&LimitOrderArgs {
                    token_id: token_id.into(),
                    price,
                    size,
                    side: if *side == QuoteSide::Bid {
                        Side::Buy
                    } else {
                        Side::Sell
                    },
                })
                .await?;
            let order_id = signed.order_id().to_string();
            ensure!(
                !order_id.is_empty(),
                "signed maker order has no deterministic identity"
            );
            let analysis_value = bot.analysis_state.last_analysis.read().await.clone();
            let analysis = ParsedAnalysis::from_value(&analysis_value);
            let quote_expected_return = if *side == QuoteSide::Bid {
                super::execute::entry_return_at_price(
                    &analysis,
                    Signal::parse(direction.as_str()),
                    price,
                    0.0,
                )
                .ok_or_else(|| anyhow!("live buy quote lacks a valid outcome probability"))?
            } else {
                // Exit templates do not admit new exposure; their original
                // position's entry estimate remains in its trade row.
                analysis.net_ev
            };
            let template = Trade {
                timestamp: Utc::now().to_rfc3339(),
                market_id: market_id.into(),
                token_id: Some(token_id.into()),
                question: analysis.market.question.clone(),
                direction: direction.as_str().into(),
                entry_price: price,
                expected_value: quote_expected_return,
                reason: "confirmed venue maker match".into(),
                confidence: analysis.confidence,
                status: "OPEN".into(),
                mode: "live".into(),
                source: "bot".into(),
                end_date: analysis.market.end_date.clone(),
                raw_confidence: analysis.raw_confidence,
                partial_closed: Some(0),
                fok_failed: Some(0),
                strategy: "maker".into(),
                signals_json: analysis.signals.as_ref().map(Value::to_string),
                calibration_version: analysis_value
                    .get("calibration_version")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                run_id: Some(bot.run_id().into()),
                accounting_version: 1,
                ..Trade::default()
            };
            let now = Utc::now().to_rfc3339();
            let intent = db::maker::MakerOrder {
                order_id: order_id.clone(),
                market_id: market_id.into(),
                token_id: token_id.into(),
                side: if *side == QuoteSide::Bid {
                    "BUY"
                } else {
                    "SELL"
                }
                .into(),
                price,
                size,
                confirmed_size: 0.0,
                state: "SUBMITTING".into(),
                template_json: serde_json::to_string(&template)?,
                settlement_json: serde_json::to_string(
                    &crate::clob::settlement::SettlementOrderIdentity::from(&signed),
                )?,
                created_at: now.clone(),
                updated_at: now,
            };
            ensure!(
                !bot.entries_frozen() && maker_origin_allowed(bot, origin, cfg.mode_kind()).await,
                "automatic maker controls changed during validation"
            );
            if *side == QuoteSide::Bid {
                ensure!(!bot.is_drawdown_halted().await, "percentage drawdown halt");
            }
            if *side == QuoteSide::Bid {
                db::maker::admit_buy_order(
                    &bot.db,
                    &intent,
                    db::maker::MakerBuyBudget {
                        available_balance: bot.balance().await,
                        min_trade_usd: cfg.min_trade_usd,
                        max_positions: cfg.max_open_positions,
                        max_inventory_notional: cfg.maker_max_inventory_notional_usd,
                        max_exposure_pct: cfg.max_exposure_pct,
                    },
                )
                .await?;
            } else {
                db::maker::register_order(&bot.db, &intent).await?;
            }
            if bot.entries_frozen()
                || !maker_origin_allowed(bot, origin, cfg.mode_kind()).await
                || (*side == QuoteSide::Bid && bot.is_drawdown_halted().await)
            {
                db::maker::update_state(&bot.db, &order_id, "REJECTED").await?;
                return Err(anyhow!("entries froze before maker submission"));
            }
            match clob
                .post_order_with_options(&signed, OrderType::GTC, true)
                .await
            {
                Ok(receipt) if receipt.success == Some(false) => {
                    db::maker::update_state(&bot.db, &order_id, "REJECTED").await?;
                    Err(anyhow!("venue refused maker quote: {}", receipt.error_msg))
                }
                Ok(receipt) if receipt.order_id == order_id => {
                    if let Err(error) = db::maker::update_state(&bot.db, &order_id, "LIVE").await {
                        bot.set_degraded_no_trade(true);
                        bot.halt_trading("maker_acceptance_persistence", error.to_string())
                            .await;
                        return Err(error);
                    }
                    Ok(order_id)
                }
                outcome => {
                    bot.set_degraded_no_trade(true);
                    bot.halt_trading(
                        "maker_submission_unknown",
                        format!(
                            "maker order {order_id} acceptance needs reconciliation: {outcome:?}"
                        ),
                    )
                    .await;
                    db::maker::update_state(&bot.db, &order_id, "UNKNOWN").await?;
                    Err(anyhow!("maker order submission is unresolved"))
                }
            }
        }
        .await;
        let (order_id, status, details) = match &result {
            Ok(id) => (
                Some(id.clone()),
                "ok",
                format!("live post-only price={price:.4} size={size:.6}"),
            ),
            Err(error) => (None, "error", error.to_string()),
        };
        outbox_emit(
            outbox_tx,
            &bot.db,
            OutboxEvent {
                stage: "maker_quote".into(),
                market_id: market_id.into(),
                order_id,
                token_id: Some(token_id.into()),
                action: "place".into(),
                status: status.into(),
                details,
                priority: OutboxPriority::Critical,
            },
        );
        return result.ok();
    }
    if !bot.paper_execution_allowed().await {
        return None;
    }
    let order_id = format!(
        "sim-mkr-{}-{}-{}-{}-{}-{}",
        bot.run_id(),
        market_id.chars().take(8).collect::<String>(),
        side.label().to_ascii_lowercase(),
        synthetic_seq,
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
    );
    if *side == QuoteSide::Bid {
        if let Err(error) = admit_simulated_maker_order(
            bot,
            cfg,
            market_id,
            token_id,
            is_yes_token,
            price,
            size,
            &order_id,
        )
        .await
        {
            outbox_emit(
                outbox_tx,
                &bot.db,
                OutboxEvent {
                    stage: "maker_quote".into(),
                    market_id: market_id.into(),
                    order_id: Some(order_id),
                    token_id: Some(token_id.into()),
                    action: "place".into(),
                    status: "error".into(),
                    details: format!("paper quote admission refused: {error}"),
                    priority: OutboxPriority::Critical,
                },
            );
            return None;
        }
    }
    if !maker_origin_allowed(bot, origin, cfg.mode_kind()).await
        || (*side == QuoteSide::Bid && bot.is_drawdown_halted().await)
    {
        if let Err(error) = db::paper::cancel_order(&bot.db, &order_id).await {
            bot.set_degraded_no_trade(true);
            bot.halt_trading("paper_maker_cancel_persistence", error.to_string())
                .await;
        }
        return None;
    }
    outbox_emit(
        outbox_tx,
        &bot.db,
        OutboxEvent {
            stage: "maker_quote".into(),
            market_id: market_id.into(),
            order_id: Some(order_id.clone()),
            token_id: Some(token_id.into()),
            action: "place".into(),
            status: "ok".into(),
            details: format!("simulated price={price:.4} size={size:.6}"),
            priority: OutboxPriority::BestEffort,
        },
    );
    Some(order_id)
}

#[allow(clippy::too_many_arguments)]
async fn sync_quote_side(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    cfg: &Config,
    market_id: &str,
    token_id: &str,
    side: &QuoteSide,
    desired_price: f64,
    desired_size: f64,
    bbo: &TopOfBook,
    plan: &QuotePlan,
    market_ctx: &MakerMarketContext,
    decisions: &mut Vec<String>,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
    origin: ExecutionOrigin,
) -> MakerSideOutcome {
    let now = Instant::now();
    let existing = maker_state.quote_for(market_id, token_id, side).cloned();

    if let Some(remaining) = maker_state.fill_cooldown_remaining(market_id, token_id, side, now) {
        let reason = format!("fill_cooldown={}ms", remaining.as_millis());
        decisions.push(format!("{} hold {}", side.label(), reason));
        return MakerSideOutcome::new("hold", Some(reason));
    }

    if let Some(q) = existing {
        let drift_bps = (q.price - desired_price).abs() * 10_000.0;
        if drift_bps < cfg.maker_drift_threshold_bps {
            let reason = format!("drift={drift_bps:.1}bps");
            decisions.push(format!("{} hold {}", side.label(), reason));
            return MakerSideOutcome::new("hold", Some(reason));
        }

        match maker_state.check_replace_limits(market_id, token_id, side, &q.order_id, now, cfg) {
            ReplaceCheck::TooSoon => {
                decisions.push(format!("{} hold min_lifetime", side.label()));
                return MakerSideOutcome::new("hold", Some("min_lifetime".to_string()));
            }
            ReplaceCheck::Cooldown => {
                decisions.push(format!("{} hold cooldown", side.label()));
                return MakerSideOutcome::new("hold", Some("cooldown".to_string()));
            }
            ReplaceCheck::RateLimited => {
                decisions.push(format!("{} hold rate_limit", side.label()));
                return MakerSideOutcome::new("hold", Some("rate_limit".to_string()));
            }
            ReplaceCheck::ReplaceAllowed => {}
        }

        if !cancel_quote(bot, &q.order_id, market_id, token_id, "replace", outbox_tx).await {
            decisions.push(format!("{} cancel_failed", side.label()));
            return MakerSideOutcome::new("error", Some("cancel_failed".to_string()));
        }
        if let Some(context) = maker_state.quote_context(&q.order_id).cloned() {
            let current_bbo = maker_state.l2_bbo_by_token.get(token_id).cloned();
            let quote_age_ms = maker_state
                .quote_age(&q.order_id, now)
                .map(|age| age.as_millis().min(u128::from(u64::MAX)) as u64);
            emit_quote_telemetry(
                bot,
                maker_state,
                outbox_tx,
                "cancel",
                Some("replace"),
                &context,
                current_bbo.as_ref(),
                market_ctx,
                quote_age_ms,
            );
        }
        let _ = maker_state.remove_quote(&q.order_id);
        let synthetic_seq = maker_state.next_sequence();
        if let Some(new_id) = place_quote_for_origin(
            bot,
            market_id,
            token_id,
            side,
            desired_price,
            desired_size,
            cfg,
            bbo.is_yes_token,
            synthetic_seq,
            outbox_tx,
            origin,
        )
        .await
        {
            let context = build_quote_telemetry_context(
                new_id.clone(),
                market_id,
                token_id,
                side,
                desired_price,
                desired_size,
                bbo,
                plan,
                market_ctx,
                maker_state.roll_by_token.get(token_id),
            );
            let _ = maker_state.upsert_quote(
                new_id,
                market_id.to_string(),
                token_id.to_string(),
                side.clone(),
                desired_price,
                desired_size,
            );
            maker_state.set_quote_context(context.clone());
            emit_quote_telemetry(
                bot,
                maker_state,
                outbox_tx,
                "place",
                Some("replace"),
                &context,
                Some(bbo),
                market_ctx,
                Some(0),
            );
            maker_state.mark_replace(market_id, token_id, side, now);
            decisions.push(format!("{} replaced", side.label()));
            return MakerSideOutcome::new("replaced", Some("replace".to_string()));
        } else {
            decisions.push(format!("{} place_failed", side.label()));
            return MakerSideOutcome::new("error", Some("place_failed".to_string()));
        }
    }

    let synthetic_seq = maker_state.next_sequence();
    if let Some(order_id) = place_quote_for_origin(
        bot,
        market_id,
        token_id,
        side,
        desired_price,
        desired_size,
        cfg,
        bbo.is_yes_token,
        synthetic_seq,
        outbox_tx,
        origin,
    )
    .await
    {
        let context = build_quote_telemetry_context(
            order_id.clone(),
            market_id,
            token_id,
            side,
            desired_price,
            desired_size,
            bbo,
            plan,
            market_ctx,
            maker_state.roll_by_token.get(token_id),
        );
        let _ = maker_state.upsert_quote(
            order_id,
            market_id.to_string(),
            token_id.to_string(),
            side.clone(),
            desired_price,
            desired_size,
        );
        maker_state.set_quote_context(context.clone());
        emit_quote_telemetry(
            bot,
            maker_state,
            outbox_tx,
            "place",
            None,
            &context,
            Some(bbo),
            market_ctx,
            Some(0),
        );
        decisions.push(format!("{} placed", side.label()));
        MakerSideOutcome::new("placed", None)
    } else {
        decisions.push(format!("{} place_failed", side.label()));
        MakerSideOutcome::new("error", Some("place_failed".to_string()))
    }
}

// Suppression records the decision context and actor-owned quote cancellation.
#[allow(clippy::too_many_arguments)]
async fn suppress_quote_side(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    market_id: &str,
    token_id: &str,
    side: &QuoteSide,
    reason: &str,
    market_ctx: &MakerMarketContext,
    decisions: &mut Vec<String>,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> MakerSideOutcome {
    let now = Instant::now();
    let existing = maker_state.quote_for(market_id, token_id, side).cloned();
    if let Some(ref q) = existing {
        if !cancel_quote(bot, &q.order_id, market_id, token_id, reason, outbox_tx).await {
            decisions.push(format!("{} guard_cancel_failed", side.label()));
            return MakerSideOutcome::new("error", Some(format!("guard_cancel_failed({reason})")));
        }
        if let Some(context) = maker_state.quote_context(&q.order_id).cloned() {
            let current_bbo = maker_state.l2_bbo_by_token.get(token_id).cloned();
            let quote_age_ms = maker_state
                .quote_age(&q.order_id, now)
                .map(|age| age.as_millis().min(u128::from(u64::MAX)) as u64);
            emit_quote_telemetry(
                bot,
                maker_state,
                outbox_tx,
                "cancel",
                Some(reason),
                &context,
                current_bbo.as_ref(),
                market_ctx,
                quote_age_ms,
            );
        }
        let _ = maker_state.remove_quote(&q.order_id);
    }
    if maker_state.suppression_is_new(market_id, side.label(), reason) {
        outbox_emit(
            outbox_tx,
            &bot.db,
            OutboxEvent {
                stage: "maker_quote".to_string(),
                market_id: market_id.to_string(),
                order_id: existing.as_ref().map(|q| q.order_id.clone()),
                token_id: Some(token_id.to_string()),
                action: "suppress".to_string(),
                status: "observed".to_string(),
                details: format!("side={} reason={reason}", side.label()),
                priority: OutboxPriority::BestEffort,
            },
        );
    }
    decisions.push(format!("{} suppressed {}", side.label(), reason));
    MakerSideOutcome::new("suppressed", Some(reason.to_string()))
}

async fn cancel_all_quotes(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    reason: &str,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> usize {
    cancel_quotes_in(bot, maker_state, None, reason, outbox_tx).await
}

/// Cancel every resting quote, or only one market's when `market_id` is
/// given.
async fn cancel_quotes_in(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    market_id: Option<&str>,
    reason: &str,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> usize {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    cancel_quotes_in_with(
        bot,
        maker_state,
        market_id,
        reason,
        outbox_tx,
        |q| async move {
            cancel_quote_before(
                bot,
                &q.order_id,
                &q.market_id,
                &q.token_id,
                reason,
                outbox_tx,
                deadline,
            )
            .await
        },
    )
    .await
}

async fn cancel_quotes_in_with<F, Fut>(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    market_id: Option<&str>,
    reason: &str,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
    mut cancel: F,
) -> usize
where
    F: FnMut(QuoteState) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let quotes: Vec<QuoteState> = maker_state
        .quotes
        .values()
        .filter(|q| market_id.is_none_or(|m| q.market_id == m))
        .cloned()
        .collect();
    let mut removed = 0usize;
    for q in quotes {
        let market_ctx = current_maker_market_context(bot).await;
        if cancel(q.clone()).await {
            if let Some(context) = maker_state.quote_context(&q.order_id).cloned() {
                let current_bbo = maker_state.l2_bbo_by_token.get(&q.token_id).cloned();
                let quote_age_ms = maker_state
                    .quote_age(&q.order_id, Instant::now())
                    .map(|age| age.as_millis().min(u128::from(u64::MAX)) as u64);
                emit_quote_telemetry(
                    bot,
                    maker_state,
                    outbox_tx,
                    "cancel",
                    Some(reason),
                    &context,
                    current_bbo.as_ref(),
                    &market_ctx,
                    quote_age_ms,
                );
            }
            if maker_state.remove_quote(&q.order_id) {
                removed += 1;
            }
        }
    }
    removed
}

async fn maintain_quote_lifecycle(
    bot: &TradingBot,
    state: &mut MakerActorState,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) {
    if state
        .lifecycle_cancel_last_attempt
        .is_some_and(|last| last.elapsed() < Duration::from_secs(2))
    {
        return;
    }
    let cfg = bot.control_plane.config.read().await.clone();
    let mut reason = if state.l2_stale || bot.l2_feed_stale() {
        Some("l2_stale")
    } else if !bot.loop_running() || !cfg.auto_trading || bot.entries_frozen() {
        Some("maker disabled")
    } else if !matches!(
        normalized_execution_mode(&cfg.execution_mode),
        "maker" | "hybrid"
    ) {
        Some("execution mode changed")
    } else if cfg.mode_kind() == BotMode::Live && (!cfg.maker_live_quoting || cfg.shadow_trading) {
        Some("live quoting disabled")
    } else {
        None
    };
    let orders = match db::maker::pending_orders(&bot.db).await {
        Ok(orders) => orders,
        Err(error) => {
            bot.set_degraded_no_trade(true);
            bot.halt_trading("maker_quote_cleanup", error.to_string())
                .await;
            return;
        }
    };
    let paper: Vec<(String,String,String,f64,f64)> = match sqlx::query_as(
        "SELECT order_id,market_id,token_id,limit_price,MAX(size-filled_shares,0) FROM paper_maker_orders WHERE status='OPEN'"
    ).fetch_all(&bot.db).await {
        Ok(orders) => orders,
        Err(error) => {
            bot.set_degraded_no_trade(true);
            bot.halt_trading("maker_quote_cleanup", error.to_string()).await;
            return;
        }
    };
    if state.quotes.is_empty() && orders.is_empty() && paper.is_empty() {
        return;
    }
    let drawdown_only = reason.is_none() && bot.is_drawdown_halted().await;
    if drawdown_only {
        reason = Some("percentage drawdown halt");
    }
    let cancellation_pending: HashSet<String> = orders
        .iter()
        .filter(|order| order.state == "CANCEL_REQUESTED")
        .map(|order| order.order_id.clone())
        .collect();
    let retry_pending = !cancellation_pending.is_empty();
    if reason.is_none() && !retry_pending {
        return;
    }
    // Restore identities for cancellation even if a failed history read prevented hot restoration.
    for order in &orders {
        if reason.is_some() || order.state == "CANCEL_REQUESTED" {
            state.upsert_quote(
                order.order_id.clone(),
                order.market_id.clone(),
                order.token_id.clone(),
                if order.side == "BUY" {
                    QuoteSide::Bid
                } else {
                    QuoteSide::Ask
                },
                order.price,
                (order.size - order.confirmed_size).max(0.0),
            );
        }
    }
    if reason.is_some() {
        for (id, market, token, price, remaining) in paper {
            state.upsert_quote(id, market, token, QuoteSide::Bid, price, remaining);
        }
    }
    if state.quotes.is_empty() {
        return;
    }
    // Cancellation can discover and book an earlier venue fill. Keep integrity
    // observers out of its durable/hot publication window, restoring any outer writer lease.
    struct MutationLease<'a> {
        flag: &'a std::sync::atomic::AtomicBool,
        previous: bool,
    }
    impl Drop for MutationLease<'_> {
        fn drop(&mut self) {
            self.flag
                .store(self.previous, std::sync::atomic::Ordering::Release);
        }
    }
    let flag = &bot.runtime_state.actor_mutating;
    let _lease = MutationLease {
        flag,
        previous: flag.swap(true, std::sync::atomic::Ordering::AcqRel),
    };
    state.lifecycle_cancel_last_attempt = Some(Instant::now());
    let reason = reason.unwrap_or("cancellation retry");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    cancel_quotes_in_with(bot, state, None, reason, outbox_tx, |quote| {
        let eligible = !drawdown_only
            || quote.side == QuoteSide::Bid
            || cancellation_pending.contains(&quote.order_id);
        async move {
            eligible
                && cancel_quote_before(
                    bot,
                    &quote.order_id,
                    &quote.market_id,
                    &quote.token_id,
                    reason,
                    outbox_tx,
                    deadline,
                )
                .await
        }
    })
    .await;
    let pending = state
        .quotes
        .values()
        .filter(|quote| {
            !drawdown_only
                || quote.side == QuoteSide::Bid
                || cancellation_pending.contains(&quote.order_id)
        })
        .count();
    if pending > 0 {
        bot.set_degraded_no_trade(true);
        bot.halt_trading(
            "maker_quote_cleanup",
            format!(
                "{reason}: {} quotes remain; cancellation will retry",
                pending
            ),
        )
        .await;
        bot.record_reconcile_error(
            "maker_quote_cleanup",
            format!("{reason}: cancellation unresolved; reservations retained"),
        )
        .await;
    }
}

/// Final shutdown cancellation runs in the single writer before the position snapshot.
async fn cancel_maker_orders_for_shutdown(
    bot: &TradingBot,
    state: &mut MakerActorState,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) -> anyhow::Result<()> {
    for order in db::maker::pending_orders(&bot.db).await? {
        state.upsert_quote(
            order.order_id.clone(),
            order.market_id.clone(),
            order.token_id.clone(),
            if order.side == "BUY" {
                QuoteSide::Bid
            } else {
                QuoteSide::Ask
            },
            order.price,
            (order.size - order.confirmed_size).max(0.0),
        );
    }
    cancel_all_quotes(bot, state, "shutdown", outbox_tx).await;
    // Process any previously submitted taker history without creating a new identity or order.
    ensure!(
        bot.recover_live_orders_or_halt().await,
        "shutdown taker recovery remains unresolved"
    );
    ensure!(
        state.quotes.is_empty() && db::maker::pending_orders(&bot.db).await?.is_empty(),
        "shutdown maker cancellation remains unresolved"
    );
    // Unrestored paper reservations cannot execute without actor state; cancel them durably too.
    db::paper::expire_unrestored_quotes(&bot.db).await?;
    ensure!(
        db::live::pending_orders(&bot.db).await?.is_empty(),
        "shutdown taker orders remain unresolved"
    );
    Ok(())
}

/// Hands-off blocks entry immediately, but cancellation remains active until
/// every resting quote has an acknowledgement. Run this even without new books.
async fn retry_manual_quote_cleanup(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) {
    let markets: Vec<String> = maker_state
        .hands_off_markets
        .iter()
        .filter(|market| maker_state.by_market.contains_key(*market))
        .cloned()
        .collect();
    for market_id in markets {
        if !maker_state.manual_cancel_due(&market_id, Instant::now()) {
            continue;
        }
        cancel_quotes_in(
            bot,
            maker_state,
            Some(&market_id),
            "manual_close",
            outbox_tx,
        )
        .await;
        if maker_state.by_market.contains_key(&market_id) {
            bot.record_reconcile_error(
                "manual_quote_cleanup",
                format!(
                    "manual close on {market_id}: resting quotes remain; cancellation will retry"
                ),
            )
            .await;
        }
    }
}

async fn process_maker_bbo_event(
    bot: &TradingBot,
    maker_state: &mut MakerActorState,
    bbo: &TopOfBook,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) {
    let origin = bot.automatic_entry_origin();
    maintain_quote_lifecycle(bot, maker_state, outbox_tx).await;
    let cfg = bot.control_plane.config.read().await.clone();
    let exec_mode = normalized_execution_mode(&cfg.execution_mode);
    if !matches!(exec_mode, "maker" | "hybrid") {
        return;
    }

    if cfg.mode_kind() == BotMode::Live && maker_state.last_live_reconcile.is_none() {
        if let Err(error) = reconcile_maker_orders(bot, maker_state).await {
            bot.set_degraded_no_trade(true);
            bot.halt_trading("maker_fill_reconciliation", error.to_string())
                .await;
            return;
        }
    }

    if maker_state.hands_off_markets.contains(&bbo.market_id) {
        retry_manual_quote_cleanup(bot, maker_state, outbox_tx).await;
        let pending = maker_state.by_market.contains_key(&bbo.market_id);
        maker_state.update_model(MakerModelState {
            market_id: bbo.market_id.clone(),
            token_id: bbo.token_id.clone(),
            is_yes_token: bbo.is_yes_token,
            observed_bid: bbo.best_bid,
            observed_ask: bbo.best_ask,
            snapshot_loaded: bbo.snapshot_loaded,
            book_two_sided: book_two_sided(bbo),
            basis_pct: None,
            price_to_beat_distance_pct: None,
            bid_action: "suppressed".to_string(),
            bid_reason: Some("manual_close(hands off until the market rolls)".to_string()),
            ask_action: "suppressed".to_string(),
            ask_reason: Some("manual_close(hands off until the market rolls)".to_string()),
            decision: if pending {
                "manual_close_cancel_pending"
            } else {
                "manual_close"
            }
            .to_string(),
            updated_at: Utc::now().to_rfc3339(),
            ..MakerModelState::default()
        });
        return;
    }

    if !cfg.auto_trading || !bot.loop_running() || bot.entries_frozen() {
        if maker_state.by_market.contains_key(&bbo.market_id) {
            let removed = cancel_all_quotes(bot, maker_state, "maker disabled", outbox_tx).await;
            if !maker_state.quotes.is_empty() {
                warn!(
                    "maker disabled quote cleanup incomplete for market_id={} removed={} remaining={}",
                    bbo.market_id,
                    removed,
                    maker_state.quotes.len()
                );
                bot.set_degraded_no_trade(true);
                bot.set_kill_switch(true);
                bot.record_reconcile_error(
                    "maker_quote_cleanup",
                    format!(
                        "maker disabled cleanup incomplete market_id={} removed={} remaining={}",
                        bbo.market_id,
                        removed,
                        maker_state.quotes.len()
                    ),
                )
                .await;
            }
        }
        maker_state.update_model(MakerModelState {
            market_id: bbo.market_id.clone(),
            token_id: bbo.token_id.clone(),
            is_yes_token: bbo.is_yes_token,
            snapshot_loaded: bbo.snapshot_loaded,
            book_two_sided: book_two_sided(bbo),
            basis_pct: None,
            price_to_beat_distance_pct: None,
            bid_action: "suppressed".to_string(),
            bid_reason: Some("maker_disabled".to_string()),
            ask_action: "suppressed".to_string(),
            ask_reason: Some("maker_disabled".to_string()),
            decision: "maker_disabled".to_string(),
            updated_at: Utc::now().to_rfc3339(),
            ..MakerModelState::default()
        });
        return;
    }

    // In simulation mode, skip L2 stale gate — no real orders go on-book,
    // and the stale flag can stay stuck due to warmup/rearm timing.
    let is_simulation = cfg.mode.eq_ignore_ascii_case("simulation");
    let bot_feed_stale = bot.l2_feed_stale();
    if !is_simulation
        && (maker_state.l2_stale || bot_feed_stale || bbo.stale || !bbo.snapshot_loaded)
    {
        let l2_not_ready_reason = maker_l2_not_ready_reason(maker_state, bot_feed_stale, bbo);
        maker_state.update_model(MakerModelState {
            market_id: bbo.market_id.clone(),
            token_id: bbo.token_id.clone(),
            is_yes_token: bbo.is_yes_token,
            observed_bid: bbo.best_bid,
            observed_ask: bbo.best_ask,
            snapshot_loaded: bbo.snapshot_loaded,
            book_two_sided: book_two_sided(bbo),
            basis_pct: None,
            price_to_beat_distance_pct: None,
            bid_action: "suppressed".to_string(),
            bid_reason: Some(l2_not_ready_reason.clone()),
            ask_action: "suppressed".to_string(),
            ask_reason: Some(l2_not_ready_reason.clone()),
            decision: l2_not_ready_reason,
            updated_at: Utc::now().to_rfc3339(),
            ..MakerModelState::default()
        });
        return;
    }

    if cfg.mode_kind() == BotMode::Live
        && (!cfg.maker_live_quoting || cfg.shadow_trading || !maker_state.live_accounting_ready)
    {
        let removed =
            cancel_all_quotes(bot, maker_state, "live maker unsupported", outbox_tx).await;
        if !maker_state.quotes.is_empty() {
            bot.set_degraded_no_trade(true);
            bot.halt_trading(
                "live_maker_cleanup",
                format!("Unsupported live maker cleanup incomplete: removed={removed}"),
            )
            .await;
        }
        maker_state.update_model(MakerModelState {
            market_id: bbo.market_id.clone(),
            token_id: bbo.token_id.clone(),
            bid_action: "suppressed".into(),
            ask_action: "suppressed".into(),
            bid_reason: Some("live_maker_unsupported".into()),
            ask_reason: Some("live_maker_unsupported".into()),
            decision: "live_maker_unsupported".into(),
            updated_at: Utc::now().to_rfc3339(),
            ..MakerModelState::default()
        });
        return;
    }

    let market_ctx = current_maker_market_context(bot).await;
    let filled =
        simulate_passive_maker_fills(bot, maker_state, &cfg, bbo, &market_ctx, outbox_tx).await;

    let yes_mid_fallback = market_yes_mid(maker_state, &bbo.market_id).or_else(|| {
        mid_from_bbo(bbo).map(|m| {
            if bbo.is_yes_token == Some(false) {
                1.0 - m
            } else {
                m
            }
        })
    });
    let (p_yes, source) = fair_yes_probability(bot, &bbo.market_id, yes_mid_fallback).await;
    let inventory_notional = market_inventory_notional(bot, maker_state, &bbo.market_id).await;
    let gross_open_notional = market_gross_open_notional_usd(bot, &bbo.market_id).await;
    let token_inventory_shares =
        market_token_inventory_shares(bot, maker_state, &bbo.market_id, bbo.is_yes_token).await;

    let rules = maker_state.venue_rules(&bbo.market_id);
    let Some(plan) = build_quote_plan(&cfg, bbo, &rules, p_yes, inventory_notional, &market_ctx)
    else {
        maker_state.update_model(MakerModelState {
            market_id: bbo.market_id.clone(),
            token_id: bbo.token_id.clone(),
            is_yes_token: bbo.is_yes_token,
            p_yes,
            observed_bid: bbo.best_bid,
            observed_ask: bbo.best_ask,
            snapshot_loaded: bbo.snapshot_loaded,
            book_two_sided: book_two_sided(bbo),
            basis_pct: market_ctx.basis_pct,
            price_to_beat_distance_pct: market_ctx.price_to_beat_distance_pct,
            bid_action: "suppressed".to_string(),
            bid_reason: Some("no_quote_plan".to_string()),
            ask_action: "suppressed".to_string(),
            ask_reason: Some("no_quote_plan".to_string()),
            decision: "no_quote_plan".to_string(),
            updated_at: Utc::now().to_rfc3339(),
            ..MakerModelState::default()
        });
        return;
    };

    let mut decisions = vec![format!("source={source}")];
    let mut bid_outcome = MakerSideOutcome::default();
    let mut ask_outcome = MakerSideOutcome::default();
    if filled > 0 {
        decisions.push(format!("fills={filled}"));
    }
    let roll_ctx = maker_state.roll_by_token.get(&bbo.token_id).cloned();

    if let Some(guard) = maker_basis_guard(&cfg, &market_ctx) {
        emit_guard_telemetry(
            bot,
            maker_state,
            outbox_tx,
            if guard.shadow_only {
                "guard_shadow"
            } else {
                "guard_skip"
            },
            &guard.reason,
            &bbo.market_id,
            &bbo.token_id,
            &QuoteSide::Bid,
            plan.bid,
            plan.size,
            bbo,
            &plan,
            &market_ctx,
            roll_ctx.as_ref(),
        );
        emit_guard_telemetry(
            bot,
            maker_state,
            outbox_tx,
            if guard.shadow_only {
                "guard_shadow"
            } else {
                "guard_skip"
            },
            &guard.reason,
            &bbo.market_id,
            &bbo.token_id,
            &QuoteSide::Ask,
            plan.ask,
            plan.size,
            bbo,
            &plan,
            &market_ctx,
            roll_ctx.as_ref(),
        );

        if !guard.shadow_only {
            if maker_state.by_market.contains_key(&bbo.market_id) {
                let removed = cancel_all_quotes(bot, maker_state, "basis_guard", outbox_tx).await;
                if !maker_state.quotes.is_empty() {
                    warn!(
                        "basis guard quote cleanup incomplete for market_id={} removed={} remaining={}",
                        bbo.market_id,
                        removed,
                        maker_state.quotes.len()
                    );
                    bot.set_degraded_no_trade(true);
                    bot.set_kill_switch(true);
                    bot.record_reconcile_error(
                        "maker_quote_cleanup",
                        format!(
                            "basis guard cleanup incomplete market_id={} removed={} remaining={}",
                            bbo.market_id,
                            removed,
                            maker_state.quotes.len()
                        ),
                    )
                    .await;
                }
            }
            maker_state.update_model(MakerModelState {
                market_id: bbo.market_id.clone(),
                token_id: bbo.token_id.clone(),
                is_yes_token: bbo.is_yes_token,
                fair_value: plan.fair_value,
                p_yes: plan.p_yes,
                skew_bps: plan.skew_bps,
                target_spread_bps: plan.target_spread_bps,
                target_bid: plan.bid,
                target_ask: plan.ask,
                observed_bid: plan.observed_bid,
                observed_ask: plan.observed_ask,
                snapshot_loaded: bbo.snapshot_loaded,
                book_two_sided: book_two_sided(bbo),
                basis_pct: market_ctx.basis_pct,
                price_to_beat_distance_pct: market_ctx.price_to_beat_distance_pct,
                inventory_notional_usd: plan.inventory_notional_usd,
                bid_action: "suppressed".to_string(),
                bid_reason: Some(guard.reason.clone()),
                ask_action: "suppressed".to_string(),
                ask_reason: Some(guard.reason.clone()),
                decision: format!("source={source} | basis_guard_active | {}", guard.reason),
                updated_at: Utc::now().to_rfc3339(),
            });
            return;
        }
        decisions.push(format!("basis_guard_shadow {}", guard.reason));
    }
    if let Some(reason) = venue_min_size_guard(&cfg, &rules, &plan) {
        bid_outcome = suppress_quote_side(
            bot,
            maker_state,
            &bbo.market_id,
            &bbo.token_id,
            &QuoteSide::Bid,
            &reason,
            &market_ctx,
            &mut decisions,
            outbox_tx,
        )
        .await;
        ask_outcome = suppress_quote_side(
            bot,
            maker_state,
            &bbo.market_id,
            &bbo.token_id,
            &QuoteSide::Ask,
            &reason,
            &market_ctx,
            &mut decisions,
            outbox_tx,
        )
        .await;
        maker_state.update_model(MakerModelState {
            market_id: bbo.market_id.clone(),
            token_id: bbo.token_id.clone(),
            is_yes_token: bbo.is_yes_token,
            fair_value: plan.fair_value,
            p_yes: plan.p_yes,
            skew_bps: plan.skew_bps,
            target_spread_bps: plan.target_spread_bps,
            target_bid: plan.bid,
            target_ask: plan.ask,
            observed_bid: plan.observed_bid,
            observed_ask: plan.observed_ask,
            snapshot_loaded: bbo.snapshot_loaded,
            book_two_sided: book_two_sided(bbo),
            basis_pct: market_ctx.basis_pct,
            price_to_beat_distance_pct: market_ctx.price_to_beat_distance_pct,
            inventory_notional_usd: plan.inventory_notional_usd,
            bid_action: bid_outcome.action.clone(),
            bid_reason: bid_outcome.reason.clone(),
            ask_action: ask_outcome.action.clone(),
            ask_reason: ask_outcome.reason.clone(),
            decision: format!("source={source} | {reason}"),
            updated_at: Utc::now().to_rfc3339(),
        });
        return;
    }
    if let Some(reason) = maker_market_quality_guard(&cfg, bbo) {
        bid_outcome = suppress_quote_side(
            bot,
            maker_state,
            &bbo.market_id,
            &bbo.token_id,
            &QuoteSide::Bid,
            &reason,
            &market_ctx,
            &mut decisions,
            outbox_tx,
        )
        .await;
        ask_outcome = suppress_quote_side(
            bot,
            maker_state,
            &bbo.market_id,
            &bbo.token_id,
            &QuoteSide::Ask,
            &reason,
            &market_ctx,
            &mut decisions,
            outbox_tx,
        )
        .await;

        maker_state.update_model(MakerModelState {
            market_id: bbo.market_id.clone(),
            token_id: bbo.token_id.clone(),
            is_yes_token: bbo.is_yes_token,
            fair_value: plan.fair_value,
            p_yes: plan.p_yes,
            skew_bps: plan.skew_bps,
            target_spread_bps: plan.target_spread_bps,
            target_bid: plan.bid,
            target_ask: plan.ask,
            observed_bid: plan.observed_bid,
            observed_ask: plan.observed_ask,
            snapshot_loaded: bbo.snapshot_loaded,
            book_two_sided: book_two_sided(bbo),
            basis_pct: market_ctx.basis_pct,
            price_to_beat_distance_pct: market_ctx.price_to_beat_distance_pct,
            inventory_notional_usd: plan.inventory_notional_usd,
            bid_action: bid_outcome.action,
            bid_reason: bid_outcome.reason,
            ask_action: ask_outcome.action,
            ask_reason: ask_outcome.reason,
            decision: decisions.join(" | "),
            updated_at: Utc::now().to_rfc3339(),
        });
        return;
    }
    let quote_sides = [(QuoteSide::Bid, plan.bid), (QuoteSide::Ask, plan.ask)];
    for (side, desired_price) in quote_sides {
        let desired_notional = plan.size * desired_price;
        // A collector never posts the flatten side. Holding is the point; the
        // exits are take-profit, stop-loss, a strong reversal, or resolution.
        if side == QuoteSide::Ask && cfg.maker_hold_positions {
            ask_outcome = suppress_quote_side(
                bot,
                maker_state,
                &bbo.market_id,
                &bbo.token_id,
                &side,
                "hold_mode(no flatten ask)",
                &market_ctx,
                &mut decisions,
                outbox_tx,
            )
            .await;
            continue;
        }
        if side == QuoteSide::Ask && token_inventory_shares <= 0.0 {
            ask_outcome = suppress_quote_side(
                bot,
                maker_state,
                &bbo.market_id,
                &bbo.token_id,
                &side,
                "no_inventory",
                &market_ctx,
                &mut decisions,
                outbox_tx,
            )
            .await;
            continue;
        }
        if side == QuoteSide::Bid {
            let Some(direction) = direct_fill_direction(bbo.is_yes_token) else {
                bid_outcome = suppress_quote_side(
                    bot,
                    maker_state,
                    &bbo.market_id,
                    &bbo.token_id,
                    &side,
                    "unknown maker bid direction",
                    &market_ctx,
                    &mut decisions,
                    outbox_tx,
                )
                .await;
                continue;
            };
            let continuing_quote = maker_state
                .quote_for(&bbo.market_id, &bbo.token_id, &side)
                .filter(|quote| {
                    cfg.mode_kind() != BotMode::Live
                        && quote.order_id.starts_with("sim-mkr-")
                        && quote.status == "PARTIALLY_FILLED"
                        && (quote.price - desired_price).abs() * 10_000.0
                            < cfg.maker_drift_threshold_bps
                });
            if let Err(block) = evaluate_maker_entry(
                bot,
                &cfg,
                &bbo.market_id,
                direction,
                continuing_quote
                    .map(|quote| quote.price)
                    .unwrap_or(desired_price),
                continuing_quote
                    .map(|quote| quote.price * quote.size)
                    .unwrap_or(desired_notional),
                continuing_quote.map(|quote| quote.order_id.as_str()),
            )
            .await
            {
                let reason = format!("entry_ineligible({})", block.reason);
                bid_outcome = suppress_quote_side(
                    bot,
                    maker_state,
                    &bbo.market_id,
                    &bbo.token_id,
                    &side,
                    &reason,
                    &market_ctx,
                    &mut decisions,
                    outbox_tx,
                )
                .await;
                continue;
            }
        }
        if side == QuoteSide::Bid
            && cfg.maker_max_inventory_notional_usd > 0.0
            && gross_open_notional + desired_notional > cfg.maker_max_inventory_notional_usd
        {
            let reason = format!(
                "inventory_cap(gross_open={gross_open_notional:.2} + desired={desired_notional:.2} > cap={:.2})",
                cfg.maker_max_inventory_notional_usd
            );
            bid_outcome = suppress_quote_side(
                bot,
                maker_state,
                &bbo.market_id,
                &bbo.token_id,
                &side,
                &reason,
                &market_ctx,
                &mut decisions,
                outbox_tx,
            )
            .await;
            continue;
        }
        if let Some(guard) = maker_roll_guard(&cfg, bbo.is_yes_token, &side, roll_ctx.as_ref()) {
            emit_guard_telemetry(
                bot,
                maker_state,
                outbox_tx,
                if guard.shadow_only {
                    "guard_shadow"
                } else {
                    "guard_skip"
                },
                &guard.reason,
                &bbo.market_id,
                &bbo.token_id,
                &side,
                desired_price,
                plan.size,
                bbo,
                &plan,
                &market_ctx,
                roll_ctx.as_ref(),
            );
            if guard.shadow_only {
                decisions.push(format!("roll_guard_shadow {}", guard.reason));
            } else {
                let outcome = suppress_quote_side(
                    bot,
                    maker_state,
                    &bbo.market_id,
                    &bbo.token_id,
                    &side,
                    &guard.reason,
                    &market_ctx,
                    &mut decisions,
                    outbox_tx,
                )
                .await;
                match side {
                    QuoteSide::Bid => bid_outcome = outcome,
                    QuoteSide::Ask => ask_outcome = outcome,
                }
                continue;
            }
        }
        let outcome = sync_quote_side(
            bot,
            maker_state,
            &cfg,
            &bbo.market_id,
            &bbo.token_id,
            &side,
            desired_price,
            plan.size,
            bbo,
            &plan,
            &market_ctx,
            &mut decisions,
            outbox_tx,
            origin,
        )
        .await;
        match side {
            QuoteSide::Bid => bid_outcome = outcome,
            QuoteSide::Ask => ask_outcome = outcome,
        }
    }

    maker_state.update_model(MakerModelState {
        market_id: bbo.market_id.clone(),
        token_id: bbo.token_id.clone(),
        is_yes_token: bbo.is_yes_token,
        fair_value: plan.fair_value,
        p_yes: plan.p_yes,
        skew_bps: plan.skew_bps,
        target_spread_bps: plan.target_spread_bps,
        target_bid: plan.bid,
        target_ask: plan.ask,
        observed_bid: plan.observed_bid,
        observed_ask: plan.observed_ask,
        snapshot_loaded: bbo.snapshot_loaded,
        book_two_sided: book_two_sided(bbo),
        basis_pct: market_ctx.basis_pct,
        price_to_beat_distance_pct: market_ctx.price_to_beat_distance_pct,
        inventory_notional_usd: plan.inventory_notional_usd,
        bid_action: bid_outcome.action,
        bid_reason: bid_outcome.reason,
        ask_action: ask_outcome.action,
        ask_reason: ask_outcome.reason,
        decision: decisions.join(" | "),
        updated_at: Utc::now().to_rfc3339(),
    });
}

/// Release all maker reservations only after cancellation and finalized fills
/// agree. The actor keeps this handoff and the ensuing taker admission in one
/// writer turn; quotes cannot reopen between its proof and the entry.
async fn prepare_live_hybrid_handoff(
    bot: &TradingBot,
    state: &mut MakerActorState,
    outbox: &mpsc::Sender<OutboxEvent>,
) -> anyhow::Result<()> {
    ensure!(
        bot.mode_kind().await == BotMode::Live,
        "live hybrid handoff requires live mode"
    );
    ensure!(
        !bot.entries_frozen() && !bot.degraded_no_trade_active(),
        "hybrid entries are frozen or degraded"
    );
    cancel_quotes_in(bot, state, None, "hybrid taker handoff", outbox).await;
    reconcile_maker_orders(bot, state).await?;
    ensure!(
        state.quotes.is_empty() && db::maker::pending_orders(&bot.db).await?.is_empty(),
        "hybrid taker handoff waits for maker cancellation and finalized fills"
    );
    bot.require_reconciled_live_exits().await?;
    ensure!(
        bot.sync_balance().await.is_some(),
        "hybrid taker handoff requires a fresh authenticated collateral balance"
    );
    ensure!(
        !bot.entries_frozen() && !bot.degraded_no_trade_active(),
        "hybrid entries froze during reconciliation"
    );
    Ok(())
}

async fn process_execution_command(
    cmd: ExecutionCommand,
    bot: &Arc<TradingBot>,
    maker_state: &mut MakerActorState,
    outbox_tx: &mpsc::Sender<OutboxEvent>,
) {
    // Flag trade-mutating commands so the integrity checker can suppress
    // false-positive drift caused by non-atomic balance/trade updates.
    let is_trade_mutation = matches!(
        cmd,
        ExecutionCommand::CancelMakerOrdersForShutdown { .. }
            | ExecutionCommand::SetL2Stale { .. }
            | ExecutionCommand::FlushBestBidAsk
            | ExecutionCommand::ExecuteTrade { .. }
            | ExecutionCommand::CloseTrade { .. }
            | ExecutionCommand::PartialCloseTrade { .. }
            | ExecutionCommand::ReconcileRedemption { .. }
            | ExecutionCommand::ClosePair { .. }
            | ExecutionCommand::ReconcileMakerOrders { .. }
            | ExecutionCommand::ReconcileLiveState { .. }
            | ExecutionCommand::ResetPaperBalance { .. }
    );
    if is_trade_mutation {
        bot.runtime_state
            .actor_mutating
            .store(true, std::sync::atomic::Ordering::Release);
    }
    // Ensure the flag is always cleared, even on early returns / panics.
    struct MutationGuard<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for MutationGuard<'_> {
        fn drop(&mut self) {
            self.0.store(false, std::sync::atomic::Ordering::Release);
        }
    }
    let _guard = if is_trade_mutation {
        Some(MutationGuard(&bot.runtime_state.actor_mutating))
    } else {
        None
    };

    match cmd {
        ExecutionCommand::CancelMakerOrdersForShutdown { resp } => {
            let result = cancel_maker_orders_for_shutdown(bot, maker_state, outbox_tx)
                .await
                .map_err(|error| error.to_string());
            if let Err(error) = &result {
                bot.set_degraded_no_trade(true);
                bot.halt_trading("shutdown_order_cleanup", error.clone())
                    .await;
            }
            let _ = resp.send(result);
        }

        ExecutionCommand::ResetPaperBalance {
            amount,
            force,
            resp,
        } => {
            let result = bot
                .reset_paper_balance_direct(amount, force)
                .await
                .map_err(|error| error.to_string());
            let _ = resp.send(result);
        }
        ExecutionCommand::ResetDrawdownCashPeak { resp } => {
            let result = bot
                .reset_drawdown_cash_peak_direct()
                .await
                .map_err(|error| error.to_string());
            let _ = resp.send(result);
        }
        ExecutionCommand::ReconcileLiveState { full, resp } => {
            let result = async {
                reconcile_maker_orders(bot, maker_state).await?;
                let outcome = bot.reconcile_live_state_inner().await?;
                if full {
                    bot.full_resync_live_state_inner(outcome).await?;
                }
                Ok(outcome)
            }
            .await
            .map_err(|error: anyhow::Error| error.to_string());
            if let Err(error) = &result {
                bot.set_degraded_no_trade(true);
                bot.halt_trading("live_state_reconciliation", error.clone())
                    .await;
            }
            let _ = resp.send(result);
        }
        ExecutionCommand::PartialCloseTrade {
            trade_id,
            exit_price,
            fraction,
            intent,
            request_id: partial_request_id,
            resp,
        } => {
            if let Some(request_id) = partial_request_id {
                let intents = bot.trade_lifecycle.partial_close_intents.read().await;
                if intents
                    .get(&trade_id)
                    .is_none_or(|intent| intent.request_id != request_id)
                {
                    let _ = resp.send(Some(json!({"ok":false,"status":"pending",
                        "reason":"partial request no longer owns its intent; refresh the position evidence"})));
                    return;
                }
            }
            let trade = bot
                .portfolio_state
                .trades
                .read()
                .await
                .iter()
                .find(|trade| trade.id == trade_id && trade.has_unsettled_exposure())
                .cloned();
            if let Some(trade) = &trade {
                if bot.refuse_mixed_mode_paper_exit(trade).await {
                    let result = Some(json!({"ok":false,"id":trade_id,"status":"rejected",
                        "reason":"paper inventory cannot settle into live or wallet-backed cash; use an isolated paper process",
                        "remaining_shares":trade.shares,"partial":true,"pnl":null}));
                    bot.finish_partial_close_request(trade_id, partial_request_id, &result)
                        .await;
                    let _ = resp.send(result);
                    return;
                }
                if intent == CloseIntent::Manual {
                    maker_state.stand_down(&trade.market_id);
                    if let Err(error) =
                        db::maker::persist_manual_stand_down(&bot.db, &trade.market_id).await
                    {
                        bot.set_degraded_no_trade(true);
                        bot.halt_trading("maker_manual_stop_persistence", error.to_string())
                            .await;
                        let result = Some(json!({"ok":false,"status":"pending",
                            "reason":"manual stand-down could not be persisted",
                            "remaining_shares":trade.shares,"trade_id":trade_id}));
                        bot.finish_partial_close_request(trade_id, partial_request_id, &result)
                            .await;
                        let _ = resp.send(result);
                        return;
                    }
                    retry_manual_quote_cleanup(bot, maker_state, outbox_tx).await;
                }
                if bot.mode_kind().await == BotMode::Live {
                    let _ = cancel_quotes_in(
                        bot,
                        maker_state,
                        Some(&trade.market_id),
                        "live partial close",
                        outbox_tx,
                    )
                    .await;
                    if maker_state.by_market.contains_key(&trade.market_id)
                        || db::maker::pending_orders(&bot.db)
                            .await
                            .map_or(true, |orders| {
                                orders
                                    .iter()
                                    .any(|order| order.market_id == trade.market_id)
                            })
                    {
                        bot.set_degraded_no_trade(true);
                        bot.halt_trading(
                            "maker_cancel_before_close",
                            "live partial close waits for maker cancellation and settled fills",
                        )
                        .await;
                        let result = Some(json!({"ok":false,"status":"pending",
                            "reason":"maker cancellation and confirmed fills remain unresolved",
                            "remaining_shares":trade.shares,"trade_id":trade_id}));
                        bot.finish_partial_close_request(trade_id, partial_request_id, &result)
                            .await;
                        let _ = resp.send(result);
                        return;
                    }
                }
            }
            let result = bot
                .partial_close_trade_with_request(
                    trade_id,
                    exit_price,
                    fraction,
                    intent,
                    partial_request_id,
                )
                .await;
            bot.finish_partial_close_request(trade_id, partial_request_id, &result)
                .await;
            let _ = resp.send(result);
        }
        ExecutionCommand::ReconcileRedemption { request, resp } => {
            let result = bot.reconcile_redemption_direct(request).await;
            let _ = resp.send(result);
        }
        ExecutionCommand::RecheckTerminalTakerOrder { order_id, resp } => {
            let result = taker_audit::recheck(bot, &order_id)
                .await
                .map_err(|error| error.to_string());
            maker_state.live_accounting_ready = false;
            maker_state.taker_audit.request_next_check();
            let _ = resp.send(result);
        }
        ExecutionCommand::RecheckTerminalMakerOrder { order_id, resp } => {
            let result = recheck_terminal_maker_order(bot, &order_id)
                .await
                .map_err(|error| error.to_string());
            maker_state.live_accounting_ready = false;
            maker_state.last_terminal_audit = None;
            let _ = resp.send(result);
        }
        ExecutionCommand::ReconcileMakerOrders { resp } => {
            let result = reconcile_maker_orders(bot, maker_state)
                .await
                .map_err(|error| error.to_string());
            if let Err(error) = &result {
                bot.set_degraded_no_trade(true);
                bot.halt_trading("maker_fill_reconciliation", error.clone())
                    .await;
            }
            let _ = resp.send(result);
        }
        ExecutionCommand::ExecuteTrade {
            origin,
            analysis,
            hail_mary,
            override_amount,
            strategy,
            resp,
        } => {
            if !bot.entry_origin_allowed(origin).await {
                info!("Execution actor: automatic entry invalidated by Stop/Disable/shutdown");
                let _ = resp.send(None);
                return;
            }
            let (exec_mode, needs_live_handoff) = {
                let cfg = bot.control_plane.config.read().await;
                (
                    normalized_execution_mode(&cfg.execution_mode).to_string(),
                    cfg.mode_kind() == BotMode::Live && !cfg.shadow_trading,
                )
            };

            // An operator force is an explicit request for a taker entry now.
            // It goes through the taker path whatever the mode; the mode gate
            // below is for the automatic signal, which in maker mode is
            // expressed through quotes instead. Without this, a force in maker
            // mode was dropped here and reported upstream only as "did not
            // result in an opened trade".
            let route = if analysis.forced && exec_mode != "hybrid" {
                "forced"
            } else {
                exec_mode.as_str()
            };

            let out = match route {
                "maker" => {
                    info!("Execution actor: execution_mode=maker, skipping taker execute path");
                    None
                }
                "hybrid" => {
                    // In simulation mode, skip L2 stale check — no real execution happens
                    // and the L2 feed may not connect without open trades to subscribe to.
                    let l2_blocked = needs_live_handoff
                        && (bot.l2_feed_stale() || bot.degraded_no_trade_active());
                    if l2_blocked {
                        info!(
                            "Execution actor: hybrid gate blocked because feed is stale/degraded"
                        );
                        None
                    } else {
                        let (allowed, reason) = if analysis.forced {
                            (true, String::new())
                        } else {
                            should_take_in_hybrid(bot, &analysis).await
                        };
                        if allowed && !needs_live_handoff {
                            bot.execute_trade_strategy_for_origin(
                                &analysis,
                                hail_mary,
                                override_amount,
                                &strategy,
                                origin,
                            )
                            .await
                        } else if allowed {
                            match prepare_live_hybrid_handoff(bot, maker_state, outbox_tx).await {
                                Ok(()) => {
                                    bot.execute_trade_after_hybrid_handoff(
                                        &analysis,
                                        hail_mary,
                                        override_amount,
                                        &strategy,
                                        origin,
                                    )
                                    .await
                                }
                                Err(error) => {
                                    bot.set_degraded_no_trade(true);
                                    bot.halt_trading("hybrid_taker_handoff", error.to_string())
                                        .await;
                                    None
                                }
                            }
                        } else {
                            info!("Execution actor: hybrid gate blocked taker — {reason}");
                            None
                        }
                    }
                }
                _ => {
                    bot.execute_trade_strategy_for_origin(
                        &analysis,
                        hail_mary,
                        override_amount,
                        &strategy,
                        origin,
                    )
                    .await
                }
            };
            let _ = resp.send(out);
        }
        ExecutionCommand::CloseTrade {
            trade_id,
            exit_price,
            intent,
            resp,
        } => {
            if matches!(intent, CloseIntent::Market | CloseIntent::Resolution)
                && bot
                    .trade_lifecycle
                    .partial_close_intents
                    .read()
                    .await
                    .contains_key(&trade_id)
            {
                let _ = resp.send(None);
                return;
            }
            let initial_trade = bot
                .portfolio_state
                .trades
                .read()
                .await
                .iter()
                .find(|trade| trade.id == trade_id)
                .cloned();
            if let Some(trade) = initial_trade {
                if bot.refuse_mixed_mode_paper_exit(&trade).await {
                    let _ = resp.send(None);
                    return;
                }
            }
            // The operator's hands-off request applies immediately, including
            // while an earlier venue match is still settling.
            if intent == CloseIntent::Manual {
                let market = bot
                    .portfolio_state
                    .trades
                    .read()
                    .await
                    .iter()
                    .find(|trade| trade.id == trade_id && trade.has_unsettled_exposure())
                    .map(|trade| trade.market_id.clone());
                if let Some(market) = market {
                    maker_state.stand_down(&market);
                    if let Err(error) = db::maker::persist_manual_stand_down(&bot.db, &market).await
                    {
                        bot.set_degraded_no_trade(true);
                        bot.halt_trading("maker_manual_stop_persistence", error.to_string())
                            .await;
                        let unchanged = bot
                            .portfolio_state
                            .trades
                            .read()
                            .await
                            .iter()
                            .find(|trade| trade.id == trade_id)
                            .cloned();
                        let _ = resp.send(unchanged);
                        return;
                    }
                    retry_manual_quote_cleanup(bot, maker_state, outbox_tx).await;
                }
            }
            if bot.mode_kind().await == BotMode::Live {
                // Remove both sides before a risk/manual sale. Otherwise a
                // resting ask could sell the same inventory concurrently or
                // an entry could reopen it while the operator closes it.
                let market = bot
                    .portfolio_state
                    .trades
                    .read()
                    .await
                    .iter()
                    .find(|trade| trade.id == trade_id && trade.is_open())
                    .map(|trade| trade.market_id.clone());
                if let Some(market) = market {
                    let _ =
                        cancel_quotes_in(bot, maker_state, Some(&market), "live close", outbox_tx)
                            .await;
                    if maker_state.by_market.contains_key(&market)
                        || db::maker::pending_orders(&bot.db)
                            .await
                            .map_or(true, |orders| {
                                orders.iter().any(|order| order.market_id == market)
                            })
                    {
                        bot.set_degraded_no_trade(true);
                        bot.halt_trading(
                            "maker_cancel_before_close",
                            "live close waits for maker cancellation and settled fills",
                        )
                        .await;
                        let unchanged = bot
                            .portfolio_state
                            .trades
                            .read()
                            .await
                            .iter()
                            .find(|t| t.id == trade_id)
                            .cloned();
                        let _ = resp.send(unchanged);
                        return;
                    }
                }
            }
            let out = bot
                .close_trade_with_intent(trade_id, exit_price, intent)
                .await;
            // If trade is still open and FOK failed, schedule background retry.
            if let Some(ref t) = out {
                if t.is_open() && t.fok_failed == Some(1) {
                    TradingBot::spawn_fok_close_retry(
                        Arc::clone(bot),
                        trade_id,
                        exit_price,
                        intent,
                    );
                }
            }
            let _ = resp.send(out);
        }
        ExecutionCommand::ClosePair {
            first_trade_id,
            second_trade_id,
            exit_price,
            resp,
        } => {
            let partial_guard = bot.trade_lifecycle.partial_close_intents.read().await;
            if partial_guard.contains_key(&first_trade_id)
                || partial_guard.contains_key(&second_trade_id)
            {
                let _ = resp.send(Err(crate::bot::state::ActorCallError::PartialClosePending));
                return;
            }
            drop(partial_guard);
            if exit_price != Some(0.5)
                || !bot.paper_execution_allowed().await
                || !bot
                    .has_exact_complementary_pair(first_trade_id, second_trade_id)
                    .await
            {
                let _ = resp.send(Err(crate::bot::state::ActorCallError::PairNotMergeable));
                return;
            }
            // This command is used by the complementary-inventory rebalancer.
            // Redeeming a pair pays $1 across the two legs without a book sale.
            let result = bot
                .merge_simulated_pair_direct(first_trade_id, second_trade_id)
                .await;
            let _ = resp.send(result);
        }
        ExecutionCommand::UpsertQuoteState {
            order_id,
            market_id,
            token_id,
            side,
            price,
            size,
            resp,
        } => {
            let out = maker_state.upsert_quote(order_id, market_id, token_id, side, price, size);
            let _ = resp.send(out);
        }
        ExecutionCommand::RemoveQuoteState {
            order_id,
            reason,
            resp,
        } => {
            if let Some(r) = reason {
                info!("Maker quote remove: order_id={} reason={}", order_id, r);
            }
            let removed = maker_state.remove_quote(&order_id);
            let _ = resp.send(removed);
        }
        ExecutionCommand::ClearMarketQuotes {
            market_id,
            reason,
            resp,
        } => {
            if let Some(r) = reason {
                warn!("Maker market clear: market_id={} reason={}", market_id, r);
            }
            let cleared = maker_state.clear_market(&market_id);
            let _ = resp.send(cleared);
        }
        ExecutionCommand::UpdateInventoryState {
            market_id,
            net_yes_shares,
            net_no_shares,
            net_notional_usd,
            resp,
        } => {
            let state = maker_state.update_inventory(
                market_id,
                net_yes_shares,
                net_no_shares,
                net_notional_usd,
            );
            let _ = resp.send(state);
        }
        ExecutionCommand::SnapshotMakerState { resp } => {
            let _ = resp.send(maker_state.snapshot());
        }
        ExecutionCommand::FlushBestBidAsk => {
            let updates = bot.drain_pending_bbo_updates().await;
            for bbo in updates {
                debug_assert!(!bbo.token_id.is_empty());
                maker_state.update_bbo(bbo.clone());
                process_maker_bbo_event(bot, maker_state, &bbo, outbox_tx).await;
            }
        }
        ExecutionCommand::RetainL2Tokens {
            token_ids,
            venue_rules,
        } => {
            let evicted = maker_state.retain_l2_tokens(&token_ids);
            if evicted > 0 {
                info!(
                    "Maker L2 evicted {evicted} book(s) for untargeted tokens; {} remain",
                    maker_state.l2_bbo_by_token.len()
                );
            }
            let targeted: HashSet<String> = venue_rules.keys().cloned().collect();
            maker_state
                .venue_rules_by_market
                .retain(|market_id, _| targeted.contains(market_id));
            for (market_id, rules) in venue_rules {
                if maker_state.set_venue_rules(&market_id, rules) {
                    info!(
                        "Venue rules for market {market_id}: tick {} min order {} shares",
                        rules.tick_size, rules.min_order_size
                    );
                }
            }
        }
        ExecutionCommand::SetVenueRules { market_id, rules } => {
            if maker_state.set_venue_rules(&market_id, rules) {
                info!(
                    "Venue rules for market {market_id}: tick {} min order {} shares",
                    rules.tick_size, rules.min_order_size
                );
            }
        }
        ExecutionCommand::SetL2Stale {
            stale,
            code,
            reason,
        } => {
            let was_stale = maker_state.l2_stale;
            // Only log on transition (healthy → stale) to avoid spamming during
            // startup "no target markets" polling.
            if stale && !was_stale && code.as_deref() != Some("execution_mode_taker") {
                warn!("Maker L2 stale: code={code:?} reason={reason:?}");
            }
            maker_state.set_l2_stale(stale, code.clone(), reason.clone());
            if stale && !was_stale {
                let removed = cancel_all_quotes(bot, maker_state, "l2_stale", outbox_tx).await;
                if removed > 0 {
                    warn!("Maker stale cleanup removed {removed} quote(s)");
                }
                if !maker_state.quotes.is_empty() {
                    warn!(
                        "Maker stale cleanup incomplete removed={} remaining={}",
                        removed,
                        maker_state.quotes.len()
                    );
                    bot.set_degraded_no_trade(true);
                    bot.set_kill_switch(true);
                    bot.record_reconcile_error(
                        "maker_quote_cleanup",
                        format!(
                            "l2 stale cleanup incomplete removed={} remaining={}",
                            removed,
                            maker_state.quotes.len()
                        ),
                    )
                    .await;
                }
            }
        }
        ExecutionCommand::AppendOutboxAudit {
            stage,
            market_id,
            order_id,
            token_id,
            action,
            status,
            details,
        } => {
            let priority = infer_outbox_priority(&stage, &action, &status);
            outbox_emit(
                outbox_tx,
                &bot.db,
                OutboxEvent {
                    stage,
                    market_id,
                    order_id,
                    token_id,
                    action,
                    status,
                    details,
                    priority,
                },
            );
        }
    }
}

pub struct ExecutionActorHandle {
    pub sender: mpsc::Sender<ExecutionCommand>,
    pub task: tokio::task::JoinHandle<Result<(), String>>,
}

pub fn spawn_execution_actor(bot: Arc<TradingBot>) -> ExecutionActorHandle {
    let (tx, mut rx) = mpsc::channel::<ExecutionCommand>(512);
    let (outbox_tx, outbox_task) = spawn_outbox_writer_handle(bot.db.clone());

    let monitor_bot = Arc::clone(&bot);
    let task = crate::task_monitor::spawn_critical_bot_task_handle(
        "execution_actor",
        monitor_bot,
        async move {
            let mut maker_state = MakerActorState::default();
            if let Err(error) = restore_maker_controls(&bot, &mut maker_state).await {
                bot.set_degraded_no_trade(true);
                bot.halt_trading("maker_manual_stop_restore", error.to_string())
                    .await;
            }
            if let Err(error) = db::paper::expire_unrestored_quotes(&bot.db).await {
                bot.set_degraded_no_trade(true);
                bot.halt_trading("paper_maker_restart_persistence", error.to_string())
                    .await;
            }
            maintain_live_maker_state(&bot, &mut maker_state).await;
            let mut q_high: VecDeque<ExecutionCommand> = VecDeque::new();
            let mut q_close: VecDeque<ExecutionCommand> = VecDeque::new();
            let mut q_medium: VecDeque<ExecutionCommand> = VecDeque::new();
            let mut q_low: VecDeque<ExecutionCommand> = VecDeque::new();
            let mut cleanup_tick = tokio::time::interval(Duration::from_secs(1));
            cleanup_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut last_cleanup = Instant::now();

            loop {
                if q_high.is_empty()
                    && q_close.is_empty()
                    && q_medium.is_empty()
                    && q_low.is_empty()
                {
                    let cmd = tokio::select! {
                        cmd = rx.recv() => cmd,
                        _ = cleanup_tick.tick() => {
                            maintain_quote_lifecycle(&bot, &mut maker_state, &outbox_tx).await;
                            maintain_live_maker_state(&bot, &mut maker_state).await;
                            retry_manual_quote_cleanup(&bot, &mut maker_state, &outbox_tx).await;
                            last_cleanup = Instant::now();
                            continue;
                        }
                    };
                    let Some(cmd) = cmd else {
                        break;
                    };
                    try_push_lane(cmd, &mut q_high, &mut q_close, &mut q_medium, &mut q_low);
                    metrics::set_actor_lane_depths(
                        q_high.len(),
                        q_close.len() + q_medium.len(),
                        q_low.len(),
                    );
                }

                // Keep queued work bounded and yield to execution even while
                // senders continuously refill the bounded ingress channel.
                for _ in 0..512 {
                    if q_high.len() >= LANE_HIGH_CAPACITY
                        || q_close.len() + q_medium.len() >= LANE_MEDIUM_CAPACITY
                    {
                        break;
                    }
                    let Ok(cmd) = rx.try_recv() else {
                        break;
                    };
                    try_push_lane(cmd, &mut q_high, &mut q_close, &mut q_medium, &mut q_low);
                }
                metrics::set_actor_lane_depths(
                    q_high.len(),
                    q_close.len() + q_medium.len(),
                    q_low.len(),
                );

                let next = if let Some(cmd) = q_high.pop_front() {
                    Some(cmd)
                } else if let Some(cmd) = q_close.pop_front() {
                    Some(cmd)
                } else {
                    q_medium.pop_front().or_else(|| q_low.pop_front())
                };

                let Some(cmd) = next else { continue };
                process_execution_command(cmd, &bot, &mut maker_state, &outbox_tx).await;
                if last_cleanup.elapsed() >= Duration::from_secs(1) {
                    maintain_quote_lifecycle(&bot, &mut maker_state, &outbox_tx).await;
                    maintain_live_maker_state(&bot, &mut maker_state).await;
                    retry_manual_quote_cleanup(&bot, &mut maker_state, &outbox_tx).await;
                    last_cleanup = Instant::now();
                }
                metrics::set_actor_lane_depths(
                    q_high.len(),
                    q_close.len() + q_medium.len(),
                    q_low.len(),
                );
            }

            metrics::set_actor_lane_depths(0, 0, 0);
            // Closing the final sender lets the writer flush every queued
            // audit before the actor task reports that shutdown is complete.
            drop(outbox_tx);
            match outbox_task.await {
                Ok(Ok(())) => {}
                Ok(Err(message)) => panic!("execution outbox writer failed: {message}"),
                Err(error) => panic!("execution outbox writer join failed: {error}"),
            }
        },
    );

    ExecutionActorHandle { sender: tx, task }
}

#[cfg(test)]
#[path = "maker_live_tests.rs"]
pub(crate) mod maker_live_tests;

#[cfg(test)]
#[path = "actor_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "execution_controls_tests.rs"]
mod execution_controls_tests;
