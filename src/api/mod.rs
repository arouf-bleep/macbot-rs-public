mod accounting;
mod auth;
pub use auth::prune_rate_map;
mod backtest;
mod bot_routes;
mod config_api;
mod deep_model_api;
mod ev;
pub mod health;
mod integrity;
#[cfg(test)]
pub(crate) use integrity::dashboard_integrity_report;
mod maker;
pub mod markets;
mod metrics;
mod realtime;
mod recovery;
mod report;
mod sse;
mod trades;
mod ws;

use axum::{
    http::{header, HeaderName, HeaderValue, Method},
    middleware,
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower_http::{cors::CorsLayer, services::ServeDir, trace::TraceLayer};

use crate::bot::state::TradingBot;

pub fn router(bot: Arc<TradingBot>) -> Router {
    let protected_dashboard_read_routes = Router::new()
        .route("/api/config", get(config_api::get_config))
        .route("/api/recovery", get(recovery::status))
        .route(
            "/api/recovery/orders/:order_id",
            get(recovery::order_evidence),
        )
        .route("/api/integrity/report", get(integrity::integrity_report))
        .route("/api/preflight", get(health::preflight))
        // ── Trades ────────────────────────────────────────────────────────────
        .route("/api/trades", get(trades::list_trades))
        .route("/api/trades/:id/fills", get(trades::trade_fills))
        .route("/api/trades/analysis", get(trades::trade_analysis))
        .route("/api/signal-stats", get(trades::signal_stats))
        .route("/api/attribution", get(trades::attribution))
        .route("/api/rejections", get(trades::rejections))
        .route("/api/report", get(report::ledger_report))
        .route("/api/decisions", get(report::decisions))
        .route("/api/accounting/truth", get(accounting::accounting_truth))
        // ── Markets ───────────────────────────────────────────────────────────
        .route("/api/markets", get(markets::list_markets))
        .route("/api/markets/no-arb", get(markets::no_arb_snapshot))
        .route("/api/markets/:cid/price", get(markets::market_price))
        .route("/api/outcomes", get(markets::recent_outcomes))
        // ── Bot loop ──────────────────────────────────────────────────────────
        .route("/api/loop/status", get(bot_routes::loop_status))
        .route("/api/risk/status", get(bot_routes::risk_status))
        .route("/api/maker/state", get(maker::maker_state))
        .route("/api/maker/ab-summary", get(maker::maker_ab_summary))
        .route(
            "/api/maker/rollout-status",
            get(maker::maker_rollout_status),
        )
        // ── Analysis + trade ──────────────────────────────────────────────────
        .route("/api/analysis", get(bot_routes::get_analysis))
        .route("/api/stats", get(bot_routes::get_stats))
        .route("/api/ml/state", get(bot_routes::ml_state))
        .route("/api/btc-chart", get(bot_routes::btc_chart_data))
        // ── Deep model monitoring ────────────────────────────────────────
        .route(
            "/api/deep-model/status",
            get(deep_model_api::deep_model_status),
        )
        // ── Prometheus metrics ────────────────────────────────────────────────
        .route("/api/metrics", get(metrics::prometheus_metrics))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&bot),
            auth::require_read_auth,
        ));

    let protected_read_routes = Router::new()
        .route("/api/analyze", post(bot_routes::trigger_analysis))
        .route("/api/backtest", post(backtest::run_backtest))
        .route_layer(middleware::from_fn(auth::require_complete_json))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&bot),
            auth::require_api_auth,
        ))
        .route_layer(middleware::from_fn(auth::rate_limit));

    let protected_routes = Router::new()
        .route("/api/trades/:id/close", post(trades::close_trade))
        .route("/api/trades/close-all", post(trades::close_all))
        .route("/api/balance/reset", post(bot_routes::reset_balance))
        .route("/api/loop/start", post(bot_routes::start_loop))
        .route("/api/loop/stop", post(bot_routes::stop_loop))
        .route("/api/risk/kill-switch", post(bot_routes::set_kill_switch))
        .route("/api/db/backup", post(bot_routes::backup_database))
        .route("/api/drawdown/reset", post(bot_routes::reset_drawdown))
        .route("/api/resync/full", post(bot_routes::full_resync))
        .route(
            "/api/recovery/maker/:order_id/recheck",
            post(recovery::recheck),
        )
        .route(
            "/api/recovery/orders/:order_id/recheck",
            post(recovery::recheck),
        )
        .route(
            "/api/recovery/redemptions/reconcile",
            post(recovery::reconcile_redemption),
        )
        .route("/api/trade", post(bot_routes::execute_trade_handler))
        .route("/api/trade/force", post(bot_routes::force_trade))
        .route("/api/config", post(config_api::update_config))
        .route_layer(middleware::from_fn(auth::require_complete_json))
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&bot),
            auth::require_api_auth,
        ))
        .route_layer(middleware::from_fn(auth::rate_limit));

    Router::new()
        .route("/api/health", get(health::health))
        .route("/api/ready", get(health::ready))
        .route(
            "/api/ev/calc",
            post(ev::calc_ev).layer(middleware::from_fn(auth::require_complete_json)),
        )
        .route("/api/stream/token", get(sse::issue_stream_token))
        .route("/api/stream", get(sse::stream))
        .route("/api/ws", get(ws::stream_ws))
        .merge(protected_dashboard_read_routes)
        .merge(protected_read_routes)
        // ── Protected mutating routes ─────────────────────────────────────────
        .merge(protected_routes)
        // ── Dashboard (no-cache) ────────────────────────────────────────────
        .route("/", get(serve_index))
        .nest_service("/static", ServeDir::new("static"))
        .layer(middleware::from_fn(auth::public_rate_limit))
        .layer(middleware::from_fn(security_headers))
        .layer(build_cors_layer(&bot))
        .layer(TraceLayer::new_for_http().make_span_with(request_trace_span))
        .with_state(Arc::clone(&bot))
}

