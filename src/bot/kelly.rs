//! Kelly criterion sizing + balance helpers.
//!
//! sync_balance()       — authenticated funding-wallet collateral refresh/read
//! effective_balance()  — balance minus locked collateral in open live orders
//! kelly_size()         — full sizing pipeline (stake_pct → Kelly mult → fraction cap → CB)
//! get_today_pnl()      — exact UTC-day realized PnL, including partial sales
//! get_live_win_rate()  — win-rate of closed live trades

use chrono::Utc;
use tracing::{info, warn};

use super::state::TradingBot;
use crate::db;
use crate::domain::BotMode;

impl TradingBot {
    // ── Balance sync ──────────────────────────────────────────────────────────

    /// Refresh and read the venue's spendable collateral for the authenticated
    /// funding wallet. A failed read cannot establish a new trading budget.
    pub async fn sync_balance(&self) -> Option<f64> {
        self.sync_balance_with_read_deadline(None)
            .await
            .ok()
            .flatten()
    }

    pub(crate) async fn sync_balance_before(
        &self,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<Option<f64>> {
        self.sync_balance_with_read_deadline(Some(deadline)).await
    }

    async fn sync_balance_with_read_deadline(
        &self,
        deadline: Option<tokio::time::Instant>,
    ) -> anyhow::Result<Option<f64>> {
        use crate::clob::{AssetType, BalanceAllowanceParams};
        let Some(clob) = self.clob.as_ref() else {
            return Ok(None);
        };
        let params = BalanceAllowanceParams {
            asset_type: AssetType::Collateral,
            token_id: None,
        };
        let read = async {
            if let Err(error) = clob.update_balance_allowance(&params).await {
                warn!("CLOB collateral refresh failed: {error}");
                return None;
            }
            let response = match clob.get_balance_allowance(&params).await {
                Ok(response) => response,
                Err(error) => {
                    warn!("CLOB collateral balance read failed: {error}");
                    return None;
                }
            };
            let balance = response.get("balance").and_then(collateral_balance_units);
            if balance.is_none() {
                warn!("CLOB collateral balance lacks valid nonnegative base units");
            }
            balance
        };
        let balance = match deadline {
            Some(deadline) => {
                super::live_orders::before_recovery_read_deadline(
                    deadline,
                    "collateral balance",
                    async { Ok(read.await) },
                )
                .await?
            }
            None => read.await,
        };
        let Some(balance) = balance else {
            return Ok(None);
        };
        // A valid completed read publishes outside the cancellable timer.
        self.set_balance(balance).await;
        info!(
            "Balance synced for funding wallet {}: ${balance:.2}",
            clob.account_address()
        );
        Ok(Some(balance))
    }

    // ── Effective balance ─────────────────────────────────────────────────────

    /// Balance minus pending taker and maker BUY collateral reservations.
    pub async fn effective_balance(&self) -> f64 {
        let bal = self.balance().await;
        let locked: f64 = self
            .portfolio_state
            .trades
            .read()
            .await
            .iter()
            .filter(|t| t.is_open() && t.is_live() && t.has_order())
            .map(|t| t.locked_collateral())
            .sum();
        let maker_reserved = match db::maker::reserved_buy_collateral(&self.db).await {
            Ok(reserved) => reserved,
            Err(error) => {
                self.set_degraded_no_trade(true);
                self.halt_trading(
                    "maker_collateral_reservation_unavailable",
                    error.to_string(),
                )
                .await;
                return 0.0;
            }
        };
        if !bal.is_finite() || bal < 0.0 || !locked.is_finite() || locked < 0.0 {
            self.set_degraded_no_trade(true);
            self.halt_trading(
                "live_budget_invalid",
                "live collateral budget is not finite and nonnegative",
            )
            .await;
            return 0.0;
        }
        (bal - locked - maker_reserved).max(0.0)
    }

    // ── Kelly sizing ──────────────────────────────────────────────────────────

    /// Compute the trade size to use, respecting:
    /// 1. stake_pct-based sizing (if configured)
    /// 2. Kelly multiplier (if kelly_sizing enabled)
    /// 3. Kelly fraction cap
    /// 4. Circuit-breaker halving after N consecutive losses
    /// 5. Consecutive same-direction haircut (3+=0.85x, 4=0.70x, 5+=0.50x)
    pub async fn kelly_size(
        &self,
        base_amount: f64,
        confidence: f64,
        override_amount: Option<f64>,
        direction: &str,
    ) -> f64 {
        let cfg = self.control_plane.config.read().await.clone();
        // Override sizing deliberately bypasses portfolio reads and haircuts.
        let mut state = KellySizingInput {
            base_amount,
            confidence,
            override_amount,
            effective_balance: 0.0,
            consecutive_losses: 0,
            correlation_multiplier: 1.0,
            consecutive_direction: 0,
        };
        if override_amount.is_none() {
            state.effective_balance = if cfg.mode_kind() == BotMode::Live {
                self.effective_balance().await
            } else {
                self.balance().await
            };
            if cfg.circuit_breaker_losses > 0 {
                state.consecutive_losses =
                    self.count_recent_consecutive_losses(cfg.mode_kind()).await;
            }
            state.correlation_multiplier = self
                .signal_trackers
                .correlation_tracker
                .lock()
                .await
                .position_size_multiplier();
            if !direction.is_empty() && direction != "HOLD" {
                state.consecutive_direction = self
                    .count_consecutive_same_direction(direction, cfg.mode_kind())
                    .await;
            }
        }
        let decision = evaluate_kelly_size(&cfg, &state);
        crate::replay::capture(
            "kelly_sizing",
            || {
                serde_json::json!({
                    "config": cfg.redacted_value(), "state": state
                })
            },
            &decision,
        );
        // Side effects stay in the live adapter; replay only computes the plan.
        if decision.activate_cooldown {
            let mut pause = self
                .trade_lifecycle
                .circuit_breaker_pause_until
                .write()
                .await;
            if pause.is_none() {
                *pause = Some(chrono::Utc::now() + chrono::Duration::minutes(30));
                let losses = state.consecutive_losses;
                warn!("Circuit breaker COOLDOWN: {losses} consecutive losses — pausing 30 minutes");
                tokio::spawn(async move {
                    crate::notifications::notify_circuit_breaker(losses, 30).await;
                });
            }
        }
        if decision.halvings > 0 {
            warn!(
                losses = state.consecutive_losses,
                halvings = decision.halvings,
                amount = decision.amount,
                "Circuit breaker sizing applied"
            );
        }
        if decision.direction_haircut < 1.0 {
            warn!(
                direction,
                consecutive = state.consecutive_direction,
                amount = decision.amount,
                "Direction bias sizing haircut applied"
            );
        }
        decision.amount
    }

    async fn count_recent_consecutive_losses(&self, mode: BotMode) -> usize {
        let mut count = 0usize;
        let closed = match db::risk::recent_closed_trades_for_origin(
            &self.db,
            512,
            db::risk::CapitalOrigin::for_mode(mode),
        )
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                self.set_degraded_no_trade(true);
                self.halt_trading("risk_loss_history_unavailable", error.to_string())
                    .await;
                return 0;
            }
        };
        for t in closed.iter().rev() {
            match t.pnl {
                Some(_) if t.net_realized_pnl() < 0.0 => count += 1,
                // Stop on any non-loss: a win breaks the streak, and None
                // (unfilled/cancelled) should not inflate the loss count.
                _ => break,
            }
        }
        count
    }

    /// Count how many of the most recent closed trades share the same direction
    /// as `proposed_direction` (e.g. "BUY_YES" or "BUY_NO"), walking backwards
    /// from the newest trade.
    async fn count_consecutive_same_direction(
        &self,
        proposed_direction: &str,
        mode: BotMode,
    ) -> usize {
        let mut count = 0usize;
        let closed = match db::risk::recent_closed_trades_for_origin(
            &self.db,
            64,
            db::risk::CapitalOrigin::for_mode(mode),
        )
        .await
        {
            Ok(rows) => rows,
            Err(error) => {
                self.set_degraded_no_trade(true);
                self.halt_trading("risk_direction_history_unavailable", error.to_string())
                    .await;
                return 0;
            }
        };
        let norm = proposed_direction.trim().to_ascii_uppercase();
        for t in closed.iter().rev() {
            if t.direction.trim().to_ascii_uppercase() == norm {
                count += 1;
            } else {
                break;
            }
        }
        count
    }

    // ── Today's PnL ───────────────────────────────────────────────────────────

    pub async fn get_today_pnl(&self) -> f64 {
        self.get_today_pnl_for_mode(self.mode_kind().await).await
    }

    pub(crate) async fn get_today_pnl_for_mode(&self, mode: BotMode) -> f64 {
        let result = db::risk::daily_realized_pnl_for_origin(
            &self.db,
            Utc::now().date_naive(),
            db::risk::CapitalOrigin::for_mode(mode),
        )
        .await;
        match result {
            Ok(snapshot) if snapshot.evidence_complete => {
                snapshot.exact_today_pnl.unwrap_or(f64::NEG_INFINITY)
            }
            outcome => {
                self.set_degraded_no_trade(true);
                let detail = match outcome {
                    Ok(snapshot) => format!("daily realized P&L has undated partial sales for trades {:?}; conservative undated loss ${:.4}", snapshot.unallocated_trade_ids, snapshot.conservative_unallocated_loss),
                    Err(error) => error.to_string(),
                };
                self.halt_trading("risk_daily_pnl_unavailable", detail)
                    .await;
                // A missing loss total can never establish permission to enter.
                f64::NEG_INFINITY
            }
        }
    }
    // ── Live win-rate ─────────────────────────────────────────────────────────

    pub async fn get_live_win_rate(&self) -> f64 {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM trades WHERE status='CLOSED' AND mode='live'")
                .fetch_one(&self.db)
                .await
                .unwrap_or(0);
        if n <= 0 {
            return 0.0;
        }
        let wins: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM trades WHERE status='CLOSED' AND mode='live' AND (COALESCE(pnl,0.0) - CASE WHEN accounting_version=1 THEN 0.0 ELSE COALESCE(fee_paid,0.0) END) > 0.0",
        )
        .fetch_one(&self.db)
        .await
        .unwrap_or(0);
        wins.max(0) as f64 / n.max(1) as f64
    }
}

