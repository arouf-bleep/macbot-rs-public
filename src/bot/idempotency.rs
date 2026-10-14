use std::time::Instant;

use anyhow::Result;
use chrono::Utc;
use serde_json::{json, Value};
use tracing::warn;

use crate::db;

use super::state::TradingBot;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db;
    use serde_json::json;

    async fn test_bot(name: &str) -> anyhow::Result<(TradingBot, std::path::PathBuf, String)> {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_idempotency_{name}_{uniq}.sqlite"));
        let path_str = path.to_string_lossy().to_string();
        let pool = db::open(&path_str).await?;

        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json"))?;
        cfg.mode = "simulation".to_string();
        cfg.initial_balance = 100.0;

        let bot = TradingBot::new(cfg, pool).await?;
        Ok((bot, path, path_str))
    }

    fn cleanup_db(path: &std::path::Path, path_str: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{path_str}-shm"));
        let _ = std::fs::remove_file(format!("{path_str}-wal"));
    }

    #[tokio::test]
    async fn claim_put_get_round_trip_and_pending_state_are_consistent() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("round_trip").await?;
        let key = "idem-round-trip";
        let payload = json!({"ok": true, "n": 7});

        match bot.idempotency_claim_or_get(key).await? {
            IdempotencyClaim::Claimed => {}
            _ => panic!("expected initial claim"),
        }
        let pending = db::get_idempotency_entry(&bot.db, key).await?;
        let pending_marker = pending_payload_raw();
        assert_eq!(
            pending.as_ref().map(|row| row.0.as_str()),
            Some(pending_marker.as_str())
        );
        assert!(
            pending.as_ref().map(|row| row.1).unwrap_or_default() > 0,
            "pending claim should persist a timestamp"
        );

        match bot.idempotency_claim_or_get(key).await? {
            IdempotencyClaim::InProgress => {}
            _ => panic!("expected pending lock"),
        }

        bot.idempotency_put(key.to_string(), payload.clone()).await;

        assert_eq!(bot.idempotency_get(key).await, Some(payload.clone()));
        match bot.idempotency_claim_or_get(key).await? {
            IdempotencyClaim::Cached(v) => assert_eq!(v, payload),
            _ => panic!("expected cached payload after put"),
        }

        let persisted = db::get_idempotency_entry(&bot.db, key).await?;
        assert_eq!(persisted.map(|row| row.0), Some(payload.to_string()));

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn malformed_payload_keeps_an_unknown_execution_locked() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("malformed").await?;
        let key = "idem-malformed";
        let now = chrono::Utc::now().timestamp();

        db::upsert_idempotency_entry(&bot.db, key, "not-json", now).await?;
        assert!(bot.idempotency_get(key).await.is_none());
        assert!(db::get_idempotency_entry(&bot.db, key).await?.is_some());
        assert!(matches!(
            bot.idempotency_claim_or_get(key).await?,
            IdempotencyClaim::InProgress
        ));

        drop(bot);
        cleanup_db(&path, &path_str);
        Ok(())
    }

    #[tokio::test]
    async fn old_pending_claim_survives_hot_cache_and_durable_pruning() -> anyhow::Result<()> {
        let (bot, path, path_str) = test_bot("old_pending").await?;
        bot.control_plane
            .config
            .write()
            .await
            .idempotency_ttl_seconds = 1;
        db::upsert_idempotency_entry(
            &bot.db,
            "unknown-order",
            &pending_payload_raw(),
            Utc::now().timestamp() - 100,
        )
        .await?;
        bot.prune_idempotency_cache().await;
        assert!(matches!(
            bot.idempotency_claim_or_get("unknown-order").await?,
            IdempotencyClaim::InProgress
        ));
        assert!(db::get_idempotency_entry(&bot.db, "unknown-order")
            .await?
            .is_some());
        bot.db.close().await;
        cleanup_db(&path, &path_str);
        Ok(())
    }
}

pub struct IdempotencyEntry {
    pub ts: Instant,
    pub payload: Value,
}

pub enum IdempotencyClaim {
    Claimed,
    Cached(Value),
    InProgress,
}

const IDEMPOTENCY_PENDING_FIELD: &str = "_macbot_state";
const IDEMPOTENCY_PENDING_VALUE: &str = "pending";

impl TradingBot {
    pub async fn idempotency_get(&self, key: &str) -> Option<Value> {
        match self.idempotency_lookup(key, true).await {
            Some(payload) if !is_pending_payload(&payload) => Some(payload),
            _ => None,
        }
    }

    pub async fn idempotency_claim_or_get(&self, key: &str) -> Result<IdempotencyClaim> {
        if let Some(payload) = self.idempotency_get(key).await {
            return Ok(IdempotencyClaim::Cached(payload));
        }

        let now_unix = Utc::now().timestamp();
        let pending_raw = pending_payload_raw();
        if db::insert_idempotency_entry_if_absent(&self.db, key, &pending_raw, now_unix).await? {
            return Ok(IdempotencyClaim::Claimed);
        }

        match self.idempotency_lookup(key, false).await {
            Some(payload) if is_pending_payload(&payload) => Ok(IdempotencyClaim::InProgress),
            Some(payload) => Ok(IdempotencyClaim::Cached(payload)),
            None => {
                if db::insert_idempotency_entry_if_absent(&self.db, key, &pending_raw, now_unix)
                    .await?
                {
                    Ok(IdempotencyClaim::Claimed)
                } else {
                    Ok(IdempotencyClaim::InProgress)
                }
            }
        }
    }

