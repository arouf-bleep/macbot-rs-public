use crate::http_client::JsonResponseExt;
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::query;
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::analysis::AnalysisResult as ParsedAnalysis;
use crate::config::Config;
use crate::db::Trade;
use crate::domain::Direction;
use crate::http_client::HTTP;

use super::trade_lifecycle::CachedTradePrice;
use super::TradingBot;

fn tp_sl_fallback_max_age_secs(cfg: &Config) -> u64 {
    cfg.tp_sl_fallback_max_age_seconds.max(1)
}

fn analysis_fallback_is_fresh(cfg: &Config, analysis_age_secs: u64) -> bool {
    let max_age = tp_sl_fallback_max_age_secs(cfg).min(cfg.analysis_stale_seconds.max(1));
    analysis_age_secs <= max_age
}

fn cached_price_is_fresh(cfg: &Config, cached: CachedTradePrice) -> bool {
    cached.observed_at.elapsed().as_secs() <= tp_sl_fallback_max_age_secs(cfg)
}

impl TradingBot {
    /// Lightweight TP/SL guard pass for background loops.
    ///
    /// Uses the latest cached analysis for the currently-active market and
    /// per-trade live price fetches for all other open positions.
    pub async fn tp_sl_guard_tick(&self) {
        let cfg = self.control_plane.config.read().await.clone();
        if !(cfg.take_profit || cfg.stop_loss || cfg.partial_take_profit) {
            return;
        }
        let analysis = self.analysis_state.last_analysis.read().await.clone();
        self.check_tp_sl(&cfg, &analysis).await;
    }