fn request_trace_span<B>(request: &axum::http::Request<B>) -> tracing::Span {
    // EventSource/WebSocket authentication uses a short-lived query token.
    // tower-http's default span logs the whole URI, so record only its path
    // and omit both query parameters and headers at every logging level.
    tracing::debug_span!(
        "request",
        method = %request.method(),
        path = %request.uri().path(),
        version = ?request.version(),
    )
}

async fn serve_index() -> impl IntoResponse {
    let html = tokio::fs::read_to_string("static/index.html")
        .await
        .unwrap_or_else(|_| "<!DOCTYPE html><html><body>index.html not found</body></html>".into());
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
            (header::PRAGMA, "no-cache"),
            (header::EXPIRES, "0"),
        ],
        html,
    )
}

async fn security_headers(
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let is_static = req.uri().path().starts_with("/static/");
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    if is_static {
        // The dashboard's JS and CSS went out with `Last-Modified` alone, so
        // browsers applied heuristic caching and kept executing the previous
        // build through a hard reload and a server restart. `no-cache` means
        // revalidate every time: a 304 when nothing changed, the new file the
        // moment it does. Not `no-store`, which would refetch 200 KB per load.
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, must-revalidate"),
        );
    }
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        HeaderName::from_static("x-frame-options"),
        HeaderValue::from_static("DENY"),
    );
    headers.insert(
        HeaderName::from_static("x-xss-protection"),
        HeaderValue::from_static("1; mode=block"),
    );
    headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    resp
}

fn build_cors_layer(bot: &Arc<TradingBot>) -> CorsLayer {
    let dev_cors = std::env::var("MACBOT_DEV_CORS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if dev_cors {
        let mode = bot
            .control_plane
            .config
            .try_read()
            .ok()
            .map(|c| c.mode.clone())
            .unwrap_or_default();
        if mode == "live" {
            tracing::error!(
                "MACBOT_DEV_CORS=1 is set in LIVE mode — permissive CORS is dangerous in production! \
                 Falling through to restrictive CORS. Unset MACBOT_DEV_CORS to silence this."
            );
        } else {
            tracing::warn!("MACBOT_DEV_CORS=1: permissive CORS enabled (non-live mode)");
            return CorsLayer::permissive();
        }
    }

    let cfg = bot.control_plane.config.try_read().ok().map(|g| g.clone());
    let mut origins = cfg
        .as_ref()
        .map(|c| c.cors_allowed_origins.clone())
        .unwrap_or_default();
    if origins.is_empty() {
        origins = vec![
            "http://localhost:3000".to_string(),
            "http://127.0.0.1:3000".to_string(),
            "http://localhost:8000".to_string(),
            "http://127.0.0.1:8000".to_string(),
        ];
    }

    let parsed_origins: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| HeaderValue::from_str(o).ok())
        .collect();

    let headers = vec![
        header::CONTENT_TYPE,
        HeaderName::from_static("x-api-key"),
        HeaderName::from_static("x-api-ts"),
        HeaderName::from_static("x-api-nonce"),
        HeaderName::from_static("x-api-signature"),
        HeaderName::from_static("x-idempotency-key"),
    ];

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::DELETE])
        .allow_headers(headers);

    if parsed_origins.is_empty() {
        cors
    } else {
        cors.allow_origin(parsed_origins)
    }
}

#[cfg(test)]
mod tests {
    use super::router;
    use crate::bot::state::TradingBot;
    use axum::{
        body::{to_bytes, Body},
        extract::connect_info::ConnectInfo,
        http::{HeaderValue, Request, StatusCode},
    };
    use serde_json::Value;
    use std::{net::SocketAddr, sync::Arc};
    use tower::util::ServiceExt;

    async fn auth_test_bot(
        label: &str,
        auth_required: bool,
    ) -> anyhow::Result<(Arc<TradingBot>, std::path::PathBuf, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_api_router_{label}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = crate::db::open(&path_str).await?;

        let mut cfg: crate::config::Config =
            serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.auth_required = auth_required;
        cfg.api_key = "local".to_string();
        cfg.api_hmac_secret = "localsecret".to_string();

        let config_path = path.with_extension("json");
        cfg.save(config_path.to_str().expect("config path"))?;
        let mut bot = TradingBot::new(cfg, pool).await?;
        Arc::get_mut(&mut bot.control_plane)
            .expect("new control plane is exclusively owned")
            .config_path = config_path.to_string_lossy().into_owned();
        let bot = Arc::new(bot);
        Ok((bot, path, path_str))
    }

