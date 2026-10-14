use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{to_bytes, Body};
use axum::extract::{connect_info::ConnectInfo, State};
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use rand::RngCore;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::bot::state::{StreamTokenEntry, TradingBot};
use crate::config::Config;
use crate::domain::BotMode;

type HmacSha256 = Hmac<Sha256>;

/// Bounded retention makes replay claims safe to prune even after a skew
/// configuration change. A timestamp older than this can never authenticate.
const MAX_AUTH_SKEW_SECONDS: u64 = 300;

/// Axum 0.7's Json extractor can deserialize the first value while ignoring
/// trailing bytes. Preserve the exact signed body, but require the complete
/// JSON document before handing a mutation to its typed extractor.
pub async fn require_complete_json(req: Request<Body>, next: Next) -> Response {
    let is_json = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase())
        .is_some_and(|value| {
            value == "application/json"
                || (value.starts_with("application/") && value.ends_with("+json"))
        });
    if !is_json {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    // Control commands without JSON arguments may have an empty body. Typed
    // Json handlers still reject that themselves; nonempty bodies must be exact.
    if !bytes.is_empty() && serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        return (
            StatusCode::BAD_REQUEST,
            "body must contain exactly one valid JSON value",
        )
            .into_response();
    }
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

pub(super) fn request_canonical(
    timestamp: &str,
    nonce: &str,
    method: &str,
    path: &str,
    idempotency: &str,
    body: &str,
) -> String {
    serde_json::json!([timestamp, nonce, method, path, idempotency, body]).to_string()
}

