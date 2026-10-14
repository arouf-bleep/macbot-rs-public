//! Config GET/POST handlers.
//!
//! POST accepts a *partial* JSON object — only the provided keys are merged
//! into the running Config, and the result is saved back to the booted config.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::{Map, Value};
use std::sync::Arc;

use crate::bot::state::TradingBot;
use crate::config::Config;

// ─────────────────────────────────────────────────────────────────────────────
// GET /api/config
// ─────────────────────────────────────────────────────────────────────────────
pub async fn get_config(State(bot): State<Arc<TradingBot>>) -> impl IntoResponse {
    let cfg = bot.control_plane.config.read().await;
    Json(cfg.redacted_value())
}

// ─────────────────────────────────────────────────────────────────────────────
// POST /api/config  — partial update
// ─────────────────────────────────────────────────────────────────────────────
pub async fn update_config(
    State(bot): State<Arc<TradingBot>>,
    Json(patch): Json<Value>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let patch_map: &Map<String, Value> = patch.as_object().ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            "body must be a JSON object".to_string(),
        )
    })?;

    // Capital-origin and cash publication must be serialized with entries and
    // exits by the execution actor. Even a sensitive-config override must not
    // revive this outside-actor accounting writer or partially apply a patch.
    if patch_map.contains_key("initial_balance") {
        return Err((
            StatusCode::BAD_REQUEST,
            "initial_balance is read-only in config; use signed POST /api/balance/reset for a non-live balance reset".to_string(),
        ));
    }

    let _recovery_control = bot
        .execution_state
        .recovery_control_gate
        .try_lock()
        .map_err(|_| {
            (
                StatusCode::CONFLICT,
                "receipt recovery is active; configuration changes are paused".to_string(),
            )
        })?;

    let allow_sensitive = std::env::var("MACBOT_ALLOW_SENSITIVE_CONFIG_API")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if !allow_sensitive && patch_map.keys().any(|k| is_sensitive_config_key(k)) {
        return Err((
            StatusCode::FORBIDDEN,
            "sensitive config keys must be changed outside the API".to_string(),
        ));
    }

    {
        let mut cfg = bot.control_plane.config.write().await;
        let updated_secret_fields: Vec<&str> = patch_map
            .keys()
            .filter(|key| is_persisted_secret_config_key(key))
            .map(String::as_str)
            .collect();

        // Serialise current config to a Value, merge the patch, deserialise back.
        let mut cfg_val = serde_json::to_value(&*cfg)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let cfg_before = cfg.clone();
        if let Some(unknown) = patch_map.keys().find(|key| cfg_val.get(*key).is_none()) {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("unknown config field: {unknown}"),
            ));
        }

        merge_json(&mut cfg_val, patch_map);

        let mut candidate: Config = serde_json::from_value(cfg_val)
            .map_err(|e| (StatusCode::BAD_REQUEST, format!("config schema error: {e}")))?;

        candidate
            .normalize_and_validate()
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

        candidate
            .save_runtime_with_secret_updates(
                &bot.control_plane.config_path,
                &updated_secret_fields,
            )
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        // A runtime config change used to leave no trace at all — no log line,
        // no snapshot — so "who switched this bot to hybrid, and when" had no
        // answer. Every accepted change is now on the record, old value and new.
        tracing::info!(
            "Config updated via API: {}",
            describe_config_change(&cfg_before, &candidate, patch_map)
        );
        if cfg_before.auto_trading && !candidate.auto_trading {
            bot.invalidate_automatic_entries();
        }
        *cfg = candidate;
    }

    let updated = bot.control_plane.config.read().await;
    Ok(Json(updated.redacted_value()))
}

/// One line per changed key: `key: old -> new`. Some provider credentials
/// are API-editable, so redact both versions before formatting any values.
fn describe_config_change(before: &Config, after: &Config, patch: &Map<String, Value>) -> String {
    let before_val = before.redacted_value();
    let after_val = after.redacted_value();
    let mut parts: Vec<String> = patch
        .keys()
        .map(|key| {
            let old = before_val
                .get(key)
                .map(Value::to_string)
                .unwrap_or_else(|| "∅".into());
            let new = after_val
                .get(key)
                .map(Value::to_string)
                .unwrap_or_else(|| "∅".into());
            format!("{key}: {old} -> {new}")
        })
        .collect();
    parts.sort();
    parts.join(", ")
}

#[cfg(test)]
mod change_log_tests {
    use super::*;

