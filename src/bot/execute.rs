//! Trade execution — routes to live (CLOB limit order) or paper trade paths.

use super::actor::ExecutionOrigin;

use crate::http_client::JsonResponseExt;
use chrono::{Timelike, Utc};
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::analysis::AnalysisResult as ParsedAnalysis;
use crate::clob::{
    fetch_public_midpoint, fetch_public_order_book, LimitOrderArgs, OrderType, PublicBookSnapshot,
    Side, SimTradePriceEvidence,
};
use crate::domain::{BotMode, Direction, Signal, TradeStatus};
use crate::http_client::HTTP;
use crate::json_util::extract_levels;
use crate::metrics;

use super::risk::{
    evaluate_entry_coordination, evaluate_trade_guardrails, mode_min_confidence, session_bucket,
    EntryCoordinationProposal, EntryCoordinationSnapshot, ProposedTrade,
};
use super::state::TradingBot;
use crate::db::{self, Trade};

struct ValidatedTradeInput {
    signal: Signal,
    entry_price: f64,
    market_id: String,
    question: String,
    end_date: String,
    liquidity: f64,
    effective_conf: f64,
    raw_conf: Option<f64>,
    amount: f64,
    reason: String,
    signals_json: Option<String>,
}

/// Expected return per dollar at the price actually paid (or the live order's
/// worst signed limit), using the posterior rather than heuristic confidence.
pub(super) fn entry_return_at_price(
    analysis: &ParsedAnalysis,
    signal: Signal,
    price: f64,
    cost_fraction: f64,
) -> Option<f64> {
    if !price.is_finite()
        || price <= 0.0
        || price > 1.0
        || !cost_fraction.is_finite()
        || cost_fraction < 0.0
    {
        return None;
    }
    let p_up = match analysis.p_up {
        Some(p) if p.is_finite() && (0.0..=1.0).contains(&p) => p,
        _ => return None,
    };
    let probability = match signal {
        Signal::BuyYes => p_up,
        Signal::BuyNo => 1.0 - p_up,
        Signal::Hold => return None,
    };
    Some(probability / price - 1.0 - cost_fraction)
}

fn buy_no_trend_up_guard_reason(
    cfg: &crate::config::Config,
    analysis: &ParsedAnalysis,
    regime: &str,
) -> Option<String> {
    if !cfg.buy_no_trend_up_guard_enabled
        || analysis.forced
        || analysis.signal != Signal::BuyNo
        || regime != "TREND_UP"
    {
        return None;
    }
    let p_up = analysis.p_up.unwrap_or(0.5);
    if p_up <= cfg.buy_no_trend_up_max_p_up {
        return None;
    }
    Some(format!(
        "BUY_NO trend-up guard: p_up {:.3} > max {:.3} in TREND_UP",
        p_up, cfg.buy_no_trend_up_max_p_up
    ))
}

/// The most a taker entry will pay for a token whose midpoint is `mid`.
///
/// Taker mode used to place its buy limit 2% *below* mid — a resting order —
/// and then, in simulation, walk the asks for an *immediate* fill. On a
/// one-cent-spread book nothing sits 2% below mid, so a correctly read book
/// never filled, and the only simulated taker entry in the record filled
/// because the price crashed through the limit between two REST calls. The
/// two halves contradicted each other; the misread book (fixed earlier) had
/// hidden that by failing every entry at the thin-book gate first.
///
/// Taker means crossing the spread. The limit sits above mid by
/// `max_slippage`, so a marketable order fills against the asks it can reach
/// and stops where the operator said to stop. The same figure is used in the
/// simulated and the live path, so the paper record describes the live bot.
pub(crate) fn taker_limit_price(mid: f64, max_slippage: f64) -> f64 {
    // A cap of zero would make the limit exactly mid, which on any real book
    // is below the best ask and can never fill; one tick is the floor.
    let reach = max_slippage.max(0.01);
    (mid + reach).clamp(0.01, 0.99)
}

fn live_taker_limit_price(book: &serde_json::Value, max_slippage: f64) -> Option<f64> {
    let bid = extract_levels(book, "bids").first()?.0;
    let ask = extract_levels(book, "asks").first()?.0;
    let tick = book
        .get("tick_size")
        .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))?;
    if !tick.is_finite()
        || tick <= 0.0
        || tick >= 0.5
        || !max_slippage.is_finite()
        || max_slippage < 0.0
        || ask < bid
    {
        return None;
    }
    // The book belongs to the selected token (including NO). Keep the price
    // and spread from one snapshot, and round DOWN to respect the spend cap.
    let cap = ((bid + ask) / 2.0 + max_slippage.max(tick)).min(1.0 - tick);
    let price = ((cap / tick + 1e-9).floor() * tick).max(tick);
    (price >= ask && price <= cap + 1e-8).then_some(price)
}

fn book_best_bid_ask(book: &serde_json::Value) -> (f64, f64) {
    let bids = extract_levels(book, "bids");
    let asks = extract_levels(book, "asks");
    // When a side is missing, use 0.50 (neutral midpoint) as default
    // instead of 0.0/1.0 which creates a fake 1.0 spread on empty books.
    // extract_levels already rejects non-finite and <= 0 prices.
    let best_bid = bids.first().map(|x| x.0).unwrap_or(0.50);
    let best_ask = asks.first().map(|x| x.0).unwrap_or(0.50);
    // Final sanity: if either value escaped validation, clamp to safe range.
    (best_bid.clamp(0.001, 1.0), best_ask.clamp(0.001, 1.0))
}

fn book_total_usd(book: &serde_json::Value) -> f64 {
    let sum_side = |side: &str| -> f64 {
        extract_levels(book, side)
            .into_iter()
            .map(|(p, s)| p * s)
            .sum::<f64>()
    };
    sum_side("bids") + sum_side("asks")
}

fn book_side_usd(book: &serde_json::Value, side: &str) -> f64 {
    extract_levels(book, side)
        .into_iter()
        .map(|(p, s)| p * s)
        .sum::<f64>()
}

#[cfg(test)]
fn apply_polled_entry_fill_update(
    trade: &mut Trade,
    matched_size: f64,
    avg_px: f64,
    status: &str,
) -> bool {
    const ORDER_NOTIONAL_TOLERANCE_USD: f64 = 0.01;

    if matched_size <= 0.0 {
        return false;
    }

    let mut dirty = false;
    if (avg_px - trade.entry_price).abs() > 0.001 {
        info!(
            "Fill monitor: trade {} fill price updated {:.4} → {:.4}",
            trade.id, trade.entry_price, avg_px
        );
        trade.entry_price = avg_px;
        dirty = true;
    }
    if (matched_size - trade.shares).abs() > 1e-9 {
        info!(
            "Fill monitor: trade {} shares updated {:.6} → {:.6}",
            trade.id, trade.shares, matched_size
        );
        trade.shares = matched_size;
        dirty = true;
    }

    let filled_notional = trade.shares * trade.entry_price;
    if (filled_notional - trade.amount).abs() > ORDER_NOTIONAL_TOLERANCE_USD {
        info!(
            "Fill monitor: trade {} amount updated {:.4} → {:.4}",
            trade.id, trade.amount, filled_notional
        );
        trade.amount = filled_notional;
        dirty = true;
    }

    if trade.entry_order_inactive(matched_size, avg_px, status) && trade.order_id.is_some() {
        info!(
            "Fill monitor: trade {} entry order is no longer active; clearing order id",
            trade.id
        );
        trade.order_id = None;
        dirty = true;
    }

    dirty
}

#[cfg(test)]
fn simulate_buy_fill(book: &serde_json::Value, limit_price: f64, size: f64) -> Option<(f64, f64)> {
    let result = simulate_buy_fill_core(book, limit_price, size);
    crate::replay::capture(
        "simulated_buy_fill",
        || {
            serde_json::json!({
                "book": book, "limit_price": limit_price, "size": size
            })
        },
        &result,
    );
    result
}

pub(crate) fn simulate_buy_fill_core(
    book: &serde_json::Value,
    limit_price: f64,
    size: f64,
) -> Option<(f64, f64)> {
    if size <= 0.0 || !limit_price.is_finite() || limit_price <= 0.0 {
        return None;
    }
    // extract_levels already filters non-finite and <= 0 prices/sizes.
    let asks = extract_levels(book, "asks");
    let mut remaining = size;
    let mut cost = 0.0;
    let mut filled = 0.0;
    for (price, qty) in asks {
        if price > limit_price {
            break;
        }
        let take = remaining.min(qty);
        cost += take * price;
        filled += take;
        remaining -= take;
        if remaining <= 1e-9 {
            break;
        }
    }
    if filled <= 1e-9 {
        None
    } else {
        Some((cost / filled, filled))
    }
}

fn scale_entry_amount(
    cfg: &crate::config::Config,
    regime: &str,
    effective_confidence: f64,
    has_override: bool,
    amount: f64,
) -> f64 {
    let result = scale_entry_amount_core(cfg, regime, effective_confidence, has_override, amount);
    crate::replay::capture(
        "entry_sizing",
        || {
            serde_json::json!({
                "config": cfg.redacted_value(), "regime": regime, "effective_confidence": effective_confidence,
                "has_override": has_override, "amount": amount
            })
        },
        &result,
    );
    result
}