async fn claim_request_nonce(
    pool: &sqlx::SqlitePool,
    nonce: &str,
    timestamp: i64,
    now: i64,
) -> Result<bool, sqlx::Error> {
    let key = format!(
        "api_auth_nonce:{}",
        hex::encode(Sha256::digest(nonce.as_bytes()))
    );
    let expires = timestamp.saturating_add(MAX_AUTH_SKEW_SECONDS as i64);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    // Expired nonces can only stay expired while UTC does not move backward.
    // Keep this bound durably with the claims so a clock step cannot reopen a
    // previously pruned signature window, including after a process restart.
    let highest: Option<String> =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='api_auth_clock_high_water'")
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(highest) = highest {
        let highest = highest.parse::<i64>().map_err(|_| {
            sqlx::Error::Protocol("API authentication clock evidence is invalid".into())
        })?;
        if now < highest {
            return Err(sqlx::Error::Protocol(
                "UTC moved backward; signed controls are paused until the recorded authentication time is reached".into(),
            ));
        }
    }
    sqlx::query("INSERT INTO meta(key,value) VALUES('api_auth_clock_high_water',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(now.to_string())
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM meta WHERE key GLOB 'api_auth_nonce:*' AND CAST(value AS INTEGER) < ?",
    )
    .bind(now)
    .execute(&mut *tx)
    .await?;
    let claimed =
        sqlx::query("INSERT INTO meta(key,value) VALUES(?,?) ON CONFLICT(key) DO NOTHING")
            .bind(key)
            .bind(expires.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected()
            == 1;
    tx.commit().await?;
    Ok(claimed)
}

pub async fn require_api_auth(
    State(bot): State<Arc<TradingBot>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let client_ip = extract_client_ip(&req);
    let path = req.uri().path().to_string();
    let cfg = bot.control_plane.config.read().await.clone();
    let auth_required = auth_required_for_request(&cfg, &path);
    if !auth_required {
        return next.run(req).await;
    }

    if let Err(status) = enforce_allowed_ip(&cfg, client_ip) {
        return (status, "ip not allowlisted").into_response();
    }

    let (parts, body) = req.into_parts();
    let body_bytes = match to_bytes(body, 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };
    let body_str = match std::str::from_utf8(&body_bytes) {
        Ok(body) => body.to_string(),
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "request body must be valid UTF-8").into_response()
        }
    };
    let req = Request::from_parts(parts, Body::from(body_bytes));

    let expected_key = current_api_key(&cfg);
    if let Some(expected_key) = expected_key {
        let provided = req
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !secure_eq(&expected_key, provided) {
            return (StatusCode::UNAUTHORIZED, "invalid api key").into_response();
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "api key not configured").into_response();
    }

    let expected_secret = current_hmac_secret(&cfg);
    let Some(secret) = expected_secret else {
        return (StatusCode::UNAUTHORIZED, "hmac secret not configured").into_response();
    };

    let ts_header = req
        .headers()
        .get("x-api-ts")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i64>().ok());
    let sig_header = req
        .headers()
        .get("x-api-signature")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_lowercase());
    let nonce_header = req
        .headers()
        .get("x-api-nonce")
        .and_then(|v| v.to_str().ok())
        .filter(|nonce| nonce.len() == 64 && nonce.bytes().all(|b| b.is_ascii_hexdigit()));

    let (ts, provided_sig, nonce) = match (ts_header, sig_header, nonce_header) {
        (Some(ts), Some(sig), Some(nonce)) => (ts, sig, nonce),
        _ => return (StatusCode::UNAUTHORIZED, "missing auth signature headers").into_response(),
    };

    let now = chrono::Utc::now().timestamp();
    let max_skew = cfg.api_max_skew_seconds.clamp(1, MAX_AUTH_SKEW_SECONDS);
    if now.abs_diff(ts) > max_skew {
        return (StatusCode::UNAUTHORIZED, "timestamp skew too large").into_response();
    }

    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(req.uri().path());
    let timestamp_text = req
        .headers()
        .get("x-api-ts")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let idempotency = match req.headers().get("x-idempotency-key") {
        Some(value) => match value.to_str() {
            Ok(value) => value,
            Err(_) => {
                return (StatusCode::UNAUTHORIZED, "invalid idempotency header").into_response()
            }
        },
        None => "",
    };
    let canonical = request_canonical(
        timestamp_text,
        nonce,
        req.method().as_str(),
        path,
        idempotency,
        &body_str,
    );
    let Some(expected_sig) = hmac_hex(&secret, &canonical) else {
        return (StatusCode::UNAUTHORIZED, "invalid hmac secret").into_response();
    };
    if !secure_eq(&expected_sig, &provided_sig) {
        return (StatusCode::UNAUTHORIZED, "invalid request signature").into_response();
    }

    // A verified request must consume its nonce durably before the handler
    // runs, including when a later execution response is uncertain. Retries
    // sign a fresh nonce and reuse their execution idempotency key.
    match claim_request_nonce(&bot.db, nonce, ts, now).await {
        Ok(true) => {}
        Ok(false) => {
            return (StatusCode::UNAUTHORIZED, "request nonce already used").into_response()
        }
        Err(error) => {
            warn!("Cannot persist API replay guard: {error}");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "request replay guard unavailable",
            )
                .into_response();
        }
    }

    next.run(req).await
}

pub async fn require_read_auth(
    State(bot): State<Arc<TradingBot>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let client_ip = extract_client_ip(&req);
    let provided_key = req.headers().get("x-api-key").and_then(|v| v.to_str().ok());

    if let Err(status) = authorize_read_only(&bot, client_ip, provided_key).await {
        return (status, "read auth failed").into_response();
    }

    next.run(req).await
}

pub async fn authorize_read_only(
    bot: &Arc<TradingBot>,
    client_ip: Option<String>,
    provided_key: Option<&str>,
) -> Result<(), StatusCode> {
    let cfg = bot.control_plane.config.read().await.clone();
    authorize_read_with_config(&cfg, client_ip, provided_key)
}

fn authorize_read_with_config(
    cfg: &Config,
    client_ip: Option<String>,
    provided_key: Option<&str>,
) -> Result<(), StatusCode> {
    let auth_required =
        cfg.auth_required || cfg.mode_kind() == BotMode::Live || !cfg.private_rpc_url.is_empty();
    if !auth_required {
        return Ok(());
    }

    enforce_allowed_ip(cfg, client_ip)?;

    let Some(expected_key) = current_api_key(cfg) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    let Some(provided) = provided_key.filter(|v| !v.trim().is_empty()) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if !secure_eq(&expected_key, provided) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(())
}