    #[test]
    fn a_config_change_is_described_as_old_to_new_per_key() {
        let mut before: Config =
            serde_json::from_str(include_str!("../../config.json")).expect("config");
        before.execution_mode = "maker".to_string();
        before.circuit_breaker_losses = 3;
        let mut after = before.clone();
        after.execution_mode = "hybrid".to_string();
        after.circuit_breaker_losses = 4;

        let mut patch = Map::new();
        patch.insert("execution_mode".into(), Value::String("hybrid".into()));
        patch.insert("circuit_breaker_losses".into(), Value::from(4));

        let line = describe_config_change(&before, &after, &patch);
        assert_eq!(
            line,
            "circuit_breaker_losses: 3 -> 4, execution_mode: \"maker\" -> \"hybrid\""
        );
    }

    #[test]
    fn config_change_log_redacts_old_and_new_credentials() {
        let mut before: Config =
            serde_json::from_str(include_str!("../../config.json")).expect("config");
        before.api_key = "dummy-old-api-key".into();
        before.api_hmac_secret = "dummy-old-hmac".into();
        before.private_rpc_url = "https://example.invalid/dummy-old-rpc".into();
        before.finnhub_api_key = "dummy-old-finnhub".into();
        before.lunarcrush_api_key = "dummy-old-lunarcrush".into();
        let mut after = before.clone();
        after.api_key = "dummy-new-api-key".into();
        after.api_hmac_secret = "dummy-new-hmac".into();
        after.private_rpc_url = "https://example.invalid/dummy-new-rpc".into();
        after.finnhub_api_key = "dummy-new-finnhub".into();
        after.lunarcrush_api_key = "dummy-new-lunarcrush".into();
        let after_raw = serde_json::to_value(&after).expect("value");
        let patch = [
            "api_key",
            "api_hmac_secret",
            "private_rpc_url",
            "finnhub_api_key",
            "lunarcrush_api_key",
        ]
        .into_iter()
        .map(|key| (key.to_string(), after_raw[key].clone()))
        .collect();

        let line = describe_config_change(&before, &after, &patch);
        assert!(
            !line.contains("dummy-"),
            "credentials reached the audit log"
        );
        for key in patch.keys() {
            assert!(line.contains(&format!("{key}: \"***\" -> \"***\"")));
        }
    }
}

fn is_sensitive_config_key(key: &str) -> bool {
    matches!(
        key,
        "mode"
            | "auth_required"
            | "api_key"
            | "api_hmac_secret"
            | "private_rpc_url"
            | "macro_event_calendar_path"
    )
}

fn is_persisted_secret_config_key(key: &str) -> bool {
    matches!(
        key,
        "api_key"
            | "api_hmac_secret"
            | "private_rpc_url"
            | "finnhub_api_key"
            | "lunarcrush_api_key"
    )
}

#[cfg(test)]
mod tests {
    use super::{is_persisted_secret_config_key, is_sensitive_config_key};

    #[test]
    fn sensitive_keys_are_restricted() {
        assert!(is_sensitive_config_key("mode"));
        assert!(is_sensitive_config_key("api_key"));
        assert!(is_sensitive_config_key("private_rpc_url"));
        assert!(is_sensitive_config_key("macro_event_calendar_path"));
        assert!(!is_sensitive_config_key("execution_mode"));
    }

    #[test]
    fn persisted_secret_keys_are_tracked() {
        assert!(is_persisted_secret_config_key("api_key"));
        assert!(is_persisted_secret_config_key("finnhub_api_key"));
        assert!(!is_persisted_secret_config_key("mode"));
    }
}