pub(crate) fn scale_entry_amount_core(
    cfg: &crate::config::Config,
    regime: &str,
    effective_confidence: f64,
    has_override: bool,
    mut amount: f64,
) -> f64 {
    if !has_override {
        let min_conf = mode_min_confidence(cfg);
        amount *= ((effective_confidence - min_conf) / (1.0 - min_conf).max(0.001)).clamp(0.5, 1.5);
    }
    if regime == "CHOP" || regime == "RANGE" {
        amount *= 0.5;
    }
    amount
}

trait TradeBackend {
    async fn execute(
        &self,
        bot: &TradingBot,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade>;
}

struct LiveBackend;

impl TradeBackend for LiveBackend {
    async fn execute(
        &self,
        bot: &TradingBot,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        bot.execute_live_trade(analysis, hail_mary, override_amount, strategy, origin)
            .await
    }
}

/// Constructed only after the execution actor has settled every maker quote.
struct HybridLiveBackend;

impl TradeBackend for HybridLiveBackend {
    async fn execute(
        &self,
        bot: &TradingBot,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        bot.execute_live_trade_inner(analysis, hail_mary, override_amount, strategy, true, origin)
            .await
    }
}

struct PaperBackend {
    mode_override: Option<&'static str>,
    source_override: Option<&'static str>,
}

struct SimulatedLiveBackend;

impl TradeBackend for PaperBackend {
    async fn execute(
        &self,
        bot: &TradingBot,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        bot.execute_paper_trade(
            analysis,
            hail_mary,
            override_amount,
            self.mode_override,
            self.source_override,
            strategy,
            origin,
        )
        .await
    }
}

impl TradeBackend for SimulatedLiveBackend {
    async fn execute(
        &self,
        bot: &TradingBot,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        bot.execute_simulated_live_trade(analysis, hail_mary, override_amount, strategy, origin)
            .await
    }
}

/// Adversarial market probing: analyze orderbook depth to detect aggressive sweeping.
///
/// Checks multiple indicators:
/// 1. Spread width — wide spread = thin/unstable book
/// 2. Depth ratio — asymmetric depth = one side being swept
/// 3. Top-of-book size — tiny resting orders = easily swept
/// 4. Price location — extreme prices have thinner books
///
/// Returns true if book is stable (safe to enter), false if aggressive sweeping detected.
pub fn probe_book_stability(
    yes_price: f64,
    best_bid: f64,
    best_ask: f64,
    bid_depth_usd: f64,
    ask_depth_usd: f64,
    top_bid_size: f64,
    top_ask_size: f64,
) -> (bool, f64) {
    // 1. Spread check — tight spread = competitive market making
    let spread = if best_ask > best_bid {
        best_ask - best_bid
    } else {
        1.0 // No valid book
    };
    let spread_ok = spread < 0.04; // 4 cents max acceptable spread
    let spread_score = (1.0 - spread / 0.06).clamp(0.0, 1.0);

    // 2. Depth ratio — significant asymmetry = one side being swept
    let total_depth = bid_depth_usd + ask_depth_usd;
    let depth_ratio = if total_depth > 0.0 {
        (bid_depth_usd - ask_depth_usd).abs() / total_depth
    } else {
        1.0
    };
    let depth_balanced = depth_ratio < 0.70; // < 70% on one side
    let depth_score = (1.0 - depth_ratio / 0.80).clamp(0.0, 1.0);

    // 3. Top-of-book size — too small = easily moved
    let min_top_size = 50.0; // $50 minimum at top of book
    let top_ok = top_bid_size >= min_top_size && top_ask_size >= min_top_size;
    let top_score = ((top_bid_size.min(top_ask_size)) / 200.0).clamp(0.0, 1.0);

    // 4. Price location — extreme prices have thinner books
    let location_ok = yes_price > 0.12 && yes_price < 0.88;
    let location_score = if yes_price > 0.20 && yes_price < 0.80 {
        1.0
    } else if location_ok {
        0.6
    } else {
        0.2
    };

    // 5. Minimum total depth
    let depth_adequate = total_depth > 500.0; // $500 total depth minimum
    let total_depth_score = (total_depth / 2000.0).clamp(0.0, 1.0);

    // Weighted stability score
    let stability = spread_score * 0.25
        + depth_score * 0.20
        + top_score * 0.20
        + location_score * 0.15
        + total_depth_score * 0.20;

    let is_stable = spread_ok && depth_balanced && top_ok && location_ok && depth_adequate;

    (is_stable, stability)
}

impl TradingBot {
    pub(crate) async fn wait_paper_order_latency(&self, milliseconds: u64) -> bool {
        tokio::select! {
            biased;
            ()=super::state::shutdown_begun(self.runtime_state.shutdown_watch())=>false,
            ()=sleep(Duration::from_millis(milliseconds))=>true,
        }
    }

    async fn persist_trade_rejection(
        &self,
        analysis: &ParsedAnalysis,
        signal: Signal,
        strategy: &str,
        rejection_reason: &str,
    ) {
        let run_id = self.run_id().to_string();
        if let Err(e) = db::append_trade_rejection(
            &self.db,
            &analysis.market.market_id,
            signal.as_str(),
            analysis.confidence,
            analysis.net_ev,
            rejection_reason,
            strategy,
            Some(run_id.as_str()),
        )
        .await
        {
            warn!(
                "failed to persist trade rejection for market {}: {e}",
                analysis.market.market_id
            );
        }
    }

    async fn execute_with_backend<B: TradeBackend>(
        &self,
        backend: &B,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        backend
            .execute(self, analysis, hail_mary, override_amount, strategy, origin)
            .await
    }

    // Both entry paths share this full validation context before any accounting.
    #[allow(clippy::too_many_arguments)]
    async fn validate_trade_common(
        &self,
        analysis: &ParsedAnalysis,
        cfg: &crate::config::Config,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        use_effective_balance: bool,
        path_label: &str,
    ) -> Option<ValidatedTradeInput> {
        let signal = analysis.signal;
        // Manual and forced entries share the same financial admission as
        // automatic work. Stop provenance never bypasses percentage loss caps.
        if self.is_drawdown_halted_for_config(cfg).await {
            crate::metrics::inc_trade_entry_block_drawdown();
            self.persist_trade_rejection(analysis, signal, strategy, "percentage drawdown halt")
                .await;
            warn!("{path_label}: percentage drawdown halt");
            return None;
        }

        let regime = analysis.regime.to_ascii_uppercase();
        if let Some(guard_reason) = buy_no_trend_up_guard_reason(cfg, analysis, &regime) {
            if cfg.buy_no_trend_up_guard_shadow_only {
                info!("{path_label}: shadow-only {guard_reason}");
            } else {
                warn!("{path_label}: {guard_reason}");
                return None;
            }
        }

        let market_price = analysis.market.price;
        let token_price = if signal == Signal::BuyNo {
            1.0 - market_price
        } else {
            market_price
        };

        // ── Kelly sizing ────────────────────────────────────────────────────
        let base = if hail_mary {
            cfg.hail_mary_stake
        } else {
            cfg.trade_amount
        };
        let effective_conf = analysis.confidence.min(cfg.live_confidence_cap);
        let mut amount = self
            .kelly_size(base, effective_conf, override_amount, signal.as_str())
            .await;

        amount = scale_entry_amount(
            cfg,
            &regime,
            effective_conf,
            override_amount.is_some(),
            amount,
        );

        let snapshot = self.trade_risk_snapshot(cfg, use_effective_balance).await;
        let time_remaining_secs =
            crate::bot::analyze::parse_utc(&analysis.market.end_date).map(|end_dt| {
                end_dt.signed_duration_since(Utc::now()).num_milliseconds() as f64 / 1000.0
            });
        let entry_hour_utc = Utc::now().hour();
        let _entry_session_utc = session_bucket(entry_hour_utc);
        let decision = match evaluate_trade_guardrails(
            cfg,
            &snapshot,
            &ProposedTrade {
                signal,
                strategy: strategy.to_string(),
                confidence: analysis.confidence,
                funding_rate: analysis.funding_rate,
                regime: regime.clone(),
                token_price,
                liquidity: analysis.market.liquidity,
                entry_hour_utc,
                time_remaining_secs,
                btc_change_pct: analysis.btc_change_pct,
                net_ev: analysis.net_ev,
                forced: analysis.forced,
                hail_mary,
                requested_amount: amount,
            },
        ) {
            Ok(decision) => decision,
            Err(reject) => {
                let reason = format!("{reject}");
                warn!("{path_label}: {reason}");
                self.persist_trade_rejection(analysis, signal, strategy, &reason)
                    .await;
                return None;
            }
        };
        if let Some(from) = decision.canary_capped_from {
            info!(
                "{path_label}: canary rollout cap — reduced from ${from:.2} to ${:.2}",
                decision.approved_amount
            );
        }
        if decision.exposure_adjusted_from.is_some() {
            info!(
                "{path_label}: exposure cap — reduced to ${:.2} (${:.2} already open)",
                decision.approved_amount, snapshot.current_exposure
            );
        }
        let amount = decision.approved_amount;
        let effective_conf = decision.effective_confidence;

        let signals_json = analysis
            .signals
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());

        Some(ValidatedTradeInput {
            signal,
            entry_price: token_price,
            market_id: analysis.market.market_id.clone(),
            question: analysis.market.question.clone(),
            end_date: analysis.market.end_date.clone(),
            liquidity: analysis.market.liquidity,
            effective_conf,
            raw_conf: analysis.raw_confidence.or(Some(analysis.confidence)),
            amount,
            reason: analysis.reason.clone(),
            signals_json,
        })
    }

    // ── Simulated live path (orderbook-driven) ─────────────────────────────