    fn parse_price_value(v: &Value) -> Option<f64> {
        v.as_f64()
            .or_else(|| v.as_i64().map(|x| x as f64))
            .or_else(|| v.as_u64().map(|x| x as f64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
    }

    fn yes_price_from_market_json(market: &Value) -> Option<f64> {
        let out_prices: Vec<Value> = market
            .get("outcomePrices")
            .and_then(|op| {
                op.as_array()
                    .cloned()
                    .or_else(|| op.as_str().and_then(|s| serde_json::from_str(s).ok()))
            })
            .unwrap_or_default();
        if out_prices.len() < 2 {
            if let Some(tokens) = market.get("tokens").and_then(|t| t.as_array()) {
                for token in tokens {
                    let outcome = token
                        .get("outcome")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_lowercase();
                    let is_yes = !(outcome == "no" || outcome == "down");
                    if !is_yes {
                        continue;
                    }
                    let price = token.get("price").and_then(Self::parse_price_value)?;
                    if (0.0..=1.0).contains(&price) {
                        return Some(price);
                    }
                }
            }
            return None;
        }

        let outcomes = market
            .get("outcomes")
            .and_then(|o| {
                o.as_array()
                    .cloned()
                    .or_else(|| o.as_str().and_then(|s| serde_json::from_str(s).ok()))
            })
            .unwrap_or_default();
        let yes_is_idx0 = outcomes
            .first()
            .and_then(|o| o.as_str())
            .map(|s| {
                let l = s.to_lowercase();
                !(l == "no" || l == "down")
            })
            .unwrap_or(true);

        let p0 = Self::parse_price_value(&out_prices[0])?;
        let p1 = Self::parse_price_value(&out_prices[1])?;
        let yes_price = if yes_is_idx0 { p0 } else { p1 };
        if (0.0..=1.0).contains(&yes_price) {
            Some(yes_price)
        } else {
            None
        }
    }

    async fn fetch_token_price_clob(token_id: &str) -> Option<f64> {
        let url = format!(
            "https://clob.polymarket.com/price?token_id={}&side=BUY",
            token_id
        );
        let r = HTTP.get(&url).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        let v: Value = r.bounded_json().await.ok()?;
        let p = v.get("price").and_then(Self::parse_price_value)?;
        if p > 0.0 && p < 1.0 {
            Some(p)
        } else {
            None
        }
    }

    async fn fetch_yes_price_gamma(market_id: &str) -> Option<f64> {
        let url = format!("https://gamma-api.polymarket.com/markets/{market_id}");
        let r = HTTP.get(&url).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        let v: Value = r.bounded_json().await.ok()?;
        let p = Self::yes_price_from_market_json(&v)?;
        if p > 0.0 && p < 1.0 {
            Some(p)
        } else {
            None
        }
    }

    async fn fetch_yes_price_clob_market(market_id: &str) -> Option<f64> {
        let url = format!("https://clob.polymarket.com/markets/{market_id}");
        let r = HTTP.get(&url).send().await.ok()?;
        if !r.status().is_success() {
            return None;
        }
        let v: Value = r.bounded_json().await.ok()?;
        let p = Self::yes_price_from_market_json(&v)?;
        if p > 0.0 && p < 1.0 {
            Some(p)
        } else {
            None
        }
    }

    pub async fn live_token_price_for_trade(&self, trade: &Trade) -> Option<f64> {
        if let Some(token_id) = trade.token_id.as_deref().filter(|s| !s.is_empty()) {
            if let Some(p) = Self::fetch_token_price_clob(token_id).await {
                return Some(p);
            }
        }

        let yes_price = if let Some(p) = Self::fetch_yes_price_gamma(&trade.market_id).await {
            p
        } else {
            Self::fetch_yes_price_clob_market(&trade.market_id).await?
        };
        let token_price = if trade.direction_kind() == Direction::BuyNo {
            1.0 - yes_price
        } else {
            yes_price
        };
        if token_price > 0.0 && token_price < 1.0 {
            Some(token_price)
        } else {
            None
        }
    }

    pub fn unrealized_pnl_for_trade(&self, trade: &Trade, token_price: f64) -> f64 {
        let mut pnl = trade.shares * (token_price - trade.entry_price);
        if !trade.is_live() && token_price > 0.01 && token_price < 0.99 {
            let exit_fee = crate::fusion::estimate_taker_fee(token_price);
            pnl -= exit_fee * (trade.shares * token_price);
        }
        pnl
    }

    /// OPEN alone does not prove a completed entry. Match the durable BUY
    /// phase; a rejected SELL may leave an order ID on an OPEN residual lot.
    pub(crate) async fn entry_confirmation_state(
        &self,
        trade: &Trade,
    ) -> anyhow::Result<Option<&'static str>> {
        if !trade.is_live() || !trade.is_open() {
            return Ok(None);
        }
        let state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM live_orders WHERE trade_id=? AND phase='entry'
             AND (state='audit_uncertain' OR (state='pending' AND order_id=?))
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(trade.id)
        .bind(trade.order_id.as_deref().unwrap_or_default())
        .fetch_optional(&self.db)
        .await?;
        Ok(match state.as_deref() {
            Some("pending") => Some("pending"),
            Some("audit_uncertain") => Some("audit_uncertain"),
            _ if trade.has_order() && trade.shares <= 0.0 => Some("unavailable"),
            _ => None,
        })
    }

    pub async fn trade_with_live_fields(&self, trade: &Trade) -> Value {
        let mut out = json!(trade);
        if let Some(intent) = self
            .trade_lifecycle
            .partial_close_intents
            .read()
            .await
            .get(&trade.id)
            .filter(|_| trade.has_unsettled_exposure())
        {
            out["partial_close_state"] = json!(if intent.outcome_unknown {
                "unknown"
            } else {
                "in_flight"
            });
            out["partial_close_reason"] = json!("A partial sale is awaiting a known result; automatic exits for this position are held. Inspect live order evidence, or stop and restart to reload the paper ledger.");
            out["live_price"] = Value::Null;
            out["unrealized_pnl"] = Value::Null;
            out["mark_value"] = Value::Null;
            return out;
        }
        if !trade.is_open() {
            return out;
        }
        let entry_state = match self.entry_confirmation_state(trade).await {
            Ok(state) => state,
            Err(error) if trade.is_live() => {
                warn!(
                    "entry confirmation lookup failed for trade {}: {error}",
                    trade.id
                );
                Some("unavailable")
            }
            Err(_) => None,
        };
        if let Some(state) = entry_state {
            out["entry_execution_status"] = json!(state);
            out["live_price"] = Value::Null;
            out["unrealized_pnl"] = Value::Null;
            out["mark_value"] = Value::Null;
            return out;
        }

        let (live_price, unrealized_pnl, mark_value) =
            if let Some(p) = self.live_token_price_for_trade(trade).await {
                (
                    Some(p),
                    Some(self.unrealized_pnl_for_trade(trade, p)),
                    Some(trade.shares * p),
                )
            } else {
                (None, None, None)
            };

        if let Some(obj) = out.as_object_mut() {
            obj.insert("live_price".into(), json!(live_price));
            obj.insert("unrealized_pnl".into(), json!(unrealized_pnl));
            obj.insert("mark_value".into(), json!(mark_value));
        }
        out
    }

    pub async fn set_trade_exit_type(&self, trade_id: i64, exit_type: &str) {
        if let Err(e) = query("UPDATE trades SET exit_type = ? WHERE id = ?")
            .bind(exit_type)
            .bind(trade_id)
            .execute(&self.db)
            .await
        {
            warn!("set_trade_exit_type DB update failed for trade {trade_id}: {e}");
            return;
        }
        {
            let mut trades = self.portfolio_state.trades.write().await;
            if let Some(t) = trades.iter_mut().find(|t| t.id == trade_id) {
                t.exit_type = Some(exit_type.to_string());
            }
        }
    }

    async fn is_trade_still_open(&self, trade_id: i64) -> bool {
        self.portfolio_state
            .trades
            .read()
            .await
            .iter()
            .any(|t| t.id == trade_id && t.is_open())
    }

    pub(super) async fn check_tp_sl(&self, cfg: &Config, analysis: &Value) {
        let parsed = ParsedAnalysis::from_value(analysis);
        let analysis_age_secs = self.analysis_age_secs().await;
        let active_yes_price = if parsed.market.price > 0.0 && parsed.market.price < 1.0 {
            Some(parsed.market.price)
        } else {
            None
        };
        let active_market_id = if parsed.market.market_id.is_empty() {
            None
        } else {
            Some(parsed.market.market_id.as_str())
        };

        let open_trades: Vec<Trade> = self
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|t| t.is_open())
            .cloned()
            .collect();

        for trade in open_trades {
            if self
                .trade_lifecycle
                .partial_close_intents
                .read()
                .await
                .contains_key(&trade.id)
            {
                debug!(
                    trade_id = trade.id,
                    "automatic exits wait for the partial sale's known result"
                );
                continue;
            }
            if !self.is_trade_still_open(trade.id).await {
                continue;
            }

            let is_scalp = trade.strategy == "scalp";
            let is_maker = trade.strategy == "maker";

            let mut observed_at = Instant::now();
            let token_price = match self.live_token_price_for_trade(&trade).await {
                Some(p) => p,
                None => {
                    if active_market_id == Some(trade.market_id.as_str())
                        && active_yes_price.is_some()
                        && analysis_fallback_is_fresh(cfg, analysis_age_secs)
                    {
                        observed_at = *self.analysis_state.last_analysis_at.read().await;
                        let yes_price = active_yes_price.unwrap_or(0.5);
                        let p = if trade.direction_kind() == Direction::BuyNo {
                            1.0 - yes_price
                        } else {
                            yes_price
                        };
                        warn!(
                            "TP/SL: API unavailable for trade {} ({}), using fresh analysis cache {:.4} (age={}s <= {}s)",
                            trade.id,
                            trade.strategy,
                            p,
                            analysis_age_secs,
                            tp_sl_fallback_max_age_secs(cfg).min(cfg.analysis_stale_seconds.max(1))
                        );
                        p
                    } else if let Some(cached_price) = self
                        .trade_lifecycle
                        .last_trade_prices
                        .read()
                        .await
                        .get(&trade.id)
                        .copied()
                        .filter(|cached| cached_price_is_fresh(cfg, *cached))
                    {
                        observed_at = cached_price.observed_at;
                        let cached_age_secs = cached_price.observed_at.elapsed().as_secs();
                        warn!(
                            "TP/SL: API price unavailable for trade {} ({}), using fresh cached {:.4} (age={}s <= {}s)",
                            trade.id,
                            trade.strategy,
                            cached_price.price,
                            cached_age_secs,
                            tp_sl_fallback_max_age_secs(cfg)
                        );
                        cached_price.price
                    } else {
                        if active_market_id == Some(trade.market_id.as_str())
                            && active_yes_price.is_some()
                            && !analysis_fallback_is_fresh(cfg, analysis_age_secs)
                        {
                            warn!(
                                "TP/SL: analysis fallback too stale for trade {} ({}) age={}s > {}s — skipping",
                                trade.id,
                                trade.strategy,
                                analysis_age_secs,
                                tp_sl_fallback_max_age_secs(cfg).min(cfg.analysis_stale_seconds.max(1))
                            );
                        } else if let Some(cached_price) = self
                            .trade_lifecycle
                            .last_trade_prices
                            .read()
                            .await
                            .get(&trade.id)
                            .copied()
                        {
                            warn!(
                                "TP/SL: cached trade price too stale for trade {} ({}) age={}s > {}s — skipping",
                                trade.id,
                                trade.strategy,
                                cached_price.observed_at.elapsed().as_secs(),
                                tp_sl_fallback_max_age_secs(cfg)
                            );
                        }
                        if !self.is_trade_still_open(trade.id).await {
                            continue;
                        }
                        warn!(
                            "TP/SL: no price at all for trade {} ({}), skipping",
                            trade.id, trade.strategy
                        );
                        continue;
                    }
                }
            };

            self.trade_lifecycle.last_trade_prices.write().await.insert(
                trade.id,
                CachedTradePrice {
                    price: token_price,
                    observed_at,
                },
            );

            if is_scalp && cfg.scalp_enabled {
                let entry = trade.entry_price;
                let gain_pct = (token_price - entry) / entry;

                let trade_age_secs =
                    if let Ok(opened_at) = chrono::DateTime::parse_from_rfc3339(&trade.timestamp) {
                        (Utc::now() - opened_at.with_timezone(&Utc)).num_seconds()
                    } else {
                        999
                    };

                let mut hw = self.trade_lifecycle.scalp_high_water.write().await;
                let high_water = hw.entry(trade.id).or_insert(entry);
                if token_price > *high_water {
                    *high_water = token_price;
                }
                let hw_price = *high_water;
                let hw_gain_pct = (hw_price - entry) / entry;
                drop(hw);

                debug!(
                    "Scalp check: trade {} entry={:.4} live={:.4} high={:.4} gain={:.1}% hw_gain={:.1}% age={}s",
                    trade.id, entry, token_price, hw_price, gain_pct * 100.0, hw_gain_pct * 100.0, trade_age_secs
                );

                if gain_pct >= cfg.scalp_take_profit_threshold {
                    info!(
                        "Scalp TP: trade {} {} entry={:.4} now={:.4} gain={:.1}%",
                        trade.id,
                        trade.direction,
                        entry,
                        token_price,
                        gain_pct * 100.0
                    );
                    if let Some(closed) = self
                        .close_trade_serialized(trade.id, Some(token_price))
                        .await
                    {
                        if closed.status_kind() == crate::domain::TradeStatus::Closed {
                            self.set_trade_exit_type(closed.id, "scalp_take_profit")
                                .await;
                        }
                    }
                    self.trade_lifecycle
                        .scalp_high_water
                        .write()
                        .await
                        .remove(&trade.id);
                    continue;
                }

                if hw_gain_pct > 0.05 {
                    let drawdown_from_peak = (hw_price - token_price) / hw_price;
                    if drawdown_from_peak > 0.08 {
                        let trail_threshold = hw_price * (1.0 - 0.08);
                        if token_price <= trail_threshold {
                            info!(
                                "Scalp Trail: trade {} {} entry={:.4} peak={:.4} now={:.4} (gave back {:.1}%)",
                                trade.id, trade.direction, entry, hw_price, token_price, drawdown_from_peak * 100.0
                            );
                            if let Some(closed) = self
                                .close_trade_serialized(trade.id, Some(token_price))
                                .await
                            {
                                if closed.status_kind() == crate::domain::TradeStatus::Closed {
                                    self.set_trade_exit_type(closed.id, "scalp_trailing").await;
                                }
                            }
                            self.trade_lifecycle
                                .scalp_high_water
                                .write()
                                .await
                                .remove(&trade.id);
                            continue;
                        }
                    }
                }

                if trade_age_secs >= 30 && gain_pct <= -cfg.scalp_stop_loss_threshold {
                    info!(
                        "Scalp SL: trade {} {} entry={:.4} now={:.4} loss={:.1}%",
                        trade.id,
                        trade.direction,
                        entry,
                        token_price,
                        gain_pct * 100.0
                    );
                    if let Some(closed) = self
                        .close_trade_serialized(trade.id, Some(token_price))
                        .await
                    {
                        if closed.status_kind() == crate::domain::TradeStatus::Closed {
                            self.set_trade_exit_type(closed.id, "scalp_stop_loss").await;
                        }
                    }
                    self.trade_lifecycle
                        .scalp_high_water
                        .write()
                        .await
                        .remove(&trade.id);
                    continue;
                }

                if gain_pct <= -0.25 {
                    info!(
                        "Scalp Emergency SL: trade {} {} entry={:.4} now={:.4} loss={:.1}%",
                        trade.id,
                        trade.direction,
                        entry,
                        token_price,
                        gain_pct * 100.0
                    );
                    if let Some(closed) = self
                        .close_trade_serialized(trade.id, Some(token_price))
                        .await
                    {
                        if closed.status_kind() == crate::domain::TradeStatus::Closed {
                            self.set_trade_exit_type(closed.id, "scalp_emergency_sl")
                                .await;
                        }
                    }
                    self.trade_lifecycle
                        .scalp_high_water
                        .write()
                        .await
                        .remove(&trade.id);
                    continue;
                }

                continue;
            }

            if trade.is_live() {
                if let Ok(end_dt) = chrono::DateTime::parse_from_rfc3339(&trade.end_date) {
                    let remaining = end_dt.signed_duration_since(Utc::now());
                    if remaining.num_seconds() > 0
                        && remaining.num_seconds() < 120
                        && token_price < trade.entry_price
                    {
                        info!(
                            "Time-decay exit: trade {} {} losing with {}s left (entry={:.4} live={:.4})",
                            trade.id, trade.direction, remaining.num_seconds(), trade.entry_price, token_price
                        );
                        if let Some(closed) = self
                            .close_trade_serialized(trade.id, Some(token_price))
                            .await
                        {
                            if closed.status_kind() == crate::domain::TradeStatus::Closed {
                                self.set_trade_exit_type(closed.id, "time_decay").await;
                            }
                        }
                        continue;
                    }
                }
            }

            if is_maker && !cfg.maker_hold_positions {
                // Maker inventory should be managed by quote-side fills and explicit
                // maker risk logic, not conviction TP/SL or reversal exits.
                // In hold mode there are no quote-side exits, and these are the
                // only ones the position has.
                continue;
            }

            if let Some(p_up) = analysis.get("p_up").and_then(|v| v.as_f64()).filter(|_| {
                active_market_id == Some(trade.market_id.as_str())
                    && analysis_fallback_is_fresh(cfg, analysis_age_secs)
            }) {
                let is_long_yes = trade.direction == "BUY_YES";
                let strong_reversal = if is_long_yes {
                    p_up < 0.30
                } else {
                    p_up > 0.70
                };
                if strong_reversal {
                    info!(
                        "Conviction reversal: trade {} {} p_up={:.2} flipped against position",
                        trade.id, trade.direction, p_up
                    );
                    if let Some(closed) = self
                        .close_trade_serialized(trade.id, Some(token_price))
                        .await
                    {
                        if closed.status_kind() == crate::domain::TradeStatus::Closed {
                            self.set_trade_exit_type(closed.id, "reversal").await;
                        }
                    }
                    continue;
                }
            }

            self.check_profit_exit_policies(cfg, &trade, token_price)
                .await;
        }
    }