// ── Deep-merge helper ─────────────────────────────────────────────────────────
//
// For flat top-level keys, the patch value wins.
// For nested objects (e.g. signal_weights), we recurse so the caller can
// update only one sub-key without wiping the rest.
fn merge_json(base: &mut Value, patch: &Map<String, Value>) {
    for (key, patch_val) in patch {
        match base.get_mut(key) {
            Some(base_val) if base_val.is_object() && patch_val.is_object() => {
                // Recurse into nested object
                if let Some(patch_obj) = patch_val.as_object() {
                    merge_json(base_val, patch_obj);
                }
            }
            _ => {
                // Overwrite (or insert)
                if let Some(obj) = base.as_object_mut() {
                    obj.insert(key.clone(), patch_val.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod adversarial_sale_audit_tests {
    use super::*;
    use axum::{body::Body, extract::connect_info::ConnectInfo, http::Request};
    use hmac::{Hmac, Mac};
    use std::net::SocketAddr;
    use tower::ServiceExt;

    struct RestoreEnv(Vec<(&'static str, Option<String>)>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            for (name, previous) in &self.0 {
                match previous {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }

    #[tokio::test]
    async fn saved_auto_trading_disable_invalidates_queued_generation_across_reenable(
    ) -> anyhow::Result<()> {
        let path = std::env::temp_dir().join(format!(
            "macbot-auto-setting-{:032x}.json",
            rand::random::<u128>()
        ));
        let mut config: Config = serde_json::from_str(include_str!("../../config.json"))?;
        config.mode = "simulation".into();
        config.auto_trading = true;
        config.save(path.to_str().unwrap())?;
        let mut bot = TradingBot::new(config, crate::db::open(":memory:").await?).await?;
        Arc::get_mut(&mut bot.control_plane).unwrap().config_path =
            path.to_string_lossy().into_owned();
        let bot = Arc::new(bot);
        bot.start_loop();
        let original = bot.automatic_entry_origin();
        assert!(bot.entry_origin_allowed(original).await);
        update_config(
            State(Arc::clone(&bot)),
            Json(serde_json::json!({"auto_trading":false})),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.1))?;
        update_config(
            State(Arc::clone(&bot)),
            Json(serde_json::json!({"auto_trading":true})),
        )
        .await
        .map_err(|e| anyhow::anyhow!(e.1))?;
        assert!(
            !bot.entry_origin_allowed(original).await,
            "reenabling cannot revive a request queued before disable"
        );
        assert!(
            bot.entry_origin_allowed(bot.automatic_entry_origin()).await,
            "newly captured automatic requests retain supported operation"
        );
        bot.db.close().await;
        std::fs::remove_file(path)?;
        Ok(())
    }

    /// A normal writable-provider update must not convert unrelated process
    /// secrets into persisted credentials. The original source snapshot failed
    /// this actual signed route regression; only its disposable fixture changes.
    #[test]
    fn adversarial_provider_config_edit_does_not_persist_unrelated_environment_secrets(
    ) -> anyhow::Result<()> {
        let _lock = crate::env_util::test_env_lock();
        let entries = [
            ("MACBOT_API_KEY", "public-audit-environment-api-key"),
            (
                "MACBOT_API_HMAC_SECRET",
                "public-audit-environment-hmac-secret",
            ),
            (
                "MACBOT_PRIVATE_RPC_URL",
                "https://public-audit-rpc.invalid/key",
            ),
            ("MACBOT_ALLOW_SENSITIVE_CONFIG_API", ""),
            ("MACBOT_FINNHUB_API_KEY", ""),
            ("MACBOT_LUNARCRUSH_API_KEY", ""),
        ];
        let restore = RestoreEnv(
            entries
                .iter()
                .map(|(name, _)| (*name, std::env::var(name).ok()))
                .collect(),
        );
        for (name, value) in entries {
            std::env::set_var(name, value);
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let changed = runtime.block_on(async {
            let pool = crate::db::open(":memory:").await?;
            let path = std::env::temp_dir().join(format!(
                "macbot-adversarial-provider-edit-{:032x}.json",
                rand::random::<u128>()
            ));
            let mut persisted: Config = serde_json::from_str(include_str!("../../config.json"))?;
            persisted.mode = "simulation".into();
            persisted.api_key = "saved-local-key".into();
            persisted.api_hmac_secret = "saved-local-secret".into();
            persisted.private_rpc_url = String::new();
            persisted.save(path.to_str().unwrap())?;
            let loaded = Config::load(path.to_str().unwrap())?;
            let mut bot = TradingBot::new(loaded, pool).await?;
            Arc::get_mut(&mut bot.control_plane).unwrap().config_path =
                path.to_string_lossy().into_owned();
            let bot = Arc::new(bot);
            let body = r#"{"finnhub_api_key":"public-audit-new-provider-key"}"#;
            let ts = chrono::Utc::now().timestamp().to_string();
            let nonce = hex::encode(rand::random::<[u8; 32]>());
            let canonical =
                super::super::auth::request_canonical(&ts, &nonce, "POST", "/api/config", "", body);
            let mut mac =
                Hmac::<sha2::Sha256>::new_from_slice(b"public-audit-environment-hmac-secret")?;
            mac.update(canonical.as_bytes());
            let mut request = Request::builder()
                .method("POST")
                .uri("/api/config")
                .header("content-type", "application/json")
                .header("x-api-key", "public-audit-environment-api-key")
                .header("x-api-ts", ts)
                .header("x-api-nonce", nonce)
                .header("x-api-signature", hex::encode(mac.finalize().into_bytes()))
                .body(Body::from(body))?;
            request
                .extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 42091))));
            let response = crate::api::router(Arc::clone(&bot))
                .oneshot(request)
                .await?;
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "ordinary provider edit must be accepted without sensitive override"
            );
            let saved: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
            assert_eq!(saved["finnhub_api_key"], "public-audit-new-provider-key");
            let before = serde_json::to_value(persisted)?;
            let changed: Vec<&str> = ["api_key", "api_hmac_secret", "private_rpc_url"]
                .into_iter()
                .filter(|name| saved[name] != before[name])
                .collect();
            bot.db.close().await;
            std::fs::remove_file(path)?;
            anyhow::Ok(changed)
        })?;
        drop(restore);
        assert!(
            changed.is_empty(),
            "provider edit persisted unrelated environment-only secret fields: {changed:?}"
        );
        Ok(())
    }
}