    async fn execute_simulated_live_trade(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        if !self.entry_origin_allowed(origin).await {
            return None;
        }

        if !self
            .ensure_paper_execution_origin("simulated taker entry")
            .await
        {
            return None;
        }
        let cfg = self.control_plane.config.read().await.clone();
        let calibration_version = cfg.calibration_version();
        let validated = self
            .validate_trade_common(
                analysis,
                &cfg,
                hail_mary,
                override_amount,
                strategy,
                false,
                "SimLive",
            )
            .await?;
        let signal = validated.signal;
        let market_id = validated.market_id;
        let question = validated.question;
        let end_date = validated.end_date;
        let liquidity = validated.liquidity;
        let effective_conf = validated.effective_conf;
        let raw_conf = validated.raw_conf;
        let amount = validated.amount;
        let reason = validated.reason;
        let signals_json = validated.signals_json;

        let token_id = match self.get_clob_token_id(&market_id, signal).await {
            Some(t) => t,
            None => {
                warn!("SimLive: could not resolve token_id for {market_id}");
                return None;
            }
        };

        let book = match fetch_public_order_book(&token_id).await {
            Ok(b) => b,
            Err(_) => {
                warn!("SimLive: order book fetch failed for {token_id}");
                return None;
            }
        };

        let (best_bid, best_ask) = book_best_bid_ask(&book);
        let spread = best_ask - best_bid;
        let total_usd = book_total_usd(&book);
        let ask_depth_usd = book_side_usd(&book, "asks");

        // ── Book stability check ──────────────────────────────────────────
        {
            let bids = extract_levels(&book, "bids");
            let asks = extract_levels(&book, "asks");
            let bid_depth: f64 = bids.iter().map(|(p, s)| p * s).sum();
            let ask_depth: f64 = asks.iter().map(|(p, s)| p * s).sum();
            let top_bid_size = bids.first().map(|(_, s)| *s).unwrap_or(0.0);
            let top_ask_size = asks.first().map(|(_, s)| *s).unwrap_or(0.0);
            let yes_price = analysis.market.price;
            let (is_stable, stability_score) = probe_book_stability(
                yes_price,
                best_bid,
                best_ask,
                bid_depth,
                ask_depth,
                top_bid_size,
                top_ask_size,
            );
            if !is_stable {
                if cfg.mode_kind() == BotMode::Simulation {
                    info!(
                        "SimLive: book unstable (score={stability_score:.2}) — allowing in simulation mode"
                    );
                } else {
                    info!("SimLive: book unstable (score={stability_score:.2}) — skipping trade");
                    return None;
                }
            }
        }

        // Scalp-specific: reject if spread too wide or depth too thin for scalping
        if strategy == "scalp" {
            if spread > 0.06 {
                info!("Scalp rejected: spread {spread:.4} > 0.06 max for scalps");
                return None;
            }
            if ask_depth_usd < 500.0 {
                info!("Scalp rejected: ask depth ${ask_depth_usd:.0} < $500 min for scalps");
                return None;
            }
            // Maker-only scalps: enter at best bid instead of taker price
            // This saves the taker fee (~2-4 cents per round trip)
            // In simulation, we model this as filling at best_bid + 0.001
            // (one tick above best bid to ensure priority)
            // The actual fill in live mode would use the existing limit order infrastructure
        }

        // ── Smart order routing ──────────────────────────────────────────
        {
            let order_decision = self
                .strategy_services
                .smart_order_router
                .route_order(spread, total_usd, amount);
            let order_type_str = match &order_decision {
                crate::bot::smart_order::OrderType::Limit { .. } => "limit",
                crate::bot::smart_order::OrderType::Market => "market",
            };
            info!(
                "Smart order suggestion (advisory): {} order (spread={:.4}, depth=${:.0})",
                order_type_str, spread, total_usd
            );

            // For large orders, log iceberg split plan
            let chunks = self
                .strategy_services
                .smart_order_router
                .iceberg_split(amount);
            if chunks.len() > 1 {
                info!(
                    "Smart order suggestion (advisory): iceberg split into {} chunks of ${:.2} each",
                    chunks.len(),
                    chunks.first().unwrap_or(&0.0)
                );
            }
        }

        // Fetch midpoint from API (always available even on thin books)
        // One snapshot, one truth: the book already fetched is the source of its
        // own midpoint. `/midpoint` is a second call at a second instant, and on a
        // fast market the two disagree; see `book_mid`. It remains the fallback
        // for a one-sided book, where a midpoint against a defaulted 0.50 is not one.
        let mid = match crate::json_util::book_mid(&book) {
            Some(m) => m,
            None => fetch_public_midpoint(&token_id)
                .await
                .unwrap_or_else(|_| (best_bid + best_ask) / 2.0),
        };

        // NOTE: `mid` is already the midpoint for the CORRECT token.
        // get_clob_token_id returns YES token for BuyYes, NO token for BuyNo.
        // So `mid` is the YES midpoint for BuyYes, NO midpoint for BuyNo.
        // Do NOT compute `1.0 - mid` — that would invert the price.

        if spread > cfg.max_spread {
            let reason = format!(
                "SimLive thin book: spread {spread:.4} > max {:.4}",
                cfg.max_spread
            );
            self.persist_trade_rejection(analysis, signal, strategy, &reason)
                .await;
            return None;
        }
        if ask_depth_usd < cfg.min_depth_usd {
            let reason = format!(
                "SimLive thin book: ask depth ${ask_depth_usd:.2} < min ${:.2}",
                cfg.min_depth_usd
            );
            self.persist_trade_rejection(analysis, signal, strategy, &reason)
                .await;
            return None;
        }

        // Normal book: a taker crosses the spread, paying up to `max_slippage`
        // above mid. The FOK arrival model walks reachable asks and requires
        // the entire requested quantity at their volume-weighted price; see
        // `taker_limit_price` for why the limit is no longer below mid.
        let limit_price = taker_limit_price(mid, cfg.max_slippage);

        let requested_shares = amount / limit_price;
        if !self
            .wait_paper_order_latency(cfg.sim_order_latency_ms)
            .await
        {
            return None;
        }
        let arrival_book = match fetch_public_order_book(&token_id).await {
            Ok(book) => book,
            Err(error) => {
                warn!("SimLive arrival book unavailable: {error}");
                return None;
            }
        };
        let book_snapshot = PublicBookSnapshot::from_order_book(
            &token_id,
            &arrival_book,
            crate::json_util::book_mid(&arrival_book),
            "clob_rest_arrival_book",
        );
        let (fill_price, filled_size) = match super::simulation::taker_arrival_fill_with_policy(
            &arrival_book,
            limit_price,
            requested_shares,
            cfg.sim_available_depth_fraction,
            super::simulation::TakerPaperFillPolicy::FillOrKill,
        ) {
            Some((avg, filled)) => (avg, filled),
            None => {
                warn!("SimLive: no complete FOK fill available at limit {limit_price:.4}");
                return None;
            }
        };
        let executed_amount = fill_price * filled_size;
        if executed_amount <= 0.0 || !executed_amount.is_finite() {
            warn!("SimLive: invalid executed amount {executed_amount:.6}");
            return None;
        }
        if crate::bot::risk::is_below_min_trade_usd(executed_amount, cfg.min_trade_usd) {
            let reason = format!(
                "SimLive filled notional too small: executed ${executed_amount:.2} < min ${:.2}",
                cfg.min_trade_usd
            );
            self.persist_trade_rejection(analysis, signal, strategy, &reason)
                .await;
            return None;
        }

        let taker_fee = crate::fusion::estimate_taker_fee(fill_price);
        let lmsr_slip =
            crate::signals::lmsr::estimate_lmsr_slippage(executed_amount, fill_price, liquidity);
        let fee_cost = (taker_fee + lmsr_slip) * executed_amount;
        let net_ev =
            entry_return_at_price(analysis, analysis.signal, fill_price, taker_fee + lmsr_slip)?;
        let required_ev = cfg.min_ev + cfg.edge_safety_buffer + cfg.trade_gas_usd / executed_amount;
        if net_ev < required_ev {
            self.persist_trade_rejection(
                analysis,
                signal,
                strategy,
                &format!(
                    "arrival fill expected return {net_ev:.4} below required {required_ev:.4}"
                ),
            )
            .await;
            return None;
        }
        let total_debit = executed_amount + fee_cost;
        let bal = self.balance().await;
        if bal < total_debit {
            warn!("SimLive: insufficient balance ${bal:.2} for amount+fees ${total_debit:.2}");
            return None;
        }

        let order_id = Some(format!("sim-{:032x}", rand::random::<u128>()));
        let now = Utc::now().to_rfc3339();
        let mode = if cfg.shadow_trading || cfg.mode_kind() == BotMode::Shadow {
            BotMode::Shadow.as_str().to_string()
        } else {
            cfg.mode.clone()
        };
        let source = if cfg.shadow_trading || cfg.mode_kind() == BotMode::Shadow {
            "shadow".to_string()
        } else {
            "bot".to_string()
        };

        let trade = Trade {
            id: 0,
            timestamp: now.clone(),
            market_id: market_id.clone(),
            question: question.clone(),
            direction: Direction::from(signal).as_str().to_string(),
            entry_price: fill_price,
            amount: executed_amount,
            shares: filled_size,
            expected_value: net_ev,
            reason,
            confidence: effective_conf,
            status: TradeStatus::Open.as_str().to_string(),
            mode,
            source,
            end_date: end_date.clone(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: raw_conf,
            order_id: order_id.clone(),
            token_id: Some(token_id.clone()),
            fok_failed: Some(0),
            fee_paid: fee_cost,
            accounting_version: 0,
            signals_json,
            calibration_version: Some(calibration_version),
            run_id: Some(self.control_plane.run_id.clone()),
            strategy: strategy.to_string(),
        };

        if self.entries_frozen() {
            return None;
        }
        let price_evidence = SimTradePriceEvidence::new(
            "entry",
            "sim_taker_orderbook",
            "buy",
            limit_price,
            fill_price,
            filled_size,
            "simulated orderbook fill",
            book_snapshot.clone(),
        );
        let mut details = serde_json::to_value(price_evidence).ok()?;
        details["paper_execution"] = serde_json::json!({
            "fill_policy": super::simulation::TakerPaperFillPolicy::FillOrKill,
            "latency_ms": cfg.sim_order_latency_ms,
            "available_depth_fraction": cfg.sim_available_depth_fraction,
        });
        let details = details.to_string();
        let evidence = db::paper::PaperExecutionEvidence {
            order_id: order_id.as_deref()?,
            phase: "entry",
            intended_price: limit_price,
            avg_price: fill_price,
            shares: filled_size,
            source: "execute_simulated_live_trade",
            details: &details,
        };
        // A config change while fetching the book must not let a paper fill
        // publish into live cash. Keep the current-origin lease through the
        // durable lot, cash and hot-position publication.
        let origin_config = self.control_plane.config.read().await;
        if !self.entry_origin_allowed_in_config(origin, &origin_config) {
            return None;
        }
        if self.clob.is_some()
            || !matches!(
                origin_config.mode_kind(),
                BotMode::Paper | BotMode::Shadow | BotMode::Simulation
            )
        {
            self.reject_paper_execution_origin("simulated taker entry")
                .await;
            return None;
        }
        let trade = match db::paper::insert_taker_entry(&self.db, &trade, &evidence).await {
            Ok(trade) => trade,
            Err(error) => {
                metrics::inc_db_insert_fail();
                self.set_degraded_no_trade(true);
                self.halt_trading("paper_taker_entry_persistence", &error.to_string())
                    .await;
                warn!("simulated entry lot and evidence were not committed: {error}");
                return None;
            }
        };
        // Publish only after the lot and its execution evidence both persist.
        self.set_balance((bal - total_debit).max(0.0)).await;
        self.portfolio_state
            .trades
            .write()
            .await
            .push(trade.clone());
        drop(origin_config);

        metrics::inc_trades_opened();
        // Track slippage: difference between midpoint and actual fill
        let slippage_bps = if mid > 0.0 {
            ((fill_price - mid) / mid * 10_000.0).round()
        } else {
            0.0
        };
        info!(
            "SimLive trade opened: {} {} ${executed_amount:.2} @ {fill_price:.4} mid={mid:.4} slip={slippage_bps:.0}bps conf={effective_conf:.2}",
            signal.as_str(),
            market_id,
        );

        Some(trade)
    }

    // ── Public entry point ────────────────────────────────────────────────────

    /// Route to the appropriate execution path based on mode.
    /// Returns the newly created Trade, or None if execution was skipped.
    pub async fn execute_trade(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
    ) -> Option<Trade> {
        self.execute_trade_strategy(analysis, hail_mary, override_amount, "conviction")
            .await
    }

    pub async fn execute_trade_strategy(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
    ) -> Option<Trade> {
        self.execute_trade_strategy_for_origin(
            analysis,
            hail_mary,
            override_amount,
            strategy,
            ExecutionOrigin::Manual,
        )
        .await
    }

    pub(super) async fn execute_trade_strategy_for_origin(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        self.execute_trade_strategy_inner(
            analysis,
            hail_mary,
            override_amount,
            strategy,
            false,
            origin,
        )
        .await
    }

    pub(super) async fn execute_trade_after_hybrid_handoff(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        let cfg = self.control_plane.config.read().await.clone();
        if cfg.mode_kind() != BotMode::Live || cfg.shadow_trading || cfg.execution_mode != "hybrid"
        {
            return None;
        }
        self.execute_trade_strategy_inner(
            analysis,
            hail_mary,
            override_amount,
            strategy,
            true,
            origin,
        )
        .await
    }

    async fn execute_trade_strategy_inner(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        hybrid_handoff: bool,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        if !self.entry_origin_allowed(origin).await {
            return None;
        }

        if self.kill_switch_active() {
            warn!("Kill switch active — skipping trade execution");
            return None;
        }
        if self.degraded_no_trade_active() {
            warn!("Degraded no-trade active — skipping trade execution");
            return None;
        }

        let signal = analysis.signal;
        if signal == Signal::Hold {
            return None;
        }

        let market_id = analysis.market.market_id.as_str();

        let forced = analysis.forced;
        let cfg = self.control_plane.config.read().await.clone();
        if (cfg.mode_kind() != BotMode::Live || cfg.shadow_trading)
            && !self
                .ensure_paper_execution_origin("paper or shadow entry")
                .await
        {
            return None;
        }
        if !forced && self.is_analysis_stale().await {
            warn!(
                "Analysis is stale (age={}s > {}s) — skipping",
                self.analysis_age_secs().await,
                cfg.analysis_stale_seconds
            );
            return None;
        }

        // Guard: no duplicate open position on the same market *for the same strategy*
        {
            let trades = self.portfolio_state.trades.read().await;
            let yes_price = analysis.market.price;
            let token_price = if signal == Signal::BuyNo {
                1.0 - yes_price
            } else {
                yes_price
            };
            let snapshot = EntryCoordinationSnapshot {
                has_same_market_strategy_position: trades
                    .iter()
                    .any(|t| t.market_id == market_id && t.is_open() && t.strategy == strategy),
                open_scalps: trades
                    .iter()
                    .filter(|t| t.is_open() && t.strategy == "scalp")
                    .count() as u32,
                open_conviction: trades
                    .iter()
                    .filter(|t| t.is_open() && t.strategy != "scalp")
                    .count() as u32,
            };
            let proposal = EntryCoordinationProposal {
                signal,
                strategy: strategy.to_string(),
                market_id: market_id.to_string(),
                market_yes_price: yes_price,
                token_price,
                p_up: analysis.p_up,
                liquidity: analysis.market.liquidity,
            };
            if let Err(reject) = evaluate_entry_coordination(&cfg, &snapshot, &proposal) {
                let reason = format!("{reject}");
                info!("{reason}");
                self.persist_trade_rejection(analysis, signal, strategy, &reason)
                    .await;
                return None;
            }
        }

        let mode = cfg.mode_kind();
        let shadow_exec = cfg.shadow_trading || mode == BotMode::Shadow;
        let sim_live = (cfg.sim_live_execution || (cfg.shadow_live_parity && shadow_exec))
            && mode != BotMode::Live;
        if mode == BotMode::Live && !shadow_exec {
            if hybrid_handoff {
                self.execute_with_backend(
                    &HybridLiveBackend,
                    analysis,
                    hail_mary,
                    override_amount,
                    strategy,
                    origin,
                )
                .await
            } else {
                self.execute_with_backend(
                    &LiveBackend,
                    analysis,
                    hail_mary,
                    override_amount,
                    strategy,
                    origin,
                )
                .await
            }
        } else if sim_live {
            self.execute_with_backend(
                &SimulatedLiveBackend,
                analysis,
                hail_mary,
                override_amount,
                strategy,
                origin,
            )
            .await
        } else if shadow_exec {
            self.execute_with_backend(
                &PaperBackend {
                    mode_override: Some("shadow"),
                    source_override: Some("shadow"),
                },
                analysis,
                hail_mary,
                override_amount,
                strategy,
                origin,
            )
            .await
        } else {
            self.execute_with_backend(
                &PaperBackend {
                    mode_override: None,
                    source_override: None,
                },
                analysis,
                hail_mary,
                override_amount,
                strategy,
                origin,
            )
            .await
        }
    }

    // ── Live path ─────────────────────────────────────────────────────────────

    async fn execute_live_trade(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        self.execute_live_trade_inner(
            analysis,
            hail_mary,
            override_amount,
            strategy,
            false,
            origin,
        )
        .await
    }

    async fn execute_live_trade_inner(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        strategy: &str,
        hybrid_handoff: bool,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        if !self.entry_origin_allowed(origin).await {
            return None;
        }

        // Serialize live executions to prevent TOCTOU race on balance check.
        // The guard is held from validation through order placement + DB insert.
        let _exec_guard = self.execution_state.live_execution_lock.lock().await;

        let cfg = self.control_plane.config.read().await.clone();
        if cfg.execution_mode != "taker" && !(cfg.execution_mode == "hybrid" && hybrid_handoff) {
            warn!("Direct live entries require taker mode; hybrid requires the execution actor's settled maker handoff");
            return None;
        }
        if hybrid_handoff
            && db::maker::pending_orders(&self.db)
                .await
                .map_or(true, |orders| !orders.is_empty())
        {
            self.set_degraded_no_trade(true);
            self.halt_trading(
                "hybrid_taker_handoff",
                "maker quotes or settlement remain unresolved before a hybrid entry",
            )
            .await;
            return None;
        }
        if self.require_reconciled_live_exits().await.is_err() {
            return None;
        }
        let calibration_version = cfg.calibration_version();
        let validated = self
            .validate_trade_common(
                analysis,
                &cfg,
                hail_mary,
                override_amount,
                strategy,
                true,
                "Live",
            )
            .await?;
        let signal = validated.signal;
        let market_id = validated.market_id;
        let question = validated.question;
        let end_date = validated.end_date;
        let _liquidity = validated.liquidity;
        let effective_conf = validated.effective_conf;
        let raw_conf = validated.raw_conf;
        let amount = validated.amount;
        let reason = validated.reason;
        let signals_json = validated.signals_json;

        // ── Resolve token_id ──────────────────────────────────────────────────
        let token_id = match self.get_clob_token_id(&market_id, signal).await {
            Some(t) => t,
            None => {
                warn!("Could not resolve token_id for {market_id}");
                return None;
            }
        };

        // ── Truth check: sync balance ─────────────────────────────────────────
        if self.sync_balance().await.is_none() {
            let err = format!(
                "live balance sync failed before entry market_id={} signal={}",
                market_id,
                signal.as_str()
            );
            warn!("{err}");
            self.set_degraded_no_trade(true);
            self.halt_trading("live_balance_sync_before_entry", err.clone())
                .await;
            self.record_reconcile_error("live_balance_sync_before_entry", err)
                .await;
            return None;
        }
        let eff_bal = self.effective_balance().await;
        let locked: f64 = self
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|t| t.is_open() && t.is_live() && t.has_order())
            .map(|t| t.locked_collateral())
            .sum();
        info!("Balance check: effective=${eff_bal:.2} locked=${locked:.2}");
        if eff_bal < amount {
            warn!("Insufficient effective balance ${eff_bal:.2} for trade ${amount:.2}");
            return None;
        }

        // ── Spread / depth check ──────────────────────────────────────────────
        let clob = self.clob.as_ref()?;
        if let Err(error) = clob.check_settlement_rpc().await {
            self.set_degraded_no_trade(true);
            self.halt_trading("live_settlement_rpc_not_ready", error.to_string())
                .await;
            return None;
        }

        let book = match clob.get_order_book(&token_id).await {
            Ok(book) => {
                let (best_bid, best_ask) = book_best_bid_ask(&book);
                let spread = best_ask - best_bid;
                if spread > cfg.max_spread {
                    warn!("Spread {spread:.4} > max {}", cfg.max_spread);
                    return None;
                }

                let total_usd = book_total_usd(&book);
                let ask_depth_usd = book_side_usd(&book, "asks");

                if ask_depth_usd < cfg.min_depth_usd {
                    warn!("Ask depth ${ask_depth_usd:.2} < min ${}", cfg.min_depth_usd);
                    return None;
                }

                // ── Smart order routing (live) ───────────────────────────────
                {
                    let order_decision = self.strategy_services.smart_order_router.route_order(
                        spread,
                        ask_depth_usd,
                        amount,
                    );
                    let order_type_str = match &order_decision {
                        crate::bot::smart_order::OrderType::Limit { .. } => "limit",
                        crate::bot::smart_order::OrderType::Market => "market",
                    };
                    info!(
                        "Smart order suggestion (advisory): {} order (spread={:.4}, depth=${:.0})",
                        order_type_str, spread, total_usd
                    );

                    let chunks = self
                        .strategy_services
                        .smart_order_router
                        .iceberg_split(amount);
                    if chunks.len() > 1 {
                        info!(
                            "Smart order suggestion (advisory): iceberg split into {} chunks of ${:.2} each",
                            chunks.len(),
                            chunks.first().unwrap_or(&0.0)
                        );
                    }
                }
                book
            }
            Err(e) => {
                warn!("Order book check failed: {e}");
                return None;
            }
        };

        let limit_price = live_taker_limit_price(&book, cfg.max_slippage)?;
        let side = Side::Buy;

        // Reserve the worst point of the market fee curve inside the budget.
        let fees = match clob.market_fee_parameters(&token_id).await {
            Ok(fees) => fees,
            Err(error) => {
                warn!("Cannot establish live fee policy: {error}");
                return None;
            }
        };
        let per_share_fee_cap = fees.rate * 0.25_f64.powf(fees.exponent);
        let net_ev = entry_return_at_price(
            analysis,
            analysis.signal,
            limit_price,
            per_share_fee_cap / limit_price,
        )?;
        let required_ev = cfg.min_ev + cfg.edge_safety_buffer + cfg.trade_gas_usd / amount;
        if net_ev < required_ev {
            self.persist_trade_rejection(
                analysis,
                signal,
                strategy,
                &format!(
                    "signed-limit expected return {net_ev:.4} below required {required_ev:.4}"
                ),
            )
            .await;
            return None;
        }
        let intended_shares = (amount / (limit_price + per_share_fee_cap) - 0.01).max(0.0);
        let signed = match clob
            .create_order(&LimitOrderArgs {
                token_id: token_id.clone(),
                price: limit_price,
                size: intended_shares,
                side,
            })
            .await
        {
            Ok(order) => order,
            Err(error) => {
                warn!("Cannot sign live entry: {error}");
                return None;
            }
        };
        let order_id = signed.order_id().to_string();
        let mut trade = Trade {
            id: 0,
            timestamp: Utc::now().to_rfc3339(),
            market_id: market_id.clone(),
            question,
            direction: Direction::from(signal).as_str().to_string(),
            entry_price: limit_price,
            amount,
            shares: 0.0,
            expected_value: net_ev,
            reason,
            confidence: effective_conf,
            status: TradeStatus::Open.as_str().to_string(),
            mode: BotMode::Live.as_str().to_string(),
            source: "bot".to_string(),
            end_date,
            raw_confidence: raw_conf,
            order_id: Some(order_id.clone()),
            token_id: Some(token_id.clone()),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json,
            calibration_version: Some(calibration_version),
            run_id: Some(self.control_plane.run_id.clone()),
            strategy: strategy.to_string(),
            ..Trade::default()
        };
        if self.entries_frozen() || !self.entry_origin_allowed(origin).await {
            return None;
        }
        trade.id = match db::insert_trade(&self.db, &trade).await {
            Ok(id) => id,
            Err(error) => {
                warn!("Cannot persist entry before submission: {error}");
                return None;
            }
        };
        if let Err(error) = db::live::prepare_order(
            &self.db,
            trade.id,
            &clob.account_address(),
            &signed,
            limit_price,
            fees,
            "entry",
        )
        .await
        {
            // No network submission occurred. Keep the row visible for repair.
            self.set_degraded_no_trade(true);
            self.halt_trading("live_entry_intent_persistence_failed", error.to_string())
                .await;
            return None;
        }
        self.publish_live_trade_state(trade.clone()).await;
        if self.entries_frozen() || !self.entry_origin_allowed(origin).await {
            match db::live::reject_order(&self.db, &order_id).await {
                Ok(updated) => self.publish_live_trade_state(updated).await,
                Err(error) => {
                    self.set_degraded_no_trade(true);
                    self.halt_trading("live_entry_cancelled_before_submission", error.to_string())
                        .await;
                }
            }
            return None;
        }
        // A taker entry is FOK. Never leave the unfilled remainder as a maker,
        // and never retry an ambiguous POST with a fresh order identity.
        match clob.post_order(&signed, OrderType::FOK).await {
            Ok(response) if response.success == Some(false) => {
                metrics::inc_clob_order_reject();
                match db::live::reject_order(&self.db, &order_id).await {
                    Ok(updated) => self.publish_live_trade_state(updated).await,
                    Err(error) => {
                        self.set_degraded_no_trade(true);
                        self.halt_trading(
                            "live_entry_rejection_persistence_failed",
                            error.to_string(),
                        )
                        .await;
                    }
                }
                return None;
            }
            Ok(response) if response.order_id != order_id => {
                self.set_degraded_no_trade(true);
                self.halt_trading(
                    "live_order_identity_mismatch",
                    "venue response differs from the order identity saved before submission",
                )
                .await;
                return None;
            }
            Ok(_) => {}
            Err(error) => {
                self.set_degraded_no_trade(true);
                self.halt_trading("live_entry_submission_uncertain", error.to_string())
                    .await;
            }
        }
        self.recover_live_orders_or_halt().await;
        let pending = db::live::pending_orders(&self.db).await;
        if !matches!(pending, Ok(ref rows) if rows.is_empty()) {
            self.set_degraded_no_trade(true);
            self.halt_trading(
                "live_entry_settlement_pending",
                "entry is saved; confirmed exchange matches are required before further trading",
            )
            .await;
        }
        self.sync_balance().await;
        let updated = db::load_trade_by_id(&self.db, trade.id)
            .await
            .ok()
            .flatten()?;
        if updated.is_open() {
            Some(updated)
        } else {
            None
        }
    }