    /// Final conviction exit policies, after price freshness and reversal checks.
    async fn check_profit_exit_policies(&self, cfg: &Config, trade: &Trade, token_price: f64) {
        if self
            .trade_lifecycle
            .partial_close_intents
            .read()
            .await
            .contains_key(&trade.id)
        {
            return;
        }
        let min_tp_edge = 0.08;
        let has_tp_edge = token_price >= trade.entry_price + min_tp_edge;

        if cfg.partial_take_profit
            && (!trade.is_live() || cfg.live_partial_take_profit)
            && trade.partial_closed == Some(0)
            && token_price >= cfg.partial_tp_threshold
            && has_tp_edge
        {
            info!(
                "Partial TP: trade {} {} {:.4} >= {:.2} (entry={:.4}, edge={:.4})",
                trade.id,
                trade.direction,
                token_price,
                cfg.partial_tp_threshold,
                trade.entry_price,
                token_price - trade.entry_price
            );
            let outcome = self
                .partial_close_trade_serialized(trade.id, token_price, 0.5)
                .await;
            // Only an explicit no-execution rejection permits this pass to
            // consider the full-sale policy. A lost reply, an absent result or
            // a pending sale may already have reduced this lot; keep its intent.
            match outcome {
                Ok(Some(value)) if value["status"] == "rejected" && value["ok"] == false => {}
                Err(error) => {
                    warn!(trade_id = trade.id, %error,
                    "partial take-profit outcome unknown; ending this lot's guard pass");
                    return;
                }
                _ => return,
            }
            warn!(
                trade_id = trade.id,
                "partial take-profit did not execute; checking the configured full-exit policies"
            );
        }

        if cfg.take_profit && token_price >= cfg.take_profit_threshold && has_tp_edge {
            info!(
                "Conviction TP: trade {} {} {:.4} >= {:.2} (entry={:.4}, edge={:.4})",
                trade.id,
                trade.direction,
                token_price,
                cfg.take_profit_threshold,
                trade.entry_price,
                token_price - trade.entry_price
            );
            if let Some(closed) = self
                .close_trade_serialized(trade.id, Some(token_price))
                .await
            {
                if closed.status_kind() == crate::domain::TradeStatus::Closed {
                    self.set_trade_exit_type(closed.id, "take_profit").await;
                } else {
                    warn!(
                        "TP hit but trade {} did not close (status={})",
                        closed.id, closed.status
                    );
                }
            }
            return;
        }

        if cfg.stop_loss
            && token_price <= cfg.stop_loss_threshold
            && token_price < trade.entry_price
        {
            info!(
                "Conviction SL: trade {} {} {:.4} <= {:.2} (entry={:.4})",
                trade.id, trade.direction, token_price, cfg.stop_loss_threshold, trade.entry_price
            );
            if let Some(closed) = self
                .close_trade_serialized(trade.id, Some(token_price))
                .await
            {
                if closed.status_kind() == crate::domain::TradeStatus::Closed {
                    self.set_trade_exit_type(closed.id, "stop_loss").await;
                } else {
                    warn!(
                        "SL hit but trade {} did not close (status={})",
                        closed.id, closed.status
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::TradingBot;
    use super::{analysis_fallback_is_fresh, cached_price_is_fresh, tp_sl_fallback_max_age_secs};
    use crate::bot::state::trade_lifecycle::CachedTradePrice;
    use crate::config::Config;
    use crate::db::{self, Trade};
    use chrono::Utc;
    use std::time::{Duration, Instant};

    fn fixture() -> Config {
        serde_json::from_str(include_str!("../../../config.json")).expect("valid fixture")
    }

    #[test]
    fn analysis_fallback_obeys_tighter_tp_sl_bound() {
        let mut cfg = fixture();
        cfg.analysis_stale_seconds = 45;
        cfg.tp_sl_fallback_max_age_seconds = 15;

        assert!(analysis_fallback_is_fresh(&cfg, 15));
        assert!(!analysis_fallback_is_fresh(&cfg, 16));
    }

    #[test]
    fn analysis_fallback_never_exceeds_analysis_stale_threshold() {
        let mut cfg = fixture();
        cfg.analysis_stale_seconds = 5;
        cfg.tp_sl_fallback_max_age_seconds = 15;

        assert!(analysis_fallback_is_fresh(&cfg, 5));
        assert!(!analysis_fallback_is_fresh(&cfg, 6));
    }

    #[test]
    fn cached_trade_price_requires_fresh_observation() {
        let mut cfg = fixture();
        cfg.tp_sl_fallback_max_age_seconds = 3;
        assert_eq!(tp_sl_fallback_max_age_secs(&cfg), 3);

        let fresh = CachedTradePrice {
            price: 0.42,
            observed_at: Instant::now(),
        };
        let stale = CachedTradePrice {
            price: 0.42,
            observed_at: Instant::now() - Duration::from_secs(4),
        };

        assert!(cached_price_is_fresh(&cfg, fresh));
        assert!(!cached_price_is_fresh(&cfg, stale));
    }

    #[test]
    fn yes_price_from_market_json_prefers_yes_token_when_outcome_prices_missing() {
        let market = json!({
            "tokens": [
                {"outcome": "No", "price": "0.37"},
                {"outcome": "Yes", "price": "0.63"}
            ]
        });

        let yes = TradingBot::yes_price_from_market_json(&market)
            .expect("token payload should yield a yes price");
        assert!((yes - 0.63).abs() < f64::EPSILON);
    }

    #[test]
    fn yes_price_from_market_json_respects_outcome_ordering() {
        let market = json!({
            "outcomes": ["No", "Yes"],
            "outcomePrices": ["0.41", "0.59"]
        });

        let yes = TradingBot::yes_price_from_market_json(&market)
            .expect("outcomePrices payload should yield a yes price");
        assert!((yes - 0.59).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn maker_trade_skips_conviction_reversal_logic() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_tp_sl_maker_skip_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let cfg = fixture();
        let bot = TradingBot::new(cfg.clone(), pool.clone()).await?;

        let trade = Trade {
            id: 0,
            timestamp: Utc::now().to_rfc3339(),
            market_id: "mkt-1".to_string(),
            question: "maker tp/sl skip".to_string(),
            direction: "BUY_NO".to_string(),
            entry_price: 0.02,
            amount: 1.0,
            shares: 50.0,
            expected_value: 0.01,
            reason: "maker passive fill".to_string(),
            confidence: 0.8,
            status: "OPEN".to_string(),
            mode: "simulation".to_string(),
            source: "maker_sim".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: Some(0.8),
            order_id: Some("maker-order-1".to_string()),
            token_id: Some("tok-no".to_string()),
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: None,
            run_id: Some("test-run".to_string()),
            strategy: "maker".to_string(),
        };
        let trade_id = db::insert_trade(&pool, &trade).await?;
        let inserted = db::load_open_trades(&pool)
            .await?
            .into_iter()
            .find(|t| t.id == trade_id)
            .expect("inserted maker trade");
        bot.portfolio_state.trades.write().await.push(inserted);

        let analysis = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "maker tp/sl skip",
                "price": 0.99,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.99,
            "confidence": 0.95,
            "net_ev": 0.20
        });

        bot.check_tp_sl(&cfg, &analysis).await;

        let still_open = db::load_open_trades(&pool)
            .await?
            .into_iter()
            .any(|t| t.id == trade_id);
        assert!(
            still_open,
            "maker trade should not be auto-closed by conviction reversal"
        );

        pool.close().await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    /// The mirror of the test above. Off, maker inventory is the ask's job to
    /// close. On, there is no ask, and take-profit, stop-loss and reversal are
    /// the only exits a held position has.
    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn in_hold_mode_a_maker_position_gets_the_conviction_exits() -> anyhow::Result<()> {
        let _guard = crate::env_util::test_env_lock();
        // The held position is 50 shares: at the explicit 25% liquidity
        // haircut below, 300 displayed bids leave 75 executable shares.
        let base = crate::bot::close::tests::serve_one_book(json!({
            "bids": [{"price":"0.01", "size":"300"}],
            "asks": [{"price":"0.02", "size":"100"}]
        }))
        .await;
        let _env = crate::bot::close::tests::EnvRestore::set("MACBOT_CLOB_BASE", &base);
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_tp_sl_maker_hold_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg = fixture();
        cfg.maker_hold_positions = true;
        cfg.sim_available_depth_fraction = 0.25;
        let bot = std::sync::Arc::new(TradingBot::new(cfg.clone(), pool.clone()).await?);
        // The close runs through the execution actor, which the fixture does
        // not start; the skip test never needed it.
        bot.start_loop();
        bot.start_execution_actor().await;

        let trade = Trade {
            id: 0,
            timestamp: Utc::now().to_rfc3339(),
            market_id: "mkt-1".to_string(),
            question: "maker tp/sl skip".to_string(),
            direction: "BUY_NO".to_string(),
            entry_price: 0.02,
            amount: 1.0,
            shares: 50.0,
            expected_value: 0.01,
            reason: "maker passive fill".to_string(),
            confidence: 0.8,
            status: "OPEN".to_string(),
            mode: "simulation".to_string(),
            source: "maker_sim".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: Some(0.8),
            order_id: Some("maker-order-1".to_string()),
            token_id: Some("tok-no".to_string()),
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: None,
            run_id: Some("test-run".to_string()),
            strategy: "maker".to_string(),
        };
        let trade_id = db::insert_trade(&pool, &trade).await?;
        let inserted = db::load_open_trades(&pool)
            .await?
            .into_iter()
            .find(|t| t.id == trade_id)
            .expect("inserted maker trade");
        bot.portfolio_state.trades.write().await.push(inserted);

        let analysis = json!({
            "market_data": {
                "market_id": "mkt-1",
                "question": "maker tp/sl skip",
                "price": 0.99,
                "end_date": "2099-01-01T00:00:00Z"
            },
            "p_up": 0.99,
            "confidence": 0.95,
            "net_ev": 0.20
        });

        bot.check_tp_sl(&cfg, &analysis).await;

        let still_open = db::load_open_trades(&pool)
            .await?
            .into_iter()
            .any(|t| t.id == trade_id);
        assert!(
            !still_open,
            "in hold mode there are no quote-side exits; a BUY_NO held against p_up 0.99 \
             must be closed by the reversal, or it has no exit but resolution"
        );
        let closed = db::load_closed_trades(&pool).await?;
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].exit_type.as_deref(), Some("reversal"));
        assert_eq!(closed[0].exit_price, Some(0.01));

        pool.close().await;
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[derive(Clone, Copy)]
    enum PartialReply {
        LostAfterSettlement,
        TimeoutBeforeSettlement,
        MissingAfterPersistenceFailure,
        Pending,
        RejectedWithoutExecution,
    }

    /// Exercise the same partial/full exit block used by the guard, with a
    /// known fresh mark and a local actor. No price feed or venue is contacted.
    async fn exercise_partial_reply(reply: PartialReply) -> anyhow::Result<()> {
        use crate::bot::actor::ExecutionCommand;
        use crate::bot::close::CloseIntent;
        use std::sync::Arc;
        use tokio::sync::mpsc;

        let pool = db::open(":memory:").await?;
        let mut cfg = fixture();
        cfg.mode = "simulation".into();
        cfg.sim_live_execution = false;
        cfg.partial_take_profit = true;
        cfg.partial_tp_threshold = 0.75;
        cfg.take_profit = true;
        cfg.take_profit_threshold = 0.85;
        let bot = Arc::new(TradingBot::new(cfg.clone(), pool.clone()).await?);
        let trade = Trade {
            id: 0,
            timestamp: Utc::now().to_rfc3339(),
            market_id: "partial-reply-market".into(),
            question: "local partial reply regression".into(),
            direction: "BUY_YES".into(),
            entry_price: 0.5,
            amount: 6.0,
            shares: 12.0,
            expected_value: 0.1,
            reason: "test".into(),
            confidence: 0.8,
            status: "OPEN".into(),
            mode: "simulation".into(),
            source: "bot".into(),
            end_date: "2099-01-01T00:00:00Z".into(),
            exit_price: None,
            exit_timestamp: None,
            pnl: None,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: Some(0.8),
            order_id: None,
            token_id: Some("local-test-token".into()),
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 1,
            signals_json: None,
            calibration_version: None,
            run_id: Some("local-test".into()),
            strategy: "conviction".into(),
        };
        let trade_id = db::insert_trade(&pool, &trade).await?;
        let trade = db::load_trade_by_id(&pool, trade_id).await?.unwrap();
        bot.portfolio_state.trades.write().await.push(trade.clone());
        let rejected = matches!(reply, PartialReply::RejectedWithoutExecution);
        // A pre-existing entry stop must not blanket-disable known safe exits.
        if rejected {
            bot.set_kill_switch(true);
        }
        if matches!(reply, PartialReply::MissingAfterPersistenceFailure) {
            sqlx::query("CREATE TRIGGER reject_tp_partial_fill BEFORE INSERT ON trade_fills BEGIN SELECT RAISE(FAIL, 'partial fixture evidence refused'); END")
                .execute(&pool).await?;
        }
        let (tx, mut receiver) = mpsc::channel(4);
        let (deadline_seen, deadline_wait) = tokio::sync::oneshot::channel();
        let (allow_completion, completion_wait) = tokio::sync::oneshot::channel();
        *bot.execution_state.execution_tx.write().await = Some(tx);
        let actor_bot = bot.clone();
        let actor = tokio::spawn(async move {
            let command = receiver.recv().await.expect("partial command dispatched");
            let ExecutionCommand::PartialCloseTrade {
                trade_id: dispatched_id,
                exit_price,
                fraction,
                intent,
                request_id,
                mut resp,
            } = command
            else {
                panic!("first command must be a partial close")
            };
            assert_eq!(dispatched_id, trade_id);
            assert_eq!(fraction, 0.5);
            assert_eq!(intent, CloseIntent::Market);
            let request_id = request_id.expect("serialized command carries its exact generation");
            match reply {
                PartialReply::LostAfterSettlement | PartialReply::TimeoutBeforeSettlement => {
                    if matches!(reply, PartialReply::TimeoutBeforeSettlement) {
                        // Await the real receiver deadline; then the actor
                        // commits late and reopens the six-share residual.
                        resp.closed().await;
                        deadline_seen.send(()).unwrap();
                        completion_wait.await.unwrap();
                    }
                    let result = actor_bot
                        .partial_close_trade(dispatched_id, exit_price, fraction)
                        .await;
                    assert_eq!(result.as_ref().unwrap()["status"], "settled");
                    assert_eq!(result.as_ref().unwrap()["remaining_shares"], 6.0);
                    actor_bot
                        .finish_partial_close_request(trade_id, Some(request_id), &result)
                        .await;
                    drop(resp);
                }
                PartialReply::MissingAfterPersistenceFailure => {
                    let result = actor_bot
                        .partial_close_trade(dispatched_id, exit_price, fraction)
                        .await;
                    assert!(result.is_none());
                    assert!(actor_bot.kill_switch_active());
                    actor_bot
                        .finish_partial_close_request(trade_id, Some(request_id), &result)
                        .await;
                    let _ = resp.send(result);
                }
                PartialReply::Pending => {
                    let _ = resp.send(Some(
                        json!({"ok":false,"status":"pending","remaining_shares":12.0}),
                    ));
                }
                PartialReply::RejectedWithoutExecution => {
                    let _ = resp.send(Some(json!({"ok":false,"status":"rejected",
                        "reason":"partial quantity below venue minimum; no order submitted",
                        "remaining_shares":12.0})));
                }
            }
            match tokio::time::timeout(Duration::from_millis(250), receiver.recv()).await {
                Ok(Some(ExecutionCommand::CloseTrade {
                    trade_id: full_id,
                    resp,
                    ..
                })) => {
                    assert_eq!(full_id, trade_id);
                    let _ = resp.send(None);
                    true
                }
                Err(_) => false,
                Ok(None) => panic!("actor command channel closed before the observation window"),
                Ok(Some(_)) => panic!("unexpected follow-up actor command"),
            }
        });
        tokio::time::timeout(
            Duration::from_secs(35),
            bot.check_profit_exit_policies(&cfg, &trade, 0.9),
        )
        .await
        .expect("profit guard completes within its actor deadline");
        if matches!(reply, PartialReply::TimeoutBeforeSettlement) {
            deadline_wait.await?;
            assert!(bot
                .trade_lifecycle
                .partial_close_intents
                .read()
                .await
                .contains_key(&trade_id));
            // This second actual guard pass reaches the hold before price
            // acquisition or any reversal/stop/full-profit command.
            tokio::time::timeout(Duration::from_secs(1), bot.check_tp_sl(&cfg, &json!({})))
                .await
                .expect("held lot must not request another price or exit");
            let duplicate = bot
                .partial_close_trade_serialized_with_intent(trade_id, 0.9, 0.5, CloseIntent::Manual)
                .await?;
            assert_eq!(duplicate.as_ref().unwrap()["status"], "pending");
            assert!(
                bot.close_trade_serialized_result(trade_id, Some(0.9))
                    .await?
                    .is_none(),
                "the shared automatic full-exit dispatch must also respect the held intent"
            );
            let fields = bot.trade_with_live_fields(&trade).await;
            assert_eq!(fields["partial_close_state"], "unknown");
            assert!(fields["unrealized_pnl"].is_null());
            allow_completion.send(()).unwrap();
        }
        let full_dispatched = actor.await?;
        assert_eq!(
            full_dispatched, rejected,
            "only a known no-execution rejection may fall through to a full sale"
        );
        let persisted = db::load_trade_by_id(&pool, trade_id).await?.unwrap();
        if matches!(
            reply,
            PartialReply::Pending | PartialReply::MissingAfterPersistenceFailure
        ) {
            assert!(bot
                .trade_lifecycle
                .partial_close_intents
                .read()
                .await
                .contains_key(&trade_id));
            tokio::time::timeout(Duration::from_secs(1), bot.check_tp_sl(&cfg, &json!({})))
                .await
                .expect("an unresolved completed reply must hold later guard passes too");
        }
        assert!(persisted.is_open());
        if matches!(
            reply,
            PartialReply::LostAfterSettlement | PartialReply::TimeoutBeforeSettlement
        ) {
            assert_eq!(persisted.shares, 6.0);
            assert_eq!(persisted.partial_closed, Some(1));
            assert!(
                bot.kill_switch_active(),
                "unknown reply retains the entry stop"
            );
            assert!(
                !bot.trade_lifecycle
                    .partial_close_intents
                    .read()
                    .await
                    .contains_key(&trade_id),
                "known actor completion releases the exact held request even without a receiver"
            );
        } else {
            assert_eq!(persisted.shares, 12.0);
            assert_eq!(persisted.partial_closed, Some(0));
        }
        let fills: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM trade_fills WHERE trade_id=?")
            .bind(trade_id)
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            fills,
            i64::from(matches!(
                reply,
                PartialReply::LostAfterSettlement | PartialReply::TimeoutBeforeSettlement
            ))
        );
        if matches!(reply, PartialReply::TimeoutBeforeSettlement) {
            let (tx, mut receiver) = mpsc::channel(2);
            *bot.execution_state.execution_tx.write().await = Some(tx);
            let fresh = tokio::spawn(async move {
                let Some(ExecutionCommand::CloseTrade {
                    trade_id: id, resp, ..
                }) = receiver.recv().await
                else {
                    panic!("a fresh guard may close the now confirmed residual")
                };
                assert_eq!(id, trade_id);
                let _ = resp.send(None);
            });
            bot.check_profit_exit_policies(&cfg, &persisted, 0.9).await;
            fresh.await?;
        }
        *bot.execution_state.execution_tx.write().await = None;
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn partial_tp_lost_reply_after_settlement_does_not_queue_full_sale() -> anyhow::Result<()>
    {
        exercise_partial_reply(PartialReply::LostAfterSettlement).await
    }

    #[tokio::test]
    async fn partial_tp_timeout_before_late_settlement_does_not_queue_full_sale(
    ) -> anyhow::Result<()> {
        exercise_partial_reply(PartialReply::TimeoutBeforeSettlement).await
    }

    #[tokio::test]
    async fn partial_tp_missing_result_after_db_failure_does_not_queue_full_sale(
    ) -> anyhow::Result<()> {
        exercise_partial_reply(PartialReply::MissingAfterPersistenceFailure).await
    }

    #[tokio::test]
    async fn partial_tp_pending_result_does_not_queue_full_sale() -> anyhow::Result<()> {
        exercise_partial_reply(PartialReply::Pending).await
    }

    #[tokio::test]
    async fn partial_tp_explicit_rejection_preserves_full_sale_policy() -> anyhow::Result<()> {
        exercise_partial_reply(PartialReply::RejectedWithoutExecution).await
    }

    #[tokio::test]
    async fn partial_intent_holds_only_its_lot_and_allows_explicit_operator_close(
    ) -> anyhow::Result<()> {
        use crate::bot::actor::ExecutionCommand;
        use crate::bot::close::CloseIntent;
        use crate::bot::state::trade_lifecycle::PartialCloseIntent;
        let pool = db::open(":memory:").await?;
        let bot = std::sync::Arc::new(TradingBot::new(fixture(), pool.clone()).await?);
        let first = Trade {
            id: 43,
            market_id: "independent-pair".into(),
            direction: "BUY_YES".into(),
            status: "OPEN".into(),
            mode: "simulation".into(),
            shares: 12.0,
            amount: 4.8,
            entry_price: 0.4,
            ..Trade::default()
        };
        let mut second = first.clone();
        second.id = 44;
        second.direction = "BUY_NO".into();
        *bot.portfolio_state.trades.write().await = vec![first, second];
        bot.trade_lifecycle
            .partial_close_intents
            .write()
            .await
            .insert(
                42,
                PartialCloseIntent {
                    request_id: 9,
                    outcome_unknown: true,
                    live_order_id: None,
                },
            );
        let (tx, mut receiver) = tokio::sync::mpsc::channel(4);
        *bot.execution_state.execution_tx.write().await = Some(tx);
        assert!(bot
            .close_trade_serialized_result(42, Some(0.9))
            .await?
            .is_none());
        let duplicate = bot
            .partial_close_trade_serialized_with_intent(42, 0.9, 0.5, CloseIntent::Manual)
            .await?;
        assert_eq!(duplicate.unwrap()["status"], "pending");
        assert!(
            receiver.try_recv().is_err(),
            "neither conflicting request reached the actor"
        );
        for pair in [(42, 43), (43, 42)] {
            assert!(matches!(
                bot.close_pair_serialized_result(pair.0, pair.1, Some(0.5))
                    .await,
                Err(super::super::ActorCallError::PartialClosePending),
            ));
        }
        assert!(bot
            .close_trade_serialized_with_intent(42, Some(1.0), CloseIntent::Resolution)
            .await?
            .is_none());
        assert!(
            receiver.try_recv().is_err(),
            "neither held pair nor resolution reached the actor"
        );
        {
            let mut trades = bot.portfolio_state.trades.write().await;
            trades[1].shares = 11.0;
        }
        assert!(matches!(
            bot.close_pair_serialized_result(43, 44, Some(0.5)).await,
            Err(super::super::ActorCallError::PairNotMergeable)
        ));
        assert!(matches!(
            bot.close_pair_serialized_result(43, 43, Some(0.5)).await,
            Err(super::super::ActorCallError::PairNotMergeable)
        ));
        assert!(
            receiver.try_recv().is_err(),
            "invalid pairs cannot reach the actor"
        );
        bot.portfolio_state.trades.write().await[1].shares = 12.0;
        assert!(matches!(
            bot.close_pair_serialized_result(43, 44, Some(0.9)).await,
            Err(super::super::ActorCallError::PairNotMergeable)
        ));
        assert!(
            receiver.try_recv().is_err(),
            "wrong pair payout cannot reach the actor"
        );
        let worker = tokio::spawn(async move {
            for expected in [(43, CloseIntent::Market), (42, CloseIntent::Manual)] {
                let Some(ExecutionCommand::CloseTrade {
                    trade_id,
                    intent,
                    resp,
                    ..
                }) = receiver.recv().await
                else {
                    panic!("expected the independent or explicit full exit")
                };
                assert_eq!((trade_id, intent), expected);
                let _ = resp.send(None);
            }
            let Some(ExecutionCommand::ClosePair {
                first_trade_id,
                second_trade_id,
                resp,
                ..
            }) = receiver.recv().await
            else {
                panic!("an independent pair must remain available")
            };
            assert_eq!((first_trade_id, second_trade_id), (43, 44));
            let _ = resp.send(Ok((None, None)));
        });
        bot.close_trade_serialized_result(43, Some(0.9)).await?;
        bot.close_trade_serialized_with_intent(42, Some(0.9), CloseIntent::Manual)
            .await?;
        bot.close_pair_serialized_result(43, 44, Some(0.5)).await?;
        worker.await?;
        assert!(bot
            .trade_lifecycle
            .partial_close_intents
            .read()
            .await
            .contains_key(&42));
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn partial_intent_generation_cannot_clear_a_newer_request_or_wrap() -> anyhow::Result<()>
    {
        use crate::bot::close::CloseIntent;
        use crate::bot::state::trade_lifecycle::PartialCloseIntent;
        use std::sync::atomic::Ordering;
        let pool = db::open(":memory:").await?;
        let bot = TradingBot::new(fixture(), pool.clone()).await?;
        bot.trade_lifecycle
            .partial_close_intents
            .write()
            .await
            .insert(
                42,
                PartialCloseIntent {
                    request_id: 2,
                    outcome_unknown: false,
                    live_order_id: None,
                },
            );
        let settled = Some(json!({"ok":true,"status":"settled"}));
        bot.finish_partial_close_request(42, Some(1), &settled)
            .await;
        assert_eq!(
            bot.trade_lifecycle.partial_close_intents.read().await[&42].request_id,
            2
        );
        bot.finish_partial_close_request(42, Some(2), &None).await;
        assert!(bot.trade_lifecycle.partial_close_intents.read().await[&42].outcome_unknown);
        bot.finish_partial_close_request(42, Some(2), &settled)
            .await;
        assert!(!bot
            .trade_lifecycle
            .partial_close_intents
            .read()
            .await
            .contains_key(&42));
        bot.trade_lifecycle
            .partial_close_sequence
            .store(u64::MAX, Ordering::SeqCst);
        let (tx, mut receiver) = tokio::sync::mpsc::channel(1);
        *bot.execution_state.execution_tx.write().await = Some(tx);
        let exhausted = bot
            .partial_close_trade_serialized_with_intent(42, 0.9, 0.5, CloseIntent::Manual)
            .await?;
        assert_eq!(exhausted.unwrap()["status"], "pending");
        assert_eq!(
            bot.trade_lifecycle
                .partial_close_sequence
                .load(Ordering::SeqCst),
            u64::MAX
        );
        assert!(
            receiver.try_recv().is_err(),
            "exhaustion must refuse before dispatch"
        );
        pool.close().await;
        Ok(())
    }
}
