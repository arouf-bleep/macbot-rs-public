use std::fmt;

use tracing::warn;

use super::state::TradingBot;
use crate::bot::macro_calendar::MacroGuardStatus;
use crate::config::Config;
use crate::db;
use crate::domain::{BotMode, Signal, TradeStatus};

pub(crate) fn session_bucket(hour_utc: u32) -> &'static str {
    match hour_utc {
        0..=7 => "asia",
        8..=15 => "europe",
        _ => "us",
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TradeRiskSnapshot {
    pub today_pnl: f64,
    pub available_balance: f64,
    pub current_exposure: f64,
    pub macro_guard: MacroGuardStatus,
    pub apply_live_rollout_guardrails: bool,
    pub require_shadow_pass: bool,
    pub shadow_passed: bool,
    pub rollout_stage: String,
    pub canary_max_usd: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct ProposedTrade {
    pub signal: Signal,
    pub strategy: String,
    pub confidence: f64,
    pub funding_rate: Option<f64>,
    pub regime: String,
    pub token_price: f64,
    pub liquidity: f64,
    pub entry_hour_utc: u32,
    pub time_remaining_secs: Option<f64>,
    pub btc_change_pct: Option<f64>,
    pub net_ev: f64,
    pub forced: bool,
    pub hail_mary: bool,
    pub requested_amount: f64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct EntryCoordinationSnapshot {
    pub has_same_market_strategy_position: bool,
    pub open_scalps: u32,
    pub open_conviction: u32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct EntryCoordinationProposal {
    pub signal: Signal,
    pub strategy: String,
    pub market_id: String,
    pub market_yes_price: f64,
    pub token_price: f64,
    pub p_up: Option<f64>,
    pub liquidity: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TradeRiskDecision {
    pub approved_amount: f64,
    pub effective_confidence: f64,
    pub min_confidence: f64,
    pub gas_pct: f64,
    pub required_ev: f64,
    pub canary_capped_from: Option<f64>,
    pub exposure_adjusted_from: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum TradeRiskReject {
    DailyLossLimit {
        today_pnl: f64,
        limit: f64,
    },
    HoldSignal,
    FundingExtremeReversal {
        funding_rate: f64,
        signal: Signal,
    },
    MacroEventStandDown {
        label: String,
        phase: String,
        minutes_until_event: f64,
    },
    MacroCalendarUnavailable {
        error: String,
    },
    RegimeNotAllowed {
        regime: String,
    },
    SessionNotAllowed {
        session: String,
    },
    BlockedHourUtc {
        hour_utc: u32,
    },
    TooCloseToExpiry {
        strategy: String,
        remaining_secs: f64,
        min_remaining_secs: f64,
    },
    ConfidenceTooLow {
        effective_confidence: f64,
        min_confidence: f64,
    },
    ConfidenceMarginTooLow {
        effective_confidence: f64,
        min_confidence: f64,
        confidence_margin: f64,
        required_margin: f64,
    },
    MinLiquidity {
        liquidity: f64,
        min_liquidity: f64,
    },
    EntryPriceOutOfRange {
        token_price: f64,
        min_entry_price: f64,
        max_entry_price: f64,
    },
    HailMaryPriceTooHigh {
        token_price: f64,
        max_price: f64,
    },
    HailMaryBtcChangeTooSmall {
        btc_change_pct: f64,
        min_btc_change_pct: f64,
    },
    TradeAmountBelowMinimum {
        amount: f64,
        min_trade_usd: f64,
    },
    ShadowPassRequired,
    ShadowRolloutBlocked,
    InsufficientBalance {
        available_balance: f64,
        amount: f64,
    },
    ExposureCapExceeded {
        current_exposure: f64,
        amount: f64,
        max_exposure_pct: f64,
        available_balance: f64,
    },
    NetEvBelowRequired {
        net_ev: f64,
        required_ev: f64,
        min_ev: f64,
        edge_safety_buffer: f64,
        gas_pct: f64,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) enum EntryCoordinationReject {
    DuplicateStrategyPosition {
        market_id: String,
        strategy: String,
    },
    ScalpYesPriceOutOfRange {
        yes_price: f64,
    },
    ScalpTokenPriceTooHigh {
        token_price: f64,
        max_price: f64,
    },
    ScalpPositionCap {
        open_scalps: u32,
        max_scalps: u32,
    },
    ScalpMomentumTooWeak {
        signal: Signal,
        p_up: f64,
    },
    ScalpLiquidityTooLow {
        liquidity: f64,
        min_liquidity: f64,
    },
    ConvictionPositionCap {
        open_conviction: u32,
        max_open_positions: u32,
    },
}

impl fmt::Display for TradeRiskReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DailyLossLimit { today_pnl, limit } => write!(
                f,
                "daily loss limit ${limit:.2} reached (today_pnl=${today_pnl:.2})"
            ),
            Self::HoldSignal => write!(f, "signal is HOLD"),
            Self::MacroCalendarUnavailable { error } => write!(f, "macro calendar unavailable: {error}"),
            Self::FundingExtremeReversal {
                funding_rate,
                signal,
            } => write!(
                f,
                "extreme funding rate {funding_rate:.6} suggests reversal — skipping {signal:?}"
            ),
            Self::MacroEventStandDown {
                label,
                phase,
                minutes_until_event,
            } => {
                let relation = if *minutes_until_event >= 0.0 {
                    format!("{minutes_until_event:.1}m to release")
                } else {
                    format!("{:.1}m after release", minutes_until_event.abs())
                };
                write!(
                    f,
                    "macro guard active for {label} ({phase}, {relation}) — skipping"
                )
            }
            Self::RegimeNotAllowed { regime } => {
                write!(f, "regime {regime} not in allowlist — skipping")
            }
            Self::SessionNotAllowed { session } => {
                write!(f, "session {session} not in allowlist — skipping")
            }
            Self::BlockedHourUtc { hour_utc } => {
                write!(f, "hour {hour_utc:02} UTC is blocked — skipping")
            }
            Self::TooCloseToExpiry {
                strategy,
                remaining_secs,
                min_remaining_secs,
            } => write!(
                f,
                "{strategy} entry blocked: only {:.0}s remaining < {:.0}s minimum",
                remaining_secs.max(0.0),
                min_remaining_secs.max(0.0)
            ),
            Self::ConfidenceTooLow {
                effective_confidence,
                min_confidence,
            } => write!(
                f,
                "confidence {effective_confidence:.2} < min {min_confidence:.2}, skipping"
            ),
            Self::ConfidenceMarginTooLow {
                effective_confidence,
                min_confidence,
                confidence_margin,
                required_margin,
            } => write!(
                f,
                "confidence margin {:.2} (confidence {:.2} - min {:.2}) < required {:.2}, skipping",
                confidence_margin,
                effective_confidence,
                min_confidence,
                required_margin
            ),
            Self::MinLiquidity {
                liquidity,
                min_liquidity,
            } => write!(
                f,
                "liquidity ${liquidity:.2} < min ${min_liquidity:.2}, skipping"
            ),
            Self::EntryPriceOutOfRange {
                token_price,
                min_entry_price,
                max_entry_price,
            } => write!(
                f,
                "token price {token_price:.3} outside entry range [{min_entry_price}, {max_entry_price}]"
            ),
            Self::HailMaryPriceTooHigh {
                token_price,
                max_price,
            } => write!(
                f,
                "Hail Mary: price {token_price:.3} > max {max_price}"
            ),
            Self::HailMaryBtcChangeTooSmall {
                btc_change_pct,
                min_btc_change_pct,
            } => write!(
                f,
                "Hail Mary: BTC change {btc_change_pct:.2}% < min {min_btc_change_pct:.2}% — skipping"
            ),
            Self::TradeAmountBelowMinimum {
                amount,
                min_trade_usd,
            } => write!(
                f,
                "trade amount ${amount:.2} < min_trade_usd ${min_trade_usd:.2}"
            ),
            Self::ShadowPassRequired => write!(
                f,
                "live execution blocked (MACBOT_REQUIRE_SHADOW_PASS=1 and MACBOT_SHADOW_PASS!=1)"
            ),
            Self::ShadowRolloutBlocked => {
                write!(f, "live execution blocked by MACBOT_ROLLOUT_STAGE=shadow")
            }
            Self::InsufficientBalance {
                available_balance,
                amount,
            } => write!(
                f,
                "insufficient balance ${available_balance:.2} for trade ${amount:.2}"
            ),
            Self::ExposureCapExceeded {
                current_exposure,
                amount,
                max_exposure_pct,
                available_balance,
            } => write!(
                f,
                "exposure cap — ${current_exposure:.2} open + ${amount:.2} exceeds {:.0}% of ${available_balance:.2} — skipping",
                max_exposure_pct * 100.0
            ),
            Self::NetEvBelowRequired {
                net_ev,
                required_ev,
                min_ev,
                edge_safety_buffer,
                gas_pct,
            } => write!(
                f,
                "net_ev {net_ev:.4} < required_ev {required_ev:.4} (min_ev={min_ev:.4} buffer={edge_safety_buffer:.4} gas_pct={gas_pct:.4})"
            ),
        }
    }
}

impl fmt::Display for EntryCoordinationReject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateStrategyPosition {
                market_id,
                strategy,
            } => write!(
                f,
                "Skipping {market_id}: already have an open {strategy} position"
            ),
            Self::ScalpYesPriceOutOfRange { yes_price } => write!(
                f,
                "Scalp skipped: YES price {:.3} outside scalp range [0.15, 0.85]",
                yes_price
            ),
            Self::ScalpTokenPriceTooHigh {
                token_price,
                max_price,
            } => write!(
                f,
                "Scalp skipped: token price {:.3} too high for scalp entry (max {:.2})",
                token_price, max_price
            ),
            Self::ScalpPositionCap {
                open_scalps,
                max_scalps,
            } => write!(
                f,
                "Scalp skipped: open scalp cap reached ({open_scalps}/{max_scalps})"
            ),
            Self::ScalpMomentumTooWeak { signal, p_up } => match signal {
                Signal::BuyYes => {
                    write!(f, "Scalp skipped: weak upward momentum (p_up={p_up:.2})")
                }
                Signal::BuyNo => {
                    write!(f, "Scalp skipped: weak downward momentum (p_up={p_up:.2})")
                }
                Signal::Hold => write!(f, "Scalp skipped: HOLD signal"),
            },
            Self::ScalpLiquidityTooLow {
                liquidity,
                min_liquidity,
            } => write!(
                f,
                "Scalp skipped: depth ${:.0} < ${:.0} minimum for scalp",
                liquidity, min_liquidity
            ),
            Self::ConvictionPositionCap {
                open_conviction,
                max_open_positions,
            } => write!(
                f,
                "Open-position cap reached ({open_conviction}/{max_open_positions}) — skipping"
            ),
        }
    }
}

pub(crate) fn evaluate_entry_coordination(
    cfg: &Config,
    snapshot: &EntryCoordinationSnapshot,
    proposal: &EntryCoordinationProposal,
) -> Result<(), EntryCoordinationReject> {
    let result = evaluate_entry_coordination_core(cfg, snapshot, proposal);
    crate::replay::capture(
        "entry_coordination",
        || {
            serde_json::json!({
                "config": cfg.redacted_value(), "snapshot": snapshot, "proposal": proposal
            })
        },
        &result,
    );
    result
}

pub(crate) fn evaluate_entry_coordination_core(
    cfg: &Config,
    snapshot: &EntryCoordinationSnapshot,
    proposal: &EntryCoordinationProposal,
) -> Result<(), EntryCoordinationReject> {
    if snapshot.has_same_market_strategy_position {
        return Err(EntryCoordinationReject::DuplicateStrategyPosition {
            market_id: proposal.market_id.clone(),
            strategy: proposal.strategy.clone(),
        });
    }

    if proposal.strategy == "scalp" {
        if proposal.market_yes_price < 0.15 || proposal.market_yes_price > 0.85 {
            return Err(EntryCoordinationReject::ScalpYesPriceOutOfRange {
                yes_price: proposal.market_yes_price,
            });
        }
        if proposal.token_price > 0.85 {
            return Err(EntryCoordinationReject::ScalpTokenPriceTooHigh {
                token_price: proposal.token_price,
                max_price: 0.85,
            });
        }
        if snapshot.open_scalps >= 2 {
            return Err(EntryCoordinationReject::ScalpPositionCap {
                open_scalps: snapshot.open_scalps,
                max_scalps: 2,
            });
        }
        let p_up = proposal.p_up.unwrap_or(0.5);
        if (proposal.signal == Signal::BuyYes && p_up < 0.60)
            || (proposal.signal == Signal::BuyNo && p_up > 0.40)
        {
            return Err(EntryCoordinationReject::ScalpMomentumTooWeak {
                signal: proposal.signal,
                p_up,
            });
        }
        if proposal.liquidity > 0.0 && proposal.liquidity < 500.0 {
            return Err(EntryCoordinationReject::ScalpLiquidityTooLow {
                liquidity: proposal.liquidity,
                min_liquidity: 500.0,
            });
        }
    } else if cfg.max_open_positions > 0 && snapshot.open_conviction >= cfg.max_open_positions {
        return Err(EntryCoordinationReject::ConvictionPositionCap {
            open_conviction: snapshot.open_conviction,
            max_open_positions: cfg.max_open_positions,
        });
    }

    Ok(())
}

pub(crate) fn evaluate_trade_guardrails(
    cfg: &Config,
    snapshot: &TradeRiskSnapshot,
    proposal: &ProposedTrade,
) -> Result<TradeRiskDecision, TradeRiskReject> {
    let result = evaluate_trade_guardrails_core(cfg, snapshot, proposal);
    crate::replay::capture(
        "trade_risk",
        || {
            serde_json::json!({
                "config": cfg.redacted_value(), "snapshot": snapshot, "proposal": proposal
            })
        },
        &result,
    );
    result
}

pub(crate) fn evaluate_trade_guardrails_core(
    cfg: &Config,
    snapshot: &TradeRiskSnapshot,
    proposal: &ProposedTrade,
) -> Result<TradeRiskDecision, TradeRiskReject> {
    let (regime_allowlist, session_allowlist) = if proposal.strategy == "maker" {
        (&cfg.maker_regime_allowlist, &cfg.maker_session_allowlist)
    } else {
        (&cfg.regime_allowlist, &cfg.session_allowlist)
    };

    if snapshot.today_pnl <= -cfg.daily_loss_limit {
        return Err(TradeRiskReject::DailyLossLimit {
            today_pnl: snapshot.today_pnl,
            limit: cfg.daily_loss_limit,
        });
    }

    if proposal.signal == Signal::Hold {
        return Err(TradeRiskReject::HoldSignal);
    }

    if cfg.macro_event_guard_enabled {
        if let Some(error) = &snapshot.macro_guard.error {
            return Err(TradeRiskReject::MacroCalendarUnavailable {
                error: error.clone(),
            });
        }
    }

    if snapshot.macro_guard.blocking {
        if let Some(event) = snapshot.macro_guard.active.as_ref() {
            return Err(TradeRiskReject::MacroEventStandDown {
                label: event.label.clone(),
                phase: event.phase.clone(),
                minutes_until_event: event.minutes_until_event,
            });
        }
    }

    if let Some(fr) = proposal.funding_rate {
        let extreme_threshold = 0.001;
        if fr.abs() > extreme_threshold {
            let funding_bullish = fr < 0.0;
            let signal_bullish = proposal.signal == Signal::BuyYes;
            if signal_bullish != funding_bullish {
                return Err(TradeRiskReject::FundingExtremeReversal {
                    funding_rate: fr,
                    signal: proposal.signal,
                });
            }
        }
    }

    if !proposal.forced
        && !regime_allowlist.is_empty()
        && !regime_allowlist.iter().any(|r| r == &proposal.regime)
    {
        return Err(TradeRiskReject::RegimeNotAllowed {
            regime: proposal.regime.clone(),
        });
    }
    if !proposal.forced && !session_allowlist.is_empty() {
        let session = session_bucket(proposal.entry_hour_utc).to_string();
        if !session_allowlist.iter().any(|allowed| allowed == &session) {
            return Err(TradeRiskReject::SessionNotAllowed { session });
        }
    }
    if !proposal.forced && cfg.blocked_hours_utc.contains(&proposal.entry_hour_utc) {
        return Err(TradeRiskReject::BlockedHourUtc {
            hour_utc: proposal.entry_hour_utc,
        });
    }

    if !proposal.forced {
        if let Some(remaining_secs) = proposal.time_remaining_secs {
            let min_remaining_secs = if proposal.strategy == "scalp" {
                180.0
            } else {
                cfg.expiry_guard_seconds as f64
            };
            if min_remaining_secs > 0.0 && remaining_secs <= min_remaining_secs {
                return Err(TradeRiskReject::TooCloseToExpiry {
                    strategy: proposal.strategy.clone(),
                    remaining_secs,
                    min_remaining_secs,
                });
            }
        }
    }

    let min_confidence = mode_min_confidence(cfg);
    let effective_confidence = proposal.confidence.min(cfg.live_confidence_cap);
    if effective_confidence < min_confidence {
        return Err(TradeRiskReject::ConfidenceTooLow {
            effective_confidence,
            min_confidence,
        });
    }
    let confidence_margin = (effective_confidence - min_confidence).max(0.0);
    if !proposal.forced
        && cfg.min_confidence_margin > 0.0
        && confidence_margin < cfg.min_confidence_margin
    {
        return Err(TradeRiskReject::ConfidenceMarginTooLow {
            effective_confidence,
            min_confidence,
            confidence_margin,
            required_margin: cfg.min_confidence_margin,
        });
    }

    if !proposal.forced && cfg.min_liquidity > 0.0 {
        let liquidity = proposal.liquidity;
        if liquidity > 0.0 && liquidity < cfg.min_liquidity {
            return Err(TradeRiskReject::MinLiquidity {
                liquidity,
                min_liquidity: cfg.min_liquidity,
            });
        }
    }

    if !proposal.hail_mary {
        if proposal.token_price < cfg.min_entry_price || proposal.token_price > cfg.max_entry_price
        {
            return Err(TradeRiskReject::EntryPriceOutOfRange {
                token_price: proposal.token_price,
                min_entry_price: cfg.min_entry_price,
                max_entry_price: cfg.max_entry_price,
            });
        }
    } else if proposal.token_price > cfg.hail_mary_max_price {
        return Err(TradeRiskReject::HailMaryPriceTooHigh {
            token_price: proposal.token_price,
            max_price: cfg.hail_mary_max_price,
        });
    }

    if proposal.hail_mary && cfg.hail_mary_min_btc_change > 0.0 {
        let btc_change_pct = proposal.btc_change_pct.unwrap_or(0.0);
        if btc_change_pct.abs() < cfg.hail_mary_min_btc_change {
            return Err(TradeRiskReject::HailMaryBtcChangeTooSmall {
                btc_change_pct,
                min_btc_change_pct: cfg.hail_mary_min_btc_change,
            });
        }
    }

    let mut approved_amount = proposal.requested_amount;
    if is_below_min_trade_usd(approved_amount, cfg.min_trade_usd) {
        return Err(TradeRiskReject::TradeAmountBelowMinimum {
            amount: approved_amount,
            min_trade_usd: cfg.min_trade_usd,
        });
    }

    let mut canary_capped_from = None;
    if snapshot.apply_live_rollout_guardrails {
        if snapshot.require_shadow_pass && !snapshot.shadow_passed {
            return Err(TradeRiskReject::ShadowPassRequired);
        }
        if snapshot.rollout_stage == "shadow" {
            return Err(TradeRiskReject::ShadowRolloutBlocked);
        }
        if snapshot.rollout_stage == "canary" {
            let cap = snapshot.canary_max_usd.unwrap_or(1.0).max(0.01);
            if approved_amount > cap {
                canary_capped_from = Some(approved_amount);
                approved_amount = cap;
            }
        }
    }

    if snapshot.available_balance < approved_amount {
        return Err(TradeRiskReject::InsufficientBalance {
            available_balance: snapshot.available_balance,
            amount: approved_amount,
        });
    }

    let mut exposure_adjusted_from = None;
    if cfg.max_exposure_pct > 0.0 {
        let max_exposure = snapshot.available_balance * cfg.max_exposure_pct;
        // Include estimated fees in the exposure check so the actual debit
        // (amount + fees) never exceeds the configured cap.
        let fee_fraction = crate::fusion::estimate_taker_fee(proposal.token_price);
        let total_with_fees = approved_amount * (1.0 + fee_fraction);
        if snapshot.current_exposure + total_with_fees > max_exposure {
            let remaining_raw = (max_exposure - snapshot.current_exposure).max(0.0);
            // Back out fee so the amount + its fee stays within the cap.
            let remaining = remaining_raw / (1.0 + fee_fraction);
            if remaining < cfg.min_stake {
                return Err(TradeRiskReject::ExposureCapExceeded {
                    current_exposure: snapshot.current_exposure,
                    amount: approved_amount,
                    max_exposure_pct: cfg.max_exposure_pct,
                    available_balance: snapshot.available_balance,
                });
            }
            exposure_adjusted_from = Some(approved_amount);
            approved_amount = remaining;
        }
    }

    let gas_pct = if approved_amount > 0.0 {
        (cfg.trade_gas_usd / approved_amount).max(0.0)
    } else {
        0.0
    };
    let required_ev = cfg.min_ev + cfg.edge_safety_buffer + gas_pct;
    if proposal.net_ev < required_ev {
        return Err(TradeRiskReject::NetEvBelowRequired {
            net_ev: proposal.net_ev,
            required_ev,
            min_ev: cfg.min_ev,
            edge_safety_buffer: cfg.edge_safety_buffer,
            gas_pct,
        });
    }

    Ok(TradeRiskDecision {
        approved_amount,
        effective_confidence,
        min_confidence,
        gas_pct,
        required_ev,
        canary_capped_from,
        exposure_adjusted_from,
    })
}

pub(crate) fn is_below_min_trade_usd(amount: f64, min_trade_usd: f64) -> bool {
    let epsilon = (min_trade_usd.abs() * 1e-6).max(0.005);
    amount + epsilon < min_trade_usd
}

pub(crate) fn mode_min_confidence(cfg: &Config) -> f64 {
    let base_min_conf = cfg.strategy.params.min_confidence;
    match cfg.mode_kind() {
        BotMode::Simulation => base_min_conf.min(0.62),
        BotMode::Shadow => base_min_conf.min(0.68),
        _ => base_min_conf,
    }
}

fn rollout_stage() -> String {
    std::env::var("MACBOT_ROLLOUT_STAGE")
        .unwrap_or_else(|_| "full".to_string())
        .trim()
        .to_ascii_lowercase()
}

impl TradingBot {
    /// Reset the available-cash peak without changing starting capital, P&L,
    /// daily limits, open positions, or the entry freeze.
    pub(crate) async fn reset_drawdown_cash_peak_direct(&self) -> anyhow::Result<f64> {
        let mode = self.mode_kind().await;
        let balance = self.balance().await;
        Ok(db::risk::reset_cash_peak(&self.db, mode.as_str(), balance)
            .await?
            .peak_balance)
    }
    pub(crate) async fn trade_risk_snapshot(
        &self,
        cfg: &Config,
        use_effective_balance: bool,
    ) -> TradeRiskSnapshot {
        let apply_live_rollout_guardrails =
            use_effective_balance && cfg.mode_kind() == BotMode::Live;
        let rollout_stage = if apply_live_rollout_guardrails {
            rollout_stage()
        } else {
            "full".to_string()
        };
        let canary_max_usd = if apply_live_rollout_guardrails && rollout_stage == "canary" {
            Some(
                std::env::var("MACBOT_CANARY_MAX_USD")
                    .ok()
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(1.0)
                    .max(0.01),
            )
        } else {
            None
        };

        TradeRiskSnapshot {
            today_pnl: self.get_today_pnl_for_mode(cfg.mode_kind()).await,
            available_balance: if use_effective_balance {
                self.effective_balance().await
            } else {
                self.balance().await
            },
            current_exposure: {
                let trades = self.portfolio_state.trades.read().await;
                trades
                    .iter()
                    .filter(|t| t.is_open())
                    .map(|t| t.amount)
                    .sum()
            },
            macro_guard: crate::bot::macro_calendar::macro_guard_status(cfg, chrono::Utc::now()),
            apply_live_rollout_guardrails,
            require_shadow_pass: crate::env_util::env_bool("MACBOT_REQUIRE_SHADOW_PASS", false),
            shadow_passed: crate::env_util::env_bool("MACBOT_SHADOW_PASS", false),
            rollout_stage,
            canary_max_usd,
        }
    }

    /// Returns `true` when either the daily drawdown or the ATH-from-peak
    /// drawdown limit is breached. Returns `false` when both limits are 0
    /// (disabled) or when neither is breached.
    pub async fn is_drawdown_halted(&self) -> bool {
        let cfg = self.control_plane.config.read().await.clone();
        self.is_drawdown_halted_for_config(&cfg).await
    }

    pub(crate) async fn is_drawdown_halted_for_config(&self, cfg: &Config) -> bool {
        if cfg.drawdown_halt_daily_pct <= 0.0 && cfg.drawdown_halt_ath_pct <= 0.0 {
            return false;
        }

        let initial = cfg.initial_balance;
        if initial <= 0.0 {
            return false;
        }

        if cfg.drawdown_halt_daily_pct > 0.0 {
            let today_pnl = self.get_today_pnl_for_mode(cfg.mode_kind()).await;
            let today_dd_pct = (-today_pnl / initial) * 100.0;
            if today_dd_pct >= cfg.drawdown_halt_daily_pct {
                warn!(
                    "Drawdown halt: daily loss {today_dd_pct:.1}% >= limit {:.1}%",
                    cfg.drawdown_halt_daily_pct
                );
                return true;
            }
        }

        if cfg.drawdown_halt_ath_pct > 0.0 {
            let current_bal = self.balance().await;
            let persisted_peak =
                match db::risk::observe_cash_peak(&self.db, cfg.mode_kind().as_str(), current_bal)
                    .await
                {
                    Ok(peak) => peak,
                    Err(error) => {
                        self.set_degraded_no_trade(true);
                        self.halt_trading("risk_drawdown_peak_unavailable", error.to_string())
                            .await;
                        return true;
                    }
                };
            if let Some(peak) = persisted_peak {
                let dd = if peak.peak_balance > 0.0 {
                    (peak.peak_balance - current_bal) / peak.peak_balance * 100.0
                } else {
                    0.0
                };
                return dd >= cfg.drawdown_halt_ath_pct;
            }
            let rows = match sqlx::query_as::<_, db::NonLiveBalanceReplayRow>(
                "SELECT status,amount,CASE WHEN accounting_version=1 THEN 0.0 ELSE fee_paid END AS fee_paid,pnl
                 FROM trades WHERE (LOWER(TRIM(mode))='live')=? ORDER BY id",
            )
            .bind(i64::from(cfg.mode_kind() == BotMode::Live))
            .fetch_all(&self.db)
            .await
            {
                Ok(rows) => rows,
                Err(error) => {
                    self.set_degraded_no_trade(true);
                    self.halt_trading("risk_drawdown_history_unavailable", error.to_string())
                        .await;
                    return true;
                }
            };
            let mut running = initial;
            let mut ath = initial;
            for row in rows.iter() {
                let status = TradeStatus::parse(&row.status);
                if status == TradeStatus::Closed {
                    running += row.pnl.unwrap_or(0.0) - row.fee_paid;
                } else if status == TradeStatus::Open {
                    running -= row.amount + row.fee_paid;
                }
                if running > ath {
                    ath = running;
                }
            }

            let ath = ath.max(current_bal);
            let dd_from_ath = if ath > 0.0 {
                (ath - current_bal) / ath * 100.0
            } else {
                0.0
            };

            if dd_from_ath >= cfg.drawdown_halt_ath_pct {
                warn!(
                    "Drawdown halt: ATH drawdown {dd_from_ath:.1}% >= limit {:.1}%",
                    cfg.drawdown_halt_ath_pct
                );
                return true;
            }
        }

        false
    }
}

#[cfg(test)]
mod tests {

    /// What `/api/trade/force` now relies on: with the hail-mary flag off, a
    /// token at a normal price is judged by the normal entry range, not the
    /// five-cent hail-mary cap, and no BTC move is required.
    #[test]
    fn an_operator_force_is_judged_by_the_normal_rules_not_the_hail_mary_ones() {
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.mode = "simulation".to_string();
        cfg.hail_mary_max_price = 0.05;
        cfg.hail_mary_min_btc_change = 2.0;
        let snapshot = base_snapshot();
        let mut trade = ProposedTrade {
            hail_mary: false,
            token_price: 0.50,
            btc_change_pct: None,
            ..base_trade()
        };
        assert!(
            evaluate_trade_guardrails(&cfg, &snapshot, &trade).is_ok(),
            "a forced entry at 0.50 with no BTC move must pass the normal rules"
        );
        trade.hail_mary = true;
        assert!(
            matches!(
                evaluate_trade_guardrails(&cfg, &snapshot, &trade),
                Err(TradeRiskReject::HailMaryPriceTooHigh { .. })
            ),
            "the same entry as a hail-mary is what the old force path produced"
        );
    }

    use super::{
        evaluate_entry_coordination, evaluate_trade_guardrails, is_below_min_trade_usd,
        EntryCoordinationProposal, EntryCoordinationReject, EntryCoordinationSnapshot,
        ProposedTrade, TradeRiskReject, TradeRiskSnapshot,
    };
    use crate::bot::macro_calendar::MacroGuardStatus;
    use crate::config::Config;
    use crate::db;
    use crate::db::Trade;
    use crate::domain::Signal;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn base_cfg() -> Config {
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.mode = "simulation".to_string();
        cfg.daily_loss_limit = 100.0;
        cfg.min_trade_usd = 0.5;
        cfg.min_stake = 0.5;
        cfg.max_stake = 1_000.0;
        cfg.trade_gas_usd = 0.0;
        cfg.min_ev = 0.0;
        cfg.edge_safety_buffer = 0.0;
        cfg.max_exposure_pct = 0.0;
        cfg.min_liquidity = 0.0;
        cfg.strategy.params.min_confidence = 0.70;
        cfg
    }

    fn base_snapshot() -> TradeRiskSnapshot {
        TradeRiskSnapshot {
            today_pnl: 0.0,
            available_balance: 100.0,
            current_exposure: 0.0,
            macro_guard: MacroGuardStatus::default(),
            apply_live_rollout_guardrails: false,
            require_shadow_pass: false,
            shadow_passed: false,
            rollout_stage: "full".to_string(),
            canary_max_usd: None,
        }
    }

    fn base_trade() -> ProposedTrade {
        ProposedTrade {
            signal: Signal::BuyYes,
            strategy: "conviction".to_string(),
            confidence: 0.80,
            funding_rate: None,
            regime: "TREND_UP".to_string(),
            token_price: 0.55,
            liquidity: 10_000.0,
            entry_hour_utc: 12,
            time_remaining_secs: Some(900.0),
            btc_change_pct: Some(1.0),
            net_ev: 0.10,
            forced: false,
            hail_mary: false,
            requested_amount: 10.0,
        }
    }

    #[test]
    fn min_trade_threshold_tolerates_tiny_float_noise() {
        assert!(!is_below_min_trade_usd(0.9999995, 1.0));
        assert!(!is_below_min_trade_usd(0.995, 1.0));
        assert!(is_below_min_trade_usd(0.994, 1.0));
    }

    #[test]
    fn macro_event_guard_blocks_entries() {
        let cfg = base_cfg();
        let mut snapshot = base_snapshot();
        snapshot.macro_guard = MacroGuardStatus {
            enabled: true,
            blocking: true,
            source_path: "ops/macro-events.json".to_string(),
            error: None,
            guard_minutes_before: 15,
            guard_minutes_after: 5,
            active: Some(crate::bot::macro_calendar::MacroEventWindow {
                label: "CPI".to_string(),
                kind: "cpi".to_string(),
                timestamp: "2026-03-21T12:30:00Z".to_string(),
                source: "test".to_string(),
                phase: "pre_event".to_string(),
                window_start: "2026-03-21T12:15:00Z".to_string(),
                window_end: "2026-03-21T12:35:00Z".to_string(),
                secs_until_event: 300,
                minutes_until_event: 5.0,
            }),
            next: None,
            refreshed_at: None,
            valid_until: None,
        };

        let reject = evaluate_trade_guardrails(&cfg, &snapshot, &base_trade()).unwrap_err();
        assert!(matches!(
            reject,
            TradeRiskReject::MacroEventStandDown { label, .. } if label == "CPI"
        ));
    }

    #[test]
    fn enabled_macro_guard_cannot_silently_pass_a_missing_calendar() {
        let mut cfg = base_cfg();
        cfg.macro_event_guard_enabled = true;
        let mut snapshot = base_snapshot();
        snapshot.macro_guard.error = Some("calendar is unavailable".into());
        assert!(matches!(
            evaluate_trade_guardrails(&cfg, &snapshot, &base_trade()),
            Err(TradeRiskReject::MacroCalendarUnavailable { .. })
        ));
        cfg.macro_event_guard_enabled = false;
        assert!(evaluate_trade_guardrails(&cfg, &snapshot, &base_trade()).is_ok());
    }

    fn base_entry_snapshot() -> EntryCoordinationSnapshot {
        EntryCoordinationSnapshot {
            has_same_market_strategy_position: false,
            open_scalps: 0,
            open_conviction: 0,
        }
    }

    fn base_entry_proposal() -> EntryCoordinationProposal {
        EntryCoordinationProposal {
            signal: Signal::BuyYes,
            strategy: "conviction".to_string(),
            market_id: "mkt-1".to_string(),
            market_yes_price: 0.55,
            token_price: 0.55,
            p_up: Some(0.75),
            liquidity: 10_000.0,
        }
    }

    fn trade(
        id: i64,
        status: &str,
        amount: f64,
        fee_paid: f64,
        pnl: Option<f64>,
        exit_timestamp: Option<String>,
    ) -> Trade {
        Trade {
            id,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            market_id: "m".to_string(),
            question: "q".to_string(),
            direction: "BUY_YES".to_string(),
            entry_price: 0.5,
            amount,
            shares: 2.0,
            expected_value: 0.0,
            reason: "".to_string(),
            confidence: 0.6,
            status: status.to_string(),
            mode: "simulation".to_string(),
            source: "bot".to_string(),
            end_date: "".to_string(),
            exit_price: None,
            exit_timestamp,
            pnl,
            exit_type: None,
            was_correct: None,
            forced: None,
            partial_closed: Some(0),
            raw_confidence: None,
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid,
            accounting_version: 0,
            signals_json: Some(json!({"orderbook_imbalance": 0.5}).to_string()),
            calibration_version: Some("test-cal-v1".to_string()),
            run_id: Some("test-run".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    async fn test_bot(
        name: &str,
        cfg: Config,
    ) -> anyhow::Result<(
        Arc<crate::bot::state::TradingBot>,
        std::path::PathBuf,
        String,
    )> {
        let uniq = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_risk_{name}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;
        let bot = Arc::new(crate::bot::state::TradingBot::new(cfg, pool).await?);
        Ok((bot, path, path_str))
    }

    #[test]
    fn entry_coordination_rejects_duplicate_strategy_position() {
        let cfg = base_cfg();
        let mut snapshot = base_entry_snapshot();
        snapshot.has_same_market_strategy_position = true;

        let reject = evaluate_entry_coordination(&cfg, &snapshot, &base_entry_proposal())
            .expect_err("duplicate position should be blocked");
        assert_eq!(
            reject,
            EntryCoordinationReject::DuplicateStrategyPosition {
                market_id: "mkt-1".to_string(),
                strategy: "conviction".to_string(),
            }
        );
    }

    #[test]
    fn entry_coordination_rejects_weak_scalp_momentum() {
        let cfg = base_cfg();
        let proposal = EntryCoordinationProposal {
            strategy: "scalp".to_string(),
            p_up: Some(0.55),
            ..base_entry_proposal()
        };

        let reject = evaluate_entry_coordination(&cfg, &base_entry_snapshot(), &proposal)
            .expect_err("weak scalp momentum should be blocked");
        assert_eq!(
            reject,
            EntryCoordinationReject::ScalpMomentumTooWeak {
                signal: Signal::BuyYes,
                p_up: 0.55,
            }
        );
    }

    #[test]
    fn entry_coordination_rejects_conviction_position_cap() {
        let mut cfg = base_cfg();
        cfg.max_open_positions = 2;
        let mut snapshot = base_entry_snapshot();
        snapshot.open_conviction = 2;

        let reject = evaluate_entry_coordination(&cfg, &snapshot, &base_entry_proposal())
            .expect_err("conviction cap should be enforced");
        assert_eq!(
            reject,
            EntryCoordinationReject::ConvictionPositionCap {
                open_conviction: 2,
                max_open_positions: 2,
            }
        );
    }

    #[test]
    fn entry_coordination_rejects_scalp_depth_below_floor() {
        let cfg = base_cfg();
        let proposal = EntryCoordinationProposal {
            strategy: "scalp".to_string(),
            liquidity: 300.0,
            ..base_entry_proposal()
        };

        let reject = evaluate_entry_coordination(&cfg, &base_entry_snapshot(), &proposal)
            .expect_err("thin scalp book should be blocked");
        assert_eq!(
            reject,
            EntryCoordinationReject::ScalpLiquidityTooLow {
                liquidity: 300.0,
                min_liquidity: 500.0,
            }
        );
    }

    fn cleanup_db(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    #[test]
    fn risk_evaluator_caps_exposure_to_remaining_room() {
        let mut cfg = base_cfg();
        cfg.max_exposure_pct = 0.50;

        let snapshot = TradeRiskSnapshot {
            current_exposure: 40.0,
            ..base_snapshot()
        };
        let mut trade = base_trade();
        trade.requested_amount = 20.0;

        let decision = evaluate_trade_guardrails(&cfg, &snapshot, &trade).unwrap();
        // The remaining room (10.0) is divided by (1 + fee) so that
        // amount + fees stays within the exposure cap.
        let fee = crate::fusion::estimate_taker_fee(trade.token_price);
        let expected = 10.0 / (1.0 + fee);
        assert!(
            (decision.approved_amount - expected).abs() < 1e-6,
            "approved_amount={} expected={}",
            decision.approved_amount,
            expected
        );
        assert_eq!(decision.exposure_adjusted_from, Some(20.0));
    }

    #[test]
    fn risk_evaluator_rejects_when_exposure_remaining_is_below_min_stake() {
        let mut cfg = base_cfg();
        cfg.max_exposure_pct = 0.50;
        cfg.min_stake = 6.0;

        let snapshot = TradeRiskSnapshot {
            current_exposure: 45.0,
            ..base_snapshot()
        };
        let trade = base_trade();

        let reject = evaluate_trade_guardrails(&cfg, &snapshot, &trade).unwrap_err();
        assert!(matches!(
            reject,
            TradeRiskReject::ExposureCapExceeded { .. }
        ));
    }

    #[test]
    fn risk_evaluator_blocks_live_shadow_rollout_without_shadow_pass() {
        let mut cfg = base_cfg();
        cfg.mode = "live".to_string();

        let snapshot = TradeRiskSnapshot {
            apply_live_rollout_guardrails: true,
            require_shadow_pass: true,
            shadow_passed: false,
            rollout_stage: "shadow".to_string(),
            ..base_snapshot()
        };
        let trade = base_trade();

        let reject = evaluate_trade_guardrails(&cfg, &snapshot, &trade).unwrap_err();
        assert_eq!(reject, TradeRiskReject::ShadowPassRequired);
    }

    #[test]
    fn risk_evaluator_caps_canary_notional() {
        let cfg = base_cfg();
        let snapshot = TradeRiskSnapshot {
            apply_live_rollout_guardrails: true,
            rollout_stage: "canary".to_string(),
            canary_max_usd: Some(1.25),
            ..base_snapshot()
        };
        let trade = base_trade();

        let decision = evaluate_trade_guardrails(&cfg, &snapshot, &trade).unwrap();
        assert!((decision.approved_amount - 1.25).abs() < 1e-9);
        assert_eq!(decision.canary_capped_from, Some(10.0));
    }

    #[test]
    fn risk_evaluator_rejects_when_ev_falls_below_required_threshold() {
        let mut cfg = base_cfg();
        cfg.trade_gas_usd = 1.0;
        cfg.min_ev = 0.05;
        cfg.edge_safety_buffer = 0.02;

        let mut trade = base_trade();
        trade.net_ev = 0.10;
        trade.requested_amount = 5.0;

        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(matches!(reject, TradeRiskReject::NetEvBelowRequired { .. }));
    }

    #[test]
    fn risk_evaluator_blocks_conviction_entries_too_close_to_expiry() {
        let mut cfg = base_cfg();
        cfg.expiry_guard_seconds = 120;

        let mut trade = base_trade();
        trade.strategy = "conviction".to_string();
        trade.time_remaining_secs = Some(60.0);

        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(matches!(reject, TradeRiskReject::TooCloseToExpiry { .. }));
    }

    #[test]
    fn risk_evaluator_blocks_scalp_entries_in_final_three_minutes() {
        let cfg = base_cfg();

        let mut trade = base_trade();
        trade.strategy = "scalp".to_string();
        trade.time_remaining_secs = Some(150.0);

        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(matches!(reject, TradeRiskReject::TooCloseToExpiry { .. }));
    }

    #[test]
    fn risk_evaluator_blocks_session_not_in_allowlist() {
        let mut cfg = base_cfg();
        cfg.session_allowlist = vec!["us".to_string()];

        let mut trade = base_trade();
        trade.entry_hour_utc = 2;

        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(
            matches!(reject, TradeRiskReject::SessionNotAllowed { session } if session == "asia")
        );
    }

    #[test]
    fn maker_trade_does_not_inherit_conviction_regime_or_session_allowlists() {
        let mut cfg = base_cfg();
        cfg.regime_allowlist = vec!["TREND_UP".to_string()];
        cfg.session_allowlist = vec!["us".to_string()];

        let mut trade = base_trade();
        trade.strategy = "maker".to_string();
        trade.regime = "SQUEEZE".to_string();
        trade.entry_hour_utc = 2;

        let decision = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap();
        assert!((decision.approved_amount - trade.requested_amount).abs() < 1e-9);
    }

    #[test]
    fn maker_trade_respects_maker_specific_regime_allowlist() {
        let mut cfg = base_cfg();
        cfg.maker_regime_allowlist = vec!["TREND_UP".to_string()];

        let mut trade = base_trade();
        trade.strategy = "maker".to_string();
        trade.regime = "SQUEEZE".to_string();

        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(matches!(
            reject,
            TradeRiskReject::RegimeNotAllowed { regime } if regime == "SQUEEZE"
        ));
    }

    #[test]
    fn maker_trade_respects_maker_specific_session_allowlist() {
        let mut cfg = base_cfg();
        cfg.maker_session_allowlist = vec!["us".to_string()];

        let mut trade = base_trade();
        trade.strategy = "maker".to_string();
        trade.entry_hour_utc = 2;

        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(
            matches!(reject, TradeRiskReject::SessionNotAllowed { session } if session == "asia")
        );
    }

    #[test]
    fn risk_evaluator_blocks_explicit_utc_hour() {
        let mut cfg = base_cfg();
        cfg.blocked_hours_utc = vec![12];

        let trade = base_trade();
        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(matches!(
            reject,
            TradeRiskReject::BlockedHourUtc { hour_utc: 12 }
        ));
    }

    #[test]
    fn risk_evaluator_blocks_when_confidence_margin_is_too_thin() {
        let mut cfg = base_cfg();
        cfg.min_confidence_margin = 0.25;

        let trade = base_trade();
        let reject = evaluate_trade_guardrails(&cfg, &base_snapshot(), &trade).unwrap_err();
        assert!(matches!(
            reject,
            TradeRiskReject::ConfidenceMarginTooLow {
                required_margin,
                ..
            } if (required_margin - 0.25).abs() < 1e-9
        ));
    }

    #[tokio::test]
    async fn daily_guard_blocks_realized_loss_on_still_open_partial_position() -> anyhow::Result<()>
    {
        let mut cfg = base_cfg();
        cfg.daily_loss_limit = 1.0;
        let (bot, path, path_str) = test_bot("partial_daily_loss", cfg.clone()).await?;
        let mut original = trade(0, "OPEN", 5.0, 0.0, None, None);
        original.shares = 10.0;
        original.timestamp = chrono::Utc::now().to_rfc3339();
        original.id = db::insert_trade(&bot.db, &original).await?;
        let mut updated = original.clone();
        updated.shares = 6.0;
        updated.amount = 3.0;
        updated.pnl = Some(-1.2);
        updated.partial_closed = Some(1);
        db::paper::apply_partial_exit(
            &bot.db,
            &original,
            &updated,
            "daily-risk-sale",
            0.2,
            0.2,
            4.0,
            "fixture",
            "{}",
        )
        .await?;
        let mut snapshot = base_snapshot();
        snapshot.today_pnl = bot.get_today_pnl().await;
        assert!((snapshot.today_pnl + 1.2).abs() < 1e-9);
        assert!(matches!(
            evaluate_trade_guardrails(&cfg, &snapshot, &base_trade()),
            Err(TradeRiskReject::DailyLossLimit { .. })
        ));
        assert!(db::load_trade_by_id(&bot.db, original.id)
            .await?
            .unwrap()
            .is_open());
        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn undated_partial_realization_fails_closed_in_runtime_risk() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("undated_daily_loss", base_cfg()).await?;
        let mut partial = trade(0, "OPEN", 2.5, 0.0, Some(3.0), None);
        partial.shares = 5.0;
        partial.partial_closed = Some(1);
        db::insert_trade(&bot.db, &partial).await?;
        assert_eq!(bot.get_today_pnl().await, f64::NEG_INFINITY);
        assert!(bot.kill_switch_active());
        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn drawdown_halt_trips_on_daily_loss_limit() -> anyhow::Result<()> {
        let mut cfg = base_cfg();
        cfg.drawdown_halt_daily_pct = 10.0;
        cfg.drawdown_halt_ath_pct = 0.0;
        cfg.initial_balance = 100.0;

        let (bot, path, path_str) = test_bot("daily_dd", cfg.clone()).await?;
        let closed = trade(
            1,
            "CLOSED",
            0.0,
            0.0,
            Some(-15.0),
            Some(chrono::Utc::now().to_rfc3339()),
        );
        let _ = db::insert_trade(&bot.db, &closed).await?;

        assert!(bot.is_drawdown_halted().await);

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn live_admission_uses_live_losses_even_when_simulation_wins() -> anyhow::Result<()> {
        let mut cfg = base_cfg();
        cfg.daily_loss_limit = 50.0;
        cfg.initial_balance = 100.0;
        cfg.drawdown_halt_daily_pct = 10.0;
        cfg.drawdown_halt_ath_pct = 0.0;
        let (bot, path, path_str) = test_bot("capital_origin", cfg.clone()).await?;
        for (mode, pnl) in [("simulation", 100.0), ("live", -60.0)] {
            let mut closed = trade(
                0,
                "CLOSED",
                0.0,
                0.0,
                Some(pnl),
                Some(chrono::Utc::now().to_rfc3339()),
            );
            closed.mode = mode.into();
            db::insert_trade(&bot.db, &closed).await?;
        }
        cfg.mode = "live".into();
        let snapshot = bot.trade_risk_snapshot(&cfg, false).await;
        assert_eq!(snapshot.today_pnl, -60.0);
        assert!(matches!(
            evaluate_trade_guardrails(&cfg, &snapshot, &base_trade()),
            Err(TradeRiskReject::DailyLossLimit { .. })
        ));
        *bot.control_plane.config.write().await = cfg;
        assert_eq!(bot.get_today_pnl().await, -60.0);
        assert!(bot.is_drawdown_halted().await);
        bot.control_plane.config.write().await.mode = "simulation".into();
        assert_eq!(bot.get_today_pnl().await, 100.0);
        assert!(!bot.is_drawdown_halted().await);
        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn drawdown_halt_trips_on_ath_drawdown() -> anyhow::Result<()> {
        let mut cfg = base_cfg();
        cfg.drawdown_halt_daily_pct = 0.0;
        cfg.drawdown_halt_ath_pct = 20.0;
        cfg.initial_balance = 100.0;

        let path_uniq = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_risk_ath_{path_uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let closed = trade(
            1,
            "CLOSED",
            0.0,
            0.0,
            Some(50.0),
            Some(chrono::Utc::now().to_rfc3339()),
        );
        let _ = db::insert_trade(&pool, &closed).await?;

        let bot = Arc::new(crate::bot::state::TradingBot::new(cfg, pool).await?);
        bot.set_balance(100.0).await;

        assert!(bot.is_drawdown_halted().await);

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }
}