    // ── Paper path ────────────────────────────────────────────────────────────

    // This backend binds the two legacy provenance overrides and explicit dispatch origin.
    #[allow(clippy::too_many_arguments)]
    async fn execute_paper_trade(
        &self,
        analysis: &ParsedAnalysis,
        hail_mary: bool,
        override_amount: Option<f64>,
        mode_override: Option<&str>,
        source_override: Option<&str>,
        strategy: &str,
        origin: ExecutionOrigin,
    ) -> Option<Trade> {
        if !self.entry_origin_allowed(origin).await {
            return None;
        }

        // Acquire the execution lock to prevent concurrent paper trades from
        // racing on the balance check (same TOCTOU pattern as the live path).
        let _exec_guard = self.execution_state.live_execution_lock.lock().await;

        if !self.ensure_paper_execution_origin("paper entry").await {
            return None;
        }
        let cfg = self.control_plane.config.read().await.clone();
        let calibration_version = cfg.calibration_version();
        let validated = self
            .validate_trade_common(
                analysis,
                &cfg,
                hail_mary,
                override_amount,
                strategy,
                false,
                "Paper",
            )
            .await?;
        let signal = validated.signal;
        let entry_price = validated.entry_price;
        let market_id = validated.market_id;
        let question = validated.question;
        let end_date = validated.end_date;
        let effective_conf = validated.effective_conf;
        let raw_conf = validated.raw_conf;
        let amount = validated.amount;
        let reason = validated.reason;
        let signals_json = validated.signals_json;

        // Balance check remains local to paper path because entry fees are also deducted here.
        let bal = self.balance().await;

        // ── Fees + slippage (mirror live cost structure) ─────────────────────
        // The fee exits your wallet at entry (Polymarket charges it upfront).
        // Deduct it from balance on top of the trade amount. Keep full shares so
        // the PnL formula stays correct for both WIN and LOSE at resolution.
        let liquidity = validated.liquidity.max(1000.0);
        let taker_fee = crate::fusion::estimate_taker_fee(entry_price);
        let lmsr_slip =
            crate::signals::lmsr::estimate_lmsr_slippage(amount, entry_price, liquidity);
        let fee_cost = (taker_fee + lmsr_slip) * amount;
        let net_ev = entry_return_at_price(
            analysis,
            analysis.signal,
            entry_price,
            taker_fee + lmsr_slip,
        )?;
        let required_ev = cfg.min_ev + cfg.edge_safety_buffer + cfg.trade_gas_usd / amount;
        if net_ev < required_ev {
            self.persist_trade_rejection(
                analysis,
                signal,
                strategy,
                &format!("paper entry expected return {net_ev:.4} below required {required_ev:.4}"),
            )
            .await;
            return None;
        }
        let shares = amount / entry_price; // full gross shares, fee charged separately
        let total_debit = amount + fee_cost;
        if bal < total_debit {
            warn!(
                "Paper: insufficient balance ${bal:.2} for amount+fees ${total_debit:.2} \
                 (amount=${amount:.2}, fees=${fee_cost:.2})"
            );
            return None;
        }

        let now = Utc::now().to_rfc3339();
        let mode = mode_override.unwrap_or(&cfg.mode).to_string();
        let source = source_override.unwrap_or("bot").to_string();

        let trade = Trade {
            id: 0,
            timestamp: now.clone(),
            market_id: market_id.clone(),
            question: question.clone(),
            direction: Direction::from(signal).as_str().to_string(),
            entry_price,
            amount,
            shares,
            expected_value: net_ev,
            reason,
            confidence: effective_conf,
            status: TradeStatus::Open.as_str().to_string(),
            mode,
            source,
            end_date: end_date.clone(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: raw_conf,
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid: fee_cost,
            accounting_version: 0,
            signals_json,
            calibration_version: Some(calibration_version),
            run_id: Some(self.control_plane.run_id.clone()),
            strategy: strategy.to_string(),
        };

        if self.entries_frozen() {
            return None;
        }
        let origin_config = self.control_plane.config.read().await;
        if !self.entry_origin_allowed_in_config(origin, &origin_config) {
            return None;
        }
        if self.clob.is_some()
            || !matches!(
                origin_config.mode_kind(),
                BotMode::Paper | BotMode::Shadow | BotMode::Simulation
            )
        {
            self.reject_paper_execution_origin("paper entry").await;
            return None;
        }
        let inserted_id = match db::insert_trade(&self.db, &trade).await {
            Ok(id) => id,
            Err(e) => {
                metrics::inc_db_insert_fail();
                warn!("paper insert_trade failed: {e}");
                return None;
            }
        };

        // Deduct amount + entry fees (fee exits wallet upfront, never returned)
        let new_bal = (bal - total_debit).max(0.0);
        self.set_balance(new_bal).await;

        let mut trade = trade;
        trade.id = inserted_id;
        self.portfolio_state
            .trades
            .write()
            .await
            .push(trade.clone());
        drop(origin_config);
        metrics::inc_trades_opened();
        let sim_label = if trade.mode_kind() == BotMode::Shadow {
            "Shadow"
        } else {
            "Paper"
        };
        info!(
            "{sim_label} trade opened: {} {} ${amount:.2} @ {entry_price:.4} conf={effective_conf:.2} fee=${fee_cost:.3} (taker={:.2}% slip={:.2}%)",
            signal.as_str(), market_id, taker_fee * 100.0, lmsr_slip * 100.0,
        );

        Some(trade)
    }

    // ── Live order fill monitor ───────────────────────────────────────────────

    /// Recover durable taker intents using confirmed account trades and
    /// finalized receipts. Legacy active orders require explicit migration;
    /// an order-status limit price never supplies execution VWAP.
    pub async fn poll_open_live_orders(&self) {
        self.recover_live_orders_or_halt().await;
        // Older pending GTC entries have no durable fee/identity policy. They
        // must be explicitly migrated, rather than priced at their limit.
        let targets: Vec<_> = self
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|trade| trade.is_live() && trade.is_open() && trade.has_order())
            .cloned()
            .collect();
        for trade in targets {
            let known: Result<i64, _> =
                sqlx::query_scalar("SELECT COUNT(*) FROM live_orders WHERE order_id=?")
                    .bind(trade.order_id.as_deref())
                    .fetch_one(&self.db)
                    .await;
            if !matches!(known,Ok(n) if n>0) {
                self.set_degraded_no_trade(true);
                self.halt_trading(
                    "legacy_live_order_requires_migration",
                    format!("trade {} predates confirmed-match accounting", trade.id),
                )
                .await;
            }
        }
    }