fn collateral_balance_units(value: &serde_json::Value) -> Option<f64> {
    // Six-decimal base units must be integral and exact before conversion.
    // Beyond 2^53-1, a floating-point ledger cannot preserve those units.
    const MAX_EXACT_UNITS: u128 = (1_u128 << 53) - 1;
    let units = if let Some(raw) = value.as_str() {
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        raw.parse::<u128>().ok()?
    } else {
        u128::from(value.as_u64()?)
    };
    (units <= MAX_EXACT_UNITS).then_some(units as f64 / 1_000_000.0)
}

#[cfg(test)]
mod balance_sync_tests {
    use super::*;
    use axum::{extract::State, http::StatusCode, response::IntoResponse, Json, Router};
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct Venue {
        balance: Value,
        refresh_ok: bool,
        signer: String,
        requests: Arc<Mutex<Vec<String>>>,
        delay_ms: Arc<std::sync::atomic::AtomicU64>,
    }

    async fn reply(
        State(venue): State<Venue>,
        request: axum::extract::Request,
    ) -> axum::response::Response {
        let path = request.uri().path().to_string();
        assert_eq!(request.headers()["POLY_ADDRESS"], venue.signer);
        assert_eq!(
            request.uri().query(),
            Some("asset_type=COLLATERAL&signature_type=2")
        );
        venue.requests.lock().unwrap().push(path.clone());
        let delay = venue.delay_ms.load(std::sync::atomic::Ordering::Relaxed);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        match path.as_str() {
            "/balance-allowance/update" if venue.refresh_ok => Json(json!({})).into_response(),
            "/balance-allowance/update" => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            "/balance-allowance" => Json(json!({"balance":venue.balance})).into_response(),
            _ => StatusCode::NOT_FOUND.into_response(),
        }
    }

