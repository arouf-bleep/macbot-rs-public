//! Deterministic, conservative paper execution assumptions.
//!
//! BBO is not a trade tape. Maker depth changes below are an explicitly
//! uncalibrated opportunity proxy; they never claim price-time priority.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PaperFillSettings {
    pub latency_ms: u64,
    pub available_depth_fraction: f64,
    pub queue_ahead_shares: f64,
    pub max_fill_fraction: f64,
    pub fill_on_touch: bool,
}

impl From<&Config> for PaperFillSettings {
    fn from(config: &Config) -> Self {
        Self {
            latency_ms: config.sim_order_latency_ms,
            available_depth_fraction: config.sim_available_depth_fraction,
            queue_ahead_shares: config.maker_sim_queue_ahead_shares,
            max_fill_fraction: config.maker_sim_max_fill_fraction,
            fill_on_touch: config.maker_sim_fill_on_touch,
        }
    }
}

impl PaperFillSettings {
    fn valid(self) -> bool {
        // The effective admission delay also includes the maker's minimum
        // quote lifetime, which may exceed the configured network latency cap.
        self.available_depth_fraction.is_finite()
            && (0.0..=1.0).contains(&self.available_depth_fraction)
            && self.queue_ahead_shares.is_finite()
            && (0.0..=1_000_000.0).contains(&self.queue_ahead_shares)
            && self.max_fill_fraction.is_finite()
            && (0.0..=1.0).contains(&self.max_fill_fraction)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MakerPaperState {
    pub initial_size: f64,
    pub queue_remaining: f64,
    pub last_sequence: Option<u64>,
    pub last_observed_at_us: Option<i64>,
    pub last_contra_price: Option<f64>,
    pub last_contra_size: f64,
}

impl MakerPaperState {
    pub fn new(size: f64, settings: PaperFillSettings) -> Self {
        Self {
            initial_size: size,
            queue_remaining: settings.queue_ahead_shares,
            last_sequence: None,
            last_observed_at_us: None,
            last_contra_price: None,
            last_contra_size: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MakerPaperInput {
    pub settings: PaperFillSettings,
    pub state: MakerPaperState,
    pub side: PaperSide,
    pub quote_price: f64,
    pub remaining_shares: f64,
    pub age_ms: u64,
    pub sequence: Option<u64>,
    pub observed_at_us: i64,
    pub contra_price: Option<f64>,
    pub contra_size: Option<f64>,
    pub usable_book: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MakerPaperStep {
    pub state: MakerPaperState,
    pub fill_shares: f64,
    pub reason: String,
}

/// A repeated/frozen ladder cannot manufacture more available liquidity.
/// At the same contra price only a positive *increase* in displayed size
/// supplies a new opportunity. A new eligible price supplies its own depth.
/// Unused depth from an update is discarded rather than recycled on timers.
pub fn maker_fill_step(input: &MakerPaperInput) -> MakerPaperStep {
    let mut result = MakerPaperStep {
        state: input.state.clone(),
        fill_shares: 0.0,
        reason: "no_opportunity".into(),
    };
    let state = &mut result.state;
    if !input.settings.valid()
        || !input.quote_price.is_finite()
        || input.quote_price <= 0.0
        || !(0.0..1.0).contains(&input.quote_price)
        || !input.remaining_shares.is_finite()
        || input.remaining_shares <= 0.0
        || !state.initial_size.is_finite()
        || state.initial_size <= 0.0
        || input.remaining_shares > state.initial_size
        || !state.queue_remaining.is_finite()
        || state.queue_remaining < 0.0
        || !state.last_contra_size.is_finite()
        || state.last_contra_size < 0.0
    {
        result.reason = "invalid_assumptions".into();
        return result;
    }
    if state
        .last_observed_at_us
        .is_some_and(|last| input.observed_at_us <= last)
        || matches!((state.last_sequence, input.sequence), (Some(last), Some(next)) if next <= last)
    {
        result.reason = "duplicate_or_old_book".into();
        return result;
    }
    state.last_observed_at_us = Some(input.observed_at_us);
    state.last_sequence = input.sequence.or(state.last_sequence);
    let price = input
        .contra_price
        .filter(|price| price.is_finite() && *price > 0.0 && *price < 1.0);
    let size = input
        .contra_size
        .filter(|size| size.is_finite() && *size > 0.0)
        .unwrap_or(0.0);
    let same_level = price.is_some() && price == state.last_contra_price;
    let new_depth = if same_level {
        (size - state.last_contra_size).max(0.0)
    } else {
        size
    };
    state.last_contra_price = price;
    state.last_contra_size = size;
    // Observations before admission and during feed gaps are discarded. A
    // frozen crossing after the delay cannot retroactively fill this order.
    if !input.usable_book || input.age_ms < input.settings.latency_ms {
        result.reason = "not_admitted_or_unusable_book".into();
        return result;
    }
    let crosses = price.is_some_and(|price| match input.side {
        PaperSide::Buy if input.settings.fill_on_touch => price <= input.quote_price,
        PaperSide::Buy => price < input.quote_price,
        PaperSide::Sell if input.settings.fill_on_touch => price >= input.quote_price,
        PaperSide::Sell => price > input.quote_price,
    });
    if !crosses {
        return result;
    }
    let opportunity = new_depth * input.settings.available_depth_fraction;
    let queue_consumed = opportunity.min(state.queue_remaining);
    state.queue_remaining -= queue_consumed;
    let available = opportunity - queue_consumed;
    result.fill_shares = available
        .min(input.remaining_shares)
        .min(state.initial_size * input.settings.max_fill_fraction);
    result.reason = if result.fill_shares > 1e-9 {
        "depth_proxy_partial_fill"
    } else if state.queue_remaining > 0.0 {
        "queued_behind_existing_size"
    } else {
        "no_new_depth"
    }
    .into();
    result
}

pub(crate) fn capture_maker_fill_step(input: &MakerPaperInput) -> MakerPaperStep {
    let result = maker_fill_step(input);
    crate::replay::capture(
        "maker_paper_fill",
        || serde_json::to_value(input).expect("finite paper fill input"),
        &result,
    );
    result
}

/// Retained IOC model for old callers and captured input compatibility.
/// Runtime taker entries explicitly request FOK through the policy-aware path.
pub fn taker_arrival_fill(
    arrival_book: &Value,
    limit_price: f64,
    requested_shares: f64,
    available_depth_fraction: f64,
) -> Option<(f64, f64)> {
    taker_arrival_fill_with_policy(
        arrival_book,
        limit_price,
        requested_shares,
        available_depth_fraction,
        TakerPaperFillPolicy::ImmediateOrCancel,
    )
}

/// Haircut the arrival book while keeping the original decision limit fixed.
/// The recorded policy determines whether an unfilled remainder is discarded
/// (IOC) or causes the whole hypothetical execution to be refused (FOK).
pub fn taker_arrival_fill_with_policy(
    arrival_book: &Value,
    limit_price: f64,
    requested_shares: f64,
    available_depth_fraction: f64,
    fill_policy: TakerPaperFillPolicy,
) -> Option<(f64, f64)> {
    if !limit_price.is_finite()
        || !requested_shares.is_finite()
        || !available_depth_fraction.is_finite()
    {
        return None;
    }
    let input = TakerPaperInput {
        arrival_book: arrival_book.clone(),
        limit_price,
        requested_shares,
        available_depth_fraction,
        fill_policy,
    };
    let result = taker_arrival_fill_core(&input);
    crate::replay::capture(
        "taker_paper_fill",
        || serde_json::to_value(&input).expect("paper taker input"),
        &result,
    );
    result
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TakerPaperFillPolicy {
    /// Missing policy in an older capture retains its original IOC semantics.
    #[default]
    ImmediateOrCancel,
    FillOrKill,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakerPaperInput {
    pub arrival_book: Value,
    pub limit_price: f64,
    pub requested_shares: f64,
    pub available_depth_fraction: f64,
    #[serde(default)]
    pub fill_policy: TakerPaperFillPolicy,
}

pub fn taker_arrival_fill_core(input: &TakerPaperInput) -> Option<(f64, f64)> {
    let TakerPaperInput {
        arrival_book,
        limit_price,
        requested_shares,
        available_depth_fraction,
        fill_policy,
    } = input;
    if !limit_price.is_finite()
        || !(0.0..1.0).contains(limit_price)
        || *limit_price <= 0.0
        || !available_depth_fraction.is_finite()
        || !(0.0..=1.0).contains(available_depth_fraction)
        || !requested_shares.is_finite()
        || *requested_shares <= 0.0
    {
        return None;
    }
    let asks = crate::json_util::extract_levels(arrival_book, "asks")
        .into_iter()
        .take_while(|(price, _)| price <= limit_price);
    let (price, shares, complete) =
        fill_arrival_levels(asks, *requested_shares, *available_depth_fraction)?;
    if *fill_policy == TakerPaperFillPolicy::FillOrKill && !complete {
        return None;
    }
    Some((price, shares))
}

/// Walk only executable capacity. Completion is established by actually
/// consuming the remaining quantity, never by an absolute shortfall epsilon.
fn fill_arrival_levels(
    levels: impl Iterator<Item = (f64, f64)>,
    requested_shares: f64,
    depth_fraction: f64,
) -> Option<(f64, f64, bool)> {
    let mut remaining = requested_shares;
    let mut value = 0.0;
    for (price, size) in levels {
        let take = remaining.min(size * depth_fraction);
        value += take * price;
        remaining -= take;
        if remaining == 0.0 {
            break;
        }
    }
    let filled = requested_shares - remaining;
    // The existing execution callers treat sub-nanoshare dust as no fill.
    // In particular, zero available depth can never imply a completed sale.
    if filled <= 1e-9 || !filled.is_finite() || !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some((value / filled, filled, remaining == 0.0))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakerPaperSellInput {
    pub arrival_book: Value,
    pub requested_shares: f64,
    pub available_depth_fraction: f64,
}

/// Full-position paper sales are FOK: insufficient haircutted arrival depth
/// refuses the entire sale without inventing a fill for the remainder.
/// Returns (volume-weighted price, best arrival bid) on a complete fill.
pub fn taker_arrival_sell(
    arrival_book: &Value,
    requested_shares: f64,
    available_depth_fraction: f64,
) -> Option<(f64, f64)> {
    if !requested_shares.is_finite() || !available_depth_fraction.is_finite() {
        return None;
    }
    let input = TakerPaperSellInput {
        arrival_book: arrival_book.clone(),
        requested_shares,
        available_depth_fraction,
    };
    let result = taker_arrival_sell_core(&input);
    crate::replay::capture(
        "taker_paper_sell",
        || serde_json::to_value(&input).expect("paper sell input"),
        &result,
    );
    result
}

pub fn taker_arrival_sell_core(input: &TakerPaperSellInput) -> Option<(f64, f64)> {
    if !input.requested_shares.is_finite()
        || input.requested_shares <= 0.0
        || !input.available_depth_fraction.is_finite()
        || !(0.0..=1.0).contains(&input.available_depth_fraction)
    {
        return None;
    }
    let bids: Vec<_> = crate::json_util::extract_levels(&input.arrival_book, "bids")
        .into_iter()
        .filter(|(price, _)| *price < 1.0)
        .collect();
    let best_bid = bids.first()?.0;
    let (price, _, complete) = fill_arrival_levels(
        bids.into_iter(),
        input.requested_shares,
        input.available_depth_fraction,
    )?;
    if complete {
        Some((price, best_bid))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> MakerPaperInput {
        let settings = PaperFillSettings {
            latency_ms: 250,
            available_depth_fraction: 0.25,
            queue_ahead_shares: 5.0,
            max_fill_fraction: 0.25,
            fill_on_touch: false,
        };
        MakerPaperInput {
            settings,
            state: MakerPaperState::new(20.0, settings),
            side: PaperSide::Buy,
            quote_price: 0.50,
            remaining_shares: 20.0,
            age_ms: 250,
            sequence: Some(1),
            observed_at_us: 1000,
            contra_price: Some(0.49),
            contra_size: Some(40.0),
            usable_book: true,
        }
    }

    #[test]
    fn a_long_quote_lifetime_delays_admission_without_disabling_fills() {
        let mut case = input();
        case.settings.latency_ms = 60_001;
        case.age_ms = 60_000;
        let early = capture_maker_fill_step(&case);
        assert_eq!(early.reason, "not_admitted_or_unusable_book");
        assert_eq!(early.fill_shares, 0.0);
        case.state = early.state;
        case.age_ms = 60_001;
        case.observed_at_us += 1;
        case.sequence = Some(2);
        case.contra_size = Some(80.0);
        let admitted = capture_maker_fill_step(&case);
        assert_eq!(admitted.fill_shares, 5.0);
    }

    #[test]
    fn admission_discards_early_liquidity_even_after_latency_expires() {
        let mut case = input();
        case.age_ms = 249;
        let step = capture_maker_fill_step(&case);
        assert_eq!(step.fill_shares, 0.0);
        assert_eq!(step.state.queue_remaining, 5.0);
        case.state = step.state;
        case.age_ms = 250;
        case.observed_at_us += 1;
        case.sequence = Some(2);
        assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
        case.contra_size = Some(80.0);
        case.observed_at_us += 1;
        case.sequence = Some(3);
        assert_eq!(maker_fill_step(&case).fill_shares, 5.0);
    }

    #[test]
    fn queue_depth_and_partial_limits_constrain_each_distinct_opportunity() {
        let mut case = input();
        case.contra_size = Some(12.0);
        let queued = capture_maker_fill_step(&case);
        assert_eq!(queued.fill_shares, 0.0);
        assert_eq!(queued.state.queue_remaining, 2.0);
        case.state = queued.state;
        case.sequence = Some(2);
        case.observed_at_us += 1;
        case.contra_size = Some(52.0);
        let partial = capture_maker_fill_step(&case);
        assert_eq!(partial.fill_shares, 5.0);
        assert_eq!(partial.state.queue_remaining, 0.0);
        case.state = partial.state;
        case.remaining_shares = 2.0;
        case.sequence = Some(3);
        case.observed_at_us += 1;
        case.contra_size = Some(92.0);
        assert_eq!(maker_fill_step(&case).fill_shares, 2.0);
    }

    #[test]
    fn duplicates_frozen_sizes_and_out_of_order_updates_never_repeat_a_fill() {
        let mut case = input();
        let first = capture_maker_fill_step(&case);
        assert_eq!(first.fill_shares, 5.0);
        case.state = first.state;
        assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
        case.sequence = Some(2);
        case.observed_at_us += 1;
        let frozen = maker_fill_step(&case);
        assert_eq!(frozen.fill_shares, 0.0);
        case.state = frozen.state;
        case.sequence = Some(1);
        case.contra_size = Some(1000.0);
        assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
    }

    #[test]
    fn touch_stale_missing_or_nonfinite_depth_cannot_fill() {
        let mut case = input();
        case.contra_price = Some(0.50);
        assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
        case = input();
        case.usable_book = false;
        assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
        for depth in [None, Some(f64::NAN), Some(-1.0)] {
            case = input();
            case.contra_size = depth;
            assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
        }
        case = input();
        case.settings.max_fill_fraction = f64::INFINITY;
        assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
    }

    #[test]
    fn sell_queue_uses_bid_depth_and_strict_trade_through() {
        let mut case = input();
        case.side = PaperSide::Sell;
        case.contra_price = Some(0.51);
        assert_eq!(maker_fill_step(&case).fill_shares, 5.0);
        case.contra_price = Some(0.50);
        assert_eq!(maker_fill_step(&case).fill_shares, 0.0);
    }

    #[test]
    fn taker_uses_arrival_prices_and_haircut_without_moving_the_limit() {
        let old = serde_json::json!({"asks":[{"price":"0.50","size":"40"}]});
        let moved = serde_json::json!({"asks":[{"price":"0.55","size":"40"}]});
        assert_eq!(
            taker_arrival_fill(&old, 0.52, 20.0, 0.25),
            Some((0.5, 10.0))
        );
        assert_eq!(taker_arrival_fill(&moved, 0.52, 20.0, 0.25), None);
        assert_eq!(taker_arrival_fill(&old, 0.52, 20.0, 0.0), None);
        assert_eq!(taker_arrival_fill(&old, 0.52, f64::INFINITY, 1.0), None);
    }

    #[test]
    fn paper_market_sale_requires_every_share_in_haircutted_arrival_depth() {
        let book = serde_json::json!({"bids":[
            {"price":"0.40","size":"12"},
            {"price":"0.60","size":"8"}
        ]});
        assert_eq!(taker_arrival_sell(&book, 4.0, 0.25), Some((0.50, 0.60)));
        assert_eq!(taker_arrival_sell(&book, 6.0, 0.25), None);
        assert_eq!(taker_arrival_sell(&book, 1.0, 0.0), None);
        assert_eq!(taker_arrival_sell(&book, f64::NAN, 0.25), None);
        let moved = serde_json::json!({"bids":[{"price":"0.20","size":"100"}]});
        assert_eq!(taker_arrival_sell(&moved, 4.0, 0.25), Some((0.20, 0.20)));
    }

    #[test]
    fn paper_sale_cannot_round_zero_or_insufficient_depth_into_a_full_fill() {
        let book = serde_json::json!({"bids":[{"price":0.5,"size":1.0}]});
        assert_eq!(taker_arrival_sell(&book, 5e-10, 0.0), None);
        let short = serde_json::json!({"bids":[{"price":0.5,"size":0.9999999995}]});
        assert_eq!(taker_arrival_sell(&short, 1.0, 1.0), None);
    }

    #[test]
    fn paper_taker_limits_must_be_inside_the_binary_order_price_domain() {
        let book = serde_json::json!({"asks":[{"price":0.5,"size":10.0}]});
        for limit in [0.0, -0.1, 1.0, 1.1, f64::NAN, f64::INFINITY] {
            assert_eq!(
                taker_arrival_fill(&book, limit, 5.0, 1.0),
                None,
                "invalid limit {limit}"
            );
        }
    }

    #[test]
    fn maker_remaining_size_cannot_exceed_the_admitted_order() {
        let mut case = input();
        case.remaining_shares = case.state.initial_size + 1.0;
        let step = maker_fill_step(&case);
        assert_eq!(step.fill_shares, 0.0);
        assert_eq!(step.reason, "invalid_assumptions");
        assert_eq!(step.state, case.state);
    }

    #[test]
    fn recorded_taker_policy_distinguishes_partial_ioc_from_all_or_nothing_fok() {
        let book = serde_json::json!({"asks":[
            {"price":0.52,"size":20.0}, {"price":0.50,"size":20.0}
        ]});
        let mut input = TakerPaperInput {
            arrival_book: book,
            limit_price: 0.52,
            requested_shares: 12.0,
            available_depth_fraction: 0.25,
            fill_policy: TakerPaperFillPolicy::ImmediateOrCancel,
        };
        assert_eq!(taker_arrival_fill_core(&input), Some((0.51, 10.0)));
        input.fill_policy = TakerPaperFillPolicy::FillOrKill;
        assert_eq!(taker_arrival_fill_core(&input), None);
        input.requested_shares = 8.0;
        let (price, size) = taker_arrival_fill_core(&input).unwrap();
        assert!((price - 0.5075).abs() < 1e-12);
        assert_eq!(size, 8.0);
        assert_eq!(
            serde_json::to_value(&input).unwrap()["fill_policy"],
            "fill_or_kill"
        );
        input.limit_price = 0.51;
        assert_eq!(
            taker_arrival_fill_core(&input),
            None,
            "FOK cannot buy beyond the fixed limit"
        );
    }

    #[test]
    fn missing_policy_keeps_legacy_ioc_input_semantics() {
        let old_input = serde_json::json!({
            "arrival_book":{"asks":[{"price":0.5,"size":20.0}]},
            "limit_price":0.52,"requested_shares":8.0,"available_depth_fraction":0.25
        });
        let parsed: TakerPaperInput = serde_json::from_value(old_input).unwrap();
        assert_eq!(parsed.fill_policy, TakerPaperFillPolicy::ImmediateOrCancel);
        assert_eq!(taker_arrival_fill_core(&parsed), Some((0.5, 5.0)));
    }

    #[test]
    fn fok_sells_walk_exact_capacity_and_reject_non_order_prices() {
        let book = serde_json::json!({"bids":[
            {"price":0.5,"size":4.0}, {"price":0.6,"size":4.0}
        ]});
        assert_eq!(taker_arrival_sell(&book, 2.0, 0.25), Some((0.55, 0.6)));
        assert_eq!(taker_arrival_sell(&book, 2.0000000005, 0.25), None);
        for price in [0.0, 1.0, 1.1] {
            let invalid = serde_json::json!({"bids":[{"price":price,"size":100.0}]});
            assert_eq!(taker_arrival_sell(&invalid, 5.0, 1.0), None);
        }
    }
}