    // ── Token ID helper ───────────────────────────────────────────────────────

    /// Fetch YES/UP or NO/DOWN token_id from clob.polymarket.com/markets/{id},
    /// using a 60-second cache to avoid redundant HTTP calls.
    pub async fn get_clob_token_id(&self, market_id: &str, signal: Signal) -> Option<String> {
        use std::time::{Duration, Instant};
        const TTL: Duration = Duration::from_secs(60);

        let want_yes = signal == Signal::BuyYes;

        // ── Check cache first ────────────────────────────────────────────────
        let cache = if want_yes {
            &self.market_state.up_token_cache
        } else {
            &self.market_state.no_token_cache
        };
        if let Some(entry) = cache.get(market_id) {
            let (fetched_at, token_id) = entry.value().clone();
            if fetched_at.elapsed() < TTL {
                return Some(token_id);
            }
        }

        // ── Fetch from CLOB ─────────────────────────────────────────────────
        let base = std::env::var("MACBOT_CLOB_BASE")
            .unwrap_or_else(|_| "https://clob.polymarket.com".into());
        let url = format!("{}/markets/{market_id}", base.trim_end_matches('/'));
        let resp: serde_json::Value = match HTTP.get(&url).send().await {
            Ok(r) => r.bounded_json().await.ok()?,
            Err(e) => {
                warn!("get_clob_token_id fetch error: {e}");
                return None;
            }
        };

        // Parse and cache both YES and NO tokens from the single response.
        let mut yes_token: Option<String> = None;
        let mut no_token: Option<String> = None;
        if let Some(arr) = resp.get("tokens").and_then(|v| v.as_array()) {
            for t in arr {
                let outcome = t
                    .get("outcome")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase();
                let tid = t
                    .get("token_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(ref tid) = tid {
                    if outcome.contains("YES") || outcome.contains("UP") {
                        yes_token = Some(tid.clone());
                    } else if outcome.contains("NO") || outcome.contains("DOWN") {
                        no_token = Some(tid.clone());
                    }
                }
            }
        }

        let now = Instant::now();
        if let Some(ref tid) = yes_token {
            self.market_state
                .up_token_cache
                .insert(market_id.to_string(), (now, tid.clone()));
        }
        if let Some(ref tid) = no_token {
            self.market_state
                .no_token_cache
                .insert(market_id.to_string(), (now, tid.clone()));
        }

        if want_yes {
            yes_token
        } else {
            no_token
        }
    }