    async fn fixture(
        balance: Value,
        refresh_ok: bool,
    ) -> (TradingBot, Venue, tokio::task::JoinHandle<()>) {
        let auth = crate::clob::ClobAuth::new_wallet(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "public-api-key",
            "cHVibGljLWZpeHR1cmUtc2VjcmV0",
            "public-passphrase",
            2,
            Some("0x1111111111111111111111111111111111111111"),
        )
        .unwrap();
        let venue = Venue {
            balance,
            refresh_ok,
            signer: auth.signer_address(),
            requests: Arc::new(Mutex::new(Vec::new())),
            delay_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(reply).with_state(venue.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json")).unwrap();
        config.mode = "simulation".into();
        config.initial_balance = 123.0;
        let pool = db::open(":memory:").await.unwrap();
        let mut bot = TradingBot::new(config, pool).await.unwrap();
        bot.clob = Some(Arc::new(crate::clob::ClobClient::with_test_host(
            auth, host,
        )));
        (bot, venue, task)
    }

    #[test]
    fn balance_units_require_exact_nonnegative_integer_micro_units() {
        assert_eq!(collateral_balance_units(&json!("1234567")), Some(1.234567));
        assert_eq!(collateral_balance_units(&json!(0)), Some(0.0));
        for value in [
            json!(null),
            json!(true),
            json!(-1),
            json!(1.25),
            json!("-1"),
            json!("NaN"),
            json!("inf"),
            json!("1.5"),
            json!(""),
            json!("9007199254740992"),
        ] {
            assert_eq!(collateral_balance_units(&value), None, "{value}");
        }
    }

    #[tokio::test]
    async fn funding_wallet_balance_is_read_after_refresh_including_zero() {
        for (raw, expected) in [(json!("1234567"), 1.234567), (json!("0"), 0.0)] {
            let (bot, venue, task) = fixture(raw, true).await;
            assert_eq!(bot.sync_balance().await, Some(expected));
            assert_eq!(bot.balance().await, expected);
            assert_eq!(
                *venue.requests.lock().unwrap(),
                ["/balance-allowance/update", "/balance-allowance"]
            );
            assert_eq!(
                bot.clob.as_ref().unwrap().account_address(),
                "0x1111111111111111111111111111111111111111"
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn expired_or_stalled_balance_reads_never_publish_a_new_cash_budget() {
        let (bot, venue, task) = fixture(json!("999999999"), true).await;
        assert!(bot
            .sync_balance_before(tokio::time::Instant::now())
            .await
            .is_err());
        assert!(venue.requests.lock().unwrap().is_empty());
        assert_eq!(bot.balance().await, 123.0);
        venue
            .delay_ms
            .store(500, std::sync::atomic::Ordering::Relaxed);
        assert!(bot
            .sync_balance_before(tokio::time::Instant::now() + std::time::Duration::from_millis(30))
            .await
            .is_err());
        assert_eq!(bot.balance().await, 123.0);
        assert_eq!(venue.requests.lock().unwrap().len(), 1);
        task.abort();
    }

    #[tokio::test]
    async fn completed_balance_read_publishes_cash_even_if_the_read_deadline_later_expires() {
        let (bot, venue, task) = fixture(json!("1234567"), true).await;
        let bot = Arc::new(bot);
        let held_cash = bot.portfolio_state.balance.write().await;
        let sync_bot = bot.clone();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(200);
        let sync = tokio::spawn(async move { sync_bot.sync_balance_before(deadline).await });
        while venue.requests.lock().unwrap().len() < 2 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "fixture balance reads stalled"
            );
            tokio::task::yield_now().await;
        }
        // The valid read is ready, but publication waits for the held cash
        // lease. The read timer must not cancel this separate state update.
        tokio::time::sleep_until(deadline + std::time::Duration::from_millis(20)).await;
        assert!(!sync.is_finished());
        drop(held_cash);
        assert_eq!(sync.await.unwrap().unwrap(), Some(1.234567));
        assert_eq!(bot.balance().await, 1.234567);
        task.abort();
    }

    #[tokio::test]
    async fn invalid_getter_balance_cannot_replace_current_balance_or_use_another_source() {
        let (bot, venue, task) = fixture(json!("NaN"), true).await;
        assert_eq!(bot.sync_balance().await, None);
        assert_eq!(bot.balance().await, 123.0);
        assert_eq!(venue.requests.lock().unwrap().len(), 2);
        task.abort();
    }

    #[tokio::test]
    async fn refresh_failure_cannot_read_a_stale_budget() {
        let (bot, venue, task) = fixture(json!("999999999"), false).await;
        assert_eq!(bot.sync_balance().await, None);
        assert_eq!(bot.balance().await, 123.0);
        assert_eq!(
            *venue.requests.lock().unwrap(),
            ["/balance-allowance/update"]
        );
        task.abort();
    }

    #[tokio::test]
    async fn pending_maker_bids_and_taker_intents_reserve_cash_without_relocking_filled_cost() {
        let (bot, _, task) = fixture(json!("123000000"), true).await;
        let auth = crate::clob::ClobAuth::new(
            "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "public-api-key",
            "cHVibGljLWZpeHR1cmUtc2VjcmV0",
            "public-passphrase",
        )
        .unwrap();
        let signed = auth
            .sign_order_v2("123", 5_000_000, 10_000_000, 0, 7, 1717171717000, false)
            .unwrap();
        let template = db::Trade {
            market_id: "reserved-maker".into(),
            mode: "live".into(),
            direction: "BUY_YES".into(),
            token_id: Some("123".into()),
            ..db::Trade::default()
        };
        let order = db::maker::MakerOrder {
            order_id: signed.order_id().into(),
            market_id: template.market_id.clone(),
            token_id: "123".into(),
            side: "BUY".into(),
            price: 0.5,
            size: 10.0,
            confirmed_size: 0.0,
            state: "SUBMITTING".into(),
            template_json: serde_json::to_string(&template).unwrap(),
            settlement_json: serde_json::to_string(
                &crate::clob::settlement::SettlementOrderIdentity::from(&signed),
            )
            .unwrap(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        db::maker::register_order(&bot.db, &order).await.unwrap();
        db::maker::apply_confirmed_match(
            &bot.db,
            &db::maker::MakerMatch {
                match_id: "confirmed-receipt:1".into(),
                order_id: order.order_id.clone(),
                token_id: "123".into(),
                side: "BUY".into(),
                size: 2.0,
                price: 0.4,
                fee_usd: 0.0,
                timestamp: Utc::now().to_rfc3339(),
            },
        )
        .await
        .unwrap();
        // The venue wallet balance already reflects .80 spent on two shares.
        bot.set_balance(122.2).await;
        bot.portfolio_state.trades.write().await.push(db::Trade {
            mode: "live".into(),
            status: "OPEN".into(),
            amount: 2.0,
            order_id: Some("pending-taker".into()),
            ..db::Trade::default()
        });
        assert!(
            (bot.effective_balance().await - 116.2).abs() < 1e-9,
            "reserve only 8*.5 remaining maker cash plus2 pending taker cash"
        );
        db::maker::update_state(&bot.db, &order.order_id, "TERMINAL")
            .await
            .unwrap();
        let available = bot.effective_balance().await;
        assert!(
            (available - 120.2).abs() < 1e-9,
            "a confirmed terminal quote releases only its unfilled reservation: available={available}, cause={:?}",
            bot.kill_switch_status().1
        );
        task.abort();
    }

    #[tokio::test]
    async fn unreadable_maker_reservations_never_establish_spendable_cash() {
        let (bot, _, task) = fixture(json!("123000000"), true).await;
        sqlx::query("DROP TABLE maker_orders")
            .execute(&bot.db)
            .await
            .unwrap();
        assert_eq!(bot.effective_balance().await, 0.0);
        assert!(bot.degraded_no_trade_active());
        assert_eq!(
            bot.kill_switch_status().1.unwrap().source,
            "maker_collateral_reservation_unavailable"
        );
        task.abort();
    }
}

/// Compute the consecutive-direction haircut multiplier.
/// Returns 1.0 (no haircut) when `consecutive` < 3.
pub(crate) fn direction_bias_haircut(consecutive: usize) -> f64 {
    match consecutive {
        0..=2 => 1.0,
        3 => 0.85,
        4 => 0.70,
        _ => 0.50,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bot::state::TradingBot;
    use crate::db::{self, Trade};

    fn closed_trade(direction: &str, pnl: f64) -> Trade {
        Trade {
            id: 0,
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            market_id: "kelly-test-market".to_string(),
            question: "kelly-test-question".to_string(),
            direction: direction.to_string(),
            entry_price: 0.5,
            amount: 1.0,
            shares: 2.0,
            expected_value: 0.05,
            reason: "test".to_string(),
            confidence: 0.75,
            status: "CLOSED".to_string(),
            mode: "simulation".to_string(),
            source: "test".to_string(),
            end_date: "2099-01-01T00:00:00Z".to_string(),
            exit_price: Some(0.6),
            exit_timestamp: Some("2026-01-01T00:05:00Z".to_string()),
            pnl: Some(pnl),
            exit_type: Some("tp".to_string()),
            was_correct: Some(1),
            forced: Some(0),
            partial_closed: Some(0),
            raw_confidence: None,
            order_id: None,
            token_id: None,
            fok_failed: Some(0),
            fee_paid: 0.0,
            accounting_version: 0,
            signals_json: None,
            calibration_version: Some("test-cal".to_string()),
            run_id: Some("test-run".to_string()),
            strategy: "conviction".to_string(),
        }
    }

    #[test]
    fn direction_bias_haircut_thresholds() {
        assert_eq!(direction_bias_haircut(0), 1.0);
        assert_eq!(direction_bias_haircut(1), 1.0);
        assert_eq!(direction_bias_haircut(2), 1.0);
        assert_eq!(direction_bias_haircut(3), 0.85);
        assert_eq!(direction_bias_haircut(4), 0.70);
        assert_eq!(direction_bias_haircut(5), 0.50);
        assert_eq!(direction_bias_haircut(10), 0.50);
    }

    #[test]
    fn direction_bias_haircut_applied_to_amount() {
        let base_amount = 100.0;

        // No haircut for 2 consecutive
        let amount_2 = base_amount * direction_bias_haircut(2);
        assert!((amount_2 - 100.0).abs() < f64::EPSILON);

        // 15% haircut for 3 consecutive
        let amount_3 = base_amount * direction_bias_haircut(3);
        assert!((amount_3 - 85.0).abs() < f64::EPSILON);

        // 30% haircut for 4 consecutive
        let amount_4 = base_amount * direction_bias_haircut(4);
        assert!((amount_4 - 70.0).abs() < f64::EPSILON);

        // 50% haircut for 5+ consecutive
        let amount_5 = base_amount * direction_bias_haircut(5);
        assert!((amount_5 - 50.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn kelly_size_reads_recent_closed_trades_for_direction_haircut() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_kelly_direction_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 1_000.0;
        cfg.stake_pct = 0.0;
        cfg.kelly_sizing = false;
        cfg.kelly_fraction = 1.0;
        cfg.min_stake = 0.01;
        cfg.max_stake = 1_000.0;
        cfg.circuit_breaker_losses = 0;

        for _ in 0..3 {
            let _ = db::insert_trade(&pool, &closed_trade("BUY_YES", 0.1)).await?;
        }

        let bot = TradingBot::new(cfg, pool).await?;
        let same_direction = bot.kelly_size(100.0, 0.75, None, "BUY_YES").await;
        let opposite_direction = bot.kelly_size(100.0, 0.75, None, "BUY_NO").await;

        assert!((same_direction - 85.0).abs() < 1e-9);
        assert!((opposite_direction - 100.0).abs() < 1e-9);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn kelly_size_skips_trade_when_circuit_breaker_would_drop_below_min_stake(
    ) -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_kelly_cb_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 1_000.0;
        cfg.stake_pct = 0.0;
        cfg.kelly_sizing = false;
        cfg.kelly_fraction = 1.0;
        cfg.min_stake = 10.0;
        cfg.max_stake = 1_000.0;
        cfg.circuit_breaker_losses = 1;

        let _ = db::insert_trade(&pool, &closed_trade("BUY_YES", -0.1)).await?;

        let bot = TradingBot::new(cfg, pool).await?;
        let sized = bot.kelly_size(10.0, 0.75, None, "HOLD").await;
        assert_eq!(sized, 0.0);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }

    #[tokio::test]
    async fn kelly_size_streak_breaks_on_non_loss_before_threshold() -> anyhow::Result<()> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_kelly_streak_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 1_000.0;
        cfg.stake_pct = 0.0;
        cfg.kelly_sizing = false;
        cfg.kelly_fraction = 1.0;
        cfg.min_stake = 10.0;
        cfg.max_stake = 1_000.0;
        cfg.circuit_breaker_losses = 2;

        let _ = db::insert_trade(&pool, &closed_trade("BUY_YES", -0.1)).await?;
        let _ = db::insert_trade(&pool, &closed_trade("BUY_YES", 0.1)).await?;
        let _ = db::insert_trade(&pool, &closed_trade("BUY_YES", -0.1)).await?;

        let bot = TradingBot::new(cfg, pool).await?;
        let sized = bot.kelly_size(100.0, 0.75, None, "HOLD").await;
        assert_eq!(sized, 100.0);

        drop(bot);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
        Ok(())
    }
}

/// All external state needed by the production Kelly calculation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct KellySizingInput {
    pub base_amount: f64,
    pub confidence: f64,
    pub override_amount: Option<f64>,
    pub effective_balance: f64,
    pub consecutive_losses: usize,
    pub correlation_multiplier: f64,
    pub consecutive_direction: usize,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct KellySizingDecision {
    pub amount: f64,
    pub activate_cooldown: bool,
    pub halvings: u32,
    pub direction_haircut: f64,
}

pub(crate) fn evaluate_kelly_size(
    cfg: &crate::config::Config,
    state: &KellySizingInput,
) -> KellySizingDecision {
    let mut result = KellySizingDecision {
        amount: 0.0,
        activate_cooldown: false,
        halvings: 0,
        direction_haircut: 1.0,
    };
    if let Some(amount) = state.override_amount {
        result.amount = amount.max(0.01);
        return result;
    }
    let min_conf = cfg.strategy.params.min_confidence;
    let mut amount = if cfg.stake_pct > 0.0 {
        (state.effective_balance * cfg.stake_pct).clamp(cfg.min_stake, cfg.max_stake)
    } else {
        state.base_amount
    };
    if cfg.kelly_sizing && state.confidence > min_conf {
        let kelly_max = cfg.kelly_max_mult.max(1.0);
        let mult = (1.0
            + (state.confidence - min_conf) / (1.0 - min_conf).max(0.001) * (kelly_max - 1.0))
            .clamp(1.0, kelly_max);
        amount *= mult;
    }
    let fraction_cap = cfg.kelly_fraction * state.effective_balance;
    if fraction_cap > 0.0 {
        amount = amount.min(fraction_cap);
    }
    amount = amount.clamp(cfg.min_stake, cfg.max_stake);
    let cb_losses = cfg.circuit_breaker_losses as usize;
    if cb_losses > 0 && state.consecutive_losses >= cb_losses {
        result.halvings = (state.consecutive_losses / cb_losses) as u32;
        amount /= 2_f64.powi(result.halvings as i32);
        if amount < cfg.min_stake {
            return result;
        }
        result.activate_cooldown = state.consecutive_losses >= cb_losses * 2;
    }
    if state.correlation_multiplier < 1.0 {
        amount *= state.correlation_multiplier;
    }
    result.direction_haircut = direction_bias_haircut(state.consecutive_direction);
    amount *= result.direction_haircut;
    result.amount = amount.max(cfg.min_stake.max(0.01));
    result
}

#[cfg(test)]
mod replay_sizing_tests {
    use super::*;

    fn fixture() -> (crate::config::Config, KellySizingInput) {
        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.stake_pct = 0.1;
        cfg.min_stake = 1.0;
        cfg.max_stake = 100.0;
        cfg.kelly_sizing = false;
        cfg.kelly_fraction = 1.0;
        cfg.circuit_breaker_losses = 2;
        (
            cfg,
            KellySizingInput {
                base_amount: 5.0,
                confidence: 0.8,
                override_amount: None,
                effective_balance: 100.0,
                consecutive_losses: 0,
                correlation_multiplier: 1.0,
                consecutive_direction: 0,
            },
        )
    }

    #[test]
    fn captured_sizing_preserves_caps_haircuts_and_cooldown_order() {
        let (cfg, mut state) = fixture();
        assert_eq!(evaluate_kelly_size(&cfg, &state).amount, 10.0);
        state.consecutive_losses = 4;
        state.correlation_multiplier = 0.5;
        state.consecutive_direction = 3;
        let decision = evaluate_kelly_size(&cfg, &state);
        assert_eq!(decision.amount, 1.0625); // 10 / 4 * 0.5 * 0.85
        assert!(decision.activate_cooldown);
        state.consecutive_losses = 8;
        let decision = evaluate_kelly_size(&cfg, &state);
        assert_eq!(decision.amount, 0.0);
        assert!(!decision.activate_cooldown); // Original early skip happens first.
        state.override_amount = Some(0.001);
        let decision = evaluate_kelly_size(&cfg, &state);
        assert_eq!(decision.amount, 0.01);
        assert!(!decision.activate_cooldown);
    }
}