/// A connected stream keeps its grant until credentials or access policy change.
/// Token expiry limits new handshakes; it does not disconnect a valid session.
#[derive(Clone)]
pub struct StreamAuthorization {
    client_ip: Option<String>,
    auth_fingerprint: [u8; 32],
}

impl StreamAuthorization {
    pub async fn remains_authorized(&self, bot: &Arc<TradingBot>) -> bool {
        let cfg = bot.control_plane.config.read().await;
        self.auth_fingerprint == auth_fingerprint(&cfg)
            && enforce_allowed_ip(&cfg, self.client_ip.clone()).is_ok()
    }
}

fn auth_fingerprint(cfg: &Config) -> [u8; 32] {
    // Hash a structured representation to avoid ambiguous concatenations. No
    // credential is retained in the token map or written into a stream URL.
    let policy = serde_json::json!([
        cfg.auth_required,
        cfg.mode_kind() == BotMode::Live,
        !cfg.private_rpc_url.is_empty(),
        current_api_key(cfg),
        current_hmac_secret(cfg),
    ]);
    Sha256::digest(policy.to_string().as_bytes()).into()
}

pub async fn authorize_stream(
    bot: &Arc<TradingBot>,
    client_ip: Option<String>,
    provided_key: Option<&str>,
    provided_token: Option<&str>,
) -> Result<StreamAuthorization, StatusCode> {
    let cfg = bot.control_plane.config.read().await.clone();
    if let Some(key) = provided_key.filter(|v| !v.trim().is_empty()) {
        authorize_read_with_config(&cfg, client_ip.clone(), Some(key))?;
    } else if let Some(token) = provided_token.filter(|v| !v.trim().is_empty()) {
        authorize_stream_token(bot, &cfg, client_ip.clone(), token)?;
    } else {
        authorize_read_with_config(&cfg, client_ip.clone(), None)?;
    }
    Ok(StreamAuthorization {
        client_ip,
        auth_fingerprint: auth_fingerprint(&cfg),
    })
}

pub async fn issue_stream_token(
    bot: &Arc<TradingBot>,
    client_ip: Option<String>,
    provided_key: Option<&str>,
) -> Result<String, StatusCode> {
    let cfg = bot.control_plane.config.read().await.clone();
    authorize_read_with_config(&cfg, client_ip.clone(), provided_key)?;
    prune_stream_tokens(bot);

    let mut raw = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut raw);
    let token = hex::encode(raw);
    bot.control_plane.stream_tokens.insert(
        token.clone(),
        StreamTokenEntry {
            ts: Instant::now(),
            client_ip,
            auth_fingerprint: auth_fingerprint(&cfg),
        },
    );
    Ok(token)
}

pub fn auth_required_for_request(cfg: &Config, path: &str) -> bool {
    cfg.auth_required
        || cfg.mode_kind() == BotMode::Live
        || !cfg.private_rpc_url.is_empty()
        || path_requires_always_on_auth(path)
}

fn path_requires_always_on_auth(path: &str) -> bool {
    matches!(
        path,
        "/api/config" | "/api/trade" | "/api/trade/force" | "/api/analyze" | "/api/backtest"
    ) || path.starts_with("/api/rpc/")
}

fn extract_client_ip(req: &Request<Body>) -> Option<String> {
    // Prefer ConnectInfo (trusted, from TCP socket) over X-Forwarded-For (spoofable)
    if let Some(ci) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return Some(ci.0.ip().to_string());
    }
    let trust_xff = std::env::var("MACBOT_TRUST_XFF")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if trust_xff {
        // Fallback to X-Forwarded-For only when explicitly trusted (e.g. behind proxy)
        if let Some(forwarded) = req
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
        {
            warn!("Using X-Forwarded-For for client IP (MACBOT_TRUST_XFF=1)");
            if let Some(first) = forwarded.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return Some(ip.to_string());
                }
            }
        }
    }
    None
}

