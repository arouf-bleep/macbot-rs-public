use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::config::Config;

pub const DEFAULT_MACRO_EVENT_CALENDAR_PATH: &str = "ops/macro-events.json";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct MacroEvent {
    pub label: String,
    #[serde(default)]
    pub kind: String,
    pub timestamp: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct MacroEventWindow {
    pub label: String,
    pub kind: String,
    pub timestamp: String,
    pub source: String,
    pub phase: String,
    pub window_start: String,
    pub window_end: String,
    pub secs_until_event: i64,
    pub minutes_until_event: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
pub struct MacroGuardStatus {
    pub enabled: bool,
    pub blocking: bool,
    pub source_path: String,
    pub error: Option<String>,
    pub guard_minutes_before: u64,
    pub guard_minutes_after: u64,
    pub active: Option<MacroEventWindow>,
    pub next: Option<MacroEventWindow>,
    pub refreshed_at: Option<String>,
    pub valid_until: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CalendarDocument {
    Static(Vec<MacroEvent>),
    Refreshed {
        schema_version: u32,
        refreshed_at: String,
        valid_until: String,
        events: Vec<MacroEvent>,
    },
}

fn calendar_path(cfg: &Config) -> String {
    let path = cfg.macro_event_calendar_path.trim();
    if path.is_empty() {
        DEFAULT_MACRO_EVENT_CALENDAR_PATH.to_string()
    } else {
        path.to_string()
    }
}

fn parse_event_time(raw: &str) -> Result<DateTime<Utc>> {
    ensure!(raw.len() <= 64, "macro event timestamp is too long");
    let time = DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("invalid RFC3339 timestamp '{raw}'"))?
        .with_timezone(&Utc);
    ensure!(
        time.checked_sub_signed(Duration::hours(4)).is_some()
            && time.checked_add_signed(Duration::hours(4)).is_some(),
        "macro event timestamp cannot represent its guard window"
    );
    Ok(time)
}

/// Only the operator-selected interpreter runs the fixed bundled updater. No
/// bot credentials are inherited and an overdue child is killed on drop.
pub async fn refresh_official_calendar(cfg: &Config) -> Result<()> {
    use std::process::Stdio;
    let python =
        std::env::var("MACBOT_MACRO_CALENDAR_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let mut command = tokio::process::Command::new(python);
    command
        .arg("scripts/update_macro_calendar.py")
        .arg("--output")
        .arg(calendar_path(cfg))
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for key in [
        "PATH",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "MACBOT_OPENSSL_BACKPORT_EVIDENCE",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let mut child = command.spawn().context("start official calendar updater")?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait())
        .await
        .context("official calendar updater exceeded 30s")??;
    ensure!(result.success(), "official calendar updater exited with {result}; run the bundled updater to inspect its source error");
    Ok(())
}

fn load_events(
    path: &str,
    now: DateTime<Utc>,
    status: &mut MacroGuardStatus,
    require_refresh: bool,
) -> Result<Vec<MacroEvent>> {
    use std::io::Read;
    const MAX_BYTES: u64 = 1024 * 1024;
    let mut text = String::new();
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to read macro event calendar '{path}'"))?;
    ensure!(
        file.metadata()?.is_file(),
        "macro calendar must be a regular local file"
    );
    file.take(MAX_BYTES + 1).read_to_string(&mut text)?;
    ensure!(
        text.len() as u64 <= MAX_BYTES,
        "macro calendar exceeds 1MiB"
    );
    let document: CalendarDocument = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse macro event calendar '{path}'"))?;
    let mut events = match document {
        CalendarDocument::Static(events) => {
            ensure!(!require_refresh, "automatic macro guard needs a fresh calendar; awaiting successful official-source refresh");
            events
        }
        CalendarDocument::Refreshed {
            schema_version,
            refreshed_at,
            valid_until,
            events,
        } => {
            ensure!(schema_version == 1, "unsupported macro calendar schema");
            let refreshed = parse_event_time(&refreshed_at)?;
            let expires = parse_event_time(&valid_until)?;
            status.refreshed_at = Some(refreshed_at);
            status.valid_until = Some(valid_until);
            ensure!(
                refreshed <= now + Duration::minutes(5),
                "macro calendar refresh time is in the future"
            );
            ensure!(
                expires > refreshed && expires <= refreshed + Duration::hours(48),
                "invalid macro calendar freshness window"
            );
            ensure!(now <= expires, "macro calendar refresh expired");
            events
        }
    };
    ensure!(
        !events.is_empty() && events.len() <= 4096,
        "macro calendar must contain 1–4096 events"
    );
    let mut identities = std::collections::HashSet::new();
    for event in &events {
        ensure!(
            !event.label.trim().is_empty()
                && event.label.len() <= 256
                && event.kind.len() <= 64
                && event.source.len() <= 2048,
            "invalid macro event fields"
        );
        let time = parse_event_time(&event.timestamp)?;
        ensure!(
            identities.insert((time, event.kind.clone())),
            "duplicate macro event identity"
        );
    }
    events.sort_by_key(|event| parse_event_time(&event.timestamp).expect("validated timestamp"));
    Ok(events)
}

fn to_window(
    event: &MacroEvent,
    event_time: DateTime<Utc>,
    now: DateTime<Utc>,
    before: Duration,
    after: Duration,
) -> MacroEventWindow {
    let window_start = event_time - before;
    let window_end = event_time + after;
    let secs_until_event = event_time.signed_duration_since(now).num_seconds();
    MacroEventWindow {
        label: event.label.clone(),
        kind: event.kind.clone(),
        timestamp: event.timestamp.clone(),
        source: event.source.clone(),
        phase: if now < event_time {
            "pre_event".to_string()
        } else {
            "post_event".to_string()
        },
        window_start: window_start.to_rfc3339(),
        window_end: window_end.to_rfc3339(),
        secs_until_event,
        minutes_until_event: secs_until_event as f64 / 60.0,
    }
}

pub fn macro_guard_status(cfg: &Config, now: DateTime<Utc>) -> MacroGuardStatus {
    let path = calendar_path(cfg);
    let mut status = MacroGuardStatus {
        enabled: cfg.macro_event_guard_enabled,
        blocking: false,
        source_path: path.clone(),
        error: None,
        guard_minutes_before: cfg.macro_event_guard_minutes_before,
        guard_minutes_after: cfg.macro_event_guard_minutes_after,
        active: None,
        next: None,
        refreshed_at: None,
        valid_until: None,
    };

    let before = Duration::minutes(cfg.macro_event_guard_minutes_before.min(240) as i64);
    let after = Duration::minutes(cfg.macro_event_guard_minutes_after.min(240) as i64);

    let events = match load_events(&path, now, &mut status, cfg.macro_event_auto_refresh) {
        Ok(events) => events,
        Err(err) => {
            status.error = Some(err.to_string());
            return status;
        }
    };
    if !events.iter().any(|event| {
        parse_event_time(&event.timestamp).expect("validated timestamp") + after >= now
    }) {
        status.error = Some(
            "macro calendar has no remaining event coverage; renew it before trading".to_string(),
        );
        return status;
    }

    for event in events {
        let event_time = match parse_event_time(&event.timestamp) {
            Ok(ts) => ts,
            Err(err) => {
                if status.error.is_none() {
                    status.error = Some(err.to_string());
                }
                continue;
            }
        };
        let window_start = event_time - before;
        let window_end = event_time + after;
        if now >= window_start && now <= window_end {
            let active = to_window(&event, event_time, now, before, after);
            status.blocking = status.enabled;
            status.active = Some(active);
            continue;
        }
        if event_time > now && status.next.is_none() {
            status.next = Some(to_window(&event, event_time, now, before, after));
        }
    }

    status
}

#[cfg(test)]
mod tests {
    use super::{macro_guard_status, MacroEvent};
    use crate::config::Config;
    use chrono::{DateTime, Utc};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture_cfg(path: &str) -> Config {
        let mut cfg: Config = serde_json::from_str(include_str!("../../config.json")).unwrap();
        cfg.macro_event_guard_enabled = true;
        cfg.macro_event_guard_minutes_before = 15;
        cfg.macro_event_guard_minutes_after = 5;
        cfg.macro_event_calendar_path = path.to_string();
        cfg
    }

    #[test]
    fn macro_guard_status_blocks_inside_window_and_reports_next() {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("macro_events_{uniq}.json"));
        let events = vec![
            MacroEvent {
                label: "CPI".to_string(),
                kind: "cpi".to_string(),
                timestamp: "2026-03-21T12:30:00Z".to_string(),
                source: "test".to_string(),
            },
            MacroEvent {
                label: "FOMC".to_string(),
                kind: "fomc".to_string(),
                timestamp: "2026-03-22T18:00:00Z".to_string(),
                source: "test".to_string(),
            },
        ];
        std::fs::write(&path, serde_json::to_vec(&events).unwrap()).unwrap();

        let cfg = fixture_cfg(path.to_str().unwrap());
        let status = macro_guard_status(
            &cfg,
            DateTime::parse_from_rfc3339("2026-03-21T12:20:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );

        assert!(status.blocking);
        assert_eq!(
            status.active.as_ref().map(|e| e.label.as_str()),
            Some("CPI")
        );
        assert_eq!(status.next.as_ref().map(|e| e.label.as_str()), Some("FOMC"));

        let _ = std::fs::remove_file(path);
    }

    fn status_for(document: serde_json::Value, automatic: bool) -> super::MacroGuardStatus {
        let path = std::env::temp_dir().join(format!(
            "macro-invalid-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
        let mut cfg = fixture_cfg(path.to_str().unwrap());
        cfg.macro_event_auto_refresh = automatic;
        let status = macro_guard_status(
            &cfg,
            DateTime::parse_from_rfc3339("2026-10-11T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        std::fs::remove_file(path).unwrap();
        status
    }

    fn event(timestamp: &str) -> serde_json::Value {
        serde_json::json!({"label":"CPI", "kind":"cpi", "timestamp":timestamp, "source":"fixture"})
    }

    #[test]
    fn invalid_empty_duplicate_and_exhausted_calendars_are_unavailable() {
        for document in [
            serde_json::json!([]),
            serde_json::json!([event("invalid")]),
            serde_json::json!([event("2026-10-12T12:30:00Z"), event("2026-10-12T12:30:00Z")]),
            serde_json::json!([event("2026-10-10T12:30:00Z")]),
            serde_json::json!([event("+262142-12-31T23:59:59Z")]),
            serde_json::json!([event("-262143-01-01T00:00:00Z")]),
        ] {
            assert!(status_for(document, false).error.is_some());
        }
        assert!(
            status_for(serde_json::json!([event("2026-10-12T12:30:00Z")]), true)
                .error
                .is_some()
        );
    }

    #[test]
    fn automatic_calendars_require_a_bounded_unexpired_refresh() {
        let valid = serde_json::json!({"schema_version":1,"refreshed_at":"2026-10-10T12:00:00Z","valid_until":"2026-10-12T00:00:00Z","events":[event("2026-10-12T12:30:00Z")]});
        let status = status_for(valid.clone(), true);
        assert!(status.error.is_none());
        assert!(status.next.is_some());
        assert_eq!(status.valid_until.as_deref(), Some("2026-10-12T00:00:00Z"));
        for (field, value) in [
            ("valid_until", serde_json::json!("2026-10-10T23:59:59Z")),
            ("valid_until", serde_json::json!("2027-01-01T00:00:00Z")),
            ("refreshed_at", serde_json::json!("2026-10-11T01:00:00Z")),
            ("schema_version", serde_json::json!(2)),
        ] {
            let mut changed = valid.clone();
            changed[field] = value;
            assert!(
                status_for(changed, true).error.is_some(),
                "accepted changed {field}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn named_pipes_are_refused_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let path = std::env::temp_dir().join(format!(
            "macro-fifo-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the CString is live and terminated; the new FIFO is owned by
        // this test and removed once the bounded read has returned.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let cfg = fixture_cfg(path.to_str().unwrap());
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(macro_guard_status(&cfg, Utc::now()));
        });
        let result = receiver.recv_timeout(std::time::Duration::from_secs(2));
        std::fs::remove_file(path).unwrap();
        assert!(result
            .expect("calendar open blocked on a FIFO")
            .error
            .is_some());
    }
}