    fn cleanup_sqlite(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path.with_extension("json"));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    fn request_with_ip(path: &str, ip: [u8; 4], port: u16) -> Request<Body> {
        let mut req = Request::builder()
            .uri(path)
            .body(Body::empty())
            .expect("request");
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from((ip, port))));
        req
    }

    fn loopback_request(path: &str) -> Request<Body> {
        request_with_ip(path, [127, 0, 0, 1], 41000)
    }

    fn remote_request(path: &str) -> Request<Body> {
        request_with_ip(path, [10, 0, 0, 8], 41000)
    }

    fn signed_config_request(body: &str) -> Request<Body> {
        signed_post_request("/api/config", body)
    }

    #[tokio::test]
    async fn debug_request_tracing_omits_query_credentials_and_headers() -> anyhow::Result<()> {
        use std::io::Write;
        use std::sync::Mutex;

        const CHILD_ENV: &str = "MACBOT_TRACE_REDACTION_TEST_CHILD";
        const CHILD_MARKER: &str = "MACBOT_REQUEST_TRACE_REDACTION_VERIFIED";
        if std::env::var_os(CHILD_ENV).is_none() {
            // Callsite interest is process-global. An isolated subscriber makes
            // this assertion independent of concurrent request tracing in the
            // library suite, and matches the server's one global subscriber.
            let output = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::process::Command::new(std::env::current_exe()?)
                    .args([
                        "--exact",
                        "api::tests::debug_request_tracing_omits_query_credentials_and_headers",
                        "--nocapture",
                    ])
                    .env(CHILD_ENV, "1")
                    .kill_on_drop(true)
                    .output(),
            )
            .await??;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::ensure!(
                output.status.success(),
                "Request trace redaction child failed: {stdout}{stderr}"
            );
            anyhow::ensure!(
                stdout.contains(CHILD_MARKER),
                "Request trace redaction child did not run its assertions: {stdout}{stderr}"
            );
            return Ok(());
        }

        #[derive(Clone)]
        struct CapturedLog(Arc<Mutex<Vec<u8>>>);
        impl Write for CapturedLog {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (bot, path, path_str) = auth_test_bot("trace_redaction", true).await?;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&captured);
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .without_time()
            .with_writer(move || CapturedLog(Arc::clone(&output)))
            .finish();
        tracing::subscriber::set_global_default(subscriber)?;
        let mut request = loopback_request(
            "/api/health?stream_token=private-stream-value&api_key=private-query-value",
        );
        request.headers_mut().insert(
            "x-api-key",
            HeaderValue::from_static("private-header-value"),
        );
        let response = router(Arc::clone(&bot)).oneshot(request).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let logs = String::from_utf8(captured.lock().unwrap().clone())?;
        assert!(
            logs.contains("/api/health"),
            "no request trace captured: {logs}"
        );
        assert!(
            logs.contains("method=GET"),
            "request metadata missing: {logs}"
        );
        for secret in [
            "private-stream-value",
            "private-query-value",
            "private-header-value",
            "stream_token",
            "api_key",
        ] {
            assert!(!logs.contains(secret), "request trace exposed {secret}");
        }
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        println!("{CHILD_MARKER}");
        Ok(())
    }

    #[tokio::test]
    async fn signed_json_mutations_reject_trailing_values_without_changing_config(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("strict_json", true).await?;
        let before = bot.control_plane.config.read().await.price_refresh_rate;
        let app = router(Arc::clone(&bot));
        for suffix in ["{}", "not-json", "null"] {
            let body = format!("{{\"price_refresh_rate\":{}}}{suffix}", before + 1);
            let response = app.clone().oneshot(signed_config_request(&body)).await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{suffix}");
            assert_eq!(
                bot.control_plane.config.read().await.price_refresh_rate,
                before
            );
        }
        let accepted = format!("{{\"price_refresh_rate\":{}}} \n\t", before + 1);
        let response = app.oneshot(signed_config_request(&accepted)).await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            bot.control_plane.config.read().await.price_refresh_rate,
            before + 1
        );
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn json_validation_preserves_authentication_and_public_calculation_boundaries(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("strict_json_auth", true).await?;
        let app = router(Arc::clone(&bot));
        let mut unauthenticated = signed_config_request("{}not-json");
        unauthenticated.headers_mut().remove("x-api-signature");
        assert_eq!(
            app.clone().oneshot(unauthenticated).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let request = Request::builder()
            .method("POST")
            .uri("/api/ev/calc")
            .header("content-type", "application/problem+json; charset=utf-8")
            .body(Body::from("{}{}"))?;
        assert_eq!(
            app.oneshot(request).await?.status(),
            StatusCode::BAD_REQUEST
        );
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    fn signed_post_request(path: &str, body: &str) -> Request<Body> {
        signed_request(axum::http::Method::POST, path, body)
    }

    fn signed_request(method: axum::http::Method, path: &str, body: &str) -> Request<Body> {
        signed_request_with_idempotency(method, path, body, "")
    }

    fn signed_request_with_idempotency(
        method: axum::http::Method,
        path: &str,
        body: &str,
        idempotency: &str,
    ) -> Request<Body> {
        use hmac::{Hmac, Mac};
        let ts = chrono::Utc::now().timestamp().to_string();
        let nonce = hex::encode(rand::random::<[u8; 32]>());
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(b"localsecret").expect("hmac key");
        mac.update(
            super::auth::request_canonical(&ts, &nonce, method.as_str(), path, idempotency, body)
                .as_bytes(),
        );
        let signature = hex::encode(mac.finalize().into_bytes());
        let mut req = loopback_request(path);
        *req.method_mut() = method;
        *req.body_mut() = Body::from(body.to_owned());
        for (name, value) in [
            ("content-type", "application/json"),
            ("x-api-key", "local"),
            ("x-api-ts", ts.as_str()),
            ("x-api-nonce", nonce.as_str()),
            ("x-api-signature", signature.as_str()),
        ] {
            req.headers_mut()
                .insert(name, value.parse().expect("header"));
        }
        if !idempotency.is_empty() {
            req.headers_mut().insert(
                "x-idempotency-key",
                idempotency.parse().expect("idempotency header"),
            );
        }
        req
    }

    fn request_with_captured_headers(
        path: &str,
        body: &str,
        headers: &axum::http::HeaderMap,
    ) -> Request<Body> {
        let mut req = signed_post_request(path, body);
        *req.headers_mut() = headers.clone();
        req
    }

    #[tokio::test]
    async fn signed_request_replay_is_denied_after_idempotency_header_change_and_restart(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("durable_nonce", true).await?;
        let body = r#"{"execution_mode":"maker"}"#;
        let first = signed_request_with_idempotency(
            axum::http::Method::POST,
            "/api/config",
            body,
            "first-key",
        );
        let captured = first.headers().clone();
        let app = router(Arc::clone(&bot));
        assert_eq!(app.clone().oneshot(first).await?.status(), StatusCode::OK);
        assert_eq!(
            app.clone()
                .oneshot(request_with_captured_headers(
                    "/api/config",
                    body,
                    &captured
                ))
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let mut tampered = captured.clone();
        tampered.insert("x-idempotency-key", "changed-key".parse()?);
        assert_eq!(
            app.clone()
                .oneshot(request_with_captured_headers(
                    "/api/config",
                    body,
                    &tampered
                ))
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );
        drop(app);
        bot.db.close().await;
        drop(bot);

        let cfg = serde_json::from_slice(&std::fs::read(path.with_extension("json"))?)?;
        let pool = crate::db::open(&path_str).await?;
        let mut reopened = TradingBot::new(cfg, pool).await?;
        Arc::get_mut(&mut reopened.control_plane)
            .expect("new owner")
            .config_path = path.with_extension("json").to_string_lossy().into_owned();
        let reopened = Arc::new(reopened);
        let app = router(Arc::clone(&reopened));
        assert_eq!(
            app.clone()
                .oneshot(request_with_captured_headers(
                    "/api/config",
                    body,
                    &captured
                ))
                .await?
                .status(),
            StatusCode::UNAUTHORIZED
        );
        // A legitimate retry signs a new nonce and can retain its operation key.
        let retry = signed_request_with_idempotency(
            axum::http::Method::POST,
            "/api/config",
            body,
            "first-key",
        );
        assert_eq!(app.oneshot(retry).await?.status(), StatusCode::OK);
        reopened.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn nonce_is_required_signed_and_atomically_consumed_under_concurrent_replay(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("atomic_nonce", true).await?;
        let app = router(Arc::clone(&bot));
        let body = r#"{"execution_mode":"maker"}"#;
        let original = signed_request_with_idempotency(
            axum::http::Method::POST,
            "/api/config",
            body,
            "bound-operation",
        );
        let captured = original.headers().clone();
        let mut missing = request_with_captured_headers("/api/config", body, &captured);
        missing.headers_mut().remove("x-api-nonce");
        assert_eq!(
            app.clone().oneshot(missing).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let mut tampered = request_with_captured_headers("/api/config", body, &captured);
        tampered
            .headers_mut()
            .insert("x-api-nonce", "f".repeat(64).parse()?);
        assert_eq!(
            app.clone().oneshot(tampered).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let mut stripped = request_with_captured_headers("/api/config", body, &captured);
        stripped.headers_mut().remove("x-idempotency-key");
        assert_eq!(
            app.clone().oneshot(stripped).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let mut invalid_header = request_with_captured_headers("/api/config", body, &captured);
        invalid_header.headers_mut().insert(
            "x-idempotency-key",
            axum::http::HeaderValue::from_bytes(b"invalid\xff")?,
        );
        assert_eq!(
            app.clone().oneshot(invalid_header).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        let mut invalid_body = request_with_captured_headers("/api/config", body, &captured);
        *invalid_body.body_mut() = Body::from(vec![0xff]);
        assert_eq!(
            app.clone().oneshot(invalid_body).await?.status(),
            StatusCode::BAD_REQUEST
        );
        let (a, b) = tokio::join!(
            app.clone().oneshot(original),
            app.oneshot(request_with_captured_headers(
                "/api/config",
                body,
                &captured
            )),
        );
        let statuses = [a?.status(), b?.status()];
        assert_eq!(statuses.iter().filter(|&&s| s == StatusCode::OK).count(), 1);
        assert_eq!(
            statuses
                .iter()
                .filter(|&&s| s == StatusCode::UNAUTHORIZED)
                .count(),
            1
        );
        let claims: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE key GLOB 'api_auth_nonce:*'")
                .fetch_one(&bot.db)
                .await?;
        assert_eq!(
            claims, 1,
            "unauthenticated input must not fill the durable replay guard"
        );
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn replay_guard_storage_failure_blocks_the_handler() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("nonce_storage_failure", true).await?;
        let before = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let disk_before = std::fs::read(path.with_extension("json"))?;
        sqlx::query("DROP TABLE meta").execute(&bot.db).await?;
        let response = router(Arc::clone(&bot))
            .oneshot(signed_config_request(r#"{"execution_mode":"maker"}"#))
            .await?;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            serde_json::to_value(bot.control_plane.config.read().await.clone())?,
            before
        );
        assert_eq!(std::fs::read(path.with_extension("json"))?, disk_before);
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn untracked_execution_mutators_are_absent_in_every_runtime_mode() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("no_raw_relay", true).await?;
        for mode in ["simulation", "live"] {
            {
                let mut cfg = bot.control_plane.config.write().await;
                cfg.mode = mode.into();
                cfg.private_rpc_url = "http://127.0.0.1:1/must-not-be-called".into();
            }
            for backtest_only in [false, true] {
                bot.runtime_state
                    .backtest_only
                    .store(backtest_only, std::sync::atomic::Ordering::SeqCst);
                let app = router(Arc::clone(&bot));
                use axum::http::Method;
                for (method, route) in [
                    (Method::POST, "/api/rpc/send-raw"),
                    (Method::POST, "/api/rpc/priority-fee"),
                    (Method::POST, "/api/maker/quote"),
                    (Method::DELETE, "/api/maker/quote/manual-order"),
                    (Method::POST, "/api/maker/clear-market"),
                    (Method::POST, "/api/maker/inventory"),
                    (Method::POST, "/api/trades/clear-history"),
                    (Method::DELETE, "/api/trades/history"),
                ] {
                    let response = app
                        .clone()
                        .oneshot(signed_request(
                            method,
                            route,
                            r#"{"raw_tx":"0x01","order_id":"manual-order","market_id":"manual-market","token_id":"manual-token","side":"bid","price":0.5,"size":5,"net_yes_shares":0,"net_no_shares":0,"net_notional_usd":0}"#,
                        ))
                        .await?;
                    assert_eq!(response.status(), StatusCode::NOT_FOUND);
                }
            }
        }
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn extreme_auth_timestamps_are_rejected_without_panicking() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("timestamp_overflow", true).await?;
        let app = router(bot);
        for timestamp in [i64::MIN, i64::MAX] {
            let mut req = signed_config_request("{}");
            req.headers_mut()
                .insert("x-api-ts", timestamp.to_string().parse()?);
            let response = app.clone().oneshot(req).await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    /// The dashboard's JS went out with `Last-Modified` alone, so browsers
    /// kept executing the previous build through a hard reload and a server
    /// restart — three times in one evening. Static assets must revalidate.
    #[tokio::test]
    async fn static_assets_tell_the_browser_to_revalidate() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("static_cache", false).await?;
        let app = router(bot);

        let response = app
            .clone()
            .oneshot(remote_request("/static/dashboard.js"))
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "dashboard.js must be served"
        );
        let cache = response
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            cache.contains("no-cache"),
            "static assets need Cache-Control: no-cache, got {cache:?}"
        );

        // The API is not static and must not inherit it.
        let response = app.oneshot(remote_request("/api/health")).await?;
        let cache = response
            .headers()
            .get(axum::http::header::CACHE_CONTROL)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            !cache.contains("no-cache"),
            "the API got the static header: {cache:?}"
        );

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn drawdown_reset_preserves_accounting_origin_daily_history_and_kill_switch(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("drawdown_reset", true).await?;
        bot.start_execution_actor().await;
        bot.halt_trading("test", "fault remains unresolved").await;
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query("INSERT INTO trades(timestamp,market_id,question,direction,entry_price,amount,shares,expected_value,reason,confidence,status,mode,source,pnl,exit_timestamp,was_correct,signals_json) VALUES(?,'m','q','BUY_YES',0.5,1,2,0,'test',0.5,'CLOSED','simulation','bot',-2,?,1,'{\"neutral\":0,\"positive\":0.5}')")
            .bind(&now).bind(&now).execute(&bot.db).await?;
        let origin = bot.control_plane.config.read().await.initial_balance;
        let daily = bot.get_today_pnl().await;
        assert_eq!(daily, -2.0);
        *bot.portfolio_state.balance.write().await = 75.0;
        let response = router(bot.clone())
            .oneshot(signed_post_request("/api/drawdown/reset", "{}"))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await?;
        let payload: Value = serde_json::from_slice(&body)?;
        assert_eq!(payload["peak_balance"], 75.0);
        assert_eq!(payload["daily_losses_reset"], false);
        assert_eq!(payload["kill_switch_cleared"], false);
        assert_eq!(
            bot.control_plane.config.read().await.initial_balance,
            origin
        );
        assert_eq!(bot.get_today_pnl().await, daily);
        assert!(bot.kill_switch_active());
        assert_eq!(bot.balance().await, 75.0);
        let mut req = remote_request("/api/signal-stats");
        req.headers_mut().insert("x-api-key", "local".parse()?);
        let response = router(bot.clone()).oneshot(req).await?;
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
        let signals: Value = serde_json::from_slice(&bytes)?;
        assert!(
            signals["signals"].get("neutral").is_none(),
            "neutral signals did not vote DOWN"
        );
        assert_eq!(signals["signals"]["positive"]["trades"], 1);
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn config_get_requires_read_auth_for_remote_clients_when_enabled() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("config_read_auth", true).await?;
        let app = router(bot);

        let response = app.clone().oneshot(remote_request("/api/config")).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut ok_req = remote_request("/api/config");
        ok_req.headers_mut().insert("x-api-key", "local".parse()?);
        let response = app.oneshot(ok_req).await?;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await?;
        let payload: Value = serde_json::from_slice(&body)?;
        assert_eq!(payload.get("api_key").and_then(Value::as_str), Some("***"));

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn sensitive_rest_reads_require_the_same_key_as_the_live_stream() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("runtime_reads", true).await?;
        let app = router(Arc::clone(&bot));
        for endpoint in [
            "/api/trades",
            "/api/trades/analysis",
            "/api/signal-stats",
            "/api/attribution",
            "/api/rejections",
            "/api/report",
            "/api/decisions",
            "/api/accounting/truth",
            "/api/loop/status",
            "/api/risk/status",
            "/api/maker/state",
            "/api/analysis",
            "/api/stats",
            "/api/ml/state",
            "/api/deep-model/status",
            "/api/preflight",
            "/api/metrics",
        ] {
            let response = app.clone().oneshot(remote_request(endpoint)).await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{endpoint}");
        }
        for endpoint in [
            "/api/trades",
            "/api/stats",
            "/api/risk/status",
            "/api/preflight",
            "/api/metrics",
        ] {
            let mut req = remote_request(endpoint);
            req.headers_mut().insert("x-api-key", "local".parse()?);
            let response = app.clone().oneshot(req).await?;
            assert_eq!(response.status(), StatusCode::OK, "{endpoint}");
        }
        for endpoint in ["/api/health", "/api/ready"] {
            let response = app.clone().oneshot(remote_request(endpoint)).await?;
            assert_ne!(response.status(), StatusCode::UNAUTHORIZED, "{endpoint}");
            if endpoint == "/api/ready" {
                let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
                let payload: Value = serde_json::from_slice(&bytes)?;
                assert!(payload.get("preflight").is_none());
                assert!(payload.get("checks").is_none());
                assert!(payload.get("last_reconcile_error").is_none());
            }
        }
        let mut req = remote_request("/api/ready");
        req.headers_mut().insert("x-api-key", "local".parse()?);
        let response = app.oneshot(req).await?;
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
        let payload: Value = serde_json::from_slice(&bytes)?;
        assert!(payload.get("preflight").is_some());
        assert!(payload.get("checks").is_some());
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn dispatched_requests_keep_their_claim_when_the_actor_loses_its_reply(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("uncertain_dispatch", true).await?;
        *bot.analysis_state.last_analysis.write().await = serde_json::json!({"signal": "BUY_YES"});
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        *bot.execution_state.execution_tx.write().await = Some(tx);
        let actor = tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    crate::bot::actor::ExecutionCommand::ExecuteTrade { resp, .. }
                    | crate::bot::actor::ExecutionCommand::CloseTrade { resp, .. } => drop(resp),
                    _ => panic!("unexpected command"),
                }
            }
        });
        let app = router(Arc::clone(&bot));
        for endpoint in ["/api/trade", "/api/trades/999/close"] {
            let request = signed_request_with_idempotency(
                axum::http::Method::POST,
                endpoint,
                "{}",
                "lost-reply",
            );
            let response = app.clone().oneshot(request).await?;
            assert_eq!(
                response.status(),
                StatusCode::INTERNAL_SERVER_ERROR,
                "{endpoint}"
            );
            let retry = signed_request_with_idempotency(
                axum::http::Method::POST,
                endpoint,
                "{}",
                "lost-reply",
            );
            let response = app.clone().oneshot(retry).await?;
            assert_eq!(
                response.status(),
                StatusCode::CONFLICT,
                "a lost reply must not make {endpoint} eligible for a second submission"
            );
        }
        actor.abort();
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn config_get_stays_public_when_read_auth_disabled() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("config_public", false).await?;
        let app = router(bot);

        let response = app.oneshot(loopback_request("/api/config")).await?;
        assert_eq!(response.status(), StatusCode::OK);

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn stream_token_round_trip_requires_read_auth_and_unlocks_sse() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("stream_token_round_trip", true).await?;
        let app = router(bot);

        let response = app.clone().oneshot(remote_request("/api/stream")).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .clone()
            .oneshot(remote_request("/api/stream/token"))
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut token_req = remote_request("/api/stream/token");
        token_req
            .headers_mut()
            .insert("x-api-key", "local".parse()?);
        let response = app.clone().oneshot(token_req).await?;
        assert_eq!(response.status(), StatusCode::OK);

        let body = to_bytes(response.into_body(), 1024 * 1024).await?;
        let payload: Value = serde_json::from_slice(&body)?;
        let token = payload
            .get("token")
            .and_then(Value::as_str)
            .expect("stream token");

        let response = app
            .clone()
            .oneshot(remote_request(&format!("/api/stream?stream_token={token}")))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);

        let mut wrong_ip_req = Request::builder()
            .uri(format!("/api/stream?stream_token={token}"))
            .body(Body::empty())?;
        wrong_ip_req
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 9], 41001))));
        let response = app.oneshot(wrong_ip_req).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn stream_access_is_revoked_when_credentials_or_ip_policy_change() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("stream_revocation", true)
            .await
            .expect("authorized test request");
        let app = router(Arc::clone(&bot));
        let ip = Some("10.0.0.8".to_string());
        let token = super::auth::issue_stream_token(&bot, ip.clone(), Some("local"))
            .await
            .expect("authorized test request");
        let grant = super::auth::authorize_stream(&bot, ip.clone(), None, Some(&token))
            .await
            .expect("authorized test request");
        assert!(grant.remains_authorized(&bot).await);
        let response = app
            .clone()
            .oneshot(remote_request(&format!("/api/stream?stream_token={token}")))
            .await
            .expect("authorized test request");
        assert_eq!(response.status(), StatusCode::OK);

        bot.control_plane.config.write().await.api_key = "replacement-key".into();
        assert!(!grant.remains_authorized(&bot).await);
        let ended = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            to_bytes(response.into_body(), 4096),
        )
        .await??;
        assert!(
            ended.is_empty(),
            "a revoked stream must stop before emitting private data"
        );
        let revoked = app
            .clone()
            .oneshot(remote_request(&format!("/api/stream?stream_token={token}")))
            .await
            .expect("authorized test request");
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);

        let token = super::auth::issue_stream_token(&bot, ip.clone(), Some("replacement-key"))
            .await
            .expect("authorized test request");
        let grant = super::auth::authorize_stream(&bot, ip.clone(), None, Some(&token))
            .await
            .expect("authorized test request");
        bot.control_plane.config.write().await.allowed_ips = vec!["127.0.0.1".into()];
        assert!(!grant.remains_authorized(&bot).await);
        assert_eq!(
            super::auth::authorize_stream(&bot, ip.clone(), None, Some(&token))
                .await
                .err(),
            Some(StatusCode::FORBIDDEN)
        );
        bot.control_plane.config.write().await.allowed_ips.clear();
        bot.control_plane.config.write().await.api_hmac_secret = "replacement-secret".into();
        assert!(!grant.remains_authorized(&bot).await);
        assert_eq!(
            super::auth::authorize_stream(&bot, ip, None, Some(&token))
                .await
                .err(),
            Some(StatusCode::UNAUTHORIZED)
        );
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn integrity_report_requires_read_auth_when_enabled() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("integrity_report_auth", true).await?;
        let app = router(bot);

        let response = app
            .clone()
            .oneshot(remote_request("/api/integrity/report"))
            .await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut ok_req = remote_request("/api/integrity/report");
        ok_req.headers_mut().insert("x-api-key", "local".parse()?);
        let response = app.oneshot(ok_req).await?;
        assert_eq!(response.status(), StatusCode::OK);

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn loopback_and_forwarded_requests_require_credentials() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("loopback_auth", true).await?;
        let app = router(bot.clone());

        let response = app.clone().oneshot(loopback_request("/api/config")).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        for endpoint in ["/api/integrity/report", "/api/stream/token", "/api/stream"] {
            let response = app.clone().oneshot(loopback_request(endpoint)).await?;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{endpoint}");
        }

        let mut update_req = loopback_request("/api/config");
        *update_req.method_mut() = axum::http::Method::POST;
        *update_req.body_mut() = Body::from(r#"{"execution_mode":"maker"}"#);
        update_req.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse()?,
        );
        update_req
            .headers_mut()
            .insert("x-forwarded-for", "203.0.113.9".parse()?);
        let response = app.clone().oneshot(update_req).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let mut key_only = signed_config_request(r#"{"execution_mode":"maker"}"#);
        key_only.headers_mut().remove("x-api-signature");
        let response = app.clone().oneshot(key_only).await?;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(signed_config_request(r#"{"execution_mode":"maker"}"#))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);

        let cfg = bot.control_plane.config.read().await;
        assert_eq!(cfg.execution_mode, "maker");

        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn invalid_config_patch_preserves_memory_and_disk() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("invalid_config", true).await?;
        let before = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let disk_before = std::fs::read(path.with_extension("json"))?;
        let response = router(bot.clone())
            .oneshot(signed_config_request(
                r#"{"min_entry_price":0.9,"max_entry_price":0.2}"#,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::to_value(bot.control_plane.config.read().await.clone())?,
            before
        );
        assert_eq!(std::fs::read(path.with_extension("json"))?, disk_before);
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn misspelled_config_patch_is_rejected_without_changing_memory_or_disk(
    ) -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("unknown_config", true).await?;
        let before = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let disk_before = std::fs::read(path.with_extension("json"))?;
        let response = router(Arc::clone(&bot))
            .oneshot(signed_config_request(
                r#"{"daily_loss_limt":2,"trade_amount":10}"#,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::to_value(bot.control_plane.config.read().await.clone())?,
            before
        );
        assert_eq!(std::fs::read(path.with_extension("json"))?, disk_before);
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn failed_config_save_preserves_runtime_state() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("failed_config_save", true).await?;
        let before = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let balance_before = bot.balance().await;
        let config_path = path.with_extension("json");
        // A directory at the destination deterministically prevents atomic rename.
        std::fs::remove_file(&config_path)?;
        std::fs::create_dir(&config_path)?;
        let response = router(bot.clone())
            .oneshot(signed_config_request(r#"{"execution_mode":"maker"}"#))
            .await?;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            serde_json::to_value(bot.control_plane.config.read().await.clone())?,
            before
        );
        assert_eq!(bot.balance().await, balance_before);
        std::fs::remove_dir(&config_path)?;
        let _ = std::fs::remove_file(std::path::PathBuf::from(format!(
            "{}.tmp",
            config_path.display()
        )));
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn balance_reset_persists_the_same_cash_that_restart_reconstructs() -> anyhow::Result<()>
    {
        let (bot, path, path_str) = auth_test_bot("balance_reset_replay", true).await?;
        bot.start_execution_actor().await;
        let closed = crate::db::Trade {
            status: "CLOSED".into(),
            mode: "simulation".into(),
            pnl: Some(12.0),
            fee_paid: 2.0,
            ..Default::default()
        };
        crate::db::insert_trade(&bot.db, &closed).await?;
        let mut open = crate::db::Trade {
            status: "OPEN".into(),
            mode: "simulation".into(),
            amount: 20.0,
            fee_paid: 1.0,
            pnl: Some(3.0),
            ..Default::default()
        };
        open.id = crate::db::insert_trade(&bot.db, &open).await?;
        *bot.portfolio_state.trades.write().await = vec![open];
        let response = router(Arc::clone(&bot))
            .oneshot(signed_post_request(
                "/api/balance/reset",
                r#"{"amount":150}"#,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bot.balance().await, 132.0);
        assert_eq!(bot.control_plane.config.read().await.initial_balance, 140.0);
        let cfg = serde_json::from_slice(&std::fs::read(path.with_extension("json"))?)?;
        let restarted = TradingBot::new(cfg, bot.db.clone()).await?;
        assert_eq!(restarted.balance().await, bot.balance().await);
        drop(restarted);
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn failed_balance_reset_save_preserves_runtime_balance_and_config() -> anyhow::Result<()>
    {
        let (bot, path, path_str) = auth_test_bot("failed_balance_reset", true).await?;
        bot.start_execution_actor().await;
        let before = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let balance_before = bot.balance().await;
        let config_path = path.with_extension("json");
        std::fs::remove_file(&config_path)?;
        std::fs::create_dir(&config_path)?;
        let response = router(Arc::clone(&bot))
            .oneshot(signed_post_request(
                "/api/balance/reset",
                r#"{"amount":100}"#,
            ))
            .await?;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            serde_json::to_value(bot.control_plane.config.read().await.clone())?,
            before
        );
        assert_eq!(bot.balance().await, balance_before);
        std::fs::remove_dir(&config_path)?;
        let _ = std::fs::remove_file(std::path::PathBuf::from(format!(
            "{}.tmp",
            config_path.display()
        )));
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn valid_config_patch_publishes_the_persisted_normalized_config() -> anyhow::Result<()> {
        let (bot, path, path_str) = auth_test_bot("valid_config", true).await?;
        let response = router(bot.clone())
            .oneshot(signed_config_request(r#"{"edge_safety_buffer":-1.0}"#))
            .await?;
        assert_eq!(response.status(), StatusCode::OK);
        let current = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let persisted: Value =
            serde_json::from_slice(&std::fs::read(path.with_extension("json"))?)?;
        assert_eq!(current, persisted);
        assert_eq!(current["edge_safety_buffer"], 0.0);
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn initial_balance_config_patch_is_refused_before_accounting_read() -> anyhow::Result<()>
    {
        let (bot, path, path_str) = auth_test_bot("failed_balance_read", true).await?;
        let before = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let disk_before = std::fs::read(path.with_extension("json"))?;
        let balance_before = bot.balance().await;
        // Keep the replay ledger writable. Refusal must precede an unavailable
        // accounting read rather than attempting to reset outside the actor.
        sqlx::query("DROP TABLE trades").execute(&bot.db).await?;
        let response = router(bot.clone())
            .oneshot(signed_config_request(r#"{"initial_balance":100}"#))
            .await?;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::to_value(bot.control_plane.config.read().await.clone())?,
            before
        );
        assert_eq!(std::fs::read(path.with_extension("json"))?, disk_before);
        assert_eq!(bot.balance().await, balance_before);
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    // This current-thread test holds the process environment lease through async fixture work.
    #[allow(clippy::await_holding_lock)]
    async fn initial_balance_config_patch_is_atomic_despite_sensitive_override(
    ) -> anyhow::Result<()> {
        let _environment = crate::env_util::test_env_lock();
        const KEY: &str = "MACBOT_ALLOW_SENSITIVE_CONFIG_API";
        struct RestoreSensitiveOverride(Option<std::ffi::OsString>);
        impl Drop for RestoreSensitiveOverride {
            fn drop(&mut self) {
                if let Some(previous) = &self.0 {
                    std::env::set_var(KEY, previous);
                } else {
                    std::env::remove_var(KEY);
                }
            }
        }
        let _restore = RestoreSensitiveOverride(std::env::var_os(KEY));
        let (bot, path, path_str) = auth_test_bot("initial_balance_config_refusal", true).await?;
        let now = chrono::Utc::now().to_rfc3339();
        let closed = crate::db::Trade {
            status: "CLOSED".into(),
            mode: "simulation".into(),
            pnl: Some(12.0),
            fee_paid: 2.0,
            exit_timestamp: Some(now),
            ..Default::default()
        };
        crate::db::insert_trade(&bot.db, &closed).await?;
        crate::db::risk::reset_cash_peak(&bot.db, "simulation", 77.0).await?;
        // Deliberately differ from the accounting reconstruction, as can occur
        // while an actor-owned trade is awaiting persistence. The config path
        // must not publish a competing cash value in any case.
        *bot.portfolio_state.balance.write().await = 47.5;
        let before = serde_json::to_value(bot.control_plane.config.read().await.clone())?;
        let disk_before = std::fs::read(path.with_extension("json"))?;
        let ledger_before =
            serde_json::to_value(crate::db::balance_reconstruction_components(&bot.db).await?)?;
        let peak_before: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='drawdown_cash_peak:simulation'")
                .fetch_one(&bot.db)
                .await?;
        let app = router(Arc::clone(&bot));
        let body = r#"{"initial_balance":150,"execution_mode":"maker","mode":"live"}"#;
        let mut unsigned = signed_config_request(body);
        unsigned.headers_mut().remove("x-api-signature");
        assert_eq!(
            app.clone().oneshot(unsigned).await?.status(),
            StatusCode::UNAUTHORIZED
        );
        {
            let _recovery = bot.execution_state.recovery_control_gate.lock().await;
            assert_eq!(
                app.clone().oneshot(signed_config_request(body)).await?.status(),
                StatusCode::BAD_REQUEST,
                "this permanently read-only field must direct callers to the actor reset even during recovery"
            );
        }
        for override_value in [None, Some("1"), Some("true")] {
            if let Some(value) = override_value {
                std::env::set_var(KEY, value);
            } else {
                std::env::remove_var(KEY);
            }
            let response = app.clone().oneshot(signed_config_request(body)).await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            let message = String::from_utf8(bytes.to_vec())?;
            assert!(message.contains("initial_balance is read-only"));
            assert!(message.contains("signed POST /api/balance/reset"));
            assert_eq!(
                serde_json::to_value(bot.control_plane.config.read().await.clone())?,
                before
            );
            assert_eq!(std::fs::read(path.with_extension("json"))?, disk_before);
            assert_eq!(bot.balance().await, 47.5);
            assert_eq!(
                serde_json::to_value(crate::db::balance_reconstruction_components(&bot.db).await?)?,
                ledger_before
            );
            let peak: String = sqlx::query_scalar(
                "SELECT value FROM meta WHERE key='drawdown_cash_peak:simulation'",
            )
            .fetch_one(&bot.db)
            .await?;
            assert_eq!(peak, peak_before);
        }
        // The presence of the key, even with an unchanged or malformed value,
        // must be refused rather than reaching deserialization or persistence.
        for value in [
            before["initial_balance"].clone(),
            Value::Null,
            Value::from(-1),
        ] {
            let response = app
                .clone()
                .oneshot(signed_config_request(
                    &serde_json::json!({"initial_balance": value, "trade_amount": 10}).to_string(),
                ))
                .await?;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            serde_json::to_value(bot.control_plane.config.read().await.clone())?,
            before
        );
        assert_eq!(std::fs::read(path.with_extension("json"))?, disk_before);
        assert_eq!(bot.balance().await, 47.5);
        bot.db.close().await;
        cleanup_sqlite(&path, &path_str);
        Ok(())
    }
}