fn hmac_hex(secret: &str, data: &str) -> Option<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).ok()?;
    mac.update(data.as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

pub(crate) fn stream_token_ttl_secs() -> u64 {
    std::env::var("MACBOT_STREAM_TOKEN_TTL_SECONDS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|ttl| ttl.max(5))
        .unwrap_or(120)
}

fn prune_stream_tokens(bot: &Arc<TradingBot>) {
    let ttl_secs = stream_token_ttl_secs();
    let stale: Vec<String> = bot
        .control_plane
        .stream_tokens
        .iter()
        .filter(|entry| entry.ts.elapsed().as_secs() > ttl_secs)
        .map(|entry| entry.key().clone())
        .collect();
    for key in stale {
        bot.control_plane.stream_tokens.remove(&key);
    }
}

fn authorize_stream_token(
    bot: &Arc<TradingBot>,
    cfg: &Config,
    client_ip: Option<String>,
    token: &str,
) -> Result<(), StatusCode> {
    enforce_allowed_ip(cfg, client_ip.clone())?;
    prune_stream_tokens(bot);
    let Some(entry) = bot.control_plane.stream_tokens.get(token) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if entry.auth_fingerprint != auth_fingerprint(cfg)
        || entry.ts.elapsed().as_secs() > stream_token_ttl_secs()
    {
        drop(entry);
        bot.control_plane.stream_tokens.remove(token);
        return Err(StatusCode::UNAUTHORIZED);
    }
    if let Some(expected_ip) = entry.client_ip.as_deref() {
        if client_ip.as_deref() != Some(expected_ip) {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }
    Ok(())
}

fn secure_eq(a: &str, b: &str) -> bool {
    // Pad to equal length to avoid leaking length information
    let max_len = a.len().max(b.len()).max(1);
    let mut a_padded = vec![0u8; max_len];
    let mut b_padded = vec![0u8; max_len];
    a_padded[..a.len()].copy_from_slice(a.as_bytes());
    b_padded[..b.len()].copy_from_slice(b.as_bytes());
    // Also compare lengths in constant time
    let len_eq: bool = a.len().to_le_bytes().ct_eq(&b.len().to_le_bytes()).into();
    let content_eq: bool = a_padded.ct_eq(&b_padded).into();
    len_eq && content_eq
}

fn read_secret(primary: Option<String>, secondary: Option<String>) -> Option<String> {
    [primary, secondary]
        .into_iter()
        .flatten()
        .find(|v| !v.trim().is_empty())
}

fn current_api_key(cfg: &Config) -> Option<String> {
    read_secret(
        std::env::var("MACBOT_API_KEY").ok(),
        Some(cfg.api_key.clone()),
    )
    .map(|value| value.trim().to_string())
}

fn current_hmac_secret(cfg: &Config) -> Option<String> {
    read_secret(
        read_secret(
            std::env::var("MACBOT_API_HMAC_SECRET").ok(),
            std::env::var("MACBOT_API_SECRET").ok(),
        ),
        Some(cfg.api_hmac_secret.clone()),
    )
}

fn enforce_allowed_ip(cfg: &Config, client_ip: Option<String>) -> Result<(), StatusCode> {
    if cfg.allowed_ips.is_empty() {
        return Ok(());
    }
    let allowed = client_ip
        .as_ref()
        .map(|ip| cfg.allowed_ips.iter().any(|a| a == ip))
        .unwrap_or(false);
    if !allowed {
        warn!(
            "API auth blocked request from non-allowlisted IP: {:?}",
            client_ip
        );
        Err(StatusCode::FORBIDDEN)
    } else {
        Ok(())
    }
}

// ── Rate limiter ────────────────────────────────────────────────────────────

const PUBLIC_RATE_LIMIT_REQUESTS: u64 = 300;
const PUBLIC_RATE_LIMIT_WINDOW_SECS: u64 = 60;
const RATE_LIMIT_REQUESTS: u64 = 60;
const RATE_LIMIT_WINDOW_SECS: u64 = 60;

static PUBLIC_RATE_MAP: Lazy<DashMap<String, VecDeque<Instant>>> = Lazy::new(DashMap::new);
static RATE_MAP: Lazy<DashMap<String, VecDeque<Instant>>> = Lazy::new(DashMap::new);

pub async fn rate_limit(req: Request<Body>, next: Next) -> Response {
    rate_limit_with_map(
        req,
        next,
        &RATE_MAP,
        RATE_LIMIT_REQUESTS,
        RATE_LIMIT_WINDOW_SECS,
    )
    .await
}

pub async fn public_rate_limit(req: Request<Body>, next: Next) -> Response {
    if should_skip_public_rate_limit(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    rate_limit_with_map(
        req,
        next,
        &PUBLIC_RATE_MAP,
        PUBLIC_RATE_LIMIT_REQUESTS,
        PUBLIC_RATE_LIMIT_WINDOW_SECS,
    )
    .await
}

fn should_skip_public_rate_limit(method: &Method, path: &str) -> bool {
    if !path.starts_with("/api/") {
        return true;
    }

    matches!(
        path,
        "/api/health" | "/api/ready" | "/api/stream" | "/api/ws"
    ) || !matches!(*method, Method::GET)
}

async fn rate_limit_with_map(
    req: Request<Body>,
    next: Next,
    map: &DashMap<String, VecDeque<Instant>>,
    max_requests: u64,
    window_secs: u64,
) -> Response {
    let ip = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    match record_rate_limit_hit(map, ip, Instant::now(), max_requests, window_secs) {
        Ok(()) => next.run(req).await,
        Err(retry_after_secs) => {
            let mut resp = (StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded").into_response();
            if let Ok(value) = HeaderValue::from_str(&retry_after_secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, value);
            }
            resp
        }
    }
}

/// Hard cap on unique IPs tracked in each rate limiter map to prevent memory
/// exhaustion from distributed attacks with many unique source addresses.
const MAX_RATE_MAP_ENTRIES: usize = 50_000;

fn record_rate_limit_hit(
    map: &DashMap<String, VecDeque<Instant>>,
    ip: String,
    now: Instant,
    max_requests: u64,
    window_secs: u64,
) -> Result<(), u64> {
    // If the map exceeds the hard cap, reject new IPs to prevent OOM.
    // Existing IPs are still served so legitimate clients aren't affected.
    if map.len() >= MAX_RATE_MAP_ENTRIES && !map.contains_key(&ip) {
        return Err(window_secs.max(1));
    }

    let mut entry = map.entry(ip).or_default();

    while let Some(ts) = entry.front().copied() {
        if now.duration_since(ts).as_secs() >= window_secs {
            entry.pop_front();
        } else {
            break;
        }
    }

    if entry.len() >= max_requests as usize {
        let retry_after_secs = entry
            .front()
            .copied()
            .map(|ts| {
                window_secs
                    .saturating_sub(now.duration_since(ts).as_secs())
                    .max(1)
            })
            .unwrap_or(1);
        return Err(retry_after_secs);
    }

    entry.push_back(now);
    Ok(())
}

/// Prune stale entries from the rate-limiter map.  Called periodically to
/// prevent unbounded growth when many unique IPs connect once and never return.
pub fn prune_rate_map() {
    prune_rate_map_entries(&RATE_MAP, RATE_LIMIT_WINDOW_SECS);
    prune_rate_map_entries(&PUBLIC_RATE_MAP, PUBLIC_RATE_LIMIT_WINDOW_SECS);
}

fn prune_rate_map_entries(map: &DashMap<String, VecDeque<Instant>>, window_secs: u64) {
    let now = Instant::now();
    map.retain(|_ip, deque| {
        while let Some(ts) = deque.front().copied() {
            if now.duration_since(ts).as_secs() >= window_secs {
                deque.pop_front();
            } else {
                break;
            }
        }
        !deque.is_empty()
    });
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::time::Duration;
    use std::time::Instant;

    use axum::http::Method;
    use dashmap::DashMap;

    use super::{
        auth_required_for_request, hmac_hex, read_secret, record_rate_limit_hit, secure_eq,
        should_skip_public_rate_limit, stream_token_ttl_secs,
    };
    use crate::config::Config;

    #[test]
    fn secure_eq_requires_exact_match() {
        assert!(secure_eq("abc", "abc"));
        assert!(!secure_eq("abc", "abC"));
        assert!(!secure_eq("abc", "abcd"));
    }

    #[test]
    fn read_secret_prefers_primary_then_secondary() {
        assert_eq!(
            read_secret(Some("x".into()), Some("y".into())).unwrap_or_default(),
            "x"
        );
        assert_eq!(
            read_secret(Some("".into()), Some("y".into())).unwrap_or_default(),
            "y"
        );
    }

    #[test]
    fn hmac_hex_is_deterministic() {
        let a = hmac_hex("secret", "msg").unwrap_or_default();
        let b = hmac_hex("secret", "msg").unwrap_or_default();
        assert_eq!(a, b);
        assert_ne!(a, hmac_hex("secret2", "msg").unwrap_or_default());
    }

    #[test]
    fn signature_framing_matches_the_python_and_browser_unicode_vector() {
        let nonce = "0".repeat(64);
        let canonical = super::request_canonical(
            "1700000000",
            &nonce,
            "POST",
            "/api/config?note=colon%3Aquote",
            "key:quote\"end",
            "{\"note\":\"é:🙂\\n\"}\n",
        );
        assert_eq!(
            hmac_hex("public-fixture-secret", &canonical).as_deref(),
            Some("82d0e2bdb1555f0d3e91d2e4df2fb533ec7f823074b93de4b2bfc437f65218e2")
        );
        assert_ne!(
            super::request_canonical("1", &nonce, "POST", "/api/trade", "key:a", "b"),
            super::request_canonical("1", &nonce, "POST", "/api/trade", "key", "a:b")
        );
    }

    #[test]
    fn always_auth_paths_require_auth_even_in_sim() {
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))
            .unwrap_or_else(|_| panic!("config fixture should parse"));
        cfg.mode = "simulation".to_string();
        cfg.auth_required = false;
        cfg.private_rpc_url.clear();
        assert!(auth_required_for_request(&cfg, "/api/config"));
        assert!(auth_required_for_request(&cfg, "/api/backtest"));
        assert!(auth_required_for_request(&cfg, "/api/trade"));
        assert!(!auth_required_for_request(&cfg, "/api/health"));
    }

    #[test]
    fn public_rate_limit_skips_health_and_non_get_routes() {
        assert!(should_skip_public_rate_limit(&Method::GET, "/"));
        assert!(should_skip_public_rate_limit(
            &Method::GET,
            "/static/api-auth.js"
        ));
        assert!(should_skip_public_rate_limit(&Method::GET, "/api/health"));
        assert!(!should_skip_public_rate_limit(
            &Method::GET,
            "/api/preflight"
        ));
        assert!(should_skip_public_rate_limit(&Method::GET, "/api/stream"));
        assert!(!should_skip_public_rate_limit(
            &Method::GET,
            "/api/backtest"
        ));
        assert!(should_skip_public_rate_limit(
            &Method::POST,
            "/api/backtest"
        ));
        assert!(!should_skip_public_rate_limit(&Method::GET, "/api/trades"));
    }

    #[test]
    fn record_rate_limit_hit_blocks_after_max_requests() {
        let map: DashMap<String, VecDeque<Instant>> = DashMap::new();
        let ip = "127.0.0.1".to_string();
        let now = Instant::now();

        assert!(record_rate_limit_hit(&map, ip.clone(), now, 2, 60).is_ok());
        assert!(record_rate_limit_hit(&map, ip.clone(), now, 2, 60).is_ok());
        assert!(record_rate_limit_hit(&map, ip, now, 2, 60).is_err());
    }

    #[test]
    fn record_rate_limit_hit_prunes_expired_entries_before_counting() {
        let map: DashMap<String, VecDeque<Instant>> = DashMap::new();
        let ip = "127.0.0.1".to_string();
        let old = Instant::now() - Duration::from_secs(120);

        assert!(record_rate_limit_hit(&map, ip.clone(), old, 1, 60).is_ok());
        assert!(record_rate_limit_hit(&map, ip.clone(), Instant::now(), 1, 60).is_ok());
        assert_eq!(map.get(&ip).map(|v| v.value().len()), Some(1));
    }

    #[test]
    fn stream_token_ttl_has_floor() {
        let _guard = crate::env_util::test_env_lock();
        std::env::set_var("MACBOT_STREAM_TOKEN_TTL_SECONDS", "1");
        assert_eq!(stream_token_ttl_secs(), 5);
        std::env::remove_var("MACBOT_STREAM_TOKEN_TTL_SECONDS");
    }

    #[tokio::test]
    async fn nonce_pruning_preserves_the_last_valid_second_and_unrelated_metadata(
    ) -> anyhow::Result<()> {
        let pool = crate::db::open(":memory:").await?;
        sqlx::query(
            "INSERT INTO meta(key,value) VALUES('drawdown_cash_peak:simulation','preserved')",
        )
        .execute(&pool)
        .await?;
        let nonce = "a".repeat(64);
        assert!(super::claim_request_nonce(&pool, &nonce, 1_000, 1_000).await?);
        assert!(!super::claim_request_nonce(&pool, &nonce, 1_000, 1_300).await?);
        assert!(super::claim_request_nonce(&pool, &nonce, 1_301, 1_301).await?);
        let unrelated: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='drawdown_cash_peak:simulation'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(unrelated, "preserved");
        pool.close().await;
        Ok(())
    }

    #[tokio::test]
    async fn backward_clock_cannot_reopen_a_pruned_nonce_after_restart() -> anyhow::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "macbot_auth_clock_{}.sqlite",
            rand::random::<u64>()
        ));
        let pool = crate::db::open(path.to_str().unwrap()).await?;
        let old_nonce = "a".repeat(64);
        assert!(super::claim_request_nonce(&pool, &old_nonce, 1_000, 1_000).await?);
        assert!(super::claim_request_nonce(&pool, &"b".repeat(64), 1_301, 1_301).await?);
        let retained: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE key GLOB 'api_auth_nonce:*'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(retained, 1, "the old claim really was pruned");
        pool.close().await;
        let pool = crate::db::open(path.to_str().unwrap()).await?;
        assert!(super::claim_request_nonce(&pool, &old_nonce, 1_000, 1_000)
            .await
            .is_err());
        let retained: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE key GLOB 'api_auth_nonce:*'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(retained, 1, "backward time must not mutate replay evidence");
        assert!(super::claim_request_nonce(&pool, &"c".repeat(64), 1_302, 1_302).await?);
        pool.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        Ok(())
    }

    #[tokio::test]
    async fn unreadable_authentication_clock_evidence_refuses_new_claims() -> anyhow::Result<()> {
        let pool = crate::db::open(":memory:").await?;
        sqlx::query(
            "INSERT INTO meta(key,value) VALUES('api_auth_clock_high_water','not-an-integer')",
        )
        .execute(&pool)
        .await?;
        assert!(
            super::claim_request_nonce(&pool, &"d".repeat(64), 1_000, 1_000)
                .await
                .is_err()
        );
        let claims: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM meta WHERE key GLOB 'api_auth_nonce:*'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(claims, 0);
        pool.close().await;
        Ok(())
    }
}
