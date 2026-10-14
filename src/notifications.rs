//! Discord/Telegram webhook notification system.
//! Sends alerts for significant trading events:
//! - Large losses (> $2)
//! - Kill switch activation
//! - Circuit breaker cooldown
//! - New model deployment
//!
//! Set MACBOT_DISCORD_WEBHOOK_URL env var to enable Discord notifications.
//! Set MACBOT_TELEGRAM_BOT_TOKEN and MACBOT_TELEGRAM_CHAT_ID for Telegram.

use crate::http_client::HTTP;
use chrono::Utc;
use serde_json::json;
use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

fn alert_fallback_path() -> PathBuf {
    std::env::var("MACBOT_ALERT_FALLBACK_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("artifacts/alerts/alerts.jsonl"))
}

fn append_local_alert(
    title: &str,
    message: &str,
    level: NotifyLevel,
    discord_delivered: bool,
    telegram_delivered: bool,
) -> std::io::Result<()> {
    append_local_alert_to(
        &alert_fallback_path(),
        title,
        message,
        level,
        discord_delivered,
        telegram_delivered,
    )
}

fn append_local_alert_to(
    path: &Path,
    title: &str,
    message: &str,
    level: NotifyLevel,
    discord_delivered: bool,
    telegram_delivered: bool,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let payload = json!({
        "timestamp": Utc::now().to_rfc3339(),
        "title": title,
        "message": message,
        "level": level.as_str(),
        "discord_delivered": discord_delivered,
        "telegram_delivered": telegram_delivered,
    });
    writeln!(file, "{payload}")?;
    Ok(())
}

/// Send a notification to all configured channels.
pub async fn notify(title: &str, message: &str, level: NotifyLevel) {
    let emoji = match level {
        NotifyLevel::Info => "ℹ️",
        NotifyLevel::Warning => "⚠️",
        NotifyLevel::Critical => "🚨",
        NotifyLevel::Success => "✅",
    };
    let mut discord_delivered = false;
    let mut telegram_delivered = false;

    // Discord webhook
    if let Ok(url) = std::env::var("MACBOT_DISCORD_WEBHOOK_URL") {
        let payload = serde_json::json!({
            "content": format!("{emoji} **{title}**\n{message}"),
            "username": "MacBot Alerts"
        });
        match HTTP.post(&url).json(&payload).send().await {
            Ok(r) if r.status().is_success() => {
                discord_delivered = true;
                info!("Discord notification sent: {title}");
            }
            Ok(r) => warn!("Discord webhook failed: status={}", r.status()),
            Err(_) => warn!("Discord webhook error"),
        }
    }

    // Telegram bot
    if let (Ok(token), Ok(chat_id)) = (
        std::env::var("MACBOT_TELEGRAM_BOT_TOKEN"),
        std::env::var("MACBOT_TELEGRAM_CHAT_ID"),
    ) {
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let payload = serde_json::json!({
            "chat_id": chat_id,
            "text": format!("{emoji} *{title}*\n{message}"),
            "parse_mode": "Markdown"
        });
        match HTTP.post(&url).json(&payload).send().await {
            Ok(r) if r.status().is_success() => {
                telegram_delivered = true;
                info!("Telegram notification sent: {title}");
            }
            Ok(r) => warn!("Telegram send failed: status={}", r.status()),
            Err(_) => warn!("Telegram send error"),
        }
    }

    if let Err(err) =
        append_local_alert(title, message, level, discord_delivered, telegram_delivered)
    {
        warn!("Local alert fallback write failed: {err}");
    }
}

#[derive(Debug, Clone, Copy)]
pub enum NotifyLevel {
    Info,
    Warning,
    Critical,
    Success,
}

impl NotifyLevel {
    fn as_str(self) -> &'static str {
        match self {
            NotifyLevel::Info => "info",
            NotifyLevel::Warning => "warning",
            NotifyLevel::Critical => "critical",
            NotifyLevel::Success => "success",
        }
    }
}

/// Helper: notify on large loss
pub async fn notify_large_loss(trade_id: i64, pnl: f64) {
    if pnl < -2.0 {
        notify(
            "Large Loss",
            &format!("Trade #{trade_id} closed with P&L ${pnl:.2}"),
            NotifyLevel::Critical,
        )
        .await;
    }
}

/// Helper: notify on kill switch
pub async fn notify_kill_switch(reason: &str) {
    notify("Kill Switch Activated", reason, NotifyLevel::Critical).await;
}

/// Helper: notify on circuit breaker
pub async fn notify_circuit_breaker(consecutive_losses: usize, pause_minutes: i64) {
    notify(
        "Circuit Breaker Cooldown",
        &format!("{consecutive_losses} consecutive losses — pausing {pause_minutes} minutes"),
        NotifyLevel::Warning,
    )
    .await;
}

/// Helper: notify when FOK close retries are exhausted
pub async fn notify_fok_retry_exhausted(trade_id: i64) {
    notify(
        "FOK Close Retry Exhausted",
        &format!("Trade {trade_id} could not be closed after all FOK retries — manual intervention required"),
        NotifyLevel::Critical,
    )
    .await;
}

/// Helper: notify on new model deployment
pub async fn notify_model_deployed(version: &str, brier: Option<f64>) {
    let score = brier
        .map(|b| format!(" (Brier: {b:.3})"))
        .unwrap_or_default();
    notify(
        "New Model Deployed",
        &format!("Version: {version}{score}"),
        NotifyLevel::Success,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::{append_local_alert_to, NotifyLevel};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn local_alert_fallback_writes_jsonl() {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macbot_alerts_{uniq}.jsonl"));

        append_local_alert_to(
            &path,
            "Kill Switch Activated",
            "test message",
            NotifyLevel::Critical,
            false,
            false,
        )
        .expect("alert write");

        let body = fs::read_to_string(&path).expect("alert file");
        assert!(body.contains("\"title\":\"Kill Switch Activated\""));
        assert!(body.contains("\"level\":\"critical\""));

        let _ = fs::remove_file(path);
    }
}
