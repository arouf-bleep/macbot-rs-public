use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::{
    extract::{connect_info::ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use chrono::Utc;
use serde::Serialize;
use serde_json::json;

use crate::bot::state::TradingBot;
use crate::config::Config;
use crate::domain::BotMode;

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct PreflightRuntimeHints {
    bind_all: bool,
    dev_cors: bool,
    exchange_credentials_configured: bool,
    settlement_rpc_configured: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct LivePreflightChecks {
    config_valid: bool,
    auth_required: bool,
    auth_credentials_configured: bool,
    dev_cors_disabled: bool,
    env_file_permissions_ok: bool,
    private_rpc_configured: bool,
    analysis_stale_limit_ok: bool,
    daily_loss_limit_configured: bool,
    min_trade_covers_gas: bool,
    idempotency_ttl_ok: bool,
    live_maker_accounting_supported: bool,
    exchange_credentials_configured: bool,
    settlement_rpc_configured: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct LivePreflightReport {
    pub required: bool,
    pub ready: bool,
    pub mode: String,
    pub execution_mode: String,
    pub rollout_stage: String,
    pub(crate) runtime: PreflightRuntimeHints,
    pub(crate) checks: LivePreflightChecks,
    pub config_error: Option<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

fn env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| {
            let value = v.trim().to_ascii_lowercase();
            matches!(value.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(default)
}

fn rollout_stage() -> String {
    std::env::var("MACBOT_ROLLOUT_STAGE")
        .unwrap_or_else(|_| "full".to_string())
        .trim()
        .to_ascii_lowercase()
}

fn runtime_preflight_hints() -> PreflightRuntimeHints {
    PreflightRuntimeHints {
        bind_all: env_bool("MACBOT_BIND_ALL", false),
        dev_cors: env_bool("MACBOT_DEV_CORS", false),
        exchange_credentials_configured: [
            "PRIVATE_KEY",
            "POLYMARKET_API_KEY",
            "POLYMARKET_SECRET",
            "POLYMARKET_PASSPHRASE",
        ]
        .iter()
        .all(|name| {
            std::env::var(name)
                .map(zeroize::Zeroizing::new)
                .is_ok_and(|v| !v.trim().is_empty())
        }),
        settlement_rpc_configured: std::env::var("POLYGON_RPC_URL")
            .is_ok_and(|url| !url.trim().is_empty()),
    }
}

fn env_file_permissions_ok_at(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|meta| meta.permissions().mode() & 0o077 == 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        true
    }
}

fn env_file_permissions_ok() -> bool {
    let dotenv_path = std::env::var("MACBOT_DOTENV_PATH").unwrap_or_else(|_| ".env".to_string());
    env_file_permissions_ok_at(Path::new(&dotenv_path))
}

fn evaluate_live_preflight(cfg: &Config, runtime: PreflightRuntimeHints) -> LivePreflightReport {
    let mut effective_cfg = cfg.clone();
    let config_error = effective_cfg
        .normalize_and_validate()
        .err()
        .map(|err| err.to_string());
    let required = effective_cfg.mode_kind() == BotMode::Live;

    let checks = LivePreflightChecks {
        config_valid: config_error.is_none(),
        auth_required: effective_cfg.auth_required,
        auth_credentials_configured: !effective_cfg.api_key.trim().is_empty()
            && !effective_cfg.api_hmac_secret.trim().is_empty(),
        dev_cors_disabled: !runtime.dev_cors,
        env_file_permissions_ok: env_file_permissions_ok(),
        private_rpc_configured: !effective_cfg.private_rpc_url.trim().is_empty(),
        analysis_stale_limit_ok: effective_cfg.analysis_stale_seconds <= 120,
        daily_loss_limit_configured: effective_cfg.daily_loss_limit > 0.0,
        min_trade_covers_gas: effective_cfg.min_trade_usd >= (effective_cfg.trade_gas_usd * 2.0),
        idempotency_ttl_ok: effective_cfg.idempotency_ttl_seconds >= 60,
        live_maker_accounting_supported: effective_cfg.execution_mode == "taker"
            || (matches!(effective_cfg.execution_mode.as_str(), "maker" | "hybrid")
                && effective_cfg.maker_live_quoting),
        settlement_rpc_configured: runtime.settlement_rpc_configured
            || !effective_cfg.private_rpc_url.trim().is_empty(),
        exchange_credentials_configured: runtime.exchange_credentials_configured,
    };

    let mut warnings = Vec::new();
    let mut errors = Vec::new();

    if required {
        if !checks.exchange_credentials_configured {
            errors.push(
                "Live execution requires configured wallet and Polymarket API credentials".into(),
            );
        }
        if !checks.live_maker_accounting_supported {
            errors.push("Live maker/hybrid requires maker_live_quoting=true and receipt reconciliation; hybrid crosses require the serialized cancel-and-settle handoff".to_string());
        }
        if !checks.settlement_rpc_configured {
            errors.push("Live settlement requires POLYGON_RPC_URL or private_rpc_url with Polygon finalized-receipt support".to_string());
        }
        if let Some(err) = config_error.as_ref() {
            errors.push(err.clone());
        }
        if !checks.dev_cors_disabled {
            errors.push(
                "MACBOT_DEV_CORS is enabled; disable permissive CORS before live deployment"
                    .to_string(),
            );
        }
        if !checks.env_file_permissions_ok {
            errors.push(
                ".env is present with overly broad permissions; restrict it to owner-only (chmod 600 .env)"
                    .to_string(),
            );
        }
        if runtime.bind_all {
            warnings.push(
                "MACBOT_BIND_ALL=1 exposes the API directly; keep it behind a TLS-terminating reverse proxy"
                    .to_string(),
            );
        }
        if !checks.private_rpc_configured {
            warnings.push(
                "private_rpc_url is empty; settlement must use the explicitly configured POLYGON_RPC_URL"
                    .to_string(),
            );
        }
    }

    let ready = !required
        || (checks.config_valid
            && checks.auth_required
            && checks.auth_credentials_configured
            && checks.dev_cors_disabled
            && checks.env_file_permissions_ok
            && checks.live_maker_accounting_supported
            && checks.settlement_rpc_configured
            && checks.exchange_credentials_configured);

    LivePreflightReport {
        required,
        ready,
        mode: effective_cfg.mode.clone(),
        execution_mode: effective_cfg.execution_mode.clone(),
        rollout_stage: rollout_stage(),
        runtime,
        checks,
        config_error,
        warnings,
        errors,
    }
}

pub fn live_preflight_report(cfg: &Config) -> LivePreflightReport {
    evaluate_live_preflight(cfg, runtime_preflight_hints())
}

// GET /api/health
pub async fn health(State(_bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    Json(json!({
        "status": "ok",
        "utc": Utc::now().to_rfc3339(),
    }))
}

// GET /api/preflight
pub async fn preflight(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let cfg = bot.control_plane.config.read().await.clone();
    let report = live_preflight_report(&cfg);
    let status = if report.required && !report.ready {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    (
        status,
        Json(json!({
            "preflight": report,
            "utc": Utc::now().to_rfc3339(),
        })),
    )
}

// GET /api/ready
pub async fn ready(
    State(bot): State<Arc<TradingBot>>,
    peer: Option<ConnectInfo<SocketAddr>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(&bot.db)
        .await
        .is_ok();

    let execution_actor_ready = bot.execution_state.execution_tx.read().await.is_some();
    let cfg = bot.control_plane.config.read().await.clone();
    let db_quick_check_enabled = env_bool(
        "MACBOT_READY_DB_QUICK_CHECK",
        cfg.mode_kind() == BotMode::Live,
    );
    let db_quick_check_ok = if db_quick_check_enabled {
        crate::db::quick_check(&bot.db).await.is_ok()
    } else {
        true
    };
    let preflight = live_preflight_report(&cfg);
    let config_valid = preflight.checks.config_valid;
    let requires_l2 = matches!(cfg.execution_mode.as_str(), "maker" | "hybrid");

    let l2_stale = bot.l2_feed_stale();
    let l2_feed_age_ms = bot.l2_feed_age_ms().await;
    let l2_age_slo_ms = std::env::var("MACBOT_SLO_MAX_L2_AGE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(3_000);
    let l2_age_slo_breached = l2_feed_age_ms
        .map(|age| age > l2_age_slo_ms)
        .unwrap_or(true);
    let kill_switch = bot.kill_switch_active();
    let loop_running = bot.loop_running();
    let analysis_age_secs = bot.analysis_age_secs().await;
    let analysis_stale = bot.is_analysis_stale().await;
    let last_reconcile_summary = bot.last_reconcile_summary().await;
    let last_reconcile_error = bot.last_reconcile_error().await;

    let service_ready = db_ok && db_quick_check_ok && execution_actor_ready && config_valid;
    let clob_client_ready = cfg.mode_kind() != BotMode::Live || bot.clob.is_some();
    let backtest_only = bot.backtest_only();
    let trading_ready = service_ready
        && !backtest_only
        && preflight.ready
        && clob_client_ready
        && !kill_switch
        && (!requires_l2 || (!l2_stale && !l2_age_slo_breached));

    let status = if service_ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let detailed_read_allowed = super::auth::authorize_read_only(
        &bot,
        peer.map(|ConnectInfo(addr)| addr.ip().to_string()),
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok()),
    )
    .await
    .is_ok();
    if !detailed_read_allowed {
        return (
            status,
            Json(json!({
                "service_ready": service_ready,
                "trading_ready": trading_ready,
                "utc": Utc::now().to_rfc3339(),
            })),
        );
    }

    (
        status,
        Json(json!({
            "service_ready": service_ready,
            "trading_ready": trading_ready,
            "checks": {
                "db_ok": db_ok,
                "db_quick_check_enabled": db_quick_check_enabled,
                "db_quick_check_ok": db_quick_check_ok,
                "execution_actor_ready": execution_actor_ready,
                "config_valid": config_valid,
                "clob_client_ready": clob_client_ready,
                "backtest_only": backtest_only,
                "kill_switch": kill_switch,
                "requires_l2": requires_l2,
                "l2_stale": l2_stale,
                "l2_feed_age_ms": l2_feed_age_ms,
                "l2_age_slo_ms": l2_age_slo_ms,
                "l2_age_slo_breached": l2_age_slo_breached,
                "loop_running": loop_running,
                "analysis_age_secs": analysis_age_secs,
                "analysis_stale": analysis_stale,
            },
            "preflight": preflight,
            "last_reconcile_summary": last_reconcile_summary,
            "last_reconcile_error": last_reconcile_error,
            "execution_mode": cfg.execution_mode,
            "utc": Utc::now().to_rfc3339(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::{evaluate_live_preflight, PreflightRuntimeHints};
    use crate::config::Config;
    use std::fs;

    fn fixture() -> Config {
        serde_json::from_str(include_str!("../../config.json"))
            .unwrap_or_else(|_| panic!("config fixture should parse"))
    }

    #[test]
    fn research_taker_readiness_serves_without_claiming_execution_is_available(
    ) -> anyhow::Result<()> {
        use axum::{body::to_bytes, response::IntoResponse};
        use std::sync::{atomic::Ordering, Arc};

        let _environment = crate::env_util::test_env_lock();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
            let mut cfg = fixture();
            cfg.mode = "simulation".into();
            cfg.execution_mode = "taker".into();
            cfg.auth_required = true;
            cfg.api_key = "readiness-test-key".into();
            cfg.allowed_ips = vec!["127.0.0.1".into()];
            let key = std::env::var("MACBOT_API_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| cfg.api_key.clone());
            let bot = Arc::new(
                crate::bot::state::TradingBot::new(cfg, crate::db::open(":memory:").await?).await?,
            );
            bot.start_execution_actor().await;
            assert!(!bot.kill_switch_active());
            assert!(!bot.loop_running());
            assert!(bot.l2_feed_stale(), "no L2 feed is started by this fixture");

            for research_only in [false, true] {
                bot.runtime_state
                    .backtest_only
                    .store(research_only, Ordering::SeqCst);
                for authenticated in [false, true] {
                    let mut headers = axum::http::HeaderMap::new();
                    if authenticated {
                        headers.insert("x-api-key", key.trim().parse()?);
                    }
                    let response = super::ready(
                        axum::extract::State(Arc::clone(&bot)),
                        Some(axum::extract::ConnectInfo("127.0.0.1:41000".parse()?)),
                        headers,
                    )
                    .await
                    .into_response();
                    assert_eq!(response.status(), axum::http::StatusCode::OK);
                    let payload: serde_json::Value =
                        serde_json::from_slice(&to_bytes(response.into_body(), 64 * 1024).await?)?;
                    assert_eq!(payload["service_ready"], true, "{payload}");
                    assert_eq!(payload["trading_ready"], !research_only, "{payload}");
                    if authenticated {
                        assert_eq!(payload["checks"]["execution_actor_ready"], true);
                        assert_eq!(payload["checks"]["requires_l2"], false);
                        assert_eq!(payload["checks"]["backtest_only"], research_only);
                    } else {
                        assert!(payload.get("checks").is_none());
                    }
                }
            }
            let records: i64 = sqlx::query_scalar(
                "SELECT (SELECT COUNT(*) FROM trades) + (SELECT COUNT(*) FROM trade_fills) + (SELECT COUNT(*) FROM live_orders) + (SELECT COUNT(*) FROM maker_orders) + (SELECT COUNT(*) FROM paper_maker_orders)",
            )
            .fetch_one(&bot.db)
            .await?;
            assert_eq!(records, 0, "readiness must not create execution evidence");
            bot.stop_execution_actor().await?;
            bot.db.close().await;
            Ok(())
            })
    }

    #[test]
    fn live_preflight_fails_when_dev_cors_is_enabled() {
        let mut cfg = fixture();
        cfg.mode = "live".to_string();
        cfg.auth_required = true;
        // Live validation rejects placeholder and short credentials, so the
        // preflight fixture uses values that would be acceptable in live.
        cfg.api_key = "a1b2c3d4e5f60718".to_string();
        cfg.api_hmac_secret = "9f3c7d21b8e045a6c1d93f70b2e845ac".to_string();

        let report = evaluate_live_preflight(
            &cfg,
            PreflightRuntimeHints {
                bind_all: false,
                dev_cors: true,
                exchange_credentials_configured: true,
                settlement_rpc_configured: true,
            },
        );

        assert!(report.required);
        assert!(!report.ready);
        assert!(!report.checks.dev_cors_disabled);
        assert!(report
            .errors
            .iter()
            .any(|msg| msg.contains("MACBOT_DEV_CORS")));
    }

    #[test]
    fn live_preflight_warns_on_public_bind_without_blocking_ready() {
        let mut cfg = fixture();
        cfg.mode = "live".to_string();
        cfg.execution_mode = "taker".to_string();
        cfg.auth_required = true;
        // Live validation rejects placeholder and short credentials, so the
        // preflight fixture uses values that would be acceptable in live.
        cfg.api_key = "a1b2c3d4e5f60718".to_string();
        cfg.api_hmac_secret = "9f3c7d21b8e045a6c1d93f70b2e845ac".to_string();
        cfg.private_rpc_url.clear();

        let report = evaluate_live_preflight(
            &cfg,
            PreflightRuntimeHints {
                bind_all: true,
                dev_cors: false,
                exchange_credentials_configured: true,
                settlement_rpc_configured: true,
            },
        );

        assert!(report.required);
        assert!(report.ready);
        assert!(report
            .warnings
            .iter()
            .any(|msg| msg.contains("TLS-terminating reverse proxy")));
        assert!(report
            .warnings
            .iter()
            .any(|msg| msg.contains("private_rpc_url")));
    }

    #[test]
    fn live_maker_and_hybrid_require_real_quoting_and_receipt_support() {
        for execution_mode in ["maker", "hybrid"] {
            for live_quoting in [false, true] {
                let mut cfg = fixture();
                cfg.mode = "live".into();
                cfg.auth_required = true;
                cfg.api_key = "a1b2c3d4e5f60718".into();
                cfg.api_hmac_secret = "9f3c7d21b8e045a6c1d93f70b2e845ac".into();
                cfg.execution_mode = execution_mode.into();
                cfg.maker_live_quoting = live_quoting;
                let report = evaluate_live_preflight(
                    &cfg,
                    PreflightRuntimeHints {
                        bind_all: false,
                        dev_cors: false,
                        exchange_credentials_configured: true,
                        settlement_rpc_configured: true,
                    },
                );
                let supported = live_quoting;
                assert_eq!(report.ready, supported, "{:?}", report.errors);
                assert_eq!(report.checks.live_maker_accounting_supported, supported);
                assert_eq!(
                    report
                        .errors
                        .iter()
                        .any(|e| e.contains("receipt reconciliation")),
                    !supported
                );
            }
        }
    }

    #[test]
    fn live_preflight_requires_receipt_verification_rpc() {
        let mut cfg = fixture();
        cfg.mode = "live".into();
        cfg.execution_mode = "taker".into();
        cfg.private_rpc_url.clear();
        let report = evaluate_live_preflight(
            &cfg,
            PreflightRuntimeHints {
                bind_all: false,
                dev_cors: false,
                exchange_credentials_configured: true,
                settlement_rpc_configured: false,
            },
        );
        assert!(!report.ready);
        assert!(report
            .errors
            .iter()
            .any(|e| e.contains("finalized-receipt")));
    }

    #[test]
    fn live_preflight_blocks_missing_exchange_credentials() {
        let mut cfg = fixture();
        cfg.mode = "live".into();
        cfg.execution_mode = "taker".into();
        cfg.auth_required = true;
        cfg.api_key = "a1b2c3d4e5f60718".into();
        cfg.api_hmac_secret = "9f3c7d21b8e045a6c1d93f70b2e845ac".into();
        let report = evaluate_live_preflight(
            &cfg,
            PreflightRuntimeHints {
                bind_all: false,
                dev_cors: false,
                exchange_credentials_configured: false,
                settlement_rpc_configured: true,
            },
        );
        assert!(!report.ready);
        assert!(report
            .errors
            .iter()
            .any(|e| e.contains("Polymarket API credentials")));
    }

    #[test]
    fn simulation_mode_preflight_is_not_required() {
        let cfg = fixture();
        let report = evaluate_live_preflight(
            &cfg,
            PreflightRuntimeHints {
                bind_all: false,
                dev_cors: true,
                exchange_credentials_configured: true,
                settlement_rpc_configured: true,
            },
        );

        assert!(!report.required);
        assert!(report.ready);
        assert!(report.errors.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn env_file_permission_check_requires_owner_only_access() {
        use std::os::unix::fs::PermissionsExt;

        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_env_perm_{uniq}.env"));
        fs::write(&path, "KEY=value\n").expect("temp env file should be writable");

        let mut perms = fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&path, perms).expect("set 644");
        assert!(!super::env_file_permissions_ok_at(&path));

        let mut perms = fs::metadata(&path).expect("metadata").permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&path, perms).expect("set 600");
        assert!(super::env_file_permissions_ok_at(&path));

        let _ = fs::remove_file(&path);
    }
}