    /// Calculate hedge size for an existing position.
    /// When holding YES, place a small NO as insurance.
    /// Returns the hedge amount in USD (0 = no hedge needed).
    pub fn calculate_hedge_amount(&self, position_amount: f64, confidence: f64) -> f64 {
        // Only hedge if confidence is moderate (not ultra-high conviction)
        // and position is large enough to warrant it
        let min_hedge_position = 10.0; // Min $10 position to hedge
        if position_amount < min_hedge_position {
            return 0.0;
        }

        // Hedge 5-15% of position size, scaled inversely with confidence
        // High confidence (0.90) → small hedge (5%)
        // Moderate confidence (0.65) → larger hedge (15%)
        let hedge_pct = (1.0 - confidence.clamp(0.5, 0.95)) * 0.30;
        let hedge_pct = hedge_pct.clamp(0.05, 0.15);

        position_amount * hedge_pct
    }
}

impl From<Signal> for Direction {
    fn from(value: Signal) -> Self {
        match value {
            Signal::BuyYes => Direction::BuyYes,
            Signal::BuyNo => Direction::BuyNo,
            Signal::Hold => Direction::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_polled_entry_fill_update, book_best_bid_ask, book_total_usd,
        buy_no_trend_up_guard_reason, entry_return_at_price, probe_book_stability,
        simulate_buy_fill, simulate_buy_fill_core, taker_limit_price,
    };
    use crate::analysis::{AnalysisMarketData, AnalysisResult};
    use crate::bot::actor::ExecutionOrigin;
    use crate::config::Config;
    use crate::db::Trade;
    use crate::domain::Signal;
    use serde_json::json;

    async fn paper_entry_fixture() -> crate::bot::state::TradingBot {
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.mode = "simulation".into();
        cfg.initial_balance = 100.0;
        cfg.shadow_trading = false;
        cfg.sim_live_execution = false;
        cfg.shadow_live_parity = false;
        cfg.macro_event_guard_enabled = false;
        crate::bot::state::TradingBot::new(cfg, crate::db::open(":memory:").await.unwrap())
            .await
            .unwrap()
    }

    fn paper_entry_analysis() -> AnalysisResult {
        AnalysisResult {
            signal: Signal::BuyYes,
            confidence: 0.95,
            p_up: Some(0.95),
            net_ev: 0.8,
            forced: true,
            market: AnalysisMarketData {
                market_id: "paper-origin-entry".into(),
                price: 0.5,
                liquidity: 100_000.0,
                end_date: "2099-01-01T00:00:00Z".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn paper_entry_backends_reject_live_shadow_and_boot_wallet_origin_without_writes(
    ) -> anyhow::Result<()> {
        for (mode, wallet_backed) in [("live", false), ("simulation", true), ("unknown", false)] {
            let mut bot = paper_entry_fixture().await;
            {
                let mut cfg = bot.control_plane.config.write().await;
                cfg.mode = mode.into();
                cfg.shadow_trading = true;
                cfg.sim_live_execution = true;
            }
            if wallet_backed {
                let auth = crate::clob::ClobAuth::new(
                    "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    "public-api-key",
                    "cHVibGljLWZpeHR1cmUtc2VjcmV0",
                    "public-passphrase",
                )?;
                // Inert public fixture: no request may reach this client.
                bot.clob = Some(std::sync::Arc::new(
                    crate::clob::ClobClient::with_test_host(auth, "http://127.0.0.1:9".into()),
                ));
            }
            let analysis = paper_entry_analysis();
            let before = bot.balance().await;
            for backend in ["router", "paper", "simulated_taker"] {
                // A preceding refusal must not let a generic stop mask a
                // missing origin guard in either directly callable backend.
                bot.set_kill_switch(false);
                bot.set_degraded_no_trade(false);
                let result = match backend {
                    "router" => {
                        bot.execute_trade_strategy(&analysis, false, Some(5.0), "conviction")
                            .await
                    }
                    "paper" => {
                        bot.execute_paper_trade(
                            &analysis,
                            false,
                            Some(5.0),
                            Some("shadow"),
                            Some("shadow"),
                            "conviction",
                            ExecutionOrigin::Manual,
                        )
                        .await
                    }
                    _ => {
                        bot.execute_simulated_live_trade(
                            &analysis,
                            false,
                            Some(5.0),
                            "conviction",
                            ExecutionOrigin::Manual,
                        )
                        .await
                    }
                };
                assert!(
                    result.is_none(),
                    "{backend} must refuse {mode}/{wallet_backed}"
                );
                assert_eq!(
                    bot.kill_switch_status().1.unwrap().source,
                    "paper_live_accounting_conflict",
                    "{backend} must establish its own refusal cause"
                );
                assert!(bot.degraded_no_trade_active());
            }
            assert_eq!(bot.balance().await, before);
            assert!(bot.trades().await.is_empty());
            let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
                "SELECT (SELECT COUNT(*) FROM trades),(SELECT COUNT(*) FROM trade_fills),(SELECT COUNT(*) FROM live_orders),(SELECT COUNT(*) FROM realized_pnl_events),(SELECT COUNT(*) FROM trade_rejections)",
            ).fetch_one(&bot.db).await?;
            assert_eq!(
                counts,
                (0, 0, 0, 0, 0),
                "{mode}/{wallet_backed} changed entry evidence"
            );
            assert!(bot.kill_switch_active());
            assert!(bot.degraded_no_trade_active());
            assert_eq!(
                bot.kill_switch_status().1.unwrap().source,
                "paper_live_accounting_conflict"
            );
            bot.db.close().await;
        }
        Ok(())
    }

    #[tokio::test]
    async fn isolated_paper_entry_still_persists_and_debits_its_own_cash() -> anyhow::Result<()> {
        let bot = paper_entry_fixture().await;
        let before = bot.balance().await;
        let opened = bot
            .execute_trade_strategy(&paper_entry_analysis(), false, Some(5.0), "conviction")
            .await
            .expect("valid isolated paper entry must remain available");
        assert_eq!(opened.mode, "simulation");
        assert!((bot.balance().await - (before - opened.amount - opened.fee_paid)).abs() < 1e-9);
        assert_eq!(bot.trades().await.len(), 1);
        assert_eq!(
            crate::db::load_trade_by_id(&bot.db, opened.id)
                .await?
                .unwrap()
                .amount,
            opened.amount
        );
        assert!(!bot.kill_switch_active());
        bot.db.close().await;
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn simulated_taker_entry_uses_fok_before_accounting_the_arrival_fill(
    ) -> anyhow::Result<()> {
        let _environment = crate::env_util::test_env_lock();
        for (displayed_ask_size, should_fill) in [(20.0, false), (200.0, true)] {
            let bot = paper_entry_fixture().await;
            {
                let mut cfg = bot.control_plane.config.write().await;
                cfg.sim_live_execution = true;
                cfg.execution_mode = "taker".into();
                cfg.sim_order_latency_ms = 0;
                cfg.sim_available_depth_fraction = 0.25;
                cfg.min_depth_usd = 0.0;
                cfg.min_trade_usd = 1.0;
            }
            let base = crate::bot::close::tests::serve_one_book(json!({
                "bids":[{"price":0.5,"size":200.0}],
                "asks":[{"price":0.51,"size":displayed_ask_size}]
            }))
            .await;
            let _base = crate::bot::close::tests::EnvRestore::set("MACBOT_CLOB_BASE", &base);
            let analysis = paper_entry_analysis();
            bot.market_state.up_token_cache.insert(
                analysis.market.market_id.clone(),
                (std::time::Instant::now(), "paper-fok-token".into()),
            );
            let cash = bot.balance().await;
            let outcome = bot
                .execute_trade_strategy(&analysis, false, Some(10.0), "conviction")
                .await;
            if should_fill {
                let trade = outcome.expect("complete haircutted arrival capacity must fill");
                let requested = 10.0
                    / super::taker_limit_price(
                        0.505,
                        bot.control_plane.config.read().await.max_slippage,
                    );
                assert_eq!(trade.shares, requested);
                assert!((trade.entry_price - 0.51).abs() < 1e-12);
                assert!(
                    (bot.balance().await - (cash - trade.amount - trade.fee_paid)).abs() < 1e-9
                );
                let fills = crate::db::load_trade_fills(&bot.db, trade.id, 10).await?;
                assert_eq!(fills.len(), 1);
                let details: serde_json::Value = serde_json::from_str(&fills[0].details)?;
                assert_eq!(details["paper_execution"]["fill_policy"], "fill_or_kill");
                assert_eq!(details["paper_execution"]["available_depth_fraction"], 0.25);
                assert_eq!(details["paper_execution"]["latency_ms"], 0);
            } else {
                assert!(
                    outcome.is_none(),
                    "partial IOC capacity must not create a FOK position"
                );
                assert_eq!(bot.balance().await, cash);
                assert!(bot.trades().await.is_empty());
                let counts: (i64, i64) = sqlx::query_as(
                    "SELECT (SELECT COUNT(*) FROM trades),(SELECT COUNT(*) FROM trade_fills)",
                )
                .fetch_one(&bot.db)
                .await?;
                assert_eq!(counts, (0, 0));
            }
            bot.db.close().await;
        }
        Ok(())
    }

    #[test]
    fn actual_entry_price_can_consume_edge_despite_high_heuristic_confidence() {
        let mut analysis = AnalysisResult {
            signal: Signal::BuyYes,
            confidence: 0.99,
            p_up: Some(0.55),
            ..Default::default()
        };
        assert!(entry_return_at_price(&analysis, analysis.signal, 0.5, 0.04).unwrap() > 0.0);
        assert!(entry_return_at_price(&analysis, analysis.signal, 0.6, 0.04).unwrap() < 0.0);
        analysis.signal = Signal::BuyNo;
        analysis.p_up = Some(0.2);
        assert!(
            (entry_return_at_price(&analysis, analysis.signal, 0.5, 0.04).unwrap() - 0.56).abs()
                < 1e-9
        );
        analysis.p_up = None;
        assert!(entry_return_at_price(&analysis, analysis.signal, 0.5, 0.04).is_none());
        analysis.forced = true;
        analysis.net_ev = f64::NAN;
        assert!(entry_return_at_price(&analysis, analysis.signal, 0.5, 0.04).is_none());
    }

    #[tokio::test]
    async fn shutdown_interrupts_an_admitted_paper_order_delay() {
        let (bot, _trade, path) = crate::bot::close::tests::maker_exit_fixture(5.0).await;
        let delayed = bot.wait_paper_order_latency(60_000);
        let shutdown = async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            bot.runtime_state.begin_shutdown();
        };
        let (admitted, ()) = tokio::time::timeout(std::time::Duration::from_millis(250), async {
            tokio::join!(delayed, shutdown)
        })
        .await
        .expect("shutdown must not wait for the configured latency");
        assert!(!admitted, "an order interrupted by shutdown must not fill");
        crate::bot::close::tests::cleanup_exit_fixture(&bot, &path).await;
    }

    #[test]
    fn live_taker_uses_selected_token_book_and_rounds_within_slippage_cap() {
        for (tick, bid, ask, slippage, expected) in [
            ("0.01", 0.19, 0.20, 0.02, 0.21),
            ("0.001", 0.192, 0.193, 0.002, 0.194),
        ] {
            let book = json!({"tick_size":tick,"bids":[{"price":"0.01","size":"100"},{"price":bid,"size":10}],"asks":[{"price":"0.99","size":"100"},{"price":ask,"size":10}]});
            let price = super::live_taker_limit_price(&book, slippage).unwrap();
            assert!((price - expected).abs() < 1e-9);
            assert!(
                price < 0.3,
                "NO's own price must not be complemented to YES's price"
            );
            assert!(price >= ask);
            assert!(price <= (bid + ask) / 2.0 + slippage + 1e-9);
        }
        for book in [
            json!({"bids":[],"asks":[]}),
            json!({"bids":[{"price":0.5,"size":10}],"asks":[]}),
            json!({"tick_size":"NaN","bids":[{"price":0.5,"size":10}],"asks":[{"price":0.51,"size":10}]}),
        ] {
            assert!(super::live_taker_limit_price(&book, 0.02).is_none());
        }
    }

    fn approx_eq(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "expected {a} to be approximately {b}");
    }

    /// The CLOB lists each side worst-first. Read with `.first()`, a live
    /// book with a one-cent spread reported 0.98 and every taker entry was
    /// rejected as a thin book — 823 identical rejections in the real record.
    #[test]
    fn best_prices_are_the_best_even_when_the_wire_lists_the_worst_first() {
        let book = json!({
            "bids": [
                {"price": "0.01", "size": "5000"},
                {"price": "0.50", "size": "40"}
            ],
            "asks": [
                {"price": "0.99", "size": "5000"},
                {"price": "0.51", "size": "40"}
            ]
        });
        let (best_bid, best_ask) = book_best_bid_ask(&book);
        approx_eq(best_bid, 0.50);
        approx_eq(best_ask, 0.51);
        assert!(
            best_ask - best_bid < 0.05,
            "spread {:.4} would fail the thin-book gate on a one-cent book",
            best_ask - best_bid
        );
    }

    /// Same root cause, second symptom: the fill walk breaks on the first ask
    /// above the limit, so a worst-first book never fills a single share.
    #[test]
    fn a_buy_fills_from_the_best_ask_even_when_it_is_listed_last() {
        let book = json!({
            "asks": [
                {"price": "0.99", "size": "5000"},
                {"price": "0.70", "size": "200"},
                {"price": "0.51", "size": "40"}
            ]
        });
        let (avg_price, filled) = simulate_buy_fill_core(&book, 0.55, 10.0)
            .expect("a 0.55 limit against a 0.51 best ask must fill");
        approx_eq(filled, 10.0);
        approx_eq(avg_price, 0.51);
    }

    /// A taker crosses the spread. On a live-shaped book with a one-cent
    /// spread, the limit must reach the best ask and the fill must land there.
    #[test]
    fn a_taker_entry_crosses_the_spread_and_fills_at_the_ask() {
        let book = json!({
            "bids": [{"price": "0.01", "size": "5000"}, {"price": "0.50", "size": "40"}],
            "asks": [{"price": "0.99", "size": "5000"}, {"price": "0.51", "size": "40"}]
        });
        let mid = 0.505;
        let limit = taker_limit_price(mid, 0.02);
        assert!(limit >= 0.51, "limit {limit} does not reach the best ask");
        let (paid, filled) =
            simulate_buy_fill_core(&book, limit, 10.0).expect("a taker on a one-cent book fills");
        approx_eq(paid, 0.51);
        approx_eq(filled, 10.0);
    }

    /// The old placement, 2% below mid, could not fill on the same book — the
    /// exact failure the live log showed as "no fill available at limit".
    #[test]
    fn a_limit_below_mid_never_fills_against_the_asks() {
        let book = json!({
            "asks": [{"price": "0.99", "size": "5000"}, {"price": "0.51", "size": "40"}]
        });
        let below_mid = 0.505 * 0.98;
        assert!(simulate_buy_fill_core(&book, below_mid, 10.0).is_none());
    }

    /// The slippage cap is the stop: a book whose best ask is further above
    /// mid than the operator allowed does not fill, rather than filling badly.
    #[test]
    fn a_taker_stops_at_the_slippage_cap() {
        let book = json!({ "asks": [{"price": "0.60", "size": "100"}] });
        let limit = taker_limit_price(0.505, 0.02);
        assert!(limit < 0.60);
        assert!(simulate_buy_fill_core(&book, limit, 10.0).is_none());
        approx_eq(taker_limit_price(0.985, 0.05), 0.99);
        approx_eq(taker_limit_price(0.50, 0.0), 0.51);
    }

    #[test]
    fn missing_book_side_defaults_best_prices_to_midpoint() {
        let book = json!({
            "bids": [
                {"price": "0.47", "size": "20"}
            ]
        });

        let (best_bid, best_ask) = book_best_bid_ask(&book);
        approx_eq(best_bid, 0.47);
        approx_eq(best_ask, 0.50);
    }

    #[test]
    fn total_book_usd_sums_bids_and_asks() {
        let book = json!({
            "bids": [
                {"price": "0.45", "size": "100"},
                {"price": "0.44", "size": "50"}
            ],
            "asks": [
                {"price": "0.55", "size": "80"}
            ]
        });

        approx_eq(book_total_usd(&book), 111.0);
    }

    #[test]
    fn book_helpers_ignore_out_of_range_prices() {
        let book = json!({
            "bids": [
                {"price": "1.20", "size": "100"},
                {"price": "0.45", "size": "100"}
            ],
            "asks": [
                {"price": "-0.10", "size": "100"},
                {"price": "0.55", "size": "80"}
            ]
        });

        let (best_bid, best_ask) = book_best_bid_ask(&book);
        approx_eq(best_bid, 0.45);
        approx_eq(best_ask, 0.55);
        approx_eq(book_total_usd(&book), 89.0);
    }

    #[test]
    fn simulate_buy_fill_respects_limit_price_and_partial_depth() {
        let book = json!({
            "asks": [
                {"price": "0.51", "size": "10"},
                {"price": "0.52", "size": "10"},
                {"price": "0.53", "size": "10"}
            ]
        });

        let (avg_price, filled) =
            simulate_buy_fill(&book, 0.52, 15.0).unwrap_or_else(|| panic!("expected fill"));

        approx_eq(avg_price, 7.7 / 15.0);
        approx_eq(filled, 15.0);
    }

    #[test]
    fn polled_fill_update_repairs_partial_fill_notional() {
        let mut trade = Trade {
            id: 42,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            market_id: "m".to_string(),
            question: "q".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.50,
            amount: 10.0,
            shares: 20.0,
            expected_value: 0.0,
            reason: "".to_string(),
            confidence: 0.7,
            status: "OPEN".to_string(),
            mode: "live".to_string(),
            source: "bot".to_string(),
            end_date: "".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: None,
            order_id: Some("entry-1".to_string()),
            token_id: Some("token".to_string()),
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: None,
            run_id: None,
            strategy: "conviction".to_string(),
        };

        let dirty = apply_polled_entry_fill_update(&mut trade, 12.0, 0.48, "matched");
        assert!(dirty);
        approx_eq(trade.entry_price, 0.48);
        approx_eq(trade.shares, 12.0);
        approx_eq(trade.amount, 5.76);
    }

    #[test]
    fn probe_book_stability_rejects_thin_and_wide_books() {
        let (stable, score) = probe_book_stability(0.95, 0.10, 0.25, 40.0, 20.0, 5.0, 4.0);

        assert!(!stable);
        assert!(score < 0.5);
    }

    #[test]
    fn buy_no_trend_up_guard_flags_weak_no_in_trend_up() {
        let mut cfg: Config =
            serde_json::from_str(include_str!("../../config.json")).expect("valid fixture");
        cfg.buy_no_trend_up_guard_enabled = true;
        cfg.buy_no_trend_up_guard_shadow_only = true;
        cfg.buy_no_trend_up_max_p_up = 0.35;

        let analysis = AnalysisResult {
            signal: Signal::BuyNo,
            p_up: Some(0.41),
            regime: "TREND_UP".to_string(),
            market: AnalysisMarketData::default(),
            ..AnalysisResult::default()
        };

        let reason = buy_no_trend_up_guard_reason(&cfg, &analysis, "TREND_UP");
        assert!(reason.is_some());
    }

    #[test]
    fn buy_no_trend_up_guard_allows_strong_no_in_trend_up() {
        let mut cfg: Config =
            serde_json::from_str(include_str!("../../config.json")).expect("valid fixture");
        cfg.buy_no_trend_up_guard_enabled = true;
        cfg.buy_no_trend_up_max_p_up = 0.35;

        let analysis = AnalysisResult {
            signal: Signal::BuyNo,
            p_up: Some(0.24),
            regime: "TREND_UP".to_string(),
            market: AnalysisMarketData::default(),
            ..AnalysisResult::default()
        };

        let reason = buy_no_trend_up_guard_reason(&cfg, &analysis, "TREND_UP");
        assert!(reason.is_none());
    }
}