    pub async fn idempotency_put(&self, key: String, payload: Value) {
        let now_unix = Utc::now().timestamp();
        match serde_json::to_string(&payload) {
            Ok(raw) => {
                if let Err(e) = db::upsert_idempotency_entry(&self.db, &key, &raw, now_unix).await {
                    warn!("idempotency db upsert failed for key={key}: {e}");
                    // The action may already have executed. Keep its durable pending
                    // claim through failure/restart and cache the known result locally;
                    // deleting the claim here would authorize a duplicate side effect.
                    self.control_plane.idempotency_cache.insert(
                        key,
                        IdempotencyEntry {
                            ts: Instant::now(),
                            payload,
                        },
                    );
                    return;
                }
            }
            Err(e) => {
                warn!("idempotency serialize failed for key={key}: {e}");
                // Preserve the pending claim when an executed result cannot be encoded.
                return;
            }
        }
        self.control_plane.idempotency_cache.insert(
            key,
            IdempotencyEntry {
                ts: Instant::now(),
                payload,
            },
        );
        self.idempotency_prune().await;
    }

    pub async fn idempotency_abort(&self, key: &str) {
        self.control_plane.idempotency_cache.remove(key);
        if let Err(e) = db::delete_idempotency_entry(&self.db, key).await {
            warn!("idempotency db delete failed for key={key}: {e}");
        }
    }

    /// Public entry point for the periodic cache-pruning task.
    pub async fn prune_idempotency_cache(&self) {
        self.idempotency_prune().await;
    }

    async fn idempotency_prune(&self) {
        let (ttl_secs, max_entries) = {
            let cfg = self.control_plane.config.read().await;
            (
                cfg.idempotency_ttl_seconds.max(1),
                cfg.idempotency_max_entries.max(1),
            )
        };

        let stale: Vec<String> = self
            .control_plane
            .idempotency_cache
            .iter()
            .filter(|it| it.ts.elapsed().as_secs() > ttl_secs)
            .map(|it| it.key().clone())
            .collect();
        for key in stale {
            self.control_plane.idempotency_cache.remove(&key);
        }

        let len = self.control_plane.idempotency_cache.len();
        // Evict oldest entries instead of depending on DashMap iteration order.
        // Durable pruning must run even when the small hot cache is within its cap.
        let excess = len.saturating_sub(max_entries);
        let mut entries: Vec<(String, Instant)> = self
            .control_plane
            .idempotency_cache
            .iter()
            .map(|it| (it.key().clone(), it.ts))
            .collect();
        entries.sort_by_key(|(_, ts)| *ts);
        for (key, _) in entries.into_iter().take(excess) {
            self.control_plane.idempotency_cache.remove(&key);
        }

        let min_created = Utc::now()
            .timestamp()
            .saturating_sub(i64::try_from(ttl_secs).unwrap_or(i64::MAX));
        if let Err(e) = db::prune_idempotency_entries(&self.db, min_created, max_entries).await {
            warn!("idempotency db prune failed: {e}");
        }
    }

    async fn idempotency_lookup(&self, key: &str, cache_finalized: bool) -> Option<Value> {
        let ttl_secs = {
            let cfg = self.control_plane.config.read().await;
            cfg.idempotency_ttl_seconds.max(1)
        };
        if let Some(v) = self.control_plane.idempotency_cache.get(key) {
            if v.ts.elapsed().as_secs() <= ttl_secs {
                return Some(v.payload.clone());
            }
        }
        self.control_plane.idempotency_cache.remove(key);

        let row = db::get_idempotency_entry(&self.db, key)
            .await
            .ok()
            .flatten();
        let (payload_raw, created_at_unix) = match row {
            Some(v) => v,
            None => return None,
        };
        let payload = match serde_json::from_str::<Value>(&payload_raw) {
            Ok(v) => v,
            Err(e) => {
                warn!("idempotency payload parse failed for key={key}: {e}");
                // Corruption cannot establish that an action never executed.
                return Some(json!({ IDEMPOTENCY_PENDING_FIELD: IDEMPOTENCY_PENDING_VALUE }));
            }
        };
        let now_unix = Utc::now().timestamp();
        let age_secs = now_unix.saturating_sub(created_at_unix).max(0) as u64;
        // Pending actions are not known failures: a timeout/restart may conceal
        // an executed order. Only an explicit known-not-executed abort clears one.
        if !is_pending_payload(&payload) && age_secs > ttl_secs {
            if let Err(e) = db::delete_idempotency_entry(&self.db, key).await {
                warn!("idempotency expired entry cleanup failed for key={key}: {e}");
            }
            return None;
        }
        if cache_finalized && !is_pending_payload(&payload) {
            self.control_plane.idempotency_cache.insert(
                key.to_string(),
                IdempotencyEntry {
                    ts: Instant::now()
                        .checked_sub(std::time::Duration::from_secs(age_secs))
                        .unwrap_or_else(Instant::now),
                    payload: payload.clone(),
                },
            );
        }
        Some(payload)
    }
}

fn pending_payload_raw() -> String {
    json!({ IDEMPOTENCY_PENDING_FIELD: IDEMPOTENCY_PENDING_VALUE }).to_string()
}

fn is_pending_payload(payload: &Value) -> bool {
    payload
        .get(IDEMPOTENCY_PENDING_FIELD)
        .and_then(|v| v.as_str())
        == Some(IDEMPOTENCY_PENDING_VALUE)
}
